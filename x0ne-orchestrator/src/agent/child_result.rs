//! One turn's report, read as one child's result.
//!
//! `AgentResult` is what `WaitAgent`, `ListAgents` and the worlds UI consume.
//! It is not a driver concept and the driver does not know it exists: a turn
//! report says what a turn did, and only the layer that owns a child's
//! lifecycle knows that this turn *was* a child.
//!
//! # Why a conversion and not a second counter
//!
//! The historical path built `AgentResult` by re-reading the child's
//! conversation: it scanned backwards for the last assistant message that was
//! not an `[ACTION]` or `<result` frame (`sysaicalls.rs:1429`), pulled counts
//! off `Conversation::action_stats`, and asked a separate `UsageTracker` for
//! tokens. Three sources, each able to disagree with the others.
//!
//! [`TurnReport`] already holds all three, measured on the canonical path:
//! usage from `model::invoke`'s own accounting, action counts from the history
//! the driver writes, duration frozen when the turn ended. So this file is a
//! projection, not a computation — nothing here rescans, recounts, estimates,
//! or fills a gap with a zero.

use crate::agent::driver::TurnReport;
use crate::sysaicalls::AgentResult;

/// Project a finished turn onto the child-lifecycle record.
///
/// # `success` has two clauses, and both are load-bearing
///
/// The statistical clause is the historical one: `run_ok = sub_stats.failed ==
/// 0` (`sysaicalls.rs:1210`, `:1444`). It is preserved exactly.
///
/// The lifecycle clause is the one the historical path could not express. A
/// turn that ended on a deadline, a cancellation, a budget refusal or a model
/// failure never reached prose — and if it also never ran an action, its
/// `failed` count is `0`. Reporting that as a success would tell the parent
/// the child answered when the child never spoke. `run_agent_loop` returned no
/// outcome to consult, so the old code could not make this distinction;
/// `TurnOutcome` can, and a child that did not complete is not a success.
pub fn agent_result_from_turn(id: &str, task: &str, report: &TurnReport) -> AgentResult {
    let (response, completed) = match report.response() {
        Some(message) => (message.to_string(), true),
        // Not "No response". The parent is owed the reason, and the outcome
        // kind is the reason, in the driver's own vocabulary.
        None => (format!("child ended: {}", report.outcome.kind()), false),
    };
    AgentResult {
        id: id.to_string(),
        task: task.to_string(),
        response,
        tokens_used: report.usage.total_tokens(),
        actions_executed: report.actions.total,
        actions_breakdown: report.actions.to_json(),
        success: completed && report.actions_all_succeeded(),
        duration_ms: report.duration.as_millis() as u64,
    }
}

/// The record for a child that never started.
///
/// A world whose dedicated runtime fails to open has no turn, so it has no
/// report — there is nothing to project. The historical path wrote exactly
/// this shape (`sysaicalls.rs:1128-1137`): the failure text as the response,
/// no tokens, no actions, and the empty breakdown so a `WaitAgent` consumer
/// never has to defend against a missing key. The duration is the only
/// measured number, and it is measured here rather than invented.
pub fn startup_failure(id: &str, task: &str, reason: &str, duration_ms: u64) -> AgentResult {
    AgentResult {
        id: id.to_string(),
        task: task.to_string(),
        response: reason.to_string(),
        tokens_used: 0,
        actions_executed: 0,
        actions_breakdown: AgentResult::empty_breakdown(),
        success: false,
        duration_ms,
    }
}

/// The envelope a finished child is reported to the model in.
///
/// Historical field names, historical extra keys. `mode` and `bridge` are how
/// the model tells an inline `SpawnAgent` result from a `SpawnWorld` handle
/// without looking back at the action it came from, and `elapsed_secs` is
/// derived from the frozen `duration_ms` so repeated reads never drift.
pub fn to_inline_envelope(result: &AgentResult) -> serde_json::Value {
    serde_json::json!({
        "success": result.success,
        "action_type": "SpawnAgent",
        "output": {
            "id": result.id,
            "task": result.task,
            "response": result.response,
            "tokens_used": result.tokens_used,
            "actions_executed": result.actions_executed,
            "actions_breakdown": result.actions_breakdown,
            "duration_ms": result.duration_ms,
            "elapsed_secs": (result.duration_ms as f64) / 1000.0,
            "mode": "inline",
            "bridge": "shared",
        }
    })
}

