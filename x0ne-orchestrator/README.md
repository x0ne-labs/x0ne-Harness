# x0ne-orchestrator

**The AI agent loop. Connects an LLM to [x0ne-runtime](../x0ne-runtime).**

```
User input
  ↓
[Orchestrator] → builds messages → LLM (Anthropic / Ollama / LiteLLM)
  ↓
LLM response
  ↓
┌── text message?  → print to user, wait for input
│
└── <action> tag?  → send to x0ne-runtime
                    ↓
                  ActionResult JSON
                    ↓
              append to history, loop
```

---

## How it works

The orchestrator is **not** a standard LLM agent framework. It does not use tool-use / function-calling APIs. Instead, it treats the LLM as a **text generator** and applies its own parsing, validation, and security pipeline.

### Why no tool-use API?

| Standard approach | X0NE approach |
|---|---|
| Uses provider's `tools[]` / `function_calling` | Raw text + custom `<action>` tag parsing |
| Tied to one provider's format | Works with **any LLM** that produces text |
| Structured output is a black box | Full control: hallucination detection, typo correction, retry |
| Tools execute in the same process | Everything goes through a **separate process** (x0ne-runtime) via bridge |

Small local models (Mistral 7B, Qwen, DeepSeek via Ollama) often have unreliable tool-use support. All of them can produce text with XML tags.

### The agent loop

Each user message triggers an inner loop that runs until the LLM responds with plain text (no action):

```
┌─────────────────────────────────────────────────────────────────┐
│                        AGENT LOOP                               │
│                                                                 │
│  1. Telemetry sync (SystemInfo, throttled to every 30s)         │
│  2. Auto-check idle handles (>60s since last drain)             │
│  3. Build system prompt + conversation messages                 │
│  4. Stream LLM response (180s timeout)                          │
│  5. Parse response:                                             │
│     ├── Plain text → show to user, break                        │
│     ├── <action>JSON</action> → step 6                          │
│     ├── Hallucination detected → inject correction, retry       │
│     ├── Parse error → inject schema hint, retry (max 3)         │
│     └── Fabricated data → inject warning, retry                 │
│  6. Action dispatch:                                            │
│     ├── Sysaicall (Reflect/Plan) → handle in orchestrator       │
│     ├── Pipeline with sysaicall → orchestrator-first dispatch    │
│     │   (each step routed to local or x0ne-runtime bridge)          │
│     ├── SpawnWorld → tokio::spawn new agent loop + bridge        │
│     └── Core action → confirmation gate → bridge → x0ne-runtime    │
│  7. Append result to conversation, loop to step 1               │
│                                                                 │
│  Safety rails:                                                  │
│  • Max 20 consecutive actions without user input                │
│  • Max 3 parse retries before giving up                         │
│  • Max 5 identical-signature failures → loop_detected block     │
│  • Max 8 total failures per turn → hard stop                    │
│  • Destructive actions require user confirmation [y/n/a]        │
│  • Tolerant input: args/input/parameters/payload auto-unwrapped │
│  • Structural error hints (e.g. "params" wrapper detection)     │
│  • validation_error enriched with sent / expected / diff        │
└─────────────────────────────────────────────────────────────────┘
```

### Response parsing

The parser (`parser.rs`) handles multiple scenarios since LLMs are unpredictable:

1. **Tag extraction** — Find `<action>{...}</action>` in the response
2. **Bare JSON recovery** — If no tags, try to find a raw `{"type": "..."}` JSON object
3. **Think block stripping** — Remove `<think>...</think>` blocks from reasoning models (DeepSeek, Qwen)
4. **Hallucination detection** — Detect when the LLM fabricates system data (fake directory listings, process tables, etc.) instead of using a real action
5. **Typo suggestion** — If the action type is unknown (e.g. `"ReadFille"`), suggest the closest match via Levenshtein distance
6. **Schema hints** — On parse error, inject the correct schema for the action type so the LLM can self-correct
7. **Structural hints** — Detect common mistakes like wrapping fields inside a `"params"` object and inject explicit correction
8. **Loop breaker** — Two-tier protection against stuck LLMs:
   - **Per-signature circuit breaker** (`loop_tracker.rs`): after 5 consecutive failures of the same `(action_type, normalized_args, error_code)` tuple, the orchestrator **blocks** the next dispatch and returns a `loop_detected` envelope (`retryable: false`) framed as `policy_denied` so the LLM is told *"do NOT retry, change your approach"*. Counter resets on success or whenever args / error code change.
   - **Hard total-failure cap**: after 8 total failures regardless of action type, the loop hard-stops.

### Sysaicalls

Meta-actions handled by the orchestrator itself, not forwarded to x0ne-runtime:

