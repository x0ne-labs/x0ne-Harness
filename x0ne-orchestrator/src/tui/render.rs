//! Pure-render layer for tui.
//!
//! Reads `&AppState`, draws a frame. No mutation, no I/O, no async.
//! Layout (top-to-bottom):
//!
//! ```text
//! ┌─ x0ne · session a3f2 ──── ◉ runtime  ◌ vm  ○ browser ─────────┐
//! │ ◆ anthropic/claude-sonnet-5   12.4k tk   $0.08   🛡 sandbox  │  ← 2-line header
//! │ goal: ship the v0.3 cut · step 3/7                            │
//! ├──────────────────────────────────┬─────────────────────────────┤
//! │ conversation / action trail      │ side panel (Tab to cycle)   │
//! │ ...                              │                             │
//! │                                  │                             │
//! ├──────────────────────────────────┴─────────────────────────────┤
//! │ ❯ input zone (multi-line)                                      │
//! │                                                                │
//! ├────────────────────────────────────────────────────────────────┤
//! │ /goal /queue /restore ─────────────── ^Tab panel · ^C quit · ? │
//! └────────────────────────────────────────────────────────────────┘
//! ```

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::app::{AppState, ConversationLine, LineKind, SidePanel};
use super::badge;

// ── colors (kept in one place so a future theme module can swap them)
pub(crate) const ACCENT: Color = Color::Rgb(94, 234, 212); // teal — the X0NE accent
const ACCENT_SHIMMER: Color = Color::Rgb(76, 210, 192); // barely-different teal (one shimmer step)
const ACCENT_MID: Color = Color::Rgb(56, 178, 172); // dimmer teal
const ACCENT_LOW: Color = Color::Rgb(40, 110, 100); // dimmer still
pub(crate) const ACCENT_DIM: Color = Color::Rgb(40, 90, 80);
pub(crate) const TEXT: Color = Color::Rgb(220, 224, 232);
pub(crate) const TEXT_SOFT: Color = Color::Rgb(180, 188, 200);
pub(crate) const DIM: Color = Color::Rgb(152, 162, 177);
pub(crate) const FAINT: Color = Color::Rgb(112, 124, 142);
/// Pane borders. One tone for every border in the frame — conversation, side
/// panel and input — so the chrome reads as one object rather than three
/// widgets that happen to be adjacent.
pub(crate) const FRAME: Color = Color::Rgb(104, 112, 128);
/// Plan mode (`/plan`): amber, the one colour nothing else uses.
const PLAN: Color = Color::Rgb(230, 170, 60);
pub(crate) const WARN: Color = Color::Rgb(251, 191, 36);
pub(crate) const BAD: Color = Color::Rgb(248, 113, 113);
pub(crate) const VIOLET: Color = Color::Rgb(196, 181, 224);
// Three shades for the "mirror" breath on a pending user line. The
// step from VIOLET → VIOLET_REFLECT_MID → VIOLET_REFLECT_BRIGHT
// gives a perceptible swell (~25% brightness delta peak) without
// being garish. Tuned to feel like light catching a curve.
const VIOLET_REFLECT_MID: Color = Color::Rgb(218, 206, 238);
const VIOLET_REFLECT_BRIGHT: Color = Color::Rgb(240, 230, 252);
const VIOLET_DIM: Color = Color::Rgb(140, 130, 180);

/// Subtle, single-location shimmer on the X0NE logo word. The whole
/// "frame" feel relies on one quiet pulse — not two, not aggressive.
/// Idle: solid ACCENT. Busy: gentle breath between ACCENT and a
/// barely-different ACCENT_SHIMMER over 24 ticks (~2s full cycle).
/// One shade delta, no stops in between — the human eye reads it as
/// "alive" without anything actually flashing.
fn x0ne_logo_color(busy: bool, phase: u32) -> Color {
    if !busy {
        return ACCENT;
    }
    // 24-tick cycle (12 ticks at each shade). Big intervals so the
    // transition is a slow inhale, not a strobe.
    if (phase / 12).is_multiple_of(2) {
        ACCENT
    } else {
        ACCENT_SHIMMER
    }
}

/// Where each conversation row landed on screen, so a click can find it.
///
/// Rebuilt every frame by [`draw_conversation`]. `rows[i]` is the index into
/// `AppState::conversation` that produced the screen row at `top + i`.
///
/// The mapping is **approximate by one row in places**: `Paragraph` re-wraps
/// long lines, so a logical line can occupy more than one screen row while
/// this map assumes one. Callers must therefore treat a hit as pointing at an
/// action *block*, never at an exact line — walking back to the enclosing
/// `action_head` absorbs the error everywhere except a block boundary. Tool
/// lines are pre-truncated to the panel width by `display::result_line`, so
/// the wrap is rare in the rows that matter.
#[derive(Debug, Clone, Default)]
pub struct ConvHits {
    /// First screen row of the conversation area.
    pub top: u16,
    /// Left edge and width, so clicks outside the panel are ignored.
    pub left: u16,
    pub width: u16,
    /// Conversation index per screen row, top-first.
    pub rows: Vec<usize>,
}

impl ConvHits {
    /// The conversation entry under `(col, row)`, or `None` off-panel.
    pub fn hit(&self, col: u16, row: u16) -> Option<usize> {
        if col < self.left || col >= self.left + self.width || row < self.top {
            return None;
        }
        self.rows.get((row - self.top) as usize).copied()
    }
}

pub fn draw(frame: &mut Frame, state: &AppState, hits: &mut ConvHits) {
    draw_in(frame, frame.area(), state, hits);
}

/// [`draw`] into `area` instead of the whole frame: a `windows` pane.
pub fn draw_in(frame: &mut Frame, area: Rect, state: &AppState, hits: &mut ConvHits) {
    // Four horizontal slices: header(5 = 3 banner rows + 1 status + 1
    // border) · body(grow) · input/ask(grow according to mode) · slash
    // bar(1). When an AskUser modal is pending, the bottom slot
    // expands to fit the question + options instead of the textarea.
    let bottom_height = if state.pending_confirm.is_some() {
        confirm_modal_height(state, area.height)
    } else if state.pending_ask.is_some() {
        ask_modal_height(state)
    } else {
        input_height(state)
    };
    // Live status row: a 1-row strip just above the input that shows
    // what the agent is doing RIGHT NOW (thinking, writing, running a
    // tool…). Costs nothing when idle (it just renders a blank row);
    // when active it's a calm dim-italic line so the user sees life
    // even when a tool takes 30s.
    let status_height: u16 = if live_status_active(state) { 1 } else { 0 };
    // While the gate is open, the gate outranks the transcript: the body may
    // shrink to `GATE_BODY_MIN` rows so the modal keeps its choices on
    // screen. `confirm_modal_height` sizes itself against the same numbers.
    let body_min = if state.pending_confirm.is_some() {
        GATE_BODY_MIN
    } else {
        8
    };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // The logo rows + the tagline + the status line, derived from
            // the logo itself. This area will host the matrix view, hence
            // the deliberate height.
            Constraint::Length(HEADER_ROWS), // header (logo banner + status)
            Constraint::Min(body_min),       // body
            Constraint::Length(status_height),
            Constraint::Length(bottom_height),
            Constraint::Length(1), // slash bar
        ])
        .split(area);

    draw_header(frame, outer[0], state);
    draw_body(frame, outer[1], state, hits);
    if status_height > 0 {
        draw_live_status(frame, outer[2], state);
    }
    if state.pending_confirm.is_some() {
        draw_confirm_modal(frame, outer[3], state);
    } else if state.pending_ask.is_some() {
        draw_ask_modal(frame, outer[3], state);
    } else {
        draw_input(frame, outer[3], state);
    }
    draw_slash_bar(frame, outer[4], state);

    // The help overlay (F1) is the absolute last paint pass so it
    // sits above every other surface — header, conversation, modal,
    // input. Rendered as a centered floating card with discreet grey
    // borders and a teal title bar.
    if state.show_help {
        draw_help_overlay(frame, area);
    }
}

fn input_height(state: &AppState) -> u16 {
    // Grow with content (max 6), min 3. Saves space on small terminals
    // while letting users compose multi-line messages comfortably.
    let lines = state.input.lines().len() as u16;
    lines.saturating_add(2).clamp(3, 6)
}

/// Vertical space needed by the AskUser modal:
///   * 1 row question header
///   * 1 row hint (optional)
///   * 2 rows per option (label + description) — clamped
///   * 1 row help footer
///   * 2 rows of border/padding
/// Capped so the modal never eats the whole screen on a small term.
/// Rows the modal spends on the action itself: the CLI's own renderers,
/// parsed span by span — `action_box` for every action (`▸ exec · RunCommand`
/// then `$ cmd arg arg`), `render_manifest` on top for a composite (one row
/// per child). Invariant 5: what is approved must be readable. Owner,
/// 2026-08-17: « when it asks me, if I allow I don't see the whole
/// command » — the modal showed the type and `command` alone, never the
/// arguments, and the TUI prints no action row before the prompt (the CLI
/// does, which is why its prompt could stay short).
///
/// SECURITY: every string here is model-authored; both renderers sanitize
/// their fields (`text_safety`), and ansi-to-tui only maps SGR — a stray
/// escape cannot move the cursor or repaint a row already drawn.
fn confirm_action_lines(action: &serde_json::Value, action_type: &str) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText;
    // The renderers read `type` from the JSON; the request carries it beside
    // the action too, so a payload without the key still names itself.
    let owned;
    let action = if action.get("type").is_some() {
        action
    } else {
        let mut v = action.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "type".into(),
                serde_json::Value::String(action_type.to_string()),
            );
        }
        owned = v;
        &owned
    };
    let mut text = crate::display::action_box(action);
    if action.get("steps").is_some() || action.get("actions").is_some() {
        text.push_str(&crate::agent::cli_sink::render_manifest(action));
    }
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let line = match raw.as_bytes().into_text() {
            Ok(mut t) => t.lines.pop().unwrap_or_default(),
            Err(_) => Line::from(crate::text_safety::sanitize(raw)),
        };
        out.push(line);
    }
    if out.len() > CONFIRM_ACTION_ROWS_MAX {
        let hidden = out.len() - CONFIRM_ACTION_ROWS_MAX;
        out.truncate(CONFIRM_ACTION_ROWS_MAX);
        out.push(Line::from(Span::styled(
            format!("    … {hidden} more rows not shown — deny and ask for a smaller step if you cannot read it all"),
            Style::default().fg(WARN),
        )));
    }
    out
}

/// Rows of action detail the modal is willing to show before it says so.
const CONFIRM_ACTION_ROWS_MAX: usize = 24;

/// Rows the transcript keeps while a gate is open. One: the question is what
/// matters until it is answered, and a transcript row is worth less than a
/// choice the operator cannot see.
const GATE_BODY_MIN: u16 = 1;

/// The modal's height for a frame of `frame_height` rows: everything it has
/// to say (border, action rows, reason, choices) — capped so that the header,
/// `GATE_BODY_MIN` rows of body and the slash bar still fit. What is cut when
/// the cap bites is the ACTION DETAIL (with a row saying how much), never a
/// choice: `draw_confirm_modal` lays the choices out first in its budget.
/// Before 2026-08-17 the modal was `Paragraph`-clipped from the bottom, so on
/// a short terminal « deny » was the first row to disappear (T8's finding).
fn confirm_modal_height(state: &AppState, frame_height: u16) -> u16 {
    let Some(confirm) = state.pending_confirm.as_ref() else {
        return 3;
    };
    let action_rows = confirm_action_lines(&confirm.action, &confirm.action_type).len() as u16;
    let choices = confirm.choices().len() as u16;
    let wanted = 2 /* border */ + action_rows + 1 /* reason */ + choices;
    let cap = frame_height.saturating_sub(HEADER_ROWS + GATE_BODY_MIN + 1);
    wanted.min(cap).max((3 + choices).min(cap))
}

