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

log "agents (each = its own tmux server)"
docker run -d --name "$MINI" --network "$NET" \
  -e MUXMESH_HOST=mac_mini -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_MINI" -e MUXMESH_SOCKET="$SOCK_MINI" -e RUST_LOG=debug \
  "$IMAGE" sh -c "tmux -S $SOCK_MINI new-session -d -s main; mkdir -p ~/.config/muxmesh; printf '%s' '$TOK_MINI' > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; muxmesh-agent" >/dev/null

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

log "arm the macbook window toward mac_mini (fake ssh) + run poller once"
# detached so the fake ssh stays alive across docker exec calls (a plain
# exec exits with the pane and tmux kills it).
docker exec -d "$MBP" tmux -S "$SOCK_MBP" new-window -t main: "bash /opt/muxmesh/arm-ssh.sh mac_mini"
sleep 1
# the embedded poller runs every 2s; force one pass against the live hub so
# @muxmesh-armed-win is set on the fake-ssh window before we press.
docker exec "$MBP" sh -c "MUXMESH_SOCKET=$SOCK_MBP MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli poll --once" 2>&1 | tail -2 || true
sleep 1

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
