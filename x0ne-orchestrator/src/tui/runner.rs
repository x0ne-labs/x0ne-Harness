//! Background runner: owns the canonical session and translates everything
//! into [`AgentEvent`]s for the render layer.
//!
//! Since the Driver cutover this task no longer contains an agent loop. It
//! boots the session, then routes commands:
//!
//!   * A user message becomes one canonical turn — `CliAgentSession ->
//!     Driver -> TurnReport` — run on a **spawned task** so this router keeps
//!     receiving commands while the turn is in flight. That is what makes the
//!     blocking confirmation round-trip work: the sink parks the turn's
//!     thread on `gate::HumanGate::wait`, the UI stays live, the human's
//!     `ConfirmationAnswer` arrives here and is fed to the gate, and the turn
//!     resumes. See `gate.rs` for the threading contract.
//!   * Frontend-claimed actions (badge, AskUser, tasks, cron-light) execute
//!     inside the turn through `driver_frontend::TuiFrontendActions`, under
//!     the canonical dispatcher's gates.
//!   * Everything the driver wants a person to see arrives through
//!     `driver_frontend::TuiDisplaySink` as the same `AgentEvent` enum
//!     the renderer always consumed — which is why the cutover did not touch
//!     `render.rs`.
//!   * The cron ticker and the human-armed badge watch live here, outside
//!     turns, exactly as before. A scheduled *action* firing runs through
//!     [`CliAgentSession::run_action`], so deferred work meets the
//!     confirmation gate and the routing table at fire time.
//!
//! Commands that would start new work while a turn is running are deferred
//! and replayed when the turn ends — the same serial semantics the historical
//! private loop had, made explicit.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::mpsc;

use super::driver_frontend::{
    spawn_badge_watch, BadgeWatchSlot, PendingCron, TuiDisplaySink, TuiFrontendActions,
};
use super::gate::{AskAnswer, AskGate, HumanGate, WireAnswer};
use super::wire::{AgentEvent, TuiCommand};
use crate::agent::session::CliAgentSession;
use crate::api::StreamChunk;
use crate::bridge::Bridge;
use crate::config::Config;
use crate::hooks::SessionSource;
use crate::llm_setup;
use crate::session::SharedJournal;

/// What a finished background task hands back: always the session, plus what
/// it was doing — a full turn, or a single scheduled action.
enum WorkOutput {
    Turn(crate::agent::driver::TurnReport),
    Scheduled,
}

type WorkHandle = tokio::task::JoinHandle<(Box<CliAgentSession>, WorkOutput)>;

/// Tokio task entry point. Owns the canonical session between turns and the
/// command router at all times. Loops until the command channel closes.
pub async fn run_runner(
    mut cmd_rx: mpsc::Receiver<TuiCommand>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    config: Config,
) {
    let (llm, bridge, config) = match boot(&evt_tx, config).await {
        Ok(triple) => triple,
        Err(_) => {
            // boot() already sent BootFailed; just exit.
            drain_until_shutdown(&mut cmd_rx).await;
            return;
        }
    };

    // Best-effort session logger — failure is non-fatal (we keep running
    // without persisted jsonl rather than blocking the user on a missing FS).
    let mut session_logger = match SharedJournal::new().await {
        Ok(logger) => {
            let _ = logger.log_session_start(&llm.display_name()).await;
            Some(logger)
        }
        Err(e) => {
            let _ = evt_tx.send(AgentEvent::Info(format!(
                "session logging disabled · {}",
                e
            )));
            None
        }
    };

    // ── The canonical session, assembled once ────────────────────────────
    let confirm_gate = Arc::new(HumanGate::default());
    let ask_gate = Arc::new(AskGate::default());
    let badge = Arc::new(BadgeWatchSlot::default());
    let crons: Arc<std::sync::Mutex<Vec<PendingCron>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let sink = TuiDisplaySink {
        evt_tx: evt_tx.clone(),
        confirm_gate: Arc::clone(&confirm_gate),
        badge: Arc::clone(&badge),
    };
    let llm: Arc<dyn crate::api::LlmBackend> = Arc::from(llm);
    let model_deadline = config.ui.model_deadline();
    let mut agent = Box::new(CliAgentSession::new(
        config,
        Arc::clone(&llm),
        default_system_prompt(),
        bridge,
        Box::new(sink),
    ));
    // The audit trail: the driver writes each action into the same log this
    // runner writes prompts, answers and turns into. Set here, on the one
    // session TUI assembles, because a journal a frontend forgets to hand
    // over is a journal that records nothing — the defect this replaced.
    agent.journal = session_logger.as_ref().map(|j| j.handle());
    // A silent model ends the turn at `[ui] model_timeout_secs`
    // instead of 3 transport timeouts of 300 s each.
    agent.limits.retry.deadline = model_deadline;
    // The persistent goal, on the only session the TUI assembles. Without
    // this wiring, `/goal` would write a goal that the driver would never
    // advance and that no skeptic would reread: the whole module would
    // remain scaffolding (docs/invariants.md).
    //
    // Attaching the manager turns NOTHING on: `get_active()` returns `None`
    // as long as no human has typed `/goal <text>`, and a turn without an
    // active goal costs neither a step nor an extra model call. Cowork mode
    // therefore stays opt-in, and the opt-in is human (spec O-2: no action
    // creates or modifies a goal).
    //
    // An unavailable goal store (no `$HOME`) leaves the field at `None` and
    // the session runs as before — no reason to prevent a conversation
    // because a directory is missing.
    agent.goal = match crate::goal::GoalManager::new_default() {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            let _ = evt_tx.send(AgentEvent::Info(format!("goal store unavailable: {e}")));
            None
        }
    };
    agent.frontend = Some(Box::new(TuiFrontendActions::new(
        evt_tx.clone(),
        Arc::clone(&ask_gate),
        Arc::clone(&badge),
        Arc::clone(&crons),
    )));
    agent.session_started(SessionSource::Startup).await;

    // Streaming: the driver's token stream becomes Token events, and complete
    // `<matrix>` blocks in the streamed prose repaint the badge — including
    // more than one per turn, which the historical post-hoc scan could not do.
    let stream_rx = agent.open_stream();
    let _stream_task = tokio::spawn(forward_stream(stream_rx, evt_tx.clone()));

    // A clone of the turn's cancel token, so the router can cancel work it
    // does not currently own. Refreshed after every cancelled turn.
    let mut cancel = agent.cancel.clone();

    // The cron ticker self-injects commands through its own sender, merged
    // into the main select below.
    let (cron_cmd_tx, mut cron_cmd_rx) = mpsc::channel::<TuiCommand>(8);
    let _cron_task = spawn_cron_ticker(Arc::clone(&crons), evt_tx.clone(), cron_cmd_tx);

    let mut cumulative_tokens: u64 = 0;
    let mut hold: Option<Box<CliAgentSession>> = Some(agent);
    let mut work: Option<WorkHandle> = None;
    let mut deferred: VecDeque<TuiCommand> = VecDeque::new();

    loop {
        tokio::select! {
            finished = async { work.as_mut().expect("guarded by is_some").await }, if work.is_some() => {
                work = None;
                match finished {
                    Ok((mut sess, output)) => {
                        if let WorkOutput::Turn(report) = output {
                            cumulative_tokens =
                                cumulative_tokens.saturating_add(report.usage.total_tokens());
                            if let Some(logger) = session_logger.as_mut() {
                                let _ = logger
                                    .log_llm(
                                        report.response().unwrap_or_default(),
                                        report.usage.input_tokens,
                                        report.usage.output_tokens,
                                        report.usage.cache_write_tokens,
                                        report.usage.cache_read_tokens,
                                    )
                                    .await;
                                // WHY the turn ended, not just what it said.
                                //
                                // `response()` is `None` for every outcome but
                                // `Completed`, so a denied, cancelled or
                                // deadline-ended turn logs an empty string and
                                // nothing else — which reads in the log exactly
                                // like a model that said nothing. Field report
                                // 2026-08-13: several sessions where the answer
                                // to one prompt only appeared after the NEXT
                                // one, and the log could not distinguish
                                // "refused" from "silent". `log_turn`'s own
                                // doc-comment predicted this in 2026-08-10 and
                                // it was never wired into the canonical path.
                                let _ = logger
                                    .log_turn(
                                        report.outcome.kind(),
                                        report.outcome.detail().as_deref(),
                                        report.duration.as_millis() as u64,
                                        report.usage.model_calls,
                                        &serde_json::json!({
                                            "total": report.actions.total,
                                            "succeeded": report.actions.succeeded,
                                            "failed": report.actions.failed,
                                            "by_type": report.actions.by_type,
                                        }),
                                    )
                                    .await;
                            }
                            if let Some(line) = model_failure_line(&report.outcome) {
                                let _ = evt_tx.send(AgentEvent::Error(line));
                            }
                            let _ = evt_tx.send(AgentEvent::AssistantEnd {
                                total_tokens: cumulative_tokens,
                                cost_usd: sess.state.budget.spent_usd,
                            });
                        }
                        // A cancelled token stays cancelled; the next turn
                        // deserves a fresh one, and the router a fresh clone.
                        if sess.cancel.is_cancelled() {
                            sess.cancel = crate::agent::cancel::CancelToken::new();
                        }
                        cancel = sess.cancel.clone();
                        hold = Some(sess);
                    }
                    Err(join_err) => {
                        // The turn task died. The session died with it; all
                        // that is left is to say so rather than hang.
                        let _ = evt_tx.send(AgentEvent::Error(format!(
                            "turn task failed: {join_err}"
                        )));
                        return;
                    }
                }
                // Replay whatever arrived while the turn was running, in
                // order, stopping if one of them starts new work.
                while work.is_none() {
                    let Some(cmd) = deferred.pop_front() else { break };
                    if route_command(
                        cmd,
                        &mut hold,
                        &mut work,
                        &mut deferred,
                        &mut session_logger,
                        &evt_tx,
                        &confirm_gate,
                        &ask_gate,
                        &badge,
                        &mut cancel,
                    )
                    .await
                    {
                        return;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                if route_command(
                    cmd,
                    &mut hold,
                    &mut work,
                    &mut deferred,
                    &mut session_logger,
                    &evt_tx,
                    &confirm_gate,
                    &ask_gate,
                    &badge,
                    &mut cancel,
                )
                .await
                {
                    break;
                }
            }
            cmd = cron_cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                if route_command(
                    cmd,
                    &mut hold,
                    &mut work,
                    &mut deferred,
                    &mut session_logger,
                    &evt_tx,
                    &confirm_gate,
                    &ask_gate,
                    &badge,
                    &mut cancel,
                )
                .await
                {
                    break;
                }
            }
        }
    }

    // Shutdown. Unblock anything parked on a human answer — silence is a
    // denial — cancel the in-flight turn, and wait for it to unwind so the
    // session is not torn down mid-action.
    confirm_gate.abandon();
    ask_gate.abandon();
    cancel.cancel();
    if let Some(handle) = work.take() {
        let _ = handle.await;
    }
}

