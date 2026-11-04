//! `x0ne fleet`: the Claude Code sessions running on this host (step 1).
//!
//! Read-only view fed by Claude Code hooks. `x0ne fleet hook` reads one hook
//! payload on stdin and rewrites one per-session state file; `x0ne fleet ls`
//! reads those files. Hook reference: <https://code.claude.com/docs/en/hooks>
//! (read 2026-09-23, Claude Code 2.1.280).
//!
//! SECURITY: the payload is data. Nothing in it is executed, opened or
//! followed; `session_id` becomes a file name only after a strict charset
//! check. The recorder never exits non-zero and never writes to stdout:
//! exit 2 on `PreToolUse` blocks the tool, and stdout on `SessionStart` /
//! `UserPromptSubmit` is injected into the model's context.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::text_safety::snippet;

/// Largest payload parsed. `PostToolUse` carries the tool output.
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
/// Beyond `MAX_PAYLOAD`, stdin is drained up to this, then abandoned.
const MAX_DRAIN: usize = 64 * 1024 * 1024;
/// A state file larger than this is not ours.
const MAX_STATE_FILE: u64 = 64 * 1024;
/// Without a pid to probe, a session silent this long is shown stale.
const STALE_WITHOUT_PID_SECS: i64 = 6 * 3600;
/// `ls` hides done/stale sessions older than this unless `--all`.
const HIDE_FINISHED_AFTER_SECS: i64 = 24 * 3600;

/// `$X0NE_FLEET_DIR`, else `~/.x0ne/fleet`.
pub fn fleet_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("X0NE_FLEET_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join("fleet")
}

// ── Stored state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetState {
    /// Between turns: waiting for the human's next prompt.
    Idle,
    Working,
    WaitingPermission,
    Done,
}

/// One file per session, `<dir>/<session_id>.json`. Strings are sanitized and
/// bounded when written; `ls` sanitizes again, the file being editable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub state: FleetState,
    pub state_since: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_event: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub last_tool: Option<String>,
    /// `Tool: summary` of the request the session is blocked on.
    #[serde(default)]
    pub pending: Option<String>,
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub end_reason: Option<String>,
    /// The Claude Code process, found by walking up from the hook's parent.
    #[serde(default)]
    pub pid: Option<u32>,
    /// `/proc/<pid>/stat` start time: guards against pid reuse.
    #[serde(default)]
    pub pid_start: Option<u64>,
    #[serde(default)]
    pub events: u64,
}

// ── Hook payload ─────────────────────────────────────────────────────────────

/// The fields of a hook payload that the fleet uses, already bounded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookEvent {
    pub session_id: String,
    pub event: String,
    pub cwd: Option<String>,
    pub permission_mode: Option<String>,
    pub tool_name: Option<String>,
    pub tool_summary: Option<String>,
    pub prompt: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,
    pub reason: Option<String>,
}

/// A session id usable as a file name: `[A-Za-z0-9_-]{1,128}`.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn str_field(v: &Value, key: &str, max: usize) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| snippet(s, max))
        .filter(|s| !s.trim().is_empty())
}

/// What the human would need to read to determine: the command, path or URL.
fn summarize_tool_input(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    for key in [
        "command",
        "file_path",
        "notebook_path",
        "path",
        "url",
        "pattern",
        "query",
        "description",
    ] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            return Some(snippet(s, 160));
        }
    }
    obj.values()
        .find_map(Value::as_str)
        .map(|s| snippet(s, 160))
}

/// Parses a payload. `event_arg` is used only when the payload names no event.
pub fn parse_payload(raw: &[u8], event_arg: Option<&str>) -> Result<HookEvent> {
    let v: Value = serde_json::from_slice(raw).context("hook payload is not JSON")?;
    if !v.is_object() {
        bail!("hook payload is not a JSON object");
    }
    let session_id = v
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let event = str_field(&v, "hook_event_name", 64)
        .or_else(|| event_arg.map(|e| snippet(e, 64)))
        .unwrap_or_default();
    Ok(HookEvent {
        session_id,
        event,
        cwd: str_field(&v, "cwd", 512),
        permission_mode: str_field(&v, "permission_mode", 32),
        tool_name: str_field(&v, "tool_name", 96),
        tool_summary: v.get("tool_input").and_then(summarize_tool_input),
        prompt: str_field(&v, "prompt", 120),
        notification_type: str_field(&v, "notification_type", 64),
        message: str_field(&v, "message", 160),
        reason: str_field(&v, "reason", 64),
    })
}

