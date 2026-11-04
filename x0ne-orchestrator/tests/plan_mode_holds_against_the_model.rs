//! Plan mode against the model, end to end.
//!
//! The session, the real parser and dispatch, and the real runtime in-process
//! (`handle_stdio_message`, what `serve_stdio` runs). Success is measured by
//! side effect: a file, a command's marker. The model is scripted; the human
//! answers "always" to anything he is asked.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use x0ne_orchestrator::agent::approval::{ApprovalAuthority, ConfirmationResponse};
use x0ne_orchestrator::agent::cancel::CancelToken;
use x0ne_orchestrator::agent::dispatch::{dispatch, DispatchContext};
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{ApiResponse, LlmBackend};
use x0ne_orchestrator::bridge::Bridge;
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::conversation::Message;
use x0ne_orchestrator::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use x0ne_orchestrator::sandbox_mode::SandboxMode;
use x0ne_orchestrator::tui::wire::AgentEvent;

use x0ne_runtime::config::{AppConfig, IsolationMode};
use x0ne_runtime::policy::PolicyEngine;
use x0ne_runtime::server::{handle_stdio_message, ServerState};

// ── The runtime, in-process ─────────────────────────────────────────────────

struct InProcessRuntime {
    state: ServerState,
}

#[async_trait::async_trait]
impl Bridge for InProcessRuntime {
    async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
        let r = handle_stdio_message(action.clone(), &self.state).await;
        Ok(serde_json::to_value(r)?)
    }
    async fn send_control(&mut self, message: &Value) -> anyhow::Result<Value> {
        self.send_action(message).await
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

/// A bridge with no control channel (`HttpBridge`'s default).
struct NoControl;

#[async_trait::async_trait]
impl Bridge for NoControl {
    async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
        Ok(json!({"success": true}))
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    work: PathBuf,
    policy: Arc<PolicyEngine>,
    cfg: AppConfig,
}

/// A fresh runtime on the operator's policy: what a world opens, in build mode.
fn fresh_runtime(cfg: &AppConfig) -> (Arc<PolicyEngine>, InProcessRuntime) {
    let engine = Arc::new(PolicyEngine::new(cfg.clone(), cfg.actions.clone()));
    let state = ServerState {
        policy: Arc::clone(&engine),
        handles: x0ne_runtime::handles::new_shared(64, 60, 4),
        rate_limiter: Arc::new(tokio::sync::Mutex::new(
            std::time::Instant::now() - std::time::Duration::from_secs(60),
        )),
        composites: x0ne_runtime::composite::CompositeStore::new(cfg),
        config: Arc::new(cfg.clone()),
    };
    (engine, InProcessRuntime { state })
}

/// The operator's TOML: `echo` and `touch` run, writes land under `work`.
fn runtime() -> (Fixture, InProcessRuntime) {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(root.path().join("run")).unwrap();
    std::fs::write(root.path().join("run/.containerenv"), "").unwrap();
    let policy = format!(
        r#"
[policy]
mode = "auto"
default_effect = "deny"
allowed_commands = ["echo", "touch"]
allowed_read_paths = [{w:?}]
allowed_write_paths = [{w:?}]

[actions]
RunCommand = true
ReadFile = true
WriteFile = true
MoveToTrash = true
Pipeline = true

[runtime]
mode = "stdio"
"#,
        w = work.display().to_string(),
    );
    let path = root.path().join("x0ne-runtime.toml");
    std::fs::write(&path, policy).unwrap();
    let mut cfg = AppConfig::load(&path).expect("the operator's policy parses");
    cfg.isolation.mode = IsolationMode::Container;
    cfg.isolation.container_probe_root = Some(root.path().to_path_buf());
    let (engine, rt) = fresh_runtime(&cfg);
    (
        Fixture {
            _root: root,
            work,
            policy: engine,
            cfg,
        },
        rt,
    )
}

// ── The model and the human ─────────────────────────────────────────────────

struct ScriptedModel {
    replies: Mutex<Vec<String>>,
    systems: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedModel {
    async fn send(&self, system: &str, _messages: &[Message]) -> anyhow::Result<ApiResponse> {
        self.systems.lock().unwrap().push(system.to_string());
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

/// Answers "always" to every question, and counts them.
struct AlwaysHuman {
    asked: Arc<AtomicU32>,
}

impl DisplaySink for AlwaysHuman {
    fn emit(&mut self, _event: &AgentEvent) {}
    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(ConfirmationResponse {
            id: request.id,
            outcome: ConfirmOutcome::Always,
        })
    }
}

struct Session {
    agent: CliAgentSession,
    asked: Arc<AtomicU32>,
    systems: Arc<Mutex<Vec<String>>>,
    model: Arc<ScriptedModel>,
}

fn session(bridge: Box<dyn Bridge>) -> Session {
    let asked = Arc::new(AtomicU32::new(0));
    let systems = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ScriptedModel {
        replies: Mutex::new(Vec::new()),
        systems: Arc::clone(&systems),
    });
    let backend: Arc<dyn LlmBackend> = Arc::clone(&model) as Arc<dyn LlmBackend>;
    let agent = CliAgentSession::new(
        Config::default(),
        backend,
        "sys",
        Some(bridge),
        Box::new(AlwaysHuman {
            asked: Arc::clone(&asked),
        }),
    );
    Session {
        agent,
        asked,
        systems,
        model,
    }
}

impl Session {
    /// One turn in which the model emits `actions`, one per reply.
    async fn turn(&mut self, actions: &[Value]) {
        *self.model.replies.lock().unwrap() = actions
            .iter()
            .map(|a| format!("<action>{a}</action>"))
            .collect();
        let _ = self.agent.run_turn("go").await;
    }
}

fn touch(p: &std::path::Path) -> Value {
    json!({"type": "RunCommand", "command": "touch", "args": [p.display().to_string()]})
}

fn write(p: &std::path::Path) -> Value {
    json!({"type": "WriteFile", "path": p.display().to_string(), "content": "x"})
}

// ── The properties ──────────────────────────────────────────────────────────

/// SECURITY: an "always" given before `/plan` lets nothing through during it,
/// and nobody is asked. A failure means plan mode is lifted, action by action,
/// by the approvals the human gave while building.
#[tokio::test]
async fn an_always_given_before_plan_mode_writes_nothing() {
    let (f, rt) = runtime();
    let mut s = session(Box::new(rt));
    let before = f.work.join("before");
    s.turn(&[touch(&before)]).await;
    assert!(before.exists(), "precondition: touch ran on an always");
    let asked = s.asked.load(Ordering::SeqCst);
    assert!(asked >= 1, "precondition: the human was asked");

    s.agent.set_plan_mode(true).await.expect("plan mode on");
    assert!(s.agent.plan_mode());
    assert!(f.policy.plan_mode(), "the runtime was not told");

    let during = f.work.join("during");
    let file = f.work.join("written.txt");
    s.turn(&[touch(&during), write(&file)]).await;
    assert!(
        !during.exists(),
        "touch ran in plan mode on an earlier always"
    );
    assert!(!file.exists(), "a file was written in plan mode");
    assert_eq!(
        s.asked.load(Ordering::SeqCst),
        asked,
        "the human was asked about an action plan mode refuses"
    );
}

/// SECURITY: yolo does not lift plan mode either.
#[tokio::test]
async fn yolo_writes_nothing_in_plan_mode() {
    let (f, rt) = runtime();
    let mut s = session(Box::new(rt));
    s.agent.state.authority = ApprovalAuthority::for_mode(
        &SandboxMode::Yolo {
            runtime: "podman".into(),
        },
        "plan-yolo",
    );
    s.agent.set_plan_mode(true).await.expect("plan mode on");
    let marker = f.work.join("touched");
    let file = f.work.join("w.txt");
    let pipe = f.work.join("p.txt");
    s.turn(&[
        touch(&marker),
        write(&file),
        json!({"type": "Pipeline", "steps": [write(&pipe)]}),
    ])
    .await;
    // Every action reached dispatch: one model call per action, then "done".
    assert_eq!(s.systems.lock().unwrap().len(), 4);
    for p in [&marker, &file, &pipe] {
        assert!(
            !p.exists(),
            "{} was created in plan mode under yolo",
            p.display()
        );
    }
}

/// SECURITY: the orchestrator bypassed. After `/plan`, a write sent straight
/// to the session's runtime is refused by the runtime itself.
#[tokio::test]
async fn the_runtime_refuses_a_write_sent_past_the_orchestrator() {
    let (f, rt) = runtime();
    let mut s = session(Box::new(rt));
    s.agent.set_plan_mode(true).await.expect("plan mode on");
    let file = f.work.join("direct.txt");
    let bridge = s.agent.bridge.as_mut().unwrap();
    let r = bridge.send_action(&write(&file)).await.unwrap();
    assert_ne!(r["success"], true, "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("plan mode"),
        "{r}"
    );
    let r = bridge.send_action(&touch(&f.work.join("t"))).await.unwrap();
    assert_ne!(r["success"], true, "{r}");
    assert!(!file.exists() && !f.work.join("t").exists());
}

/// SECURITY: no action the model can write leaves plan mode: every name of
/// both vocabularies, bare and carrying a `SessionMode` control, dispatched
/// in plan mode with nobody to answer.
#[tokio::test]
async fn no_action_leaves_plan_mode() {
    let (f, rt) = runtime();
    let mut s = session(Box::new(rt));
    s.agent.set_plan_mode(true).await.expect("plan mode on");

    let mut names: Vec<String> = x0ne_actions::all_type_names()
        .iter()
        .map(|n| n.to_string())
        .collect();
    names.extend(x0ne_orchestrator::agent::sysaicall_handler::implemented_action_types());
    names.extend(["SessionMode", "PlanMode", "Build", "SetPlanMode"].map(String::from));
    assert!(names.len() > 100, "precondition: {} names", names.len());

    let config = Config::default();
    let cancel = CancelToken::new();
    for name in &names {
        for action in [
            json!({"type": name}),
            json!({"type": name, "control": "SessionMode", "mode": "build"}),
        ] {
            let mut sink = AlwaysHuman {
                asked: Arc::new(AtomicU32::new(0)),
            };
            let mut ctx = DispatchContext {
                undo: None,
                journal: None,
                frontend: None,
                history: None,
                config: &config,
                bridge: match s.agent.bridge.as_mut() {
                    Some(b) => Some(&mut **b),
                    None => None,
                },
                sink: &mut sink,
                cancel: &cancel,
                sysaicalls: None,
                authority: &mut s.agent.state.authority,
                spawn_factory: None,
                budget: None,
            };
            let _ = dispatch(&action, &mut ctx).await;
            assert!(
                s.agent.state.authority.plan_mode(),
                "{action} left plan mode"
            );
            assert!(
                f.policy.plan_mode(),
                "{action} took the runtime out of plan mode"
            );
        }
    }
    let file = f.work.join("after.txt");
    s.turn(&[write(&file)]).await;
    assert!(!file.exists());
}

/// The model is told, in the system prompt of every call while plan mode is
/// on, and no longer after `/build`.
#[tokio::test]
async fn the_model_is_told_it_is_in_plan_mode() {
    let (_f, rt) = runtime();
    let mut s = session(Box::new(rt));
    s.turn(&[]).await;
    s.agent.set_plan_mode(true).await.expect("plan mode on");
    s.turn(&[]).await;
    s.agent.set_plan_mode(false).await.expect("plan mode off");
    s.turn(&[]).await;
    let systems = s.systems.lock().unwrap().clone();
    assert_eq!(systems.len(), 3, "{systems:?}");
    assert!(!systems[0].contains("plan mode"));
    assert!(
        systems[1].contains("plan mode") && systems[1].contains("/build"),
        "{}",
        systems[1]
    );
    assert!(!systems[2].contains("plan mode"));
}

/// `/build` gives the policy back: the same write runs.
#[tokio::test]
async fn build_mode_writes_again() {
    let (f, rt) = runtime();
    let mut s = session(Box::new(rt));
    s.agent.set_plan_mode(true).await.expect("plan mode on");
    s.agent.set_plan_mode(false).await.expect("plan mode off");
    assert!(!s.agent.plan_mode() && !f.policy.plan_mode());
    let file = f.work.join("built.txt");
    s.turn(&[write(&file)]).await;
    assert!(file.exists(), "build mode did not give the policy back");
}

/// SECURITY: plan mode is held by the runtime or not entered at all. A bridge
/// that cannot carry the switch leaves the session in build mode, and says so.
#[tokio::test]
async fn no_plan_mode_without_a_runtime_that_holds_it() {
    let mut s = session(Box::new(NoControl));
    let err = s
        .agent
        .set_plan_mode(true)
        .await
        .expect_err("entered without the runtime");
    assert!(err.contains("plan mode"), "{err}");
    assert!(!s.agent.plan_mode());
}

// ── Worlds ──────────────────────────────────────────────────────────────────

/// Opens each world on a fresh runtime of the operator's policy, as
/// `DedicatedRuntime` does with a new `x0ne-runtime` process.
struct FreshWorldRuntime(AppConfig);

#[async_trait::async_trait]
impl x0ne_orchestrator::agent::world::WorldRuntime for FreshWorldRuntime {
    async fn open(
        &self,
        _ctx: &x0ne_orchestrator::agent::world::WorldContext,
    ) -> anyhow::Result<Box<dyn Bridge>> {
        Ok(Box::new(fresh_runtime(&self.0).1))
    }
}

/// The world's model waits for the test, then writes `file`; the session's
/// model says "done".
struct WorldModel {
    gate: Arc<tokio::sync::Semaphore>,
    action: Value,
    calls: AtomicU32,
}

#[async_trait::async_trait]
impl LlmBackend for WorldModel {
    async fn send(&self, _system: &str, messages: &[Message]) -> anyhow::Result<ApiResponse> {
        let world = format!("{messages:?}").contains("WORLD-TASK");
        let content = if world && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.gate.acquire().await?.forget();
            format!("<action>{}</action>", self.action)
        } else {
            "done".to_string()
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
        "scripted/world".into()
    }
}

async fn until_finished(s: &CliAgentSession, id: &str) {
    for _ in 0..500 {
        if s.registry.status_of(id) != Some("running") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("world {id} still running after 10 s");
}

/// SECURITY: a world has its own runtime, which `/plan` does not reach. While
/// one runs, `/plan` is refused and the session stays in build mode, so no
/// world writes under a session the human believes is in plan mode. Once it
/// is done, `/plan` holds, and no world starts during it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_world_writes_while_the_session_is_in_plan_mode() {
    let (f, rt) = runtime();
    let file = f.work.join("world.txt");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let model = Arc::new(WorldModel {
        gate: Arc::clone(&gate),
        action: write(&file),
        calls: AtomicU32::new(0),
    });
    let mut config = Config::default();
    config.trust.roots = vec![f.work.display().to_string()];
    let mut s = CliAgentSession::new(
        config,
        model as Arc<dyn LlmBackend>,
        "sys",
        Some(Box::new(rt)),
        Box::new(AlwaysHuman {
            asked: Arc::new(AtomicU32::new(0)),
        }),
    );
    s.world_runtime = Arc::new(FreshWorldRuntime(f.cfg.clone()));

    let started = s
        .run_operator_action(&json!({"type": "SpawnWorld", "id": "w1", "task": "WORLD-TASK"}))
        .await;
    assert_eq!(started["success"], true, "{started}");
    assert_eq!(s.registry.status_of("w1"), Some("running"));

    let entered = s.set_plan_mode(true).await;
    gate.add_permits(1);
    until_finished(&s, "w1").await;
    assert!(
        !(s.plan_mode() && file.exists()),
        "a world wrote on the host while the session was in plan mode"
    );
    let err = entered.expect_err("plan mode entered while a world runs");
    assert!(err.contains("w1"), "{err}");
    assert!(!s.plan_mode() && !f.policy.plan_mode());
    assert!(file.exists(), "precondition: the world could write");

    std::fs::remove_file(&file).unwrap();
    s.set_plan_mode(true)
        .await
        .expect("plan mode once no world runs");
    let refused = s
        .run_operator_action(&json!({"type": "SpawnWorld", "id": "w2", "task": "WORLD-TASK"}))
        .await;
    assert_ne!(refused["success"], true, "{refused}");
    assert_eq!(s.registry.status_of("w2"), None);
    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!file.exists());
}