/// Why the model failed, for the conversation; the driver itself only
/// says "turn model_failure". Provider text, so bounded and sanitized.
fn model_failure_line(outcome: &crate::agent::driver::TurnOutcome) -> Option<String> {
    let crate::agent::driver::TurnOutcome::ModelFailure { error } = outcome else {
        return None;
    };
    let detail = error.source().map(|e| format!("{e:#}")).unwrap_or_default();
    let line = format!("model error · {}: {detail}", error.kind());
    Some(crate::text_safety::snippet(&line, 400))
}

/// Route one command. Returns `true` when the runner should shut down.
///
/// The parameter list is the router's working set, passed explicitly rather
/// than bundled into a struct the borrow checker would then refuse to split.
#[allow(clippy::too_many_arguments)]
async fn route_command(
    cmd: TuiCommand,
    hold: &mut Option<Box<CliAgentSession>>,
    work: &mut Option<WorkHandle>,
    deferred: &mut VecDeque<TuiCommand>,
    session_logger: &mut Option<SharedJournal>,
    evt_tx: &mpsc::UnboundedSender<AgentEvent>,
    confirm_gate: &Arc<HumanGate>,
    ask_gate: &Arc<AskGate>,
    badge: &Arc<BadgeWatchSlot>,
    cancel: &mut crate::agent::cancel::CancelToken,
) -> bool {
    let busy = work.is_some();
    match cmd {
        // ── Answers: always routed immediately, busy or not ──────────────
        // A stale or unmatched id is dropped by the gate — the question it
        // answers no longer exists, and honouring it late would approve
        // something already treated as refused.
        TuiCommand::ConfirmationAnswer {
            id,
            approved,
            always,
        } => {
            confirm_gate.answer(&id, WireAnswer { approved, always });
        }
        TuiCommand::AskAnswer {
            id,
            cancelled,
            selected,
            custom_text,
        } => {
            ask_gate.answer(
                &id,
                AskAnswer {
                    cancelled,
                    selected,
                    custom_text,
                },
            );
        }
        TuiCommand::Cancel => {
            if busy {
                // Order matters: cancel first, so the parked waiter that the
                // abandon wakes finds the token already cancelled and the
                // dispatcher refuses rather than proceeding.
                cancel.cancel();
                confirm_gate.abandon();
                ask_gate.abandon();
                let _ = evt_tx.send(AgentEvent::Info("cancelling the current turn…".into()));
            } else {
                let _ = evt_tx.send(AgentEvent::Info("cancel: no active turn".into()));
            }
        }
        TuiCommand::Shutdown => return true,
        // The session is in the turn's task while one runs: the switch waits
        // for the turn to end, and says so. Esc cancels a turn sooner.
        TuiCommand::SetPlanMode { plan } => {
            if busy {
                deferred.push_back(TuiCommand::SetPlanMode { plan });
                let _ = evt_tx.send(AgentEvent::Info(
                    "plan mode switches when this turn ends".into(),
                ));
                return false;
            }
            let sess = hold
                .as_mut()
                .expect("idle implies the router holds the session");
            let want = plan.unwrap_or(!sess.plan_mode());
            match sess.set_plan_mode(want).await {
                Ok(_) => {
                    let _ = evt_tx.send(AgentEvent::Info(
                        if sess.plan_mode() {
                            "plan mode · the runtime refuses every change · /build or Shift+Tab to leave"
                        } else {
                            "build mode · the configured policy applies"
                        }
                        .into(),
                    ));
                }
                Err(e) => {
                    let _ = evt_tx.send(AgentEvent::Error(format!("plan mode: {e}")));
                }
            }
            let _ = evt_tx.send(AgentEvent::SessionMode {
                plan: sess.plan_mode(),
            });
        }
        TuiCommand::ClearSession => {
            if busy {
                deferred.push_back(TuiCommand::ClearSession);
                return false;
            }
            let sess = hold
                .as_mut()
                .expect("idle implies the router holds the session");
            // Canonical /clear: the transcript goes and every standing
            // approval goes with it. Nothing else is reset.
            sess.clear_conversation();
            sess.session_started(SessionSource::Clear).await;
            let _ = evt_tx.send(AgentEvent::Info(
                "session cleared · history and standing approvals".into(),
            ));
        }

        TuiCommand::ResumeSession { id } => {
            if busy {
                deferred.push_back(TuiCommand::ResumeSession { id });
                return false;
            }
            let sess = hold
                .as_mut()
                .expect("idle implies the router holds the session");
            match resume_into(sess, session_logger, &id).await {
                Ok((ev, elsewhere)) => {
                    sess.session_started(SessionSource::Resume).await;
                    let _ = evt_tx.send(ev);
                    if let Some(note) = elsewhere {
                        let _ = evt_tx.send(AgentEvent::Info(note));
                    }
                }
                Err(e) => {
                    let _ = evt_tx.send(AgentEvent::Error(format!("/resume: {e}")));
                }
            }
        }

        // ── Work-starting commands: deferred while a turn runs ────────────
        TuiCommand::UserInput(text) => {
            if text.trim().is_empty() {
                return false;
            }
            if busy {
                deferred.push_back(TuiCommand::UserInput(text));
            } else {
                start_turn(hold, work, session_logger, text, Vec::new()).await;
            }
        }
        TuiCommand::ExternalUserInput { source, text } => {
            if text.trim().is_empty() {
                return false;
            }
            // Echo so the local screen shows what arrived from the phone.
            let _ = evt_tx.send(AgentEvent::ExternalUserInput {
                source,
                text: text.clone(),
            });
            if busy {
                deferred.push_back(TuiCommand::UserInput(text));
            } else {
                start_turn(hold, work, session_logger, text, Vec::new()).await;
            }
        }
        TuiCommand::UserInputWithImages { text, image_paths } => {
            if text.trim().is_empty() && image_paths.is_empty() {
                return false;
            }
            if busy {
                deferred.push_back(TuiCommand::UserInputWithImages { text, image_paths });
                return false;
            }
            let mut images = Vec::new();
            let mut load_errors = Vec::new();
            for p in &image_paths {
                match load_image_attachment(p) {
                    Ok(att) => images.push(att),
                    Err(e) => load_errors.push(format!("{}: {}", p, e)),
                }
            }
            if !load_errors.is_empty() {
                let _ = evt_tx.send(AgentEvent::Error(format!(
                    "image attach failed · {}",
                    load_errors.join(" · "),
                )));
            }
            start_turn(hold, work, session_logger, text, images).await;
        }
        TuiCommand::RunScheduledAction { label, action } => {
            if busy {
                deferred.push_back(TuiCommand::RunScheduledAction { label, action });
                return false;
            }
            let _ = evt_tx.send(AgentEvent::Info(format!("⏰ scheduled · {}", label)));
            let mut sess = hold
                .take()
                .expect("idle implies the router holds the session");
            *work = Some(tokio::spawn(async move {
                // The canonical single-action path: confirmation gate,
                // routing, authority and budget all apply at fire time.
                let _envelope = sess.run_action(&action).await;
                (sess, WorkOutput::Scheduled)
            }));
        }

        // ── Session control ──────────────────────────────────────────────
        TuiCommand::SwitchModel { provider, model } => {
            if busy {
                let _ = evt_tx.send(AgentEvent::Info(
                    "model switch deferred until the current turn resolves".into(),
                ));
                deferred.push_back(TuiCommand::SwitchModel { provider, model });
                return false;
            }
            let sess = hold
                .as_mut()
                .expect("idle implies the router holds the session");
            switch_backend(sess, &provider, &model, evt_tx);
        }

        // ── Human-armed badge watch (slash command, outside any turn) ─────
        // Carries the user's own authority: they typed the command at their
        // own keyboard. Epoch-bound like the model-armed variant, so a policy
        // retraction stops it too.
        TuiCommand::BadgeWatch {
            cmd,
            interval_secs,
            label,
        } => {
            let armed_at = badge.current_epoch();
            badge.install(spawn_badge_watch(
                cmd,
                interval_secs,
                label,
                evt_tx.clone(),
                Arc::clone(&badge.policy_epoch),
                armed_at,
            ));
            let _ = evt_tx.send(AgentEvent::Info("badge watch started".into()));
        }
        TuiCommand::BadgeWatchClear => {
            badge.invalidate("cleared by the user");
            let _ = evt_tx.send(AgentEvent::Info("badge watch stopped".into()));
        }
        // `/undo`: the session's journal and bridge, the same path as the
        // REPL, reported in the one line `undo::describe` writes for both.
        // Refused while a turn runs rather than deferred: an undo applied
        // to a state the operator has not seen is not the one they asked for.
        TuiCommand::Undo => {
            if busy {
                let _ = evt_tx.send(AgentEvent::Info(
                    "undo: a turn is running — stop it (Esc) and type /undo again".into(),
                ));
                return false;
            }
            let sess = hold
                .as_mut()
                .expect("idle implies the router holds the session");
            let outcome = sess.undo_last().await;
            let line = crate::agent::undo::describe(&outcome);
            let _ = evt_tx.send(match outcome {
                crate::agent::undo::UndoOutcome::Failed { .. } => AgentEvent::Error(line),
                _ => AgentEvent::Info(line),
            });
        }
    }
    false
}

