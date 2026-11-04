//! Slash command handling — /quit, /help, /check, /target, etc.

use chrono::Utc;
use colored::Colorize;
use serde_json::Value;

use crate::api::UsageTracker;
use crate::api::{self, LlmBackend};
use crate::bridge;
use crate::config::{Config, OpenAICompatConfig};
use crate::conversation::Conversation;
use crate::goal::{
    AchievementCheck, Budget, GoalManager, GoalState, GoalStatus, HistoryKind, SkepticCadence,
};
use crate::input_queue;
use crate::llm_setup;
use crate::pulse;
use crate::setup::SetupWizard;
use crate::terminal::*;
use crate::worlds_cli;
use crate::worlds_registry;

// ═══════════════════════════════════════════════════════════════════════════
//  Slash result
// ═══════════════════════════════════════════════════════════════════════════

pub enum SlashResult {
    Handled,
    Quit,
    /// `/clear` — the frontend must clear the canonical transcript *and*
    /// revoke standing approvals, which only the session can do.
    ClearSession,
    /// `/resume <id|last>` — the frontend must reopen the journal, reload
    /// the transcript into the session and rebind its writer. Only the
    /// frontend can: it owns the `SharedJournal`, and only the session can
    /// revoke the standing approvals a resume must drop. Carries the id the
    /// operator typed, unvalidated — `session_store::validate_session_id`
    /// runs at the frontend, where the id becomes a path.
    Resume(String),
    /// `/rename <title>` — the frontend appends a `session_renamed` event to
    /// the journal it owns. The journal is hash-chained: a title is added,
    /// no line is rewritten.
    Rename(String),
    /// Hot-swap the LLM backend (returned by `/model <name>`).
    SwitchModel(Box<dyn LlmBackend>),
    /// `/spawn N <prompt>` — the frontend must start the worlds through the
    /// session, so the actions cross the same gates as a model's `SpawnWorld`.
    SpawnWorlds(worlds_cli::SpawnRequest),
    /// Benchmark: run the same prompt through multiple models sequentially
    /// using the full agent loop. `(models, prompt)`
    Benchmark(Vec<String>, String),
    /// `/plan` (`true`) or `/build` (`false`): the session switches, after
    /// the runtime does (`CliAgentSession::set_plan_mode`).
    SetPlanMode(bool),
    #[allow(dead_code)] // defensive variant
    NotSlash,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Command dispatch
// ═══════════════════════════════════════════════════════════════════════════

/// Normalize raw slash-command input before matching. Pasted text can
/// carry characters that are invisible in the terminal but fatal to the
/// exact `match`: BOM / zero-width / word-joiner chars make "/actions"
/// render fine yet match nothing, and NBSP-style spaces defeat the
/// verb/arg split. (TODO.md: `/actions` → `Unknown command: /ions`.)
fn normalize_slash_input(raw: &str) -> String {
    raw.chars()
        .filter_map(|c| match c {
            // Unicode space variants → plain space so splitn(' ') works.
            c if c.is_whitespace() => Some(' '),
            // Zero-width + format chars and the soft hyphen: drop.
            '\u{feff}' | '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{00ad}' => None,
            // Other control chars (paste artifacts, stray escapes): drop.
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Closest known command for an unknown verb, for "did you mean" help.
/// Plain Levenshtein with a distance cap — the command list is tiny.
fn suggest_command(verb: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0; b.len() + 1];
        for (i, ca) in a.iter().enumerate() {
            cur[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let sub = prev[j] + usize::from(ca != cb);
                cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }
    let verb_lower = verb.to_lowercase();
    crate::cli_helper::commands()
        .map(|c| (c, distance(&verb_lower, c)))
        .filter(|(_, d)| *d <= 3)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

pub async fn handle_slash_command(
    cmd: &str,
    history: &mut crate::agent::history::History,
    bridge: &mut Box<dyn bridge::Bridge>,
    usage: &UsageTracker,
    config: &mut Config,
    // The running session's id, so `/session` can name what `--resume`
    // takes and `/sessions` can mark the current row. `None` on a surface
    // with no journal.
    session_id: Option<&str>,
    // The session's undo points, filled by the dispatcher.
    undo: &mut crate::agent::undo::UndoJournal,
) -> SlashResult {
    let cleaned = normalize_slash_input(cmd);
    let parts: Vec<&str> = cleaned.splitn(2, ' ').collect();
    let verb = parts[0];
    let arg = parts.get(1).copied().unwrap_or("").trim();

    match verb {
        "/quit" | "/exit" | "/q" => {
            println!("\n  {}\n", "session closed".dimmed());
            return SlashResult::Quit;
        }
        // Not `history.clear()` here. Clearing the conversation also revokes
        // the session's standing approvals, and the authority lives on the
        // session, not on the transcript — so the command reports what the
        // user asked for and the session performs it. A dispatcher that
        // reached into approvals would be a second place authority is
        // decided.
        "/clear" => return SlashResult::ClearSession,
        // The session switches, and the runtime before it: a dispatcher that
        // flipped a flag here would be plan mode held by the prompt alone.
        "/plan" => return SlashResult::SetPlanMode(true),
        "/build" => return SlashResult::SetPlanMode(false),
        "/history" => print_history(history.conversation()),
        "/expand" | "/x" | "/unfold" => print_full_history(history.conversation()),
        "/help" => print_help(),
        "/auth" => {
            // `/auth` alone = status panel. `/auth <arg>` = shortcut into
            // a concrete provider. Tab-completion exposes the surface
            // (see cli_helper.rs).
            let arg_lower = arg.trim().to_lowercase();
            match arg_lower.as_str() {
                "" => print_auth_status(),
                "setup" => {
                    if let Err(e) = SetupWizard::run() {
                        print_error(&format!("setup error: {}", e));
                    }
                }
                "anthropic" | "openai" | "openai-compat" | "ollama" => {
                    return handle_provider_switch(&arg_lower, usage, config).await;
                }
                other => {
                    print_error(&format!(
                        "Unknown /auth subcommand: {}. Try /auth, /auth anthropic, /auth openai, /auth openai-compat, /auth ollama, /auth setup.",
                        other,
                    ));
                }
            }
        }
        "/model" => return handle_model_switch(arg, usage, config).await,
        "/provider" => return handle_provider_switch(arg, usage, config).await,
        "/save_config" | "/save-config" => {
            let path = if arg.is_empty() {
                Config::default_user_config_path()
            } else {
                std::path::PathBuf::from(arg)
            };
            match config.save(&path) {
                Ok(()) => println!("  {} config saved to {}", "✓".green(), path.display()),
                Err(e) => print_error(&format!("save config: {}", e)),
            }
        }
        "/cost" => print_cost(usage, config),
        "/handles" => print_handles(history.conversation()),
        "/queue" => handle_queue(arg),
        "/session" => print_session(history.conversation(), usage, session_id),
        "/sessions" => {
            let project = match arg.trim() {
                "--all" | "all" => None,
                _ => crate::session_store::Project::current(),
            };
            print_sessions(
                &crate::session_store::list_for(
                    &crate::session_store::sessions_dir(),
                    project.as_ref(),
                ),
                session_id,
            )
        }
        // The frontend appends it to its journal; see `SlashResult::Rename`.
        "/rename" => match rename_title(arg) {
            Ok(title) => return SlashResult::Rename(title),
            Err(why) => print_error(why),
        },
        // The frontend performs it; see `SlashResult::Resume`.
        "/resume" => {
            if arg.is_empty() {
                print_error("/resume requires a session id, or `last` — see /sessions");
                return SlashResult::Handled;
            }
            return SlashResult::Resume(arg.to_string());
        }
        // Kept, and said for what it is: the gate ignores the flag.
        "/auto" => {
            config.ui.auto_confirm = !config.ui.auto_confirm;
            println!(
                "  {} auto-confirm: {}",
                "●".cyan(),
                auto_state_line(config.ui.auto_confirm).bold()
            );
        }
        "/compact" => {
            config.ui.compact_prompt = !config.ui.compact_prompt;
            let state = if config.ui.compact_prompt {
                "ON (compact)"
            } else {
                "OFF (full)"
            };
            println!("  {} prompt mode: {}", "●".cyan(), state.bold());
        }
        "/goal" => handle_goal(arg).await,
        "/check" => {
            if arg.is_empty() {
                print_error("/check requires a handle ID");
                return SlashResult::Handled;
            }
            let action = serde_json::json!({"type": "CheckHandle", "handle": arg});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&result)
                            .unwrap_or_else(|_| result.to_string())
                    );
                    history.touch_handle(arg);
                    let status = result
                        .pointer("/output/status/state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status == "completed" || status == "cancelled" || status == "failed" {
                        history.remove_handle(arg);
                    }
                }
                Err(e) => print_error(&format!("CheckHandle failed: {}", e)),
            }
        }
        "/stop" => {
            if arg.is_empty() {
                print_error("/stop requires a handle ID");
                return SlashResult::Handled;
            }
            let action = serde_json::json!({"type": "StopHandle", "handle": arg});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    let ok = result["success"].as_bool().unwrap_or(false);
                    if ok {
                        println!("  {} handle {} stopped", "✓".green(), arg);
                        history.remove_handle(arg);
                    } else {
                        print_error(&format!(
                            "StopHandle failed: {:?}",
                            result.pointer("/error/message")
                        ));
                    }
                }
                Err(e) => print_error(&format!("StopHandle failed: {}", e)),
            }
        }
        "/snapshot" => {
            if arg.is_empty() {
                print_error("/snapshot requires a name");
                return SlashResult::Handled;
            }
            let action = serde_json::json!({"type": "CreateSnapshot", "name": arg, "paths": ["."]});
            match bridge.send_action(&action).await {
                Ok(result) => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
                ),
                Err(e) => print_error(&format!("CreateSnapshot failed: {}", e)),
            }
        }
        "/restore" => {
            if arg.is_empty() {
                print_error("/restore requires a name");
                return SlashResult::Handled;
            }
            let action = serde_json::json!({"type": "RestoreSnapshot", "name": arg});
            match bridge.send_action(&action).await {
                Ok(result) => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
                ),
                Err(e) => print_error(&format!("RestoreSnapshot failed: {}", e)),
            }
        }
        "/actions" => {
            let action = serde_json::json!({"type": "ListActions"});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    if let Some(entries) =
                        result.pointer("/output/actions").and_then(|v| v.as_array())
                    {
                        println!(
                            "\n  {} available actions:",
                            entries.len().to_string().bold()
                        );
                        for entry in entries {
                            let name = entry["type"].as_str().unwrap_or("?");
                            let cat = entry["category"].as_str().unwrap_or("");
                            println!("  {:30} {}", name.cyan(), cat.dimmed());
                        }
                    } else {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&result)
                                .unwrap_or_else(|_| result.to_string())
                        );
                    }
                }
                Err(e) => print_error(&format!("ListActions failed: {}", e)),
            }
        }
        "/policy" => {
            if arg.is_empty() {
                print_error("/policy requires a JSON action");
                return SlashResult::Handled;
            }
            match serde_json::from_str::<Value>(arg) {
                Ok(inner) => {
                    let action = serde_json::json!({"type": "PolicyCheck", "action": inner});
                    match bridge.send_action(&action).await {
                        Ok(result) => println!(
                            "{}",
                            serde_json::to_string_pretty(&result)
                                .unwrap_or_else(|_| result.to_string())
                        ),
                        Err(e) => print_error(&format!("PolicyCheck failed: {}", e)),
                    }
                }
                Err(e) => print_error(&format!("Invalid JSON: {}", e)),
            }
        }
        "/pulse" => {
            pulse::run_pulse(bridge).await;
        }

        // ── Parallel worlds ────────────────────────────────────────────────
        // `/worlds` prints the measured table (`watch`: live view, `diff`,
        // `pick`, `drop`); `/spawn N <prompt>` is
        // parsed here and started by the frontend, which owns the session.
        "/worlds" => {
            if arg.trim() == "watch" {
                if let Err(e) = worlds_cli::render_live().await {
                    print_error(&format!("/worlds failed: {}", e));
                }
            } else {
                print!("{}", worlds_cli::worlds_command(arg));
            }
        }
        "/spawn" => match worlds_cli::parse_spawn(arg) {
            Ok(request) => return SlashResult::SpawnWorlds(request),
            Err(usage) => print_error(&usage),
        },
        "/worlds-reset" => {
            worlds_registry::global().reset();
            println!("  {}  worlds registry cleared", "⫸".truecolor(0, 200, 220));
        }

        // ── Artifacts (web artifacts served by x0ne-artifact-proxy) ───────────
        // Subcommands: open <id>, save <id> [name], stop <id>, rm <id> [-f].
        // Bare `/artifact` prints the listing.
        "/artifact" | "/artifacts" => {
            handle_artifact(arg, bridge).await;
        }
        "/target" => {
            if arg.is_empty() {
                println!(
                    "  {} current target: {}",
                    "●".cyan(),
                    config.bridge.mode.bold()
                );
                return SlashResult::Handled;
            }
            println!("  {} switching target to {}...", "●".cyan(), arg.bold());
            let old_mode = std::mem::replace(&mut config.bridge.mode, "sandbox".to_string());
            let old_cmd = config.bridge.sandbox_cmd.clone();

            if arg == "local" {
                config.bridge.mode = "local".to_string();
            } else if arg == "sandbox" {
                config.bridge.mode = "sandbox".to_string();
            } else if arg.starts_with("ssh ") {
                config.bridge.mode = "sandbox".to_string();
                config.bridge.sandbox_cmd = Some(arg.to_string());
            } else {
                print_error("Invalid target. Use local, sandbox, or an 'ssh ...' command.");
                return SlashResult::Handled;
            }

            match crate::bridge::create_bridge_for(&config.bridge).await {
                Ok(new_b) => {
                    *bridge = new_b;
                    println!("  {} switched successfully", "✓".green());
                }
                Err(e) => {
                    print_error(&format!("Failed to connect: {}", e));
                    config.bridge.mode = old_mode;
                    config.bridge.sandbox_cmd = old_cmd;
                }
            }
        }
        "/read" => {
            if arg.is_empty() {
                print_error("/read requires a file path");
                return SlashResult::Handled;
            }
            // Read as `@file` reads: never through a link out of the folder.
            let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
            match crate::project::read_reference(&root, arg) {
                crate::project::Reference::Text { shown, text } => {
                    history.add_user(&format!("File {}:\n```\n{}\n```", shown, text));
                    let shown = crate::text_safety::sanitize(&shown);
                    println!("  {} {} injected ({}B)", "✓".green(), shown, text.len());
                }
                crate::project::Reference::Refused(why) => print_error(&format!(
                    "read refused: {}: {}",
                    arg,
                    crate::text_safety::sanitize(&why)
                )),
                crate::project::Reference::Absent => {
                    print_error(&format!("read failed: {} — not a regular file", arg))
                }
                crate::project::Reference::Unreadable => {
                    print_error(&format!("read failed: {} — unreadable", arg))
                }
            }
        }
        "/setup" => {
            // Re-entrant menu when there is already a config to show; the
            // first-run wizard only when there is nothing at all. Coming back
            // to change one setting must not re-walk (and re-template) the
            // whole thing.
            let first_run = !Config::default_user_config_path().exists();
            let result = if first_run {
                SetupWizard::run()
            } else {
                crate::setup_menu::run()
            };
            if let Err(e) = result {
                print_error(&format!("setup error: {}", e));
            }
            return SlashResult::Handled;
        }

        // ── Project memory ───────────────────────────────────────────────────
        // `/remember <text>` appends to X0NE.md in the current directory.
        // Use it to persist discoveries the agent makes during a session so
        // the next session starts with context already loaded.
        //
        // The agent can also call WriteFile directly on X0NE.md — /remember
        // is the ergonomic shortcut for the human at the keyboard.
        "/remember" | "/mem" => {
            if arg.is_empty() {
                // No arg → show current X0NE.md content
                let candidates = [
                    std::path::PathBuf::from("X0NE.md"),
                    std::path::PathBuf::from(".x0ne/X0NE.md"),
                ];
                let found = candidates.iter().find(|p| p.exists());
                match found {
                    Some(p) => match std::fs::read_to_string(p) {
                        Ok(content) => println!("{}", content),
                        Err(e) => print_error(&format!("read X0NE.md: {}", e)),
                    },
                    None => println!(
                        "  {} no X0NE.md found — use /remember <text> to create one",
                        "ℹ".truecolor(132, 142, 157)
                    ),
                }
                return SlashResult::Handled;
            }

            // Append to X0NE.md in cwd (create if absent)
            let path = std::path::PathBuf::from("X0NE.md");
            let ts = chrono::Utc::now().format("%Y-%m-%d");
            let entry = format!("\n<!-- {} -->\n{}\n", ts, arg.trim());
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(entry.as_bytes())
                }) {
                Ok(()) => println!("  {} saved to X0NE.md ({} chars)", "✓".green(), arg.len()),
                Err(e) => print_error(&format!("write X0NE.md: {}", e)),
            }
        }

        // ── Budget guard ─────────────────────────────────────────────────────
        // `/budget` shows the current cap; `/budget <n>` sets a new cap
        // for the running session without restarting.
        "/budget" => {
            if arg.is_empty() {
                let cap = config.cost.budget_usd_limit;
                if cap <= 0.0 {
                    println!(
                        "  {} budget guard: {} (set with /budget <usd>)",
                        "ℹ".truecolor(132, 142, 157),
                        "OFF".dimmed()
                    );
                } else {
                    println!(
                        "  {} budget cap: ${:.2} per session",
                        "●".truecolor(0, 200, 120),
                        cap
                    );
                }
            } else if arg == "off" || arg == "0" {
                config.cost.budget_usd_limit = 0.0;
                println!("  {} budget guard disabled", "●".cyan());
            } else {
                match arg.parse::<f64>() {
                    Ok(v) if v > 0.0 => {
                        config.cost.budget_usd_limit = v;
                        println!("  {} budget cap set to ${:.2}", "✓".green(), v);
                    }
                    _ => print_error(&format!(
                        "invalid budget '{}' — use a positive number or 'off'",
                        arg
                    )),
                }
            }
        }

        // ── Git workflow commands ─────────────────────────────────────────
        "/diff" => {
            let action = serde_json::json!({"type": "RunCommand", "command": "git", "args": ["diff", "--stat"]});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    let stdout = result
                        .pointer("/output/stdout")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if stdout.is_empty() {
                        println!("  {} working tree clean", "✓".green());
                    } else {
                        println!("{}", stdout);
                    }
                }
                Err(e) => print_error(&format!("git diff failed: {}", e)),
            }
        }
        "/log" => {
            let n = if arg.is_empty() { "20" } else { arg };
            let action = serde_json::json!({"type": "RunCommand", "command": "git", "args": ["log", "--oneline", format!("-{}", n)]});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    let stdout = result
                        .pointer("/output/stdout")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{}", stdout);
                }
                Err(e) => print_error(&format!("git log failed: {}", e)),
            }
        }
        "/commit" => {
            let msg = if arg.is_empty() {
                "x0ne: auto-commit"
            } else {
                arg
            };
            // git add -A
            let _ = bridge
                .send_action(&serde_json::json!({
                    "type": "RunCommand", "command": "git", "args": ["add", "-A"]
                }))
                .await;
            // git commit
            let action = serde_json::json!({
                "type": "RunCommand", "command": "git", "args": ["commit", "-m", msg]
            });
            match bridge.send_action(&action).await {
                Ok(result) => {
                    let stdout = result
                        .pointer("/output/stdout")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let exit_code = result
                        .pointer("/output/exit_code")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1);
                    if exit_code == 0 {
                        println!("  {} committed: {}", "✓".green(), msg);
                    } else {
                        println!("  {} commit failed:\n{}", "✗".red(), stdout);
                    }
                }
                Err(e) => print_error(&format!("git commit failed: {}", e)),
            }
        }
        "/connectors" => {
            // Third-party MCP servers: list, enable, disable, re-trust.
            let report = crate::connectors::slash(arg).await;
            println!("{}", crate::text_safety::sanitize_block(&report));
        }
        "/undo" => {
            // Restores the files of the agent's last write from the snapshot
            // taken before it. Never git: `git checkout -- .` destroyed
            // uncommitted work the agent had not made.
            let outcome = undo.undo_last(bridge.as_mut()).await;
            let line = crate::agent::undo::describe(&outcome);
            match outcome {
                crate::agent::undo::UndoOutcome::Restored { .. } => {
                    println!("  {} {}", "⟲".green(), line)
                }
                crate::agent::undo::UndoOutcome::NothingToUndo { .. } => {
                    println!("  {} {}", "ℹ".truecolor(132, 142, 157), line)
                }
                crate::agent::undo::UndoOutcome::Failed { .. } => print_error(&line),
            }
        }
        "/stash" => {
            let sub = if arg.is_empty() { "push" } else { arg };
            let args: Vec<&str> = sub.split_whitespace().collect();
            let mut cmd_args = vec!["stash"];
            cmd_args.extend(args);
            let action =
                serde_json::json!({"type": "RunCommand", "command": "git", "args": cmd_args});
            match bridge.send_action(&action).await {
                Ok(result) => {
                    let stdout = result
                        .pointer("/output/stdout")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{}", stdout);
                }
                Err(e) => print_error(&format!("git stash failed: {}", e)),
            }
        }

        "/export" => {
            let export_dir = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".x0ne")
                .join("exports");
            std::fs::create_dir_all(&export_dir).ok();
            let ts = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
            let filename = if arg.is_empty() {
                format!("session_{}.json", ts)
            } else {
                format!("{}_{}.json", arg.replace(' ', "_"), ts)
            };
            let path = export_dir.join(&filename);

            // Build export payload
            let messages: Vec<serde_json::Value> = history.messages().iter().map(|m| {
                serde_json::json!({
                    "role": match m.role { crate::conversation::Role::User => "user", crate::conversation::Role::Assistant => "assistant" },
                    "content": m.content,
                })
            }).collect();

            let export = serde_json::json!({
                "exported_at": ts,
                "model": &usage.model,
                "total_input_tokens": usage.total_input_tokens,
                "total_output_tokens": usage.total_output_tokens,
                "total_requests": usage.total_requests,
                "messages": messages,
            });

            match std::fs::write(
                &path,
                serde_json::to_string_pretty(&export).unwrap_or_default(),
            ) {
                Ok(_) => println!(
                    "  {}  exported {} messages → {}",
                    "✓".truecolor(0, 200, 120),
                    messages.len(),
                    path.display()
                ),
                Err(e) => print_error(&format!("export failed: {}", e)),
            }
        }

        "/shell" | "/sh" => {
            let is_sandbox =
                config.bridge.mode.contains("sandbox") || config.bridge.mode.contains("container");

            if is_sandbox {
                // Exec into the sandbox container
                if let Some(ref sandbox_cmd) = config.bridge.sandbox_cmd {
                    // sandbox_cmd is e.g. "incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode stdio"
                    // We want everything before "--" + "-- bash"
                    let prefix: Vec<&str> = sandbox_cmd
                        .split_whitespace()
                        .take_while(|w| *w != "--")
                        .collect();
                    if prefix.is_empty() {
                        print_error("Could not parse sandbox_cmd for shell access");
                        return SlashResult::Handled;
                    }
                    let exe = prefix[0];
                    let mut args: Vec<&str> = prefix[1..].to_vec();
                    args.push("--");
                    args.push("bash");

                    println!(
                        "  {} entering sandbox shell — type 'exit' to return to x0ne",
                        "⬡".truecolor(0, 200, 220)
                    );
                    println!(
                        "  {} {}",
                        "🛡".truecolor(200, 160, 0),
                        format!("{} {} -- bash", exe, args[..args.len() - 2].join(" "))
                            .truecolor(112, 124, 142)
                    );
                    println!("  {}", "─".repeat(44).truecolor(96, 104, 120));

                    let status = std::process::Command::new(exe)
                        .args(&args)
                        .stdin(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .stderr(std::process::Stdio::inherit())
                        .status();
                    match status {
                        Ok(s) => {
                            println!("  {}", "─".repeat(44).truecolor(96, 104, 120));
                            println!(
                                "  {} back to x0ne (sandbox shell exited {})",
                                "⬡".truecolor(0, 200, 220),
                                if s.success() {
                                    "cleanly".dimmed().to_string()
                                } else {
                                    format!("code {}", s.code().unwrap_or(-1)).red().to_string()
                                }
                            );
                        }
                        Err(e) => print_error(&format!("sandbox shell failed: {}", e)),
                    }
                } else {
                    print_error("No sandbox_cmd configured — can't shell into sandbox");
                }
            } else {
                // Local mode — just open a shell
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
                println!(
                    "  {} dropping into {} — type 'exit' to return to x0ne",
                    "⬡".truecolor(0, 200, 220),
                    shell.truecolor(0, 180, 200).bold()
                );
                println!("  {}", "─".repeat(44).truecolor(96, 104, 120));
                let status = std::process::Command::new(&shell)
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();
                match status {
                    Ok(s) => {
                        println!("  {}", "─".repeat(44).truecolor(96, 104, 120));
                        println!(
                            "  {} back to x0ne (shell exited {})",
                            "⬡".truecolor(0, 200, 220),
                            if s.success() {
                                "cleanly".dimmed().to_string()
                            } else {
                                format!("code {}", s.code().unwrap_or(-1)).red().to_string()
                            }
                        );
                    }
                    Err(e) => print_error(&format!("shell failed: {}", e)),
                }
            }
        }

        "/benchmark" | "/bench" => {
            return handle_benchmark(arg, config).await;
        }

        _ => match suggest_command(verb) {
            Some(s) => print_error(&format!(
                "Unknown command: {}. Did you mean {}? Type /help for a list.",
                verb, s,
            )),
            None => print_error(&format!(
                "Unknown command: {}. Type /help for a list.",
                verb
            )),
        },
    }

    SlashResult::Handled
}

