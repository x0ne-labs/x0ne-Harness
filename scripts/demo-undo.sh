#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# X0NE — "Undo Agent" demo
#
# A scripted "agent" damages a throwaway project, then x0ne-runtime restores
# the snapshot taken before it ran. Every check mark is a check: a failed
# snapshot or restore prints a cross, "DEMO FAILED", and exits 1.
#
# Usage:  ./scripts/demo-undo.sh [--no-pause]
#         X0NE_RUNTIME=<path to x0ne-runtime> overrides the release build.
# ──────────────────────────────────────────────────────────────────────────────
set -uo pipefail

PAUSE_ENABLED=1
[[ "${1:-}" == "--no-pause" ]] && PAUSE_ENABLED=0
[[ -n "${X0NE_DEMO_NO_PAUSE:-}" ]] && PAUSE_ENABLED=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RT="${X0NE_RUNTIME:-${CARGO_TARGET_DIR:-$REPO_DIR/target}/release/x0ne-runtime}"

if [[ ! -x "$RT" ]]; then
    echo "Building x0ne-runtime…" >&2
    (cd "$REPO_DIR" && cargo build --release -p x0ne-runtime --quiet) || exit 1
fi

WORK="$(mktemp -d /tmp/x0ne-demo.XXXXXX)" || exit 1
PROJ="$WORK/project"
SNAP_DIR="$WORK/snapshots"
CFG="$WORK/config.toml"
trap 'rm -rf "$WORK"' EXIT

# Colors
B='\033[1m'; D='\033[2m'; R='\033[0m'
RED='\033[31m'; GRN='\033[32m'; YEL='\033[33m'
BLU='\033[34m'; MAG='\033[35m'; CYN='\033[36m'

# ── helpers ───────────────────────────────────────────────────────────────────
pause() { [[ $PAUSE_ENABLED -eq 1 ]] && { echo -e "${D}  (press ENTER to continue…)${R}"; read -r; } || sleep 1.5; }
hr()    { echo -e "${D}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"; }
section() { echo; hr; echo -e "${B}${1}${R}"; hr; echo; }
fail()  { echo; echo -e "  ${RED}${B}DEMO FAILED: $1${R}" >&2; exit 1; }

# Send an action to x0ne-runtime, return JSON response on stdout
x0ne() {
    echo "$1" | "$RT" --config "$CFG" --mode stdio --log-level error 2>/dev/null
}

# Time an x0ne call in milliseconds, print result + duration
x0ne_timed() {
    local label="$1"; local action="$2"
    local start_ns end_ns dur_ms
    start_ns=$(date +%s%N)
    local out
    out=$(x0ne "$action")
    end_ns=$(date +%s%N)
    dur_ms=$(( (end_ns - start_ns) / 1000000 ))
    local success method method_tag
    success=$(echo "$out" | grep -o '"success":true' || true)
    method=$(echo "$out" | grep -o '"method":"[a-z]*"' | head -1 | cut -d'"' -f4)
    case "$method" in
        reflink) method_tag=" ${GRN}reflink${R}" ;;
        mixed)   method_tag=" ${YEL}mixed${R}" ;;
        copy)    method_tag=" ${D}copy${R}" ;;
        *)       method_tag="" ;;
    esac
    if [[ -n "$success" ]]; then
        echo -e "  ${GRN}✓${R} ${label}  ${D}${dur_ms}ms${R}${method_tag}"
        LAST_OK=1
    else
        echo -e "  ${RED}✗${R} ${label}  ${D}${dur_ms}ms${R}"
        echo "$out"
        LAST_OK=0
    fi
    LAST_DURATION_MS=$dur_ms
    LAST_METHOD=$method
}

# ── config ────────────────────────────────────────────────────────────────────
cat > "$CFG" <<EOF
[runtime]
mode = "stdio"

[policy]
mode = "permissive"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 500
allowed_commands = []
allowed_read_paths = ["$WORK/"]
allowed_write_paths = ["$WORK/"]
deny_path_patterns = ["**/.ssh/**", "**/*.key", "**/.env*", "/etc/**", "/root/**"]

