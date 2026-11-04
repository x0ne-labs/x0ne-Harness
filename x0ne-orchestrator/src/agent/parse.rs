//! Model output → one of three things the engine can act on.
//!
//! Every loop already called `parser::parse_llm_response`, except Matrix,
//! which had its own. The divergence that mattered was not the parsing but
//! what happened afterwards: `matrix_mode.rs` did
//! `if let Ok(action) = serde_json::from_str(...)` with **no else**, so a
//! malformed action simply vanished. The model was told nothing, the operator
//! saw a no-op, and a model stuck on a bad format had no way to discover it.
//!
//! So this module's contract is not "parse" — it is that **every outcome is
//! actionable**. Either there is an action to dispatch, or a message for the
//! user, or corrective feedback for the model. There is no fourth case and no
//! silent one.

use crate::conversation::SystemFeedbackClass;
use crate::parser::{self, LlmOutput, WrapperKind};
use serde_json::Value;

/// What the model's turn produced.
#[derive(Debug, Clone)]
pub enum TurnInput {
    /// An action to route through the dispatcher.
    Action {
        reasoning: String,
        action: Value,
        wrapper: WrapperKind,
        unwrapped_field: Option<String>,
    },
    /// Prose for the user. The turn ends here.
    Message { text: String },
    /// The output could not be used. `feedback` goes back to the model so it
    /// can correct itself on the next iteration.
    Correction(CorrectiveFeedback),
}

/// What the model is told when its output could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectiveFeedback {
    /// Drives the conversation's feedback framing and the caller's counters.
    pub class: SystemFeedbackClass,
    /// Addressed to the model: what was wrong and what to do instead.
    pub message: String,
    /// The offending output, kept for the session log.
    pub raw: String,
}

impl CorrectiveFeedback {
    /// True when the failure is the model's formatting rather than its
    /// judgement — the caller charges these to the parse-retry budget.
    pub fn is_parse_failure(&self) -> bool {
        self.class == SystemFeedbackClass::ParseError
    }
}

/// Interpret one model response.
///
/// Uses the shared parser, so the fix that stops a prose or code-span mention
/// of `<action>` being read as an incomplete action applies everywhere at once
/// rather than in whichever loop remembered it.
pub fn interpret(raw_text: &str) -> TurnInput {
    // SECURITY: the guarded entry point, not `parse_llm_response` directly.
    // This is the single choke point every live loop goes through, so the
    // "a model response can never end the process" guarantee lives here
    // rather than in whichever loop remembered it — the same reason the
    // parsing itself was hoisted into this module.
    match parser::parse_llm_response_guarded(raw_text) {
        LlmOutput::Action {
            reasoning,
            action,
            wrapper,
            unwrapped_field,
        } => TurnInput::Action {
            reasoning,
            action,
            wrapper,
            unwrapped_field,
        },
        LlmOutput::Message { text } => TurnInput::Message { text },
        LlmOutput::ParseError { raw_text, error } => TurnInput::Correction(CorrectiveFeedback {
            class: SystemFeedbackClass::ParseError,
            message: format!(
                "{error}\nEmit exactly one action as \
                     `<action>{{\"type\": \"...\", ...}}</action>`, with the JSON on \
                     its own between the tags. If you meant to talk about the format \
                     rather than use it, write normally without the tags."
            ),
            raw: raw_text,
        }),
        LlmOutput::Hallucination { raw_text } => TurnInput::Correction(CorrectiveFeedback {
            class: SystemFeedbackClass::Hallucination,
            message: "That response contains a fabricated action result. Nothing ran. \
                      Emit a real <action> block and wait for the actual result before \
                      describing any outcome."
                .to_string(),
            raw: raw_text,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_action_is_returned_for_dispatch() {
        match interpret("Reading it.\n<action>{\"type\": \"ListSnapshots\"}</action>") {
            TurnInput::Action { action, .. } => assert_eq!(action["type"], "ListSnapshots"),
            other => panic!("expected an action, got {other:?}"),
        }
    }

    #[test]
    fn prose_ends_the_turn() {
        match interpret("Done — the file now has 12 lines.") {
            TurnInput::Message { text } => assert!(text.contains("12 lines")),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// The Matrix defect, stated as an invariant: unusable output must never
    /// vanish. Whatever the model emitted, it gets told something it can act
    /// on.
    #[test]
    fn unusable_output_always_produces_corrective_feedback() {
        for bad in [
            "<action>{\"type\": \"ReadFile\", \"path\": \"/tm",
            "<action>not json at all",
        ] {
            match interpret(bad) {
                TurnInput::Correction(f) => {
                    assert!(
                        f.message.len() > 30,
                        "feedback must tell the model what to do: {}",
                        f.message
                    );
                    assert_eq!(f.raw, bad, "the offending output must be kept");
                }
                other => panic!("{bad:?} must produce feedback, got {other:?}"),
            }
        }
    }

    /// An unknown action name is *not* caught here. The parser deserialises
    /// into `Value`, so it validates JSON syntax and nothing more — and it has
    /// to, because a user-defined composite primitive is a legitimate name
    /// this layer cannot know. Identity is checked where the vocabulary and
    /// the composite store both exist: at dispatch. Pinned so nobody "fixes"
    /// this by adding a check that would reject custom primitives.
    #[test]
    fn an_unknown_name_passes_parsing_and_is_caught_at_dispatch() {
        match interpret("<action>{\"type\": \"ReadFiles2\"}</action>") {
            TurnInput::Action { action, .. } => assert_eq!(action["type"], "ReadFiles2"),
            other => panic!("parsing must not adjudicate identity, got {other:?}"),
        }
    }

    /// The live-session regression, now guarded at the engine boundary rather
    /// than only in the parser: talking about the format is not using it.
    #[test]
    fn discussing_the_action_tag_is_a_message_not_a_failure() {
        let text = "I was asked to respond only via a custom `<action>` tag.";
        match interpret(text) {
            TurnInput::Message { .. } => {}
            other => panic!("prose about the tag must not be a failure, got {other:?}"),
        }
    }

    /// A fabricated result with no real action is a different failure from a
    /// malformed one: the model's formatting was fine, its honesty was not.
    /// The classes must not be collapsed — the caller charges them to
    /// different budgets.
    #[test]
    fn a_fabricated_result_is_classed_apart_from_a_parse_failure() {
        let fake = "[Action executed]\n{\"success\": true, \"content\": \"invented\"}";
        match interpret(fake) {
            TurnInput::Correction(f) => {
                assert_eq!(f.class, SystemFeedbackClass::Hallucination);
                assert!(!f.is_parse_failure(), "must not consume the parse budget");
            }
            other => panic!("expected a hallucination correction, got {other:?}"),
        }
    }
}
