# x0ne-runtime

**Secure local capability runtime for AI agents.**

x0ne-runtime is one half of the X0NE system — it is the *arms*. It receives JSON actions from an LLM, enforces a local policy engine, executes only what is explicitly allowed, and returns structured JSON results.

```
x0ne-orchestrator  ←→  (stdio / HTTP)  ←→  x0ne-runtime
    "the brain"                               "the arms"
```

---

## Features (v0.2)

| Capability | Actions | Feature gate |
|---|---|---|
| **Filesystem** | ReadFile, ReadFileChunk, WriteFile, AppendFile, ReplaceInFile, EditFile, InsertAtLine, ApplyPatch, ListDir, TreeView, FindFiles, CreateDir, MoveToTrash, CopyFile, MoveFile, CreateTempFile | always |
| **Search** | SearchCode (regex across files) | always |
| **Process** | RunCommand (blocking + streaming) | always |
| **Podman** | RunInSandbox, RunContainerGroup (parallel containers, resource caps) | always |
| **Pipeline** | Pipeline (sequential, stop_on_error, parallel, auto-snapshot) | always |
| **Try / Assert** | Try (snapshot + rollback), Assert (file_exists, eq, not_empty…) | always |
| **Snapshots** | CreateSnapshot, RestoreSnapshot, ListSnapshots, DeleteSnapshot | always |
| **Math** | Add, Sub, Mul, Div, Mod, Abs, Pow, Sqrt, Min, Max, Round, Compare, Gt, Lt, Eq, Clamp, Sum, Avg | always |
| **String** | StrLen, StrContains, StrCount, StrReplace, StrSplit, StrUpper, StrLower, StrTrim, StrSlice | always |
| **Memory** | Remember, Recall, Forget, ListMemory (namespaced key-value) | always |
| **Validation** | Diff, Validate (type / schema checks) | always |
| **System** | SystemInfo, FileInfo, Checksum, ListProcesses, Whoami, DiskUsage, Uptime, Sleep | always |
| **Data** | ReadJson, WriteJson, ParseToml, RandomBytes (hex/base64) | always |
| **Logs** | ReadLogs | always |
| **Human** | AskHuman, Confirm, AskMultipleChoice | always |
| **Meta** | ListActions, DescribeAction, PolicyCheck | always |
| **Composites** | DefinePrimitive, CallPrimitive, ListCustomPrimitives, DeletePrimitive | always |
| **Network** | FetchUrl, HttpRequest (GET/POST/PUT/PATCH/DELETE/HEAD), PortCheck, DnsLookup | `prim-network` |
| **Git** | GitStatus, GitDiff | `prim-git` |
| **Events** | Subscribe (file/port/process), CheckHandle, StopHandle, ListHandles | `prim-events` |
| **Desktop** | Screenshot, GetWindowTree, MoveMouse, Click, TypeText, PressKey, FocusWindow, Clipboard, Notifications | `prim-desktop` |
| **ML** | OllamaChat, OllamaApi (model management) | `prim-ml` |

> **Incus** (RunIncus, RunIncusGroup, Incus snapshots) lives in **x0ne-orchestrator**, not the runtime — privilege boundary fix: the runtime may run inside the very container these primitives would manage.

> **Feature gates:** By default all features are compiled (`all-prims`). Build with `--no-default-features` for a lean binary, then add individual `--features prim-git,prim-network` etc.

---

## Quick start

```bash
# Stdio mode (default — used by x0ne-orchestrator)
cargo run -- --config x0ne-runtime.toml

# HTTP mode
cargo run -- --mode http --bind 127.0.0.1:7878

# MCP mode (the clients docs/mcp.md configures)
cargo run -- --mode mcp

# Self-test (debug builds only)
cargo run -- --self-test
```

The binary reads newline-delimited JSON from stdin and writes newline-delimited JSON to stdout.

Three communication modes:
| Mode | Transport | Use case |
|------|-----------|----------|
| **stdio** | ndjson on stdin/stdout | Default, used by x0ne-orchestrator |
| **HTTP** | REST POST `/execute` + bearer auth | Web dashboards, remote access |
| **MCP** | JSON-RPC over stdio | Claude Code, Codex, Claude Desktop, Cursor, Windsurf (`docs/mcp.md`) |

---

## Action reference

Every request is a JSON object with a `"type"` field. Every response is an `ActionResult`:

```jsonc
{
  "id": "550e8400-...",
  "success": true,
  "action_type": "ReadFile",
  "output": { "kind": "file_content", "path": "...", "content": "...", "size_bytes": 42, "truncated": false },
  "error": null,           // present only on failure
  "timestamp": "2026-...",
  "duration_ms": 3
}
```

---

### Filesystem

