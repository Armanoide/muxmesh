//! Local tmux surface for the agent: scan state, resolve the live socket,
//! and the small read helpers used in exec traces. The only place the agent
//! spawns `tmux` directly.

use std::process::Command;

use muxmesh_core::{HostState, RemoteSession};

/// Live local tmux? sessions + active window per session.
/// Returns (armed, sessions). `socket` is the agent's configured tmux socket
/// ("" = tmux default); when set it is passed as -S so the agent scans its
/// own server, not /tmp/tmux-<uid>/default. Without this the agent on a custom
/// socket never sees its tmux and posts armed=false forever.
pub fn scan_tmux(socket: &str) -> (bool, Vec<RemoteSession>) {
    // tmux ls -> "sess: 3 windows (created ...)" per line, error if no server
    let mut cmd = Command::new("tmux");
    if !socket.is_empty() {
        cmd.arg("-S").arg(socket);
    }
    // tmux does NOT expand "\t" in a format string — it emits the literal
    // two characters backslash-t. Use an explicit tab via tmux's own escape
    // "#{?...}" is overkill; a literal marker we control is simplest and safe
    // (session names are validated elsewhere, but the marker must never appear
    // in a name — ":WS:" is not a legal tmux session-name sequence).
    let out = match cmd
        .args(["list-sessions", "-F", "#{session_name}:WS:#{session_windows}"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return (false, Vec::new()),
    };
    let text = String::from_utf8_lossy(&out);
    let mut sessions = Vec::new();
    for line in text.lines() {
        let mut it = line.split(":WS:");
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
pub fn active_window(socket: &str, session: &str) -> String {
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

/// The payload is built with muxmesh-core (state.rs) -> same contract as the hub.
pub fn build_state(host: &str, socket: &str) -> HostState {
    let (armed, sessions) = scan_tmux(socket);
    HostState {
        host: host.to_string(),
        armed,
        seen: muxmesh_core::now_unix(),
        sessions,
        parents: muxmesh_core::tmux::ssh_parent_hosts(),
    }
}

/// Resolve the tmux socket the agent controls: the configured one, or ""
/// meaning "default". A stale/default socket that does not exist on disk is
/// resolved to the live one (`tmux display -p '#{socket_path}'`) — macOS
/// sockets live in a per-user /tmp/claude-<uid>/ style dir that changes
/// between sessions, so a literal "default" often 404s while tmux itself
/// knows where its socket is.
pub fn resolve_socket(cfg: &muxmesh_core::AgentConfig) -> String {
    if !cfg.tmux_socket.is_empty() && std::path::Path::new(&cfg.tmux_socket).exists() {
        return cfg.tmux_socket.clone();
    }
    Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.tmux_socket.clone())
}

/// prefix+status of a socket in one line, for the before/after reclaim trace.
pub fn tmux_prefix_and_status(socket: &str) -> String {
    let get = |args: &[&str]| -> String {
        Command::new("tmux")
            .arg("-S")
            .arg(socket)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "?".into())
    };
    format!(
        "prefix={} status={}",
        get(&["show-options", "-gv", "prefix"]),
        get(&["show-options", "-gv", "status"])
    )
}

/// Human-readable list of sessions on a socket, for the exec trace.
pub fn tmux_sessions(socket: &str) -> String {
    Command::new("tmux")
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
