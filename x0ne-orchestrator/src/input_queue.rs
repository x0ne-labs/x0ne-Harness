//! Between-turn stdin queue + process-wide visible message queue.
//!
//! Two layers:
//!
//! 1. **stdin drain** (this module's original purpose) — `drain_pending_lines`
//!    + `EchoSuppressor` capture lines the user types while the agent is busy
//!    via the kernel's cooked-mode line buffer.
//!
//! 2. **Process-wide queue** — `push_back` / `pop_front` / `queue_len` /
//!    `queue_snapshot` / `queue_clear` / `queue_drop_at` operate on a single
//!    `Mutex<VecDeque<String>>` shared across the binary. The agent loop pops
//!    from the front, drain pushes to the back, the sticky-header renderer
//!    reads `queue_len()` to badge the count, and slash commands inspect /
//!    mutate via the snapshot/clear/drop helpers. Keeps "where's the queue"
//!    in exactly one place so the agent, the UI, and the slash dispatcher
//!    can all reach it without passing handles around.
//!
//! Problem: while the agent is working (streaming an LLM response,
//! dispatching actions, waiting for a command to exit) the REPL is
//! blocked inside the agent turn. Users who typed a new
//! message during that window had no feedback — worse, depending on
//! terminal mode, their input could either vanish on the next
//! `rustyline::readline` call or show up surprisingly as the next
//! prompt.
//!
//! Solution: between turns, peek stdin via `poll(2)` with a zero
//! timeout. If bytes are pending (courtesy of the kernel's cooked-mode
//! line discipline which buffered complete lines while we were busy),
//! drain them directly into a `Vec<String>`. The caller then replays
//! those lines as additional REPL iterations, exactly as if the user
//! had typed them interactively.
//!
//! # Why not a background thread / async reader?
//! Rustyline owns stdin during `readline()`. A second reader would race
//! with it. Doing the drain *between* rustyline calls (and only when
//! the TTY is in cooked mode, i.e. no rustyline active) side-steps the
//! ownership problem entirely.
//!
//! # Platform scope
//! Unix only. On non-unix targets the functions become no-ops; the
//! caller still compiles.

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

// ── Process-wide message queue ───────────────────────────────────────────────

/// Held by every test that fills, clears or pops the queue (`AssistantEnd`
/// pops it): unlocked, one test emptied another's queue (2026-10-25, 1 run
/// in 3 of `tui::`).
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

fn queue() -> &'static Mutex<VecDeque<String>> {
    static Q: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Push a fully-typed line to the back of the queue. Called by the
/// between-turn drain after `drain_pending_lines`.
pub fn push_back(line: String) {
    queue().lock().unwrap().push_back(line);
}

/// Pop the next queued line. The agent loop calls this in place of
/// `rustyline::readline()` whenever the queue isn't empty.
pub fn pop_front() -> Option<String> {
    queue().lock().unwrap().pop_front()
}

/// Number of messages waiting. Cheap, used by the sticky header to
/// render a "📋 N" badge.
pub fn queue_len() -> usize {
    queue().lock().unwrap().len()
}

/// Snapshot of the queued lines in arrival order. Used by `/queue` to
/// list what's pending.
pub fn queue_snapshot() -> Vec<String> {
    queue().lock().unwrap().iter().cloned().collect()
}

/// Drop everything pending, return the number that was removed.
pub fn queue_clear() -> usize {
    let mut q = queue().lock().unwrap();
    let n = q.len();
    q.clear();
    n
}

/// Remove the message at `idx` (1-indexed for user-friendliness; idx=1
/// drops the next one to run). Returns the removed line, or `None` if
/// `idx` is out of range.
pub fn queue_drop_at(idx: usize) -> Option<String> {
    if idx == 0 {
        return None;
    }
    queue().lock().unwrap().remove(idx - 1)
}

// ── Echo suppression during a busy turn ──────────────────────────────────────
//
// Problem: while the agent is streaming (printing tokens, updating the
// spinner via `\r`, etc.), the TTY is still in cooked mode with ECHO
// enabled. Every keystroke the user types collides with the agent's
// output — the kernel echoes the character at the current cursor
// position, then the next `\r`-based spinner refresh paints over it,
// then the next char is echoed past the spinner. Visually: "weird line
// jumps" + lost feedback.
//
// We can't switch to raw mode (loses line buffering = loses the queue
// drain machinery that's already in place at the bottom of this file).
// We can't drop stdin entirely (rustyline owns it before/after the turn).
//
// Solution: keep canonical/cooked mode ON so the kernel still buffers
// complete lines on the fd, but turn ECHO OFF for the duration of the
// turn. The user types blind; the kernel queues lines for the
// between-turn drain; the screen stays clean. Restored on Drop so a
// panic / Ctrl-C / early return can't leave the TTY broken.

