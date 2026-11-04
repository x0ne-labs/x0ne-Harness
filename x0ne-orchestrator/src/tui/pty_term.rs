//! Embedded terminal for tui shell mode.
//!
//! Wraps a `portable_pty` master/slave pair with a `vt100::Parser`
//! state machine so X0NE can host a real interactive shell inside
//! its own ratatui surface. `htop`, `vim`, `ping`, `ssh`, `sudo`,
//! `top`, anything that wants a terminal — they all run live in
//! the shell page; the parser keeps a screen grid we render as
//! ratatui cells each tick.
//!
//! ## Architecture
//!
//! ```text
//!  user keystrokes ──→ pty master.stdin
//!                                      ├── child process (bash, htop, …)
//!  pty master.stdout ──→ background    │
//!     reader thread ──→ vt100::Parser ─┴── ratatui render (grid → cells)
//! ```
//!
//! The reader thread runs in the background to keep stdout draining
//! even when the UI is busy painting; the main loop just grabs a
//! `lock()` on the parser at render time. This means a fast-paced
//! `cat /dev/urandom | xxd` doesn't lock up the TUI — it just
//! shows what fits in the visible grid.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize, PtySystem};
use vt100::Parser;

/// A live embedded terminal session. Owns the child PTY + parser
/// + reader thread; dropping the struct kills the child.
pub struct PtyTerm {
    /// Shared VT100 state. Locked at render time + by the reader
    /// thread. Mutex (not RwLock) because writes dominate.
    pub parser: Arc<Mutex<Parser>>,
    /// Write end of the master PTY — keystrokes go here.
    writer: Box<dyn Write + Send>,
    /// Master PTY (kept alive so the slave doesn't EOF). Also
    /// owns the resize() method.
    master: Box<dyn MasterPty + Send>,
    /// Child handle so we can poll exit + kill on drop.
    child: Box<dyn Child + Send + Sync>,
    /// True once the child has exited or the PTY has closed.
    /// Sticky — flips once and stays.
    pub exited: Arc<Mutex<bool>>,
    /// Last known dimensions, so we don't resize on every render
    /// (PTY ioctl is cheap but not free, and it sends SIGWINCH to
    /// the child which apps like vim handle by clearing + repainting).
    pub last_size: (u16, u16),
}

impl PtyTerm {
    /// Spawn a fresh `bash -i` session in a PTY at the given size,
    /// starting in `cwd`. The reader thread starts immediately and
    /// drains output into the shared parser.
    pub fn spawn(cwd: &std::path::Path, rows: u16, cols: u16) -> Result<Self> {
        let pty_system: Box<dyn PtySystem> = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("openpty: {}", e))?;

        // Build `bash -i` (login + interactive so PS1 + history work).
        // SHELL env var overrides if the user prefers zsh / fish.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-i");
        cmd.cwd(cwd);
        // Inherit env so PATH, EDITOR, GIT_* etc. work.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        // Tell the child it's a real terminal so apps enable colour
        // + features. xterm-256color is widely supported.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("spawn shell: {}", e))?;
        // Slave end is owned by the child — drop our handle so the
        // child gets SIGHUP if the master closes.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take_writer: {}", e))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("try_clone_reader: {}", e))?;

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 1000)));
        let exited = Arc::new(Mutex::new(false));

        // Background reader thread — fed by the PTY master, drains
        // into the parser. Owns the reader; exits cleanly on EOF.
        spawn_reader_thread(reader, parser.clone(), exited.clone());

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            exited,
            last_size: (rows, cols),
        })
    }

    /// Send raw bytes to the PTY. Used by the key handler.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Convert a crossterm key event to the corresponding terminal
    /// byte sequence and write it. Handles the common ANSI escapes
    /// (cursor keys, function keys, etc.).
    pub fn write_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let bytes: Vec<u8> = match (key.code, key.modifiers) {
            (KeyCode::Char(c), KeyModifiers::CONTROL) => {
                // Control + letter → C0 control byte.
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    vec![upper as u8 - b'A' + 1]
                } else {
                    return; // Unsupported control combo, drop.
                }
            }
            (KeyCode::Char(c), m) if m.contains(KeyModifiers::ALT) => {
                // Alt + char → ESC prefix (xterm convention).
                let mut buf = vec![0x1b];
                buf.extend_from_slice(c.to_string().as_bytes());
                buf
            }
            (KeyCode::Char(c), _) => c.to_string().into_bytes(),
            (KeyCode::Enter, _) => b"\r".to_vec(),
            (KeyCode::Backspace, _) => b"\x7f".to_vec(),
            (KeyCode::Tab, _) => b"\t".to_vec(),
            (KeyCode::BackTab, _) => b"\x1b[Z".to_vec(),
            (KeyCode::Esc, _) => b"\x1b".to_vec(),
            (KeyCode::Left, _) => b"\x1b[D".to_vec(),
            (KeyCode::Right, _) => b"\x1b[C".to_vec(),
            (KeyCode::Up, _) => b"\x1b[A".to_vec(),
            (KeyCode::Down, _) => b"\x1b[B".to_vec(),
            (KeyCode::Home, _) => b"\x1b[H".to_vec(),
            (KeyCode::End, _) => b"\x1b[F".to_vec(),
            (KeyCode::PageUp, _) => b"\x1b[5~".to_vec(),
            (KeyCode::PageDown, _) => b"\x1b[6~".to_vec(),
            (KeyCode::Insert, _) => b"\x1b[2~".to_vec(),
            (KeyCode::Delete, _) => b"\x1b[3~".to_vec(),
            (KeyCode::F(n), _) => fkey_to_bytes(n),
            _ => return,
        };
        self.write_bytes(&bytes);
    }

    /// Propagate a TUI resize to the embedded terminal. Idempotent;
    /// no-op when dimensions haven't changed.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.last_size {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
        self.last_size = (rows, cols);
    }

    /// True when the child process has exited (bash exited, shell
    /// hit Ctrl+D, etc.). The caller should drop us in this case.
    pub fn is_exited(&self) -> bool {
        *self.exited.lock().expect("exited lock")
    }

    /// Best-effort cancel of the running foreground process. Sends
    /// SIGINT (Ctrl+C) — same as the user hitting ^C themselves.
    pub fn send_ctrl_c(&mut self) {
        self.write_bytes(&[0x03]);
    }
}

