//! Visual preview of the new `crate::prose` markdown renderer.
//!
//! Run with: `cargo run -p x0ne-orchestrator --example prose_preview`
//! Pipe through `less -R` or just resize your terminal to see how the
//! width-cap and word-wrap behave on different layouts.

use x0ne_orchestrator::prose::{render_prose, ProseLayout, ProseTheme};

const SAMPLE: &str = r#"# X0NE 2030 — Le North Star

To make X0NE the **"Best 2030 Agent,"** we need to move beyond the *Request → Action → Result* loop and toward `Objective → Strategy → Execution → Validation`.

If I look at my own current limitations and the trajectory of AI, here is my blueprint for X0NE 2030. I've divided this into three **Evolutionary Leaps**:

## 1. The "Cognitive" Leap

- **Hierarchical Planning** — Instead of a linear list of actions, X0NE should generate a "Project Roadmap" with milestones.
- **Multi-Agent Orchestration** (The "Hive" Mode) — Spawn `SecurityAgent`, `QAAgent`, `DevOpsAgent`, all reporting back to a Lead Architect.
- **Deep Semantic Memory** — A 2030 agent needs a **Graph-based Long-Term Memory** that captures *intent*, not just text.

> [!NOTE]
> X0NE.md is a great start, but a 2030 agent needs more.

> [!WARNING]
> Without graph memory, the agent will rediscover the same patterns every session.

## 2. The "Tooling" Leap

| Current X0NE (2024/25) | X0NE 2030             |
|------------------------|-----------------------|
| Reactive               | Proactive             |
| Linear: `Action → Result` | Strategic: `Objective → Plan` |
| Single-threaded        | Orchestrated          |
| Text-based             | Systemic              |
| Empirical              | Formally verified     |

```rust
fn main() {
    let agent = x0ne::Agent::new()
        .with_memory(GraphMemory::default())
        .with_planner(Hierarchical::default());
    agent.run("ship the GOSIM demo");
}
```

If we implement these, X0NE won't just be a tool you use — it will be a **teammate you trust** with the keys to the production server. ✨
"#;

fn main() {
    let theme = ProseTheme::x0ne_dark();
    // Match `print_llm_message` width logic so the preview is faithful.
    let term_w = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(120);
    let cap = 100usize;
    let width = term_w.saturating_sub(4).min(cap).max(20);
    let layout = ProseLayout::new(width);

    println!();
    println!("  ╭─ response (preview, width={width}) ───");
    // Match `print_llm_message`: drop leading blanks from the body so the box
    // doesn't open with a double-empty rail when the first element is a
    // heading (which prepends its own breathing room).
    let body: Vec<String> = render_prose(SAMPLE, &layout, &theme)
        .into_iter()
        .skip_while(|s| s.is_empty())
        .collect();
    println!("  │");
    for line in &body {
        if line.is_empty() {
            println!("  │");
        } else {
            println!("  │ {line}");
        }
    }
    println!("  ╰─{}", "─".repeat(width.saturating_sub(2)));
    println!();
}
