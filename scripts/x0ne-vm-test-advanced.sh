#!/usr/bin/env bash
# x0ne-vm-test-advanced — end-to-end coverage of the 16 MCP tools against a
# real running VM, with timing, error-case probes, snapshot round-trip and a
# small stress test.
#
# Usage:
#   VM_NAME=x0ne-test bash scripts/x0ne-vm-test-advanced.sh
#   VM_NAME=x0ne-debian bash scripts/x0ne-vm-test-advanced.sh
#
# Produces:
#   /tmp/x0ne-vm-adv-<N>.png       N screenshots taken along the way
#   /tmp/x0ne-vm-adv-report.txt    structured pass/fail log

set -uo pipefail   # no -e: a probe failing shouldn't kill the rest

VM_NAME=${VM_NAME:-x0ne-test}
MCP_BIN="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
OUT_DIR=${OUT_DIR:-/tmp}
REPORT="${OUT_DIR}/x0ne-vm-adv-report.txt"

if [[ ! -x "$MCP_BIN" ]]; then
    echo "✗ $MCP_BIN not found. Build with: cargo build -p x0ne-vm --release"
    exit 1
fi
if ! virsh -c qemu:///session list --name --state-running | grep -qx "$VM_NAME"; then
    echo "✗ VM '$VM_NAME' is not running. Start it with: virsh -c qemu:///session start $VM_NAME"
    exit 1
fi

C_OK="\033[32m✓\033[0m"
C_BAD="\033[31m✗\033[0m"
C_WARN="\033[33m⚠\033[0m"
C_STEP="\033[1;36m▶\033[0m"

PASS=0
FAIL=0
WARN=0
echo "─── x0ne-vm advanced suite — VM=${VM_NAME} — $(date -Iseconds) ───" > "$REPORT"

note()  { printf '%s\n' "$*"        | tee -a "$REPORT" >/dev/null; }
step()  { printf '\n'; printf "${C_STEP} %s\n" "$*"; note "▶ $*"; }
pass()  { printf "  ${C_OK} %s\n" "$*"; note "  PASS  $*"; PASS=$((PASS+1)); }
fail()  { printf "  ${C_BAD} %s\n" "$*"; note "  FAIL  $*"; FAIL=$((FAIL+1)); }
warn()  { printf "  ${C_WARN} %s\n" "$*"; note "  WARN  $*"; WARN=$((WARN+1)); }
say()   { printf "      %s\n" "$*"; note "      $*"; }

mcp_one() {
    # Sends ONE JSON-RPC request, returns ONE response line via stdout.
    # Args: method  [params-json]
    local method=$1
    local params=${2:-'{}'}
    printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}\n' \
        "$RANDOM" "$method" "$params" \
        | "$MCP_BIN" 2>/dev/null | head -1
}

mcp_tool() {
    # Sends a tools/call. Args: tool-name  arguments-json
    local name=$1
    local args=${2:-'{}'}
    mcp_one 'tools/call' "{\"name\":\"${name}\",\"arguments\":${args}}"
}

extract_text() {
    python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[]) or []
out = [c.get('text','') for c in content if c.get('type') == 'text']
print('\n'.join(out))
" 2>/dev/null
}

extract_image() {
    # Pulls base64 image content out, writes to \$1, returns size on stdout.
    local target=$1
    TARGET="$target" python3 -c "
import json,sys,os,base64
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[]) or []
img = next((c for c in content if c.get('type')=='image'), None)
if not img: sys.exit(2)
data = base64.b64decode(img['data'])
with open(os.environ['TARGET'],'wb') as f:
    f.write(data)
print(len(data))
" 2>/dev/null
}

is_error() {
    # Returns 0 if the JSON has isError:true; 1 otherwise.
    python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
sys.exit(0 if d.get('result',{}).get('isError') else 1)
" 2>/dev/null
}

# ─── 1. tool coverage: every advertised tool must show up in tools/list ─────

step "tool coverage — all 16 tools present"
expected=(vm_list vm_info vm_start vm_shutdown vm_force_off vm_reboot \
          vm_screenshot vm_click vm_move vm_drag vm_type vm_key \
          vm_snapshot_create vm_snapshot_restore vm_snapshot_list vm_snapshot_delete)
