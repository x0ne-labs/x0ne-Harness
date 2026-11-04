//! Corpus B: the emission bench. THE criterion of the asm-ai phase.
//!
//! Definition (ii): "valid" = the program parses AND passes the verifier
//! against the test policy. On out_of_grammar and adversarial statements the
//! right answer is no program at all; a program there counts 0.
//!
//! The bench and its stratification check are `#[ignore]`, started by
//! `make asm-bench`; the sourcing guards below run in `cargo test`. The model is reached through the Anthropic
//! Messages API with a dollar cap measured on the usage the API returns.
//! The runtime is reached through the stdio bridge, never linked.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Deserialize;
use serde_json::{json, Value};
use x0ne_orchestrator::api::{AnthropicAuth, AnthropicClient, LlmBackend};
use x0ne_orchestrator::conversation::{Message, Role};

const MODEL: &str = "claude-haiku-4-5";
/// USD per million tokens, Haiku 4.5 (published price, 2026-10-05).
const USD_PER_M_INPUT: f64 = 1.0;
const USD_PER_M_OUTPUT: f64 = 5.0;
const USD_PER_M_CACHE_WRITE: f64 = 1.25;
const USD_PER_M_CACHE_READ: f64 = 0.10;

#[derive(Deserialize)]
struct Utterance {
    utterance: String,
    stratum: String,
    #[serde(default)]
    reviewed_by_owner: bool,
    #[serde(default)]
    author: String,
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn corpus() -> Vec<Utterance> {
    std::fs::read_to_string(fixture("corpus_b.jsonl"))
        .expect("the corpus must have been harvested: scripts/corpus_b_harvest.py")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL line"))
        .collect()
}

/// The sourcing guard. Without a human review the metric measures the
/// harvest script's classifier and the implementer, not the model.
#[test]
#[ignore = "needs the harvested fixture; run with `make asm-bench`"]
fn the_corpus_is_stratified_and_partly_reviewed_by_a_human() {
    let c = corpus();
    assert_eq!(c.len(), 50, "50 statements");
    let count = |s: &str| c.iter().filter(|u| u.stratum == s).count();
    assert_eq!(count("easy"), 20);
    assert_eq!(count("awkward"), 15);
    assert_eq!(count("out_of_grammar"), 10);
    assert_eq!(count("adversarial"), 5);
    assert!(
        c.iter().filter(|u| u.reviewed_by_owner).count() >= 10,
        "at least ten statements must carry the owner's review"
    );
}

/// No committed row may be a turn read from a real session: those are a
/// person's own messages (2026-10-16). Harvested rows stay local.
#[test]
fn the_committed_corpus_holds_no_session_turn() {
    let text = std::fs::read_to_string(fixture("corpus_b.jsonl")).expect("corpus fixture");
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).expect("valid JSONL line");
        assert_ne!(row["author"], "harvest", "a harvested session turn: {line}");
        assert!(
            row.get("source_file").is_none(),
            "a session file name: {line}"
        );
    }
}

/// Without `--from-my-sessions` the harvest script refuses and writes nothing.
#[test]
fn the_harvest_reads_sessions_only_with_explicit_consent() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join(".x0ne/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("s.jsonl"),
        "{\"type\":\"user_input\",\"content\":\"CANARY please refactor the parser module\"}\n",
    )
    .unwrap();
    let script = format!(
        "{}/../scripts/corpus_b_harvest.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let run = |consent: bool| {
        let out = home
            .path()
            .join(if consent { "yes.jsonl" } else { "no.jsonl" });
        let mut cmd = Command::new("python3");
        cmd.arg(&script)
            .arg("--out")
            .arg(&out)
            .env("HOME", home.path());
        if consent {
            cmd.arg("--from-my-sessions");
        }
        (
            cmd.output().expect("python3 runs"),
            std::fs::read_to_string(&out).ok(),
        )
    };
    let (refused, written) = run(false);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(written.is_none(), "no output without consent");
    let (allowed, written) = run(true);
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(written.unwrap_or_default().contains("CANARY"));
}

