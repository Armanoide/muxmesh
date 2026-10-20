//! muxmesh-hub — state aggregator daemon (runs on the always-on pivot machine).
//!
//! Listens on TCP :9997. Agents POST /state (JSON HostState); the hub merges
//! into state.json with ATOMIC writes (tmp + rename) so readers (tmux status
//! bar, CLI) never see a half-written file.
//!
//! The hub NEVER executes commands: it only stores and serves state.
//! std::net only -> no network dependency, offline build.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use muxmesh_core::{now_unix, valid_host, valid_name, HostState, HubConfig, StateDoc, TokenStore};

/// Monotonic counter for unique tmp filenames (see write_atomic).
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

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

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default.
    let cfg = HubConfig::load();
    // Per-machine token registry (digests only). Empty pair key => closed.
    let tokens_path = cfg.tokens_path.clone();
    let store: Arc<Mutex<TokenStore>> = Arc::new(Mutex::new(TokenStore::load(&tokens_path)));
    if store.lock().unwrap().hosts.is_empty() {
        eprintln!(
            "muxmesh-hub: no registered hosts in {} -> every /state POST is 401 until an agent pairs.",
            tokens_path.display()
        );
    }

    let bind = cfg.bind.clone();
    let state_path = cfg.state_path.clone();

    // purge once at startup (expired entries disappear from the file)
    if let Err(e) = purge_and_write(&state_path, cfg.ttl) {
        eprintln!("muxmesh-hub: initial purge failed: {e}");
    }

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("muxmesh-hub: bind {bind} failed: {e}");
            process::exit(1);
        }
    };
    eprintln!(
        "muxmesh-hub: listening on {bind} -> {} (tokens: {}, pair: {})",
        state_path.display(),
        tokens_path.display(),
        if cfg.pair_key.is_empty() {
            "CLOSED"
        } else {
            "open with key"
        }
    );

    // serialize file writes (one POST at a time)
    let lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

    loop {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                let path = state_path.clone();
                let tokpath = tokens_path.clone();
                let lock = lock.clone();
                let store = store.clone();
                let pair = cfg.pair_key.clone();
                // backpressure BEFORE spawning: over cap -> 503, no thread, no read.
                match Permit::acquire() {
                    Some(permit) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        thread::spawn(move || {
                            let _permit = permit; // released when the handler returns
                            if let Err(e) =
                                handle(stream, peer, &path, &tokpath, &lock, &store, &pair)
                            {
                                eprintln!("muxmesh-hub: {peer}: {e}");
                            }
                        });
                    }
                    None => {
                        let _ = respond(&mut stream, 503, "overloaded\n");
                    }
                }
            }
            Err(e) => eprintln!("muxmesh-hub: accept: {e}"),
        }
    }
}

