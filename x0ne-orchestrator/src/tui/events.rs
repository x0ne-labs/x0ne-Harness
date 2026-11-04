//! Keyboard / event handling for tui.
//!
//! Pure routing: each event mutates `AppState` and returns whether the
//! run-loop should exit (true) or keep going (false). No I/O, no draw
//! — that's the renderer's job.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

#[cfg(test)]
use super::app::ToolStatus;
use super::app::{AppState, ConversationLine, LineKind};
use super::wire::TuiCommand;
use crate::input_queue;

/// Page scroll step (in visual rows). Big enough to feel like a
/// page-jump on a typical conversation, small enough that the
/// reader doesn't lose context.
const PAGE_SCROLL: usize = 10;
/// Mouse-wheel scroll step (smaller for finer control).
const WHEEL_SCROLL: usize = 3;

/// Scroll the conversation up by `n` visual rows (towards older
/// content). Saturates at `usize::MAX` so callers can use it for
/// "jump to top". Disables the auto-stick-to-bottom flag as soon
/// as the user takes manual control.
pub fn scroll_up(state: &mut AppState, n: usize) {
    state.scroll_offset = state.scroll_offset.saturating_add(n);
    state.auto_stick_bottom = false;
}

/// Scroll the conversation down by `n` visual rows. When the
/// offset hits 0 we re-enable auto-stick so future turns land in
/// view automatically.
pub fn scroll_down(state: &mut AppState, n: usize) {
    state.scroll_offset = state.scroll_offset.saturating_sub(n);
    if state.scroll_offset == 0 {
        state.auto_stick_bottom = true;
    }
}

/// Handle a mouse event: wheel scrolls, a left click expands the action
/// under the cursor. Drags and other buttons are ignored.
pub fn handle_mouse(ev: MouseEvent, state: &mut AppState) {
    match ev.kind {
        MouseEventKind::ScrollUp => scroll_up(state, WHEEL_SCROLL),
        MouseEventKind::ScrollDown => scroll_down(state, WHEEL_SCROLL),
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            toggle_action_under(state, ev.column, ev.row)
        }
        _ => {}
    }
}

/// Expand or collapse the action block under `(col, row)`.
///
/// The runtime keeps up to 24 000 bytes of a command's output
/// (`executor/process.rs:156`) and the collapsed view shows only the tail, so
/// expanding is what makes the rest reachable at all. Anything beyond that cap
/// was already dropped before the orchestrator saw it, and says so in the text
/// — expanding shows everything that still exists, which is not always
/// everything that was printed.
///
/// The click resolves to a conversation entry, then walks **backwards** to the
/// `action_head` that owns it. That walk is what makes the feature tolerate the
/// one-row imprecision documented on `ConvHits`: any row inside a block finds
/// the same head, so only a click exactly on a boundary can pick the neighbour.
fn toggle_action_under(state: &mut AppState, col: u16, row: u16) {
    let Some(idx) = state.conv_hits.hit(col, row) else {
        return;
    };
    // Walk back to the head of the block. `take_while` would stop at the first
    // non-head line and never reach the head above it, so this is a plain
    // reverse scan.
    let head = (0..=idx.min(state.conversation.len().saturating_sub(1)))
        .rev()
        .find(|&i| state.conversation[i].action_head);
    if let Some(i) = head {
        // Only an action carrying a result has anything more to show; a head
        // whose action never completed would expand to an empty box.
        if state.conversation[i].action_result.is_some() {
            state.conversation[i].expanded = !state.conversation[i].expanded;
        }
    }
}

/// Window in which a second Ctrl-C quits instead of stopping (CLI parity).
const DOUBLE_PRESS: std::time::Duration = std::time::Duration::from_secs(2);

/// Ctrl-C during a turn sends `Cancel`. `false` when it must quit
/// instead: no turn, a second press within [`DOUBLE_PRESS`], no runner.
fn interrupt_stops_the_turn(state: &mut AppState) -> bool {
    let now = std::time::Instant::now();
    let again = state
        .last_interrupt
        .is_some_and(|t| now.duration_since(t) < DOUBLE_PRESS);
    let running = state.agent_busy || state.turn_pending;
    if !running || again || !state.send_command(TuiCommand::Cancel) {
        return false;
    }
    state.last_interrupt = Some(now);
    state.push_line(ConversationLine::new(
        LineKind::System,
        "stopping the turn · ^C again within 2 s quits".to_string(),
    ));
    true
}

/// Returns `true` if the caller should exit the run loop.
pub fn handle_key(key: KeyEvent, state: &mut AppState) -> bool {
    // ── Shell mode = live embedded PTY ────────────────────────────────────
    //
    // While shell mode is active, every key goes straight to the
    // embedded bash via the PTY — arrows, function keys, Ctrl-C,
    // EVERYTHING. Two TUI-level exceptions: `Ctrl+\` exits shell
    // mode (returns to the agent without killing bash), and
    // `Ctrl+Alt+C` quits X0NE entirely (separate from Ctrl-C
    // which we want to forward to the foreground process inside
    // the shell, e.g. to kill `ping`).
    if state.shell_mode {
        // Quit X0NE: Ctrl+Alt+C only (so plain Ctrl-C remains for
        // killing whatever's running INSIDE the shell).
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            state.should_quit = true;
            return true;
        }
        // Return to agent mode: Ctrl+\ (file-separator). Keeps the
        // PTY alive in the background so the next `Ctrl+S` resumes
        // the same session — exactly like a tmux detach.
        if key.code == KeyCode::Char('\\') && key.modifiers == KeyModifiers::CONTROL {
            state.shell_mode = false;
            state
                .input
                .set_placeholder_text("Type a message · Tab to switch panel · Enter to send");
            return false;
        }
        // Everything else → straight to the PTY.
        if let Some(pty) = state.pty.as_mut() {
            pty.write_key(key);
        }
        return false;
    }

    // ── Shell panel owns input while it has focus ─────────────────────────
    //
    // Same PTY, same semantics as full-page shell mode, in the side panel:
    // every key goes to the shell — Tab included, because shell completion
    // matters more than panel cycling here. The two ways out are Shift+Tab
    // (previous panel) and Ctrl+\ (focus back to the agent input). Plain
    // Ctrl+C goes INTO the shell to kill whatever runs there; Ctrl+Alt+C
    // still quits x0ne.
    if !state.shell_mode
        && state.side_panel == crate::tui::app::SidePanel::Shell
        && state.focus == crate::tui::app::FocusRegion::SidePanel
        && state.pending_ask.is_none()
        && state.pending_confirm.is_none()
    {
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            state.should_quit = true;
            return true;
        }
        if key.code == KeyCode::BackTab {
            state.side_panel = state.side_panel.cycle_prev();
            return false;
        }
        if key.code == KeyCode::Char('\\') && key.modifiers == KeyModifiers::CONTROL {
            state.focus = crate::tui::app::FocusRegion::Conversation;
            return false;
        }
        // Esc hands focus straight back to the message input — the fastest
        // way from "typing in the shell" to "talking to the model". The
        // trade-off is deliberate: Esc no longer reaches programs inside the
        // PANEL shell (vim there wants the full-page Ctrl+S view, which
        // still routes Esc through).
        if key.code == KeyCode::Esc {
            state.focus = crate::tui::app::FocusRegion::Conversation;
            return false;
        }
        // Keys route to the shell only while it is ALIVE. When the child
        // exits (Ctrl+D on bash), the panel must stop swallowing input —
        // Tab cycles again, Shift+Tab leaves — and only Enter restarts a
        // fresh session. Respawning on any key was the trap: every
        // keystroke resurrected the shell and the user could never leave.
        let pty_live = state.pty.as_ref().map(|p| !p.is_exited()).unwrap_or(false);
        if pty_live {
            if let Some(pty) = state.pty.as_mut() {
                pty.write_key(key);
            }
            return false;
        }
        if key.code == KeyCode::Enter {
            ensure_pty(state);
            return false;
        }
        // Dead or never started: fall through to the global handlers.
    }

    // ── Hooks panel: ↑/↓ move, Space toggles, Esc back to input ──────────
    if state.side_panel == crate::tui::app::SidePanel::Hooks
        && state.focus == crate::tui::app::FocusRegion::SidePanel
        && state.pending_ask.is_none()
        && state.pending_confirm.is_none()
    {
        match key.code {
            KeyCode::Up => {
                state.hooks_cursor = state.hooks_cursor.saturating_sub(1);
                return false;
            }
            KeyCode::Down => {
                state.hooks_cursor =
                    (state.hooks_cursor + 1).min(state.hooks.len().saturating_sub(1));
                return false;
            }
            KeyCode::Char(' ') => {
                state.toggle_hook_at_cursor();
                return false;
            }
            KeyCode::Esc => {
                state.focus = crate::tui::app::FocusRegion::Conversation;
                return false;
            }
            _ => {}
        }
    }

    // ── F1 help overlay swallows input while visible ──────────────────────
    //
    // Any key dismisses it; Esc / F1 / ? are the canonical close keys.
    // Ctrl-C still quits even from inside help (muscle-memory parity).
    if state.show_help {
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL),
        ) {
            state.should_quit = true;
            return true;
        }
        // Any key closes — keeps the overlay frictionless.
        state.show_help = false;
        return false;
    }

    // ── Confirmation modal owns input while pending ───────────────────────
    //
    // The engine is parked on this answer, so every key goes to the security
    // prompt — except Ctrl-C/Ctrl-D, which still quit (and the runner then
    // abandons the question, which the engine reads as a denial).
    if state.pending_confirm.is_some() {
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL),
        ) {
            state.should_quit = true;
            return true;
        }
        handle_confirm_key(key, state);
        return false;
    }

    // ── AskUser modal owns input while pending ────────────────────────────
    //
    // When the LLM has asked a question, the textarea is dimmed and ALL
    // keys go through the MCQ handler — including Ctrl-C, so the user
    // can still kill the session if the agent gets stuck on a question.
    if state.pending_ask.is_some() {
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL),
        ) {
            state.should_quit = true;
            return true;
        }
        handle_ask_key(key, state);
        return false;
    }

    // ── Global shortcuts (always active) ──────────────────────────────────
    match (key.code, key.modifiers) {
        // Ctrl-C stops a running turn; idle, or pressed again within
        // 2 s, it quits, as on the CLI. Ctrl-D always quits.
        (KeyCode::Char('c'), KeyModifiers::CONTROL) if interrupt_stops_the_turn(state) => {
            return false;
        }
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            state.should_quit = true;
            return true;
        }
        // Tab — completes a slash command when the input is one; otherwise
        // cycles the side panel. Owner, 2026-08-17: « when typing / make tab
        // hook into completion, and not switch the right-hand panel ».
        (KeyCode::Tab, KeyModifiers::NONE) => {
            if complete_slash(state) {
                return false;
            }
            state.side_panel = state.side_panel.cycle_next();
            state.focus = crate::tui::app::FocusRegion::SidePanel;
            return false;
        }
        // Ctrl-B — toggle focus mode. Hides the side panel so the
        // conversation gets the full body width; press again to
        // restore. The side_panel state is preserved so cycling
        // resumes from where you left off.
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
            state.focus_mode = !state.focus_mode;
            return false;
        }
        // Ctrl-E — TOGGLE ALL action heads as a group. Smart toggle:
        //   * if ANY action is collapsed → expand all
        //   * if all already expanded → collapse all
        // The previous "only the last one" semantics felt arbitrary
        // — you couldn't see two actions side-by-side. This treats
        // expand as a global mode: "show me the work" vs "compact".
        // Each action still stays individually visible when
        // expanded; new actions land collapsed and Ctrl+E will then
        // expand them on the next press.
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            let action_heads: Vec<usize> = state
                .conversation
                .iter()
                .enumerate()
                .filter(|(_, cl)| cl.action_head)
                .map(|(i, _)| i)
                .collect();
            if action_heads.is_empty() {
                return false;
            }
            let all_expanded = action_heads.iter().all(|&i| state.conversation[i].expanded);
            let new_state = !all_expanded;
            for i in action_heads {
                state.conversation[i].expanded = new_state;
            }
            return false;
        }
        // Esc — collapse any expanded action.
        (KeyCode::Esc, _) => {
            let any_expanded = state.conversation.iter().any(|cl| cl.expanded);
            if any_expanded {
                for cl in state.conversation.iter_mut() {
                    cl.expanded = false;
                }
                return false;
            }
        }
        // Shift+Tab: plan mode on or off. The runner decides, after the
        // runtime holds the switch, and answers with `SessionMode`.
        (KeyCode::BackTab, _) => {
            state.send_command(TuiCommand::SetPlanMode { plan: None });
            return false;
        }
        // ── Conversation scroll. Page-keys are the safe surface
        // (no conflict with the input textarea). Mouse-wheel events
        // are wired in `handle_mouse` below.
        (KeyCode::PageUp, _) => {
            state.focus = crate::tui::app::FocusRegion::Conversation;
            scroll_up(state, PAGE_SCROLL);
            return false;
        }
        (KeyCode::PageDown, _) => {
            state.focus = crate::tui::app::FocusRegion::Conversation;
            scroll_down(state, PAGE_SCROLL);
            return false;
        }
        // Home / End jump to the very top / bottom of the trail.
        // `usize::MAX` saturates inside scroll_up.
        (KeyCode::Home, KeyModifiers::CONTROL) | (KeyCode::Home, KeyModifiers::NONE) => {
            scroll_up(state, usize::MAX);
            return false;
        }
        (KeyCode::End, KeyModifiers::CONTROL) | (KeyCode::End, KeyModifiers::NONE) => {
            state.scroll_offset = 0;
            state.auto_stick_bottom = true;
            return false;
        }
        // F1 / Ctrl-? — open the help overlay (centered floating card
        // with every shortcut, slash command, side panel and LLM
        // meta-action listed in one place).
        // F7 — raw/rendered toggle for the Data panel, and jump to it. The
        // legacy data pane's F7, ported with its meaning intact.
        (KeyCode::F(7), _) => {
            if state.side_panel == crate::tui::app::SidePanel::Data {
                state.data_raw = !state.data_raw;
            } else {
                state.side_panel = crate::tui::app::SidePanel::Data;
            }
            state.focus = crate::tui::app::FocusRegion::SidePanel;
            return false;
        }
        // F4/F5/F6 — one-press provider switching, ported from the legacy
        // F-key bar. The switch preserves the session: history, spend,
        // approvals, registry and runtime all survive (locked semantics).
        (KeyCode::F(4), _) => {
            state.send_command(TuiCommand::SwitchModel {
                provider: "ollama".into(),
                model: String::new(),
            });
            return false;
        }
        (KeyCode::F(5), _) => {
            state.send_command(TuiCommand::SwitchModel {
                provider: "anthropic".into(),
                model: String::new(),
            });
            return false;
        }
        (KeyCode::F(6), _) => {
            state.send_command(TuiCommand::SwitchModel {
                provider: "openai-compat".into(),
                model: String::new(),
            });
            return false;
        }
        (KeyCode::F(1), _) | (KeyCode::Char('?'), KeyModifiers::CONTROL) => {
            state.show_help = true;
            return false;
        }
        // Ctrl+S — enter shell mode. The body switches to a live
        // embedded terminal (real PTY + vt100 emulator); htop, vim,
        // ping all run inside. Ctrl+\ returns to the agent while
        // keeping the shell session alive in the background.
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
            enter_shell_mode(state);
            return false;
        }
        _ => {}
    }

    // ── Enter (no modifier) — submit; Shift-Enter — newline ──────────────
    if key.code == KeyCode::Enter && key.modifiers.is_empty() {
        return submit_input(state);
    }

    // ── ↑/↓ — bash-style history when at edge rows of textarea ──────────
    // At row 0 with ↑ → load previous entry. At last row with ↓ →
    // load next (or empty). Anywhere else, let the textarea handle
    // the key normally so multi-line cursor navigation still works.
    if matches!(key.code, KeyCode::Up | KeyCode::Down) && key.modifiers.is_empty() {
        let row = state.input.cursor().0;
        let last_row = state.input.lines().len().saturating_sub(1);
        match key.code {
            KeyCode::Up if row == 0 => {
                history_prev(state);
                return false;
            }
            KeyCode::Down if row >= last_row => {
                history_next(state);
                return false;
            }
            _ => {}
        }
    }

    // ── Everything else flows to the input zone ──────────────────────────
    // Any keystroke that mutates content also breaks us out of "I'm
    // recalling history" mode — that way a follow-up ↑ rewinds from
    // the current position, not the original one.
    state.history_cursor = None;
    state.slash_cycle = None;
    state.input.input(crossterm::event::Event::Key(key));
    false
}

