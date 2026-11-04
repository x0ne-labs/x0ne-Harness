//! The presentation boundary.
//!
//! Everything downstream of the canonical agent loop is a *sink*: it renders
//! what happened and may collect input, and it can do nothing else. Execution
//! semantics — policy, confirmation, dispatch, retries, loop detection,
//! budgets, cancellation — live in the loop, on the other side of this trait.
//!
//! ## Why the event type is `AgentEvent`
//!
//! There were two candidate abstractions. The one that lived here was a
//! `DisplayEvent` enum with zero call sites, and it could not have served: its
//! variants carried terminal cursor mechanics (`EraseStreamedTokens`,
//! `CollapsedAction { erase_lines }`) that only a rustyline CLI can honour,
//! and it carried already-rendered strings, meaning the loop had to format for
//! the CLI before handing the event over. That is presentation inside
//! execution, which is precisely what this refactor removes.
//!
//! `AgentEvent` is the other one, and it is the one that works: three
//! frontends already consume it over a channel, it serialises, and it carries
//! no cursor state. So the sink concept is kept and the event model converges
//! on `AgentEvent` rather than a third abstraction being invented.
//!
//! ## Why a sink cannot approve
//!
//! The previous trait had `fn confirm_action(...) -> Option<ConfirmOutcome>`
//! defaulting to `Some(ConfirmOutcome::Yes)` — documented as
//! "non-interactive = auto-approve". `NullDisplaySink` inherited it, and its
//! doc comment nominated it for serve/MCP mode. Wiring it there would have
//! auto-approved every destructive action on a headless surface, and nothing
//! in the type system would have objected.
//!
//! So approval is not a sink capability. A sink may *relay* a question to a
//! human and report the answer; it cannot answer. `None` means "no human was
//! reachable", and the loop treats that as refusal. The default is `None`, so
//! a sink that ignores confirmation entirely fails closed by construction.

use crate::tui::wire::AgentEvent;

/// What the loop needs a human to determine before it will proceed.
#[derive(Debug, Clone)]
pub struct ConfirmationRequest<'a> {
    /// Identifies this question. A response must carry it back, so a late or
    /// forged answer cannot be applied to a different question.
    pub id: crate::agent::approval::ConfirmationRequestId,
    /// The action awaiting approval.
    pub action: &'a serde_json::Value,
    /// Its wire name, already resolved.
    pub action_type: &'a str,
    /// Why approval is required, in a form fit to show a person.
    pub reason: &'a str,
    /// What a standing approval would authorize, in the user's terms — or
    /// `None` when this action supports no persistent grant at all.
    ///
    /// This is the frontend capability contract. `Some(text)` means the
    /// persistent option may be offered and MUST be labelled with `text`;
    /// `None` means only "allow once" may be shown. The previous prompt
    /// offered a bare `a` with no label, and answering it disabled
    /// confirmation globally for the session and every sub-agent.
    pub persistent_grant: Option<&'a str>,
    /// Set when this question is not "may this action run?" but "may your
    /// policy be widened so that it can?".
    ///
    /// SECURITY: its presence is the frontend's authority to relabel the three
    /// choices as *session grant / config grant / deny*. `None` — the case for
    /// every ordinary approval — means the frontend must show the ordinary
    /// labels, and in particular must never present an unliftable refusal as
    /// something with a "yes". The orchestrator has already decided, by the
    /// exhaustive match in the runtime, that this particular refusal is the
    /// operator's to lift; a frontend cannot reach that decision on its own
    /// and is not asked to.
    ///
    /// The three outcomes keep their meanings, mapped by
    /// [`crate::agent::confirm::decide_policy_lift`]: `Yes` → widen this
    /// runtime process, `Always` → widen it AND write the config file, `No` →
    /// refuse. There is deliberately no fourth answer meaning "and stop asking
    /// about everything": each command, path and action is its own question.
    pub lift: Option<&'a x0ne_actions::policy_grant::LiftableDenial>,
}

impl ConfirmationRequest<'_> {
    /// The label a frontend must put on its THIRD option — the one that
    /// outlives this single answer — or `None` when it must not offer one.
    ///
    /// One accessor rather than each frontend reading `persistent_grant` and
    /// `lift` for itself: there are four `request_confirmation` implementations
    /// in this workspace, and the 2026-08-15 audit found the fourth still raw
    /// after the first three were closed. A frontend that reads this cannot be
    /// the one that forgot the new case.
    ///
    /// SECURITY: `None` still means "do not offer it". For an ordinary
    /// approval that is an action with no stable identity to scope a standing
    /// grant to; for a policy question it never happens, because a liftable
    /// refusal always names a list.
    pub fn persistent_label(&self) -> Option<String> {
        match self.lift {
            Some(lift) => Some(lift.persist_choice()),
            None => self.persistent_grant.map(String::from),
        }
    }
}