/// The security prompt, as a modal.
///
/// Renders and transports; decides nothing. What the answer authorises is
/// the ApprovalAuthority's business, and the standing option appears only
/// when the authority offered it, under the authority's own wording —
/// mirroring `CliDisplaySink::request_confirmation` line for line.
fn draw_confirm_modal(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(confirm) = state.pending_confirm.as_ref() else {
        return;
    };
    let warn = Color::Rgb(220, 180, 60);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(warn))
        .title(Span::styled(
            " ⚠ approval required · ↑↓ + Enter, or 1/2/3 ",
            Style::default().fg(warn).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // SECURITY (audit 2026-08-15, finding 4): the CLI prompt and this modal
    // are two frontends onto the same decision, so closing the forgery on one
    // and leaving it open on the other would be the dead-scaffolding pattern
    // in its purest form — see docs/invariants.md. `action_type`, `target` and `reason`
    // are model-authored; ratatui lays out whatever chars it is handed and
    // crossterm writes them to the terminal, so an ESC here reaches the same
    // interpreter it reaches on the CLI. See `crate::text_safety`.
    use crate::text_safety::sanitize;
    // What is being approved: the whole action, as the CLI prints it —
    // type, then the command line with EVERY argument, then each child of a
    // composite. See `confirm_action_lines`. Budgeted: the reason and the
    // choices are laid out whatever the height; the action rows take what is
    // left, and say how many were cut.
    let choices = confirm.choices();
    let mut action_lines = confirm_action_lines(&confirm.action, &confirm.action_type);
    let avail = inner.height as usize;
    // Below the height where reason + choices fit, the reason goes first and
    // then the EARLIEST choices — « deny » is the last row and the last to
    // go. Keys still answer (1/2/3, Esc, n) whatever is on screen.
    let show_reason = avail > choices.len();
    let choices: Vec<(u8, String)> = if avail >= choices.len() {
        choices
    } else {
        choices[choices.len() - avail..].to_vec()
    };
    let fixed = usize::from(show_reason) + choices.len();
    let budget = avail.saturating_sub(fixed);
    if action_lines.len() > budget {
        let keep = budget.saturating_sub(1);
        let hidden = action_lines.len() - keep;
        action_lines.truncate(keep);
        if budget > 0 {
            action_lines.push(Line::from(Span::styled(
                format!("    … {hidden} more rows not shown — a taller terminal shows them; deny if you cannot read what this covers"),
                Style::default().fg(WARN),
            )));
        }
    }
    lines.extend(action_lines);
    if show_reason {
        lines.push(Line::from(Span::styled(
            sanitize(&confirm.reason),
            Style::default().fg(warn),
        )));
    }
    for (i, (n, label)) in choices.into_iter().enumerate() {
        let focused = i == confirm.cursor;
        let marker = if focused { "❯" } else { " " };
        let style = if focused {
            Style::default()
                .fg(Color::Rgb(230, 235, 245))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(150, 160, 175))
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(ACCENT)),
            Span::styled(format!("{n}  "), Style::default().fg(ACCENT)),
            Span::styled(label, style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn ask_modal_height(state: &AppState) -> u16 {
    let Some(ask) = state.pending_ask.as_ref() else {
        return 3;
    };
    let hint_rows = if ask.hint.is_some() { 1 } else { 0 };
    let opt_rows: u16 = ask
        .options
        .iter()
        .map(|r| {
            if r.option
                .description
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
            {
                2
            } else {
                1
            }
        })
        .sum();
    let filter_row = 1; // always shown: it is where typed letters go
    let total = 1 /* question */ + hint_rows + filter_row + opt_rows + 1 /* footer */ + 2 /* padding */;
    // A long list (122 sessions) does not grow the modal — it scrolls in a
    // window; see `draw_ask_modal`.
    total.clamp(7, 20)
}

// ── Header ───────────────────────────────────────────────────────────────

fn draw_header(frame: &mut Frame, area: Rect, state: &AppState) {
    // Layout: 3-row X0NE logo banner (left) sharing a row with bodies +
    // session info (right), one separator-style status row, one
    // bottom border. The wider header gives the logo a presence and
    // makes the shimmer visible when the agent is thinking.
    // No rule under the header: the framed panes below draw their own top
    // border, and two horizontals a row apart read as a mistake.
    let inner = area;

    // Top: the banner (logo rows + tagline), then 1 status row.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(HEADER_ROWS - 1), Constraint::Length(1)])
        .split(inner);

    // Banner row: split horizontally into [ logo block | meta block ]. The
    // slot is measured, not guessed: at 28 columns it clipped `v0.2.0` to
    // `v0.2` and the tagline to `Safe Open-Sour` (review, 2026-09-10).
    let banner = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(logo_slot_width()), Constraint::Min(0)])
        .split(rows[0]);

    draw_logo_banner(frame, banner[0], state);
    draw_header_meta(frame, banner[1], state);
    draw_header_status(frame, rows[1], state);

    // Stage 2 will add a collapsible row for the goal pin when one is
    // active. For now we just acknowledge the fields so dead_code lint
    // stays quiet.
    let _ = state.goal_label;
    let _ = state.goal_step;
}

/// Header height: the logo rows, the tagline, the status row. Every layout
/// that stacks on the header derives from this one number.
const HEADER_ROWS: u16 = crate::terminal::LOGO_ROWS + 2;

const TAGLINE: &str = "harness · Safe Open-Source Agent";

fn version_tag() -> String {
    format!("  v{}", env!("CARGO_PKG_VERSION"))
}

/// Columns the logo slot needs: the pad, then the wider of (widest logo row
/// + version tag) and the tagline, then one column of air.
fn logo_slot_width() -> u16 {
    let logo_row = crate::terminal::LOGO_WIDTH + version_tag().len();
    let tagline = TAGLINE.chars().count();
    (1 + logo_row.max(tagline) + 1) as u16
}

/// Render the logo slot on the left of the header. Two modes:
///   * No badge set → the canonical logo, from its single definition in
///     `terminal`, gently breathing when busy.
///   * Badge set → the LLM's painted frame replaces the logo. This
///     is the "LLM as GPU" canvas — the agent literally owns its
///     own avatar inside the frame.
fn x0ne_orchestrator_logo() -> [&'static str; 4] {
    crate::terminal::LOGO
}

fn draw_logo_banner(frame: &mut Frame, area: Rect, state: &AppState) {
    if let Some(playback) = state.badge.as_ref() {
        if let Some(b) = playback.current_frame() {
            let lines = badge::frame_to_lines(b, Style::default().fg(TEXT));
            let p = Paragraph::new(lines);
            frame.render_widget(p, area);
            return;
        }
    }

    // The CLI's banner: same glyph rows, same gradient, same version tag.
    // NOT verbatim: the CLI's rule row (`─── harness · vX ───`) has no row
    // here, so `harness` becomes a prefix on the tagline instead.
    let _ = x0ne_logo_color(state.agent_busy, state.animation_phase);
    let logo_lines = x0ne_orchestrator_logo();
    let pad = " ".to_string();

    // The logo rows with `terminal::LOGO_RAMP`, then the tagline. The
    // version sits on row 1, one of the widest, so it stays next to the
    // letters instead of floating.
    let mut lines: Vec<Line> = Vec::with_capacity(logo_lines.len() + 1);
    for (i, row) in logo_lines.iter().enumerate() {
        let (_, g, b) = crate::terminal::LOGO_RAMP[i];
        let mut spans = vec![
            Span::raw(pad.clone()),
            Span::styled(
                *row,
                Style::default()
                    .fg(Color::Rgb(0, g, b))
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if i == 1 {
            spans.push(Span::styled(
                version_tag(),
                Style::default().fg(Color::Rgb(112, 124, 138)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        Span::raw(pad),
        Span::styled(TAGLINE, Style::default().fg(Color::Rgb(80, 90, 110))),
    ]));

    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

/// Render the right side of the banner row: session id, model label,
/// usage, sandbox. Three lines stacked vertically to match the logo.
fn draw_header_meta(frame: &mut Frame, area: Rect, state: &AppState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Line 1: just the session on the right. The tagline was removed
    // on user feedback ("remove the frame for llms at the top"); the
    // header should whisper, not declare a brand.
    let left: Vec<Span<'static>> = vec![Span::raw("")];
    let session = vec![
        Span::styled("session ", Style::default().fg(FAINT)),
        Span::styled(state.session_id.clone(), Style::default().fg(TEXT_SOFT)),
        Span::raw(" "),
    ];
    let row1 = two_col_line(rows[0], left, session);
    frame.render_widget(row1, rows[0]);

    // Line 2: model, with the bodies alongside it. Tokens and cost used to
    // sit here; they now live in the conversation frame's top border, where
    // they cost no vertical space and sit next to the thing they measure.
    let left = vec![
        Span::styled("◆ ", Style::default().fg(ACCENT)),
        Span::styled(state.model_label.clone(), Style::default().fg(TEXT)),
    ];
    let right = vec![
        Span::styled("runtime ", Style::default().fg(FAINT)),
        Span::styled(
            state.bodies.runtime.glyph(),
            Style::default().fg(body_color(state.bodies.runtime)),
        ),
        Span::styled("  vm ", Style::default().fg(FAINT)),
        Span::styled(
            state.bodies.vm.glyph(),
            Style::default().fg(body_color(state.bodies.vm)),
        ),
        Span::styled("  browser ", Style::default().fg(FAINT)),
        Span::styled(
            state.bodies.browser.glyph(),
            Style::default().fg(body_color(state.bodies.browser)),
        ),
        // Where the actions land. The other three say which subsystems are
        // alive; this one says whether what they do is bounded, which is the
        // question an operator running an agent unattended actually has.
        Span::styled("  sandbox ", Style::default().fg(FAINT)),
        {
            let (glyph, label, rgb) = state.containment.badge();
            Span::styled(
                format!("{glyph} {label}"),
                Style::default().fg(Color::Rgb(rgb.0, rgb.1, rgb.2)),
            )
        },
        Span::raw(" "),
    ];
    let row2 = two_col_line(rows[1], left, right);
    frame.render_widget(row2, rows[1]);

    // Line 3: the sandbox this session is bound to.
    let row3 = two_col_line(
        rows[2],
        vec![
            Span::styled("🛡 ", Style::default().fg(ACCENT_DIM)),
            Span::styled(state.sandbox_label.clone(), Style::default().fg(DIM)),
        ],
        vec![Span::raw(" ")],
    );
    frame.render_widget(row3, rows[2]);
}

/// Bottom status row of the header. Deliberately quiet. Left side
/// carries the discreet build signature (date · time · git rev,
/// baked at compile time by `build.rs`); right side carries the
/// queue badge. Both faded so they whisper, not shout.
fn draw_header_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let stamp = build_stamp_line();
    let left = vec![
        Span::raw(" "),
        Span::styled(
            stamp,
            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
        ),
    ];
    let mut right = vec![queue_badge_span(state.queue_len())];
    if state.plan_mode {
        right.insert(
            0,
            Span::styled(
                " PLAN ",
                Style::default()
                    .fg(Color::Black)
                    .bg(PLAN)
                    .add_modifier(Modifier::BOLD),
            ),
        );
        right.insert(1, Span::raw("  "));
    }
    // The legacy sysmon strip: a compact ram gauge + load for the host this
    // UI runs on, same thresholds and glyphs as the pane it came from.
    if let Some(sys) = state.sys_state.as_ref() {
        let sys = sys.read().unwrap_or_else(|e| e.into_inner());
        if sys.mem_total_gb > 0.0 {
            let pct = sys.mem_pct as usize;
            let bar_w = 8;
            let filled = ((pct * bar_w) / 100).min(bar_w);
            let bar = format!("{}{}", "━".repeat(filled), "╌".repeat(bar_w - filled));
            let color = if pct > 85 {
                Color::Red
            } else if pct > 60 {
                Color::Yellow
            } else {
                DIM
            };
            right.insert(
                0,
                Span::styled(
                    format!(
                        "ram {} {:.1}/{:.0}G · load {:.1}  ",
                        bar, sys.mem_used_gb, sys.mem_total_gb, sys.load_1
                    ),
                    Style::default().fg(color),
                ),
            );
        }
    }
    // GPU + burn-rate strip, same glyphs and thresholds as the ram gauge.
    // Absent entirely when nothing is measured — a zero here would read as
    // "costs nothing", which is exactly the lie this gauge exists to prevent.
    if let Some(cloud) = state.cloud_state.as_ref() {
        let cloud = cloud.read().unwrap_or_else(|e| e.into_inner());
        if let Some(g) = cloud.gpus.first() {
            let pct = g.util_pct as usize;
            let bar_w = 8;
            let filled = ((pct * bar_w) / 100).min(bar_w);
            let bar = format!("{}{}", "━".repeat(filled), "╌".repeat(bar_w - filled));
            let color = if cloud.has_alert() || g.temp_c > 85 {
                Color::Red
            } else if g.temp_c > 70 {
                Color::Yellow
            } else {
                DIM
            };
            let cost = cloud
                .burn
                .as_ref()
                .map(|b| format!(" · {:.2}$/h", b.hourly_usd))
                .unwrap_or_default();
            right.insert(
                0,
                Span::styled(
                    format!("gpu {} {}% {}°C{}  ", bar, g.util_pct, g.temp_c, cost),
                    Style::default().fg(color),
                ),
            );
        } else if let Some(b) = cloud.burn.as_ref() {
            right.insert(
                0,
                Span::styled(
                    format!("gcp {:.2}$/h  ", b.hourly_usd),
                    Style::default().fg(if cloud.has_alert() { Color::Red } else { DIM }),
                ),
            );
        }
    }
    let row = two_col_line(area, left, right);
    frame.render_widget(row, area);
}

/// One-line build signature `v0.2.0 · 26 May 13:25 · ab12cd34`.
/// Date/time come from the build stamp (baked at compile time so
/// it changes on every rebuild — that's the user's contract);
/// version from CARGO_PKG_VERSION.
fn build_stamp_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    // X0NE_BUILD_STAMP looks like `YYMMDDhhmm·<rev>[*]`. Reformat
    // it into something a human can scan at a glance.
    let raw = option_env!("X0NE_BUILD_STAMP").unwrap_or("?·?");
    let mut parts = raw.split('·');
    let date_part = parts.next().unwrap_or("");
    let rev_part = parts.next().unwrap_or("");
    let pretty = format_yymmddhhmm(date_part);
    if pretty.is_empty() {
        format!("v{}", version)
    } else if rev_part.is_empty() {
        format!("v{} · {}", version, pretty)
    } else {
        format!("v{} · {} · {}", version, pretty, rev_part)
    }
}

/// `2605261325` → `26 May 13:25`. Returns empty on malformed input
/// rather than failing — the build stamp is a nice-to-have, not a
/// load-bearing field.
fn format_yymmddhhmm(s: &str) -> String {
    if s.len() != 10 || !s.chars().all(|c| c.is_ascii_digit()) {
        return String::new();
    }
    let _yy = &s[0..2];
    let mm = &s[2..4];
    let dd = &s[4..6];
    let hh = &s[6..8];
    let min = &s[8..10];
    let month = match mm {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => "???",
    };
    format!("{} {} {}:{}", dd.trim_start_matches('0'), month, hh, min)
}

fn body_color(s: super::app::BodyState) -> Color {
    use super::app::BodyState::*;
    match s {
        Active => ACCENT,
        Idle => ACCENT_DIM,
        Off => FAINT,
    }
}

fn queue_badge_span(n: usize) -> Span<'static> {
    if n == 0 {
        Span::raw(" ")
    } else {
        Span::styled(
            format!("📋 {} queued ", n),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    }
}

/// Render a single line split into a left-aligned and right-aligned
/// span list. Used by the header rows.
fn two_col_line(
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) -> Paragraph<'static> {
    let left_len: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_len: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let total = area.width as usize;
    let padding = total.saturating_sub(left_len + right_len);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(padding)));
    spans.extend(right);
    Paragraph::new(Line::from(spans))
}

// ── Body ─────────────────────────────────────────────────────────────────

fn draw_body(frame: &mut Frame, area: Rect, state: &AppState, hits: &mut ConvHits) {
    if state.shell_mode {
        // Shell mode takes over the body — full-width terminal trail
        // with the last N (cmd, output) blocks and the active CWD
        // headline. The agent panel is paused while shell is up.
        draw_shell_page(frame, area, state);
        return;
    }
    if state.focus_mode {
        // Focus mode: full-width conversation, no side panel. The
        // conversation gets to breathe — long markdown answers stop
        // having to compete with the right-side rail.
        draw_conversation(frame, area, state, hits);
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(64),
            Constraint::Length(1), // gutter, so the two frames don't fuse
            Constraint::Percentage(36),
        ])
        .split(area);

    draw_conversation(frame, cols[0], state, hits);
    draw_side_panel(frame, cols[2], state);
}

/// The border colour for a pane, by whether the keyboard is addressing it.
///
/// Owner requirement: the focused pane must be visually obvious. The focused
/// frame carries the dim accent teal — bright enough to be unambiguous,
/// dim enough not to fight the content — and the inactive one keeps the
/// ordinary frame grey.
fn focus_border(state: &AppState, region: crate::tui::app::FocusRegion) -> Style {
    if state.focus == region && !state.focus_mode {
        Style::default().fg(ACCENT_MID)
    } else {
        Style::default().fg(FRAME)
    }
}

fn draw_conversation(frame: &mut Frame, area: Rect, state: &AppState, hits: &mut ConvHits) {
    // Framed, with the live counters inset into the top border rather than
    // spending a header row on them. The rounded frame and the in-border
    // title are the legacy TUI's visual signature — the thing that makes it
    // read as an application rather than a scrolling log.
    let model_title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(ACCENT)),
        Span::styled(state.model_label.clone(), Style::default().fg(TEXT_SOFT)),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(focus_border(
            state,
            crate::tui::app::FocusRegion::Conversation,
        ))
        .title_top(model_title.clone())
        .title_top(
            Line::from(vec![
                Span::raw(" "),
                Span::styled(fmt_tokens(state.tokens_used), Style::default().fg(DIM)),
                Span::styled(" tk · ", Style::default().fg(FAINT)),
                Span::styled(format!("${:.2}", state.cost_usd), Style::default().fg(DIM)),
                Span::raw(" "),
            ])
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // ratatui 0.30 draws right titles over left ones; 0.29 did the reverse.
    // Redrawn so a narrow pane keeps the model name, as before the upgrade.
    if area.width > 2 && area.height > 0 {
        let width = (model_title.width() as u16).min(area.width - 2);
        frame.render_widget(
            model_title,
            Rect {
                x: area.x + 1,
                y: area.y,
                width,
                height: 1,
            },
        );
    }

    // Reserve a 1-column strip on the right edge for the scrollbar
    // rail. The conversation lives in the remaining area to the left.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let conv_area = cols[0];
    let scrollbar_area = cols[1];

    // Identify the "pending" user message — the last User line when
    // the agent is actively reflecting on it. Subtle shimmer.
    let pending_user_idx = if state.agent_busy {
        state
            .conversation
            .iter()
            .enumerate()
            .rev()
            .find(|(_, cl)| cl.kind == LineKind::User)
            .map(|(i, _)| i)
    } else {
        None
    };

    // Build ALL the visible Lines (full conversation expanded),
    // then apply scroll_offset + window. We do it this way so the
    // scrollbar can know the real total row count and the user can
    // PgUp through history.
    let panel_width = conv_area.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(state.conversation.len() * 2);
    // Which conversation entry produced each rendered line. Grown in lockstep
    // with `lines` so the two cannot drift; `owners.resize` after each entry
    // covers every push the arms below make, including the multi-row ones.
    let mut owners: Vec<usize> = Vec::with_capacity(lines.capacity());
    // An expanded action's input/result dump goes UNDER its whole block —
    // header, args, `└ ok`, output box — not right after the header line.
    // Rendered after the header it sat above the very output the operator
    // had clicked on and read as belonging to the action above (owner
    // screenshot 2026-08-17 12:34, « it expands the thing above »). So the
    // dump is held here and flushed when the block ends: at the next action
    // head, at the next non-tool line, or at the end of the transcript.
    let mut pending_expansion: Option<usize> = None;
    let flush_expansion =
        |pending: &mut Option<usize>, lines: &mut Vec<Line<'static>>, owners: &mut Vec<usize>| {
            if let Some(head_idx) = pending.take() {
                for jl in render_action_expansion(&state.conversation[head_idx], panel_width) {
                    lines.push(jl);
                }
                owners.resize(lines.len(), head_idx);
            }
        };
    // Breathing room: one empty row whenever the CONTEXT changes — a user
    // line after an answer, an action block after a user line, an answer
    // after an action, and between two actions. Owner, 2026-08-17: « more
    // airy, it's too condensed, line breaks between different
    // contexts ». Lines of one action block (header, args, result, output
    // box) stay packed; consecutive System lines too.
    let mut prev_kind: Option<LineKind> = None;
    for (idx, cl) in state.conversation.iter().enumerate() {
        let block_ends_here = cl.action_head || !matches!(cl.kind, LineKind::Tool);
        if block_ends_here {
            flush_expansion(&mut pending_expansion, &mut lines, &mut owners);
        }
        let context_changes = match (prev_kind, cl.kind) {
            (None, _) => false,
            (Some(LineKind::Tool), LineKind::Tool) => cl.action_head,
            (Some(p), k) => p != k,
        };
        if context_changes {
            lines.push(Line::from(""));
        }
        prev_kind = Some(cl.kind);
        match cl.kind {
            LineKind::Assistant => {
                use ansi_to_tui::IntoText;
                for boxed_row in super::app::x0ne_response_box_lines(&cl.text, panel_width) {
                    let line = match boxed_row.as_bytes().into_text() {
                        Ok(mut t) => t.lines.pop().unwrap_or_default(),
                        Err(_) => Line::from(boxed_row),
                    };
                    lines.push(line);
                }
            }
            LineKind::User if Some(idx) == pending_user_idx => {
                // Pre-wrap the violet pending line so its continuation
                // rows count as separate logical lines in the windowing
                // math — otherwise the bottom of a long prompt gets
                // clipped because the ratatui Paragraph's visual wrap
                // happens AFTER we slice `visible`. See bug screenshot
                // 20260528_003416.
                for l in format_pending_user_line_wrapped(cl, state.animation_phase, panel_width) {
                    lines.push(l);
                }
            }
            LineKind::User => {
                // Same windowing concern for sent (non-pending) user
                // lines — they would silently clip too on long prompts.
                for l in format_user_line_wrapped(cl, panel_width) {
                    lines.push(l);
                }
            }
            _ => lines.push(format_line(cl)),
        }
        owners.resize(lines.len(), idx);
        // Inline expand: if this conv entry is the head of an action that
        // the user clicked / pressed Ctrl+E on, its full input + output +
        // result JSON follows the block — see `pending_expansion`.
        if cl.action_head && cl.expanded {
            pending_expansion = Some(idx);
        }
    }
    flush_expansion(&mut pending_expansion, &mut lines, &mut owners);

    // Wrap HERE, not in the `Paragraph`, so one element of `lines` is one
    // screen row.
    //
    // BUG (owner capture 2026-08-16_17-19.png): the window below slices the
    // last `conv_area.height` LOGICAL lines, and the paragraph then wrapped
    // any line wider than the pane into several rows. More rows than the
    // area is tall means ratatui clips — from the BOTTOM — so the newest
    // lines were the ones thrown away while the agent was still working.
    // Measured on an 80×24 TestBackend: one 5-row policy refusal near the
    // end hid the five lines after it, the tail of the pane showed stale
    // rows, and the last action's box was cut mid-frame.
    //
    // Two more things rode on the same wrong count: `owners`, which maps a
    // SCREEN row to a conversation entry for click-to-expand (it was indexed
    // by logical line, so every click below a wrapped line hit the wrong
    // action), and the scrollbar's `total`, which measured a document with a
    // different number of rows than the one on screen.
    let (lines, owners) = wrap_to_rows(lines, owners, conv_area.width as usize);

    let total = lines.len();
    let max_lines = conv_area.height as usize;

    // Compute the visible window. scroll_offset = N means we're
    // looking N rows above the bottom. Clamp so we never scroll
    // past the start of the conversation.
    let max_offset = total.saturating_sub(max_lines);
    let effective_offset = state.scroll_offset.min(max_offset);
    let end = total.saturating_sub(effective_offset);
    let start = end.saturating_sub(max_lines);
    let visible: Vec<Line> = lines[start..end].to_vec();

    // Publish where those rows landed, for click-to-expand.
    hits.top = conv_area.y;
    hits.left = conv_area.x;
    hits.width = conv_area.width;
    hits.rows = owners[start..end].to_vec();

    // No `.wrap()`: every element of `visible` is already exactly one row
    // wide enough to fit. Re-wrapping here would put back the mismatch the
    // line above exists to remove.
    let p = Paragraph::new(visible).style(Style::default().fg(TEXT));
    frame.render_widget(p, conv_area);

    draw_scrollbar(frame, scrollbar_area, total, max_lines, effective_offset);
}

