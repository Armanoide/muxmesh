//! muxmesh-agent — daemon on a remote machine.
//!
//! Reads its LOCAL tmux (unix socket, free) and POSTs the state to the hub.
//! No ssh, no forwarded SSH agent consulted -> no credential-manager popups.
//!
//! Commands arrive on the agent's OWN OUTBOUND WebSocket (/ws): the agent
//! dials the hub at startup and holds the channel open forever (auto-reconnect,
//! 2 s backoff). The hub NEVER dials the agent — the agent serves NOTHING
//! inbound. A pushed CommandEnvelope (dst == own host) executes on the live
//! tmux socket and the agent replies with an ack frame {v,id,ok,err}.
//!
//! Cadence: heartbeat every 15 s by default (MUXMESH_INTERVAL). tmux alive -> armed:true.
//! tmux absent -> armed:false (the hub disarms at TTL).

use std::process::Command;
use std::thread;
use std::time::Duration;

use muxmesh_core::{now_unix, Ack, CommandEnvelope, HostState, RemoteSession};
use tungstenite::{connect, Message};

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
    // It signs BOTH POST /state and the /ws handshake. The file is re-read
    // per push/per reconnect, so pasting (or rotating) it takes effect
    // without restarting the agent. Absent => 401s, with a hint.
    if muxmesh_core::read_token().as_str().is_empty() {
        tracing::warn!("no token (~/.config/muxmesh/token) -> the hub will reject (401). Mint one: muxmesh-hub generate-token {}", cfg.host);
    }
    tracing::info!("host={} -> {url} (interval {}s)", cfg.host, cfg.interval);

    // Embedded badge poller: the agent's own machine gets @muxmesh-status
    // badges automatically. It targets the agent's own push target
    // ([agent] hub), not the hub-side bind default — a remote agent
    // configured with only [agent] hub still works.
    {
        let mut poll_cfg = muxmesh_core::HubConfig::load();
        poll_cfg.poll_hub = muxmesh_core::agent_poll_hub(&cfg);
        thread::spawn(move || muxmesh_core::poll::run_embedded(&poll_cfg));
    }

    // WS command channel on its own thread: dial out, hold open, execute.
    {
        let ws_cfg = cfg.clone();
        thread::spawn(move || ws_channel(&ws_cfg));
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

/// Persistent outbound WS channel: connect /ws with the machine token headers,
/// execute pushed commands, ack them. Auto-reconnect every 2 s; logs state
/// changes only (never the token).
/// shortcut: fixed 2 s backoff, no jitter — fine at homelab scale, add
/// exponential+jitter if many agents ever hammer a restarting hub.
fn ws_channel(cfg: &muxmesh_core::AgentConfig) {
    let url = cfg.ws_url();
    loop {
        let token = muxmesh_core::read_token().as_str().to_string();
        if token.is_empty() {
            tracing::warn!("ws: no token -> retrying in 2s");
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        let backoff = match connect_with_auth(&url, &cfg.host, &token) {
            Ok((mut ws, resp)) => {
                tracing::info!("ws: connected to {url} (status {})", resp.status());
                ws_session(cfg, &mut ws);
                tracing::warn!("ws: channel closed -> reconnecting in 2s");
                Duration::from_secs(2)
            }
            Err(e) => {
                // revoked/bad token: 100k-round KDF per handshake is real CPU —
                // back off hard; re-reads the token file each attempt so pasting
                // a fresh one recovers without restart.
                let denied = matches!(
                    &e,
                    tungstenite::Error::Http(resp) if resp.status() == 401
                );
                let wait = if denied {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(2)
                };
                tracing::warn!(
                    "ws: connect {url} failed: {e} -> retry in {}s",
                    wait.as_secs()
                );
                wait
            }
        };
        thread::sleep(backoff);
    }
}

/// Dial the hub's /ws with X-Muxmesh-Host + X-Muxmesh-Token (own machine
/// token — the same identity as POST /state, zero relay tokens).
#[allow(clippy::result_large_err)] // tungstenite::Error is theirs, not ours to box
fn connect_with_auth(
    url: &str,
    host: &str,
    token: &str,
) -> tungstenite::Result<(
    tungstenite::WebSocket<WsStream>,
    tungstenite::handshake::client::Response,
)> {
    use tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request()?;
    let h = req.headers_mut();
    // header values must be visible ASCII: a malformed pasted token must not
    // panic the agent — fail the attempt, the loop re-reads the file.
    let (hv, tv) = match (host.parse(), token.parse()) {
        (Ok(h), Ok(t)) => (h, t),
        _ => {
            tracing::error!("ws: host/token contain invalid header characters (check token file)");
            return Err(tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid header value",
            )));
        }
    };
    h.insert("X-Muxmesh-Host", hv);
    h.insert("X-Muxmesh-Token", tv);
    connect(req)
}

