//! Goal manager: high-level operations on top of `GoalStore`.
//!
//! The manager is the boundary between the MCP server (`mcp_server.rs`)
//! and the persistence layer (`store.rs`) + the achievement-check engine
//! (which talks back to the downstream runtime).
//!
//! Concurrency: the manager holds a `Mutex<GoalStore>` so calls from
//! multiple MCP handlers don't race on the active pointer / file writes.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use x0ne_mcp::ChildMcpClient;

use super::skeptic::{skeptic_prompt, SkepticOracle, SkepticVerdict};
use super::state::{AchievementCheck, Budget, GoalState, GoalStatus, HistoryKind, SkepticCadence};
use super::store::GoalStore;

pub struct GoalManager {
    store: Mutex<GoalStore>,
}

/// Outcome of one `step()` call. The MCP layer turns this into the wire
/// payload it returns to the host agent.
#[derive(Debug)]
pub struct StepOutcome {
    pub state: GoalState,
    /// `true` if the achievement check ran and reported success this tick.
    pub achievement_succeeded: bool,
    /// Free-form note from the achievement check (e.g. "exit 0", "file
    /// exists", or a downstream error message). Always present after a
    /// non-Manual check; `None` when no check is configured or the goal
    /// transitioned out of `Active` before we ran one.
    pub achievement_note: Option<String>,
}

/// What a skeptic pass produced. The verdict is returned **with** its
/// fingerprint: the caller does not have to recompute it, and two
/// computations cannot diverge.
#[derive(Debug)]
pub struct SkepticOutcome {
    pub state: GoalState,
    pub verdict: SkepticVerdict,
    /// Empty when no gap cited a `file:line` — in that case the stall guard
    /// does not apply.
    pub fingerprint: String,
    /// `true` when the same fingerprint came back `GOAL_STALL_THRESHOLD` times
    /// in a row: the goal has just moved to `Blocked`.
    pub stalled: bool,
}

