use thiserror::Error;

/// Crate-wide Result alias — every fallible fn in muxmesh-agent returns this.
pub type Result<T> = std::result::Result<T, AgentError>;

/// One error enum for the whole agent crate. Core errors pass through
/// transparently; Ws is the transport only the agent speaks (the hub uses
/// axum's WS).
#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Core(#[from] muxmesh_core::ErrorCore),

    #[error("websocket: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
}