// ═══════════════════════════════════════════════════════════════════════════
//  Model hot-swap
// ═══════════════════════════════════════════════════════════════════════════

/// The list exactly as `/model list` last displayed it. `/model <N>` resolves
/// against THIS, never against a re-fetch: the provider can return a different
/// set or order between two calls (live API one moment, static fallback the
/// next), and then index 3 silently names a different model than the one the
/// user read on screen — the 2026-08-10 empty-response session.
static DISPLAYED_MODEL_LIST: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// One-shot flag: `/model list` was the previous command, so a bare number
/// typed at the prompt is a selection, not a message for the model. The
/// session logs show the owner typing `3` after the list — twice — and the
/// digit going to the LLM as prose.
static MODEL_LIST_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn remember_displayed_models(models: &[String]) {
    *DISPLAYED_MODEL_LIST.lock().unwrap() = models.to_vec();
    MODEL_LIST_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn displayed_models() -> Vec<String> {
    DISPLAYED_MODEL_LIST.lock().unwrap().clone()
}

/// Called by the REPL on every non-slash input. Returns `true` (consuming the
/// pending state) when the input is a bare index into the list the user is
/// currently looking at; any other input clears the pending state, so a lone
/// number typed later in conversation is never hijacked.
pub fn take_bare_model_selection(input: &str) -> bool {
    use std::sync::atomic::Ordering;
    let is_index =
        !input.is_empty() && input.len() <= 3 && input.chars().all(|c| c.is_ascii_digit());
    if MODEL_LIST_PENDING.swap(false, Ordering::SeqCst) {
        is_index
    } else {
        false
    }
}

/// Handle `/model [name|list]`:
/// - no arg  → print active model
/// - `list`  → fetch + display available models from the active provider
/// - `<name>`→ switch to that model (hot-swap the LLM backend)
/// What `/auto` prints for the new state of `ui.auto_confirm`. The flag
/// shows the `auto` badge and is saved by `/save_config`; the approval gate
/// neutralises it (`confirm::requires_approval`), measured 2026-10-27.
fn auto_state_line(on: bool) -> String {
    if on {
        "ON (badge only) — every write still asks for approval; \
         [trust] roots in your user config waive it"
            .to_string()
    } else {
        "OFF — every write asks for approval".to_string()
    }
}

async fn handle_model_switch(arg: &str, usage: &UsageTracker, config: &mut Config) -> SlashResult {
    // ── No argument: show current model ──────────────────────────────────
    if arg.is_empty() {
        println!(
            "  {} {}  {}",
            "●".truecolor(0, 200, 220),
            usage.model.truecolor(0, 200, 220).bold(),
            format!("({})", config.llm.provider).dimmed(),
        );
        println!(
            "  {}  /model list    — show available models",
            "ℹ".truecolor(132, 142, 157)
        );
        println!(
            "  {}  /model <name>  — switch model",
            "ℹ".truecolor(132, 142, 157)
        );
        return SlashResult::Handled;
    }

    // ── `list`: fetch available models from the active provider ──────────
    if arg == "list" {
        let models = fetch_provider_models(config).await;
        if models.is_empty() {
            print_error("Could not fetch model list from the active provider.");
            print_local_servers(config).await;
            return SlashResult::Handled;
        }
        println!(
            "\n  {} available models ({})",
            models.len().to_string().bold(),
            config.llm.provider.dimmed()
        );
        for (i, m) in models.iter().enumerate() {
            let current = is_current_model(m, config);
            let marker = if current {
                " ◂".truecolor(0, 200, 120).to_string()
            } else {
                String::new()
            };
            println!(
                "  {} {}{}",
                format!("[{}]", i + 1).cyan(),
                if current {
                    m.truecolor(0, 200, 120).bold().to_string()
                } else {
                    m.to_string()
                },
                marker,
            );
        }
        println!(
            "\n  {}  /model <name or number> to switch (or just type the number)\n",
            "ℹ".truecolor(132, 142, 157)
        );
        remember_displayed_models(&models);
        print_local_servers(config).await;
        return SlashResult::Handled;
    }

    // ── Resolve model name (number → name from list) ────────────────────
    let target_model = if let Ok(idx) = arg.parse::<usize>() {
        // A number is an index into the list the user last SAW. Only when no
        // list was ever displayed this session do we fetch one — and that
        // fetch then becomes the reference the index resolves against.
        let mut models = displayed_models();
        if models.is_empty() {
            models = fetch_provider_models(config).await;
            remember_displayed_models(&models);
            MODEL_LIST_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        if idx >= 1 && idx <= models.len() {
            models[idx - 1].clone()
        } else {
            print_error(&format!(
                "Invalid model index {}. Use /model list to see available models.",
                idx
            ));
            return SlashResult::Handled;
        }
    } else {
        arg.to_string()
    };

    // ── Update config and create new backend ────────────────────────────
    println!(
        "  {} switching to {}…",
        "●".truecolor(0, 200, 220),
        target_model.truecolor(0, 200, 220).bold(),
    );

    // Mutate the active provider's model field. For openai-compat we ALSO
    // sync the change to the saved compat profile that shares this name
    // (if any), so a future `/provider <name>` doesn't pull the stale
    // model back when the user switches between profiles.
    match config.llm.provider.as_str() {
        "ollama" => config.llm.ollama.model = target_model.clone(),
        "openai-compat" | "openai_compat" | "litellm" => {
            config.llm.openai_compat.model = target_model.clone();
            if let Some(name) = config.llm.openai_compat.name.clone() {
                if let Some(slot) = config
                    .llm
                    .compat_profiles
                    .iter_mut()
                    .find(|p| p.name.as_deref() == Some(name.as_str()))
                {
                    slot.model = target_model.clone();
                }
            }
        }
        "openai" => config.llm.openai.model = target_model.clone(),
        _ => config.llm.anthropic.model = target_model.clone(),
    }

    let backend = match api::create_backend(&config.llm, None) {
        Ok(b) => b,
        Err(e) => {
            print_error(&format!("Failed to create backend: {}", e));
            return SlashResult::Handled;
        }
    };

    // Persist so the choice survives the next `x0ne` invocation. Save
    // failure is non-fatal — the in-memory swap already worked, the user
    // just won't keep the choice on restart.
    let path = Config::default_user_config_path();
    if let Err(e) = config.save(&path) {
        eprintln!(
            "  {} could not save config to {}: {}",
            "⚠".truecolor(200, 160, 0),
            path.display(),
            e
        );
        println!(
            "  {} model switched to {} {}",
            "✓".green(),
            backend.display_name().truecolor(0, 200, 220).bold(),
            "(in-memory only)".dimmed(),
        );
    } else {
        println!(
            "  {} model switched to {} {}",
            "✓".green(),
            backend.display_name().truecolor(0, 200, 220).bold(),
            format!("(saved to {})", path.display()).dimmed(),
        );
    }
    SlashResult::SwitchModel(backend)
}

// ═══════════════════════════════════════════════════════════════════════════
//  /provider — switch LLM provider (anthropic / ollama / openai / openai-compat)
// ═══════════════════════════════════════════════════════════════════════════
//
// `/provider`                                    show current
// `/provider list`                               show available + active
// `/provider <name>`                             switch to a provider (uses
//                                                stored config for that name)
// `/provider openai-compat <url> <model> [key]`  set URL + model (+ optional
//                                                key) and switch in one shot
// `/provider key <api_key>`                      set the API key for the
//                                                current provider's compat
//                                                slot (writes to config)
//
// On success the call returns `SlashResult::SwitchModel(backend)` so the
// agent loop hot-swaps the LLM client without restarting the session.
// Persists the config to `~/.x0ne/config.toml` (best effort) so the next
// `x0ne` invocation picks up the new provider.

async fn handle_provider_switch(
    arg: &str,
    _usage: &UsageTracker,
    config: &mut Config,
) -> SlashResult {
    let trimmed = arg.trim();

    // No arg → show current.
    if trimmed.is_empty() {
        return print_provider_status(config);
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let head = parts[0].to_ascii_lowercase();

    if head == "list" {
        return print_provider_list(config);
    }

    if head == "key" {
        if parts.len() < 2 {
            print_error("/provider key <api-key> — missing key argument");
            return SlashResult::Handled;
        }
        let key = parts[1..].join(" ");
        return set_provider_key(&key, config).await;
    }

    // A local preset: `/provider llamacpp [model]`, `/provider lmstudio
    // [model]`. A saved profile of that name is reused when no model is given.
    if let Some(preset) = crate::local_models::preset_for(&head) {
        let saved = config
            .llm
            .compat_profiles
            .iter()
            .any(|p| p.name.as_deref() == Some(preset.name));
        if parts.len() > 1 || !saved {
            return switch_to_local_preset(preset, parts.get(1).copied(), config).await;
        }
    }

    // First, try to interpret the head as a *saved compat profile name*.
    // This is the typical fast path: `/provider kimi`, `/provider r9s`.
    if let Some(profile) = config
        .llm
        .compat_profiles
        .iter()
        .find(|p| {
            p.name
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(&head))
                .unwrap_or(false)
        })
        .cloned()
    {
        return activate_compat_profile(profile, config).await;
    }

    // Otherwise treat the head as a canonical provider name.
    let provider = match normalize_provider_name(&head) {
        Some(p) => p,
        None => {
            print_error(&format!(
                "unknown provider '{}'. valid canonical names: anthropic, ollama, openai, openai-compat; local presets: llamacpp, lmstudio. Or pass a saved profile name (see /provider list).",
                head
            ));
            return SlashResult::Handled;
        }
    };

    // For openai-compat with positional args, we're either UPSERTING a new
    // profile (or updating an existing one with the same auto-derived name)
    // and switching to it. The profile is named explicitly with the 5th
    // arg if provided, otherwise auto-derived from the URL hostname.
    //   /provider openai-compat <url> <model> <key> [name]
    if provider == "openai-compat" && parts.len() >= 2 {
        let url = parts.get(1).copied().unwrap_or("").to_string();
        let model = parts.get(2).copied().unwrap_or("").to_string();
        let key = parts.get(3).copied().unwrap_or("").to_string();
        let name = parts
            .get(4)
            .map(|s| s.to_string())
            .unwrap_or_else(|| crate::config::derive_compat_name(&url));

        // Build / merge the profile in memory.
        let mut profile = config
            .llm
            .compat_profiles
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str()))
            .cloned()
            .unwrap_or_else(|| {
                let mut p = config.llm.openai_compat.clone();
                p.name = Some(name.clone());
                p
            });
        profile.name = Some(name.clone());
        if !url.is_empty() {
            profile.base_url = url;
        }
        if !model.is_empty() {
            profile.model = model;
        }
        if !key.is_empty() {
            profile.api_key = Some(key);
        }
        upsert_compat_profile(config, profile.clone());
        return activate_compat_profile(profile, config).await;
    }

    // Anthropic: one credential path now (pay-per-token API key).
    if provider == "anthropic" {
        let set = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
        let apikey_present = set("ANTHROPIC_API_KEY") || set(config.llm.anthropic.key_var());
        if !apikey_present {
            // Ask for the key here, rather than sending the user to a
            // setup that never asked for it.
            return match crate::llm_setup::ask_and_connect(config).await {
                Ok(b) => SlashResult::SwitchModel(b),
                Err(e) => {
                    print_error(&format!("{e:#}"));
                    SlashResult::Handled
                }
            };
        }
        config.llm.provider = "anthropic".to_string();
        return persist_and_swap(config, "provider switched to anthropic".to_string()).await;
    }

    // Pure provider switch with no args: switch the canonical provider.
    config.llm.provider = provider.to_string();
    persist_and_swap(config, format!("provider switched to {}", provider)).await
}

