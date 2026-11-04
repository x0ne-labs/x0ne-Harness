//! Every ceiling the model can name is the operator's to set, not the model's.
//!
//! Inventory of 2026-08-15, section 6 (« Limitation »), three findings that are
//! the same defect wearing three coats: a bound exists in the code, the model
//! supplies the value, and nothing compares the two.
//!
//! 1. `RunCommand.timeout_secs` went to `Duration::from_secs` unexamined. A
//!    model asking for `1_000_000_000` got 31 years of patience — and in
//!    streaming mode a handle that outlives the session holding a child.
//! 2. `run_command_streaming` pushed lines with no byte ceiling at all.
//!    `MAX_CAPTURE_BYTES` bounds the *blocking* capture only, so `yes` in
//!    streaming mode filled the handle buffer, the orchestrator's drain and
//!    the model's context for as long as anyone kept reading.
//! 3. `ReadFile.limit` overrode `policy.max_read_bytes` instead of being
//!    clamped by it, while `ReadFiles` next door clamped correctly
//!    (`efficient.rs:24-26`). Same policy key, two answers.
//!
//! A failure in this file means a model can once again choose how long the
//! runtime waits, or how many bytes it hands back — both of which the operator
//! wrote down in the policy file precisely so that it could not.
//!
//! The config is loaded through `AppConfig::load`, never parsed by hand: this
//! repository's recurring defect is the setting that is read but not *in the
//! path*, and only a real load proves the path.

use std::path::PathBuf;
use std::time::Instant;

use x0ne_runtime::action::Action;
use x0ne_runtime::composite::CompositeStore;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::executor;
use x0ne_runtime::handles;
use x0ne_runtime::policy::PolicyEngine;
use x0ne_runtime::result::ActionResult;

/// `max_command_timeout_secs = 2` and `max_read_bytes = 65536` are deliberately
/// far below anything a real deployment would use: the test asserts that the
/// *policy* number wins over the *model* number, and a small policy number is
/// the only way to see the difference within a test's patience.
fn config_toml(root: &str, ceiling: u64) -> String {
    format!(
        r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 65536
max_list_entries = 512
max_command_timeout_secs = {ceiling}
allowed_commands = ["sleep", "yes", "sh"]
allowed_read_paths = ["{root}"]
allowed_write_paths = ["{root}"]
deny_path_patterns = []
readable_env_vars = []

[actions]
RunCommand = true
ReadFile = true
ReadFiles = true
ReadLogs = true
SearchCode = true
SearchAndRead = true
Diff = true
"#
    )
}

struct Env {
    config: AppConfig,
    root: PathBuf,
}

fn setup(tag: &str) -> Env {
    setup_ceiling(tag, 2)
}