#[cfg(unix)]
pub struct EchoSuppressor {
    original: libc::termios,
    /// True if we successfully read the original state — only then do
    /// we attempt to restore. Avoids zeroing a real terminal's flags
    /// when the constructor failed mid-call.
    valid: bool,
}

#[cfg(unix)]
impl EchoSuppressor {
    /// Disable terminal echo (and visual erase) on stdin, keeping
    /// canonical-mode line buffering ON. Returns `None` when stdin is
    /// not a TTY (piped input, CI) — in that case there's no echo to
    /// suppress so there's nothing to do.
    pub fn new() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `isatty` is documented to be safe to call on any
        // valid file descriptor; returns 0 if not a TTY which we
        // gracefully bail on.
        if unsafe { libc::isatty(fd) } == 0 {
            return None;
        }
        // SAFETY: zeroing a `termios` is sound — it's a POD with all
        // primitive fields. We immediately fill it via tcgetattr below
        // before reading any other field.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut modified = original;
        // Clear ECHO + ECHOE/ECHOK/ECHONL — covers the standard echo,
        // backspace-erase, kill-line, and newline-echo. Keep ICANON ON
        // so the kernel still chunks input by lines; that's what the
        // between-turn drain depends on.
        modified.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &modified) } != 0 {
            return None;
        }
        Some(Self {
            original,
            valid: true,
        })
    }

    /// Restore the original termios. Idempotent — calling twice is safe.
    /// Called automatically on Drop; callers can also invoke it
    /// explicitly when the agent finishes early so the prompt
    /// re-appears with normal echo before the next rustyline prompt
    /// installs ITS own termios.
    pub fn restore(&mut self) {
        if !self.valid {
            return;
        }
        let fd = std::io::stdin().as_raw_fd();
        // Best-effort: a failed restore here would leave the TTY in
        // no-echo mode, which is annoying but recoverable via `stty
        // sane`. We don't escalate to a panic because terminal state
        // is a fundamentally racy resource (job control, fg/bg toggle).
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.original) };
        self.valid = false;
    }
}

#[cfg(unix)]
impl Drop for EchoSuppressor {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Re-enable terminal echo for the lifetime of the guard, then put it back.
///
/// The turn-long [`EchoSuppressor`] keeps the operator's keystrokes from
/// colliding with the agent's streaming output — right for streaming, WRONG
/// for a prompt. A confirmation asks the operator to type `1`, `2` or `3`; with
/// echo off they see nothing, the keyboard looks dead, and a mis-keyed answer
/// is invisible. Worse, an empty Enter then fails closed into a denial that
/// looks like a bug (field report, 2026-08-12).
///
/// Scoped so the suppression resumes the moment the answer is read.
#[cfg(unix)]
pub struct EchoRestorer {
    previous: libc::termios,
    valid: bool,
}

#[cfg(unix)]
impl EchoRestorer {
    /// Turn echo back on. `None` when stdin is not a TTY — there is no echo to
    /// restore, and nothing to put back.
    pub fn new() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `isatty` is safe on any valid fd; 0 means not a TTY.
        if unsafe { libc::isatty(fd) } == 0 {
            return None;
        }
        // SAFETY: `termios` is a POD; filled by tcgetattr before any read.
        let mut previous: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut previous) } != 0 {
            return None;
        }
        let mut modified = previous;
        modified.c_lflag |= libc::ECHO | libc::ECHOE | libc::ECHOK;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &modified) } != 0 {
            return None;
        }
        Some(Self {
            previous,
            valid: true,
        })
    }
}

#[cfg(unix)]
impl Drop for EchoRestorer {
    fn drop(&mut self) {
        if !self.valid {
            return;
        }
        let fd = std::io::stdin().as_raw_fd();
        // Best effort, same rationale as EchoSuppressor::restore: terminal
        // state is racy and a failure here is recoverable with `stty sane`.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.previous) };
        self.valid = false;
    }
}

/// No-op on non-Unix: there is no termios to toggle.
#[cfg(not(unix))]
pub struct EchoRestorer;

#[cfg(not(unix))]
impl EchoRestorer {
    pub fn new() -> Option<Self> {
        None
    }
}

#[cfg(not(unix))]
pub struct EchoSuppressor;
#[cfg(not(unix))]
impl EchoSuppressor {
    pub fn new() -> Option<Self> {
        None
    }
    pub fn restore(&mut self) {}
}