/// Break every line that is wider than `width` into the rows it will occupy,
/// carrying its owner index onto each row.
///
/// Word-greedy, like the `Wrap` it replaces: a word moves down whole unless
/// it cannot fit on a line of its own, in which case it is cut. Styles ride
/// along per character, so a wrapped line keeps its colours — a red refusal
/// stays red on its continuation rows.
///
/// `width == 0` is a degenerate pane; the lines are returned untouched rather
/// than looped on forever.
fn wrap_to_rows(
    lines: Vec<Line<'static>>,
    owners: Vec<usize>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    use unicode_width::UnicodeWidthChar;

    if width == 0 {
        return (lines, owners);
    }
    let mut out_lines: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut out_owners: Vec<usize> = Vec::with_capacity(owners.len());

    for (idx, line) in lines.into_iter().enumerate() {
        let owner = owners.get(idx).copied().unwrap_or(0);
        let alignment = line.alignment;
        let line_style = line.style;

        // Flatten to (char, style) so a break can fall anywhere, including
        // inside a span.
        let mut cells: Vec<(char, Style)> = Vec::new();
        for span in &line.spans {
            for ch in span.content.chars() {
                cells.push((ch, span.style));
            }
        }
        let total_w: usize = cells
            .iter()
            .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
            .sum();
        if total_w <= width {
            out_lines.push(line);
            out_owners.push(owner);
            continue;
        }

        let mut row: Vec<(char, Style)> = Vec::new();
        let mut row_w = 0usize;
        // The start of the last word boundary inside `row`, so a word that
        // overflows moves down whole instead of being cut in half.
        let mut break_at: Option<usize> = None;
        let flush = |row: &mut Vec<(char, Style)>,
                     out_lines: &mut Vec<Line<'static>>,
                     out_owners: &mut Vec<usize>| {
            out_lines.push(cells_to_line(std::mem::take(row), line_style, alignment));
            out_owners.push(owner);
        };

        for (ch, style) in cells {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if row_w + cw > width && !row.is_empty() {
                match break_at {
                    // A word boundary exists on this row: carry the trailing
                    // word down rather than cutting it.
                    Some(at) if at > 0 => {
                        let carry: Vec<(char, Style)> = row.split_off(at);
                        flush(&mut row, &mut out_lines, &mut out_owners);
                        row = carry;
                        row_w = row
                            .iter()
                            .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
                            .sum();
                    }
                    // A single word longer than the pane: cut it, or it
                    // would never be shown at all.
                    _ => {
                        flush(&mut row, &mut out_lines, &mut out_owners);
                        row_w = 0;
                    }
                }
                break_at = None;
            }
            if ch == ' ' {
                // Leading spaces of a continuation row are the indentation
                // the line was written with; keep them.
                break_at = Some(row.len() + 1);
            }
            row.push((ch, style));
            row_w += cw;
        }
        if !row.is_empty() {
            flush(&mut row, &mut out_lines, &mut out_owners);
        }
    }
    (out_lines, out_owners)
}

/// Rebuild a `Line` from flattened cells, coalescing runs of equal style so
/// the buffer is not handed one span per character.
fn cells_to_line(
    cells: Vec<(char, Style)>,
    line_style: Style,
    alignment: Option<ratatui::layout::Alignment>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (ch, style) in cells {
        if cur != Some(style) {
            if let Some(s) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
            }
            cur = Some(style);
        }
        buf.push(ch);
    }
    if let Some(s) = cur {
        spans.push(Span::styled(buf, s));
    }
    let mut line = Line::from(spans).style(line_style);
    line.alignment = alignment;
    line
}

/// Draw the discreet right-edge scrollbar: a faint `│` track all
/// the way down, with a brighter `┃` thumb whose vertical position
/// reflects where we are in the conversation. Hidden entirely when
/// everything fits in view (total ≤ max_lines).
fn draw_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    visible: usize,
    offset_from_bottom: usize,
) {
    if area.height == 0 || total == 0 || total <= visible {
        return;
    }
    let track_h = area.height as usize;

    // Thumb height proportional to (visible / total). Floor at 1 so
    // tiny windows still draw something pickable.
    let thumb_h = ((visible as f32 / total as f32) * track_h as f32)
        .floor()
        .max(1.0) as usize;
    let thumb_h = thumb_h.min(track_h);

    // Thumb position: scroll_offset = 0 → bottom; max → top.
    // Convert "offset from bottom" into "offset from top of track".
    let max_off = total.saturating_sub(visible).max(1);
    let progress = 1.0 - (offset_from_bottom as f32 / max_off as f32);
    let thumb_top = ((track_h - thumb_h) as f32 * progress).round() as usize;
    let thumb_top = thumb_top.min(track_h - thumb_h);

    let mut lines: Vec<Line> = Vec::with_capacity(track_h);
    for i in 0..track_h {
        let in_thumb = i >= thumb_top && i < thumb_top + thumb_h;
        // `┃` (HEAVY VERTICAL) for the thumb, `│` (LIGHT VERTICAL)
        // for the track. The two glyphs at the same column give a
        // subtle "fill" effect without needing background colours.
        let (glyph, color) = if in_thumb {
            ("┃", ACCENT_MID)
        } else {
            ("│", FAINT)
        };
        lines.push(Line::from(Span::styled(glyph, Style::default().fg(color))));
    }
    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

/// The user line currently being chewed on by the agent. Same shape
/// as a normal user line (` › <text>` in violet italic) but the
/// color swells through a 4-stop curve: dim → mid → bright → mid →
/// dim → … The 24-tick period (~1.9 s) keeps it slow enough to feel
/// like light catching a polished surface, not a strobe. The
/// chevron `›` brightens in sync so the eye registers motion across
/// the whole row, not just the text.
/// Build the inline "expanded action" lines for a Tool conv entry
/// whose `action_head && expanded` flags are set. Emits a 2-section
/// view (input, result) prefixed by `    ╎ ` rails so the block
/// reads as a sub-section of the action above it.
fn render_action_expansion(cl: &ConversationLine, panel_width: usize) -> Vec<Line<'static>> {
    let usable = panel_width.saturating_sub(6).max(20);
    let rail = "    ╎ ".to_string();
    let rail_color = Color::Rgb(104, 112, 128);
    let label_color = Color::Rgb(152, 162, 177);
    let body_color = Color::Rgb(170, 180, 200);

    let mut out: Vec<Line<'static>> = Vec::new();

    // ── input section ─────────────────────────────────────────────
    out.push(Line::from(vec![
        Span::styled(rail.clone(), Style::default().fg(rail_color)),
        Span::styled(
            "input  ".to_string(),
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            "─".repeat(usable.saturating_sub(8)),
            Style::default().fg(rail_color),
        ),
    ]));
    let input_json = cl
        .action_input
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "(no input recorded)".into());
    for raw in input_json.lines() {
        let wrapped = if raw.is_empty() {
            vec![std::borrow::Cow::Borrowed("")]
        } else {
            textwrap::wrap(raw, usable)
        };
        for piece in wrapped {
            out.push(Line::from(vec![
                Span::styled(rail.clone(), Style::default().fg(rail_color)),
                Span::styled(piece.to_string(), Style::default().fg(body_color)),
            ]));
        }
    }

    // ── result section ────────────────────────────────────────────
    let result_label = if cl.action_duration_ms > 0 {
        format!("result · {}ms  ", cl.action_duration_ms)
    } else {
        "result  ".to_string()
    };
    out.push(Line::from(vec![
        Span::styled(rail.clone(), Style::default().fg(rail_color)),
        Span::styled(
            result_label.clone(),
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            "─".repeat(usable.saturating_sub(result_label.chars().count())),
            Style::default().fg(rail_color),
        ),
    ]));
    // The output itself, as TEXT, before the envelope. `to_string_pretty`
    // keeps `stdout` as one JSON string — every newline an escaped `\n` — so
    // a 200-line command result came out as a single wall of a line and the
    // reader saw "everything except the output" (owner, 2026-08-17). The
    // textual fields are lifted out, printed with real newlines under their
    // own label, and replaced in the envelope by a pointer to that block.
    let mut envelope = cl.action_result.clone();
    let mut text_blocks: Vec<(String, String)> = Vec::new();
    if let Some(v) = envelope.as_mut() {
        lift_text_fields(v, "", &mut text_blocks);
    }
    for (label, text) in &text_blocks {
        let n = text.lines().count();
        let head = format!("{label} · {n} line{}  ", if n == 1 { "" } else { "s" });
        out.push(Line::from(vec![
            Span::styled(rail.clone(), Style::default().fg(rail_color)),
            Span::styled(
                head.clone(),
                Style::default()
                    .fg(label_color)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                "┄".repeat(usable.saturating_sub(head.chars().count())),
                Style::default().fg(rail_color),
            ),
        ]));
        for raw in text.lines() {
            let wrapped = if raw.is_empty() {
                vec![std::borrow::Cow::Borrowed("")]
            } else {
                textwrap::wrap(raw, usable)
            };
            for piece in wrapped {
                out.push(Line::from(vec![
                    Span::styled(rail.clone(), Style::default().fg(rail_color)),
                    Span::styled(piece.to_string(), Style::default().fg(TEXT_SOFT)),
                ]));
            }
        }
    }
    let result_json = envelope
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_else(|| "(no result recorded)".into());
    for raw in result_json.lines() {
        let wrapped = if raw.is_empty() {
            vec![std::borrow::Cow::Borrowed("")]
        } else {
            textwrap::wrap(raw, usable)
        };
        for piece in wrapped {
            out.push(Line::from(vec![
                Span::styled(rail.clone(), Style::default().fg(rail_color)),
                Span::styled(piece.to_string(), Style::default().fg(body_color)),
            ]));
        }
    }
    // Closer rail
    out.push(Line::from(vec![Span::styled(
        format!("    ╰{}", "─".repeat(usable)),
        Style::default().fg(rail_color),
    )]));
    out
}

/// Keys whose string values are program output rather than metadata. They
/// are what the operator opens an action to read.
const TEXT_FIELD_KEYS: [&str; 6] = ["stdout", "stderr", "content", "text", "diff", "message"];

/// Pull multi-line string fields out of a result envelope, in document order,
/// replacing each in place by a one-line pointer. Recurses through objects
/// and arrays (a `Pipeline` nests one envelope per step). Single-line values
/// stay in the JSON — they read fine there and a block per one-liner would be
/// noise.
fn lift_text_fields(v: &mut serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                let here = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                let lift = TEXT_FIELD_KEYS.contains(&k.as_str())
                    && child.as_str().map(|t| t.contains('\n')).unwrap_or(false);
                if lift {
                    let text = child.as_str().unwrap_or_default().to_string();
                    let bytes = text.len();
                    *child =
                        serde_json::Value::String(format!("<{here}: shown above, {bytes} bytes>"));
                    out.push((here, text));
                } else {
                    lift_text_fields(child, &here, out);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                lift_text_fields(child, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// Wrap a User line's body to the available panel width, returning
/// one ratatui `Line` per visual row. The first row carries the
/// `  › ` prefix; continuation rows are indented to align with the
/// first character of the body so the eye reads them as one block.
///
/// `panel_width` is the renderable column count for the body slice
/// of the conversation pane. We reserve 4 columns for the leading
/// `  › ` glyph + a comfortable 2-col right margin so wrap-induced
/// jitter never lands flush against the panel border.
fn wrap_user_body(text: &str, panel_width: usize) -> Vec<String> {
    // Smallest sensible inner width — clamp so a very narrow panel
    // still renders something.
    let inner = panel_width.saturating_sub(6).max(20);
    let options = textwrap::Options::new(inner).break_words(true);
    textwrap::wrap(text, &options)
        .into_iter()
        .map(|cow| cow.into_owned())
        .collect()
}

fn format_pending_user_line_wrapped(
    line: &ConversationLine,
    phase: u32,
    panel_width: usize,
) -> Vec<Line<'static>> {
    let step = (phase / 6) % 4;
    let (text_color, glyph_color) = match step {
        0 => (VIOLET, VIOLET_DIM),
        1 => (VIOLET_REFLECT_MID, VIOLET),
        2 => (VIOLET_REFLECT_BRIGHT, VIOLET_REFLECT_MID),
        _ => (VIOLET_REFLECT_MID, VIOLET),
    };
    let mut out = Vec::new();
    let rows = wrap_user_body(&line.text, panel_width);
    for (i, row) in rows.into_iter().enumerate() {
        let prefix = if i == 0 { "  › " } else { "    " };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(glyph_color)),
            Span::styled(
                row,
                Style::default()
                    .fg(text_color)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    if out.is_empty() {
        // Defensive: an empty pending line still gets the glyph so
        // the user sees that something is in flight.
        out.push(Line::from(Span::styled(
            "  › ",
            Style::default().fg(glyph_color),
        )));
    }
    out
}

fn format_user_line_wrapped(line: &ConversationLine, panel_width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let rows = wrap_user_body(&line.text, panel_width);
    for (i, row) in rows.into_iter().enumerate() {
        let prefix = if i == 0 { "  › " } else { "    " };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(VIOLET_DIM)),
            Span::styled(
                row,
                Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "  › ",
            Style::default().fg(VIOLET_DIM),
        )));
    }
    out
}

