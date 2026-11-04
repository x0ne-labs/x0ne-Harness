use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::handles::SharedHandles;
use crate::policy::PolicyEngine;
use crate::result::{ActionOutput, ActionResult, ErrorKind, StepResult};

use super::desktop::RateLimiter;
use super::snapshot;

/// What actually happened when a `Try` tried to undo itself.
///
/// A partial restore fails AND carries its counts in the payload (HA-02):
/// both are read, so "restored 3, failed 1" is not collapsed to "failed".
///
/// Rollback success means the intended state restoration actually happened.
/// Anything less is reported as what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackOutcome {
    /// No rollback was attempted — the block succeeded, or none was requested.
    NotAttempted,
    /// Every file came back.
    Restored,
    /// Some files came back and some did not. The tree is in neither state.
    Partial { restored: usize, failed: usize },
    /// Nothing came back, or the restore itself errored.
    Failed { failed: usize },
}

impl RollbackOutcome {
    /// Whether the caller can say what state the filesystem is in.
    ///
    /// This is the distinction that matters operationally: "the action failed
    /// but I undid it" is recoverable; "the action failed and I could not undo
    /// it" needs a human, now.
    pub fn state_is_known(&self) -> bool {
        matches!(
            self,
            RollbackOutcome::NotAttempted | RollbackOutcome::Restored
        )
    }

    pub fn message(&self) -> String {
        match self {
            RollbackOutcome::NotAttempted => "no rollback was attempted".into(),
            RollbackOutcome::Restored => "changes were rolled back".into(),
            RollbackOutcome::Partial { restored, failed } => format!(
                "ROLLBACK INCOMPLETE: {restored} file(s) restored, {failed} failed. The \
                 working tree is in an unknown state — inspect it before continuing."
            ),
            RollbackOutcome::Failed { failed } => format!(
                "ROLLBACK FAILED: {failed} file(s) could not be restored. The working tree \
                 is in an unknown state — inspect it before continuing."
            ),
        }
    }
}

/// Read the restore result's payload rather than its success flag.
fn rollback_outcome(result: &ActionResult) -> RollbackOutcome {
    match result.output.as_ref() {
        Some(ActionOutput::SnapshotRestored {
            files_restored,
            files_failed,
            ..
        }) => match (*files_restored, *files_failed) {
            (_, 0) if result.success => RollbackOutcome::Restored,
            (_, 0) => RollbackOutcome::Failed { failed: 0 },
            (0, f) => RollbackOutcome::Failed { failed: f },
            (r, f) => RollbackOutcome::Partial {
                restored: r,
                failed: f,
            },
        },
        // A restore that reported success without the expected payload cannot
        // be shown to have restored anything.
        _ => RollbackOutcome::Failed { failed: 0 },
    }
}

/// Did this step *verify*, as opposed to merely *complete*?
///
/// `Try` and `SafeChange` promise to undo their changes when the change does
/// not hold up. Deciding that on `ActionResult.success` alone does not work,
/// because several primitives report semantic failure inside a successful
/// envelope: a `RunCommand` that exits 1 ran fine, a `SafeTest` whose suite
/// fails ran fine, an `Assert` that evaluates false ran fine. Reading only
/// `.success`, a failing test suite committed the change and deleted the
/// snapshot.
///
/// So the two questions are separated here rather than by redefining what
/// `ActionResult.success` means globally — `RunCommand` has callers outside
/// any transaction for which "exit 1" is an ordinary, non-failing outcome
/// (`grep` finding nothing), and changing that contract would break them.
/// Inside a transaction the interpretation is different, and this is where
/// that interpretation belongs.
///
/// Anything carrying no verdict is judged on execution alone.
fn step_verified(result: &ActionResult) -> bool {
    if !result.success {
        return false;
    }
    match result.output.as_ref() {
        Some(ActionOutput::CommandOutput { exit_code, .. }) => *exit_code == 0,
        Some(ActionOutput::AssertResult { ok, .. }) => *ok,
        // A nested Try that rolled back must not read as success upward, or
        // the outer transaction commits over a reverted inner one.
        Some(ActionOutput::TryResult { ok, .. }) => *ok,
        // `SafeTest` reports through the generic envelope.
        Some(ActionOutput::Generic { data }) => data
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|c| c == 0)
            .unwrap_or(true),
        _ => true,
    }
}

