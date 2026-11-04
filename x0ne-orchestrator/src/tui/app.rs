//! tui application state.
//!
//! All the data the renderer needs lives here. Mutated by:
//!   * keyboard events (events.rs)
//!   * the agent loop, via channels (stage 2)
//!   * background tickers (queue badge refresh, body strip update)
//!
//! Render functions never mutate; they consume `&AppState`.

use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use super::badge::{self, BadgePlayback};
use super::wire::{AgentEvent, AskOption, TuiCommand};
use crate::input_queue;

/// Active AskUser MCQ — installed by [`AgentEvent::AskUser`], driven by
/// the keyboard, drained by a matching [`TuiCommand::AskAnswer`].
///
/// While this is `Some(_)`, the renderer overlays a modal above the
/// input bar and the event handler routes keys through `events::handle_ask_key`
/// instead of the normal textarea path. The user cannot escape into
/// free-form typing until they either pick + submit or hit Esc.
#[derive(Debug, Clone)]
pub struct AskState {
    /// Unique id (matches the runner's pending oneshot map). Empty for a
    /// picker the TUI opened for itself — see `sink`.
    pub id: String,
    /// Who consumes the answer: the model (through the runner) or the TUI
    /// itself. One modal, one keyboard map, two consumers — the operator
    /// cannot tell a `/sessions` list from an `AskMultipleChoice` and does
    /// not need to.
    pub sink: AskSink,
    /// What the operator has typed to narrow the list. Letters go here (not
    /// to j/k navigation); rows whose label or description do not contain it
    /// (case-insensitively) are hidden, and the cursor moves over the
    /// visible rows only.
    pub filter: String,
    /// The question rendered as the header.
    pub question: String,
    /// Optional dim helper line shown under the question.
    pub hint: Option<String>,
    /// All options + their current selected/highlighted state.
    pub options: Vec<AskRow>,
    /// True for checkbox-style multi-pick (Space toggles, Enter submits);
    /// false for radio-style single-pick (Enter on the focused row picks
    /// and submits in one go).
    pub multi_select: bool,
    /// Index in `options` currently under the keyboard caret (0-based).
    pub cursor: usize,
}

/// Who gets the answer of an [`AskState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskSink {
    /// The model asked (`AskMultipleChoice` / `AskUser`): the answer goes back
    /// through [`TuiCommand::AskAnswer`] with the ask's `id`.
    Model,
    /// The TUI opened the picker for its own purpose; the chosen label is
    /// handed to `events::apply_picker_choice` for this action.
    Local(PickerAction),
}

/// What the TUI does with a locally-picked row. Exhaustive `match` in
/// `events::apply_picker_choice`: a new use of the picker must decide what a
/// choice does — the compiler is the mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    /// The label is a session id — `TuiCommand::ResumeSession`.
    ResumeSession,
    /// The label is `provider/model` — `TuiCommand::SwitchModel`.
    SwitchModel,
}

impl AskState {
    /// Indices (into `options`) of the rows the filter lets through, in order.
    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.options.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.option.label.to_lowercase().contains(&needle)
                    || r.option
                        .description
                        .as_deref()
                        .map(|d| d.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// A picker the TUI opens for itself: `rows` as (label, description).
    pub fn local(
        question: &str,
        hint: Option<&str>,
        rows: Vec<(String, Option<String>)>,
        action: PickerAction,
    ) -> Self {
        Self {
            id: String::new(),
            sink: AskSink::Local(action),
            filter: String::new(),
            question: question.to_string(),
            hint: hint.map(str::to_string),
            options: rows
                .into_iter()
                .map(|(label, description)| AskRow {
                    option: crate::tui::wire::AskOption {
                        label,
                        description,
                        recommended: false,
                    },
                    selected: false,
                })
                .collect(),
            multi_select: false,
            cursor: 0,
        }
    }
}

/// Which pane the keyboard is currently addressing.
///
/// Follows the keys rather than a separate focus-switching mode: `Tab` /
/// `Shift+Tab` address the side panel, the scroll keys address the
/// conversation, and whichever was addressed last carries the bright border.
/// Frontend UI state only — never part of `AgentState`, never consulted by
/// execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusRegion {
    #[default]
    Conversation,
    SidePanel,
}

/// One frontend notification hook: a semantic event pattern wired to a
/// desktop notification. OFF by default; only a human toggle arms it. This
/// is presentation, deliberately: a hook here cannot execute an action —
/// anything with effects belongs to the typed Action boundary, not to a
/// notification preference.
#[derive(Debug, Clone)]
pub struct HookRow {
    pub id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
}

impl HookRow {
    pub fn defaults() -> Vec<HookRow> {
        vec![
            HookRow {
                id: "notify-turn-end",
                label: "notify when a turn finishes",
                enabled: false,
            },
            HookRow {
                id: "notify-approval",
                label: "notify when an approval is needed",
                enabled: false,
            },
            HookRow {
                id: "notify-error",
                label: "notify on errors",
                enabled: false,
            },
        ]
    }
}

/// Fire a desktop notification, best effort. `notify-send` missing or
/// failing is silent — a notification preference must never break the UI.
fn desktop_notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .arg("-a")
        .arg("X0NE")
        .arg(summary)
        .arg(body)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Active confirmation question — installed by
/// [`AgentEvent::ConfirmationRequested`], answered by
/// [`TuiCommand::ConfirmationAnswer`].
///
/// The security modal, as opposed to [`AskState`], the model's question
/// modal. Kept as separate types on purpose: answering a model's question
/// grants nothing, answering this one does, and the code paths must not be
/// confusable. While this is `Some(_)`, keys route through
/// `events::handle_confirm_key` and the turn is parked on the answer.
#[derive(Debug, Clone)]
pub struct ConfirmState {
    /// Correlates with the engine's pending question. An answer with any
    /// other id is dropped as stale.
    pub id: String,
    pub action_type: String,
    /// The full action JSON, so the person can read what they are approving.
    pub action: serde_json::Value,
    /// Why approval is required, in the engine's words.
    pub reason: String,
    /// The authority's description of what a standing grant would cover, or
    /// `None` when the session-wide option must not be offered at all.
    pub persistent_grant: Option<String>,
    /// Index of the focused choice.
    pub cursor: usize,
}

impl ConfirmState {
    /// The choices, in display order. The standing option appears only when
    /// the authority offered it, labelled with the authority's own scope —
    /// the same contract the CLI prompt follows.
    pub fn choices(&self) -> Vec<(u8, String)> {
        let mut out = vec![(1u8, "allow once".to_string())];
        if let Some(scope) = &self.persistent_grant {
            // The authority's own sentence (« Allow RunCommand on `…` for the
            // rest of this session »), first letter lowered to sit with the
            // two other choices. It used to be wrapped in a second « allow …
            // for this session » — the operator read « allow Allow RunCommand
            // on `bash …` for the rest of this session for this session »
            // (screenshot 2026-08-17 14:43).
            out.push((2, lower_first(scope)));
        }
        out.push((3, "deny".to_string()));
        out
    }
}

/// « Allow X » → « allow X »; anything else unchanged.
pub(crate) fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// One renderable row in the AskUser modal — wraps a wire `AskOption`
/// with the runtime-mutable `selected` bit.
#[derive(Debug, Clone)]
pub struct AskRow {
    pub option: AskOption,
    pub selected: bool,
}

/// One LLM- or user-created task in the visible task list. Cheap
/// in-memory state — survives a Tab switch but not a TUI restart.
/// Persistence lives in the GoalManager; this is the *active* TODO
/// surface for the current session.
#[derive(Debug, Clone)]
pub struct TaskItem {
    pub id: u32,
    pub title: String,
    pub done: bool,
    pub created_at: Instant,
}

/// What the Goal panel shows — and nothing more.
///
/// SECURITY: **presentation only.** The frontend fills it from
/// `GoalManager::get_active`, that is from what is actually persisted in
/// `~/.x0ne/goals/`. No model action writes it, and none must ever write
/// it: a field the model fills is a channel of influence over what the
/// operator READS, hence over what they approve. The goal itself has the
/// same rule upstream (spec O-2: no action can create or modify a goal).
///
/// Everything is already formatted here: the renderer knows neither
/// `GoalStatus` nor `HistoryEntry`, it only has text to place in a frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GoalSummary {
    pub text: String,
    pub status: String,
    pub steps_used: u32,
    pub steps_limit: Option<u32>,
    /// The last history entries, one line each, most recent last. Bounded
    /// to [`GOAL_SUMMARY_HISTORY`]: a goal's history grows every turn, the
    /// panel does not.
    pub history: Vec<String>,
    /// How many entries the goal ACTUALLY carries.
    ///
    /// What is cut says it is cut — the panel shows `history N of M`.
    /// Without this count, a two-hundred-step goal would read as a
    /// twelve-step goal, and « nothing happened » would become a possible
    /// reading of a screen that shows all it has.
    pub history_total: usize,
}

/// How many history entries the panel shows. Fixed, and small: the panel
/// is a glance, not a log — `/goal status` returns the rest.
pub const GOAL_SUMMARY_HISTORY: usize = 12;

/// Length of a history line copied into the panel. Notes carry text
/// written by the model (`turn ended: …`, the gaps of a skeptic pass):
/// nobody controls their length.
pub const GOAL_SUMMARY_NOTE_CHARS: usize = 160;

impl GoalSummary {
    /// Format the persisted goal for the screen.
    ///
    /// The only allowed direction: `GoalState` → `GoalSummary`. Nothing
    /// flows back, the panel decides nothing.
    pub fn of(goal: &crate::goal::GoalState) -> Self {
        // Exhaustive, with no `_` arm: a new status must name itself on
        // screen rather than inherit a label that lies (invariant 3).
        let status = match &goal.status {
            crate::goal::GoalStatus::Active => "active".to_string(),
            crate::goal::GoalStatus::Paused => "paused".to_string(),
            crate::goal::GoalStatus::Achieved => "achieved".to_string(),
            crate::goal::GoalStatus::Blocked { reason } => {
                format!("blocked · {}", clip(reason, GOAL_SUMMARY_NOTE_CHARS))
            }
            crate::goal::GoalStatus::BudgetExhausted => "budget exhausted".to_string(),
            crate::goal::GoalStatus::Cleared => "cleared".to_string(),
        };
        let history = goal
            .history
            .iter()
            .rev()
            .take(GOAL_SUMMARY_HISTORY)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|entry| {
                let kind = format!("{:?}", entry.kind).to_lowercase();
                match entry
                    .note
                    .as_deref()
                    .or(entry.achievement_outcome.as_deref())
                {
                    Some(note) => {
                        format!("{kind} · {}", clip(note, GOAL_SUMMARY_NOTE_CHARS))
                    }
                    None => kind,
                }
            })
            .collect();
        Self {
            text: clip(&goal.text, GOAL_SUMMARY_NOTE_CHARS * 2),
            status,
            // A counter that overflows `u32` reads « very large », it does
            // not restart from zero.
            steps_used: u32::try_from(goal.budget.steps_used).unwrap_or(u32::MAX),
            steps_limit: goal
                .budget
                .max_steps
                .map(|m| u32::try_from(m).unwrap_or(u32::MAX)),
            history,
            history_total: goal.history.len(),
        }
    }
}

/// Cut on characters, not bytes — a note can contain UTF-8, and a cut in
/// the middle of a code point panics.
fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

/// One scheduled action / reminder. Cron-light: the runner ticks
/// once per second and fires due items as Info events the agent can
/// react to. Two flavours: a free-text reminder ("ping me about X")
/// or a self-trigger to feed the agent a new prompt automatically.
#[derive(Debug, Clone)]
pub struct CronItem {
    pub id: u32,
    pub label: String,
    /// Unix seconds at which the cron should fire. Absolute time so
    /// the renderer can show the wall clock; the runner compares to
    /// `now()` each tick.
    pub due_at_unix: u64,
    /// When `Some(text)`, the cron fires `TuiCommand::UserInput(text)`
    /// at due time (self-trigger). When `None`, it just emits a
    /// `Info` so the user sees a reminder without the agent running.
    pub trigger_prompt: Option<String>,
    pub fired: bool,
}

/// Most-recent Pipeline run, broken down step by step. Populated on
/// every `ToolEnd { name: "Pipeline" }` from the result envelope —
/// the runtime ships per-step status/duration/preview in
/// `result.output.steps[]` so the panel doesn't need to call back.
/// `None` until the agent runs its first Pipeline this session.
#[derive(Debug, Clone)]
pub struct PipelineRun {
    pub started_at: Instant,
    pub completed: bool,
    pub total_steps: usize,
    pub completed_steps: usize,
    pub failed: bool,
    pub total_duration_ms: u64,
    pub steps: Vec<PipelineStep>,
}

#[derive(Debug, Clone)]
pub struct PipelineStep {
    pub idx: usize,
    pub action_type: String,
    /// `ok`, `failed`, or `skipped`. We accept whatever string the
    /// runtime gives us, mapped to a glyph at render time.
    pub status: String,
    pub duration_ms: u64,
    /// One-line summary derived from the step's result envelope
    /// (e.g. `1.6KB`, `sha256 aef…`, `err: ...`). Already truncated.
    pub summary: String,
}

/// One executed shell command in the shell-mode trail. The bytes
/// captured here are also surfaced to the LLM as context on the
/// next agent-mode turn so the agent "sees what you just did".
#[derive(Debug, Clone)]
pub struct ShellEntry {
    /// Exact command line the user typed.
    pub cmd: String,
    /// Working directory at the time of execution.
    pub cwd: std::path::PathBuf,
    /// Combined stdout. Empty when the command produced no stdout.
    pub stdout: String,
    /// Combined stderr.
    pub stderr: String,
    /// Process exit code; -1 if the spawn failed.
    pub exit_code: i32,
    /// Duration in ms — to surface slow commands visually.
    pub duration_ms: u64,
    pub at: Instant,
    /// When `true`, the renderer collapses the entry to a single
    /// summary line (`$ cmd · 30 lines · exit=0 · 12ms`). New
    /// entries land expanded; pushing a fresh entry auto-collapses
    /// all previous ones, keeping the screen calm. Ctrl+E in shell
    /// mode toggles ALL entries (same idiom as agent action expand).
    pub collapsed: bool,
}

/// One URL the agent touched this session — populated automatically
/// from `FetchUrl`, `BrowseWeb`, and `WebSearch` action results. The
/// Sources panel renders them in arrival order with title + status,
/// and the underlying jsonl already captures every detail (request
/// + result envelope) for full audit.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub url: String,
    /// Title where available (page `<title>`, search-result title);
    /// falls back to the URL when nothing better is known.
    pub title: String,
    /// Origin action — `FetchUrl` / `BrowseWeb` / `WebSearch`.
    pub origin: String,
    pub ok: bool,
    pub at: Instant,
    /// Search query when this entry came from a WebSearch result —
    /// lets the panel group hits by query.
    pub query: Option<String>,
    /// Runtime citation id (`S<n>`), shared by every action that saw the URL.
    pub cite: Option<String>,
}

/// One entry in the live session timeline (Pulse panel). Auto-derived
/// from `AgentEvent`s — the renderer doesn't compute, it just paints.
#[derive(Debug, Clone)]
pub struct PulseEvent {
    pub at: Instant,
    pub kind: PulseKind,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseKind {
    Turn,      // assistant turn boundary
    Tool,      // tool succeeded
    ToolError, // tool failed
    Snapshot,
    Ask,  // user was asked a question
    Goal, // goal set or stepped
    Task, // task added or completed
    Cron, // cron fired
}

/// Visible side panel. Cycled with Tab / Shift-Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidePanel {
    /// Default — the conversation history with the action trail.
    Conversation,
    /// Messages the user typed while the agent was busy.
    Queue,
    /// Restore points the agent (or the user) created.
    Snapshots,
    /// Async handles (streaming processes, watchers, subscribes).
    Handles,
    /// Parallel agents (Spawn, Worlds).
    Worlds,
    /// Active goal + step history.
    Goal,
    /// Live TODO list driven by the agent (TaskAdd / TaskDone meta-actions)
    /// or the user (`/task add`).
    Tasks,
    /// Scheduled reminders / self-triggers (CronSchedule meta-action).
    Cron,
    /// Live session timeline — heart-rate monitor for the agent.
    /// Auto-fills from every AgentEvent — zero new APIs.
    Pulse,
    /// Most-recent Pipeline action with per-step breakdown — like
    /// tui1's pipeline-preview but as a sticky side surface, so the
    /// user can study what just ran without scrolling the trail.
    Pipeline,
    /// Live list of URLs the agent has touched this session — every
    /// FetchUrl, BrowseWeb, and WebSearch hit lands here so the
    /// operator can trace where each fact came from.
    Sources,
    /// The last action result, rendered by the typed per-action renderers
    /// ported from the legacy TUI's data pane. `F7` toggles raw JSON.
    Data,
    /// The matrix canvas, as a tab: the model-painted `<matrix>`/badge
    /// frames rendered at panel size instead of the small header slot.
    Matrix,
    /// The embedded shell, as a tab: the same live PTY `Ctrl+S` opens
    /// full-page, rendered in the panel. While this panel holds focus, keys
    /// go to the PTY — type, paste, answer a sudo prompt — and `Shift+Tab`
    /// leaves.
    Shell,
    /// Frontend hooks: desktop notifications on semantic events, listed as
    /// toggle rows. Focus the panel, ↑/↓ to pick, Space to enable/disable.
    /// All OFF by default — a hook fires only because a human switched it on.
    Hooks,
    /// Cloud cost + GPU: burn rate, per-resource $/h, GPU gauges with their
    /// attached processes, and whether the billed figure is real or estimated.
    Infra,
}

impl SidePanel {
    pub fn cycle_next(self) -> Self {
        match self {
            Self::Conversation => Self::Queue,
            Self::Queue => Self::Tasks,
            Self::Tasks => Self::Cron,
            Self::Cron => Self::Pipeline,
            Self::Pipeline => Self::Sources,
            Self::Sources => Self::Data,
            Self::Data => Self::Matrix,
            Self::Matrix => Self::Shell,
            Self::Shell => Self::Hooks,
            Self::Hooks => Self::Infra,
            Self::Infra => Self::Pulse,
            Self::Pulse => Self::Snapshots,
            Self::Snapshots => Self::Handles,
            Self::Handles => Self::Worlds,
            Self::Worlds => Self::Goal,
            Self::Goal => Self::Conversation,
        }
    }
    pub fn cycle_prev(self) -> Self {
        match self {
            Self::Conversation => Self::Goal,
            Self::Queue => Self::Conversation,
            Self::Tasks => Self::Queue,
            Self::Cron => Self::Tasks,
            Self::Pipeline => Self::Cron,
            Self::Sources => Self::Pipeline,
            Self::Data => Self::Sources,
            Self::Matrix => Self::Data,
            Self::Shell => Self::Matrix,
            Self::Hooks => Self::Shell,
            Self::Infra => Self::Hooks,
            Self::Pulse => Self::Infra,
            Self::Snapshots => Self::Pulse,
            Self::Handles => Self::Snapshots,
            Self::Worlds => Self::Handles,
            Self::Goal => Self::Worlds,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conv",
            Self::Queue => "queue",
            Self::Snapshots => "snaps",
            Self::Handles => "handles",
            Self::Worlds => "worlds",
            Self::Goal => "goal",
            Self::Tasks => "tasks",
            Self::Cron => "cron",
            Self::Pulse => "pulse",
            Self::Pipeline => "pipeline",
            Self::Sources => "sources",
            Self::Data => "data",
            Self::Matrix => "matrix",
            Self::Shell => "shell",
            Self::Hooks => "hooks",
            Self::Infra => "infra",
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Conversation => "◉",
            Self::Queue => "📋",
            Self::Snapshots => "◐",
            Self::Handles => "⏳",
            Self::Worlds => "▦",
            Self::Goal => "◆",
            Self::Tasks => "✓",
            Self::Cron => "⏰",
            Self::Pulse => "❤",
            Self::Pipeline => "▶",
            Self::Sources => "📚",
            Self::Data => "▤",
            Self::Matrix => "⫸",
            Self::Shell => "🐚",
            Self::Hooks => "🔔",
            Self::Infra => "☁",
        }
    }
    pub fn all() -> &'static [SidePanel] {
        &[
            Self::Conversation,
            Self::Queue,
            Self::Tasks,
            Self::Cron,
            Self::Pipeline,
            Self::Sources,
            Self::Data,
            Self::Matrix,
            Self::Shell,
            Self::Hooks,
            Self::Infra,
            Self::Pulse,
            Self::Snapshots,
            Self::Handles,
            Self::Worlds,
            Self::Goal,
        ]
    }
}

/// One line in the conversation/action trail.
#[derive(Debug, Clone)]
pub struct ConversationLine {
    pub kind: LineKind,
    pub text: String,
    pub status: Option<ToolStatus>,
    /// Tool-line metadata. Set on the FIRST line emitted for a
    /// given action (the header row); all subsequent preview rows
    /// for the same action have these set to None. Allows Ctrl+E
    /// to find action heads and expand them inline.
    pub action_head: bool,
    pub action_input: Option<serde_json::Value>,
    pub action_result: Option<serde_json::Value>,
    pub action_duration_ms: u64,
    /// True when the user has Ctrl+E'd this action head. Renderer
    /// follows the normal line with a JSON dump of input + result.
    pub expanded: bool,
}

impl ConversationLine {
    pub fn new(kind: LineKind, text: String) -> Self {
        Self {
            kind,
            text,
            status: None,
            action_head: false,
            action_input: None,
            action_result: None,
            action_duration_ms: 0,
            expanded: false,
        }
    }
    pub fn tool(text: String, status: ToolStatus) -> Self {
        Self {
            kind: LineKind::Tool,
            text,
            status: Some(status),
            action_head: false,
            action_input: None,
            action_result: None,
            action_duration_ms: 0,
            expanded: false,
        }
    }
}

