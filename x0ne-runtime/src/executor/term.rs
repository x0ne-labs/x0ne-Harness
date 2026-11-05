//! Named terminals (Term* actions): tmux on a private socket, native PTY when
//! tmux is absent. Every terminal is recorded (asciicast v2) and every
//! TermOpen / TermSend / TermResize / TermClose is journaled.
//!
//! SECURITY, in one place:
//! - the tmux server is the runtime's own child, on a socket in a private 0700
//!   directory, with `-f` on a private config (no user `~/.tmux.conf`), never
//!   the operator's tmux; it dies with the runtime (PDEATHSIG);
//! - the program in a terminal is spawned through `isolation::plan` like
//!   `RunCommand`, and the socket directory is hidden from it (bwrap tmpfs), so
//!   it cannot ask the unconfined server for a new window;
//! - tmux splits its argv on a trailing `;` (cmd-parse.c): every agent string
//!   that reaches a tmux argv goes through `tmux_arg`; tmux expands no format in
//!   new-session's command or in `send-keys -l` (measured with tmux 3.3a).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::action::Action;
use crate::config::AppConfig;
use crate::handles::SharedHandles;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

pub const DEFAULT_COLS: u16 = 200;
pub const DEFAULT_ROWS: u16 = 50;
const DEFAULT_WAIT_MS: u64 = 10_000;
const HISTORY_LIMIT: usize = 10_000;
const POLL: Duration = Duration::from_millis(80);
/// What one `display-message` reads of a tmux pane, in [`PaneInfo`] order.
const PANE_FMT: &str =
    "#{cursor_x} #{cursor_y} #{pane_dead} #{pane_dead_status} #{pane_dead_signal}";
/// How long a view waits for tmux to reap a dead pane's child.
const REAP_WAIT: Duration = Duration::from_secs(1);
const REAP_POLL: Duration = Duration::from_millis(10);
/// How long a PTY exit waits for the output to drain to EOF. A
/// descendant that keeps the terminal open (setsid, SIGHUP ignored)
/// withholds EOF: the exit is then reported this much later.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Fixed locations, never `$PATH`: same rule as bwrap (`isolation.rs`).
const TMUX_LOCATIONS: &[&str] = &["/usr/bin/tmux", "/usr/local/bin/tmux", "/bin/tmux"];

// ── Small pure helpers ───────────────────────────────────────────────────────

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// tmux ends a command at an argument ending in `;` and turns a final `\;`
/// into `;` (cmd-parse.c, `cmd_parse_from_arguments`). Measured 2026-10-06:
/// `end;` reached the program as `end`. This keeps any string intact.
pub fn tmux_arg(s: &str) -> String {
    match s.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => s.to_string(),
    }
}

/// A named key: its tmux name and the bytes a terminal would send.
pub fn key(name: &str) -> Option<(String, Vec<u8>)> {
    let fixed: &[(&[&str], &str, &[u8])] = &[
        (&["Enter", "Return"], "Enter", b"\r"),
        (&["Tab"], "Tab", b"\t"),
        (&["BTab", "S-Tab"], "BTab", b"\x1b[Z"),
        (&["Escape", "Esc"], "Escape", b"\x1b"),
        (&["BSpace", "Backspace"], "BSpace", b"\x7f"),
        (&["Space"], "Space", b" "),
        (&["Up"], "Up", b"\x1b[A"),
        (&["Down"], "Down", b"\x1b[B"),
        (&["Right"], "Right", b"\x1b[C"),
        (&["Left"], "Left", b"\x1b[D"),
        (&["Home"], "Home", b"\x1b[H"),
        (&["End"], "End", b"\x1b[F"),
        (&["PageUp", "PgUp", "PPage"], "PPage", b"\x1b[5~"),
        (&["PageDown", "PgDn", "NPage"], "NPage", b"\x1b[6~"),
        (&["Insert", "IC"], "IC", b"\x1b[2~"),
        (&["Delete", "DC"], "DC", b"\x1b[3~"),
        (&["F1"], "F1", b"\x1bOP"),
        (&["F2"], "F2", b"\x1bOQ"),
        (&["F3"], "F3", b"\x1bOR"),
        (&["F4"], "F4", b"\x1bOS"),
        (&["F5"], "F5", b"\x1b[15~"),
        (&["F6"], "F6", b"\x1b[17~"),
        (&["F7"], "F7", b"\x1b[18~"),
        (&["F8"], "F8", b"\x1b[19~"),
        (&["F9"], "F9", b"\x1b[20~"),
        (&["F10"], "F10", b"\x1b[21~"),
        (&["F11"], "F11", b"\x1b[23~"),
        (&["F12"], "F12", b"\x1b[24~"),
    ];
    for (aliases, tmux, bytes) in fixed {
        if aliases.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            return Some((tmux.to_string(), bytes.to_vec()));
        }
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next(), chars.next(), chars.next()) {
        (Some('C' | 'c'), Some('-'), Some(c), None) if c.is_ascii_alphabetic() => {
            let c = c.to_ascii_lowercase();
            Some((format!("C-{c}"), vec![(c as u8) & 0x1f]))
        }
        (Some('M' | 'm'), Some('-'), Some(c), None) if c.is_ascii_graphic() => {
            Some((format!("M-{c}"), vec![0x1b, c as u8]))
        }
        _ => None,
    }
}