#[allow(dead_code)]
fn format_pending_user_line(line: &ConversationLine, phase: u32) -> Line<'static> {
    // 4 stops × 6 ticks = 24-tick cycle. Bright stop is centred so
    // the wave has a clear "peak" — that's what makes the effect
    // read as a reflection passing through the line.
    let step = (phase / 6) % 4;
    let (text_color, glyph_color) = match step {
        0 => (VIOLET, VIOLET_DIM),
        1 => (VIOLET_REFLECT_MID, VIOLET),
        2 => (VIOLET_REFLECT_BRIGHT, VIOLET_REFLECT_MID),
        _ => (VIOLET_REFLECT_MID, VIOLET),
    };
    Line::from(vec![
        Span::styled("  › ", Style::default().fg(glyph_color)),
        Span::styled(
            line.text.clone(),
            Style::default()
                .fg(text_color)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

fn format_line(line: &ConversationLine) -> Line<'static> {
    match line.kind {
        // User — quoted, indented, in a softer violet that doesn't
        // compete with the agent's responses. Italic + a guillemet
        // makes it scan as "their input" at a glance.
        LineKind::User => Line::from(vec![
            Span::styled("  › ", Style::default().fg(VIOLET_DIM)),
            Span::styled(
                line.text.clone(),
                Style::default().fg(VIOLET).add_modifier(Modifier::ITALIC),
            ),
        ]),
        // Assistant — each line is one row of the `╭─ x0ne ─… / │ …
        // / ╰─…` box generated by `x0ne_response_box_lines`. The
        // strings carry embedded ANSI escapes (charcoal-grey border,
        // brighter-grey label) so we just feed them through
        // ansi-to-tui and let the styling flow through unchanged.
        // No `x0ne ❯` prefix here — the box itself carries the
        // `x0ne` label on its top border, prefixing every row would
        // print `x0ne › ╭─ x0ne ─…` and that's exactly the bug we
        // saw in the user's screenshot ("ces affreux x0ne > x0ne >").
        LineKind::Assistant => {
            use ansi_to_tui::IntoText;
            match line.text.as_bytes().into_text() {
                Ok(mut text) => text.lines.pop().unwrap_or_default(),
                Err(_) => Line::from(Span::styled(line.text.clone(), Style::default().fg(TEXT))),
            }
        }
        LineKind::Tool => {
            // The CLI's `display::result_line` already produced an
            // ANSI-colored string with all the glyphs (└, │, ✓/✗),
            // spacing, and per-action previews. We just parse it
            // into ratatui spans so the look matches the rustyline
            // CLI byte-for-byte. If parsing fails (rare — malformed
            // ANSI), we fall back to a dim plain-text rendering so
            // the line is still readable.
            use ansi_to_tui::IntoText;
            match line.text.as_bytes().into_text() {
                Ok(mut text) => text.lines.pop().unwrap_or_default(),
                Err(_) => Line::from(Span::styled(
                    line.text.clone(),
                    Style::default().fg(TEXT_SOFT),
                )),
            }
        }
        LineKind::System => Line::from(vec![
            Span::styled("  ⋮ ", Style::default().fg(FAINT)),
            Span::styled(
                line.text.clone(),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]),
        LineKind::Error => Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(BAD)),
            Span::styled(line.text.clone(), Style::default().fg(BAD)),
        ]),
    }
}

// ── Side panel ───────────────────────────────────────────────────────────

fn draw_side_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    // The tab strip lives in the top border, not in a row of its own. That
    // is the legacy trick — a title inset into the frame — and it hands two
    // rows back to the panel content on every terminal size.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(focus_border(state, crate::tui::app::FocusRegion::SidePanel))
        .title_top(panel_switcher_line(state.side_panel, area.width));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match state.side_panel {
        SidePanel::Conversation => conversation_panel_hint(),
        SidePanel::Queue => queue_panel_lines(state),
        SidePanel::Tasks => tasks_panel_lines(state),
        SidePanel::Cron => cron_panel_lines(state),
        SidePanel::Pipeline => pipeline_panel_lines(state),
        SidePanel::Sources => sources_panel_lines(state),
        SidePanel::Pulse => pulse_panel_lines(state),
        SidePanel::Infra => infra_panel_lines(state),
        SidePanel::Snapshots => placeholder_lines(
            "snapshots",
            "Stage 2 wires this to x0ne_runtime::CreateSnapshot / ListSnapshots.",
        ),
        SidePanel::Handles => placeholder_lines(
            "handles",
            "Stage 2 wires this to the orchestrator's handle registry.",
        ),
        SidePanel::Worlds => worlds_panel_lines(state),
        SidePanel::Goal => goal_panel_lines(state),
        SidePanel::Data => data_panel_lines(state),
        SidePanel::Matrix => matrix_panel_lines(state),
        SidePanel::Shell => {
            draw_shell_panel(frame, inner, state);
            return;
        }
        SidePanel::Hooks => hooks_panel_lines(state),
    };
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);
}

/// The Shell tab: the same live PTY as full-page shell mode, in the panel.
/// Focus the panel (Tab to it) and type — keys route to the shell, sudo
/// prompts and pastes included, because it is a real pseudo-terminal.
fn draw_shell_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(pty) = state.pty.as_ref() else {
        let hint = placeholder_lines(
            "shell",
            "No shell session. With this panel focused, press Enter to start \
             one — the same session Ctrl+S opens full-page. When the shell \
             exits (Ctrl+D), Tab cycles panels again and Enter restarts.",
        );
        frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: false }), area);
        return;
    };
    frame.render_widget(Paragraph::new(pty_screen_lines(pty)), area);
}

/// The tab strip, sized to the frame it sits in.
///
/// Eleven panels never fit across a side panel, and a title clipped
/// mid-glyph reads as a bug rather than as more content. So the strip
/// scrolls: it shows a window around the active tab and marks each side it
/// has run past, which is the same affordance a tab bar gives anywhere else.
fn panel_switcher_line(active: SidePanel, width: u16) -> Line<'static> {
    let all = SidePanel::all();
    let active_idx = all.iter().position(|p| *p == active).unwrap_or(0);
    // Two corners, one leading space, and room for the two markers.
    let budget = width.saturating_sub(6) as usize;

    // Grow a window outward from the active tab for as long as it fits.
    let mut first = active_idx;
    let mut last = active_idx;
    let label_len = |i: usize| all[i].label().chars().count() + 3; // glyph + spaces
    let mut used = label_len(active_idx);
    loop {
        let can_extend_right = last + 1 < all.len() && used + label_len(last + 1) <= budget;
        let can_extend_left = first > 0 && used + label_len(first - 1) <= budget;
        if can_extend_right {
            last += 1;
            used += label_len(last);
        } else if can_extend_left {
            first -= 1;
            used += label_len(first);
        } else {
            break;
        }
    }

    let mut spans = vec![Span::raw(" ")];
    if first > 0 {
        spans.push(Span::styled("‹ ", Style::default().fg(FAINT)));
    }
    for (n, p) in all[first..=last].iter().enumerate() {
        if n > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(FAINT)));
        }
        let style = if *p == active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled(format!("{} {}", p.glyph(), p.label()), style));
    }
    if last + 1 < all.len() {
        spans.push(Span::styled(" ›", Style::default().fg(FAINT)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn conversation_panel_hint() -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Conversation is shown on the left.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Tab to switch to:",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  📋 queue   pending messages",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  ◐ snaps    restore points",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  ⏳ handles  async processes",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  ▦ worlds   parallel agents",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  ◆ goal     active goal + steps",
            Style::default().fg(DIM),
        )),
    ]
}

fn queue_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let snap = state.queue_snapshot();
    if snap.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled("  Queue is empty.", Style::default().fg(DIM))),
            Line::from(""),
            Line::from(Span::styled(
                "  While the agent is working, your messages",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  stack here in order. Press Enter to send.",
                Style::default().fg(FAINT),
            )),
        ];
    }

    let mut out = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  {} message{} queued, in order:",
                snap.len(),
                if snap.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(ACCENT),
        )),
        Line::from(""),
    ];
    for (i, msg) in snap.iter().enumerate() {
        let truncated: String = msg.chars().take(60).collect();
        let suffix = if msg.chars().count() > 60 { "…" } else { "" };
        out.push(Line::from(vec![
            Span::styled(format!("  {:>2}. ", i + 1), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{}{}", truncated, suffix),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        "  /queue clear · /queue drop N",
        Style::default().fg(FAINT),
    )));
    out
}

/// Tasks panel — live TODO surface the agent populates via TaskAdd /
/// TaskDone. Open tasks at the top with a teal accent dot, completed
/// ones below dimmed with a check mark.
fn tasks_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.tasks.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled("  No tasks yet.", Style::default().fg(DIM))),
            Line::from(""),
            Line::from(Span::styled(
                "  The agent populates this panel with",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  TaskAdd / TaskDone as it works through a plan.",
                Style::default().fg(FAINT),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Try: \"plan in 4 steps and externalise each one\"",
                Style::default().fg(FAINT),
            )),
        ];
    }
    let open = state.tasks.iter().filter(|t| !t.done).count();
    let done = state.tasks.len() - open;
    let mut out = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {} open · {} done", open, done),
            Style::default().fg(ACCENT),
        )),
        Line::from(""),
    ];
    for t in &state.tasks {
        let (glyph, glyph_style, text_style) = if t.done {
            (
                "✓",
                Style::default().fg(ACCENT_MID),
                Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT),
            )
        } else {
            ("●", Style::default().fg(ACCENT), Style::default().fg(TEXT))
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", glyph), glyph_style),
            Span::styled(format!("#{:<3} ", t.id), Style::default().fg(FAINT)),
            Span::styled(t.title.clone(), text_style),
        ]));
    }
    out
}

/// Cron panel — pending scheduled actions with a live countdown
/// rendered as `in 2m 13s` / `in 4h`. Fired entries fade out (dim +
/// strike) until cleared.
fn cron_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.crons.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No scheduled crons.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  CronSchedule lets the agent set timers or",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  self-trigger after a delay. The runner ticks",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  once per second and fires due items.",
                Style::default().fg(FAINT),
            )),
        ];
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pending = state.crons.iter().filter(|c| !c.fired).count();
    let mut out = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  {} pending · {} fired",
                pending,
                state.crons.len() - pending
            ),
            Style::default().fg(ACCENT),
        )),
        Line::from(""),
    ];
    for c in &state.crons {
        let (countdown, color) = if c.fired {
            ("fired".to_string(), DIM)
        } else if c.due_at_unix <= now {
            ("now".to_string(), WARN)
        } else {
            (fmt_remaining(c.due_at_unix - now), ACCENT)
        };
        let title_style = if c.fired {
            Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT)
        } else {
            Style::default().fg(TEXT)
        };
        let glyph = if c.trigger_prompt.is_some() {
            "⚡"
        } else {
            "⏰"
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
            Span::styled(format!("#{:<3} ", c.id), Style::default().fg(FAINT)),
            Span::styled(c.label.clone(), title_style),
            Span::raw(" "),
            Span::styled(format!("· {}", countdown), Style::default().fg(color)),
        ]));
        if let Some(p) = &c.trigger_prompt {
            let truncated: String = p.chars().take(50).collect();
            let suffix = if p.chars().count() > 50 { "…" } else { "" };
            out.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    format!("→ {}{}", truncated, suffix),
                    Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    out
}

fn fmt_remaining(secs: u64) -> String {
    if secs < 60 {
        format!("in {}s", secs)
    } else if secs < 3600 {
        format!("in {}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("in {}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("in {}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Pipeline panel — most-recent Pipeline action with per-step
/// breakdown. Mirrors tui1's pipeline-preview but sticky in the
/// side panel so the user can study the run without scrolling the
/// conversation trail. Re-renders the run header (X/Y · Zms · ok)
/// then a list with one row per step (idx · type · status · dur ·
/// 1-line summary). Failed steps go red, ok teal, skipped grey.
fn pipeline_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(run) = state.last_pipeline.as_ref() else {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No Pipeline run yet.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Once the agent dispatches a Pipeline, its",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  per-step breakdown lands here — type, status,",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  duration, and a one-line summary per step.",
                Style::default().fg(FAINT),
            )),
        ];
    };

    let header_color = if run.failed { WARN } else { ACCENT };
    let header_icon = if run.failed { "✗" } else { "✓" };
    let mut out = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {} ", header_icon),
                Style::default().fg(header_color),
            ),
            Span::styled(
                format!(
                    "Pipeline · {}/{} step{} · {}",
                    run.completed_steps,
                    run.total_steps,
                    if run.total_steps == 1 { "" } else { "s" },
                    fmt_ms(run.total_duration_ms),
                ),
                Style::default()
                    .fg(header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    for step in &run.steps {
        let (glyph, glyph_color, text_color) = match step.status.as_str() {
            "ok" => ("✓", ACCENT_MID, TEXT),
            "failed" => ("✗", WARN, WARN),
            _ => ("◌", FAINT, DIM),
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", glyph), Style::default().fg(glyph_color)),
            Span::styled(format!("{:>2}. ", step.idx + 1), Style::default().fg(FAINT)),
            Span::styled(
                format!("{:<14}", step.action_type),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(fmt_ms(step.duration_ms), Style::default().fg(FAINT)),
        ]));
        if !step.summary.is_empty() {
            out.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(step.summary.clone(), Style::default().fg(text_color)),
            ]));
        }
    }
    out
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Sources panel — every URL the agent has touched this session.
/// Auto-populated from FetchUrl / BrowseWeb / WebSearch results;
/// the LLM doesn't have to think about it. Each row shows origin
/// glyph + (truncated) title + ok/fail + age + the URL on a
/// continuation line so the user can click in their terminal.
fn sources_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.sources.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled("  No sources yet.", Style::default().fg(DIM))),
            Line::from(""),
            Line::from(Span::styled(
                "  Every URL fetched (FetchUrl, BrowseWeb) and every",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  search hit (WebSearch) lands here so you can trace",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  what the agent read. Full request + result is in",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  ~/.x0ne/sessions/<ts>.jsonl for audit.",
                Style::default().fg(FAINT),
            )),
        ];
    }
    let now = std::time::Instant::now();
    let mut out = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "  {} source{}",
                state.sources.len(),
                if state.sources.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(ACCENT),
        )),
        Line::from(""),
    ];
    // Show the most recent 30 to keep the panel readable; older are
    // still in the jsonl.
    let start = state.sources.len().saturating_sub(30);
    for src in &state.sources[start..] {
        let (glyph, color) = match (src.origin.as_str(), src.ok) {
            (_, false) => ("✗", WARN),
            ("WebSearch", _) => ("🔍", ACCENT_MID),
            ("BrowseWeb", _) => ("🌐", ACCENT_MID),
            (_, _) => ("●", ACCENT_MID),
        };
        let ago = now.saturating_duration_since(src.at).as_secs();
        let ago_s = if ago < 60 {
            format!("{}s", ago)
        } else if ago < 3600 {
            format!("{}m", ago / 60)
        } else {
            format!("{}h", ago / 3600)
        };
        // Title row.
        let mut title_spans = vec![
            Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
            Span::styled(
                src.cite
                    .as_ref()
                    .map(|c| format!("[{c}] "))
                    .unwrap_or_default(),
                Style::default().fg(FAINT),
            ),
            Span::styled(
                crate::tui::app::trim_inline(&src.title, 50),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {}", ago_s), Style::default().fg(FAINT)),
        ];
        if let Some(q) = &src.query {
            title_spans.push(Span::styled(
                format!(" · q: {}", crate::tui::app::trim_inline(q, 30)),
                Style::default().fg(VIOLET),
            ));
        }
        out.push(Line::from(title_spans));
        // URL on a continuation row — many terminals auto-detect URLs
        // and make them clickable.
        out.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(
                crate::tui::app::trim_inline(&src.url, 76),
                Style::default()
                    .fg(FAINT)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]));
    }
    out
}

