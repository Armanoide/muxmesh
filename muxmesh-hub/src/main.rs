//! muxmesh-hub — state aggregator + command dispatcher (runs on the always-on
//! pivot machine).
//!
//! Listens on TCP :9997. Agents POST /state (JSON HostState); the hub merges
//! into state.json with ATOMIC writes (tmp + rename) so readers (tmux status
//! bar, CLI) never see a half-written file.
//!
//! Commands flow driver -> HUB -> agent over the agent's OWN outbound WebSocket:
//! the hub NEVER dials an agent (no outbound TcpStream here, ever). Each agent
//! holds a persistent WS connection (/ws), authenticated at the handshake with
//! X-Muxmesh-Host + X-Muxmesh-Token = its own machine token (same registry and
//! KDF as POST /state). A driver POSTs /cmd; the hub authenticates it, checks
//! dst is registered + WS-connected right now, enqueues the frame on that
//! agent's channel and answers 202 queued immediately. The agent executes tmux
//! locally and replies with an ack frame {v,id,ok,err}, which the hub logs
//! (and mirrors into state.json's last_cmd so the driver can read it back via
//! GET /state). std::net + tungstenite (sync) only -> trusted LAN, no TLS.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use muxmesh_core::{
    now_unix, valid_host, valid_name, Ack, CommandEnvelope, HostState, HubConfig, StateDoc,
    TokenStore,
};
use tungstenite::{handshake::derive_accept_key, protocol::Role, Message, WebSocket};

/// Monotonic counter for unique tmp filenames (see write_atomic).
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Monotonic id for /ws connection generations (channel takeover).
static WS_GEN: AtomicU64 = AtomicU64::new(1);

/// In-flight connection guard: threads are cheap but unbounded is not "bounded".
/// Acquire before reading a body; over cap -> 503. LAN tool, generous cap.
const MAX_INFLIGHT: usize = 64;
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

