//! Width-aware markdown rendering for LLM prose in CLI mode.
//!
//! Pure formatter: no I/O, no global state, returns a `Vec<String>` of
//! fully-styled (ANSI-coloured) lines that the caller frames with its
//! own `  │ ` rail in `terminal::print_llm_message`.
//!
//! Design rules:
//!
//! * **Wrap on plain text, style after** — `textwrap` doesn't understand
//!   ANSI escape sequences, so we wrap the source markdown before the
//!   per-line `render_inline_styles` pass. This means a line never breaks
//!   inside a `\x1b[1m...\x1b[0m` group. The trade-off is that wrapping is
//!   slightly conservative on lines containing markdown markers
//!   (`**bold**` counts 8 chars, not 4), but for prose this is invisible.
//! * **Unicode width everywhere** — counting `chars()` makes CJK and most
//!   emoji measure 1 column when they actually take 2. We use
//!   `unicode_width::UnicodeWidthStr::width` consistently, both in the
//!   wrapper (via `textwrap`'s `unicode-width` feature) and in
//!   `visible_width` for ANSI-stripped measurement.
//! * **Tables truncate-in-cell, not wrap-in-cell** — wrapping inside a
//!   table cell means the table becomes a mini layout engine. For v1 we
//!   shrink columns proportionally to fit `layout.width`, then truncate
//!   long contents with `…`. Multi-line cells are a v2 problem.
//! * **No syntax highlighting yet** — fenced code blocks get a subtle
//!   background tint (`on_truecolor(20, 22, 28)`) and language label,
//!   but no token colouring. Add `syntect` later when the binary size
//!   trade-off is worth it.

use colored::Colorize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::terminal::{
    latex_to_unicode, looks_like_block_latex, parse_list_item, render_inline_styles,
    strip_html_tags, visible_cell_width,
};

// ── Public types ─────────────────────────────────────────────────────────────

/// Layout knobs for a single render pass. `width` is the **column budget for
/// the body content** — the caller's framing (e.g. `  │ `) is *not* included
/// and must be subtracted before constructing the layout.
#[derive(Debug, Clone, Copy)]
pub struct ProseLayout {
    pub width: usize,
}

impl ProseLayout {
    pub fn new(width: usize) -> Self {
        Self {
            // 12 columns is roughly the lower bound where prose stays
            // readable (a 6-letter word + bullet + indent fits). Below that
            // we still render — the user may have an extreme terminal —
            // but textwrap will force-break long tokens.
            width: width.max(12),
        }
    }
}

/// X0ne dark theme — colours pulled from existing `terminal.rs` palette
/// so the prose box matches the action cards and pulse output.
#[derive(Debug, Clone, Copy)]
pub struct ProseTheme {
    pub heading_h1: (u8, u8, u8),
    pub heading_h2: (u8, u8, u8),
    pub heading_lo: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub muted: (u8, u8, u8),
    pub dim: (u8, u8, u8),
    pub rail: (u8, u8, u8),
    pub code_bg: (u8, u8, u8),
    pub code_fg: (u8, u8, u8),
    pub quote_text: (u8, u8, u8),
    pub callout_note: (u8, u8, u8),
    pub callout_tip: (u8, u8, u8),
    pub callout_warn: (u8, u8, u8),
    pub callout_imp: (u8, u8, u8),
    pub callout_caution: (u8, u8, u8),
}

impl ProseTheme {
    pub const fn x0ne_dark() -> Self {
        Self {
            heading_h1: (0, 220, 240),
            heading_h2: (0, 200, 220),
            heading_lo: (0, 170, 190),
            accent: (0, 200, 220),
            muted: (158, 168, 182),
            dim: (132, 142, 157),
            rail: (104, 116, 130),
            // Sits a hair above terminal black — survives most "dark" themes,
            // including default tmux and Alacritty's #1d1f21.
            code_bg: (20, 22, 28),
            code_fg: (180, 200, 220),
            quote_text: (158, 168, 182),
            callout_note: (90, 170, 240),
            callout_tip: (110, 220, 160),
            callout_warn: (240, 200, 100),
            callout_imp: (210, 130, 240),
            callout_caution: (240, 110, 110),
        }
    }
}

