//! ssh process-tree detection — the only place that spawns `ps`/`pgrep`.
//!
//! Detection rule (locked): a pane is armed when its process tree holds an
//! OUTBOUND ssh client (argv[0] basename == `ssh`, never `sshd`). The tree
//! is walked DOWN from the pane pid only — tmux servers on other machines
//! are never touched.

use std::process::Command;

use crate::StateDoc;

use super::exec::pane_pids;

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

/// Host targeted by a live `ssh <host>` in this window (first armed match wins).
/// Used by the key bindings to know which machine F8/F9/F10 should drive.
/// Checks each pane's OWN process first, then its descendants: a window opened
/// with `tmux new-window 'ssh host'` has ssh as the pane pid itself, so
/// scanning only children (pgrep -P) would never arm it.
pub fn window_ssh_host(base: &[String], wid: &str) -> Option<String> {
    let pids = pane_pids(base, wid)?;
    for pid in pids {
        // the pane process itself may be the ssh client (window-command panes).
        if let Some(h) = ssh_host_of_pid(pid) {
            return Some(h);
        }
        if let Some(h) = pane_ssh_host(pid) {
            return Some(h);
        }
    }
    None
}

/// Host keys of live `ssh` CLIENTS on this machine — i.e. machines we are
/// currently dialing OUT to (we are their parent). Inside an inbound ssh there
/// is no ssh client process here, so a pure target returns empty — its own
/// parents come from `parents_of` (the hub directory), not from here.
///
/// Known limit: a parent who launched its ssh INSIDE tmux (`new-window 'ssh
/// host'`) is not caught here — `ps -eo args` shows the pane's argv
/// (`sh -c ssh ...`, no leading `ssh`) and the client may hang off a shell
/// tree, not at argv head. tmux-armed parents are caught by the badge scan
/// (`window_ssh_host`), which is why armed_host_of_current_window stays
/// authoritative for the press-1 branch.
pub fn ssh_parent_hosts() -> Vec<String> {
    let out = match Command::new("ps").args(["-eo", "args"]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return vec![],
    };
    let mut hosts: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(ssh_target_of)
        .map(|h| h.split_once('@').map(|(_, h)| h).unwrap_or(&h).to_string())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Parent hub keys that currently have a live ssh INTO `me`: scan every other
/// host's reported outbound ssh targets (`parents`) for us. This is how the
/// child finds whom to answer on press 2 — the parent's own registration says
/// who it dialed, and we are the machine it dialed into. First match wins;
/// multiple simultaneous inbound ssh sessions to one child are not supported.
pub fn parents_of(doc: &StateDoc, me: &str) -> Vec<String> {
    doc.hosts
        .iter()
        .filter(|(h, st)| *h != me && st.parents.iter().any(|t| t == me))
        .map(|(h, _)| h.clone())
        .collect()
}

/// Target host if the given pid's own command line is `ssh <host>`.
pub fn ssh_host_of_pid(pid: u32) -> Option<String> {
    let cmd = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !cmd.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&cmd.stdout).trim().to_string();
    ssh_target_of(&line).map(|h| h.split_once('@').map(|(_, h)| h).unwrap_or(&h).to_string())
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
    fn pane_ssh_host_missing_pid_is_none() {
        // pid 0 has no children in practice
        assert_eq!(pane_ssh_host(0), None);
    }

    #[test]
    fn window_ssh_host_no_tmux_is_none_not_panic() {
        // a dead socket: None, never a panic, never a phantom server
        let base = vec!["tmux".into(), "-S".into(), "/nonexistent.sock".into()];
        assert_eq!(window_ssh_host(&base, "%999"), None);
    }
}
