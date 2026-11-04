//! Wire types between the tui render layer and the agent runner.
//!
//! Two channels:
//!   * tui  →  runner   (`TuiCommand`)
//!   * runner  →  tui   (`AgentEvent`)
//!
//! The TUI never blocks on LLM/bridge work; the runner spawns its own
//! tokio task. Events drain into `AppState` every tick of the render
//! loop, so the screen reflects activity at ~12 fps minimum without
//! the runner ever touching ratatui.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One choice in an AskUser MCQ — the agent supplies these, the renderer
/// shows them, the user picks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AskOption {
    /// Short label shown in the list (max ~5 words).
    pub label: String,
    /// Optional longer description shown under the focused option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `(Recommended)` style flag — the renderer paints a subtle accent
    /// when true. The LLM uses this to bias the user toward the "right"
    /// answer without removing the freedom to pick another.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recommended: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Things the user (via keyboard) asks the runner to do.
#[derive(Debug, Clone, Serialize)]
pub enum TuiCommand {
    /// A new user message to feed into the next turn.
    UserInput(String),
    /// User asked to cancel the in-flight turn.
    Cancel,
    /// `/clear` — a new conversational trust context. The session clears the
    /// canonical transcript AND revokes every standing approval, per the
    /// locked owner decision; the frontend clears its display separately.
    ClearSession,
    /// `/plan` (`Some(true)`), `/build` (`Some(false)`), Shift+Tab (`None`,
    /// toggle). The session switches once the runtime holds the mode, and
    /// answers with [`AgentEvent::SessionMode`].
    SetPlanMode { plan: Option<bool> },
    /// Reopen an earlier session in place — `/resume <id>` in the TUI. `id`
    /// is a session id or `last`. The runner validates it, reloads the
    /// transcript into the agent (which revokes standing approvals first,
    /// like `/clear`), moves the journal, and answers with
    /// [`AgentEvent::SessionResumed`] or an [`AgentEvent::Error`].
    ResumeSession { id: String },
    /// Hot-swap the LLM (provider:model).
    SwitchModel { provider: String, model: String },
    /// Answer to a pending AskUser modal. `cancelled = true` means the
    /// user dismissed without picking (Esc); `selected` is the list of
    /// chosen option labels (length ≥ 1 unless cancelled, length ≥ 1
    /// even for single-select). `custom_text` carries free-form input
    /// when the user picked the "type something" affordance.
    AskAnswer {
        id: String,
        cancelled: bool,
        selected: Vec<String>,
        custom_text: Option<String>,
    },
    /// Answer to a pending [`AgentEvent::ConfirmationRequested`], correlated
    /// by `id`.
    ///
    /// There is deliberately no "no answer" variant: the engine treats the
    /// absence of this command as a refusal. A frontend that drops the
    /// question, a connection that closes, a user who walks away — all deny.
    /// Only an explicit `approved: true` lets the action run.
    ConfirmationAnswer {
        id: String,
        approved: bool,
        /// The user asked not to be prompted again this session.
        #[serde(default)]
        always: bool,
    },
    /// User message arriving from an external bridge (Telegram,
    /// Matrix, future Slack…). The runner treats it like a normal
    /// `UserInput` for the agent loop, but ALSO emits an
    /// `AgentEvent::ExternalUserInput` so the TUI and other bridges
    /// can show what the user typed from the phone. `source` is the
    /// bridge name; the originating bridge filters out its own echo.
    ExternalUserInput { source: String, text: String },
    /// User message with attached images (multimodal). The runner
    /// loads each path, base64-encodes, and pushes a `Message` with
    /// the `images` field populated — the vision plumbing in
    /// `crate::api` then routes it to whichever backend supports
    /// it (Anthropic / Ollama). Subprocess backends without native
    /// vision will see just the text + a `[image: <path>]` marker.
    UserInputWithImages {
        text: String,
        image_paths: Vec<String>,
    },
    /// Turn the X0NE badge into a live widget: run `cmd` via `bash -c`
    /// every `interval_secs` and paint its output into the 28×3
    /// canvas. The command can be any bash (if/while/pipes/funcs all
    /// work). One-shot setup — the runner's ticker re-runs forever,
    /// so the prompt isn't re-emitted each tick. `label` heads the
    /// badge; empty = no header row.
    BadgeWatch {
        cmd: String,
        interval_secs: u64,
        label: String,
    },
    /// Stop the live badge watcher and let the static logo / LLM
    /// badge take over again.
    BadgeWatchClear,
    /// `/undo`: restore the files of the agent's last write from the
    /// snapshot taken before it. Answered by the runner, which holds the
    /// session's undo journal and its bridge; never a prompt to the model.
    Undo,
    /// Internal: the cron ticker fires this when a recurring
    /// scheduled action is due. The runner dispatches `action`
    /// through the bridge (it owns the single bridge handle) and
    /// surfaces the result in the trail / pulse. Not emitted by
    /// the user or LLM directly — they set it up via `CronSchedule`
    /// with an `action` payload.
    RunScheduledAction {
        label: String,
        action: serde_json::Value,
    },
    /// The TUI is about to exit; runner should flush and drop cleanly.
    Shutdown,
}

