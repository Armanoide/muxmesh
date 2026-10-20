//! `muxmesh-cli why` — one-shot arm diagnosis of the CURRENT tmux window.
//! Prints the full decision tree: window id, every pane pid + its process
//! tree (looking for a live ssh client), the hub state (which hosts are
//! armed+fresh) and the final verdict with the exact failing condition.
//! This is the answer to "why doesn't F10 do anything" without strace.

use std::process::Command;

use muxmesh_core::HubConfig;

pub fn run_why(cfg: &HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::poll::tmux_base(&socket);

    println!("muxmesh-cli why — hub = {}", cfg.poll_hub);

    // 1. current window
    let wid = match muxmesh_core::poll::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(w) => w.trim().to_string(),
        None => {
            println!("X no tmux server / no current window (run inside tmux)");
            return 1;
        }
    };
    let wname = muxmesh_core::poll::tmux_out(&base, &["display", "-p", "#{window_name}"])
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("* window {wid} \"{wname}\"");

    // 2. panes + process trees
    let pids = muxmesh_core::poll::pane_pids(&base, &wid).unwrap_or_default();
    if pids.is_empty() {
        println!("X no panes found for this window");
    }
    let mut ssh_targets: Vec<String> = Vec::new();
    for pid in &pids {
        println!("  pane pid {pid}:");
        print_tree(*pid, "    ");
        if let Some(h) = muxmesh_core::poll::pane_ssh_host(*pid) {
            println!("    -> live ssh client, target = {h}");
            ssh_targets.push(h);
        }
    }

    // 3. hub state
    let doc = muxmesh_core::poll::fetch_state(&cfg.poll_hub);
    let now = muxmesh_core::now_unix();
    if doc.hosts.is_empty() {
        println!("* hub state: EMPTY (no agent ever pushed? hub up?)");
    }
    for (host, st) in &doc.hosts {
        let age = now.saturating_sub(st.seen);
        println!(
            "* hub: {host} armed={} seen {}s ago fresh={}",
            st.armed,
            age,
            st.is_fresh(now, cfg.ttl)
        );
    }

    // 4. verdict — the first failing condition wins
    let target = match ssh_targets.first() {
        Some(t) => t.clone(),
        None => {
            println!("VERDICT: not armed — no live `ssh <host>` client in this window's panes");
            println!(
                "  (an agent/hub running here does NOT arm the window; it must be an OUTBOUND ssh)"
            );
            return 1;
        }
    };
    if !muxmesh_core::valid_host(&target) {
        println!(
            "VERDICT: not armed — ssh target '{target}' rejected by valid_host ([A-Za-z0-9_.-])"
        );
        return 1;
    }
    if doc.host_armed(&target, now, cfg.ttl) {
        println!("VERDICT: ARMED -> target {target} (F10 should work here)");
        return 0;
    }
    match doc.hosts.get(&target) {
        None => println!(
            "VERDICT: not armed — hub does not know '{target}'. Does an agent run there with host=\"{target}\"? (known: {:?})",
            doc.hosts.keys().collect::<Vec<_>>()
        ),
        Some(st) if !st.armed => println!("VERDICT: not armed — '{target}' reports armed=false (remote tmux has no client attached?)"),
        Some(st) => println!(
            "VERDICT: not armed — '{target}' stale (seen {}s ago > ttl {}s; agent down or sleeping?)",
            now.saturating_sub(st.seen),
            cfg.ttl
        ),
    }
    1
}

/// Recursively print a process tree (pgrep -P + ps), depth-capped at 4.
fn print_tree(pid: u32, indent: &str) {
    let out = match Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return,
    };
    for line in String::from_utf8_lossy(&out).lines() {
        let child: u32 = match line.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmd = Command::new("ps")
            .args(["-o", "args=", "-p", &child.to_string()])
            .output()
            .map(|c| String::from_utf8_lossy(&c.stdout).trim().to_string())
            .unwrap_or_default();
        println!("{indent}{child} {cmd}");
        print_tree(child, &format!("{indent}  "));
    }
}
