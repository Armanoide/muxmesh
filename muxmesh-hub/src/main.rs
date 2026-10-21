//! muxmesh-hub — state aggregator + command dispatcher (runs on the always-on
//! pivot machine).
//!
//! One axum server on ONE port (tokio runtime). Agents POST /state (JSON
//! HostState); the hub merges into state.json with ATOMIC writes (tmp +
//! rename) so readers (tmux status bar, CLI) never see a half-written file.
//!
//! Commands flow driver -> HUB -> agent over the agent's OWN outbound WebSocket
//! (/agent/ws): the hub NEVER dials an agent. Each agent holds a persistent WS
//! session, authenticated at the handshake from headers BEFORE the upgrade
//! (X-Muxmesh-Host + X-Muxmesh-Token = its own machine token — same registry
//! and KDF as POST /state; 401 before any 101). A driver POSTs /cmd; the hub
//! authenticates it, checks dst is registered + has a LIVE ws channel right
//! now, enqueues the frame on that channel's mpsc sender and answers 202
//! queued immediately. The agent executes tmux locally and replies with an ack
//! frame {v,id,ok,err}, which the hub logs (and mirrors into state.json's
//! last_cmd so the driver can read it back via GET /state).
//!
//! Liveness: the hub pings each agent every 30 s; two consecutive unanswered
//! pings close the channel (dead-peer detection). There is NO socket read
//! timeout — a quiet persistent channel is normal (the old 5 s accept-loop
//! timeout killed every WS at 5.07 s; that bug is structurally gone).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::mpsc::UnboundedSender;

use muxmesh_core::{
    now_unix, valid_host, valid_name, Ack, CommandEnvelope, HostState, HubConfig, StateDoc,
    TokenStore,
};

/// Monotonic counter for unique tmp filenames (see write_atomic).
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Monotonic id for /agent/ws connection generations (channel takeover).
static WS_GEN: AtomicU64 = AtomicU64::new(1);

/// Keepalive cadence: one WS ping per agent per interval.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Close a channel after this many consecutive unanswered pings (dead peer).
const MAX_MISSED_PONGS: u32 = 2;

/// Cap on the /cmd body (a command envelope is a few hundred bytes).
const MAX_CMD_BODY: usize = 1_048_576;

/// Shared hub state: token registry (digests only), state.json path + write
/// serialization, and one live mpsc channel per connected host.
struct AppState {
    tokens: Mutex<TokenStore>,
    state_path: PathBuf,
    write_lock: Mutex<()>,
    channels: Mutex<HashMap<String, Sender>>,
}

/// A live WS session for one host. POST /cmd only ever pushes onto `tx`;
/// the session task drains it. `gen` makes takeover safe: a replaced session's
/// task stops when its gen is superseded, so a dead socket is never used again.
struct Sender {
    tx: UnboundedSender<String>,
    gen: u64,
}

impl AppState {
    /// Authenticate `host`+`token` against the registry. The record is cloned
    /// out of the lock and the KDF runs OUTSIDE it — otherwise every auth
    /// serializes behind 100k sha256 rounds.
    fn auth_ok(&self, host: &str, token: &str) -> bool {
        let rec = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hosts
            .get(host)
            .cloned();
        matches!(
            rec,
            Some(r)
                if token.len() >= muxmesh_core::PREFIX_LEN
                    && muxmesh_core::verify_token(token, &r.hash)
        )
    }

    /// Read a header value (names are case-insensitive; values keep their case).
    fn hdr(map: &HeaderMap, name: &str) -> String {
        map.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
}

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let args = muxmesh_core::log::parse_verbose_flag(&args);
    muxmesh_core::log::init();
    let cfg = HubConfig::load();

    // Console mode (herdr-style `add machine`): mint a token for a host,
    // print it ONCE, exit. No server, no network. The operator pastes the
    // output on the agent machine; the agent then dials /agent/ws with it.
    if args.first().map(|s| s.as_str()) == Some("generate-token") {
        generate_token(&cfg, &args[1..]); // diverges (process::exit)
    }

