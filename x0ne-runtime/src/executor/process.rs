use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::action::Action;
use crate::config::AppConfig;
use crate::handles::{gen_id, HandleStatus, HandleType, SharedHandles};
use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Execute a whitelisted shell command.
/// If `stream: true`, spawns in background and returns a handle.
/// If `stream: false` (default), waits for completion and returns output.
pub async fn run_command(
    action: &Action,
    config: &AppConfig,
    handles: SharedHandles,
) -> ActionResult {
    let Action::RunCommand {
        command,
        args,
        cwd,
        env,
        timeout_secs,
        stream,
    } = action
    else {
        unreachable!()
    };

    // SECURITY: the `env` map is model-written text, and until 2026-08-15 it
    // was applied to the child with no filter on the key — so `{"command":
    // "cat", "env":{"PATH":"/tmp/evil"}}` ran /tmp/evil/cat while the human
    // gate, the audit log and `allowed_commands` all said `cat`. Refused, not
    // skipped: an agent reaching for LD_PRELOAD is something an operator has to
    // be told about. See `x0ne_actions::env_keys`.
    if let Err(why) = crate::child_env::check_caller_env(env) {
        return ActionResult::err("RunCommand", ErrorKind::EnvVarDenied, why, 0);
    }

    // SECURITY: single clamp site for both modes. The two branches below used
    // to each call `Duration::from_secs(timeout_secs.unwrap_or(_))` on the
    // model's own number, and `run_command_blocking` took a `_config` it never
    // read. Computing the effective duration here means a third mode cannot be
    // written without one.
    let streaming = stream.unwrap_or(false);
    let (timeout_duration, clamp_note) = clamp_timeout(
        *timeout_secs,
        if streaming {
            DEFAULT_STREAM_TIMEOUT_SECS
        } else {
            DEFAULT_BLOCKING_TIMEOUT_SECS
        },
        config.policy.max_command_timeout_secs,
    );

    if streaming {
        run_command_streaming(
            command,
            args,
            cwd,
            env,
            timeout_duration,
            clamp_note,
            handles,
            config,
        )
        .await
    } else {
        run_command_blocking(
            command,
            args,
            cwd,
            env,
            timeout_duration,
            clamp_note,
            config,
        )
        .await
    }
}

/// Default patience when the model names none. Blocking mode answers into the
/// turn, so it is short; streaming mode exists for builds, so it is long.
const DEFAULT_BLOCKING_TIMEOUT_SECS: u64 = 30;
const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 3600;

/// Resolve the effective timeout, bounded by the operator's ceiling.
///
/// Returns the duration to use and, when the requested value was cut down, a
/// sentence saying so.
///
/// SECURITY: the clamp is deliberately *visible*. A silent clamp is worse than
/// none: the model reads a killed command's partial output as a finished one
/// and acts on it. `policy.max_command_timeout_secs` is the operator's number;
/// `requested` is the model's. Never swap the two arguments of the `.min()`
/// for "clarity" — it reads the same and means the opposite.
fn clamp_timeout(
    requested: Option<u64>,
    default_secs: u64,
    ceiling_secs: u64,
) -> (Duration, Option<String>) {
    let asked = requested.unwrap_or(default_secs);
    if asked <= ceiling_secs {
        return (Duration::from_secs(asked), None);
    }
    (
        Duration::from_secs(ceiling_secs),
        Some(format!(
            "timeout_secs {asked} exceeds the policy ceiling and was clamped \
             to {ceiling_secs}s (policy.max_command_timeout_secs). Anything \
             below is what the command produced within {ceiling_secs}s, not \
             within {asked}s."
        )),
    )
}

// ── Blocking mode (default) ───────────────────────────────────────────────────

async fn run_command_blocking(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
    clamp_note: Option<String>,
    config: &AppConfig,
) -> ActionResult {
    // O-5b: this is the ONLY call site that spills. Streaming has its own
    // ceiling and its own vocabulary, and the shared `spawn_capture` serves
    // cloud probes and SafeTest, whose output nobody re-reads by path.
    //
    // The identifier is generated here rather than taken from the thread: the
    // runtime is handed an `Action`, never a `tool_call_id`. A fresh id per
    // invocation also means two calls never collide, which is what
    // `write_spill`'s suffix loop exists to survive when they do.
    let outputs_dir = config.executor.resolved_outputs_dir();
    let spill_id = gen_id("out");
    let result = spawn_capture_spilling(
        command,
        args,
        cwd,
        env,
        timeout_duration,
        Some(SpillTarget {
            dir: &outputs_dir,
            id: &spill_id,
        }),
        Some(config),
    )
    .await;

    match result {
        Ok(cap) => {
            let mut stderr = if cap.timed_out {
                format!(
                    "Command timed out after {}s. Partial output above/below \
                     is everything captured before the kill.\n{}",
                    timeout_duration.as_secs(),
                    cap.stderr
                )
            } else {
                cap.stderr
            };
            // SECURITY: the clamp rides on stderr because that is the field the
            // model already reads for "what the runtime did to your command",
            // and adding it there needs no change to the wire shape every
            // frontend parses. Prepended, not appended: `cap_capture` keeps the
            // head, so a truncated capture still carries the notice.
            if let Some(note) = clamp_note {
                stderr = format!("{note}\n{stderr}");
            }
            // SECURITY: the spill path is HANDED to the model, not opened for
            // it. `~/.x0ne/outputs` is deliberately absent from
            // `allowed_read_paths`, so re-reading it is a `ReadFile` like any
            // other — policy, then gate (design § 8-1). Auto-allowing the
            // directory here would be the "$HOME approved by default" the
            // design rejects, and it would arrive through a field nobody reads
            // as a policy change.
            ActionResult::ok(
                "RunCommand",
                ActionOutput::CommandOutput {
                    command: command.to_string(),
                    args: args.to_vec(),
                    exit_code: cap.exit_code,
                    stdout: cap.stdout,
                    stderr,
                    timed_out: cap.timed_out,
                    truncated: cap.spill.truncated,
                    omitted_bytes: cap.spill.omitted_bytes,
                    stdout_spill_path: cap.spill.stdout_path,
                    stderr_spill_path: cap.spill.stderr_path,
                    spill_refused: cap.spill.refused,
                },
                0,
            )
        }
        Err(e) => {
            // ENOENT from spawn() is ambiguous — could be the command, the cwd,
            // or (rarely) a shared library needed by the binary. Disambiguate
            // on the error path so the LLM does not waste turns guessing.
            if e.kind() == crate::executor::isolation::UNAVAILABLE_KIND {
                return ActionResult::err(
                    "RunCommand",
                    ErrorKind::MissingDependency,
                    e.to_string(),
                    0,
                );
            }
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                disambiguate_enoent(command, cwd)
            } else {
                e.to_string()
            };
            ActionResult::err("RunCommand", ErrorKind::ExecutionFailed, msg, 0)
        }
    }
}

