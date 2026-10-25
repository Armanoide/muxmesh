use serde::{Deserialize, Serialize};

/// Wire form of the hub's handshake proof (adjacent-tagged payload).
/// The signing logic lives in signing.rs; this is only what crosses the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChallengeWire {
    pub nonce: String,
    pub sig: String,
}
