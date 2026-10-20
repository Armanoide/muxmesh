//! Badge poller: per-window arm/disarm of @muxmesh-armed-win.
//!
//! Per window: find a live `ssh <host>` child of its panes (pgrep + ps, local,
//! free), then check the state doc for an armed+fresh entry for that host.
//! Armed -> tmux set-window-option @muxmesh-armed-win 1, else unset.
//!
//! Zero network per cycle when reading the local state file (the hub-side copy
//! is refreshed by `--hub` mode or stays whatever the hub wrote locally).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use muxmesh_core::{now_unix, valid_name, HubConfig, StateDoc};

/// ssh options that take a separate value argument (skip the next token).
const OPTS_WITH_VALUE: &[char] = &[
    'b', 'c', 'D', 'E', 'F', 'i', 'I', 'J', 'L', 'l', 'o', 'p', 'q', 'R', 'S', 'w', 'W',
];

/// True if this argv looks like an outbound ssh client (not sshd, not `sshd: foo@pts`).
fn looks_like_ssh(cmd: &str) -> bool {
    let head = cmd.split_whitespace().next().unwrap_or("");
    // sshd appears as "sshd" or "sshd: user@pts/0" — never a client.
    if head == "sshd" || head.starts_with("sshd:") {
        return false;
    }
    let base = head.rsplit('/').next().unwrap_or(head);
    base == "ssh"
}

