#!/usr/bin/env bash
# x0ne-vm-doctor — system check.
# Tells you what's installed, what's running, what's missing, what to do.

C_OK="\033[32m✓\033[0m"; C_BAD="\033[31m✗\033[0m"; C_WARN="\033[33m·\033[0m"; C_HDR="\033[1;36m"
RESET="\033[0m"
say()  { printf '  %b %s\n' "$1" "$2"; }
hdr()  { printf '\n%b%s%b\n' "$C_HDR" "$1" "$RESET"; }
hint() { printf '      └─ %s\n' "$*"; }

# ─── core toolchain ─────────────────────────────────────────────────────────

hdr "─── core toolchain ───"
for c in virsh virt-install qemu-system-x86_64 qemu-img cargo; do
    if command -v $c >/dev/null 2>&1; then
        v=$($c --version 2>&1 | head -1)
        say "$C_OK" "$c — $v"
    else
        say "$C_BAD" "$c (missing)"
        case $c in
            virsh|virt-install) hint "apt install libvirt-clients virtinst" ;;
            qemu-system-x86_64|qemu-img) hint "apt install qemu-system-x86 qemu-utils" ;;
            cargo) hint "https://rustup.rs" ;;
        esac
    fi
done

# ─── KVM accel ──────────────────────────────────────────────────────────────

hdr "─── KVM acceleration ───"
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    say "$C_OK" "/dev/kvm accessible — VMs will boot with hardware accel"
else
    if [[ -e /dev/kvm ]]; then
        say "$C_WARN" "/dev/kvm present but not RW for you — TCG fallback (slow)"
        hint "fix: sudo usermod -aG kvm \$USER && newgrp kvm"
    else
        say "$C_BAD" "/dev/kvm missing — no virtualization support"
        hint "enable VT-x/AMD-V in BIOS, then 'sudo modprobe kvm_intel' (or kvm_amd)"
    fi
fi

# ─── x0ne-vm binary + crate state ──────────────────────────────────────────

hdr "─── x0ne-vm ───"
T="${CARGO_TARGET_DIR:-target}"
for p in "$T/release/x0ne-vm-mcp" "$T/debug/x0ne-vm-mcp" "$HOME/.cargo/bin/x0ne-vm-mcp"; do
    if [[ -x "$p" ]]; then
        say "$C_OK" "x0ne-vm-mcp at $p"
        sz=$(stat -c %s "$p")
        hint "$(numfmt --to=iec "$sz") binary"
        break
    fi
done
[[ -x "$T/release/x0ne-vm-mcp" || -x "$T/debug/x0ne-vm-mcp" || -x "$HOME/.cargo/bin/x0ne-vm-mcp" ]] || {
    say "$C_BAD" "x0ne-vm-mcp not built or installed"
    hint "cargo build -p x0ne-vm --release    (or: make install-vm)"
}

if [[ -x scripts/x0ne-vm ]]; then
    say "$C_OK" "scripts/x0ne-vm wrapper present"
fi

# ─── viewers ────────────────────────────────────────────────────────────────

hdr "─── live viewing ───"
if command -v virt-viewer >/dev/null 2>&1; then
    say "$C_OK" "virt-viewer installed (best for live sessions)"
elif command -v virt-manager >/dev/null 2>&1; then
    say "$C_WARN" "virt-manager available (GUI; opens VMs via menu)"
    hint "lightweight alt: apt install virt-viewer"
else
    say "$C_WARN" "no virt-viewer / virt-manager — use the polling watcher"
    hint "bash scripts/x0ne-vm-watch.sh  (browser-based, no extra deps)"
fi

if command -v feh >/dev/null 2>&1; then say "$C_OK" "feh (auto-reload viewer alt)"; fi
command -v xdg-open >/dev/null 2>&1 && say "$C_OK" "xdg-open available"

# ─── OCR ────────────────────────────────────────────────────────────────────

hdr "─── OCR (for vm_*_text tools) ───"
if command -v tesseract >/dev/null 2>&1; then
    v=$(tesseract --version 2>&1 | head -1 | tr -d '\n')
    say "$C_OK" "$v"
    if tesseract --list-langs 2>&1 | grep -qE "^eng$"; then
        say "$C_OK" "eng lang pack installed"
    else
        say "$C_WARN" "no 'eng' lang pack"
        hint "apt install tesseract-ocr-eng"
    fi
else
    say "$C_WARN" "tesseract missing — vm_find_text / vm_click_text / vm_wait_for_text will fail"
    hint "apt install tesseract-ocr tesseract-ocr-eng"
fi

# ─── Wayland backend prereqs ───────────────────────────────────────────────

hdr "─── Wayland backend prereqs (optional) ───"
for c in sway grim wlrctl; do
    if command -v $c >/dev/null 2>&1; then
        say "$C_OK" "$c"
    else
        say "$C_WARN" "$c (Wayland backend will fail to spawn $c subprocess)"
    fi
done

# ─── running VMs ────────────────────────────────────────────────────────────

hdr "─── VMs (qemu:///session) ───"
out=$(virsh -c qemu:///session list --all 2>&1)
echo "$out" | sed 's/^/      /'

# ─── final ─────────────────────────────────────────────────────────────────

hdr "─── next steps ───"
echo "      • create a tiny test VM:   bash scripts/x0ne-vm-setup.sh"
echo "      • create a Firefox VM:     bash scripts/x0ne-vm-firefox-setup.sh"
echo "      • drive any running VM:    bash scripts/x0ne-vm help"
echo "      • run control test:        bash scripts/x0ne-vm-control-test.sh"
echo "      • browser-based live view: bash scripts/x0ne-vm-watch.sh"
echo
