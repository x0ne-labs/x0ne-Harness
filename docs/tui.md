# The TUI

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

## Side panels (Tab to cycle)

| Glyph | Panel | Purpose |
|---|---|---|
| ● | **conv** | Main conversation + tool trail (default) |
| 📋 | **queue** | Pre-typed messages waiting for the agent to free up |
| ✓ | **tasks** | Live TODO list — agent populates via `TaskAdd` / `TaskDone`, you can `/task add` too |
| ⏰ | **cron** | Scheduled reminders / self-triggers with live countdown |
| ▶ | **pipeline** | Per-step breakdown of the most-recent `Pipeline` action (status · duration · summary) |
| ❤ | **pulse** | Session heart-rate monitor — every turn / tool / snapshot / question as a coloured dot |
| ◐ | snaps · ⏳ handles · ▦ worlds · ◆ goal | Snapshots, async handles, parallel agents, persistent goals |

## Keyboard
- `Tab` cycles panels · `Shift-Tab` plan mode on/off · `^B` focus mode (hide panel) · `^E` expand all action heads
- `↑` `↓` history at textarea edges, scroll otherwise · `PgUp`/`PgDn`/`Home`/`End`/wheel for the trail
- `F1` (or `/help` · `^?`) opens a 3-column cheat sheet — keyboard · slash & panels · agent actions
- `^C` stops the running turn; idle, pressed again within 2 s, or inside a
  modal, it quits. `^D` quits
- A model that has not answered after `[ui] model_timeout_secs` (default 300,
  retries included; 0 = no bound) ends the turn; why a model call failed is
  shown in the conversation, and the next prompt is sent as usual

## Slash commands (user-driven, no LLM round-trip)
```
/task add <title>            queue a TODO  · /task done <id>  · /task clear
/cron in <secs> <label>      passive reminder
/cron in <secs> <label> -> <prompt>   self-trigger (agent receives the prompt when due)
/cron cancel <id>            cancel before it fires
/plan · /build               plan mode on · off (also in the REPL)
/help · /?                   cheat-sheet overlay
/quit · /q · :q              exit
```

## Plan mode (`/plan`, `Shift-Tab`)

`/plan` (REPL and TUI) or `Shift-Tab` (TUI) puts the session in plan mode;
`/build` (or `Shift-Tab` again) takes it out. In plan mode the session's
`x0ne-runtime` refuses every action that touches the host (writes, commands,
network, desktop, deletions), as `[policy] mode = "readonly"` does, whoever
sends it; the orchestrator refuses the same actions before any prompt, and
refuses its own sysaicalls that are not reads. The model is told in its system
prompt to read and answer with a plan, and the prompt shows `plan ❯`.

- Only you switch. No action, tool or configuration key leaves plan mode.
- "Always" approvals and yolo are suspended in plan mode, not revoked: they
  apply again after `/build`.
- Commands are refused even when they only read (`git log`), and so are
  network reads. A process started before `/plan` keeps running.
- In the TUI, a switch typed during a turn applies when the turn ends.
- `/plan` is refused while a world (`/spawn`, `SpawnWorld`) is still
  running: a world has its own runtime, which plan mode does not reach. No
  world starts in plan mode.
- A bridge with no control channel (HTTP) cannot hold plan mode: `/plan` is
  refused there and the session stays as it was.

## Hooks

Commands from your user configuration run on `PreToolUse` (they may refuse
an action), `PostToolUse`, `SessionStart` and `Stop` (they only observe), with
Claude Code's payload fields. A project file cannot set them. See
[docs/hooks.md](hooks.md).

## Agent meta-actions (LLM emits these — surfaced live in the panels)
- **`AskUser`** — multi-choice modal asking you to pick. The agent uses this whenever a decision is non-obvious. Auto-triggered when a runtime `policy_denied` arrives, so the policy gate becomes a dialog instead of a wall.
- **`TaskAdd` / `TaskDone` / `TaskClear`** — externalise the plan into the Tasks panel.
- **`CronSchedule` / `CronCancel`** — schedule a reminder or a self-trigger (the runner ticks once per second and fires due crons).
- **`PaintBadge`** — repaint the X0NE logo slot in the header with an LLM-painted ASCII frame (static or multi-frame animation).
- **`Simulate`** — dry-run any action / pipeline through the policy engine without side effects (the "what would happen?" primitive).

## Auto-AskUser on policy_denied
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