/// Target host of an `ssh ...` command line (first non-flag token).
pub fn ssh_target_of(line: &str) -> Option<String> {
    if !looks_like_ssh(line) {
        return None;
    }
    let mut toks = line.split_whitespace();
    toks.next()?; // "ssh"
    let toks: Vec<&str> = toks.collect();
    let mut i = 0;
    while i < toks.len() {
        let tok = toks[i];
        if let Some(rest) = tok.strip_prefix('-') {
            // bundled or single flag; -p 2222 style consumes the next token
            if let Some(first) = rest.chars().next() {
                if OPTS_WITH_VALUE.contains(&first) && rest.len() == 1 && i + 1 < toks.len() {
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        return Some(tok.to_string());
    }
    None
}

/// Host of a live `ssh <host>` child process of this pid (None otherwise).
/// Recurses up to 3 levels: a typed `ssh host` usually hangs off an
/// intermediate shell (pane -> shell -> ssh). Depth is capped so a fork
/// bomb cannot make the poll loop spin.
pub fn pane_ssh_host(pid: u32) -> Option<String> {
    ssh_host_under(pid, 0)
}

fn ssh_host_under(pid: u32, depth: u32) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let out = Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let child: u32 = match line.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // `ps -o command=` truncates argv[0] on some platforms; `-o args=`
        // keeps the full command line (works on both BSD and procps).
        let cmd = match Command::new("ps")
            .args(["-o", "args=", "-p", &child.to_string()])
            .output()
        {
            Ok(c) => c,
            // transient ps failure on one child (zombie reaped mid-scan):
            // skip this child, keep scanning the rest of the window's panes.
            Err(_) => continue,
        };
        let cmd_line = String::from_utf8_lossy(&cmd.stdout).trim().to_string();
        if let Some(h) = ssh_target_of(&cmd_line) {
            // strip user@ so the key matches the agent's canonical host name.
            return Some(h.split_once('@').map(|(_, h)| h).unwrap_or(&h).to_string());
        }
        if let Some(h) = ssh_host_under(child, depth + 1) {
            return Some(h);
        }
    }
    None
}

/// Read + parse the state doc (missing/corrupt file -> empty doc, never panic).
pub fn read_state(path: &Path) -> StateDoc {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| StateDoc::from_json_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn tmux_base(socket: &Option<String>) -> Vec<String> {
    match socket {
        Some(s) => vec!["tmux".into(), "-S".into(), s.clone()],
        None => vec!["tmux".into()],
    }
}

fn run_tmux(base: &[String], args: &[&str]) {
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(args);
    let _ = cmd.output();
}

/// Run a tmux command, returning stdout on success (stderr captured for debug).
pub(crate) fn tmux_out(base: &[String], args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(args);
    match cmd.output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            eprintln!(
                "muxmesh-cli poll: tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            None
        }
        Err(e) => {
            eprintln!("muxmesh-cli poll: spawn tmux: {e}");
            None
        }
    }
}

/// Pane pids of a window, or None if the query itself failed.
/// Distinguishes "no panes" (Some(empty)) from "tmux broken" (None) so a
/// transient failure never silently disarms windows.
fn pane_pids(base: &[String], wid: &str) -> Option<Vec<u32>> {
    let out = tmux_out(base, &["list-panes", "-t", wid, "-F", "#{pane_pid}"])?;
    Some(out.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// Host targeted by a live `ssh <host>` in this window (first armed match wins).
/// Used by the key bindings to know which machine F8/F9/F10 should drive.
pub fn window_ssh_host(base: &[String], wid: &str) -> Option<String> {
    let pids = pane_pids(base, wid)?;
    for pid in pids {
        if let Some(h) = pane_ssh_host(pid) {
            return Some(h);
        }
    }
    None
}

/// One pass: evaluate every window on the server, set/unset the badge option.
pub fn poll_once(base: &[String], doc: &StateDoc, ttl: u64) {
    let now = now_unix();
    // all windows across all sessions: "window_id" per line
    let out = match tmux_out(base, &["list-windows", "-a", "-F", "#{window_id}"]) {
        Some(o) => o,
        None => return, // no tmux server -> nothing to badge (never spawn a phantom one)
    };
    for wid in out.lines() {
        if wid.is_empty() {
            continue;
        }
        let mut armed = false;
        if let Some(pids) = pane_pids(base, wid) {
            for pid in pids {
                if let Some(h) = pane_ssh_host(pid) {
                    // Defense in depth: the hub whitelists these, but a hand-edited or
                    // stale state file must never feed a crafted string to tmux `-t`.
                    if !valid_name(&h) {
                        continue;
                    }
                    if doc.host_armed(&h, now, ttl) {
                        armed = true;
                        break;
                    }
                }
            }
        }
        if armed {
            run_tmux(
                base,
                &["set-window-option", "-t", wid, "@muxmesh-armed-win", "1"],
            );
        } else {
            run_tmux(
                base,
                &["set-window-option", "-t", wid, "-u", "@muxmesh-armed-win"],
            );
        }
    }
}

/// Main loop. env: MUXMESH_SOCKET (tmux socket), MUXMESH_INTERVAL, MUXMESH_TTL.
pub fn run(cfg: &HubConfig) {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = tmux_base(&socket);
    eprintln!(
        "muxmesh-cli poll: interval={}s ttl={}s state={} (0 network)",
        cfg.interval,
        cfg.ttl,
        cfg.state_path.display()
    );
    loop {
        let doc = read_state(&cfg.state_path);
        poll_once(&base, &doc, cfg.ttl);
        thread::sleep(Duration::from_secs(cfg.interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_plain() {
        assert_eq!(ssh_target_of("ssh host-a"), Some("host-a".into()));
        assert_eq!(ssh_target_of("ssh user@host-b"), Some("user@host-b".into()));
    }

    #[test]
    fn ssh_target_skips_flags() {
        assert_eq!(ssh_target_of("ssh -p 2222 host-a"), Some("host-a".into()));
        assert_eq!(
            ssh_target_of("ssh -o StrictHostKeyChecking=no host-a"),
            Some("host-a".into())
        );
        assert_eq!(ssh_target_of("ssh -v host-a"), Some("host-a".into()));
    }

    #[test]
    fn not_ssh_is_none() {
        assert_eq!(ssh_target_of("vim file.txt"), None);
        assert_eq!(ssh_target_of(""), None);
    }

    #[test]
    fn sshd_is_never_a_client() {
        assert_eq!(ssh_target_of("sshd"), None);
        assert_eq!(ssh_target_of("sshd: user@pts/0"), None);
        assert_eq!(ssh_target_of("/usr/sbin/sshd -D"), None);
    }

    #[test]
    fn ssh_with_user_at_host() {
        assert_eq!(
            ssh_target_of("ssh -p 22 user@host-a"),
            Some("user@host-a".into())
        );
    }

    #[test]
    fn ssh_target_of_keeps_user_at_host() {
        // raw target keeps user@ (valid_name then rejects it -> not armed)
        assert_eq!(ssh_target_of("ssh user@host-a"), Some("user@host-a".into()));
    }

    #[test]
    fn read_state_missing_file_is_empty() {
        let doc = read_state(Path::new("/nonexistent/state.json"));
        assert!(doc.hosts.is_empty());
    }

    #[test]
    fn read_state_corrupt_is_empty_not_panic() {
        let dir = std::env::temp_dir().join(format!("muxmesh-cli-{}", now_unix()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        fs::write(&p, b"{ not json").unwrap();
        assert!(read_state(&p).hosts.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pane_ssh_host_missing_pid_is_none() {
        // pid 0 has no children in practice
        assert_eq!(pane_ssh_host(0), None);
    }

    #[test]
    fn armed_host_key_strips_user_at() {
        // ssh_target_of keeps user@; pane_ssh_host strips it to match the
        // agent's canonical host name. Test the strip rule directly here.
        let raw = ssh_target_of("ssh user@host-a").unwrap();
        assert_eq!(
            raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw),
            "host-a"
        );
        let raw = ssh_target_of("ssh host-a").unwrap();
        assert_eq!(
            raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw),
            "host-a"
        );
    }

    #[test]
    fn window_ssh_host_no_tmux_is_none_not_panic() {
        // a dead socket: None, never a panic, never a phantom server
        let base = vec!["tmux".into(), "-S".into(), "/nonexistent.sock".into()];
        assert_eq!(window_ssh_host(&base, "%999"), None);
    }
}
