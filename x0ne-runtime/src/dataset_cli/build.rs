// dataset_build — extract trajectories from ~/.x0ne/sessions/*.jsonl into a
// clean (goal → typed_action_sequence → outcome) corpus suitable for RAG,
// few-shot injection, synthetic-data generation, and downstream SFT/DPO.
//
// The session log schema is already typed (each `action` line is a serialized
// Action enum, each `action_result` line is a typed Result). The extractor
// just regroups, computes per-session stats, and rejects empty/abandoned
// sessions.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use x0ne_dataset::x0ne_home;

#[derive(Debug, Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    action: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    action_type: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    tokens_in: Option<u64>,
    #[serde(default)]
    tokens_out: Option<u64>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Serialize)]
struct Step {
    action: Value,
    result: Value,
    action_type: String,
    success: bool,
    duration_ms: u64,
    ts: String,
}

#[derive(Debug, Serialize)]
struct TrajectoryStats {
    num_actions: usize,
    num_success: usize,
    num_failures: usize,
    success_rate: f64,
    total_duration_ms: u64,
    tools_used: Vec<String>,
    tokens_in: u64,
    tokens_out: u64,
}

#[derive(Debug, Serialize)]
struct Trajectory {
    session_id: String,
    started_at: String,
    model: String,
    version: String,
    hostname: String,
    goal: String,
    follow_ups: Vec<String>,
    trajectory: Vec<Step>,
    stats: TrajectoryStats,
    outcome: String,
    quality_score: u32,
}

// Heuristic 0-100 score so RAG / SFT consumers can filter dev noise without
// re-implementing this everywhere. Weighted: real-looking goal (50pt),
// non-trivial trajectory (30pt), good outcome (20pt).
fn quality_score(goal: &str, num_actions: usize, success_rate: f64, outcome: &str) -> u32 {
    let mut s = 0u32;
    let goal_trimmed = goal.trim();
    let word_count = goal_trimmed.split_whitespace().count();
    let is_slash = goal_trimmed.starts_with('/');
    let is_short = word_count < 2;
    let is_smoke = matches!(
        goal_trimmed.to_lowercase().as_str(),
        "q" | "quit" | "exit" | "hi" | "yo" | "ok" | "test" | "ping"
    );
    if !goal_trimmed.is_empty() && !is_slash && !is_short && !is_smoke {
        s += 30;
    }
    if word_count >= 4 {
        s += 20;
    }
    if num_actions >= 3 {
        s += 20;
    }
    if num_actions >= 8 {
        s += 10;
    }
    match outcome {
        "success" => s += 20,
        "partial" if success_rate >= 0.7 => s += 10,
        _ => {}
    }
    s.min(100)
}

fn classify_outcome(success_rate: f64, num_actions: usize) -> &'static str {
    if num_actions == 0 {
        "abandoned"
    } else if success_rate >= 0.95 {
        "success"
    } else if success_rate >= 0.5 {
        "partial"
    } else {
        "failure"
    }
}

fn extract_one(path: &PathBuf) -> Result<Option<Trajectory>> {
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(f);

    let mut started_at = String::new();
    let mut model = String::new();
    let mut version = String::new();
    let mut hostname = String::new();
    let mut user_inputs: Vec<String> = Vec::new();
    let mut pending_actions: Vec<(Value, String, bool, u64, String)> = Vec::new();
    let mut results_by_type: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;

    for line in reader.lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        let raw: RawLine = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match raw.kind.as_str() {
            "session_start" => {
                started_at = raw.ts.unwrap_or_default();
                model = raw.model.unwrap_or_default();
                version = raw.version.unwrap_or_default();
                hostname = raw.hostname.unwrap_or_default();
            }
            "user_input" => {
                if let Some(c) = raw.content.as_ref().and_then(|v| v.as_str()) {
                    let s = c.trim();
                    if !s.is_empty() {
                        user_inputs.push(s.to_string());
                    }
                }
            }
            "llm_response" => {
                tokens_in = tokens_in.saturating_add(raw.tokens_in.unwrap_or(0));
                tokens_out = tokens_out.saturating_add(raw.tokens_out.unwrap_or(0));
            }
            "action" => {
                let act = raw.action.unwrap_or(Value::Null);
                // Older session logs sometimes omit the top-level `action_type`
                // field; fall back to `action.type` so we don't drop them.
                let at = raw
                    .action_type
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        act.get("type")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                let ok = raw.success.unwrap_or(false);
                let dur = raw.duration_ms.unwrap_or(0);
                let ts = raw.ts.unwrap_or_default();
                pending_actions.push((act, at, ok, dur, ts));
            }
            "action_result" => {
                let r = raw.result.unwrap_or(Value::Null);
                let at = raw
                    .action_type
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        r.get("action_type")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                results_by_type.entry(at).or_default().push(r);
            }
            _ => {}
        }
    }

    // Pair actions with results in arrival order, by matching action_type.
    let mut steps: Vec<Step> = Vec::with_capacity(pending_actions.len());
    let mut cursor: BTreeMap<String, usize> = BTreeMap::new();
    for (act, at, ok, dur, ts) in pending_actions {
        let idx = *cursor.entry(at.clone()).or_insert(0);
        let result = results_by_type
            .get(&at)
            .and_then(|v| v.get(idx))
            .cloned()
            .unwrap_or(Value::Null);
        cursor.insert(at.clone(), idx + 1);
        steps.push(Step {
            action: act,
            result,
            action_type: at,
            success: ok,
            duration_ms: dur,
            ts,
        });
    }

    if user_inputs.is_empty() && steps.is_empty() {
        return Ok(None);
    }

    let num_actions = steps.len();
    let num_success = steps.iter().filter(|s| s.success).count();
    let num_failures = num_actions.saturating_sub(num_success);
    let success_rate = if num_actions == 0 {
        0.0
    } else {
        num_success as f64 / num_actions as f64
    };
    let total_duration_ms = steps.iter().map(|s| s.duration_ms).sum();
    let mut tools_used: Vec<String> = steps.iter().map(|s| s.action_type.clone()).collect();
    tools_used.sort();
    tools_used.dedup();

    let outcome = classify_outcome(success_rate, num_actions).to_string();
    let goal = user_inputs.first().cloned().unwrap_or_default();
    let follow_ups = user_inputs.into_iter().skip(1).collect();
    let q = quality_score(&goal, num_actions, success_rate, &outcome);

    Ok(Some(Trajectory {
        session_id,
        started_at,
        model,
        version,
        hostname,
        goal,
        follow_ups,
        trajectory: steps,
        stats: TrajectoryStats {
            num_actions,
            num_success,
            num_failures,
            success_rate,
            total_duration_ms,
            tools_used,
            tokens_in,
            tokens_out,
        },
        outcome,
        quality_score: q,
    }))
}

