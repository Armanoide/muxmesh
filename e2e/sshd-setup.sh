#!/usr/bin/env bash
# e2e-sshd.sh — bootstrap a real sshd inside the machine_b container so the
# machine_a agent can open a genuine `ssh machine_b` session (no simulation).
# Runs as root inside mm-b. Reads the authorized key from /data/id_e2e.pub
# (shared volume), starts sshd on port 22, then execs the muxmesh agent so
# the container keeps its normal role.
set -euo pipefail

# sshd needs its host keys and a privilege-separation user.
ssh-keygen -A >/dev/null 2>&1 || true
mkdir -p /run/sshd

# authorize the machine_a public key for root (test-only, ephemeral container).
if [ -f /data/id_e2e.pub ]; then
  mkdir -p /root/.ssh
  chmod 700 /root/.ssh
  cp /data/id_e2e.pub /root/.ssh/authorized_keys
  chmod 600 /root/.ssh/authorized_keys
fi

# start sshd in the background (no daemonize flag needed; -D keeps it foreground
# under a supervisor, but here we background it so the agent runs in foreground).
/usr/sbin/sshd

exec "$@"
