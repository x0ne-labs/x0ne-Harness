//! The typed action vocabulary shared across the X0NE workspace.
//!
//! This crate is the single source of truth for what an agent may ask for and
//! what that request *means* for security. It deliberately has no I/O, no
//! policy engine and no executor — only the vocabulary and its semantics — so
//! that every layer above (policy, simulate, execute, trace) reasons about the
//! same types instead of re-deriving them from JSON.
//!
//! The load-bearing invariant lives in [`Action::side_effect_class`]: a match
//! over every variant with **no wildcard arm**. Adding a variant fails the
//! build until its side-effect class is stated, so a new action cannot slip
//! into the system unclassified and be treated as inert.

/// Which environment variable names an action's own `env` map may hold.
///
/// Lives here, in the pure vocabulary crate, because both sides of the
/// workspace need the same answer and `x0ne-orchestrator` deliberately does not
/// depend on `x0ne-runtime` (see its `Cargo.toml`, line 56). One rule, one
/// implementation — the four hand-rolled variants that preceded it are what let
/// `RunCommand` and `ArtifactServe` be missed entirely.
pub mod deny_paths;
pub mod env_keys;
/// Host / Origin / bearer guards shared by the HTTP serve modes.
pub mod http_guard;
/// Which vendor key may reach which host: the session's model calls and the
/// runtime's `auth` read one table.
pub mod model_keys;

/// How an `ArtifactServe` command line becomes an argv, and which lines are
/// refused.
///
/// Lives here for the same reason as [`env_keys`]: the policy gate and the
/// executor must give the same answer to "which program does this line run",
/// and the only way to guarantee that is one implementation. Before it, the
/// gate read the first whitespace token and the executor handed the whole line
/// to `sh -c`.
pub mod serve_command;

/// The four things an operator can widen from a prompt, and the control
/// message that widens them.
///
/// Lives here for the same reason as [`env_keys`]: the runtime decides what a
/// refusal is, the orchestrator decides whether to put it to a human, and both
/// must name the same four things. SECURITY: nothing in it is an [`Action`] —
/// see the module docs.
pub mod policy_grant;

/// A `RunCommand` whose whole line sits in `command`, split into argv.
pub mod command_line;

/// Which programs a shell/`env` invocation actually runs, for the command gate.
pub mod interpreter;