// x0ne_home() is imported from x0ne_dataset (single source of truth).

pub fn run(force_full: bool) -> Result<()> {
    let x0ne = x0ne_home();
    let sessions_dir = x0ne.join("sessions");
    let dataset_dir = x0ne.join("dataset");
    fs::create_dir_all(&dataset_dir).context("creating dataset dir")?;

    let marker_path = dataset_dir.join(".last_build_ts");
    let out_path_for_check = dataset_dir.join("trajectories.jsonl");
    let run_started_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Self-heal: marker present but trajectories.jsonl missing (user `rm`d it
    // by hand, or it never materialized due to an earlier crash) → silently
    // promote to full rebuild instead of incrementally appending into a void.
    // This is the single most common foot-gun reported during the brick rollout.
    let marker_present = marker_path.exists();
    let trajectories_present = out_path_for_check.exists();
    let stale_marker = marker_present && !trajectories_present;
    if stale_marker {
        eprintln!(
            "[dataset] marker present but trajectories.jsonl missing — promoting to full rebuild"
        );
        let _ = fs::remove_file(&marker_path);
    }
    let last_build_secs: u64 = if force_full || stale_marker {
        0
    } else {
        fs::read_to_string(&marker_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };

    let pattern = format!("{}/*.jsonl", sessions_dir.display());
    let mut entries: Vec<PathBuf> = glob::glob(&pattern)
        .map_err(|e| anyhow!("glob: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let out_path = dataset_dir.join("trajectories.jsonl");
    // Incremental mode: keep what's already in trajectories.jsonl, skip
    // session files older than the last marker, and append the new ones.
    let total_seen = entries.len();
    let mut skipped_old = 0usize;
    if last_build_secs > 0 && out_path.exists() {
        entries.retain(|p| match fs::metadata(p).and_then(|m| m.modified()) {
            Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) if d.as_secs() > last_build_secs => true,
                _ => {
                    skipped_old += 1;
                    false
                }
            },
            Err(_) => true, // can't stat → process to be safe
        });
    }
    let incremental = last_build_secs > 0 && out_path.exists();

    eprintln!(
        "[dataset] {} mode: scanning {}/{} session files in {} (skipped {} older than last build)",
        if incremental { "incremental" } else { "full" },
        entries.len(),
        total_seen,
        sessions_dir.display(),
        skipped_old
    );

    let mut out = if incremental {
        File::options()
            .append(true)
            .create(true)
            .open(&out_path)
            .context("opening trajectories.jsonl in append mode")?
    } else {
        File::create(&out_path).context("creating trajectories.jsonl")?
    };

    let mut total = 0usize;
    let mut kept = 0usize;
    let mut errors = 0usize;
    let mut by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    let mut tool_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut goal_first_word: BTreeMap<String, usize> = BTreeMap::new();
    let mut model_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_actions = 0usize;
    let mut total_success = 0usize;
    let mut total_tokens_in = 0u64;
    let mut total_tokens_out = 0u64;
    let mut traj_lengths: Vec<usize> = Vec::new();
    let mut quality_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut high_quality = 0usize;

    for path in &entries {
        total += 1;
        match extract_one(path) {
            Ok(Some(t)) => {
                kept += 1;
                *by_outcome.entry(t.outcome.clone()).or_insert(0) += 1;
                let model_key = if t.model.is_empty() {
                    "unknown".to_string()
                } else {
                    t.model.clone()
                };
                *model_counts.entry(model_key).or_insert(0) += 1;
                for s in &t.trajectory {
                    *tool_counts.entry(s.action_type.clone()).or_insert(0) += 1;
                }
                if !t.goal.is_empty() {
                    let first = t
                        .goal
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_lowercase();
                    if !first.is_empty() {
                        *goal_first_word.entry(first).or_insert(0) += 1;
                    }
                }
                total_actions += t.stats.num_actions;
                total_success += t.stats.num_success;
                total_tokens_in = total_tokens_in.saturating_add(t.stats.tokens_in);
                total_tokens_out = total_tokens_out.saturating_add(t.stats.tokens_out);
                traj_lengths.push(t.stats.num_actions);
                let bucket = match t.quality_score {
                    0..=29 => "noise",
                    30..=59 => "low",
                    60..=79 => "ok",
                    _ => "gold",
                };
                *quality_buckets.entry(bucket).or_insert(0) += 1;
                if t.quality_score >= 60 {
                    high_quality += 1;
                }
                let line = serde_json::to_string(&t).context("serializing trajectory")?;
                writeln!(out, "{}", line)?;
            }
            Ok(None) => {}
            Err(_) => {
                errors += 1;
            }
        }
    }
    out.flush()?;

    let mut top_tools: Vec<(String, usize)> =
        tool_counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
    top_tools.sort_by_key(|t| std::cmp::Reverse(t.1));
    top_tools.truncate(20);

    let mut top_goals: Vec<(String, usize)> = goal_first_word
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    top_goals.sort_by_key(|g| std::cmp::Reverse(g.1));
    top_goals.truncate(20);

    let avg_len = if !traj_lengths.is_empty() {
        traj_lengths.iter().sum::<usize>() as f64 / traj_lengths.len() as f64
    } else {
        0.0
    };
    let median_len = {
        let mut v = traj_lengths.clone();
        v.sort();
        if v.is_empty() {
            0
        } else {
            v[v.len() / 2]
        }
    };

    let global_success = if total_actions == 0 {
        0.0
    } else {
        total_success as f64 / total_actions as f64
    };

    let stats = json!({
        "scanned_files": total,
        "trajectories_kept": kept,
        "trajectories_high_quality": high_quality,
        "parse_errors": errors,
        "by_outcome": by_outcome,
        "by_quality": quality_buckets,
        "by_model": model_counts,
        "total_actions": total_actions,
        "global_success_rate": global_success,
        "avg_trajectory_length": avg_len,
        "median_trajectory_length": median_len,
        "tokens_in_total": total_tokens_in,
        "tokens_out_total": total_tokens_out,
        "top_tools": top_tools,
        "top_goal_first_words": top_goals,
        "output_path": out_path.display().to_string(),
    });

    let stats_path = dataset_dir.join("stats.json");
    let mut sf = File::create(&stats_path)?;
    writeln!(sf, "{}", serde_json::to_string_pretty(&stats)?)?;

    eprintln!("\n[dataset] === SUMMARY ===");
    eprintln!(
        "scanned: {} files | kept: {} trajectories | parse errors: {}",
        total, kept, errors
    );
    eprintln!("by outcome:");
    for (k, v) in &by_outcome {
        eprintln!("  {:<12} {}", k, v);
    }
    eprintln!("by quality:");
    for (k, v) in &quality_buckets {
        eprintln!("  {:<12} {}", k, v);
    }
    eprintln!(
        "high-quality (>=60): {} / {}  ({:.1}%)",
        high_quality,
        kept,
        if kept > 0 {
            high_quality as f64 / kept as f64 * 100.0
        } else {
            0.0
        }
    );
    eprintln!(
        "actions: {} total | success rate: {:.1}% | avg traj len: {:.1} | median: {}",
        total_actions,
        global_success * 100.0,
        avg_len,
        median_len
    );
    eprintln!("tokens: in={} out={}", total_tokens_in, total_tokens_out);
    eprintln!("top 10 tools used:");
    for (t, c) in top_tools.iter().take(10) {
        eprintln!("  {:<24} {}", t, c);
    }
    eprintln!("top 10 goal first-words:");
    for (g, c) in top_goals.iter().take(10) {
        eprintln!("  {:<16} {}", g, c);
    }
    eprintln!(
        "\n[dataset] wrote {} ({} bytes)",
        out_path.display(),
        fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0)
    );
    eprintln!("[dataset] wrote {}", stats_path.display());
    // Update the incremental marker. Use run-start epoch (not now) so any
    // session file created mid-run is still picked up by the NEXT invocation.
    let _ = fs::write(&marker_path, run_started_secs.to_string());
    if incremental {
        eprintln!("[dataset] mode: incremental (appended {} new). Run with --full to rebuild aggregate stats.", kept);
    } else {
        eprintln!("[dataset] mode: full. Marker -> {}", marker_path.display());
    }
    Ok(())
}
