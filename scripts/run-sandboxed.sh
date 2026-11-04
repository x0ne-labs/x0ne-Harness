#!/usr/bin/env bash
# Run x0ne (the TUI and its runtime) inside a throwaway Incus or Podman container.
# Usage: scripts/run-sandboxed.sh incus|podman [x0ne args...]
#
# Mounted: the release binaries x0ne and x0ne-runtime (read-only, /opt/x0ne) and
# one work dir (X0NE_WORK_DIR, default ~/x0ne-work, read-write, /work). Nothing
# else from the host: no $HOME, ~/.ssh, ~/.config, no podman or incus socket.
# Undo store: X0NE_SNAPSHOT_DIR (default <work dir>.x0ne-snapshots),
# read-write at /x0ne-snapshots, outside /work so `rm -rf /work` spares it.
# Same filesystem as the work dir keeps snapshots as reflinks on btrfs/XFS.
# Unprivileged, no nesting, CPU/memory/process limits; removed on exit.
#
# Secrets: X0NE_SANDBOX_ENV (default ~/.config/x0ne-sandbox.env), KEY=VALUE lines,
# mode 0600, owned by you. Never on a command line: podman reads it with
# --env-file, incus receives it on stdin (`incus file push -`).
# The key is in the x0ne process environment inside the container, so code the
# agent runs there (same uid) can read it. Use a key scoped to this use.
#
# Network: no route out by default. The only egress is "<name>-proxy",
# a tinyproxy opening onto an allowlist (LLM provider, crates.io, PyPI,
# deb.debian.org, github.com): see scripts/sandbox-egress.sh for the knobs.
# Podman: the sandbox sits on an --internal network. Incus: no NIC, the proxy
# is reached through a proxy device. X0NE_SANDBOX_NET=open restores NAT egress.
#
# Knobs: X0NE_BIN_DIR (default ./target/release), X0NE_SBX_CPUS (2),
# X0NE_SBX_MEMORY (4GiB), X0NE_SBX_PIDS (512), X0NE_SBX_NAME (x0ne-sbx-<pid>).
# X0NE_SBX_TEST_CMD: run this shell command (as the agent user, cwd /work)
# instead of x0ne. For scripts/test-run-sandboxed.sh.
set -euo pipefail

backend=${1:-}
case "$backend" in
    incus|podman) shift ;;
    *) echo "usage: $0 incus|podman [x0ne args...]" >&2; exit 2 ;;
esac

die() { echo "run-sandboxed: $*" >&2; exit 1; }
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=sandbox-egress.sh
. "$here/sandbox-egress.sh"
net_mode=$(egress_mode) || exit 2

# Pins, Debian 13 (glibc 2.41): the binaries are built on the host, and a
# Debian 13 build needs GLIBC_2.39, absent from Debian 12 (2026-10-06).
# The tooled image (make sandbox-image: rust, python, build-essential, git,
# tmux) is used when it exists. Incus: images:debian/13 (fingerprint printed).
# Podman: trixie-slim of 2026-09-18, by digest.
INCUS_TOOLS=${X0NE_SBX_TOOLS_ALIAS:-x0ne-sbx-tools}
PODMAN_TOOLS=localhost/x0ne-sbx-tools:trixie
if [ -z "${X0NE_SBX_INCUS_IMAGE:-}" ] && command -v incus >/dev/null && incus image info "$INCUS_TOOLS" >/dev/null 2>&1; then
    X0NE_SBX_INCUS_IMAGE=$INCUS_TOOLS
fi
if [ -z "${X0NE_SBX_PODMAN_IMAGE:-}" ] && command -v podman >/dev/null && podman image exists "$PODMAN_TOOLS" 2>/dev/null; then
    X0NE_SBX_PODMAN_IMAGE=$PODMAN_TOOLS
fi
INCUS_IMAGE=${X0NE_SBX_INCUS_IMAGE:-images:debian/13/amd64}
PODMAN_IMAGE=${X0NE_SBX_PODMAN_IMAGE:-docker.io/library/debian@sha256:7792b1f7702a86946cd518db72b6a407302c3e9bc1635634368b878189e8221c}

