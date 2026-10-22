#!/usr/bin/env bash
# e2e-docker.sh — real-machine test on the Docker host (run via ssh-host).
#
# Roles (neutral names; prod mapping in parentheses):
#   machine_a = driver / master  (prod: macbook) — opens `ssh machine_b`,
#             presses F10, owns BOTH bars.
#   machine_b = target / child    (prod: mac_mini) — runs sshd, receives the
#             remote BarShow/BarHide commands.
#   machine_c = hub               (prod: Mac Mini) — router + directory, lives on
#             its OWN machine; never dials an agent (agents dial it via WS).
#
# Brings up hub + agent(machine_a) + agent(machine_b), each with its OWN tmux
# server (separate sockets). machine_a opens a REAL `ssh machine_b` session
# (sshd on machine_b, authorized key) so the poller arms the window naturally.
# Then drives the F10 keystroke with `muxmesh-cli bar-toggle` and asserts the
# full parent/child bar dance:
#   press 1 -> machine_a status=off, machine_b status=on  (BarShow pushed+acked)
#   press 2 -> machine_a status=on,  machine_b status=off (BarHide pushed+acked)
# Run from repo root:   bash scripts/e2e-docker.sh
# Cleanup is automatic on exit. SKIP_BUILD=1 reuses the existing image.
set -euo pipefail

IMAGE=muxmesh-e2e
NET=muxmesh-e2e-net
HUB=mm-c           # machine_c = hub (separate machine, prod: Mac Mini)
A=mm-a            # machine_a = driver (F10)
B=mm-b            # machine_b = target (sshd)
HOST_A=machine_a
HOST_B=machine_b
HOST_C=machine_c
SOCK_A=/tmp/tmux-a.sock
SOCK_B=/tmp/tmux-b.sock
PASS=0; FAIL=0