    // Per-machine token registry (digests only).
    let tokens_path = cfg.tokens_path.clone();
    let store = TokenStore::load(&tokens_path);
    if store.hosts.is_empty() {
        tracing::warn!("no registered hosts in {} -> every /state POST and /agent/ws handshake is 401 until you mint one: muxmesh-hub generate-token <host>", tokens_path.display());
    }

    // Embedded badge poller: tmux windows on THIS machine get the
    // @muxmesh-status badge without the operator launching anything. On a
    // headless pivot (no tmux server) it is inert by design. Spawned as a
    // plain std::thread BEFORE the tokio runtime (it is blocking ureq+tmux).
    {
        let poll_cfg = cfg.clone();
        thread::spawn(move || muxmesh_core::poll::run_embedded(&poll_cfg));
    }

    let state_path = cfg.state_path.clone();

    // purge once at startup (expired entries disappear from the file)
    if let Err(e) = purge_and_write(&state_path, cfg.ttl) {
        tracing::error!("initial purge failed: {e}");
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("tokio runtime: {e}");
            process::exit(1);
        }
    };
    if let Err(e) = rt.block_on(run(cfg, store)) {
        tracing::error!("server: {e}");
        process::exit(1);
    }
}

/// Build the router and serve until the listener dies.
async fn run(cfg: HubConfig, store: TokenStore) -> std::io::Result<()> {
    let state = Arc::new(AppState {
        tokens: Mutex::new(store),
        state_path: cfg.state_path.clone(),
        write_lock: Mutex::new(()),
        channels: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/state", get(get_state).post(post_state))
        .route("/cmd", post(post_cmd))
        .route("/agent/ws", get(agent_ws))
        // legacy path: old agents dial /ws — same handler, same auth.
        .route("/ws", get(agent_ws))
        .fallback(not_found)
        // 401 before any route runs when the upgrade headers are missing or
        // wrong (never a bare 404 on the WS paths, never a 101).
        .route_layer(middleware::from_fn_with_state(state.clone(), ws_auth_gate))
        // body cap 1 MiB on POSTs (agents send a few KB; beyond that = abuse)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(
        "listening on {} -> {} (tokens: {}, agents dial /agent/ws)",
        cfg.bind,
        cfg.state_path.display(),
        cfg.tokens_path.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(std::io::Error::other)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

/// Gate on the WS paths: a GET /agent/ws (or legacy /ws) without valid auth
/// headers is 401 HERE — before the upgrade, before the handler. Any other
/// request passes through untouched.
async fn ws_auth_gate(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/agent/ws" || path == "/ws" {
        let host = AppState::hdr(req.headers(), "x-muxmesh-host");
        let token = AppState::hdr(req.headers(), "x-muxmesh-token");
        if !valid_host(&host) || !state.auth_ok(&host, &token) {
            tracing::warn!("/agent/ws host={host} auth=FAIL -> 401 (no upgrade)");
            return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
        }
        // single source of truth: the handler must NOT re-read attacker-
        // influenced headers to decide identity — use what was verified here.
        req.extensions_mut().insert(AuthenticatedHost(host));
    }
    next.run(req).await
}

/// Identity verified by ws_auth_gate; consumed by agent_ws via axum's
/// FromRequestParts — the handler never re-reads attacker-influenced headers.
#[derive(Clone, Debug)]
struct AuthenticatedHost(String);

impl FromRequestParts<Arc<AppState>> for AuthenticatedHost {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<AuthenticatedHost>()
            .cloned()
            .unwrap_or_else(|| AuthenticatedHost(String::new())))
    }
}

