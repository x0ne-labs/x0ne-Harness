//! Matrix Mode — LLM-controlled terminal canvas.
//!
//! The LLM outputs `<matrix>...</matrix>` frames containing a character grid
//! with optional per-cell styling. The orchestrator renders them using
//! crossterm raw terminal mode. The LLM is the "GPU".

use std::io::{self, Write};

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

use crate::agent::cancel::CancelToken;
use crate::agent::model::{
    self, ModelError, ModelRequest, ModelResponse, RetryPolicy, TokioSleeper,
};
use crate::api::{ApiResponse, InternalRetry, LlmBackend, UsageTracker};
use crate::bridge;
use crate::config::Config;
use crate::conversation::Conversation;
use crate::session::SessionLogger;

// ═══════════════════════════════════════════════════════════════════════════
//  Frame types
// ═══════════════════════════════════════════════════════════════════════════

/// A single style span applied to a region of the matrix.
#[derive(Debug, Clone)]
struct CellStyle {
    row: usize,
    col: usize,
    len: usize,
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
}

/// An editable text field positioned in the matrix.
#[derive(Debug, Clone)]
struct Field {
    id: String,
    row: usize,
    col: usize,
    len: usize,
    value: String,
    placeholder: String,
}

/// A complete frame produced by the LLM.
#[derive(Debug, Clone)]
struct MatrixFrame {
    rows: Vec<String>,
    styles: Vec<CellStyle>,
    fields: Vec<Field>,
    cursor: Option<(usize, usize)>, // (row, col)
    input_mode: InputMode,
    status: String,
    clear: bool,
    delay_ms: u64, // for multi-frame animations (0 = no delay)
}

#[derive(Debug, Clone, PartialEq)]
enum InputMode {
    Line,
    Char,
    Fields,
    None,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Watch / Crontab system
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
enum WatchResponse {
    AlertLlm,
    UpdateWidget,
    LogOnly,
}

#[derive(Debug, Clone)]
struct WatchTrigger {
    field: String,
    op: String,
    value: f64,
}

/// A scheduled action that runs on a timer with optional triggers.
#[derive(Debug, Clone)]
struct Watch {
    id: String,
    interval_s: u64,
    action: Value,
    trigger: Option<WatchTrigger>,
    on_trigger: WatchResponse,
    #[allow(dead_code)]
    on_tick: WatchResponse,
    last_run: std::time::Instant,
}

fn parse_watch(v: &Value) -> Option<Watch> {
    let id = v.get("id")?.as_str()?.to_string();
    let interval_s = v.get("interval_s").and_then(|i| i.as_u64()).unwrap_or(30);
    let action = v.get("action")?.clone();

    let trigger = v.get("trigger").and_then(|t| {
        Some(WatchTrigger {
            field: t.get("field")?.as_str()?.to_string(),
            op: t.get("op")?.as_str()?.to_string(),
            value: t.get("value")?.as_f64()?,
        })
    });

    let on_trigger = match v.get("on_trigger").and_then(|m| m.as_str()) {
        Some("update_widget") => WatchResponse::UpdateWidget,
        Some("log_only") => WatchResponse::LogOnly,
        _ => WatchResponse::AlertLlm,
    };
    let on_tick = match v.get("on_tick").and_then(|m| m.as_str()) {
        Some("alert_llm") => WatchResponse::AlertLlm,
        Some("log_only") => WatchResponse::LogOnly,
        _ => WatchResponse::UpdateWidget,
    };

    Some(Watch {
        id,
        interval_s,
        action,
        trigger,
        on_trigger,
        on_tick,
        last_run: std::time::Instant::now(),
    })
}

/// Extract `<watch>...</watch>` blocks from LLM output.
fn extract_watch_blocks(text: &str) -> Vec<String> {
    let mut result = vec![];
    // SECURITY: ASCII-only — `open`/`close` slice `text`. See parser.rs:
    // `to_lowercase()` is not byte-length preserving (ẞ, K U+212A, Å U+212B, İ),
    // so a non-ASCII lowercasing here reads the wrong bytes or panics on a
    // char boundary, on the raw model response.
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(open_rel) = lower[search_from..].find("<watch>") {
        let open = search_from + open_rel;
        if let Some(close_rel) = lower[open..].find("</watch>") {
            let close = open + close_rel;
            result.push(text[open + 7..close].trim().to_string());
            search_from = close + 8;
        } else {
            break;
        }
    }
    result
}

/// Evaluate a trigger condition against a JSON result.
fn evaluate_trigger(trigger: &WatchTrigger, result: &Value) -> bool {
    // Navigate dotted field path (e.g. "load_avg" or "cpu.percent")
    let val = trigger
        .field
        .split('.')
        .fold(Some(result), |acc, key| acc.and_then(|v| v.get(key)));
    let actual = match val.and_then(|v| v.as_f64()) {
        Some(f) => f,
        None => return false,
    };
    match trigger.op.as_str() {
        ">" => actual > trigger.value,
        ">=" => actual >= trigger.value,
        "<" => actual < trigger.value,
        "<=" => actual <= trigger.value,
        "==" => (actual - trigger.value).abs() < f64::EPSILON,
        "!=" => (actual - trigger.value).abs() >= f64::EPSILON,
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Parsing
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a hex color like "#00dfa8" to a crossterm Color.
fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb { r, g, b })
}

fn parse_style(v: &Value) -> Option<CellStyle> {
    Some(CellStyle {
        row: v.get("row")?.as_u64()? as usize,
        col: v.get("col")?.as_u64()? as usize,
        len: v.get("len")?.as_u64()? as usize,
        fg: v
            .get("fg")
            .and_then(|c| c.as_str())
            .and_then(parse_hex_color),
        bg: v
            .get("bg")
            .and_then(|c| c.as_str())
            .and_then(parse_hex_color),
        bold: v.get("bold").and_then(|b| b.as_bool()).unwrap_or(false),
        dim: v.get("dim").and_then(|b| b.as_bool()).unwrap_or(false),
    })
}

fn parse_field(v: &Value) -> Option<Field> {
    Some(Field {
        id: v.get("id")?.as_str()?.to_string(),
        row: v.get("row")?.as_u64()? as usize,
        col: v.get("col")?.as_u64()? as usize,
        len: v.get("len")?.as_u64()? as usize,
        value: v
            .get("value")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        placeholder: v
            .get("placeholder")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_frame(v: &Value) -> Result<MatrixFrame> {
    let rows: Vec<String> = v
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if rows.is_empty() {
        anyhow::bail!("MatrixFrame has no rows");
    }

    let styles: Vec<CellStyle> = v
        .get("styles")
        .and_then(|s| s.as_array())
        .map(|arr| arr.iter().filter_map(parse_style).collect())
        .unwrap_or_default();

    let fields: Vec<Field> = v
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(parse_field).collect())
        .unwrap_or_default();

    let cursor = v.get("cursor").and_then(|c| {
        let row = c.get("row")?.as_u64()? as usize;
        let col = c.get("col")?.as_u64()? as usize;
        Some((row, col))
    });

    // Auto-detect input_mode: if fields are present and no explicit mode, use Fields
    let input_mode = match v.get("input_mode").and_then(|m| m.as_str()) {
        Some("char") => InputMode::Char,
        Some("none") => InputMode::None,
        Some("line") => InputMode::Line,
        Some("fields") => InputMode::Fields,
        _ => {
            if !fields.is_empty() {
                InputMode::Fields
            } else {
                InputMode::Line
            }
        }
    };

    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let clear = v.get("clear").and_then(|c| c.as_bool()).unwrap_or(true);
    let delay_ms = v.get("delay_ms").and_then(|d| d.as_u64()).unwrap_or(0);

    // Normalize all rows to the same display width (fixes right-side alignment)
    let max_width = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.as_str()))
        .max()
        .unwrap_or(0);
    let rows: Vec<String> = rows
        .into_iter()
        .map(|r| {
            let w = UnicodeWidthStr::width(r.as_str());
            if w < max_width {
                format!("{}{}", r, " ".repeat(max_width - w))
            } else {
                r
            }
        })
        .collect();

    Ok(MatrixFrame {
        rows,
        styles,
        fields,
        cursor,
        input_mode,
        status,
        clear,
        delay_ms,
    })
}

/// Extract a `<matrix>...</matrix>` block from LLM output.
#[allow(dead_code)]
pub fn extract_matrix_frame(text: &str) -> Option<(String, String)> {
    // SECURITY: ASCII-only — see `extract_watch_blocks`.
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<matrix>")?;
    let close = lower.find("</matrix>")?;
    if close <= open {
        return None;
    }
    let reasoning = text[..open].trim().to_string();
    let json_str = text[open + 8..close].trim().to_string();
    Some((reasoning, json_str))
}

/// Extract ALL `<matrix>...</matrix>` blocks for multi-frame animation.
fn extract_all_matrix_frames(text: &str) -> Vec<(String, String)> {
    let mut result = vec![];
    // SECURITY: ASCII-only — see `extract_watch_blocks`.
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    let mut first_reasoning = true;
    while let Some(open_rel) = lower[search_from..].find("<matrix>") {
        let open = search_from + open_rel;
        if let Some(close_rel) = lower[open..].find("</matrix>") {
            let close = open + close_rel;
            let reasoning = if first_reasoning {
                text[..open].trim().to_string()
            } else {
                String::new()
            };
            first_reasoning = false;
            let json_str = text[open + 8..close].trim().to_string();
            result.push((reasoning, json_str));
            search_from = close + 9;
        } else {
            break;
        }
    }
    result
}

// ═══════════════════════════════════════════════════════════════════════════
//  Rendering
// ═══════════════════════════════════════════════════════════════════════════

fn render_frame(
    stdout: &mut io::Stdout,
    frame: &MatrixFrame,
    prev_frame: Option<&MatrixFrame>,
    term_cols: u16,
    term_rows: u16,
    animate: bool,
    has_bridge: bool,
    action_log: &[String],
    micro_log: &[String],
    total_tokens: u64,
    model_label: &str,
) -> Result<()> {
    if frame.clear && prev_frame.is_none() {
        queue!(stdout, terminal::Clear(ClearType::All))?;
    }

    let frame_height = frame.rows.len();
    let frame_width = frame
        .rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.as_str()))
        .max()
        .unwrap_or(0);

    // Center vertically (leave 2 rows for status bar)
    let available_rows = (term_rows as usize).saturating_sub(2);
    let y_offset = if frame_height < available_rows {
        ((available_rows - frame_height) / 2) as u16
    } else {
        0
    };

    // Center horizontally
    let x_offset = if frame_width < term_cols as usize {
        ((term_cols as usize - frame_width) / 2) as u16
    } else {
        0
    };

    // Build a set of style spans indexed by (row, col)
    // For efficiency, build a row→spans map
    let mut style_map: std::collections::HashMap<usize, Vec<&CellStyle>> =
        std::collections::HashMap::new();
    for s in &frame.styles {
        style_map.entry(s.row).or_default().push(s);
    }

    for (row_idx, row_text) in frame.rows.iter().enumerate() {
        if y_offset as usize + row_idx >= term_rows as usize - 1 {
            break;
        }

        // Frame diff: skip unchanged rows (no styles or fields on them)
        if let Some(prev) = prev_frame {
            let same_text = prev
                .rows
                .get(row_idx)
                .map(|r| r == row_text)
                .unwrap_or(false);
            let has_styles = style_map.contains_key(&row_idx);
            let has_fields = frame.fields.iter().any(|f| f.row == row_idx);
            if same_text && !has_styles && !has_fields && !animate {
                continue;
            }
        }

        queue!(stdout, cursor::MoveTo(x_offset, y_offset + row_idx as u16))?;

        // SECURITY (audit 2026-08-15, finding 4): a canvas row is model-painted
        // and written to the terminal one char at a time with a raw `Print`.
        // Painting with characters is the feature; painting with escape
        // sequences is not. `unicode_width` returns `None` for ESC and the
        // `.unwrap_or(1)` below then counts it as a column, so an escape both
        // reaches the terminal and desynchronises the column accounting that
        // keeps rows aligned — and the approval line of `MatrixConfirmSink`
        // is drawn on this same surface, one row below.
        let chars: Vec<char> = crate::text_safety::sanitize(row_text).chars().collect();
        let row_styles = style_map.get(&row_idx);

        // Track display column (not char index) for proper wide-char alignment
        let mut display_col: usize = 0;
        let max_col = (term_cols as usize).saturating_sub(x_offset as usize);

        for ch in chars.iter() {
            let ch_width = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(1);
            // Stop if we'd exceed terminal width
            if display_col + ch_width > max_col {
                break;
            }

            // Check if any style applies to this display column
            let active_style = row_styles.and_then(|styles| {
                styles
                    .iter()
                    .find(|s| display_col >= s.col && display_col < s.col + s.len)
            });

            if let Some(style) = active_style {
                if let Some(fg) = style.fg {
                    queue!(stdout, SetForegroundColor(fg))?;
                }
                if let Some(bg) = style.bg {
                    queue!(stdout, SetBackgroundColor(bg))?;
                }
                if style.bold {
                    queue!(stdout, SetAttribute(Attribute::Bold))?;
                }
                if style.dim {
                    queue!(stdout, SetAttribute(Attribute::Dim))?;
                }
                queue!(stdout, Print(ch))?;
                queue!(stdout, ResetColor)?;
                queue!(stdout, SetAttribute(Attribute::Reset))?;
            } else {
                // Default: accent color for box chars, dim for others
                if is_box_char(*ch) {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Rgb {
                            r: 0,
                            g: 223,
                            b: 168
                        })
                    )?;
                    queue!(stdout, Print(ch))?;
                    queue!(stdout, ResetColor)?;
                } else {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Rgb {
                            r: 180,
                            g: 190,
                            b: 200
                        })
                    )?;
                    queue!(stdout, Print(ch))?;
                    queue!(stdout, ResetColor)?;
                }
            }
            display_col += ch_width;
        }

        // Clear any leftover chars from previous wider frame
        queue!(stdout, terminal::Clear(ClearType::UntilNewLine))?;

        // Progressive reveal: flush + small delay per row
        if animate {
            stdout.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
    }

    // Render editable fields
    for field in &frame.fields {
        if field.row >= frame.rows.len() {
            continue;
        }
        let fy = y_offset + field.row as u16;
        let fx = x_offset + field.col as u16;
        queue!(stdout, cursor::MoveTo(fx, fy))?;

        // Show value or placeholder
        let display: String = if field.value.is_empty() {
            let ph: String = field.placeholder.chars().take(field.len).collect();
            format!("{:<width$}", ph, width = field.len)
        } else {
            let val: String = field.value.chars().take(field.len).collect();
            format!("{:<width$}", val, width = field.len)
        };

        if field.value.is_empty() {
            // Placeholder: dim with underline
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: 60,
                    g: 70,
                    b: 80
                })
            )?;
        } else {
            // Value: bright
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: 200,
                    g: 210,
                    b: 220
                })
            )?;
        }
        queue!(stdout, SetAttribute(Attribute::Underlined))?;
        queue!(stdout, Print(&display))?;
        queue!(stdout, SetAttribute(Attribute::Reset))?;
        queue!(stdout, ResetColor)?;
    }

    // Action log (fading upward, above status bar)
    draw_action_log_at(stdout, action_log, micro_log, term_cols, term_rows)?;

    // Status bar at bottom
    let status_row = term_rows - 1;
    queue!(stdout, cursor::MoveTo(0, status_row))?;
    queue!(
        stdout,
        SetForegroundColor(Color::Rgb {
            r: 50,
            g: 60,
            b: 70
        })
    )?;
    let status_left = if frame.status.is_empty() {
        " matrix".to_string()
    } else {
        // SECURITY: `status` is model-authored. Sanitized because it shares a
        // surface with the approval line drawn one row away, and cut on a CHAR
        // boundary because `&s[..n]` on a byte index inside a UTF-8 sequence
        // panics — and the model picks the string.
        let clean = crate::text_safety::sanitize(&frame.status);
        let cut = safe_byte_boundary(&clean, term_cols as usize / 2);
        format!(" {}", &clean[..cut])
    };
    let token_label = if total_tokens >= 1000 {
        format!(" {:.1}k tk ", total_tokens as f64 / 1000.0)
    } else if total_tokens > 0 {
        format!(" {} tk ", total_tokens)
    } else {
        String::new()
    };
    let status_right = " F1·help  F3·prompt  F8·log  Esc·quit ";
    let model_tag = if model_label.is_empty() {
        String::new()
    } else {
        format!(" {} ", model_label)
    };
    let bridge_label = if has_bridge {
        " ● x0ne-runtime "
    } else {
        " ○ x0ne-runtime "
    };

    // Build: [status_left] ... [token_label] [model_tag] [bridge_label] [status_right]
    let right_block_width = UnicodeWidthStr::width(token_label.as_str())
        + UnicodeWidthStr::width(model_tag.as_str())
        + UnicodeWidthStr::width(bridge_label)
        + UnicodeWidthStr::width(status_right);
    let left_width = UnicodeWidthStr::width(status_left.as_str());
    let gap = (term_cols as usize).saturating_sub(left_width + right_block_width);
    let status_text = format!("{}{}", status_left, " ".repeat(gap));
    // Truncate by display width, not bytes
    let mut printed = 0usize;
    for ch in status_text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if printed + w > term_cols as usize {
            break;
        }
        queue!(stdout, Print(ch))?;
        printed += w;
    }

    // Token count
    if !token_label.is_empty() {
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: 80,
                g: 80,
                b: 100
            })
        )?;
        queue!(stdout, Print(&token_label))?;
    }

    // Model label
    if !model_tag.is_empty() {
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: 120,
                g: 130,
                b: 160
            })
        )?;
        queue!(stdout, Print(&model_tag))?;
    }

    // Bridge label
    if has_bridge {
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: 0,
                g: 200,
                b: 100
            })
        )?;
    } else {
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: 80,
                g: 50,
                b: 50
            })
        )?;
    }
    queue!(stdout, Print(bridge_label))?;

    // Ctrl+C hint
    queue!(
        stdout,
        SetForegroundColor(Color::Rgb {
            r: 60,
            g: 70,
            b: 80
        })
    )?;
    queue!(stdout, Print(status_right))?;
    queue!(stdout, ResetColor)?;

    // Position cursor for input
    if let Some((cr, cc)) = frame.cursor {
        queue!(
            stdout,
            cursor::MoveTo(x_offset + cc as u16, y_offset + cr as u16)
        )?;
        queue!(stdout, cursor::Show)?;
    } else {
        queue!(stdout, cursor::Hide)?;
    }

    stdout.flush()?;
    Ok(())
}