#### `ReadFile`
```json
{ "type": "ReadFile", "path": "./src/main.rs", "offset": 0, "limit": 200 }
```
Returns: `file_content`  
Policy: read path checked, deny patterns checked.

#### `WriteFile`
```json
{ "type": "WriteFile", "path": "/tmp/hello.txt", "content": "hello\n" }
```
Returns: `file_written`

#### `AppendFile`
```json
```

#### `ReplaceInFile`
```json
```

#### `ListDir`
```json
```
Returns: `dir_listing`

#### `FindFiles`
```json
```
Returns: `file_list`

#### `CreateDir`
```json
```

#### `MoveToTrash`
```json
{ "type": "MoveToTrash", "path": "/tmp/old_file.txt" }
```

#### `CopyFile`
```json
```
Copies a file. Creates parent directories if needed. Returns: `file_copied` — `{ src, dest, bytes_copied }`.

#### `MoveFile`
```json
```
Moves/renames a file or directory. Returns: `file_moved`.

#### `CreateTempFile`
```json
```
All fields optional. Returns: `temp_file_created` — `{ path, size_bytes }`.

---

### Search

#### `SearchCode`
```json
{ "type": "SearchCode", "root": "./src", "pattern": "TODO", "file_glob": "*.rs", "limit": 20 }
```
Returns: `search_matches` — list of `{ file, line, content }`.

---

### Git

#### `GitStatus`
```json
```
Returns: `git_status` — list of `{ status, path }`.

#### `GitDiff`
```json
```
Returns: `git_diff` — unified diff string.

---

### Process

#### `RunCommand` — blocking
```json
{ "type": "RunCommand", "command": "cargo", "args": ["test"], "cwd": ".", "timeout_secs": 60 }
```
Returns: `command_output` — `{ exit_code, stdout, stderr, timed_out }`.

#### `RunCommand` — streaming
```json
{ "type": "RunCommand", "command": "cargo", "args": ["build"], "stream": true }
```
Returns: `handle_created` — poll the handle with `CheckHandle`.

Each `CheckHandle` call drains buffered events (ring-buffer, oldest dropped when full):
```jsonc
{ "stream": "stdout", "line": "Compiling foo v0.1.0", "timestamp": "..." }
{ "stream": "stderr", "line": "warning: unused import", "timestamp": "..." }
{ "stream": "exit",   "exit_code": 0, "timestamp": "..." }
```

---

### Logs

#### `ReadLogs`
```json
{ "type": "ReadLogs", "path": "/var/log/app.log", "lines": 50 }
```
Returns: `log_lines`.

---

### Human-in-the-loop

#### `AskHuman`
```json
{ "type": "AskHuman", "question": "Which branch should I target?" }
```
Blocks until the human answers on stdin.

#### `Confirm`
```json
{ "type": "Confirm", "message": "Delete all build artifacts?" }
```
Yes/No prompt. Returns `confirmed: true/false`.

---

### Action Pipelines

Execute multiple actions sequentially in a **single round-trip**. Policy is checked for every step before any step executes.

```json
{
  "type": "Pipeline",
  "steps": [
    { "type": "CreateSnapshot", "name": "before-refactor", "paths": ["./src/main.rs"] },
    { "type": "RunCommand", "command": "cargo", "args": ["test"] }
  ],
  "stop_on_error": true,
  "snapshot_before": "auto-before-pipeline"
}
```

- `stop_on_error` (default `true`) — stops on first failure.
- `snapshot_before` — auto-snapshot write-target files before execution.
- **Pipelines cannot be nested.**

Returns: `pipeline_result` — `{ steps_total, steps_completed, stopped_early, results[] }`.

**Per-step error reporting:** If a step has a schema mismatch, the error includes the step index and type: `"Failed to parse step 2 (Min): missing field 'values'"`.

---

### Try / Assert

Transactional execution with automatic snapshot and rollback.

#### `Try`
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
If any action fails → all file changes are **automatically restored** from snapshot.
- `rollback_on_fail` (default `true`) — auto-restore on failure
- `keep_snapshot` (default `false`) — keep the snapshot even on success
- `label` — human-readable identifier

Returns: `try_result` — `{ ok, completed, total, failed_step, rolled_back, label }`.

#### `Assert`
```json
{"type": "Assert", "check": "file_exists", "path": "/tmp/output.txt"}
{"type": "Assert", "check": "eq", "actual": 42, "expected": 42}
{"type": "Assert", "check": "not_empty", "value": "hello"}
```

Supported checks:
| Check | Fields | Description |
|-------|--------|-------------|
| `file_exists` | `path` | File exists on disk |
| `file_contains` | `path`, `value` | File contains substring |
| `port_open` | `host`, `port` | TCP port is reachable |
| `command_succeeds` | `command`, `args` | Command exits 0 |
| `eq` | `actual`, `expected` | Values are equal |
| `gt` / `lt` | `actual`, `expected` | Numeric comparison |
| `not_empty` | `value` | Value is non-null and non-empty |
| `env_set` | `name` | Environment variable is set |

