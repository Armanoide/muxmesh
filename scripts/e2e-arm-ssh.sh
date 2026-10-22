#!/usr/bin/env bash
# e2e-arm-ssh.sh — make a fake "ssh <target>" process appear as a CHILD of the
# pane's shell so armed-host detection (pane_ssh_host -> looks_like_ssh +
# ssh_target_of) arms the window toward <target>, exactly as a real `ssh mac_mini`
# typed in an interactive shell does.
#
# In production the pane's pid is the interactive shell and `ssh host` is its
# child; pane_ssh_host scans children (pgrep -P), never the pane pid itself. So
# we must NOT exec the fake ssh (that would make it the pane pid and never be
# scanned). Instead we start it in the background and keep this shell alive as
# the pane's parent.
#
# looks_like_ssh checks argv[0] basename == "ssh"; ssh_target_of takes the first
# non-flag token. A wrapper script literally named "ssh" satisfies both: its
# basename is ssh, it ignores args (so sleep never sees the target as interval).
set -euo pipefail
TARGET="${1:-mac_mini}"
FAKE_DIR="/tmp/muxmesh-e2e-bin"
mkdir -p "$FAKE_DIR"
cat > "$FAKE_DIR/ssh" <<'EOF'
#!/bin/sh
# fake ssh for e2e: ignore args, stay alive so the poller sees us.
sleep 3600
EOF
chmod +x "$FAKE_DIR/ssh"
# child process named ssh with target as argv[1]; ps shows "ssh mac_mini".
"$FAKE_DIR/ssh" "$TARGET" &
# stay alive as the pane's parent shell so the child is a real descendant.
wait
