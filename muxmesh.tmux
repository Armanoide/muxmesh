#!/usr/bin/env bash
# muxmesh.tmux — tpm entry point (tpm globs *.tmux at the plugin root and
# EXECUTES this file; it is a bash script, not tmux config). All logic for
# F10 lives in Rust (muxmesh-cli bar-toggle); this script only:
#   1. puts <plugin>/bin (launcher -> target/release) on tmux's PATH
#   2. binds F10
# The badge poller runs embedded in muxmesh-hub / muxmesh-agent.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# PATH: the tmux server's run-shell does not read shell rc files; make the
# plugin's bin/ (and Homebrew on macOS) visible to every run-shell it spawns.
tmux set-environment -g PATH "$CURRENT_DIR/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

# F10: one-bar toggle against the armed remote host (Rust decides everything).
tmux bind-key -n F10 run-shell "muxmesh-cli bar-toggle"
