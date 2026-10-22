//! tmux layer: exec helpers + process-tree ssh detection.
//!
//! Everything that spawns `tmux`/`ps`/`pgrep` lives here. The badge poller
//! (`poll.rs`) and the CLI state machine ask these questions; they never
//! spawn processes themselves.

mod exec;
mod ssh_scan;

pub use exec::{pane_pids, run_tmux, tmux_base, tmux_out, tmux_run};
pub use ssh_scan::{
    pane_ssh_host, parents_of, ssh_host_of_pid, ssh_parent_hosts, ssh_target_of,
    window_ssh_host,
};
