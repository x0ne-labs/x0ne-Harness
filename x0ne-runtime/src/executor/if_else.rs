//! `If` — conditional execution primitive.
//!
//! Evaluates a check using the same grammar as [`Action::Assert`], then
//! sequentially runs either the `then` or the `else` branch. Saves one LLM
//! round-trip per decision compared to Assert-then-wait-then-choose.
//!
//! ## Semantics
//!
//! 1. **Policy preflight.** Both branches are evaluated by [`PolicyEngine`]
//!    *before* any step runs — a denied action in the branch that won't be
//!    taken still blocks the whole `If`. This mirrors Pipeline/Try and keeps
//!    the no-surprises contract: if an `If` returns success, we know every
//!    reachable step was policy-allowed.
//! 2. **Depth tracking.** The selected branch executes at `depth + 1`,
//!    counting against `pipeline::MAX_PIPELINE_DEPTH`. An `If` inside an
//!    `If` inside a `Pipeline` ≡ a 3-level nesting.
//! 3. **Stop-on-error.** The selected branch is sequential with
//!    stop-on-error (like a non-parallel Pipeline). There is no parallel
//!    mode for `If` — it never makes sense to "conditionally-in-parallel".
//! 4. **Empty branches.** An empty `then` (or empty `else`) is valid and
//!    reports `branch: "none"` with zero steps. Useful for one-sided guards
//!    like "if port open, run nothing; else start the service".

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::handles::SharedHandles;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionOutput, ActionResult, ErrorKind, StepResult};

use super::assert::execute_assert;
use super::desktop::RateLimiter;
use super::pipeline::MAX_PIPELINE_DEPTH;