Returns: `assert_result` — `{ ok, check, detail }`.

---

### Math

Pure arithmetic operations — no side effects, always allowed.

```json
```

18 operations: `Add` `Sub` `Mul` `Div` `Mod` `Abs` `Pow` `Sqrt` `Min` `Max` `Round` `Compare` `Gt` `Lt` `Eq` `Clamp` `Sum` `Avg`

---

### String

Pure string operations — no side effects, always allowed.

```json
```

9 operations: `StrLen` `StrContains` `StrCount` `StrReplace` `StrSplit` `StrUpper` `StrLower` `StrTrim` `StrSlice`

**Field aliases:** `StrContains` and `StrCount` accept `input`, `text`, or `string` as aliases for the canonical `haystack` field. All four names are equivalent — use whichever is natural.

**StrSlice clamping:** `end` is clamped to string length (Python-style). `{"type": "StrSlice", "input": "short", "start": 0, "end": 100}` returns `"short"` instead of erroring.

---

### Memory

Namespaced key-value store persisted to `~/.config/x0ne/memory/`.

```json
{"type": "Remember", "key": "project_lang", "value": "rust", "namespace": "context"}
{"type": "Recall", "key": "project_lang", "namespace": "context"}
{"type": "Forget", "key": "project_lang", "namespace": "context"}
{"type": "ListMemory", "namespace": "context"}
```

Memory persists across sessions — useful for project context, user preferences, and accumulated knowledge.

---

### Validation

#### `Validate`
```json
{"type": "Validate", "value": "hello", "check": "type", "expect": "string"}
{"type": "Validate", "value": 42, "check": "range", "expect": {"min": 0, "max": 100}}
```

#### `Diff`
```json
{"type": "Diff", "a": "hello\nworld", "b": "hello\nrust", "files": false}
{"type": "Diff", "a": "./old.txt", "b": "./new.txt", "files": true, "context_lines": 3}
```
Returns unified diff output.

---

### File Snapshots

Point-in-time copies of files stored on disk as a content-addressed archive with a `manifest.json`.

#### `CreateSnapshot`
```json
{ "type": "CreateSnapshot", "name": "before-refactor", "paths": ["./src/main.rs", "./Cargo.toml"] }
```

#### `RestoreSnapshot`
```json
{ "type": "RestoreSnapshot", "name": "before-refactor" }
```

#### `ListSnapshots`
```json
{ "type": "ListSnapshots" }
```

#### `DeleteSnapshot`
```json
{ "type": "DeleteSnapshot", "name": "before-refactor" }
```

Name rules: alphanumeric, hyphens, underscores, max 64 chars.

---

### Event Subscriptions

Subscribe to machine events. Returns a **handle** immediately; poll with `CheckHandle`.

#### Subscribe to file changes
```json
{
  "type": "Subscribe",
  "event_type": { "kind": "file_changed", "path": "/tmp/watch_me", "recursive": false }
}
```
Events: `{ "event": "modified"|"created"|"deleted", "path": "...", "timestamp": "..." }`

#### Subscribe to file creation
```json
{
  "type": "Subscribe",
  "event_type": { "kind": "file_created", "dir": "/tmp", "pattern": "*.log" }
}
```

#### Subscribe to process exit
```json
{
  "type": "Subscribe",
  "event_type": { "kind": "process_exited", "pid": 12345 }
}
```
Event: `{ "event": "exited", "pid": 12345, "timestamp": "..." }`

#### Subscribe to port open
```json
{
  "type": "Subscribe",
  "event_type": { "kind": "port_open", "host": "127.0.0.1", "port": 8080 }
}
```
Polls every `events.port_check_interval_secs` seconds up to `events.port_check_timeout_secs`.
Event: `{ "event": "port_open", "host": "...", "port": 8080, "timestamp": "..." }`

---

### Handle management

#### `CheckHandle`
```json
{ "type": "CheckHandle", "handle": "proc_a1b2c3d4" }
```
Drains buffered events (cleared after each read). Returns status + events.

#### `StopHandle`
```json
{ "type": "StopHandle", "handle": "watch_a1b2c3d4" }
```

#### `ListHandles`
```json
{ "type": "ListHandles" }
```

---

### Capability discovery (always allowed)

#### `ListActions`
```json
{ "type": "ListActions" }
```
Returns the full registry of action types with schema and category.

#### `DescribeAction`
```json
{ "type": "DescribeAction", "action_type": "Pipeline" }
```
Returns schema, examples, mutable flag.

#### `PolicyCheck`
```json
{
  "type": "PolicyCheck",
  "action": { "type": "WriteFile", "path": "/etc/passwd", "content": "bad" }
}
```
Returns `{ allowed, action_type, reason }` — never executes the action.

