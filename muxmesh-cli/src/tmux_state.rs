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

/// Armed remote host of the CURRENT window: badge first (@muxmesh-armed-win=1
/// + host from @muxmesh-status, the poller's own answer — one tmux call),
/// cross-checked armed+fresh on the hub directory. Live pgrep scan is the
/// fallback for windows the poller has not stamped yet.
pub fn armed_host_of_current_window(cfg: &HubConfig) -> Option<String> {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::tmux::tmux_base(&socket);
    let wid = muxmesh_core::tmux::tmux_out(&base, &["display", "-p", "#{window_id}"])?;
    let wid = wid.trim().to_string();
    // armed+fresh host set, from the hub directory THROUGH THE AGENT (IPC).
    // The CLI never opens a network socket; no agent -> empty set -> the live
    // tmux scan still answers, only the hub-freshness cross-check degrades.
    let doc = crate::transport::ask_directory().unwrap_or_default();

    // badge first: it is the agent's own answer (poller already resolved
    // "this window holds ssh <host>") and costs one tmux call. The live pgrep
    // scan is the fallback for windows the poller has not stamped yet.
    let badge = muxmesh_core::tmux::tmux_out(
        &base,
        &["show-window-options", "-t", &wid, "-gv", "@muxmesh-armed-win"],
    )
    .map(|o| o.trim() == "1")
    .unwrap_or(false);
    if badge {
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
        } else if doc.host_armed(&host, muxmesh_core::now_unix(), cfg.ttl) {
            return Some(host);
        } else {
            tracing::debug!("badge host '{host}' not armed+fresh on hub");
        }
    }

    if let Some(h) = muxmesh_core::tmux::window_ssh_host(&base, &wid) {
        if !muxmesh_core::valid_host(&h) {
            tracing::debug!("window {wid}: ssh target '{h}' rejected by valid_host");
            return None;
        }
        if doc.host_armed(&h, muxmesh_core::now_unix(), cfg.ttl) {
            return Some(h);
        }
        tracing::debug!(
            "ssh target '{h}' not armed on hub (known: {:?})",
            doc.hosts.keys().collect::<Vec<_>>()
        );
        return None;
    }

    tracing::debug!("window {wid}: no armed ssh (badge off, live scan empty)");
    None
}

/// Post-reclaim audit: re-read tmux, never claim success on faith. The
/// delegated state is prefix None + GLOBAL key-table off (tmux-remote), so
/// the audit checks both — a prefix back in table `off` would still be deaf.
pub fn reclaim_audit(base: &[String]) -> (bool, bool) {
    let prefix_back = muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() != "None")
        .unwrap_or(false);
    let f10_back = muxmesh_core::tmux::tmux_out(base, &["list-keys", "-T", "root"])
        .map(|k| k.contains("F10"))
        .unwrap_or(false);
    let kt = muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "key-table"])
        .map(|o| o.trim() != "off")
        .unwrap_or(true);
    (prefix_back && kt, f10_back)
}
