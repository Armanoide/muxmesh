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
    /// hand control TO the child (delegate, tmux-remote recipe): deafen this
    /// server's keyboard with the GLOBAL key-table, not a per-client move —
    /// `set-option -g prefix None` + `set-option -g key-table off` +
    /// `refresh-client -S`. key-table is server state: every client, now and
    /// later, is deaf except the F10 bound in table `off`. Sent BY the parent
    /// on press-1; mirrors bar.rs delegate().
    GiveControl,
    /// take control BACK (reclaim, tmux-remote recipe): unset the GLOBAL
    /// prefix and key-table so this machine's own keyboard works again —
    /// `set-option -gu prefix` + `set-option -gu key-table` +
    /// `refresh-client -S`. Sent BY the child to its parent on press-2;
    /// mirrors bar.rs reclaim(). Distinct from GiveControl — opposite effect.
    ResumeControl,
    /// agent -> hub ONLY: a state report pushed on the agent's own WS every
    /// interval (replaces the old HTTP POST /state). The hub merges it into
    /// state.json and never routes it to another agent, so a child can never
    /// forge its parent's bar. `state` = the HostState payload, self-named
    /// (its `host` field is the key — same identity rule as the WS handshake).
    /// Identity comes from the authenticated WS channel, never from a header.
    /// (No skip_deserializing: the hub must parse it inbound; the agent's
    /// dispatch ignores any ReportState a malicious hub might push onward.)
    ReportState { state: serde_json::Value },
}

/// Routing policy a receiver applies before executing a command. Declarative:
/// the rule lives on the Command (tmux_argv_meta), not scattered in `if`s at
/// each call site. Add a field here + set it per verb, and every receiver that
/// reads meta obeys. `ignore_by_child`/`ignore_by_parent` are role guards: a
/// child = a machine that has a live parent (parents_of non-empty); a parent =
/// one with a live outbound ssh to the sender (ssh_parent_hosts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandMeta {
    /// a child (has a parent) must not execute this — it is the parent's verb.
    pub ignore_by_child: bool,
    /// a parent (no parent of its own) must not execute this — the child's verb.
    pub ignore_by_parent: bool,
}