/// Every slash command the TUI answers itself, for completion. The list is
/// the contract: a command missing here is one Tab cannot reach, and one
/// present here that `try_handle_local_slash` does not answer would complete
/// to a prompt sent to the model — `slash_completion_only_offers_handled_commands`
/// pins the second half.
pub(crate) const TUI_SLASH_COMMANDS: &[&str] = &[
    "/badge clear",
    "/badge watch ",
    "/build",
    "/clear",
    "/cron in ",
    "/help",
    "/model ",
    "/plan",
    "/playground ",
    "/provider ",
    "/quit",
    "/resume ",
    "/session",
    "/sessions",
    "/task add ",
    "/task clear",
    "/task done ",
    "/undo",
];

/// Tab on a single-line input that starts with `/`: complete it against
/// [`TUI_SLASH_COMMANDS`]. One candidate → written in full. Several → the
/// first is written and each further Tab shows the next (menu-complete, the
/// prefix kept in `state.slash_cycle`). None → nothing happens, and Tab
/// still does NOT reach the side panel: the operator was typing a command.
/// Returns whether the input was a slash line (i.e. Tab was consumed).
fn complete_slash(state: &mut AppState) -> bool {
    let lines = state.input.lines();
    if lines.len() != 1 || !lines[0].starts_with('/') {
        return false;
    }
    let typed = lines[0].to_string();
    // The prefix to complete on: what the operator typed before the first
    // Tab of this cycle, so cycling does not narrow itself to the candidate.
    let (prefix, next) = match state.slash_cycle.take() {
        Some((prefix, i)) => (prefix, i + 1),
        None => (typed.to_ascii_lowercase(), 0),
    };
    let candidates: Vec<&str> = TUI_SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(prefix.as_str()))
        .collect();
    if candidates.is_empty() {
        return true;
    }
    let pick = candidates[next % candidates.len()];
    replace_textarea(state, pick);
    if candidates.len() > 1 {
        state.slash_cycle = Some((prefix, next % candidates.len()));
    }
    true
}

/// Levenshtein distance, for « did you mean » on a mistyped command.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Replace the textarea contents with the given string. Helper for
/// history navigation — clears then re-inserts so the cursor lands
/// at end of content (where the user expects to keep typing).
fn replace_textarea(state: &mut AppState, text: &str) {
    state.input = crate::tui::app::styled_input();
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            state.input.insert_newline();
        }
        state.input.insert_str(line);
        first = false;
    }
}

/// ↑ — recall the previous (older) submitted user message.
/// First press from "fresh" lands on the most-recent entry.
fn history_prev(state: &mut AppState) {
    if state.input_history.is_empty() {
        return;
    }
    let new_cursor = match state.history_cursor {
        None => state.input_history.len() - 1,
        Some(0) => 0, // already at the oldest — clamp
        Some(i) => i - 1,
    };
    state.history_cursor = Some(new_cursor);
    let text = state.input_history[new_cursor].clone();
    replace_textarea(state, &text);
}

/// ↓ — recall the next (more-recent) submitted user message.
/// Going past the last entry resets to a fresh empty textarea.
fn history_next(state: &mut AppState) {
    let cursor = match state.history_cursor {
        None => return, // nothing to do — already fresh
        Some(i) => i,
    };
    let last = state.input_history.len().saturating_sub(1);
    if cursor >= last {
        // Past the end — back to fresh typing.
        state.history_cursor = None;
        replace_textarea(state, "");
        return;
    }
    let new_cursor = cursor + 1;
    state.history_cursor = Some(new_cursor);
    let text = state.input_history[new_cursor].clone();
    replace_textarea(state, &text);
}

/// Insert pasted text into the input textarea verbatim. Splits on
/// `\n` (and the rare `\r\n`) so each line lands as a separate
/// textarea row instead of triggering Enter-submit per newline. The
/// jsonl audit showed multi-line prompts being chopped into N
/// separate user_input turns — this branch is the fix.
pub fn handle_paste(text: &str, state: &mut AppState) {
    // Normalise line endings — crossterm passes CRLF on Windows
    // terminals; we want `\n` so the textarea inserts a single row
    // break per newline.
    let normalised = text.replace("\r\n", "\n");
    let mut first = true;
    for line in normalised.split('\n') {
        if !first {
            state.input.insert_newline();
        }
        state.input.insert_str(line);
        first = false;
    }
}

