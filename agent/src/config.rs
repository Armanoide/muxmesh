use std::env;

/// Agent configuration, loaded once from the environment (a config file comes
/// later). The hub public key is copied at install time — it is the anchor
/// of the agent's trust in the hub.
pub struct Config {
    pub hub_url: String,
    pub token: String,
    pub hub_pubkey_hex: String,
    pub label: String,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            hub_url: env::var("MUXMESH_HUB").unwrap_or_else(|_| "ws://127.0.0.1:9997/agent/ws".into()),
            token: env::var("MUXMESH_TOKEN").expect("MUXMESH_TOKEN required"),
            hub_pubkey_hex: env::var("MUXMESH_HUB_PUBKEY").expect("MUXMESH_HUB_PUBKEY required"),
            label: env::var("MUXMESH_LABEL").unwrap_or_else(|_| "unknown".into()),
        }
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