/// Find the largest byte index `<= max` that sits on a UTF-8 char boundary.
/// Prevents panics when slicing strings containing multi-byte characters
/// (box-drawing like ─/═/╔ are 3 bytes each).
fn safe_byte_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    // Walk backward from max to find a char boundary
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Draw action log entries fading upward from bottom.
/// Most recent at bottom (brightest), older entries above (dimmer).
fn draw_action_log(
    stdout: &mut io::Stdout,
    action_log: &[String],
    micro_log: &[String],
) -> Result<()> {
    let (tc, tr) = terminal::size()?;
    draw_action_log_at(stdout, action_log, micro_log, tc, tr)?;
    stdout.flush()?;
    Ok(())
}

fn draw_action_log_at(
    stdout: &mut io::Stdout,
    action_log: &[String],
    micro_log: &[String],
    term_cols: u16,
    term_rows: u16,
) -> Result<()> {
    let brightness: [u8; 4] = [60, 100, 160, 220];
    let half_w = term_cols as usize / 2;

    // Left: macro log (primitives called)
    if !action_log.is_empty() {
        let count = action_log.len().min(4);
        for i in 0..count {
            let log_idx = action_log.len() - count + i;
            let row = term_rows.saturating_sub(2 + (count - i) as u16);
            let b = brightness[i];
            queue!(stdout, cursor::MoveTo(0, row))?;
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: b / 3,
                    g: b,
                    b: (b as u16 + 30).min(255) as u8
                })
            )?;
            let text = &action_log[log_idx];
            let limit = half_w.saturating_sub(2);
            let end = safe_byte_boundary(text, limit);
            let truncated = &text[..end];
            let padded = format!("  {:<width$}", truncated, width = limit);
            let pad_end = safe_byte_boundary(&padded, half_w);
            queue!(stdout, Print(&padded[..pad_end]))?;
            queue!(stdout, ResetColor)?;
        }
    }

    // Right: micro log (sub-action details)
    if !micro_log.is_empty() {
        let count = micro_log.len().min(4);
        for i in 0..count {
            let log_idx = micro_log.len() - count + i;
            let row = term_rows.saturating_sub(2 + (count - i) as u16);
            let b = brightness[i];
            let right_col = half_w as u16;
            queue!(stdout, cursor::MoveTo(right_col, row))?;
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: b / 4,
                    g: b / 2,
                    b
                })
            )?;
            let text = &micro_log[log_idx];
            let max_len = (term_cols as usize).saturating_sub(half_w + 2);
            let end = safe_byte_boundary(text, max_len);
            let truncated = &text[..end];
            let padded = format!("{:>width$}  ", truncated, width = max_len);
            let pad_end = safe_byte_boundary(&padded, term_cols as usize - half_w);
            queue!(stdout, Print(&padded[..pad_end]))?;
            queue!(stdout, ResetColor)?;
        }
    }

    Ok(())
}

fn is_box_char(c: char) -> bool {
    matches!(
        c,
        '╔' | '═'
            | '╗'
            | '║'
            | '╚'
            | '╝'
            | '╠'
            | '╣'
            | '╬'
            | '─'
            | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '╭'
            | '╮'
            | '╯'
            | '╰'
            | '█'
            | '▓'
            | '▒'
            | '░'
            | '●'
            | '◆'
            | '◇'
            | '▸'
            | '▹'
            | '▾'
            | '▿'
            | '►'
            | '◄'
            | '▲'
            | '▼'
    )
}

/// Play a cinematic entrance animation — depth/floating effect.
///
/// Kept as a thin trampoline to [`crate::intro::play_entrance_animation`]
/// so Matrix Mode and the plain `x0ne` boot path share the exact same
/// pixels. If you want to tweak the animation, edit `intro.rs` — this
/// function only exists to preserve the call-site in `run_matrix_loop`.
#[allow(dead_code)] // kept for the legacy call-site below
fn play_entrance_animation(stdout: &mut io::Stdout) -> Result<()> {
    crate::intro::play_entrance_animation(stdout)
}

// ═══════════════════════════════════════════════════════════════════════════
//  System prompt
// ═══════════════════════════════════════════════════════════════════════════