/// Colour of the assistant's response FRAME — the `╭─ x0ne ─╮ │ ╰─╯` strokes
/// around a reply, on both frontends. A pale teal one step off the X0NE
/// accent, opposite the violet the TUI paints the operator's lines in, so a
/// transcript scans as two voices at a glance. The prose itself keeps the
/// terminal's default colour: owner, 2026-08-17 — « I want it to be the
/// outline », not the text. Shared with the TUI (`tui/app.rs`,
/// `x0ne_response_box_lines`), one constant, one colour.
pub const ASSISTANT_FRAME: (u8, u8, u8) = (172, 216, 208);

// ── Entry point ──────────────────────────────────────────────────────────────

/// Render `markdown` to a list of pre-styled terminal lines. Each line is
/// already wrapped to `layout.width` and ready to be printed verbatim
/// (typically with a leading `  │ ` rail, supplied by the caller).
pub fn render_prose(markdown: &str, layout: &ProseLayout, theme: &ProseTheme) -> Vec<String> {
    // SECURITY (audit 2026-08-15, finding 4): the model writes this markdown
    // and this function turns it into ANSI. Any escape the model puts in
    // passes straight through — `visible_width` below SKIPS escape sequences
    // to count columns, which is a measurement, not a filter, and reading it
    // as one is how this went unnoticed. The response box is printed
    // immediately before the next action's box and its confirmation prompt,
    // and SGR state persists across lines: prose ending in `\x1b[8m` leaves
    // that prompt invisible. `sanitize_block` keeps the line and indentation
    // structure markdown is made of.
    let markdown = crate::text_safety::sanitize_block(markdown);
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_code_block = false;
    let mut _in_html_block = false;

    while i < lines.len() {
        let l = lines[i];
        let trimmed = l.trim();

        // ── Fenced code blocks ───────────────────────────────────────────
        if trimmed.starts_with("```") {
            // Collect until closing fence
            let lang = trimmed.trim_start_matches('`').trim().to_string();
            let mut body: Vec<&str> = Vec::new();
            i += 1;
            while i < lines.len() {
                let cl = lines[i];
                if cl.trim().starts_with("```") {
                    i += 1;
                    break;
                }
                body.push(cl);
                i += 1;
            }
            out.extend(render_code_block(&body, &lang, layout, theme));
            in_code_block = false;
            continue;
        }
        let _ = in_code_block;

        if trimmed.is_empty() {
            out.push(String::new());
            i += 1;
            continue;
        }

        // ── HTML <details> / <summary> / <br> / <hr> ─────────────────────
        if trimmed.starts_with("<details") {
            _in_html_block = true;
            i += 1;
            continue;
        }
        if trimmed.starts_with("</details") {
            _in_html_block = false;
            out.push(String::new());
            i += 1;
            continue;
        }
        if trimmed.starts_with("<summary") {
            let inner = strip_html_tags(trimmed);
            let icon = "▾".truecolor(theme.accent.0, theme.accent.1, theme.accent.2);
            let body = render_inline_styles(&inner).bold().to_string();
            out.push(format!("{} {}", icon, body));
            i += 1;
            continue;
        }
        if trimmed.starts_with("</summary") {
            i += 1;
            continue;
        }
        if trimmed.starts_with("<br") {
            out.push(String::new());
            i += 1;
            continue;
        }
        if trimmed.starts_with("<hr") || trimmed == "---" || trimmed == "***" || trimmed == "___" {
            out.push(horizontal_rule(layout.width, theme));
            i += 1;
            continue;
        }

        // ── Tables ───────────────────────────────────────────────────────
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut ti = i;
            while ti < lines.len() {
                let tl = lines[ti].trim();
                if !tl.starts_with('|') || !tl.ends_with('|') {
                    break;
                }
                let is_sep = tl
                    .chars()
                    .all(|c| c == '|' || c == '-' || c == ':' || c == ' ');
                if !is_sep {
                    let cells: Vec<String> = tl
                        .split('|')
                        .filter(|s| !s.is_empty())
                        .map(|s| strip_html_tags(s.trim()))
                        .collect();
                    rows.push(cells);
                }
                ti += 1;
            }
            if !rows.is_empty() {
                out.extend(render_table(&rows, layout, theme));
            }
            i = ti;
            continue;
        }

        // ── Blockquotes & GitHub callouts ────────────────────────────────
        if trimmed.starts_with('>') {
            let mut quote_lines: Vec<String> = Vec::new();
            let mut qi = i;
            while qi < lines.len() {
                let ql = lines[qi].trim();
                if !ql.starts_with('>') {
                    break;
                }
                quote_lines.push(ql.trim_start_matches('>').trim_start().to_string());
                qi += 1;
            }
            out.extend(render_quote_or_callout(&quote_lines, layout, theme));
            i = qi;
            continue;
        }

        // ── Headings ─────────────────────────────────────────────────────
        if let Some((level, content)) = parse_heading(trimmed) {
            out.extend(render_heading(level, &content, layout, theme));
            i += 1;
            continue;
        }

        // ── Lists ────────────────────────────────────────────────────────
        let raw = l;
        let indent_cols = raw.len() - raw.trim_start().len();
        if (trimmed.starts_with("- ") || trimmed.starts_with("* "))
            || (trimmed.len() > 2
                && trimmed
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                && trimmed.find(". ").map(|p| p <= 3).unwrap_or(false))
        {
            let depth = (indent_cols / 2).min(4);
            let (icon, content) = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                let (ic, body) = parse_list_item(&trimmed[2..]);
                // Replace the default "·" bullet with a depth-aware glyph so
                // nested lists are visually distinguishable.
                let glyph = nested_bullet(depth, theme);
                let icon = if ic
                    == "·"
                        .truecolor(theme.dim.0, theme.dim.1, theme.dim.2)
                        .to_string()
                {
                    glyph
                } else {
                    ic
                };
                (icon, body)
            } else {
                let dot_pos = trimmed.find(". ").unwrap();
                let num = &trimmed[..=dot_pos];
                let body = trimmed[dot_pos + 2..].to_string();
                (
                    num.truecolor(theme.accent.0, theme.accent.1, theme.accent.2)
                        .to_string(),
                    body,
                )
            };
            out.extend(render_list_item(depth, &icon, &content, layout, theme));
            i += 1;
            continue;
        }

        // ── Block-level LaTeX ────────────────────────────────────────────
        if looks_like_block_latex(trimmed) {
            let rendered = latex_to_unicode(trimmed);
            let icon = "≡"
                .truecolor(theme.heading_lo.0, theme.heading_lo.1, theme.heading_lo.2)
                .to_string();
            for w in wrap_plain(&rendered, layout.width.saturating_sub(2), 0) {
                let styled = w.truecolor(theme.code_fg.0, theme.code_fg.1, theme.code_fg.2);
                out.push(format!("{} {}", icon, styled));
            }
            i += 1;
            continue;
        }

        // ── Plain paragraph line ─────────────────────────────────────────
        let cleaned = strip_html_tags(trimmed);
        for w in wrap_plain(&cleaned, layout.width, 0) {
            out.push(render_inline_styles(&w));
        }
        i += 1;
    }

    out
}

