# Architecture

Technical deep dive into how X0NE is built and how its components interact.

---

## Overview

Advanced and hardened sandboxing for agentic dev. A workspace of fourteen Rust crates. The three binaries that `make install` installs: `x0ne-runtime` (the safety surface, 101 typed actions), `x0ne-orchestrator` (agent loop and MCP gateway) and `x0ne` (from `x0ne-code`: CLI / TUI / HTTP). `x0ne-vm` (QEMU/KVM) and `x0ne-browser` (isolated Chromium over CDP) stay out of the default build and the gateway does not route to them.

```
x0ne-orchestrator          x0ne-runtime
├── TUI (ratatui)          ├── Action parser (JSON → typed enum)
├── Matrix Mode            ├── Policy engine (deny / allow / confirm)
├── CLI (rustyline)        ├── Executor (101 typed actions)
├── LLM backends           ├── Composite primitives (user-defined)
│   Ollama / Anthropic     ├── Handle manager (async events)
│   OpenAI-compat          ├── Snapshot engine (FS, reflink-aware)
│   (multi-profile, hot-   ├── Try / SafeChange (auto-snapshot rollback)
│   swap via /provider)    ├── MCP server (tools/list, tools/call)
├── Non-interactive (-p)   └── Path canonicalization (anti-traversal)
├── Autonomous (--auto)
├── MCP gateway (22 tools,
│   plus the runtime's
│   70 MCP tools):
│   • x0ne_plan / _execute
│     (single-use ledger)
│   • x0ne_simulate
│     (pre-flight, no writes)
│   • x0ne_goal_set / step /
│     status / pause / resume /
│     complete / clear
│     (persistent ~/.x0ne/goals/)
│   • x0ne_artifact_expose
│     (privilege boundary)
├── Orchestrator-first
│   dispatch (sysaicalls
│   + Pipeline interception)
├── Multi-agent (SpawnAgent
│   / SpawnWorld / WaitAgent /
│    BroadcastAgents)
├── Incus container mgmt
│   (RunIncus, RunIncusGroup,
│   Incus*Snapshot, outside
│   the runtime's privilege
│   boundary by design)
├── X0NE.md (project memory)
├── Codebase indexing
├── @file injection
├── Session audit logger (JSONL)
├── Execution target display
│   (🛡 sandbox / ⚡ local / 🌐 remote)
├── /export (JSON for fine-tuning)
├── Loop protection
│   (anti-loop, hints)
└── Bridge
    (stdio / HTTP / SSH+Incus)

x0ne-mcp (shared lib)        x0ne-code (`x0ne` binary)
├── JSON-RPC 2.0 types       ├── CLI / TUI / Matrix entrypoint
├── line-delimited stdio     ├── -p one-shot, --stdin pipe
└── ChildMcpClient           └── --auto autonomous loop
    (downstream proxy)           consumes orchestrator as lib
```

> **Privilege boundary**, anything that modifies the runtime's environment
> (containers, host-side artifact exposure, sub-agent spawning) lives in
> the orchestrator, not the runtime. The runtime literally cannot compile
> a `RunIncus` action; the orchestrator intercepts these before the bridge
> dispatch and runs `incus` itself on the host. The LLM sees the same
> wire format either way.

### How artifacts are served

When the agent emits `ArtifactCreate`, files (HTML/CSS/JS, optional Python/Node
source) live in the **runtime's** trust domain — never on the host disk if the
runtime is sandboxed. Two HTTP servers carry them to the user's browser:

```
USER BROWSER ─── http://localhost:7800 ────► x0ne-orchestrator (always on host)
                                                    │
                                                    │  reverse-proxy /a/<id>/...
                                                    ▼
host mode                                   sandbox mode
─────────                                   ────────────
runtime on host                             runtime in container
  127.0.0.1:7900                              <bridge-IP>:7900
  serves ~/.x0ne/artifacts/                   serves /root/.x0ne/artifacts/
                                              (files stay in the container,
                                               no host bind mount, no agent
                                               source ever touches host disk)
```

The orchestrator never reads or executes artifact bytes — it only forwards
them as opaque HTTP, and only `GET` and `HEAD` from a browser on this
machine (loopback `Host` and `Origin`, no cross-site embedding; see the
*Policy* section of `x0ne-orchestrator/README.md`). The runtime's server
on 7900 keeps the same guard: its port is in every URL, so a page whose
name resolves to this machine reached it directly. URLs are always
`localhost:7800/a/<id>/`, regardless of
mode. The container's bridge IP is auto-detected from the `--downstream-prefix`
on startup; override with `X0NE_RUNTIME_UPSTREAM=<host>:<port>`.

---

## Workspace structure

