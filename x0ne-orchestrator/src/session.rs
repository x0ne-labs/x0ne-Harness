use crate::agent::journal::{ActionJournal, DenialKind};
use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Longest single string this log will reproduce, in bytes.
///
/// Mirrors `x0ne-runtime`'s own capture ceiling
/// (`executor/process.rs:156`, `MAX_CAPTURE_BYTES = 24_000`) rather than
/// importing it: that constant is `pub(crate)` in the runtime, and the
/// journal must bound results the runtime never touched either — an
/// orchestrator-side `ReadFile`, a sysaicall answer, a `Pipeline` tally. A
/// separate constant with the same value says "same ceiling, different
/// enforcer", which is the true statement.
const MAX_JOURNALED_STRING_BYTES: usize = 24_000;

/// Ceiling on a whole `action_result` line. A single bounded string is not
/// enough: `ReadFiles` over a hundred files is a hundred strings, each under
/// the per-string bound, and an audit log is not a copy of the filesystem.
const MAX_JOURNALED_RESULT_BYTES: usize = 64 * 1024;

/// Create `dir` if needed, owner-only, and tighten it if it already exists
/// with wider permissions.
///
/// The tightening is deliberate and it does change the operator's machine:
/// every install before this created `~/.x0ne/sessions` at 0775, so leaving
/// existing directories alone would mean the fix only protects new machines.
/// One `chmod 700` on a directory that holds nothing but the operator's own
/// transcripts is the smallest thing that makes the README's "treat sessions
/// as chmod 600" true where it matters.
async fn private_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    tighten_dir(dir);
    Ok(())
}

fn private_dir_blocking(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    tighten_dir(dir);
    Ok(())
}

fn tighten_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Reduce a client-supplied session id to something that can only ever be a
/// file name.
///
/// SECURITY: the HTTP session id reaches this from the network
/// (`serve.rs`). Anything but `[A-Za-z0-9_-]` is dropped — a `/` or a `..`
/// surviving here would let a caller choose where the server writes.
fn safe_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect()
}

/// `~/.x0ne/sessions`, owner-only.
pub(crate) fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join("sessions")
}

/// The id a journal path carries.
///
/// A path is the only place a session's identity is durable — the process
/// that wrote it is gone by the time anyone resumes. Falls back to the raw
/// stem when the name is not a valid id (a hand-copied file, an older build),
/// so an odd file still reports *something* in `/session` rather than an
/// empty label; such a session cannot be resumed, and
/// `session_store::id_from_path` is what says so.
fn id_of(path: &Path) -> String {
    crate::session_store::id_from_path(path).unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string()
    })
}