---

### Composite Primitives

Define reusable, parameterized macros from existing primitives. Composites are stored on disk (`~/.config/x0ne/primitives/`) and appear in `ListActions`/`DescribeAction` alongside native actions.

#### `DefinePrimitive`
```json
{
  "type": "DefinePrimitive",
  "name": "HealthCheck",
  "description": "Check if a service is alive and logs are clean",
  "params": {
    "url": { "type": "string", "required": true },
    "log_path": { "type": "string", "required": true },
    "error_pattern": { "type": "string", "default": "ERROR" }
  },
  "steps": [
    { "type": "HttpRequest", "url": "{url}", "method": "HEAD", "timeout_secs": 5 },
    { "type": "ReadLogs", "path": "{log_path}", "lines": 50 },
    { "type": "SearchCode", "root": "{log_path}", "pattern": "{error_pattern}", "limit": 10 }
  ]
}
```
Returns: `primitive_defined` — `{ name, param_count, step_count, ephemeral }`.

**Validation rules:**
- Name cannot collide with any native action type
- Steps cannot contain `DefinePrimitive` (no nested definitions)
- Steps cannot contain `CallPrimitive` referencing itself (no recursion)
- All step types must exist (native or another composite)
- Max steps: 20 (configurable via `composites.max_steps`)

**Params:** Use `{param_name}` placeholders in step JSON. Supports partial strings (`"{url}/api/health"`), nested objects, and arrays. Required params error if missing; optional params use defaults.

#### `CallPrimitive`
```json
{
  "type": "CallPrimitive",
  "name": "HealthCheck",
  "args": {
    "url": "https://api.example.com",
    "log_path": "/var/log/app.log"
  }
}
```
Returns: `pipeline_result` — same format as Pipeline. Each step is individually policy-checked before execution.

**Depth limit:** Composites calling other composites are limited to `composites.max_depth` (default: 3).

#### `ListCustomPrimitives`
```json
{ "type": "ListCustomPrimitives" }
```
Returns: `primitive_list` — `{ primitives: [{ name, description, params, step_count, ephemeral, created_at }] }`.

Always allowed (no policy check required).

#### `DeletePrimitive`
```json
{ "type": "DeletePrimitive", "name": "HealthCheck" }
```
Returns: `primitive_deleted` — `{ name, deleted: true/false }`.

**Ephemeral mode:** Set `"ephemeral": true` on `DefinePrimitive` to skip disk persistence — the composite only lives in memory for the current session.

**Config:**
```toml
[composites]
max_depth = 3                                    # max nested call depth
max_steps = 20                                   # max steps per definition
primitives_dir = "~/.config/x0ne/primitives"     # disk storage
```

---

### System information

#### `SystemInfo`
```json
```
Returns: hostname, OS, arch, CPU count, memory (total / available), disk free.

#### `FileInfo`
```json
{ "type": "FileInfo", "path": "./src/main.rs" }
```
Returns: size, permissions (octal), is_dir, is_symlink, symlink_target, modified, created, MIME type.

#### `Checksum`
```json
{ "type": "Checksum", "path": "./Cargo.lock", "algorithm": "sha256" }
```
Algorithms: `sha256` (default), `md5`, `sha1`.

#### `ListProcesses`
```json
{ "type": "ListProcesses", "filter": "cargo" }
```
Reads `/proc` directly — no subprocess. Requires `system.enable_process_list = true`.
Arguments that look like credentials come back masked (`--password=[redacted]`,
`--token [redacted]`, `https://[redacted]@host`, `--user name:[redacted]`, `mysql -p[redacted]`,
`sshpass -p`, `redis-cli -a`, `curl -u`, known token shapes), inside `sh -c` scripts too; the
other arguments are unchanged. Masking is pattern-based: a secret in another shape is returned.

#### `PortCheck`
```json
{ "type": "PortCheck", "host": "127.0.0.1", "port": 8080, "timeout_secs": 5 }
```
Returns: `{ open, latency_ms }`.

#### `GetEnv`
```json
```
Only variables in `policy.readable_env_vars` are accessible.

#### `DnsLookup`
```json
{ "type": "DnsLookup", "hostname": "api.example.com" }
```
Returns: list of resolved IP addresses.

#### `Whoami`
```json
```
Returns: `{ username, uid, gid, home, shell }`.

#### `DiskUsage`
```json
```
Returns: `{ total_bytes, used_bytes, free_bytes, usage_percent }`.

#### `Uptime`
```json
```
Returns: `{ seconds, load_avg: [1min, 5min, 15min] }`.

---

### Network

#### `FetchUrl`
```json
{ "type": "FetchUrl", "url": "https://example.com", "headers": {"Accept": "text/plain"} }
```
HTTP GET only. Returns: `{ url, status, body }`.

