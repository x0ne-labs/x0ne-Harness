//! Goal types — persistent across sessions.
//!
//! A `Goal` is a long-lived intent ("finish the auth refactor and make all
//! tests pass") that survives between agent turns and even between `x0ne`
//! invocations. It carries a budget, a history of steps, and an optional
//! self-check that decides when the goal has been achieved.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// State machine for a goal. Transitions are driven by `x0ne_goal_*` calls.
///
/// - `Active`: the agent should keep working on it; tick increments allowed.
/// - `Paused`: explicitly paused by the user (`x0ne_goal_pause`).
/// - `Achieved`: the achievement_check returned ok, or `x0ne_goal_complete`
///   was called explicitly.
/// - `Blocked { reason }`: the agent ran into something it cannot resolve
///   without human input; the agent itself sets this via `x0ne_goal_block`.
/// - `BudgetExhausted`: max_steps or max_seconds was reached.
/// - `Cleared`: dropped by `x0ne_goal_clear` — kept for one tick of
///   discoverability, then removed from disk.
///
/// `Deserialize` is hand-written, below: tolerant on input, fail-closed on
/// output. Do not replace it with the derive.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Achieved,
    Blocked { reason: String },
    BudgetExhausted,
    Cleared,
}

impl GoalStatus {
    /// Rebuilds a status from what a file actually contains.
    ///
    /// SECURITY: the only place that decides an entry is unreadable, and the
    /// only answer to that question is `Paused`. Repository invariant 1
    /// (fail-closed) applied to persistence: a damaged snapshot — partial
    /// write, migration, hand edit — must never come back as `Active`, i.e.
    /// as a loop that advances on its own. Returning `Paused` costs the
    /// operator a `/goal resume`; returning `Active` costs them an agent that
    /// resumes a goal nobody has reread.
    ///
    /// No `_` arm on the `match` over known names: it matches `&str`, not the
    /// enum, and the exhaustiveness that matters (invariant 3) is that of
    /// `as_str`, of which this `match` is the exact mirror. A variant added
    /// without its line here would read back as paused — fail-closed, again.
    fn from_persisted(kind: Option<&str>, reason: Option<String>) -> Self {
        match kind {
            Some("active") => GoalStatus::Active,
            Some("paused") => GoalStatus::Paused,
            Some("achieved") => GoalStatus::Achieved,
            // A `blocked` without a reason stays blocked: the reason is text
            // for the human, its absence is not a reason to hand control back
            // to the loop.
            Some("blocked") => GoalStatus::Blocked {
                reason: reason.unwrap_or_else(|| "(reason missing from the stored goal)".into()),
            },
            Some("budget_exhausted") => GoalStatus::BudgetExhausted,
            Some("cleared") => GoalStatus::Cleared,
            Some(_) | None => GoalStatus::Paused,
        }
    }
}

impl<'de> Deserialize<'de> for GoalStatus {
    /// Reads both forms a file can carry — the tagged object that `Serialize`
    /// writes (`{"kind":"active"}`) and the bare string a hand writes
    /// (`"active"`) — and returns `Paused` for everything else, including a
    /// number, a `null`, a list, or an unknown tag.
    ///
    /// SECURITY: none of these inputs may produce an ERROR either. An error
    /// would fail the reload of the whole goal, `list()` would skip it as
    /// "corrupt" (see `store.rs`), and the goal would vanish from the
    /// inventory instead of showing up there as paused.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct StatusVisitor;

        impl<'de> Visitor<'de> for StatusVisitor {
            type Value = GoalStatus;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a goal status: {\"kind\":\"active\"} or \"active\"")
            }

            fn visit_str<E>(self, v: &str) -> Result<GoalStatus, E> {
                Ok(GoalStatus::from_persisted(Some(v), None))
            }

