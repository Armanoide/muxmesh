//! muxmesh-agent — daemon on a remote machine.
//!
//! Reads its LOCAL tmux (unix socket, free) and POSTs the state to the hub.
//! No ssh, no forwarded SSH agent consulted -> no credential-manager popups.
//!
//! Cadence: heartbeat every 15 s by default (MUXMESH_INTERVAL). tmux alive -> armed:true.
//! tmux absent -> armed:false (the hub disarms at TTL).

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
        Err(e) => {
            tracing::error!("serialize state: {e}");
            return false;
        }
    };
    match ureq::post(url)
        .set("X-Muxmesh-Host", host)
        .set("X-Muxmesh-Token", token)
        .send_bytes(&body)
    {
        Ok(resp) => {
            tracing::debug!("POST {url} -> {}", resp.status());
            resp.status() < 300
        }
        Err(e) => {
            tracing::warn!("POST {url} -> error {e}");
            false
        }
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let _args = muxmesh_core::log::parse_verbose_flag(&args);
    muxmesh_core::log::init();
    let cfg = muxmesh_core::AgentConfig::load();
    let url = cfg.url();

    // The machine token is minted OFFLINE on the hub (`muxmesh-hub
    // generate-token <host>`) and pasted here as ~/.config/muxmesh/token.
    // The file is re-read every push, so pasting (or rotating) it takes
    // effect without restarting the agent. Absent => 401s, with a hint.
    if muxmesh_core::read_token().as_str().is_empty() {
        tracing::warn!("no token (~/.config/muxmesh/token) -> the hub will reject (401). Mint one: muxmesh-hub generate-token {}", cfg.host);
    }
    // hub-relayed /cmd is authenticated with the relay token (same mint,
    // second printed value). Absent => hub-relayed commands 401 here.
    if muxmesh_core::read_relay_token().as_str().is_empty() {
        tracing::warn!("no relay token (~/.config/muxmesh/relay-token) -> hub-relayed /cmd will 401. Paste the second value of: muxmesh-hub generate-token {}", cfg.host);
    }
    tracing::info!("host={} -> {url} (interval {}s)", cfg.host, cfg.interval);

    // Embedded badge poller: the agent's own machine gets @muxmesh-status
    // badges automatically (same thread pattern as /cmd below). It targets
    // the agent's own push target ([agent] hub), not the hub-side bind
    // default — a remote agent configured with only [agent] hub still works.
    {
        let mut poll_cfg = muxmesh_core::HubConfig::load();
        poll_cfg.poll_hub = muxmesh_core::agent_poll_hub(&cfg);
        thread::spawn(move || muxmesh_core::poll::run_embedded(&poll_cfg));
    }

    // /cmd server on its own thread: clients POST closed-list commands here.
    // It re-reads the token file per request for the same reason.
    if cfg.cmd_port > 0 {
        let cmd_cfg = cfg.clone();
        thread::spawn(move || serve_cmd(&cmd_cfg));
    }

    // heartbeat: debug per cycle (quiet at default INFO); WARN on push failure;
    // INFO only when armed/sessions state CHANGES — a running agent must not
    // spam the pane it lives in.
    let mut last = (false, usize::MAX);
    loop {
        let token = muxmesh_core::read_token().as_str().to_string();
        let state = build_state(&cfg.host);
        let ok = push(&url, &cfg.host, &token, &state);
        tracing::debug!(
            "armed={} sessions={} push={}",
            state.armed,
            state.sessions.len(),
            if ok { "ok" } else { "FAIL" }
        );
        if !ok {
            tracing::warn!(
                "armed={} sessions={} push=FAIL",
                state.armed,
                state.sessions.len()
            );
        } else if (state.armed, state.sessions.len()) != last {
            tracing::info!(
                "armed={} sessions={} push=ok",
                state.armed,
                state.sessions.len()
            );
        }
        last = (state.armed, state.sessions.len());
        thread::sleep(Duration::from_secs(cfg.interval));
    }
}

