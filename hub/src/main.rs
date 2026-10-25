mod logger;
mod state;
mod ws;

use axum::{routing::get, Router};
use std::sync::Arc;
use tracing::info;

use state::HubState;

// The hub = a router + a listener. Everything else hangs off HERE.
#[tokio::main]
async fn main() {
    logger::init_tracing();

    let state = Arc::new(HubState::new());
    // dev bootstrap: mint one token so an agent can enroll immediately.
    let token = state.mint("dev").await;

    let app = Router::new()
        .route("/", get(|| async { "muxmesh-hub" }))
        .route("/agent/ws", get(ws::agent_ws))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9997").await.unwrap();
    info!("muxmesh-hub listening on http://127.0.0.1:9997");
    info!(public_key = %state.keys.public_key_hex(), "hub identity key — copy into agent config");
    info!(token = %token, "dev enrollment token — copy into the agent config");
    axum::serve(listener, app).await.unwrap();
}