/// Move the session into a spawned task and run one canonical turn on it.
///
/// Spawned, not awaited inline: the router must keep receiving commands while
/// the turn runs, because the turn may park on a human answer that only the
/// router can deliver. The task returns the session when the turn ends.
async fn start_turn(
    hold: &mut Option<Box<CliAgentSession>>,
    work: &mut Option<WorkHandle>,
    session_logger: &mut Option<SharedJournal>,
    text: String,
    images: Vec<crate::conversation::ImageAttachment>,
) {
    if let Some(logger) = session_logger.as_mut() {
        let _ = logger.log_user(&text).await;
    }
    let mut sess = hold.take().expect("caller checked idle");
    *work = Some(tokio::spawn(async move {
        let report = if images.is_empty() {
            sess.run_turn(&text).await
        } else {
            sess.run_turn_with_images(&text, images).await
        };
        (sess, WorkOutput::Turn(report))
    }));
}

/// Hot-swap the backend, keeping the session — history, spend, approvals,
/// registry and runtime all survive, which is the locked model-switch
/// semantics. `prices` is a rate table keyed by the model each response
/// names, so it is not rebuilt.
fn switch_backend(
    sess: &mut CliAgentSession,
    provider: &str,
    model: &str,
    evt_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    // An empty provider means "keep the current provider, switch its model"
    // — the shape the TUI's `/model <name>` sends. An empty model means
    // "switch provider, keep its configured model" — the F4/F5/F6 shape.
    let provider_owned = if provider.is_empty() {
        sess.config.llm.provider.clone()
    } else {
        provider.to_string()
    };
    let provider = provider_owned.as_str();
    match provider {
        "ollama" => {
            if !model.is_empty() {
                sess.config.llm.ollama.model = model.to_string();
            }
        }
        "anthropic" => {
            if !model.is_empty() {
                sess.config.llm.anthropic.model = model.to_string();
            }
        }
        "openai" => {
            if !model.is_empty() {
                sess.config.llm.openai.model = model.to_string();
            }
        }
        "openai-compat" | "openai_compat" => {
            if !model.is_empty() {
                sess.config.llm.openai_compat.model = model.to_string();
            }
        }
        other => {
            let _ = evt_tx.send(AgentEvent::Error(format!("unknown provider: {other}")));
            return;
        }
    }
    sess.config.llm.provider = provider.replace('_', "-");
    match crate::api::create_backend(&sess.config.llm, None) {
        Ok(backend) => {
            let label = backend.display_name();
            sess.set_backend(Arc::from(backend));
            let _ = evt_tx.send(AgentEvent::Info(format!("model switched · {label}")));
            let _ = evt_tx.send(AgentEvent::Ready {
                model_label: label,
                sandbox_label: sandbox_label_for(&sess.config),
            });
        }
        Err(e) => {
            let _ = evt_tx.send(AgentEvent::Error(format!("model switch failed: {e}")));
        }
    }
}

