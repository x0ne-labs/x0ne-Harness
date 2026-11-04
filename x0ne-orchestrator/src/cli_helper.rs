use colored::Colorize;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Cmd, ConditionalEventHandler, Context, Event, EventContext, Helper, RepeatCount};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

// Tab-completion catalogue. Every slash command (and their useful
// sub-tokens) should appear here so `/<TAB>` shows the operator the
// full surface. Aliases listed alongside the canonical name. Keep
// alphabetised within sections to make missing-entry audits cheap.
//
// To audit: `grep -E '"/[a-z]' src/slash.rs | sort -u` and diff against
// this list — anything unique on the slash.rs side is a missing entry.
pub(crate) const COMMANDS: &[&str] = &[
    // ── Session & UX ────────────────────────────────────────────────
    "/help",
    "/setup",
    "/actions",
    "/history",
    "/expand",
    "/unfold",
    "/x",
    "/session",
    "/sessions",
    "/sessions --all",
    "/rename",
    "/resume",
    "/playground",
    "/cost",
    "/handles",
    "/clear",
    "/plan",
    "/build",
    "/quit",
    "/exit",
    "/q",
    "/auto",
    "/compact",
    "/save_config",
    "/save-config",
    "/connectors",
    "/queue",
    "/queue clear",
    "/queue drop",
    // ── Persistent goals (Codex-style) ──────────────────────────────
    "/goal",
    "/goal status",
    "/goal step",
    "/goal pause",
    "/goal resume",
    "/goal complete",
    "/goal clear",
    "/goal help",
    // ── LLM provider / model ───────────────────────────────────────
    "/model",
    "/model list",
    "/provider",
    "/provider list",
    "/provider key",
    "/provider anthropic",
    "/provider ollama",
    "/provider openai",
    "/provider openai-compat",
    "/provider llamacpp",
    "/provider lmstudio",
    "/provider kimi",
    "/provider groq",
    "/provider openrouter",
    "/auth",
    "/auth setup",
    "/auth anthropic",
    "/auth openai",
    "/auth openai-compat",
    "/auth ollama",
    // ── Memory & budget ────────────────────────────────────────────
    "/remember",
    "/mem",
    "/budget",
    // ── Handles & policy ───────────────────────────────────────────
    "/check",
    "/stop",
    "/policy",
    "/pulse",
    "/target",
    // ── Snapshots ──────────────────────────────────────────────────
    "/snapshot",
    "/restore",
    // ── Files & context ────────────────────────────────────────────
    "/read",
    // ── Git workflow ───────────────────────────────────────────────
    "/diff",
    "/log",
    "/commit",
    "/undo",
    "/stash",
    // ── Modes & meta ───────────────────────────────────────────────
    "/until",
    "/export",
    "/matrix",
    "/benchmark",
    "/bench",
    "/shell",
    "/sh",
    // ── Multi-agent worlds ─────────────────────────────────────────
    "/worlds",
    "/spawn",
    "/worlds-reset",
    // ── Artifacts (web-served folders) ─────────────────────────────
    "/artifact",
    "/artifacts",
    "/artifact list",
    "/artifact open",
    "/artifact save",
    "/artifact stop",
    "/artifact rm",
];

/// Every command Tab may offer.
pub(crate) fn commands() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().copied()
}

#[derive(Helper)]
pub struct X0neHelper;

impl Validator for X0neHelper {
    fn validate(&self, _ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(ValidationResult::Valid(None))
    }

    fn validate_while_typing(&self) -> bool {
        false
    }
}

impl Completer for X0neHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        let mut candidates = Vec::new();
        // Only autocomplete if we are at the start and typing a slash command
        if line.starts_with('/') {
            let word = &line[..pos];
            for cmd in commands() {
                if cmd.starts_with(word) {
                    candidates.push(Pair {
                        display: cmd.to_string(),
                        replacement: cmd.to_string(),
                    });
                }
            }
        }

        let start = 0; // replace from the beginning of the line
        Ok((start, candidates))
    }
}