/// Things the runner emits as work progresses. The render side reads
/// these from the receiver each tick and mutates `AppState`.
#[derive(Debug, Clone, Serialize)]
pub enum AgentEvent {
    /// Runner has booted (LLM backend + bridge connected). Carries the
    /// resolved labels for the header.
    Ready {
        model_label: String,
        sandbox_label: String,
    },
    /// Runner failed to boot. Fatal — the user sees an error line and
    /// the TUI continues as a viewer (no further turns possible).
    BootFailed(String),
    /// The session's mode after a `/plan`, `/build` or Shift+Tab.
    SessionMode { plan: bool },

    /// A new assistant turn is starting (model started replying).
    AssistantStart,
    /// Streaming token from the model. Appended to the current
    /// assistant line; the renderer batches them per-tick.
    Token(String),
    /// The assistant turn is finished. Carries cumulative tokens + cost
    /// after the turn so the header can update.
    AssistantEnd { total_tokens: u64, cost_usd: f64 },

    /// A tool call started (typed action dispatched to the bridge).
    /// `action` carries the full input JSON so the TUI can show a
    /// detailed in-flight view (Ctrl+E modal).
    ToolStart {
        name: String,
        target: Option<String>,
        #[serde(default)]
        action: Value,
    },
    /// A tool call completed. The full action + result JSON travel
    /// with the event so the renderer can use the same `display.rs`
    /// helpers the rustyline CLI uses — same boxes, same previews,
    /// same colors. The `summary` field is the fallback one-liner
    /// shown when JSON rendering isn't applicable (e.g. bridge errors
    /// where we have no result).
    ToolEnd {
        name: String,
        ok: bool,
        summary: String,
        action: Value,
        result: Value,
        duration_ms: u64,
    },

    /// A snapshot was just created (auto, via Try/SafeChange, or manual).
    Snapshot { name: String, paths: Vec<String> },

    /// The active goal changed (set / step / cleared).
    GoalUpdate {
        label: Option<String>,
        step: Option<(u32, u32)>,
    },

    /// Header counters changed mid-turn (token budget consumed without
    /// finishing a turn). Used to keep the header live during long
    /// streams.
    HeaderTick { total_tokens: u64, cost_usd: f64 },

    /// The engine needs a human decision before an action may run.
    ///
    /// The frontend must reply with [`TuiCommand::ConfirmationAnswer`]
    /// carrying the same `id`. Silence is a denial — see that variant.
    ConfirmationRequested {
        id: String,
        action: Value,
        action_type: String,
        /// Why approval is required, in a form fit to show a person.
        reason: String,
        /// The authority's own description of what a session-wide grant would
        /// cover, when it may be offered at all. `None` means the frontend
        /// must not show an "always" option — the action has no stable
        /// identity to scope a standing approval to. Mirrors
        /// `ConfirmationRequest::persistent_grant` on the sink contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persistent_grant: Option<String>,
    },

    /// An action was refused before running. Distinct from a failed action:
    /// nothing was attempted, so there is no result to show.
    ActionDenied {
        name: String,
        action: Value,
        /// Why, in the engine's terms — a human declined, or none was
        /// reachable. The frontend words it; it does not decide it.
        reason: String,
    },

    /// A step inside a composite began. Lets a frontend show progress through
    /// a Pipeline instead of one opaque pause.
    PipelineStepStarted {
        index: usize,
        total: usize,
        step_type: String,
    },

    /// An action returned a handle for asynchronous work, so the frontend can
    /// surface it as something still running rather than something finished.
    HandleTracked {
        handle_id: String,
        action_type: String,
    },

