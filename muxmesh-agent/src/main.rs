//! muxmesh-agent — daemon on a remote machine (tokio + tokio-tungstenite).
//!
//! The machine's ONLY gateway to the hub: it reads its LOCAL tmux and pushes
//! the state on its OWN OUTBOUND WebSocket every interval as a ReportState
//! frame. No HTTP, no ssh, no forwarded SSH agent consulted -> no
//! credential-manager popups.
//!
//! Commands arrive on the same WS (/agent/ws): the agent dials the hub at
//! startup and holds the channel open forever (reconnect loop, exponential
//! backoff 1s->30s, reset on success). The hub NEVER dials the agent — the
//! agent serves NOTHING inbound over the network. A pushed CommandEnvelope
//! (dst == own host) executes on the live tmux socket and the agent replies
//! with an ack frame {v,id,ok,err}.
//!
//! The CLI speaks to this process over a local Unix socket (~/.muxmesh/
//! agent.sock, see ipc.rs): send/ask-directory relayed on the WS. No agent
//! running -> no socket -> the CLI fails. That is intended (WS-Refacto §9).
//!
//! Keepalive: the agent sends a WS ping every 30 s; tungstenite answers the
//! hub's pings automatically (never reply manually). Two consecutive sent
//! pings without a pong -> drop the connection -> reconnect loop.
//!
//! Cadence: ReportState every 15 s by default (MUXMESH_INTERVAL), on the WS.
//! tmux alive -> armed:true. tmux absent -> armed:false (the hub disarms at TTL).
//!
//! Layout (lm-sanaga conventions):
//!   tmux.rs     local tmux scan + socket resolution (the only tmux spawns)
//!   ipc.rs      the CLI's Unix socket: accept, dispatch, answer
//!   ws.rs       outbound channel: dial, hold, reconnect forever (Channel)
//!   session.rs  one live session: reader/writer/pinger/watchdog + dispatch
//!   main.rs     bootstrap: config, log channel, WS loop + IPC listener

mod ipc;
mod tmux;
#[path = "ws.rs"]
mod ws;