/// Reads `"key": "<string>"` from the head of a payload too large to parse.
/// The docs list the common fields before `tool_input`; not measured on a real payload.
fn scan_string_field(head: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    let needle = format!("\"{key}\"");
    let at = text.find(&needle)? + needle.len();
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return serde_json::from_str::<String>(&rest[..=i]).ok(),
            _ => i += 1,
        }
    }
    None
}

/// Degraded parse of a truncated payload: identity and event, no details.
pub fn parse_truncated(head: &[u8], event_arg: Option<&str>) -> Option<HookEvent> {
    let head = &head[..head.len().min(64 * 1024)];
    let session_id = scan_string_field(head, "session_id")?;
    let event = scan_string_field(head, "hook_event_name")
        .or_else(|| event_arg.map(str::to_string))
        .map(|e| snippet(&e, 64))
        .unwrap_or_default();
    Some(HookEvent {
        session_id,
        event,
        cwd: scan_string_field(head, "cwd").map(|s| snippet(&s, 512)),
        tool_name: scan_string_field(head, "tool_name").map(|s| snippet(&s, 96)),
        ..HookEvent::default()
    })
}

// ── Transitions ──────────────────────────────────────────────────────────────

/// The next record. Pure: `now` and the process identity are passed in.
pub fn apply(
    prev: Option<SessionRecord>,
    ev: &HookEvent,
    now: DateTime<Utc>,
    process: Option<(u32, Option<u64>)>,
) -> SessionRecord {
    let mut rec = prev.unwrap_or_else(|| SessionRecord {
        session_id: ev.session_id.clone(),
        state: FleetState::Idle,
        state_since: now,
        updated_at: now,
        last_event: String::new(),
        cwd: None,
        permission_mode: None,
        last_tool: None,
        pending: None,
        last_prompt: None,
        end_reason: None,
        pid: None,
        pid_start: None,
        events: 0,
    });
    let tool_label = || {
        let name = ev.tool_name.clone().unwrap_or_else(|| "tool".into());
        match &ev.tool_summary {
            Some(s) => format!("{name}: {s}"),
            None => name,
        }
    };
    let next = match ev.event.as_str() {
        "SessionStart" => {
            rec.end_reason = None;
            rec.pending = None;
            Some(FleetState::Idle)
        }
        "UserPromptSubmit" => {
            rec.pending = None;
            if ev.prompt.is_some() {
                rec.last_prompt = ev.prompt.clone();
            }
            Some(FleetState::Working)
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
            rec.pending = None;
            if ev.tool_name.is_some() {
                rec.last_tool = Some(tool_label());
            }
            Some(FleetState::Working)
        }
        "PermissionRequest" => {
            rec.pending = Some(tool_label());
            Some(FleetState::WaitingPermission)
        }
        "Notification" => match ev.notification_type.as_deref() {
            Some("permission_prompt") => {
                if rec.pending.is_none() {
                    rec.pending = ev.message.clone().or_else(|| Some("permission".into()));
                }
                Some(FleetState::WaitingPermission)
            }
            Some("idle_prompt") => {
                rec.pending = None;
                Some(FleetState::Idle)
            }
            _ => None,
        },
        "Stop" | "StopFailure" => {
            rec.pending = None;
            Some(FleetState::Idle)
        }
        "SessionEnd" => {
            rec.pending = None;
            rec.end_reason = ev.reason.clone();
            Some(FleetState::Done)
        }
        _ => None,
    };
    // A late event from a finished session does not revive it.
    let next = next.filter(|s| {
        rec.state != FleetState::Done || ev.event == "SessionStart" || *s == FleetState::Done
    });
    if let Some(s) = next {
        if s != rec.state {
            rec.state_since = now;
        }
        rec.state = s;
    }
    if !ev.event.is_empty() {
        rec.last_event = ev.event.clone();
    }
    if ev.cwd.is_some() {
        rec.cwd = ev.cwd.clone();
    }
    if ev.permission_mode.is_some() {
        rec.permission_mode = ev.permission_mode.clone();
    }
    if let Some((pid, start)) = process {
        rec.pid = Some(pid);
        rec.pid_start = start;
    }
    rec.updated_at = now;
    rec.events = rec.events.saturating_add(1);
    rec
}

// ── Recorder ─────────────────────────────────────────────────────────────────

fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn read_record(path: &Path) -> Option<SessionRecord> {
    let f = fs::File::open(path).ok()?;
    if f.metadata().ok()?.len() > MAX_STATE_FILE {
        return None;
    }
    let mut buf = Vec::new();
    f.take(MAX_STATE_FILE).read_to_end(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_atomic(dir: &Path, id: &str, rec: &SessionRecord) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let tmp = dir.join(format!(".{id}.{}.{nanos}.{seq}.tmp", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = (|| -> Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(rec)?)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(format!("{id}.json")))?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Serializes read-modify-write per session. Hooks run in parallel (docs), so
/// two `PreToolUse` can race. Gives up after ~1 s: a lost update beats a hang.
fn lock_session(dir: &Path, id: &str) -> Option<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(dir.join(format!(".{id}.lock"))).ok()?;
    for _ in 0..100 {
        if f.try_lock().is_ok() {
            return Some(f);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Records one event in `dir`. Returns the record written.
pub fn record_event(
    dir: &Path,
    ev: &HookEvent,
    now: DateTime<Utc>,
    process: Option<(u32, Option<u64>)>,
) -> Result<SessionRecord> {
    if !valid_session_id(&ev.session_id) {
        bail!("session_id missing or not [A-Za-z0-9_-]{{1,128}}");
    }
    ensure_dir(dir)?;
    let _lock = lock_session(dir, &ev.session_id);
    let path = dir.join(format!("{}.json", ev.session_id));
    let rec = apply(read_record(&path), ev, now, process);
    write_atomic(dir, &ev.session_id, &rec)?;
    Ok(rec)
}

/// Reads at most `MAX_PAYLOAD` bytes; `true` if the input was longer.
pub fn read_bounded(mut input: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    (&mut input)
        .take(MAX_PAYLOAD as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() <= MAX_PAYLOAD {
        return Ok((buf, false));
    }
    buf.truncate(MAX_PAYLOAD);
    // Drain so the writer does not see EPIPE, within a bound.
    let _ = std::io::copy(&mut input.take(MAX_DRAIN as u64), &mut std::io::sink());
    Ok((buf, true))
}

/// Bytes in, record out: the whole recorder minus stdin and the process walk.
pub fn record_payload(
    dir: &Path,
    raw: &[u8],
    truncated: bool,
    event_arg: Option<&str>,
    now: DateTime<Utc>,
    process: Option<(u32, Option<u64>)>,
) -> Result<SessionRecord> {
    let ev = if truncated {
        parse_truncated(raw, event_arg).context("truncated payload without a session_id")?
    } else {
        parse_payload(raw, event_arg)?
    };
    record_event(dir, &ev, now, process)
}

/// `x0ne fleet hook [event]`. Never fails, never prints to stdout.
pub fn run_hook(event_arg: Option<&str>) {
    let res = read_bounded(std::io::stdin().lock())
        .map_err(anyhow::Error::from)
        .and_then(|(raw, truncated)| {
            record_payload(
                &fleet_dir(),
                &raw,
                truncated,
                event_arg,
                Utc::now(),
                claude_process(),
            )
        });
    if let Err(e) = res {
        eprintln!("x0ne fleet hook: {e:#}");
    }
}

// ── Process identity (Linux /proc) ───────────────────────────────────────────

/// `(ppid, comm, starttime)` from `/proc/<pid>/stat`.
fn proc_stat(pid: u32) -> Option<(u32, String, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parens: split on the LAST ')'.
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s.get(close + 1..)?.split_whitespace().collect();
    // rest[0] = state (field 3), so field N is rest[N - 3].
    let ppid = rest.get(1)?.parse().ok()?;
    let start = rest.get(19)?.parse().ok()?;
    Some((ppid, comm, start))
}

/// Start time of a live pid, `None` if it is gone.
pub fn process_start(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|(_, _, start)| start)
}

#[cfg(unix)]
const WRAPPERS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "env", "timeout"];

#[cfg(not(unix))]
pub fn claude_process() -> Option<(u32, Option<u64>)> {
    None
}

/// The Claude Code process: the first ancestor named `claude`, else the first
/// that is not a shell. Measured 2026-09-23: Bash tool shells are direct
/// children of a process whose comm is `claude` (2.1.280).
#[cfg(unix)]
pub fn claude_process() -> Option<(u32, Option<u64>)> {
    let mut pid = std::os::unix::process::parent_id();
    let mut first_non_wrapper = None;
    for _ in 0..8 {
        let Some((ppid, comm, start)) = proc_stat(pid) else {
            break;
        };
        if comm == "claude" {
            return Some((pid, Some(start)));
        }
        if first_non_wrapper.is_none() && !WRAPPERS.contains(&comm.as_str()) {
            first_non_wrapper = Some((pid, Some(start)));
        }
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    first_non_wrapper
}

// ── Listing ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShownState {
    WaitingPermission,
    Working,
    Idle,
    Stale,
    Done,
}

impl ShownState {
    fn label(self) -> &'static str {
        match self {
            ShownState::WaitingPermission => "permission",
            ShownState::Working => "working",
            ShownState::Idle => "idle",
            ShownState::Stale => "stale",
            ShownState::Done => "done",
        }
    }
}

/// A record as `ls` shows it.
#[derive(Debug, Clone)]
pub struct FleetRow {
    pub state: ShownState,
    pub record: SessionRecord,
}

/// `--json`: the record, with `state` = shown state and `recorded_state` = stored.
pub fn rows_json(rows: &[FleetRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                let mut v = serde_json::to_value(&row.record).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    let recorded = o.remove("state").unwrap_or(Value::Null);
                    o.insert("recorded_state".into(), recorded);
                    o.insert("state".into(), serde_json::json!(row.state));
                }
                v
            })
            .collect(),
    )
}

