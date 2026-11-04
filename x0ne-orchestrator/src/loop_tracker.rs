//! (T11) Repeated-action circuit breaker.
//!
//! ## Why this exists
//!
//! On Mistral 7B (and other small classroom models) we routinely observe
//! the *same* failed action being re-dispatched 10–30 times in a row when
//! the model gets stuck on a schema confusion: it ignores the validation
//! error, ignores the wrapper nudge, ignores the diff hint, and keeps
//! emitting the same broken JSON. The session burns context, the user
//! gives up, the classroom session ends in frustration.
//!
//! The circuit breaker is the last line of defence. Per session, we count
//! identical failed invocations keyed by `(action_type, normalized_args,
//! error_code)`. The 5th identical failure is rejected *before* dispatch
//! with a typed `loop_detected` envelope that explicitly tells the model
//! "the runtime stopped you — do not retry, change your approach". Past
//! that wall the model is forced to either pick a different primitive or
//! ask the user.
//!
//! ## Reset semantics
//!
//! - **Different normalized args** ⇒ different key ⇒ counter starts at 0.
//! - **Different error code** ⇒ different key ⇒ counter starts at 0
//!   (a different *kind* of failure means the model is iterating, not
//!   stuck).
//! - **A successful invocation of the same `(action_type, args)`** ⇒
//!   the failure counter for *every* error_code under that prefix is
//!   reset.
//!
//! ## What "normalized args" means
//!
//! `normalize_args` strips out the `type` key and pretty-prints the
//! remaining fields as canonical JSON (sorted keys). That way
//! `{"type":"Add","a":2,"b":3}` and `{"type":"Add","b":3,"a":2}` hash
//! identically — the model is doing the *same* thing, just shuffling key
//! order between turns.
//!
//! ## The second breaker: stationarity
//!
//! `RepeatedActionTracker` only ever sees *failures*, and a success clears its
//! slate outright (`record_outcome(_, None)`). A model that keeps **succeeding**
//! at the same call — reading the same file thirty times in a row — is therefore
//! invisible to it, and burns the budget with nothing to stop it.
//!
//! [`StationarityTracker`] answers the other half of the question: how long is
//! the current run of *identical* calls, outcome irrelevant. It shares
//! `normalize_args` with the failure breaker on purpose — two normalisations
//! that drifted would give the repo two different notions of "the same action".

use std::collections::HashMap;

use serde_json::{json, Value};

/// Threshold at which the breaker trips. The 5th identical failure is the
/// one that gets rejected pre-dispatch; the first four are observed and
/// counted.
pub const LOOP_THRESHOLD: usize = 5;

/// Number of consecutive identical calls — SUCCESSES INCLUDED — after which
/// the turn stops.
///
/// SECURITY: distinct from `LOOP_THRESHOLD` (5), which counts only failures
/// and which a success resets (`record_outcome`, below). An action that
/// succeeds in circles is seen only by this counter.
pub const STATIONARITY_THRESHOLD: usize = 8;

/// Nudge rank: half the threshold, once per streak.
pub const STATIONARITY_NUDGE: usize = 4;

// The order is a property, not a convention: a nudge after the stop would
// never be emitted. Fixed at compile time.
const _: () = assert!(STATIONARITY_NUDGE < STATIONARITY_THRESHOLD);
const _: () = assert!(STATIONARITY_NUDGE > 0);

/// What the counter asks the driver to do.
#[derive(Debug, Clone)]
pub enum Stationary {
    /// Inject `message` into the history and continue.
    Nudge { message: String },
    /// End the turn.
    Stop { action_type: String, run_len: usize },
}

/// Streak of identical calls in progress. Only one streak lives at a time: as
/// soon as an argument changes, the previous one is forgotten.
#[derive(Debug, Default, Clone)]
pub struct StationarityTracker {
    signature: Option<(String, String)>,
    run_len: usize,
    nudged: bool,
}

