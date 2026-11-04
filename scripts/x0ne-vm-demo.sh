#!/usr/bin/env bash
# x0ne-vm-demo — visual round-trip exercising the x0ne-vm-mcp server.
#
# Drives x0ne-vm-mcp via stdio with raw JSON-RPC and saves screenshots
# from each step into /tmp/x0ne-vm-demo-<N>.png. Opens them with
# xdg-open at the end so you can visually confirm:
#
#   1. vm_list returned the test VM
#   2. vm_screenshot captured the boot screen
#   3. vm_key 'return' caused a screen change
#   4. vm_screenshot captured the new state
#
# Prereq:  scripts/x0ne-vm-setup.sh has been run.

set -euo pipefail

VM_NAME=${VM_NAME:-x0ne-test}
MCP_BIN="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
OUT_DIR=${OUT_DIR:-/tmp}

if [[ ! -x "$MCP_BIN" ]]; then
    echo "✗ $MCP_BIN not found. Run scripts/x0ne-vm-setup.sh first."
    exit 1
fi

# ─── helper: send a JSON-RPC request, return the parsed response ─────────────

mcp_call() {
    local method=$1
    local params=${2:-'{}'}
    local id=$RANDOM
    local req
    req=$(printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}\n' \
        "$id" "$method" "$params")
    # Send the request and read exactly one line back. The server stays
    # alive on stdin until EOF — we close stdin after our request line.
    echo "$req" | "$MCP_BIN" 2>/dev/null | head -1
}

step() { echo -e "\n\033[1;36m▶\033[0m $*"; }
ok()   { echo -e "  \033[32m✓\033[0m $*"; }
say()  { echo -e "  $*"; }

# ─── 1. initialize ──────────────────────────────────────────────────────────

step "initialize"
init=$(mcp_call initialize)
echo "$init" | python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
r = d.get('result', {})
si = r.get('serverInfo', {})
print('  server: ' + str(si.get('name')) + ' v' + str(si.get('version')))
print('  protocol: ' + str(r.get('protocolVersion')))
"

# ─── 2. tools/list ──────────────────────────────────────────────────────────

step "tools/list"
tools=$(mcp_call tools/list)
echo "$tools" | python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
tools = d.get('result',{}).get('tools',[])
print('  ' + str(len(tools)) + ' tools exposed:')
for t in tools:
    name = t['name']
    desc = t.get('description','')[:60]
    print('    - ' + name.ljust(24) + '  ' + desc)
"

# ─── 3. vm_list ─────────────────────────────────────────────────────────────

step "vm_list — what VMs does x0ne-vm-mcp see?"
list=$(mcp_call 'tools/call' "{\"name\":\"vm_list\",\"arguments\":{}}")
echo "$list" | python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[])
for c in content:
    if c.get('type') == 'text':
        for line in c['text'].splitlines():
            print('  ' + line)
"

# ─── 4. vm_info ─────────────────────────────────────────────────────────────

step "vm_info ${VM_NAME}"
info=$(mcp_call 'tools/call' "{\"name\":\"vm_info\",\"arguments\":{\"name\":\"${VM_NAME}\"}}")
echo "$info" | python3 -c "
import json,sys
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[])
for c in content:
    if c.get('type') == 'text':
        for line in c['text'].splitlines():
            print('  ' + line)
"

# ─── 5. vm_screenshot — capture boot screen ────────────────────────────────

step "vm_screenshot — capture 1 (boot screen)"

shot1=$(mcp_call 'tools/call' "{\"name\":\"vm_screenshot\",\"arguments\":{\"name\":\"${VM_NAME}\"}}")
out1="${OUT_DIR}/x0ne-vm-demo-1.png"
echo "$shot1" | python3 -c "
import json,sys,base64,os
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[])
img = next((c for c in content if c.get('type')=='image'), None)
if not img:
    txt = next((c for c in content if c.get('type')=='text'), {}).get('text','(no payload)')
    print('  ✗ no image returned:', txt)
    sys.exit(1)
data = base64.b64decode(img['data'])
with open('${out1}','wb') as f:
    f.write(data)
print(f'  ✓ wrote {len(data)} bytes → ${out1}')
"