/// Pulse panel — live session timeline. Each AgentEvent we care about
/// becomes a coloured dot + label, newest at the bottom (latest is
/// always in view when the panel is opened). The dot colour encodes
/// the event kind so a glance tells the story: teal = turn, accent =
/// tool ok, warn = tool error, violet = ask, gold = goal, etc.
/// The Infra panel: burn rate, per-resource $/h, GPU gauges with attached
/// processes, and whether the billed figure is real or estimated. Every
/// number is labelled for what it is — an estimate is never dressed as a
/// bill, and an unpriced resource is listed, not zeroed.
fn infra_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(cloud) = state.cloud_state.as_ref() else {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Cloud monitoring is disabled.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Enable with [cloud] enabled = true in ~/.x0ne/config.toml,",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  and allowlist gcloud / nvidia-smi in policy.allowed_commands.",
                Style::default().fg(FAINT),
            )),
        ];
    };
    let cloud = cloud.read().unwrap_or_else(|e| e.into_inner());
    let mut out = vec![Line::from("")];

    match &cloud.burn {
        Some(b) => {
            out.push(Line::from(Span::styled(
                format!(
                    "  burn  {:.2}$/h · {:.0}$/day (estimated)",
                    b.hourly_usd, b.per_day_usd
                ),
                Style::default().fg(Color::Rgb(0, 200, 220)),
            )));
            for p in &b.priced {
                out.push(Line::from(Span::styled(
                    format!("    {:>7.2}$/h  {}", p.hourly_usd, p.resource.name),
                    Style::default().fg(Color::Rgb(180, 200, 220)),
                )));
            }
            if !b.unpriced.is_empty() {
                out.push(Line::from(Span::styled(
                    "  price unknown (not counted):",
                    Style::default().fg(Color::Yellow),
                )));
                for r in &b.unpriced {
                    out.push(Line::from(Span::styled(
                        format!(
                            "    {}  ({})",
                            r.name,
                            r.machine_type.as_deref().unwrap_or("?")
                        ),
                        Style::default().fg(Color::Yellow),
                    )));
                }
            }
        }
        None => out.push(Line::from(Span::styled(
            "  no cost data yet — run CloudCost or wait for the monitor",
            Style::default().fg(DIM),
        ))),
    }

    out.push(Line::from(""));
    for g in &cloud.gpus {
        let bar_w = 10usize;
        let filled = ((g.util_pct as usize * bar_w) / 100).min(bar_w);
        let bar = format!("{}{}", "━".repeat(filled), "╌".repeat(bar_w - filled));
        let color = if g.temp_c > 85 {
            Color::Red
        } else if g.temp_c > 70 {
            Color::Yellow
        } else {
            Color::Rgb(0, 200, 120)
        };
        out.push(Line::from(Span::styled(
            format!(
                "  gpu{} {} {:>3}%  {}/{}MB  {}°C {}W · {} proc",
                g.index,
                bar,
                g.util_pct,
                g.mem_used_mb,
                g.mem_total_mb,
                g.temp_c,
                g.power_w,
                g.processes.len()
            ),
            Style::default().fg(color),
        )));
    }

    for a in &cloud.alerts {
        out.push(Line::from(Span::styled(
            format!("  ⚠ {}", a),
            Style::default().fg(Color::Red),
        )));
    }

    out.push(Line::from(""));
    match cloud.billed_yesterday {
        Some(v) => out.push(Line::from(Span::styled(
            format!("  billed yesterday: {:.2}$", v),
            Style::default().fg(FAINT),
        ))),
        None => out.push(Line::from(Span::styled(
            "  billed: estimated only (no billing export configured)",
            Style::default().fg(FAINT),
        ))),
    }
    if !cloud.pricing_as_of.is_empty() {
        out.push(Line::from(Span::styled(
            format!("  prices as of {}", cloud.pricing_as_of),
            Style::default().fg(FAINT),
        )));
    }
    out
}

fn pulse_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    use crate::tui::app::PulseKind;
    if state.pulse.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Session is quiet.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Pulse is the live heartbeat of the agent —",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  turns, tools, snapshots, questions, crons.",
                Style::default().fg(FAINT),
            )),
            Line::from(Span::styled(
                "  Everything that matters lands here.",
                Style::default().fg(FAINT),
            )),
        ];
    }
    let now = std::time::Instant::now();
    let mut out = vec![Line::from("")];
    // Show the newest ~40 events — older history scrolls off the
    // top, the agent has the full trail in the conversation if
    // needed.
    let start = state.pulse.len().saturating_sub(40);
    for ev in &state.pulse[start..] {
        let (glyph, color): (&str, ratatui::style::Color) = match ev.kind {
            PulseKind::Turn => ("◆", ACCENT),
            PulseKind::Tool => ("●", ACCENT_MID),
            PulseKind::ToolError => ("●", WARN),
            PulseKind::Snapshot => ("◐", VIOLET),
            PulseKind::Ask => ("?", VIOLET),
            PulseKind::Goal => ("◇", Color::Rgb(251, 191, 36)),
            PulseKind::Task => ("✓", ACCENT),
            PulseKind::Cron => ("⏰", ACCENT_MID),
        };
        let ago = now.saturating_duration_since(ev.at).as_secs();
        let ago_str = if ago < 60 {
            format!("{}s", ago)
        } else if ago < 3600 {
            format!("{}m", ago / 60)
        } else {
            format!("{}h", ago / 3600)
        };
        out.push(Line::from(vec![
            Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
            Span::styled(format!("{:>4} ", ago_str), Style::default().fg(FAINT)),
            Span::styled(ev.label.clone(), Style::default().fg(TEXT)),
        ]));
    }
    out
}

fn placeholder_lines(panel: &'static str, note: &'static str) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {} · placeholder", panel),
            Style::default().fg(WARN),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", note),
            Style::default().fg(DIM),
        )),
    ]
}

// ── Input zone ───────────────────────────────────────────────────────────

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    // Rounded box around the input — the frame's border for the user's
    // voice. Border in the same discreet charcoal grey the CLI uses
    // for `response_box` (truecolor 50,55,65) so the frame whispers.
    // Only the `❯` chevron in the title carries the teal accent —
    // it's the prompt indicator, the one visual cue that says "speak."
    // In plan mode the chevron says so, where the user types.
    let (chevron, color) = if state.plan_mode {
        (" plan ❯ ", PLAN)
    } else {
        (" ❯ ", ACCENT)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FRAME))
        .title(Span::styled(
            chevron,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The TextArea renders itself; we just hand it an area.
    frame.render_widget(&state.input, inner);
}

// ── Live status row ──────────────────────────────────────────────────────
//
// One greyed, italic line that shows what the agent is doing right
// now: thinking before the first token, writing the response, or
// dispatching a tool. Anchored just above the input bar so the user's
// gaze lands on it the moment something takes longer than instant.
//
// Three states (highest priority first):
//   1. Tool in flight     → "▶ x0ne is running <Action> · 2.3s"
//   2. Awaiting 1st token → "⋯ x0ne is thinking · 0.4s"
//   3. Streaming tokens   → "✎ x0ne is writing · 0.8s"
//
// When none apply we don't render anything (the layout drops the
// 1-row strip entirely, no wasted space).

fn live_status_active(state: &AppState) -> bool {
    state.last_action_started.is_some()
        || state.awaiting_first_token
        || (state.agent_busy && !state.assistant_buffer.is_empty())
        || state.idle_status.is_some()
}

fn draw_live_status(frame: &mut Frame, area: Rect, state: &AppState) {
    // Spinner glyph cycles every ~320ms (4 ticks @ 80ms each) — slow
    // enough to be a heartbeat, not a strobe.
    let spin = ["⋯", "⋱", "⋮", "⋰"];
    let i = (state.animation_phase / 4) as usize % spin.len();

    // Idle presence: "connected · ready when you are" lives here, above the
    // input, and is replaced the moment any activity status exists.
    let activity = state.last_action_started.is_some()
        || state.awaiting_first_token
        || (state.agent_busy && !state.assistant_buffer.is_empty());
    if !activity {
        if let Some(idle) = state.idle_status.as_ref() {
            // Two facts, not one. "connected" says a bridge answered;
            // the enclave badge says whether what runs behind it is protected
            // from its own host. They are independent, and the second is the
            // one an operator staking weights on a rented GPU needs in view
            // without asking.
            let (glyph, label, rgb) = state.enclave.badge();
            let row = Line::from(vec![
                Span::raw("  "),
                Span::styled("●", Style::default().fg(Color::Rgb(0, 200, 120))),
                Span::raw(" "),
                Span::styled(idle.clone(), Style::default().fg(DIM)),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(glyph, Style::default().fg(Color::Rgb(rgb.0, rgb.1, rgb.2))),
                Span::raw(" "),
                Span::styled(label, Style::default().fg(Color::Rgb(rgb.0, rgb.1, rgb.2))),
            ]);
            frame.render_widget(Paragraph::new(row), area);
            return;
        }
    }

    // Pick the most informative state.
    let (glyph, label, elapsed_s) = if let Some(started) = state.last_action_started {
        let action_name = state.last_action_name.as_deref().unwrap_or("action");
        let secs = started.elapsed().as_millis() as f64 / 1000.0;
        ("▶", format!("x0ne is running {}", action_name), secs)
    } else if state.awaiting_first_token {
        let secs = state
            .turn_started_at
            .map(|t| t.elapsed().as_millis() as f64 / 1000.0)
            .unwrap_or(0.0);
        (spin[i], "x0ne is thinking".to_string(), secs)
    } else if state.agent_busy && !state.assistant_buffer.is_empty() {
        let secs = state
            .turn_started_at
            .map(|t| t.elapsed().as_millis() as f64 / 1000.0)
            .unwrap_or(0.0);
        let tok = state.assistant_buffer.split_whitespace().count();
        let label = if tok > 0 {
            format!("x0ne is writing · ~{} tok", tok)
        } else {
            "x0ne is writing".to_string()
        };
        ("✎", label, secs)
    } else {
        // Shouldn't happen — live_status_active guards.
        return;
    };

    let elapsed_str = if elapsed_s >= 60.0 {
        format!("{}m{:02}s", elapsed_s as u64 / 60, elapsed_s as u64 % 60)
    } else if elapsed_s >= 10.0 {
        format!("{:.0}s", elapsed_s)
    } else {
        format!("{:.1}s", elapsed_s)
    };

    let line = Line::from(vec![
        Span::styled(format!("  {} ", glyph), Style::default().fg(ACCENT_MID)),
        Span::styled(
            label,
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ),
        Span::styled(format!(" · {}", elapsed_str), Style::default().fg(FAINT)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── Shell mode page — embedded PTY rendering ─────────────────────────────
//
// Paints the vt100::Screen owned by `state.pty` as a grid of ratatui
// cells. The PTY's reader thread keeps the screen up to date in the
// background; we just lock + read on each frame. Resize is
// propagated to the PTY so apps like vim / htop get the real
// dimensions.

fn draw_shell_page(frame: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_DIM))
        .title(Span::styled(
            " 🐚  shell · ^\\ back to agent · ^Alt+C quits ".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(pty) = state.pty.as_ref() else {
        // Fallback: no PTY yet (spawn failed). Show a friendly hint.
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No shell session.",
                Style::default().fg(DIM),
            )),
        ]);
        frame.render_widget(p, inner);
        return;
    };

    let p = Paragraph::new(pty_screen_lines(pty));
    frame.render_widget(p, inner);
}

/// The PTY's screen grid as styled lines — one renderer for both viewports
/// (the full page and the Shell side panel). A poisoned parser (reader
/// thread panicked, rare) renders as a one-line notice instead.
fn pty_screen_lines(pty: &crate::tui::pty_term::PtyTerm) -> Vec<Line<'static>> {
    match pty.parser.lock() {
        Ok(parser) => {
            let screen = parser.screen();
            let (rows, cols) = screen.size();
            let mut out: Vec<Line<'static>> = Vec::with_capacity(rows as usize);
            for r in 0..rows {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols as usize);
                for c in 0..cols {
                    if let Some(cell) = screen.cell(r, c) {
                        let contents = cell.contents();
                        let glyph: String = if contents.is_empty() {
                            " ".to_string()
                        } else {
                            contents.to_string()
                        };
                        let style = vt100_cell_to_ratatui_style(cell);
                        spans.push(Span::styled(glyph, style));
                    } else {
                        spans.push(Span::raw(" "));
                    }
                }
                out.push(Line::from(spans));
            }
            out
        }
        Err(_) => vec![Line::from(Span::styled(
            "  (terminal parser unavailable — try `^\\` to reset)",
            Style::default().fg(WARN),
        ))],
    }
}

/// Convert a vt100 cell's style attributes to a ratatui Style.
/// Maps fg/bg colours, bold/italic/underline. We don't reach for
/// every SGR attribute the parser exposes — just the ones modern
/// CLIs actually use.
fn vt100_cell_to_ratatui_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    let bg = vt100_color_to_ratatui(cell.bgcolor());
    if bg != Color::Reset {
        style = style.bg(bg);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn vt100_color_to_ratatui(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => match i {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::White,
            _ => Color::Indexed(i),
        },
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

// ── AskUser modal ────────────────────────────────────────────────────────
//
// Drawn in the bottom slot in place of the textarea while an AskState is
// active. Mirrors Claude Code's MCQ pattern: question header, list of
// options with check/dot glyphs and the focused row underlined, the
// active option's description rendered dim under it, and a single help
// footer with the four shortcuts (Up/Down · Space/Enter · Esc · ^C).

fn draw_ask_modal(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(ask) = state.pending_ask.as_ref() else {
        return;
    };

    let frame_grey = Color::Rgb(104, 112, 128);
    let title = if ask.multi_select {
        " ❯ pick one or more · Space to toggle · Enter to confirm "
    } else {
        " ❯ pick one · Enter to confirm "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_grey))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Question header — bold ink, the actual ask. We don't markdown-
    // parse here (the LLM already keeps these short by contract); a
    // single emphasized line reads cleanly without extra cost.
    lines.push(Line::from(Span::styled(
        ask.question.clone(),
        Style::default()
            .fg(Color::Rgb(220, 225, 235))
            .add_modifier(Modifier::BOLD),
    )));
    if let Some(hint) = ask.hint.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled(
            hint.clone(),
            Style::default().fg(DIM),
        )));
    }

    // Filter row: what the operator typed, and how many rows it lets
    // through. Present even when empty — it is where letters go, and the
    // counter is what tells a 122-row list from a 3-row one.
    let visible = ask.visible();
    let counter = format!("{}/{}", visible.len(), ask.options.len());
    lines.push(Line::from(vec![
        Span::styled(" ⌕ ".to_string(), Style::default().fg(ACCENT_MID)),
        Span::styled(
            if ask.filter.is_empty() {
                "type to filter…".to_string()
            } else {
                ask.filter.clone()
            },
            Style::default().fg(if ask.filter.is_empty() { FAINT } else { TEXT }),
        ),
        Span::styled(format!("  {counter}"), Style::default().fg(FAINT)),
    ]));

    // Rows: a WINDOW of the visible rows around the cursor. Each row is one
    // line — label, then the description dim on the same line — so a long
    // list stays dense. Rows above/below the window are announced.
    let fixed = lines.len() as u16 + 1 /* footer */;
    let capacity = inner.height.saturating_sub(fixed).max(1) as usize;
    let cursor = ask.cursor.min(visible.len().saturating_sub(1));
    let start = if visible.len() <= capacity {
        0
    } else {
        // keep the cursor inside the window, scrolling by whole rows
        cursor
            .saturating_sub(capacity / 2)
            .min(visible.len() - capacity)
    };
    let end = (start + capacity).min(visible.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("   … {start} above"),
            Style::default().fg(FAINT),
        )));
    }
    let shown_capacity = capacity
        .saturating_sub(usize::from(start > 0))
        .saturating_sub(usize::from(end < visible.len()));
    let end = (start + shown_capacity).min(visible.len()).max(start);
    let width = inner.width as usize;
    for (pos, &i) in visible[start..end].iter().enumerate() {
        let row = &ask.options[i];
        let focused = start + pos == cursor;
        let glyph: &str = if ask.multi_select {
            if row.selected {
                "[✓]"
            } else {
                "[ ]"
            }
        } else if focused {
            "◉"
        } else {
            "○"
        };
        let caret = if focused { "›" } else { " " };
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(
            format!(" {} ", caret),
            Style::default().fg(ACCENT),
        ));
        spans.push(Span::styled(
            format!("{} ", glyph),
            Style::default().fg(if row.selected || (!ask.multi_select && focused) {
                ACCENT
            } else {
                FAINT
            }),
        ));
        let label_color = if focused {
            Color::Rgb(225, 230, 240)
        } else {
            Color::Rgb(170, 180, 200)
        };
        spans.push(Span::styled(
            row.option.label.clone(),
            Style::default().fg(label_color).add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ));
        if row.option.recommended {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "(Recommended)".to_string(),
                Style::default()
                    .fg(ACCENT_MID)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        if let Some(desc) = row.option.description.as_ref().filter(|s| !s.is_empty()) {
            // Same line, dim, cut to what is left of the row.
            let used: usize = spans
                .iter()
                .map(|sp| sp.content.chars().count())
                .sum::<usize>()
                + 3;
            let room = width.saturating_sub(used);
            if room > 4 {
                spans.push(Span::styled(
                    "  ·  ".to_string(),
                    Style::default().fg(FAINT),
                ));
                spans.push(Span::styled(
                    super::app::trim_inline(desc, room.saturating_sub(5)),
                    Style::default().fg(DIM),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    if end < visible.len() {
        lines.push(Line::from(Span::styled(
            format!("   … {} below", visible.len() - end),
            Style::default().fg(FAINT),
        )));
    }
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "   nothing matches — Backspace widens".to_string(),
            Style::default().fg(FAINT),
        )));
    }

    // Footer — kept on a single line for compactness.
    let footer = if ask.multi_select {
        " ↑/↓ navigate · Space toggle · type to filter · Enter submit · Esc cancel"
    } else {
        " ↑/↓ navigate · type to filter · Enter pick · Esc cancel"
    };
    lines.push(Line::from(Span::styled(
        footer.to_string(),
        Style::default().fg(FAINT),
    )));

    let p = Paragraph::new(lines);
    frame.render_widget(p, inner);
}

