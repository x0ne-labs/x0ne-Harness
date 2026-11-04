#!/usr/bin/env bash
# x0ne-vm-watch — quasi-live view of what the agent sees.
#
# Polls `vm_screenshot` every N ms and writes to /tmp/x0ne-vm-watch/live.png,
# then serves a tiny HTML page that auto-refreshes the image.
#
# Why not just open virt-manager?
#   QEMU's VNC server serializes ServerInit messages — if a VNC viewer is
#   connected at the same time as our agent's input client, our handshakes
#   race and `vm_type` / `vm_click` start failing with "read width".
#
# This watcher uses the SAME path as the agent (virsh screenshot, no VNC
# input contention), so it works concurrently with vm_type / vm_click.
#
# Usage:
#   bash scripts/x0ne-vm-watch.sh                  # default vm=x0ne-test
#   VM_NAME=x0ne-debian bash scripts/x0ne-vm-watch.sh
#   POLL_MS=200 bash scripts/x0ne-vm-watch.sh      # faster polling
#   PORT=9090 bash scripts/x0ne-vm-watch.sh        # different port
#
# Press Ctrl-C to stop.

set -uo pipefail

VM_NAME=${VM_NAME:-x0ne-test}
MCP_BIN="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
POLL_MS=${POLL_MS:-500}
PORT=${PORT:-7841}
WATCH_DIR=${WATCH_DIR:-/tmp/x0ne-vm-watch}

if [[ ! -x "$MCP_BIN" ]]; then
    echo "✗ $MCP_BIN not found. Build with: cargo build -p x0ne-vm --release"
    exit 1
fi

mkdir -p "$WATCH_DIR"

# ─── tiny HTML page with JS auto-refresh ────────────────────────────────────

cat > "$WATCH_DIR/index.html" <<HTML
<!doctype html>
<html><head>
<meta charset="utf-8">
<title>x0ne-vm live view — ${VM_NAME}</title>
<style>
  html, body { margin:0; background:#000; color:#9c9; font-family:monospace; }
  .bar { padding:6px 12px; border-bottom:1px solid #222; font-size:13px; }
  .bar b { color:#fff; }
  img { display:block; max-width:100%; }
</style>
</head><body>
<div class="bar">
  <b>x0ne-vm live</b> · vm=<b>${VM_NAME}</b> · poll=${POLL_MS}ms · <span id=t></span>
</div>
<img id="frame" src="live.png">
<script>
  const img = document.getElementById('frame');
  const tm  = document.getElementById('t');
  let n = 0;
  setInterval(() => {
    img.src = 'live.png?t=' + Date.now();
    tm.textContent = 'frame #' + (++n);
  }, ${POLL_MS});
</script>
</body></html>
HTML

echo "─── x0ne-vm-watch ─────────────────────────────────────────────"
echo "  vm:        $VM_NAME"
echo "  poll:      ${POLL_MS} ms"
echo "  watch dir: $WATCH_DIR"
echo "  url:       http://localhost:${PORT}/"
echo "───────────────────────────────────────────────────────────────"

# Start the HTTP server in background — bind to loopback ONLY so screenshots
# don't leak to anyone else on the LAN. Defaults would expose 0.0.0.0.
cd "$WATCH_DIR"
python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 &
SERVER_PID=$!
cleanup() {
    echo
    echo "stopping…"
    kill "$SERVER_PID" 2>/dev/null
    rm -f "$WATCH_DIR/live.png" "$WATCH_DIR/live.png.tmp"
}
trap cleanup EXIT INT TERM

# Open the URL in the user's browser once.
sleep 0.3
if command -v xdg-open >/dev/null; then
    xdg-open "http://localhost:${PORT}/" >/dev/null 2>&1 &
fi

# ─── poll loop ──────────────────────────────────────────────────────────────

cd "$OLDPWD"
CURSOR_FILE="${HOME}/.x0ne/vm/cursor-${VM_NAME}.json"
frame=0
while :; do
    out="${WATCH_DIR}/live.png.tmp"
    printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vm_screenshot","arguments":{"name":"%s"}}}\n' \
        "$VM_NAME" \
      | "$MCP_BIN" 2>/dev/null \
      | TARGET="$out" CURSOR="$CURSOR_FILE" python3 -c "
import json,sys,os,base64
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[]) or []
img = next((c for c in content if c.get('type')=='image'), None)
if not img: sys.exit(2)
data = base64.b64decode(img['data'])
# Overlay a synthetic cursor at last-known position if we can read it.
# QEMU separates the hw cursor overlay from the framebuffer, so virsh
# screenshots don't include the real pointer — this PIL overlay closes that gap.
try:
    cursor = json.load(open(os.environ['CURSOR']))
    x, y = int(cursor.get('x', -1)), int(cursor.get('y', -1))
    if x >= 0 and y >= 0:
        from PIL import Image, ImageDraw
        from io import BytesIO
        im = Image.open(BytesIO(data)).convert('RGBA')
        draw = ImageDraw.Draw(im)
        # white outer ring + red dot — visible on any background
        for r, color in [(10, (255, 255, 255, 220)), (7, (255, 0, 0, 230)), (3, (255, 255, 255, 255))]:
            draw.ellipse([x-r, y-r, x+r, y+r], fill=color)
        # crosshair lines
        draw.line([(x-15, y), (x-5, y)], fill=(255, 0, 0, 200), width=2)
        draw.line([(x+5, y), (x+15, y)], fill=(255, 0, 0, 200), width=2)
        draw.line([(x, y-15), (x, y-5)], fill=(255, 0, 0, 200), width=2)
        draw.line([(x, y+5), (x, y+15)], fill=(255, 0, 0, 200), width=2)
        buf = BytesIO()
        im.convert('RGB').save(buf, 'PNG')
        data = buf.getvalue()
except Exception:
    pass  # fall through to raw screenshot
with open(os.environ['TARGET'],'wb') as f: f.write(data)
" 2>/dev/null
    # Atomic rename so the HTTP server never serves a half-written file.
    if [[ -f "$out" ]]; then
        mv "$out" "${WATCH_DIR}/live.png"
        frame=$((frame+1))
        if (( frame % 10 == 0 )); then
            printf '\r  frame %4d  · size: %6s bytes' "$frame" "$(stat -c %s "${WATCH_DIR}/live.png")"
        fi
    fi
    sleep "$(awk "BEGIN{print $POLL_MS/1000}")"
done