impl Hinter for X0neHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if line.is_empty() || pos < line.len() {
            return None;
        }

        if line.starts_with('/') {
            for cmd in commands() {
                if cmd.starts_with(line) && cmd.len() > line.len() {
                    return Some(cmd[line.len()..].to_string());
                }
            }
        }

        None
    }
}

impl Highlighter for X0neHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Output hint in dark gray
        Cow::Owned(hint.truecolor(100, 100, 100).to_string())
    }

    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.starts_with('/') {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            let cmd = parts[0];
            let rest = parts.get(1).unwrap_or(&"");

            let colored_cmd = cmd.cyan().bold().to_string();

            if parts.len() > 1 {
                Cow::Owned(format!("{} {}", colored_cmd, rest))
            } else {
                Cow::Owned(colored_cmd)
            }
        } else if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            // Plain prose gets the same treatment as a user message in the
            // TUI — violet, italic (render.rs VIOLET = 196,181,224). One
            // identity for "what the human said", whichever surface they are
            // on, and it separates their words from the agent's at a glance.
            Cow::Owned(line.truecolor(196, 181, 224).italic().to_string())
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        // Every keystroke re-highlights: prose is styled now too, not just
        // slash commands, so returning false for prose would leave the line
        // unstyled until a slash appeared.
        true
    }
}

// ── Ctrl+E → /x (single-keystroke unfold) ─────────────────────────────────────
//
// Rustyline binds one `Cmd` per event and `EventHandler::Macro` is still a
// TODO upstream, so we cannot atomically "replace buffer with /x AND submit"
// from a single keystroke. We work around this with a process-wide flag:
//
// 1. The Ctrl+E `ConditionalEventHandler` flips `EXPAND_REQUESTED` and returns
//    `Cmd::AcceptLine` — readline returns immediately (whatever was in the
//    buffer is discarded; this is fine because Ctrl+E is by design an
//    "between-prompts" peek shortcut, not something you'd press mid-message).
// 2. The CLI loop checks `take_expand_requested()` right after `readline()`
//    and, if set, substitutes the resulting line with `/x` before slash
//    dispatch. From the operator's POV: one keystroke → unfolded session.
//
// `AtomicBool` (instead of e.g. a OnceCell<Mutex<bool>>) is deliberate — the
// hot path is "check this flag every readline() return", and a relaxed atomic
// load is the cheapest possible signaling primitive.

