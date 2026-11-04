#!/usr/bin/env bash
# Grades 05. Exit 0 means the three failing tests were named correctly.
set -uo pipefail
cd "$(dirname "$0")"

expected=(
  test_retry_backoff_overflows_on_the_ninth_attempt
  test_cache_eviction_frees_the_wrong_entry
  test_deadline_counts_time_the_user_spent_thinking
)

# ── The exercise's own premise, re-measured ──────────────────────────────
# If a line length ever drifts, the failures could slide into the kept head
# or tail and the exercise would quietly become trivial. Checking it here
# means the exercise cannot rot into a false pass.
total=$(./suite.sh | wc -c)
cap=24000
if [ "$total" -le "$cap" ]; then
  echo "BROKEN EXERCISE: suite.sh emits $total bytes, under the $cap cap." >&2
  exit 2
fi
head_end=$((cap * 3 / 5))
tail_start=$((total - (cap - head_end)))
hidden=$(./suite.sh | awk -v h="$head_end" -v t="$tail_start" \
  '{n+=length($0)+1} /^  FAIL/{if (n>h && n<t) c++} END{print c+0}')
if [ "$hidden" -ne 3 ]; then
  echo "BROKEN EXERCISE: $hidden of 3 failures land in the discarded window." >&2
  exit 2
fi

# ── The answer ──────────────────────────────────────────────────────────
if [ ! -f answer.txt ]; then
  echo "✗ answer.txt does not exist. Write the three failing test names into it."
  exit 1
fi

# Compare as sets: order is not part of the question, and blank lines and
# stray whitespace are not mistakes worth failing someone over.
got=$(grep -o 'test_[a-z_]*' answer.txt 2>/dev/null | sort -u)
want=$(printf '%s\n' "${expected[@]}" | sort -u)

if [ "$got" = "$want" ]; then
  echo "✓ all three failures named correctly."
  exit 0
fi

echo "✗ not the right set."
missing=$(comm -23 <(echo "$want") <(echo "$got"))
extra=$(comm -13 <(echo "$want") <(echo "$got"))
[ -n "$missing" ] && echo "  missing:" && echo "$missing" | sed 's/^/    /'
[ -n "$extra" ] && echo "  not a failing test:" && echo "$extra" | sed 's/^/    /'
exit 1
