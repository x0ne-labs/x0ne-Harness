//! Behavioural loop detection, as an engine component.
//!
//! The implementation is [`crate::loop_tracker::RepeatedActionTracker`], which
//! was already correct and already used by three of the five loops. What was
//! missing was a stated boundary, because a second, cruder copy of the same
//! idea had grown inside the budget.
//!
//! ## The boundary with `TurnBudget`
//!
//! > **Loop detection asks "is the model repeating itself?" — a question about
//! > behaviour, answered from canonical action identity.**
//! >
//! > **The budget asks "how much has this turn consumed?" — a question about
//! > quantity, answered from counters.**
//!
//! They are not two views of one thing, and where they overlapped, the overlap
//! was removed rather than kept in sync. `TurnBudget` briefly carried a
//! `SameActionFailures` counter keyed on the action *name*; that is the same
//! question this module answers, asked worse. `WriteFile` failing on six
//! different paths is a model working through a problem, and the name-keyed
//! counter would have stopped it. `WriteFile` failing six times on the *same*
//! path with the *same* error is a loop, and only identity-plus-arguments can
//! tell those apart. So the counter is gone and this module owns the question.
//!
//! The budget keeps what is genuinely quantitative: consecutive actions before
//! a check-in, parse retries, total failures, and money.
//!
//! ## Identity, not rendering
//!
//! The key is `(action type, normalised arguments, error code)`. Arguments are
//! canonicalised — `serde_json` orders object keys, at every depth, so two
//! payloads that differ only in field order or whitespace land in the same
//! bucket. Nothing here reads a rendered string, so a frontend that formats
//! actions differently cannot change what counts as a loop.
//!
//! ## What a success resets
//!
//! Only the entries for that exact `(type, arguments)` pair. A model that gets
//! `WriteFile /a` working has not demonstrated anything about `WriteFile /b`,
//! and blanket-clearing the history would let a genuine loop restart its count
//! every time an unrelated action happened to succeed.

//! ## The second question: stationarity
//!
//! "Is the model repeating itself?" was, until 2026-08-16, only ever asked
//! about *failures*. A success clears `RepeatedActionTracker` outright, so a
//! model stuck in a **succeeding** loop — the same `ReadFile` thirty times over
//! — met no breaker at all and simply burned the budget.
//!
//! [`StationarityTracker`] closes that: it counts the run of identical calls
//! regardless of outcome, nudges once at [`STATIONARITY_NUDGE`], and ends the
//! turn at [`STATIONARITY_THRESHOLD`]. The two breakers keep distinct
//! thresholds because they answer distinct questions — five identical
//! *failures* is already a wall, eight identical *successes* is merely a run
//! long enough that the next one cannot teach the model anything new.