// ── Heading parsing & rendering ──────────────────────────────────────────────

fn parse_heading(trimmed: &str) -> Option<(usize, String)> {
    for (prefix, lvl) in [
        ("###### ", 6),
        ("##### ", 5),
        ("#### ", 4),
        ("### ", 3),
        ("## ", 2),
        ("# ", 1),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((lvl, rest.to_string()));
        }
    }
    None
}

fn render_heading(
    level: usize,
    content: &str,
    layout: &ProseLayout,
    theme: &ProseTheme,
) -> Vec<String> {
    let cleaned = strip_html_tags(content);
    let mut out = Vec::new();
    match level {
        1 => {
            out.push(String::new());
            let bar = "━".truecolor(theme.heading_h2.0, theme.heading_h2.1, theme.heading_h2.2);
            for (idx, w) in wrap_plain(&cleaned, layout.width.saturating_sub(2), 2)
                .into_iter()
                .enumerate()
            {
                let styled = render_inline_styles(&w)
                    .truecolor(theme.heading_h1.0, theme.heading_h1.1, theme.heading_h1.2)
                    .bold()
                    .to_string();
                if idx == 0 {
                    out.push(format!("{} {}", bar, styled));
                } else {
                    out.push(format!("  {}", styled));
                }
            }
        }
        2 | 3 => {
            let icon = "▸".truecolor(theme.heading_h2.0, theme.heading_h2.1, theme.heading_h2.2);
            for (idx, w) in wrap_plain(&cleaned, layout.width.saturating_sub(2), 2)
                .into_iter()
                .enumerate()
            {
                let styled = render_inline_styles(&w)
                    .truecolor(theme.heading_h2.0, theme.heading_h2.1, theme.heading_h2.2)
                    .bold()
                    .to_string();
                if idx == 0 {
                    out.push(format!("{} {}", icon, styled));
                } else {
                    out.push(format!("  {}", styled));
                }
            }
        }
        _ => {
            // h4-h6: smaller accent, slightly dimmer
            let icon = "▸".truecolor(theme.heading_lo.0, theme.heading_lo.1, theme.heading_lo.2);
            for (idx, w) in wrap_plain(&cleaned, layout.width.saturating_sub(2), 2)
                .into_iter()
                .enumerate()
            {
                let styled = render_inline_styles(&w).truecolor(
                    theme.heading_lo.0,
                    theme.heading_lo.1,
                    theme.heading_lo.2,
                );
                if idx == 0 {
                    out.push(format!("{} {}", icon, styled));
                } else {
                    out.push(format!("  {}", styled));
                }
            }
        }
    }
    out
}

