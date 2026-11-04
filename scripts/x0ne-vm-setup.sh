#!/usr/bin/env bash
# x0ne-vm-setup — zero-sudo bootstrap of a libvirt test VM for x0ne-vm-mcp.
#
# What this does:
#   1. Verifies virsh / qemu-system-x86_64 / virt-install are installed.
#   2. Warns (without sudo) if you're not in the `kvm` group — VM will
#      still boot using TCG software emulation, just slower.
#   3. Builds x0ne-vm-mcp in release mode if not already built.
#   4. Downloads Alpine Linux Virtual ISO (~50MB) → ~/.x0ne/vm/iso/
#      (cached, only downloads once).
#   5. Creates a 256MB qcow2 disk → ~/.x0ne/vm/disks/x0ne-test.qcow2
#   6. Defines + starts VM "x0ne-test" via `virsh -c qemu:///session`
#      with VNC display 127.0.0.1:N (N = first free).
#
# Re-run anytime — idempotent.
#
# After this, run:  scripts/x0ne-vm-demo.sh

set -euo pipefail

VM_NAME=${VM_NAME:-x0ne-test}
LIBVIRT_URI="qemu:///session"
X0NE_VM_DIR="${HOME}/.x0ne/vm"
ISO_DIR="${X0NE_VM_DIR}/iso"
DISK_DIR="${X0NE_VM_DIR}/disks"
LOG_DIR="${X0NE_VM_DIR}/logs"

# Alpine Virtual is the smallest Linux that boots reliably under QEMU + has
# a console you can see in a VNC window. ~50MB.
ALPINE_VERSION="3.21.3"
ALPINE_ARCH="x86_64"
ISO_URL="https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/${ALPINE_ARCH}/alpine-virt-${ALPINE_VERSION}-${ALPINE_ARCH}.iso"
ISO_PATH="${ISO_DIR}/alpine-virt-${ALPINE_VERSION}.iso"
DISK_PATH="${DISK_DIR}/${VM_NAME}.qcow2"

C_OK="\033[32m✓\033[0m"
C_WARN="\033[33m⚠\033[0m"
C_ERR="\033[31m✗\033[0m"
C_STEP="\033[1;36m▶\033[0m"

step() { echo -e "\n${C_STEP} $*"; }
ok()   { echo -e "  ${C_OK} $*"; }
warn() { echo -e "  ${C_WARN} $*"; }
err()  { echo -e "  ${C_ERR} $*" >&2; }

# ─── 1. Prereqs ──────────────────────────────────────────────────────────────

step "Checking prerequisites"

require() {
    local cmd=$1
    if command -v "$cmd" >/dev/null 2>&1; then
        ok "$cmd: $($cmd --version 2>&1 | head -1)"
    else
        err "$cmd is not installed."
        echo "    On Debian/Ubuntu:  sudo apt install $2"
        exit 1
    fi
}

require virsh "libvirt-clients"
require qemu-system-x86_64 "qemu-system-x86"
require virt-install "virtinst"
require curl "curl"
require qemu-img "qemu-utils"

# KVM acceleration is optional — TCG works fine for the demo.
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    ok "KVM acceleration available (/dev/kvm RW)"
    ACCEL="kvm"
else
    warn "No KVM access (you're not in the kvm group)."
    warn "VM will use TCG software emulation — boots in ~30s instead of ~5s."
    warn "To fix later:  sudo usermod -aG kvm \$USER  &&  newgrp kvm"
    ACCEL="tcg"
fi

# ─── 2. Cargo build ──────────────────────────────────────────────────────────

step "Building x0ne-vm-mcp (release)"

if [[ -x "${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp" ]]; then
    ok "already built: ${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
else
    cargo build -p x0ne-vm --release --bin x0ne-vm-mcp
    ok "built"
fi

# ─── 3. Directories ──────────────────────────────────────────────────────────

step "Preparing ${X0NE_VM_DIR}/"

mkdir -p "$ISO_DIR" "$DISK_DIR" "$LOG_DIR"
ok "iso dir:   $ISO_DIR"
ok "disk dir:  $DISK_DIR"
ok "log dir:   $LOG_DIR"

