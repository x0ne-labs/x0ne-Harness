#!/usr/bin/env bash
# Refuses a branch name that does not follow docs/BRANCHING.md.
# check-branch-name.sh <name> | (no argument: current branch) | as pre-push hook.
set -euo pipefail

PATTERN='^(feat|fix|docs|chore|security)/([0-9]+-)?[a-z0-9]+(-[a-z0-9]+)*$'
MAX=50

check() {
  local name=$1
  case $name in main|dependabot/*) return 0 ;; esac
  if (( ${#name} <= MAX )) && [[ $name =~ $PATTERN ]]; then return 0; fi
  echo "refused branch name: $name" >&2
  echo "expected <feat|fix|docs|chore|security>/<optional issue>-<words>, at most $MAX chars, e.g. fix/123-short-desc" >&2
  return 1
}

if (( $# == 2 )); then              # git pre-push: <remote> <url>, refs on stdin
  rc=0
  while read -r _ _ remote_ref _; do
    [[ $remote_ref == refs/heads/* ]] && { check "${remote_ref#refs/heads/}" || rc=1; }
  done
  exit $rc
fi
check "${1:-$(git rev-parse --abbrev-ref HEAD)}"
