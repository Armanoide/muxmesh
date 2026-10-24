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

/// The hub directory (full StateDoc) through the agent's WS. None on any
/// failure (no agent, hub silent) — callers degrade to tmux-only answers or
/// report the gap, they never dial the hub themselves.
pub fn ask_directory() -> Option<muxmesh_core::StateDoc> {
    match ask_agent(&IpcRequest::AskDirectory) {
        Ok(r) if r.ok => r
            .data
            .get("directory")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        Ok(r) => {
            tracing::warn!("cli: agent refused directory: {}", r.msg);
            None
        }
        Err(e) => {
            tracing::warn!("cli: ask_directory: {e}");
            None
        }
    }
}

/// Is the hub directory reachable right now (agent + WS alive)? bar-toggle
/// uses this to refuse a blind role decision: no directory -> we cannot know
/// if we are parent or child -> never cut the prefix (safe-default CRIT2).
pub fn directory_reachable() -> bool {
    ask_agent(&IpcRequest::AskDirectory).map(|r| r.ok).unwrap_or(false)
}

/// Our hub identity: MUST equal the agent's canonical host name (the key in
/// tokens.json) — the same resolution the agent dials with. NOT `hostname -s`
/// (macOS mDNS advertises a different name the hub never registered).
/// Order: MUXMESH_HOST env > config.toml agent.host > hostname -s fallback.
pub fn driver_host() -> String {
    if let Ok(h) = std::env::var("MUXMESH_HOST") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    let map = muxmesh_core::load_map();
    if let Some(h) = map.get("agent.host").cloned().filter(|h| !h.is_empty()) {
        return h;
    }
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_agent_fails_cleanly_without_daemon() {
        // a socket path that does not exist -> actionable "no agent" error,
        // never a panic. This is the no-agent-running contract.
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/muxmesh-absent-test.sock");
        let r = ask_agent(&IpcRequest::AskDirectory);
        std::env::remove_var("MUXMESH_IPC_SOCKET");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("no agent"));
    }

    #[test]
    fn ask_directory_none_without_daemon() {
        std::env::set_var("MUXMESH_IPC_SOCKET", "/tmp/muxmesh-absent-test.sock");
        assert!(ask_directory().is_none());
        std::env::remove_var("MUXMESH_IPC_SOCKET");
    }
}