#### `HttpRequest`
```json
{
  "type": "HttpRequest",
  "url": "https://httpbin.org/post",
  "method": "POST",
  "headers": { "Content-Type": "application/json" },
  "body": "{\"key\": \"value\"}",
  "timeout_secs": 30
}
```
Full HTTP: GET/POST/PUT/PATCH/DELETE/HEAD. Returns: `{ url, status, headers, body }`.

---

### Data

#### `ReadJson`
```json
```
Reads and parses a JSON file. Returns: `{ path, data, size_bytes }` where `data` is the parsed JSON structure.

#### `WriteJson`
```json
```
Writes structured data as JSON. `pretty` defaults to `true`. Returns: `{ path, bytes_written, pretty }`.

#### `ParseToml`
```json
```
Reads and parses a TOML file. Returns data as JSON-compatible structure: `{ path, data, size_bytes }`.

---

### Crypto

#### `RandomBytes`
```json
{ "type": "RandomBytes", "count": 32, "encoding": "hex" }
```
Generates cryptographically random bytes from `/dev/urandom`. Max 1MB.  
Encoding: `"hex"` (default) or `"base64"`. Returns: `{ bytes, encoding, count }`.

---

### Desktop / Wayland

> **Requires:** `desktop.enabled = true` in config **and** each action enabled under `[actions]`.  
> External binaries: `grim` (screenshots), `wlrctl` (mouse), `wtype` (keyboard), `swaymsg` (windows), `wl-copy`/`wl-paste` (clipboard), `notify-send` (notifications).

#### `Screenshot`
```json
{ "type": "Screenshot", "output": "HDMI-A-1", "region": "0,0 1920x1080" }
```
Saved to `desktop.screenshot_dir`. Returns path, dimensions, size.

#### `GetWindowTree`
```json
{ "type": "GetWindowTree" }
```
Returns flattened list of Sway windows.

#### `GetFocusedWindow`
```json
{ "type": "GetFocusedWindow" }
```

#### `MoveMouse`
```json
{ "type": "MoveMouse", "x": 500, "y": 300 }
```

#### `Click` / `DoubleClick`
```json
{ "type": "Click", "x": 100, "y": 200, "button": "left" }
{ "type": "DoubleClick", "x": 100, "y": 200 }
```
Buttons: `left`, `right`, `middle`.

#### `Scroll`
```json
{ "type": "Scroll", "direction": "down", "amount": 3 }
```
Directions: `up`, `down`, `left`, `right`.

#### `TypeText`
```json
{ "type": "TypeText", "text": "Hello, World!" }
```

#### `PressKey`
```json
{ "type": "PressKey", "key": "Return" }
```

#### `PressKeyCombination`
```json
{ "type": "PressKeyCombination", "keys": ["ctrl", "s"] }
```
Modifiers: `ctrl`, `alt`, `shift`, `super`.

#### `FocusWindow`
```json
{ "type": "FocusWindow", "match_str": "firefox" }
```
Matches against window title and `app_id`.

#### `SendNotification`
```json
{ "type": "SendNotification", "title": "Build done", "body": "All tests passed", "urgency": "normal" }
```
Urgency: `low`, `normal`, `critical`.

#### `GetClipboard` / `SetClipboard`
```json
{ "type": "GetClipboard" }
{ "type": "SetClipboard", "content": "copied text" }
```

---

### Podman containers

> Daemonless, rootless OCI containers. Works on Linux and macOS (via `podman machine`).

#### `RunInSandbox`
```json
{
  "type": "RunInSandbox",
  "image": "python:3.12-slim",
  "command": ["python", "main.py"],
  "files": {"main.py": "print('hello')"},
  "collect_files": ["/tmp/output.txt"],
  "memory_mb": 256,
  "cpu_limit": 2,
  "timeout_secs": 30,
  "network": false
}
```
Single ephemeral Podman container — inject files, run, collect output files via `podman cp`, auto-delete.

- **Image allowlist** — only images in `sandbox.podman.allowed_images` are accepted (fail-closed)
- **Resource caps** — `memory_mb`, `cpu_limit`, `timeout_secs` enforced against config maximums
- **Security** — `--cap-drop=ALL`, `--no-new-privileges`, env key validation, path traversal prevention
- **RAII cleanup** — `ContainerGuard` force-removes container even on panic or timeout

#### `RunContainerGroup`
```json
{
  "type": "RunContainerGroup",
  "tasks": [
    {"id": "test-py", "image": "python:3.12-slim", "command": ["python", "-m", "pytest"]},
    {"id": "lint",    "image": "node:20-slim",      "command": ["npx", "eslint", "."]},
    {"id": "build",   "image": "alpine",            "command": ["make", "release"]}
  ],
  "memory_mb": 256,
  "timeout_secs": 60,
  "max_parallel": 4
}
```
N containers in parallel — semaphore-limited concurrency (default: 4). Each task has its own image, command, files, and env vars.

