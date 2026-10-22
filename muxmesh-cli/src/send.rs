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
        // child -> parent answer on press 2; not meant for manual `send` but
        // valid there too (drives the same reclaim).
        "ParentReclaim" => Some(Command::ParentReclaim),
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
    // headers flushed first, body in its own segment (harmless with axum, kept).
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

/// The exact run-shell command F10 must carry, for re-binding it after a
/// delegation (press 2 / rollback). Same shape as muxmesh.tmux's bind: the
/// log redirect keeps tmux's status line clean and leaves a full trace.
fn f10_cmd() -> String {
    let cli = if std::path::Path::new("/usr/local/bin/muxmesh-cli").is_file() {
        "/usr/local/bin/muxmesh-cli".to_string()
    } else if let Ok(dir) = std::env::var("MUXMESH_BIN_DIR") {
        format!("{dir}/muxmesh-cli")
    } else {
        "muxmesh-cli".to_string()
    };
    format!("{cli} bar-toggle -v >>$HOME/.muxmesh/bar-toggle.log 2>&1")
}

/// `muxmesh-cli bar-toggle` — the whole F10 logic, no if-shell in tmux.
/// Finds the armed host of the current window (live detection), reads the
/// bar's hidden state from tmux itself (`show-options -g status` — tmux is
/// the source of truth, no local flag), and POSTs BarHide or BarShow.
pub fn run_bar_toggle(cfg: &muxmesh_core::HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = muxmesh_core::poll::tmux_base(&socket);
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
    let wid = match muxmesh_core::poll::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(o) => o.trim().to_string(),
        None => {
            tracing::error!("bar-toggle: STOP at step 1 — no tmux server / no current window");
            return 1;
        }
    };

    // hidden-state of THIS server's bar: tmux itself knows. `status` is a
    // server-global option; after `set -g status off`, show-options answers
    // "status off" (verified on tmux 3.3a). No local flag to maintain — the
    // old @muxmesh-bar-hidden window option was fragile (scope) and redundant.
    let hidden_now = muxmesh_core::poll::tmux_out(&base, &["show-options", "-g", "status"])
        .map(|o| o.trim().ends_with("off"))
        .unwrap_or(false);
    tracing::debug!("bar-toggle: step 1 ok — window {wid}, bar-hidden={hidden_now}");

    // Delegated state of the PARENT (only meaningful when a host is armed):
    // `prefix None` = control delegated to the child (press 1 cut it, tmux-
    // remote's mechanism); anything else (C-b, C-a…) = parent holds control.
    // A CHILD server may legitimately run prefix None in its own config, so
    // this is never consulted on the child branch (host=None). Exact match so
    // a custom prefix is never mistaken for delegated (ECC P2). 3.3a verified.
    let delegated = muxmesh_core::poll::tmux_out(&base, &["show-options", "-gv", "prefix"])
        .map(|o| o.trim() == "None")
        .unwrap_or(false);

    let host = armed_host_of_current_window(cfg);
    tracing::info!(
        "bar-toggle: step 2 — armed host = {:?}, delegated={delegated}",
        host
    );

    // Parent/child roles are decided by the KEYSTROKE's server, not by us:
    // - F10 lands on the driver's server while an armed ssh has focus ->
    //   this server is the PARENT: press 1 DELEGATES (BarShow remote, hide
    //   local bar, prefix None + unbind F10 — tmux-remote's exact mechanism).
    //   While delegated the parent is DEAF to F10: press 2 traverses ssh and
    //   fires on the CHILD (local branch below), which answers BarHide over
    //   the hub WS; the parent's poller applies it and reclaims control.
    // - F10 pressed inside the nested remote tmux traverses ssh and lands
    //   on the CHILD's server, where armed_host_of_current_window finds no
    //   outbound ssh -> local branch below (child-only toggle / press 2).
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
                    src: driver_host(),
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

            if !delegated {
                // press 1: delegate control to the child. GUARD — never hand over
                // the keyboard blindly: only cut the prefix if BarShow is ACKed by
                // the hub (child reachable). If the child is down we keep control.
                let ok = post(Command::BarShow, "BarShow");
                if !ok {
                    tracing::error!("bar-toggle: STOP — child unreachable, NOT delegating (parent keeps prefix+bar)");
                    println!("muxmesh: remote {h} unreachable — control NOT delegated (parent keeps the keyboard)");
                    return 1;
                }
                // child is live: hide local bar, then hand the keyboard over —
                // tmux-remote's exact mechanism (toggle_on.sh): prefix None +
                // switch-client -T off. The F10 root binding STAYS: while the
                // client sits in table `off`, keystrokes route to it, so a press
                // that comes back to us (ssh exited, or the child is deaf) still
                // reaches bar-toggle — the reclaim branch below. No unbind: an
                // unbound parent can only be reclaimed over the network, and if
                // the child lacks muxmesh the user is stranded forever (observed).
                muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "off"]);
                let cut =
                    muxmesh_core::poll::tmux_run(&base, &["set-option", "-g", "prefix", "None"]);
                let slept = muxmesh_core::poll::tmux_run(&base, &["switch-client", "-T", "off"]);
                // ECC P0: never claim DELEGATED without re-reading tmux — a
                // silently failed prefix cut would leave control undelegated.
                let now = muxmesh_core::poll::tmux_out(&base, &["show-options", "-gv", "prefix"])
                    .map(|o| o.trim() == "None")
                    .unwrap_or(false);
                if !cut || !now {
                    // rollback: the keyboard never left the parent's hands.
                    muxmesh_core::poll::tmux_run(&base, &["set-option", "-gu", "prefix"]);
                    muxmesh_core::poll::run_tmux(&base, &["switch-client", "-T", "root"]);
                    muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "on"]);
                    tracing::error!(
                        "bar-toggle: prefix move FAILED — rolled back, parent keeps control"
                    );
                    println!("muxmesh: delegation FAILED (prefix cut refused) — parent keeps the keyboard");
                    return 1;
                }
                tracing::info!(
                    "bar-toggle: DELEGATED ts={t0} — prefix None, client table off (F10 binding kept for local reclaim), sleep={slept}"
                );
                println!("muxmesh: control DELEGATED to {h} — press F10 again to reclaim");
                0
            } else {
                // press 2 RECLAIM, local (tmux-remote's toggle_off.sh): F10
                // landed back on the PARENT — either the ssh was exited or the
                // child has no F10 binding. Reclaim without the network: prefix
                // unset + client back in root table + bar on. The keystroke
                // reached us through the off-table F10 binding, so recovery never
                // depends on the child or the hub. Double-fire is harmless: the
                // second pass sees prefix restored and toggles the bar only.
                muxmesh_core::poll::run_tmux(&base, &["set-option", "-gu", "prefix"]);
                muxmesh_core::poll::run_tmux(&base, &["switch-client", "-T", "root"]);
                muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", "on"]);
                let back = muxmesh_core::poll::tmux_out(&base, &["show-options", "-gv", "prefix"])
                    .map(|o| o.trim() != "None")
                    .unwrap_or(false);
                tracing::info!(
                    "bar-toggle: RECLAIMED local ts={t0} — prefix unset confirmed={back}, table root, bar on"
                );
                println!("muxmesh: control RECLAIMED from {h} (local)");
                0
            }
        }
        // CHILD side (or no armed ssh): toggle THIS server's bar locally. Two
        // real paths:
        // 1. F10 pressed inside the nested remote tmux (user not delegating);
        // 2. PRESS 2 while DELEGATED (tmux-remote's mechanism): the parent cut
        //    its prefix and unbound F10, so the keystroke traverses ssh and
        //    fires HERE. We hide our bar AND answer BarHide back to the parent
        //    over the hub (/cmd, dst = the host that armed an ssh into us,
        //    from the hub's directory). The parent's agent applies it: prefix
        //    restored + F10 rebound + bar on -> control reclaimed without any
        //    keystroke reaching the deaf parent.
        None => {
            let next = if hidden_now { "on" } else { "off" };
            tracing::info!(
                "bar-toggle: CHILD/local branch ts={t0} — no armed ssh here, toggling THIS server's bar -> status {next}"
            );
            // tmux redraws the bar by itself after set status — no refresh-client.
            muxmesh_core::poll::run_tmux(&base, &["set-option", "-g", "status", next]);
            if next == "off" && !cfg.poll_hub.is_empty() {
                answer_parent_bar_hide(cfg);
            }
            println!("muxmesh: local bar {next}");
            0
        }
    }
}

