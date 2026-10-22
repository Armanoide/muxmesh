//! state.json persistence + shared hub state (AppState).

pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::HeaderMap;
use muxmesh_core::TokenStore;
use tokio::sync::mpsc;

pub use store::{log_path_for, merge_state, purge_and_write, write_file_atomic};

// read_doc/write_atomic are used by main.rs's tests (state::read_doc) — the
// re-export is for that surface; the lib build itself never calls them.
#[allow(unused_imports)]
pub use store::{read_doc, write_atomic};

/// Extract the message from an agent log frame `{"log": "..."}`. None for
/// non-log frames (acks, junk) so the hub never appends garbage to hub.log.
pub fn agent_log_line(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("log")?.as_str().map(|s| s.to_string())
}

/// Append one agent log line to the central hub.log: ts + host + level tag
/// kept as-is from the agent's own formatting. Best effort, never fails the
/// WS session. shortcut: append per line — batch if log volume ever matters.
pub fn record_agent_log(state: &AppState, host: &str, line: &str) {
    let _g = state.write_lock.lock();
    use std::io::Write;
    if let Some(dir) = state.log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.log_path)
    {
        Ok(mut f) => {
            let ts = muxmesh_core::now_unix();
            if writeln!(f, "{ts} {host} {line}").is_err() {
                tracing::debug!("hub.log write: io error");
            }
        }
        Err(e) => tracing::warn!("hub.log open {}: {e}", state.log_path.display()),
    }
}

/// Mirror an agent ack into state.json's last_cmd (best effort — the driver
/// reads it back via GET /state). Never fails the WS session. Blocking file
/// IO on a tokio worker: state.json is a few KB, acceptable here.
pub fn record_ack(state: &AppState, host: &str, ack: &muxmesh_core::Ack) {
    record_ack_at(&state.state_path, host, ack)
}

use store::record_ack as record_ack_at;

/// Which agent holds the /agent/ws channel and where queued commands go.
pub struct Sender {
    pub tx: mpsc::UnboundedSender<String>,
    /// connection generation — a reconnect replaces the old sender (ECC P2).
    pub gen: u64,
}

/// Shared hub state: token registry + state path + live agent channels.
pub struct AppState {
    pub tokens: Mutex<TokenStore>,
    pub state_path: PathBuf,
    /// companion log for agent lines (hub.log next to state.json).
    pub log_path: PathBuf,
    /// serialize read-modify-write of state.json so concurrent POSTs cannot
    /// clobber each other's host entries.
    pub write_lock: Mutex<()>,
    /// live /agent/ws channels by host key; post_cmd enqueues here.
    pub channels: Mutex<HashMap<String, Sender>>,
}

impl AppState {
    /// Authenticate a request by TOKEN ALONE: the token is the machine's
    /// identity; X-Muxmesh-Host is a label the machine declares. Returns the
    /// DECLARED name when the token matches its registered record (rename =
    /// edit config.toml, no re-mint); the registered name when the machine
    /// sends no header. None = 401. KDF runs OUTSIDE the store lock.
    pub fn auth_host(&self, declared: &str, token: &str) -> Option<String> {
        if token.len() < muxmesh_core::PREFIX_LEN {
            return None;
        }
        let prefix: String = token.chars().take(muxmesh_core::PREFIX_LEN).collect();
        // snapshot the candidate under the lock, KDF outside it.
        let cand = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hosts
            .iter()
            .find(|(_, r)| r.prefix == prefix)
            .map(|(h, r)| (h.clone(), r.hash.clone()));
        match cand {
            Some((registered, hash)) => {
                if muxmesh_core::verify_token(token, &hash) {
                    let name = if declared.is_empty() {
                        registered
                    } else {
                        declared.to_string()
                    };
                    Some(name)
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Read a header value (names are case-insensitive; values keep their case).
    pub fn hdr(h: &HeaderMap, name: &str) -> String {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    /// Hosts currently holding a live /agent/ws channel (for logging/409 hints).
    #[allow(dead_code)]
    pub fn connected_hosts(&self) -> Vec<String> {
        self.channels
            .lock()
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    }
}
