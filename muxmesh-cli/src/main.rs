//! muxmesh-cli — driver-side binary: badge poller, `tree`, and (brique 3) key commands.
//!
//! Subcommands:
//!   poll  : the badge loop. Per window: does a pane hold a live `ssh <host>`
//!           (pgrep + ps, local) AND is <host> armed+fresh in the state doc
//!           -> set @muxmesh-armed-win. Zero network per cycle (local file read).
//!   tree  : print the aggregated state (local file, or GET /state with --hub).
//!
//! Detection rule: the pane's process tree is walked DOWN (pane pid -> children
//! via pgrep) so tmux servers on other machines are never touched. A child is
//! an ssh client when argv[0] basename is `ssh` (client) — never `sshd`.
//!
//! std + muxmesh-core only -> offline build.

mod poll;
mod send;

use std::process;

fn main() {
    let cfg = muxmesh_core::HubConfig::load();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("poll");

    match cmd {
        "poll" => poll::run(&cfg),
        "tree" => tree(&cfg, args.iter().any(|a| a == "--hub")),
        "send" => std::process::exit(send::run_send(&cfg, &args[1..]) as i32),
        "armed-host" => std::process::exit(send::run_armed_host(&cfg) as i32),
        other => {
            eprintln!("muxmesh-cli: unknown subcommand '{other}' (expected: poll | tree | send | armed-host)");
            process::exit(2);
        }
    }
}

/// Print host/session tree. Default reads the local state file; --hub fetches live.
fn tree(cfg: &muxmesh_core::HubConfig, from_hub: bool) {
    let doc = if from_hub {
        match fetch_hub_state(cfg) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("muxmesh-cli tree: hub unreachable: {e}");
                process::exit(1);
            }
        }
    } else {
        poll::read_state(&cfg.state_path)
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

/// GET /state from the hub. Minimal HTTP/1.0 over std::net (no extra deps).
fn fetch_hub_state(cfg: &muxmesh_core::HubConfig) -> Result<muxmesh_core::StateDoc, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    // "host:port" -> split at the LAST colon (IPv6-safe enough for LAN use).
    let (host, port) = cfg.bind.rsplit_once(':').unwrap_or(("127.0.0.1", "9997"));
    let host = if host == "0.0.0.0" || host.is_empty() {
        "127.0.0.1"
    } else {
        host
    };
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let token = muxmesh_core::read_token().as_str().to_string();
    let req = format!("GET /state HTTP/1.0\r\nX-Muxmesh-Token: {token}\r\nHost: {addr}\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let body = raw
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .ok_or_else(|| "malformed http response".to_string())?;
    muxmesh_core::StateDoc::from_json_str(body).map_err(|e| format!("bad state json: {e}"))
}
