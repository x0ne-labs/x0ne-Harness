//! Parallel-worlds split-pane renderer.
//!
//! Renders N "parallel worlds" (sub-agents launched concurrently via
//! `SpawnWorld`) as vertical columns in the CLI, each column scrolling its
//! own primitive history in real time. Final status is colored:
//!
//!   - green  ✓  Completed
//!   - yellow ⟲  RolledBack (Try restored a snapshot)
//!   - red    ✗  Failed (an action failed, no rollback ran)
//!   - red    ☠  Killed  (timeout, kill signal, transport death)
//!
//! ## Architecture
//!
//! - The orchestrator (or a demo binary) owns a `mpsc::UnboundedSender<WorldEvent>`
//!   and clones it into every spawned sub-agent.
//! - Each sub-agent emits `ActionStarted` / `ActionFinished` / `ActionFailed`
//!   events for every primitive it runs through the bridge, plus a terminal
//!   `WorldCompleted` / `WorldRolledBack` / `WorldFailed` / `WorldKilled`.
//! - The renderer task consumes the receiver, mutates a `WorldsRenderer`, and
//!   redraws the frame in place at ~10 Hz using crossterm cursor moves.
//!
//! The pure-data layer (`WorldsRenderer::apply` + `render_plain`) is fully
//! testable without a TTY; the `run_renderer` async loop is the only piece
//! that touches the terminal directly.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::{
    cursor, execute, queue,
    style::Print,
    terminal::{Clear, ClearType},
};
use tokio::sync::mpsc;

// ── Types ──────────────────────────────────────────────────────────────────────

/// Terminal status of a parallel world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldStatus {
    Pending,
    Running,
    Completed,
    RolledBack,
    Failed,
    Killed,
}

impl WorldStatus {
    /// True once the world reached a terminal state (rendering can stop).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorldStatus::Completed
                | WorldStatus::RolledBack
                | WorldStatus::Failed
                | WorldStatus::Killed
        )
    }

    /// Single-glyph status icon (no ANSI color — `render_to_string` adds it).
    pub fn icon(self) -> &'static str {
        match self {
            WorldStatus::Pending => "·",
            WorldStatus::Running => "▶",
            WorldStatus::Completed => "✓",
            WorldStatus::RolledBack => "⟲",
            WorldStatus::Failed => "✗",
            WorldStatus::Killed => "☠",
        }
    }
}

/// Per-action status (kept in `WorldState::history`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimStatus {
    Ok,
    Failed,
}

/// One finished primitive, kept in the per-world history strip.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub action_type: String,
    pub status: PrimStatus,
    pub duration_ms: u64,
}

/// Live state of one parallel world.
#[derive(Debug, Clone)]
pub struct WorldState {
    pub id: String,
    pub status: WorldStatus,
    /// Action currently in flight (drawn at the top of the column).
    pub current_action: Option<String>,
    /// Recently-finished primitives (most recent first), capped.
    pub history: VecDeque<HistoryEntry>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub last_error: Option<String>,
    pub primitives_total: usize,
    pub primitives_failed: usize,
    /// Optional one-line summary added on terminal events.
    pub summary: Option<String>,
}

impl WorldState {
    fn new(id: String) -> Self {
        Self {
            id,
            status: WorldStatus::Pending,
            current_action: None,
            history: VecDeque::new(),
            started_at: Instant::now(),
            finished_at: None,
            last_error: None,
            primitives_total: 0,
            primitives_failed: 0,
            summary: None,
        }
    }

    /// Elapsed time since spawn (or until terminal event).
    pub fn elapsed(&self) -> Duration {
        self.finished_at
            .unwrap_or_else(Instant::now)
            .duration_since(self.started_at)
    }
}