pub(crate) fn build_matrix_system_prompt(cols: u16, rows: u16, has_bridge: bool) -> String {
    let bridge_section = if has_bridge {
        r#"

X0NE-CORE BRIDGE (CONNECTED):
You have access to real system primitives via <action>...</action> blocks.
CRITICAL RULES:
- NEVER hallucinate or invent file listings, system info, command output, CPU/RAM/disk stats, or any real data.
- ALWAYS use <action> blocks to fetch real data BEFORE displaying it.
- On {"type":"init"}: your FIRST response MUST be an <action> block running RunCommand with `uname -a` (or similar) to get real system data. Then build your welcome screen from that real data.
- Available actions: {"type":"RunCommand","command":"ls","args":["-la","."]}, {"type":"ReadFile","path":"./file"}, {"type":"RunCommand","command":"cmd","args":["arg"]}, {"type":"ListProcesses"}, {"type":"WriteFile","path":"./file","content":"..."}, {"type":"EditFile","path":"./file","edits":[{"old":"old","new":"new"}]}
- Output an <action> block INSTEAD of a <matrix> block when you need data. The result will be sent back, then render it.
- MULTI-ACTION CHAINING: You can output MULTIPLE <action> blocks in a single response. All will be executed and results returned together. Use this to batch operations:
  <action>{"type":"RunCommand","command":"uname","args":["-a"]}</action>
  <action>{"type":"RunCommand","command":"ls","args":["-la","."]}</action>
  This is faster than one action per response. Chain whenever possible.
- If the user asks to see files, list directories, run commands, check system info: USE THE PRIMITIVES. Zero tolerance for fake data.

VERIFICATION HABIT:
- ALWAYS verify your work after any modification. After writing a file, read it back. After running a build, check for errors. After installing a package, confirm it's available.
- When developing code: write it, then test it (compile, run, check output). Show results to the user.
- This is a sandboxed environment — you CAN and SHOULD install packages freely (apt-get, pip, npm, cargo, etc.) when needed. The machine is disposable.
- Chain actions: write code → compile → test → show results. Do not stop at writing.

WATCH / CRONTAB SYSTEM:
You can schedule recurring actions using <watch>...</watch> blocks. These run automatically on a timer.
Format: <watch>{{"id":"sysmon","interval_s":30,"action":{{"type":"ListProcesses"}},"trigger":{{"field":"count","op":">","value":400}},"on_trigger":"alert_llm","on_tick":"update_widget"}}</watch>
- "id": unique name for this watch
- "interval_s": how often to run (seconds)
- "action": any action object (ListProcesses, RunCommand, etc.)
- "trigger": optional condition. "field" supports dotted paths. "op" can be >, >=, <, <=, ==, !=
- "on_trigger": what to do when triggered: "alert_llm" (send result to you), "update_widget", or "log_only"
- "on_tick": what to do on each tick (default: "update_widget")
- To cancel: <watch>{{"id":"sysmon","cancel":true}}</watch>
Use watches for: system monitoring, log tailing, performance alerts, periodic status updates."#
    } else {
        "\n\nNOTE: x0ne-runtime bridge is NOT connected. You are in display-only mode. You cannot execute real commands or read files. Make this clear to the user if they ask for system interactions."
    };

    format!(
        r##"You are controlling a terminal canvas of {cols}×{rows} characters.

OUTPUT FORMAT: Wrap your frame in <matrix>...</matrix> tags containing a JSON object.

Frame JSON fields:
- "rows": REQUIRED. Array of strings, each string is one row of the display.
- "styles": Optional array of style objects: {{"row": 0, "col": 5, "len": 10, "fg": "#00dfa8", "bg": "#1a1a2e", "bold": true}}
- "cursor": Optional {{"row": 9, "col": 5}} — where to show cursor for input.
- "input_mode": "line" (read a line at cursor), "char" (single keypress), "fields" (editable form fields), or "none" (display only). Default: "line". Auto-detects "fields" if fields array is present.
- "fields": Optional array of editable input fields: {{"id": "name", "row": 3, "col": 10, "len": 20, "value": "", "placeholder": "enter name"}}. User presses Enter to submit. You receive: {{"type": "submit", "fields": {{"name": "John"}}}}.
- "status": Optional string shown in the status bar.
- "clear": Optional bool (default true). Set false to overlay without clearing.
- "delay_ms": Optional milliseconds for ANIMATION frames.

USER INPUT as JSON:
- {{"type": "key", "value": "a"}} / {{"type": "line", "value": "hello"}} / {{"type": "click", "row": 5, "col": 12, "row_text": "📁 .config"}} / {{"type": "submit", "fields": {{...}}}} / {{"type": "resize", "cols": 80, "rows": 24}} / {{"type": "init"}}

INTERACTIVITY: Make UIs clickable! File listings navigate on click, menus respond to clicks.

DESIGN:
- ALL rows MUST be EXACT SAME display width (pad with spaces). Emojis = 2 columns.
- Keep ≤ {cols} wide, ≤ {usable_rows} tall. Use Unicode box-drawing (╔═╗║╚╝), blocks (█▓▒░).
- Think brief, then draw. Keep reasoning MINIMAL — 1-2 sentences max.
- PREFER ASCII over emojis for alignment reliability.

TEMPLATES — use these compact patterns, adapt dimensions to {cols}×{rows}:

T1: BORDERED PANEL
{{"rows":["╔══ TITLE ═══════════════════════════════════╗","║  content line 1                             ║","║  content line 2                             ║","╚═════════════════════════════════════════════╝"]}}

T2: TWO-COLUMN
{{"rows":["┌─ LEFT ──────────┬─ RIGHT ─────────────────┐","│ item 1           │ detail 1                 │","│ item 2           │ detail 2                 │","└──────────────────┴──────────────────────────┘"]}}

T3: STATUS BAR
{{"rows":["  cpu 12%  │  mem 4.2G / 16G  │  disk 45%  │  load 1.2  "],"styles":[{{"row":0,"col":0,"len":80,"fg":"#00dfa8"}}]}}

T4: MENU
{{"rows":["  ┌─ MENU ─────────────┐","  │  ▸ Option A          │","  │    Option B          │","  │    Option C          │","  └──────────────────────┘"]}}

T5: LIST WITH ICONS
{{"rows":["  📁 src/","  📁 tests/","  📄 Cargo.toml","  📄 README.md"]}}
{bridge_section}

Start by outputting a welcome frame."##,
        cols = cols,
        rows = rows,
        usable_rows = rows.saturating_sub(2),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  Main loop
// ═══════════════════════════════════════════════════════════════════════════

/// Launch matrix mode with an existing config and user request, skipping
/// the onboarding menus. Used by `/matrix` from the CLI.
pub async fn run_matrix_with_config_and_request(
    mut config: Config,
    user_request: &str,
) -> Result<()> {
    // A failed bridge only makes matrix display-only; refuse it first.
    crate::sandbox_mode::require_sandbox(
        &config.bridge.mode,
        config.bridge.sandbox_cmd.as_deref(),
    )?;
    let llm = crate::llm_setup::setup_llm(&mut config).await?;
    crate::model_keys::announce(&config.llm);
    let mut usage = crate::api::UsageTracker::default();

    let mut bridge: Option<Box<dyn bridge::Bridge>> = bridge::create_bridge(
        &config.bridge.mode,
        &config.bridge.x0ne_core_bin,
        &config.bridge.x0ne_core_addr,
        config.bridge.health_check_timeout_secs,
        config.bridge.sandbox_cmd.as_deref(),
    )
    .await
    .ok();

    let mut session = crate::session::SessionLogger::new().await.ok();

    // Enter raw mode + alternate screen
    terminal::enable_raw_mode().context("Failed to enter raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        cursor::Hide,
        EnableMouseCapture
    )?;

    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            let mut out = io::stdout();
            let _ = execute!(out, DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }
    let _guard = TermGuard;

    let model_label = llm.display_name();
    let result = run_matrix_loop(
        &mut stdout,
        &*llm,
        &mut bridge,
        &mut usage,
        &config,
        &mut session,
        user_request,
        &model_label,
    )
    .await;

    result
}

pub async fn run_matrix_with_config(mut config: Config) -> Result<()> {
    // Before the onboarding prompts, for the reason given just above.
    crate::sandbox_mode::require_sandbox(
        &config.bridge.mode,
        config.bridge.sandbox_cmd.as_deref(),
    )?;
    // ── Matrix Mode onboarding ───────────────────────────────────────
    {
        let dim = "\x1b[2m";
        let bold = "\x1b[1m";
        let _cyan = "\x1b[36m";
        let _green = "\x1b[32m";
        let reset = "\x1b[0m";

        // Header
        println!();
        println!("  {dim}╭─────────────────────────────────────╮{reset}");
        println!("  {dim}│{reset}  {bold}· matrix ·{reset}  {dim}LLM-controlled canvas{reset}  {dim}│{reset}");
        println!("  {dim}╰─────────────────────────────────────╯{reset}");
        println!();
        // Provider/model selection is handled by setup_llm() below
    }

    // ── Interface request ─────────────────────────────────────────────
    let user_request = {
        use std::io::Write;
        let dim = "\x1b[2m";
        let cyan = "\x1b[36m";
        let reset = "\x1b[0m";

        println!("  {dim}what to build{reset}");
        println!();
        println!("  {cyan}1{reset}  free canvas  {dim}(you decide later){reset}");
        println!("  {cyan}2{reset}  dashboard  {dim}(system monitor){reset}");
        println!("  {cyan}3{reset}  file browser");
        println!("  {cyan}4{reset}  game");
        println!("  {cyan}5{reset}  custom…");
        print!("\n  {dim}select{reset} [1] {dim}›{reset} ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        match input.trim() {
            "2" => "Build a system monitoring dashboard showing CPU, memory, disk, and network. Use bar charts and live-updating values.".to_string(),
            "3" => "Build an interactive file browser. Start in the current directory. Show files with icons, sizes, permissions. Arrow keys to navigate, Enter to open.".to_string(),
            "4" => {
                print!("  {dim}which game?{reset} {dim}›{reset} ");
                std::io::stdout().flush()?;
                let mut game = String::new();
                std::io::stdin().read_line(&mut game)?;
                let g = game.trim();
                if g.is_empty() { "Build a fun terminal game of your choice.".into() }
                else { format!("Build a {} game.", g) }
            }
            "5" => {
                print!("  {dim}describe it{reset} {dim}›{reset} ");
                std::io::stdout().flush()?;
                let mut desc = String::new();
                std::io::stdin().read_line(&mut desc)?;
                desc.trim().to_string()
            }
            _ => String::new(), // free canvas
        }
    };

    println!();

    // Set up LLM (interactive — runs before raw mode)
    let llm = crate::llm_setup::setup_llm(&mut config).await?;
    crate::model_keys::announce(&config.llm);
    let mut usage = UsageTracker::new(llm.display_name());

    // Set up bridge (optional — for executing actions)
    let mut bridge: Option<Box<dyn bridge::Bridge>> = match bridge::create_bridge(
        &config.bridge.mode,
        &config.bridge.x0ne_core_bin,
        &config.bridge.x0ne_core_addr,
        config.bridge.health_check_timeout_secs,
        config.bridge.sandbox_cmd.as_deref(),
    )
    .await
    {
        Ok(b) => {
            println!("  \x1b[32m●\x1b[0m x0ne-runtime  \x1b[90mconnected\x1b[0m");
            Some(b)
        }
        Err(e) => {
            println!("  \x1b[31m●\x1b[0m x0ne-runtime  \x1b[90mdisconnected — display only\x1b[0m");
            tracing::debug!("Bridge error: {}", e);
            None
        }
    };

    // Session logger
    let mut session = SessionLogger::new().await.ok();

    // Enter raw mode + alternate screen
    terminal::enable_raw_mode().context("Failed to enter raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        cursor::Hide,
        EnableMouseCapture
    )?;

    // RAII guard: ensures terminal is restored even on panic/abort
    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            let mut out = io::stdout();
            let _ = execute!(out, DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }
    let _guard = TermGuard;

    let model_label = llm.display_name();
    let result = run_matrix_loop(
        &mut stdout,
        &*llm,
        &mut bridge,
        &mut usage,
        &config,
        &mut session,
        &user_request,
        &model_label,
    )
    .await;

    // _guard drops here and restores terminal
    result
}

/// The canvas's confirmation prompt, in the canvas's own idiom.
///
/// The canvas is a raw-mode surface, so the question is rendered as a bottom
/// status line and answered with a single key — but what the answer
/// *authorises* is decided by the canonical `ApprovalAuthority`, exactly as
/// on every other surface. The session-wide option is offered only when the
/// authority supplied a scope, labelled with the authority's own wording.
struct MatrixConfirmSink;

impl crate::display_sink::DisplaySink for MatrixConfirmSink {
    fn emit(&mut self, _event: &crate::tui::wire::AgentEvent) {
        // The canvas renders execution through its own action log; the
        // semantic events need no second rendering here.
    }

    fn request_confirmation(
        &mut self,
        request: &crate::display_sink::ConfirmationRequest<'_>,
    ) -> Option<crate::agent::approval::ConfirmationResponse> {
        use crossterm::event::{Event, KeyCode};
        let mut stdout = io::stdout();
        let (_cols, rows) = terminal::size().ok()?;
        let target = request
            .action
            .get("path")
            .or_else(|| request.action.get("command"))
            .or_else(|| request.action.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let persistent = request.persistent_label();
        let offer = if persistent.is_some() {
            "[y]=once  [s]=session  [n]=deny"
        } else {
            "[y]=once  [n]=deny"
        };
        // A policy question reads differently from an approval: the headline
        // already says which list refused, so the canvas shows that instead of
        // "approve RunCommand ls ?", which would be the wrong question.
        let headline = if request.lift.is_some() {
            format!(
                "⚠ {} — {}",
                crate::text_safety::sanitize(request.reason),
                offer
            )
        } else {
            String::new()
        };
        // SECURITY (audit 2026-08-15, finding 4): the FOURTH `request_confirmation`
        // in the workspace, and the one most exposed — this line is drawn with
        // a raw `Print` onto the bottom row of a canvas the model itself
        // paints (`render_frame`, above). Closing the CLI prompt and the tui
        // modal while leaving this one raw would be the dead-scaffolding
        // pattern of docs/invariants.md in its exact shape: three doors shut, one open,
        // and nothing in the code to say which one an operator is standing at.
        let line = if headline.is_empty() {
            format!(
                "⚠ approve {} {} ? {}",
                crate::text_safety::sanitize(request.action_type),
                crate::text_safety::sanitize(target),
                offer
            )
        } else {
            headline
        };
        let _ = execute!(
            stdout,
            cursor::MoveTo(0, rows.saturating_sub(1)),
            terminal::Clear(terminal::ClearType::CurrentLine),
            SetForegroundColor(Color::Yellow),
            Print(&line),
            ResetColor,
        );
        // Sync single-key read: the canvas loop is not polling events while
        // an action is being dispatched, so the keyboard is ours here.
        let outcome = loop {
            match crossterm::event::read() {
                Ok(Event::Key(k)) => match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        break crate::display_sink::ConfirmOutcome::Yes
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') if persistent.is_some() => {
                        break crate::display_sink::ConfirmOutcome::Always
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        break crate::display_sink::ConfirmOutcome::No
                    }
                    _ => continue,
                },
                Ok(_) => continue,
                // No keyboard — nobody can answer. Fail closed.
                Err(_) => return None,
            }
        };
        let _ = execute!(
            stdout,
            cursor::MoveTo(0, rows.saturating_sub(1)),
            terminal::Clear(terminal::ClearType::CurrentLine),
        );
        Some(crate::agent::approval::ConfirmationResponse {
            id: request.id,
            outcome,
        })
    }
}

/// Run one canvas-initiated action through the canonical boundary.
///
/// The canvas keeps its presentation protocol; its *effects* stop being
/// special. Every action a frame proposes — and every watch re-fire — meets
/// the same gates as everywhere else: human confirmation with scoped
/// authority, routing, and the unknown-name refusal. A watch is thereby
/// re-evaluated at fire time under the policy of that moment, not the one it
/// was armed under.
async fn dispatch_canvas_action(
    config: &Config,
    bridge: &mut Box<dyn bridge::Bridge>,
    authority: &mut crate::agent::approval::ApprovalAuthority,
    undo: &mut crate::agent::undo::UndoJournal,
    action: &Value,
) -> Value {
    let cancel = crate::agent::cancel::CancelToken::new();
    let mut sink = MatrixConfirmSink;
    let mut ctx = crate::agent::nested::ExecCtx {
        undo: Some(undo),
        // The canvas has no session journal; a grant taken here is still
        // applied and still gated, it is only unlogged. Named rather than
        // silent.
        journal: None,
        config,
        bridge: Some(&mut **bridge),
        sink: &mut sink,
        cancel: &cancel,
        // The canvas historically forwarded every name to the runtime; an
        // orchestrator-only name now gets the canonical refusal instead of a
        // confused runtime error. No handler is wired on purpose.
        sysaicalls: None,
        authority,
        discovered_actions: None,
        budget: None,
        history: None,
        frontend: None,
        spawn_factory: None,
    };
    let (_outcome, envelope) = crate::agent::nested::execute_one_action(action, &mut ctx).await;
    envelope
}

async fn run_matrix_loop(
    stdout: &mut io::Stdout,
    llm: &dyn LlmBackend,
    bridge: &mut Option<Box<dyn bridge::Bridge>>,
    usage: &mut UsageTracker,
    _config: &Config,
    session: &mut Option<SessionLogger>,
    user_request: &str,
    model_label: &str,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let sys_prompt = build_matrix_system_prompt(cols, rows, bridge.is_some());
    let mut conv = Conversation::new(32_000).with_summarize_config(60, 30); // trim aggressively for matrix

    // Initial event: tell the LLM what to build
    let init_event = if user_request.is_empty() {
        json!({"type": "init"}).to_string()
    } else {
        json!({"type": "init", "request": user_request}).to_string()
    };
    conv.add_user(&init_event);

    let mut prev_frame: Option<MatrixFrame> = None;
    let mut animate = true; // first frame always animates
    let mut action_log: Vec<String> = Vec::new();
    let mut micro_log: Vec<String> = Vec::new();
    let mut watches: Vec<Watch> = Vec::new();
    // The canvas's approval authority. Session-scoped, like every other
    // surface: an "s" answer covers the identical operation for this canvas
    // session and nothing else.
    let mut authority = crate::agent::approval::ApprovalAuthority::default();
    // `/undo` points of this canvas session.
    let mut undo = crate::agent::undo::UndoJournal::default();
    let mut total_chars = 0usize;
    let mut total_tokens: u64 = 0;
    let mut is_first_call = true;
    let mut frame_retries: u8 = 0;
    const MAX_FRAME_RETRIES: u8 = 5;
    // Bounded attempts with a doubling delay, from the canonical
    // invoker, under `[ui] model_timeout_secs`. Matrix used to sleep 2 s and
    // call again, forever.
    let policy = RetryPolicy {
        deadline: _config.ui.model_deadline(),
        ..RetryPolicy::default()
    };

    loop {
        // ── Stream LLM response with live progress ───────────────────────
        let (cur_cols, cur_rows) = terminal::size()?;
        let messages = conv.messages().to_vec();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::api::StreamChunk>();
        // Ctrl+C and Esc drop this future, its backoff sleeps included.
        let cancel = CancelToken::new();
        let stream_fut = model::invoke(
            llm,
            ModelRequest {
                system: &sys_prompt,
                messages: &messages,
                tools: &[],
            },
            tx,
            &policy,
            &cancel,
            &TokioSleeper,
        );
        tokio::pin!(stream_fut);

        // Play entrance animation during first LLM call (runs concurrently)
        if is_first_call {
            is_first_call = false;
            play_entrance_animation(stdout)?;
        }

        let orbit_chars = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
        let mut spin_idx = 0usize;
        let mut char_count = 0usize;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
        let mut stream_done = false;

        let mut user_quit = false;

        let resp = loop {
            if user_quit {
                break Ok(crate::api::ApiResponse {
                    stop_reason: "user_cancel".to_string(),
                    ..Default::default()
                });
            }

            tokio::select! {
                biased;

                result = &mut stream_fut, if !stream_done => {
                    #[allow(unused_assignments)]
                    { stream_done = true; }
                    break result.map(api_response_of);
                }
                chunk = rx.recv() => {
                    if let Some(c) = chunk {
                        // Both content and thinking deltas count toward
                        // the orbit-spinner activity feedback — for the
                        // user "the model is doing work" is what matters.
                        let len = c.text().len();
                        char_count += len;
                        total_chars += len;
                    }
                }
                _ = ticker.tick() => {
                    // Check for key events (crossterm captures as key events in raw mode)
                    if let Ok(true) = event::poll(std::time::Duration::from_millis(0)) {
                        if let Ok(Event::Key(ke)) = event::read() {
                            if (ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL))
                                || ke.code == KeyCode::Esc
                            {
                                user_quit = true;
                            }
                            // F1: Help overlay — works even during streaming
                            if ke.code == KeyCode::F(1) {
                                let _ = show_help_overlay(stdout, model_label, bridge.is_some(), total_tokens, &action_log).await;
                                // Re-render cached frame after help closes
                                if let Some(ref cached) = prev_frame {
                                    let (tc, tr) = terminal::size()?;
                                    render_frame(stdout, cached, None, tc, tr, false, bridge.is_some(), &action_log, &micro_log, total_tokens, model_label)?;
                                }
                            }
                        }
                    }

                    let (tc, tr) = terminal::size()?;
                    let orb = orbit_chars[spin_idx % orbit_chars.len()];
                    let token_str = if total_tokens >= 1000 {
                        format!("{:.1}k", total_tokens as f64 / 1000.0)
                    } else {
                        format!("{}", total_tokens)
                    };

                    if prev_frame.is_none() {
                        // ── First call: centered spinner ──────────────────────
                        let cx = tc / 2;
                        let cy = tr / 2;

                        let label = if char_count > 0 {
                            format!("{} streaming", orb)
                        } else {
                            format!("{} connecting", orb)
                        };
                        let label_x = cx.saturating_sub(label.len() as u16 / 2);
                        queue!(stdout, cursor::MoveTo(label_x, cy))?;
                        queue!(stdout, SetForegroundColor(Color::Rgb { r: 0, g: 180, b: 160 }))?;
                        queue!(stdout, Print(&label))?;
                        queue!(stdout, terminal::Clear(ClearType::UntilNewLine))?;

                        let stats = format!("{}ch │ {}tk", total_chars, token_str);
                        let stats_x = cx.saturating_sub(stats.len() as u16 / 2);
                        queue!(stdout, cursor::MoveTo(stats_x, cy + 1))?;
                        queue!(stdout, SetForegroundColor(Color::Rgb { r: 50, g: 65, b: 80 }))?;
                        queue!(stdout, Print(&stats))?;
                        queue!(stdout, terminal::Clear(ClearType::UntilNewLine))?;

                        // The model and endpoint this call goes to.
                        let model_line = crate::text_safety::snippet(model_label, tc.saturating_sub(4) as usize);
                        let model_x = cx.saturating_sub(model_line.chars().count() as u16 / 2);
                        queue!(stdout, cursor::MoveTo(model_x, cy + 2))?;
                        queue!(stdout, Print(&model_line))?;
                        queue!(stdout, terminal::Clear(ClearType::UntilNewLine))?;

                        // x0ne-runtime status
                        let core_status = if bridge.is_some() { "● x0ne-runtime" } else { "○ x0ne-runtime" };
                        let core_x = cx.saturating_sub(core_status.len() as u16 / 2);
                        queue!(stdout, cursor::MoveTo(core_x, cy + 3))?;
                        if bridge.is_some() {
                            queue!(stdout, SetForegroundColor(Color::Rgb { r: 0, g: 200, b: 100 }))?;
                        } else {
                            queue!(stdout, SetForegroundColor(Color::Rgb { r: 80, g: 50, b: 50 }))?;
                        }
                        queue!(stdout, Print(core_status))?;
                        queue!(stdout, terminal::Clear(ClearType::UntilNewLine))?;
                    } else {
                        // ── Subsequent calls: bottom status bar ───────────────
                        let status_row = tr - 1;
                        let progress = if char_count > 0 {
                            format!("  {}  streaming  │  {}ch  │  {}tk  │  Ctrl+C", orb, total_chars, token_str)
                        } else {
                            format!("  {}  matrixing  │  {}tk  │  Ctrl+C", orb, token_str)
                        };
                        let padded = format!("{:<width$}", progress, width = tc as usize);
                        queue!(stdout, cursor::MoveTo(0, status_row))?;
                        queue!(stdout, SetForegroundColor(Color::Rgb { r: 60, g: 75, b: 90 }))?;
                        queue!(stdout, Print(&padded[..padded.len().min(tc as usize)]))?;
                    }

                    // Bottom "Ctrl+C" hint (always)
                    if prev_frame.is_none() {
                        let status_row = tr - 1;
                        let bar = "  Ctrl+C to exit".to_string();
                        let bar_padded = format!("{:<width$}", bar, width = tc as usize);
                        queue!(stdout, cursor::MoveTo(0, status_row))?;
                        queue!(stdout, SetForegroundColor(Color::Rgb { r: 40, g: 50, b: 60 }))?;
                        queue!(stdout, Print(&bar_padded[..bar_padded.len().min(tc as usize)]))?;
                    }

                    queue!(stdout, ResetColor)?;
                    stdout.flush()?;
                    spin_idx += 1;
                }
            }
        };

        // Drain any remaining chunks in the channel — count token AND
        // thinking deltas equally toward total_chars (the matrix-mode
        // orbit-spinner just measures activity).
        while let Ok(c) = rx.try_recv() {
            total_chars += c.text().len();
        }

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let note = attempts_note(&e, llm.internal_retry(), &policy);
                show_model_error(stdout, model_label, &e, &note)?;
                // A retry is the operator's call, one key per bounded cycle.
                if !matches!(wait_for_key().await?, KeyInput::Key(k) if k == "r") {
                    anyhow::bail!("matrix stopped on a model error ({note}): {e}");
                }
                if let Some(ref cached) = prev_frame {
                    let (tc, tr) = terminal::size()?;
                    render_frame(
                        stdout,
                        cached,
                        None,
                        tc,
                        tr,
                        false,
                        bridge.is_some(),
                        &action_log,
                        &micro_log,
                        total_tokens,
                        model_label,
                    )?;
                }
                continue;
            }
        };

        // Handle Ctrl+C during streaming
        if resp.stop_reason == "user_cancel" {
            break;
        }

        usage.record(&resp);
        total_tokens += resp.input_tokens + resp.output_tokens;

        // Log
        if let Some(s) = session.as_mut() {
            let _ = s
                .log_llm(
                    &resp.content,
                    resp.input_tokens,
                    resp.output_tokens,
                    resp.cache_creation_input_tokens,
                    resp.cache_read_input_tokens,
                )
                .await;
        }

        conv.add_assistant(&resp.content);

        // Trim conversation: compress old frame JSON to save tokens
        let msgs = conv.messages().to_vec();
        let frame_indices: Vec<usize> = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == crate::conversation::Role::Assistant && m.content.contains("<matrix>")
            })
            .map(|(i, _)| i)
            .collect();
        if frame_indices.len() > 3 {
            let trim_count = frame_indices.len() - 3;
            for &idx in frame_indices.iter().take(trim_count) {
                let msg = &msgs[idx];
                let row_count = msg.content.matches("\"rows\"").count();
                let summary = format!(
                    "[previous frame: ~{} rows, compressed to save tokens]",
                    row_count * 20
                );
                conv.messages_mut()[idx].content = summary;
            }
        }

        // Check for <action> blocks (supports multi-action chaining)
        if let Some(bridge) = bridge.as_mut() {
            let action_blocks = extract_all_action_blocks(&resp.content);
            if !action_blocks.is_empty() {
                let mut all_results = Vec::new();
                for action_json in &action_blocks {
                    if let Ok(action) = serde_json::from_str::<Value>(action_json) {
                        let action_type =
                            action.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                        let detail = action
                            .get("path")
                            .or(action.get("command"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // SECURITY: both halves are model-authored and this
                        // log is drawn on the same surface as the approval
                        // line.
                        let entry = format!(
                            "→ {} {}",
                            crate::text_safety::sanitize(action_type),
                            crate::text_safety::sanitize(detail)
                        );
                        action_log.push(entry);
                        if action_log.len() > 4 {
                            action_log.remove(0);
                        }

                        // Through the canonical boundary: gate → route →
                        // execute → normalised envelope. Never a transport
                        // Err — failures come back as canonical error
                        // envelopes the model can read.
                        {
                            let result = dispatch_canvas_action(
                                _config,
                                bridge,
                                &mut authority,
                                &mut undo,
                                &action,
                            )
                            .await;
                            {
                                // Micro log: extract result summary
                                let o = result.get("output").unwrap_or(&result);
                                let micro_entry = match action_type {
                                    "SystemInfo" => {
                                        let os =
                                            o.get("os").and_then(|v| v.as_str()).unwrap_or("?");
                                        let host = o
                                            .get("hostname")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        format!("← {} @ {}", os, host)
                                    }
                                    "ReadFile" => {
                                        let size = o
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.len())
                                            .unwrap_or(0);
                                        if size >= 1024 {
                                            format!("← {:.1}KB", size as f64 / 1024.0)
                                        } else {
                                            format!("← {}B", size)
                                        }
                                    }
                                    "RunCommand" => {
                                        let ok = result
                                            .get("success")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let out =
                                            o.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                                        let lines = out.lines().count();
                                        format!("← {} {}L", if ok { "✓" } else { "✗" }, lines)
                                    }
                                    "WriteFile" | "SearchReplace" => {
                                        let ok = result
                                            .get("success")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        format!("← {}", if ok { "✓ written" } else { "✗ failed" })
                                    }
                                    _ => "← ok".to_string(),
                                };
                                micro_log.push(micro_entry);
                                if micro_log.len() > 4 {
                                    micro_log.remove(0);
                                }

                                draw_action_log(stdout, &action_log, &micro_log)?;
                                let result_str =
                                    serde_json::to_string_pretty(&result).unwrap_or_default();
                                all_results
                                    .push(format!("[{} result]\n{}", action_type, result_str));
                            }
                        }
                    }
                }
                // Send all results back in one message
                conv.add_user(&all_results.join("\n\n"));
                continue;
            }
        }

        // Extract <watch> blocks (crontab system)
        let watch_blocks = extract_watch_blocks(&resp.content);
        for wb in &watch_blocks {
            if let Ok(v) = serde_json::from_str::<Value>(wb) {
                // Check for "cancel" command
                if v.get("cancel").and_then(|c| c.as_bool()).unwrap_or(false) {
                    if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                        watches.retain(|w| w.id != id);
                        micro_log.push(format!("⊘ watch '{}' cancelled", id));
                        if micro_log.len() > 4 {
                            micro_log.remove(0);
                        }
                    }
                } else if let Some(w) = parse_watch(&v) {
                    // Remove existing watch with same id
                    watches.retain(|existing| existing.id != w.id);
                    let label = format!("⊕ watch '{}' every {}s", w.id, w.interval_s);
                    watches.push(w);
                    micro_log.push(label);
                    if micro_log.len() > 4 {
                        micro_log.remove(0);
                    }
                }
            }
        }

        // Extract matrix frames (supports multi-frame animation)
        let frames_raw = extract_all_matrix_frames(&resp.content);
        let frames: Vec<MatrixFrame> = if !frames_raw.is_empty() {
            let mut parsed = vec![];
            for (reasoning, json_str) in frames_raw {
                match serde_json::from_str::<Value>(&json_str) {
                    Ok(v) => match parse_frame(&v) {
                        Ok(mut f) => {
                            if !reasoning.is_empty() && f.status.is_empty() {
                                f.status = reasoning.chars().take(60).collect();
                            }
                            parsed.push(f);
                        }
                        Err(_) => {
                            break;
                        }
                    },
                    Err(_) => {
                        break;
                    }
                }
            }
            if parsed.is_empty() {
                frame_retries += 1;
                if frame_retries < MAX_FRAME_RETRIES {
                    // Retry: re-display cached frame and ask LLM to fix
                    if let Some(ref cached) = prev_frame {
                        let (tc, tr) = terminal::size()?;
                        render_frame(
                            stdout,
                            cached,
                            None,
                            tc,
                            tr,
                            false,
                            bridge.is_some(),
                            &action_log,
                            &micro_log,
                            total_tokens,
                            model_label,
                        )?;
                    }
                    let err_msg = if frame_retries <= 2 {
                        r#"{"type": "error", "message": "Frame JSON was malformed. The 'rows' array must contain LITERAL strings, NOT template calls like T1() or T2(). Example: {\"rows\":[\"╔══ WELCOME ══════════════════╗\",\"║  Hello World               ║\",\"╚════════════════════════════╝\"]}. Write the actual characters directly."}"#
                    } else {
                        r#"{"type": "error", "message": "CRITICAL: Your frame is STILL invalid. Output ONLY a <matrix> tag containing a JSON object with a 'rows' array of plain strings. NO function calls, NO template references, NO T1/T2/T3. Just literal strings like: <matrix>{\"rows\":[\"  Welcome to X0NE  \",\"  Type a command:  \"]}</matrix>"}"#
                    };
                    conv.add_user(err_msg);
                    continue;
                } else {
                    // Max retries hit — show raw response as text frame
                    frame_retries = 0;
                    let text_lines: Vec<String> = resp
                        .content
                        .lines()
                        .take(rows as usize - 4)
                        .map(String::from)
                        .collect();
                    vec![MatrixFrame {
                        rows: text_lines,
                        styles: vec![],
                        fields: vec![],
                        cursor: None,
                        input_mode: InputMode::Line,
                        status: "parse failed — showing raw".into(),
                        clear: true,
                        delay_ms: 0,
                    }]
                }
            } else {
                frame_retries = 0;
                parsed
            }
        } else if resp.content.to_lowercase().contains("<matrix>") {
            frame_retries += 1;
            if frame_retries < MAX_FRAME_RETRIES {
                if let Some(ref cached) = prev_frame {
                    let (tc, tr) = terminal::size()?;
                    render_frame(
                        stdout,
                        cached,
                        None,
                        tc,
                        tr,
                        false,
                        bridge.is_some(),
                        &action_log,
                        &micro_log,
                        total_tokens,
                        model_label,
                    )?;
                }
                conv.add_user(r#"{"type": "error", "message": "Incomplete matrix frame — please resend the complete <matrix>...</matrix> response."}"#);
                continue;
            } else {
                frame_retries = 0;
                let text_lines: Vec<String> = resp
                    .content
                    .lines()
                    .take(rows as usize - 4)
                    .map(String::from)
                    .collect();
                vec![MatrixFrame {
                    rows: text_lines,
                    styles: vec![],
                    fields: vec![],
                    cursor: None,
                    input_mode: InputMode::Line,
                    status: "parse failed — showing raw".into(),
                    clear: true,
                    delay_ms: 0,
                }]
            }
        } else {
            // No matrix tag — wrap text as a simple frame
            frame_retries = 0;
            let text_lines: Vec<String> = resp
                .content
                .lines()
                .take(rows as usize - 4)
                .map(String::from)
                .collect();
            vec![MatrixFrame {
                rows: text_lines,
                styles: vec![],
                fields: vec![],
                cursor: None,
                input_mode: InputMode::Line,
                status: "text response".into(),
                clear: true,
                delay_ms: 0,
            }]
        };

        // Render all frames (multi-frame animation support)
        let frame_count = frames.len();
        for (fi, frame) in frames.iter().enumerate() {
            let is_last = fi == frame_count - 1;
            let (cur_cols, cur_rows) = terminal::size()?;
            let should_animate = animate && is_last;
            render_frame(
                stdout,
                frame,
                prev_frame.as_ref(),
                cur_cols,
                cur_rows,
                should_animate,
                bridge.is_some(),
                &action_log,
                &micro_log,
                total_tokens,
                model_label,
            )?;
            stdout.flush()?;

            if !is_last && frame.delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(frame.delay_ms));
            }
            prev_frame = Some(frame.clone());
        }

        let frame = frames.last().unwrap().clone();
        animate = false; // subsequent frames use diff, no animation

        // Compute offsets for input positioning
        let frame_width = frame
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .max()
            .unwrap_or(0);
        let x_off = if frame_width < cur_cols as usize {
            ((cur_cols as usize - frame_width) / 2) as u16
        } else {
            0
        };
        let y_off = {
            let fh = frame.rows.len();
            let avail = (cur_rows as usize).saturating_sub(2);
            if fh < avail {
                ((avail - fh) / 2) as u16
            } else {
                0
            }
        };

        // Execute due watches (crontab tick)
        if let Some(bridge) = bridge.as_mut() {
            let mut watch_alerts: Vec<String> = Vec::new();
            for watch in watches.iter_mut() {
                if watch.last_run.elapsed().as_secs() >= watch.interval_s {
                    watch.last_run = std::time::Instant::now();
                    // A watch re-fire is deferred authority becoming an
                    // action *now*: it meets the gate at fire time, under the
                    // policy of this moment — not the one it was armed under.
                    {
                        let result = dispatch_canvas_action(
                            _config,
                            bridge,
                            &mut authority,
                            &mut undo,
                            &watch.action,
                        )
                        .await;
                        {
                            // Check trigger
                            let triggered = watch
                                .trigger
                                .as_ref()
                                .map(|t| evaluate_trigger(t, &result))
                                .unwrap_or(false);

                            if triggered && watch.on_trigger == WatchResponse::AlertLlm {
                                let alert = format!(
                                    "{{\"type\":\"watch_alert\",\"id\":\"{}\",\"data\":{}}}",
                                    watch.id,
                                    serde_json::to_string(&result).unwrap_or_default()
                                );
                                watch_alerts.push(alert);
                                micro_log.push(format!("⚠ {} triggered!", watch.id));
                                if micro_log.len() > 4 {
                                    micro_log.remove(0);
                                }
                            } else {
                                // Normal tick — log result summary
                                let summary = result
                                    .get("hostname")
                                    .and_then(|v| v.as_str())
                                    .map(|_| format!("↻ {} ok", watch.id))
                                    .unwrap_or_else(|| format!("↻ {}", watch.id));
                                micro_log.push(summary);
                                if micro_log.len() > 4 {
                                    micro_log.remove(0);
                                }
                            }

                            draw_action_log(stdout, &action_log, &micro_log)?;
                        }
                    }
                }
            }
            // If any watch triggered an alert, inject it into conversation
            if !watch_alerts.is_empty() {
                conv.add_user(&watch_alerts.join("\n"));
                // Don't wait for input — go straight to LLM
                continue;
            }
        }

        // Capture input based on mode
        let input_event = match frame.input_mode {
            InputMode::Char => {
                match wait_for_key().await? {
                    KeyInput::Quit => break,
                    KeyInput::Resize(c, r) => json!({"type": "resize", "cols": c, "rows": r}),
                    KeyInput::Click(tc, tr) => {
                        let fr = (tr as i32 - y_off as i32).max(0) as u16;
                        let fc = (tc as i32 - x_off as i32).max(0) as u16;
                        // Include the text of the clicked row for context
                        let row_text = frame.rows.get(fr as usize).cloned().unwrap_or_default();
                        json!({"type": "click", "row": fr, "col": fc, "row_text": row_text.trim()})
                    }
                    KeyInput::Key(k) => json!({"type": "key", "value": k}),
                }
            }
            InputMode::Line => {
                let (cr, cc) = frame.cursor.unwrap_or((frame.rows.len(), 2));
                execute!(
                    stdout,
                    cursor::MoveTo(x_off + cc as u16, y_off + cr as u16),
                    cursor::Show
                )?;
                match read_line_raw(stdout).await? {
                    LineInput::Quit => break,
                    LineInput::Resize(c, r) => json!({"type": "resize", "cols": c, "rows": r}),
                    LineInput::Click(tc, tr) => {
                        let fr = (tr as i32 - y_off as i32).max(0) as u16;
                        let fc = (tc as i32 - x_off as i32).max(0) as u16;
                        let row_text = frame.rows.get(fr as usize).cloned().unwrap_or_default();
                        json!({"type": "click", "row": fr, "col": fc, "row_text": row_text.trim()})
                    }
                    LineInput::Text(t) => json!({"type": "line", "value": t}),
                    LineInput::FKey(n) => json!({"type": "key", "value": format!("f{}", n)}),
                }
            }
            InputMode::Fields => match edit_fields(stdout, &frame.fields, x_off, y_off).await? {
                FieldsResult::Quit => break,
                FieldsResult::Resize(c, r) => json!({"type": "resize", "cols": c, "rows": r}),
                FieldsResult::Submit(values) => {
                    let mut obj = serde_json::Map::new();
                    for (id, val) in values {
                        obj.insert(id, Value::String(val));
                    }
                    json!({"type": "submit", "fields": obj})
                }
            },
            InputMode::None => match wait_for_event_or_timeout(2000).await? {
                EventResult::Quit => break,
                EventResult::Resize(c, r) => json!({"type": "resize", "cols": c, "rows": r}),
                EventResult::Click(tc, tr) => {
                    let fr = (tr as i32 - y_off as i32).max(0) as u16;
                    let fc = (tc as i32 - x_off as i32).max(0) as u16;
                    json!({"type": "click", "row": fr, "col": fc})
                }
                EventResult::Key(k) => json!({"type": "key", "value": k}),
                EventResult::Timeout => json!({"type": "tick"}),
            },
        };

        // ── F-key interception (before sending to LLM) ──────────────────
        if let Some(key_val) = input_event.get("value").and_then(|v| v.as_str()) {
            if input_event.get("type").and_then(|t| t.as_str()) == Some("key") {
                match key_val {
                    // F1: Help overlay
                    "f1" => {
                        show_help_overlay(
                            stdout,
                            model_label,
                            bridge.is_some(),
                            total_tokens,
                            &action_log,
                        )
                        .await?;
                        if let Some(ref cached) = prev_frame {
                            let (tc, tr) = terminal::size()?;
                            render_frame(
                                stdout,
                                cached,
                                None,
                                tc,
                                tr,
                                false,
                                bridge.is_some(),
                                &action_log,
                                &micro_log,
                                total_tokens,
                                model_label,
                            )?;
                        }
                        continue;
                    }
                    // F3: Prompt overlay — inject a direct instruction
                    "f3" => {
                        if let Some(prompt) = show_prompt_overlay(stdout).await? {
                            if !prompt.is_empty() {
                                conv.add_user(
                                    &json!({"type": "instruction", "value": prompt}).to_string(),
                                );
                                micro_log.push(format!("▸ {}", &prompt[..prompt.len().min(40)]));
                                if micro_log.len() > 4 {
                                    micro_log.remove(0);
                                }
                                // Re-render frame then continue to LLM
                                if let Some(ref cached) = prev_frame {
                                    let (tc, tr) = terminal::size()?;
                                    render_frame(
                                        stdout,
                                        cached,
                                        None,
                                        tc,
                                        tr,
                                        false,
                                        bridge.is_some(),
                                        &action_log,
                                        &micro_log,
                                        total_tokens,
                                        model_label,
                                    )?;
                                }
                                continue; // go to LLM with the new instruction
                            }
                        }
                        // Cancelled — re-render and wait for input again
                        if let Some(ref cached) = prev_frame {
                            let (tc, tr) = terminal::size()?;
                            render_frame(
                                stdout,
                                cached,
                                None,
                                tc,
                                tr,
                                false,
                                bridge.is_some(),
                                &action_log,
                                &micro_log,
                                total_tokens,
                                model_label,
                            )?;
                        }
                        continue;
                    }
                    // F8: Console overlay — show logs
                    "f8" => {
                        show_console_overlay(stdout, &conv, &action_log, &micro_log).await?;
                        // Re-render frame after closing overlay
                        if let Some(ref cached) = prev_frame {
                            let (tc, tr) = terminal::size()?;
                            render_frame(
                                stdout,
                                cached,
                                None,
                                tc,
                                tr,
                                false,
                                bridge.is_some(),
                                &action_log,
                                &micro_log,
                                total_tokens,
                                model_label,
                            )?;
                        }
                        continue;
                    }
                    // F9: Model info (can't hot-swap yet, show current)
                    "f9" => {
                        micro_log.push(format!("model: {}", model_label));
                        if micro_log.len() > 4 {
                            micro_log.remove(0);
                        }
                        if let Some(ref cached) = prev_frame {
                            let (tc, tr) = terminal::size()?;
                            render_frame(
                                stdout,
                                cached,
                                None,
                                tc,
                                tr,
                                false,
                                bridge.is_some(),
                                &action_log,
                                &micro_log,
                                total_tokens,
                                model_label,
                            )?;
                        }
                        continue;
                    }
                    _ => {}
                }
            }
        }

        let input_str = input_event.to_string();
        if let Some(s) = session.as_mut() {
            let _ = s.log_user(&input_str).await;
        }

        // ── Slash commands (intercepted before sending to LLM) ──────────
        if let Some(line_val) = input_event.get("value").and_then(|v| v.as_str()) {
            if input_event.get("type").and_then(|t| t.as_str()) == Some("line") {
                let cmd = line_val.trim();
                if cmd.starts_with('/') {
                    match cmd {
                        "/quit" | "/exit" | "/q" => break,
                        "/clear" => {
                            conv = Conversation::new(128_000);
                            conv.add_user(&json!({"type": "init"}).to_string());
                            prev_frame = None;
                            action_log.clear();
                            micro_log.clear();
                            watches.clear();
                            total_tokens = 0;
                            micro_log.push("⟳ session cleared".to_string());
                            continue;
                        }
                        "/cost" => {
                            let cost_msg = format!(
                                "Tokens: {} | Estimated: ${:.4}",
                                total_tokens,
                                total_tokens as f64 * 0.000003 // rough avg
                            );
                            micro_log.push(cost_msg);
                            if micro_log.len() > 4 {
                                micro_log.remove(0);
                            }
                            draw_action_log(stdout, &action_log, &micro_log)?;
                            // Re-render current frame to show updated micro log
                            if let Some(ref cached) = prev_frame {
                                let (tc, tr) = terminal::size()?;
                                render_frame(
                                    stdout,
                                    cached,
                                    None,
                                    tc,
                                    tr,
                                    false,
                                    bridge.is_some(),
                                    &action_log,
                                    &micro_log,
                                    total_tokens,
                                    model_label,
                                )?;
                            }
                            continue;
                        }
                        "/help" => {
                            micro_log.push("/clear /cost /help /quit".to_string());
                            if micro_log.len() > 4 {
                                micro_log.remove(0);
                            }
                            draw_action_log(stdout, &action_log, &micro_log)?;
                            if let Some(ref cached) = prev_frame {
                                let (tc, tr) = terminal::size()?;
                                render_frame(
                                    stdout,
                                    cached,
                                    None,
                                    tc,
                                    tr,
                                    false,
                                    bridge.is_some(),
                                    &action_log,
                                    &micro_log,
                                    total_tokens,
                                    model_label,
                                )?;
                            }
                            continue;
                        }
                        "/undo" => {
                            if let Some(bridge) = bridge.as_mut() {
                                let outcome = undo.undo_last(&mut **bridge).await;
                                let line = crate::agent::undo::describe(&outcome);
                                micro_log.push(format!(
                                    "⟲ {}",
                                    line.chars().take(60).collect::<String>()
                                ));
                                if micro_log.len() > 4 {
                                    micro_log.remove(0);
                                }
                                draw_action_log(stdout, &action_log, &micro_log)?;
                            }
                            continue;
                        }
                        _ => {
                            // Unknown slash command — pass to LLM as regular text
                        }
                    }
                }
            }
        }

        conv.add_user(&input_str);
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
//  API helpers
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
//  Input helpers
// ═══════════════════════════════════════════════════════════════════════════