/// Remove ANSI/VT escape sequences and stray control characters.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                Some('[') => {
                    for d in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    while let Some(d) = it.next() {
                        if d == '\u{7}' {
                            break;
                        }
                        if d == '\u{1b}' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('#') => {
                    it.next();
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Keep the last `max` bytes on a char boundary. Returns (text, truncated).
fn keep_tail(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    (format!("[... {cut} bytes cut]\n{}", &s[cut..]), true)
}

fn trim_rows(rows: &[String]) -> String {
    let mut v: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    while v.last().map(|l| l.is_empty()).unwrap_or(false) {
        v.pop();
    }
    v.join("\n")
}

fn diff_rows(old: &[String], new: &[String]) -> Vec<Value> {
    new.iter()
        .enumerate()
        .filter(|(i, row)| old.get(*i).map(|o| o.trim_end()).unwrap_or("") != row.trim_end())
        .map(|(i, row)| json!({"row": i, "text": row.trim_end()}))
        .collect()
}

fn ok(op: &str, summary: String, data: Value) -> ActionResult {
    ActionResult::ok(
        op,
        ActionOutput::Term {
            op: op.to_string(),
            summary,
            data,
        },
        0,
    )
}

fn fail(op: &str, kind: ErrorKind, msg: impl Into<String>) -> ActionResult {
    ActionResult::err(op, kind, msg.into(), 0)
}

// ── Recording and journal ────────────────────────────────────────────────────

/// One asciicast v2 file. Output, input and resizes, timestamped.
pub struct Cast {
    file: Mutex<std::fs::File>,
    carry: Mutex<Vec<u8>>,
    start: Instant,
    pub path: PathBuf,
}

impl Cast {
    fn create(
        path: PathBuf,
        cols: u16,
        rows: u16,
        name: &str,
        argv: &[String],
    ) -> std::io::Result<Cast> {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let header = json!({
            "version": 2, "width": cols, "height": rows,
            "timestamp": chrono::Utc::now().timestamp(),
            "title": name, "command": argv.join(" "),
        });
        writeln!(f, "{header}")?;
        Ok(Cast {
            file: Mutex::new(f),
            carry: Mutex::new(Vec::new()),
            start: Instant::now(),
            path,
        })
    }

    fn event(&self, kind: &str, data: &str) {
        let t = (self.start.elapsed().as_secs_f64() * 1e6).round() / 1e6;
        let line = json!([t, kind, data]).to_string();
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Bytes may split a UTF-8 sequence: the incomplete tail waits.
    fn output(&self, bytes: &[u8]) {
        let mut carry = self.carry.lock().unwrap_or_else(|e| e.into_inner());
        carry.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => carry.len(),
        };
        let text = String::from_utf8_lossy(&carry[..valid]).into_owned();
        carry.drain(..valid);
        drop(carry);
        if !text.is_empty() {
            self.event("o", &text);
        }
    }
}

/// `audit.jsonl` of one runtime run: what was opened and typed, in order.
/// Masked, then hash-chained (`x0ne audit verify`); sealed at close-all when
/// `~/.x0ne/keys/journal-ed25519` exists.
struct Journal {
    file: Mutex<x0ne_audit::chain::ChainedFile>,
}

impl Journal {
    fn open(path: &Path) -> std::io::Result<Self> {
        let session = path
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("term");
        Ok(Self {
            file: Mutex::new(x0ne_audit::chain::ChainedFile::open(path, session)?),
        })
    }

    fn write(&self, entry: Value) {
        let mut e = entry;
        e["ts"] = json!(chrono::Utc::now().to_rfc3339());
        // Masked before the trace too: the trace is a second copy of the line.
        x0ne_audit::redact::redact_value(&mut e);
        tracing::info!(target: "x0ne::term", "{e}");
        if let Ok(mut f) = self.file.lock() {
            if let Err(err) = f.write(e) {
                tracing::warn!("terminal audit journal: {err}");
            }
        }
    }

    fn seal(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.seal();
        }
    }
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// ── Output pump: a FIFO (tmux pipe-pane) or a PTY master, into the cast ──────

struct Pump {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Pump {
    fn idle() -> Pump {
        Pump {
            stop: Arc::new(AtomicBool::new(true)),
            thread: None,
        }
    }

    /// Read `fd` until `stop`, then until 100 ms without data, polled so a
    /// silent writer cannot wedge it. `sink` gets every chunk; `on_eof` runs
    /// once, at a PTY's end of file (slave closed, all read) or at the stop.
    fn start(
        fd: std::os::fd::OwnedFd,
        sink: impl Fn(&[u8]) + Send + 'static,
        on_eof: impl FnOnce() + Send + 'static,
    ) -> Pump {
        use std::os::fd::AsRawFd;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut on_eof = Some(on_eof);
            let mut eof = || {
                if let Some(f) = on_eof.take() {
                    f();
                }
            };
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, owned by this frame.
                let n = unsafe { libc::poll(&mut pfd, 1, 100) };
                if n > 0 && pfd.revents & libc::POLLIN != 0 {
                    // SAFETY: reading into a buffer we own, length bounded.
                    let r =
                        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if r > 0 {
                        sink(&buf[..r as usize]);
                        continue;
                    }
                    // A PTY master reads EIO only once the closed slave's
                    // pending bytes were flushed and read (Linux n_tty_read).
                    if r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EIO) {
                        eof();
                    }
                    if r == 0 || flag.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                if n > 0 && pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    // PTY: the slave is closed and, without POLLIN, the kernel
                    // flushed it and found nothing left (Linux n_tty_poll).
                    eof();
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if flag.load(Ordering::SeqCst) {
                    break;
                }
            }
            eof();
        });
        Pump {
            stop,
            thread: Some(thread),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ── A minimal screen for the PTY backend ─────────────────────────────────────

/// Not a VT emulator: printable text, CR/LF/BS/TAB, cursor moves and erases.
/// Enough for REPLs, shells and prompts; full-screen TUIs want tmux.
struct Screen {
    lines: std::collections::VecDeque<Vec<char>>,
    row: usize,
    col: usize,
    cols: usize,
    rows: usize,
    raw: std::collections::VecDeque<u8>,
    carry: Vec<u8>,
    esc: Option<String>,
}

const RAW_KEEP: usize = 1 << 20;

impl Screen {
    fn new(cols: u16, rows: u16) -> Screen {
        let mut lines = std::collections::VecDeque::new();
        lines.push_back(Vec::new());
        Screen {
            lines,
            row: 0,
            col: 0,
            cols: cols as usize,
            rows: rows as usize,
            raw: std::collections::VecDeque::new(),
            carry: Vec::new(),
            esc: None,
        }
    }

    fn top(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    fn ensure_row(&mut self) {
        while self.row >= self.lines.len() {
            self.lines.push_back(Vec::new());
        }
        while self.lines.len() > HISTORY_LIMIT {
            self.lines.pop_front();
            self.row = self.row.saturating_sub(1);
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.raw.extend(bytes);
        while self.raw.len() > RAW_KEEP {
            self.raw.pop_front();
        }
        self.carry.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.carry) {
            Ok(_) => self.carry.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.carry.len(),
        };
        let text = String::from_utf8_lossy(&self.carry[..valid]).into_owned();
        self.carry.drain(..valid);
        for c in text.chars() {
            self.put(c);
        }
    }

    fn csi(&mut self, seq: &str) {
        let Some(fin) = seq.chars().last() else {
            return;
        };
        let body = &seq[..seq.len() - fin.len_utf8()];
        if body.starts_with('?') || body.starts_with('>') || body.starts_with('=') {
            return;
        }
        let nums: Vec<usize> = body.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let n = nums.first().copied().unwrap_or(0).max(1);
        let top = self.top();
        let last_row = top + self.rows.saturating_sub(1);
        match fin {
            'A' => self.row = self.row.saturating_sub(n).max(top),
            'B' => self.row = (self.row + n).min(last_row),
            'C' => self.col = (self.col + n).min(self.cols.saturating_sub(1)),
            'D' => self.col = self.col.saturating_sub(n),
            'G' => self.col = (n - 1).min(self.cols.saturating_sub(1)),
            'd' => self.row = (top + n - 1).min(last_row),
            'H' | 'f' => {
                let r = nums.first().copied().unwrap_or(1).max(1) - 1;
                let c = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                self.row = (top + r).min(last_row);
                self.col = c.min(self.cols.saturating_sub(1));
            }
            'K' => {
                self.ensure_row();
                let mode = nums.first().copied().unwrap_or(0);
                let col = self.col;
                let line = &mut self.lines[self.row];
                match mode {
                    0 => line.truncate(col),
                    1 => line.iter_mut().take(col + 1).for_each(|c| *c = ' '),
                    _ => line.clear(),
                }
            }
            'J' => {
                let mode = nums.first().copied().unwrap_or(0);
                self.ensure_row();
                if mode == 2 || mode == 3 {
                    for r in top..self.lines.len() {
                        self.lines[r].clear();
                    }
                } else if mode == 0 {
                    let col = self.col;
                    self.lines[self.row].truncate(col);
                    for r in self.row + 1..self.lines.len() {
                        self.lines[r].clear();
                    }
                }
            }
            _ => {}
        }
    }

    fn put(&mut self, c: char) {
        if let Some(esc) = self.esc.as_mut() {
            esc.push(c);
            let done = match esc.chars().next() {
                Some('[') => esc.len() > 1 && ('\u{40}'..='\u{7e}').contains(&c),
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    c == '\u{7}' || esc.ends_with("\u{1b}\\")
                }
                Some('(') | Some(')') | Some('#') => esc.chars().count() >= 2,
                _ => true,
            };
            if done {
                let seq = self.esc.take().unwrap_or_default();
                if let Some(body) = seq.strip_prefix('[') {
                    self.csi(body);
                }
            }
            return;
        }
        match c {
            '\u{1b}' => self.esc = Some(String::new()),
            '\r' => self.col = 0,
            '\n' => {
                self.row += 1;
                self.ensure_row();
            }
            '\u{8}' => self.col = self.col.saturating_sub(1),
            '\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols.saturating_sub(1)),
            c if c.is_control() => {}
            c => {
                if self.col >= self.cols {
                    self.col = 0;
                    self.row += 1;
                }
                self.ensure_row();
                let col = self.col;
                let line = &mut self.lines[self.row];
                while line.len() < col {
                    line.push(' ');
                }
                if col < line.len() {
                    line[col] = c;
                } else {
                    line.push(c);
                }
                self.col += 1;
            }
        }
    }

    /// Rows from `start` to the end, padded to a full screen.
    fn rows_from(&self, start: usize) -> Vec<String> {
        let mut v: Vec<String> = self
            .lines
            .iter()
            .skip(start)
            .map(|l| l.iter().collect())
            .collect();
        let want = self.lines.len().saturating_sub(start).max(self.rows);
        while v.len() < want {
            v.push(String::new());
        }
        v
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols as usize;
        self.rows = rows as usize;
    }
}

// ── Backends ─────────────────────────────────────────────────────────────────

struct TmuxServer {
    bin: PathBuf,
    dir: tempfile::TempDir,
    sock: PathBuf,
    pid: u32,
}

impl TmuxServer {
    fn find() -> Option<PathBuf> {
        TMUX_LOCATIONS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    }

    fn start(bin: PathBuf) -> Result<TmuxServer, String> {
        let dir = tempfile::Builder::new()
            .prefix("x0ne-term-")
            .tempdir()
            .map_err(|e| format!("tmux socket dir: {e}"))?;
        let conf = dir.path().join("tmux.conf");
        // `set -g window-size manual` here crashed the tmux 3.3a server on the
        // first new-session (measured 2026-10-06); resize-window sets it per window.
        std::fs::write(
            &conf,
            format!(
                "set -g remain-on-exit on\nset -g status off\nset -g history-limit {HISTORY_LIMIT}\n\
                 set -g escape-time 10\n"
            ),
        )
        .map_err(|e| format!("tmux config: {e}"))?;
        let sock = dir.path().join("sock");
        let (tx, rx) = std::sync::mpsc::channel();
        let (b, s, c) = (bin.clone(), sock.clone(), conf.clone());
        // The thread lives as long as the server: PDEATHSIG fires on the death
        // of the spawning THREAD, so it must not be a pool thread.
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new(&b);
            cmd.arg("-u")
                .arg("-D")
                .arg("-S")
                .arg(&s)
                .arg("-f")
                .arg(&c)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::child_env::harden(&mut cmd);
            die_with_parent(&mut cmd, libc::SIGTERM);
            match cmd.spawn() {
                Ok(mut child) => {
                    let _ = tx.send(Ok(child.id()));
                    let _ = child.wait();
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        });
        let pid = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("tmux server: {e}"))??;
        let t0 = Instant::now();
        while !sock.exists() {
            if t0.elapsed() > Duration::from_secs(5) {
                // SAFETY: signalling our own child by pid.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                return Err("tmux server did not create its socket".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(TmuxServer {
            bin,
            dir,
            sock,
            pid,
        })
    }

    async fn run(&self, args: &[String], cwd: Option<&Path>) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-u").arg("-S").arg(&self.sock).args(args);
        crate::child_env::harden(&mut cmd);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        cmd.stdin(std::process::Stdio::null());
        let out = cmd.output().await.map_err(|e| format!("tmux: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn kill(&self) {
        // SAFETY: our own child; SIGTERM makes tmux HUP every pane.
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
    }

    fn reap(&self) {
        // SAFETY: our own child; a spare SIGCHLD only makes tmux call waitpid.
        unsafe { libc::kill(self.pid as i32, libc::SIGCHLD) };
    }
}

/// One look at a tmux pane, parsed from [`PANE_FMT`].
#[derive(Debug)]
struct PaneInfo {
    cursor: Option<(u64, u64)>,
    dead: bool,
    status: Option<i32>,
    /// A number on Linux, a name where libc has `sys_signame` (macOS).
    signaled: bool,
}

impl PaneInfo {
    fn parse(out: &str) -> PaneInfo {
        let f: Vec<&str> = out.trim_end_matches('\n').split(' ').collect();
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
        PaneInfo {
            cursor: match (num(0), num(1)) {
                (Some(x), Some(y)) => Some((x as u64, y as u64)),
                _ => None,
            },
            dead: num(2) == Some(1),
            status: num(3).map(|v| v as i32),
            signaled: f.get(4).is_some_and(|s| !s.is_empty()),
        }
    }

    /// Dead, but tmux has not reaped the child: no exit status, no signal.
    fn unreaped(&self) -> bool {
        self.dead && self.status.is_none() && !self.signaled
    }
}

/// tmux 3.4 with libutempter (ubuntu-24.04) loses the SIGCHLD of a child that exits during
/// `utempter_remove_record` (tmux issue 311): 124 of 200 panes dead without status, 2026-11-05.
/// A SIGCHLD to the server reaps them; past `wait` the pane is reported without status.
async fn settle<L, F>(mut look: L, nudge: impl Fn(), wait: Duration) -> Result<PaneInfo, String>
where
    L: FnMut() -> F,
    F: std::future::Future<Output = Result<String, String>>,
{
    let t0 = Instant::now();
    let mut p = PaneInfo::parse(&look().await?);
    while p.unreaped() && t0.elapsed() < wait {
        nudge();
        tokio::time::sleep(REAP_POLL).await;
        p = PaneInfo::parse(&look().await?);
    }
    Ok(p)
}

#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut std::process::Command, sig: i32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: async-signal-safe prctl between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, sig as libc::c_ulong);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut std::process::Command, _sig: i32) {}

/// Test-only pauses at the two exit races of a PTY. Empty outside `cfg(test)`.
#[derive(Default, Clone)]
struct RaceHooks {
    /// In `PtyTerm::view`, between its two reads.
    #[cfg(test)]
    between_reads: Option<Arc<dyn Fn(&PtyTerm) + Send + Sync>>,
    /// In the PTY pump, before a chunk reaches the screen.
    #[cfg(test)]
    before_feed: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl RaceHooks {
    #[cfg_attr(not(test), allow(clippy::unused_self, unused_variables))]
    fn between_reads(&self, p: &PtyTerm) {
        #[cfg(test)]
        if let Some(f) = &self.between_reads {
            f(p);
        }
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn before_feed(&self) {
        #[cfg(test)]
        if let Some(f) = &self.before_feed {
            f();
        }
    }
}

struct PtyTerm {
    master: Mutex<std::fs::File>,
    screen: Arc<Mutex<Screen>>,
    pid: u32,
    status: Arc<Mutex<Option<i32>>>,
    hooks: RaceHooks,
}

impl PtyTerm {
    fn spawn(
        program: &OsString,
        args: &[OsString],
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
        cast: Arc<Cast>,
        hooks: RaceHooks,
    ) -> Result<(PtyTerm, Pump), String> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: out-pointers to two ints and a winsize we own. Apple's openpty
        // takes *mut termios and winsize, Linux's *const: *mut coerces to both.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut::<libc::termios>(),
                &raw mut ws,
            )
        };
        if rc != 0 {
            return Err(format!("openpty: {}", std::io::Error::last_os_error()));
        }
        // openpty sets no CLOEXEC: a slave inherited by a concurrent spawn would
        // hold the terminal open and withhold the EOF an exit waits for.
        for fd in [master, slave] {
            // SAFETY: F_SETFD on the two fds openpty just returned.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // SAFETY: openpty returned two fresh fds we now own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let dup = |fd: &OwnedFd| fd.try_clone().map_err(|e| format!("dup: {e}"));
        let reader = dup(&master)?;
        let writer = dup(&master)?;
        drop(master);
        let (s_in, s_out, s_err) = (dup(&slave)?, dup(&slave)?, dup(&slave)?);

        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::from(s_in))
            .stdout(std::process::Stdio::from(s_out))
            .stderr(std::process::Stdio::from(s_err));
        crate::child_env::harden(&mut cmd);
        cmd.env("TERM", "xterm-256color");
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid/ioctl/prctl are async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    #[cfg(target_os = "linux")]
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                    Ok(())
                });
            }
        }
        let status = Arc::new(Mutex::new(None));
        let st = status.clone();
        // Set by the pump at EOF: every byte the program wrote is on the screen.
        let drained = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let dr = drained.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // Same rule as the tmux server: the spawning thread outlives the child.
        std::thread::spawn(move || match cmd.spawn() {
            Ok(mut child) => {
                drop(cmd);
                let _ = tx.send(Ok(child.id()));
                let code = match child.wait() {
                    Ok(s) => {
                        use std::os::unix::process::ExitStatusExt;
                        s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
                    }
                    Err(_) => -1,
                };
                // Reaped before the pump read the last bytes: 2 of 27 runs of
                // the y/N test under load, 2026-10-27. Published after EOF.
                let (done, cv) = &*dr;
                let done = done.lock().unwrap_or_else(|e| e.into_inner());
                drop(cv.wait_timeout_while(done, DRAIN_GRACE, |d| !*d));
                *st.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
            }
        });
        let pid = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("spawn: {e}"))??;
        drop(slave);
        let screen = Arc::new(Mutex::new(Screen::new(cols, rows)));
        let sc = screen.clone();
        let h = hooks.clone();
        let pump = Pump::start(
            reader,
            move |b| {
                h.before_feed();
                sc.lock().unwrap_or_else(|e| e.into_inner()).feed(b);
                cast.output(b);
            },
            move || {
                let (done, cv) = &*drained;
                *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cv.notify_all();
            },
        );
        Ok((
            PtyTerm {
                master: Mutex::new(std::fs::File::from(writer)),
                screen,
                pid,
                status,
                hooks,
            },
            pump,
        ))
    }

    /// Status first, screen second: the status is published once the
    /// screen holds the output up to EOF, so `exited` brings the last output.
    fn view(&self, history: usize) -> View {
        let status = *self.status.lock().unwrap_or_else(|e| e.into_inner());
        self.hooks.between_reads(self);
        let sc = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        let top = sc.top();
        let start = top.saturating_sub(history);
        let mut all = sc.rows_from(start);
        let split = all.len().saturating_sub(sc.rows);
        let mut rows = all.split_off(split);
        let cursor = Some((sc.col as u64, sc.row.saturating_sub(top) as u64));
        pad_to_cursor(&mut rows, cursor);
        drop(sc);
        View {
            rows,
            history: all,
            cursor,
            dead: status.is_some(),
            status,
        }
    }

    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut m = self.master.lock().unwrap_or_else(|e| e.into_inner());
        m.write_all(bytes)?;
        m.flush()
    }

    fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let m = self.master.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: TIOCSWINSZ on our master fd with a winsize we own.
        if unsafe { libc::ioctl(m.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(cols, rows);
        Ok(())
    }

    fn kill(&self) {
        // SAFETY: the child is a session leader (setsid): its group is its pid.
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
            libc::kill(self.pid as i32, libc::SIGKILL);
        }
    }
}

