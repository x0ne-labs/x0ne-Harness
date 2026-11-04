// `x0ne-dataset query` — retrieve top-K matching past trajectories from the
// RAG index. Output is human-readable by default, JSON with --json (the same
// shape returned by the `x0ne_recipe` MCP tool exposed via the gateway).

use anyhow::Result;
use x0ne_dataset::query as run_query;

pub fn run(intent: &str, k: usize, want_json: bool) -> Result<()> {
    let hits = run_query(intent, k)?;
    if want_json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    println!("query: {}\n", intent);
    if hits.is_empty() {
        println!("(no matches — index may be empty; run `x0ne-dataset index` first)");
        return Ok(());
    }
    for (i, h) in hits.iter().enumerate() {
        println!(
            "#{}  sim={:.3}  outcome={:<8}  steps={}\n     goal: {}\n     plan: {}\n     tools: {}\n",
            i + 1,
            h.similarity,
            h.outcome,
            h.num_actions,
            h.goal,
            h.summary,
            h.tools.join(", ")
        );
    }
    Ok(())
}
