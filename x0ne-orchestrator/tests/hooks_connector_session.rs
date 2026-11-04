//! Attack: the model calls live connector tools in sandbox yolo, and a user
//! hook matching `fake.*` refuses them. Nothing may reach the MCP server.
//! One test per file: the process hub is global.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use x0ne_orchestrator::agent::approval::{ApprovalAuthority, ConfirmationResponse, OperatorTrust};
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{ApiResponse, LlmBackend};
use x0ne_orchestrator::bridge::Bridge;
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::connectors::{
    self, secrets::SecretStore, ConnectorConfig, ConnectorHub, HubPaths, Transport,
};
use x0ne_orchestrator::conversation::Message;
use x0ne_orchestrator::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use x0ne_orchestrator::session::{SessionLogger, SharedJournal};
use x0ne_orchestrator::tui::wire::AgentEvent;

const CANARY: &str = "CANARY-hooks-5c2e-DO-NOT-LEAK";

fn fake_server() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .and_then(Path::parent)
        .unwrap()
        .join("examples")
        .join("fake_mcp_server")
}

struct ScriptedModel {
    replies: Mutex<Vec<String>>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedModel {
    async fn send(&self, system: &str, messages: &[Message]) -> anyhow::Result<ApiResponse> {
        let mut seen = self.seen.lock().unwrap();
        seen.push(system.to_string());
        seen.push(format!("{messages:?}"));
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

struct IdleRuntime;

#[async_trait::async_trait]
impl Bridge for IdleRuntime {
    async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
        Ok(
            json!({"success": true, "action_type": action["type"], "output": {"kind": "generic", "data": {}}}),
        )
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct Operator {
    asked: Arc<Mutex<Vec<String>>>,
    shown: Arc<Mutex<Vec<String>>>,
}

impl DisplaySink for Operator {
    fn emit(&mut self, event: &AgentEvent) {
        self.shown.lock().unwrap().push(format!("{event:?}"));
    }
    fn request_confirmation(
        &mut self,
        r: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        let line = format!("{} {} {}", r.action_type, r.reason, r.action);
        self.shown.lock().unwrap().push(line.clone());
        self.asked.lock().unwrap().push(line);
        Some(ConfirmationResponse {
            id: r.id,
            outcome: ConfirmOutcome::No,
        })
    }
}

#[tokio::test]
async fn a_hook_refuses_live_connector_calls_before_the_server_sees_them() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let secrets = d.join("secrets.toml");
    std::fs::write(&secrets, format!("fake_token = \"{CANARY}\"\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        "FAKE_MCP_TOKEN".to_string(),
        "${secret:fake_token}".to_string(),
    );
    env.insert(
        "FAKE_MCP_CALLS".to_string(),
        d.join("calls.txt").display().to_string(),
    );
    let mut declared = std::collections::BTreeMap::new();
    declared.insert(
        "fake".to_string(),
        ConnectorConfig {
            enabled: Some(true),
            transport: Some(Transport::Stdio),
            command: Some(fake_server().display().to_string()),
            env: Some(env),
            read: Some(vec!["read_*".into()]),
            ..Default::default()
        },
    );
    let mut hub = ConnectorHub::new(
        &declared,
        SecretStore {
            file: secrets,
            keyring_cmd: "/nonexistent".into(),
        },
        HubPaths {
            pins: d.join("pins.json"),
            state: d.join("state.json"),
            log: d.join("connectors.log"),
        },
    );
    let started = hub.start_enabled().await;
    assert!(connectors::install(hub));
    assert!(started[0].contains("ready"), "{started:?}");

    // The hook logs what it saw on stdin, then refuses.
    let seen_by_hook = d.join("hook-stdin.jsonl");
    let hook = d.join("deny-fake.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >> '{}'\necho >> '{}'\necho 'connector fake is frozen' >&2\nexit 2\n",
            seen_by_hook.display(),
            seen_by_hook.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = Config::default();
    config
        .hooks
        .pre_tool
        .push(x0ne_orchestrator::hooks::PreToolHook {
            matcher: "fake.*".into(),
            command: hook.display().to_string(),
            args: vec![],
            timeout_secs: Some(5),
        });

    let seen = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let shown = Arc::new(Mutex::new(Vec::new()));
    let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel {
        replies: Mutex::new(vec![
            r#"<action>{"type":"fake.read_note","arguments":{"id":"leak"}}</action>"#.into(),
            r#"<action>{"type":"fake.send_message","arguments":{"to":"boss","body":"hi"}}</action>"#.into(),
        ]),
        seen: seen.clone(),
    });
    let log = d.join("session.jsonl");
    let journal = SharedJournal::wrap(SessionLogger::at(log.clone()).await.unwrap());
    let mut agent = CliAgentSession::new(
        config,
        backend,
        "sys",
        Some(Box::new(IdleRuntime)),
        Box::new(Operator {
            asked: asked.clone(),
            shown: shown.clone(),
        }),
    );
    agent.state.authority = ApprovalAuthority::new(OperatorTrust::IsolatedEnvironment, "sandbox");
    agent.journal = Some(journal.handle());
    let _ = agent
        .run_turn("read note leak, then message the boss")
        .await;

    // Nothing reached the hub: no call line, no refusal line, no side effect.
    let clog = std::fs::read_to_string(d.join("connectors.log")).unwrap_or_default();
    assert_eq!(clog.lines().count(), 0, "a call reached the hub: {clog}");
    let calls = std::fs::read_to_string(d.join("calls.txt")).unwrap_or_default();
    assert!(calls.is_empty(), "the server ran a tool: {calls}");
    assert!(
        asked.lock().unwrap().is_empty(),
        "the operator was asked anyway"
    );

    // The hook saw both calls, with the connector split out.
    let stdin = std::fs::read_to_string(&seen_by_hook).unwrap();
    let inputs: Vec<Value> = stdin
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(inputs.len(), 2, "{stdin}");
    assert_eq!(inputs[0]["tool_name"], "fake.read_note");
    assert_eq!(inputs[1]["connector"]["tool"], "send_message");
    assert_eq!(
        inputs[1]["connector"]["arguments"]["to"], "boss",
        "{}",
        inputs[1]
    );

    // The model was told, the journal recorded who refused.
    let seen = seen.lock().unwrap().join("\n");
    assert!(seen.contains("connector fake is frozen"), "{seen}");
    let session_log = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        session_log.matches(r#""denial":"hook""#).count(),
        2,
        "{session_log}"
    );
    assert!(!session_log.contains(CANARY));
}
