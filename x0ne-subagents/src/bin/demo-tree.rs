//! demo-tree: run one goal through a recursive sub-agent tree and print it live.
//!
//!   demo-tree [--goal TEXT] [--max-depth 1..4] [--max-usd X<=2.00] [--width N]
//!             [--out DIR] [--verify CMD] [--mock]
//!
//! Real mode reads ANTHROPIC_API_KEY, X0NE_LLM_MODEL, HTTPS_PROXY from the
//! environment. `--mock` starts a fake Messages API on 127.0.0.1 that plays a
//! scripted tree: same HTTP client, same budget path, no network, no key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use x0ne_subagents::anthropic::{AnthropicConfig, AnthropicLlm};
use x0ne_subagents::budget::Budget;
use x0ne_subagents::caps::{CapSet, Capability};
use x0ne_subagents::merge::rel_paths;
use x0ne_subagents::mock_http::{MockServer, Recorded};
use x0ne_subagents::node::{ModelTier, NodeId, NodeState, TaskSpec};
use x0ne_subagents::tree::{NodeSnapshot, Tree, TreeConfig};

/// Hard ceiling for this demo, whatever the flags say.
const HARD_CAP_USD: f64 = 2.00;

const DEFAULT_GOAL: &str = "Write a small Python command-line tool, textstats.py: given file paths (or stdin), \
print line, word and character counts and the 3 most frequent words. Include unittest tests (test_*.py). \
Split the work between sub-agents (for example: counting functions with their tests, word frequency with \
its tests, the command-line front end with its tests); each may split further. All files in the work dir \
root, standard library only. Done when `python3 -m unittest discover -q` passes.";

const DEFAULT_VERIFY: &str = "python3 -m unittest discover -q";