/// Take the current input contents, decide what to do with them:
///   * empty → no-op
///   * starts with `/` → slash command (handled inline, stage 1 echoes
///     the slash verbatim; stage 2 wires the dispatcher)
///   * otherwise → either queue (agent busy) or simulate-submit (idle)
///
/// Always clears the input buffer afterwards.
fn submit_input(state: &mut AppState) -> bool {
    let raw: String = state.input.lines().join("\n");
    let trimmed = raw.trim().to_string();
    state.input = crate::tui::app::styled_input();

    if trimmed.is_empty() {
        return false;
    }

    // Push to bash-style history (dedup against the immediately
    // preceding entry — repeated submits don't pollute) and reset
    // the cursor so the next ↑ rewinds from the fresh "now".
    if state.input_history.last().map(String::as_str) != Some(trimmed.as_str()) {
        state.input_history.push(trimmed.clone());
    }
    state.history_cursor = None;

    // Special case: /quit and friends short-circuit.
    let lowered = trimmed.to_ascii_lowercase();
    if matches!(lowered.as_str(), "/quit" | "/exit" | "/q" | ":q") {
        state.should_quit = true;
        return true;
    }

    // ── User-driven Tasks / Cron slash commands ──────────────────────
    // These mirror the LLM-side TaskAdd / TaskDone / CronSchedule
    // primitives so a human can populate the panels manually. They
    // never go to the LLM — they're handled locally and echoed as
    // System lines.
    if let Some(handled) = try_handle_local_slash(state, &trimmed) {
        return handled;
    }

    // `/playground [N]` is a prompt in disguise: what is echoed is the short
    // slash the operator typed, what is sent is the shared prompt text.
    let (echo, trimmed) = match crate::playground::parse(&trimmed) {
        Some(req) => {
            if req.reset {
                match crate::playground::reset_playground() {
                    Ok(msg) => state.push_line(ConversationLine::new(LineKind::System, msg)),
                    Err(e) => {
                        state.push_line(ConversationLine::new(
                            LineKind::Error,
                            format!("playground not reset — {e} (use `/playground keep …` to run on the current state)"),
                        ));
                        return false;
                    }
                }
            }
            (trimmed.clone(), req.prompt())
        }
        None => (trimmed.clone(), trimmed),
    };
    // Echo the user line into the conversation regardless of where it goes.
    state.push_line(ConversationLine::new(LineKind::User, echo));

    if state.agent_busy {
        // Push to the process-wide queue so the runner can pick it up
        // when the current turn ends. The drainer in app::apply_event
        // pops the front off after `AssistantEnd`, so chains flow
        // automatically without the user pressing Enter again.
        input_queue::push_back(trimmed);
        state.push_line(ConversationLine::new(
            LineKind::System,
            format!("queued · agent is busy ({} pending)", state.queue_len()),
        ));
    } else if state.cmd_tx.is_some() {
        // Detect `@file.png` style image references — if present,
        // route via the multimodal variant so the runner loads +
        // attaches them as base64 to the next Message. Bare text
        // without image refs takes the fast UserInput path.
        //
        // ── Shell-mode context: prepend the shell trail so the LLM
        // ── sees what the user just ran on the host. We DRAIN the
        // ── trail after sharing so the next turn only carries fresh
        // ── activity; the full history stays in the shell panel +
        // ── jsonl for audit.
        let prefixed_text = if let Some(note) = state.shell_context_note() {
            let combined = format!("{}\n{}", note, trimmed);
            state.shell_history.clear();
            combined
        } else {
            trimmed.clone()
        };
        let images = super::app::extract_image_refs(&trimmed);
        let cmd = if images.is_empty() {
            TuiCommand::UserInput(prefixed_text)
        } else {
            // Echo a marker line so the user sees that X0NE will
            // actually look at the file (otherwise they wouldn't know
            // it parsed).
            state.push_line(ConversationLine::new(
                LineKind::System,
                format!(
                    "🖼 attached {} image{} for vision: {}",
                    images.len(),
                    if images.len() == 1 { "" } else { "s" },
                    images
                        .iter()
                        .map(|p| std::path::Path::new(p)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| p.clone()))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ));
            TuiCommand::UserInputWithImages {
                text: prefixed_text,
                image_paths: images,
            }
        };
        // Runner is alive — forward the input. The runner will reply
        // with AssistantStart / Token* / AssistantEnd events and the
        // drainer translates them back into ConversationLines.
        if !state.send_command(cmd) {
            // Channel full or closed. Surface as an error line; the
            // user can try again.
            state.push_line(ConversationLine::new(
                LineKind::Error,
                "runner queue is full — try again in a moment".into(),
            ));
        }
    } else {
        // No runner — boot failed earlier. Tell the user instead of
        // silently dropping their message.
        state.push_line(ConversationLine::new(
            LineKind::Error,
            "runner not connected · cannot send messages".into(),
        ));
    }
    false
}

/// User-driven slash commands handled entirely inside the TUI (no
/// LLM round-trip). Returns `Some(true)` to mean "submit consumed,
/// quit", `Some(false)` to mean "consumed but stay running", `None`
/// to fall through to the normal LLM/queue dispatch.
///
/// Supported:
///   /task add <title>       — adds a task to the Tasks panel
///   /task done <id>         — marks task complete
///   /task clear             — wipes all tasks
///   /cron in <secs> <label> — schedule a passive reminder
///   /cron in <secs> <label> -> <prompt>   — self-trigger
///   /cron cancel <id>       — cancel a pending cron
fn try_handle_local_slash(state: &mut AppState, line: &str) -> Option<bool> {
    use super::wire::AgentEvent;
    let lower = line.to_ascii_lowercase();
    let echo_sys = |s: &mut AppState, msg: String| {
        s.push_line(ConversationLine::new(LineKind::System, msg));
    };

    // /task add <title>
    if let Some(rest) = lower
        .strip_prefix("/task add ")
        .or_else(|| lower.strip_prefix("/task add"))
    {
        let title_lower_len = rest.trim().len();
        let title = line[line.len() - title_lower_len..].trim().to_string();
        if title.is_empty() {
            echo_sys(state, "usage: /task add <title>".into());
            return Some(false);
        }
        let id = state.next_task_id;
        state.next_task_id += 1;
        state.apply_event(AgentEvent::TaskAdded {
            id,
            title: title.clone(),
        });
        echo_sys(state, format!("task #{} added · {}", id, title));
        return Some(false);
    }
    // /task done <id>
    if let Some(rest) = lower.strip_prefix("/task done ") {
        match rest.trim().parse::<u32>() {
            Ok(id) => {
                state.apply_event(AgentEvent::TaskCompleted { id });
                echo_sys(state, format!("task #{} marked done", id));
            }
            Err(_) => echo_sys(state, "usage: /task done <id>".into()),
        }
        return Some(false);
    }
    // /task clear
    if lower == "/task clear" {
        state.apply_event(AgentEvent::TasksCleared);
        echo_sys(state, "tasks cleared".into());
        return Some(false);
    }
    // /help / /? — pop the help overlay (same as F1).
    // /model [name] · /provider <name> [model] — the session backend switch.
    // Without these the words fell through to the LLM as a prompt, which is
    // how a bare /model got an LLM answer instead of a picker.
    if lower == "/model" || lower == "/provider" {
        // No argument: the same picker as `/sessions`, over the providers.
        let current = state.model_label.clone();
        let rows = vec![
            (
                "anthropic".to_string(),
                Some("Anthropic API · F5".to_string()),
            ),
            ("ollama".to_string(), Some("local Ollama · F4".to_string())),
            (
                "openai-compat".to_string(),
                Some("any OpenAI-compatible endpoint (vLLM, …) · F6".to_string()),
            ),
        ];
        let hint = format!("current · {current} — Enter switches provider (its configured model), /model <name> sets a model");
        state.pending_ask = Some(crate::tui::app::AskState::local(
            "Switch to which provider?",
            Some(&hint),
            rows,
            crate::tui::app::PickerAction::SwitchModel,
        ));
        return Some(false);
    }
    if let Some(rest) = line.strip_prefix("/model ") {
        let model = rest.trim().to_string();
        if !model.is_empty() {
            // Empty provider = keep the current one; the runner resolves it.
            state.send_command(TuiCommand::SwitchModel {
                provider: String::new(),
                model,
            });
            return Some(false);
        }
    }
    if let Some(rest) = line.strip_prefix("/provider ") {
        let mut parts = rest.split_whitespace();
        if let Some(provider) = parts.next() {
            state.send_command(TuiCommand::SwitchModel {
                provider: provider.to_string(),
                model: parts.next().unwrap_or("").to_string(),
            });
            return Some(false);
        }
    }

    // ── Sessions ──────────────────────────────────────────────────────
    // The CLI answers these in `slash.rs`; the TUI has to answer them here or
    // they go to the model as prose — which is what happened on 2026-08-17
    // (`/session` became a turn, the model read its own journal with `cat`).
    if lower == "/session" {
        echo_sys(
            state,
            format!(
                "session {} · {} · {} tokens · ${:.2} · journal ~/.x0ne/sessions/{}.jsonl",
                state.session_id,
                state.model_label,
                state.tokens_used,
                state.cost_usd,
                state.session_id
            ),
        );
        return Some(false);
    }
    if lower == "/sessions" || lower == "/sessions --all" {
        open_sessions_picker(state, lower.ends_with("--all"));
        return Some(false);
    }
    if lower == "/resume" || lower.starts_with("/resume ") {
        let id = line["/resume".len()..].trim();
        if id.is_empty() {
            open_sessions_picker(state, false);
        } else {
            apply_picker_choice(state, crate::tui::app::PickerAction::ResumeSession, id);
        }
        return Some(false);
    }
    // A mistyped command must not become a prompt: `/sesssions` went to the
    // model, which ran `ls ~/.x0ne/sessions` for it. Only a first token close
    // to a known command is caught — a path (`/etc/hosts …`) still flows.
    if lower == "/undo" {
        // The runner holds the session, its undo journal and its bridge.
        // Nothing is echoed as a User line: this is not a prompt.
        state.send_command(TuiCommand::Undo);
        return Some(false);
    }
    if let Some(first) = lower.split_whitespace().next() {
        let heads = || {
            TUI_SLASH_COMMANDS
                .iter()
                .filter_map(|c| c.split_whitespace().next())
        };
        let known = heads().any(|h| h == first);
        if !known && first.len() > 2 {
            let closest = heads().min_by_key(|h| edit_distance(first, h));
            if let Some(c) = closest {
                if edit_distance(first, c) <= 2 {
                    echo_sys(
                        state,
                        format!("unknown command {first} — did you mean {c} ? (/help lists them)"),
                    );
                    return Some(false);
                }
            }
        }
    }

    if lower == "/plan" || lower == "/build" {
        state.send_command(TuiCommand::SetPlanMode {
            plan: Some(lower == "/plan"),
        });
        return Some(false);
    }

    if lower == "/clear" {
        // Display first, then the session: the canonical transcript and the
        // standing approvals are the runner's to clear (locked /clear
        // semantics), the visible scrollback is ours.
        state.conversation.clear();
        state.scroll_offset = 0;
        state.send_command(TuiCommand::ClearSession);
        state.push_line(ConversationLine::new(
            LineKind::System,
            "conversation cleared · standing approvals revoked".into(),
        ));
        return Some(false);
    }

    if matches!(lower.as_str(), "/help" | "/?" | "/h") {
        state.show_help = true;
        return Some(false);
    }
    // ── Live badge widget ────────────────────────────────────────────
    //   /badge watch <secs> <cmd…>  → run cmd every N secs, paint output
    //   /badge clear                → stop the watcher
    if let Some(rest) = lower.strip_prefix("/badge watch ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let secs_str = parts.next().unwrap_or("").trim();
        let cmd_tail = parts.next().unwrap_or("").trim();
        let secs: u64 = match secs_str.parse() {
            Ok(n) => n,
            Err(_) => {
                echo_sys(state, "usage: /badge watch <secs> <cmd>".into());
                return Some(false);
            }
        };
        if cmd_tail.is_empty() {
            echo_sys(state, "usage: /badge watch <secs> <cmd>".into());
            return Some(false);
        }
        let cmd = line[line.len() - cmd_tail.len()..].trim().to_string();
        let label = cmd.split_whitespace().next().unwrap_or("watch").to_string();
        let _ = state.send_command(TuiCommand::BadgeWatch {
            cmd: cmd.clone(),
            interval_secs: secs,
            label,
        });
        echo_sys(
            state,
            format!(
                "badge watching every {}s · {}",
                secs,
                super::app::trim_inline(&cmd, 50)
            ),
        );
        return Some(false);
    }
    if lower == "/badge clear" {
        let _ = state.send_command(TuiCommand::BadgeWatchClear);
        echo_sys(state, "badge watch cleared".into());
        return Some(false);
    }
    // /cron in <secs> <label> [-> <prompt>]
    if let Some(rest) = lower.strip_prefix("/cron in ") {
        // Parse <secs> then everything else as label (+ optional -> prompt).
        let mut parts = rest.splitn(2, char::is_whitespace);
        let secs_str = parts.next().unwrap_or("").trim();
        let tail = parts.next().unwrap_or("").trim();
        let secs: u64 = match secs_str.parse() {
            Ok(n) => n,
            Err(_) => {
                echo_sys(state, "usage: /cron in <secs> <label> [-> <prompt>]".into());
                return Some(false);
            }
        };
        // Recover the original-case label/prompt from `line` by
        // taking the same suffix length.
        let tail_full = line[line.len() - tail.len()..].to_string();
        let (label, prompt) = match tail_full.split_once("->") {
            Some((l, p)) => (
                l.trim().to_string(),
                Some(p.trim().to_string()).filter(|s| !s.is_empty()),
            ),
            None => (tail_full.trim().to_string(), None),
        };
        if label.is_empty() {
            echo_sys(state, "usage: /cron in <secs> <label> [-> <prompt>]".into());
            return Some(false);
        }
        let id = state.next_cron_id;
        state.next_cron_id += 1;
        let due_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() + secs)
            .unwrap_or(secs);
        state.apply_event(AgentEvent::CronScheduled {
            id,
            label: label.clone(),
            due_at_unix,
            trigger_prompt: prompt.clone(),
        });
        let detail = if prompt.is_some() {
            " · self-trigger"
        } else {
            " · reminder"
        };
        echo_sys(
            state,
            format!("cron #{} in {}s · {}{}", id, secs, label, detail),
        );
        // The user-side /cron does NOT actually wire into the runner's
        // crons map (that lives in the runner task). It's a panel-
        // only affordance for now — the runner cron ticker won't
        // fire it. For LLM-side automation, the agent uses
        // CronSchedule, which DOES persist to the runner.
        return Some(false);
    }
    // /cron cancel <id>
    if let Some(rest) = lower.strip_prefix("/cron cancel ") {
        match rest.trim().parse::<u32>() {
            Ok(id) => {
                state.apply_event(AgentEvent::CronCancelled { id });
                echo_sys(state, format!("cron #{} cancelled", id));
            }
            Err(_) => echo_sys(state, "usage: /cron cancel <id>".into()),
        }
        return Some(false);
    }
    None
}

