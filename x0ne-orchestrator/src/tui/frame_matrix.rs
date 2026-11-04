//! Rendering matrix (T-B).
//!
//! # Why this module exists
//!
//! Measured on 2026-08-16: `TestBackend` appears only in `render.rs`, for
//! eight tests, and its three harnesses build their `AppState` HARD-CODED. The
//! gate modal (`render.rs:193`), the question modal (`:2251`), the help
//! (`:2380`), the slash bar (`:2680`), the "thinking" line (`:2037`) and the
//! badge are never rendered in a test — their tests are LOGIC tests, and
//! nothing proves that what the logic decides reaches the screen.
//!
//! The harness therefore takes an `AppState` as a parameter instead of
//! building one: every cell of the T-B matrix is "one state → one screen", and
//! the assertion bears on the `TestBackend` cells, never on a string that the
//! code under test would have regenerated (spec P2).

use ratatui::{backend::TestBackend, Terminal};

use crate::tui::app::AppState;
use crate::tui::render::{draw, ConvHits};

/// Renders `state` at the requested size and returns the rows as they are.
///
/// No `trim_end`: a cell that wants to prove a border touches the right edge
/// needs the spaces.
pub(crate) fn render_state_to_rows(state: &AppState, w: u16, h: u16) -> Vec<String> {
    render_state_with_hits(state, w, h).0
}

/// Like [`render_state_to_rows`], plus the row map that the render publishes
/// for clicks (`ConvHits`).
///
/// It serves as **geometry**: a cell that wants to read the conversation pane
/// alone finds its corner and width there, instead of copying the layout
/// computation — which is precisely what the test should catch if it
/// changed (spec P2).
pub(crate) fn render_state_with_hits(state: &AppState, w: u16, h: u16) -> (Vec<String>, ConvHits) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = ConvHits::default();
    terminal.draw(|f| draw(f, state, &mut hits)).expect("draw");
    let buf = terminal.backend().buffer().clone();
    let rows = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

/// Like above, but each row is `trim_end`-ed and the whole joined — the form
/// the eight historical tests expect.
pub(crate) fn render_state_to_text(state: &AppState, w: u16, h: u16) -> String {
    render_state_to_rows(state, w, h)
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A realistic "idle" state, the starting point for every cell.
pub(crate) fn resting_state() -> AppState {
    let mut state = AppState::new();
    state.model_label = "anthropic/claude-sonnet-5".into();
    state.sandbox_label = "stdio".into();
    state.session_id = "gf9p".into();
    state.tokens_used = 5_200;
    state.cost_usd = 0.03;
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ConfirmState, ConversationLine, EnclaveStatus, LineKind};

    /// A chatty state: enough lines, including lines that wrap, for the
    /// conversation to overflow the pane at 80×24.
    fn talkative_state(n: usize) -> AppState {
        let mut state = resting_state();
        for i in 0..n {
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
        state
    }

    /// The cells of the conversation pane alone, cut at the geometry the
    /// render published — never at a geometry recomputed here.
    fn conversation_block(state: &AppState, w: u16, h: u16) -> Vec<String> {
        let (rows, hits) = render_state_with_hits(state, w, h);
        let l = hits.left as usize;
        let r = l + hits.width as usize;
        (0..hits.rows.len())
            .map(|i| {
                let row = &rows[hits.top as usize + i];
                // One cell = one character here: if a double-width glyph
                // ever appeared in the frame, the column index would stop
                // being a cell index, and the slicing below would silently
                // lie.
                assert_eq!(
                    row.chars().count(),
                    w as usize,
                    "row {i}: one cell is no longer one character"
                );
                row.chars().skip(l).take(r - l).collect::<String>()
            })
            .collect()
    }

    /// The generalized harness renders the same thing as the old one for the
    /// default state. This is the guard that allows rewriting the eight
    /// historical tests without weakening them.
    #[test]
    fn the_generalised_harness_still_frames_every_pane() {
        let screen = render_state_to_text(&resting_state(), 120, 30);
        assert!(
            screen.contains('╭') && screen.contains('╰'),
            "the rounded corners must be there:\n{screen}"
        );
    }

    /// The harness returns the rows WITHOUT trimming them: a cell that proves
    /// a border reaches the right edge reads the last character of the row,
    /// and a `trim_end` would make it always green.
    ///
    /// A failure means `render_state_to_rows` has started normalizing what it
    /// observes — and every T-B cell that measures a right border then
    /// becomes unable to fail.
    #[test]
    fn the_row_harness_returns_the_backend_cells_untrimmed() {
        let rows = render_state_to_rows(&resting_state(), 120, 30);
        assert_eq!(rows.len(), 30, "one line per screen row");
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.chars().count(), 120, "line {y} was trimmed: {row:?}");
        }
        // And the property is not trivial: the screen does have rows that
        // `trim_end` would shorten, so the two harnesses really differ.
        let trimmed = render_state_to_text(&resting_state(), 120, 30);
        assert!(
            trimmed.lines().any(|l| l.chars().count() < 120),
            "no line to trim: the guard above can no longer fail:\n{trimmed}"
        );
    }

    // ── Scrolling ────────────────────────────────────────────────────
    //
    // 4dec66f: `draw_conversation` sliced the window into LOGICAL lines, then
    // let the `Paragraph` wrap the ones that were too wide, so one scroll
    // notch was not worth one screen row. The cells below pin the OBSERVABLE
    // property — one notch = one row — without copying the wrap computation,
    // which is precisely what must be able to fail.

    /// One scroll notch moves the pane by exactly one row.
    ///
    /// A failure means the window is once again sliced in a unit other than
    /// the painted row: the scrollbar, the click map and what the eye reads
    /// stop talking about the same document, and the most recent lines
    /// disappear off the bottom — the defect photographed by the owner on
    /// 2026-08-16.
    #[test]
    fn one_scroll_step_moves_the_conversation_by_exactly_one_row() {
        let mut state = talkative_state(60);
        state.auto_stick_bottom = false;
        for offset in [0usize, 1, 2, 7] {
            state.scroll_offset = offset;
            let before = conversation_block(&state, 80, 24);
            state.scroll_offset = offset + 1;
            let after = conversation_block(&state, 80, 24);
            assert_eq!(
                before.len(),
                after.len(),
                "the pane changed height between {offset} and {}",
                offset + 1
            );
            assert_ne!(
                before,
                after,
                "scrolling from {offset} to {} moved nothing",
                offset + 1
            );
            assert_eq!(
                &after[1..],
                &before[..before.len() - 1],
                "one notch from {offset} did not shift the pane by one row"
            );
            // Negative control: the equality above really distinguishes one
            // notch from two. Without it, a green test would not say whether
            // the offset is one row or any number.
            state.scroll_offset = offset + 2;
            let twice = conversation_block(&state, 80, 24);
            assert_ne!(
                &twice[1..],
                &before[..before.len() - 1],
                "two notches read as one from {offset}"
            );
        }
    }

    /// Scrolling past the top stops on the first line.
    ///
    /// A failure means an offset that is too large empties the pane or panics
    /// on a slice: the mouse wheel would become a way to lose the screen.
    #[test]
    fn scrolling_past_the_top_stops_on_the_first_line() {
        let mut state = talkative_state(60);
        state.auto_stick_bottom = false;
        state.scroll_offset = 10_000;
        let far = conversation_block(&state, 80, 24);
        state.scroll_offset = 1_000_000;
        let farther = conversation_block(&state, 80, 24);
        assert_eq!(far, farther, "the offset is not bounded");
        assert!(
            far.iter().any(|r| r.contains("line 000")),
            "the first line is not reached:\n{}",
            far.join("\n")
        );
    }

    // ── Badge d'enclave ──────────────────────────────────────────────

    /// The three enclave postures reach the screen, glyph and label.
    ///
    /// A failure means the posture decided by `from_verdict` is not visible:
    /// the operator who stakes weights on a rented GPU would read "connected"
    /// without knowing whether the computation is protected from its host.
    /// It is the "render" form of the thirteenth dead scaffolding.
    #[test]
    fn every_enclave_posture_reaches_the_screen() {
        let cells = [
            (EnclaveStatus::Standard, "○", "confidential GPU disabled"),
            (EnclaveStatus::CpuOnly, "◐", "cpu enclave, gpu exposed"),
            (EnclaveStatus::Attested, "●", "confidential"),
        ];
        for (posture, glyph, label) in cells {
            let mut state = resting_state();
            state.idle_status = Some("connected · ready when you are".into());
            state.enclave = posture;
            let screen = render_state_to_text(&state, 120, 30);
            assert!(
                screen.contains(label),
                "{posture:?}: the label {label:?} is not rendered:\n{screen}"
            );
            assert!(
                screen.contains(glyph),
                "{posture:?}: the glyph {glyph:?} is not rendered:\n{screen}"
            );
        }
    }

    // ── Porte humaine ────────────────────────────────────────────────

    fn pending_gate(persistent_grant: Option<&str>) -> AppState {
        let mut state = resting_state();
        state.pending_confirm = Some(ConfirmState {
            id: "q1".into(),
            action_type: "WriteFile".into(),
            action: serde_json::json!({"path": "/srv/app/config.toml"}),
            reason: "path outside allowed_write_paths".into(),
            persistent_grant: persistent_grant.map(|s| s.to_string()),
            cursor: 0,
        });
        state
    }

    /// The gate modal renders its three choices, and what they decide.
    ///
    /// A failure means approving what one cannot see (invariant 5): the logic
    /// of `ConfirmState::choices` can be right and the screen show none of it
    /// — exactly what the logic tests of `gate.rs` could not catch.
    #[test]
    fn the_gate_modal_renders_its_three_choices_and_what_it_asks_for() {
        let state = pending_gate(Some("Allow writes under /srv/app for this session"));
        let screen = render_state_to_text(&state, 120, 30);
        for needle in [
            "approval required",
            "WriteFile",
            "/srv/app/config.toml",
            "path outside allowed_write_paths",
            "allow once",
            "allow writes under /srv/app for this session",
            "deny",
        ] {
            assert!(
                screen.contains(needle),
                "the modal does not show {needle:?}:\n{screen}"
            );
        }
    }

    /// Without a permanent grant offered, the modal does not invent the option.
    ///
    /// A failure means the screen offers a scope that the authority did not
    /// grant — an approval broader than what was requested.
    #[test]
    fn the_gate_modal_never_offers_a_standing_grant_the_authority_withheld() {
        let state = pending_gate(None);
        let screen = render_state_to_text(&state, 120, 30);
        assert!(
            screen.contains("allow once") && screen.contains("deny"),
            "the two remaining choices must be there:\n{screen}"
        );
        assert!(
            !screen.contains("for this session"),
            "phantom scope on screen:\n{screen}"
        );
    }
}