impl GoalManager {
    pub fn new(store: GoalStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    /// Default to `~/.x0ne/goals/`. Falls back to a tempdir-style failure
    /// if `$HOME` is somehow missing — bubble up the error.
    pub fn new_default() -> Result<Self> {
        Ok(Self::new(GoalStore::default_for_home()?))
    }

    /// Create / replace the active goal. If an active goal already
    /// exists, it is **kept on disk** but no longer pointed-to (the user
    /// can `x0ne_goal_status --id` later if we expose it). This matches
    /// Codex's `/goal <text>` semantics: the new text wins, the old goal
    /// is archived, not destroyed.
    pub async fn set(
        &self,
        text: String,
        budget: Budget,
        achievement_check: Option<AchievementCheck>,
    ) -> Result<GoalState> {
        let id = format!("goal_{}", Uuid::now_v7().simple());
        let goal = GoalState::new(id.clone(), text, budget, achievement_check);

        let store = self.store.lock().await;
        store.save(&goal)?;
        store.set_active(&id)?;
        Ok(goal)
    }

    pub async fn get_active(&self) -> Result<Option<GoalState>> {
        let store = self.store.lock().await;
        store.get_active()
    }

    /// Pause the active goal (no-op idempotent). Returns the post-state.
    pub async fn pause(&self) -> Result<Option<GoalState>> {
        self.transition_active(|g| {
            if matches!(g.status, GoalStatus::Active) {
                g.status = GoalStatus::Paused;
                g.append_history(HistoryKind::Paused, None, None, None);
            }
            Ok(())
        })
        .await
    }

    /// Resume a paused goal. No-op if Active. Errors if Achieved/Cleared
    /// — the user should `set` a fresh goal in those cases.
    pub async fn resume(&self) -> Result<Option<GoalState>> {
        self.transition_active(|g| {
            match &g.status {
                GoalStatus::Paused | GoalStatus::Blocked { .. } => {
                    g.status = GoalStatus::Active;
                    g.append_history(HistoryKind::Resumed, None, None, None);
                    Ok(())
                }
                GoalStatus::Active => Ok(()), // idempotent
                terminal => Err(anyhow!(
                    "cannot resume goal in terminal state {:?}; create a new goal with x0ne_goal_set",
                    terminal
                )),
            }
        })
        .await
    }

    /// Mark the goal as achieved manually (no auto-check needed).
    pub async fn complete(&self, note: Option<String>) -> Result<Option<GoalState>> {
        self.transition_active(|g| {
            g.status = GoalStatus::Achieved;
            g.append_history(HistoryKind::Completed, note, None, None);
            Ok(())
        })
        .await
    }

    /// Test setup only; production blocks through the stall guard in `skeptic_pass`.
    #[cfg(test)]
    pub async fn block(&self, reason: String) -> Result<Option<GoalState>> {
        self.transition_active(|g| {
            g.status = GoalStatus::Blocked {
                reason: reason.clone(),
            };
            g.append_history(HistoryKind::Blocked, Some(reason.clone()), None, None);
            Ok(())
        })
        .await
    }

    /// Set the skeptic cadence on the active goal.
    ///
    /// The only producer of the field, and it is **human**: `/goal skeptic
    /// on|off`. No model action goes through here — a goal is neither created
    /// nor modified from an action (spec O-2), and a model that could turn
    /// off its own reviewer would no longer have one.
    ///
    /// **Does not write a history entry**, and this is deliberate:
    /// `HistoryKind::Skeptic` means "a pass took place", and nothing else.
    /// Storing setting changes there too would make "the skeptic reread the
    /// report" and "someone turned the skeptic off" indistinguishable — for a
    /// human rereading, and for the test that requires a canonical turn to
    /// produce a pass. The setting is read in `/goal status`, which displays
    /// it.
    pub async fn set_skeptic(&self, cadence: SkepticCadence) -> Result<Option<GoalState>> {
        self.transition_active(move |g| {
            g.skeptic = cadence;
            Ok(())
        })
        .await
    }

    /// Drop the active pointer and delete the goal file.
    pub async fn clear(&self) -> Result<Option<String>> {
        let store = self.store.lock().await;
        let active = store.read_active_id()?;
        if let Some(ref id) = active {
            store.delete(id)?;
        }
        store.clear_active()?;
        Ok(active)
    }

    /// One iteration: increments steps, optionally runs the achievement
    /// check via the downstream runtime, persists, returns the outcome.
    ///
    /// Budget enforcement happens before any check runs — exhausted goals
    /// are recorded and short-circuited so the caller never sees a stale
    /// "Active" status.
    pub async fn step(
        &self,
        downstream: Option<&Mutex<ChildMcpClient>>,
        note: Option<String>,
        plan_id: Option<String>,
    ) -> Result<Option<StepOutcome>> {
        // Phase 1: load + budget check + step counter, all under store lock.
        let mut goal = {
            let store = self.store.lock().await;
            let Some(g) = store.get_active()? else {
                return Ok(None);
            };
            g
        };

        if !goal.status.is_actionable() {
            return Ok(Some(StepOutcome {
                state: goal,
                achievement_succeeded: false,
                achievement_note: Some("goal is not active".into()),
            }));
        }

        goal.budget.steps_used += 1;
        goal.append_history(HistoryKind::Step, note, plan_id, None);

        if let Some(reason) = goal.budget.check_exhausted() {
            goal.status = GoalStatus::BudgetExhausted;
            goal.append_history(
                HistoryKind::BudgetExhausted,
                Some(format!("budget axis exhausted: {reason}")),
                None,
                None,
            );
            self.persist(&goal).await?;
            return Ok(Some(StepOutcome {
                state: goal,
                achievement_succeeded: false,
                achievement_note: Some(format!("budget exhausted ({reason})")),
            }));
        }

        // Phase 2: optional achievement check.
        let (succeeded, check_note) = match (&goal.achievement_check, downstream) {
            (Some(check), Some(d)) => match run_achievement_check(check, d).await {
                Ok((ok, note)) => (ok, Some(note)),
                Err(e) => (false, Some(format!("check_error: {e}"))),
            },
            (Some(_), None) => (false, Some("no downstream runtime to run check".into())),
            (None, _) => (false, None),
        };

        if let Some(note) = &check_note {
            goal.append_history(
                HistoryKind::AchievementCheck,
                None,
                None,
                Some(note.clone()),
            );
        }

        if succeeded {
            goal.status = GoalStatus::Achieved;
            goal.append_history(HistoryKind::Achieved, None, None, None);
        }

        self.persist(&goal).await?;
        Ok(Some(StepOutcome {
            state: goal,
            achievement_succeeded: succeeded,
            achievement_note: check_note,
        }))
    }

    /// The second pass: have the turn's report reread by a skeptic, record
    /// what it named, and stop the goal when it names exactly the same gaps
    /// twice in a row.
    ///
    /// Returns `Ok(None)` — **without calling the model** — when there is no
    /// active goal, or when the goal is no longer active: a goal that is not
    /// advancing has no report to reread, and a second model call is paid
    /// for.
    ///
    /// SECURITY: a skeptic that fails (transport, cancellation, unreadable
    /// JSON) returns a REFUTED verdict, never a validated report — repository
    /// invariant 1. And a refuted verdict without a citation has no
    /// fingerprint, so two failures in a row do not stop a healthy goal.
    ///
    /// The budget is not touched: blocking on a stall stops, it does not
    /// spend. `step()` remains the only place that increments a step.
    pub async fn skeptic_pass(
        &self,
        oracle: &dyn SkepticOracle,
        report: &str,
    ) -> Result<Option<SkepticOutcome>> {
        let mut goal = {
            let store = self.store.lock().await;
            let Some(g) = store.get_active()? else {
                return Ok(None);
            };
            g
        };
        if !goal.status.is_actionable() {
            return Ok(None);
        }
        // The cadence is read HERE, before the model call: this is the only
        // place that has loaded the goal, and a second call is paid for.
        // Reading it after the call would amount to not reading it.
        if !goal.skeptic.runs_this_turn() {
            return Ok(None);
        }

        let prompt = skeptic_prompt(&goal.text, report);
        // `Err` -> `SkepticVerdict::failed()`: the failure is not propagated
        // as an error, because a caller doing `?` or `unwrap_or_default`
        // would turn the failure into "nothing to report".
        let verdict = match oracle.review(&prompt).await {
            Ok(raw) => SkepticVerdict::parse(&raw),
            Err(e) => {
                tracing::warn!("skeptic unavailable, counted as refuted: {e}");
                SkepticVerdict::failed()
            }
        };

        let fingerprint = verdict.fingerprint();
        let stalled = goal.record_gap_fingerprint(&fingerprint);

        goal.append_history(
            HistoryKind::Skeptic,
            Some(format!("[{}] {}", oracle.model_label(), verdict.summary())),
            None,
            None,
        );

        if stalled {
            let reason = format!(
                "the skeptic named the same gaps {} times in a row; \
                 the goal is not progressing. Gaps: {}",
                goal.gap_repeat_count,
                verdict.summary()
            );
            goal.status = GoalStatus::Blocked {
                reason: reason.clone(),
            };
            goal.append_history(HistoryKind::Blocked, Some(reason), None, None);
        }

        self.persist(&goal).await?;
        Ok(Some(SkepticOutcome {
            state: goal,
            verdict,
            fingerprint,
            stalled,
        }))
    }

    /// Internal helper: load active, mutate via `f`, persist.
    async fn transition_active<F>(&self, f: F) -> Result<Option<GoalState>>
    where
        F: FnOnce(&mut GoalState) -> Result<()>,
    {
        let store = self.store.lock().await;
        let Some(mut goal) = store.get_active()? else {
            return Ok(None);
        };
        f(&mut goal)?;
        goal.touch();
        store.save(&goal)?;
        Ok(Some(goal))
    }

    async fn persist(&self, goal: &GoalState) -> Result<()> {
        let store = self.store.lock().await;
        store.save(goal)
    }
}

/// Execute the achievement check against the downstream runtime. Returns
/// `(succeeded, human_readable_note)`. We accept the LLM-friendly action
/// shape (PascalCase `type`) since the runtime accepts both.
async fn run_achievement_check(
    check: &AchievementCheck,
    downstream: &Mutex<ChildMcpClient>,
) -> Result<(bool, String)> {
    match check {
        AchievementCheck::Manual => Ok((false, "manual check — no auto-completion".into())),
        AchievementCheck::FileExists { path } => {
            let resp = call_downstream(downstream, "file_info", json!({ "path": path })).await?;
            // file_info returns success=true with an output field when the file
            // exists; success=false (NotFound) otherwise.
            let ok = parse_runtime_success(&resp);
            Ok((ok, format!("file_exists({path}) -> {ok}")))
        }
        AchievementCheck::FileContains { path, needle } => {
            let resp = call_downstream(downstream, "read_file", json!({ "path": path })).await?;
            // Extract the file contents from the runtime's text envelope.
            let body = extract_text(&resp).unwrap_or_default();
            // The runtime serializes the result as a JSON envelope; the
            // file content is in output.content. Best-effort parse: if we
            // can pull a JSON object with a `content` string, search that;
            // else fall back to substring match on the raw envelope.
            let haystack = match serde_json::from_str::<Value>(&body) {
                Ok(v) => v
                    .get("output")
                    .and_then(|o| o.get("content"))
                    .and_then(|c| c.as_str())
                    .map(String::from)
                    .unwrap_or(body),
                Err(_) => body,
            };
            let ok = haystack.contains(needle);
            Ok((ok, format!("file_contains({path}, …) -> {ok}")))
        }
        AchievementCheck::Command {
            command,
            args,
            cwd,
            expected_exit,
            timeout_secs,
        } => {
            let mut payload = json!({
                "command": command,
                "args": args,
            });
            if let Some(c) = cwd {
                payload["cwd"] = json!(c);
            }
            if let Some(t) = timeout_secs {
                payload["timeout_secs"] = json!(t);
            }
            let resp = call_downstream(downstream, "run_command", payload).await?;
            let exit_code = extract_exit_code(&resp);
            let ok = exit_code == Some(*expected_exit);
            Ok((
                ok,
                format!(
                    "command({command}) exit={} expected={} -> {}",
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    expected_exit,
                    ok
                ),
            ))
        }
        AchievementCheck::Action { action } => {
            // Forward as a single-step pipeline so the runtime treats it
            // identically to any other agent-issued action.
            let resp =
                call_downstream(downstream, "pipeline", json!({ "steps": [action] })).await?;
            let ok = parse_runtime_success(&resp);
            Ok((ok, format!("custom_action -> {ok}")))
        }
    }
}

async fn call_downstream(
    downstream: &Mutex<ChildMcpClient>,
    tool: &str,
    args: Value,
) -> Result<Value> {
    let mut c = downstream.lock().await;
    c.tools_call(tool, args).await
}

/// Extract the JSON-stringified text body from an MCP `tools/call` result.
fn extract_text(resp: &Value) -> Option<String> {
    resp.get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .map(String::from)
}

/// Inspect a runtime tools/call result and return whether the underlying
/// action reported `success: true`. The runtime puts the typed envelope
/// inside `content[0].text` as a JSON string.
fn parse_runtime_success(resp: &Value) -> bool {
    if let Some(text) = extract_text(resp) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            return parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        }
    }
    false
}

