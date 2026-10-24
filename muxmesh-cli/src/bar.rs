//! The F10 bar-toggle state machine — the whole delegation logic.
//!
//! Roles are decided by the KEYSTROKE's server, not configured anywhere:
//! an armed ssh in the current window -> PARENT branch, else CHILD branch.
//! The CLI owns the LOCAL tmux dance (prefix/table/bar — no network); every
//! hub interaction goes through the agent's WS via IPC (transport.rs). No
//! agent -> no relay -> the remote side is simply not notified; the local bar
//! still toggles (the keystroke must never be swallowed).
//!
//!   PARENT  press 1 -> DELEGATE: GiveControl(child) via agent->hub, then cut
//!                     the local prefix (None) + park the client in table `off`.
//!   PARENT  press 2 -> RECLAIM locally + BarHide(child) via agent->hub.
//!   CHILD   F10     -> local bar toggle; if hiding, ResumeControl to whoever
//!                     armed an ssh into us (the hub directory, via agent->hub).

use muxmesh_core::{Command, HubConfig};

use crate::tmux_state;
use crate::transport;

/// `muxmesh-cli bar-toggle` entry point: read the tmux state, then branch on
/// (armed host, delegated) to delegate/reclaim/child-press. Exit code = status.
pub fn toggle_bar_for_current_window(cfg: &HubConfig) -> u8 {
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
        (Some(h), true) => reclaim(&base, Some(&h)),
        // parent + press 1: hand the keyboard to the child.
        (Some(h), false) => delegate(&base, &h),
        // child, or parent whose armed window the scans both missed.
        (None, d) => child_press(&base, hidden_now, d),
    }
}

/// press 1 — hand the keyboard to the child (samoshkin OFF mode).
///
/// GUARD: the prefix is cut BEFORE anything else is touched — if the cut is
/// refused, nothing happened and the parent keeps everything. Bar off + deaf
/// table only after the cut is confirmed by re-reading tmux. The child is
/// notified via GiveControl relayed on the agent's WS (no direct network).
fn delegate(base: &[String], h: &str) -> u8 {
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
    // notify the child (give it control) via the agent's WS; best-effort —
    // the parent already delegated its own keyboard, a silent child is not a
    // reason to undo the local cut.
    if transport::relay_send(h, Command::GiveControl) {
        println!("muxmesh: control DELEGATED to {h} — press F10 again to reclaim");
    } else {
        println!("muxmesh: control DELEGATED locally; child '{h}' NOT notified (agent down?)");
    }
    0
}

/// The single reclaim path. Restores prefix + table root + bar locally, audits
/// tmux, then fixes the other side via the agent's WS: `dst = Some(host)` ->
/// BarHide the child (parent pressed F10 itself); `dst = None` -> ResumeControl
/// to our parent (self-reclaim, child pressing again while delegated).
fn reclaim(base: &[String], dst: Option<&str>) -> u8 {
    muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
    muxmesh_core::tmux::run_tmux(base, &["switch-client", "-T", "root"]);
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
    match dst {
        Some(h) => {
            if !transport::relay_send(h, Command::BarHide) {
                println!("muxmesh: RECLAIMED locally; child '{h}' NOT told to hide its bar (agent down?)");
            }
        }
        None => {
            answer_parent_reclaim();
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
fn child_press(base: &[String], hidden_now: bool, delegated: bool) -> u8 {
    if delegated {
        return reclaim(base, None);
    }
    let next = if hidden_now { "on" } else { "off" };
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", next]);
    if next == "off" {
        answer_parent_reclaim();
    }
    println!("muxmesh: local bar {next}");
    0
}

/// Answer a press-2 back to the parent: relay ResumeControl to whoever has a
/// live ssh INTO us. Source: hub directory `parents_of` — the parent's own
/// ReportState names the ssh it dialed, so the child does not need to see
/// the parent's badge (it never can: that window lives on the parent's
/// server). Best-effort: never block the local bar toggle on the network.
fn answer_parent_reclaim() {
    let doc = match transport::ask_directory() {
        Some(d) => d,
        None => {
            tracing::info!("child: no directory (agent/hub silent) — local-only toggle");
            println!("muxmesh: parent NOT notified (agent/hub silent) — local-only toggle");
            return;
        }
    };
    let parent = match muxmesh_core::tmux::parents_of(&doc, &transport::driver_host())
        .into_iter()
        .next()
    {
        Some(p) => p,
        None => {
            tracing::info!("child: no parent in the hub directory — local-only toggle");
            return;
        }
    };
    transport::relay_send(&parent, Command::ResumeControl);
}