/// Whether the compute this session drives is inside an attested enclave.
///
/// The objective is a GCP confidential VM with a confidential-mode GPU, so
/// the operator needs this visible at all times rather than on request — the
/// difference between "my weights are protected from the host" and "they are
/// not" is not something to go and check.
///
/// # Why there is no grey "unknown"
///
/// There is, and it is [`EnclaveStatus::Standard`] — red — because on
/// ordinary infrastructure the honest report is *not confidential*, not
/// *unknown*. `x0ne-confidential` says it plainly: "absence of a claim is not
/// a weak claim" (`model.rs`, `ExecutionEnvironment::Standard`). A grey
/// indicator would let an operator read "probably fine".
///
/// # Why [`EnclaveStatus::Attested`] cannot be set from a claim
///
/// A VM reports its own posture, and a compromised host reports whatever it
/// likes. `GpuCcMode::On` is a *claim*; a [`x0ne_confidential::model::Verdict::Verified`] is evidence
/// checked against an expected measurement. Only the second may set this
/// variant — the constructor below is the only way in, and it takes a
/// verdict. `GpuCcMode::DevTools` is deliberately not green either: the crate
/// requires policy to reject it wherever weights are staked.
///
/// Nothing sets `Attested` today, because subsystem 2 (attestation) is not
/// built. The indicator therefore reads red, which is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnclaveStatus {
    /// Ordinary infrastructure. No hardware confidentiality claim exists.
    #[default]
    Standard,
    /// A confidential VM whose CPU TEE is attested, with no confidential GPU
    /// — protected memory, unprotected accelerator. Amber: real, not enough.
    CpuOnly,
    /// CPU TEE and GPU confidential mode, both attested.
    Attested,
}

impl EnclaveStatus {
    /// The only route to a non-`Standard` value, so a reported posture can
    /// never reach the indicator without evidence behind it.
    ///
    /// `gpu_cc_on` is the *claimed* GPU mode; it downgrades `Attested` to
    /// `CpuOnly` but can never upgrade anything on its own. The verdict is
    /// what carries the authority, and a rejected one lands on `Standard`
    /// however good the claim looked.
    pub fn from_verdict(verdict: &x0ne_confidential::model::Verdict, gpu_cc_on: bool) -> Self {
        if !verdict.is_verified() {
            return Self::Standard;
        }
        if gpu_cc_on {
            Self::Attested
        } else {
            Self::CpuOnly
        }
    }

    /// The ONE producer, from the outcome of an `x0ne cc` run.
    ///
    /// # Why this exists
    ///
    /// Until 2026-08-16 nothing in the program called [`Self::from_verdict`],
    /// so the indicator read `○ confidential GPU disabled` on every session including
    /// the ones inside a proven enclave. That is the repo's recurring defect
    /// — a flag that is written, documented, read, and connected to no
    /// producer (docs/invariants.md, "dead scaffolding").
    ///
    /// # SECURITY: still no route that does not pass through a verdict
    ///
    /// This does not add a second door. It reads the verdict the
    /// [`crate::cc_runner::ProofBundle`] carries — a bundle exists only where
    /// `ProofBundle::from_verdict` accepted a `Verified` — and hands it to
    /// [`Self::from_verdict`], which is still the only thing that can return a
    /// non-`Standard` value. `Refused` and `Failed` have no proof to read and
    /// land on `Standard`, which is the honest answer: a run that failed
    /// measured nothing worth a colour.
    ///
    /// The GPU half is the outcome's own `GpuEvidence`, and it is a *claim*
    /// exactly as `from_verdict`'s doc says: it can only downgrade `Attested`
    /// to `CpuOnly`, never light anything on its own.
    pub fn from_outcome(outcome: &crate::cc_runner::CcOutcome) -> Self {
        use crate::cc_runner::{CcOutcome, GpuEvidence};
        match outcome {
            CcOutcome::Proven { proof, gpu, .. } => Self::from_verdict(
                proof.verdict(),
                gpu.as_ref().is_some_and(GpuEvidence::cc_mode_on),
            ),
            // A run that stopped may hold a real CPU proof (`Failed` carries
            // one), and the report prints it. The BADGE is deliberately not
            // derived from it: the badge claims "this session runs inside a
            // proven enclave", and a pipeline that failed before serving the
            // model never established that. The proof stays in the report,
            // where it is a measurement rather than a promise.
            CcOutcome::Failed { .. } | CcOutcome::Refused { .. } => Self::Standard,
        }
    }

    /// The wire name for this status — what `~/.x0ne/cc/last-report.json` and
    /// `GET /attestation` publish.
    ///
    /// SECURITY: a *state*, never a bare boolean. A front end handed
    /// `verified: true/false` computes the colour itself, and then the rule
    /// for what may be green lives in a browser instead of here — which is
    /// the whole reason `CpuOnly` exists as a third value rather than as a
    /// `true` with a footnote.
    pub fn wire_state(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::CpuOnly => "cpu_only",
            Self::Attested => "attested",
        }
    }

    /// Read a status back off the wire.
    ///
    /// SECURITY: unknown text is `None`, never a default. This is what a
    /// report file read from disk goes through, and a file is something an
    /// attacker with local write access can rewrite — a `_ => Standard` arm
    /// would be harmless, but a `_ => Attested` or a silent `unwrap_or` on the
    /// wrong side would not, and the exhaustive match is what keeps that
    /// decision explicit. The caller decides what an unreadable state means;
    /// this function refuses to guess.
    pub fn from_wire(state: &str) -> Option<Self> {
        match state {
            "standard" => Some(Self::Standard),
            "cpu_only" => Some(Self::CpuOnly),
            "attested" => Some(Self::Attested),
            _ => None,
        }
    }

    /// The badge for a RUNNING session, from the last attestation report and
    /// what this session actually talks to.
    ///
    /// # Why a report alone is not enough
    ///
    /// `x0ne cc` runs on the operator's laptop and writes its report there.
    /// The session that `x0ne cc … enter` opens does NOT run on that laptop:
    /// `enclave_ssh_argv` is `gcloud compute ssh <vm> -- -t ./x0ne`, so the
    /// process that renders this badge is a *different process on the machine
    /// that was attested* (measured 2026-08-16 while wiring this producer —
    /// there is no in-process handoff to make). Lighting the badge from "a
    /// proven report exists somewhere on this disk" would therefore turn every
    /// local session green for an hour after any successful `x0ne cc`, while
    /// the conversation went to a hosted API.
    ///
    /// So the badge is a conjunction, and both halves are about THIS session:
    ///
    /// 1. a fresh report says the enclave was proven — carried into the VM by
    ///    `x0ne cc` before it opens the session, and read here through
    ///    [`crate::cc_runner::attestation_view`], the same function
    ///    `GET /attestation` serves, so a console and a terminal cannot show
    ///    different colours for the same machine;
    /// 2. the model this session talks to is reachable only on loopback AND is
    ///    the model the report names. Inside the enclave that is vLLM on
    ///    `127.0.0.1`, serving the model out of TDX-encrypted RAM
    ///    (`enclave_x0ne_config`). Anywhere else it is an address on the
    ///    internet, and a conversation that leaves the machine is not protected
    ///    by the machine.
    ///
    /// # SECURITY: what this is worth, exactly
    ///
    /// The evidence is the verdict `x0ne cc` computed and recorded; this
    /// function only decides whether that verdict is about the thing in front
    /// of the operator. It cannot manufacture a verdict: an absent, stale,
    /// forged or unparseable report is `Standard` by
    /// [`crate::cc_runner::attestation_view`], and `Standard` is the value
    /// every failure of every check below returns.
    ///
    /// The residual, stated rather than hidden: an operator who attests a VM
    /// and, within the hour, points a local session at a loopback server
    /// running the same model name gets a green badge for a local server. It
    /// takes deliberate effort to build and it is not a remote attack; closing
    /// it properly needs the in-VM binary to attest itself, which is the
    /// attestation subsystem, not a string comparison.
    pub fn for_session(
        report: Option<&serde_json::Value>,
        now: chrono::DateTime<chrono::Utc>,
        config: &crate::config::Config,
    ) -> Self {
        let view = crate::cc_runner::attestation_view(report, now);
        let Some(status) = view
            .get("state")
            .and_then(serde_json::Value::as_str)
            .and_then(Self::from_wire)
        else {
            return Self::Standard;
        };
        if status == Self::Standard {
            return Self::Standard;
        }
        // The endpoint. `None` — a hosted API with no configurable address —
        // is not loopback, and is not "unknown" either.
        let Some(url) = config.active_base_url() else {
            return Self::Standard;
        };
        if !is_loopback_url(url) {
            return Self::Standard;
        }
        // And it must be the model the report says was served. A loopback
        // address alone would light the badge for any local server the
        // operator happens to be running.
        let served = report
            .and_then(|r| r.get("model"))
            .and_then(serde_json::Value::as_str);
        match served {
            Some(m) if m == config.active_model_name() => status,
            _ => Self::Standard,
        }
    }

    /// Glyph, label and colour for the status strip.
    pub fn badge(self) -> (&'static str, &'static str, (u8, u8, u8)) {
        match self {
            Self::Standard => ("○", "confidential GPU disabled", (200, 80, 70)),
            Self::CpuOnly => ("◐", "cpu enclave, gpu exposed", (215, 165, 60)),
            Self::Attested => ("●", "confidential", (0, 200, 120)),
        }
    }
}

/// Does this base URL name the machine the process is running on?
///
/// SECURITY: parsed, never pattern-matched on the string. `http://127.0.0.1.evil.com/`
/// contains "127.0.0.1" and is a public host; `http://user@127.0.0.1@evil.com/`
/// is the same trick with an authority separator. The host component is what
/// decides, and anything that does not parse as a URL is not loopback.
///
/// `localhost` counts: it is what an operator writes, and resolving it to
/// something else requires already controlling the machine's own resolver.
fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    // `url` keeps IPv6 literals bracketed; strip them before parsing.
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    host.eq_ignore_ascii_case("localhost")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// "▸ you: ..."
    User,
    /// "▸ x0ne: ..."
    Assistant,
    /// "  ↳ tool_name · arg"
    Tool,
    /// "  ⋮ system note"
    System,
    /// "  ✗ error"
    Error,
}

/// One-shot status icon decoration for a tool line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Streaming,
    Ok,
    Failed,
    Cancelled,
    Snapshot,
}

/// Body strip indicator — runtime / vm / browser presence.
#[derive(Debug, Clone, Copy)]
pub struct BodyStatus {
    pub runtime: BodyState,
    pub vm: BodyState,
    pub browser: BodyState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyState {
    /// Process not started / opt-in disabled.
    Off,
    /// Connected but not currently executing anything.
    Idle,
    /// In use right now (last tool call was this body).
    Active,
}

impl BodyState {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Off => "○",
            Self::Idle => "◌",
            Self::Active => "◉",
        }
    }
}

/// Single source of truth for the screen.
pub struct AppState {
    /// All conversation lines (oldest first). Auto-trimmed past
    /// `MAX_CONVERSATION_LINES` to keep memory bounded; ratatui renders
    /// the visible tail.
    pub conversation: Vec<ConversationLine>,

    /// Bottom input zone. Owns the typing buffer + cursor.
    pub input: TextArea<'static>,

    /// Active side panel.
    pub side_panel: SidePanel,

    /// Header info — refreshed by `on_tick`.
    pub model_label: String,
    pub session_id: String,
    pub tokens_used: u64,
    pub cost_usd: f64,
    pub sandbox_label: String,
    /// Plan mode, as the session last reported it (`AgentEvent::SessionMode`).
    pub plan_mode: bool,
    /// Whether actions land in a container, and which one provides it.
    /// Derived once at startup from the bridge config, never from a runtime
    /// claim — see [`crate::container_runtime::Containment`].
    pub containment: crate::container_runtime::Containment,
    pub bodies: BodyStatus,

    /// Pinned goal (if any).
    pub goal_label: Option<String>,
    pub goal_step: Option<(u32, u32)>,

    /// The persistent goal, formatted for the `Goal` panel.
    ///
    /// SECURITY: filled **only** by the frontend, from
    /// `GoalManager::get_active` — see [`GoalSummary`]. `None` means
    /// « no active goal », and the panel says so; it never shows an empty
    /// goal, which would read as a goal whose text has disappeared.
    pub goal_summary: Option<GoalSummary>,

    /// Whether the agent loop is currently working. Drives the spinner
    /// in the header and gates Enter-submit behavior (when busy, Enter
    /// pushes to the queue instead of triggering a turn).
    pub agent_busy: bool,
    /// A prompt went to the runner and its `AssistantStart` is not drained
    /// yet: the turn runs already, so Ctrl-C must stop it, not quit.
    pub turn_pending: bool,
    pub turn_started_at: Option<Instant>,

    /// Monotonic tick counter, incremented by `on_tick` every ~80ms.
    /// Used by the renderer to drive subtle animations (logo shimmer
    /// when busy, ellipsis cycling in the "thinking…" indicator).
    /// Wraps freely; consumers should always read it modulo a period.
    pub animation_phase: u32,

    /// True between an `AssistantStart` and the first `Token` of that
    /// turn. The renderer surfaces a one-line "x0ne is reflecting …"
    /// hint so the user never stares at dead air while the model
    /// chews on a long first-token latency.
    pub awaiting_first_token: bool,

    /// LLM-painted badge for the X0NE logo slot. `None` = render the
    /// default animated logo; `Some` = the model has taken control of
    /// the badge. A playback holds 1+ frames + delay + loop flag;
    /// single-frame payloads are still supported as a degenerate
    /// 1-frame playback. Advanced by `on_tick` when due.
    pub badge: Option<BadgePlayback>,

    /// How many visual rows we're scrolled UP from the bottom of the
    /// conversation. 0 = pinned to bottom (the default — every new
    /// turn auto-scrolls to land in view). Incremented by PgUp /
    /// scroll-wheel-up, decremented by PgDn / scroll-wheel-down.
    pub scroll_offset: usize,
    /// Where each conversation row landed last frame, so a click can find the
    /// action under the cursor. Written by the renderer, read by the mouse
    /// handler; empty until the first draw.
    pub conv_hits: crate::tui::render::ConvHits,
    /// True while the user hasn't manually scrolled — keeps the
    /// view stuck to the bottom as new content arrives. Set to
    /// false on any user scroll up, back to true when they reach
    /// the bottom again (offset = 0).
    pub auto_stick_bottom: bool,

    /// In-flight / latest tool action — captured on `ToolStart` and
    /// updated on `ToolEnd`. Powers the Ctrl+E "expand modal" that
    /// shows the full input + full result JSON. `None` until the
    /// agent dispatches its first action this session.
    pub last_action_input: Option<serde_json::Value>,
    pub last_action_result: Option<serde_json::Value>,
    pub last_action_name: Option<String>,
    /// Set to `Some(Instant)` between ToolStart and ToolEnd so the
    /// modal can show elapsed time. `None` when no action is mid-flight.
    pub last_action_started: Option<Instant>,
    /// Ctrl+E modal toggle — overlays the body with a fullscreen
    /// view of the latest tool action.
    pub expand_action_modal: bool,

    /// Focus mode toggle (Ctrl+B). When `true`, the side panel is
    /// hidden and the conversation expands to fill the whole body
    /// width — better for reading long markdown answers without
    /// distraction. Tab still cycles which panel WOULD be shown if
    /// focus mode is exited.
    pub focus_mode: bool,

    /// Bash/readline-style history of submitted user messages.
    /// Most recent at the END. Populated on every `submit_input`
    /// (dedup against the immediately preceding entry so spammed
    /// repeats don't pollute the list).
    pub input_history: Vec<String>,
    /// Pointer into `input_history`. `None` = the textarea reflects
    /// fresh typing (or never recalled); `Some(i)` = the textarea
    /// currently shows `input_history[i]`. Navigated via ↑/↓ when
    /// the cursor is at the first/last row of the textarea (bash
    /// convention — gives Up/Down their usual line-move job for
    /// multi-line edits, and the history role at boundaries).
    pub history_cursor: Option<usize>,
    /// Slash-command completion in flight: the prefix the operator typed
    /// before the first Tab, and the index of the candidate currently shown.
    /// Reset by any other key. See `events::complete_slash`.
    pub slash_cycle: Option<(String, usize)>,

    /// User asked to quit (Ctrl-C / `:q`). The run-loop reads this on
    /// the next iteration and tears down the terminal cleanly.
    pub should_quit: bool,
    /// When Ctrl-C last stopped a turn: a second press within 2 s quits.
    pub last_interrupt: Option<Instant>,

    /// Buffer for the in-progress assistant response. As tokens stream
    /// in from the runner we keep accumulating into this; once a full
    /// `AssistantEnd` arrives it gets flushed as a single line to the
    /// conversation (so wrap behaves nicely).
    pub assistant_buffer: String,

    /// Which pane the last navigation key addressed. Drives the focus
    /// styling: the focused pane's border brightens, the other dims.
    pub focus: FocusRegion,
    /// The last completed action, for the Data panel's typed rendering:
    /// `(action_type, result envelope)`. Presentation state only.
    pub last_result: Option<(String, serde_json::Value)>,
    /// `F7` — show the Data panel's result as raw coloured JSON instead of
    /// the typed rendering. The legacy data pane's toggle, ported.
    pub data_raw: bool,
    /// Live parallel-worlds view, ported from the legacy worlds pane. Fed
    /// from `worlds_registry::global()` on tick; lazily subscribed the first
    /// time the panel is drawn with worlds present.
    pub worlds_renderer: Option<crate::worlds_view::WorldsRenderer>,
    /// The live half of the registry subscription.
    pub worlds_rx: Option<tokio::sync::broadcast::Receiver<crate::worlds_view::WorldEvent>>,
    /// Host metrics for the header strip — the legacy sysmon line, ported.
    /// `None` until `run_tui` starts the monitor and hands the state over.
    pub sys_state: Option<std::sync::Arc<std::sync::RwLock<crate::sysmon::SysState>>>,
    /// Cloud cost + GPU state, shared with `cloudmon`. `None` when cloud
    /// monitoring is disabled — the Infra panel then says so rather than
    /// rendering zeros that would read as "costs nothing".
    pub cloud_state: Option<std::sync::Arc<std::sync::RwLock<crate::cloudmon::CloudState>>>,
    /// Idle line for the status strip ("connected · ready when you are").
    /// Shown when no activity status is; replaced the moment one appears.
    pub idle_status: Option<String>,
    /// Whether this session's compute is running inside an attested enclave.
    /// Sits beside `idle_status`; see [`EnclaveStatus`] for why it starts red
    /// rather than grey and what it takes to turn it green.
    pub enclave: EnclaveStatus,
    /// Frontend notification hooks (the Hooks panel rows). Presentation
    /// preferences, not execution: a hook here can only ever notify.
    pub hooks: Vec<HookRow>,
    /// Cursor inside the Hooks panel.
    pub hooks_cursor: usize,
    /// Active confirmation modal (if any). Owns the keyboard while pending,
    /// exactly like the AskUser modal, and for the same reason: the engine is
    /// parked on this answer.
    pub pending_confirm: Option<ConfirmState>,
    /// Active AskUser modal (if any). `Some(_)` makes the renderer paint
    /// the MCQ overlay over the input bar and the key handler enter
    /// modal-mode. Drained by a [`TuiCommand::AskAnswer`] that carries
    /// the same `id`.
    pub pending_ask: Option<AskState>,

    /// True when the F1 help overlay is visible. A centered floating
    /// card listing every keyboard shortcut, slash command, side panel,
    /// and LLM meta-action — one stop, no terminology to memorise.
    pub show_help: bool,

    /// Shell mode (Ctrl+S) — full-body overlay that turns the chat
    /// surface into a real local shell. Commands run via `bash -c`
    /// on the host (not the sandbox — that's what `RunCommand` is for
    /// in agent mode); the shell trail accumulates here and is fed
    /// to the LLM as context on the next agent turn so it sees
    /// what you just did.
    pub shell_mode: bool,
    /// Persistent CWD for shell mode. Since each `bash -c` is a
    /// fresh process, we track `cd` client-side and prepend it.
    /// Initialised to $PWD on first toggle.
    pub shell_cwd: std::path::PathBuf,
    /// Shell trail. Nothing pushes to it since `push_shell_entry` was
    /// removed (2026-09-23, no caller); it is only read and cleared.
    pub shell_history: Vec<ShellEntry>,
    /// Currently-running shell command (if any). Set just before
    /// spawn, cleared on completion. Enables a "⋯ running" hint and
    /// blocks Enter from queueing a second command on top.
    pub shell_running: Option<String>,
    /// Live embedded terminal. `Some(_)` while shell mode is active
    /// — holds the PTY + parser state. None when shell mode is off.
    /// htop / vim / ping / ssh / sudo all run live inside this
    /// surface; the render layer paints the vt100 grid as ratatui
    /// cells. Created on `Ctrl+S` enter, dropped on exit.
    pub pty: Option<super::pty_term::PtyTerm>,

    /// Live TODO list (Tasks panel). Mutated by `TaskUpdate` events
    /// from the runner — which fires them when the LLM emits
    /// `TaskAdd` / `TaskDone` / `TaskClear` meta-actions or the user
    /// types `/task add …`. In-memory only.
    pub tasks: Vec<TaskItem>,
    pub next_task_id: u32,

    /// Scheduled actions (Cron panel). Tick-driven; `runner` ticks
    /// once per second and fires due items.
    pub crons: Vec<CronItem>,
    pub next_cron_id: u32,

    /// Pulse timeline — auto-derived from every AgentEvent. Bounded
    /// to MAX_PULSE_EVENTS so the panel stays responsive on long
    /// sessions.
    pub pulse: Vec<PulseEvent>,

    /// Most recent Pipeline run for the dedicated Pipeline panel.
    /// Auto-parsed from the ToolEnd result envelope when the action
    /// type is "Pipeline". Stays on screen after completion so the
    /// user can review the breakdown.
    pub last_pipeline: Option<PipelineRun>,

    /// Every URL the agent has touched this session — populated by
    /// `apply_event` when a network action (FetchUrl, BrowseWeb,
    /// WebSearch) completes. Bounded to MAX_SOURCES so a runaway
    /// crawl can't eat memory; the full audit is in the jsonl.
    pub sources: Vec<SourceEntry>,

