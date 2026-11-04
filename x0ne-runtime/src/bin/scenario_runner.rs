//! X0NE Scenario Runner — In-Process Test Harness
//!
//! Bypasses LLM inference entirely: loads JSON scenario files, injects action
//! payloads directly into the runtime (parsing → policy → executor), and asserts
//! on structured results. Deterministic, fast, CI-ready.
//!
//! Usage:
//!   cargo run --bin x0ne-scenario-runner
//!   cargo run --bin x0ne-scenario-runner -- --scenario tests/scenarios/happy_path.json
//!   cargo run --bin x0ne-scenario-runner -- --verbose
//!
//! What this harness CANNOT cover, so that its absence is not read as a gap
//! (arbitrated 2026-08-14, `x0ne cc full` task 5 review): the `x0ne cc`
//! pipeline. Everything here is an action injected into the runtime, and `cc`
//! lives in the orchestrator and the CLI — it drives gcloud through a `CcExec`
//! trait and exposes no action to inject. Its coverage is Rust tests
//! (`cc_runner.rs`, `cc_cmd.rs`, `cc_plan.rs`) replaying whole pipelines
//! against a fake executor, plus per-task mutation campaigns.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use serde::Deserialize;
use serde_json::Value;

use x0ne_runtime::action::Action;
use x0ne_runtime::composite::CompositeStore;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::executor;
use x0ne_runtime::executor::human::{install_prompt, HumanPrompt, ScriptedPrompt};
use x0ne_runtime::handles::new_shared;
use x0ne_runtime::policy::{PolicyEffect, PolicyEngine};
use x0ne_runtime::result::{ActionResult, ErrorKind};
use x0ne_runtime::server::ServerState;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "x0ne-scenario-runner",
    about = "In-process test harness for X0NE runtime — deterministic, zero-inference testing"
)]
struct Cli {
    /// Run only a specific scenario file instead of all
    #[arg(long, value_name = "FILE")]
    scenario: Option<PathBuf>,

    /// One-shot probe: run a single raw JSON action and print the result.
    /// Example: x0ne-scenario-runner --probe '{"type":"SystemInfo"}'
    #[arg(long, value_name = "JSON")]
    probe: Option<String>,

    /// Print the actual ActionOutput value on success (not just pass/fail)
    #[arg(long, short = 'o')]
    show_output: bool,

    /// Print full ActionResult JSON on failures
    #[arg(long, short)]
    verbose: bool,

    /// Path to the scenarios directory
    #[arg(long, default_value = "x0ne-runtime/tests/scenarios")]
    scenarios_dir: PathBuf,

    /// Enable desktop (Wayland) scenarios — requires a graphical session with wlrctl/wtype/grim
    #[arg(long)]
    desktop: bool,

    /// Enable Incus sandbox config (reads INCUS_REMOTE, INCUS_IMAGE, INCUS_SNAPSHOT env vars).
    /// Adds wildcard image allowlist so RunIncus/RunIncusGroup can run.
    #[arg(long)]
    incus: bool,

    /// Remove all command restrictions — use only when probing on a trusted dev machine.
    /// WARNING: allows ANY command on the host (rm, dd, etc.).
    #[arg(long)]
    allow_all_commands: bool,
}

