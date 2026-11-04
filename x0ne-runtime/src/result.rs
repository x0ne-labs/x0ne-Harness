use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Structured result returned from any action execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub id: String,
    pub success: bool,
    pub action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<ActionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionError>,
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
}

impl ActionResult {
    pub fn ok(action_type: impl Into<String>, output: ActionOutput, duration_ms: u64) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            success: true,
            action_type: action_type.into(),
            output: Some(output),
            error: None,
            timestamp: Utc::now(),
            duration_ms,
        }
    }

    pub fn err(
        action_type: impl Into<String>,
        kind: ErrorKind,
        message: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        let action_type = action_type.into();
        let (class, retryable) = kind.classify();
        let code = kind.code();
        Self {
            id: Uuid::new_v4().to_string(),
            success: false,
            action_type: action_type.clone(),
            output: None,
            error: Some(ActionError {
                kind,
                class,
                code: code.to_string(),
                message: message.into(),
                action_name: action_type,
                retryable,
                recommended_next_step: class.recommended_next_step().to_string(),
                liftable: None,
            }),
            timestamp: Utc::now(),
            duration_ms,
        }
    }

    /// The refusal envelope for a policy denial — the ONE constructor that
    /// attaches what would lift it.
    ///
    /// SECURITY: every site that turns a `DenyReason` into a result must come
    /// through here. A site that calls plain `err` instead produces a refusal
    /// with `liftable: None`, which the orchestrator reads as "not liftable"
    /// and renders as a wall — fail-closed, but it silently un-implements this
    /// feature at that one call site, which is exactly the dead-scaffold shape
    /// docs/invariants.md names. `every_policy_refusal_carries_its_lift` walks the
    /// reasons to keep the two in step.
    pub fn policy_denied(
        action_type: impl Into<String>,
        reason: &crate::policy::DenyReason,
        duration_ms: u64,
    ) -> Self {
        let action_type = action_type.into();
        let liftable = reason.liftable(&action_type);
        let mut result = Self::err(
            action_type,
            ErrorKind::PolicyDenied,
            reason.message(),
            duration_ms,
        );
        if let Some(error) = result.error.as_mut() {
            error.liftable = liftable;
        }
        result
    }
}

/// What the policy engine would do with an action, on the wire.
///
/// The three arms are [`crate::policy::PolicyEffect`]'s three arms, carried
/// out of the process without the reason string having to encode which one it
/// was. Serialized lowercase (`"allow"`, `"deny"`, `"confirm"`).
///
/// SECURITY: `Confirm` does **not** mean "ask and then it will run". It means
/// `policy.mode = "confirm"`, and every execution entry point in this runtime
/// turns `RequireConfirmation` into a refusal — `executor/mod.rs:179`,
/// `server.rs:327` (which `serve_stdio` goes through), `mcp/server.rs:194`,
/// `pipeline.rs:72`, `composite.rs:367` — because a confined runtime has
/// nobody to ask. Measured on this binary 2026-08-16 with `mode = "confirm"`:
/// `RunCommand cargo --version` → `Action requires user approval: RunCommand
/// requires confirmation`, `success: false`. A caller that reads `Confirm` as
/// "prompt, then execute" reproduces the refusal-after-a-yes of report P8a.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyVerdict {
    /// The policy engine would let this run.
    Allow,
    /// The policy engine refuses it, and says why in `reason`.
    Deny,
    /// The policy engine demands a confirmation it cannot obtain. See the
    /// SECURITY note above: this refuses at execution.
    Confirm,
}

