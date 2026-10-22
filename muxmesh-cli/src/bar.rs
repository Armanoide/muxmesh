//! The F10 bar-toggle state machine — the whole delegation logic.
//!
//! Roles are decided by the KEYSTROKE's server, not configured anywhere:
//!
//!   PARENT (armed ssh focused)
//!     press 1 -> DELEGATE: BarShow(child) via hub, bar off, prefix None +
//!                 switch-client -T off (samoshkin OFF mode). F10 stays bound
//!                 in table `off` = the parent's own escape hatch.
//!     press 2 -> RECLAIM locally: prefix restored + table root + bar on,
//!                 BarHide(child) via hub (best-effort). Never depends on the
//!                 keystroke traversing ssh.
//!
//!   CHILD (no armed ssh — the keystroke traversed ssh, or a plain local F10)
//!     press 2 while parent delegated (prefix None here is impossible on a
//!                 child; delegated=true here means THIS server delegated from
//!                 an earlier press and is waking itself) -> self-reclaim.
//!     otherwise   -> local bar toggle; if hiding, answer ParentReclaim to
//!                 whoever armed an ssh into us (hub directory).
//!
//! Commands flow driver -> HUB -> agent over the agent's persistent outbound
//! WS. The hub NEVER dials an agent (transport.rs owns the wire).

use muxmesh_core::{Command, HubConfig};

use crate::tmux_state;
use crate::transport;

/// `muxmesh-cli bar-toggle` entry point. Returns a process exit code.
pub fn run(cfg: &HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::tmux::tmux_base(&socket);
    // TRACE: millisecond timestamp + PID of every invocation, so the log shows
    // if ONE physical F10 fires bar-toggle TWICE (double-fire MacBook).
    let t0 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    tracing::info!(
        "bar-toggle: START ts={t0} pid={} (hub={}, socket={:?})",
        std::process::id(),
        cfg.poll_hub,
        socket
    );

    let wid = match muxmesh_core::tmux::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(o) => o.trim().to_string(),
        None => {
            tracing::error!("bar-toggle: STOP at step 1 — no tmux server / no current window");
            return 1;
        }
    };
    let hidden_now = tmux_state::bar_hidden(&base);
    let delegated = tmux_state::is_delegated(&base);
    let host = tmux_state::armed_host_of_current_window(cfg);
    tracing::info!(
        "bar-toggle: step 2 — window {wid}, bar-hidden={hidden_now}, armed host = {:?}, delegated={delegated}",
        host
    );

    match host {
        Some(h) => parent_press(&base, &h, delegated, cfg, t0),
        None => child_or_self_press(&base, hidden_now, delegated, cfg, t0),
    }
}

/// PARENT side: driver presses F10 with an armed ssh pane focused.
fn parent_press(base: &[String], h: &str, delegated: bool, cfg: &HubConfig, t0: u128) -> u8 {
    if !delegated {
        delegate(base, h, cfg, t0)
    } else {
        reclaim_local(base, h, cfg, t0)
    }
}

/// press 1 — hand the keyboard to the child (samoshkin OFF mode).
///
/// GUARD: never hand over blindly — the prefix is cut only after BarShow is
/// ACKed (202) by the hub, proving the child is reachable. If the prefix cut
/// itself fails, full rollback: the keyboard never leaves the parent's hands.
fn delegate(base: &[String], h: &str, cfg: &HubConfig, t0: u128) -> u8 {
    if !transport::post_verb(Command::BarShow, h, "BarShow", cfg) {
        tracing::error!("bar-toggle: STOP — child unreachable, NOT delegating (parent keeps prefix+bar)");
        println!("muxmesh: remote {h} unreachable — control NOT delegated (parent keeps the keyboard)");
        return 1;
    }
    // child is live: hide local bar, then deafen the parent — samoshkin's
    // canonical OFF mode: prefix None + switch-client -T off. F10 stays bound
    // in table `off` (muxmesh.tmux): press 2 traverses ssh into the child OR
    // fires this table directly — both paths restore control. No unbind:
    // unbinding F10 killed the parent's recovery path (MacBook symptom).
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "off"]);
    // switch-client is per-CLIENT state: headless servers (E2E, no attached
    // client) answer "no current client" — prefix None still delegates the
    // keyboard there, so failure is a warning.
    let deaf = muxmesh_core::tmux::tmux_run(base, &["switch-client", "-T", "off"]);
    let table = muxmesh_core::tmux::tmux_out(base, &["display-message", "-p", "#{client_key_table}"])
        .map(|o| o.trim().to_string())
        .unwrap_or_default();
    tracing::info!(
        "bar-toggle: DEAFNESS applied — switch-client ok={deaf} client_key_table='{table}' ts={t0}"
    );
    let cut = muxmesh_core::tmux::tmux_run(base, &["set-option", "-g", "prefix", "None"]);
    // ECC P0: never claim DELEGATED without re-reading tmux.
    let now = tmux_state::is_delegated(base);
    if !cut || !now {
        muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
        muxmesh_core::tmux::run_tmux(base, &["bind-key", "-n", "F10", "run-shell", &tmux_state::f10_cmd()]);
        muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
        tracing::error!("bar-toggle: prefix cut FAILED — rolled back, parent keeps control");
        println!("muxmesh: delegation FAILED (prefix cut refused) — parent keeps the keyboard");
        return 1;
    }
    println!("muxmesh: control DELEGATED to {h} — press F10 again to reclaim");
    0
}

