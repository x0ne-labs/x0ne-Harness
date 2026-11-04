//! Boot intro — the "noise → logo → subtitle → fade" entrance animation.
//!
//! Originally lived inside `matrix_mode::play_entrance_animation` so it only
//! fired when the user launched `x0ne --matrix`. Plain `x0ne` deserves the
//! same delight, so the six phases are extracted here as a standalone
//! function that any entry point (CLI, TUI, Matrix) can call at boot.
//!
//! ## Terminal contract
//!
//! `play_boot_intro()` is *self-contained*: it enters the alternate screen
//! buffer, hides the cursor, plays the animation, and unconditionally
//! restores the previous terminal state on return (including on panic —
//! the RAII guard below handles that). When control comes back, the user's
//! scrollback is untouched and the next `println!` lands on a pristine
//! primary screen.
//!
//! ## Skipping
//!
//! Any keypress during the animation ends it at once and hands control to
//! x0ne (the key is consumed, never forwarded to the prompt).
//!
//! We skip silently (return Ok immediately) when:
//!   - stdout is not a TTY (piped, redirected, CI)
//!   - `X0NE_NO_INTRO=1` is set (CI / headless runs)
//!   - `TERM=dumb` (terminals with no ANSI support)
//!
//! The CLI may also expose a `--no-intro` flag that sets `X0NE_NO_INTRO=1`
//! for its callees.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;

/// Sentinel carried by the `Err` that ends the animation on a keypress.
/// Not an error for the caller: `play_boot_intro` swallows it like any
/// cosmetic failure and control passes to x0ne immediately.
const SKIPPED: &str = "intro skipped by keypress";
use crossterm::{
    cursor, execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Play the entrance animation on an alternate screen buffer, then return
/// to the primary screen with the user's terminal content intact.
///
/// Returns `Ok(())` immediately (no animation) when the environment says
/// "please don't" (non-TTY, dumb terminal, or `X0NE_NO_INTRO=1`). This
/// lets callers invoke `play_boot_intro()` unconditionally at startup
/// without guarding it themselves.
pub fn play_boot_intro() -> Result<()> {
    if !should_play() {
        return Ok(());
    }

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    // RAII guard — restore the primary screen even on panic or early return.
    // Drops in reverse order of construction, so the alt-screen comes off
    // BEFORE raw mode is disabled, matching the push order.
    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            let mut out = io::stdout();
            let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }
    let _guard = TermGuard;

    let result = play_entrance_animation(&mut stdout);
    // _guard drops here and restores the terminal state. Swallow animation
    // errors intentionally — a cosmetic failure must never break boot.
    let _ = result;
    Ok(())
}

fn should_play() -> bool {
    if !io::stdout().is_terminal() {
        return false;
    }
    if std::env::var("X0NE_NO_INTRO").is_ok() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term == "dumb" {
            return false;
        }
    }
    true
}

