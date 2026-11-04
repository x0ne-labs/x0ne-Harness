#!/usr/bin/env bash
# x0ne-vm-mouse-tricks — show off pointer control by drawing geometric
# patterns with the VM's cursor:
#
#   1. slow wide circle  (radius 250, one revolution)
#   2. tight fast circle (radius 100, three revolutions)
#   3. expanding spiral  (radius 30 → 250)
#   4. lissajous figure-8
#
# Watch with `virt-viewer --connect qemu:///session x0ne-firefox` (live)
# or via the browser-based watcher at http://localhost:7842/.
#
# Usage:
#   VM_NAME=x0ne-firefox bash scripts/x0ne-vm-mouse-tricks.sh
#   CENTER_X=512 CENTER_Y=384 bash scripts/x0ne-vm-mouse-tricks.sh

set -uo pipefail
VM=${VM_NAME:-x0ne-firefox}
CX=${CENTER_X:-512}
CY=${CENTER_Y:-384}
MCP=${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp

if [[ ! -x "$MCP" ]]; then
    echo "✗ $MCP not built. Run: cargo build -p x0ne-vm --release"
    exit 1
fi

move() {
    # Send a vm_move JSON-RPC call.
    printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vm_move","arguments":{"name":"%s","x":%d,"y":%d}}}\n' \
        "$VM" "$1" "$2" \
      | "$MCP" 2>/dev/null >/dev/null
}

# Precompute all points and stream them through move() with a fixed cadence.
# Each call already takes ~40-60ms (VNC handshake + 1 PointerEvent), so an
# extra `sleep 0.02` keeps things smooth without making it choppy.
run_pattern() {
    local label=$1 ; shift
    local script=$1 ; shift
    echo
    echo "▶ $label"
    python3 -c "$script" | while read -r x y; do
        move "$x" "$y"
    done
}

# ─── 1. slow wide circle ───────────────────────────────────────────────────

run_pattern "wide circle (radius 250)" "
import math
CX,CY = $CX,$CY
N = 60
for i in range(N+1):
    a = 2*math.pi*i/N
    print(int(CX + 250*math.cos(a)), int(CY + 250*math.sin(a)))
"

# ─── 2. fast tight circle, 3 revolutions ────────────────────────────────────

run_pattern "tight circle x3 (radius 100)" "
import math
CX,CY = $CX,$CY
N = 90  # 3 revolutions × 30 pts
for i in range(N+1):
    a = 3 * 2*math.pi*i/N
    print(int(CX + 100*math.cos(a)), int(CY + 100*math.sin(a)))
"

# ─── 3. expanding spiral ────────────────────────────────────────────────────

run_pattern "expanding spiral (30 → 250)" "
import math
CX,CY = $CX,$CY
N = 120
for i in range(N+1):
    t = i/N
    r = 30 + (250-30)*t           # linearly grow radius
    a = 4*math.pi*t               # 2 turns
    print(int(CX + r*math.cos(a)), int(CY + r*math.sin(a)))
"

# ─── 4. lissajous figure-8 ──────────────────────────────────────────────────

run_pattern "lissajous figure-8" "
import math
CX,CY = $CX,$CY
N = 100
A,B = 250, 150
for i in range(N+1):
    t = 2*math.pi*i/N
    x = CX + A*math.sin(2*t)
    y = CY + B*math.sin(t)
    print(int(x), int(y))
"

# ─── return to center ───────────────────────────────────────────────────────

echo
echo "▶ return to center ($CX, $CY)"
move "$CX" "$CY"
echo
echo "✓ done"
