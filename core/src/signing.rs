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

    /// Raw bytes of the fresh nonce (the hub uses this to build the wire msg).
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
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

    /// Agent helper: verify a wire challenge from hex inputs (nonce + sig as
    /// they arrived over JSON), against the hub's known public key.
    pub fn verify_hex(
        nonce_hex: &str,
        sig_hex: &str,
        pub_key: &VerifyingKey,
        agent_fp: &str,
    ) -> Result<()> {
        let nonce = hex_bytes(nonce_hex)?;
        let sig_bytes = hex_bytes(sig_hex)?;
        let sig = Signature::from_slice(&sig_bytes)?;
        let mut msg = nonce.to_vec();
        msg.extend_from_slice(agent_fp.as_bytes());
        Ok(pub_key.verify(&msg, &sig)?)
    }
}

fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(crate::error::ErrorCore::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "odd-length hex",
        )));
    }
    s.as_bytes()
        .chunks_exact(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap_or("00"), 16).map_err(crate::error::ErrorCore::from))
        .collect()
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

    #[test]
    fn odd_length_hex_is_error_not_panic() {
        // a truncated frame from the hub must fail cleanly, never crash.
        assert!(hex_bytes("abc").is_err());
        assert!(hex_bytes("").is_ok());
        assert_eq!(hex_bytes("00ff").unwrap(), vec![0x00, 0xff]);
    }
}
