#!/usr/bin/env bash
# e2e-docker.sh — real-machine simulation on the Docker host (run via ssh-host).
# Brings up: hub + agent(mac_mini) + agent(macbook), each with its OWN tmux
# server (separate sockets). Simulates the F10 keystroke by running
# `muxmesh-cli bar-toggle` in the macbook context with a fake armed ssh toward
# mac_mini, asserting the full parent/child bar dance:
#   press 1 -> macbook status=off, mac_mini status=on (BarShow pushed+acked)
#   press 2 -> macbook status=on,  mac_mini status=off (BarHide pushed+acked)
# This is the cycle that failed on the real MacBook. Run from repo root:
#   bash scripts/e2e-docker.sh
# Cleanup is automatic on exit.
set -euo pipefail

IMAGE=muxmesh-e2e
NET=muxmesh-e2e-net
HUB=mm-hub
MINI=mm-mini
MBP=mm-mbp
SOCK_MINI=/tmp/tmux-mini.sock
SOCK_MBP=/tmp/tmux-mbp.sock
PASS=0; FAIL=0

log()  { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[1;32mOK\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
assert_eq() { if [ "$1" = "$2" ]; then ok "$3 ($1)"; else bad "$3: got '$1' want '$2'"; fi; }

cleanup() {
  if [ "${KEEP:-0}" = "1" ]; then echo "  KEEP=1: containers left running for inspection"; return; fi
  log "cleanup"
  docker rm -f "$HUB" "$MINI" "$MBP" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "build image $IMAGE"
if docker image inspect "$IMAGE" >/dev/null 2>&1 && [ "${SKIP_BUILD:-0}" = "1" ]; then
  echo "  skipped (image exists, SKIP_BUILD=1)"
else
  docker build -f Dockerfile.e2e -t "$IMAGE" . >/dev/null
fi

log "network + tokens (minted BEFORE the server: it loads tokens.json once at startup)"
docker network create "$NET" >/dev/null 2>&1 || true
VOL=muxmesh-e2e-data
docker volume create "$VOL" >/dev/null 2>&1 || true

# mint into a shared volume with the SAME config the server will use
docker run --rm --entrypoint muxmesh-hub -v "$VOL":/data \
  -e MUXMESH_TOKENS=/data/tokens.json "$IMAGE" generate-token mac_mini | head -1 > /tmp/mm-tok-mini
docker run --rm --entrypoint muxmesh-hub -v "$VOL":/data \
  -e MUXMESH_TOKENS=/data/tokens.json "$IMAGE" generate-token macbook | head -1 > /tmp/mm-tok-mbp
TOK_MINI=$(cat /tmp/mm-tok-mini); TOK_MBP=$(cat /tmp/mm-tok-mbp)
echo "  minted tokens for mac_mini + macbook (volume, before server start)"

# SSH keypair so macbook can open a REAL `ssh mac_mini` (no simulation). Generated
# on the host into /tmp, copied into the shared volume; mm-mini authorizes it.
rm -f /tmp/mm-id_e2e /tmp/mm-id_e2e.pub
ssh-keygen -t ed25519 -N '' -f /tmp/mm-id_e2e -C muxmesh-e2e >/dev/null
docker run --rm -v "$VOL":/data -v /tmp/mm-id_e2e.pub:/key.pub:ro alpine \
  sh -c 'cp /key.pub /data/id_e2e.pub' >/dev/null 2>&1 || \
docker run --rm -v "$VOL":/data debian:bookworm-slim \
  sh -c 'cp /key.pub /data/id_e2e.pub' >/dev/null
echo "  ssh keypair generated (macbook -> mac_mini, passwordless)"

log "hub (loads the pre-minted tokens.json at startup)"
docker run -d --name "$HUB" --network "$NET" -v "$VOL":/data \
  -e MUXMESH_BIND=0.0.0.0:9997 \
  -e MUXMESH_STATE=/data/state.json \
  -e MUXMESH_TOKENS=/data/tokens.json \
  -e MUXMESH_POLL_HUB=127.0.0.1:1 \
  -e RUST_LOG=debug \
  "$IMAGE" muxmesh-hub >/dev/null

for i in $(seq 1 50); do
  docker exec "$HUB" sh -c 'exec 3<>/dev/tcp/127.0.0.1/9997' 2>/dev/null && break
  sleep 0.2
done

log "agents (each = its own tmux server; mac_mini also runs sshd)"
docker run -d --name "$MINI" --network "$NET" -v "$VOL":/data \
  -e MUXMESH_HOST=mac_mini -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_MINI" -e MUXMESH_SOCKET="$SOCK_MINI" -e RUST_LOG=debug \
  "$IMAGE" sh -c "/opt/muxmesh/sshd-setup.sh sh -c 'tmux -S $SOCK_MINI new-session -d -s main; mkdir -p ~/.config/muxmesh; printf %s \"$TOK_MINI\" > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; exec muxmesh-agent'" >/dev/null

docker run -d --name "$MBP" --network "$NET" \
  -e MUXMESH_HOST=macbook -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_MBP" -e MUXMESH_SOCKET="$SOCK_MBP" -e RUST_LOG=debug \
  "$IMAGE" sh -c "tmux -S $SOCK_MBP new-session -d -s main; mkdir -p ~/.config/muxmesh; printf '%s' '$TOK_MBP' > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; muxmesh-agent" >/dev/null

log "wait for both agents to register + WS-connect"
for i in $(seq 1 60); do
  S=$(docker exec "$HUB" cat /data/state.json 2>/dev/null || true)
  echo "$S" | grep -q mac_mini && echo "$S" | grep -q macbook && break
  sleep 0.5
done
echo "  state: $(docker exec "$HUB" cat /data/state.json)"

log "arm the macbook window toward mac_mini (REAL ssh)"
# copy the private key into macbook, then open a genuine `ssh mac_mini` in a
# tmux window. The sshd on mac_mini accepts it (authorized key), so the pane
# holds a real `ssh mac_mini` process — exactly what the poller detects. No
# simulation, no argv hacks.
docker cp /tmp/mm-id_e2e "$MBP":/root/.ssh/id_e2e >/dev/null
docker exec "$MBP" sh -c 'chmod 600 /root/.ssh/id_e2e'
# ssh with a pty (-t) so the session persists; BatchMode avoids any prompt.
docker exec "$MBP" tmux -S "$SOCK_MBP" new-window -t main: \
  "ssh -t -i /root/.ssh/id_e2e -o BatchMode=yes -o StrictHostKeyChecking=no mac_mini 'sleep 3600'"
# wait for the poller (every 2s) to arm @muxmesh-armed-win toward mac_mini.
ARMED=""
for i in $(seq 1 30); do
  ARMED=$(docker exec "$MBP" tmux -S "$SOCK_MBP" list-windows -a -F '#{window_id} #{@muxmesh-armed-win}' | awk '$2!=""{print $1; exit}')
  [ -n "$ARMED" ] && break
  sleep 0.5
done
echo "  armed window: ${ARMED:-NONE}"
docker exec "$MBP" ps -eo pid,ppid,args | grep '[s]sh mac_mini' || echo "  WARN: no live ssh mac_mini"

log "PRE: both bars visible (status on)"
assert_eq "$(docker exec "$MINI" tmux -S "$SOCK_MINI" show-options -g status | awk '{print $2}')" "on" "mac_mini bar visible before test"
assert_eq "$(docker exec "$MBP" tmux -S "$SOCK_MBP" show-options -g status | awk '{print $2}')" "on" "macbook bar visible before test"

log "PRESS 1 (bar-toggle in macbook, armed ssh toward mac_mini)"
docker exec "$MBP" sh -c "MUXMESH_SOCKET=$SOCK_MBP MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -2
sleep 1
assert_eq "$(docker exec "$MBP" tmux -S "$SOCK_MBP" show-options -g status | awk '{print $2}')" "off" "press1: macbook bar hidden"
assert_eq "$(docker exec "$MINI" tmux -S "$SOCK_MINI" show-options -g status | awk '{print $2}')" "on"  "press1: mac_mini bar shown (BarShow)"

log "PRESS 2 (the step that failed on the real MacBook)"
docker exec "$MBP" sh -c "MUXMESH_SOCKET=$SOCK_MBP MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -2
sleep 1
assert_eq "$(docker exec "$MBP" tmux -S "$SOCK_MBP" show-options -g status | awk '{print $2}')" "on"  "press2: macbook bar restored"
assert_eq "$(docker exec "$MINI" tmux -S "$SOCK_MINI" show-options -g status | awk '{print $2}')" "off" "press2: mac_mini bar hidden (BarHide)"

log "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
