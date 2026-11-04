//! The fullscreen frontend's two canonical adapters.
//!
//! [`TuiDisplaySink`] is the presentation half: every semantic event the
//! driver emits is forwarded onto the TUI's event channel — the same
//! [`AgentEvent`] enum the renderer already consumes, which is why this file
//! contains no rendering. Confirmations block through `gate::HumanGate`
//! (owner-selected Option A; see that module for the threading contract).
//!
//! [`TuiFrontendActions`] is the capability half: the nine frontend-claimed
//! actions (`tui::intercept`) — badge painting, the AskUser modal, the task
//! panel, cron-light — executed here under the canonical dispatcher's gates
//! instead of a private `if action_type == …` ladder. The typed table in
//! `intercept.rs` remains the single place their side-effect semantics are
//! declared; `claims` merely reads it.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::approval::ConfirmationResponse;
use crate::agent::dispatch::FrontendActions;
use crate::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use crate::tui::badge;
use crate::tui::gate::{AskGate, HumanGate};
use crate::tui::intercept::Intercepted;
use crate::tui::wire::{AgentEvent, AskOption};

/// The armed badge watcher, shared between the frontend executor (arms it),
/// the command router (`/badge watch` and clear), and the sink (retracts it on
/// a policy verdict).
pub struct BadgeWatchSlot {
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Generation counter for standing authorizations. Bumping it stops an
    /// armed watcher at its next tick — the watcher captured the value it was
    /// armed under and compares before every run.
    pub policy_epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for BadgeWatchSlot {
    fn default() -> Self {
        Self {
            handle: std::sync::Mutex::new(None),
            policy_epoch: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
}

impl BadgeWatchSlot {
    /// Install a new watcher, aborting any previous one.
    pub fn install(&self, handle: tokio::task::JoinHandle<()>) {
        if let Some(old) = self.handle.lock().unwrap().replace(handle) {
            old.abort();
        }
    }

    /// The current epoch, for arming a watcher against.
    pub fn current_epoch(&self) -> u64 {
        self.policy_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_armed(&self) -> bool {
        self.handle.lock().unwrap().is_some()
    }

    /// Retract every standing authorization: advance the epoch (stops a
    /// watcher mid-flight at its next tick) and abort the task.
    pub fn invalidate(&self, reason: &str) {
        self.policy_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
            tracing::info!(reason, "retracted armed badge watch");
        }
    }

    /// Inspect a result envelope for evidence the effective policy is
    /// stricter than when a watcher was armed. A `policy_denied` means the
    /// engine is refusing something now; a command authorized earlier cannot
    /// be assumed still authorized.
    pub fn note_policy_verdict(&self, result: &Value, evt_tx: &mpsc::UnboundedSender<AgentEvent>) {
        if !self.is_armed() {
            return;
        }
        let class = result
            .get("error")
            .and_then(|e| e.get("class").or_else(|| e.get("kind")))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if class != "policy_denied" {
            return;
        }
        self.invalidate("runtime returned a policy denial");
        let _ = evt_tx.send(AgentEvent::Info(
            "⏹  badge watch stopped · the policy engine refused an action, so the \
             standing authorization for the watched command no longer applies"
                .into(),
        ));
    }
}

/// The fullscreen `DisplaySink`.
///
/// `emit` forwards; `request_confirmation` blocks on the human's answer via
/// the gate.
pub struct TuiDisplaySink {
    pub evt_tx: mpsc::UnboundedSender<AgentEvent>,
    pub confirm_gate: Arc<HumanGate>,
    pub badge: Arc<BadgeWatchSlot>,
}

impl DisplaySink for TuiDisplaySink {
    fn emit(&mut self, event: &AgentEvent) {
        if let AgentEvent::ToolEnd { result, .. } = event {
            // Standing-authorization hygiene rode on the old dispatch tail;
            // it rides on the event stream now, which sees the same results.
            self.badge.note_policy_verdict(result, &self.evt_tx);
            // A Pipeline can carry PaintBadge steps whose stub results come
            // back embedded in the composite envelope; repaint from them.
            forward_embedded_badge_paints(result, &self.evt_tx);
        }
        let _ = self.evt_tx.send(event.clone());
    }

    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        let wire_id = self.confirm_gate.next_wire_id("confirm");
        let rx = self.confirm_gate.open(wire_id.clone());
        let sent = self.evt_tx.send(AgentEvent::ConfirmationRequested {
            id: wire_id,
            action: request.action.clone(),
            action_type: request.action_type.to_string(),
            reason: request.reason.to_string(),
            // Covers both kinds of third option: a standing approval for an
            // ordinary action, and "add to config" for a policy question.
            persistent_grant: request.persistent_label(),
        });
        if sent.is_err() {
            // No UI to ask. Fail closed rather than park forever.
            self.confirm_gate.abandon();
            return None;
        }
        let answer = self.confirm_gate.wait(rx)?;
        let outcome = match (answer.approved, answer.always) {
            (false, _) => ConfirmOutcome::No,
            (true, false) => ConfirmOutcome::Yes,
            // `always` is honoured only when the authority offered it — the
            // same rule the CLI prompt applies. An answer claiming a standing
            // scope nobody was shown degrades to allow-once.
            (true, true) if request.persistent_label().is_some() => ConfirmOutcome::Always,
            (true, true) => ConfirmOutcome::Yes,
        };
        Some(ConfirmationResponse {
            id: request.id,
            outcome,
        })
    }
}

/// A deferred cron entry.
///
/// One shape difference from the historical runner: every entry carries a
/// `CapabilityLease`. A recurring cron used to re-arm unconditionally —
/// permanent autonomous repetition created as a side effect of scheduling.
/// Now the lease bounds it: a validity window and a firing budget, both
/// clamped to policy ceilings, revoked by `CronCancel`, and reported when
/// they run out. The lease only earns the entry its trip to the canonical
/// boundary; the confirmation gate still runs at fire time.
#[derive(Debug, Clone)]
pub struct PendingCron {
    pub id: u32,
    pub label: String,
    pub due_at_unix: u64,
    /// `Some(prompt)` → fire as a synthetic user input; `None` → passive
    /// reminder line.
    pub trigger_prompt: Option<String>,
    /// `Some(action)` → when due, run this action through the canonical
    /// single-action path. Takes priority over `trigger_prompt`.
    pub action: Option<Value>,
    /// `Some(secs)` → recurring; `None` → one-shot.
    pub every_secs: Option<u64>,
    /// The bounded authority for this entry's future firings.
    pub lease: crate::agent::lease::CapabilityLease,
}

/// The nine frontend-claimed actions, executed under the canonical gates.
pub struct TuiFrontendActions {
    pub evt_tx: mpsc::UnboundedSender<AgentEvent>,
    pub ask_gate: Arc<AskGate>,
    pub badge: Arc<BadgeWatchSlot>,
    pub crons: Arc<std::sync::Mutex<Vec<PendingCron>>>,
    next_task_id: u32,
    next_cron_id: u32,
}

impl TuiFrontendActions {
    pub fn new(
        evt_tx: mpsc::UnboundedSender<AgentEvent>,
        ask_gate: Arc<AskGate>,
        badge: Arc<BadgeWatchSlot>,
        crons: Arc<std::sync::Mutex<Vec<PendingCron>>>,
    ) -> Self {
        Self {
            evt_tx,
            ask_gate,
            badge,
            crons,
            next_task_id: 1,
            next_cron_id: 1,
        }
    }

    fn ok(output: Value) -> Value {
        serde_json::json!({"success": true, "output": output})
    }

    fn invalid(message: &str) -> Value {
        serde_json::json!({
            "success": false,
            "error": {"message": message, "kind": "invalid_input"},
        })
    }

    fn paint_badge(&self, action: &Value) -> Value {
        let frame_json = serde_json::to_string(action).unwrap_or_default();
        let _ = self.evt_tx.send(AgentEvent::BadgeUpdate { frame_json });
        Self::ok(serde_json::json!({"painted": true}))
    }

    fn ask_user(&self, action: &Value) -> Value {
        let question = action
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let multi_select = action
            .get("multi_select")
            .or_else(|| action.get("multiSelect"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hint = action
            .get("hint")
            .and_then(|v| v.as_str())
            .map(String::from);
        let options: Vec<AskOption> = action
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        if let Some(s) = o.as_str() {
                            return Some(AskOption {
                                label: s.to_string(),
                                description: None,
                                recommended: false,
                            });
                        }
                        let label = o.get("label").and_then(|v| v.as_str())?.to_string();
                        Some(AskOption {
                            label,
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            recommended: o
                                .get("recommended")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if question.is_empty() || options.is_empty() {
            return Self::invalid(
                "AskUser requires a non-empty `question` and at least one entry in `options`",
            );
        }

        let wire_id = self.ask_gate.next_wire_id("ask");
        let rx = self.ask_gate.open(wire_id.clone());
        if self
            .evt_tx
            .send(AgentEvent::AskUser {
                id: wire_id,
                question,
                options,
                multi_select,
                hint,
            })
            .is_err()
        {
            self.ask_gate.abandon();
            return serde_json::json!({
                "success": false,
                "error": {"message": "no frontend to ask", "kind": "cancelled"},
            });
        }
        // Same blocking contract as a confirmation; same current-thread guard.
        match self.ask_gate.wait(rx) {
            Some(answer) if !answer.cancelled => Self::ok(serde_json::json!({
                "selected": answer.selected,
                "custom_text": answer.custom_text,
            })),
            _ => serde_json::json!({
                "success": false,
                "error": {"message": "user cancelled the question", "kind": "cancelled"},
            }),
        }
    }

    fn badge_watch(&self, action: &Value) -> Value {
        let cmd = action
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if cmd.trim().is_empty() {
            return Self::invalid("BadgeWatch requires a non-empty `cmd`");
        }
        let interval = action
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(5);
        let label = action
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("watch")
            .to_string();
        // The human said yes upstream (ProcessSpawn-classed, so the canonical
        // gate ran before this call). Bind the watcher to the current epoch
        // so retraction still works exactly as before.
        let armed_at = self.badge.current_epoch();
        self.badge.install(spawn_badge_watch(
            cmd,
            interval,
            label.clone(),
            self.evt_tx.clone(),
            Arc::clone(&self.badge.policy_epoch),
            armed_at,
        ));
        let _ = self
            .evt_tx
            .send(AgentEvent::Info("badge watch started".into()));
        Self::ok(serde_json::json!({"armed": true, "label": label}))
    }

    fn task_add(&mut self, action: &Value) -> Value {
        let title = action
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            return Self::invalid("TaskAdd requires a non-empty `title`");
        }
        let id = self.next_task_id;
        self.next_task_id += 1;
        let _ = self.evt_tx.send(AgentEvent::TaskAdded {
            id,
            title: title.clone(),
        });
        Self::ok(serde_json::json!({"task_id": id, "title": title}))
    }

    fn task_done(&self, action: &Value) -> Value {
        let Some(id) = action.get("id").and_then(|v| v.as_u64()).map(|n| n as u32) else {
            return Self::invalid("TaskDone requires a numeric `id` from a prior TaskAdd");
        };
        let _ = self.evt_tx.send(AgentEvent::TaskCompleted { id });
        Self::ok(serde_json::json!({"task_id": id, "done": true}))
    }

    fn cron_schedule(&mut self, action: &Value) -> Value {
        let label = action
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if label.is_empty() {
            return Self::invalid("CronSchedule requires a non-empty `label`");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let every_secs = action.get("every_secs").and_then(|v| v.as_u64());
        let due_at_unix = if let Some(every) = every_secs {
            now.saturating_add(every.max(1))
        } else if let Some(in_secs) = action.get("in_secs").and_then(|v| v.as_u64()) {
            now.saturating_add(in_secs)
        } else if let Some(at) = action.get("at_unix").and_then(|v| v.as_u64()) {
            at
        } else {
            return Self::invalid(
                "CronSchedule requires `every_secs` (recurring), `in_secs`, or `at_unix`",
            );
        };
        let trigger_prompt = action
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(String::from);
        let scheduled_action = action
            .get("action")
            .cloned()
            .filter(|a| a.get("type").and_then(|v| v.as_str()).is_some());
        let id = self.next_cron_id;
        self.next_cron_id += 1;
        // The model may narrow its own bounds (`max_fires`,
        // `expires_in_secs`); it cannot widen past the ceilings, and a
        // request with no bounds receives the ceilings rather than forever.
        let lease = crate::agent::lease::CapabilityLease::new(
            format!("cron '{label}'"),
            crate::agent::lease::LeaseProvenance::Model,
            now,
            action.get("expires_in_secs").and_then(|v| v.as_u64()),
            action
                .get("max_fires")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .or(if every_secs.is_none() { Some(1) } else { None }),
        );
        self.crons.lock().unwrap().push(PendingCron {
            id,
            label: label.clone(),
            due_at_unix,
            trigger_prompt: trigger_prompt.clone(),
            action: scheduled_action.clone(),
            every_secs,
            lease,
        });
        let _ = self.evt_tx.send(AgentEvent::CronScheduled {
            id,
            label: label.clone(),
            due_at_unix,
            trigger_prompt,
        });
        let fires = if scheduled_action.is_some() {
            "action"
        } else if action.get("prompt").is_some() {
            "prompt"
        } else {
            "reminder"
        };
        Self::ok(serde_json::json!({
            "cron_id": id, "label": label, "due_at_unix": due_at_unix,
            "recurring": every_secs.is_some(), "fires": fires,
        }))
    }

    fn cron_cancel(&self, action: &Value) -> Value {
        let Some(id) = action.get("id").and_then(|v| v.as_u64()).map(|n| n as u32) else {
            return Self::invalid("CronCancel requires a numeric `id`");
        };
        self.crons.lock().unwrap().retain(|c| c.id != id);
        let _ = self.evt_tx.send(AgentEvent::CronCancelled { id });
        Self::ok(serde_json::json!({"cron_id": id, "cancelled": true}))
    }
}

impl FrontendActions for TuiFrontendActions {
    fn claims(&self, action_type: &str) -> Option<x0ne_actions::SideEffectClass> {
        Intercepted::from_type_name(action_type).map(|i| i.side_effect_class())
    }

    fn execute(&mut self, action: &Value) -> Value {
        let name = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match Intercepted::from_type_name(name) {
            Some(Intercepted::PaintBadge) => self.paint_badge(action),
            Some(Intercepted::AskUser) => self.ask_user(action),
            Some(Intercepted::BadgeWatch) => self.badge_watch(action),
            Some(Intercepted::BadgeWatchClear) => {
                self.badge.invalidate("cleared by the model");
                let _ = self
                    .evt_tx
                    .send(AgentEvent::Info("badge watch stopped".into()));
                Self::ok(serde_json::json!({"cleared": true}))
            }
            Some(Intercepted::TaskAdd) => self.task_add(action),
            Some(Intercepted::TaskDone) => self.task_done(action),
            Some(Intercepted::TaskClear) => {
                let _ = self.evt_tx.send(AgentEvent::TasksCleared);
                Self::ok(serde_json::json!({"cleared": true}))
            }
            Some(Intercepted::CronSchedule) => self.cron_schedule(action),
            Some(Intercepted::CronCancel) => self.cron_cancel(action),
            // `execute` is only called after `claims` returned `Some`; an
            // unknown name here is a dispatcher bug, answered fail-closed.
            None => Self::invalid("not a frontend action"),
        }
    }
}

/// Scan a result envelope (possibly a composite) for embedded `PaintBadge`
/// stub results and repaint from them, so a badge frame inside a Pipeline
/// still lands on screen. Ported unchanged from the historical runner.
pub fn forward_embedded_badge_paints(value: &Value, evt_tx: &mpsc::UnboundedSender<AgentEvent>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("PaintBadge") {
                let frame_json = serde_json::to_string(value).unwrap_or_default();
                let _ = evt_tx.send(AgentEvent::BadgeUpdate { frame_json });
                return;
            }
            for v in map.values() {
                forward_embedded_badge_paints(v, evt_tx);
            }
        }
        Value::Array(items) => {
            for v in items {
                forward_embedded_badge_paints(v, evt_tx);
            }
        }
        _ => {}
    }
}

/// Spawn a live badge-watcher task. Runs `cmd` via `bash -c` every
/// `interval_secs`, formats the output into a badge frame, and emits a
/// `BadgeUpdate`. The epoch is checked immediately before every run rather
/// than trusted from arm time. Ported unchanged from the historical runner.
pub fn spawn_badge_watch(
    cmd: String,
    interval_secs: u64,
    label: String,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    policy_epoch: Arc<std::sync::atomic::AtomicU64>,
    armed_at_epoch: u64,
) -> tokio::task::JoinHandle<()> {
    let interval = std::time::Duration::from_secs(interval_secs.clamp(1, 3600));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if policy_epoch.load(std::sync::atomic::Ordering::SeqCst) != armed_at_epoch {
                return;
            }
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(&cmd)
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            let text = match output {
                Ok(Ok(out)) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stdout.trim().is_empty() {
                        stdout.into_owned()
                    } else {
                        stderr.into_owned()
                    }
                }
                Ok(Err(e)) => format!("err: {}", e),
                Err(_) => "timed out".to_string(),
            };
            let frame_json = badge::format_watch_frame(&label, &text);
            if evt_tx.send(AgentEvent::BadgeUpdate { frame_json }).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::CliAgentSession;
    use crate::config::Config;
    use crate::tui::gate::WireAnswer;
    use serde_json::json;
    use tokio::sync::mpsc::unbounded_channel;

    struct ScriptedBackend {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    #[async_trait::async_trait]
    impl crate::api::LlmBackend for ScriptedBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            let next = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "done".into());
            Ok(crate::api::ApiResponse {
                content: next,
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "scripted".into()
        }
    }

    struct CountingBridge {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::bridge::Bridge for CountingBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            // A preflight probe is a question, not work: it is neither
            // counted nor recorded, and it is answered with nothing that
            // parses as a verdict, so the human gate stays in charge exactly
            // as it did before the preflight existed.
            if crate::agent::preflight::is_probe(action)
                || crate::agent::undo::is_bookkeeping(action)
            {
                return Ok(serde_json::json!({"success": true, "output": {}}));
            }
            let t = action["type"].as_str().unwrap_or("?").to_string();
            self.seen.lock().unwrap().push(t.clone());
            Ok(json!({"success": true, "action_type": t, "output": {}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    fn action_reply(action: Value) -> String {
        format!("<action>{action}</action>")
    }

    struct Rig {
        agent: Box<CliAgentSession>,
        evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
        confirm_gate: Arc<HumanGate>,
        #[allow(dead_code)]
        ask_gate: Arc<AskGate>,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// The production assembly, in miniature: session + TuiDisplaySink +
    /// TuiFrontendActions, exactly as `run_runner` builds them.
    fn rig(replies: Vec<String>) -> Rig {
        let (evt_tx, evt_rx) = unbounded_channel();
        let confirm_gate = Arc::new(HumanGate::default());
        let ask_gate = Arc::new(AskGate::default());
        let badge = Arc::new(BadgeWatchSlot::default());
        let crons = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = TuiDisplaySink {
            evt_tx: evt_tx.clone(),
            confirm_gate: Arc::clone(&confirm_gate),
            badge: Arc::clone(&badge),
        };
        let mut agent = Box::new(CliAgentSession::new(
            Config::default(),
            Arc::new(ScriptedBackend {
                replies: std::sync::Mutex::new(replies.into()),
            }),
            "SYS",
            Some(Box::new(CountingBridge {
                seen: Arc::clone(&seen),
            })),
            Box::new(sink),
        ));
        agent.frontend = Some(Box::new(TuiFrontendActions::new(
            evt_tx,
            Arc::clone(&ask_gate),
            Arc::clone(&badge),
            crons,
        )));
        Rig {
            agent,
            evt_rx,
            confirm_gate,
            ask_gate,
            seen,
        }
    }

    fn destructive() -> Value {
        json!({"type": "MoveToTrash", "path": "/tmp/x"})
    }

    /// Drive one turn the way the runner does — turn on a spawned task, the
    /// test body playing the router — answering each confirmation with
    /// `answers` in order (silence when they run out).
    async fn run_turn_answering(
        mut r: Rig,
        input: &str,
        answers: Vec<WireAnswer>,
    ) -> (
        Box<CliAgentSession>,
        crate::agent::driver::TurnReport,
        Vec<String>,
        Vec<String>,
    ) {
        let mut agent = r.agent;
        let input = input.to_string();
        let handle = tokio::spawn(async move {
            let report = agent.run_turn(&input).await;
            (agent, report)
        });
        let mut answers = answers.into_iter();
        let mut events = Vec::new();
        let mut asked = Vec::new();
        let mut handle = handle;
        // The router: receive events while the turn runs; every question is
        // proof the UI stayed live while the driver was parked.
        let (agent, report) = loop {
            tokio::select! {
                ev = r.evt_rx.recv() => {
                    let Some(ev) = ev else { break handle.await.expect("turn task") };
                    if let AgentEvent::ConfirmationRequested { id, action_type, .. } = &ev {
                        asked.push(action_type.clone());
                        match answers.next() {
                            Some(a) => { r.confirm_gate.answer(id, a); }
                            None => { r.confirm_gate.abandon(); }
                        }
                    }
                    events.push(kind_of(&ev));
                }
                finished = &mut handle => {
                    break finished.expect("turn task");
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(2000)) => {
                    panic!("turn did not complete; events so far: {events:?}");
                }
            }
        };
        let seen = r.seen.lock().unwrap().clone();
        (agent, report, seen, asked)
    }

    fn kind_of(ev: &AgentEvent) -> String {
        match ev {
            AgentEvent::ToolStart { name, .. } => format!("start:{name}"),
            AgentEvent::ToolEnd { name, ok, .. } => format!("end:{name}:{ok}"),
            AgentEvent::ActionDenied { name, .. } => format!("denied:{name}"),
            AgentEvent::ConfirmationRequested { action_type, .. } => format!("ask:{action_type}"),
            AgentEvent::TaskAdded { .. } => "task-added".into(),
            AgentEvent::BadgeUpdate { .. } => "badge".into(),
            other => format!("{:?}", std::mem::discriminant(other)),
        }
    }

    /// 1+2 — a sensitive action raises ConfirmationRequested through the
    /// sink, an explicit Deny prevents execution, and the UI (played by the
    /// router loop) stayed responsive the whole time the driver was parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_deny_prevents_execution() {
        let r = rig(vec![action_reply(destructive()), "done".into()]);
        let (agent, report, seen, asked) = run_turn_answering(
            r,
            "trash it",
            vec![WireAnswer {
                approved: false,
                always: false,
            }],
        )
        .await;
        assert_eq!(asked, vec!["MoveToTrash"]);
        assert_eq!(report.outcome.kind(), "denied");
        assert!(seen.is_empty(), "a denied action reached the runtime");
        assert!(!agent.config.ui.auto_confirm);
    }

    /// 3 — Allow Once permits exactly that request: the identical action a
    /// second time is asked again, and silence then denies it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_allow_once_is_not_a_standing_grant() {
        let r = rig(vec![
            action_reply(destructive()),
            action_reply(destructive()),
            "done".into(),
        ]);
        let (_agent, report, seen, asked) = run_turn_answering(
            r,
            "trash twice",
            vec![WireAnswer {
                approved: true,
                always: false,
            }],
        )
        .await;
        // First ran, second was asked again and got silence → denial.
        assert_eq!(asked, vec!["MoveToTrash", "MoveToTrash"]);
        assert_eq!(seen, vec!["MoveToTrash"]);
        assert_eq!(report.outcome.kind(), "denied");
    }

    /// 4+5 — Always creates canonical scoped standing authority the same
    /// parent session reuses without a second question.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_always_grants_scoped_standing_authority() {
        let r = rig(vec![
            action_reply(destructive()),
            action_reply(destructive()),
            "done".into(),
        ]);
        let (agent, report, seen, asked) = run_turn_answering(
            r,
            "trash twice",
            vec![WireAnswer {
                approved: true,
                always: true,
            }],
        )
        .await;
        assert_eq!(report.response(), Some("done"));
        assert_eq!(
            asked,
            vec!["MoveToTrash"],
            "the standing grant did not cover the identical second action"
        );
        assert_eq!(seen, vec!["MoveToTrash", "MoveToTrash"]);
        assert_eq!(agent.state.authority.grant_count(), 1);
        assert!(
            !agent.config.ui.auto_confirm,
            "a grant became a config flag"
        );
    }

    /// 9 — a disconnected human (event channel with no router) fails closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2e_tui_disconnected_ui_fails_closed() {
        let mut r = rig(vec![action_reply(destructive()), "done".into()]);
        // Drop the receiver: there is no UI left to ask.
        r.evt_rx.close();
        drop(r.evt_rx);
        let report = r.agent.run_turn("trash it").await;
        assert_eq!(report.outcome.kind(), "denied");
        assert!(r.seen.lock().unwrap().is_empty());
    }

    /// 10 — a stale answer (wrong id) does not authorize; the real question
    /// still gets its real (absent) answer and the action is denied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_stale_answer_is_rejected() {
        let r = rig(vec![action_reply(destructive()), "done".into()]);
        let confirm_gate = Arc::clone(&r.confirm_gate);
        let mut agent = r.agent;
        let mut evt_rx = r.evt_rx;
        let seen = r.seen;
        let handle = tokio::spawn(async move {
            let report = agent.run_turn("trash it").await;
            (agent, report)
        });
        // Wait for the real question, answer with a *wrong* id, then abandon.
        loop {
            match evt_rx.recv().await {
                Some(AgentEvent::ConfirmationRequested { id, .. }) => {
                    assert!(
                        !confirm_gate.answer(
                            "confirm-999",
                            WireAnswer {
                                approved: true,
                                always: true
                            }
                        ),
                        "a stale answer was accepted"
                    );
                    let _ = id;
                    confirm_gate.abandon();
                    break;
                }
                Some(_) => continue,
                None => panic!("event channel closed early"),
            }
        }
        let (_agent, report) = handle.await.unwrap();
        assert_eq!(report.outcome.kind(), "denied");
        assert!(seen.lock().unwrap().is_empty());
    }

    /// 12 — an unknown action executes nowhere, through this frontend too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_unknown_action_executes_nowhere() {
        let r = rig(vec![
            action_reply(json!({"type": "NoSuchActionAnywhere", "x": 1})),
            "done".into(),
        ]);
        // Unknown names are assumed destructive, so a question arrives;
        // approving it changes nothing — the route table still refuses.
        let (_agent, _report, seen, _asked) = run_turn_answering(
            r,
            "try it",
            vec![WireAnswer {
                approved: true,
                always: false,
            }],
        )
        .await;
        assert!(
            seen.is_empty(),
            "an unknown action reached the runtime: {seen:?}"
        );
    }

    /// A Meta-classed frontend action (TaskAdd) executes with no
    /// confirmation, never reaches the runtime, and lands in the panel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_task_add_is_ungated_ui_state() {
        let r = rig(vec![
            action_reply(json!({"type": "TaskAdd", "title": "port the renderers"})),
            "done".into(),
        ]);
        let (agent, report, seen, asked) = run_turn_answering(r, "plan it", Vec::new()).await;
        assert_eq!(report.response(), Some("done"));
        assert!(asked.is_empty(), "a task-panel row required approval");
        assert!(seen.is_empty(), "a frontend action reached the runtime");
        // And the model was told the id, through canonical history.
        let transcript = agent
            .state
            .history
            .messages()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(transcript.contains("task_id"), "{transcript}");
    }

    /// A ProcessSpawn-classed frontend action (BadgeWatch) is human-gated:
    /// with nobody answering, it is denied and never armed. The historical
    /// TUI armed it with no human anywhere in the path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn e2e_tui_badge_watch_is_human_gated() {
        let r = rig(vec![
            action_reply(json!({"type": "BadgeWatch", "cmd": "echo hi", "interval_secs": 60})),
            "done".into(),
        ]);
        let (_agent, report, _seen, asked) = run_turn_answering(r, "watch it", Vec::new()).await;
        assert_eq!(asked, vec!["BadgeWatch"], "arming was never put to a human");
        assert_eq!(report.outcome.kind(), "denied");
    }

    /// The policy-verdict hygiene survived the cutover: a ToolEnd carrying a
    /// policy denial retracts the armed watch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_policy_denial_in_the_event_stream_retracts_the_watch() {
        let (evt_tx, _evt_rx) = unbounded_channel();
        let badge = Arc::new(BadgeWatchSlot::default());
        let epoch_before = badge.current_epoch();
        badge.install(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }));
        assert!(badge.is_armed());
        let mut sink = TuiDisplaySink {
            evt_tx,
            confirm_gate: Arc::new(HumanGate::default()),
            badge: Arc::clone(&badge),
        };
        use crate::display_sink::DisplaySink;
        sink.emit(&AgentEvent::ToolEnd {
            name: "RunCommand".into(),
            ok: false,
            summary: "denied".into(),
            action: json!({"type": "RunCommand"}),
            result: json!({"success": false, "error": {"class": "policy_denied"}}),
            duration_ms: 1,
        });
        assert!(!badge.is_armed(), "the watch survived a policy denial");
        assert!(badge.current_epoch() > epoch_before);
    }
}