/// Insert or update a compat profile in `compat_profiles` keyed by name.
fn upsert_compat_profile(config: &mut Config, profile: OpenAICompatConfig) {
    let name = match profile.name.as_deref() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return, // unnamed profiles are not stored in the list
    };
    if let Some(slot) = config
        .llm
        .compat_profiles
        .iter_mut()
        .find(|p| p.name.as_deref() == Some(name.as_str()))
    {
        *slot = profile;
    } else {
        config.llm.compat_profiles.push(profile);
    }
}

/// Make a profile active: copy it into `openai_compat`, set
/// `provider = "openai-compat"`, persist + hot-swap.
async fn activate_compat_profile(profile: OpenAICompatConfig, config: &mut Config) -> SlashResult {
    let name = profile.name.clone().unwrap_or_else(|| "compat".into());
    let url = profile.base_url.clone();
    config.llm.openai_compat = profile;
    config.llm.provider = "openai-compat".into();
    persist_and_swap(
        config,
        format!(
            "provider switched to openai-compat/{} @ {}",
            name,
            short_origin(&url)
        ),
    )
    .await
}

async fn persist_and_swap(config: &mut Config, success_msg: String) -> SlashResult {
    let backend = match api::create_backend(&config.llm, None) {
        Ok(b) => b,
        Err(e) => {
            print_error(&format!("could not initialise backend: {}", e));
            return SlashResult::Handled;
        }
    };
    let path = Config::default_user_config_path();
    if let Err(e) = config.save(&path) {
        eprintln!(
            "  {} could not save config to {}: {}",
            "⚠".truecolor(200, 160, 0),
            path.display(),
            e
        );
    } else {
        println!(
            "  {} {} {}",
            "✓".green(),
            success_msg.truecolor(0, 200, 220).bold(),
            format!("(saved to {})", path.display()).dimmed(),
        );
    }
    let _ = backend;
    // Re-create AFTER save so display_name shows the post-save state.
    match api::create_backend(&config.llm, None) {
        Ok(b) => SlashResult::SwitchModel(b),
        Err(e) => {
            print_error(&format!("backend init failed: {}", e));
            SlashResult::Handled
        }
    }
}

