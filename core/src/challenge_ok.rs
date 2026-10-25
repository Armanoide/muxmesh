use serde::{Deserialize, Serialize};

/// Agent -> hub: proof that the agent verified the hub's CHALLENGE signature.
/// The session is only considered open by the hub after this arrives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChallengeOk {
    /// Echo of the challenge nonce, proving the agent saw *this* challenge.
    pub nonce: String,
}