enum Backend {
    Tmux(Arc<TmuxServer>),
    Pty(Arc<PtyTerm>),
}

struct Term {
    argv: Vec<String>,
    cols: u16,
    rows: u16,
    pid: u32,
    backend: Backend,
    cast: Arc<Cast>,
    pump: Pump,
    last: Vec<String>,
}

/// tmux trims trailing blanks: `[y/N] ` would read `[y/N]`. Text up to the
/// cursor is kept so a prompt pattern ending in a space matches.
fn pad_to_cursor(rows: &mut [String], cursor: Option<(u64, u64)>) {
    if let Some((x, y)) = cursor {
        if let Some(r) = rows.get_mut(y as usize) {
            let n = r.chars().count();
            if (x as usize) > n {
                r.extend(std::iter::repeat_n(' ', x as usize - n));
            }
        }
    }
}

/// What one look at a terminal returns.
struct View {
    rows: Vec<String>,
    history: Vec<String>,
    cursor: Option<(u64, u64)>,
    dead: bool,
    status: Option<i32>,
}

impl Term {
    fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Tmux(_) => "tmux",
            Backend::Pty(_) => "pty",
        }
    }

    fn target(name: &str) -> String {
        format!("={name}:")
    }

    async fn view(&self, name: &str, history: usize) -> Result<View, String> {
        match &self.backend {
            Backend::Tmux(srv) => {
                let t = Term::target(name);
                // pane_dead before capture-pane: a dead pane's fd is
                // closed, so the capture that follows is final.
                let ask: [String; 5] = [
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    t.clone(),
                    PANE_FMT.into(),
                ];
                let p = settle(|| srv.run(&ask, None), || srv.reap(), REAP_WAIT).await?;
                let mut args = vec!["capture-pane".to_string(), "-p".into(), "-t".into(), t];
                if history > 0 {
                    args.push("-S".into());
                    args.push(format!("-{history}"));
                }
                let text = srv.run(&args, None).await?;
                let mut all: Vec<String> = text.lines().map(strip_ansi).collect();
                let screen_rows = self.rows as usize;
                while all.len() < screen_rows {
                    all.push(String::new());
                }
                let split = all.len().saturating_sub(screen_rows);
                let mut rows = all.split_off(split);
                pad_to_cursor(&mut rows, p.cursor);
                Ok(View {
                    rows,
                    history: all,
                    cursor: p.cursor,
                    dead: p.dead,
                    status: p.status,
                })
            }
            Backend::Pty(p) => Ok(p.view(history)),
        }
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

type Shared = Arc<tokio::sync::Mutex<Term>>;

#[derive(Default)]
struct Inner {
    tmux: Option<Arc<TmuxServer>>,
    tmux_failed: Option<String>,
    terms: BTreeMap<String, Shared>,
    rec_dir: Option<PathBuf>,
    journal: Option<Arc<Journal>>,
    opened: u64,
}

/// The terminals of one runtime process. `close_all` runs at exit; the kernel
/// (PDEATHSIG) covers a runtime that is killed.
#[derive(Default)]
pub struct TermRegistry {
    inner: Mutex<Inner>,
}

impl Drop for TermRegistry {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl TermRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where this run's recordings are, once a terminal was opened.
    pub fn recordings_dir(&self) -> Option<PathBuf> {
        self.lock().rec_dir.clone()
    }

    fn journal(&self) -> Option<Arc<Journal>> {
        self.lock().journal.clone()
    }

    fn get(&self, name: &str) -> Option<Shared> {
        self.lock().terms.get(name).cloned()
    }

    fn recording_setup(&self, cfg: &AppConfig) -> Result<(PathBuf, Arc<Journal>), String> {
        let mut g = self.lock();
        if let (Some(d), Some(j)) = (&g.rec_dir, &g.journal) {
            return Ok((d.clone(), j.clone()));
        }
        let root = match &cfg.term.recordings_root {
            Some(r) => r.clone(),
            None => dirs::home_dir()
                .ok_or("no home directory for terminal recordings")?
                .join(".x0ne/term"),
        };
        let dir = root.join(format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            std::process::id()
        ));
        private_dir(&dir).map_err(|e| format!("recordings dir {}: {e}", dir.display()))?;
        let j = Arc::new(
            Journal::open(&dir.join("audit.jsonl")).map_err(|e| format!("audit journal: {e}"))?,
        );
        g.rec_dir = Some(dir.clone());
        g.journal = Some(j.clone());
        Ok((dir, j))
    }

    fn tmux(&self, cfg: &AppConfig) -> Option<Arc<TmuxServer>> {
        if cfg.term.force_pty {
            return None;
        }
        let mut g = self.lock();
        if let Some(s) = &g.tmux {
            return Some(s.clone());
        }
        if g.tmux_failed.is_some() {
            return None;
        }
        let bin = TmuxServer::find()?;
        match TmuxServer::start(bin) {
            Ok(s) => {
                let s = Arc::new(s);
                g.tmux = Some(s.clone());
                Some(s)
            }
            Err(e) => {
                tracing::warn!("tmux unavailable, native PTY instead: {e}");
                g.tmux_failed = Some(e);
                None
            }
        }
    }

    /// Kill every terminal and the tmux server. Idempotent, synchronous.
    pub fn close_all(&self) {
        let (terms, tmux, journal) = {
            let mut g = self.lock();
            (
                std::mem::take(&mut g.terms),
                g.tmux.take(),
                g.journal.clone(),
            )
        };
        if terms.is_empty() && tmux.is_none() {
            return;
        }
        let mut names = Vec::new();
        let mut pumps = Vec::new();
        for (name, t) in terms {
            if let Ok(mut t) = t.try_lock() {
                if let Backend::Pty(p) = &t.backend {
                    p.kill();
                }
                pumps.push(std::mem::replace(&mut t.pump, Pump::idle()));
            }
            names.push(name);
        }
        if let Some(s) = &tmux {
            s.kill();
        }
        for mut p in pumps {
            p.finish();
        }
        if let Some(j) = journal {
            j.write(json!({"action": "TermCloseAll", "names": names}));
            j.seal();
        }
    }
}