// ── Lists ────────────────────────────────────────────────────────────────────

fn nested_bullet(depth: usize, theme: &ProseTheme) -> String {
    // Per-depth glyph rotation. Matches GFM-ish convention but keeps dim/muted
    // tones so the bullet never overpowers the content.
    let (g, c) = match depth {
        0 => ("·", theme.dim),
        1 => ("◦", theme.dim),
        2 => ("▸", theme.muted),
        3 => ("▹", theme.muted),
        _ => ("∘", theme.muted),
    };
    g.truecolor(c.0, c.1, c.2).to_string()
}

fn render_list_item(
    depth: usize,
    icon: &str,
    content: &str,
    layout: &ProseLayout,
    theme: &ProseTheme,
) -> Vec<String> {
    let pad = "  ".repeat(depth);
    // Visible width of "icon + space" — used for hanging indent on continuation
    // lines. Bullet glyphs are 1 column; numeric prefixes ("12.") may be wider.
    let icon_w = visible_width(icon);
    let prefix_first = format!("{}{} ", pad, icon);
    let prefix_cont = format!("{}{} ", pad, " ".repeat(icon_w));
    let content_w = layout
        .width
        .saturating_sub(visible_width(&prefix_first))
        .max(8);

    let cleaned = strip_html_tags(content);
    let wrapped = wrap_plain(&cleaned, content_w, 0);
    let mut out = Vec::with_capacity(wrapped.len());
    for (idx, w) in wrapped.into_iter().enumerate() {
        let body = render_inline_styles(&w);
        if idx == 0 {
            out.push(format!("{}{}", prefix_first, body));
        } else {
            // Dim the rail-style continuation marker so multi-line items
            // visually stay attached to their parent bullet.
            let _ = theme;
            out.push(format!("{}{}", prefix_cont, body));
        }
    }
    out
}