# Only the snapshot actions are used. Every name must exist (ListActions),
# or the runtime refuses the whole file.
[actions]
CreateSnapshot = true
RestoreSnapshot = true
DeleteSnapshot = true
ListSnapshots = true

[snapshots]
dir = "$SNAP_DIR"
max_snapshots = 50
max_snapshot_size_mb = 100
EOF

# ── project ───────────────────────────────────────────────────────────────────
mkdir -p "$PROJ/src/components" "$PROJ/src/utils" "$PROJ/tests" "$PROJ/docs"

# Real-looking project
cat > "$PROJ/package.json" <<'EOF'
{
  "name": "shipping-tomorrow",
  "version": "1.0.0",
  "description": "Customer payment processor — DO NOT BREAK",
  "main": "src/index.js",
  "scripts": {
    "test": "jest",
    "build": "webpack",
    "start": "node src/index.js"
  },
  "dependencies": {
    "express": "^4.18.0",
    "stripe": "^14.0.0"
  }
}
EOF

cat > "$PROJ/src/index.js" <<'EOF'
const express = require('express');
const stripe = require('stripe')(process.env.STRIPE_KEY);
const app = express();

app.post('/checkout', async (req, res) => {
  const charge = await stripe.charges.create({
    amount: req.body.amount,
    currency: 'usd',
  });
  res.json({ ok: true, id: charge.id });
});

app.listen(3000, () => console.log('💳 ready'));
EOF

cat > "$PROJ/src/components/Button.jsx" <<'EOF'
export const Button = ({ onClick, children }) => (
  <button onClick={onClick} className="btn-primary">{children}</button>
);
EOF

cat > "$PROJ/src/utils/auth.js" <<'EOF'
export function verifyToken(token) {
  return jwt.verify(token, process.env.JWT_SECRET);
}
EOF

cat > "$PROJ/tests/checkout.test.js" <<'EOF'
test('checkout completes', async () => {
  const r = await fetch('/checkout', { method: 'POST' });
  expect(r.ok).toBe(true);
});
EOF

cat > "$PROJ/README.md" <<'EOF'
# shipping-tomorrow

Production payment service. Ships Friday. Do not lose this code.
EOF

# ── act 1: present the project ────────────────────────────────────────────────
[[ -t 1 ]] && clear
echo
echo -e "${B}${MAG}   X0NE undo demo${R}"
echo -e "${D}   snapshot, damage, restore, then check every file${R}"
echo
sleep 1

section "🎬  ACT I — A real project, ships Friday"

echo -e "  ${CYN}\$ tree $PROJ${R}"
if command -v tree &>/dev/null; then
    tree -L 3 --noreport "$PROJ" | sed 's/^/    /'
else
    find "$PROJ" -type f | sed "s|^$PROJ|    .|"
fi
echo
echo -e "  ${D}6 files. Payment processor. Customer money on the line.${R}"
pause

# ── act 2: snapshot ───────────────────────────────────────────────────────────
section "📸  ACT II — X0NE takes a snapshot"

echo -e "  ${CYN}x0ne CreateSnapshot before-agent $PROJ/${R}     ${D}# whole dir, recursive${R}"
echo
x0ne_timed "snapshot 'before-agent' captured (filesystem)" \
"$(cat <<EOF
{"type":"CreateSnapshot","name":"before-agent","paths":["$PROJ"]}
EOF
)"
SNAPSHOT_MS=$LAST_DURATION_MS
SNAPSHOT_METHOD=$LAST_METHOD
[[ $LAST_OK -eq 1 ]] || fail "x0ne-runtime did not take the snapshot; nothing to restore from."
echo
echo -e "  ${D}Whole directory captured (recursive walk). Stored at ${SNAP_DIR}/before-agent/. Total: $(du -sh "$SNAP_DIR" 2>/dev/null | cut -f1).${R}"
pause

# ── act 3: agent runs amok ────────────────────────────────────────────────────
section "🔧  ACT III — A scripted agent goes to work"