/// Done stays done; otherwise stale when the recorded process is gone.
pub fn shown_state(
    rec: &SessionRecord,
    now: DateTime<Utc>,
    probe: &dyn Fn(u32) -> Option<u64>,
) -> ShownState {
    let live = match rec.state {
        FleetState::Done => return ShownState::Done,
        FleetState::Idle => ShownState::Idle,
        FleetState::Working => ShownState::Working,
        FleetState::WaitingPermission => ShownState::WaitingPermission,
    };
    let alive = match (rec.pid, rec.pid_start) {
        (Some(pid), Some(start)) => probe(pid) == Some(start),
        (Some(pid), None) => probe(pid).is_some(),
        (None, _) => (now - rec.updated_at).num_seconds() < STALE_WITHOUT_PID_SECS,
    };
    if alive {
        live
    } else {
        ShownState::Stale
    }
}

/// Every readable `*.json` record in `dir`. Missing dir = empty fleet.
pub fn load_records(dir: &Path) -> Vec<SessionRecord> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json")?;
            if !valid_session_id(id) {
                return None;
            }
            read_record(&e.path()).filter(|r| r.session_id == id)
        })
        .collect()
}

/// Sorted rows: blocked first, then working, idle, stale, done; newest first.
pub fn rows(
    records: Vec<SessionRecord>,
    now: DateTime<Utc>,
    all: bool,
    probe: &dyn Fn(u32) -> Option<u64>,
) -> Vec<FleetRow> {
    let mut out: Vec<FleetRow> = records
        .into_iter()
        .map(|r| FleetRow {
            state: shown_state(&r, now, probe),
            record: r,
        })
        .filter(|row| {
            all || !matches!(row.state, ShownState::Done | ShownState::Stale)
                || (now - row.record.updated_at).num_seconds() < HIDE_FINISHED_AFTER_SECS
        })
        .collect();
    out.sort_by(|a, b| {
        a.state
            .cmp(&b.state)
            .then(b.record.updated_at.cmp(&a.record.updated_at))
    });
    out
}

/// `12s`, `4m`, `3h`, `2d`.
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// `~/…` for paths under home, then the tail kept within `max` chars.
fn short_cwd(cwd: &str, home: Option<&Path>, max: usize) -> String {
    let mut s = cwd.to_string();
    if let Some(h) = home.and_then(Path::to_str).filter(|h| !h.is_empty()) {
        if let Some(rest) = s.strip_prefix(h) {
            if rest.is_empty() || rest.starts_with('/') {
                s = format!("~{rest}");
            }
        }
    }
    crate::text_safety::tail_snippet(&s, max)
}

fn paint(s: &str, rgb: (u8, u8, u8), bold: bool, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    let c = s.truecolor(rgb.0, rgb.1, rgb.2);
    if bold {
        c.bold().to_string()
    } else {
        c.to_string()
    }
}

fn state_rgb(s: ShownState) -> (u8, u8, u8) {
    match s {
        ShownState::WaitingPermission => (255, 176, 0),
        ShownState::Working => (0, 220, 120),
        ShownState::Idle => (0, 200, 220),
        ShownState::Stale => (220, 80, 80),
        ShownState::Done => (96, 104, 120),
    }
}