/// Six-phase cinematic entrance — noise condensing into "X0NE", a white
/// pulse, gradient settle, subtitle fade-in, and a clean fade-out. Total
/// runtime ≈ 2.2 s on a modern terminal; every sleep is tuned once (bump
/// them all in `delay()` if users complain it drags).
///
/// Assumes the caller has already taken care of raw mode + alternate
/// screen if the use-case wants them — `play_boot_intro()` above is the
/// convenience wrapper for the "I just want it at startup" case, while
/// `matrix_mode` calls this directly because it already owns the
/// alternate-screen lifetime for the whole session.
pub fn play_entrance_animation(stdout: &mut io::Stdout) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let cx = cols / 2;
    let cy = rows / 2;
    // Any key hands control to x0ne right away (owner request, 2026-08-16:
    // « if any key is pressed, it must hand over control
    // directly »). Every pause below is a keyboard poll, not a sleep;
    // the key that interrupts is consumed here so it never reaches the
    // prompt. Raw mode is on (see `play_boot_intro`), so `event::read`
    // returns per keystroke. `Skipped` is an ordinary early return.
    let delay = |ms: u64| -> Result<()> {
        if crossterm::event::poll(Duration::from_millis(ms))? {
            let _ = crossterm::event::read()?; // consume, do not forward
            return Err(anyhow::anyhow!(SKIPPED));
        }
        Ok(())
    };

    queue!(stdout, terminal::Clear(ClearType::All), cursor::Hide)?;
    stdout.flush()?;
    delay(400)?;

    // Logo height, to place what comes below it.
    // Derived, not redeclared: the decorative row was dropped on
    // 2026-09-10 and this constant would have followed it wrongly.
    const LOGO_H: u16 = crate::terminal::LOGO.len() as u16;

    // ── X0NE materializes from noise ──────────────────────────────────
    // The same definition as the CLI banner and the TUI header: one
    // brand, one array.
    let logo_lines = crate::terminal::LOGO;
    // The MAXIMUM WIDTH, not the first row's. The first row of the
    // « slant » font is only 11 columns out of 22: centring on it would
    // shift the whole logo five columns to the right.
    let logo_width = logo_lines
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0);
    let logo_x = cx.saturating_sub(logo_width as u16 / 2);

    let noise_chars = ['░', '▒', '▓', '█', '▀', '▄'];
    for pass in 0u32..6 {
        let correctness = pass as f64 / 5.0;
        let brightness = (80 + pass * 35).min(255) as u8;

        for (li, logo_line) in logo_lines.iter().enumerate() {
            let mut cp = 0u16;
            for (ci, ch) in logo_line.chars().enumerate() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
                queue!(stdout, cursor::MoveTo(logo_x + cp, cy + li as u16))?;

                let hash = ((cp as u64 * 7 + li as u64 * 13 + pass as u64 * 3) % 10) as f64 / 10.0;
                let show_real = ch == ' ' || hash < correctness;

                if show_real {
                    // The banner gradient, row by row, capped by the
                    // animation's rising brightness. The former
                    // `li == 0` test separated two rows; with more it
                    // would have given one bright row and four identical.
                    let (_, g, b) = crate::terminal::LOGO_RAMP[li];
                    let color = Color::Rgb {
                        r: 0,
                        g: brightness.min(g),
                        b: brightness.min(b),
                    };
                    queue!(stdout, SetForegroundColor(color))?;
                    queue!(stdout, SetAttribute(Attribute::Bold))?;
                    queue!(stdout, Print(ch))?;
                    queue!(stdout, SetAttribute(Attribute::Reset))?;
                } else if ch != ' ' {
                    let nc = noise_chars[(ci + pass as usize * 3) % noise_chars.len()];
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Rgb {
                            r: brightness / 5,
                            g: brightness / 3,
                            b: brightness / 2
                        })
                    )?;
                    queue!(stdout, Print(nc))?;
                } else {
                    queue!(stdout, Print(' '))?;
                }
                queue!(stdout, ResetColor)?;
                cp += w;
            }
        }
        stdout.flush()?;
        delay(55)?;
    }

    // ── Phase 4: Logo pulse — white flash → settle ────────────────────
    for (li, logo_line) in logo_lines.iter().enumerate() {
        let mut cp = 0u16;
        for ch in logo_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
            if ch != ' ' {
                queue!(stdout, cursor::MoveTo(logo_x + cp, cy + li as u16))?;
                queue!(
                    stdout,
                    SetForegroundColor(Color::Rgb {
                        r: 220,
                        g: 255,
                        b: 255
                    })
                )?;
                queue!(stdout, SetAttribute(Attribute::Bold))?;
                queue!(stdout, Print(ch))?;
                queue!(stdout, SetAttribute(Attribute::Reset))?;
            }
            cp += w;
        }
    }
    queue!(stdout, ResetColor)?;
    stdout.flush()?;
    delay(120)?;

    // DERIVED from the logo, never redeclared. This array was a hand-written
    // `[Color; 2]` when the logo had two rows; it changed on 2026-09-10,
    // `colors[li]` went out of bounds from the third row and the binary died
    // with 101 before displaying anything — the intro plays on the alternate
    // screen, so the panic was invisible.
    //
    // The length now comes from `LOGO`: one more logo row gives one more
    // colour here, without anyone having to think about it.
    let colors: [Color; crate::terminal::LOGO.len()] = std::array::from_fn(|i| {
        let (r, g, b) = crate::terminal::LOGO_RAMP[i];
        Color::Rgb { r, g, b }
    });
    for (li, logo_line) in logo_lines.iter().enumerate() {
        let mut cp = 0u16;
        for ch in logo_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
            if ch != ' ' {
                queue!(stdout, cursor::MoveTo(logo_x + cp, cy + li as u16))?;
                queue!(stdout, SetForegroundColor(colors[li]))?;
                queue!(stdout, SetAttribute(Attribute::Bold))?;
                queue!(stdout, Print(ch))?;
                queue!(stdout, SetAttribute(Attribute::Reset))?;
            }
            cp += w;
        }
    }
    queue!(stdout, ResetColor)?;
    stdout.flush()?;
    delay(300)?;

    // ── Phase 5: Subtitle fades in ────────────────────────────────────
    let subtitle = "Safe Open-Source Agent";
    let sub_w = UnicodeWidthStr::width(subtitle);
    let sub_x = cx.saturating_sub(sub_w as u16 / 2);
    for step in 0u32..5 {
        let b = (25 + step * 18).min(100) as u8;
        queue!(stdout, cursor::MoveTo(sub_x, cy + LOGO_H + 1))?;
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: b / 2,
                g: b,
                b: b + 10
            })
        )?;
        queue!(stdout, Print(subtitle))?;
        stdout.flush()?;
        delay(60)?;
    }
    delay(700)?;

    // ── Phase 6: Fade out — dims and vanishes ─────────────────────────
    for fade in (0..=2).rev() {
        let dim = fade as u8 * 30;
        for (li, logo_line) in logo_lines.iter().enumerate() {
            let mut cp = 0u16;
            for ch in logo_line.chars() {
                let w = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
                if ch != ' ' {
                    queue!(stdout, cursor::MoveTo(logo_x + cp, cy + li as u16))?;
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Rgb {
                            r: 0,
                            g: dim / 2,
                            b: dim
                        })
                    )?;
                    queue!(stdout, Print(ch))?;
                }
                cp += w;
            }
        }
        queue!(stdout, cursor::MoveTo(sub_x, cy + LOGO_H + 1))?;
        queue!(
            stdout,
            SetForegroundColor(Color::Rgb {
                r: dim / 4,
                g: dim / 2,
                b: dim / 2
            })
        )?;
        queue!(stdout, Print(subtitle))?;
        queue!(stdout, ResetColor)?;
        stdout.flush()?;
        delay(80)?;
    }

    delay(80)?;
    queue!(stdout, terminal::Clear(ClearType::All))?;
    stdout.flush()?;
    delay(60)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-TTY (test harness is always piped) must return Ok immediately
    /// without touching the terminal. If this test ever *does* touch the
    /// terminal it'll hang cargo — so Ok is the only acceptable outcome.
    #[test]
    fn play_boot_intro_is_noop_in_non_tty() {
        assert!(play_boot_intro().is_ok());
    }

    /// `X0NE_NO_INTRO=1` must bypass the animation even in a hypothetical
    /// TTY test environment. `should_play` is the single gate.
    #[test]
    fn env_var_disables_intro() {
        // Cargo runs tests in parallel threads, but env vars are process-
        // wide — safe to set here because `should_play` only reads and
        // we don't rely on a specific absent-then-present transition.
        std::env::set_var("X0NE_NO_INTRO", "1");
        assert!(!should_play());
        std::env::remove_var("X0NE_NO_INTRO");
    }

    /// A keypress must end the intro immediately. This exercises the same
    /// `Err(SKIPPED)` path the poll takes: if `play_entrance_animation`
    /// ever stops propagating that error (a `delay(..)` without `?`), the
    /// user is stuck watching the whole animation again — which is the
    /// exact complaint that produced this behaviour.
    #[test]
    fn every_pause_in_the_animation_propagates_a_keypress() {
        let src = include_str!("intro.rs");
        let body = src.split("pub fn play_entrance_animation").nth(1).unwrap();
        let body = body.split("#[cfg(test)]").next().unwrap();
        let bare: Vec<&str> = body
            .lines()
            .filter(|l| l.contains("delay(") && !l.contains("let delay"))
            .filter(|l| !l.trim_start().starts_with("//") && !l.contains(")?;"))
            .collect();
        assert!(
            bare.is_empty(),
            "pauses that swallow the keypress: {bare:?}"
        );
        assert!(
            body.contains("event::poll("),
            "the pause must poll the keyboard, not sleep"
        );
    }

    #[test]
    fn dumb_term_disables_intro() {
        let prev = std::env::var("TERM").ok();
        std::env::set_var("TERM", "dumb");
        assert!(!should_play());
        match prev {
            Some(v) => std::env::set_var("TERM", v),
            None => std::env::remove_var("TERM"),
        }
    }
}