static EXPAND_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` exactly once after Ctrl+E was pressed; clears the flag.
/// Call this right after `Editor::readline()` returns.
pub fn take_expand_requested() -> bool {
    EXPAND_REQUESTED.swap(false, Ordering::Relaxed)
}

// ── Expand 3-state cycle ─────────────────────────────────────────────────────
//
// Ctrl+E cycles through three views of the session in a clean round-trip:
//
//   `Live`     → live conversation as it happens; boxed AI responses, no
//                session-wide view. The default after any regular input.
//                Ctrl+E here goes to `Expanded`.
//   `Expanded` → FULL session unfolded (every action JSON, every result
//                body, no truncation — the output of `/expand` / `/x`).
//                Ctrl+E here goes to `Summary`.
//   `Summary`  → 1-line-per-message summary view (the output of
//                `/history`). Ctrl+E here goes BACK to `Live` so the
//                cycle round-trips: Live → Expanded → Summary → Live.
//
// `Ctrl+O` is a SHORTCUT (not a cycle): it always jumps directly to
// `Expanded`, regardless of the current state. Mnemonic: "Overview".
// Mirrors Claude Code's `Ctrl+O` (full transcript) so users coming from
// there get familiar muscle memory.
//
// The state is process-global (`EXPAND_STATE: AtomicU8`) because
// rustyline's keybind handler runs in a different stack frame from the
// CLI loop and they need a lock-free signaling channel. ANY non-keystroke
// input (a real command, an empty Enter) resets to `Live` because the
// bottom of the screen is no longer the session view.
//
// Why a screen clear (vs. erasing exactly N lines) on each transition:
// line counting is fragile against terminal wrapping — the expand view
// contains very wide JSON / stdout lines and tracking how many *visual*
// rows a `println!` produced is terminal-specific. Clearing the screen
// is universally reliable; the conversation memory is untouched (the
// LLM still sees everything).

const EXPAND_LIVE: u8 = 0;
const EXPAND_EXPANDED: u8 = 1;
const EXPAND_SUMMARY: u8 = 2;

static EXPAND_STATE: AtomicU8 = AtomicU8::new(EXPAND_LIVE);

/// `Ctrl+O` request flag. Set once by the keybind, consumed by the CLI
/// loop. Same one-shot semantics as `EXPAND_REQUESTED` for `Ctrl+E`.
static OVERVIEW_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Variants returned by `expand_state()` — see module doc above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandState {
    /// Live conversation — boxed AI responses, no session-wide view.
    Live,
    /// Full unfolded session — every action JSON, every result body.
    Expanded,
    /// Compact summary — one line per message.
    Summary,
}

impl ExpandState {
    fn from_u8(v: u8) -> Self {
        match v {
            EXPAND_EXPANDED => Self::Expanded,
            EXPAND_SUMMARY => Self::Summary,
            _ => Self::Live,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Self::Live => EXPAND_LIVE,
            Self::Expanded => EXPAND_EXPANDED,
            Self::Summary => EXPAND_SUMMARY,
        }
    }

    /// What the next `Ctrl+E` press will produce. Drives the navbar hint
    /// so the affordance reads "^E `<next-view>`" — the operator always
    /// knows what's one keystroke away.
    pub fn next_in_cycle(self) -> Self {
        match self {
            Self::Live => Self::Expanded,
            Self::Expanded => Self::Summary,
            Self::Summary => Self::Live,
        }
    }

    /// Short label for the navbar / `/help`. Lowercase, single word.
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Expanded => "expand",
            Self::Summary => "summary",
        }
    }
}

/// Atomically read the current expand-cycle state.
pub fn expand_state() -> ExpandState {
    ExpandState::from_u8(EXPAND_STATE.load(Ordering::Relaxed))
}

/// Atomically set the expand-cycle state. Used by the CLI loop after
/// printing the corresponding view.
pub fn set_expand_state(state: ExpandState) {
    EXPAND_STATE.store(state.as_u8(), Ordering::Relaxed);
}

/// Reset to `Live` — call when ANY non-keystroke input is processed.
pub fn clear_expand_active() {
    set_expand_state(ExpandState::Live);
}

/// Returns `true` if a session view (Expanded OR Summary) is currently
/// displayed. Read-only peek; does NOT mutate state. Used by the navbar
/// to format the affordance.
pub fn is_expand_active() -> bool {
    expand_state() != ExpandState::Live
}

/// Returns `true` exactly once after `Ctrl+O` was pressed; clears the flag.
pub fn take_overview_requested() -> bool {
    OVERVIEW_REQUESTED.swap(false, Ordering::Relaxed)
}

/// `ConditionalEventHandler` for the Ctrl+E keybind. Flips the global flag
/// and asks rustyline to accept the (about to be discarded) line so the
/// readline loop returns control to us immediately.
pub struct ExpandHandler;

impl ConditionalEventHandler for ExpandHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        EXPAND_REQUESTED.store(true, Ordering::Relaxed);
        Some(Cmd::AcceptLine)
    }
}

/// `ConditionalEventHandler` for the Ctrl+O keybind. Same plumbing as
/// `ExpandHandler`, but signals via `OVERVIEW_REQUESTED` so the loop can
/// distinguish "advance the cycle" (Ctrl+E) from "jump straight to
/// expanded" (Ctrl+O).
pub struct OverviewHandler;

impl ConditionalEventHandler for OverviewHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        OVERVIEW_REQUESTED.store(true, Ordering::Relaxed);
        Some(Cmd::AcceptLine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `take_expand_requested()` must be a one-shot consumer — repeated calls
    /// after a single Ctrl+E should NOT keep returning `true`, otherwise
    /// every subsequent readline would silently get rewritten to `/x`.
    #[test]
    fn expand_flag_is_one_shot() {
        EXPAND_REQUESTED.store(true, Ordering::Relaxed);
        assert!(
            take_expand_requested(),
            "first call should observe the flag"
        );
        assert!(
            !take_expand_requested(),
            "flag should be cleared after read"
        );
        assert!(
            !take_expand_requested(),
            "still cleared on subsequent reads"
        );
    }

    /// The 3-state cycle must round-trip cleanly:
    ///   Live → Expanded → Summary → Live → Expanded → ...
    /// `clear_expand_active` resets to Live at any point. `is_expand_active`
    /// is `true` for both Expanded and Summary (so the navbar can switch
    /// its hint label) and `false` only for Live.
    #[test]
    fn expand_state_cycle() {
        // Defensive reset — other tests in this module touch the same flag.
        clear_expand_active();
        assert_eq!(expand_state(), ExpandState::Live);
        assert!(!is_expand_active(), "Live is not 'active'");

        // Cycle: Live → Expanded → Summary → Live (clean round-trip).
        let next = expand_state().next_in_cycle();
        assert_eq!(next, ExpandState::Expanded);
        set_expand_state(next);
        assert!(is_expand_active(), "Expanded counts as active");

        let next = expand_state().next_in_cycle();
        assert_eq!(next, ExpandState::Summary);
        set_expand_state(next);
        assert!(is_expand_active(), "Summary counts as active");

        let next = expand_state().next_in_cycle();
        assert_eq!(next, ExpandState::Live, "Summary cycles back to Live");
        set_expand_state(next);
        assert!(!is_expand_active(), "back at Live");

        clear_expand_active();
        assert_eq!(
            expand_state(),
            ExpandState::Live,
            "clear_expand_active resets to Live regardless of source state"
        );
        assert!(!is_expand_active());
    }

    /// `take_overview_requested()` is a one-shot consumer like
    /// `take_expand_requested()`.
    #[test]
    fn overview_flag_is_one_shot() {
        OVERVIEW_REQUESTED.store(true, Ordering::Relaxed);
        assert!(
            take_overview_requested(),
            "first call should observe the flag"
        );
        assert!(
            !take_overview_requested(),
            "flag should be cleared after read"
        );
        assert!(
            !take_overview_requested(),
            "still cleared on subsequent reads"
        );
    }

    /// Every verb `slash.rs` dispatches is offered by Tab. Measured
    /// 2026-10-27: `/queue` was missing.
    #[test]
    fn every_slash_verb_is_offered_by_tab_completion() {
        let src = include_str!("slash.rs");
        let production = &src[..src.find("#[cfg(test)]").expect("test module")];
        let offered: Vec<&str> = commands().collect();
        let mut missing = Vec::new();
        for line in production.lines() {
            // A match arm: `"/a" | "/b" => …`, every pattern a quoted verb.
            let t = line.trim_start();
            let arm = match t.split_once("=>") {
                Some((arm, _)) if t.starts_with("\"/") => arm,
                _ => continue,
            };
            let verbs: Vec<&str> = arm.split('|').map(str::trim).collect();
            if verbs
                .iter()
                .all(|v| v.len() > 2 && v.starts_with("\"/") && v.ends_with('"'))
            {
                for verb in verbs.iter().map(|v| v.trim_matches('"')) {
                    if !offered.contains(&verb) {
                        missing.push(verb.to_string());
                    }
                }
            }
        }
        assert!(missing.is_empty(), "not offered by Tab: {missing:?}");
    }

    /// `expand_state()` is a non-consuming peek — it must NOT mutate state.
    /// Otherwise the navbar (which polls every redraw) would race with
    /// the keystroke handler.
    #[test]
    fn expand_state_peek_does_not_mutate() {
        set_expand_state(ExpandState::Summary);
        assert_eq!(expand_state(), ExpandState::Summary);
        assert_eq!(expand_state(), ExpandState::Summary, "still Summary");
        assert!(is_expand_active(), "still active");
        clear_expand_active();
    }
}
