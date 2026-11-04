#!/bin/sh
# Tools for the run-incus / run-podman sandbox: Rust stable, Python,
# build-essential, git, tmux, iproute2. No nmap. Runs as root, once, at
# image build time (Containerfile, or `make sandbox-image-incus`).
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl git tmux iproute2 iputils-ping procps tree less \
    build-essential pkg-config libssl-dev \
    python3 python3-pip python3-venv \
    sudo
# Rust stable for every user: toolchains under /usr/local, read-only to them;
# each user's registry cache goes to their own ~/.cargo.
export RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
    --default-toolchain stable --component rustfmt,clippy
chmod -R a+rX /usr/local/rustup /usr/local/cargo
for b in /usr/local/cargo/bin/*; do ln -sf "$b" /usr/local/bin/; done
cat > /etc/profile.d/x0ne-rust.sh <<'P'
export RUSTUP_HOME=/usr/local/rustup
P
# apt for the agent, and nothing else as root. Inside an unprivileged
# container root is not the host's root; apt hooks can still run code as
# container root, which is accepted here: the container is the boundary.
printf 'ALL ALL=(root) NOPASSWD: /usr/bin/apt-get, /usr/bin/apt\n' > /etc/sudoers.d/x0ne-apt
chmod 0440 /etc/sudoers.d/x0ne-apt
apt-get clean
rm -rf /var/lib/apt/lists/*