```
x0ne/
├── Cargo.toml                  # Cargo workspace root
├── x0ne-runtime/                  # The runtime binary
│   ├── src/
│   │   ├── main.rs             # Entry point, CLI, server setup
│   │   ├── action.rs           # Action enum — all 40+ typed actions
│   │   ├── config.rs           # Config structs + TOML loading
│   │   ├── policy.rs           # Policy engine
│   │   ├── result.rs           # ActionResult + ActionOutput types
│   │   ├── server.rs           # HTTP + stdio server dispatch
│   │   ├── handles.rs          # Handle manager (streaming/events)
│   │   └── executor/
│   │       ├── mod.rs          # Dispatch to sub-executors
│   │       ├── fs.rs           # Filesystem actions
│   │       ├── git.rs          # Git actions
│   │       ├── process.rs      # RunCommand + streaming
│   │       ├── search.rs       # SearchCode, FindFiles
│   │       ├── logs.rs         # ReadLogs
│   │       ├── human.rs        # AskHuman, Confirm
│   │       ├── pipeline.rs     # Pipeline executor
│   │       ├── snapshot.rs     # Snapshot CRUD
│   │       ├── events.rs       # Subscribe, CheckHandle, StopHandle
│   │       ├── meta.rs         # ListActions, DescribeAction, PolicyCheck
│   │       ├── system.rs       # SystemInfo, FileInfo, Checksum, etc.
│   │       └── desktop.rs      # Wayland desktop actions
└── x0ne-orchestrator/          # The agent loop binary
    └── src/
        ├── main.rs             # CLI, main loop, slash commands
        ├── config.rs           # Orchestrator config
        ├── api.rs              # LLM backends + UsageTracker
        ├── bridge.rs           # stdio/HTTP bridge to x0ne-runtime
        ├── conversation.rs     # Message history + handle tracking
        ├── parser.rs           # <action> tag extraction + validation
        └── prompt.rs           # System prompt builder
```

---

## How core and orchestrator communicate

### Stdio mode (default)

The orchestrator spawns `x0ne-runtime` as a child process and exchanges **newline-delimited JSON** over stdin/stdout:

```
orchestrator                    x0ne-runtime
    │                               │
    │  {"type":"ReadFile",...}\n    │
    │ ────────────────────────────▶ │
    │                               │  (policy check)
    │                               │  (executor)
    │  {"success":true,...}\n       │
    │ ◀──────────────────────────── │
```

- One JSON object per line, no framing
- x0ne-runtime's stderr is inherited (logs appear in the orchestrator's terminal)
- Health check on startup: orchestrator sends `{"type":"ListActions"}` and verifies a successful response

### HTTP mode

The orchestrator sends `POST /action` (or `/execute`) and receives the JSON response:

```
orchestrator → POST http://127.0.0.1:7700/action
              ← {"success":true,"output":{...},"duration_ms":4}
```

Health check: `GET /health` with a configurable timeout.

Stdio is preferred for single-user use (no network surface). HTTP is useful when:
- The orchestrator and core run on different machines
- Multiple agents connect to a single core instance
- You want to use core from a language other than Rust

---

## The agent loop

```
read_user_input()
       │
       ├── slash command? → handle, continue
       │
       └── normal input → conv.add_user()
                │
                ▼
        ┌─ INNER LOOP ─────────────────────────────┐
        │                                           │
        │  auto_check_idle_handles()                │
        │  inject handles_summary() into context    │
        │                                           │
        │  llm.send(system_prompt, messages)        │
        │         │                                 │
        │  parse_llm_response()                     │
        │         │                                 │
        │  ┌──────┴──────────────────────┐          │
        │  │ Message  → print, break     │          │
        │  │ Action   → bridge.send()    │          │
        │  │           → process result  │          │
        │  │           → track handles   │          │
        │  │           → continue loop   │          │
        │  │ ParseErr → inject feedback  │          │
        │  │           → retry (max 3)   │          │
        │  └─────────────────────────────┘          │
        └───────────────────────────────────────────┘
```

The loop is bounded by `MAX_CONSECUTIVE_ACTIONS = 20`. After 20 consecutive actions without a text message, the orchestrator injects a "check in with the user" prompt.

---

## How handles work

A **handle** is an opaque string ID returned when an action starts a long-running background task:

```json
{"type":"RunCommand","command":"cargo","args":["test"],"stream":true}
→ {"output":{"kind":"handle_created","handle":"proc_a1b2c3","streaming":true}}
```

### Handle lifecycle

```
Created → Running → [Completed | Failed | Cancelled]
```

Inside `x0ne-runtime`, each handle is backed by:
- A spawned Tokio task
- A ring buffer of output lines / events (bounded by `max_events_per_handle`)
- A `HandleStatus` enum