struct Args {
    goal: String,
    max_depth: usize,
    max_usd: f64,
    width: usize,
    out: PathBuf,
    verify: String,
    mock: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        goal: DEFAULT_GOAL.into(),
        max_depth: 3,
        max_usd: HARD_CAP_USD,
        width: 3,
        out: PathBuf::from("demo-out"),
        verify: DEFAULT_VERIFY.into(),
        mock: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--goal" => a.goal = val()?,
            "--max-depth" => a.max_depth = val()?.parse().map_err(|_| "bad --max-depth")?,
            "--max-usd" => a.max_usd = val()?.parse().map_err(|_| "bad --max-usd")?,
            "--width" => a.width = val()?.parse().map_err(|_| "bad --width")?,
            "--out" => a.out = PathBuf::from(val()?),
            "--verify" => a.verify = val()?,
            "--mock" => a.mock = true,
            "-h" | "--help" => {
                println!("demo-tree [--goal TEXT] [--max-depth 1..4] [--max-usd X<=2.00] [--width N] [--out DIR] [--verify CMD] [--mock]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !(1..=4).contains(&a.max_depth) {
        return Err("--max-depth must be 1..4".into());
    }
    if !(a.max_usd > 0.0 && a.max_usd <= HARD_CAP_USD) {
        return Err(format!("--max-usd must be in (0, {HARD_CAP_USD}]"));
    }
    if !(1..=10).contains(&a.width) {
        return Err("--width must be 1..10".into());
    }
    Ok(a)
}

/// Fake Messages API: each node writes a module and its test, non-leaves
/// spawn children (2 at depth 0-1, 1 deeper), wait, run the tests, report.
fn mock_handler(r: &Recorded) -> (u16, String) {
    let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    if r.target.ends_with("/count_tokens") {
        let n = body.to_string().len() as u64 / 4;
        return (200, json!({ "input_tokens": n }).to_string());
    }
    let sys = body["system"].as_str().unwrap_or("");
    let id = sys
        .split("You are node ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .unwrap_or("root")
        .to_string();
    let depth = id.matches('/').count();
    let can_spawn = body["tools"]
        .as_array()
        .map(|t| t.iter().any(|x| x["name"] == "spawn"))
        .unwrap_or(false);
    let turn = body["messages"]
        .as_array()
        .map(|m| m.len() / 2)
        .unwrap_or(0);
    let stem = id.replace('/', "_");
    let kids = if !can_spawn {
        0
    } else if depth <= 1 {
        2
    } else {
        1
    };
    let mut steps: Vec<(&str, Value)> = vec![
        (
            "write",
            json!({"path": format!("m_{stem}.py"),
            "content": format!("def who():\n    return {id:?}\n\n\ndef count_words(text):\n    return len(text.split())\n")}),
        ),
        (
            "write",
            json!({"path": format!("test_m_{stem}.py"),
            "content": format!("import unittest\n\nimport m_{stem} as m\n\n\nclass T(unittest.TestCase):\n    def test_who(self):\n        self.assertEqual(m.who(), {id:?})\n\n    def test_words(self):\n        self.assertEqual(m.count_words('a b  c'), 3)\n\n\nif __name__ == '__main__':\n    unittest.main()\n")}),
        ),
    ];
    for k in 1..=kids {
        steps.push((
            "spawn",
            json!({"statement": format!("part {k} of {id}"),
            "verify": DEFAULT_VERIFY, "share_percent": if kids == 2 { 40 } else { 50 }}),
        ));
    }
    if kids > 0 {
        steps.push(("wait", json!({})));
    }
    steps.push(("exec", json!({"cmd": DEFAULT_VERIFY})));
    steps.push((
        "report",
        json!({"summary": format!("{id}: m_{stem}.py and its test written, {kids} child(ren) merged, tests pass")}),
    ));
    let (name, input) = steps
        .get(turn)
        .cloned()
        .unwrap_or(("report", json!({"summary": "done"})));
    let input_tokens = 900 + 350 * turn as u64;
    let reply = json!({
        "id": format!("msg_mock_{stem}_{turn}"), "type": "message", "role": "assistant",
        "model": "mock",
        "content": [{"type": "tool_use", "id": format!("toolu_{stem}_{turn}"), "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": 120}
    });
    (200, reply.to_string())
}

fn usd(micro: u64) -> String {
    format!("{:.4} $", micro as f64 / 1e6)
}

fn verdict(s: &NodeState) -> &'static str {
    match s {
        NodeState::Queued => "queued",
        NodeState::Running => "running",
        NodeState::Verified => "VERIFIED",
        NodeState::Unverified => "unverified",
        NodeState::Failed(_) => "FAILED",
        NodeState::Cancelled => "cancelled",
    }
}

fn line(t0: Instant, n: &NodeSnapshot, started: Option<Instant>) -> String {
    let pad = "  ".repeat(n.depth);
    let mut s = format!(
        "[{:>6.2}s] {pad}{:<14} d{} {:<10}",
        t0.elapsed().as_secs_f64(),
        n.id.to_string(),
        n.depth,
        verdict(&n.state)
    );
    if !matches!(n.state, NodeState::Queued | NodeState::Running) {
        let dur = started
            .map(|t| format!("{:.2}s", t.elapsed().as_secs_f64()))
            .unwrap_or_default();
        s.push_str(&format!(
            " {dur:>7} {:>7} tok {}",
            n.ledger.spent.tokens,
            usd(n.ledger.spent.micro_usd)
        ));
        if let NodeState::Failed(why) = &n.state {
            s.push_str(&format!("  ({why})"));
        }
    }
    s
}

/// Poll the tree and print every state change, with per-node durations.
async fn live(tree: Tree, t0: Instant, stop: Arc<tokio::sync::Notify>) {
    let mut seen: BTreeMap<NodeId, (String, Instant)> = BTreeMap::new();
    loop {
        let done = tokio::select! {
            _ = stop.notified() => true,
            _ = tokio::time::sleep(Duration::from_millis(50)) => false,
        };
        for (id, n) in tree.snapshot() {
            let label = verdict(&n.state).to_string();
            let entry = seen.entry(id.clone());
            let started = match entry {
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    if e.get().0 == label {
                        continue;
                    }
                    e.get_mut().0 = label;
                    e.get().1
                }
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert((label, Instant::now())).1
                }
            };
            println!("{}", line(t0, &n, Some(started)));
        }
        if done {
            return;
        }
    }
}

fn print_tree(snap: &BTreeMap<NodeId, NodeSnapshot>) {
    for n in snap.values() {
        let summary = n
            .report
            .as_ref()
            .map(|r| r.summary.replace('\n', " "))
            .unwrap_or_default();
        let summary = match &n.state {
            NodeState::Failed(why) if summary.is_empty() => format!("({why})"),
            _ => summary,
        };
        let summary: String = summary.chars().take(110).collect();
        println!(
            "  {}{:<14} {:<10} {:>7} tok {}  {}",
            "  ".repeat(n.depth),
            n.id.to_string(),
            verdict(&n.state),
            n.ledger.spent.tokens,
            usd(n.ledger.spent.micro_usd),
            summary
        );
    }
}