# Not secrets, so they may ride on the command line: the sandbox declaration
# (x0ne runs yolo in it unless X0NE_SANDBOX_CONFIRM=1) and the host's model
# choice (X0NE_MODEL set on the host never reached the sandbox).
fwd=("X0NE_SANDBOX=$backend")
for v in X0NE_LLM_PROVIDER X0NE_MODEL X0NE_LLM_MODEL X0NE_SANDBOX_CONFIRM; do
    if [ -n "${!v:-}" ]; then
        [[ "${!v}" =~ ^[A-Za-z0-9._:/-]+$ ]] || die "$v has unexpected characters"
        fwd+=("$v=${!v}")
    fi
done

bin_dir=$(realpath "${X0NE_BIN_DIR:-${CARGO_TARGET_DIR:-./target}/release}")
work_dir=${X0NE_WORK_DIR:-$HOME/x0ne-work}
env_file=${X0NE_SANDBOX_ENV:-$HOME/.config/x0ne-sandbox.env}
snap_dir=${X0NE_SNAPSHOT_DIR:-}
cpus=${X0NE_SBX_CPUS:-2}
memory=${X0NE_SBX_MEMORY:-4GiB}
pids=${X0NE_SBX_PIDS:-512}
name=${X0NE_SBX_NAME:-x0ne-sbx-$$}
uid=$(id -u)
gid=$(id -g)

for b in x0ne x0ne-runtime; do
    [ -x "$bin_dir/$b" ] || die "missing $bin_dir/$b (build: make build, or set X0NE_BIN_DIR)"
done
[[ "$name" =~ ^x0ne-sbx-[a-z0-9-]+$ ]] || die "container name must match x0ne-sbx-[a-z0-9-]+"

mkdir -p "$work_dir/.x0ne-home"
chmod 700 "$work_dir" "$work_dir/.x0ne-home"
work_dir=$(realpath "$work_dir")
case "$work_dir" in
    "$HOME"|/|"$HOME/.ssh"*|"$HOME/.config"*|"$HOME/.gnupg"*) die "refusing work dir $work_dir" ;;
