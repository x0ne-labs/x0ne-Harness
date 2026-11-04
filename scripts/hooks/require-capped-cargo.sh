#!/usr/bin/env bash
# Claude Code PreToolUse hook (Bash tool): refuse a heavy `cargo` invocation
# that is not running inside a memory-capped cgroup.
#
# Why (2026-08-16): two machine crashes came from test binaries eating all RAM
# during mutation campaigns run by agents. Humans forget wrappers; agents do
# too. The hook is the mechanism; this header is the explanation.
#
# Accepted forms: `scripts/capped cargo …`, `make …` (the Makefile wraps
# CARGO itself), anything already under `systemd-run`.
# Light subcommands (fmt, --version, metadata, tree, search, help) pass.
#
# SECURITY (audit 2026-09-11, 04-F22/F09): this hook FAILS CLOSED. Until then
# an unreadable payload yielded an empty command and `exit 0` — a guard with
# a silent off switch — and the allow test was a substring match, so
# `cargo test # scripts/capped` passed. Now the capped forms are erased from
# the line BEFORE the heavy pattern is searched, so only a cargo that is
# actually wrapped is exempt. `scripts/hooks/test-require-capped-cargo.sh`
# holds the cases; `make loop` runs it.

set -u

deny() {
    python3 -c 'import json,sys
print(json.dumps({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":sys.argv[1]}}))' "$1"
    exit 0
}

input="$(cat)"
cmd="$(printf '%s' "$input" | python3 -c 'import json,sys
d=json.load(sys.stdin); c=d.get("tool_input",{}).get("command")
if c is None: sys.exit(3)
print(c)' 2>/dev/null)" || {
    deny "Refused: the memory-cap hook could not read the command (unreadable payload). It refuses rather than let an uncapped cargo through."
}

# Erase the WRAPPED forms, so what remains is what would run bare:
#   scripts/capped cargo …   → CAPPED …
#   systemd-run … cargo …    → CAPPED …   (up to the next separator)
#   make …  as a command     → MAKE …     (the Makefile caps CARGO itself)
stripped="$(printf '%s' "$cmd" | sed -E \
    -e 's#(^|[;&|(]\s*)(\S*/)?scripts/capped\s+#\1CAPPED #g' \
    -e 's#(^|[;&|(]\s*)systemd-run[^;&|]*#\1CAPPED #g' \
    -e 's#(^|[;&|(]\s*)make(\s|$)#\1MAKE\2#g')"

# A heavy cargo subcommand appearing as a command (start of line, or after
# ; && || | or a subshell paren), possibly prefixed by env assignments or a
# harmless launcher (env, nice, time, nohup, xargs), by path or by name.
heavy='(^|[;&|(]\s*)((env|nice|time|nohup|xargs)\s+(-\S+\s+)*)?([A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(\S*/)?cargo\s+(\+\S+\s+)?(test|nextest|run|bench|build|check|clippy|doc|miri|llvm-cov|tarpaulin|mutants)\b'
if printf '%s' "$stripped" | grep -qE "$heavy"; then
    deny "Refused: heavy \`cargo\` outside a memory cap. Two machine crashes on 2026-08-15/16 (a mutation campaign turned a test into a memory bomb inside the session cgroup). Re-run the SAME command prefixed with \`scripts/capped\` (e.g. \`scripts/capped cargo test --workspace\`, X0NE_MEMCAP=16G by default) or through \`make test\`/\`make scenarios\`, which already cap. One cargo at a time."
fi
exit 0
