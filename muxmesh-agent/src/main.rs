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
//!
//! Layout (lm-sanaga conventions):
//!   tmux.rs     local tmux scan + socket resolution (the only tmux spawns)
//!   ws.rs       outbound channel: dial, hold, reconnect forever
//!   session.rs  one live session: reader/writer/pinger/watchdog + dispatch
//!   main.rs     bootstrap: config, log channel, heartbeat thread, WS loop

mod tmux;
#[path = "ws.rs"]
mod ws;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use muxmesh_core::HostState;

use crate::tmux::build_state;
use crate::ws::session::WsLogLayer;

fn main() {
    // Config: env > ~/.config/muxmesh/config.toml > default. Token = separate file.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -v/--verbose anywhere -> RUST_LOG=muxmesh=debug (explicit RUST_LOG wins).
    let _args = muxmesh_core::log::parse_verbose_flag(&args);
    // Central-log channel: every agent log line goes to stderr AND, when the
    // WS is up, to the hub's hub.log (one file for the whole fleet).
    let (log_tx, log_rx) = tokio::sync::broadcast::channel::<String>(512);
    muxmesh_core::log::init_with_extra(Box::new(WsLogLayer(log_tx)));
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
    let ws = ws::ws_channel(Arc::new(cfg.clone()), log_rx);
    {
        let cfg = cfg.clone();
        thread::spawn(move || {
            let mut last = (false, usize::MAX);
            loop {
                let token = muxmesh_core::read_token();
                let state = build_state(&cfg.host, &tmux::resolve_socket(&cfg));
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

    // tokio runtime for the WS channel only (the threads above stay std).
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(ws); // the WS channel reconnects forever; main just keeps it alive
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
        assert_eq!(ws::session::dispatch_command(&c, "me", foreign), None);
        // bad json -> ignored
        assert_eq!(ws::session::dispatch_command(&c, "me", "not json"), None);
        // bad version -> ack err (id echoed)
        let oldv =
            r#"{"v":2,"id":"c-2","src":"driver","dst":"me","cmd":{"type":"BarShow"},"ts":1}"#;
        let ack: muxmesh_core::Ack =
            serde_json::from_str(&ws::session::dispatch_command(&c, "me", oldv).unwrap()).unwrap();
        assert_eq!(ack.id, "c-2");
        assert!(!ack.ok);
        // own BarHide: no live tmux here -> exec fails, but the ack must
        // echo the id (dispatch path proven without touching tmux state).
        let mine =
            r#"{"v":1,"id":"c-3","src":"driver","dst":"me","cmd":{"type":"BarHide"},"ts":1}"#;
        let ack: muxmesh_core::Ack =
            serde_json::from_str(&ws::session::dispatch_command(&c, "me", mine).unwrap()).unwrap();
        assert_eq!(ack.id, "c-3");
    }

    #[test]
    fn ws_url_is_hub_channel() {
        assert_eq!(cfg().ws_url(), "ws://127.0.0.1:9997/ws");
    }
}