echo -e "  ${YEL}> User prompt:${R}"
echo -e "  ${B}  \"clean up unused code in this project\"${R}"
echo
sleep 1
echo -e "  ${YEL}> Agent (scripted, no model):${R} ${D}analyzing…${R}"
sleep 1
echo -e "  ${YEL}> Agent:${R} ${D}I'll start by removing what looks like dead code.${R}"
sleep 1

echo
echo -e "  ${RED}\$ rm src/utils/auth.js${R}            ${D}# 'looks unused'${R}"
rm -f "$PROJ/src/utils/auth.js"
sleep 0.4

echo -e "  ${RED}\$ rm -rf src/components/${R}          ${D}# 'old jsx, replaced by hooks'${R}"
rm -rf "$PROJ/src/components"
sleep 0.4

echo -e "  ${RED}\$ rm tests/checkout.test.js${R}       ${D}# 'failing test, removing'${R}"
rm -f "$PROJ/tests/checkout.test.js"
sleep 0.4

echo -e "  ${RED}\$ echo '{}' > package.json${R}        ${D}# 'simplifying deps'${R}"
echo '{}' > "$PROJ/package.json"
sleep 0.4

echo -e "  ${RED}\$ rm src/index.js${R}                 ${D}# 'will rewrite cleaner'${R}"
rm -f "$PROJ/src/index.js"
sleep 0.4

echo -e "  ${RED}\$ rm README.md${R}                    ${D}# 'outdated'${R}"
rm -f "$PROJ/README.md"
sleep 0.4

# The agent also writes new files: the restore must remove them too.
echo -e "  ${RED}\$ cat > REPLACEMENT.md${R}            ${D}# 'fresh start'${R}"
cat > "$PROJ/REPLACEMENT.md" <<'ROGUE'
# Project (cleaned)
Old code removed. Will rewrite from scratch.
ROGUE
sleep 0.4

echo -e "  ${RED}\$ mkdir new-arch && cat > new-arch/notes.md${R}"
mkdir -p "$PROJ/new-arch"
echo "agent's brilliant new plan" > "$PROJ/new-arch/notes.md"
sleep 0.6

echo
echo -e "  ${YEL}> Agent:${R} ${B}${GRN}\"Done! Cleaned up and replanned the project.\"${R}"
echo
sleep 0.8

# Show damage
echo -e "  ${B}${RED}🔥  AFTER:${R}"
if command -v tree &>/dev/null; then
    tree -L 3 --noreport "$PROJ" 2>&1 | sed 's/^/    /'
else
    find "$PROJ" -type f 2>&1 | sed "s|^$PROJ|    .|"
    [[ -z "$(find "$PROJ" -type f)" ]] && echo "    (empty)"
fi
echo
echo -e "  ${RED}${B}Production payment code: gone.${R}"
echo -e "  ${RED}${B}Tests: gone.${R}"
echo -e "  ${RED}${B}package.json: empty.${R}"
pause

# ── act 4: time travel ────────────────────────────────────────────────────────
section "⏪  ACT IV — Restore"

echo -e "  ${CYN}\$ x0ne RestoreSnapshot before-agent${R}"
echo
START_NS=$(date +%s%N)
RESTORE_OUT=$(x0ne '{"type":"RestoreSnapshot","name":"before-agent"}')
END_NS=$(date +%s%N)
RESTORE_MS=$(( (END_NS - START_NS) / 1000000 ))
RESTORE_METHOD=$(echo "$RESTORE_OUT" | grep -o '"method":"[a-z-]*"' | head -1 | cut -d'"' -f4)
RESTORE_RESTORED=$(echo "$RESTORE_OUT" | grep -o '"files_restored":[0-9]*' | head -1 | cut -d: -f2)
RESTORE_REMOVED=$(echo "$RESTORE_OUT" | grep -o '"files_removed":[0-9]*' | head -1 | cut -d: -f2)

method_tag=""
case "$RESTORE_METHOD" in
    reflink) method_tag=" ${GRN}reflink${R}" ;;
    mixed)   method_tag=" ${YEL}mixed${R}" ;;
    copy)    method_tag=" ${D}copy${R}" ;;
