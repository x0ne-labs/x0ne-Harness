//! ACP v1 end to end: a real ACP client (the official crate) drives
//! `acp::serve` over an in-memory channel. The model asks for a destructive
//! action; the editor's permission answer decides whether the runtime sees it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Channel, Client, ConnectionTo};
use futures::future::BoxFuture;
use serde_json::{json, Value};

use x0ne_orchestrator::acp::{self, SessionFactory};
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{ApiResponse, LlmBackend};
use x0ne_orchestrator::bridge::Bridge;
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::conversation::Message;
use x0ne_orchestrator::display_sink::DisplaySink;
use x0ne_orchestrator::session::{SessionLogger, SharedJournal};

const TRASH: &str = r#"<action>{"type":"MoveToTrash","path":"/tmp/acp-victim"}</action>"#;

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

struct CountingRuntime(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Bridge for CountingRuntime {
    async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
        if action["type"] == "MoveToTrash" {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(
            json!({"success": true, "action_type": action["type"], "output": {"kind": "generic", "data": {}}}),
        )
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct Factory {
    trash: Arc<AtomicUsize>,
    log: PathBuf,
    replies: Vec<String>,
}

impl SessionFactory for Factory {
    fn create(
        &self,
        _cwd: PathBuf,
        sink: Box<dyn DisplaySink>,
    ) -> BoxFuture<'_, anyhow::Result<CliAgentSession>> {
        Box::pin(async move {
            let backend: Arc<dyn LlmBackend> = Arc::new(ScriptedModel {
                replies: Mutex::new(self.replies.clone()),
            });
            let mut s = CliAgentSession::new(
                Config::default(),
                backend,
                "sys",
                Some(Box::new(CountingRuntime(self.trash.clone()))),
                sink,
            );
            let journal = SharedJournal::wrap(SessionLogger::at(self.log.clone()).await?);
            s.journal = Some(journal.handle());
            Ok(s)
        })
    }
}

/// What the editor does when asked.
#[derive(Clone, Copy)]
enum Editor {
    Allow,
    Reject,
    CancelWhileAsked,
}

struct Run {
    stop: StopReason,
    updates: Vec<SessionUpdate>,
    asked: usize,
    trash: usize,
    journal: String,
}

async fn run(editor: Editor, replies: Vec<String>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let trash = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(Factory {
        trash: trash.clone(),
        log: dir.path().join("session.jsonl"),
        replies,
    });
    let (agent_end, client_end) = Channel::duplex();
    let server = tokio::spawn(acp::serve(agent_end, factory));

    let updates = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(AtomicUsize::new(0));
    let session_slot: Arc<Mutex<Option<String>>> = Arc::default();
    let (u, a, slot) = (updates.clone(), asked.clone(), session_slot.clone());
    let stop = Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                u.lock().unwrap().push(n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, cx: ConnectionTo<Agent>| {
                a.fetch_add(1, Ordering::SeqCst);
                let pick = |kind: &str| {
                    req.options
                        .iter()
                        .find(|o| o.option_id.to_string() == kind)
                        .map(|o| o.option_id.clone())
                        .expect("option offered")
                };
                let outcome = match editor {
                    Editor::Allow => RequestPermissionOutcome::Selected(
                        SelectedPermissionOutcome::new(pick("allow_once")),
                    ),
                    Editor::Reject => RequestPermissionOutcome::Selected(
                        SelectedPermissionOutcome::new(pick("reject_once")),
                    ),
                    Editor::CancelWhileAsked => {
                        let id = slot.lock().unwrap().clone().unwrap();
                        cx.send_notification(CancelNotification::new(id))?;
                        RequestPermissionOutcome::Cancelled
                    }
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_end, async move |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V1);
            let s = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            *session_slot.lock().unwrap() = Some(s.session_id.to_string());
            let r = cx
                .send_request(PromptRequest::new(
                    s.session_id,
                    vec![ContentBlock::Text(TextContent::new(
                        "clean up /tmp/acp-victim",
                    ))],
                ))
                .block_task()
                .await?;
            Ok(r.stop_reason)
        })
        .await
        .expect("client run");
    server.abort();
    let journal = std::fs::read_to_string(dir.path().join("session.jsonl")).unwrap_or_default();
    let updates = updates.lock().unwrap().clone();
    Run {
        stop,
        updates,
        asked: asked.load(Ordering::SeqCst),
        trash: trash.load(Ordering::SeqCst),
        journal,
    }
}

fn statuses(updates: &[SessionUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::ToolCall(t) => Some(format!("call:{:?}", t.status)),
            SessionUpdate::ToolCallUpdate(t) => Some(format!("update:{:?}", t.fields.status)),
            _ => None,
        })
        .collect()
}

fn agent_text(updates: &[SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk(c) => match &c.content {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_permission_never_reaches_the_runtime_and_is_journaled() {
    let r = run(Editor::Reject, vec![TRASH.into()]).await;
    assert_eq!(r.asked, 1, "the editor was not asked");
    assert_eq!(r.trash, 0, "the runtime trashed the file after a reject");
    assert_eq!(r.stop, StopReason::EndTurn);
    let st = statuses(&r.updates);
    assert!(st.contains(&"update:Some(Failed)".to_string()), "{st:?}");
    assert!(
        r.journal.contains(r#""denial":"human_declined""#),
        "{}",
        r.journal
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowed_permission_runs_the_action_and_the_reply_comes_before_the_response() {
    let r = run(Editor::Allow, vec![TRASH.into(), "trashed it".into()]).await;
    assert_eq!(r.asked, 1);
    assert_eq!(r.trash, 1, "the allowed action did not run");
    assert_eq!(r.stop, StopReason::EndTurn);
    let st = statuses(&r.updates);
    assert_eq!(st, ["call:InProgress", "update:Some(Completed)"], "{st:?}");
    assert!(
        agent_text(&r.updates).contains("trashed it"),
        "{:?}",
        r.updates
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_during_a_permission_wait_ends_cancelled_and_runs_nothing() {
    let r = run(Editor::CancelWhileAsked, vec![TRASH.into()]).await;
    assert_eq!(r.asked, 1);
    assert_eq!(r.trash, 0);
    assert_eq!(r.stop, StopReason::Cancelled);
}