/// Keep a `false`/`0` out of the wire. The overwhelmingly common command
/// produces nothing to spill, and its payload must stay exactly the shape every
/// frontend already parses.
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// All possible successful output payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionOutput {
    // ── Filesystem ────────────────────────────────────────────────────────────
    FileContent {
        path: String,
        content: String,
        size_bytes: u64,
        truncated: bool,
    },
    FileWritten {
        path: String,
        bytes_written: u64,
    },
    FileEdited {
        path: String,
        edits_applied: usize,
        edits_failed: usize,
        details: Vec<EditDetail>,
    },
    LineInserted {
        path: String,
        inserted_at_line: usize,
        lines_inserted: usize,
        total_lines_after: usize,
    },
    PatchApplied {
        path: String,
        hunks_total: usize,
        hunks_applied: usize,
        hunks_failed: usize,
        lines_added: usize,
        lines_removed: usize,
    },
    FileChunk {
        path: String,
        start_line: usize,
        end_line: usize,
        total_lines: usize,
        content: String,
        truncated: bool,
    },
    DirCreated {
        path: String,
    },
    MovedToTrash {
        path: String,
    },
    FileCopied {
        src: String,
        dest: String,
        bytes_copied: u64,
    },
    FileMoved {
        src: String,
        dest: String,
    },
    TempFileCreated {
        path: String,
        size_bytes: u64,
    },
    DirListing {
        path: String,
        entries: Vec<DirEntry>,
        total: usize,
        truncated: bool,
    },
    TreeOutput {
        path: String,
        tree: String,
        stats: TreeStats,
    },
    FileList {
        root: String,
        pattern: String,
        matches: Vec<String>,
        total: usize,
        truncated: bool,
    },

    // ── Search ────────────────────────────────────────────────────────────────
    SearchMatches {
        root: String,
        pattern: String,
        matches: Vec<CodeMatch>,
        total: usize,
        truncated: bool,
        /// Entries of the walk withheld by the read policy (deny patterns):
        /// files skipped and directories pruned, counted, never named.
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_by_policy: usize,
    },

    // ── Network ───────────────────────────────────────────────────────────────
    FetchUrlResult {
        url: String,
        status: u16,
        body: String,
        /// Size of the whole converted text; `body` may be one window of it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_bytes: Option<usize>,
        /// Pass as `offset` to read the next window; absent at the end.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_offset: Option<usize>,
        #[serde(default, skip_serializing_if = "is_false")]
        cached: bool,
        /// Citation id (`S<n>`), stable for this URL in this runtime process.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    HttpResponse {
        url: String,
        status: u16,
        headers: std::collections::HashMap<String, String>,
        body: String,
    },

    // ── Git ──────────────────────────────────────────────────────────────────
    GitStatus {
        path: String,
        entries: Vec<GitStatusEntry>,
    },
    GitDiff {
        path: String,
        diff: String,
    },

    // ── Process ───────────────────────────────────────────────────────────────
    CommandOutput {
        command: String,
        args: Vec<String>,
        exit_code: i32,
        stdout: String,
        stderr: String,
        timed_out: bool,
        /// The capture hit `MAX_CAPTURE_BYTES` and what follows is head+tail.
        ///
        /// SECURITY: stated, never inferred. A model that reads a silently
        /// amputated output concludes on a file it believes it read whole —
        /// the same defect class as a silent timeout clamp.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
        /// Lower bound on the bytes missing from `stdout` + `stderr`. Lower,
        /// not exact: the cap walks back to character boundaries, and the whole
        /// output is in the spill file anyway (design § O-5).
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted_bytes: usize,
        /// Where the untruncated stdout was written, when it was. `None` means
        /// there is nothing to read — never "look somewhere else".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout_spill_path: Option<String>,
        /// Same, for stderr. A distinct file: a build's diagnostics are on
        /// stderr and its progress on stdout, and one concatenated file cannot
        /// be split back apart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stderr_spill_path: Option<String>,
        /// A spill was DUE (the capture was truncated) and refused, and this
        /// is why — the 64 MiB directory cap, an unreadable directory.
        ///
        /// SECURITY: the cap used to refuse silently, so past 64 MiB
        /// the model stopped receiving paths and had no way to tell "nothing
        /// was omitted" from "everything past the cap is gone". Silence is
        /// the failure mode of a control: this names it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spill_refused: Option<String>,
    },
    /// Returned when RunCommand is called with stream: true.
    HandleCreated {
        handle: String,
        handle_type: String,
        streaming: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// Returned by RunInSandbox — output from an ephemeral Podman container.
    SandboxOutput {
        image: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
        timed_out: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        container_id: Option<String>,
        /// Files collected from inside the container (path → content).
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        collected_files: std::collections::HashMap<String, String>,
    },

    /// Returned by RunContainerGroup — aggregated results from parallel Podman containers.
    ContainerGroupOutput {
        /// Total tasks requested.
        total: usize,
        /// Number of tasks that exited with code 0.
        succeeded: usize,
        /// Number of tasks that failed or timed out.
        failed: usize,
        /// Per-task results.
        results: Vec<ContainerTaskResult>,
    },

    // IncusSandboxOutput / IncusGroupOutput were the strongly-typed outputs
    // for runtime-side RunIncus / RunIncusGroup. Those primitives moved to
    // the orchestrator (privilege boundary fix) and now emit raw JSON
    // matching the same shape — no runtime type plumbing needed.

    // ── Logs ─────────────────────────────────────────────────────────────────
    LogLines {
        path: String,
        lines: Vec<String>,
        total_lines: usize,
    },

    // ── Human ─────────────────────────────────────────────────────────────────
    HumanAnswer {
        question: String,
        answer: String,
    },
    HumanConfirmed {
        message: String,
        confirmed: bool,
    },
    /// Returned by AskMultipleChoice. Exactly one of `choice_index`, `skipped`,
    /// or `other` describes the operator's pick.
    HumanChoice {
        question: String,
        /// Original choices as presented to the operator.
        choices: Vec<String>,
        /// 0-based index into `choices`. None when `skipped` or `other`.
        #[serde(skip_serializing_if = "Option::is_none")]
        choice_index: Option<usize>,
        /// Selected label, or the free-form text typed when `other=true`.
        /// None when `skipped`.
        #[serde(skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// True when the operator picked the "Skip" escape hatch.
        skipped: bool,
        /// True when the operator picked "Other (specify)" and typed a custom value.
        other: bool,
    },

    // ── Math ─────────────────────────────────────────────────────────────────
    MathResult {
        result: f64,
    },
    /// Arbitrary-precision integer result (for numbers too large for f64).
    MathBigResult {
        result: String,
        digits: usize,
    },
    MathExactResult {
        result: f64,
        exact: bool,
    },
    MathBoolResult {
        result: bool,
    },
    MathCompareResult {
        result: String,
        a: f64,
        b: f64,
    },
    MathMinMaxResult {
        result: f64,
        index: usize,
    },
    MathAggregateResult {
        result: f64,
        count: usize,
    },
    MathEqResult {
        result: bool,
        diff: f64,
    },

    // ── String ───────────────────────────────────────────────────────────────
    StringResult {
        result: String,
    },
    StringParts {
        parts: Vec<String>,
        count: usize,
    },
    StringCount {
        count: usize,
    },
    StringLength {
        length: usize,
    },
    StringMatch {
        matches: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },

    // ── Pipeline ─────────────────────────────────────────────────────────────
    PipelineResult {
        steps_total: usize,
        steps_completed: usize,
        stopped_early: bool,
        results: Vec<StepResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_name: Option<String>,
    },

    // ── Try / Assert ──────────────────────────────────────────────────────────
    TryResult {
        ok: bool,
        completed: usize,
        total: usize,
        rolled_back: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot_name: Option<String>,
        results: Vec<StepResult>,
    },
    AssertResult {
        ok: bool,
        check: String,
        detail: String,
    },
    /// Output of a successfully-evaluated `Action::If`.
    /// `check_ok` tells which branch ran (`"then"` vs `"else"`); the branch
    /// itself is a mini-pipeline whose per-step results are embedded so the
    /// LLM can reason about what happened without a follow-up query.
    IfResult {
        /// Result of the condition check.
        check_ok: bool,
        /// Which branch ran: `"then"`, `"else"`, or `"none"` when the
        /// selected branch was empty.
        branch: String,
        /// Human-readable detail for the condition (same format as
        /// `AssertResult.detail`).
        check_detail: String,
        /// Total steps in the branch that was taken.
        steps_total: usize,
        /// Number of steps that were executed (stops at first failure if
        /// the branch had a failing step).
        steps_completed: usize,
        /// True if a step failed and we bailed before the branch end.
        stopped_early: bool,
        /// Per-step results — same shape as `PipelineResult.results`.
        results: Vec<StepResult>,
    },

    // ── Snapshots ─────────────────────────────────────────────────────────────
    SnapshotCreated {
        name: String,
        files_count: usize,
        total_bytes: u64,
        created_at: String,
        /// "reflink" if all files were CoW-cloned (btrfs/XFS/APFS),
        /// "copy" if all were byte-copied (ext4/etc.),
        /// "mixed" if both paths were taken across the file set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        /// Where the snapshot lives: "filesystem" (this host's local FS,
        /// stored under ~/.x0ne/snapshots/) or "incus" (a btrfs subvolume
        /// snapshot of an Incus container). Helps users tell the two
        /// snapshot families apart at a glance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    SnapshotRestored {
        name: String,
        files_restored: usize,
        files_failed: usize,
        /// Files deleted during restore because they were added by the
        /// agent after the snapshot was taken. Makes restore atomic:
        /// state-after-restore == state-at-snapshot.
        #[serde(default)]
        files_removed: usize,
        details: Vec<RestoreDetail>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    SnapshotList {
        snapshots: Vec<SnapshotMeta>,
    },
    SnapshotDeleted {
        name: String,
        deleted: bool,
    },

    // ── Handles ───────────────────────────────────────────────────────────────
    /// Returned by Subscribe.
    SubscribeCreated {
        handle: String,
        event_type: String,
        subscribed: bool,
    },
    HandleEvents {
        handle: String,
        status: serde_json::Value,
        events: Vec<serde_json::Value>,
        events_dropped: u64,
    },
    HandleStopped {
        handle: String,
        stopped: bool,
        final_status: String,
    },
    HandleList {
        handles: Vec<serde_json::Value>,
    },
    /// Returned by SendInput — confirmation that bytes were written to stdin.
    InputSent {
        handle: String,
        bytes_written: usize,
    },
    /// Returned by every Term* action; `data` holds the screen, status, paths.
    Term {
        op: String,
        summary: String,
        data: serde_json::Value,
    },
    /// Returned by BrowseUrl — extracted text from a web page.
    BrowseResult {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        text: String,
        /// Browser engine used (chromium, chrome, lynx, curl).
        engine: String,
        truncated: bool,
        /// `true` when the extracted text looks like a noscript shim,
        /// a JS-redirect intermediate page ("Click here if not redirected"),
        /// or pure navigation chrome. Present so an LLM (and the TUI)
        /// can treat the result with appropriate scepticism instead of
        /// happily citing a search-engine's redirect page as a source.
        /// When true, `low_signal_reason` carries a human-readable tag.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        low_signal: bool,
        /// Machine-readable reason (e.g. `"js_redirect_shim"`,
        /// `"too_short"`, `"nav_chrome_only"`). Present iff `low_signal`.
        /// The LLM prompt is expected to branch on this tag — e.g.
        /// "try a different URL" for a JS-redirect, "use WebSearch
        /// instead" for nav_chrome_only.
        #[serde(skip_serializing_if = "Option::is_none")]
        low_signal_reason: Option<String>,
    },

    // ── Meta ─────────────────────────────────────────────────────────────────
    ActionList {
        actions: Vec<serde_json::Value>,
    },
    ActionDescription {
        action_type: String,
        description: String,
        category: String,
        mutable: bool,
        schema: serde_json::Value,
        examples: Vec<serde_json::Value>,
    },
    PolicyCheckResult {
        /// Kept for callers written against the two-state shape. It is
        /// `false` for BOTH `Deny` and `Confirm`, which is exactly the
        /// conflation `verdict` exists to undo — prefer `verdict`.
        allowed: bool,
        /// SECURITY: the tri-state a caller must branch on before it decides
        /// whether to put a question in front of a human.
        ///
        /// `allowed: bool` alone cannot answer "would this run?", because a
        /// `Deny` and a `RequireConfirmation` both render as `false` and only
        /// the free-text `reason` told them apart — by the prefix
        /// `"Requires confirmation: "`, which is a message, not a contract.
        /// The orchestrator's preflight (`agent::preflight`) reads this field
        /// to refuse before prompting; a caller that string-matched `reason`
        /// instead would break the first time the wording changed.
        verdict: PolicyVerdict,
        action_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// SECURITY: same contract as `ActionError::liftable` — set only for
        /// a `Deny` the operator may widen, absent for every other verdict.
        /// P8a reads it to determine between a question and a wall.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        liftable: Option<x0ne_actions::policy_grant::LiftableDenial>,
    },

    /// Result of [`crate::action::Action::Simulate`] — a projection of what an
    /// action (or nested Pipeline) *would* do, with no mutations executed.
    SimulationResult {
        /// Type of the root action that was simulated.
        target_type: String,
        /// Aggregate verdict: would the whole tree be allowed by policy?
        would_execute: bool,
        /// First denial reason encountered (None when `would_execute` is true).
        #[serde(skip_serializing_if = "Option::is_none")]
        blocking_reason: Option<String>,
        /// Total number of leaf steps walked (composites are not counted).
        step_count: usize,
        /// Per-step projection (depth-first order).
        steps: Vec<SimulatedStep>,
        /// Aggregate side-effect projection — at-a-glance "what would change".
        side_effects: SideEffectSummary,
    },

    // ── Composite Primitives ──────────────────────────────────────────────────
    PrimitiveDefined {
        name: String,
        param_count: usize,
        step_count: usize,
        ephemeral: bool,
    },
    PrimitiveDeleted {
        name: String,
        deleted: bool,
    },
    PrimitiveList {
        primitives: Vec<serde_json::Value>,
    },

    // ── Scripted Functions (user-defined Rhai/WASM primitives — Tier 1) ───────
    FunctionRegistered {
        name: String,
        tier: String,
        param_count: usize,
        source_hash: String,
    },
    FunctionCallResult {
        name: String,
        value: serde_json::Value,
        duration_us: u64,
    },
    FunctionListing {
        count: usize,
        functions: Vec<serde_json::Value>,
    },
    FunctionDeleted {
        name: String,
        removed: bool,
    },

    // ── asm-ai (Tier 2 capability) ────────────────────────────────────────────
    /// Preview of an asm-ai program: what it would touch, and — from T6
    /// onward — the diff it would produce. **Nothing has been written.**
    ProgramPreview {
        /// SHA-256 of the source. The same as `ResourceIdentity::Module` on the
        /// orchestrator side, kept equal by a test.
        program_hash: String,
        /// SECURITY: the **proven** paths — the full expansion, deduplicated
        /// by canonical path, each one individually passed through
        /// `PolicyEngine::evaluate`. One real file, one entry: this is the
        /// number the proof header (T9) displays as « N files », and it would
        /// count nothing if it counted claims rather than files.
        ///
        /// **From T6 onward**, it will differ from
        /// `side_effects.paths_read`/`paths_written`, which will be the
        /// **effective** set after the `where` filter. At this step nothing is
        /// executed, so the filter has not run and the two lists coincide:
        /// `side_effects` is populated from `files`, split by capability.
        /// Reading them as « proven » versus « effective » now would lend them
        /// a distinction they do not have yet.
        files: Vec<String>,
        /// SECURITY: symbolic links excluded from the expansion. Doctrine
        /// « never silent »: the difference between this count and zero is what
        /// explains an « N files » smaller than what the pattern seemed to
        /// designate.
        #[serde(default)]
        symlinks_skipped: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<i64>,
        /// Diff, table and module output, capped together.
        bounded: crate::asm_verify::Bounded,
        side_effects: SideEffectSummary,
    },

    // ── System ────────────────────────────────────────────────────────────────
    SystemInfoOutput {
        hostname: String,
        os: String,
        arch: String,
        cpu_count: usize,
        memory_total_mb: u64,
        memory_available_mb: u64,
        disk_free_mb: u64,
        /// Process working directory **inside** the executor (T1).
        /// Critical when the executor runs in a sandbox/container or via
        /// HTTP on another host: the orchestrator's host cwd is irrelevant
        /// and using it would have the model probe paths that don't exist
        /// in the runtime. Empty string when the cwd cannot be read.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        cwd: String,
    },
    FileInfoOutput {
        path: String,
        size_bytes: u64,
        permissions: String,
        is_dir: bool,
        is_symlink: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        symlink_target: Option<String>,
        modified: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        created: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    ChecksumOutput {
        path: String,
        algorithm: String,
        hash: String,
    },
    ProcessList {
        processes: Vec<ProcessEntry>,
    },
    PortCheckResult {
        host: String,
        port: u16,
        open: bool,
        latency_ms: Option<u64>,
    },
    EnvVarResult {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        set: bool,
    },
    DnsResult {
        hostname: String,
        addresses: Vec<String>,
        resolved: bool,
    },
    WhoamiResult {
        username: String,
        uid: u32,
        gid: u32,
        home: String,
        shell: String,
    },
    DiskUsageResult {
        path: String,
        total_bytes: u64,
        used_bytes: u64,
        free_bytes: u64,
        usage_percent: f64,
    },
    UptimeResult {
        seconds: u64,
        load_avg: [f64; 3],
    },

    Slept {
        ms: u64,
    },

    // ── Data ─────────────────────────────────────────────────────────────────
    JsonData {
        path: String,
        data: serde_json::Value,
        size_bytes: u64,
    },
    JsonWritten {
        path: String,
        bytes_written: u64,
        pretty: bool,
    },
    TomlData {
        path: String,
        data: serde_json::Value,
        size_bytes: u64,
    },

    // ── Crypto ────────────────────────────────────────────────────────────────
    RandomBytesResult {
        bytes: String,
        encoding: String,
        count: usize,
    },

    // ── Desktop ───────────────────────────────────────────────────────────────
    ScreenshotTaken {
        path: String,
        width: u32,
        height: u32,
        size_bytes: u64,
    },
    WindowTree {
        windows: Vec<WindowInfo>,
    },
    FocusedWindow {
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<WindowInfo>,
    },
    MouseMoved {
        x: i32,
        y: i32,
    },
    MouseClicked {
        x: Option<i32>,
        y: Option<i32>,
        button: String,
        double: bool,
    },
    Scrolled {
        direction: String,
        amount: i32,
    },
    TextTyped {
        length: usize,
    },
    KeyPressed {
        keys: Vec<String>,
    },
    WindowFocused {
        window_name: String,
        window_id: i64,
    },
    NotificationSent {
        sent: bool,
    },
    ClipboardContent {
        content: String,
        length: usize,
    },
    ClipboardSet {
        set: bool,
        length: usize,
    },

    // ── ML / LLM ─────────────────────────────────────────────────────────────
    LlmResponse {
        model: String,
        response: String,
        tokens_used: u64,
    },
    OllamaModels {
        operation: String,
        models: Vec<serde_json::Value>,
        message: String,
    },
    DatasetCreated {
        path: String,
        entries_count: usize,
        failures: usize,
        topic: String,
        size_bytes: u64,
    },
    DocumentExtracted {
        input_path: String,
        output_path: String,
        format: String,
        entries_count: usize,
        text_length: usize,
        source_type: String,
    },

    // ── Memory ───────────────────────────────────────────────────────────────
    /// Flexible JSON result — used by Memory primitives and future extensions.
    Generic {
        data: serde_json::Value,
    },

    // ── Artifact ───────────────────────────────────────────────────────────────
    /// Returned by ArtifactCreate, ArtifactServe, ArtifactSave — a snapshot of one
    /// artifact with everything the agent or CLI needs to use it.
    ///
    /// Field is `artifact_kind` (not `kind`) to avoid clashing with the enum's
    /// own serde `tag = "kind"` discriminator.
    ArtifactInfo {
        id: String,
        name: Option<String>,
        artifact_kind: String, // "static" | "service"
        state: String,         // "created" | "running" | "stopped" | "failed"
        persistent: bool,
        url: String,
        root_path: String,
        proxy_port: u16,
        size_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        service_pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        service_port: Option<u16>,
    },

    /// Returned by ArtifactList — the proxy port plus all artifacts.
    ArtifactListing {
        proxy_port: u16,
        proxy_running: bool,
        artifacts: Vec<ArtifactSummary>,
    },

    /// Returned by ArtifactStop / ArtifactDestroy — confirmation payload.
    ArtifactOpResult {
        id: String,
        op: String, // "stopped" | "destroyed"
        message: String,
    },
}

/// Lightweight per-artifact record returned in `ArtifactListing`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub id: String,
    pub name: Option<String>,
    pub artifact_kind: String,
    pub state: String,
    pub persistent: bool,
    pub url: String,
    pub root_path: String,
    pub size_bytes: u64,
    pub age_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_port: Option<u16>,
}

impl ActionOutput {
    /// Produce a short, single-line, human-readable summary of this output.
    ///
    /// Used to populate `StepResult::output_summary` so a Pipeline result
    /// gives the LLM a per-step overview of what actually happened —
    /// without forcing it to crawl the full structured payload of every
    /// step. Keep summaries short (target: ≤60 chars) and free of secrets.
    /// Variants we don't have a tailored line for fall back to "ok"
    /// rather than dumping the entire payload.
    pub fn summarize(&self) -> String {
        use ActionOutput::*;
        match self {
            FileContent {
                size_bytes,
                truncated,
                ..
            } => {
                let s = format_size(*size_bytes);
                if *truncated {
                    format!("{} (truncated)", s)
                } else {
                    s
                }
            }
            FileWritten { bytes_written, .. } => format!("{} written", format_size(*bytes_written)),
            FileEdited {
                edits_applied,
                edits_failed,
                ..
            } => {
                if *edits_failed > 0 {
                    format!("{} edits ok, {} failed", edits_applied, edits_failed)
                } else {
                    format!(
                        "{} edit{}",
                        edits_applied,
                        if *edits_applied == 1 { "" } else { "s" }
                    )
                }
            }
            LineInserted { lines_inserted, .. } => format!(
                "{} line{} inserted",
                lines_inserted,
                if *lines_inserted == 1 { "" } else { "s" }
            ),
            PatchApplied {
                hunks_applied,
                hunks_failed,
                lines_added,
                lines_removed,
                ..
            } => {
                if *hunks_failed > 0 {
                    format!(
                        "{}/{} hunks (+{} -{})",
                        hunks_applied,
                        hunks_applied + hunks_failed,
                        lines_added,
                        lines_removed
                    )
                } else {
                    format!(
                        "{} hunks (+{} -{})",
                        hunks_applied, lines_added, lines_removed
                    )
                }
            }
            FileChunk {
                start_line,
                end_line,
                total_lines,
                truncated,
                ..
            } => {
                let base = format!("L{}-{}/{}", start_line, end_line, total_lines);
                if *truncated {
                    format!("{} (truncated)", base)
                } else {
                    base
                }
            }
            DirCreated { path } => format!("created {}", path),
            MovedToTrash { path } => format!("trashed {}", path),
            FileCopied { bytes_copied, .. } => format!("{} copied", format_size(*bytes_copied)),
            FileMoved { dest, .. } => format!("→ {}", dest),
            TempFileCreated { path, .. } => format!("temp {}", path),
            DirListing {
                entries,
                total,
                truncated,
                ..
            } => {
                let base = format!("{} entries", entries.len());
                if *truncated {
                    format!("{} of {}", base, total)
                } else {
                    base
                }
            }
            TreeOutput { stats, .. } => {
                format!("{} files, {} dirs", stats.total_files, stats.total_dirs)
            }
            FileList {
                matches,
                total,
                truncated,
                ..
            } => {
                if *truncated {
                    format!("{} of {} files", matches.len(), total)
                } else {
                    format!("{} files", matches.len())
                }
            }
            SearchMatches {
                matches,
                total,
                truncated,
                ..
            } => {
                if *truncated {
                    format!("{} of {} matches", matches.len(), total)
                } else {
                    format!(
                        "{} match{}",
                        matches.len(),
                        if matches.len() == 1 { "" } else { "es" }
                    )
                }
            }
            FetchUrlResult { status, body, .. } | HttpResponse { status, body, .. } => {
                format!("{} {}", status, format_size(body.len() as u64))
            }
            GitStatus { entries, .. } => {
                if entries.is_empty() {
                    "clean".into()
                } else {
                    format!("{} changed", entries.len())
                }
            }
            GitDiff { diff, .. } => {
                let adds = diff
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                    .count();
                let dels = diff
                    .lines()
                    .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                    .count();
                if adds == 0 && dels == 0 {
                    "no changes".into()
                } else {
                    format!("+{} -{}", adds, dels)
                }
            }
            CommandOutput {
                exit_code,
                timed_out,
                ..
            } => {
                if *timed_out {
                    "timeout".into()
                } else if *exit_code == 0 {
                    "exit 0".into()
                } else {
                    format!("exit {}", exit_code)
                }
            }
            HandleCreated {
                handle, streaming, ..
            } => {
                if *streaming {
                    format!("→ {} (streaming)", handle)
                } else {
                    format!("→ {}", handle)
                }
            }
            SandboxOutput {
                exit_code,
                image,
                timed_out,
                ..
            } => {
                if *timed_out {
                    format!("{} timeout", image)
                } else {
                    format!("{} → exit {}", image, exit_code)
                }
            }
            ContainerGroupOutput {
                total,
                succeeded,
                failed,
                ..
            } => {
                format!("{}/{} ok, {} failed", succeeded, total, failed)
            }
            LogLines {
                lines, total_lines, ..
            } => format!("{} of {} lines", lines.len(), total_lines),
            HumanAnswer { answer, .. } => format!("\"{}\"", truncate_str(answer, 40)),
            HumanConfirmed { confirmed, .. } => {
                if *confirmed {
                    "yes".into()
                } else {
                    "no".into()
                }
            }
            HumanChoice {
                answer, skipped, ..
            } => {
                if *skipped {
                    "skipped".into()
                } else {
                    answer
                        .as_deref()
                        .map(|a| format!("\"{}\"", truncate_str(a, 40)))
                        .unwrap_or_else(|| "ok".into())
                }
            }
            MathResult { result } | MathExactResult { result, .. } => {
                format!("= {}", format_number(*result))
            }
            MathBigResult { result, digits } => {
                if *digits <= 40 {
                    format!("= {}", result)
                } else {
                    format!(
                        "= {}…{} ({} digits)",
                        &result[..20],
                        &result[result.len() - 10..],
                        digits
                    )
                }
            }
            MathBoolResult { result } => result.to_string(),
            MathCompareResult { result, .. } => result.clone(),
            MathMinMaxResult { result, .. } => format!("= {}", format_number(*result)),
            MathAggregateResult { result, count, .. } => {
                format!("= {} (n={})", format_number(*result), count)
            }
            MathEqResult { result, .. } => result.to_string(),
            StringResult { result } => format!("\"{}\"", truncate_str(result, 40)),
            StringParts { count, .. } => format!("{} parts", count),
            StringCount { count } => format!("= {}", count),
            StringLength { length } => format!("= {}", length),
            StringMatch { matches, index, .. } => match (matches, index) {
                (true, Some(i)) => format!("found at {}", i),
                (true, None) => "found".into(),
                (false, _) => "not found".into(),
            },
            PipelineResult {
                steps_total,
                steps_completed,
                stopped_early,
                ..
            } => {
                if *stopped_early {
                    format!("{}/{} (stopped)", steps_completed, steps_total)
                } else {
                    format!("{}/{} steps", steps_completed, steps_total)
                }
            }
            TryResult { .. } | AssertResult { .. } => "ok".into(),
            IfResult {
                check_ok,
                branch,
                steps_completed,
                steps_total,
                stopped_early,
                ..
            } => {
                let gate = if *check_ok { "true" } else { "false" };
                if *stopped_early {
                    format!(
                        "{} → {} {}/{} (stopped)",
                        gate, branch, steps_completed, steps_total
                    )
                } else if *steps_total == 0 {
                    format!("{} → {} (empty)", gate, branch)
                } else {
                    format!("{} → {} {}/{}", gate, branch, steps_completed, steps_total)
                }
            }
            SnapshotCreated { name, .. } => format!("'{}'", name),
            SnapshotRestored { name, .. } => format!("'{}'", name),
            SnapshotList { snapshots, .. } => format!("{} snapshots", snapshots.len()),
            SnapshotDeleted { name, .. } => format!("deleted '{}'", name),
            SubscribeCreated { handle, .. } => format!("→ {}", handle),
            HandleEvents { events, .. } => format!("{} events", events.len()),
            HandleStopped { .. } => "stopped".into(),
            HandleList { handles, .. } => format!("{} handles", handles.len()),
            ActionList { actions, .. } => format!("{} actions", actions.len()),
            ActionDescription { action_type, .. } => action_type.clone(),
            PolicyCheckResult {
                allowed,
                action_type,
                ..
            } => {
                if *allowed {
                    format!("{} allowed", action_type)
                } else {
                    format!("{} denied", action_type)
                }
            }
            SimulationResult {
                target_type,
                would_execute,
                step_count,
                ..
            } => {
                if *would_execute {
                    format!(
                        "simulate {} → {} steps, all allowed",
                        target_type, step_count
                    )
                } else {
                    format!("simulate {} → {} steps, BLOCKED", target_type, step_count)
                }
            }
            PrimitiveDefined { name, .. } => format!("defined '{}'", name),
            PrimitiveDeleted { name, .. } => format!("deleted '{}'", name),
            PrimitiveList { primitives, .. } => format!("{} primitives", primitives.len()),
            FunctionRegistered { name, tier, .. } => format!("defined fn '{}' ({})", name, tier),
            FunctionCallResult {
                name, duration_us, ..
            } => {
                if *duration_us < 1000 {
                    format!("{}() → {}µs", name, duration_us)
                } else {
                    format!("{}() → {:.1}ms", name, *duration_us as f64 / 1000.0)
                }
            }
            FunctionListing { count, .. } => {
                format!("{} function{}", count, if *count == 1 { "" } else { "s" })
            }
            FunctionDeleted { name, removed } => {
                if *removed {
                    format!("deleted fn '{}'", name)
                } else {
                    format!("'{}' not found", name)
                }
            }
            // The number is that of the **proven** paths: it is the one the
            // proof header displays, and the summary of a Pipeline step must
            // not state a different one.
            ProgramPreview { files, bounded, .. } => {
                let n = files.len();
                if bounded.truncated {
                    format!("{n} file(s) proved, output truncated")
                } else {
                    format!("{n} file(s) proved, nothing written")
                }
            }
            SystemInfoOutput { os, hostname, .. } => format!("{} @ {}", os, hostname),
            FileInfoOutput { size_bytes, .. } => format_size(*size_bytes),
            ChecksumOutput {
                algorithm, hash, ..
            } => format!("{} {}", algorithm, &hash[..hash.len().min(12)]),
            ProcessList { processes, .. } => format!("{} processes", processes.len()),
            PortCheckResult { open, .. } => {
                if *open {
                    "open".into()
                } else {
                    "closed".into()
                }
            }
            EnvVarResult { name, value, .. } => match value {
                Some(v) => format!("{}={}", name, truncate_str(v, 30)),
                None => format!("{} unset", name),
            },
            DnsResult { addresses, .. } => format!("{} addrs", addresses.len()),
            WhoamiResult { username, .. } => username.clone(),
            DiskUsageResult {
                total_bytes,
                used_bytes,
                ..
            } => {
                format!("{}/{}", format_size(*used_bytes), format_size(*total_bytes))
            }
            UptimeResult { seconds, .. } => format!("up {}s", seconds),
            Slept { ms } => format!("slept {}ms", ms),
            JsonData { .. } => "json".into(),
            JsonWritten { path, .. } => format!("wrote {}", path),
            TomlData { .. } => "toml".into(),
            RandomBytesResult { count, .. } => format!("{} bytes", count),
            ScreenshotTaken { path, .. } => format!("→ {}", path),
            WindowTree { windows, .. } => format!("{} windows", windows.len()),
            FocusedWindow { window } => match window {
                Some(w) => w.app_id.clone(),
                None => "no focus".into(),
            },
            MouseMoved { .. }
            | MouseClicked { .. }
            | Scrolled { .. }
            | TextTyped { .. }
            | KeyPressed { .. }
            | WindowFocused { .. }
            | NotificationSent { .. }
            | ClipboardSet { .. } => "ok".into(),
            ClipboardContent { content, .. } => format!("\"{}\"", truncate_str(content, 40)),
            LlmResponse { tokens_used, .. } => format!("{} tokens", tokens_used),
            OllamaModels { models, .. } => format!("{} models", models.len()),
            DatasetCreated {
                topic,
                entries_count,
                ..
            } => format!("'{}' ({} entries)", topic, entries_count),
            DocumentExtracted {
                format,
                text_length,
                ..
            } => format!("{} {}", format, format_size(*text_length as u64)),
            InputSent { bytes_written, .. } => format!("{} bytes sent", bytes_written),
            Term { summary, .. } => summary.clone(),
            BrowseResult {
                engine,
                text,
                truncated,
                low_signal,
                low_signal_reason,
                ..
            } => {
                let base = format!("{} {}", engine, format_size(text.len() as u64));
                let mut s = if *truncated {
                    format!("{} (truncated)", base)
                } else {
                    base
                };
                if *low_signal {
                    let reason = low_signal_reason.as_deref().unwrap_or("low_signal");
                    s.push_str(&format!(" ⚠ {}", reason));
                }
                s
            }
            Generic { .. } => "ok".into(),
            ArtifactInfo {
                id,
                state,
                artifact_kind,
                ..
            } => format!("{} ({} {})", id, artifact_kind, state),
            ArtifactListing { artifacts, .. } => format!("{} artifact(es)", artifacts.len()),
            ArtifactOpResult { id, op, .. } => format!("{} {}", id, op),
        }
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

// ── Sub-types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditDetail {
    pub old_preview: String,
    pub replacements: usize,
    /// 1-based line where the (first) replacement landed, so the model can
    /// verify its edit without a follow-up ReadFile round-trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// The replacement in place, with a couple of surrounding lines — the
    /// same "show the modified region" contract Claude Code's Edit has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeStats {
    pub total_files: usize,
    pub total_dirs: usize,
    pub total_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeMatch {
    pub file: String,
    pub line: u64,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub status: String,
    pub path: String,
}

/// One step result inside a PipelineResult.
///
/// `output_summary` is a short, human-readable one-line description of what
/// the step actually produced (e.g. "12 entries", "exit 0", "1.2KB written").
/// Populated by `summarize_step_output` so the LLM can reason about pipeline
/// progress without re-parsing every step's full `output` payload — it's the
/// counterpart of the orchestrator's per-action result_line, but baked into
/// the structured pipeline result so every consumer sees the same summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step: usize,
    pub action_type: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<ActionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ActionError>,
    pub duration_ms: u64,
}

/// Metadata about a single snapshot (used in SnapshotList).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub name: String,
    pub files_count: usize,
    pub total_bytes: u64,
    pub created_at: String,
}

/// One file's restore outcome (used in SnapshotRestored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreDetail {
    pub path: String,
    pub status: String, // "restored" | "failed: <reason>"
}

