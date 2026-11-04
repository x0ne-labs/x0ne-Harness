# Action Reference

<!-- GENERATED FILE — do not edit by hand.
     Regenerate with:
       cargo run -p x0ne-runtime --bin gen-action-docs > docs/actions.md
     The content comes from the `Action` enum in x0ne-actions, so this file
     cannot fall out of step with what the runtime actually accepts. -->

X0NE exposes **101 typed actions**. Every action is a JSON object with a `type` field naming the action and the fields below as its parameters:

```json
{ "type": "ReadFile", "path": "/tmp/foo.txt" }
```

The **class** column is the side-effect classification the policy engine derives every access decision from. `ReadOnly` and `Meta` actions change nothing; the rest are gated.

## Index

| Class | Actions |
|---|---|
| `Composite` | `Pipeline`, `Try`, `SafeChange`, `If`, `RenderTemplate`, `Simulate` |
| `Desktop` | `Screenshot`, `GetWindowTree`, `GetFocusedWindow`, `MoveMouse`, `Click`, `DoubleClick`, `Scroll`, `TypeText`, `PressKey`, `PressKeyCombination`, `FocusWindow`, `SendNotification`, `GetClipboard`, `SetClipboard` |
| `Destructive` | `MoveToTrash`, `RestoreSnapshot`, `DeleteSnapshot`, `StopHandle`, `TermClose`, `Forget`, `DeletePrimitive`, `DeleteFunction`, `ArtifactStop`, `ArtifactDestroy` |
| `FsWrite` | `WriteFile`, `EditFile`, `ApplyPatch`, `PatchSet`, `CreateSnapshot`, `Remember`, `DefinePrimitive`, `DefineFunction`, `RunProgram`, `ArtifactCreate`, `ArtifactSave` |
| `Meta` | `AskHuman`, `Confirm`, `AskMultipleChoice`, `Subscribe`, `CheckHandle`, `ListHandles`, `PolicyCheck`, `Validate`, `RandomBytes`, `CallPrimitive`, `ListCustomPrimitives`, `CallFunction`, `ListFunctions`, `ListActions`, `DescribeAction`, `PaintBadge` |
| `Network` | `FetchUrl`, `WebSearch`, `HttpRequest`, `SshExec`, `PortCheck`, `DnsLookup`, `LlmCall`, `OllamaApi`, `DatasetGenerate`, `OllamaChat` |
| `ProcessSpawn` | `SafeTest`, `RunCommand`, `CloudCost`, `GpuStatus`, `RunInSandbox`, `RunContainerGroup`, `SendInput`, `TermOpen`, `TermSend`, `TermResize`, `Listen`, `WatchAndReact`, `DocumentExtract`, `ArtifactServe` |
| `ReadOnly` | `ReadFile`, `ReadFiles`, `StatFiles`, `SearchAndRead`, `SearchCode`, `ReadLogs`, `Assert`, `ListSnapshots`, `TermRead`, `TermWait`, `TermSnapshot`, `TermList`, `FileInfo`, `Checksum`, `ListProcesses`, `Recall`, `ListMemory`, `Diff`, `PreviewProgram`, `ArtifactList` |

## Composite

### Pipeline

Execute multiple actions in a single request.

| Field | Type | Required |
|---|---|---|
| `steps` | `array` | yes |
| `stop_on_error` | `boolean` | no |
| `snapshot_before` | `string` | no |
| `parallel` | `boolean` | no |

### Try

Try: execute a sequence of actions with automatic snapshot + rollback on failure.

| Field | Type | Required |
|---|---|---|
| `actions` | `array` | yes |
| `rollback_on_fail` | `boolean` | no |
| `label` | `string` | no |
| `keep_snapshot` | `boolean` | no |

### SafeChange

SafeChange: transactional agent change loop.

| Field | Type | Required |
|---|---|---|
| `steps` | `array` | yes |
| `tests` | `array` | no |
| `rollback_on_fail` | `boolean` | no |
| `label` | `string` | no |
| `keep_snapshot` | `boolean` | no |

### If

Conditional execution: evaluate a check, then run either `then` or

| Field | Type | Required |
|---|---|---|
| `check` | `string` | yes |
| `path` | `string` | no |
| `value` | `any` | no |
| `host` | `string` | no |
| `port` | `integer` | no |
| `command` | `string` | no |
| `args` | `array` | no |
| `name` | `string` | no |
| `actual` | `any` | no |
| `expected` | `any` | no |
| `then` | `array` | no |
| `else_branch` | `array` | no |

### RenderTemplate

