//! The local IPC contract between `muxmesh-cli` (short-lived driver) and the
//! `muxmesh-agent` (the only pair with a persistent WS to the hub).
//!
//! Architecture rule: the CLI NEVER touches the network. It speaks to the
//! agent over a Unix socket (`~/.muxmesh/agent.sock`); the agent relays on
//! its already-open WS. No agent running -> no socket -> the CLI fails. That
//! is intended: the agent is the single network gateway for its machine.
//!
//! One JSON object per line, in both directions (JSON Lines). The CLI opens a
//! fresh connection per command, so the agent answers on the same connection
//! then closes it — no correlation ids needed (unlike the hub WS, where many
//! frames share one long-lived channel).

use serde::{Deserialize, Serialize};

use crate::command::Command;

/// CLI -> agent: what the driver asks the agent to do on its behalf. The
/// agent owns every network decision (routing, parent resolution); the CLI
/// only states an intent and carries tmux-local facts it can read itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    /// relay `send <host> <verb>` to the hub (the agent enqueues on dst's WS).
    Send { host: String, verb: Command },
    /// who has a live ssh INTO me? the agent asks the hub over the WS.
    /// who has a live ssh INTO me? the agent asks the hub over the WS.
    AskParents,
    /// which hosts are armed+fresh right now? the agent asks the hub over the
    /// WS (the CLI needs this to badge / pick a send target, never GETs it).
    AskArmed,
    /// the F10 bar-toggle intent: the agent runs the whole delegate/reclaim
    /// dance (it holds the WS and the hub directory), then reports the outcome.
    BarToggle,
}

/// agent -> CLI: the result of one IpcRequest. `ok=false` carries a short
/// reason (never secrets). `data` is op-specific JSON (parents list, tree…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    /// short human-readable status or error (logged + printed by the CLI).
    pub msg: String,
    /// op-specific payload (e.g. {"parents":["mac-mini"]}), empty when unused.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl IpcResponse {
    pub fn ok(msg: impl Into<String>) -> Self {
        IpcResponse { ok: true, msg: msg.into(), data: Default::default() }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        IpcResponse { ok: false, msg: msg.into(), data: Default::default() }
    }
}

/// hub -> agent on the WS: answers AskParents. The hub holds the directory
/// (every host's reported outbound ssh targets), so it — not the agent — can
/// say who has a live ssh INTO `me`. This is why the agent never GETs /state:
/// the answer comes back on the channel it already owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentsAnswer {
    /// the host this answer is addressed to (echo, guards against misroute).
    pub me: String,
    /// host keys with a live inbound ssh into `me` (empty = no parent).
    pub parents: Vec<String>,
}

/// The Unix socket path the agent listens on and the CLI dials. Overridable
/// via MUXMESH_IPC_SOCKET (tests, non-default installs).
pub fn ipc_socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("MUXMESH_IPC_SOCKET") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".muxmesh").join("agent.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_per_op() {
        for req in [
            IpcRequest::AskParents,
            IpcRequest::BarToggle,
            IpcRequest::Send { host: "mac-mini".into(), verb: Command::BarHide },
        ] {
            let line = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(req, back);
        }
    }

    #[test]
    fn response_skips_empty_data() {
        let j = serde_json::to_string(&IpcResponse::ok("queued")).unwrap();
        assert!(!j.contains("data"));
        assert!(serde_json::from_str::<IpcResponse>(&j).unwrap().ok);
    }

    #[test]
    fn parents_answer_roundtrips() {
        let a = ParentsAnswer { me: "me".into(), parents: vec!["parent".into()] };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<ParentsAnswer>(&j).unwrap(), a);
    }

    #[test]
    fn ipc_socket_path_honors_env_override() {
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/x/agent.sock");
        assert_eq!(ipc_socket_path(), std::path::PathBuf::from("/tmp/x/agent.sock"));
        std::env::remove_var("MUXMESH_IPC_SOCKET");
    }
}
