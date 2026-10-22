//! Logging init — same pattern as lm-sanaga's sn_core::logger (init_tracing): level from
//! env (RUST_LOG), fallback via a project flag. stderr, target + file:line.
//!
//! Priority: RUST_LOG > MUXMESH_DEBUG=true -> "debug" > "info".
//! `muxmesh-hub -v` / `muxmesh-agent -v` set RUST_LOG=muxmesh=debug before
//! calling init, so the flag and the env var compose.

use std::env;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Install the global subscriber. Idempotent-safe: a second call (or an
/// already-set global) is ignored rather than panicking.
pub fn init() {
    let default_level = match env::var("MUXMESH_DEBUG").as_deref() {
        Ok("true") | Ok("1") => "debug",
        _ => "info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Like `init()` but composes an extra layer (e.g. the agent's WS log
/// forwarder) alongside stderr. The filter is applied to the whole registry,
/// so both layers see exactly the same events. stderr output is unchanged.
pub fn init_with_extra(
    extra: Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
) {
    let default_level = match env::var("MUXMESH_DEBUG").as_deref() {
        Ok("true") | Ok("1") => "debug",
        _ => "info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // ureq/hyper/rustls are DEBUG-noisy (raw headers, stream pool): the
    // agent forwards its own debug lines to hub.log — cap third-party crates
    // at INFO so the central log stays readable even under RUST_LOG=debug.
    let filter = with_quiet_http(filter);
    // Registry + layers first (the boxed layer is dyn Layer<Registry>, so it
    // must join before the EnvFilter changes the subscriber type), then the
    // global filter, then stderr. Same events reach every layer.
    let _ = tracing_subscriber::registry()
        .with(extra)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_writer(std::io::stderr),
        )
        .with(filter)
        .try_init();
}

/// Silence the DEBUG chatter of HTTP client crates (raw headers, stream
/// pool) that would otherwise flood the central hub.log when the agent runs
/// with debug logging. Their INFO lines are kept.
fn with_quiet_http(filter: EnvFilter) -> EnvFilter {
    let mut f = filter;
    for c in ["ureq", "reqwest", "hyper", "hyper_util", "rustls"] {
        if let Ok(d) = format!("{c}=info").parse() {
            f = f.add_directive(d);
        }
    }
    f
}

/// Parse `-v`/`--verbose` out of argv (std only, no clap): sets
/// RUST_LOG=muxmesh=debug when absent so init_tracing picks it up, and
/// returns the remaining args. An explicit RUST_LOG always wins.
pub fn parse_verbose_flag(args: &[String]) -> Vec<String> {
    let mut rest = Vec::with_capacity(args.len());
    let mut seen = false;
    for a in args {
        if a == "-v" || a == "--verbose" {
            seen = true;
        } else {
            rest.push(a.clone());
        }
    }
    if seen && env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "muxmesh=debug");
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_sets_rust_log_when_absent() {
        env::remove_var("RUST_LOG");
        let args = vec!["run".into(), "-v".into()];
        let rest = parse_verbose_flag(&args);
        assert_eq!(rest, vec!["run".to_string()]);
        assert_eq!(env::var("RUST_LOG").unwrap(), "muxmesh=debug");
        env::remove_var("RUST_LOG");
    }

    #[test]
    fn explicit_rust_log_wins() {
        env::set_var("RUST_LOG", "trace");
        let args = vec!["-v".into()];
        parse_verbose_flag(&args);
        assert_eq!(env::var("RUST_LOG").unwrap(), "trace");
        env::remove_var("RUST_LOG");
    }
}