Render a string template, substituting `{{tool(args)}}` placeholders

| Field | Type | Required |
|---|---|---|
| `template` | `string` | yes |
| `max_depth` | `integer` | no |
| `strict` | `boolean` | no |

### Simulate

Simulate: dry-run an Action (or nested Pipeline / Try / SafeChange / If)

| Field | Type | Required |
|---|---|---|
| `target` | `any` | yes |
| `explain` | `boolean` | no |

## Desktop

### Screenshot

Take a screenshot of the desktop or a region.

| Field | Type | Required |
|---|---|---|
| `output` | `string` | no |
| `region` | `string` | no |

### GetWindowTree

Get the Sway window tree as a flattened list.

Takes no parameters.

### GetFocusedWindow

Get the currently focused window.

Takes no parameters.

### MoveMouse

Move the mouse pointer to an absolute position.

| Field | Type | Required |
|---|---|---|
| `x` | `integer` | yes |
| `y` | `integer` | yes |

### Click

Click a mouse button (optionally moving to x,y first).

| Field | Type | Required |
|---|---|---|
| `x` | `integer` | no |
| `y` | `integer` | no |
| `button` | `string` | no |

### DoubleClick

Double-click (optionally moving to x,y first).

| Field | Type | Required |
|---|---|---|
| `x` | `integer` | no |
| `y` | `integer` | no |

### Scroll

Scroll the mouse wheel.

| Field | Type | Required |
|---|---|---|
| `direction` | `string` | yes |
| `amount` | `integer` | no |

### TypeText

Type text at the current cursor position.

| Field | Type | Required |
|---|---|---|
| `text` | `string` | yes |

### PressKey

Press a single key by name (e.g. "Return", "Escape", "Tab").

| Field | Type | Required |
|---|---|---|
| `key` | `string` | yes |

### PressKeyCombination

Press a key combination (e.g. ["ctrl", "s"]).

| Field | Type | Required |
|---|---|---|
| `keys` | `array` | yes |

### FocusWindow

Focus a window by title or app_id substring.

| Field | Type | Required |
|---|---|---|
| `match_str` | `string` | yes |

### SendNotification

Send a desktop notification.

| Field | Type | Required |
|---|---|---|
| `title` | `string` | yes |
| `body` | `string` | yes |
| `urgency` | `string` | no |

### GetClipboard

Read the Wayland clipboard.

Takes no parameters.

### SetClipboard

Write to the Wayland clipboard.

| Field | Type | Required |
|---|---|---|
| `content` | `string` | yes |

## Destructive

### MoveToTrash

Move a file or directory to the OS trash.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |

### RestoreSnapshot

Restore all files from a named snapshot.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### DeleteSnapshot

Delete a named snapshot.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### StopHandle

Stop and cancel a running handle.

| Field | Type | Required |
|---|---|---|
| `handle` | `string` | yes |

### TermClose

Close a terminal and kill what runs in it.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### Forget

Delete a memory entry by key.

| Field | Type | Required |
|---|---|---|
| `key` | `string` | yes |
| `namespace` | `string` | no |

### DeletePrimitive

Delete a composite primitive.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### DeleteFunction

Remove a scripted function by name.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### ArtifactStop

Stop the backing service of a `Service` artifact without deleting files.

| Field | Type | Required |
|---|---|---|
| `id` | `string` | yes |

### ArtifactDestroy

Permanently remove a artifact: stop its service, delete its files, drop

| Field | Type | Required |
|---|---|---|
| `id` | `string` | yes |
| `force` | `boolean` | no |

## FsWrite

### WriteFile

Overwrite (or create) a file with new content.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `content` | `string` | yes |

### EditFile

Apply multiple edits to a single file sequentially.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `edits` | `array` | yes |
| `create_if_missing` | `boolean` | no |

### ApplyPatch

Apply a standard unified diff patch.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `patch` | `string` | yes |
| `reverse` | `boolean` | no |

### PatchSet

Apply multiple unified diffs in one action.

| Field | Type | Required |
|---|---|---|
| `patches` | `array` | yes |
| `stop_on_error` | `boolean` | no |

### CreateSnapshot

Create a snapshot of the given file paths.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `paths` | `array` | yes |

### Remember

Persist a key/value pair in project memory (~/.x0ne/memory/).

| Field | Type | Required |
|---|---|---|
| `key` | `string` | yes |
| `value` | `any` | yes |
| `namespace` | `string` | no |

### DefinePrimitive

