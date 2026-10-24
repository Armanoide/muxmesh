//! Shared contract between hub, agents and the CLI.
//! The wire protocol lives here, once. All binaries use it as-is.

pub mod command;
pub mod config;
pub mod error;
pub mod http;
pub mod identity;
pub mod log;
pub mod paths;
pub mod poll;
pub mod state;
pub mod tmux;

pub use command::{valid_host, valid_name, Ack, Command, CommandEnvelope, CommandMeta};
pub use config::{
    agent_poll_hub, config_dir, expand_tilde, load_map, read_token, token_matches, AgentConfig,
    HubConfig, Token,
};
pub use error::CoreError;
pub use identity::{new_token, verify_token, TokenRecord, TokenStore, PREFIX_LEN};
pub use paths::bar_toggle_cmd;
pub use state::{now_unix, HostState, RemoteSession, StateDoc};
