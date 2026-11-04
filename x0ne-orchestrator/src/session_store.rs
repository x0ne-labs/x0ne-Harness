//! Reading side of `~/.x0ne/sessions` — listing and resuming.
//!
//! [`crate::session`] writes the journal; this module reads it back. They are
//! separate on purpose: the writer trusts its own input, and the reader
//! trusts nothing.
//!
//! SECURITY: a session journal is UNTRUSTED INPUT. Most of its bytes were
//! authored by the model (`llm_response`, every `action` payload), the rest by
//! whoever can write to the operator's home directory, and it has been on disk
//! since. Three consequences run through everything below:
//!
//! * the parser is line-by-line and tolerant — a malformed line is counted and
//!   skipped, never fatal, because one truncated write (a full disk, a kill
//!   mid-flush) must not cost the operator the other 400 lines;
//! * what is *displayed* goes through [`crate::text_safety`], the one
//!   sanitizer, before it is stored in a summary — the listing prints a first
//!   prompt the model may have shaped;
//! * an id supplied on the command line is *validated*, never repaired: see
//!   [`validate_session_id`].
//!
//! What a resume restores is a **transcript**. It does not restore approvals,
//! trust, handles, snapshots or spend — see
//! [`crate::agent::session::CliAgentSession::resume_conversation`].

use crate::conversation::{Message, Role};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ── Bounds ────────────────────────────────────────────────────────────────────

/// Most bytes of one journal this module will read back into memory.
///
/// 4 MiB is roughly 1 M tokens of transcript — already more than any context
/// window this product targets, so a larger window would only buy material
/// that `Conversation::trim_if_needed` drops on the first turn. Journals go
/// well past it in practice: `action_result` lines carry up to
/// `MAX_JOURNALED_RESULT_BYTES` (64 KiB) each, so a hundred `ReadFiles` turns
/// is a multi-megabyte file whose *conversation* is a few kilobytes.
///
/// Past the bound the **tail** is kept, and [`ResumedSession::bytes_skipped`]
/// says how much was not read. Keeping the tail rather than the head is the
/// only useful choice: a resume continues the most recent work.
pub const MAX_RESUME_BYTES: u64 = 4 * 1024 * 1024;

/// Most bytes of any single reloaded message.
///
/// Same ceiling as the journal's own per-string bound
/// (`session::MAX_JOURNALED_STRING_BYTES`), for the same reason and against a
/// different producer: `user_input` and `llm_response` are written to the
/// journal *unbounded*, so a 10 MB paste is on disk verbatim and would come
/// back as one message. Truncation is marked in the text, because a silently
/// shortened message is a transcript that lies to the model about what was
/// said.
pub const MAX_RESUME_MESSAGE_BYTES: usize = 24_000;

/// Characters of the first prompt shown in `x0ne sessions`.
pub const FIRST_PROMPT_CHARS: usize = 80;

/// Characters of a `/rename` title shown in `x0ne sessions`.
pub const TITLE_CHARS: usize = 80;

/// Characters of a project path shown in `x0ne sessions`.
pub const PROJECT_CHARS: usize = 60;

/// Longest id this module will accept.
///
/// A minted id is 21 characters (`2026-08-16_09-41_ab12`); legacy names run to
/// 44 (`<timestamp>_<24-char serve label>`). 64 leaves room without letting an
/// argument become a screenful.
pub const MAX_SESSION_ID_CHARS: usize = 64;

// ── Identity ──────────────────────────────────────────────────────────────────

/// Why an id was refused. Every variant is a refusal, never a repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRejection {
    Empty,
    TooLong { chars: usize },
    IllegalChar(char),
}

impl std::fmt::Display for IdRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdRejection::Empty => write!(f, "a session id cannot be empty"),
            IdRejection::TooLong { chars } => write!(
                f,
                "a session id is at most {MAX_SESSION_ID_CHARS} characters ({chars} given)"
            ),
            IdRejection::IllegalChar(c) => write!(
                f,
                "a session id holds only letters, digits, '-' and '_' (found {c:?})"
            ),
        }
    }
}

/// Mint the id of a fresh session: minute-resolution timestamp, then four
/// random hex digits.
///
/// The random suffix is not decoration. Until 2026-08-16 a session's file name
/// was the clock alone (`%Y-%m-%d_%H-%M-%S.jsonl`), so two sessions started in
/// the same second shared one file and interleaved their lines. Minute
/// resolution plus 16 bits keeps the id short enough to type after `--resume`
/// while making a collision an accident nobody will meet.
///
/// The alphabet is deliberately the one [`validate_session_id`] accepts, so a
/// minted id always round-trips through a file name and back.
pub fn new_session_id() -> String {
    use chrono::Utc;
    format!(
        "{}_{:04x}",
        Utc::now().format("%Y-%m-%d_%H-%M"),
        rand::random::<u16>()
    )
}

/// Accept an id, or say why not.
///
/// SECURITY: this is the boundary between an operator's argument and a path.
/// It **rejects** where `session::safe_label` **filters**, and the difference
/// is the point: filtering is right for a label the server mints (any survivor
/// is a fine file name), and wrong for a lookup, where `../../etc/passwd`
/// would silently become `etcpasswd` and open a session the operator never
/// named. A resume must open the file that was asked for or none at all.
///
/// The accepted alphabet (`[A-Za-z0-9_-]`) contains no `/`, no `.` and no NUL,
/// so a validated id cannot traverse, cannot escape `~/.x0ne/sessions`, and
/// cannot name a parent. It is joined to the directory and suffixed `.jsonl`
/// by [`journal_path`] and nowhere else.
pub fn validate_session_id(raw: &str) -> Result<&str, IdRejection> {
    if raw.is_empty() {
        return Err(IdRejection::Empty);
    }
    let chars = raw.chars().count();
    if chars > MAX_SESSION_ID_CHARS {
        return Err(IdRejection::TooLong { chars });
    }
    if let Some(c) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(IdRejection::IllegalChar(c));
    }
    Ok(raw)
}