/// Pull `output.exit_code` (or top-level `exit_code` for legacy shapes)
/// out of a runtime response. `None` if not parseable as i32.
fn extract_exit_code(resp: &Value) -> Option<i32> {
    let text = extract_text(resp)?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    if let Some(c) = parsed.pointer("/output/exit_code").and_then(|v| v.as_i64()) {
        return i32::try_from(c).ok();
    }
    parsed
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .and_then(|c| i32::try_from(c).ok())
}

/// Wrap the manager in an `Arc` so `OrchestratorMcpServer` can share it
/// cheaply across handlers.
pub type SharedGoalManager = Arc<GoalManager>;

#[cfg(test)]
// `pub(crate)` for a single consumer: `agent::session` mounts a real manager
// on a tempdir to measure a canonical turn, rather than a twin that would
// drift. Nothing leaves it: the module stays `cfg(test)`.
pub(crate) mod tests {
    use super::*;
    use crate::goal::store::GoalStore;

    pub(crate) fn make_manager() -> (tempfile::TempDir, GoalManager) {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::with_root(dir.path()).unwrap();
        (dir, GoalManager::new(store))
    }

    #[tokio::test]
    async fn set_then_get_active_returns_the_goal() {
        let (_dir, m) = make_manager();
        let g = m
            .set(
                "ship the auth refactor".into(),
                Budget::new(Some(5), None),
                None,
            )
            .await
            .unwrap();
        let active = m.get_active().await.unwrap().unwrap();
        assert_eq!(active.id, g.id);
        assert_eq!(active.text, "ship the auth refactor");
    }