            // Anything that is neither a string nor a map lands here, and lands
            // as paused. Every `visit_*` is written: the `Visitor` default is
            // to return an error, and an error is not the intended answer
            // (see the doc-comment above).
            fn visit_bool<E>(self, _: bool) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_i64<E>(self, _: i64) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_u64<E>(self, _: u64) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_f64<E>(self, _: f64) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_bytes<E>(self, _: &[u8]) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_unit<E>(self) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_none<E>(self) -> Result<GoalStatus, E> {
                Ok(GoalStatus::Paused)
            }
            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<GoalStatus, D::Error> {
                GoalStatus::deserialize(d)
            }
            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<GoalStatus, D::Error> {
                GoalStatus::deserialize(d)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<GoalStatus, A::Error> {
                // Drained, not ignored: a non-self-describing format requires
                // the sequence to be consumed before returning.
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(GoalStatus::Paused)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<GoalStatus, A::Error> {
                let mut kind: Option<String> = None;
                let mut reason: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    // The value goes through `serde_json::Value` rather than
                    // `String`: `{"kind":42}` must read as "unreadable", not
                    // fail the reload on an unexpected type.
                    let value: Value = map.next_value()?;
                    match key.as_str() {
                        "kind" => kind = value.as_str().map(str::to_owned),
                        "reason" => reason = value.as_str().map(str::to_owned),
                        _ => {}
                    }
                }
                Ok(GoalStatus::from_persisted(kind.as_deref(), reason))
            }
        }

        d.deserialize_any(StatusVisitor)
    }
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Achieved => "achieved",
            GoalStatus::Blocked { .. } => "blocked",
            GoalStatus::BudgetExhausted => "budget_exhausted",
            GoalStatus::Cleared => "cleared",
        }
    }

    /// `true` if a `step` call should be allowed to advance state.
    pub fn is_actionable(&self) -> bool {
        matches!(self, GoalStatus::Active)
    }
}

/// When the second skeptic pass runs, for THIS goal.
///
/// A skeptic pass is **one more model call per turn**: it is money on every
/// turn, and therefore an operator decision. It lives with the goal —
/// persisted in `~/.x0ne/goals/<id>.json`, set with `/goal skeptic on|off` —
/// and not in a code constant: a setting that cannot be set is a dead setting
/// (docs/invariants.md, "dead scaffolding", "configuration" form).
///
/// `Deserialize` is hand-written, below. Do not replace it with the
/// derive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkepticCadence {
    /// The O-2 spec default: on every turn where the goal is active and the
    /// turn produced a report.
    #[default]
    EveryTurn,
    /// Never. The operator chose not to pay for this second call.
    Off,
}

impl SkepticCadence {
    /// Does this turn go through the skeptic?
    ///
    /// Exhaustive, no `_` arm: a new cadence ("one turn in three", "only after
    /// a write action") must force a decision here, not inherit a default —
    /// repository invariant 3 applied to spending.
    pub fn runs_this_turn(&self) -> bool {
        match self {
            SkepticCadence::EveryTurn => true,
            SkepticCadence::Off => false,
        }
    }
}

impl<'de> Deserialize<'de> for SkepticCadence {
    /// SECURITY: an unreadable value returns `EveryTurn`, never `Off`.
    ///
    /// Same reasoning as `GoalStatus::from_persisted` (T15), applied in the
    /// other direction: what a damaged snapshot must not be able to do, here,
    /// is **remove a check**. Turning the skeptic off requires having written
    /// `"off"` on purpose; everything else — unknown tag, number, `null`,
    /// object, `"OFF"` — leaves the second pass in place.
    ///
    /// And, as for the status, none of these inputs returns an ERROR: an
    /// error would fail the reload of the whole goal and make it vanish from
    /// the inventory (`store::list`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // `Value` first: the field is self-describing JSON (the only format
        // of `store.rs`), and `{"skeptic": 42}` must read as "unreadable"
        // rather than fail the reload on an unexpected type.
        let raw = Value::deserialize(d)?;
        Ok(match raw.as_str() {
            Some("off") => SkepticCadence::Off,
            _ => SkepticCadence::EveryTurn,
        })
    }
}

