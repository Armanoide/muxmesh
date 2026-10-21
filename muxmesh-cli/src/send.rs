//! Send closed-list commands to a remote agent VIA THE HUB (`POST /cmd`).
//!
//!   muxmesh-cli send <host> <command> [args...]
//!     send host-a NewWindow main
//!     send host-a NewSession build
//!     send host-a FocusWindow main 2
//!     send host-a AttachOrDetach main
//!
//! Operator rule: the hub NEVER dials an agent. Each agent holds a persistent
//! OUTBOUND WebSocket (/ws) to the hub; the CLI POSTs /cmd to the hub
//! (poll.hub), the envelope carries `dst: <host>`; the hub authenticates the
//! driver, checks dst is registered + WS-connected right now (else 409),
//! enqueues the frame on that agent's channel and answers 202 queued. The
//! driver authenticates with its own token (~/.config/muxmesh/token,
//! MUXMESH_CMD_TOKEN overrides). std::net only.

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

/// POST /cmd to host:port (the hub; it queues on the target's WS channel).
/// Returns the raw response body. HTTP/1.1 + Connection: close; std only.
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
    // headers flushed first, body in its own segment: the hub reads the head
    // byte-at-a-time, so this ordering is belt-and-braces, not required.
    let req = format!(
        "POST /cmd HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nX-Muxmesh-Host: {}\r\nX-Muxmesh-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        env.src,
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
    //   this server is the PARENT and OWNS BOTH BARS via HTTP: press 1
    //   hides local + shows remote (child's bar on screen), press 2 shows
    //   local + hides remote. No send-keys echo: a forwarded F10 would be
    //   handled by the child's own bar-toggle with ITS hidden-state, which
    //   desyncs from the parent's (observed: 2nd press re-hid the child).
    // - F10 pressed inside the nested remote tmux traverses ssh and lands
    //   on the CHILD's server, where armed_host_of_current_window finds no
    //   outbound ssh -> local branch below (child-only toggle).
    match host {
        // PARENT side: driver presses F10 with an armed ssh pane focused.
        Some(h) => {
            // commands flow driver -> HUB -> agent's WS (never direct to the
            // host): POST to poll.hub, dst in the envelope, hub queues + pushes.
            let (hub_host, hub_port) = split_host_port(&cfg.poll_hub);
            let port: u16 = std::env::var("MUXMESH_CMD_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(hub_port);
            let token = std::env::var("MUXMESH_CMD_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| muxmesh_core::read_token().as_str().to_string());
            let post = |verb: Command, label: &str| -> bool {
                let env = CommandEnvelope {
                    v: 1,
                    id: format!("c-{}", now_ts()),
                    src: hostname_short(),
                    dst: h.clone(),
                    cmd: verb,
                    ts: now_ts(),
                };
                match send_cmd(hub_host, port, &token, &env) {
                    Ok(resp) if resp.starts_with("HTTP/1.1 202") => {
                        tracing::info!("bar-toggle: hub {hub_host}:{port} queued {label} for {h} (pushed on its /ws channel)");
                        true
                    }
                    other => {
                        tracing::error!(
                            "bar-toggle: STOP at step 5 — hub {hub_host}:{port} {label} for {h}: {:?}",
                            other.map(|r| r.lines().next().unwrap_or("").to_string())
                        );
                        false
                    }
                }
            };
            if !hidden_now {
                // press 1: parent yields the screen — hide local, show remote.
                tracing::info!("bar-toggle: PARENT press 1 — hide local, show remote {h}");
                muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "off"]);
                muxmesh_core::poll::run_tmux(
                    &base,
                    &["set-window-option", "-t", &wid, "@muxmesh-bar-hidden", "on"],
                );
                muxmesh_core::poll::run_tmux(&base, &["refresh-client", "-S"]);
                let ok = post(Command::BarShow, "BarShow");
                println!(
                    "muxmesh: parent bar hidden{} — remote {h} bar shown (via hub)",
                    if ok { "" } else { " (hub push failed)" }
                );
                if ok {
                    0
                } else {
                    1
                }
            } else {
                // press 2: parent takes the screen back — show local, hide remote.
                tracing::info!("bar-toggle: PARENT press 2 — show local, hide remote {h}");
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
                muxmesh_core::poll::run_tmux(&base, &["refresh-client", "-S"]);
                let ok = post(Command::BarHide, "BarHide");
                println!(
                    "muxmesh: parent bar shown{} — remote {h} bar hidden (via hub)",
                    if ok { "" } else { " (hub push failed)" }
                );
                if ok {
                    0
                } else {
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

/// `muxmesh-cli send <host> <verb> [args...]` — POSTed to the HUB, which
/// enqueues it on <host>'s persistent WS channel (202 queued). Returns a
/// small exit code: 0 ok, 1 failure, 2 usage.
pub fn run_send(cfg: &muxmesh_core::HubConfig, args: &[String]) -> u8 {
    if args.len() < 2 {
        eprintln!("usage: muxmesh-cli send <host> <NewWindow|NewSession|FocusWindow|AttachOrDetach|BarHide|BarShow> [args]");
        return 2;
    }
    let host = &args[0];
    // the destination is a hub host key, not a tmux name -> valid_host (dots ok)
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

    // driver -> hub -> agent: POST to poll.hub, dst in the envelope.
    let (hub_host, hub_port) = split_host_port(&cfg.poll_hub);
    let port: u16 = std::env::var("MUXMESH_CMD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(hub_port);
    let token = std::env::var("MUXMESH_CMD_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| muxmesh_core::read_token().as_str().to_string());

    let env = CommandEnvelope {
        v: 1,
        id: format!("c-{}", now_ts()),
        src: hostname_short(),
        dst: host.clone(),
        cmd,
        ts: now_ts(),
    };
    match send_cmd(hub_host, port, &token, &env) {
        Ok(resp) => {
            println!("{resp}");
            if resp.starts_with("HTTP/1.1 202") {
                println!("muxmesh: queued via hub (pushed on {host}'s channel)");
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("muxmesh-cli send: hub {hub_host}:{port}: {e}");
            1
        }
    }
}

/// Split "host:port" / "[v6]:port" into (host, port-or-default-9997).
fn split_host_port(addr: &str) -> (&str, u16) {
    let a = addr.trim();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let p = tail
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(9997);
            return (h, p);
        }
        return (a, 9997);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse().unwrap_or(9997)),
        _ => (a, 9997),
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
            dst: "host-a".into(),
            cmd: Command::NewWindow {
                session: "main".into(),
            },
            ts: 1,
        };
        let json = serde_json::to_vec(&env).unwrap();
        let text = String::from_utf8_lossy(&json);
        assert!(text.contains("\"type\":\"NewWindow\""));
        // the hub needs dst to route the frame
        assert!(text.contains("\"dst\":\"host-a\""));
    }

    #[test]
    fn split_host_port_handles_v4_v6_bare() {
        assert_eq!(split_host_port("10.0.10.3:9997"), ("10.0.10.3", 9997));
        assert_eq!(split_host_port("10.0.10.3"), ("10.0.10.3", 9997));
        assert_eq!(split_host_port("[::1]:9998"), ("::1", 9998));
        assert_eq!(split_host_port("[::1]"), ("::1", 9997));
    }
}
