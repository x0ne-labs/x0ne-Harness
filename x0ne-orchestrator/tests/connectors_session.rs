//! A connector tool called by the model through the canonical session path
//! (`CliAgentSession` -> driver -> `agent::dispatch`), in sandbox yolo.
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

const CANARY: &str = "CANARY-session-91be-DO-NOT-LEAK";

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
async fn the_model_calls_a_connector_through_the_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let secrets = d.join("secrets.toml");
    std::fs::write(&secrets, format!("fake_token = \"{CANARY}\"\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
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

    let section = connectors::prompt_section();
    assert!(section.contains("fake.send_message (asks)"), "{section}");
    assert!(section.contains("fake.read_note —"), "{section}");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let shown = Arc::new(Mutex::new(Vec::new()));
    let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel {
        replies: Mutex::new(vec![
            r#"<action>{"type":"fake.read_note","arguments":{"id":"leak"}}</action>"#.into(),
            r#"<action>{"type":"fake.read_note","arguments":{"id":"inject"}}</action>"#.into(),
            r#"<action>{"type":"fake.send_message","arguments":{"to":"boss","body":"hi"}}</action>"#.into(),
        ]),
        seen: seen.clone(),
    });
    let log = d.join("session.jsonl");
    let journal = SharedJournal::wrap(SessionLogger::at(log.clone()).await.unwrap());
    let mut agent = CliAgentSession::new(
        Config::default(),
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

    let taint = agent.state.authority.injection().map(str::to_string);
    assert!(
        taint
            .as_deref()
            .is_some_and(|t| t.starts_with("fake.read_note")),
        "the injection guard did not see the connector result: {taint:?}"
    );
    let seen = seen.lock().unwrap().join("\n");
    assert!(
        seen.contains("connector:fake/read_note"),
        "the result never reached the model: {seen}"
    );
    assert!(seen.contains("token=[secret]"), "{seen}");
    assert!(!seen.contains(CANARY), "the model saw the token");

    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked.len(),
        1,
        "yolo must still ask for the write tool: {asked:?}"
    );
    assert!(asked[0].starts_with("fake.send_message"), "{asked:?}");
    let calls = std::fs::read_to_string(d.join("calls.txt")).unwrap_or_default();
    assert!(calls.is_empty(), "the message was sent after a no: {calls}");

    let session_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        session_log.contains("fake.read_note"),
        "the session log missed the call"
    );
    assert!(!session_log.contains(CANARY), "token in the session log");
    assert_eq!(
        session_log
            .matches(r#""type":"prompt_injection_suspected""#)
            .count(),
        1,
        "only the injected note is flagged, not the plain one: {session_log}"
    );
    let screen = shown.lock().unwrap().join("\n");
    assert!(!screen.contains(CANARY), "token on screen");
    let clog = std::fs::read_to_string(d.join("connectors.log")).unwrap();
    assert_eq!(clog.lines().count(), 3, "{clog}");
    assert!(!clog.contains(CANARY));
}
