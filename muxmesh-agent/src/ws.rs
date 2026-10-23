//! Outbound WebSocket channel to the hub: dial, hold, reconnect forever.
//!
//! The agent dials /agent/ws at startup with its machine token headers and
//! keeps the channel open for life (backoff 1s -> 30s, reset on success).
//! The hub NEVER dials the agent — this task is the only contact.

use std::sync::Arc;
use std::time::Duration;

use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[path = "ws/session.rs"]
pub mod session;
pub use session::run_socket_until_dead;

// execute_and_ack + WsLogLayer are reached through ws::session::* (main.rs
// and tests spell the full path) — re-export kept for ws:: dispatch callers.
#[allow(unused_imports)]
pub use session::{execute_and_ack, WsLogLayer};

/// Keepalive cadence: one WS ping per 30 s.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Drop the connection after this many consecutive unanswered sent pings.
pub const MAX_MISSED_PONGS: u64 = 2;

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Persistent outbound WS channel: dial /agent/ws forever with the machine
/// token headers. One successful session hands the socket to
/// run_socket_until_dead; when it dies, reconnect with exponential backoff
/// 1s -> 30s (reset on success); logs state changes only (never the token).
/// shortcut: no jitter on the backoff — fine at homelab scale, add jitter if
/// many agents ever hammer a restarting hub.
pub async fn dial_hub_forever(
    cfg: Arc<muxmesh_core::AgentConfig>,
    log_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let url = {
        // core's ws_url() is the legacy /ws path; the axum hub serves both,
        // canonical /agent/ws wins.
        let legacy = cfg.ws_url();
        match legacy.strip_suffix("/ws") {
            Some(prefix) => format!("{prefix}/agent/ws"),
            None => legacy,
        }
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        let token = muxmesh_core::read_token().as_str().to_string();
        if token.is_empty() {
            tracing::warn!("ws: no token -> retrying in 1s");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        match connect_with_auth(&url, &cfg.host, &token).await {
            Ok((ws, resp)) => {
                tracing::info!("ws: connected to {url} (status {})", resp.status());
                run_socket_until_dead(&cfg, ws, log_rx.resubscribe()).await;
                tracing::warn!("ws: channel closed -> reconnecting");
                backoff = Duration::from_secs(1); // success resets the ramp
            }
            Err(e) => {
                // revoked/bad token: 100k-round KDF per handshake is real CPU —
                // back off hard; re-reads the token file each attempt so pasting
                // a fresh one recovers without restart.
                let denied = matches!(
                    &e,
                    tokio_tungstenite::tungstenite::Error::Http(resp) if resp.status() == 401
                );
                if denied {
                    backoff = Duration::from_secs(30);
                }
                tracing::warn!(
                    "ws: connect {url} failed: {e} -> retry in {}s",
                    backoff.as_secs()
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Dial the hub's /agent/ws with X-Muxmesh-Host + X-Muxmesh-Token (own
/// machine token — the same identity that signs commands and state reports).
#[allow(clippy::result_large_err)] // tungstenite::Error is theirs, not ours to box
async fn connect_with_auth(
    url: &str,
    host: &str,
    token: &str,
) -> tokio_tungstenite::tungstenite::Result<(
    Ws,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let mut req = url.into_client_request().map_err(|_| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid ws url",
        ))
    })?;
    // header values must be visible ASCII: a malformed pasted token must not
    // panic the agent — fail the attempt, the loop re-reads the file.
    let (hv, tv) = match (host.parse(), token.parse()) {
        (Ok(h), Ok(t)) => (h, t),
        _ => {
            tracing::error!("ws: host/token contain invalid header characters (check token file)");
            return Err(tokio_tungstenite::tungstenite::Error::Io(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid header value"),
            ));
        }
    };
    let h = req.headers_mut();
    h.insert("X-Muxmesh-Host", hv);
    h.insert("X-Muxmesh-Token", tv);
    connect_async(req).await
}