/// Short "scheme://host" extract for log messages — e.g. "https://api.r9s.ai".
/// A userinfo is shown as `***@`.
fn short_origin(url: &str) -> String {
    let url = crate::redact::url_for_display(url);
    let url = url.as_str();
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let after_scheme = &url[scheme_end..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    format!("{}{}", &url[..scheme_end], &after_scheme[..host_end])
}

/// `/provider llamacpp|lmstudio [model]`: the preset's default loopback
/// address as a named openai-compat profile. Without a model the server is
/// asked (loopback only, `local_models`); its only model is taken, several
/// are listed for the user to pick from.
async fn switch_to_local_preset(
    preset: &'static crate::local_models::Preset,
    model: Option<&str>,
    config: &mut Config,
) -> SlashResult {
    use crate::local_models as local;
    let candidates: Vec<local::Preset> = local::PRESETS
        .iter()
        .filter(|p| p.name == preset.name)
        .copied()
        .collect();
    match local::choose(preset, model, &candidates).await {
        Ok(profile) => {
            upsert_compat_profile(config, profile.clone());
            activate_compat_profile(profile, config).await
        }
        Err(lines) => {
            for line in lines {
                println!("  {line}");
            }
            SlashResult::Handled
        }
    }
}

/// Ask the local presets (loopback only) and print what answered, the
/// server `config` already uses left out.
pub(crate) async fn print_local_servers(config: &Config) {
    use crate::local_models as local;
    let active = match config.llm.provider.as_str() {
        "ollama" => Some(config.llm.ollama.endpoint.as_str()),
        p if p.contains("compat") || p == "litellm" => {
            Some(config.llm.openai_compat.base_url.as_str())
        }
        _ => None,
    }
    .and_then(|root| local::loopback_root(root).ok());
    let found: Vec<local::Found> = local::discover(local::PRESETS, local::TIMEOUT)
        .await
        .into_iter()
        .filter(|f| local::loopback_root(f.preset.root).ok() != active)
        .collect();
    println!(
        "  {}",
        "── local servers (loopback only) ──".truecolor(132, 142, 157)
    );
    for line in local::report(&found) {
        println!("  {line}");
    }
    println!();
}

/// Longest `/rename` title accepted.
pub const MAX_TITLE_CHARS: usize = 120;

/// The title `/rename` was given, trimmed, or why it is refused.
fn rename_title(arg: &str) -> Result<String, &'static str> {
    let title = arg.trim();
    if title.is_empty() {
        return Err("usage: /rename <title>");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err("/rename: a title is 120 characters at most");
    }
    Ok(title.to_string())
}

/// Map common aliases to the canonical provider name used by `create_backend`.
fn normalize_provider_name(s: &str) -> Option<&'static str> {
    match s {
        "anthropic" | "claude" => Some("anthropic"),
        "ollama" => Some("ollama"),
        "openai" => Some("openai"),
        "openai-compat" | "openai_compat" | "litellm" | "compat" | "kimi" | "groq"
        | "openrouter" | "vllm" => Some("openai-compat"),
        // Removed subscription-OAuth path → coerce to the API-key
        // provider so a stale alias still resolves to something usable.
        "claude-max" | "anthropic-oauth" => Some("anthropic"),
        _ => None,
    }
}