enum KeyInput {
    Key(String),
    Click(u16, u16), // terminal (col, row)
    Resize(u16, u16),
    Quit,
}

enum FieldsResult {
    Submit(Vec<(String, String)>), // (field_id, value)
    Resize(u16, u16),
    Quit,
}

async fn wait_for_key() -> Result<KeyInput> {
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(ke) => {
                    if ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(KeyInput::Quit);
                    }
                    let key_str = match ke.code {
                        KeyCode::Char(c) => c.to_string(),
                        KeyCode::Enter => "enter".to_string(),
                        KeyCode::Backspace => "backspace".to_string(),
                        KeyCode::Esc => "escape".to_string(),
                        KeyCode::Tab => "tab".to_string(),
                        KeyCode::Up => "up".to_string(),
                        KeyCode::Down => "down".to_string(),
                        KeyCode::Left => "left".to_string(),
                        KeyCode::Right => "right".to_string(),
                        KeyCode::Home => "home".to_string(),
                        KeyCode::End => "end".to_string(),
                        KeyCode::Delete => "delete".to_string(),
                        KeyCode::F(n) => format!("f{}", n),
                        _ => continue,
                    };
                    return Ok(KeyInput::Key(key_str));
                }
                Event::Resize(c, r) => return Ok(KeyInput::Resize(c, r)),
                Event::Mouse(me) => {
                    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                        return Ok(KeyInput::Click(me.column, me.row));
                    }
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