**Config:**
```toml
[sandbox.podman]
allowed_images = ["alpine", "python:3.12-slim", "node:20-slim"]
max_memory_mb = 1024
max_timeout_secs = 300
max_parallel_instances = 4
```

→ Full setup: [docs/sandbox.md](../docs/sandbox.md)

---

### Incus containers

> **Moved to x0ne-orchestrator.** RunIncus, RunIncusGroup, and the four
> Incus*Snapshot primitives all live in `x0ne-orchestrator/src/incus_actions.rs`
> now — the runtime no longer dispatches them. Privilege boundary fix:
> the runtime may run *inside* the container these primitives would manage,
> which would have been a structurally unsafe self-management scenario.
>
> The wire format the LLM emits is unchanged; only the dispatch site moved.
> Schemas below are kept here for reference.

#### `RunIncus`
```json
{
  "type": "RunIncus",
  "image": "images:ubuntu/24.04",
  "remote": "prod-infra",
  "command": ["sh", "-c", "cargo test"],
  "files": {"src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }"},
  "timeout_secs": 60
}
```
Single ephemeral LXC container — inject files, run, collect output, auto-delete.

#### `RunIncusGroup`
```json
{
  "type": "RunIncusGroup",
  "remote": "prod-infra",
  "max_parallel": 4,
  "tasks": [
    {"id": "ubuntu", "image": "images:ubuntu/24.04", "command": ["python3", "test.py"]},
    {"id": "alpine", "image": "images:alpine/edge", "command": ["python3", "test.py"]}
  ]
}
```
N containers in parallel — same btrfs snapshot, different commands. CoW clone < 200ms.

#### Time Travel — snapshot & rollback

On btrfs-backed remotes, every container state is a **free checkpoint**:

- `base_snapshot` — freeze a known-good state before exploring. All `RunIncusGroup` forks start from this frozen point.
- `snapshot_on_exit` — save the final container state as a named rollback point before auto-deletion.
- `RestoreSnapshot` — rewind to any checkpoint in < 200ms (btrfs reflink, zero-copy).

The agent can explore aggressively — risky refactors, destructive tests, speculative builds — knowing that **mistakes are free**. Any snapshot is an instant undo.

---

### ML / Multimodal (Ollama)

> Requires `prim-ml` feature (default: on)

#### `OllamaChat`

Multi-turn chat with optional vision. Supports image attachments for multimodal models (llava, bakllava).

```json
{"type": "OllamaChat", "endpoint": "http://localhost:11434", "model": "mistral",
 "messages": [{"role": "user", "content": "hello"}]}
```

Vision example (llava):
```json
{
  "type": "OllamaChat",
  "endpoint": "http://localhost:11434",
  "model": "llava",
  "messages": [
    {"role": "user", "content": "What do you see in this image?",
     "images": ["iVBORw0KGgo...base64..."]}
  ]
}
```

Each message can carry an `images` array of base64-encoded strings. Non-vision models ignore the field.

#### `OllamaApi`
```json
{"type": "OllamaApi", "endpoint": "http://localhost:11434", "operation": "list"}
```
Operations: `list`, `pull`, `create`, `delete`, `show`.

---

## Security

### SSRF Protection

All network actions (`FetchUrl`, `HttpRequest`, `PortCheck`, `DnsLookup`) pass through `is_ssrf_target()` which blocks:

| Target | Blocked |
|--------|---------|
| Cloud metadata | `169.254.169.254`, `metadata.google.internal` |
| Loopback | `127.0.0.0/8`, `::1` (unless `X0NE_ALLOW_LOOPBACK=1`) |
| Private networks | `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` |
| Link-local | `169.254.0.0/16`, `fe80::/10` |

### Path Safety

- All paths resolved via `std::fs::canonicalize()` — blocks `../` traversal and symlink escapes
- Hardcoded deny patterns: `.ssh`, `.gnupg`, `*.pem`, `*.key`, `secrets/`
- UTF-8 safe truncation prevents panics on multi-byte sequences

---

## Policy engine

Rules are evaluated in order. First match wins (deny short-circuits).