/// Events streamed from spawned sub-agents to the renderer.
#[derive(Debug, Clone)]
pub enum WorldEvent {
    /// A new primitive started for a given world.
    ActionStarted {
        world_id: String,
        action_type: String,
    },
    /// A primitive finished successfully (its row enters the history).
    ActionFinished {
        world_id: String,
        action_type: String,
        duration_ms: u64,
    },
    /// A primitive failed but the world is not necessarily terminal yet
    /// (e.g. inside a `Try` block before rollback).
    ActionFailed {
        world_id: String,
        action_type: String,
        error: String,
        duration_ms: u64,
    },
    /// Terminal: world succeeded.
    WorldCompleted {
        world_id: String,
        summary: Option<String>,
    },
    /// Terminal: a `Try` block rolled back to a snapshot.
    WorldRolledBack {
        world_id: String,
        snapshot: Option<String>,
    },
    /// Terminal: an action failed and was NOT rolled back.
    WorldFailed { world_id: String, error: String },
    /// Terminal: timeout / kill signal / unrecoverable bridge death.
    WorldKilled { world_id: String, reason: String },
}

impl WorldEvent {
    pub fn world_id(&self) -> &str {
        match self {
            WorldEvent::ActionStarted { world_id, .. }
            | WorldEvent::ActionFinished { world_id, .. }
            | WorldEvent::ActionFailed { world_id, .. }
            | WorldEvent::WorldCompleted { world_id, .. }
            | WorldEvent::WorldRolledBack { world_id, .. }
            | WorldEvent::WorldFailed { world_id, .. }
            | WorldEvent::WorldKilled { world_id, .. } => world_id,
        }
    }
}

// ── Renderer ───────────────────────────────────────────────────────────────────

/// Maximum history rows shown per column.
const HISTORY_ROWS: usize = 8;

/// Spinner frames.
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Pure-data renderer: receives `WorldEvent`s, owns `WorldState`s, produces frames.
///
/// The order of columns is the order of `world_ids` passed to `new`. We use
/// a `BTreeMap` for O(log n) lookups but also keep an `order` Vec so the
/// columns stay where the user spawned them.
pub struct WorldsRenderer {
    worlds: BTreeMap<String, WorldState>,
    order: Vec<String>,
    term_width: usize,
    /// Frame counter, used to advance the spinner.
    tick: usize,
}

impl WorldsRenderer {
    pub fn new(world_ids: Vec<String>, term_width: usize) -> Self {
        let mut worlds = BTreeMap::new();
        for id in &world_ids {
            worlds.insert(id.clone(), WorldState::new(id.clone()));
        }
        Self {
            worlds,
            order: world_ids,
            term_width: term_width.max(40),
            tick: 0,
        }
    }

    pub fn worlds(&self) -> &BTreeMap<String, WorldState> {
        &self.worlds
    }
    pub fn order(&self) -> &[String] {
        &self.order
    }
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// All worlds reached a terminal state ⇒ the renderer can exit.
    pub fn all_terminal(&self) -> bool {
        !self.worlds.is_empty() && self.worlds.values().all(|w| w.status.is_terminal())
    }

    /// Tally of terminal statuses: (completed, rolled_back, failed, killed).
    pub fn tally(&self) -> (usize, usize, usize, usize) {
        let mut t = (0, 0, 0, 0);
        for w in self.worlds.values() {
            match w.status {
                WorldStatus::Completed => t.0 += 1,
                WorldStatus::RolledBack => t.1 += 1,
                WorldStatus::Failed => t.2 += 1,
                WorldStatus::Killed => t.3 += 1,
                _ => {}
            }
        }
        t
    }