fn print_provider_status(config: &Config) -> SlashResult {
    let model = match config.llm.provider.as_str() {
        "ollama" => &config.llm.ollama.model,
        "openai-compat" | "openai_compat" | "litellm" => &config.llm.openai_compat.model,
        "openai" => &config.llm.openai.model,
        _ => &config.llm.anthropic.model,
    };
    println!(
        "  {} {}  {}",
        "●".truecolor(0, 200, 220),
        config.llm.provider.truecolor(0, 200, 220).bold(),
        format!("({})", model).dimmed(),
    );
    if config.llm.provider.contains("compat") {
        println!(
            "  {}  base_url: {}",
            "·".dimmed(),
            crate::redact::url_for_display(&config.llm.openai_compat.base_url).dimmed()
        );
    }
    println!();
    println!(
        "  {}  /provider list                          — show all",
        "ℹ".truecolor(132, 142, 157)
    );
    println!(
        "  {}  /provider <name>                        — switch",
        "ℹ".truecolor(132, 142, 157)
    );
    println!(
        "  {}  /provider openai-compat <url> <model>   — set URL + model",
        "ℹ".truecolor(132, 142, 157)
    );
    println!(
        "  {}  /provider key <api-key>                 — set API key",
        "ℹ".truecolor(132, 142, 157)
    );
    SlashResult::Handled
}

fn print_provider_list(config: &Config) -> SlashResult {
    let active = config.llm.provider.as_str();
    let active_compat_name = config.llm.openai_compat.name.as_deref();
    let canonical = [
        ("anthropic", "Claude (Anthropic API)"),
        ("ollama", "Ollama (local LLMs)"),
        ("openai", "OpenAI"),
        (
            "openai-compat",
            "OpenAI-compatible base slot (LiteLLM, vLLM, raw …)",
        ),
    ];

    println!();
    println!("  {}", "── canonical providers ──".truecolor(132, 142, 157));
    for (name, desc) in canonical {
        let is_active = name == active
            || (active.contains("compat")
                && name == "openai-compat"
                && active_compat_name.is_none());
        let marker = if is_active {
            "◂".truecolor(0, 200, 120).to_string()
        } else {
            " ".to_string()
        };
        println!(
            "  {} {:16} {}",
            marker,
            name.cyan(),
            desc.truecolor(132, 142, 157)
        );
    }

    println!();
    println!(
        "  {}",
        "── local presets (/provider <name> [model]) ──".truecolor(132, 142, 157)
    );
    for p in crate::local_models::PRESETS
        .iter()
        .filter(|p| crate::local_models::preset_for(p.name).is_some_and(|q| q.root == p.root))
    {
        let place = format!("{} at {}", p.label, p.root.trim_start_matches("http://"));
        println!(
            "    {:16} {}",
            p.name.cyan(),
            place.truecolor(132, 142, 157)
        );
    }

    if !config.llm.compat_profiles.is_empty() {
        println!();
        println!(
            "  {}",
            "── saved openai-compat profiles ──".truecolor(132, 142, 157)
        );
        for profile in &config.llm.compat_profiles {
            let name = profile.name.as_deref().unwrap_or("(unnamed)");
            let is_active = active.contains("compat") && active_compat_name == Some(name);
            let marker = if is_active {
                "◂".truecolor(0, 200, 120).to_string()
            } else {
                " ".to_string()
            };
            let detail = format!(
                "{}  {}",
                profile.model.truecolor(0, 200, 220),
                short_origin(&profile.base_url).truecolor(132, 142, 157),
            );
            println!("  {} {:16} {}", marker, name.cyan(), detail);
        }
    } else {
        println!();
        println!(
            "  {}  add a profile with: {}",
            "ℹ".truecolor(132, 142, 157),
            "/provider openai-compat <url> <model> <api-key> [name]".cyan()
        );
    }
    println!();
    SlashResult::Handled
}

async fn set_provider_key(key: &str, config: &mut Config) -> SlashResult {
    // The "key" subcommand only makes sense for the openai-compat slot
    // today (Anthropic / OpenAI / Ollama use env vars exclusively). If the
    // user is on a non-compat provider, set it on the compat slot anyway
    // so a subsequent `/provider openai-compat …` finds the key already
    // in place.
    config.llm.openai_compat.api_key = Some(key.to_string());

    let path = Config::default_user_config_path();
    if let Err(e) = config.save(&path) {
        print_error(&format!(
            "could not save config to {}: {}",
            path.display(),
            e
        ));
        return SlashResult::Handled;
    }

    // If we're on openai-compat, hot-swap the backend now. Otherwise just
    // confirm the save and exit; user can `/provider openai-compat` later.
    if config.llm.provider.contains("compat") {
        match api::create_backend(&config.llm, None) {
            Ok(backend) => {
                println!(
                    "  {} api_key updated for {} {}",
                    "✓".green(),
                    backend.display_name().truecolor(0, 200, 220).bold(),
                    format!("(saved to {})", path.display()).dimmed(),
                );
                return SlashResult::SwitchModel(backend);
            }
            Err(e) => {
                print_error(&format!("key saved but backend failed: {}", e));
                return SlashResult::Handled;
            }
        }
    }

    println!(
        "  {} api_key saved for openai-compat slot {}",
        "✓".green(),
        format!("(in {})", path.display()).dimmed(),
    );
    println!(
        "  {}  switch to it via: {}",
        "ℹ".truecolor(132, 142, 157),
        "/provider openai-compat".cyan()
    );
    SlashResult::Handled
}