| Sysaicall | Description |
|---|---|
| `Plan` | Propose a multi-step plan for user approval before execution |
| `SpawnWorld` | Spawn a sub-agent with its own conversation, LLM context, and x0ne-runtime bridge |
| `WaitAgent` | Wait for a sub-agent to finish and collect its result + token usage |
| `ListAgents` | List active sub-agents and their status (running/completed/failed) |
| `BroadcastAgents` | Append a message to one or more sub-agent inboxes (optional `targets: [...]`; receipt reports `requested_targets`, `resolved_targets`, `delivered_to`, `failed_targets[{id, reason}]` with `not_found` / `agent_completed` / `agent_failed` reasons) |
| `Reflect` | Self-evaluation over the conversation context |

> **Note:** `Try` and `Assert` were migrated to native x0ne-runtime primitives in v0.2. They now execute through the policy engine like all other actions.

#### Orchestrator-First Dispatch

When the LLM sends a `Pipeline` containing any sysaicall (SpawnWorld, WaitAgent, etc.), the orchestrator **intercepts** the entire Pipeline and executes it locally — routing each step to either orchestrator-local handling or the x0ne-runtime bridge. Pure-system Pipelines are forwarded to x0ne-runtime directly (zero overhead).

```
Pipeline { SpawnWorld, ReadFile, WaitAgent }
    │
    ├─ step 1: SpawnWorld → orchestrator (tokio::spawn + dedicated bridge)
    ├─ step 2: ReadFile   → x0ne-runtime bridge (standard dispatch)
    └─ step 3: WaitAgent  → orchestrator (await oneshot)
```

This enables composable multi-agent workflows in a single Pipeline:

```json
<action>
{"type": "Pipeline", "steps": [
  {"type": "SpawnWorld", "id": "ui",    "task": "Design a TUI dashboard for BTC price"},
  {"type": "SpawnWorld", "id": "api",   "task": "Implement CoinGecko API client"},
  {"type": "WaitAgent",  "id": "ui",    "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "api",   "timeout_secs": 120}
]}
</action>
```

**Safety boundaries:**
- Sub-agents **cannot spawn further agents** (enforced at dispatch: registry=None)
- Each sub-agent has its own action budget (default: 10 via `X0NE_SERVE_SUB_AGENT_MAX_ACTIONS`)
- The LLM backend is `Arc`-shared between parent and sub-agents (single connection pool)
- Interrupt propagation: Ctrl+C stops all in-flight sub-agents

