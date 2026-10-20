# muxmesh — déploiement : zéro commande à la main

Règle : **aucun composant ne se lance à la main** au quotidien. Chaque machine
déclare ses services au gestionnaire d'init du système ; lui les démarre au
boot, les redémarre s'ils meurent. `./muxmesh-hub` à la main = uniquement le
debug ponctuel, jamais l'usage normal.

## Qui lance quoi (et pourquoi)

| Machine | Rôles | Lancé par |
|---|---|---|
| Pivot (Mini aujourd'hui, node1/NAS demain) | `muxmesh-hub` + `muxmesh-agent` (hub et agent cohabitent) | launchd (`com.muxmesh.hub.plist` + `com.muxmesh.agent.plist`) |
| MacBook (driver) | `muxmesh-cli poll` + tmux (bindings F10) | launchd (`com.muxmesh.poll.plist`) + tmux au login |
| Autres serveurs | `muxmesh-agent` | systemd unit (modèle ci-dessous) |

Le **poll** n'a pas à être lancé à la main : c'est un service du driver, comme
le hub l'est du pivot. Le **badge** se met tout seul dès que l'agent du
destinataire tourne — c'est le design : tu ouvres ton terminal, `ssh`, badge.

## macOS (Mini + MacBook)

Les plists existent (`launchd/`). Installation sur chaque machine :

```bash
# Mini (pivot) : hub + agent
mkdir -p ~/.muxmesh
sed 's/USER/norbert/g' launchd/com.muxmesh.hub.plist  > ~/Library/LaunchAgents/com.muxmesh.hub.plist
sed 's/USER/norbert/g' launchd/com.muxmesh.agent.plist > ~/Library/LaunchAgents/com.muxmesh.agent.plist
launchctl load ~/Library/LaunchAgents/com.muxmesh.hub.plist
launchctl load ~/Library/LaunchAgents/com.muxmesh.agent.plist

# MacBook (driver) : poll + tmux
sed 's/USER/norbert/g' launchd/com.muxmesh.poll.plist > ~/Library/LaunchAgents/com.muxmesh.poll.plist
launchctl load ~/Library/LaunchAgents/com.muxmesh.poll.plist
```

`RunAtLoad + KeepAlive` = démarre au login, redémarre s'il meurt. Rien d'autre à faire.

## Linux (node1/NAS demain) — modèle systemd

```ini
# /etc/systemd/system/muxmesh-hub.service  (pivot)
[Unit]
Description=muxmesh hub (state pivot)
After=network-online.target

[Service]
ExecStart=/usr/local/bin/muxmesh-hub
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```
```bash
systemctl enable --now muxmesh-hub   # une seule fois, puis oublié
```

## Ce qui reste manuel (assumé, court)

1. **Premier déploiement par machine** : copier le binaire + config + token (ou `register`, voir note de design à venir).
2. Debug ponctuel : lancer un composant à la main pour lire ses logs.

## Statut de cette règle

- ✅ plists hub/agent/poll écrits (RunAtLoad+KeepAlive)
- ❌ **manque** : installation launchd jamais faite sur tes machines (c'est toi qui appliques)