/// One process entry (used in ListProcesses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub command: String,
    pub memory_kb: u64,
}

/// One Sway/Wayland window entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: i64,
    pub name: String,
    pub app_id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    pub visible: bool,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    PolicyDenied,
    ActionDisabled,
    PathDenied,
    NotFound,
    PermissionDenied,
    NotImplemented,
    Timeout,
    ExecutionFailed,
    InvalidInput,
    Internal,
    // New in v0.2
    DesktopDisabled,
    MissingDependency,
    HandleNotFound,
    NestingNotAllowed,
    SnapshotError,
    EnvVarDenied,
    CompositeError,
    /// Action belongs to a Cargo feature that was not compiled into this build.
    /// Re-build with `--features prim-<family>` to enable.
    FeatureDisabled,
}

/// High-level error class. Drives LLM recovery strategy.
///
/// Orthogonal to `ErrorKind`:
/// - `kind` says *what specifically went wrong* (e.g. `PathDenied`).
/// - `class` says *what family of recovery applies* (e.g. `policy_denied` →
///   change the plan, do not retry the same call).
///
/// The model can switch on `class` alone to pick the right next move,
/// without having to enumerate every `kind`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Malformed payload before reaching the executor (bad JSON, bad wrapper).
    /// Recovery: re-emit a syntactically valid action.
    ParseError,
    /// Schema/type violation (unknown variant, missing required field, bad
    /// argument type). Recovery: correct the arguments.
    ValidationError,
    /// Authorization failure (policy, allowlist, deny pattern, readonly mode,
    /// rate limit). Recovery: do NOT retry the same call — change the plan,
    /// pick a different primitive or ask the user.
    PolicyDenied,
    /// Target resource does not exist (file, handle, snapshot, env var).
    /// Recovery: verify existence first (e.g. `ListDir`, `ListHandles`).
    NotFound,
    /// Execution reached the target but the environment refused or failed
    /// (command non-zero, network down, missing dep, timeout). Recovery:
    /// diagnose the environment, possibly retry with backoff if `retryable`.
    ///
    /// NOTE: variant name is `RuntimeError` (not `Runtime`) so the serde
    /// snake_case representation is `runtime_error` — matches the orchestrator
    /// `SystemFeedbackClass::from_core_class` mapping and the public docs.
    RuntimeError,
    /// Bug or unrecoverable internal state in x0ne-runtime.
    /// Recovery: surface to the user — do not retry.
    InternalError,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::ParseError => "parse_error",
            ErrorClass::ValidationError => "validation_error",
            ErrorClass::PolicyDenied => "policy_denied",
            ErrorClass::NotFound => "not_found",
            ErrorClass::RuntimeError => "runtime_error",
            ErrorClass::InternalError => "internal_error",
        }
    }

    /// Class-level recovery hint, embedded as `recommended_next_step` in the
    /// serialized error so the LLM doesn't need a side mapping. Kept short
    /// (one sentence). The orchestrator-side `SystemFeedbackClass::hint`
    /// stays the long-form version for the framed system message; this is
    /// the structured-field counterpart per T7's spec.
    pub fn recommended_next_step(self) -> &'static str {
        match self {
            ErrorClass::ParseError =>
                "Re-emit the same call inside <action>…</action> with valid JSON.",
            ErrorClass::ValidationError =>
                "Compare your JSON to the schema in the system prompt and fix the field names/types.",
            ErrorClass::PolicyDenied =>
                "Do NOT retry. Choose a different primitive or stop and ask the user.",
            ErrorClass::NotFound =>
                "Verify the target exists (StatFiles, ListHandles, ListSnapshots) before re-issuing.",
            ErrorClass::RuntimeError =>
                "Diagnose the environment (logs, exit code). Retry only if the cause is transient.",
            ErrorClass::InternalError =>
                "Surface this to the user. Do not retry — this is an x0ne-runtime bug.",
        }
    }
}