/// The file a validated id names inside `dir`.
///
/// The single place an id becomes a path. `id` must have come from
/// [`validate_session_id`] or from [`id_from_path`] (which validates too).
pub fn journal_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// The id of an existing journal, or `None` if its name is not one.
///
/// Legacy files keep working: `2026-08-14_10-30-05.jsonl` has been a valid id
/// all along, it just was not called one. A file whose stem is not a valid id
/// is skipped rather than listed, because `--resume` could not address it
/// either — listing it would offer the operator something that cannot be
/// opened.
pub fn id_from_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    validate_session_id(stem).ok().map(str::to_string)
}

/// `~/.x0ne/sessions`. Same directory [`crate::session`] writes to.
pub fn sessions_dir() -> PathBuf {
    crate::session::sessions_dir()
}

// ── What a listing shows ──────────────────────────────────────────────────────

/// One row of `x0ne sessions`.
///
/// Every string here is already sanitized ([`crate::text_safety::snippet`]).
/// The property is enforced at construction, not at print time, so a second
/// frontend cannot forget it — see `sessions_list_sanitizes_what_it_prints`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    /// The header's `git_root`, else its `cwd`: what [`list_for`] compares
    /// with the current project. Journal text, never printed as is.
    pub project_key: Option<String>,
    /// [`SessionSummary::project_key`], sanitized and cut for display.
    pub project: Option<String>,
    /// The latest `/rename` title. Sanitized.
    pub title: Option<String>,
    /// `ts` of the `session_start` line, if the file has one. Sanitized.
    pub started: Option<String>,
    /// Model named by `session_start`. Sanitized.
    pub model: Option<String>,
    /// `user_input` events seen while scanning. A lower bound when
    /// [`SessionSummary::complete`] is false.
    pub turns: usize,
    /// First user prompt, sanitized and cut to [`FIRST_PROMPT_CHARS`].
    pub first_prompt: Option<String>,
    pub bytes: u64,
    pub modified: SystemTime,
    /// Whether the whole file was scanned. False once it exceeds
    /// [`MAX_RESUME_BYTES`], in which case `turns` undercounts.
    pub complete: bool,
}

/// Where a session runs: its canonical directory, and the root of the git
/// work tree holding it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub cwd: PathBuf,
    pub git_root: Option<PathBuf>,
}

impl Project {
    /// `dir` canonicalized, and its nearest ancestor holding a `.git`
    /// directory or file (a linked worktree's). File system only: no `git`
    /// process runs and no repository config is read.
    pub fn of(dir: &Path) -> Self {
        let cwd = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let git_root = cwd
            .ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf);
        Self { cwd, git_root }
    }

    /// The process's project, `None` when its directory cannot be read.
    pub fn current() -> Option<Self> {
        std::env::current_dir().ok().map(|d| Self::of(&d))
    }

    /// What sessions are grouped by: the git root, else the directory.
    pub fn key(&self) -> String {
        self.git_root
            .as_deref()
            .unwrap_or(&self.cwd)
            .to_string_lossy()
            .into_owned()
    }
}

/// A session listing: every journal, or one project's.
#[derive(Debug, Clone)]
pub struct Listed {
    pub rows: Vec<SessionSummary>,
    /// The project shown, sanitized; `None` when every journal is.
    pub project: Option<String>,
    /// Journals left out: other projects', and those written before the
    /// header recorded a directory.
    pub hidden: usize,
}

/// Sessions of `project` in `dir`, or all of them when `project` is `None`.
pub fn list_for(dir: &Path, project: Option<&Project>) -> Listed {
    let all = list_sessions(dir);
    let Some(project) = project else {
        return Listed {
            rows: all,
            project: None,
            hidden: 0,
        };
    };
    let key = project.key();
    let (rows, others): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|s| s.project_key.as_deref() == Some(key.as_str()));
    Listed {
        rows,
        project: Some(crate::text_safety::snippet(&key, PROJECT_CHARS)),
        hidden: others.len(),
    }
}

/// Every readable session in `dir`, most recently written first.
///
/// Ordered by modification time, not by id. A resumed session appends to its
/// original file, so its id keeps naming the day it was *created* while its
/// mtime tracks the day it was *used* — and "the last session" means the one
/// last worked in. Ties break on id, descending, so the order is total and the
/// listing does not shuffle between runs.
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let mut out: Vec<SessionSummary> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| summarize(&e.path()).ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Read one journal's header row without reloading its conversation.
pub fn summarize(path: &Path) -> Result<SessionSummary, ResumeError> {
    let id = id_from_path(path).ok_or(ResumeError::NotAJournal)?;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ResumeError::NotAJournal);
    }
    let bytes = meta.len();
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let complete = bytes <= MAX_RESUME_BYTES;
    let (text, _skipped) = read_tail(path, MAX_RESUME_BYTES)?;
    // The header lives at the FRONT, which the tail window may have cut off.
    let head = read_head(path, 64 * 1024)?;

    let mut started = None;
    let mut model = None;
    let mut project_key = None;
    for line in head.lines() {
        if let Some(ev) = parse_event(line) {
            if ev.kind == "session_start" {
                started = ev.string("ts").map(|s| crate::text_safety::snippet(s, 40));
                model = ev
                    .string("model")
                    .map(|s| crate::text_safety::snippet(s, 40));
                project_key = ev
                    .string("git_root")
                    .or_else(|| ev.string("cwd"))
                    .map(str::to_string);
                break;
            }
        }
    }

    let mut turns = 0usize;
    let mut first_prompt = None;
    let mut title = None;
    // Head then tail: the tail ends the file, so the last rename seen is
    // the latest one written.
    for line in head.lines().chain(text.lines()) {
        if let Some(ev) = parse_event(line) {
            if ev.kind == "user_input" && first_prompt.is_none() {
                first_prompt = ev
                    .string("content")
                    .map(|s| crate::text_safety::snippet(s, FIRST_PROMPT_CHARS));
            }
            if ev.kind == "session_renamed" {
                if let Some(t) = ev.string("title") {
                    title = Some(crate::text_safety::snippet(t, TITLE_CHARS));
                }
            }
        }
    }
    // `turns` counts only the window actually loaded, so it agrees with what a
    // resume would restore rather than with what the file contains.
    for line in text.lines() {
        if let Some(ev) = parse_event(line) {
            if ev.kind == "user_input" && ev.string("content").is_some() {
                turns += 1;
            }
        }
    }

    Ok(SessionSummary {
        id,
        path: path.to_path_buf(),
        project: project_key
            .as_deref()
            .map(|k| crate::text_safety::snippet(k, PROJECT_CHARS)),
        project_key,
        title,
        started,
        model,
        turns,
        first_prompt,
        bytes,
        modified,
        complete,
    })
}

