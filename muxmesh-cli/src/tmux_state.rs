//! tmux state reads for the F10 bar-toggle state machine.
//!
//! Pure reads: every answer comes from tmux itself (no local mirror flags).
//! The delegation state machine (`bar.rs`) asks these questions and decides.

use muxmesh_core::HubConfig;

/// The exact run-shell command F10 must carry, for re-binding it after a
/// delegation (press 2 / rollback). Same shape as muxmesh.tmux's bind: the
/// log redirect keeps tmux's status line clean and leaves a full trace.
pub fn f10_cmd() -> String {
    let cli = if std::path::Path::new("/usr/local/bin/muxmesh-cli").is_file() {
        "/usr/local/bin/muxmesh-cli".to_string()
    } else if let Ok(dir) = std::env::var("MUXMESH_BIN_DIR") {
        format!("{dir}/muxmesh-cli")
    } else {
        "muxmesh-cli".to_string()
    };
    format!("{cli} bar-toggle -v >>$HOME/.muxmesh/bar-toggle.log 2>&1")
}

/// Is THIS server's bar hidden? tmux itself knows: `status` is a server-global
/// option; after `set -g status off`, show-options answers "status off"
/// (verified tmux 3.3a). No local flag to maintain — the old
/// @muxmesh-bar-hidden window option was fragile (scope) and redundant.
pub fn bar_hidden(base: &[String]) -> bool {
    muxmesh_core::tmux::tmux_out(base, &["show-options", "-g", "status"])
        .map(|o| o.trim().ends_with("off"))
        .unwrap_or(false)
}

/// Is THIS server delegated? `prefix None` = control handed to the child
/// (press 1 cut it, samoshkin OFF mode); anything else (C-b, C-a…) = this
/// server holds the keyboard. A CHILD server may legitimately run prefix None
/// in its own config, so this is never consulted on the child branch. Exact
/// match so a custom prefix is never mistaken for delegated (ECC P2).
pub fn is_delegated(base: &[String]) -> bool {
    muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() == "None")
        .unwrap_or(false)
}

/// Armed remote host of the CURRENT window: a live `ssh <host>` in its panes
/// AND <host> armed+fresh on the hub. Badge fallback included: when the live
/// pane scan misses a deep ssh tree (zsh -> node -> ssh > depth cap), tmux's
/// own @muxmesh-armed-win=1 + @muxmesh-status carry the truth.
pub fn armed_host_of_current_window(cfg: &HubConfig) -> Option<String> {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::tmux::tmux_base(&socket);
    let wid = muxmesh_core::tmux::tmux_out(&base, &["display", "-p", "#{window_id}"])?;
    let wid = wid.trim().to_string();
    let doc = muxmesh_core::poll::fetch_state(&cfg.poll_hub);

    // LIVE scan first: pane pids -> ssh target. Precise answer, and it yields
    // the host name needed for BarShow/BarHide routing.
    if let Some(h) = muxmesh_core::tmux::window_ssh_host(&base, &wid) {
        tracing::debug!("armed-host: live ssh target in window {wid} = '{h}'");
        if !muxmesh_core::valid_host(&h) {
            tracing::debug!("window {wid}: ssh target '{h}' rejected by valid_host");
            return None;
        }
        if doc.host_armed(&h, muxmesh_core::now_unix(), cfg.ttl) {
            tracing::debug!("armed-host: '{h}' armed+fresh on hub -> ARMED");
            return Some(h);
        }
        let known: Vec<&String> = doc.hosts.keys().collect();
        tracing::debug!(
            "ssh target '{h}' not armed on hub (known hosts: {known:?}; hub={})",
            cfg.poll_hub
        );
        return None;
    }

    // Badge fallback: the embedded poller armed this window by pane scan every
    // interval and wrote @muxmesh-armed-win=1 + '@muxmesh-status  REMOTE <host> '.
    // If OUR live scan missed the ssh (deep process tree: zsh -> node -> ssh
    // exceeds the depth cap; observed prod MacBook: press 1 fell to local
    // toggle, never delegated -> 'control never returns'), trust the badge and
    // recover the host name from @muxmesh-status so BarShow still routes.
    let badge = muxmesh_core::tmux::tmux_out(
        &base,
        &["show-window-options", "-t", &wid, "-gv", "@muxmesh-armed-win"],
    )
    .map(|o| o.trim() == "1")
    .unwrap_or(false);
    if !badge {
        tracing::debug!("window {wid}: no live ssh and badge not armed -> None");
        return None;
    }
    let status = muxmesh_core::tmux::tmux_out(
        &base,
        &["show-window-options", "-t", &wid, "-gv", "@muxmesh-status"],
    )
    .unwrap_or_default();
    // '@muxmesh-status' format is exactly " REMOTE <host> " (poll.rs).
    let host = status
        .trim()
        .strip_prefix("REMOTE ")
        .map(|r| r.trim().to_string())
        .filter(|h| muxmesh_core::valid_host(h))
        .unwrap_or_default();
    if host.is_empty() {
        tracing::warn!(
            "window {wid}: badge armed but @muxmesh-status '{status}' has no host -> None"
        );
        return None;
    }
    if !doc.host_armed(&host, muxmesh_core::now_unix(), cfg.ttl) {
        tracing::debug!("badge host '{host}' not armed+fresh on hub -> None");
        return None;
    }
    tracing::info!(
        "window {wid}: live scan missed ssh, badge says armed -> trusting '{host}' (deep process tree)"
    );
    Some(host)
}

/// ECC P0 audit after a reclaim: re-read tmux, never claim success on faith.
pub fn reclaim_audit(base: &[String]) -> (bool, bool) {
    let prefix_back = muxmesh_core::tmux::tmux_out(base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() != "None")
        .unwrap_or(false);
    let f10_back = muxmesh_core::tmux::tmux_out(base, &["list-keys", "-T", "root"])
        .map(|k| k.contains("F10"))
        .unwrap_or(false);
    (prefix_back, f10_back)
}
