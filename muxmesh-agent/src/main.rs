//! muxmesh-agent — daemon on a remote machine.
//!
//! Reads its LOCAL tmux (unix socket, free) and POSTs the state to the hub.
//! No ssh, no forwarded SSH agent consulted -> no credential-manager popups.
//!
//! Cadence: heartbeat every 15 s by default (MUXMESH_INTERVAL). tmux alive -> armed:true.
//! tmux absent -> armed:false (the hub disarms at TTL).

use std::io::Read;
use std::process::Command;
use std::thread;
use std::time::Duration;

use muxmesh_core::{now_unix, HostState, RemoteSession};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Canonical name of this machine (must match the ssh alias clients use).
/// Used at startup by AgentConfig::load (which re-reads MUXMESH_HOST); kept for tests/tools.
#[allow(dead_code)]
fn host_name() -> String {
    env_or("MUXMESH_HOST", &hostname())
}

/// short hostname (no domain), via `hostname -s`.
fn hostname() -> String {
    let out = Command::new("hostname").arg("-s").output().ok();
    match out {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Live local tmux? sessions + active window per session.
/// Returns (armed, sessions).
fn scan_tmux() -> (bool, Vec<RemoteSession>) {
    // tmux ls -> "sess: 3 windows (created ...)" per line, error if no server
    let out = match Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}\t#{session_windows}"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return (false, Vec::new()),
    };
    let text = String::from_utf8_lossy(&out);
    let mut sessions = Vec::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let name = it.next().unwrap_or("").to_string();
        let windows: u32 = it.next().and_then(|w| w.parse().ok()).unwrap_or(0);
        if name.is_empty() {
            continue;
        }
        let active = active_window(&name);
        sessions.push(RemoteSession {
            name,
            windows,
            active_window: active,
        });
    }
    (true, sessions)
}

/// name of the active window of a local tmux session.
fn active_window(session: &str) -> String {
    let out = Command::new("tmux")
        .args([
            "list-windows",
            "-t",
            session,
            "-F",
            "#{?window_active,#{window_name},}",
        ])
        .output()
        .ok();
    match out {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// POST the state to the hub. Returns true on HTTP 2xx.
fn push(url: &str, host: &str, token: &str, state: &HostState) -> bool {
    let body = match serde_json::to_vec(state) {
        Ok(b) => b,
        Err(_) => return false,
    };
    match ureq::post(url)
        .set("X-Muxmesh-Host", host)
        .set("X-Muxmesh-Token", token)
        .send_bytes(&body)
    {
        Ok(resp) => resp.status() < 300,
        Err(_) => false,
    }
}

/// The payload is built with muxmesh-core (state.rs) -> same contract as the hub.
fn build_state(host: &str) -> HostState {
    let (armed, sessions) = scan_tmux();
    HostState {
        host: host.to_string(),
        armed,
        seen: now_unix(),
        sessions,
    }
}

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default. Token = separate file.
    let cfg = muxmesh_core::AgentConfig::load();
    let url = cfg.url();

    // First-run pairing: no token yet + pair key configured => ask the hub
    // for OUR OWN token (never shared, revocable per machine).
    let mut token = muxmesh_core::read_token().as_str().to_string();
    if token.is_empty() {
        match register(&cfg) {
            Ok(t) => {
                token = t;
                eprintln!("muxmesh-agent: paired, token stored (0600).");
            }
            Err(e) => {
                eprintln!(
                    "muxmesh-agent: no token and pairing failed: {e} -> the hub will reject (401)."
                );
            }
        }
    }
    eprintln!(
        "muxmesh-agent: host={} -> {url} (interval {}s)",
        cfg.host, cfg.interval
    );

    // /cmd server on its own thread: clients POST closed-list commands here.
    if cfg.cmd_port > 0 {
        let cmd_cfg = cfg.clone();
        let cmd_token = token.clone();
        thread::spawn(move || serve_cmd(&cmd_cfg, &cmd_token));
    }

    loop {
        let state = build_state(&cfg.host);
        let ok = push(&url, &cfg.host, &token, &state);
        eprintln!(
            "muxmesh-agent: armed={} sessions={} push={}",
            state.armed,
            state.sessions.len(),
            if ok { "ok" } else { "FAIL" }
        );
        thread::sleep(Duration::from_secs(cfg.interval));
    }
}

/// First-run pairing: POST /register with the pair key, store the issued
/// token at ~/.config/muxmesh/token (0600). One HTTP call, then normal push.
fn register(cfg: &muxmesh_core::AgentConfig) -> Result<String, String> {
    if cfg.pair_key.is_empty() {
        return Err("agent.pair_key not set (and no token file)".into());
    }
    let body =
        serde_json::to_vec(&serde_json::json!({"host": cfg.host})).map_err(|e| e.to_string())?;
    let resp = ureq::post(&cfg.register_url())
        .set("X-Muxmesh-Pair", &cfg.pair_key)
        .send_bytes(&body)
        .map_err(|e| format!("{}: {e}", cfg.register_url()))?;
    if resp.status() != 201 {
        return Err(format!("hub answered {}", resp.status()));
    }
    let mut s = String::new();
    resp.into_reader()
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    let token = v
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("no token in reply")?
        .to_string();
    write_token_file(&token)?;
    Ok(token)
}

/// Write the issued token to <config_dir>/token, mode 0600 (tmp+rename).
fn write_token_file(token: &str) -> Result<(), String> {
    use std::io::Write;
    let dir = muxmesh_core::config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("token");
    let tmp = dir.join(format!("token.tmp.{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, token.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Serve POST /cmd on LAN: auth, closed-list deserialize, validate, execute on
/// the local tmux socket, answer Ack. GET /ping for liveness. std::net only.
fn serve_cmd(cfg: &muxmesh_core::AgentConfig, token: &str) {
    use std::net::TcpListener;

    let bind = format!("0.0.0.0:{}", cfg.cmd_port);
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("muxmesh-agent: /cmd bind {bind} failed: {e}");
            return;
        }
    };
    eprintln!(
        "muxmesh-agent: /cmd listening on {bind} (socket={:?})",
        cfg.tmux_socket
    );

    for stream in listener.incoming() {
        match stream {
            // One thread per connection: a slow client (or a tmux exec) must
            // never block the accept loop -> no spurious EAGAIN for others.
            Ok(mut stream) => {
                let cfg = cfg.clone();
                let tok = token.to_string();
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    if let Err(e) = handle_cmd(&mut stream, &cfg, &tok) {
                        eprintln!("muxmesh-agent: /cmd: {e}");
                    }
                });
            }
            Err(e) => eprintln!("muxmesh-agent: /cmd accept: {e}"),
        }
    }
}

