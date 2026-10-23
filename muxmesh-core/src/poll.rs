//! Badge poller: per-window arm/disarm of @muxmesh-armed-win + @muxmesh-status.
//! Lives in muxmesh-core so the hub and the agent can embed it as a thread:
//! starting muxmesh-hub or muxmesh-agent badges tmux windows automatically.
//!
//! Per window: find a live `ssh <host>` child of its panes (tmux::ssh_scan),
//! then check the state doc for an armed+fresh entry for that host.
//! Armed -> tmux set-window-option @muxmesh-armed-win 1, else unset.
//!
//! Process spawning lives in tmux/ and http/ — this module only orchestrates.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::tmux;
use crate::{valid_host, HubConfig, StateDoc};

/// Read the aggregated state from a local file (the hub-side copy). Missing
/// or corrupt -> empty doc, never a panic.
pub fn read_state(path: &Path) -> StateDoc {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| StateDoc::from_json_str(&s).ok())
        .unwrap_or_default()
}

/// One pass: evaluate every window on the server, set/unset the badge option.
pub fn refresh_window_badges(base: &[String], doc: &StateDoc, ttl: u64) {
    let now = crate::now_unix();
    // all windows across all sessions: "window_id" per line
    let out = match tmux::tmux_out(base, &["list-windows", "-a", "-F", "#{window_id}"]) {
        Some(o) => o,
        None => return, // no tmux server -> nothing to badge (never spawn a phantom one)
    };
    for wid in out.lines() {
        if wid.is_empty() {
            continue;
        }
        let mut armed = false;
        let mut armed_host = String::new();
        if let Some(pids) = tmux::pane_pids(base, wid) {
            for pid in pids {
                if let Some(h) = tmux::pane_ssh_host(pid) {
                    // Defense in depth: the hub whitelists these, but a hand-edited or
                    // stale state file must never feed a crafted string to tmux `-t`.
                    if !valid_host(&h) {
                        continue;
                    }
                    if doc.host_armed(&h, now, ttl) {
                        armed = true;
                        armed_host = h;
                        break;
                    }
                }
            }
        }
        if armed {
            tmux::run_tmux(
                base,
                &["set-window-option", "-t", wid, "@muxmesh-armed-win", "1"],
            );
            // Badge text for the theme to render (tmux-remote model): the poller
            // never touches status-left, it fills @muxmesh-status and the operator
            // places it. Validated host (valid_host above) -> safe to interpolate.
            tmux::run_tmux(
                base,
                &[
                    "set-window-option",
                    "-t",
                    wid,
                    "@muxmesh-status",
                    &format!(" REMOTE {armed_host} "),
                ],
            );
        } else {
            tmux::run_tmux(
                base,
                &["set-window-option", "-t", wid, "-u", "@muxmesh-armed-win"],
            );
            tmux::run_tmux(
                base,
                &["set-window-option", "-t", wid, "-u", "@muxmesh-status"],
            );
        }
    }
}

/// Main loop (CLI `poll`): badge every cfg.interval until the process dies.
pub fn poll_badges_forever(cfg: &HubConfig) {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = tmux::tmux_base(&socket);
    tracing::info!(interval = cfg.interval, ttl = cfg.ttl, hub = %cfg.poll_hub, "badge poller started");
    if cfg.poll_hub.starts_with("127.0.0.1") || cfg.poll_hub.starts_with("localhost") {
        tracing::warn!("hub is loopback — on a driver machine set poll.hub = \"<hub-ip>:9997\" (or MUXMESH_POLL_HUB) to badge remote hosts");
    }
    loop {
        let doc = fetch_state(&cfg.poll_hub);
        refresh_window_badges(&base, &doc, cfg.ttl);
        thread::sleep(Duration::from_secs(cfg.interval));
    }
}

/// Embedded badge poller for hub/agent: same loop, quiet. On a machine with
/// no local tmux server (the hub), refresh_window_badges is inert and fetch_state only
/// logs a deduped line every ~30s; that is the expected steady state there.
pub fn poll_hub_and_badge_windows(cfg: &HubConfig) {
    tracing::info!(hub = %cfg.poll_hub, "embedded badge poller started");
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = tmux::tmux_base(&socket);
    loop {
        let doc = fetch_state(&cfg.poll_hub);
        refresh_window_badges(&base, &doc, cfg.ttl);
        thread::sleep(Duration::from_secs(cfg.interval));
    }
}

/// One badge pass for tests and `poll --once` (no loop).
pub fn refresh_badges_once(cfg: &HubConfig) {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = tmux::tmux_base(&socket);
    let doc = fetch_state(&cfg.poll_hub);
    refresh_window_badges(&base, &doc, cfg.ttl);
}

/// Live state from the hub. A driver machine has no local state.json — the
/// hub is the source of truth. On unreachable/corrupt, returns the empty doc
/// (windows disarm after ttl anyway; a flaky hub must not panic the poller).
pub fn fetch_state(hub_addr: &str) -> StateDoc {
    match crate::http::get_state(hub_addr) {
        Ok(d) => d,
        Err(e) => {
            // dedupe: log the first failure and every 15th (~30s at 2s poll)
            // note: process-global counter — assumes one poller per process (true today).
            // so a down hub doesn't spam stderr when supervised (later: launchd/systemd).
            static FAILS: AtomicU64 = AtomicU64::new(0);
            let n = FAILS.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(15) {
                tracing::warn!("hub {hub_addr}: {e} (empty state this cycle, #{})", n + 1);
            }
            StateDoc::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_state_missing_file_is_empty() {
        let doc = read_state(Path::new("/nonexistent/state.json"));
        assert!(doc.hosts.is_empty());
    }

    #[test]
    fn read_state_corrupt_is_empty_not_panic() {
        let dir = std::env::temp_dir().join(format!("muxmesh-cli-{}", crate::now_unix()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        fs::write(&p, b"{ not json").unwrap();
        assert!(read_state(&p).hosts.is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
