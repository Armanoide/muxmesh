//! Authentication for the hub's two surfaces: the WS handshake gate and the
//! per-request header auth used by /state and /cmd. TOKEN is the identity —
//! the declared host name is only the label state is recorded under.

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{request::Parts, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::state::AppState;

/// Identity verified by `ws_auth_gate`; consumed by the WS handler via axum's
/// FromRequestParts — the handler never re-reads attacker-influenced headers.
#[derive(Clone, Debug)]
pub struct AuthenticatedHost(pub String);

impl FromRequestParts<Arc<AppState>> for AuthenticatedHost {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut Parts,
        _state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<AuthenticatedHost>()
            .cloned()
            .unwrap_or_else(|| AuthenticatedHost(String::new())))
    }
}

/// Gate on the WS paths: a GET /agent/ws (or legacy /ws) without valid auth
/// headers is 401 HERE — before the upgrade, before the handler. Any other
/// request passes through untouched.
pub async fn ws_auth_gate(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/agent/ws" || path == "/ws" {
        let host = AppState::hdr(req.headers(), "x-muxmesh-host");
        let token = AppState::hdr(req.headers(), "x-muxmesh-token");
        // token is the identity; host is a declared label. A machine may send
        // no host header (or rename freely) — the token decides, and the
        // declared name becomes the key state is recorded under.
        let authed = if host.is_empty() {
            None
        } else {
            state.auth_host(&host, &token)
        };
        match authed {
            Some(name) => {
                req.extensions_mut().insert(AuthenticatedHost(name));
            }
            None => {
                tracing::warn!("/agent/ws host={host} auth=FAIL -> 401 (no upgrade)");
                return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
            }
        }
    }
    next.run(req).await
}
