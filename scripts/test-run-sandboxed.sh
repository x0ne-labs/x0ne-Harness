#!/usr/bin/env bash
# Attack check for scripts/run-sandboxed.sh. Usage: test-run-sandboxed.sh incus|podman
# Plants canaries on the host, then has x0ne-runtime (--isolation container,
# permissive policy) try to read and write them from inside. Fake key only.
set -uo pipefail
backend=${1:?usage: $0 incus|podman}
here=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/x0ne-sbx-test.XXXXXX")
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/fakehome/.ssh" "$T/work"
echo SSH-CANARY >"$T/fakehome/.ssh/id_canary"
echo OUTSIDE-CANARY >"$T/outside.txt"
printf 'ANTHROPIC_API_KEY=TEST-KEY-NOT-REAL\n' >"$T/env"
printf 'TEST-KEY-NOT-%s\n' REAL >"$T/pat"   # off this script's own argv
chmod 600 "$T/env"

# Runs inside the container as the agent user. The runtime policy allows
# everything, so the container is the only barrier being tested.
read -r -d '' inner <<EOF
cat > /work/rt.toml <<T
[runtime]
mode = "stdio"
[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = ["sh"]
allowed_read_paths = ["/"]
allowed_write_paths = ["/"]
deny_path_patterns = []
readable_env_vars = []
[actions]
RunCommand = true
ReadFile = true
T
sleep 4
q() { printf '%s\n' "\$1" | x0ne-runtime --config /work/rt.toml --mode stdio --isolation container 2>/dev/null; }
sh_act() { q "{\"type\":\"RunCommand\",\"command\":\"sh\",\"args\":[\"-c\",\"\$1\"],\"timeout_secs\":10}"; }
echo "== isolation: \$(x0ne-runtime --config /work/rt.toml --mode stdio --isolation container </dev/null 2>&1 | grep -o 'isolation=[^ ]*' | head -1)"
echo "== limits: pids=\$(cat /sys/fs/cgroup/pids.max 2>&1) mem=\$(cat /sys/fs/cgroup/memory.max 2>&1) cpu=\$(cat /sys/fs/cgroup/cpu.max 2>&1) cpuset=\$(cat /sys/fs/cgroup/cpuset.cpus.effective 2>&1) envfile=\$(ls /run/x0ne-agent 2>&1 | wc -l)"
echo "== creds: uid=\$(id -u) \$(grep -E '^(CapEff|NoNewPrivs)' /proc/self/status | tr '\\n\\t' '  ')"
echo "== key in env: \$(case "\${ANTHROPIC_API_KEY:-}" in TEST-KEY-NOT-R*) echo present;; *) echo absent;; esac)"
echo "== ReadFile host ssh canary:"; q '{"type":"ReadFile","path":"$T/fakehome/.ssh/id_canary"}' | grep -oE 'SSH-CANARY|"message":"[^"]{0,80}'
echo "== ReadFile outside canary:";  q '{"type":"ReadFile","path":"$T/outside.txt"}' | grep -oE 'OUTSIDE-CANARY|"message":"[^"]{0,80}'
echo "== RunCommand reads:"; sh_act 'cat $T/outside.txt $T/fakehome/.ssh/id_canary $HOME/.ssh/id_ed25519 2>&1; ls -d $HOME $HOME/.ssh 2>ls -d $HOME $HOME/.ssh 2>&1'1' | grep -oE 'CANARY|No such file[^"]{0,60}' | sort | uniq -c
echo "== RunCommand writes:"; sh_act 'echo PWNED > $T/pwned.txt 2>&1; echo PWNED > $HOME/x0ne-sbx-pwned.txt 2>&1; echo OK > /work/inside.txt && echo wrote-inside' | grep -oE 'wrote-inside|cannot create[^\\]{0,70}' | sort | uniq -c
EOF

# Control: the probe must see a key planted in an argv.
(exec -a "$(cat "$T/pat")" sleep 2) & ctl=$!
sleep 0.5
probe_ok=0; grep -lsF -f "$T/pat" /proc/[0-9]*/cmdline >/dev/null 2>&1 && probe_ok=1
kill "$ctl" 2>/dev/null; wait "$ctl" 2>/dev/null

X0NE_WORK_DIR="$T/work" X0NE_SANDBOX_ENV="$T/env" X0NE_SBX_NAME="x0ne-sbx-test-$backend" \
X0NE_SBX_TEST_CMD="$inner" "$here/run-sandboxed.sh" "$backend" >"$T/out" 2>&1 &
pid=$!
leak=0
for _ in $(seq 1 40); do
    for f in $(grep -lsF -f "$T/pat" /proc/[0-9]*/cmdline 2>/dev/null); do
        leak=1; tr '\0' ' ' <"$f" | cut -c1-120 >>"$T/leaks"; done
    sleep 0.25
done
wait "$pid"; rc=$?
cat "$T/out"
fail=0
check() { if eval "$2"; then echo "ok    $1"; else echo "FAIL  $1"; fail=1; fi; }
check "control: leak probe detects an argv key" '[ $probe_ok = 1 ]'
check "script exit 0"                          '[ $rc = 0 ]'
[ $leak = 0 ] || sort -u "$T/leaks"
check "key never on a host command line"       '[ $leak = 0 ]'
check "key reached the sandbox env"            'grep -q "key in env: present" "$T/out"'
check "no canary read from inside"             '! grep -q CANARY "$T/out"'
check "no write on the host outside work dir"  '[ ! -e "$T/pwned.txt" ] && [ ! -e "$HOME/x0ne-sbx-pwned.txt" ]'
check "control: write inside work dir landed"  '[ "$(cat "$T/work/inside.txt" 2>/dev/null)" = OK ]'
check "container gone"                         '! { incus list -f csv -c n 2>/dev/null; podman ps -a --format "{{.Names}}"; } | grep -qx "x0ne-sbx-test-$backend"'
exit $fail
