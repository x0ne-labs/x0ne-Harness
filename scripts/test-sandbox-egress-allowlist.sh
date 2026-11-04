#!/usr/bin/env bash
# Allowlist rendering of scripts/sandbox-egress.sh. No container.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
. "$here/sandbox-egress.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
printf 'ANTHROPIC_API_KEY=x\n' >"$T/env"; : >"$T/none"
fail=0
check() { if eval "$2"; then echo "ok    $1"; else echo "FAIL  $1"; fail=1; fi; }
hosts() { egress_hosts "$@" 2>/dev/null; }
check "default list has the registries"       'hosts "$T/none" | grep -qx static.crates.io && hosts "$T/none" | grep -qx files.pythonhosted.org'
check "provider from the key name"            'hosts "$T/env" | grep -qx api.anthropic.com'
check "no key, no provider host"              '! hosts "$T/none" | grep -q api.anthropic.com'
check "X0NE_LLM_PROVIDER wins"                'X0NE_LLM_PROVIDER=openrouter hosts "$T/env" | grep -qx openrouter.ai'
check "openai-compat base URL host"           'X0NE_LLM_PROVIDER=openai-compat X0NE_OPENAI_COMPAT_BASE_URL=https://llm.corp.example:8443/v1 hosts "$T/none" | grep -qx llm.corp.example'
check "X0NE_SANDBOX_ALLOW adds"               'X0NE_SANDBOX_ALLOW="registry.npmjs.org *.example.org" hosts "$T/none" | grep -qx registry.npmjs.org'
printf 'only.example.net\n' >"$T/list"
check "X0NE_SANDBOX_ALLOW_FILE replaces"      '[ "$(X0NE_SANDBOX_ALLOW_FILE=$T/list hosts "$T/none")" = only.example.net ]'
check "invalid entries dropped"               '[ "$(X0NE_SANDBOX_ALLOW_FILE=$T/none X0NE_SANDBOX_ALLOW="a..b .* x/y ok.example" hosts "$T/none")" = ok.example ]'
check "filter is anchored"                    '[ "$(echo github.com | egress_filter)" = "^github\\.com\$" ]'
re=$(echo "*.example.org" | egress_filter)
check "wildcard matches subdomains only"      'echo a.b.example.org | grep -Eq "$re" && echo example.org | grep -Eq "$re" && ! echo evilexample.org | grep -Eq "$re"'
check "anchored filter refuses suffix trick"  '! echo github.com.evil.net | grep -Eq "$(echo github.com | egress_filter)"'
check "unknown X0NE_SANDBOX_NET refused"      '! X0NE_SANDBOX_NET=maybe egress_mode 2>/dev/null'
exit $fail
