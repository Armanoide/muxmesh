use std::env;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber once, at binary startup.
/// Level comes from RUST_LOG; fallback driven by MUXMESH_DEBUG ("true" => debug, else info).
pub fn init_tracing() {
    // Check for MUXMESH_DEBUG environment variable
    let muxmesh_debug = env::var("MUXMESH_DEBUG").unwrap_or_else(|_| "false".to_string());
    // Set base filter depending on debug flag
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