impl CommandMeta {
    /// No guard: any authenticated receiver may execute.
    pub const DEFAULT: CommandMeta = CommandMeta {
        ignore_by_child: false,
        ignore_by_parent: false,
    };
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
    /// destination machine (hub routes by this; the agent double-checks it).
    /// Required on /cmd: hub 400s without it. `#[serde(default)]` keeps old
    /// serialized payloads deserializable (v:1 kept).
    #[serde(default)]
    pub dst: String,
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
/// Whitelist for tmux target names (sessions, windows): [A-Za-z0-9_-].
/// tmux itself allows ':' in window names (e.g. the default "shell: 2 windows"
/// style or user names like "foo:build"); ':' is NOT a valid target separator
/// in our poller/agent `-t` paths, so such names are rejected and must be
/// renamed by the operator — the hub logs the exact offending value.
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
            // control hand-back: global prefix reset, no name to validate.
            Command::GiveControl | Command::ResumeControl => vec![],
            // agent->hub state report: no tmux names, nothing to validate.
            Command::ReportState { .. } => vec![],
        }
    }

    /// Declarative routing meta for a command — the *policy* the receiver
    /// applies before executing. Kept separate from tmux_argv (the *action*):
    /// argv says what tmux runs, meta says who may run it and when. Add a
    /// rule here (not an `if` in execute_and_ack) and the receiver obeys.
    pub fn tmux_argv_meta(&self) -> CommandMeta {
        match self {
            // delegate: only a parent cuts its prefix. A child (a machine that
            // has a parent) receiving GiveControl is misaddressed or forged ->
            // the receiver ignores it. Bar/control verbs carry no tmux names.
            Command::GiveControl => CommandMeta {
                ignore_by_child: true,
                ..CommandMeta::DEFAULT
            },
            // reclaim: only a child hands control back to its parent. A parent
            // receiving ResumeControl (no parent of its own) is misaddressed.
            Command::ResumeControl => CommandMeta {
                ignore_by_parent: true,
                ..CommandMeta::DEFAULT
            },
            _ => CommandMeta::DEFAULT,
        }
    }

    /// Are all args of the command valid?
    pub fn args_valid(&self) -> bool {
        self.names().iter().all(|n| valid_name(n))
    }

    /// A state report pushed by an agent on its own WS. The hub merges it into
    /// state.json (same merge path as the legacy POST /state) and never routes it onward.
    pub fn is_report_state(&self) -> bool {
        matches!(self, Command::ReportState { .. })
    }

    /// Full tmux argv for the closed list (never shell-interpolated).
    /// Returns `tmux <argv...>` arguments, scoped to `session`. Commands that
    /// need several tmux invocations (AttachOrDetach, GiveControl,
    /// ResumeControl) return their FIRST step here; execute() runs the rest.
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
            // delegate: cut the prefix (deaf keyboard); execute() then parks
            // the client in table `off`. First step = the prefix, like BarHide.
            Command::GiveControl => {
                vec!["set-option".into(), "-g".into(), "prefix".into(), "None".into()]
            }
            // reclaim: restore the default prefix; execute() then returns the
            // client to table `root`. First step = the prefix, like BarShow.
            Command::ResumeControl => {
                vec!["set-option".into(), "-gu".into(), "prefix".into()]
            }
            // the child's press-2 answer: the parent's bar must come back with
            // its control. BarShow alone would restore the prefix but leave a
            // hidden bar (the hub's router never re-sends BarShow).
            Command::BarShow => {
                vec!["set".into(), "-g".into(), "status".into(), "on".into()]
            }
            Command::ReportState { .. } => {
                // never executed on tmux: the hub merges it into state.json and
                // does not route it to agents. An agent that receives one (a
                // malicious hub) ignores it — see execute_and_ack.
                vec![]
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
            Command::GiveControl | Command::ResumeControl => String::new(),
            Command::ReportState { .. } => String::new(),
        }
    }

    /// Execute this command against a local tmux socket, strictly via argv.
    /// Every call is audited to stderr (ts, verb, target, ok) — SPEC §7.
    /// Stateful verbs run their steps in order (tmux: one command each):
    /// AttachOrDetach (list-clients -> attach|detach), GiveControl (prefix
    /// None -> table off), ResumeControl (unset prefix -> table root).
    pub fn execute(&self, socket: &str) -> Result<(), String> {
        use std::process::Command as Proc;

        // a state report is the hub's to merge, never tmux's to run.
        if matches!(self, Command::ReportState { .. }) {
            return Ok(());
        }

        // ordered steps: the FIRST is tmux_argv() (the audited verb), the
        // rest are follow-up commands tmux needs in a separate invocation.
        let steps: Vec<Vec<String>> = match self {
            Command::AttachOrDetach { .. } => {
                let target = self.target();
                let attached = Proc::new("tmux")
                    .args(["-S", socket, "list-clients", "-t", &target])
                    .output()
                    .map(|o| o.status.success() && !o.stdout.is_empty())
                    .unwrap_or(false);
                if attached {
                    vec![vec!["detach-client".into(), "-t".into(), target]]
                } else {
                    vec![vec!["attach-session".into(), "-d".into(), "-t".into(), target]]
                }
            }
            Command::GiveControl => vec![
                self.tmux_argv(),
                vec!["set-option".into(), "-g".into(), "key-table".into(), "off".into()],
                // CRIT1 one-bar: the child's bar lights up exactly when the
                // parent goes dark (delegate sends status off). Visibility is
                // owned here; the badge TEXT belongs to the poller.
                vec!["set-option".into(), "-g".into(), "status".into(), "on".into()],
                vec!["refresh-client".into(), "-S".into()],
            ],
            // reclaim: restore prefix AND the global key-table; execute() and
            // execute_steps_for_test must stay byte-identical (the pinned test
            // guards the recipe tmux-remote established).
            Command::ResumeControl => vec![
                self.tmux_argv(),
                vec!["set-option".into(), "-gu".into(), "key-table".into()],
                // the bar returns WITH control: a reclaimed parent that stays
                // dark is blind (the live e2e regression). Visibility belongs
                // here; the badge TEXT belongs to the poller (single writer).
                vec!["set-option".into(), "-g".into(), "status".into(), "on".into()],
                vec!["refresh-client".into(), "-S".into()],
            ],
            _ => vec![self.tmux_argv()],
        };

        self.run_steps(socket, &steps)
    }

    /// Ordered tmux steps for execute() — exposed to tests so the multi-step
    /// verbs (ResumeControl's bar restore) stay pinned without a live socket.
    pub fn execute_steps_for_test(&self) -> Vec<Vec<String>> {
        match self {
            Command::GiveControl => vec![
                self.tmux_argv(),
                vec!["set-option".into(), "-g".into(), "key-table".into(), "off".into()],
                // CRIT1 one-bar: the child's bar lights up exactly when the
                // parent goes dark (delegate sends status off). Visibility is
                // owned here; the badge TEXT belongs to the poller.
                vec!["set-option".into(), "-g".into(), "status".into(), "on".into()],
                vec!["refresh-client".into(), "-S".into()],
            ],
            Command::ResumeControl => vec![
                self.tmux_argv(),
                vec!["set-option".into(), "-gu".into(), "key-table".into()],
                vec!["set-option".into(), "-g".into(), "status".into(), "on".into()],
                vec!["refresh-client".into(), "-S".into()],
            ],
            _ => vec![self.tmux_argv()],
        }
    }

    /// Run ordered tmux steps (one command per invocation), auditing each.
    /// Fails if any step fails; the audit names the offending verb+args.
    fn run_steps(&self, socket: &str, steps: &[Vec<String>]) -> Result<(), String> {
        use std::process::Command as Proc;
        let target = self.target();
        let mut all_ok = true;
        for argv in steps {
            let verb = argv[0].clone();
            let out = Proc::new("tmux")
                .arg("-S")
                .arg(socket)
                .args(argv)
                .output()
                .map_err(|e| format!("spawn tmux {verb}: {e}"))?;
            let ok = out.status.success();
            // audit line: no secrets, only whitelisted names (already validated).
            eprintln!(
                "muxmesh-agent: exec ts={} verb={verb} target={target} ok={ok}",
                now_unix()
            );
            if !ok {
                all_ok = false;
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if !err.is_empty() {
                    eprintln!("muxmesh-agent: step {verb} stderr: {err}");
                }
            }
        }
        if all_ok {
            Ok(())
        } else {
            Err("tmux step failed (see audit)".into())
        }
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
            dst: "host-b".into(),
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
    fn dst_roundtrip_and_default_empty() {
        // driver -> hub envelope carries the destination machine
        let env = CommandEnvelope {
            v: 1,
            id: "c-1".into(),
            src: "client-a".into(),
            dst: "host-b".into(),
            cmd: Command::BarHide,
            ts: 1,
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"dst\":\"host-b\""));
        let back: CommandEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
        // an old client that omits dst still deserializes (empty = direct)
        let legacy = r#"{"v":1,"id":"c","src":"x","cmd":{"type":"BarHide"},"ts":1}"#;
        let env: CommandEnvelope = serde_json::from_str(legacy).unwrap();
        assert_eq!(env.dst, "");
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
            (r#"{"type":"GiveControl"}"#, Command::GiveControl),
            (r#"{"type":"ResumeControl"}"#, Command::ResumeControl),
        ] {
            let full = format!(r#"{{"v":1,"id":"c","src":"x","cmd":{json},"ts":1}}"#);
            let env: CommandEnvelope = serde_json::from_str(&full).unwrap();
            assert_eq!(env.cmd, cmd);
            assert!(env.cmd.args_valid()); // no names to validate
        }
        assert_eq!(Command::BarHide.tmux_argv(), ["set", "-g", "status", "off"]);
        assert_eq!(Command::BarShow.tmux_argv(), ["set", "-g", "status", "on"]);
        // control verbs are multi-step: tmux_argv is the FIRST step (prefix),
        // execute() adds key-table off + refresh. Delegate cuts prefix,
        // reclaim restores it (global state, tmux-remote recipe).
        assert_eq!(Command::GiveControl.tmux_argv(), ["set-option", "-g", "prefix", "None"]);
        assert_eq!(Command::ResumeControl.tmux_argv(), ["set-option", "-gu", "prefix"]);
    }

    #[test]
    fn resume_control_steps_restore_prefix_table_and_bar_globally() {
        // press-2 at the child: tmux-remote recipe — unset prefix AND the
        // GLOBAL key-table, bar back on, repaint. switch-client is per-client
        // and was the rule violation (new/headless clients heard the prefix).
        let steps = Command::ResumeControl.execute_steps_for_test();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0], ["set-option", "-gu", "prefix"]);
        assert_eq!(steps[1], ["set-option", "-gu", "key-table"]);
        assert_eq!(steps[2], ["set-option", "-g", "status", "on"]);
        assert_eq!(steps[3], ["refresh-client", "-S"]);
    }

    #[test]
    fn give_control_steps_deafen_prefix_and_key_table_globally() {
        // press-1 at the parent: prefix None + key-table off (server-global)
        // + status on (the child's bar lights up as the parent goes dark —
        // CRIT1 one-bar) + repaint. F10 stays bound in table `off`.
        let steps = Command::GiveControl.execute_steps_for_test();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0], ["set-option", "-g", "prefix", "None"]);
        assert_eq!(steps[1], ["set-option", "-g", "key-table", "off"]);
        assert_eq!(steps[2], ["set-option", "-g", "status", "on"]);
        assert_eq!(steps[3], ["refresh-client", "-S"]);
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