enum LineInput {
    Text(String),
    Click(u16, u16),
    Resize(u16, u16),
    FKey(u8),
    Quit,
}

async fn read_line_raw(stdout: &mut io::Stdout) -> Result<LineInput> {
    let mut buf = String::new();

    loop {
        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(ke) => {
                    if ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(LineInput::Quit);
                    }
                    match ke.code {
                        KeyCode::Enter => return Ok(LineInput::Text(buf)),
                        KeyCode::Char(c) => {
                            buf.push(c);
                            queue!(stdout, Print(c))?;
                            stdout.flush()?;
                        }
                        KeyCode::Backspace => {
                            if !buf.is_empty() {
                                buf.pop();
                                queue!(
                                    stdout,
                                    cursor::MoveLeft(1),
                                    Print(' '),
                                    cursor::MoveLeft(1)
                                )?;
                                stdout.flush()?;
                            }
                        }
                        KeyCode::Esc => return Ok(LineInput::Text(String::new())),
                        KeyCode::F(n) => return Ok(LineInput::FKey(n)),
                        _ => {}
                    }
                }
                Event::Resize(c, r) => return Ok(LineInput::Resize(c, r)),
                Event::Mouse(me) => {
                    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                        return Ok(LineInput::Click(me.column, me.row));
                    }
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

/// Edit multiple inline fields. Tab/Shift-Tab to navigate, Enter to submit.
async fn edit_fields(
    stdout: &mut io::Stdout,
    fields: &[Field],
    x_off: u16,
    y_off: u16,
) -> Result<FieldsResult> {
    if fields.is_empty() {
        // No fields — fall back to waiting for a key
        match wait_for_key().await? {
            KeyInput::Quit => return Ok(FieldsResult::Quit),
            KeyInput::Resize(c, r) => return Ok(FieldsResult::Resize(c, r)),
            KeyInput::Click(_, _) => {
                return Ok(FieldsResult::Submit(vec![]));
            }
            KeyInput::Key(_) => {
                return Ok(FieldsResult::Submit(vec![]));
            }
        }
    }

    let mut values: Vec<String> = fields.iter().map(|f| f.value.clone()).collect();
    let mut cursors: Vec<usize> = fields.iter().map(|f| f.value.len()).collect();
    let mut active: usize = 0;

    // Draw initial field highlights
    render_field_highlights(stdout, fields, &values, active, x_off, y_off)?;

    loop {
        // Position cursor in active field
        let f = &fields[active];
        let cx = x_off + f.col as u16 + cursors[active] as u16;
        let cy = y_off + f.row as u16;
        execute!(stdout, cursor::MoveTo(cx, cy), cursor::Show)?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(ke) => {
                    if ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(FieldsResult::Quit);
                    }
                    match ke.code {
                        KeyCode::Enter => {
                            let result: Vec<(String, String)> = fields
                                .iter()
                                .zip(values.iter())
                                .map(|(f, v)| (f.id.clone(), v.clone()))
                                .collect();
                            return Ok(FieldsResult::Submit(result));
                        }
                        KeyCode::Tab => {
                            active = (active + 1) % fields.len();
                            render_field_highlights(stdout, fields, &values, active, x_off, y_off)?;
                        }
                        KeyCode::BackTab => {
                            active = if active == 0 {
                                fields.len() - 1
                            } else {
                                active - 1
                            };
                            render_field_highlights(stdout, fields, &values, active, x_off, y_off)?;
                        }
                        KeyCode::Char(c) => {
                            let f = &fields[active];
                            if values[active].chars().count() < f.len {
                                values[active].insert(cursors[active], c);
                                cursors[active] += 1;
                                render_field_highlights(
                                    stdout, fields, &values, active, x_off, y_off,
                                )?;
                            }
                        }
                        KeyCode::Backspace => {
                            if cursors[active] > 0 {
                                cursors[active] -= 1;
                                values[active].remove(cursors[active]);
                                render_field_highlights(
                                    stdout, fields, &values, active, x_off, y_off,
                                )?;
                            }
                        }
                        KeyCode::Left => {
                            if cursors[active] > 0 {
                                cursors[active] -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if cursors[active] < values[active].len() {
                                cursors[active] += 1;
                            }
                        }
                        KeyCode::Home => {
                            cursors[active] = 0;
                        }
                        KeyCode::End => {
                            cursors[active] = values[active].len();
                        }
                        KeyCode::Esc => {
                            // Submit empty on escape
                            let result: Vec<(String, String)> = fields
                                .iter()
                                .zip(values.iter())
                                .map(|(f, v)| (f.id.clone(), v.clone()))
                                .collect();
                            return Ok(FieldsResult::Submit(result));
                        }
                        _ => {}
                    }
                }
                Event::Resize(c, r) => return Ok(FieldsResult::Resize(c, r)),
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

/// Redraw all fields, highlighting the active one.
fn render_field_highlights(
    stdout: &mut io::Stdout,
    fields: &[Field],
    values: &[String],
    active: usize,
    x_off: u16,
    y_off: u16,
) -> Result<()> {
    for (i, field) in fields.iter().enumerate() {
        let fy = y_off + field.row as u16;
        let fx = x_off + field.col as u16;
        queue!(stdout, cursor::MoveTo(fx, fy))?;

        let val = &values[i];
        let display: String = if val.is_empty() {
            let ph: String = field.placeholder.chars().take(field.len).collect();
            format!("{:<width$}", ph, width = field.len)
        } else {
            let v: String = val.chars().take(field.len).collect();
            format!("{:<width$}", v, width = field.len)
        };

        let is_active = i == active;
        if is_active {
            // Active field: bright underline with accent
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: 220,
                    g: 230,
                    b: 240
                })
            )?;
            queue!(
                stdout,
                SetBackgroundColor(Color::Rgb {
                    r: 25,
                    g: 30,
                    b: 40
                })
            )?;
        } else if val.is_empty() {
            // Inactive empty: dim placeholder
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: 60,
                    g: 70,
                    b: 80
                })
            )?;
        } else {
            // Inactive with value: normal
            queue!(
                stdout,
                SetForegroundColor(Color::Rgb {
                    r: 180,
                    g: 190,
                    b: 200
                })
            )?;
        }
        queue!(stdout, SetAttribute(Attribute::Underlined))?;
        queue!(stdout, Print(&display))?;
        queue!(stdout, SetAttribute(Attribute::Reset))?;
        queue!(stdout, ResetColor)?;
    }
    stdout.flush()?;
    Ok(())
}