    /// Apply an event. Unknown world ids are silently ignored — the
    /// orchestrator may emit events before the renderer has the id list.
    pub fn apply(&mut self, ev: WorldEvent) {
        let id = ev.world_id().to_string();
        let Some(w) = self.worlds.get_mut(&id) else {
            return;
        };

        match ev {
            WorldEvent::ActionStarted { action_type, .. } => {
                if w.status == WorldStatus::Pending {
                    w.status = WorldStatus::Running;
                    w.started_at = Instant::now();
                }
                w.current_action = Some(action_type);
            }
            WorldEvent::ActionFinished {
                action_type,
                duration_ms,
                ..
            } => {
                w.primitives_total += 1;
                w.current_action = None;
                w.history.push_front(HistoryEntry {
                    action_type,
                    status: PrimStatus::Ok,
                    duration_ms,
                });
                while w.history.len() > HISTORY_ROWS {
                    w.history.pop_back();
                }
            }
            WorldEvent::ActionFailed {
                action_type,
                error,
                duration_ms,
                ..
            } => {
                w.primitives_total += 1;
                w.primitives_failed += 1;
                w.current_action = None;
                w.last_error = Some(error);
                w.history.push_front(HistoryEntry {
                    action_type,
                    status: PrimStatus::Failed,
                    duration_ms,
                });
                while w.history.len() > HISTORY_ROWS {
                    w.history.pop_back();
                }
            }
            WorldEvent::WorldCompleted { summary, .. } => {
                w.status = WorldStatus::Completed;
                w.finished_at = Some(Instant::now());
                w.current_action = None;
                w.summary = summary;
            }
            WorldEvent::WorldRolledBack { snapshot, .. } => {
                w.status = WorldStatus::RolledBack;
                w.finished_at = Some(Instant::now());
                w.current_action = None;
                w.summary = snapshot.map(|s| format!("snapshot: {}", s));
            }
            WorldEvent::WorldFailed { error, .. } => {
                w.status = WorldStatus::Failed;
                w.finished_at = Some(Instant::now());
                w.current_action = None;
                w.last_error = Some(error);
            }
            WorldEvent::WorldKilled { reason, .. } => {
                w.status = WorldStatus::Killed;
                w.finished_at = Some(Instant::now());
                w.current_action = None;
                w.last_error = Some(reason);
            }
        }
    }

    /// Number of lines the rendered frame will occupy (used to MoveUp on next tick).
    pub fn frame_height(&self) -> usize {
        // header (1) + top border (1) + body (HISTORY_ROWS + 1 for current_action) + bottom border (1) + tally (1)
        1 + 1 + (HISTORY_ROWS + 1) + 1 + 1
    }

    /// Render the frame as a multi-line string. When `with_color` is true,
    /// embeds ANSI escape codes for colors; otherwise produces a plain
    /// snapshot (used by tests).
    pub fn render(&self, with_color: bool) -> String {
        let n = self.order.len().max(1);
        // Each column body width — leave 1 char per separator.
        // Layout: │<col>│<col>│...│<col>│  ⇒  inner_width = (term_width - n - 1) / n
        let usable = self.term_width.saturating_sub(n + 1);
        let col_w = (usable / n).max(10);
        let total_w = col_w * n + n + 1;

        let mut out = String::new();
        let spinner = SPINNER[self.tick % SPINNER.len()];

        // ── Header line ────────────────────────────────────────────────────
        let header = format!("── parallel worlds ({}) ──", n);
        out.push_str(&center(&header, total_w));
        out.push('\n');

        // ── Top border with world ids ──────────────────────────────────────
        out.push('┌');
        for (i, id) in self.order.iter().enumerate() {
            let label = format!(" {} ", truncate(id, col_w.saturating_sub(2)));
            out.push_str(&pad_with('─', &label, col_w));
            out.push(if i + 1 == n { '┐' } else { '┬' });
        }
        out.push('\n');

        // ── Body rows ───────────────────────────────────────────────────────
        // Row 0: current_action with spinner (or empty)
        out.push('│');
        for id in &self.order {
            let w = self.worlds.get(id).unwrap();
            let cell = match (&w.current_action, w.status) {
                (Some(a), WorldStatus::Running) => {
                    let line = format!(" {} {}", spinner, truncate(a, col_w.saturating_sub(4)));
                    pad_to(&line, col_w)
                }
                _ => pad_to("", col_w),
            };
            out.push_str(&colorize_cell(
                &cell,
                color_for_status(w.status),
                with_color,
            ));
            out.push('│');
        }
        out.push('\n');

        // Rows 1..=HISTORY_ROWS: history entries (most recent on top)
        for row in 0..HISTORY_ROWS {
            out.push('│');
            for id in &self.order {
                let w = self.worlds.get(id).unwrap();
                let cell = match w.history.get(row) {
                    Some(h) => {
                        let icon = match h.status {
                            PrimStatus::Ok => '✓',
                            PrimStatus::Failed => '✗',
                        };
                        let dur = format_dur(h.duration_ms);
                        let max_a = col_w.saturating_sub(dur.len() + 4);
                        let a = truncate(&h.action_type, max_a);
                        let line = format!(" {} {} {}", icon, a, dur);
                        pad_to(&line, col_w)
                    }
                    None => pad_to("", col_w),
                };
                let color = match w.history.get(row).map(|h| h.status) {
                    Some(PrimStatus::Failed) => Some(Color::Red),
                    Some(PrimStatus::Ok) => Some(Color::Dim),
                    None => None,
                };
                out.push_str(&colorize_cell(&cell, color, with_color));
                out.push('│');
            }
            out.push('\n');
        }

        // ── Bottom border with status tag ──────────────────────────────────
        out.push('└');
        for (i, id) in self.order.iter().enumerate() {
            let w = self.worlds.get(id).unwrap();
            let tag = status_tag(w, spinner);
            let cell = pad_with('─', &format!(" {} ", tag), col_w);
            out.push_str(&colorize_cell(
                &cell,
                color_for_status(w.status),
                with_color,
            ));
            out.push(if i + 1 == n { '┘' } else { '┴' });
        }
        out.push('\n');

        // ── Tally ──────────────────────────────────────────────────────────
        let (ok, rb, fl, kl) = self.tally();
        let pending = self
            .worlds
            .values()
            .filter(|w| !w.status.is_terminal())
            .count();
        let tally = if pending > 0 {
            format!(
                "✓ {}  ⟲ {}  ✗ {}  ☠ {}    ⠋ {} running",
                ok, rb, fl, kl, pending
            )
        } else {
            format!("✓ {}  ⟲ {}  ✗ {}  ☠ {}    done", ok, rb, fl, kl)
        };
        out.push_str(&center(&tally, total_w));
        out.push('\n');

        out
    }
}