/// Execute an `Action::If`.
///
/// Parameters mirror the variant's fields; `depth` is forwarded from the
/// caller so nested `If`/`Pipeline` combos honour the global depth guard.
#[allow(clippy::too_many_arguments)]
pub async fn execute_if(
    check: &str,
    check_params: &serde_json::Value,
    then_branch: &[Action],
    else_branch: &[Action],
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
    depth: usize,
) -> ActionResult {
    // ── Depth guard (matches Pipeline) ───────────────────────────────────────
    if depth >= MAX_PIPELINE_DEPTH {
        return ActionResult::err(
            "If",
            ErrorKind::NestingNotAllowed,
            format!(
                "If nesting too deep (depth {}, max {}). Flatten your logic.",
                depth, MAX_PIPELINE_DEPTH
            ),
            0,
        );
    }

    // ── Step guard (both branches count) ─────────────────────────────────────
    const MAX_IF_STEPS_PER_BRANCH: usize = 20;
    if then_branch.len() > MAX_IF_STEPS_PER_BRANCH || else_branch.len() > MAX_IF_STEPS_PER_BRANCH {
        return ActionResult::err(
            "If",
            ErrorKind::InvalidInput,
            format!(
                "If branches may have at most {} steps each (got then={}, else={}).",
                MAX_IF_STEPS_PER_BRANCH,
                then_branch.len(),
                else_branch.len(),
            ),
            0,
        );
    }

    // ── Policy preflight on BOTH branches before any execution ───────────────
    // Same contract as Pipeline: even the branch that won't be taken must be
    // allowed. An If that returns success guarantees every reachable step
    // was policy-approved.
    for (label, branch) in [("then", then_branch), ("else", else_branch)] {
        for (i, step) in branch.iter().enumerate() {
            match policy.evaluate(step) {
                PolicyEffect::Allow => {}
                PolicyEffect::Deny(reason) => {
                    return ActionResult::err(
                        "If",
                        ErrorKind::PolicyDenied,
                        format!(
                            "If rejected before execution: {} branch step {} ({}) denied — {}",
                            label,
                            i,
                            step.type_name(),
                            reason.message()
                        ),
                        0,
                    );
                }
                PolicyEffect::RequireConfirmation(_) => {
                    return ActionResult::err(
                        "If",
                        ErrorKind::PolicyDenied,
                        format!(
                            "If rejected before execution: {} branch step {} ({}) requires confirmation — \
                             If cannot host interactive steps, move the action outside the If",
                            label,
                            i,
                            step.type_name()
                        ),
                        0,
                    );
                }
            }
        }
    }

    // ── Evaluate the condition ───────────────────────────────────────────────
    // We reuse Assert's full grammar and plumbing. Any check Assert knows
    // about works here — no duplication, no new vocabulary.
    let check_result = execute_assert(
        check,
        check_params,
        config,
        handles.clone(),
        policy,
        rate_limiter,
        composites,
    )
    .await;

    // Assert is engineered to always return success=true with an
    // AssertResult output carrying `ok: bool`. If that changes we want
    // to notice, so we defensively match and bail with a clear error
    // instead of silently treating a broken check as "false".
    let (check_ok, check_detail) = match check_result.output.as_ref() {
        Some(ActionOutput::AssertResult { ok, detail, .. }) => (*ok, detail.clone()),
        _ => {
            return ActionResult::err(
                "If",
                ErrorKind::ExecutionFailed,
                format!(
                    "If check '{}' did not return a standard AssertResult — this is an internal bug.",
                    check
                ),
                0,
            );
        }
    };

    // ── Execute the chosen branch ────────────────────────────────────────────
    let (branch_name, steps): (&str, &[Action]) = if check_ok {
        ("then", then_branch)
    } else {
        ("else", else_branch)
    };

    if steps.is_empty() {
        return ActionResult::ok(
            "If",
            ActionOutput::IfResult {
                check_ok,
                branch: "none".to_string(),
                check_detail,
                steps_total: 0,
                steps_completed: 0,
                stopped_early: false,
                results: Vec::new(),
            },
            0,
        );
    }

    let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());
    let mut steps_completed = 0usize;
    let mut stopped_early = false;
    let mut any_failed = false;

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

        steps_completed += 1;
        if !success {
            any_failed = true;
            stopped_early = i + 1 < steps.len();
            break;
        }
    }

    let payload = ActionOutput::IfResult {
        check_ok,
        branch: branch_name.to_string(),
        check_detail,
        steps_total: steps.len(),
        steps_completed,
        stopped_early,
        results,
    };

    if any_failed {
        let mut err = ActionResult::err(
            "If",
            ErrorKind::ExecutionFailed,
            if stopped_early {
                format!(
                    "If {}-branch stopped after {}/{} steps (one step failed).",
                    branch_name,
                    steps_completed,
                    steps.len()
                )
            } else {
                format!(
                    "If {}-branch finished with a failing step ({}/{}).",
                    branch_name,
                    steps_completed,
                    steps.len()
                )
            },
            0,
        );
        err.output = Some(payload);
        return err;
    }

    ActionResult::ok("If", payload, 0)
}

/// Build the Assert-compatible params JSON object from the flat `If`
/// variant fields. Mirrors the mapping done in `executor::mod.rs` for
/// `Action::Assert`.
pub fn build_check_params(
    path: &Option<String>,
    value: &Option<serde_json::Value>,
    host: &Option<String>,
    port: &Option<u16>,
    command: &Option<String>,
    args: &Option<Vec<String>>,
    name: &Option<String>,
    actual: &Option<serde_json::Value>,
    expected: &Option<serde_json::Value>,
) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    if let Some(p) = path {
        params.insert("path".into(), serde_json::Value::String(p.clone()));
    }
    if let Some(v) = value {
        params.insert("value".into(), v.clone());
    }
    if let Some(h) = host {
        params.insert("host".into(), serde_json::Value::String(h.clone()));
    }
    if let Some(p) = port {
        params.insert("port".into(), serde_json::json!(p));
    }
    if let Some(c) = command {
        params.insert("command".into(), serde_json::Value::String(c.clone()));
    }
    if let Some(a) = args {
        params.insert("args".into(), serde_json::json!(a));
    }
    if let Some(n) = name {
        params.insert("name".into(), serde_json::Value::String(n.clone()));
    }
    if let Some(a) = actual {
        params.insert("actual".into(), a.clone());
    }
    if let Some(e) = expected {
        params.insert("expected".into(), e.clone());
    }
    serde_json::Value::Object(params)
}