impl StationarityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the current streak. Called at the start of every turn, in one
    /// place only (`Driver::run_turn_inner`, next to `budget.start_turn()`).
    ///
    /// SECURITY: without this, a `Stop` leaves `run_len` at the top and the
    /// signature in place — the FIRST identical action of the next turn, the
    /// one the human just asked for, would be stopped on the spot and the
    /// session would stay stuck on it. The question asked is "is the model
    /// going in circles WITHIN THIS TURN?"; a streak that survives the turn
    /// measures the session, and two identical human requests seven turns
    /// apart do not make one.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// To be called BEFORE dispatch, on every action.
    pub fn observe(&mut self, action: &Value) -> Option<Stationary> {
        let action_type = action
            .get("type")
            .and_then(|t| t.as_str())
            .map(String::from)?;
        // `normalize_args` sorts the keys: JSON order does not split a streak.
        let key = (action_type.clone(), normalize_args(action));
        if self.signature.as_ref() == Some(&key) {
            self.run_len += 1;
        } else {
            self.signature = Some(key);
            self.run_len = 1;
            self.nudged = false;
        }

        if self.run_len >= STATIONARITY_THRESHOLD {
            return Some(Stationary::Stop {
                action_type,
                run_len: self.run_len,
            });
        }
        if self.run_len >= STATIONARITY_NUDGE && !self.nudged {
            self.nudged = true;
            // English, descriptive, and says NOTHING to the model about its
            // mental state or about any confinement (repository rule).
            return Some(Stationary::Nudge {
                message: format!(
                    "The last {} tool calls were identical: `{}` with the same arguments. \
                     The result will not change. Consider a different approach, or ask the user.",
                    self.run_len, action_type
                ),
            });
        }
        None
    }
}

/// Per-session circuit breaker state. Cheap to construct; lives on the
/// agent loop and the TUI app state.
#[derive(Debug, Default, Clone)]
pub struct RepeatedActionTracker {
    /// (action_type, normalized_args_json, error_code) → consecutive failure count.
    counts: HashMap<(String, String, String), usize>,
}

/// Result of asking the tracker whether an upcoming dispatch should be
/// rejected. We carry the structured error envelope here so the agent
/// loop only needs to surface it verbatim.
#[derive(Debug, Clone)]
pub struct LoopDetected {
    /// Canonical action-result envelope for the rejection. Has the same
    /// shape as a real x0ne-runtime `success: false` payload, with
    /// `error.class = "loop_detected"`. Includes the offending `sent`
    /// payload, the recurring `last_error_code`, and the count for the
    /// reviewer.
    pub envelope: Value,
}