// ── Ratatui renderer ──────────────────────────────────────────────────────────
//
// The TUI `Worlds` pane renders the same data model as the CLI renderer, but
// as `ratatui::text::Line`s so it can sit inside a frame alongside the other
// panes (data view, pipeline…). We reuse **exactly** the same layout math
// (column widths, history rows, status tags) as `render()` so the two
// surfaces always stay visually consistent.
impl WorldsRenderer {
    /// Render the current worlds state as ratatui `Line`s, ready to drop
    /// into a `Paragraph`. One `Line` per terminal row of `frame_height()`.
    /// Zero dependencies on a live TTY — fully testable.
    pub fn render_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::style::Color as RColor;
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};

        fn rcolor(c: Color) -> RColor {
            match c {
                Color::Green => RColor::Green,
                Color::Yellow => RColor::Yellow,
                Color::Red => RColor::Red,
                Color::Cyan => RColor::Cyan,
                Color::Dim => RColor::Rgb(90, 95, 110),
            }
        }
        fn styled(text: String, color: Option<Color>) -> Span<'static> {
            match color {
                Some(c) => Span::styled(text, Style::default().fg(rcolor(c))),
                None => Span::raw(text),
            }
        }

        let n = self.order.len().max(1);
        let usable = self.term_width.saturating_sub(n + 1);
        let col_w = (usable / n).max(10);
        let total_w = col_w * n + n + 1;

        let spinner = SPINNER[self.tick % SPINNER.len()];
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.frame_height());

        // ── Centered header ────────────────────────────────────────────────
        let header = format!("⫸ parallel worlds ({})  ⫸", n);
        lines.push(Line::from(Span::styled(
            center(&header, total_w),
            Style::default()
                .fg(RColor::Cyan)
                .add_modifier(Modifier::BOLD),
        )));

        // ── Top border with world ids ──────────────────────────────────────
        let mut top: Vec<Span<'static>> = vec![Span::raw("┌".to_string())];
        for (i, id) in self.order.iter().enumerate() {
            let label = format!(" {} ", truncate(id, col_w.saturating_sub(2)));
            top.push(Span::styled(
                pad_with('─', &label, col_w),
                Style::default().fg(RColor::Rgb(80, 90, 110)),
            ));
            top.push(Span::raw(if i + 1 == n {
                "┐".to_string()
            } else {
                "┬".to_string()
            }));
        }
        lines.push(Line::from(top));

        // ── Current action row (with spinner) ──────────────────────────────
        let mut cur: Vec<Span<'static>> = vec![Span::raw("│".to_string())];
        for id in &self.order {
            let w = self.worlds.get(id).unwrap();
            let cell = match (&w.current_action, w.status) {
                (Some(a), WorldStatus::Running) => {
                    let line = format!(" {} {}", spinner, truncate(a, col_w.saturating_sub(4)));
                    pad_to(&line, col_w)
                }
                _ => pad_to("", col_w),
            };
            cur.push(styled(cell, color_for_status(w.status)));
            cur.push(Span::raw("│".to_string()));
        }
        lines.push(Line::from(cur));

        // ── History rows ───────────────────────────────────────────────────
        for row in 0..HISTORY_ROWS {
            let mut cells: Vec<Span<'static>> = vec![Span::raw("│".to_string())];
            for id in &self.order {
                let w = self.worlds.get(id).unwrap();
                let (cell, color) = match w.history.get(row) {
                    Some(h) => {
                        let icon = match h.status {
                            PrimStatus::Ok => '✓',
                            PrimStatus::Failed => '✗',
                        };
                        let dur = format_dur(h.duration_ms);
                        let max_a = col_w.saturating_sub(dur.len() + 4);
                        let a = truncate(&h.action_type, max_a);
                        let line = format!(" {} {} {}", icon, a, dur);
                        let c = match h.status {
                            PrimStatus::Ok => Some(Color::Dim),
                            PrimStatus::Failed => Some(Color::Red),
                        };
                        (pad_to(&line, col_w), c)
                    }
                    None => (pad_to("", col_w), None),
                };
                cells.push(styled(cell, color));
                cells.push(Span::raw("│".to_string()));
            }
            lines.push(Line::from(cells));
        }

        // ── Bottom border with status tags ─────────────────────────────────
        let mut bot: Vec<Span<'static>> = vec![Span::raw("└".to_string())];
        for (i, id) in self.order.iter().enumerate() {
            let w = self.worlds.get(id).unwrap();
            let tag = status_tag(w, spinner);
            let cell = pad_with('─', &format!(" {} ", tag), col_w);
            bot.push(styled(cell, color_for_status(w.status)));
            bot.push(Span::raw(if i + 1 == n {
                "┘".to_string()
            } else {
                "┴".to_string()
            }));
        }
        lines.push(Line::from(bot));

        // ── Tally ──────────────────────────────────────────────────────────
        let (ok, rb, fl, kl) = self.tally();
        let pending = self
            .worlds
            .values()
            .filter(|w| !w.status.is_terminal())
            .count();
        let tally = if pending > 0 {
            format!(
                "✓ {}  ⟲ {}  ✗ {}  ☠ {}    {} {} running",
                ok, rb, fl, kl, spinner, pending
            )
        } else {
            format!("✓ {}  ⟲ {}  ✗ {}  ☠ {}    done", ok, rb, fl, kl)
        };
        lines.push(Line::from(Span::styled(
            center(&tally, total_w),
            Style::default().fg(RColor::Rgb(120, 130, 150)),
        )));

        lines
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Color {
    Green,
    Yellow,
    Red,
    Cyan,
    Dim,
}