/// Budget tracker. Both fields are optional; `None` means unlimited on
/// that axis. The server enforces whichever is tighter.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<u64>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub steps_used: u64,
}

impl Budget {
    pub fn new(max_steps: Option<u64>, max_seconds: Option<u64>) -> Self {
        Self {
            max_steps,
            max_seconds,
            started_at: Utc::now(),
            steps_used: 0,
        }
    }

    /// Returns `Some("steps" | "time")` if the budget is now exhausted.
    pub fn check_exhausted(&self) -> Option<&'static str> {
        if let Some(m) = self.max_steps {
            if self.steps_used >= m {
                return Some("steps");
            }
        }
        if let Some(m) = self.max_seconds {
            let elapsed = (Utc::now() - self.started_at).num_seconds();
            if elapsed >= m as i64 {
                return Some("time");
            }
        }
        None
    }

    pub fn elapsed_seconds(&self) -> i64 {
        (Utc::now() - self.started_at).num_seconds()
    }
}

/// Optional automatic achievement check. Each variant maps to a runtime
/// action JSON object that the orchestrator will route through the
/// downstream runtime — meaning the same policy that protects the agent
/// loop also protects the goal-completion check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AchievementCheck {
    /// Always-false: the agent must call `x0ne_goal_complete` explicitly.
    Manual,
    /// True iff the file exists. Wraps a `file_info` runtime call.
    FileExists { path: String },
    /// True iff the file contains the substring. Wraps `read_file` + a
    /// substring match (done in-orchestrator after the read).
    FileContains { path: String, needle: String },
    /// True iff `command` exits 0 (default) or with `expected_exit`. Routed
    /// through the runtime's `run_command` — so the standard policy gates
    /// (allow_run_commands, allow_exec, etc.) apply.
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        expected_exit: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// True iff the runtime returns success for an arbitrary action. The
    /// raw JSON is forwarded verbatim. Use this for `assert`, `policy_check`,
    /// or any future primitive — the contract is just `success == true`.
    Action { action: Value },
}

/// One iteration entry in the goal's history. We append on every meaningful
/// state change so the user can audit "what did the agent do for this goal?"
/// without grepping logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    pub kind: HistoryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievement_outcome: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Created,
    Step,
    Paused,
    Resumed,
    AchievementCheck,
    /// A second model pass named (or could not name) the gaps in the report.
    /// The note carries the verdict, the model that answered and the
    /// fingerprint — enough to reread a stall after the fact.
    Skeptic,
    Achieved,
    Blocked,
    BudgetExhausted,
    Completed,
    Cleared,
}

/// The full persisted shape of a goal. Lives at
/// `~/.x0ne/goals/<id>.json`. Atomic writes via tempfile+rename in
/// `store.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub id: String,
    pub text: String,
    pub status: GoalStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub budget: Budget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievement_check: Option<AchievementCheck>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    /// Fingerprint of the gaps from the last skeptic pass that had one.
    ///
    /// Persisted with the goal, not only held in memory: the stall guard must
    /// survive a restart between two turns, otherwise relaunching `x0ne` is
    /// enough to start the same loop again.
    /// `#[serde(default)]`: a goal written before this task reads back
    /// without a counter, and without an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gap_fingerprint: Option<String>,
    /// How many times in a row `last_gap_fingerprint` came back identical.
    /// `>= GOAL_STALL_THRESHOLD` means "the model is going in circles".
    #[serde(default)]
    pub gap_repeat_count: usize,
    /// When the second skeptic pass runs for this goal.
    ///
    /// `#[serde(default)]`: a goal written before this task reads back with
    /// the default cadence (every turn), without error — therefore WITH the
    /// skeptic, which is the fail-closed direction (see [`SkepticCadence`]).
    #[serde(default)]
    pub skeptic: SkepticCadence,
}

