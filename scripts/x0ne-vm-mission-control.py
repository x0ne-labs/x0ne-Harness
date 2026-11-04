#!/usr/bin/env python3
"""
x0ne-vm Mission Control — observable grid of every VM body in the pool.

What it does:
  - Reads ~/.x0ne/vm/vmd-state.json to enumerate pool slots
  - For each slot, polls vm_screenshot via x0ne-vm-mcp + overlays cursor
  - Serves http://127.0.0.1:7900/ with a live tile grid
  - Browser auto-refreshes images + state every 1s
  - One-click acquire / release / shutdown directly from the dashboard

Bind: loopback only.  No auth (acceptable for local dev; do not expose).
"""
import base64
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from io import BytesIO
from pathlib import Path
from urllib.parse import urlparse, parse_qs

# ─── config ────────────────────────────────────────────────────────────────

HOME = Path(os.environ.get("HOME", "/tmp"))
STATE_FILE = HOME / ".x0ne" / "vm" / "vmd-state.json"
SOCK_PATH = HOME / ".x0ne" / "vm" / "vmd.sock"
CURSOR_FMT = HOME / ".x0ne" / "vm" / "cursor-{name}.json"
MCP_BIN = os.environ.get(
    "X0NE_VM_MCP",
    str(Path(__file__).resolve().parent.parent / "target" / "release" / "x0ne-vm-mcp")
)
PORT = int(os.environ.get("PORT", "7900"))
POLL_INTERVAL = float(os.environ.get("POLL_INTERVAL", "1.2"))

# ─── in-memory frame cache (per-slot PNG with cursor overlay) ───────────────

_frames_lock = threading.Lock()
_frames = {}  # name -> (bytes, captured_ts)

# ─── helpers ────────────────────────────────────────────────────────────────

def vmd_cmd(req: dict):
    """Send one JSON command to x0ne-vmd over the Unix socket; return reply."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(2.0)
            s.connect(str(SOCK_PATH))
            s.sendall((json.dumps(req) + "\n").encode())
            data = b""
            while b"\n" not in data:
                chunk = s.recv(65536)
                if not chunk:
                    break
                data += chunk
            line = data.split(b"\n")[0]
            return json.loads(line.decode()) if line else None
    except Exception as e:
        return {"ok": False, "error": str(e)}


def call_mcp_screenshot(vm: str):
    """Call x0ne-vm-mcp via subprocess and grab the PNG of one VM."""
    if not os.path.isfile(MCP_BIN):
        return None
    req = {
        "jsonrpc": "2.0", "id": 1,
        "method": "tools/call",
        "params": {"name": "vm_screenshot", "arguments": {"name": vm}},
    }
    try:
        p = subprocess.run(
            [MCP_BIN], input=(json.dumps(req) + "\n").encode(),
            capture_output=True, timeout=10
        )
        line = p.stdout.split(b"\n")[0]
        d = json.loads(line)
        content = d.get("result", {}).get("content", []) or []
        img = next((c for c in content if c.get("type") == "image"), None)
        if not img:
            return None
        return base64.b64decode(img["data"])
    except Exception:
        return None


def overlay_cursor(png_bytes: bytes, vm: str) -> bytes:
    """If we have a recorded cursor for this VM, draw a crosshair on the PNG."""
    cur_path = Path(str(CURSOR_FMT).format(name=vm))
    if not cur_path.exists():
        return png_bytes
    try:
        cur = json.loads(cur_path.read_text())
        x, y = int(cur.get("x", -1)), int(cur.get("y", -1))
        if x < 0 or y < 0:
            return png_bytes
        from PIL import Image, ImageDraw
        im = Image.open(BytesIO(png_bytes)).convert("RGBA")
        d = ImageDraw.Draw(im)
        for r, col in [(10, (255, 255, 255, 220)), (7, (255, 0, 0, 230)), (3, (255, 255, 255, 255))]:
            d.ellipse([x - r, y - r, x + r, y + r], fill=col)
        for (a, b, c, e) in [(x - 15, y, x - 5, y), (x + 5, y, x + 15, y),
                              (x, y - 15, x, y - 5), (x, y + 5, x, y + 15)]:
            d.line([(a, b), (c, e)], fill=(255, 0, 0, 200), width=2)
        out = BytesIO()
        im.convert("RGB").save(out, "PNG")
        return out.getvalue()
    except Exception:
        return png_bytes


def read_state() -> dict:
    """Read vmd-state.json — or return an empty state if vmd isn't running."""
    try:
        return json.loads(STATE_FILE.read_text())
    except Exception:
        return {"config": {"base": "?", "target": 0, "prefix": "pool", "max": 0},
                "slots": {}}


