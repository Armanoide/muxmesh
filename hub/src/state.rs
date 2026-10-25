use std::collections::HashMap;
use std::sync::Arc;

use muxmesh_core::{mint_token, Fingerprint, HubKeypair, TokenHash};
use tokio::sync::Mutex;
/// Per-agent record pinned in the hub directory.
#[derive(Debug)]
pub struct Agent {
    pub label: String,
    pub fingerprint: Fingerprint,
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
}

pub type SharedState = Arc<HubState>;
