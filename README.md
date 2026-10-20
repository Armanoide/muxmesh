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
- **LAN only**: bind on your LAN interface; never expose these ports to the WAN. The hub is a plain-HTTP, thread-per-connection server with a shared static token — trusted LAN only, no TLS, no per-client identity.

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

Key bindings: source [`misc/muxmesh.tmux.conf`](misc/muxmesh.tmux.conf) on the driver machine — **F10** toggles the remote status bar (one-bar protocol, ported from the tmux-remote fork): armed window -> `BarHide` on the remote (its bar goes off, yours stays), press again -> `BarShow`. Inert on non-armed windows. New window / new session stay native (`prefix + c` / `prefix + s` pass through the ssh straight into the remote tmux).

## Status

- hub + agent + poller: working, tested (43 tests).
- `/cmd` execution on the agent (`muxmesh-cli send`) + F10 one-bar toggle: working, E2E-tested (bindings parse clean; live keystroke UAT on real machines).

MIT.
