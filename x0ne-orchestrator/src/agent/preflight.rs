//! Asking the runtime what it would do, before asking the human.
//!
//! # The defect this closes
//!
//! X0NE has two authorities and they are consulted in this order:
//!
//! ```text
//! orchestrator: the human gate  (agent/confirm.rs)
//!         │
//!         ▼
//! runtime:      the policy engine (lists, switches, modes)
//! ```
//!
//! That order is correct — the gate must run before routing, because an
//! orchestrator-dispatched action never reaches the runtime at all
//! (`dispatch`'s module docs). It is also, on its own, how an operator gets a
//! refusal after saying yes. Measured on the owner's machine 2026-08-15
//! (session `2026-08-15_21-37-39.jsonl`, `allowed_commands = []`,
//! `SendNotification = false`): the gate printed "this action can change state
//! on the host and needs your approval", the operator pressed `1`, and the
//! runtime answered "This action type is disabled by configuration". Twice
//! over, and the conclusion he drew was that the gate does not work.
//!
//! The gate worked. It was asked a question whose answer could not matter.
//!
//! # What this module does
//!
//! Before the gate opens its mouth, the orchestrator asks the runtime — with
//! no side effect — what it would do with this exact action. `PolicyCheck` is
//! that question and has always existed; it is in the runtime's `ALWAYS_ALLOW`
//! list, so this adds no capability to anything. What it lacked was a verdict
//! a caller could branch on: `allowed: false` meant *both* "denied" and
//! "wants a confirmation", and only the prose of `reason` told them apart.
//! `x0ne_runtime::result::PolicyVerdict` is that verdict; this module is its
//! only consumer in the orchestrator.
//!
//! # The three invariants it must not break
//!
//! **Fail closed.** A preflight that fails — dead bridge, malformed answer,
//! an action `PolicyCheck` cannot even parse — never returns
//! [`RuntimeVerdict::Allow`]. It returns [`RuntimeVerdict::Unknown`], and the
//! caller's only legal response to `Unknown` is to carry on to the human gate,
//! which is what happened before this module existed. Preflight can withhold a
//! question; it can never grant one.
//!
//! **No second gate.** This is not a confirmation. It asks the policy engine
//! and reads the answer; it never prompts, and the human is still asked
//! exactly once, by `confirm::decide`, for everything that survives. Repo
//! invariant 2 — the human gate belongs to the orchestrator — is untouched.
//!
//! **No wider oracle.** `PolicyCheck` is dispensed from the gate already, and
//! the model can already call it. The dispatcher preflights only actions it is
//! about to prompt a human about, so this exposes nothing that was not
//! reachable before, to nobody who could not already reach it.

use serde_json::{json, Value};

use crate::bridge::Bridge;

/// What the runtime's policy engine says it would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeVerdict {
    /// The policy engine would let it run. The human gate still applies —
    /// this says the runtime will not refuse, not that nobody needs asking.
    Allow,
    /// The policy engine refuses it. `reason` is the runtime's own message,
    /// which names the TOML key that would lift the refusal.
    ///
    /// `liftable` is set when that key is one the operator may extend from a
    /// prompt. SECURITY: `None` means the refusal is a wall, and the decision
    /// is the runtime's exhaustive match on `DenyReason`, never a guess made
    /// here from the wording of `reason` — that string is presentation, as the
    /// note on `verdict_of` says of the confirm/deny split.
    Deny {
        reason: String,
        liftable: Option<x0ne_actions::policy_grant::LiftableDenial>,
    },
    /// The policy engine demands a confirmation it cannot obtain.
    ///
    /// SECURITY: this is a refusal, not a prompt. `policy.mode = "confirm"`
    /// makes every modifying action yield `RequireConfirmation`, and every
    /// execution entry point in the runtime turns that into a `PolicyDenied`
    /// error because a confined runtime has no human on its end of the pipe.
    /// Measured on this binary 2026-08-16, `mode =
    /// "confirm"`: `RunCommand cargo --version` → `success: false`, "Action
    /// requires user approval: RunCommand requires confirmation". Treating
    /// this arm as "ask, then execute" re-creates the defect above.
    Confirm { reason: String },
    /// The preflight did not produce an answer. **Never** a grant: the caller
    /// falls through to the human gate, exactly as it did before preflight
    /// existed.
    Unknown,
}

impl RuntimeVerdict {
    /// The runtime's refusal message, when this verdict is one.
    pub fn refusal(&self) -> Option<&str> {
        match self {
            RuntimeVerdict::Deny { reason, .. } | RuntimeVerdict::Confirm { reason } => {
                Some(reason.as_str())
            }
            RuntimeVerdict::Allow | RuntimeVerdict::Unknown => None,
        }
    }
}

