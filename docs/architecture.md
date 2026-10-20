
## Appariement hors-ligne (`generate-token`) — pourquoi pas d'endpoint

`muxmesh-hub generate-token <host>` s'exécute **sur le pivot, en mode console**
(pas de serveur, pas de réseau) : il inscrit l'empreinte dans `tokens.json`,
imprime le token **une seule fois** sur stdout, et rend la main. Tu colles ce
token dans `~/.config/muxmesh/token` (0600) de la machine cible.

- **Aucun endpoint d'enrôlement** : le hub n'écoute que `GET /state` (libre,
  sans secret) et `POST /state` (token de machine). Une machine inconnue, un
  scanner, ou une machine compromise qui « s'invente » un `host` reçoivent
  `404`/`401` — la liste des pairs n'existe que parce que tu l'écris.
- **Conséquence assumée** : le hub ne peut pas créer de token tout seul, il
  n'a aucune autorité pour faire confiance ; c'est toi qui la lui donnes, une
  machine à la fois. `pair_key` et `/register` ont été retirés pour ça.
- **Fraîcheur des secrets** : agent et hub relisent leurs fichiers de secrets à
  chaque requête → coller ou faire tourner un token ne demande aucun
  redémarrage (piège corrigé en session : le token était lu une seule fois au
  démarrage, ce qui faisait échouer le flux « generate → colle → ça marche »).