## Session jsonl — exhaustive audit trail
Every action the agent dispatches is logged to `~/.x0ne/sessions/<ts>.jsonl`, including meta-actions (`AskUser`, `TaskAdd`, `CronSchedule`, `PaintBadge`). For each invocation you get **two lines**: the request (as requested) and the full result envelope (as executed):

```jsonl
{"type":"action","action_type":"AskUser","action":{"question":"…","type":"AskUser"},"duration_ms":12340,"success":true,"ts":"…"}
{"type":"action_result","action_type":"AskUser","result":{"output":{"selected":["Find a safer way"],"custom_text":null},"success":true},"ts":"…"}
```

`jq` your way through it for replay, dataset extraction, or post-mortem.

## Live status line
A 1-row dim italic strip just above the input shows what X0NE is doing right now — `⋯ x0ne is thinking · 2.3s` while the LLM chews on the prompt, `✎ x0ne is writing · ~142 tok` during streaming, `▶ x0ne is running Pipeline · 12s` while a tool is in flight. Zero space when idle.

## Web search

```
<action>{"type":"WebSearch","query":"rust tokio mpsc broadcast","limit":10}</action>
```

Three backends, first answer wins, same envelope:
- **Exa** public MCP endpoint (`mcp.exa.ai`), no key. **The query is sent to Exa.** `X0NE_EXA_DISABLE=1` skips it.
- **Brave Search API** when `X0NE_BRAVE_API_KEY` is set
- **DuckDuckGo HTML scrape** last (no key; an anti-bot page is reported as an error, not as 0 results)

`source` names the backend that answered; `fallbacks` lists each one that failed before it. Every hit carries a citation id. Details and privacy: [docs/web-search.md](web-search.md).

```json
{"kind":"web_search_result","query":"…","source":"exa","count":8,
 "results":[{"title":"…","url":"https://…","snippet":"…","source":"S3"}, …]}
```

## Sources panel — session bibliography (📚)

Every URL the agent touches — `FetchUrl`, `BrowseWeb`, **every WebSearch result** — auto-lands in the `📚 sources` side panel with title, status, and age. The underlying jsonl already has the full request + result envelope per action; the panel is the at-a-glance summary so you can trace where each fact in the agent's reply came from. Most terminals make the URLs clickable directly.

## Shell mode (`Ctrl+S`) — X0NE becomes your shell

Press `Ctrl+S` from anywhere in the TUI to enter **shell mode**: the body switches to a real terminal trail, the input bar turns into a `$` prompt, and what you type goes to one persistent interactive shell (`$SHELL -i`, `bash` when unset) in a PTY on the host: the same trust domain as your normal shell, not the sandbox — that's what `RunCommand` is for when the LLM drives.

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

- **State persists**: it is one shell, so `cd`, variables and history carry over.
- **No per-command output cap.** The terminal keeps 1,000 lines of scrollback, which bounds memory; output is read in 64 KB chunks.
- **Not journaled.** Shell-mode commands and their output are not written to `~/.x0ne/sessions/`.

The agent does not see what you ran in shell mode; paste what it needs into your next message.

## Multimodal — `@file.png` for vision

Drop a `@*.png/.jpg/.jpeg/.webp/.gif` reference anywhere in your message and X0NE loads + base64-encodes the file and attaches it to the next LLM turn:

```
> Compare these two layouts: @~/Desktop/before.png @~/Desktop/after.png
🖼 attached 2 images for vision: before.png, after.png
```

The vision plumbing flows through `Message.images: Vec<ImageAttachment>` → routed to Anthropic's multi-part content blocks or Ollama's `images` array, depending on the active backend. Max 8 MB per image, tilde (`@~/…`) expanded, dedup, trailing punctuation stripped. PDFs are deferred for v2 (Anthropic supports them, Ollama doesn't).

## Execution Target Display

The status bar shows exactly where actions execute:

| Icon | Mode | Example |
|------|------|---------|
| 🛡 **sandbox** (green) | Incus/LXC container | `🛡 sandbox x0ne-sandbox` |
| ⚡ **local** (blue) | Direct on host | `⚡ local hp` |
| 🌐 **remote** (yellow) | HTTP API server | `🌐 remote 192.0.2.50:3100` |

Container name is auto-extracted from the `sandbox_cmd` (detects `incus exec <name>`, `ssh <host>`).

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
| `/until <cmd>` | Goal-driven loop: the agent retries until the command exits 0. The command runs as a `RunCommand` of the session, through the human gate, the policy and the sandbox, before the first turn and after each one |
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
| `Ctrl+C` | Stop the running turn; idle or twice within 2 s, quit |
