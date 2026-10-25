use serde::{Deserialize, Serialize};

/// Hub -> agent: the current directory (who is armed where), on demand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Directory {
    pub me: String,
    pub armed: Vec<String>,
}
