//! The F10 bar-toggle state machine — the whole delegation logic.
//!
//! Roles are decided by the KEYSTROKE's server, not configured anywhere:
//! an armed ssh in the current window -> PARENT branch, else CHILD branch.
//! The CLI owns the LOCAL tmux dance (prefix/table/bar — no network); every
//! hub interaction goes through the agent's WS via IPC (transport.rs). No
//! agent -> no relay -> the remote side is simply not notified; the local bar
//! still toggles (the keystroke must never be swallowed).
//!
//!   PARENT  press 1 -> DELEGATE: cut the local prefix + global key-table
//!                     `off` (tmux-remote recipe), then GiveControl(child) via
//!                     agent->hub (the child shows its bar, badge = its host).
//!   PARENT  press 2 -> RECLAIM locally (prefix + key-table unset) and the
//!                     poller repaints our bar with OUR hostname. The child's
//!                     bar goes dark on its own press-2 (ResumeControl).
//!   CHILD   F10     -> hide our bar + ResumeControl to whoever armed an ssh
//!                     into us (the hub directory, via agent->hub). The bar is
//!                     never toggled by hand: the poller owns status-left and
//!                     paints the hostname of THIS machine when it returns.

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
        // parent WITHOUT a fresh hub view (agent/hub down): never cut the
        // prefix blind — safe-default keeps the keyboard (CRIT2).
        (None, true) => reclaim(&base, None),
        (None, false) => {
            if transport::directory_reachable() {
                child_press(&base, hidden_now)
            } else {
                println!("muxmesh: hub directory unreachable — no role known, nothing changed (safe-default)");
                1
            }
        }
    }
}

/// press 1 — hand the keyboard to the child (tmux-remote OFF recipe).
///
/// GUARD: the prefix is cut BEFORE anything else is touched — if the cut is
/// refused, nothing happened and the parent keeps everything. The deaf state
/// is the GLOBAL key-table (`set -g key-table off`), never switch-client:
/// per-client tables let new/headless clients hear the prefix (the rule tmux
/// -remote pins). The bar stays visible; its badge says where we are. The
/// child is notified via GiveControl relayed on the agent's WS (no network).
fn delegate(base: &[String], h: &str) -> u8 {
    let cut = muxmesh_core::tmux::tmux_run(base, &["set-option", "-g", "prefix", "None"]);
    if !cut || !tmux_state::is_delegated(base) {
        muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
        tracing::error!("bar-toggle: prefix cut FAILED — rolled back, parent keeps control");
        println!("muxmesh: delegation FAILED (prefix cut refused) — parent keeps the keyboard");
        return 1;
    }
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "key-table", "off"]);
    muxmesh_core::tmux::run_tmux(base, &["refresh-client", "-S"]);
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

/// The single reclaim path (tmux-remote recipe): unset prefix + key-table
/// globally + repaint, audit tmux, then fix the other side via the agent's
/// WS: `dst = Some(host)` -> BarHide the child (parent pressed F10 itself);
/// `dst = None` -> ResumeControl to our parent (self-reclaim).
fn reclaim(base: &[String], dst: Option<&str>) -> u8 {
    muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
    // parent press-2 reclaim: the bar comes back NOW, not at the next poll
    // tick (the poller owns status-left's text; we own visibility).
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-gu", "key-table"]);
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
    muxmesh_core::tmux::run_tmux(base, &["refresh-client", "-S"]);
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

/// CHILD press while in control: hide OUR bar (one-bar rule CRIT1) and tell
/// our parent to reclaim — the poller will repaint our bar with OUR hostname
/// when it comes back. The bar text is never edited here: status-left belongs
/// to the poller (single writer).
fn child_press(base: &[String], hidden_now: bool) -> u8 {
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
