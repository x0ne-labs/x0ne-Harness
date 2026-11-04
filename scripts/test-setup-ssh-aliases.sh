#!/usr/bin/env bash
# Checks `make setup-ssh-aliases` against `ssh -G`. DESTRUCTIVE for ~/.ssh:
# run it only in a throwaway container (make test-ssh-aliases does).
set -euo pipefail
[ -f /run/.containerenv ] || [ -f /.dockerenv ] || { echo "refusing: not in a container" >&2; exit 2; }
cd "$(dirname "$0")/.."
fail=0
check() { if eval "$2"; then echo "ok    $1"; else echo "FAIL  $1"; fail=1; fi; }
cm() { ssh -G "$1" 2>/dev/null | awk '$1=="controlmaster"{print $2}'; }

rm -rf ~/.ssh && mkdir -p ~/.ssh && chmod 700 ~/.ssh
printf 'Host prod\n    HostName 192.0.2.10\n    User deploy\n' > ~/.ssh/config
chmod 600 ~/.ssh/config
make -s setup-ssh-aliases >/dev/null
make -s setup-ssh-aliases >/dev/null

check "x0ne-* hosts get multiplexing"          '[ "$(cm x0ne-box)" = auto ]'
check "other hosts are untouched (prod)"       '[ "$(cm prod)" = false ]'
check "other hosts are untouched (github.com)" '[ "$(cm github.com)" = false ]'
check "prod keeps its own settings"            'ssh -G prod | grep -qx "user deploy"'
check "Include precedes the first Host block"  '[ "$(grep -nE "^[[:space:]]*(Include|Host|Match)" ~/.ssh/config | head -1 | grep -c Include)" = 1 ]'
check "Include written once after two runs"    '[ "$(grep -cE "^Include config.d/" ~/.ssh/config)" = 1 ]'
check "~/.ssh/config stays 0600"               '[ "$(stat -c %a ~/.ssh/config)" = 600 ]'
exit $fail
