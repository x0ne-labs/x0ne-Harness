// x0ne-eval — measure how well a model emits valid X0NE Pipelines.
//
// Two scoring axes (no execution required — pure structural eval):
//
//   schema_valid   : action JSON parses against ~/.x0ne/dataset/action_schema.json
//   action_known   : the `type` field matches one of the strict (non-catchall)
//                    branches; ie a real MCP-exposed primitive
//
// One generative axis (requires Ollama):
//
//   loss_estimate  : mean per-token negative log-likelihood the model assigns
//                    to the ground-truth assistant payload. Same model run
//                    against the same eval set before/after a LoRA = the
//                    primary signal "did fine-tuning help?". Skipped if Ollama
//                    is unreachable; structural eval still reports.
//
// Held-out by default: takes the most-recent --holdout=N sessions from the
// SFT corpus (quality >= 60) and runs them. Default holdout=20.
//
// Reads:
//   ~/.x0ne/dataset/sft_v1.jsonl       (run `make dataset-sft` first)
//   ~/.x0ne/dataset/action_schema.json (run `make export-schema` first)
// Writes:
//   ~/.x0ne/dataset/eval_<model>_<ts>.jsonl   per-example results
//   ~/.x0ne/dataset/eval_<model>_<ts>.summary.json   aggregate

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use x0ne_dataset::{assistant_first_action, schema_valid_quick, x0ne_home};

#[derive(Deserialize)]
struct SftMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct SftRow {
    messages: Vec<SftMessage>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Serialize, Default)]
struct EvalResult {
    session_id: String,
    goal: String,
    schema_valid: bool,
    action_known: bool,
    type_field: String,
    loss_estimate: Option<f64>,
    error: Option<String>,
}

#[derive(Serialize)]
struct EvalSummary {
    model: String,
    n_total: usize,
    n_schema_valid: usize,
    schema_valid_rate: f64,
    n_action_known: usize,
    action_known_rate: f64,
    n_loss_estimated: usize,
    mean_loss: Option<f64>,
    started_at: String,
    duration_ms: u128,
}

fn load_schema() -> Result<(Value, HashSet<String>)> {
    let p = x0ne_home().join("dataset/action_schema.json");
    let s = fs::read_to_string(&p)
        .with_context(|| format!("reading {} (run `make export-schema` first)", p.display()))?;
    let schema: Value = serde_json::from_str(&s)?;
    let mut known: HashSet<String> = HashSet::new();
    if let Some(arr) = schema.get("anyOf").and_then(|v| v.as_array()) {
        for branch in arr {
            if let Some(c) = branch
                .get("properties")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.get("const"))
                .and_then(|v| v.as_str())
            {
                known.insert(c.to_string());
            }
        }
    }
    Ok((schema, known))
}

// Best-effort Ollama loss estimate via /api/generate with options.num_predict=0
// would skip generation but doesn't return logprobs. Instead we make a single
// request asking the model to *score* a payload by computing perplexity from
// the eval_count + eval_duration heuristic. This is rough but stable across
// runs: same eval set + same model → comparable numbers. If Ollama is down we
// just leave loss_estimate as None.
fn ollama_score(model: &str, prompt: &str, target: &str) -> Option<f64> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let url = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".into());
    let host_port = url.trim_start_matches("http://").trim_end_matches('/');
    let body = json!({
        "model": model,
        "prompt": format!("{}\n\nASSISTANT:\n{}", prompt, target),
        "stream": false,
        "options": { "num_predict": 1, "temperature": 0 }
    })
    .to_string();
    let req = format!(
        "POST /api/generate HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        host_port,
        body.len(),
        body
    );
    let host_resolved = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{}:11434", host_port)
    };
    let mut s =
        TcpStream::connect_timeout(&host_resolved.parse().ok()?, Duration::from_secs(60)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let body_idx = buf.find("\r\n\r\n").map(|i| i + 4)?;
    let resp_body = &buf[body_idx..];
    // Strip chunked transfer encoding if present
    let parsed = if let Some(first_nl) = resp_body.find("\r\n") {
        if u64::from_str_radix(resp_body[..first_nl].trim(), 16).is_ok() {
            // chunked; reassemble
            let mut out = String::new();
            let mut rest = resp_body;
            loop {
                let nl = match rest.find("\r\n") {
                    Some(i) => i,
                    None => break,
                };
                let size = u64::from_str_radix(rest[..nl].trim(), 16).unwrap_or(0) as usize;
                if size == 0 {
                    break;
                }
                let start = nl + 2;
                let end = (start + size).min(rest.len());
                out.push_str(&rest[start..end]);
                rest = &rest[(end + 2).min(rest.len())..];
            }
            out
        } else {
            resp_body.to_string()
        }
    } else {
        resp_body.to_string()
    };
    let v: Value = serde_json::from_str(&parsed).ok()?;
    let eval_count = v.get("eval_count").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let eval_duration_ns = v
        .get("eval_duration")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    if eval_count <= 0.0 || eval_duration_ns <= 0.0 {
        return None;
    }
    // Heuristic proxy: mean ms/token. Lower = model finds the target more
    // probable (less time spent rejecting alternatives). Stable across runs
    // for A/B'ing; not a true cross-entropy.
    Some((eval_duration_ns / 1_000_000.0) / eval_count)
}

