//! A refusal that comes from CONFIGURATION is a decision, not a wall.
//!
//! # The field report this file answers
//!
//! Session `2026-08-16_01-11_111b`. The operator types "go dans ./playground".
//! The model emits `RunCommand ls playground`. The runtime refuses:
//!
//! ```text
//! Command 'ls' is not in the allowed_commands list. Add "ls" to [policy]
//! allowed_commands in x0ne-runtime.toml, or run `x0ne --setup`. An empty list
//! denies every command.
//! ```
//!
//! The operator was never asked anything. He wrote: « why don't I have a
//! gate? … can you gate that for me? ».
//!
//! Note the path this took, because it is NOT the preflight one. `ls` is
//! waived by `agent::readonly_cmd`, so `confirm::requires_approval` is false,
//! so `dispatch` never runs P8a on it (`an_action_the_gate_waives_is_not_preflighted`).
//! The action was SENT, the runtime refused it at execution, and the refusal
//! came back as an `error.class == "policy_denied"` envelope. A session gate
//! hung only on preflight would not have fired for the very case that was
//! reported.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use x0ne_orchestrator::agent::approval::ConfirmationResponse;
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{ApiResponse, LlmBackend};
use x0ne_orchestrator::bridge::Bridge;
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::conversation::Message;
use x0ne_orchestrator::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use x0ne_orchestrator::session::{SessionLogger, SharedJournal};
use x0ne_orchestrator::tui::wire::AgentEvent;

// ── Fakes ─────────────────────────────────────────────────────────────────────

