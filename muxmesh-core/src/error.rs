use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