// ── Executors ────────────────────────────────────────────────────────────────

/// Run one Term* action. The policy gate has already run.
pub async fn execute(action: &Action, config: &AppConfig, handles: SharedHandles) -> ActionResult {
    let reg = handles.lock().await.terms.clone();
    match action {
        Action::TermOpen { .. } => open(&reg, action, config).await,
        Action::TermSend { name, text, keys } => send(&reg, name, text.as_deref(), keys).await,
        Action::TermRead {
            name,
            history,
            diff,
        } => read(&reg, config, name, history.unwrap_or(0), *diff).await,
        Action::TermWait { .. } => wait(&reg, config, action).await,
        Action::TermResize { name, cols, rows } => resize(&reg, name, *cols, *rows).await,
        Action::TermSnapshot { name, ansi } => snapshot(&reg, config, name, *ansi).await,
        Action::TermList => list(&reg).await,
        Action::TermClose { name } => close(&reg, name).await,
        other => fail(other.type_name(), ErrorKind::Internal, "not a Term action"),
    }
}

fn not_found(op: &str, name: &str) -> ActionResult {
    fail(
        op,
        ErrorKind::NotFound,
        format!("no open terminal named '{name}'. TermList shows the open ones."),
    )
}

async fn open(reg: &TermRegistry, action: &Action, cfg: &AppConfig) -> ActionResult {
    const OP: &str = "TermOpen";
    let Action::TermOpen {
        name,
        command,
        args,
        cwd,
        cols,
        rows,
    } = action
    else {
        unreachable!()
    };
    if !valid_name(name) {
        return fail(
            OP,
            ErrorKind::InvalidInput,
            "name must match [A-Za-z0-9_-]{1,32}",
        );
    }
    let cols = cols.unwrap_or(DEFAULT_COLS).clamp(10, 1000);
    let rows = rows.unwrap_or(DEFAULT_ROWS).clamp(2, 500);
    {
        let g = reg.lock();
        if g.terms.contains_key(name) {
            return fail(
                OP,
                ErrorKind::InvalidInput,
                format!("terminal '{name}' is already open"),
            );
        }
        if g.terms.len() >= cfg.term.max_sessions {
            return fail(
                OP,
                ErrorKind::InvalidInput,
                format!(
                    "{} terminals open, the ceiling is [term] max_sessions = {}. Close one with TermClose.",
                    g.terms.len(),
                    cfg.term.max_sessions
                ),
            );
        }
    }
    let (rec_dir, journal) = match reg.recording_setup(cfg) {
        Ok(v) => v,
        Err(e) => {
            return fail(
                OP,
                ErrorKind::ExecutionFailed,
                format!("{e}: refusing to run unrecorded"),
            )
        }
    };
    let tmux = reg.tmux(cfg);
    let hidden: Vec<PathBuf> = tmux.iter().map(|s| s.dir.path().to_path_buf()).collect();
    let spawn = match crate::executor::isolation::plan(
        &cfg.isolation,
        &cfg.policy,
        &cfg.desktop,
        command,
        args,
        cwd.as_deref(),
        &crate::executor::isolation::Options {
            keep_tty: true,
            extra_hidden: &hidden,
            ..Default::default()
        },
    ) {
        Ok(s) => s,
        Err(e) if e.kind() == crate::executor::isolation::UNAVAILABLE_KIND => {
            return fail(OP, ErrorKind::MissingDependency, e.to_string())
        }
        Err(e) => return fail(OP, ErrorKind::ExecutionFailed, format!("{command}: {e}")),
    };
    let argv: Vec<String> = std::iter::once(command.clone())
        .chain(args.iter().cloned())
        .collect();
    let seq = {
        let mut g = reg.lock();
        g.opened += 1;
        g.opened
    };
    let cast = match Cast::create(
        rec_dir.join(format!("{seq:03}-{name}.cast")),
        cols,
        rows,
        name,
        &argv,
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => return fail(OP, ErrorKind::ExecutionFailed, format!("recording: {e}")),
    };
    let start_dir = spawn
        .current_dir
        .clone()
        .or_else(|| std::env::current_dir().ok());

    let (backend, pid, pump) = match &tmux {
        Some(srv) => match open_tmux(
            srv,
            name,
            seq,
            cols,
            rows,
            &spawn,
            start_dir.as_deref(),
            &cast,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => return fail(OP, ErrorKind::ExecutionFailed, e),
        },
        None => match PtyTerm::spawn(
            &spawn.program,
            &spawn.args,
            start_dir.as_deref(),
            cols,
            rows,
            cast.clone(),
            RaceHooks::default(),
        ) {
            Ok((p, pump)) => {
                let pid = p.pid;
                (Backend::Pty(Arc::new(p)), pid, pump)
            }
            Err(e) => return fail(OP, ErrorKind::ExecutionFailed, format!("{command}: {e}")),
        },
    };
    let term = Term {
        argv: argv.clone(),
        cols,
        rows,
        pid,
        backend,
        cast: cast.clone(),
        pump,
        last: Vec::new(),
    };
    let backend_name = term.backend_name();
    reg.lock()
        .terms
        .insert(name.clone(), Arc::new(tokio::sync::Mutex::new(term)));
    journal.write(json!({"action": OP, "name": name, "argv": argv, "cwd": cwd,
        "cols": cols, "rows": rows, "backend": backend_name, "pid": pid}));
    ok(
        OP,
        format!("'{name}' ({backend_name}, pid {pid})"),
        json!({"name": name, "backend": backend_name, "pid": pid, "cols": cols, "rows": rows,
               "recording": cast.path}),
    )
}