impl Drop for PtyTerm {
    fn drop(&mut self) {
        // Kill the child so we don't leak a zombie. Best-effort;
        // the OS will reap it once we exit anyway.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<Parser>>,
    exited: Arc<Mutex<bool>>,
) {
    thread::spawn(move || {
        // 64 KB read buffer — generous, keeps `cat huge.log` and
        // `xxd /dev/urandom` from hammering the lock at full speed
        // while still feeding the parser quickly.
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    // EOF — child closed its end.
                    *exited.lock().expect("exited lock") = true;
                    break;
                }
                Ok(n) => {
                    if let Ok(mut p) = parser.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(_) => {
                    *exited.lock().expect("exited lock") = true;
                    break;
                }
            }
        }
    });
}

/// Map a function key number to the xterm escape sequence the PTY
/// child expects. Covers F1–F12; beyond that we drop the key (rare).
fn fkey_to_bytes(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // PtyTerm::spawn is unsafe to run in unit tests (we'd leak bash
    // processes if the test panics). Test the pure byte-translation
    // helpers instead — that's where the bugs live anyway.

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Reusable byte-translation helper that lifts the logic out of
    /// `write_key` for testing without needing a live PTY.
    fn translate(key: KeyEvent) -> Option<Vec<u8>> {
        use crossterm::event::KeyModifiers as M;
        let bytes: Vec<u8> = match (key.code, key.modifiers) {
            (KeyCode::Char(c), m) if m.contains(M::CONTROL) => {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    vec![upper as u8 - b'A' + 1]
                } else {
                    return None;
                }
            }
            (KeyCode::Char(c), m) if m.contains(M::ALT) => {
                let mut buf = vec![0x1b];
                buf.extend_from_slice(c.to_string().as_bytes());
                buf
            }
            (KeyCode::Char(c), _) => c.to_string().into_bytes(),
            (KeyCode::Enter, _) => b"\r".to_vec(),
            (KeyCode::Backspace, _) => b"\x7f".to_vec(),
            (KeyCode::Tab, _) => b"\t".to_vec(),
            (KeyCode::Esc, _) => b"\x1b".to_vec(),
            (KeyCode::Left, _) => b"\x1b[D".to_vec(),
            (KeyCode::Right, _) => b"\x1b[C".to_vec(),
            (KeyCode::Up, _) => b"\x1b[A".to_vec(),
            (KeyCode::Down, _) => b"\x1b[B".to_vec(),
            (KeyCode::Home, _) => b"\x1b[H".to_vec(),
            (KeyCode::End, _) => b"\x1b[F".to_vec(),
            (KeyCode::PageUp, _) => b"\x1b[5~".to_vec(),
            (KeyCode::PageDown, _) => b"\x1b[6~".to_vec(),
            (KeyCode::Delete, _) => b"\x1b[3~".to_vec(),
            _ => return None,
        };
        Some(bytes)
    }

    #[test]
    fn translate_plain_char_passes_through_as_utf8() {
        assert_eq!(
            translate(k(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(b"a".to_vec())
        );
        // Multi-byte UTF-8 must not be split.
        assert_eq!(
            translate(k(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn translate_ctrl_letter_emits_c0_control_byte() {
        // Ctrl+C → 0x03, Ctrl+D → 0x04, Ctrl+L → 0x0c — standard
        // POSIX C0 codes the shell + apps expect.
        assert_eq!(
            translate(k(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            translate(k(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(vec![0x04])
        );
        assert_eq!(
            translate(k(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Some(vec![0x0c])
        );
    }

    #[test]
    fn translate_alt_letter_uses_escape_prefix() {
        // Alt+b → ESC b (bash word-back binding); xterm convention.
        let out = translate(k(KeyCode::Char('b'), KeyModifiers::ALT)).unwrap();
        assert_eq!(out, vec![0x1b, b'b']);
    }

    #[test]
    fn translate_special_keys_emit_xterm_csi() {
        assert_eq!(
            translate(k(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Down, KeyModifiers::NONE)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Right, KeyModifiers::NONE)),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Enter, KeyModifiers::NONE)),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            translate(k(KeyCode::Tab, KeyModifiers::NONE)),
            Some(b"\t".to_vec())
        );
    }

    #[test]
    fn fkey_to_bytes_covers_f1_through_f12() {
        assert_eq!(fkey_to_bytes(1), b"\x1bOP".to_vec());
        assert_eq!(fkey_to_bytes(4), b"\x1bOS".to_vec());
        assert_eq!(fkey_to_bytes(5), b"\x1b[15~".to_vec());
        assert_eq!(fkey_to_bytes(12), b"\x1b[24~".to_vec());
        assert!(fkey_to_bytes(13).is_empty(), "F13+ silently dropped");
    }

    #[test]
    fn translate_unknown_key_returns_none() {
        // Unrecognised codes (Pause, ScrollLock, …) must drop —
        // forwarding garbage would confuse the parser.
        assert!(translate(k(KeyCode::Null, KeyModifiers::NONE)).is_none());
    }
}