def slot_state_label(state) -> str:
    """Normalise a slot's state to a single short label."""
    if isinstance(state, str):
        return state
    if isinstance(state, dict):
        return next(iter(state.keys()), "?")
    return "?"


def slot_state_who(state):
    """Return the `by` field of an Acquired state, if any."""
    if isinstance(state, dict) and "acquired" in state:
        return state["acquired"].get("by")
    return None


# ─── background poller — keeps _frames warm ─────────────────────────────────

_stop = False

def poller_loop():
    while not _stop:
        state = read_state()
        slot_names = list(state.get("slots", {}).keys())
        for name in slot_names:
            png = call_mcp_screenshot(name)
            if png:
                png = overlay_cursor(png, name)
                with _frames_lock:
                    _frames[name] = (png, time.time())
        # Drop frames for slots that no longer exist.
        with _frames_lock:
            for stale in list(_frames.keys()):
                if stale not in slot_names:
                    _frames.pop(stale, None)
        time.sleep(POLL_INTERVAL)


# ─── http server ───────────────────────────────────────────────────────────

INDEX_HTML = r"""<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>x0ne-vm · Mission Control</title>
<link href="https://fonts.googleapis.com/css2?family=DM+Mono:wght@400;500&family=Instrument+Serif&display=swap" rel="stylesheet">
<style>
:root{--bg:#06070d;--s:#0a0b12;--accent:#00dfa8;--accent2:#ff6b9d;--t:rgba(255,255,255,.85);--m:rgba(255,255,255,.42);--d:rgba(255,255,255,.13);--b:rgba(255,255,255,.06)}
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:'DM Mono',monospace;background:var(--bg);color:var(--t);font-size:13px;line-height:1.5;overflow-x:hidden}
.bg{position:fixed;inset:0;z-index:0;pointer-events:none;background:
  radial-gradient(ellipse 80% 60% at 50% 0%, rgba(0,223,168,.04), transparent 70%),
  repeating-linear-gradient(0deg,transparent,transparent 2px,rgba(0,0,0,.03) 2px,rgba(0,0,0,.03) 4px)}
.wrap{position:relative;z-index:1;max-width:1600px;margin:0 auto;padding:1.6rem 2rem}

header{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--b);padding-bottom:.9rem;margin-bottom:1.6rem;flex-wrap:wrap;gap:1rem}
.brand{font-family:'Instrument Serif',serif;font-size:1.6rem;letter-spacing:-.01em}
.brand .a{color:var(--accent)}
.stats{display:flex;gap:1.6rem;font-size:.78rem;color:var(--m)}
.stats .n{font-family:'Instrument Serif',serif;font-style:italic;font-size:1.4rem;color:var(--t);margin-right:.3rem}
.stats .free .n{color:var(--accent)}
.stats .acq .n{color:var(--accent2)}

.actions{display:flex;gap:.5rem;align-items:center}
button.btn{background:rgba(255,255,255,.04);border:1px solid var(--b);color:var(--t);font-family:inherit;font-size:.7rem;letter-spacing:.12em;text-transform:uppercase;padding:.45rem .9rem;cursor:pointer;transition:all .15s}
button.btn:hover{background:rgba(0,223,168,.1);border-color:var(--accent);color:var(--accent)}
button.btn.danger:hover{background:rgba(255,107,157,.1);border-color:var(--accent2);color:var(--accent2)}

.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(380px,1fr));gap:1rem}
.tile{background:rgba(255,255,255,.02);border:1px solid var(--b);position:relative;transition:border-color .2s,transform .2s}
.tile:hover{border-color:rgba(255,255,255,.18)}
.tile-img{display:block;width:100%;aspect-ratio:4/3;object-fit:contain;background:#000;border-bottom:1px solid var(--b)}
.tile-bar{padding:.7rem .9rem;display:flex;justify-content:space-between;align-items:center;gap:.6rem}
.tile-name{font-weight:500;color:var(--t)}
.tile-name small{color:var(--m);font-weight:400;margin-left:.4rem}
.tile-state{font-size:.6rem;letter-spacing:.14em;text-transform:uppercase;padding:.18rem .55rem;border:1px solid currentColor;border-radius:99px;display:inline-block}
.s-free{color:var(--accent)}
.s-acquired{color:var(--accent2)}
.s-recycling{color:#9fb0e0}
.s-reaped{color:var(--d)}
.tile-actions{display:flex;gap:.4rem;padding:0 .9rem .8rem}
.tile-actions button{flex:1;font-size:.65rem;padding:.4rem .5rem}

.banner{padding:.7rem 1rem;margin-bottom:1rem;border:1px solid var(--b);background:rgba(255,107,157,.05);color:var(--m);font-size:.78rem}
.banner b{color:var(--accent2)}
.banner.ok{background:rgba(0,223,168,.04);border-color:rgba(0,223,168,.18);color:var(--m)}
.banner.ok b{color:var(--accent)}

.empty{padding:3rem;text-align:center;color:var(--m);font-family:'Instrument Serif',serif;font-style:italic;font-size:1.2rem}
.empty small{display:block;font-family:'DM Mono',monospace;font-style:normal;font-size:.7rem;letter-spacing:.16em;margin-top:.8rem;color:var(--d)}

.toast{position:fixed;bottom:1rem;right:1rem;background:rgba(0,223,168,.95);color:#000;padding:.6rem 1rem;font-size:.78rem;letter-spacing:.06em;animation:slidein .3s ease, fadeout .3s ease 2.5s forwards;z-index:99}
.toast.err{background:rgba(255,107,157,.95)}
@keyframes slidein{from{transform:translateX(100%);opacity:0}to{transform:translateX(0);opacity:1}}
@keyframes fadeout{to{opacity:0;transform:translateX(20px)}}
</style></head><body>

<div class="bg"></div>

<div class="wrap">
<header>
  <div>
    <div class="brand"><span class="a">x0ne-vm</span> · mission control</div>
    <div style="color:var(--m);font-size:.72rem;letter-spacing:.04em;margin-top:.3rem" id="cfg">…</div>
  </div>
  <div class="stats">
    <div class="free"><span class="n" id="kFree">0</span>free</div>
    <div class="acq"><span class="n" id="kAcq">0</span>acquired</div>
    <div><span class="n" id="kTotal">0</span>slots</div>
    <div><span class="n" id="kTarget">0</span>target</div>
  </div>
  <div class="actions">
    <button class="btn" id="bAcq">+ acquire</button>
    <button class="btn danger" id="bShut">shutdown vmd</button>
  </div>
</header>

<div id="banner"></div>
<div class="grid" id="grid"></div>

</div>

<script>
const $ = (id) => document.getElementById(id);

function toast(msg, err) {
  const el = document.createElement('div');
  el.className = 'toast' + (err ? ' err' : '');
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(() => el.remove(), 3200);
}

async function postCmd(cmd, body) {
  const r = await fetch('/api/' + cmd, {method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify(body || {})});
  return r.json();
}

async function refresh() {
  let st;
  try { st = await (await fetch('/api/state')).json(); }
  catch (e) {
    $('banner').innerHTML = '<div class="banner"><b>x0ne-vmd not reachable</b> at ~/.x0ne/vm/vmd.sock — start it with: <code>./target/release/x0ne-vmd</code></div>';
    return;
  }
  if (!st || !st.config) {
    $('grid').innerHTML = '<div class="empty">no pool state on disk yet<small>start x0ne-vmd first</small></div>';
    return;
  }

  const cfg = st.config;
  $('cfg').textContent = `base=${cfg.base} · prefix=${cfg.prefix}/${cfg.max} · poll ${st.poll}s`;
  $('kFree').textContent = st.free;
  $('kAcq').textContent = st.acquired;
  $('kTotal').textContent = st.slots.length;
  $('kTarget').textContent = cfg.target;
  $('banner').innerHTML = (st.vmd_alive
    ? `<div class="banner ok">x0ne-vmd <b>live</b> · ${st.slots.length}/${cfg.max} slots · ${st.free} free · ${st.acquired} acquired</div>`
    : `<div class="banner">x0ne-vmd <b>unreachable</b> — pool actions disabled</div>`);

  const grid = $('grid');
  const existing = new Map([...grid.children].map(el => [el.dataset.name, el]));
  for (const slot of st.slots) {
    let el = existing.get(slot.name);
    if (!el) {
      el = document.createElement('div'); el.className = 'tile'; el.dataset.name = slot.name;
      el.innerHTML = `
        <img class="tile-img" alt="${slot.name}">
        <div class="tile-bar">
          <span class="tile-name">${slot.name} <small></small></span>
          <span class="tile-state"></span>
        </div>
        <div class="tile-actions"></div>`;
      grid.appendChild(el);
    }
    existing.delete(slot.name);
    const stateLabel = slot.stateLabel || 'unknown';
    const who = slot.acquiredBy;
    el.querySelector('.tile-img').src = '/slot/' + encodeURIComponent(slot.name) + '/live.png?t=' + Date.now();
    el.querySelector('.tile-name small').textContent = who ? '· ' + who : '';
    const stEl = el.querySelector('.tile-state');
    stEl.textContent = stateLabel;
    stEl.className = 'tile-state s-' + stateLabel;
    const acts = el.querySelector('.tile-actions');
    acts.innerHTML = '';
    if (stateLabel === 'free') {
      const b = document.createElement('button'); b.className = 'btn'; b.textContent = 'acquire'; b.onclick = async () => { const r = await postCmd('acquire', {by:'console'}); toast(r.ok ? 'acquired ' + r.vm : ('err: ' + r.error), !r.ok); refresh(); };
      acts.appendChild(b);
    } else if (stateLabel === 'acquired') {
      const b = document.createElement('button'); b.className = 'btn danger'; b.textContent = 'release'; b.onclick = async () => { const r = await postCmd('release', {vm:slot.name}); toast(r.ok ? 'releasing ' + slot.name : ('err: ' + r.error), !r.ok); refresh(); };
      acts.appendChild(b);
    } else if (stateLabel === 'recycling') {
      acts.innerHTML = '<div style="font-size:.65rem;color:var(--m);padding:.4rem 0;text-align:center;width:100%">…recycling…</div>';
    }
  }
  // Remove dead tiles
  for (const dead of existing.values()) dead.remove();
}

$('bAcq').onclick = async () => { const r = await postCmd('acquire', {by:'console'}); toast(r.ok ? 'acquired ' + r.vm : ('err: ' + r.error), !r.ok); refresh(); };
$('bShut').onclick = async () => {
  if (!confirm('shutdown x0ne-vmd? all pool VMs will be destroyed.')) return;
  await postCmd('shutdown', {}); toast('shutdown sent'); setTimeout(refresh, 1500);
};

refresh();
setInterval(refresh, 1500);
</script>

</body></html>"""