// ── Shell mode helpers ───────────────────────────────────────────────────
//
// Shell mode hosts a real embedded terminal inside the TUI: PTY +
// vt100 emulator. The first `Ctrl+S` press spawns a `bash -i`
// session in a PTY; from there every keystroke goes to bash and
// the parser's screen grid is rendered as ratatui cells each tick.
// `Ctrl+\\` returns to the agent without killing the shell, so the
// next `Ctrl+S` resumes the same session (tmux-style detach).

fn enter_shell_mode(state: &mut AppState) {
    state.shell_mode = true;
    state.input.set_placeholder_text("");
    ensure_pty(state);
}

/// Spawn the PTY lazily — most sessions never open a shell — and reuse the
/// same session afterwards, whether it is viewed full-page (`Ctrl+S`) or in
/// the Shell side panel. One PTY, two viewports.
fn ensure_pty(state: &mut AppState) {
    if state.pty.is_none() || state.pty.as_ref().map(|p| p.is_exited()).unwrap_or(false) {
        // Pick a reasonable initial size; the render loop resizes
        // on the next frame once it knows the actual area.
        match super::pty_term::PtyTerm::spawn(&state.shell_cwd, 24, 80) {
            Ok(term) => {
                state.pty = Some(term);
                state.push_line(ConversationLine::new(
                    super::app::LineKind::System,
                    format!(
                        " 🐚  shell · {} · ^\\ returns to agent · ^Alt+C quits x0ne",
                        state.shell_cwd.display()
                    ),
                ));
            }
            Err(e) => {
                state.shell_mode = false;
                state.push_line(ConversationLine::new(
                    super::app::LineKind::Error,
                    format!("shell spawn failed: {}", e),
                ));
            }
        }
    }
}

/// Handle a key while an [`AskState`] is active. Maps:
///   * Up/Down/j/k        → move the caret (cyclic)
///   * Space (multi)      → toggle the focused option
///   * Enter (single)     → pick + submit the focused option
///   * Enter (multi)      → submit the current selection (auto-selects
///                          the focused row if nothing selected yet)
///   * Esc                → cancel — the runner gets an "abandoned"
///                          answer and bails the turn gracefully.
///
/// Keys for the confirmation modal. Up/Down (or j/k) move between the
/// choices; Enter submits the focused one; `1`/`2`/`3` answer directly with
/// the same meanings the CLI prompt prints; Esc and `n` deny.
///
/// `2` is accepted only when the authority offered a standing grant — the
/// choices list simply does not contain it otherwise, so a stray keypress
/// cannot widen an approval nobody was shown.
fn handle_confirm_key(key: KeyEvent, state: &mut AppState) {
    let Some(confirm) = state.pending_confirm.as_mut() else {
        return;
    };
    let choices = confirm.choices();
    let submit = |state: &mut AppState, approved: bool, always: bool| {
        let Some(confirm) = state.pending_confirm.take() else {
            return;
        };
        state.send_command(TuiCommand::ConfirmationAnswer {
            id: confirm.id,
            approved,
            always,
        });
        // What the transcript needs is what the next rows will NOT say.
        //
        // A plain approval says nothing new: the modal named the action, and
        // the action runs immediately and prints its own `↳ ReadFile ✓`. The
        // old `⏵ ReadFile · allowed once` sat between those two, repeating
        // both — one grey row per approval, on the most frequent answer of
        // the three.
        //
        // The other two answers each leave a gap only this line can fill:
        //
        // - a denial produces no action row at all, so without a line the
        //   screen does not move and the keypress looks lost;
        // - a session grant changes standing state — the next N prompts will
        //   not appear — and nothing else on screen would ever say so.
        match (approved, always) {
            (true, false) => {}
            (false, _) => state.push_line(crate::tui::app::ConversationLine::new(
                crate::tui::app::LineKind::Error,
                format!("{} denied", confirm.action_type),
            )),
            (true, true) => state.push_line(crate::tui::app::ConversationLine::new(
                crate::tui::app::LineKind::System,
                format!(
                    "{} allowed for the rest of this session",
                    confirm.action_type
                ),
            )),
        }
    };
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
            confirm.cursor = confirm.cursor.saturating_sub(1);
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
            confirm.cursor = (confirm.cursor + 1).min(choices.len() - 1);
        }
        (KeyCode::Enter, _) => {
            let picked = choices[confirm.cursor.min(choices.len() - 1)].0;
            submit(state, picked != 3, picked == 2);
        }
        (KeyCode::Char('1'), _) => submit(state, true, false),
        (KeyCode::Char('2'), _) if choices.iter().any(|(n, _)| *n == 2) => {
            submit(state, true, true)
        }
        (KeyCode::Char('3'), _) | (KeyCode::Char('n'), _) | (KeyCode::Esc, _) => {
            submit(state, false, false)
        }
        _ => {}
    }
}

/// Submitting clears `pending_ask` and answers whoever asked: the model gets a
/// [`TuiCommand::AskAnswer`], the TUI's own pickers get
/// [`apply_picker_choice`]. Printable characters narrow the list (the
/// filter), Backspace widens it, Up/Down move over the VISIBLE rows,
/// Space toggles in multi-select, Enter picks, Esc cancels — Esc answers the
/// model with `cancelled = true`, never silence: the runner is blocked on the
/// answer.
pub fn handle_ask_key(key: KeyEvent, state: &mut AppState) {
    let Some(ask) = state.pending_ask.as_mut() else {
        return;
    };
    if ask.options.is_empty() {
        // Defensive: an empty option list would deadlock the user.
        // Cancel immediately so the runner sees `cancelled = true`.
        let (id, sink) = (ask.id.clone(), ask.sink.clone());
        state.pending_ask = None;
        answer_cancel(state, id, sink);
        return;
    }
    let visible = ask.visible();
    // Keep the cursor on a visible row: it indexes `visible`, not `options`.
    if visible.is_empty() {
        ask.cursor = 0;
    } else {
        ask.cursor = ask.cursor.min(visible.len() - 1);
    }
    let n = visible.len();
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => {
            if n > 0 {
                ask.cursor = if ask.cursor == 0 {
                    n - 1
                } else {
                    ask.cursor - 1
                };
            }
        }
        (KeyCode::Down, _) => {
            if n > 0 {
                ask.cursor = (ask.cursor + 1) % n;
            }
        }
        (KeyCode::PageUp, _) => ask.cursor = ask.cursor.saturating_sub(10),
        (KeyCode::PageDown, _) => {
            if n > 0 {
                ask.cursor = (ask.cursor + 10).min(n - 1);
            }
        }
        (KeyCode::Char(' '), _) if ask.multi_select => {
            if let Some(&i) = visible.get(ask.cursor) {
                ask.options[i].selected = !ask.options[i].selected;
            }
        }
        (KeyCode::Backspace, _) => {
            ask.filter.pop();
            ask.cursor = 0;
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !c.is_control() => {
            ask.filter.push(c);
            ask.cursor = 0;
        }
        (KeyCode::Enter, _) => {
            if n == 0 {
                return; // nothing to pick under this filter — widen it first
            }
            let (id, sink, multi) = (ask.id.clone(), ask.sink.clone(), ask.multi_select);
            let focused = ask.options[visible[ask.cursor]].option.label.clone();
            let mut selected: Vec<String> = if multi {
                ask.options
                    .iter()
                    .filter(|r| r.selected)
                    .map(|r| r.option.label.clone())
                    .collect()
            } else {
                Vec::new()
            };
            // Single-select: the focused row. Multi-select with nothing
            // checked: the focused row counts as the implicit pick — saves a
            // Space + Enter for the common case.
            if selected.is_empty() {
                selected.push(focused);
            }
            state.pending_ask = None;
            match sink {
                crate::tui::app::AskSink::Model => {
                    let _ = state.send_command(TuiCommand::AskAnswer {
                        id,
                        cancelled: false,
                        selected,
                        custom_text: None,
                    });
                }
                crate::tui::app::AskSink::Local(action) => {
                    apply_picker_choice(state, action, &selected[0]);
                }
            }
        }
        (KeyCode::Esc, _) => {
            let (id, sink) = (ask.id.clone(), ask.sink.clone());
            state.pending_ask = None;
            answer_cancel(state, id, sink);
        }
        _ => {}
    }
}

/// Esc / empty list: the model is told the question was abandoned; a local
/// picker just closes.
fn answer_cancel(state: &mut AppState, id: String, sink: crate::tui::app::AskSink) {
    if sink == crate::tui::app::AskSink::Model {
        let _ = state.send_command(TuiCommand::AskAnswer {
            id,
            cancelled: true,
            selected: Vec::new(),
            custom_text: None,
        });
    }
}