/// The id `--resume last` means: the most recently written session that holds
/// at least one turn, is not `exclude`, and, when `project` is given, was
/// started in that project.
///
/// `exclude` is the session currently running. Without it, `/resume last`
/// inside a live REPL reloads the file that REPL is writing to — the newest by
/// definition — which is a no-op dressed as a feature.
///
/// "Non-empty" is `turns >= 1`, not `bytes > 0`: a journal holding only
/// `session_start` restores nothing, so offering it would answer the operator
/// with an empty transcript.
pub fn resolve_last(
    dir: &Path,
    exclude: Option<&str>,
    project: Option<&Project>,
) -> Option<String> {
    let key = project.map(Project::key);
    list_sessions(dir)
        .into_iter()
        .filter(|s| key.is_none() || s.project_key == key)
        .find(|s| s.turns >= 1 && Some(s.id.as_str()) != exclude)
        .map(|s| s.id)
}

/// The argument of `/resume`: one id or `last`, and `--all`.
pub fn parse_resume_arg(arg: &str) -> Result<(&str, bool), String> {
    let mut all = false;
    let mut target = None;
    for word in arg.split_whitespace() {
        if word == "--all" {
            all = true;
        } else if target.replace(word).is_some() {
            return Err("one session id, or `last`, and `--all`".into());
        }
    }
    target
        .map(|t| (t, all))
        .ok_or_else(|| "a session id, or `last`".into())
}

/// A session to resume, and the project it was started in when that is not
/// the current one: sanitized, for a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeTarget {
    pub id: String,
    pub elsewhere: Option<String>,
}

/// The session `target` names. `last` is this project's latest (`here`),
/// or any project's with `all`; an id is validated, then taken wherever it
/// was started. Review of 2026-10-27: `last` crossed projects silently.
pub fn resolve_resume(
    dir: &Path,
    target: &str,
    all: bool,
    exclude: Option<&str>,
    here: Option<&Project>,
) -> Result<ResumeTarget, String> {
    let id = if target.eq_ignore_ascii_case("last") {
        let scope = if all {
            None
        } else {
            Some(here.ok_or("the current directory cannot be read: add --all")?)
        };
        resolve_last(dir, exclude, scope).ok_or_else(|| {
            if all {
                "no session to resume".to_string()
            } else {
                "no session to resume in this project; `last --all` looks in every project"
                    .to_string()
            }
        })?
    } else {
        // SECURITY: the id becomes a path. Validated, never repaired.
        validate_session_id(target)
            .map_err(|e| e.to_string())?
            .to_string()
    };
    let here_key = here.map(Project::key);
    let elsewhere = summarize(&journal_path(dir, &id))
        .ok()
        .and_then(|s| s.project_key)
        .filter(|k| here_key.as_ref() != Some(k))
        .map(|k| crate::text_safety::snippet(&k, PROJECT_CHARS));
    Ok(ResumeTarget { id, elsewhere })
}

// ── What a resume restores ────────────────────────────────────────────────────

/// Token consumption recorded by the session being resumed.
///
/// Tokens, not dollars: the journal records tokens, and prices change between
/// two runs. Re-pricing an old session at today's rates would print a number
/// nobody was ever billed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriorTokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl PriorTokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// A transcript reloaded from a journal, plus what it cost to read it.
#[derive(Debug, Clone)]
pub struct ResumedSession {
    pub id: String,
    pub path: PathBuf,
    /// The conversation, in file order. Nothing else — see the module docs.
    pub messages: Vec<Message>,
    /// `turn` events replayed. What the operator is told.
    pub turns_reloaded: usize,
    /// Lines that were not usable JSON events. Counted, never fatal.
    pub broken_lines: usize,
    /// Events whose `type` this build does not know. Not an error: a journal
    /// written by a newer X0NE must read as "unfamiliar", not as "corrupt".
    pub unknown_events: usize,
    /// Bytes at the front of the file that the [`MAX_RESUME_BYTES`] window did
    /// not reach. Non-zero means older turns are missing from `messages`.
    pub bytes_skipped: u64,
    /// Model named by the journal's `session_start`, if it was still in range.
    pub model: Option<String>,
    /// What the journalled session consumed. Reported, never charged — see
    /// [`crate::agent::session::CliAgentSession::resume_conversation`].
    pub prior_tokens: PriorTokens,
}

impl ResumedSession {
    /// Whether the window cut older turns off the front.
    pub fn truncated_from_front(&self) -> bool {
        self.bytes_skipped > 0
    }
}