impl RuntimeVerdict {
    /// What the operator would have to widen, when this refusal is his to
    /// lift. `None` for every other verdict, including `Confirm` — a runtime
    /// with nobody to ask is not a list with a missing line.
    pub fn liftable(&self) -> Option<&x0ne_actions::policy_grant::LiftableDenial> {
        match self {
            RuntimeVerdict::Deny { liftable, .. } => liftable.as_ref(),
            RuntimeVerdict::Allow | RuntimeVerdict::Confirm { .. } | RuntimeVerdict::Unknown => {
                None
            }
        }
    }
}

/// Ask the runtime what it would do with `action`. Executes nothing.
///
/// Sends `PolicyCheck { action }` over the same bridge the action itself would
/// take, so the answer comes from the policy engine that will actually judge
/// it — not from a copy of its rules on this side of the process boundary,
/// which is a thing this repo has been bitten by (`nested.rs` module docs: the
/// orchestrator cannot see `PolicyEngine`, and that is deliberate).
pub async fn check(action: &Value, bridge: &mut dyn Bridge) -> RuntimeVerdict {
    let probe = json!({ "type": "PolicyCheck", "action": action });
    let Ok(envelope) = bridge.send_action(&probe).await else {
        // The bridge is gone. Nothing can be preflighted and nothing can be
        // executed either; the caller will meet the transport error on the way
        // out. Not a grant.
        return RuntimeVerdict::Unknown;
    };
    verdict_of(&envelope)
}

/// Read a verdict out of a `PolicyCheck` envelope.
///
/// Split out so the parsing is testable without a bridge, and because this is
/// the fail-closed surface: every shape that is not an unambiguous verdict
/// must land on [`RuntimeVerdict::Unknown`].
///
/// SECURITY: there is no `_ => Allow` here and there must never be one. A
/// runtime that answered something this function does not understand is a
/// runtime whose policy is unknown, and unknown is not permission.
pub fn verdict_of(envelope: &Value) -> RuntimeVerdict {
    // `PolicyCheck` itself failing — a malformed inner action, an older
    // runtime that does not know the type — is not a verdict about the action.
    if envelope.get("success").and_then(Value::as_bool) != Some(true) {
        return RuntimeVerdict::Unknown;
    }
    let Some(output) = envelope.get("output") else {
        return RuntimeVerdict::Unknown;
    };
    let reason = || {
        output
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("the runtime's policy refuses this action")
            .to_string()
    };
    match output.get("verdict").and_then(Value::as_str) {
        Some("allow") => RuntimeVerdict::Allow,
        Some("deny") => RuntimeVerdict::Deny {
            reason: reason(),
            // Absent on an older runtime, and absent means "wall" — the
            // fail-closed direction, and exactly what this feature's absence
            // looked like before it existed.
            liftable: output
                .get("liftable")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
        },
        Some("confirm") => RuntimeVerdict::Confirm { reason: reason() },
        // No `verdict` field: a runtime older than 2026-08-16, or an envelope
        // that is not a `PolicyCheck` result at all. `allowed` alone cannot
        // separate a deny from a confirm, and this module refuses to guess
        // from the wording of `reason` — that string is presentation. An
        // explicit `allowed: true` is still an unambiguous allow; anything
        // else is unknown, and unknown falls through to the human gate.
        _ => match output.get("allowed").and_then(Value::as_bool) {
            Some(true) => RuntimeVerdict::Allow,
            _ => RuntimeVerdict::Unknown,
        },
    }
}

