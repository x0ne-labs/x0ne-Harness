//! The audit seam of the canonical path: what a turn *did*, written down.
//!
//! # Why this is a trait and not a `SessionLogger`
//!
//! The journal was already implemented — `session::SessionLogger::log_action`
//! writes the two lines the README promises — and it was already called, from
//! twelve places inside `agent_loop.rs`. None of them ran: the only two
//! callers of `run_agent_loop` pass `None` for the logger, and the canonical
//! path (`CliAgentSession → Driver → TurnReport`) had no reference to a logger
//! at all. Measured 2026-08-15 on `~/.x0ne/sessions`: 22 real sessions
//! declaring 250 actions in their `turn` events and **zero** `action` lines;
//! the 360 `action` events that do exist across 40 sessions are all
//! `command == "gcloud"`, written by the one live caller
//! (`x0ne-code/src/cc_cmd.rs:1147`).
//!
//! That is this repo's signature defect — the dead scaffold (see docs/invariants.md):
//! a feature that compiles, is documented, has tests, and is not in the path.
//! So the journal is expressed here as a **capability the driver holds**,
//! next to the bridge and the sink, rather than as a type a frontend happens
//! to own. `TurnContext.journal` is `None` only where nothing should be
//! recorded, and the choice is visible at every construction site.
//!
//! `Arc<dyn ActionJournal>` and `&self` methods, not `&mut`: one session log
//! has several writers — the frontend writes `user_input`/`llm_response`/
//! `turn`, the driver writes the actions, a detached world writes from its own
//! task — and they must land in the *same* file. A `&mut` handle would have
//! forced a second `SessionLogger`, which means a second timestamped path and
//! a session split in two.

use serde_json::Value;

/// Why an action produced no effect.
///
/// Three distinct facts an auditor needs to tell apart, which a single
/// `policy_denial` line collapsed: a human who said no, a context where no
/// human could be asked (fail-closed — see docs/invariants.md, invariant 1), and the
/// runtime's own policy engine refusing what it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialKind {
    /// A human was asked and declined.
    HumanDeclined,
    /// No human could be asked: piped stdin, headless server, sub-agent.
    NoHumanChannel,
    /// The runtime refused: `error.class == "policy_denied"` came back on the
    /// envelope. Journaled *in addition to* the action's own two lines, since
    /// the action was really dispatched and really answered.
    RuntimePolicy,
    /// A `[[hooks.pre_tool]]` command refused it before any route.
    Hook,
    /// Plan mode refused it before any route (`agent::plan_mode`).
    PlanMode,
}

impl DenialKind {
    /// Stable string for the log. Greppable, and it never changes shape.
    pub fn as_str(self) -> &'static str {
        match self {
            DenialKind::HumanDeclined => "human_declined",
            DenialKind::NoHumanChannel => "no_human_channel",
            DenialKind::RuntimePolicy => "runtime_policy",
            DenialKind::Hook => "hook",
            DenialKind::PlanMode => "plan_mode",
        }
    }
}

/// Where the canonical path records side effects.
///
/// Both methods swallow their errors on purpose: a full disk must not stop a
/// turn, and there is no caller that could act on the failure. What must never
/// happen is the opposite — a turn that runs and writes nothing — and that is
/// what the wiring, not the error type, guarantees.
#[async_trait::async_trait]
pub trait ActionJournal: Send + Sync {
    /// One dispatched action and the envelope it came back with, including a
    /// failed or cancelled one. Two lines: `action` then `action_result`.
    async fn journal_action(&self, action: &Value, result: &Value, duration_ms: u64);

    /// An action that was refused, and by whom.
    async fn journal_denial(&self, action: &Value, kind: DenialKind, detail: &str);

    /// The operator widened his own policy mid-session.
    ///
    /// SECURITY: this is the only trace a grant leaves. The runtime's copy
    /// lives in memory and dies with the process; a session grant writes
    /// nothing anywhere. An auditor who cannot see this line sees an action
    /// refused and then, moments later, the same action succeeding, with no
    /// explanation between them — and the explanation is "a human was asked
    /// and said yes", which is the single most important fact about it.
    ///
    /// `persisted` carries what was written to the config file, or `None` when
    /// the widening was for this session only (or when the write failed) — so
    /// the log distinguishes a grant that is gone at exit from one that is
    /// still there tomorrow.
    async fn journal_policy_grant(
        &self,
        action: &Value,
        kind: &str,
        value: &str,
        persist: bool,
        persisted: Option<&str>,
    );

    /// A stated test/build result with no evidence behind it
    /// (`agent::honesty`): a refused report write or a flagged answer.
    async fn journal_unverified_claim(&self, detection: &Value);

    /// A result read by the agent looked like instructions for it.
    async fn journal_injection_suspected(&self, detection: &Value) {
        let _ = detection;
    }

    /// The session log's id, handed to hooks as `session_id`.
    fn session_id(&self) -> Option<String> {
        None
    }
}

/// A journal that records nothing and names its session, for hook tests.
#[cfg(test)]
pub(crate) struct NamedJournal(pub String);

#[cfg(test)]
#[async_trait::async_trait]
impl ActionJournal for NamedJournal {
    async fn journal_action(&self, _: &Value, _: &Value, _: u64) {}
    async fn journal_denial(&self, _: &Value, _: DenialKind, _: &str) {}
    async fn journal_policy_grant(&self, _: &Value, _: &str, _: &str, _: bool, _: Option<&str>) {}
    async fn journal_unverified_claim(&self, _: &Value) {}
    fn session_id(&self) -> Option<String> {
        Some(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_denial_kind_has_its_own_greppable_name() {
        // An auditor filtering on `human_declined` must not also match the
        // refusals no human ever saw.
        let names = [
            DenialKind::HumanDeclined.as_str(),
            DenialKind::NoHumanChannel.as_str(),
            DenialKind::RuntimePolicy.as_str(),
            DenialKind::Hook.as_str(),
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "two denial kinds share a name: {names:?}");
    }
}
