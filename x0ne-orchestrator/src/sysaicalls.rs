//! Orchestrator-side sysaicalls: Plan, WaitAgent, ListAgents, BroadcastAgents, Reflect.
//!
//! These are NOT sent to x0ne-runtime — they compose existing bridge actions.

use anyhow::Result;
use colored::Colorize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Monotonic counter mixed into broadcast message ids so two broadcasts
/// fired in the same nanosecond still produce distinct ids.
static BROADCAST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Plan ───────────────────────────────────────────────────────────────────────

/// Decision outcome for a Plan approval gate.
///
/// `decision_source` is the audit trail: one of
///   "env_auto_approve" | "env_explicit_reject" |
///   "stdin_yes" | "tty_enter_default" | "tty_rejected" |
///   "rejected_subagent" | "rejected_non_interactive_eof" |
///   "rejected_piped_empty_line" | "stdin_io_error" |
///   "rejected_serve_mode"
///
/// JEI/CIR auditors must be able to grep for these and prove every Plan was
/// resolved through an explicit human or env decision — never by accident.
#[derive(Debug, Clone)]
pub struct PlanDecision {
    pub approved: bool,
    pub decision_source: &'static str,
    /// Set when `approved == false` AND the rejection is a fail-closed safety
    /// gate (non-interactive context with no env override). `None` for a
    /// legitimate human "no".
    pub error: Option<PlanError>,
}

#[derive(Debug, Clone)]
pub struct PlanError {
    pub class: &'static str,
    pub code: &'static str,
    pub message: String,
    pub recommended_next_step: String,
}

impl PlanError {
    pub fn requires_human(decision_source: &'static str) -> Self {
        let context_hint = match decision_source {
            "rejected_subagent" =>
                "A sub-agent emitted a Plan. Plans must be approved by the top-level human operator; \
                 redesign the sub-agent task to use Try directly, or surface the plan back to the parent.",
            "rejected_serve_mode" =>
                "HTTP serve mode has no human at the keyboard. Either approve via X0NE_PLAN_AUTO_APPROVE \
                 in the server env, or replace Plan with Try in the calling agent.",
            _ =>
                "Retry from an interactive session (real TTY), or set X0NE_PLAN_AUTO_APPROVE=y for \
                 automated contexts. Empty piped stdin is treated as 'no answer', not 'yes'.",
        };
        Self {
            class: "validation_error",
            code: "plan_requires_human",
            message: "Plan requires explicit human approval, but no interactive stdin or env override is available.".to_string(),
            recommended_next_step: context_hint.to_string(),
        }
    }
}

/// Read source: how `decide_plan_approval` learned the answer.
/// Carried through to the printed result and the session JSONL.
fn print_plan_summary(action: &Value) {
    // SECURITY (audit 2026-08-15, finding 4): this summary is the Plan gate's
    // manifest — `display_plan_and_confirm` prints it and the very next
    // statement is `Execute this plan? [y/n/e]`. Label, step types and the
    // step's path/command are all model-authored, so a `\n` here plants a
    // step the plan does not contain and `\r\x1b[2K` repaints one it does.
    use crate::text_safety::sanitize;
    let label = sanitize(action["label"].as_str().unwrap_or("proposed plan"));
    let steps = action["steps"].as_array().cloned().unwrap_or_default();

    println!(
        "\n  {} {} {}",
        "plan:".bright_cyan().bold(),
        label.bold(),
        format!("({} steps)", steps.len()).dimmed()
    );
    let max_visible = 5;
    let shown = steps.len().min(max_visible);
    for (i, step) in steps.iter().enumerate().take(shown) {
        let t = step["type"].as_str().unwrap_or("?");
        // Classified from the exact value, printed sanitized.
        let cat = crate::display::action_category_name(t);
        let t = sanitize(t);
        let path = sanitize(
            step["path"]
                .as_str()
                .or_else(|| step["command"].as_str())
                .or_else(|| step["root"].as_str())
                .unwrap_or(""),
        );
        let short_path = path.rsplit('/').next().unwrap_or(&path);
        println!(
            "  {}  {}  {}  {}",
            format!("{:>2}.", i + 1).dimmed(),
            cat.dimmed(),
            t.bright_cyan().bold(),
            short_path.dimmed()
        );
    }
    if steps.len() > max_visible {
        println!(
            "  {}  +{} more…",
            "   ".dimmed(),
            (steps.len() - max_visible).to_string().dimmed()
        );
    }
}

/// Pure decision logic, factored out for unit testing.
///
/// Heuristic: "did we receive a character?" (per T0.5 spec), not "is stdin a TTY?".
/// This handles `echo "y" | x0ne` (piped intent) the same as a TTY 'y' press,
/// while still rejecting `x0ne < /dev/null` (EOF before any byte) as
/// `plan_requires_human`. The TTY check is only consulted to determine whether
/// a bare Enter (`""`) means "default yes" or "suspicious empty line".
pub fn decide_plan_approval_inner(
    is_subagent: bool,
    env_value: Option<&str>,
    stdin_read: std::io::Result<(usize, String)>,
    stdin_is_tty: bool,
) -> PlanDecision {
    // 1. Env override wins, regardless of context.
    if let Some(v) = env_value {
        if !v.is_empty() {
            let trimmed = v.trim().to_lowercase();
            if matches!(trimmed.as_str(), "y" | "yes" | "1") {
                return PlanDecision {
                    approved: true,
                    decision_source: "env_auto_approve",
                    error: None,
                };
            }
            return PlanDecision {
                approved: false,
                decision_source: "env_explicit_reject",
                error: None,
            };
        }
    }
    // 2. Sub-agent: never prompt. Stdin is shared with the parent process,
    //    so reading would race the parent's TTY and the user would not know
    //    which agent's Plan they're approving.
    if is_subagent {
        return PlanDecision {
            approved: false,
            decision_source: "rejected_subagent",
            error: Some(PlanError::requires_human("rejected_subagent")),
        };
    }
    // 3. Stdin attempt.
    match stdin_read {
        Ok((0, _)) => PlanDecision {
            approved: false,
            decision_source: "rejected_non_interactive_eof",
            error: Some(PlanError::requires_human("rejected_non_interactive_eof")),
        },
        Ok((_, buf)) => {
            let trimmed = buf.trim().to_lowercase();
            if matches!(trimmed.as_str(), "y" | "yes") {
                PlanDecision {
                    approved: true,
                    decision_source: "stdin_yes",
                    error: None,
                }
            } else if trimmed.is_empty() {
                if stdin_is_tty {
                    PlanDecision {
                        approved: true,
                        decision_source: "tty_enter_default",
                        error: None,
                    }
                } else {
                    PlanDecision {
                        approved: false,
                        decision_source: "rejected_piped_empty_line",
                        error: Some(PlanError::requires_human("rejected_piped_empty_line")),
                    }
                }
            } else {
                PlanDecision {
                    approved: false,
                    decision_source: "tty_rejected",
                    error: None,
                }
            }
        }
        Err(_) => PlanDecision {
            approved: false,
            decision_source: "stdin_io_error",
            error: Some(PlanError::requires_human("stdin_io_error")),
        },
    }
}

/// The plan answer source: the operator's `X0NE_PLAN_AUTO_APPROVE` if set,
/// else "y" in sandbox yolo (2026-10-06).
pub(crate) fn plan_env(env: Option<&str>, yolo: bool) -> Option<String> {
    match env.filter(|v| !v.is_empty()) {
        Some(v) => Some(v.to_string()),
        None if yolo => Some("y".into()),
        None => None,
    }
}

/// Live wrapper around `decide_plan_approval_inner` that performs the actual
/// stdin read and TTY probe. Kept separate so tests can drive every branch
/// without monkeypatching stdin.
fn decide_plan_approval(is_subagent: bool) -> PlanDecision {
    use std::io::IsTerminal;
    let env_value = plan_env(
        std::env::var("X0NE_PLAN_AUTO_APPROVE").ok().as_deref(),
        crate::sandbox_mode::current().yolo(),
    );
    if env_value.as_deref().map(|s| !s.is_empty()).unwrap_or(false) || is_subagent {
        // Skip the stdin read entirely on the env / sub-agent paths to avoid
        // blocking on a TTY when the answer is already known.
        return decide_plan_approval_inner(
            is_subagent,
            env_value.as_deref(),
            Ok((0, String::new())),
            false,
        );
    }
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut input = String::new();
    let read = std::io::stdin().read_line(&mut input).map(|n| (n, input));
    decide_plan_approval_inner(is_subagent, env_value.as_deref(), read, stdin_is_tty)
}

