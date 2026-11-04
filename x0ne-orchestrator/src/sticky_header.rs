//! Pinned CLI header via the VT100 scrolling region (DECSTBM).
//!
//! A `StickyHeader` freezes the first `header_lines` rows of the terminal
//! so the X0NE banner + status line stay visible while the rest of the
//! output scrolls underneath. On `Drop` (normal exit, panic, Ctrl+C) the
//! scrolling region is reset and the cursor is moved to the bottom so the
//! shell prompt renders cleanly.
//!
//! # Requirements
//! * stdout must be a TTY (`crossterm::terminal::size` succeeds).
//! * The terminal must have enough rows to leave a reasonable scroll area
//!   (`rows >= header_lines * 3`); otherwise installation is skipped.
//!
//! # Escape sequences used
//! * `CSI top;bottom r` — set scrolling region (DECSTBM).
//! * `CSI row;col H`    — move cursor.
//! * `CSI r`            — reset scrolling region to the full screen.
//!
//! # What we deliberately do NOT do
//! * Handle `SIGWINCH`. A resize leaves the region on its original rows.
//!   Good enough for typical sessions; users can `/clear` to redraw.
//! * Use the alternate screen. That would clobber scrollback, which we
//!   want to preserve (the whole point is that chronology stays readable).

use std::io::Write;

/// Drop guard that resets the scrolling region when it goes out of scope.
///
/// `active == false` means installation was skipped (non-TTY, tiny
/// terminal, or explicitly disabled) — the Drop impl becomes a no-op.
pub struct StickyHeader {
    active: bool,
}

impl StickyHeader {
    /// Pin the top `header_lines` rows. Returns a guard; keep it alive
    /// for the duration of the interactive session.
    ///
    /// **Opt-in**: must be enabled explicitly with `X0NE_STICKY_HEADER=1`.
    ///
    /// # Why opt-in
    /// DECSTBM pins lines *on the live screen only* — lines outside the
    /// scroll region are NOT re-emitted into the user's scrollback when
    /// they wheel-scroll backwards. So while the header appears "fixed"
    /// during normal forward output, it disappears the moment the user
    /// scrolls to review history. That surprise is worse than not having
    /// a pinned header at all, so we default to the natural line-mode
    /// behaviour (header scrolls with output, scrollback works cleanly)
    /// and expose the pinning as an opt-in for users who know the
    /// trade-off.
    ///
    /// Silently becomes a no-op in the following cases:
    /// * `X0NE_STICKY_HEADER` is unset or not equal to `"1"`.
    /// * stdout is not a TTY.
    /// * Terminal has fewer than `header_lines * 3` rows (we refuse to
    ///   shrink the scroll area to a painful slit).
    pub fn install(header_lines: u16) -> Self {
        if std::env::var("X0NE_STICKY_HEADER").as_deref() != Ok("1") {
            return Self { active: false };
        }
        let (_, rows) = match crossterm::terminal::size() {
            Ok(sz) => sz,
            Err(_) => return Self { active: false },
        };
        if rows < header_lines.saturating_mul(3) || header_lines == 0 {
            return Self { active: false };
        }

        let top = header_lines + 1;
        let bottom = rows;
        let mut out = std::io::stdout();
        let _ = write!(
            out,
            "\x1b[{top};{bottom}r\x1b[{bottom};1H",
            top = top,
            bottom = bottom,
        );
        let _ = out.flush();
        Self { active: true }
    }
}

impl Drop for StickyHeader {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut out = std::io::stdout();
        // Reset scroll region to full screen; move cursor to the last
        // visible row so the subsequent shell prompt starts on a fresh
        // line rather than inside the (now-defunct) frozen header.
        let _ = write!(out, "\x1b[r");
        if let Ok((_, rows)) = crossterm::terminal::size() {
            let _ = write!(out, "\x1b[{};1H", rows);
        }
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env-var touching tests: `std::env::set_var`/`remove_var`
    // mutate the process-wide environment and Rust runs tests in
    // parallel by default. Without this lock two cases racing on
    // `X0NE_STICKY_HEADER` will flakily see each other's writes.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn install_is_noop_when_env_not_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("X0NE_STICKY_HEADER").ok();
        // Safety: single-threaded section under ENV_LOCK, so no other
        // test observes the transient mutation.
        unsafe {
            std::env::remove_var("X0NE_STICKY_HEADER");
        }
        let g = StickyHeader::install(5);
        assert!(!g.active);
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("X0NE_STICKY_HEADER", v);
            }
        }
    }

    #[test]
    fn install_is_noop_when_env_is_anything_other_than_one() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("X0NE_STICKY_HEADER").ok();
        for v in ["0", "true", "yes", "on", ""] {
            unsafe {
                std::env::set_var("X0NE_STICKY_HEADER", v);
            }
            let g = StickyHeader::install(5);
            assert!(!g.active, "value `{}` should be treated as disabled", v);
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var("X0NE_STICKY_HEADER", v),
                None => std::env::remove_var("X0NE_STICKY_HEADER"),
            }
        }
    }
}
