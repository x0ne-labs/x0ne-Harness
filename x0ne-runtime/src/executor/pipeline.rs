use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::executor::desktop::RateLimiter;
use crate::handles::SharedHandles;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionOutput, ActionResult, ErrorKind, StepResult};

/// Maximum nesting depth for pipelines-in-pipelines.
/// depth 0 = top-level, depth 1 = nested once, etc.
pub const MAX_PIPELINE_DEPTH: usize = 3;

pub async fn execute_pipeline(
    steps: &[Action],
    stop_on_error: bool,
    parallel: bool,
    snapshot_before: &Option<String>,
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
    depth: usize,
) -> ActionResult {
    // ── Max steps guard ──────────────────────────────────────────────────────
    const MAX_PIPELINE_STEPS: usize = 20;
    if steps.len() > MAX_PIPELINE_STEPS {
        return ActionResult::err(
            "Pipeline",
            ErrorKind::InvalidInput,
            format!(
                "Pipeline has {} steps (max {}). Split into smaller pipelines.",
                steps.len(),
                MAX_PIPELINE_STEPS
            ),
            0,
        );
    }

    // ── Depth guard ───────────────────────────────────────────────────────────
    if depth >= MAX_PIPELINE_DEPTH {
        return ActionResult::err(
            "Pipeline",
            ErrorKind::NestingNotAllowed,
            format!(
                "Pipeline nesting too deep (depth {}, max {}). Flatten your pipeline.",
                depth, MAX_PIPELINE_DEPTH
            ),
            0,
        );
    }

    // ── Policy preflight: check ALL steps before executing ANY ───────────────
    // Both Deny AND RequireConfirmation block a pipeline — there's no interactive
    // confirmation possible in automated pipeline context.
    for (i, step) in steps.iter().enumerate() {
        match policy.evaluate(step) {
            PolicyEffect::Allow => {}
            PolicyEffect::Deny(reason) => {
                return ActionResult::err(
                    "Pipeline",
                    ErrorKind::PolicyDenied,
                    format!(
                        "Pipeline rejected before execution: step {} ({}) denied — {}",
                        i,
                        step.type_name(),
                        reason.message()
                    ),
                    0,
                );
            }
            PolicyEffect::RequireConfirmation(_) => {
                return ActionResult::err(
                    "Pipeline",
                    ErrorKind::PolicyDenied,
                    format!(
                        "Pipeline rejected before execution: step {} ({}) requires confirmation — use standalone action with confirm mode",
                        i, step.type_name()
                    ),
                    0,
                );
            }
        }
    }

    // ── Optional snapshot ─────────────────────────────────────────────────────
    let mut taken_snapshot: Option<String> = None;
    if let Some(snap_name) = snapshot_before {
        // Collect write paths from the pipeline for auto-snapshotting
        let paths: Vec<String> = steps.iter().flat_map(write_paths_of).collect();
        if !paths.is_empty() {
            match super::snapshot::create_snapshot(snap_name, &paths, config).await {
                Ok(name) => taken_snapshot = Some(name),
                Err(e) => {
                    return ActionResult::err(
                        "Pipeline",
                        ErrorKind::SnapshotError,
                        format!("Pre-pipeline snapshot '{}' failed: {}", snap_name, e),
                        0,
                    );
                }
            }
        }
    }

    // ── Execute steps ─────────────────────────────────────────────────────────
    if parallel {
        execute_parallel(
            steps,
            config,
            policy,
            handles,
            rate_limiter,
            taken_snapshot,
            composites,
            depth,
        )
        .await
    } else {
        execute_sequential(
            steps,
            stop_on_error,
            config,
            policy,
            handles,
            rate_limiter,
            taken_snapshot,
            composites,
            depth,
        )
        .await
    }
}