# ─── 4. ISO download (cached) ────────────────────────────────────────────────

step "Fetching Alpine Linux Virtual $ALPINE_VERSION ISO"

if [[ -f "$ISO_PATH" ]]; then
    size=$(stat -c %s "$ISO_PATH")
    ok "cached: $ISO_PATH (${size} bytes)"
else
    echo "  downloading $ISO_URL"
    curl --location --progress-bar --output "$ISO_PATH.tmp" "$ISO_URL"
    mv "$ISO_PATH.tmp" "$ISO_PATH"
    ok "downloaded → $ISO_PATH"
fi

# ─── 5. Disk image ───────────────────────────────────────────────────────────

step "Preparing disk image"

if [[ -f "$DISK_PATH" ]]; then
    ok "disk exists: $DISK_PATH"
else
    qemu-img create -f qcow2 "$DISK_PATH" 256M >/dev/null
    ok "created 256M qcow2: $DISK_PATH"
fi

# ─── 6. VM definition + start ───────────────────────────────────────────────

step "Provisioning VM '${VM_NAME}' via libvirt session"

# If a VM with this name exists, we keep it. To rebuild from scratch:
#   virsh -c qemu:///session destroy x0ne-test
#   virsh -c qemu:///session undefine x0ne-test
if virsh -c "$LIBVIRT_URI" list --all --name | grep -qx "$VM_NAME"; then
    ok "VM already defined: $VM_NAME"
else
    echo "  defining VM with virt-install (network=none, vnc auto-assigned)..."
    # --noautoconsole prevents virt-install from blocking us in a terminal.
    # --wait=0 returns immediately after starting.
    # --import would skip OS install — we want it to boot from the ISO.
    virt-install \
        --connect "$LIBVIRT_URI" \
        --name "$VM_NAME" \
        --memory 256 \
        --vcpus 1 \
        --cdrom "$ISO_PATH" \
        --disk path="$DISK_PATH",format=qcow2 \
        --os-variant generic \
        --network none \
        --graphics vnc,listen=127.0.0.1 \
        --noautoconsole \
        --virt-type "$ACCEL" \
        --wait 0 \
        >"${LOG_DIR}/virt-install.log" 2>&1
    ok "VM defined and starting"
fi

# Ensure it's running.
state=$(virsh -c "$LIBVIRT_URI" domstate "$VM_NAME" 2>/dev/null | tr -d '[:space:]' || true)
case "$state" in
    running)
        ok "state: running"
        ;;
    *)
        echo "  state was '$state' → starting…"
        virsh -c "$LIBVIRT_URI" start "$VM_NAME" >/dev/null 2>&1 || true
        sleep 1
        ok "state: $(virsh -c "$LIBVIRT_URI" domstate "$VM_NAME")"
        ;;
esac

# ─── 7. VNC endpoint ─────────────────────────────────────────────────────────

step "Looking up VNC endpoint"

vnc=$(virsh -c "$LIBVIRT_URI" vncdisplay "$VM_NAME" 2>/dev/null | head -1 | tr -d '[:space:]')
ok "VNC display: $vnc → 127.0.0.1:$((5900 + ${vnc##*:}))"

# ─── 8. Done ─────────────────────────────────────────────────────────────────

cat <<EOF

────────────────────────────────────────────────────────────────────
 ${C_OK} setup complete.

 Connect URI:    ${LIBVIRT_URI}
 VM:             ${VM_NAME}
 ISO:            ${ISO_PATH}
 Disk:           ${DISK_PATH}
 VNC:            ${vnc}  →  127.0.0.1:$((5900 + ${vnc##*:}))
 Accel:          ${ACCEL}

 Next:           scripts/x0ne-vm-demo.sh
                 (takes screenshots and exercises vm_screenshot /
                  vm_key / vm_click via the MCP server)

 Manual VNC:     remote-viewer vnc://127.0.0.1:$((5900 + ${vnc##*:}))
                 # or: virt-viewer --connect ${LIBVIRT_URI} ${VM_NAME}
────────────────────────────────────────────────────────────────────
EOF