/// Loud one-time banner. Call once at session start (from main / serve init)
/// when `X0NE_PLAN_AUTO_APPROVE` is set in the environment, regardless of
/// whether any Plan is ever emitted. Auditors get a single explicit line in
/// the session header.
pub fn announce_plan_auto_approve_if_set() {
    use std::sync::OnceLock;
    static ANNOUNCED: OnceLock<()> = OnceLock::new();
    if ANNOUNCED.set(()).is_err() {
        return;
    }
    let v = match std::env::var("X0NE_PLAN_AUTO_APPROVE") {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    eprintln!(
        "{} {}",
        "⚠".yellow().bold(),
        format!(
            "X0NE_PLAN_AUTO_APPROVE={v} is set — Plans will be resolved \
             without TTY confirmation. Unset for production sessions."
        )
        .yellow()
    );
}

/// Convert a `PlanDecision` into the canonical action-result JSON consumed by
/// session log + conversation feedback. The `decision_source` is always
/// surfaced so a JSONL grep proves how each Plan was resolved.
pub fn plan_decision_to_result_json(action: &Value, decision: &PlanDecision) -> Value {
    let label = action["label"].as_str().unwrap_or("");
    if let Some(err) = &decision.error {
        // Fail-closed rejection: structured error.
        return json!({
            "success": false,
            "action_type": "Plan",
            "output": {
                "kind": "plan_result",
                "approved": false,
                "label": label,
                "decision_source": decision.decision_source,
            },
            "error": {
                "kind": "plan_error",
                "class": err.class,
                "code": err.code,
                "retryable": false,
                "message": err.message,
                "recommended_next_step": err.recommended_next_step,
                "decision_source": decision.decision_source,
            }
        });
    }
    json!({
        "success": decision.approved,
        "action_type": "Plan",
        "output": {
            "kind": "plan_result",
            "approved": decision.approved,
            "label": label,
            "decision_source": decision.decision_source,
        }
    })
}

/// Display a plan and resolve approval. Returns the structured `PlanDecision`
/// so callers can render it to JSON, log it, and (when approved) execute the
/// underlying steps. Callers must pass `is_subagent: true` for any path
/// invoked from a spawned sub-agent loop — this enforces fail-closed
/// behavior on the shared-stdin race.
pub async fn display_plan_and_confirm(
    bridge: &mut Box<dyn crate::bridge::Bridge>,
    _config: &crate::config::Config,
    action: &Value,
    conv: &mut crate::conversation::Conversation,
    is_subagent: bool,
) -> Result<PlanDecision> {
    print_plan_summary(action);
    print!("  {} [y/n/e] ", "Execute this plan?".bold());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let decision = decide_plan_approval(is_subagent);

    // Visual feedback so the operator (or log reader) knows which path fired.
    let suffix = match decision.decision_source {
        "env_auto_approve" => "y  (env auto)".dimmed().to_string(),
        "env_explicit_reject" => "n  (env reject)".dimmed().to_string(),
        "stdin_yes" => "y".dimmed().to_string(),
        "tty_enter_default" => "y  (default)".dimmed().to_string(),
        "tty_rejected" => "n".dimmed().to_string(),
        "rejected_subagent" => "rejected (sub-agent — Plans require human)"
            .red()
            .to_string(),
        "rejected_non_interactive_eof" => "rejected (no stdin — Plans require human)"
            .red()
            .to_string(),
        "rejected_piped_empty_line" => "rejected (empty piped line — ambiguous)".red().to_string(),
        "stdin_io_error" => "rejected (stdin error)".red().to_string(),
        other => other.dimmed().to_string(),
    };
    println!("{}", suffix);

    let label = action["label"].as_str().unwrap_or("proposed plan");
    let steps = action["steps"].as_array().cloned().unwrap_or_default();

    if decision.approved {
        let try_action = json!({
            "type": "Try",
            "actions": steps,
            "rollback_on_fail": true,
            "label": label
        });
        let try_result = bridge.send_action(&try_action).await?;
        let result_ok = try_result["output"]["ok"].as_bool().unwrap_or(false);
        let result_completed = try_result["output"]["completed"].as_u64().unwrap_or(0) as usize;
        let result_total = try_result["output"]["total"].as_u64().unwrap_or(0) as usize;
        let result_rolled_back = try_result["output"]["rolled_back"]
            .as_bool()
            .unwrap_or(false);
        let result_json = json!({
            "ok": result_ok,
            "completed": result_completed,
            "total": result_total,
            "rolled_back": result_rolled_back,
            "label": label,
            "decision_source": decision.decision_source,
        });
        let msg = if result_ok {
            format!(
                "Plan '{}' completed successfully ({}/{} steps).",
                label, result_completed, result_total
            )
        } else {
            format!(
                "Plan '{}' failed at step {}. {}",
                label,
                result_completed + 1,
                if result_rolled_back {
                    "Changes rolled back."
                } else {
                    ""
                }
            )
        };
        conv.add_user(&format!("[System] Plan result: {}\n{}", msg, result_json));
        Ok(PlanDecision {
            approved: result_ok,
            decision_source: decision.decision_source,
            error: None,
        })
    } else if let Some(err) = &decision.error {
        // Fail-closed rejection: feed the model the same structured guidance
        // the JSONL records, so it can recover (e.g., emit Try directly).
        println!("  {} {}", "✗".red(), err.message);
        conv.add_user(&format!(
            "[System] Plan rejected ({}): {} {}",
            decision.decision_source, err.message, err.recommended_next_step,
        ));
        Ok(decision)
    } else {
        // Genuine human "no" (or env explicit reject).
        println!("  {} plan rejected", "✗".red());
        conv.add_user(&format!(
            "[System] Plan rejected by {}. Ask what to do differently.",
            decision.decision_source,
        ));
        Ok(decision)
    }
}

// ── Multi-Agent System ─────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// Status of a spawned agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Running,
    Completed,
    Failed(String),
}

/// Result from a completed agent.
///
/// T4: `actions_executed` is now the **real** action count taken from the
/// sub-agent's `Conversation::action_stats`, not an output-line proxy.
/// `actions_breakdown` carries the JSON-serialised `ActionStats`
/// (total / succeeded / failed / by_type), and `duration_ms` is frozen
/// at completion time so subsequent reads always return the same value.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentResult {
    pub id: String,
    pub task: String,
    pub response: String,
    pub tokens_used: u64,
    pub actions_executed: usize,
    pub actions_breakdown: Value,
    pub success: bool,
    pub duration_ms: u64,
}

impl AgentResult {
    /// Empty breakdown suitable for the early-exit / failure cases where
    /// no sub-agent loop ran (e.g. bridge setup error). Keeps the JSON
    /// shape stable so `WaitAgent` consumers never have to defend
    /// against a missing `actions_breakdown` key.
    pub fn empty_breakdown() -> Value {
        serde_json::json!({
            "total": 0, "succeeded": 0, "failed": 0,
            "by_type": {},
        })
    }
}

/// One inter-agent broadcast as observed by a recipient (T3).
///
/// We track the message id so the parent that issued the broadcast can
/// query *who* received it and *who* explicitly acknowledged it. Both
/// timestamps are unix seconds (`now_ts()`); `None` means "not yet".
#[derive(Debug, Clone)]
pub struct BroadcastEntry {
    pub message_id: String,
    pub body: String,
    /// When `true`, the recipient must call `AcknowledgeBroadcast` to flag
    /// `acknowledged_at`. When `false`, the agent_loop auto-acks on drain
    /// (preserves the historical fire-and-forget semantics).
    pub require_ack: bool,
    pub delivered_at: u64,
    pub acknowledged_at: Option<u64>,
}

/// Internal entry in the registry.
struct AgentEntry {
    id: String,
    task: String,
    status: AgentStatus,
    result: Option<AgentResult>,
    /// Channel to receive the result when the agent completes.
    result_rx: Option<oneshot::Receiver<AgentResult>>,
    started_at: std::time::Instant,
    /// Inbox for inter-agent messages (e.g. from BroadcastAgents).
    /// Shared with the running agent's loop which drains it each turn.
    /// (T3) Each entry carries an id + ack metadata so the parent can
    /// reconstruct receipt history via `GetBroadcastReceipts`.
    inbox: Arc<Mutex<Vec<BroadcastEntry>>>,
}

/// Thread-safe registry of spawned agents.
pub struct AgentRegistry {
    agents: Arc<Mutex<HashMap<String, AgentEntry>>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of currently tracked agents.
    pub fn count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    /// Returns true if an agent with this id is already registered
    /// (regardless of whether it is Running, Completed, or Failed).
    ///
    /// Used by the serve-mode `SpawnWorld` dispatcher to reject duplicate
    /// ids before setting up a `tokio::spawn` + `oneshot` pair.
    pub fn contains_id(&self, id: &str) -> bool {
        self.agents.lock().unwrap().contains_key(id)
    }

    /// Insert a `Running` entry and return the oneshot sender the spawned
    /// task should use to deliver the final result.
    ///
    /// This is the production-side counterpart of `insert_pending_for_test`.
    /// It intentionally takes the same shape so that both code paths exercise
    /// the same `AgentEntry` layout.
    pub fn insert_pending(&self, id: &str, task: &str) -> oneshot::Sender<AgentResult> {
        let (tx, rx) = oneshot::channel::<AgentResult>();
        let mut agents = self.agents.lock().unwrap();
        agents.insert(
            id.to_string(),
            AgentEntry {
                id: id.to_string(),
                task: task.to_string(),
                status: AgentStatus::Running,
                result: None,
                result_rx: Some(rx),
                started_at: std::time::Instant::now(),
                inbox: Arc::new(Mutex::new(Vec::new())),
            },
        );
        tx
    }

    /// Insert a `Running` entry, or refuse because the id is taken.
    ///
    /// The duplicate check and the insertion happen under one lock. The
    /// historical `SpawnWorld` released the lock between the two
    /// (`sysaicalls.rs:1053-1080`), so two worlds racing on the same id could
    /// both pass the check and the second would overwrite the first's
    /// `result_rx` — leaving the first `WaitAgent` waiting on a channel nobody
    /// would ever send to. Deterministic rejection is the point of this
    /// method's existence.
    pub fn try_insert_pending(&self, id: &str, task: &str) -> Option<oneshot::Sender<AgentResult>> {
        let (tx, rx) = oneshot::channel::<AgentResult>();
        let mut agents = self.agents.lock().unwrap();
        if agents.contains_key(id) {
            return None;
        }
        agents.insert(
            id.to_string(),
            AgentEntry {
                id: id.to_string(),
                task: task.to_string(),
                status: AgentStatus::Running,
                result: None,
                result_rx: Some(rx),
                started_at: std::time::Instant::now(),
                inbox: Arc::new(Mutex::new(Vec::new())),
            },
        );
        Some(tx)
    }