impl GoalState {
    pub fn new(
        id: String,
        text: String,
        budget: Budget,
        achievement_check: Option<AchievementCheck>,
    ) -> Self {
        let now = Utc::now();
        let mut s = Self {
            id,
            text,
            status: GoalStatus::Active,
            created_at: now,
            updated_at: now,
            budget,
            achievement_check,
            history: Vec::new(),
            last_gap_fingerprint: None,
            gap_repeat_count: 0,
            skeptic: SkepticCadence::default(),
        };
        s.history.push(HistoryEntry {
            at: now,
            kind: HistoryKind::Created,
            note: None,
            plan_id: None,
            achievement_outcome: None,
        });
        s
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub fn append_history(
        &mut self,
        kind: HistoryKind,
        note: Option<String>,
        plan_id: Option<String>,
        achievement_outcome: Option<String>,
    ) {
        let entry = HistoryEntry {
            at: Utc::now(),
            kind,
            note,
            plan_id,
            achievement_outcome,
        };
        self.history.push(entry);
        self.touch();
    }

    /// Records the gap fingerprint of a skeptic pass, and says whether **the
    /// same one** comes back — that is, whether the goal is going in circles.
    ///
    /// `true` from `GOAL_STALL_THRESHOLD` consecutive passes carrying the same
    /// fingerprint: two reports that name exactly the same unmeasured lines
    /// are not two attempts, they are the same one.
    ///
    /// SECURITY: an **empty** fingerprint counts for nothing — neither
    /// repetition nor progress — and leaves the streak intact. An empty
    /// fingerprint means "no citation, hence no stable fingerprint"
    /// (`gap_fingerprint`); counting it would block, on the second turn, any
    /// goal whose report cites no file.
    pub fn record_gap_fingerprint(&mut self, fingerprint: &str) -> bool {
        use super::skeptic::GOAL_STALL_THRESHOLD;

        if fingerprint.is_empty() {
            return false;
        }
        if self.last_gap_fingerprint.as_deref() == Some(fingerprint) {
            self.gap_repeat_count = self.gap_repeat_count.saturating_add(1);
        } else {
            self.last_gap_fingerprint = Some(fingerprint.to_string());
            self.gap_repeat_count = 1;
        }
        self.gap_repeat_count >= GOAL_STALL_THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_steps_exhaustion() {
        let mut b = Budget::new(Some(3), None);
        b.steps_used = 2;
        assert!(b.check_exhausted().is_none());
        b.steps_used = 3;
        assert_eq!(b.check_exhausted(), Some("steps"));
    }

    #[test]
    fn status_is_actionable_only_when_active() {
        assert!(GoalStatus::Active.is_actionable());
        assert!(!GoalStatus::Paused.is_actionable());
        assert!(!GoalStatus::Achieved.is_actionable());
        assert!(!GoalStatus::Blocked { reason: "x".into() }.is_actionable());
        assert!(!GoalStatus::BudgetExhausted.is_actionable());
    }

    #[test]
    fn goal_state_seeds_history_with_created_entry() {
        let g = GoalState::new(
            "goal_1".into(),
            "test goal".into(),
            Budget::new(None, None),
            None,
        );
        assert_eq!(g.history.len(), 1);
        assert!(matches!(g.history[0].kind, HistoryKind::Created));
        assert_eq!(g.status, GoalStatus::Active);
    }

    /// A complete goal snapshot in which only the status varies. Fixed size,
    /// no field derived from a constant: what is tested here is reading a
    /// damaged byte, not the ability to allocate.
    fn goal_json(status_wire: &str) -> String {
        format!(
            r#"{{"id":"g1","text":"t","status":{status_wire},
                "created_at":"2026-08-16T00:00:00Z",
                "updated_at":"2026-08-16T00:00:00Z",
                "budget":{{"started_at":"2026-08-16T00:00:00Z","steps_used":0}},
                "history":[]}}"#
        )
    }