/// Sequential pipeline execution (original behavior).
async fn execute_sequential(
    steps: &[Action],
    stop_on_error: bool,
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    taken_snapshot: Option<String>,
    composites: &CompositeStore,
    depth: usize,
) -> ActionResult {
    let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());
    let mut steps_completed = 0usize;
    let mut stopped_early = false;

    for (i, step) in steps.iter().enumerate() {
        let start = std::time::Instant::now();
        let ar = super::execute_action_at_depth(
            step,
            config,
            handles.clone(),
            policy,
            rate_limiter,
            composites,
            depth + 1,
        )
        .await;
        let duration_ms = start.elapsed().as_millis() as u64;

        let success = ar.success;
        // T2: pre-compute the per-step output_summary so consumers (LLM,
        // TUI, exports) get a one-line view of what each step produced
        // without re-deriving it from the full ActionOutput payload.
        let output_summary = match (success, ar.output.as_ref(), ar.error.as_ref()) {
            (true, Some(o), _) => o.summarize(),
            (false, _, Some(e)) => format!("err: {}", truncate_err(&e.message, 60)),
            _ => String::new(),
        };
        results.push(StepResult {
            step: i,
            action_type: step.type_name().to_string(),
            success,
            output_summary,
            output: ar.output,
            error: ar.error,
            duration_ms,
        });

        if success {
            steps_completed += 1;
        } else {
            steps_completed += 1; // count it as attempted
            if stop_on_error {
                stopped_early = i + 1 < steps.len();
                break;
            }
        }
    }

    // T2 — kill the lying footer: a Pipeline whose steps all succeeded is
    // a success; otherwise the Pipeline as a whole has failed and we say
    // so explicitly via ActionResult::err. Previously the Pipeline always
    // reported success=true even when half its steps had blown up, which
    // confused both the LLM and the TUI's denial renderer.
    let any_failed = results.iter().any(|r| !r.success);
    if any_failed || stopped_early {
        let payload = ActionOutput::PipelineResult {
            steps_total: steps.len(),
            steps_completed,
            stopped_early,
            results,
            snapshot_name: taken_snapshot,
        };
        let mut err = ActionResult::err(
            "Pipeline",
            ErrorKind::ExecutionFailed,
            if stopped_early {
                format!(
                    "Pipeline stopped after {}/{} steps (one step failed and stop_on_error=true).",
                    steps_completed,
                    steps.len()
                )
            } else {
                format!(
                    "Pipeline finished with {}/{} steps failed.",
                    results_failed_count(&payload),
                    steps.len()
                )
            },
            0,
        );
        // Even on failure we attach the per-step results so the model can
        // see exactly which steps passed and which didn't.
        err.output = Some(payload);
        return err;
    }

    ActionResult::ok(
        "Pipeline",
        ActionOutput::PipelineResult {
            steps_total: steps.len(),
            steps_completed,
            stopped_early,
            results,
            snapshot_name: taken_snapshot,
        },
        0,
    )
}

fn truncate_err(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

fn results_failed_count(payload: &ActionOutput) -> usize {
    if let ActionOutput::PipelineResult { results, .. } = payload {
        results.iter().filter(|r| !r.success).count()
    } else {
        0
    }
}

/// Parallel pipeline execution — all steps run concurrently via join_all.
async fn execute_parallel(
    steps: &[Action],
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    taken_snapshot: Option<String>,
    composites: &CompositeStore,
    depth: usize,
) -> ActionResult {
    use futures::future::join_all;

    let start = std::time::Instant::now();
    let futures: Vec<_> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let h = handles.clone();
            async move {
                let step_start = std::time::Instant::now();
                let ar = super::execute_action_at_depth(
                    step,
                    config,
                    h,
                    policy,
                    rate_limiter,
                    composites,
                    depth + 1,
                )
                .await;
                let duration_ms = step_start.elapsed().as_millis() as u64;
                let output_summary = match (ar.success, ar.output.as_ref(), ar.error.as_ref()) {
                    (true, Some(o), _) => o.summarize(),
                    (false, _, Some(e)) => format!("err: {}", truncate_err(&e.message, 60)),
                    _ => String::new(),
                };
                StepResult {
                    step: i,
                    action_type: step.type_name().to_string(),
                    success: ar.success,
                    output_summary,
                    output: ar.output,
                    error: ar.error,
                    duration_ms,
                }
            }
        })
        .collect();

    let results = join_all(futures).await;
    let steps_completed = results.len();
    let _total_ms = start.elapsed().as_millis() as u64;

    let any_failed = results.iter().any(|r| !r.success);
    let payload = ActionOutput::PipelineResult {
        steps_total: steps.len(),
        steps_completed,
        stopped_early: false,
        results,
        snapshot_name: taken_snapshot,
    };
    if any_failed {
        let mut err = ActionResult::err(
            "Pipeline",
            ErrorKind::ExecutionFailed,
            format!(
                "Parallel pipeline finished with {}/{} steps failed.",
                results_failed_count(&payload),
                steps.len()
            ),
            0,
        );
        err.output = Some(payload);
        return err;
    }
    ActionResult::ok("Pipeline", payload, 0)
}