fn color_for_status(s: WorldStatus) -> Option<Color> {
    match s {
        WorldStatus::Completed => Some(Color::Green),
        WorldStatus::RolledBack => Some(Color::Yellow),
        WorldStatus::Failed | WorldStatus::Killed => Some(Color::Red),
        WorldStatus::Running => Some(Color::Cyan),
        WorldStatus::Pending => Some(Color::Dim),
    }
}

fn ansi(c: Color) -> &'static str {
    match c {
        Color::Green => "\x1b[32m",
        Color::Yellow => "\x1b[33m",
        Color::Red => "\x1b[31m",
        Color::Cyan => "\x1b[36m",
        Color::Dim => "\x1b[2m",
    }
}
const RESET: &str = "\x1b[0m";

fn colorize_cell(s: &str, color: Option<Color>, with_color: bool) -> String {
    if !with_color {
        return s.to_string();
    }
    match color {
        Some(c) => format!("{}{}{}", ansi(c), s, RESET),
        None => s.to_string(),
    }
}

fn status_tag(w: &WorldState, spinner: char) -> String {
    // Compact tag designed to fit inside ~16 chars (so it doesn't get
    // truncated when 4 columns share an 80-char terminal).
    let elapsed = w.elapsed();
    let secs = elapsed.as_millis() as f64 / 1000.0;
    match w.status {
        WorldStatus::Pending => "· pending".to_string(),
        WorldStatus::Running => format!("{} run {:.1}s", spinner, secs),
        WorldStatus::Completed => format!("✓ ok {:.1}s", secs),
        WorldStatus::RolledBack => format!("⟲ rollback {:.1}s", secs),
        WorldStatus::Failed => format!("✗ fail {:.1}s", secs),
        WorldStatus::Killed => format!("☠ killed {:.1}s", secs),
    }
}

