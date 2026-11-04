//! The audit trail, proven end to end on the canonical path.
//!
//! # What this file is answering
//!
//! The README promises, four times over, that every action lands in the
//! session log — "two lines", "a full audit trail of every command, exit code,
//! and output". Measured on 2026-08-15 against `~/.x0ne/sessions`: 22 real
//! sessions whose own `turn` events declare 250 actions between them, and
//! **zero** `action` lines written. The 360 `action` events that exist across
//! 40 sessions all carry `command == "gcloud"` — written by `x0ne cc`
//! (`x0ne-code/src/cc_cmd.rs:1147`), the single live caller of `log_action`
//! in the product.
//!
//! The cause was not a bug inside the logger. `SessionLogger::log_action`
//! works and has always worked; the eleven calls to it lived in
//! `agent_loop.rs`, whose only two callers hard-code the logger to `None`,
//! while the canonical path (`CliAgentSession → Driver → TurnReport`) had no
//! reference to a logger at all. The feature compiled, was documented, was
//! tested in isolation, and was not in the path — the failure mode docs/invariants.md
//! calls the dead scaffold.
//!
//! So these tests deliberately do **not** call the logger. They run a turn the
//! way a frontend runs one, through `CliAgentSession`, and then read the file
//! off the disk. A test that called `log_action` itself would have passed
//! throughout the eight months the product recorded nothing.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use x0ne_orchestrator::agent::approval::ConfirmationResponse;
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{ApiResponse, LlmBackend};
use x0ne_orchestrator::bridge::Bridge;
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::conversation::Message;
use x0ne_orchestrator::display_sink::{
    ConfirmOutcome, ConfirmationRequest, DisplaySink, NullDisplaySink,
};
use x0ne_orchestrator::session::{SessionLogger, SharedJournal};
use x0ne_orchestrator::tui::wire::AgentEvent;

// ── Fakes ─────────────────────────────────────────────────────────────────────

/// A model that says exactly what the test tells it to, in order.
struct ScriptedModel {
    replies: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(replies: &[&str]) -> Self {
        Self {
            replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
        }
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedModel {
    async fn send(&self, _system: &str, _messages: &[Message]) -> anyhow::Result<ApiResponse> {
        let mut q = self.replies.lock().unwrap();
        if q.is_empty() {
            anyhow::bail!("script exhausted — the turn asked more times than expected");
        }
        Ok(ApiResponse {
            content: q.remove(0),
            input_tokens: 7,
            output_tokens: 3,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            stop_reason: "end_turn".into(),
        })
    }
    fn display_name(&self) -> String {
        "scripted/model".into()
    }
}

/// A runtime that answers every action with a fixed envelope.
struct FixedBridge {
    envelope: Value,
}

#[async_trait::async_trait]
impl Bridge for FixedBridge {
    async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
        Ok(self.envelope.clone())
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

/// A human who answers the same way every time.
struct AnsweringSink {
    outcome: ConfirmOutcome,
}

impl DisplaySink for AnsweringSink {
    fn emit(&mut self, _event: &AgentEvent) {}
    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        Some(ConfirmationResponse {
            id: request.id,
            outcome: self.outcome,
        })
    }
}

// ── Harness ───────────────────────────────────────────────────────────────────

fn action(json_line: &str) -> String {
    format!("<action>{json_line}</action>")
}

/// Assemble a session exactly as a frontend does, run one turn, and hand back
/// what the session log contains afterwards.
async fn turn_events(replies: &[&str], envelope: Value, sink: Box<dyn DisplaySink>) -> Vec<Value> {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("session.jsonl");
    let journal = SharedJournal::wrap(SessionLogger::at(path.clone()).await.unwrap());

    let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel::new(replies));
    let mut agent = CliAgentSession::new(
        Config::default(),
        backend,
        "sys",
        Some(Box::new(FixedBridge { envelope })),
        sink,
    );
    // The one line under test. Everything else here is scaffolding.
    agent.journal = Some(journal.handle());

    let _report = agent.run_turn("do the thing").await;

    // Read while `tmp` is still alive — dropping it removes the directory.
    let raw = tokio::fs::read_to_string(&path).await.unwrap();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("every line is valid JSON"))
        .collect()
}

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["type"].as_str() == Some(ty))
        .collect()
}

// ── The properties ────────────────────────────────────────────────────────────