impl ErrorKind {
    /// Map a fine-grained `ErrorKind` to its `(ErrorClass, retryable)` tuple.
    ///
    /// `retryable` is a *hint to the LLM*, not a hard guarantee. It means
    /// "the same call with the same arguments has a non-trivial chance of
    /// succeeding on a later attempt" (e.g. network timeout). Policy denials
    /// and validation failures are explicitly NOT retryable — the model must
    /// change something before trying again.
    pub fn classify(&self) -> (ErrorClass, bool) {
        use ErrorKind::*;
        match self {
            // Authorization — never retry the same call.
            PolicyDenied | ActionDisabled | PathDenied | PermissionDenied | DesktopDisabled
            | EnvVarDenied | NestingNotAllowed | FeatureDisabled => {
                (ErrorClass::PolicyDenied, false)
            }

            // Schema / argument problems — fix the args, then retry.
            InvalidInput => (ErrorClass::ValidationError, false),

            // Missing target resources — verify, then retry with the right id.
            NotFound | HandleNotFound => (ErrorClass::NotFound, false),

            // Environment / execution — the call shape is fine, the world isn't.
            Timeout => (ErrorClass::RuntimeError, true),
            ExecutionFailed | MissingDependency | SnapshotError | CompositeError => {
                (ErrorClass::RuntimeError, false)
            }

            // Not yet implemented or x0ne-runtime bug — surface, do not retry.
            NotImplemented | Internal => (ErrorClass::InternalError, false),
        }
    }

