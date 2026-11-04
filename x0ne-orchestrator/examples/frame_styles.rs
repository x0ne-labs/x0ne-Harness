//! Side-by-side bench for the action-call frame, in a real terminal.
//!
//! Picking a border out of a chat message does not work: the glyphs land in a
//! different font at a different size against a different background, and the
//! candidate that looked best in the message is rarely the one that reads best
//! in the terminal it will live in. So this prints every candidate, in the
//! product's own colours, at a width the caller controls.
//!
//! ```sh
//! cargo run -p x0ne-orchestrator --example frame_styles
//! cargo run -p x0ne-orchestrator --example frame_styles -- 100   # width
//! ```
//!
//! Nothing here is wired into the product. It is a bench: choose a style, and
//! only then does `display.rs` change.

use colored::Colorize;

/// The four corner glyphs plus the two edge glyphs that define a frame.
struct Frame {
    name: &'static str,
    note: &'static str,
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
}

const FRAMES: &[Frame] = &[
    Frame {
        name: "0 · current",
        note: "no frame at all — what ships today",
        tl: ' ',
        tr: ' ',
        bl: ' ',
        br: ' ',
        h: ' ',
        v: ' ',
    },
    Frame {
        name: "A · quadruple dash",
        note: "┈ ┊ — finest dashes, most air",
        tl: '╭',
        tr: '╮',
        bl: '╰',
        br: '╯',
        h: '┈',
        v: '┊',
    },
    Frame {
        name: "B · double dash",
        note: "╌ ╎ — longest dashes, closest to solid",
        tl: '╭',
        tr: '╮',
        bl: '╰',
        br: '╯',
        h: '╌',
        v: '╎',
    },
    Frame {
        name: "C · triple dash",
        note: "┄ ┆ — the middle one",
        tl: '╭',
        tr: '╮',
        bl: '╰',
        br: '╯',
        h: '┄',
        v: '┆',
    },
    Frame {
        name: "D · dashed rails, solid sides",
        note: "┈ │ — horizontals recede, verticals hold the column",
        tl: '╭',
        tr: '╮',
        bl: '╰',
        br: '╯',
        h: '┈',
        v: '│',
    },
    Frame {
        name: "E · dashed, square corners",
        note: "┈ ┊ ┌┐└┘ — echoes the existing `output` box",
        tl: '┌',
        tr: '┐',
        bl: '└',
        br: '┘',
        h: '┈',
        v: '┊',
    },
];

/// One sample call per category, so the per-category colour is visible.
struct Sample {
    tag: &'static str,
    action: &'static str,
    rgb: (u8, u8, u8),
    body: &'static [&'static str],
}

const SAMPLES: &[Sample] = &[
    Sample {
        tag: "read",
        action: "StatFiles",
        rgb: (90, 160, 250),
        body: &[
            "paths=[playground/01-failing-test playground/02-unfinished +2]",
            "✗ Path is not under any allowed path prefix",
        ],
    },
    Sample {
        tag: "exec",
        action: "RunCommand",
        rgb: (220, 160, 60),
        body: &["$ cargo test --workspace", "ok · 1402 passed (12.4s)"],
    },
    Sample {
        tag: "write",
        action: "EditFile",
        rgb: (220, 200, 60),
        body: &["src/agent/confirm.rs · 1 edit", "ok (3ms)"],
    },
];

fn main() {
    let width: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(78);

    // `colored` suppresses styling when stdout is not a tty; a piped run of
    // this bench is still useful for checking alignment, so say which mode it
    // is rather than letting a colourless dump look like a bug.
    println!();
    println!(
        "  {}  width {width} · {} ",
        "frame bench".truecolor(0, 200, 220).bold(),
        if colored::control::SHOULD_COLORIZE.should_colorize() {
            "colour on"
        } else {
            "colour off (piped)"
        }
        .truecolor(90, 95, 105)
    );

    for frame in FRAMES {
        println!();
        println!(
            "  {} {}",
            frame.name.truecolor(230, 230, 235).bold(),
            frame.note.truecolor(90, 95, 105)
        );
        println!();
        for sample in SAMPLES {
            render(frame, sample, width);
            println!();
        }
    }
}

fn render(frame: &Frame, sample: &Sample, width: usize) {
    let (r, g, b) = sample.rgb;
    // The frame is dimmer than what it contains — it is scaffolding, and a
    // border as loud as its content is what makes a dense transcript tiring.
    let (fr, fg, fb) = (r / 2 + 20, g / 2 + 20, b / 2 + 20);
    let head = format!("{} · {}", sample.tag, sample.action);

    if frame.h == ' ' {
        // Style 0 reproduces today's unframed rendering for comparison.
        println!(
            "  {} {} {} {}",
            "▸".truecolor(r, g, b),
            sample.tag.truecolor(r, g, b),
            "·".truecolor(70, 75, 85),
            sample.action.truecolor(r, g, b).bold()
        );
        for (i, line) in sample.body.iter().enumerate() {
            let glyph = if i + 1 == sample.body.len() {
                "└"
            } else {
                "│"
            };
            println!(
                "  {}  {}",
                glyph.truecolor(70, 75, 85),
                line.truecolor(150, 155, 165)
            );
        }
        return;
    }

    // Top rail: corner, dash, label, dashes to the right edge, corner.
    // `chars().count()` rather than `len()` — the label is ASCII today but the
    // action names are not guaranteed to stay that way, and a byte count would
    // silently shorten the rail.
    let head_used = 3 + head.chars().count() + 1;
    let fill = width.saturating_sub(head_used + 1);
    println!(
        "  {}{} {} {}{}",
        frame.tl.to_string().truecolor(fr, fg, fb),
        frame.h.to_string().truecolor(fr, fg, fb),
        head.truecolor(r, g, b).bold(),
        frame
            .h
            .to_string()
            .repeat(fill)
            .truecolor(fr / 2 + 10, fg / 2 + 10, fb / 2 + 10),
        frame.tr.to_string().truecolor(fr, fg, fb)
    );

    for line in sample.body {
        let pad = width.saturating_sub(line.chars().count() + 4);
        println!(
            "  {} {}{} {}",
            frame.v.to_string().truecolor(fr, fg, fb),
            line.truecolor(150, 155, 165),
            " ".repeat(pad),
            frame.v.to_string().truecolor(fr, fg, fb)
        );
    }

    println!(
        "  {}{}{}",
        frame.bl.to_string().truecolor(fr, fg, fb),
        frame
            .h
            .to_string()
            .repeat(width.saturating_sub(2))
            .truecolor(fr / 2 + 10, fg / 2 + 10, fb / 2 + 10),
        frame.br.to_string().truecolor(fr, fg, fb)
    );
}