# Helper: take a screenshot and write it to a file.
snap() {
    local n=$1
    local label=$2
    local out="${OUT_DIR}/x0ne-vm-demo-${n}.png"
    local body
    body=$(mcp_call 'tools/call' "{\"name\":\"vm_screenshot\",\"arguments\":{\"name\":\"${VM_NAME}\"}}")
    echo "$body" | OUT_PATH="$out" python3 -c "
import json,sys,base64,os
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[])
img = next((c for c in content if c.get('type')=='image'), None)
data = base64.b64decode(img['data'])
with open(os.environ['OUT_PATH'],'wb') as f:
    f.write(data)
print('  ✓ ' + str(len(data)) + ' bytes → ' + os.environ['OUT_PATH'])
"
    echo "$out"
}

# ─── 6. agent loop: read screen → press Return → type root → Return → uname ─

step "agent loop step 1: vm_key 'return' (wake the login prompt)"
mcp_call 'tools/call' "{\"name\":\"vm_key\",\"arguments\":{\"name\":\"${VM_NAME}\",\"key\":\"return\"}}" \
    | python3 -c "import json,sys; print('  ' + json.loads(sys.stdin.read())['result']['content'][0]['text'])"
sleep 1

step "vm_screenshot — capture 2 (after Return)"
out2=$(snap 2 "after-return")

step "agent loop step 2: vm_type 'root' (log in)"
mcp_call 'tools/call' "{\"name\":\"vm_type\",\"arguments\":{\"name\":\"${VM_NAME}\",\"text\":\"root\"}}" \
    | python3 -c "import json,sys; print('  ' + json.loads(sys.stdin.read())['result']['content'][0]['text'])"
sleep 0.5

step "agent loop step 3: vm_key 'return' (submit login)"
mcp_call 'tools/call' "{\"name\":\"vm_key\",\"arguments\":{\"name\":\"${VM_NAME}\",\"key\":\"return\"}}" \
    | python3 -c "import json,sys; print('  ' + json.loads(sys.stdin.read())['result']['content'][0]['text'])"
sleep 1.5

step "vm_screenshot — capture 3 (after login)"
out3=$(snap 3 "after-login")

step "agent loop step 4: vm_type 'uname -a' (run a command)"
mcp_call 'tools/call' "{\"name\":\"vm_type\",\"arguments\":{\"name\":\"${VM_NAME}\",\"text\":\"uname -a\"}}" \
    | python3 -c "import json,sys; print('  ' + json.loads(sys.stdin.read())['result']['content'][0]['text'])"
sleep 0.5

step "agent loop step 5: vm_key 'return' (execute it)"
mcp_call 'tools/call' "{\"name\":\"vm_key\",\"arguments\":{\"name\":\"${VM_NAME}\",\"key\":\"return\"}}" \
    | python3 -c "import json,sys; print('  ' + json.loads(sys.stdin.read())['result']['content'][0]['text'])"
sleep 1

step "vm_screenshot — capture 4 (after uname)"
out4=$(snap 4 "after-uname")

# ─── 7. diff progression ───────────────────────────────────────────────────

step "diff: did each step change the screen?"
prev="$out1"
for cur in "$out2" "$out3" "$out4"; do
    if cmp -s "$prev" "$cur"; then
        say "$(basename "$prev") == $(basename "$cur")   (no visible change)"
    else
        ok "$(basename "$prev") → $(basename "$cur")  changed"
    fi
    prev="$cur"
done

# ─── 8. open them ──────────────────────────────────────────────────────────

step "opening screenshots in image viewer"
if command -v xdg-open >/dev/null 2>&1; then
    for f in "$out1" "$out2" "$out3" "$out4"; do
        xdg-open "$f" >/dev/null 2>&1 &
        say "→ $f"
    done
else
    say "(install xdg-utils to auto-open; files are at $out1, $out2, $out3, $out4)"
fi

echo
echo "────────────────────────────────────────────────────────────────────"
echo " ✓ demo complete. To rebuild from scratch:"
echo "    virsh -c qemu:///session destroy ${VM_NAME}"
echo "    virsh -c qemu:///session undefine ${VM_NAME}"
echo "    rm -rf ~/.x0ne/vm/"
echo "    scripts/x0ne-vm-setup.sh"
echo "────────────────────────────────────────────────────────────────────"
