//! muxmesh-cli — driver-side binary: badge poller, `tree`, and (brique 3) key commands.
//!
//! Subcommands:
//!   poll  : the badge loop. Per window: does a pane hold a live `ssh <host>`
//!           (pgrep + ps, local) AND is <host> armed+fresh in the hub state
//!           -> set @muxmesh-armed-win. State comes from the hub over HTTP
//!           (GET /state, open endpoint) — a driver has no local state.json.
//!   tree  : print the aggregated state (live hub by default, --local file).
//!
//! Detection rule: the pane's process tree is walked DOWN (pane pid -> children
//! via pgrep) so tmux servers on other machines are never touched. A child is
//! an ssh client when argv[0] basename is `ssh` (client) — never `sshd`.
//!
//! std + muxmesh-core only -> offline build.

mod send;

use std::process;

fn main() {
    let cfg = muxmesh_core::HubConfig::load();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("poll");

    match cmd {
        "poll" if args.iter().any(|a| a == "--once") => muxmesh_core::poll::run_once(&cfg),
        "poll" => muxmesh_core::poll::run(&cfg),
        "tree" => tree(&cfg, args.iter().any(|a| a == "--local")),
        "send" => std::process::exit(send::run_send(&cfg, &args[1..]) as i32),
        "armed-host" => std::process::exit(send::run_armed_host(&cfg) as i32),
        "bar-toggle" => std::process::exit(send::run_bar_toggle(&cfg) as i32),
        other => {
            eprintln!("muxmesh-cli: unknown subcommand '{other}' (expected: poll | tree | send | armed-host | bar-toggle)");
            process::exit(2);
        }
    }
}

/// Print host/session tree. Default reads the live hub (poll.hub); --local
/// falls back to the local state file (for a hub running on this machine).
fn tree(cfg: &muxmesh_core::HubConfig, from_local: bool) {
    let doc = if from_local {
        muxmesh_core::poll::read_state(&cfg.state_path)
    } else {
        match muxmesh_core::poll::fetch_hub_state(&cfg.poll_hub) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("muxmesh-cli tree: hub unreachable: {e}");
                process::exit(1);
            }
        }
    };
    let now = muxmesh_core::now_unix();
    if doc.hosts.is_empty() {
        println!("muxmesh: no hosts known");
        return;
    }
    for (host, st) in &doc.hosts {
        let age = now.saturating_sub(st.seen);
        let status = if st.is_fresh(now, cfg.ttl) {
            "armed"
        } else {
            "offline"
        };
        println!(
            "{host} [{status}] age={age}s windows={}",
            st.sessions.iter().map(|s| s.windows).sum::<u32>()
        );
        for s in &st.sessions {
            println!(
                "  {} ({} win, active: {})",
                s.name,
                s.windows,
                if s.active_window.is_empty() {
                    "-"
                } else {
                    &s.active_window
                }
            );
        }
    }
}
