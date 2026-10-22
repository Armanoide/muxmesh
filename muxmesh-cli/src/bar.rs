//! The F10 bar-toggle state machine — the whole delegation logic.
//!
//! Roles are decided by the KEYSTROKE's server, not configured anywhere:
//! an armed ssh in the current window -> PARENT branch, else CHILD branch.
//!
//!   PARENT  press 1 -> DELEGATE: BarShow(child) via hub, then prefix None
//!                     + switch-client -T off (samoshkin OFF mode). F10 stays
//!                     bound in table `off` = the parent's escape hatch.
//!   PARENT  press 2 -> RECLAIM locally (no network needed for the keyboard).
//!   CHILD   F10     -> local bar toggle; if hiding, answer ParentReclaim to
//!                     whoever armed an ssh into us (hub directory).

use muxmesh_core::{Command, HubConfig};

use crate::tmux_state;
use crate::transport;

/// `muxmesh-cli bar-toggle` entry point. Returns a process exit code.
pub fn run(cfg: &HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::tmux::tmux_base(&socket);

    let wid = match muxmesh_core::tmux::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(o) => o.trim().to_string(),
        None => {
            tracing::error!("bar-toggle: no tmux server / no current window");
            return 1;
        }
    };
    let hidden_now = tmux_state::bar_hidden(&base);
    let delegated = tmux_state::is_delegated(&base);
    let host = tmux_state::armed_host_of_current_window(cfg);
    tracing::info!(
        "bar-toggle: window {wid}, bar-hidden={hidden_now}, armed host = {host:?}, delegated={delegated}"
    );

    match (host, delegated) {
        // child, or server still delegated with no armed ssh: reclaim.
        (Some(h), true) => reclaim(&base, Some(&h), cfg),
        // parent + press 1: hand the keyboard to the child.
        (Some(h), false) => delegate(&base, &h, cfg),
        // child, or parent whose armed window the scans both missed.
        (None, d) => child_press(&base, hidden_now, d, cfg),
    }
}

/// press 1 — hand the keyboard to the child (samoshkin OFF mode).
///
/// GUARD: the prefix is cut BEFORE anything else is touched — if the cut is
/// refused, nothing happened and the parent keeps everything. Bar off + deaf
/// table only after the cut is confirmed by re-reading tmux.
fn delegate(base: &[String], h: &str, cfg: &HubConfig) -> u8 {
    if !transport::post_verb(Command::BarShow, h, "BarShow", cfg) {
        tracing::error!("bar-toggle: child unreachable, NOT delegating");
        println!("muxmesh: remote {h} unreachable — control NOT delegated (parent keeps the keyboard)");
        return 1;
    }
    let cut = muxmesh_core::tmux::tmux_run(base, &["set-option", "-g", "prefix", "None"]);
    if !cut || !tmux_state::is_delegated(base) {
        muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
        tracing::error!("bar-toggle: prefix cut FAILED — rolled back, parent keeps control");
        println!("muxmesh: delegation FAILED (prefix cut refused) — parent keeps the keyboard");
        return 1;
    }
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "off"]);
    // headless servers answer "no current client" — harmless, prefix None is
    // what actually delegates the keyboard there.
    muxmesh_core::tmux::tmux_run(base, &["switch-client", "-T", "off"]);
    println!("muxmesh: control DELEGATED to {h} — press F10 again to reclaim");
    0
}

/// The single reclaim path (was two near-copies). Restores prefix + table
/// root + bar, audits tmux, then fixes the other side via the hub:
/// `dst = Some(host)` -> BarHide the child (parent pressed F10 itself);
/// `dst = None` -> answer ParentReclaim to our hub parent (self-reclaim).
fn reclaim(base: &[String], dst: Option<&str>, cfg: &HubConfig) -> u8 {
    muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
    muxmesh_core::tmux::run_tmux(base, &["switch-client", "-T", "root"]);
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
    match dst {
        Some(h) => {
            transport::post_verb(Command::BarHide, h, "BarHide", cfg);
        }
        None => {
            if !cfg.poll_hub.is_empty() {
                transport::answer_parent_reclaim(cfg);
            }
        }
    }
    let (prefix_back, f10_back) = tmux_state::reclaim_audit(base);
    tracing::info!("bar-toggle: reclaim — prefix_back={prefix_back} f10_back={f10_back}");
    println!("muxmesh: control RECLAIMED");
    0
}

/// CHILD branch (no armed ssh here): plain local bar toggle, or self-reclaim
/// if this server is still delegated (prefix None from an earlier press 1).
/// If hiding, tell our parent (whoever has a live ssh into us) to reclaim.
fn child_press(base: &[String], hidden_now: bool, delegated: bool, cfg: &HubConfig) -> u8 {
    if delegated {
        return reclaim(base, None, cfg);
    }
    let next = if hidden_now { "on" } else { "off" };
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", next]);
    if next == "off" && !cfg.poll_hub.is_empty() {
        transport::answer_parent_reclaim(cfg);
    }
    println!("muxmesh: local bar {next}");
    0
}
