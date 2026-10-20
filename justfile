# muxmesh — just: workspace recipes.

build:
    cargo build --workspace

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt

check: lint test

# run the hub locally (debug)
hub:
    cargo run --bin muxmesh-hub

# run one agent locally (debug); canonical machine name via MUXMESH_HOST
agent:
    cargo run --bin muxmesh-agent

# install: build release + place the binaries where every caller finds them.
# /usr/local/bin is on tmux run-shell's default PATH (macOS + Linux) -> F10
# works without any PATH injection. Binaries: hub (pivot), agent (machines),
# cli (driver keys + poller). Run with sudo if /usr/local/bin needs it.
install: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    DEST="${PREFIX:-/usr/local/bin}"
    mkdir -p "$DEST"
    install -m 0755 target/release/muxmesh-hub  "$DEST/muxmesh-hub"
    install -m 0755 target/release/muxmesh-agent "$DEST/muxmesh-agent"
    install -m 0755 target/release/muxmesh-cli   "$DEST/muxmesh-cli"
    echo "installed: $DEST/muxmesh-{hub,agent,cli}"

build-release:
    cargo build --release --workspace

# uninstall the installed binaries (leaves config + tokens untouched)
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    DEST="${PREFIX:-/usr/local/bin}"
    rm -f "$DEST/muxmesh-hub" "$DEST/muxmesh-agent" "$DEST/muxmesh-cli"
    echo "removed: $DEST/muxmesh-{hub,agent,cli}"
