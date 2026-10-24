//! Local IPC transport to the agent — std::os::unix UnixStream only.
//!
//! The CLI NEVER touches the network: every hub interaction goes through the
//! agent's Unix socket (`~/.muxmesh/agent.sock`), and the agent relays on its
//! already-open WS. No agent running -> connect fails -> the CLI reports it.
//! That is the architecture: the agent is the single network gateway.
//!
//! One JSON request line per connection, one JSON response line back, then the
//! agent closes. Fresh connection per command = no correlation ids needed.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use muxmesh_core::{ipc_socket_path, IpcRequest, IpcResponse};

/// Send one request to the agent and read its single response line. The
/// connect error is surfaced verbatim — "no agent" is a real, actionable answer.
pub fn ask_agent(req: &IpcRequest) -> Result<IpcResponse, String> {
    let path = ipc_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("no agent on {}: {e}", path.display()))?;
    let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    // half-close our write side so the agent sees EOF and answers promptly.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).map_err(|e| e.to_string())?;
    if resp_line.trim().is_empty() {
        return Err("agent closed without an answer".into());
    }
    serde_json::from_str(&resp_line).map_err(|e| format!("bad agent reply: {e}"))
}

/// Convenience: relay `send <host> <verb>` to the hub via the agent. True on
/// ok. Prints the agent's msg so the driver sees the queue result.
pub fn relay_send(host: &str, verb: muxmesh_core::Command) -> bool {
    match ask_agent(&IpcRequest::Send { host: host.to_string(), verb }) {
        Ok(r) => {
            println!("{}", r.msg);
            r.ok
        }
        Err(e) => {
            eprintln!("muxmesh-cli: {e}");
            false
        }
    }
}

/// Convenience: ask the agent (-> hub over WS) who has a live ssh INTO us.
/// Empty vec on any failure (no agent, hub silent) — the caller treats empty
/// as "no parent", which is the safe reading for a local-only toggle.
pub fn ask_parents() -> Vec<String> {
    match ask_agent(&IpcRequest::AskParents) {
        Ok(r) => r
            .data
            .get("parents")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!("cli: ask_parents: {e}");
            Vec::new()
        }
    }
}

/// Convenience: ask the agent (-> hub over WS) which hosts are armed+fresh
/// right now. Empty on any failure — the caller then falls back to tmux-only
/// detection (the badge), so a missing agent degrades, never blocks F10.
pub fn ask_armed_hosts() -> Vec<String> {
    match ask_agent(&IpcRequest::AskArmed) {
        Ok(r) => r
            .data
            .get("armed")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!("cli: ask_armed_hosts: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_agent_fails_cleanly_without_daemon() {
        // a socket path that does not exist -> actionable "no agent" error,
        // never a panic. This is the no-agent-running contract.
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/muxmesh-absent-test.sock");
        let r = ask_agent(&IpcRequest::AskParents);
        std::env::remove_var("MUXMESH_IPC_SOCKET");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("no agent"));
    }

    #[test]
    fn ask_parents_empty_without_daemon() {
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/muxmesh-absent-test.sock");
        assert!(ask_parents().is_empty());
        std::env::remove_var("MUXMESH_IPC_SOCKET");
    }
}
