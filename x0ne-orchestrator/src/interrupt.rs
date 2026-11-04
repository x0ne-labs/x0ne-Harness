//! Process-global interrupt flag with double-press semantics.
//!
//! Ctrl+C behavior:
//!   - First press   → `stop_requested()` is true; the agent loop aborts the
//!                     current turn gracefully, injecting a system message so
//!                     the LLM knows its work was interrupted.
//!   - Second press  → `quit_requested()` is true (if within `DOUBLE_PRESS_MS`
//!                     of the first press); the REPL exits the process.
//!
//! SIGINT is process-wide, so we keep a single global instance.
//!
//! - In agent loop (canonical TTY mode), SIGINT reaches the tokio handler,
//!   which calls [`Interrupt::trigger`].
//! - At the rustyline prompt (raw TTY mode), Ctrl+C does **not** generate
//!   SIGINT; rustyline returns `ReadlineError::Interrupted`, and the REPL
//!   calls [`Interrupt::trigger`] manually. Same double-press logic applies.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;

/// Time window for a "double press" — two Ctrl+C presses within this window
/// count as a quit request.
const DOUBLE_PRESS_MS: u64 = 2000;

/// 0 = idle, 1 = stop requested, 2 = quit requested.
const STATE_IDLE: u8 = 0;
const STATE_STOP: u8 = 1;
const STATE_QUIT: u8 = 2;

pub struct Interrupt {
    state: AtomicU8,
    last_press_ms: AtomicU64,
    notify: tokio::sync::Notify,
}

impl Interrupt {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_IDLE),
            last_press_ms: AtomicU64::new(0),
            notify: tokio::sync::Notify::const_new(),
        }
    }

    /// True if any interrupt (stop or quit) has been requested.
    pub fn stop_requested(&self) -> bool {
        self.state.load(Ordering::SeqCst) >= STATE_STOP
    }

    /// True if the user asked to quit the process (second Ctrl+C).
    pub fn quit_requested(&self) -> bool {
        self.state.load(Ordering::SeqCst) >= STATE_QUIT
    }

    /// Clear the flag (call between REPL turns after handling the stop).
    pub fn reset(&self) {
        self.state.store(STATE_IDLE, Ordering::SeqCst);
    }

    /// Record a Ctrl+C press. Returns the new state:
    ///   - `STATE_STOP` → first press (or late second press).
    ///   - `STATE_QUIT` → second press within `DOUBLE_PRESS_MS`.
    ///
    /// The decision is based purely on the elapsed time since the previous
    /// trigger — independent of the current `state` — so callers can safely
    /// call `reset` after handling a STOP without breaking the double-press
    /// window.
    pub fn trigger(&self) -> u8 {
        let now = now_ms();
        let last = self.last_press_ms.swap(now, Ordering::SeqCst);
        let within_window = last != 0 && now.saturating_sub(last) <= DOUBLE_PRESS_MS;

        let new_state = if within_window {
            STATE_QUIT
        } else {
            STATE_STOP
        };
        self.state.store(new_state, Ordering::SeqCst);
        self.notify.notify_waiters();
        new_state
    }

    /// Await the next interrupt. Wakes whenever `trigger` is called.
    pub async fn wait(&self) {
        // Fast path: already interrupted.
        if self.stop_requested() {
            return;
        }
        self.notify.notified().await;
    }
}

static GLOBAL: OnceLock<Interrupt> = OnceLock::new();

/// Access the process-global interrupt handle.
pub fn global() -> &'static Interrupt {
    GLOBAL.get_or_init(Interrupt::new)
}

/// Install a tokio task that calls [`Interrupt::trigger`] on every SIGINT.
/// Safe to call multiple times — only the first call wins.
pub fn install_signal_handler() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                // If we can't listen for SIGINT, bail — rely on default handling.
                break;
            }
            global().trigger();
        }
    });
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_once_requests_stop() {
        let i = Interrupt::new();
        assert!(!i.stop_requested());
        assert!(!i.quit_requested());
        let s = i.trigger();
        assert_eq!(s, STATE_STOP);
        assert!(i.stop_requested());
        assert!(!i.quit_requested());
    }

    #[test]
    fn double_trigger_requests_quit() {
        let i = Interrupt::new();
        i.trigger();
        let s = i.trigger();
        assert_eq!(s, STATE_QUIT);
        assert!(i.quit_requested());
    }

    #[test]
    fn reset_clears_state() {
        let i = Interrupt::new();
        i.trigger();
        assert!(i.stop_requested());
        i.reset();
        assert!(!i.stop_requested());
        assert!(!i.quit_requested());
    }

    #[test]
    fn double_press_survives_reset() {
        // Simulates the rustyline readline flow: first press → REPL handles
        // it → reset(); second press within 2s must still promote to QUIT.
        let i = Interrupt::new();
        i.trigger();
        i.reset();
        assert!(!i.stop_requested(), "reset should clear state");
        let s = i.trigger();
        assert_eq!(
            s, STATE_QUIT,
            "second press within window must quit even after reset"
        );
    }

    #[test]
    fn late_second_press_does_not_quit() {
        let i = Interrupt::new();
        i.trigger();
        // Fake a very old previous press by rewriting last_press_ms.
        i.last_press_ms.store(
            now_ms().saturating_sub(DOUBLE_PRESS_MS + 1000),
            Ordering::SeqCst,
        );
        // After an explicit reset (simulating what the REPL does between turns),
        // a second press should again only request stop.
        i.reset();
        let s = i.trigger();
        assert_eq!(s, STATE_STOP);
    }

    #[tokio::test]
    async fn wait_returns_after_trigger() {
        let i = std::sync::Arc::new(Interrupt::new());
        let i2 = i.clone();
        let h = tokio::spawn(async move { i2.wait().await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        i.trigger();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), h)
            .await
            .expect("wait should return promptly after trigger");
    }

    #[tokio::test]
    async fn wait_fast_path_when_already_interrupted() {
        let i = Interrupt::new();
        i.trigger();
        // Should not hang — already interrupted.
        tokio::time::timeout(std::time::Duration::from_millis(500), i.wait())
            .await
            .expect("wait should be instant when already interrupted");
    }
}
