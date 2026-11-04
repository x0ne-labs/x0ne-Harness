# Modes of operation

Besides the TUI and the REPL, `x0ne` runs headless, autonomously, or as a terminal canvas driven by the model; and it reads a project memory file.

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

**Exit status.** `0` when the turn completed; `2` when it ended any other
way — an action denied, budget or deadline reached, loop detected, model
failure — with the reason on stderr (`x0ne: turn ended without completing
(denied): …`); `1` on a startup error (no runtime, bad config). The JSON
envelope is unchanged (`response`, `tokens`, `model`).

**Writes in `-p`.** With stdin or stdout piped there is no one to answer the
approval prompt, so every action that needs one is **denied** and the turn
ends with exit `2` — `[ui] auto_confirm = true` does not change that, and
`x0ne -p` says so at startup. To let a headless run write, declare the
directories under `[trust] roots` in your user config (see [Trust roots](policy.md#trust-roots-writing-without-the-prompt)); the runtime's `allowed_write_paths` must cover them too.

## Autonomous Mode

Continuous agent loop, the LLM works independently with auto git checkpoints:

```bash
x0ne --auto-mode                          # default 50 iterations
x0ne --auto-mode --max-iterations 100     # custom limit
```

Every 10 iterations, X0NE auto-commits (`git add -A && git commit`). Ctrl+C to stop gracefully.

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
