#!/usr/bin/env bash
# Grades 15. Exit 0 means both files exist with the right content.
set -uo pipefail
cd "$(dirname "$0")"
. ../_lib/journal.sh
fail=0

for pair in "a.txt:alpha" "b.txt:beta"; do
  f="out/${pair%%:*}"; want="${pair##*:}"
  if [ ! -f "$f" ]; then
    echo "✗ $f does not exist."
    fail=1
  elif [ "$(tr -d '[:space:]' < "$f")" != "$want" ]; then
    echo "✗ $f says '$(head -c 40 "$f")', expected '$want'."
    fail=1
  fi
done

if journal="$(pg_journal)"; then
  require_wired_journal "$journal"
  verdict=$(journal_query "$journal" '
denied_pipeline_at = None
single_after = False
for i, e in enumerate(events):
    t = e.get("type")
    a = e.get("action") or {}
    if t == "policy_denial" and a.get("type") == "Pipeline" and denied_pipeline_at is None:
        denied_pipeline_at = i
    if denied_pipeline_at is not None and i > denied_pipeline_at and t == "action" and a.get("type") == "WriteFile":
        single_after = True
if denied_pipeline_at is not None and not single_after:
    print("gave-up")
elif denied_pipeline_at is not None:
    print("retried-alone")
else:
    print("no-pipeline")
')
  case "$verdict" in
    gave-up)
      echo "✗ a Pipeline was refused and no single WriteFile was tried afterwards."
      echo "  The refusal said it: a composite is not offered a grant, the step alone is."
      fail=1 ;;
    retried-alone) echo "  (a refused Pipeline was retried step by step — the intended path.)" ;;
  esac
fi

if [ "$fail" -eq 0 ]; then
  echo "✓ out/a.txt and out/b.txt are in place."
fi
exit "$fail"