1. **Meta always-allow** — `ListActions`, `DescribeAction`, `PolicyCheck`, `CheckHandle`, `ListHandles`, `ListCustomPrimitives` are unconditionally allowed.
2. **Desktop gate** — any desktop action is denied with `desktop_disabled` if `desktop.enabled = false`.
3. **Per-action enable switch** — check `[actions]` map; if `false` → `action_disabled`.
4. **Default effect** — if action not in map and `default_effect = "deny"` → `default_deny`.
5. **Pipeline preflight** — all steps are policy-checked before any executes; nested pipelines are rejected.
6. **Deny path patterns** — glob match against `deny_path_patterns`.
7. **Path allowlists** — `allowed_read_paths` / `allowed_write_paths` (when non-empty).
8. **Command allowlist** — `RunCommand` checked against `allowed_commands`.
9. **GetEnv whitelist** — `readable_env_vars`.
10. **Subscribe path rules** — file-watch paths follow read-path rules.
11. **Child environment allowlist** — every process the runtime spawns starts
    from `env_clear()` plus the allowlist in `src/child_env.rs`, widened only by
    `policy.env_passthrough` (empty by default). This one is not a verdict the
    policy engine returns; it is applied at the spawn site, so it holds for
    `RunCommand`, `RunInSandbox`, `SshExec`, `ArtifactServe`, the desktop
    primitives and `DocumentExtract` alike. Before 2026-08-15 a child inherited
    the operator's whole environment and `printenv` returned it to the model
    and to the session log (`AUDIT-2026-08-14.md`, section 4).

---

## Self-test

x0ne-runtime includes a built-in diagnostic harness that probes every primitive family:

```bash
# Debug builds (automatic)
cargo run -- --self-test

# Release builds (opt-in)
cargo run --release --features self_test -- --self-test
```

Probes: system, memory, math, string, fs, meta + feature-gated: network, git, events, desktop, incus, ml. Returns structured `SelfTestReport` with pass/fail/skip per family.

---

## Testing

x0ne-runtime has a comprehensive zero-inference test suite — no LLM required:

```bash
# Unit tests (MCP compliance + internal)
cargo test --workspace

# Scenario suite — 175 JSON-driven test cases
cargo run --bin x0ne-scenario-runner

# Incus integration (requires remote)
cargo run --bin x0ne-scenario-runner -- --scenario tests/scenarios/incus.json --incus

# Single action probe (ad-hoc)
cargo run --bin x0ne-scenario-runner -- --probe '{"type":"SystemInfo"}'

# Full suite from workspace root
make test-all
```

### Scenario runner

The `x0ne-scenario-runner` is an in-process harness that bypasses LLM inference entirely: it loads JSON scenario files, injects actions directly into the runtime (parser → policy → executor), and asserts on structured results.

| Category | Tests | Description |
|----------|-------|-------------|
| `happy_path` | 42 | Full CRUD, git, system info, memory, validation |
| `edge_cases` | 22 | Empty paths, nonexistent files, zero timeouts |
| `policy_violations` | 22 | Path traversal, forbidden commands, SSRF, secrets |
| `malformed` | 18 | Invalid JSON, missing fields, wrong types |
| `qcm` | 16 | AskMultipleChoice: skip, other, out-of-range |
| `human_interaction` | 12 | AskHuman, Confirm with scripted responses |
| `pipeline_advanced` | 12 | Parallel, stop_on_error, snapshot, nested rejection |
| `composites` | 10 | DefinePrimitive/CallPrimitive lifecycle |
| `snapshots` | 9 | Create/restore/delete/list snapshots |
| `try_rollback` | 7 | Transactional execution with automatic rollback |
| `pipeline` | 5 | Sequential multi-step execution |

### MCP compliance tests

32 tests in `tests/mcp_compliance.rs` validate conformance with the MCP specification (2024-11-05):
- JSON-RPC 2.0 message format (integer/string IDs, notifications)
- Tool registry (unique names, snake_case, inputSchema with properties)
- Essential tools present (`read_file`, `write_file`, `list_dir`, `run_command`, `search_code`, `replace_in_file`)
- Error code constants match JSON-RPC spec
- Serialization roundtrips
- Schema validation (all tools have `type: "object"`, required is array)

### Incus integration tests

13 tests against real ephemeral containers on a remote Incus server:

| Test | What it validates |
|------|-------------------|
| `single-echo-alpine` | Basic container lifecycle (launch → exec → cleanup) |
| `file-injection-shell` | File injection + shell execution |
| `file-collection` | Collect files from container after execution |
| `timeout-kills-container` | Timeout enforcement with RAII cleanup |
| `image-allowlist-blocks-unknown` | Policy: unknown images are rejected |
| `parallel-worlds-pick-winner` | RunIncusGroup: 3 variants, pick fastest |
| `parallel-4-containers-timing` | 4 containers complete faster than sequential |
| `parallel-semaphore-cap` | Concurrency limit enforced (max_parallel) |
| `cross-platform-alpine-ubuntu` | Same command on Alpine + Ubuntu |
| `snapshot-on-exit` | Container state saved as snapshot |
| `cow-clone-from-snapshot` | Instant CoW clone from base snapshot |
| `env-injection` | Environment variable injection |
| `memory-cap-enforced` | Memory limit enforcement |

**Pre-flight check:** When a non-local remote is configured, the runner probes connectivity with a 5s timeout before running any container tests. Unreachable remotes are skipped with a warning.

