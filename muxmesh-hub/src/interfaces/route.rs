//! HTTP route handlers: /state (GET+POST), /cmd, fallback.

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use muxmesh_core::{now_unix, valid_host, valid_name, CommandEnvelope, HostState};

use crate::state::{merge_state, AppState};
use crate::ws;

/// Max POST body the hub accepts (agents send a few KB; beyond that = abuse).
pub const MAX_BODY: usize = 1_048_576;

/// GET /state: open (badge pollers and `tree` are read-only and the state
/// holds no secrets).
pub async fn get_state(State(state): State<Arc<AppState>>) -> Response {
    let body =
        std::fs::read_to_string(&state.state_path).unwrap_or_else(|_| "{\"hosts\":{}}".into());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// POST /state: auth by TOKEN (the identity); the DECLARED host name is the
/// key state is recorded under, so renaming a machine = edit config.toml, no
/// re-mint. Tokens are minted OFFLINE by `muxmesh-hub generate-token <host>`
/// — there is NO registration endpoint: an unregistered token cannot introduce
/// itself, whatever name it declares.
pub async fn post_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let host_hdr = AppState::hdr(&headers, "x-muxmesh-host");
    let got_token = AppState::hdr(&headers, "x-muxmesh-token");
    // token decides; declared name (header or payload) is the label.
    let authed = state.auth_host(&host_hdr, &got_token);
    if authed.is_none() {
        tracing::warn!(
            "POST /state host={host_hdr} auth=FAIL (registered: {})",
            state.tokens.lock().map(|s| s.hosts.len()).unwrap_or(0)
        );
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }

    if body.is_empty() || body.len() > MAX_BODY {
        return (StatusCode::BAD_REQUEST, "bad length\n").into_response();
    }

    // host: X-Muxmesh-Host header (source of truth), else the payload field
    let mut st: HostState = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid payload: {e}\n"),
            )
                .into_response()
        }
    };
    if st.seen == 0 {
        st.seen = now_unix(); // clockless agent -> stamp on receipt
    } else {
        // clock-skew guard: a future `seen` would stay "fresh" forever; clamp it.
        let now = now_unix();
        if st.seen > now + 60 {
            st.seen = now;
        }
    }
    let host_key = authed.unwrap(); // token-verified identity (declared label)
    if host_key.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing host\n").into_response();
    }

    // TRUST BOUNDARY: host and session names become tmux target strings later
    // (poller `-t`). Whitelist them here so downstream tmux argv is provably
    // [A-Za-z0-9_.-]+ and target injection is structurally impossible.
    if !valid_host(&host_key) {
        return (StatusCode::BAD_REQUEST, "invalid host name\n").into_response();
    }
    for s in &st.sessions {
        if !valid_name(&s.name)
            || (!s.active_window.is_empty() && !valid_name(&s.active_window))
        {
            // name the rejected value: tmux allows ':' in window names, our
            // whitelist does not -> without this the 400 is undiagnosable.
            tracing::warn!(
                "rejecting session name='{}' active_window='{}' from host={host_hdr} (valid_name=[A-Za-z0-9_-])",
                s.name,
                s.active_window
            );
            return (StatusCode::BAD_REQUEST, "invalid session name\n").into_response();
        }
    }

    // merge under the write lock -> atomic write. Raw JSON doc (not StateDoc)
    // so the out-of-band last_cmd ack mirror survives the agent's next merge.
    let merge = {
        let _g = state.write_lock.lock();
        merge_state(&state.state_path, &host_key, &st)
    };
    match merge {
        Ok(trace) => {
            tracing::info!("state updated: {host_key} {trace}");
            (StatusCode::OK, "ok\n").into_response()
        }
        Err(e) => {
            tracing::error!("POST /state merge {host_key}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "write failed\n").into_response()
        }
    }
}

/// POST /cmd: authenticate the driver, check dst registered + has a live WS
/// channel right now, enqueue the frame on that channel, answer 202 queued.
/// The hub NEVER dials the agent — delivery happens on the agent's own socket.
pub async fn post_cmd(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // HEAD parity: empty/over-cap body -> 400 "bad length" (no axum
    // DefaultBodyLimit on this route: it would answer 413 before the handler
    // and break the CLI's literal status-line contract).
    if body.is_empty() || body.len() > MAX_BODY {
        return (StatusCode::BAD_REQUEST, "bad length\n").into_response();
    }

    // closed list: unknown verbs die here, before any dispatch.
    let env: CommandEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid payload: {e}\n"),
            )
                .into_response()
        }
    };

    // AUTH driver: token is the identity (same as /state). The declared src
    // must match the authenticated identity — a driver cannot impersonate
    // another host. dst is NOT checked before auth: 400-before-401 would let
    // an unauthenticated caller enumerate registered hosts.
    let host_hdr = AppState::hdr(&headers, "x-muxmesh-host");
    let got_token = AppState::hdr(&headers, "x-muxmesh-token");
    let authed = state.auth_host(&host_hdr, &got_token);
    if authed.is_none() {
        tracing::warn!("POST /cmd src={host_hdr} auth=FAIL -> 401");
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    let authed = authed.unwrap();
    if env.src != authed {
        tracing::warn!("POST /cmd src={host_hdr} mismatch authenticated='{authed}' -> 403");
        return (StatusCode::FORBIDDEN, "src mismatch\n").into_response();
    }

    // dst must be a well-formed hub host key before any lookup.
    let dst = env.dst.clone();
    if dst.is_empty() || !valid_host(&dst) {
        return (StatusCode::BAD_REQUEST, "invalid dst\n").into_response();
    }
    // the agent must hold a live WS channel RIGHT NOW — the hub has no other
    // way (and no right) to reach it. A live channel is proof the dst token
    // was authenticated at handshake, so no separate tokens.json check here:
    // a renamed machine's channel (declared label) is reachable by its new name.
    let enqueued = {
        let ch = state.channels.lock().unwrap_or_else(|e| e.into_inner());
        match ch.get(&dst) {
            Some(s) => match serde_json::to_string(&env) {
                Ok(frame) => s.tx.send(frame).is_ok(),
                // a frame that cannot be serialized must never reach the agent
                // as an empty text frame — drop it and name it in the log.
                Err(e) => {
                    tracing::error!("POST /cmd id={} serialize failed: {e}", env.id);
                    false
                }
            },
            None => false,
        }
    };
    if !enqueued {
        tracing::warn!("POST /cmd dst={dst} -> 409 target not connected");
        return (StatusCode::CONFLICT, "target not connected\n").into_response();
    }

    tracing::info!("POST /cmd src={host_hdr} -> queued dst={dst} id={}", env.id);
    // serde-serialized (env.id is CLI-generated today, but never hand-build JSON)
    let body = axum::Json(serde_json::json!({"v":1,"id":env.id,"queued":true}));
    (StatusCode::ACCEPTED, body).into_response()
}

pub async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// GET /agent/ws: upgrade only for a host the gate authenticated.
pub async fn agent_ws(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    crate::interfaces::auth::AuthenticatedHost(host): crate::interfaces::auth::AuthenticatedHost,
) -> Response {
    // identity comes from the gate's verification, never from a second header read
    ws.on_upgrade(move |socket| ws::handle_agent(socket, host, state))
}
