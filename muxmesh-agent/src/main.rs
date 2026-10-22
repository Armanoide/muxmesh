//! muxmesh-agent — daemon on a remote machine (tokio + tokio-tungstenite).
//!
//! Reads its LOCAL tmux (unix socket, free) and POSTs the state to the hub
//! (ureq — short outbound HTTP, blocking is fine). No ssh, no forwarded SSH
//! agent consulted -> no credential-manager popups.
//!
//! Commands arrive on the agent's OWN OUTBOUND WebSocket (/agent/ws): the
//! agent dials the hub at startup and holds the channel open forever
//! (reconnect loop, exponential backoff 1s->30s, reset on success). The hub
//! NEVER dials the agent — the agent serves NOTHING inbound. A pushed
//! CommandEnvelope (dst == own host) executes on the live tmux socket and the
//! agent replies with an ack frame {v,id,ok,err}.
//!
//! Keepalive: the agent sends a WS ping every 30 s; tungstenite answers the
//! hub's pings automatically (never reply manually). Two consecutive sent
//! pings without a pong -> drop the connection -> reconnect loop.
//!
//! Cadence: heartbeat every 15 s by default (MUXMESH_INTERVAL). tmux alive -> armed:true.
//! tmux absent -> armed:false (the hub disarms at TTL).

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{now_unix, Ack, CommandEnvelope, HostState, RemoteSession};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Keepalive cadence: one WS ping per 30 s.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Drop the connection after this many consecutive unanswered sent pings.
const MAX_MISSED_PONGS: u64 = 2;

/// Live local tmux? sessions + active window per session.
/// Returns (armed, sessions). `socket` is the agent's configured tmux socket
/// ("" = tmux default); when set it is passed as -S so the agent scans its
/// own server, not /tmp/tmux-<uid>/default. Without this the agent on a custom
/// socket never sees its tmux and posts armed=false forever.
fn scan_tmux(socket: &str) -> (bool, Vec<RemoteSession>) {
    // tmux ls -> "sess: 3 windows (created ...)" per line, error if no server
    let mut cmd = Command::new("tmux");
    if !socket.is_empty() {
        cmd.arg("-S").arg(socket);
    }
    let out = match cmd
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
        let active = active_window(socket, &name);
        sessions.push(RemoteSession {
            name,
            windows,
            active_window: active,
        });
    }
    (true, sessions)
}