/// Extract write-relevant paths from an action (for auto-snapshotting).
fn write_paths_of(action: &Action) -> Vec<String> {
    match action {
        Action::WriteFile { path, .. }
        | Action::EditFile { path, .. }
        | Action::ApplyPatch { path, .. }
        | Action::MoveToTrash { path } => vec![path.clone()],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::handles;
    use crate::policy::PolicyEngine;

    /// Composite tests do real file I/O under the system temp dir. Since an
    /// empty allowlist now means DENY ALL (Aug 2026 confinement change), grant
    /// that directory explicitly. Confinement itself is proven in
    /// `executor::fs::confinement_tests`, which uses tight per-test roots.
    fn test_cfg() -> AppConfig {
        let mut c = AppConfig::defaults();
        let root = std::env::temp_dir().to_string_lossy().into_owned();
        // Reads also touch /etc/hostname as a stable "safe read" fixture.
        c.policy.allowed_read_paths = vec![root.clone(), "/etc".to_string()];
        c.policy.allowed_write_paths = vec![root];
        c
    }

    fn make_engine(cfg: &AppConfig) -> PolicyEngine {
        PolicyEngine::new(cfg.clone(), cfg.actions.clone())
    }

    fn make_handles(cfg: &AppConfig) -> SharedHandles {
        handles::new_shared(
            cfg.events.max_events_per_handle,
            cfg.handles.completed_retention_secs,
            cfg.handles.max_concurrent,
        )
    }

    fn make_limiter() -> RateLimiter {
        crate::executor::desktop::new_rate_limiter()
    }

    fn make_composites(cfg: &AppConfig) -> CompositeStore {
        CompositeStore::new(cfg)
    }

    #[tokio::test]
    async fn pipeline_all_read_steps_succeed() {
        // Use /etc/hostname and /proc/version — always readable on Linux
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let steps = vec![
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(result.success, "pipeline failed: {:?}", result.error);
        if let Some(ActionOutput::PipelineResult {
            steps_total,
            steps_completed,
            stopped_early,
            ..
        }) = result.output
        {
            assert_eq!(steps_total, 2);
            assert_eq!(steps_completed, 2);
            assert!(!stopped_early);
        } else {
            panic!("unexpected output type");
        }
    }

    #[tokio::test]
    async fn pipeline_rejects_at_max_depth() {
        let cfg = test_cfg();
        let mut policy_cfg = cfg.clone();
        policy_cfg.actions.insert("Pipeline".into(), true);
        let policy = make_engine(&policy_cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        // Calling at MAX_PIPELINE_DEPTH should be rejected
        let steps = vec![Action::ListSnapshots];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            MAX_PIPELINE_DEPTH,
        )
        .await;
        assert!(!result.success);
        assert!(result.error.unwrap().message.contains("nesting too deep"));
    }

    #[tokio::test]
    async fn pipeline_allows_depth_below_max() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        // depth 1 should be fine (nested once)
        let steps = vec![Action::ListSnapshots];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            1,
        )
        .await;
        assert!(
            result.success,
            "depth 1 should be allowed: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn pipeline_preflight_blocks_disabled_step() {
        let mut cfg = test_cfg();
        cfg.actions.insert("WriteFile".into(), false);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let steps = vec![
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::WriteFile {
                path: "/tmp/test_x0ne_pipeline.txt".into(),
                content: "x".into(),
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(!result.success);
        let msg = result.error.unwrap().message;
        assert!(msg.contains("step 1"), "expected step index in: {}", msg);
    }

    #[tokio::test]
    async fn pipeline_stop_on_error() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        // First step ok (hostname), second fails (non-existent file), third never runs
        let steps = vec![
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::ReadFile {
                // Inside the allowed root so policy PASSES and the step
                // fails at open time — this test asserts runtime early-stop,
                // not preflight denial.
                path: std::env::temp_dir()
                    .join("x0ne_no_such_file_ever.txt")
                    .to_string_lossy()
                    .into_owned(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        // T2 contract: a Pipeline whose stop_on_error tripped reports
        // success=false (no more "lying footer"). The structured output
        // is still attached so the LLM/UI can see per-step results.
        assert!(
            !result.success,
            "Pipeline must report failure when a step failed"
        );
        let err = result
            .error
            .as_ref()
            .expect("failed pipeline must carry an error");
        assert!(
            err.message.contains("stopped"),
            "error should mention early stop: {}",
            err.message
        );
        if let Some(ActionOutput::PipelineResult {
            stopped_early,
            steps_completed,
            results,
            ..
        }) = result.output
        {
            assert!(stopped_early);
            assert_eq!(steps_completed, 2); // step 0 ok + step 1 failed
            assert_eq!(results.len(), 2);
            assert!(results[0].success);
            assert!(
                !results[0].output_summary.is_empty(),
                "T2: step 0 success must carry a non-empty output_summary"
            );
            assert!(!results[1].success);
            assert!(
                results[1].output_summary.starts_with("err:"),
                "T2: step 1 failure must carry an 'err:' output_summary, got '{}'",
                results[1].output_summary
            );
        } else {
            panic!("unexpected output");
        }
    }

    /// T2 — A Pipeline where every step succeeds keeps success=true and
    /// every StepResult carries a non-empty `output_summary`. This locks
    /// the new contract from the happy-path side.
    #[tokio::test]
    async fn pipeline_all_success_attaches_output_summary_per_step() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let steps = vec![
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(result.success);
        if let Some(ActionOutput::PipelineResult { results, .. }) = result.output {
            for r in &results {
                assert!(r.success);
                assert!(
                    !r.output_summary.is_empty(),
                    "step {} must carry a non-empty output_summary",
                    r.step
                );
                assert!(
                    !r.output_summary.starts_with("err:"),
                    "successful step {} must not start with 'err:'",
                    r.step
                );
            }
        } else {
            panic!("unexpected output");
        }
    }

    // ── Nested pipeline tests ────────────────────────────────────────────

    /// A Pipeline containing a nested Pipeline (depth 0 → inner at depth 1)
    /// should execute successfully and report correct step counts.
    #[tokio::test]
    async fn nested_pipeline_executes_inner_steps() {
        let mut cfg = test_cfg();
        cfg.actions.insert("Pipeline".into(), true);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        // Outer pipeline: [nested_pipeline, read_file]
        let inner = Action::Pipeline {
            steps: vec![
                Action::ReadFile {
                    path: "/etc/hostname".into(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
                Action::ReadFile {
                    path: "/etc/hostname".into(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
            ],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let steps = vec![
            inner,
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(result.success, "nested pipeline failed: {:?}", result.error);
        if let Some(ActionOutput::PipelineResult {
            steps_total,
            steps_completed,
            results,
            ..
        }) = result.output
        {
            assert_eq!(steps_total, 2, "outer pipeline has 2 steps");
            assert_eq!(steps_completed, 2);
            // First step is the nested pipeline — its output should be a PipelineResult
            assert!(results[0].success);
            assert!(results[1].success);
            // The nested pipeline step should have an output_summary
            assert!(
                !results[0].output_summary.is_empty(),
                "nested pipeline step must carry output_summary"
            );
        } else {
            panic!("unexpected output type");
        }
    }

    /// Three levels of nesting (depth 0 → 1 → 2) should all succeed.
    #[tokio::test]
    async fn triple_nested_pipeline_succeeds() {
        let mut cfg = test_cfg();
        cfg.actions.insert("Pipeline".into(), true);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let innermost = Action::Pipeline {
            steps: vec![Action::ListSnapshots],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let middle = Action::Pipeline {
            steps: vec![innermost],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let outer_steps = vec![middle];
        let result = execute_pipeline(
            &outer_steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(
            result.success,
            "depth 0→1→2 must succeed: {:?}",
            result.error
        );
    }

    /// Four levels of nesting (depth 0 → 1 → 2 → 3) should fail because
    /// depth 3 == MAX_PIPELINE_DEPTH.
    #[tokio::test]
    async fn quadruple_nested_pipeline_fails() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let deepest = Action::Pipeline {
            steps: vec![Action::ListSnapshots],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let level2 = Action::Pipeline {
            steps: vec![deepest],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let level1 = Action::Pipeline {
            steps: vec![level2],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let outer_steps = vec![level1];
        let result = execute_pipeline(
            &outer_steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        // The outermost pipeline may report success=false because an inner step failed
        assert!(!result.success, "depth 0→1→2→3 must fail due to max depth");
    }

    /// A nested pipeline failure with stop_on_error=true should propagate
    /// up to the outer pipeline and stop it.
    #[tokio::test]
    async fn nested_pipeline_failure_propagates() {
        let mut cfg = test_cfg();
        cfg.actions.insert("Pipeline".into(), true);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        // Inner pipeline has a step that will fail
        let inner = Action::Pipeline {
            steps: vec![
                Action::ReadFile {
                    path: "/etc/hostname".into(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
                Action::ReadFile {
                    // Inside the allowed root: must fail at open, not preflight.
                    path: std::env::temp_dir()
                        .join("x0ne_no_such_file_ever.txt")
                        .to_string_lossy()
                        .into_owned(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
            ],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        // Outer: [failing_inner, read_file_that_should_not_run]
        let steps = vec![
            inner,
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        let result = execute_pipeline(
            &steps,
            true,
            false,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(!result.success, "outer must fail when inner pipeline fails");
        if let Some(ActionOutput::PipelineResult {
            stopped_early,
            steps_completed,
            ..
        }) = result.output
        {
            assert!(stopped_early, "outer should stop early");
            assert_eq!(
                steps_completed, 1,
                "only the inner pipeline step should have been attempted"
            );
        } else {
            panic!("unexpected output");
        }
    }

    /// A nested pipeline inside a parallel outer pipeline should work.
    #[tokio::test]
    async fn nested_pipeline_in_parallel_mode() {
        let mut cfg = test_cfg();
        cfg.actions.insert("Pipeline".into(), true);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let inner = Action::Pipeline {
            steps: vec![Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            }],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let steps = vec![
            inner,
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
        ];
        // parallel = true on the outer
        let result = execute_pipeline(
            &steps,
            true,
            true,
            &None,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;
        assert!(
            result.success,
            "parallel outer with nested inner should succeed: {:?}",
            result.error
        );
    }

    /// Verify MAX_PIPELINE_DEPTH constant is what we expect.
    #[test]
    fn max_depth_is_three() {
        assert_eq!(
            MAX_PIPELINE_DEPTH, 3,
            "MAX_PIPELINE_DEPTH must be 3 to allow 3 levels of nesting (0, 1, 2)"
        );
    }
}