    /// Stable machine-readable code for an `ErrorKind`. Snake-case, matches
    /// the serde representation. Useful for log correlation and metrics.
    pub fn code(&self) -> &'static str {
        use ErrorKind::*;
        match self {
            PolicyDenied => "policy_denied",
            ActionDisabled => "action_disabled",
            PathDenied => "path_denied",
            NotFound => "not_found",
            PermissionDenied => "permission_denied",
            NotImplemented => "not_implemented",
            Timeout => "timeout",
            ExecutionFailed => "execution_failed",
            InvalidInput => "invalid_input",
            Internal => "internal",
            DesktopDisabled => "desktop_disabled",
            MissingDependency => "missing_dependency",
            HandleNotFound => "handle_not_found",
            NestingNotAllowed => "nesting_not_allowed",
            SnapshotError => "snapshot_error",
            EnvVarDenied => "env_var_denied",
            CompositeError => "composite_error",
            FeatureDisabled => "feature_disabled",
        }
    }
}

/// Structured error payload. The LLM can dispatch its recovery strategy on
/// `class` alone; `kind` and `code` give finer granularity for logs and UI.
///
/// Schema (T7 contract — must stay stable across releases):
/// `{ kind, class, code, message, action_name, retryable, recommended_next_step }`
/// — `class` drives recovery, `code` is the machine-readable identifier,
/// `action_name` echoes the failing action so an isolated error block is
/// self-describing, `recommended_next_step` is a one-sentence guidance hint
/// the LLM can follow without consulting any external mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionError {
    /// Fine-grained discriminant (back-compat — existing field).
    pub kind: ErrorKind,
    /// Recovery class. Stable across kinds.
    pub class: ErrorClass,
    /// Stable machine code (snake_case). Same string as `kind` serialized.
    pub code: String,
    /// Human/LLM-readable message.
    pub message: String,
    /// Echoes the `action_type` of the failing call. Optional only because
    /// older results in cached logs may not have it; new errors always set it.
    /// Skipped from JSON when empty so we don't pollute legacy snapshots.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action_name: String,
    /// True if retrying the same call with the same arguments is sensible
    /// (e.g. transient network timeout). False for policy/validation/not_found.
    pub retryable: bool,
    /// One-sentence recovery hint derived from `class`. Lets the LLM act on
    /// a single field without a side table.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_next_step: String,
    /// What the operator would have to grant for this refusal to go away.
    ///
    /// SECURITY: present ONLY on a `policy_denied` refusal the operator can
    /// lift by widening one list he already maintains — the exhaustive match
    /// in `policy::DenyReason::liftable` decides, never this field's producer.
    /// Its ABSENCE is what tells the orchestrator that a refusal must be shown
    /// as a wall and never offered as a choice, so a `None` here is a security
    /// statement and not a missing value.
    ///
    /// The model reads this envelope too. It is safe for it to: naming the
    /// list that would lift a refusal is what the `message` already does in
    /// prose, and nothing the model can emit turns a `LiftableDenial` into a
    /// grant — only a human answering a prompt does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liftable: Option<x0ne_actions::policy_grant::LiftableDenial>,
}