Define a reusable composite primitive from a pipeline of existing actions.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `steps` | `array` | yes |
| `description` | `string` | no |
| `params` | `any` | no |
| `ephemeral` | `boolean` | no |

### DefineFunction

Define a scripted function. Validates the source (compile-only) and

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `source` | `string` | yes |
| `description` | `string` | no |
| `tier` | `string` | no |
| `params` | `array` | no |
| `returns` | `string` | no |

### RunProgram

Apply the program that was previewed. The runtime commits the

| Field | Type | Required |
|---|---|---|
| `source` | `string` | yes |

### ArtifactCreate

Allocate a fresh artifact — a self-contained directory served at

| Field | Type | Required |
|---|---|---|
| `name` | `string` | no |
| `kind` | `string` | no |
| `persistent` | `boolean` | no |
| `session_id` | `string` | no |

### ArtifactSave

Promote an ephemeral artifact to the persistent (`saved/`) store. The

| Field | Type | Required |
|---|---|---|
| `id` | `string` | yes |
| `name` | `string` | no |

## Meta

### AskHuman

Ask the human operator a freeform question; returns their typed answer.

| Field | Type | Required |
|---|---|---|
| `question` | `string` | yes |

### Confirm

Ask the human operator for a yes/no confirmation.

| Field | Type | Required |
|---|---|---|
| `message` | `string` | yes |

### AskMultipleChoice

Ask the human operator a multiple-choice question (QCM).

| Field | Type | Required |
|---|---|---|
| `question` | `string` | yes |
| `choices` | `array` | yes |
| `allow_skip` | `boolean` | no |
| `allow_other` | `boolean` | no |

### Subscribe

Subscribe to a machine event. Returns a handle.

| Field | Type | Required |
|---|---|---|
| `event_type` | `any` | yes |
| `config` | `any` | no |

### CheckHandle

Drain buffered events from a handle.

| Field | Type | Required |
|---|---|---|
| `handle` | `string` | yes |

### ListHandles

List all active and recent handles.

Takes no parameters.

### PolicyCheck

Check if an action would be allowed by policy (without executing it).

| Field | Type | Required |
|---|---|---|
| `action` | `any` | yes |

### Validate

Structured validation: check a value against JSON schema, regex pattern, or type.

| Field | Type | Required |
|---|---|---|
| `value` | `any` | yes |
| `check` | `string` | yes |
| `expect` | `any` | no |

### RandomBytes

Generate cryptographically random bytes.

| Field | Type | Required |
|---|---|---|
| `count` | `integer` | yes |
| `encoding` | `string` | no |

### CallPrimitive

Call a previously defined composite primitive.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `args` | `any` | no |

### ListCustomPrimitives

List all defined composite primitives.

Takes no parameters.

### CallFunction

Invoke a previously registered scripted function. Args are passed

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `args` | `array` | no |

### ListFunctions

List every defined scripted function.

Takes no parameters.

### ListActions

Enumerate the runtime's action vocabulary.

Takes no parameters.

### DescribeAction

Describe one action: its summary, its fields, and its side-effect class.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |

### PaintBadge

Orchestrator-owned UI action — accepted here ONLY so it can

| Field | Type | Required |
|---|---|---|
| `payload` | `any` | yes |

## Network

### FetchUrl

HTTP GET. HTML comes back as `format` (markdown default, text, html), 64 KiB per call; `offset` reads on; cached 15 min.

| Field | Type | Required |
|---|---|---|
| `url` | `string` | yes |
| `headers` | `any` | no |
| `format` | `string` | no |
| `timeout` | `integer` | no |
| `offset` | `integer` | no |

### WebSearch

Web search, `{title, url, snippet, source}` hits. Privacy: the query goes to Exa (mcp.exa.ai, no key sent), then Brave if keyed, then DuckDuckGo.

| Field | Type | Required |
|---|---|---|
| `query` | `string` | yes |
| `limit` | `integer` | yes |

### HttpRequest

Full HTTP request: any method, headers, body, returns status + headers + body.

| Field | Type | Required |
|---|---|---|
| `url` | `string` | yes |
| `method` | `string` | no |
| `headers` | `any` | no |
| `body` | `string` | no |
| `timeout_secs` | `integer` | no |

### SshExec

Run a single command on a remote host via the local OpenSSH client.

| Field | Type | Required |
|---|---|---|
| `host` | `string` | yes |
| `command` | `string` | yes |
| `args` | `array` | no |
| `cwd` | `string` | no |
| `env` | `any` | no |
| `timeout_secs` | `integer` | no |
| `ssh_options` | `array` | no |

### PortCheck

