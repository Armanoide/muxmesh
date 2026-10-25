use crate::error::Result;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// The hub holds a permanent ED25519 keypair (generated at first start,
/// persisted in state/). Its public key is the "KEY" copied to agents at
/// install time. The hub proves its identity by signing a fresh challenge.
pub struct HubKeypair {
    signing: SigningKey,
}

impl HubKeypair {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
        HubKeypair {
            signing: SigningKey::from_bytes(&bytes),
        }
    }

    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        HubKeypair {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_hex(&self) -> String {
        self.public_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing.sign(msg)
    }
}

impl std::fmt::Debug for HubKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HubKeypair(pub: {}…)", &self.public_key_hex()[..12])
    }
}

/// Handshake challenge: the hub emits it right after the WS upgrade, the
/// agent verifies it. A fake hub (MITM) cannot sign a fresh nonce without
/// the private key.
pub struct Challenge(pub [u8; 32]);

impl Challenge {
    pub fn nonce() -> Self {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
        Challenge(bytes)
    }

    /// What the hub signs: nonce + agent fingerprint. Binding the challenge
    /// to the agent prevents replaying a signature made for another peer.
    pub fn sign_for(&self, key: &HubKeypair, agent_fp: &str) -> Result<Signature> {
        let mut msg = self.0.to_vec();
        msg.extend_from_slice(agent_fp.as_bytes());
        Ok(key.sign(&msg))
    }

    /// Agent side: verify the signature comes from the hub holding pub_key.
    pub fn verify_for(&self, pub_key: &VerifyingKey, agent_fp: &str, sig: &Signature) -> Result<()> {
        let mut msg = self.0.to_vec();
        msg.extend_from_slice(agent_fp.as_bytes());
        Ok(pub_key.verify(&msg, sig)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_verifies_genuine_hub_challenge() {
        let hub = HubKeypair::generate();
        let fp = "a1b2c3";
        let challenge = Challenge::nonce();
        let sig = challenge.sign_for(&hub, fp).unwrap();
        assert!(challenge.verify_for(&hub.public_key(), fp, &sig).is_ok());
    }

    #[test]
    fn fake_hub_cannot_sign() {
        let real = HubKeypair::generate();
        let fake = HubKeypair::generate();
        let challenge = Challenge::nonce();
        let sig = challenge.sign_for(&fake, "fp").unwrap();
        assert!(challenge
            .verify_for(&real.public_key(), "fp", &sig)
            .is_err());
    }

    #[test]
    fn challenge_bound_to_agent_fingerprint() {
        let hub = HubKeypair::generate();
        let challenge = Challenge::nonce();
        let sig = challenge.sign_for(&hub, "agent-A").unwrap();
        // the same signature replayed for another agent must fail
        assert!(challenge
            .verify_for(&hub.public_key(), "agent-B", &sig)
            .is_err());
    }

    #[test]
    fn debug_never_leaks_secret() {
        let hub = HubKeypair::generate();
        let dbg = format!("{hub:?}");
        assert!(dbg.contains("pub"));
        assert!(!dbg.contains(
            &hub.secret_bytes()[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
    }
}