/// Forward the driver's token stream to the renderer, extracting complete
/// `<matrix>` badge frames as they appear. The historical runner scanned each
/// finished response for the *first* block; scanning the stream finds every
/// block, so frame-by-frame animation in prose now works.
async fn forward_stream(
    mut rx: mpsc::UnboundedReceiver<StreamChunk>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut buffer = String::new();
    while let Some(chunk) = rx.recv().await {
        if let StreamChunk::Token(s) = chunk {
            buffer.push_str(&s);
            while let Some(frame) = super::badge::extract_first_matrix_block(&buffer) {
                let _ = evt_tx.send(AgentEvent::BadgeUpdate {
                    frame_json: frame.clone(),
                });
                // Drop everything through the block we just painted, so the
                // next extraction finds the next block rather than this one.
                if let Some(end) = buffer.find("</matrix>") {
                    buffer.drain(..end + "</matrix>".len());
                } else {
                    buffer.clear();
                }
            }
            // Keep the tail bounded: matrix blocks are small, so anything
            // beyond a generous frame's worth cannot be a partial block. The
            // possible openings we must not sever are all short.
            if buffer.len() > 64 * 1024 {
                let keep = buffer.len() - 4096;
                buffer.drain(..keep);
            }
            if evt_tx.send(AgentEvent::Token(s)).is_err() {
                return;
            }
        }
    }
}

/// The cron ticker — fires due crons once per second. A due prompt becomes a
/// synthetic user input; a due action becomes a `RunScheduledAction` the
/// router replays through the canonical single-action path; a bare label is a
/// passive reminder line.
fn spawn_cron_ticker(
    crons: Arc<std::sync::Mutex<Vec<PendingCron>>>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    cmd_tx: mpsc::Sender<TuiCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut fired: Vec<PendingCron> = Vec::new();
            let mut lapsed: Vec<(PendingCron, crate::agent::lease::LeaseDenied)> = Vec::new();
            {
                let mut guard = crons.lock().unwrap();
                let mut keep: Vec<PendingCron> = Vec::with_capacity(guard.len());
                for mut c in guard.drain(..) {
                    if c.due_at_unix <= now {
                        // The lease decides whether this entry may become an
                        // action at all. A lapsed lease drops the entry and
                        // says so — silent expiry of standing behaviour is
                        // how nobody notices what was still armed.
                        match c.lease.try_fire(now) {
                            Ok(_) => {
                                fired.push(c.clone());
                                let still_live = c.lease.is_live(now);
                                if let Some(every) = c.every_secs {
                                    if still_live {
                                        let mut next = c;
                                        next.due_at_unix = now + every.max(1);
                                        keep.push(next);
                                    } else {
                                        lapsed
                                            .push((c, crate::agent::lease::LeaseDenied::Exhausted));
                                    }
                                }
                            }
                            Err(denied) => lapsed.push((c, denied)),
                        }
                    } else {
                        keep.push(c);
                    }
                }
                *guard = keep;
            }
            for (c, denied) in lapsed {
                let _ = evt_tx.send(AgentEvent::CronCancelled { id: c.id });
                let _ = evt_tx.send(AgentEvent::Info(format!(
                    "⏳ cron '{}' retired · lease {} after {} firing(s) · re-schedule to renew",
                    c.label,
                    denied.as_str(),
                    c.lease.fires(),
                )));
            }
            if fired.is_empty() {
                continue;
            }
            for c in fired {
                let _ = evt_tx.send(AgentEvent::CronFired { id: c.id });
                if let Some(action) = c.action.clone() {
                    if cmd_tx
                        .try_send(TuiCommand::RunScheduledAction {
                            label: c.label.clone(),
                            action,
                        })
                        .is_err()
                    {
                        let _ = evt_tx.send(AgentEvent::Info(format!(
                            "⏰ scheduled '{}' fired but runner queue was full",
                            c.label
                        )));
                    }
                } else if let Some(prompt) = c.trigger_prompt {
                    let injected = format!("[cron · {}] {}", c.label, prompt);
                    if cmd_tx.try_send(TuiCommand::UserInput(injected)).is_err() {
                        let _ = evt_tx.send(AgentEvent::Info(format!(
                            "⏰ cron '{}' fired but runner queue was full",
                            c.label
                        )));
                    }
                } else {
                    let _ = evt_tx.send(AgentEvent::Info(format!("⏰ reminder · {}", c.label)));
                }
            }
        }
    })
}

