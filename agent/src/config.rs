use std::env;

/// Agent configuration, loaded once from the environment (a config file comes
/// later). The hub public key is copied at install time — it is the anchor
/// of the agent's trust in the hub, parsed once at startup so a bad key
/// fails fast instead of mid-handshake on every reconnect.
pub struct Config {
    pub hub_url: String,
    pub token: String,
    pub hub_pubkey: ed25519_dalek::VerifyingKey,
    pub label: String,
}

impl Config {
    pub fn from_env() -> muxmesh_core::Result<Self> {
        let hub_pubkey_hex = env::var("MUXMESH_HUB_PUBKEY").expect("MUXMESH_HUB_PUBKEY required");
        let bytes = muxmesh_core::hex_bytes(&hub_pubkey_hex)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            muxmesh_core::ErrorCore::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("MUXMESH_HUB_PUBKEY must be 32 bytes (64 hex), got {}", v.len()),
            ))
        })?;
        Ok(Config {
            hub_url: env::var("MUXMESH_HUB").unwrap_or_else(|_| "ws://127.0.0.1:9997/agent/ws".into()),
            token: env::var("MUXMESH_TOKEN").expect("MUXMESH_TOKEN required"),
            hub_pubkey: ed25519_dalek::VerifyingKey::from_bytes(&arr)?,
            label: env::var("MUXMESH_LABEL").unwrap_or_else(|_| "unknown".into()),
        })
    }

    /// The agent's machine fingerprint, computed locally. On Linux this reads
    /// /etc/machine-id; the fallback keeps dev machines working.
    pub fn fingerprint(&self) -> muxmesh_core::Fingerprint {
        let machine_id = std::fs::read_to_string("/etc/machine-id")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| format!("fallback-{}", self.label));
        muxmesh_core::Fingerprint::compute(&machine_id)
    }
}