struct ScriptedModel {
    replies: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedModel {
    async fn send(&self, _system: &str, _messages: &[Message]) -> anyhow::Result<ApiResponse> {
        let mut q = self.replies.lock().unwrap();
        let content = if q.is_empty() {
            "done".to_string()
        } else {
            q.remove(0)
        };
        Ok(ApiResponse {
            content,
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

/// A runtime whose `allowed_commands` is empty, exactly like the operator's.
///
/// It answers the way the real `x0ne-runtime` binary does since this lot:
/// `RunCommand` is refused with a `policy_denied` envelope carrying the
/// structured `liftable`, and a `PolicyGrant` control line widens the list for
/// the rest of this process. It counts every command it was asked to run, and
/// every grant it received.
struct EmptyAllowlistRuntime {
    ran: Arc<Mutex<Vec<String>>>,
    granted: Arc<Mutex<Vec<String>>>,
    /// Set by the test to model a runtime that refuses the grant, or one whose
    /// bridge has no control channel at all.
    control: Control,
    config_path: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Control {
    Accepts,
    Refuses,
    /// No control channel — an HTTP bridge, or a transport that carries none.
    Absent,
}

#[async_trait::async_trait]
impl Bridge for EmptyAllowlistRuntime {
    async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
        let ty = action["type"].as_str().unwrap_or("");
        if ty == "RunCommand" {
            let cmd = action["command"].as_str().unwrap_or("").to_string();
            self.ran.lock().unwrap().push(cmd.clone());
            if self.granted.lock().unwrap().contains(&cmd) {
                return Ok(json!({
                    "success": true,
                    "action_type": "RunCommand",
                    "output": {"kind": "command_output", "stdout": "playground\n", "exit_code": 0}
                }));
            }
            return Ok(json!({
                "success": false,
                "action_type": "RunCommand",
                "error": {
                    "class": "policy_denied",
                    "code": "policy_denied",
                    "message": format!(
                        "Command '{cmd}' is not in the allowed_commands list. Add \"{cmd}\" \
                         to [policy] allowed_commands in x0ne-runtime.toml, answer 1 or 2 at \
                         the prompt, or run `x0ne --setup`. An empty list denies every command."
                    ),
                    "liftable": {"kind": "command", "value": cmd},
                }
            }));
        }
        Ok(
            json!({ "success": true, "action_type": ty, "output": { "kind": "generic", "data": {} } }),
        )
    }

    async fn send_control(&mut self, message: &Value) -> anyhow::Result<Value> {
        match self.control {
            Control::Absent => anyhow::bail!("this bridge has no control channel"),
            Control::Refuses => Ok(json!({
                "success": false,
                "action_type": "PolicyGrant",
                "error": {"class": "policy_denied", "message": "refused"}
            })),
            Control::Accepts => {
                let value = message["value"].as_str().unwrap_or("").to_string();
                self.granted.lock().unwrap().push(value.clone());
                Ok(json!({
                    "success": true,
                    "action_type": "PolicyGrant",
                    "output": {"kind": "generic", "data": {
                        "granted": true,
                        "value": value,
                        "config_path": self.config_path,
                    }}
                }))
            }
        }
    }

    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

fn action(json_line: &str) -> String {
    format!("<action>{json_line}</action>")
}

// ── Harness ───────────────────────────────────────────────────────────────────

struct Outcome {
    asked: u32,
    headlines: Vec<String>,
    ran: Vec<String>,
    granted: Vec<String>,
    journal: Vec<Value>,
}

/// Run one turn in which the model asks for `ls playground`, against a runtime
/// with an empty `allowed_commands`, and report everything that happened.
async fn run_turn(
    answer: Option<ConfirmOutcome>,
    control: Control,
    config_path: Option<String>,
) -> Outcome {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let granted = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(AtomicU32::new(0));
    let headlines = Arc::new(Mutex::new(Vec::new()));

    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("session.jsonl");
    let journal = SharedJournal::wrap(SessionLogger::at(log.clone()).await.unwrap());

    let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel {
        replies: Mutex::new(vec![action(
            r#"{"type":"RunCommand","command":"ls","args":["playground"]}"#,
        )]),
    });
    let mut agent = CliAgentSession::new(
        Config::default(),
        backend,
        "sys",
        Some(Box::new(EmptyAllowlistRuntime {
            ran: Arc::clone(&ran),
            granted: Arc::clone(&granted),
            control,
            config_path,
        })),
        Box::new(MaybeHuman {
            outcome: answer,
            asked: Arc::clone(&asked),
            headlines: Arc::clone(&headlines),
        }),
    );
    agent.journal = Some(journal.handle());
    let _ = agent.run_turn("go dans ./playground").await;

    let raw = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    let asked = asked.load(Ordering::SeqCst);
    let headlines = headlines.lock().unwrap().clone();
    let ran = ran.lock().unwrap().clone();
    let granted = granted.lock().unwrap().clone();
    let journal = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    Outcome {
        asked,
        headlines,
        ran,
        granted,
        journal,
    }
}

/// A human who may or may not be there. `None` is EOF: the sink relays the
/// question and nobody answers.
struct MaybeHuman {
    outcome: Option<ConfirmOutcome>,
    asked: Arc<AtomicU32>,
    headlines: Arc<Mutex<Vec<String>>>,
}

impl DisplaySink for MaybeHuman {
    fn emit(&mut self, _event: &AgentEvent) {}
    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.headlines
            .lock()
            .unwrap()
            .push(request.reason.to_string());
        self.outcome.map(|outcome| ConfirmationResponse {
            id: request.id,
            outcome,
        })
    }
}

// ── The properties ────────────────────────────────────────────────────────────

/// A refusal the operator can lift by editing one line of his own config is
/// put to him as a question, once, and the action runs when he says yes.
///
/// A failure here means X0NE is back to the 2026-08-16 field report: the
/// operator asks for `ls`, gets a paragraph about TOML, and is never offered
/// the decision that is plainly his to make. It is also the failure mode that
/// makes the NEXT prompt — the one that matters — go unread.
#[tokio::test]
async fn a_liftable_denial_asks_the_operator_once_and_runs_on_yes() {
    let o = run_turn(Some(ConfirmOutcome::Yes), Control::Accepts, None).await;

    assert_eq!(
        o.asked, 1,
        "the operator was asked {} times about a refusal he can lift; headlines: {:?}",
        o.asked, o.headlines
    );
    assert_eq!(
        o.ran,
        vec!["ls".to_string(), "ls".to_string()],
        "after the grant the command must be retried, so the runtime sees `ls` twice: \
         once refused, once run. It saw {:?}",
        o.ran
    );
    assert_eq!(o.granted, vec!["ls".to_string()]);
    let headline = &o.headlines[0];
    assert!(
        headline.contains("ls") && headline.contains("allowed_commands"),
        "the prompt does not say what is being decided: {headline}"
    );
}

/// SECURITY: nobody at the keyboard is a refusal, and the command does not run.
///
/// A failure means a piped stdin or a closed terminal widens the operator's
/// `allowed_commands` on its own — fail-closed broken in the one place where
/// breaking it edits the policy itself.
#[tokio::test]
async fn eof_at_the_session_gate_is_a_denial() {
    let o = run_turn(None, Control::Accepts, None).await;
    assert_eq!(o.asked, 1, "the question was never even put");
    assert!(o.granted.is_empty(), "EOF granted {:?}", o.granted);
    assert_eq!(
        o.ran,
        vec!["ls".to_string()],
        "the command ran again after nobody answered"
    );
}

/// Saying no leaves things exactly as they were before this gate existed.
#[tokio::test]
async fn declining_leaves_the_refusal_standing() {
    let o = run_turn(Some(ConfirmOutcome::No), Control::Accepts, None).await;
    assert_eq!(o.asked, 1);
    assert!(o.granted.is_empty());
    assert_eq!(o.ran, vec!["ls".to_string()]);
}

/// SECURITY: a runtime that refuses the grant is obeyed, and a bridge with no
/// control channel is not worked around.
///
/// A failure means the orchestrator's prompt is the real authority and the
/// runtime's refusal is decorative — the double-gating defect inverted,
/// with the orchestrator overruling the engine instead of the other way round.
#[tokio::test]
async fn a_grant_the_runtime_refuses_is_not_a_permission() {
    for control in [Control::Refuses, Control::Absent] {
        let o = run_turn(Some(ConfirmOutcome::Yes), control, None).await;
        assert_eq!(o.asked, 1);
        assert_eq!(
            o.ran,
            vec!["ls".to_string()],
            "the action was retried although the widening never happened"
        );
    }
}

/// SECURITY: every grant leaves a line in the session log.
///
/// A failure means an auditor reading the journal sees an action refused and
/// then the same action succeeding, with nothing in between — and the missing
/// fact is "a human was asked and said yes", which is the only thing that
/// makes the widening legitimate.
#[tokio::test]
async fn every_grant_is_journaled() {
    let o = run_turn(Some(ConfirmOutcome::Yes), Control::Accepts, None).await;
    let grants: Vec<&Value> = o
        .journal
        .iter()
        .filter(|e| e["type"].as_str() == Some("policy_grant"))
        .collect();
    assert_eq!(
        grants.len(),
        1,
        "the session log has {} policy_grant lines; it holds {:?}",
        grants.len(),
        o.journal
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(grants[0]["value"].as_str(), Some("ls"));
    assert_eq!(grants[0]["persist"].as_bool(), Some(false));
    assert_eq!(grants[0]["action_type"].as_str(), Some("RunCommand"));

    // And a refusal writes no grant line at all.
    let refused = run_turn(Some(ConfirmOutcome::No), Control::Accepts, None).await;
    assert!(
        !refused
            .journal
            .iter()
            .any(|e| e["type"].as_str() == Some("policy_grant")),
        "a refusal was journaled as a grant"
    );
}

/// Answering "add to config" writes the operator's file, and leaves every
/// comment in it.
///
/// A failure means one keystroke strips the explanations from the one X0NE
/// file written to be read by a human — or writes a file the runtime never
/// loaded, which is worse: the operator is told his config was updated and is
/// asked the same question again next session.
#[tokio::test]
async fn answering_two_writes_the_file_the_runtime_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("x0ne-runtime.toml");
    std::fs::write(
        &cfg,
        "[policy]\n# An EMPTY list denies every command.\nallowed_commands = []\n",
    )
    .unwrap();

    let o = run_turn(
        Some(ConfirmOutcome::Always),
        Control::Accepts,
        Some(cfg.to_string_lossy().to_string()),
    )
    .await;

    assert_eq!(o.granted, vec!["ls".to_string()]);
    assert_eq!(o.ran, vec!["ls".to_string(), "ls".to_string()]);

    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        after.contains("\"ls\""),
        "the file was not extended:\n{after}"
    );
    assert!(
        after.contains("# An EMPTY list denies every command."),
        "the operator's comment was erased:\n{after}"
    );

    let grants: Vec<&Value> = o
        .journal
        .iter()
        .filter(|e| e["type"].as_str() == Some("policy_grant"))
        .collect();
    assert_eq!(grants[0]["persist"].as_bool(), Some(true));
    assert!(
        grants[0]["persisted"].as_str().is_some(),
        "the log does not record that the file was written: {}",
        grants[0]
    );
}

/// SECURITY: a refusal with no `liftable` is never offered — including the
/// refusal an older runtime produces, which carries no such field at all.
///
/// A failure means the orchestrator decided for itself that something was
/// waivable, from the wording of a message. The decision belongs to the
/// runtime's exhaustive match on `DenyReason`, and a deny pattern reaching a
/// prompt is the worst outcome this feature could have.
#[tokio::test]
async fn an_unliftable_denial_is_never_offered() {
    struct WallRuntime {
        ran: Arc<Mutex<u32>>,
        envelope: Value,
    }

    #[async_trait::async_trait]
    impl Bridge for WallRuntime {
        async fn send_action(&mut self, act: &Value) -> anyhow::Result<Value> {
            if act["type"].as_str() == Some("ReadFile") {
                *self.ran.lock().unwrap() += 1;
                return Ok(self.envelope.clone());
            }
            Ok(
                json!({"success": true, "action_type": "x", "output": {"kind": "generic", "data": {}}}),
            )
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    // (a) a deny pattern, and (b) an older runtime: no `liftable` either way.
    for envelope in [
        json!({"success": false, "action_type": "ReadFile", "error": {
            "class": "policy_denied",
            "message": "Path '/home/u/.ssh/id_ed25519' matches deny pattern '**/.ssh/**'."
        }}),
        json!({"success": false, "action_type": "ReadFile", "error": {
            "class": "policy_denied",
            "message": "Command 'ls' is not in the allowed_commands list."
        }}),
    ] {
        let asked = Arc::new(AtomicU32::new(0));
        let ran = Arc::new(Mutex::new(0));
        let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel {
            replies: Mutex::new(vec![action(
                r#"{"type":"ReadFile","path":"/home/u/.ssh/id_ed25519"}"#,
            )]),
        });
        let mut agent = CliAgentSession::new(
            Config::default(),
            backend,
            "sys",
            Some(Box::new(WallRuntime {
                ran: Arc::clone(&ran),
                envelope,
            })),
            Box::new(MaybeHuman {
                outcome: Some(ConfirmOutcome::Yes),
                asked: Arc::clone(&asked),
                headlines: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let _ = agent.run_turn("read my key").await;

        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "a refusal carrying no `liftable` was put to the operator as a choice"
        );
        assert_eq!(
            *ran.lock().unwrap(),
            1,
            "the action was retried after an unliftable refusal"
        );
    }
}