    /// Insert a `Running` entry with no result channel, or refuse.
    ///
    /// The inline (`SpawnAgent`) shape: the caller fills `result` directly
    /// before returning, so there is no oneshot and a later `WaitAgent` takes
    /// the cached-result fast path. Historical behaviour, at
    /// `sysaicalls.rs:1368-1383`.
    pub fn try_insert_inline(&self, id: &str, task: &str) -> bool {
        let mut agents = self.agents.lock().unwrap();
        if agents.contains_key(id) {
            return false;
        }
        agents.insert(
            id.to_string(),
            AgentEntry {
                id: id.to_string(),
                task: task.to_string(),
                status: AgentStatus::Running,
                result: None,
                result_rx: None,
                started_at: std::time::Instant::now(),
                inbox: Arc::new(Mutex::new(Vec::new())),
            },
        );
        true
    }

    /// Mark an agent as finished with an explicit status note.
    ///
    /// `mark_completed` writes a fixed sentence; the historical lifecycle
    /// wrote the counts (`"{n} action(s) failed ({ok} ok / {total} total)"`)
    /// and a startup failure wrote the transport error. `ListAgents` shows
    /// this string, so it is the difference between "it failed" and "here is
    /// what failed".
    pub fn mark_finished(&self, id: &str, result: AgentResult, failure_note: Option<String>) {
        if let Ok(mut agents) = self.agents.lock() {
            if let Some(entry) = agents.get_mut(id) {
                entry.status = match failure_note {
                    None => AgentStatus::Completed,
                    Some(note) => AgentStatus::Failed(note),
                };
                entry.result = Some(result);
            }
        }
    }

    /// Ids of the agents still running, sorted.
    pub fn running_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .agents
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.status == AgentStatus::Running)
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// The recorded status of one agent, as a short word. Test and diagnostic
    /// surface: `ListAgents` is the product view.
    pub fn status_of(&self, id: &str) -> Option<&'static str> {
        self.agents.lock().unwrap().get(id).map(|e| match e.status {
            AgentStatus::Running => "running",
            AgentStatus::Completed => "completed",
            AgentStatus::Failed(_) => "failed",
        })
    }

    /// Get a clone of an agent's inbox handle (for passing to the agent's loop).
    pub fn get_inbox(&self, id: &str) -> Option<Arc<Mutex<Vec<BroadcastEntry>>>> {
        self.agents
            .lock()
            .unwrap()
            .get(id)
            .map(|e| Arc::clone(&e.inbox))
    }

    /// (T3) Build a JSON receipt summary for a single broadcast.
    /// Iterates every agent's inbox once; this is fine — the registry is
    /// expected to hold tens of agents, not millions, and we never call
    /// this from a hot loop.
    ///
    /// Returned shape:
    /// ```json
    /// {
    ///   "message_id": "bc_…",
    ///   "delivered_count": 2,
    ///   "acknowledged_count": 1,
    ///   "recipients": [
    ///     {"agent_id":"a","delivered_at":1700000000,"acknowledged_at":1700000005},
    ///     {"agent_id":"b","delivered_at":1700000000,"acknowledged_at":null}
    ///   ]
    /// }
    /// ```
    pub fn broadcast_receipts(&self, message_id: &str) -> Value {
        let agents = self.agents.lock().unwrap();
        let mut recipients: Vec<Value> = Vec::new();
        let mut delivered = 0usize;
        let mut acknowledged = 0usize;
        for entry in agents.values() {
            let inbox = entry.inbox.lock().unwrap();
            if let Some(b) = inbox.iter().find(|b| b.message_id == message_id) {
                delivered += 1;
                let ack = b.acknowledged_at;
                if ack.is_some() {
                    acknowledged += 1;
                }
                recipients.push(json!({
                    "agent_id": entry.id,
                    "delivered_at": b.delivered_at,
                    "acknowledged_at": ack,
                }));
            }
        }
        json!({
            "message_id": message_id,
            "delivered_count": delivered,
            "acknowledged_count": acknowledged,
            "recipients": recipients,
        })
    }

    /// Mark an agent as finished and cache its result so a later
    /// `WaitAgent` call can read it without blocking.
    ///
    /// `success = false` maps to `AgentStatus::Failed("completed with failure")`.
    /// If the id is unknown (the entry was evicted), the call is a no-op.
    pub fn mark_completed(&self, id: &str, result: AgentResult, success: bool) {
        if let Ok(mut agents) = self.agents.lock() {
            if let Some(entry) = agents.get_mut(id) {
                entry.status = if success {
                    AgentStatus::Completed
                } else {
                    AgentStatus::Failed("completed with failure".into())
                };
                entry.result = Some(result);
            }
        }
    }
}

/// WaitAgent — block until an agent completes, return its result.
pub async fn execute_wait_agent(registry: &AgentRegistry, action: &Value) -> Result<Value> {
    let id = action["id"].as_str().unwrap_or("").to_string();
    let timeout_secs = action["timeout_secs"].as_u64().unwrap_or(120);

    if id.is_empty() {
        return Ok(json!({"success": false, "error": {"message": "WaitAgent requires 'id'"}}));
    }

    // Check if already completed
    {
        let agents = registry.agents.lock().unwrap();
        if let Some(entry) = agents.get(&id) {
            if let Some(ref result) = entry.result {
                let r = result.clone();
                // Display handled by agent_loop.rs via result_line.
                // T4: include the per-type breakdown and the frozen
                // `elapsed_secs` (derived from the cached duration_ms,
                // never recomputed) so repeated WaitAgent reads are
                // idempotent in their numbers.
                return Ok(json!({
                    "success": r.success,
                    "output": {
                        "id": r.id,
                        "task": r.task,
                        "response": r.response,
                        "tokens_used": r.tokens_used,
                        "actions_executed": r.actions_executed,
                        "actions_breakdown": r.actions_breakdown,
                        "duration_ms": r.duration_ms,
                        "elapsed_secs": (r.duration_ms as f64) / 1000.0,
                    }
                }));
            }
        } else {
            return Ok(
                json!({"success": false, "error": {"message": format!("Agent '{}' not found", id)}}),
            );
        }
    }

    // Extract the receiver
    let rx = {
        let mut agents = registry.agents.lock().unwrap();
        agents.get_mut(&id).and_then(|e| e.result_rx.take())
    };

    if let Some(rx) = rx {
        // Wait with timeout
        let wait_spinner = indicatif::ProgressBar::new_spinner();
        wait_spinner.set_style(
            indicatif::ProgressStyle::default_spinner()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
                .template("{spinner:.cyan} {msg}")
                .expect("hardcoded"),
        );
        wait_spinner.set_message(format!("waiting for agent '{}'…", id));
        wait_spinner.enable_steady_tick(std::time::Duration::from_millis(100));

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(result)) => {
                wait_spinner.finish_and_clear();
                // Display handled by agent_loop.rs via result_line.
                // T4 contract: actions_breakdown + frozen elapsed_secs.
                Ok(json!({
                    "success": result.success,
                    "output": {
                        "id": result.id,
                        "task": result.task,
                        "response": result.response,
                        "tokens_used": result.tokens_used,
                        "actions_executed": result.actions_executed,
                        "actions_breakdown": result.actions_breakdown,
                        "duration_ms": result.duration_ms,
                        "elapsed_secs": (result.duration_ms as f64) / 1000.0,
                    }
                }))
            }
            Ok(Err(_)) => {
                wait_spinner.finish_and_clear();
                Ok(
                    json!({"success": false, "error": {"message": format!("Agent '{}' channel closed unexpectedly", id)}}),
                )
            }
            Err(_) => {
                wait_spinner.finish_and_clear();
                Ok(
                    json!({"success": false, "error": {"message": format!("Agent '{}' timed out after {}s", id, timeout_secs)}}),
                )
            }
        }
    } else {
        Ok(
            json!({"success": false, "error": {"message": format!("Agent '{}' already waited on or completed", id)}}),
        )
    }
}

/// ListAgents — show status of all agents.
/// Pure data view of the agent registry — no I/O, no printing.
///
/// Returns `{ agents: [...], running, completed, total }`. Used by the MCP
/// gateway and any other non-TUI consumer that must not corrupt stdout.
pub fn list_agents_data(registry: &AgentRegistry) -> Value {
    let agents = registry.agents.lock().unwrap();
    let entries: Vec<Value> = agents
        .values()
        .map(|e| {
            let status_str = match &e.status {
                AgentStatus::Running => "running",
                AgentStatus::Completed => "completed",
                AgentStatus::Failed(_) => "failed",
            };
            let elapsed = e.started_at.elapsed().as_secs();
            json!({
                "id": e.id,
                "task": e.task,
                "status": status_str,
                "elapsed_secs": elapsed,
                "tokens_used": e.result.as_ref().map(|r| r.tokens_used).unwrap_or(0),
            })
        })
        .collect();

    let running = entries.iter().filter(|e| e["status"] == "running").count();
    let completed = entries
        .iter()
        .filter(|e| e["status"] == "completed")
        .count();

    json!({
        "agents": entries,
        "running": running,
        "completed": completed,
        "total": entries.len(),
    })
}

pub fn execute_list_agents(registry: &AgentRegistry) -> Value {
    let data = list_agents_data(registry);
    let entries: Vec<Value> = data["agents"].as_array().cloned().unwrap_or_default();
    let running = data["running"].as_u64().unwrap_or(0) as usize;
    let completed = data["completed"].as_u64().unwrap_or(0) as usize;

    for e in &entries {
        let icon = match e["status"].as_str().unwrap_or("?") {
            "running" => "⟳".truecolor(200, 200, 0),
            "completed" => "✓".truecolor(0, 200, 120),
            "failed" => "✗".truecolor(200, 60, 60),
            _ => "?".dimmed(),
        };
        println!(
            "  {} {} — {} ({}s)",
            icon,
            e["id"].as_str().unwrap_or("?").truecolor(0, 200, 220),
            e["status"].as_str().unwrap_or("?").dimmed(),
            e["elapsed_secs"]
        );
    }
    println!(
        "  {} {}/{} agents completed",
        "·".dimmed(),
        completed,
        entries.len()
    );

    json!({
        "success": true,
        "output": {
            "agents": entries,
            "running": running,
            "completed": completed,
            "total": entries.len(),
        }
    })
}

