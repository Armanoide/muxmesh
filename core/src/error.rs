use thiserror::Error;

/// Crate-wide Result alias — every fallible fn in muxmesh-core returns this.
pub type Result<T> = std::result::Result<T, ErrorCore>;

/// One error enum for the whole core crate. Foreign errors are wrapped
/// transparently with #[from]; domain variants carry their own context.
#[derive(Debug, Error)]
pub enum ErrorCore {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("protocol JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid hex: {0}")]
    Hex(#[from] std::num::ParseIntError),

    #[error("signature verification failed: {0}")]
    Signature(#[from] ed25519_dalek::SignatureError),

    #[error("missing X-Muxmesh-Token header")]
    MissingToken,

    #[error("unknown token — not enrolled (or revoked)")]
    UnknownToken,

    #[error("token valid but machine mismatch — fingerprint not pinned for this token")]
    FingerprintMismatch,
}