#[allow(clippy::too_many_arguments)]
async fn open_tmux(
    srv: &Arc<TmuxServer>,
    name: &str,
    seq: u64,
    cols: u16,
    rows: u16,
    spawn: &crate::executor::isolation::Spawn,
    start_dir: Option<&Path>,
    cast: &Arc<Cast>,
) -> Result<(Backend, u32, Pump), String> {
    let fifo = srv.dir.path().join(format!("{seq:03}.fifo"));
    let fifo_s = fifo.to_string_lossy().into_owned();
    // pipe-pane hands its argument to `sh -c`: the path must need no quoting.
    if fifo_s.contains('\'') {
        return Err("unsafe temporary directory name".into());
    }
    let c = std::ffi::CString::new(fifo_s.clone()).map_err(|e| e.to_string())?;
    // SAFETY: mkfifo on a NUL-terminated path we built.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(format!("mkfifo: {}", std::io::Error::last_os_error()));
    }
    // O_RDWR: never blocks on open, never reads EOF; the pump stops on its flag.
    let fd: std::os::fd::OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .map_err(|e| format!("fifo: {e}"))?
        .into();
    let sink = cast.clone();
    let mut pump = Pump::start(fd, move |b| sink.output(b), || {});
    let mut a: Vec<String> = vec![
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        name.to_string(),
        "-x".into(),
        cols.to_string(),
        "-y".into(),
        rows.to_string(),
        "-P".into(),
        "-F".into(),
        "#{pane_pid}".into(),
        "--".into(),
    ];
    a.push(tmux_arg(&spawn.program.to_string_lossy()));
    a.extend(spawn.args.iter().map(|x| tmux_arg(&x.to_string_lossy())));
    // Same command list: tmux runs both before reading the pane, so the
    // recording misses no output.
    a.extend([
        ";".into(),
        "pipe-pane".into(),
        "-O".into(),
        "-t".into(),
        Term::target(name),
        format!("exec cat > '{fifo_s}'"),
    ]);
    match srv.run(&a, start_dir).await {
        Ok(out) => {
            let pid = out.trim().parse::<u32>().unwrap_or(0);
            Ok((Backend::Tmux(srv.clone()), pid, pump))
        }
        Err(e) => {
            pump.finish();
            Err(format!("tmux new-session: {e}"))
        }
    }
}

