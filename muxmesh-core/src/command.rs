//! Closed set of protocol commands (SPEC.md section 4).
//!
//! Style borrowed from lm-sanaga (`RunModelRequest`): an externally-tagged
//! serde enum means the closed list is ENCODED in the wire format. An unknown
//! verb or wrongly-typed args are rejected by the deserializer before any
//! execution — the closed list is not a convention, it is the type.
//!
//! JSON convention: `{"type": "NewWindow", "session": "main"}` (tag `type`, PascalCase).

use serde::{Deserialize, Serialize};

use crate::state::now_unix;

/// Allowed verbs. Nothing else deserializes -> 400 on arrival.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum Command {
    /// tmux new-window -t <session>
    NewWindow { session: String },
    /// tmux new-session -d -s <name>
    NewSession { name: String },
    /// tmux select-window -t <session>:<window>
    FocusWindow { session: String, window: String },
    /// tmux attach / detach-client depending on current state
    AttachOrDetach { session: String },
    /// hide the status bar on the target (one-bar protocol): `set -g status off`.
    /// No args: a global option, nothing to validate or target.
    BarHide,
    /// restore the status bar on the target: `set -g status on`.
    BarShow,
}

/// Envelope of a command sent by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    /// protocol version; v1 = 1. Unknown => reject.
    pub v: u8,
    /// opaque id chosen by the client; echoed back in the ack.
    pub id: String,
    /// canonical name of the sending client (e.g. "client-a").
    pub src: String,
    pub cmd: Command,
    /// unix timestamp (seconds) of emission.
    pub ts: u64,
}

/// Agent reply after executing (or rejecting) a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub v: u8,
    /// echoed command id.
    pub id: String,
    pub ok: bool,
    /// short error message when ok=false (never contains sensitive content).
    #[serde(default)]
    pub err: Option<String>,
}

impl Ack {
    pub fn ok(v: u8, id: &str) -> Self {
        Self {
            v,
            id: id.to_string(),
            ok: true,
            err: None,
        }
    }
    pub fn err(v: u8, id: &str, msg: &str) -> Self {
        Self {
            v,
            id: id.to_string(),
            ok: false,
            err: Some(msg.to_string()),
        }
    }
}

/// Arg validation (SPEC section 4): alphanumeric + `_-`, 1..=64, must start
/// alphanumeric. tmux is invoked via argv (never through a shell) -> no
/// injection, but we still reject anything outside the expected vocabulary
/// (paths, flags like `-x`).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Host identifiers (poller ssh targets, hub host keys): same whitelist as
/// valid_name PLUS '.' so dotted-quad IPs work as canonical host names
/// (e.g. "10.0.10.3"). Still no '/', spaces or flags -> tmux `-t` targets
/// built from these stay inert (a dot is not a valid session/window char).
pub fn valid_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

impl Command {
    /// tmux names carried by the command; all must pass `valid_name`.
    pub fn names(&self) -> Vec<&str> {
        match self {
            Command::NewWindow { session } => vec![session],
            Command::NewSession { name } => vec![name],
            Command::FocusWindow { session, window } => vec![session, window],
            Command::AttachOrDetach { session } => vec![session],
            Command::BarHide | Command::BarShow => vec![],
        }
    }

    /// Are all args of the command valid?
    pub fn args_valid(&self) -> bool {
        self.names().iter().all(|n| valid_name(n))
    }

    /// Full tmux argv for the closed list (never shell-interpolated).
    /// Returns `tmux <argv...>` arguments, scoped to `session`.
    pub fn tmux_argv(&self) -> Vec<String> {
        match self {
            Command::NewWindow { session } => {
                vec!["new-window".into(), "-t".into(), session.clone()]
            }
            Command::NewSession { name } => {
                vec!["new-session".into(), "-d".into(), "-s".into(), name.clone()]
            }
            Command::FocusWindow { session, window } => {
                vec![
                    "select-window".into(),
                    "-t".into(),
                    format!("{session}:{window}"),
                ]
            }
            Command::AttachOrDetach { session } => {
                // the agent chooses attach or detach based on current state; base argv:
                vec!["attach-session".into(), "-t".into(), session.clone()]
            }
            // global option; session is validated + audited but tmux takes no -t
            Command::BarHide => {
                vec!["set".into(), "-g".into(), "status".into(), "off".into()]
            }
            Command::BarShow => {
                vec!["set".into(), "-g".into(), "status".into(), "on".into()]
            }
        }
    }

