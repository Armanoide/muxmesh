mod logger;

use axum::{routing::get, Router};
use tracing::info;

// The hub = a router + a listener. Everything else will hang off HERE.
#[tokio::main]
async fn main() {
    logger::init_tracing();

    let app = Router::new().route("/", get(|| async { "muxmesh-hub" }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9997").await.unwrap();
    info!("muxmesh-hub listening on http://127.0.0.1:9997");
    axum::serve(listener, app).await.unwrap();
}