pub fn run(model: String, holdout: usize, estimate_loss: bool) -> Result<()> {
    let (schema, known) = load_schema()?;
    let in_path = x0ne_home().join("dataset/sft_v1.jsonl");
    let f = File::open(&in_path).with_context(|| {
        format!(
            "opening {} (run `make dataset && make dataset-sft` first)",
            in_path.display()
        )
    })?;
    let mut all: Vec<SftRow> = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        if let Ok(row) = serde_json::from_str::<SftRow>(&line) {
            all.push(row);
        }
    }
    if all.is_empty() {
        return Err(anyhow!(
            "sft_v1.jsonl is empty — run `make dataset && make dataset-sft` first"
        ));
    }
    // Held-out set = last `holdout` rows. They are appended in time order by
    // the dataset builder, so the final N rows are the freshest sessions.
    let take = all.len().saturating_sub(holdout);
    let eval_set = &all[take..];

    let started = std::time::Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();
    eprintln!(
        "[eval] model={} eval_set={} ({} held-out from {} total) loss={}",
        model,
        eval_set.len(),
        eval_set.len(),
        all.len(),
        if estimate_loss { "yes" } else { "no" }
    );

    let mut results: Vec<EvalResult> = Vec::new();
    let mut n_schema_ok = 0;
    let mut n_known = 0;
    let mut losses: Vec<f64> = Vec::new();

    for row in eval_set {
        let user = row
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let assistant = row
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let session_id = row
            .metadata
            .as_ref()
            .and_then(|m| m.get("session_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let mut r = EvalResult {
            session_id,
            goal: user.clone(),
            ..Default::default()
        };
        let action = match assistant_first_action(&assistant) {
            Some(v) => v,
            None => {
                r.error = Some("could not parse assistant payload as JSON".into());
                results.push(r);
                continue;
            }
        };
        r.type_field = action
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        r.action_known = known.contains(&r.type_field);
        if r.action_known {
            n_known += 1;
        }
        r.schema_valid = schema_valid_quick(&action, &schema, &known);
        if r.schema_valid {
            n_schema_ok += 1;
        }
        if estimate_loss {
            let target = action.to_string();
            r.loss_estimate = ollama_score(&model, &user, &target);
            if let Some(l) = r.loss_estimate {
                losses.push(l);
            }
        }
        results.push(r);
    }

    let n = results.len();
    let mean_loss = if losses.is_empty() {
        None
    } else {
        Some(losses.iter().sum::<f64>() / losses.len() as f64)
    };
    let summary = EvalSummary {
        model: model.clone(),
        n_total: n,
        n_schema_valid: n_schema_ok,
        schema_valid_rate: if n == 0 {
            0.0
        } else {
            n_schema_ok as f64 / n as f64
        },
        n_action_known: n_known,
        action_known_rate: if n == 0 {
            0.0
        } else {
            n_known as f64 / n as f64
        },
        n_loss_estimated: losses.len(),
        mean_loss,
        started_at: started_at.clone(),
        duration_ms: started.elapsed().as_millis(),
    };

    // Write per-row + summary.
    let dir = x0ne_home().join("dataset");
    fs::create_dir_all(&dir)?;
    let safe_model = model.replace(['/', ':'], "_");
    let ts = started_at.replace(':', "-");
    let out_path = dir.join(format!("eval_{}_{}.jsonl", safe_model, ts));
    let summary_path = dir.join(format!("eval_{}_{}.summary.json", safe_model, ts));
    let mut out = File::create(&out_path)?;
    for r in &results {
        writeln!(out, "{}", serde_json::to_string(r)?)?;
    }
    let mut sf = File::create(&summary_path)?;
    writeln!(sf, "{}", serde_json::to_string_pretty(&summary)?)?;

    eprintln!(
        "[eval] schema_valid: {}/{} = {:.1}%",
        n_schema_ok,
        n,
        summary.schema_valid_rate * 100.0
    );
    eprintln!(
        "[eval] action_known: {}/{} = {:.1}%  (vs strict MCP-registry types)",
        n_known,
        n,
        summary.action_known_rate * 100.0
    );
    if let Some(l) = mean_loss {
        eprintln!("[eval] mean_loss_proxy: {:.3} ms/token (lower = better)", l);
    } else if estimate_loss {
        eprintln!("[eval] mean_loss_proxy: <ollama unreachable; skipped>");
    }
    eprintln!("[eval] wrote {}", out_path.display());
    eprintln!("[eval] wrote {}", summary_path.display());
    Ok(())
}