/// Execute a Try block: snapshot → execute steps → rollback on failure.
///
/// Try is like Pipeline but with built-in safety:
/// 1. Automatically snapshots files that will be written
/// 2. Executes steps sequentially
/// 3. Rolls back on failure (if rollback_on_fail)
/// 4. Cleans up the snapshot on success (unless keep_snapshot)
pub async fn execute_try(
    actions: &[Action],
    rollback_on_fail: bool,
    label: &Option<String>,
    keep_snapshot: bool,
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    let total = actions.len();
    let label_str = label.as_deref().unwrap_or("");

    // ── Generate snapshot name ────────────────────────────────────────────────
    // A monotonic per-process counter is appended because the timestamp has
    // 1-second granularity: two Try blocks in the same second would otherwise
    // derive the same snapshot name and collide. That used to degrade silently;
    // now that snapshot failure is fail-closed (below) a collision would refuse
    // to run an otherwise valid block, so the name must be unique.
    static TRY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TRY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ts = chrono::Utc::now().timestamp();
    let snap_name = if label_str.is_empty() {
        format!("try__auto__{}__{}", ts, seq)
    } else {
        let safe: String = label_str.replace(' ', "_").chars().take(32).collect();
        format!("try__{}__{}__{}", safe, ts, seq)
    };

    // ── Collect write paths for snapshotting ──────────────────────────────────
    let write_paths = collect_write_paths(actions);
    let mut did_snapshot = false;

    if !write_paths.is_empty() {
        match snapshot::create_snapshot(&snap_name, &write_paths, config).await {
            Ok(_) => {
                did_snapshot = true;
            }
            // FAIL CLOSED. Previously the error was swallowed and the block ran
            // its writes anyway, reporting `rolled_back: false` with no
            // indication the guarantee had been lost. Because snapshot creation
            // hard-errors once `max_snapshots` is reached, a few failed runs
            // could silently disarm rollback for every subsequent Try/SafeChange
            // in the session — exactly the promise this primitive exists to make.
            // Refusing to execute is the only safe outcome: no snapshot, no
            // mutation.
            Err(e) if rollback_on_fail => {
                return ActionResult::err(
                    "Try",
                    ErrorKind::ExecutionFailed,
                    format!(
                        "refusing to run: snapshot of {} write path(s) failed, so rollback \
                         could not be guaranteed — {}. Free space or prune snapshots \
                         (max_snapshots) and retry, or set rollback_on_fail=false to run \
                         unprotected deliberately.",
                        write_paths.len(),
                        e
                    ),
                    0,
                );
            }
            // The caller explicitly opted out of rollback, so running without a
            // snapshot is the documented behaviour rather than a silent loss.
            Err(_) => {}
        }
    }

    // ── Execute steps ─────────────────────────────────────────────────────────
    let mut completed = 0usize;
    let mut results: Vec<StepResult> = Vec::with_capacity(total);
    let mut failed = false;

    for (i, step) in actions.iter().enumerate() {
        let start = std::time::Instant::now();
        // Re-checked at execution time, as Pipeline does. The gate preflights a
        // Try's declared actions, but re-checking here means a step cannot run
        // unpoliced if this block is ever reached by a path that skipped the
        // gate.
        let step_result = super::execute_nested(
            step,
            config,
            handles.clone(),
            policy,
            rate_limiter,
            composites,
        )
        .await;
        let duration_ms = start.elapsed().as_millis() as u64;

        // Execution completed *and* verification held. See `step_verified`.
        let step_success = step_verified(&step_result);
        let output_summary = match (
            step_success,
            step_result.output.as_ref(),
            step_result.error.as_ref(),
        ) {
            (true, Some(o), _) => o.summarize(),
            (false, _, Some(e)) => {
                let m = &e.message;
                if m.chars().count() <= 60 {
                    format!("err: {}", m)
                } else {
                    format!("err: {}…", m.chars().take(60).collect::<String>())
                }
            }
            _ => String::new(),
        };
        results.push(StepResult {
            step: i + 1,
            action_type: step.type_name().to_string(),
            success: step_success,
            output_summary,
            output: step_result.output,
            error: step_result.error,
            duration_ms,
        });

        if step_success {
            completed += 1;
        } else {
            failed = true;
            break;
        }
    }

    // ── Rollback on failure ───────────────────────────────────────────────────
    let rollback = if failed && rollback_on_fail && did_snapshot {
        rollback_outcome(&snapshot::action_restore_snapshot(&snap_name, config).await)
    } else {
        RollbackOutcome::NotAttempted
    };
    let rolled_back = rollback == RollbackOutcome::Restored;
    if !rollback.state_is_known() {
        tracing::error!(snapshot = %snap_name, "{}", rollback.message());
    }

    // ── Cleanup snapshot on success ───────────────────────────────────────────
    if !failed && did_snapshot && !keep_snapshot {
        let _ = snapshot::action_delete_snapshot(&snap_name, config).await;
    }

    // A rollback that did not fully succeed leaves the filesystem in a state
    // nobody can name. That is a different and worse outcome than "the block
    // failed and I undid it", so it is surfaced as an error rather than folded
    // into `ok: false` — the caller has to notice, and the snapshot is kept so
    // the operator can finish the job by hand.
    if !rollback.state_is_known() {
        return ActionResult::err(
            "Try",
            ErrorKind::ExecutionFailed,
            format!(
                "{} The snapshot `{}` has been kept so the restore can be retried.",
                rollback.message(),
                snap_name
            ),
            0,
        );
    }

    let ok = !failed;
    ActionResult::ok(
        "Try",
        ActionOutput::TryResult {
            ok,
            completed,
            total,
            rolled_back,
            label: label.clone(),
            snapshot_name: if did_snapshot { Some(snap_name) } else { None },
            results,
        },
        0,
    )
}

