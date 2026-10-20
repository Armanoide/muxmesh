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

mod poll;
mod send;

use std::process;

fn main() {
    let cfg = muxmesh_core::HubConfig::load();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("poll");

    match cmd {
        "poll" if args.iter().any(|a| a == "--once") => poll::run_once(&cfg),
        "poll" => poll::run(&cfg),
        "tree" => tree(&cfg, args.iter().any(|a| a == "--local")),
        "send" => std::process::exit(send::run_send(&cfg, &args[1..]) as i32),
        "armed-host" => std::process::exit(send::run_armed_host(&cfg) as i32),
        other => {
            eprintln!("muxmesh-cli: unknown subcommand '{other}' (expected: poll | tree | send | armed-host)");
            process::exit(2);
        }
    }
}

/// Print host/session tree. Default reads the live hub (poll.hub); --local
/// falls back to the local state file (for a hub running on this machine).
fn tree(cfg: &muxmesh_core::HubConfig, from_local: bool) {
    let doc = if from_local {
        poll::read_state(&cfg.state_path)
    } else {
        match fetch_hub_state(&cfg.poll_hub) {
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

/// GET /state from the hub (poll.hub, "host:port"). Minimal HTTP/1.0 over
/// std::net (no extra deps). GET /state is open on the hub — no token needed.
pub fn fetch_hub_state(hub_addr: &str) -> Result<muxmesh_core::StateDoc, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut stream = TcpStream::connect(hub_addr).map_err(|e| format!("{hub_addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let req = format!("GET /state HTTP/1.0\r\nHost: {hub_addr}\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    // capped read (1 MiB): a misdirected poll.hub must not exhaust memory.
    let mut raw = String::new();
    {
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if raw.len() + n > 1_048_576 {
                return Err("response over 1 MiB (wrong hub?)".into());
            }
            raw.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed http response".to_string())?;
    let status = head.lines().next().unwrap_or("");
    if !(status.ends_with("200") || status.contains(" 200 ")) {
        return Err(format!("hub returned: {status}"));
    }
    muxmesh_core::StateDoc::from_json_str(body).map_err(|e| format!("bad state json: {e}"))
}
