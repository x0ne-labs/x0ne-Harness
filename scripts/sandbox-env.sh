#!/usr/bin/env bash
# Push the X0NE_FORWARD_ENV variables into an Incus sandbox through stdin, so
# no value appears on any argv (`ps`, `make -n`). Usage: sandbox-env.sh <remote:container>
set -euo pipefail
target=${1:?usage: sandbox-env.sh <remote:container>}
{
    for name in ${X0NE_FORWARD_ENV:-}; do
        [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || continue
        [ -n "${!name:-}" ] && printf 'export %s=%q\n' "$name" "${!name}"
    done
    true
} | incus file push --mode 0600 - "$target/root/.x0ne-forward.env"
printf '%s\n' '#!/bin/bash' 'set -a; . /root/.x0ne-forward.env; set +a' 'exec "$@"' \
    | incus file push --mode 0700 - "$target/root/x0ne-runtime-env.sh"