def _state_for_ui() -> dict:
    """Build the JSON payload the UI consumes — flatten slot state for JS."""
    raw = read_state()
    slots = []
    for name, slot in raw.get("slots", {}).items():
        state = slot.get("state")
        slots.append({
            "name": name,
            "stateLabel": slot_state_label(state),
            "acquiredBy": slot_state_who(state),
            "createdMs": slot.get("created_ms"),
        })
    config = raw.get("config", {})
    vmd_alive = SOCK_PATH.exists() and SOCK_PATH.is_socket()
    free = sum(1 for s in slots if s["stateLabel"] == "free")
    acquired = sum(1 for s in slots if s["stateLabel"] == "acquired")
    return {
        "config": config,
        "slots": slots,
        "free": free,
        "acquired": acquired,
        "vmd_alive": vmd_alive,
        "poll": POLL_INTERVAL,
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # silence access log

    def _json(self, payload, status=200):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        if path == "/":
            body = INDEX_HTML.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path == "/api/state":
            self._json(_state_for_ui()); return
        m = re.match(r"^/slot/([A-Za-z0-9._-]+)/live\.png$", path)
        if m:
            name = m.group(1)
            with _frames_lock:
                cached = _frames.get(name)
            if not cached:
                self.send_response(204); self.end_headers(); return
            png, _ = cached
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(png)))
            self.end_headers()
            self.wfile.write(png)
            return
        self.send_response(404); self.end_headers()

    def do_POST(self):
        parsed = urlparse(self.path)
        path = parsed.path
        length = int(self.headers.get("Content-Length") or 0)
        raw_body = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw_body) if raw_body else {}
        except Exception:
            payload = {}
        if path == "/api/acquire":
            res = vmd_cmd({"cmd": "acquire", "by": payload.get("by", "console")})
            self._json(res or {"ok": False, "error": "no reply"}); return
        if path == "/api/release":
            res = vmd_cmd({"cmd": "release", "vm": payload.get("vm")})
            self._json(res or {"ok": False, "error": "no reply"}); return
        if path == "/api/shutdown":
            res = vmd_cmd({"cmd": "shutdown"})
            self._json(res or {"ok": False, "error": "no reply"}); return
        self.send_response(404); self.end_headers()


def main():
    threading.Thread(target=poller_loop, daemon=True).start()
    print(f"x0ne-vm Mission Control on http://127.0.0.1:{PORT}/")
    print(f"  vmd socket: {SOCK_PATH}")
    print(f"  state:      {STATE_FILE}")
    print(f"  poll:       {POLL_INTERVAL}s")
    try:
        HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
    except KeyboardInterrupt:
        global _stop
        _stop = True
        print("\nbye.")


if __name__ == "__main__":
    main()