impl ActionError {
    /// Build a fully-populated `ActionError` from `kind` + `message`.
    /// `action_name` is left empty — set it via `with_action_name` or rely
    /// on `ActionResult::err` to fill it from the parent `action_type`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let (class, retryable) = kind.classify();
        let code = kind.code().to_string();
        let recommended_next_step = class.recommended_next_step().to_string();
        Self {
            kind,
            class,
            code,
            message: message.into(),
            action_name: String::new(),
            retryable,
            recommended_next_step,
            liftable: None,
        }
    }

    /// Builder-style setter for `action_name`. Used when constructing the
    /// error standalone (composite primitives, manual error paths).
    pub fn with_action_name(mut self, name: impl Into<String>) -> Self {
        self.action_name = name.into();
        self
    }
}

// IncusTaskResult moved to x0ne-orchestrator (handled as raw JSON now).

/// Result for a single container task within a `RunContainerGroup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerTaskResult {
    /// Task ID as provided in the RunContainerGroup.tasks list.
    pub id: String,
    /// Auto-generated container name.
    pub container_name: String,
    /// Process exit code (-1 on timeout).
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// True if killed due to timeout.
    pub timed_out: bool,
    /// True if exit_code == 0.
    pub success: bool,
    /// Total wall-clock duration for this task (ms).
    pub duration_ms: u64,
}