    /// Channels to/from the agent runner. `cmd_tx` is `None` when the
    /// runner has terminated (boot failed or shutdown). `evt_rx` is
    /// `None` after we move it out for draining.
    pub cmd_tx: Option<mpsc::Sender<TuiCommand>>,
    pub evt_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
}

const MAX_CONVERSATION_LINES: usize = 2048;
const MAX_PULSE_EVENTS: usize = 200;
const MAX_SOURCES: usize = 200;

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// The input box, styled to look like what it will become.
///
/// Text you are typing and text you have sent are the same words seconds
/// apart, so they read as the same thing: violet, italic, no highlight.
/// `TextArea::default()` underlines the cursor line, which put a bar under
/// the message while it was being written and made the box look like a form
/// field rather than a line of the conversation.
///
/// One constructor because there are three sites that build this box — the
/// initial state, the history-navigation replace, and the post-send reset —
/// and a style set in only two of them would flicker on send.
pub fn styled_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_placeholder_text("Type a message · Tab to switch panel · Enter to send");
    input.set_style(
        Style::default()
            .fg(crate::tui::render::VIOLET)
            .add_modifier(Modifier::ITALIC),
    );
    // Empty, not "default": default is an underline.
    input.set_cursor_line_style(Style::default());
    input
}

impl AppState {
    pub fn new() -> Self {
        let input = styled_input();
        Self {
            conversation: Vec::with_capacity(256),
            input,
            side_panel: SidePanel::Conversation,
            // No model is known before `Ready`; naming one lied.
            model_label: "connecting…".into(),
            session_id: short_session_id(),
            tokens_used: 0,
            cost_usd: 0.0,
            sandbox_label: "local".into(),
            plan_mode: false,
            containment: crate::container_runtime::Containment::Host,
            bodies: BodyStatus {
                runtime: BodyState::Idle,
                vm: BodyState::Off,
                browser: BodyState::Off,
            },
            goal_label: None,
            goal_step: None,
            goal_summary: None,
            agent_busy: false,
            turn_pending: false,
            turn_started_at: None,
            animation_phase: 0,
            awaiting_first_token: false,
            badge: None,
            scroll_offset: 0,
            conv_hits: Default::default(),
            enclave: EnclaveStatus::default(),
            auto_stick_bottom: true,
            focus_mode: false,
            last_action_input: None,
            last_action_result: None,
            last_action_name: None,
            last_action_started: None,
            expand_action_modal: false,
            input_history: Vec::new(),
            history_cursor: None,
            slash_cycle: None,
            should_quit: false,
            assistant_buffer: String::new(),
            pending_ask: None,
            pending_confirm: None,
            focus: FocusRegion::default(),
            last_result: None,
            data_raw: false,
            worlds_renderer: None,
            worlds_rx: None,
            sys_state: None,
            cloud_state: None,
            idle_status: None,
            hooks: HookRow::defaults(),
            hooks_cursor: 0,
            show_help: false,
            shell_mode: false,
            shell_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
            shell_history: Vec::new(),
            shell_running: None,
            pty: None,
            tasks: Vec::new(),
            next_task_id: 1,
            crons: Vec::new(),
            next_cron_id: 1,
            pulse: Vec::new(),
            last_pipeline: None,
            sources: Vec::new(),
            cmd_tx: None,
            evt_rx: None,
            last_interrupt: None,
        }
    }

    /// Build the system note that gets prepended to the next agent
    /// turn whenever the shell trail has new entries. Caller resets
    /// the `last_shared_at` cursor by calling `mark_shell_shared`.
    /// Compact format so token cost stays bounded:
    /// `$ cmd → exit_code · stdout snippet`.
    pub fn shell_context_note(&self) -> Option<String> {
        if self.shell_history.is_empty() {
            return None;
        }
        let mut out = String::from("[shell trail since last turn]\n");
        // Include up to the last 8 commands — enough for context,
        // bounded enough to keep token cost in check.
        let start = self.shell_history.len().saturating_sub(8);
        for entry in &self.shell_history[start..] {
            out.push_str(&format!(
                "$ {}\n  ↳ exit={} · ({}ms)\n",
                trim_inline(&entry.cmd, 80),
                entry.exit_code,
                entry.duration_ms,
            ));
            if !entry.stdout.trim().is_empty() {
                let snippet = trim_inline(entry.stdout.trim(), 400);
                out.push_str(&format!("  stdout: {}\n", snippet));
            }
            if !entry.stderr.trim().is_empty() {
                let snippet = trim_inline(entry.stderr.trim(), 200);
                out.push_str(&format!("  stderr: {}\n", snippet));
            }
        }
        Some(out)
    }

    /// Append a pulse event, trimming the oldest entries past the cap
    /// so memory stays bounded over long sessions.
    pub fn push_pulse(&mut self, kind: PulseKind, label: impl Into<String>) {
        if self.pulse.len() >= MAX_PULSE_EVENTS {
            self.pulse.drain(..MAX_PULSE_EVENTS / 4);
        }
        self.pulse.push(PulseEvent {
            at: Instant::now(),
            kind,
            label: label.into(),
        });
    }

    /// Wire the runner's channels into the state. Called once at startup.
    pub fn attach_channels(
        &mut self,
        cmd_tx: mpsc::Sender<TuiCommand>,
        evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        self.cmd_tx = Some(cmd_tx);
        self.evt_rx = Some(evt_rx);
    }

    /// Send a command to the runner; non-blocking. Drops the command
    /// on the floor if the runner is gone — caller decides whether to
    /// surface that to the user.
    pub fn send_command(&mut self, cmd: TuiCommand) -> bool {
        let starts_turn = matches!(
            cmd,
            TuiCommand::UserInput(_) | TuiCommand::UserInputWithImages { .. }
        );
        if let Some(tx) = &self.cmd_tx {
            let sent = tx.try_send(cmd).is_ok();
            self.turn_pending |= sent && starts_turn;
            sent
        } else {
            false
        }
    }

    /// Pull every event currently queued from the runner. Called from
    /// the main render loop each tick — keeps the UI in sync without
    /// async-ifying the render path.
    pub fn drain_agent_events(&mut self) {
        // Take the receiver out to avoid borrowing `self` twice.
        let Some(mut rx) = self.evt_rx.take() else {
            return;
        };
        while let Ok(ev) = rx.try_recv() {
            self.apply_event(ev);
        }
        self.evt_rx = Some(rx);
    }

    /// Mutate state in response to one runner event. Public for unit
    /// testing — feed event sequences and assert on resulting state
    /// without needing a real runner.
    pub fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::SessionMode { plan } => self.plan_mode = plan,
            AgentEvent::Ready {
                model_label,
                sandbox_label,
            } => {
                self.model_label = model_label;
                self.sandbox_label = sandbox_label;
                self.bodies.runtime = BodyState::Idle;
                // Lives in the status strip above the input, not in the
                // conversation: it is presence, not dialogue. The activity
                // status ("x0ne is thinking…") replaces it while a turn
                // runs, and it returns between turns.
                self.idle_status = Some("connected · ready when you are".into());
            }
            // The engine asks; the frontend relays the question to the
            // person and transports their answer back. It decides nothing:
            // what an answer authorises is the ApprovalAuthority's business.
            AgentEvent::ConfirmationRequested {
                id,
                action,
                action_type,
                reason,
                persistent_grant,
            } => {
                if self.hook_enabled("notify-approval") {
                    desktop_notify(
                        "X0NE approval needed",
                        &format!("{action_type} is waiting for your decision"),
                    );
                }
                // No transcript line here. The modal is a blocking overlay
                // that already names the action, states the reason and lists
                // the choices — it *is* the announcement, and it is
                // impossible to miss. A row saying "awaiting your decision"
                // underneath it announced a third time what the pulse and the
                // modal had each already said, and then survived into the
                // scrollback as a permanent marker for a wait that lasted two
                // seconds. What the transcript keeps is the *outcome*, and
                // only when the outcome is not otherwise visible — see
                // `handle_confirm_key`.
                self.push_pulse(PulseKind::Ask, format!("⚠ {action_type}"));
                self.pending_confirm = Some(ConfirmState {
                    id,
                    action_type,
                    action,
                    reason,
                    persistent_grant,
                    cursor: 0,
                });
            }
            AgentEvent::ActionDenied { name, reason, .. } => {
                self.push_line(ConversationLine::new(
                    LineKind::System,
                    format!("✗ {name} not run — {reason}"),
                ));
            }
            AgentEvent::PipelineStepStarted {
                index,
                total,
                step_type,
            } => {
                self.push_line(ConversationLine::new(
                    LineKind::System,
                    format!("· step {}/{} · {step_type}", index + 1, total),
                ));
            }
            AgentEvent::HandleTracked {
                handle_id,
                action_type,
            } => {
                self.push_line(ConversationLine::new(
                    LineKind::System,
                    format!("⧗ {action_type} running in the background · {handle_id}"),
                ));
            }
            // Display only. The engine owns enforcement; a frontend that
            // decided for itself is how the limits diverged in the first place.
            AgentEvent::BudgetUpdated { cost_usd, .. } => {
                // Cost only. `AssistantEnd` owns `tokens_used`.
                self.cost_usd = cost_usd;
            }
            AgentEvent::BootFailed(msg) => {
                self.model_label = "not connected".into();
                self.turn_pending = false;
                self.push_line(ConversationLine::new(
                    LineKind::Error,
                    format!("boot failed: {} (TUI continues in read-only mode)", msg),
                ));
                // Drop the command channel so future submits surface the
                // error path naturally.
                self.cmd_tx = None;
            }
            AgentEvent::AssistantStart => {
                self.agent_busy = true;
                self.turn_pending = false;
                self.awaiting_first_token = true;
                self.turn_started_at = Some(Instant::now());
                self.assistant_buffer.clear();
                self.bodies.runtime = BodyState::Active;
            }
            AgentEvent::Token(t) => {
                // First token of the turn flips off the "thinking…"
                // hint — from here on the user sees the response stream.
                self.awaiting_first_token = false;
                self.assistant_buffer.push_str(&t);
            }
            AgentEvent::AssistantEnd {
                total_tokens,
                cost_usd,
            } => {
                if self.hook_enabled("notify-turn-end") {
                    desktop_notify(
                        "X0NE turn finished",
                        &format!("{} tokens this session", total_tokens),
                    );
                }
                self.tokens_used = total_tokens;
                self.cost_usd = cost_usd;
                self.agent_busy = false;
                self.turn_pending = false;
                self.awaiting_first_token = false;
                self.bodies.runtime = BodyState::Idle;
                self.push_pulse(PulseKind::Turn, format!("turn · {} tok", total_tokens));
                let buf = std::mem::take(&mut self.assistant_buffer);
                if !buf.trim().is_empty() {
                    // Strip <matrix> + <action> wrappers before
                    // display (handled separately). Store the raw
                    // cleaned text as a SINGLE Assistant line — the
                    // renderer expands it into the `╭─ x0ne ─… / │ …
                    // / ╰─…` box at draw time using the live panel
                    // width. That's what makes the box fill the
                    // window dynamically instead of a hard-coded
                    // 40-char bar.
                    let cleaned = strip_inline_blocks(&buf);
                    if !cleaned.trim().is_empty() {
                        self.push_line(ConversationLine::new(LineKind::Assistant, cleaned));
                    }
                }
                self.turn_started_at = None;

                // Auto-process the next queued message if any, so chains
                // run without the user pressing Enter again.
                if let Some(next) = input_queue::pop_front() {
                    self.push_line(ConversationLine::new(LineKind::User, next.clone()));
                    let _ = self.send_command(TuiCommand::UserInput(next));
                }
            }
            AgentEvent::ToolStart {
                name,
                target,
                action,
            } => {
                // Capture the in-flight action for Ctrl+E expand modal.
                self.last_action_name = Some(name.clone());
                self.last_action_input = Some(action);
                self.last_action_result = None;
                self.last_action_started = Some(Instant::now());

                // No transcript line. The three fields above already drive the
                // live status row — `▶ x0ne is running RunCommand · 2.3s`
                // (`render.rs:1854`) — which is the whole job this line used
                // to do: name the action in flight.
                //
                // It named it a second time once `ToolEnd` grew a real call
                // header, and the result was a bare `RunCommand` sitting on
                // top of `▸ exec · RunCommand` for every single action
                // (screenshot 2026-08-13_04-47). The name is now carried by
                // the header, with its command; a label with neither is not
                // worth a row.
                let _ = target;
            }
            AgentEvent::ToolEnd {
                name,
                ok,
                summary,
                action,
                result,
                duration_ms,
            } => {
                self.last_result = Some((name.clone(), result.clone()));
                let status = if ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Failed
                };
                self.push_pulse(
                    if ok {
                        PulseKind::Tool
                    } else {
                        PulseKind::ToolError
                    },
                    format!("{} · {}ms", name, duration_ms),
                );
                // When a Pipeline lands, parse the per-step breakdown
                // into `last_pipeline` so the Pipeline panel shows
                // exactly what just ran.
                if name == "Pipeline" {
                    self.last_pipeline = parse_pipeline_result(&action, &result, duration_ms, ok);
                }
                // Auto-track URLs the agent touched (FetchUrl /
                // BrowseWeb / WebSearch) so the Sources panel reads
                // as a session bibliography without any LLM
                // cooperation. Underlying jsonl already has the
                // request+result, this is the at-a-glance view.
                for src in extract_sources(&name, &action, &result, ok) {
                    if self.sources.len() >= MAX_SOURCES {
                        self.sources.drain(..MAX_SOURCES / 4);
                    }
                    self.sources.push(src);
                }
                // Keep the latest action handy for the focus-mode
                // info bar; the per-line metadata (`action_head`)
                // below is what powers inline Ctrl+E expansion.
                self.last_action_name = Some(name.clone());
                self.last_action_input = Some(action.clone());
                self.last_action_result = Some(result.clone());
                self.last_action_started = None;
                // Both halves come from the CLI's own renderers, so a tool
                // call reads the same on either frontend. The returned string
                // is ANSI-coloured; the tui renderer converts it span-by-span
                // on draw via ansi-to-tui.
                let formatted = if action.get("type").is_some() && !result.is_null() {
                    crate::display::result_line(&action, &result, duration_ms)
                } else {
                    // Fallback when no result envelope is available
                    // (e.g. bridge missing): keep the simple summary.
                    let icon = if ok { "✓" } else { "✗" };
                    let body = if summary.is_empty() {
                        name.clone()
                    } else {
                        format!("{} · {}", name, summary)
                    };
                    format!("  └  {}  {}", icon, body)
                };
                // Push each formatted line as its own conv entry.
                // Mark the FIRST one as the action head + attach the
                // raw input/result/duration so Ctrl+E can find it and
                // dump the full JSON inline below.
                // SECURITY: the header carries WHAT was run, and until now the
                // TUI never printed it.
                //
                // `result_line` renders the outcome — `└ ok (<1ms)` and the
                // output box. `action_box` renders the call — `▸ exec ·
                // RunCommand` and, for RunCommand/RunInSandbox, the `$ cmd
                // args` line. The CLI prints both (`main.rs:2986-2987`); this
                // frontend printed only the second, so a transcript full of
                // `RunCommand / └ ok` said that something ran and never what.
                // Screenshot 2026-08-13_04-27: eight consecutive RunCommands,
                // not one command visible.
                //
                // An operator who cannot read the command cannot audit the
                // session, and approving a class of action while being unable
                // to see its instances is the thing the gate exists to
                // prevent. The comment below used to claim tool calls looked
                // "identical" to the CLI here; they did not.
                let head = crate::display::action_box(&action);
                let mut first = true;
                for line in head.lines().chain(formatted.lines()) {
                    if line.is_empty() {
                        continue;
                    }
                    let mut cl = ConversationLine::tool(line.to_string(), status);
                    if first {
                        cl.action_head = true;
                        cl.action_input = Some(action.clone());
                        cl.action_result = Some(result.clone());
                        cl.action_duration_ms = duration_ms;
                        first = false;
                    }
                    self.push_line(cl);
                }
            }
            AgentEvent::Snapshot { name, paths } => {
                let label = if paths.is_empty() {
                    format!("snapshot · {}", name)
                } else {
                    format!("snapshot · {} ({} paths)", name, paths.len())
                };
                self.push_pulse(PulseKind::Snapshot, label.clone());
                self.push_line(ConversationLine::tool(label, ToolStatus::Snapshot));
            }
            AgentEvent::GoalUpdate { label, step } => {
                let pulse_label = match (&label, &step) {
                    (Some(l), Some((cur, total))) => format!("{} ({}/{})", l, cur, total),
                    (Some(l), None) => l.clone(),
                    (None, _) => "goal cleared".to_string(),
                };
                self.push_pulse(PulseKind::Goal, pulse_label);
                self.goal_label = label;
                self.goal_step = step;
            }
            AgentEvent::HeaderTick {
                total_tokens,
                cost_usd,
            } => {
                self.tokens_used = total_tokens;
                self.cost_usd = cost_usd;
            }
            AgentEvent::ExternalUserInput { source, text } => {
                // Render as a User line with a source tag so the
                // operator instantly sees that the message came from
                // outside the TUI. Pulse feed picks it up too.
                // The generic external-bridge concept outlives the retired
                // messaging products; a future remote surface reuses it.
                let tag = source.as_str();
                self.push_line(ConversationLine::new(
                    LineKind::User,
                    format!("[{}] {}", tag, text),
                ));
                self.push_pulse(PulseKind::Turn, format!("via {}", source));
            }
            AgentEvent::Info(msg) => {
                // Driver turn telemetry ("turn completed", "turn denied"…)
                // is not dialogue. A completed turn is self-evident from the
                // response above it; the pulse records the fact, and only
                // non-completed outcomes earn a visible line.
                if msg == "turn completed" {
                    self.push_pulse(PulseKind::Turn, "turn completed".to_string());
                    return;
                }
                self.push_line(ConversationLine::new(LineKind::System, msg));
            }
            AgentEvent::Error(msg) => {
                if self.hook_enabled("notify-error") {
                    desktop_notify("X0NE error", &msg);
                }
                self.push_line(ConversationLine::new(LineKind::Error, msg));
            }
            AgentEvent::TaskAdded { id, title } => {
                if !self.tasks.iter().any(|t| t.id == id) {
                    self.tasks.push(TaskItem {
                        id,
                        title: title.clone(),
                        done: false,
                        created_at: Instant::now(),
                    });
                    self.next_task_id = self.next_task_id.max(id + 1);
                }
                self.push_pulse(
                    PulseKind::Task,
                    format!("+ task: {}", trim_inline(&title, 40)),
                );
            }
            AgentEvent::TaskCompleted { id } => {
                let title = self
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.title.clone())
                    .unwrap_or_else(|| format!("#{}", id));
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.done = true;
                }
                // Slide completed entries to the bottom of the list so
                // open tasks always read at the top.
                self.tasks.sort_by_key(|t| t.done);
                self.push_pulse(PulseKind::Task, format!("✓ {}", trim_inline(&title, 40)));
            }
            AgentEvent::TasksCleared => {
                self.tasks.clear();
                self.push_pulse(PulseKind::Task, "tasks cleared".to_string());
            }
            AgentEvent::CronScheduled {
                id,
                label,
                due_at_unix,
                trigger_prompt,
            } => {
                if !self.crons.iter().any(|c| c.id == id) {
                    self.crons.push(CronItem {
                        id,
                        label: label.clone(),
                        due_at_unix,
                        trigger_prompt,
                        fired: false,
                    });
                    self.next_cron_id = self.next_cron_id.max(id + 1);
                }
                self.push_pulse(PulseKind::Cron, format!("⏰ {}", trim_inline(&label, 40)));
            }
            AgentEvent::CronFired { id } => {
                let label = self
                    .crons
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| format!("#{}", id));
                if let Some(c) = self.crons.iter_mut().find(|c| c.id == id) {
                    c.fired = true;
                }
                self.push_pulse(PulseKind::Cron, format!("🔔 {}", trim_inline(&label, 40)));
            }
            AgentEvent::CronCancelled { id } => {
                self.crons.retain(|c| c.id != id);
            }
            AgentEvent::AskUser {
                id,
                question,
                options,
                multi_select,
                hint,
            } => {
                // Install the modal. A previous-still-pending Ask is
                // overwritten — the runner that emitted the previous
                // one is responsible for cleaning up its dangling
                // oneshot, but in practice the runner is single-turn
                // so only one Ask is ever in flight.
                let rows: Vec<AskRow> = options
                    .into_iter()
                    .map(|o| AskRow {
                        option: o,
                        selected: false,
                    })
                    .collect();
                let cursor = rows.iter().position(|r| r.option.recommended).unwrap_or(0);
                self.push_pulse(PulseKind::Ask, format!("? {}", trim_inline(&question, 50)));
                self.pending_ask = Some(AskState {
                    id,
                    sink: AskSink::Model,
                    filter: String::new(),
                    question,
                    hint,
                    options: rows,
                    multi_select,
                    cursor,
                });
            }
            AgentEvent::SessionResumed {
                id,
                turns_reloaded,
                bytes_skipped,
                messages,
            } => {
                // The transcript on screen becomes the reloaded one. The
                // texts are already sanitized by the runner; the assistant
                // ones go through the same box renderer as live replies.
                self.conversation.clear();
                self.scroll_offset = 0;
                for (role, text) in messages {
                    let kind = if role == "user" {
                        LineKind::User
                    } else {
                        LineKind::Assistant
                    };
                    self.push_line(ConversationLine::new(kind, text));
                }
                let mut note = format!("↺ resumed session {id} · {turns_reloaded} turns reloaded");
                if bytes_skipped > 0 {
                    note.push_str(&format!(
                        " · oldest {bytes_skipped} bytes beyond the resume window"
                    ));
                }
                self.push_line(ConversationLine::new(LineKind::System, note));
                self.session_id = id;
            }
            AgentEvent::BadgeUpdate { frame_json } => {
                // Parse as either single-frame or multi-frame
                // animation. Bad JSON silently drops — keeps the
                // previous badge alive. The animation playback
                // advances on tick (see `on_tick` below).
                if let Some(mut playback) = badge::parse_badge_payload(&frame_json) {
                    // Anchor the first-tick wait from the current
                    // animation_phase so the first frame stays
                    // visible for the full delay window.
                    playback.last_advance_tick = self.animation_phase;
                    self.badge = Some(playback);
                }
            }
        }
    }

    pub fn push_line(&mut self, line: ConversationLine) {
        if self.conversation.len() >= MAX_CONVERSATION_LINES {
            self.conversation.drain(..MAX_CONVERSATION_LINES / 4);
        }
        self.conversation.push(line);
    }

    /// Called every ~80ms by the run loop. Cheap refresh of derived
    /// state that nothing else owns (queue count, elapsed clock,
    /// animation phase for the logo shimmer and thinking ellipsis).
    /// Whether the hook `id` is enabled.
    pub fn hook_enabled(&self, id: &str) -> bool {
        self.hooks.iter().any(|h| h.id == id && h.enabled)
    }

    /// Toggle the hook under the Hooks-panel cursor.
    pub fn toggle_hook_at_cursor(&mut self) {
        if let Some(h) = self.hooks.get_mut(self.hooks_cursor) {
            h.enabled = !h.enabled;
        }
    }

    /// Keep the worlds panel current. Subscribes on first sight of a world
    /// (replay included, so late subscription loses nothing), then drains
    /// whatever arrived since the last tick.
    fn poll_worlds(&mut self) {
        let registry = crate::worlds_registry::global();
        if self.worlds_renderer.is_none() {
            if registry.total() == 0 {
                return;
            }
            let (replay, rx) = registry.subscribe();
            let mut renderer = crate::worlds_view::WorldsRenderer::new(registry.ids(), 40);
            for ev in replay {
                renderer.apply(ev);
            }
            renderer.tick();
            self.worlds_renderer = Some(renderer);
            self.worlds_rx = Some(rx);
            return;
        }
        let Some(rx) = self.worlds_rx.as_mut() else {
            return;
        };
        let renderer = self.worlds_renderer.as_mut().expect("set together");
        let mut moved = false;
        while let Ok(ev) = rx.try_recv() {
            renderer.apply(ev);
            moved = true;
        }
        if moved || !renderer.all_terminal() {
            renderer.tick();
        }
    }

    pub fn on_tick(&mut self) {
        // Wrap freely — the renderer always reads modulo a period.
        self.animation_phase = self.animation_phase.wrapping_add(1);
        self.poll_worlds();
        // Advance any running badge animation by one step if its
        // per-frame delay has elapsed.
        if let Some(playback) = self.badge.as_mut() {
            playback.advance_if_due(self.animation_phase);
        }
        // Queue count is read directly from `input_queue` every render
        // (no mutation needed here). This hook is kept for future
        // periodic updates (cost ticker, body health probes, …).
    }

    /// Number of messages currently waiting in the process-wide queue.
    /// Read at render time; never cached because other threads can mutate it.
    pub fn queue_len(&self) -> usize {
        input_queue::queue_len()
    }

    /// Snapshot of the queue for the Queue side panel.
    pub fn queue_snapshot(&self) -> Vec<String> {
        input_queue::queue_snapshot()
    }
}