/// A human's answer to a [`ConfirmationRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Run this one.
    Yes,
    /// Do not run it.
    No,
    /// Run this one and stop asking for the rest of the session.
    Always,
}

/// Renders agent activity and, optionally, relays questions to a human.
///
/// Implementors may render and collect input. They may not decide whether an
/// action runs, and they are given no means to dispatch one.
pub trait DisplaySink: Send {
    /// Render one event. Called for every event the loop emits, in order.
    fn emit(&mut self, event: &AgentEvent);

    /// Put a confirmation question to a human and return their answer.
    ///
    /// `None` means no human could be asked — a piped stdin, a headless
    /// server, a sub-agent. The loop refuses the action in that case, so the
    /// default here is `None` and silence is never consent.
    fn request_confirmation(
        &mut self,
        _request: &ConfirmationRequest<'_>,
    ) -> Option<crate::agent::approval::ConfirmationResponse> {
        None
    }
}

/// Discards everything. For servers, sub-agents and tests that only care about
/// execution outcomes.
///
/// Inherits `request_confirmation` → `None`, so an action needing approval is
/// refused rather than waved through.
#[derive(Default)]
pub struct NullDisplaySink;

impl DisplaySink for NullDisplaySink {
    fn emit(&mut self, _event: &AgentEvent) {}
}

/// Records every event for assertions. The parity tests use two of these
/// behind different frontends to prove the execution path is identical.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingDisplaySink {
    pub events: Vec<AgentEvent>,
    /// Answers handed out in order; exhausted means "no human left to ask".
    pub scripted_answers: std::collections::VecDeque<ConfirmOutcome>,
    pub confirmations_requested: Vec<String>,
}

#[cfg(test)]
impl DisplaySink for RecordingDisplaySink {
    fn emit(&mut self, event: &AgentEvent) {
        self.events.push(event.clone());
    }

    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<crate::agent::approval::ConfirmationResponse> {
        self.confirmations_requested
            .push(request.action_type.to_string());
        // Answers the question it was actually asked. A frontend that replied
        // with any other id would be rejected by the authority.
        self.scripted_answers.pop_front().map(|outcome| {
            crate::agent::approval::ConfirmationResponse {
                id: request.id,
                outcome,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant this trait exists to hold: a sink that does not implement
    /// confirmation cannot approve anything. The previous default returned
    /// `Some(Yes)`, so the safest possible sink was also the most permissive.
    #[test]
    fn a_sink_that_ignores_confirmation_cannot_approve() {
        let mut sink = NullDisplaySink;
        let action = serde_json::json!({"type": "MoveToTrash", "path": "/tmp/x"});
        let mut auth = crate::agent::approval::ApprovalAuthority::default();
        let answer = sink.request_confirmation(&ConfirmationRequest {
            id: auth.open_request(),
            action: &action,
            action_type: "MoveToTrash",
            reason: "destructive",
            persistent_grant: Some("Allow MoveToTrash on `/tmp/x` for the rest of this session"),
            lift: None,
        });
        assert_eq!(
            answer, None,
            "a sink with no human attached must not answer on their behalf"
        );
    }

    /// And a sink whose scripted answers run out must stop approving, rather
    /// than falling back to a default.
    #[test]
    fn an_exhausted_sink_stops_approving() {
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Yes].into_iter().collect(),
            ..Default::default()
        };
        let action = serde_json::json!({"type": "MoveToTrash", "path": "/tmp/x"});
        let mut auth = crate::agent::approval::ApprovalAuthority::default();
        let request = ConfirmationRequest {
            id: auth.open_request(),
            action: &action,
            action_type: "MoveToTrash",
            reason: "destructive",
            persistent_grant: Some("Allow MoveToTrash on `/tmp/x` for the rest of this session"),
            lift: None,
        };
        assert_eq!(
            sink.request_confirmation(&request).map(|r| r.outcome),
            Some(ConfirmOutcome::Yes)
        );
        assert_eq!(
            sink.request_confirmation(&request),
            None,
            "running out of answers is not consent"
        );
        assert_eq!(sink.confirmations_requested.len(), 2);
    }
}
