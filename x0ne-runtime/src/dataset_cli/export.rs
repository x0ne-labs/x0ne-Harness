// dataset_export — convert ~/.x0ne/dataset/trajectories.jsonl into formats
// ready for fine-tuning. Two modes:
//
//   --format=sft   → ChatML-style instruction tuning pairs.
//                    {"messages": [{"role":"system","content":<schema/prompt>},
//                                  {"role":"user","content":<goal>},
//                                  {"role":"assistant","content":<pipeline_json>}]}
//                    Output: ~/.x0ne/dataset/sft_v1.jsonl
//
//   --format=dpo   → Auto-mined preference pairs from same-session retries.
//                    For each consecutive (failure_action, success_action) on
//                    the same Action variant within one trajectory, emit:
//                    {"prompt": <goal>, "chosen": <success_json>,
//                     "rejected": <failure_json>}
//                    Output: ~/.x0ne/dataset/dpo_v1.jsonl
//
// Both formats are the de-facto inputs for axolotl / unsloth / TRL / TorchTune.
// Add `--min-quality=N` to filter on the quality_score from `x0ne-dataset-build`
// (default 60 → only "ok"+"gold" buckets).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use x0ne_dataset::{build_assistant_payload, sft_system_prompt, x0ne_home};

#[derive(Debug, Deserialize)]
struct RawStep {
    action: Value,
    #[serde(default)]
    action_type: String,
    #[serde(default)]
    success: bool,
}

#[derive(Deserialize)]
struct RawTraj {
    #[serde(default)]
    session_id: String,
    goal: String,
    #[serde(default)]
    #[allow(dead_code)]
    follow_ups: Vec<String>,
    outcome: String,
    quality_score: u32,
    trajectory: Vec<RawStep>,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Serialize)]
struct SftExample<'a> {
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
}

#[derive(Debug, Serialize)]
struct DpoExample {
    prompt: String,
    chosen: String,
    rejected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
}

// The SFT system prompt is canonicalized in `x0ne_dataset::sft_system_prompt()`.
// At runtime it reads `~/.x0ne/prompts/sft_system.txt` if present, otherwise
// falls back to `DEFAULT_SFT_SYSTEM_PROMPT`. Edit that file to iterate the
// prompt without a cargo rebuild — same string is reused by RAG and eval.

fn assistant_payload_for(steps: &[RawStep]) -> String {
    let actions: Vec<Value> = steps.iter().map(|s| s.action.clone()).collect();
    build_assistant_payload(&actions)
}

fn write_sft(min_quality: u32, only_success: bool) -> Result<(PathBuf, usize, usize)> {
    let dir = x0ne_home().join("dataset");
    fs::create_dir_all(&dir).context("creating dataset dir")?;
    let in_path = dir.join("trajectories.jsonl");
    let out_path = dir.join("sft_v1.jsonl");
    let f = File::open(&in_path).with_context(|| format!("opening {}", in_path.display()))?;
    let mut out = File::create(&out_path)?;

    let mut total = 0usize;
    let mut kept = 0usize;
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        let t: RawTraj = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        total += 1;
        if t.quality_score < min_quality {
            continue;
        }
        if only_success && t.outcome != "success" {
            continue;
        }
        if t.goal.trim().is_empty() || t.trajectory.is_empty() {
            continue;
        }
        let assistant = assistant_payload_for(&t.trajectory);
        let example = SftExample {
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: sft_system_prompt(),
                },
                ChatMessage {
                    role: "user",
                    content: t.goal.clone(),
                },
                ChatMessage {
                    role: "assistant",
                    content: assistant,
                },
            ],
            metadata: Some(json!({
                "session_id": t.session_id,
                "outcome": t.outcome,
                "quality_score": t.quality_score,
                "num_actions": t.trajectory.len(),
            })),
        };
        writeln!(out, "{}", serde_json::to_string(&example)?)?;
        kept += 1;
    }
    out.flush()?;
    Ok((out_path, kept, total))
}

fn write_dpo(min_quality: u32) -> Result<(PathBuf, usize, usize)> {
    let dir = x0ne_home().join("dataset");
    fs::create_dir_all(&dir).context("creating dataset dir")?;
    let in_path = dir.join("trajectories.jsonl");
    let out_path = dir.join("dpo_v1.jsonl");
    let f = File::open(&in_path)?;
    let mut out = File::create(&out_path)?;

    let mut pairs = 0usize;
    let mut sessions_seen = 0usize;
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        let t: RawTraj = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if t.quality_score < min_quality {
            continue;
        }
        sessions_seen += 1;
        // Auto-mine retry pairs: walk consecutive steps; when we see a failed
        // action of type T followed soon-after by a successful action of the
        // same type T, treat (failed, succeeded) as a (rejected, chosen)
        // preference pair. The model learns "this is what a recovery looks like".
        let steps = &t.trajectory;
        for i in 0..steps.len() {
            if !steps[i].success || steps[i].action_type.is_empty() {
                continue;
            }
            // search backward up to 3 steps for a failed call of same type
            let lookback = i.saturating_sub(3);
            for j in lookback..i {
                if !steps[j].success && steps[j].action_type == steps[i].action_type {
                    let example = DpoExample {
                        prompt: format!(
                            "{} (after a failed {} attempt, what's the corrected JSON?)",
                            t.goal, steps[i].action_type
                        ),
                        chosen: steps[i].action.to_string(),
                        rejected: steps[j].action.to_string(),
                        metadata: Some(json!({
                            "session_id": t.session_id,
                            "action_type": steps[i].action_type,
                            "step_indices": [j, i],
                        })),
                    };
                    writeln!(out, "{}", serde_json::to_string(&example)?)?;
                    pairs += 1;
                    break;
                }
            }
        }
    }
    out.flush()?;
    Ok((out_path, pairs, sessions_seen))
}

pub fn run_sft(min_quality: u32, only_success: bool) -> Result<()> {
    let (path, kept, total) = write_sft(min_quality, only_success)?;
    eprintln!(
        "[sft] wrote {} ({} kept of {} total)",
        path.display(),
        kept,
        total
    );
    eprintln!("[sft] feed this into axolotl/unsloth/TRL with --dataset_format=chatml");
    Ok(())
}

pub fn run_dpo(min_quality: u32) -> Result<()> {
    let (path, pairs, sessions) = write_dpo(min_quality)?;
    eprintln!(
        "[dpo] wrote {} ({} pairs auto-mined from {} sessions)",
        path.display(),
        pairs,
        sessions
    );
    eprintln!("[dpo] feed this into TRL DPOTrainer; chosen=success retry, rejected=initial fail");
    Ok(())
}