esac
snap_dir=${snap_dir:-$work_dir.x0ne-snapshots}
mkdir -p "$snap_dir"
chmod 700 "$snap_dir"
snap_dir=$(realpath "$snap_dir")
case "$snap_dir" in
    "$work_dir"|"$work_dir"/*) die "X0NE_SNAPSHOT_DIR must be outside the work dir" ;;
    "$HOME"|/|"$HOME/.ssh"*|"$HOME/.config"*|"$HOME/.gnupg"*) die "refusing snapshot dir $snap_dir" ;;
esac

if [ -e "$env_file" ]; then
    [ "$(stat -c %u "$env_file")" = "$uid" ] || die "$env_file is not owned by you"
    [ "$(stat -c %a "$env_file")" = 600 ] || die "$env_file must be mode 0600"
    case "$(realpath "$env_file")" in "$(realpath .)"/*) die "$env_file is inside the repository" ;; esac
else
    echo "run-sandboxed: no $env_file; starting without API keys" >&2
    env_file=/dev/null
fi

agent_cmd=${X0NE_SBX_TEST_CMD:-}
if [ "$net_mode" = open ]; then
    egress_warn_open
    fwd+=("X0NE_SANDBOX_NET=open")
fi

# The runtime clears a command's environment down to an allowlist without
# RUSTUP_HOME, so rustup looked in ~/.rustup and found no toolchain: cargo
# exited 1 (2026-10-06). ~/.rustup points at the image's install.
rustup_link='if [ -d /usr/local/rustup ] && [ ! -e "$HOME/.rustup" ] && [ ! -L "$HOME/.rustup" ]; then
        ln -s /usr/local/rustup "$HOME/.rustup"; fi'

run_incus() {
    command -v incus >/dev/null || die "incus CLI not found"
    cleanup() { incus delete --force "$name" >/dev/null 2>&1 || true; egress_down "$name"; }
    trap cleanup EXIT INT TERM
    incus init "$INCUS_IMAGE" "$name" \
        -c security.privileged=false -c security.nesting=false \
        -c limits.cpu="$cpus" -c limits.memory="$memory" -c limits.processes="$pids" \
        >/dev/null
    if [ "$net_mode" = proxy ]; then
        # No NIC: a "none" device masks the profile's eth0. The proxy device
        # listens inside the instance and connects from the host's loopback.
        egress_up "$name" "$env_file" loopback || die "egress proxy failed to start"
        incus config device add "$name" eth0 none >/dev/null
        incus config device add "$name" egress proxy bind=instance \
            listen=tcp:127.0.0.1:3128 connect="tcp:127.0.0.1:$(egress_host_port "$name")" >/dev/null
        while IFS= read -r v; do fwd+=("$v"); done < <(egress_env 127.0.0.1:3128)
    fi
    echo "run-sandboxed: image $(incus config get "$name" volatile.base_image)" >&2
    # shift=true: idmapped mount, so files keep your host uid inside and out.
    incus config device add "$name" x0ne-bin disk source="$bin_dir" path=/opt/x0ne readonly=true shift=true >/dev/null
    incus config device add "$name" x0ne-work disk source="$work_dir" path=/work shift=true >/dev/null
    incus config device add "$name" x0ne-snapshots disk source="$snap_dir" path=/x0ne-snapshots shift=true >/dev/null
    incus start "$name"
    incus exec "$name" -- sh -c "getent group $gid >/dev/null || groupadd -g $gid agent; \
        id -u $uid >/dev/null 2>&1 || useradd -m -u $uid -g $gid -s /bin/sh agent"
    incus exec "$name" -- install -d -m 0700 -o "$uid" -g "$gid" /run/x0ne-agent
    incus file push -q --mode 0600 --uid "$uid" --gid "$gid" - "$name/run/x0ne-agent/env" <"$env_file"
    local inner='set -a; . /run/x0ne-agent/env; set +a; rm -f /run/x0ne-agent/env
        export PATH=/opt/x0ne:$PATH
        '"$rustup_link"'
        if [ -n "${X0NE_SBX_TEST_CMD:-}" ]; then exec sh -c "$X0NE_SBX_TEST_CMD"; fi
        exec x0ne --isolation container "$@"'
    local tty=-t
    [ -n "$agent_cmd" ] && tty=-T
    local envs=() v
    for v in "${fwd[@]}" RUSTUP_HOME=/usr/local/rustup; do envs+=(--env "$v"); done
    incus exec "$name" $tty --user "$uid" --group "$gid" --cwd /work \
        --env HOME=/work/.x0ne-home --env X0NE_SBX_TEST_CMD="$agent_cmd" "${envs[@]}" \
        -- sh -c "$inner" sh "$@"
}

run_podman() {
    command -v podman >/dev/null || die "podman not found"
    local tty=-it envs=() v
    [ -n "$agent_cmd" ] && tty=-i
    local net=()
    if [ "$net_mode" = proxy ]; then
        trap 'egress_down "$name"' EXIT INT TERM
        egress_up "$name" "$env_file" internal || die "egress proxy failed to start"
        net=(--network "$name-int")
        while IFS= read -r v; do envs+=(-e "$v"); done < <(egress_env "x0ne-egress:$EGRESS_PORT")
    fi
    for v in "${fwd[@]}"; do envs+=(-e "$v"); done
    # apt stays unavailable here: --read-only, cap-drop=ALL and
    # no-new-privileges block sudo. Use run-incus to install packages.
    podman run --rm $tty --name "$name" "${net[@]}" \
        --userns=keep-id --user "$uid:$gid" \
        --cap-drop=ALL --security-opt no-new-privileges \
        --read-only --tmpfs /tmp:rw,nosuid,nodev,size=512m \
        --cpus "$cpus" --memory "${memory/GiB/g}" --pids-limit "$pids" \
        --env-file "$env_file" \
        -v "$bin_dir/x0ne:/opt/x0ne/x0ne:ro" -v "$bin_dir/x0ne-runtime:/opt/x0ne/x0ne-runtime:ro" \
        -v "$work_dir:/work:rw" -w /work -v "$snap_dir:/x0ne-snapshots:rw" \
        -e HOME=/work/.x0ne-home -e PATH=/opt/x0ne:/usr/local/bin:/usr/bin:/bin \
        -e X0NE_SBX_TEST_CMD="$agent_cmd" "${envs[@]}" \
        "$PODMAN_IMAGE" \
        sh -c "$rustup_link"'
               if [ -n "$X0NE_SBX_TEST_CMD" ]; then exec sh -c "$X0NE_SBX_TEST_CMD"; fi
               exec x0ne --isolation container "$@"' sh "$@"
}

"run_$backend" "$@"