fn truncate_err(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `If`'s condition is evaluated by the same code as `Assert`, so it
    /// inherited the same bypass: a `command_succeeds` condition spawned a
    /// process with no policy check. Pinned separately because `If` is
    /// `Composite` — enabled by default — and reaches the check by a different
    /// route than `Assert` does.
    #[tokio::test]
    async fn if_condition_cannot_run_a_disabled_run_command() {
        let mut config = AppConfig::defaults();
        config.actions.insert("RunCommand".into(), false);
        let policy = PolicyEngine::new(config.clone(), config.actions.clone());
        let handles = crate::handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = crate::composite::CompositeStore::new(&config);

        let marker = std::env::temp_dir().join(format!("x0ne-if-red-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let check_params = serde_json::json!({
            "command": "touch",
            "args": [marker.to_string_lossy()],
        });
        let _ = execute_if(
            "command_succeeds",
            &check_params,
            &[],
            &[],
            &config,
            &policy,
            handles,
            &rate_limiter,
            &composites,
            0,
        )
        .await;

        assert!(
            !marker.exists(),
            "an If condition must not spawn a disabled RunCommand"
        );
        let _ = std::fs::remove_file(&marker);
    }
    use crate::composite;
    use crate::config::AppConfig;
    use crate::handles;
    use crate::policy::PolicyEngine;
    use serde_json::json;

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
    use std::io::Write;

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
        composite::CompositeStore::new(cfg)
    }

    /// Happy path — condition is true, the `then` branch runs, and the
    /// result carries per-step output_summary. Mirrors
    /// `pipeline_all_success_attaches_output_summary_per_step`.
    #[tokio::test]
    async fn if_true_runs_then_branch() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 42, "expected": 42});
        let then_steps = vec![Action::ReadFile {
            path: "/etc/hostname".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }];
        let else_steps = vec![Action::ListSnapshots];

        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        assert!(result.success, "If should succeed: {:?}", result.error);
        match result.output {
            Some(ActionOutput::IfResult {
                check_ok,
                branch,
                steps_total,
                steps_completed,
                results,
                ..
            }) => {
                assert!(check_ok);
                assert_eq!(branch, "then");
                assert_eq!(steps_total, 1);
                assert_eq!(steps_completed, 1);
                assert_eq!(results.len(), 1);
                assert!(results[0].success);
                assert!(!results[0].output_summary.is_empty());
            }
            other => panic!("unexpected output: {:?}", other),
        }
    }

    /// Condition is false — the `else` branch runs, the `then` branch is
    /// never executed (so a write in `then` would never land, verified
    /// separately). Flat negation of the previous test.
    #[tokio::test]
    async fn if_false_runs_else_branch() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 1, "expected": 2});
        let then_steps = vec![Action::ListSnapshots];
        let else_steps = vec![Action::ReadFile {
            path: "/etc/hostname".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }];

        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        assert!(result.success);
        match result.output {
            Some(ActionOutput::IfResult {
                check_ok,
                branch,
                steps_total,
                ..
            }) => {
                assert!(!check_ok);
                assert_eq!(branch, "else");
                assert_eq!(steps_total, 1);
            }
            other => panic!("unexpected output: {:?}", other),
        }
    }

    /// Selected branch is empty — `If` still succeeds and reports
    /// `branch: "none"`. Covers the "one-sided guard" use case.
    #[tokio::test]
    async fn if_empty_branch_is_noop() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 1, "expected": 2}); // false
        let then_steps = vec![Action::ListSnapshots];
        let else_steps: Vec<Action> = vec![];

        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        assert!(result.success);
        match result.output {
            Some(ActionOutput::IfResult {
                check_ok,
                branch,
                steps_total,
                ..
            }) => {
                assert!(!check_ok);
                assert_eq!(branch, "none");
                assert_eq!(steps_total, 0);
            }
            other => panic!("unexpected output: {:?}", other),
        }
    }

    /// A failing step inside the taken branch makes the whole `If` fail
    /// and attaches the per-step trace. Same contract as Pipeline.
    #[tokio::test]
    async fn if_branch_step_failure_propagates() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 1, "expected": 1}); // true
        let then_steps = vec![
            Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::ReadFile {
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
        let else_steps: Vec<Action> = vec![];

        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        assert!(!result.success);
        match result.output {
            Some(ActionOutput::IfResult {
                branch,
                steps_total,
                steps_completed,
                stopped_early,
                results,
                ..
            }) => {
                assert_eq!(branch, "then");
                assert_eq!(steps_total, 3);
                assert_eq!(steps_completed, 2); // 1 success + 1 failure
                assert!(stopped_early);
                assert!(results[0].success);
                assert!(!results[1].success);
                assert!(results[1].output_summary.starts_with("err:"));
            }
            other => panic!("unexpected output: {:?}", other),
        }
    }

    /// Policy preflight blocks an `If` whose `else` branch contains a
    /// disabled action — even if the condition is true and the `else`
    /// will never execute. Matches the Pipeline contract.
    #[tokio::test]
    async fn if_preflight_blocks_disabled_step_in_untaken_branch() {
        let mut cfg = test_cfg();
        cfg.actions.insert("WriteFile".into(), false);
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 1, "expected": 1}); // true
        let then_steps = vec![Action::ReadFile {
            path: "/etc/hostname".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }];
        let else_steps = vec![
            // Even though this branch is never taken, policy should still reject it.
            Action::WriteFile {
                path: "/tmp/x0ne_if_preflight_test.txt".into(),
                content: "x".into(),
            },
        ];

        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        assert!(!result.success);
        let msg = result
            .error
            .expect("policy denial must carry an error")
            .message;
        assert!(
            msg.contains("else branch"),
            "expected else branch mention: {}",
            msg
        );
    }

    /// A real `file_exists` check wired through Assert: write a temp file,
    /// run `If file_exists → ReadFile else Sleep`, confirm the `then`
    /// branch ran. Exercises the full Assert→bridge→primitive path.
    #[tokio::test]
    async fn if_file_exists_check_integrates_with_assert() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let tmp = std::env::temp_dir().join(format!("x0ne_if_test_{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).expect("create tmp");
            writeln!(f, "hello").unwrap();
        }

        let params = json!({"path": tmp.to_string_lossy()});
        let then_steps = vec![Action::ReadFile {
            path: tmp.to_string_lossy().into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }];
        let else_steps = vec![Action::ListSnapshots];

        let result = execute_if(
            "file_exists",
            &params,
            &then_steps,
            &else_steps,
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            0,
        )
        .await;

        let _ = std::fs::remove_file(&tmp);

        assert!(result.success);
        if let Some(ActionOutput::IfResult {
            branch, check_ok, ..
        }) = result.output
        {
            assert!(check_ok, "file should exist");
            assert_eq!(branch, "then");
        } else {
            panic!("expected IfResult");
        }
    }

    /// An `If` nested at `MAX_PIPELINE_DEPTH` must refuse to run — same
    /// guard as Pipeline. Prevents runaway recursion of conditions.
    #[tokio::test]
    async fn if_rejects_at_max_depth() {
        let cfg = test_cfg();
        let policy = make_engine(&cfg);
        let h = make_handles(&cfg);
        let c = make_composites(&cfg);

        let params = json!({"actual": 1, "expected": 1});
        let then_steps = vec![Action::ListSnapshots];
        let result = execute_if(
            "eq",
            &params,
            &then_steps,
            &[],
            &cfg,
            &policy,
            h,
            &make_limiter(),
            &c,
            MAX_PIPELINE_DEPTH,
        )
        .await;

        assert!(!result.success);
        assert!(
            result.error.unwrap().message.contains("nesting too deep"),
            "depth guard must fire"
        );
    }
}