/// Two lines per action — what was asked, then what happened — for every
/// action the canonical path dispatches, with the command, its exit code and
/// its output recoverable from the second.
///
/// A failure here means the product's central audit claim is false again: a
/// turn ran, the host changed, and the session log says nothing about it.
#[tokio::test]
async fn every_action_on_the_canonical_path_leaves_two_lines_in_the_session_log() {
    let events = turn_events(
        &[
            &action(r#"{"type":"RunCommand","command":"ls","args":["-la"]}"#),
            "Listed it.",
        ],
        json!({
            "success": true,
            "output": {"stdout": "total 0\n", "stderr": "", "exit_code": 0}
        }),
        Box::new(AnsweringSink {
            outcome: ConfirmOutcome::Yes,
        }),
    )
    .await;

    let actions = of_type(&events, "action");
    let results = of_type(&events, "action_result");
    assert_eq!(
        actions.len(),
        1,
        "one dispatched action, one `action` line — got {events:#?}"
    );
    assert_eq!(
        results.len(),
        1,
        "…and exactly one `action_result` beside it"
    );

    assert_eq!(actions[0]["action_type"], "RunCommand");
    assert_eq!(actions[0]["action"]["command"], "ls");
    assert_eq!(actions[0]["success"], true);

    // The audit half: an auditor must be able to read what the command did
    // without the terminal that ran it.
    assert_eq!(results[0]["result"]["output"]["exit_code"], 0);
    assert_eq!(results[0]["result"]["output"]["stdout"], "total 0\n");

    // Order matters: the request is recorded before its outcome, so a log that
    // stops mid-action still shows what was attempted.
    let ia = events.iter().position(|e| e["type"] == "action").unwrap();
    let ir = events
        .iter()
        .position(|e| e["type"] == "action_result")
        .unwrap();
    assert!(ia < ir, "the action must be journaled before its result");
}

/// The runtime's policy engine refused: recorded as a security event of its
/// own, on top of the action's two lines.
///
/// A failure here means a blocked action is indistinguishable from an action
/// that merely failed — which is exactly the question an audit is opened to
/// answer.
#[tokio::test]
async fn a_policy_denial_is_journaled() {
    let events = turn_events(
        &[
            &action(r#"{"type":"ReadFile","path":"/etc/shadow"}"#),
            "I cannot read that.",
        ],
        json!({
            "success": false,
            "error": {"class": "policy_denied", "message": "path outside allowed_dirs"}
        }),
        Box::new(NullDisplaySink),
    )
    .await;

    let denials = of_type(&events, "policy_denial");
    assert_eq!(denials.len(), 1, "the refusal is missing from {events:#?}");
    assert_eq!(denials[0]["denial"], "runtime_policy");
    assert_eq!(denials[0]["action_type"], "ReadFile");
    assert_eq!(denials[0]["reason"], "path outside allowed_dirs");

    // It was really dispatched, so the ordinary pair is there too.
    assert_eq!(of_type(&events, "action").len(), 1);
    assert_eq!(of_type(&events, "action_result").len(), 1);
}

/// A human was asked and said no. The action never ran, so there is no result
/// to record — but the refusal itself is the event worth keeping.
///
/// A failure here means the log cannot answer "did anyone approve this?", and
/// a refused destructive action leaves no trace at all.
#[tokio::test]
async fn a_human_refusal_is_journaled() {
    let events = turn_events(
        &[&action(r#"{"type":"MoveToTrash","path":"/tmp/x"}"#)],
        json!({"success": true, "output": {}}),
        Box::new(AnsweringSink {
            outcome: ConfirmOutcome::No,
        }),
    )
    .await;

    let denials = of_type(&events, "policy_denial");
    assert_eq!(denials.len(), 1, "the refusal is missing from {events:#?}");
    assert_eq!(denials[0]["denial"], "human_declined");
    assert_eq!(denials[0]["action_type"], "MoveToTrash");
    assert_eq!(denials[0]["action"]["path"], "/tmp/x");

    // Nothing ran, so nothing is recorded as having run. A `success: false`
    // action line here would misreport a refusal as a failed attempt.
    assert!(
        of_type(&events, "action").is_empty(),
        "a refused action must not appear as dispatched: {events:#?}"
    );
}

/// No human could be asked — piped stdin, a headless server, a sub-agent — and
/// the gate failed closed. That is a different fact from a human saying no,
/// and the log has to say which one happened.
#[tokio::test]
async fn a_refusal_with_nobody_to_ask_is_journaled_as_such() {
    let events = turn_events(
        &[&action(r#"{"type":"MoveToTrash","path":"/tmp/x"}"#)],
        json!({"success": true, "output": {}}),
        Box::new(NullDisplaySink),
    )
    .await;

    let denials = of_type(&events, "policy_denial");
    assert_eq!(denials.len(), 1, "the refusal is missing from {events:#?}");
    assert_eq!(
        denials[0]["denial"], "no_human_channel",
        "a refusal nobody was asked about must not read as a human's decision"
    );
}

/// A session log holds every prompt, every answer and every command run. It is
/// created owner-only, in an owner-only directory.
///
/// A failure here means transcripts of the operator's work are readable by
/// every account on the machine — the README says "treat sessions as chmod
/// 600", and until 2026-08-15 that was advice the code did not follow: 0664
/// files in a 0775 directory.
#[cfg(unix)]
#[tokio::test]
async fn session_files_are_private_0600() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fresh.jsonl");
    let mut logger = SessionLogger::at(path.clone()).await.unwrap();
    logger.log_user("something private").await.unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o600,
        "a session log is owner-only; found {mode:o} on {}",
        path.display()
    );

    // And an existing, wider log is tightened when it is reopened, so the
    // fix reaches the sessions already on an operator's disk.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
    let _reopened = SessionLogger::at(path.clone()).await.unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "reopening must tighten an already-wide log");
}