/// How the registry should record a finished child.
///
/// `AgentStatus::Failed` carries a sentence, and the historical one names the
/// counts (`sysaicalls.rs:1240`). Kept, because it is what `ListAgents` shows.
pub fn failure_note(report: &TurnReport) -> String {
    format!(
        "{} action(s) failed ({} ok / {} total)",
        report.actions.failed, report.actions.succeeded, report.actions.total
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::driver::{TurnOutcome, TurnUsage};
    use crate::conversation::ActionStats;
    use std::time::Duration;

    fn report(outcome: TurnOutcome, stats: ActionStats, usage: TurnUsage) -> TurnReport {
        TurnReport {
            outcome,
            usage,
            actions: stats,
            duration: Duration::from_millis(1234),
        }
    }

    fn stats(total: usize, succeeded: usize, failed: usize, by: &[(&str, usize)]) -> ActionStats {
        let mut s = ActionStats {
            total,
            succeeded,
            failed,
            ..Default::default()
        };
        for (k, v) in by {
            s.by_type.insert((*k).to_string(), *v);
        }
        s
    }

    /// (1) Every field comes from the report. Nothing is recomputed and the
    /// numbers are the driver's own.
    #[test]
    fn every_field_is_projected_from_the_turn_report() {
        let r = report(
            TurnOutcome::Completed {
                message: "done".into(),
            },
            stats(3, 3, 0, &[("ReadFile", 2), ("WriteFile", 1)]),
            TurnUsage {
                input_tokens: 100,
                output_tokens: 25,
                cache_read_tokens: 900,
                ..Default::default()
            },
        );
        let out = agent_result_from_turn("rev", "review the diff", &r);

        assert_eq!(out.id, "rev");
        assert_eq!(out.task, "review the diff");
        assert_eq!(out.response, "done");
        // input + output + cache, matching `UsageTracker::total_tokens`.
        //
        // This used to assert 125 on the rationale that "cache reads are not
        // billed" — which is wrong on the facts: `UsageTracker::cost()` prices
        // them at 10% of the input rate, and they occupy the context window
        // like any other token. Excluding them made a child that read 900
        // cached tokens report 125, and the parent budgeting on that figure
        // was blind to nearly 90% of what the child moved.
        assert_eq!(out.tokens_used, 1025);
        assert_eq!(out.actions_executed, 3);
        assert_eq!(out.actions_breakdown["total"], 3);
        assert_eq!(out.actions_breakdown["by_type"]["ReadFile"], 2);
        assert!(out.success);
        assert_eq!(out.duration_ms, 1234);
    }

    /// (1) The historical statistical clause, unchanged: one failed action
    /// flips the child red even though it completed and spoke.
    #[test]
    fn one_failed_action_flips_success_on_a_completed_child() {
        let r = report(
            TurnOutcome::Completed {
                message: "partially done".into(),
            },
            stats(2, 1, 1, &[("WriteFile", 2)]),
            TurnUsage::default(),
        );
        let out = agent_result_from_turn("c", "t", &r);
        assert!(!out.success);
        assert_eq!(out.response, "partially done");
        assert_eq!(out.actions_executed, 2);
    }

    /// (5) The lifecycle clause. Zero failed actions is not a success when the
    /// turn never completed — the child did not answer, and saying it did
    /// would be the invented success the historical shape could not rule out.
    #[test]
    fn a_child_that_never_completed_is_not_a_success_with_zero_failures() {
        for outcome in [
            TurnOutcome::Cancelled,
            TurnOutcome::DeadlineExceeded {
                after: Duration::from_secs(180),
            },
        ] {
            let kind = outcome.kind();
            let r = report(outcome, stats(0, 0, 0, &[]), TurnUsage::default());
            let out = agent_result_from_turn("c", "t", &r);
            assert!(!out.success, "{kind} reported as a success");
            assert!(
                out.response.contains(kind),
                "the parent is not told why: {}",
                out.response
            );
        }
    }

    /// A child that never started has no report to project, and the record
    /// says so rather than showing a zeroed success.
    #[test]
    fn a_child_that_never_started_records_the_reason_and_an_empty_breakdown() {
        let out = startup_failure("w1", "explore", "Bridge error: no runtime", 12);
        assert!(!out.success);
        assert_eq!(out.response, "Bridge error: no runtime");
        assert_eq!(out.tokens_used, 0);
        assert_eq!(out.actions_executed, 0);
        assert_eq!(out.actions_breakdown, AgentResult::empty_breakdown());
        assert_eq!(out.duration_ms, 12);
    }

    /// The envelope keeps the historical field names, including the two that
    /// tell an inline result from a world handle.
    #[test]
    fn the_inline_envelope_keeps_the_historical_field_names() {
        let r = report(
            TurnOutcome::Completed {
                message: "ok".into(),
            },
            stats(1, 1, 0, &[("ReadFile", 1)]),
            TurnUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        );
        let env = to_inline_envelope(&agent_result_from_turn("a", "t", &r));
        assert_eq!(env["success"], true);
        assert_eq!(env["action_type"], "SpawnAgent");
        let out = &env["output"];
        for key in [
            "id",
            "task",
            "response",
            "tokens_used",
            "actions_executed",
            "actions_breakdown",
            "duration_ms",
            "elapsed_secs",
            "mode",
            "bridge",
        ] {
            assert!(out.get(key).is_some(), "`{key}` missing from the envelope");
        }
        assert_eq!(out["mode"], "inline");
        assert_eq!(out["bridge"], "shared");
        // Derived from the frozen duration, never from a running clock.
        assert_eq!(out["elapsed_secs"], 1.234);
    }

    /// The conversion is the only place statistics are read, so the module
    /// must not grow a second source for any of them.
    #[test]
    fn guard_nothing_here_rescans_recounts_or_estimates() {
        let src = include_str!("child_result.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["history", "messages()", "UsageTracker", "action_stats"] {
            assert!(
                !production.contains(banned),
                "`{banned}` appears: the record would then have a second source"
            );
        }
    }
}