/// name of the active window of a local tmux session.
fn active_window(socket: &str, session: &str) -> String {
    let mut cmd = Command::new("tmux");
    if !socket.is_empty() {
        cmd.arg("-S").arg(socket);
    }
    let out = cmd
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
fn build_state(host: &str, socket: &str) -> HostState {
    let (armed, sessions) = scan_tmux(socket);
    HostState {
        host: host.to_string(),
        armed,
        seen: now_unix(),
        sessions,
    }
}

#[tokio::main]
async fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default. Token = separate file.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let _args = muxmesh_core::log::parse_verbose_flag(&args);
    muxmesh_core::log::init();
    let cfg = muxmesh_core::AgentConfig::load();
    let url = cfg.url();

    // The machine token is minted OFFLINE on the hub (`muxmesh-hub
    // generate-token <host>`) and pasted here as ~/.config/muxmesh/token.
    // It signs BOTH POST /state and the /agent/ws handshake. The file is
    // re-read per push/per reconnect, so pasting (or rotating) it takes
    // effect without restarting the agent. Absent => 401s, with a hint.
    if muxmesh_core::read_token().as_str().is_empty() {
        tracing::warn!("no token (~/.config/muxmesh/token) -> the hub will reject (401). Mint one: muxmesh-hub generate-token {}", cfg.host);
    }
    tracing::info!("host={} -> {url} (interval {}s)", cfg.host, cfg.interval);

    // Embedded badge poller: the agent's own machine gets @muxmesh-status
    // badges automatically. It targets the agent's own push target
    // ([agent] hub), not the hub-side bind default — a remote agent
    // configured with only [agent] hub still works. Blocking ureq+tmux ->
    // dedicated std::thread, off the tokio runtime.
    {
        let mut poll_cfg = muxmesh_core::HubConfig::load();
        poll_cfg.poll_hub = muxmesh_core::agent_poll_hub(&cfg);
        thread::spawn(move || muxmesh_core::poll::run_embedded(&poll_cfg));
    }

    // heartbeat: debug per cycle (quiet at default INFO); WARN on push failure;
    // INFO only when armed/sessions state CHANGES — a running agent must not
    // spam the pane it lives in. Blocking ureq+tmux -> dedicated std::thread
    // (same pattern as the poller), off the tokio runtime.
    let ws = ws_channel(Arc::new(cfg.clone()));
    {
        let cfg = cfg.clone();
        thread::spawn(move || {
            let mut last = (false, usize::MAX);
            loop {
                let token = muxmesh_core::read_token();
                let state = build_state(&cfg.host, &resolve_socket(&cfg));
                let ok = push(&cfg.url(), &cfg.host, token.as_str(), &state);
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
        });
    }

    ws.await; // the WS channel reconnects forever; main just keeps it alive
}

/// Persistent outbound WS channel: dial /agent/ws with the machine token
/// headers, execute pushed commands, ack them. Reconnect with exponential
/// backoff 1s -> 30s (reset on a successful session); logs state changes only
/// (never the token).
/// shortcut: no jitter on the backoff — fine at homelab scale, add jitter if
/// many agents ever hammer a restarting hub.
async fn ws_channel(cfg: Arc<muxmesh_core::AgentConfig>) {
    let url = {
        // core's ws_url() is the legacy /ws path; the axum hub serves both,
        // canonical /agent/ws wins.
        let legacy = cfg.ws_url();
        match legacy.strip_suffix("/ws") {
            Some(prefix) => format!("{prefix}/agent/ws"),
            None => legacy,
        }
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        let token = muxmesh_core::read_token().as_str().to_string();
        if token.is_empty() {
            tracing::warn!("ws: no token -> retrying in 1s");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        match connect_with_auth(&url, &cfg.host, &token).await {
            Ok((ws, resp)) => {
                tracing::info!("ws: connected to {url} (status {})", resp.status());
                ws_session(&cfg, ws).await;
                tracing::warn!("ws: channel closed -> reconnecting");
                backoff = Duration::from_secs(1); // success resets the ramp
            }
            Err(e) => {
                // revoked/bad token: 100k-round KDF per handshake is real CPU —
                // back off hard; re-reads the token file each attempt so pasting
                // a fresh one recovers without restart.
                let denied = matches!(
                    &e,
                    tokio_tungstenite::tungstenite::Error::Http(resp) if resp.status() == 401
                );
                if denied {
                    backoff = Duration::from_secs(30);
                }
                tracing::warn!(
                    "ws: connect {url} failed: {e} -> retry in {}s",
                    backoff.as_secs()
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Dial the hub's /agent/ws with X-Muxmesh-Host + X-Muxmesh-Token (own
/// machine token — the same identity as POST /state, zero relay tokens).
#[allow(clippy::result_large_err)] // tungstenite::Error is theirs, not ours to box
async fn connect_with_auth(
    url: &str,
    host: &str,
    token: &str,
) -> tokio_tungstenite::tungstenite::Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let mut req = url.into_client_request().map_err(|_| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid ws url",
        ))
    })?;
    // header values must be visible ASCII: a malformed pasted token must not
    // panic the agent — fail the attempt, the loop re-reads the file.
    let (hv, tv) = match (host.parse(), token.parse()) {
        (Ok(h), Ok(t)) => (h, t),
        _ => {
            tracing::error!("ws: host/token contain invalid header characters (check token file)");
            return Err(tokio_tungstenite::tungstenite::Error::Io(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid header value"),
            ));
        }
    };
    let h = req.headers_mut();
    h.insert("X-Muxmesh-Host", hv);
    h.insert("X-Muxmesh-Token", tv);
    connect_async(req).await
}

/// Read pushed frames until the socket dies: parse the closed-list envelope,
/// execute on the live tmux socket (Command is std-blocking; shortcut: run
/// inline — a hung tmux client stalls the reader, acceptable at homelab
/// scale; wrap dispatch in spawn_blocking if it ever bites), ack.
/// Also: send our own ping every 30 s and drop the connection after
/// MAX_MISSED_PONGS unanswered pings (dead hub). tungstenite answers hub
/// pings with pong automatically — never reply manually.
type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_session(cfg: &muxmesh_core::AgentConfig, ws: Ws) {
    let (mut sink, mut stream) = ws.split();
    // liveness counters are SESSION-local: statics shared across reconnects
    // would corrupt the missed-pong accounting between sessions.
    let last_ping_seq = Arc::new(AtomicU64::new(0)); // last ping WE sent
    let alive_seq = Arc::new(AtomicU64::new(0)); // last ping the hub answered
                                                 // outbound frames (acks + keepalive pings) go through one channel so a
                                                 // single task owns the sink (a SplitSink cannot be shared).
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // ping loop: outbound keepalive, seq-stamped so pongs prove liveness.
    let tx_p = tx.clone();
    let last_ping_p = last_ping_seq.clone();
    let pinger = async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ping.tick().await;
            let seq = last_ping_p.fetch_add(1, Ordering::Relaxed) + 1;
            if tx_p.send(Message::Ping(seq.to_string().into())).is_err() {
                return; // writer gone -> session over
            }
        }
    };

    // writer task: the ONLY owner of the sink.
    let writer = async {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                return; // socket error -> session over
            }
        }
    };

    let last_ping_r = last_ping_seq.clone();
    let alive_r = alive_seq.clone();
    let reader = async move {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Some(ack) = dispatch_command(cfg, &cfg.host, text.as_str()) {
                        if tx.send(Message::Text(ack.into())).is_err() {
                            return;
                        }
                    }
                }
                Ok(Message::Pong(data)) => {
                    // tungstenite echoes the ping payload back verbatim: a pong
                    // carrying our latest sent seq proves the hub is alive.
                    let seq: u64 = String::from_utf8_lossy(&data).parse().unwrap_or(0);
                    if seq != 0 && seq == last_ping_r.load(Ordering::Relaxed) {
                        alive_r.store(seq, Ordering::Relaxed);
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {} // binary/ping: ping auto-answered by tungstenite
                Err(e) => {
                    tracing::warn!("ws: recv error: {e}");
                    return;
                }
            }
        }
    };

    // Dead-hub watchdog: MAX_MISSED_PONGS pings sent without a matching pong
    // -> end the session so the reconnect loop takes over. A silent half-open
    // socket surfaces here, not as a read timeout (there is none).
    let last_ping_w = last_ping_seq.clone();
    let alive_w = alive_seq.clone();
    let watchdog = async move {
        loop {
            tokio::time::sleep(PING_INTERVAL).await;
            let last = last_ping_w.load(Ordering::Relaxed);
            if last.saturating_sub(alive_w.load(Ordering::Relaxed)) >= MAX_MISSED_PONGS {
                tracing::warn!(
                    "ws: {MAX_MISSED_PONGS} unanswered pings -> dropping connection (dead hub)"
                );
                return;
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = pinger => {}
        _ = writer => {}
        _ = watchdog => {}
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
        let s = build_state("host-a", "");
        assert_eq!(s.host, "host-a");
        // seen = now_unix() -> recent; scan_tmux depends on real tmux (may be absent here).
        assert!(s.seen > 1_700_000_000);
    }

    #[test]
    fn payload_roundtrips_through_core_contract() {
        let s = build_state("host-b", "");
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
