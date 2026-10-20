# muxmesh — multi-machine tmux orchestrator over a plain HTTP pivot.

One keystroke on your driver machine opens, focuses or attaches a tmux session
on another machine — no interactive ssh, no credential-manager popups.

```
            ┌────────────────────────────┐
            │  hub (always-on machine)   │
            │  muxmesh-hub :9997         │
            │  POST /state  GET /state   │
            └───────▲──────────┬─────────┘
     agents push  │          │  client reads state
            ┌─────┴────┐  ┌───┴──────────┐
            │ agent A  │  │ driver       │
            │ tmux     │◄─│ F10 bar toggle │
            │ local    │  │ POST /cmd      │
            └──────────┘  └────────────────┘
```

- **hub** — state aggregator on any always-on machine. It never executes anything.
- **agent** — runs on each server: pushes local tmux state, serves a closed list of `/cmd` actions.
- **cli** — driver-side: badge in the tmux status bar, keys -> `POST /cmd`.

## Design rules

- **Closed command list, encoded in the type**: commands are an externally-tagged
  serde enum (`muxmesh-core::Command`). Unknown verbs fail deserialization (400)
  before any execution. tmux is invoked via argv, never through a shell.
- **Zero network on the read path**: agents push state to the hub; the status bar
  reads a local file. No `ssh` per poll cycle, ever.
- **Shared token** (Atuin-like): `config.toml` is shareable and committed as a
  template; the token lives in `~/.config/muxmesh/token` (0600, gitignored).
  Constant-time comparison; missing token = reject everything.
- **LAN only**: bind on your LAN interface; never expose these ports to the WAN.
  `bind = "0.0.0.0:9997"` is correct **only if a firewall sits in front**. On
  macOS, `0.0.0.0` can resolve route-to-loopback in surprising ways: bind the
  hub to its explicit LAN address (`bind = "10.0.10.3:9997"`) so clients that
  dial the LAN IP actually reach it. The hub is plain-HTTP, thread-per-connection,
  trusted LAN only — no TLS.

## Workspace

```
muxmesh/
├── muxmesh-core/   # shared contract: HostState, Command (closed list), Ack, config
├── muxmesh-hub/    # state pivot daemon (std::net only, offline build)
├── muxmesh-agent/  # per-machine daemon (push /state, serve /cmd)
├── muxmesh-cli/    # driver client (placeholder, brique 3)
├── muxmesh-cli/    # driver client: badge poller (`poll`), state tree (`tree`), keys (brique 3)
└── launchd/        # macOS service templates (Linux: systemd units)
```

## Quick start

```sh
# on every machine
mkdir -p ~/.config/muxmesh
cp config.toml.example ~/.config/muxmesh/config.toml
openssl rand -hex 32 > ~/.config/muxmesh/token && chmod 600 ~/.config/muxmesh/token

just check                      # fmt + clippy -D warnings + tests
cargo run --bin muxmesh-hub    # on the always-on machine (set hub.bind in config)
MUXMESH_HOST=host-a cargo run --bin muxmesh-agent   # on each server
cargo run --bin muxmesh-cli poll   # on the driver machine (tmux badge)

# send a closed-list command to a remote agent (POST /cmd, direct client->agent)
cargo run --bin muxmesh-cli send host-a NewWindow main
cargo run --bin muxmesh-cli send host-a NewSession build
cargo run --bin muxmesh-cli send host-a FocusWindow main 2
cargo run --bin muxmesh-cli send host-a AttachOrDetach main
```

Verbs are a closed list (`NewWindow`, `NewSession`, `FocusWindow`, `AttachOrDetach`, `BarHide`, `BarShow`); names are whitelisted `[A-Za-z0-9_-]`; the agent only ever spawns `tmux` via argv — never a shell.

Key bindings (driver machine) — install via [tpm](https://github.com/tmux-plugins/tpm):

```tmux
set -g @plugin 'Armanoide/muxmesh'
run '~/.tmux/plugins/tpm/tpm'
```

The root [`muxmesh.tmux.conf`](muxmesh.tmux.conf) is the tpm entry hook; it
forwards to [`misc/muxmesh.tmux.conf`](misc/muxmesh.tmux.conf) (the repo root
belongs to Cargo). Without tpm, source `misc/muxmesh.tmux.conf` directly.
F10 needs `muxmesh-cli` on PATH and the poller running (`muxmesh-cli poll`).
**F10** toggles the remote status bar (one-bar protocol, ported from the tmux-remote fork): armed window -> `BarHide` on the remote (its bar goes off, yours stays), press again -> `BarShow`. Inert on non-armed windows. New window / new session stay native (`prefix + c` / `prefix + s` pass through the ssh straight into the remote tmux).

## Status

- hub + agent + poller: working, tested (43 tests).
- `/cmd` execution on the agent (`muxmesh-cli send`) + F10 one-bar toggle: working, E2E-tested (bindings parse clean; live keystroke UAT on real machines).

MIT.
