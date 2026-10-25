use serde::{Deserialize, Serialize};

/// The visible truth of a tmux status bar — read from tmux, never mirrored.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BarStatus {
    On,
    Off,
}

/// Agent -> hub: heartbeat — "I am alive, my bar is `status`".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportState {
    pub seen: u64,
    pub status: BarStatus,
}
