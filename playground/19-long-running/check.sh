#!/usr/bin/env bash
# Grades 19. Exit 0 means the server ran, was reached, and was stopped cleanly.
set -uo pipefail
cd "$(dirname "$0")"
fail=0
if [ ! -f started.txt ]; then
  echo "✗ started.txt does not exist — the server never started."
  fail=1
fi
if [ ! -f stopped.txt ]; then
  echo "✗ stopped.txt does not exist — the server was not stopped with SIGTERM"
  echo "  (or is still running)."
  fail=1
fi
if command -v ss >/dev/null 2>&1; then
  if ss -ltn 2>/dev/null | grep -q ':8765 '; then
    echo "✗ something is still listening on 8765."
    fail=1
  fi
fi
if [ "$fail" -eq 0 ]; then
  echo "✓ started, reached, stopped cleanly."
fi
exit "$fail"
