//! Typed result renderers — the legacy TUI's richest asset, extracted so
//! both rich frontends (and whatever succeeds them) render an action result
//! the same way.
//!
//! Pure functions: `&Value -> Option<Vec<Line>>`, no frontend state, no I/O.
//! Each renderer understands one action family's result envelope and turns it
//! into a formatted pane; [`render_result`] is the dispatch table that used
//! to live inline in `ui.rs::draw_data_view`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// The typed renderer for a successful `action_type` result, or `None` when
/// no structured rendering exists and the caller should fall back to
/// markdown/JSON colouring.
pub fn render_result(action_type: &str, result: &Value) -> Option<Vec<Line<'static>>> {
    if result["success"].as_bool() != Some(true) {
        return None;
    }
    // Keyed strictly to the LIVE v0.3 vocabulary. The legacy table also
    // carried ListDir / TreeView / SystemInfo / GitStatus / GitDiff and the
    // math and string families — actions v0.3 removed in favour of shell
    // one-liners, so those arms could never match again and are gone with
    // their renderers.
    match action_type {
        "ListProcesses" => render_processes(result),
        "ReadFile" => render_file(result),
        "RunCommand" => render_command(result),
        "RunInSandbox" => render_sandbox(result),
        "SearchCode" => render_search(result),
        "FileInfo" | "Checksum" => render_fileinfo(result),
        _ => None,
    }
}

pub fn render_processes(result: &Value) -> Option<Vec<Line<'static>>> {
    let procs = result["output"]["processes"].as_array()?;
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            format!(" {:>6}  {:>8}  {:>6}  ", "PID", "MEM", "CPU%"),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("COMMAND", Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(Span::styled(
        " ─────  ────────  ──────  ─────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    for p in procs.iter().take(50) {
        let pid = p["pid"].as_u64().unwrap_or(0);
        let mem = p["memory_kb"]
            .as_u64()
            .or_else(|| p["rss_kb"].as_u64())
            .unwrap_or(0);
        let cpu = p["cpu_percent"].as_f64().unwrap_or(0.0);
        let name = p["name"]
            .as_str()
            .or_else(|| p["command"].as_str())
            .unwrap_or("?");
        let cmd = p["cmdline"].as_str().unwrap_or(name);

        let mem_str = if mem >= 1024 * 1024 {
            format!("{:.1}GB", mem as f64 / 1024.0 / 1024.0)
        } else if mem >= 1024 {
            format!("{:.1}MB", mem as f64 / 1024.0)
        } else {
            format!("{}KB", mem)
        };

        let mem_color = if mem > 500_000 {
            Color::Red
        } else if mem > 100_000 {
            Color::Yellow
        } else {
            Color::Gray
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" {:>6}", pid), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("  {:>8}", mem_str), Style::default().fg(mem_color)),
            Span::styled(
                format!("  {:>5.1}%", cpu),
                Style::default().fg(if cpu > 50.0 { Color::Red } else { Color::Gray }),
            ),
            Span::styled(
                format!("  {}", truncate(cmd, 45)),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    let total = procs.len();
    if total > 50 {
        lines.push(Line::from(Span::styled(
            format!(" … +{} more", total - 50),
            Style::default().fg(Color::DarkGray),
        )));
    }
    Some(lines)
}

pub fn render_file(result: &Value) -> Option<Vec<Line<'static>>> {
    let content = result["output"]["content"].as_str()?;
    let size = result["output"]["size_bytes"].as_u64().unwrap_or(0);
    let truncated = result["output"]["truncated"].as_bool().unwrap_or(false);

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            format!(" {} ", format_size(size)),
            Style::default().fg(Color::Cyan),
        ),
        if truncated {
            Span::styled("(truncated)", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("", Style::default())
        },
    ]));
    lines.push(Line::from(Span::styled(" ", Style::default())));

    for (i, line) in content.lines().enumerate().take(200) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:>4} ", i + 1),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
        ]));
    }
    if content.lines().count() > 200 {
        lines.push(Line::from(Span::styled(
            format!(" … +{} more lines", content.lines().count() - 200),
            Style::default().fg(Color::DarkGray),
        )));
    }
    Some(lines)
}