impl RepeatedActionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-dispatch gate: returns `Some(LoopDetected)` when the next
    /// invocation of `(action_type, args)` has already failed
    /// `LOOP_THRESHOLD - 1` times in a row with the same `error_code`.
    ///
    /// Call right before `bridge.send_action`. When `error_code` is
    /// `None`, returns `None` (we cannot predict the future error class
    /// — only past *failures* count toward the breaker).
    pub fn would_block(
        &self,
        action: &Value,
        anticipated_error_code: Option<&str>,
    ) -> Option<LoopDetected> {
        let action_type = action.get("type").and_then(|t| t.as_str())?.to_string();
        let normalized = normalize_args(action);
        let code = anticipated_error_code?.to_string();
        let count = self
            .counts
            .get(&(action_type.clone(), normalized.clone(), code.clone()))
            .copied()
            .unwrap_or(0);
        if count + 1 >= LOOP_THRESHOLD {
            Some(LoopDetected {
                envelope: build_loop_detected_envelope(&action_type, action, &code, count + 1),
            })
        } else {
            None
        }
    }

    /// Record the outcome of a dispatched action. Pass `Some(error_code)`
    /// for failures (typically `error.code` from x0ne-runtime, falling back
    /// to `error.class`), `None` for successes (which clear the slate
    /// for that `(action_type, args)` prefix).
    ///
    /// Returns the new failure count for the keyed signature, useful for
    /// telemetry / tests.
    pub fn record_outcome(&mut self, action: &Value, error_code: Option<&str>) -> usize {
        let Some(action_type) = action
            .get("type")
            .and_then(|t| t.as_str())
            .map(String::from)
        else {
            return 0;
        };
        let normalized = normalize_args(action);
        match error_code {
            Some(code) => {
                let entry = self
                    .counts
                    .entry((action_type, normalized, code.to_string()))
                    .or_insert(0);
                *entry += 1;
                *entry
            }
            None => {
                // Success: clear all error_code variants under (type, args).
                self.counts
                    .retain(|(t, n, _), _| t != &action_type || n != &normalized);
                0
            }
        }
    }

    /// Test/inspection helper: peek at the count for a given signature
    /// without mutating state.
    pub fn count_for(&self, action_type: &str, action: &Value, error_code: &str) -> usize {
        let normalized = normalize_args(action);
        self.counts
            .get(&(action_type.to_string(), normalized, error_code.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Pre-dispatch helper for the agent loop: returns `Some(LoopDetected)`
    /// when *any* recorded error code for `(action_type, args)` has hit
    /// `LOOP_THRESHOLD - 1`. Used when we don't know yet which error code
    /// the next dispatch would produce — if any prior error code is
    /// already on the brink, blocking is the safe default.
    pub fn would_block_any_error(&self, action: &Value) -> Option<LoopDetected> {
        let action_type = action.get("type").and_then(|t| t.as_str())?.to_string();
        let normalized = normalize_args(action);
        let (worst_code, worst_count) = self
            .counts
            .iter()
            .filter(|((t, n, _), _)| t == &action_type && n == &normalized)
            .map(|((_, _, code), count)| (code.as_str(), *count))
            .max_by_key(|(_, c)| *c)?;
        if worst_count + 1 >= LOOP_THRESHOLD {
            Some(LoopDetected {
                envelope: build_loop_detected_envelope(
                    &action_type,
                    action,
                    worst_code,
                    worst_count + 1,
                ),
            })
        } else {
            None
        }
    }
}

/// Canonicalise the action's argument set into a stable, sorted-key JSON
/// string so reordered fields hash to the same bucket. Strips the `type`
/// key (kept separately in the tracker key) so the args portion is just
/// the field set.
fn normalize_args(action: &Value) -> String {
    let Some(obj) = action.as_object() else {
        return action.to_string();
    };
    let mut rest = obj.clone();
    rest.remove("type");
    crate::canonical_json::canonical(&Value::Object(rest)).to_string()
}

/// Build the typed `loop_detected` envelope. Shape mirrors x0ne-runtime's
/// `success: false` results so the existing add_action_result path
/// records it as a normal failure (with the right error class) — the
/// key difference is `retryable: false` on the error object, which the
/// existing T7 plumbing recognises as "do NOT retry".
fn build_loop_detected_envelope(
    action_type: &str,
    action: &Value,
    last_error_code: &str,
    repeat_count: usize,
) -> Value {
    json!({
        "success": false,
        "action_type": action_type,
        "error": {
            "class": "loop_detected",
            "code": "loop_detected",
            "retryable": false,
            "message": format!(
                "Refusing to dispatch `{action_type}`: this exact payload has failed {repeat_count} times \
                 in a row with `{last_error_code}`. The runtime stopped you. \
                 Do NOT retry. Either change the arguments, pick a different primitive, or ask the user."
            ),
            "sent": action.clone(),
            "last_error_code": last_error_code,
            "repeat_count": repeat_count,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(a: i64, b: i64) -> Value {
        json!({"type":"Add","a":a,"b":b})
    }

    #[test]
    fn counts_consecutive_failures_per_signature() {
        let mut t = RepeatedActionTracker::new();
        let a = add(1, 2);
        for expected in 1..=4 {
            let got = t.record_outcome(&a, Some("type_mismatch"));
            assert_eq!(
                got, expected,
                "failure count must increment per identical failure"
            );
        }
        assert_eq!(t.count_for("Add", &a, "type_mismatch"), 4);
    }

    #[test]
    fn would_block_trips_on_fifth_identical_failure() {
        let mut t = RepeatedActionTracker::new();
        let a = add(1, 2);
        // 4 prior failures.
        for _ in 0..4 {
            t.record_outcome(&a, Some("type_mismatch"));
        }
        // 5th attempt MUST be blocked pre-dispatch.
        let blocked = t.would_block(&a, Some("type_mismatch"));
        assert!(
            blocked.is_some(),
            "T11: 5th identical failure must be blocked"
        );
        let env = blocked.unwrap().envelope;
        assert_eq!(env["success"], false);
        assert_eq!(env["error"]["class"], "loop_detected");
        assert_eq!(
            env["error"]["retryable"], false,
            "T11: loop_detected must be retryable=false so T7 framing says 'do not retry'"
        );
        assert_eq!(env["error"]["repeat_count"], 5);
        assert_eq!(env["error"]["sent"], a);
    }

    #[test]
    fn does_not_block_before_threshold() {
        let mut t = RepeatedActionTracker::new();
        let a = add(1, 2);
        for i in 0..3 {
            t.record_outcome(&a, Some("type_mismatch"));
            assert!(
                t.would_block(&a, Some("type_mismatch")).is_none(),
                "T11: must not block at attempt {} (only the 5th identical failure trips)",
                i + 2
            );
        }
    }

    #[test]
    fn key_changes_when_args_change_resets_counter() {
        let mut t = RepeatedActionTracker::new();
        let a1 = add(1, 2);
        let a2 = add(7, 8);
        for _ in 0..4 {
            t.record_outcome(&a1, Some("type_mismatch"));
        }
        // Same action type, *different* args → fresh bucket, no block.
        assert!(
            t.would_block(&a2, Some("type_mismatch")).is_none(),
            "T11: counter must reset when args change"
        );
    }

    #[test]
    fn key_changes_when_error_code_changes_resets_counter() {
        let mut t = RepeatedActionTracker::new();
        let a = add(1, 2);
        for _ in 0..4 {
            t.record_outcome(&a, Some("type_mismatch"));
        }
        // Different error code → different bucket. The model is at least
        // making the failure mode change, which is progress.
        assert!(
            t.would_block(&a, Some("missing_required_field")).is_none(),
            "T11: counter must reset when error_code changes"
        );
    }

    #[test]
    fn success_clears_failure_count_for_same_signature() {
        let mut t = RepeatedActionTracker::new();
        let a = add(1, 2);
        for _ in 0..4 {
            t.record_outcome(&a, Some("type_mismatch"));
        }
        assert_eq!(
            t.record_outcome(&a, None),
            0,
            "success must reset counter to 0"
        );
        assert!(
            t.would_block(&a, Some("type_mismatch")).is_none(),
            "T11: a successful invocation forgives prior failures for the same (type, args)"
        );
    }

    /// The failure counter clears everything on a success
    /// (`record_outcome(_, None)`, `loop_tracker.rs:127`). An action that
    /// SUCCEEDS in a loop is therefore invisible to it.
    ///
    /// A failure of this test would mean that a stuck model rereading the same
    /// file thirty times burns the budget with nothing to stop it — the
    /// existing circuit breaker only sees failures.
    #[test]
    fn a_succeeding_action_repeated_forever_is_still_stopped() {
        let a = json!({"type": "ReadFile", "path": "/etc/hostname"});
        let mut t = StationarityTracker::new();
        for i in 1..STATIONARITY_THRESHOLD {
            // Not `is_none()`: the nudge at rank 4 is a legitimate `Some`, and
            // it does not end the turn. What is forbidden before the threshold
            // is the STOP.
            assert!(
                !matches!(t.observe(&a), Some(Stationary::Stop { .. })),
                "call {i}: stopped too early"
            );
        }
        assert!(
            matches!(t.observe(&a), Some(Stationary::Stop { .. })),
            "the identical call number {} must stop",
            STATIONARITY_THRESHOLD
        );
    }

    /// The nudge fires at half, and only once per streak.
    #[test]
    fn the_nudge_fires_once_at_half_the_threshold() {
        let a = json!({"type": "ListDirectory", "path": "/tmp"});
        let mut t = StationarityTracker::new();
        let mut nudges = 0;
        for _ in 0..(STATIONARITY_THRESHOLD - 1) {
            if matches!(t.observe(&a), Some(Stationary::Nudge { .. })) {
                nudges += 1;
            }
        }
        assert_eq!(nudges, 1, "the nudge must be unique per streak");
    }

    /// A single different field starts again from zero.
    #[test]
    fn one_different_argument_resets_the_run() {
        let mut t = StationarityTracker::new();
        for _ in 0..(STATIONARITY_THRESHOLD - 1) {
            t.observe(&json!({"type": "ReadFile", "path": "/a"}));
        }
        assert!(
            t.observe(&json!({"type": "ReadFile", "path": "/b"}))
                .is_none(),
            "a different argument must reset the streak"
        );
    }

    /// JSON key order does not create two streaks.
    #[test]
    fn key_order_does_not_split_a_run() {
        let mut t = StationarityTracker::new();
        for i in 0..STATIONARITY_THRESHOLD {
            let a = if i % 2 == 0 {
                json!({"type": "ReadFile", "path": "/a", "max_bytes": 10})
            } else {
                json!({"type": "ReadFile", "max_bytes": 10, "path": "/a"})
            };
            // `observe` on EVERY turn: it is the streak that is measured, not
            // the last call. Calling it only under the `if` would make the
            // test pass for the wrong reason — run_len would stay at 1.
            let got = t.observe(&a);
            if i + 1 == STATIONARITY_THRESHOLD {
                assert!(
                    matches!(got, Some(Stationary::Stop { .. })),
                    "key order split the streak"
                );
            }
        }
    }

    /// A `Stop` leaves the streak at its top; it is the next turn that
    /// reopens it.
    ///
    /// A failure of this test would mean that after a stop, the FIRST action
    /// of the next turn — the one the human just asked for in so many words,
    /// "reread it one more time" — would be stopped on the spot, and the
    /// session would stay stuck on that action until another one comes in
    /// between.
    #[test]
    fn a_reset_starts_a_fresh_run_after_a_stop() {
        let a = json!({"type": "ReadFile", "path": "/etc/hostname"});
        let mut t = StationarityTracker::new();
        for _ in 0..STATIONARITY_THRESHOLD {
            t.observe(&a);
        }
        assert!(
            matches!(t.observe(&a), Some(Stationary::Stop { .. })),
            "the streak stays at the top until it is reset"
        );

        t.reset();
        assert!(
            t.observe(&a).is_none(),
            "after a reset, the first identical call starts again at 1"
        );
    }

    /// The failure circuit breaker stays distinct and tighter.
    ///
    /// SECURITY: pins both constants by COMPARISON. A fixture that derived its
    /// size from `STATIONARITY_THRESHOLD` would become a memory bomb as soon
    /// as the threshold is mutated (machine crash of 2026-08-16).
    #[test]
    fn the_two_breakers_keep_their_distinct_thresholds() {
        assert_eq!(LOOP_THRESHOLD, 5);
        assert_eq!(STATIONARITY_NUDGE, 4);
        assert_eq!(STATIONARITY_THRESHOLD, 8);
    }

    #[test]
    fn key_order_does_not_matter_for_normalization() {
        let mut t = RepeatedActionTracker::new();
        let canonical = json!({"type":"Add","a":1,"b":2});
        let shuffled = json!({"type":"Add","b":2,"a":1});
        for _ in 0..4 {
            t.record_outcome(&canonical, Some("type_mismatch"));
        }
        // Shuffled-key version of the *same* args must hit the same bucket
        // — otherwise a model that re-orders keys between turns dodges the
        // breaker forever.
        assert!(
            t.would_block(&shuffled, Some("type_mismatch")).is_some(),
            "T11: normalized args must be order-insensitive"
        );
    }
}