/// Shell metacharacters that indicate the LLM packed an entire pipeline
/// into the `command` field (a common RunCommand misuse).
const SHELL_METAS: &[&str] = &["&&", "||", "|", ";", ">", "<", "`", "$("];

/// Disambiguate a `NotFound` error from `Command::spawn`. Called only on the
/// error path — stat calls are cheap and the happy path is untouched.
///
/// Priority order:
///   1. Command contains shell metacharacters → the LLM wrote a pipeline in
///      the command field. Most common root cause in agent traces.
///   2. `cwd` set but directory does not exist.
///   3. Command contains a slash (absolute/relative path) and file is missing.
///   4. Bare command (no slash) → likely a PATH lookup failure.
fn disambiguate_enoent(command: &str, cwd: &Option<String>) -> String {
    if SHELL_METAS.iter().any(|m| command.contains(m)) {
        return format!(
            "Shell syntax detected in `command` field. RunCommand executes \
             a single binary — it does not parse shell metacharacters (|, &&, ;, \
             redirection, substitution). Wrap your pipeline: \
             {{\"type\":\"RunCommand\",\"command\":\"sh\",\"args\":[\"-c\",{:?}]}}.",
            command
        );
    }
    if let Some(dir) = cwd {
        if !std::path::Path::new(dir).is_dir() {
            return format!(
                "Working directory not found: '{dir}'. Create it first \
                 (e.g. CreateDir) or omit `cwd` to use the default."
            );
        }
    }
    if command.contains('/') {
        if !std::path::Path::new(command).exists() {
            return format!("Executable not found at path: '{command}'.");
        }
        return format!(
            "Cannot execute '{command}' (os error 2). The file exists but \
             may not be executable or lacks a required loader."
        );
    }
    format!(
        "Command not found on PATH: '{command}'. Either install it or \
         invoke it via an absolute path like /usr/bin/{command}."
    )
}

/// What a bounded capture run produced. On `timed_out`, `stdout`/`stderr`
/// hold everything read before the kill — a hanging test suite's output is
/// exactly the diagnostic the model needs, and the old code threw it away.
pub(crate) struct Capture {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// What the cap took away, and where it went. Always filled; the paths are
    /// `None` unless the caller asked for a [`SpillTarget`] AND the write
    /// succeeded.
    pub spill: CaptureSpill,
}

/// Where the overflow of a capture went — the half of `Capture` that a caller
/// hands to the model.
#[derive(Default)]
pub(crate) struct CaptureSpill {
    pub truncated: bool,
    pub omitted_bytes: usize,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    /// A spill was due and refused — why, in the model's payload.
    pub refused: Option<String>,
}

/// The directory a capture may spill into, and the identifier its files are
/// named after. `None` at a call site means "cap and drop", which is what every
/// caller did before O-5b.
pub(crate) struct SpillTarget<'a> {
    pub dir: &'a Path,
    pub id: &'a str,
}

/// Per-stream ceiling on captured output. One `cargo build` on a broken
/// workspace can emit hundreds of KB; injected whole into the conversation
/// it evicts the task history around it (the context trimmer drops whole
/// messages, it does not shrink one). Head+tail keeps the two ends that
/// matter: the invocation banner and the final error summary.
pub(crate) const MAX_CAPTURE_BYTES: usize = 24_000;

pub(crate) fn cap_capture(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= MAX_CAPTURE_BYTES {
        return s.into_owned();
    }
    let head_len = MAX_CAPTURE_BYTES * 3 / 5;
    let tail_len = MAX_CAPTURE_BYTES - head_len;
    let mut head_end = head_len;
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail_len;
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[... {} bytes truncated ...]\n{}",
        &s[..head_end],
        s.len() - head_end - (s.len() - tail_start),
        &s[tail_start..]
    )
}

/// Cumulative ceiling on one spill directory. Past it, `cap_capture_spilling`
/// refuses to write and degrades to plain truncation rather than filling the
/// disk with build logs nobody reads (design § O-5).
///
/// SECURITY: never derive a test fixture from this constant — a mutant raising
/// it would make the fixture allocate it. Fixtures are literal, and
/// `the_context_ceiling_is_unchanged_by_the_spill_feature` pins both ceilings
/// by comparison (machine crash of 2026-08-16).
pub const MAX_SPILL_BYTES: usize = 67_108_864;

/// How many directory entries the cumulative-size scan will look at before it
/// gives up and refuses to spill. A bound, not a guess: `read_dir` on a
/// directory an agent has been writing into for a week must not turn one
/// command's capture into an unbounded walk.
const MAX_SPILL_ENTRIES: usize = 4_096;

/// How long a spill file lives before `write_spill` sweeps it. Spills
/// are working captures the model re-reads within its session — not an
/// archive; before this bound existed the directory kept potentially secret
/// outputs FOREVER, 0600 their only protection, until the 64 MiB cap silently
/// stopped all further spills.
///
/// SECURITY: never derive a test fixture from this constant (same rule as
/// `MAX_SPILL_BYTES`); tests pass their own retention to `purge_old_spills`.
const SPILL_RETENTION_SECS: u64 = 7 * 24 * 3600;

/// Capture rendered to the model, plus where the rest of it went.
///
/// `text` is exactly what `cap_capture` has always produced — the ceiling on
/// the model's context is untouched by this feature. `spill_path` is `Some`
/// only when the full bytes are readable at that path *right now*; every I/O
/// failure yields `None`, because a payload promising a file that does not
/// exist sends the model into a `ReadFile` that fails, and the failure gets
/// blamed on the policy.
pub(crate) struct Capped {
    pub text: String,
    pub spill_path: Option<String>,
    pub omitted_bytes: usize,
    /// `Some` only when a spill was DUE (output over the context cap, a
    /// target given) and could not be written — the reason, for the model.
    pub spill_refusal: Option<&'static str>,
}

/// Turn a thread-supplied identifier into a single, inert path component.
///
/// SECURITY: the identifier is a `tool_call_id` — text the model influences.
/// Everything outside `[A-Za-z0-9_-]` becomes `_`, so `../../etc/passwd` can
/// name a file inside the spill directory and nowhere else. Adapted from Grok
/// Build's `sanitized_stem`, mechanism only (design § O-5).
pub(crate) fn sanitized_stem(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "unnamed".to_string()
    } else {
        stem
    }
}