fn detail(row: &FleetRow) -> String {
    let r = &row.record;
    let d = match row.state {
        ShownState::WaitingPermission => r.pending.clone(),
        ShownState::Working => r.last_tool.clone().or_else(|| r.last_prompt.clone()),
        ShownState::Idle | ShownState::Stale => {
            r.last_prompt.clone().or_else(|| r.last_tool.clone())
        }
        ShownState::Done => r.end_reason.clone().map(|x| format!("ended: {x}")),
    };
    snippet(&d.unwrap_or_else(|| "—".into()), 90)
}

/// The human listing. `color = false` gives plain text (tests, pipes).
pub fn render(rows: &[FleetRow], now: DateTime<Utc>, dir: &Path, color: bool) -> String {
    let grey = (96, 104, 120);
    let cyan = (0, 200, 220);
    let rail = paint("┊", grey, false, color);
    let bar = paint(&"━".repeat(76), cyan, false, color);
    let home = dirs::home_dir();
    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("  {bar}\n"));
    out.push_str(&format!(
        "  {} Fleet  {}\n",
        paint("▶", cyan, true, color),
        paint(
            &format!(
                "({} Claude Code sessions in {})",
                rows.len(),
                short_cwd(&dir.to_string_lossy(), home.as_deref(), 40)
            ),
            grey,
            false,
            color
        )
    ));
    out.push_str(&format!("  {bar}\n"));
    if rows.is_empty() {
        out.push_str(&format!(
            "  {rail}  no session recorded yet (hooks installed?)\n\n"
        ));
        return out;
    }
    out.push_str(&format!(
        "  {rail}  {}\n",
        paint(
            &format!(
                "{:<10} {:>5}  {:<8}  {:<32}  {}",
                "STATE", "SINCE", "SESSION", "CWD", "DETAIL"
            ),
            grey,
            false,
            color
        )
    ));
    for row in rows {
        let r = &row.record;
        let id8: String = snippet(&r.session_id, 64).chars().take(8).collect();
        let cwd = r
            .cwd
            .as_deref()
            .map(|c| short_cwd(c, home.as_deref(), 32))
            .unwrap_or_else(|| "—".into());
        let since = format_age((now - r.state_since).num_seconds());
        let bold = row.state == ShownState::WaitingPermission;
        out.push_str(&format!(
            "  {rail}  {} {:>5}  {}  {}  {}\n",
            paint(
                &format!("{:<10}", row.state.label()),
                state_rgb(row.state),
                bold,
                color
            ),
            paint(&since, (180, 190, 200), false, color),
            paint(&format!("{id8:<8}"), cyan, false, color),
            paint(&format!("{cwd:<32}"), (150, 160, 175), false, color),
            paint(
                &detail(row),
                if bold { (255, 176, 0) } else { (150, 160, 175) },
                false,
                color
            ),
        ));
    }
    let count = |s: ShownState| rows.iter().filter(|r| r.state == s).count();
    let summary: Vec<String> = [
        ShownState::WaitingPermission,
        ShownState::Working,
        ShownState::Idle,
        ShownState::Stale,
        ShownState::Done,
    ]
    .into_iter()
    .filter(|s| count(*s) > 0)
    .map(|s| format!("{} {}", count(s), s.label()))
    .collect();
    out.push('\n');
    out.push_str(&format!(
        "  {}\n\n",
        paint(&summary.join("  ·  "), grey, false, color)
    ));
    out
}

