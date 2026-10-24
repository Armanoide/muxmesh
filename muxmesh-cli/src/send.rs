//! Closed-list command parsing + `muxmesh-cli send <host> <verb>` entry.
//!
//! The wire lives in transport.rs, the F10 state machine in bar.rs, tmux
//! reads in tmux_state.rs. This file owns only: CLI words -> Command, and the
//! plain `send` subcommand (driver -> hub -> agent).

use muxmesh_core::{Command, CommandEnvelope};

use crate::transport;

/// Build a Command from CLI words. Returns None on unknown verb / arity.
pub fn parse_command(words: &[String]) -> Option<Command> {
    let verb = words.first()?.as_str();
    match verb {
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
        "GiveControl" => Some(Command::GiveControl),
        "ResumeControl" => Some(Command::ResumeControl),
        _ => None,
    }
}

/// `muxmesh-cli send <host> <verb> [args...]` — POSTed to the HUB, which
/// enqueues it on <host>'s persistent WS channel (202 queued). Returns a
/// small exit code: 0 ok, 1 failure, 2 usage.
pub fn run_send(cfg: &muxmesh_core::HubConfig, args: &[String]) -> u8 {
    if args.len() < 2 {
        eprintln!("usage: muxmesh-cli send <host> <NewWindow|NewSession|FocusWindow|AttachOrDetach|BarHide|BarShow|GiveControl|ResumeControl> [args]");
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
    let (hub_host, hub_port) = transport::hub_endpoint(cfg);
    let env = CommandEnvelope {
        v: 1,
        id: format!("c-{}", transport::now_ts()),
        src: transport::driver_host(),
        dst: host.clone(),
        cmd,
        ts: transport::now_ts(),
    };
    match transport::send_cmd(&hub_host, hub_port, &transport::cmd_token(), &env) {
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
            eprintln!("muxmesh-cli send: hub {hub_host}:{hub_port}: {e}");
            1
        }
    }
}

/// `muxmesh-cli armed-host` — print the armed remote host of the current
/// window if a live `ssh <host>` sits in it AND <host> is armed+fresh on the
/// hub. Exit 1 otherwise. This is what F8/F9 call to pick the send target.
pub fn run_armed_host(cfg: &muxmesh_core::HubConfig) -> u8 {
    match crate::tmux_state::armed_host_of_current_window(cfg) {
        Some(h) => {
            println!("{h}");
            0
        }
        None => 1,
    }
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
        assert!(matches!(
            parse_command(&[v("GiveControl")]),
            Some(Command::GiveControl)
        ));
        assert!(matches!(
            parse_command(&[v("ResumeControl")]),
            Some(Command::ResumeControl)
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
        assert_eq!(transport::split_host_port("10.0.10.3:9997"), ("10.0.10.3", 9997));
        assert_eq!(transport::split_host_port("10.0.10.3"), ("10.0.10.3", 9997));
        assert_eq!(transport::split_host_port("[::1]:9998"), ("::1", 9998));
        assert_eq!(transport::split_host_port("[::1]"), ("::1", 9997));
    }
}