/// Non-blockingly check whether `stdin` has at least one byte ready to
/// read. Returns `false` on non-TTY or non-unix builds.
#[cfg(unix)]
pub fn stdin_has_pending() -> bool {
    let fd = std::io::stdin().as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Safety: pollfd is fully initialized; timeout=0 makes this a pure
    // status check (no blocking), and we only inspect POLLIN on a
    // single fd.
    let rc = unsafe { libc::poll(&mut pfd as *mut _, 1, 0) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

#[cfg(not(unix))]
pub fn stdin_has_pending() -> bool {
    false
}

/// Drain every complete line currently buffered on `stdin` and return
/// them in the order they were typed. Trailing partial input (no
/// newline yet) is left on the file descriptor for the next reader.
///
/// Each returned string is already trimmed of its terminating `\n`
/// (and optional `\r`) and of surrounding whitespace. Empty lines are
/// filtered out — they are pure noise from a queueing standpoint.
#[cfg(unix)]
pub fn drain_pending_lines() -> Vec<String> {
    let mut out = Vec::new();
    let mut leftover = String::new();
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();

    // Loop until poll says "no more data" — each pass reads whatever
    // is currently buffered (up to 4 KiB) and splits on '\n'. A
    // dangling fragment without newline is discarded here: it is still
    // in the fd's read buffer AFTER our read, so we cannot push it
    // back. This is an accepted trade-off: a user who was mid-typing
    // when the turn ended sees their prompt re-appear and can finish.
    //
    // NOTE: we pre-flight each iteration with poll() to avoid a
    // blocking read. Without it, `read()` on a TTY in cooked mode
    // blocks until a newline arrives — exactly what we must not do.
    loop {
        if !stdin_has_pending() {
            break;
        }
        let mut buf = [0u8; 4096];
        // Safety: the fd belongs to our stdin handle and the buffer
        // length matches the slice we pass.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        leftover.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
        while let Some(nl) = leftover.find('\n') {
            let line: String = leftover.drain(..=nl).collect();
            let trimmed = line.trim_end_matches(&['\n', '\r'][..]).trim().to_string();
            if !trimmed.is_empty() {
                out.push(trimmed);
            }
        }
    }
    // `leftover` now holds any partial trailing input (no '\n'). We
    // deliberately drop it: there is no portable API to push bytes
    // back onto stdin, and rustyline will re-prompt cleanly.
    //
    // Manual shenanigans with TIOCSTI were considered and rejected:
    // most hardened kernels refuse it for non-CAP_SYS_ADMIN, and some
    // users (rightly) disable it entirely for security.
    let _ = std::io::stdin(); // keep the guard alive until end of scope
    let _ = stdin;
    drop(leftover);
    out
}

#[cfg(not(unix))]
pub fn drain_pending_lines() -> Vec<String> {
    Vec::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn drain_is_empty_when_stdin_idle() {
        // Without any pre-seeded input, drain returns an empty vec.
        // We can't inject stdin data from a unit test without taking
        // over the test harness's own stdin, so we only cover the
        // "no data" branch here; the populated path is exercised
        // manually via the CLI.
        //
        // Notably `stdin_has_pending()` must NOT block even when no
        // data is available — if this test hangs, the poll call is
        // broken.
        let v = drain_pending_lines();
        assert!(v.is_empty());
    }

    // ── Process-wide queue ───────────────────────────────────────────────

    // NOTE: the queue is a process-wide static, so these tests share
    // state. They live in one #[test] fn that resets between scenarios
    // to avoid order-dependent flakes when cargo runs them in parallel.
    #[test]
    fn queue_push_pop_len_snapshot_clear_drop() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Start clean — other tests might have left state.
        queue_clear();
        assert_eq!(queue_len(), 0);
        assert!(queue_snapshot().is_empty());
        assert!(pop_front().is_none());

        push_back("first".into());
        push_back("second".into());
        push_back("third".into());

        assert_eq!(queue_len(), 3);
        assert_eq!(queue_snapshot(), vec!["first", "second", "third"]);

        // FIFO pop
        assert_eq!(pop_front().as_deref(), Some("first"));
        assert_eq!(queue_len(), 2);
        assert_eq!(queue_snapshot(), vec!["second", "third"]);

        // drop_at is 1-indexed; idx=0 is invalid.
        assert!(queue_drop_at(0).is_none());
        assert_eq!(queue_len(), 2);

        // drop the 1st remaining ("second")
        assert_eq!(queue_drop_at(1).as_deref(), Some("second"));
        assert_eq!(queue_len(), 1);
        assert_eq!(queue_snapshot(), vec!["third"]);

        // out-of-range drop is a no-op
        assert!(queue_drop_at(99).is_none());
        assert_eq!(queue_len(), 1);

        // clear returns count + leaves empty
        assert_eq!(queue_clear(), 1);
        assert_eq!(queue_len(), 0);
        assert_eq!(queue_clear(), 0); // idempotent
    }
}
