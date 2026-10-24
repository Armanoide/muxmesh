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
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-environment -g PATH "$CURRENT_DIR/bin:/usr/local/bin:/opt/homebrew/bin:/usr/local/sbin:/usr/bin:/bin:$PATH"

# Declare the user options the poller writes. tmux rejects set of an
# unknown @-option ("invalid option"), so window-scope defaults must exist
# before the first badge update. The bar's hidden state is NOT stored here:
# tmux itself answers `show-options -g status` (verified 3.3a) — no flag to
# maintain, no scope footgun.
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-window-option -gq @muxmesh-armed-win 0
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-window-option -gq @muxmesh-status ""

# The bar shows WHERE the keyboard is: the poller paints status-left
# " LOCAL <host> " every pass (and a window armed toward a remote shows its
# own @muxmesh-status badge). The default window format must interpolate it.
# #{?A@B,C,D} is NOT a thing in tmux formats: an @-option must be written
# #{@muxmesh-status} to be read (verified tmux 3.3a).
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-window-option -g window-status-format "#[fg=colour208]#I#[fg=default]:#W#{?#{@muxmesh-status},#{@muxmesh-status},}"

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
# stdout redirected too: tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} run-shell shows non-zero-exit output as
# "#[cell]..." noise on the status line; keep tmux's view clean.
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} run-shell "mkdir -p $HOME/.muxmesh"

# TRACE removed: the double-fire diagnostic (prefix2 probe) did its job and
# is retired — F10 double-fire was terminal-side, settled.

# F10 local/parent binding + the delegated-mode escape hatch (table `off`):
# in OFF mode bar-toggle cuts the prefix to None and moves the client to
# table `off`; F10 stays bound there, so the parent can always reclaim.
# Nothing is ever unbound — control can never be stranded at the child.
CMD="$CLI bar-toggle -v >>\$HOME/.muxmesh/bar-toggle.log 2>&1"
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} bind-key -T root -n F10 run-shell "$CMD"
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} bind-key -T off -n F10 run-shell "$CMD"
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-option -gu prefix2
exit 0