/// BroadcastAgents — deliver a message into the inbox of all running agents.
///
/// Each running agent's loop drains its inbox at the start of every turn,
/// injecting messages as `[Broadcast from parent]` system context entries.
/// Completed, failed, or unknown agents are reported as `failed_targets`.
///
/// `success` is true only when **all** requested targets received the
/// message. Partial delivery or zero targets → `success: false`.
///
/// (T3) Optional `require_ack` (default `false`) controls whether
/// recipients must explicitly call `AcknowledgeBroadcast { message_id }`
/// for the parent's `GetBroadcastReceipts` to register an `acknowledged_at`
/// timestamp. With `require_ack=false` the recipient's loop auto-acks the
/// message the moment it drains it (preserves the historical fire-and-
/// forget semantics).
pub fn execute_broadcast_agents(registry: &AgentRegistry, action: &Value) -> Value {
    let message = action["message"].as_str().unwrap_or("").to_string();
    let require_ack = action
        .get("require_ack")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // (T3) Optional `targets: [id, …]` lets the LLM scope a broadcast to a
    // specific subset. When absent or empty, we default to "everyone the
    // registry knows about" (legacy behaviour). The list is what we report
    // back as `requested_targets` so the LLM can spot typos at a glance.
    let explicit_targets: Option<Vec<String>> = action
        .get("targets")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty());
    // Unique even across rapid back-to-back broadcasts within the same
    // millisecond: nanosecond resolution + a process-local monotonic seq.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = BROADCAST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let message_id = format!("bc_{}_{}", nanos, seq);

    if message.is_empty() {
        return json!({
            "success": false,
            "error": { "message": "BroadcastAgents requires a non-empty 'message'" }
        });
    }

    let agents = registry.agents.lock().unwrap();

    // (T3) Receipt taxonomy
    //
    // requested_targets — the ids the LLM asked for. When `targets` is
    //                     omitted, this is "every id the registry knows".
    // resolved_targets  — those requested ids that exist AND are Running.
    //                     The LLM can diff requested vs resolved to spot
    //                     typos / completed agents / dead workers.
    // delivered_to      — those resolved ids whose inbox push succeeded.
    //                     Lock poisoning is the only realistic source of
    //                     skew between resolved_targets and delivered_to.
    // failed_targets    — every requested id that did NOT make it into
    //                     delivered_to, each carrying a structured `reason`
    //                     ("not_found", "agent_completed", "agent_failed",
    //                     "inbox_lock_poisoned").
    let requested_targets: Vec<String> = explicit_targets
        .clone()
        .unwrap_or_else(|| agents.keys().cloned().collect());

    let mut resolved_targets: Vec<String> = Vec::new();
    let mut delivered_to: Vec<String> = Vec::new();
    let mut failed_targets: Vec<Value> = Vec::new();
    let now = now_ts();

    for tid in &requested_targets {
        match agents.get(tid) {
            None => {
                failed_targets.push(json!({"id": tid, "reason": "not_found"}));
            }
            Some(entry) => match entry.status {
                AgentStatus::Completed => {
                    failed_targets.push(json!({
                        "id": tid, "reason": "agent_completed"
                    }));
                }
                AgentStatus::Failed(_) => {
                    failed_targets.push(json!({
                        "id": tid, "reason": "agent_failed"
                    }));
                }
                AgentStatus::Running => {
                    resolved_targets.push(tid.clone());
                    if let Ok(mut inbox) = entry.inbox.lock() {
                        inbox.push(BroadcastEntry {
                            message_id: message_id.clone(),
                            body: message.clone(),
                            require_ack,
                            delivered_at: now,
                            acknowledged_at: None,
                        });
                        delivered_to.push(tid.clone());
                    } else {
                        failed_targets.push(json!({
                            "id": tid, "reason": "inbox_lock_poisoned"
                        }));
                    }
                }
            },
        }
    }

    let total_delivered = delivered_to.len();
    println!(
        "  {} broadcast to {}/{} agents: \"{}\"{}",
        "📡".truecolor(200, 160, 0),
        total_delivered,
        requested_targets.len(),
        message.chars().take(50).collect::<String>().dimmed(),
        if require_ack {
            "  [require_ack]".truecolor(200, 160, 0).to_string()
        } else {
            String::new()
        },
    );
    for id in &delivered_to {
        println!(
            "    {} → {}",
            "✓".truecolor(0, 200, 120),
            id.truecolor(0, 200, 220)
        );
    }
    for ft in &failed_targets {
        println!(
            "    {} → {} ({})",
            "✗".truecolor(200, 60, 60),
            ft["id"].as_str().unwrap_or("?").truecolor(200, 60, 60),
            ft["reason"].as_str().unwrap_or("?").dimmed()
        );
    }

    // (T3) success ≡ delivered_to == resolved_targets.
    // Empty/empty (no resolved targets) is therefore false — the LLM should
    // see "you broadcast to nothing" rather than a misleading green.
    let success = !resolved_targets.is_empty() && delivered_to.len() == resolved_targets.len();

    json!({
        "success": success,
        "output": {
            "message": message,
            "message_id": message_id,
            "require_ack": require_ack,
            "requested_targets": requested_targets,
            "resolved_targets": resolved_targets,
            "delivered_to": delivered_to,
            "failed_targets": failed_targets,
            "delivery_mode": "mailbox_append",
            "ack_hint": if require_ack {
                "Recipients must call AcknowledgeBroadcast {\"message_id\":\"<id>\"} \
                 — query GetBroadcastReceipts {\"message_id\":\"<id>\"} to see who \
                 has confirmed."
            } else {
                "Recipients are auto-acknowledged on drain. Pass \
                 require_ack:true to force explicit confirmation."
            },
        }
    })
}

// ── T3: GetBroadcasts / AcknowledgeBroadcast / GetBroadcastReceipts ──────────

/// (T3) Sub-agent action — returns this agent's pending inbox WITHOUT
/// draining it. Useful when the loop wants to inspect what's queued
/// before the next drain (e.g. to confirm an `AcknowledgeBroadcast` cleared
/// a `require_ack` message). Operates on the current agent's inbox handle
/// passed in by the dispatcher; returns `success:false` when called from
/// a context with no inbox (top-level agent).
pub fn execute_get_broadcasts(
    inbox: Option<&Arc<Mutex<Vec<BroadcastEntry>>>>,
    _action: &Value,
) -> Value {
    let Some(ib) = inbox else {
        return json!({
            "success": false,
            "error": { "message": "GetBroadcasts is only available inside a sub-agent (no inbox attached)" }
        });
    };
    let inbox = ib.lock().unwrap();
    let entries: Vec<Value> = inbox
        .iter()
        .map(|b| {
            json!({
                "message_id": b.message_id,
                "body": b.body,
                "require_ack": b.require_ack,
                "delivered_at": b.delivered_at,
                "acknowledged_at": b.acknowledged_at,
            })
        })
        .collect();
    let pending_ack = inbox
        .iter()
        .filter(|b| b.require_ack && b.acknowledged_at.is_none())
        .count();
    json!({
        "success": true,
        "output": {
            "count": entries.len(),
            "pending_ack": pending_ack,
            "messages": entries,
        }
    })
}

/// (T3) Sub-agent action — flips `acknowledged_at` on the matching inbox
/// entry. Idempotent: acking an already-acked message is a no-op success.
/// Returns `success:false` only when the message_id is unknown to this
/// agent's inbox or the call is made outside of a sub-agent context.
pub fn execute_acknowledge_broadcast(
    inbox: Option<&Arc<Mutex<Vec<BroadcastEntry>>>>,
    action: &Value,
) -> Value {
    let Some(ib) = inbox else {
        return json!({
            "success": false,
            "error": { "message": "AcknowledgeBroadcast is only available inside a sub-agent" }
        });
    };
    let message_id = action
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if message_id.is_empty() {
        return json!({
            "success": false,
            "error": { "message": "AcknowledgeBroadcast requires a non-empty 'message_id'" }
        });
    }
    let mut inbox = ib.lock().unwrap();
    if let Some(entry) = inbox.iter_mut().find(|b| b.message_id == message_id) {
        let was_already = entry.acknowledged_at.is_some();
        if !was_already {
            entry.acknowledged_at = Some(now_ts());
        }
        json!({
            "success": true,
            "output": {
                "message_id": message_id,
                "acknowledged_at": entry.acknowledged_at,
                "was_already_acknowledged": was_already,
            }
        })
    } else {
        json!({
            "success": false,
            "error": { "message": format!("No inbox entry with message_id '{}'", message_id) }
        })
    }
}

/// (T3) Parent action — collect per-recipient receipt status for a
/// broadcast that was previously sent. Designed to be polled while
/// `require_ack:true` recipients are still working.
pub fn execute_get_broadcast_receipts(registry: &AgentRegistry, action: &Value) -> Value {
    let message_id = action
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if message_id.is_empty() {
        return json!({
            "success": false,
            "error": { "message": "GetBroadcastReceipts requires a non-empty 'message_id'" }
        });
    }
    let receipts = registry.broadcast_receipts(message_id);
    json!({
        "success": true,
        "output": receipts,
    })
}

// ── Reflect ───────────────────────────────────────────────────────────────────