// ── Simulate ──────────────────────────────────────────────────────────────────

/// Per-step projection produced by `Action::Simulate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatedStep {
    /// Dotted breadcrumb identifying this step inside the tree, e.g.
    /// `"root"`, `"root/Pipeline[0]"`, `"root/SafeChange.steps[1]/Pipeline[2]"`.
    pub path: String,
    /// `Action::type_name()` of this step.
    pub action_type: String,
    /// Side-effect bucket. Matches `policy::SideEffectClass::as_str()`.
    pub class: String,
    /// Free-form one-line "would do X" — only set when `explain: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `"allow"`, `"require_confirmation"`, or `"deny"`.
    pub verdict: String,
    /// Policy reason when verdict is not `allow`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_reason: Option<String>,
    /// Best-effort identification of what the step would touch
    /// (a path, a host, a command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// Aggregated "what would change" summary for a whole simulation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SideEffectSummary {
    pub paths_read: Vec<String>,
    pub paths_written: Vec<String>,
    pub paths_destroyed: Vec<String>,
    pub commands_spawned: Vec<String>,
    pub hosts_contacted: Vec<String>,
    pub desktop_actions: usize,
    /// Step indices (into `steps`) where policy returned a non-allow verdict.
    pub blocked_step_indices: Vec<usize>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_ok_result() {
        let result = ActionResult::ok(
            "ReadFile",
            ActionOutput::FileContent {
                path: "/tmp/a".into(),
                content: "hello".into(),
                size_bytes: 5,
                truncated: false,
            },
            12,
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"action_type\":\"ReadFile\""));
    }

    #[test]
    fn serialize_err_result() {
        let result = ActionResult::err("WriteFile", ErrorKind::PolicyDenied, "write denied", 0);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("policy_denied"));
    }

    #[test]
    fn serialize_pipeline_result() {
        let result = ActionResult::ok(
            "Pipeline",
            ActionOutput::PipelineResult {
                steps_total: 2,
                steps_completed: 2,
                stopped_early: false,
                results: vec![],
                snapshot_name: None,
            },
            5,
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"steps_total\":2"));
    }

    #[test]
    fn serialize_desktop_disabled_error() {
        let result = ActionResult::err("Screenshot", ErrorKind::DesktopDisabled, "disabled", 0);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("desktop_disabled"));
    }

    // ── Error classification ────────────────────────────────────────────────
    //
    // These tests pin the public contract that drives orchestrator framing:
    // for every `ErrorKind`, `(class, retryable)` MUST land in the bucket the
    // LLM expects. Changing one of these tuples is a breaking change to the
    // recovery semantics — bump and document.

    #[test]
    fn classify_authorization_kinds_to_policy_denied_non_retryable() {
        for k in [
            ErrorKind::PolicyDenied,
            ErrorKind::ActionDisabled,
            ErrorKind::PathDenied,
            ErrorKind::PermissionDenied,
            ErrorKind::DesktopDisabled,
            ErrorKind::EnvVarDenied,
            ErrorKind::NestingNotAllowed,
            ErrorKind::FeatureDisabled,
        ] {
            let (class, retryable) = k.classify();
            assert_eq!(
                class,
                ErrorClass::PolicyDenied,
                "kind {:?} must classify as PolicyDenied",
                k
            );
            assert!(
                !retryable,
                "policy denials must never be retryable: {:?}",
                k
            );
        }
    }

    #[test]
    fn classify_invalid_input_to_validation_error() {
        let (class, retryable) = ErrorKind::InvalidInput.classify();
        assert_eq!(class, ErrorClass::ValidationError);
        assert!(
            !retryable,
            "validation errors must require argument correction first"
        );
    }

    #[test]
    fn classify_not_found_kinds_to_not_found_non_retryable() {
        for k in [ErrorKind::NotFound, ErrorKind::HandleNotFound] {
            let (class, retryable) = k.classify();
            assert_eq!(class, ErrorClass::NotFound, "kind {:?}", k);
            assert!(
                !retryable,
                "not_found requires existence check first: {:?}",
                k
            );
        }
    }

    #[test]
    fn classify_timeout_to_runtime_retryable() {
        let (class, retryable) = ErrorKind::Timeout.classify();
        assert_eq!(class, ErrorClass::RuntimeError);
        assert!(
            retryable,
            "transient timeouts SHOULD be retryable with backoff"
        );
    }

    #[test]
    fn classify_runtime_kinds_to_runtime_non_retryable() {
        for k in [
            ErrorKind::ExecutionFailed,
            ErrorKind::MissingDependency,
            ErrorKind::SnapshotError,
            ErrorKind::CompositeError,
        ] {
            let (class, retryable) = k.classify();
            assert_eq!(class, ErrorClass::RuntimeError, "kind {:?}", k);
            assert!(!retryable, "runtime failures need diagnosis first: {:?}", k);
        }
    }

    #[test]
    fn classify_internal_kinds_to_internal_non_retryable() {
        for k in [ErrorKind::Internal, ErrorKind::NotImplemented] {
            let (class, retryable) = k.classify();
            assert_eq!(class, ErrorClass::InternalError, "kind {:?}", k);
            assert!(
                !retryable,
                "internal errors must never be silently retried: {:?}",
                k
            );
        }
    }

    #[test]
    fn class_serde_matches_as_str() {
        // Wire format MUST match `as_str()` so logs, dashboards, and the
        // orchestrator's `SystemFeedbackClass::from_core_class` agree.
        for class in [
            ErrorClass::ParseError,
            ErrorClass::ValidationError,
            ErrorClass::PolicyDenied,
            ErrorClass::NotFound,
            ErrorClass::RuntimeError,
            ErrorClass::InternalError,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{}\"", class.as_str()));
        }
    }

    #[test]
    fn err_result_exposes_class_and_retryable_in_json() {
        let policy = ActionResult::err("WriteFile", ErrorKind::PolicyDenied, "denied", 0);
        let policy_json = serde_json::to_string(&policy).unwrap();
        assert!(
            policy_json.contains("\"class\":\"policy_denied\""),
            "{}",
            policy_json
        );
        assert!(
            policy_json.contains("\"retryable\":false"),
            "{}",
            policy_json
        );

        let timeout = ActionResult::err("RunCommand", ErrorKind::Timeout, "timed out", 0);
        let timeout_json = serde_json::to_string(&timeout).unwrap();
        assert!(
            timeout_json.contains("\"class\":\"runtime_error\""),
            "{}",
            timeout_json
        );
        assert!(
            timeout_json.contains("\"retryable\":true"),
            "{}",
            timeout_json
        );

        let validation = ActionResult::err("ReadFile", ErrorKind::InvalidInput, "missing field", 0);
        let validation_json = serde_json::to_string(&validation).unwrap();
        assert!(
            validation_json.contains("\"class\":\"validation_error\""),
            "{}",
            validation_json
        );

        let not_found = ActionResult::err("ReadFile", ErrorKind::NotFound, "no such file", 0);
        let not_found_json = serde_json::to_string(&not_found).unwrap();
        assert!(
            not_found_json.contains("\"class\":\"not_found\""),
            "{}",
            not_found_json
        );
    }

    #[test]
    fn err_result_exposes_stable_code() {
        let r = ActionResult::err("ReadFile", ErrorKind::PathDenied, "blocked", 0);
        let json = serde_json::to_string(&r).unwrap();
        // `code` MUST equal the snake_case serialized `kind` so logs and
        // dashboards can pivot on it without parsing prose.
        assert!(json.contains("\"code\":\"path_denied\""), "{}", json);
        assert!(json.contains("\"kind\":\"path_denied\""), "{}", json);
    }

    #[test]
    fn three_classes_produce_visibly_different_json() {
        // The whole point of the redesign: an LLM (or grep, or a dashboard)
        // can tell parse / policy / runtime apart at a glance.
        let parse = ActionResult::err("X", ErrorKind::InvalidInput, "bad arg", 0);
        let policy = ActionResult::err("X", ErrorKind::PolicyDenied, "nope", 0);
        let runtm = ActionResult::err("X", ErrorKind::ExecutionFailed, "crashed", 0);
        let pj = serde_json::to_string(&parse).unwrap();
        let py = serde_json::to_string(&policy).unwrap();
        let rj = serde_json::to_string(&runtm).unwrap();
        assert_ne!(
            pj.split("\"class\":")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(20)
                .collect::<String>(),
            py.split("\"class\":")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(20)
                .collect::<String>(),
        );
        assert_ne!(
            py.split("\"class\":")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(20)
                .collect::<String>(),
            rj.split("\"class\":")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(20)
                .collect::<String>(),
        );
    }

    #[test]
    fn err_result_exposes_action_name_and_recommended_next_step() {
        // T7 contract: every error block emitted by ActionResult::err must
        // self-describe with `action_name` (the failing action_type) and
        // `recommended_next_step` (one-sentence recovery hint). This makes
        // an isolated error JSON sufficient for the LLM to pick the right
        // recovery strategy without consulting a side mapping.
        let nf = ActionResult::err("ReadFile", ErrorKind::NotFound, "no such file: ./x", 0);
        let json = serde_json::to_string(&nf).unwrap();
        assert!(
            json.contains("\"action_name\":\"ReadFile\""),
            "missing action_name: {}",
            json
        );
        assert!(
            json.contains("\"recommended_next_step\""),
            "missing recommended_next_step field: {}",
            json
        );
        // Hint MUST be class-aware — not_found must talk about verifying.
        let err = nf.error.as_ref().unwrap();
        assert_eq!(err.action_name, "ReadFile");
        assert!(
            err.recommended_next_step.to_lowercase().contains("verify"),
            "not_found hint must mention verification, got: {}",
            err.recommended_next_step
        );

        // policy_denied hint MUST forbid retry and suggest plan change.
        let pol = ActionResult::err("WriteFile", ErrorKind::PathDenied, "blocked", 0);
        let phint = pol.error.unwrap().recommended_next_step;
        assert!(
            phint.to_lowercase().contains("not retry") || phint.contains("Do NOT"),
            "policy hint must forbid retry: {}",
            phint
        );

        // parse_error hint MUST point at the <action> wrapper.
        // ValidationError class for InvalidInput — not parse. Use the enum
        // helper directly to confirm parse_error class hint shape:
        let parse_hint = ErrorClass::ParseError.recommended_next_step();
        assert!(
            parse_hint.contains("<action>") && parse_hint.contains("valid JSON"),
            "parse_error hint must mention <action> + valid JSON: {}",
            parse_hint
        );
        // and ValidationError points the LLM at the in-prompt schema.
        // DescribeAction was removed in v0.3 — recommending a
        // non-existent action was a self-defeating message.
        let validation_hint = ErrorClass::ValidationError.recommended_next_step();
        assert!(
            validation_hint.contains("schema") && validation_hint.contains("fix"),
            "validation hint must point at the schema: {}",
            validation_hint
        );
    }

    #[test]
    fn standalone_action_error_can_set_action_name_via_builder() {
        // For error paths that don't go through ActionResult::err (e.g.
        // composite primitives, manual error construction in tests), the
        // builder must let the caller stamp the action_name explicitly.
        let e = ActionError::new(ErrorKind::Timeout, "slow").with_action_name("RunCommand");
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("\"action_name\":\"RunCommand\""), "{}", j);
        assert!(j.contains("\"recommended_next_step\""), "{}", j);
        assert!(j.contains("\"class\":\"runtime_error\""), "{}", j);
        assert!(j.contains("\"retryable\":true"), "{}", j);
    }
}