/// The OpenSSH options and destinations an agent may hand to `ssh`, for
/// `SshExec` and the host inventory (H3-2).
pub mod ssh_options;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Every action the LLM can request.
///
/// The LLM emits a JSON object like:
/// ```json
/// { "type": "ReadFile", "path": "/tmp/foo.txt" }
/// ```
/// and this enum captures every valid variant with its parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum Action {
    // ── Filesystem ────────────────────────────────────────────────────────────
    /// Read the full contents of a file (UTF-8).
    ReadFile {
        path: String,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        limit: Option<u64>,
        /// 1-based first line to read. When set, the read is line-oriented:
        /// the result is a FileChunk whose coordinates line up with
        /// SearchCode/SearchAndRead line numbers, instead of a byte slice
        /// that can start mid-line.
        #[serde(default)]
        start_line: Option<usize>,
        /// Max number of lines when `start_line` is set (default 200).
        #[serde(default)]
        limit_lines: Option<usize>,
    },

    /// Read several files in one policy-checked action.
    ReadFiles {
        paths: Vec<String>,
        #[serde(default)]
        max_bytes_per_file: Option<u64>,
    },

    /// Overwrite (or create) a file with new content.
    WriteFile { path: String, content: String },

    /// Apply multiple edits to a single file sequentially.
    EditFile {
        path: String,
        edits: Vec<Edit>,
        /// If true, create the file if it does not exist using the `new` param of the first edit
        #[serde(default)]
        create_if_missing: Option<bool>,
    },

    /// Apply a standard unified diff patch.
    ApplyPatch {
        path: String,
        patch: String,
        #[serde(default)]
        reverse: Option<bool>,
    },

    /// Metadata for several files, optionally including sha256.
    StatFiles {
        paths: Vec<String>,
        #[serde(default)]
        hash: Option<bool>,
    },

    // ── Filesystem fluff removed in v0.3 ─────────────────────────────────────
    //
    // The shell does these in one round-trip, with audit via run_in_sandbox
    // when needed: ReadFileChunk/ReadFileLines (head/sed -n), WriteFileLines
    // (printf), ReplaceInFile/ReplaceMany/InsertAtLine (sed -i), AppendFile/
    // AppendLines (>>), CreateDir (mkdir), CopyFile/MoveFile (cp/mv),
    // CreateTempFile (mktemp), ListDir/FindFiles (ls/find), TreeView (tree),
    // DiskUsage (du). Kept: ReadFile, ReadFiles, WriteFile, EditFile,
    // ApplyPatch, PatchSet, MoveToTrash, FileInfo, StatFiles, Checksum, Diff.
    /// Apply multiple unified diffs in one action.
    PatchSet {
        patches: Vec<PatchSetItem>,
        #[serde(default)]
        stop_on_error: Option<bool>,
    },

    /// Search and return surrounding context chunks in one call.
    SearchAndRead {
        // `path` accepted as an alias of `root` — LLMs trained on
        // generic filesystem-action shapes overwhelmingly emit `path`,
        // and rejecting the call over a naming mismatch is the worst
        // possible UX for a primitive we want widely used.
        #[serde(default, alias = "path")]
        root: String,
        #[serde(default)]
        pattern: String,
        #[serde(default)]
        patterns: Vec<String>,
        #[serde(default)]
        file_glob: Option<String>,
        #[serde(default)]
        context_lines: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },

    /// Run a typed test command without raw shell syntax.
    SafeTest {
        kind: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Run only tests matching this name — `cargo test <filter>`,
        /// `pytest -k <filter>`, `go test -run <filter>`. The short
        /// edit→retest loop needs single-test runs, not the whole suite.
        #[serde(default)]
        filter: Option<String>,
    },

    /// Move a file or directory to the OS trash.
    MoveToTrash { path: String },

    // JsonSet / TomlSet / ListDir / FindFiles / TreeView / CreateDir /
    // CopyFile / MoveFile / CreateTempFile / ProjectOverview removed in v0.3.
    // Shell does these in one round-trip: jq, dasel, ls, find, mkdir, cp, mv,
    // mktemp, tree.

    // ── Search ────────────────────────────────────────────────────────────────
    /// Regex search inside file contents under a directory.
    /// Pass either `pattern` (single regex) or `patterns` (multiple regexes,
    /// OR-matched in one walk via RegexSet) — never both.
    SearchCode {
        // `path` accepted as an alias of `root`. See SearchAndRead.
        #[serde(default, alias = "path")]
        root: String,
        #[serde(default)]
        pattern: String,
        #[serde(default)]
        patterns: Vec<String>,
        #[serde(default)]
        file_glob: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },

    // ── Network ───────────────────────────────────────────────────────────────
    /// HTTP GET. HTML comes back as `format` (markdown default, text, html), 64 KiB per call; `offset` reads on; cached 15 min.
    FetchUrl {
        url: String,
        #[serde(default)]
        headers: Option<std::collections::HashMap<String, String>>,
        /// `markdown` (default), `text` or `html`.
        #[serde(default)]
        format: Option<String>,
        /// Seconds, default 30, at most 120.
        #[serde(default)]
        timeout: Option<u64>,
        /// Byte offset into the converted text, from a previous `next_offset`.
        #[serde(default)]
        offset: Option<usize>,
    },

    /// Web search, `{title, url, snippet, source}` hits. Privacy: the query goes to Exa (mcp.exa.ai, no key sent), then Brave if keyed, then DuckDuckGo.
    ///
    /// Backends, first answer wins (`source` names it, `fallbacks` lists
    /// earlier failures): Exa's public MCP endpoint (`X0NE_EXA_DISABLE=1`
    /// skips it), Brave Search API when `X0NE_BRAVE_API_KEY` is set,
    /// DuckDuckGo HTML. See docs/web-search.md.
    WebSearch {
        query: String,
        /// Max results to return. Capped at 25 by the executor.
        #[serde(
            default = "default_search_limit",
            alias = "max_results",
            alias = "top_k"
        )]
        limit: usize,
    },

    /// Full HTTP request: any method, headers, body, returns status + headers + body.
    HttpRequest {
        url: String,
        /// "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD". Default "GET".
        #[serde(default = "default_get")]
        method: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },

    // Git status / diff primitives removed in v0.3 — `git status` / `git diff`
    // are first-class shell commands; the runtime wrappers added no value.

    // ── Process ───────────────────────────────────────────────────────────────
    /// Run a shell command (requires explicit policy allow).
    RunCommand {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// If true, start streaming mode: returns a handle instead of waiting.
        #[serde(default)]
        stream: Option<bool>,
    },

    /// Run a single command on a remote host via the local OpenSSH client.
    /// Adhoc-mode fleet admin: no x0ne-runtime install required on the
    /// target. Uses the user's `~/.ssh/config` directly (so ProxyJump,
    /// IdentityFile, ControlMaster… all just work). Stays a thin wrapper
    /// — for typed/policy-controlled remote execution, deploy x0ne-runtime
    /// to the host and use the bridge in `mode = "native"` instead.
    SshExec {
        /// SSH target as accepted by `ssh(1)`: `user@host`, an alias from
        /// `~/.ssh/config`, or just `host` (resolves via System defaults).
        host: String,
        /// The exact program to run on the remote. Single binary path or
        /// PATH-resolved name. Shell metacharacters in `command` are
        /// rejected (use `args` for parameters); to run a pipeline, set
        /// `command = "sh"`, `args = ["-c", "<your pipeline>"]`.
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Optional remote working directory (`cd <cwd> &&` is prepended
        /// inside an `sh -c` wrapper when set).
        #[serde(default)]
        cwd: Option<String>,
        /// Environment variables to set on the remote command. Exported
        /// via `KEY=value sh -c '…'`. Avoid secrets here — use
        /// `~/.ssh/environment` or an env-file the agent already pushed.
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        /// Wall-clock timeout for the entire ssh round-trip (default 30s).
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Extra `-o key=value` options injected before the target.
        /// Examples: `BatchMode=yes` (no password prompt), `StrictHostKeyChecking=accept-new`.
        #[serde(default)]
        ssh_options: Vec<String>,
    },

    /// Report the current cloud burn rate + any idle-GPU waste. Runs
    /// `gcloud compute instances list`; provider is "gcp" for now.
    CloudCost {
        provider: String,
        #[serde(default)]
        project: Option<String>,
    },

    /// Report GPU state (util, memory, temp, attached processes). `host` None
    /// = local `nvidia-smi`; Some = SSH alias run through the fleet inventory.
    GpuStatus {
        #[serde(default)]
        host: Option<String>,
    },

    /// Run arbitrary code inside an ephemeral Podman container (auto-removed after exit).
    /// The container is isolated: no network by default, read-only root fs, no host mounts.
    /// `files` are written into `/workspace` inside the container before execution.
    RunInSandbox {
        /// Container image (must be in `sandbox.podman.allowed_images`).
        image: String,
        /// Command to run inside the container.
        #[serde(default)]
        command: Vec<String>,
        /// Files to inject into /workspace inside the container. key = filename, value = content.
        #[serde(default)]
        files: std::collections::HashMap<String, String>,
        /// Extra environment variables inside the container.
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        /// Timeout in seconds (capped by config.sandbox.podman.max_timeout_secs).
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Allow network access (default: false — safest).
        #[serde(default)]
        network: Option<bool>,
        /// Memory limit in MB (capped by config.sandbox.podman.max_memory_mb).
        #[serde(default)]
        memory_mb: Option<u64>,
        /// vCPU limit (default: 1).
        #[serde(default)]
        cpu_limit: Option<u32>,
        /// Paths inside the container to pull back after execution.
        #[serde(default)]
        collect_files: Vec<String>,
    },

    /// Run multiple tasks across isolated parallel Podman containers.
    ///
    /// All containers are spawned concurrently (up to `max_parallel` or
    /// config cap), each with its own image, command, and files.
    /// Ideal for: parallel test runs, multi-env validation, batch processing.
    RunContainerGroup {
        /// Individual task definitions — one container each.
        tasks: Vec<ContainerTask>,
        /// Allow network access for all containers (default: false).
        #[serde(default)]
        network: Option<bool>,
        /// Memory limit per container in MB.
        #[serde(default)]
        memory_mb: Option<u64>,
        /// Timeout per container in seconds.
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Max containers to run in parallel (capped by config.sandbox.podman.max_parallel_instances).
        #[serde(default)]
        max_parallel: Option<usize>,
    },

    // ── Incus (LXC) — moved to orchestrator ──────────────────────────────────
    //
    // RunIncus, RunIncusGroup, and the four IncusCreate/Restore/List/Delete
    // Snapshot variants now live in x0ne-orchestrator/src/incus_actions.rs.
    // Container management is structurally outside the runtime's privilege
    // boundary: the runtime may itself run *inside* the Incus container it
    // would otherwise be asked to manage. Routing these orchestrator-side
    // makes the privilege boundary a property of the code, not of policy.

    // ── Logs ─────────────────────────────────────────────────────────────────
    /// Read the tail of a log file.
    ReadLogs {
        path: String,
        #[serde(default)]
        lines: Option<usize>,
    },

    // ── Human interaction ─────────────────────────────────────────────────────
    /// Ask the human operator a freeform question; returns their typed answer.
    ///
    /// The field accepts `question` (canonical), `prompt` or `message` —
    /// LLMs routinely pick the wrong one; aliasing costs nothing.
    AskHuman {
        #[serde(alias = "prompt", alias = "message")]
        question: String,
    },

    /// Ask the human operator for a yes/no confirmation.
    Confirm { message: String },

    /// Ask the human operator a multiple-choice question (QCM).
    ///
    /// The operator picks one of `choices` by its 1-based number.
    /// When `allow_skip` is true, an extra "Skip" option is offered and the
    /// returned `ChoiceAnswer` will have `skipped=true` with no index/value.
    /// When `allow_other` is true, an extra "Other (specify)" option is
    /// offered: the operator then types a free-form string which lands in
    /// `answer`, with `other=true` and `choice_index=null`.
    AskMultipleChoice {
        question: String,
        /// Labelled options shown to the operator. Must contain at least one item.
        choices: Vec<String>,
        /// Offer a "Skip" escape hatch. Default: false.
        #[serde(default)]
        allow_skip: bool,
        /// Offer an "Other (specify)" free-form fallback. Default: false.
        #[serde(default)]
        allow_other: bool,
    },

    // ── Pipelines ─────────────────────────────────────────────────────────────
    /// Execute multiple actions in a single request.
    /// Sequential by default; set `parallel: true` for concurrent read-only steps.
    Pipeline {
        #[serde(deserialize_with = "deserialize_steps_with_index")]
        steps: Vec<Action>,
        /// Stop on first failed step (default true). Ignored when parallel=true.
        #[serde(default = "default_true")]
        stop_on_error: bool,
        /// If set, create a snapshot with this name before executing.
        #[serde(default)]
        snapshot_before: Option<String>,
        /// If true, execute all steps concurrently. Only safe for read-only steps.
        #[serde(default)]
        parallel: bool,
    },

    // ── Try / Assert ──────────────────────────────────────────────────────────
    /// Try: execute a sequence of actions with automatic snapshot + rollback on failure.
    /// Like a Pipeline, but with built-in safety: snapshot before, rollback if any step fails.
    Try {
        /// Actions to execute sequentially.
        #[serde(deserialize_with = "deserialize_steps_with_index")]
        actions: Vec<Action>,
        /// Whether to rollback on failure (default: true).
        #[serde(default = "default_true")]
        rollback_on_fail: bool,
        /// Optional human-readable label for the try block.
        #[serde(default)]
        label: Option<String>,
        /// Keep the snapshot even on success (default: false).
        #[serde(default)]
        keep_snapshot: bool,
    },

    /// SafeChange: transactional agent change loop.
    /// Executes change steps, then test/verification steps. If any step fails,
    /// restores the pre-change snapshot when rollback is enabled.
    SafeChange {
        /// Mutating or read-only steps that implement the change.
        #[serde(deserialize_with = "deserialize_steps_with_index")]
        steps: Vec<Action>,
        /// Verification steps, usually SafeTest / Assert / ReadFile / SearchCode.
        #[serde(default, deserialize_with = "deserialize_steps_with_index")]
        tests: Vec<Action>,
        /// Whether to rollback on any change/test failure (default: true).
        #[serde(default = "default_true")]
        rollback_on_fail: bool,
        /// Optional human-readable label.
        #[serde(default)]
        label: Option<String>,
        /// Keep the snapshot even on success (default: false).
        #[serde(default)]
        keep_snapshot: bool,
    },

    /// Conditional execution: evaluate a check, then run either `then` or
    /// `else_branch` based on the outcome. Uses the exact same `check` grammar
    /// as [`Action::Assert`] (file_exists / file_contains / port_open /
    /// command_succeeds / env_set / eq / gt / lt / not_empty) so the LLM has
    /// zero new vocabulary to learn. Both branches are `Vec<Action>` and run
    /// sequentially with stop-on-error semantics.
    ///
    /// Primary value: cut one LLM round-trip per decision. Instead of
    /// `Assert` → wait for result → pick next action, the whole "if
    /// file exists do A else do B" collapses into a single turn.
    ///
    /// Safety: the policy engine preflights both branches before ANY step
    /// executes, exactly like Pipeline/Try — so a denied action in the
    /// `else` branch blocks the whole If even when the `then` branch is
    /// clean. Nesting counts against `MAX_PIPELINE_DEPTH`.
    If {
        /// Check name. Same grammar as Assert's `check`.
        check: String,
        /// File path (for file_exists, file_contains).
        #[serde(default)]
        path: Option<String>,
        /// String value (for file_contains needle, not_empty).
        #[serde(default)]
        value: Option<Value>,
        /// Hostname (for port_open).
        #[serde(default)]
        host: Option<String>,
        /// Port number (for port_open).
        #[serde(default)]
        port: Option<u16>,
        /// Command (for command_succeeds).
        #[serde(default)]
        command: Option<String>,
        /// Command args (for command_succeeds).
        #[serde(default)]
        args: Option<Vec<String>>,
        /// Env var name (for env_set).
        #[serde(default)]
        name: Option<String>,
        /// Actual value (for eq, gt, lt).
        #[serde(default)]
        actual: Option<Value>,
        /// Expected value (for eq, gt, lt).
        #[serde(default)]
        expected: Option<Value>,
        /// Actions to run when the check is truthy. Empty = no-op.
        #[serde(default)]
        then: Vec<Action>,
        /// Actions to run when the check is falsy. Empty = no-op.
        /// `else` is a Rust keyword, so the wire name is `else` (via rename).
        #[serde(default, rename = "else")]
        else_branch: Vec<Action>,
    },

    /// Render a string template, substituting `{{tool(args)}}` placeholders
    /// with the live result of calling that X0NE action. Lets the LLM emit
    /// a single fluent sentence (or whole document) and have the runtime
    /// fill in the deterministic bits — no second LLM round-trip.
    ///
    /// Two placeholder forms are accepted, both inside `{{` `}}` delimiters:
    ///
    ///   * **Call form** — `tool_name(arg1, arg2, ...)` where `tool_name` is
    ///     snake_case and args are JSON literals. Positional binding is
    ///     resolved against a small built-in table covering ~30 of the
    ///     pure-function actions (math, string, file_info, system_info,
    ///     checksum, …). Unknown tools fall back to JSON form.
    ///
    ///   * **JSON form** — `tool_name { …params… }` where the brace block
    ///     is the same JSON object you'd put inside an `Action::<Tool>`
    ///     payload (minus the `type` field). Works for every typed Action,
    ///     no positional table required. Verbose but unambiguous.
    ///
    /// Recursion: a tool's result may itself contain `{{…}}` placeholders;
    /// they're re-rendered up to `max_depth` passes (default 2). Set
    /// `strict = true` to fail the whole render on the first tool error;
    /// otherwise failed placeholders are left as the raw `{{…}}` token so
    /// the caller can inspect the call log and recover.
    ///
    /// Output: `Generic { data: { rendered, calls[], passes } }` where each
    /// call records `{ tool, args, ok, value, duration_ms }`. The session
    /// JSONL captures every substitution — same audit trail as a Pipeline.
    ///
    /// Example agent payload:
    /// ```json
    /// {"type":"RenderTemplate",
    ///  "template":"There are {{str_count(\"strawberry\",\"r\")}} r in strawberry."}
    /// ```
    /// Result: `"There are 3 r in strawberry."` — one LLM call, no re-prompt.
    RenderTemplate {
        /// The template string with `{{tool(args)}}` placeholders.
        template: String,
        /// Max number of substitution passes. Default 2 — handles one
        /// level of nested templates (a tool result that itself contains
        /// `{{…}}`). Capped at 8 to bound runtime.
        #[serde(default)]
        max_depth: Option<u32>,
        /// If true, the first failed placeholder aborts the render.
        /// Default false: failures leave the raw `{{…}}` token in place.
        #[serde(default)]
        strict: Option<bool>,
    },

    /// Assert: verify a condition without side effects.
    /// Routes through existing primitives (FileInfo, ReadFile, PortCheck, etc.)
    /// and returns a boolean result.
    Assert {
        /// Check type: "file_exists", "file_contains", "port_open",
        /// "command_succeeds", "eq", "gt", "lt", "not_empty", "env_set"
        check: String,
        /// File path (for file_exists, file_contains)
        #[serde(default)]
        path: Option<String>,
        /// String value (for file_contains needle, not_empty)
        #[serde(default)]
        value: Option<Value>,
        /// Hostname (for port_open)
        #[serde(default)]
        host: Option<String>,
        /// Port number (for port_open)
        #[serde(default)]
        port: Option<u16>,
        /// Command (for command_succeeds)
        #[serde(default)]
        command: Option<String>,
        /// Command args (for command_succeeds)
        #[serde(default)]
        args: Option<Vec<String>>,
        /// Env var name (for env_set)
        #[serde(default)]
        name: Option<String>,
        /// Actual value (for eq, gt, lt)
        #[serde(default)]
        actual: Option<Value>,
        /// Expected value (for eq, gt, lt)
        #[serde(default)]
        expected: Option<Value>,
    },

    // ── Snapshots ─────────────────────────────────────────────────────────────
    /// Create a snapshot of the given file paths.
    CreateSnapshot {
        name: String,
        /// Paths to snapshot. Must be non-empty.
        paths: Vec<String>,
    },

    /// Restore all files from a named snapshot.
    RestoreSnapshot { name: String },

    /// List all available snapshots.
    ListSnapshots,

    /// Delete a named snapshot.
    DeleteSnapshot { name: String },

    // ── Handles ───────────────────────────────────────────────────────────────
    /// Subscribe to a machine event. Returns a handle.
    Subscribe {
        // `event` and `kind` accepted as aliases — common LLM
        // misnamings caught in the jsonl audit.
        #[serde(alias = "event", alias = "kind")]
        event_type: EventType,
        /// Event-type-specific configuration (see EventType variants for fields).
        #[serde(default)]
        config: Value,
    },

    /// Drain buffered events from a handle.
    CheckHandle { handle: String },

    /// Stop and cancel a running handle.
    StopHandle { handle: String },

    /// List all active and recent handles.
    ListHandles,

    /// Send text input to a running streaming process's stdin.
    ///
    /// The target handle must be a running `Process` handle created via
    /// `RunCommand` with `stream: true`. Include `\n` in `input` to submit
    /// a line (most programs read line-by-line).
    SendInput {
        /// Handle ID from a previous `RunCommand` (stream: true).
        handle: String,
        /// Text to write to the process's stdin.
        input: String,
    },

    // ── Terminals ─────────────────────────────────────────────────────────────
    /// Open a named terminal (tmux pane, or a native PTY) running a program.
    ///
    /// For interactive programs: REPLs, TUIs, `ssh`, `gdb`, prompts, servers.
    /// Policy: the same gates as `RunCommand` (allowlist, cwd, isolation),
    /// in a container too.
    TermOpen {
        /// Terminal name, `[A-Za-z0-9_-]{1,32}`, unique among open terminals.
        name: String,
        /// Program to run (argv, no shell).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// Width in columns (default 200).
        #[serde(default)]
        cols: Option<u16>,
        /// Height in rows (default 50).
        #[serde(default)]
        rows: Option<u16>,
    },

    /// Type into a terminal: literal `text`, then named `keys`.
    ///
    /// Keys: Enter, Tab, Escape, BSpace, Space, Up, Down, Left, Right, Home,
    /// End, PageUp, PageDown, Insert, Delete, F1-F12, C-a..C-z, `M-<char>`.
    TermSend {
        name: String,
        /// Sent verbatim (multi-line paste allowed).
        #[serde(default)]
        text: Option<String>,
        /// Named keys sent after `text`, in order.
        #[serde(default)]
        keys: Vec<String>,
    },

    /// Read a terminal: the visible screen, or the last `history` lines.
    TermRead {
        name: String,
        /// Lines of scrollback to include (0 = screen only).
        #[serde(default)]
        history: Option<usize>,
        /// Return only the rows changed since the last read of this terminal.
        #[serde(default)]
        diff: bool,
    },

    /// Wait until a regex appears, the screen is stable, or the program exits.
    TermWait {
        name: String,
        /// Regex searched on the screen, multi-line (`^`/`$` match each row).
        #[serde(default)]
        pattern: Option<String>,
        /// Return once the screen has not changed for this many milliseconds.
        #[serde(default)]
        stable_ms: Option<u64>,
        /// Return when the program exits, with its exit status.
        #[serde(default)]
        exit: bool,
        /// Upper bound on the wait (default 10000 ms, capped by `[term]`).
        #[serde(default)]
        timeout_ms: Option<u64>,
        /// Return only the rows changed since the last read (default true).
        #[serde(default = "default_true")]
        diff: bool,
    },

    /// Resize a terminal.
    TermResize { name: String, cols: u16, rows: u16 },

    /// Whole scrollback and screen of a terminal, plain or with ANSI colours.
    TermSnapshot {
        name: String,
        /// Keep the ANSI escape sequences (colours, attributes).
        #[serde(default)]
        ansi: bool,
    },

    /// List the open terminals.
    TermList,

    /// Close a terminal and kill what runs in it.
    TermClose { name: String },

    // ── Browse ────────────────────────────────────────────────────────────────
    /// Fetch a URL and extract readable text using a headless browser.
    ///
    /// Unlike `FetchUrl` (raw HTTP GET), this renders JavaScript and returns
    // BrowseUrl removed in v0.3 — `x0ne-browser` ships proper isolated
    // Chromium CDP control with allowlist/audit. The bare `curl|lynx`
    // fallback added no policy or scrubbing.

    // ── Reactive ──────────────────────────────────────────────────────────────

    /// Listen to a long-running command and turn its interesting lines into
    /// turns of the conversation.
    ///
    /// The difference with `WatchAndReact`, which sits right below: that one
    /// watches a FILE and runs a FIXED action, so the model is never woken
    /// and never comments. This one watches a STREAM and hands what it sees
    /// back to the model, which is the only way the operator's "listen, and
    /// tell me what matters" loop can exist.
    ///
    /// Lines are grouped into batches (`executor::listen_batch`): a batch is
    /// a turn. A batch that hit its ceiling SAYS how many lines it is not
    /// showing, and a stream that floods stops the listener rather than
    /// drowning the conversation.
    Listen {
        /// The command to run. Same shape as `RunCommand`: no shell, argv
        /// only — `;`, `|`, `>` are literal arguments (invariant 6).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Only lines matching this regex become turns. Absent means every
        /// line, which is almost never what anyone wants on a real stream.
        #[serde(default)]
        filter: Option<String>,
        /// Grouping window in milliseconds (default: 400).
        #[serde(default)]
        window_ms: Option<u64>,
        /// Lines SHOWN per batch (default: 20). Beyond it they are counted,
        /// never dropped in silence.
        #[serde(default)]
        max_lines: Option<usize>,
        /// Outlive the session. `false` (default) means the listener dies
        /// with the process that started it.
        #[serde(default)]
        persistent: bool,
    },
    /// Watch a file or directory and automatically execute an action when
    /// changes are detected. Returns a handle — poll with `CheckHandle`
    /// to see trigger results.
    ///
    /// Each change triggers the specified action (policy-checked at trigger
    /// time). Events are debounced and capped at `max_triggers`.
    WatchAndReact {
        /// Path to watch (file or directory).
        path: String,
        /// Optional glob pattern filter (e.g. "*.rs"). Only matching files trigger.
        #[serde(default)]
        pattern: Option<String>,
        /// Action to execute on each trigger.
        action: Box<Action>,
        /// Debounce interval in milliseconds (default: 500).
        #[serde(default)]
        debounce_ms: Option<u64>,
        /// Maximum number of triggers before auto-stopping (default: 50).
        #[serde(default)]
        max_triggers: Option<u32>,
        /// Watch subdirectories recursively (default: true).
        #[serde(default = "default_true")]
        recursive: bool,
    },

    // ── Capability Discovery ──────────────────────────────────────────────────
    //
    // ListActions / DescribeAction removed in v0.3 — the MCP tools/list endpoint
    // exposes the catalog directly, and `tools as filesystem` (cat /x0ne/tools/*.md)
    // is the long-term direction. PolicyCheck stays — it's the safety primitive.
    /// Check if an action would be allowed by policy (without executing it).
    PolicyCheck { action: Value },

    // ── System ────────────────────────────────────────────────────────────────
    /// Return detailed metadata for a file path.
    FileInfo { path: String },

    /// Compute a checksum of a file.
    Checksum {
        path: String,
        /// "sha256" (default) | "md5" | "sha1"
        #[serde(default)]
        algorithm: Option<String>,
    },

    /// List running processes (reads /proc on Linux).
    ListProcesses {
        /// Optional substring filter on process name.
        #[serde(default)]
        filter: Option<String>,
    },

    /// Check if a TCP port is open. `host` defaults to `127.0.0.1` since
    /// that's the common case (agent probing a server it just launched).
    PortCheck {
        #[serde(default = "default_port_check_host")]
        host: String,
        port: u16,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },

    /// Resolve a hostname to its IP addresses.
    DnsLookup {
        // `host` accepted as alias — LLMs default to `host` from
        // the rest of the stdlib (PortCheck uses `host`).
        #[serde(alias = "host")]
        hostname: String,
    },

    // SystemInfo, Whoami, Uptime, GetEnv, DiskUsage, Sleep, ReadJson, WriteJson,
    // ParseToml all removed in v0.3 — shell handles each in one line.

    // ── Memory ──────────────────────────────────────────────────────────────
    /// Persist a key/value pair in project memory (~/.x0ne/memory/).
    /// Survives between sessions. Useful for agent context, notes, preferences.
    Remember {
        key: String,
        value: Value,
        /// Optional namespace to group related memories (default: "default").
        #[serde(default)]
        namespace: Option<String>,
    },

    /// Recall a previously stored memory by exact key.
    /// Returns the stored value, or an error if not found.
    Recall {
        key: String,
        /// Optional namespace (default: "default").
        #[serde(default)]
        namespace: Option<String>,
    },

    /// Delete a memory entry by key.
    Forget {
        key: String,
        /// Optional namespace (default: "default").
        #[serde(default)]
        namespace: Option<String>,
    },

    /// List all stored memory keys in a namespace.
    ListMemory {
        /// Optional namespace (default: "default"). Use "*" for all namespaces.
        #[serde(default)]
        namespace: Option<String>,
    },

    // ── Reflection & Validation ─────────────────────────────────────────────
    /// Structured diff: compare two strings or files and return line-by-line changes.
    /// More structured than `git diff` — returns additions, deletions, and context.
    Diff {
        /// First input — either inline text or a file path.
        #[serde(alias = "from", alias = "left", alias = "old")]
        a: String,
        /// Second input — either inline text or a file path.
        #[serde(alias = "to", alias = "right", alias = "new")]
        b: String,
        /// If true, treat `a` and `b` as file paths and read their contents.
        #[serde(default)]
        files: bool,
        /// Number of context lines around changes (default: 3).
        #[serde(default)]
        context_lines: Option<usize>,
    },

    /// Simulate: dry-run an Action (or nested Pipeline / Try / SafeChange / If)
    /// without executing it. Walks the action tree, classifies each leaf
    /// (read_only / fs_write / process_spawn / network / desktop / destructive),
    /// asks the policy engine for its verdict per step, and returns a structured
    /// projection of what would happen.
    ///
    /// "What would my agent actually do?" — the missing primitive for safety
    /// loops, training datasets, and pre-commit gates. Zero mutations executed,
    /// completes in <1ms even on deep pipelines.
    Simulate {
        /// The action (often a Pipeline / SafeChange) to project.
        /// `action` accepted as alias — every LLM tested emits
        /// `{type:Simulate, action:{...}}` because that's the
        /// natural wording. Caught repeatedly in jsonl audits.
        #[serde(alias = "action")]
        target: Box<Action>,
        /// Include per-step human-readable summaries (default: true).
        #[serde(default = "default_true")]
        explain: bool,
    },

    /// Structured validation: check a value against JSON schema, regex pattern, or type.
    Validate {
        /// The value to validate (any JSON value).
        /// `input` / `data` / `subject` accepted as aliases — LLMs
        /// hit this primitive with many naming intuitions.
        #[serde(alias = "input", alias = "data", alias = "subject")]
        value: Value,
        /// Validation mode: "schema", "regex", "type", "not_empty", "range", "one_of".
        #[serde(alias = "mode", alias = "kind", alias = "type_check")]
        check: String,
        /// The expected pattern/schema/type (semantics depend on `check`):
        /// - schema: a JSON Schema object
        /// - regex: a regex pattern string
        /// - type: one of "string", "number", "boolean", "array", "object", "null"
        /// - not_empty: ignored
        /// - range: {"min": N, "max": N}
        /// - one_of: an array of allowed values
        #[serde(default)]
        expect: Option<Value>,
    },

    // ── Math/String/Comparison primitives removed in v0.3 ───────────────────
    //
    // The agent runs `python3 -c '...'`, `awk`, `sed`, `bc` etc. in its
    // sandbox for arithmetic, string ops, comparisons. Typed wrappers for
    // those operations bloated the toolbelt without adding policy or audit
    // value. Removed: Add/Sub/Mul/Div/Mod/Abs/Pow/Sqrt/Min/Max/Round/Compare/
    // Gt/Lt/Eq/Clamp/Sum/Avg (14) + StrLen/StrContains/StrCount/StrReplace/
    // StrSplit/StrUpper/StrLower/StrTrim/StrSlice (9) = 23 actions.

    // ── Crypto ────────────────────────────────────────────────────────────────
    /// Generate cryptographically random bytes.
    RandomBytes {
        /// Number of bytes to generate (max 1048576 = 1MB).
        /// `len` / `length` / `n` accepted as aliases — the count vs
        /// length naming split is the kind of thing that costs an
        /// LLM a parse-fail it shouldn't.
        #[serde(alias = "len", alias = "length", alias = "n")]
        count: usize,
        /// Output encoding: "hex" (default) or "base64".
        #[serde(default)]
        encoding: Option<String>,
    },

    // ── Desktop (Wayland/Sway) — requires desktop.enabled = true ─────────────
    /// Take a screenshot of the desktop or a region.
    Screenshot {
        /// Output name (e.g. "HDMI-A-1"). None = primary output.
        #[serde(default)]
        output: Option<String>,
        /// Geometry region "x,y widthxheight". None = full screen.
        #[serde(default)]
        region: Option<String>,
    },

    /// Get the Sway window tree as a flattened list.
    GetWindowTree,

    /// Get the currently focused window.
    GetFocusedWindow,

    /// Move the mouse pointer to an absolute position.
    MoveMouse { x: i32, y: i32 },

    /// Click a mouse button (optionally moving to x,y first).
    Click {
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
        /// "left" | "right" | "middle". Default "left".
        #[serde(default)]
        button: Option<String>,
    },

    /// Double-click (optionally moving to x,y first).
    DoubleClick {
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
    },

    /// Scroll the mouse wheel.
    Scroll {
        /// "up" | "down" | "left" | "right"
        direction: String,
        /// Number of scroll steps (default 3).
        #[serde(default)]
        amount: Option<i32>,
    },

    /// Type text at the current cursor position.
    TypeText { text: String },

    /// Press a single key by name (e.g. "Return", "Escape", "Tab").
    PressKey { key: String },

    /// Press a key combination (e.g. ["ctrl", "s"]).
    PressKeyCombination { keys: Vec<String> },

    /// Focus a window by title or app_id substring.
    FocusWindow { match_str: String },

    /// Send a desktop notification.
    SendNotification {
        title: String,
        body: String,
        /// "low" | "normal" | "critical". Default "normal".
        #[serde(default)]
        urgency: Option<String>,
    },

    /// Read the Wayland clipboard.
    GetClipboard,

    /// Write to the Wayland clipboard.
    SetClipboard { content: String },

    // ── Composite Primitives ──────────────────────────────────────────────────
    /// Define a reusable composite primitive from a pipeline of existing actions.
    DefinePrimitive {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        params: std::collections::HashMap<String, Value>,
        steps: Vec<Value>,
        #[serde(default)]
        ephemeral: bool,
    },

    /// Call a previously defined composite primitive.
    CallPrimitive {
        name: String,
        #[serde(default)]
        args: std::collections::HashMap<String, Value>,
    },

    /// List all defined composite primitives.
    ListCustomPrimitives,

    /// Delete a composite primitive.
    DeletePrimitive { name: String },

    // ── Scripted Functions (user-defined "scripted primitives") ──────────────
    //
    // Companion to the composite-primitive family above. Where composites
    // glue typed actions into a parameterised pipeline, scripted functions
    // run real code (Rhai today, WASM tomorrow) — fills the gap for
    // "transform this JSON / extract that field / compute this metric"
    // operations the agent invents on the fly. Each function is persisted
    // to `~/.x0ne/functions/` and (via the orchestrator) promoted to a
    // first-class MCP tool other agents can call.
    /// Define a scripted function. Validates the source (compile-only) and
    /// stores it under `~/.x0ne/functions/registry.json`.
    DefineFunction {
        name: String,
        #[serde(default)]
        description: Option<String>,
        /// `"rhai"` (default) or `"wasm"` (Tier 2 — not yet implemented).
        #[serde(default)]
        tier: Option<String>,
        /// Source code for the function. Must define `fn run(<params...>)`
        /// at the top level (Rhai). Compiled on registration to fail fast.
        /// `body` and `code` accepted as aliases — LLMs naturally emit
        /// either depending on language training. The deserializer
        /// ALSO accepts an array of strings (one per line), joining
        /// them with `\n` — the jsonl audit caught the LLM doing
        /// `source: ["fn run(x) { x*2 }"]` because that's how it
        /// renders multi-line code in its chain-of-thought.
        #[serde(
            alias = "body",
            alias = "code",
            deserialize_with = "deser_string_or_lines"
        )]
        source: String,
        /// Ordered parameter list. Required for documentation + the MCP
        /// tool schema generated by the orchestrator gateway.
        #[serde(default)]
        params: Vec<Value>,
        /// Optional return-type hint ("string", "number", "object", "array",
        /// "boolean", "any"). Doc-only for now.
        #[serde(default)]
        returns: Option<String>,
    },

    /// Invoke a previously registered scripted function. Args are passed
    /// positionally in the order declared in `params`.
    CallFunction {
        name: String,
        #[serde(default)]
        args: Vec<Value>,
    },

    /// List every defined scripted function.
    ListFunctions,

    /// Remove a scripted function by name.
    DeleteFunction { name: String },

    /// Preview one asm-ai program. X0NE compiles it, derives the files it can
    /// read and write from the compiled module, proves those paths against the
    /// policy one at a time, runs it in a buffered world and returns a diff.
    /// Nothing is written. The diff is what a human then approves.
    PreviewProgram {
        /// asm-ai source, starting with the `#!asm-ai/v0` shebang.
        #[serde(alias = "program", deserialize_with = "deser_string_or_lines")]
        source: String,
    },

    /// Apply the program that was previewed. The runtime commits the
    /// transaction the preview left pending; it does not re-run the module,
    /// because re-running it would make the approved diff a lie.
    RunProgram {
        /// asm-ai source, starting with the `#!asm-ai/v0` shebang.
        #[serde(alias = "program", deserialize_with = "deser_string_or_lines")]
        source: String,
    },

    // ── ML / LLM ─────────────────────────────────────────────────────────────
    /// Call an LLM endpoint (Ollama or OpenAI-compatible) and return the response.
    LlmCall {
        /// Endpoint URL (e.g. "http://localhost:11434" for Ollama)
        endpoint: String,
        /// Model name (e.g. "mistral", "gpt-4")
        model: String,
        /// User prompt
        prompt: String,
        /// Optional system prompt
        #[serde(default)]
        system: Option<String>,
        /// Temperature (0.0-2.0, default 0.7)
        #[serde(default)]
        temperature: Option<f64>,
        /// Max tokens to generate
        #[serde(default)]
        max_tokens: Option<u64>,
        /// Optional API token. Two forms accepted:
        ///   "$ENV:ANTHROPIC_API_KEY"  → resolved from the runtime's env (preferred,
        ///                               keeps the secret out of trajectory JSONL)
        ///   "sk-…"                    → literal token (will appear in logs; only
        ///                               use for ephemeral test calls)
        /// Checked against [`model_keys::KEYS`] before any request: a vendor's
        /// variable (`$ENV:ANTHROPIC_API_KEY`, ...) goes to that vendor's hosts
        /// over https only, a key holding `sk-ant-` to api.anthropic.com only.
        /// Wire format auto-detected from endpoint: Anthropic hosts get
        /// `x-api-key` + `anthropic-version`; everything else gets
        /// `Authorization: Bearer <token>`.
        #[serde(default)]
        auth: Option<String>,
    },

    /// Manage Ollama models (list, pull, create, delete, show).
    OllamaApi {
        /// Ollama endpoint URL (e.g. "http://localhost:11434")
        endpoint: String,
        /// Operation: "list", "pull", "create", "delete", "show"
        operation: String,
        /// Model name (required for pull/create/delete/show)
        #[serde(default)]
        model: Option<String>,
        /// Modelfile content (required for create)
        #[serde(default)]
        modelfile: Option<String>,
        /// Optional API token (see `LlmCall.auth` for syntax) — needed when
        /// pointing at managed Ollama proxies (ollama.com, openrouter…).
        #[serde(default)]
        auth: Option<String>,
    },

    /// Generate a JSONL training dataset using an LLM.
    DatasetGenerate {
        /// LLM endpoint to use for generation
        endpoint: String,
        /// Model to use for generation
        model: String,
        /// Topic / description for the training data
        topic: String,
        /// Number of examples to generate (max 1000)
        #[serde(default = "default_dataset_count")]
        count: usize,
        /// Output JSONL file path
        output: String,
        /// Style: "instruction-response", "chat", "completion"
        #[serde(default)]
        style: Option<String>,
    },

    /// Extract text from a document (PDF, video, audio, image) and optionally
    /// convert it to a JSONL training dataset via LLM.
    DocumentExtract {
        /// Input file path (PDF, mp4, mp3, png, txt, etc.)
        input: String,
        /// Output file path (.txt for raw, .jsonl for dataset)
        output: String,
        /// "raw" (text extraction only) or "dataset" (convert to JSONL via LLM)
        #[serde(default = "default_raw")]
        format: String,
        /// LLM endpoint (required when format = "dataset")
        #[serde(default)]
        llm_endpoint: Option<String>,
        /// LLM model (required when format = "dataset")
        #[serde(default)]
        llm_model: Option<String>,
        /// Chunk size in characters for dataset conversion (default 2000)
        #[serde(default)]
        chunk_size: Option<usize>,
    },

    /// Multi-turn chat with Ollama models, with optional vision (images).
    ///
    /// Unlike `LlmCall` (single prompt/response), this supports full
    /// conversation history and image attachments for multimodal models
    /// (llava, bakllava, etc.).
    OllamaChat {
        /// Ollama endpoint URL (e.g. "http://localhost:11434")
        endpoint: String,
        /// Model name (e.g. "llava", "mistral", "codellama")
        model: String,
        /// Chat messages with optional images per message
        messages: Vec<ChatMessage>,
        /// Temperature (0.0-2.0)
        #[serde(default)]
        temperature: Option<f64>,
        /// Max tokens to generate
        #[serde(default)]
        max_tokens: Option<u64>,
        /// Optional API token (see `LlmCall.auth` for syntax).
        #[serde(default)]
        auth: Option<String>,
    },

    // ── Artifact (web artifacts served via x0ne-artifact-proxy) ──────────────────
    /// Allocate a fresh artifact — a self-contained directory served at
    /// `http://localhost:7800/a/<id>/`. Returns the artifact id, on-disk root
    /// path (use it as the prefix for subsequent `WriteFile` calls), and URL.
    /// Auto-launches the artifact proxy daemon if not already running.
    ArtifactCreate {
        /// Optional human-friendly name shown in `/artifact` listings.
        #[serde(default)]
        name: Option<String>,
        /// `static` (default) = served as plain files, or `service` =
        /// reverse-proxied to a long-running process started via `ArtifactServe`.
        #[serde(default)]
        kind: Option<String>,
        /// If `true`, save immediately into the persistent store instead of
        /// the per-session ephemeral one (default `false`).
        #[serde(default)]
        persistent: Option<bool>,
        /// Optional session id used to scope ephemeral storage; falls back
        /// to env `X0NE_SESSION_ID` then `"orphan"`.
        #[serde(default)]
        session_id: Option<String>,
    },

    /// List every known artifact (ephemeral + saved + running) with state, age,
    /// size and URL — same payload as the `/artifact` slash command in the CLI.
    ArtifactList,

    /// Attach a long-running service to a `Service` artifact and start it.
    /// The proxy will reverse-proxy `/a/<id>/` to the chosen port.
    ArtifactServe {
        id: String,
        /// Command launched in the artifact root (e.g.
        /// `python3 -m http.server $PORT` or `node server.js`).
        /// `$PORT` is substituted with the auto-allocated port.
        ///
        /// This is an argv, not a shell line: it is split on whitespace and
        /// executed directly. Shell metacharacters (`; & | $ ` ( ) < > " ' \`)
        /// are refused rather than escaped — see [`crate::serve_command`].
        command: String,
        /// Override the auto-allocated port (otherwise picked in 7810-7899).
        #[serde(default)]
        port: Option<u16>,
        /// Extra environment variables.
        #[serde(default)]
        env: HashMap<String, String>,
    },

    /// Promote an ephemeral artifact to the persistent (`saved/`) store. The
    /// files are moved on disk and the registry is updated atomically.
    ArtifactSave {
        id: String,
        /// Optional renamed value.
        #[serde(default)]
        name: Option<String>,
    },

    /// Stop the backing service of a `Service` artifact without deleting files.
    /// No-op for `Static` artifacts.
    ArtifactStop { id: String },

    /// Permanently remove a artifact: stop its service, delete its files, drop
    /// it from the registry. Use `force: true` to bypass the persistent guard.
    ArtifactDestroy {
        id: String,
        #[serde(default)]
        force: Option<bool>,
    },

    /// Enumerate the runtime's action vocabulary.
    ///
    /// The catalog is derived from this enum, so what the model is told it can
    /// emit is exactly what the deserializer accepts. This variant was removed
    /// once and the removal broke discovery silently: every caller kept asking
    /// and kept getting `unknown variant`, so the orchestrator fell back to a
    /// hand-written list that had drifted years out of date.
    ListActions,

    /// Describe one action: its summary, its fields, and its side-effect class.
    DescribeAction {
        /// Wire name of the action to describe.
        #[serde(alias = "action_type", alias = "type_name")]
        name: String,
    },

    /// Orchestrator-owned UI action — accepted here ONLY so it can
    /// live inside Pipeline/Try/If/SafeChange steps without the
    /// runtime rejecting it as an unknown variant. The runtime
    /// executor is a stub: it echoes the payload in the result so
    /// the orchestrator can intercept it post-execution and emit
    /// the matching BadgeUpdate event to tui.
    PaintBadge {
        /// Free-form payload — the full PaintBadge body (rows,
        /// styles, frames, delay_ms, loop, …). We don't model it
        /// here because the orchestrator's `tui::badge` module
        /// owns the schema; serde just forwards the JSON across.
        #[serde(flatten)]
        payload: Value,
    },
}