// ── Blockquotes & callouts ───────────────────────────────────────────────────

/// Detect GitHub-style callout: first line is `[!KIND] optional title`.
fn detect_callout(
    quote_lines: &[String],
) -> Option<(&'static str, (u8, u8, u8), String, Vec<String>)> {
    let theme = ProseTheme::x0ne_dark();
    let first = quote_lines.first()?.trim();
    let rest = first.strip_prefix("[!")?;
    let close = rest.find(']')?;
    let kind = rest[..close].to_ascii_uppercase();
    let title_raw = rest[close + 1..].trim().to_string();
    let (label, color) = match kind.as_str() {
        "NOTE" => ("NOTE", theme.callout_note),
        "TIP" => ("TIP", theme.callout_tip),
        "WARNING" => ("WARNING", theme.callout_warn),
        "IMPORTANT" => ("IMPORTANT", theme.callout_imp),
        "CAUTION" => ("CAUTION", theme.callout_caution),
        _ => return None,
    };
    let body = quote_lines[1..].to_vec();
    Some((label, color, title_raw, body))
}

fn render_quote_or_callout(
    quote_lines: &[String],
    layout: &ProseLayout,
    theme: &ProseTheme,
) -> Vec<String> {
    if let Some((label, color, title, body)) = detect_callout(quote_lines) {
        let mut out = Vec::new();
        // Header: ◆ NOTE  (optional title)
        let icon = "◆".truecolor(color.0, color.1, color.2);
        let label_s = label
            .truecolor(color.0, color.1, color.2)
            .bold()
            .to_string();
        let head = if title.is_empty() {
            format!("{} {}", icon, label_s)
        } else {
            format!("{} {} {}", icon, label_s, render_inline_styles(&title))
        };
        out.push(head);
        // Body wrapped with rail
        let rail = "▎".truecolor(color.0, color.1, color.2);
        let content_w = layout.width.saturating_sub(2).max(8);
        for line in &body {
            if line.trim().is_empty() {
                out.push(format!("{}", rail));
                continue;
            }
            let cleaned = strip_html_tags(line);
            for w in wrap_plain(&cleaned, content_w, 0) {
                let body = render_inline_styles(&w);
                out.push(format!("{} {}", rail, body));
            }
        }
        out
    } else {
        // Plain blockquote — single-glyph rail, muted text
        let rail = "▎".truecolor(theme.dim.0, theme.dim.1, theme.dim.2);
        let mut out = Vec::new();
        let content_w = layout.width.saturating_sub(2).max(8);
        for line in quote_lines {
            if line.trim().is_empty() {
                out.push(format!("{}", rail));
                continue;
            }
            let cleaned = strip_html_tags(line);
            for w in wrap_plain(&cleaned, content_w, 0) {
                let body = render_inline_styles(&w)
                    .truecolor(theme.quote_text.0, theme.quote_text.1, theme.quote_text.2)
                    .to_string();
                out.push(format!("{} {}", rail, body));
            }
        }
        out
    }
}

// ── Code blocks ──────────────────────────────────────────────────────────────