pub async fn execute_safe_change(
    steps: &[Action],
    tests: &[Action],
    rollback_on_fail: bool,
    label: &Option<String>,
    keep_snapshot: bool,
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    let mut actions = Vec::with_capacity(steps.len() + tests.len());
    actions.extend_from_slice(steps);
    actions.extend_from_slice(tests);

    let safe_label = label.clone().or_else(|| Some("safe-change".into()));
    let mut result = execute_try(
        &actions,
        rollback_on_fail,
        &safe_label,
        keep_snapshot,
        config,
        policy,
        handles,
        rate_limiter,
        composites,
    )
    .await;
    result.action_type = "SafeChange".into();
    result
}

/// Snapshot targets for these actions.
///
/// Delegates to `x0ne_actions::write_paths` so the runtime and the
/// orchestrator cannot disagree about what a `Try` protects — two
/// hand-maintained copies is exactly how the previous drift happened.
fn collect_write_paths(actions: &[Action]) -> Vec<String> {
    x0ne_actions::write_paths(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite;
    use crate::handles;

    /// Regression (August 2026 audit, robustness finding 1): `collect_write_paths`
    /// recursed into `SafeChange` but NOT into `Pipeline`, `Try` or `If`, so a
    /// `Try { steps: [Pipeline { WriteFile }] }` snapshotted ZERO paths and ran
    /// the writes with no rollback capability — silently.
    #[test]
    fn collect_write_paths_recurses_into_nested_composites() {
        let nested_pipeline = Action::Pipeline {
            steps: vec![Action::WriteFile {
                path: "/tmp/in_pipeline.txt".into(),
                content: "x".into(),
            }],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let paths = collect_write_paths(&[nested_pipeline]);
        assert!(
            paths.contains(&"/tmp/in_pipeline.txt".to_string()),
            "writes inside a nested Pipeline must be snapshotted, got {paths:?}"
        );

        let nested_try = Action::Try {
            actions: vec![Action::WriteFile {
                path: "/tmp/in_try.txt".into(),
                content: "x".into(),
            }],
            rollback_on_fail: true,
            label: None,
            keep_snapshot: false,
        };
        let paths = collect_write_paths(&[nested_try]);
        assert!(
            paths.contains(&"/tmp/in_try.txt".to_string()),
            "writes inside a nested Try must be snapshotted, got {paths:?}"
        );

        let nested_if = Action::If {
            check: "file_exists".into(),
            path: Some("/tmp/probe".into()),
            value: None,
            host: None,
            port: None,
            command: None,
            args: None,
            name: None,
            actual: None,
            expected: None,
            then: vec![Action::WriteFile {
                path: "/tmp/in_then.txt".into(),
                content: "x".into(),
            }],
            else_branch: vec![Action::WriteFile {
                path: "/tmp/in_else.txt".into(),
                content: "x".into(),
            }],
        };
        let paths = collect_write_paths(&[nested_if]);
        assert!(
            paths.contains(&"/tmp/in_then.txt".to_string())
                && paths.contains(&"/tmp/in_else.txt".to_string()),
            "writes in BOTH If branches must be snapshotted, got {paths:?}"
        );
    }

    // ── RED: verification failure must trigger rollback ────────────────────

    fn cmd_result(exit_code: i32) -> ActionResult {
        ActionResult::ok(
            "RunCommand",
            ActionOutput::CommandOutput {
                command: "sh".into(),
                args: vec![],
                exit_code,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                truncated: false,
                omitted_bytes: 0,
                stdout_spill_path: None,
                stderr_spill_path: None,
                spill_refused: None,
            },
            0,
        )
    }

    fn assert_result(ok: bool) -> ActionResult {
        ActionResult::ok(
            "Assert",
            ActionOutput::AssertResult {
                ok,
                check: "command_succeeds".into(),
                detail: String::new(),
            },
            0,
        )
    }

    fn safetest_result(exit_code: i32) -> ActionResult {
        ActionResult::ok(
            "SafeTest",
            ActionOutput::Generic {
                data: serde_json::json!({"exit_code": exit_code}),
            },
            0,
        )
    }

    /// A command that ran and exited non-zero *completed* — but it did not
    /// *verify*. `Try` used to read only `ActionResult.success`, which is
    /// `true` for any command that merely started, so a failing test suite
    /// committed the change instead of rolling it back.
    #[test]
    fn a_non_zero_exit_is_not_a_passing_verification() {
        assert!(step_verified(&cmd_result(0)), "exit 0 verifies");
        assert!(!step_verified(&cmd_result(1)), "exit 1 must not verify");
        assert!(!step_verified(&cmd_result(127)));
    }

    #[test]
    fn a_false_assert_is_not_a_passing_verification() {
        assert!(step_verified(&assert_result(true)));
        assert!(!step_verified(&assert_result(false)));
    }

    #[test]
    fn a_failing_safetest_is_not_a_passing_verification() {
        assert!(step_verified(&safetest_result(0)));
        assert!(!step_verified(&safetest_result(1)));
    }

    /// A nested Try that rolled back must not report upward as a success, or
    /// the outer transaction commits over a reverted inner one.
    #[test]
    fn a_failed_nested_try_does_not_verify() {
        let inner = ActionResult::ok(
            "Try",
            ActionOutput::TryResult {
                ok: false,
                completed: 1,
                total: 2,
                rolled_back: true,
                label: None,
                snapshot_name: None,
                results: vec![],
            },
            0,
        );
        assert!(!step_verified(&inner), "a failed inner Try must propagate");
    }

    /// Anything that does not carry a verification verdict is judged on
    /// execution alone — a successful `WriteFile` is a successful step.
    #[test]
    fn actions_without_a_verdict_are_judged_on_execution() {
        let write = ActionResult::ok(
            "WriteFile",
            ActionOutput::Generic {
                data: serde_json::json!({"bytes_written": 12}),
            },
            0,
        );
        assert!(step_verified(&write));
        let failed = ActionResult::err("WriteFile", ErrorKind::ExecutionFailed, "nope", 0);
        assert!(!step_verified(&failed));
    }

    // ── RED: rollback failure must be observable ──────────────────────────

    fn restore_result(restored: usize, failed: usize) -> ActionResult {
        ActionResult::ok(
            "RestoreSnapshot",
            ActionOutput::SnapshotRestored {
                name: "snap".into(),
                files_restored: restored,
                files_failed: failed,
                files_removed: 0,
                details: vec![],
                method: None,
                scope: None,
            },
            0,
        )
    }

    /// `action_restore_snapshot` always returns `ActionResult::ok`;
    /// `files_failed` lives in the payload. Reading only `.success`, a restore
    /// that recovered nothing reported `rolled_back: true`.
    #[test]
    fn rollback_outcome_reflects_what_was_actually_restored() {
        assert_eq!(
            rollback_outcome(&restore_result(4, 0)),
            RollbackOutcome::Restored
        );
        assert_eq!(
            rollback_outcome(&restore_result(3, 1)),
            RollbackOutcome::Partial {
                restored: 3,
                failed: 1
            },
            "a partial restore must not read as a full one"
        );
        assert_eq!(
            rollback_outcome(&restore_result(0, 4)),
            RollbackOutcome::Failed { failed: 4 },
            "restoring nothing is not a rollback"
        );
    }

    /// A restore that errors outright is a failed rollback, not a missing one.
    #[test]
    fn an_errored_restore_is_a_failed_rollback() {
        let err = ActionResult::err("RestoreSnapshot", ErrorKind::ExecutionFailed, "disk", 0);
        assert!(matches!(
            rollback_outcome(&err),
            RollbackOutcome::Failed { .. }
        ));
    }

    /// The distinction the user and the model need: "action failed, rollback
    /// succeeded" is recoverable; "action failed, rollback also failed" means
    /// the working tree is in an unknown state and must be reported as such.
    #[test]
    fn a_failed_rollback_is_reported_distinctly_from_a_successful_one() {
        assert!(RollbackOutcome::Restored.state_is_known());
        assert!(!RollbackOutcome::Failed { failed: 2 }.state_is_known());
        assert!(
            !RollbackOutcome::Partial {
                restored: 1,
                failed: 1
            }
            .state_is_known(),
            "a partially restored tree is not a known state"
        );
        assert!(RollbackOutcome::Restored.message().contains("rolled back"));
        assert!(RollbackOutcome::Failed { failed: 2 }
            .message()
            .contains("unknown"));
    }

    #[tokio::test]
    async fn try_success_returns_ok() {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);

        let actions = vec![Action::ListSnapshots];

        let result = execute_try(
            &actions,
            true,
            &Some("test".into()),
            false,
            &config,
            &policy,
            handles,
            &rate_limiter,
            &composites,
        )
        .await;

        assert!(result.success);
        if let Some(ActionOutput::TryResult {
            ok,
            completed,
            total,
            ..
        }) = result.output
        {
            assert!(ok);
            assert_eq!(completed, 1);
            assert_eq!(total, 1);
        } else {
            panic!("Expected TryResult");
        }
    }

    #[tokio::test]
    async fn safe_change_success_returns_safechange_type() {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);

        let steps = vec![Action::ListSnapshots];
        let tests = vec![Action::ListSnapshots];

        let result = execute_safe_change(
            &steps,
            &tests,
            true,
            &Some("test".into()),
            false,
            &config,
            &policy,
            handles,
            &rate_limiter,
            &composites,
        )
        .await;

        assert!(result.success);
        assert_eq!(result.action_type, "SafeChange");
        if let Some(ActionOutput::TryResult {
            ok,
            completed,
            total,
            ..
        }) = result.output
        {
            assert!(ok);
            assert_eq!(completed, 2);
            assert_eq!(total, 2);
        } else {
            panic!("Expected TryResult");
        }
    }
}