enum EventResult {
    Key(String),
    Click(u16, u16),
    Resize(u16, u16),
    Timeout,
    Quit,
}

async fn wait_for_event_or_timeout(ms: u64) -> Result<EventResult> {
    let start = std::time::Instant::now();
    loop {
        let remaining = ms.saturating_sub(start.elapsed().as_millis() as u64);
        if remaining == 0 {
            return Ok(EventResult::Timeout);
        }
        if event::poll(std::time::Duration::from_millis(remaining.min(50)))? {
            match event::read()? {
                Event::Key(ke) => {
                    if ke.code == KeyCode::Char('c') && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(EventResult::Quit);
                    }
                    let key_str = match ke.code {
                        KeyCode::Char(c) => c.to_string(),
                        KeyCode::Enter => "enter".to_string(),
                        KeyCode::Esc => "escape".to_string(),
                        KeyCode::Up => "up".to_string(),
                        KeyCode::Down => "down".to_string(),
                        KeyCode::Left => "left".to_string(),
                        KeyCode::Right => "right".to_string(),
                        KeyCode::F(n) => format!("f{}", n),
                        _ => continue,
                    };
                    return Ok(EventResult::Key(key_str));
                }
                Event::Resize(c, r) => return Ok(EventResult::Resize(c, r)),
                Event::Mouse(me) => {
                    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                        return Ok(EventResult::Click(me.column, me.row));
                    }
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ═══════════════════════════════════════════════════════════════════════════
//  F1: Help overlay — show all shortcuts and session info
// ═══════════════════════════════════════════════════════════════════════════

async fn show_help_overlay(
    stdout: &mut io::Stdout,
    model_label: &str,
    has_bridge: bool,
    total_tokens: u64,
    action_log: &[String],
) -> Result<()> {
    let (_tc, tr) = terminal::size()?;

    queue!(stdout, terminal::Clear(ClearType::All))?;

    let accent = Color::Rgb {
        r: 0,
        g: 180,
        b: 160,
    };
    let header = Color::Rgb {
        r: 220,
        g: 230,
        b: 240,
    };
    let key_fg = Color::Rgb {
        r: 0,
        g: 200,
        b: 220,
    };
    let desc_fg = Color::Rgb {
        r: 120,
        g: 130,
        b: 140,
    };
    let val_fg = Color::Rgb {
        r: 180,
        g: 190,
        b: 200,
    };
    let dim = Color::Rgb {
        r: 60,
        g: 70,
        b: 80,
    };
    let green = Color::Rgb {
        r: 0,
        g: 200,
        b: 100,
    };
    let red = Color::Rgb {
        r: 200,
        g: 60,
        b: 60,
    };

    let mut y = 1u16;

    // Title
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(accent))?;
    queue!(stdout, Print("╭─────────────────────────────────────────╮"))?;
    y += 1;
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, Print("│"))?;
    queue!(stdout, SetForegroundColor(header))?;
    queue!(stdout, Print("  · matrix · help                       "))?;
    queue!(stdout, SetForegroundColor(accent))?;
    queue!(stdout, Print("│"))?;
    y += 1;
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, Print("╰─────────────────────────────────────────╯"))?;
    y += 2;

    // Keyboard shortcuts
    let shortcuts = [
        ("F1", "this help"),
        ("F3", "inject instruction to LLM"),
        ("F8", "view conversation log"),
        ("F9", "show model info"),
        ("Esc", "quit matrix mode"),
        ("Ctrl+C", "quit (also works during streaming)"),
    ];

    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(accent))?;
    queue!(stdout, Print("▸ Keyboard"))?;
    y += 1;
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(dim))?;
    queue!(stdout, Print("─────────────────────────────"))?;
    y += 1;

    for (key, desc) in &shortcuts {
        queue!(stdout, cursor::MoveTo(5, y))?;
        queue!(stdout, SetForegroundColor(key_fg))?;
        queue!(stdout, Print(format!("{:<10}", key)))?;
        queue!(stdout, SetForegroundColor(desc_fg))?;
        queue!(stdout, Print(desc))?;
        y += 1;
    }
    y += 1;

    // Slash commands
    let slash_cmds = [
        ("/quit", "exit matrix"),
        ("/clear", "reset conversation"),
        ("/cost", "token usage"),
        ("/help", "command list"),
        ("/undo", "restore before the agent's last write"),
    ];

    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(accent))?;
    queue!(stdout, Print("▸ Commands (in line input mode)"))?;
    y += 1;
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(dim))?;
    queue!(stdout, Print("─────────────────────────────"))?;
    y += 1;

    for (cmd, desc) in &slash_cmds {
        queue!(stdout, cursor::MoveTo(5, y))?;
        queue!(stdout, SetForegroundColor(key_fg))?;
        queue!(stdout, Print(format!("{:<10}", cmd)))?;
        queue!(stdout, SetForegroundColor(desc_fg))?;
        queue!(stdout, Print(desc))?;
        y += 1;
    }
    y += 1;

    // Session info
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(accent))?;
    queue!(stdout, Print("▸ Session"))?;
    y += 1;
    queue!(stdout, cursor::MoveTo(3, y))?;
    queue!(stdout, SetForegroundColor(dim))?;
    queue!(stdout, Print("─────────────────────────────"))?;
    y += 1;

    let info_rows: Vec<(&str, String, Color)> = vec![
        ("Model", model_label.to_string(), val_fg),
        (
            "Bridge",
            if has_bridge {
                "● connected".to_string()
            } else {
                "○ disconnected".to_string()
            },
            if has_bridge { green } else { red },
        ),
        (
            "Tokens",
            if total_tokens >= 1000 {
                format!("{:.1}k", total_tokens as f64 / 1000.0)
            } else {
                total_tokens.to_string()
            },
            val_fg,
        ),
        ("Actions", action_log.len().to_string(), val_fg),
    ];

    for (label, value, color) in &info_rows {
        queue!(stdout, cursor::MoveTo(5, y))?;
        queue!(stdout, SetForegroundColor(desc_fg))?;
        queue!(stdout, Print(format!("{:<10}", label)))?;
        queue!(stdout, SetForegroundColor(*color))?;
        queue!(stdout, Print(value))?;
        y += 1;
    }

    // Footer
    queue!(stdout, cursor::MoveTo(3, tr - 1))?;
    queue!(stdout, SetForegroundColor(dim))?;
    queue!(stdout, Print("  press any key to close"))?;
    queue!(stdout, ResetColor)?;
    stdout.flush()?;

    // Wait for any key
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(_) = event::read()? {
                break;
            }
        }
        tokio::task::yield_now().await;
    }

    Ok(())
}

//  F3: Prompt overlay — draw an input box over the current frame
// ═══════════════════════════════════════════════════════════════════════════

async fn show_prompt_overlay(stdout: &mut io::Stdout) -> Result<Option<String>> {
    let (tc, tr) = terminal::size()?;
    let box_w = (tc as usize).min(60).max(30);
    let box_x = (tc as usize).saturating_sub(box_w) / 2;
    let box_y = tr / 2 - 2;

    // Draw overlay box
    let border_fg = Color::Rgb {
        r: 0,
        g: 180,
        b: 160,
    };
    let bg = Color::Rgb {
        r: 15,
        g: 20,
        b: 30,
    };
    let dim = Color::Rgb {
        r: 80,
        g: 90,
        b: 100,
    };

    let top = format!(
        "╭─ F3 instruction {}╮",
        "─".repeat(box_w.saturating_sub(20))
    );
    let bot = format!("╰{}╯", "─".repeat(box_w - 2));
    let empty = format!("│{}│", " ".repeat(box_w - 2));
    let hint = format!(
        "│ {} │",
        format!("{:<w$}", "Enter: send · Esc: cancel", w = box_w - 4)
    );

    for dy in 0..6u16 {
        queue!(stdout, cursor::MoveTo(box_x as u16, box_y + dy))?;
        queue!(stdout, SetBackgroundColor(bg))?;
        match dy {
            0 => {
                queue!(stdout, SetForegroundColor(border_fg))?;
                queue!(stdout, Print(&top[..top.len().min(tc as usize)]))?;
            }
            1 => {
                queue!(stdout, SetForegroundColor(border_fg))?;
                queue!(stdout, Print(&empty[..empty.len().min(tc as usize)]))?;
            }
            2 => {
                queue!(stdout, SetForegroundColor(border_fg))?;
                queue!(stdout, Print(&format!("│ {:<w$} │", "▸ ", w = box_w - 4)))?;
            }
            3 => {
                queue!(stdout, SetForegroundColor(border_fg))?;
                queue!(stdout, Print(&empty[..empty.len().min(tc as usize)]))?;
            }
            4 => {
                queue!(stdout, SetForegroundColor(dim))?;
                queue!(stdout, Print(&hint[..hint.len().min(tc as usize)]))?;
            }
            5 => {
                queue!(stdout, SetForegroundColor(border_fg))?;
                queue!(stdout, Print(&bot[..bot.len().min(tc as usize)]))?;
            }
            _ => {}
        }
    }
    queue!(stdout, ResetColor)?;

    // Position cursor for input
    let input_x = box_x as u16 + 4;
    let input_y = box_y + 2;
    let max_input_len = box_w.saturating_sub(6);
    queue!(stdout, cursor::MoveTo(input_x, input_y), cursor::Show)?;
    stdout.flush()?;

    // Read input character by character
    let mut buf = String::new();
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(ke) = event::read()? {
                match ke.code {
                    KeyCode::Enter => {
                        queue!(stdout, cursor::Hide)?;
                        return Ok(Some(buf));
                    }
                    KeyCode::Esc => {
                        queue!(stdout, cursor::Hide)?;
                        return Ok(None);
                    }
                    KeyCode::Char('c') if ke.modifiers.contains(KeyModifiers::CONTROL) => {
                        queue!(stdout, cursor::Hide)?;
                        return Ok(None);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => {
                        if buf.len() < max_input_len {
                            buf.push(c);
                        }
                    }
                    _ => {}
                }
                // Redraw input line
                let display = format!("│ ▸ {:<w$} │", buf, w = max_input_len);
                queue!(stdout, cursor::MoveTo(box_x as u16, input_y))?;
                queue!(
                    stdout,
                    SetForegroundColor(Color::Rgb {
                        r: 220,
                        g: 230,
                        b: 240
                    })
                )?;
                queue!(stdout, SetBackgroundColor(bg))?;
                queue!(stdout, Print(&display[..display.len().min(tc as usize)]))?;
                queue!(stdout, ResetColor)?;
                queue!(stdout, cursor::MoveTo(input_x + buf.len() as u16, input_y))?;
                stdout.flush()?;
            }
        }
        tokio::task::yield_now().await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  F8: Console overlay — show conversation log and actions
// ═══════════════════════════════════════════════════════════════════════════

async fn show_console_overlay(
    stdout: &mut io::Stdout,
    conv: &Conversation,
    action_log: &[String],
    _micro_log: &[String],
) -> Result<()> {
    let (tc, tr) = terminal::size()?;

    // Clear screen
    queue!(stdout, terminal::Clear(ClearType::All))?;

    let border_fg = Color::Rgb {
        r: 60,
        g: 75,
        b: 90,
    };
    let header_fg = Color::Rgb {
        r: 0,
        g: 180,
        b: 160,
    };
    let role_user = Color::Rgb {
        r: 100,
        g: 180,
        b: 255,
    };
    let role_assistant = Color::Rgb {
        r: 180,
        g: 130,
        b: 255,
    };
    let text_fg = Color::Rgb {
        r: 160,
        g: 170,
        b: 180,
    };
    let dim = Color::Rgb {
        r: 80,
        g: 90,
        b: 100,
    };

    // Header
    queue!(stdout, cursor::MoveTo(2, 0))?;
    queue!(stdout, SetForegroundColor(header_fg))?;
    queue!(
        stdout,
        Print("╭─ F8 console ─────────────────────────────╮")
    )?;

    // Conversation messages (last N that fit)
    let msgs = conv.messages();
    let max_lines = (tr as usize).saturating_sub(8);
    let mut lines: Vec<(Color, String)> = Vec::new();

    for msg in msgs.iter() {
        let (color, prefix) = match msg.role {
            crate::conversation::Role::User => (role_user, "▸ user"),
            crate::conversation::Role::Assistant => (role_assistant, "◂ llm"),
        };
        // Truncate content
        let content: String = msg.content.chars().take(tc as usize - 14).collect();
        let content = content.replace('\n', " ↵ ");
        lines.push((color, format!("  {} │ {}", prefix, content)));
    }

    // Show last N lines
    let start = lines.len().saturating_sub(max_lines);
    for (i, (color, line)) in lines[start..].iter().enumerate() {
        let y = 2 + i as u16;
        if y >= tr - 6 {
            break;
        }
        queue!(stdout, cursor::MoveTo(0, y))?;
        queue!(stdout, SetForegroundColor(*color))?;
        let truncated: String = line.chars().take(tc as usize).collect();
        queue!(stdout, Print(&truncated))?;
    }

    // Action log section
    let action_y = tr - 5;
    queue!(stdout, cursor::MoveTo(2, action_y))?;
    queue!(stdout, SetForegroundColor(border_fg))?;
    queue!(stdout, Print("── actions ──"))?;

    for (i, entry) in action_log.iter().enumerate() {
        let y = action_y + 1 + i as u16;
        if y >= tr - 1 {
            break;
        }
        queue!(stdout, cursor::MoveTo(4, y))?;
        queue!(stdout, SetForegroundColor(text_fg))?;
        queue!(stdout, Print(&entry[..entry.len().min(tc as usize - 6)]))?;
    }

    // Footer
    queue!(stdout, cursor::MoveTo(2, tr - 1))?;
    queue!(stdout, SetForegroundColor(dim))?;
    queue!(stdout, Print("  press any key to close"))?;
    queue!(stdout, ResetColor)?;
    stdout.flush()?;

    // Wait for any key
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(_) = event::read()? {
                break;
            }
        }
        tokio::task::yield_now().await;
    }

    Ok(())
}

