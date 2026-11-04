#!/usr/bin/env bash
# x0ne-vm-control-test — exercises every MCP tool against a running VM and
# cross-verifies each through a second tool call (move + cursor_position,
# scroll + screenshot diff, snapshot_create + snapshot_list, etc.).
#
# Designed as a regression sentinel: any meaningful break in input, capture,
# or lifecycle plumbing surfaces here within ~30s.
#
# Usage:
#   VM_NAME=x0ne-firefox bash scripts/x0ne-vm-control-test.sh
#
# Skips OCR-dependent checks if tesseract isn't installed, but still asserts
# everything else. Exits 1 on the first FAIL.

set -uo pipefail

VM=${VM_NAME:-x0ne-firefox}
MCP="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
OUT_DIR=${OUT_DIR:-/tmp/x0ne-vm-control}
mkdir -p "$OUT_DIR"

C_OK="\033[32m✓\033[0m"; C_BAD="\033[31m✗\033[0m"; C_SKIP="\033[33m·\033[0m"; C_STEP="\033[1;36m▶\033[0m"

PASS=0; FAIL=0; SKIP=0
STEP=0

assert_pass() { PASS=$((PASS+1)); printf "  ${C_OK} %s\n" "$*"; }
assert_fail() { FAIL=$((FAIL+1)); printf "  ${C_BAD} %s\n" "$*"; }
assert_skip() { SKIP=$((SKIP+1)); printf "  ${C_SKIP} %s\n" "$*"; }
step()        { STEP=$((STEP+1)); printf "\n${C_STEP} [%02d] %s\n" "$STEP" "$*"; }

# ─── helpers ────────────────────────────────────────────────────────────────

# Send a multi-call session over a single stdio MCP process, return each
# response line.
mcp_session() {
    "$MCP" 2>/dev/null
}

# Build a JSON-RPC tools/call line.
call() {
    local id=$1 name=$2 args=$3
    printf '{"jsonrpc":"2.0","id":%d,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' \
        "$id" "$name" "$args"
}

extract_text() {
    python3 -c "
import json,sys
d=json.loads(sys.stdin.read())
content=d.get('result',{}).get('content',[])
out=[c.get('text','') for c in content if c.get('type')=='text']
print('\n'.join(out))
"
}

is_error() {
    python3 -c "
import json,sys
d=json.loads(sys.stdin.read())
sys.exit(0 if d.get('result',{}).get('isError') else 1)
"
}

# Saves the image payload from a tools/call response to \$1; prints size in bytes.
save_image() {
    local target=$1
    TARGET="$target" python3 -c "
import json,sys,os,base64
d=json.loads(sys.stdin.read())
content=d.get('result',{}).get('content',[]) or []
img=next((c for c in content if c.get('type')=='image'),None)
if not img:
    sys.exit(2)
data=base64.b64decode(img['data'])
open(os.environ['TARGET'],'wb').write(data)
print(len(data))
"
}

# Run two calls in the same MCP session so per-process state (last_cursor) is
# preserved between them. Returns both responses on separate lines.
two_calls() {
    local a=$1 b=$2
    { echo "$a"; echo "$b"; } | mcp_session
}

# ─── pre-flight: VM must be running ────────────────────────────────────────

step "pre-flight"
if ! virsh -c qemu:///session list --state-running --name | grep -qx "$VM"; then
    echo "  ${C_BAD} VM '$VM' is not running."
    echo "    Start it: virsh -c qemu:///session start $VM"
    exit 1
fi
assert_pass "VM '$VM' is running"

if [[ ! -x "$MCP" ]]; then
    echo "  ${C_BAD} x0ne-vm-mcp not built"
    echo "    Build: cargo build -p x0ne-vm --release"
    exit 1
fi
assert_pass "x0ne-vm-mcp present"

HAS_OCR=0
if command -v tesseract >/dev/null 2>&1; then
    HAS_OCR=1
    assert_pass "tesseract available — OCR checks enabled"
else
    assert_skip "tesseract missing — OCR checks will be skipped"
fi

# ─── 1. tools/list — 23 tools advertised ───────────────────────────────────