// ── The sixteen panels ───────────────────────────────────────────────
//
// T-B2. A side panel is a surface that receives text of a length nobody
// controls — a page title, a GPU alert, a task label written by the model.
// The only thing that keeps it from eating the neighbour's border is the
// frame, and the frame can only be proven on screen.

#[cfg(test)]
mod panels {
    use super::*;
    use crate::tui::app::{
        ConversationLine, CronItem, FocusRegion, GoalSummary, HookRow, LineKind, PipelineRun,
        PipelineStep, PulseEvent, PulseKind, SidePanel, SourceEntry, TaskItem,
    };
    use crate::tui::badge::{BadgeFrame, BadgePlayback};
    use std::sync::{Arc, RwLock};
    use std::time::Instant;

    /// The sixteen panels, literally. A table derived from an iterator over
    /// the enum would be tempting; it would no longer say anything the day a
    /// variant disappears.
    const PANELS: &[(SidePanel, &str)] = &[
        (SidePanel::Conversation, "Conversation"),
        (SidePanel::Queue, "Queue"),
        (SidePanel::Snapshots, "Snapshots"),
        (SidePanel::Handles, "Handles"),
        (SidePanel::Worlds, "Worlds"),
        (SidePanel::Goal, "Goal"),
        (SidePanel::Tasks, "Tasks"),
        (SidePanel::Cron, "Cron"),
        (SidePanel::Pulse, "Pulse"),
        (SidePanel::Pipeline, "Pipeline"),
        (SidePanel::Sources, "Sources"),
        (SidePanel::Data, "Data"),
        (SidePanel::Matrix, "Matrix"),
        (SidePanel::Shell, "Shell"),
        (SidePanel::Hooks, "Hooks"),
        (SidePanel::Infra, "Infra"),
    ];

    const WIDTHS: &[u16] = &[80, 120, 200];
    const FOCI: &[FocusRegion] = &[FocusRegion::Conversation, FocusRegion::SidePanel];

    /// The literal table above covers the enum — no more, no less.
    ///
    /// A failure means a `SidePanel` variant was added (or removed) without
    /// the sweep following it: the matrix would keep going green while no
    /// longer measuring the whole screen. This is the only place where the
    /// table and the enum look at each other; elsewhere, the table stays
    /// literal on purpose.
    #[test]
    fn the_literal_panel_table_covers_every_variant() {
        let all = SidePanel::all();
        assert_eq!(
            PANELS.len(),
            all.len(),
            "the literal table has {} entries for {} variants",
            PANELS.len(),
            all.len()
        );
        for variant in all {
            assert!(
                PANELS.iter().any(|(p, _)| p == variant),
                "{variant:?} is not in the sweep table"
            );
        }
    }

    // ── The pane's box, read from the image ──────────────────────────

    /// The four coordinates of the side pane's box, **read off the rendered
    /// screen** and not recomputed from the layout.
    ///
    /// Copying the `Percentage(64) | 1 | Percentage(36)` split here would be
    /// exactly the mistake the spec forbids (P2): the test and the code would
    /// agree, both wrong, and a panel overflowing by one column would stay
    /// green. So the drawing is read: the top-right corner of the screen
    /// belongs to the pane, and its left edge is the last `╭` of the border
    /// row.
    struct PanelBox {
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    }

    fn cell(rows: &[String], y: usize, x: usize) -> char {
        rows[y]
            .chars()
            .nth(x)
            .unwrap_or_else(|| panic!("row {y} has no column {x}"))
    }

    /// The box of the **conversation** pane, at the columns the render
    /// published in `ConvHits`.
    ///
    /// Here the geometry is not guessed at all: `hits.left` and `hits.width`
    /// are what the click map uses to determine which message is under the
    /// mouse. If the frame and that map diverged, clicking would designate
    /// the wrong line — and this test would fail before the user did.
    fn conv_box(rows: &[String], hits: &ConvHits, ctx: &str) -> PanelBox {
        assert!(hits.left >= 1, "{ctx}: the left pane has no border");
        // Left edge: the column just before the published clickable area.
        // Right edge: the first `╮` met after it, READ from the image —
        // `hits.left + hits.width` does not give it, because the pane still
        // reserves one column for the scrollbar rail (`render.rs:713`).
        // Copying that "+1" here would be the P2 mistake: the day the rail
        // changes side, the test would follow the code.
        let left = hits.left as usize - 1;
        let top = (0..rows.len())
            .find(|y| cell(rows, *y, left) == '╭')
            .unwrap_or_else(|| panic!("{ctx}: no ╭ corner in column {left}"));
        let bottom = (top + 1..rows.len())
            .find(|y| cell(rows, *y, left) == '╰')
            .unwrap_or_else(|| panic!("{ctx}: the conversation box does not close"));
        let right = (left + 1..rows[top].chars().count())
            .find(|x| cell(rows, top, *x) == '╮')
            .unwrap_or_else(|| panic!("{ctx}: the conversation box has no ╮ corner"));
        // The frame does contain the area the click map announces: if the two
        // diverged, a click would designate a line outside the pane.
        assert!(
            left < hits.left as usize && hits.left as usize + hits.width as usize <= right,
            "{ctx}: the clickable area ({}..{}) leaves the frame ({left}..{right})",
            hits.left,
            hits.left as usize + hits.width as usize
        );
        PanelBox {
            top,
            bottom,
            left,
            right,
        }
    }

