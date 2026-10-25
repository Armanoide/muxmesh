use std::env;
use tracing_subscriber::EnvFilter;

/// Same logging contract as the hub: RUST_LOG wins, MUXMESH_DEBUG drives the
/// default level. Hub and agent must emit identical log shapes.
pub fn init_tracing() {
    let muxmesh_debug = env::var("MUXMESH_DEBUG").unwrap_or_else(|_| "false".to_string());
    let default_level = if muxmesh_debug == "true" { "debug" } else { "info" };

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}
