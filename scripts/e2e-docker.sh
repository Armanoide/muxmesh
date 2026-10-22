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
# config.toml: `muxmesh-cli bar-toggle` lit poll.hub/agent.host depuis ici —
# sans ça il tombe sur 127.0.0.1:9997 (défaut) et ne voit jamais le hub.
docker run -d --name "$B" --network "$NET" --network-alias "$HOST_B" -v "$VOL":/data \
  -e MUXMESH_HOST="$HOST_B" -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_B" -e MUXMESH_SOCKET="$SOCK_B" -e RUST_LOG=debug \
  "$IMAGE" sh -c "/opt/muxmesh/sshd-setup.sh sh -c 'tmux -S $SOCK_B new-session -d -s main; bash /opt/muxmesh/muxmesh.tmux; mkdir -p ~/.config/muxmesh; printf %s \"$TOK_B\" > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; printf \"[agent]\\nhost = \\\"$HOST_B\\\"\\nhub = \\\"$HUB\\\"\\nport = 9997\\n[poll]\\nhub = \\\"$HUB:9997\\\"\\n\" > ~/.config/muxmesh/config.toml; exec muxmesh-agent'" >/dev/null

# machine_a: driver. plain agent + tmux; opens the ssh toward machine_b later.
# muxmesh.tmux is sourced on SOCK_A so the F10 bindings (root + muxmesh-remote)
# exist, exactly like the real MacBook tpm setup.
docker run -d --name "$A" --network "$NET" --network-alias "$HOST_A" \
  -e MUXMESH_HOST="$HOST_A" -e MUXMESH_HUB="$HUB" -e MUXMESH_PORT=9997 \
  -e MUXMESH_TMUX_SOCKET="$SOCK_A" -e MUXMESH_SOCKET="$SOCK_A" -e RUST_LOG=debug \
  "$IMAGE" sh -c "tmux -S $SOCK_A new-session -d -s main; export MUXMESH_SOCKET=$SOCK_A; bash /opt/muxmesh/muxmesh.tmux > /tmp/muxtmux-A.log 2>&1; mkdir -p ~/.config/muxmesh; printf '%s' '$TOK_A' > ~/.config/muxmesh/token; chmod 600 ~/.config/muxmesh/token; printf '[agent]\nhost = \"$HOST_A\"\nhub = \"$HUB\"\nport = 9997\n[poll]\nhub = \"$HUB:9997\"\n' > ~/.config/muxmesh/config.toml; exec muxmesh-agent" >/dev/null

log "wait for both agents to register + WS-connect"
for i in $(seq 1 60); do
  S=$(docker exec "$HUB" cat /data/state.json 2>/dev/null || true)
  echo "$S" | grep -q "$HOST_A" && echo "$S" | grep -q "$HOST_B" && break
  sleep 0.5
done
echo "  state: $(docker exec "$HUB" cat /data/state.json)"

log "arm machine_a window toward $HOST_B (REAL ssh)"
# client factice ATTACHE des ici : switch-client -T off est un etat PAR
# CLIENT — sans client attache tmux repond 'no current client' et la bascule
# OFF mode ne peut pas etre verifiee. En prod le client est toujours attache
# (le terminal de l'utilisateur). detach-client -a a la fin du script.
# script(1) alloue un pty (docker exec -d n'en a pas -> tmux attach meurt
# 'lost terminal'). Le fifo avec writer permanent (sleep, setsid) garde les
# deux bouts ouverts quand le shell exec() se termine ; TERM explicite car
# l'environnement du conteneur est nu. Le client reste attache jusqu'a la fin
# du run (conteneur detruit au cleanup).
docker exec "$A" sh -c 'rm -f /tmp/attach.fifo; mkfifo /tmp/attach.fifo'
docker exec "$A" sh -c '(setsid sleep 3600 >/tmp/attach.fifo &); setsid sh -c "TERM=xterm-256color script -q -c \"tmux -S '"$SOCK_A"' attach -t main\" /dev/null </tmp/attach.fifo >/dev/null 2>&1 & exit"'
sleep 2
docker exec "$A" tmux -S "$SOCK_A" list-clients | grep -q main \
  && echo "  fake client attached (OFF-mode verifiable)" \
  || echo "  WARN: no attached client — key-table assertions will not hold"