async fn send(reg: &TermRegistry, name: &str, text: Option<&str>, keys: &[String]) -> ActionResult {
    const OP: &str = "TermSend";
    let mut parsed = Vec::with_capacity(keys.len());
    for k in keys {
        match key(k) {
            Some(v) => parsed.push(v),
            None => {
                return fail(
                    OP,
                    ErrorKind::InvalidInput,
                    format!(
                        "unknown key '{k}'. Known: Enter Tab BTab Escape BSpace Space Up Down Left \
                         Right Home End PageUp PageDown Insert Delete F1-F12 C-a..C-z M-<char>"
                    ),
                )
            }
        }
    }
    if text.map(str::is_empty).unwrap_or(true) && parsed.is_empty() {
        return fail(
            OP,
            ErrorKind::InvalidInput,
            "nothing to send: give `text`, `keys` or both",
        );
    }
    let Some(t) = reg.get(name) else {
        return not_found(OP, name);
    };
    let t = t.lock().await;
    let mut typed = String::from(text.unwrap_or(""));
    for (_, bytes) in &parsed {
        typed.push_str(&String::from_utf8_lossy(bytes));
    }
    let res = match &t.backend {
        Backend::Tmux(srv) => {
            let target = Term::target(name);
            let mut r = Ok(String::new());
            if let Some(s) = text.filter(|s| !s.is_empty()) {
                r = srv
                    .run(
                        &[
                            "send-keys".into(),
                            "-t".into(),
                            target.clone(),
                            "-l".into(),
                            "--".into(),
                            tmux_arg(s),
                        ],
                        None,
                    )
                    .await;
            }
            if r.is_ok() && !parsed.is_empty() {
                let mut a = vec!["send-keys".to_string(), "-t".into(), target];
                a.extend(parsed.iter().map(|(k, _)| k.clone()));
                r = srv.run(&a, None).await;
            }
            r.map(|_| ())
        }
        Backend::Pty(p) => {
            let mut bytes = text.unwrap_or("").as_bytes().to_vec();
            for (_, b) in &parsed {
                bytes.extend_from_slice(b);
            }
            p.write(&bytes).map_err(|e| e.to_string())
        }
    };
    t.cast.event("i", &typed);
    if let Some(j) = reg.journal() {
        j.write(
            json!({"action": OP, "name": name, "text": text, "keys": keys,
                       "ok": res.is_ok()}),
        );
    }
    match res {
        Ok(()) => ok(
            OP,
            format!("'{name}' <- {} bytes", typed.len()),
            json!({"name": name, "bytes": typed.len()}),
        ),
        Err(e) => fail(
            OP,
            ErrorKind::ExecutionFailed,
            format!("send to '{name}': {e}"),
        ),
    }
}

fn view_json(name: &str, v: &View, text: String, cfg: &AppConfig) -> Value {
    let (text, truncated) = keep_tail(text, cfg.term.max_read_bytes);
    json!({
        "name": name,
        "text": text,
        "truncated": truncated,
        "cursor": v.cursor.map(|(x, y)| json!({"x": x, "y": y})),
        "exited": v.dead,
        "exit_status": v.status,
    })
}

async fn read(
    reg: &TermRegistry,
    cfg: &AppConfig,
    name: &str,
    history: usize,
    diff: bool,
) -> ActionResult {
    const OP: &str = "TermRead";
    let Some(t) = reg.get(name) else {
        return not_found(OP, name);
    };
    let mut t = t.lock().await;
    let v = match t.view(name, history.min(HISTORY_LIMIT)).await {
        Ok(v) => v,
        Err(e) => return fail(OP, ErrorKind::ExecutionFailed, e),
    };
    let mut all = v.history.clone();
    all.extend(v.rows.iter().cloned());
    let mut out = view_json(name, &v, trim_rows(&all), cfg);
    if diff {
        out["diff"] = json!(diff_rows(&t.last, &v.rows));
        out["text"] = json!("");
    }
    t.last = v.rows.clone();
    ok(OP, format!("'{name}' {} rows", v.rows.len()), out)
}