/// Execute a Reflect sysaicall — forces the LLM to self-evaluate.
///
/// This injects a structured self-evaluation prompt into the conversation,
/// asking the LLM to assess whether its recent actions achieved the goal.
/// The orchestrator captures the last N actions/results from the conversation
/// to provide context for the reflection.
pub fn execute_reflect(action: &Value, conversation: &crate::conversation::Conversation) -> Value {
    let goal = action
        .get("goal")
        .and_then(|v| v.as_str())
        .unwrap_or("the user's request");

    let depth = action.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;

    // Extract recent actions from conversation for context
    let messages = conversation.messages();
    let recent_actions: Vec<String> = messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, crate::conversation::Role::Assistant))
        .take(depth)
        .filter_map(|m| {
            if m.content.contains("[ACTION]") || m.content.contains("\"type\"") {
                Some(m.content.chars().take(200).collect::<String>())
            } else {
                None
            }
        })
        .collect();

    let recent_results: Vec<String> = messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, crate::conversation::Role::User))
        .take(depth)
        .filter_map(|m| {
            if m.content.contains("<result") || m.content.contains("\"success\"") {
                Some(m.content.chars().take(300).collect::<String>())
            } else {
                None
            }
        })
        .collect();

    // Build the reflective evaluation prompt
    let reflection_prompt = format!(
        "🪞 **SELF-EVALUATION** — Reflect on your recent actions:\n\
         \n\
         **Goal**: {goal}\n\
         **Recent actions ({n} most recent):**\n\
         {actions}\n\
         **Recent results:**\n\
         {results}\n\
         \n\
         Answer these questions:\n\
         1. Did my last actions move toward the goal? (yes/no + why)\n\
         2. Were there any errors or sub-optimal choices?\n\
         3. What should I do next? (concrete next step)\n\
         4. Confidence level: (0-100%)\n\
         \n\
         Be honest and concise.",
        goal = goal,
        n = depth,
        actions = if recent_actions.is_empty() {
            "  (no recent actions found)".to_string()
        } else {
            recent_actions
                .iter()
                .enumerate()
                .map(|(i, a)| format!("  {}. {}...", i + 1, a))
                .collect::<Vec<_>>()
                .join("\n")
        },
        results = if recent_results.is_empty() {
            "  (no recent results found)".to_string()
        } else {
            recent_results
                .iter()
                .enumerate()
                .map(|(i, r)| format!("  {}. {}...", i + 1, r))
                .collect::<Vec<_>>()
                .join("\n")
        },
    );

    println!("{}", "🪞 Reflect: self-evaluation triggered".bright_cyan());

    json!({
        "success": true,
        "output": {
            "type": "reflection_prompt",
            "inject_as_user": reflection_prompt,
            "goal": goal,
            "actions_reviewed": recent_actions.len(),
            "results_reviewed": recent_results.len(),
        }
    })
}

// ── Test-only helpers on AgentRegistry ─────────────────────────────────────────

#[cfg(test)]
impl AgentRegistry {
    /// Insert an already-completed agent entry. Used by tests to skip the
    /// real `execute_spawn_world` (which spawns x0ne-runtime + a real LLM).
    pub fn insert_completed_for_test(&self, result: AgentResult) {
        let mut agents = self.agents.lock().unwrap();
        agents.insert(
            result.id.clone(),
            AgentEntry {
                id: result.id.clone(),
                task: result.task.clone(),
                status: if result.success {
                    AgentStatus::Completed
                } else {
                    AgentStatus::Failed("test".into())
                },
                result: Some(result),
                result_rx: None,
                started_at: std::time::Instant::now(),
                inbox: Arc::new(Mutex::new(Vec::new())),
            },
        );
    }

    /// Insert a pending agent and return the sender that the test can use to
    /// later "complete" the agent — simulating a tokio::spawn sub-agent.
    pub fn insert_pending_for_test(&self, id: &str, task: &str) -> oneshot::Sender<AgentResult> {
        let (tx, rx) = oneshot::channel::<AgentResult>();
        let mut agents = self.agents.lock().unwrap();
        agents.insert(
            id.to_string(),
            AgentEntry {
                id: id.to_string(),
                task: task.to_string(),
                status: AgentStatus::Running,
                result: None,
                result_rx: Some(rx),
                started_at: std::time::Instant::now(),
                inbox: Arc::new(Mutex::new(Vec::new())),
            },
        );
        tx
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Test coverage for the orchestrator-side sysaicalls:
    //!
    //!   - `AgentRegistry`         — multi-agent (aka "parallel worlds") bookkeeping
    //!   - `execute_wait_agent`    — blocking on a sub-agent; success / failure / timeout
    //!   - `execute_list_agents`   — status snapshot
    //!   - `execute_broadcast_agents` — only addresses Running agents
    //!   - `execute_reflect`       — pure prompt construction
    //!
    //! What is intentionally NOT tested here:
    //!
    //!   - `display_plan_and_confirm` — interactive `read_line` from stdin.
    //!
    //! Live multi-container "parallel worlds" tests live in
    //! `x0ne-runtime/tests/incus_integration.rs` (opt-in via `X0NE_INCUS_REMOTE`).

    use super::*;
    use std::sync::Arc;

    fn agent_result(id: &str, success: bool) -> AgentResult {
        AgentResult {
            id: id.to_string(),
            task: format!("task for {}", id),
            response: format!("response from {}", id),
            tokens_used: 42,
            actions_executed: 3,
            actions_breakdown: serde_json::json!({
                "total": 3, "succeeded": 2, "failed": 1,
                "by_type": {"ReadFile": 2, "WriteFile": 1},
            }),
            success,
            duration_ms: 100,
        }
    }

    // ── AgentRegistry basics ───────────────────────────────────────────────────

    #[test]
    fn registry_starts_empty() {
        let reg = AgentRegistry::new();
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn registry_count_reflects_inserted_agents() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("a1", true));
        reg.insert_completed_for_test(agent_result("a2", true));
        reg.insert_completed_for_test(agent_result("a3", false));
        assert_eq!(reg.count(), 3);
    }