fn show_files(dir: &Path) {
    let mut files = rel_paths(dir);
    files.sort();
    for f in &files {
        let p = dir.join(f);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        println!("  {:<40} {size:>6} B", f.display());
    }
    if files.is_empty() {
        println!("  (no files)");
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("demo-tree: {e}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;
    let _mock;
    let mut cfg = if args.mock {
        let srv = MockServer::start(mock_handler)
            .await
            .map_err(|e| format!("mock server: {e}"))?;
        let url = srv.url.clone();
        _mock = srv;
        // No key, no proxy: the fake API lives on 127.0.0.1 in this process.
        AnthropicConfig::from_vars(|k| match k {
            "ANTHROPIC_API_KEY" => Some("mock-no-real-key".into()),
            "ANTHROPIC_BASE_URL" => Some(url.clone()),
            _ => None,
        })?
    } else {
        AnthropicConfig::from_env()?
    };
    cfg.max_depth = Some(args.max_depth);
    cfg.max_tokens = 2048;
    let model = cfg.model.clone();
    let proxy = cfg.proxy.clone();
    let llm = Arc::new(AnthropicLlm::new(cfg)?);

    let out = &args.out;
    let workspace = out.join("workspace");
    let journal_dir = out.join("journal");
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
    let tree_cfg = TreeConfig {
        max_children_per_node: args.width,
        max_concurrent_sandboxes: 4,
        max_depth: Some(args.max_depth),
        exec_timeout: Duration::from_secs(30),
        ..TreeConfig::default()
    };
    let tree = Tree::new(
        tree_cfg,
        &journal_dir,
        &out.join("sandboxes"),
        &workspace,
        llm.clone(),
        String::new(),
    )?;
    let cap_micro = (args.max_usd * 1e6).floor() as u64;
    let spec = TaskSpec {
        statement: args.goal.clone(),
        caps: CapSet::of(&[
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Spawn,
        ]),
        budget: Budget {
            tokens: 2_000_000,
            micro_usd: cap_micro,
            ms: 300_000,
            actions: 400,
        },
        verify: Some(args.verify.clone()),
        model: ModelTier::Cheap,
    };

    println!(
        "demo-tree  mode={}  model={}  proxy={}  max_depth={}  width={}  budget={}",
        if args.mock { "MOCK" } else { "REAL" },
        if args.mock { "mock" } else { &model },
        proxy.as_deref().unwrap_or("-"),
        args.max_depth,
        args.width,
        usd(cap_micro)
    );
    println!("goal: {}\n", args.goal);

    let t0 = Instant::now();
    let stop = Arc::new(tokio::sync::Notify::new());
    let printer = tokio::spawn(live(tree.clone(), t0, Arc::clone(&stop)));
    let outcome = tree.run_root(spec).await?;
    stop.notify_one();
    let _ = printer.await;
    let elapsed = t0.elapsed();

    let snap = tree.snapshot();
    println!("\n== tree ==");
    print_tree(&snap);

    println!("\n== merged result (root work dir) ==");
    if let Some(w) = tree.root_workdir() {
        show_files(&w);
    }
    match tree.export_root() {
        Ok(rep) => println!(
            "exported to {}: {} applied, {} rejected, {} conflicts",
            workspace.display(),
            rep.applied.len(),
            rep.rejected.len(),
            rep.conflicts.len()
        ),
        Err(e) => println!("not exported: {e}"),
    }
    if let Some(r) = &outcome.report {
        println!("root verdict: {}\n{}", r.state, r.evidence);
    }

    let mut entries = 0;
    let mut broken = Vec::new();
    for id in snap.keys() {
        match tree.journal().verify_chain(id) {
            Ok(n) => entries += n,
            Err(e) => broken.push(format!("{id}: {e}")),
        }
    }
    let totals = llm.totals();
    let spent = tree.total_spent();
    let max_depth = snap.keys().map(|k| k.depth()).max().unwrap_or(0);
    println!("\n== totals ==");
    println!(
        "nodes {}  depth {}  duration {:.2}s  verified {}",
        snap.len(),
        max_depth,
        elapsed.as_secs_f64(),
        outcome.verified
    );
    println!(
        "llm calls {}  in {}  out {}  cost {}{}  (tree debited {}, cap {})  refused-for-budget {}  errors {}",
        totals.calls,
        totals.input_tokens,
        totals.output_tokens,
        usd(totals.micro_usd),
        if args.mock { " (simulated)" } else { "" },
        usd(spent.micro_usd),
        usd(cap_micro),
        totals.refused_for_budget,
        totals.errors
    );
    println!(
        "journal {}  {} nodes, {} chained entries, {}",
        journal_dir.display(),
        snap.len(),
        entries,
        if broken.is_empty() {
            "all chains verify".to_string()
        } else {
            format!("BROKEN: {}", broken.join("; "))
        }
    );
    let summary = json!({
        "mode": if args.mock { "mock" } else { "real" },
        "model": if args.mock { "mock".to_string() } else { model },
        "goal": args.goal, "max_depth": args.max_depth, "width": args.width,
        "cap_micro_usd": cap_micro, "duration_ms": elapsed.as_millis() as u64,
        "verified": outcome.verified, "nodes": snap.len(), "depth": max_depth,
        "llm": totals, "tree_spent": spent, "journal_entries": entries,
        "journal_ok": broken.is_empty(),
        "states": snap.values().map(|n| (n.id.to_string(), n.state.label())).collect::<BTreeMap<_, _>>(),
    });
    std::fs::write(
        out.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    if spent.micro_usd > cap_micro || totals.micro_usd > cap_micro {
        return Err("spend above the cap: this must never happen".into());
    }
    Ok(())
}
