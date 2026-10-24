//! tmux state reads for the F10 bar-toggle state machine.
//!
//! Pure reads: every answer comes from tmux itself (no local mirror flags).

use muxmesh_core::HubConfig;

/// Is THIS server's bar hidden? `status` is a server-global option — tmux
/// itself is the source of truth (no flag to maintain).
pub fn bar_hidden(base: &[String]) -> bool {
    muxmesh_core::tmux::tmux_out(base, &["show-options", "-g", "status"])
        .map(|o| o.trim().ends_with("off"))
        .unwrap_or(false)
}

/// Is THIS server delegated? `prefix None` = control handed to the child
/// (samoshkin OFF mode). Exact match so a custom prefix is never mistaken
/// for delegated.
pub fn is_delegated(base: &[String]) -> bool {
    muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() == "None")
        .unwrap_or(false)
}

/// Armed remote host of the CURRENT window: a live `ssh <host>` in its panes
/// AND <host> armed+fresh on the hub. Badge fallback: the live pane scan has
/// a depth cap and misses deep trees (zsh -> node -> ssh); the embedded
/// poller already answered that question in @muxmesh-armed-win, so trust it
/// and recover the host name from @muxmesh-status (" REMOTE <host> ").
pub fn armed_host_of_current_window(_cfg: &HubConfig) -> Option<String> {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::tmux::tmux_base(&socket);
    let wid = muxmesh_core::tmux::tmux_out(&base, &["display", "-p", "#{window_id}"])?;
    let wid = wid.trim().to_string();
    // armed+fresh host set, fetched from the hub THROUGH THE AGENT (IPC). The
    // CLI never opens a network socket; no agent -> empty set -> the live scan
    // still answers via tmux, only the hub-freshness cross-check degrades.
    let armed_hosts = crate::transport::ask_armed_hosts();

    if let Some(h) = muxmesh_core::tmux::window_ssh_host(&base, &wid) {
        if !muxmesh_core::valid_host(&h) {
            tracing::debug!("window {wid}: ssh target '{h}' rejected by valid_host");
            return None;
        }
        if armed_hosts.iter().any(|a| a == &h) {
            return Some(h);
        }
        tracing::debug!(
            "ssh target '{h}' not armed on hub (known: {armed_hosts:?})"
        );
        return None;
    }

    let badge = muxmesh_core::tmux::tmux_out(
        &base,
        &["show-window-options", "-t", &wid, "-gv", "@muxmesh-armed-win"],
    )
    .map(|o| o.trim() == "1")
    .unwrap_or(false);
    if !badge {
        return None;
    }
    let status = muxmesh_core::tmux::tmux_out(
        &base,
        &["show-window-options", "-t", &wid, "-gv", "@muxmesh-status"],
    )
    .unwrap_or_default();
    let host = status
        .trim()
        .strip_prefix("REMOTE ")
        .map(|r| r.trim().to_string())
        .filter(|h| muxmesh_core::valid_host(h))
        .unwrap_or_default();
    if host.is_empty() {
        tracing::warn!("window {wid}: badge armed but @muxmesh-status '{status}' has no host");
        return None;
    }
    if !armed_hosts.iter().any(|a| a == &host) {
        return None;
    }
    tracing::info!("window {wid}: live scan missed ssh, trusting badge -> '{host}'");
    Some(host)
}

/// Post-reclaim audit: re-read tmux, never claim success on faith.
pub fn reclaim_audit(base: &[String]) -> (bool, bool) {
    let prefix_back = muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() != "None")
        .unwrap_or(false);
    let f10_back = muxmesh_core::tmux::tmux_out(base, &["list-keys", "-T", "root"])
        .map(|k| k.contains("F10"))
        .unwrap_or(false);
    (prefix_back, f10_back)
}