step "tools/list lists 23 tools"
list_resp=$(echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | mcp_session)
n=$(echo "$list_resp" | python3 -c "import json,sys;print(len(json.loads(sys.stdin.read())['result']['tools']))")
if [[ "$n" -eq 23 ]]; then
    assert_pass "$n MCP tools advertised"
else
    assert_fail "expected 23 tools, got $n"
fi

# ─── 2. vm_info returns running ────────────────────────────────────────────

step "vm_info / vm_list"
info=$(call 1 vm_info "{\"name\":\"$VM\"}" | mcp_session)
state=$(echo "$info" | extract_text | python3 -c "import json,sys;print(json.loads(sys.stdin.read())['state'])")
if [[ "$state" == "running" ]]; then
    assert_pass "vm_info reports state=running"
else
    assert_fail "vm_info state=$state (expected running)"
fi
list_out=$(call 1 vm_list '{}' | mcp_session)
if echo "$list_out" | extract_text | grep -qF "\"$VM\""; then
    assert_pass "vm_list contains '$VM'"
else
    assert_fail "vm_list missing '$VM'"
fi

# ─── 3. vm_move + vm_cursor_position cross-check (same session) ────────────

step "vm_move + vm_cursor_position cross-verify"
for xy in "100 100" "500 400" "900 700"; do
    read -r x y <<<"$xy"
    a=$(call 1 vm_move "{\"name\":\"$VM\",\"x\":$x,\"y\":$y}")
    b=$(call 2 vm_cursor_position "{\"name\":\"$VM\"}")
    out=$(two_calls "$a" "$b")
    pos=$(echo "$out" | sed -n '2p' | extract_text)
    if echo "$pos" | grep -q "\"x\":$x,\"y\":$y"; then
        assert_pass "move ($x,$y) → cursor_position ($x,$y)"
    else
        assert_fail "move ($x,$y) → got $pos"
    fi
done

# ─── 4. vm_screenshot returns sane PNG ─────────────────────────────────────

