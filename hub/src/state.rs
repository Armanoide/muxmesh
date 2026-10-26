use std::collections::HashMap;
use std::sync::Arc;

use muxmesh_core::{mint_token, BarStatus, Fingerprint, HubKeypair, ReportState, TokenHash};
use tokio::sync::Mutex;
/// Per-agent record pinned in the hub directory — the live card.
#[derive(Debug)]
pub struct Agent {
    pub label: String,
    pub fingerprint: Fingerprint,
    /// last REPORT_STATE seen on this agent's socket
    pub bar_on: bool,
    pub last_seen: u64,
}

/// The hub's in-memory directory: enrolled tokens and connected agents.
/// Arc so handlers share it; Mutex because enrollment (mint) happens while
/// the hub runs, not only at boot.
#[derive(Debug)]
pub struct HubState {
    /// token hash -> pinned fingerprint (None = enrolled, never connected yet)
    enrolled: Mutex<Vec<(TokenHash, Option<Fingerprint>)>>,
    /// label -> connected agent (the live directory)
    pub agents: Mutex<HashMap<String, Agent>>,
    /// the hub's permanent signing keypair (anti-MITM)
    pub keys: HubKeypair,
}

impl HubState {
    pub fn new() -> Self {
        HubState {
            enrolled: Mutex::new(Vec::new()),
            agents: Mutex::new(HashMap::new()),
            keys: HubKeypair::generate(),
        }
    }

    /// Mint a new machine token: returns the plaintext once (for enrollment),
    /// stores only its hash.
    pub async fn mint(&self, label: &str) -> String {
        let token = mint_token();
        self.enrolled.lock().await.push((TokenHash::of(&token), None));
        tracing::info!(label, "enrolled new machine token");
        token
    }

    /// Verify a presented token against the enrolled hashes.
    pub async fn verify_token(&self, token: &str) -> bool {
        self.enrolled.lock().await.iter().any(|(h, _)| h.matches(token))
    }

    /// Pin a fingerprint to its token on first connect; reject mismatch.
    pub async fn pin_fingerprint(&self, token: &str, fp: &Fingerprint) -> bool {
        let mut enrolled = self.enrolled.lock().await;
        for (h, pinned) in enrolled.iter_mut() {
            if h.matches(token) {
                match pinned {
                    None => {
                        *pinned = Some(fp.clone());
                        return true;
                    }
                    Some(p) => return p == fp,
                }
            }
        }
        false
    }

    /// Record a connected agent and refresh it with each REPORT_STATE heartbeat.
    pub async fn update_agent(&self, label: &str, fp: &Fingerprint, report: &ReportState) {
        self.agents.lock().await.insert(
            label.to_string(),
            Agent {
                label: label.to_string(),
                fingerprint: fp.clone(),
                bar_on: report.status == BarStatus::On,
                last_seen: report.seen,
            },
        );
    }

    /// Socket died — pull the agent off the live directory.
    pub async fn remove_agent(&self, label: &str) {
        self.agents.lock().await.remove(label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_lands_on_the_card_then_vanishes() {
        let state = HubState::new();
        let fp = Fingerprint::compute("machine-1");

        state
            .update_agent(
                "node-a",
                &fp,
                &ReportState {
                    seen: 100,
                    status: BarStatus::On,
                },
            )
            .await;
        {
            let agents = state.agents.lock().await;
            let a = agents.get("node-a").expect("agent on card");
            assert!(a.bar_on);
            assert_eq!(a.last_seen, 100);
        }

        state.remove_agent("node-a").await;
        assert!(state.agents.lock().await.is_empty());
    }
}

pub type SharedState = Arc<HubState>;