`CheckHandle` drains the ring buffer and returns current status. The handle is retained for `completed_retention_secs` after completion (default 5 minutes) before being garbage collected.

### Orchestrator-side tracking

The orchestrator maintains an `active_handles: Vec<ActiveHandle>` in the conversation struct. Before each API call:
1. Handles idle longer than `auto_check_interval_secs` (default 60s) are auto-checked via `CheckHandle`
2. A `handles_summary()` is injected as an ephemeral user message so the LLM stays aware
3. Completed handles are removed from the tracker

---

## How pipelines execute

A Pipeline sends multiple actions in one round-trip:

```json
{
  "type": "Pipeline",
  "steps": [{"type":"ReadFile","path":"./a"}, {"type":"ReadFile","path":"./b"}],
  "stop_on_error": true,
  "snapshot_before": "pre-change"
}
```

### Execution flow (inside x0ne-runtime)

```
1. Nesting check — pipelines cannot contain pipelines
2. Policy preflight — ALL steps checked before ANY execute
   └── If any step is denied → entire pipeline rejected immediately
3. Optional snapshot — if snapshot_before is set, create snapshot of write-paths
4. Sequential execution:
   for each step:
     a. execute_action(step)
     b. collect StepResult {success, output, error, duration_ms}
     c. if !success && stop_on_error → break
5. Return PipelineResult {steps_total, steps_completed, stopped_early, results}
```

The policy preflight in step 2 ensures the LLM cannot use a pipeline to sneak a disallowed action past the policy engine.

### Context compaction

In the orchestrator, pipeline results are stored compactly — one summary line per step, full output only for failures. This prevents a 10-step pipeline from bloating the context window with 10 full JSON result objects.

---

## How snapshots work

Snapshots are a safety net for reversible edits. Each snapshot is:
- A named directory in `~/.x0ne/snapshots/<name>/`
- A `manifest.json` describing which files were captured and their hashes
- Copies of each captured file at their relative paths

### Create

```
CreateSnapshot {name: "pre-refactor", paths: ["./src/main.rs", "./Cargo.toml"]}
```

1. Resolve each path relative to the cwd
2. Check size against `max_snapshot_size_mb`
3. Copy files into `~/.x0ne/snapshots/pre-refactor/`
4. Write `manifest.json` with file list, hashes, and timestamp

### Restore

```
RestoreSnapshot {name: "pre-refactor"}
```

1. Read `manifest.json`
2. For each file in manifest: copy back to original location
3. Files added after the snapshot are left untouched (restore is non-destructive)

### Auto-snapshot via Pipeline

```json
{"type": "Pipeline", "snapshot_before": "auto", "steps": [...]}
```

The pipeline executor automatically snapshots all paths in write-type steps before executing.

---

## How desktop actions work

Desktop actions require:
- `desktop.enabled = true` in `x0ne-runtime.toml`
- The action enabled in `[actions]`
- External Wayland tools installed

### Tool mapping

| Action | Binary |
|---|---|
| `Screenshot` | `grim` |
| `GetWindowTree`, `GetFocusedWindow`, `FocusWindow` | `swaymsg` |
| `MoveMouse`, `Click`, `DoubleClick`, `Scroll` | `wlrctl` |
| `TypeText`, `PressKey`, `PressKeyCombination` | `wtype` |
| `SendNotification` | `notify-send` |
| `GetClipboard` | `wl-paste` |
| `SetClipboard` | `wl-copy` |

### Rate limiting

All input actions (click, type, key) share a `RateLimiter = Arc<Mutex<Instant>>`. Before each input action:
1. Check elapsed time since last input ≥ `input_rate_limit_ms` (default 100ms)
2. If not: return `PolicyDenied` — "Input rate limit exceeded"
3. If yes: execute, then update the timestamp

This prevents the LLM from generating runaway input floods.

---

## How the policy engine evaluates actions

Policy evaluation happens before every execution. Order of checks:

```
1. Is the action type enabled in [actions]?
   └── No → Deny(ActionDisabled)

2. Desktop check (for all desktop action types):
   └── !desktop.enabled → Deny(DesktopDisabled)

3. Path checks (for file read/write/list/search/trash):
   a. Deny patterns (glob match) → Deny(PathDenied)
   b. Allowed read paths (if non-empty and action is read-type) → Deny if not prefix match
   c. Allowed write paths (if non-empty and action is write-type) → Deny if not prefix match

4. Command check (RunCommand):
   a. Extract basename of command[0]
   b. Check against allowed_commands list → Deny if not in list
   c. No shell expansion: Command::new() not sh -c

5. default_effect:
   └── "deny" → Deny(DefaultDeny) if none of the above allowed
   └── "allow" → Allow
```

