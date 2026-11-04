# Egress of run-sandboxed.sh, sourced by it. Default: the sandbox has no
# route out; its only door is a tinyproxy that opens onto an allowlist of hosts
# (scripts/sandbox-egress/allow.default + the LLM provider). Same design as
# nuit-de-test's nuit-proxy (34/34 attacks held, 2026-10).
#
# Knobs:
#   X0NE_SANDBOX_NET=proxy|open   proxy (default) or unrestricted NAT egress.
#   X0NE_SANDBOX_ALLOW="h1 h2"    extra hosts ("*.example.org" = subdomains too).
#   X0NE_SANDBOX_ALLOW_FILE=path  replaces allow.default.
# Limit: a CONNECT proxy sees host names, not methods. github.com is reachable
# for reads, but an HTTPS write with a token the agent holds is not blocked.

EGRESS_DIR=${EGRESS_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/sandbox-egress}
EGRESS_IMAGE=${X0NE_SBX_EGRESS_IMAGE:-localhost/x0ne-sbx-egress:trixie}
EGRESS_PORT=8888

egress_mode() {
    case "${X0NE_SANDBOX_NET:-proxy}" in
        ""|proxy) echo proxy ;;
        open) echo open ;;
        *) echo "run-sandboxed: X0NE_SANDBOX_NET must be proxy or open" >&2; return 1 ;;
    esac
}

