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

## Design retenu pour `register` (calqué sur dockhand/hawser, adapté)

Lecture du code de `Finsys/dockhand` (collector Go + serveur SvelteKit) : chaque
agent edge a **son** token (`hw_…`, 32 octets aléatoires) ; le serveur ne stocke
que **le hash Argon2id + les 8 premiers caractères** (lookup O(1) par préfixe,
une seule vérif au lieu de N) ; régénérer un token **ferme la connexion live et
révoque les anciens** du même environnement — une machine compromise se remplace
en un clic, l'usurpateur reste dehors.

Adaptation muxmesh (hub = fichier, pas de DB) :

```
agent neuf                    hub
  │ POST /register {host}       (X-Muxmesh-Pair: <clé d'appariement>)
  │◄── 201 {agent_token}        hub génère 32 octets, stocke hash+préfixe
  │ écrit ~/.config/muxmesh/token (0600), démarre le push normal
```

- `tokens.json` côté hub : `{host: {prefix, hash, created}}` — jamais le token en clair.
- Clé d'appariement : une chaîne que tu colles une fois par machine (dans le
  `config.toml`, champ `pair = "…"`), révoquable en changeant le fichier du hub.
- `/cmd` : le driver passe **par le hub** (le hub authentifie le driver, relaie
  avec le token de la cible) → les ports 9998 des agents peuvent fermer au LAN.
- Révocation : `hub revoke <host>` = efface l'entrée → l'agent reçoit 401 et
  s'arrête proprement, les autres machines intactes.

## Statut de cette règle

- ✅ plists hub/agent/poll écrits (RunAtLoad+KeepAlive)
- ❌ **manque** : installation launchd jamais faite sur tes machines (c'est toi qui appliques)