/// press 2 on the PARENT while delegated — reclaim locally, no network needed.
///
/// F10 fired HERE either via the off-table binding (canonical samoshkin
/// return) or a stray press while the table was still root. Either way the
/// user is asking for control back: restore prefix + table root + bar on,
/// then tell the child to hide its bar via the hub (best-effort — if the hub
/// is down the child's bar stays, but the parent's keyboard is back).
fn reclaim_local(base: &[String], h: &str, cfg: &HubConfig, t0: u128) -> u8 {
    muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
    muxmesh_core::tmux::run_tmux(base, &["switch-client", "-T", "root"]);
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
    transport::post_verb(Command::BarHide, h, "BarHide", cfg);
    let (prefix_back, f10_back) = tmux_state::reclaim_audit(base);
    tracing::info!(
        "bar-toggle: PARENT self-reclaim on press2 — prefix_back={prefix_back} f10_back={f10_back} ts={t0}"
    );
    println!("muxmesh: control RECLAIMED (F10 revif sur le parent)");
    0
}

/// CHILD side (or no armed ssh on this window): two real paths.
/// 1. delegated=true here = THIS server delegated earlier (press 1 cut its own
///    prefix) and the keystroke came back to it -> self-reclaim (same three
///    steps as reclaim_local; the armed host is unknown, so ParentReclaim
///    direction is reversed via answer_parent_reclaim's hub lookup).
/// 2. plain local F10 (nested remote tmux, not delegating) -> toggle this
///    server's bar; if hiding, answer ParentReclaim to our hub parent.
fn child_or_self_press(
    base: &[String],
    hidden_now: bool,
    delegated: bool,
    cfg: &HubConfig,
    t0: u128,
) -> u8 {
    if delegated {
        muxmesh_core::tmux::tmux_run(base, &["set-option", "-gu", "prefix"]);
        muxmesh_core::tmux::run_tmux(base, &["switch-client", "-T", "root"]);
        muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", "on"]);
        if !cfg.poll_hub.is_empty() {
            transport::answer_parent_reclaim(cfg);
        }
        let (prefix_back, f10_back) = tmux_state::reclaim_audit(base);
        tracing::info!(
            "bar-toggle: PARENT self-reclaim (no armed ssh, delegated=true) — prefix_back={prefix_back} f10_back={f10_back} ts={t0}"
        );
        println!("muxmesh: control RECLAIMED (F10 revif sur le parent)");
        return 0;
    }
    let next = if hidden_now { "on" } else { "off" };
    tracing::info!(
        "bar-toggle: CHILD/local branch ts={t0} — no armed ssh here, toggling THIS server's bar -> status {next}"
    );
    // tmux redraws the bar by itself after set status — no refresh-client.
    muxmesh_core::tmux::run_tmux(base, &["set-option", "-g", "status", next]);
    if next == "off" && !cfg.poll_hub.is_empty() {
        transport::answer_parent_reclaim(cfg);
    }
    println!("muxmesh: local bar {next}");
    0
}