// Box geometry matches `display::response_box` byte-for-byte so tui
// and the rustyline CLI render x0ne turns identically. The only
// change is the header label (`x0ne` instead of `response`) to anchor
// the frame's voice.

/// Strip `<matrix>...</matrix>` and `<action>...</action>` blocks from
/// an assistant response before it lands in the conversation. The
/// runner already extracted both (matrix → badge update, action →
/// bridge dispatch), so leaving the raw JSON in would just be visual
/// noise. Multiple blocks of either kind are all stripped.
pub(crate) fn strip_inline_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let rest = &text[i..];
        if let Some(consumed) = strip_one(rest, "<matrix>", "</matrix>")
            .or_else(|| strip_one(rest, "<action>", "</action>"))
        {
            i += consumed;
            continue;
        }
        // Push one char at a time (UTF-8 aware via char_indices on the
        // rest, fall back to the byte when we're mid-multibyte already).
        let mut chars = rest.char_indices();
        if let Some((_, c)) = chars.next() {
            out.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// If `text` starts with `open` and contains a matching `close`,
/// return the total number of bytes covered by `open..close` (so the
/// caller can skip past the whole block). Otherwise `None`.
fn strip_one(text: &str, open: &str, close: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    if !lower.starts_with(open) {
        return None;
    }
    let after = &lower[open.len()..];
    let rel_close = after.find(close)?;
    Some(open.len() + rel_close + close.len())
}

/// Wrap an x0ne response in the same rounded box the rustyline CLI
/// uses via `display::response_box` — same truecolor values, same
/// label, same shape. The width is **dynamic**: caller passes the
/// available column count, the box fills that space minus a small
/// indent. This is what gives the frame its "it takes the whole
/// window" feel — no fixed 40-char hard-coded width.
///
/// Returns one String per visual row; each row carries embedded
/// ANSI escapes that ansi-to-tui parses at render time into ratatui
/// spans.
///
/// `panel_width` is the width of the renderer's inner area (the
/// conversation column). We reserve 2 cols for the leading indent
/// and 2 for `│ ` / ` ` rails, so the body has `panel_width - 4`
/// usable chars.
pub(crate) fn x0ne_response_box_lines(text: &str, panel_width: usize) -> Vec<String> {
    use colored::Colorize;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Frame colour, shared with the CLI (`prose::ASSISTANT_FRAME`).
    const FR: (u8, u8, u8) = crate::prose::ASSISTANT_FRAME;

    // Width budget. Floor at 20 so very narrow panels still get a
    // legible box rather than negative widths or zero-length bars.
    let total = panel_width.max(20);
    let indent = 2; // leading "  " before the box
    let usable = total.saturating_sub(indent);
    // The frame is CLOSED on all four sides, matching the CLI's response
    // box. Every row spends exactly `usable` columns after the indent:
    //   top    "╭─ x0ne " (8) + fill + "╮" (1)   → fill = usable - 9
    //   body   "│ " (2) + content + " │" (2)     → content = usable - 4
    //   bottom "╰" (1) + fill + "╯" (1)          → fill = usable - 2
    let header_bar = usable.saturating_sub(9);
    let bottom_bar = usable.saturating_sub(2);
    let body_width = usable.saturating_sub(4);

    // (132,142,157) — slightly brighter for the header label
    // (104,112,128) — barely-there charcoal for every frame stroke
    let top = format!(
        "  {} {} {}{}",
        "╭─".truecolor(FR.0, FR.1, FR.2),
        "x0ne".truecolor(FR.0, FR.1, FR.2),
        "─".repeat(header_bar).truecolor(FR.0, FR.1, FR.2),
        "╮".truecolor(FR.0, FR.1, FR.2),
    );
    let bot = format!(
        "  {}{}{}",
        "╰".truecolor(FR.0, FR.1, FR.2),
        "─".repeat(bottom_bar).truecolor(FR.0, FR.1, FR.2),
        "╯".truecolor(FR.0, FR.1, FR.2),
    );

    // Strip "noise lines" before block processing:
    //   * reference-link definitions  `[ref]: https://… "title"`
    //   * footnote definitions        `[^ref]: …text`
    // These don't have a visual rendering in our box style — they're
    // metadata. Showing them raw would just be clutter.
    let trimmed = pre_strip_noise(trimmed);

    let mut out = vec![top];

    // Markdown is interpreted with a small block-aware state machine:
    //   * fenced code blocks (``` ... ```) → verbatim body, syntax
    //     highlighted by language;
    //   * pipe tables (`| h |` row + `|---|` separator + body rows)
    //     → buffered and rendered with aligned columns;
    //   * everything else → line-by-line markdown (headers, bullets,
    //     quotes, etc.).
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut table_buffer: Vec<Vec<String>> = Vec::new();
    let mut in_table = false;

    let lines: Vec<&str> = trimmed.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let raw_line = lines[i];

        // ── Table detection (outside code blocks) ──────────────────
        //
        // A table starts when the current line is a pipe-row AND the
        // next line is a separator (`|---|---|`). We buffer rows
        // until we hit a non-pipe line, then flush via render_table.
        if !in_code_block
            && !in_table
            && looks_like_pipe_row(raw_line)
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1])
        {
            in_table = true;
            table_buffer.clear();
            table_buffer.push(parse_pipe_row(raw_line));
            // Skip the separator line itself.
            i += 2;
            continue;
        }
        if in_table {
            if looks_like_pipe_row(raw_line) {
                table_buffer.push(parse_pipe_row(raw_line));
                i += 1;
                continue;
            }
            // End of table — flush.
            for tline in render_table(&table_buffer, body_width) {
                out.push(format!("  {} {}", "│".truecolor(FR.0, FR.1, FR.2), tline));
            }
            table_buffer.clear();
            in_table = false;
            // Fall through to process the current (non-pipe) line.
        }

        // ── Fenced code block toggle (``` or ```lang) ──────────────
        let lstripped = raw_line.trim_start();
        if let Some(after) = lstripped.strip_prefix("```") {
            if !in_code_block {
                in_code_block = true;
                let lang = after.trim();
                code_lang = if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                };
                let header_text = match &code_lang {
                    Some(l) => format!("┄ {} ┄", l),
                    None => "┄ code ┄".to_string(),
                };
                let header = header_text.truecolor(152, 162, 177).italic().to_string();
                out.push(format!("  {} {}", "│".truecolor(FR.0, FR.1, FR.2), header));
                i += 1;
                continue;
            } else {
                in_code_block = false;
                code_lang = None;
                // Match the body width so the closer feels like a
                // proper bookend instead of a stub.
                let closer_text = format!("┄{}", "─".repeat(body_width.saturating_sub(1).min(48)));
                let closer = closer_text.truecolor(152, 162, 177).to_string();
                out.push(format!("  {} {}", "│".truecolor(FR.0, FR.1, FR.2), closer));
                i += 1;
                continue;
            }
        }

        // ── Inside a code block: verbatim, with syntax highlight ───
        if in_code_block {
            // Preserve leading whitespace; no markdown processing.
            // Long code lines get gently wrapped at body_width too,
            // because horizontal scroll isn't a thing in ratatui
            // Paragraphs.
            let wrapped: Vec<std::borrow::Cow<'_, str>> = if raw_line.is_empty() {
                vec![std::borrow::Cow::Borrowed("")]
            } else {
                textwrap::wrap(raw_line, body_width)
            };
            for piece in wrapped {
                let body = highlight_code_line(&piece, code_lang.as_deref());
                out.push(format!("  {} {}", "│".truecolor(FR.0, FR.1, FR.2), body));
            }
            i += 1;
            continue;
        }

        // ── Outside a code block: line-by-line markdown ────────────
        let wrapped: Vec<std::borrow::Cow<'_, str>> = if raw_line.is_empty() {
            vec![std::borrow::Cow::Borrowed("")]
        } else {
            textwrap::wrap(raw_line, body_width)
        };
        for (idx, piece) in wrapped.iter().enumerate() {
            let rendered = if idx == 0 {
                format_md_line(piece)
            } else {
                format_md_inline(piece)
            };
            out.push(format!(
                "  {} {}",
                "│".truecolor(FR.0, FR.1, FR.2),
                rendered
            ));
        }
        i += 1;
    }
    // Flush any unterminated table buffered at EOF.
    if in_table && !table_buffer.is_empty() {
        for tline in render_table(&table_buffer, body_width) {
            out.push(format!("  {} {}", "│".truecolor(FR.0, FR.1, FR.2), tline));
        }
    }

    // Close every body row on the right in ONE pass. The markdown state
    // machine above emits rows from about ten different branches (headings,
    // code fences, lists, tables, quotes, plain prose); threading a right
    // border through each would guarantee that one branch eventually drifts
    // and leaves a ragged edge. Padding is by VISIBLE width because the rows
    // carry ANSI styling — byte length would misplace the border.
    let right = "│".truecolor(FR.0, FR.1, FR.2).to_string();
    let frame_end = indent + usable;
    for row in out.iter_mut().skip(1) {
        let vis = crate::prose::visible_width(row);
        // A row wider than the frame (an unbreakable token) keeps its content
        // and simply gets its border straight after — better a one-column
        // bulge than truncated output.
        let pad = frame_end.saturating_sub(vis + 1);
        row.push_str(&" ".repeat(pad));
        row.push_str(&right);
    }

    out.push(bot);
    out
}

/// Render markdown for a full source line. The whole point of this
/// pass is to feel like a serif book layout in a terminal: the
/// markers disappear, weight does the talking. Sober, elegant, no
/// loud accent colors except where they earn their place.
fn format_md_line(line: &str) -> String {
    use colored::Colorize;

    // ─ Line-start: headers. The text color stays the body's TEXT
    // (220,224,232) — same family, just bold. Quiet authority.
    if let Some(rest) = line.strip_prefix("### ") {
        return format_md_inline(rest).bold().to_string();
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return format_md_inline(rest).bold().to_string();
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return format_md_inline(rest).bold().to_string();
    }
    // ─ Line-start: blockquote — typesetter's `▎` (LEFT VERTICAL
    // ONE EIGHTH BLOCK) in dim, content in italic dim.
    if let Some(rest) = line.strip_prefix("> ") {
        return format!(
            "{} {}",
            "▎".truecolor(152, 162, 177),
            format_md_inline(rest).truecolor(170, 180, 200).italic()
        );
    }
    // ─ Line-start: horizontal rule (--- or *** or ___ on its own).
    // Replace with a soft dashed bar.
    if matches!(line.trim(), "---" | "***" | "___") {
        return "─────────────".truecolor(70, 80, 95).to_string();
    }
    // ─ Line-start: bullet — `· ` (middle dot) feels more typeset
    // than `•`. The marker itself is dimmed so the eye reads it as
    // a separator, not as a glyph competing with the text. Nested
    // bullets (any leading whitespace before `- ` / `* `) are also
    // detected and rendered, indentation preserved.
    {
        let trimmed = line.trim_start();
        let indent_len = line.len() - trimmed.len();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let indent = &line[..indent_len];
            return format!(
                "{}{} {}",
                indent,
                "·".truecolor(152, 162, 177),
                format_md_inline(rest)
            );
        }
    }
    // ─ Line-start: numbered list `1. item`, `12) item`. We keep the
    // number (it carries meaning) but dim it the way bullets are.
    if let Some(rest) = strip_ordered_marker(line) {
        let (num, body) = rest;
        return format!(
            "{} {}",
            format!("{}.", num).truecolor(152, 162, 177),
            format_md_inline(body)
        );
    }
    format_md_inline(line)
}

/// `1. foo`, `12) bar`, ` 3. baz` → returns `("1"|"12"|"3", body)`.
/// Returns `None` if the line isn't an ordered-list opener. Accepts
/// either `.` or `)` as the delimiter (both are GFM-legal).
fn strip_ordered_marker(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &trimmed[digits.len()..];
    let body = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((&trimmed[..digits.len()], body))
}

/// Inline-only markdown: **bold**, *italic*, `code`, ~~strike~~,
/// [label](url), `[^ref]` footnote refs, and inline HTML tags.
/// Order matters: longer markers first so a single `*` doesn't eat
/// inner `**`, double `~~` before any single-char fallback, HTML
/// before everything (so `<kbd>**foo**</kbd>` becomes `**foo**`
/// which then renders as bold).
fn format_md_inline(s: &str) -> String {
    use colored::Colorize;

    // Strip inline HTML — keep the inner text. Common in markdown
    // tutorials (`<kbd>Ctrl</kbd>`, `<mark>highlight</mark>`,
    // `<details>` / `<summary>` …). Rendering these would need a
    // proper HTML parser; for now we transparently unwrap them so
    // the prose underneath survives.
    let s = strip_inline_html(s);
    // Footnote refs `[^name]` → superscript-style `[¹]`-ish. The
    // user just sees a tiny marker, footnote definitions were
    // already filtered by pre_strip_noise.
    let s = render_footnote_refs(&s);
    // [label](url) → underlined label only. The URL is dropped.
    let s = render_links(&s);
    // **bold** → just the bold modifier, no color change.
    let s = replace_pair(&s, "**", |inner| inner.bold().to_string());
    // ~~strikethrough~~ → ratatui understands the SGR 9 escape via
    // ansi-to-tui (it sets Modifier::CROSSED_OUT).
    let s = replace_pair(&s, "~~", |inner| inner.strikethrough().to_string());
    // `inline code` → quiet warm grey.
    let s = replace_pair(&s, "`", |inner| inner.truecolor(140, 150, 170).to_string());
    // *italic* → just italic modifier.

    replace_pair(&s, "*", |inner| inner.italic().to_string())
}

