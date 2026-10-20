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

1. **Premier déploiement par machine** : copier le binaire + config, puis appairer avec `generate-token` (voir section suivante).
2. Debug ponctuel : lancer un composant à la main pour lire ses logs.

## Appariement d'une machine (`generate-token`)

Le hub ne connaît que ce que **toi** tu lui déclares. Pas d'auto-registration :
une machine inconnue ne peut pas se présenter au hub (le endpoint `/register`
n'existe pas — un scanner qui le trouve reçoit `404`).

```
Mac Mini (pivot)                          machine à appairer
$ muxmesh-hub generate-token 10.0.10.7
mm_9f3a…(43 car., affiché UNE SEULE FOIS)
   │ tokens.json : {host: {prefix, hash, created}} — jamais le clair
   └──────────── tu colles le token, une fois ────────────►  ~/.config/muxmesh/token (0600)
                                                            l'agent le relit à chaque push,
                                                            aucun redémarrage nécessaire
```

- **Token par machine** : une machine compromise se remplace en régénérant son
  token (`generate-token <host>` révoque l'ancien immédiatement), les autres intactes.
- **Le hub ne stocke jamais le secret** : hash SHA-256 itéré ×100 000 (sel aléatoire)
  + préfixe 8 car. pour le lookup. Vol de `tokens.json` ne donne pas les tokens.
- **Rotation sans redémarrage** : agent et hub relisent leurs fichiers de secrets
  à chaque requête — coller un nouveau token dans `token` suffit.
- `/cmd` : le driver passe **par le hub** (le hub authentifie le driver, relaie
  avec le token de la cible) → les ports 9998 des agents peuvent fermer au LAN.
- Révocation : supprimer l'entrée de `tokens.json` (ou régénérer) → la machine
  reçoit 401 et s'arrête proprement, les autres intactes.

> Note de design : calqué sur `Finsys/dockhand` (token par agent, hash+préfixe
> côté serveur, régénération = révocation), simplifié pour un hub sans DB.
> dockhand utilise Argon2id ; ici SHA-256 itéré ×100 000 en std seul —
> hors menaces GPU/offline, qui exigeraient un vrai KDF.

## Statut de cette règle

- ✅ plists hub/agent/poll écrits (RunAtLoad+KeepAlive)
- ❌ **manque** : installation launchd jamais faite sur tes machines (c'est toi qui appliques)