async fn wait(reg: &TermRegistry, cfg: &AppConfig, action: &Action) -> ActionResult {
    const OP: &str = "TermWait";
    let Action::TermWait {
        name,
        pattern,
        stable_ms,
        exit,
        timeout_ms,
        diff,
    } = action
    else {
        unreachable!()
    };
    if pattern.is_none() && stable_ms.is_none() && !exit {
        return fail(
            OP,
            ErrorKind::InvalidInput,
            "give `pattern`, `stable_ms` or `exit: true`",
        );
    }
    let re = match pattern
        .as_deref()
        .map(|p| regex::RegexBuilder::new(p).multi_line(true).build())
        .transpose()
    {
        Ok(r) => r,
        Err(e) => return fail(OP, ErrorKind::InvalidInput, format!("pattern: {e}")),
    };
    let Some(t) = reg.get(name) else {
        return not_found(OP, name);
    };
    let budget = Duration::from_millis(
        timeout_ms
            .unwrap_or(DEFAULT_WAIT_MS)
            .min(cfg.term.max_wait_ms),
    );
    let t0 = Instant::now();
    let mut stable_since = Instant::now();
    let mut prev: Option<Vec<String>> = None;
    let mut t = t.lock().await;
    let (v, condition) = loop {
        let v = match t.view(name, 0).await {
            Ok(v) => v,
            Err(e) => return fail(OP, ErrorKind::ExecutionFailed, e),
        };
        let screen = v.rows.join("\n");
        if re.as_ref().map(|r| r.is_match(&screen)).unwrap_or(false) {
            break (v, "pattern");
        }
        if v.dead {
            break (v, "exit");
        }
        if prev.as_ref() != Some(&v.rows) {
            stable_since = Instant::now();
            prev = Some(v.rows.clone());
        } else if let Some(ms) = stable_ms {
            if stable_since.elapsed() >= Duration::from_millis(*ms) {
                break (v, "stable");
            }
        }
        if t0.elapsed() >= budget {
            break (v, "timeout");
        }
        tokio::time::sleep(POLL).await;
    };
    if condition == "timeout" {
        // A ✓ here made the model treat an unfinished build as done (2026-10-06).
        t.last = v.rows.clone();
        let want = match (pattern, stable_ms) {
            (Some(p), _) => format!("pattern `{p}`"),
            (None, Some(ms)) => format!("{ms} ms of stable screen"),
            (None, None) => "exit".to_string(),
        };
        let tail: Vec<&str> = v
            .rows
            .iter()
            .map(|r| r.trim_end())
            .filter(|r| !r.is_empty())
            .collect();
        let tail = tail[tail.len().saturating_sub(5)..].join("\n");
        return fail(
            OP,
            ErrorKind::Timeout,
            format!(
                "'{name}' timed out after {} ms waiting for {want}; last screen lines:\n{tail}",
                t0.elapsed().as_millis()
            ),
        );
    }
    let mut out = view_json(name, &v, trim_rows(&v.rows), cfg);
    out["condition"] = json!(condition);
    out["waited_ms"] = json!(t0.elapsed().as_millis() as u64);
    if *diff {
        out["diff"] = json!(diff_rows(&t.last, &v.rows));
        out["text"] = json!("");
    }
    t.last = v.rows.clone();
    ok(OP, format!("'{name}' {condition}"), out)
}

async fn resize(reg: &TermRegistry, name: &str, cols: u16, rows: u16) -> ActionResult {
    const OP: &str = "TermResize";
    let (cols, rows) = (cols.clamp(10, 1000), rows.clamp(2, 500));
    let Some(t) = reg.get(name) else {
        return not_found(OP, name);
    };
    let mut t = t.lock().await;
    let r = match &t.backend {
        Backend::Tmux(srv) => srv
            .run(
                &[
                    "resize-window".into(),
                    "-t".into(),
                    Term::target(name),
                    "-x".into(),
                    cols.to_string(),
                    "-y".into(),
                    rows.to_string(),
                ],
                None,
            )
            .await
            .map(|_| ()),
        Backend::Pty(p) => p.resize(cols, rows).map_err(|e| e.to_string()),
    };
    if let Err(e) = r {
        return fail(OP, ErrorKind::ExecutionFailed, e);
    }
    t.cols = cols;
    t.rows = rows;
    t.cast.event("r", &format!("{cols}x{rows}"));
    if let Some(j) = reg.journal() {
        j.write(json!({"action": OP, "name": name, "cols": cols, "rows": rows}));
    }
    ok(
        OP,
        format!("'{name}' {cols}x{rows}"),
        json!({"name": name, "cols": cols, "rows": rows}),
    )
}

async fn snapshot(reg: &TermRegistry, cfg: &AppConfig, name: &str, ansi: bool) -> ActionResult {
    const OP: &str = "TermSnapshot";
    let Some(t) = reg.get(name) else {
        return not_found(OP, name);
    };
    let t = t.lock().await;
    let text = match &t.backend {
        Backend::Tmux(srv) => {
            let mut a = vec![
                "capture-pane".to_string(),
                "-p".into(),
                "-t".into(),
                Term::target(name),
                "-S".into(),
                "-".into(),
                "-E".into(),
                "-".into(),
            ];
            if ansi {
                a.push("-e".into());
            }
            match srv.run(&a, None).await {
                Ok(s) if ansi => s.trim_end().to_string(),
                Ok(s) => trim_rows(&s.lines().map(strip_ansi).collect::<Vec<_>>()),
                Err(e) => return fail(OP, ErrorKind::ExecutionFailed, e),
            }
        }
        Backend::Pty(p) => {
            let sc = p.screen.lock().unwrap_or_else(|e| e.into_inner());
            if ansi {
                let raw: Vec<u8> = sc.raw.iter().copied().collect();
                String::from_utf8_lossy(&raw).into_owned()
            } else {
                trim_rows(&sc.rows_from(0))
            }
        }
    };
    let lines = text.lines().count();
    let (text, truncated) = keep_tail(text, cfg.term.max_read_bytes);
    ok(
        OP,
        format!("'{name}' {lines} lines"),
        json!({"name": name, "text": text, "lines": lines, "truncated": truncated, "ansi": ansi}),
    )
}

async fn list(reg: &TermRegistry) -> ActionResult {
    let terms: Vec<(String, Shared)> = reg
        .lock()
        .terms
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = Vec::new();
    for (name, t) in terms {
        let t = t.lock().await;
        let v = t.view(&name, 0).await.ok();
        out.push(json!({
            "name": name, "command": t.argv, "backend": t.backend_name(), "pid": t.pid,
            "cols": t.cols, "rows": t.rows,
            "alive": v.as_ref().map(|v| !v.dead),
            "exit_status": v.as_ref().and_then(|v| v.status),
            "recording": t.cast.path,
        }));
    }
    let n = out.len();
    ok(
        "TermList",
        format!("{n} terminals"),
        json!({"terminals": out, "recordings": reg.recordings_dir()}),
    )
}