/// Bytes already sitting in `dir`, or `None` if the directory cannot be read
/// or holds more entries than the scan is allowed to look at.
fn spill_dir_usage(dir: &Path) -> Option<u64> {
    let entries = fs::read_dir(dir).ok()?;
    let mut total: u64 = 0;
    for (seen, entry) in entries.enumerate() {
        if seen >= MAX_SPILL_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        // `DirEntry::metadata` does not traverse a symlink, so the accounting
        // stays on what this directory itself holds.
        total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
    }
    Some(total)
}

/// `cap_capture`, but the bytes it drops are written to `dir` instead of lost.
///
/// The rendered text is byte-for-byte what `cap_capture` returns: this feature
/// changes what survives on disk, never what reaches the model's context.
/// Below the ceiling nothing is written at all — not even the directory.
pub(crate) fn cap_capture_spilling(bytes: &[u8], dir: &Path, id: &str) -> Capped {
    let text = cap_capture(bytes);
    let full_len = String::from_utf8_lossy(bytes).len();
    if full_len <= MAX_CAPTURE_BYTES {
        return Capped {
            text,
            spill_path: None,
            omitted_bytes: 0,
            spill_refusal: None,
        };
    }
    // Lower bound: `cap_capture` walks head/tail back to char boundaries, so a
    // few more bytes than this may be missing from `text`. Exact accounting
    // would re-implement `cap_capture`, which P2 of the design forbids — and
    // the whole payload is in the file anyway.
    let omitted_bytes = full_len.saturating_sub(MAX_CAPTURE_BYTES);
    match write_spill(bytes, dir, id) {
        Ok(path) => Capped {
            text,
            spill_path: Some(path),
            omitted_bytes,
            spill_refusal: None,
        },
        Err(why) => Capped {
            text,
            spill_path: None,
            omitted_bytes,
            spill_refusal: Some(why),
        },
    }
}

/// Write the full capture, 0600, under `dir`. `Err` names the refusal (
/// it used to be a silent `None` — past the 64 MiB cap the model stopped
/// receiving paths with no way to tell why) — the caller degrades to plain
/// truncation AND carries the reason, never a promised path.
fn write_spill(bytes: &[u8], dir: &Path, id: &str) -> Result<String, &'static str> {
    // Retention first: spills are session-scoped working files, and
    // before this sweep the directory kept potentially secret outputs
    // forever. Best effort — a failed sweep must not block the capture.
    purge_old_spills(dir, std::time::Duration::from_secs(SPILL_RETENTION_SECS));
    write_spill_bounded(bytes, dir, id, MAX_SPILL_BYTES as u64)
}

/// Sweep this runtime's own spill files (`out*.txt`, regular files, never
/// through a symlink) older than `retention`. Bounded by the same entry
/// budget as the usage scan; every failure is skipped, never fatal.
fn purge_old_spills(dir: &Path, retention: std::time::Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.take(MAX_SPILL_ENTRIES) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Only what `write_spill` itself names: a foreign file in a
        // user-pointed directory is not ours to delete.
        if !(name.starts_with("out") && name.ends_with(".txt")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue; // `metadata` on a DirEntry does not traverse symlinks
        }
        let old_enough = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= retention);
        if old_enough && fs::remove_file(entry.path()).is_ok() {
            tracing::info!(path = %entry.path().display(), "purged expired spill");
        }
    }
}

/// The bounded write itself, ceiling injected so a test can reach the refusal
/// without a 64 MiB fixture (`MAX_SPILL_BYTES` stays pinned by its own test).
fn write_spill_bounded(
    bytes: &[u8],
    dir: &Path,
    id: &str,
    max_bytes: u64,
) -> Result<String, &'static str> {
    fs::create_dir_all(dir).map_err(|_| "outputs directory could not be created")?;
    let used =
        spill_dir_usage(dir).ok_or("outputs directory unreadable or over its entry budget")?;
    if used.saturating_add(bytes.len() as u64) > max_bytes {
        return Err("outputs directory is at its size cap — the full output was NOT kept");
    }

    let stem = sanitized_stem(id);
    // SECURITY: `create_new`, so an existing entry is never written through.
    // The spill directory lives in $HOME and a pre-planted symlink there would
    // otherwise turn a command's stdout into a write to whatever it points at.
    // A collision is a refusal, so a handful of suffixes keep a reused id
    // useful instead of silently dropping every spill after the first.
    for attempt in 0..8u32 {
        let name = if attempt == 0 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{attempt}.txt")
        };
        let path = dir.join(name);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .map_err(|_| "io error writing the spill file")?;
                file.flush()
                    .map_err(|_| "io error writing the spill file")?;
                // Every write is logged with its size (design § O-5): the spill
                // directory grows silently otherwise, and `MAX_SPILL_BYTES`
                // refusing to write more is a thing an operator must be able to
                // explain from the log rather than by staring at `du`.
                tracing::info!(
                    path = %path.display(),
                    bytes = bytes.len(),
                    "spilled captured output"
                );
                return Ok(path.to_string_lossy().into_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("io error creating the spill file"),
        }
    }
    Err("spill file name collided 8 times")
}

/// Spawn + capture with the timeout INSIDE: on expiry the child is killed
/// and the partially-read buffers are returned instead of being dropped
/// with the future.
pub(crate) async fn spawn_capture(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
) -> std::io::Result<Capture> {
    spawn_capture_spilling(command, args, cwd, env, timeout_duration, None, None).await
}

/// [`spawn_capture`] for an agent-chosen command: confined by
/// `executor::isolation` (SafeTest). `spawn_capture` stays unconfined for the
/// runtime's own fixed probes (`cloud.rs`), which need gcloud's credentials.
pub(crate) async fn spawn_capture_isolated(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
    config: &AppConfig,
) -> std::io::Result<Capture> {
    spawn_capture_spilling(
        command,
        args,
        cwd,
        env,
        timeout_duration,
        None,
        Some(config),
    )
    .await
}