Check if a TCP port is open. `host` defaults to `127.0.0.1` since

| Field | Type | Required |
|---|---|---|
| `port` | `integer` | yes |
| `host` | `string` | no |
| `timeout_secs` | `integer` | no |

### DnsLookup

Resolve a hostname to its IP addresses.

| Field | Type | Required |
|---|---|---|
| `hostname` | `string` | yes |

### LlmCall

Call an LLM endpoint (Ollama or OpenAI-compatible) and return the response.

| Field | Type | Required |
|---|---|---|
| `endpoint` | `string` | yes |
| `model` | `string` | yes |
| `prompt` | `string` | yes |
| `system` | `string` | no |
| `temperature` | `integer` | no |
| `max_tokens` | `integer` | no |
| `auth` | `string` | no |

### OllamaApi

Manage Ollama models (list, pull, create, delete, show).

| Field | Type | Required |
|---|---|---|
| `endpoint` | `string` | yes |
| `operation` | `string` | yes |
| `model` | `string` | no |
| `modelfile` | `string` | no |
| `auth` | `string` | no |

### DatasetGenerate

Generate a JSONL training dataset using an LLM.

| Field | Type | Required |
|---|---|---|
| `endpoint` | `string` | yes |
| `model` | `string` | yes |
| `topic` | `string` | yes |
| `output` | `string` | yes |
| `count` | `integer` | no |
| `style` | `string` | no |

### OllamaChat

Multi-turn chat with Ollama models, with optional vision (images).

| Field | Type | Required |
|---|---|---|
| `endpoint` | `string` | yes |
| `model` | `string` | yes |
| `messages` | `array` | yes |
| `temperature` | `integer` | no |
| `max_tokens` | `integer` | no |
| `auth` | `string` | no |

## ProcessSpawn

### SafeTest

Run a typed test command without raw shell syntax.

| Field | Type | Required |
|---|---|---|
| `kind` | `string` | yes |
| `scope` | `string` | no |
| `path` | `string` | no |
| `timeout_secs` | `integer` | no |
| `filter` | `string` | no |

### RunCommand

Run a shell command (requires explicit policy allow).

| Field | Type | Required |
|---|---|---|
| `command` | `string` | yes |
| `args` | `array` | no |
| `cwd` | `string` | no |
| `env` | `any` | no |
| `timeout_secs` | `integer` | no |
| `stream` | `boolean` | no |

### CloudCost

Report the current cloud burn rate + any idle-GPU waste. Runs

| Field | Type | Required |
|---|---|---|
| `provider` | `string` | yes |
| `project` | `string` | no |

### GpuStatus

Report GPU state (util, memory, temp, attached processes). `host` None

| Field | Type | Required |
|---|---|---|
| `host` | `string` | no |

### RunInSandbox

Run arbitrary code inside an ephemeral Podman container (auto-removed after exit).

| Field | Type | Required |
|---|---|---|
| `image` | `string` | yes |
| `command` | `array` | no |
| `files` | `any` | no |
| `env` | `any` | no |
| `timeout_secs` | `integer` | no |
| `network` | `boolean` | no |
| `memory_mb` | `integer` | no |
| `cpu_limit` | `integer` | no |
| `collect_files` | `array` | no |

### RunContainerGroup

Run multiple tasks across isolated parallel Podman containers.

| Field | Type | Required |
|---|---|---|
| `tasks` | `array` | yes |
| `network` | `boolean` | no |
| `memory_mb` | `integer` | no |
| `timeout_secs` | `integer` | no |
| `max_parallel` | `integer` | no |

### SendInput

Send text input to a running streaming process's stdin.

| Field | Type | Required |
|---|---|---|
| `handle` | `string` | yes |
| `input` | `string` | yes |

### TermOpen

Open a named terminal (tmux pane, or a native PTY) running a program.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `command` | `string` | yes |
| `args` | `array` | no |
| `cwd` | `string` | no |
| `cols` | `integer` | no |
| `rows` | `integer` | no |

### TermSend

Type into a terminal: literal `text`, then named `keys`.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `text` | `string` | no |
| `keys` | `array` | no |

### TermResize

Resize a terminal.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `cols` | `integer` | yes |
| `rows` | `integer` | yes |

### Listen

Listen to a long-running command and turn its interesting lines into

| Field | Type | Required |
|---|---|---|
| `command` | `string` | yes |
| `args` | `array` | no |
| `filter` | `string` | no |
| `window_ms` | `integer` | no |
| `max_lines` | `integer` | no |
| `persistent` | `boolean` | no |