/// The bench. One table per stratum; the go/no-go bears on strata 1-2 only.
#[test]
#[ignore = "calls a model and spends money; run with `make asm-bench`"]
fn measure_the_first_shot_valid_program_rate() {
    let cap_usd: f64 = std::env::var("ASM_BENCH_CAP_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let out_path = std::env::var("ASM_BENCH_OUT").unwrap_or_else(|_| "/tmp/asm-bench.jsonl".into());
    let mut out = std::fs::File::create(&out_path).expect("results file");

    let mut runtime = RuntimeBridge::spawn();
    let actions = runtime.list_actions();
    assert!(
        actions
            .iter()
            .any(|a| a["type"].as_str() == Some("PreviewProgram")),
        "the bench policy must expose PreviewProgram"
    );
    let mut cfg = x0ne_orchestrator::config::Config::default();
    cfg.llm.provider = "anthropic".into();
    cfg.llm.anthropic.model = std::env::var("ASM_BENCH_MODEL").unwrap_or_else(|_| MODEL.into());
    cfg.ui.lazy_discovery = false;
    cfg.ui.force_full_prompt = true;
    let system = x0ne_orchestrator::prompt::build_system_prompt_dynamic(
        &cfg,
        false,
        Some("rust: toy (a small crate: src/, tests/, docs/)"),
        Some(&actions),
    );
    assert!(
        system.contains("#!asm-ai/v0"),
        "the guide must be in the prompt"
    );

    let model = Model::new(&cfg.llm.anthropic.model);
    let mut per_stratum: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for u in corpus() {
        let reply = model.ask(&system, &u.utterance);
        let spent = model.spent_usd();
        assert!(
            spent <= cap_usd,
            "cost cap reached: {spent:.4} USD > {cap_usd} USD; stopping"
        );
        let emitted = extract_program(&reply);
        let (valid, why) = match u.stratum.as_str() {
            "out_of_grammar" | "adversarial" => match &emitted {
                None => (true, "no program (correct)".to_string()),
                Some(_) => (false, "emitted a program where none was right".to_string()),
            },
            _ => match &emitted {
                None => (false, "no program".to_string()),
                Some(src) => program_verdict(src, &mut runtime),
            },
        };
        let e = per_stratum.entry(u.stratum.clone()).or_default();
        e.1 += 1;
        if valid {
            e.0 += 1;
        }
        println!(
            "[{:>14}] {} — {} — {}",
            u.stratum,
            if valid { "ok " } else { "KO " },
            why,
            u.utterance
        );
        let row = json!({
            "stratum": u.stratum, "author": u.author, "utterance": u.utterance,
            "valid": valid, "why": why, "program": emitted, "reply": reply,
            "spent_usd_so_far": spent,
        });
        writeln!(out, "{row}").unwrap();
    }
    for (stratum, (ok, total)) in &per_stratum {
        println!(
            "{stratum:>16} : {ok}/{total} ({:.0}%)",
            100.0 * *ok as f64 / *total as f64
        );
    }
    let (ok, total) = ["easy", "awkward"]
        .iter()
        .filter_map(|s| per_stratum.get(*s))
        .fold((0, 0), |(a, b), (o, t)| (a + o, b + t));
    let rate = 100.0 * ok as f64 / total as f64;
    println!("STRATA 1-2 : {ok}/{total} ({rate:.1}%) — go/no-go threshold: 95%");
    println!(
        "model {} — tokens in {} out {} cache-write {} cache-read {} — {:.4} USD (cap {cap_usd}) — results in {out_path}",
        cfg.llm.anthropic.model,
        model.input_tokens.get(),
        model.output_tokens.get(),
        model.cache_write.get(),
        model.cache_read.get(),
        model.spent_usd()
    );
    assert!(rate >= 95.0, "under the threshold: {rate:.1}%");
}

/// Definition (ii), with the reason when it fails.
fn program_verdict(src: &str, runtime: &mut RuntimeBridge) -> (bool, String) {
    if let Err(r) = x0ne_asm::parse(src) {
        return (
            false,
            format!("parse: {}", r.render().lines().next().unwrap_or("")),
        );
    }
    let resp = runtime.call(&json!({"type": "PreviewProgram", "source": src}));
    if resp["success"].as_bool() == Some(true) {
        return (true, "parses and verifies".into());
    }
    let msg = resp["error"]["message"]
        .as_str()
        .unwrap_or("no error message");
    (
        false,
        format!("verifier: {}", msg.lines().next().unwrap_or("")),
    )
}

/// The first program in the reply: a `"source"` JSON string in an action, or
/// a fenced block starting with the shebang.
fn extract_program(text: &str) -> Option<String> {
    if let Some(start) = text.find("\"source\"") {
        let rest = text[start + "\"source\"".len()..]
            .trim_start()
            .trim_start_matches(':')
            .trim_start();
        if let Some(Ok(s)) = serde_json::Deserializer::from_str(rest)
            .into_iter::<String>()
            .next()
        {
            if s.trim_start().starts_with("#!asm-ai/v0") {
                return Some(s);
            }
        }
    }
    for block in text.split("```").skip(1).step_by(2) {
        let src = block.trim_start_matches("asm-ai").trim_start();
        if src.starts_with("#!asm-ai/v0") {
            return Some(src.to_string());
        }
    }
    None
}

/// The model under test, with the cost measured on the usage the API returns.
struct Model {
    client: AnthropicClient,
    rt: tokio::runtime::Runtime,
    input_tokens: std::cell::Cell<u64>,
    output_tokens: std::cell::Cell<u64>,
    cache_write: std::cell::Cell<u64>,
    cache_read: std::cell::Cell<u64>,
}

impl Model {
    fn new(model: &str) -> Self {
        let key = api_key();
        let client = AnthropicClient::new(AnthropicAuth::ApiKey(key), model, 1024).expect("client");
        Self {
            client,
            rt: tokio::runtime::Runtime::new().expect("tokio"),
            input_tokens: Default::default(),
            output_tokens: Default::default(),
            cache_write: Default::default(),
            cache_read: Default::default(),
        }
    }

    fn ask(&self, system: &str, utterance: &str) -> String {
        let msgs = [Message {
            role: Role::User,
            content: utterance.to_string(),
            images: Vec::new(),
        }];
        let resp = self
            .rt
            .block_on(self.client.send(system, &msgs))
            .expect("Anthropic reply");
        self.input_tokens
            .set(self.input_tokens.get() + resp.input_tokens);
        self.output_tokens
            .set(self.output_tokens.get() + resp.output_tokens);
        self.cache_write
            .set(self.cache_write.get() + resp.cache_creation_input_tokens);
        self.cache_read
            .set(self.cache_read.get() + resp.cache_read_input_tokens);
        resp.content
    }

    fn spent_usd(&self) -> f64 {
        (self.input_tokens.get() as f64 * USD_PER_M_INPUT
            + self.output_tokens.get() as f64 * USD_PER_M_OUTPUT
            + self.cache_write.get() as f64 * USD_PER_M_CACHE_WRITE
            + self.cache_read.get() as f64 * USD_PER_M_CACHE_READ)
            / 1_000_000.0
    }
}

/// `ANTHROPIC_API_KEY` from the environment, else from the sandbox env file.
/// The value is never printed.
fn api_key() -> String {
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        if !k.is_empty() {
            return k;
        }
    }
    let path = std::env::var("ASM_BENCH_ENV_FILE").unwrap_or_else(|_| {
        format!(
            "{}/.config/x0ne-sandbox.env",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    text.lines()
        .filter_map(|l| l.strip_prefix("ANTHROPIC_API_KEY="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .find(|v| !v.is_empty())
        .expect("ANTHROPIC_API_KEY= line in the env file")
}

/// `x0ne-runtime --mode stdio` on the bench policy. The bridge, not a link.
/// `X0NE_ASM_ROOT` anchors the patterns on the disposable tree (T3 escape).
struct RuntimeBridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RuntimeBridge {
    fn spawn() -> Self {
        let bin =
            std::env::var("ASM_BENCH_RUNTIME").expect("ASM_BENCH_RUNTIME, set by `make asm-bench`");
        let mut child = Command::new(bin)
            .args([
                "--config",
                &fixture("asm_bench_policy.toml"),
                "--mode",
                "stdio",
                "--isolation",
                "none",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("X0NE_ASM_ROOT", fixture("asm_bench_tree"))
            .env("X0NE_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("runtime starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn call(&mut self, action: &Value) -> Value {
        writeln!(self.stdin, "{action}").expect("write to runtime");
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read from runtime");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("runtime answered non-JSON ({e}): {line}"))
    }

    fn list_actions(&mut self) -> Vec<Value> {
        self.call(&json!({"type": "ListActions"}))["output"]["actions"]
            .as_array()
            .cloned()
            .expect("ListActions returns output.actions")
    }
}

impl Drop for RuntimeBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn extract_program_reads_an_action_or_a_fenced_block() {
    let action = r##"I will preview it. <action>{"type":"PreviewProgram","source":"#!asm-ai/v0\nfrom files \"**/*.md\"\nread lines /^## /\nemit table\n"}</action>"##;
    assert!(extract_program(action)
        .unwrap()
        .starts_with("#!asm-ai/v0\nfrom files"));
    let fenced =
        "Here:\n```asm-ai\n#!asm-ai/v0\nfrom files \"src/**/*.rs\"\ncount files\nemit value\n```\n";
    assert!(extract_program(fenced).unwrap().contains("count files"));
    assert!(extract_program("I would rather not; this needs a loop.").is_none());
    assert!(extract_program(r#"{"type":"ReadFile","source":"notes"}"#).is_none());
}