    #[tokio::test]
    async fn pause_then_resume_round_trip() {
        let (_dir, m) = make_manager();
        m.set("test".into(), Budget::new(None, None), None)
            .await
            .unwrap();

        let after_pause = m.pause().await.unwrap().unwrap();
        assert_eq!(after_pause.status, GoalStatus::Paused);

        let after_resume = m.resume().await.unwrap().unwrap();
        assert_eq!(after_resume.status, GoalStatus::Active);
    }

    /// `resume` puts the goal back in motion but EXECUTES NO STEP.
    ///
    /// SECURITY: invariant 1 applied over the long run. Resuming on its own
    /// after a pause is an approval by default: nobody has said "continue"
    /// since something stopped. A failure would mean that a goal blocked for
    /// repeated gaps restarts without a human having looked at why it had
    /// blocked.
    #[tokio::test]
    async fn resume_makes_the_goal_actionable_without_taking_a_step() {
        let (_dir, m) = make_manager();
        m.set("t".into(), Budget::new(Some(9), None), None)
            .await
            .unwrap();
        m.step(None, Some("one".into()), None).await.unwrap();
        m.pause().await.unwrap();

        let resumed = m.resume().await.unwrap().expect("a paused goal resumes");
        assert!(matches!(resumed.status, GoalStatus::Active));
        assert_eq!(
            resumed.budget.steps_used, 1,
            "resume consumed a step: the resumption ran on its own"
        );
    }