# copy the private key into machine_a, then open a genuine `ssh machine_b` in a
# tmux window. sshd on machine_b accepts it (authorized key), so the pane holds
# a real `ssh machine_b` process — exactly what the poller detects.
docker exec "$A" sh -c 'mkdir -p /root/.ssh && chmod 700 /root/.ssh'
docker cp /tmp/mm-id_e2e "$A":/root/.ssh/id_e2e >/dev/null
docker exec "$A" sh -c 'chmod 600 /root/.ssh/id_e2e'
docker exec "$A" tmux -S "$SOCK_A" new-window -t main: \
  "ssh -t -i /root/.ssh/id_e2e -o BatchMode=yes -o StrictHostKeyChecking=no $HOST_B 'sleep 3600'"
# wait for the poller (every 2s) to arm @muxmesh-armed-win toward machine_b.
# $2=="1" specifically: the poller writes 0 on non-armed windows, so $2!=""
# would match immediately on the initial bash window and exit too early.
ARMED=""
for i in $(seq 1 30); do
  ARMED=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a -F '#{window_id} #{@muxmesh-armed-win}' | awk '$2=="1"{print $1; exit}')
  [ -n "$ARMED" ] && break
  sleep 0.5
done
echo "  armed window: ${ARMED:-NONE}"
# focus the ssh window: bar-toggle reads the CURRENT window's armed state via
# display -p. select-window works without an attached client (verified tmux 3.3a).
# The poller arms @muxmesh-armed-win every 2s; we already waited for it above.
docker exec "$A" tmux -S "$SOCK_A" select-window -t "$ARMED"
sleep 1
docker exec "$A" ps -eo pid,ppid,args | grep "[s]sh $HOST_B" || echo "  WARN: no live ssh $HOST_B"

log "PRE: both bars visible (status on), parent prefix intact"
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "on" "$HOST_B bar visible before test"
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on" "$HOST_A bar visible before test"
# parent must start in LOCAL state: prefix is NOT None (control held by parent).
P0=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$P0" != "None" ] && ok "press0: $HOST_A prefix intact (LOCAL, '$P0')" || bad "press0: $HOST_A prefix already None"

log "PRESS 1 (delegate control: bar-toggle in $HOST_A, armed ssh toward $HOST_B)"
# MUXMESH_HOST is required: inside the container, hostname -s returns the
# container id (9e079a56cb5c), not machine_a -> the hub rejects POST /cmd 401.
docker exec "$A" sh -c "MUXMESH_HOST=$HOST_A MUXMESH_SOCKET=$SOCK_A MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -3
sleep 1
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "off" "press1: $HOST_A bar hidden"
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "on"  "press1: $HOST_B bar shown (BarShow)"
# control delegated: parent prefix None AND attached client moved to key
# table `off` (samoshkin OFF mode). root F10 stays bound but is INACTIVE
# while the client sits in `off` — press 2 traverses ssh into the child, and
# if it ever lands on the parent instead, the off-table F10 reclaims locally.
P1=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
assert_eq "$P1" "None" "press1: $HOST_A control DELEGATED (prefix None)"
# key table est un etat PAR CLIENT (client attache depuis le debut du run).
TBL=$(docker exec "$A" tmux -S "$SOCK_A" display-message -p '#{client_key_table}' 2>/dev/null || echo "")
assert_eq "$TBL" "off" "press1: $HOST_A client in key table 'off' (deaf except F10)"

log "WORKFLOW USER: presse1 -> fenetre chez B -> presse2 chez B -> fenetre chez A"
# fenetre chez l'enfant PENDANT la delegation (le focus clavier est dans le ssh).
WB0=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
docker exec "$B" tmux -S "$SOCK_B" new-window -t main:
sleep 1
WB1=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
[ "$WB1" -eq "$((WB0+1))" ] && ok "child: fenetre creee pendant delegation ($WB0->$WB1)" || bad "child: pas de nouvelle fenetre chez $HOST_B ($WB0->$WB1)"