#[derive(Debug)]
pub enum ResumeError {
    Id(IdRejection),
    NotFound(String),
    /// The journal exists but holds no user turn to restore.
    NoTurns(String),
    NotAJournal,
    Io(std::io::Error),
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeError::Id(r) => write!(f, "{r}"),
            ResumeError::NotFound(id) => write!(f, "no session named {id}"),
            ResumeError::NoTurns(id) => write!(f, "session {id} has no turn to resume"),
            ResumeError::NotAJournal => write!(f, "not a session journal"),
            ResumeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResumeError {}

impl From<std::io::Error> for ResumeError {
    fn from(e: std::io::Error) -> Self {
        ResumeError::Io(e)
    }
}

/// Reload the conversation of session `id` from `dir`.
///
/// Restores `user_input` and `llm_response` in file order, plus one bounded
/// note per turn saying how many actions ran and how many were refused.
///
/// **Action results are deliberately not replayed as messages.** Three
/// reasons, and the first is the one that matters:
///
/// 1. A result is an *observation of a world that has moved on*. The files it
///    described have changed, the handles it returned are dead, the command it
///    captured ran in a process that exited. Re-injecting it presents stale
///    observation as fresh observation — the exact failure
///    `SystemFeedbackClass::Hallucination` exists to punish.
/// 2. The journal's own copy is already lossy: `action_result` lines are
///    bounded and sometimes replaced by a summary
///    (`session::bounded_result`), so a replay would be a transcript that
///    quietly differs from what the model saw the first time.
/// 3. Results dominate a journal by volume. Spending the
///    [`MAX_RESUME_BYTES`] window on captured stdout buys almost no
///    conversation.
///
/// The note keeps the *shape* of the turn — that work happened, that some of
/// it was refused — and tells the model to look again before relying on it.
pub fn load_session(dir: &Path, id: &str) -> Result<ResumedSession, ResumeError> {
    let id = validate_session_id(id).map_err(ResumeError::Id)?;
    let path = journal_path(dir, id);
    if !path.is_file() {
        return Err(ResumeError::NotFound(id.to_string()));
    }
    let resumed = parse_journal(&path, id)?;
    if resumed.messages.is_empty() {
        return Err(ResumeError::NoTurns(id.to_string()));
    }
    Ok(resumed)
}

/// [`load_session`] against an explicit file. Public for the frontends that
/// already hold the path.
pub fn parse_journal(path: &Path, id: &str) -> Result<ResumedSession, ResumeError> {
    let (text, bytes_skipped) = read_tail(path, MAX_RESUME_BYTES)?;
    let head = if bytes_skipped > 0 {
        read_head(path, 64 * 1024)?
    } else {
        String::new()
    };

    let mut messages: Vec<Message> = Vec::new();
    let mut turns_reloaded = 0usize;
    let mut broken_lines = 0usize;
    let mut unknown_events = 0usize;
    let mut prior_tokens = PriorTokens::default();
    let mut model = None;
    let mut actions_in_turn = 0usize;
    let mut denials_in_turn = 0usize;

    // The header may live outside the window. Read it separately rather than
    // reporting "unknown model" for every large session.
    for line in head.lines() {
        if let Some(ev) = parse_event(line) {
            if ev.kind == "session_start" {
                model = ev.string("model").map(str::to_string);
                break;
            }
        }
    }

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(ev) = parse_event(line) else {
            broken_lines += 1;
            continue;
        };
        match ev.kind.as_str() {
            "session_start" => {
                if model.is_none() {
                    model = ev.string("model").map(str::to_string);
                }
            }
            "user_input" => match ev.string("content") {
                Some(c) => messages.push(Message {
                    role: Role::User,
                    content: bound_message(c),
                    images: vec![],
                }),
                None => broken_lines += 1,
            },
            "llm_response" => {
                prior_tokens.input += ev.u64("tokens_in");
                prior_tokens.output += ev.u64("tokens_out");
                prior_tokens.cache_write += ev.u64("cache_write_tokens");
                prior_tokens.cache_read += ev.u64("cache_read_tokens");
                match ev.string("content") {
                    Some(c) => messages.push(Message {
                        role: Role::Assistant,
                        content: bound_message(c),
                        images: vec![],
                    }),
                    None => broken_lines += 1,
                }
            }
            "action" => actions_in_turn += 1,
            "policy_denial" => denials_in_turn += 1,
            // `action_result` is read for nothing on purpose: see the note on
            // `load_session`. Counting it would double every action.
            "action_result" => {}
            "turn" => {
                turns_reloaded += 1;
                if let Some(note) = turn_note(actions_in_turn, denials_in_turn) {
                    messages.push(Message {
                        role: Role::User,
                        content: note,
                        images: vec![],
                    });
                }
                actions_in_turn = 0;
                denials_in_turn = 0;
            }
            "session_resumed" => {}
            _ => unknown_events += 1,
        }
    }

    // A journal cut short mid-turn (the process was killed) has actions with
    // no closing `turn`. Their tally still belongs to the transcript.
    if let Some(note) = turn_note(actions_in_turn, denials_in_turn) {
        messages.push(Message {
            role: Role::User,
            content: note,
            images: vec![],
        });
    }

    Ok(ResumedSession {
        id: id.to_string(),
        path: path.to_path_buf(),
        messages,
        turns_reloaded,
        broken_lines,
        unknown_events,
        bytes_skipped,
        model,
        prior_tokens,
    })
}

/// The per-turn note, or `None` when the turn ran no action.
///
/// English and descriptive: the model reads it. It says what happened and what
/// the model must not assume, and it carries no payload — two integers cannot
/// be a prompt-injection surface, which is why this and not the results.
fn turn_note(actions: usize, denials: usize) -> Option<String> {
    if actions == 0 && denials == 0 {
        return None;
    }
    Some(format!(
        "[System Context]\nResumed transcript: this turn ran {actions} action(s) and had \
         {denials} refused. Their results are not part of this transcript — observe the \
         system again before relying on anything they returned."
    ))
}

/// Bound one reloaded message, marking the cut.
fn bound_message(s: &str) -> String {
    if s.len() <= MAX_RESUME_MESSAGE_BYTES {
        return s.to_string();
    }
    let cut = crate::session::floor_char_boundary(s, MAX_RESUME_MESSAGE_BYTES);
    format!(
        "{}… [resume: {} bytes truncated to {}]",
        &s[..cut],
        s.len(),
        cut
    )
}

// ── Tolerant line reading ─────────────────────────────────────────────────────