/// A single message in a multi-turn chat, with optional image attachments.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    /// Role: "system", "user", or "assistant"
    pub role: String,
    /// Text content
    pub content: String,
    /// Optional base64-encoded images (for vision models like llava)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

fn default_dataset_count() -> usize {
    50
}

fn default_raw() -> String {
    "raw".into()
}

fn default_true() -> bool {
    true
}

fn default_search_limit() -> usize {
    10
}

fn default_get() -> String {
    "GET".into()
}

/// A single editing operation within EditFile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    /// The exact text to replace. Also accepted as `old_str` — models trained
    /// on other agent toolkits reach for that name by reflex, and a rejected
    /// edit costs a whole turn to rediscover (field report, 2026-08-12).
    /// Journals of 2026-08-16 and 2026-08-17 add `old_text` / `match`
    /// (guessed first, twice, on two different days — three attempts before
    /// the right name each time): accepted too, with `old_string` (Claude
    /// Code's Edit tool) for the same reason.
    #[serde(
        alias = "old_str",
        alias = "oldText",
        alias = "old_text",
        alias = "old_string",
        alias = "match"
    )]
    pub old: String,
    /// The replacement. Also accepted as `new_str` / `newText` / `new_text` /
    /// `new_string` / `replace`.
    #[serde(
        alias = "new_str",
        alias = "newText",
        alias = "new_text",
        alias = "new_string",
        alias = "replace"
    )]
    pub new: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

