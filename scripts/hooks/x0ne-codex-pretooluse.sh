#!/bin/sh
# Fail-closed envelope for the x0ne PreToolUse hook on Codex.
#
# Claude Code has `onFailure: "block"`, so a missing, crashed or timed-out hook
# blocks the tool call. Codex has no such option: pointed straight at `x0ne
# hook pretooluse`, a missing binary (exit 127), a crash or a hang lets the
# call reach the host with no policy (measured 2026-10-16: absent binary →
# exit 127, no decision). Wire Codex's PreToolUse command at THIS script
# instead: it denies, in Codex's format, whenever x0ne cannot render a verdict.
#
# Env: X0NE_BIN (default `x0ne`), X0NE_HOOK_TIMEOUT seconds (default 25, under
# Codex's own timeout). Stdin (the tool call) and any extra args are forwarded.
set -u

BIN="${X0NE_BIN:-x0ne}"
TIMEOUT="${X0NE_HOOK_TIMEOUT:-25}"

# permissionDecision=deny plus exit 2: the two signals Codex treats as a block.
# $1 is a plain reason with no quotes or backslashes, so it needs no escaping.
deny() {
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$1"
    exit 2
}

# No binary means no policy: refuse rather than run the tool unjudged.
command -v "$BIN" >/dev/null 2>&1 || deny "x0ne hook binary not found; refusing (fail closed)"

# Run under a hard timeout, forwarding stdin and args; capture the verdict.
out=$(timeout -k 2 "$TIMEOUT" "$BIN" hook pretooluse --agent codex "$@")
rc=$?

case $rc in
    0) [ -n "$out" ] && printf '%s\n' "$out"; exit 0 ;;   # allow (often silent)
    2) [ -n "$out" ] && printf '%s\n' "$out"; exit 2 ;;   # policy deny
    124 | 137) deny "x0ne hook timed out after ${TIMEOUT}s; refusing (fail closed)" ;;
    127) deny "x0ne hook binary not found; refusing (fail closed)" ;;
    *) deny "x0ne hook failed (exit $rc); refusing (fail closed)" ;;
esac
