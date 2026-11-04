# Example: Explore Codebase

**Scenario:** You drop into an unfamiliar project and want X0NE to explain it.

---

```
User: what does this project do? give me a summary.
```

```
[Reasoning] I'll use a Pipeline to read the structure and key files in one shot,
then give a full summary.

[ACTION] Pipeline
  Step 0: ListDir .
  Step 1: ReadFile ./README.md
  Step 2: ReadFile ./Cargo.toml
  Step 3: FindFiles {root: ".", pattern: "*.rs", max_results: 30}

[RESULT] Pipeline 4/4 steps
  Step 0 ✓ ListDir (1 ms) — 12 entries
  Step 1 ✓ ReadFile (2 ms) — 4.2 KB read
  Step 2 ✓ ReadFile (1 ms) — 1.1 KB read
  Step 3 ✓ FindFiles (8 ms) — 14 .rs files found
```

```
[ACTION] Pipeline
  Step 0: ReadFile ./src/main.rs
  Step 1: ReadFile ./src/lib.rs
  Step 2: SearchCode {root: "./src", pattern: "pub fn", max_results: 30}

[RESULT] Pipeline 3/3 steps
  Step 0 ✓ ReadFile (2 ms) — 5.8 KB read
  Step 1 ✓ ReadFile (3 ms) — 12.1 KB read
  Step 2 ✓ SearchCode (15 ms) — 23 matches
```

```
[ACTION] GitStatus .
[RESULT] ✓ 3 modified files, 1 untracked
```

```
Here's what this project does:

**rust-csv-analyzer** — A CLI tool for analyzing CSV files and producing statistical reports.

Key capabilities I found:
- Reads CSV files of any size using streaming (no full load into memory)
- Computes per-column statistics: min, max, mean, median, standard deviation, null count
- Exports reports in JSON, Markdown, or HTML format via --format flag
- Supports filtering rows with --filter "column=value"
- Has a TUI mode (--interactive) using the ratatui library

Entry point: src/main.rs sets up clap args and dispatches to lib.rs
Core logic: src/lib.rs — Analyzer struct with analyze() method
Stats engine: src/stats.rs — per-column statistics (23 public functions found)

Recently modified: src/stats.rs, src/output/markdown.rs, Cargo.toml
There's an untracked file: examples/sales_2026.csv — likely a test file.

**Dependencies:** csv, serde, clap, ratatui, ratatui-table, serde_json

Want me to dive deeper into any specific part?
```

---

**What happened technically:**

1. First Pipeline gathered the project shape (directory, README, Cargo.toml, file list) in one LLM call
2. Second Pipeline read the key source files and found public API surface
3. `GitStatus` gave context on what's being actively worked on
4. Total: 3 actions (2 pipelines + 1 status), 3 LLM calls vs. 7+ sequential calls without pipelines
