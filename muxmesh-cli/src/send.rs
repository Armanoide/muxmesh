//! Send closed-list commands to a remote agent (`POST /cmd`).
//!
//!   muxmesh-cli send <host> <command> [args...]
//!     send host-a NewWindow main
//!     send host-a NewSession build
//!     send host-a FocusWindow main 2
//!     send host-a AttachOrDetach main
//!
//! The host's port comes from agent.cmd_port (default :9998) — the client and
//! the agent share config.toml, so both agree on the port. Token is added from
//! ~/.config/muxmesh/token. std::net only -> offline build.

use std::io::{Read, Write};
use std::net::TcpStream;

use std::time::{SystemTime, UNIX_EPOCH};

use muxmesh_core::{Command, CommandEnvelope};

/// Build a Command from CLI words. Returns None on unknown verb / arity.
pub fn parse_command(words: &[String]) -> Option<Command> {
    match words.first().map(|s| s.as_str())? {
        "NewWindow" => Some(Command::NewWindow {
            session: words.get(1)?.clone(),
        }),
        "NewSession" => Some(Command::NewSession {
            name: words.get(1)?.clone(),
        }),
        "FocusWindow" => Some(Command::FocusWindow {
            session: words.get(1)?.clone(),
            window: words.get(2)?.clone(),
        }),
        "AttachOrDetach" => Some(Command::AttachOrDetach {
            session: words.get(1)?.clone(),
        }),
        "BarHide" => Some(Command::BarHide),
        "BarShow" => Some(Command::BarShow),
        _ => None,
    }
}

