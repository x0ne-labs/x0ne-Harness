#!/usr/bin/env bash
# Stop the Mission Control stack — daemon + dashboard. Cleanly destroys
# all pool VMs via the daemon's shutdown command before killing it.

DIR="$HOME/.x0ne/vm"
SOCK="$DIR/vmd.sock"

echo "▶ asking x0ne-vmd to shut down (destroys all pool VMs)..."
if [[ -S "$SOCK" ]]; then
    echo '{"cmd":"shutdown"}' | timeout 3 socat - "UNIX-CONNECT:$SOCK" 2>/dev/null | head -1
    sleep 1
fi

echo "▶ ensuring processes are gone..."
# By PID, matched on the exact executable or script path: `pkill -f` also
# matched any shell whose command line merely contained the pattern.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
for pid in $(ls /proc | grep -E '^[0-9]+$'); do
    [[ "$pid" == "$$" ]] && continue
    exe="$(readlink "/proc/$pid/exe" 2>/dev/null)" || continue
    argv1="$(tr '\0' '\n' < "/proc/$pid/cmdline" 2>/dev/null | sed -n 2p)"
    [[ "$argv1" == /* ]] || argv1="$(readlink "/proc/$pid/cwd" 2>/dev/null)/$argv1"
    if [[ "$exe" == "${CARGO_TARGET_DIR:-$ROOT/target}/release/x0ne-vmd" ]] \
        || [[ "$exe" == */python3* && "$argv1" -ef "$ROOT/scripts/x0ne-vm-mission-control.py" ]]; then
        kill "$pid" 2>/dev/null && echo "  stopped pid $pid"
    fi
done
sleep 0.5

# Cleanup any pool-* VMs that survived (paranoia)
for n in $(virsh -c qemu:///session list --all --name 2>/dev/null | grep "^pool-"); do
    virsh -c qemu:///session destroy "$n" 2>/dev/null
    virsh -c qemu:///session undefine "$n" --remove-all-storage 2>/dev/null
    echo "  destroyed $n"
done

rm -f "$SOCK"
echo "✓ stopped"
