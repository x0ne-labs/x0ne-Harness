#!/bin/sh
# Cases the Codex fail-closed hook envelope must decide correctly.
# A failing case means Codex could run a tool with no x0ne verdict.
set -u
here=$(dirname -- "$0")
here=$(cd -- "$here" && pwd)
wrap="$here/x0ne-codex-pretooluse.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# A fake `x0ne` whose `hook pretooluse` behaviour we script: $1 = shell body.
fake() { # $1 body -> prints a dir to put first on PATH
    d=$(mktemp -d "$tmp/bin.XXXXXX")
    cat > "$d/x0ne" <<EOF
#!/bin/sh
$1
EOF
    chmod +x "$d/x0ne"
    printf '%s' "$d"
}

run() { # stdin empty; env already set by caller; prints "rc|out"
    out=$(printf '%s' '{"tool_name":"shell"}' | "$wrap" 2>/dev/null)
    printf '%s|%s' "$?" "$out"
}

expect_deny() { # $1 label ; env X0NE_BIN/PATH set by caller
    r=$(run); rc=${r%%|*}; out=${r#*|}
    if [ "$rc" = 2 ] && printf '%s' "$out" | grep -q '"permissionDecision":"deny"'; then
        echo "  ok   $1"
    else echo "  FAIL $1: rc=$rc out=$out"; fail=1; fi
}
expect_allow() { # $1 label
    r=$(run); rc=${r%%|*}
    if [ "$rc" = 0 ]; then echo "  ok   $1"; else echo "  FAIL $1: rc=$rc"; fail=1; fi
}
expect_passthru_deny() { # $1 label ; the policy's own deny, forwarded verbatim
    r=$(run); rc=${r%%|*}; out=${r#*|}
    if [ "$rc" = 2 ] && printf '%s' "$out" | grep -q '"permissionDecisionReason":"policy said no"'; then
        echo "  ok   $1"
    else echo "  FAIL $1: rc=$rc out=$out"; fail=1; fi
}

echo "fail closed when x0ne cannot render a verdict:"
X0NE_BIN="$tmp/does-not-exist" PATH="/usr/bin:/bin" expect_deny "absent binary"
d=$(fake 'exit 139'); X0NE_BIN="$d/x0ne" expect_deny "crash (exit 139)"
d=$(fake 'exit 1');   X0NE_BIN="$d/x0ne" expect_deny "unexpected exit 1"
d=$(fake 'sleep 30'); X0NE_BIN="$d/x0ne" X0NE_HOOK_TIMEOUT=1 expect_deny "hang (timeout)"

echo "pass the verdict through when x0ne answers:"
d=$(fake 'exit 0'); X0NE_BIN="$d/x0ne" expect_allow "silent allow"
d=$(fake 'printf "{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}\n"; exit 0'); X0NE_BIN="$d/x0ne" expect_allow "allow with JSON"
d=$(fake 'printf "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"policy said no\"}}\n"; exit 2'); X0NE_BIN="$d/x0ne" expect_passthru_deny "policy deny forwarded"

exit $fail