/// Largest index `<= n` that starts a character. Truncating on a raw byte
/// index panics mid-codepoint, and a log that can panic on the output it is
/// recording is worse than no log (same class as an earlier `char_indices`
/// defect).
pub(crate) fn floor_char_boundary(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    let mut i = n;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Truncate every string in `value` to [`MAX_JOURNALED_STRING_BYTES`], marking
/// what was dropped. Structure is preserved — `success`, `exit_code`,
/// `error.class` all survive — because those are what an audit is read for.
fn bound_strings(value: &Value) -> Value {
    match value {
        Value::String(s) if s.len() > MAX_JOURNALED_STRING_BYTES => {
            let cut = floor_char_boundary(s, MAX_JOURNALED_STRING_BYTES);
            Value::String(format!(
                "{}… [journal: {} bytes truncated to {}]",
                &s[..cut],
                s.len(),
                cut
            ))
        }
        Value::Array(items) => Value::Array(items.iter().map(bound_strings).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), bound_strings(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// What gets written for a result: bounded per string, and replaced by a
/// summary if the whole thing is still too large to belong in a log.
fn bounded_result(result: &Value) -> Value {
    let bounded = bound_strings(result);
    let len = serde_json::to_string(&bounded).map_or(0, |s| s.len());
    if len <= MAX_JOURNALED_RESULT_BYTES {
        return bounded;
    }
    json!({
        "success": result.get("success").cloned().unwrap_or(Value::Null),
        "error": result.get("error").map(bound_strings),
        "journal_omitted": {
            "reason": "result exceeds the journal ceiling",
            "bytes": len,
            "ceiling": MAX_JOURNALED_RESULT_BYTES,
        }
    })
}

/// Appends structured JSONL events to a per-session log file.
/// Each line is a self-contained JSON object with a `ts` and `type` field.
///
/// Event types:
/// - `session_start`: emitted once at creation with model/version/host metadata
/// - `user_input`: user prompt text
/// - `llm_response`: full LLM response with token counts
/// - `action`: action JSON with reasoning, success flag, and duration
/// - `action_result`: full result payload for audit
/// - `policy_denial`: action blocked by policy engine
pub struct SessionLogger {
    file: File,
    #[allow(dead_code)]
    pub path: PathBuf,
    /// This session's stable name — the file stem, and the argument
    /// `x0ne --resume <id>` takes. Held rather than recomputed so the value
    /// the banner prints and the value the file is named by cannot drift.
    id: String,
    /// Hash chain over every line this process writes (`x0ne audit verify`).
    chain: x0ne_audit::Chain,
}

/// A chain over `path`'s current bytes and its header line.
fn open_chain(path: &Path, id: &str) -> (x0ne_audit::Chain, String) {
    let preceding = std::fs::read(path).unwrap_or_default();
    let (signer, why) = x0ne_audit::Signer::load_default();
    x0ne_audit::Chain::open(&preceding, id, signer, &why)
}

impl SessionLogger {
    /// Create a new session log in `~/.x0ne/sessions/YYYY-MM-DD_HH-MM-SS.jsonl`.
    ///
    /// The directory is owner-only and so is the file. A session log holds
    /// every prompt, every model answer and every command run — the README
    /// tells operators to "treat sessions as chmod 600", and until this it was
    /// only advice: the directory was created 0775 and the files 0664 under
    /// the usual umask, world-readable on any shared machine.
    pub async fn new() -> Result<Self> {
        let base = sessions_dir();
        private_dir(&base).await?;
        let id = crate::session_store::new_session_id();
        Self::at(crate::session_store::journal_path(&base, &id)).await
    }

    /// Blocking twin of [`SessionLogger::at`].
    ///
    /// For the one assembly point that is not async: `serve` builds its
    /// session inside a synchronous factory closure handed to the session
    /// registry (`serve.rs:815`). Opening a file is a syscall, not a wait, so
    /// this is not a blocking call worth an executor hop — and the alternative
    /// was leaving the headless surface, the one nobody is watching, as the
    /// only path with no audit trail.
    pub fn at_blocking(path: PathBuf) -> Result<Self> {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let id = id_of(&path);
        let (chain, header) = open_chain(&path, &id);
        std::io::Write::write_all(&mut file, header.as_bytes())?;
        Ok(Self {
            file: File::from_std(file),
            id,
            chain,
            path,
        })
    }

    /// Open (or create) a session log at an explicit path, owner-only.
    ///
    /// Public so a test can read back what a real turn wrote without touching
    /// `~/.x0ne` — the property under test is "the canonical path journals",
    /// and it is not testable if the destination is a constant.
    pub async fn at(path: PathBuf) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            // SECURITY: at creation, before a single byte is written. Setting
            // the mode afterwards leaves a window where the log exists and is
            // readable by everyone.
            opts.mode(0o600);
        }
        let mut file = opts.open(&path).await?;
        #[cfg(unix)]
        {
            // The mode above only applies to a file this call creates. An
            // existing log — an appended session, or one written by an older
            // build — keeps whatever it had, so tighten it too.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        let id = id_of(&path);
        let (chain, header) = open_chain(&path, &id);
        file.write_all(header.as_bytes()).await?;
        file.flush().await?;
        Ok(Self {
            id,
            chain,
            file,
            path,
        })
    }

    /// This session's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Emit a session_start header with metadata for audit correlation.
    /// `cwd` and `git_root` place the session in its project
    /// (`session_store::Project`), which `x0ne sessions` lists by.
    pub async fn log_session_start(&mut self, model: &str) -> Result<()> {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "unknown".into());
        let project = crate::session_store::Project::current();
        let cwd = project
            .as_ref()
            .map(|p| p.cwd.to_string_lossy().into_owned());
        let git_root = project
            .and_then(|p| p.git_root)
            .map(|r| r.to_string_lossy().into_owned());
        self.write(json!({
            "ts": ts(),
            "type": "session_start",
            // The id is written into the file as well as being its name.
            // A journal copied elsewhere, or renamed, still says which
            // session it is — the name alone is metadata anyone can lose.
            "session_id": self.id,
            "version": env!("CARGO_PKG_VERSION"),
            "model": model,
            "hostname": hostname,
            "pid": std::process::id(),
            "cwd": cwd,
            "git_root": git_root,
        }))
        .await
    }

    /// Name the session. Appended like any event: the header and every
    /// earlier line stay as the hash chain sealed them, and the reader
    /// (`session_store::summarize`) takes the latest title. Sealed like a
    /// turn, so the title is under a signed head when a key exists.
    pub async fn log_session_renamed(&mut self, title: &str) -> Result<()> {
        let id = self.id.clone();
        self.write(json!({
            "ts": ts(),
            "type": "session_renamed",
            "session_id": id,
            "title": title,
        }))
        .await?;
        self.seal().await
    }

    /// Sign the chain's head, when a key exists.
    async fn seal(&mut self) -> Result<()> {
        if let Some((line, _)) = self.chain.seal() {
            self.file.write_all(line.as_bytes()).await?;
            self.file.flush().await?;
        }
        Ok(())
    }

    /// Log a user message.
    pub async fn log_user(&mut self, content: &str) -> Result<()> {
        self.write(json!({
            "ts": ts(),
            "type": "user_input",
            "content": content
        }))
        .await
    }

    /// Log an LLM response with its token counts.
    ///
    /// Cache traffic is recorded separately because Anthropic reports it
    /// separately: with prompt caching on, `tokens_in` is the small
    /// uncached remainder, so a log carrying only that showed `tokens_in: 2`
    /// for turns that had moved thousands (field report, 2026-08-12). A log
    /// used to diagnose cost has to show the whole bill.
    pub async fn log_llm(
        &mut self,
        content: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> Result<()> {
        self.write(json!({
            "ts": ts(),
            "type": "llm_response",
            "content": content,
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "cache_write_tokens": cache_write,
            "cache_read_tokens": cache_read,
            "tokens_total": tokens_in + tokens_out + cache_write + cache_read
        }))
        .await
    }

    /// Log an action and its result: the two lines the README promises for
    /// every action — what was asked, then what happened.
    ///
    /// The result is bounded (`MAX_JOURNALED_STRING_BYTES`, and
    /// `MAX_JOURNALED_RESULT_BYTES` for the whole line). An audit trail that
    /// a single `cat` of a large file can turn into a gigabyte is one an
    /// operator deletes, and a deleted log audits nothing.
    pub async fn log_action(
        &mut self,
        action: &Value,
        result: &Value,
        duration_ms: u64,
    ) -> Result<()> {
        let result = &bounded_result(result);
        self.write(json!({
            "ts": ts(),
            "type": "action",
            "action_type": action["type"].as_str().unwrap_or("unknown"),
            "action": action,
            "success": result["success"].as_bool().unwrap_or(false),
            "duration_ms": duration_ms
        }))
        .await?;
        // Full result for audit trail — includes output, error, etc.
        self.write(json!({
            "ts": ts(),
            "type": "action_result",
            "action_type": action["type"].as_str().unwrap_or("unknown"),
            "result": result
        }))
        .await
    }

    /// Log a turn summary: how the turn ENDED and what it did. This is the
    /// event that makes a session log diagnosable after the fact — the
    /// 2026-08-10 failure sessions contained only user_input/llm_response
    /// pairs, so an empty response carried no cause at all.
    pub async fn log_turn(
        &mut self,
        outcome_kind: &str,
        detail: Option<&str>,
        duration_ms: u64,
        model_calls: u32,
        actions: &Value,
    ) -> Result<()> {
        self.write(json!({
            "ts": ts(),
            "type": "turn",
            "outcome": outcome_kind,
            // `outcome` alone collapsed four different ceilings into
            // `budget_exhausted`, and a log that says only that leaves the
            // cause a guess (field report 2026-08-13: a 35-minute turn whose
            // reason could not be recovered from its own record).
            "detail": detail,
            "duration_ms": duration_ms,
            "model_calls": model_calls,
            "actions": actions,
        }))
        .await?;
        // Sign the head at the end of every turn, when a key exists.
        self.seal().await
    }

    /// Log a refused action, and by whom it was refused.
    ///
    /// It carried `#[allow(dead_code)]` and a comment promising a future
    /// caller until 2026-08-15; it now has three, all on the canonical path
    /// (`Driver::run_action`). `kind` is what the old signature lacked: a
    /// human saying no, a context with no human to ask, and the runtime's
    /// policy engine are three different security events, and a log that
    /// spells them the same way cannot answer "was anyone actually asked?".
    pub async fn log_policy_denial(
        &mut self,
        action: &Value,
        kind: DenialKind,
        reason: &str,
    ) -> Result<()> {
        self.write(json!({
            "ts": ts(),
            "type": "policy_denial",
            "denial": kind.as_str(),
            "action_type": action["type"].as_str().unwrap_or("unknown"),
            "action": action,
            "reason": reason
        }))
        .await
    }

    /// A test/build result stated without evidence (`agent::honesty`).
    /// One `unverified_claim` line per detection, the detection's fields
    /// merged in (checkpoint, claims, missing tools, outcome).
    /// A read result addressed the model: one `prompt_injection_suspected` line.
    pub async fn log_injection_suspected(&mut self, detection: &Value) -> Result<()> {
        let mut line = json!({"ts": ts(), "type": "prompt_injection_suspected"});
        if let (Some(obj), Some(fields)) = (line.as_object_mut(), detection.as_object()) {
            for (k, v) in fields {
                if k != "ts" && k != "type" {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        self.write(line).await
    }

    pub async fn log_unverified_claim(&mut self, detection: &Value) -> Result<()> {
        let mut line = json!({"ts": ts(), "type": "unverified_claim"});
        if let (Some(obj), Some(fields)) = (line.as_object_mut(), detection.as_object()) {
            for (k, v) in fields {
                if k != "ts" && k != "type" {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        self.write(line).await
    }

    /// The operator widened his own policy in the middle of a session.
    ///
    /// SECURITY: `policy_grant` is its own event type, greppable and distinct
    /// from `policy_denial`, because they answer opposite questions. An audit
    /// asking "was this session's policy the one on disk?" reads exactly these
    /// lines and nothing else — the runtime keeps its grants in memory and
    /// writes no log of its own.
    pub async fn log_policy_grant(
        &mut self,
        action: &Value,
        kind: &str,
        value: &str,
        persist: bool,
        persisted: Option<&str>,
    ) -> Result<()> {
        self.write(json!({
            "ts": ts(),
            "type": "policy_grant",
            "kind": kind,
            "value": value,
            "persist": persist,
            "persisted": persisted,
            "action_type": action["type"].as_str().unwrap_or("unknown"),
            "action": action,
        }))
        .await
    }

    /// Mark that this journal was reopened and what came back out of it.
    ///
    /// Written on resume, into the SAME file: a resumed session is one
    /// session with a gap, not two sessions that happen to share a name. An
    /// auditor reading the file top to bottom sees where the process died and
    /// where it picked up, and `broken_lines` / `bytes_skipped` record that
    /// the reload was lossy — a silent reload would make the transcript
    /// after the marker look continuous with the one before it.
    pub async fn log_session_resumed(
        &mut self,
        turns_reloaded: usize,
        broken_lines: usize,
        bytes_skipped: u64,
    ) -> Result<()> {
        let id = self.id.clone();
        self.write(json!({
            "ts": ts(),
            "type": "session_resumed",
            "session_id": id,
            "resumed_at": ts(),
            "turns_reloaded": turns_reloaded,
            "broken_lines": broken_lines,
            "bytes_skipped": bytes_skipped,
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
        }))
        .await
    }

    async fn write(&mut self, value: Value) -> Result<()> {
        // F3: SECURITY.md says the log holds no secret. `Chain::line` masks
        // every string before hashing, so the hash covers the masked text.
        let line = self.chain.line(value);
        self.file.write_all(line.as_bytes()).await?;
        self.file.flush().await?;
        Ok(())
    }
}

fn ts() -> String {
    Utc::now().to_rfc3339()
}

// ── The shared handle ─────────────────────────────────────────────────────────

/// One session log, several writers.
///
/// A frontend writes `user_input`, `llm_response` and `turn`; the driver — in
/// a task the frontend has already handed the session to — writes the actions;
/// a detached world writes from a task that outlives the call that started it.
/// They must all land in the same file, and `SessionLogger::new()` picks its
/// path from the clock, so "just open another one" splits the session across
/// two logs a minute apart.
///
/// Cloning shares the file. The mutex is held for one line at a time.
#[derive(Clone)]
pub struct SharedJournal {
    inner: Arc<Mutex<SessionLogger>>,
    /// Outside the mutex on purpose. The id never changes for the life of a
    /// journal, and the banner, `/session` and the resume line all read it on
    /// paths that are not async — behind the lock it would need an `.await`
    /// to print a constant.
    id: Arc<str>,
}

impl SharedJournal {
    /// Open a fresh session log under `~/.x0ne/sessions`.
    pub async fn new() -> Result<Self> {
        Ok(Self::wrap(SessionLogger::new().await?))
    }

    /// Reopen an existing session log for append.
    ///
    /// The whole of "continue writing into the same file": a resumed session
    /// has no new file and no new id. `SessionLogger::at` opens with
    /// `append(true)`, so nothing already written can be overwritten, and it
    /// chmods the file to 0600 — a journal from an install that predates the
    /// permissions fix is tightened by the act of resuming it, exactly as
    /// `tighten_dir` does for the directory.
    ///
    /// SECURITY: `id` must already have passed
    /// [`crate::session_store::validate_session_id`] — it is re-validated
    /// here rather than trusted, because this is the call that turns it into
    /// a path.
    pub async fn resume(id: &str) -> Result<Self> {
        Self::resume_in(&sessions_dir(), id).await
    }

    /// [`SharedJournal::resume`] against an explicit directory.
    ///
    /// The seam a test needs: the property is "the same file keeps growing",
    /// and it is not testable while the destination is `~/.x0ne`.
    pub async fn resume_in(dir: &Path, id: &str) -> Result<Self> {
        let id =
            crate::session_store::validate_session_id(id).map_err(|e| anyhow::anyhow!("{e}"))?;
        private_dir(dir).await?;
        let path = crate::session_store::journal_path(dir, id);
        if !path.is_file() {
            anyhow::bail!("no session named {id}");
        }
        Ok(Self::wrap(SessionLogger::at(path).await?))
    }

    /// Share an already-open logger.
    pub fn wrap(logger: SessionLogger) -> Self {
        let id: Arc<str> = Arc::from(logger.id.as_str());
        Self {
            inner: Arc::new(Mutex::new(logger)),
            id,
        }
    }

    /// This session's stable id — what `--resume` takes and what the banner
    /// prints.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn log_session_resumed(
        &self,
        turns_reloaded: usize,
        broken_lines: usize,
        bytes_skipped: u64,
    ) -> Result<()> {
        self.inner
            .lock()
            .await
            .log_session_resumed(turns_reloaded, broken_lines, bytes_skipped)
            .await
    }

    /// A log for one named session, opened without awaiting.
    ///
    /// `label` is client-supplied on the server path, so it is reduced to
    /// `[A-Za-z0-9_-]` and truncated: a session id is a correlation aid, and
    /// it must never be able to steer where the file lands. SECURITY: dropping
    /// this filter would make the HTTP session id a path.
    pub fn for_session(label: &str) -> Result<Self> {
        let dir = sessions_dir();
        private_dir_blocking(&dir)?;
        let safe = safe_label(label);
        let ts = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let name = if safe.is_empty() {
            format!("{ts}.jsonl")
        } else {
            format!("{ts}_{safe}.jsonl")
        };
        Ok(Self::wrap(SessionLogger::at_blocking(dir.join(name))?))
    }

    /// A handle for the driver, the session, a child or a world.
    pub fn handle(&self) -> Arc<dyn ActionJournal> {
        Arc::new(self.clone())
    }

    /// Where this session is being written. For tests and for the frontends
    /// that print the path.
    pub async fn path(&self) -> PathBuf {
        self.inner.lock().await.path.clone()
    }

    pub async fn log_session_start(&self, model: &str) -> Result<()> {
        self.inner.lock().await.log_session_start(model).await
    }

    pub async fn log_session_renamed(&self, title: &str) -> Result<()> {
        self.inner.lock().await.log_session_renamed(title).await
    }

    pub async fn log_user(&self, content: &str) -> Result<()> {
        self.inner.lock().await.log_user(content).await
    }

    pub async fn log_llm(
        &self,
        content: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> Result<()> {
        self.inner
            .lock()
            .await
            .log_llm(content, tokens_in, tokens_out, cache_write, cache_read)
            .await
    }

    pub async fn log_turn(
        &self,
        outcome_kind: &str,
        detail: Option<&str>,
        duration_ms: u64,
        model_calls: u32,
        actions: &Value,
    ) -> Result<()> {
        self.inner
            .lock()
            .await
            .log_turn(outcome_kind, detail, duration_ms, model_calls, actions)
            .await
    }
}

#[async_trait::async_trait]
impl ActionJournal for SharedJournal {
    fn session_id(&self) -> Option<String> {
        Some(self.id.to_string())
    }

    async fn journal_action(&self, action: &Value, result: &Value, duration_ms: u64) {
        // Errors are swallowed here and nowhere else: a full disk must not
        // abort a turn, and `log_action` has no recoverable failure. The trace
        // exists so a silent journal is diagnosable — the defect this whole
        // seam was built to end was precisely a journal nobody noticed.
        let mut logger = self.inner.lock().await;
        if let Err(e) = logger.log_action(action, result, duration_ms).await {
            tracing::warn!("session journal: could not record an action: {e}");
        }
    }

    async fn journal_denial(&self, action: &Value, kind: DenialKind, detail: &str) {
        let mut logger = self.inner.lock().await;
        if let Err(e) = logger.log_policy_denial(action, kind, detail).await {
            tracing::warn!("session journal: could not record a denial: {e}");
        }
    }

    async fn journal_policy_grant(
        &self,
        action: &Value,
        kind: &str,
        value: &str,
        persist: bool,
        persisted: Option<&str>,
    ) {
        let mut logger = self.inner.lock().await;
        if let Err(e) = logger
            .log_policy_grant(action, kind, value, persist, persisted)
            .await
        {
            tracing::warn!("session journal: could not record a policy grant: {e}");
        }
    }

    async fn journal_unverified_claim(&self, detection: &Value) {
        let mut logger = self.inner.lock().await;
        if let Err(e) = logger.log_unverified_claim(detection).await {
            tracing::warn!("session journal: could not record an unverified claim: {e}");
        }
    }

    async fn journal_injection_suspected(&self, detection: &Value) {
        let mut logger = self.inner.lock().await;
        if let Err(e) = logger.log_injection_suspected(detection).await {
            tracing::warn!("session journal: could not record a suspected injection: {e}");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    /// F3: an approved `cat .env` must not put the secrets in the journal.
    #[tokio::test]
    async fn a_secret_read_by_an_action_never_reaches_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut logger = SessionLogger::at(path.clone()).await.unwrap();
        let env = "ANTHROPIC_API_KEY=CANARYa1b2c3d4e5f6g7h8\nDB_PASSWORD=CANARYhunter2\n\
                   token sk-ant-api03-CANARYxY9zQ8wV7uT6sR5qP4oN3mL2\n\
                   -----BEGIN OPENSSH PRIVATE KEY-----\nCANARYb3BlbnNzaC1rZXktdjE\n\
                   -----END OPENSSH PRIVATE KEY-----\n";
        logger
            .log_action(
                &json!({"type": "ReadFile", "path": ".env"}),
                &json!({"success": true, "output": {"content": env}}),
                1,
            )
            .await
            .unwrap();
        logger.log_llm(env, 1, 1, 0, 0).await.unwrap();
        let text = fs::read_to_string(&path).await.unwrap();
        assert_eq!(text.matches("CANARY").count(), 0, "{text}");
        assert!(text.contains("[redacted]"));
        assert!(
            text.contains("\"path\":\".env\""),
            "ordinary fields stay readable"
        );
    }

    /// Every honesty detection is one greppable `unverified_claim` line.
    #[tokio::test]
    async fn an_unverified_claim_is_one_typed_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("u.jsonl");
        let journal = SharedJournal::wrap(SessionLogger::at(path.clone()).await.unwrap());
        journal
            .journal_unverified_claim(&json!({"type": "spoofed", "checkpoint": "write",
                "outcome": "refused", "missing_tools": ["cargo"]}))
            .await;
        let text = fs::read_to_string(&path).await.unwrap();
        let line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line["type"], "unverified_claim");
        assert_eq!(line["checkpoint"], "write");
        assert_eq!(line["missing_tools"], json!(["cargo"]));
    }

    #[tokio::test]
    async fn writes_valid_jsonl() {
        // Use a temp path to avoid polluting ~/.x0ne
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .unwrap();

        let mut logger = SessionLogger {
            file,
            id: id_of(&path),
            chain: x0ne_audit::Chain::open(b"", "t", None, "test").0,
            path: path.clone(),
        };

        logger.log_user("hello").await.unwrap();
        logger
            .log_llm("I'll list the dir", 100, 20, 0, 0)
            .await
            .unwrap();
        logger
            .log_action(
                &json!({"type": "ListDir", "path": "."}),
                &json!({"success": true, "output": {"entries": []}}),
                0,
            )
            .await
            .unwrap();

        // Read back and verify each line is valid JSON
        let content = fs::read_to_string(&path).await.unwrap();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let parsed: serde_json::Result<Value> = serde_json::from_str(line);
            assert!(parsed.is_ok(), "invalid JSONL line: {}", line);
        }

        // Check structure
        let events: Vec<Value> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(events[0]["type"], "user_input");
        assert_eq!(events[0]["content"], "hello");
        assert_eq!(events[1]["type"], "llm_response");
        assert_eq!(events[1]["tokens_in"], 100);
    }

    /// An audit log must not be a copy of what the command printed.
    ///
    /// `RunCommand` output already meets the runtime's 24 000-byte capture
    /// ceiling, but the journal records results the runtime never touched —
    /// an orchestrator-side read, a sysaicall answer — and one unbounded
    /// `cat` would otherwise write a gigabyte into the operator's home. A log
    /// that grows without bound is a log that gets deleted.
    #[tokio::test]
    async fn a_huge_output_is_bounded_in_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.jsonl");
        let mut logger = SessionLogger::at(path.clone()).await.unwrap();

        // A multi-byte character straddling the cut, so a byte-index
        // truncation would panic rather than fail an assertion.
        let huge = "é".repeat(MAX_JOURNALED_STRING_BYTES);
        logger
            .log_action(
                &json!({"type": "RunCommand", "command": "cat"}),
                &json!({"success": true, "output": {"stdout": huge, "exit_code": 0}}),
                12,
            )
            .await
            .unwrap();

        let content = fs::read_to_string(&path).await.unwrap();
        let result: Value = content
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|e| e["type"] == "action_result")
            .expect("the result line");
        let stdout = result["result"]["output"]["stdout"].as_str().unwrap();
        assert!(
            stdout.len() < MAX_JOURNALED_STRING_BYTES + 100,
            "the journal reproduced {} bytes of output",
            stdout.len()
        );
        assert!(
            stdout.contains("truncated"),
            "truncation must be visible, or the log lies by omission"
        );
        // What an auditor reads is still there.
        assert_eq!(result["result"]["output"]["exit_code"], 0);
        assert_eq!(result["result"]["success"], true);
    }

    /// A session id arrives from the network and names a file. It must not be
    /// able to name a *path*.
    #[test]
    fn a_client_supplied_session_id_cannot_steer_the_log_out_of_its_directory() {
        for hostile in [
            "../../etc/evil",
            "/absolute",
            "a/b/c",
            "..",
            "with space; rm -rf /",
        ] {
            let safe = safe_label(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('.') && !safe.contains(' '),
                "{hostile:?} survived as {safe:?}"
            );
        }
        assert_eq!(safe_label("sess-42_ok"), "sess-42_ok");
        assert!(safe_label(&"x".repeat(200)).len() <= 24);
    }

    /// A refusal names who refused. `human_declined` and `no_human_channel`
    /// are the difference between "someone said no" and "nobody was asked",
    /// and an audit that cannot tell them apart cannot check the fail-closed
    /// invariant at all.
    #[tokio::test]
    async fn a_denial_records_which_authority_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d.jsonl");
        let mut logger = SessionLogger::at(path.clone()).await.unwrap();

        logger
            .log_policy_denial(
                &json!({"type": "MoveToTrash", "path": "/tmp/x"}),
                DenialKind::HumanDeclined,
                "the user declined",
            )
            .await
            .unwrap();

        let content = fs::read_to_string(&path).await.unwrap();
        // Line 1 is the chain header.
        let ev: Value = serde_json::from_str(content.lines().nth(1).unwrap()).unwrap();
        assert_eq!(ev["type"], "policy_denial");
        assert_eq!(ev["denial"], "human_declined");
        assert_eq!(ev["action_type"], "MoveToTrash");
    }

    /// The header says where the session ran, so `x0ne sessions` can group
    /// it with its project.
    #[tokio::test]
    async fn the_header_records_the_directory_and_its_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("2026-10-27_10-00-00_aaaa.jsonl");
        let log = SharedJournal::wrap(SessionLogger::at(path.clone()).await.unwrap());
        log.log_session_start("m").await.unwrap();
        let content = fs::read_to_string(&path).await.unwrap();
        let start: Value = content
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|e| e["type"] == "session_start")
            .unwrap();
        let here = crate::session_store::Project::current().unwrap();
        assert_eq!(start["cwd"], json!(here.cwd.to_string_lossy()));
        assert_eq!(
            start["git_root"],
            json!(here.git_root.map(|r| r.to_string_lossy().into_owned()))
        );
    }

    /// `/rename` appends one line and rewrites none: every byte already
    /// written stays, and `x0ne audit verify` still passes on the file.
    #[tokio::test]
    async fn a_rename_is_appended_and_the_chain_still_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("2026-10-27_10-00-01_bbbb.jsonl");
        let log = SharedJournal::wrap(SessionLogger::at(path.clone()).await.unwrap());
        log.log_session_start("m").await.unwrap();
        log.log_user("hello").await.unwrap();
        let before = std::fs::read(&path).unwrap();

        log.log_session_renamed("fix the parser").await.unwrap();
        let after = std::fs::read(&path).unwrap();
        assert!(after.starts_with(&before), "an earlier line was rewritten");
        let added = String::from_utf8_lossy(&after[before.len()..]).to_string();
        assert_eq!(added.lines().count(), 1, "{added}");
        let event: Value = serde_json::from_str(added.trim()).unwrap();
        assert_eq!(
            (event["type"].as_str(), event["title"].as_str()),
            (Some("session_renamed"), Some("fix the parser"))
        );

        let report = x0ne_audit::verify_file(&path, None).unwrap();
        assert_eq!(report.status, x0ne_audit::Status::Ok, "{report:?}");
        let row = crate::session_store::summarize(&path).unwrap();
        assert_eq!(row.title.as_deref(), Some("fix the parser"));

        // The check has teeth: the same title edited in place breaks it.
        let forged = String::from_utf8(after)
            .unwrap()
            .replace("fix the parser", "fix the parsex");
        std::fs::write(&path, forged).unwrap();
        let report = x0ne_audit::verify_file(&path, None).unwrap();
        assert!(
            matches!(report.status, x0ne_audit::Status::Broken { .. }),
            "{report:?}"
        );
    }

    /// A resumed session keeps writing where it left off: same path, same
    /// id, nothing overwritten, and a marker saying the gap is a resume.
    ///
    /// A failure here means a resume forks the audit trail in two, which is
    /// the defect the whole feature exists to avoid: the operator would think
    /// they had one session and the log would hold two halves of one.
    #[tokio::test]
    async fn resume_appends_to_the_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "2026-08-16_09-41_ab12";
        let path = tmp.path().join(format!("{id}.jsonl"));

        let first = SharedJournal::wrap(SessionLogger::at(path.clone()).await.unwrap());
        assert_eq!(first.id(), id, "the id is the file stem");
        first.log_session_start("claude-x").await.unwrap();
        first.log_user("before the crash").await.unwrap();
        drop(first);

        let again = SharedJournal::resume_in(tmp.path(), id).await.unwrap();
        assert_eq!(again.id(), id);
        assert_eq!(again.path().await, path, "resume opened a different file");
        again.log_session_resumed(1, 0, 0).await.unwrap();
        again.log_user("after the resume").await.unwrap();

        let content = fs::read_to_string(&path).await.unwrap();
        let events: Vec<Value> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec![
                "chain_header",
                "session_start",
                "user_input",
                "chain_header",
                "session_resumed",
                "user_input"
            ],
            "the earlier lines must survive, in order, ahead of the new ones"
        );
        assert_eq!(events[1]["session_id"], id);
        assert_eq!(events[4]["session_id"], id);
        assert_eq!(events[4]["turns_reloaded"], 1);
        assert!(events[4]["resumed_at"].is_string());
        assert_eq!(events[5]["content"], "after the resume");
        // The second chain's header binds the first one's bytes.
        let r = x0ne_audit::verify_file(&path, None).unwrap();
        assert_eq!(r.status, x0ne_audit::Status::Ok, "{r:?}");

        assert!(SharedJournal::resume_in(tmp.path(), "ghost").await.is_err());
        // SECURITY: the path-shaped argument never reaches the filesystem.
        assert!(SharedJournal::resume_in(tmp.path(), "../../etc/passwd")
            .await
            .is_err());
    }

    /// SECURITY: a journal from an install that predates the permissions fix
    /// is tightened by the act of resuming it. Resuming must never be the one
    /// path that leaves a world-readable transcript world-readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn resuming_an_old_journal_tightens_it_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let id = "legacy";
        let path = tmp.path().join("legacy.jsonl");
        std::fs::write(&path, "{\"type\":\"user_input\",\"content\":\"hi\"}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _j = SharedJournal::resume_in(tmp.path(), id).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a resumed journal stayed at {mode:o}");
    }

    #[tokio::test]
    async fn turn_event_carries_outcome_and_action_tally() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .unwrap();
        let mut logger = SessionLogger {
            file,
            id: id_of(&path),
            chain: x0ne_audit::Chain::open(b"", "t", None, "test").0,
            path: path.clone(),
        };

        logger
            .log_turn(
                "model_failure",
                Some("the model returned an error after 3 attempts"),
                1234,
                2,
                &json!({"total": 3, "succeeded": 2, "failed": 1}),
            )
            .await
            .unwrap();

        let content = fs::read_to_string(&path).await.unwrap();
        let ev: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(ev["type"], "turn");
        assert_eq!(ev["outcome"], "model_failure");
        assert_eq!(ev["duration_ms"], 1234);
        assert_eq!(ev["model_calls"], 2);
        assert_eq!(ev["actions"]["failed"], 1);
        // The coarse kind alone left a 35-minute `budget_exhausted` turn
        // unexplainable from its own record (2026-08-13); the detail is what
        // names which of the four ceilings bound.
        assert_eq!(ev["detail"], "the model returned an error after 3 attempts");
    }

    /// `/rename` seals like a turn: with a key, the title line is under a
    /// signed head. Review of 2026-10-27: it stayed after the last seal.
    #[tokio::test]
    async fn a_rename_is_sealed_when_a_key_exists() {
        let keys = tempfile::tempdir().unwrap();
        let signer = x0ne_audit::Signer::generate(keys.path()).unwrap();
        let public = signer.public_hex().to_owned();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let (chain, header) = x0ne_audit::Chain::open(b"", "r", Some(signer), "");
        std::fs::write(&path, header).unwrap();
        let file = OpenOptions::new().append(true).open(&path).await.unwrap();
        let mut logger = SessionLogger {
            file,
            id: id_of(&path),
            chain,
            path: path.clone(),
        };

        logger.log_session_renamed("parser rewrite").await.unwrap();

        let r = x0ne_audit::verify_file(&path, Some(public.as_str())).unwrap();
        assert_eq!(r.status, x0ne_audit::Status::Ok, "{r:?}");
        assert!(
            !r.notes
                .iter()
                .any(|n| n.starts_with("UNSEALED") || n.contains("after the last signed head")),
            "{:?}",
            r.notes
        );
    }
}