/// [`spawn_capture`], plus a directory the overflow is written to.
///
/// One implementation, two entry points: the defect class this repository keeps
/// rediscovering is the second copy written next door. `spill: None` is
/// byte-for-byte the old behaviour — cap and drop.
pub(crate) async fn spawn_capture_spilling(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    timeout_duration: Duration,
    spill: Option<SpillTarget<'_>>,
    isolate: Option<&AppConfig>,
) -> std::io::Result<Capture> {
    // SECURITY: the backstop, deliberately duplicated with the check at the top
    // of `run_command`. This is the shared spawn — `cloud.rs`, `efficient.rs`
    // and SafeTest reach the same child through it — and the defect class this
    // repository keeps rediscovering is the CALLER written afterwards that
    // nobody applied the fix to. The caller's check produces the good error
    // kind (EnvVarDenied); this one produces an error at all.
    if let Err(why) = crate::child_env::check_caller_env(env) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why));
    }

    // SECURITY: `None` only for the runtime's own fixed probes. An
    // agent-chosen command passes its config and runs under bwrap, or not at all.
    let spawn = match isolate {
        Some(c) => crate::executor::isolation::plan(
            &c.isolation,
            &c.policy,
            &c.desktop,
            command,
            args,
            cwd.as_deref(),
            &Default::default(),
        )?,
        None => direct_spawn(command, args, cwd),
    };
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args)
        // SECURITY: stdin MUST NOT be inherited. Without this the child gets
        // the runtime's own fd 0 — which in stdio mode *is* the bridge protocol
        // pipe from the orchestrator. `O_NONBLOCK` is a property of the shared
        // file *description*, not of the descriptor, so a child that sets it
        // (ssh does, on the fds it drives) poisons the parent's channel, and a
        // child we SIGKILL on timeout never restores it. The next read_line on
        // the bridge then returns EAGAIN and the runtime dies — 2026-08-14,
        // H100 milestone: a `gcloud compute ssh` probe timed out at 20 s, the
        // runtime exited "Resource temporarily unavailable (os error 11)", the
        // 26 following actions went nowhere and the automatic stop of a
        // 3.72 $/h VM failed. See openssh-portable#246 (fixed by 7be4ac8) and
        // jdebp.info/FGA/dont-set-shared-file-descriptors-to-non-blocking-mode.
        // RunCommand has no stdin by contract; `stream: true` opens its own
        // pipe for SendInput.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // SECURITY: the operator's environment stops here. This is the exact site
    // the 2026-08-14 audit measured: a canary exported in the shell that
    // launched `x0ne` came back out of `printenv`, into the model's context
    // (x0ne-orchestrator conversation.rs:854) and into the session log
    // (session.rs:100), in clear text and forever. `~/.x0ne/.env` is loaded by
    // dotenvy into this very process, so that was every API key the operator
    // owns. See `crate::child_env` for the allowlist and who needs each entry.
    //
    // Order matters and is not stylistic: `harden` clears, so the caller's
    // explicit `env` map must be applied AFTER it. Moving this below the loop
    // compiles and silently erases the action's own variables.
    crate::child_env::harden(&mut cmd);

    if let Some(dir) = &spawn.current_dir {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn()?;

    // Read stdout and stderr concurrently to prevent deadlocks when the
    // child writes heavily to both streams. The buffers live OUTSIDE the
    // read future so a timeout drops the future, not the bytes.
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // SECURITY: bounded reads. `read_to_end` with no ceiling held the whole
    // stream in RAM before `MAX_CAPTURE_BYTES` ever looked at it — the cap
    // applied to the context AFTER the allocation, so `yes | head -c 10G`
    // was 10 GiB of resident memory on the way to a 24 KiB summary. This
    // is the crash class that took the machine down twice (2026-08-15/16);
    // the streaming path bounded itself (`pipe.take`) and this one did not
    // (audit 2026-09-11, 03-F5).
    let (drained_out, drained_err) = {
        let read_fut = async {
            let stdout_fut = async {
                match stdout_handle {
                    Some(stdout) => {
                        read_capped(stdout, &mut stdout_buf, MAX_CAPTURE_READ_BYTES).await
                    }
                    None => 0,
                }
            };
            let stderr_fut = async {
                match stderr_handle {
                    Some(stderr) => {
                        read_capped(stderr, &mut stderr_buf, MAX_CAPTURE_READ_BYTES).await
                    }
                    None => 0,
                }
            };
            tokio::join!(stdout_fut, stderr_fut)
        };
        match timeout(timeout_duration, read_fut).await {
            Ok(d) => d,
            Err(_) => {
                let _ = child.kill().await;
                // The partial output of a hung suite is spilled too: it is the
                // very diagnostic the model came for, and the cap is exactly
                // as brutal here.
                let (stdout, stderr, spill) =
                    render_capture(&stdout_buf, &stderr_buf, spill.as_ref());
                return Ok(Capture {
                    exit_code: -1,
                    stdout,
                    stderr,
                    timed_out: true,
                    spill,
                });
            }
        }
    };

    let status = child.wait().await?;
    let (stdout, stderr, mut spill) = render_capture(&stdout_buf, &stderr_buf, spill.as_ref());
    // What the read ceiling discarded is part of what the model did not see.
    let discarded = drained_out.saturating_add(drained_err);
    if discarded > 0 {
        spill.truncated = true;
        spill.omitted_bytes = spill.omitted_bytes.saturating_add(discarded as usize);
    }
    Ok(Capture {
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        timed_out: false,
        spill,
    })
}

/// An unconfined spawn, for the runtime's own fixed probes only.
fn direct_spawn(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
) -> crate::executor::isolation::Spawn {
    crate::executor::isolation::Spawn {
        program: command.into(),
        args: args.iter().map(Into::into).collect(),
        current_dir: cwd.as_ref().map(Into::into),
        confinement: crate::executor::isolation::Confinement::HostOptOut,
    }
}

/// Per-stream ceiling on what a blocking capture keeps in memory. Equal to
/// the spill directory's cumulative ceiling: no spill could ever hold more,
/// so reading more would be buffering bytes that have nowhere to go.
///
/// SECURITY: never derive a test fixture from this constant (same rule as
/// `MAX_SPILL_BYTES`).
pub const MAX_CAPTURE_READ_BYTES: usize = 67_108_864;

/// Read a pipe into `buf` up to `cap` bytes, then keep reading and DISCARD
/// the rest until EOF, so the child never blocks on a full pipe. Returns the
/// number of bytes discarded — a count the caller owes the model.
///
/// Discarding rather than killing: a build that prints 100 MiB of warnings
/// should still finish and report its exit code; what it printed past the
/// ceiling is lost, and said to be.
pub(crate) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> u64 {
    let _ = (&mut pipe).take(cap as u64).read_to_end(buf).await;
    tokio::io::copy(&mut pipe, &mut tokio::io::sink())
        .await
        .unwrap_or(0)
}

/// Cap both streams, spilling each one that overflowed into its OWN file.
///
/// Both files carry the `id`, suffixed by stream, so an operator reading
/// `~/.x0ne/outputs` can tell which half of which command he is looking at.
fn render_capture(
    stdout_buf: &[u8],
    stderr_buf: &[u8],
    spill: Option<&SpillTarget<'_>>,
) -> (String, String, CaptureSpill) {
    let out = cap_stream(stdout_buf, spill, "out");
    let err = cap_stream(stderr_buf, spill, "err");
    let summary = CaptureSpill {
        // `truncated` is about the CONTEXT, not about the disk: a spill that
        // failed to be written still leaves the model with an amputated
        // output, and that is the fact it must be told.
        truncated: out.omitted_bytes > 0 || err.omitted_bytes > 0,
        omitted_bytes: out.omitted_bytes.saturating_add(err.omitted_bytes),
        stdout_path: out.spill_path,
        stderr_path: err.spill_path,
        // One reason serves both streams: they share the directory and its
        // cap, so the first refusal explains the second.
        refused: out
            .spill_refusal
            .or(err.spill_refusal)
            .map(|r| r.to_string()),
    };
    (out.text, err.text, summary)
}

