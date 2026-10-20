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

use crate::poll;

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
/// window if a live `ssh <host>` sits in it AND <host> is armed+fresh in the
/// state doc. Exit 1 otherwise. This is what F8/F9/F10 call to pick the
/// target machine of `send`.
pub fn run_armed_host(cfg: &muxmesh_core::HubConfig) -> u8 {
    let socket = std::env::var("MUXMESH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let base = poll::tmux_base(&socket);
    let wid = match poll::tmux_out(&base, &["display", "-p", "#{window_id}"]) {
        Some(o) => o.trim().to_string(),
        None => return 1,
    };
    let doc = poll::read_state(&cfg.state_path);
    match poll::window_ssh_host(&base, &wid) {
        Some(h)
            if muxmesh_core::valid_host(&h)
                && doc.host_armed(&h, muxmesh_core::now_unix(), cfg.ttl) =>
        {
            println!("{h}");
            0
        }
        _ => 1,
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