pub use crate::loop_tracker::{
    LoopDetected, RepeatedActionTracker, StationarityTracker, Stationary, LOOP_THRESHOLD,
    STATIONARITY_NUDGE, STATIONARITY_THRESHOLD,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &str) -> serde_json::Value {
        json!({"type": "WriteFile", "path": path, "content": "x"})
    }

    /// The core case: the same payload failing the same way. The breaker trips
    /// on the attempt that would be the `LOOP_THRESHOLD`-th.
    #[test]
    fn the_same_failing_action_is_eventually_refused() {
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
        }
        assert!(
            t.would_block_any_error(&write("/tmp/a")).is_some(),
            "a payload that has failed identically {} times must be refused",
            LOOP_THRESHOLD - 1
        );
    }

    /// Below the threshold the model is still allowed to work.
    #[test]
    fn a_few_failures_are_not_a_loop() {
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 2 {
            t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
        }
        assert!(t.would_block_any_error(&write("/tmp/a")).is_none());
    }

    /// Identity is semantic. Reordered keys and different whitespace are the
    /// same action, and a frontend that serialises differently must not be
    /// able to hide a loop or invent one.
    #[test]
    fn key_order_and_whitespace_do_not_change_identity() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"type":"WriteFile","path":"/tmp/a","content":"x"}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(
            "{\n  \"content\" : \"x\",\n  \"path\":\"/tmp/a\",\n  \"type\":\"WriteFile\"\n}",
        )
        .unwrap();

        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&a, Some("permission_denied"));
        }
        assert!(
            t.would_block_any_error(&b).is_some(),
            "the same action written differently must count as the same action"
        );
    }

    /// Different arguments are different work. This is the false positive that
    /// a name-keyed counter would produce: a model fixing paths one at a time
    /// is making progress, not looping.
    #[test]
    fn different_arguments_are_not_a_loop() {
        let mut t = RepeatedActionTracker::new();
        for i in 0..LOOP_THRESHOLD + 3 {
            t.record_outcome(&write(&format!("/tmp/{i}")), Some("permission_denied"));
        }
        assert!(
            t.would_block_any_error(&write("/tmp/new")).is_none(),
            "distinct payloads must not accumulate into a false loop"
        );
    }

    /// The same payload failing for *different* reasons is also not a loop in
    /// the sense that matters: the situation is changing under the model.
    #[test]
    fn a_changing_error_is_not_the_same_loop() {
        let mut t = RepeatedActionTracker::new();
        for code in ["permission_denied", "not_found", "invalid_input", "timeout"] {
            t.record_outcome(&write("/tmp/a"), Some(code));
        }
        assert!(
            t.would_block_any_error(&write("/tmp/a")).is_none(),
            "four different failures are four problems, not one loop"
        );
    }

    /// Progress clears the loop — but only for the action that made it.
    #[test]
    fn success_clears_that_action_and_only_that_action() {
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
            t.record_outcome(&write("/tmp/b"), Some("permission_denied"));
        }
        t.record_outcome(&write("/tmp/a"), None); // /a now works

        assert!(
            t.would_block_any_error(&write("/tmp/a")).is_none(),
            "the action that succeeded must be free to run again"
        );
        assert!(
            t.would_block_any_error(&write("/tmp/b")).is_some(),
            "an unrelated success must not absolve a different stuck action"
        );
    }

    /// Failure → success → the same failure again starts a fresh count rather
    /// than resuming the old one.
    #[test]
    fn a_retry_after_success_starts_over() {
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
        }
        t.record_outcome(&write("/tmp/a"), None);
        t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
        assert!(
            t.would_block_any_error(&write("/tmp/a")).is_none(),
            "one failure after a success is not a resumed loop"
        );
    }

    /// Composites are identified by their whole shape, so two Pipelines that
    /// differ in a nested step are different work — and two that differ only
    /// in nested key order are not.
    #[test]
    fn composites_are_identified_by_their_contents() {
        let pipeline = |path: &str| {
            json!({
                "type": "Pipeline",
                "steps": [{"type": "WriteFile", "path": path, "content": "x"}],
                "stop_on_error": true,
            })
        };
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&pipeline("/tmp/a"), Some("permission_denied"));
        }
        assert!(
            t.would_block_any_error(&pipeline("/tmp/a")).is_some(),
            "an identical composite repeated is a loop"
        );
        assert!(
            t.would_block_any_error(&pipeline("/tmp/b")).is_none(),
            "a composite with a different step is different work"
        );

        // Same composite, nested keys written in another order.
        let reordered: serde_json::Value = serde_json::from_str(
            r#"{"steps":[{"content":"x","path":"/tmp/a","type":"WriteFile"}],
                "stop_on_error":true,"type":"Pipeline"}"#,
        )
        .unwrap();
        assert!(
            t.would_block_any_error(&reordered).is_some(),
            "nested key order must not change composite identity"
        );
    }

    /// The refusal must tell the model what happened and forbid the retry,
    /// otherwise the breaker just moves the loop up a level.
    #[test]
    fn the_refusal_explains_itself_and_is_not_retryable() {
        let mut t = RepeatedActionTracker::new();
        for _ in 0..LOOP_THRESHOLD - 1 {
            t.record_outcome(&write("/tmp/a"), Some("permission_denied"));
        }
        let blocked = t
            .would_block_any_error(&write("/tmp/a"))
            .expect("must trip");
        let env = &blocked.envelope;
        assert_eq!(env["success"], false);
        assert_eq!(env["error"]["class"], "loop_detected");
        assert_eq!(env["error"]["retryable"], false);
        let msg = env["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("Do NOT retry"), "no guidance: {msg}");
    }

    /// An action that has never failed is never blocked. Guards the whole
    /// component against the worst failure mode: refusing healthy work.
    #[test]
    fn an_untried_action_is_never_blocked() {
        let t = RepeatedActionTracker::new();
        assert!(t.would_block_any_error(&write("/tmp/fresh")).is_none());
        assert!(t
            .would_block_any_error(&json!({"type": "ReadFile", "path": "/etc/hostname"}))
            .is_none());
    }
}