/// Strip simple inline HTML tags, keeping their textual content.
/// Handles `<tag>...</tag>` pairs AND self-contained tags (`<br>`).
/// No fancy parsing — anything that LOOKS like a tag (a `<` followed
/// by an ASCII letter and a matching `>`) gets dropped. Content
/// between paired tags is preserved.
///
/// Iterates over chars (not bytes) so UTF-8 multi-byte sequences
/// like `é` (0xC3 0xA9) survive intact. The original byte-oriented
/// version silently mangled every accent on the screen.
fn strip_inline_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            // Find the matching `>` within chars[i+1..].
            let mut found_close: Option<usize> = None;
            for j in (i + 1)..chars.len() {
                if chars[j] == '>' {
                    found_close = Some(j);
                    break;
                }
            }
            if let Some(close) = found_close {
                let tag_inner: String = chars[i + 1..close].iter().collect();
                let first = tag_inner.chars().next().unwrap_or(' ');
                // Skip autolinks (`<https://...>`) — let render_links
                // handle them. Also skip emails `<a@b.com>`.
                let looks_like_url_or_email = tag_inner.contains("://") || tag_inner.contains('@');
                if (first.is_ascii_alphabetic() || first == '/' || first == '!')
                    && !looks_like_url_or_email
                {
                    // Drop the tag entirely.
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Render footnote refs `[^name]` as a small bracketed marker so
/// the user sees there's a note without the raw `^` showing.
/// Definition lines `[^name]: …` are stripped earlier by
/// `pre_strip_noise` so this only fires on REFERENCES in prose.
fn render_footnote_refs(s: &str) -> String {
    use colored::Colorize;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("[^") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        if let Some(close) = after.find(']') {
            let name = &after[..close];
            // Tiny dim badge.
            out.push_str(&format!("[{}]", name).truecolor(140, 150, 170).to_string());
            rest = &after[close + 1..];
            continue;
        }
        out.push_str(&rest[open..]);
        break;
    }
    out.push_str(rest);
    out
}

/// Render a `[label](url)`-shaped link as `<underlined label> <dim url>`.
/// The trailing URL in dim grey is what terminals auto-detect and
/// make ctrl-clickable (iTerm2, GNOME Terminal, kitty, wezterm,
/// ghostty, …). We can't use OSC 8 because `ansi-to-tui` strips it,
/// but inline URL is just as effective and is byte-cheap.
fn render_clickable(label: &str, url: &str) -> String {
    use colored::Colorize;
    format!("{} {}", label.underline(), url.truecolor(152, 162, 177))
}

/// Replace markdown link variants with `<underlined label> <dim url>`:
///   * `[label](url)` — classic
///   * `[label](url "title")` — with title attribute (title dropped)
///   * `![alt](url)` — image, prefixed with a tiny `🖼` glyph
///   * `<https://...>` — autolinks (URL itself becomes label)
/// Reference links `[label][ref]` + `[ref]: url` aren't resolved
/// here (would need a 2-pass); their visible `[label]` is rendered
/// underlined alone (no URL to append).
fn render_links(s: &str) -> String {
    use colored::Colorize;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // ── Autolinks `<https://…>` / `<http://…>` ─────────────────
        // Render the URL once, underlined (since the URL IS the
        // label). No extra inline URL — that would duplicate it.
        if let Some(lt) = rest.find('<') {
            let after_lt = &rest[lt + 1..];
            let next_gt = after_lt.find('>');
            let candidate = next_gt.map(|gt| &after_lt[..gt]);
            if let Some(url) = candidate {
                if url.starts_with("http://") || url.starts_with("https://") {
                    out.push_str(&rest[..lt]);
                    out.push_str(&url.underline().to_string());
                    rest = &after_lt[next_gt.unwrap() + 1..];
                    continue;
                }
            }
        }

        // ── Image `![alt](url)` — `🖼 <alt> <url>` so the image
        //    URL is just as ctrl-clickable as a plain link.
        if let Some(bang) = rest.find("![") {
            let after_bang = &rest[bang + 2..];
            if let Some(close) = after_bang.find(']') {
                if after_bang[close + 1..].as_bytes().first() == Some(&b'(') {
                    let url_start = close + 2;
                    if let Some(url_end) = after_bang[url_start..].find(')') {
                        out.push_str(&rest[..bang]);
                        let alt = &after_bang[..close];
                        let url = &after_bang[url_start..url_start + url_end];
                        out.push_str(&format!("🖼 {}", render_clickable(alt, url)));
                        rest = &after_bang[url_start + url_end + 1..];
                        continue;
                    }
                }
            }
        }

        // ── Inline link `[label](url [title?])` ────────────────────
        let Some(open) = rest.find('[') else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find(']') else {
            out.push_str(&rest[open..]);
            return out;
        };
        if after_open[close + 1..].as_bytes().first() != Some(&b'(') {
            // Could be a reference link `[label][ref]` — we don't
            // resolve refs, but we DO render the visible label
            // underlined when followed by `[…]` so the user gets
            // a hint that this was meant as a link.
            if after_open[close + 1..].as_bytes().first() == Some(&b'[') {
                let after_close = &after_open[close + 1..];
                if let Some(ref_end) = after_close[1..].find(']') {
                    let label = &after_open[..close];
                    out.push_str(&label.underline().to_string());
                    rest = &after_close[1 + ref_end + 1..];
                    continue;
                }
            }
            // Otherwise — not a link at all, emit `[label]` verbatim.
            out.push_str(&rest[open..open + 1 + close + 1]);
            rest = &after_open[close + 1..];
            continue;
        }
        // `[label](` followed by URL (possibly with " title"), close `)`.
        let url_start = close + 2;
        // Find the matching `)` ignoring `)` inside the optional
        // `"title"` block.
        let after_url = &after_open[url_start..];
        let mut depth = 1usize;
        let mut idx = 0usize;
        let mut in_title = false;
        for (i, b) in after_url.bytes().enumerate() {
            match b {
                b'"' => in_title = !in_title,
                b'(' if !in_title => depth += 1,
                b')' if !in_title => {
                    depth -= 1;
                    if depth == 0 {
                        idx = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            // Unmatched paren — emit verbatim.
            out.push_str(&rest[open..]);
            return out;
        }
        let label = &after_open[..close];
        // Extract the URL from after_url[..idx]. Strip an optional
        // " title" tail (everything from the first space onward).
        let url_raw = &after_url[..idx];
        let url = url_raw.split(char::is_whitespace).next().unwrap_or(url_raw);
        out.push_str(&render_clickable(label, url));
        rest = &after_url[idx + 1..];
    }
}

/// Replace `<marker>inner<marker>` segments using a transform. Stops
/// at the first unmatched opener (leaves it as-is) so a stray `**`
/// in the prose doesn't eat the rest of the line.
fn replace_pair(s: &str, marker: &str, transform: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find(marker) {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + marker.len()..];
        match after_open.find(marker) {
            Some(close) => {
                let inner = &after_open[..close];
                if inner.is_empty() {
                    // Empty pair (e.g. `**`) — emit as-is so we don't
                    // generate styled-empty noise.
                    out.push_str(marker);
                    out.push_str(marker);
                    rest = &after_open[close + marker.len()..];
                } else {
                    out.push_str(&transform(inner));
                    rest = &after_open[close + marker.len()..];
                }
            }
            None => {
                // Unmatched opener — emit the rest verbatim and stop.
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

// ── Pre-processing noise stripping ──────────────────────────────────────────

/// Drop markdown "infrastructure" lines that have no visual rendering
/// in the box style:
///   * reference-link definitions  `[ref]: https://… "title"`
///   * footnote definitions        `[^name]: …text`
/// Both are metadata that the rendering layer either resolves
/// elsewhere (visible labels) or doesn't support — leaving them in
/// would just print weird `[doc]: …` lines to the user.
fn pre_strip_noise(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let t = line.trim_start();
            // `[anything]:` followed by something → reference definition.
            // Covers both link refs (`[ref]: https://...`) and footnotes
            // (`[^1]: text`).
            if let Some(end) = t.find("]:") {
                let head = &t[..end];
                if head.starts_with('[') && head.len() > 1 {
                    return false;
                }
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Tables ──────────────────────────────────────────────────────────────────

/// True if `line` looks like a pipe-table row: starts with `|` and
/// contains at least one more `|`. Strictest possible to avoid
/// catching arbitrary prose with pipes in the middle.
fn looks_like_pipe_row(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    // Count `|`s — need ≥ 2 (the leading one + at least one cell sep).
    trimmed.matches('|').count() >= 2
}

/// True if `line` is the header/body separator: `|---|---|`,
/// `|:---:|:---|`, etc. (cells contain only `-`, `:`, and spaces).
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return false;
    }
    // Strip leading/trailing `|`, split, check each cell.
    let inner = &trimmed[1..trimmed.len() - 1];
    inner.split('|').all(|cell| {
        let c = cell.trim();
        !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':') && c.contains('-')
    })
}

/// Split `| a | b | c |` into `["a", "b", "c"]`. Trims each cell.
fn parse_pipe_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    // Strip the boundary `|`s so we don't get empty leading/trailing
    // cells.
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Render a parsed pipe table as a sequence of pre-styled lines.
/// First row = header (bold), separated from body by a dim `─`
/// underline. Cells are pipe-separated with a `│` in the same
/// charcoal grey as the response box frame, padded to the column's
/// max display width. If the total width exceeds `body_width`,
/// columns are uniformly truncated with `…`.
fn render_table(rows: &[Vec<String>], body_width: usize) -> Vec<String> {
    use colored::Colorize;
    use unicode_width::UnicodeWidthStr;

    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return Vec::new();
    }
    // Compute max width per column (display-width, not bytes) on the cell
    // text WITHOUT its markdown markers — `**ID**` takes two columns, not
    // six. Owner screenshot 2026-08-17 12:35: a table of `**Champ**` /
    // `` `valeur` `` cells printed the asterisks and backticks verbatim.
    let plain: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| strip_md_inline(c)).collect())
        .collect();
    let mut widths = vec![0usize; ncols];
    for row in &plain {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    // Try to fit inside body_width. Each column has a 1-char left
    // pad, a 1-char right pad, and a `│` separator (except last).
    // Total = sum(widths) + 3*(ncols-1) + 2.
    let total = |w: &[usize]| w.iter().sum::<usize>() + 3 * (ncols.saturating_sub(1)) + 2;
    while total(&widths) > body_width.max(20) {
        // Trim the widest column by 1, repeat. Floor at 3 so we
        // can still fit a one-char + `…`.
        let (max_idx, _) = widths.iter().enumerate().max_by_key(|(_, &w)| w).unwrap();
        if widths[max_idx] <= 3 {
            break;
        }
        widths[max_idx] -= 1;
    }

    let sep = "│".truecolor(104, 112, 128).to_string();
    let mut out = Vec::with_capacity(rows.len() + 1);

    // ─ Helper: render one row with header_style flag ─
    let render_row = |row: &[String], plain_row: &[String], header: bool| -> String {
        let mut line = String::new();
        for c in 0..ncols {
            let cell = row.get(c).cloned().unwrap_or_default();
            let cell_plain = plain_row.get(c).cloned().unwrap_or_default();
            // A cell that fits is rendered with its inline markdown (bold,
            // code, links…) and padded by VISIBLE width; one that must be
            // truncated is cut on its plain text — a marker pair split by
            // the `…` would leak half an escape into the next column.
            let fits = UnicodeWidthStr::width(cell_plain.as_str()) <= widths[c];
            let padded = if fits {
                let rendered = format_md_inline(&cell);
                let vis = crate::prose::visible_width(&rendered);
                format!("{}{}", rendered, " ".repeat(widths[c].saturating_sub(vis)))
            } else {
                pad_to_width(&truncate_for_width(&cell_plain, widths[c]), widths[c])
            };
            let styled = if header {
                padded.bold().to_string()
            } else {
                padded
            };
            if c == 0 {
                line.push(' ');
            }
            line.push_str(&styled);
            if c + 1 < ncols {
                line.push(' ');
                line.push_str(&sep);
                line.push(' ');
            }
        }
        line
    };

    out.push(render_row(&rows[0], &plain[0], true));
    // Underline = `─` matching the row width.
    let mut underline = String::new();
    for c in 0..ncols {
        if c == 0 {
            underline.push(' ');
        }
        underline.push_str(&"─".repeat(widths[c]));
        if c + 1 < ncols {
            underline.push_str("─┼─");
        }
    }
    out.push(underline.truecolor(104, 112, 128).to_string());

    for (row, plain_row) in rows[1..].iter().zip(&plain[1..]) {
        out.push(render_row(row, plain_row, false));
    }
    out
}

/// The cell text with its inline markdown markers removed — what the reader
/// sees, for measuring. Same pairs as `format_md_inline`, same order.
fn strip_md_inline(s: &str) -> String {
    let s = strip_inline_html(s);
    let s = replace_pair(&s, "**", |inner| inner.to_string());
    let s = replace_pair(&s, "~~", |inner| inner.to_string());
    let s = replace_pair(&s, "`", |inner| inner.to_string());
    replace_pair(&s, "*", |inner| inner.to_string())
}

/// Truncate `s` to fit in `width` display columns, appending `…` if
/// truncation happens.
fn truncate_for_width(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width.saturating_sub(1);
    let mut acc = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > budget {
            break;
        }
        out.push(c);
        acc += cw;
    }
    out.push('…');
    out
}

/// Right-pad `s` with spaces to `width` display columns.
fn pad_to_width(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{}{}", s, " ".repeat(width - w))
}

// ── Code syntax highlighting (very light) ───────────────────────────────────

/// Highlight one line of a fenced code block based on the language
/// declared after the opening ```. Hand-rolled, no syntect — keeps
/// the binary slim and avoids regex blowups. The palette is sober:
/// keywords stay BODY color (just bolded), strings in a soft teal,
/// comments in faint italic, numbers untouched. Anything we don't
/// recognise falls back to the previous "everything dimmed body"
/// rendering so unknown languages still look like code.
fn highlight_code_line(line: &str, lang: Option<&str>) -> String {
    use colored::Colorize;
    let base = (170, 180, 200);
    let Some(lang) = lang else {
        return line.truecolor(base.0, base.1, base.2).to_string();
    };
    let kind = match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => LangKind::Rust,
        "python" | "py" => LangKind::Python,
        "js" | "javascript" | "ts" | "typescript" => LangKind::Js,
        "sh" | "bash" | "shell" | "zsh" => LangKind::Shell,
        _ => return line.truecolor(base.0, base.1, base.2).to_string(),
    };

    // Comment handling first — once we hit a comment marker the rest
    // of the line is dim italic.
    let (head, comment) = split_comment(line, kind);
    let mut out = String::new();
    out.push_str(&highlight_code_segment(head, kind));
    if let Some(c) = comment {
        out.push_str(&c.truecolor(152, 162, 177).italic().to_string());
    }
    out
}

#[derive(Clone, Copy)]
enum LangKind {
    Rust,
    Python,
    Js,
    Shell,
}

fn split_comment(line: &str, lang: LangKind) -> (&str, Option<&str>) {
    let marker = match lang {
        LangKind::Rust | LangKind::Js => "//",
        LangKind::Python | LangKind::Shell => "#",
    };
    if let Some(pos) = find_outside_strings(line, marker) {
        return (&line[..pos], Some(&line[pos..]));
    }
    (line, None)
}

/// Find `needle` in `hay` but ignore positions inside `"..."` or
/// `'...'` literals. Tolerates escapes (`\"`) lightly.
fn find_outside_strings(hay: &str, needle: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut i = 0;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(quote) = in_str {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == quote {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            in_str = Some(c);
            i += 1;
            continue;
        }
        if hay[i..].starts_with(needle) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Highlight a code segment (no comment trailing). Strings + keywords
/// only — keep the rest body-coloured. Char-iteration so any
/// UTF-8 string literal (`"héllo"`, `"日本"`) survives intact.
fn highlight_code_segment(s: &str, lang: LangKind) -> String {
    use colored::Colorize;
    let base = (170, 180, 200);
    let string_color = (94, 234, 212); // teal — the frame accent
    let kw_set: &[&str] = match lang {
        LangKind::Rust => &[
            "fn", "pub", "let", "mut", "const", "static", "enum", "struct", "impl", "mod", "use",
            "match", "if", "else", "for", "while", "loop", "return", "Some", "None", "Ok", "Err",
            "Self", "self", "as", "in", "where", "trait", "type", "ref", "move", "async", "await",
            "true", "false",
        ],
        LangKind::Python => &[
            "def", "class", "import", "from", "if", "elif", "else", "for", "while", "return",
            "yield", "lambda", "True", "False", "None", "and", "or", "not", "in", "is", "with",
            "as", "try", "except", "finally", "raise", "pass", "break", "continue", "global",
            "nonlocal", "async", "await",
        ],
        LangKind::Js => &[
            "function",
            "const",
            "let",
            "var",
            "class",
            "import",
            "export",
            "from",
            "if",
            "else",
            "for",
            "while",
            "return",
            "async",
            "await",
            "null",
            "undefined",
            "true",
            "false",
            "new",
            "this",
            "super",
            "extends",
            "typeof",
            "instanceof",
            "in",
            "of",
            "try",
            "catch",
            "finally",
            "throw",
            "break",
            "continue",
            "switch",
            "case",
            "default",
        ],
        LangKind::Shell => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac",
            "function", "return", "echo", "export", "source", "local", "readonly", "set", "unset",
            "true", "false",
        ],
    };

    let mut out = String::with_capacity(s.len() + 16);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut buffer = String::new();
    let flush_buffer = |buf: &mut String, out: &mut String, kw_set: &[&str], base: (u8, u8, u8)| {
        if buf.is_empty() {
            return;
        }
        if kw_set.contains(&buf.as_str()) {
            out.push_str(&buf.as_str().bold().to_string());
        } else {
            out.push_str(&buf.as_str().truecolor(base.0, base.1, base.2).to_string());
        }
        buf.clear();
    };

    while i < chars.len() {
        let c = chars[i];
        // String literal
        if c == '"' || c == '\'' {
            flush_buffer(&mut buffer, &mut out, kw_set, base);
            let quote = c;
            let mut lit = String::new();
            lit.push(quote);
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    lit.push(chars[i]);
                    lit.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                lit.push(chars[i]);
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(
                &lit.truecolor(string_color.0, string_color.1, string_color.2)
                    .to_string(),
            );
            continue;
        }
        // Word char
        if c.is_ascii_alphanumeric() || c == '_' {
            buffer.push(c);
            i += 1;
            continue;
        }
        // Non-word: flush + emit literal char (base-coloured).
        flush_buffer(&mut buffer, &mut out, kw_set, base);
        out.push_str(&c.to_string().truecolor(base.0, base.1, base.2).to_string());
        i += 1;
    }
    flush_buffer(&mut buffer, &mut out, kw_set, base);
    out
}

/// Decode a Pipeline ToolEnd result into the `PipelineRun` shape the
/// Pipeline panel renders. Runtime ships:
///   `result.output.steps[]` with `{action_type, success, duration_ms,
///   output, error}`
/// Each step's one-line summary comes from a small set of common
/// fields (output.size_bytes, output.sha256, error.message, …) so
/// the panel reads as a glance.
pub(crate) fn parse_pipeline_result(
    action: &serde_json::Value,
    result: &serde_json::Value,
    total_duration_ms: u64,
    ok: bool,
) -> Option<PipelineRun> {
    let steps_v = result
        .get("output")
        .and_then(|o| o.get("steps"))
        .and_then(|s| s.as_array())?;
    let action_steps = action.get("steps").and_then(|s| s.as_array());
    let total_steps = action_steps.map(|a| a.len()).unwrap_or(steps_v.len());
    let mut completed_steps = 0usize;
    let mut failed = false;
    let mut steps: Vec<PipelineStep> = Vec::with_capacity(steps_v.len());
    for (idx, step_v) in steps_v.iter().enumerate() {
        let action_type = step_v
            .get("action_type")
            .and_then(|v| v.as_str())
            .or_else(|| step_v.get("type").and_then(|v| v.as_str()))
            .unwrap_or("?")
            .to_string();
        let step_ok = step_v
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let duration_ms = step_v
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let status = if !step_ok {
            failed = true;
            "failed".to_string()
        } else {
            completed_steps += 1;
            "ok".to_string()
        };
        let summary = step_summary(step_v);
        steps.push(PipelineStep {
            idx,
            action_type,
            status,
            duration_ms,
            summary,
        });
    }
    Some(PipelineRun {
        started_at: Instant::now() - std::time::Duration::from_millis(total_duration_ms),
        completed: true,
        total_steps,
        completed_steps,
        failed: failed || !ok,
        total_duration_ms,
        steps,
    })
}

fn step_summary(step: &serde_json::Value) -> String {
    if let Some(err) = step
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
    {
        return trim_inline(&format!("err: {}", err), 60);
    }
    if let Some(out) = step.get("output") {
        if let Some(sz) = out.get("size_bytes").and_then(|v| v.as_u64()) {
            return format!("{}B", sz);
        }
        if let Some(hash) = out.get("sha256").and_then(|v| v.as_str()) {
            return format!("sha256 {}…", hash.chars().take(8).collect::<String>());
        }
        if let Some(s) = out.get("stdout").and_then(|v| v.as_str()) {
            return trim_inline(s, 50);
        }
        if let Some(matches) = out.get("matches").and_then(|v| v.as_array()) {
            return format!(
                "{} match{}",
                matches.len(),
                if matches.len() == 1 { "" } else { "es" }
            );
        }
        if let Some(content) = out.get("content").and_then(|v| v.as_str()) {
            return format!("{} chars", content.chars().count());
        }
    }
    "ok".to_string()
}

/// Pull every URL the agent touched in a given ToolEnd into a flat
/// list of [`SourceEntry`]s. Three shapes handled:
///   * `FetchUrl` / `BrowseWeb` → one entry (from `action.url`).
///   * `WebSearch` → N entries from `result.output.results[]`,
///     each carrying its own title + snippet + URL.
/// Anything else returns an empty list — kept side-effect-free so
/// callers can splat into `state.sources` without bookkeeping.
pub(crate) fn extract_sources(
    name: &str,
    action: &serde_json::Value,
    result: &serde_json::Value,
    ok: bool,
) -> Vec<SourceEntry> {
    let now = Instant::now();
    let mut out = Vec::new();
    match name {
        "FetchUrl" | "BrowseWeb" => {
            if let Some(url) = action.get("url").and_then(|v| v.as_str()) {
                // Page title is sometimes returned by BrowseWeb in
                // result.output.title; fall back to a host-derived
                // synthetic name when absent so the panel stays
                // readable.
                let title = result
                    .get("output")
                    .and_then(|o| o.get("title"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| short_title_for(url));
                out.push(SourceEntry {
                    url: url.to_string(),
                    title,
                    origin: name.to_string(),
                    ok,
                    at: now,
                    query: None,
                    cite: result
                        .get("output")
                        .and_then(|o| o.get("source"))
                        .and_then(|v| v.as_str())
                        .map(String::from),
                });
            }
        }
        "WebSearch" => {
            let query = action
                .get("query")
                .and_then(|v| v.as_str())
                .map(String::from);
            // The runtime wraps the payload in `ActionOutput::Generic` (`output.data`).
            let output = result.get("output");
            if let Some(arr) = output
                .and_then(|o| o.get("results").or_else(|| o.get("data")?.get("results")))
                .and_then(|r| r.as_array())
            {
                for item in arr {
                    let url = item
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if url.is_empty() {
                        continue;
                    }
                    let title = item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| short_title_for(&url));
                    out.push(SourceEntry {
                        url,
                        title,
                        origin: "WebSearch".into(),
                        ok,
                        at: now,
                        query: query.clone(),
                        cite: item
                            .get("source")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    });
                }
            }
        }
        _ => {}
    }
    out
}

/// Pretty-name fallback for a URL: keep the host + a hint of the
/// path so the panel stays readable when no real title is available.
fn short_title_for(url: &str) -> String {
    // Strip scheme.
    let no_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    // Take everything up to the first slash for the host.
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    let path: String = no_scheme.chars().skip(host.len()).take(40).collect();
    if path.is_empty() {
        host.to_string()
    } else {
        format!("{}{}", host, path)
    }
}

/// Scan a user message for image references and return the resolved
/// paths (in original order, deduplicated). Recognised forms:
///   * `@file.png` — bare token in the message (anywhere)
///   * `@/abs/path/img.jpg` — absolute path with extension
///   * `@~/Desktop/swappy-….png` — tilde-expanded
/// Recognised extensions: png · jpg · jpeg · webp · gif.
///
/// Returns paths only (no validation that the file exists — the
/// runner does that and emits a clear error if the file is missing,
/// so the user sees the bug immediately).
pub fn extract_image_refs(text: &str) -> Vec<String> {
    let exts = ["png", "jpg", "jpeg", "webp", "gif"];
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let t = token.trim_end_matches(|c: char| ",.;:!?)]}>\"".contains(c));
        let Some(rest) = t.strip_prefix('@') else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        // Tilde expansion — `@~/x.png` → `<home>/x.png`.
        let expanded = if let Some(after) = rest.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(after).to_string_lossy().into_owned()
            } else {
                rest.to_string()
            }
        } else {
            rest.to_string()
        };
        let lower = expanded.to_ascii_lowercase();
        if exts.iter().any(|ext| lower.ends_with(&format!(".{}", ext))) && !out.contains(&expanded)
        {
            out.push(expanded);
        }
    }
    out
}