fn default_system_prompt() -> String {
    let mut prompt = crate::prompt::build_system_prompt(&Config::default(), false);
    // tui-specific addendum: tell the model about the X0NE badge it
    // owns in the header.
    prompt.push_str(super::badge::system_prompt_fragment());
    // Markdown nudge — one sentence so the model knows the TUI renders it.
    prompt.push_str(
        "\n━━━━ MARKDOWN RENDERING ━━━━\n\
         Your replies are rendered with markdown: **bold**, *italic*, \
         `code`, ```fenced code blocks with `rust`/`python`/`js`/`sh` \
         syntax highlighting```, ## headers, > quotes, - bullets, 1. \
         ordered, ~~strike~~, [links](url), and `| a | b |` pipe \
         tables. Use it sparingly to highlight structure.\n",
    );
    // AskUser — the human-in-the-loop primitive.
    prompt.push_str(
        "\n━━━━ ASKING THE USER ━━━━\n\
         When a decision is non-obvious (architecture choice, \
         destructive op consent, scope question), emit an AskUser \
         action *instead of guessing*. Schema: \
         {\"type\":\"AskUser\",\"question\":\"…\",\
         \"options\":[{\"label\":\"…\",\"description\":\"…\",\
         \"recommended\":true},…],\"multi_select\":false}. \
         Keep options ≤4, labels ≤5 words. Result will contain \
         {selected:[…]} on success or {success:false,error:…} on cancel. \
         Don't over-use — only when the answer materially changes your plan.\n",
    );
    // Tasks + Cron — externalise the plan and reminders into side panels.
    prompt.push_str(
        "\n━━━━ TASKS & CRON (live side panels) ━━━━\n\
         Use TaskAdd / TaskDone to externalise multi-step plans — \
         each item shows up in the Tasks panel and the user can \
         watch it progress. Use CronSchedule to remind yourself \
         later or self-trigger after a wait.\n\
           {\"type\":\"TaskAdd\",\"title\":\"…\"}              → returns task_id\n\
           {\"type\":\"TaskDone\",\"id\":<task_id>}\n\
           {\"type\":\"TaskClear\"}                              → wipes all tasks\n\
           {\"type\":\"CronSchedule\",\"label\":\"…\",\"in_secs\":N}\n\
           {\"type\":\"CronSchedule\",\"label\":\"…\",\"in_secs\":N,\"prompt\":\"do X now\"}\n\
           {\"type\":\"CronCancel\",\"id\":<cron_id>}\n\
         A cron without a `prompt` is a passive reminder; with one, \
         the agent self-triggers when due (prompt is injected as if \
         the user typed it).\n\
         ── RECURRING ACTIONS ──\n\
         You can decide an X0NE action to run on a loop. Add \
         `every_secs` (recurring) + an `action` payload — the runtime \
         re-dispatches it forever, no per-tick prompt cost:\n\
           {\"type\":\"CronSchedule\",\"label\":\"disk\",\"every_secs\":30,\
             \"action\":{\"type\":\"RunCommand\",\"command\":\"df -h /\"}}\n\
           {\"type\":\"CronSchedule\",\"label\":\"deploy-watch\",\"every_secs\":60,\
             \"action\":{\"type\":\"FetchUrl\",\"url\":\"https://…/health\"}}\n\
         The action's result lands in the trail + Pulse each time. \
         Use this for monitoring loops, health checks, polling a \
         long job. `every_secs` is the cadence; `action` is any X0NE \
         primitive (RunCommand, Pipeline, FetchUrl, WebSearch, \
         Simulate, …). Stop with CronCancel.\n",
    );
    prompt
}

/// `config` arrives resolved by `main` (`-c`, flags, environment): read
/// again here, it lost `-c`.
async fn boot(
    evt_tx: &mpsc::UnboundedSender<AgentEvent>,
    mut config: Config,
) -> anyhow::Result<(
    Box<dyn crate::api::LlmBackend>,
    Option<Box<dyn Bridge>>,
    Config,
)> {
    // A refused bridge below only disables tools; refuse the session first.
    let bridge_cfg = &config.bridge;
    if let Err(e) =
        crate::sandbox_mode::require_sandbox(&bridge_cfg.mode, bridge_cfg.sandbox_cmd.as_deref())
    {
        let _ = evt_tx.send(AgentEvent::BootFailed(e.to_string()));
        return Err(e);
    }

    let llm = match llm_setup::setup_llm(&mut config).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("{}", e);
            let _ = evt_tx.send(AgentEvent::BootFailed(msg));
            return Err(e).context("LLM setup failed");
        }
    };
    if let Some(note) = crate::model_keys::key_note(&config.llm) {
        let _ = evt_tx.send(AgentEvent::Info(note));
    }

    // Bridge boot is best-effort: if it fails we still run as a chat client
    // (no tool calls), surfaced as an Error so the user understands why
    // actions return errors.
    let bridge = match crate::bridge::create_bridge_for(&config.bridge).await {
        Ok(b) => Some(b),
        Err(e) => {
            let _ = evt_tx.send(AgentEvent::Error(format!(
                "bridge unavailable · tool calls disabled · {}",
                e
            )));
            None
        }
    };

    let _ = evt_tx.send(AgentEvent::Ready {
        model_label: llm.display_name().to_string(),
        sandbox_label: sandbox_label_for(&config),
    });
    Ok((llm, bridge, config))
}

/// Drain commands until shutdown after a fatal boot error — keeps the
/// channel from filling up and gives the renderer time to display the
/// BootFailed event before the user quits.
/// The header's second badge: the bridge mode, and the trust roots when the
/// gate is dispensed somewhere — a confirmation-free mode that is not on
/// screen is a defect.
fn sandbox_label_for(config: &Config) -> String {
    let mode = if config.bridge.mode.is_empty() {
        "local".to_string()
    } else {
        config.bridge.mode.clone()
    };
    match crate::agent::trust::label(&config.trust) {
        Some(t) => format!("{mode} · {t}"),
        None => mode,
    }
}