    /// tmux target for `-t` (session, or session:window). Names must already
    /// pass `args_valid()`.
    pub fn target(&self) -> String {
        match self {
            Command::NewWindow { session } | Command::AttachOrDetach { session } => session.clone(),
            Command::NewSession { name } => name.clone(),
            Command::FocusWindow { session, window } => format!("{session}:{window}"),
            // global option, no -t target
            Command::BarHide | Command::BarShow => String::new(),
        }
    }

    /// Execute this command against a local tmux socket, strictly via argv.
    /// AttachOrDetach is stateful: attached client -> detach, else attach -d.
    /// Every call is audited to stderr (ts, verb, target, ok) — SPEC §7.
    pub fn execute(&self, socket: &str) -> Result<(), String> {
        use std::process::Command as Proc;

        let argv: Vec<String> = if matches!(self, Command::AttachOrDetach { .. }) {
            let target = self.target();
            let attached = Proc::new("tmux")
                .args(["-S", socket, "list-clients", "-t", &target])
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false);
            if attached {
                vec!["detach-client".into(), "-t".into(), target]
            } else {
                vec!["attach-session".into(), "-d".into(), "-t".into(), target]
            }
        } else {
            self.tmux_argv()
        };

        let verb = argv[0].clone();
        let out = Proc::new("tmux")
            .arg("-S")
            .arg(socket)
            .args(&argv)
            .output()
            .map_err(|e| format!("spawn tmux: {e}"))?;

        let ok = out.status.success();
        let target = self.target();
        // audit line: no secrets, only whitelisted names (already validated).
        eprintln!(
            "muxmesh-agent: exec ts={} verb={verb} target={target} ok={ok}",
            now_unix()
        );
        if !ok {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if err.is_empty() {
                "tmux failed".into()
            } else {
                err
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_list_roundtrip() {
        let env = CommandEnvelope {
            v: 1,
            id: "c-1".into(),
            src: "client-a".into(),
            cmd: Command::NewWindow {
                session: "main".into(),
            },
            ts: 1,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"NewWindow\""));
        let back: CommandEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn unknown_verb_is_rejected_by_deserializer() {
        let bad =
            r#"{"v":1,"id":"c","src":"x","cmd":{"type":"ExecShell","cmd":"rm -rf /"},"ts":1}"#;
        assert!(serde_json::from_str::<CommandEnvelope>(bad).is_err());
    }

    #[test]
    fn missing_field_is_rejected() {
        let bad = r#"{"v":1,"id":"c","cmd":{"type":"NewWindow"},"ts":1}"#; // no src, no session
        assert!(serde_json::from_str::<CommandEnvelope>(bad).is_err());
    }

    #[test]
    fn args_validation_blocks_injection_shapes() {
        assert!(valid_name("main"));
        assert!(valid_name("host_b-2"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name(";rm -rf /"));
        assert!(!valid_name("-x")); // arbitrary tmux flag rejected
        let c = Command::NewWindow {
            session: "main; touch /tmp/pwn".into(),
        };
        assert!(!c.args_valid());
    }

    #[test]
    fn bar_verbs_roundtrip_and_argv() {
        for (json, cmd) in [
            (r#"{"type":"BarHide"}"#, Command::BarHide),
            (r#"{"type":"BarShow"}"#, Command::BarShow),
        ] {
            let full = format!(r#"{{"v":1,"id":"c","src":"x","cmd":{json},"ts":1}}"#);
            let env: CommandEnvelope = serde_json::from_str(&full).unwrap();
            assert_eq!(env.cmd, cmd);
            assert!(env.cmd.args_valid()); // no names to validate
        }
        assert_eq!(Command::BarHide.tmux_argv(), ["set", "-g", "status", "off"]);
        assert_eq!(Command::BarShow.tmux_argv(), ["set", "-g", "status", "on"]);
    }

    #[test]
    fn valid_host_allows_dots_rejects_injection() {
        assert!(valid_host("10.0.10.3"));
        assert!(valid_host("host-a"));
        assert!(valid_host("mac_mini"));
        assert!(!valid_host("10.0.10.3; rm -rf /"));
        assert!(!valid_host("a/b"));
        assert!(!valid_host("-x"));
        assert!(!valid_host(""));
    }

    #[test]
    fn tmux_argv_is_exact_and_shell_free() {
        let c = Command::FocusWindow {
            session: "main".into(),
            window: "2".into(),
        };
        assert_eq!(c.tmux_argv(), ["select-window", "-t", "main:2"]);
    }

    #[test]
    fn ack_roundtrip() {
        let a = Ack::err(1, "c-1", "no such session");
        let json = serde_json::to_string(&a).unwrap();
        let back: Ack = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