/// GET /state: open (badge pollers and `tree` are read-only and the state
/// holds no secrets).
async fn get_state(State(state): State<Arc<AppState>>) -> Response {
    let body = fs::read_to_string(&state.state_path).unwrap_or_else(|_| "{\"hosts\":{}}".into());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// POST /state: per-machine auth (unknown host or bad token => 401). Tokens
/// are minted OFFLINE by `muxmesh-hub generate-token <host>` — there is NO
/// registration endpoint: an unregistered machine cannot introduce itself.
async fn post_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let host_hdr = AppState::hdr(&headers, "x-muxmesh-host");
    let got_token = AppState::hdr(&headers, "x-muxmesh-token");
    if !state.auth_ok(&host_hdr, &got_token) {
        tracing::warn!(
            "POST /state host={host_hdr} auth=FAIL (registered: {})",
            state.tokens.lock().map(|s| s.hosts.len()).unwrap_or(0)
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }

    if body.is_empty() || body.len() > MAX_CMD_BODY {
        return (StatusCode::BAD_REQUEST, "bad length\n").into_response();
    }

    // host: X-Muxmesh-Host header (source of truth), else the payload field
    let mut st: HostState = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid payload: {e}\n"),
            )
                .into_response()
        }
    };
    if st.seen == 0 {
        st.seen = now_unix(); // clockless agent -> stamp on receipt
    } else {
        // clock-skew guard: a future `seen` would stay "fresh" forever; clamp it.
        let now = now_unix();
        if st.seen > now + 60 {
            st.seen = now;
        }
    }
    let host_key = if host_hdr.is_empty() {
        st.host.clone()
    } else {
        host_hdr.clone()
    };
    if host_key.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing host\n").into_response();
    }

    // TRUST BOUNDARY: host and session names become tmux target strings later
    // (poller `-t`). Whitelist them here so downstream tmux argv is provably
    // [A-Za-z0-9_.-]+ and target injection is structurally impossible.
    if !valid_host(&host_key) {
        return (StatusCode::BAD_REQUEST, "invalid host name\n").into_response();
    }
    for s in &st.sessions {
        if !valid_name(&s.name) || (!s.active_window.is_empty() && !valid_name(&s.active_window)) {
            // name the rejected value: tmux allows ':' in window names, our
            // whitelist does not -> without this the 400 is undiagnosable.
            tracing::warn!(
                "rejecting session name='{}' active_window='{}' from host={host_hdr} (valid_name=[A-Za-z0-9_-])",
                s.name,
                s.active_window
            );
            return (StatusCode::BAD_REQUEST, "invalid session name\n").into_response();
        }
    }

    // merge under the write lock -> atomic write. Raw JSON doc (not StateDoc)
    // so the out-of-band last_cmd ack mirror survives the agent's next merge.
    let merge = {
        let _g = state.write_lock.lock();
        merge_state(&state.state_path, &host_key, &st)
    };
    match merge {
        Ok(trace) => {
            tracing::info!("state updated: {host_key} {trace}");
            (StatusCode::OK, "ok\n").into_response()
        }
        Err(e) => {
            tracing::error!("POST /state merge {host_key}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response()
        }
    }
}

/// Merge one HostState into state.json (caller holds write_lock).
/// Returns the debug trace ("armed=.. sessions=[..]").
fn merge_state(path: &Path, host_key: &str, st: &HostState) -> std::io::Result<String> {
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
        st.armed,
        st.sessions
            .iter()
            .map(|s| format!("{}:{}w/active={}", s.name, s.windows, s.active_window))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut entry = serde_json::to_value(st).map_err(std::io::Error::other)?;
    // carry the ack mirror across the merge (the agent payload has no last_cmd)
    if let Some(prev) = hosts.get(host_key).and_then(|h| h.get("last_cmd")) {
        entry["last_cmd"] = prev.clone();
    }
    hosts[host_key] = entry;
    let json = serde_json::to_string_pretty(&raw).map_err(std::io::Error::other)?;
    write_bytes_atomic(path, json.as_bytes())?;
    tracing::debug!(
        "state updated host={host_key} {trace} -> {}",
        path.display()
    );
    Ok(trace)
}

