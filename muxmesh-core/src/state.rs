//! State pushed by an agent to the hub.
//!
//! Each machine's agent reads its local tmux and POSTs this payload to the hub.
//! The hub writes it atomically to state.json. A local poller cross-checks:
//! the focused window holds `ssh <host>` (pgrep) + a fresh entry here = arm.
//!
//! Zero network on the read path: the status bar reads a file, 0 ms.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One tmux session on a remote machine, as seen by its local agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSession {
    pub name: String,
    /// number of windows in the session
    pub windows: u32,
    /// name of the active window (empty if unavailable)
    #[serde(default)]
    pub active_window: String,
}

/// One machine (pushed by its agent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostState {
    /// canonical machine name (must match the ssh alias used to reach it)
    pub host: String,
    /// a tmux server is running on this machine
    pub armed: bool,
    /// unix timestamp (seconds) of emission by the agent
    pub seen: u64,
    /// host keys that currently have a live `ssh` INTO this machine. The
    /// child fills this from the hub directory (`parents_of`): every other
    /// registered host whose live ssh targets include us. Empty = no known
    /// parent (local-only machine, or ssh from an unregistered address).
    #[serde(default)]
    pub parents: Vec<String>,
    /// local tmux sessions (virtual tree). Empty if no tmux.
    #[serde(default)]
    pub sessions: Vec<RemoteSession>,
}

impl HostState {
    /// Is this entry still trustworthy? (fresh = heartbeat received < ttl ago).
    /// Beyond that the machine is considered offline (powered off, agent killed, net down).
    pub fn is_fresh(&self, now: u64, ttl: u64) -> bool {
        self.armed && now.saturating_sub(self.seen) <= ttl
    }
}

/// The full state document on the hub: host -> state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDoc {
    /// ordered map (BTreeMap = stable output, readable diffs)
    #[serde(default)]
    pub hosts: BTreeMap<String, HostState>,
}

impl StateDoc {
    /// Host armed AND fresh? That is the only question the poller asks.
    pub fn host_armed(&self, host: &str, now: u64, ttl: u64) -> bool {
        self.hosts.get(host).is_some_and(|h| h.is_fresh(now, ttl))
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Unix timestamp in seconds (saturating at 0).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(host: &str, armed: bool, seen: u64) -> HostState {
        HostState {
            host: host.into(),
            armed,
            seen,
            sessions: vec![],
            parents: vec![],
        }
    }

    #[test]
    fn freshness_window() {
        let s = state("a", true, 100);
        assert!(s.is_fresh(130, 30)); // exactly at ttl => still fresh
        assert!(!s.is_fresh(131, 30)); // past ttl => stale
        assert!(!state("a", false, 100).is_fresh(100, 30)); // disarmed never fresh
    }

    #[test]
    fn host_armed_lookup() {
        let mut doc = StateDoc::default();
        doc.hosts.insert("a".into(), state("a", true, 100));
        doc.hosts.insert("b".into(), state("b", false, 100));
        assert!(doc.host_armed("a", 100, 30));
        assert!(!doc.host_armed("b", 100, 30));
        assert!(!doc.host_armed("missing", 100, 30));
    }

    #[test]
    fn json_roundtrip_stable_order() {
        let mut doc = StateDoc::default();
        doc.hosts.insert("z".into(), state("z", true, 1));
        doc.hosts.insert("a".into(), state("a", true, 2));
        let json = doc.to_json_pretty().unwrap();
        assert!(json.find("\"a\"").unwrap() < json.find("\"z\"").unwrap());
        assert_eq!(StateDoc::from_json_str(&json).unwrap(), doc);
    }

    #[test]
    fn missing_fields_default() {
        let s: HostState = serde_json::from_str(r#"{"host":"a","armed":true,"seen":5}"#).unwrap();
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn now_is_recent() {
        assert!(now_unix() > 1_700_000_000);
    }
}
