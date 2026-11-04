//! X0NE badge — a small LLM-painted canvas living in the header logo
//! area. When the model emits a `<matrix>{...}</matrix>` block inside
//! its response, we parse it into a [`BadgeFrame`] and render it in
//! place of the static X0NE logo. The agent literally takes
//! possession of its own avatar.
//!
//! This is the in-tui cousin of [`crate::matrix_mode`] — but scoped
//! to a fixed 28-column × 3-row slot rather than the whole terminal,
//! and with ratatui colors instead of crossterm's. Interactive
//! features (fields, cursor, input_mode) are intentionally dropped:
//! a badge is a glyph, not a UI.
//!
//! Format (same shape as `<matrix>` in matrix_mode):
//! ```json
//! { "rows": ["█▀█ X0NE", "█▀█ idle"],
//!   "styles": [{"row":0,"col":0,"len":3,"fg":"#5eead4","bold":true}] }
//! ```
//! Missing fields default to "no style on that cell".

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;

/// Width of the badge slot in the tui header (in chars). The LLM
/// is told about this size so it can size its rows accordingly. Rows
/// longer than this are truncated; shorter ones are left-aligned.
pub const BADGE_WIDTH: usize = 28;

/// Height of the badge slot (in rows). Same contract as width — rows
/// past this index are dropped, missing rows are filled blank.
pub const BADGE_HEIGHT: usize = 3;

/// Parsed badge — what the renderer consumes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BadgeFrame {
    pub rows: Vec<String>,
    pub styles: Vec<CellStyle>,
}

/// A multi-frame badge animation. Single-frame badges are a
/// degenerate case (1 frame, delay=0, loop=false). The app-state
/// holder advances `current` on tick when `delay_ms` has elapsed
/// since the last advance. Looping cycles back to frame 0; non-
/// looping stops at the last frame and stays there.
#[derive(Debug, Clone, PartialEq)]
pub struct BadgePlayback {
    pub frames: Vec<BadgeFrame>,
    pub current: usize,
    pub delay_ms: u64,
    pub looping: bool,
    /// Tick count at which we last advanced. Drives the per-frame
    /// timing without needing a wall-clock probe — the tick rate
    /// is fixed at 80ms in mod.rs so we can compute elapsed in
    /// ticks: `(now_tick - last_advance_tick) * 80 ≥ delay_ms`.
    pub last_advance_tick: u32,
}

impl BadgePlayback {
    /// A single-frame badge — no animation, just a static image.
    pub fn single(frame: BadgeFrame) -> Self {
        Self {
            frames: vec![frame],
            current: 0,
            delay_ms: 0,
            looping: false,
            last_advance_tick: 0,
        }
    }

    /// The currently visible frame. Always returns Some unless the
    /// frames vec is empty (which the parser refuses to produce).
    pub fn current_frame(&self) -> Option<&BadgeFrame> {
        self.frames.get(self.current)
    }