use std::sync::Arc;

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

    // The machine token is minted OFFLINE on the hub (`muxmesh-hub
    // generate-token <host>`) and pasted here as ~/.config/muxmesh/token.
    // It signs the /agent/ws handshake (state and commands share the channel).
    // The file is re-read per reconnect, so pasting (or rotating) it takes
    // effect without restarting the agent. Absent => 401s, with a hint.
    if muxmesh_core::read_token().as_str().is_empty() {
        tracing::warn!("no token (~/.config/muxmesh/token) -> the hub will reject (401). Mint one: muxmesh-hub generate-token {}", cfg.host);
    }
    tracing::info!(
        "host={} -> {} (interval {}s)",
        cfg.host,
        cfg.ws_url(),
        cfg.interval
    );

    // No embedded HTTP badge poller here (WS-Refacto §9): the agent makes
    // ZERO network calls except its own WS. Badges on this machine come from
    // muxmesh-cli poll/bar-toggle reading tmux locally + the hub directory
    // over IPC. A std::thread doing GET /state would break that contract.

    // heartbeat: state pushed on the WS every interval (see session.rs —
    // same armed/sessions log rules: quiet at INFO unless state CHANGES).
    // The Channel is the stable half: it survives reconnects and carries the
    // CLI's relayed sends + hub directory answers. The IPC listener holds it.
    let chan = Arc::new(ws::Channel::new(cfg.host.clone()));
    let ws = ws::dial_hub_forever(Arc::new(cfg.clone()), chan.clone(), log_rx);

    // tokio runtime for the WS channel + the CLI's Unix socket (the badge
    // poller thread stays std). No agent -> no socket -> the CLI fails: the
    // agent is this machine's only gateway to the hub.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        tokio::select! {
            _ = ws => {}
            r = ipc::serve_cli_forever(chan.clone()) => {
                // bind failed (bad path, permissions): the WS must still live,
                // but say it loudly — F10 would be deaf without the socket.
                tracing::error!("ipc: listener dead: {r:?} — the CLI cannot reach the hub");
                std::future::pending::<()>().await;
            }
        }
    }); // the WS channel reconnects forever; main just keeps it alive
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxmesh_core::{Command, CommandMeta};
    use crate::tmux::build_state;

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
        assert_eq!(ws::session::execute_and_ack(&c, foreign, &[]), None);
        // bad json -> ignored
        assert_eq!(ws::session::execute_and_ack(&c, "not json", &[]), None);
        // bad version -> ack err (id echoed)
        let oldv =
            r#"{"v":2,"id":"c-2","src":"driver","dst":"me","cmd":{"type":"BarShow"},"ts":1}"#;
        let ack: muxmesh_core::Ack =
            serde_json::from_str(&ws::session::execute_and_ack(&c, oldv, &[]).unwrap()).unwrap();
        assert_eq!(ack.id, "c-2");
        assert!(!ack.ok);
        // own BarHide: no live tmux here -> exec fails, but the ack must
        // echo the id (dispatch path proven without touching tmux state).
        let mine =
            r#"{"v":1,"id":"c-3","src":"driver","dst":"me","cmd":{"type":"BarHide"},"ts":1}"#;
        let ack: muxmesh_core::Ack =
            serde_json::from_str(&ws::session::execute_and_ack(&c, mine, &[]).unwrap()).unwrap();
        assert_eq!(ack.id, "c-3");
    }

    #[test]
    fn dispatch_ignores_report_state_a_hub_might_push() {
        // ReportState is agent->hub only; if a (malicious or buggy) hub pushed
        // one back, the agent must not execute it nor ack it.
        let c = cfg();
        let rs = r#"{"v":1,"id":"s-1","src":"hub","dst":"me","ts":1,"cmd":{"type":"ReportState","state":{"host":"me","armed":false,"seen":1}}}"#;
        assert_eq!(ws::session::execute_and_ack(&c, rs, &[]), None);
    }

    #[test]
    fn ws_url_is_hub_channel() {
        assert_eq!(cfg().ws_url(), "ws://127.0.0.1:9997/ws");
    }

    #[test]
    fn give_control_meta_is_ignored_by_child() {
        // GiveControl is the parent's verb: its meta flags ignore_by_child.
        let m = Command::GiveControl.tmux_argv_meta();
        assert!(m.ignore_by_child);
        assert!(!m.ignore_by_parent);
        // ResumeControl is the child's verb: opposite guard.
        let m = Command::ResumeControl.tmux_argv_meta();
        assert!(m.ignore_by_parent);
        assert!(!m.ignore_by_child);
        // bar verbs carry no role guard.
        assert_eq!(Command::BarHide.tmux_argv_meta(), CommandMeta::DEFAULT);
    }

    #[test]
    fn child_ignores_give_control_when_a_parent_holds_an_inbound_ssh() {
        // Nominal CRIT2 guard: the receiver's LOCAL scan reports a live
        // inbound ssh ("parent") -> I am a child -> GiveControl (the parent's
        // verb) is ignored, no ack. The facts come in as an argument: the
        // pure cycle makes no network call (WS-Refacto §9).
        let c = cfg();
        let gc = r#"{"v":1,"id":"g-1","src":"parent","dst":"me","cmd":{"type":"GiveControl"},"ts":1}"#;
        assert_eq!(
            ws::session::execute_and_ack(&c, gc, &["parent".into()]),
            None,
            "child with a live parent must ignore GiveControl"
        );
    }

    #[test]
    fn no_inbound_parents_executes_give_control_as_parent() {
        // the other side of the guard: no inbound ssh visible locally -> we
        // are the parent -> GiveControl is ours to execute (ack, even if tmux
        // exec fails here — the dispatch path is what's under test).
        let c = cfg();
        let gc = r#"{"v":1,"id":"g-2","src":"driver","dst":"me","cmd":{"type":"GiveControl"},"ts":1}"#;
        let ack = ws::session::execute_and_ack(&c, gc, &[]).expect("parent executes GiveControl");
        let ack: muxmesh_core::Ack = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack.id, "g-2");
    }

    #[test]
    fn directory_answer_parses_from_hub_frame() {
        // the hub's {"dir":{...}} frame must parse into DirectoryAnswer (the
        // mailbox payload) — the hub-side contract, checked here.
        let frame = r#"{"dir":{"me":"me","directory":{"hosts":{}}}}"#;
        let v: serde_json::Value = serde_json::from_str(frame).unwrap();
        let ans: muxmesh_core::DirectoryAnswer = serde_json::from_value(v["dir"].clone()).unwrap();
        assert_eq!(ans.me, "me");
    }
}
