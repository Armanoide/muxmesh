use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use muxmesh_core::{decode, encode, hex, ChallengeOk, Fingerprint, Message as AppMessage};
use tracing::{info, warn};

use crate::state::SharedState;
const TOKEN_HEADER: &str = "x-muxmesh-token";
const FP_HEADER: &str = "x-muxmesh-fp";

/// Route handler for GET /agent/ws. Authenticates the token from the upgrade
/// headers, then runs the handshake on the open socket.
pub async fn agent_ws(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    let token = match headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        Some(t) => t.to_string(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let fp = match headers.get(FP_HEADER).and_then(|v| v.to_str().ok()) {
        Some(f) => Fingerprint(f.to_string()),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    if !state.verify_token(&token).await {
        warn!("agent presented unknown token — rejecting");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| run_handshake(socket, state, token, fp))
}

/// The live handshake: CHALLENGE down, CHALLENGE_OK up, then the session loop.
async fn run_handshake(socket: WebSocket, state: SharedState, token: String, fp: Fingerprint) {
    let (mut sink, mut stream) = socket.split();

    // 1. Hub proves its identity: sign a fresh nonce bound to this agent's fp.
    let challenge = muxmesh_core::Challenge::nonce();
    let sig = challenge.sign_for(&state.keys, fp.as_str()).unwrap();
    let wire = AppMessage::Challenge(muxmesh_core::ChallengeWire {
        nonce: hex(&challenge.0),
        sig: hex(&sig.to_bytes()),
    });

    // The agent announces itself first (HELLO); consume it before the challenge.
    let label = match stream.next().await {
        Some(Ok(WsMessage::Text(line))) => match decode(&line) {
            Ok(AppMessage::Hello(h)) => {
                info!(label = %h.label, "agent announced");
                h.label
            }
            Ok(other) => {
                warn!(?other, "expected HELLO first — closing");
                return;
            }
            Err(e) => {
                warn!("malformed first frame: {e}");
                return;
            }
        },
        _ => return,
    };

    if sink.send(WsMessage::Text(encode(&wire).unwrap().into())).await.is_err() {
        return;
    }

    // 2. Wait for the agent's proof that it verified our signature.
    let ok = match stream.next().await {
        Some(Ok(WsMessage::Text(line))) => decode(&line).ok().and_then(|m| {
            if let AppMessage::ChallengeOk(ChallengeOk { nonce }) = m {
                (nonce == hex(&challenge.0)).then_some(())
            } else {
                None
            }
        }),
        _ => None,
    };
    let Some(()) = ok else {
        warn!("agent did not confirm challenge — closing");
        return;
    };

    // 3. Pin the fingerprint (first connect) or reject a clone.
    if !state.pin_fingerprint(&token, &fp).await {
        warn!("fingerprint mismatch — possible token clone on another machine");
        return;
    }

    info!("agent session open");
    // Session loop: consume heartbeats into the live directory until the
    // socket dies, then pull the agent off the card.
    while let Some(Ok(WsMessage::Text(line))) = stream.next().await {
        if let Ok(AppMessage::ReportState(r)) = decode(&line) {
            state.update_agent(&label, &fp, &r).await;
        }
    }
    state.remove_agent(&label).await;
    info!(%label, "agent disconnected — removed from directory");
}