/// Helper: respond with a serialized Ack as application/json.
fn respond_json(
    stream: &mut std::net::TcpStream,
    code: u16,
    ack: &muxmesh_core::Ack,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(ack).unwrap_or_else(|_| b"{}".to_vec());
    muxmesh_core::httpd::respond(stream, code, "application/json", &body)
}

fn handle_cmd(
    stream: &mut std::net::TcpStream,
    cfg: &muxmesh_core::AgentConfig,
    token: &str,
) -> std::io::Result<()> {
    use muxmesh_core::{httpd, token_matches, Ack, CommandEnvelope};

    let owned = stream.try_clone()?;
    let req = match httpd::read_request(owned, 64 * 1024) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("muxmesh-agent: /cmd parse: {e}");
            return httpd::respond(stream, 400, "text/plain", b"bad request\n");
        }
    };

    if !token_matches(token, req.header("x-muxmesh-token").unwrap_or("")) {
        return httpd::respond(stream, 401, "text/plain", b"unauthorized\n");
    }

    if req.method == "GET" && req.path.starts_with("/ping") {
        return httpd::respond(stream, 200, "application/json", b"{\"ok\":true}\n");
    }
    if req.method != "POST" || !req.path.starts_with("/cmd") {
        return httpd::respond(stream, 404, "text/plain", b"not found\n");
    }

    // closed list: an unknown verb or bad types fail here, before any exec.
    let env: CommandEnvelope = match serde_json::from_slice(&req.body) {
        Ok(e) => e,
        Err(e) => return respond_json(stream, 400, &Ack::err(1, "?", &format!("bad json: {e}"))),
    };
    if env.v != 1 {
        return respond_json(stream, 400, &Ack::err(env.v, &env.id, "unsupported v"));
    }
    if !env.cmd.args_valid() {
        return respond_json(stream, 400, &Ack::err(env.v, &env.id, "invalid args"));
    }

    let socket = if cfg.tmux_socket.is_empty() {
        "default"
    } else {
        &cfg.tmux_socket
    };
    match env.cmd.execute(socket) {
        Ok(()) => respond_json(stream, 200, &Ack::ok(env.v, &env.id)),
        Err(e) => respond_json(stream, 200, &Ack::err(env.v, &env.id, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> muxmesh_core::AgentConfig {
        muxmesh_core::AgentConfig {
            hub: "127.0.0.1".into(),
            port: 9997,
            host: "test-host".into(),
            interval: 15,
            cmd_port: 0,
            tmux_socket: String::new(),
            pair_key: String::new(),
        }
    }

    #[test]
    fn build_state_has_host_and_seen() {
        let s = build_state("host-a");
        assert_eq!(s.host, "host-a");
        // seen = now_unix() -> recent; scan_tmux depends on real tmux (may be absent here).
        assert!(s.seen > 1_700_000_000);
    }

    #[test]
    fn payload_roundtrips_through_core_contract() {
        let s = build_state("host-b");
        let json = serde_json::to_vec(&s).unwrap();
        let back: muxmesh_core::HostState = serde_json::from_slice(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn register_needs_pair_key_when_no_token() {
        // no pair key + unreachable hub => clear error, never a panic
        let e = register(&cfg()).unwrap_err();
        assert!(e.contains("pair_key"), "got: {e}");
    }

    #[test]
    fn hostname_is_not_empty() {
        assert!(!hostname().is_empty());
    }
}