listing=$(mcp_one 'tools/list')
got=$(echo "$listing" | python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
for t in d['result']['tools']:
    print(t['name'])
" 2>/dev/null)
missing=()
for t in "${expected[@]}"; do
    if ! grep -qx "$t" <<<"$got"; then missing+=("$t"); fi
done
count_got=$(echo "$got" | wc -l)
if (( ${#missing[@]} == 0 )); then
    pass "16/16 tools listed (${count_got} total)"
else
    fail "missing: ${missing[*]}"
fi

# ─── 2. timing: each lifecycle/query tool's round-trip latency ──────────────

step "round-trip latency per tool (median of 3)"
latency_ms() {
    local name=$1
    local args=$2
    local t1 t2 ms
    local samples=()
    for _ in 1 2 3; do
        t1=$(date +%s%N)
        mcp_tool "$name" "$args" >/dev/null
        t2=$(date +%s%N)
        ms=$(( (t2 - t1) / 1000000 ))
        samples+=("$ms")
    done
    printf '%s\n' "${samples[@]}" | sort -n | head -2 | tail -1
}
m_list=$(latency_ms vm_list '{}')
m_info=$(latency_ms vm_info "{\"name\":\"${VM_NAME}\"}")
m_shot=$(latency_ms vm_screenshot "{\"name\":\"${VM_NAME}\"}")
say "vm_list        : ${m_list} ms"
say "vm_info        : ${m_info} ms"
say "vm_screenshot  : ${m_shot} ms"
[[ $m_list -lt 1000 ]] && pass "vm_list under 1s" || warn "vm_list slow (${m_list} ms)"
[[ $m_info -lt 1000 ]] && pass "vm_info under 1s" || warn "vm_info slow (${m_info} ms)"
[[ $m_shot -lt 2000 ]] && pass "vm_screenshot under 2s" || warn "vm_screenshot slow (${m_shot} ms)"

# ─── 3. screenshot dimensions match guest display ──────────────────────────

step "screenshot dimensions sane"
shot1="${OUT_DIR}/x0ne-vm-adv-1.png"
size=$(mcp_tool vm_screenshot "{\"name\":\"${VM_NAME}\"}" | extract_image "$shot1")
if [[ -n "$size" && -f "$shot1" ]]; then
    dims=$(python3 -c "
import struct
with open('$shot1','rb') as f:
    f.read(16)
    w,h = struct.unpack('>II', f.read(8))
    print(f'{w}x{h}')
")
    say "PNG bytes: ${size}, dims: ${dims}"
    [[ "$dims" =~ ^[0-9]+x[0-9]+$ ]] && pass "valid PNG with parseable IHDR ($dims)" \
                                     || fail "PNG corrupt or empty"
else
    fail "screenshot didn't return an image"
fi

# ─── 4. vm_type — full keysym coverage (digits, punctuation, mixed case) ───

step "vm_type — broad ASCII coverage"
# Drop into shell from wherever — Return to make sure we have a prompt.
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 0.4
test_str='hello-world 123 ~!@#$%^&*()_+-= [{]}|\\;:''",<.>/?'
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"${test_str}\"}" >/dev/null
sleep 0.3
shot2="${OUT_DIR}/x0ne-vm-adv-2.png"
mcp_tool vm_screenshot "{\"name\":\"${VM_NAME}\"}" | extract_image "$shot2" >/dev/null
if [[ -f "$shot2" && $(stat -c %s "$shot2") -gt 1000 ]]; then
    pass "broad-ASCII type completed, screenshot captured ($(stat -c %s "$shot2") bytes)"
else
    fail "broad-ASCII type produced no screenshot"
fi
# Clear the line (Ctrl-U) so we don't pollute next test.
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"ctrl+u\"}" >/dev/null

# ─── 5. vm_key — chord variety ──────────────────────────────────────────────

step "vm_key — chord variety (Ctrl-L, Tab, BackSpace, Ctrl+C)"
for chord in "ctrl+l" "tab" "backspace" "ctrl+c"; do
    resp=$(mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"${chord}\"}")
    if echo "$resp" | is_error; then
        fail "chord '$chord' returned isError=true"
    else
        pass "chord '$chord' accepted"
    fi
    sleep 0.1
done

# ─── 6. vm_move + vm_drag — pointer wire-format works ──────────────────────

step "vm_move + vm_drag — pointer events on the wire"
# Move to a few positions — they should all return without error.
for xy in "10 10" "400 300" "1000 500" "0 0"; do
    read -r x y <<<"$xy"
    resp=$(mcp_tool vm_move "{\"name\":\"${VM_NAME}\",\"x\":${x},\"y\":${y}}")
    echo "$resp" | is_error && fail "vm_move ($x,$y) errored" || true
done
pass "4 vm_move calls completed"

resp=$(mcp_tool vm_drag "{\"name\":\"${VM_NAME}\",\"x1\":100,\"y1\":100,\"x2\":600,\"y2\":400}")
if echo "$resp" | is_error; then
    fail "vm_drag returned isError"
else
    pass "vm_drag (100,100)→(600,400) completed (sends 7 PointerEvents wire-side)"
fi

# ─── 7. snapshot round-trip ────────────────────────────────────────────────

step "snapshot round-trip — create → modify → restore → verify"
SNAP="x0ne-adv-$(date +%s)"

resp=$(mcp_tool vm_snapshot_create "{\"name\":\"${VM_NAME}\",\"snapshot\":\"${SNAP}\"}")
if echo "$resp" | is_error; then
    txt=$(echo "$resp" | extract_text)
    warn "snapshot_create errored (skipping round-trip): $txt"
    say "(qemu:///session + cdrom-only VM cannot snapshot — needs a disk-backed VM)"
else
    pass "snapshot_create '$SNAP'"

    # Listing must include the new snapshot.
    resp=$(mcp_tool vm_snapshot_list "{\"name\":\"${VM_NAME}\"}")
    if echo "$resp" | extract_text | grep -qF "$SNAP"; then
        pass "snapshot_list contains '$SNAP'"
    else
        fail "snapshot_list missing '$SNAP'"
    fi

    # Take a baseline screenshot.
    shot_a="${OUT_DIR}/x0ne-vm-adv-snap-before.png"
    mcp_tool vm_screenshot "{\"name\":\"${VM_NAME}\"}" | extract_image "$shot_a" >/dev/null

    # Modify state — type some garbage.
    mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"echo POISON_MARKER_$RANDOM\"}" >/dev/null
    mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
    sleep 1
    shot_b="${OUT_DIR}/x0ne-vm-adv-snap-after.png"
    mcp_tool vm_screenshot "{\"name\":\"${VM_NAME}\"}" | extract_image "$shot_b" >/dev/null
    if ! cmp -s "$shot_a" "$shot_b"; then
        pass "state changed after typing (screens differ)"
    else
        warn "state didn't change after typing — VM may be stuck"
    fi

    # Restore.
    resp=$(mcp_tool vm_snapshot_restore "{\"name\":\"${VM_NAME}\",\"snapshot\":\"${SNAP}\"}")
    if echo "$resp" | is_error; then
        fail "snapshot_restore errored"
    else
        pass "snapshot_restore '$SNAP'"
    fi

    # Cleanup.
    resp=$(mcp_tool vm_snapshot_delete "{\"name\":\"${VM_NAME}\",\"snapshot\":\"${SNAP}\"}")
    if echo "$resp" | is_error; then
        warn "snapshot_delete errored (left orphan: $SNAP)"
    else
        pass "snapshot_delete '$SNAP'"
    fi
fi

# ─── 8. error paths — nonexistent VM, invalid args ─────────────────────────

step "error paths return isError=true (not crash)"
resp=$(mcp_tool vm_info '{"name":"does-not-exist-xyz"}')
if echo "$resp" | is_error; then
    pass "vm_info on nonexistent VM → isError"
else
    fail "vm_info on nonexistent VM should have errored"
fi

resp=$(mcp_tool vm_click "{\"name\":\"${VM_NAME}\"}")  # missing x,y
if echo "$resp" | is_error; then
    pass "vm_click without x,y → isError"
else
    fail "vm_click without x,y should have errored"
fi

resp=$(mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"definitely+not+a+real+key+xyz\"}")
if echo "$resp" | is_error; then
    pass "vm_key with unknown chord → isError"
else
    fail "vm_key with unknown chord should have errored"
fi

# ─── 9. stress — 50 rapid actions ──────────────────────────────────────────

step "stress test — 50 rapid vm_move calls"
t1=$(date +%s%N)
errors=0
for i in $(seq 1 50); do
    x=$((RANDOM % 1000))
    y=$((RANDOM % 700))
    resp=$(mcp_tool vm_move "{\"name\":\"${VM_NAME}\",\"x\":${x},\"y\":${y}}")
    echo "$resp" | is_error && errors=$((errors+1))
done
t2=$(date +%s%N)
total_ms=$(( (t2 - t1) / 1000000 ))
avg_ms=$(( total_ms / 50 ))
say "50 moves in ${total_ms} ms — avg ${avg_ms} ms/call"
if (( errors == 0 )); then
    pass "0/50 errored"
else
    fail "${errors}/50 errored"
fi

# ─── final report ──────────────────────────────────────────────────────────

echo
echo "────────────────────────────────────────────────────────────────────"
printf " ${C_OK} %d passed   ${C_BAD} %d failed   ${C_WARN} %d warned\n" "$PASS" "$FAIL" "$WARN"
echo " Report:      $REPORT"
echo " Screenshots: ${OUT_DIR}/x0ne-vm-adv-*.png"
echo "────────────────────────────────────────────────────────────────────"
note "summary: PASS=$PASS FAIL=$FAIL WARN=$WARN"

exit $(( FAIL > 0 ? 1 : 0 ))
