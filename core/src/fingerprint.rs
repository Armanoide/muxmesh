use sha2::{Digest, Sha256};

pub const PEPPER: &str = "muxmesh-v1";

/// Hardware fingerprint of the agent, computed BY THE AGENT, pinned by the
/// hub at first connection. The token proves "I belong to the cluster", the
/// fingerprint proves "I am the same machine as last time".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(pub String);

impl Fingerprint {
    /// Inputs = stable machine descriptors (Linux machine-id, macOS
    /// IOPlatformUUID, or hostname+cpu as fallback). The agent reads them.
    pub fn compute(machine_id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PEPPER.as_bytes());
        hasher.update(machine_id.as_bytes());
        let digest = hasher.finalize();
        Fingerprint(digest.iter().take(24).map(|b| format!("{b:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_machine_id_same_fingerprint() {
        assert_eq!(
            Fingerprint::compute("abc123"),
            Fingerprint::compute("abc123")
        );
    }

    #[test]
    fn different_machine_id_different_fingerprint() {
        assert_ne!(
            Fingerprint::compute("abc123"),
            Fingerprint::compute("xyz789")
        );
    }

    #[test]
    fn fingerprint_is_48_hex() {
        assert_eq!(Fingerprint::compute("any").as_str().len(), 48);
    }

    #[test]
    fn pepper_changes_everything() {
        // an agent switching PEPPER (v1 -> v2) = different fingerprint
        let with_v1 = {
            let mut h = Sha256::new();
            h.update(b"muxmesh-v1");
            h.update(b"abc123");
            h.finalize()
        };
        let with_v2 = {
            let mut h = Sha256::new();
            h.update(b"muxmesh-v2");
            h.update(b"abc123");
            h.finalize()
        };
        assert_ne!(with_v1, with_v2);
    }
}