pub fn render_command(result: &Value) -> Option<Vec<Line<'static>>> {
    let o = &result["output"];
    let exit_code = o["exit_code"].as_i64().unwrap_or(-1);
    let stdout = o["stdout"].as_str().unwrap_or("");
    let stderr = o["stderr"].as_str().unwrap_or("");
    let timed_out = o["timed_out"].as_bool().unwrap_or(false);

    let mut lines: Vec<Line<'static>> = Vec::new();

    let exit_color = if exit_code == 0 {
        Color::Green
    } else {
        Color::Red
    };
    let mut header = vec![Span::styled(
        format!(" exit {} ", exit_code),
        Style::default().fg(exit_color).add_modifier(Modifier::BOLD),
    )];
    if timed_out {
        header.push(Span::styled(
            " TIMEOUT ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header));
    lines.push(Line::from(Span::styled(" ", Style::default())));

    let text = if stdout.is_empty() { stderr } else { stdout };
    for line in text.lines().take(100) {
        lines.push(Line::from(Span::styled(
            format!(" {}", line),
            Style::default().fg(Color::Gray),
        )));
    }
    if !stdout.is_empty() && !stderr.is_empty() {
        lines.push(Line::from(Span::styled(" ", Style::default())));
        lines.push(Line::from(Span::styled(
            " stderr:",
            Style::default().fg(Color::Red),
        )));
        for line in stderr.lines().take(20) {
            lines.push(Line::from(Span::styled(
                format!(" {}", line),
                Style::default().fg(Color::Red),
            )));
        }
    }
    Some(lines)
}

pub fn render_sandbox(result: &Value) -> Option<Vec<Line<'static>>> {
    let o = &result["output"];
    let mut lines = render_command(result)?;
    if let Some(image) = o["image"].as_str() {
        lines.insert(
            0,
            Line::from(vec![Span::styled(
                format!(" container: {} ", image),
                Style::default().fg(Color::Cyan),
            )]),
        );
    }
    Some(lines)
}

pub fn render_search(result: &Value) -> Option<Vec<Line<'static>>> {
    let matches = result["output"]["matches"]
        .as_array()
        .or_else(|| result["output"]["files"].as_array())?;
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!(" {} results", matches.len()),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(" ", Style::default())));

    for m in matches.iter().take(40) {
        let file = m["file"]
            .as_str()
            .or_else(|| m["path"].as_str())
            .or_else(|| m.as_str())
            .unwrap_or("?");
        let line_no = m["line"].as_u64();
        let content = m["line_content"]
            .as_str()
            .or_else(|| m["content"].as_str())
            .unwrap_or("");

        let mut spans = vec![Span::styled(
            format!(" {}", file),
            Style::default().fg(Color::Cyan),
        )];
        if let Some(ln) = line_no {
            spans.push(Span::styled(
                format!(":{}", ln),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !content.is_empty() {
            spans.push(Span::styled(
                format!("  {}", truncate(content.trim(), 50)),
                Style::default().fg(Color::Gray),
            ));
        }
        lines.push(Line::from(spans));
    }
    if matches.len() > 40 {
        lines.push(Line::from(Span::styled(
            format!(" … +{} more", matches.len() - 40),
            Style::default().fg(Color::DarkGray),
        )));
    }
    Some(lines)
}

pub fn render_fileinfo(result: &Value) -> Option<Vec<Line<'static>>> {
    let o = &result["output"];
    let mut lines: Vec<Line<'static>> = Vec::new();

    let fields: Vec<(&str, Option<String>)> = vec![
        ("path", o["path"].as_str().map(|s| s.to_string())),
        ("size", o["size_bytes"].as_u64().map(format_size)),
        ("type", o["file_type"].as_str().map(|s| s.to_string())),
        (
            "permissions",
            o["permissions"].as_str().map(|s| s.to_string()),
        ),
        ("modified", o["modified"].as_str().map(|s| s.to_string())),
        (
            "checksum",
            o["checksum"]
                .as_str()
                .or_else(|| o["hash"].as_str())
                .map(|s| s.to_string()),
        ),
    ];

    for (label, val) in &fields {
        if let Some(v) = val {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:>12}  ", label),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(v.clone(), Style::default().fg(Color::White)),
            ]));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines)
}

pub fn colorize_json_line(line: &str) -> Line<'_> {
    if line.contains("\":") {
        let parts: Vec<&str> = line.splitn(2, "\":").collect();
        if parts.len() == 2 {
            let val = parts[1];
            let val_color = if val.trim().starts_with('"') {
                Color::Green
            } else if val.trim().starts_with("true") || val.trim().starts_with("false") {
                Color::LightRed
            } else {
                Color::Yellow
            };
            return Line::from(vec![
                Span::styled(parts[0], Style::default().fg(Color::Cyan)),
                Span::styled("\":", Style::default().fg(Color::DarkGray)),
                Span::styled(val, Style::default().fg(val_color)),
            ]);
        }
    }
    if matches!(line.trim(), "{" | "}" | "}," | "[" | "]," | "]") {
        return Line::from(Span::styled(line, Style::default().fg(Color::DarkGray)));
    }
    Line::from(Span::styled(line, Style::default().fg(Color::Gray)))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

pub fn progress_bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

pub fn bar_color(pct: f64) -> Color {
    if pct > 85.0 {
        Color::Red
    } else if pct > 60.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}
