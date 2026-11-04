#!/usr/bin/env bash
# x0ne-vm-agent-scenario — a narrated agent loop with per-step screenshots.
#
# This is the "parlant" demo: simulates an LLM agent investigating a fresh
# Alpine system. Each step:
#   1. Print what the agent is "thinking" (narrative)
#   2. Execute one or more MCP tool calls
#   3. Take a screenshot
#   4. Save it with a semantic name: NN-<label>.png
#
# At the end, build an annotated composite image grid via ImageMagick,
# open it in the viewer, and print a transcript ready to be reviewed.
#
# Usage:
#   bash scripts/x0ne-vm-agent-scenario.sh
#
# Outputs:
#   /tmp/x0ne-vm-scenario/NN-<label>.png   per-step screenshots
#   /tmp/x0ne-vm-scenario/composite.png     6-panel grid with labels

set -uo pipefail

VM_NAME=${VM_NAME:-x0ne-test}
MCP_BIN="${CARGO_TARGET_DIR:-$(pwd)/target}/release/x0ne-vm-mcp"
OUT_DIR=${OUT_DIR:-/tmp/x0ne-vm-scenario}
mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.png

C_DIM="\033[2m"; C_BRIGHT="\033[1;36m"; C_AGENT="\033[35m"; C_RESET="\033[0m"
C_OK="\033[32m✓\033[0m"

mcp_one() {
    local method=$1 params=${2:-'{}'}
    printf '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}\n' \
        "$RANDOM" "$method" "$params" | "$MCP_BIN" 2>/dev/null | head -1
}
mcp_tool() {
    local name=$1 args=${2:-'{}'}
    mcp_one 'tools/call' "{\"name\":\"${name}\",\"arguments\":${args}}"
}
snap() {
    local n=$1 label=$2
    local out=$(printf '%s/%02d-%s.png' "$OUT_DIR" "$n" "$label")
    mcp_tool vm_screenshot "{\"name\":\"${VM_NAME}\"}" | TARGET="$out" python3 -c "
import json,sys,os,base64
d = json.loads(sys.stdin.read())
content = d.get('result',{}).get('content',[]) or []
img = next((c for c in content if c.get('type')=='image'), None)
if not img: sys.exit(2)
data = base64.b64decode(img['data'])
with open(os.environ['TARGET'],'wb') as f: f.write(data)
" 2>/dev/null
    printf '%s' "$out"
}

agent_says() {
    printf "\n${C_AGENT}[agent]${C_RESET} %s\n" "$*"
}
act() {
    printf "${C_DIM}        → %s${C_RESET}\n" "$*"
}
narrate_step() {
    printf "\n${C_BRIGHT}━━━ step %s/%s — %s ━━━${C_RESET}\n" "$1" "$TOTAL" "$2"
}

TOTAL=10
N=0

# ─── step 0: baseline ───────────────────────────────────────────────────────

N=$((N+1)); narrate_step "$N" "wake the screen"
agent_says "I see a fresh Linux system. Let me check what's on screen."
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 0.6
out=$(snap "$N" baseline)
act "captured ${out}"

# ─── step 2: login as root ──────────────────────────────────────────────────

N=$((N+1)); narrate_step "$N" "log in as root"
agent_says "There's a 'localhost login:' prompt. Default Alpine account is root with no password."
act "vm_type 'root'"
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"root\"}" >/dev/null
act "vm_key 'return'"
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 1.2
out=$(snap "$N" after-login)
act "captured ${out}"

# ─── step 3: clear screen ──────────────────────────────────────────────────

N=$((N+1)); narrate_step "$N" "clear screen with Ctrl-L"
agent_says "The boot output is noisy. Let me clear it for clarity."
act "vm_key 'ctrl+l'"
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"ctrl+l\"}" >/dev/null
sleep 0.5
out=$(snap "$N" cleared)
act "captured ${out}"

# ─── step 4: identify the system ───────────────────────────────────────────

N=$((N+1)); narrate_step "$N" "investigate the system"
agent_says "Now let me find out what kernel and distro I'm on."
act "vm_type 'uname -a && cat /etc/os-release | head -3'"
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"uname -a && cat /etc/os-release | head -3\"}" >/dev/null
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 1.2
out=$(snap "$N" system-info)
act "captured ${out}"

# ─── step 5: leave a signature file ────────────────────────────────────────

N=$((N+1)); narrate_step "$N" "write a marker file"
agent_says "I want to prove I can write to the filesystem. Let me drop a marker."
act "vm_type 'echo \"x0ne-was-here-\$(date +%s)\" > /tmp/marker.txt'"
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"echo x0ne-was-here-\$(date +%s) > /tmp/marker.txt\"}" >/dev/null
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 0.4
act "vm_type 'cat /tmp/marker.txt'"
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"cat /tmp/marker.txt\"}" >/dev/null
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 1
out=$(snap "$N" marker)
act "captured ${out}"

# ─── step 6: a longer multi-action sequence ────────────────────────────────

N=$((N+1)); narrate_step "$N" "small ASCII art"
agent_says "And finally, prove vm_type handles punctuation reliably."
# Echo a small banner.
mcp_tool vm_type "{\"name\":\"${VM_NAME}\",\"text\":\"printf '###  X0NE-VM was here  ###\\\\n'\"}" >/dev/null
mcp_tool vm_key "{\"name\":\"${VM_NAME}\",\"key\":\"return\"}" >/dev/null
sleep 0.6
out=$(snap "$N" banner)
act "captured ${out}"

# ─── final: build composite grid ───────────────────────────────────────────

printf "\n${C_BRIGHT}━━━ composite — assembling all panels ━━━${C_RESET}\n"

# Annotate each panel with its label so the grid tells a story.
ANN_DIR="${OUT_DIR}/_annotated"
mkdir -p "$ANN_DIR"
for f in "$OUT_DIR"/*.png; do
    [[ -d "$f" ]] && continue
    base=$(basename "$f" .png)
    convert "$f" \
        -resize 640x \
        -background '#0a0a0a' -fill white -font DejaVu-Sans-Mono \
        -pointsize 18 -gravity North \
        -splice 0x36 -annotate +0+8 "$base" \
        "$ANN_DIR/$base.png" 2>/dev/null
done

COMPOSITE="${OUT_DIR}/composite.png"
montage "$ANN_DIR"/*.png \
    -tile 3x2 -geometry "640x+8+8" -background '#000' \
    "$COMPOSITE" 2>/dev/null

if [[ -f "$COMPOSITE" ]]; then
    printf "  ${C_OK} composite written: %s (%s bytes)\n" "$COMPOSITE" "$(stat -c %s "$COMPOSITE")"
    if command -v xdg-open >/dev/null; then
        xdg-open "$COMPOSITE" >/dev/null 2>&1 &
        printf "  ${C_OK} opened in image viewer\n"
    fi
else
    echo "  composite failed — check montage output"
fi

printf "\n${C_BRIGHT}═══ transcript ═══${C_RESET}\n"
ls -la "$OUT_DIR"/*.png 2>/dev/null | awk '{printf "  %s  %8s  %s\n", $9, $5, ""}' | sed 's|.*/||; s|^|  |'
echo
echo "All screenshots: ${OUT_DIR}/"
echo "Composite grid:  ${COMPOSITE}"