/// `invoke`'s answer in the shape the canvas loop reads; `usage_of` filled
/// every count, so `unwrap_or(0)` loses nothing.
fn api_response_of(r: ModelResponse) -> ApiResponse {
    ApiResponse {
        content: r.content,
        input_tokens: r.usage.input_tokens.unwrap_or(0),
        output_tokens: r.usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: r.usage.cache_write_tokens.unwrap_or(0),
        cache_read_input_tokens: r.usage.cache_read_tokens.unwrap_or(0),
        stop_reason: r.stop_reason,
    }
}

/// What `invoke` did before giving up, as `invoke` decides it.
fn attempts_note(err: &ModelError, provider: InternalRetry, policy: &RetryPolicy) -> String {
    match err {
        ModelError::RateLimited {
            retry_after: Some(wait),
            ..
        } if *wait > policy.max_backoff => format!(
            "the provider asked to wait {} s, more than {} s: not retried",
            wait.as_secs(),
            policy.max_backoff.as_secs()
        ),
        e if e.is_retryable_after(provider) => {
            format!("gave up after {} attempts", policy.max_attempts)
        }
        e if e.is_retryable() => "not retried here: the provider layer retries it".to_string(),
        _ => "not retried: the same request cannot succeed".to_string(),
    }
}

/// The final model error, full screen: kind, provider text, the model and
/// endpoint called, what was retried, and the two ways out.
fn show_model_error(
    stdout: &mut io::Stdout,
    model_label: &str,
    err: &ModelError,
    note: &str,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = usize::from(cols.saturating_sub(8).max(20));
    let detail = err
        .source()
        .map(|e| crate::text_safety::snippet(&format!("{e:#}"), width * 6))
        .unwrap_or_default();
    let chars: Vec<char> = detail.chars().collect();
    let mut lines: Vec<(String, Color)> =
        vec![(format!("✗ model error · {}", err.kind()), Color::Red)];
    lines.extend(
        chars
            .chunks(width)
            .map(|c| (format!("  {}", c.iter().collect::<String>()), Color::Grey)),
    );
    lines.push((
        format!("  {}", crate::text_safety::snippet(model_label, width)),
        Color::DarkGrey,
    ));
    lines.push((format!("  {note}"), Color::DarkGrey));
    lines.push((String::new(), Color::Reset));
    lines.push((
        "  r  retry   ·   any other key  quit".to_string(),
        Color::Cyan,
    ));
    queue!(stdout, terminal::Clear(ClearType::All))?;
    for (row, (text, color)) in (2..rows).zip(lines) {
        queue!(
            stdout,
            cursor::MoveTo(2, row),
            SetForegroundColor(color),
            Print(text)
        )?;
    }
    queue!(stdout, ResetColor)?;
    stdout.flush()?;
    Ok(())
}

/// Extract `<action>...</action>` block (simplified version for matrix mode).
#[cfg(test)]
fn extract_action_block(text: &str) -> Option<(String, String)> {
    // SECURITY: ASCII-only — see `extract_watch_blocks`.
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<action>")?;
    let close = lower.find("</action>")?;
    if close <= open {
        return None;
    }
    let reasoning = text[..open].trim().to_string();
    let json_str = text[open + 8..close].trim().to_string();
    Some((reasoning, json_str))
}