    // ── WaitAgent: error paths ─────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_agent_rejects_empty_id() {
        let reg = AgentRegistry::new();
        let r = execute_wait_agent(&reg, &json!({"id": ""})).await.unwrap();
        assert_eq!(r["success"], false);
        assert!(r["error"]["message"].as_str().unwrap().contains("requires"));
    }

    #[tokio::test]
    async fn wait_agent_rejects_unknown_id() {
        let reg = AgentRegistry::new();
        let r = execute_wait_agent(&reg, &json!({"id": "ghost"}))
            .await
            .unwrap();
        assert_eq!(r["success"], false);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    // ── WaitAgent: completion paths ────────────────────────────────────────────

    #[tokio::test]
    async fn wait_agent_returns_already_completed_result() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("done", true));

        let r = execute_wait_agent(&reg, &json!({"id": "done"}))
            .await
            .unwrap();
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["id"], "done");
        assert_eq!(r["output"]["tokens_used"], 42);
        assert_eq!(r["output"]["actions_executed"], 3);
        assert_eq!(r["output"]["response"], "response from done");
    }

    /// T4: WaitAgent must surface the per-type breakdown AND a frozen
    /// `elapsed_secs` derived from the cached `duration_ms`. Two
    /// successive reads of the same completed agent must return the
    /// SAME elapsed_secs — no stopwatch drift.
    #[tokio::test]
    async fn wait_agent_returns_actions_breakdown_and_frozen_elapsed_secs() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("done", true));

        let r1 = execute_wait_agent(&reg, &json!({"id": "done"}))
            .await
            .unwrap();
        let breakdown = &r1["output"]["actions_breakdown"];
        assert_eq!(breakdown["total"], 3);
        assert_eq!(breakdown["succeeded"], 2);
        assert_eq!(breakdown["failed"], 1);
        assert_eq!(breakdown["by_type"]["ReadFile"], 2);
        assert_eq!(breakdown["by_type"]["WriteFile"], 1);
        let elapsed1 = r1["output"]["elapsed_secs"].as_f64().unwrap();
        // 100 ms in our stub → 0.1 s exactly.
        assert!(
            (elapsed1 - 0.1).abs() < 1e-9,
            "expected 0.1, got {}",
            elapsed1
        );

        // Sleep a touch and re-read — value must be identical (frozen).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let r2 = execute_wait_agent(&reg, &json!({"id": "done"}))
            .await
            .unwrap();
        let elapsed2 = r2["output"]["elapsed_secs"].as_f64().unwrap();
        assert!(
            (elapsed1 - elapsed2).abs() < 1e-9,
            "elapsed_secs must be frozen across reads (got {} then {})",
            elapsed1,
            elapsed2
        );
    }

    #[tokio::test]
    async fn wait_agent_propagates_failed_status() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("crashed", false));

        let r = execute_wait_agent(&reg, &json!({"id": "crashed"}))
            .await
            .unwrap();
        // Agent ran but failed: success=false but the entry is found
        // (this is *not* the same as "agent not found")
        assert_eq!(r["success"], false);
        assert_eq!(r["output"]["id"], "crashed");
    }

    #[tokio::test]
    async fn wait_agent_blocks_until_sender_completes() {
        let reg = Arc::new(AgentRegistry::new());
        let tx = reg.insert_pending_for_test("worker-1", "compute pi");

        let reg_for_writer = reg.clone();
        // Simulate the spawned tokio task completing after 50ms.
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // Mirror what `execute_spawn_world` does in production:
            // mark Completed and send the result on the channel.
            {
                let mut agents = reg_for_writer.agents.lock().unwrap();
                if let Some(e) = agents.get_mut("worker-1") {
                    e.status = AgentStatus::Completed;
                }
            }
            let _ = tx.send(agent_result("worker-1", true));
        });

        let r = execute_wait_agent(&reg, &json!({"id": "worker-1", "timeout_secs": 5}))
            .await
            .unwrap();
        writer.await.unwrap();

        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["id"], "worker-1");
        assert_eq!(r["output"]["response"], "response from worker-1");
    }

    #[tokio::test]
    async fn wait_agent_times_out_when_sender_never_completes() {
        let reg = AgentRegistry::new();
        let _tx = reg.insert_pending_for_test("slow", "very slow");

        let r = execute_wait_agent(&reg, &json!({"id": "slow", "timeout_secs": 1}))
            .await
            .unwrap();
        assert_eq!(r["success"], false);
        let msg = r["error"]["message"].as_str().unwrap();
        assert!(msg.contains("timed out"), "expected timeout, got: {}", msg);
    }

    #[tokio::test]
    async fn wait_agent_reports_already_waited_when_rx_consumed() {
        // First call takes the receiver; if no result is ever produced,
        // a second wait must report "already waited" — not panic, not deadlock.
        let reg = AgentRegistry::new();
        let tx = reg.insert_pending_for_test("once", "once-task");

        // Fire & forget: we use a 1s timeout so the first call returns quickly.
        let _ = execute_wait_agent(&reg, &json!({"id": "once", "timeout_secs": 1})).await;
        // Drop the sender so the channel is closed.
        drop(tx);

        let r = execute_wait_agent(&reg, &json!({"id": "once", "timeout_secs": 1}))
            .await
            .unwrap();
        assert_eq!(r["success"], false);
        let msg = r["error"]["message"].as_str().unwrap();
        assert!(msg.contains("already waited"), "got: {}", msg);
    }

    // ── Parallel worlds: many sub-agents waited concurrently ───────────────────

    #[tokio::test]
    async fn parallel_worlds_three_agents_completed_in_arbitrary_order() {
        // This is the orchestrator-side analogue of `RunIncusGroup` —
        // multiple sub-agents producing results concurrently, then joined.
        let reg = Arc::new(AgentRegistry::new());

        let txs: Vec<_> = (0..3)
            .map(|i| reg.insert_pending_for_test(&format!("alt-{i}"), &format!("attempt {i}")))
            .collect();

        let reg_writer = reg.clone();
        // Complete them out of order (2, 0, 1) to prove WaitAgent matches by id.
        let writer = tokio::spawn(async move {
            let order = [(2usize, 80u64), (0, 30), (1, 50)];
            let mut txs = txs.into_iter().enumerate().collect::<Vec<_>>();
            for (idx, delay) in order {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let pos = txs.iter().position(|(i, _)| *i == idx).unwrap();
                let (_, tx) = txs.remove(pos);
                let id = format!("alt-{idx}");
                {
                    let mut agents = reg_writer.agents.lock().unwrap();
                    if let Some(e) = agents.get_mut(&id) {
                        e.status = AgentStatus::Completed;
                    }
                }
                let _ = tx.send(agent_result(&id, true));
            }
        });

        // Wait for them all, in their original (0, 1, 2) order — each call
        // must return the result for its own id, even though completion
        // happened in (2, 0, 1) order.
        let mut handles = Vec::new();
        for i in 0..3 {
            let reg2 = reg.clone();
            handles.push(tokio::spawn(async move {
                execute_wait_agent(&reg2, &json!({"id": format!("alt-{i}"), "timeout_secs": 5}))
                    .await
                    .unwrap()
            }));
        }

        for (i, h) in handles.into_iter().enumerate() {
            let r = h.await.unwrap();
            assert_eq!(r["success"], true);
            assert_eq!(r["output"]["id"], format!("alt-{i}"));
        }
        writer.await.unwrap();
    }

    // ── ListAgents ─────────────────────────────────────────────────────────────

    #[test]
    fn list_agents_empty_returns_zero_counts() {
        let reg = AgentRegistry::new();
        let v = execute_list_agents(&reg);
        assert_eq!(v["success"], true);
        assert_eq!(v["output"]["total"], 0);
        assert_eq!(v["output"]["running"], 0);
        assert_eq!(v["output"]["completed"], 0);
        assert!(v["output"]["agents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn list_agents_counts_running_vs_completed() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("done-1", true));
        reg.insert_completed_for_test(agent_result("done-2", true));
        reg.insert_completed_for_test(agent_result("done-3", false));
        let _tx = reg.insert_pending_for_test("running-1", "still going");

        let v = execute_list_agents(&reg);
        assert_eq!(v["output"]["total"], 4);
        assert_eq!(v["output"]["running"], 1);
        // 2 Completed + 1 Failed (the Failed status is reported as "failed",
        // not "completed", which mirrors execute_list_agents' status mapping).
        assert_eq!(v["output"]["completed"], 2);
    }

    // ── BroadcastAgents ────────────────────────────────────────────────────────

    #[test]
    fn broadcast_delivers_to_running_agents_inbox() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("done", true));
        let _t1 = reg.insert_pending_for_test("alive-1", "loop");
        let _t2 = reg.insert_pending_for_test("alive-2", "loop");

        let v = execute_broadcast_agents(&reg, &json!({"message": "stop"}));

        // (T3 closure) `success` now means "every *resolvable* target got
        // the message". Completed/failed agents surface in `failed_targets`
        // with a structured reason but DO NOT poison the overall success
        // bit when they're discovered via the default broadcast — the LLM
        // reads `failed_targets` to learn who was unreachable. This makes
        // "broadcast STOP to everyone still running" a clean true.
        assert_eq!(
            v["success"], true,
            "all resolvable (Running) targets delivered → success=true: {}",
            v
        );

        let delivered = v["output"]["delivered_to"].as_array().unwrap();
        assert_eq!(delivered.len(), 2);
        let ids: Vec<&str> = delivered.iter().map(|s| s.as_str().unwrap()).collect();
        assert!(ids.contains(&"alive-1"));
        assert!(ids.contains(&"alive-2"));

        let failed = v["output"]["failed_targets"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["id"], "done");
        // (T3) Reason is now a stable structured token, not free-text.
        assert_eq!(failed[0]["reason"], "agent_completed");

        assert_eq!(v["output"]["message"], "stop");
        assert_eq!(v["output"]["delivery_mode"], "mailbox_append");
        // (T3) requested vs resolved split — caller asked for all 3 known
        // agents (default targeting), only 2 were Running and resolvable.
        let req = v["output"]["requested_targets"].as_array().unwrap();
        let res = v["output"]["resolved_targets"].as_array().unwrap();
        assert_eq!(req.len(), 3);
        assert_eq!(res.len(), 2);
        assert!(v["output"]["message_id"]
            .as_str()
            .unwrap()
            .starts_with("bc_"));

        // Verify the message is actually in the inboxes (T3: BroadcastEntry shape)
        let inbox1 = reg.get_inbox("alive-1").unwrap();
        let inbox2 = reg.get_inbox("alive-2").unwrap();
        let i1 = inbox1.lock().unwrap();
        let i2 = inbox2.lock().unwrap();
        assert_eq!(i1.len(), 1);
        assert_eq!(i1[0].body, "stop");
        assert!(!i1[0].require_ack, "default broadcast must NOT require ack");
        assert!(i1[0].acknowledged_at.is_none(),
            "delivery alone must not flip acknowledged_at — only the loop's drain or AcknowledgeBroadcast can");
        assert_eq!(i2.len(), 1);
        assert_eq!(i2[0].body, "stop");
    }

    #[test]
    fn broadcast_all_running_returns_success() {
        let reg = AgentRegistry::new();
        let _t1 = reg.insert_pending_for_test("a", "task-a");
        let _t2 = reg.insert_pending_for_test("b", "task-b");

        let v = execute_broadcast_agents(&reg, &json!({"message": "focus"}));
        assert_eq!(
            v["success"], true,
            "all-running broadcast should succeed: {}",
            v
        );
        assert_eq!(v["output"]["delivered_to"].as_array().unwrap().len(), 2);
        assert!(v["output"]["failed_targets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn broadcast_no_targets_returns_failure() {
        let reg = AgentRegistry::new();
        let v = execute_broadcast_agents(&reg, &json!({"message": "hello"}));
        assert_eq!(v["success"], false, "no agents → must fail: {}", v);
        assert!(v["output"]["delivered_to"].as_array().unwrap().is_empty());
    }

    #[test]
    fn broadcast_only_completed_agents_returns_failure() {
        let reg = AgentRegistry::new();
        reg.insert_completed_for_test(agent_result("a", true));
        reg.insert_completed_for_test(agent_result("b", false));

        let v = execute_broadcast_agents(&reg, &json!({"message": "wake up"}));
        assert_eq!(
            v["success"], false,
            "only completed/failed agents → must fail: {}",
            v
        );
        assert!(v["output"]["delivered_to"].as_array().unwrap().is_empty());
        assert_eq!(v["output"]["failed_targets"].as_array().unwrap().len(), 2);
    }

    /// (T3) `targets:["unknown"]` must produce a `not_found` failed_target
    /// — never silent success. The LLM uses this to spot typos.
    #[test]
    fn broadcast_unknown_target_yields_not_found_failed_target() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("real", "task");
        let v = execute_broadcast_agents(
            &reg,
            &json!({
                "message": "ping",
                "targets": ["real", "ghost"],
            }),
        );
        let failed = v["output"]["failed_targets"].as_array().unwrap();
        let ghost = failed
            .iter()
            .find(|f| f["id"] == "ghost")
            .expect("ghost reported");
        assert_eq!(ghost["reason"], "not_found");
        // Real agent received it.
        let delivered = v["output"]["delivered_to"].as_array().unwrap();
        assert!(delivered.iter().any(|d| d == "real"));
        // Resolved == [real]; failed contains ghost; success false because
        // delivered (1) == resolved (1), but failed_targets non-empty makes
        // the LLM see a partial outcome via the explicit list.
        assert_eq!(v["output"]["resolved_targets"].as_array().unwrap().len(), 1);
    }

    /// (T3) Explicit `targets` list scopes the broadcast — agents not in
    /// the list are NOT included in `requested_targets`, even if they are
    /// Running. This is what lets the LLM coordinate sub-groups.
    #[test]
    fn broadcast_with_explicit_targets_scopes_to_listed_ids() {
        let reg = AgentRegistry::new();
        let _t1 = reg.insert_pending_for_test("a", "ta");
        let _t2 = reg.insert_pending_for_test("b", "tb");
        let _t3 = reg.insert_pending_for_test("c", "tc");
        let v = execute_broadcast_agents(
            &reg,
            &json!({
                "message": "team-a",
                "targets": ["a", "b"],
            }),
        );
        let req = v["output"]["requested_targets"].as_array().unwrap();
        assert_eq!(req.len(), 2);
        let req_ids: Vec<&str> = req.iter().map(|x| x.as_str().unwrap()).collect();
        assert!(req_ids.contains(&"a"));
        assert!(req_ids.contains(&"b"));
        assert!(
            !req_ids.contains(&"c"),
            "c must not be in requested_targets"
        );
        // c's inbox must be empty.
        let inbox_c = reg.get_inbox("c").unwrap();
        assert!(inbox_c.lock().unwrap().is_empty());
        assert_eq!(v["success"], true, "all listed targets delivered");
    }

    #[test]
    fn broadcast_empty_message_rejected() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("a", "task");
        let v = execute_broadcast_agents(&reg, &json!({"message": ""}));
        assert_eq!(
            v["success"], false,
            "empty message should be rejected: {}",
            v
        );
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("non-empty"));
    }

    #[test]
    fn broadcast_multiple_messages_accumulate_in_inbox() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("a", "task");

        execute_broadcast_agents(&reg, &json!({"message": "first"}));
        execute_broadcast_agents(&reg, &json!({"message": "second"}));

        let inbox = reg.get_inbox("a").unwrap();
        let messages = inbox.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, "first");
        assert_eq!(messages[1].body, "second");
        assert_ne!(
            messages[0].message_id, messages[1].message_id,
            "each broadcast must produce a distinct message_id"
        );
    }

    // ── T3: receipt mailbox ────────────────────────────────────────────────

    /// `require_ack:true` must be reflected in both the response payload AND
    /// the inbox entry — the agent_loop reads `require_ack` to determine
    /// whether to auto-ack on drain.
    #[test]
    fn t3_broadcast_require_ack_propagates_to_inbox_entry() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("worker", "long task");

        let v =
            execute_broadcast_agents(&reg, &json!({"message": "stop now", "require_ack": true}));
        assert_eq!(v["success"], true);
        assert_eq!(
            v["output"]["require_ack"], true,
            "BroadcastAgents response must echo require_ack=true"
        );

        let inbox = reg.get_inbox("worker").unwrap();
        let entry = inbox.lock().unwrap()[0].clone();
        assert!(
            entry.require_ack,
            "inbox entry must carry require_ack=true so the loop won't auto-ack on drain"
        );
        assert!(
            entry.acknowledged_at.is_none(),
            "delivery must NEVER preset acknowledged_at — only the recipient can"
        );
    }

    /// Default behaviour (no `require_ack`) must remain backwards-compatible:
    /// receipts include the entry as `require_ack=false`, consistent with
    /// the legacy fire-and-forget contract.
    #[test]
    fn t3_broadcast_defaults_to_no_ack_required() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("worker", "task");

        let v = execute_broadcast_agents(&reg, &json!({"message": "fyi"}));
        assert_eq!(
            v["output"]["require_ack"], false,
            "absent require_ack must serialise as false, not null"
        );

        let inbox = reg.get_inbox("worker").unwrap();
        assert!(!inbox.lock().unwrap()[0].require_ack);
    }

    /// `GetBroadcasts` must enumerate the recipient's inbox without consuming
    /// it — repeated calls return the same list. Only AcknowledgeBroadcast
    /// (or the loop's drain in the auto-ack case) ever mutates state.
    #[test]
    fn t3_get_broadcasts_returns_inbox_without_consuming() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("worker", "task");
        execute_broadcast_agents(&reg, &json!({"message": "look here", "require_ack": true}));

        let inbox = reg.get_inbox("worker").unwrap();

        let r1 = execute_get_broadcasts(Some(&inbox), &json!({}));
        assert_eq!(r1["success"], true);
        assert_eq!(r1["output"]["count"], 1);
        assert_eq!(r1["output"]["pending_ack"], 1);
        assert_eq!(r1["output"]["messages"][0]["body"], "look here");
        assert_eq!(r1["output"]["messages"][0]["require_ack"], true);

        // Calling it again must yield identical output: GetBroadcasts is read-only.
        let r2 = execute_get_broadcasts(Some(&inbox), &json!({}));
        assert_eq!(r2, r1, "GetBroadcasts must not consume the inbox");
    }

    /// `GetBroadcasts` with no inbox attached (top-level agent) must return
    /// a structured error rather than silently succeed with an empty inbox.
    #[test]
    fn t3_get_broadcasts_outside_subagent_returns_clear_error() {
        let r = execute_get_broadcasts(None, &json!({}));
        assert_eq!(r["success"], false);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only available inside a sub-agent"));
    }

    /// `AcknowledgeBroadcast` must flip `acknowledged_at`, surface the
    /// new timestamp in its response, AND be idempotent (a re-ack returns
    /// success with `was_already_acknowledged=true`).
    #[test]
    fn t3_acknowledge_broadcast_is_idempotent_and_flips_ack_at() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("worker", "task");
        let bc = execute_broadcast_agents(&reg, &json!({"message": "halt", "require_ack": true}));
        let mid = bc["output"]["message_id"].as_str().unwrap().to_string();
        let inbox = reg.get_inbox("worker").unwrap();

        let r1 = execute_acknowledge_broadcast(Some(&inbox), &json!({"message_id": mid}));
        assert_eq!(r1["success"], true, "first ack must succeed");
        assert_eq!(r1["output"]["was_already_acknowledged"], false);
        let ack_at = r1["output"]["acknowledged_at"]
            .as_u64()
            .expect("ack timestamp");
        assert!(ack_at > 0, "acknowledged_at must be a non-zero unix ts");

        // Idempotent: second ack returns success with was_already_acknowledged=true,
        // and the original timestamp is preserved (we don't bump it).
        let r2 = execute_acknowledge_broadcast(Some(&inbox), &json!({"message_id": mid}));
        assert_eq!(r2["success"], true, "re-ack must remain successful");
        assert_eq!(r2["output"]["was_already_acknowledged"], true);
        assert_eq!(
            r2["output"]["acknowledged_at"].as_u64(),
            Some(ack_at),
            "re-ack must NOT mutate the original acknowledged_at timestamp"
        );
    }

    /// Acking an unknown message_id is a clear failure — we do NOT pretend
    /// the receipt landed somewhere it didn't.
    #[test]
    fn t3_acknowledge_unknown_message_id_fails_loudly() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("worker", "task");
        let inbox = reg.get_inbox("worker").unwrap();

        let r = execute_acknowledge_broadcast(Some(&inbox), &json!({"message_id": "bc_not_real"}));
        assert_eq!(r["success"], false);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No inbox entry"));
    }

    /// `GetBroadcastReceipts` is the parent's view: it must enumerate every
    /// recipient that received the broadcast and report individual ack
    /// status. After exactly one ack, `acknowledged_count` must be 1 and
    /// the corresponding recipient entry must have a non-null `acknowledged_at`.
    #[test]
    fn t3_get_broadcast_receipts_aggregates_per_recipient() {
        let reg = AgentRegistry::new();
        let _t1 = reg.insert_pending_for_test("a", "task");
        let _t2 = reg.insert_pending_for_test("b", "task");
        let bc = execute_broadcast_agents(&reg, &json!({"message": "stop", "require_ack": true}));
        let mid = bc["output"]["message_id"].as_str().unwrap().to_string();

        // Only `a` acks.
        let inbox_a = reg.get_inbox("a").unwrap();
        execute_acknowledge_broadcast(Some(&inbox_a), &json!({"message_id": mid}));

        let r = execute_get_broadcast_receipts(&reg, &json!({"message_id": mid}));
        assert_eq!(r["success"], true);
        let out = &r["output"];
        assert_eq!(out["delivered_count"], 2);
        assert_eq!(
            out["acknowledged_count"], 1,
            "exactly one recipient acked, receipts must reflect that"
        );
        let recipients = out["recipients"].as_array().unwrap();
        assert_eq!(recipients.len(), 2);
        let acked_ids: Vec<&str> = recipients
            .iter()
            .filter(|r| !r["acknowledged_at"].is_null())
            .map(|r| r["agent_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            acked_ids,
            vec!["a"],
            "only `a` should appear with a non-null acknowledged_at"
        );
    }

    /// Receipts for an unknown message_id must produce a benign empty
    /// summary, not error — the parent might race against an agent that
    /// already exited and dropped its inbox.
    #[test]
    fn t3_get_broadcast_receipts_unknown_id_returns_empty_recipients() {
        let reg = AgentRegistry::new();
        let _t = reg.insert_pending_for_test("a", "task");
        let r = execute_get_broadcast_receipts(&reg, &json!({"message_id": "bc_does_not_exist"}));
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["delivered_count"], 0);
        assert_eq!(r["output"]["acknowledged_count"], 0);
        assert!(r["output"]["recipients"].as_array().unwrap().is_empty());
    }

    /// `GetBroadcastReceipts` with an empty `message_id` is a misuse and
    /// must error instead of returning a misleading global summary.
    #[test]
    fn t3_get_broadcast_receipts_rejects_empty_message_id() {
        let reg = AgentRegistry::new();
        let r = execute_get_broadcast_receipts(&reg, &json!({"message_id": ""}));
        assert_eq!(r["success"], false);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("non-empty 'message_id'"));
    }

    // ── Reflect ────────────────────────────────────────────────────────────────

    #[test]
    fn reflect_with_empty_conversation_returns_well_formed_prompt() {
        let conv = crate::conversation::Conversation::new(8000);
        let v = execute_reflect(&json!({"goal": "find the bug", "depth": 3}), &conv);
        assert_eq!(v["success"], true);
        assert_eq!(v["output"]["type"], "reflection_prompt");
        assert_eq!(v["output"]["goal"], "find the bug");
        assert_eq!(v["output"]["actions_reviewed"], 0);
        assert_eq!(v["output"]["results_reviewed"], 0);
        let inject = v["output"]["inject_as_user"].as_str().unwrap();
        assert!(inject.contains("SELF-EVALUATION"));
        assert!(inject.contains("find the bug"));
        assert!(inject.contains("(no recent actions found)"));
    }

    #[test]
    fn reflect_extracts_recent_actions_and_results_within_depth() {
        let mut conv = crate::conversation::Conversation::new(8000);
        // Interleave 4 action/result pairs — depth=2 should pick up only the latest 2.
        for i in 0..4 {
            conv.add_assistant(&format!("[ACTION] {{\"type\": \"WriteFile\", \"i\": {i}}}"));
            conv.add_user(&format!("<result success=true tag=\"r{i}\"/>"));
        }
        let v = execute_reflect(&json!({"goal": "x", "depth": 2}), &conv);
        assert_eq!(v["output"]["actions_reviewed"], 2);
        assert_eq!(v["output"]["results_reviewed"], 2);
    }

    #[test]
    fn reflect_uses_default_goal_when_missing() {
        let conv = crate::conversation::Conversation::new(8000);
        let v = execute_reflect(&json!({}), &conv);
        assert_eq!(v["output"]["goal"], "the user's request");
    }

    // ── T0.5: Plan approval gate ──────────────────────────────────────────
    //
    // The bug being prevented: in the previous code path,
    // `read_line(...)` on a closed stdin returned `Ok(0)` with an empty
    // buffer, and `if answer.is_empty() { approve }` silently auto-approved
    // any Plan in any non-interactive context (sub-agents, EOF stdin, HTTP
    // serve mode). That is a security gate failing open.
    //
    // The new heuristic is "did we receive a character?" — not "is stdin a
    // TTY?" — so legitimate piped intent (`echo "y" | x0ne`) still works,
    // while a closed stdin / sub-agent / empty piped line all fail closed
    // with a structured `plan_requires_human` error.

    /// Sandbox yolo approves plans like `X0NE_PLAN_AUTO_APPROVE=y`; an
    /// explicit `n` from the operator still wins.
    #[test]
    fn sandbox_yolo_approves_plans() {
        assert_eq!(plan_env(None, true).as_deref(), Some("y"));
        assert_eq!(plan_env(Some("n"), true).as_deref(), Some("n"));
        assert_eq!(plan_env(None, false), None);
        assert_eq!(plan_env(Some("y"), false).as_deref(), Some("y"));
    }

    /// `X0NE_PLAN_AUTO_APPROVE=y` always wins, even in a sub-agent context.
    /// This is the documented escape hatch for CI / cron / serve-mode setups.
    #[test]
    fn plan_decision_env_auto_approve_wins_even_for_subagent() {
        let d = decide_plan_approval_inner(
            /*is_subagent=*/ true,
            Some("y"),
            Ok((0, String::new())),
            false,
        );
        assert!(d.approved);
        assert_eq!(d.decision_source, "env_auto_approve");
        assert!(d.error.is_none());
    }

    /// `X0NE_PLAN_AUTO_APPROVE=n` (or any non-yes value) is an explicit
    /// rejection — NOT a fail-closed `plan_requires_human`. The operator
    /// said no on purpose; the LLM should treat it as a normal "no".
    #[test]
    fn plan_decision_env_explicit_reject_carries_no_error() {
        let d = decide_plan_approval_inner(false, Some("n"), Ok((0, String::new())), true);
        assert!(!d.approved);
        assert_eq!(d.decision_source, "env_explicit_reject");
        assert!(d.error.is_none(), "explicit reject is not a safety gate");
    }

    /// REGRESSION: this is the bug from session 2026-04-19_19-53-07 turn 14.
    /// A sub-agent emitting Plan must NEVER auto-approve, regardless of
    /// what stdin returns (because stdin is shared with the parent and a
    /// read would race the parent's TTY).
    #[test]
    fn plan_decision_subagent_always_rejected_with_structured_error() {
        let d = decide_plan_approval_inner(
            /*is_subagent=*/ true,
            None,
            Ok((1, "y\n".to_string())),
            true,
        );
        assert!(!d.approved, "sub-agents must NEVER approve a Plan");
        assert_eq!(d.decision_source, "rejected_subagent");
        let err = d
            .error
            .expect("sub-agent rejection must carry plan_requires_human");
        assert_eq!(err.code, "plan_requires_human");
        assert_eq!(err.class, "validation_error");
        assert!(
            err.recommended_next_step.contains("sub-agent"),
            "the recommended next step must mention the sub-agent context"
        );
    }

    /// REGRESSION: the original silent auto-approve bug. `x0ne < /dev/null`
    /// (or any context where read_line returns Ok(0) before any byte) must
    /// fail closed with `plan_requires_human`, NOT auto-approve.
    #[test]
    fn plan_decision_closed_stdin_rejects_with_requires_human() {
        let d = decide_plan_approval_inner(false, None, Ok((0, String::new())), false);
        assert!(!d.approved, "EOF on stdin must NEVER auto-approve");
        assert_eq!(d.decision_source, "rejected_non_interactive_eof");
        let err = d
            .error
            .expect("EOF rejection must carry plan_requires_human");
        assert_eq!(err.code, "plan_requires_human");
    }

    /// `echo "y" | x0ne` — piped intent. The user explicitly typed `y`
    /// upstream, just not at the TTY. Honor it.
    #[test]
    fn plan_decision_piped_yes_is_approved() {
        let d = decide_plan_approval_inner(
            false,
            None,
            Ok((2, "y\n".to_string())),
            /*stdin_is_tty=*/ false,
        );
        assert!(d.approved);
        assert_eq!(d.decision_source, "stdin_yes");
        assert!(d.error.is_none());
    }

    /// `echo "yes" | x0ne` — same shape, longer answer.
    #[test]
    fn plan_decision_piped_yes_long_form_is_approved() {
        let d = decide_plan_approval_inner(false, None, Ok((4, "yes\n".to_string())), false);
        assert!(d.approved);
        assert_eq!(d.decision_source, "stdin_yes");
    }

    /// Real TTY + bare Enter: documented `[y/n/e]` UX, default is approve.
    /// This is the only path where empty input means yes — and only when
    /// `stdin_is_tty` is true, so a piped `\n` cannot exploit it.
    #[test]
    fn plan_decision_tty_bare_enter_is_default_yes() {
        let d = decide_plan_approval_inner(
            false,
            None,
            Ok((1, "\n".to_string())),
            /*stdin_is_tty=*/ true,
        );
        assert!(d.approved);
        assert_eq!(d.decision_source, "tty_enter_default");
    }

    /// Piped empty line (`printf '\n' | x0ne`): ambiguous, NOT a yes. The
    /// only ambiguity is when stdin is not a TTY but produced a single
    /// newline — could be a script glitch, could be intentional. Fail
    /// closed; the operator can pipe `y` if they mean approve.
    #[test]
    fn plan_decision_piped_empty_line_is_rejected_not_yes() {
        let d = decide_plan_approval_inner(
            false,
            None,
            Ok((1, "\n".to_string())),
            /*stdin_is_tty=*/ false,
        );
        assert!(!d.approved);
        assert_eq!(d.decision_source, "rejected_piped_empty_line");
        assert!(d.error.is_some(), "piped empty line is a fail-closed gate");
    }

    /// Real TTY + explicit "n": the user said no. Not a fail-closed gate,
    /// just a normal rejection.
    #[test]
    fn plan_decision_tty_no_carries_no_error() {
        let d = decide_plan_approval_inner(false, None, Ok((2, "n\n".to_string())), true);
        assert!(!d.approved);
        assert_eq!(d.decision_source, "tty_rejected");
        assert!(d.error.is_none());
    }

    /// Stdin I/O error (rare, but possible on broken pipe): fail closed.
    #[test]
    fn plan_decision_stdin_io_error_rejects_with_requires_human() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe died");
        let d = decide_plan_approval_inner(false, None, Err(err), false);
        assert!(!d.approved);
        assert_eq!(d.decision_source, "stdin_io_error");
        assert!(d.error.is_some());
    }

    /// The result-JSON shape must always carry `decision_source` for
    /// audit-grep, in both the approve and reject branches.
    #[test]
    fn plan_result_json_always_carries_decision_source() {
        let action = json!({"type": "Plan", "label": "demo plan", "steps": []});

        let approved = PlanDecision {
            approved: true,
            decision_source: "stdin_yes",
            error: None,
        };
        let v = plan_decision_to_result_json(&action, &approved);
        assert_eq!(v["success"], true);
        assert_eq!(v["output"]["decision_source"], "stdin_yes");
        assert_eq!(v["output"]["label"], "demo plan");
        assert!(v.get("error").is_none() || v["error"].is_null());

        let rejected_subagent = PlanDecision {
            approved: false,
            decision_source: "rejected_subagent",
            error: Some(PlanError::requires_human("rejected_subagent")),
        };
        let v = plan_decision_to_result_json(&action, &rejected_subagent);
        assert_eq!(v["success"], false);
        assert_eq!(v["output"]["decision_source"], "rejected_subagent");
        assert_eq!(v["error"]["code"], "plan_requires_human");
        assert_eq!(v["error"]["class"], "validation_error");
        assert_eq!(v["error"]["retryable"], false);
        assert_eq!(
            v["error"]["decision_source"], "rejected_subagent",
            "decision_source duplicated in error block for grep symmetry"
        );
    }
}