# Host part of a URL, or nothing.
egress_url_host() {
    local u=${1#*://}
    u=${u%%/*}; u=${u##*@}; u=${u%%:*}
    printf '%s\n' "$u"
}

# The configured LLM provider's host(s). Reads key NAMES from the env file,
# never values.
egress_provider_hosts() {
    local env_file=$1 p=${X0NE_LLM_PROVIDER:-}
    case "$p" in
        anthropic) echo api.anthropic.com ;;
        openai) echo api.openai.com ;;
        openrouter) echo openrouter.ai ;;
        groq) echo api.groq.com ;;
        openai-compat|openai_compat)
            [ -n "${X0NE_OPENAI_COMPAT_BASE_URL:-}" ] && egress_url_host "$X0NE_OPENAI_COMPAT_BASE_URL" ;;
        ollama) [ -n "${OLLAMA_BASE_URL:-}" ] && egress_url_host "$OLLAMA_BASE_URL" ;;
        "")
            local names
            names=$(grep -oE '^[[:space:]]*(export[[:space:]]+)?[A-Z_]+_API_KEY=' "$env_file" 2>/dev/null | grep -oE '[A-Z_]+_API_KEY' || true)
            case "$names" in *ANTHROPIC_API_KEY*) echo api.anthropic.com ;; esac
            case "$names" in *OPENAI_API_KEY*) echo api.openai.com ;; esac
            case "$names" in *OPENROUTER_API_KEY*) echo openrouter.ai ;; esac
            case "$names" in *GROQ_API_KEY*) echo api.groq.com ;; esac
            ;;
        *) echo "run-sandboxed: unknown X0NE_LLM_PROVIDER=$p; add its host to X0NE_SANDBOX_ALLOW" >&2 ;;
    esac
    return 0
}

# Every allowed host, validated, one per line.
egress_hosts() {
    local env_file=$1 list=${X0NE_SANDBOX_ALLOW_FILE:-$EGRESS_DIR/allow.default} h
    [ -r "$list" ] || { echo "run-sandboxed: cannot read allowlist $list" >&2; return 1; }
    {
        sed -e 's/#.*//' -e 's/[[:space:]]//g' "$list"
        for h in ${X0NE_SANDBOX_ALLOW:-}; do echo "$h"; done
        egress_provider_hosts "$env_file"
    } | tr 'A-Z' 'a-z' | grep -v '^$' | while read -r h; do
        if [[ "$h" =~ ^(\*\.)?[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$ ]]; then
            echo "$h"
        else
            echo "run-sandboxed: ignoring invalid allowlist entry '$h'" >&2
        fi
    done | sort -u
}

# tinyproxy filter: one anchored ERE per host.
egress_filter() {
    local h re
    while read -r h; do
        re=${h#\*.}; re=${re//./\\.}
        if [ "${h:0:2}" = "*." ]; then echo "^([a-z0-9-]+\\.)*$re\$"; else echo "^$re\$"; fi
    done
}

egress_warn_open() {
    cat >&2 <<'W'

  ┌─────────────────────────────────────────────────────────────────────────┐
  │ WARNING  X0NE_SANDBOX_NET=open: the sandbox has UNRESTRICTED Internet    │
  │ egress. Anything it can read, /work/.env included, can be sent anywhere. │
  └─────────────────────────────────────────────────────────────────────────┘

W
}

egress_image() {
    podman image exists "$EGRESS_IMAGE" 2>/dev/null && return 0
    echo "run-sandboxed: building the egress proxy image $EGRESS_IMAGE" >&2
    podman build -q -t "$EGRESS_IMAGE" -f "$EGRESS_DIR/Containerfile" "$EGRESS_DIR" >/dev/null
}

# Start "<name>-proxy". $1 name, $2 env file, $3 "internal" (podman: the sandbox
# joins <name>-int) or "loopback" (incus: published on 127.0.0.1, port printed).
egress_up() {
    local name=$1 env_file=$2 how=$3 dir
    egress_image || return 1
    dir=$(mktemp -d "${TMPDIR:-/tmp}/x0ne-egress.XXXXXX")
    EGRESS_TMP=$dir
    egress_hosts "$env_file" >"$dir/hosts" || return 1
    egress_filter <"$dir/hosts" >"$dir/allow.filter"
    chmod 644 "$dir/allow.filter"; chmod 755 "$dir"
    echo "run-sandboxed: egress through a proxy, allowed: $(tr '\n' ' ' <"$dir/hosts")" >&2
    podman network create "$name-ext" >/dev/null
    local net=(--network "$name-ext") pub=()
    if [ "$how" = internal ]; then
        podman network create --internal "$name-int" >/dev/null
        net=(--network "$name-int:alias=x0ne-egress" --network "$name-ext")
    else
        pub=(-p "127.0.0.1::$EGRESS_PORT")
    fi
    podman run -d --name "$name-proxy" "${net[@]}" "${pub[@]}" \
        --cap-drop=ALL --security-opt no-new-privileges --read-only \
        --tmpfs /run:rw,size=1m --tmpfs /var/log/tinyproxy:rw,size=8m \
        --memory 128m --pids-limit 64 \
        -v "$dir/allow.filter:/etc/tinyproxy/allow.filter:ro" \
        "$EGRESS_IMAGE" >/dev/null
    local _
    for _ in $(seq 1 50); do
        podman logs "$name-proxy" 2>&1 | grep -qi 'accepting connections\|listening on' && return 0
        sleep 0.1
    done
    return 0
}

egress_host_port() {
    podman port "$1-proxy" "$EGRESS_PORT/tcp" | head -1 | sed 's/.*://'
}

egress_down() {
    local name=$1
    podman rm -f -t 2 "$name-proxy" >/dev/null 2>&1 || true
    podman network rm "$name-int" >/dev/null 2>&1 || true
    podman network rm "$name-ext" >/dev/null 2>&1 || true
    [ -n "${EGRESS_TMP:-}" ] && rm -rf "$EGRESS_TMP"
    return 0
}

# Proxy variables for a client that reaches the proxy at $1 (host:port).
egress_env() {
    local u="http://$1" v
    for v in HTTP_PROXY HTTPS_PROXY http_proxy https_proxy ALL_PROXY all_proxy; do
        echo "$v=$u"
    done
    echo "NO_PROXY=localhost,127.0.0.1,::1"
    echo "no_proxy=localhost,127.0.0.1,::1"
    echo "X0NE_SANDBOX_NET=proxy"
}