/// Ask the Anthropic Models API which models this account can actually use.
///
/// Returns an empty vec on any failure (no credentials, offline, non-200) so
/// the caller can fall back to its static list — a `/model` picker that fails
/// closed to nothing would be worse than one that is slightly stale.
async fn fetch_anthropic_models(config: &Config) -> Vec<String> {
    // The key provider anthropic would send, checked (`crate::model_keys`).
    let mut llm = config.llm.clone();
    llm.provider = "anthropic".into();
    let Ok(Some(key)) = crate::model_keys::resolve(&llm, None) else {
        return Vec::new();
    };

    let req = reqwest::Client::new()
        .get("https://api.anthropic.com/v1/models")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", key);

    let Ok(resp) = req.timeout(std::time::Duration::from_secs(5)).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    body["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch the list of available models from the current provider.
async fn fetch_provider_models(config: &Config) -> Vec<String> {
    match config.llm.provider.as_str() {
        "ollama" => {
            let endpoint = config.llm.ollama.endpoint.clone();
            llm_setup::list_ollama_models_pub(&endpoint)
                .await
                .unwrap_or_default()
        }
        "openai-compat" | "openai_compat" | "litellm" => {
            let cfg = &config.llm.openai_compat;
            // Strip trailing /v1 to avoid double /v1/v1/models
            let base = cfg.base_url.trim_end_matches('/').trim_end_matches("/v1");
            let url = format!("{}/v1/models", base);
            let Ok(client) = crate::api::client_for(&url).build() else {
                return Vec::new();
            };
            let mut req = client.get(&url);
            // The key the session sends, checked before any request.
            let key_val = match crate::model_keys::resolve(&config.llm, None) {
                Ok(k) => k,
                Err(e) => {
                    print_error(&format!("{e:#}"));
                    return Vec::new();
                }
            };
            if let Some(k) = &key_val {
                req = req.header("Authorization", format!("Bearer {}", k));
            }
            match req.timeout(std::time::Duration::from_secs(5)).send().await {
                Ok(resp) => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        let mut models: Vec<String> = body["data"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        models.sort();
                        models
                    } else {
                        Vec::new()
                    }
                }
                Err(_) => Vec::new(),
            }
        }
        "anthropic" => {
            // Ask the API what it actually serves. A hardcoded list goes stale
            // silently: it kept offering Opus 4.7 / Sonnet 4.6 long after Opus 5
            // and Sonnet 5 shipped, and a retired id in the *config* default
            // (claude-sonnet-4-20250514) surfaced only as an opaque 404.
            let live = fetch_anthropic_models(config).await;
            if !live.is_empty() {
                return live;
            }
            // Offline, no credentials, or the endpoint refused — fall back to
            // the aliases plus current flagships. Any name is still typeable
            // via `/model <name>`; it is forwarded verbatim and the API
            // reports it if wrong.
            vec![
                // Aliases resolve to the latest of each family server-side.
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
                // Current generation, for explicit pinning.
                "claude-opus-5".into(),
                "claude-sonnet-5".into(),
                // Previous generation, still served.
                "claude-opus-4-8".into(),
                "claude-sonnet-4-6".into(),
                "claude-haiku-4-5".into(),
            ]
            // NB: the old `claude-opus-4-7[1m]` / `claude-sonnet-4-6[1m]`
            // entries are gone rather than renamed. `[1m]` marked an opt-in
            // 1M-context variant; on the current generation a 1M window is the
            // default, so there is no `claude-opus-5[1m]` to point them at.
        }
        _ => Vec::new(),
    }
}

/// Check if a model name matches the currently active model for the provider.
fn is_current_model(model: &str, config: &Config) -> bool {
    let current = match config.llm.provider.as_str() {
        "ollama" => &config.llm.ollama.model,
        "openai-compat" | "openai_compat" | "litellm" => &config.llm.openai_compat.model,
        "openai" => &config.llm.openai.model,
        _ => &config.llm.anthropic.model,
    };
    model == current
        || model.trim_end_matches(":latest") == current.as_str()
        || format!("{}:latest", current) == model
}

// ═══════════════════════════════════════════════════════════════════════════
//  Benchmark — collect models + prompt, hand off to the main loop
// ═══════════════════════════════════════════════════════════════════════════

/// `/benchmark [prompt]` — interactive model picker, then returns
/// `SlashResult::Benchmark` so main.rs runs the full agent loop per model.
async fn handle_benchmark(arg: &str, config: &mut Config) -> SlashResult {
    // ── Provider check ───────────────────────────────────────────────────
    if config.llm.provider != "ollama" {
        print_error(
            "Benchmark currently only supports Ollama. Switch with /model or --provider ollama.",
        );
        return SlashResult::Handled;
    }

    let endpoint = config.llm.ollama.endpoint.clone();
    let shown = crate::redact::url_for_display(&endpoint);

    // ── Fetch models ─────────────────────────────────────────────────────
    println!(
        "  {} fetching models from {}…",
        "●".truecolor(0, 200, 220),
        shown.truecolor(132, 142, 157)
    );
    let models = llm_setup::list_ollama_models_pub(&endpoint)
        .await
        .unwrap_or_default();
    if models.is_empty() {
        print_error(&format!("No models found at {}. Is Ollama running?", shown));
        return SlashResult::Handled;
    }

    // Clear "fetching" line
    print!("\x1b[1A\x1b[2K");

    // ── Display model list ───────────────────────────────────────────────
    println!("\n  {} available models:", models.len().to_string().bold());
    for (i, m) in models.iter().enumerate() {
        let current = is_current_model(m, config);
        let marker = if current {
            " ◂".truecolor(0, 200, 120).to_string()
        } else {
            String::new()
        };
        println!(
            "  {} {}{}",
            format!("[{}]", i + 1).cyan(),
            if current {
                m.truecolor(0, 200, 120).bold().to_string()
            } else {
                m.to_string()
            },
            marker,
        );
    }

    // ── Model selection ──────────────────────────────────────────────────
    println!();
    print!(
        "  {} ",
        "Select models (comma-separated, e.g. 1,3,7):".bold()
    );
    flush_stdout();
    let selection_line = crate::terminal::read_line_raw().unwrap_or_default();
    let selection_line = selection_line.trim();
    if selection_line.is_empty() {
        print_error("No models selected.");
        return SlashResult::Handled;
    }

    let selected_models: Vec<String> = selection_line
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if let Ok(idx) = s.parse::<usize>() {
                if idx >= 1 && idx <= models.len() {
                    Some(models[idx - 1].clone())
                } else {
                    None
                }
            } else if !s.is_empty() {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();

    if selected_models.is_empty() {
        print_error("No valid models selected.");
        return SlashResult::Handled;
    }

    // ── Prompt ───────────────────────────────────────────────────────────
    let prompt = if arg.is_empty() {
        println!();
        print!("  {} ", "Prompt:".bold());
        flush_stdout();
        let p = crate::terminal::read_line_raw().unwrap_or_default();
        let p = p.trim().to_string();
        if p.is_empty() {
            print_error("Empty prompt.");
            return SlashResult::Handled;
        }
        p
    } else {
        arg.to_string()
    };

    // ── Hand off to main loop ────────────────────────────────────────────
    SlashResult::Benchmark(selected_models, prompt)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Graceful shutdown
// ═══════════════════════════════════════════════════════════════════════════

pub async fn graceful_shutdown(bridge: &mut Box<dyn bridge::Bridge>, conv: &Conversation) {
    let running: Vec<_> = conv.active_handles.iter().map(|h| h.id.clone()).collect();

    if running.is_empty() {
        return;
    }

    println!(
        "\n  {} Stopping {} active handle(s)…",
        "⟳".yellow(),
        running.len()
    );
    for id in &running {
        let action = serde_json::json!({"type": "StopHandle", "handle": id});
        match bridge.send_action(&action).await {
            Ok(_) => println!("  {} stopped {}", "✓".green(), id),
            Err(e) => println!("  {} could not stop {}: {}", "✗".red(), id, e),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  /artifact — listing + subcommands (open / save / stop / rm)
// ═══════════════════════════════════════════════════════════════════════════

async fn handle_artifact(arg: &str, bridge: &mut Box<dyn bridge::Bridge>) {
    let (verb, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    let rest = rest.trim();
    match verb {
        "" | "list" | "ls" => print_artifact_list(bridge).await,
        "open" => {
            if rest.is_empty() {
                print_error("/artifact open <id> requires a artifact id");
                return;
            }
            artifact_open(rest, bridge).await;
        }
        "save" => {
            if rest.is_empty() {
                print_error("/artifact save <id> [name] requires a artifact id");
                return;
            }
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            let mut payload = serde_json::json!({"type":"ArtifactSave","id":id});
            if !name.is_empty() {
                payload["name"] = Value::String(name.into());
            }
            artifact_run_simple(bridge, payload, &format!("saved {}", id)).await;
        }
        "stop" => {
            if rest.is_empty() {
                print_error("/artifact stop <id> requires a artifact id");
                return;
            }
            artifact_run_simple(
                bridge,
                serde_json::json!({"type":"ArtifactStop","id":rest}),
                &format!("stopped {}", rest),
            )
            .await;
        }
        "rm" | "destroy" => {
            let (id_part, flag) = rest.split_once(' ').unwrap_or((rest, ""));
            if id_part.is_empty() {
                print_error("/artifact rm <id> [--force] requires a artifact id");
                return;
            }
            let force = flag.contains("-f") || flag.contains("force");
            artifact_run_simple(
                bridge,
                serde_json::json!({"type":"ArtifactDestroy","id":id_part,"force":force}),
                &format!("destroyed {}", id_part),
            )
            .await;
        }
        other => {
            print_error(&format!(
                "unknown /artifact subcommand '{}'. Try: list, open <id>, save <id> [name], stop <id>, rm <id> [-f]",
                other
            ));
        }
    }
}

async fn print_artifact_list(bridge: &mut Box<dyn bridge::Bridge>) {
    let payload = serde_json::json!({"type":"ArtifactList"});
    let result = match bridge.send_action(&payload).await {
        Ok(r) => r,
        Err(e) => {
            print_error(&format!("ArtifactList failed: {}", e));
            return;
        }
    };
    if !result["success"].as_bool().unwrap_or(false) {
        print_error(&format!(
            "ArtifactList error: {}",
            result
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ));
        return;
    }

    let port = result
        .pointer("/output/proxy_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(7800);
    let proxy_up = result
        .pointer("/output/proxy_running")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let artifacts = result
        .pointer("/output/artifacts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let proxy_badge = if proxy_up {
        format!("{}", "● online".green())
    } else {
        format!("{}", "○ offline".red())
    };
    println!(
        "\n  {} artifact proxy {} on http://localhost:{}",
        "⫸".truecolor(0, 200, 220),
        proxy_badge,
        port
    );
    if !proxy_up {
        println!(
            "  {} {}",
            "·".dimmed(),
            "Proxy not reachable from the executor (or binary missing). Tips:".dimmed()
        );
        println!(
            "    {} {}",
            "·".dimmed(),
            "Ask the model for ArtifactCreate first — it auto-starts x0ne-artifact-proxy when policy allows."
                .dimmed()
        );
        println!(
            "    {} {}",
            "·".dimmed(),
            "Remote sandbox: run x0ne-artifact-proxy on the host, set X0NE_ARTIFACT_PROXY_CHECK_HOST to the host IP from inside the sandbox, and X0NE_ARTIFACT_PUBLIC_HOST for correct links."
                .dimmed()
        );
    }

    if artifacts.is_empty() {
        println!(
            "  {} no artifact yet — agent should emit {{\"type\":\"ArtifactCreate\",\"kind\":\"static\"}} then WriteFile under the returned root_path (e.g. …/index.html)\n",
            "·".dimmed()
        );
        return;
    }

    println!(
        "  {:<11} {:<8} {:<8} {:<10} {:>5} {:>9}  {}",
        "id".dimmed(),
        "kind".dimmed(),
        "state".dimmed(),
        "where".dimmed(),
        "age".dimmed(),
        "size".dimmed(),
        "url".dimmed(),
    );
    for cv in &artifacts {
        let id = cv["id"].as_str().unwrap_or("?");
        let kind = cv["artifact_kind"].as_str().unwrap_or("?");
        let state = cv["state"].as_str().unwrap_or("?");
        let where_ = if cv["persistent"].as_bool().unwrap_or(false) {
            "saved"
        } else {
            "ephemeral"
        };
        let age = format_age(cv["age_secs"].as_i64().unwrap_or(0));
        let size = format_bytes(cv["size_bytes"].as_u64().unwrap_or(0));
        let url = cv["url"].as_str().unwrap_or("?");
        let state_colored = match state {
            "running" => state.green().to_string(),
            "failed" => state.red().to_string(),
            "stopped" => state.yellow().to_string(),
            _ => state.dimmed().to_string(),
        };
        println!(
            "  {:<11} {:<8} {:<17} {:<10} {:>5} {:>9}  {}",
            id.cyan(),
            kind,
            state_colored,
            where_.dimmed(),
            age,
            size,
            url.truecolor(120, 180, 220),
        );
    }
    println!();
}

async fn artifact_open(id: &str, bridge: &mut Box<dyn bridge::Bridge>) {
    // Resolve URL via ArtifactList, then fire xdg-open (best-effort).
    let payload = serde_json::json!({"type":"ArtifactList"});
    let result = match bridge.send_action(&payload).await {
        Ok(r) => r,
        Err(e) => {
            print_error(&format!("ArtifactList failed: {}", e));
            return;
        }
    };
    let url = result
        .pointer("/output/artifacts")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().find(|c| c["id"].as_str() == Some(id)))
        .and_then(|c| c["url"].as_str())
        .map(|s| s.to_string());
    let Some(url) = url else {
        print_error(&format!("no artifact with id '{}'", id));
        return;
    };
    println!("  {} {}", "→".cyan(), url.truecolor(120, 180, 220));
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn artifact_run_simple(
    bridge: &mut Box<dyn bridge::Bridge>,
    payload: Value,
    success_msg: &str,
) {
    match bridge.send_action(&payload).await {
        Ok(r) => {
            if r["success"].as_bool().unwrap_or(false) {
                println!("  {} {}", "✓".green(), success_msg);
            } else {
                let m = r
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                print_error(m);
            }
        }
        Err(e) => print_error(&format!("action failed: {}", e)),
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  /goal — persistent agent goals (Codex-style)
// ═══════════════════════════════════════════════════════════════════════════
//
// `/goal <text>`        set / replace the active goal
// `/goal`               show current status (alias for `/goal status`)
// `/goal status`        show current status
// `/goal pause`         pause the active goal
// `/goal resume`        resume a paused/blocked goal
// `/goal step [note]`   manually advance the step counter
// `/goal complete [n]`  manually mark as achieved
// `/goal clear`         drop the active goal
// `/goal help`          show this list
//
// State is stored at `~/.x0ne/goals/` and survives sessions. Each invocation
// constructs a fresh `GoalManager` — atomic file writes make this safe.

async fn handle_goal(arg: &str) {
    let manager = match GoalManager::new_default() {
        Ok(m) => m,
        Err(e) => {
            print_error(&format!("goal store unavailable: {}", e));
            return;
        }
    };

    let trimmed = arg.trim();
    let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((s, r)) => (s, r.trim()),
        None => (trimmed, ""),
    };

    match sub.to_ascii_lowercase().as_str() {
        "" | "status" => match manager.get_active().await {
            Ok(Some(g)) => print_goal_state(&g),
            Ok(None) => println!(
                "\n  {} no active goal — `/goal <text>` to create one\n",
                "·".dimmed()
            ),
            Err(e) => print_error(&format!("goal status: {}", e)),
        },
        "help" => print_goal_help(),
        "pause" => match manager.pause().await {
            Ok(Some(g)) => {
                println!("\n  {} goal paused", "⏸".truecolor(220, 180, 80));
                print_goal_state(&g);
            }
            Ok(None) => println!("  {} no active goal to pause", "·".dimmed()),
            Err(e) => print_error(&format!("goal pause: {}", e)),
        },
        "resume" => match manager.resume().await {
            Ok(Some(g)) => {
                println!("\n  {} goal resumed", "▶".truecolor(0, 200, 120));
                print_goal_state(&g);
            }
            Ok(None) => println!("  {} no active goal to resume", "·".dimmed()),
            Err(e) => print_error(&format!("goal resume: {}", e)),
        },
        // The second pass costs model calls. It is therefore an operator
        // setting, and it has one producer: this line. No model action
        // touches the cadence — an agent that could switch off its own
        // reviewer would no longer have one.
        "skeptic" => {
            let cadence = match rest.to_ascii_lowercase().as_str() {
                "on" | "" => Some(SkepticCadence::EveryTurn),
                "off" => Some(SkepticCadence::Off),
                _ => None,
            };
            match cadence {
                None => print_error("usage: /goal skeptic on|off"),
                Some(c) => match manager.set_skeptic(c).await {
                    Ok(Some(g)) => {
                        println!(
                            "\n  {} skeptic {}",
                            "·".dimmed(),
                            if c.runs_this_turn() {
                                "on — one extra model call per turn that produces a report"
                            } else {
                                "off — no second pass, and no stall guard"
                            }
                        );
                        print_goal_state(&g);
                    }
                    Ok(None) => println!("  {} no active goal to configure", "·".dimmed()),
                    Err(e) => print_error(&format!("goal skeptic: {}", e)),
                },
            }
        }
        "clear" => match manager.clear().await {
            Ok(Some(id)) => println!("  {} cleared goal {}", "✓".green(), id.dimmed()),
            Ok(None) => println!("  {} no active goal to clear", "·".dimmed()),
            Err(e) => print_error(&format!("goal clear: {}", e)),
        },
        "complete" => {
            let note = (!rest.is_empty()).then(|| rest.to_string());
            match manager.complete(note).await {
                Ok(Some(g)) => {
                    println!(
                        "\n  {} goal achieved",
                        "🎯".to_string().truecolor(0, 200, 120)
                    );
                    print_goal_state(&g);
                }
                Ok(None) => println!("  {} no active goal to complete", "·".dimmed()),
                Err(e) => print_error(&format!("goal complete: {}", e)),
            }
        }
        "step" => {
            let note = (!rest.is_empty()).then(|| rest.to_string());
            match manager.step(None, note, None).await {
                Ok(Some(outcome)) => {
                    if outcome.achievement_succeeded {
                        println!(
                            "\n  {} achievement check passed",
                            "🎯".to_string().truecolor(0, 200, 120)
                        );
                    }
                    if let Some(note) = outcome.achievement_note {
                        println!("  {} {}", "·".dimmed(), note.dimmed());
                    }
                    print_goal_state(&outcome.state);
                }
                Ok(None) => println!(
                    "  {} no active goal — `/goal <text>` to create one",
                    "·".dimmed()
                ),
                Err(e) => print_error(&format!("goal step: {}", e)),
            }
        }
        // Anything else is treated as a fresh goal (replaces existing).
        _ => {
            if trimmed.is_empty() {
                // Unreachable given the empty-match arm above, but defensive.
                print_goal_help();
                return;
            }
            // Reuse the full trimmed arg as the goal text — `sub` stripping
            // would lose the first word otherwise.
            let text = trimmed.to_string();
            match manager
                .set(
                    text,
                    Budget::new(None, None),
                    Some(AchievementCheck::Manual),
                )
                .await
            {
                Ok(g) => {
                    println!("\n  {} goal set", "🎯".to_string().truecolor(0, 200, 220));
                    print_goal_state(&g);
                    println!(
                        "  {} `/goal step` to record progress · `/goal complete` when done\n",
                        "ℹ".truecolor(132, 142, 157)
                    );
                }
                Err(e) => print_error(&format!("goal set: {}", e)),
            }
        }
    }
}

fn print_goal_help() {
    println!();
    println!(
        "  {} {}",
        "▸".truecolor(0, 180, 200),
        "Goal commands".truecolor(0, 200, 220).bold()
    );
    println!("  {}", "─".repeat(44).truecolor(96, 104, 120));
    let row = |cmd: &str, desc: &str| {
        println!(
            "  {:24} {}",
            cmd.truecolor(0, 180, 220),
            desc.truecolor(132, 142, 157)
        );
    };
    row("/goal <text>", "set or replace the active goal");
    row("/goal", "show current status");
    row("/goal status", "show current status");
    row("/goal step [note]", "record one step of progress");
    row("/goal pause", "pause the active goal");
    row("/goal resume", "resume a paused goal");
    row("/goal complete [n]", "manually mark as achieved");
    row(
        "/goal skeptic on|off",
        "second pass over each turn's report (costs one model call)",
    );
    row("/goal clear", "drop the active goal");
    row("/goal help", "this help");
    println!(
        "\n  {} state persists in ~/.x0ne/goals/ across sessions\n",
        "·".dimmed()
    );
}

fn print_goal_state(g: &GoalState) {
    use chrono::Utc;
    let (status_label, status_color) = match &g.status {
        GoalStatus::Active => ("active", (0u8, 200u8, 120u8)),
        GoalStatus::Paused => ("paused", (220, 180, 80)),
        GoalStatus::Achieved => ("achieved", (0, 200, 220)),
        GoalStatus::Blocked { .. } => ("blocked", (220, 100, 80)),
        GoalStatus::BudgetExhausted => ("budget_exhausted", (220, 100, 80)),
        GoalStatus::Cleared => ("cleared", (120, 120, 120)),
    };
    let blocked_reason = if let GoalStatus::Blocked { reason } = &g.status {
        Some(reason.clone())
    } else {
        None
    };

    println!();
    println!(
        "  {} {}",
        "🎯".to_string().truecolor(0, 200, 220),
        g.text.bold()
    );
    println!(
        "  {:>8}  {}{}",
        "status".truecolor(132, 142, 157),
        status_label
            .truecolor(status_color.0, status_color.1, status_color.2)
            .bold(),
        match &blocked_reason {
            Some(r) => format!(" — {}", r).truecolor(120, 120, 120).to_string(),
            None => String::new(),
        }
    );

    let age_secs = (Utc::now() - g.created_at).num_seconds();
    println!(
        "  {:>8}  created {} ago · updated {} ago",
        "time".truecolor(132, 142, 157),
        format_age(age_secs).dimmed(),
        format_age((Utc::now() - g.updated_at).num_seconds()).dimmed(),
    );

    let used = g.budget.steps_used;
    let cap = g
        .budget
        .max_steps
        .map(|m| format!("/{m}"))
        .unwrap_or_default();
    let elapsed = g.budget.elapsed_seconds();
    let secs_cap = g
        .budget
        .max_seconds
        .map(|m| format!(" · {}/{}s", elapsed, m))
        .unwrap_or_default();
    println!(
        "  {:>8}  {}{} step{}{}",
        "budget".truecolor(132, 142, 157),
        used.to_string().bold(),
        cap.dimmed(),
        if used == 1 { "" } else { "s" },
        secs_cap.dimmed(),
    );

    // The skeptic cadence is shown here, with the rest: a second model call
    // per turn is a cost, and a cost is displayed. Off also means the
    // stagnation guard no longer runs — stating it is the point (scaffolding
    // is recognised by a setting that is visible nowhere).
    println!(
        "  {:>8}  {}",
        "skeptic".truecolor(132, 142, 157),
        if g.skeptic.runs_this_turn() {
            "every turn".truecolor(0, 200, 120).to_string()
        } else {
            "off · no stall guard".truecolor(220, 180, 80).to_string()
        }
    );

    if let Some(check) = &g.achievement_check {
        let kind = match check {
            AchievementCheck::Manual => "manual".to_string(),
            AchievementCheck::FileExists { path } => format!("file_exists({})", path),
            AchievementCheck::FileContains { path, .. } => format!("file_contains({})", path),
            AchievementCheck::Command { command, .. } => format!("command({})", command),
            AchievementCheck::Action { .. } => "action".to_string(),
        };
        println!(
            "  {:>8}  {}",
            "check".truecolor(132, 142, 157),
            kind.dimmed()
        );
    }

    // Recent history — last 5 entries, oldest first for chronological feel.
    let recent: Vec<_> = g
        .history
        .iter()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !recent.is_empty() {
        println!(
            "  {:>8}  {}",
            "history".truecolor(132, 142, 157),
            recent
                .iter()
                .map(|h| history_kind_glyph(&h.kind))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    println!();
}

fn history_kind_glyph(k: &HistoryKind) -> &'static str {
    match k {
        HistoryKind::Created => "✦",
        HistoryKind::Step => "·",
        HistoryKind::Paused => "⏸",
        HistoryKind::Resumed => "▶",
        HistoryKind::AchievementCheck => "?",
        // The skeptic objects: a mark distinct from the « ? » of the
        // achievement check, which, for its part, queries the runtime.
        HistoryKind::Skeptic => "!",
        HistoryKind::Achieved => "✓",
        HistoryKind::Blocked => "✗",
        HistoryKind::BudgetExhausted => "∅",
        HistoryKind::Completed => "✓",
        HistoryKind::Cleared => "×",
    }
}

fn format_bytes(b: u64) -> String {
    if b >= 1_048_576 {
        format!("{:.1}MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.1}KB", b as f64 / 1024.0)
    } else {
        format!("{}B", b)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  /queue — inspect / mutate the between-turn message queue
// ═══════════════════════════════════════════════════════════════════════════

fn handle_queue(arg: &str) {
    let arg = arg.trim();

    // ── /queue clear ──────────────────────────────────────────────────────
    if arg.eq_ignore_ascii_case("clear") {
        let n = input_queue::queue_clear();
        if n == 0 {
            println!("  {} queue already empty.", "·".truecolor(152, 162, 177));
        } else {
            println!(
                "  {} cleared {} pending message{}.",
                "×".truecolor(220, 80, 80),
                n.to_string().bold(),
                if n == 1 { "" } else { "s" }
            );
        }
        return;
    }

    // ── /queue drop N ─────────────────────────────────────────────────────
    if let Some(rest) = arg.strip_prefix("drop ").map(str::trim).or_else(|| {
        arg.strip_prefix("drop")
            .filter(|s| s.is_empty())
            .map(|_| "")
    }) {
        if rest.is_empty() {
            print_error("/queue drop requires a 1-indexed position. e.g. /queue drop 2");
            return;
        }
        let idx: usize = match rest.parse() {
            Ok(n) if n >= 1 => n,
            _ => {
                print_error("/queue drop expects a positive integer (1-indexed).");
                return;
            }
        };
        match input_queue::queue_drop_at(idx) {
            Some(line) => println!(
                "  {} dropped #{}: {}",
                "×".truecolor(220, 80, 80),
                idx,
                truncate_for_display(&line, 70).truecolor(160, 160, 170)
            ),
            None => print_error(&format!(
                "no message at position #{} (queue has {})",
                idx,
                input_queue::queue_len()
            )),
        }
        return;
    }

    // ── /queue (no arg) — list pending ────────────────────────────────────
    let snap = input_queue::queue_snapshot();
    if snap.is_empty() {
        println!(
            "  {} queue is empty. {}",
            "·".truecolor(152, 162, 177),
            "Type while the agent is busy; lines stack up here.".truecolor(132, 142, 157)
        );
        println!(
            "  {} {} · {}",
            "·".truecolor(112, 124, 138),
            "/queue clear".truecolor(152, 162, 177),
            "/queue drop N".truecolor(152, 162, 177),
        );
        return;
    }
    println!(
        "  📋  {} message{} queued (will run in order):",
        snap.len().to_string().truecolor(0, 200, 220).bold(),
        if snap.len() == 1 { "" } else { "s" }
    );
    for (i, line) in snap.iter().enumerate() {
        println!(
            "    {:>2}. {}",
            (i + 1).to_string().truecolor(0, 200, 220),
            truncate_for_display(line, 100).truecolor(180, 200, 220)
        );
    }
    println!(
        "  {} {} drop a message · {} clear all",
        "·".truecolor(112, 124, 138),
        "/queue drop N".truecolor(152, 162, 177),
        "/queue clear".truecolor(152, 162, 177),
    );
}

fn truncate_for_display(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── /undo ─────────────────────────────────────────────────

    /// Records every action; answers success, as a runtime would.
    struct RecordingBridge(std::sync::Arc<std::sync::Mutex<Vec<Value>>>);

    #[async_trait::async_trait]
    impl bridge::Bridge for RecordingBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            self.0.lock().unwrap().push(action.clone());
            Ok(serde_json::json!({"success": true, "output": {"exit_code": 0}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// `/plan` and `/build` are decided by the session, which tells the
    /// runtime first; the dispatcher only names the request, and sends nothing.
    #[tokio::test]
    async fn plan_and_build_are_requests_to_the_session() {
        for (line, want) in [("/plan", true), ("/build", false)] {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut bridge: Box<dyn bridge::Bridge> = Box::new(RecordingBridge(seen.clone()));
            let mut history = crate::agent::history::History::new(100_000);
            let r = handle_slash_command(
                line,
                &mut history,
                &mut bridge,
                &UsageTracker::new("test"),
                &mut Config::default(),
                None,
                &mut crate::agent::undo::UndoJournal::default(),
            )
            .await;
            assert!(
                matches!(r, SlashResult::SetPlanMode(p) if p == want),
                "{line}"
            );
            assert!(
                seen.lock().unwrap().is_empty(),
                "{line} reached the runtime"
            );
        }
        assert!(crate::cli_helper::COMMANDS.contains(&"/plan"));
        assert!(crate::cli_helper::COMMANDS.contains(&"/build"));
    }

    #[tokio::test]
    async fn undo_without_a_snapshot_refuses_and_sends_nothing() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut bridge: Box<dyn bridge::Bridge> = Box::new(RecordingBridge(seen.clone()));
        let mut history = crate::agent::history::History::new(100_000);
        let usage = UsageTracker::new("test");
        let mut config = Config::default();
        let r = handle_slash_command(
            "/undo",
            &mut history,
            &mut bridge,
            &usage,
            &mut config,
            None,
            &mut crate::agent::undo::UndoJournal::default(),
        )
        .await;
        assert!(matches!(r, SlashResult::Handled));
        assert!(
            seen.lock().unwrap().is_empty(),
            "/undo with nothing to undo must not touch the runtime: {:?}",
            seen.lock().unwrap()
        );
    }

    /// The whole path: the agent's write goes through the dispatcher, the
    /// user's own uncommitted edits (in that file before, in another file
    /// after) survive `/undo`, and git is never run.
    #[tokio::test]
    async fn undo_keeps_uncommitted_work_the_agent_did_not_make() {
        use crate::agent::dispatch::{dispatch, DispatchContext};
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let edited = work.join("lib.rs");
        let other = work.join("notes.txt");
        std::fs::write(&edited, "user: uncommitted edit").unwrap();
        std::fs::write(&other, "user: draft").unwrap();
        let snap = crate::agent::undo::tests::SnapshotBridge::new(&work, &tmp.path().join("s"));
        let seen = snap.seen.clone();
        let mut bridge: Box<dyn bridge::Bridge> = Box::new(snap);
        let mut config = Config::default();
        config.trust.roots = vec![work.display().to_string()];
        let mut undo = crate::agent::undo::UndoJournal::default();

        let write = serde_json::json!({"type": "WriteFile",
            "path": edited.display().to_string(), "content": "agent: rewrite"});
        let mut sink = crate::display_sink::NullDisplaySink;
        let cancel = crate::agent::cancel::CancelToken::new();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: Some(&mut undo),
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(bridge.as_mut()),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        assert!(dispatch(&write, &mut ctx).await.succeeded());
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "agent: rewrite");
        std::fs::write(&other, "user: draft, continued").unwrap();

        let mut history = crate::agent::history::History::new(100_000);
        let usage = UsageTracker::new("test");
        handle_slash_command(
            "/undo",
            &mut history,
            &mut bridge,
            &usage,
            &mut config,
            None,
            &mut undo,
        )
        .await;

        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "user: uncommitted edit"
        );
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "user: draft, continued"
        );
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .all(|a| a["type"] != "RunCommand"));
    }

    #[test]
    fn no_slash_command_runs_git_checkout() {
        let src = include_str!("slash.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(!production.contains("\"checkout\""));
    }

    // ── normalize_slash_input ────────────────────────────────────────
    //
    // The `/actions` → `Unknown command: /ions` report came from input
    // that *rendered* as a valid command but carried invisible chars.
    // These tests pin the cleanup contract.

    /// `/auto` flips `ui.auto_confirm`, which the approval gate neutralises
    /// (`confirm::requires_approval`). Measured 2026-10-27: it still printed
    /// "ON — no confirmations" while every write kept asking.
    #[tokio::test]
    async fn auto_says_the_gate_still_asks_and_the_gate_does() {
        let mut bridge: Box<dyn bridge::Bridge> = Box::new(RecordingBridge(Default::default()));
        let mut history = crate::agent::history::History::new(100_000);
        let usage = UsageTracker::new("test");
        let mut config = Config::default();
        let r = handle_slash_command(
            "/auto",
            &mut history,
            &mut bridge,
            &usage,
            &mut config,
            None,
            &mut crate::agent::undo::UndoJournal::default(),
        )
        .await;
        assert!(matches!(r, SlashResult::Handled));
        assert!(config.ui.auto_confirm, "/auto still toggles the flag");
        let write = serde_json::json!({"type": "WriteFile", "path": "/tmp/x", "content": "x"});
        assert!(
            crate::agent::confirm::requires_approval(&write, &config),
            "the gate ignores the flag"
        );
        let on = auto_state_line(true);
        assert!(!on.contains("no confirmations"), "{on}");
        assert!(on.contains("every write still asks"), "{on}");
        let off = auto_state_line(false);
        assert!(off.contains("every write asks"), "{off}");
    }

    /// `/rename` hands the trimmed title to the frontend, which owns the
    /// journal; an empty or overlong one is refused here.
    #[tokio::test]
    async fn rename_hands_the_title_to_the_frontend() {
        let mut bridge: Box<dyn bridge::Bridge> = Box::new(RecordingBridge(Default::default()));
        let mut history = crate::agent::history::History::new(100_000);
        let usage = UsageTracker::new("test");
        let mut config = Config::default();
        let long = format!("/rename {}", "a".repeat(121));
        let mut results = Vec::new();
        for line in ["/rename   fix the parser  ", "/rename", long.as_str()] {
            results.push(
                handle_slash_command(
                    line,
                    &mut history,
                    &mut bridge,
                    &usage,
                    &mut config,
                    None,
                    &mut crate::agent::undo::UndoJournal::default(),
                )
                .await,
            );
        }
        assert!(
            matches!(&results[0], SlashResult::Rename(t) if t == "fix the parser"),
            "the title did not reach the frontend"
        );
        assert!(matches!(results[1], SlashResult::Handled));
        assert!(matches!(results[2], SlashResult::Handled));
    }

    #[test]
    fn normalize_passes_clean_commands_through() {
        assert_eq!(normalize_slash_input("/actions"), "/actions");
        assert_eq!(normalize_slash_input("/model qwen2.5"), "/model qwen2.5");
    }

    #[test]
    fn normalize_strips_bom_and_zero_width_chars() {
        assert_eq!(normalize_slash_input("\u{feff}/actions"), "/actions");
        assert_eq!(normalize_slash_input("/act\u{200b}ions"), "/actions");
        assert_eq!(
            normalize_slash_input("/ac\u{200d}tions\u{2060}"),
            "/actions"
        );
        assert_eq!(normalize_slash_input("/act\u{00ad}ions"), "/actions");
    }

    #[test]
    fn normalize_converts_unicode_spaces_so_verb_arg_split_works() {
        // NBSP between verb and arg (common when pasting from rich text).
        assert_eq!(normalize_slash_input("/model\u{a0}llama3"), "/model llama3");
        assert_eq!(normalize_slash_input("  /help  "), "/help");
    }

    #[test]
    fn normalize_drops_control_chars_from_paste_artifacts() {
        assert_eq!(normalize_slash_input("/hist\u{7f}ory"), "/history");
        assert_eq!(normalize_slash_input("\u{1b}/quit"), "/quit");
    }

    // ── suggest_command ──────────────────────────────────────────────

    #[test]
    fn suggest_recovers_the_reported_ions_mangle() {
        assert_eq!(suggest_command("/ions"), Some("/actions"));
    }

    #[test]
    fn suggest_handles_typos_and_transpositions() {
        assert_eq!(suggest_command("/actoins"), Some("/actions"));
        assert_eq!(suggest_command("/hlep"), Some("/help"));
        assert_eq!(suggest_command("/quitt"), Some("/quit"));
    }

    #[test]
    fn suggest_returns_none_when_nothing_is_close() {
        assert_eq!(suggest_command("/completely-unrelated-verb"), None);
    }

    // ── /model list → bare-number selection ──────────────────────────
    //
    // One test, not several: the pending flag and the displayed list are
    // process-global statics, so parallel test threads would race.

    #[test]
    fn bare_number_selects_only_immediately_after_model_list() {
        use std::sync::atomic::Ordering;

        // No list displayed yet: a bare number is just a message.
        MODEL_LIST_PENDING.store(false, Ordering::SeqCst);
        assert!(!take_bare_model_selection("3"));

        // `/model list` ran: the very next bare number is a selection…
        remember_displayed_models(&["opus".into(), "sonnet".into()]);
        assert!(take_bare_model_selection("2"));
        // …and the state is one-shot: consumed by the pick.
        assert!(!take_bare_model_selection("2"));

        // Any other input clears it: a number typed later is never hijacked.
        remember_displayed_models(&["opus".into(), "sonnet".into()]);
        assert!(!take_bare_model_selection("how many tests?"));
        assert!(!take_bare_model_selection("3"));

        // Long digit strings are conversation, not indexes (e.g. a port,
        // a PID) — even right after the list.
        remember_displayed_models(&["opus".into(), "sonnet".into()]);
        assert!(!take_bare_model_selection("1996032"));

        // `/model N` resolves against what was DISPLAYED, not a re-fetch.
        remember_displayed_models(&["a-model".into(), "b-model".into()]);
        assert_eq!(displayed_models(), vec!["a-model", "b-model"]);
        MODEL_LIST_PENDING.store(false, Ordering::SeqCst);
    }
}
