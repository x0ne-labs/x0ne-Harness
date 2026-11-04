#!/usr/bin/env bash
# The undo demo must say what happened. Two runs:
#   1. a fake runtime that answers success to everything and restores nothing:
#      the demo must exit non-zero, print a cross and never claim the files back;
#   2. the real runtime ($X0NE_RUNTIME, or the release build): exit 0, 6/6 restored.
# Usage: X0NE_RUNTIME=target/debug/x0ne-runtime scripts/test-demo-undo.sh
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
T=$(mktemp -d) || exit 1
trap 'rm -rf "$T"' EXIT
fails=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }

# 1. A runtime that lies. Placed where the demo looks by default, so the same
#    test also runs against a demo without the X0NE_RUNTIME override.
mkdir -p "$T/fake/release"
cat > "$T/fake/release/x0ne-runtime" <<'EOF'
#!/bin/sh
cat >/dev/null
echo '{"success":true,"output":{"method":"reflink","files_restored":6,"files_removed":2}}'
EOF
chmod +x "$T/fake/release/x0ne-runtime"
env -u X0NE_RUNTIME CARGO_TARGET_DIR="$T/fake" X0NE_DEMO_NO_PAUSE=1 bash scripts/demo-undo.sh > "$T/lie.out" 2>&1
lie_rc=$?
check "a restore that restored nothing exits non-zero" '[ "$lie_rc" -ne 0 ]'
check "it prints a cross" 'grep -q "✗" "$T/lie.out"'
check "it never claims the files are back" '! grep -q "originals back" "$T/lie.out"'
check "it says DEMO FAILED" 'grep -q "DEMO FAILED" "$T/lie.out"'

# 2. The real runtime.
RT=${X0NE_RUNTIME:-${CARGO_TARGET_DIR:-target}/release/x0ne-runtime}
if [ -x "$RT" ]; then
    X0NE_RUNTIME="$RT" X0NE_DEMO_NO_PAUSE=1 bash scripts/demo-undo.sh > "$T/real.out" 2>&1
    real_rc=$?
    check "the real runtime restores: exit 0" '[ "$real_rc" -eq 0 ]'
    check "6/6 files restored, checked by content" 'grep -q "Files restored.*6/6" "$T/real.out"'
    check "no cross" '! grep -q "✗" "$T/real.out"'
    [ "$real_rc" -eq 0 ] || sed 's/^/    | /' "$T/real.out"
else
    echo "FAIL no runtime at $RT (set X0NE_RUNTIME)"; fails=$((fails + 1))
fi
[ "$fails" -eq 0 ] && echo "demo-undo: all checks passed" || echo "demo-undo: $fails check(s) failed"
exit $((fails > 0))
