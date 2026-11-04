#!/usr/bin/env bash
# The cases the memory-cap hook must decide correctly. Run by `make loop`.
# A failing case here means an agent can run a bare cargo (machine crash
# class) or is refused a wrapped one (the guard gets removed).
set -u
hook="$(dirname "$0")/require-capped-cargo.sh"
fail=0

decide() { # $1 = raw stdin payload → prints "deny" or "allow"
    out="$(printf '%s' "$1" | bash "$hook")"
    if printf '%s' "$out" | grep -q '"permissionDecision": *"deny"'; then echo deny; else echo allow; fi
}
cmd_json() { python3 -c 'import json,sys; print(json.dumps({"tool_input":{"command":sys.argv[1]}}))' "$1"; }
expect() { # $1 = expected, $2 = command
    got="$(decide "$(cmd_json "$2")")"
    if [ "$got" != "$1" ]; then echo "  ✗ expected $1, got $got: $2"; fail=1; else echo "  ✓ $1: $2"; fi
}

echo "deny — bare heavy cargo:"
expect deny 'cargo test --workspace'
expect deny 'cd x && cargo build'
expect deny 'X0NE_MEMCAP=8G cargo clippy -- -D warnings'
expect deny 'nice cargo test'
expect deny '/home/u/.cargo/bin/cargo test'
expect deny 'cargo test # scripts/capped'
expect deny 'echo scripts/capped; cargo test'
expect deny 'make test; cargo test'
echo "allow — wrapped or light:"
expect allow 'scripts/capped cargo test --workspace'
expect allow 'X0NE_MEMCAP=8G scripts/capped cargo test -p x'
expect allow 'cd ~/x0ne && scripts/capped cargo build'
expect allow 'make test'
expect allow 'systemd-run --scope -p MemoryMax=8G cargo test'
expect allow 'cargo fmt --all -- --check'
expect allow 'cargo metadata'
echo "fail closed — unreadable payload:"
got="$(decide 'not json at all')"
if [ "$got" != deny ]; then echo "  ✗ garbage payload must be denied, got $got"; fail=1; else echo "  ✓ deny: garbage payload"; fi
got="$(decide '{"tool_input":{}}')"
if [ "$got" != deny ]; then echo "  ✗ missing command must be denied, got $got"; fail=1; else echo "  ✓ deny: missing command"; fi

exit $fail
