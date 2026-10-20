//! `muxmesh-cli start` — supervisor for the driver machine.
//!
//! Spawns and supervises `muxmesh-cli poll` (the badge loop) as a child
//! process, so one command = the whole driver side running. If the poller
//! dies it is restarted after a short backoff. Ctrl-C stops both.
//!
//! Why not just run `poll` directly: you SHOULD (launchd does exactly that).
//! `start` exists for the "one command, nothing to remember" UX and as the
//! place where future driver supervision (hub-watch, reconnect) will live.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

/// Locate our own binary so we can spawn `poll` as a sibling.
fn self_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "muxmesh-cli".into())
}

pub fn run_start() -> u8 {
    eprintln!("muxmesh-cli start: supervising poll (Ctrl-C to stop)");
    let mut child: Option<Child> = None;
    loop {
        // reap the previous child if it exited
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("muxmesh-cli start: poll exited ({status}) — restarting in 3s")
                }
                Ok(None) => {
                    // still alive
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
                Err(e) => {
                    eprintln!("muxmesh-cli start: wait: {e}");
                    return 1;
                }
            }
        }
        child = match Command::new(self_path())
            .arg("poll")
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(c) => Some(c),
            Err(e) => {
                let _ = std::io::stderr().flush();
                eprintln!("muxmesh-cli start: spawn poll: {e}");
                return 1;
            }
        };
    }
}