    fn panel_box(rows: &[String], w: u16, ctx: &str) -> PanelBox {
        let right = w as usize - 1;
        let top = (0..rows.len())
            .find(|y| cell(rows, *y, right) == '╮')
            .unwrap_or_else(|| panic!("{ctx}: no ╮ corner in the right column"));
        let bottom = (top + 1..rows.len())
            .find(|y| cell(rows, *y, right) == '╯')
            .unwrap_or_else(|| panic!("{ctx}: the pane box does not close"));
        let left = rows[top]
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '╭')
            .map(|(i, _)| i)
            .last()
            .unwrap_or_else(|| panic!("{ctx}: no ╭ corner on row {top}"));
        PanelBox {
            top,
            bottom,
            left,
            right,
        }
    }

    /// The pane's frame is closed on all four sides, row by row.
    ///
    /// This is THE overflow measurement. Counting a row's characters does not
    /// give it: `TestBackend` always renders exactly `w` cells and resets to
    /// `" "` the cell hidden by a double-width glyph (ratatui 0.29,
    /// `buffer.rs:360`), so `chars().count() == w` is true whatever the panel
    /// writes. A line that is too long shows up **here**: the right `│`
    /// becomes a letter.
    fn assert_box_is_closed(rows: &[String], b: &PanelBox, ctx: &str) {
        assert!(
            b.left + 2 <= b.right,
            "{ctx}: degenerate box ({}..{})",
            b.left,
            b.right
        );
        assert!(b.top + 2 <= b.bottom, "{ctx}: box shorter than three rows");
        assert_eq!(cell(rows, b.top, b.left), '╭', "{ctx}: top-left corner");
        assert_eq!(cell(rows, b.top, b.right), '╮', "{ctx}: top-right corner");
        assert_eq!(
            cell(rows, b.bottom, b.left),
            '╰',
            "{ctx}: bottom-left corner"
        );
        assert_eq!(
            cell(rows, b.bottom, b.right),
            '╯',
            "{ctx}: bottom-right corner"
        );
        for y in b.top + 1..b.bottom {
            assert_eq!(
                cell(rows, y, b.left),
                '│',
                "{ctx}: row {y}, pane left border erased:\n{}",
                rows[y]
            );
            assert_eq!(
                cell(rows, y, b.right),
                '│',
                "{ctx}: row {y}, pane right border erased:\n{}",
                rows[y]
            );
        }
    }

    /// BOTH frames of the screen body hold: the side pane's and the
    /// conversation's.
    ///
    /// Measuring them together is the point: overflowing content does not
    /// necessarily erase its own border, it erases the neighbour's.
    fn assert_both_frames_are_closed(rows: &[String], hits: &ConvHits, w: u16, ctx: &str) {
        assert_box_is_closed(rows, &panel_box(rows, w, ctx), &format!("{ctx} pane"));
        assert_box_is_closed(
            rows,
            &conv_box(rows, hits, ctx),
            &format!("{ctx} conversation"),
        );
    }

    /// No panel, at any width, under any focus, exceeds its frame.
    ///
    /// A failure would mean a panel writes outside its area and erases a
    /// neighbour's border — the defect measured on an 80×24 in 2026-08 (a
    /// five-line refusal hid the following five lines).
    #[test]
    fn no_panel_ever_writes_past_the_frame() {
        for (panel, name) in PANELS {
            for width in WIDTHS {
                for focus in FOCI {
                    let mut state = resting_state();
                    state.side_panel = *panel;
                    state.focus = *focus;
                    let (rows, hits) = render_state_with_hits(&state, *width, 30);
                    assert_eq!(rows.len(), 30, "{name}@{width}: wrong number of lines");
                    // One cell = one character: the guard that allows
                    // indexing a row by column just below.
                    for (i, row) in rows.iter().enumerate() {
                        assert_eq!(
                            row.chars().count(),
                            *width as usize,
                            "{name}@{width} focus={focus:?} line {i}: one cell is no longer \
                             one character"
                        );
                    }
                    assert_both_frames_are_closed(
                        &rows,
                        &hits,
                        *width,
                        &format!("{name}@{width} focus={focus:?}"),
                    );
                }
            }
        }
    }

    /// Each panel keeps its right border at every width.
    ///
    /// Generalizes `the_side_panel_keeps_its_right_border_at_every_width`
    /// (`render.rs`), which swept only one panel — and settled for ONE
    /// bordered row somewhere on screen. Here the border is required on
    /// **every** row of the pane: a panel that eats its frame on a single
    /// line fails.
    #[test]
    fn every_panel_keeps_its_right_border_at_every_width() {
        for (panel, name) in PANELS {
            for width in 60u16..=200 {
                let mut state = resting_state();
                state.side_panel = *panel;
                let (rows, hits) = render_state_with_hits(&state, width, 24);
                assert_both_frames_are_closed(&rows, &hits, width, &format!("{name}@{width}"));
            }
        }
    }

    /// Replaces one cell of the rendered image. Used by the two negative
    /// controls below, and nothing else.
    fn poke(rows: &mut [String], y: usize, x: usize, c: char) {
        rows[y] = rows[y]
            .chars()
            .enumerate()
            .map(|(i, old)| if i == x { c } else { old })
            .collect();
    }

    /// The instrument really measures: an eaten border makes the test fail.
    ///
    /// A failure (i.e. the absence of a panic) would mean
    /// `assert_box_is_closed` has gone blind — a `find` that no longer finds
    /// anything, an empty interval — and that the sixteen panels stay green
    /// with nothing measured. The render is not mutated to see it red: **the
    /// image** is mutated, which proves the same thing on the detector alone.
    #[test]
    #[should_panic(expected = "pane right border erased")]
    fn the_frame_check_fails_when_a_panel_line_eats_its_right_border() {
        let mut state = resting_state();
        state.side_panel = SidePanel::Sources;
        let (mut rows, hits) = render_state_with_hits(&state, 80, 24);
        let b = panel_box(&rows, 80, "negative control");
        poke(&mut rows, b.top + 1, b.right, 'X');
        assert_both_frames_are_closed(&rows, &hits, 80, "negative control");
    }

    /// Same control for the conversation frame.
    ///
    /// It is distinct because it is found differently: the side pane anchors
    /// on the screen edge, the conversation on the published clickable area.
    /// A detector can very well lose one and keep the other.
    #[test]
    #[should_panic(expected = "pane right border erased")]
    fn the_frame_check_fails_when_the_conversation_loses_its_border() {
        let state = resting_state();
        let (mut rows, hits) = render_state_with_hits(&state, 80, 24);
        let b = conv_box(&rows, &hits, "negative control");
        poke(&mut rows, b.top + 1, b.right, 'X');
        assert_both_frames_are_closed(&rows, &hits, 80, "negative control");
    }

    /// The panels still rendered as placeholders are NAMED.
    ///
    /// A placeholder is not a bug; ignoring it is. This test pins the list: a
    /// panel that regresses to a placeholder fails, and a panel that gets
    /// finished leaves the list with the commit that finishes it.
    ///
    /// The marker is "Stage 2", not "· placeholder": `placeholder_lines`
    /// (`render.rs:1977`) also serves the **empty states** of panels that are
    /// genuinely wired (worlds, matrix, data, shell). What we want to name is
    /// the panel that has no data source at all.
    #[test]
    fn the_placeholder_panels_are_exactly_the_ones_we_know_about() {
        // Read off the real render on 2026-08-16.
        const KNOWN_PLACEHOLDERS: &[&str] = &[
            "Snapshots", // Stage 2: CreateSnapshot / ListSnapshots wiring
            "Handles",   // Stage 2: orchestrator handle registry
                         // "Goal" left on 2026-08-16 (T17): the panel reads a
                         // presentation `GoalSummary`, filled from `GoalManager`.
        ];
        let mut found = Vec::new();
        for (panel, name) in PANELS {
            let mut state = resting_state();
            state.side_panel = *panel;
            let screen = render_state_to_rows(&state, 120, 30).join("\n");
            if screen.contains("Stage 2") || screen.contains("not wired") {
                found.push(*name);
            }
        }
        found.sort_unstable();
        let mut known = KNOWN_PLACEHOLDERS.to_vec();
        known.sort_unstable();
        assert_eq!(found, known, "the list of unwired panels has changed");
        // Negative control: the pattern really discriminates, it does not
        // mark the whole screen nor nothing at all.
        assert!(
            !found.is_empty() && found.len() < PANELS.len(),
            "the placeholder detector no longer tells anything apart"
        );
    }

    /// The Goal panel shows the goal instead of a placeholder.
    ///
    /// Measured on 2026-08-16: `render.rs:1495` rendered
    /// `placeholder_lines("goal", "Stage 2 reads from goal::GoalManager.")`.
    /// A failure means the operator is looking at a panel that says nothing
    /// about the current goal — i.e. they cannot see that a goal has stopped,
    /// nor why.
    #[test]
    fn the_goal_panel_shows_the_objective_and_its_history() {
        let mut state = resting_state();
        state.side_panel = SidePanel::Goal;
        state.goal_summary = Some(GoalSummary {
            text: "diagnose the slow service".into(),
            status: "active".into(),
            steps_used: 3,
            steps_limit: Some(9),
            history: vec!["step · turn ended: completed".into()],
            history_total: 4,
        });
        let screen = render_state_to_text(&state, 120, 30);
        assert!(screen.contains("diagnose the slow service"), "{screen}");
        assert!(
            !screen.contains("Stage 2"),
            "the placeholder is still there:\n{screen}"
        );
        assert!(screen.contains("3/9"), "the steps are not shown:\n{screen}");
        assert!(
            screen.contains("turn ended: completed"),
            "the history is not shown:\n{screen}"
        );
    }

    /// Without a goal, the panel says so — it does not show an empty goal.
    ///
    /// A failure would mean an empty panel reads as a goal whose text has
    /// disappeared, which is a false reading of the state.
    #[test]
    fn the_goal_panel_says_so_when_no_goal_is_active() {
        let mut state = resting_state();
        state.side_panel = SidePanel::Goal;
        assert!(
            state.goal_summary.is_none(),
            "the resting state has no goal"
        );
        let screen = render_state_to_text(&state, 120, 30);
        assert!(screen.contains("no active goal"), "{screen}");
        assert!(!screen.contains("Stage 2"), "{screen}");
    }

    // ── Filling: empty, full, overflowing ────────────────────────────

    /// A label longer than the widest frame in this file, so that wrapping
    /// has something to wrap and the border something to contain. The test
    /// checks this length instead of assuming it.
    const LONG: &str = "a label deliberately longer than the widest of the frames measured \
                        here, with enough words for ratatui's wrapping to produce several \
                        rows and push the right border of the pane to its very limits and beyond";

    fn label(i: usize) -> String {
        if i.is_multiple_of(5) {
            format!("{i:03} {LONG}")
        } else {
            format!("{i:03} short")
        }
    }

    /// What filling was able to do with a panel.
    #[derive(Debug, PartialEq)]
    enum Filled {
        /// `n` items placed in the `AppState`: the panel overflows in height
        /// AND in width.
        Rows,
        /// The filling goes into the **left** pane: the Conversation tab only
        /// renders a static reminder on the pane side, and it is the
        /// conversation's frame that the overflow puts to the test.
        LeftPane,
        /// The panel has a fixed height and can only overflow in width (the
        /// canvas is `BADGE_HEIGHT` rows, no more).
        WideOnly,
        /// Nothing to fill from the `AppState` — and why. The reason is a
        /// named debt, not an oversight.
        No(&'static str),
    }

    /// Fills `state` with `n` items for `panel`. **Literal per panel**: each
    /// panel reads a different `AppState` field, and a generic loop would
    /// prove only one thing, the one that would have been written twice.
    fn fill(panel: SidePanel, state: &mut AppState, n: usize) -> Filled {
        match panel {
            SidePanel::Conversation => {
                for i in 0..n {
                    state
                        .conversation
                        .push(ConversationLine::new(LineKind::System, label(i)));
                }
                Filled::LeftPane
            }
            SidePanel::Tasks => {
                for i in 0..n {
                    state.tasks.push(TaskItem {
                        id: i as u32,
                        title: label(i),
                        done: false,
                        created_at: Instant::now(),
                    });
                }
                Filled::Rows
            }
            SidePanel::Cron => {
                for i in 0..n {
                    state.crons.push(CronItem {
                        id: i as u32,
                        label: label(i),
                        due_at_unix: 4_000_000_000,
                        trigger_prompt: None,
                        fired: false,
                    });
                }
                Filled::Rows
            }
            SidePanel::Pulse => {
                for i in 0..n {
                    state.pulse.push(PulseEvent {
                        at: Instant::now(),
                        kind: PulseKind::Tool,
                        label: label(i),
                    });
                }
                Filled::Rows
            }
            SidePanel::Pipeline => {
                state.last_pipeline = Some(PipelineRun {
                    started_at: Instant::now(),
                    completed: true,
                    total_steps: n,
                    completed_steps: n,
                    failed: false,
                    total_duration_ms: 1_234,
                    steps: (0..n)
                        .map(|i| PipelineStep {
                            idx: i,
                            action_type: "ReadFile".into(),
                            status: "ok".into(),
                            duration_ms: 12,
                            summary: label(i),
                        })
                        .collect(),
                });
                Filled::Rows
            }
            SidePanel::Sources => {
                for i in 0..n {
                    state.sources.push(SourceEntry {
                        url: format!("https://example.invalid/{i}"),
                        title: label(i),
                        origin: "FetchUrl".into(),
                        ok: true,
                        at: Instant::now(),
                        query: None,
                        cite: None,
                    });
                }
                Filled::Rows
            }
            SidePanel::Data => {
                let payload: Vec<String> = (0..n).map(label).collect();
                state.last_result =
                    Some(("ReadFile".into(), serde_json::json!({ "lines": payload })));
                state.data_raw = true;
                Filled::Rows
            }
            SidePanel::Hooks => {
                // `HookRow::label` is `&'static str`: no index in the label,
                // two literals are enough to overflow.
                for i in 0..n {
                    state.hooks.push(HookRow {
                        id: "test-hook",
                        label: if i.is_multiple_of(5) { LONG } else { "short" },
                        enabled: i.is_multiple_of(2),
                    });
                }
                Filled::Rows
            }
            SidePanel::Infra => {
                let mut cloud = crate::cloudmon::CloudState::default();
                for i in 0..n {
                    cloud.alerts.push(label(i));
                }
                state.cloud_state = Some(Arc::new(RwLock::new(cloud)));
                Filled::Rows
            }
            SidePanel::Matrix => {
                state.badge = Some(BadgePlayback::single(BadgeFrame {
                    rows: vec![LONG.to_string(), LONG.to_string(), LONG.to_string()],
                    styles: Vec::new(),
                }));
                Filled::WideOnly
            }
            // The six that are not filled, and why. Each reason is
            // checkable; none is "did not have time".
            SidePanel::Queue => Filled::No(
                "the queue is a process global (input_queue::queue_snapshot) that \
                 two existing tests drain and count — filling it here would make them \
                 flaky",
            ),
            SidePanel::Worlds => Filled::No(
                "the panel reads worlds_registry::global(), a process registry: \
                 same parallelism hazard as the queue",
            ),
            SidePanel::Shell => Filled::No(
                "the panel only renders state.pty, a real pseudo-terminal — a render \
                 test does not spawn processes",
            ),
            SidePanel::Goal => {
                // A goal's text comes from a human, but its history carries
                // notes the model wrote: exactly the length nobody controls.
                // FIXED step counter: the only place `n` may appear on screen
                // from must be the history counter, otherwise "this panel
                // declares its total" would be true by accident
                // (cf. `an_overflowing_panel_either_declares_…`).
                state.goal_summary = Some(GoalSummary {
                    text: LONG.into(),
                    status: "active".into(),
                    steps_used: 3,
                    steps_limit: Some(9),
                    history: (0..n).map(label).collect(),
                    history_total: n,
                });
                Filled::Rows
            }
            SidePanel::Snapshots => Filled::No("Stage 2 placeholder, no data source"),
            SidePanel::Handles => Filled::No("Stage 2 placeholder, no data source"),
        }
    }

    /// The panels that cannot be filled from the `AppState` are exactly
    /// these.
    ///
    /// A failure means a panel entered (or left) the blind spot: either a data
    /// source appeared and the overflow sweep must take it, or a wired panel
    /// started reading a global and the "filling" dimension just lost it
    /// silently.
    #[test]
    fn the_panels_we_cannot_fill_from_the_state_are_named_one_by_one() {
        // "Goal" left the list on 2026-08-16 (T17): the panel reads
        // `AppState::goal_summary`, so the overflow sweep takes it.
        const UNFILLABLE: &[&str] = &["Handles", "Queue", "Shell", "Snapshots", "Worlds"];
        let mut unfillable = Vec::new();
        for (panel, name) in PANELS {
            let mut state = resting_state();
            if let Filled::No(reason) = fill(*panel, &mut state, 3) {
                assert!(!reason.is_empty(), "{name}: excluded without a reason");
                unfillable.push(*name);
            }
        }
        unfillable.sort_unstable();
        assert_eq!(
            unfillable, UNFILLABLE,
            "the list of panels out of the state's reach has changed"
        );
    }

    /// An overflowing panel stays inside its frame.
    ///
    /// Three fillings per panel — empty, full, overflowing — under both
    /// focuses. A failure means content whose length nobody controls (a page
    /// title, a GPU alert, a label written by the model) has eaten the
    /// border: on screen, two panes merge, and the operator reads in one
    /// panel what belongs to the other.
    #[test]
    fn an_overflowing_panel_stays_inside_its_frame() {
        // The test's premise: the long label really exceeds the widest frame
        // measured here. Otherwise nothing would overflow and green would say
        // nothing.
        assert!(
            LONG.chars().count() > 200,
            "the long label ({}) no longer exceeds the widest tested width",
            LONG.chars().count()
        );
        const EMPTY: usize = 0;
        const FULL: usize = 20;
        const OVERFLOWING: usize = 40;

        for (panel, name) in PANELS {
            for n in [EMPTY, FULL, OVERFLOWING] {
                for focus in FOCI {
                    for (w, h) in [(80u16, 24u16), (120, 30), (200, 40)] {
                        let mut state = resting_state();
                        state.side_panel = *panel;
                        state.focus = *focus;
                        let filled = fill(*panel, &mut state, n);
                        let (rows, hits) = render_state_with_hits(&state, w, h);
                        let ctx = format!("{name} n={n} {w}×{h} focus={focus:?}");
                        assert_both_frames_are_closed(&rows, &hits, w, &ctx);

                        // Non-vacuity check: at the overflowing filling, the
                        // content does reach the last inner row of the pane
                        // it targets. Without it, a panel that rendered
                        // nothing at all would pass this test with its eyes
                        // closed.
                        let overflown = match filled {
                            Filled::Rows => Some(panel_box(&rows, w, &ctx)),
                            Filled::LeftPane => Some(conv_box(&rows, &hits, &ctx)),
                            Filled::WideOnly | Filled::No(_) => None,
                        };
                        if let (Some(b), OVERFLOWING) = (overflown, n) {
                            let last_inner: String = rows[b.bottom - 1]
                                .chars()
                                .skip(b.left + 1)
                                .take(b.right - b.left - 1)
                                .collect();
                            assert!(
                                !last_inner.trim().is_empty(),
                                "{ctx}: the pane is not full, the overflow is not \
                                 measured"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Does an overflowing panel declare what it does not show? The survey,
    /// panel by panel.
    ///
    /// Cross-cutting property of the T-B spec: "all truncated content is
    /// declared truncated". Measured on 2026-08-16: four panels announce their
    /// total at the top (so the eye knows some are missing), six cut
    /// silently. This test pins both lists rather than painting green a
    /// screen that lies by omission: a panel that gains its counter leaves
    /// `SILENT`, a panel that loses its counter fails.
    ///
    /// A failure means this display contract moved without being said — in
    /// either direction.
    #[test]
    fn an_overflowing_panel_either_declares_its_total_or_is_a_named_silence() {
        /// A count that appears nowhere else on screen (no token, no cost, no
        /// session identifier): its presence can only come from the panel's
        /// counter.
        const N: usize = 137;
        // "Goal" had joined DECLARES on 2026-08-16 (T17): the panel showed
        // the last history entries and said how many there are. It LEFT on
        // 2026-09-10, and not because anyone wanted it to: the banner went
        // from 4 to 7 lines for the five-row logo, and at 80×24 the counter
        // line is the one that drops.
        //
        // It is a silence that is no longer honest: the panel hides entries
        // without saying how many. It is listed here rather than fixed,
        // because the owner decided on 2026-09-10 knowing the cost. Making it
        // talkative again requires either shrinking the banner, or fitting
        // the counter into a line already displayed.
        const DECLARES: &[&str] = &["Cron", "Pipeline", "Sources", "Tasks"];
        const SILENT: &[&str] = &[
            "Conversation",
            "Data",
            "Goal",
            "Hooks",
            "Infra",
            "Matrix",
            "Pulse",
        ];

        let mut declares = Vec::new();
        let mut silent = Vec::new();
        for (panel, name) in PANELS {
            let mut state = resting_state();
            state.side_panel = *panel;
            if matches!(fill(*panel, &mut state, N), Filled::No(_)) {
                continue;
            }
            let screen = render_state_to_rows(&state, 80, 24).join("\n");
            if screen.contains("137") {
                declares.push(*name);
            } else {
                silent.push(*name);
            }
        }
        declares.sort_unstable();
        silent.sort_unstable();
        assert_eq!(
            declares, DECLARES,
            "a panel gained or lost the announcement of its total"
        );
        assert_eq!(
            silent, SILENT,
            "the list of panels that truncate silently has changed"
        );
        // And the screen does not show 137 rows: what is announced is well
        // beyond what is visible, otherwise "declaring" would mean nothing.
        assert!(N > 24, "the filling no longer exceeds the screen height");
    }
}

// ── T-B3: the gate, the refusal, the badge, the wait ──────────────────
//
// T-B2 measures the FRAME; here we measure what the frame contains at the
// second a decision is made. The tests of `gate.rs` (5), `badge.rs` (20)
// and `app.rs` (84) are LOGIC tests: they prove the right answer is
// computed, never that it reaches the operator's screen. A gate with a
// choice missing from the display is not a gate: answering a badly
// rendered question is not answering.

#[cfg(test)]
mod gate {
    use super::*;
    use crate::tui::app::ConfirmState;
    use serde_json::json;

    /// The widths at which the modal must keep its promise. 60 is the narrow
    /// terminal of a vertical split; 200 a full screen.
    const WIDTHS: &[u16] = &[60, 80, 120, 200];

    /// Arms the gate by setting **exactly** the fields that
    /// `confirm_modal_height` (`render.rs:178`) and `draw_confirm_modal`
    /// (`:193`) read — `action_type`, `action`, `reason`,
    /// `persistent_grant`, `cursor`.
    ///
    /// Deliberately without going through `confirm::decide`: this module
    /// measures the RENDER. Having the logic decide here would make the test
    /// green the day the logic and the render were wrong together — the P2
    /// mistake.
    fn arm_confirm(
        state: &mut AppState,
        action_type: &str,
        action: serde_json::Value,
        grant: Option<&str>,
    ) {
        state.pending_confirm = Some(ConfirmState {
            id: "q1".into(),
            action_type: action_type.into(),
            action,
            reason: "this action can change state on the host and needs your approval".into(),
            persistent_grant: grant.map(|s| s.to_string()),
            cursor: 0,
        });
    }

    /// The same gate, on a composite — the form `confirm::decide` receives
    /// when the model sends a `Pipeline` (the whole action is passed as is,
    /// `agent/confirm.rs:120`).
    fn arm_confirm_pipeline(state: &mut AppState, paths: &[&str]) {
        let steps: Vec<serde_json::Value> = paths
            .iter()
            .map(|p| json!({"type": "WriteFile", "path": p, "content": "x"}))
            .collect();
        arm_confirm(
            state,
            "Pipeline",
            json!({"type": "Pipeline", "steps": steps}),
            None,
        );
    }

    /// A needle searched **row by row**, never in the rejoined screen: a word
    /// cut by a wrap must not pass as visible.
    fn on_some_row(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|r| r.contains(needle))
    }

    /// The gate modal displays the three choices, at every width.
    ///
    /// SECURITY: a failure would mean an operator sees a question with one
    /// answer missing from the screen. This modal had never been rendered at
    /// a width other than 120 before today (measured 2026-08-16); its tests
    /// were all logic. Each choice is searched on ONE row, with its number
    /// attached to its label: "3" alone is found in the binary's version, and
    /// a test that searches for "3" measures nothing.
    #[test]
    fn the_gate_modal_shows_every_choice_at_every_width() {
        for width in WIDTHS {
            let mut state = resting_state();
            arm_confirm(
                &mut state,
                "WriteFile",
                json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
                Some("Allow writes under /srv/app for this session"),
            );
            let rows = render_state_to_rows(&state, *width, 30);
            for choice in [
                "1  allow once",
                "2  allow writes under /srv/app for this session",
                "3  deny",
            ] {
                assert!(
                    on_some_row(&rows, choice),
                    "width {width}: the choice \"{choice}\" is missing:\n{}",
                    rows.join("\n")
                );
            }
            // And what is approved is named, at the same width: a readable
            // choice above a truncated question is no better.
            for needle in ["approval required", "WriteFile", "/srv/app/config.toml"] {
                assert!(
                    on_some_row(&rows, needle),
                    "width {width}: {needle:?} is not on screen:\n{}",
                    rows.join("\n")
                );
            }
        }
    }

    /// Only one choice carries the cursor, and the arrow follows `cursor`.
    ///
    /// A failure would mean Enter confirms something other than what the eye
    /// designates — the worst form of the defect, because the operator
    /// believes they have answered.
    #[test]
    fn exactly_one_choice_carries_the_cursor_and_it_is_the_one_selected() {
        let labels = [
            "1  allow once",
            "2  allow writes under /srv/app for this session",
            "3  deny",
        ];
        for (cursor, label) in labels.iter().enumerate() {
            let mut state = resting_state();
            arm_confirm(
                &mut state,
                "WriteFile",
                json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
                Some("Allow writes under /srv/app for this session"),
            );
            state.pending_confirm.as_mut().expect("armed").cursor = cursor;
            let rows = render_state_to_rows(&state, 120, 30);
            let marked: Vec<&String> = rows.iter().filter(|r| r.contains('❯')).collect();
            assert_eq!(
                marked.len(),
                1,
                "cursor {cursor}: {} rows marked:\n{}",
                marked.len(),
                rows.join("\n")
            );
            assert!(
                marked[0].contains(label),
                "cursor {cursor}: the arrow points at {:?}, not \"{label}\"",
                marked[0].trim()
            );
        }
    }

    /// Without a permanent grant offered, the modal invents neither the option
    /// nor its number.
    ///
    /// SECURITY: a failure would mean the screen offers a scope that the
    /// authority did not grant. `render.rs` builds it from `persistent_grant`;
    /// this test proves it on screen, at every width.
    #[test]
    fn the_gate_modal_never_offers_a_standing_grant_at_any_width() {
        for width in WIDTHS {
            let mut state = resting_state();
            arm_confirm(
                &mut state,
                "WriteFile",
                json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
                None,
            );
            let rows = render_state_to_rows(&state, *width, 30);
            assert!(
                on_some_row(&rows, "1  allow once") && on_some_row(&rows, "3  deny"),
                "width {width}: the two remaining choices must be there:\n{}",
                rows.join("\n")
            );
            assert!(
                !on_some_row(&rows, "for this session"),
                "width {width}: phantom scope on screen:\n{}",
                rows.join("\n")
            );
        }
    }

    /// The modal names the most telling argument of the action, whatever its
    /// form: `path`, `command` or `url`.
    ///
    /// A failure would mean a bare "WriteFile" above three choices —
    /// approving an action class without seeing its instance, which is what
    /// the gate exists precisely to prevent (`app.rs:1500`, capture
    /// 2026-08-13).
    #[test]
    fn the_gate_modal_names_the_argument_whatever_its_shape() {
        let cells = [
            (
                "WriteFile",
                json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
                "/srv/app/config.toml",
            ),
            (
                "RunCommand",
                json!({"type": "RunCommand", "command": "systemctl", "args": ["restart", "nginx"]}),
                "systemctl",
            ),
            (
                "FetchUrl",
                json!({"type": "FetchUrl", "url": "https://example.invalid/payload"}),
                "https://example.invalid/payload",
            ),
        ];
        for (action_type, action, argument) in cells {
            for width in [80u16, 120] {
                let mut state = resting_state();
                arm_confirm(&mut state, action_type, action.clone(), None);
                let rows = render_state_to_rows(&state, width, 30);
                assert!(
                    on_some_row(&rows, action_type) && on_some_row(&rows, argument),
                    "{action_type}@{width}: {argument:?} is not rendered:\n{}",
                    rows.join("\n")
                );
            }
        }
    }

    /// A composite arrives whole in the state and the screen shows EACH of its
    /// steps — or says how many it hides.
    ///
    /// SECURITY: invariant 5, "what is approved must be readable". Measured
    /// on 2026-08-16: the modal rendered only `action_type` then
    /// `path`/`command`/`url`, and a `Pipeline` of five writes was approved
    /// without any of them on screen (test "surveyed, not approved", T8).
    /// Closed on 2026-08-17: the modal renders `action_box` +
    /// `render_manifest` (the CLI's renderers), budgeted — on a terminal too
    /// short for everything, the choices stay and a row announces how many
    /// action rows were cut. This test is the opposite of the previous one.
    #[test]
    fn a_composite_reaches_the_modal_whole_and_the_screen_shows_its_steps_or_says_how_many_it_hides(
    ) {
        const STEPS: &[&str] = &[
            "/tmp/a.txt",
            "/tmp/b.txt",
            "/tmp/c.txt",
            "/tmp/d.txt",
            "/tmp/e.txt",
        ];
        let mut state = resting_state();
        arm_confirm_pipeline(&mut state, STEPS);
        let carried = state.pending_confirm.as_ref().expect("gate armed").action["steps"]
            .as_array()
            .expect("a composite has steps")
            .len();
        assert_eq!(
            carried,
            STEPS.len(),
            "the state does not carry the composite"
        );

        // Tall enough: every step is on screen, with the choices.
        for (w, h) in [(120u16, 40u16), (200, 50)] {
            let rows = render_state_to_rows(&state, w, h);
            assert!(
                on_some_row(&rows, "1  allow once") && on_some_row(&rows, "3  deny"),
                "{w}×{h}: the choices are missing:\n{}",
                rows.join("\n")
            );
            for step in STEPS {
                assert!(
                    on_some_row(&rows, step),
                    "{w}×{h}: {step} is not on screen — invariant 5:\n{}",
                    rows.join("\n")
                );
            }
        }
        // Too short for everything: the choices stay, and the screen SAYS
        // what it hides rather than keeping quiet about it.
        let rows = render_state_to_rows(&state, 80, 18);
        let screen = rows.join("\n");
        assert!(
            on_some_row(&rows, "1  allow once") && on_some_row(&rows, "3  deny"),
            "80×18: the choices must survive the lack of space:\n{screen}"
        );
        let all_shown = STEPS.iter().all(|s| on_some_row(&rows, s));
        assert!(
            all_shown || on_some_row(&rows, "more rows not shown"),
            "80×18: neither all the steps nor the admission that some are missing:\n{screen}"
        );
    }

    /// Below which height the gate stops being readable.
    ///
    /// The modal takes `4 + choices` rows (`confirm_modal_height`), but the
    /// body claims `Min(8)` before it: on a short terminal, ratatui trims,
    /// and it is the modal that loses its rows — silently.
    /// This test surveys the **first height** at which all three choices are
    /// rendered and pins it: a layout change that raised it would make the
    /// gate mute on terminals where it used to speak.
    ///
    /// 20 = 4 (banner) + 8 (body, `Min(8)`) + 7 (three-choice modal) + 1
    /// (slash bar). What disappears below is measured by the next test, and
    /// it is not what one would want.
    #[test]
    fn the_gate_needs_this_many_rows_before_every_choice_is_readable() {
        /// Surveyed on the real render on 2026-08-16, at 120 columns: 20.
        /// Re-surveyed on 2026-08-17 after the budgeted modal (the choices are
        /// placed first, the body can go down to `GATE_BODY_MIN` = 1):
        /// 11 = 4 (banner) + 1 (body) + 5 (modal: border 2 + 3 choices — the
        /// reason only comes back at 12, the action above) + 1 (slash bar).
        ///
        /// **13 since 2026-09-10**, and it is an accepted REGRESSION, not a
        /// measurement that improves: the banner went from 4 to 6 lines to
        /// host the logo. It was 14 for one hour, with a fifth decorative row
        /// — three floating dashes that the owner saw on a screenshot and had
        /// removed. The row given back is the line given back to the gate.
        ///
        /// What it costs, said plainly: under 14 lines, an approval request
        /// may not show all its choices. It is repository invariant 5 ("what
        /// is approved must be readable") weakened on a short terminal.
        /// Decided by the owner on 2026-09-10, knowing the number.
        const READABLE_FROM: u16 = 13;
        let mut state = resting_state();
        arm_confirm(
            &mut state,
            "WriteFile",
            json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
            Some("Allow writes under /srv/app for this session"),
        );
        let readable = |h: u16| {
            let rows = render_state_to_rows(&state, 120, h);
            ["1  allow once", "2  allow writes", "3  deny"]
                .iter()
                .all(|c| on_some_row(&rows, c))
        };
        let first = (1u16..=40).find(|h| readable(*h)).expect("no height");
        assert_eq!(
            first, READABLE_FROM,
            "the gate's minimum readable height has moved"
        );
        // And it stays so above: readability that comes and goes with the
        // height would be worse than a threshold.
        for h in first..=40 {
            assert!(readable(h), "height {h}: the gate becomes unreadable again");
        }
    }

    /// On a short terminal, "deny" NEVER leaves the screen before the other
    /// choices: what gives way is the action's detail.
    ///
    /// SECURITY: measured on 2026-08-16 (test "surveyed, not approved", T8):
    /// the modal was a `Paragraph` trimmed from the bottom, its choices in the
    /// order 1, 2, 3 — at 19 rows the operator saw "allow once" and "allow …
    /// for this session" and NOTHING else, the cursor on "allow once", Enter
    /// under the finger. Closed on 2026-08-17: `confirm_modal_height` sizes
    /// itself against the frame height, the body gives way down to
    /// `GATE_BODY_MIN`, and `draw_confirm_modal` places the reason and the
    /// choices in its budget BEFORE the action rows. This test is the
    /// opposite of the previous one: at every height where a choice is
    /// visible, "deny" is.
    #[test]
    fn on_a_short_terminal_deny_stays_on_screen_as_long_as_any_choice_does() {
        let mut state = resting_state();
        arm_confirm(
            &mut state,
            "WriteFile",
            json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
            Some("Allow writes under /srv/app for this session"),
        );
        let mut seen_any = false;
        for h in 8u16..=24 {
            let rows = render_state_to_rows(&state, 120, h);
            let screen = rows.join("\n");
            let any_choice = [
                "1  allow once",
                "2  allow writes under /srv/app for this session",
            ]
            .iter()
            .any(|c| on_some_row(&rows, c));
            if any_choice {
                seen_any = true;
                assert!(
                    on_some_row(&rows, "3  deny"),
                    "height {h}: a choice is visible but not \"deny\":\n{screen}"
                );
            }
        }
        assert!(
            seen_any,
            "no height up to 24 shows a choice — the gate is mute"
        );
    }
}

#[cfg(test)]
mod refusals {
    use super::*;
    use crate::tui::wire::AgentEvent;
    use serde_json::json;
    use x0ne_runtime::policy::{DenyReason, PathAccess};

    /// What a refusal must give the operator so they can lift it.
    enum Lever {
        /// The configuration key, as it is written in
        /// `x0ne-runtime.toml`.
        Named(&'static str),
        /// No configuration lever, and why. A checkable reason, not an
        /// oversight.
        None(&'static str),
    }

    /// The lever of each refusal. **Exhaustive, no `_` arm** — same
    /// mechanism as repository invariant 3: the next `DenyReason` added must
    /// not be able to slip into "nothing to say" by default, it must make
    /// this test fail to compile until someone decides what the screen shows
    /// of it.
    fn lever(reason: &DenyReason) -> Lever {
        match reason {
            DenyReason::ActionDisabled => Lever::Named("[actions]"),
            DenyReason::PathMatchesDenyPattern { .. } => Lever::Named("deny_path_patterns"),
            DenyReason::PathNotInAllowlist { access, .. } => match access {
                PathAccess::Read => Lever::Named("allowed_read_paths"),
                PathAccess::Write => Lever::Named("allowed_write_paths"),
            },
            DenyReason::CommandNotAllowed { .. } => Lever::Named("allowed_commands"),
            DenyReason::DefaultDeny => Lever::Named("default_effect"),
            DenyReason::DesktopDisabled => Lever::Named("desktop.enabled"),
            DenyReason::EnvVarNotReadable { .. } => Lever::Named("readable_env_vars"),
            DenyReason::ImageNotAllowed { .. } => Lever::Named("allowed_images"),
            DenyReason::PipelineStepDenied { .. } => Lever::Named("allowed_commands"),
            DenyReason::NestingNotAllowed => Lever::None(
                "the maximum depth (3) is a runtime constant, not a key: \
                 the message says what to do (flatten), and there is nothing to edit",
            ),
            DenyReason::ReadonlyMode => Lever::None(
                "readonly is the mode itself; the message names it in full, \
                 and changing mode is not lifting a denial, it is changing posture",
            ),
            DenyReason::PlanMode => Lever::None(
                "plan mode is the session's posture, chosen by the human; \
                 the message names /build, and no key or grant lifts it",
            ),
            DenyReason::CallerEnvDenied { .. } => Lever::None(
                "the loader denylist (env_keys: LD_PRELOAD and relatives) is closed \
                 by design — no TOML key behind it, nothing a yes could \
                 grant; the message relays check_caller_env's denial",
            ),
            DenyReason::CommandNotAnalyzable { .. } => Lever::None(
                "an opaque shell script's programs are not on screen, nothing a yes \
                 could vouch for; liftable() is None and the way past is \
                 --allow-all-commands in a container",
            ),
        }
    }

    /// The state after a runtime refusal, via the **real** path: the runtime
    /// answers a `policy_denied` envelope, `apply_event` pushes it into the
    /// conversation via `display::result_line` (`app.rs:1473`), and the render
    /// wraps it in the pane. Nothing is written by hand into `conversation`.
    fn refused(action: &serde_json::Value, reason: &DenyReason) -> AppState {
        let mut state = resting_state();
        state.apply_event(AgentEvent::ToolEnd {
            name: action["type"].as_str().unwrap_or("action").to_string(),
            ok: false,
            summary: String::new(),
            action: action.clone(),
            result: json!({
                "success": false,
                "error": {"class": "policy_denied", "message": reason.message()}
            }),
            duration_ms: 2,
        });
        state
    }

    /// The eleven forms of refusal, each with the action that triggers it and
    /// the word the screen must render to say **what** it is about.
    ///
    /// Literal table: derived from an iterator, it would stop saying anything
    /// at all the day a variant disappeared.
    fn cases() -> Vec<(&'static str, serde_json::Value, DenyReason, &'static str)> {
        vec![
            (
                "ActionDisabled",
                json!({"type": "RunCommand", "command": "systemctl"}),
                DenyReason::ActionDisabled,
                "disabled",
            ),
            (
                "PathMatchesDenyPattern",
                json!({"type": "ReadFile", "path": "/home/op/.ssh/id_ed25519"}),
                DenyReason::PathMatchesDenyPattern {
                    path: "/home/op/.ssh/id_ed25519".into(),
                    pattern: "**/.ssh/**".into(),
                },
                "/home/op/.ssh/id_ed25519",
            ),
            (
                "PathNotInAllowlist/read",
                json!({"type": "ReadFile", "path": "/etc/shadow"}),
                DenyReason::PathNotInAllowlist {
                    path: "/etc/shadow".into(),
                    access: PathAccess::Read,
                },
                "/etc/shadow",
            ),
            (
                "PathNotInAllowlist/write",
                json!({"type": "WriteFile", "path": "/etc/passwd", "content": "x"}),
                DenyReason::PathNotInAllowlist {
                    path: "/etc/passwd".into(),
                    access: PathAccess::Write,
                },
                "/etc/passwd",
            ),
            (
                "CommandNotAllowed",
                json!({"type": "RunCommand", "command": "curl"}),
                DenyReason::CommandNotAllowed {
                    command: "curl".into(),
                },
                "curl",
            ),
            (
                "DefaultDeny",
                json!({"type": "SendInput", "text": "hello"}),
                DenyReason::DefaultDeny,
                "default",
            ),
            (
                "DesktopDisabled",
                json!({"type": "Screenshot"}),
                DenyReason::DesktopDisabled,
                "Desktop",
            ),
            (
                "EnvVarNotReadable",
                json!({"type": "ReadEnv", "name": "AWS_SECRET_ACCESS_KEY"}),
                DenyReason::EnvVarNotReadable {
                    name: "AWS_SECRET_ACCESS_KEY".into(),
                },
                "AWS_SECRET_ACCESS_KEY",
            ),
            (
                "ImageNotAllowed",
                json!({"type": "RunInSandbox", "image": "ghcr.io/evil/base:latest"}),
                DenyReason::ImageNotAllowed {
                    image: "ghcr.io/evil/base:latest".into(),
                },
                "ghcr.io/evil/base:latest",
            ),
            (
                "NestingNotAllowed",
                json!({"type": "Pipeline", "steps": []}),
                DenyReason::NestingNotAllowed,
                "Flatten",
            ),
            (
                "ReadonlyMode",
                json!({"type": "WriteFile", "path": "/srv/app/x", "content": "x"}),
                DenyReason::ReadonlyMode,
                "readonly",
            ),
            (
                "PipelineStepDenied",
                json!({"type": "Pipeline", "steps": [{"type": "RunCommand", "command": "curl"}]}),
                DenyReason::PipelineStepDenied {
                    step: 2,
                    step_type: "RunCommand".into(),
                    reason: Box::new(DenyReason::CommandNotAllowed {
                        command: "curl".into(),
                    }),
                },
                "curl",
            ),
        ]
    }

    /// Each refusal reaches the screen, names what it refuses, and names the
    /// key that would lift it.
    ///
    /// SECURITY: a failure means an operator reads "✗" without knowing what
    /// to edit, or worse, believes they understood and edits the wrong list —
    /// exactly what `PathAccess` (`policy.rs:26-31`) was added to prevent on
    /// 2026-08-16, and the field is useless if the screen does not render it.
    /// Each needle is searched on ONE row: wrapping is greedy by word
    /// (`render.rs:837`), so a whole word fits on one row, and searching the
    /// rejoined screen would let through a key cut in two.
    #[test]
    fn every_refusal_names_what_it_refuses_and_the_key_that_lifts_it() {
        for (ctx, action, reason, echoed) in cases() {
            let state = refused(&action, &reason);
            let rows = render_state_to_rows(&state, 120, 30);
            let screen = rows.join("\n");
            assert!(
                rows.iter().any(|r| r.contains(echoed)),
                "{ctx}: the screen does not say what it is about ({echoed:?}):\n{screen}"
            );
            match lever(&reason) {
                Lever::Named(key) => assert!(
                    rows.iter().any(|r| r.contains(key)),
                    "{ctx}: the key {key:?} is not on screen:\n{screen}"
                ),
                Lever::None(why) => assert!(!why.is_empty(), "{ctx}: excluded without a reason"),
            }
        }
    }

    /// The survey is not trivial: without a refusal, none of these keys is on
    /// screen.
    ///
    /// Without this check, a screen that permanently displayed a
    /// configuration reminder would make the test above green without any
    /// refusal having been rendered.
    #[test]
    fn a_resting_screen_names_no_policy_key_at_all() {
        let rows = render_state_to_rows(&resting_state(), 120, 30);
        for (ctx, _, reason, _) in cases() {
            if let Lever::Named(key) = lever(&reason) {
                assert!(
                    !rows.iter().any(|r| r.contains(key)),
                    "{ctx}: {key:?} is on screen although no denial occurred:\n{}",
                    rows.join("\n")
                );
            }
        }
    }

    /// **Surveyed, not approved**: the "hint" line of a path refusal still
    /// names `policy.allowed_dirs`, a key of the v0.2 schema that no longer
    /// exists.
    ///
    /// Measured on 2026-08-16: `display::denial_fix` (`display.rs:1916` and
    /// `:1947`) advises adding the path to `policy.allowed_dirs`, and this
    /// line arrives as is in the conversation pane, just below the message
    /// which, for its part, names the right list. The proof that the key is
    /// dead is in the repository: `setup.rs:1199` refuses to let a profile
    /// write `allowed_dirs` ("the removed v0.2 key"), and `cc_runner.rs:1435`
    /// says the same. An operator who follows the advice edits a key the
    /// runtime ignores, believes they lifted the refusal, and the refusal
    /// comes back.
    ///
    /// Second defect of the same line: it does not distinguish read from
    /// write, whereas the message above does.
    ///
    /// `display.rs` is not fixed from a test. This test pins the measurement
    /// so it is not lost; the day the line names
    /// `allowed_read_paths` / `allowed_write_paths`, it will show here, and
    /// this test must be replaced by its opposite.
    #[test]
    fn the_refusal_hint_names_the_v0_3_list_the_refusal_names() {
        // Replaced on 2026-08-20 (the opposite of the measurement it pinned):
        // the hint now relays the list that the runtime's refusal names —
        // read or write — and the dead v0.2 key is gone (9360e95). A
        // failure would mean the return of the dead advice: an operator edits
        // a key the runtime ignores, believes they lifted the refusal, and
        // the refusal comes back.
        let action = json!({"type": "WriteFile", "path": "/etc/passwd", "content": "x"});
        let reason = DenyReason::PathNotInAllowlist {
            path: "/etc/passwd".into(),
            access: PathAccess::Write,
        };
        let state = refused(&action, &reason);
        let rows = render_state_to_rows(&state, 120, 30);
        let hint = rows
            .iter()
            .find(|r| r.contains("hint"))
            .unwrap_or_else(|| panic!("no hint line:\n{}", rows.join("\n")));
        assert!(
            hint.contains("allowed_write_paths"),
            "the hint must relay the v0.3 list the denial names: {hint:?}"
        );
        assert!(
            !hint.contains("allowed_dirs"),
            "the dead v0.2 key is back in the hint: {hint:?}"
        );
    }
}

#[cfg(test)]
mod enclave {
    use super::*;
    use x0ne_confidential::model::{
        ExecutionEnvironment, GpuCcMode, Measurement, TeeTechnology, Verdict, VerifierStrength,
        WorkloadIdentity,
    };

    use crate::tui::app::EnclaveStatus;

    /// A verified verdict — the only thing that can light the badge.
    fn verified(gpu: GpuCcMode) -> Verdict {
        Verdict::Verified {
            identity: WorkloadIdentity {
                name: "x0ne-session".into(),
                environment: ExecutionEnvironment::ConfidentialVmWithGpu {
                    cpu: TeeTechnology::AmdSevSnp,
                    gpu_mode: gpu,
                },
                measurements: vec![Measurement {
                    register: "launch_digest".into(),
                    algorithm: "sha384".into(),
                    value_hex: "ab".repeat(48),
                }],
            },
            verifier: "structural".into(),
            strength: VerifierStrength::Cryptographic,
        }
    }

    /// A refused verdict — a machine that claims, with no retained proof.
    fn rejected() -> Verdict {
        Verdict::Rejected {
            reason: "launch digest does not match the expected measurement".into(),
            verifier: "structural".into(),
        }
    }

    fn at_rest(status: EnclaveStatus) -> AppState {
        let mut state = resting_state();
        state.idle_status = Some("connected · ready when you are".into());
        state.enclave = status;
        state
    }

    /// The displayed posture comes from a verdict, proof or not — and a claim
    /// without proof lights nothing.
    ///
    /// SECURITY: a failure would mean a compromised host gets a green badge by
    /// declaring `GpuCcMode::On`. `from_verdict` already refuses to do so
    /// (`app.rs:536`); this test proves the REFUSAL reaches the screen, which
    /// is the half that was missing — thirteenth dead scaffolding.
    /// The four cells cover the three postures and both halves of
    /// "with / without proof".
    #[test]
    fn the_badge_shows_the_posture_a_verdict_earned_and_no_other() {
        let cells = [
            (
                "proof + confidential GPU",
                verified(GpuCcMode::On),
                true,
                EnclaveStatus::Attested,
                "●",
                "confidential",
            ),
            (
                "proof, GPU exposed",
                verified(GpuCcMode::Off),
                false,
                EnclaveStatus::CpuOnly,
                "◐",
                "cpu enclave, gpu exposed",
            ),
            (
                "no proof, GPU that claims",
                rejected(),
                true,
                EnclaveStatus::Standard,
                "○",
                "confidential GPU disabled",
            ),
            (
                "no proof, no claim",
                rejected(),
                false,
                EnclaveStatus::Standard,
                "○",
                "confidential GPU disabled",
            ),
        ];
        for (ctx, verdict, gpu_claim, expected, glyph, label) in cells {
            let posture = EnclaveStatus::from_verdict(&verdict, gpu_claim);
            assert_eq!(posture, expected, "{ctx}: wrong posture decided");
            let rows = render_state_to_rows(&at_rest(posture), 120, 30);
            let row = rows.iter().find(|r| r.contains(label)).unwrap_or_else(|| {
                panic!("{ctx}: {label:?} is not rendered:\n{}", rows.join("\n"))
            });
            assert!(
                row.contains(glyph),
                "{ctx}: the glyph {glyph:?} is missing from the badge row: {row:?}"
            );
            // Both facts are on the SAME row as the presence: read
            // separately, "connected" would be taken for the posture.
            assert!(
                row.contains("connected"),
                "{ctx}: the posture left the presence row: {row:?}"
            );
        }
    }

    /// The badge exists only when idle, and that is measured, not assumed.
    ///
    /// The status row is contested: as soon as an activity starts, the
    /// posture gives way to "x0ne is thinking" (`render.rs:2045`). An
    /// operator who stakes weights on a rented GPU therefore cannot read the
    /// posture while the turn is running — which is when it matters. The
    /// survey is here so that this choice stays a choice.
    #[test]
    fn the_posture_leaves_the_screen_as_soon_as_the_agent_works() {
        let mut state = at_rest(EnclaveStatus::CpuOnly);
        let rows = render_state_to_rows(&state, 120, 30);
        assert!(
            rows.iter().any(|r| r.contains("cpu enclave, gpu exposed")),
            "premise: the posture must be readable at rest"
        );
        state.awaiting_first_token = true;
        let busy = render_state_to_rows(&state, 120, 30);
        assert!(
            busy.iter().any(|r| r.contains("x0ne is thinking")),
            "the activity must take the row:\n{}",
            busy.join("\n")
        );
        assert!(
            !busy.iter().any(|r| r.contains("cpu enclave, gpu exposed")),
            "the posture survives the activity — the reading has changed:\n{}",
            busy.join("\n")
        );
    }
}

#[cfg(test)]
mod liveness {
    use super::*;
    use std::time::Instant;

    /// The waiting row, or `None` when it is not rendered at all.
    fn status_row(state: &AppState) -> Option<String> {
        render_state_to_rows(state, 120, 30)
            .into_iter()
            .find(|r| r.contains("x0ne is ") || r.contains("connected ·"))
    }

    /// The four postures of the waiting line reach the screen.
    ///
    /// A failure would mean a thirty-second turn unfolds under a mute screen:
    /// the operator can no longer tell "the agent is thinking" from "the
    /// agent is stuck", and the only available action becomes Ctrl+C.
    #[test]
    fn every_phase_of_the_waiting_row_reaches_the_screen() {
        let mut idle = resting_state();
        idle.idle_status = Some("connected · ready when you are".into());

        let mut thinking = resting_state();
        thinking.awaiting_first_token = true;

        let mut writing = resting_state();
        writing.agent_busy = true;
        writing.assistant_buffer = "two words".into();

        let mut running = resting_state();
        running.last_action_started = Some(Instant::now());
        running.last_action_name = Some("ReadFile".into());

        for (ctx, state, needle) in [
            ("idle", idle, "connected · ready when you are"),
            ("thinking", thinking, "x0ne is thinking"),
            ("writing", writing, "x0ne is writing · ~2 tok"),
            ("action", running, "x0ne is running ReadFile"),
        ] {
            let row =
                status_row(&state).unwrap_or_else(|| panic!("{ctx}: no waiting row rendered"));
            assert!(row.contains(needle), "{ctx}: rendered row {row:?}");
        }
    }

    /// When several postures are true at the same time, the screen shows the
    /// most informative one.
    ///
    /// A failure would mean "x0ne is thinking" displayed while a command is
    /// running: the question the operator asks at that moment is "what is
    /// running", and the screen would answer another one.
    #[test]
    fn the_most_informative_phase_wins_the_row() {
        let mut state = resting_state();
        state.idle_status = Some("connected · ready when you are".into());
        state.agent_busy = true;
        state.assistant_buffer = "two words".into();
        state.awaiting_first_token = true;
        state.last_action_started = Some(Instant::now());
        state.last_action_name = Some("ReadFile".into());

        let row = status_row(&state).expect("waiting row");
        assert!(
            row.contains("x0ne is running ReadFile"),
            "rendered: {row:?}"
        );

        state.last_action_started = None;
        let row = status_row(&state).expect("waiting row");
        assert!(row.contains("x0ne is thinking"), "rendered: {row:?}");

        state.awaiting_first_token = false;
        let row = status_row(&state).expect("waiting row");
        assert!(row.contains("x0ne is writing"), "rendered: {row:?}");

        state.agent_busy = false;
        let row = status_row(&state).expect("waiting row");
        assert!(row.contains("connected ·"), "rendered: {row:?}");
    }

    /// The waiting beat has four distinct frames, and it loops.
    ///
    /// A failure would mean a frozen line — the sign an operator reads as
    /// "it is stuck". The four glyphs are required to be **different**: a
    /// counter that always rendered the same frame would pass a test that
    /// settled for "there is a glyph".
    #[test]
    fn the_waiting_glyph_has_four_distinct_frames_and_wraps() {
        let mut state = resting_state();
        state.awaiting_first_token = true;
        let mut frames = Vec::new();
        for phase in [0u32, 4, 8, 12] {
            state.animation_phase = phase;
            let row = status_row(&state).expect("waiting row");
            let glyph = row
                .chars()
                .find(|c| "⋯⋱⋮⋰".contains(*c))
                .unwrap_or_else(|| panic!("phase {phase}: no beat glyph: {row:?}"));
            frames.push(glyph);
        }
        let mut distinct = frames.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 4, "the beat does not beat: {frames:?}");

        // And it loops: phase 16 renders the same frame as phase 0. Without
        // this half, a counter that overflowed would go unnoticed.
        state.animation_phase = 16;
        let row = status_row(&state).expect("waiting row");
        assert!(
            row.contains(frames[0]),
            "phase 16 does not return to the first frame: {row:?}"
        );
        // An intermediate notch changes nothing: the beat is 4 notches
        // (~320 ms), not one notch — otherwise it is a strobe.
        state.animation_phase = 1;
        let row = status_row(&state).expect("waiting row");
        assert!(row.contains(frames[0]), "the beat ran away: {row:?}");
    }
}

#[cfg(test)]
mod geometry {
    use super::*;
    use crate::tui::app::{ConversationLine, LineKind};
    use serde_json::json;

    fn fill_conversation(state: &mut AppState, n: usize) {
        for i in 0..n {
            state.conversation.push(ConversationLine::new(
                LineKind::System,
                format!("line {i:03} a line long enough to be worth wrapping on a narrow pane"),
            ));
        }
    }

    /// An armed-gate state, to measure degenerate sizes **with** a decision
    /// in progress: that is the moment a panic costs the most.
    fn gated_state() -> AppState {
        let mut state = resting_state();
        fill_conversation(&mut state, 40);
        state.pending_confirm = Some(crate::tui::app::ConfirmState {
            id: "q1".into(),
            action_type: "WriteFile".into(),
            action: json!({"type": "WriteFile", "path": "/srv/app/config.toml"}),
            reason: "this action can change state on the host and needs your approval".into(),
            persistent_grant: Some("Allow writes under /srv/app for this session".into()),
            cursor: 1,
        });
        state
    }

    /// A degenerate size does not make the render panic.
    ///
    /// A failure would mean a shrunk terminal kills the session — and a
    /// session killed in the middle of a gate is a gate with no answer, hence
    /// a refusal, hence at best lost work. Twelve sizes, including 1×1, and
    /// each with and without a decision in progress.
    #[test]
    fn a_degenerate_terminal_size_still_renders() {
        const SIZES: &[(u16, u16)] = &[
            (1, 1),
            (2, 2),
            (3, 1),
            (1, 40),
            (20, 5),
            (40, 3),
            (200, 2),
            (10, 10),
            (60, 8),
            (80, 24),
            (120, 30),
            (200, 50),
        ];
        for (w, h) in SIZES {
            for (ctx, state) in [("resting", resting_state()), ("gate", gated_state())] {
                let rows = render_state_to_rows(&state, *w, *h);
                assert_eq!(
                    rows.len(),
                    *h as usize,
                    "{ctx} {w}×{h}: wrong number of lines"
                );
                for (y, row) in rows.iter().enumerate() {
                    assert_eq!(
                        row.chars().count(),
                        *w as usize,
                        "{ctx} {w}×{h}: line {y} does not have the frame's width"
                    );
                }
            }
        }
    }

    /// Scrolling past the bounds is clamped instead of going out.
    ///
    /// A failure would mean an out-of-bounds slice — hence a panic — or an
    /// empty pane: in both cases the mouse wheel becomes a way to lose the
    /// screen. `10_000` and `usize::MAX` are there because an offset is
    /// bounded nowhere before the render (`render.rs:812`).
    #[test]
    fn scrolling_past_either_end_clamps_instead_of_escaping() {
        for offset in [0usize, 1, 5, 10_000, usize::MAX] {
            let mut state = resting_state();
            fill_conversation(&mut state, 60);
            state.auto_stick_bottom = false;
            state.scroll_offset = offset;
            let rows = render_state_to_rows(&state, 120, 24);
            assert_eq!(rows.len(), 24, "offset {offset}: the frame changed size");
        }
    }

    /// Both ends of the document are reachable, and they differ.
    ///
    /// Without this check, a pane that always rendered the same thing would
    /// pass the clamping above with its eyes closed.
    #[test]
    fn both_ends_of_the_conversation_are_reachable_and_differ() {
        let mut state = resting_state();
        fill_conversation(&mut state, 60);
        state.auto_stick_bottom = false;

        state.scroll_offset = 0;
        let bottom = render_state_to_rows(&state, 120, 24);
        state.scroll_offset = usize::MAX;
        let top = render_state_to_rows(&state, 120, 24);

        assert!(
            bottom.iter().any(|r| r.contains("line 059")),
            "the bottom of the document is not reached:\n{}",
            bottom.join("\n")
        );
        assert!(
            top.iter().any(|r| r.contains("line 000")),
            "the top of the document is not reached:\n{}",
            top.join("\n")
        );
        assert_ne!(top, bottom, "both ends render the same image");
    }

    /// Resizing does not lose the conversation.
    ///
    /// The last line written stays readable at every usable size: a failure
    /// would mean a simple resize makes what the agent just said disappear —
    /// the defect photographed on 2026-08-16, in its "width" dimension this
    /// time.
    #[test]
    fn the_last_line_survives_every_usable_size() {
        for (w, h) in [(60u16, 12u16), (80, 24), (100, 16), (120, 30), (200, 50)] {
            let mut state = resting_state();
            fill_conversation(&mut state, 60);
            let rows = render_state_to_rows(&state, w, h);
            assert!(
                rows.iter().any(|r| r.contains("line 059")),
                "{w}×{h}: the last line has disappeared:\n{}",
                rows.join("\n")
            );
        }
    }
}