    /// A goal stopped by the stall guard is lifted by hand, and is lifted in
    /// the stopped state.
    ///
    /// A failure would mean that a `Blocked` — the state where the skeptic
    /// named the same gaps twice — restarts by advancing one step, i.e. the
    /// only thing that had stopped the loop has just restarted it.
    #[tokio::test]
    async fn resuming_a_blocked_goal_takes_no_step_either() {
        let (_dir, m) = make_manager();
        m.set("t".into(), Budget::new(Some(9), None), None)
            .await
            .unwrap();
        m.step(None, None, None).await.unwrap();
        m.block("the skeptic named the same gaps twice".into())
            .await
            .unwrap();

        let resumed = m.resume().await.unwrap().expect("a blocked goal resumes");
        assert!(matches!(resumed.status, GoalStatus::Active));
        assert_eq!(
            resumed.budget.steps_used, 1,
            "resuming a blocked goal consumed a step"
        );
    }

    /// The skeptic cadence can be set, and reads back.
    ///
    /// A failure would mean a setting written nowhere: the field exists, the
    /// gate reads it, and nothing can set it — dead scaffolding in its
    /// "configuration" form (docs/invariants.md).
    #[tokio::test]
    async fn the_operator_can_turn_the_skeptic_off_on_the_active_goal() {
        let (_dir, m) = make_manager();
        let fresh = m
            .set("t".into(), Budget::new(Some(9), None), None)
            .await
            .unwrap();
        assert_eq!(
            fresh.skeptic,
            SkepticCadence::EveryTurn,
            "the O-2 spec default is \"every turn\""
        );

        let off = m
            .set_skeptic(SkepticCadence::Off)
            .await
            .unwrap()
            .expect("an active goal accepts the setting");
        assert_eq!(off.skeptic, SkepticCadence::Off);
        // Reread from disk, not from the returned value.
        let reread = m.get_active().await.unwrap().unwrap();
        assert_eq!(reread.skeptic, SkepticCadence::Off);
    }

    #[tokio::test]
    async fn resume_errors_on_terminal_status() {
        let (_dir, m) = make_manager();
        m.set("test".into(), Budget::new(None, None), None)
            .await
            .unwrap();
        m.complete(Some("done".into())).await.unwrap();
        assert!(m.resume().await.is_err());
    }

    #[tokio::test]
    async fn step_increments_counter_and_records_history() {
        let (_dir, m) = make_manager();
        m.set("test".into(), Budget::new(Some(3), None), None)
            .await
            .unwrap();
        let outcome = m
            .step(None, Some("first".into()), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.state.budget.steps_used, 1);
        assert!(matches!(outcome.state.status, GoalStatus::Active));
        // Created + Step entries.
        assert!(outcome.state.history.len() >= 2);
    }

    #[tokio::test]
    async fn step_marks_budget_exhausted_at_limit() {
        let (_dir, m) = make_manager();
        m.set("test".into(), Budget::new(Some(2), None), None)
            .await
            .unwrap();
        m.step(None, None, None).await.unwrap().unwrap();
        let outcome = m.step(None, None, None).await.unwrap().unwrap();
        assert_eq!(outcome.state.status, GoalStatus::BudgetExhausted);
        assert_eq!(outcome.state.budget.steps_used, 2);
    }

    #[tokio::test]
    async fn step_returns_none_when_no_active_goal() {
        let (_dir, m) = make_manager();
        let outcome = m.step(None, None, None).await.unwrap();
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn step_no_op_when_paused() {
        let (_dir, m) = make_manager();
        m.set("test".into(), Budget::new(Some(5), None), None)
            .await
            .unwrap();
        m.pause().await.unwrap();
        let outcome = m.step(None, None, None).await.unwrap().unwrap();
        assert_eq!(outcome.state.status, GoalStatus::Paused);
        assert_eq!(outcome.state.budget.steps_used, 0);
    }

    #[tokio::test]
    async fn clear_drops_goal_and_active_pointer() {
        let (_dir, m) = make_manager();
        let g = m
            .set("test".into(), Budget::new(None, None), None)
            .await
            .unwrap();
        let cleared_id = m.clear().await.unwrap();
        assert_eq!(cleared_id.as_deref(), Some(g.id.as_str()));
        assert!(m.get_active().await.unwrap().is_none());
    }
}