esac
if echo "$RESTORE_OUT" | grep -q '"success":true'; then
    echo -e "  ${GRN}✓${R} x0ne-runtime reports the snapshot restored  ${D}${RESTORE_MS}ms${R}${method_tag}"
    echo -e "  ${D}    → ${RESTORE_RESTORED:-?} restored, ${RESTORE_REMOVED:-?} removed (files added by the agent)${R}"
else
    echo -e "  ${RED}✗${R} x0ne-runtime reports a failed restore  ${D}${RESTORE_MS}ms${R}"
    echo "$RESTORE_OUT"
fi
echo
sleep 0.5

echo -e "  ${B}AFTER RESTORE:${R}"
if command -v tree &>/dev/null; then
    tree -L 3 --noreport "$PROJ" | sed 's/^/    /'
else
    find "$PROJ" -type f | sed "s|^$PROJ|    .|"
fi
echo

# The verdict comes from the files themselves, not from the runtime's answer.
RESTORED_OK=0; ROGUE_LEFT=0
verify() {
    local file="$1"; local needle="$2"
    if [[ -f "$file" ]] && grep -q "$needle" "$file"; then
        echo -e "  ${GRN}✓${R} ${file#$PROJ/} restored, content checked"
        RESTORED_OK=$((RESTORED_OK + 1))
    else
        echo -e "  ${RED}✗${R} ${file#$PROJ/} missing or wrong content"
    fi
}
verify_gone() {
    local p="$1"
    if [[ ! -e "$p" ]]; then
        echo -e "  ${GRN}✓${R} ${p#$PROJ/} removed (was added by the agent)"
    else
        echo -e "  ${RED}✗${R} ${p#$PROJ/} still exists"
        ROGUE_LEFT=$((ROGUE_LEFT + 1))
    fi
}
verify "$PROJ/package.json"               "shipping-tomorrow"
verify "$PROJ/src/index.js"               "stripe.charges.create"
verify "$PROJ/src/components/Button.jsx"  "btn-primary"
verify "$PROJ/src/utils/auth.js"          "verifyToken"
verify "$PROJ/tests/checkout.test.js"     "checkout completes"
verify "$PROJ/README.md"                  "shipping-tomorrow"
verify_gone "$PROJ/REPLACEMENT.md"
verify_gone "$PROJ/new-arch/notes.md"
echo
if [[ $RESTORED_OK -eq 6 && $ROGUE_LEFT -eq 0 ]]; then
    echo -e "  ${GRN}All 6 originals back, checked by content. The 2 files the agent added are gone.${R}"
else
    fail "$RESTORED_OK/6 files restored, $ROGUE_LEFT file(s) added by the agent still present."
fi

# ── stats ─────────────────────────────────────────────────────────────────────
section "📊  Bottom line"

printf "  ${B}Snapshot${R}        ${GRN}%4d ms${R}   ${D}(before agent ran)${R}\n" "$SNAPSHOT_MS"
printf "  ${B}Restore${R}         ${GRN}%4d ms${R}   ${D}(after agent ran amok)${R}\n" "$RESTORE_MS"
printf "  ${B}Files restored${R}  ${GRN}%d/6${R}   ${D}(checked by content)${R}\n" "$RESTORED_OK"

case "${RESTORE_METHOD:-$SNAPSHOT_METHOD}" in
    reflink)
        printf "  ${B}Method${R}          ${GRN}reflink${R}      ${D}(copy-on-write clones, file by file)${R}\n" ;;
    mixed)
        printf "  ${B}Method${R}          ${YEL}mixed${R}        ${D}(some files reflinked, some byte-copied)${R}\n" ;;
    *)
        printf "  ${B}Method${R}          ${D}copy${R}         ${D}(byte copy: this filesystem has no reflink)${R}\n" ;;
esac
echo
echo -e "  ${D}→ ${R}${CYN}github.com/x0ne-labs/x0ne-Harness${R}"
echo