# ============================================================================
# CHEMINS CRITIQUES (définition utilisateur — à vérifier ABSOLUMENT à chaque
# run E2E, bloquants : si l'un tombe, le test doit FAIL).
#
# CHEMIN CRITIQUE 1 — UNE SEULE BARRE VISIBLE :
#   - F10 (presse 1) => la barre de l'ENFANT s'affiche SEULE
#     (barre parent eteinte, barre enfant allumee) => A=off B=on
#   - F10 (presse 2) => la barre du PARENT s'affiche SEULE
#     (barre enfant eteinte, barre parent allumee) => A=on B=off
#   Jamais deux barres, jamais zero barre, a aucun moment.
#
# CHEMIN CRITIQUE 2 — DELEGATE CONTROL :
#   - BASE (non délégué) => prefix+c cree une fenetre CHEZ LE PARENT seulement.
#   - F10 (presse 1) => l'ENFANT a le control : prefix+c cree une fenetre
#     CHEZ L'ENFANT (et seulement chez l'enfant).
#   - F10 (presse 2) => le PARENT recupere le control : prefix+c cree une
#     fenetre CHEZ LE PARENT (et seulement chez le parent).
# ============================================================================

log "CHEMIN CRITIQUE 1: une seule barre visible (A=off/B=on apres presse1)"
# apres presse1 : l'enfant a la sienne allumee, le parent eteinte = une seule.
C1A=$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')
C1B=$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')
[ "$C1A" = "off" ] && [ "$C1B" = "on" ] \
  && ok "CRIT1: une seule barre apres presse1 (A=$C1A B=$C1B = enfant seul)" \
  || bad "CRIT1: PRESSE1 doit donner A=off B=on (enfant seul) — got A=$C1A B=$C1B"

log "CHEMIN CRITIQUE 2: delegate control (fenetre chez l'enfant pendant delegation)"
# le parent est sourd (prefix None + F10 unbound) : prefix+c doit creer chez B.
C2WB0=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
C2WA0=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
docker exec "$B" tmux -S "$SOCK_B" new-window -t main:
sleep 1
C2WB1=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
C2WA1=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
[ "$C2WB1" -eq "$((C2WB0+1))" ] && [ "$C2WA1" -eq "$C2WA0" ] \
  && ok "CRIT2: presse1 -> fenetre creee CHEZ L'ENFANT seulement (B $C2WB0->$C2WB1, A stable $C2WA0)" \
  || bad "CRIT2: presse1 = control chez l'enfant — fenetre doit atterrir chez B seulement (B $C2WB0->$C2WB1 A $C2WA0->$C2WA1)"

# PRESSE 2 CHEZ L'ENFANT (le keystroke traverse le ssh, parent sourd = table
# off + prefix None) : l'enfant masque sa barre et repond ParentReclaim via
# le hub.
docker exec "$B" sh -c "MUXMESH_HOST=$HOST_B MUXMESH_SOCKET=$SOCK_B MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -3
sleep 2
assert_eq "$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')" "off" "press2: $HOST_B bar hidden (child answers press 2)"
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on"  "press2: $HOST_A bar restored (ParentReclaim via hub)"
P2=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$P2" != "None" ] && ok "press2: $HOST_A control RECLAIMED (prefix '$P2')" || bad "press2: prefix still None — parent stuck"
docker exec "$A" tmux -S "$SOCK_A" list-keys -T root | grep -q F10 \
  && ok "press2: $HOST_A F10 present in root (safety ring restored)" \
  || bad "press2: $HOST_A F10 missing from root — parent deaf forever"

log "CHEMIN CRITIQUE 1 (retour): une seule barre visible (A=on/B=off apres presse2)"
# apres presse2 : le parent a la sienne allumee, l'enfant eteinte = une seule.
C1rA=$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')
C1rB=$(docker exec "$B" tmux -S "$SOCK_B" show-options -g status | awk '{print $2}')
[ "$C1rA" = "on" ] && [ "$C1rB" = "off" ] \
  && ok "CRIT1: une seule barre apres presse2 (A=$C1rA B=$C1rB = parent seul)" \
  || bad "CRIT1: PRESSE2 doit donner A=on B=off (parent seul) — got A=$C1rA B=$C1rB"