fn render_code_block(
    body: &[&str],
    lang: &str,
    layout: &ProseLayout,
    theme: &ProseTheme,
) -> Vec<String> {
    let mut out = Vec::new();
    let rail_color = theme.rail;
    let rail = "│".truecolor(rail_color.0, rail_color.1, rail_color.2);
    // Header: ┌── lang
    let head_bar = "┌──".truecolor(rail_color.0, rail_color.1, rail_color.2);
    if lang.is_empty() {
        out.push(format!(
            "{}",
            "┌──────────".truecolor(rail_color.0, rail_color.1, rail_color.2)
        ));
    } else {
        let lang_s = lang.truecolor(theme.muted.0, theme.muted.1, theme.muted.2);
        out.push(format!("{} {}", head_bar, lang_s));
    }
    // Body — each line gets a rail + 1-space pad + content padded to width
    // with a subtle bg fill. Long lines are *truncated* (not wrapped) so
    // code fidelity wins over spillage; the user can scroll the source.
    let content_w = layout.width.saturating_sub(4).max(8);
    for raw in body {
        let truncated = truncate_to_width(raw, content_w);
        let pad = content_w.saturating_sub(visible_width(&truncated));
        let filled = format!("{}{}", truncated, " ".repeat(pad));
        let painted = filled
            .truecolor(theme.code_fg.0, theme.code_fg.1, theme.code_fg.2)
            .on_truecolor(theme.code_bg.0, theme.code_bg.1, theme.code_bg.2)
            .to_string();
        out.push(format!("{} {}", rail, painted));
    }
    out.push(format!(
        "{}",
        "└──────────".truecolor(rail_color.0, rail_color.1, rail_color.2)
    ));
    out
}

// ── Tables ───────────────────────────────────────────────────────────────────

fn render_table(rows: &[Vec<String>], layout: &ProseLayout, theme: &ProseTheme) -> Vec<String> {
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }
    // Natural width per column = max visible (post-render) width across all rows
    let mut natural: Vec<usize> = vec![0; col_count];
    for r in rows {
        for (j, cell) in r.iter().enumerate() {
            if j < col_count {
                natural[j] = natural[j].max(visible_cell_width(cell));
            }
        }
    }
    // Floor each col to 3 (so it can hold "…") and ceil to 30 (no monster cols)
    for w in natural.iter_mut() {
        *w = (*w).clamp(3, 30);
    }
    // Total width budget = layout.width - separator overhead (" │ " between cols, 3 chars)
    let sep_width = 3 * (col_count.saturating_sub(1));
    let budget = layout.width.saturating_sub(sep_width).max(col_count * 3);

    let mut col_w = natural.clone();
    let total: usize = col_w.iter().sum();
    if total > budget {
        // Proportional shrink. Scale every col by budget/total, ensuring min 3.
        let scale = budget as f64 / total as f64;
        col_w = natural
            .iter()
            .map(|n| ((*n as f64 * scale).floor() as usize).max(3))
            .collect();
        // Re-balance any rounding loss against the largest column
        let mut diff = budget as isize - col_w.iter().sum::<usize>() as isize;
        while diff > 0 {
            if let Some((idx, _)) = col_w.iter().enumerate().max_by_key(|(_, w)| **w) {
                col_w[idx] += 1;
                diff -= 1;
            } else {
                break;
            }
        }
        while diff < 0 {
            if let Some((idx, _)) = col_w
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .filter(|(_, w)| **w > 3)
            {
                col_w[idx] -= 1;
                diff += 1;
            } else {
                break;
            }
        }
    }

    let sep_styled = " │ ".truecolor(theme.rail.0, theme.rail.1, theme.rail.2);
    let mut out = Vec::new();
    if let Some(header) = rows.first() {
        let cells: Vec<String> = header
            .iter()
            .enumerate()
            .map(|(j, cell)| {
                let w = col_w.get(j).copied().unwrap_or(8);
                let text = truncate_to_width(cell, w);
                let pad = w.saturating_sub(visible_cell_width(&text));
                format!(
                    "{}{}",
                    text.truecolor(theme.accent.0, theme.accent.1, theme.accent.2)
                        .bold(),
                    " ".repeat(pad)
                )
            })
            .collect();
        out.push(cells.join(&sep_styled.to_string()));
        // Separator row
        let sep_cells: Vec<String> = col_w.iter().map(|w| "─".repeat(*w)).collect();
        out.push(
            sep_cells
                .join("─┼─")
                .truecolor(theme.rail.0, theme.rail.1, theme.rail.2)
                .to_string(),
        );
    }
    for row in rows.iter().skip(1) {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(j, cell)| {
                let w = col_w.get(j).copied().unwrap_or(8);
                let text = truncate_to_width(cell, w);
                let pad = w.saturating_sub(visible_cell_width(&text));
                format!("{}{}", render_inline_styles(&text), " ".repeat(pad))
            })
            .collect();
        out.push(cells.join(&sep_styled.to_string()));
    }
    out
}

