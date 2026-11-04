#!/usr/bin/env bash
# x0ne-vm-control-start — bring up the full Mission Control stack.
#
# Launches (and survives even if the parent shell closes):
#   1. x0ne-vmd                 — pool daemon (creates pool of N clones)
#   2. mission-control.py       — web dashboard at http://localhost:7901/
#
# Usage:
#   bash scripts/x0ne-vm-control-start.sh           # default: target=3, base=x0ne-test
#   TARGET=5 BASE=x0ne-firefox bash scripts/x0ne-vm-control-start.sh
#   PORT=8080 bash scripts/x0ne-vm-control-start.sh
#
# Stop with:   bash scripts/x0ne-vm-control-stop.sh

set -uo pipefail

TARGET=${TARGET:-3}
BASE=${BASE:-x0ne-test}
PORT=${PORT:-7901}
DIR="$HOME/.x0ne/vm"

mkdir -p "$DIR"
cd "$(dirname "$0")/.."

# ─── prereqs ───
VMD="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vmd"
[[ -x "$VMD" ]] || { echo "✗ x0ne-vmd not built — run: cargo build -p x0ne-vm --release"; exit 1; }
[[ -f scripts/x0ne-vm-mission-control.py ]] || { echo "✗ mission-control script missing"; exit 1; }
virsh -c qemu:///session list --all --name 2>/dev/null | grep -qx "$BASE" || {
    echo "✗ base VM '$BASE' not defined. Set BASE=<name> or provision one first."
    exit 1
}

# ─── config ───
cat > "$DIR/vmd-config.json" <<EOF
{
  "base": "$BASE",
  "target": $TARGET,
  "prefix": "pool",
  "max": 16
}
EOF
echo "wrote $DIR/vmd-config.json:"
cat "$DIR/vmd-config.json"
echo

# ─── ensure clean state file ───
rm -f "$DIR/vmd.sock"
# Keep state.json if present (preserves existing pool assignments)

# ─── launch x0ne-vmd (nohup + setsid) ───
echo "▶ starting x0ne-vmd..."
setsid nohup "$VMD" >> "$DIR/vmd.log" 2>&1 < /dev/null &
VMD_PID=$!
disown "$VMD_PID" 2>/dev/null
sleep 0.3

# ─── wait for socket ───
for i in $(seq 1 30); do
    [[ -S "$DIR/vmd.sock" ]] && break
    sleep 0.2
done
if [[ ! -S "$DIR/vmd.sock" ]]; then
    echo "✗ x0ne-vmd did not create the socket within 6s — check $DIR/vmd.log"
    tail -5 "$DIR/vmd.log"
    exit 1
fi
echo "  ✓ x0ne-vmd live (PID $VMD_PID, socket $DIR/vmd.sock)"

# ─── wait for pool to fill ───
echo "▶ waiting for pool refill (~${TARGET} clones × ~70ms each)..."
sleep $(awk "BEGIN{print $TARGET*0.7+1}")
free=$(echo '{"cmd":"status"}' | timeout 2 socat - "UNIX-CONNECT:$DIR/vmd.sock" 2>/dev/null \
        | python3 -c "import json,sys;print(json.loads(sys.stdin.readline())['free'])" 2>/dev/null || echo "?")
echo "  pool: $free free slots"

# ─── launch mission control ───
echo "▶ starting mission control on port $PORT..."
PORT=$PORT setsid nohup python3 scripts/x0ne-vm-mission-control.py >> "$DIR/mc.log" 2>&1 < /dev/null &
MC_PID=$!
disown "$MC_PID" 2>/dev/null
sleep 1.5

# ─── verify ───
if ss -tlnp 2>/dev/null | grep -q "127.0.0.1:$PORT\b"; then
    echo "  ✓ mission control live (PID $MC_PID, http://127.0.0.1:$PORT/)"
else
    echo "  ✗ mission control didn't bind — check $DIR/mc.log"
    tail -5 "$DIR/mc.log"
    exit 1
fi

cat <<EOF

════════════════════════════════════════════════════════════════════
 ✓ x0ne-vm Mission Control is live

  Dashboard:    http://127.0.0.1:$PORT/
  Pool target:  $TARGET clones from base "$BASE"
  Pool socket:  $DIR/vmd.sock
  vmd log:      $DIR/vmd.log
  mc log:       $DIR/mc.log

  Stop with:    bash scripts/x0ne-vm-control-stop.sh
════════════════════════════════════════════════════════════════════
EOF

# Auto-open if we have a display
if command -v xdg-open >/dev/null 2>&1 && [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
    xdg-open "http://127.0.0.1:$PORT/" >/dev/null 2>&1 &
fi