/// One stream: capped always, written only when a target was asked for.
fn cap_stream(bytes: &[u8], spill: Option<&SpillTarget<'_>>, suffix: &str) -> Capped {
    match spill {
        Some(target) => cap_capture_spilling(bytes, target.dir, &format!("{}-{suffix}", target.id)),
        None => {
            let text = cap_capture(bytes);
            Capped {
                omitted_bytes: String::from_utf8_lossy(bytes)
                    .len()
                    .saturating_sub(MAX_CAPTURE_BYTES),
                text,
                spill_path: None,
                // No target asked for: nothing was due, nothing was refused.
                spill_refusal: None,
            }
        }
    }
}

// ── Streaming mode ────────────────────────────────────────────────────────────

/// Cumulative ceiling on what a streaming command may push, both streams
/// together, for the whole life of the child.
///
/// SECURITY: streaming was the mode with no byte ceiling at all.
/// `MAX_CAPTURE_BYTES` bounds `spawn_capture`, which streaming does not use,
/// and the handle ring caps the NUMBER of buffered events, never their size —
/// so `yes` measured 41 179 878 bytes pushed in 20 seconds on 2026-08-15, into
/// the handle buffer, the orchestrator's drain and the model's context, with a
/// core spinning behind it and no end.
///
/// 1 MiB and not `MAX_CAPTURE_BYTES` (24 000): streaming exists for builds
/// whose output the model consumes as it arrives, and 24 KB is less than one
/// `cargo build` on a broken workspace. 1 MiB is `max_read_bytes`' own default
/// — the most content the runtime hands back for any one action — and it lets
/// `yes` die in under a second.
pub const MAX_STREAM_BYTES: usize = 1_048_576;

/// Pump one pipe into the handle's event buffer, line by line, spending from a
/// budget shared with the other pipe.
///
/// SECURITY: two ceilings, and both are needed. `take` bounds a stream that
/// never emits a newline — `next_line` would otherwise grow one `String`
/// without limit and the process dies of OOM before any counter is consulted.
/// The counter bounds the total across both pipes and is what triggers the
/// kill. Removing either one restores an unbounded path.
fn spawn_line_pump<R>(
    pipe: Option<R>,
    stream_name: &'static str,
    hid: String,
    handles: SharedHandles,
    budget: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    over_budget: std::sync::Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(pipe) = pipe else { return };
        let mut reader = BufReader::new(pipe.take(MAX_STREAM_BYTES as u64)).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let cost = line.len() + 1; // the newline `lines()` consumed
            let spent = budget.fetch_add(cost, std::sync::atomic::Ordering::Relaxed) + cost;
            if spent > MAX_STREAM_BYTES {
                let ev = json!({
                    "stream": "truncated",
                    "limit_bytes": MAX_STREAM_BYTES,
                    "message": format!(
                        "Output exceeded {MAX_STREAM_BYTES} bytes across stdout and \
                         stderr; the stream stops here and the process was stopped. \
                         Re-run with a narrower command if you need the rest."
                    ),
                    "timestamp": Utc::now().to_rfc3339(),
                });
                handles.lock().await.push_event(&hid, ev);
                over_budget.notify_one();
                return;
            }
            let ev = json!({
                "stream": stream_name,
                "line": line,
                "timestamp": Utc::now().to_rfc3339(),
            });
            handles.lock().await.push_event(&hid, ev);
        }
    })
}