/// UTF-8 safe right-trim helper for pulse / panel labels — keeps the
/// rendered width predictable without panicking on multi-byte chars.
pub(crate) fn trim_inline(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.into()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn short_session_id() -> String {
    // 4-char base36 from the timestamp — enough to disambiguate in a
    // header line, not a real session id (the orchestrator's
    // session_registry owns the real one). Replaced in stage 2.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut buf = String::with_capacity(4);
    let mut n = ts % (36u64.pow(4));
    for _ in 0..4 {
        let d = (n % 36) as u8;
        n /= 36;
        let c = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
        buf.insert(0, c as char);
    }
    buf
}

/// The badge that had no producer.
///
/// # The defect, measured 2026-08-16
///
/// `EnclaveStatus::from_verdict` existed, was documented, and was read by
/// `render.rs`. Nothing called it: `AppState::new` set `Standard` and no line
/// in the program ever set anything else — the doc comment said so out loud
/// ("Nothing sets `Attested` today"). That is this repo's recurring defect, a
/// flag that is written, read, and connected to no producer
/// (docs/invariants.md).
#[cfg(test)]
mod enclave_badge {
    use super::*;
    use crate::cc_runner::{CcOutcome, ProofBundle};
    use serde_json::json;
    use x0ne_cloud::cc_plan::CcStep;
    use x0ne_confidential::model::Verdict;

    /// Same synthetic TD Quote v4 layout as `x0ne_confidential::tdx_quote`'s
    /// own tests (header 48B, body 584B, mr_td @136, report_data @520). A
    /// third copy of eight lines beats exporting a quote FORGER from the
    /// library that verifies quotes.
    fn synthetic_quote_bytes(nonce: &[u8; 64]) -> Vec<u8> {
        let mut q = vec![0u8; 48 + 584 + 8];
        q[0] = 4;
        q[4..8].copy_from_slice(&0x81u32.to_le_bytes());
        q[48 + 136..48 + 184].copy_from_slice(&[0xAB; 48]);
        q[48 + 520..48 + 584].copy_from_slice(nonce);
        q
    }

    fn proven_cpu() -> ProofBundle {
        let nonce = [3u8; 64];
        let quote = synthetic_quote_bytes(&nonce);
        let (claims, verdict) = x0ne_confidential::tdx_quote::verify(&quote, &nonce);
        ProofBundle::from_verdict(
            &verdict,
            claims.as_ref(),
            x0ne_confidential::tdx_quote::hex(&nonce),
        )
        .expect("the synthetic quote verifies")
    }

    /// The config an `x0ne cc`-installed x0ne runs with inside the enclave:
    /// vLLM on loopback, serving the model the report names.
    fn enclave_config(model: &str) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.llm.provider = "openai-compat".into();
        c.llm.openai_compat.base_url = "http://127.0.0.1:8000/v1".into();
        c.llm.openai_compat.model = model.into();
        c
    }

    fn report(state: &str, model: &str, at: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
        json!({
            "verdict": "proven",
            "state": state,
            "model": model,
            "measured_at": at.to_rfc3339(),
            "limits": [],
        })
    }

    const MODEL: &str = "Qwen/Qwen2.5-7B-Instruct";

    /// SECURITY: green means a proven outcome AND a session inside it.
    ///
    /// A failure means either the indicator is dead again — red inside a
    /// proven enclave, which trains an operator to ignore it — or it is green
    /// somewhere it should not be, which is worse: the badge is what someone
    /// staking model weights on rented hardware reads instead of asking.
    #[test]
    fn the_enclave_badge_is_green_only_after_a_proven_outcome_and_enter() {
        // The producer half: a proven outcome with the GPU in CC mode.
        let outcome = CcOutcome::Proven {
            proof: proven_cpu(),
            gpu: None,
            encryption: None,
        };
        assert_eq!(
            EnclaveStatus::from_outcome(&outcome),
            EnclaveStatus::CpuOnly,
            "a proven CPU with no GPU measurement is amber, never green"
        );

        // The session half: a fresh report and a session wired to the enclave.
        let now = chrono::Utc::now();
        let green = EnclaveStatus::for_session(
            Some(&report("attested", MODEL, now)),
            now,
            &enclave_config(MODEL),
        );
        assert_eq!(green, EnclaveStatus::Attested);
        assert_eq!(green.badge().1, "confidential");
    }

    /// SECURITY: a run that proved nothing lights nothing.
    ///
    /// A failure means `ATTESTATION REFUSED` on screen next to a green badge
    /// — the two disagreeing, with the reassuring one winning because it is
    /// the one still visible after the report scrolls away.
    #[test]
    fn a_refused_or_failed_outcome_never_lights_the_badge() {
        let refused = CcOutcome::Refused {
            reason: "the quote did not echo the nonce".into(),
        };
        assert_eq!(
            EnclaveStatus::from_outcome(&refused),
            EnclaveStatus::Standard
        );

        // A failure that happened AFTER a real CPU proof is the interesting
        // one: the proof is genuine and the report prints it, and the badge
        // still must not claim the session runs inside a working enclave.
        let failed = CcOutcome::Failed {
            step: CcStep::AttestGpu,
            error: "the GPU answered OFF".into(),
            proof: Some(proven_cpu()),
            gpu: None,
            encryption: None,
        };
        assert_eq!(
            EnclaveStatus::from_outcome(&failed),
            EnclaveStatus::Standard
        );

        // And a rejected verdict cannot reach `from_verdict` as anything else.
        let rejected = Verdict::Rejected {
            reason: "no".into(),
            verifier: "tdx-structural".into(),
        };
        assert_eq!(
            EnclaveStatus::from_verdict(&rejected, true),
            EnclaveStatus::Standard,
            "a claimed GPU mode cannot upgrade a rejection"
        );
    }

    /// SECURITY: everywhere that is not a session inside the enclave is red.
    ///
    /// The report lives on the operator's LAPTOP — `x0ne cc` writes it there —
    /// so "a proven report exists on this disk" is true for every ordinary
    /// local session for an hour after any successful run. A failure of this
    /// test means those sessions read `confidential` while the conversation
    /// goes to a hosted API, which is the exact inversion of what the badge is
    /// for.
    #[test]
    fn outside_x0ne_cc_the_badge_is_standard() {
        let now = chrono::Utc::now();
        let proven = report("attested", MODEL, now);

        // No report at all.
        assert_eq!(
            EnclaveStatus::for_session(None, now, &enclave_config(MODEL)),
            EnclaveStatus::Standard
        );

        // A proven report, but this session talks to Anthropic.
        let hosted = crate::config::Config::default();
        assert_eq!(
            EnclaveStatus::for_session(Some(&proven), now, &hosted),
            EnclaveStatus::Standard,
            "a hosted API is not protected by a VM somewhere else"
        );

        // A proven report, loopback — but some OTHER local server.
        let mut other = enclave_config("llama3.2");
        other.llm.openai_compat.base_url = "http://127.0.0.1:11434/v1".into();
        assert_eq!(
            EnclaveStatus::for_session(Some(&proven), now, &other),
            EnclaveStatus::Standard,
            "a local ollama is not an enclave"
        );

        // A proven report and the right model, but reached over the network.
        let mut remote = enclave_config(MODEL);
        remote.llm.openai_compat.base_url = "http://10.0.0.5:8000/v1".into();
        assert_eq!(
            EnclaveStatus::for_session(Some(&proven), now, &remote),
            EnclaveStatus::Standard,
            "a conversation that leaves the machine is not protected by the machine"
        );

        // SECURITY: and the host that only LOOKS like loopback.
        let mut lookalike = enclave_config(MODEL);
        lookalike.llm.openai_compat.base_url = "http://127.0.0.1.evil.example/v1".into();
        assert_eq!(
            EnclaveStatus::for_session(Some(&proven), now, &lookalike),
            EnclaveStatus::Standard,
            "the host is parsed, never string-matched"
        );

        // A stale report, everything else right.
        let old = now - chrono::Duration::hours(48);
        assert_eq!(
            EnclaveStatus::for_session(
                Some(&report("attested", MODEL, old)),
                now,
                &enclave_config(MODEL)
            ),
            EnclaveStatus::Standard,
            "an old proof does not describe the machine serving now"
        );
    }

    /// The wire vocabulary is closed, and unknown text is not a state.
    ///
    /// A failure means a hand-edited `last-report.json` can name a state this
    /// build does not know and have it treated as something — the direction
    /// that matters being "as something green".
    #[test]
    fn an_unknown_wire_state_is_not_a_state() {
        assert_eq!(
            EnclaveStatus::from_wire("attested"),
            Some(EnclaveStatus::Attested)
        );
        assert_eq!(
            EnclaveStatus::from_wire("cpu_only"),
            Some(EnclaveStatus::CpuOnly)
        );
        assert_eq!(
            EnclaveStatus::from_wire("standard"),
            Some(EnclaveStatus::Standard)
        );
        for bogus in [
            "",
            "Attested",
            "attested ",
            "attested-plus",
            "true",
            "confidential",
        ] {
            assert_eq!(EnclaveStatus::from_wire(bogus), None, "{bogus:?}");
        }
        // And the round trip holds for the three that exist.
        for s in [
            EnclaveStatus::Standard,
            EnclaveStatus::CpuOnly,
            EnclaveStatus::Attested,
        ] {
            assert_eq!(EnclaveStatus::from_wire(s.wire_state()), Some(s));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `colored` downgrades `.truecolor()` to the 256-colour cube unless
    /// COLORTERM says truecolor, and drops every escape off a TTY.
    pub(crate) fn force_truecolor() {
        std::env::set_var("COLORTERM", "truecolor");
        colored::control::set_override(true);
    }

    /// The input box carries the sent-message style, so a line does not
    /// change appearance at the moment it is sent — and carries no underline,
    /// which is what `TextArea::default()` would have given it.
    #[test]
    fn the_input_reads_like_the_message_it_becomes() {
        let input = styled_input();
        let style = input.style();
        assert_eq!(style.fg, Some(crate::tui::render::VIOLET));
        assert!(
            style.add_modifier.contains(Modifier::ITALIC),
            "typing and sent text must share the italic"
        );
        assert_eq!(
            input.cursor_line_style(),
            Style::default(),
            "the cursor line must carry no highlight — default underlines it"
        );
    }

    /// Every site that rebuilds the box must go through the one constructor.
    /// Three sites build it (initial, history replace, post-send reset), and
    /// a bare `TextArea::default()` in any of them means the style vanishes
    /// on send or on arrow-up — visible to the operator, invisible to a test
    /// that only checks the constructor.
    #[test]
    fn no_site_builds_a_bare_textarea() {
        // Assembled at runtime so this test's own source does not contain the
        // literal it searches for — `include_str!` reads the file this test
        // lives in, and a spelled-out needle would match the search line.
        let needle = format!("TextArea::{}()", "default");
        for (file, src) in [
            ("app.rs", include_str!("app.rs")),
            ("events.rs", include_str!("events.rs")),
        ] {
            let bare = src
                .lines()
                .filter(|l| l.contains(&needle))
                .filter(|l| !l.trim_start().starts_with("//"))
                .count();
            let expected = usize::from(file == "app.rs"); // the constructor itself
            assert_eq!(
                bare, expected,
                "{file} builds a TextArea outside `styled_input()`"
            );
        }
    }

    /// The strip draws from `all()`, the keyboard walks `cycle_next()`. If
    /// the two lists disagree, a panel exists that the tab bar cannot name —
    /// which is exactly the bug this test pins: Data and Matrix were
    /// reachable by Tab but invisible in the strip.
    #[test]
    fn the_tab_strip_and_the_cycle_agree() {
        let all = SidePanel::all();
        let mut walked = vec![SidePanel::Conversation];
        let mut p = SidePanel::Conversation.cycle_next();
        while p != SidePanel::Conversation {
            walked.push(p);
            p = p.cycle_next();
        }
        assert_eq!(
            walked.len(),
            all.len(),
            "cycle and strip carry different panel counts"
        );
        for panel in &walked {
            assert!(
                all.contains(panel),
                "{panel:?} is reachable by Tab but absent from the strip"
            );
        }
    }

    #[test]
    fn side_panel_cycles_in_order() {
        let mut p = SidePanel::Conversation;
        for expected in [
            SidePanel::Queue,
            SidePanel::Tasks,
            SidePanel::Cron,
            SidePanel::Pipeline,
            SidePanel::Sources,
            SidePanel::Data,
            SidePanel::Matrix,
            SidePanel::Shell,
            SidePanel::Hooks,
            SidePanel::Infra,
            SidePanel::Pulse,
            SidePanel::Snapshots,
            SidePanel::Handles,
            SidePanel::Worlds,
            SidePanel::Goal,
            SidePanel::Conversation, // wraps
        ] {
            p = p.cycle_next();
            assert_eq!(p, expected);
        }
    }

    #[test]
    fn side_panel_cycles_backwards() {
        let mut p = SidePanel::Conversation;
        for expected in [
            SidePanel::Goal,
            SidePanel::Worlds,
            SidePanel::Handles,
            SidePanel::Snapshots,
            SidePanel::Pulse,
            SidePanel::Infra,
            SidePanel::Hooks,
            SidePanel::Shell,
            SidePanel::Matrix,
            SidePanel::Data,
            SidePanel::Sources,
            SidePanel::Pipeline,
            SidePanel::Cron,
            SidePanel::Tasks,
            SidePanel::Queue,
            SidePanel::Conversation,
        ] {
            p = p.cycle_prev();
            assert_eq!(p, expected);
        }
    }

    // ── Multimodal · @file.png parsing ──────────────────────────────

    #[test]
    fn extract_image_refs_finds_a_single_png() {
        let refs = super::extract_image_refs("look at @screenshot.png");
        assert_eq!(refs, vec!["screenshot.png"]);
    }

    #[test]
    fn extract_image_refs_recognises_all_common_extensions() {
        let refs = super::extract_image_refs("@a.png @b.jpg @c.JPEG @d.webp @e.gif");
        // 5 refs, original order, mixed-case extension OK.
        assert_eq!(refs.len(), 5);
        assert!(refs.iter().any(|p| p.ends_with(".png")));
        assert!(refs.iter().any(|p| p.ends_with(".gif")));
        // Case is preserved on the input path but matching is case-insensitive.
        assert!(refs.iter().any(|p| p == "c.JPEG"));
    }

    #[test]
    fn extract_image_refs_ignores_non_image_extensions() {
        let refs = super::extract_image_refs("read @notes.md and @data.json");
        assert!(
            refs.is_empty(),
            "non-image @file references must NOT be picked up by the image helper"
        );
    }

    #[test]
    fn extract_image_refs_strips_trailing_punctuation() {
        // Users type things like "explain @x.png." or "compare @a.png, @b.png."
        // The helper must strip trailing `.,;:!?` etc so the path resolves cleanly.
        let refs = super::extract_image_refs("explain @a.png, then @b.png.");
        assert_eq!(refs, vec!["a.png", "b.png"]);
    }

    #[test]
    fn extract_image_refs_deduplicates() {
        let refs = super::extract_image_refs("@same.png and again @same.png");
        assert_eq!(refs, vec!["same.png"]);
    }

    #[test]
    fn extract_image_refs_handles_absolute_paths() {
        let refs = super::extract_image_refs("look at @/tmp/swappy-20260528_010237.png");
        assert_eq!(refs, vec!["/tmp/swappy-20260528_010237.png"]);
    }

    #[test]
    fn extract_image_refs_expands_tilde() {
        let refs = super::extract_image_refs("@~/Desktop/foo.png");
        // Tilde must resolve to $HOME (unless $HOME is missing, in
        // which case we keep the literal).
        if let Some(home) = dirs::home_dir() {
            let expected = home.join("Desktop/foo.png").to_string_lossy().into_owned();
            assert_eq!(refs, vec![expected]);
        }
    }

    #[test]
    fn extract_image_refs_returns_empty_when_no_refs() {
        let refs = super::extract_image_refs("just a normal message with no attachments");
        assert!(refs.is_empty());
    }

    #[test]
    fn extract_sources_pulls_one_url_from_fetchurl() {
        let action = serde_json::json!({"type": "FetchUrl", "url": "https://example.com/x"});
        let result = serde_json::json!({"success": true, "output": {}});
        let srcs = super::extract_sources("FetchUrl", &action, &result, true);
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].url, "https://example.com/x");
        assert_eq!(srcs[0].origin, "FetchUrl");
        assert!(srcs[0].ok);
        assert!(srcs[0].query.is_none());
    }

    #[test]
    fn extract_sources_uses_browseweb_title_when_present() {
        let action = serde_json::json!({"type": "BrowseWeb", "url": "https://x.example/q"});
        let result = serde_json::json!({
            "success": true,
            "output": {"title": "Welcome to Example"},
        });
        let srcs = super::extract_sources("BrowseWeb", &action, &result, true);
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].title, "Welcome to Example");
    }

    #[test]
    fn extract_sources_fans_out_websearch_results() {
        let action = serde_json::json!({"type": "WebSearch", "query": "rust async"});
        let result = serde_json::json!({
            "success": true,
            "output": {
                "kind": "web_search_result",
                "results": [
                    {"title": "Async book", "url": "https://rust-lang.github.io/async-book/", "snippet": "..."},
                    {"title": "Tokio docs",  "url": "https://tokio.rs/",                       "snippet": "..."},
                ]
            }
        });
        let srcs = super::extract_sources("WebSearch", &action, &result, true);
        assert_eq!(srcs.len(), 2);
        assert!(srcs
            .iter()
            .all(|s| s.query.as_deref() == Some("rust async")));
        assert_eq!(srcs[0].origin, "WebSearch");
        assert_eq!(srcs[1].title, "Tokio docs");
    }

    /// The runtime serialises WebSearch as `ActionOutput::Generic`: results sit
    /// under `output.data`, each with a citation id.
    #[test]
    fn extract_sources_reads_the_runtime_websearch_shape_and_its_citations() {
        let action = serde_json::json!({"type": "WebSearch", "query": "tokio"});
        let result = serde_json::json!({"success": true, "output": {"kind": "generic", "data": {
            "kind": "web_search_result", "source": "exa",
            "results": [{"title": "Tokio", "url": "https://tokio.rs/", "snippet": "", "source": "S4"}]}}});
        let srcs = super::extract_sources("WebSearch", &action, &result, true);
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].cite.as_deref(), Some("S4"));
        let fetch = serde_json::json!({"success": true, "output": {"kind": "fetch_url_result",
            "url": "https://tokio.rs/", "status": 200, "body": "", "source": "S4"}});
        let action = serde_json::json!({"type": "FetchUrl", "url": "https://tokio.rs/"});
        let srcs = super::extract_sources("FetchUrl", &action, &fetch, true);
        assert_eq!(srcs[0].cite.as_deref(), Some("S4"));
    }

    #[test]
    fn extract_sources_returns_empty_for_unrelated_actions() {
        let action = serde_json::json!({"type": "ReadFile", "path": "/etc/hostname"});
        let result = serde_json::json!({"success": true, "output": {"content": "hp"}});
        let srcs = super::extract_sources("ReadFile", &action, &result, true);
        assert!(srcs.is_empty());
    }

    #[test]
    fn extract_sources_marks_failures_ok_false() {
        let action = serde_json::json!({"type": "FetchUrl", "url": "https://broken.test/"});
        let result = serde_json::json!({"success": false, "error": {"message": "timeout"}});
        let srcs = super::extract_sources("FetchUrl", &action, &result, false);
        assert_eq!(srcs.len(), 1);
        assert!(!srcs[0].ok);
    }

    #[test]
    fn parse_pipeline_result_extracts_step_breakdown() {
        // Mirrors the runtime's success-shape: per-step success+duration.
        let action = serde_json::json!({
            "type": "Pipeline",
            "steps": [
                {"type": "FileInfo", "path": "/tmp/x"},
                {"type": "Checksum", "path": "/tmp/x"},
            ]
        });
        let result = serde_json::json!({
            "success": true,
            "output": {
                "steps": [
                    {"action_type": "FileInfo", "success": true, "duration_ms": 9,
                     "output": {"size_bytes": 1638}},
                    {"action_type": "Checksum", "success": true, "duration_ms": 1,
                     "output": {"sha256": "aef7659f8c30deadbeef"}},
                ]
            }
        });
        let pr = parse_pipeline_result(&action, &result, 12, true).expect("parsed");
        assert_eq!(pr.total_steps, 2);
        assert_eq!(pr.completed_steps, 2);
        assert!(!pr.failed);
        assert_eq!(pr.steps[0].action_type, "FileInfo");
        assert!(pr.steps[0].summary.contains("1638"));
        assert!(pr.steps[1].summary.starts_with("sha256 "));
    }

    #[test]
    fn parse_pipeline_result_marks_failed_step() {
        let action = serde_json::json!({"type": "Pipeline", "steps": []});
        let result = serde_json::json!({
            "success": false,
            "output": {
                "steps": [
                    {"action_type": "SearchCode", "success": false, "duration_ms": 2,
                     "error": {"message": "No pattern provided"}},
                ]
            }
        });
        let pr = parse_pipeline_result(&action, &result, 3, false).expect("parsed");
        assert!(pr.failed);
        assert_eq!(pr.completed_steps, 0);
        assert!(pr.steps[0].summary.starts_with("err:"));
    }

    #[test]
    fn push_line_trims_when_over_cap() {
        let mut s = AppState::new();
        s.conversation.clear();
        for i in 0..(MAX_CONVERSATION_LINES + 100) {
            s.push_line(ConversationLine::new(LineKind::System, format!("{i}")));
        }
        assert!(s.conversation.len() <= MAX_CONVERSATION_LINES);
        // The freshest lines must survive the drain — last one should be
        // the highest index we pushed.
        let last = &s.conversation.last().unwrap().text;
        assert_eq!(last, &(MAX_CONVERSATION_LINES + 100 - 1).to_string());
    }

    #[test]
    fn short_session_id_is_4_chars() {
        let id = short_session_id();
        assert_eq!(id.len(), 4);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    // ── apply_event coverage ─────────────────────────────────────────────

    /// Until `Ready` the header names no model; the splash showed
    /// "claude-sonnet-4" whatever the config said. A failed boot says so.
    #[test]
    fn the_header_names_no_model_before_ready() {
        assert_eq!(AppState::new().model_label, "connecting…");
        let mut s = AppState::new();
        s.apply_event(AgentEvent::BootFailed("no key".into()));
        assert_eq!(s.model_label, "not connected");
    }

    #[test]
    fn ready_event_updates_header_and_sets_the_idle_status() {
        let mut s = AppState::new();
        let before = s.conversation.len();
        s.apply_event(AgentEvent::SessionMode { plan: true });
        assert!(s.plan_mode);
        s.apply_event(AgentEvent::SessionMode { plan: false });
        assert!(!s.plan_mode);
        s.apply_event(AgentEvent::Ready {
            model_label: "claude-opus-4".into(),
            sandbox_label: "incus-prod".into(),
        });
        assert_eq!(s.model_label, "claude-opus-4");
        assert_eq!(s.sandbox_label, "incus-prod");
        assert_eq!(s.bodies.runtime, BodyState::Idle);
        // Presence lives in the status strip above the input, not in the
        // conversation — and the activity status replaces it while a turn
        // runs.
        assert_eq!(
            s.conversation.len(),
            before,
            "Ready polluted the conversation"
        );
        assert!(s.idle_status.as_deref().unwrap_or("").contains("ready"));
    }

    #[test]
    fn boot_failed_drops_command_channel_and_logs_error() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<TuiCommand>(1);
        let (_tx2, rx2) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        s.attach_channels(tx, rx2);
        assert!(s.cmd_tx.is_some());

        s.apply_event(AgentEvent::BootFailed("no key".into()));

        assert!(
            s.cmd_tx.is_none(),
            "cmd_tx must be dropped after BootFailed"
        );
        assert_eq!(s.conversation.last().unwrap().kind, LineKind::Error);
        assert!(s.conversation.last().unwrap().text.contains("no key"));
    }

    #[test]
    fn token_stream_then_end_emits_one_assistant_line() {
        let _g = QUEUE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = AppState::new();
        let before = s.conversation.len();

        s.apply_event(AgentEvent::AssistantStart);
        assert!(s.agent_busy);
        assert_eq!(s.bodies.runtime, BodyState::Active);
        assert_eq!(s.conversation.len(), before); // no line yet

        s.apply_event(AgentEvent::Token("Hello ".into()));
        s.apply_event(AgentEvent::Token("world".into()));
        s.apply_event(AgentEvent::Token("!".into()));
        assert_eq!(s.conversation.len(), before); // still buffering

        s.apply_event(AgentEvent::AssistantEnd {
            total_tokens: 42,
            cost_usd: 0.001,
        });

        assert!(!s.agent_busy);
        assert_eq!(s.bodies.runtime, BodyState::Idle);
        assert_eq!(s.tokens_used, 42);
        assert!((s.cost_usd - 0.001).abs() < 1e-9);
        // App-side now stores the assistant turn as a SINGLE
        // Assistant line carrying the RAW text — the renderer expands
        // it into the rounded box at draw time using the live panel
        // width. So we expect one Assistant entry with our content
        // verbatim, no box glyphs in the stored text.
        let added: Vec<_> = s.conversation[before..]
            .iter()
            .filter(|l| l.kind == LineKind::Assistant)
            .collect();
        assert_eq!(
            added.len(),
            1,
            "expected exactly one raw Assistant line; got {:#?}",
            added
        );
        assert_eq!(added[0].text, "Hello world!");
    }

    // `input_queue` is process-global, so any test that observes the
    // post-`AssistantEnd` auto-pop must hold this lock to serialise
    // against the other queue-touching tests under `--test-threads N`.
    use crate::input_queue::TEST_LOCK as QUEUE_LOCK;

    #[test]
    fn assistant_end_without_content_does_not_emit_empty_line() {
        // A turn that ends with no tokens (e.g. cancelled or empty
        // response) shouldn't leave a blank Assistant line in the trail.
        let _g = QUEUE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::input_queue::queue_clear();
        let mut s = AppState::new();
        let before = s.conversation.len();
        s.apply_event(AgentEvent::AssistantStart);
        s.apply_event(AgentEvent::AssistantEnd {
            total_tokens: 0,
            cost_usd: 0.0,
        });
        assert_eq!(s.conversation.len(), before);
        assert!(!s.agent_busy);
    }

    #[test]
    fn assistant_end_drains_one_queued_message_and_resends_it() {
        force_truecolor();
        // After the assistant finishes a turn, if the user had pre-typed
        // a follow-up, the runner should be poked again automatically.
        use crate::input_queue;
        let _g = QUEUE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        input_queue::queue_clear();
        input_queue::push_back("next prompt".into());

        let mut s = AppState::new();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        s.attach_channels(cmd_tx, evt_rx);

        s.apply_event(AgentEvent::AssistantStart);
        s.apply_event(AgentEvent::Token("done".into()));
        s.apply_event(AgentEvent::AssistantEnd {
            total_tokens: 0,
            cost_usd: 0.0,
        });

        // The queued line should have been popped and forwarded.
        assert_eq!(input_queue::queue_len(), 0);
        let cmd = cmd_rx
            .try_recv()
            .expect("auto-follow-up command should be sent");
        match cmd {
            TuiCommand::UserInput(s) => assert_eq!(s, "next prompt"),
            other => panic!("wrong command: {:?}", other),
        }

        // And it should have been echoed in the conversation as a User line.
        let last = s.conversation.last().unwrap();
        assert_eq!(last.kind, LineKind::User);
        assert_eq!(last.text, "next prompt");
    }

    /// The indicator cannot go green on a claim.
    ///
    /// A VM reports its own posture, and a host that has been taken over
    /// reports whatever suits it. `gpu_cc_on` here is that self-report: it can
    /// downgrade a verified verdict to `CpuOnly`, never upgrade anything.
    /// A failure of this test means the badge would say "confidential" over
    /// hardware that merely said so about itself — the single worst thing
    /// this indicator could do, because it is consulted precisely when
    /// something valuable is about to be staked on the answer.
    #[test]
    fn a_rejected_verdict_is_red_however_good_the_claim_looks() {
        use x0ne_confidential::model::Verdict;
        let rejected = Verdict::Rejected {
            reason: "measurement mismatch".into(),
            verifier: "structural".into(),
        };
        assert_eq!(
            EnclaveStatus::from_verdict(&rejected, true),
            EnclaveStatus::Standard,
            "a rejected verdict with a perfect GPU claim must still read red"
        );
    }

    /// A verified CPU enclave without a confidential GPU is amber, not green.
    /// Protected memory with an exposed accelerator is a real posture and a
    /// partial one; collapsing it into "confidential" would hide exactly the
    /// gap that matters when the weights live on the GPU.
    #[test]
    fn a_cpu_enclave_without_a_confidential_gpu_is_not_green() {
        use x0ne_confidential::model::{
            ExecutionEnvironment, TeeTechnology, Verdict, VerifierStrength, WorkloadIdentity,
        };
        let verified = Verdict::Verified {
            identity: WorkloadIdentity {
                name: "x0ne-inference".into(),
                environment: ExecutionEnvironment::ConfidentialVm {
                    cpu: TeeTechnology::IntelTdx,
                },
                measurements: Vec::new(),
            },
            verifier: "structural".into(),
            strength: VerifierStrength::Structural,
        };
        assert_eq!(
            EnclaveStatus::from_verdict(&verified, false),
            EnclaveStatus::CpuOnly
        );
        assert_eq!(
            EnclaveStatus::from_verdict(&verified, true),
            EnclaveStatus::Attested
        );
    }

    /// A fresh session is red, not grey, and names what is missing.
    ///
    /// On ordinary infrastructure the honest report is that the GPU's
    /// confidential mode is off — `x0ne-confidential` states it as "absence
    /// of a claim is not a weak claim", and a grey badge would read as
    /// "probably fine". The wording is the owner's (2026-08-16):
    /// `confidential GPU disabled` names the thing that is off, where `not
    /// confidential` named a property nobody had claimed.
    #[test]
    fn a_fresh_session_reports_the_confidential_gpu_is_disabled() {
        let s = AppState::new();
        assert_eq!(s.enclave, EnclaveStatus::Standard);
        let (_, label, rgb) = s.enclave.badge();
        assert_eq!(label, "confidential GPU disabled");
        assert!(
            rgb.0 > rgb.1 && rgb.0 > rgb.2,
            "must render red, got {rgb:?}"
        );
    }

    /// A budget event moves the cost and leaves the token counter alone.
    ///
    /// `BudgetUpdated` fires on every action; `AssistantEnd` fires once per
    /// turn. When both wrote `tokens_used`, the frequent one won and it was
    /// writing a hardcoded zero, so the navbar showed `0 tk · $0.08` — a cost
    /// with no tokens, which cannot happen. A failure here means a second
    /// writer is back on the counter.
    #[test]
    fn a_budget_update_does_not_clobber_the_token_count() {
        let _g = QUEUE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = AppState::new();
        s.apply_event(AgentEvent::AssistantEnd {
            total_tokens: 84_678,
            cost_usd: 0.08,
        });
        assert_eq!(s.tokens_used, 84_678);

        s.apply_event(AgentEvent::BudgetUpdated {
            cost_usd: 0.11,
            limit_usd: Some(2.0),
        });
        assert_eq!(
            s.tokens_used, 84_678,
            "a budget event must not reset the token counter"
        );
        assert_eq!(s.cost_usd, 0.11, "but it does carry the new cost");
    }

    /// The transcript must say WHICH command ran, not merely that one did.
    ///
    /// A failure here means the TUI is back to printing `RunCommand / └ ok`
    /// with the command invisible — the state of screenshot 2026-08-13_04-27,
    /// where eight consecutive RunCommands scrolled past and not one of them
    /// could be read. An operator who cannot see the command cannot audit the
    /// session, so this is a security property, not a cosmetic one.
    #[test]
    fn a_run_command_shows_the_command_it_ran() {
        let mut s = AppState::new();
        s.apply_event(AgentEvent::ToolEnd {
            name: "RunCommand".into(),
            ok: true,
            summary: String::new(),
            action: serde_json::json!({
                "type": "RunCommand",
                "command": "cargo",
                "args": ["test", "--workspace"],
            }),
            result: serde_json::json!({"success": true, "output": {"stdout": "ok", "exit_code": 0}}),
            duration_ms: 3,
        });
        let transcript: String = s
            .conversation
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            transcript.contains("cargo test --workspace"),
            "the command must be readable in the transcript, got:\n{transcript}"
        );
    }

    /// The head line is the anchor Ctrl+E expands from. Prepending the call
    /// header moved which line comes first, and the flag has to move with it —
    /// otherwise expansion attaches to a row that is no longer the top of the
    /// action and the JSON dump lands mid-block.
    #[test]
    fn the_action_head_is_the_first_line_of_the_call() {
        let mut s = AppState::new();
        let before = s.conversation.len();
        s.apply_event(AgentEvent::ToolEnd {
            name: "RunCommand".into(),
            ok: true,
            summary: String::new(),
            action: serde_json::json!({"type": "RunCommand", "command": "ls", "args": []}),
            result: serde_json::json!({"success": true, "output": {"stdout": "", "exit_code": 0}}),
            duration_ms: 1,
        });
        let heads: Vec<_> = s.conversation[before..]
            .iter()
            .filter(|l| l.action_head)
            .collect();
        assert_eq!(heads.len(), 1, "exactly one line may anchor the expansion");
        assert!(
            std::ptr::eq(heads[0], &s.conversation[before]),
            "the anchor must be the first line of the block"
        );
    }

    /// One action, one block. Starting it writes no transcript row.
    ///
    /// `ToolStart` used to push a bare `ReadFile` line, which was the only
    /// thing naming the action until `ToolEnd` gained a real call header.
    /// With both, every action printed its own name twice, once with no
    /// command attached (screenshot 2026-08-13_04-47). The in-flight signal
    /// lives in the status row, which `ToolStart` still arms through
    /// `last_action_name` / `last_action_started` — asserted here, because
    /// dropping the row would otherwise silently cost that too.
    #[test]
    fn starting_a_tool_arms_the_status_row_without_writing_a_line() {
        let mut s = AppState::new();
        let before = s.conversation.len();
        s.apply_event(AgentEvent::ToolStart {
            name: "ReadFile".into(),
            target: Some("/tmp/x".into()),
            action: serde_json::json!({"type": "ReadFile", "path": "/tmp/x"}),
        });
        assert_eq!(
            s.conversation.len(),
            before,
            "starting an action must not write a transcript row"
        );
        assert_eq!(s.last_action_name.as_deref(), Some("ReadFile"));
        assert!(
            s.last_action_started.is_some(),
            "the status row needs a start instant to count from"
        );

        s.apply_event(AgentEvent::ToolEnd {
            name: "ReadFile".into(),
            ok: true,
            summary: "8 KB".into(),
            action: serde_json::json!({"type": "ReadFile", "path": "/tmp/x"}),
            result: serde_json::json!({"success": true, "output": {"content": "hi"}}),
            duration_ms: 3,
        });
        let block: Vec<_> = s.conversation[before..]
            .iter()
            .filter(|l| l.kind == LineKind::Tool)
            .collect();
        assert!(!block.is_empty(), "the completed action must be rendered");
        assert!(
            block.iter().all(|l| l.status == Some(ToolStatus::Ok)),
            "a successful action carries the success status throughout"
        );
        // The name appears, and it appears once as a header — not as a
        // standalone row above one.
        assert_eq!(
            block.iter().filter(|l| l.text.contains("ReadFile")).count(),
            1,
            "the action is named exactly once"
        );
    }

    #[test]
    fn tool_end_failure_renders_failed_status() {
        let mut s = AppState::new();
        s.apply_event(AgentEvent::ToolEnd {
            name: "EditFile".into(),
            ok: false,
            summary: "permission denied".into(),
            action: serde_json::json!({"type": "EditFile", "path": "/tmp/x"}),
            result: serde_json::json!({"success": false, "error": {"message": "permission denied"}}),
            duration_ms: 1,
        });
        let last = s.conversation.last().unwrap();
        assert_eq!(last.status, Some(ToolStatus::Failed));
    }

    #[test]
    fn snapshot_event_emits_snapshot_status() {
        let mut s = AppState::new();
        s.apply_event(AgentEvent::Snapshot {
            name: "pre-refactor".into(),
            paths: vec!["src/".into(), "tests/".into()],
        });
        let last = s.conversation.last().unwrap();
        assert_eq!(last.status, Some(ToolStatus::Snapshot));
        assert!(last.text.contains("pre-refactor"));
        assert!(last.text.contains("2 paths"));
    }

    #[test]
    fn goal_update_replaces_pinned_goal() {
        let mut s = AppState::new();
        assert!(s.goal_label.is_none());
        s.apply_event(AgentEvent::GoalUpdate {
            label: Some("ship v0.3".into()),
            step: Some((1, 3)),
        });
        assert_eq!(s.goal_label.as_deref(), Some("ship v0.3"));
        assert_eq!(s.goal_step, Some((1, 3)));

        // Clear via None.
        s.apply_event(AgentEvent::GoalUpdate {
            label: None,
            step: None,
        });
        assert!(s.goal_label.is_none());
        assert!(s.goal_step.is_none());
    }

    /// The panel summary comes from the persisted goal, bounded, and it says
    /// what it cuts.
    ///
    /// A failure one way would mean a panel that grows with the goal's
    /// history; the other way, a panel that shows twelve entries without
    /// saying it hides one hundred and eighty-eight — « nothing happened »
    /// would become a possible reading of a two-hundred-step goal.
    #[test]
    fn the_goal_summary_is_bounded_and_says_what_it_hides() {
        use crate::goal::state::{Budget, GoalState, GoalStatus};

        // FIXED size, unrelated to `GOAL_SUMMARY_HISTORY`: a fixture never
        // derives from a constant (repo rule, 2026-08-16).
        const ENTRIES: usize = 200;
        assert_eq!(GOAL_SUMMARY_HISTORY, 12);
        assert_eq!(GOAL_SUMMARY_NOTE_CHARS, 160);

        let mut goal = GoalState::new(
            "g1".into(),
            "diagnose the slow service".into(),
            Budget::new(Some(9), None),
            None,
        );
        goal.budget.steps_used = 3;
        for i in 0..ENTRIES {
            goal.append_history(
                crate::goal::state::HistoryKind::Step,
                Some(format!("note {i}")),
                None,
                None,
            );
        }

        let summary = GoalSummary::of(&goal);
        assert_eq!(summary.text, "diagnose the slow service");
        assert_eq!(summary.status, "active");
        assert_eq!(summary.steps_used, 3);
        assert_eq!(summary.steps_limit, Some(9));
        assert_eq!(summary.history.len(), GOAL_SUMMARY_HISTORY);
        // `Created` + `ENTRIES` steps: the total is the file's, not the
        // shown window's.
        assert_eq!(summary.history_total, ENTRIES + 1);
        assert!(
            summary
                .history
                .last()
                .unwrap()
                .contains(&format!("note {}", ENTRIES - 1)),
            "the last entry shown must be the most recent: {:?}",
            summary.history.last()
        );

        // A blocked goal says why, and the reason is cut like the rest: it
        // carries text written by the model (the gaps of a skeptic pass).
        goal.status = GoalStatus::Blocked {
            reason: "x".repeat(1_000),
        };
        let blocked = GoalSummary::of(&goal);
        assert!(
            blocked.status.starts_with("blocked · "),
            "{}",
            blocked.status
        );
        assert!(
            blocked.status.chars().count() < 200,
            "the reason is not bounded: {} characters",
            blocked.status.chars().count()
        );
    }

    /// Cutting is done on characters, never on bytes.
    ///
    /// A failure is a panic mid-render — the screen disappears because a
    /// note contained an accent in the wrong place.
    #[test]
    fn a_multibyte_note_is_clipped_without_panicking() {
        let long = "é".repeat(GOAL_SUMMARY_NOTE_CHARS + 50);
        let clipped = clip(&long, GOAL_SUMMARY_NOTE_CHARS);
        assert_eq!(clipped.chars().count(), GOAL_SUMMARY_NOTE_CHARS + 1);
        assert!(clipped.ends_with('…'));
        // What is already short passes intact, with no cut marker.
        assert_eq!(clip("éàü", GOAL_SUMMARY_NOTE_CHARS), "éàü");
    }

    #[test]
    fn header_tick_only_mutates_counters() {
        let mut s = AppState::new();
        let before_conv = s.conversation.len();
        s.apply_event(AgentEvent::HeaderTick {
            total_tokens: 1234,
            cost_usd: 0.42,
        });
        assert_eq!(s.tokens_used, 1234);
        assert!((s.cost_usd - 0.42).abs() < 1e-9);
        assert_eq!(s.conversation.len(), before_conv); // no new lines
    }

    #[test]
    fn info_and_error_push_distinct_line_kinds() {
        let mut s = AppState::new();
        s.apply_event(AgentEvent::Info("just FYI".into()));
        s.apply_event(AgentEvent::Error("oops".into()));
        let lines = &s.conversation;
        assert_eq!(lines[lines.len() - 2].kind, LineKind::System);
        assert_eq!(lines[lines.len() - 1].kind, LineKind::Error);
    }

    #[test]
    fn drain_agent_events_processes_pending_events() {
        let mut s = AppState::new();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        s.attach_channels(cmd_tx, evt_rx);

        evt_tx.send(AgentEvent::Info("one".into())).unwrap();
        evt_tx.send(AgentEvent::Info("two".into())).unwrap();
        evt_tx.send(AgentEvent::Info("three".into())).unwrap();
        let before = s.conversation.len();

        s.drain_agent_events();

        assert_eq!(s.conversation.len(), before + 3);
        assert!(s.evt_rx.is_some(), "evt_rx must be replaced after drain");
    }

    #[test]
    fn strip_inline_blocks_removes_matrix_payload() {
        // The badge JSON is extracted by the runner before display;
        // leaving it in the conversation would be noise. Both lowercase
        // and the no-trailing-content edge case must be handled.
        let out = strip_inline_blocks("Hey <matrix>{\"rows\":[\"X0NE\"]}</matrix> · ready");
        assert_eq!(out, "Hey  · ready");
    }

    #[test]
    fn strip_inline_blocks_also_strips_action_wrappers() {
        // <action> blocks are dispatched separately by the runner; in
        // the conversation surface they'd be redundant with the tool
        // result line that follows.
        let out = strip_inline_blocks("let me read it. <action>{\"type\":\"ReadFile\"}</action>");
        assert!(out.contains("let me read it"));
        assert!(!out.contains("ReadFile"));
    }

    #[test]
    fn x0ne_response_box_lines_frames_a_short_text() {
        // A one-line response must produce three rows (top, body,
        // bottom). Each row carries embedded ANSI escapes so we
        // check for the *box glyphs* anywhere in the string, not
        // string prefixes — the escapes shift visible positions.
        let lines = x0ne_response_box_lines("Hey!", 60);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains("╭─"),
            "top row missing ╭─: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("x0ne"),
            "top row missing x0ne label: {:?}",
            lines[0]
        );
        assert!(lines[1].contains("│"), "body row missing │: {:?}", lines[1]);
        assert!(lines[1].contains("Hey!"));
        assert!(
            lines[2].contains("╰"),
            "bottom row missing ╰: {:?}",
            lines[2]
        );
    }

    #[test]
    fn x0ne_response_box_lines_handles_multiline_response() {
        // Each newline becomes its own `│ …` row inside the box; the
        // top and bottom borders are still single rows.
        let lines = x0ne_response_box_lines("line 1\nline 2\nline 3", 60);
        assert_eq!(lines.len(), 5); // top + 3 body + bottom
        assert!(lines[1].contains("line 1"));
        assert!(lines[2].contains("line 2"));
        assert!(lines[3].contains("line 3"));
    }

    #[test]
    fn x0ne_response_box_lines_borders_carry_the_assistant_frame_colour() {
        // The contour is the ONE coloured thing about a reply: it carries
        // `prose::ASSISTANT_FRAME` (pale teal) so the answers stand apart
        // from the operator's violet lines, and the prose inside stays in
        // the terminal's default colour. History: the frame was a
        // discreet charcoal (104,112,128) — « discreet grey » — until the
        // owner asked on 2026-08-17 for the colour to be on the outline
        // (« I want it to be the outline »). Same constant as the CLI's
        // `terminal::print_llm_message`, so both frontends agree.
        // `colored` skips emitting escapes when it detects a non-TTY
        // (the test runner) — force it on so we verify the payload
        // the real renderer will see at runtime.
        force_truecolor();
        let lines = x0ne_response_box_lines("hi", 60);
        let (r, g, b) = crate::prose::ASSISTANT_FRAME;
        let esc = format!("\x1b[38;2;{r};{g};{b}m");
        for (i, row) in lines.iter().enumerate() {
            assert!(
                row.contains(&esc),
                "row {i} must carry the frame colour; got:\n{row}"
            );
        }
        assert!(
            !lines.join("\n").contains("\x1b[38;2;104;112;128m"),
            "the old charcoal frame must be gone"
        );
        // The prose itself is not painted: between the `│ ` and the
        // trailing ` │`, "hi" appears with no colour escape of its own.
        let body = &lines[1];
        let hi = body.find("hi").expect("body text");
        let before = &body[..hi];
        assert!(
            before.ends_with(' ') && before.ends_with("m "),
            "text follows the frame reset, uncoloured: {body:?}"
        );
    }

    /// A table cell's inline markdown is rendered, not printed: `**ID**` is
    /// bold "ID", `` `x` `` is coloured "x", and the column is measured on
    /// the visible text so the rows still line up. Owner screenshot
    /// 2026-08-17 12:35: a `/session` table showed the asterisks and the
    /// backticks. A failure would show the markers again, or misalign the
    /// columns by the width of the markers.
    #[test]
    fn table_cells_render_their_inline_markdown() {
        force_truecolor();
        let lines = x0ne_response_box_lines(
            "| Field | Value |\n|---|---|\n| **ID** | `abc` |\n| Résumé | plain |",
            80,
        );
        let raw = lines.join("\n");
        let plain: Vec<String> = lines.iter().map(|l| visible(l)).collect();
        let shown = plain.join("\n");
        assert!(!shown.contains("**"), "bold markers must not show: {shown}");
        assert!(!shown.contains('`'), "code markers must not show: {shown}");
        assert!(raw.contains("\x1b[1mID"), "ID is bold: {raw:?}");
        // Alignment: the separator `│` sits in the same column on both data rows.
        let rows: Vec<&String> = plain
            .iter()
            .filter(|r| r.contains("ID") || r.contains("Résumé"))
            .collect();
        assert_eq!(rows.len(), 2, "{shown}");
        // first `│` is the box rail; the second is the table separator
        let sep = |row: &str| {
            let first = row.find('│').unwrap();
            let rest = &row[first + '│'.len_utf8()..];
            rest.find('│').map(|i| rest[..i].chars().count())
        };
        assert_eq!(
            sep(rows[0]),
            sep(rows[1]),
            "columns aligned on visible width: {shown}"
        );
    }

    /// Strip ANSI so we can measure what the terminal actually shows.
    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            if c == '\x1b' {
                in_esc = true;
                continue;
            }
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn x0ne_response_box_is_closed_on_all_four_sides() {
        // The user reported the TUI box hanging open on the right while the
        // CLI one was already closed. Every row must end with a border glyph,
        // and the corners must be the rounded set.
        force_truecolor();
        let lines = x0ne_response_box_lines(
            "Hello there.\n\n- a list item\n- another one\n\nAnd a closing paragraph.",
            74,
        );
        let plain: Vec<String> = lines.iter().map(|l| visible(l)).collect();

        assert!(plain[0].starts_with("  ╭─ x0ne "), "top: {:?}", plain[0]);
        assert!(plain[0].ends_with('╮'), "top not closed: {:?}", plain[0]);
        let last = plain.last().unwrap();
        assert!(last.starts_with("  ╰"), "bottom: {last:?}");
        assert!(last.ends_with('╯'), "bottom not closed: {last:?}");
        for row in &plain[1..plain.len() - 1] {
            assert!(row.starts_with("  │"), "body row not railed: {row:?}");
            assert!(row.ends_with('│'), "body row open on the right: {row:?}");
        }
    }

    #[test]
    fn x0ne_response_box_rows_are_all_the_same_width() {
        // A closed box is only premium if the right border is a straight
        // line. Padding is by visible width, so ANSI styling must not bend it.
        force_truecolor();
        for panel in [30usize, 48, 74, 100] {
            let lines = x0ne_response_box_lines(
                "Some **bold** prose that will wrap at narrow widths, plus `code`.",
                panel,
            );
            let widths: Vec<usize> = lines.iter().map(|l| visible(l).chars().count()).collect();
            let first = widths[0];
            assert!(
                widths.iter().all(|w| *w == first),
                "ragged edge at panel {panel}: widths {widths:?}"
            );
            // And the frame fills the panel it was given.
            assert_eq!(first, panel.max(20), "frame does not span the panel");
        }
    }

    #[test]
    fn print_x0ne_box_snapshot() {
        colored::control::set_override(false);
        println!();
        for l in x0ne_response_box_lines(
            "Hello! I see you're on the `harden/security-core-2026-08` branch.\n\n- item one\n- item two\n\nWhat would you like to work on?",
            74,
        ) {
            println!("{}", l);
        }
    }

    #[test]
    fn x0ne_response_box_lines_empty_input_is_empty_output() {
        // Empty / whitespace-only responses should not push an empty
        // box into the conversation.
        assert!(x0ne_response_box_lines("", 60).is_empty());
        assert!(x0ne_response_box_lines("   \n  ", 60).is_empty());
    }

    #[test]
    fn badge_update_event_lands_on_state_with_parsed_frame() {
        // End-to-end sanity for the "LLM-as-GPU" path: a BadgeUpdate
        // wire event must parse the JSON, set state.badge to Some,
        // and the parsed frame must carry the rows we sent. This is
        // the contract every PaintBadge action and every <matrix>
        // inline block ultimately rides on.
        let mut s = AppState::new();
        assert!(s.badge.is_none(), "fresh state must have no badge");

        let payload = serde_json::json!({
            "rows": ["X0NE", "live"],
            "styles": [{"row":0,"col":0,"len":4,"fg":"#5eead4","bold":true}]
        });
        s.apply_event(AgentEvent::BadgeUpdate {
            frame_json: payload.to_string(),
        });

        let playback = s
            .badge
            .as_ref()
            .expect("badge should be set after BadgeUpdate");
        let badge = playback.current_frame().expect("playback has a frame");
        // The parser pads rows to BADGE_HEIGHT × BADGE_WIDTH, so we
        // verify the FIRST chars of each row rather than full equality.
        assert!(badge.rows[0].starts_with("X0NE"));
        assert!(badge.rows[1].starts_with("live"));
        // The styled span must have carried through with its color.
        assert_eq!(badge.styles.len(), 1);
        assert!(badge.styles[0].bold);
        // Single-frame payload → degenerate playback (1 frame, no loop).
        assert_eq!(playback.frames.len(), 1);
        assert!(!playback.looping);
    }

    #[test]
    fn badge_update_with_garbage_json_does_not_clobber_existing_badge() {
        // The runner must NEVER let a malformed payload from the LLM
        // wipe a previously-good badge — that would mean a sloppy
        // model could erase its own avatar by trying to paint a new
        // one. We keep the old one and silently ignore the bad input.
        let mut s = AppState::new();
        let good = serde_json::json!({"rows": ["X0NE"]}).to_string();
        s.apply_event(AgentEvent::BadgeUpdate { frame_json: good });
        assert!(s.badge.is_some());

        let before = s.badge.clone();
        s.apply_event(AgentEvent::BadgeUpdate {
            frame_json: "not json at all".into(),
        });
        assert_eq!(
            s.badge, before,
            "garbage payload must not clobber the previous badge"
        );
    }

    #[test]
    fn markdown_bold_wraps_text_with_bold_sgr() {
        force_truecolor();
        let out = format_md_inline("hello **world** !");
        // SGR 1 = bold; the inner text must be wrapped in it.
        assert!(
            out.contains("\x1b[1m") && out.contains("world"),
            "expected bold escape around 'world': {:?}",
            out
        );
    }

    #[test]
    fn markdown_inline_code_uses_warm_dim_grey() {
        force_truecolor();
        let out = format_md_inline("call `ReadFile` then");
        // (140,150,170) is the chosen "quiet code" tone.
        assert!(
            out.contains("\x1b[38;2;140;150;170m"),
            "expected truecolor(140,150,170) around `ReadFile`: {:?}",
            out
        );
        assert!(out.contains("ReadFile"));
    }

    #[test]
    fn markdown_headers_strip_marker_and_bold() {
        force_truecolor();
        let out = format_md_line("## My header");
        assert!(
            !out.contains("##"),
            "header marker must be stripped: {:?}",
            out
        );
        assert!(out.contains("\x1b[1m") && out.contains("My header"));
    }

    #[test]
    fn markdown_nested_bullet_preserves_indent() {
        // `  - nested` (2-space indent) should still render as a
        // dimmed `·`, indent preserved so the visual hierarchy
        // matches the source. The screenshot caught this — without
        // it the `- ` was leaking through raw on nested items.
        force_truecolor();
        let out = format_md_line("  - nested item");
        assert!(
            !out.contains("- nested"),
            "raw `- ` marker should be gone: {:?}",
            out
        );
        assert!(
            out.contains("·"),
            "nested bullet must render as `·`: {:?}",
            out
        );
        assert!(
            out.starts_with("  "),
            "two-space indent must be preserved: {:?}",
            out
        );
        assert!(out.contains("nested item"));
    }

    #[test]
    fn markdown_bullet_substitutes_interpunct() {
        force_truecolor();
        let out = format_md_line("- first item");
        // The leading `- ` becomes a dimmed `·` interpunct.
        assert!(out.contains("·"), "bullet must use interpunct: {:?}", out);
        assert!(out.contains("first item"));
        assert!(!out.starts_with("- "), "raw `- ` marker must be gone");
    }

    #[test]
    fn markdown_blockquote_uses_dim_bar_and_italic_body() {
        force_truecolor();
        let out = format_md_line("> wisdom incoming");
        assert!(!out.starts_with("> "), "blockquote marker must be stripped");
        assert!(
            out.contains("▎"),
            "blockquote must use the vertical bar glyph: {:?}",
            out
        );
        // SGR 3 = italic; `colored` fuses it with truecolor as
        // `\x1b[3;38;2;…m` so we look for the `3;` opening pattern
        // rather than the standalone `\x1b[3m`.
        assert!(
            out.contains("\x1b[3;") || out.contains("\x1b[3m"),
            "blockquote body must carry italic SGR (3): {:?}",
            out
        );
    }

    #[test]
    fn markdown_ordered_list_keeps_number_and_dims_marker() {
        force_truecolor();
        let out = format_md_line("1. first thing");
        assert!(
            out.contains("1."),
            "ordered marker must be preserved: {:?}",
            out
        );
        assert!(out.contains("first thing"));
        // Marker must carry the dim tone (152,162,177). Raised from the
        // original (110,120,135), which sat near 2.4:1 on the dark ground —
        // below the WCAG AA floor and the reason the UI was hard to read.
        assert!(
            out.contains("\x1b[38;2;152;162;177m"),
            "marker should be dimmed: {:?}",
            out
        );
    }

    #[test]
    fn markdown_horizontal_rule_replaces_with_dashed_bar() {
        let out = format_md_line("---");
        assert!(out.contains("─"), "hr must render as a dash bar: {:?}", out);
        // Stars and underscores should also work.
        assert!(format_md_line("***").contains("─"));
        assert!(format_md_line("___").contains("─"));
    }

    #[test]
    fn markdown_strip_inline_html_keeps_content() {
        // `<kbd>Ctrl</kbd>` → `Ctrl`. `<mark>foo</mark>` → `foo`.
        // The tags themselves vanish, the prose underneath stays.
        let out = format_md_inline("Press <kbd>Ctrl</kbd> then <mark>foo</mark>");
        assert!(out.contains("Ctrl"));
        assert!(out.contains("foo"));
        assert!(!out.contains("<kbd>"));
        assert!(!out.contains("</mark>"));
    }

    #[test]
    fn markdown_strip_inline_html_does_not_eat_autolinks() {
        // Regression: my first cut of strip_inline_html ate
        // `<https://...>` because it looks tag-shaped. The `://`
        // discriminator keeps autolinks alive for render_links.
        force_truecolor();
        let out = format_md_inline("see <https://anthropic.com>");
        assert!(out.contains("https://anthropic.com"));
        assert!(out.contains("\x1b[4m"), "URL must be underlined: {:?}", out);
    }

    #[test]
    fn markdown_footnote_refs_render_as_dim_marker() {
        // `Here[^1] a note` → `Here[1] a note` (dimmed).
        force_truecolor();
        let out = format_md_inline("Here[^1] a claim");
        assert!(out.contains("[1]"), "footnote marker missing: {:?}", out);
        assert!(!out.contains("[^"), "raw `[^` must be gone: {:?}", out);
        // Dim grey for the marker.
        assert!(out.contains("\x1b[38;2;140;150;170m"));
    }

    #[test]
    fn pre_strip_noise_drops_reference_and_footnote_definitions() {
        let text = "Some prose.\n[doc]: https://example.com\n[^1]: Footnote body.\nMore prose.";
        let stripped = pre_strip_noise(text);
        assert!(stripped.contains("Some prose."));
        assert!(stripped.contains("More prose."));
        assert!(!stripped.contains("[doc]:"), "ref def must be gone");
        assert!(!stripped.contains("[^1]:"), "footnote def must be gone");
    }

    #[test]
    fn markdown_link_with_title_keeps_url_drops_title() {
        // `[label](url "title")` — label underlined, URL appended
        // in dim (so the terminal auto-detects and ctrl-clicks),
        // title dropped (no good place to show it).
        force_truecolor();
        let out = format_md_inline(r#"see [Claude](https://claude.com "official")"#);
        assert!(out.contains("\x1b[4m"));
        assert!(out.contains("Claude"));
        assert!(
            out.contains("https://claude.com"),
            "url must be in dim for terminal auto-detect: {:?}",
            out
        );
        assert!(!out.contains("official"), "title text must be dropped");
        // URL gets the dim tone (152,162,177) — our "data" tone, raised
        // from (110,120,135) by the contrast pass.
        assert!(out.contains("\x1b[38;2;152;162;177m"));
    }

    #[test]
    fn markdown_autolink_renders_url_underlined() {
        // `<https://...>` — the URL itself becomes the underlined
        // label, the angle brackets disappear.
        force_truecolor();
        let out = format_md_inline("voir <https://anthropic.com> ici");
        assert!(out.contains("\x1b[4m"));
        assert!(out.contains("https://anthropic.com"));
        assert!(!out.contains("<https"));
        assert!(out.contains("voir") && out.contains("ici"));
    }

    #[test]
    fn markdown_image_renders_alt_with_image_glyph_and_url() {
        // `![alt](url)` — `🖼 <alt-underlined> <url-dim>` so the
        // image URL is ctrl-clickable too.
        force_truecolor();
        let out = format_md_inline("![Logo](https://x.com/img.png)");
        assert!(out.contains("🖼"), "image glyph missing: {:?}", out);
        assert!(out.contains("Logo"));
        assert!(
            out.contains("img.png"),
            "image URL must be present: {:?}",
            out
        );
    }

    #[test]
    fn markdown_reference_link_underlines_visible_label() {
        // `[docs][ref]` — we don't resolve `[ref]: url`, but the
        // visible `[docs]` label is still rendered underlined.
        force_truecolor();
        let out = format_md_inline("see [docs][ref] section");
        assert!(out.contains("\x1b[4m"));
        assert!(out.contains("docs"));
        // The `[ref]` marker must NOT remain visible.
        assert!(!out.contains("[ref]"));
        assert!(out.contains("section"));
    }

    #[test]
    fn markdown_link_keeps_url_inline_for_terminal_auto_detect() {
        // The URL stays in the rendered output (dim grey) so the
        // terminal's URL-detector can make it ctrl-clickable — that
        // was the user's complaint. Label still underlined for
        // visual emphasis.
        force_truecolor();
        let out = format_md_inline("see [docs](https://example.com) for more");
        assert!(
            out.contains("\x1b[4m"),
            "link label must be underlined: {:?}",
            out
        );
        assert!(out.contains("docs"));
        assert!(
            out.contains("https://example.com"),
            "URL must be present: {:?}",
            out
        );
        assert!(out.contains("for more"), "trailing prose must survive");
    }

    #[test]
    fn markdown_strikethrough_uses_crossed_out_sgr() {
        force_truecolor();
        let out = format_md_inline("~~deleted~~ kept");
        // SGR 9 = strikethrough.
        assert!(out.contains("\x1b[9m"), "strike must use SGR 9: {:?}", out);
        assert!(out.contains("deleted"));
        assert!(out.contains("kept"));
    }

    #[test]
    fn markdown_fenced_code_block_preserves_lines_and_dims_them() {
        // Block-aware: a ```rust ... ``` chunk must render its body
        // verbatim (no markdown processing inside), with each token
        // syntax-highlighted. The fence lines turn into header/
        // footer separators. We check for tokens individually
        // because ANSI escapes now split the source between every
        // keyword / string boundary.
        force_truecolor();
        let lines = x0ne_response_box_lines(
            "before\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\nafter",
            80,
        );

        let body: String = lines.join("\n");
        assert!(
            body.contains("┄ rust ┄"),
            "code-block header missing: {:?}",
            body
        );
        // Tokens survive (escapes may sit between them). `println`
        // is split from its trailing `!` by the highlighter so we
        // only check the identifier piece.
        assert!(body.contains("fn"));
        assert!(body.contains("main"));
        assert!(body.contains("println"));
        assert!(body.contains("\"hi\""));
        // Triple-backticks themselves must NOT appear as text inside
        // the rendered box — they were consumed as fences.
        assert!(
            !body.contains("```"),
            "raw ``` should not survive: {:?}",
            body
        );
    }

    #[test]
    fn markdown_inside_code_block_is_not_interpreted() {
        // `**foo**` inside a fenced block must stay literal — no
        // bold escape, no marker consumption. That's the whole
        // point of code blocks.
        force_truecolor();
        let lines = x0ne_response_box_lines("```\n**not bold**\n```", 80);
        let body: String = lines.join("\n");
        assert!(
            body.contains("**not bold**"),
            "asterisks inside code block must survive verbatim: {:?}",
            body
        );
    }

    #[test]
    fn markdown_unmatched_marker_is_left_verbatim() {
        // An asterisk in prose ("3 * 4 = 12") must not eat the rest
        // of the line because there's no matching close. The unmatched
        // opener is left as-is.
        let out = format_md_inline("score is 3 * 4 always");
        assert!(out.contains("3 * 4 always"));
    }

    #[test]
    fn markdown_table_detects_pipe_row_plus_separator() {
        // A row + a `|---|---|` separator + body rows must be
        // detected as a table, NOT rendered as raw `|` lines.
        let text = "Intro\n\n| A | B |\n|---|---|\n| 1 | 2 |\n| foo | bar |\n\nDone";
        let lines = x0ne_response_box_lines(text, 80);
        let blob: String = lines.join("\n");
        // Raw `|---|---|` separator must NOT appear in output.
        assert!(
            !blob.contains("|---|"),
            "raw separator must be consumed: {}",
            blob
        );
        // Cells must be present.
        assert!(blob.contains("A") && blob.contains("B"));
        assert!(blob.contains("foo") && blob.contains("bar"));
        // The dim cell separator (vertical bar) must show up.
        assert!(blob.contains("│"), "table must use `│` separator");
    }

    #[test]
    fn markdown_table_header_row_is_bold_underlined() {
        // First row = header → bold. Then a `─┼─` underline before
        // the body. Confirms the typography hierarchy.
        force_truecolor();
        let lines = x0ne_response_box_lines("| h1 | h2 |\n|---|---|\n| a | b |", 80);
        let blob: String = lines.join("\n");
        // SGR 1 = bold on the header row.
        assert!(blob.contains("\x1b[1m"), "header must be bold: {}", blob);
        // Underline glyph `─` connected with `┼` between cols.
        assert!(
            blob.contains("─┼─"),
            "must have `─┼─` underline between cols: {}",
            blob
        );
    }

    #[test]
    fn markdown_table_truncates_wide_columns_with_ellipsis() {
        // A table wider than the panel must shrink with `…` so it
        // never escapes the box.
        let text = "| short | very long long long long long content |\n|---|---|\n| x | y |";
        let lines = x0ne_response_box_lines(text, 30);
        let blob: String = lines.join("\n");
        assert!(
            blob.contains("…"),
            "wide column must be truncated with `…`: {}",
            blob
        );
    }

    #[test]
    fn highlight_code_keywords_in_rust_are_bolded() {
        force_truecolor();
        let out = highlight_code_line("fn main() { let x = 1; }", Some("rust"));
        // SGR 1 = bold on keywords (fn, let).
        assert!(
            out.contains("\x1b[1m"),
            "rust keywords must be bold: {}",
            out
        );
        assert!(out.contains("fn") && out.contains("let") && out.contains("main"));
    }

    #[test]
    fn highlight_code_strings_get_teal_color() {
        force_truecolor();
        let out = highlight_code_line(r#"print("hello")"#, Some("python"));
        // Strings → ACCENT teal (94, 234, 212).
        assert!(
            out.contains("\x1b[38;2;94;234;212m"),
            "string literal must be teal: {}",
            out
        );
        assert!(
            out.contains("\"hello\""),
            "string content survives: {}",
            out
        );
    }

    #[test]
    fn highlight_code_comments_are_dim_italic() {
        force_truecolor();
        let out = highlight_code_line("let x = 1; // important", Some("rust"));
        // Comment marker + body → dim italic. SGR 3 = italic, used
        // fused with truecolor as `\x1b[3;38;…m`.
        assert!(out.contains("// important"));
        assert!(
            out.contains("\x1b[3;") || out.contains("\x1b[3m"),
            "comment must be italic: {}",
            out
        );
    }

    #[test]
    fn highlight_code_unknown_lang_falls_back_to_body_color() {
        // Unknown language → no keyword bolding, no string teal.
        // Just the soft body grey (170, 180, 200).
        force_truecolor();
        let out = highlight_code_line("fn main() {}", Some("klingon"));
        assert!(
            out.contains("\x1b[38;2;170;180;200m"),
            "unknown lang must use body grey: {}",
            out
        );
        // No bold escape should be emitted for keywords.
        assert!(
            !out.contains("\x1b[1m"),
            "no keyword bolding for unknown lang: {}",
            out
        );
    }

    #[test]
    fn x0ne_response_box_lines_scales_header_bar_with_panel_width() {
        // The bar length adapts to panel_width — that's what gives
        // the frame its dynamic look ("it doesn't take all the
        // allocated window space"). Strip ANSI and count `─` chars.
        let strip_ansi = |s: &str| -> String {
            let mut out = String::new();
            let mut in_esc = false;
            for c in s.chars() {
                if c == '\x1b' {
                    in_esc = true;
                    continue;
                }
                if in_esc {
                    if c == 'm' {
                        in_esc = false;
                    }
                    continue;
                }
                out.push(c);
            }
            out
        };
        let narrow = x0ne_response_box_lines("hi", 30);
        let wide = x0ne_response_box_lines("hi", 100);
        let count = |s: &str| s.chars().filter(|c| *c == '─').count();
        assert!(
            count(&strip_ansi(&wide[0])) > count(&strip_ansi(&narrow[0])),
            "wide panel must give a longer header bar"
        );
    }
}
