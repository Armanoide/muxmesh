use serde::{Deserialize, Serialize};

/// Closed set of actions an agent can perform on its local tmux.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Cmd {
    BarShow,
    BarHide,
    GiveControl,
    ResumeControl,
}

/// Hub -> agent: do this on this machine. `id` correlates the matching Ack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Command {
    pub id: u64,
    pub cmd: Cmd,
    pub target: String,
}