/// Parse one line, or `None` if it is not a usable event.
///
/// Not usable: invalid JSON, a JSON value that is not an object, an object
/// with no `type`, or a `type` that is not a string. Every one of those is a
/// line the writer could not have produced, so the file has been damaged or
/// hand-edited — the caller counts it and moves on.
fn parse_event(line: &str) -> Option<ParsedEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = match value {
        serde_json::Value::Object(m) => m,
        _ => return None,
    };
    let kind = obj.get("type")?.as_str()?.to_string();
    Some(ParsedEvent { kind, obj })
}

struct ParsedEvent {
    kind: String,
    obj: serde_json::Map<String, serde_json::Value>,
}

impl ParsedEvent {
    fn string(&self, key: &str) -> Option<&str> {
        self.obj.get(key).and_then(|v| v.as_str())
    }
    fn u64(&self, key: &str) -> u64 {
        self.obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    }
}

/// Last `max` bytes of `path`, as lossy UTF-8, plus how many bytes were
/// skipped.
///
/// When the window bites, the first (necessarily partial) line is dropped:
/// half a JSON object is not an event, and feeding it to the parser would
/// inflate `broken_lines` with an artefact of our own windowing.
///
/// Lossy rather than strict: a journal truncated mid-codepoint by a crash must
/// still be readable. `from_utf8_lossy` replaces the broken bytes; a strict
/// decode would lose the whole file to one bad byte.
fn read_tail(path: &Path, max: u64) -> Result<(String, u64), ResumeError> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= max {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf)?;
        return Ok((String::from_utf8_lossy(&buf).into_owned(), 0));
    }
    let skipped = len - max;
    file.seek(SeekFrom::Start(skipped))?;
    let mut buf = Vec::with_capacity(max as usize);
    file.take(max).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let cut = match text.find('\n') {
        Some(i) => i + 1,
        None => text.len(),
    };
    Ok((text[cut..].to_string(), skipped + cut as u64))
}