// ── Horizontal rule ──────────────────────────────────────────────────────────

fn horizontal_rule(width: usize, theme: &ProseTheme) -> String {
    "─"
        .repeat(width)
        .truecolor(theme.rail.0, theme.rail.1, theme.rail.2)
        .to_string()
}

// ── Width helpers ────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences (CSI / OSC) and return the unicode display
/// width of the remaining text. Used for padding / alignment of already-styled
/// content.
pub fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI ESC [ ... letter | OSC ESC ] ... BEL/ST
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // Consume until BEL (\x07) or ST (ESC \\)
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            // Followed by '\\' to terminate
                            if matches!(chars.peek(), Some('\\')) {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    // Lone ESC — skip next char if any
                    chars.next();
                }
            }
            continue;
        }
        if let Some(cw) = UnicodeWidthChar::width(c) {
            w += cw;
        }
    }
    w
}

/// Wrap *plain* (un-styled) text to `width` columns at word boundaries,
/// honouring unicode width and force-breaking long tokens (URLs, paths).
/// `hanging` is the hanging-indent width — applied to lines 2..N as spaces.
pub fn wrap_plain(text: &str, width: usize, hanging: usize) -> Vec<String> {
    let width = width.max(8);
    let opts = textwrap::Options::new(width)
        .break_words(true)
        .word_separator(textwrap::WordSeparator::AsciiSpace)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let opts = if hanging > 0 {
        opts.subsequent_indent(Box::leak(" ".repeat(hanging).into_boxed_str()))
    } else {
        opts
    };
    textwrap::wrap(text, opts)
        .into_iter()
        .map(|c| c.into_owned())
        .collect()
}

/// Truncate a (potentially marked-up) string so its **visible width** fits
/// in `width` columns. The `…` ellipsis is appended only if truncation
/// actually happened. Width-aware via unicode-width.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    let vw = visible_width(s);
    if vw <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    // Walk char by char, accumulating display width, keeping budget for "…".
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut acc = 0usize;
    let mut in_csi = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_csi {
            out.push(c);
            if c.is_ascii_alphabetic() {
                in_csi = false;
            }
            continue;
        }
        if c == '\x1b' {
            out.push(c);
            if matches!(chars.peek(), Some('[')) {
                in_csi = true;
            }
            continue;
        }
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > budget {
            out.push('…');
            return out;
        }
        out.push(c);
        acc += cw;
    }
    out
}