// ── Slash hint bar ───────────────────────────────────────────────────────

// ── Help overlay (F1) ────────────────────────────────────────────────────
//
// Centered floating card listing every interactive primitive in one
// shot: keyboard shortcuts (left column), slash commands (middle),
// LLM-side meta-actions (right). The whole card is read-only — any
// key dismisses it. Designed to feel less like an alert and more like
// flipping open a cheat-sheet inside the frame.

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    // Floating card: 90% of the body width, 80% of the height, both
    // capped so the card stays comfortably readable on a big screen
    // without losing density on a small one.
    let card_w = area.width.saturating_sub(4).min(108).max(60);
    let card_h = area.height.saturating_sub(2).min(28).max(18);
    let x = area.x + (area.width.saturating_sub(card_w)) / 2;
    let y = area.y + (area.height.saturating_sub(card_h)) / 2;
    let card_area = Rect {
        x,
        y,
        width: card_w,
        height: card_h,
    };

    // First, clear the area so the conversation doesn't bleed
    // through the card. Then paint the bordered box.
    frame.render_widget(Clear, card_area);

    let frame_grey = Color::Rgb(104, 112, 128);
    let card_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_grey))
        .title(Span::styled(
            " ✦  x0ne · cheat sheet ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Center);
    let inner = card_block.inner(card_area);
    frame.render_widget(card_block, card_area);

    // Three columns of equal weight: keys · panels+slash · LLM actions.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(inner);

    // Helper to build a column with a teal header + dim-grey rows.
    let column = |title: &str, rows: Vec<(&str, &str)>| -> Paragraph<'static> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {}", title),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "  ─────────────".to_string(),
            Style::default().fg(FAINT),
        )));
        for (key, desc) in rows {
            // Two-column row: key (teal-dim, fixed-width-ish) +
            // description (soft grey).
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{:<14}", key),
                    Style::default().fg(ACCENT_MID).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(TEXT_SOFT)),
            ]));
        }
        Paragraph::new(lines).wrap(Wrap { trim: false })
    };

    let keys = column(
        "KEYBOARD",
        vec![
            ("Tab", "next panel"),
            ("Shift+Tab", "plan mode on/off"),
            ("^B", "focus mode"),
            ("^S", "shell mode"),
            ("^E", "expand actions"),
            ("↑ / ↓", "history / scroll"),
            ("PgUp / PgDn", "scroll a page"),
            ("Home / End", "scroll edges"),
            ("Wheel", "scroll fine"),
            ("Esc", "close overlay"),
            ("F1 / ^?", "this help"),
            ("^C", "stop the turn · quit"),
        ],
    );

    let slash = column(
        "SLASH & PANELS",
        vec![
            ("/task add", "queue a todo"),
            ("/task done N", "complete task N"),
            ("/task clear", "wipe tasks"),
            ("/cron in N", "schedule timer"),
            ("/cron cancel", "kill cron"),
            ("/sessions", "pick a session to resume"),
            ("/resume ID", "reopen that session"),
            ("/session", "this session"),
            ("/plan · /build", "plan mode on · off"),
            ("/undo", "restore the agent's last write"),
            ("/model", "pick a provider"),
            ("/playground [N]", "run the exercises"),
            ("Tab on /…", "complete the command"),
            ("/help · /?", "this help"),
            ("/quit · :q", "exit"),
            ("", ""),
            ("● conv", "main thread"),
            ("✓ tasks", "live TODO"),
            ("⏰ cron", "timers"),
            ("▶ pipeline", "last run"),
            ("❤ pulse", "session beat"),
        ],
    );

    let actions = column(
        "AGENT ACTIONS",
        vec![
            ("AskUser", "MCQ to you"),
            ("PaintBadge", "header art"),
            ("TaskAdd", "external plan"),
            ("TaskDone", "mark a step"),
            ("CronSchedule", "self-trigger"),
            ("Simulate", "dry-run any"),
            ("RunInSandbox", "isolated exec"),
            ("Pipeline", "chain steps"),
            ("Try", "auto-snapshot"),
            ("Subscribe", "watch handle"),
            ("", ""),
            ("• Read-only", "free to chain"),
            ("• Destructive", "needs your OK"),
        ],
    );

    frame.render_widget(keys, cols[0]);
    frame.render_widget(slash, cols[1]);
    frame.render_widget(actions, cols[2]);

    // Footer hint, centered at the bottom of the card.
    let footer_y = card_area.y + card_area.height.saturating_sub(2);
    let footer_w = card_area.width;
    let footer_area = Rect {
        x: card_area.x,
        y: footer_y,
        width: footer_w,
        height: 1,
    };
    let footer_text = " press any key to close · F1 to toggle anytime ";
    let pad = (footer_w as usize).saturating_sub(footer_text.chars().count()) / 2;
    let footer = Paragraph::new(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            footer_text.to_string(),
            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
        ),
    ]));
    frame.render_widget(footer, footer_area);
}

/// The worlds panel: the live parallel-worlds view the legacy TUI's worlds
/// pane owned, fed from `worlds_registry::global()`. Same renderer
/// (`worlds_view::WorldsRenderer`), new home.
fn worlds_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let registry = crate::worlds_registry::global();
    let total = registry.total();
    if total == 0 {
        return placeholder_lines(
            "worlds",
            "No parallel worlds yet. Try /spawn 2 <prompt>, or the agent's SpawnWorld primitive.",
        );
    }
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(
                "⫸ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} total · {} active", total, registry.active_count()),
                Style::default().fg(TEXT_SOFT),
            ),
        ]),
        Line::from(""),
    ];
    if let Some(r) = state.worlds_renderer.as_ref() {
        lines.extend(r.render_lines());
    }
    lines
}

/// The persistent goal, as it is on disk.
///
/// This panel rendered a placeholder (« Stage 2 reads from goal::GoalManager »)
/// until 2026-08-16: one more dead scaffolding (docs/invariants.md).
/// It now reads `state.goal_summary`, which the frontend fills from
/// `GoalManager::get_active`.
///
/// SECURITY: the source is the goal's file, never a model action. A model
/// that could write this panel would choose what the operator reads about
/// the work they are monitoring — and a stopped goal could display as
/// "active".
fn goal_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(goal) = state.goal_summary.as_ref() else {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no active goal",
                Style::default().fg(TEXT_SOFT),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  `/goal <text>` to set one · it survives the session",
                Style::default().fg(DIM),
            )),
        ];
    };

    let status_color = if goal.status.starts_with("active") {
        ACCENT
    } else if goal.status.starts_with("blocked") || goal.status.starts_with("budget") {
        BAD
    } else {
        WARN
    };
    let steps = match goal.steps_limit {
        Some(limit) => format!("{}/{}", goal.steps_used, limit),
        None => format!("{}", goal.steps_used),
    };

    // No empty row at the top, and no empty row before the history: in a
    // full pane, the last inner row is the only one the eye reaches, and an
    // empty row there would make a full panel look like a finished panel.
    // The overflow sweep measures exactly that
    // (`frame_matrix::panels::an_overflowing_panel_stays_inside_its_frame`).
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(
                "  ⫸ ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                goal.text.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("    status  ", Style::default().fg(DIM)),
            Span::styled(goal.status.clone(), Style::default().fg(status_color)),
        ]),
        Line::from(vec![
            Span::styled("    steps   ", Style::default().fg(DIM)),
            Span::styled(steps, Style::default().fg(TEXT_SOFT)),
        ]),
    ];

    if !goal.history.is_empty() {
        // What is cut says it is cut: the panel shows the last entries, the
        // counter says how many there are.
        lines.push(Line::from(vec![
            Span::styled("    history ", Style::default().fg(DIM)),
            Span::styled(
                format!("{} of {}", goal.history.len(), goal.history_total),
                Style::default().fg(TEXT_SOFT),
            ),
        ]));
        for entry in &goal.history {
            lines.push(Line::from(vec![
                Span::styled("    · ", Style::default().fg(ACCENT_LOW)),
                Span::styled(entry.clone(), Style::default().fg(DIM)),
            ]));
        }
    }
    lines
}

/// The Hooks tab: notification hooks as toggle rows. A hook fires only
/// because a human switched it on here — Space toggles the focused row.
fn hooks_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            " 🔔 desktop notifications (notify-send)",
            Style::default().fg(TEXT_SOFT),
        )),
        Line::from(""),
    ];
    for (i, h) in state.hooks.iter().enumerate() {
        let focused = i == state.hooks_cursor
            && state.side_panel == SidePanel::Hooks
            && state.focus == crate::tui::app::FocusRegion::SidePanel;
        let marker = if focused { "❯" } else { " " };
        let (box_glyph, box_style) = if h.enabled {
            ("[x]", Style::default().fg(ACCENT))
        } else {
            ("[ ]", Style::default().fg(DIM))
        };
        let label_style = if focused {
            Style::default()
                .fg(Color::Rgb(230, 235, 245))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_SOFT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(ACCENT)),
            Span::styled(box_glyph, box_style),
            Span::raw(" "),
            Span::styled(h.label, label_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " these only notify — they can never execute anything",
        Style::default().fg(FAINT),
    )));
    lines
}

/// The Matrix tab: the model-painted canvas at panel size. Same frames the
/// header badge slot plays — `PaintBadge` actions and `<matrix>` blocks in
/// prose — shown large, so the "LLM as GPU" surface has a real screen.
fn matrix_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(frame) = state.badge.as_ref().and_then(|p| p.current_frame()) else {
        return placeholder_lines(
            "matrix",
            "No frame painted yet. The model paints with PaintBadge or a <matrix> block; \
             the latest frame lands here and in the header slot.",
        );
    };
    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.extend(badge::frame_to_lines(frame, Style::default().fg(TEXT)));
    lines
}

/// The Data panel: the last action result through the typed renderers the
/// legacy TUI's data pane owned, with `F7` flipping to raw coloured JSON.
/// The renderers live in `crate::result_render`, shared by both frontends.
fn data_panel_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some((action_type, result)) = state.last_result.as_ref() else {
        return placeholder_lines("data", "No action has completed yet this session.");
    };
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("▤ ", Style::default().fg(ACCENT)),
            Span::styled(
                action_type.clone(),
                Style::default().fg(TEXT_SOFT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if state.data_raw {
                    "  · raw (F7)"
                } else {
                    "  · rendered (F7)"
                },
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
    ];
    if state.data_raw {
        let pretty = serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
        lines.extend(
            pretty
                .lines()
                .map(|l| crate::result_render::colorize_json_line(l).to_owned())
                .map(owned_line),
        );
        return lines;
    }
    match crate::result_render::render_result(action_type, result) {
        Some(rendered) => lines.extend(rendered),
        None => {
            let pretty =
                serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
            lines.extend(
                pretty
                    .lines()
                    .map(|l| crate::result_render::colorize_json_line(l).to_owned())
                    .map(owned_line),
            );
        }
    }
    lines
}

/// `colorize_json_line` borrows its input; the panel needs `'static`.
fn owned_line(line: Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|sp| Span::styled(sp.content.into_owned(), sp.style))
            .collect::<Vec<_>>(),
    )
}

