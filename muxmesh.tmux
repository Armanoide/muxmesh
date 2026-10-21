#!/usr/bin/env bash
# muxmesh.tmux — tpm entry point (tpm globs *.tmux at the plugin root and
# EXECUTES this file; it is a bash script, not tmux config). All logic for
# F10 lives in Rust (muxmesh-cli bar-toggle); this script only:
#   1. puts <plugin>/bin (launcher -> target/release) on tmux's PATH
#   2. binds F10
# The badge poller runs embedded in muxmesh-hub / muxmesh-agent.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# PATH: the tmux server's run-shell does not read shell rc files; its PATH is
# frozen at server start, so /usr/local/bin (where `just install` puts the
# binaries) may be missing -> F10 127. Inject it explicitly, here and now.
tmux set-environment -g PATH "$CURRENT_DIR/bin:/usr/local/bin:/opt/homebrew/bin:/usr/local/sbin:/usr/bin:/bin:$PATH"

# Declare the user options with defaults. tmux rejects set/show of an
# unknown @-option ("invalid option"), so the toggle's persistent state must
# be declared here to survive presses and server restarts. Without these the
# hidden flag never persists -> every F10 reads "not hidden" -> press 2 can
# never restore the parent bar (observed in UAT).
# SCOPE MATTERS: the CLI sets these with `set-window-option -t <window>`,
# which requires a WINDOW-Scope default. A global (-g) declaration does NOT
# satisfy the per-window check -> "invalid option" persists (observed).
tmux set-window-option -gq @muxmesh-bar-hidden off
tmux set-window-option -gq @muxmesh-armed-win 0
tmux set-window-option -gq @muxmesh-status ""

# F10: one-bar toggle against the armed remote host (Rust decides everything).
# Absolute path first (PATH is frozen at server start -> 127 otherwise); the
# bare name works once set-environment above has taken effect.
CLI="$CURRENT_DIR/bin/muxmesh-cli"
[ -x /usr/local/bin/muxmesh-cli ] && CLI=/usr/local/bin/muxmesh-cli
# -v: every F10 press leaves a full trace (steps + STOP points) in
# ~/.muxmesh/bar-toggle.log — the parent/child bar dance is invisible
# otherwise. Line-buffered by tracing; file append, never truncate.
# mkdir -p first: a missing ~/.muxmesh makes the shell's stderr redirect fail
# *before* exec -> F10 dies silently with exit 1 (observed on the driver).
# stdout redirected too: tmux run-shell shows non-zero-exit output as
# "#[cell]..." noise on the status line; keep tmux's view clean.
tmux run-shell "mkdir -p $HOME/.muxmesh"
tmux bind-key -n F10 run-shell "$CLI bar-toggle -v >>$HOME/.muxmesh/bar-toggle.log 2>&1"