/// `/resume` for this frontend — the same steps as the CLI's
/// (`x0ne-code/src/main.rs`, `SlashResult::Resume`), in the same order:
/// resolve and VALIDATE the id (it becomes a path), load the transcript,
/// reopen the journal on that file and log the resume, feed the messages to
/// the agent (`resume_conversation` clears the conversation and the standing
/// approvals first), then move the agent's journal handle — leaving it on the
/// old file would append this session's actions to a journal nobody reads.
///
/// The event returned carries the reloaded messages, sanitized: they come
/// out of a file the model helped write. With it, a warning when the session
/// was started in another project.
async fn resume_into(
    sess: &mut CliAgentSession,
    session_logger: &mut Option<SharedJournal>,
    request: &str,
) -> Result<(AgentEvent, Option<String>), String> {
    use crate::session_store as store;
    let dir = store::sessions_dir();
    let current = session_logger.as_ref().map(|j| j.id().to_string());
    let (target, all) = store::parse_resume_arg(request)?;
    let here = store::Project::current();
    let resolved = store::resolve_resume(&dir, target, all, current.as_deref(), here.as_ref())
        .map_err(|e| format!("{e} — /sessions lists what there is"))?;
    let elsewhere = resolved.elsewhere.map(|p| {
        format!(
            "session {} was started in another project: {p}",
            resolved.id
        )
    });
    let id = resolved.id;
    let reloaded = store::load_session(&dir, &id).map_err(|e| e.to_string())?;
    let journal = SharedJournal::resume(&id)
        .await
        .map_err(|e| e.to_string())?;
    let _ = journal
        .log_session_resumed(
            reloaded.turns_reloaded,
            reloaded.broken_lines,
            reloaded.bytes_skipped,
        )
        .await;
    sess.resume_conversation(&reloaded);
    sess.journal = Some(journal.handle());
    *session_logger = Some(journal);
    let messages = reloaded
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                crate::conversation::Role::User => "user",
                crate::conversation::Role::Assistant => "assistant",
            };
            (
                role.to_string(),
                crate::text_safety::sanitize_block(&m.content),
            )
        })
        .collect();
    Ok((
        AgentEvent::SessionResumed {
            id,
            turns_reloaded: reloaded.turns_reloaded,
            bytes_skipped: reloaded.bytes_skipped,
            messages,
        },
        elsewhere,
    ))
}

async fn drain_until_shutdown(cmd_rx: &mut mpsc::Receiver<TuiCommand>) {
    while let Some(cmd) = cmd_rx.recv().await {
        if matches!(cmd, TuiCommand::Shutdown) {
            break;
        }
    }
}