/// POST /cmd: authenticate the driver, check dst registered + has a live WS
/// channel right now, enqueue the frame on that channel, answer 202 queued.
/// The hub NEVER dials the agent — delivery happens on the agent's own socket.
/// shortcut: unbounded mpsc drops HEAD's 10 s queue TTL (a frame queued while
/// the agent is slow arrives late); upgrade to a bounded/TTL'd queue if stale
/// bar toggles firing late ever matter. `shortcut:` blocking file IO in
/// record_ack on a tokio worker — state.json is a few KB, homelab scale.
async fn post_cmd(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // HEAD parity: empty/over-cap body -> 400 "bad length" (no axum
    // DefaultBodyLimit on this route: it would answer 413 before the handler
    // and break the CLI's literal status-line contract).
    if body.is_empty() || body.len() > MAX_CMD_BODY {
        return (StatusCode::BAD_REQUEST, "bad length\n").into_response();
    }

    // closed list: unknown verbs die here, before any dispatch.
    let env: CommandEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid payload: {e}\n"),
            )
                .into_response()
        }
    };

    // AUTH driver: same registry + same KDF verify as /state (KDF outside lock).
    // dst is NOT checked before this: answering 400-before-401 would let an
    // unauthenticated caller enumerate registered hosts.
    let host_hdr = AppState::hdr(&headers, "x-muxmesh-host");
    let got_token = AppState::hdr(&headers, "x-muxmesh-token");
    if !state.auth_ok(&host_hdr, &got_token) {
        tracing::warn!("POST /cmd src={host_hdr} auth=FAIL");
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }

    let dst = env.dst.clone();
    if dst.is_empty() || !valid_host(&dst) {
        return (StatusCode::BAD_REQUEST, "invalid dst\n").into_response();
    }
    let registered = state
        .tokens
        .lock()
        .map(|s| s.hosts.contains_key(&dst))
        .unwrap_or(false);
    if !registered {
        tracing::warn!("POST /cmd dst={dst} -> 403 dst not registered");
        return (StatusCode::FORBIDDEN, "dst not registered\n").into_response();
    }
    // the agent must hold a live WS channel RIGHT NOW — the hub has no other
    // way (and no right) to reach it.
    let enqueued = {
        let ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        match ch.get(&dst) {
            Some(s) => match serde_json::to_string(&env) {
                Ok(frame) => s.tx.send(frame).is_ok(),
                // a frame that cannot be serialized must never reach the agent
                // as an empty text frame — drop it and name it in the log.
                Err(e) => {
                    tracing::error!("POST /cmd id={} serialize failed: {e}", env.id);
                    false
                }
            },
            None => false,
        }
    };
    if !enqueued {
        tracing::warn!("POST /cmd dst={dst} -> 409 target not connected");
        return (StatusCode::CONFLICT, "target not connected\n").into_response();
    }

    tracing::info!("POST /cmd src={host_hdr} -> queued dst={dst} id={}", env.id);
    // serde-serialized (env.id is CLI-generated today, but never hand-build JSON)
    let body = Json(serde_json::json!({"v":1,"id":env.id,"queued":true}));
    (StatusCode::ACCEPTED, body).into_response()
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// GET /agent/ws: the gate middleware already authenticated host+token from
/// headers BEFORE this runs (bad creds = 401, no upgrade ever). Complete the
/// upgrade and run the dispatch session. One channel per host — a second
/// connection from the same host replaces the first (logged, old task exits
/// via gen).
async fn agent_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    AuthenticatedHost(host): AuthenticatedHost,
) -> Response {
    // identity comes from the gate's verification, never from a second header read
    ws.on_upgrade(move |socket| handle_agent(socket, host, state))
}