async fn run_command_streaming(
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    timeout_dur: Duration,
    clamp_note: Option<String>,
    handles: SharedHandles,
    config: &AppConfig,
) -> ActionResult {
    // SECURITY: streaming is a second, separately written spawn site, and it
    // does not go through `spawn_capture`. Checked here too rather than trusted
    // to the caller — the same reason as `spawn_capture`'s own backstop.
    if let Err(why) = crate::child_env::check_caller_env(env) {
        return ActionResult::err("RunCommand", ErrorKind::EnvVarDenied, why, 0);
    }

    // SECURITY: planned before the handle exists, so a refusal leaves
    // nothing registered.
    let spawn = match crate::executor::isolation::plan(
        &config.isolation,
        &config.policy,
        &config.desktop,
        command,
        args,
        cwd.as_deref(),
        &Default::default(),
    ) {
        Ok(s) => s,
        Err(e) if e.kind() == crate::executor::isolation::UNAVAILABLE_KIND => {
            return ActionResult::err("RunCommand", ErrorKind::MissingDependency, e.to_string(), 0)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ActionResult::err(
                "RunCommand",
                ErrorKind::ExecutionFailed,
                disambiguate_enoent(command, cwd),
                0,
            )
        }
        Err(e) => {
            return ActionResult::err("RunCommand", ErrorKind::ExecutionFailed, e.to_string(), 0)
        }
    };

    let handle_id = gen_id("proc");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::Process) {
            return ActionResult::err("RunCommand", ErrorKind::Internal, e, 0);
        }
        // SECURITY: streaming has no stderr field to carry the notice, so the
        // clamp becomes the first event of the stream. Same rule as the
        // blocking path: a clamp the model cannot see is a lie about how long
        // its command was given.
        if let Some(note) = clamp_note {
            mgr.push_event(
                &handle_id,
                json!({ "stream": "notice", "message": note, "timestamp": Utc::now().to_rfc3339() }),
            );
        }
    }

    // Build command
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SECURITY: same barrier as `spawn_capture`, and the same ordering rule —
    // clear first, caller's explicit `env` after. `stream: true` is the mode
    // whose output is pushed to the orchestrator line by line, so a leak here
    // reaches the transcript exactly as fast.
    crate::child_env::harden(&mut cmd);
    if let Some(dir) = &spawn.current_dir {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            handles.lock().await.set_status(
                &handle_id,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: e.to_string(),
                },
            );
            return ActionResult::err("RunCommand", ErrorKind::ExecutionFailed, e.to_string(), 0);
        }
    };

    let pid = child.id();

    // Capture stdin for SendInput support
    if let Some(child_stdin) = child.stdin.take() {
        let shared_stdin = std::sync::Arc::new(tokio::sync::Mutex::new(child_stdin));
        handles.lock().await.set_stdin(&handle_id, shared_stdin);
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let handles_clone = handles.clone();
    let hid = handle_id.clone();

    let task = tokio::spawn(async move {
        // SECURITY: one budget for both pipes, and one `Notify` to say it is
        // gone. Per-stream budgets would let a child spend twice the ceiling,
        // and a flag polled by the waiter would let it spend whatever it
        // manages between two polls.
        let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let over_budget = std::sync::Arc::new(tokio::sync::Notify::new());

        let stdout_task = spawn_line_pump(
            stdout,
            "stdout",
            hid.clone(),
            handles_clone.clone(),
            budget.clone(),
            over_budget.clone(),
        );
        let stderr_task = spawn_line_pump(
            stderr,
            "stderr",
            hid.clone(),
            handles_clone.clone(),
            budget.clone(),
            over_budget.clone(),
        );

        // Wait for the child — until its timeout, or until it has said more
        // than anyone is allowed to hear.
        //
        // SECURITY: the budget branch KILLS. Draining without pushing would
        // leave a process burning a core to write into a pipe nobody reads,
        // for as long as the handle lives; a process that talks with no
        // listener has no reason left to run. The operator can always start it
        // again with a narrower command.
        let result = tokio::select! {
            r = tokio::time::timeout(timeout_dur, child.wait()) => r,
            _ = over_budget.notified() => {
                let _ = child.start_kill();
                Ok(child.wait().await)
            }
        };

        let _ = stdout_task.await;
        let _ = stderr_task.await;

        let mut mgr = handles_clone.lock().await;
        match result {
            Ok(Ok(status)) => {
                let exit = status.code().unwrap_or(-1);
                let ev = json!({ "stream": "exit", "exit_code": exit, "timestamp": Utc::now().to_rfc3339() });
                mgr.push_event(&hid, ev);
                mgr.set_status(
                    &hid,
                    HandleStatus::Completed {
                        at: Utc::now().to_rfc3339(),
                    },
                );
            }
            Ok(Err(e)) => {
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: e.to_string(),
                    },
                );
            }
            Err(_) => {
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: "Command timed out".into(),
                    },
                );
            }
        }
    });

    {
        let mut mgr = handles.lock().await;
        mgr.set_task(&handle_id, task);
        mgr.set_pid(&handle_id, pid);
    }

    ActionResult::ok(
        "RunCommand",
        ActionOutput::HandleCreated {
            handle: handle_id,
            handle_type: "process".into(),
            streaming: true,
            pid,
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::handles;
    use crate::result::ActionOutput;

    fn make_handles() -> SharedHandles {
        handles::new_shared(1000, 300, 20)
    }

    #[tokio::test]
    async fn run_command_blocking_returns_output() {
        let cfg = AppConfig::defaults();
        let h = make_handles();
        let action = Action::RunCommand {
            command: "echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, h).await;
        assert!(result.success);
        if let Some(ActionOutput::CommandOutput {
            stdout, exit_code, ..
        }) = result.output
        {
            assert_eq!(exit_code, 0);
            assert!(stdout.contains("hello"));
        }
    }

    #[tokio::test]
    async fn run_command_streaming_returns_handle() {
        let cfg = AppConfig::defaults();
        let h = make_handles();
        let action = Action::RunCommand {
            command: "echo".into(),
            args: vec!["streaming".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: Some(true),
        };
        let result = run_command(&action, &cfg, h.clone()).await;
        assert!(result.success);
        if let Some(ActionOutput::HandleCreated {
            handle, streaming, ..
        }) = result.output
        {
            assert!(streaming);
            // Wait for completion
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // Check events
            let events = h.lock().await.drain_events(&handle).unwrap_or_default();
            assert!(
                events.iter().any(|e| e.get("line").is_some()),
                "expected output lines"
            );
        } else {
            panic!("expected HandleCreated");
        }
    }

    #[tokio::test]
    async fn timeout_preserves_partial_output_instead_of_discarding_it() {
        // A hanging test suite's partial output is exactly the diagnostic
        // the model needs. The old code dropped the read future — and the
        // buffers with it — on timeout.
        let cfg = AppConfig::defaults();
        let h = make_handles();
        let action = Action::RunCommand {
            command: "sh".into(),
            args: vec!["-c".into(), "echo before-the-hang; sleep 30".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: Some(1),
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, h).await;
        assert!(result.success);
        let Some(ActionOutput::CommandOutput {
            stdout,
            stderr,
            timed_out,
            exit_code,
            ..
        }) = result.output
        else {
            panic!("expected CommandOutput");
        };
        assert!(timed_out);
        assert_eq!(exit_code, -1);
        assert!(
            stdout.contains("before-the-hang"),
            "partial stdout lost: {stdout:?}"
        );
        assert!(stderr.contains("timed out"), "stderr: {stderr:?}");
    }

    #[test]
    fn cap_capture_keeps_head_and_tail_with_marker() {
        let big = "a".repeat(30_000) + "THE-END";
        let capped = cap_capture(big.as_bytes());
        assert!(capped.len() < 25_000, "not capped: {}", capped.len());
        assert!(capped.starts_with("aaa"));
        assert!(capped.ends_with("THE-END"), "tail lost");
        assert!(capped.contains("bytes truncated"));
        // Small outputs pass through untouched.
        assert_eq!(cap_capture(b"small"), "small");
    }

    // ── Output spilling (O-5a) ───────────────────────────────────────────────

    /// The identifier cannot escape the spill directory.
    ///
    /// SECURITY: the identifier comes from the wire — it is a `tool_call_id`,
    /// hence text the model influences. A failure would mean a command output
    /// is written where the model asked, for example over a configuration or
    /// a key.
    #[test]
    fn a_hostile_identifier_cannot_escape_the_spill_directory() {
        for hostile in [
            "../../etc/passwd",
            "a/b",
            "..",
            "",
            "a\0b",
            "~/.ssh/id_ed25519",
        ] {
            let stem = sanitized_stem(hostile);
            assert!(
                !stem.contains('/'),
                "{hostile:?} → {stem:?} contains a separator"
            );
            assert!(!stem.contains(".."), "{hostile:?} → {stem:?} contient ..");
            assert!(!stem.is_empty(), "{hostile:?} → empty identifier");
            assert!(
                stem.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{hostile:?} → {stem:?} leaves the allowed alphabet"
            );
        }
    }

    /// The context bound DOES NOT CHANGE.
    ///
    /// SECURITY: pins the constant by comparison, never by a fixture sized on
    /// it — the rule born from the machine crash of 2026-08-16 (fixture
    /// derived from a mutated constant → 9 EB allocated).
    #[test]
    fn the_context_ceiling_is_unchanged_by_the_spill_feature() {
        assert_eq!(MAX_CAPTURE_BYTES, 24_000);
        assert_eq!(MAX_SPILL_BYTES, 67_108_864);
        assert_eq!(MAX_CAPTURE_READ_BYTES, 67_108_864);
    }

    /// A blocking capture keeps at most the ceiling in memory, drains the
    /// rest so the child can finish, and counts what it threw away.
    ///
    /// SECURITY: a failure in the first assertion is the unbounded
    /// `read_to_end` of before 2026-09-11 — the whole stream resident before
    /// any cap. The fixture is a LITERAL 1000-byte cap over a 5000-byte
    /// stream, never derived from the production constant (machine crash
    /// of 2026-08-16).
    #[tokio::test]
    async fn a_capped_read_keeps_the_ceiling_drains_the_rest_and_counts_it() {
        let five_thousand = std::io::Cursor::new(vec![b'x'; 5000]);
        let mut buf = Vec::new();
        let discarded = read_capped(five_thousand, &mut buf, 1000).await;
        assert_eq!(buf.len(), 1000, "exactly the ceiling is kept");
        assert_eq!(discarded, 4000, "the rest is counted, not lost in silence");

        let short = std::io::Cursor::new(vec![b'y'; 10]);
        let mut buf = Vec::new();
        assert_eq!(read_capped(short, &mut buf, 1000).await, 0);
        assert_eq!(buf.len(), 10);
    }

    /// Under the bound, nothing is written to disk.
    #[test]
    fn a_short_output_writes_no_file_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let out = cap_capture_spilling(b"hello", dir.path(), "call-1");
        assert_eq!(out.text, "hello");
        assert!(out.spill_path.is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Beyond it, the rest is written with mode 0600 and the path is returned.
    #[test]
    fn a_refused_spill_names_its_reason_to_the_model_never_silence() {
        // SECURITY: a failure would mean the silent cap is back — past
        // 64 MiB the model stopped receiving paths with no way to tell
        // "nothing omitted" from "everything beyond is lost".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out_taken.txt"), vec![7u8; 64]).unwrap();
        // Injected cap (the constant stays pinned by its own test):
        // 64 bytes already in place + 32 new > 80 → named refusal.
        let err = write_spill_bounded(&[1u8; 32], dir.path(), "x", 80).unwrap_err();
        assert!(err.contains("size cap"), "{err}");
        assert!(err.contains("NOT kept"), "{err}");
        // And the wiring: a directory that cannot be created (the path is a
        // FILE) refuses with its reason in `Capped.spill_refusal`.
        let file_as_dir = dir.path().join("not-a-dir");
        std::fs::write(&file_as_dir, b"x").unwrap();
        let big = vec![b'A'; 25_000]; // LITERAL size > MAX_CAPTURE_BYTES
        let capped = cap_capture_spilling(&big, &file_as_dir, "y");
        assert!(capped.spill_path.is_none());
        assert!(
            capped
                .spill_refusal
                .is_some_and(|r| r.contains("could not be created")),
            "{:?}",
            capped.spill_refusal
        );
    }

    /// Spills expire. Only OUR files (`out*.txt`, regular files) go; a
    /// foreign file in a directory the operator pointed to is never ours to
    /// delete. A failure would mean either infinite retention is back, or a
    /// purge that eats other people's files.
    #[test]
    fn expired_spills_are_swept_and_foreign_files_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("out_old-out.txt");
        let foreign = dir.path().join("notes.txt");
        std::fs::write(&ours, b"secret").unwrap();
        std::fs::write(&foreign, b"keep me").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Zero retention: everything of ours older than "now" goes.
        purge_old_spills(dir.path(), std::time::Duration::ZERO);
        assert!(!ours.exists(), "the expired spill must go");
        assert!(foreign.exists(), "a foreign file NEVER goes");
        // And a young spill stays: one-hour retention.
        let young = dir.path().join("out_young-err.txt");
        std::fs::write(&young, b"s").unwrap();
        purge_old_spills(dir.path(), std::time::Duration::from_secs(3600));
        assert!(young.exists(), "a spill within retention stays");
    }

    #[test]
    fn a_long_output_is_written_privately_and_its_path_returned() {
        let dir = tempfile::tempdir().unwrap();
        // LITERAL size, never derived from MAX_CAPTURE_BYTES.
        let payload = vec![b'x'; 30_000];
        let out = cap_capture_spilling(&payload, dir.path(), "call-1");

        let path = out.spill_path.expect("a spill should have been written");
        assert_eq!(std::fs::read(&path).unwrap().len(), 30_000);
        assert!(out.omitted_bytes > 0);
        assert!(
            out.text.len() <= 24_000 + 200,
            "the returned text exceeds the bound"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the spill must be private");
        }
    }

    /// A directory already full refuses to write, it does not fill the disk.
    ///
    /// SECURITY: `MAX_SPILL_BYTES` is the only thing between "one chatty
    /// command per turn" and a saturated `$HOME`. The witness file is
    /// **sparse** (`set_len`, no byte written): its apparent size is what
    /// `read_dir` counts, and nothing is allocated — neither in memory nor on
    /// disk. The payload size stays literal.
    #[test]
    fn a_full_spill_directory_refuses_to_write_more() {
        let dir = tempfile::tempdir().unwrap();
        let squatter = std::fs::File::create(dir.path().join("already-there.txt")).unwrap();
        squatter.set_len(67_108_864).unwrap();
        drop(squatter);

        let payload = vec![b'x'; 30_000];
        let out = cap_capture_spilling(&payload, dir.path(), "call-1");
        assert!(
            out.spill_path.is_none(),
            "the cumulative cap did not refuse the write"
        );
        assert!(out.text.contains("truncated"));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "a file was written beyond the cap"
        );
    }

    /// A write failure degrades to plain truncation, without promising a file.
    ///
    /// SECURITY: a payload announcing an absent path would send the model to
    /// read a file that does not exist, and the resulting refusal would be
    /// attributed to the policy. An honest truncation is better.
    #[test]
    fn a_failed_write_degrades_to_plain_truncation_without_a_path() {
        let missing = std::path::Path::new("/proc/definitely/not/writable/x0ne");
        let payload = vec![b'x'; 30_000];
        let out = cap_capture_spilling(&payload, missing, "call-1");
        assert!(
            out.spill_path.is_none(),
            "a path is promised although the write failed"
        );
        assert!(out.text.contains("truncated"));
    }

    // ── The spill reaches the payload (O-5b) ─────────────────────────────────

    /// A configuration whose spill directory is disposable.
    fn config_spilling_into(dir: &std::path::Path) -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.executor.outputs_dir = dir.join("outputs").to_string_lossy().into_owned();
        // The fixtures live in a /tmp tempdir: a read root is what makes them
        // visible inside the command's private /tmp.
        cfg.policy.allowed_read_paths = vec![dir.to_string_lossy().into_owned()];
        cfg
    }

    /// The `RunCommand` payload carries the spill path, and the file it names
    /// contains the WHOLE output.
    ///
    /// SECURITY: this is the "useful" half of spec decision § 8-1 — the model
    /// only gets its output back through this path, and re-reads it through
    /// `ReadFile`, hence under policy. A failure would mean either the path
    /// never leaves the runtime (the Task 13 write is useless), or it is
    /// promised without the file existing — and the model would then blame
    /// the policy for a refusal that is not one.
    #[tokio::test]
    async fn a_capped_run_command_hands_back_the_path_to_the_whole_output() {
        let dir = tempfile::tempdir().unwrap();
        let noisy = dir.path().join("noisy.txt");
        // LITERAL size, never derived from MAX_CAPTURE_BYTES: a fixture sized
        // on a constant becomes a bomb as soon as a mutant raises it to
        // u64::MAX/2 (machine crash of 2026-08-16).
        std::fs::write(&noisy, vec![b'x'; 30_000]).unwrap();
        let cfg = config_spilling_into(dir.path());

        let action = Action::RunCommand {
            command: "cat".into(),
            args: vec![noisy.to_string_lossy().into_owned()],
            cwd: None,
            env: Default::default(),
            timeout_secs: Some(30),
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, make_handles()).await;
        assert!(result.success, "cat failed: {:?}", result.error);

        let Some(ActionOutput::CommandOutput {
            stdout,
            truncated,
            omitted_bytes,
            stdout_spill_path,
            stderr_spill_path,
            ..
        }) = result.output
        else {
            panic!("expected CommandOutput");
        };

        assert!(truncated, "the capping is not declared to the model");
        assert!(omitted_bytes > 0, "no byte is declared missing");
        assert!(stdout.contains("bytes truncated"), "stdout : {stdout:.80?}");

        let path = stdout_spill_path.expect("the payload must carry the spill path");
        assert!(
            path.starts_with(&cfg.executor.outputs_dir),
            "the spill left the configured directory: {path}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            30_000,
            "the file does not hold the whole output"
        );
        assert!(
            stderr_spill_path.is_none(),
            "stderr spilled nothing, yet a path is promised"
        );
    }

    /// Under the bound: no path promised, and **no directory created**.
    ///
    /// SECURITY: a failure would mean every `echo` of a turn leaves a trace in
    /// `~/.x0ne/outputs` — an agent's command output is exactly what contains
    /// secrets, and what is not written does not leak.
    #[tokio::test]
    async fn a_short_run_command_promises_nothing_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_spilling_into(dir.path());

        let action = Action::RunCommand {
            command: "echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: Some(30),
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, make_handles()).await;

        let Some(ActionOutput::CommandOutput {
            truncated,
            omitted_bytes,
            stdout_spill_path,
            stderr_spill_path,
            ..
        }) = result.output
        else {
            panic!("expected CommandOutput");
        };
        assert!(!truncated);
        assert_eq!(omitted_bytes, 0);
        assert!(stdout_spill_path.is_none());
        assert!(stderr_spill_path.is_none());
        assert!(
            !dir.path().join("outputs").exists(),
            "the spill directory was created for a 6-byte output"
        );
    }

    /// A stderr spill has its OWN file.
    ///
    /// Concatenating both streams would give the model a file it can no
    /// longer split: a `cargo build`'s output is on stderr, its progress on
    /// stdout, and the diagnosis is the difference between the two.
    #[tokio::test]
    async fn stdout_and_stderr_spill_into_two_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let noisy = dir.path().join("noisy.txt");
        std::fs::write(&noisy, vec![b'x'; 30_000]).unwrap();
        let cfg = config_spilling_into(dir.path());

        let action = Action::RunCommand {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "cat {p}; cat {p} 1>&2",
                    p = noisy.to_string_lossy().into_owned()
                ),
            ],
            cwd: None,
            env: Default::default(),
            timeout_secs: Some(30),
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, make_handles()).await;

        let Some(ActionOutput::CommandOutput {
            stdout_spill_path,
            stderr_spill_path,
            ..
        }) = result.output
        else {
            panic!("expected CommandOutput");
        };
        let out = stdout_spill_path.expect("stdout spilled");
        let err = stderr_spill_path.expect("stderr spilled");
        assert_ne!(out, err, "both streams wrote to the same file");
        assert_eq!(std::fs::read(&out).unwrap().len(), 30_000);
        assert_eq!(std::fs::read(&err).unwrap().len(), 30_000);
    }

    // ── ENOENT disambiguation ────────────────────────────────────────────────

    #[test]
    fn disambiguate_detects_shell_pipeline_in_command() {
        let msg = disambiguate_enoent("cd /home/user && cargo test", &None);
        assert!(msg.contains("Shell syntax detected"), "got: {msg}");
        assert!(msg.contains("sh"));
        assert!(msg.contains("-c"));
    }

    #[test]
    fn disambiguate_detects_pipe_character() {
        let msg = disambiguate_enoent("ls | grep foo", &None);
        assert!(msg.contains("Shell syntax"));
    }

    #[test]
    fn disambiguate_flags_missing_cwd_first() {
        let msg = disambiguate_enoent("echo", &Some("/absolutely/does/not/exist".into()));
        assert!(msg.contains("Working directory not found"), "got: {msg}");
        assert!(msg.contains("/absolutely/does/not/exist"));
    }

    #[test]
    fn disambiguate_catches_missing_absolute_path_executable() {
        let msg = disambiguate_enoent("/nonexistent/bin/ghostcmd", &None);
        assert!(msg.contains("Executable not found at path"), "got: {msg}");
    }

    #[test]
    fn disambiguate_hints_path_lookup_for_bare_command() {
        let msg = disambiguate_enoent("definitelynotreal", &None);
        assert!(msg.contains("Command not found on PATH"), "got: {msg}");
    }

    #[tokio::test]
    async fn run_command_surfaces_shell_pipeline_hint() {
        let cfg = AppConfig::defaults();
        let h = make_handles();
        let action = Action::RunCommand {
            command: "echo hi && ls".into(),
            args: vec![],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, h).await;
        assert!(!result.success);
        let msg = result
            .error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("");
        assert!(
            msg.contains("Shell syntax"),
            "expected shell-syntax hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_command_surfaces_missing_cwd_hint() {
        let cfg = AppConfig::defaults();
        let h = make_handles();
        let action = Action::RunCommand {
            command: "echo".into(),
            args: vec!["hi".into()],
            cwd: Some("/does/not/exist/at/all".into()),
            env: Default::default(),
            timeout_secs: None,
            stream: Some(false),
        };
        let result = run_command(&action, &cfg, h).await;
        assert!(!result.success);
        let msg = result
            .error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("");
        assert!(
            msg.contains("Working directory not found"),
            "expected cwd hint, got: {msg}"
        );
    }
}