struct Permit;
impl Permit {
    fn acquire() -> Option<Self> {
        if INFLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            INFLIGHT.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(Permit)
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A queued command frame waiting to go out on an agent's WS channel.
struct Queued {
    id: String,
    frame: String,
    at: Instant,
}

/// Per-host dispatch channel. POST /cmd handlers only ever push frames onto
/// `queue`; the writer thread of the CURRENT connection drains it. `gen`
/// makes takeover safe: a replaced connection's writer stops when its gen is
/// superseded, so a dead socket is never written to again.
struct Channel {
    queue: VecDeque<Queued>,
    gen: u64,
}

type Channels = Arc<Mutex<HashMap<String, Channel>>>;

/// TTL of a queued frame: an agent that has not drained within ~10 s is wedged
/// — a stale bar toggle must never fire late.
const QUEUE_TTL: Duration = Duration::from_secs(10);

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let args = muxmesh_core::log::parse_verbose_flag(&args);
    muxmesh_core::log::init();
    let cfg = HubConfig::load();

    // Console mode (herdr-style `add machine`): mint a token for a host,
    // print it ONCE, exit. No server, no network. The operator pastes the
    // output on the agent machine; the agent then dials /ws with it.
    if args.first().map(|s| s.as_str()) == Some("generate-token") {
        generate_token(&cfg, &args[1..]); // diverges (process::exit)
    }

    // Per-machine token registry (digests only).
    let tokens_path = cfg.tokens_path.clone();
    let store: Arc<Mutex<TokenStore>> = Arc::new(Mutex::new(TokenStore::load(&tokens_path)));
    if store.lock().unwrap().hosts.is_empty() {
        tracing::warn!("no registered hosts in {} -> every /state POST and /ws handshake is 401 until you mint one: muxmesh-hub generate-token <host>", tokens_path.display());
    }

    // Embedded badge poller: tmux windows on THIS machine get the
    // @muxmesh-status badge without the operator launching anything. On a
    // headless pivot (no tmux server) it is inert by design.
    {
        let poll_cfg = cfg.clone();
        thread::spawn(move || muxmesh_core::poll::run_embedded(&poll_cfg));
    }

    let bind = cfg.bind.clone();
    let state_path = cfg.state_path.clone();

    // purge once at startup (expired entries disappear from the file)
    if let Err(e) = purge_and_write(&state_path, cfg.ttl) {
        tracing::error!("initial purge failed: {e}");
    }

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {bind} failed: {e}");
            process::exit(1);
        }
    };
    tracing::info!(
        "listening on {bind} -> {} (tokens: {}, agents dial /ws)",
        state_path.display(),
        tokens_path.display()
    );

    // serialize file writes (one POST at a time)
    let lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    // live WS dispatch channels, host -> channel
    let channels: Channels = Arc::new(Mutex::new(HashMap::new()));

    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                let path = state_path.clone();
                let lock = lock.clone();
                let store = store.clone();
                let channels = channels.clone();
                // backpressure BEFORE spawning: over cap -> 503, no thread, no read.
                match Permit::acquire() {
                    Some(permit) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        // no Nagle: the 101 handshake reply must reach the peer
                        // promptly (the agent sends nothing until it sees it).
                        let _ = stream.set_nodelay(true);

                        // Read the FULL head (request line + headers) HERE, in
                        // the accept loop, with a raw byte-at-a-time read (no
                        // BufReader near this socket). Routing then happens on
                        // parsed data and the head is handed to the handler as
                        // a value — no socket replay, no stranded buffers.
                        let mut head = String::new();
                        if read_head(&mut stream, &mut head).is_err() {
                            continue;
                        }
                        let req_line = head.lines().next().unwrap_or("").to_string();
                        // route ANY GET /ws to handle_ws; it authenticates and
                        // only then upgrades (401 before the 101, never 404).
                        let is_ws = {
                            let p = req_line.split_whitespace().nth(1).unwrap_or("");
                            p == "/ws" || p.starts_with("/ws?")
                        };

                        // /ws upgrade: handled on its own thread; the persistent
                        // connection holds one permit by design (cap 64 agents).
                        if is_ws {
                            let ws_stream = match stream.try_clone() {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            drop(stream);
                            thread::spawn(move || {
                                let _permit = permit;
                                if let Err(e) = handle_ws(
                                    ws_stream, peer, head, &store, &channels, &path, &lock,
                                ) {
                                    tracing::debug!("{peer} /ws: {e}");
                                }
                            });
                            continue;
                        }

                        thread::spawn(move || {
                            let _permit = permit; // released when the handler returns
                            if let Err(e) =
                                handle(stream, peer, head, &path, &lock, &store, &channels)
                            {
                                tracing::error!("{peer}: {e}");
                            }
                        });
                    }
                    None => {
                        let _ = respond(&mut stream, 503, "overloaded\n");
                    }
                }
            }
            Err(e) => tracing::error!("accept: {e}"),
        }
    }
}

/// Console mode: `muxmesh-hub generate-token <host>` — mint a machine token,
/// register it in tokens.json, print it ONCE. herdr-style `add machine`:
/// operator runs this on the pivot, pastes the printed token on the agent
/// (~/.config/muxmesh/token), done. Re-issue revokes the host's old token.
/// ONE token per host: it authenticates POST /state AND the /ws handshake.
fn generate_token(cfg: &HubConfig, args: &[String]) -> ! {
    let host = match args.first() {
        Some(h) if valid_host(h) => h.clone(),
        _ => {
            eprintln!(
                "usage: muxmesh-hub generate-token <host>   (host = what you type after ssh)"
            );
            process::exit(2);
        }
    };
    let mut store = TokenStore::load(&cfg.tokens_path);
    let token = match store.issue(&host) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("muxmesh-hub: generate-token: {e}");
            process::exit(1);
        }
    };
    // persist BEFORE printing: if the file write fails, no token was issued.
    let json = match store.to_json_pretty() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("muxmesh-hub: generate-token: serialize: {e}");
            process::exit(1);
        }
    };
    if let Err(e) = write_file_atomic(&cfg.tokens_path, json.as_bytes()) {
        eprintln!(
            "muxmesh-hub: generate-token: write {}: {e}",
            cfg.tokens_path.display()
        );
        process::exit(1);
    }
    // the ONLY place raw tokens ever appear: stdout, once, for the operator.
    println!("{token}");
    eprintln!(
        "muxmesh-hub: host '{host}' paired — machine token in {}. Paste it on the agent as ~/.config/muxmesh/token (0600). The agent uses it for POST /state and its /ws handshake. Shown once.",
        cfg.tokens_path.display()
    );
    process::exit(0)
}