async fn close(reg: &TermRegistry, name: &str) -> ActionResult {
    const OP: &str = "TermClose";
    let removed = reg.lock().terms.remove(name);
    let Some(t) = removed else {
        return not_found(OP, name);
    };
    let mut t = t.lock().await;
    match &t.backend {
        Backend::Tmux(srv) => {
            let _ = srv
                .run(
                    &["kill-session".into(), "-t".into(), format!("={name}")],
                    None,
                )
                .await;
        }
        Backend::Pty(p) => p.kill(),
    }
    let mut pump = std::mem::replace(&mut t.pump, Pump::idle());
    let _ = tokio::task::spawn_blocking(move || pump.finish()).await;
    if let Some(j) = reg.journal() {
        j.write(json!({"action": OP, "name": name}));
    }
    ok(
        OP,
        format!("'{name}' closed"),
        json!({"name": name, "recording": t.cast.path}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audit_journal_masks_secrets_and_is_chained() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("audit.jsonl");
        let j = Journal::open(&p).unwrap();
        j.write(json!({"action": "TermOpen", "name": "sh",
            "argv": ["sh", "-c", "curl -H 'Authorization: Bearer abcdefgh12345678' x"]}));
        j.write(json!({"action": "TermSend", "name": "sh",
            "text": "export OPENAI_API_KEY=sk-proj-Ab3dEf6hIj9kLmNoPq", "keys": ["Enter"]}));
        drop(j);
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            !body.contains("Ab3dEf6") && !body.contains("abcdefgh1234"),
            "{body}"
        );
        let r = x0ne_audit::verify_file(&p, None).unwrap();
        assert_eq!(r.status, x0ne_audit::Status::Ok, "{r:?}");
        assert_eq!(r.chained, 3, "header and two lines: {r:?}");
    }

    #[test]
    fn a_trailing_semicolon_survives_tmux_parsing() {
        assert_eq!(tmux_arg("end;"), "end\\;");
        assert_eq!(tmux_arg("a\\;"), "a\\\\;");
        assert_eq!(tmux_arg("a;b"), "a;b");
    }

    #[test]
    fn keys_map_to_tmux_names_and_bytes() {
        assert_eq!(key("Enter").unwrap(), ("Enter".into(), b"\r".to_vec()));
        assert_eq!(key("C-c").unwrap(), ("C-c".into(), vec![3]));
        assert_eq!(key("M-x").unwrap(), ("M-x".into(), vec![0x1b, b'x']));
        assert_eq!(key("PageUp").unwrap().0, "PPage");
        assert!(key("Enter; kill-server").is_none());
        assert!(key("C-cc").is_none());
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[31mred\u{1b}[0m \u{1b}]0;t\u{7}x"),
            "red x"
        );
    }

    #[test]
    fn names_are_plain() {
        assert!(valid_name("py-1_a"));
        for bad in ["", "a;b", "a:b", "a.b", "=x", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_pty_screen_handles_cr_and_erase() {
        let mut s = Screen::new(20, 3);
        s.feed(b">>> abc\x1b[K\r\n42\r\nxyz\x08Q");
        let rows = s.rows_from(s.top());
        assert_eq!(rows, vec![">>> abc", "42", "xyQ"]);
    }

    // ── `exited` with a screen missing the program's last output ──

    fn pty_sh(script: &str, hooks: RaceHooks, dir: &Path) -> (PtyTerm, Pump) {
        let cast = Arc::new(Cast::create(dir.join("t.cast"), 80, 10, "t", &[]).unwrap());
        let args = [OsString::from("-c"), OsString::from(script)];
        PtyTerm::spawn(&OsString::from("/bin/sh"), &args, None, 80, 10, cast, hooks).unwrap()
    }

    /// Reaped by its waiter thread: a zombie still has its /proc entry.
    fn reaped(pid: u32) -> bool {
        !Path::new(&format!("/proc/{pid}")).exists()
    }

    fn wait_reaped(pid: u32) {
        let t0 = Instant::now();
        while !reaped(pid) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Views every 5 ms until one says `exited`, at most 10 s.
    fn until_exited(p: &PtyTerm) -> View {
        let t0 = Instant::now();
        loop {
            let v = p.view(0);
            if v.dead || t0.elapsed() > Duration::from_secs(10) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The answer goes in after the view's first read and the program is
    /// reaped before its second: the flake of the y/N test, made certain.
    #[test]
    fn an_exit_between_the_two_reads_of_a_view_never_shows_a_stale_screen() {
        let d = tempfile::tempdir().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let a = armed.clone();
        let hooks = RaceHooks {
            between_reads: Some(Arc::new(move |p: &PtyTerm| {
                if a.swap(false, Ordering::SeqCst) {
                    p.write(b"y\r").unwrap();
                    wait_reaped(p.pid);
                }
            })),
            ..Default::default()
        };
        let (p, mut pump) = pty_sh("read -r a; echo \"answer=$a\"; exit 7", hooks, d.path());
        armed.store(true, Ordering::SeqCst);
        let first = p.view(0);
        assert!(
            !first.dead || first.rows.join("\n").contains("answer=y"),
            "exited (status {:?}) with a screen missing the last output: {:?}",
            first.status,
            trim_rows(&first.rows)
        );
        let v = until_exited(&p);
        assert_eq!(v.status, Some(7), "{:?}", trim_rows(&v.rows));
        assert!(
            v.rows.join("\n").contains("answer=y"),
            "{:?}",
            trim_rows(&v.rows)
        );
        pump.finish();
    }

    /// The reader holds the last chunk until the program is reaped, as a
    /// starved thread does on a loaded machine.
    #[test]
    fn a_lagging_reader_never_lets_a_view_say_exited_before_the_last_output() {
        let d = tempfile::tempdir().unwrap();
        let pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let q = pid.clone();
        let hooks = RaceHooks {
            before_feed: Some(Arc::new(move || {
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(1) {
                    let p = q.load(Ordering::SeqCst);
                    if p != 0 && reaped(p) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(100));
            })),
            ..Default::default()
        };
        let (p, mut pump) = pty_sh("echo last-line; exit 7", hooks, d.path());
        pid.store(p.pid, Ordering::SeqCst);
        let v = until_exited(&p);
        assert_eq!(v.status, Some(7), "{:?}", trim_rows(&v.rows));
        assert!(
            v.rows.join("\n").contains("last-line"),
            "exited with a screen missing the last output: {:?}",
            trim_rows(&v.rows)
        );
        pump.finish();
    }

    /// A descendant that ignores SIGHUP holds the slave: no EOF, the exit is
    /// reported after `DRAIN_GRACE`. Trap set before the fork: in the subshell
    /// it lost to the session leader's SIGHUP in 2 of 5 runs (2026-10-27).
    #[test]
    fn an_exit_is_reported_while_a_descendant_keeps_the_terminal_open() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let (p, mut pump) = pty_sh(
            "trap '' HUP; sleep 30 & echo started; exit 3",
            RaceHooks::default(),
            d.path(),
        );
        let v = until_exited(&p);
        let took = t0.elapsed();
        p.kill();
        pump.finish();
        assert_eq!(
            v.status,
            Some(3),
            "after {took:?}: {:?}",
            trim_rows(&v.rows)
        );
        assert!(
            v.rows.join("\n").contains("started"),
            "{:?}",
            trim_rows(&v.rows)
        );
        assert!(
            took >= DRAIN_GRACE,
            "EOF came with the slave held: {took:?}"
        );
    }

    // ── tmux: a dead pane whose child is not reaped yet ──

    /// `settle` over canned `display-message` outputs, the last one repeated.
    /// Returns the info, the looks and the nudges.
    async fn settle_on(outs: &[&str], wait: Duration) -> (PaneInfo, usize, usize) {
        let looks = std::cell::Cell::new(0);
        let nudges = std::cell::Cell::new(0);
        let p = settle(
            || {
                let i = looks.get();
                looks.set(i + 1);
                let out = outs.get(i).or(outs.last()).map(|s| (*s).to_string());
                async move { out.ok_or_else(|| "no output".to_string()) }
            },
            || nudges.set(nudges.get() + 1),
            wait,
        )
        .await
        .unwrap();
        (p, looks.get(), nudges.get())
    }

    /// The CI's tmux 3.4 answered `exited: true, exit_status: null` (2026-11-05).
    #[tokio::test]
    async fn a_dead_tmux_pane_without_a_status_is_read_again_until_it_has_one() {
        let (p, looks, nudges) = settle_on(&["0 2 1  \n", "0 2 1 7 \n"], REAP_WAIT).await;
        assert_eq!(p.status, Some(7), "{p:?}");
        assert!(p.dead && p.cursor == Some((0, 2)), "{p:?}");
        assert_eq!((looks, nudges), (2, 1));
    }

    #[tokio::test]
    async fn a_live_pane_or_one_with_a_status_or_a_signal_is_read_once() {
        assert_eq!(PANE_FMT.split(' ').nth(4), Some("#{pane_dead_signal}"));
        for out in ["0 2 0  \n", "0 2 1 0 \n", "0 2 1  9\n", "0 2 1  kill\n"] {
            let (p, looks, nudges) = settle_on(&[out], REAP_WAIT).await;
            assert_eq!((looks, nudges), (1, 0), "{out:?}: {p:?}");
            assert!(!p.unreaped(), "{out:?}: {p:?}");
        }
        let (p, ..) = settle_on(&["0 2 1  9\n"], REAP_WAIT).await;
        assert!(p.dead && p.signaled && p.status.is_none(), "{p:?}");
    }

    #[tokio::test]
    async fn a_pane_never_reaped_is_reported_dead_once_the_wait_is_over() {
        let t0 = Instant::now();
        let (p, looks, _) = settle_on(&["0 2 1  \n"], Duration::from_millis(100)).await;
        let took = t0.elapsed();
        assert!(p.unreaped(), "{p:?}");
        assert!(looks > 1, "{looks} looks");
        assert!(took < Duration::from_secs(1), "{took:?}");
    }
}