fn draw_slash_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let left = if state.focus_mode {
        " ^B focus · /goal · /queue · /restore · /diff · /commit"
    } else {
        match state.side_panel {
            SidePanel::Conversation => " /goal · /queue · /restore · /diff · /commit",
            SidePanel::Queue => " /queue clear · /queue drop N",
            SidePanel::Tasks => " /task add <title> · /task done <id> · /task clear",
            SidePanel::Cron => " /cron in <secs> <label> [→ <prompt>] · /cron cancel <id>",
            SidePanel::Pipeline => " (read-only · ▶ last Pipeline run breakdown)",
            SidePanel::Sources => " (read-only · 📚 URLs touched this session)",
            SidePanel::Pulse => " (read-only · ❤ live session heartbeat)",
            SidePanel::Snapshots => " /snapshot <name> · /restore · /snapshots",
            SidePanel::Handles => " /handles · /check <id> · /stop <id>",
            SidePanel::Worlds => " /worlds · /spawn N <prompt>",
            SidePanel::Goal => " /goal set <text> · /goal status · /goal clear",
            SidePanel::Data => " (read-only · ▤ last result · F7 raw/rendered)",
            SidePanel::Matrix => " (read-only · ⫸ model-painted canvas · PaintBadge / <matrix>)",
            SidePanel::Shell => {
                " (interactive · type/paste here · Shift+Tab leaves · ^\\ to agent)"
            }
            SidePanel::Hooks => " (interactive · ↑/↓ pick · Space toggles · all off by default)",
            SidePanel::Infra => " (read-only · ☁ burn rate + GPU · stop stays a human Confirm)",
        }
    };
    let any_expanded = state.conversation.iter().any(|cl| cl.expanded);
    let right = if state.shell_mode {
        " ^S exit shell · ↑ recall · Esc back · ^C quit "
    } else if any_expanded {
        " ^E toggle · Esc collapse all · ^C quit "
    } else if state.focus_mode {
        " ^B focus on  ·  ^E expand action  ·  ^Tab panel  ·  ^C quit "
    } else {
        " ^Tab panel · ^B focus · ^E expand · ^S shell · ^C quit · F1 help "
    };
    // During a turn Ctrl+C stops it first.
    let right = if state.agent_busy && !state.shell_mode {
        right.replace("^C quit", "^C stop")
    } else {
        right.to_string()
    };

    let line = Line::from(vec![
        Span::styled(left.to_string(), Style::default().fg(DIM)),
        Span::raw(" ".repeat(
            (area.width as usize).saturating_sub(left.chars().count() + right.chars().count()),
        )),
        Span::styled(
            right.to_string(),
            Style::default().fg(if state.focus_mode { ACCENT_DIM } else { FAINT }),
        ),
    ]);
    let p = Paragraph::new(line);
    frame.render_widget(p, area);
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{AppState, CronItem, PipelineRun, PipelineStep, TaskItem};
    use std::time::Instant;

    // ── assistant frame colour ──────────────────────────────────────────

    /// The frame of a response is teal (`prose::ASSISTANT_FRAME`) and the
    /// prose inside keeps the terminal's default colour. A failure would mean
    /// the frame fell back to charcoal (a reply no longer stands out from
    /// the operator's violet lines) or the text got painted — the owner asked
    /// for the outline, not the words (2026-08-17).
    #[test]
    fn assistant_frame_is_teal_and_the_prose_is_not() {
        use ansi_to_tui::IntoText;
        // `colored` drops every escape when stdout is not a TTY; the TUI
        // forces them on at start-up (`tui/mod.rs`), so does this test.
        crate::tui::app::tests::force_truecolor();
        let fr = crate::prose::ASSISTANT_FRAME;
        let teal = Color::Rgb(fr.0, fr.1, fr.2);
        let rows = crate::tui::app::x0ne_response_box_lines("plain words here", 60);
        for row in &rows {
            let line = row.as_bytes().into_text().unwrap().lines.pop().unwrap();
            for sp in &line.spans {
                let is_frame = sp
                    .content
                    .chars()
                    .all(|c| "╭─╮│╰╯ ".contains(c) || c.is_whitespace())
                    && sp.content.chars().any(|c| "╭─╮│╰╯".contains(c));
                if is_frame || sp.content.trim() == "x0ne" {
                    assert_eq!(sp.style.fg, Some(teal), "frame span {:?}", sp.content);
                }
                if sp.content.contains("plain words") {
                    assert!(
                        matches!(sp.style.fg, None | Some(Color::Reset)),
                        "prose must keep the default colour, got {:?}",
                        sp.style.fg
                    );
                }
            }
        }
    }

    /// The expansion of a clicked action is drawn UNDER its whole block —
    /// after the output box — not between the header and the rest. Rendered
    /// after the header it sat above the output the operator clicked and
    /// read as the previous action's (owner, 2026-08-17 12:34). A failure
    /// would put the dump back above the block's own lines.
    #[test]
    fn an_expanded_action_dumps_below_its_whole_block() {
        use crate::tui::app::{ConversationLine, ToolStatus};
        use crate::tui::frame_matrix::{render_state_to_text, resting_state};
        let mut state = resting_state();
        state.focus_mode = true; // full width for the conversation
        let mut head = ConversationLine::tool("▸ exec · RunCommand".into(), ToolStatus::Ok);
        head.action_head = true;
        head.expanded = true;
        head.action_input = Some(serde_json::json!({"type": "RunCommand", "command": "ls"}));
        head.action_result =
            Some(serde_json::json!({"success": true, "output": {"stdout": "a\nb"}}));
        state.push_line(head);
        state.push_line(ConversationLine::tool("  $ ls".into(), ToolStatus::Ok));
        state.push_line(ConversationLine::tool(
            "  └ ok (2ms)".into(),
            ToolStatus::Ok,
        ));
        state.push_line(ConversationLine::tool(
            "  ┌ output ─ LASTROW".into(),
            ToolStatus::Ok,
        ));
        let text = render_state_to_text(&state, 100, 40);
        let last_block_row = text.find("LASTROW").expect("the block's last row is drawn");
        let dump = text.find("input  ").expect("the expansion is drawn");
        let stdout_block = text.find("output.stdout").expect("the lifted stdout block");
        assert!(
            dump > last_block_row,
            "expansion must come after the block:\n{text}"
        );
        assert!(
            stdout_block > last_block_row,
            "the output text too:\n{text}"
        );
    }

    /// On a 60-column terminal the conversation title row reads
    /// `╭ ◆ anthropic/claude-sonnet-5 $0.03 ╮` under ratatui 0.29. ratatui
    /// 0.30 truncated the model name to `anthropic/claud` unless it is redrawn.
    #[test]
    fn a_narrow_pane_keeps_the_model_name_over_the_counters() {
        use crate::tui::frame_matrix::{render_state_to_rows, resting_state};
        let rows = render_state_to_rows(&resting_state(), 60, 24);
        let title = rows
            .iter()
            .find(|r| r.contains("╭ ◆"))
            .expect("the conversation title row is drawn");
        assert!(
            title.contains("╭ ◆ anthropic/claude-sonnet-5 $0.03 ╮"),
            "title row: {title}"
        );
    }

    /// A long picker scrolls in a window: the modal does not grow past its
    /// cap, the counter says how many rows the filter lets through, and the
    /// rows outside the window are announced. A failure would clip a
    /// 122-session list to whatever fits, silently.
    #[test]
    fn a_long_picker_scrolls_in_a_window_and_says_so() {
        use crate::tui::app::{AskState, PickerAction};
        use crate::tui::frame_matrix::{render_state_to_text, resting_state};
        let mut state = resting_state();
        let rows: Vec<(String, Option<String>)> = (0..40)
            .map(|i| (format!("session-{i:02}"), Some(format!("detail {i}"))))
            .collect();
        let mut ask = AskState::local(
            "Resume which session?",
            None,
            rows,
            PickerAction::ResumeSession,
        );
        ask.cursor = 20;
        state.pending_ask = Some(ask);
        let text = render_state_to_text(&state, 120, 40);
        assert!(text.contains("40/40"), "counter:\n{text}");
        assert!(
            text.contains("above") && text.contains("below"),
            "window announced:\n{text}"
        );
        assert!(
            text.contains("session-20"),
            "the cursor row is in the window:\n{text}"
        );
        assert!(
            !text.contains("session-00") || !text.contains("session-39"),
            "not everything at once"
        );
    }

    /// One empty row separates contexts — user line, action block, answer —
    /// and two actions; the rows of one action stay packed. A failure would
    /// pack the transcript again (owner: « too condensed »), or split an
    /// action's own header from its output.
    #[test]
    fn contexts_are_separated_by_a_blank_row_and_an_action_block_stays_packed() {
        use crate::tui::app::{ConversationLine, LineKind, ToolStatus};
        use crate::tui::frame_matrix::{render_state_to_rows, resting_state};
        let mut state = resting_state();
        state.focus_mode = true;
        state.conversation.clear();
        state.push_line(ConversationLine::new(LineKind::User, "USERLINE".into()));
        let mut head = ConversationLine::tool("HEAD-A".into(), ToolStatus::Ok);
        head.action_head = true;
        state.push_line(head);
        state.push_line(ConversationLine::tool("BODY-A".into(), ToolStatus::Ok));
        let mut head2 = ConversationLine::tool("HEAD-B".into(), ToolStatus::Ok);
        head2.action_head = true;
        state.push_line(head2);
        state.push_line(ConversationLine::new(LineKind::Assistant, "ANSWER".into()));
        let rows = render_state_to_rows(&state, 100, 30);
        let find = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not drawn"))
        };
        let (u, a, b, b2, ans) = (
            find("USERLINE"),
            find("HEAD-A"),
            find("BODY-A"),
            find("HEAD-B"),
            find("ANSWER"),
        );
        assert_eq!(a, u + 2, "blank row between the user line and the action");
        assert_eq!(b, a + 1, "an action's rows stay packed");
        assert_eq!(b2, b + 2, "blank row between two actions");
        assert!(ans >= b2 + 2, "blank row before the answer box");
    }

    // ── action expansion: output as text ────────────────────────────────

    /// A command's `stdout` is lifted out of the envelope as text, with its
    /// newlines, and the envelope points at it; a one-line field stays in the
    /// JSON; a Pipeline's nested step output is found too. A failure would
    /// mean the expanded action shows the output as one escaped JSON string
    /// — « everything but the output » (owner, 2026-08-17).
    #[test]
    fn expansion_lifts_multiline_output_fields_out_of_the_envelope() {
        let mut v = serde_json::json!({
            "output": {"stdout": "one\ntwo\nthree", "stderr": "", "exit_code": 0},
            "success": true,
            "steps": [{"output": {"content": "a\nb"}}]
        });
        let mut blocks = Vec::new();
        lift_text_fields(&mut v, "", &mut blocks);
        assert_eq!(
            blocks,
            vec![
                ("output.stdout".to_string(), "one\ntwo\nthree".to_string()),
                ("steps[0].output.content".to_string(), "a\nb".to_string()),
            ]
        );
        assert_eq!(v["output"]["stderr"], "", "a one-line field stays in place");
        assert!(
            v["output"]["stdout"]
                .as_str()
                .unwrap()
                .contains("shown above"),
            "the envelope points at the block: {}",
            v["output"]["stdout"]
        );
        assert_eq!(v["output"]["exit_code"], 0);
    }

    // ── live_status_active / draw_live_status ───────────────────────
    //
    // The renderer must reflect three distinct phases of a turn so
    // the user can tell at a glance whether X0NE is thinking, writing,
    // or running a tool. We don't poke pixels — we just assert that
    // `live_status_active` correctly enables the row in each state
    // and stays off when nothing is happening.

    #[test]
    fn live_status_is_off_when_idle() {
        let s = AppState::new();
        assert!(!live_status_active(&s), "fresh state should be idle");
    }

    #[test]
    fn live_status_on_while_awaiting_first_token() {
        let mut s = AppState::new();
        s.awaiting_first_token = true;
        s.agent_busy = true;
        s.turn_started_at = Some(Instant::now());
        assert!(
            live_status_active(&s),
            "thinking phase must surface the live status row"
        );
    }

    #[test]
    fn live_status_on_while_streaming_tokens() {
        let mut s = AppState::new();
        s.agent_busy = true;
        s.awaiting_first_token = false;
        s.assistant_buffer.push_str("Hello world");
        s.turn_started_at = Some(Instant::now());
        assert!(
            live_status_active(&s),
            "streaming phase (buffer non-empty) must surface the row"
        );
    }

    #[test]
    fn live_status_on_during_tool_dispatch() {
        let mut s = AppState::new();
        s.last_action_started = Some(Instant::now());
        s.last_action_name = Some("Pipeline".into());
        assert!(live_status_active(&s), "tool dispatch must surface the row");
    }

    // ── Tasks / Cron / Pipeline / Pulse panel renderers ─────────────
    //
    // Cheap smoke tests: the panel functions must not panic on any
    // state shape (empty, populated, mixed completed/open). We
    // assert that the output is non-empty so a regression that
    // returns Vec::new() (and accidentally leaves a blank panel)
    // gets caught.

    #[test]
    fn tasks_panel_empty_shows_hint() {
        let s = AppState::new();
        let lines = tasks_panel_lines(&s);
        assert!(
            !lines.is_empty(),
            "empty-tasks panel must still render a hint"
        );
    }

    #[test]
    fn tasks_panel_with_mixed_open_and_done_renders_both() {
        let mut s = AppState::new();
        s.tasks.push(TaskItem {
            id: 1,
            title: "draft README".into(),
            done: false,
            created_at: Instant::now(),
        });
        s.tasks.push(TaskItem {
            id: 2,
            title: "ship release".into(),
            done: true,
            created_at: Instant::now(),
        });
        let lines = tasks_panel_lines(&s);
        // header + blank + counter + blank + 2 task rows = ≥ 5 lines
        assert!(
            lines.len() >= 5,
            "tasks panel must render at least header + 2 rows, got {}",
            lines.len()
        );
    }

    #[test]
    fn cron_panel_empty_shows_hint() {
        let s = AppState::new();
        let lines = cron_panel_lines(&s);
        assert!(!lines.is_empty());
    }

    #[test]
    fn cron_panel_with_pending_reminder_renders_countdown() {
        let mut s = AppState::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        s.crons.push(CronItem {
            id: 1,
            label: "check CI".into(),
            due_at_unix: now + 90,
            trigger_prompt: None,
            fired: false,
        });
        let lines = cron_panel_lines(&s);
        // At least header + counter + 1 row.
        assert!(lines.len() >= 3);
    }

    #[test]
    fn pipeline_panel_empty_shows_hint() {
        let s = AppState::new();
        let lines = pipeline_panel_lines(&s);
        assert!(
            !lines.is_empty(),
            "empty Pipeline panel must still tell the user what it'll show"
        );
    }

    #[test]
    fn pipeline_panel_renders_step_breakdown() {
        let mut s = AppState::new();
        s.last_pipeline = Some(PipelineRun {
            started_at: Instant::now(),
            completed: true,
            total_steps: 2,
            completed_steps: 1,
            failed: true,
            total_duration_ms: 42,
            steps: vec![
                PipelineStep {
                    idx: 0,
                    action_type: "FileInfo".into(),
                    status: "ok".into(),
                    duration_ms: 9,
                    summary: "1638B".into(),
                },
                PipelineStep {
                    idx: 1,
                    action_type: "SearchCode".into(),
                    status: "failed".into(),
                    duration_ms: 2,
                    summary: "err: no pattern".into(),
                },
            ],
        });
        let lines = pipeline_panel_lines(&s);
        // header + blank + 2 step rows (each with summary subrow) = ≥ 6 lines
        assert!(
            lines.len() >= 6,
            "pipeline panel must render header + per-step rows, got {}",
            lines.len()
        );
    }

    #[test]
    fn pulse_panel_empty_shows_hint() {
        let s = AppState::new();
        let lines = pulse_panel_lines(&s);
        assert!(!lines.is_empty());
    }

    #[test]
    fn fmt_ms_picks_unit_thresholds() {
        // pipeline panel + step rows lean on fmt_ms — make sure the
        // unit chooser is sane across the three thresholds.
        assert_eq!(fmt_ms(0), "0ms");
        assert_eq!(fmt_ms(999), "999ms");
        assert!(fmt_ms(1_500).ends_with("s"));
        assert!(fmt_ms(60_000).contains("m"));
    }

    // ── Long-user-message wrap (bug 20260528_003416) ────────────────
    //
    // Before this fix, a long User line rendered as a SINGLE
    // ratatui Line; the Paragraph's visual wrap then clipped any
    // overflow at the bottom of the conversation slot. The fix is
    // to pre-wrap into multiple Lines so the windowing math sees
    // each wrapped row as its own logical line.

    #[test]
    fn wrap_user_body_emits_multiple_rows_when_text_overflows() {
        let long = "ok I will soon make x0ne the official tool of my next \
                    machine and I want everything ready for that moment — Claude \
                    Code as backup, a smooth MCP gateway, working incus and podman \
                    sandboxes, and a résumé of the whole setup.";
        let rows = wrap_user_body(long, 80);
        assert!(
            rows.len() >= 3,
            "expected at least 3 wrapped rows for a long line at width 80, got {}",
            rows.len()
        );
        // No row should exceed the inner width (panel_width - 6).
        for r in &rows {
            assert!(
                r.chars().count() <= 74,
                "wrapped row exceeds inner width: {} chars in {:?}",
                r.chars().count(),
                r
            );
        }
        // Concatenating the wrapped rows should preserve every word
        // (textwrap may rebalance whitespace but the substantive
        // content is kept).
        let joined: String = rows.join(" ");
        assert!(joined.contains("résumé"));
        assert!(joined.contains("incus"));
    }

    #[test]
    fn wrap_user_body_passes_short_text_through_as_one_row() {
        let short = "hi";
        let rows = wrap_user_body(short, 80);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], "hi");
    }

    #[test]
    fn wrap_user_body_clamps_silly_widths_to_20() {
        // A miniscule panel width must still produce something
        // readable (20-col inner clamp protects against degenerate
        // wraps that fight on every word).
        let rows = wrap_user_body("the quick brown fox jumps over the lazy dog", 4);
        for r in &rows {
            assert!(
                r.chars().count() <= 20,
                "narrow-panel rows must still respect the 20-col clamp, got {} chars",
                r.chars().count()
            );
        }
        assert!(!rows.is_empty());
    }

    #[test]
    fn format_pending_user_line_wrapped_returns_multiple_lines_on_long_text() {
        let mut cl = ConversationLine::new(
            crate::tui::app::LineKind::User,
            "a ".repeat(80), // 160-char single-token-stream
        );
        cl.text = cl.text.trim().to_string();
        let lines = format_pending_user_line_wrapped(&cl, 0, 80);
        assert!(
            lines.len() >= 2,
            "long pending line must produce ≥ 2 ratatui Lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn format_user_line_wrapped_returns_multiple_lines_on_long_text() {
        let cl = ConversationLine::new(
            crate::tui::app::LineKind::User,
            "lorem ipsum dolor sit amet ".repeat(20),
        );
        let lines = format_user_line_wrapped(&cl, 80);
        assert!(
            lines.len() >= 2,
            "long sent user line must wrap to ≥ 2 ratatui Lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn fmt_tokens_handles_ranges() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn queue_badge_empty_is_blank() {
        let s = queue_badge_span(0);
        assert_eq!(s.content, " ");
    }

    #[test]
    fn queue_badge_nonempty_has_count() {
        let s = queue_badge_span(3);
        assert!(s.content.contains("3 queued"));
    }

    #[test]
    fn x0ne_logo_color_is_solid_when_idle() {
        // Whatever phase we pass, idle must hand back the canonical
        // ACCENT shade — no shimmer when nothing is happening.
        assert_eq!(x0ne_logo_color(false, 0), ACCENT);
        assert_eq!(x0ne_logo_color(false, 42), ACCENT);
        assert_eq!(x0ne_logo_color(false, u32::MAX), ACCENT);
    }

    #[test]
    fn format_yymmddhhmm_pretty_prints_or_falls_back_silently() {
        // Standard input → `26 May 13:25`.
        assert_eq!(format_yymmddhhmm("2605261325"), "26 May 13:25");
        // Single-digit day must be trimmed of leading zero.
        assert_eq!(format_yymmddhhmm("2605030914"), "3 May 09:14");
        // Malformed input must NOT panic — it just returns empty so
        // the caller falls back to `v<version>` alone.
        assert_eq!(format_yymmddhhmm(""), "");
        assert_eq!(format_yymmddhhmm("not-digits"), "");
        assert_eq!(format_yymmddhhmm("12345"), ""); // wrong length
        assert_eq!(format_yymmddhhmm("9999991325"), "99 ??? 13:25"); // unknown month → `???`
    }

    #[test]
    fn build_stamp_line_starts_with_version() {
        // The signature is the user-facing contract: it must lead
        // with `v<CARGO_PKG_VERSION>` so users can tell at a glance
        // what binary they're running.
        let s = build_stamp_line();
        let expected_prefix = format!("v{}", env!("CARGO_PKG_VERSION"));
        assert!(
            s.starts_with(&expected_prefix),
            "stamp must start with version; got {:?}",
            s
        );
    }

    #[test]
    fn pending_user_line_sweeps_through_3_violet_shades() {
        // The "mirror reflection" pass goes dim → mid → bright →
        // mid → dim. Each stop lasts 6 ticks; the full cycle is
        // 24 ticks (~1.9 s at 80 ms/tick). Brighter peak than the
        // previous 2-shade version so the user can SEE that their
        // message is being chewed on.
        let cl = ConversationLine::new(LineKind::User, "hello".into());
        let body_color = |l: &Line| l.spans.last().unwrap().style.fg.unwrap();

        assert_eq!(
            body_color(&format_pending_user_line(&cl, 0)),
            VIOLET,
            "phase 0 must be base VIOLET"
        );
        assert_eq!(
            body_color(&format_pending_user_line(&cl, 6)),
            VIOLET_REFLECT_MID,
            "phase 6 must be MID"
        );
        assert_eq!(
            body_color(&format_pending_user_line(&cl, 12)),
            VIOLET_REFLECT_BRIGHT,
            "phase 12 must hit the bright peak"
        );
        assert_eq!(
            body_color(&format_pending_user_line(&cl, 18)),
            VIOLET_REFLECT_MID,
            "phase 18 must be back to MID on the descent"
        );
        // Cycle wraps at 24.
        assert_eq!(
            body_color(&format_pending_user_line(&cl, 24)),
            VIOLET,
            "phase 24 must wrap to base VIOLET"
        );
    }

    #[test]
    fn pending_user_line_chevron_brightens_in_sync_with_text() {
        // The `›` glyph follows the same wave (one step behind the
        // body) so the whole row moves together — the visual cue is
        // not just on the text. Verify the glyph at peak phase is
        // BRIGHTER than at base phase.
        let cl = ConversationLine::new(LineKind::User, "hello".into());
        let glyph_color = |l: &Line| l.spans.first().unwrap().style.fg.unwrap();
        let base = glyph_color(&format_pending_user_line(&cl, 0));
        let peak = glyph_color(&format_pending_user_line(&cl, 12));
        assert_ne!(
            base, peak,
            "chevron should shimmer too (got identical color)"
        );
        assert_eq!(base, VIOLET_DIM, "at base phase glyph stays in deep violet");
        assert_eq!(peak, VIOLET_REFLECT_MID, "at peak the glyph rides the wave");
    }

    #[test]
    fn x0ne_logo_color_is_subtle_2step_shimmer_when_busy() {
        // The single sanctioned shimmer in the frame lives on the
        // logo word: idle → solid ACCENT; busy → gentle breath
        // between ACCENT and ACCENT_SHIMMER over a 24-tick cycle (~2s
        // at 80ms ticks). No other shimmer surface should exist.
        for phase in [0u32, 5, 11] {
            assert_eq!(
                x0ne_logo_color(true, phase),
                ACCENT,
                "busy/phase {} should be ACCENT (first half of cycle)",
                phase
            );
        }
        for phase in [12u32, 17, 23] {
            assert_eq!(
                x0ne_logo_color(true, phase),
                ACCENT_SHIMMER,
                "busy/phase {} should be ACCENT_SHIMMER (second half)",
                phase
            );
        }
        // Wraps back to ACCENT at phase 24.
        assert_eq!(x0ne_logo_color(true, 24), ACCENT);
        // Idle always solid ACCENT regardless of phase.
        for phase in [0u32, 12, 100, u32::MAX] {
            assert_eq!(x0ne_logo_color(false, phase), ACCENT);
        }
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::tui::app::{AppState, ConversationLine, LineKind};
    // Drawing goes through the generalized harness (T-B): these three
    // functions now only build an `AppState`, and the screen comes from one
    // place.
    use crate::tui::frame_matrix;

    /// Render the whole screen into a text buffer.
    ///
    /// Used both as a regression guard on the chrome and to produce a
    /// reviewable snapshot of the layout without needing a real terminal.
    fn render_to_text(width: u16, height: u16) -> String {
        let mut state = frame_matrix::resting_state();
        state.conversation.push(ConversationLine::new(
            LineKind::System,
            "connected · ready when you are.".into(),
        ));
        frame_matrix::render_state_to_text(&state, width, height)
    }

    /// The first screen, before `Ready`, names no model.
    #[test]
    fn the_splash_names_no_model_before_ready() {
        let screen = frame_matrix::render_state_to_text(&AppState::new(), 140, 40);
        assert!(!screen.contains("claude"), "{screen}");
        assert!(screen.contains("◆ connecting…"), "{screen}");
    }

    /// The chrome the legacy TUI was liked for: every pane framed, and the
    /// metadata inset into the frame rather than spending its own row.
    #[test]
    fn panes_are_framed_with_rounded_corners() {
        let screen = render_to_text(120, 30);
        assert!(
            screen.contains('╭') && screen.contains('╰'),
            "rounded corners must be present:\n{screen}"
        );
        assert!(
            screen.contains("conv") && screen.contains('›'),
            "the tab strip must sit in the side panel border, and mark that it \
             scrolls rather than clipping mid-glyph:\n{screen}"
        );
        assert!(
            screen.contains("tk ·"),
            "tokens/cost must appear in the conversation border:\n{screen}"
        );
    }

    // ── The conversation pane's bottom edge ──────────────────────────
    //
    // Owner capture 2026-08-16_17-19.png: with the conversation overflowing
    // and `⠴ x0ne is thinking` lit, the tail of the pane showed stale rows
    // and the last action's box was cut mid-frame. These draw a real screen
    // and read the buffer, because the defect was invisible to every test
    // that only checked the strings a line formatter returns.

    /// A screen with `n` conversation lines, some of them wider than the pane.
    fn screen_with_overflow(width: u16, height: u16, busy: bool) -> Vec<String> {
        let mut state = AppState::new();
        state.model_label = "anthropic/claude-sonnet-5".into();
        state.agent_busy = busy;
        state.awaiting_first_token = busy;
        for i in 0..60 {
            // Every fifth line is a long policy refusal — the shape that
            // triggered the bug, because it wraps to several rows.
            let text = if i % 5 == 0 {
                format!(
                    "line {i:03} no read paths configured: every read is denied · \
                     answer 1/2 at the prompt or set [policy] allowed_read_paths"
                )
            } else {
                format!("line {i:03} short")
            };
            state
                .conversation
                .push(ConversationLine::new(LineKind::System, text));
        }
        frame_matrix::render_state_to_rows(&state, width, height)
    }

    /// The newest line is on screen when the conversation is pinned to the
    /// bottom, however much of it wraps.
    ///
    /// A failure means the window is being sliced in logical lines while the
    /// pane is painted in visual rows: ratatui clips the excess from the
    /// bottom, so the rows the user is actually waiting for — the ones the
    /// agent just produced — are the ones dropped. That is what the owner
    /// photographed, and it reads as a frozen TUI.
    #[test]
    fn the_newest_conversation_line_is_visible_when_lines_wrap() {
        for busy in [true, false] {
            let screen = screen_with_overflow(80, 24, busy).join("\n");
            assert!(
                screen.contains("line 059"),
                "the last line must be on screen (busy={busy}):\n{screen}"
            );
        }
    }

    /// The pane keeps its bottom border, live-status row or not.
    ///
    /// The frame is what tells the eye where the conversation stops. A
    /// failure means content is being painted over the border row — the
    /// conversation would bleed straight into the status line below it.
    #[test]
    fn the_conversation_frame_keeps_its_bottom_border_when_content_overflows() {
        for (busy, status_height) in [(true, 1u16), (false, 0)] {
            let rows = screen_with_overflow(80, 24, busy);
            // Header 4 + body; the body's last row is the frame's bottom
            // border. Input is 3 rows, slash bar 1.
            let bottom = rows.len() - 1 - 1 - 3 - status_height as usize;
            let row = &rows[bottom];
            assert!(
                row.starts_with('╰'),
                "no bottom-left corner (busy={busy}) in {row:?}"
            );
            let frame_end = row
                .find('╯')
                .unwrap_or_else(|| panic!("no bottom-right corner (busy={busy}) in {row:?}"));
            assert!(
                row[..frame_end].chars().skip(1).all(|c| c == '─'),
                "the bottom border is broken (busy={busy}): {row:?}"
            );
        }
    }

    /// The side panel is never clipped, at any terminal width.
    ///
    /// `Percentage(64) + Length(1) + Percentage(36)` asks for one column more
    /// than exists on odd widths. A failure means the rightmost frame loses
    /// its border and the panel reads as cut off.
    #[test]
    fn the_side_panel_keeps_its_right_border_at_every_width() {
        for width in 60u16..=200 {
            let rows = screen_with_overflow(width, 24, true);
            let body = &rows[10];
            assert!(
                body.ends_with('│'),
                "side panel clipped at width {width}: {body:?}"
            );
        }
    }

    /// Wrapping keeps the colour of the line it came from.
    ///
    /// A refusal that turns grey halfway down stops reading as a refusal.
    #[test]
    fn a_wrapped_line_keeps_its_style_on_every_row() {
        let style = Style::default().fg(Color::Red);
        let line = Line::from(Span::styled("aaaa bbbb cccc dddd".to_string(), style));
        let (rows, owners) = wrap_to_rows(vec![line], vec![7], 10);
        assert!(rows.len() > 1, "the line must wrap: {rows:?}");
        assert_eq!(owners, vec![7; rows.len()], "each row keeps its owner");
        for row in &rows {
            for span in &row.spans {
                assert_eq!(span.style, style, "a row lost the colour: {row:?}");
            }
        }
    }

    /// A word longer than the pane is cut rather than dropped.
    #[test]
    fn a_word_wider_than_the_pane_is_cut_not_lost() {
        let line = Line::from("abcdefghijklmnop".to_string());
        let (rows, _) = wrap_to_rows(vec![line], vec![0], 5);
        let joined: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(joined, "abcdefghijklmnop", "characters were lost: {rows:?}");
        assert!(rows.len() == 4, "expected 4 rows of 5, got {}", rows.len());
    }

    #[test]
    fn print_layout_snapshot() {
        println!("\n{}", render_to_text(120, 30));
    }

    // ── Header GPU/cost gauge ────────────────────────────────────────
    //
    // The strip beside the ram gauge is a LIVE surface — the kind that
    // shipped two border bugs unnoticed because nothing rendered it in a
    // test. These draw a real frame through TestBackend and read the
    // resulting screen.

    /// Plan mode is on screen twice: a badge in the header, and the input
    /// box's title, where the user types.
    #[test]
    fn plan_mode_shows_in_the_header_and_the_input() {
        let mut state = AppState::new();
        let before = frame_matrix::render_state_to_rows(&state, 100, 24).join("\n");
        assert!(!before.contains("PLAN") && !before.contains("plan ❯"));
        state.plan_mode = true;
        let screen = frame_matrix::render_state_to_rows(&state, 100, 24).join("\n");
        assert!(screen.contains("PLAN"), "{screen}");
        assert!(screen.contains("plan ❯"), "{screen}");
    }

    /// Draw a full frame with the given cloud state and return the screen.
    fn render_with_cloud(cloud: crate::cloudmon::CloudState, width: u16) -> String {
        use std::sync::{Arc, RwLock};
        let mut state = AppState::new();
        state.model_label = "anthropic/claude-sonnet-5".into();
        state.cloud_state = Some(Arc::new(RwLock::new(cloud)));
        // Lines NOT trimmed, joined: the exact form these three tests read
        // before the harness was generalized.
        frame_matrix::render_state_to_rows(&state, width, 20).join("\n")
    }

    fn gauge_gpu(util: u32, temp: u32) -> x0ne_cloud::gpu::GpuState {
        x0ne_cloud::gpu::GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: util,
            mem_used_mb: 40000,
            mem_total_mb: 81559,
            temp_c: temp,
            power_w: 402,
            processes: vec![],
        }
    }

    #[test]
    fn header_gauge_shows_gpu_and_cost_when_measured() {
        use x0ne_cloud::burn::BurnRate;
        let screen = render_with_cloud(
            crate::cloudmon::CloudState {
                burn: Some(BurnRate {
                    hourly_usd: 11.26,
                    per_day_usd: 270.0,
                    priced: vec![],
                    unpriced: vec![],
                }),
                gpus: vec![gauge_gpu(87, 71)],
                ..Default::default()
            },
            140,
        );
        assert!(screen.contains("gpu"), "gpu gauge missing:\n{screen}");
        assert!(screen.contains("87%"), "utilization missing:\n{screen}");
        assert!(screen.contains("71°C"), "temperature missing:\n{screen}");
        assert!(screen.contains("11.26$/h"), "cost missing:\n{screen}");
    }

    #[test]
    fn header_gauge_is_absent_when_nothing_is_measured() {
        // No GPU and no cost → the strip must not render at all. A zero here
        // would read as "costs nothing", which is the lie the gauge exists
        // to prevent.
        let screen = render_with_cloud(crate::cloudmon::CloudState::default(), 140);
        assert!(!screen.contains("$/h"), "phantom cost on screen:\n{screen}");
        assert!(!screen.contains("gpu "), "phantom gpu gauge:\n{screen}");
    }

    #[test]
    fn header_gauge_shows_cost_alone_when_there_is_no_local_gpu() {
        // The confidential-VM shape: cost is known from the inventory, but
        // the GPU lives on the remote node so nvidia-smi says nothing here.
        use x0ne_cloud::burn::BurnRate;
        let screen = render_with_cloud(
            crate::cloudmon::CloudState {
                burn: Some(BurnRate {
                    hourly_usd: 4.21,
                    per_day_usd: 101.0,
                    priced: vec![],
                    unpriced: vec![],
                }),
                gpus: vec![],
                ..Default::default()
            },
            140,
        );
        assert!(
            screen.contains("4.21$/h"),
            "cost-only strip missing:\n{screen}"
        );
    }

    // ── Infra panel ──────────────────────────────────────────────────
    //
    // Join a Line's spans into its visible text so we can assert content
    // without poking pixels.
    fn line_text(l: &Line<'static>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn infra_panel_says_disabled_when_no_state() {
        // cloud_state None (monitoring off) must render an explanation, not a
        // blank or a fake zero.
        let s = AppState::new();
        let lines = infra_panel_lines(&s);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("disabled"), "got: {text}");
    }

    #[test]
    fn infra_panel_shows_burn_gpu_and_estimated_billing() {
        use crate::cloudmon::CloudState;
        use std::sync::{Arc, RwLock};
        use x0ne_cloud::burn::{BurnRate, PricedResource};
        use x0ne_cloud::gpu::GpuState;
        use x0ne_cloud::inventory::{CloudResource, ResourceKind};

        let mut s = AppState::new();
        let cloud = CloudState {
            burn: Some(BurnRate {
                hourly_usd: 11.26,
                per_day_usd: 270.0,
                priced: vec![PricedResource {
                    resource: CloudResource {
                        name: "gpu-h100".into(),
                        kind: ResourceKind::Instance,
                        machine_type: Some("a3-highgpu-1g".into()),
                        zone: "us-central1-a".into(),
                        status: "RUNNING".into(),
                        accelerators: vec![],
                        created_iso: "t".into(),
                    },
                    hourly_usd: 11.26,
                }],
                unpriced: vec![CloudResource {
                    name: "mystery".into(),
                    kind: ResourceKind::Instance,
                    machine_type: Some("z9-unknown".into()),
                    zone: "z".into(),
                    status: "RUNNING".into(),
                    accelerators: vec![],
                    created_iso: "t".into(),
                }],
            }),
            gpus: vec![GpuState {
                index: 0,
                name: "H100".into(),
                util_pct: 87,
                mem_used_mb: 40000,
                mem_total_mb: 81559,
                temp_c: 71,
                power_w: 402,
                processes: vec![],
            }],
            billed_yesterday: None,
            alerts: vec![],
            pricing_as_of: "2026-08-10".into(),
        };
        s.cloud_state = Some(Arc::new(RwLock::new(cloud)));

        let lines = infra_panel_lines(&s);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("11.26$/h"), "burn missing: {text}");
        assert!(text.contains("gpu-h100"), "priced resource missing: {text}");
        assert!(
            text.contains("price unknown"),
            "unpriced heading missing: {text}"
        );
        assert!(
            text.contains("mystery"),
            "unpriced resource missing: {text}"
        );
        assert!(text.contains("gpu0"), "gpu row missing: {text}");
        assert!(
            text.contains("estimated only"),
            "billing honesty missing: {text}"
        );
        assert!(text.contains("2026-08-10"), "price date missing: {text}");
    }

    #[test]
    fn infra_panel_surfaces_a_waste_alert() {
        use crate::cloudmon::CloudState;
        use std::sync::{Arc, RwLock};

        let mut s = AppState::new();
        let cloud = CloudState {
            alerts: vec!["GPU 0 idle 0% with no process for 34min at $11.26/h".into()],
            ..Default::default()
        };
        s.cloud_state = Some(Arc::new(RwLock::new(cloud)));
        let lines = infra_panel_lines(&s);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("⚠"), "alert glyph missing: {text}");
        assert!(text.contains("34min"), "alert text missing: {text}");
    }
}
