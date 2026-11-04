# Policy and safety rails

> **X0NE is a safety harness: typed actions, a policy on every call, snapshots to undo, a journal of the agent's actions.**
>
> Before each file write, the agent loop snapshots the files it targets. In the container sandbox it also snapshots the workspace before a command that may delete files or that runs project code, and refuses a deleting command when that snapshot fails. On the host, a command that may delete files gets the same workspace snapshot before the confirmation prompt; when none can be taken (a target outside the workspace, a secret under `deny_path_patterns` at its top, more than `[snapshots] max_snapshot_size_mb` to copy, 100 MB by default), the prompt says there is no undo, offers no standing approval, and a run with nobody to answer refuses it. Other project code on the host runs without an undo point, behind the gate, and is reported as unprotected. `/undo` restores the last point, file by file.

## Policy

```toml
[policy]
default_effect = "deny"          # deny | allow
allowed_read_paths  = ["/home/user/code"]
allowed_write_paths = ["/home/user/code"]
allowed_commands    = ["git", "cargo", "npm", "make"]
deny_path_patterns  = ["**/.ssh/**", "**/*.key", "**/*.pem"]
readable_env_vars   = ["PATH", "HOME", "USER"]

[actions]
WriteFile  = false
RunCommand = false
# etc, per-action switches
```

Where the runtime reads it: `--config <path>`, else `~/.config/x0ne/x0ne-runtime.toml`.
Without a policy file the runtime does not start: it says which file to write and where
(`x0ne --setup` writes it). An empty, unreadable or invalid file is refused the same way.
A `./x0ne-runtime.toml` in the working directory is **ignored unless**
`X0NE_RUNTIME_ALLOW_CWD_CONFIG=1` is set — a policy file in a checkout is the
checkout's policy, not yours. A trusted project can instead name its policy as
`[bridge] runtime_config = "x0ne-runtime.toml"` in its `x0ne-orchestrator.toml`.
Keys you leave out of `[policy]` take the fail-closed defaults (`deny`, empty
allowlists), so a three-line `[policy]` is a valid file — with one rule: a table
that names `allowed_*` entries must state `default_effect` itself (a grant without
it is the shape of a truncated file, and is refused by name). See [mcp.md](mcp.md).

### Trust roots, writing without the prompt

The approval prompt (`[y]es [n]o [a]lways`) is the orchestrator's, and no
config flag switches it off: `[ui] auto_confirm` is **not honoured** by the
gate. To let the agent write without asking, name the directories in your
**user** config (`~/.config/x0ne-orchestrator/config.toml` or
`~/.x0ne/config.toml`; a project-local file cannot set this):

```toml
[trust]
roots = ["~/labs/playground"]     # relative paths resolve against the cwd
```

Inside a root: reads, writes, `MoveToTrash` and commands whose `cwd` is under
it run without the prompt. Never dispensed, whatever the path: network, desktop,
host and Incus operations, snapshot restore/delete, and any composite with one
child outside. The runtime's `allowed_write_paths` / `allowed_commands` still
apply on top, and sub-agents do not inherit the roots. The banner names the
roots while they are active.

Commands are dispensed only for a built-in list of programs (cargo, make,
python3 without `-c` or `-m`, pytest without `-p`, `--pyargs`, `-c`, `-o`, `-W`,
`--pdbcls`, `--basetemp` or `@file`, git without remotes, read-only tools…). To add others,
name them; every path argument, including a destination glued to its flag
(`-o/path`, `--output=/path`) and each word of an argument, must still resolve
inside a root. Shells, launchers (`env`, `xargs`, `sudo`…), inline-code
interpreters and network tools are refused even if listed.

```toml
[trust]
roots = ["/app"]
workdir_programs = ["gcc", "cc", "chmod", "mkdir", "sqlite3"]
```

## Safety Rails

The orchestrator prevents LLMs from getting stuck in infinite retry loops:

| Protection | Threshold | Behavior |
|---|---|---|
| **Consecutive action limit** | 100 actions (`cost.max_consecutive_actions`; 0 = never) | Forces pause, asks LLM to summarize and wait for user |
| **Total failure budget** | 16 failures per turn | Hard stop regardless of action type |
| **Repeated-action circuit breaker** | 5th identical failure (`type` + `args` + `error_code`) | Pre-dispatch refusal with `loop_detected` envelope (CLI + TUI) |
| **Parse retry limit** | 5 invalid JSON blocks | Returns to user prompt |
| **Structural hints** | On "missing field" errors | Detects `"params"` wrapper mistakes, injects correction |
| **Schema hints** | On any parse error | Injects correct schema + example from discovered actions |
| **Confirmation gate** | Destructive actions | `[y]es [n]o [a]lways` prompt before writes/commands |

## Session Audit Logging

Every `x0ne` agent session is logged to `~/.x0ne/sessions/*.jsonl`, one JSONL file per invocation (MCP calls are not). Each line is a self-contained JSON event:

| Event Type | Description |
|------------|-------------|
| `session_start` | Model, version, hostname, PID, correlates audit trails; `cwd` and `git_root`, the project the session belongs to |
| `user_input` | User prompt text |
| `llm_response` | Full LLM response with token counts (in/out) |
| `action` | Action JSON with type, success flag, duration |
| `action_result` | Full result payload (output, errors) |
| `policy_denial` | Action blocked by policy engine (with reason) |
| `session_renamed` | `/rename <title>`: appended, so `x0ne audit verify` still holds; the latest title names the session |

`x0ne sessions` and `/sessions` list the current project's sessions: the git
work tree they started in (found by its `.git`, no `git` process), else
their directory. `--all` lists every session; journals written before the
header recorded a directory appear there only. `/rename <title>` names the
current session: the listing shows the title, read back as untrusted text
(control and bidi characters folded) like every journal field.

```bash
# View latest session log
cat ~/.x0ne/sessions/$(ls -t ~/.x0ne/sessions/ | head -1) | jq .

# Export conversation as JSON (for LLM fine-tuning)
/export my-training-data    # → ~/.x0ne/exports/my-training-data_2026-04-16_02-30.json
```

The `/export` format produces clean `{"role": "user/assistant", "content": "..."}` pairs, directly compatible with LLM fine-tuning pipelines.

## Security Hardening

| Protection | Description |
|------------|-------------|
| **Path canonicalization** | All file paths resolved via `std::fs::canonicalize()`, blocks `../` traversal and symlink escapes |
| **UTF-8 safe truncation** | Character-based slicing prevents panics on multi-byte sequences |
| **I/O deadlock prevention** | Concurrent `tokio::join!` for stdout/stderr pipes instead of sequential reads |
| **Policy denial audit** | In the `x0ne` agent loop, every action the policy refuses is logged with its reason (`policy_denial`) to the session JSONL |
| **Sandbox file integrity** | Rejects absolute paths and `..` components in sandbox temp directories |
| **Podman by fixed path** | `RunInSandbox`, `RunContainerGroup`, the container probe and `x0ne setup podman` run `/usr/bin/podman`, `/usr/local/bin/podman` or `/bin/podman`, never a `podman` found through `PATH` |