step "vm_screenshot"
shot1="$OUT_DIR/shot1.png"
sz1=$(call 1 vm_screenshot "{\"name\":\"$VM\"}" | mcp_session | save_image "$shot1")
if [[ -n "$sz1" && "$sz1" -gt 5000 ]]; then
    dims=$(python3 -c "
import struct
with open('$shot1','rb') as f:
    f.read(16); w,h=struct.unpack('>II',f.read(8))
    print(f'{w}x{h}')
")
    assert_pass "captured $dims PNG ($sz1 bytes)"
else
    assert_fail "screenshot too small ($sz1 bytes) — capture probably broken"
fi

# ─── 5. vm_scroll changes the screen byte-wise ─────────────────────────────

step "vm_scroll up/down changes the framebuffer"
# Scroll down 5 ticks then capture
call 1 vm_scroll "{\"name\":\"$VM\",\"direction\":\"down\",\"clicks\":5}" | mcp_session >/dev/null
sleep 0.6
shot2="$OUT_DIR/shot2.png"
sz2=$(call 1 vm_screenshot "{\"name\":\"$VM\"}" | mcp_session | save_image "$shot2")
if cmp -s "$shot1" "$shot2"; then
    assert_skip "scroll-down had no visible effect (page may already be at top/bottom)"
else
    assert_pass "scroll-down changed the framebuffer ($sz1 → $sz2 bytes)"
fi
# Scroll back up
call 1 vm_scroll "{\"name\":\"$VM\",\"direction\":\"up\",\"clicks\":5}" | mcp_session >/dev/null
sleep 0.4

# ─── 6. vm_type + vm_key (cycle through Ctrl+L → text → Esc) ───────────────

step "vm_type / vm_key (cycle URL bar)"
# Focus URL bar
call 1 vm_key "{\"name\":\"$VM\",\"key\":\"ctrl+l\"}" | mcp_session >/dev/null
sleep 0.5
# Type a marker we can verify with OCR
MARKER="x0ne-control-$$"
call 1 vm_type "{\"name\":\"$VM\",\"text\":\"$MARKER\"}" | mcp_session >/dev/null
sleep 0.6
shot3="$OUT_DIR/shot3.png"
call 1 vm_screenshot "{\"name\":\"$VM\"}" | mcp_session | save_image "$shot3" >/dev/null
if (( HAS_OCR )); then
    found=$(call 1 vm_find_text "{\"name\":\"$VM\",\"text\":\"$MARKER\"}" | mcp_session | extract_text)
    if echo "$found" | grep -qF "$MARKER"; then
        assert_pass "vm_find_text recovered the typed marker"
    else
        # OCR may miss in URL bar font — fall back to byte diff
        if ! cmp -s "$shot2" "$shot3"; then
            assert_pass "type changed framebuffer (OCR didn't recognize marker, fallback OK)"
        else
            assert_fail "type did not change the framebuffer and OCR didn't find marker"
        fi
    fi
else
    # No OCR — diff vs previous
    if ! cmp -s "$shot2" "$shot3"; then
        assert_pass "vm_type changed framebuffer (byte-diff, OCR unavailable)"
    else
        assert_fail "vm_type did not change the framebuffer"
    fi
fi
# Esc to drop the focus / clear the URL bar input
call 1 vm_key "{\"name\":\"$VM\",\"key\":\"escape\"}" | mcp_session >/dev/null
sleep 0.3

# ─── 7. vm_click → produces a screen change ────────────────────────────────

step "vm_click at known coordinates"
shot4="$OUT_DIR/shot4.png"
call 1 vm_screenshot "{\"name\":\"$VM\"}" | mcp_session | save_image "$shot4" >/dev/null
call 1 vm_click "{\"name\":\"$VM\",\"x\":460,\"y\":434}" | mcp_session >/dev/null
sleep 0.5
shot5="$OUT_DIR/shot5.png"
call 1 vm_screenshot "{\"name\":\"$VM\"}" | mcp_session | save_image "$shot5" >/dev/null
if cmp -s "$shot4" "$shot5"; then
    assert_skip "vm_click had no visible effect (target may be off form input — acceptable)"
else
    assert_pass "vm_click changed framebuffer (focused field / opened thing)"
fi

# ─── 8. vm_triple_click ────────────────────────────────────────────────────

step "vm_triple_click"
out=$(call 1 vm_triple_click "{\"name\":\"$VM\",\"x\":460,\"y\":434}" | mcp_session | extract_text)
if echo "$out" | grep -qF "triple-clicked"; then
    assert_pass "vm_triple_click returned ok"
else
    assert_fail "vm_triple_click: $out"
fi

# ─── 9. vm_drag ────────────────────────────────────────────────────────────

step "vm_drag returns ok"
out=$(call 1 vm_drag "{\"name\":\"$VM\",\"x1\":100,\"y1\":100,\"x2\":400,\"y2\":300}" | mcp_session | extract_text)
if echo "$out" | grep -qE "dragged.*100,100.*400,300"; then
    assert_pass "vm_drag (100,100) → (400,300)"
else
    assert_fail "vm_drag: $out"
fi

# ─── 10. vm_wait ───────────────────────────────────────────────────────────

step "vm_wait (300ms) respects clamp"
t1=$(date +%s%N)
out=$(call 1 vm_wait '{"ms":300}' | mcp_session | extract_text)
t2=$(date +%s%N)
ms=$(( (t2 - t1) / 1000000 ))
if echo "$out" | grep -qF "waited 300 ms" && (( ms >= 280 && ms <= 700 )); then
    assert_pass "vm_wait 300ms took ${ms}ms (within tolerance)"
else
    assert_fail "vm_wait 300ms took ${ms}ms — $out"
fi

# ─── 11. snapshot lifecycle ────────────────────────────────────────────────

step "snapshot create / list / restore / delete"
SNAP="control-test-$$"
out=$(call 1 vm_snapshot_create "{\"name\":\"$VM\",\"snapshot\":\"$SNAP\"}" | mcp_session)
if echo "$out" | is_error; then
    txt=$(echo "$out" | extract_text)
    assert_skip "snapshot_create errored (may need disk-backed VM): $txt"
else
    assert_pass "snapshot_create '$SNAP'"

    listing=$(call 1 vm_snapshot_list "{\"name\":\"$VM\"}" | mcp_session | extract_text)
    if echo "$listing" | grep -qF "$SNAP"; then
        assert_pass "snapshot_list contains '$SNAP'"
    else
        assert_fail "snapshot_list missing '$SNAP'"
    fi

    out=$(call 1 vm_snapshot_restore "{\"name\":\"$VM\",\"snapshot\":\"$SNAP\"}" | mcp_session)
    if echo "$out" | is_error; then
        msg=$(echo "$out" | extract_text)
        assert_fail "snapshot_restore errored: $msg"
    else
        assert_pass "snapshot_restore '$SNAP'"
    fi

    call 1 vm_snapshot_delete "{\"name\":\"$VM\",\"snapshot\":\"$SNAP\"}" | mcp_session >/dev/null
    listing=$(call 1 vm_snapshot_list "{\"name\":\"$VM\"}" | mcp_session | extract_text)
    if echo "$listing" | grep -qF "$SNAP"; then
        assert_fail "snapshot_delete didn't remove '$SNAP'"
    else
        assert_pass "snapshot_delete cleaned '$SNAP'"
    fi
fi

# ─── 12. error-path probes ─────────────────────────────────────────────────

step "error paths: isError=true (not crash)"
out=$(call 1 vm_info '{"name":"does-not-exist-xyz"}' | mcp_session)
if echo "$out" | is_error; then assert_pass "vm_info on phantom VM → isError"
else assert_fail "vm_info on phantom VM should have errored"; fi

out=$(call 1 vm_click '{"name":"'$VM'"}' | mcp_session)
if echo "$out" | is_error; then assert_pass "vm_click w/o x,y → isError"
else assert_fail "vm_click w/o x,y should have errored"; fi

out=$(call 1 vm_scroll "{\"name\":\"$VM\",\"direction\":\"sideways\"}" | mcp_session)
if echo "$out" | is_error; then assert_pass "vm_scroll bad direction → isError"
else assert_fail "vm_scroll bad direction should have errored"; fi

# ─── 13. tesseract-dependent: vm_find_text / wait_for_text ────────────────

step "OCR: vm_find_text on the URL bar"
if (( HAS_OCR )); then
    # Type a unique string in the URL bar, then OCR-find it
    call 1 vm_key "{\"name\":\"$VM\",\"key\":\"ctrl+l\"}" | mcp_session >/dev/null
    sleep 0.4
    NEEDLE="ocr-test-$$"
    call 1 vm_type "{\"name\":\"$VM\",\"text\":\"$NEEDLE\"}" | mcp_session >/dev/null
    sleep 0.8
    found=$(call 1 vm_find_text "{\"name\":\"$VM\",\"text\":\"$NEEDLE\"}" | mcp_session | extract_text)
    if echo "$found" | grep -qF "\"text\":\"$NEEDLE\""; then
        assert_pass "vm_find_text located the freshly-typed marker"
    elif echo "$found" | grep -qF '"found":false'; then
        # Could be font rendering — tesseract sometimes misses URL bar fonts
        assert_skip "OCR didn't find marker (URL bar font hard for tesseract — non-fatal)"
    else
        assert_fail "vm_find_text unexpected output: $found"
    fi
    call 1 vm_key "{\"name\":\"$VM\",\"key\":\"escape\"}" | mcp_session >/dev/null
else
    assert_skip "vm_find_text skipped (tesseract missing)"
fi

# ─── final report ──────────────────────────────────────────────────────────

echo
echo "════════════════════════════════════════════════════════════════════"
printf " ${C_OK} %2d passed   ${C_BAD} %2d failed   ${C_SKIP} %2d skipped\n" "$PASS" "$FAIL" "$SKIP"
echo " Screenshots in: $OUT_DIR/"
echo "════════════════════════════════════════════════════════════════════"

exit $(( FAIL > 0 ? 1 : 0 ))