// Re-export so callers can use `prose::measure` for ad-hoc framing.
pub fn measure(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_strips_csi() {
        let s = "\x1b[1mhello\x1b[0m";
        assert_eq!(visible_width(s), 5);
    }

    #[test]
    fn visible_width_counts_cjk_as_two() {
        // 日本語 = 3 chars, 6 columns
        assert_eq!(visible_width("日本語"), 6);
    }

    #[test]
    fn visible_width_counts_emoji_as_two() {
        // 🚀 = 1 char, 2 columns (per unicode-width)
        assert_eq!(visible_width("🚀"), 2);
    }

    #[test]
    fn wrap_respects_word_boundaries() {
        let lines = wrap_plain("the quick brown fox jumps over the lazy dog", 20, 0);
        // No mid-word break: every word should appear intact
        let joined = lines.join(" ");
        for word in ["quick", "brown", "fox", "jumps"] {
            assert!(joined.contains(word), "word {word} broken across wrap");
        }
        // Every line must respect width
        for l in &lines {
            assert!(l.chars().count() <= 20, "line over width: {l:?}");
        }
    }

    #[test]
    fn wrap_force_breaks_long_url() {
        let url = "https://example.com/a/very/long/path/that/exceeds/our/twenty/column/budget";
        let lines = wrap_plain(url, 20, 0);
        assert!(lines.len() > 1, "long URL was not broken");
        for l in &lines {
            assert!(l.chars().count() <= 20, "URL line over width: {l:?}");
        }
    }

    #[test]
    fn wrap_hanging_indent() {
        let lines = wrap_plain("alpha beta gamma delta epsilon zeta", 12, 3);
        assert!(lines.len() >= 2);
        for (i, l) in lines.iter().enumerate() {
            if i == 0 {
                assert!(!l.starts_with("   "), "first line should not be indented");
            } else {
                assert!(
                    l.starts_with("   "),
                    "continuation line missing hanging indent: {l:?}"
                );
            }
        }
    }

    #[test]
    fn truncate_appends_ellipsis_when_over() {
        let s = "abcdefghij";
        assert_eq!(truncate_to_width(s, 5), "abcd…");
        assert_eq!(truncate_to_width(s, 100), "abcdefghij");
    }

    #[test]
    fn truncate_handles_cjk() {
        // 4 CJK chars = 8 cols. Asking for 5 → "日…" = 3 cols, accumulator
        // can't fit a second 2-col char without overshooting the 4-col budget.
        let s = "日本語学";
        let t = truncate_to_width(s, 5);
        assert!(visible_width(&t) <= 5);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn render_basic_paragraph_wraps() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(20);
        let lines = render_prose(
            "the quick brown fox jumps over the lazy dog",
            &layout,
            &theme,
        );
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(visible_width(l) <= 20, "rendered line over width: {l:?}");
        }
    }

    #[test]
    fn render_heading_styles_first_line() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(40);
        let lines = render_prose("## Hello there", &layout, &theme);
        assert!(!lines.is_empty());
        // Heading line contains the ▸ glyph
        assert!(lines.iter().any(|l| l.contains('▸')));
    }

    #[test]
    fn render_callout_note_detected() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(60);
        let lines = render_prose(
            "> [!NOTE] heads up\n> body line one\n> body line two",
            &layout,
            &theme,
        );
        assert!(lines.iter().any(|l| l.contains("NOTE")));
        assert!(lines.iter().any(|l| l.contains('◆')));
        // Rail glyph on body lines
        assert!(lines.iter().any(|l| l.contains('▎')));
    }

    #[test]
    fn render_callout_warning_uses_color_marker() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(60);
        let lines = render_prose("> [!WARNING] fire", &layout, &theme);
        assert!(lines.iter().any(|l| l.contains("WARNING")));
    }

    #[test]
    fn render_table_fits_in_layout_width() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(40);
        let md = "\
| Col A | Col B | Col C |
|-------|-------|-------|
| short | longer cell value | tiny |
| a     | b     | c     |
";
        let lines = render_prose(md, &layout, &theme);
        assert!(!lines.is_empty());
        for l in &lines {
            assert!(visible_width(l) <= 40, "table line over width: {l:?}");
        }
    }

    #[test]
    fn render_code_block_pads_to_width() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(30);
        let md = "```rust\nfn main() {}\n```";
        let lines = render_prose(md, &layout, &theme);
        // Header + body + footer
        assert!(lines.len() >= 3);
        for l in &lines {
            assert!(visible_width(l) <= 30, "code line over width: {l:?}");
        }
    }

    #[test]
    fn render_list_hanging_indent() {
        let theme = ProseTheme::x0ne_dark();
        let layout = ProseLayout::new(24);
        let lines = render_prose(
            "- alpha beta gamma delta epsilon zeta eta theta iota kappa",
            &layout,
            &theme,
        );
        // Should produce multiple lines, all within width
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(visible_width(l) <= 24, "list line over width: {l:?}");
        }
    }
}
