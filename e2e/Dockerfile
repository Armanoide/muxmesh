# E2E test image: builds the real muxmesh binaries (hub/agent/cli) on Linux
# and runs tmux inside. One image, three roles (hub / agent machine_b /
# agent machine_a) selected by command at run time. NOT for macOS deploy — the
# Linux binary is a test artifact only; each real machine builds natively.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
# toolchain + tmux needed to compile and run the workspace.
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux procps openssh-client ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --workspace

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux procps openssh-client openssh-server ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/muxmesh
COPY --from=build /src/target/release/muxmesh-hub /usr/local/bin/
COPY --from=build /src/target/release/muxmesh-agent /usr/local/bin/
COPY --from=build /src/target/release/muxmesh-cli /usr/local/bin/
# tpm-style plugin entry + sshd bootstrap so the child accepts a REAL ssh
# from the driver (no simulated ssh process — the poller sees a genuine one).
COPY muxmesh.tmux /opt/muxmesh/muxmesh.tmux
COPY e2e/sshd-setup.sh /opt/muxmesh/sshd-setup.sh
RUN chmod +x /opt/muxmesh/*.sh /usr/local/bin/muxmesh-*
ENTRYPOINT []
