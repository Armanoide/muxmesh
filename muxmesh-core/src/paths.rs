//! Where the muxmesh binaries live, and the exact F10 run-shell command.
//! One spelling for every crate (cli rollback, hub F10 bind).

/// Absolute path of muxmesh-cli: install dir, MUXMESH_BIN_DIR override, or
/// bare name (PATH lookup).
pub fn cli_path() -> String {
    if std::path::Path::new("/usr/local/bin/muxmesh-cli").is_file() {
        "/usr/local/bin/muxmesh-cli".to_string()
    } else if let Ok(dir) = std::env::var("MUXMESH_BIN_DIR") {
        format!("{dir}/muxmesh-cli")
    } else {
        "muxmesh-cli".to_string()
    }
}

/// The exact run-shell command F10 must carry (bind + re-bind paths). The
/// log redirect keeps tmux's status line clean and leaves a full trace.
pub fn bar_toggle_cmd() -> String {
    format!(
        "{} bar-toggle -v >>$HOME/.muxmesh/bar-toggle.log 2>&1",
        cli_path()
    )
}