See [docs/actions.md](../docs/actions.md#sysaicalls-orchestrator) for details.

### Confirmation gate

Actions that modify the filesystem or run commands are **destructive** and require user confirmation:

```
  ┌─ WriteFile ./src/main.rs (2048 bytes)
  │  Preview: fn main() { println!("hello"); ...
  └─ [y]es  [n]o  [a]lways
```

Pressing `a` enables auto-confirm for the rest of the session. The full list of confirmed actions: `WriteFile`, `AppendFile`, `ReplaceInFile`, `EditFile`, `InsertAtLine`, `ApplyPatch`, `CreateDir`, `MoveToTrash`, `CopyFile`, `MoveFile`, `RunCommand`, `RunInSandbox`, `RestoreSnapshot`, `DeleteSnapshot`.

### Security model

The orchestrator **never executes anything on the host directly**. Every action goes through:

```
Orchestrator → Bridge (stdio/HTTP) → x0ne-runtime → Policy Engine → Executor
```

**Sandbox mode** adds a dual-bridge architecture:

```
Orchestrator (host)
├── Primary bridge → incus exec sandbox → x0ne-runtime A (all standard actions)
└── Incus bridge   → local x0ne-runtime B  (RunIncus/RunIncusGroup only)
```

The orchestrator intercepts `RunIncus`/`RunIncusGroup` and routes them to the host-side bridge. All other actions go to the sandbox. The host-side x0ne-runtime only receives Incus actions — no file or command access to the host.

The policy engine in x0ne-runtime enforces:
- **Path restrictions** — `allowed_read_paths`, `allowed_write_paths`, `deny_path_patterns`
- **Command whitelist** — `allowed_commands` (no shell access)
- **Network restrictions** — URL/host allowlists
- **Environment variable whitelist** — `readable_env_vars`
- **Desktop action gating** — each desktop action individually enabled

Even if the LLM is compromised or hallucinates a dangerous action, x0ne-runtime denies it. The orchestrator surfaces the denial to the LLM with context so it can adapt.

### Dynamic prompt generation

On startup, the orchestrator calls `ListActions` on x0ne-runtime to discover all available actions and their schemas. The system prompt is built dynamically based on:
- Which actions are enabled by policy
- Which provider/model is active (compact vs full prompt)
- Current project context (`.x0ne-project` file if present)
- Active handles and their status

---


## Quick start

```bash
# Stdio mode (default — x0ne-runtime spawned as subprocess)
cargo run

# HTTP mode (connect to a running x0ne-runtime server)
cargo run -- --mode http

# Choose provider at startup
cargo run -- --provider ollama
cargo run -- --provider openai-compat

# Override log level
X0NE_LOG=debug cargo run
```

On first run with no API key in the environment, an interactive onboarding prompt will ask you to choose a provider and enter credentials.

---

## The TUI

Launch with `x0ne --tui` (or `make run-tui` from the workspace root). One screen, one flag: `--tui2` named this same TUI and was removed on 2026-08-16. What it carries:

- **9 cyclable side panels** (Tab / Shift-Tab) — `conv` · `queue` · `tasks` · `cron` · `pipeline` · `pulse` · `snaps` · `handles` · `worlds` · `goal`. Each is bound to live state mutated by both the agent and the user.
- **`AskUser` meta-action** — agent emits `{"type":"AskUser","question":"…","options":[…],"multi_select":false}` to pop a multiple-choice modal. The user's answer lands in the tool-result slot of the next turn.
- **Auto-AskUser on `policy_denied`** — when the runtime rejects a gated action (RunInSandbox, ArtifactCreate, …), the TUI automatically pops a 2-option modal ("Find a safer way" / "Stop and explain") instead of bouncing a red error envelope back to the LLM. Turns the policy gate into a dialog.
- **Tasks panel** — `TaskAdd { title }` / `TaskDone { id }` / `TaskClear` externalise the agent's plan. User-side `/task add` / `/task done <id>` / `/task clear` mirror these.
- **Cron panel** — `CronSchedule { label, in_secs|at_unix, prompt? }` schedules reminders or self-triggers (the runner ticks 1Hz and fires due crons; with `prompt` set, the cron self-injects a UserInput). `/cron in <secs> <label> [-> <prompt>]` from the user side.
- **Pipeline panel** — sticky per-step breakdown (status · duration · 1-line summary) of the most-recent Pipeline action, auto-parsed from the result envelope.
- **Pulse panel** — live session heart-rate: every turn / tool / snapshot / ask / goal / task / cron event becomes a coloured dot + label + age. Auto-derived from `AgentEvent`s.
- **Live status row** — 1-row dim italic strip above the input showing `⋯ x0ne is thinking · 2.3s` / `✎ x0ne is writing · ~142 tok` / `▶ x0ne is running Pipeline · 12s`. Zero space when idle.
- **Help overlay** — `F1` / `/help` / `Ctrl-?` open a centred cheat sheet (keyboard · slash & panels · agent actions). Any key dismisses.
- **Exhaustive jsonl audit** — every action (bridge actions, meta-actions, AskUser questions + user answers, PaintBadge frames, policy-denied envelopes) is persisted to `~/.x0ne/sessions/<ts>.jsonl` as paired `type:"action"` + `type:"action_result"` lines for replay / dataset extraction.

---

## Chat bridges — Telegram + Matrix

Two transports, same agent loop. Choose based on trust model:

| | Telegram | Matrix |
|---|---|---|
| Setup | 30s | ~5min |
| Self-host | No | Yes (Conduit/Dendrite/Synapse) |
| E2EE | Opt-in | Default |
| AskUser UX | Inline keyboard | Numbered options + reaction emojis |

Both run as headless processes (`x0ne --telegram` / `x0ne --mx`), reusing the exact same agent loop the TUI uses.

### Telegram bridge — drive X0NE from your phone

Two CLI flags wire the same agent loop to a Telegram bot:

- **`x0ne --telegram-setup`** — one-time pairing. Prompts for the bot token (saved mode 0600 to `~/.x0ne/telegram.toml`), then renders a QR code in the terminal pointing at `https://t.me/<bot>?start=<8-byte-token>`. Scan with the camera → Telegram auto-sends `/start <token>` → X0NE captures + whitelists your `chat_id`. 5-min TTL, single use.

- **`x0ne --telegram`** — headless bridge. Inbound chat messages become `UserInput`s; assistant turns get sent back (markdown, capped at 3800 chars). Tool calls surface as one-liners (`▶ ReadFile · /etc/hostname` then `✓ · 3ms · 3B`). **`AskUser` modals render as inline-keyboard buttons** — one tap on mobile sends the answer back as `TuiCommand::AskAnswer`. Only whitelisted chat_ids reach the agent.

Direct `reqwest` calls to the Bot API (no `teloxide` dep — keeps the build lean). Module: `x0ne_orchestrator::telegram`.

### Matrix bridge — open-protocol counterpart

- **`x0ne --mx-setup`** — interactive pairing. Prompts for homeserver URL, bot user, bot password (rpassword); logs in via `m.login.password`, persists access token + bot user id to `~/.x0ne/matrix.toml` (mode 0600). Then renders a QR pointing at `https://matrix.to/#/<bot>` — scan with the camera, Element opens the bot profile, tap "Start chat", X0NE captures your Matrix ID via `/sync` and whitelists it.

- **`x0ne --mx`** — headless bridge. Long-polls `/sync`, forwards whitelisted senders' messages as `UserInput`s, sends agent replies back as `m.room.message` with `org.matrix.custom.html` formatting. Tool calls one-line. **`AskUser` modals render as numbered options + reaction emojis** (`1️⃣ 2️⃣ 3️⃣ …`) attached as reactions to X0NE's own message — tap to pick, X0NE listens to `m.reaction` events and dispatches the matching `TuiCommand::AskAnswer`.

- **Lean deps** — direct `reqwest` calls to the Client-Server API (5 endpoints: `/login`, `/sync`, `send/m.room.message`, `send/m.reaction`, `join`). No `matrix-rust-sdk` (which brings ~80 sub-deps for features we don't need yet — E2EE state machine, SAS, sliding sync). Module: `x0ne_orchestrator::matrix`.

Flag is spelled `--mx` rather than `--matrix` because `--matrix` is already taken by an unrelated LLM matrix-mode feature.

---

## Supported LLM backends

| Provider | Config section | Auth | Vision |
|---|---|---|---|
| Anthropic (Claude) | `[llm.anthropic]` | `ANTHROPIC_API_KEY` | ✅ multi-part content blocks |
| Ollama (local) | `[llm.ollama]` | none (optional `OLLAMA_API_KEY`) | ✅ via `images: [base64...]` (llava, bakllava, moondream, minicpm-v) |
| Remote Ollama | `[llm.ollama]` + `OLLAMA_BASE_URL` | optional Bearer token | ✅ same as local |
| LiteLLM proxy | `[llm.openai_compat]` | optional | ✅ data-URL image blocks |
| vLLM / LocalAI | `[llm.openai_compat]` | optional | ✅ data-URL image blocks |
| Groq / OpenRouter | `[llm.openai_compat]` | API key | ✅ data-URL image blocks |

The same `Message` struct (with optional `images: Vec<ImageAttachment>`) is converted into each backend's native wire format by three pure converters in `api.rs`:

- `message_to_anthropic_value()` — Anthropic `[{type:"text"}, {type:"image", source:{type:"base64", ...}}]`
- `message_to_ollama_value()` — Ollama `{role, content, images: [<base64>, ...]}`
- `message_to_openai_value()` — OpenAI `{role, content: [{type:"text"}, {type:"image_url", image_url:{url:"data:<mime>;base64,..."}}]}`

Wire-format parity across the 3 backends is locked down by `tests/integration_multi_model.rs`.

### Anthropic prompt caching

When the active backend is Anthropic, the orchestrator automatically marks the **last user message** with a `cache_control: ephemeral` breakpoint (`attach_cache_to_last_user()` in `api.rs`). Every subsequent turn that shares the same prefix becomes a cache *read* instead of a fresh tokenization — typically **2× faster and ≈ 90 % cheaper** after the first call.

`UsageTracker` reports cache traffic separately:

```
Requests: 12  |  Input: 45 320 tok (cache read 38 200 / write 4 000)  |  Output: 8 900 tok  |  Cost: $0.27  |  Model: anthropic/claude-sonnet-4
```

Disable per-session via `--no-cache` if you need fresh prompts for benchmarking.

---

## Slash commands

| Command | Description |
|---|---|
| `/help` | Show all commands |
| `/history` | Print conversation history |
| `/model` | Show active model |
| `/cost` | Show cumulative token usage and estimated cost |
| `/handles` | List all active streaming handles |
| `/check <id>` | Manually check a handle (drain buffered output) |
| `/stop <id>` | Manually stop a handle |
| `/snapshot <name>` | Quick create snapshot of current working directory |
| `/restore <name>` | Quick restore a named snapshot |
| `/actions` | List all available x0ne-runtime actions |
| `/policy <json>` | Dry-run policy check without executing the action |
| `/clear` | Clear conversation history |
| `/export [name]` | Export session log (JSON for audit & fine-tuning) |
| `/matrix [request]` | Enter Matrix Mode (LLM-controlled artifact) |
| `/auto` | Toggle auto-confirm |
| `/compact` | Toggle compact prompt |
| `/until <cmd>` | Goal-driven loop — agent retries until cmd passes (exit 0) |
| `/quit` | Exit (`/exit` or `/q` also work) |

### Parallel Worlds

| Command | Description |
|---|---|
| `/worlds` | Live split-pane view of running sub-agents |
| `/spawn [N] [msg]` | Seed N synthetic parallel worlds (demo, default 4) |
| `/worlds-reset` | Clear the worlds registry |

---

## v0.2 capabilities

The orchestrator is aware of all x0ne-runtime v0.2 features and teaches the LLM to use them:

### Action Pipelines

The LLM can batch multiple actions in one round-trip:

```json
<action>
{
  "type": "Pipeline",
  "steps": [
    {"type": "ReadFile", "path": "./Cargo.toml"},
    {"type": "ReadFile", "path": "./src/main.rs"},
    {"type": "SearchCode", "root": "./src", "pattern": "TODO"}
  ],
  "stop_on_error": true,
  "snapshot_before": "pre-refactor"
}
</action>
```

Pipeline results are displayed compactly in the terminal — one summary line per step. Full output is shown only for failed steps to keep the context clean.

### Streaming handles

Long-running commands and event subscriptions return a **handle** instead of blocking:

```json
{"type": "RunCommand", "command": "cargo", "args": ["test"], "stream": true}
→ handle "proc_abc12345"
```

The orchestrator:
- Tracks all active handles automatically
- Auto-checks handles idle for > 60 seconds before the next LLM call
- Injects a handle summary into the context so the LLM stays aware
- Removes handles when they complete / are stopped
- Stops all running handles gracefully on `/quit`

Use `/handles` to see the current table. Use `/check <id>` and `/stop <id>` to manage them manually.

### Event subscriptions

```json
{
  "type": "Subscribe",
  "event_type": {"kind": "file_changed", "path": "./src", "recursive": true}
}
```

```json
{
  "type": "Subscribe",
  "event_type": {"kind": "port_open", "host": "127.0.0.1", "port": 8080}
}
```

### File snapshots

The LLM is instructed to snapshot before risky changes:

```json
{"type": "CreateSnapshot", "name": "before-refactor", "paths": ["./src/main.rs"]}
{"type": "RestoreSnapshot", "name": "before-refactor"}
{"type": "ListSnapshots"}
```

You can also create/restore snapshots directly from the terminal:
```
/snapshot before-big-change
/restore before-big-change
```

### Capability discovery

The LLM can always introspect what it can do:

```json
{"type": "ListActions"}
{"type": "DescribeAction", "action_type": "Pipeline"}
{"type": "PolicyCheck", "action": {"type": "WriteFile", "path": "/etc/passwd", "content": "bad"}}
```

Use `/actions` and `/policy <json>` for quick terminal access.

### Desktop / Wayland (opt-in)

Enable with `X0NE_DESKTOP=1`:

```bash
X0NE_DESKTOP=1 cargo run
```

Requires `desktop.enabled = true` in `x0ne-runtime.toml` and the external binaries (`grim`, `wlrctl`, `wtype`, `swaymsg`, `wl-copy`/`wl-paste`, `notify-send`).

---

## Token usage & cost tracking

After each LLM call, token usage is shown inline (configurable via `display.show_token_count`):

```
  tokens: +350in +120out
```

Use `/cost` to see cumulative usage:

```
  [Token usage & cost]
  Requests: 12  |  Input: 45320 tok  |  Output: 8900 tok  |  Cost: $0.2696  |  Model: anthropic/claude-sonnet-4-20250514
```

Claude models are billed at Anthropic's list prices, built in
(`src/agent/pricing.rs`, read on
<https://platform.claude.com/docs/en/about-claude/pricing>). `[cost]` prices
GPT-4o, GPT-4o mini, and a Sonnet or Haiku name that list does not carry:

```toml
[cost]
sonnet_input_per_m = 3.0
sonnet_output_per_m = 15.0
```

For Ollama, tokens are tracked but cost shows $0.00. A model with no known
price shows `unknown`, never a guessed figure.

---

## Context management

When the conversation approaches 80% of `max_context_tokens`, the orchestrator will trigger summarization:
1. Calls the LLM with the oldest 60% of messages + a summarization prompt
2. Replaces those messages with a single `[Conversation summary]` block
3. Keeps the most recent 40% of messages intact

This keeps the context window healthy for long sessions without losing important history.

Pipeline results are compacted in the conversation history (summaries only, full output for failures) to avoid bloating the context with large step outputs.

---

## Artifact — browser-reachable web artifacts

When an agent writes HTML/CSS/JS that a human wants to *open* (mockup, dashboard, live ticker, mini-game), the orchestrator embeds a **loopback reverse-proxy** on `127.0.0.1:7800` to make the artifact reachable at `http://localhost:7800/c/<artifact_id>/` on the user's machine — without ever opening a port on the network.

### Data path (sandbox mode)

```
browser ──► 127.0.0.1:7800 ──► x0ne-orchestrator ──► <runtime> exec/pull ──► tenant container
          (loopback only)      (this process)        (authenticated CLI)     (sandboxed)
```

`<runtime>` is **auto-detected** from `[bridge] sandbox_cmd`:

| First token of `sandbox_cmd` | Backend used | File read command |
|---|---|---|
| `incus exec …`  | Incus  | `incus file pull <target><path> -` |
| `podman exec …` | Podman | `podman exec <target> sh -c 'cat -- <path>'` |
| `docker exec …` | Docker | `docker exec <target> sh -c 'cat -- <path>'` |

No files are copied to the host, no proxy device is created, no firewall rule is touched. When the `x0ne` process exits, the port dies with it — there is no orphan state.

> **Why not `podman cp` / `docker cp`?** Because `cp <container>:<path> -` emits a *tar archive* on stdout rather than raw bytes; `exec sh -c 'cat --'` keeps the transport symmetric across the three backends and gives us a real non-zero exit code when the file is missing.

### The three steps a model takes

```text
1. ArtifactCreate { name?: string, kind?: "static" | "service" }
   → returns { artifact_id, root_path, url }
2. WriteFile { path: root_path/index.html, content: "<!DOCTYPE html>…" }
3. ArtifactExpose { artifact_id, ttl_secs?: integer }
   → returns { url: "http://127.0.0.1:7800/c/<id>/" } ← this is the one to show the user
```

The URL from `ArtifactExpose` is the one the browser can reach. `ArtifactCreate`'s URL field is an *in-container* address and should not be handed to the user in sandbox mode — the system prompt teaches the LLM this rule.

### Primitives

| Primitive | Lives in | What it does |
|---|---|---|
| `ArtifactCreate` | x0ne-runtime | Allocate a artifact directory, register it in the tenant's `artifacts/registry.json` |
| `WriteFile` | x0ne-runtime | Write files under `root_path` (same as any other file write) |
| `ArtifactExpose` | **orchestrator** (host-side) | Register artifact with the local proxy, return a loopback URL |
| `ArtifactUnexpose` | **orchestrator** (host-side) | Remove one artifact (or all) from the local proxy |
| `ArtifactList`, `ArtifactSave`, `ArtifactStop`, `ArtifactDestroy`, `ArtifactServe` | x0ne-runtime | Lifecycle |

`ArtifactExpose` / `ArtifactUnexpose` are host-side primitives — they execute in `x0ne-orchestrator`, not inside the sandbox. The agent declares intent; the orchestrator owns policy. Same pattern as `RunIncus` / `RunIncusGroup`.

### Policy (today, intentionally narrow)

- `visibility` is `host` only (127.0.0.1 loopback). `lan` and `public` return a clean error pending the edge-nginx iteration.
- `ttl_secs` — optional, lazy-evicted from the registry on access. No background timer, no orphan state.
- The router answers a browser on this machine only: `GET` and `HEAD`, a loopback `Host` with its port, a loopback `Origin` when the browser sends one, and no cross-site embedding (`Sec-Fetch-Site: cross-site` passes for a top-level navigation only). Anything else gets 403, another method 405, and nothing of it reaches the runtime. Request and response headers cross through allowlists (no cookie, no credential). Every answer carries `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cross-Origin-Resource-Policy: same-origin`, `Content-Security-Policy: frame-ancestors 'none'` and `Cache-Control: no-store`. `/healthz` reports a count of registered artifacts, not their ids.
- Path traversal (`..` segments) is rejected with 400; a path that canonicalises outside the artifact root (a symlink placed in the artifact) with 403. MIME types are derived from extensions.
- The proxy registry is in-memory only — no persistence, no disk writes. A restart of `x0ne` = clean slate.
- Absolute paths from artifact registries are POSIX-single-quoted before being injected into the `sh -c 'cat -- <path>'` wrapper (see `container_runtime::shell_single_quote`), so a pathological `root_path` value cannot escape into shell execution.

### Container runtime matrix

The artifact subsystem uses a small `ContainerRuntime` trait (`x0ne-orchestrator/src/container_runtime.rs`) with three concrete backends. `detect_from_sandbox_cmd` picks the right one from the configured bridge; action results surface the choice as `"backend": "incus" | "podman" | "docker"` so it shows up in session logs.

Example sandbox_cmd shapes that auto-detect correctly:

```text
incus  exec --env X0NE_ALLOW_LOOPBACK=1 prod-infra:x0ne-sandbox -- /opt/x0ne-runtime --mode stdio
podman exec -it --user root x0ne-sandbox /opt/x0ne-runtime --mode stdio
docker exec --user root worker-42 /opt/x0ne-runtime --mode stdio
```

If the first bare token is anything else (`ssh`, `kubectl`, …), `ArtifactExpose` returns a clean `"ArtifactExpose requires sandbox mode"` error.

### Why loopback (not an `incus proxy` device on the tenant host)

The previous iteration used `incus config device add … proxy listen=tcp:0.0.0.0:7800 …` to expose the port on the Incus host itself. That leaked LAN topology into the agent prompt (the model needed to know the tenant-host IP to produce a URL), collided across users on a shared Incus host, and was wiped every time `make run-incus` restored the baseline snapshot. The loopback model fixes all three: each user's `x0ne` has its own `127.0.0.1:7800`, the URL the LLM naively produces is now correct, and there's nothing to clean up.

---

## HTTP Serve Mode

The orchestrator can run as a headless HTTP/SSE server, exposing the agent loop to web frontends:

```bash
x0ne --serve                          # default bind 127.0.0.1:7700
x0ne --serve --bind 0.0.0.0:7700      # public (token and allowed hosts required)
x0ne --serve --serve-token SECRET      # bearer token (X0NE_SERVE_TOKEN)
```

A bearer token is always required: without `--serve-token`, one is
generated into `$XDG_RUNTIME_DIR/x0ne/serve-<port>.token` (else
`~/.x0ne/run/`), mode 0600, and its path is printed on stderr. Requests are
refused (403) when `Host` is not `127.0.0.1` / `localhost` / `[::1]` with the
bound port or an entry of `X0NE_SERVE_ALLOWED_HOSTS`, and when they carry an
`Origin` (or `Sec-Fetch-Site` other than `none`) not listed in
`X0NE_SERVE_CORS_ORIGINS` (empty by default). The runtime's own token is read
from `X0NE_CORE_TOKEN`, else from the file a loopback runtime generated.

> Requires the `serve` feature (default: on). Build with `--no-default-features` to strip it.

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Liveness only (`{"ok": …}`) |
| `GET` | `/info` | Public metadata (provider, model, available actions) |
| `POST` | `/prompt` | SSE stream of tokens / actions / results |

### POST /prompt body

```json
{
  "prompt": "list files in current dir",
  "history": [
    {"role": "user", "content": "..."},
    {"role": "assistant", "content": "..."}
  ],
  "provider": "ollama",
  "model": "qwen2.5",
  "base_url": "http://...:11434",
  "max_actions": 10
}
```

`provider` must be a built-in provider or a saved compat profile name;
`base_url` must equal an endpoint the operator configured (else 400).

### SSE event types

| Event | Payload |
|-------|--------|
| `meta` | `{ provider, model, x0ne_core, session }` |
| `token` | `{ text }` (LLM streamed chunk) |
| `action` | `{ reasoning, action }` (before execution) |
| `result` | `{ result, duration_ms }` (x0ne-runtime ActionResult) |
| `done` | `{ text, usage, actions }` |
| `error` | `{ message, kind }` |

### Session isolation

Each chat session is identified by an `X-X0ne-Session` header. Sessions are isolated:
- Separate `Conversation` history
- Separate `AgentRegistry` (parallel worlds are per-session)
- LRU eviction with configurable TTL (default: 30 min, max: 5000 sessions)

### Environment configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `X0NE_SERVE_MAX_ACTIONS` | 20 | Hard cap on actions per request |
| `X0NE_SERVE_SESSION_TTL_SECS` | 1800 | Session idle timeout |
| `X0NE_SERVE_MAX_SESSIONS` | 5000 | Max concurrent sessions |
| `X0NE_SERVE_MAX_AGENTS_PER_SESSION` | 4 | Max sub-agents per session |
| `X0NE_SERVE_SUB_AGENT_MAX_ACTIONS` | 10 | Max actions per sub-agent |

---

## Staged Ctrl+C

| Press | Behavior |
|-------|----------|
| First Ctrl+C | Gracefully stops current LLM streaming + cancels in-flight actions. Injects a system message. |
| Second Ctrl+C (within 1s) | Quits the application. |

Works uniformly in the REPL, autonomous loop, and during action execution.

---

## Input Queue

Users can type while the agent is processing. Pending lines are buffered via `libc::poll(2)` with zero timeout and replayed on the next turn.

---

## Self-Test (debug builds only)

```bash
x0ne --self-test
```

Visual rendering self-test that boots x0ne-runtime and runs every action category through the display pipeline. No LLM required — tests the full CLI rendering.

---

## Configuration

See [x0ne-orchestrator.toml.example](x0ne-orchestrator.toml.example) for the full reference config.

Key sections:

```toml
[llm]
provider = "anthropic"   # or "ollama" or "openai-compat"

[bridge]
mode = "stdio"           # or "http"
x0ne_core_bin = "x0ne-runtime"
health_check_timeout_secs = 5

[conversation]
max_context_tokens = 100000
summarize_at_percent = 80
keep_recent_percent = 40

[handles]
auto_check_interval_secs = 60

[display]
pipeline_max_output_lines = 10
show_token_count = true

[cost]
sonnet_input_per_m = 3.0
sonnet_output_per_m = 15.0
```

---

## Architecture

```
x0ne-orchestrator/
├── Cargo.toml
├── x0ne-orchestrator.toml.example  ← reference config
└── src/
    ├── main.rs               ← CLI entry, REPL loop, self-test harness
    ├── agent_loop.rs         ← inner agent loop (LLM → parse → dispatch → retry)
    │                            Orchestrator-first Pipeline interception
    ├── config.rs             ← all config structs with serde defaults + env overrides
    ├── api.rs                ← LLM backends (Anthropic, Ollama, OpenAI-compat)
    ├── bridge.rs             ← stdio + HTTP bridge to x0ne-runtime, health check
    ├── conversation.rs       ← message history, handle tracking, context management
    ├── parser.rs             ← <action> tag extraction, hallucination detection, typo suggestion
    ├── prompt.rs             ← dynamic system prompt (v0.2, 107 actions, compact/full modes)
    ├── sysaicalls.rs         ← SpawnWorld / WaitAgent / ListAgents / BroadcastAgents dispatch
    │                            AgentRegistry, AgentResult, execute_spawn_world
    ├── subagent.rs           ← parallel isolated agent tasks (World-based)
    ├── display.rs            ← action box rendering, result formatting, previews
    ├── terminal.rs           ← terminal helpers, confirmation prompts, banners
    ├── session.rs            ← JSONL session logging for audit/replay
    ├── pulse.rs              ← /pulse system health dashboard
    ├── slash.rs              ← slash command dispatch (/help, /model, /cost, etc.)
    ├── sysmon.rs             ← system monitoring state (CPU, RAM, disk)
    ├── project.rs            ← X0NE.md, @file injection, project detection, codebase indexing
    ├── setup.rs              ← first-run onboarding wizard
    ├── llm_setup.rs          ← interactive LLM provider setup
    ├── interrupt.rs          ← staged Ctrl+C (graceful → quit)
    ├── input_queue.rs        ← non-blocking stdin drain (libc::poll)
    ├── latex.rs              ← LaTeX-to-Unicode conversion for terminal rendering
    ├── matrix_mode.rs        ← LLM-controlled terminal artifact
    ├── tui_mode.rs           ← full TUI mode (ratatui dashboard)
    ├── serve.rs              ← HTTP/SSE serve mode (web API)
    ├── session_registry.rs   ← per-chat session isolation (LRU + eviction)
    ├── worlds_registry.rs    ← parallel worlds pub/sub + replay
    ├── worlds_view.rs        ← parallel worlds rendering (ANSI + ratatui)
    ├── worlds_cli.rs         ← /worlds, /spawn, /worlds-reset handlers
    ├── sticky_header.rs      ← opt-in VT100 sticky banner
    ├── app.rs                ← TUI application state
    ├── tui.rs                ← TUI terminal init/restore
    ├── ui.rs                 ← TUI widget rendering
    ├── cli_helper.rs         ← rustyline completion/hints
    └── lib.rs                ← module declarations
```

### Communication flow

**Stdio mode** (default):
```
orchestrator → spawn x0ne-runtime → write JSON line to stdin → read JSON line from stdout
```

**HTTP mode**:
```
orchestrator → POST /execute → x0ne-runtime HTTP server → JSON response
```

Health check runs on startup:
- **stdio**: sends `ListActions`, verifies response
- **http**: GET `/health` with configurable timeout

---

## Building

```bash
cargo build --release
cargo test
cargo clippy
```

---

## Testing

The orchestrator ships four integration suites pinning the LLM-facing
contracts. All run offline (no network, no live LLM) and complete in seconds.

| Suite | File | What it pins |
|---|---|---|
| **Test A — GOSIM demo scenario** | `tests/integration_demo.rs` | Boot context (sandbox-state warning), pipeline tally, broadcast receipts, `ActionStats.by_source`, `ListActions` merge with `source` tags, error-class mapping. Covers T0.5 + T1–T8. |
| **Test B — Mistral 7B classroom-survival** | `tests/integration_classroom.rs` | `[ACTION]` markers, lenient field-wrapper unwrapping (`args` / `input` / `parameters` / `arguments` / `payload`), `validation_error` enrichment with diff. Covers T9 + T10. + 1 ignored `mistral_recovery` test you can run live with `OLLAMA_HOST=… cargo test … -- --ignored`. |
| **Loop circuit breaker** | `tests/integration_loop_breaker.rs` | T11 — `RepeatedActionTracker` blocks the 6th identical-signature failure, framed as `policy_denied` with `retryable: false`. |
| **Multi-model wire-format parity** | `tests/integration_multi_model.rs` | Cross-backend serialization of `Message` (text-only and vision) through the three `message_to_{anthropic,ollama,openai}_value` converters; screenshot helper round-trip; `Message` history serde survives a save→load cycle without losing image data. |

Run them all:

```bash
cargo test -p x0ne-orchestrator --tests
```

Run a single suite:

```bash
cargo test -p x0ne-orchestrator --test integration_multi_model
```

When you fix a new LLM-UX issue, add a red test to whichever suite matches the
scenario (Mistral-class confabulation → `integration_classroom`, demo path →
`integration_demo`, backend wire-format → `integration_multi_model`,
runaway-loop → `integration_loop_breaker`) and make it green in the same commit.

---

## License

Apache-2.0
