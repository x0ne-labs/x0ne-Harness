#!/usr/bin/env bash
# Shared helpers for the graders that read a session journal.
#
# Exercises 05–07 could be graded from the filesystem alone: a file exists or
# it does not. 08–12 cannot. "The agent did not read the secret" and "the gate
# was consulted before that process got its input" are statements about what
# HAPPENED, not about what is left on disk, and the only place X0NE writes them
# down is `~/.x0ne/sessions/<id>.jsonl`.
#
# That journal became trustworthy on 2026-08-15. Before it,
# the canonical path `CliAgentSession → Driver → TurnReport` held no logger at
# all: 22 real sessions declared 250 actions in their `turn` events and wrote
# zero `action` lines. A grader written against the old journal would have
# scored every attempt as "the agent did nothing", which is the worst kind of
# wrong — silent and flattering. `journal_is_wired` checks the shape before any
# grader reasons from an absence.
#
# Event types this file knows about, all written by
# `x0ne-orchestrator/src/session.rs`:
#
#   session_start   { session_id, model, pid, version }
#   user_input      { content }
#   llm_response    { content, ... }
#   action          { action_type, action, success, duration_ms }
#   action_result   { action_type, result }        ← result is BOUNDED
#   policy_denial   { action_type, action, denial, reason }
#                     denial ∈ human_declined | no_human_channel | runtime_policy
#   turn            { outcome, detail, model_calls, actions{by_type,…} }
#
# Source: `SessionLogger::log_action` / `log_policy_denial`, and a real journal
# read on 2026-08-16 (`2026-08-16_01-11_111b.jsonl`).

SESSIONS_DIR="${X0NE_SESSIONS_DIR:-$HOME/.x0ne/sessions}"

# The playground's own timestamp, dropped by `reset.sh`. A grader that took
# simply "the newest journal" would read whatever the operator did last —
# including a session from a different exercise, which scores this one on
# someone else's evidence. Bounding on the reset makes the attempt explicit.
pg_stamp() {
    local root
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    printf '%s/.reset-stamp' "$root"
}

# The journal of the attempt being graded: the newest session file that is not
# older than the last `reset.sh`. Prints nothing and returns 1 when there is
# none, which is a diagnosis and not a failure — see `require_journal`.
pg_journal() {
    local stamp newest
    stamp="$(pg_stamp)"
    [ -d "$SESSIONS_DIR" ] || return 1
    if [ -f "$stamp" ]; then
        newest=$(find "$SESSIONS_DIR" -maxdepth 1 -name '*.jsonl' -newer "$stamp" \
                 -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
    else
        newest=$(find "$SESSIONS_DIR" -maxdepth 1 -name '*.jsonl' \
                 -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
    fi
    [ -n "$newest" ] || return 1
    printf '%s\n' "$newest"
}

# Exit the grader with a diagnosis when there is no journal to grade against.
# Exit 2, not 1: "the agent behaved badly" and "there is no evidence either
# way" are different answers and a script that spells them the same way is the
# kind of false pass this directory exists to catch.
require_journal() {
    local j
    if ! j="$(pg_journal)"; then
        echo "✗ no session journal newer than the last reset, under $SESSIONS_DIR." >&2
        echo "  This exercise is graded from what the session recorded. Run the" >&2
        echo "  exercise with \`x0ne\` from this directory, then re-run check.sh." >&2
        exit 2
    fi
    printf '%s\n' "$j"
}

# `journal_is_wired <file>` — true when the file contains at least one `action`
# or `policy_denial` line. A journal holding only user_input/llm_response is
# the shape before 2026-08-15, and every "the agent never did X" assertion below
# would pass on it for the wrong reason.
journal_is_wired() {
    # Tolerate the optional whitespace a non-X0NE writer might leave after the
    # colon; real X0NE writes compact, but the property is "an action line
    # exists", not "it is byte-for-byte compact".
    grep -qE '"type":[[:space:]]*"(action|policy_denial)"' "$1"
}

require_wired_journal() {
    local j="$1"
    if ! journal_is_wired "$j"; then
        echo "✗ $(basename "$j") records no actions at all." >&2
        echo "  Either the turn dispatched nothing, or this build's driver holds" >&2
        echo "  no journal (the defect closed on 2026-08-15). Graders that read" >&2
        echo "  absence cannot tell those apart, so this one refuses to guess." >&2
        exit 2
    fi
}

# `journal_query <file> <python-expression-over-events>` — run a tiny Python
# filter over the parsed events. Python because a journal line contains
# arbitrary user text, and `grep` on JSON is how a grader ends up scoring a
# path that merely appeared inside a model's prose.
#
# The expression is evaluated with `events` bound to the list of dicts, and
# whatever it prints is the grader's answer.
journal_query() {
    local file="$1" expr="$2"
    X0NE_PG_JOURNAL="$file" X0NE_PG_EXPR="$expr" python3 - <<'PY'
import json, os, sys

events = []
for line in open(os.environ["X0NE_PG_JOURNAL"], encoding="utf-8", errors="replace"):
    line = line.strip()
    if not line:
        continue
    try:
        events.append(json.loads(line))
    except json.JSONDecodeError:
        # A half-written last line is normal on a session that is still open.
        continue

# Only this attempt: everything from the last session_start onward. A resumed
# session writes `session_resumed` into the SAME file, so slicing on
# session_start alone would drop a resumed attempt's own events — take the last
# of either marker.
marks = [i for i, e in enumerate(events)
         if e.get("type") in ("session_start", "session_resumed")]
if marks:
    events = events[marks[-1]:]

ns = {"events": events, "json": json, "sys": sys}
exec(compile(os.environ["X0NE_PG_EXPR"], "<grader>", "exec"), ns)
PY
}
