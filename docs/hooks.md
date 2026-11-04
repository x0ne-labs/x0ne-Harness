# Hooks

x0ne runs the commands you list under `[hooks]` in your user configuration
(`~/.config/x0ne-orchestrator/config.toml`, or `~/.x0ne/config.toml`) on four
events. The event names and the JSON fields are those of Claude Code
([hooks reference](https://code.claude.com/docs/en/hooks)), so a hook written
for Claude Code reads the same payload. This is not `x0ne hook pretooluse`,
which is the other direction: x0ne as a hook of Claude Code or Codex
([agent-hooks.md](agent-hooks.md)).

| Table | Event | When | Can it refuse? |
|---|---|---|---|
| `[[hooks.pre_tool]]` | `PreToolUse` | before every action, connector calls included | yes |
| `[[hooks.post_tool]]` | `PostToolUse` | after an action ran and succeeded | no |
| `[[hooks.session_start]]` | `SessionStart` | the REPL, `-p` or the TUI starts a session; `/clear`; `/resume` | no |
| `[[hooks.stop]]` | `Stop` | a turn ends, except on a user cancel or a model failure | no |

```toml
[[hooks.pre_tool]]
matcher = "github.*|RunCommand"   # `|`-separated, `*` = any run of characters
command = "/usr/local/bin/guard"
args = ["--strict"]
timeout_secs = 10                 # default 10

[[hooks.post_tool]]
matcher = "WriteFile"
command = "/usr/local/bin/audit-write"

[[hooks.session_start]]
matcher = "startup|resume"        # matches the source
command = "/usr/local/bin/x0ne-started"

[[hooks.stop]]                    # no matcher
command = "/usr/local/bin/notify-done"
```

## Rules

- Hooks come from the user configuration only. A project
  `x0ne-orchestrator.toml` cannot add, drop or change one, even in a directory
  listed under `[project_trust] privileged`.
- Exec form: `command` is spawned with `args`, without a shell. Hooks of one
  event run one after another, in order.
- `pre_tool` can only refuse: exit 2 (stderr is the reason), or exit 0 with
  `{"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "…"}}`.
  It cannot approve, rewrite the input or waive another gate. A hook that does
  not start, exits with another code, prints invalid JSON or times out refuses.
- `post_tool`, `session_start` and `stop` observe. x0ne never reads what they
  print, nothing they print reaches the model, and exit 2 or a deny changes
  nothing. A failure (an exit code other than 0, a timeout, a missing command)
  is logged as a warning and the session goes on. Each one delays the session
  by at most its timeout.

## Payload

Every hook reads one JSON object on stdin with:

| Field | Value |
|---|---|
| `hook_event_name` | `PreToolUse`, `PostToolUse`, `SessionStart` or `Stop` |
| `session_id` | the session log's id, the one `--resume` takes; `null` when the session keeps no log |
| `cwd` | x0ne's working directory |

and, per event:

- `PreToolUse`: `tool_name` (the action type, or `<connector>.<tool>`),
  `tool_input` (the action), and for a connector tool `connector`
  (`name`, `tool`, `arguments`).
- `PostToolUse`: the same, plus `tool_response` (the action's result
  envelope) and `duration_ms`.
- `SessionStart`: `source` (`startup`, `resume` or `clear`) and `model`.
- `Stop`: `stop_hook_active`, always `false` because x0ne never continues a
  turn from a hook, and `last_assistant_message`, the turn's answer or `null`
  when the turn ended without one.

Not sent: `transcript_path`, `permission_mode`, `tool_use_id`.