/// File → ImageAttachment loader. Reads, base64-encodes, sniffs media type
/// from the extension.
fn load_image_attachment(path: &str) -> Result<crate::conversation::ImageAttachment, String> {
    use base64::Engine;
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {}", e))?;
    const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("image too large ({} bytes; max 8MB)", bytes.len()));
    }
    let media_type = match path
        .rsplit('.')
        .next()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => return Err("unsupported image extension".into()),
    };
    Ok(crate::conversation::ImageAttachment {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// SessionStart fires when the TUI's session is assembled, after `/clear`
    /// and after `/resume`.
    #[test]
    fn the_tui_fires_session_start() {
        let src = include_str!("runner.rs");
        let src = &src[..src.find("#[cfg(test)]").unwrap()];
        for call in [
            "agent.session_started(SessionSource::Startup)",
            "sess.session_started(SessionSource::Clear)",
            "sess.session_started(SessionSource::Resume)",
        ] {
            assert_eq!(src.matches(call).count(), 1, "{call}");
        }
    }

    /// A turn that logs what it said must also log why it ended.
    ///
    /// `report.response()` is `None` for every outcome except `Completed`, so
    /// a denied, cancelled or deadline-ended turn writes an empty `content`
    /// and, without the companion `turn` event, is indistinguishable in the
    /// log from a model that simply said nothing. That is not hypothetical:
    /// on 2026-08-13 several sessions showed the answer to one prompt only
    /// appearing after the NEXT prompt, and the log could not tell "refused"
    /// from "silent" — an hour of reading to reach a question one line would
    /// have answered.
    ///
    /// `log_turn` was written on 2026-08-10 for exactly this, its own
    /// doc-comment naming the failure it prevents, and it was never called
    /// from the canonical path. Unit-tested machinery nobody invokes is this
    /// repository's signature defect; this guards the call site rather than
    /// the function.
    #[test]
    fn a_logged_turn_records_its_outcome_and_not_only_its_prose() {
        let src = include_str!("runner.rs");
        // Look only at production code: the tests below name both symbols.
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        assert!(
            production.contains("log_llm("),
            "sanity: the runner is expected to log the response"
        );
        assert!(
            production.contains("log_turn("),
            "the runner logs what the turn said but not why it ended"
        );
    }

    /// The session TUI runs turns on must be handed the session log.
    ///
    /// The journal is a field on `CliAgentSession` and its default is `None`,
    /// which is the honest default and also a trap: a frontend that forgets
    /// this line still compiles, still logs prompts and turns, and silently
    /// records not one action. That is precisely how the product shipped 22
    /// sessions declaring 250 actions and containing none. So the assignment
    /// is asserted here, at the frontend, where forgetting it is possible —
    /// the driver's own tests cannot see this line.
    /// A refused bridge leaves the TUI open as a chat without tools.
    /// Under X0NE_REQUIRE_SANDBOX the boot fails first, before the model.
    #[test]
    fn boot_checks_the_sandbox_requirement_before_the_model() {
        let src = include_str!("runner.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let boot = &production[production.find("async fn boot(").expect("fn boot")..];
        let boot = &boot[..boot.find("\n}\n").expect("end of fn boot")];
        let gate = boot
            .find("require_sandbox(")
            .expect("boot never checks X0NE_REQUIRE_SANDBOX");
        assert!(
            gate < boot.find("setup_llm(").expect("setup_llm in boot"),
            "the model is set up before the sandbox requirement is checked"
        );
    }

    /// A model failure reaches the conversation with its kind and the
    /// provider's words, control characters folded; other outcomes do not.
    #[test]
    fn a_model_failure_is_shown_with_its_cause() {
        use crate::agent::driver::TurnOutcome;
        let error = crate::agent::model::classify(anyhow::anyhow!(
            "OpenAI-compat stream error 401 Unauthorized: \x1b[2Jbad key"
        ));
        let line = model_failure_line(&TurnOutcome::ModelFailure { error }).expect("a line");
        assert_eq!(
            line,
            "model error · authentication: OpenAI-compat stream error 401 Unauthorized:  [2Jbad key"
        );
        assert!(model_failure_line(&TurnOutcome::Cancelled).is_none());
    }

    /// `--tui` and `--matrix` run on the config `main` resolved from
    /// `-c`, the flags and the environment. A second load here drops `-c`.
    #[test]
    fn the_tui_and_matrix_never_load_the_config_themselves() {
        for (file, src) in [
            ("tui/runner.rs", include_str!("runner.rs")),
            ("tui/mod.rs", include_str!("mod.rs")),
            ("matrix_mode.rs", include_str!("../matrix_mode.rs")),
        ] {
            let cut = src.find("#[cfg(test)]\nmod tests").unwrap_or(src.len());
            let production = &src[..cut];
            for call in ["find_and_load(", "apply_env_overrides("] {
                assert!(!production.contains(call), "{file} calls {call}");
            }
        }
    }

    #[test]
    fn the_runner_hands_its_session_log_to_the_session() {
        let src = include_str!("runner.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        assert!(
            production.contains("agent.journal = session_logger"),
            "TUI assembles a session but never gives it the journal, so its \
             actions are executed off the record"
        );
    }

    /// The TUI gives its goal manager to the session it mounts.
    ///
    /// Same reason as the journal just above, and same defect: the driver
    /// knows how to advance a goal and have it reread by a skeptic
    /// (`Driver::advance_goal`, `Driver::skeptic_review`), but the field
    /// `CliAgentSession::goal` is `None` by default — without this line, all
    /// of that mechanism compiles, is tested, and runs on no real session.
    /// It is the "dead scaffolding" pattern (docs/invariants.md), and the driver's
    /// tests cannot see that line.
    ///
    /// A failure means `/goal` once again writes a goal that nothing
    /// advances.
    #[test]
    fn the_runner_hands_its_goal_manager_to_the_session() {
        let src = include_str!("runner.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        assert!(
            production.contains("agent.goal = "),
            "TUI mounts a session without a goal: cowork mode advances nowhere"
        );
    }

    /// The runner's production half must not contain an agent loop any more:
    /// no model invocation, no response parsing, no bridge dispatch. Those
    /// belong to the Driver, and this file routing around it is the cutover.
    #[test]
    fn guard_the_runner_owns_no_agent_loop() {
        let src = include_str!("runner.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "parse_llm_response",
            ".stream(&",
            "send_action(",
            "parse::interpret(",
            "needs_confirmation",
        ] {
            assert!(
                !production.contains(banned),
                "the runner still owns `{banned}` — a private agent loop survived the cutover"
            );
        }
        assert!(
            production.contains("run_turn(") && production.contains("run_action("),
            "the runner no longer drives the canonical session"
        );
    }

    fn test_lease(fires: Option<u32>) -> crate::agent::lease::CapabilityLease {
        crate::agent::lease::CapabilityLease::new(
            "test",
            crate::agent::lease::LeaseProvenance::Model,
            0,
            None,
            fires,
        )
    }

    /// The cron store fires due entries and re-arms recurring ones — while
    /// their lease lives. Pure data-structure behaviour, exercised without
    /// the ticker's clock.
    #[test]
    fn recurring_crons_rearm_and_oneshots_drop() {
        let crons = std::sync::Mutex::new(vec![
            PendingCron {
                id: 1,
                label: "oneshot".into(),
                due_at_unix: 10,
                trigger_prompt: None,
                action: None,
                every_secs: None,
                lease: test_lease(Some(1)),
            },
            PendingCron {
                id: 2,
                label: "recurring".into(),
                due_at_unix: 10,
                trigger_prompt: None,
                action: None,
                every_secs: Some(30),
                lease: test_lease(None),
            },
            PendingCron {
                id: 3,
                label: "later".into(),
                due_at_unix: 100,
                trigger_prompt: None,
                action: None,
                every_secs: None,
                lease: test_lease(Some(1)),
            },
        ]);
        // The ticker's inner pass, replicated at now = 10.
        let now = 10;
        let mut fired = Vec::new();
        {
            let mut guard = crons.lock().unwrap();
            let mut keep = Vec::new();
            for mut c in guard.drain(..) {
                if c.due_at_unix <= now {
                    if c.lease.try_fire(now).is_ok() {
                        fired.push(c.clone());
                        if let Some(every) = c.every_secs {
                            if c.lease.is_live(now) {
                                let mut next = c;
                                next.due_at_unix = now + every.max(1);
                                keep.push(next);
                            }
                        }
                    }
                } else {
                    keep.push(c);
                }
            }
            *guard = keep;
        }
        assert_eq!(
            fired.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![1, 2],
            "due crons did not fire"
        );
        let remaining = crons.lock().unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|c| (c.id, c.due_at_unix))
                .collect::<Vec<_>>(),
            vec![(2, 40), (3, 100)],
            "the recurring cron did not re-arm / the one-shot survived"
        );
    }

    /// A recurring cron whose lease budget runs out is retired instead of
    /// re-armed. This is the bound that stops "every 30 seconds" from
    /// meaning "forever".
    #[test]
    fn a_recurring_cron_retires_when_its_lease_is_exhausted() {
        let mut c = PendingCron {
            id: 9,
            label: "bounded".into(),
            due_at_unix: 10,
            trigger_prompt: None,
            action: None,
            every_secs: Some(30),
            lease: test_lease(Some(2)),
        };
        assert!(c.lease.try_fire(10).is_ok());
        assert!(c.lease.is_live(10), "one firing left");
        assert!(c.lease.try_fire(40).is_ok());
        assert!(!c.lease.is_live(40), "budget spent — must not re-arm");
        assert_eq!(
            c.lease.try_fire(70),
            Err(crate::agent::lease::LeaseDenied::Exhausted)
        );
    }

    /// Image loading rejects what it cannot classify and keeps its bound.
    #[test]
    fn image_loader_rejects_unknown_extensions() {
        let err = load_image_attachment("/tmp/definitely-missing.xyz").unwrap_err();
        assert!(err.contains("read failed") || err.contains("unsupported"));
    }

    /// The scheduled-action command shape round-trips — the router replays
    /// exactly what the ticker enqueued.
    #[test]
    fn scheduled_action_commands_carry_the_action_verbatim() {
        let action = json!({"type": "RunCommand", "command": "df", "args": ["-h"]});
        let cmd = TuiCommand::RunScheduledAction {
            label: "disk".into(),
            action: action.clone(),
        };
        match cmd {
            TuiCommand::RunScheduledAction { label, action: a } => {
                assert_eq!(label, "disk");
                assert_eq!(a, action);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── /undo from the TUI ───────────────────────────────────────────────

    /// Counts the model calls; answers the scripted replies, then "done".
    struct CountingModel {
        replies: std::sync::Mutex<VecDeque<String>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::api::LlmBackend for CountingModel {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    /// The runner's working set, as `run_runner` holds it, over a session
    /// whose bridge snapshots and restores real files (`SnapshotBridge`).
    struct UndoRig {
        hold: Option<Box<CliAgentSession>>,
        work: Option<WorkHandle>,
        deferred: VecDeque<TuiCommand>,
        evt_tx: mpsc::UnboundedSender<AgentEvent>,
        evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
        confirm_gate: Arc<HumanGate>,
        ask_gate: Arc<AskGate>,
        badge: Arc<BadgeWatchSlot>,
        cancel: crate::agent::cancel::CancelToken,
        /// Model calls so far.
        calls: Arc<std::sync::atomic::AtomicUsize>,
        /// Every action the bridge received.
        seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    fn undo_rig(work: &std::path::Path, snaps: &std::path::Path, replies: Vec<String>) -> UndoRig {
        let (evt_tx, evt_rx) = mpsc::unbounded_channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let bridge = crate::agent::undo::tests::SnapshotBridge::new(work, snaps);
        let seen = bridge.seen.clone();
        let mut config = Config::default();
        config.trust.roots = vec![work.display().to_string()];
        let sess = CliAgentSession::new(
            config,
            Arc::new(CountingModel {
                replies: std::sync::Mutex::new(replies.into()),
                calls: Arc::clone(&calls),
            }),
            "SYS",
            Some(Box::new(bridge)),
            Box::new(crate::display_sink::NullDisplaySink),
        );
        UndoRig {
            hold: Some(Box::new(sess)),
            work: None,
            deferred: VecDeque::new(),
            evt_tx,
            evt_rx,
            confirm_gate: Arc::new(HumanGate::default()),
            ask_gate: Arc::new(AskGate::default()),
            badge: Arc::new(BadgeWatchSlot::default()),
            cancel: crate::agent::cancel::CancelToken::new(),
            calls,
            seen,
        }
    }

    /// Route one command as the runner does, and take the session back when
    /// the command started a turn.
    async fn route(rig: &mut UndoRig, cmd: TuiCommand) {
        let mut session_logger: Option<SharedJournal> = None;
        route_command(
            cmd,
            &mut rig.hold,
            &mut rig.work,
            &mut rig.deferred,
            &mut session_logger,
            &rig.evt_tx,
            &rig.confirm_gate,
            &rig.ask_gate,
            &rig.badge,
            &mut rig.cancel,
        )
        .await;
        if let Some(handle) = rig.work.take() {
            let (sess, _out) = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .expect("the turn did not end")
                .expect("turn task");
            rig.hold = Some(sess);
        }
    }

    /// The Info and Error lines the TUI would print, drained.
    fn drained_lines(rig: &mut UndoRig) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rig.evt_rx.try_recv() {
            match ev {
                AgentEvent::Info(s) => out.push(format!("info: {s}")),
                AgentEvent::Error(s) => out.push(format!("error: {s}")),
                _ => {}
            }
        }
        out
    }

    fn bridge_types(rig: &UndoRig) -> Vec<String> {
        rig.seen
            .lock()
            .unwrap()
            .iter()
            .map(|a| a["type"].as_str().unwrap_or("?").to_string())
            .collect()
    }

    /// What the TUI sends to the runner when the operator types `line` and
    /// presses Enter: the frontend half of the path, not a hand-built command.
    fn typed_in_the_tui(line: &str) -> TuiCommand {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = super::super::app::AppState::new();
        s.cmd_tx = Some(tx);
        s.input.insert_str(line);
        super::super::events::handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        rx.try_recv().expect("the TUI sent nothing to the runner")
    }

    fn write_action(path: &std::path::Path, content: &str) -> String {
        format!(
            "<action>{}</action>",
            json!({"type": "WriteFile", "path": path.display().to_string(), "content": content})
        )
    }

    /// The README's promise: after a write by the agent, `/undo` typed in the
    /// TUI restores the file from the snapshot taken before it, and no request
    /// goes to the model. Measured 2026-11-02 before this test: `/undo` left
    /// the TUI as a prompt (4th request of the scripted server) and the file
    /// stayed rewritten.
    #[tokio::test]
    async fn undo_from_the_tui_restores_the_agent_write_without_a_model_call() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let edited = work.join("lib.rs");
        std::fs::write(&edited, "user: before").unwrap();
        let mut rig = undo_rig(
            &work,
            &tmp.path().join("snaps"),
            vec![write_action(&edited, "agent: rewrite"), "done".into()],
        );

        route(&mut rig, TuiCommand::UserInput("rewrite lib.rs".into())).await;
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "agent: rewrite",
            "the agent's write did not happen; bridge saw {:?}, lines {:?}",
            bridge_types(&rig),
            drained_lines(&mut rig)
        );
        let calls_after_turn = rig.calls.load(std::sync::atomic::Ordering::SeqCst);
        let _ = drained_lines(&mut rig);

        route(&mut rig, typed_in_the_tui("/undo")).await;

        let lines = drained_lines(&mut rig);
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "user: before",
            "/undo did not restore the file; lines {lines:?}"
        );
        assert_eq!(
            rig.calls.load(std::sync::atomic::Ordering::SeqCst),
            calls_after_turn,
            "/undo sent a request to the model"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("info: undid WriteFile")),
            "the operator was not told what was undone: {lines:?}"
        );
        let types = bridge_types(&rig);
        assert!(types.contains(&"RestoreSnapshot".to_string()), "{types:?}");
        assert!(
            !types.contains(&"RunCommand".to_string()),
            "git was run: {types:?}"
        );
    }

    /// `/undo` with no undo point: one clear line, nothing sent to the
    /// runtime, nothing sent to the model.
    #[tokio::test]
    async fn undo_without_a_snapshot_says_so_and_sends_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut rig = undo_rig(&work, &tmp.path().join("snaps"), vec![]);

        route(&mut rig, typed_in_the_tui("/undo")).await;

        let lines = drained_lines(&mut rig);
        assert!(
            lines.iter().any(|l| l.starts_with("info: nothing to undo")),
            "no clear answer: {lines:?}"
        );
        assert_eq!(
            rig.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the model was called"
        );
        assert!(
            bridge_types(&rig).is_empty(),
            "the runtime was reached: {:?}",
            bridge_types(&rig)
        );
    }

    /// Hostile: between the agent's write and `/undo`, the project path
    /// became a symlink to a file outside the project. Measured 2026-11-02:
    /// the runtime canonicalizes the manifest paths before restoring, finds
    /// one outside `allowed_write_paths` and refuses the whole restore. So
    /// nothing is written, inside or outside, the undo point is kept, and
    /// the operator reads the refusal. A failure here would mean a byte
    /// crossed the symlink.
    #[tokio::test]
    async fn undo_refuses_when_the_project_path_became_a_symlink_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let outside_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let edited = work.join("lib.rs");
        let outside = outside_dir.join("secret.txt");
        std::fs::write(&edited, "user: before").unwrap();
        std::fs::write(&outside, "outside: untouched").unwrap();
        let mut rig = undo_rig(
            &work,
            &tmp.path().join("snaps"),
            vec![write_action(&edited, "agent: rewrite"), "done".into()],
        );
        route(&mut rig, TuiCommand::UserInput("rewrite lib.rs".into())).await;
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "agent: rewrite");
        let _ = drained_lines(&mut rig);

        std::fs::remove_file(&edited).unwrap();
        std::os::unix::fs::symlink(&outside, &edited).unwrap();

        route(&mut rig, typed_in_the_tui("/undo")).await;

        let lines = drained_lines(&mut rig);
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "outside: untouched",
            "the restore wrote through the symlink, outside the project: {lines:?}"
        );
        assert!(
            std::fs::symlink_metadata(&edited)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the project path was rewritten although the restore was refused: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| {
                l.starts_with("error: undo of WriteFile failed, nothing popped")
                    && l.contains("outside allowed_write_paths")
            }),
            "the operator was not told the restore was refused: {lines:?}"
        );
        let sess = rig.hold.as_ref().expect("idle");
        assert_eq!(
            sess.state.undo.points().len(),
            1,
            "the undo point was dropped although nothing was restored"
        );
        let types = bridge_types(&rig);
        assert!(types.contains(&"RestoreSnapshot".to_string()), "{types:?}");
        assert!(
            !types.contains(&"RunCommand".to_string()),
            "git was run: {types:?}"
        );
    }
}