    /// An unreadable goal status reads back as PAUSED.
    ///
    /// SECURITY: invariant 1 ("the absence of an answer is never an
    /// approval") applied to persistence. A corrupt snapshot — partial write,
    /// migration, hand edit — must not come back in self-driving mode. A
    /// failure would mean that a damaged file hands control to a loop that
    /// advances on its own.
    #[test]
    fn an_unreadable_goal_status_reads_back_as_paused() {
        for wire in [
            "\"martian\"",
            "\"\"",
            "42",
            "-1",
            "1.5",
            "null",
            "true",
            "[]",
            "[\"active\"]",
            "{}",
            r#"{"kind":"martian"}"#,
            r#"{"kind":42}"#,
            r#"{"kind":null}"#,
            r#"{"reason":"x"}"#,
            r#"{"kind":"active","kind_typo":"active"}"#,
        ] {
            let parsed: GoalState = serde_json::from_str(&goal_json(wire)).unwrap_or_else(|e| {
                panic!("{wire}: reload must yield a paused goal, not fail ({e})")
            });
            // `{"kind":"active","kind_typo":…}` is the only one in the list
            // whose `kind` is readable: it stays active, and that is intended
            // — an extra unknown field does not damage a status that reads.
            let expected_active = wire.contains("kind_typo");
            if expected_active {
                assert!(
                    matches!(parsed.status, GoalStatus::Active),
                    "{wire}: an unknown field must not pause a readable status"
                );
            } else {
                assert!(
                    matches!(parsed.status, GoalStatus::Paused),
                    "{wire}: an unreadable status must reload as paused, got {:?}",
                    parsed.status
                );
                assert!(
                    !parsed.status.is_actionable(),
                    "{wire}: an unreadable status must never be actionable"
                );
            }
        }
    }

    /// Tolerance does not cost fidelity: the known forms read back unchanged,
    /// in their tagged form (what `Serialize` writes) and in the bare string
    /// form (what a hand writes).
    #[test]
    fn a_known_goal_status_reads_back_as_itself() {
        let cases: Vec<(&str, GoalStatus)> = vec![
            (r#"{"kind":"active"}"#, GoalStatus::Active),
            (r#"{"kind":"paused"}"#, GoalStatus::Paused),
            (r#"{"kind":"achieved"}"#, GoalStatus::Achieved),
            (
                r#"{"kind":"budget_exhausted"}"#,
                GoalStatus::BudgetExhausted,
            ),
            (r#"{"kind":"cleared"}"#, GoalStatus::Cleared),
            (
                r#"{"kind":"blocked","reason":"needs a key"}"#,
                GoalStatus::Blocked {
                    reason: "needs a key".into(),
                },
            ),
            ("\"active\"", GoalStatus::Active),
            ("\"cleared\"", GoalStatus::Cleared),
        ];
        for (wire, expected) in cases {
            let parsed: GoalState = serde_json::from_str(&goal_json(wire)).unwrap();
            assert_eq!(parsed.status, expected, "{wire}");
        }
    }

    /// What the disk contains today must keep reading back: the hand-written
    /// `Deserialize` replaces the derived one, and a form not covered would
    /// silently turn every active goal into a paused goal.
    #[test]
    fn every_status_round_trips_through_its_persisted_form() {
        for status in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Achieved,
            GoalStatus::Blocked {
                reason: "waiting on the operator".into(),
            },
            GoalStatus::BudgetExhausted,
            GoalStatus::Cleared,
        ] {
            let wire = serde_json::to_string(&status).unwrap();
            let back: GoalStatus = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, status, "persisted form: {wire}");
        }
    }

    #[test]
    fn achievement_check_serde_round_trips() {
        let check = AchievementCheck::Command {
            command: "cargo".into(),
            args: vec!["test".into()],
            cwd: Some("./crate".into()),
            expected_exit: 0,
            timeout_secs: Some(120),
        };
        let s = serde_json::to_string(&check).unwrap();
        let back: AchievementCheck = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, AchievementCheck::Command { .. }));
    }
}
