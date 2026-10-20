//! Shared contract between hub, agents and the CLI.
//! The wire protocol lives here, once. All binaries use it as-is.

pub mod command;
pub mod config;
pub mod error;
pub mod httpd;
pub mod state;

pub use command::{valid_name, Ack, Command, CommandEnvelope};
pub use config::{
    config_dir, expand_tilde, read_token, token_matches, AgentConfig, HubConfig, Token,
};
pub use error::CoreError;
pub use state::{now_unix, HostState, RemoteSession, StateDoc};