/// The streaming tests need a ceiling far above their own runtime: their
/// subject is the BYTE budget, and a clock that fires first would make them
/// pass without it. 600 s against a 20 s drain leaves no doubt about which
/// bound ended the child.
fn setup_ceiling(tag: &str, ceiling: u64) -> Env {
    let root = std::env::temp_dir().join(format!("x0ne-limits-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let cfg_path = root.join("x0ne-runtime.toml");
    std::fs::write(&cfg_path, config_toml(&root.to_string_lossy(), ceiling)).expect("config");
    let mut config = AppConfig::load(&cfg_path).expect("config must load");
    config.isolation.confine_helper_override = Some(env!("CARGO_BIN_EXE_x0ne-runtime").into());
    Env { config, root }
}

async fn run(action: &Action, env: &Env, h: handles::SharedHandles) -> ActionResult {
    let policy = PolicyEngine::new(env.config.clone(), env.config.actions.clone());
    let rate_limiter: executor::desktop::RateLimiter = std::sync::Arc::new(
        tokio::sync::Mutex::new(Instant::now() - std::time::Duration::from_secs(60)),
    );
    let composites = CompositeStore::new(&env.config);
    executor::execute(action, &env.config, h, &policy, &rate_limiter, &composites).await
}

/// Serialized size of what the action hands back to the model. The budget is
/// about the model's context, so the measure is the payload it receives, not
/// the internal buffer that produced it.
fn payload_len(r: &ActionResult) -> usize {
    serde_json::to_string(&r.output)
        .map(|s| s.len())
        .unwrap_or(0)
}

// ── (a) the timeout ──────────────────────────────────────────────────────────

/// A model-supplied `timeout_secs` is clamped to `policy.max_command_timeout_secs`,
/// and the answer says so.
///
/// A failure means either that the clamp is gone — a model can pin a runtime
/// thread on a hung child for as long as it likes — or that the clamp became
/// silent, which is worse than no clamp: the model would read a partial result
/// as a complete one and act on it.
#[tokio::test]
async fn a_model_timeout_is_clamped_and_the_clamp_is_visible() {
    let env = setup("timeout");
    let h = handles::new_shared(1000, 300, 20);

    let action = Action::RunCommand {
        command: "sleep".into(),
        args: vec!["30".into()],
        cwd: None,
        env: Default::default(),
        timeout_secs: Some(1_000_000_000),
        stream: Some(false),
    };

    let started = Instant::now();
    let result = run(&action, &env, h).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "the ceiling is 2s and the child sleeps 30s; the call took {:?}, \
         so the model's 1e9 was used verbatim",
        elapsed
    );

    let stderr = match &result.output {
        Some(x0ne_runtime::result::ActionOutput::CommandOutput {
            stderr, timed_out, ..
        }) => {
            assert!(
                timed_out,
                "the child outlived the ceiling: it must be killed"
            );
            stderr.clone()
        }
        other => panic!("expected CommandOutput, got {other:?}"),
    };
    assert!(
        stderr.contains("clamped") && stderr.contains("1000000000") && stderr.contains('2'),
        "the clamp must name the value asked for and the ceiling applied; got: {stderr}"
    );
}

/// A policy file written before the key existed still gets a finite ceiling.
///
/// `max_command_timeout_secs` is `serde(default)` so that every config already
/// on disk keeps loading. A failure means that default became infinite — every
/// existing installation would be back to letting the model choose, and no
/// other test would notice because they all set the key.
#[tokio::test]
async fn a_policy_file_without_the_key_still_has_a_ceiling() {
    let env = setup("nokey");
    let cfg_path = env.root.join("no-key.toml");
    let toml =
        config_toml(&env.root.to_string_lossy(), 2).replace("max_command_timeout_secs = 2\n", "");
    assert!(
        !toml.contains("max_command_timeout_secs"),
        "key must be absent"
    );
    std::fs::write(&cfg_path, toml).expect("config");
    let config = AppConfig::load(&cfg_path).expect("a config without the key must still load");
    assert_eq!(
        config.policy.max_command_timeout_secs, 3600,
        "the absent-key default must stay finite and match the runtime's own \
         longest self-granted timeout"
    );
}

// ── (b) the streaming budget ─────────────────────────────────────────────────

/// A streaming command cannot push more than the streaming budget, and the
/// child is stopped once the budget is gone.
///
/// A failure means `yes` (or any chatty build) can fill the handle buffer, the
/// orchestrator's drain and the model's context without limit, and keep a CPU
/// busy doing it. The measure is cumulative across every drain, because the
/// handle ring caps the number of events, never their size.
#[tokio::test]
async fn a_streaming_command_cannot_exceed_the_capture_budget() {
    let env = setup_ceiling("stream", 600);
    // `yes` is the shortest way to a flood: it writes one line as fast as the
    // pipe accepts it and never stops on its own.
    let d = drain_streaming(&env, "yes", &["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]).await;

    assert!(
        d.truncation_seen,
        "the stream must announce its own truncation; {} bytes went out silently",
        d.total
    );
    assert!(
        d.terminal,
        "the child must be stopped once nobody is allowed to listen; \
         it was still running after {} bytes",
        d.total
    );
    // `raw` is what the budget counts: line bytes plus their newline. `total`
    // is the same content inside its JSON envelope (~3x for lines this short),
    // which is what actually lands in the model's context — asserted too, so a
    // budget that counted the wrong thing cannot pass.
    let budget = x0ne_runtime::executor::process::MAX_STREAM_BYTES;
    assert!(
        d.raw <= budget + 4096,
        "streamed {} line bytes against a {budget} byte budget",
        d.raw
    );
    assert!(
        d.total < budget * 8,
        "streamed payload was {} bytes for a {budget} byte budget",
        d.total
    );
}

/// A child that ignores SIGPIPE is stopped anyway.
///
/// This is the test that says why the kill exists. Closing the pipe is enough
/// for a well-behaved program — `yes` dies of SIGPIPE the moment the reader
/// drops — so a runtime that merely stopped reading would look correct against
/// `yes` and be wrong here: `trap "" PIPE` turns the signal into an EPIPE the
/// shell ignores, and the loop spins on a core forever. A failure means the
/// budget stops the *transcript* but not the *process*, which is the half of
/// the problem the operator pays for.
#[tokio::test]
async fn a_streaming_child_that_ignores_sigpipe_is_stopped_anyway() {
    let env = setup_ceiling("sigpipe", 600);
    let d = drain_streaming(
        &env,
        "sh",
        &[
            "-c",
            "trap '' PIPE; while :; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done",
        ],
    )
    .await;

    assert!(d.truncation_seen, "the budget must be reached at all");
    assert!(
        d.terminal,
        "a child deaf to SIGPIPE was left running after {} bytes",
        d.total
    );
}

struct Drained {
    raw: usize,
    total: usize,
    truncation_seen: bool,
    terminal: bool,
}

/// Start a streaming command and drain it repeatedly, as the orchestrator does, while it lives;
/// the sum is what reaches the model. `process::run_command`, like `executor::execute`
/// (documented "already policy-approved"), skips the policy gate: the per-command shell check does not apply here.
async fn drain_streaming(env: &Env, command: &str, args: &[&str]) -> Drained {
    let h = handles::new_shared(1000, 300, 20);
    let action = Action::RunCommand {
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        env: Default::default(),
        // Well under the policy ceiling and far beyond the drain deadline: the
        // budget must be what ends this child, not the clock.
        timeout_secs: Some(600),
        stream: Some(true),
    };
    let result =
        x0ne_runtime::executor::process::run_command(&action, &env.config, h.clone()).await;
    let handle = match &result.output {
        Some(x0ne_runtime::result::ActionOutput::HandleCreated { handle, .. }) => handle.clone(),
        other => panic!("expected HandleCreated, got {other:?}"),
    };

    let mut d = Drained {
        raw: 0,
        total: 0,
        truncation_seen: false,
        terminal: false,
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut mgr = h.lock().await;
        for ev in mgr.drain_events(&handle).unwrap_or_default() {
            d.total += serde_json::to_string(&ev).map(|s| s.len()).unwrap_or(0);
            d.raw += ev
                .get("line")
                .and_then(|l| l.as_str())
                .map(|l| l.len() + 1)
                .unwrap_or(0);
            if ev.get("stream").and_then(|s| s.as_str()) == Some("truncated") {
                d.truncation_seen = true;
            }
        }
        if let Some(status) = mgr.get_status(&handle) {
            if status.is_terminal() {
                d.terminal = true;
            }
        }
        drop(mgr);
        if d.terminal && d.truncation_seen {
            break;
        }
    }
    d
}

// ── (c) every file reader ────────────────────────────────────────────────────

/// Every action that hands file *content* back to the model honours
/// `policy.max_read_bytes`, whatever limit the model asked for.
///
/// This is an enumeration on purpose. The defect it guards is not one missing
/// `.min()` — it is the divergence: `ReadFiles` clamped, `ReadFile` did not,
/// and the two lived four files apart for months. A new reader that forgets
/// the clamp fails here, which is the only place the omission is visible.
#[tokio::test]
async fn every_file_reader_honours_max_read_bytes() {
    let env = setup("readers");
    let max = env.config.policy.max_read_bytes as usize; // 65_536

    // 4 MiB, in lines long enough that `max_list_entries` alone (512) cannot
    // bound the payload: 512 × 4 KiB = 2 MiB, thirty times the byte budget.
    let line = format!("needle {}\n", "x".repeat(4088));
    let big: String = line.repeat(4 * 1024 * 1024 / line.len());
    let big_path = env.root.join("big.log");
    std::fs::write(&big_path, &big).expect("big file");
    // One 4 MiB line, in its own directory so the search cases can be rooted
    // on it alone. A line COUNT bounds nothing here: there is one line.
    let oneline_dir = env.root.join("oneline");
    std::fs::create_dir_all(&oneline_dir).expect("oneline dir");
    std::fs::write(
        oneline_dir.join("one.log"),
        format!("needle {}", "x".repeat(4 * 1024 * 1024)),
    )
    .expect("one-line file");
    let one = oneline_dir.join("one.log").to_string_lossy().to_string();
    let one_root = oneline_dir.to_string_lossy().to_string();

    let other_path = env.root.join("other.log");
    std::fs::write(&other_path, big.replace("needle", "haypin")).expect("other file");
    let p = big_path.to_string_lossy().to_string();
    let q = other_path.to_string_lossy().to_string();

    let readers: Vec<(&str, Action)> = vec![
        (
            "ReadFile(limit=u64::MAX)",
            Action::ReadFile {
                path: p.clone(),
                offset: 0,
                limit: Some(u64::MAX),
                start_line: None,
                limit_lines: None,
            },
        ),
        (
            "ReadFile(line mode, limit_lines huge)",
            Action::ReadFile {
                path: p.clone(),
                offset: 0,
                limit: Some(u64::MAX),
                start_line: Some(1),
                limit_lines: Some(1_000_000),
            },
        ),
        (
            "ReadFiles(max_bytes_per_file=u64::MAX)",
            Action::ReadFiles {
                paths: vec![p.clone()],
                max_bytes_per_file: Some(u64::MAX),
            },
        ),
        (
            "ReadLogs(lines=usize::MAX)",
            Action::ReadLogs {
                path: p.clone(),
                lines: Some(usize::MAX),
            },
        ),
        (
            "SearchCode(limit=usize::MAX)",
            Action::SearchCode {
                root: env.root.to_string_lossy().to_string(),
                pattern: "needle".into(),
                patterns: vec![],
                file_glob: None,
                limit: Some(usize::MAX),
            },
        ),
        (
            "SearchAndRead(limit=usize::MAX)",
            Action::SearchAndRead {
                root: env.root.to_string_lossy().to_string(),
                pattern: "needle".into(),
                patterns: vec![],
                file_glob: None,
                context_lines: Some(20),
                limit: Some(usize::MAX),
            },
        ),
        (
            "ReadFile(one 4 MiB line)",
            Action::ReadFile {
                path: one.clone(),
                offset: 0,
                limit: Some(u64::MAX),
                start_line: None,
                limit_lines: None,
            },
        ),
        (
            "ReadLogs(one 4 MiB line)",
            Action::ReadLogs {
                path: one.clone(),
                lines: Some(usize::MAX),
            },
        ),
        (
            "SearchCode(one 4 MiB line)",
            Action::SearchCode {
                root: one_root.clone(),
                pattern: "needle".into(),
                patterns: vec![],
                file_glob: None,
                limit: Some(usize::MAX),
            },
        ),
        (
            "SearchAndRead(one 4 MiB line)",
            Action::SearchAndRead {
                root: one_root.clone(),
                pattern: "needle".into(),
                patterns: vec![],
                file_glob: None,
                context_lines: Some(20),
                limit: Some(usize::MAX),
            },
        ),
        (
            "Diff(files=true)",
            Action::Diff {
                a: p.clone(),
                b: q.clone(),
                files: true,
                context_lines: None,
            },
        ),
    ];

    // Serialization overhead (JSON escaping, per-match path and line fields)
    // is real but bounded; 4× the budget is generous and still 8× below the
    // 2 MiB an unclamped reader produced when this test was written RED.
    let ceiling = max * 4;
    let mut over = Vec::new();
    for (name, action) in readers {
        let h = handles::new_shared(1000, 300, 20);
        let r = run(&action, &env, h).await;
        let n = payload_len(&r);
        if n > ceiling {
            over.push(format!("{name}: {n} bytes"));
        }
    }
    assert!(
        over.is_empty(),
        "these readers exceeded max_read_bytes ({max}) by more than the \
         serialization allowance ({ceiling}): {over:?}"
    );
}