    /// The turn's budget moved. Presentation only — a frontend may display
    /// these numbers but never enforce its own limits from them.
    ///
    /// Carries no token count, deliberately. It used to, and the only
    /// emitter had no figure to put there, so it sent a literal `0`
    /// (`driver.rs`, before 2026-08-13). Since this fires on every action
    /// and `AssistantEnd` fires once per turn, the zero won: the navbar read
    /// `0 tk · $0.08` — a cost with no tokens, which is impossible and was
    /// on screen for weeks. A counter needs exactly one writer.
    BudgetUpdated {
        cost_usd: f64,
        /// Session ceiling, or `None` when unlimited.
        limit_usd: Option<f64>,
    },

    /// A free-form info line (non-fatal warning, status hint). Rendered
    /// as a `System` line in the conversation.
    Info(String),

    /// A free-form error line. Rendered red.
    Error(String),

    /// The LLM emitted a `<matrix>{...}</matrix>` block — repaint the
    /// X0NE badge slot with the new frame. Carries the raw JSON payload
    /// so the renderer can parse + drop bad frames silently. See
    /// [`crate::tui::badge`] for the format spec.
    BadgeUpdate { frame_json: String },

    /// LLM (or user) added a task to the live TODO surface. The TUI
    /// owns the storage; the runner just notifies. `id` is provided
    /// by the runner so the matching `TaskDone` can target it.
    TaskAdded { id: u32, title: String },
    /// LLM (or user) marked a task done. Idempotent.
    TaskCompleted { id: u32 },
    /// LLM (or user) wiped the whole task list (e.g. session reset).
    TasksCleared,

    /// LLM (or user) scheduled a cron-light reminder / self-trigger.
    /// `due_at_unix` is absolute wall-clock seconds; `trigger_prompt`
    /// nonempty means the runner will inject it back as UserInput
    /// when due, otherwise it's a pure reminder.
    CronScheduled {
        id: u32,
        label: String,
        due_at_unix: u64,
        trigger_prompt: Option<String>,
    },
    /// A cron fired (due time reached); TUI marks it as fired in the
    /// panel. Runner has already done whatever the fire effect was.
    CronFired { id: u32 },
    /// User or LLM cancelled a cron before it fired.
    CronCancelled { id: u32 },

    /// A user message arrived from an external bridge (Telegram /
    /// Matrix). Surfaced so the TUI can echo what the user typed
    /// from the phone — otherwise the local screen would only see
    /// X0NE's replies and the conversation would feel one-sided.
    ExternalUserInput { source: String, text: String },

    /// An earlier session was reopened by [`TuiCommand::ResumeSession`]. The
    /// renderer replaces its transcript with `messages` (role, text — text
    /// already sanitized by the runner) and adopts `id`. `turns_reloaded` is
    /// what the operator is told; `bytes_skipped > 0` means the oldest turns
    /// were beyond the resume window and are missing.
    SessionResumed {
        id: String,
        turns_reloaded: usize,
        bytes_skipped: u64,
        messages: Vec<(String, String)>,
    },