// ── Scenario types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ScenarioCase {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    action: Value,
    expect: Expectation,
    /// Pre-recorded answers fed to AskHuman/Confirm during this case.
    /// Each entry is consumed in order by the next interactive primitive.
    #[serde(default)]
    human_responses: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Expectation {
    #[serde(default = "default_true")]
    success: bool,
    #[serde(default)]
    error_kind: Option<String>,

    /// Tag of the ActionOutput variant (snake_case), e.g. "pipeline_result", "file_content".
    #[serde(default)]
    output_kind: Option<String>,
    /// Substring that must appear in the JSON-serialised output.
    #[serde(default)]
    output_contains: Option<String>,
    /// Substring that must NOT appear in the output (negative assertion).
    #[serde(default)]
    output_not_contains: Option<String>,

    /// Structured assertions for Pipeline / CallPrimitive results.
    #[serde(default)]
    pipeline: Option<PipelineExpect>,
    /// Structured assertions for Try blocks.
    #[serde(default)]
    try_block: Option<TryExpect>,
    /// Structured assertion for Assert primitive.
    #[serde(default)]
    assert_check: Option<AssertExpect>,
    /// Structured assertion for AskHuman / Confirm.
    #[serde(default)]
    human: Option<HumanExpect>,

    /// Filesystem state to verify after execution.
    #[serde(default)]
    side_effects: Vec<SideEffect>,

    /// Hard upper bound on case duration (ms). Useful for parallel-vs-sequential checks.
    #[serde(default)]
    max_duration_ms: Option<u128>,
    /// Lower bound on case duration (ms). Useful for `Sleep` and rate-limit checks.
    #[serde(default)]
    min_duration_ms: Option<u128>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct PipelineExpect {
    #[serde(default)]
    steps_total: Option<usize>,
    #[serde(default)]
    steps_completed: Option<usize>,
    #[serde(default)]
    stopped_early: Option<bool>,
    /// Per-step success flags. Length must equal the actual `results` length.
    #[serde(default)]
    step_success: Option<Vec<bool>>,
    /// Index (0-based) of the first failing step. None means "no failure expected".
    #[serde(default)]
    first_failed_index: Option<usize>,
    /// Whether the pipeline should have created a snapshot.
    #[serde(default)]
    snapshot_present: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TryExpect {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    completed: Option<usize>,
    #[serde(default)]
    total: Option<usize>,
    #[serde(default)]
    rolled_back: Option<bool>,
    #[serde(default)]
    snapshot_present: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AssertExpect {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    check: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HumanExpect {
    /// Expected value of `answer` (AskHuman or AskMultipleChoice/other).
    #[serde(default)]
    answer: Option<String>,
    /// Expected value of `confirmed` (Confirm).
    #[serde(default)]
    confirmed: Option<bool>,
    /// Expected 0-based pick (AskMultipleChoice).
    #[serde(default)]
    choice_index: Option<usize>,
    /// Expected `skipped` flag (AskMultipleChoice).
    #[serde(default)]
    skipped: Option<bool>,
    /// Expected `other` flag (AskMultipleChoice).
    #[serde(default)]
    other: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SideEffect {
    /// Path must exist (file or directory).
    PathExists { path: String },
    /// Path must NOT exist.
    PathMissing { path: String },
    /// Regular file content must equal `equals`.
    FileContent { path: String, equals: String },
    /// File content must contain `substring`.
    FileContains { path: String, substring: String },
    /// Snapshot directory `<snapshots_dir>/<name>` must exist.
    SnapshotExists { name: String },
    /// Snapshot directory must NOT exist.
    SnapshotMissing { name: String },
}

// ── Test result tracking ─────────────────────────────────────────────────────

#[derive(Debug)]
struct TestResult {
    scenario: String,
    name: String,
    passed: bool,
    duration_ms: u128,
    detail: Option<String>,
    output_preview: Option<String>,
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // ── Setup temp directory for test fixtures ───────────────────────────────
    let test_dir = std::env::temp_dir().join("x0ne-scenario-test");
    if test_dir.exists() {
        let _ = std::fs::remove_dir_all(&test_dir);
    }
    std::fs::create_dir_all(&test_dir).expect("Failed to create test directory");

    // Populate test fixtures
    setup_fixtures(&test_dir);

    // ── Build in-process runtime state ───────────────────────────────────────
    let state = build_test_state(&test_dir, cli.desktop, cli.incus, cli.allow_all_commands);

    // ── One-shot probe mode ───────────────────────────────────────────────────
    if let Some(probe_json) = &cli.probe {
        run_probe(probe_json, &state, cli.show_output).await;
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    }

    // ── Discover & load scenarios ────────────────────────────────────────────
    let mut scenario_files = match &cli.scenario {
        Some(path) => vec![path.clone()],
        None if cli.desktop => {
            let desktop_path = cli.scenarios_dir.join("desktop.json");
            if desktop_path.exists() {
                vec![desktop_path]
            } else {
                eprintln!(
                    "\x1b[33m⚠ No desktop.json found in {}\x1b[0m",
                    cli.scenarios_dir.display()
                );
                vec![]
            }
        }
        None => discover_scenarios(&cli.scenarios_dir, cli.incus),
    };

    // ── Pre-flight Incus remote connectivity check ───────────────────────────
    // When --incus is set, probe the remote with a 5s timeout before running
    // any Incus scenarios. If unreachable, skip those tests with a warning
    // instead of failing each one with 20s timeouts.
    if cli.incus {
        let remote = std::env::var("INCUS_REMOTE").unwrap_or_else(|_| "local".into());
        if remote != "local" {
            let probe = std::process::Command::new("incus")
                .args([
                    "list",
                    &format!("{}:", remote),
                    "--format",
                    "csv",
                    "-c",
                    "n",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn();
            let reachable = match probe {
                Ok(child) => {
                    let handle = child;
                    // Wait with 5s timeout
                    let start = Instant::now();
                    let mut child = handle;
                    loop {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                break status.success();
                            }
                            Ok(None) => {
                                if start.elapsed().as_secs() > 5 {
                                    let _ = child.kill();
                                    break false;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            Err(_) => {
                                break false;
                            }
                        }
                    }
                }
                Err(_) => false,
            };
            if !reachable {
                eprintln!(
                    "\x1b[33m⚠  Incus remote '{}' is unreachable — skipping Incus scenarios\x1b[0m",
                    remote
                );
                eprintln!(
                    "\x1b[33m   Fix: incus remote list  |  check server at remote '{}'\x1b[0m",
                    remote
                );
                scenario_files.retain(|p| p.file_stem().and_then(|s| s.to_str()) != Some("incus"));
            } else {
                eprintln!("  \x1b[32m✓\x1b[0m  Incus remote '{}' reachable", remote);
            }
        }
    }

    if scenario_files.is_empty() {
        eprintln!(
            "\x1b[33m⚠ No scenario files found in {}\x1b[0m",
            cli.scenarios_dir.display()
        );
        std::process::exit(1);
    }

    // ── Run all scenarios ────────────────────────────────────────────────────
    let mut all_results: Vec<TestResult> = Vec::new();
    let global_start = Instant::now();

    for scenario_path in &scenario_files {
        let scenario_name = scenario_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let cases = match load_scenario(scenario_path, &test_dir) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "\x1b[31m✗ Failed to load {}: {}\x1b[0m",
                    scenario_path.display(),
                    e
                );
                all_results.push(TestResult {
                    scenario: scenario_name.to_string(),
                    name: "LOAD_ERROR".into(),
                    passed: false,
                    duration_ms: 0,
                    detail: Some(e.to_string()),
                    output_preview: None,
                });
                continue;
            }
        };

        eprintln!(
            "\n\x1b[1m── {} ({} cases) ──\x1b[0m",
            scenario_name,
            cases.len()
        );

        // Refresh fixtures between scenario files so cross-file mutations
        // (e.g. a Try test that rewrites hello.txt) don't leak into the
        // next file. We deliberately do NOT clear snapshots / primitives:
        // they're scoped per scenario_name and tests choose unique names.
        setup_fixtures(&test_dir);

        for case in cases {
            let result = run_case(&state, scenario_name, &case, cli.verbose, cli.show_output).await;
            print_result(&result);
            all_results.push(result);
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────────────
    let _ = std::fs::remove_dir_all(&test_dir);

    // ── Final report ─────────────────────────────────────────────────────────
    let total_duration = global_start.elapsed();
    let passed = all_results.iter().filter(|r| r.passed).count();
    let failed = all_results.iter().filter(|r| !r.passed).count();
    let total = all_results.len();

    eprintln!("\n\x1b[1m════════════════════════════════════════════════\x1b[0m");
    eprintln!(
        "\x1b[1mResults: {} passed, {} failed, {} total ({:.1}s)\x1b[0m",
        passed,
        failed,
        total,
        total_duration.as_secs_f64()
    );

    if failed > 0 {
        eprintln!("\n\x1b[31;1mFailed tests:\x1b[0m");
        for r in all_results.iter().filter(|r| !r.passed) {
            eprintln!("  \x1b[31m✗\x1b[0m {}::{}", r.scenario, r.name);
            if let Some(detail) = &r.detail {
                eprintln!("    → {}", detail);
            }
        }
        eprintln!();
        std::process::exit(1);
    } else {
        eprintln!("\n\x1b[32;1m✓ All tests passed!\x1b[0m\n");
    }
}

// ── Fixture setup ────────────────────────────────────────────────────────────

fn setup_fixtures(test_dir: &Path) {
    // hello.txt
    std::fs::write(test_dir.join("hello.txt"), "hello from x0ne test fixtures")
        .expect("Failed to write hello.txt fixture");

    // multiline.txt
    std::fs::write(
        test_dir.join("multiline.txt"),
        "line one\nline two\nline three\nline four\nline five\n",
    )
    .expect("Failed to write multiline.txt fixture");

    // replaceme.txt
    std::fs::write(test_dir.join("replaceme.txt"), "before REPLACE_THIS after")
        .expect("Failed to write replaceme.txt fixture");

    // subdir with a file
    let sub = test_dir.join("subdir");
    std::fs::create_dir_all(&sub).expect("Failed to create subdir");
    std::fs::write(sub.join("nested.txt"), "nested file content")
        .expect("Failed to write nested.txt fixture");

    // data.json — JSON fixture for ReadJson tests
    std::fs::write(
        test_dir.join("data.json"),
        r#"{"name":"x0ne","version":"0.2","features":["fs","net","crypto"]}"#,
    )
    .expect("Failed to write data.json fixture");

    // config.toml — TOML fixture for ParseToml tests
    std::fs::write(
        test_dir.join("config.toml"),
        "[package]\nname = \"test\"\nversion = \"1.0.0\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .expect("Failed to write config.toml fixture");

    // copy-source.txt — fixture for CopyFile tests
    std::fs::write(test_dir.join("copy-source.txt"), "copy me please")
        .expect("Failed to write copy-source.txt fixture");

    // invalid.json — intentionally invalid JSON for edge case testing
    std::fs::write(test_dir.join("invalid.json"), "{ not valid json }")
        .expect("Failed to write invalid.json fixture");

    // invalid.toml — intentionally invalid TOML for edge case testing
    std::fs::write(test_dir.join("invalid.toml"), "[[[ broken toml")
        .expect("Failed to write invalid.toml fixture");
}

// ── Build runtime state ──────────────────────────────────────────────────────

fn build_test_state(
    test_dir: &Path,
    desktop: bool,
    incus: bool,
    allow_all_commands: bool,
) -> ServerState {
    let test_dir_str = test_dir.to_string_lossy().to_string();

    let mut cfg = AppConfig::defaults();

    // Permissive policy for happy-path testing:
    // - allow all action types
    // - allow read/write under the test directory
    // - whitelist safe commands
    cfg.policy.default_effect = "allow".into();
    cfg.policy.mode = "auto".into(); // No confirm prompts in tests
    cfg.policy.allowed_read_paths = vec![test_dir_str.clone(), "/tmp".into()];
    cfg.policy.allowed_write_paths = vec![test_dir_str.clone(), "/tmp".into()];
    // Safe default: read-only and common dev tools only.
    // Use --allow-all-commands explicitly if you need more.
    if allow_all_commands {
        eprintln!(
            "\x1b[33m⚠  --allow-all-commands active: RunCommand unrestricted on this host\x1b[0m"
        );
        cfg.policy.allowed_commands = vec![]; // empty = all allowed
    } else {
        // Safe dev tools — no shells (sh/bash are blocked as escape vectors in policy tests)
        cfg.policy.allowed_commands = vec![
            "echo".into(),
            "cat".into(),
            "ls".into(),
            "true".into(),
            "false".into(),
            "git".into(),
            "cargo".into(),
            "python3".into(),
            "python".into(),
            "node".into(),
            "npm".into(),
            "make".into(),
            "uname".into(),
            "whoami".into(),
            "which".into(),
            "env".into(),
            "curl".into(),
            "jq".into(),
            "grep".into(),
            "awk".into(),
            "sed".into(),
        ];
    }
    cfg.policy.deny_path_patterns = vec![
        "**/.ssh/**".into(),
        "**/.gnupg/**".into(),
        "**/id_rsa*".into(),
        "**/id_ed25519*".into(),
        "**/*.key".into(),
        "**/*.pem".into(),
        "**/secrets/**".into(),
    ];

    // Enable all actions for happy path
    for enabled in cfg.actions.values_mut() {
        *enabled = true;
    }

    cfg.desktop.enabled = desktop;
    if desktop {
        cfg.desktop.input_rate_limit_ms = 0;
        cfg.desktop.max_input_actions_per_minute = 60000;
    }

    // Incus: configure from env vars when --incus flag is set
    if incus {
        // Wildcard allowlist so any image in the scenario can run
        cfg.sandbox.incus.allowed_images = vec!["images:*".into(), "ubuntu:*".into()];
        cfg.sandbox.incus.default_timeout_secs = 60;
        cfg.sandbox.incus.max_timeout_secs = 120;
        cfg.sandbox.incus.max_parallel_instances = 8;
        cfg.sandbox.incus.max_memory_mb = 2048;
        // Enable RunIncus + RunIncusGroup
        cfg.actions.insert("RunIncus".into(), true);
        cfg.actions.insert("RunIncusGroup".into(), true);
    }

    // Snapshots directory in test area
    cfg.snapshots.dir = test_dir.join(".snapshots").to_string_lossy().to_string();
    // Composite primitives directory in test area — don't pollute ~/.config/x0ne/primitives
    cfg.composites.primitives_dir = test_dir.join(".primitives").to_string_lossy().to_string();

    let policy_engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());

    let handles = new_shared(
        cfg.events.max_events_per_handle,
        cfg.handles.completed_retention_secs,
        cfg.handles.max_concurrent,
    );

    let rate_limiter = Arc::new(tokio::sync::Mutex::new(
        std::time::Instant::now() - std::time::Duration::from_secs(60),
    ));

    let composites = CompositeStore::new(&cfg);

    ServerState {
        config: Arc::new(cfg),
        policy: Arc::new(policy_engine),
        handles,
        rate_limiter,
        composites,
    }
}

// ── One-shot probe mode ──────────────────────────────────────────────────────

async fn run_probe(json: &str, state: &ServerState, show_output: bool) {
    use x0ne_runtime::policy::PolicyEffect;

    let parse_result: std::result::Result<Action, _> = serde_json::from_str(json);
    let action = match parse_result {
        Ok(a) => a,
        Err(e) => {
            eprintln!("\x1b[31m✗ Parse error:\x1b[0m {}", e);
            std::process::exit(1);
        }
    };

    let type_name = action.type_name();
    let t0 = Instant::now();

    let result = match state.policy.evaluate(&action) {
        PolicyEffect::Allow => {
            executor::execute(
                &action,
                &state.config,
                state.handles.clone(),
                &state.policy,
                &state.rate_limiter,
                &state.composites,
            )
            .await
        }
        PolicyEffect::RequireConfirmation(msg) => ActionResult::err(
            type_name,
            ErrorKind::PolicyDenied,
            format!("Requires confirmation: {}", msg),
            0,
        ),
        PolicyEffect::Deny(reason) => {
            ActionResult::err(type_name, ErrorKind::PolicyDenied, reason.message(), 0)
        }
    };

    let ms = t0.elapsed().as_millis();

    eprintln!();
    if result.success {
        eprintln!("  \x1b[32m✅ {}\x1b[0m  \x1b[90m{}ms\x1b[0m", type_name, ms);
        if show_output {
            if let Some(ref out) = result.output {
                let json = serde_json::to_string_pretty(out).unwrap_or_default();
                eprintln!("{}", indent(&json, 4));
            }
        }
    } else {
        eprintln!("  \x1b[31m❌ {}\x1b[0m  \x1b[90m{}ms\x1b[0m", type_name, ms);
        if let Some(ref err) = result.error {
            eprintln!("    \x1b[31m→ [{:?}] {}\x1b[0m", err.kind, err.message);
        }
        // On failure always dump the full result JSON for debugging
        // (regardless of show_output).
        let json = serde_json::to_string_pretty(&result).unwrap_or_default();
        eprintln!("{}", indent(&json, 4));
        std::process::exit(1);
    }
    eprintln!();
}

// ── Scenario loading ─────────────────────────────────────────────────────────

fn discover_scenarios(dir: &Path, incus: bool) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == "desktop" {
                continue;
            } // requires graphical session
            if stem == "incus" && !incus {
                continue;
            } // requires INCUS_REMOTE
            files.push(path);
        }
    }
    files.sort();
    files
}

fn load_scenario(path: &Path, test_dir: &Path) -> anyhow::Result<Vec<ScenarioCase>> {
    let raw = std::fs::read_to_string(path)?;
    let test_dir_str = test_dir.to_string_lossy().to_string();

    // Template substitution — env-var overrides take priority
    let expanded = raw
        .replace("{{TEST_DIR}}", &test_dir_str)
        .replace(
            "{{INCUS_REMOTE}}",
            &std::env::var("INCUS_REMOTE").unwrap_or_default(),
        )
        .replace(
            "{{INCUS_IMAGE}}",
            &std::env::var("INCUS_IMAGE").unwrap_or_else(|_| "images:alpine/edge".into()),
        )
        .replace(
            "{{INCUS_SNAPSHOT}}",
            &std::env::var("INCUS_SNAPSHOT").unwrap_or_default(),
        );

    let cases: Vec<ScenarioCase> = serde_json::from_str(&expanded)?;
    Ok(cases)
}

// ── Case execution ───────────────────────────────────────────────────────────

async fn run_case(
    state: &ServerState,
    scenario_name: &str,
    case: &ScenarioCase,
    verbose: bool,
    show_output: bool,
) -> TestResult {
    // ── Install scripted human prompt for this case ──────────────────────────
    // Even when no `human_responses` are declared we still install a strict
    // empty queue so that any unexpected AskHuman/Confirm during the case
    // fails fast instead of blocking on real stdin.
    let scripted: Arc<dyn HumanPrompt> =
        Arc::new(ScriptedPrompt::new(case.human_responses.clone()));
    let prev_prompt = install_prompt(scripted);

    let start = Instant::now();

    let parse_result: std::result::Result<Action, _> = serde_json::from_value(case.action.clone());

    let action_result = match parse_result {
        Err(e) => ActionResult::err(
            "unknown",
            ErrorKind::InvalidInput,
            format!("Failed to parse action: {}", e),
            0,
        ),
        Ok(action) => match state.policy.evaluate(&action) {
            PolicyEffect::Allow => {
                executor::execute(
                    &action,
                    &state.config,
                    state.handles.clone(),
                    &state.policy,
                    &state.rate_limiter,
                    &state.composites,
                )
                .await
            }
            PolicyEffect::RequireConfirmation(msg) => ActionResult::err(
                action.type_name(),
                ErrorKind::PolicyDenied,
                format!("Action requires user approval: {}", msg),
                0,
            ),
            PolicyEffect::Deny(reason) => ActionResult::err(
                action.type_name(),
                ErrorKind::PolicyDenied,
                reason.message(),
                0,
            ),
        },
    };

    let duration_ms = start.elapsed().as_millis();

    // Restore the previous prompt before evaluating assertions so a panicking
    // assertion below cannot leak the scripted prompt into other cases.
    let _ = install_prompt(prev_prompt);

    // ── Assertions ────────────────────────────────────────────────────────────
    let mut failures: Vec<String> = Vec::new();

    if action_result.success != case.expect.success {
        let err_msg = action_result
            .error
            .as_ref()
            .map(|e| format!(" (error: {})", e.message))
            .unwrap_or_default();
        failures.push(format!(
            "success: expected {}, got {}{}",
            case.expect.success, action_result.success, err_msg
        ));
    }

    if let Some(expected_kind) = &case.expect.error_kind {
        match &action_result.error {
            Some(err) => {
                let actual_kind = serde_json::to_value(&err.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| format!("{:?}", err.kind));
                if actual_kind != *expected_kind {
                    failures.push(format!(
                        "error_kind: expected {}, got {} (message: {})",
                        expected_kind, actual_kind, err.message
                    ));
                }
            }
            None => failures.push(format!(
                "error_kind: expected {}, but no error present",
                expected_kind
            )),
        }
    }

    // Output-shape assertions (only when an output is present)
    let output_value: Option<Value> = action_result
        .output
        .as_ref()
        .and_then(|o| serde_json::to_value(o).ok());

    if let Some(expected_kind) = &case.expect.output_kind {
        let actual_kind = output_value.as_ref().and_then(|v| {
            v.get("kind")
                .and_then(|k| k.as_str().map(|s| s.to_string()))
        });
        match actual_kind {
            Some(k) if k == *expected_kind => {}
            Some(k) => failures.push(format!(
                "output_kind: expected {}, got {}",
                expected_kind, k
            )),
            None => failures.push(format!(
                "output_kind: expected {}, but output has no `kind`",
                expected_kind
            )),
        }
    }

    if let Some(needle) = &case.expect.output_contains {
        let json = output_value
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();
        if !json.contains(needle) {
            failures.push(format!(
                "output_contains: substring {:?} not found in output",
                needle
            ));
        }
    }

    if let Some(needle) = &case.expect.output_not_contains {
        let json = output_value
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();
        if json.contains(needle) {
            failures.push(format!(
                "output_not_contains: forbidden substring {:?} found in output",
                needle
            ));
        }
    }

    if let Some(pe) = &case.expect.pipeline {
        check_pipeline(pe, output_value.as_ref(), &mut failures);
    }
    if let Some(te) = &case.expect.try_block {
        check_try(te, output_value.as_ref(), &mut failures);
    }
    if let Some(ae) = &case.expect.assert_check {
        check_assert(ae, output_value.as_ref(), &mut failures);
    }
    if let Some(he) = &case.expect.human {
        check_human(he, output_value.as_ref(), &mut failures);
    }

    for se in &case.expect.side_effects {
        check_side_effect(se, &state.config, &mut failures);
    }

    if let Some(max) = case.expect.max_duration_ms {
        if duration_ms > max {
            failures.push(format!(
                "max_duration_ms: case took {}ms, expected ≤ {}ms",
                duration_ms, max
            ));
        }
    }
    if let Some(min) = case.expect.min_duration_ms {
        if duration_ms < min {
            failures.push(format!(
                "min_duration_ms: case took {}ms, expected ≥ {}ms",
                duration_ms, min
            ));
        }
    }

    let passed = failures.is_empty();
    let mut detail: Option<String> = if passed {
        None
    } else {
        Some(failures.join("\n       → "))
    };

    if !passed && verbose {
        let json = serde_json::to_string_pretty(&action_result).unwrap_or_default();
        let existing = detail.unwrap_or_default();
        detail = Some(format!(
            "{}\n    Full result:\n{}",
            existing,
            indent(&json, 6)
        ));
    }

    let output_preview = if show_output && passed {
        action_result.output.as_ref().and_then(|o| {
            let json = serde_json::to_string_pretty(o).ok()?;
            Some(indent(&json, 6))
        })
    } else {
        None
    };

    TestResult {
        scenario: scenario_name.to_string(),
        name: case.name.clone(),
        passed,
        duration_ms,
        detail,
        output_preview,
    }
}

// ── Structured assertion helpers ─────────────────────────────────────────────

fn check_pipeline(pe: &PipelineExpect, output: Option<&Value>, failures: &mut Vec<String>) {
    let Some(out) = output else {
        failures.push("pipeline: no output to inspect".into());
        return;
    };
    let kind = out.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    if kind != "pipeline_result" {
        failures.push(format!(
            "pipeline: expected output kind 'pipeline_result', got '{}'",
            kind
        ));
        return;
    }

    if let Some(expected) = pe.steps_total {
        let actual = out.get("steps_total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if actual != expected {
            failures.push(format!(
                "pipeline.steps_total: expected {}, got {}",
                expected, actual
            ));
        }
    }
    if let Some(expected) = pe.steps_completed {
        let actual = out
            .get("steps_completed")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        if actual != expected {
            failures.push(format!(
                "pipeline.steps_completed: expected {}, got {}",
                expected, actual
            ));
        }
    }
    if let Some(expected) = pe.stopped_early {
        let actual = out
            .get("stopped_early")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if actual != expected {
            failures.push(format!(
                "pipeline.stopped_early: expected {}, got {}",
                expected, actual
            ));
        }
    }
    if let Some(expected) = pe.snapshot_present {
        let actual = out
            .get("snapshot_name")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if actual != expected {
            failures.push(format!(
                "pipeline.snapshot_present: expected {}, got {}",
                expected, actual
            ));
        }
    }

    let results: Vec<Value> = out
        .get("results")
        .and_then(|r| r.as_array().cloned())
        .unwrap_or_default();

    if let Some(expected_flags) = &pe.step_success {
        if expected_flags.len() != results.len() {
            failures.push(format!(
                "pipeline.step_success: expected {} entries, got {} actual steps",
                expected_flags.len(),
                results.len()
            ));
        } else {
            for (i, expected) in expected_flags.iter().enumerate() {
                let actual = results[i]
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if actual != *expected {
                    failures.push(format!(
                        "pipeline.step_success[{}]: expected {}, got {}",
                        i, expected, actual
                    ));
                }
            }
        }
    }

    if let Some(expected_idx) = pe.first_failed_index {
        let actual_idx = results
            .iter()
            .position(|r| !r.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
        match actual_idx {
            Some(i) if i == expected_idx => {}
            Some(i) => failures.push(format!(
                "pipeline.first_failed_index: expected {}, got {}",
                expected_idx, i
            )),
            None => failures.push(format!(
                "pipeline.first_failed_index: expected {}, but no step failed",
                expected_idx
            )),
        }
    }
}

fn check_try(te: &TryExpect, output: Option<&Value>, failures: &mut Vec<String>) {
    let Some(out) = output else {
        failures.push("try: no output to inspect".into());
        return;
    };
    let kind = out.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    if kind != "try_result" {
        failures.push(format!(
            "try: expected output kind 'try_result', got '{}'",
            kind
        ));
        return;
    }

    if let Some(expected) = te.ok {
        let actual = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if actual != expected {
            failures.push(format!("try.ok: expected {}, got {}", expected, actual));
        }
    }
    if let Some(expected) = te.completed {
        let actual = out.get("completed").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if actual != expected {
            failures.push(format!(
                "try.completed: expected {}, got {}",
                expected, actual
            ));
        }
    }
    if let Some(expected) = te.total {
        let actual = out.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if actual != expected {
            failures.push(format!("try.total: expected {}, got {}", expected, actual));
        }
    }
    if let Some(expected) = te.rolled_back {
        let actual = out
            .get("rolled_back")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if actual != expected {
            failures.push(format!(
                "try.rolled_back: expected {}, got {}",
                expected, actual
            ));
        }
    }
    if let Some(expected) = te.snapshot_present {
        let actual = out
            .get("snapshot_name")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if actual != expected {
            failures.push(format!(
                "try.snapshot_present: expected {}, got {}",
                expected, actual
            ));
        }
    }
}

fn check_assert(ae: &AssertExpect, output: Option<&Value>, failures: &mut Vec<String>) {
    let Some(out) = output else {
        failures.push("assert: no output to inspect".into());
        return;
    };
    let kind = out.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    if kind != "assert_result" {
        failures.push(format!(
            "assert: expected output kind 'assert_result', got '{}'",
            kind
        ));
        return;
    }
    if let Some(expected) = ae.ok {
        let actual = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if actual != expected {
            failures.push(format!("assert.ok: expected {}, got {}", expected, actual));
        }
    }
    if let Some(expected) = &ae.check {
        let actual = out.get("check").and_then(|v| v.as_str()).unwrap_or("");
        if actual != expected {
            failures.push(format!(
                "assert.check: expected {}, got {}",
                expected, actual
            ));
        }
    }
}

fn check_human(he: &HumanExpect, output: Option<&Value>, failures: &mut Vec<String>) {
    let Some(out) = output else {
        failures.push("human: no output to inspect".into());
        return;
    };
    if let Some(expected) = &he.answer {
        match out.get("answer").and_then(|v| v.as_str()) {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(format!(
                "human.answer: expected {:?}, got {:?}",
                expected, actual
            )),
            None => failures.push("human.answer: output has no `answer` field".into()),
        }
    }
    if let Some(expected) = he.confirmed {
        match out.get("confirmed").and_then(|v| v.as_bool()) {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(format!(
                "human.confirmed: expected {}, got {}",
                expected, actual
            )),
            None => failures.push("human.confirmed: output has no `confirmed` field".into()),
        }
    }
    if let Some(expected) = he.choice_index {
        match out.get("choice_index").and_then(|v| v.as_u64()) {
            Some(actual) if actual as usize == expected => {}
            Some(actual) => failures.push(format!(
                "human.choice_index: expected {}, got {}",
                expected, actual
            )),
            None => failures.push(format!(
                "human.choice_index: expected {}, but field missing or null",
                expected
            )),
        }
    }
    if let Some(expected) = he.skipped {
        match out.get("skipped").and_then(|v| v.as_bool()) {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(format!(
                "human.skipped: expected {}, got {}",
                expected, actual
            )),
            None => failures.push("human.skipped: output has no `skipped` field".into()),
        }
    }
    if let Some(expected) = he.other {
        match out.get("other").and_then(|v| v.as_bool()) {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(format!(
                "human.other: expected {}, got {}",
                expected, actual
            )),
            None => failures.push("human.other: output has no `other` field".into()),
        }
    }
}

fn check_side_effect(se: &SideEffect, config: &AppConfig, failures: &mut Vec<String>) {
    match se {
        SideEffect::PathExists { path } => {
            if !std::path::Path::new(path).exists() {
                failures.push(format!("side_effect path_exists: {} not found", path));
            }
        }
        SideEffect::PathMissing { path } => {
            if std::path::Path::new(path).exists() {
                failures.push(format!(
                    "side_effect path_missing: {} exists but should not",
                    path
                ));
            }
        }
        SideEffect::FileContent { path, equals } => match std::fs::read_to_string(path) {
            Ok(content) if content == *equals => {}
            Ok(content) => failures.push(format!(
                "side_effect file_content[{}]: expected {:?}, got {:?}",
                path,
                equals,
                truncate(&content, 80)
            )),
            Err(e) => failures.push(format!(
                "side_effect file_content[{}]: read error: {}",
                path, e
            )),
        },
        SideEffect::FileContains { path, substring } => match std::fs::read_to_string(path) {
            Ok(content) if content.contains(substring) => {}
            Ok(content) => failures.push(format!(
                "side_effect file_contains[{}]: substring {:?} not found in {:?}",
                path,
                substring,
                truncate(&content, 80)
            )),
            Err(e) => failures.push(format!(
                "side_effect file_contains[{}]: read error: {}",
                path, e
            )),
        },
        SideEffect::SnapshotExists { name } => {
            let snap_dir = std::path::PathBuf::from(&config.snapshots.dir).join(name);
            if !snap_dir.exists() {
                failures.push(format!(
                    "side_effect snapshot_exists: {:?} not found",
                    snap_dir
                ));
            }
        }
        SideEffect::SnapshotMissing { name } => {
            let snap_dir = std::path::PathBuf::from(&config.snapshots.dir).join(name);
            if snap_dir.exists() {
                failures.push(format!(
                    "side_effect snapshot_missing: {:?} exists but should not",
                    snap_dir
                ));
            }
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// ── Output formatting ────────────────────────────────────────────────────────

fn print_result(result: &TestResult) {
    if result.passed {
        eprintln!(
            "  \x1b[32m✅\x1b[0m \x1b[1m{}\x1b[0m  \x1b[90m{}ms\x1b[0m",
            result.name, result.duration_ms
        );
        if let Some(ref out) = result.output_preview {
            eprintln!("{}", out);
        }
    } else {
        eprintln!(
            "  \x1b[31m❌\x1b[0m \x1b[1m{}\x1b[0m  \x1b[90m{}ms\x1b[0m",
            result.name, result.duration_ms
        );
        if let Some(detail) = &result.detail {
            eprintln!("       \x1b[31m→ {}\x1b[0m", detail);
        }
    }
}

fn indent(text: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{}{}", prefix, line))
        .collect::<Vec<_>>()
        .join("\n")
}
