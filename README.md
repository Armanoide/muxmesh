# muxmesh — multi-machine tmux orchestrator over a plain HTTP pivot.

One keystroke on your driver machine opens, focuses or attaches a tmux session
on another machine — no interactive ssh, no credential-manager popups.

```
             ┌──────────────────────────────┐
             │   hub (always-on machine)    │
             │   muxmesh-hub :9997          │
             │   GET /state  POST /cmd      │
             └──▲──────────▲──────────┬─────┘
   POST /state  │   WS /ws │          │ client reads state
   (heartbeat)  │  (outbound│         │
             ┌─┴──────────┐ │dial)  ┌─┴────────────┐
             │ agent A    ├─┘       │ driver       │
             │ tmux local │         │ F10 / send   │
             └────────────┘         │ POST /cmd    │
                                    └────────────────┘
```

- **hub** — state aggregator + command dispatcher on any always-on machine. It
  never executes commands and it NEVER dials agents: agents hold a persistent
  outbound WebSocket (`/ws`) to the hub. `POST /cmd` from a driver is queued
  on the target's live WS channel (202) and pushed as a frame; the agent acks
  on the same socket.
- **agent** — runs on each server: pushes local tmux state (`POST /state`) and
  dials `ws://hub/ws`, authenticating with its OWN machine token. It serves
  NO inbound ports; commands arrive as WS frames on its own connection.
- **cli** — driver-side: badge in the tmux status bar, keys -> `POST /cmd` to
  the **hub** (operator rule: agent→hub only, never driver→agent direct).

## Design rules

- **Closed command list, encoded in the type**: commands are an externally-tagged
  serde enum (`muxmesh-core::Command`). Unknown verbs fail deserialization (400)
  before any execution. tmux is invoked via argv, never through a shell.
- **Zero ssh on the read path**: agents push state to the hub over HTTP; the
  poller badges windows from `GET /state` (open, no secrets) + local pgrep.
  No `ssh` per poll cycle, ever.
- **ONE machine token per host** (Atuin-like): `config.toml` is shareable and
  committed as a template; the token lives in `~/.config/muxmesh/token` (0600,
  gitignored). The same token authenticates `POST /state` AND the `/ws`
  handshake. Constant-time comparison; missing token = reject everything.
  No relay tokens exist anywhere in the protocol.
- **Commands flow driver → HUB → agent's socket**: the CLI POSTs `/cmd` to the
  hub with `dst: <host>` in the envelope. The hub authenticates the driver
  (same token registry as `/state`), refuses unregistered targets (403) and
  targets without a live WS channel (409 "target not connected"), otherwise
  enqueues the frame and answers `202 {"queued":true}`. The writer loop pushes
  the frame on the agent's socket (queue TTL ~10 s), the agent executes and
  acks; the hub mirrors the ack into `state.json` as `last_cmd`. The target
  never sees the driver's token — and the hub never opens a port to it.
- **LAN only**: bind on your LAN interface; never expose these ports to the WAN.
  `bind = "0.0.0.0:9997"` is correct **only if a firewall sits in front**. On
  macOS, `0.0.0.0` can resolve route-to-loopback in surprising ways: bind the
  hub to its explicit LAN address (`bind = "10.0.10.3:9997"`) so clients that
  dial the LAN IP actually reach it. The hub is plain-HTTP/WS, thread-per-
  connection, trusted LAN only — no TLS.

## Workspace

```
muxmesh/
├── muxmesh-core/   # shared contract: HostState, Command (closed list), Ack, config
├── muxmesh-hub/    # state pivot daemon (axum + tokio, /agent/ws dispatch)
├── muxmesh-agent/  # per-machine daemon (push /state, dial /ws, execute frames)
├── muxmesh-cli/    # driver client: badge poller (`poll`), state tree (`tree`), send
```

## Quick start

```sh
# on every machine
mkdir -p ~/.config/muxmesh
cp config.toml.example ~/.config/muxmesh/config.toml

# on the pivot: mint ONE machine token per host, paste it on that machine
muxmesh-hub generate-token host-a        # prints the token ONCE
# -> paste into ~/.config/muxmesh/token (0600) on host-a

just check                      # fmt + clippy -D warnings + tests
cargo run --bin muxmesh-hub    # on the always-on machine (set hub.bind in config)
MUXMESH_HOST=host-a cargo run --bin muxmesh-agent   # on each server (dials /ws)
cargo run --bin muxmesh-cli poll   # debug only: hub/agent embed the poller already

# send a closed-list command VIA THE HUB (POST /cmd to the hub; the hub queues
# it on the target's WS channel and answers 202 "queued via hub"). Auth: YOUR
# token in ~/.config/muxmesh/token (the hub's registry knows you);
# MUXMESH_CMD_TOKEN overrides. The target never sees your token.
cargo run --bin muxmesh-cli send host-a NewWindow main
MUXMESH_CMD_TOKEN=<your-token> cargo run --bin muxmesh-cli send host-a NewSession build
MUXMESH_CMD_TOKEN=<your-token> cargo run --bin muxmesh-cli send host-a FocusWindow main 2
MUXMESH_CMD_TOKEN=<your-token> cargo run --bin muxmesh-cli send host-a AttachOrDetach main
```

## F10 — one bar, nested tmux (tmux-remote model)

The same physical F10 drives whichever tmux server owns the keystroke:

- **Press F10 in a local window** → `muxmesh-cli bar-toggle` finds the armed
  host of the current window (live `ssh <host>` in a pane + host fresh on
  the hub) and POSTs `BarHide`/`BarShow` **to the hub**, which queues them on
  that machine's WS channel; the agent runs `tmux set -g status off|on` there.
  The remote bar disappears; the local badge ` REMOTE host ` stays.
- **Press F10 inside a nested ssh session** (case: `ssh host` → `tmux`) →
  the keystroke reaches the REMOTE tmux server, whose own F10 binding runs
  `bar-toggle` locally there — no network, instant.

This is exactly tmux-remote's contract (badge + one-key bar control across
ssh), reimplemented in Rust with a hub for discovery and per-machine tokens.

Verbs are a closed list (`NewWindow`, `NewSession`, `FocusWindow`, `AttachOrDetach`, `BarHide`, `BarShow`); names are whitelisted `[A-Za-z0-9_-]`; the agent only ever spawns `tmux` via argv — never a shell.

Key bindings (driver machine) — install via [tpm](https://github.com/tmux-plugins/tpm):

```tmux
set -g @plugin 'Armanoide/muxmesh'
run '~/.tmux/plugins/tpm/tpm'
```

The root [`muxmesh.tmux.conf`](muxmesh.tmux.conf) is the whole plugin: it
puts `bin/` (a launcher for the release binary) on tmux's PATH and binds
F10 to `muxmesh-cli bar-toggle` — all toggle logic (armed detection,
hidden state, BarHide/BarShow) lives in Rust, not in tmux conditionals.
F10 needs `muxmesh-cli` on PATH; the badge poller runs embedded in muxmesh-hub/muxmesh-agent (no separate service).
**F10** toggles the remote status bar (one-bar protocol, ported from the tmux-remote fork): armed window -> `BarHide` on the remote (its bar goes off, yours stays), press again -> `BarShow`. Inert on non-armed windows. New window / new session stay native (`prefix + c` / `prefix + s` pass through the ssh straight into the remote tmux).

## Status

- hub + agent + poller: working, tested.
- `/cmd` dispatch over the agent's WS (`muxmesh-cli send` -> 202 queued) + F10 one-bar toggle: working, E2E-tested (bindings parse clean; live keystroke UAT on real machines).

MIT.