/// Serve POST /cmd on LAN: auth, closed-list deserialize, validate, execute on
/// the local tmux socket, answer Ack. GET /ping for liveness. std::net only.
fn serve_cmd(cfg: &muxmesh_core::AgentConfig) {
    use std::net::TcpListener;

    let bind = format!("0.0.0.0:{}", cfg.cmd_port);
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("/cmd bind {bind} failed: {e}");
            return;
        }
    };
    tracing::info!("/cmd listening on {bind} (socket={:?})", cfg.tmux_socket);

    for stream in listener.incoming() {
        match stream {
            // One thread per connection: a slow client (or a tmux exec) must
            // never block the accept loop -> no spurious EAGAIN for others.
            Ok(mut stream) => {
                let cfg = cfg.clone();
                thread::spawn(move || {
                    // secrets re-read per request: rotation without restart
                    let tok = muxmesh_core::read_token().as_str().to_string();
                    let relay = muxmesh_core::read_relay_token().as_str().to_string();
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    if let Err(e) = handle_cmd(&mut stream, &cfg, &tok, &relay) {
                        tracing::error!("/cmd: {e}");
                    }
                });
            }
            Err(e) => tracing::error!("/cmd accept: {e}"),
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

/// Resolve the tmux socket the agent controls: the configured one, or ""
/// meaning "default". A stale/default socket that does not exist on disk is
/// resolved to the live one (`tmux display -p '#{socket_path}'`) — macOS
/// sockets live in a per-user /tmp/claude-<uid>/ style dir that changes
/// between sessions, so a literal "default" often 404s while tmux itself
/// knows where its socket is.
fn resolve_socket(cfg: &muxmesh_core::AgentConfig) -> String {
    if !cfg.tmux_socket.is_empty() && std::path::Path::new(&cfg.tmux_socket).exists() {
        return cfg.tmux_socket.clone();
    }
    std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.tmux_socket.clone())
}

fn handle_cmd(
    stream: &mut std::net::TcpStream,
    cfg: &muxmesh_core::AgentConfig,
    token: &str,
    relay_token: &str,
) -> std::io::Result<()> {
    use muxmesh_core::{httpd, token_matches, Ack, CommandEnvelope};

    let owned = stream.try_clone()?;
    let req = match httpd::read_request(owned, 64 * 1024) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("/cmd parse: {e}");
            return httpd::respond(stream, 400, "text/plain", b"bad request\n");
        }
    };

    // two auth paths: the hub relays with the per-host relay secret, a legacy
    // direct client still authenticates with this machine's own token.
    // trace which path matched — never the secret itself.
    let got = req.header("x-muxmesh-token").unwrap_or("");
    let auth = if token_matches(relay_token, got) {
        Some("relay")
    } else if token_matches(token, got) {
        Some("direct")
    } else {
        None
    };
    match auth {
        Some(mode) => tracing::debug!("/cmd: auth={mode}"),
        None => {
            tracing::warn!(
                "/cmd: STOP at auth — token mismatch (own or relay file absent? rotated?)"
            );
            return httpd::respond(stream, 401, "text/plain", b"unauthorized\n");
        }
    }

    if req.method == "GET" && req.path.starts_with("/ping") {
        return httpd::respond(stream, 200, "application/json", b"{\"ok\":true}\n");
    }
    if req.method != "POST" || !req.path.starts_with("/cmd") {
        tracing::debug!(
            "/cmd: STOP at route — {} {} (expected POST /cmd)",
            req.method,
            req.path
        );
        return httpd::respond(stream, 404, "text/plain", b"not found\n");
    }

    // closed list: an unknown verb or bad types fail here, before any exec.
    let env: CommandEnvelope = match serde_json::from_slice(&req.body) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("/cmd: STOP at parse — bad json: {e}");
            return respond_json(stream, 400, &Ack::err(1, "?", &format!("bad json: {e}")));
        }
    };
    tracing::info!(
        "/cmd: received id={} src={} cmd={:?}",
        env.id,
        env.src,
        env.cmd
    );
    if env.v != 1 {
        tracing::warn!("/cmd: STOP at version — v={} (expected 1)", env.v);
        return respond_json(stream, 400, &Ack::err(env.v, &env.id, "unsupported v"));
    }
    if !env.cmd.args_valid() {
        tracing::warn!("/cmd: STOP at args_valid — cmd={:?}", env.cmd);
        return respond_json(stream, 400, &Ack::err(env.v, &env.id, "invalid args"));
    }

    // "" => resolve the live socket (see resolve_socket); a literal "default"
    // fails when tmux runs from a per-user socket dir macOS rotates.
    let socket = resolve_socket(cfg);
    // which tmux state is affected: the bar is a GLOBAL option, so it hits
    // every session/window on this socket — name them in the trace.
    let affected = tmux_sessions(&socket);
    tracing::info!("/cmd: executing on socket={socket} — affects: {affected}");
    match env.cmd.execute(&socket) {
        Ok(()) => {
            tracing::info!("/cmd: done id={} ok", env.id);
            respond_json(stream, 200, &Ack::ok(env.v, &env.id))
        }
        Err(e) => {
            tracing::error!("/cmd: STOP at exec — id={} socket={socket}: {e}", env.id);
            respond_json(stream, 200, &Ack::err(env.v, &env.id, &e))
        }
    }
}

/// Human-readable list of sessions on a socket, for the exec trace.
fn tmux_sessions(socket: &str) -> String {
    std::process::Command::new("tmux")
        .args([
            "-S",
            socket,
            "list-sessions",
            "-F",
            "#{session_name}(#{session_windows}w,attached=#{?session_attached}yes#{:}no)",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).replace('\n', " "))
        .unwrap_or_else(|| "(no sessions)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn hostname_is_not_empty() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn cmd_auth_accepts_relay_or_direct_denies_other() {
        use muxmesh_core::token_matches;
        let own = "own-token-aaaa";
        let relay = "relay-token-bbbb";
        // relay secret wins (hub-relayed), own token still works (legacy direct)
        assert!(token_matches(relay, relay));
        assert!(token_matches(own, own));
        // neither matches a foreign or empty token
        assert!(!token_matches(relay, own));
        assert!(!token_matches(own, relay));
        assert!(!token_matches("", ""));
    }
}