    /// Advance the playback one step if enough ticks have elapsed.
    /// Returns true if the frame actually changed (so the renderer
    /// can know if a redraw is meaningful).
    pub fn advance_if_due(&mut self, now_tick: u32) -> bool {
        if self.frames.len() <= 1 || self.delay_ms == 0 {
            return false;
        }
        let tick_rate_ms: u64 = 80; // mirrors run_loop's tick_rate
        let elapsed_ms = (now_tick.wrapping_sub(self.last_advance_tick) as u64) * tick_rate_ms;
        if elapsed_ms < self.delay_ms {
            return false;
        }
        let next = self.current + 1;
        if next >= self.frames.len() {
            if self.looping {
                self.current = 0;
            } else {
                // Stop on the last frame.
                return false;
            }
        } else {
            self.current = next;
        }
        self.last_advance_tick = now_tick;
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellStyle {
    pub row: usize,
    pub col: usize,
    pub len: usize,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
}

#[derive(Deserialize)]
struct RawFrame {
    rows: Vec<String>,
    #[serde(default)]
    styles: Vec<RawStyle>,
}

/// Top-level shape accepted by `parse_badge_payload`. Either a
/// classic single-frame `{rows, styles}` OR a multi-frame
/// `{frames: [...], delay_ms, loop}`. `#[serde(untagged)]` lets
/// serde pick automatically.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPayload {
    Anim {
        frames: Vec<RawFrame>,
        #[serde(default = "default_delay_ms")]
        delay_ms: u64,
        #[serde(default, rename = "loop")]
        looping: bool,
    },
    Single(RawFrame),
}

fn default_delay_ms() -> u64 {
    200
}

#[derive(Deserialize)]
struct RawStyle {
    row: usize,
    col: usize,
    len: usize,
    #[serde(default)]
    fg: Option<String>,
    #[serde(default)]
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    dim: bool,
}

/// Extract the FIRST `<matrix>...</matrix>` block from the LLM's
/// response, if any. Returns the inner JSON string. Mirrors the
/// helper in `matrix_mode::extract_matrix_frame` but tailored to
/// badge needs — we don't carry the preceding prose.
pub fn extract_first_matrix_block(text: &str) -> Option<String> {
    // SECURITY: ASCII-only — `open`/`close` slice `text`, and `text` is the
    // raw model response. `to_lowercase()` does not preserve byte length
    // (ẞ, K U+212A, Å U+212B, İ), so it yields a shifted slice or a
    // char-boundary panic. Same defect as parser.rs, same fix.
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<matrix>")?;
    let close = lower.find("</matrix>")?;
    if close <= open {
        return None;
    }
    Some(text[open + "<matrix>".len()..close].trim().to_string())
}

/// Parse the JSON payload as either a single-frame badge OR an
/// animation. Returns a `BadgePlayback` (single-frame = degenerate
/// playback with 1 frame). `None` on malformed JSON. The caller
/// treats that as "keep the previous badge" — never crash.
pub fn parse_badge_payload(json_str: &str) -> Option<BadgePlayback> {
    let raw: RawPayload = serde_json::from_str(json_str).ok()?;
    match raw {
        RawPayload::Single(frame) => Some(BadgePlayback::single(raw_to_frame(frame))),
        RawPayload::Anim {
            frames,
            delay_ms,
            looping,
        } => {
            if frames.is_empty() {
                return None;
            }
            let parsed: Vec<BadgeFrame> = frames.into_iter().map(raw_to_frame).collect();
            Some(BadgePlayback {
                frames: parsed,
                current: 0,
                delay_ms,
                looping,
                last_advance_tick: 0,
            })
        }
    }
}

/// Back-compat single-frame parser; some callers still want it.
/// Delegates to `parse_badge_payload` and returns the first frame.
pub fn parse_badge_frame(json_str: &str) -> Option<BadgeFrame> {
    parse_badge_payload(json_str).and_then(|p| p.frames.into_iter().next())
}

/// Convert a deserialised `RawFrame` into our internal `BadgeFrame`,
/// applying the same padding/truncation rules as before.
fn raw_to_frame(raw: RawFrame) -> BadgeFrame {
    let mut rows = raw.rows;
    // Clamp to the badge slot. Rows beyond BADGE_HEIGHT are silently
    // dropped — the model is told the size; if it overflows we'd
    // rather show what fits than panic.
    rows.truncate(BADGE_HEIGHT);
    // Truncate too-long rows to BADGE_WIDTH chars (not bytes).
    rows = rows
        .into_iter()
        .map(|r| {
            let mut out: String = r.chars().take(BADGE_WIDTH).collect();
            // Pad to width so styles applied past row.len() still land
            // on a real char position.
            let w = out.chars().count();
            if w < BADGE_WIDTH {
                out.extend(std::iter::repeat_n(' ', BADGE_WIDTH - w));
            }
            out
        })
        .collect();
    while rows.len() < BADGE_HEIGHT {
        rows.push(" ".repeat(BADGE_WIDTH));
    }

    let styles = raw
        .styles
        .into_iter()
        .filter_map(|s| {
            // Keep styles that target valid coordinates. Invalid ones
            // are silently dropped (same forgiveness as matrix_mode).
            if s.row >= BADGE_HEIGHT || s.col >= BADGE_WIDTH || s.len == 0 {
                return None;
            }
            Some(CellStyle {
                row: s.row,
                col: s.col,
                len: s.len.min(BADGE_WIDTH - s.col),
                fg: s.fg.as_deref().and_then(parse_hex_color),
                bg: s.bg.as_deref().and_then(parse_hex_color),
                bold: s.bold,
                dim: s.dim,
            })
        })
        .collect();

    BadgeFrame { rows, styles }
}

/// Convert a `BadgeFrame` to renderable [`Line`]s — one per row, with
/// the styles applied as colored spans. Cells outside any style use
/// the supplied `default_style` so the renderer can dim everything
/// uniformly (e.g. when the agent is idle).
pub fn frame_to_lines(frame: &BadgeFrame, default_style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(BADGE_HEIGHT);
    for (row_idx, row) in frame.rows.iter().enumerate() {
        let chars: Vec<char> = row.chars().collect();
        let mut spans = Vec::<Span<'static>>::new();
        let mut col = 0usize;
        while col < chars.len() {
            // Find the strongest style that covers (row_idx, col).
            // Last style in source order wins — same as matrix_mode.
            let active = frame
                .styles
                .iter()
                .rev()
                .find(|s| s.row == row_idx && col >= s.col && col < s.col + s.len);
            // How far does this style extend? Until either the end of
            // the row, the end of the style range, or the next style
            // boundary — whichever is closest.
            let run_end = if let Some(s) = active {
                (s.col + s.len).min(chars.len())
            } else {
                // Walk forward until we hit a style start or row end.
                let mut next = chars.len();
                for s in &frame.styles {
                    if s.row == row_idx && s.col > col && s.col < next {
                        next = s.col;
                    }
                }
                next
            };
            let slice: String = chars[col..run_end].iter().collect();
            spans.push(Span::styled(slice, style_for(active, default_style)));
            col = run_end;
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn style_for(cell: Option<&CellStyle>, default: Style) -> Style {
    match cell {
        None => default,
        Some(s) => {
            let mut st = default;
            if let Some(fg) = s.fg {
                st = st.fg(fg);
            }
            if let Some(bg) = s.bg {
                st = st.bg(bg);
            }
            if s.bold {
                st = st.add_modifier(Modifier::BOLD);
            }
            if s.dim {
                st = st.add_modifier(Modifier::DIM);
            }
            st
        }
    }
}

/// Parse a hex color like `#5eead4` (with or without alpha) into a
/// ratatui RGB color. Accepts `#rgb` shorthand and `rgb()` / named
/// keywords are rejected (callers feed JSON, not CSS).
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b) = match s.len() {
        3 => (
            u8::from_str_radix(&s[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&s[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&s[2..3].repeat(2), 16).ok()?,
        ),
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    Some(Color::Rgb(r, g, b))
}

/// System prompt fragment that teaches the LLM about the badge slot.
/// The orchestrator concatenates this onto the tui system prompt so
/// the model knows it has a 28×3 canvas to paint with. Kept tiny so
/// it doesn't bloat every turn.
/// Build a badge-frame JSON string from a periodic command's output
/// (the `BadgeWatch` feature). Layout in the 28×3 canvas:
///   row 0 — the label (teal, bold), centered-ish
///   rows 1-2 — the first 2 non-empty output lines, truncated to 28
///
/// If the raw output starts with `{` we assume the command emitted a
/// full badge-frame JSON itself (advanced use — the LLM can write a
/// bash one-liner that prints `{"rows":[...]}`) and pass it through
/// verbatim. Otherwise we wrap the plain text.
pub fn format_watch_frame(label: &str, output: &str) -> String {
    let trimmed = output.trim();
    // Pass-through: command emitted its own frame JSON.
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return trimmed.to_string();
    }
    // Collect up to 2 non-empty output lines.
    let body: Vec<String> = trimmed
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(2)
        .map(|l| {
            // Truncate to BADGE_WIDTH chars (UTF-8 safe).
            l.chars().take(BADGE_WIDTH).collect::<String>()
        })
        .collect();

    let label_row: String = label.chars().take(BADGE_WIDTH).collect();
    let mut rows = vec![label_row];
    rows.extend(body);
    while rows.len() < BADGE_HEIGHT {
        rows.push(String::new());
    }
    rows.truncate(BADGE_HEIGHT);

    // Label gets the teal accent + bold; body stays default.
    let payload = serde_json::json!({
        "rows": rows,
        "styles": [
            {"row": 0, "col": 0, "len": BADGE_WIDTH, "fg": "#5eead4", "bold": true},
        ],
    });
    payload.to_string()
}

pub fn system_prompt_fragment() -> &'static str {
    "\n━━━━ X0NE BADGE (LLM-painted canvas) ━━━━\n\
     You own a tiny 28-col × 3-row canvas in the header — the X0NE badge.\n\
     Two payload shapes:\n\
       Static (default):\n\
         <action>{\"type\":\"PaintBadge\",\"rows\":[...],\"styles\":[...]}</action>\n\
       Animated (multi-frame loop):\n\
         <action>{\"type\":\"PaintBadge\",\
           \"frames\":[{\"rows\":[...],\"styles\":[...]},{...},...],\
           \"delay_ms\":200,\"loop\":true}</action>\n\
     Rows are strings ≤ 28 chars; styles are\n\
     `{row,col,len,fg?,bg?,bold?,dim?}` where colors are hex `#rrggbb`.\n\
     Use animations sparingly for progress/spinners/transitions —\n\
     keep delay_ms ≥ 150 so the eye can follow. `loop:true` cycles\n\
     forever; `false` (default) freezes on the last frame.\n\
     Use the badge to reflect mood / progress / glanceable status.\n\
     ━━━━ LIVE BADGE WIDGET ━━━━\n\
     To make the badge a live dashboard tile, emit BadgeWatch instead:\n\
       <action>{\"type\":\"BadgeWatch\",\"cmd\":\"df -h / | awk 'NR==2{print $5}'\",\
         \"interval_secs\":5,\"label\":\"disk\"}</action>\n\
     The command runs via `bash -c` every interval_secs (any bash: \
     pipes, if, while, functions). Its stdout is painted into the \
     canvas — one setup, the ticker re-runs forever, no prompt cost \
     per tick. Great for `ps`, `df -h`, a download %, a build status. \
     The command may itself print a full badge-frame JSON for full \
     control of colours. Stop with {\"type\":\"BadgeWatchClear\"}.\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_watch_frame_wraps_plain_output() {
        let out = "47%\n/dev/sda1";
        let json = format_watch_frame("disk", out);
        // Must parse back into a valid single-frame badge.
        let pb = parse_badge_payload(&json).expect("valid frame");
        let frame = pb.current_frame().unwrap();
        assert_eq!(frame.rows.len(), BADGE_HEIGHT);
        // Label on row 0, first two output lines after.
        assert!(frame.rows[0].starts_with("disk"));
        assert!(frame.rows[1].contains("47%"));
        assert!(frame.rows[2].contains("/dev/sda1"));
    }

    #[test]
    fn format_watch_frame_truncates_long_lines_to_width() {
        let long = "x".repeat(100);
        let json = format_watch_frame("lbl", &long);
        let pb = parse_badge_payload(&json).unwrap();
        let frame = pb.current_frame().unwrap();
        for row in &frame.rows {
            assert!(
                row.chars().count() <= BADGE_WIDTH,
                "row exceeds badge width: {} chars",
                row.chars().count()
            );
        }
    }

    #[test]
    fn format_watch_frame_passes_through_json_command_output() {
        // If the command itself prints a frame JSON, pass it verbatim
        // so the LLM (or a clever bash one-liner) gets full control.
        let raw = r#"{"rows":["custom","frame","here"]}"#;
        let json = format_watch_frame("ignored", raw);
        assert_eq!(json, raw, "frame-JSON output must pass through unchanged");
    }

    #[test]
    fn format_watch_frame_handles_empty_output() {
        let json = format_watch_frame("idle", "");
        let pb = parse_badge_payload(&json).expect("still valid");
        let frame = pb.current_frame().unwrap();
        assert_eq!(frame.rows.len(), BADGE_HEIGHT);
        assert!(frame.rows[0].starts_with("idle"));
    }

    #[test]
    fn extract_pulls_first_matrix_block() {
        let txt = "noise <matrix>{\"rows\":[\"hi\"]}</matrix> more";
        let inner = extract_first_matrix_block(txt).unwrap();
        assert!(inner.contains("rows"));
    }

    #[test]
    fn extract_returns_none_when_no_block() {
        assert!(extract_first_matrix_block("plain prose").is_none());
        assert!(extract_first_matrix_block("<matrix>").is_none()); // no close
    }

    #[test]
    fn parse_basic_frame_pads_to_size() {
        // A short frame must be padded to BADGE_HEIGHT × BADGE_WIDTH so
        // the renderer never has to guess. Rows below the slot are
        // blank strings; rows above the slot are truncated.
        let f = parse_badge_frame(r#"{"rows":["X0NE"]}"#).unwrap();
        assert_eq!(f.rows.len(), BADGE_HEIGHT);
        for r in &f.rows {
            assert_eq!(r.chars().count(), BADGE_WIDTH, "row not padded: {:?}", r);
        }
    }

    #[test]
    fn parse_drops_invalid_styles_silently() {
        // A style targeting an out-of-range row/col is dropped rather
        // than throwing — same forgiveness as matrix_mode so a sloppy
        // LLM doesn't crash the TUI mid-conversation.
        let raw = json!({
            "rows": ["X0NE"],
            "styles": [
                {"row": 0, "col": 0, "len": 4, "fg": "#5eead4"},
                {"row": 99, "col": 0, "len": 1, "fg": "#ff0000"}, // bad row
                {"row": 0, "col": 99, "len": 1},                   // bad col
                {"row": 0, "col": 0, "len": 0},                    // empty
            ],
        });
        let f = parse_badge_frame(&raw.to_string()).unwrap();
        assert_eq!(f.styles.len(), 1);
        assert_eq!(f.styles[0].fg, Some(Color::Rgb(0x5e, 0xea, 0xd4)));
    }

    #[test]
    fn parse_hex_color_handles_three_and_six_digit_forms() {
        assert_eq!(parse_hex_color("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(
            parse_hex_color("#5eead4"),
            Some(Color::Rgb(0x5e, 0xea, 0xd4))
        );
        assert_eq!(
            parse_hex_color("5eead4"),
            Some(Color::Rgb(0x5e, 0xea, 0xd4))
        );
        assert!(parse_hex_color("not-a-color").is_none());
        assert!(parse_hex_color("#12345").is_none());
    }

    #[test]
    fn frame_to_lines_applies_style_run() {
        // A 4-cell red style on row 0 cols 0..4 must split the line
        // into a styled span (the X0NE) and a default span (the rest).
        let raw = json!({
            "rows": ["X0NE is live"],
            "styles": [{"row": 0, "col": 0, "len": 4, "fg": "#5eead4", "bold": true}],
        });
        let f = parse_badge_frame(&raw.to_string()).unwrap();
        let lines = frame_to_lines(&f, Style::default());
        assert_eq!(lines.len(), BADGE_HEIGHT);

        // Row 0 should have at least 2 spans — one styled, one default.
        let row0 = &lines[0];
        assert!(
            row0.spans.len() >= 2,
            "expected ≥2 spans, got {}",
            row0.spans.len()
        );
        // The first 4 chars are "X0NE" and should be the styled span.
        let first: String = row0.spans[0].content.to_string();
        assert!(
            first.starts_with("X0NE"),
            "first span starts wrong: {:?}",
            first
        );
    }

    #[test]
    fn parse_returns_none_on_bad_json() {
        assert!(parse_badge_frame("not json").is_none());
        // Missing required `rows` field.
        assert!(parse_badge_frame(r#"{"styles":[]}"#).is_none());
    }

    #[test]
    fn system_prompt_fragment_mentions_canvas_size_and_animation() {
        let frag = system_prompt_fragment();
        assert!(frag.contains("28"), "must mention canvas width");
        assert!(frag.contains("3"), "must mention canvas height");
        assert!(frag.contains("PaintBadge"), "must teach the action name");
        assert!(
            frag.contains("frames"),
            "must teach the animation payload shape"
        );
        assert!(frag.contains("delay_ms"), "must teach the frame timing");
        assert!(frag.contains("loop"), "must teach the loop flag");
    }

    #[test]
    fn parse_anim_payload_yields_multi_frame_playback() {
        let payload = serde_json::json!({
            "frames": [
                {"rows": ["frame 1"]},
                {"rows": ["frame 2"]},
                {"rows": ["frame 3"]},
            ],
            "delay_ms": 250,
            "loop": true,
        });
        let pb = parse_badge_payload(&payload.to_string()).expect("anim payload must parse");
        assert_eq!(pb.frames.len(), 3);
        assert_eq!(pb.delay_ms, 250);
        assert!(pb.looping);
        assert_eq!(pb.current, 0);
    }

    #[test]
    fn parse_single_frame_payload_yields_one_frame_playback() {
        // Back-compat: the old shape `{rows, styles}` must still work
        // and yield a degenerate 1-frame playback.
        let payload = serde_json::json!({"rows": ["X0NE"]});
        let pb = parse_badge_payload(&payload.to_string()).expect("single must parse");
        assert_eq!(pb.frames.len(), 1);
        assert_eq!(pb.delay_ms, 0);
        assert!(!pb.looping);
    }

    #[test]
    fn playback_advance_loops_back_to_zero_when_due() {
        // Frames cycle 0→1→2→0→1… when looping. Each advance
        // requires `delay_ms` worth of ticks (80ms per tick).
        let payload = serde_json::json!({
            "frames": [{"rows":["a"]},{"rows":["b"]},{"rows":["c"]}],
            "delay_ms": 160, // 2 ticks
            "loop": true,
        });
        let mut pb = parse_badge_payload(&payload.to_string()).unwrap();
        pb.last_advance_tick = 0;

        // Tick 1: not enough time, stay on 0.
        assert!(!pb.advance_if_due(1));
        assert_eq!(pb.current, 0);

        // Tick 2: 2×80=160ms elapsed, advance to 1.
        assert!(pb.advance_if_due(2));
        assert_eq!(pb.current, 1);

        // Tick 4: advance to 2.
        assert!(pb.advance_if_due(4));
        assert_eq!(pb.current, 2);

        // Tick 6: wrap back to 0 (looping).
        assert!(pb.advance_if_due(6));
        assert_eq!(pb.current, 0);
    }

    #[test]
    fn playback_non_loop_stops_on_last_frame() {
        let payload = serde_json::json!({
            "frames": [{"rows":["a"]},{"rows":["b"]}],
            "delay_ms": 80,
            "loop": false,
        });
        let mut pb = parse_badge_payload(&payload.to_string()).unwrap();
        pb.last_advance_tick = 0;
        // 1 tick = 80 ms → advance to frame 1.
        assert!(pb.advance_if_due(1));
        assert_eq!(pb.current, 1);
        // Already on last frame, NEVER advances again with loop=false.
        assert!(!pb.advance_if_due(2));
        assert!(!pb.advance_if_due(100));
        assert_eq!(pb.current, 1);
    }

    /// The badge extractor slices the raw model response with offsets found on
    /// a lowercased copy — the `parser.rs` defect, third surface. The four
    /// characters whose lowercase form changes byte length shift those offsets.
    ///
    /// A failure here means a badge payload the model never wrote, or a panic
    /// on a char boundary, from one character in the answer.
    #[test]
    fn a_length_changing_character_does_not_shift_the_badge_extractor() {
        for ch in ["\u{1E9E}", "\u{212A}", "\u{212B}", "\u{0130}"] {
            let text = format!("{ch}<matrix>{{\"a\":1}}</matrix>");
            assert_eq!(
                extract_first_matrix_block(&text),
                Some("{\"a\":1}".to_string()),
                "{ch:?}: the badge extractor returned a shifted slice"
            );
        }
    }
}
