//! `x0ne --acp`: the agent side of the Agent Client Protocol (v1) on stdio.
//!
//! One `CliAgentSession` per ACP session. A turn runs in `tokio::spawn`, never
//! in the ACP dispatch loop: a permission request blocks the turn's thread
//! (`DisplaySink` is sync) while the loop must stay free to carry the answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionNotification, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, Mutex};

use crate::agent::approval::ConfirmationResponse;
use crate::agent::cancel::CancelToken;
use crate::agent::driver::TurnOutcome;
use crate::agent::session::CliAgentSession;
use crate::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use crate::tui::wire::AgentEvent;

/// Builds one session for a client's `session/new`. The sink is ACP's.
pub trait SessionFactory: Send + Sync {
    fn create(
        &self,
        cwd: PathBuf,
        sink: Box<dyn DisplaySink>,
    ) -> BoxFuture<'_, anyhow::Result<CliAgentSession>>;
}

struct Slot {
    /// `None` while a turn holds it.
    session: Option<Box<CliAgentSession>>,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<Out>,
}

type Slots = Arc<Mutex<HashMap<String, Slot>>>;

/// What the sink hands to the session's forwarder.
enum Out {
    Update(SessionUpdate),
    Ask {
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    /// Answered once everything queued before it has been sent.
    Flush(tokio::sync::oneshot::Sender<()>),
}

const ALLOW_ONCE: &str = "allow_once";
const ALLOW_ALWAYS: &str = "allow_always";
const REJECT_ONCE: &str = "reject_once";

/// Serve ACP on `transport` until the client hangs up.
pub async fn serve(
    transport: impl ConnectTo<Agent> + 'static,
    factory: Arc<dyn SessionFactory>,
) -> agent_client_protocol::Result<()> {
    let slots: Slots = Arc::default();
    let next_id = Arc::new(AtomicU64::new(1));
    let (s_new, s_prompt, s_cancel) = (slots.clone(), slots.clone(), slots);
    Agent
        .builder()
        .name("x0ne")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                let _ = req;
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new("x0ne", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                if !req.mcp_servers.is_empty() {
                    tracing::warn!("acp: client MCP servers ignored; use [connectors]");
                }
                let id = format!("x0ne-{}", next_id.fetch_add(1, Ordering::Relaxed));
                let (tx, rx) = mpsc::unbounded_channel();
                let sink = Box::new(AcpSink {
                    tx: tx.clone(),
                    calls: 0,
                    current: None,
                });
                let session = match factory.create(req.cwd.clone(), sink).await {
                    Ok(s) => s,
                    Err(e) => {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::internal_error().data(format!("{e:#}")),
                        )
                    }
                };
                let cancel = session.cancel.clone();
                s_new.lock().await.insert(
                    id.clone(),
                    Slot {
                        session: Some(Box::new(session)),
                        cancel,
                        tx,
                    },
                );
                cx.spawn(forward(cx.clone(), id.clone(), rx))?;
                responder.respond(NewSessionResponse::new(id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, _cx| {
                let id = req.session_id.to_string();
                let taken = {
                    let mut slots = s_prompt.lock().await;
                    match slots.get_mut(&id) {
                        None => Err("unknown session"),
                        Some(slot) => match slot.session.take() {
                            Some(s) => Ok((s, slot.tx.clone())),
                            None => Err("a prompt is already running"),
                        },
                    }
                };
                let (mut session, tx) = match taken {
                    Ok(s) => s,
                    Err(why) => {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data(why),
                        )
                    }
                };
                let text = prompt_text(&req.prompt);
                let slots = s_prompt.clone();
                // Off the dispatch loop: see the module docs.
                tokio::spawn(async move {
                    let report = session.run_turn(&text).await;
                    // A cancel during a permission wait ends as a denial
                    // inside the driver; ACP still requires `cancelled`.
                    let cancelled = session.cancel.is_cancelled();
                    let (stop, closing) = if cancelled {
                        (StopReason::Cancelled, None)
                    } else {
                        (stop_reason(&report.outcome), closing_text(&report.outcome))
                    };
                    if cancelled {
                        session.cancel = CancelToken::new();
                    }
                    let fresh = session.cancel.clone();
                    if let Some(text) = closing {
                        let _ = tx.send(Out::Update(SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(text))),
                        )));
                    }
                    let (done, flushed) = tokio::sync::oneshot::channel();
                    if tx.send(Out::Flush(done)).is_ok() {
                        let _ = flushed.await;
                    }
                    if let Some(slot) = slots.lock().await.get_mut(&id) {
                        slot.cancel = fresh;
                        slot.session = Some(session);
                    }
                    let _ = responder.respond(PromptResponse::new(stop));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                if let Some(slot) = s_cancel.lock().await.get(&n.session_id.to_string()) {
                    slot.cancel.cancel();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

/// [`serve`] on this process's stdin and stdout.
pub async fn serve_stdio(factory: Arc<dyn SessionFactory>) -> anyhow::Result<()> {
    serve(agent_client_protocol::Stdio::new(), factory)
        .await
        .map_err(|e| anyhow::anyhow!("ACP connection failed: {e}"))
}

/// Sends the sink's traffic to the client, outside the dispatch loop.
async fn forward(
    cx: ConnectionTo<Client>,
    id: String,
    mut rx: mpsc::UnboundedReceiver<Out>,
) -> agent_client_protocol::Result<()> {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Update(u) => cx.send_notification(SessionNotification::new(id.clone(), u))?,
            Out::Ask {
                tool_call,
                options,
                reply,
            } => {
                let answer = cx
                    .send_request(RequestPermissionRequest::new(
                        id.clone(),
                        tool_call,
                        options,
                    ))
                    .block_task()
                    .await;
                let chosen = match answer.map(|r| r.outcome) {
                    Ok(RequestPermissionOutcome::Selected(s)) => Some(s.option_id.to_string()),
                    // Cancelled, a transport error, or an outcome this
                    // version does not know: nobody said yes.
                    _ => None,
                };
                let _ = reply.send(chosen);
            }
            Out::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
    Ok(())
}

struct AcpSink {
    tx: mpsc::UnboundedSender<Out>,
    calls: u64,
    current: Option<String>,
}

impl AcpSink {
    fn send(&self, u: SessionUpdate) {
        let _ = self.tx.send(Out::Update(u));
    }
}

impl DisplaySink for AcpSink {
    fn emit(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStart { name, action, .. } => {
                self.calls += 1;
                let id = format!("call-{}", self.calls);
                self.current = Some(id.clone());
                self.send(SessionUpdate::ToolCall(
                    ToolCall::new(id, name.clone())
                        .status(ToolCallStatus::InProgress)
                        .raw_input(action.clone()),
                ));
            }
            AgentEvent::ToolEnd { ok, result, .. } => {
                if let Some(id) = self.current.take() {
                    let status = if *ok {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    };
                    self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .raw_output(result.clone()),
                    )));
                }
            }
            AgentEvent::ActionDenied { reason, .. } => {
                if let Some(id) = self.current.take() {
                    self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id,
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Failed)
                            .raw_output(serde_json::json!({ "denied": reason })),
                    )));
                }
            }
            _ => {}
        }
    }

    fn request_confirmation(
        &mut self,
        r: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        let id = self.current.clone().unwrap_or_else(|| {
            self.calls += 1;
            format!("call-{}", self.calls)
        });
        let mut options = vec![PermissionOption::new(
            ALLOW_ONCE,
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )];
        if let Some(label) = r.persistent_label() {
            options.push(PermissionOption::new(
                ALLOW_ALWAYS,
                format!("Always allow: {label}"),
                PermissionOptionKind::AllowAlways,
            ));
        }
        options.push(PermissionOption::new(
            REJECT_ONCE,
            "Reject",
            PermissionOptionKind::RejectOnce,
        ));
        let tool_call = ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new()
                .title(format!("{}: {}", r.action_type, r.reason))
                .raw_input(r.action.clone()),
        );
        let (reply, rx) = std::sync::mpsc::channel();
        self.tx
            .send(Out::Ask {
                tool_call,
                options,
                reply,
            })
            .ok()?;
        // A current-thread runtime cannot block here without stalling the
        // forwarder: refuse, as with no human channel.
        if tokio::runtime::Handle::try_current()
            .map(|h| h.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread)
            .unwrap_or(true)
        {
            return None;
        }
        let chosen = tokio::task::block_in_place(|| rx.recv()).ok()??;
        let outcome = match chosen.as_str() {
            ALLOW_ONCE => ConfirmOutcome::Yes,
            ALLOW_ALWAYS if r.persistent_label().is_some() => ConfirmOutcome::Always,
            _ => ConfirmOutcome::No,
        };
        Some(ConfirmationResponse { id: r.id, outcome })
    }
}

fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text(t) => out.push(t.text.clone()),
            ContentBlock::ResourceLink(l) => out.push(format!("[file: {}]", l.uri)),
            _ => {}
        }
    }
    out.join("\n")
}

fn stop_reason(o: &TurnOutcome) -> StopReason {
    match o {
        TurnOutcome::Cancelled => StopReason::Cancelled,
        TurnOutcome::BudgetExhausted { .. } | TurnOutcome::ActionCapExceeded { .. } => {
            StopReason::MaxTurnRequests
        }
        _ => StopReason::EndTurn,
    }
}

/// The text the client shows at the end of the turn.
fn closing_text(o: &TurnOutcome) -> Option<String> {
    match o {
        TurnOutcome::Completed { message } => Some(message.clone()),
        TurnOutcome::Cancelled => None,
        other => Some(format!(
            "Turn stopped ({}){}",
            other.kind(),
            other.detail().map(|d| format!(": {d}")).unwrap_or_default()
        )),
    }
}

/// Process cwd check: the runtime confines to its own workspace.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}
