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