/// What a row picked in one of the TUI's own pickers does. Exhaustive: a
/// new [`PickerAction`] does not compile until it is decided here.
pub(crate) fn apply_picker_choice(
    state: &mut AppState,
    action: crate::tui::app::PickerAction,
    label: &str,
) {
    use crate::tui::app::PickerAction;
    match action {
        PickerAction::ResumeSession => {
            state.push_line(ConversationLine::new(
                LineKind::System,
                format!("resuming session {label} …"),
            ));
            let _ = state.send_command(TuiCommand::ResumeSession {
                id: label.to_string(),
            });
        }
        PickerAction::SwitchModel => {
            let (provider, model) = match label.split_once('/') {
                Some((p, m)) => (p.to_string(), m.to_string()),
                None => (String::new(), label.to_string()),
            };
            let _ = state.send_command(TuiCommand::SwitchModel { provider, model });
        }
    }
}

/// Open the sessions picker — `/sessions`, and `/resume` without an id.
/// Rows: id · turns · model · title or first prompt (already sanitized by
/// the store). This project's sessions, or every one with `all`.
pub(crate) fn open_sessions_picker(state: &mut AppState, all: bool) {
    use crate::session_store as store;
    let project = if all { None } else { store::Project::current() };
    let list = store::list_for(&store::sessions_dir(), project.as_ref()).rows;
    let rows: Vec<(String, Option<String>)> = list
        .iter()
        .filter(|s| s.id != state.session_id)
        .map(|s| {
            let detail = format!(
                "{} turn{} · {} · {}",
                s.turns,
                if s.turns == 1 { "" } else { "s" },
                s.model.as_deref().unwrap_or("?"),
                super::app::trim_inline(
                    s.title
                        .as_deref()
                        .or(s.first_prompt.as_deref())
                        .unwrap_or("—"),
                    60
                )
            );
            (s.id.clone(), Some(detail))
        })
        .collect();
    if rows.is_empty() {
        state.push_line(ConversationLine::new(
            LineKind::System,
            if all {
                "no other session in ~/.x0ne/sessions".into()
            } else {
                "no other session in this project (/sessions --all: every project)".into()
            },
        ));
        return;
    }
    let hint = format!(
        "{} sessions · type to filter · Enter resumes · Esc closes",
        rows.len()
    );
    state.pending_ask = Some(crate::tui::app::AskState::local(
        "Resume which session?",
        Some(&hint),
        rows,
        crate::tui::app::PickerAction::ResumeSession,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::SidePanel;
    use crossterm::event::{KeyEventKind, KeyEventState};

    /// Build a state whose conversation is one action block: a head carrying
    /// a result, then two body rows, with a hit map that says so.
    fn state_with_one_action_block() -> AppState {
        let mut s = AppState::new();
        s.conversation.clear();
        let mut head = crate::tui::app::ConversationLine::tool(
            "  ▸ exec · RunCommand".into(),
            crate::tui::app::ToolStatus::Ok,
        );
        head.action_head = true;
        head.action_input = Some(serde_json::json!({"type": "RunCommand", "command": "ls"}));
        head.action_result = Some(serde_json::json!({"success": true}));
        s.conversation.push(head);
        for body in ["  │  $ ls", "  └  ok"] {
            s.conversation.push(crate::tui::app::ConversationLine::tool(
                body.into(),
                crate::tui::app::ToolStatus::Ok,
            ));
        }
        s.conv_hits = crate::tui::render::ConvHits {
            top: 5,
            left: 2,
            width: 60,
            rows: vec![0, 1, 2],
        };
        s
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Clicking any row of an action expands that action — including the
    /// output rows, which is the whole point: the collapsed view shows only
    /// the tail of a capped capture, and this is how the rest is reached.
    #[test]
    fn a_click_anywhere_in_an_action_expands_it() {
        for row in 5..=7 {
            let mut s = state_with_one_action_block();
            handle_mouse(click(10, row), &mut s);
            assert!(
                s.conversation[0].expanded,
                "a click on screen row {row} must expand the block it belongs to"
            );
        }
    }

    /// The toggle goes both ways, so a second click closes it again.
    #[test]
    fn clicking_an_expanded_action_collapses_it() {
        let mut s = state_with_one_action_block();
        handle_mouse(click(10, 6), &mut s);
        assert!(s.conversation[0].expanded);
        handle_mouse(click(10, 6), &mut s);
        assert!(
            !s.conversation[0].expanded,
            "the second click must collapse"
        );
    }

    /// A click outside the conversation panel resolves to nothing. Without
    /// the bounds check a click on the side panel or the input bar would
    /// expand whatever action happened to share that row index.
    #[test]
    fn a_click_outside_the_conversation_panel_does_nothing() {
        for (col, row) in [(0, 6), (99, 6), (10, 4), (10, 40)] {
            let mut s = state_with_one_action_block();
            handle_mouse(click(col, row), &mut s);
            assert!(
                !s.conversation[0].expanded,
                "({col},{row}) is outside the panel and must not expand anything"
            );
        }
    }

    /// An action with no result has nothing more to show, so expanding it
    /// would open an empty box. This is reachable in practice: `ToolStart`
    /// arms a block that a cancellation or a denial may never complete.
    #[test]
    fn an_action_without_a_result_does_not_expand() {
        let mut s = state_with_one_action_block();
        s.conversation[0].action_result = None;
        handle_mouse(click(10, 5), &mut s);
        assert!(!s.conversation[0].expanded);
    }

    /// The wheel must keep scrolling. Adding a click arm to the same match
    /// is exactly where a stray reordering would swallow the wheel.
    #[test]
    fn the_wheel_still_scrolls() {
        let mut s = state_with_one_action_block();
        let before = s.scroll_offset;
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 6,
                modifiers: KeyModifiers::NONE,
            },
            &mut s,
        );
        assert!(s.scroll_offset > before, "the wheel must still scroll up");
    }

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn ctrl_e_expands_all_action_heads_at_once() {
        // Seed three action heads. First Ctrl+E → all expanded.
        // Second Ctrl+E → all collapsed.
        let mut s = AppState::new();
        for _ in 0..3 {
            let mut cl = ConversationLine::tool("X".into(), ToolStatus::Ok);
            cl.action_head = true;
            cl.action_input = Some(serde_json::json!({"type":"X"}));
            cl.action_result = Some(serde_json::json!({"success":true}));
            s.conversation.push(cl);
        }
        // Round 1 — none expanded → expand all.
        handle_key(k(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut s);
        assert!(
            s.conversation
                .iter()
                .filter(|c| c.action_head)
                .all(|c| c.expanded),
            "all heads should be expanded after first Ctrl+E"
        );
        // Round 2 — all expanded → collapse all.
        handle_key(k(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut s);
        assert!(
            s.conversation
                .iter()
                .filter(|c| c.action_head)
                .all(|c| !c.expanded),
            "all heads should be collapsed after second Ctrl+E"
        );
    }

    #[test]
    fn ctrl_e_with_mixed_state_expands_everything() {
        // If SOME are expanded and SOME are not, Ctrl+E should
        // expand them all (not collapse the already-expanded). The
        // intent is "show me the work", not "flip each one".
        let mut s = AppState::new();
        for i in 0..3 {
            let mut cl = ConversationLine::tool(format!("X{}", i), ToolStatus::Ok);
            cl.action_head = true;
            cl.expanded = i == 0; // first one already expanded
            s.conversation.push(cl);
        }
        handle_key(k(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut s);
        // After the press, ALL three are expanded.
        assert!(s
            .conversation
            .iter()
            .filter(|c| c.action_head)
            .all(|c| c.expanded));
    }

    #[test]
    fn ctrl_b_toggles_focus_mode() {
        // Press once → on. Press again → off. State is preserved
        // across other key events.
        let mut s = AppState::new();
        assert!(!s.focus_mode);
        handle_key(k(KeyCode::Char('b'), KeyModifiers::CONTROL), &mut s);
        assert!(s.focus_mode);
        handle_key(k(KeyCode::Char('b'), KeyModifiers::CONTROL), &mut s);
        assert!(!s.focus_mode);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut s = AppState::new();
        let quit = handle_key(k(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut s);
        assert!(quit);
        assert!(s.should_quit);
    }

    /// During a turn Ctrl-C sends `Cancel` and keeps the TUI; a second
    /// press within 2 s quits. Ctrl-D still quits at once.
    #[test]
    fn ctrl_c_during_a_turn_stops_it_and_a_second_press_quits() {
        let mut s = AppState::new();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(cmd_tx, evt_rx);
        s.agent_busy = true;
        let ctrl_c = k(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!handle_key(ctrl_c, &mut s), "the first press quit");
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Cancel)));
        assert!(!s.should_quit);
        assert!(handle_key(ctrl_c, &mut s), "the second press did not quit");
        assert!(cmd_rx.try_recv().is_err(), "a second Cancel was sent");

        let mut s = AppState::new();
        s.agent_busy = true;
        assert!(
            handle_key(ctrl_c, &mut s),
            "no runner: nothing to stop, so quit"
        );
        let mut s = AppState::new();
        s.agent_busy = true;
        assert!(handle_key(
            k(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut s
        ));
    }

    /// Right after Enter the turn runs, though its `AssistantStart` is
    /// not drained yet; Ctrl-C then stops it. Measured 2026-10-24: 3 of 3
    /// end-to-end runs quit the TUI in that gap.
    #[test]
    fn ctrl_c_right_after_enter_stops_the_turn() {
        let mut s = AppState::new();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(cmd_tx, evt_rx);
        s.input.insert_str("hello");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::UserInput(_))));
        assert!(!s.agent_busy && s.turn_pending);
        assert!(!handle_key(
            k(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut s
        ));
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Cancel)));
        // Not `AssistantEnd`: it pops the process-wide input queue that
        // app.rs's tests fill under their own lock.
        s.apply_event(AgentEvent::AssistantStart);
        assert!(s.agent_busy && !s.turn_pending);
    }

    #[test]
    fn tab_cycles_panel() {
        let mut s = AppState::new();
        assert_eq!(s.side_panel, SidePanel::Conversation);
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.side_panel, SidePanel::Queue);
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_ne!(s.side_panel, SidePanel::Queue);
    }

    #[test]
    fn empty_submit_is_noop() {
        let mut s = AppState::new();
        let before = s.conversation.len();
        let quit = handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert!(!quit);
        assert_eq!(s.conversation.len(), before);
    }

    #[test]
    fn slash_quit_short_circuits() {
        let mut s = AppState::new();
        s.input.insert_str("/quit");
        let quit = handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert!(quit);
        assert!(s.should_quit);
    }

    #[test]
    fn submit_when_runner_missing_emits_error() {
        // No channels attached → the user's message can't go anywhere.
        // We expect a User line followed by an Error line so the user
        // sees both what they sent and why it failed.
        let mut s = AppState::new();
        s.input.insert_str("hello");
        let before = s.conversation.len();
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert_eq!(s.conversation.len(), before + 2);
        let last_two = &s.conversation[s.conversation.len() - 2..];
        assert_eq!(last_two[0].kind, LineKind::User);
        assert_eq!(last_two[1].kind, LineKind::Error);
    }

    #[test]
    fn submit_when_runner_attached_sends_command() {
        // With channels attached, submit_input should forward the
        // message via cmd_tx instead of stubbing in the conversation.
        use crate::tui::wire::TuiCommand;
        use tokio::sync::mpsc;

        let mut s = AppState::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel();
        s.attach_channels(cmd_tx, evt_rx);

        s.input.insert_str("refactor the auth");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);

        // The User line should be in the conversation, no Error.
        let last = s.conversation.last().unwrap();
        assert_eq!(last.kind, LineKind::User);
        assert_eq!(last.text, "refactor the auth");

        // The command should be on the channel ready to be consumed.
        let cmd = cmd_rx.try_recv().expect("command was not sent");
        match cmd {
            TuiCommand::UserInput(s) => assert_eq!(s, "refactor the auth"),
            other => panic!("wrong command variant: {:?}", other),
        }
    }

    #[test]
    fn paste_with_newlines_lands_as_one_multi_row_input_not_n_submits() {
        // The bug we saw in the jsonl audit: pasting a multi-line
        // prompt chopped it into N separate user_input turns. With
        // bracketed paste + handle_paste, the whole blob lands in
        // the textarea as a single composition — exactly N rows, no
        // submits, ready for the user to press Enter once.
        let mut s = AppState::new();
        handle_paste("line one\nline two\nline three", &mut s);
        let lines = s.input.lines();
        assert_eq!(
            lines.len(),
            3,
            "expected 3 rows in textarea; got {:?}",
            lines
        );
        assert_eq!(lines[0], "line one");
        assert_eq!(lines[1], "line two");
        assert_eq!(lines[2], "line three");
    }

    #[test]
    fn paste_normalises_crlf_to_lf() {
        // Windows / some terminals send CRLF. We collapse those so
        // we don't end up with stray `\r` chars at end-of-row.
        let mut s = AppState::new();
        handle_paste("a\r\nb", &mut s);
        let lines = s.input.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
    }

    #[test]
    fn scroll_up_breaks_auto_stick_and_increments_offset() {
        // Pressing PgUp must move the offset up and disable the
        // auto-stick-to-bottom flag — that's what tells the
        // renderer to keep showing past content instead of
        // scrolling back to the latest turn.
        let mut s = AppState::new();
        assert!(s.auto_stick_bottom, "initial state must auto-stick");
        scroll_up(&mut s, 5);
        assert_eq!(s.scroll_offset, 5);
        assert!(!s.auto_stick_bottom);
    }

    #[test]
    fn scroll_down_to_zero_reenables_auto_stick() {
        // After scrolling up then back down to the bottom, the
        // auto-stick flag must flip back so new turns auto-scroll
        // into view again.
        let mut s = AppState::new();
        scroll_up(&mut s, 5);
        assert!(!s.auto_stick_bottom);
        scroll_down(&mut s, 3);
        assert_eq!(s.scroll_offset, 2);
        assert!(!s.auto_stick_bottom, "still mid-scroll");
        scroll_down(&mut s, 100);
        assert_eq!(s.scroll_offset, 0);
        assert!(s.auto_stick_bottom, "back at bottom → auto-stick on");
    }

    #[test]
    fn submit_pushes_to_history_and_resets_cursor() {
        // After Enter, the trimmed text lands in input_history and
        // the navigation cursor resets so the next ↑ rewinds from
        // the fresh tail.
        let mut s = AppState::new();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(cmd_tx, evt_rx);

        s.input.insert_str("first prompt");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input_history, vec!["first prompt"]);
        assert!(s.history_cursor.is_none());

        s.input.insert_str("second prompt");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input_history.len(), 2);
    }

    #[test]
    fn submit_deduplicates_consecutive_identical_messages() {
        // Spamming the same prompt twice shouldn't pollute history
        // with a back-to-back duplicate — readline doesn't do it,
        // we don't either.
        let mut s = AppState::new();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(4);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(cmd_tx, evt_rx);

        s.input.insert_str("hello");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        s.input.insert_str("hello");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input_history, vec!["hello"]);
    }

    #[test]
    fn arrow_up_at_row_zero_recalls_previous_input() {
        // Pre-populate history. First ↑ → most recent entry; second
        // ↑ → entry before that. Going below 0 clamps.
        let mut s = AppState::new();
        s.input_history = vec!["one".into(), "two".into(), "three".into()];

        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "three");
        assert_eq!(s.history_cursor, Some(2));

        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "two");
        assert_eq!(s.history_cursor, Some(1));

        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "one");
        assert_eq!(s.history_cursor, Some(0));

        // Past the oldest — clamps, no panic.
        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "one");
        assert_eq!(s.history_cursor, Some(0));
    }

    #[test]
    fn arrow_down_past_end_clears_to_fresh() {
        // After recalling, ↓ walks forward in history. Going past
        // the most-recent entry blanks the textarea and resets the
        // cursor to None (so future typing is "fresh").
        let mut s = AppState::new();
        s.input_history = vec!["a".into(), "b".into()];

        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "a");

        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "b");

        // Past the last → blanks.
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines().join("\n"), "");
        assert!(s.history_cursor.is_none());
    }

    #[test]
    fn arrow_up_inside_multiline_textarea_does_not_recall() {
        // When the textarea has multiple lines and the cursor is
        // not on row 0, ↑ should move the cursor inside the
        // textarea instead of swapping it out for history.
        let mut s = AppState::new();
        s.input_history = vec!["history entry".into()];

        s.input.insert_str("line one");
        s.input.insert_newline();
        s.input.insert_str("line two");
        // Cursor is now at row 1 (last). ↑ should move to row 0,
        // NOT load from history.
        handle_key(k(KeyCode::Up, KeyModifiers::NONE), &mut s);
        // Textarea content untouched.
        assert_eq!(s.input.lines().join("\n"), "line one\nline two");
        // No history cursor advance.
        assert!(s.history_cursor.is_none());
    }

    #[test]
    fn home_key_jumps_to_top_via_saturating_offset() {
        let mut s = AppState::new();
        scroll_up(&mut s, usize::MAX);
        assert_eq!(s.scroll_offset, usize::MAX);
        assert!(!s.auto_stick_bottom);
    }

    #[test]
    fn submit_when_busy_pushes_to_queue() {
        let _g = input_queue::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = AppState::new();
        s.agent_busy = true;
        input_queue::queue_clear();

        s.input.insert_str("queue me");
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);

        assert_eq!(input_queue::queue_len(), 1);
        assert_eq!(
            input_queue::queue_snapshot().last().map(String::as_str),
            Some("queue me")
        );
        input_queue::queue_clear();
    }

    // ── AskUser modal mechanics ──────────────────────────────────────
    //
    // Asserts the keyboard contract end-to-end without booting the
    // runner: we install a pending_ask manually, drive keys, then
    // peek at the cmd_tx channel to confirm the right AskAnswer
    // shape went out.

    use super::super::wire::{AgentEvent, AskOption};

    fn install_ask(state: &mut AppState, multi: bool, with_descriptions: bool) {
        let mk = |label: &str, rec: bool| AskOption {
            label: label.into(),
            description: if with_descriptions {
                Some(format!("desc for {}", label))
            } else {
                None
            },
            recommended: rec,
        };
        state.apply_event(AgentEvent::AskUser {
            id: "ask-test-1".into(),
            question: "Which tool?".into(),
            options: vec![mk("alpha", true), mk("beta", false), mk("gamma", false)],
            multi_select: multi,
            hint: Some("pick the safest".into()),
        });
    }

    fn drain_cmds(_state: &mut AppState) -> Vec<TuiCommand> {
        // Tests above keep their own `rx` end of the cmd channel for
        // direct inspection; this helper exists so the last test
        // (which doesn't care about command output) still compiles
        // without a warning about unused channel ends.
        Vec::new()
    }

    #[test]
    fn ask_modal_cursor_starts_on_recommended_option() {
        // The renderer auto-focuses the recommended option when the
        // modal opens — the LLM signals its "right answer" and the
        // keyboard caret meets it there.
        let mut s = AppState::new();
        install_ask(&mut s, /*multi=*/ false, /*desc=*/ false);
        let ask = s.pending_ask.as_ref().unwrap();
        assert_eq!(ask.cursor, 0, "alpha is recommended → cursor on row 0");
    }

    #[test]
    fn ask_arrow_down_moves_caret_with_wrap() {
        let mut s = AppState::new();
        install_ask(&mut s, false, false);
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        assert_eq!(s.pending_ask.as_ref().unwrap().cursor, 1);
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        assert_eq!(s.pending_ask.as_ref().unwrap().cursor, 2);
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        // wraps around to 0
        assert_eq!(s.pending_ask.as_ref().unwrap().cursor, 0);
    }

    #[test]
    fn ask_enter_in_single_select_submits_and_clears_modal() {
        let mut s = AppState::new();
        install_ask(&mut s, false, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s); // cursor=1 (beta)
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);

        assert!(s.pending_ask.is_none(), "submit must clear the modal");
        let cmd = rx.try_recv().expect("AskAnswer must be sent");
        match cmd {
            TuiCommand::AskAnswer {
                id,
                cancelled,
                selected,
                custom_text,
            } => {
                assert_eq!(id, "ask-test-1");
                assert!(!cancelled);
                assert_eq!(selected, vec!["beta".to_string()]);
                assert!(custom_text.is_none());
            }
            other => panic!("expected AskAnswer, got {:?}", other),
        }
    }

    #[test]
    fn ask_space_toggles_in_multi_select() {
        let mut s = AppState::new();
        install_ask(&mut s, true, false);
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s); // toggle row 0
        assert!(s.pending_ask.as_ref().unwrap().options[0].selected);
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s); // off
        assert!(!s.pending_ask.as_ref().unwrap().options[0].selected);
    }

    #[test]
    fn ask_enter_in_multi_select_submits_all_checked() {
        let mut s = AppState::new();
        install_ask(&mut s, true, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        // tick 1: cursor=0 -> Space (alpha selected)
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s);
        // cursor=1 -> Space (beta selected)
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s);
        // Enter -> submit
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);

        let cmd = rx.try_recv().expect("AskAnswer must be sent");
        match cmd {
            TuiCommand::AskAnswer {
                cancelled,
                selected,
                ..
            } => {
                assert!(!cancelled);
                assert_eq!(selected, vec!["alpha".to_string(), "beta".to_string()]);
            }
            other => panic!("expected AskAnswer, got {:?}", other),
        }
    }

    #[test]
    fn ask_enter_in_multi_with_no_check_picks_focused_row() {
        // Common case: user wants the focused choice — they shouldn't
        // need Space + Enter, just Enter. Mirrors Claude Code's
        // "Enter to select" affordance on multi-pick lists.
        let mut s = AppState::new();
        install_ask(&mut s, true, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s); // cursor=1
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);

        match rx.try_recv().expect("AskAnswer must be sent") {
            TuiCommand::AskAnswer {
                selected,
                cancelled,
                ..
            } => {
                assert!(!cancelled);
                assert_eq!(selected, vec!["beta".to_string()]);
            }
            other => panic!("expected AskAnswer, got {:?}", other),
        }
    }

    #[test]
    fn ask_esc_cancels_with_empty_selection() {
        let mut s = AppState::new();
        install_ask(&mut s, false, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        handle_key(k(KeyCode::Esc, KeyModifiers::NONE), &mut s);
        assert!(s.pending_ask.is_none());
        match rx.try_recv().expect("AskAnswer must be sent") {
            TuiCommand::AskAnswer {
                cancelled,
                selected,
                ..
            } => {
                assert!(cancelled);
                assert!(selected.is_empty());
            }
            other => panic!("expected AskAnswer cancel, got {:?}", other),
        }
    }

    // ── Help overlay (F1) ────────────────────────────────────────────

    #[test]
    fn f1_opens_help_overlay() {
        let mut s = AppState::new();
        assert!(!s.show_help, "default state: overlay is closed");
        handle_key(k(KeyCode::F(1), KeyModifiers::NONE), &mut s);
        assert!(s.show_help, "F1 must open the help overlay");
    }

    #[test]
    fn ctrl_question_also_opens_help() {
        let mut s = AppState::new();
        handle_key(k(KeyCode::Char('?'), KeyModifiers::CONTROL), &mut s);
        assert!(s.show_help, "Ctrl-? is the keyboard alias for F1");
    }

    #[test]
    fn any_key_dismisses_help_overlay() {
        let mut s = AppState::new();
        s.show_help = true;
        handle_key(k(KeyCode::Char('a'), KeyModifiers::NONE), &mut s);
        assert!(!s.show_help, "any key must close the overlay");
    }

    #[test]
    fn ctrl_c_still_quits_from_help_overlay() {
        // Help overlay swallows most keys, but Ctrl-C must always
        // remain the escape hatch — otherwise the user could feel
        // trapped if the renderer froze with the overlay up.
        let mut s = AppState::new();
        s.show_help = true;
        let quit = handle_key(k(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut s);
        assert!(quit, "Ctrl-C must propagate even from inside the overlay");
        assert!(s.should_quit);
    }

    // ── Local slash commands ──────────────────────────────────────────

    fn submit(state: &mut AppState, text: &str) {
        state.input.insert_str(text);
        let _ = submit_input(state);
    }

    // ── slash completion & local session commands (owner, 2026-08-17) ───

    fn system_lines(s: &AppState) -> Vec<String> {
        s.conversation
            .iter()
            .filter(|c| c.kind == LineKind::System)
            .map(|c| c.text.clone())
            .collect()
    }

    /// Tab on `/sess` completes to `/session`, a second Tab to `/sessions`,
    /// a third wraps — and the side panel does not move. A failure would mean
    /// Tab still flips the panel while the operator is typing a command.
    #[test]
    fn tab_on_a_slash_prefix_completes_and_cycles_instead_of_switching_panel() {
        let mut s = AppState::new();
        let panel = s.side_panel;
        s.input.insert_str("/sess");
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines()[0], "/session");
        assert_eq!(s.side_panel, panel, "panel untouched");
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines()[0], "/sessions");
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines()[0], "/session", "cycles back");
        assert_eq!(s.side_panel, panel);
    }

    /// A slash prefix that matches nothing swallows Tab too (no panel flip);
    /// a non-slash input still cycles the panel.
    #[test]
    fn tab_is_swallowed_on_an_unknown_slash_and_free_otherwise() {
        let mut s = AppState::new();
        let panel = s.side_panel;
        s.input.insert_str("/zzz");
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.input.lines()[0], "/zzz");
        assert_eq!(s.side_panel, panel);
        let mut t = AppState::new();
        t.input.insert_str("hello");
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut t);
        assert_ne!(
            t.side_panel, panel,
            "plain text: Tab still cycles the panel"
        );
    }

    /// Every completable command is answered locally: submitting it never
    /// echoes a User line (which is what precedes a send to the model).
    /// A failure would mean Tab completes to something that becomes a prompt.
    #[test]
    fn slash_completion_only_offers_handled_commands() {
        for cmd in TUI_SLASH_COMMANDS {
            let mut s = AppState::new();
            let (tx, _rx) = tokio::sync::mpsc::channel(8);
            s.cmd_tx = Some(tx);
            // Give the argument-taking forms a plausible argument.
            let line = match *cmd {
                "/badge watch " => "/badge watch 5 date".to_string(),
                "/cron in " => "/cron in 5 tick".to_string(),
                "/model " => "/model x".to_string(),
                "/playground " => continue, // a prompt in disguise: it IS sent — see `slash_playground_sends_the_prompt`
                "/provider " => "/provider p".to_string(),
                "/resume " => "/resume last".to_string(),
                "/task add " => "/task add t".to_string(),
                "/task done " => "/task done 1".to_string(),
                other => other.to_string(),
            };
            submit(&mut s, &line);
            let user_lines = s
                .conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count();
            assert_eq!(user_lines, 0, "{line:?} must be handled locally, not sent");
        }
    }

    // ── the picker: filter, local sink, sessions ────────────────────────

    fn local_picker(labels: &[&str]) -> AppState {
        let mut s = AppState::new();
        s.pending_ask = Some(crate::tui::app::AskState::local(
            "pick",
            None,
            labels
                .iter()
                .map(|l| (l.to_string(), Some(format!("about {l}"))))
                .collect(),
            crate::tui::app::PickerAction::ResumeSession,
        ));
        s
    }

    /// Typing narrows the list to the rows whose label or description
    /// contains the text; the cursor indexes the VISIBLE rows; Backspace
    /// widens. A failure would mean letters still navigate (j/k) or that
    /// Enter picks a hidden row.
    #[test]
    fn picker_letters_filter_and_the_cursor_walks_the_visible_rows() {
        let mut s = local_picker(&[
            "2026-08-16_20-08_c246",
            "2026-08-16_15-21_250b",
            "2026-08-15_09-00_aaaa",
        ]);
        for c in "15".chars() {
            handle_key(k(KeyCode::Char(c), KeyModifiers::NONE), &mut s);
        }
        let ask = s.pending_ask.as_ref().unwrap();
        assert_eq!(ask.filter, "15");
        assert_eq!(ask.visible(), vec![1, 2], "the two ids containing 15");
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        assert_eq!(
            s.pending_ask.as_ref().unwrap().cursor,
            1,
            "second visible row"
        );
        handle_key(k(KeyCode::Backspace, KeyModifiers::NONE), &mut s);
        handle_key(k(KeyCode::Backspace, KeyModifiers::NONE), &mut s);
        assert_eq!(
            s.pending_ask.as_ref().unwrap().visible().len(),
            3,
            "widened again"
        );
    }

    /// Enter in a local picker performs the picker's action — here a
    /// `ResumeSession` command with the picked id — and sends NOTHING to
    /// the model; Esc closes it silently. A failure would send an AskAnswer
    /// for a question the model never asked, or resume the wrong row.
    #[test]
    fn a_local_pick_becomes_its_action_and_esc_answers_nobody() {
        let mut s = local_picker(&["2026-08-16_20-08_c246", "2026-08-16_15-21_250b"]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        s.cmd_tx = Some(tx);
        for c in "250b".chars() {
            handle_key(k(KeyCode::Char(c), KeyModifiers::NONE), &mut s);
        }
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert!(s.pending_ask.is_none());
        match rx.try_recv().expect("a command") {
            TuiCommand::ResumeSession { id } => assert_eq!(id, "2026-08-16_15-21_250b"),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
        let mut t = local_picker(&["a", "b"]);
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);
        t.cmd_tx = Some(tx2);
        handle_key(k(KeyCode::Esc, KeyModifiers::NONE), &mut t);
        assert!(t.pending_ask.is_none());
        assert!(rx2.try_recv().is_err(), "no AskAnswer for a local picker");
    }

    /// The model's question keeps its contract under the filter: Esc still
    /// answers `cancelled = true` (the runner is blocked on it).
    #[test]
    fn the_models_question_is_still_answered_on_esc_after_filtering() {
        let mut s = AppState::new();
        install_ask(&mut s, false, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);
        handle_key(k(KeyCode::Char('z'), KeyModifiers::NONE), &mut s);
        assert!(s.pending_ask.as_ref().unwrap().visible().is_empty());
        handle_key(k(KeyCode::Enter, KeyModifiers::NONE), &mut s);
        assert!(
            s.pending_ask.is_some(),
            "Enter on an empty view picks nothing"
        );
        handle_key(k(KeyCode::Esc, KeyModifiers::NONE), &mut s);
        match rx.try_recv().expect("cancel answer") {
            TuiCommand::AskAnswer { cancelled, .. } => assert!(cancelled),
            other => panic!("{other:?}"),
        }
    }

    /// `/sessions` opens the picker (a local one, ResumeSession) instead of
    /// printing, and `/resume <id>` sends the command straight away.
    #[test]
    fn slash_sessions_opens_the_picker_and_slash_resume_id_sends_the_command() {
        let mut s = AppState::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        s.cmd_tx = Some(tx);
        submit(&mut s, "/sessions");
        // Either the picker is open, or (empty sessions dir on this
        // machine) a System line says so — never a User line.
        let opened = s.pending_ask.as_ref().map(|a| a.sink.clone());
        assert!(
            opened
                == Some(crate::tui::app::AskSink::Local(
                    crate::tui::app::PickerAction::ResumeSession
                ))
                || system_lines(&s)
                    .iter()
                    .any(|l| l.contains("no other session")),
            "picker or explicit empty message"
        );
        assert_eq!(
            s.conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count(),
            0
        );
        s.pending_ask = None;
        submit(&mut s, "/resume 2026-08-16_20-08_c246");
        match rx.try_recv().expect("resume command") {
            TuiCommand::ResumeSession { id } => assert_eq!(id, "2026-08-16_20-08_c246"),
            other => panic!("{other:?}"),
        }
    }

    /// `SessionResumed` replaces the transcript with the reloaded messages
    /// and adopts the id. A failure would leave the old conversation on
    /// screen under a new session id.
    #[test]
    fn session_resumed_replaces_the_transcript_and_the_id() {
        let mut s = AppState::new();
        s.session_id = "old0".into();
        s.push_line(ConversationLine::new(LineKind::User, "stale".into()));
        s.apply_event(AgentEvent::SessionResumed {
            id: "2026-08-16_20-08_c246".into(),
            turns_reloaded: 2,
            bytes_skipped: 0,
            messages: vec![
                ("user".into(), "hello".into()),
                ("assistant".into(), "hi there".into()),
            ],
        });
        assert_eq!(s.session_id, "2026-08-16_20-08_c246");
        let texts: Vec<&str> = s.conversation.iter().map(|c| c.text.as_str()).collect();
        assert!(!texts.contains(&"stale"));
        assert_eq!(s.conversation[0].kind, LineKind::User);
        assert_eq!(s.conversation[0].text, "hello");
        assert_eq!(s.conversation[1].kind, LineKind::Assistant);
        assert!(system_lines(&s)
            .iter()
            .any(|l| l.contains("resumed session 2026-08-16_20-08_c246")));
    }

    /// `/playground 16` echoes the slash and sends the shared prompt.
    /// `keep`, deliberately: without it the command runs the REAL
    /// `playground/reset.sh` of the repo this test runs in — a `git checkout`
    /// of the operator's playground from inside a unit test.
    #[test]
    fn slash_playground_sends_the_prompt() {
        let mut s = AppState::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        s.cmd_tx = Some(tx);
        submit(&mut s, "/playground keep 16");
        let echoed: Vec<&str> = s
            .conversation
            .iter()
            .filter(|c| c.kind == LineKind::User)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(
            echoed,
            vec!["/playground keep 16"],
            "the operator sees what they typed"
        );
        match rx.try_recv().expect("the prompt is sent") {
            TuiCommand::UserInput(text) => assert!(text.contains("starts with `16`"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// `/session` and `/sessions` are answered by the TUI itself. On the
    /// 2026-08-16/17 journals both became prompts and the model read its
    /// own journal with `cat`. A failure would send them to the model again.
    #[test]
    fn session_commands_are_answered_locally() {
        let mut s = AppState::new();
        s.session_id = "abcd".into();
        submit(&mut s, "/session");
        assert!(
            system_lines(&s).iter().any(|l| l.contains("session abcd")),
            "{:?}",
            system_lines(&s)
        );
        assert_eq!(
            s.conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count(),
            0
        );
        submit(&mut s, "/sessions");
        assert!(
            s.pending_ask.is_some()
                || system_lines(&s)
                    .iter()
                    .any(|l| l.contains("no other session")),
            "{:?}",
            system_lines(&s)
        );
        assert_eq!(
            s.conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count(),
            0
        );
    }

    /// A mistyped command is caught with a suggestion instead of becoming a
    /// prompt (`/sesssions` → the model ran `ls` for it); a path is not.
    #[test]
    fn a_mistyped_slash_command_is_caught_and_a_path_is_not() {
        let mut s = AppState::new();
        submit(&mut s, "/sesssions");
        assert!(
            system_lines(&s)
                .iter()
                .any(|l| l.contains("did you mean /sessions")),
            "{:?}",
            system_lines(&s)
        );
        assert_eq!(
            s.conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count(),
            0
        );
        let mut t = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        t.cmd_tx = Some(tx);
        submit(&mut t, "/etc/hosts what is it?");
        assert_eq!(
            t.conversation
                .iter()
                .filter(|c| c.kind == LineKind::User)
                .count(),
            1,
            "a path is a prompt"
        );
    }

    #[test]
    fn slash_task_add_appends_open_task() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/task add draft the v0.4 release notes");
        assert_eq!(s.tasks.len(), 1, "one task must land");
        assert_eq!(s.tasks[0].title, "draft the v0.4 release notes");
        assert!(!s.tasks[0].done);
    }

    #[test]
    fn slash_task_done_marks_existing_task() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/task add wire the CDN");
        let id = s.tasks[0].id;
        submit(&mut s, &format!("/task done {}", id));
        assert!(s.tasks.iter().find(|t| t.id == id).unwrap().done);
    }

    #[test]
    fn slash_task_clear_wipes_everything() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/task add a");
        submit(&mut s, "/task add b");
        submit(&mut s, "/task add c");
        assert_eq!(s.tasks.len(), 3);
        submit(&mut s, "/task clear");
        assert!(s.tasks.is_empty(), "/task clear must wipe");
    }

    #[test]
    fn slash_cron_in_schedules_a_passive_reminder() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/cron in 120 check disk space");
        assert_eq!(s.crons.len(), 1);
        assert_eq!(s.crons[0].label, "check disk space");
        assert!(
            s.crons[0].trigger_prompt.is_none(),
            "without `->`, the cron is a passive reminder"
        );
    }

    #[test]
    fn slash_cron_in_with_arrow_carries_trigger_prompt() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(
            &mut s,
            "/cron in 30 retry deploy -> redeploy main branch now",
        );
        assert_eq!(s.crons.len(), 1);
        assert_eq!(
            s.crons[0].trigger_prompt.as_deref(),
            Some("redeploy main branch now"),
        );
    }

    #[test]
    fn slash_cron_cancel_removes_the_entry() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/cron in 60 ping");
        let id = s.crons[0].id;
        submit(&mut s, &format!("/cron cancel {}", id));
        assert!(s.crons.is_empty(), "/cron cancel must remove");
    }

    #[test]
    fn slash_help_opens_overlay() {
        let mut s = AppState::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_etx, erx) = tokio::sync::mpsc::unbounded_channel();
        s.attach_channels(tx, erx);

        submit(&mut s, "/help");
        assert!(s.show_help, "/help opens the cheat sheet just like F1");
    }

    #[test]
    fn ask_modal_blocks_global_shortcuts_except_ctrl_c() {
        // While the modal is up, Tab must NOT cycle the side panel —
        // the user has a question to answer. Ctrl-C still quits.
        let mut s = AppState::new();
        let original_panel = s.side_panel;
        install_ask(&mut s, false, false);
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(
            s.side_panel, original_panel,
            "Tab must be inert while AskUser is pending"
        );
        assert!(s.pending_ask.is_some(), "Tab must not dismiss the modal");

        // Ctrl-C still kills.
        let quit = handle_key(k(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut s);
        assert!(quit);
        let _ = drain_cmds(&mut s); // silence unused-helper warn
    }

    /// The Hooks panel keys: ↓ moves the cursor, Space arms the hook under
    /// it, Esc returns focus to the message input. All hooks start OFF —
    /// only these keystrokes, from a human, can arm one.
    #[test]
    fn hooks_toggle_with_space_and_esc_returns_to_input() {
        use crate::tui::app::{FocusRegion, SidePanel};
        let mut s = AppState::new();
        s.side_panel = SidePanel::Hooks;
        s.focus = FocusRegion::SidePanel;
        assert!(s.hooks.iter().all(|h| !h.enabled), "hooks must start off");
        handle_key(k(KeyCode::Down, KeyModifiers::NONE), &mut s);
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s);
        assert!(s.hooks[1].enabled, "Space did not arm the focused hook");
        handle_key(k(KeyCode::Char(' '), KeyModifiers::NONE), &mut s);
        assert!(!s.hooks[1].enabled, "Space did not disarm it again");
        handle_key(k(KeyCode::Esc, KeyModifiers::NONE), &mut s);
        assert_eq!(
            s.focus,
            FocusRegion::Conversation,
            "Esc must return to the input"
        );
    }

    /// Owner requirement: the focused pane is visually obvious, and focus
    /// follows the keys. Tab addresses the side panel; a scroll key hands
    /// focus back to the conversation.
    #[test]
    fn focus_follows_the_keys() {
        use crate::tui::app::FocusRegion;
        let mut s = AppState::new();
        assert_eq!(s.focus, FocusRegion::Conversation);
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.focus, FocusRegion::SidePanel);
        handle_key(k(KeyCode::PageUp, KeyModifiers::NONE), &mut s);
        assert_eq!(s.focus, FocusRegion::Conversation);
        handle_key(k(KeyCode::Tab, KeyModifiers::NONE), &mut s);
        assert_eq!(s.focus, FocusRegion::SidePanel);
    }

    /// Shift+Tab asks the runner to toggle plan mode, and moves nothing on
    /// screen: the session decides, once the runtime holds the switch.
    #[test]
    fn shift_tab_asks_the_runner_to_toggle_plan_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = AppState::new();
        s.attach_channels(tx, evt_rx);
        let (panel, focus) = (s.side_panel, s.focus);
        handle_key(k(KeyCode::BackTab, KeyModifiers::SHIFT), &mut s);
        assert!(
            matches!(rx.try_recv(), Ok(TuiCommand::SetPlanMode { plan: None })),
            "Shift+Tab did not ask for a toggle"
        );
        assert_eq!((s.side_panel, s.focus), (panel, focus));
        assert!(!s.plan_mode, "the screen switched before the session did");
    }

    /// `/plan` and `/build` are the runner's, never a prompt to the model.
    #[test]
    fn plan_and_build_go_to_the_runner_not_the_model() {
        for (line, want) in [("/plan", true), ("/build", false)] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut s = AppState::new();
            s.attach_channels(tx, evt_rx);
            submit(&mut s, line);
            match rx.try_recv() {
                Ok(TuiCommand::SetPlanMode { plan: Some(p) }) => assert_eq!(p, want, "{line}"),
                other => panic!("{line}: {other:?}"),
            }
            assert!(s.conversation.iter().all(|c| c.kind != LineKind::User));
        }
    }

    /// `/undo` is the runner's: the session holds the undo journal and the
    /// bridge. A failure here means the TUI sends `/undo` to the model as a
    /// prompt, which docs/tui.md and the README promise it never does.
    #[test]
    fn undo_goes_to_the_runner_not_the_model() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = AppState::new();
        s.attach_channels(tx, evt_rx);
        submit(&mut s, "/undo");
        match rx.try_recv() {
            Ok(TuiCommand::Undo) => {}
            other => panic!("/undo must reach the runner as Undo, got {other:?}"),
        }
        assert!(
            s.conversation.iter().all(|c| c.kind != LineKind::User),
            "/undo was echoed as a prompt"
        );
        assert!(
            TUI_SLASH_COMMANDS.contains(&"/undo"),
            "Tab cannot complete /undo"
        );
    }

    /// The confirmation modal: `1` allows once, `2` is accepted only when a
    /// standing grant was offered, Esc denies, and every submission emits a
    /// correlated ConfirmationAnswer.
    #[test]
    fn confirm_modal_answers_carry_the_choice_and_the_id() {
        use crate::tui::app::ConfirmState;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = AppState::new();
        s.attach_channels(tx, evt_rx);
        // With a standing option offered.
        s.pending_confirm = Some(ConfirmState {
            id: "confirm-1".into(),
            action_type: "MoveToTrash".into(),
            action: serde_json::json!({"type": "MoveToTrash", "path": "/tmp/x"}),
            reason: "needs approval".into(),
            persistent_grant: Some("MoveToTrash on /tmp/x".into()),
            cursor: 0,
        });
        handle_key(k(KeyCode::Char('2'), KeyModifiers::NONE), &mut s);
        match rx.try_recv().expect("an answer was sent") {
            TuiCommand::ConfirmationAnswer {
                id,
                approved,
                always,
            } => {
                assert_eq!(id, "confirm-1");
                assert!(approved && always);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(s.pending_confirm.is_none());

        // Without one, `2` is inert and Esc denies.
        s.pending_confirm = Some(ConfirmState {
            id: "confirm-2".into(),
            action_type: "MoveToTrash".into(),
            action: serde_json::json!({"type": "MoveToTrash"}),
            reason: "needs approval".into(),
            persistent_grant: None,
            cursor: 0,
        });
        handle_key(k(KeyCode::Char('2'), KeyModifiers::NONE), &mut s);
        assert!(
            s.pending_confirm.is_some(),
            "`2` granted a scope the authority never offered"
        );
        handle_key(k(KeyCode::Esc, KeyModifiers::NONE), &mut s);
        match rx.try_recv().expect("a denial was sent") {
            TuiCommand::ConfirmationAnswer { id, approved, .. } => {
                assert_eq!(id, "confirm-2");
                assert!(!approved);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