/// POST /cmd to host:port. Returns the raw ack body. HTTP/1.1 + Connection:
/// close (agent litigates Content-Length); std only.
pub fn send_cmd(
    host: &str,
    port: u16,
    token: &str,
    env: &CommandEnvelope,
) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let body = serde_json::to_vec(env).map_err(|e| e.to_string())?;
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    // The agent parses the request line with read_line, which buffers up to
    // 8 KiB: if it has already swallowed our headers into its buffer, the
    // body must arrive in a SEPARATE TCP segment or it waits forever.
    let req = format!(
        "POST /cmd HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Muxmesh-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    // body in its own segment, after the header segment was flushed (see above)
    stream.write_all(&body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    // EAGAIN-safe read: this environment has surfaced spurious WouldBlock on
    // blocking sockets; retry on Interrupted/WouldBlock-with-data instead of
    // aborting the whole send on the first hiccup.
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `muxmesh-cli armed-host` — print the remote host of the CURRENT tmux
/// window if a live `ssh <host>` sits in it AND <host> is armed+fresh on the
/// hub. Exit 1 otherwise. This is what F8/F9 call to pick the send target.
pub fn run_armed_host(cfg: &muxmesh_core::HubConfig) -> u8 {
    match armed_host_of_current_window(cfg) {
        Some(h) => {
            println!("{h}");
            0
        }
        None => 1,
    }
}

/// The armed remote host of the current window, if any. Detection is live
/// (pgrep walk + hub state), NOT the @muxmesh-armed-win badge option — the
/// badge is display-only; the decision always re-runs in Rust.
fn armed_host_of_current_window(cfg: &muxmesh_core::HubConfig) -> Option<String> {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::poll::tmux_base(&socket);
    let wid = muxmesh_core::poll::tmux_out(&base, &["display", "-p", "#{window_id}"])?;
    let wid = wid.trim().to_string();
    let doc = muxmesh_core::poll::fetch_state(&cfg.poll_hub);
    tracing::debug!(
        "armed-host: hub {} -> {} host(s): {:?}",
        cfg.poll_hub,
        doc.hosts.len(),
        doc.hosts
            .iter()
            .map(|(h, s)| format!(
                "{h}:armed={}:seen={}s",
                s.armed,
                muxmesh_core::now_unix().saturating_sub(s.seen)
            ))
            .collect::<Vec<_>>()
    );
    let h = match muxmesh_core::poll::window_ssh_host(&base, &wid) {
        Some(h) => h,
        None => {
            tracing::debug!("window {wid}: no live ssh client found in its panes");
            return None;
        }
    };
    tracing::debug!("armed-host: live ssh target in window {wid} = '{h}'");
    if !muxmesh_core::valid_host(&h) {
        tracing::debug!("window {wid}: ssh target '{h}' rejected by valid_host");
        return None;
    }
    if doc.host_armed(&h, muxmesh_core::now_unix(), cfg.ttl) {
        tracing::debug!("armed-host: '{h}' armed+fresh on hub -> ARMED");
        Some(h)
    } else {
        let known: Vec<&String> = doc.hosts.keys().collect();
        tracing::debug!(
            "ssh target '{h}' not armed on hub (known hosts: {known:?}; hub={})",
            cfg.poll_hub
        );
        None
    }
}

/// `muxmesh-cli bar-toggle` — the whole F10 logic, no if-shell in tmux.
/// Finds the armed host of the current window (live detection), keeps the
/// hidden/shown state in a per-window tmux user option (@muxmesh-bar-hidden,
/// written/read via tmux show/set-window-option — the state lives in the
/// tmux server, Rust is just the brain), and POSTs BarHide or BarShow.
pub fn run_bar_toggle(cfg: &muxmesh_core::HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::poll::tmux_base(&socket);
    tracing::info!(
        "bar-toggle: start (hub={}, socket={:?})",
        cfg.poll_hub,
        socket
    );
    let wid = match muxmesh_core::poll::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(o) => o.trim().to_string(),
        None => {
            tracing::error!("bar-toggle: STOP at step 1 — no tmux server / no current window");
            return 1;
        }
    };

    // hidden-state of THIS server's bar (window option, survives presses)
    let hidden_now = muxmesh_core::poll::tmux_out(
        &base,
        &["show-window-option", "-t", &wid, "@muxmesh-bar-hidden"],
    )
    .map(|o| o.ends_with("on"))
    .unwrap_or(false);
    tracing::debug!("bar-toggle: step 1 ok — window {wid}, bar-hidden={hidden_now}");

    let host = armed_host_of_current_window(cfg);
    tracing::info!("bar-toggle: step 2 — armed host = {:?}", host);

    // Parent/child roles are decided by the KEYSTROKE's server, not by us:
    // - F10 lands on the driver's server while an armed ssh has focus ->
    //   this server is the PARENT: hide its own bar, pass control down.
    // - F10 pressed inside the nested remote tmux traverses ssh and lands
    //   on the CHILD's server, where armed_host_of_current_window finds no
    //   outbound ssh -> local branch below.
    match host {
        // PARENT side (driver): a live armed ssh is in this window. The parent
        // bar yields the screen to the child: hide LOCAL, then forward F10
        // into the focused pane as W.P (%pane_id does not traverse nested ssh
        // — verified in tmux-remote) so the remote server's own binding shows
        // ITS bar. One bar on screen: the child's. Next press reverses.
        Some(h) if !hidden_now => {
            tracing::info!("bar-toggle: PARENT branch — hiding local bar, forwarding F10 to pane");
            muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "off"]);
            if let Some(p) = muxmesh_core::poll::tmux_out(
                &base,
                &["display", "-p", "#{window_index}.#{pane_index}"],
            ) {
                let p = p.trim().to_string();
                if !p.is_empty() {
                    tracing::info!("bar-toggle: send-keys -t {p} F10 (into the ssh pane)");
                    muxmesh_core::poll::run_tmux(&base, &["send-keys", "-t", &p, "F10"]);
                }
            }
            muxmesh_core::poll::run_tmux(&base, &["refresh-client", "-S"]);
            muxmesh_core::poll::run_tmux(
                &base,
                &["set-window-option", "-t", &wid, "@muxmesh-bar-hidden", "on"],
            );
            println!("muxmesh: parent bar hidden, control passed to {h}");
            0
        }
        // PARENT side, second press: restore the local bar and tell the child
        // (via its agent) to hide its bar again — back to badge-only view.
        Some(h) => {
            tracing::info!(
                "bar-toggle: PARENT branch (2nd press) — restoring local bar, telling {h} to hide"
            );
            muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "on"]);
            muxmesh_core::poll::run_tmux(
                &base,
                &[
                    "set-window-option",
                    "-t",
                    &wid,
                    "@muxmesh-bar-hidden",
                    "off",
                ],
            );
            let port: u16 = std::env::var("MUXMESH_CMD_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(9998);
            let token = std::env::var("MUXMESH_CMD_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| muxmesh_core::read_token().as_str().to_string());
            let env = CommandEnvelope {
                v: 1,
                id: format!("c-{}", now_ts()),
                src: hostname_short(),
                cmd: Command::BarHide,
                ts: now_ts(),
            };
            match send_cmd(&h, port, &token, &env) {
                Ok(resp) if resp.starts_with("HTTP/1.1 200") => {
                    tracing::info!("bar-toggle: step 5 ok — {h}:{port} acked BarHide");
                    println!("muxmesh: parent bar shown, {h} bar hidden");
                    0
                }
                Ok(resp) => {
                    // child unreachable: parent bar is back either way, report softly
                    eprintln!(
                        "muxmesh-cli bar-toggle: {h} agent replied: {}",
                        resp.lines().next().unwrap_or("")
                    );
                    1
                }
                Err(e) => {
                    eprintln!("muxmesh-cli bar-toggle: {h}:{port}: {e}");
                    1
                }
            }
        }
        // CHILD side (or no armed ssh): toggle THIS server's bar locally —
        // pure tmux, no network. Covers case 1b/3 (F10 pressed inside the
        // nested remote tmux) and any local-only window.
        None => {
            let next = if hidden_now { "on" } else { "off" };
            tracing::info!(
                "bar-toggle: CHILD/local branch — no armed ssh here, toggling THIS server's bar -> status {next}"
            );
            muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", next]);
            muxmesh_core::poll::run_tmux(
                &base,
                &[
                    "set-window-option",
                    "-t",
                    &wid,
                    "@muxmesh-bar-hidden",
                    if hidden_now { "off" } else { "on" },
                ],
            );
            muxmesh_core::poll::run_tmux(&base, &["refresh-client", "-S"]);
            println!("muxmesh: local bar {next}");
            0
        }
    }
}