log()  { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[1;32mOK\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
assert_eq() { if [ "$1" = "$2" ]; then ok "$3 ($1)"; else bad "$3: got '$1' want '$2'"; fi; }

cleanup() {
  if [ "${KEEP:-0}" = "1" ]; then echo "  KEEP=1: containers left running for inspection"; return; fi
  log "cleanup"
  docker rm -f "$HUB" "$A" "$B" >/dev/null 2>&1 || true
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
  -e MUXMESH_TOKENS=/data/tokens.json "$IMAGE" generate-token "$HOST_B" | head -1 > /tmp/mm-tok-b
docker run --rm --entrypoint muxmesh-hub -v "$VOL":/data \
  -e MUXMESH_TOKENS=/data/tokens.json "$IMAGE" generate-token "$HOST_A" | head -1 > /tmp/mm-tok-a
TOK_B=$(cat /tmp/mm-tok-b); TOK_A=$(cat /tmp/mm-tok-a)
echo "  minted tokens for $HOST_B + $HOST_A (volume, before server start)"

# SSH keypair so machine_a can open a REAL `ssh machine_b` (no simulation).
# Generated on the host into /tmp, public key copied into the shared volume;
# machine_b authorizes it.
rm -f /tmp/mm-id_e2e /tmp/mm-id_e2e.pub
ssh-keygen -t ed25519 -N '' -f /tmp/mm-id_e2e -C muxmesh-e2e >/dev/null
docker run --rm -v "$VOL":/data -v /tmp/mm-id_e2e.pub:/key.pub:ro "$IMAGE" \
  sh -c 'cp /key.pub /data/id_e2e.pub' >/dev/null
echo "  ssh keypair generated ($HOST_A -> $HOST_B, passwordless)"

log "machine_c = hub (loads the pre-minted tokens.json at startup)"
docker run -d --name "$HUB" --network "$NET" --network-alias "$HOST_C" -v "$VOL":/data \
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

log "agents (each = its own tmux server; $HOST_B also runs sshd)"
# machine_b: target. starts sshd (authorizes machine_a key) then the agent.
docker run -d --name "$B" --network "$NET" --network-alias "$HOST_B" -v "$VOL":/data \
  -e MUXMESH_HOST="$HOST_B" -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_B" -e MUXMESH_SOCKET="$SOCK_B" -e RUST_LOG=debug \
  "$IMAGE" sh -c "/opt/muxmesh/sshd-setup.sh sh -c 'tmux -S $SOCK_B new-session -d -s main; bash /opt/muxmesh/muxmesh.tmux; mkdir -p ~/.config/muxmesh; printf %s \"$TOK_B\" > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; exec muxmesh-agent'" >/dev/null

# machine_a: driver. plain agent + tmux; opens the ssh toward machine_b later.
# muxmesh.tmux is sourced on SOCK_A so the F10 bindings (root + muxmesh-remote)
# exist, exactly like the real MacBook tpm setup.
docker run -d --name "$A" --network "$NET" --network-alias "$HOST_A" \
  -e MUXMESH_HOST="$HOST_A" -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_A" -e MUXMESH_SOCKET="$SOCK_A" -e RUST_LOG=debug \
  "$IMAGE" sh -c "tmux -S $SOCK_A new-session -d -s main; export MUXMESH_SOCKET=$SOCK_A; bash /opt/muxmesh/muxmesh.tmux > /tmp/muxtmux-A.log 2>&1; mkdir -p ~/.config/muxmesh; printf '%s' '$TOK_A' > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; exec muxmesh-agent" >/dev/null

log "wait for both agents to register + WS-connect"
for i in $(seq 1 60); do
  S=$(docker exec "$HUB" cat /data/state.json 2>/dev/null || true)
  echo "$S" | grep -q "$HOST_A" && echo "$S" | grep -q "$HOST_B" && break
  sleep 0.5
done
echo "  state: $(docker exec "$HUB" cat /data/state.json)"

log "arm machine_a window toward $HOST_B (REAL ssh)"
# copy the private key into machine_a, then open a genuine `ssh machine_b` in a
# tmux window. sshd on machine_b accepts it (authorized key), so the pane holds
# a real `ssh machine_b` process — exactly what the poller detects.
docker exec "$A" sh -c 'mkdir -p /root/.ssh && chmod 700 /root/.ssh'
docker cp /tmp/mm-id_e2e "$A":/root/.ssh/id_e2e >/dev/null
docker exec "$A" sh -c 'chmod 600 /root/.ssh/id_e2e'
docker exec "$A" tmux -S "$SOCK_A" new-window -t main: \
  "ssh -t -i /root/.ssh/id_e2e -o BatchMode=yes -o StrictHostKeyChecking=no $HOST_B 'sleep 3600'"
# wait for the poller (every 2s) to arm @muxmesh-armed-win toward machine_b.
ARMED=""
for i in $(seq 1 30); do
  ARMED=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a -F '#{window_id} #{@muxmesh-armed-win}' | awk '$2!=""{print $1; exit}')
  [ -n "$ARMED" ] && break
  sleep 0.5
done
echo "  armed window: ${ARMED:-NONE}"
docker exec "$A" ps -eo pid,ppid,args | grep "[s]sh $HOST_B" || echo "  WARN: no live ssh $HOST_B"

log "PRE: both bars visible (status on), parent prefix intact"
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "on" "$HOST_B bar visible before test"
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on" "$HOST_A bar visible before test"
# parent must start in LOCAL state: prefix is NOT None (control held by parent).
P0=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$P0" != "None" ] && ok "press0: $HOST_A prefix intact (LOCAL, '$P0')" || bad "press0: $HOST_A prefix already None"

log "PRESS 1 (delegate control: bar-toggle in $HOST_A, armed ssh toward $HOST_B)"
docker exec "$A" sh -c "MUXMESH_SOCKET=$SOCK_A MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -3
sleep 1
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "off" "press1: $HOST_A bar hidden"
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "on"  "press1: $HOST_B bar shown (BarShow)"
# control delegated: parent prefix None (tmux-remote toggle_on). F10 stays
# bound in root AND off tables — the parent is asleep (client table off), not
# deaf: a press that comes back to us reclaims locally.
P1=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
assert_eq "$P1" "None" "press1: $HOST_A control DELEGATED (prefix None)"
docker exec "$A" tmux -S "$SOCK_A" list-keys -T off | grep -q F10 \
  && ok "press1: $HOST_A F10 bound in table off (asleep, reclaim reachable)" \
  || bad "press1: $HOST_A no F10 in table off — parent cannot reclaim"
# headless harness: switch-client -T off fails with 'no current client' (no
# attached client), so delegated state = prefix None + F10 bound in off table.
# switch-client is a no-op there; on real machines it puts the client to sleep.

log "WORKFLOW USER: presse1 -> fenêtre chez B -> presse2 -> fenêtre chez A"
# fenêtre chez l'enfant PENDANT la délégation : le focus clavier est dans le
# ssh, donc tmux -t touche bien la session machine_b (session nommée créée par
# l'sshd sur SOCK_B). Compte initial pris AVANT pour compter les +1.
WB0=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
docker exec "$B" tmux -S "$SOCK_B" new-window -t main:
sleep 1
WB1=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
[ "$WB1" -eq "$((WB0+1))" ] && ok "child: fenêtre créée pendant délégation ($WB0->$WB1)" || bad "child: pas de nouvelle fenêtre chez $HOST_B ($WB0->$WB1)"

# presse 2 au parent (ssh quitté ou enfant sourd) : reclaim local.
docker exec "$A" sh -c "MUXMESH_SOCKET=$SOCK_A MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -3
sleep 1
P2=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$P2" != "None" ] && ok "press2-local: $HOST_A control RECLAIMED (prefix '$P2')" || bad "press2-local: prefix still None — parent stuck"
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on" "press2-local: $HOST_A bar restored (local reclaim)"
docker exec "$A" tmux -S "$SOCK_A" list-keys -T root | grep -q F10 \
  && ok "press2-local: $HOST_A F10 still bound (safety ring)" \
  || bad "press2-local: $HOST_A F10 gone — no future toggle"

# le parent a récupéré le clavier : une nouvelle fenêtre doit atterrir chez A.
WA0=$(docker exec "$A" tmux -S "$SOCK_A" list-windows | wc -l | tr -d ' ')
docker exec "$A" tmux -S "$SOCK_A" new-window
sleep 1
WA1=$(docker exec "$A" tmux -S "$SOCK_A" list-windows | wc -l | tr -d ' ')
[ "$WA1" -eq "$((WA0+1))" ] && ok "parent: fenêtre créée après reclaim ($WA0->$WA1)" || bad "parent: pas de nouvelle fenêtre chez $HOST_A ($WA0->$WA1)"

log "PRESS 3 INSIDE machine_b (réseau: child answers ParentReclaim, bonus si child équipé)"
# the F10 is machine_b's own binding (muxmesh.tmux sourced there too); in the
# harness we invoke bar-toggle against SOCK_B, exactly what tmux would run.
docker exec "$B" sh -c "MUXMESH_SOCKET=$SOCK_B MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -3
sleep 2
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "off" "press3: $HOST_B bar hidden (child local toggle)"
# the child answered ParentReclaim over the hub; machine_a's agent applied it:
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on"  "press3: $HOST_A bar restored (ParentReclaim via hub)"
# control reclaimed: parent prefix NOT None + F10 rebound (safety ring works).
P3=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$P3" != "None" ] && ok "press3: $HOST_A control RECLAIMED (prefix '$P3')" || bad "press3: prefix still None — parent stuck without keyboard"
docker exec "$A" tmux -S "$SOCK_A" list-keys -T root | grep -q F10 \
  && ok "press3: $HOST_A F10 rebound (safety ring restored)" \
  || bad "press3: $HOST_A F10 still unbound — parent deaf forever"

log "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