/// A single exact replacement for ReplaceMany.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceManyEdit {
    pub path: String,
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: Option<bool>,
}

/// One file patch inside PatchSet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSetItem {
    pub path: String,
    pub patch: String,
    #[serde(default)]
    pub reverse: Option<bool>,
}

/// Event type for the Subscribe action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventType {
    /// Watch a file or directory for modifications.
    FileChanged {
        path: String,
        #[serde(default)]
        recursive: bool,
    },
    /// Watch a directory for new files matching an optional glob pattern.
    FileCreated {
        dir: String,
        #[serde(default)]
        pattern: Option<String>,
    },
    /// Wait for a process (by PID) to exit.
    ProcessExited { pid: u32 },
    /// Poll a TCP host:port until it becomes reachable.
    PortOpen { host: String, port: u16 },
}

impl Action {
    /// Returns the action's type name as a string (matches the JSON `type` field).
    pub fn type_name(&self) -> &'static str {
        match self {
            // Filesystem
            Action::ReadFile { .. } => "ReadFile",
            Action::ReadFiles { .. } => "ReadFiles",
            Action::WriteFile { .. } => "WriteFile",
            Action::EditFile { .. } => "EditFile",
            Action::ApplyPatch { .. } => "ApplyPatch",
            Action::StatFiles { .. } => "StatFiles",
            Action::PatchSet { .. } => "PatchSet",
            Action::SearchAndRead { .. } => "SearchAndRead",
            Action::SafeTest { .. } => "SafeTest",
            Action::MoveToTrash { .. } => "MoveToTrash",
            // Search
            Action::SearchCode { .. } => "SearchCode",
            // Network
            Action::FetchUrl { .. } => "FetchUrl",
            Action::HttpRequest { .. } => "HttpRequest",
            Action::WebSearch { .. } => "WebSearch",
            // Process
            Action::RunCommand { .. } => "RunCommand",
            Action::SshExec { .. } => "SshExec",
            Action::CloudCost { .. } => "CloudCost",
            Action::GpuStatus { .. } => "GpuStatus",
            // Logs
            Action::ReadLogs { .. } => "ReadLogs",
            // Human
            Action::AskHuman { .. } => "AskHuman",
            Action::Confirm { .. } => "Confirm",
            Action::AskMultipleChoice { .. } => "AskMultipleChoice",
            // Pipelines
            Action::Pipeline { .. } => "Pipeline",
            // Try / Assert / If
            Action::Try { .. } => "Try",
            Action::SafeChange { .. } => "SafeChange",
            Action::Assert { .. } => "Assert",
            Action::If { .. } => "If",
            // Snapshots
            Action::CreateSnapshot { .. } => "CreateSnapshot",
            Action::RestoreSnapshot { .. } => "RestoreSnapshot",
            Action::ListSnapshots => "ListSnapshots",
            Action::DeleteSnapshot { .. } => "DeleteSnapshot",
            // Handles
            Action::Subscribe { .. } => "Subscribe",
            Action::CheckHandle { .. } => "CheckHandle",
            Action::StopHandle { .. } => "StopHandle",
            Action::ListHandles => "ListHandles",
            // Meta
            Action::PolicyCheck { .. } => "PolicyCheck",
            // System
            Action::FileInfo { .. } => "FileInfo",
            Action::Checksum { .. } => "Checksum",
            Action::ListProcesses { .. } => "ListProcesses",
            Action::PortCheck { .. } => "PortCheck",
            Action::DnsLookup { .. } => "DnsLookup",
            // Memory
            Action::Remember { .. } => "Remember",
            Action::Recall { .. } => "Recall",
            Action::Forget { .. } => "Forget",
            Action::ListMemory { .. } => "ListMemory",
            // Reflection & Validation
            Action::Diff { .. } => "Diff",
            Action::Validate { .. } => "Validate",
            Action::Simulate { .. } => "Simulate",
            // Math/String/Comparison primitives removed in v0.3 (use shell)
            // Crypto
            Action::RandomBytes { .. } => "RandomBytes",
            // Desktop
            Action::Screenshot { .. } => "Screenshot",
            Action::GetWindowTree => "GetWindowTree",
            Action::GetFocusedWindow => "GetFocusedWindow",
            Action::MoveMouse { .. } => "MoveMouse",
            Action::Click { .. } => "Click",
            Action::DoubleClick { .. } => "DoubleClick",
            Action::Scroll { .. } => "Scroll",
            Action::TypeText { .. } => "TypeText",
            Action::PressKey { .. } => "PressKey",
            Action::PressKeyCombination { .. } => "PressKeyCombination",
            Action::FocusWindow { .. } => "FocusWindow",
            Action::SendNotification { .. } => "SendNotification",
            Action::GetClipboard => "GetClipboard",
            Action::SetClipboard { .. } => "SetClipboard",
            Action::RunInSandbox { .. } => "RunInSandbox",
            Action::RunContainerGroup { .. } => "RunContainerGroup",
            // Interactive / Reactive
            Action::SendInput { .. } => "SendInput",
            Action::Listen { .. } => "Listen",
            // Terminals
            Action::TermOpen { .. } => "TermOpen",
            Action::TermSend { .. } => "TermSend",
            Action::TermRead { .. } => "TermRead",
            Action::TermWait { .. } => "TermWait",
            Action::TermResize { .. } => "TermResize",
            Action::TermSnapshot { .. } => "TermSnapshot",
            Action::TermList => "TermList",
            Action::TermClose { .. } => "TermClose",
            Action::WatchAndReact { .. } => "WatchAndReact",
            // Composite
            Action::DefinePrimitive { .. } => "DefinePrimitive",
            Action::CallPrimitive { .. } => "CallPrimitive",
            Action::ListCustomPrimitives => "ListCustomPrimitives",
            Action::DeletePrimitive { .. } => "DeletePrimitive",
            Action::DefineFunction { .. } => "DefineFunction",
            Action::CallFunction { .. } => "CallFunction",
            Action::ListFunctions => "ListFunctions",
            Action::DeleteFunction { .. } => "DeleteFunction",
            Action::PreviewProgram { .. } => "PreviewProgram",
            Action::RunProgram { .. } => "RunProgram",
            // ML
            Action::LlmCall { .. } => "LlmCall",
            Action::OllamaApi { .. } => "OllamaApi",
            Action::DatasetGenerate { .. } => "DatasetGenerate",
            Action::DocumentExtract { .. } => "DocumentExtract",
            Action::OllamaChat { .. } => "OllamaChat",
            // Artifact
            Action::ArtifactCreate { .. } => "ArtifactCreate",
            Action::ArtifactList => "ArtifactList",
            Action::ArtifactServe { .. } => "ArtifactServe",
            Action::ArtifactSave { .. } => "ArtifactSave",
            Action::ArtifactStop { .. } => "ArtifactStop",
            Action::ArtifactDestroy { .. } => "ArtifactDestroy",
            Action::ListActions => "ListActions",
            Action::DescribeAction { .. } => "DescribeAction",
            Action::PaintBadge { .. } => "PaintBadge",
            // Template
            Action::RenderTemplate { .. } => "RenderTemplate",
        }
    }

    /// Side-effect class for this action — the single source of truth used by
    /// the policy engine and by `Simulate`.
    ///
    /// SECURITY: this match is deliberately **exhaustive with no `_` arm**. The
    /// pre-hardening classifier was a set of hand-maintained `matches!` lists,
    /// so any variant nobody remembered to add silently fell through to
    /// [`SideEffectClass::Meta`] — which `Simulate` reports as inert. That is
    /// how `DocumentExtract`, `DatasetGenerate` and `WebSearch` came to be
    /// described as no-ops despite spawning processes, writing files and
    /// opening sockets (August 2026 audit, finding 9).
    ///
    /// Adding a variant to `Action` now fails the build until it is classified
    /// here. When an action has several effects, return the **most dangerous**
    /// one: Destructive > ProcessSpawn > Network > FsWrite > Desktop > ReadOnly.
    pub fn side_effect_class(&self) -> crate::SideEffectClass {
        use crate::SideEffectClass as S;
        match self {
            // ── Composite: the walker recurses into these ────────────────────
            Action::Pipeline { .. }
            | Action::Try { .. }
            | Action::SafeChange { .. }
            | Action::If { .. }
            | Action::RenderTemplate { .. }
            | Action::Simulate { .. } => S::Composite,

            // ── Destructive: removes or destroys state ───────────────────────
            Action::MoveToTrash { .. }
            | Action::DeleteSnapshot { .. }
            | Action::RestoreSnapshot { .. }
            | Action::ArtifactDestroy { .. }
            | Action::ArtifactStop { .. }
            | Action::DeleteFunction { .. }
            | Action::DeletePrimitive { .. }
            | Action::Forget { .. }
            | Action::StopHandle { .. }
            // Kills the program running in the terminal.
            | Action::TermClose { .. } => S::Destructive,

            // ── Process spawn: runs a subprocess / container ─────────────────
            Action::RunCommand { .. }
            | Action::RunInSandbox { .. }
            | Action::RunContainerGroup { .. }
            | Action::SafeTest { .. }
            // Spawn gcloud / nvidia-smi — allowlist-gated like RunCommand.
            | Action::CloudCost { .. }
            | Action::GpuStatus { .. }
            | Action::WatchAndReact { .. }
            | Action::ArtifactServe { .. }
            // Shells out to pdftotext / ffmpeg / tesseract and writes `output`.
            | Action::DocumentExtract { .. }
            // SECURITY: `Meta`, "genuinely inert", until 2026-08-16 — and it
            // writes raw bytes to the stdin of a LIVE process
            // (`x0ne-runtime/src/executor/interactive.rs:14`). What that
            // process then does is unbounded: one approved
            // `RunCommand {command: "python3", args: ["-i"], stream: true}` and
            // every later `SendInput` is arbitrary execution. The effect is a
            // subprocess's, so the class is the subprocess's. Two things follow
            // from the label and both are the point: `AppConfig::defaults`
            // derives `SendInput = false` from it (config.rs, "the value comes
            // from the side-effect class"), and `touches_host` puts it behind
            // the human gate.
            | Action::SendInput { .. }

            // SECURITY: `Listen` spawns a subprocess, period. It does not
            // listen to it from outside — it STARTS it, with an argv the
            // model proposed. That its value is the observation changes
            // nothing about its effect: that effect is `RunCommand`'s, and
            // the class is therefore the same.
            //
            // Two consequences, and they are what matters:
            // `AppConfig::defaults` derives its switch from this class,
            // and `touches_host` puts it behind the human gate. An action
            // that runs a program for hours, in the background, possibly
            // beyond the session, must pass the gate at least once.
            | Action::Listen { .. }
            // SECURITY: a terminal runs a program, and every key sent to it is
            // input to that program: a shell turns `TermSend` into arbitrary
            // execution. Same class as `RunCommand`, hence the same gates.
            | Action::TermOpen { .. }
            | Action::TermSend { .. }
            | Action::TermResize { .. } => S::ProcessSpawn,

            // ── Network: opens an outbound socket ────────────────────────────
            Action::HttpRequest { .. }
            | Action::FetchUrl { .. }
            | Action::DnsLookup { .. }
            | Action::PortCheck { .. }
            | Action::SshExec { .. }
            | Action::LlmCall { .. }
            | Action::OllamaApi { .. }
            | Action::OllamaChat { .. }
            | Action::WebSearch { .. }
            // Loops LLM calls against `endpoint`, then writes `output`.
            | Action::DatasetGenerate { .. } => S::Network,

            // ── Desktop: drives the host input/display ───────────────────────
            Action::Screenshot { .. }
            | Action::GetWindowTree
            | Action::GetFocusedWindow
            | Action::MoveMouse { .. }
            | Action::Click { .. }
            | Action::DoubleClick { .. }
            | Action::Scroll { .. }
            | Action::TypeText { .. }
            | Action::PressKey { .. }
            | Action::PressKeyCombination { .. }
            | Action::FocusWindow { .. }
            | Action::SendNotification { .. }
            | Action::GetClipboard
            | Action::SetClipboard { .. } => S::Desktop,

            // ── Filesystem writes ────────────────────────────────────────────
            Action::WriteFile { .. }
            | Action::EditFile { .. }
            | Action::ApplyPatch { .. }
            | Action::PatchSet { .. }
            | Action::CreateSnapshot { .. }
            | Action::ArtifactCreate { .. }
            | Action::ArtifactSave { .. }
            | Action::DefinePrimitive { .. }
            | Action::DefineFunction { .. }
            // SECURITY: FsWrite without consulting any registry. The class is a
            // pure function of the enum: it cannot lie because a module was
            // judged harmless elsewhere. The program is not read here yet —
            // the verifier will read it — so the class states what the
            // action *can* do, not what this one will do.
            | Action::RunProgram { .. }
            | Action::Remember { .. } => S::FsWrite,

            // ── Pure reads ───────────────────────────────────────────────────
            Action::ReadFile { .. }
            | Action::ReadFiles { .. }
            | Action::StatFiles { .. }
            | Action::SearchAndRead { .. }
            | Action::SearchCode { .. }
            | Action::ReadLogs { .. }
            | Action::FileInfo { .. }
            | Action::Checksum { .. }
            | Action::ListSnapshots
            | Action::ListProcesses { .. }
            | Action::ListMemory { .. }
            | Action::Recall { .. }
            | Action::Diff { .. }
            | Action::ArtifactList
            // SECURITY: ReadOnly because a preview cannot write anything —
            // `fs_edit_buffered` keeps its results in host buffers and the
            // commit is another action. What this class buys is not the
            // absence of a gate (there is nothing to approve): it is that
            // `PreviewProgram` is a **named** action, hence subject to the
            // per-action switch (policy.rs:165-172) and to the modes. Routing
            // it through `Simulate` — which is in ALWAYS_ALLOW, which exempts
            // from the WHOLE gate (policy.rs:140-142) — would have produced a
            // disk read the operator cannot turn off.
            | Action::PreviewProgram { .. }
            // Read the screen of a terminal this runtime opened. Switchable,
            // allowed in `readonly`: what they see was gated at `TermOpen`.
            | Action::TermRead { .. }
            | Action::TermWait { .. }
            | Action::TermSnapshot { .. }
            | Action::TermList
            | Action::Assert { .. } => S::ReadOnly,

            // ── Meta / introspection / control-flow leaves ───────────────────
            // Genuinely inert: no I/O beyond in-process state or the operator
            // prompt. Keep this arm explicit so new variants cannot join it by
            // accident.
            Action::AskHuman { .. }
            | Action::Confirm { .. }
            | Action::AskMultipleChoice { .. }
            | Action::Validate { .. }
            | Action::RandomBytes { .. }
            | Action::Subscribe { .. }
            | Action::CheckHandle { .. }
            | Action::ListHandles
            | Action::PolicyCheck { .. }
            | Action::CallPrimitive { .. }
            | Action::ListCustomPrimitives
            | Action::CallFunction { .. }
            | Action::ListFunctions
            | Action::ListActions
            | Action::DescribeAction { .. }
            | Action::PaintBadge { .. } => S::Meta,
        }
    }

    /// Returns true if this is a Pipeline action (used for nesting checks).
    pub fn is_pipeline(&self) -> bool {
        matches!(
            self,
            Action::Pipeline { .. }
                | Action::Try { .. }
                | Action::SafeChange { .. }
                | Action::RenderTemplate { .. }
        )
    }
}