    /// The LLM emitted an `AskUser` action. The renderer must install a
    /// modal MCQ (Claude-Code style) and wait for keyboard input. When
    /// the user submits (Enter) or cancels (Esc), the TUI sends a
    /// matching [`TuiCommand::AskAnswer`] back with the same `id`. The
    /// runner is blocked on a oneshot for that id; without an answer
    /// the turn deadlocks, so the renderer MUST always reply (Esc
    /// counts as a cancellation answer — not a no-op).
    AskUser {
        id: String,
        question: String,
        options: Vec<AskOption>,
        multi_select: bool,
        /// Optional free-form helper text rendered under the question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_serde() {
        // Sanity: the AgentEvent variants we use most must serialize.
        // We don't need it for runtime, but it's a free guard against
        // accidentally embedding non-serializable types (e.g. an
        // Arc<dyn …>) which would block future debug logging.
        for ev in [
            AgentEvent::Ready {
                model_label: "openai-compat/qwen3-8b @ http://127.0.0.1:18599".into(),
                sandbox_label: "incus".into(),
            },
            AgentEvent::AssistantStart,
            AgentEvent::Token("hello".into()),
            AgentEvent::AssistantEnd {
                total_tokens: 1234,
                cost_usd: 0.05,
            },
            AgentEvent::ToolStart {
                name: "ReadFile".into(),
                target: Some("/tmp/x".into()),
                action: serde_json::json!({"type": "ReadFile", "path": "/tmp/x"}),
            },
            AgentEvent::ToolEnd {
                name: "ReadFile".into(),
                ok: true,
                summary: "8 KB".into(),
                action: serde_json::json!({"type": "ReadFile"}),
                result: serde_json::json!({"success": true}),
                duration_ms: 3,
            },
            AgentEvent::Snapshot {
                name: "pre-refactor".into(),
                paths: vec!["src/".into()],
            },
            AgentEvent::GoalUpdate {
                label: Some("ship v0.3".into()),
                step: Some((3, 7)),
            },
            AgentEvent::HeaderTick {
                total_tokens: 500,
                cost_usd: 0.01,
            },
            AgentEvent::Info("connected".into()),
            AgentEvent::Error("timeout".into()),
            AgentEvent::BootFailed("no API key".into()),
            AgentEvent::BadgeUpdate {
                frame_json: r#"{"rows":["X0NE"]}"#.into(),
            },
        ] {
            let s = serde_json::to_string(&ev).expect("must serialize");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn commands_round_trip_through_serde() {
        for c in [
            TuiCommand::UserInput("refactor auth".into()),
            TuiCommand::Cancel,
            TuiCommand::Undo,
            TuiCommand::SwitchModel {
                provider: "anthropic".into(),
                model: "claude-haiku-4".into(),
            },
            TuiCommand::AskAnswer {
                id: "ask-1".into(),
                cancelled: false,
                selected: vec!["yes".into()],
                custom_text: None,
            },
            TuiCommand::Shutdown,
        ] {
            let s = serde_json::to_string(&c).expect("must serialize");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn ask_user_event_serializes() {
        let ev = AgentEvent::AskUser {
            id: "ask-7".into(),
            question: "Which approach?".into(),
            options: vec![
                AskOption {
                    label: "Refactor".into(),
                    description: Some("safer".into()),
                    recommended: true,
                },
                AskOption {
                    label: "Rewrite".into(),
                    description: None,
                    recommended: false,
                },
            ],
            multi_select: false,
            hint: None,
        };
        let s = serde_json::to_string(&ev).expect("must serialize");
        assert!(s.contains("ask-7"));
        assert!(s.contains("Refactor"));
    }

    /// The correlation contract, pinned: an answer carries the id of the
    /// question it answers, and there is no way to express "no answer".
    /// Absence is a refusal, and the type system offers no alternative.
    #[test]
    fn a_confirmation_answer_is_correlated_and_has_no_silent_variant() {
        let cmd = TuiCommand::ConfirmationAnswer {
            id: "q1".into(),
            approved: true,
            always: false,
        };
        match cmd {
            TuiCommand::ConfirmationAnswer { id, approved, .. } => {
                assert_eq!(id, "q1");
                assert!(approved);
            }
            other => panic!("expected a confirmation answer, got {other:?}"),
        }
    }

    /// The new events must survive the channel and the SSE wire like the
    /// rest — they are the execution → presentation boundary.
    #[test]
    fn the_semantic_events_serialize() {
        for ev in [
            AgentEvent::ConfirmationRequested {
                id: "q1".into(),
                action: serde_json::json!({"type": "MoveToTrash"}),
                action_type: "MoveToTrash".into(),
                reason: "destructive".into(),
                persistent_grant: Some("MoveToTrash on /tmp/x".into()),
            },
            AgentEvent::ActionDenied {
                name: "MoveToTrash".into(),
                action: serde_json::json!({"type": "MoveToTrash"}),
                reason: "no human channel".into(),
            },
            AgentEvent::PipelineStepStarted {
                index: 0,
                total: 3,
                step_type: "ReadFile".into(),
            },
            AgentEvent::HandleTracked {
                handle_id: "h1".into(),
                action_type: "RunCommand".into(),
            },
            AgentEvent::BudgetUpdated {
                cost_usd: 0.01,
                limit_usd: Some(2.0),
            },
        ] {
            let json = serde_json::to_string(&ev).expect("event must serialize");
            assert!(json.len() > 10, "empty encoding for {ev:?}");
        }
    }

    /// Nothing in the event model may carry terminal mechanics. That was the
    /// disqualifying flaw of the abstraction this one replaced.
    #[test]
    fn no_event_carries_cursor_or_layout_state() {
        // The definitions only — this test names the banned tokens, so
        // scanning its own module would always trip.
        let full = include_str!("wire.rs");
        let src = &full[..full.find("#[cfg(test)]").unwrap_or(full.len())];
        for banned in [
            "erase_lines",
            "EraseStreamedTokens",
            "rendered_lines",
            "PaintAtColumn",
            "line_count",
        ] {
            assert!(
                !src.contains(banned),
                "{banned} is presentation state and must live in the frontend"
            );
        }
    }
}