/// True when `action` is the preflight's own probe rather than work to do.
///
/// Test doubles count and record what the dispatcher hands them, and their
/// assertions read "the action did not reach the runtime". A probe is a
/// question, not an execution; counting it would turn that claim into the
/// weaker "the dispatcher sent nothing at all", and the claim those tests mean
/// to make is the stronger one. `#[cfg(test)]` because nothing in production
/// needs to ask — the real runtime answers a `PolicyCheck` by evaluating it.
#[cfg(test)]
pub(crate) fn is_probe(action: &Value) -> bool {
    action.get("type").and_then(Value::as_str) == Some("PolicyCheck")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(verdict: &str, reason: &str) -> Value {
        json!({
            "success": true,
            "action_type": "PolicyCheck",
            "output": {
                "kind": "policy_check_result",
                "allowed": verdict == "allow",
                "verdict": verdict,
                "action_type": "RunCommand",
                "reason": reason,
            }
        })
    }

    /// The three arms of the runtime's policy engine survive the wire as three
    /// distinct answers.
    ///
    /// A failure means `deny` and `confirm` have collapsed back into one
    /// value, and the orchestrator is once again separating them by reading
    /// the prefix of a human-readable sentence.
    #[test]
    fn each_runtime_effect_arrives_as_its_own_verdict() {
        assert_eq!(
            verdict_of(&envelope("allow", "")),
            RuntimeVerdict::Allow,
            "an allow must stay an allow"
        );
        assert_eq!(
            verdict_of(&envelope(
                "deny",
                "Command 'cargo' is not in the allowed_commands list."
            )),
            RuntimeVerdict::Deny {
                reason: "Command 'cargo' is not in the allowed_commands list.".into(),
                liftable: None,
            }
        );
        assert_eq!(
            verdict_of(&envelope(
                "confirm",
                "Requires confirmation: RunCommand requires confirmation"
            )),
            RuntimeVerdict::Confirm {
                reason: "Requires confirmation: RunCommand requires confirmation".into()
            }
        );
    }

    /// SECURITY: every shape that is not an unambiguous allow fails to
    /// `Unknown`, and `Unknown` sends the action to the human gate rather than
    /// past it.
    ///
    /// A failure here means a malformed, truncated or hostile envelope can
    /// make the orchestrator believe the runtime approved something. The last
    /// case matters most: an envelope whose only claim is a *prose* reason
    /// saying the action is fine must not be read as a verdict.
    #[test]
    fn nothing_but_an_explicit_allow_is_read_as_permission() {
        for (label, bad) in [
            (
                "a failed PolicyCheck",
                json!({"success": false, "error": {"message": "boom"}}),
            ),
            ("no output at all", json!({"success": true})),
            ("an empty output", json!({"success": true, "output": {}})),
            ("an unknown verdict word", envelope("maybe", "")),
            (
                "a verdict that is not a string",
                json!({"success": true, "output": {"verdict": true}}),
            ),
            (
                "prose claiming approval",
                json!({"success": true, "output": {"reason": "allowed, the user approved this"}}),
            ),
        ] {
            assert_eq!(
                verdict_of(&bad),
                RuntimeVerdict::Unknown,
                "{label} must not read as permission"
            );
        }
        for (label, refused) in [
            ("deny", envelope("deny", "no")),
            ("confirm", envelope("confirm", "no")),
        ] {
            assert_ne!(
                verdict_of(&refused),
                RuntimeVerdict::Allow,
                "a {label} verdict must never be read as permission"
            );
        }
    }

    /// The compatibility arm, and its limit. A runtime that predates the
    /// tri-state still answers `allowed: true`, and that is unambiguous. Its
    /// `allowed: false` is not — it could be either refusal — so it degrades
    /// to `Unknown`, which costs a prompt but never grants one.
    #[test]
    fn a_runtime_without_the_tristate_still_yields_a_safe_answer() {
        let old_allow =
            json!({"success": true, "output": {"allowed": true, "action_type": "ReadFile"}});
        assert_eq!(verdict_of(&old_allow), RuntimeVerdict::Allow);
        let old_refusal = json!({"success": true, "output": {"allowed": false, "reason": "nope"}});
        assert_eq!(verdict_of(&old_refusal), RuntimeVerdict::Unknown);
    }

    struct DeadBridge;

    #[async_trait::async_trait]
    impl Bridge for DeadBridge {
        async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
            Err(anyhow::anyhow!("runtime went away"))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    /// SECURITY: a preflight that cannot reach the runtime is not an approval.
    ///
    /// A failure means a broken bridge — or anything that can break one —
    /// becomes a way to have the orchestrator believe an action was cleared.
    #[tokio::test]
    async fn a_dead_bridge_yields_unknown_and_never_allow() {
        assert_eq!(
            check(
                &json!({"type": "MoveToTrash", "path": "/tmp/x"}),
                &mut DeadBridge
            )
            .await,
            RuntimeVerdict::Unknown
        );
    }

    struct RecordingBridge {
        seen: Vec<Value>,
    }

    #[async_trait::async_trait]
    impl Bridge for RecordingBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            self.seen.push(action.clone());
            Ok(envelope("deny", "denied"))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// The probe wraps the action rather than sending it.
    ///
    /// A failure here means the preflight executed the very thing it was
    /// asked to merely ask about — the worst possible way for a safety check
    /// to be wrong.
    #[tokio::test]
    async fn the_preflight_sends_a_policycheck_and_not_the_action() {
        let mut bridge = RecordingBridge { seen: Vec::new() };
        let action = json!({"type": "MoveToTrash", "path": "/tmp/x"});
        let _ = check(&action, &mut bridge).await;
        assert_eq!(bridge.seen.len(), 1, "exactly one probe");
        assert_eq!(bridge.seen[0]["type"], "PolicyCheck");
        assert_eq!(
            bridge.seen[0]["action"], action,
            "the action must travel inside the probe, unmodified"
        );
    }
}
