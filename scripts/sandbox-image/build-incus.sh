#!/usr/bin/env bash
# Build the tooled Incus image once, as local alias x0ne-sbx-tools.
# Same provisioning as the Podman image, on images:debian/13.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
alias=${X0NE_SBX_TOOLS_ALIAS:-x0ne-sbx-tools}
name=x0ne-sbx-build-$$
cleanup() { incus delete --force "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM
incus launch images:debian/13/amd64 "$name" -c security.privileged=false >/dev/null
for _ in $(seq 1 30); do incus exec "$name" -- true 2>/dev/null && break; sleep 1; done
# Wait for the network: provisioning needs apt and rustup.
for _ in $(seq 1 30); do incus exec "$name" -- getent hosts deb.debian.org >/dev/null 2>&1 && break; sleep 1; done
incus file push -q "$here/provision.sh" "$name/tmp/provision.sh"
incus exec "$name" -- sh /tmp/provision.sh
incus exec "$name" -- rm -f /tmp/provision.sh
incus stop "$name"
if incus image info "$alias" >/dev/null 2>&1; then incus image alias delete "$alias"; fi
incus publish "$name" --alias "$alias" >/dev/null
echo "built incus image alias $alias"