/// First `max` bytes of `path`, as lossy UTF-8. For the header alone.
fn read_head(path: &Path, max: u64) -> Result<String, ResumeError> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn journal(lines: &[serde_json::Value]) -> String {
        lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    /// A project is its git work tree, found by its `.git` directory or a
    /// linked worktree's `.git` file; outside one, the directory itself.
    #[test]
    fn a_project_is_its_git_root_else_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for d in ["repo/.git", "repo/src/deep", "wt/sub", "plain"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("wt/.git"), "gitdir: /elsewhere\n").unwrap();

        let deep = Project::of(&root.join("repo/src/deep"));
        assert_eq!(deep.cwd, root.join("repo/src/deep"));
        assert_eq!(deep.git_root, Some(root.join("repo")));
        assert_eq!(deep.key(), root.join("repo").to_string_lossy());
        assert_eq!(
            Project::of(&root.join("wt/sub")).git_root,
            Some(root.join("wt"))
        );
        let plain = Project::of(&root.join("plain"));
        assert_eq!(plain.git_root, None);
        assert_eq!(plain.key(), root.join("plain").to_string_lossy());
    }

    /// `x0ne sessions` shows the current project's sessions; the others,
    /// and journals from before the header named a directory, are counted.
    #[test]
    fn the_listing_keeps_the_current_project_and_counts_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let start = |cwd: &str, git: Option<&str>| serde_json::json!({"type":"session_start","model":"m","cwd":cwd,"git_root":git});
        let prompt = serde_json::json!({"type":"user_input","content":"hi"});
        write(
            tmp.path(),
            "mine.jsonl",
            &journal(&[start("/src/a/sub", Some("/src/a")), prompt.clone()]),
        );
        write(
            tmp.path(),
            "plain.jsonl",
            &journal(&[start("/notes", None), prompt.clone()]),
        );
        write(
            tmp.path(),
            "other.jsonl",
            &journal(&[start("/src/b", Some("/src/b")), prompt.clone()]),
        );
        write(
            tmp.path(),
            "legacy.jsonl",
            &journal(&[
                serde_json::json!({"type":"session_start","model":"m"}),
                prompt,
            ]),
        );

        let here = Project {
            cwd: "/src/a".into(),
            git_root: Some("/src/a".into()),
        };
        let listed = list_for(tmp.path(), Some(&here));
        let ids: Vec<&str> = listed.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["mine"]);
        assert_eq!(listed.hidden, 3);
        assert_eq!(listed.project.as_deref(), Some("/src/a"));

        let notes = Project {
            cwd: "/notes".into(),
            git_root: None,
        };
        let ids: Vec<String> = list_for(tmp.path(), Some(&notes))
            .rows
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["plain"]);

        let all = list_for(tmp.path(), None);
        assert_eq!((all.rows.len(), all.hidden, all.project), (4, 0, None));
    }

    /// The title is the last `/rename`, read as untrusted text like the rest
    /// of the journal: it cannot move the cursor of the terminal it lands on.
    #[test]
    fn the_title_is_the_last_rename_and_cannot_redraw_the_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "named.jsonl",
            &journal(&[
                serde_json::json!({"type":"session_start","model":"m"}),
                serde_json::json!({"type":"session_renamed","title":"first"}),
                serde_json::json!({"type":"user_input","content":"hi"}),
                serde_json::json!({"type":"session_renamed","title":"\u{1b}[2Jsecond\r"}),
            ]),
        );
        let row = summarize(&tmp.path().join("named.jsonl")).unwrap();
        let title = row.title.unwrap();
        assert!(
            !title.contains('\u{1b}') && !title.contains('\r'),
            "{title:?}"
        );
        assert!(title.contains("second"), "{title:?}");
        assert_eq!(row.first_prompt.as_deref(), Some("hi"));
    }

    /// A minted id is stable, short, and can only ever be a file name.
    #[test]
    fn a_new_session_announces_a_stable_id() {
        let id = new_session_id();
        assert_eq!(validate_session_id(&id), Ok(id.as_str()));
        assert!(id.len() <= 24, "an id must be typeable: {id}");
        assert!(
            id.starts_with(&chrono::Utc::now().format("%Y-%m-%d").to_string()),
            "an id carries its date: {id}"
        );
        // Round-trips through the file name it names.
        let p = journal_path(Path::new("/tmp/x"), &id);
        assert_eq!(id_from_path(&p).as_deref(), Some(id.as_str()));
        // Two ids minted in the same minute differ.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            seen.insert(new_session_id());
        }
        assert!(seen.len() > 1, "the random suffix is not random");
    }

    /// SECURITY: an id names a file inside `~/.x0ne/sessions` and nothing
    /// else. A failure here means `--resume` is an arbitrary-file-read.
    #[test]
    fn a_session_id_is_never_a_path() {
        for hostile in [
            "../../etc/passwd",
            "/abs",
            "a b",
            "",
            "..",
            ".",
            "sess/../../x",
            "a\0b",
            "a\nb",
            "évil",
            "x;rm -rf /",
        ] {
            assert!(
                validate_session_id(hostile).is_err(),
                "{hostile:?} was accepted as a session id"
            );
        }
        assert!(validate_session_id(&"x".repeat(300)).is_err());
        assert!(validate_session_id(&"x".repeat(MAX_SESSION_ID_CHARS)).is_ok());
        // The legitimate shapes still pass: minted, legacy, serve-labelled.
        for ok in [
            "2026-08-16_09-41_ab12",
            "2026-08-14_10-30-05",
            "2026-08-14_10-30-05_websess42",
        ] {
            assert_eq!(validate_session_id(ok), Ok(ok));
        }
        // And nothing that passes can produce a path outside the directory.
        let dir = Path::new("/home/u/.x0ne/sessions");
        for ok in ["a", "2026-08-16_09-41_ab12", "-_-"] {
            let p = journal_path(dir, ok);
            assert_eq!(p.parent(), Some(dir), "{ok} escaped its directory");
        }
    }

    /// The conversation comes back as it was said: user, then assistant, in
    /// file order, with nothing invented between them.
    #[test]
    fn resume_reloads_the_conversation_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "2026-08-16_09-41_ab12";
        write(
            tmp.path(),
            &format!("{id}.jsonl"),
            &journal(&[
                serde_json::json!({"ts":"t0","type":"session_start","model":"claude-x"}),
                serde_json::json!({"ts":"t1","type":"user_input","content":"first"}),
                serde_json::json!({"ts":"t2","type":"llm_response","content":"one",
                                   "tokens_in":10,"tokens_out":5}),
                serde_json::json!({"ts":"t3","type":"turn","outcome":"ok"}),
                serde_json::json!({"ts":"t4","type":"user_input","content":"second"}),
                serde_json::json!({"ts":"t5","type":"llm_response","content":"two",
                                   "tokens_in":20,"tokens_out":7}),
                serde_json::json!({"ts":"t6","type":"turn","outcome":"ok"}),
            ]),
        );

        let r = load_session(tmp.path(), id).unwrap();
        let said: Vec<(&Role, &str)> = r
            .messages
            .iter()
            .map(|m| (&m.role, m.content.as_str()))
            .collect();
        assert_eq!(
            said,
            vec![
                (&Role::User, "first"),
                (&Role::Assistant, "one"),
                (&Role::User, "second"),
                (&Role::Assistant, "two"),
            ]
        );
        assert_eq!(r.turns_reloaded, 2);
        assert_eq!(r.broken_lines, 0);
        assert_eq!(r.model.as_deref(), Some("claude-x"));
        assert_eq!(r.prior_tokens.input, 30);
        assert_eq!(r.prior_tokens.output, 12);
    }

    /// Action results are not messages; what survives a turn that acted is a
    /// bounded note carrying two integers and no payload.
    #[test]
    fn a_turn_that_acted_is_summarized_not_replayed() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "sess1";
        let secret = "SECRET-STDOUT-THAT-MUST-NOT-COME-BACK";
        write(
            tmp.path(),
            &format!("{id}.jsonl"),
            &journal(&[
                serde_json::json!({"type":"user_input","content":"go"}),
                serde_json::json!({"type":"llm_response","content":"working"}),
                serde_json::json!({"type":"action","action_type":"RunCommand",
                                   "action":{"type":"RunCommand","command":"ls"}}),
                serde_json::json!({"type":"action_result","action_type":"RunCommand",
                                   "result":{"success":true,"output":{"stdout":secret}}}),
                serde_json::json!({"type":"action","action_type":"ReadFile"}),
                serde_json::json!({"type":"policy_denial","action_type":"WriteFile",
                                   "denial":"human_declined","reason":"no"}),
                serde_json::json!({"type":"turn","outcome":"ok"}),
            ]),
        );

        let r = load_session(tmp.path(), id).unwrap();
        let all = r
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains(secret),
            "an action result was replayed into the transcript"
        );
        let note = r.messages.last().unwrap();
        assert_eq!(note.role, Role::User);
        assert!(
            note.content.contains("2 action(s)"),
            "note: {}",
            note.content
        );
        assert!(note.content.contains("1 refused"), "note: {}", note.content);
        assert!(note.content.len() < 400, "the note must stay bounded");
    }

    /// SECURITY: the journal is untrusted input. Every one of these lines is
    /// something a crash, a hand edit or a hostile model can put in the file.
    /// A failure here is `x0ne --resume` panicking on the operator's own log.
    #[test]
    fn a_broken_line_is_skipped_and_counted_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "fuzz";
        let mut body = String::new();
        body.push_str(&journal(&[
            serde_json::json!({"type":"user_input","content":"good one"}),
        ]));
        // 1. truncated line (the classic: killed mid-flush)
        body.push_str("{\"type\":\"user_input\",\"conte\n");
        // 2. plain garbage
        body.push_str("not json at all\n");
        // 3. valid JSON, wrong shape
        body.push_str("[1,2,3]\n");
        body.push_str("\"a string\"\n");
        body.push_str("null\n");
        // 4. object with no type / non-string type
        body.push_str("{\"ts\":\"t\"}\n");
        body.push_str("{\"type\":42}\n");
        // 5. a type from a build that does not exist yet
        body.push_str("{\"type\":\"quantum_event\",\"payload\":1}\n");
        // 6. known type, missing the field it promises
        body.push_str("{\"type\":\"user_input\"}\n");
        body.push_str("{\"type\":\"llm_response\",\"tokens_in\":3}\n");
        // 7. blank lines and whitespace
        body.push_str("\n   \n");
        // 8. a message far past the per-message bound but inside the window
        //    (a 10 MB one gets its own file below: it exceeds the WINDOW too,
        //    and mixing the two properties hid the broken-line count at
        //    zero the first time this test was written).
        body.push_str(&format!(
            "{}\n",
            serde_json::to_string(
                &serde_json::json!({"type":"user_input","content":"A".repeat(1024*1024)})
            )
            .unwrap()
        ));
        // 9. control characters and a lone surrogate escape survivor
        body.push_str(
            &serde_json::to_string(
                &serde_json::json!({"type":"llm_response","content":"a\u{1b}[2Kb"}),
            )
            .unwrap(),
        );
        body.push('\n');
        write(tmp.path(), &format!("{id}.jsonl"), &body);

        let r = load_session(tmp.path(), id).expect("a damaged journal still loads");
        // Nine: the truncated line, the garbage, three non-objects, the
        // object with no `type`, the non-string `type`, and the two known
        // events that promised a `content` they do not carry.
        assert_eq!(
            r.broken_lines, 9,
            "expected 9 unusable lines, got {}",
            r.broken_lines
        );
        assert_eq!(r.unknown_events, 1, "an unknown type is not corruption");
        // The good lines survived.
        assert_eq!(r.messages[0].content, "good one");
        // The 10 MB paste came back bounded and said so.
        let huge = r
            .messages
            .iter()
            .find(|m| m.content.starts_with("AAAA"))
            .expect("the big message");
        assert!(
            huge.content.len() < MAX_RESUME_MESSAGE_BYTES + 200,
            "a 10 MB message came back at {} bytes",
            huge.content.len()
        );
        assert!(huge.content.contains("truncated"));
        // The model's own history is NOT sanitized — it is what the model
        // said, and it goes to the model, not to a terminal.
        assert!(r.messages.iter().any(|m| m.content.contains('\u{1b}')));

        // A single 10 MB line is bigger than the whole window. It must not
        // panic, must not hang the caller, and must not come back whole.
        let mut monster = journal(&[serde_json::json!({"type":"user_input","content":"start"})]);
        monster.push_str(&format!(
            "{}\n",
            serde_json::to_string(
                &serde_json::json!({"type":"llm_response","content":"B".repeat(10*1024*1024)})
            )
            .unwrap()
        ));
        monster.push_str(&journal(&[
            serde_json::json!({"type":"user_input","content":"after the monster"}),
            serde_json::json!({"type":"turn","outcome":"ok"}),
        ]));
        write(tmp.path(), "monster.jsonl", &monster);
        let r = load_session(tmp.path(), "monster").expect("a 10 MB line still loads");
        assert!(r.truncated_from_front());
        assert!(r.messages.iter().all(|m| m.content.len() < 1_000_000));
        assert!(r.messages.iter().any(|m| m.content == "after the monster"));
    }

    /// A journal past the window comes back as its tail, and the caller is
    /// told bytes were left behind. Silence there would be a transcript with
    /// a hole the operator cannot see.
    #[test]
    fn an_oversized_journal_is_truncated_from_the_front_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "big";
        let mut body = journal(&[
            serde_json::json!({"type":"session_start","model":"claude-x"}),
            serde_json::json!({"type":"user_input","content":"THE-VERY-FIRST-PROMPT"}),
        ]);
        // Pad past the window with action results — the volume that makes a
        // real journal large.
        let filler = serde_json::to_string(&serde_json::json!({
            "type":"action_result","action_type":"RunCommand",
            "result":{"success":true,"output":{"stdout":"x".repeat(60_000)}}
        }))
        .unwrap();
        // SECURITY (2026-08-16): the padding is a FIXED byte count, never
        // derived from `MAX_RESUME_BYTES`. A mutation campaign set that
        // constant to `u64::MAX / 2` and this loop, then written as
        // `while body.len() < MAX_RESUME_BYTES + 100_000`, tried to build a
        // 9 EB string and took the machine down (OOM, twice). A fixture that
        // scales with the constant under test is a memory bomb; the assertion
        // that the constant is 4 MiB lives one line below, so a mutant is
        // still caught — by a comparison, not by an allocation.
        assert_eq!(
            MAX_RESUME_BYTES,
            4 * 1024 * 1024,
            "the fixture below is sized for a 4 MiB window"
        );
        const PAD_TARGET: usize = 4 * 1024 * 1024 + 100_000;
        while body.len() < PAD_TARGET {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str(&journal(&[
            serde_json::json!({"type":"user_input","content":"THE-LAST-PROMPT"}),
            serde_json::json!({"type":"llm_response","content":"the last answer"}),
            serde_json::json!({"type":"turn","outcome":"ok"}),
        ]));
        write(tmp.path(), &format!("{id}.jsonl"), &body);

        let r = load_session(tmp.path(), id).unwrap();
        assert!(r.truncated_from_front(), "the window did not bite");
        assert!(r.bytes_skipped > 0);
        let all = r
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(all.contains("THE-LAST-PROMPT"), "the tail was not kept");
        assert!(
            !all.contains("THE-VERY-FIRST-PROMPT"),
            "the front was read despite the bound"
        );
        // The header lives outside the window and is still recovered, so the
        // listing does not report "unknown model" for every long session.
        assert_eq!(r.model.as_deref(), Some("claude-x"));
        // Windowing must not manufacture corruption: the partial first line
        // is dropped, not parsed.
        assert_eq!(r.broken_lines, 0, "the window invented broken lines");
    }

    /// `last` means the session most recently worked in, skipping the one
    /// running now and any that hold nothing to restore.
    #[test]
    fn resume_last_picks_the_most_recent_nonempty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let turnful = journal(&[
            serde_json::json!({"type":"user_input","content":"hi"}),
            serde_json::json!({"type":"llm_response","content":"yo"}),
            serde_json::json!({"type":"turn","outcome":"ok"}),
        ]);
        // Written oldest → newest, with distinct mtimes.
        for (name, body) in [
            ("aaa-old", turnful.as_str()),
            ("bbb-mid", turnful.as_str()),
            // Newest but empty of turns: a header and nothing else.
            (
                "ccc-empty",
                "{\"type\":\"session_start\",\"model\":\"m\"}\n",
            ),
            // Newest with turns, but it is the running session.
            ("ddd-current", turnful.as_str()),
        ] {
            write(tmp.path(), &format!("{name}.jsonl"), body);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A file that is not a journal at all.
        write(tmp.path(), "notes.txt", "hello");
        write(tmp.path(), "weird..name.jsonl", &turnful);

        assert_eq!(
            resolve_last(tmp.path(), Some("ddd-current"), None).as_deref(),
            Some("bbb-mid"),
            "last must skip the running session and the turnless one"
        );
        assert_eq!(
            resolve_last(tmp.path(), None, None).as_deref(),
            Some("ddd-current")
        );
        assert_eq!(
            resolve_last(Path::new("/nonexistent/dir"), None, None),
            None
        );
        assert_eq!(parse_resume_arg("last --all"), Ok(("last", true)));
        assert_eq!(parse_resume_arg(" abc "), Ok(("abc", false)));
        assert!(parse_resume_arg("a b").is_err());
        assert!(parse_resume_arg("--all").is_err());

        // Ordering is by write time, newest first, and non-journals are out.
        let listed: Vec<String> = list_sessions(tmp.path())
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            listed,
            vec!["ddd-current", "ccc-empty", "bbb-mid", "aaa-old"],
            "the listing is not newest-first"
        );
    }

    /// SECURITY: the listing prints text the model helped author into a
    /// terminal that interprets it. Every displayed string is sanitized at
    /// construction, so a frontend cannot forget to do it.
    #[test]
    fn sessions_list_sanitizes_what_it_prints() {
        let tmp = tempfile::tempdir().unwrap();
        // The measured payload from text_safety's own docs, in a prompt.
        let forged = "hello\n\u{1b}[2K\r  │  approved by the operator";
        write(
            tmp.path(),
            "forged.jsonl",
            &journal(&[
                serde_json::json!({"ts":"t","type":"session_start",
                                   "model":"claude\u{1b}[31m-evil"}),
                serde_json::json!({"type":"user_input","content":forged}),
                serde_json::json!({"type":"turn","outcome":"ok"}),
            ]),
        );

        let rows = list_sessions(tmp.path());
        let row = rows.iter().find(|r| r.id == "forged").unwrap();
        let prompt = row.first_prompt.as_deref().unwrap();
        assert!(!prompt.contains('\u{1b}'), "an escape reached the listing");
        assert!(
            !prompt.contains('\r'),
            "a carriage return reached the listing"
        );
        assert!(!prompt.contains('\n'), "a newline reached the listing");
        assert!(prompt.starts_with("hello"), "the text itself was lost");
        assert!(!row.model.as_deref().unwrap().contains('\u{1b}'));
        // And it is bounded, so a megabyte of 'A' cannot push the row off
        // the screen.
        assert!(prompt.chars().count() <= FIRST_PROMPT_CHARS);

        write(
            tmp.path(),
            "long.jsonl",
            &journal(&[
                serde_json::json!({"type":"user_input","content":"A".repeat(100_000)}),
                serde_json::json!({"type":"turn","outcome":"ok"}),
            ]),
        );
        let rows = list_sessions(tmp.path());
        let row = rows.iter().find(|r| r.id == "long").unwrap();
        assert_eq!(
            row.first_prompt.as_deref().unwrap().chars().count(),
            FIRST_PROMPT_CHARS
        );
    }

    /// A summary reports the turns it can see and says when it saw the whole
    /// file, so `12` and `12+` are not the same claim.
    #[test]
    fn a_summary_reports_what_it_actually_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "s.jsonl",
            &journal(&[
                serde_json::json!({"ts":"2026-08-16T09:41:00Z","type":"session_start",
                                   "model":"m"}),
                serde_json::json!({"type":"user_input","content":"one"}),
                serde_json::json!({"type":"turn","outcome":"ok"}),
                serde_json::json!({"type":"user_input","content":"two"}),
                serde_json::json!({"type":"turn","outcome":"ok"}),
            ]),
        );
        let s = summarize(&tmp.path().join("s.jsonl")).unwrap();
        assert_eq!(s.id, "s");
        assert_eq!(s.turns, 2);
        assert!(s.complete);
        assert_eq!(s.first_prompt.as_deref(), Some("one"));
        assert_eq!(s.model.as_deref(), Some("m"));
        assert!(s.bytes > 0);

        assert!(matches!(
            summarize(&tmp.path().join("nope.jsonl")),
            Err(ResumeError::Io(_))
        ));
        assert!(matches!(
            summarize(&tmp.path().join("s.txt")),
            Err(ResumeError::NotAJournal)
        ));
    }

    /// Naming a session that is not there is an error, not an empty session.
    #[test]
    fn a_missing_or_turnless_session_refuses_to_resume() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_session(tmp.path(), "ghost"),
            Err(ResumeError::NotFound(_))
        ));
        assert!(matches!(
            load_session(tmp.path(), "../../etc/passwd"),
            Err(ResumeError::Id(_))
        ));
        write(
            tmp.path(),
            "headeronly.jsonl",
            "{\"type\":\"session_start\",\"model\":\"m\"}\n",
        );
        assert!(matches!(
            load_session(tmp.path(), "headeronly"),
            Err(ResumeError::NoTurns(_))
        ));
    }
}