/// Extract ALL `<action>...</action>` blocks for multi-action chaining.
fn extract_all_action_blocks(text: &str) -> Vec<String> {
    let mut result = vec![];
    // SECURITY: ASCII-only — see `extract_watch_blocks`.
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(open_rel) = lower[search_from..].find("<action>") {
        let open = search_from + open_rel;
        if let Some(close_rel) = lower[open..].find("</action>") {
            let close = open + close_rel;
            let json_str = text[open + 8..close].trim().to_string();
            result.push(json_str);
            search_from = close + 9;
        } else {
            break;
        }
    }
    result
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Matrix calls the model only through the bounded invoker, and its
    /// final error says what was retried, in `invoke`'s own terms.
    #[test]
    fn matrix_calls_the_model_through_the_bounded_invoker() {
        let src = include_str!("matrix_mode.rs");
        let production = &src[..src.find("#[cfg(test)]\nmod tests").expect("tests")];
        assert!(production.contains("model::invoke("));
        assert!(
            !production.contains(".stream(&"),
            "a direct stream call has no attempt bound"
        );
        let policy = RetryPolicy::default();
        let note = |text: &'static str, provider| {
            attempts_note(&model::classify(anyhow::anyhow!(text)), provider, &policy)
        };
        let busy = "OpenAI-compat stream error 503 Service Unavailable: overloaded";
        assert_eq!(note(busy, InternalRetry::None), "gave up after 3 attempts");
        assert!(note(busy, InternalRetry::RateLimitAndServerErrors).starts_with("not retried here"));
        let denied = "OpenAI-compat stream error 401 Unauthorized: bad key";
        assert!(note(denied, InternalRetry::None).starts_with("not retried: "));
    }

    /// Matrix runs display-only when its bridge fails. Under
    /// X0NE_REQUIRE_SANDBOX neither entry starts, before any prompt or model.
    #[test]
    fn both_matrix_entries_check_the_sandbox_requirement_first() {
        let src = include_str!("matrix_mode.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        for entry in [
            "pub async fn run_matrix_with_config_and_request(",
            "pub async fn run_matrix_with_config(",
        ] {
            let body = &production[production.find(entry).expect(entry)..];
            let body = &body[..body.find("\n}\n").expect("end of fn")];
            let gate = body
                .find("require_sandbox(")
                .unwrap_or_else(|| panic!("{entry} never checks X0NE_REQUIRE_SANDBOX"));
            for later in ["setup_llm(", "create_bridge("] {
                let at = body
                    .find(later)
                    .unwrap_or_else(|| panic!("{entry}: no {later}"));
                assert!(gate < at, "{entry}: {later} comes before the check");
            }
            if let Some(at) = body.find("read_line(") {
                assert!(gate < at, "{entry}: a prompt comes before the check");
            }
        }
    }

    // ── Frame Parsing ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_frame_basic() {
        let v = json!({
            "rows": ["╔══════╗", "║ X0NE ║", "╚══════╝"],
            "status": "welcome"
        });
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(frame.status, "welcome");
        assert_eq!(frame.input_mode, InputMode::Line);
        assert!(frame.clear);
        assert_eq!(frame.delay_ms, 0);
    }

    #[test]
    fn test_parse_frame_with_styles() {
        let v = json!({
            "rows": ["Hello World"],
            "styles": [
                {"row": 0, "col": 0, "len": 5, "fg": "#00dfa8", "bold": true},
                {"row": 0, "col": 6, "len": 5, "fg": "#ff5555", "bg": "#1a1a2e"}
            ]
        });
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.styles.len(), 2);
        assert!(frame.styles[0].bold);
        assert_eq!(frame.styles[0].len, 5);
        assert!(frame.styles[1].bg.is_some());
    }

    #[test]
    fn test_parse_frame_with_fields() {
        let v = json!({
            "rows": ["Name:  [          ]", "Age:   [    ]"],
            "fields": [
                {"id": "name", "row": 0, "col": 7, "len": 10, "value": "", "placeholder": "enter name"},
                {"id": "age", "row": 1, "col": 7, "len": 4, "value": "25", "placeholder": ""}
            ]
        });
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.fields.len(), 2);
        assert_eq!(frame.fields[0].id, "name");
        assert_eq!(frame.fields[1].value, "25");
        // Auto-detect Fields input mode
        assert_eq!(frame.input_mode, InputMode::Fields);
    }

    #[test]
    fn test_parse_frame_with_cursor() {
        let v = json!({
            "rows": ["> "],
            "cursor": {"row": 0, "col": 2},
            "input_mode": "line"
        });
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.cursor, Some((0, 2)));
        assert_eq!(frame.input_mode, InputMode::Line);
    }

    #[test]
    fn test_parse_frame_no_rows_fails() {
        let v = json!({"status": "oops"});
        assert!(parse_frame(&v).is_err());
    }

    #[test]
    fn test_parse_frame_empty_rows_fails() {
        let v = json!({"rows": []});
        assert!(parse_frame(&v).is_err());
    }

    #[test]
    fn test_parse_frame_animation_delay() {
        let v = json!({
            "rows": ["frame 1"],
            "delay_ms": 500
        });
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.delay_ms, 500);
    }

    #[test]
    fn test_parse_frame_input_modes() {
        for (mode_str, expected) in [
            ("char", InputMode::Char),
            ("none", InputMode::None),
            ("line", InputMode::Line),
            ("fields", InputMode::Fields),
        ] {
            let v = json!({"rows": ["test"], "input_mode": mode_str});
            let frame = parse_frame(&v).unwrap();
            assert_eq!(
                frame.input_mode, expected,
                "input_mode '{}' failed",
                mode_str
            );
        }
    }

    // ── Row Normalization ─────────────────────────────────────────────────

    #[test]
    fn test_row_width_normalization() {
        let v = json!({
            "rows": ["short", "a longer row here", "mid"]
        });
        let frame = parse_frame(&v).unwrap();
        let widths: Vec<usize> = frame
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .collect();
        // All rows should be the same width (padded to max)
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "Rows have different widths: {:?}",
            widths
        );
    }

    #[test]
    fn test_row_normalization_unicode() {
        let v = json!({
            "rows": ["╔═══╗", "║ A ║", "╚═══╝"]
        });
        let frame = parse_frame(&v).unwrap();
        let widths: Vec<usize> = frame
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "Unicode box rows have different widths: {:?}",
            widths
        );
    }

    // ── Frame Extraction ──────────────────────────────────────────────────

    #[test]
    fn test_extract_matrix_frame_basic() {
        let text = r#"Here is the frame:
<matrix>
{"rows": ["hello"]}
</matrix>"#;
        let result = extract_matrix_frame(text);
        assert!(result.is_some());
        let (reasoning, json_str) = result.unwrap();
        assert!(reasoning.contains("Here is the frame"));
        assert!(json_str.contains("hello"));
    }

    #[test]
    fn test_extract_matrix_frame_case_insensitive() {
        let text = r#"<Matrix>{"rows": ["test"]}</Matrix>"#;
        let result = extract_matrix_frame(text);
        assert!(result.is_some());
    }

    #[test]
    fn test_extract_matrix_frame_missing_close() {
        let text = r#"<matrix>{"rows": ["test"]}"#;
        let result = extract_matrix_frame(text);
        assert!(result.is_none(), "Should return None for missing </matrix>");
    }

    #[test]
    fn test_extract_all_matrix_frames_multi() {
        let text = r#"
<matrix>{"rows": ["frame 1"]}</matrix>
some text
<matrix>{"rows": ["frame 2"]}</matrix>
"#;
        let frames = extract_all_matrix_frames(text);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].1.contains("frame 1"));
        assert!(frames[1].1.contains("frame 2"));
    }

    #[test]
    fn test_extract_all_matrix_frames_empty() {
        let text = "No matrix content here";
        let frames = extract_all_matrix_frames(text);
        assert!(frames.is_empty());
    }

    // ── Action Block Extraction ───────────────────────────────────────────

    #[test]
    fn test_extract_action_block_basic() {
        let text = r#"I'll check the system.
<action>
{"type": "SystemInfo"}
</action>"#;
        let result = extract_action_block(text);
        assert!(result.is_some());
        let (reasoning, json_str) = result.unwrap();
        assert!(reasoning.contains("check the system"));
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["type"], "SystemInfo");
    }

    #[test]
    fn test_extract_action_block_listdir() {
        let text = r#"<action>{"type": "ListDir", "path": "/home"}</action>"#;
        let result = extract_action_block(text);
        assert!(result.is_some());
        let (_, json_str) = result.unwrap();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["type"], "ListDir");
        assert_eq!(parsed["path"], "/home");
    }

    #[test]
    fn test_extract_action_block_run_command() {
        let text = r#"<action>{"type": "RunCommand", "command": "uname", "args": ["-a"]}</action>"#;
        let result = extract_action_block(text);
        assert!(result.is_some());
        let (_, json_str) = result.unwrap();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["type"], "RunCommand");
        assert_eq!(parsed["command"], "uname");
    }

    #[test]
    fn test_extract_action_block_missing() {
        assert!(extract_action_block("no action here").is_none());
    }

    #[test]
    fn test_extract_action_block_no_close() {
        assert!(extract_action_block("<action>{\"type\":\"SystemInfo\"}").is_none());
    }

    #[test]
    fn test_extract_all_action_blocks_multi() {
        let text = r#"I'll get system info and list files.
<action>{"type": "SystemInfo"}</action>
<action>{"type": "ListDir", "path": "."}</action>"#;
        let blocks = extract_all_action_blocks(text);
        assert_eq!(blocks.len(), 2);
        let a1: Value = serde_json::from_str(&blocks[0]).unwrap();
        assert_eq!(a1["type"], "SystemInfo");
        let a2: Value = serde_json::from_str(&blocks[1]).unwrap();
        assert_eq!(a2["type"], "ListDir");
    }

    #[test]
    fn test_extract_all_action_blocks_single() {
        let text = r#"<action>{"type": "RunCommand", "command": "ls"}</action>"#;
        let blocks = extract_all_action_blocks(text);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn test_extract_all_action_blocks_empty() {
        assert!(extract_all_action_blocks("no actions here").is_empty());
    }

    // ── Hex Color Parsing ─────────────────────────────────────────────────

    #[test]
    fn test_parse_hex_color_valid() {
        let c = parse_hex_color("#00dfa8");
        assert!(c.is_some());
        match c.unwrap() {
            Color::Rgb { r, g, b } => {
                assert_eq!(r, 0x00);
                assert_eq!(g, 0xdf);
                assert_eq!(b, 0xa8);
            }
            _ => panic!("Expected RGB color"),
        }
    }

    #[test]
    fn test_parse_hex_color_no_hash() {
        let c = parse_hex_color("ff5555");
        assert!(c.is_some());
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert!(parse_hex_color("not-a-color").is_none());
        assert!(parse_hex_color("#gg").is_none());
    }

    // ── Box Character Detection ───────────────────────────────────────────

    #[test]
    fn test_is_box_char() {
        assert!(is_box_char('╔'));
        assert!(is_box_char('═'));
        assert!(is_box_char('║'));
        assert!(is_box_char('█'));
        assert!(is_box_char('░'));
        assert!(is_box_char('●'));
        assert!(!is_box_char('A'));
        assert!(!is_box_char(' '));
        assert!(!is_box_char('0'));
    }

    // ── System Prompt ─────────────────────────────────────────────────────

    #[test]
    fn test_system_prompt_with_bridge() {
        let prompt = build_matrix_system_prompt(80, 24, true);
        assert!(prompt.contains("80×24"));
        assert!(prompt.contains("X0NE-CORE BRIDGE (CONNECTED)"));
        assert!(prompt.contains("ListProcesses"));
        assert!(prompt.contains("NEVER hallucinate"));
    }

    #[test]
    fn test_system_prompt_without_bridge() {
        let prompt = build_matrix_system_prompt(120, 40, false);
        assert!(prompt.contains("120×40"));
        assert!(prompt.contains("NOT connected"));
        assert!(!prompt.contains("X0NE-CORE BRIDGE (CONNECTED)"));
    }

    // ── Integration: Full LLM Response → Frames ──────────────────────────

    #[test]
    fn test_full_response_to_frames() {
        let llm_response = r##"Let me show you the dashboard.
<matrix>
{
  "rows": [
    "╔════════════════════╗",
    "║   X0NE Dashboard   ║",
    "╚════════════════════╝"
  ],
  "styles": [
    {"row": 1, "col": 4, "len": 14, "fg": "#00dfa8", "bold": true}
  ],
  "status": "ready",
  "cursor": {"row": 1, "col": 4}

}
</matrix>"##;

        let frames = extract_all_matrix_frames(llm_response);
        assert_eq!(frames.len(), 1);

        let (reasoning, json_str) = &frames[0];
        assert!(reasoning.contains("dashboard"));

        let v: Value = serde_json::from_str(json_str).unwrap();
        let frame = parse_frame(&v).unwrap();
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(frame.styles.len(), 1);
        assert_eq!(frame.status, "ready");
        assert_eq!(frame.cursor, Some((1, 4)));

        // Verify row normalization
        let widths: Vec<usize> = frame
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.as_str()))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]));
    }

    #[test]
    fn test_action_then_matrix_response() {
        // Simulate: LLM first calls action, then renders frame
        let action_response = r#"I'll get the system info first.
<action>{"type": "SystemInfo"}</action>"#;

        let action = extract_action_block(action_response);
        assert!(action.is_some());
        let matrix_frame = extract_matrix_frame(action_response);
        assert!(
            matrix_frame.is_none(),
            "Action response should NOT have a matrix frame"
        );

        // Then LLM sends the frame
        let frame_response =
            r#"<matrix>{"rows": ["CPU: 45%", "MEM: 2.1G"], "status": "system"}</matrix>"#;
        let frame = extract_matrix_frame(frame_response);
        assert!(frame.is_some());
        let action2 = extract_action_block(frame_response);
        assert!(
            action2.is_none(),
            "Frame response should NOT have an action block"
        );
    }

    #[test]
    fn test_malformed_json_recovery() {
        let bad_response = r#"<matrix>
{
  "rows": ["line 1",
  "line 2"
  -- OOPS INVALID JSON --
}
</matrix>"#;
        let frames = extract_all_matrix_frames(bad_response);
        assert_eq!(frames.len(), 1); // extraction succeeds (it's text between tags)
        let (_, json_str) = &frames[0];
        // But JSON parsing should fail
        let result = serde_json::from_str::<Value>(json_str);
        assert!(result.is_err(), "Malformed JSON should fail to parse");
    }

    #[test]
    fn test_multi_frame_animation() {
        let response = r#"
<matrix>{"rows": ["Frame 1"], "delay_ms": 200}</matrix>
<matrix>{"rows": ["Frame 2"], "delay_ms": 200}</matrix>
<matrix>{"rows": ["Frame 3"], "delay_ms": 0}</matrix>
"#;
        let frames_raw = extract_all_matrix_frames(response);
        assert_eq!(frames_raw.len(), 3);

        for (_, json_str) in &frames_raw {
            let v: Value = serde_json::from_str(json_str).unwrap();
            let frame = parse_frame(&v).unwrap();
            assert_eq!(frame.rows.len(), 1);
        }
    }

    // ── Watch / Crontab System ────────────────────────────────────────────

    #[test]
    fn test_parse_watch_basic() {
        let v = json!({
            "id": "sysmon",
            "interval_s": 30,
            "action": {"type": "SystemInfo"}
        });
        let watch = parse_watch(&v).unwrap();
        assert_eq!(watch.id, "sysmon");
        assert_eq!(watch.interval_s, 30);
        assert!(watch.trigger.is_none());
        assert_eq!(watch.on_tick, WatchResponse::UpdateWidget);
        assert_eq!(watch.on_trigger, WatchResponse::AlertLlm);
    }

    #[test]
    fn test_parse_watch_with_trigger() {
        let v = json!({
            "id": "loadmon",
            "interval_s": 10,
            "action": {"type": "SystemInfo"},
            "trigger": {"field": "load_avg", "op": ">", "value": 5.0},
            "on_trigger": "alert_llm",
            "on_tick": "log_only"
        });
        let watch = parse_watch(&v).unwrap();
        assert!(watch.trigger.is_some());
        let t = watch.trigger.unwrap();
        assert_eq!(t.field, "load_avg");
        assert_eq!(t.op, ">");
        assert_eq!(t.value, 5.0);
        assert_eq!(watch.on_trigger, WatchResponse::AlertLlm);
        assert_eq!(watch.on_tick, WatchResponse::LogOnly);
    }

    #[test]
    fn test_parse_watch_missing_id() {
        let v = json!({"interval_s": 30, "action": {"type": "SystemInfo"}});
        assert!(parse_watch(&v).is_none());
    }

    #[test]
    fn test_parse_watch_missing_action() {
        let v = json!({"id": "test", "interval_s": 30});
        assert!(parse_watch(&v).is_none());
    }

    #[test]
    fn test_parse_watch_default_interval() {
        let v = json!({"id": "test", "action": {"type": "SystemInfo"}});
        let watch = parse_watch(&v).unwrap();
        assert_eq!(watch.interval_s, 30); // default
    }

    #[test]
    fn test_extract_watch_blocks() {
        let text = r#"I'll monitor the system.
<watch>{"id":"sysmon","interval_s":10,"action":{"type":"SystemInfo"}}</watch>"#;
        let blocks = extract_watch_blocks(text);
        assert_eq!(blocks.len(), 1);
        let v: Value = serde_json::from_str(&blocks[0]).unwrap();
        assert_eq!(v["id"], "sysmon");
    }

    #[test]
    fn test_extract_watch_blocks_multi() {
        let text = r#"
<watch>{"id":"sys","interval_s":30,"action":{"type":"SystemInfo"}}</watch>
<watch>{"id":"disk","interval_s":60,"action":{"type":"RunCommand","command":"df","args":["-h"]}}</watch>
"#;
        let blocks = extract_watch_blocks(text);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn test_extract_watch_blocks_empty() {
        assert!(extract_watch_blocks("no watches").is_empty());
    }

    #[test]
    fn test_extract_watch_cancel() {
        let text = r#"<watch>{"id":"sysmon","cancel":true}</watch>"#;
        let blocks = extract_watch_blocks(text);
        assert_eq!(blocks.len(), 1);
        let v: Value = serde_json::from_str(&blocks[0]).unwrap();
        assert_eq!(v["cancel"], true);
    }

    // ── Trigger Evaluation ────────────────────────────────────────────────

    #[test]
    fn test_evaluate_trigger_greater_than() {
        let trigger = WatchTrigger {
            field: "load_avg".into(),
            op: ">".into(),
            value: 5.0,
        };
        let result_high = json!({"load_avg": 9.5});
        let result_low = json!({"load_avg": 2.0});
        assert!(evaluate_trigger(&trigger, &result_high));
        assert!(!evaluate_trigger(&trigger, &result_low));
    }

    #[test]
    fn test_evaluate_trigger_less_than() {
        let trigger = WatchTrigger {
            field: "disk_free_gb".into(),
            op: "<".into(),
            value: 1.0,
        };
        let result = json!({"disk_free_gb": 0.5});
        assert!(evaluate_trigger(&trigger, &result));
    }

    #[test]
    fn test_evaluate_trigger_equal() {
        let trigger = WatchTrigger {
            field: "status".into(),
            op: "==".into(),
            value: 0.0,
        };
        let result = json!({"status": 0});
        assert!(evaluate_trigger(&trigger, &result));
    }

    #[test]
    fn test_evaluate_trigger_dotted_path() {
        let trigger = WatchTrigger {
            field: "cpu.percent".into(),
            op: ">=".into(),
            value: 90.0,
        };
        let result = json!({"cpu": {"percent": 95.5}});
        assert!(evaluate_trigger(&trigger, &result));
        let result_low = json!({"cpu": {"percent": 50.0}});
        assert!(!evaluate_trigger(&trigger, &result_low));
    }

    #[test]
    fn test_evaluate_trigger_missing_field() {
        let trigger = WatchTrigger {
            field: "nonexistent".into(),
            op: ">".into(),
            value: 0.0,
        };
        let result = json!({"other": 42});
        assert!(!evaluate_trigger(&trigger, &result));
    }

    #[test]
    fn test_evaluate_trigger_not_equal() {
        let trigger = WatchTrigger {
            field: "errors".into(),
            op: "!=".into(),
            value: 0.0,
        };
        let result = json!({"errors": 3});
        assert!(evaluate_trigger(&trigger, &result));
        let result_zero = json!({"errors": 0});
        assert!(!evaluate_trigger(&trigger, &result_zero));
    }

    // ── Watch + Action Integration ────────────────────────────────────────

    #[test]
    fn test_response_with_watch_and_matrix() {
        let response = r#"I'll set up monitoring and show the dashboard.
<watch>{"id":"sysmon","interval_s":10,"action":{"type":"SystemInfo"}}</watch>
<matrix>{"rows": ["Dashboard line 1"], "status": "monitoring"}</matrix>"#;

        // Watch extraction
        let watches = extract_watch_blocks(response);
        assert_eq!(watches.len(), 1);

        // Matrix extraction still works
        let frames = extract_all_matrix_frames(response);
        assert_eq!(frames.len(), 1);

        // No interference
        let actions = extract_all_action_blocks(response);
        assert!(actions.is_empty());
    }
}

#[cfg(test)]
mod canvas_tests {
    /// The canvas keeps its presentation protocol; its effects stopped being
    /// special. `/undo` restores the canvas's own undo points (`agent::undo`),
    /// human-typed at the canvas's own keyboard — the same precedent as the
    /// TUI's human-armed badge watch. Model-emitted actions and watch
    /// re-fires go through the canonical boundary.
    #[test]
    fn guard_canvas_effects_use_the_canonical_boundary() {
        let src = include_str!("matrix_mode.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            production.matches("bridge.send_action(").count(),
            0,
            "a canvas effect path bypasses the canonical boundary (/undo goes \
             through agent::undo, never a raw RunCommand)"
        );
        assert!(production.contains("execute_one_action(action, &mut ctx)"));
        assert!(
            production.contains("ApprovalAuthority::default()"),
            "the canvas session lost its authority"
        );
    }

    /// The frame protocol survives conversion untouched: the parser still
    /// recognises the canvas vocabulary this product concept is made of.
    #[test]
    fn guard_the_frame_protocol_is_intact() {
        let src = include_str!("matrix_mode.rs");
        for token in [
            "extract_matrix_frame",
            "parse_frame",
            "parse_field",
            "parse_style",
            "parse_watch",
            "play_entrance_animation",
            "input_mode",
        ] {
            assert!(
                src.contains(token),
                "canvas protocol element `{token}` disappeared"
            );
        }
    }
}

#[cfg(test)]
mod unicode_offset_tests {
    use super::*;

    /// Same defect as `parser.rs`, same input class: `--matrix` reads the raw
    /// model response too. `to_lowercase()` is not byte-length preserving for
    /// ẞ U+1E9E, K U+212A, Å U+212B and İ U+0130, so an offset computed on the
    /// lowercased copy either lands mid-UTF-8 (panic, and this code runs on the
    /// driver thread) or, worse, quietly slices the wrong bytes.
    ///
    /// Measured before the fix on `"\u{212A}<matrix>{…}</matrix>"`: the
    /// extractor returned `x>{"a":` instead of `{"a":1}` — a payload the model
    /// never wrote, with no error raised.
    ///
    /// A failure here means matrix mode can be crashed, or fed a forged frame,
    /// by one character in the model's answer.
    #[test]
    fn a_length_changing_character_does_not_shift_the_matrix_extractors() {
        for ch in ["\u{1E9E}", "\u{212A}", "\u{212B}", "\u{0130}"] {
            let text = format!("{ch}<matrix>{{\"a\":1}}</matrix>");
            assert_eq!(
                extract_matrix_frame(&text).map(|(_, j)| j),
                Some("{\"a\":1}".to_string()),
                "{ch:?}: extract_matrix_frame returned a shifted slice"
            );
            assert_eq!(
                extract_all_matrix_frames(&text)
                    .into_iter()
                    .map(|(_, j)| j)
                    .collect::<Vec<_>>(),
                vec!["{\"a\":1}".to_string()],
                "{ch:?}: extract_all_matrix_frames returned a shifted slice"
            );

            let atext = format!("{ch}<action>{{\"type\":\"SystemInfo\"}}</action>");
            assert_eq!(
                extract_all_action_blocks(&atext),
                vec!["{\"type\":\"SystemInfo\"}".to_string()],
                "{ch:?}: extract_all_action_blocks returned a shifted slice"
            );
            assert_eq!(
                extract_action_block(&atext).map(|(_, j)| j),
                Some("{\"type\":\"SystemInfo\"}".to_string()),
                "{ch:?}: extract_action_block returned a shifted slice"
            );

            let wtext = format!("{ch}<watch>ls -l</watch>");
            assert_eq!(
                extract_watch_blocks(&wtext),
                vec!["ls -l".to_string()],
                "{ch:?}: extract_watch_blocks returned a shifted slice"
            );
        }
    }
}