// ── Serde default helpers ─────────────────────────────────────────────────────

/// Default host for `PortCheck`: loopback. Covers the 95%-case of an agent
/// probing a service it just launched inside the sandbox.
/// Deserialize a String OR a `Vec<String>`. Arrays are joined with
/// `\n` — useful for action fields that expect a multi-line code
/// body (e.g. `DefineFunction.source`) which the LLM may emit as
/// either one big string OR an array of lines depending on how it
/// formatted the JSON internally.
fn deser_string_or_lines<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Deserialize, Error};
    let v = Value::deserialize(d)?;
    match v {
        Value::String(s) => Ok(s),
        Value::Array(arr) => {
            let mut lines = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    Value::String(s) => lines.push(s),
                    other => {
                        return Err(D::Error::custom(format!(
                            "expected string in array, found {}",
                            other
                        )))
                    }
                }
            }
            Ok(lines.join("\n"))
        }
        other => Err(D::Error::custom(format!(
            "expected string or array of strings, found {}",
            other
        ))),
    }
}

fn default_port_check_host() -> String {
    "127.0.0.1".to_string()
}

// ── Supporting types ──────────────────────────────────────────────────────────

// IncusTask moved to x0ne-orchestrator (handled as raw JSON now — no shared
// type plumbing needed since the orchestrator never imports x0ne-runtime).

