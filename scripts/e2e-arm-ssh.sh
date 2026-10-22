#!/usr/bin/env bash
# e2e-arm-ssh.sh — make a fake "ssh <target>" process appear in a tmux pane so
# armed-host detection (pane_ssh_host -> looks_like_ssh + ssh_target_of) arms
# the window toward <target>, exactly as a real `ssh mac_mini` would.
#
# looks_like_ssh checks argv[0] basename == "ssh"; ssh_target_of takes the
# first non-flag token. We cannot rely on `exec -a` (tmux runs window commands
# through POSIX sh, where -a is not a builtin and the argv rewrite misfires).
# Instead we create a wrapper script literally named "ssh" in a private dir:
# its basename is "ssh", it ignores its args (so sleep never sees the target
# as an interval), and it sleeps to stay alive. ps shows "ssh mac_mini".
set -euo pipefail
TARGET="${1:-mac_mini}"
FAKE_DIR="/tmp/muxmesh-e2e-bin"
mkdir -p "$FAKE_DIR"
# wrapper literally named "ssh": argv0 basename matches, args ignored.
cat > "$FAKE_DIR/ssh" <<'EOF'
#!/bin/sh
# fake ssh for e2e: ignore args, stay alive so the poller sees us.
sleep 3600
EOF
chmod +x "$FAKE_DIR/ssh"
# run it with the target as argv[1] so ps shows "ssh mac_mini".
exec "$FAKE_DIR/ssh" "$TARGET"