/// The concrete stream type tungstenite's `connect()` returns for ws:// URLs.
type WsStream = tungstenite::stream::MaybeTlsStream<std::net::TcpStream>;

/// Read pushed frames until the socket dies: parse the closed-list envelope,
/// execute on the live tmux socket, ack. Non-command frames are ignored.
/// tungstenite answers hub pings with pong automatically inside read() —
/// that is the keepalive: a wedged half-open socket surfaces as a ping
/// timeout -> return -> the channel reconnects.
fn ws_session(cfg: &muxmesh_core::AgentConfig, ws: &mut tungstenite::WebSocket<WsStream>) {
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Some(ack) = dispatch_command(cfg, &cfg.host, text.as_str()) {
                    if ws.send(Message::Text(ack)).is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                if ws.send(Message::Pong(data)).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => {} // binary/pong: ignored
        }
    }
}

/// Pure dispatch decision: envelope -> ack JSON (no socket, no exec side
/// effects beyond the argv-scoped tmux call). dst mismatch or bad frame ->
/// no ack (None): the hub logs the queue TTL drop instead.
fn dispatch_command(cfg: &muxmesh_core::AgentConfig, host: &str, text: &str) -> Option<String> {
    let env: CommandEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("ws: bad frame (not a command envelope): {e}");
            return None;
        }
    };
    if env.dst != host {
        tracing::warn!(
            "ws: ignoring command id={} dst={} (not me: {host})",
            env.id,
            env.dst
        );
        return None;
    }
    tracing::info!(
        "ws: command id={} src={} cmd={:?}",
        env.id,
        env.src,
        env.cmd
    );

    let ack = if env.v != 1 {
        Ack::err(env.v, &env.id, "unsupported v")
    } else if !env.cmd.args_valid() {
        Ack::err(env.v, &env.id, "invalid args")
    } else {
        // "" => resolve the live socket (see resolve_socket); a literal "default"
        // fails when tmux runs from a per-user socket dir macOS rotates.
        let socket = resolve_socket(cfg);
        // the bar is a GLOBAL option: it hits every session/window on this
        // socket — name them in the trace.
        let affected = tmux_sessions(&socket);
        tracing::info!("ws: executing on socket={socket} — affects: {affected}");
        match env.cmd.execute(&socket) {
            Ok(()) => {
                tracing::info!("ws: done id={} ok", env.id);
                Ack::ok(env.v, &env.id)
            }
            Err(e) => {
                tracing::error!("ws: exec failed id={} socket={socket}: {e}", env.id);
                Ack::err(env.v, &env.id, &e)
            }
        }
    };
    Some(serde_json::to_string(&ack).unwrap_or_else(|_| "{}".into()))
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

    fn cfg() -> muxmesh_core::AgentConfig {
        muxmesh_core::AgentConfig {
            hub: "127.0.0.1".into(),
            port: 9997,
            host: "me".into(),
            interval: 15,
            tmux_socket: String::new(),
        }
    }

    #[test]
    fn dispatch_acks_own_command_and_skips_foreign() {
        let c = cfg();
        // foreign dst -> ignored, no ack (hub logs the TTL drop)
        let foreign = r#"{"v":1,"id":"c-1","src":"driver","dst":"someone-else","cmd":{"type":"BarHide"},"ts":1}"#;
        assert_eq!(dispatch_command(&c, "me", foreign), None);
        // bad json -> ignored
        assert_eq!(dispatch_command(&c, "me", "not json"), None);
        // bad version -> ack err (id echoed)
        let oldv =
            r#"{"v":2,"id":"c-2","src":"driver","dst":"me","cmd":{"type":"BarShow"},"ts":1}"#;
        let ack: Ack = serde_json::from_str(&dispatch_command(&c, "me", oldv).unwrap()).unwrap();
        assert_eq!(ack.id, "c-2");
        assert!(!ack.ok);
        // own BarHide: no live tmux here -> exec fails, but the ack must
        // echo the id (dispatch path proven without touching tmux state).
        let mine =
            r#"{"v":1,"id":"c-3","src":"driver","dst":"me","cmd":{"type":"BarHide"},"ts":1}"#;
        let ack: Ack = serde_json::from_str(&dispatch_command(&c, "me", mine).unwrap()).unwrap();
        assert_eq!(ack.id, "c-3");
    }

    #[test]
    fn ws_url_is_hub_channel() {
        assert_eq!(cfg().ws_url(), "ws://127.0.0.1:9997/ws");
    }
}