/// Answer a press-2 back to the parent: find who armed an ssh INTO us (the
/// hub's directory knows every host's live ssh clients) and POST BarHide to
/// them via the hub. Best-effort: if no parent is found or the hub is down,
/// F10 on the parent still self-heals via press-2 reclaim — never block the
/// local bar toggle on the network.
fn answer_parent_bar_hide(cfg: &muxmesh_core::HubConfig) {
    let me = driver_host();
    let doc = muxmesh_core::poll::fetch_state(&cfg.poll_hub);
    let parent = match muxmesh_core::poll::parents_of(&doc, &me).into_iter().next() {
        Some(p) => p,
        None => {
            tracing::info!(
                "child: no host has a live ssh into '{me}' — local-only toggle, nothing to answer"
            );
            return;
        }
    };
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
        src: me.clone(),
        dst: parent.clone(),
        cmd: Command::ParentReclaim,
        ts: now_ts(),
    };
    match send_cmd(hub_host, port, &token, &env) {
        Ok(resp) if resp.starts_with("HTTP/1.1 202") => {
            tracing::info!(
                "child: answered press-2 -> ParentReclaim queued for parent '{parent}' via hub"
            );
        }
        other => {
            tracing::warn!(
                "child: answer to hub failed: {:?}",
                other.map(|r| r.lines().next().unwrap_or("").to_string())
            );
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
        src: driver_host(),
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

/// Identity for the hub: MUST equal the agent's canonical host name (the key
/// in tokens.json and the X-Muxmesh-Host the agent sends). `hostname -s` is
/// NOT that — macOS mDNS advertises "MacBook-Pro-de-Norbert.local", so -s
/// yields a name the hub never registered -> 401 on every POST /cmd.
/// Same resolution order as AgentConfig::host: env > config.toml agent.host
/// (load_map flattens [agent] to "agent.host") > hostname -s fallback.
fn driver_host() -> String {
    if let Ok(h) = std::env::var("MUXMESH_HOST") {
        let h = h.trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    let map = muxmesh_core::load_map();
    let h = map.get("agent.host").cloned().unwrap_or_default();
    if !h.is_empty() {
        return h;
    }
    hostname_short()
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
