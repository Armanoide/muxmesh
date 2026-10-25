use serde::{Deserialize, Serialize};

/// Agent -> hub: receipt for a Command, correlated by `id`.
/// Class A (control-changing) must carry proof in a future `state` field;
/// class B (rhythm) only needs ok=true to release the queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ack {
    pub id: u64,
    pub ok: bool,
}