/// Per-agent WS session: drain the mpsc queue to the sink, read acks into
/// state.json last_cmd, ping every 30 s, close after 2 missed pongs.
/// No read timeout — a quiet channel is alive; the pings prove it.
async fn handle_agent(mut socket: WebSocket, host: String, state: Arc<AppState>) {
    let gen = WS_GEN.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let mut ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = ch.insert(host.clone(), Sender { tx, gen }) {
            tracing::info!(
                "/agent/ws host={host}: replacing existing connection (gen {} -> {gen})",
                old.gen
            );
        }
    }
    tracing::info!("/agent/ws host={host} connected (gen {gen})");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = ping.tick().await; // first tick fires immediately: skip it
    let mut missed: u32 = 0;

    loop {
        tokio::select! {
            // push queued commands to the agent (frames arrive via POST /cmd)
            maybe_frame = rx.recv() => {
                match maybe_frame {
                    Some(frame) => {
                        if socket.send(Message::Text(frame.into())).await.is_err() {
                            tracing::warn!("/agent/ws host={host}: send failed");
                            break;
                        }
                    }
                    // sender dropped (replaced session): stop touching this socket
                    None => break,
                }
            }
            // acks and pongs from the agent. tungstenite answers our pings via
            // the client's auto-pong and delivers Pong HERE; a wedged half-open
            // socket never pongs -> missed keeps climbing.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Ack>(&text) {
                            Ok(ack) => {
                                tracing::info!(
                                    "/agent/ws host={host} ack id={} ok={}{}",
                                    ack.id,
                                    ack.ok,
                                    ack.err.as_deref().map(|e| format!(" err={e}")).unwrap_or_default()
                                );
                                record_ack(&state, &host, &ack);
                            }
                            Err(e) => tracing::warn!("/agent/ws host={host}: bad ack frame: {e}"),
                        }
                    }
                    Some(Ok(Message::Pong(_))) => missed = 0,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // binary/ping from agent: ignored
                    Some(Err(e)) => {
                        tracing::warn!("/agent/ws host={host}: recv: {e}");
                        break;
                    }
                }
            }
            _ = ping.tick() => {
                missed += 1;
                if missed > MAX_MISSED_PONGS {
                    tracing::warn!("/agent/ws host={host}: {MAX_MISSED_PONGS} unanswered pings -> closing (dead peer)");
                    let _ = socket.send(Message::Close(Some(CloseFrame {
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: "dead peer".into(),
                    }))).await;
                    break;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // socket closed: remove OUR channel (a replacement connection keeps its own)
    {
        let mut ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        if ch.get(&host).is_some_and(|s| s.gen == gen) {
            ch.remove(&host);
        }
    }
    tracing::info!("/agent/ws host={host} disconnected (gen {gen})");
}

/// Mirror an agent ack into state.json's last_cmd (best effort — the driver
/// reads it back via GET /state). Never fails the WS session. Blocking file
/// IO on a tokio worker: state.json is a few KB, acceptable here.
/// shortcut: inline fs::write — move to spawn_blocking if state.json ever
/// grows large enough to stall a worker.
fn record_ack(state: &AppState, host: &str, ack: &Ack) {
    let _g = state.write_lock.lock();
    let path = &state.state_path;
    let mut raw: serde_json::Value = fs::read_to_string(path)
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
        if let Err(e) = write_bytes_atomic(path, json.as_bytes()) {
            tracing::warn!("last_cmd write {host}: {e}");
        }
    }
}

/// Console mode: `muxmesh-hub generate-token <host>` — mint a machine token,
/// register it in tokens.json, print it ONCE. herdr-style `add machine`:
/// operator runs this on the pivot, pastes the printed token on the agent
/// (~/.config/muxmesh/token), done. Re-issue revokes the host's old token.
/// ONE token per host: it authenticates POST /state AND the /agent/ws handshake.
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
        "muxmesh-hub: host '{host}' paired — machine token in {}. Paste it on the agent as ~/.config/muxmesh/token (0600). The agent uses it for POST /state and its /agent/ws handshake. Shown once.",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state(name: &str) -> PathBuf {
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
        let state = AppState {
            tokens: Mutex::new(TokenStore::default()),
            state_path: p.clone(),
            write_lock: Mutex::new(()),
            channels: Mutex::new(HashMap::new()),
        };
        record_ack(&state, "host-a", &Ack::ok(1, "c-1"));
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["id"], "c-1");
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["ok"], true);
        // unknown host -> no panic, file untouched shape
        record_ack(&state, "ghost", &Ack::err(1, "c-2", "nope"));
        fs::remove_dir_all(p.parent().unwrap()).ok();
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

    #[test]
    fn merge_state_preserves_out_of_band_last_cmd() {
        let p = tmp_state("merge-lastcmd");
        let state = AppState {
            tokens: Mutex::new(TokenStore::default()),
            state_path: p.clone(),
            write_lock: Mutex::new(()),
            channels: Mutex::new(HashMap::new()),
        };
        record_ack(&state, "host-a", &Ack::ok(1, "c-9"));
        let st = HostState {
            host: "host-a".into(),
            armed: true,
            seen: now_unix(),
            sessions: vec![],
        };
        let _g = state.write_lock.lock();
        merge_state(&p, "host-a", &st).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(raw["hosts"]["host-a"]["last_cmd"]["id"], "c-9");
        assert_eq!(raw["hosts"]["host-a"]["armed"], true);
        fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}
