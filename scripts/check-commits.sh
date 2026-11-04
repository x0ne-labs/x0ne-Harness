#!/usr/bin/env bash
# Check commit identity and messages.
#   scripts/check-commits.sh [<rev-range>...]   default: --branches
#   scripts/check-commits.sh --msg <file>       one message (commit-msg hook)
set -euo pipefail

EXPECT="x0ne <dev@x0ne.co>"
FORBIDDEN='claude|co-authored-by|claude-session|chatgpt|copilot|assistant|generated (with|by)|llm-generated'
AI_WORD='\bAI\b'   # case-sensitive: crate names like asm-ai stay allowed
NOISE='^(wip|fixup!|squash!|tmp|temp)\b|fix(ed)? typo'

check_msg() { # $1 label, message on stdin; prints problems
  local label=$1 msg subject
  msg=$(cat)
  subject=$(printf '%s\n' "$msg" | head -n1)
  local bad=0
  if [ "${#subject}" -gt 72 ]; then echo "$label: subject longer than 72 chars"; bad=1; fi
  if [ -z "$subject" ]; then echo "$label: empty subject"; bad=1; fi
  if printf '%s\n' "$msg" | grep -qiE "$FORBIDDEN"; then echo "$label: forbidden mention: $(printf '%s\n' "$msg" | grep -oiE "$FORBIDDEN" | head -n1)"; bad=1; fi
  if printf '%s\n' "$msg" | grep -qE "$AI_WORD"; then echo "$label: forbidden mention: AI"; bad=1; fi
  if printf '%s\n' "$subject" | grep -qiE "$NOISE"; then echo "$label: noise subject"; bad=1; fi
  if printf '%s\n' "$msg" | grep -qE '[[:space:]]$'; then echo "$label: trailing whitespace"; bad=1; fi
  return $bad
}

if [ "${1:-}" = "--msg" ]; then
  # Drop the comment lines git adds to the editor buffer.
  grep -v '^#' "$2" | check_msg "message" && exit 0 || exit 1
fi

[ $# -eq 0 ] && set -- --branches
fails=0; n=0
while read -r c; do
  n=$((n + 1))
  a=$(git log -1 --format='%an <%ae>' "$c"); m=$(git log -1 --format='%cn <%ce>' "$c")
  [ "$a" = "$EXPECT" ] || { echo "$c: author is $a"; fails=$((fails + 1)); }
  [ "$m" = "$EXPECT" ] || { echo "$c: committer is $m"; fails=$((fails + 1)); }
  [ "$(git rev-list --parents -n1 "$c" | wc -w)" -le 2 ] || { echo "$c: merge commit"; fails=$((fails + 1)); }
  git log -1 --format='%B' "$c" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' | check_msg "$c" || fails=$((fails + 1))
done < <(git rev-list "$@")
echo "checked $n commits, $fails violation(s)"
[ "$fails" -eq 0 ]