/// tmp + rename, mode 0600 (tokens.json holds digests, still keep it tight).
fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_bytes_atomic(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Read the request head (request line + headers, up to the blank line)
/// straight from the socket, ONE BYTE AT A TIME — no BufReader anywhere near
/// this socket, so body bytes are never swallowed. The head is passed to the
/// handler as a value and NEVER replayed onto the socket (that echo raced the
/// client's FIN and corrupted reads); handlers parse the value they received.
fn read_head(stream: &mut TcpStream, head: &mut String) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
            Ok(_) => {
                head.push(byte[0] as char);
                if byte[0] == b'\n' {
                    let n = head.len();
                    if head.ends_with("\r\n\r\n") || (n >= 2 && head.ends_with("\n\n")) {
                        return Ok(()); // blank line: end of headers
                    }
                }
                if head.len() > 16_384 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "head too long",
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// GET /ws: read the handshake headers, authenticate the agent with its OWN
/// machine token (same registry + KDF as POST /state, run OUTSIDE the store
/// lock), 401 before upgrade on failure. On success: complete the upgrade and
/// run the dispatch session. One channel per host — a second connection from
/// the same host replaces the first (logged, old writer thread exits via gen).
fn handle_ws(
    mut stream: TcpStream,
    peer: std::net::SocketAddr,
    head: String,
    store: &Mutex<TokenStore>,
    channels: &Channels,
    state_path: &Path,
    lock: &Mutex<()>,
) -> std::io::Result<()> {
    // Parse the head the accept loop already read (request line + headers).
    // No socket reads happen before accept(): the head lives in `head`, and
    // tungstenite's handshake is completed BY hand below — no replay, no
    // BufReader, nothing can strand a byte.
    let mut host = String::new();
    let mut token = String::new();
    let mut ws_key = String::new();
    for line in head.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("x-muxmesh-host:") {
            host = line
                .split_once(':')
                .map(|x| x.1.trim().to_string())
                .unwrap_or_default();
        } else if lower.starts_with("x-muxmesh-token:") {
            token = line
                .split_once(':')
                .map(|x| x.1.trim().to_string())
                .unwrap_or_default();
        } else if lower.starts_with("sec-websocket-key:") {
            ws_key = line
                .split_once(':')
                .map(|x| x.1.trim().to_string())
                .unwrap_or_default();
        }
    }

    // AUTH at the handshake, BEFORE any upgrade. KDF outside the store lock
    // (M1). No WS key = not an upgrade attempt -> 401 too (nothing else on /ws).
    let rec = store
        .lock()
        .map_err(|e| io::Error::other(format!("lock: {e}")))?
        .hosts
        .get(&host)
        .cloned();
    let ok = !ws_key.is_empty()
        && valid_host(&host)
        && matches!(
            rec,
            Some(r) if token.len() >= muxmesh_core::PREFIX_LEN && muxmesh_core::verify_token(&token, &r.hash)
        );
    if !ok {
        tracing::warn!("{peer} /ws host={host} auth=FAIL -> 401 (no upgrade)");
        return respond(&mut stream, 401, "unauthorized\n");
    }

    // complete the upgrade BY HAND (the head was consumed by the accept loop):
    // 101 + Sec-WebSocket-Accept, then wrap the socket in a raw WS codec.
    // The writer thread gets its OWN clone of the socket (a raw TcpStream can
    // be read and written concurrently from two threads); this thread reads.
    let accept_key = derive_accept_key(ws_key.as_bytes());
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept_key}\r\n\r\n"
    )?;
    stream.flush()?;
    let ws_stream = stream
        .try_clone()
        .map_err(|e| io::Error::other(format!("try_clone: {e}")))?;
    let mut ws: tungstenite::WebSocket<TcpStream> =
        WebSocket::from_raw_socket(ws_stream, Role::Server, None);

    let gen = WS_GEN.fetch_add(1, Ordering::Relaxed);
    {
        let mut ch = channels
            .lock()
            .map_err(|e| io::Error::other(format!("lock: {e}")))?;
        if let Some(old) = ch.insert(
            host.clone(),
            Channel {
                queue: VecDeque::new(),
                gen,
            },
        ) {
            tracing::info!(
                "/ws host={host}: replacing existing connection (gen {} -> {gen})",
                old.gen
            );
        }
    }
    tracing::info!("{peer} /ws host={host} connected (gen {gen})");

    // writer loop: drain the channel queue, push frames on this socket (its
    // own socket clone — reads and writes never contend).
    let w_channels = channels.clone();
    let w_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return Err(io::Error::other("try_clone")),
    };
    let w_host = host.clone();
    let writer = thread::spawn(move || {
        let mut ws: tungstenite::WebSocket<TcpStream> =
            WebSocket::from_raw_socket(w_stream, Role::Server, None);
        loop {
            let frame = {
                let mut ch = match w_channels.lock() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                match ch.get_mut(&w_host) {
                    Some(c) if c.gen == gen => loop {
                        match c.queue.pop_front() {
                            Some(q) if q.at.elapsed() > QUEUE_TTL => {
                                tracing::warn!(
                                    "/ws host={}: dropping stale frame id={} (queue ttl)",
                                    w_host,
                                    q.id
                                );
                            }
                            Some(q) => break Some(Some(q.frame)),
                            None => break Some(None), // empty: keep polling
                        }
                    },
                    // replaced or gone -> stop touching this socket
                    _ => None,
                }
            };
            match frame {
                Some(Some(f)) => {
                    if let Err(e) = ws.send(Message::Text(f)) {
                        tracing::warn!("/ws host={}: send: {e}", w_host);
                        return;
                    }
                }
                Some(None) => {} // queue empty: keep the session alive
                None => return,  // channel superseded or closed -> end session
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    // reader loop: acks and pongs from the agent. Blocking read — a quiet
    // agent is normal; liveness comes from the hub pinging every ~20 s and
    // the agent's tungstenite auto-ponging inside its read(). A wedged
    // half-open socket (NAT silent-drop, hung agent) surfaces here as a
    // tungstenite Ping timeout error -> channel removed -> F10 gets 409
    // instead of vanishing into a black hole.
    let r_host = host.clone();
    loop {
        let msg = ws.read();
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<Ack>(&text) {
                Ok(ack) => {
                    tracing::info!(
                        "/ws host={r_host} ack id={} ok={}{}",
                        ack.id,
                        ack.ok,
                        ack.err
                            .as_deref()
                            .map(|e| format!(" err={e}"))
                            .unwrap_or_default()
                    );
                    record_ack(state_path, lock, &r_host, &ack);
                }
                Err(e) => tracing::warn!("/ws host={r_host}: bad ack frame: {e}"),
            },
            Ok(Message::Ping(data)) => {
                let _ = ws.send(Message::Pong(data));
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {} // binary/pong: ignored
        }
    }

    // socket closed: remove OUR channel (a replacement connection keeps its own)
    {
        let mut ch = channels.lock().unwrap_or_else(|e| e.into_inner());
        if ch.get(&host).is_some_and(|c| c.gen == gen) {
            ch.remove(&host);
        }
    }
    let _ = writer; // writer exits on its own once the channel is gone
    tracing::info!("{peer} /ws host={host} disconnected (gen {gen})");
    Ok(())
}

/// Mirror an agent ack into state.json's last_cmd (best effort — the driver
/// reads it back via GET /state). Never fails the WS session.
fn record_ack(state_path: &Path, lock: &Mutex<()>, host: &str, ack: &Ack) {
    let _g = match lock.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut raw: serde_json::Value = fs::read_to_string(state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hosts": {} }));
    if !raw.get("hosts").is_some_and(|h| h.is_object()) {
        raw["hosts"] = serde_json::json!({});
    }
    // the ack may arrive BEFORE the agent's first POST /state (test flow and
    // cold start alike) — create the host entry rather than drop the ack.
    if !raw["hosts"].get(host).is_some_and(|h| h.is_object()) {
        raw["hosts"][host] = serde_json::json!({});
    }
    raw["hosts"][host]["last_cmd"] = serde_json::json!({
        "id": ack.id,
        "ok": ack.ok,
        "err": ack.err,
        "at": now_unix(),
    });
    if let Ok(json) = serde_json::to_string_pretty(&raw) {
        if let Err(e) = write_bytes_atomic(state_path, json.as_bytes()) {
            tracing::warn!("last_cmd write {host}: {e}");
        }
    }
}

fn handle(
    mut stream: TcpStream,
    peer: std::net::SocketAddr,
    head: String,
    path: &Path,
    lock: &Mutex<()>,
    store: &Mutex<TokenStore>,
    channels: &Channels,
) -> std::io::Result<()> {
    // The accept loop already read the head (request line + headers) and
    // passed it as a value — parse it here; the socket is raw for the body.
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    // headers: Content-Length + host (X-Muxmesh-Host) + token
    // (header NAMES are case-insensitive; VALUES keep their case)
    let mut content_len: usize = 0;
    let mut header_host: Option<String> = None;
    let mut header_token: Option<String> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("x-muxmesh-host:") {
            header_host = Some(
                line.split_once(':')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            );
        } else if lower.starts_with("x-muxmesh-token:") {
            header_token = Some(
                line.split_once(':')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            );
        }
    }

    // a /ws request that reached the plain handler means the accept loop did
    // not classify it as an upgrade (missing/invalid headers) — reject, 401
    // before any upgrade, never a bare 404.
    if target.starts_with("/ws") {
        tracing::warn!("{peer} {method} /ws without a valid upgrade -> 401");
        return respond(&mut stream, 401, "unauthorized\n");
    }

    // AUTH: GET /state is open (badge pollers and `tree` are read-only and
    // the state holds no secrets); writes require a registered machine token.
    if method == "GET" && target.starts_with("/state") {
        let body = fs::read_to_string(path).unwrap_or_else(|_| "{\"hosts\":{}}".into());
        tracing::debug!("{peer} GET /state ({} bytes)", body.len());
        return respond(&mut stream, 200, &body);
    }

    // driver -> HUB -> agent command dispatch (pushed on the agent's WS).
    if method == "POST" && target.starts_with("/cmd") {
        // the /cmd body sits AFTER the headers already consumed by read_head —
        // read exactly Content-Length bytes from the raw socket.
        let mut body = vec![0u8; content_len.min(1_048_576)];
        if !body.is_empty() {
            stream.read_exact(&mut body)?;
        }
        return handle_cmd(
            &mut stream,
            peer,
            store,
            channels,
            header_host,
            header_token,
            body,
        );
    }

    // per-machine auth for POST /state. Unknown host or
    // bad token => 401. Tokens are minted OFFLINE by `muxmesh-hub
    // generate-token <host>` on the pivot — there is NO registration endpoint:
    // an unregistered machine cannot introduce itself to the hub at all.
    if method != "POST" || !target.starts_with("/state") {
        tracing::debug!("{peer} {method} {target} -> 404");
        return respond(&mut stream, 404, "not found\n");
    }
    let host_hdr = header_host.unwrap_or_default();
    let got_token = header_token.unwrap_or_default();
    // M1: clone the record out of the lock, then run the KDF OUTSIDE it —
    // otherwise every authenticated push serializes behind 100k sha256 rounds.
    let rec = store
        .lock()
        .map_err(|e| std::io::Error::other(format!("lock: {e}")))?
        .hosts
        .get(&host_hdr)
        .cloned();
    match rec {
        Some(r)
            if got_token.len() >= muxmesh_core::PREFIX_LEN
                && muxmesh_core::verify_token(&got_token, &r.hash) =>
        {
            tracing::debug!("{peer} POST /state host={host_hdr} auth=ok");
        }
        _ => {
            tracing::warn!(
                "{peer} POST /state host={host_hdr} auth=FAIL (registered: {})",
                store.lock().map(|s| s.hosts.len()).unwrap_or(0)
            );
            return respond(&mut stream, 401, "unauthorized\n");
        }
    }

    // body capped (1 MiB): an agent sends a few KB, beyond that = abuse
    if content_len == 0 || content_len > 1_048_576 {
        return respond(&mut stream, 400, "bad length\n");
    }

    let mut body = vec![0u8; content_len];
    stream.read_exact(&mut body)?;

    // host: X-Muxmesh-Host header (source of truth), else the payload field
    let mut state: HostState = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return respond(&mut stream, 422, &format!("invalid payload: {e}\n")),
    };
    if state.seen == 0 {
        state.seen = now_unix(); // clockless agent -> stamp on receipt
    } else {
        // clock-skew guard: a future `seen` would stay "fresh" forever; clamp it.
        let now = now_unix();
        if state.seen > now + 60 {
            state.seen = now;
        }
    }
    let host_key = if host_hdr.is_empty() {
        state.host.clone()
    } else {
        host_hdr.clone()
    };
    if host_key.is_empty() {
        return respond(&mut stream, 400, "missing host\n");
    }

    // TRUST BOUNDARY: host and session names become tmux target strings later
    // (poller `-t`). Whitelist them here so downstream tmux argv is provably
    // [A-Za-z0-9_.-]+ and target injection is structurally impossible.
    if !valid_host(&host_key) {
        return respond(&mut stream, 400, "invalid host name\n");
    }
    for s in &state.sessions {
        if !valid_name(&s.name) || (!s.active_window.is_empty() && !valid_name(&s.active_window)) {
            // name the rejected value: tmux allows ':' in window names, our
            // whitelist does not -> without this the 400 is undiagnosable.
            tracing::warn!(
                "rejecting session name='{}' active_window='{}' from host={host_hdr} (valid_name=[A-Za-z0-9_-])",
                s.name,
                s.active_window
            );
            return respond(&mut stream, 400, "invalid session name\n");
        }
    }

    // merge under lock -> atomic write. Raw JSON doc (not StateDoc) so the
    // out-of-band last_cmd ack mirror survives the agent's next /state merge.
    let _g = lock
        .lock()
        .map_err(|e| std::io::Error::other(format!("lock: {e}")))?;
    let mut raw: serde_json::Value = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hosts": {} }));
    if !raw.get("hosts").is_some_and(|h| h.is_object()) {
        raw["hosts"] = serde_json::json!({});
    }
    let hosts = raw.get_mut("hosts").unwrap();
    // trace BEFORE the move into the map (armed flag + session/window names),
    // so an operator can follow F10's chain (the driver reads THIS state).
    let trace = format!(
        "armed={} sessions=[{}]",
        state.armed,
        state
            .sessions
            .iter()
            .map(|s| format!("{}:{}w/active={}", s.name, s.windows, s.active_window))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut entry = serde_json::to_value(&state).map_err(std::io::Error::other)?;
    // carry the ack mirror across the merge (the agent payload has no last_cmd)
    if let Some(prev) = hosts.get(&host_key).and_then(|h| h.get("last_cmd")) {
        entry["last_cmd"] = prev.clone();
    }
    hosts[&host_key] = entry;
    let json = serde_json::to_string_pretty(&raw).map_err(std::io::Error::other)?;
    write_bytes_atomic(path, json.as_bytes())?;

    tracing::debug!(
        "state updated host={host_key} {} -> {}",
        trace,
        path.display()
    );
    tracing::info!("state updated: {host_key} {trace}");
    respond(&mut stream, 200, "ok\n")
}

/// POST /cmd: authenticate the driver, check dst registered + WS-connected,
/// enqueue the frame on the agent's channel, answer 202 queued immediately.
/// The hub NEVER dials the agent — delivery happens on the agent's own socket.
fn handle_cmd<W: Write>(
    stream: &mut W,
    peer: std::net::SocketAddr,
    store: &Mutex<TokenStore>,
    channels: &Channels,
    header_host: Option<String>,
    header_token: Option<String>,
    body: Vec<u8>,
) -> std::io::Result<()> {
    // body capped (1 MiB): a command envelope is a few hundred bytes.
    if body.is_empty() || body.len() > 1_048_576 {
        return respond(stream, 400, "bad length\n");
    }

    // closed list: unknown verbs die here, before any dispatch.
    let env: CommandEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => return respond(stream, 422, &format!("invalid payload: {e}\n")),
    };

    // AUTH driver: same registry + same KDF verify as /state (KDF outside lock).
    // dst is NOT checked before this: answering 400-before-401 would let an
    // unauthenticated caller enumerate registered hosts.
    let host_hdr = header_host.unwrap_or_default();
    let got_token = header_token.unwrap_or_default();
    let rec = store
        .lock()
        .map_err(|e| std::io::Error::other(format!("lock: {e}")))?
        .hosts
        .get(&host_hdr)
        .cloned();
    match rec {
        Some(r)
            if got_token.len() >= muxmesh_core::PREFIX_LEN
                && muxmesh_core::verify_token(&got_token, &r.hash) =>
        {
            tracing::debug!("{peer} POST /cmd src={host_hdr} auth=ok");
        }
        _ => {
            tracing::warn!("{peer} POST /cmd src={host_hdr} auth=FAIL");
            return respond(stream, 401, "unauthorized\n");
        }
    }

    let dst = env.dst.clone();
    if dst.is_empty() || !valid_host(&dst) {
        return respond(stream, 400, "invalid dst\n");
    }
    let registered = store
        .lock()
        .map(|s| s.hosts.contains_key(&dst))
        .unwrap_or(false);
    if !registered {
        tracing::warn!("{peer} POST /cmd dst={dst} -> 403 dst not registered");
        return respond(stream, 403, "dst not registered\n");
    }
    // the agent must hold a live WS channel RIGHT NOW — the hub has no other
    // way (and no right) to reach it.
    let enqueued = {
        let mut ch = channels
            .lock()
            .map_err(|e| std::io::Error::other(format!("lock: {e}")))?;
        match ch.get_mut(&dst) {
            Some(c) => {
                c.queue.push_back(Queued {
                    id: env.id.clone(),
                    frame: serde_json::to_string(&env).map_err(std::io::Error::other)?,
                    at: Instant::now(),
                });
                true
            }
            None => false,
        }
    };
    if !enqueued {
        tracing::warn!("{peer} POST /cmd dst={dst} -> 409 target not connected");
        return respond(stream, 409, "target not connected\n");
    }

    tracing::info!(
        "{peer} POST /cmd src={host_hdr} -> queued dst={dst} id={}",
        env.id
    );
    // serde-serialized (env.id is CLI-generated today, but never hand-build JSON)
    let body = serde_json::json!({"v":1,"id":env.id,"queued":true}).to_string() + "\n";
    respond_json(stream, &body)
}

fn respond_json<W: Write>(stream: &mut W, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn read_doc(path: &Path) -> StateDoc {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| StateDoc::from_json_str(&s).ok())
        .unwrap_or_default()
}

/// tmp + rename = atomic. Readers see either the old or the new file, never a mix.
/// tmp name is unique per write (pid + counter) so concurrent writers can never
/// splice into the same scratch file, even across hub instances.
fn write_atomic(path: &Path, doc: &StateDoc) -> std::io::Result<()> {
    let json = doc.to_json_pretty().map_err(std::io::Error::other)?;
    write_bytes_atomic(path, json.as_bytes())
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove expired entries and rewrite the file (called at startup).
fn purge_and_write(path: &Path, ttl: u64) -> std::io::Result<()> {
    let mut doc = read_doc(path);
    let now = now_unix();
    let before = doc.hosts.len();
    doc.hosts = doc
        .hosts
        .into_iter()
        .filter(|(_, h)| h.is_fresh(now, ttl))
        .collect::<BTreeMap<_, _>>();
    if doc.hosts.len() != before {
        write_atomic(path, &doc)?;
    }
    Ok(())
}

/// Write a minimal HTTP/1.1 response with Connection: close. Works over a raw
/// TcpStream or a BufReader<TcpStream> (Write forwards + flushes, so the
/// reader's own buffer can never strand the response).
fn respond<W: Write>(stream: &mut W, code: u16, body: &str) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        201 => "201 Created",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        409 => "409 Conflict",
        422 => "422 Unprocessable Entity",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join(format!("muxmesh-hub-test-{}-{}", std::process::id(), name))
            .join("state.json");
        if let Some(d) = p.parent() {
            let _ = fs::remove_dir_all(d);
            fs::create_dir_all(d).unwrap();
        }
        p
    }

    #[test]
    fn write_atomic_then_read_roundtrip() {
        let p = tmp_state("roundtrip");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "host-a".into(),
            HostState {
                host: "host-a".into(),
                armed: true,
                seen: 1_700_000_000,
                sessions: vec![],
            },
        );
        write_atomic(&p, &doc).unwrap();
        let back = read_doc(&p);
        assert!(back.hosts.contains_key("host-a"));
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn purge_removes_stale_keeps_fresh() {
        let p = tmp_state("purge");
        let now = now_unix();
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "fresh".into(),
            HostState {
                host: "fresh".into(),
                armed: true,
                seen: now,
                sessions: vec![],
            },
        );
        doc.hosts.insert(
            "stale".into(),
            HostState {
                host: "stale".into(),
                armed: true,
                seen: now - 9_999,
                sessions: vec![],
            },
        );
        write_atomic(&p, &doc).unwrap();
        purge_and_write(&p, 30).unwrap();
        let back = read_doc(&p);
        assert!(back.hosts.contains_key("fresh"));
        assert!(!back.hosts.contains_key("stale"));
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn read_doc_missing_file_is_empty_not_panic() {
        let p = std::env::temp_dir().join("muxmesh-hub-test-absent/state.json");
        let doc = read_doc(&p);
        assert!(doc.hosts.is_empty());
    }

    #[test]
    fn record_ack_mirrors_last_cmd_into_state_json() {
        let p = tmp_state("last-cmd");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "host-a".into(),
            HostState {
                host: "host-a".into(),
                armed: true,
                seen: now_unix(),
                sessions: vec![],
            },
        );
        write_atomic(&p, &doc).unwrap();
        let lock = Mutex::new(());
        record_ack(&p, &lock, "host-a", &Ack::ok(1, "c-1"));
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["id"], "c-1");
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["ok"], true);
        // unknown host -> no panic, file untouched shape
        record_ack(&p, &lock, "ghost", &Ack::err(1, "c-2", "nope"));
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn permit_guard_caps_inflight() {
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT {
            held.push(Permit::acquire().expect("under cap must succeed"));
        }
        assert!(Permit::acquire().is_none(), "over cap must be rejected");
        drop(held);
        assert!(Permit::acquire().is_some(), "cap restored after drop");
    }

    #[test]
    fn write_atomic_uses_unique_tmp_names() {
        // two writes must not collide on the scratch file even without the lock
        let p = tmp_state("uniq-tmp");
        let mut doc = StateDoc::default();
        doc.hosts.insert(
            "h".into(),
            HostState {
                host: "h".into(),
                armed: true,
                seen: 1,
                sessions: vec![],
            },
        );
        write_atomic(&p, &doc).unwrap();
        write_atomic(&p, &doc).unwrap();
        assert!(read_doc(&p).hosts.contains_key("h"));
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