## Feature gates

By default all features compile (`all-prims`). For lean builds:

```bash
# Bare runtime (~50% binary size)
cargo build --release --no-default-features

# Headless server (no desktop, keep network/git)
cargo build --release --no-default-features \
    --features "prim-network prim-git prim-events"
```

| Feature | Primitives |
|---------|------------|
| `prim-desktop` | Screenshot, Click, TypeText, PressKey, FocusWindow, Clipboard, Notifications |
| `prim-ml` | OllamaChat, OllamaApi |
| `prim-git` | GitStatus, GitDiff |
| `prim-network` | FetchUrl, HttpRequest, PortCheck, DnsLookup |
| `prim-events` | Subscribe, CheckHandle, StopHandle, ListHandles |
| `self_test` | Built-in diagnostic harness (auto in debug) |

When a feature is disabled, the action still deserializes (wire format is stable) but the executor returns `ErrorKind::FeatureDisabled`.

---

## Project layout

```
x0ne-runtime/
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs             ← CLI entry point (stdio / http / mcp)
    ├── action.rs           ← Action enum (80+ variants, serde internally-tagged)
    ├── result.rs           ← ActionResult + ActionOutput + ErrorKind
    ├── config.rs           ← AppConfig and all sub-structs
    ├── policy.rs           ← PolicyEngine + DenyReason (deny-by-default)
    ├── handles.rs          ← HandleManager for streaming + event subscriptions
    ├── server.rs           ← stdio + HTTP server (axum)
    ├── composite.rs        ← Composite primitives store + disk persistence
    ├── self_test.rs        ← Built-in diagnostic harness (debug/opt-in)
    ├── mcp/
    │   ├── server.rs       ← MCP JSON-RPC server
    │   ├── registry.rs     ← Tool registry (80+ tools with JSON Schema)
    │   ├── jsonrpc.rs       ← JSON-RPC 2.0 types
    │   └── transport.rs    ← Stdio transport
    └── executor/
        ├── mod.rs          ← dispatch table (match Action → executor)
        ├── fs.rs           ← filesystem (read, write, edit, copy, move…)
        ├── tree.rs         ← TreeView (.gitignore-aware)
        ├── search.rs       ← SearchCode (regex)
        ├── process.rs      ← RunCommand (blocking + streaming + handles)
        ├── sandbox.rs      ← RunInSandbox (Podman, RAII guard, image allowlist)
        ├── container_group.rs ← RunContainerGroup (parallel Podman, semaphore)
        ├── pipeline.rs     ← Pipeline (batch, parallel, snapshot)
        ├── try_block.rs    ← Try (snapshot + rollback)
        ├── assert.rs       ← Assert (9 check types)
        ├── snapshot.rs     ← File snapshots (content-addressed)
        ├── math.rs         ← 18 arithmetic operations
        ├── string.rs       ← 9 string operations
        ├── memory.rs       ← Namespaced key-value store
        ├── validate.rs     ← Diff + Validate
        ├── meta.rs         ← ListActions / DescribeAction / PolicyCheck
        ├── system.rs       ← SystemInfo, SSRF protection, env, ports
        ├── human.rs        ← AskHuman + Confirm + AskMultipleChoice
        ├── patch.rs        ← ApplyPatch (unified diff + fuzzy matching)
        ├── logs.rs         ← ReadLogs
        ├── desktop.rs      ← Wayland/Sway desktop automation
        ├── git.rs          ← GitStatus, GitDiff (libgit2)
        ├── events.rs       ← Subscribe / file watch / port watch
        ├── incus.rs        ← RunIncus / RunIncusGroup (LXC)
        └── ml.rs           ← OllamaChat / OllamaApi
```

---

## Communication protocol

**Stdio (default):**  
One JSON object per line on stdin → one JSON object per line on stdout.

```bash
echo '{"type":"SystemInfo"}' | cargo run
echo '{"type":"ListActions"}' | cargo run
echo '{"type":"ReadFile","path":"/etc/hostname"}' | cargo run
```

**HTTP:**
```bash
TOKEN=$(cat "$XDG_RUNTIME_DIR/x0ne/runtime-7878.token")   # or [runtime.http] auth_token
curl -X POST http://127.0.0.1:7878/execute \
     -H "Authorization: Bearer $TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"type":"ListActions"}'

# Handle routes
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/handles
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/handles/proc_a1b2c3d4
```

---

## Building

```bash
cargo build --release
```

Dependencies of note:
- `axum` — HTTP server
- `notify` — filesystem event watching
- `git2` — libgit2 bindings
- `sha2` / `md-5` / `sha1` — checksums
- `glob` / `regex` — path and code search
- `once_cell` — lazy action registry
- `dirs` — cross-platform home directory

---

## License

Apache-2.0