/// A single task in a `RunContainerGroup` — one Podman container per task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerTask {
    /// Unique identifier for this task (used in the result list).
    pub id: String,
    /// OCI image to use (must be in `sandbox.podman.allowed_images`).
    pub image: String,
    /// Command to run inside the container.
    #[serde(default)]
    pub command: Vec<String>,
    /// Files to inject into /workspace (relative paths only).
    #[serde(default)]
    pub files: HashMap<String, String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Custom deserializer for Pipeline/Try step arrays.
/// Parses each step individually so that on failure, the error message
/// includes the step index and the step's `type` field. Without this,
/// serde reports a generic "missing field X" with no indication of
/// which of 18 steps has the wrong schema.
fn deserialize_steps_with_index<'de, D>(deserializer: D) -> Result<Vec<Action>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw_steps: Vec<Value> = Vec::deserialize(deserializer)?;
    let mut actions = Vec::with_capacity(raw_steps.len());
    for (i, raw) in raw_steps.iter().enumerate() {
        match serde_json::from_value::<Action>(raw.clone()) {
            Ok(action) => actions.push(action),
            Err(e) => {
                let step_type = raw
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Err(serde::de::Error::custom(format!(
                    "Failed to parse step {} ({}): {}",
                    i, step_type, e
                )));
            }
        }
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the load-bearing invariant ─────────────────────────────────────────

    /// Every variant must carry a stated side-effect class.
    ///
    /// This is enforced by the compiler — `side_effect_class()` matches with no
    /// wildcard arm, so a new variant fails the build until classified. The
    /// test guards the *other* half: that nothing was quietly parked in `Meta`
    /// to silence that error. `Meta` means "no I/O beyond in-process state or
    /// the operator prompt", and `Simulate` reports it as inert — an action
    /// that touches disk, network or the desktop must never land there.

    // ── derived catalog ────────────────────────────────────────────────────

    /// The generated catalog and the deserializer must describe the same
    /// vocabulary. serde is what actually enforces it at the boundary, so if
    /// the build-time parser ever drifts from the enum — a syntax it does not
    /// handle, a variant it skips — every derived catalog silently shrinks and
    /// nothing else would notice. This is the check that makes generation
    /// safe to depend on.
    #[test]
    fn generated_catalog_matches_what_serde_enforces() {
        let mut generated: Vec<String> = all_type_names().iter().map(|s| s.to_string()).collect();
        let mut reported = serde_reported_type_names();
        generated.sort();
        reported.sort();
        assert_eq!(
            generated, reported,
            "the build-time catalog disagrees with serde about the Action vocabulary"
        );
    }

    /// Sanity on the catalog's contents, independent of how it was built.
    #[test]
    fn catalog_is_complete_and_free_of_removed_names() {
        let names = all_type_names();
        assert!(names.len() > 50, "got {} entries: {names:?}", names.len());
        for expected in [
            "ReadFile",
            "WriteFile",
            "PatchSet",
            "Pipeline",
            "SafeChange",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} missing from the catalog"
            );
        }
        for dead in [
            "ListDir",
            "AppendFile",
            "SearchReplace",
            "CreateDir",
            "InsertAtLine",
        ] {
            assert!(
                !names.contains(&dead),
                "{dead} was removed but is still listed"
            );
        }
        for name in names {
            assert!(
                !name.contains(',') && !name.contains(' ') && !name.is_empty(),
                "malformed catalog entry {name:?}"
            );
        }
    }

    /// Metadata has to be usable, not merely present: discovery describes
    /// actions to the model from these fields.
    #[test]
    fn generated_metadata_carries_summaries_and_fields() {
        let read_file = action_meta("ReadFile").expect("ReadFile is in the catalog");
        assert!(
            read_file.required_fields.contains(&"path"),
            "ReadFile's required `path` field was not captured: {read_file:?}"
        );
        assert!(
            read_file.optional_fields.contains(&"offset"),
            "ReadFile's defaulted `offset` field should be optional: {read_file:?}"
        );
        let documented = GENERATED_ACTIONS
            .iter()
            .filter(|m| !m.summary.is_empty())
            .count();
        assert!(
            documented * 2 > GENERATED_ACTIONS.len(),
            "only {documented}/{} actions carry a summary — discovery would be \
             mostly blank",
            GENERATED_ACTIONS.len()
        );
    }

    /// Every derived name must round-trip back into the enum, which is what
    /// makes the catalog safe to hand to consumers as ground truth.
    #[test]
    fn every_derived_name_parses_back_into_an_action() {
        for name in all_type_names() {
            let err = serde_json::from_value::<Action>(serde_json::json!({"type": name}))
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                !err.contains("unknown variant"),
                "{name} came from the catalog but is not a known variant"
            );
        }
    }

    // ── write-path extraction (snapshot targeting) ─────────────────────────

    /// `PatchSet` writes several paths at once. The orchestrator's old
    /// hand-maintained string list omitted it entirely, so a PatchSet inside a
    /// Try ran with nothing snapshotted.
    #[test]
    fn write_paths_covers_multi_path_writes() {
        let a = Action::PatchSet {
            patches: vec![
                PatchSetItem {
                    path: "/tmp/a".into(),
                    patch: String::new(),
                    reverse: None,
                },
                PatchSetItem {
                    path: "/tmp/b".into(),
                    patch: String::new(),
                    reverse: None,
                },
            ],
            stop_on_error: None,
        };
        assert_eq!(
            write_paths(&[a]),
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
    }

    /// Composites must be traversed: a Try wrapping a Pipeline of writes has
    /// to snapshot those writes, or the rollback guarantee is silently void.
    #[test]
    fn write_paths_traverses_composites() {
        let inner = Action::Pipeline {
            steps: vec![Action::WriteFile {
                path: "/tmp/nested".into(),
                content: String::new(),
            }],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let outer = Action::Try {
            actions: vec![inner],
            rollback_on_fail: true,
            label: None,
            keep_snapshot: false,
        };
        assert_eq!(write_paths(&[outer]), vec!["/tmp/nested".to_string()]);
    }

    /// Read-only actions must not be snapshotted, and duplicates collapse.
    #[test]
    fn write_paths_excludes_reads_and_dedups() {
        let acts = vec![
            Action::ReadFile {
                path: "/tmp/r".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            Action::WriteFile {
                path: "/tmp/w".into(),
                content: String::new(),
            },
            Action::WriteFile {
                path: "/tmp/w".into(),
                content: "again".into(),
            },
        ];
        assert_eq!(write_paths(&acts), vec!["/tmp/w".to_string()]);
    }

    #[test]
    fn no_effectful_action_is_classified_meta() {
        use SideEffectClass as S;
        let effectful: Vec<(&str, Action)> = vec![
            (
                "ReadFile",
                Action::ReadFile {
                    path: "/tmp/a".into(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
            ),
            (
                "WriteFile",
                Action::WriteFile {
                    path: "/tmp/a".into(),
                    content: String::new(),
                },
            ),
            (
                "FetchUrl",
                Action::FetchUrl {
                    url: "https://x".into(),
                    headers: None,
                    format: None,
                    timeout: None,
                    offset: None,
                },
            ),
            (
                "WebSearch",
                Action::WebSearch {
                    query: "q".into(),
                    limit: 1,
                },
            ),
            (
                "MoveToTrash",
                Action::MoveToTrash {
                    path: "/tmp/a".into(),
                },
            ),
            (
                "RestoreSnapshot",
                Action::RestoreSnapshot { name: "s".into() },
            ),
        ];
        for (name, a) in effectful {
            assert_ne!(
                a.side_effect_class(),
                S::Meta,
                "{name} has real side effects and must not be classified Meta"
            );
        }
    }

    /// The classes a security decision keys on must stay distinguishable in
    /// their serialized form — they cross the process boundary in Simulate
    /// output and in trace records.
    #[test]
    fn cloud_actions_are_process_spawn() {
        use SideEffectClass as S;
        let cost = Action::CloudCost {
            provider: "gcp".into(),
            project: None,
        };
        let gpu = Action::GpuStatus { host: None };
        assert_eq!(cost.side_effect_class(), S::ProcessSpawn);
        assert_eq!(gpu.side_effect_class(), S::ProcessSpawn);
        assert_eq!(cost.type_name(), "CloudCost");
        assert_eq!(gpu.type_name(), "GpuStatus");
    }

    #[test]
    fn side_effect_classes_serialize_distinctly() {
        use SideEffectClass as S;
        let all = [
            S::ReadOnly,
            S::FsWrite,
            S::ProcessSpawn,
            S::Network,
            S::Desktop,
            S::Destructive,
            S::Composite,
            S::Meta,
        ];
        let names: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
        let mut uniq = names.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            names.len(),
            "class names must be distinct: {names:?}"
        );
    }

    #[test]
    fn deserialize_read_file() {
        let json = r#"{"type":"ReadFile","path":"/tmp/foo.txt"}"#;
        let action: Action = serde_json::from_str(json).unwrap();
        assert!(matches!(action, Action::ReadFile { .. }));
    }

    #[test]
    fn deserialize_run_command() {
        let json = r#"{"type":"RunCommand","command":"cargo","args":["test"]}"#;
        let action: Action = serde_json::from_str(json).unwrap();
        assert!(matches!(action, Action::RunCommand { .. }));
    }

    #[test]
    fn type_name_round_trips() {
        let action = Action::WriteFile {
            path: "/tmp/a".into(),
            content: "hello".into(),
        };
        assert_eq!(action.type_name(), "WriteFile");
    }

    #[test]
    fn deserialize_pipeline() {
        let json = r#"{
            "type": "Pipeline",
            "steps": [
                {"type": "ReadFile", "path": "/tmp/a"},
                {"type": "ReadFile", "path": "/tmp/b"}
            ]
        }"#;
        let action: Action = serde_json::from_str(json).unwrap();
        assert!(matches!(action, Action::Pipeline { .. }));
        assert!(action.is_pipeline());
    }

    // FindFiles alias tests removed with the variant.

    #[test]
    fn port_check_defaults_host_to_loopback() {
        let json = r#"{"type":"PortCheck","port":8080}"#;
        let action: Action = serde_json::from_str(json).expect("should parse");
        match action {
            Action::PortCheck { host, port, .. } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8080);
            }
            other => panic!("expected PortCheck, got {:?}", other),
        }
    }

    #[test]
    fn ask_human_accepts_prompt_and_message_aliases() {
        for json in [
            r#"{"type":"AskHuman","question":"canonical"}"#,
            r#"{"type":"AskHuman","prompt":"alias 1"}"#,
            r#"{"type":"AskHuman","message":"alias 2"}"#,
        ] {
            let action: Action = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("failed to parse {json}: {e}"));
            assert!(
                matches!(action, Action::AskHuman { .. }),
                "expected AskHuman from {json}"
            );
        }
    }

    #[test]
    fn deserialize_subscribe() {
        let json = r#"{
            "type": "Subscribe",
            "event_type": {"kind": "file_changed", "path": "/tmp/watch"}
        }"#;
        let action: Action = serde_json::from_str(json).unwrap();
        assert!(matches!(action, Action::Subscribe { .. }));
    }

    #[test]
    fn deserialize_checksum() {
        let json = r#"{"type":"Checksum","path":"/tmp/file.dat","algorithm":"sha256"}"#;
        let action: Action = serde_json::from_str(json).unwrap();
        assert!(matches!(action, Action::Checksum { .. }));
    }

    #[test]
    fn deserialize_desktop_actions() {
        let screenshot = r#"{"type":"Screenshot"}"#;
        let action: Action = serde_json::from_str(screenshot).unwrap();
        assert_eq!(action.type_name(), "Screenshot");

        let click = r#"{"type":"Click","x":100,"y":200}"#;
        let action: Action = serde_json::from_str(click).unwrap();
        assert_eq!(action.type_name(), "Click");
    }

    // String/math alias tests removed with the Str*/math families.

    // ── Problem 2: Pipeline per-step error reporting ────────────────────────

    #[test]
    fn pipeline_step_error_includes_index_and_type() {
        // Step 1 has "Subscribe" with wrong field shape; must report index+type.
        let json = r#"{
            "type": "Pipeline",
            "steps": [
                {"type": "ReadFile", "path": "/tmp/a"},
                {"type": "Subscribe", "event_type": "<not_a_valid_event_shape>"}
            ]
        }"#;
        let err = serde_json::from_str::<Action>(json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("step 1"),
            "error should mention step index: {}",
            msg
        );
        assert!(
            msg.contains("Subscribe"),
            "error should mention step type: {}",
            msg
        );
    }

    #[test]
    fn pipeline_valid_steps_parse_fine() {
        let json = r#"{
            "type": "Pipeline",
            "steps": [
                {"type": "ReadFile", "path": "/tmp/a"},
                {"type": "ReadFile", "path": "/tmp/b"}
            ]
        }"#;
        let action: Action = serde_json::from_str(json).unwrap();
        if let Action::Pipeline { steps, .. } = action {
            assert_eq!(steps.len(), 2);
        } else {
            panic!("wrong variant");
        }
    }

    // ── Problem 3: Remember/Recall/Forget inside Pipeline ───────────────────

    #[test]
    fn pipeline_with_remember_recall_forget_parses() {
        let json = r#"{
            "type": "Pipeline",
            "steps": [
                {"type": "Remember", "key": "test_key", "value": "hello"},
                {"type": "ReadFile", "path": "/tmp/a"},
                {"type": "Recall", "key": "test_key"},
                {"type": "ReadFile", "path": "/tmp/b"},
                {"type": "Forget", "key": "test_key"}
            ]
        }"#;
        let action: Action = serde_json::from_str(json).unwrap();
        if let Action::Pipeline { steps, .. } = action {
            assert_eq!(steps.len(), 5);
            assert_eq!(steps[0].type_name(), "Remember");
            assert_eq!(steps[2].type_name(), "Recall");
            assert_eq!(steps[4].type_name(), "Forget");
        } else {
            panic!("wrong variant");
        }
    }

    // ── Problem 2 for Try blocks too ────────────────────────────────────────

    #[test]
    fn validate_accepts_input_data_subject_as_value_aliases() {
        // The LLM hits Validate with `input` / `data` / `subject`
        // depending on its mental model. All three must land in
        // `value` so the action passes.
        for alias in &["value", "input", "data", "subject"] {
            let json = format!(
                r#"{{"type":"Validate","{}":"hello","check":"not_empty"}}"#,
                alias
            );
            let action = serde_json::from_str::<Action>(&json)
                .unwrap_or_else(|e| panic!("alias `{}` failed to parse: {}", alias, e));
            match action {
                Action::Validate { value, .. } => assert_eq!(value, serde_json::json!("hello")),
                _ => panic!("wrong variant for alias `{}`", alias),
            }
        }
    }

    #[test]
    fn define_function_source_accepts_string_or_array_of_strings() {
        // String form (canonical) — must still parse.
        let canonical = serde_json::from_str::<Action>(
            r#"{"type":"DefineFunction","name":"f","source":"fn run(x) { x*2 }"}"#,
        )
        .expect("string source must parse");
        match canonical {
            Action::DefineFunction { source, .. } => {
                assert_eq!(source, "fn run(x) { x*2 }");
            }
            _ => panic!("wrong variant"),
        }
        // Array-of-strings form — the LLM-natural shape. Lines
        // join with `\n` so the Rhai parser sees a real multi-
        // line program.
        let from_array = serde_json::from_str::<Action>(
            r#"{"type":"DefineFunction","name":"f","source":["fn run(x) {","  x * 2","}"]}"#,
        )
        .expect("array source must parse");
        match from_array {
            Action::DefineFunction { source, .. } => {
                assert_eq!(source, "fn run(x) {\n  x * 2\n}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn paint_badge_passes_through_runtime_action_enum() {
        // Inside a Pipeline step (or If branch), PaintBadge must
        // parse without "unknown variant" errors. The runtime
        // accepts the variant as a pass-through; the orchestrator
        // intercepts the result and routes to BadgeUpdate.
        let payload =
            serde_json::from_str::<Action>(r#"{"type":"PaintBadge","rows":["X0NE","ready"]}"#)
                .expect("PaintBadge must parse on its own");
        match payload {
            Action::PaintBadge { payload } => {
                assert!(payload.get("rows").is_some());
            }
            _ => panic!("wrong variant"),
        }

        // And the real-world case — embedded inside a Pipeline.
        let pipe = serde_json::from_str::<Action>(
            r#"{"type":"Pipeline","steps":[
                {"type":"PaintBadge","rows":["start"]},
                {"type":"ReadFile","path":"/tmp/x"},
                {"type":"PaintBadge","rows":["done"]}
            ]}"#,
        )
        .expect("Pipeline with PaintBadge steps must parse");
        match pipe {
            Action::Pipeline { steps, .. } => {
                assert_eq!(steps.len(), 3);
                assert_eq!(steps[0].type_name(), "PaintBadge");
                assert_eq!(steps[1].type_name(), "ReadFile");
                assert_eq!(steps[2].type_name(), "PaintBadge");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn search_code_accepts_path_as_alias_of_root() {
        // The jsonl audit caught the LLM emitting `path` (the
        // generic filesystem field name) instead of `root` for
        // SearchCode — and the call silently returned 0 matches.
        // The serde alias `path` lets the action deserialize either
        // way so we never reject a search over a naming convention.
        let from_root = serde_json::from_str::<Action>(
            r#"{"type":"SearchCode","root":"/tmp","pattern":"foo"}"#,
        )
        .expect("root form must parse");
        let from_path = serde_json::from_str::<Action>(
            r#"{"type":"SearchCode","path":"/tmp","pattern":"foo"}"#,
        )
        .expect("path-aliased form must parse");

        match (from_root, from_path) {
            (Action::SearchCode { root: a, .. }, Action::SearchCode { root: b, .. }) => {
                assert_eq!(a, "/tmp");
                assert_eq!(b, "/tmp", "alias `path` must land in `root`; got {:?}", b);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn search_and_read_accepts_path_as_alias_of_root() {
        let aliased = serde_json::from_str::<Action>(
            r#"{"type":"SearchAndRead","path":"src","patterns":["TODO"]}"#,
        )
        .expect("path-aliased SearchAndRead must parse");
        match aliased {
            Action::SearchAndRead { root, .. } => assert_eq!(root, "src"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn try_step_error_includes_index_and_type() {
        let json = r#"{
            "type": "Try",
            "actions": [
                {"type": "ReadFile", "path": "/tmp/a"},
                {"type": "WriteFile"}
            ]
        }"#;
        let err = serde_json::from_str::<Action>(json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("step 1"),
            "error should mention step index: {}",
            msg
        );
        assert!(
            msg.contains("WriteFile"),
            "error should mention step type: {}",
            msg
        );
    }

    // ── asm-ai: the two program actions ─────────────────────────────────────

    /// SECURITY: `RunProgram` carries an asm-ai program that can rewrite files.
    /// Three mechanisms read this class and **none** complains if it drifts:
    /// `config::defaults()` (x0ne-runtime/src/config.rs:612-668) would ship
    /// the action enabled on a fresh install,
    /// `terminal::action_requires_confirmation`
    /// (x0ne-orchestrator/src/terminal.rs:1100-1143) would stop gating it, and
    /// the prompt would describe it as inert. This is exactly the shape of
    /// finding 9 of the August 2026 audit: `DocumentExtract`, `DatasetGenerate`
    /// and `WebSearch` announced as no-ops while they wrote, spawned processes
    /// and opened sockets.
    ///
    /// A failure here means: the human gate no longer opens in front of asm-ai.
    #[test]
    fn a_run_program_is_classified_as_a_filesystem_write() {
        let a = Action::RunProgram {
            source: "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n".into(),
        };
        assert_eq!(a.type_name(), "RunProgram");
        assert_eq!(a.side_effect_class(), SideEffectClass::FsWrite);
    }

    /// SECURITY: `PreviewProgram` is classified `ReadOnly`, and this is not a
    /// convenience — it is the review's structural decision.
    ///
    /// The preview **reads** files (it expands a pattern, opens each file,
    /// computes a diff) and writes none. `ReadOnly` says exactly that.
    /// What matters is not that it is ungated — it is that it is a **named**
    /// action, hence subject to the per-action switch of `policy.rs`.
    /// The rejected alternative was `Simulate { target: RunProgram }`: `Simulate`
    /// is in `ALWAYS_ALLOW`, which exempts from the **whole** gate, and a
    /// `read lines /./ + emit table` on `**/*` would then have become a disk
    /// reader nobody can turn off.
    ///
    /// A failure here means either a preview writes (the class would have
    /// drifted to FsWrite), or it stopped being switchable.
    #[test]
    fn a_preview_program_is_classified_as_a_pure_read() {
        let a = Action::PreviewProgram {
            source: "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n".into(),
        };
        assert_eq!(a.type_name(), "PreviewProgram");
        assert_eq!(a.side_effect_class(), SideEffectClass::ReadOnly);
    }

    /// The catalog derived at build time (`build.rs`) must see the variants AND
    /// their classes. `GENERATED_ACTIONS` is what `defaults()`, the docs and the
    /// prompt read: a variant missing from the catalog is an action nothing
    /// describes.
    ///
    /// Implementation note: the API is `action_meta` (lib.rs:2246-2248), not
    /// `describe_action` — the latter does not exist in `x0ne-actions` (there is
    /// a `describe_action` in `x0ne-runtime/src/executor/meta.rs:84` and another
    /// in `x0ne-orchestrator/src/agent/catalog.rs:109`, both `async` and with a
    /// different signature).
    #[test]
    fn the_generated_catalog_knows_both_program_actions() {
        let run = action_meta("RunProgram").expect("RunProgram is in the catalog");
        assert_eq!(run.class, "FsWrite");
        assert_eq!(run.required_fields, &["source"]);
        let preview = action_meta("PreviewProgram").expect("PreviewProgram is in the catalog");
        assert_eq!(preview.class, "ReadOnly");
        assert_eq!(preview.required_fields, &["source"]);
        assert!(all_type_names().contains(&"RunProgram"));
        assert!(all_type_names().contains(&"PreviewProgram"));
    }

    /// SECURITY: the bound on what a preview returns to the model.
    ///
    /// Without it, `from files "**/*"` + `read lines /./` + `emit table` would
    /// send the whole tree into the model's context — that is, exactly the
    /// exfiltration that rule R09 (bare `read` forbidden) closed, by another
    /// path.
    ///
    /// This test compares the constant to **no** `MAX_EMIT_BYTES`: `x0ne-asm`
    /// does not exist yet, so there is nothing to cross-check. It pins the
    /// value, and nothing more — a tripwire that forces writing the new value
    /// in two places rather than a check. **T3 wires the cross-check** with
    /// `x0ne_asm::limits::MAX_EMIT_BYTES`, in `x0ne-runtime`, the only crate
    /// that depends on both (`the_two_result_bounds_agree`).
    ///
    /// A failure here means someone changed the cap without deciding to.
    #[test]
    fn the_result_bound_is_256_kib() {
        assert_eq!(MAX_RESULT_BYTES, 256 * 1024);
    }
}

// ── Derived catalog ──────────────────────────────────────────────────────────

/// SECURITY: aggregate cap on what a program preview returns to the model and
/// to the human — `diff` + `table` + `emitted` **added together**.
///
/// Equal to `x0ne_asm::limits::MAX_EMIT_BYTES`: the channel the module controls
/// and the channel the host fills have no reason to have different caps. Lives
/// here rather than in `x0ne-asm` because `x0ne-actions` is the only
/// vocabulary **both** sides of the bridge share, and the bound describes a
/// property of the action, not of the language. The equality of the two
/// constants is checked by `the_two_result_bounds_agree` (T3, in
/// `x0ne-runtime`, the only crate that depends on both).
///
/// Beyond it, truncation is **declared** in the payload (`truncated: bool`,
/// `omitted_bytes`), never silent: a result cut without saying so is a result
/// that lies by omission.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

include!(concat!(env!("OUT_DIR"), "/action_catalog.rs"));

static ALL_TYPE_NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

/// Every `Action` variant name.
///
/// Reads the build-time catalog generated from the enum declaration itself, so
/// there is nothing to keep in sync. Callers that need to show the model what
/// it may emit, to validate configuration, or to suggest a correction for a
/// misspelled action must use this rather than keeping their own list —
/// hand-maintained copies are how the orchestrator ended up teaching the model
/// `ListDir` and `SearchReplace` long after they were removed, and how its
/// snapshot, confirmation and primitive-collision guards ended up naming
/// actions that no longer existed.
pub fn all_type_names() -> &'static [&'static str] {
    ALL_TYPE_NAMES.get_or_init(|| GENERATED_ACTIONS.iter().map(|m| m.name).collect())
}

/// The generated description of one action, by wire name.
pub fn action_meta(name: &str) -> Option<&'static ActionMeta> {
    GENERATED_ACTIONS.iter().find(|m| m.name == name)
}

/// Variant names as serde itself reports them, recovered from its
/// "unknown variant" diagnostic.
///
/// Only used to cross-check the generated catalog in tests. Discovery used to
/// depend on this at runtime, which tied the vocabulary to the wording of
/// someone else's error messages; it now exists purely so the build-time
/// parser cannot silently disagree with the deserializer that actually
/// enforces the vocabulary.
#[cfg(test)]
fn serde_reported_type_names() -> Vec<String> {
    let err = match serde_json::from_value::<Action>(serde_json::json!({"type": "\u{1}probe"})) {
        Ok(_) => return Vec::new(),
        Err(e) => e.to_string(),
    };
    err.split('`')
        .skip(3)
        .step_by(2)
        .map(|s| s.to_string())
        .collect()
}

// ── Write-path extraction ────────────────────────────────────────────────────

/// Every filesystem path an action would write, including inside composites.
///
/// This is the snapshot-targeting primitive: `Try` / `SafeChange` snapshot
/// exactly these paths before running, and restore them on failure. It lives
/// here, with the vocabulary, because BOTH sides of the process boundary need
/// the same answer — the runtime when it executes a `Try`, and the
/// orchestrator when it composes one over the bridge. Two hand-maintained
/// copies is how the previous drift happened: the orchestrator's list still
/// named `AppendFile` / `ReplaceInFile` / `InsertAtLine` / `CreateDir`, all
/// removed in v0.3, while omitting `PatchSet`, which writes several paths at
/// once. Writes therefore escaped snapshot targeting silently.
///
/// Composites are traversed: a `Try` wrapping a `Pipeline` of writes must
/// snapshot those writes. Both `If` branches are included — which one runs is
/// only known at execution time.
pub fn write_paths(actions: &[Action]) -> Vec<String> {
    let mut paths = Vec::new();
    collect_write_paths_into(actions, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_write_paths_into(actions: &[Action], out: &mut Vec<String>) {
    for action in actions {
        match action {
            Action::WriteFile { path, .. }
            | Action::EditFile { path, .. }
            | Action::ApplyPatch { path, .. }
            | Action::MoveToTrash { path } => out.push(path.clone()),
            // Multi-path write — missed entirely by the old string list.
            Action::PatchSet { patches, .. } => out.extend(patches.iter().map(|p| p.path.clone())),
            Action::SafeChange { steps, tests, .. } => {
                collect_write_paths_into(steps, out);
                collect_write_paths_into(tests, out);
            }
            Action::Pipeline { steps, .. } => collect_write_paths_into(steps, out),
            Action::Try { actions, .. } => collect_write_paths_into(actions, out),
            Action::If {
                then, else_branch, ..
            } => {
                collect_write_paths_into(then, out);
                collect_write_paths_into(else_branch, out);
            }
            _ => {}
        }
    }
}

// ── Side-effect semantics ────────────────────────────────────────────────────

/// Side-effect class assigned to a single action by `PolicyEngine::classify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    /// Pure read — no mutation, no network, no desktop. Safe to retry, cache, parallelize.
    ReadOnly,
    /// Writes/edits a file on disk.
    FsWrite,
    /// Spawns a subprocess or sandbox.
    ProcessSpawn,
    /// Opens an outbound network socket.
    Network,
    /// Drives the host desktop (Wayland/X11/RFB-style input).
    Desktop,
    /// Removes or destroys state.
    Destructive,
    /// Wraps other actions (Pipeline, Try, SafeChange, If, RenderTemplate, Simulate).
    Composite,
    /// Meta / introspection / control-flow leaves (ListActions, Sleep, …).
    Meta,
}

impl SideEffectClass {
    /// Whether an action of this class reaches beyond the process that runs
    /// it — the single definition of "touches the host".
    ///
    /// SECURITY: this is the predicate the human gate, the dispatcher's
    /// frontend-claim check and the TUI intercepts all read. It used to be
    /// written three times: `agent/dispatch.rs` and `tui/intercept.rs`
    /// counted `Network` in, `agent/terminal.rs` counted it out — and it was
    /// terminal.rs's copy that decided whether a human was asked. `SshExec`
    /// ran a remote shell and `HttpRequest DELETE` deleted, both without a
    /// word (P8b, 2026-08-16). Three copies of a security predicate are three
    /// chances to disagree; this is the one.
    ///
    /// It answers about the **class**, not about a particular action. Two
    /// callers hold a narrower, *named* exemption on top of it —
    /// `agent::readonly_cmd` for commands that only observe, and
    /// `agent::readonly_net` for network reads that carry no payload. Those
    /// waive the human prompt only; no allowlist, no path root and no
    /// per-action switch is affected by either.
    ///
    /// `Composite` is false here because a composite has no effect of its
    /// own: the caller must recurse into its children, and each of them meets
    /// this predicate on its own account.
    pub fn touches_host(self) -> bool {
        match self {
            Self::ProcessSpawn
            | Self::FsWrite
            | Self::Destructive
            | Self::Desktop
            | Self::Network => true,
            Self::ReadOnly | Self::Meta | Self::Composite => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::FsWrite => "fs_write",
            Self::ProcessSpawn => "process_spawn",
            Self::Network => "network",
            Self::Desktop => "desktop",
            Self::Destructive => "destructive",
            Self::Composite => "composite",
            Self::Meta => "meta",
        }
    }
}

/// Network-facing actions — anything that opens a socket beyond the host.
/// Standalone fn (not a method on `PolicyEngine`) so `executor::simulate`
/// can call it without needing an engine instance.
pub fn is_network_action(action: &Action) -> bool {
    // PortOpen is an EventType variant (event-driven primitives) not a
    // top-level Action — don't list it here.
    matches!(
        action,
        Action::HttpRequest { .. }
            | Action::FetchUrl { .. }
            | Action::DnsLookup { .. }
            | Action::PortCheck { .. }
            | Action::SshExec { .. }
            | Action::LlmCall { .. }
            | Action::OllamaApi { .. }
            | Action::OllamaChat { .. }
    )
}

/// Destructive actions — anything that removes or destroys state. These get
/// flagged distinctly from regular writes so a `Simulate` report can warn
/// loudly ("would delete X").
pub fn is_destructive_action(action: &Action) -> bool {
    matches!(
        action,
        Action::MoveToTrash { .. }
            | Action::DeleteSnapshot { .. }
            | Action::ArtifactDestroy { .. }
            | Action::ArtifactStop { .. }
            | Action::RestoreSnapshot { .. }
            | Action::DeleteFunction { .. }
            | Action::DeletePrimitive { .. }
            | Action::Forget { .. }
            | Action::StopHandle { .. }
    )
}
