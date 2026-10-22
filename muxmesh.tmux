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

# TRACE (diagnostic double-fire MacBook): `prefix2` is unused, so we borrow it
# as a keystroke probe. prefix2 C-b means: first C-b lands in table prefix2,
# we log the key that FOLLOWS with millisecond timestamps, then bounce to
# root. Read keys.log after pressing C-b then F10 twice:
#   - 2 KEY lines for 2 physical F10 = terminal is innocent, logic bug here
#   - 4 KEY lines = the MacBook terminal sends F10 twice (ESC O P + ESC [21~)
# shortcut: diagnostic only — remove once the double-fire is settled.
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} bind-key -T prefix2 -n F10 "\
  run-shell 'printf \"KEY %s F10\\n\" \"\$(date +%H:%M:%S)\" >> \$HOME/.muxmesh/keys.log'; \
  switch-client -T root"
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} set-option -g prefix2 C-b

# F10 here is the LOCAL/parent binding. In delegated mode bar-toggle UNBINDS
# it and cuts the prefix to None (tmux-remote's exact mechanism): the parent
# goes fully deaf, press 2 traverses ssh into the child's tmux, and the child
# answers ParentReclaim over the hub — the parent's agent rebinds this F10,
# restores the prefix and shows the bar again. The parent's keyboard is never
# the recovery path, so a deaf parent can never strand the user.
CMD="$CLI bar-toggle -v >>\$HOME/.muxmesh/bar-toggle.log 2>&1"
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} bind-key -T root -n F10 run-shell "$CMD"
# table off: defense in depth only. The REAL guard against the parent stealing
# press 2 is send.rs unbind-key -n F10 (delegated): switch-client protects
# the attached client, not the keystroke path — with root F10 alive the press
# never traverses ssh and the child bar never hides (MacBook regression).
tmux ${MUXMESH_SOCKET:+-S "$MUXMESH_SOCKET"} bind-key -T off -n F10 run-shell "$CMD"
# NOTE: tmux's own bind-key takes ONE argument for the command — do NOT write
# `bind-key run-shell "..."` unquoted, word splitting makes run-shell see
# several arguments ("too many arguments") -> tpm exits 1 (observed tmux 3.3a).
exit 0