/// `muxmesh-cli send <host> <verb> [args...]`. Port/token come from shared config.
/// Returns a small exit code: 0 ok, 1 failure, 2 usage.
pub fn run_send(_cfg: &muxmesh_core::HubConfig, args: &[String]) -> u8 {
    if args.len() < 2 {
        eprintln!("usage: muxmesh-cli send <host> <NewWindow|NewSession|FocusWindow|AttachOrDetach|BarHide|BarShow> [args]");
        return 2;
    }
    let host = &args[0];
    // the network peer is a host/IP, not a tmux name -> valid_host (dots ok)
    if !muxmesh_core::valid_host(host) {
        eprintln!("muxmesh-cli send: invalid host (must match [A-Za-z0-9_.-])");
        return 2;
    }
    let cmd = match parse_command(&args[1..]) {
        Some(c) => c,
        None => {
            eprintln!("muxmesh-cli send: unknown command or missing args");
            return 2;
        }
    };
    if !cmd.args_valid() {
        eprintln!("muxmesh-cli send: invalid args (must match [A-Za-z0-9_-])");
        return 2;
    }

    // the agent cmd port is shared config; reuse agent.cmd_port default 9998.
    let port: u16 = std::env::var("MUXMESH_CMD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9998);
    let token = muxmesh_core::read_token().as_str().to_string();

    let env = CommandEnvelope {
        v: 1,
        id: format!("c-{}", now_ts()),
        src: hostname_short(),
        cmd,
        ts: now_ts(),
    };
    match send_cmd(host, port, &token, &env) {
        Ok(resp) => {
            println!("{resp}");
            if resp.starts_with("HTTP/1.1 200") {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("muxmesh-cli send: {e}");
            1
        }
    }
}

fn hostname_short() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "client".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> String {
        s.into()
    }

    #[test]
    fn parse_all_verbs() {
        assert!(matches!(
            parse_command(&[v("NewWindow"), v("main")]),
            Some(Command::NewWindow { .. })
        ));
        assert!(matches!(
            parse_command(&[v("NewSession"), v("build")]),
            Some(Command::NewSession { .. })
        ));
        assert!(matches!(
            parse_command(&[v("FocusWindow"), v("main"), v("2")]),
            Some(Command::FocusWindow { .. })
        ));
        assert!(matches!(
            parse_command(&[v("AttachOrDetach"), v("main")]),
            Some(Command::AttachOrDetach { .. })
        ));
        assert!(matches!(
            parse_command(&[v("BarHide")]),
            Some(Command::BarHide)
        ));
        assert!(matches!(
            parse_command(&[v("BarShow")]),
            Some(Command::BarShow)
        ));
    }

    #[test]
    fn unknown_and_missing_args_rejected() {
        assert!(parse_command(&[v("ExecShell"), v("rm")]).is_none());
        assert!(parse_command(&[v("NewWindow")]).is_none()); // missing session
        assert!(parse_command(&[]).is_none());
    }

    #[test]
    fn envelope_serializes_with_closed_tag() {
        let env = CommandEnvelope {
            v: 1,
            id: "c-1".into(),
            src: "client-a".into(),
            cmd: Command::NewWindow {
                session: "main".into(),
            },
            ts: 1,
        };
        let json = serde_json::to_vec(&env).unwrap();
        assert!(String::from_utf8_lossy(&json).contains("\"type\":\"NewWindow\""));
    }
}
