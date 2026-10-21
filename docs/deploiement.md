# muxmesh — déploiement

> [!warning] Mode actuel : manuel, jusqu'à stabilisation (décision Norbert 2026-10-21)
> Hub et agent se lancent **à la main** sur chaque machine. Pas de launchd ni
> systemd tant que la chaîne F10 n'est pas stable en UAT. Les démons qui
> meurent quand on ferme le terminal sont un comportement ASSUMÉ à ce stade.
> Le passage en services (launchd/systemd) se fera quand tout sera vert.

## Qui lance quoi (et pourquoi)

| Machine | Rôles | Lancé par (manuel, terminal dédié) |
|---|---|---|
| Pivot (Mini aujourd'hui, node1/NAS demain) | `muxmesh-hub` + `muxmesh-agent` (hub et agent cohabitent) | `muxmesh-hub -v >> ~/.muxmesh/hub.log 2>&1` puis `muxmesh-agent -v >> ~/.muxmesh/agent.log 2>&1` |
| MacBook (driver) | tmux (bindings F10) + poller embarqué dans l'agent | `muxmesh-agent` (le poller est dedans) + tmux au login |
| Autres serveurs | `muxmesh-agent` | idem, terminal ou `nohup` |

Le **poll** n'a jamais à être lancé à la main : il est embarqué dans hub et
agent. Le **badge** se met tout seul dès que l'agent du destinataire tourne.



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
mm_9f3a…(43 car., token MACHINE) — affiché UNE SEULE FOIS
   │ tokens.json : {host: {prefix, hash, created}} — jamais le clair
   └──────────── tu colles le token MACHINE, une fois ─────►  ~/.config/muxmesh/token (0600)
                                                              l'agent s'en sert pour POST /state
                                                              ET pour sa poignée de main /ws
```

- **UN seul token par machine** : le même secret authentifie `POST /state` et
  la connexion WebSocket sortante `/ws`. Aucun secret de relais n'existe plus :
  le hub ne signe rien vers l'agent, il pousse sur le socket que l'agent a
  lui-même ouvert. `generate-token` n'affiche donc qu'UNE valeur.
- **Une machine compromise se remplace en régénérant son token**
  (`generate-token <host>` révoque l'ancien immédiatement), les autres intactes.
- **Le hub ne stocke jamais le token** : hash SHA-256 itéré ×100 000 (sel aléatoire)
  + préfixe 8 car. pour le lookup. Vol de `tokens.json` ne donne pas les tokens.
- **Rotation sans redémarrage** : l'agent relit son token à chaque battement et
  à chaque reconnexion WS — coller un nouveau token dans `token` suffit.
- `/cmd` : le driver parle **au hub** (`POST /cmd`, `dst: <host>` dans le
  corps) ; le hub authentifie le driver (même registre que `/state`), refuse
  une cible non enregistrée (`403 dst not registered`) ou sans WebSocket actif
  (`409 target not connected`), sinon met la trame en file et répond
  `202 {"queued":true}` (TTL file ~10 s). Le hub **ne joint JAMAIS l'agent** :
  la trame part sur le `/ws` que l'agent maintient en permanence (reconnexion
  auto toutes les 2 s), et l'agent accuse réception (ack) sur le même socket —
  le hub miroir l'ack dans `state.json` (`last_cmd`). Jamais de port entrant
  sur l'agent, jamais de driver→agent direct (décision opérateur 2026-10-21).
  LAN only, verbes fermés.
- Révocation : supprimer l'entrée de `tokens.json` (ou régénérer) → la machine
  reçoit 401 et s'arrête proprement, les autres intactes.

> Note de design : calqué sur `Finsys/dockhand` (token par agent, hash+préfixe
> côté serveur, régénération = révocation), simplifié pour un hub sans DB.
> dockhand utilise Argon2id ; ici SHA-256 itéré ×100 000 en std seul —
> hors menaces GPU/offline, qui exigeraient un vrai KDF.

## Statut de cette règle

- Mode manuel ASSUMÉ tant que l'UAT F10 n'est pas stable (launchd/systemd retirés du repo le 2026-10-21).
- À faire quand tout sera vert : réécrire plists + unit systemd et basculer.

## Pare-feu macOS — le piège du port 9997 (symptôme réel)

`muxmesh-agent: push=ok` sur le client alors que `state.json` du hub reste
vide, et `curl http://<lan-ip>:9997/state` qui **expire** même depuis le
pivot lui-même (alors que `127.0.0.1:9997` répond) : c'est le **pare-feu
macOS** qui drop silencieusement le port entrant. Le firewall application
peut être affiché « disabled » et filtrer quand même une règle résiduelle.

Diagnostic en 3 commandes (depuis le pivot) :
```bash
curl -s --max-time 3 http://127.0.0.1:9997/state   # répond -> hub vivant
curl -s --max-time 3 http://<lan-ip>:9997/state     # expire -> filtrage LAN
/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate
```
Fix : autoriser le port (ou le binaire) dans Pare-feu → Options, ou
`sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add <binaire>` puis
relancer le hub. Ne jamais couper le pare-feu pour faire passer un test.

## Logs et verbosité (pattern lm-sanaga)

Niveaux `tracing` : ERROR / WARN / INFO (défaut) / DEBUG.
- Sans option : uniquement l’essentiel (démarrage, `push=ok`, erreurs).
- `-v` ou `--verbose` (hub, agent, cli) : équivalent `RUST_LOG=muxmesh=debug`
  — chaque requête GET/POST /state, chaque décision bar-toggle, chaque appel tmux raté.
- `MUXMESH_DEBUG=true` : debug global sans toucher aux args.
- `RUST_LOG` explicite gagne toujours (`RUST_LOG=muxmesh_hub=trace,...`).

Exemples :
```bash
muxmesh-hub -v            # trace toutes les requetes sur le pivot
MUXMESH_DEBUG=true muxmesh-agent
RUST_LOG=muxmesh_cli=debug muxmesh-cli bar-toggle   # pourquoi F10 ne fait rien ?
```
