//! tmux exec helpers — the only place that spawns `tmux`.

use std::process::Command;

/// argv prefix for the tmux CLI: `-S <socket>` when one is configured.
pub fn tmux_base(socket: &Option<String>) -> Vec<String> {
    match socket {
        Some(s) => vec!["tmux".into(), "-S".into(), s.clone()],
        None => vec!["tmux".into()],
    }
}

/// Run a tmux command, logging stderr on failure (a silent failure here is
/// how the bar-hidden flag stopped persisting — never swallow it again).
pub fn run_tmux(base: &[String], args: &[&str]) {
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(args);
    if let Ok(o) = cmd.output() {
        if !o.status.success() {
            tracing::warn!(
                "tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
    }
}

/// Run a tmux command, reporting success. For state-changing commands whose
/// failure must not be swallowed (phase 2 prefix/key-table switches: a failed
/// `set -gu prefix` means the parent is stuck without a keyboard).
pub fn tmux_run(base: &[String], args: &[&str]) -> bool {
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(args);
    match cmd.output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            tracing::warn!(
                "tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
        Err(e) => {
            tracing::warn!("tmux {:?} spawn failed: {e}", args);
            false
        }
    }
}

/// Run a tmux command, returning stdout on success (stderr captured for debug).
pub fn tmux_out(base: &[String], args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(args);
    match cmd.output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            tracing::debug!(
                "tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            None
        }
        Err(e) => {
            tracing::debug!("spawn tmux: {e}");
            None
        }
    }
}

/// Pane pids of a window, or None if the query itself failed.
/// Distinguishes "no panes" (Some(empty)) from "tmux broken" (None) so a
/// transient failure never silently disarms windows.
pub fn pane_pids(base: &[String], wid: &str) -> Option<Vec<u32>> {
    let out = tmux_out(base, &["list-panes", "-t", wid, "-F", "#{pane_pid}"])?;
    Some(out.lines().filter_map(|l| l.trim().parse().ok()).collect())
}