fn handle(
    mut stream: TcpStream,
    _peer: std::net::SocketAddr,
    path: &Path,
    tokpath: &Path,
    lock: &Mutex<()>,
    store: &Mutex<TokenStore>,
    pair_key: &str,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // request line: "POST /state HTTP/1.1"
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    // headers: Content-Length + host (X-Muxmesh-Host) + token + pair key
    let mut content_len: usize = 0;
    let mut header_host: Option<String> = None;
    let mut header_token: Option<String> = None;
    let mut header_pair: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        // header NAMES are case-insensitive (HTTP); the token VALUE keeps its case.
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("x-muxmesh-host:") {
            let val = line.split_once(':').map(|x| x.1).unwrap_or("");
            header_host = Some(val.trim().to_string());
        } else if lower.starts_with("x-muxmesh-token:") {
            let val = line.split_once(':').map(|x| x.1).unwrap_or("");
            header_token = Some(val.trim().to_string());
        } else if lower.starts_with("x-muxmesh-pair:") {
            let val = line.split_once(':').map(|x| x.1).unwrap_or("");
            header_pair = Some(val.trim().to_string());
        }
    }

    // POST /register: pairing endpoint, authenticated by the PAIR key (not a
    // machine token — the caller has none yet). Issues one per host; re-issue
    // revokes the previous token of that host (dockhand semantics).
    if method == "POST" && target.starts_with("/register") {
        if pair_key.is_empty()
            || !muxmesh_core::token_matches(pair_key, header_pair.as_deref().unwrap_or(""))
        {
            return respond(&mut stream, 401, "pairing closed or bad pair key\n");
        }
        if content_len == 0 || content_len > 4096 {
            return respond(&mut stream, 400, "bad length\n");
        }
        let mut body = vec![0u8; content_len];
        reader.read_exact(&mut body)?;
        let req: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return respond(&mut stream, 422, &format!("invalid payload: {e}\n")),
        };
        let host = req.get("host").and_then(|h| h.as_str()).unwrap_or("");
        if !valid_host(host) {
            return respond(&mut stream, 400, "invalid host name\n");
        }
        // M3: issue + snapshot + persist in ONE critical section, so a
        // concurrent register/revoke can never write a ledger that omits
        // the token we just handed out.
        let token = {
            let mut st = store
                .lock()
                .map_err(|e| std::io::Error::other(format!("lock: {e}")))?;
            let token = st.issue(host)?;
            persist_tokens_locked(tokpath, lock, &st)?;
            token
        };
        eprintln!("muxmesh-hub: registered host {host}");
        return respond_json(&mut stream, 201, &serde_json::json!({"token": token}));
    }

    // AUTH: GET /state is open (badge pollers and `tree` are read-only and
    // the state holds no secrets); writes require a registered machine token.
    if method == "GET" && target.starts_with("/state") {
        let body = fs::read_to_string(path).unwrap_or_else(|_| "{\"hosts\":{}}".into());
        return respond(&mut stream, 200, &body);
    }

    // per-machine auth for everything else (POST /state). Unknown host or
    // bad token => 401 (pair first with POST /register).
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
                && muxmesh_core::verify_token(&got_token, &r.hash) => {}
        _ => return respond(&mut stream, 401, "unauthorized\n"),
    }

    if method != "POST" || !target.starts_with("/state") {
        return respond(&mut stream, 404, "not found\n");
    }

    // body capped (1 MiB): an agent sends a few KB, beyond that = abuse
    if content_len == 0 || content_len > 1_048_576 {
        return respond(&mut stream, 400, "bad length\n");
    }

    let mut body = vec![0u8; content_len];
    reader.read_exact(&mut body)?;

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
        host_hdr
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
            return respond(&mut stream, 400, "invalid session name\n");
        }
    }

    // merge under lock -> atomic write
    let _g = lock
        .lock()
        .map_err(|e| std::io::Error::other(format!("lock: {e}")))?;
    let mut doc = read_doc(path);
    doc.hosts.insert(host_key.clone(), state);
    write_atomic(path, &doc)?;

    respond(&mut stream, 200, "ok\n")
}

/// Persist tokens.json atomically (tmp + rename). Caller must already hold the
/// store lock (M3: snapshot + write in one critical section); `lock` only
/// serializes file writers. M2: no unwrap on the store lock — that happens in
/// the caller now; here we only take the file lock with map_err discipline.
fn persist_tokens_locked(path: &Path, lock: &Mutex<()>, store: &TokenStore) -> std::io::Result<()> {
    let json = store.to_json_pretty().map_err(std::io::Error::other)?;
    let _g = lock
        .lock()
        .map_err(|e| std::io::Error::other(format!("lock: {e}")))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
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
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = doc.to_json_pretty().map_err(std::io::Error::other)?;
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(json.as_bytes())?;
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

/// JSON response (register reply carries the issued token once).
fn respond_json(
    stream: &mut TcpStream,
    code: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let raw = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let status = match code {
        201 => "201 Created",
        _ => "200 OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        raw.len()
    )?;
    stream.write_all(&raw)?;
    stream.flush()
}

fn respond(stream: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        201 => "201 Created",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        404 => "404 Not Found",
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
