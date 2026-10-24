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
use crate::state::StateDoc;

/// CLI -> agent: what the driver asks the agent to do on its behalf. The
/// agent owns every network decision (routing, directory reads); the CLI only
/// states an intent and carries tmux-local facts it can read itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    /// relay `send <host> <verb>` to the hub (the agent enqueues on dst's WS).
    Send { host: String, verb: Command },
    /// the hub directory (full StateDoc): `tree` renders it, `armed-host`
    /// and `why` read the armed set from it. One frame, every question.
    AskDirectory,
}

/// agent -> CLI: the result of one IpcRequest. `ok=false` carries a short
/// reason (never secrets). `data` is op-specific JSON (directory, ok flag…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    /// short human-readable status or error (logged + printed by the CLI).
    pub msg: String,
    /// op-specific payload (e.g. {"directory":{...}}), empty when unused.
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

/// agent -> hub on the WS: "give me your directory". The hub holds the map
/// (every host's reported state), so it — not the agent — answers who is
/// armed and who has a live ssh INTO me. This frame replaces every GET
/// /state: the answer comes back on the channel the agent already owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskDirectory {}

/// hub -> agent on the WS: the answer to AskDirectory, pushed on the same
/// channel. `directory` is the full StateDoc; the agent keeps it only long
/// enough to answer the next CLI request (short cache, never a source of
/// truth — the hub is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryAnswer {
    /// the host this answer is addressed to (echo, guards against misroute).
    pub me: String,
    /// the hub's directory at answer time.
    pub directory: StateDoc,
}

/// The Unix socket path the agent listens on and the CLI dials. Overridable
/// via MUXMESH_IPC_SOCKET (tests, non-local installs).
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
            IpcRequest::AskDirectory,
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
    fn directory_answer_roundtrips() {
        let a = DirectoryAnswer { me: "me".into(), directory: StateDoc::default() };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<DirectoryAnswer>(&j).unwrap(), a);
    }

    #[test]
    fn ipc_socket_path_honors_env_override() {
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/x/agent.sock");
        assert_eq!(ipc_socket_path(), std::path::PathBuf::from("/tmp/x/agent.sock"));
        std::env::remove_var("MUXMESH_IPC_SOCKET");
    }
}
