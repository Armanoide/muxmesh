use serde::{Deserialize, Serialize};

/// Agent -> hub: announces itself after the WS upgrade. Decorative label only
/// (identity is the token, never this).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub label: String,
}