log "CHEMIN CRITIQUE 2 (retour): parent reprend le control (fenetre chez le parent)"
# le parent a recupere prefix + F10 : prefix+c doit creer chez A, pas chez B.
C2WA2=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
C2WB2=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
docker exec "$A" tmux -S "$SOCK_A" new-window
sleep 1
C2WA3=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
C2WB3=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
[ "$C2WA3" -eq "$((C2WA2+1))" ] && [ "$C2WB3" -eq "$C2WB2" ] \
  && ok "CRIT2: presse2 -> fenetre creee CHEZ LE PARENT seulement (A $C2WA2->$C2WA3, B stable $C2WB2)" \
  || bad "CRIT2: presse2 = control chez le parent — fenetre doit atterrir chez A seulement (A $C2WA2->$C2WA3 B $C2WB2->$C2WB3)"

log "CHEMIN CRITIQUE 2 (BASE): control local -> fenetre chez le PARENT seulement"
# le parent tient a nouveau prefix+c (control local, non delegue) : une fenetre
# creee ici doit atterrir CHEZ LE PARENT seulement (A +1, B stable). C'est le
# cas de base de CRIT2, verifie ici sans perturber la delegation (apres presse2).
C2bWA0=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
C2bWB0=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
docker exec "$A" tmux -S "$SOCK_A" new-window
sleep 1
C2bWA1=$(docker exec "$A" tmux -S "$SOCK_A" list-windows -a | wc -l | tr -d ' ')
C2bWB1=$(docker exec "$B" tmux -S "$SOCK_B" list-windows -a | wc -l | tr -d ' ')
[ "$C2bWA1" -eq "$((C2bWA0+1))" ] && [ "$C2bWB1" -eq "$C2bWB0" ] \
  && ok "CRIT2-base: prefix+c -> fenetre CHEZ LE PARENT seulement (A $C2bWA0->$C2bWA1, B stable $C2bWB0)" \
  || bad "CRIT2-base: control local = fenetre chez le parent seulement (A $C2bWA0->$C2bWA1 B $C2bWB0->$C2bWB1)"

log "CHEMIN CRITIQUE 3: presse2 SUR LE PARENT -> self-reclaim (F10 ne meurt pas)"
# symptome reel MacBook : presse1 delegue, puis l'utilisateur rappuie F10 SUR
# LE PARENT (pas dans la session ssh de l'enfant). Ancien code = F10 unbound
# + "ignored" -> prefix reste None -> F10 MORT ("marche la 1ere fois, plus la
# 2eme"). Nouveau mecanisme samoshkin : rien n'est jamais unbound, le parent
# delegue = prefix None + client en table 'off' ; son F10 (table off ou
# appel direct) le REVEILLE lui-meme : prefix restaure + table root + bar on.
# Etat de depart force : parent delegue (prefix None), independant du timing
# reseau, pour tester le self-reclaim seul.
docker exec "$A" tmux -S "$SOCK_A" set-option -g prefix None
docker exec "$A" tmux -S "$SOCK_A" set-option -g status off
echo "  etat delege force: prefix=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)"
# presse2 sur le PARENT (bar-toggle sans armed ssh, prefix=None -> self-reclaim)
docker exec "$A" sh -c "MUXMESH_HOST=$HOST_A MUXMESH_SOCKET=$SOCK_A MUXMESH_POLL_HUB=$HUB:9997 muxmesh-cli bar-toggle -v" 2>&1 | tail -2
sleep 1
C3P=$(docker exec "$A" tmux -S "$SOCK_A" show-options -gv prefix)
[ "$C3P" != "None" ] && ok "CRIT3: presse2 parent -> prefix restaure (self-reclaim, '$C3P')" || bad "CRIT3: presse2 parent -> prefix reste None, F10 MORT"
docker exec "$A" tmux -S "$SOCK_A" list-keys -T root | grep -q F10 \
  && ok "CRIT3: presse2 parent -> F10 vivant en table root" \
  || bad "CRIT3: presse2 parent -> F10 absent de root, sourd pour toujours"
assert_eq "$(docker exec "$A" tmux -S "$SOCK_A" show-options -g status | awk '{print $2}')" "on" "CRIT3: presse2 parent -> barre parent rallumee"

log "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