/// `x0ne fleet ls [--json] [--all]`. Reads only.
pub fn run_ls(json: bool, all: bool) -> Result<()> {
    let dir = fleet_dir();
    let now = Utc::now();
    let rows = rows(load_records(&dir), now, all, &process_start);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows_json(&rows))?);
    } else {
        print!(
            "{}",
            render(
                &rows,
                now,
                &dir,
                colored::control::SHOULD_COLORIZE.should_colorize()
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
    }

    fn ev(event: &str) -> HookEvent {
        HookEvent {
            session_id: "s1".into(),
            event: event.into(),
            ..HookEvent::default()
        }
    }

    fn payload(event: &str, extra: &str) -> Vec<u8> {
        format!(
            r#"{{"session_id":"abc-123","transcript_path":"/x","cwd":"/home/u/p","permission_mode":"default","hook_event_name":"{event}"{extra}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_full_turn_walks_the_states() {
        let mut r = apply(None, &ev("SessionStart"), t(0), None);
        assert_eq!(r.state, FleetState::Idle);
        let mut p = ev("UserPromptSubmit");
        p.prompt = Some("fix the build".into());
        r = apply(Some(r), &p, t(1), None);
        assert_eq!((r.state, r.state_since), (FleetState::Working, t(1)));
        let mut tool = ev("PreToolUse");
        tool.tool_name = Some("Bash".into());
        tool.tool_summary = Some("cargo test".into());
        r = apply(Some(r), &tool, t(2), None);
        assert_eq!(r.last_tool.as_deref(), Some("Bash: cargo test"));
        assert_eq!(r.state_since, t(1), "same state keeps its start");
        let mut perm = tool.clone();
        perm.event = "PermissionRequest".into();
        r = apply(Some(r), &perm, t(3), None);
        assert_eq!(r.state, FleetState::WaitingPermission);
        assert_eq!(r.pending.as_deref(), Some("Bash: cargo test"));
        let mut post = tool.clone();
        post.event = "PostToolUse".into();
        r = apply(Some(r), &post, t(4), None);
        assert_eq!((r.state, r.pending.clone()), (FleetState::Working, None));
        r = apply(Some(r), &ev("Stop"), t(5), None);
        assert_eq!(r.state, FleetState::Idle);
        let mut end = ev("SessionEnd");
        end.reason = Some("prompt_input_exit".into());
        r = apply(Some(r), &end, t(6), None);
        assert_eq!(r.state, FleetState::Done);
        assert_eq!(r.end_reason.as_deref(), Some("prompt_input_exit"));
        assert_eq!(r.events, 7);
        assert_eq!(r.last_prompt.as_deref(), Some("fix the build"));
    }

    #[test]
    fn notifications_set_waiting_and_idle_and_ignore_the_rest() {
        let r = apply(None, &ev("UserPromptSubmit"), t(0), None);
        let mut n = ev("Notification");
        n.notification_type = Some("permission_prompt".into());
        n.message = Some("Claude needs your permission to use Bash".into());
        let r = apply(Some(r), &n, t(1), None);
        assert_eq!(r.state, FleetState::WaitingPermission);
        assert!(r.pending.unwrap().contains("permission to use Bash"));
        let r = apply(None, &ev("UserPromptSubmit"), t(0), None);
        n.notification_type = Some("auth_success".into());
        let r = apply(Some(r), &n, t(1), None);
        assert_eq!(r.state, FleetState::Working, "unknown type changes nothing");
        n.notification_type = Some("idle_prompt".into());
        assert_eq!(apply(Some(r), &n, t(2), None).state, FleetState::Idle);
    }

    #[test]
    fn unknown_events_and_late_events_do_not_move_the_state() {
        let r = apply(None, &ev("UserPromptSubmit"), t(0), None);
        let r = apply(Some(r), &ev("CwdChanged"), t(1), None);
        assert_eq!(
            (r.state, r.last_event.as_str()),
            (FleetState::Working, "CwdChanged")
        );
        let r = apply(Some(r), &ev("SessionEnd"), t(2), None);
        let r = apply(Some(r), &ev("Stop"), t(3), None);
        assert_eq!(
            r.state,
            FleetState::Done,
            "a late Stop revived a finished session"
        );
        let r = apply(Some(r), &ev("SessionStart"), t(4), None);
        assert_eq!(r.state, FleetState::Idle, "resume starts it again");
    }

    #[test]
    fn session_ids_that_are_not_file_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "",
            "../../etc/passwd",
            "a/b",
            "a b",
            ".hidden",
            "x\0y",
            &"a".repeat(129),
        ] {
            let mut e = ev("Stop");
            e.session_id = bad.into();
            assert!(record_event(dir.path(), &e, t(0), None).is_err(), "{bad:?}");
        }
        assert_eq!(fs::read_dir(dir.path()).map(|d| d.count()).unwrap_or(0), 0);
    }

    #[test]
    fn garbage_payloads_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        for raw in [
            &b""[..],
            b"not json",
            b"[]",
            b"42",
            b"{\"session_id\": 7}",
            b"{\"hook_event_name\":\"Stop\"}",
            b"\xff\xfe\x00garbage",
            b"{\"session_id\":\"ok\",",
        ] {
            assert!(record_payload(dir.path(), raw, false, None, t(0), None).is_err());
        }
        assert!(load_records(dir.path()).is_empty());
    }

    #[test]
    fn wrong_types_and_unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let raw = br#"{"session_id":"s-9","hook_event_name":"PreToolUse","cwd":12,"tool_name":["x"],"tool_input":"str","future_field":{"a":1}}"#;
        let r = record_payload(dir.path(), raw, false, None, t(0), None).unwrap();
        assert_eq!(
            (r.state, r.cwd, r.last_tool),
            (FleetState::Working, None, None)
        );
    }

    #[test]
    fn terminal_escapes_in_the_payload_are_folded() {
        let dir = tempfile::tempdir().unwrap();
        let raw = payload(
            "PermissionRequest",
            r#","tool_name":"Bash","tool_input":{"command":"rm -rf /\u001b[2K\r  ls"}"#,
        );
        let r = record_payload(dir.path(), &raw, false, None, t(0), None).unwrap();
        let p = r.pending.unwrap();
        assert!(!p.contains('\u{1b}') && !p.contains('\r'), "{p:?}");
    }

    #[test]
    fn the_event_argument_is_only_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let r = record_payload(
            dir.path(),
            br#"{"session_id":"a"}"#,
            false,
            Some("Stop"),
            t(0),
            None,
        )
        .unwrap();
        assert_eq!(r.last_event, "Stop");
        let raw = payload("UserPromptSubmit", "");
        let r = record_payload(dir.path(), &raw, false, Some("Stop"), t(1), None).unwrap();
        assert_eq!(r.last_event, "UserPromptSubmit");
    }

    #[test]
    fn a_huge_payload_is_bounded_and_still_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let big = "A".repeat(MAX_PAYLOAD + 1024);
        let raw = payload(
            "PostToolUse",
            &format!(
                r#","tool_name":"Read","tool_input":{{"file_path":"/f"}},"tool_output":"{big}""#
            ),
        );
        let (head, truncated) = read_bounded(&raw[..]).unwrap();
        assert!(truncated);
        assert_eq!(head.len(), MAX_PAYLOAD);
        let r = record_payload(dir.path(), &head, truncated, None, t(0), None).unwrap();
        assert_eq!(r.session_id, "abc-123");
        assert_eq!(r.last_event, "PostToolUse");
        assert_eq!(r.last_tool.as_deref(), Some("Read"));
        let on_disk = fs::metadata(dir.path().join("abc-123.json")).unwrap().len();
        assert!(on_disk < 4096, "state file is {on_disk} bytes");
        // Within bounds, the full parse is used.
        let (small, t2) = read_bounded(&payload("Stop", "")[..]).unwrap();
        assert!(!t2 && small.ends_with(b"}"));
    }

    #[test]
    fn long_strings_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let long = "p".repeat(100_000);
        let raw = payload("UserPromptSubmit", &format!(r#","prompt":"{long}""#));
        let r = record_payload(dir.path(), &raw, false, None, t(0), None).unwrap();
        assert_eq!(r.last_prompt.unwrap().chars().count(), 120);
    }

    #[test]
    fn concurrent_writers_never_corrupt_and_never_lose_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        let raw = payload(
                            "PreToolUse",
                            &format!(r#","tool_name":"T{i}","tool_input":{{"command":"{j}"}}"#),
                        );
                        record_payload(&p, &raw, false, None, Utc::now(), None).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let recs = load_records(dir.path());
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].events, 160, "the lock lost updates");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("fleet");
        record_payload(&dir, &payload("Stop", ""), false, None, t(0), None).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("abc-123.json")), 0o600);
        assert_eq!(mode(&dir.join(".abc-123.lock")), 0o600);
    }

    fn rec(id: &str, state: FleetState, at: i64) -> SessionRecord {
        let mut e = ev("");
        e.session_id = id.into();
        let mut r = apply(None, &e, t(at), None);
        r.state = state;
        r
    }

    #[test]
    fn stale_when_the_process_is_gone_or_its_pid_was_reused() {
        let now = t(100);
        let probe = |pid: u32| match pid {
            10 => Some(555),
            11 => Some(999),
            _ => None,
        };
        let mut r = rec("a", FleetState::Working, 90);
        r.pid = Some(10);
        r.pid_start = Some(555);
        assert_eq!(shown_state(&r, now, &probe), ShownState::Working);
        r.pid = Some(11);
        assert_eq!(
            shown_state(&r, now, &probe),
            ShownState::Stale,
            "pid reused"
        );
        r.pid = Some(12);
        assert_eq!(shown_state(&r, now, &probe), ShownState::Stale, "pid gone");
        r.state = FleetState::Done;
        assert_eq!(shown_state(&r, now, &probe), ShownState::Done);
        let mut nopid = rec("b", FleetState::Idle, 0);
        assert_eq!(shown_state(&nopid, t(60), &probe), ShownState::Idle);
        nopid.updated_at = t(-STALE_WITHOUT_PID_SECS);
        assert_eq!(shown_state(&nopid, t(1), &probe), ShownState::Stale);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_proc_probe_sees_us_alive_and_a_reaped_child_gone() {
        let me = std::process::id();
        let start = process_start(me).expect("own /proc/self/stat");
        let mut r = rec("a", FleetState::Working, 0);
        r.updated_at = Utc::now();
        r.pid = Some(me);
        r.pid_start = Some(start);
        assert_eq!(
            shown_state(&r, Utc::now(), &process_start),
            ShownState::Working
        );
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        r.pid = Some(dead);
        assert_eq!(
            shown_state(&r, Utc::now(), &process_start),
            ShownState::Stale
        );
    }

    #[test]
    fn listing_sorts_blocked_first_and_hides_old_finished_sessions() {
        let now = t(200_000);
        let alive = |_: u32| Some(1);
        let mut recs = vec![
            rec("idle", FleetState::Idle, 199_990),
            rec("work", FleetState::Working, 199_000),
            rec("wait", FleetState::WaitingPermission, 150_000),
            rec("done-new", FleetState::Done, 199_999),
            rec("done-old", FleetState::Done, 0),
        ];
        for r in &mut recs {
            r.pid = Some(1);
            r.pid_start = Some(1);
        }
        let ids = |rows: &[FleetRow]| {
            rows.iter()
                .map(|r| r.record.session_id.clone())
                .collect::<Vec<_>>()
        };
        let shown = rows(recs.clone(), now, false, &alive);
        assert_eq!(ids(&shown), ["wait", "work", "idle", "done-new"]);
        assert_eq!(rows(recs, now, true, &alive).len(), 5);
    }

    #[test]
    fn load_skips_foreign_and_tampered_files() {
        let dir = tempfile::tempdir().unwrap();
        record_payload(dir.path(), &payload("Stop", ""), false, None, t(0), None).unwrap();
        fs::write(dir.path().join("junk.json"), b"{nope").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::write(
            dir.path().join("big.json"),
            vec![b' '; MAX_STATE_FILE as usize + 1],
        )
        .unwrap();
        // A file whose content claims another session is not trusted.
        let r = rec("someone-else", FleetState::Idle, 0);
        fs::write(
            dir.path().join("liar.json"),
            serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
        let recs = load_records(dir.path());
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].session_id, "abc-123");
        assert!(load_records(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn plain_listing_format() {
        let now = t(3_700);
        let mut w = rec("0123456789abcdef", FleetState::WaitingPermission, 0);
        w.state_since = t(3_580);
        w.cwd = Some("/srv/very/long/path/to/some/project/that/does/not/fit".into());
        w.pending = Some("Bash: rm -rf target\u{1b}[2K".into());
        w.pid = Some(1);
        w.pid_start = Some(1);
        let rows = rows(vec![w], now, false, &|_| Some(1));
        let out = render(&rows, now, Path::new("/tmp/fleet"), false);
        assert!(!out.contains('\u{1b}'), "{out}");
        let line = out.lines().find(|l| l.contains("permission")).unwrap();
        assert!(line.contains("  2m  01234567  …"), "{line}");
        assert!(line.contains("that/does/not/fit"), "{line}");
        assert!(line.contains("Bash: rm -rf target"), "{line}");
        assert!(out.contains("1 permission"), "{out}");
        assert!(out.contains("STATE") && out.contains("DETAIL"));
        let empty = render(&[], now, Path::new("/tmp/fleet"), false);
        assert!(empty.contains("no session recorded yet"));
    }

    #[test]
    fn json_listing_carries_the_shown_state() {
        let mut r = rec("abc", FleetState::Working, 0);
        r.pid = Some(3);
        r.pid_start = Some(3);
        let rows = rows(vec![r], t(10), false, &|_| None);
        let v = rows_json(&rows);
        assert_eq!(v[0]["state"], "stale");
        assert_eq!(v[0]["session_id"], "abc");
        assert_eq!(v[0]["recorded_state"], "working");
        assert!(v[0].get("state_since").is_some());
    }

    #[test]
    fn ages_read_short() {
        assert_eq!(
            [0, 59, 60, 3599, 3600, 86_400, -5].map(format_age),
            ["0s", "59s", "1m", "59m", "1h", "1d", "0s"]
        );
    }

    #[test]
    fn home_is_abbreviated_only_on_a_path_boundary() {
        let h = Path::new("/home/u");
        assert_eq!(short_cwd("/home/u/p", Some(h), 40), "~/p");
        assert_eq!(short_cwd("/home/u", Some(h), 40), "~");
        assert_eq!(short_cwd("/home/uv/p", Some(h), 40), "/home/uv/p");
    }
}