/// Truncate a string to at most `max` *chars* (not bytes).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pad/truncate `s` to exactly `width` chars (right-pad with spaces).
fn pad_to(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        truncate(s, width)
    } else {
        let mut out = s.to_string();
        for _ in 0..(width - count) {
            out.push(' ');
        }
        out
    }
}

/// Pad `s` with the given fill char on both sides up to `width` (best-effort centered).
fn pad_with(fill: char, s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        return truncate(s, width);
    }
    let total_pad = width - count;
    let left = total_pad / 2;
    let right = total_pad - left;
    let mut out = String::new();
    for _ in 0..left {
        out.push(fill);
    }
    out.push_str(s);
    for _ in 0..right {
        out.push(fill);
    }
    out
}

fn center(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        return s.to_string();
    }
    let left = (width - count) / 2;
    let mut out = String::new();
    for _ in 0..left {
        out.push(' ');
    }
    out.push_str(s);
    out
}

fn format_dur(ms: u64) -> String {
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

// ── Async runner ───────────────────────────────────────────────────────────────

/// Run the split-pane renderer until every world reaches a terminal state
/// or the event channel is closed. Frame is redrawn at ~10 Hz.
///
/// The frame is anchored at the *current* cursor position when `run_renderer`
/// is called. On exit, the cursor is left right below the frame so subsequent
/// `println!` from the orchestrator prints in the natural place.
pub async fn run_renderer(
    world_ids: Vec<String>,
    mut rx: mpsc::UnboundedReceiver<WorldEvent>,
) -> io::Result<()> {
    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(120);
    let mut renderer = WorldsRenderer::new(world_ids, term_width);
    let frame_height = renderer.frame_height();

    let mut stdout = io::stdout();

    // Reserve N lines below the cursor so MoveUp(N) is well-defined on the
    // next tick. We just print N empty lines and then move back up.
    for _ in 0..frame_height {
        writeln!(stdout)?;
    }
    queue!(stdout, cursor::MoveUp(frame_height as u16), cursor::Hide)?;
    stdout.flush()?;

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.tick().await; // first tick is immediate

    let mut first_frame = true;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(ev) => renderer.apply(ev),
                    None => break, // channel closed by sender(s)
                }
            }
            _ = ticker.tick() => {
                renderer.tick();
            }
        }

        // Redraw
        if !first_frame {
            queue!(stdout, cursor::MoveUp(frame_height as u16))?;
        }
        first_frame = false;
        for _ in 0..frame_height {
            queue!(
                stdout,
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine)
            )?;
            queue!(stdout, cursor::MoveDown(1))?;
        }
        queue!(stdout, cursor::MoveUp(frame_height as u16))?;

        let frame = renderer.render(true);
        queue!(stdout, Print(frame))?;
        stdout.flush()?;

        if renderer.all_terminal() {
            // Drain remaining events that may still be in flight (best-effort).
            while let Ok(ev) = rx.try_recv() {
                renderer.apply(ev);
            }
            // One last frame so the tally is up-to-date.
            queue!(stdout, cursor::MoveUp(frame_height as u16))?;
            for _ in 0..frame_height {
                queue!(
                    stdout,
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
                queue!(stdout, cursor::MoveDown(1))?;
            }
            queue!(stdout, cursor::MoveUp(frame_height as u16))?;
            queue!(stdout, Print(renderer.render(true)))?;
            break;
        }
    }

    execute!(stdout, cursor::Show)?;
    stdout.flush()?;
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn r4() -> WorldsRenderer {
        WorldsRenderer::new(
            vec!["w1".into(), "w2".into(), "w3".into(), "w4".into()],
            120,
        )
    }

    fn started(id: &str, action: &str) -> WorldEvent {
        WorldEvent::ActionStarted {
            world_id: id.into(),
            action_type: action.into(),
        }
    }
    fn finished(id: &str, action: &str, ms: u64) -> WorldEvent {
        WorldEvent::ActionFinished {
            world_id: id.into(),
            action_type: action.into(),
            duration_ms: ms,
        }
    }
    fn failed(id: &str, action: &str, ms: u64, err: &str) -> WorldEvent {
        WorldEvent::ActionFailed {
            world_id: id.into(),
            action_type: action.into(),
            duration_ms: ms,
            error: err.into(),
        }
    }

    #[test]
    fn empty_renderer_has_n_columns_in_pending_state() {
        let r = r4();
        let frame = r.render(false);
        // Should contain all 4 world ids in the top border
        assert!(frame.contains("w1"));
        assert!(frame.contains("w2"));
        assert!(frame.contains("w3"));
        assert!(frame.contains("w4"));
        assert!(frame.contains("parallel worlds (4)"));
        // No world is terminal yet
        assert!(!r.all_terminal());
        assert_eq!(r.tally(), (0, 0, 0, 0));
    }

    #[test]
    fn action_started_sets_running_and_current() {
        let mut r = r4();
        r.apply(started("w1", "ReadFile"));
        let w = r.worlds().get("w1").unwrap();
        assert_eq!(w.status, WorldStatus::Running);
        assert_eq!(w.current_action.as_deref(), Some("ReadFile"));

        let frame = r.render(false);
        assert!(
            frame.contains("ReadFile"),
            "current action must appear in frame:\n{}",
            frame
        );
    }

    #[test]
    fn action_finished_moves_into_history_and_clears_current() {
        let mut r = r4();
        r.apply(started("w1", "ReadFile"));
        r.apply(finished("w1", "ReadFile", 12));

        let w = r.worlds().get("w1").unwrap();
        assert!(
            w.current_action.is_none(),
            "current must be cleared after finish"
        );
        assert_eq!(w.primitives_total, 1);
        assert_eq!(w.primitives_failed, 0);
        assert_eq!(w.history.len(), 1);
        assert_eq!(w.history[0].status, PrimStatus::Ok);
        assert_eq!(w.history[0].duration_ms, 12);

        let frame = r.render(false);
        assert!(frame.contains("ReadFile"));
        assert!(frame.contains("12ms"));
    }

    #[test]
    fn action_failed_increments_failure_counter_and_keeps_world_running() {
        let mut r = r4();
        r.apply(started("w1", "WriteFile"));
        r.apply(failed("w1", "WriteFile", 7, "permission denied"));

        let w = r.worlds().get("w1").unwrap();
        // World is still running — only the action failed (e.g. inside Try)
        assert_eq!(w.status, WorldStatus::Running);
        assert_eq!(w.primitives_failed, 1);
        assert_eq!(w.primitives_total, 1);
        assert_eq!(w.history[0].status, PrimStatus::Failed);
        assert_eq!(w.last_error.as_deref(), Some("permission denied"));
    }

    #[test]
    fn history_is_capped_at_history_rows() {
        let mut r = r4();
        for i in 0..(HISTORY_ROWS + 5) {
            r.apply(started("w1", "ReadFile"));
            r.apply(finished("w1", &format!("Read#{i}"), 1));
        }
        let w = r.worlds().get("w1").unwrap();
        assert_eq!(w.history.len(), HISTORY_ROWS);
        // Most recent entry first
        assert_eq!(
            w.history[0].action_type,
            format!("Read#{}", HISTORY_ROWS + 4)
        );
    }

    #[test]
    fn terminal_events_color_correctly_in_tally() {
        let mut r = r4();
        r.apply(WorldEvent::WorldCompleted {
            world_id: "w1".into(),
            summary: None,
        });
        r.apply(WorldEvent::WorldRolledBack {
            world_id: "w2".into(),
            snapshot: Some("snap1".into()),
        });
        r.apply(WorldEvent::WorldFailed {
            world_id: "w3".into(),
            error: "boom".into(),
        });
        r.apply(WorldEvent::WorldKilled {
            world_id: "w4".into(),
            reason: "timeout".into(),
        });

        assert!(r.all_terminal());
        assert_eq!(r.tally(), (1, 1, 1, 1));

        let plain = r.render(false);
        // Status tags should be visible (compact form: ok / rollback / fail / killed)
        assert!(plain.contains("ok"));
        assert!(plain.contains("rollback"));
        assert!(plain.contains("fail"));
        assert!(plain.contains("killed"));

        // Color version embeds ANSI escape codes
        let colored = r.render(true);
        assert!(colored.contains("\x1b[32m"), "green for completed");
        assert!(colored.contains("\x1b[33m"), "yellow for rolled back");
        assert!(colored.contains("\x1b[31m"), "red for failed/killed");
    }

    #[test]
    fn unknown_world_id_is_ignored_silently() {
        let mut r = r4();
        // Should not panic, should not insert a 5th column
        r.apply(started("ghost", "ReadFile"));
        assert_eq!(r.worlds().len(), 4);
    }

    #[test]
    fn frame_width_is_stable_across_states() {
        // Width should be deterministic regardless of which actions are running.
        let mut r = r4();
        let blank = r.render(false);
        let blank_widths: Vec<usize> = blank.lines().map(|l| l.chars().count()).collect();

        r.apply(started("w1", "Pipeline"));
        r.apply(finished("w1", "Pipeline", 42));
        r.apply(started("w2", "RunCommand"));
        r.apply(WorldEvent::WorldCompleted {
            world_id: "w3".into(),
            summary: None,
        });
        r.apply(WorldEvent::WorldFailed {
            world_id: "w4".into(),
            error: "x".into(),
        });

        let after = r.render(false);
        let after_widths: Vec<usize> = after.lines().map(|l| l.chars().count()).collect();

        assert_eq!(blank_widths.len(), after_widths.len());
        // Body rows (skip header line which is centered text)
        for i in 1..blank_widths.len() - 1 {
            assert_eq!(
                blank_widths[i],
                after_widths[i],
                "row {i} width drifted: {} vs {} — line {:?} vs {:?}",
                blank_widths[i],
                after_widths[i],
                blank.lines().nth(i).unwrap_or(""),
                after.lines().nth(i).unwrap_or("")
            );
        }
    }

    #[test]
    fn long_world_ids_are_truncated_with_ellipsis() {
        let r = WorldsRenderer::new(
            vec!["this-id-is-way-too-long-for-the-column".into(), "ok".into()],
            80,
        );
        let frame = r.render(false);
        // Truncation marker should appear
        assert!(frame.contains('…'));
        assert!(frame.contains("ok"));
    }

    #[test]
    fn render_lines_matches_plain_render_structure() {
        // Ratatui render must produce exactly `frame_height()` lines and
        // preserve the same headline, content, and tally as the plain one.
        let mut r = r4();
        r.apply(started("w1", "ReadFile"));
        r.apply(finished("w1", "ReadFile", 12));
        r.apply(WorldEvent::WorldCompleted {
            world_id: "w2".into(),
            summary: None,
        });
        r.apply(WorldEvent::WorldFailed {
            world_id: "w3".into(),
            error: "boom".into(),
        });

        let lines = r.render_lines();
        assert_eq!(lines.len(), r.frame_height());

        // Convert back to plain text for cross-checks.
        let plain_from_ratatui: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain_from_ratatui.contains("parallel worlds (4)"));
        assert!(plain_from_ratatui.contains("ReadFile"));
        assert!(plain_from_ratatui.contains("12ms"));
        assert!(plain_from_ratatui.contains("ok")); // status tag for w2
        assert!(plain_from_ratatui.contains("fail")); // status tag for w3

        // First line (the header) should be centered.
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("⫸ parallel worlds (4)"));
    }

    #[test]
    fn render_lines_includes_spinner_for_running_worlds() {
        let mut r = r4();
        r.apply(started("w1", "Pipeline"));
        let lines = r.render_lines();
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // One of the spinner glyphs should be present (current_action row).
        assert!(SPINNER.iter().any(|c| joined.contains(*c)));
        assert!(joined.contains("Pipeline"));
    }

    #[test]
    fn long_action_names_are_truncated_inside_history() {
        let mut r = r4();
        let big = "AReallyLongPrimitiveNameThatWillNotFitInsideTheColumn".to_string();
        r.apply(started("w1", &big));
        r.apply(finished("w1", &big, 123));
        let frame = r.render(false);
        assert!(frame.contains('…'));
        assert!(frame.contains("123ms"));
    }
}
