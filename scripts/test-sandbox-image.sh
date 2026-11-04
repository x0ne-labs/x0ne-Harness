#!/usr/bin/env bash
# Starts the release binaries in the image run-sandboxed.sh pins, through
# run-sandboxed.sh itself. Usage: test-sandbox-image.sh incus|podman
# Checks: x0ne and x0ne-runtime start (glibc of the image is new enough),
# X0NE_SANDBOX is declared, and the host's model choice reaches the sandbox.
set -uo pipefail
backend=${1:?usage: $0 incus|podman}
here=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/x0ne-sbx-image.XXXXXX")
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/work"
: >"$T/env"; chmod 600 "$T/env"

# A RunCommand goes through the runtime's env_clear allowlist, which
# drops RUSTUP_HOME; `cargo --version` exited 1 there on 2026-10-06.
cat >"$T/work/rt.toml" <<'TOML'
[runtime]
mode = "stdio"
[policy]
mode = "auto"
default_effect = "deny"
max_read_bytes = 1048576
max_list_entries = 500
allowed_commands = ["cargo"]
allowed_read_paths = ["/work"]
allowed_write_paths = []
deny_path_patterns = []
[actions]
RunCommand = true
[snapshots]
dir = "/tmp/x0ne-snapshots"
max_snapshots = 0
max_snapshot_size_mb = 0
[desktop]
enabled = false
TOML

inner='x0ne --version 2>&1 | head -1; x0ne-runtime --version 2>&1 | head -1
echo "sandbox=${X0NE_SANDBOX:-unset} provider=${X0NE_LLM_PROVIDER:-unset} model=${X0NE_MODEL:-unset}"
if command -v cargo >/dev/null; then
  echo "{\"type\":\"RunCommand\",\"command\":\"cargo\",\"args\":[\"--version\"]}" |
    x0ne-runtime --config /work/rt.toml --mode stdio --isolation container 2>/dev/null | tail -1
else
  echo "no cargo in this image"
fi'

X0NE_WORK_DIR="$T/work" X0NE_SANDBOX_ENV="$T/env" X0NE_SBX_NAME="x0ne-sbx-imgtest-$backend" \
X0NE_LLM_PROVIDER=anthropic X0NE_MODEL=claude-haiku-4-5 \
X0NE_SBX_TEST_CMD="$inner" "$here/run-sandboxed.sh" "$backend" >"$T/out" 2>&1
cat "$T/out"

fail=0
check() { if grep -qE "$1" "$T/out"; then echo "ok   $2"; else echo "FAIL $2"; fail=1; fi; }
nocheck() { if grep -qE "$1" "$T/out"; then echo "FAIL $2"; fail=1; else echo "ok   $2"; fi; }
nocheck 'GLIBC_[0-9.]+. not found' "binaries load in the image (glibc)"
check '^x0ne [0-9]+\.[0-9]+' "x0ne --version"
check '^x0ne-runtime [0-9]+\.[0-9]+' "x0ne-runtime --version"
check "sandbox=$backend " "X0NE_SANDBOX declared"
check 'provider=anthropic model=claude-haiku-4-5' "host model choice forwarded"
if ! grep -q '^no cargo in this image' "$T/out"; then
    check '"exit_code":0,"stdout":"cargo [0-9]+\.' "cargo --version through the runtime exits 0"
fi
exit $fail