### WatchAndReact

Watch a file or directory and automatically execute an action when

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `action` | `any` | yes |
| `pattern` | `string` | no |
| `debounce_ms` | `integer` | no |
| `max_triggers` | `integer` | no |
| `recursive` | `boolean` | no |

### DocumentExtract

Extract text from a document (PDF, video, audio, image) and optionally

| Field | Type | Required |
|---|---|---|
| `input` | `string` | yes |
| `output` | `string` | yes |
| `format` | `string` | no |
| `llm_endpoint` | `string` | no |
| `llm_model` | `string` | no |
| `chunk_size` | `integer` | no |

### ArtifactServe

Attach a long-running service to a `Service` artifact and start it.

| Field | Type | Required |
|---|---|---|
| `id` | `string` | yes |
| `command` | `string` | yes |
| `port` | `integer` | no |
| `env` | `object` | no |

## ReadOnly

### ReadFile

Read the full contents of a file (UTF-8).

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `offset` | `integer` | no |
| `limit` | `integer` | no |
| `start_line` | `integer` | no |
| `limit_lines` | `integer` | no |

### ReadFiles

Read several files in one policy-checked action.

| Field | Type | Required |
|---|---|---|
| `paths` | `array` | yes |
| `max_bytes_per_file` | `integer` | no |

### StatFiles

Metadata for several files, optionally including sha256.

| Field | Type | Required |
|---|---|---|
| `paths` | `array` | yes |
| `hash` | `boolean` | no |

### SearchAndRead

Search and return surrounding context chunks in one call.

| Field | Type | Required |
|---|---|---|
| `root` | `string` | no |
| `pattern` | `string` | no |
| `patterns` | `array` | no |
| `file_glob` | `string` | no |
| `context_lines` | `integer` | no |
| `limit` | `integer` | no |

### SearchCode

Regex search inside file contents under a directory.

| Field | Type | Required |
|---|---|---|
| `root` | `string` | no |
| `pattern` | `string` | no |
| `patterns` | `array` | no |
| `file_glob` | `string` | no |
| `limit` | `integer` | no |

### ReadLogs

Read the tail of a log file.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `lines` | `integer` | no |

### Assert

Assert: verify a condition without side effects.

| Field | Type | Required |
|---|---|---|
| `check` | `string` | yes |
| `path` | `string` | no |
| `value` | `any` | no |
| `host` | `string` | no |
| `port` | `integer` | no |
| `command` | `string` | no |
| `args` | `array` | no |
| `name` | `string` | no |
| `actual` | `any` | no |
| `expected` | `any` | no |

### ListSnapshots

List all available snapshots.

Takes no parameters.

### TermRead

Read a terminal: the visible screen, or the last `history` lines.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `history` | `integer` | no |
| `diff` | `boolean` | no |

### TermWait

Wait until a regex appears, the screen is stable, or the program exits.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `pattern` | `string` | no |
| `stable_ms` | `integer` | no |
| `exit` | `boolean` | no |
| `timeout_ms` | `integer` | no |
| `diff` | `boolean` | no |

### TermSnapshot

Whole scrollback and screen of a terminal, plain or with ANSI colours.

| Field | Type | Required |
|---|---|---|
| `name` | `string` | yes |
| `ansi` | `boolean` | no |

### TermList

List the open terminals.

Takes no parameters.

### FileInfo

Return detailed metadata for a file path.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |

### Checksum

Compute a checksum of a file.

| Field | Type | Required |
|---|---|---|
| `path` | `string` | yes |
| `algorithm` | `string` | no |

### ListProcesses

List running processes (reads /proc on Linux).

| Field | Type | Required |
|---|---|---|
| `filter` | `string` | no |

### Recall

Recall a previously stored memory by exact key.

| Field | Type | Required |
|---|---|---|
| `key` | `string` | yes |
| `namespace` | `string` | no |

### ListMemory

List all stored memory keys in a namespace.

| Field | Type | Required |
|---|---|---|
| `namespace` | `string` | no |

### Diff

Structured diff: compare two strings or files and return line-by-line changes.

| Field | Type | Required |
|---|---|---|
| `a` | `string` | yes |
| `b` | `string` | yes |
| `files` | `boolean` | no |
| `context_lines` | `integer` | no |

### PreviewProgram

Preview one asm-ai program. X0NE compiles it, derives the files it can

| Field | Type | Required |
|---|---|---|
| `source` | `string` | yes |

### ArtifactList

List every known artifact (ephemeral + saved + running) with state, age,

Takes no parameters.

