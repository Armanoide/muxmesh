//! Logging init — same pattern as lm-sanaga's sn_core::logger (init_tracing): level from
//! env (RUST_LOG), fallback via a project flag. stderr, target + file:line.
//!
//! Priority: RUST_LOG > MUXMESH_DEBUG=true -> "debug" > "info".
//! `muxmesh-hub -v` / `muxmesh-agent -v` set RUST_LOG=muxmesh=debug before
//! calling init, so the flag and the env var compose.

use std::env;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;

/// Install the global subscriber. Idempotent-safe: a second call (or an
/// already-set global) is ignored rather than panicking.
pub fn init() {
    let default_level = match env::var("MUXMESH_DEBUG").as_deref() {
        Ok("true") | Ok("1") => "debug",
        _ => "info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(std::io::stderr)
        .try_init();
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