Meta actions (`ListActions`, `DescribeAction`, `PolicyCheck`) bypass the policy check entirely — they are always allowed.

---

## Config loading order

Both binaries use the same search order:

1. Path passed via `--config <file>` flag
2. `./x0ne-runtime.toml` (or `./x0ne-orchestrator.toml`) in the current working directory (runtime: only with `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1`)
3. `~/.config/x0ne/x0ne-runtime.toml` (or `x0ne-orchestrator.toml`)
4. Runtime: none found, it refuses to start and names the file to write. Orchestrator: compiled-in defaults.

All config structs use `#[serde(default)]` so any field can be omitted — a partial config is valid. Unknown TOML keys are silently ignored for forward compatibility.

---

## Parallel Worlds & Snapshot Architecture

> **The LLM thinks in branches, not sequences.**

The Incus backend unlocks a fundamentally different execution model: instead of trying one solution at a time and rolling back on failure, X0NE agents can **fork the world**, spawning N isolated containers simultaneously, each exploring a different approach, then picking the winner.

### The three primitives

#### `RunIncus`, single ephemeral container

A container that lives only for the duration of the task. Files are injected before execution, collected after. The container is automatically deleted when the task ends regardless of outcome (RAII guard).

```json
{
  "type": "RunIncus",
  "image": "images:ubuntu/24.04",
  "remote": "prod-infra",
  "command": ["sh", "-c", "cargo test 2>&1"],
  "files": {
    "src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }"
  },
  "collect_files": ["/workspace/test-report.json"],
  "memory_mb": 512,
  "timeout_secs": 60
}
```

#### `RunIncusGroup`, N containers in parallel

All tasks launch simultaneously (bounded by `max_parallel`). Results come back as a ranked list. Ideal for: running the same test suite across N OS versions, benchmarking N algorithm variants, or trying N patches and picking the first one that passes.

```json
{
  "type": "RunIncusGroup",
  "remote": "prod-infra",
  "max_parallel": 4,
  "tasks": [
    {
      "id": "ubuntu-24",
      "image": "images:ubuntu/24.04",
      "command": ["sh", "-c", "python3 --version && python3 run_tests.py"]
    },
    {
      "id": "alpine",
      "image": "images:alpine/edge",
      "command": ["sh", "-c", "python3 --version && python3 run_tests.py"]
    },
    {
      "id": "debian-12",
      "image": "images:debian/12",
      "command": ["sh", "-c", "python3 --version && python3 run_tests.py"]
    }
  ]
}
```

Response:
```json
{
  "total": 3,
  "succeeded": 2,
  "failed": 1,
  "results": [
    { "id": "ubuntu-24",  "exit_code": 0, "stdout": "...", "duration_ms": 3200 },
    { "id": "alpine",     "exit_code": 1, "stdout": "...", "duration_ms": 1800 },
    { "id": "debian-12",  "exit_code": 0, "stdout": "...", "duration_ms": 2900 }
  ]
}
```

#### `base_snapshot`, CoW clone

Instead of launching from a raw image (slow pull), clone from a pre-warmed snapshot. Each clone is a fully independent container, writes in one don't affect the others.

```json
{
  "type": "RunIncusGroup",
  "remote": "prod-infra",
  "tasks": [
    { "id": "solution-a", "base_snapshot": "template-base/ready", "command": ["python3", "solution_a.py"] },
    { "id": "solution-b", "base_snapshot": "template-base/ready", "command": ["python3", "solution_b.py"] },
    { "id": "solution-c", "base_snapshot": "template-base/ready", "command": ["python3", "solution_c.py"] }
  ]
}
```

All three clones start simultaneously from the same snapshot.

### Rollback & state preservation

A container can save its state before a risky operation using `snapshot_on_exit`:

```json
{ "type": "RunIncus", "snapshot_on_exit": "before-migration", ... }
```

If the next step fails, the LLM can roll back by launching from that snapshot:

```json
{ "type": "RunIncus", "base_snapshot": "my-container/before-migration", ... }
```

This gives X0NE agents a **stateful, reversible execution environment**, equivalent to git branches but for running processes.

### Agent pattern: Parallel Worlds

The recommended pattern for solving non-trivial coding problems:

```
1. CreateSnapshot  → checkpoint current workspace state
2. RunIncusGroup   → try N solutions in parallel (all from same base_snapshot)
3. Inspect results → pick winner (exit_code=0, fastest, smallest output, ...)
4. Apply winner    → copy winning files back to workspace
5. Verify          → RunIncus (one final run to confirm)
```

Covered by the `incus` scenario file (13 cases, run against a real Incus server by `make scenarios-incus`): parallel groups, CoW clone from a snapshot, env injection, memory cap, and RAII cleanup (no leaked container after the run).
