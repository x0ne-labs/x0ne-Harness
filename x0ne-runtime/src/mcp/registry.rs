use crate::action::{Action, Edit, PatchSetItem};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use x0ne_actions::SideEffectClass;

pub struct McpTool {
    pub name: String,
    /// Wire name of the `Action` variant `to_action` builds; keyed into
    /// `GENERATED_ACTIONS` for the side-effect class behind `annotations`.
    pub action: &'static str,
    pub description: String,
    pub input_schema: Value,
    pub category: String,
    pub mutable: bool,
}

impl McpTool {
    /// Side-effect class of the mapped action, from the generated catalog.
    pub fn class(&self) -> SideEffectClass {
        let meta = x0ne_actions::action_meta(self.action).unwrap_or_else(|| {
            panic!(
                "MCP tool '{}' maps to unknown action '{}'",
                self.name, self.action
            )
        });
        match meta.class {
            "ReadOnly" => SideEffectClass::ReadOnly,
            "FsWrite" => SideEffectClass::FsWrite,
            "ProcessSpawn" => SideEffectClass::ProcessSpawn,
            "Network" => SideEffectClass::Network,
            "Desktop" => SideEffectClass::Desktop,
            "Destructive" => SideEffectClass::Destructive,
            "Composite" => SideEffectClass::Composite,
            "Meta" => SideEffectClass::Meta,
            other => panic!(
                "unknown side-effect class '{other}' for action '{}'",
                self.action
            ),
        }
    }

    /// MCP tool annotations (spec 2025-03-26), derived from the class only.
    /// destructiveHint is true for Destructive alone: simulate.rs files FsWrite
    /// under paths_written and ProcessSpawn under "would spawn", never destroyed.
    pub fn annotations(&self) -> Value {
        let class = self.class();
        let read_only = matches!(class, SideEffectClass::ReadOnly | SideEffectClass::Meta);
        let mut a = json!({
            "readOnlyHint": read_only,
            "destructiveHint": class == SideEffectClass::Destructive,
            "openWorldHint": class == SideEffectClass::Network,
        });
        // Only pure reads are known idempotent without looking at arguments.
        if class == SideEffectClass::ReadOnly {
            a["idempotentHint"] = Value::Bool(true);
        }
        a
    }
}

pub struct ToolRegistry {
    tools: Vec<McpTool>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut tools = Vec::new();

        // ───────────────────────────────────────────────────────────────────────
        // FILESYSTEM
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "read_file".into(),
            action: "ReadFile",
            description: "Read the contents of a file. Set start_line for a line-oriented window whose coordinates match search results; otherwise offset/limit are byte-based.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "offset": { "type": "integer", "description": "Byte offset to start reading from" },
                    "limit": { "type": "integer", "description": "Max bytes to read" },
                    "start_line": { "type": "integer", "description": "1-based first line; switches to line mode (FileChunk result)" },
                    "limit_lines": { "type": "integer", "description": "Max lines in line mode (default 200)" }
                },
                "required": ["path"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "read_files".into(),
            action: "ReadFiles",
            description: "Read multiple files in one call. Use this instead of repeated read_file calls when gathering context.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "max_bytes_per_file": { "type": "integer" }
                },
                "required": ["paths"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "write_file".into(),
            action: "WriteFile",
            description: "Create a new file or overwrite an existing file with the given content."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "edit_file".into(),
            action: "EditFile",
            description:
                "Apply multiple search-and-replace edits to a single file in one operation.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" },
                    "create_if_missing": { "type": "boolean", "description": "Create if missing" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": { "type": "string" },
                                "new": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["old", "new"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "apply_patch".into(),
            action: "ApplyPatch",
            description: "Apply a unified diff patch to a file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" },
                    "patch": { "type": "string", "description": "Unified diff content" },
                    "reverse": { "type": "boolean", "description": "Apply in reverse to undo (default: false)" }
                },
                "required": ["path", "patch"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "stat_files".into(),
            action: "StatFiles",
            description: "Return metadata for multiple paths, optionally with sha256 hashes."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "hash": { "type": "boolean" }
                },
                "required": ["paths"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "patch_set".into(),
            action: "PatchSet",
            description:
                "Apply multiple unified-diff patches in one call. Prefer for multi-file code edits."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "patch": { "type": "string" },
                                "reverse": { "type": "boolean" }
                            },
                            "required": ["path", "patch"]
                        }
                    },
                    "stop_on_error": { "type": "boolean" }
                },
                "required": ["patches"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "move_to_trash".into(),
            action: "MoveToTrash",
            description: "Safely delete a file by moving it to the OS trash.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to trash" }
                },
                "required": ["path"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // SEARCH
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "search_code".into(),
            action: "SearchCode",
            description: "Search for one or more regex patterns across file contents under a directory. Use `patterns` (array, OR-matched via RegexSet in a single walk) for multi-needle search — saves round-trips. `pattern` (single regex) is the single-needle shortcut.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Directory to search in" },
                    "pattern": { "type": "string", "description": "Single regex pattern (use this OR `patterns`, not both)" },
                    "patterns": { "type": "array", "items": {"type": "string"}, "description": "Multiple regex patterns, OR-matched in one walk (e.g. [\"TODO\", \"FIXME\", \"XXX\"])" },
                    "file_glob": { "type": "string", "description": "Optional filename glob filter" },
                    "limit": { "type": "integer", "description": "Maximum matches" }
                },
                "required": ["root"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "search_and_read".into(),
            action: "SearchAndRead",
            description: "Search code and return matching context chunks in one call.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string" },
                    "pattern": { "type": "string" },
                    "patterns": { "type": "array", "items": { "type": "string" } },
                    "file_glob": { "type": "string" },
                    "context_lines": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "required": ["root"]
            }),
            category: "read".into(),
            mutable: false,
        });

        // ───────────────────────────────────────────────────────────────────────
        // GIT
        // ───────────────────────────────────────────────────────────────────────

        // ───────────────────────────────────────────────────────────────────────
        // PROCESS
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "run_command".into(),
            action: "RunCommand",
            description: "Run a whitelisted command. No shell — executes the binary directly. Returns exit code, stdout, stderr.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command to run (must be whitelisted)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments" },
                    "cwd": { "type": "string", "description": "Working directory (optional)" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 30)" },
                    "stream": { "type": "boolean", "description": "Start in stream mode" }
                },
                "required": ["command"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "ssh_exec".into(),
            action: "SshExec",
            description: "Run a single command on a remote host via the local OpenSSH client. Adhoc-mode fleet admin (no x0ne-runtime install on target). Inherits ~/.ssh/config (ProxyJump, IdentityFile, ControlMaster). Default-deny in policy: enable explicitly per host. Returns exit_code/stdout/stderr/timed_out. For typed + policy-controlled remote execution, deploy x0ne-runtime to the host instead.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "ssh(1) target: user@host, an alias from ~/.ssh/config, or just host" },
                    "command": { "type": "string", "description": "Single binary to run remotely (no shell metacharacters; for pipelines use command='sh', args=['-c', '<pipeline>'])" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string", "description": "Optional remote working directory" },
                    "env": { "type": "object", "additionalProperties": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "description": "ssh round-trip timeout (default 30)" },
                    "ssh_options": { "type": "array", "items": { "type": "string" }, "description": "Extra '-o KEY=VALUE' options" }
                },
                "required": ["host", "command"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "run_in_sandbox".into(),
            action: "RunInSandbox",
            description: "Execute code inside an ephemeral Podman container. \
                The container is isolated (no network by default, limited memory, auto-removed after exit). \
                Inject files into /workspace via the `files` map. \
                Image must be in policy.allowed_images. Requires Podman on the host.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "image": { "type": "string", "description": "Container image (e.g. python:3.13-slim, alpine, node:22)" },
                    "command": { "type": "array", "items": { "type": "string" }, "description": "Command to run inside the container" },
                    "files": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Files to inject into /workspace. Key = filename, value = file content."
                    },
                    "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Environment variables" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 30)" },
                    "network": { "type": "boolean", "description": "Allow network access (default: false)" },
                    "memory_mb": { "type": "integer", "description": "Memory limit in MB (default: 256)" }
                },
                "required": ["image"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "cloud_cost".into(),
            action: "CloudCost",
            description: "Report the current cloud burn rate (USD/hour and per-day) from live resource inventory. Estimated from an embedded price table; unpriced resources are listed separately, never counted as zero.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "enum": ["gcp"], "description": "Cloud provider (only gcp implemented)" },
                    "project": { "type": "string", "description": "GCP project id; omit to use the gcloud default" }
                },
                "required": ["provider"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "gpu_status".into(),
            action: "GpuStatus",
            description: "Report GPU utilization, memory, temperature and ATTACHED PROCESSES. A GPU at 0% with a process attached is a job between steps; 0% with none is idle. Omit host for local nvidia-smi.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "SSH alias for a remote host; omit for local. Remote currently runs via ssh_exec." }
                }
            }),
            category: "execute".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "safe_test".into(),
            action: "SafeTest",
            description: "Run a typed test command without shell syntax. Kinds: rust, node, python, go. Use filter to run a single test.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["rust", "node", "python", "go"] },
                    "scope": { "type": "string", "description": "For rust, use workspace for cargo test --workspace" },
                    "path": { "type": "string", "description": "Working directory" },
                    "timeout_secs": { "type": "integer" },
                    "filter": { "type": "string", "description": "Run only tests matching this name (cargo test <f> / pytest -k / go test -run)" }
                },
                "required": ["kind"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // TERMINALS (tmux on a private socket, native PTY fallback)
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "term_open".into(),
            action: "TermOpen",
            description: "Open a named terminal running a program (argv, no shell): REPL, shell, TUI, ssh, gdb, a prompt to answer, a server to watch. Several can run in parallel. Recorded (asciicast v2) and journaled. Use run_command for one-shot commands.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "[A-Za-z0-9_-]{1,32}, unique among open terminals" },
                    "command": { "type": "string", "description": "Program to run, e.g. python3, bash" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" },
                    "cols": { "type": "integer", "description": "Default 200" },
                    "rows": { "type": "integer", "description": "Default 50" }
                },
                "required": ["name", "command"]
            }),
            category: "terminal".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "term_send".into(),
            action: "TermSend",
            description: "Type into a terminal: literal text (multi-line paste allowed), then named keys in order. Keys: Enter Tab BTab Escape BSpace Space Up Down Left Right Home End PageUp PageDown Insert Delete F1-F12 C-a..C-z M-<char>. Follow with term_wait.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "text": { "type": "string" },
                    "keys": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"Enter\"], [\"C-c\"]" }
                },
                "required": ["name"]
            }),
            category: "terminal".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "term_read".into(),
            action: "TermRead",
            description: "Read a terminal: visible screen (ANSI stripped), optional scrollback lines, cursor position, exit status. diff=true returns only rows changed since the last read.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "history": { "type": "integer", "description": "Scrollback lines to include (default 0)" },
                    "diff": { "type": "boolean" }
                },
                "required": ["name"]
            }),
            category: "terminal".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "term_wait".into(),
            action: "TermWait",
            description: "Wait until a regex (multi-line) is on screen, the screen is stable for stable_ms, or the program exits (exit=true, returns exit_status). Returns condition (pattern|exit|stable|timeout) and, by default, only the rows changed since the last read.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "pattern": { "type": "string" },
                    "stable_ms": { "type": "integer" },
                    "exit": { "type": "boolean" },
                    "timeout_ms": { "type": "integer", "description": "Default 10000, capped by [term] max_wait_ms" },
                    "diff": { "type": "boolean", "description": "Default true; false returns the whole screen" }
                },
                "required": ["name"]
            }),
            category: "terminal".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "term_resize".into(),
            action: "TermResize",
            description: "Resize a terminal (columns x rows).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "cols": { "type": "integer" },
                    "rows": { "type": "integer" }
                },
                "required": ["name", "cols", "rows"]
            }),
            category: "terminal".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "term_snapshot".into(),
            action: "TermSnapshot",
            description: "Whole scrollback plus screen of a terminal, plain text or with ANSI colours (ansi=true). Bounded by [term] max_read_bytes, tail kept.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "ansi": { "type": "boolean" }
                },
                "required": ["name"]
            }),
            category: "terminal".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "term_list".into(),
            action: "TermList",
            description: "List open terminals: command, backend, pid, size, alive, exit status, recording path.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "terminal".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "term_close".into(),
            action: "TermClose",
            description: "Close a terminal and kill what runs in it. Returns the recording path."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
            category: "terminal".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // LOGS
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "read_logs".into(),
            action: "ReadLogs",
            description: "Read the last N lines of a log file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the log file" },
                    "lines": { "type": "integer", "description": "Number of lines from the end (default: 100)" }
                },
                "required": ["path"]
            }),
            category: "read".into(),
            mutable: false,
        });

        // ───────────────────────────────────────────────────────────────────────
        // HUMAN INTERACTION
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "ask_human".into(),
            action: "AskHuman",
            description: "Ask the user a question and wait for their response.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "Question to ask the user" }
                },
                "required": ["question"]
            }),
            category: "human".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "confirm".into(),
            action: "Confirm",
            description: "Ask the user for yes/no confirmation before proceeding.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "What to confirm" }
                },
                "required": ["message"]
            }),
            category: "human".into(),
            mutable: false,
        });

        // ───────────────────────────────────────────────────────────────────────
        // PIPELINE (Skip for MCP conceptually - can use sequential tool calls - but mapping for 1:1 parity)
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "pipeline".into(),
            action: "Pipeline",
            description: "Execute multiple actions in a single request. Set parallel=true for concurrent read-only steps.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": { "type": "array", "items": { "type": "object" } },
                    "stop_on_error": { "type": "boolean", "description": "Stop on first failed step (default true, ignored when parallel)" },
                    "snapshot_before": { "type": "string" },
                    "parallel": { "type": "boolean", "description": "Execute all steps concurrently (default false)" }
                },
                "required": ["steps"]
            }),
            category: "meta".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "safe_change".into(),
            action: "SafeChange",
            description: "Execute change steps, then verification steps, with automatic snapshot rollback on failure.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": { "type": "array", "items": { "type": "object" }, "description": "Actions that implement the change" },
                    "tests": { "type": "array", "items": { "type": "object" }, "description": "Actions that verify the change" },
                    "rollback_on_fail": { "type": "boolean", "description": "Rollback if any change or test step fails (default true)" },
                    "label": { "type": "string", "description": "Short label used in the auto snapshot name" },
                    "keep_snapshot": { "type": "boolean", "description": "Keep the snapshot after success (default false)" }
                },
                "required": ["steps"]
            }),
            category: "meta".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // SNAPSHOTS
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "create_snapshot".into(),
            action: "CreateSnapshot",
            description: "Save a snapshot of files for later rollback. Use before risky modifications.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Snapshot name (alphanumeric, hyphens, underscores)" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Files/dirs to snapshot" }
                },
                "required": ["name", "paths"]
            }),
            category: "meta".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "restore_snapshot".into(),
            action: "RestoreSnapshot",
            description: "Restore files from a previously saved snapshot.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Snapshot name to restore" }
                },
                "required": ["name"]
            }),
            category: "meta".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "list_snapshots".into(),
            action: "ListSnapshots",
            description: "List all saved snapshots.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "meta".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "delete_snapshot".into(),
            action: "DeleteSnapshot",
            description: "Delete a named snapshot.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }),
            category: "meta".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // ARTIFACTS — browser-reachable web artifacts served via x0ne-artifact-proxy.
        //
        // Pattern: artifact_create → write_file (×N under root_path) → optionally
        // artifact_serve (for service-kind). The proxy is auto-launched at
        // 127.0.0.1:7800 ; URL is `http://localhost:7800/a/<id>/`. To register
        // with the host's reverse-proxy from inside a sandbox, use the
        // orchestrator's gateway tool `x0ne_artifact_expose` (privilege boundary
        // — runtime cannot expose a host port).
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "artifact_create".into(),
            action: "ArtifactCreate",
            description: "Allocate a fresh web artifact (a self-contained directory served at http://localhost:7800/a/<id>/). Returns {id, root_path, url}; use root_path as prefix for subsequent write_file calls. Auto-launches the artifact-proxy daemon if not running.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Human-friendly name (shown in artifact_list)" },
                    "kind": { "type": "string", "enum": ["static", "service"], "description": "static (default): plain files. service: reverse-proxied to a long-running process started via artifact_serve." },
                    "persistent": { "type": "boolean", "description": "If true, save immediately to the persistent store instead of per-session ephemeral. Default false." },
                    "session_id": { "type": "string", "description": "Optional session id for ephemeral scoping (falls back to env X0NE_SESSION_ID then 'orphan')." }
                }
            }),
            category: "artifact".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "artifact_list".into(),
            action: "ArtifactList",
            description: "List every known artifact (ephemeral + saved + running) with state, age, size and URL.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "artifact".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "artifact_serve".into(),
            action: "ArtifactServe",
            description: "Attach a long-running service to a service-kind artifact and start it. The proxy reverse-proxies /a/<id>/ to the chosen port. $PORT is substituted with the auto-allocated port if `port` is omitted.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id":      { "type": "string", "description": "Artifact id returned by artifact_create" },
                    "command": { "type": "string", "description": "Command launched in the artifact root, as an argv split on whitespace (e.g. 'python3 -m http.server $PORT'). This is not a shell line: the metacharacters ; & | $ ` ( ) < > \" ' \\ are refused, and $PORT is the only substitution. Put anything more complex in a script inside the artifact." },
                    "port":    { "type": "integer", "description": "Override the auto-allocated port (default: picked in 7810-7899)" },
                    "env":     { "type": "object", "description": "Extra environment variables", "additionalProperties": { "type": "string" } }
                },
                "required": ["id", "command"]
            }),
            category: "artifact".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "artifact_save".into(),
            action: "ArtifactSave",
            description: "Promote an ephemeral artifact to the persistent (saved/) store. Files moved atomically on disk; registry updated. Optionally rename in the same call.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id":   { "type": "string", "description": "Artifact id returned by artifact_create" },
                    "name": { "type": "string", "description": "Optional new name to apply during the promotion" }
                },
                "required": ["id"]
            }),
            category: "artifact".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "artifact_stop".into(),
            action: "ArtifactStop",
            description: "Stop the backing service of a service-kind artifact without deleting files. No-op for static artifacts.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Artifact id" }
                },
                "required": ["id"]
            }),
            category: "artifact".into(),
            mutable: true,
        });

        tools.push(McpTool {
            name: "artifact_destroy".into(),
            action: "ArtifactDestroy",
            description: "Permanently remove an artifact: stop its service, delete its files, drop from registry. Use force=true to bypass the persistent guard.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id":    { "type": "string", "description": "Artifact id" },
                    "force": { "type": "boolean", "description": "Bypass the persistent-guard refusal. Default false." }
                },
                "required": ["id"]
            }),
            category: "artifact".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // HANDLES
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "subscribe".into(),
            action: "Subscribe",
            description: "Subscribe to a machine event. Returns a handle.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "event_type": { "type": "object" },
                    "config": { "type": "object" }
                },
                "required": ["event_type"]
            }),
            category: "meta".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "check_handle".into(),
            action: "CheckHandle",
            description: "Drain buffered events from a handle.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" }
                },
                "required": ["handle"]
            }),
            category: "meta".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "stop_handle".into(),
            action: "StopHandle",
            description: "Stop and cancel a running handle.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" }
                },
                "required": ["handle"]
            }),
            category: "meta".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "list_handles".into(),
            action: "ListHandles",
            description: "List all active and recent handles.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "meta".into(),
            mutable: false,
        });

        // ───────────────────────────────────────────────────────────────────────
        // CAPABILITY
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "policy_check".into(),
            action: "PolicyCheck",
            description: "Check if an action would be allowed by policy (without executing it)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "object" }
                },
                "required": ["action"]
            }),
            category: "meta".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "simulate".into(),
            action: "Simulate",
            description: "Dry-run an Action (often a Pipeline / Try / SafeChange) without executing it. Walks the tree, classifies each leaf (read_only / fs_write / process_spawn / network / desktop / destructive), asks policy for the verdict per step, and returns an aggregate projection of what would change. Zero mutations.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target":  { "type": "object", "description": "The action to simulate (any Action shape)." },
                    "explain": { "type": "boolean", "description": "Include per-step human-readable summaries (default: true)." }
                },
                "required": ["target"]
            }),
            category: "safety".into(),
            mutable: false,
        });

        // Scripted functions — Tier 1 of agent-extensible primitives. The
        // four meta-tools below are the *control plane*: define/list/delete.
        // Each defined function is *also* exposed as its own MCP tool by the
        // orchestrator gateway (`x0ne_fn_<name>`), so other agents can call
        // it as if it were a built-in primitive.
        tools.push(McpTool {
            name: "define_function".into(),
            action: "DefineFunction",
            description: "Define a reusable scripted function (Rhai today, WASM later). Source must define `fn run(<params>)`. Persisted in ~/.x0ne/functions/. Once defined, the function is callable via `call_function` AND exposed as `x0ne_fn_<name>` to other agents.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "snake_case, [a-z][a-z0-9_]{2,63}" },
                    "description": { "type": "string" },
                    "tier": { "type": "string", "enum": ["rhai", "wasm"], "description": "default: rhai" },
                    "source": { "type": "string", "description": "Source code; must define `fn run(...)` for Rhai" },
                    "params": {
                        "type": "array",
                        "description": "Ordered parameter list; either bare names or {name, type} objects",
                        "items": { "oneOf": [ { "type": "string" }, { "type": "object" } ] }
                    },
                    "returns": { "type": "string", "description": "Doc-only return type hint" }
                },
                "required": ["name", "source"]
            }),
            category: "meta".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "call_function".into(),
            action: "CallFunction",
            description: "Invoke a scripted function by name. Args are positional (in the order declared at define_function time). Returns the function's value as JSON, plus duration_us.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "args": { "type": "array" }
                },
                "required": ["name"]
            }),
            category: "meta".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "list_functions".into(),
            action: "ListFunctions",
            description: "List every scripted function defined in this X0NE_HOME (compact catalog: name, description, tier, params, source_hash; not the full source).".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "meta".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "delete_function".into(),
            action: "DeleteFunction",
            description: "Remove a scripted function by name.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }),
            category: "meta".into(),
            mutable: true,
        });

        // SECURITY: the MCP category is a taxonomy distinct from the
        // `x0ne-actions` classes (values in use are "read", "modify",
        // "execute", "human", "meta", "artifact", "safety"). It must say the
        // same thing as the class, otherwise an MCP client that filters on
        // category sees a write action filed with the metadata ones.
        // `run_program` writes files → "modify", like `write_file`
        // (:76) and `edit_file` (:103). `preview_program` writes nothing → "read",
        // like `read_file` (:44).
        tools.push(McpTool {
            name: "preview_program".into(),
            action: "PreviewProgram",
            description: "Preview one asm-ai program. X0NE compiles it, derives the \
                          files it can read and write from the compiled module, \
                          proves each of those paths against the policy, runs it \
                          without writing anything, and returns a diff plus the \
                          proof. Nothing changes on disk. The program is a \
                          four-line pipeline: `from files` … optional `where` … one \
                          verb … `emit`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "asm-ai source, first line `#!asm-ai/v0`"
                    }
                },
                "required": ["source"]
            }),
            category: "read".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "run_program".into(),
            action: "RunProgram",
            description: "Apply the program that `preview_program` just previewed. \
                          Pass the same source. X0NE commits the transaction the \
                          preview left pending — it does not run the module again, \
                          because that would make the approved diff a lie. With no \
                          matching preview, this is refused, never replayed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "the exact asm-ai source that was previewed"
                    }
                },
                "required": ["source"]
            }),
            category: "modify".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // SYSTEM
        // ───────────────────────────────────────────────────────────────────────

        tools.push(McpTool {
            name: "file_info".into(),
            action: "FileInfo",
            description: "Get detailed metadata about a file: size, permissions, timestamps, type."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" }
                },
                "required": ["path"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "checksum".into(),
            action: "Checksum",
            description: "Compute a hash of a file (sha256, md5, or sha1).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" },
                    "algorithm": { "type": "string", "description": "Hash algorithm: sha256 (default), md5, sha1" }
                },
                "required": ["path"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "list_processes".into(),
            action: "ListProcesses",
            description: "List running processes with name, PID, CPU, and memory usage.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "description": "Filter processes by name (optional)" }
                },
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "port_check".into(),
            action: "PortCheck",
            description: "Check if a TCP port is open on a host.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Hostname or IP" },
                    "port": { "type": "integer", "description": "Port number" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["host", "port"]
            }),
            category: "read".into(),
            mutable: false,
        });

        tools.push(McpTool {
            name: "dns_lookup".into(),
            action: "DnsLookup",
            description: "Resolve a hostname to IP addresses.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "hostname": { "type": "string", "description": "Hostname to resolve" }
                },
                "required": ["hostname"]
            }),
            category: "read".into(),
            mutable: false,
        });

        // ───────────────────────────────────────────────────────────────────────
        // DESKTOP (Wayland/Sway)
        // ───────────────────────────────────────────────────────────────────────
        tools.push(McpTool {
            name: "screenshot".into(),
            action: "Screenshot",
            description: "Take a screenshot of the desktop or a region.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string" },
                    "region": { "type": "string" }
                }
            }),
            category: "read".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "get_window_tree".into(),
            action: "GetWindowTree",
            description: "Get the Sway window tree as a flattened list.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "read".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "get_focused_window".into(),
            action: "GetFocusedWindow",
            description: "Get the currently focused window.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "read".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "move_mouse".into(),
            action: "MoveMouse",
            description: "Move the mouse pointer to an absolute position.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" }
                },
                "required": ["x", "y"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "click".into(),
            action: "Click",
            description: "Click a mouse button (optionally moving to x,y first).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string" }
                }
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "double_click".into(),
            action: "DoubleClick",
            description: "Double-click (optionally moving to x,y first).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" }
                }
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "scroll".into(),
            action: "Scroll",
            description: "Scroll the mouse wheel.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "direction": { "type": "string" },
                    "amount": { "type": "integer" }
                },
                "required": ["direction"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "type_text".into(),
            action: "TypeText",
            description: "Type text at the current cursor position.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "press_key".into(),
            action: "PressKey",
            description: "Press a single key by name (e.g. Return, Escape, Tab).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string" }
                },
                "required": ["key"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "press_key_combination".into(),
            action: "PressKeyCombination",
            description: "Press a key combination (e.g. [ctrl, s]).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "keys": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["keys"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "focus_window".into(),
            action: "FocusWindow",
            description: "Focus a window by title or app_id substring.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "match_str": { "type": "string" }
                },
                "required": ["match_str"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "send_notification".into(),
            action: "SendNotification",
            description: "Send a desktop notification.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "urgency": { "type": "string" }
                },
                "required": ["title", "body"]
            }),
            category: "execute".into(),
            mutable: true,
        });
        tools.push(McpTool {
            name: "get_clipboard".into(),
            action: "GetClipboard",
            description: "Read the Wayland clipboard.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            category: "read".into(),
            mutable: false,
        });
        tools.push(McpTool {
            name: "set_clipboard".into(),
            action: "SetClipboard",
            description: "Write to the Wayland clipboard.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
                },
                "required": ["content"]
            }),
            category: "execute".into(),
            mutable: true,
        });

        // ───────────────────────────────────────────────────────────────────────
        // Math / String tools removed in v0.3 — use shell.

        // Deterministic tools/list: clients diff the catalog across calls.
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        ToolRegistry { tools }
    }

    pub fn list_tools(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "annotations": t.annotations(),
                })
            })
            .collect()
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn to_action(&self, name: &str, args: &Value) -> Result<Action> {
        match name {
            // Filesystem
            "read_file" => Ok(Action::ReadFile {
                path: string_arg(args, "path")?,
                offset: opt_uint(args, "offset").map(|v| v as u64).unwrap_or(0),
                limit: opt_uint(args, "limit").map(|v| v as u64),
                start_line: opt_uint(args, "start_line"),
                limit_lines: opt_uint(args, "limit_lines"),
            }),
            "read_files" => Ok(Action::ReadFiles {
                paths: string_array_arg(args, "paths")?,
                max_bytes_per_file: opt_uint(args, "max_bytes_per_file").map(|v| v as u64),
            }),
            "write_file" => Ok(Action::WriteFile {
                path: string_arg(args, "path")?,
                content: string_arg(args, "content")?,
            }),
            "edit_file" => {
                let edits_val = args
                    .get("edits")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| anyhow!("missing edits array"))?;
                let mut parsed_edits = Vec::new();
                for e in edits_val {
                    parsed_edits.push(Edit {
                        old: string_arg(e, "old")?,
                        new: string_arg(e, "new")?,
                        replace_all: opt_bool(e, "replace_all"),
                    });
                }
                Ok(Action::EditFile {
                    path: string_arg(args, "path")?,
                    edits: parsed_edits,
                    create_if_missing: opt_bool(args, "create_if_missing"),
                })
            }
            "apply_patch" => Ok(Action::ApplyPatch {
                path: string_arg(args, "path")?,
                patch: string_arg(args, "patch")?,
                reverse: opt_bool(args, "reverse"),
            }),
            "stat_files" => Ok(Action::StatFiles {
                paths: string_array_arg(args, "paths")?,
                hash: opt_bool(args, "hash"),
            }),
            "patch_set" => {
                let patches_val = args
                    .get("patches")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| anyhow!("missing patches array"))?;
                let mut patches = Vec::new();
                for p in patches_val {
                    patches.push(PatchSetItem {
                        path: string_arg(p, "path")?,
                        patch: string_arg(p, "patch")?,
                        reverse: opt_bool(p, "reverse"),
                    });
                }
                Ok(Action::PatchSet {
                    patches,
                    stop_on_error: opt_bool(args, "stop_on_error"),
                })
            }
            "move_to_trash" => Ok(Action::MoveToTrash {
                path: string_arg(args, "path")?,
            }),

            // Search
            "search_code" => Ok(Action::SearchCode {
                root: string_arg(args, "root")?,
                pattern: opt_string(args, "pattern").unwrap_or_default(),
                patterns: opt_string_array(args, "patterns").unwrap_or_default(),
                file_glob: opt_string(args, "file_glob"),
                limit: opt_uint(args, "limit"),
            }),
            "search_and_read" => Ok(Action::SearchAndRead {
                root: string_arg(args, "root")?,
                pattern: opt_string(args, "pattern").unwrap_or_default(),
                patterns: opt_string_array(args, "patterns").unwrap_or_default(),
                file_glob: opt_string(args, "file_glob"),
                context_lines: opt_uint(args, "context_lines"),
                limit: opt_uint(args, "limit"),
            }),

            // Git

            // Terminals
            "term_open" => Ok(Action::TermOpen {
                name: string_arg(args, "name")?,
                command: string_arg(args, "command")?,
                args: opt_string_array(args, "args").unwrap_or_default(),
                cwd: opt_string(args, "cwd"),
                cols: opt_u16(args, "cols")?,
                rows: opt_u16(args, "rows")?,
            }),
            "term_send" => Ok(Action::TermSend {
                name: string_arg(args, "name")?,
                text: opt_string(args, "text"),
                keys: opt_string_array(args, "keys").unwrap_or_default(),
            }),
            "term_read" => Ok(Action::TermRead {
                name: string_arg(args, "name")?,
                history: opt_uint(args, "history"),
                diff: opt_bool(args, "diff").unwrap_or(false),
            }),
            "term_wait" => Ok(Action::TermWait {
                name: string_arg(args, "name")?,
                pattern: opt_string(args, "pattern"),
                stable_ms: opt_uint(args, "stable_ms").map(|v| v as u64),
                exit: opt_bool(args, "exit").unwrap_or(false),
                timeout_ms: opt_uint(args, "timeout_ms").map(|v| v as u64),
                diff: opt_bool(args, "diff").unwrap_or(true),
            }),
            "term_resize" => Ok(Action::TermResize {
                name: string_arg(args, "name")?,
                cols: opt_u16(args, "cols")?.ok_or_else(|| anyhow!("missing argument: cols"))?,
                rows: opt_u16(args, "rows")?.ok_or_else(|| anyhow!("missing argument: rows"))?,
            }),
            "term_snapshot" => Ok(Action::TermSnapshot {
                name: string_arg(args, "name")?,
                ansi: opt_bool(args, "ansi").unwrap_or(false),
            }),
            "term_list" => Ok(Action::TermList),
            "term_close" => Ok(Action::TermClose {
                name: string_arg(args, "name")?,
            }),

            // Process
            "run_command" => {
                let env_map = if let Some(e) = args.get("env").and_then(|v| v.as_object()) {
                    let mut m = std::collections::HashMap::new();
                    for (k, v) in e {
                        if let Some(s) = v.as_str() {
                            m.insert(k.clone(), s.to_string());
                        }
                    }
                    m
                } else {
                    std::collections::HashMap::new()
                };

                Ok(Action::RunCommand {
                    command: string_arg(args, "command")?,
                    args: opt_string_array(args, "args").unwrap_or_default(),
                    cwd: opt_string(args, "cwd"),
                    env: env_map,
                    timeout_secs: opt_uint(args, "timeout_secs").map(|v| v as u64),
                    stream: opt_bool(args, "stream"),
                })
            }
            "ssh_exec" => {
                let env_map = if let Some(e) = args.get("env").and_then(|v| v.as_object()) {
                    e.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };
                Ok(Action::SshExec {
                    host: string_arg(args, "host")?,
                    command: string_arg(args, "command")?,
                    args: opt_string_array(args, "args").unwrap_or_default(),
                    cwd: opt_string(args, "cwd"),
                    env: env_map,
                    timeout_secs: opt_uint(args, "timeout_secs").map(|v| v as u64),
                    ssh_options: opt_string_array(args, "ssh_options").unwrap_or_default(),
                })
            }
            "run_in_sandbox" => {
                let files = if let Some(obj) = args.get("files").and_then(|v| v.as_object()) {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };
                let env_map = if let Some(e) = args.get("env").and_then(|v| v.as_object()) {
                    e.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };
                Ok(Action::RunInSandbox {
                    image: string_arg(args, "image")?,
                    command: opt_string_array(args, "command").unwrap_or_default(),
                    files,
                    env: env_map,
                    timeout_secs: opt_uint(args, "timeout_secs").map(|v| v as u64),
                    network: opt_bool(args, "network"),
                    memory_mb: opt_uint(args, "memory_mb").map(|v| v as u64),
                    cpu_limit: opt_uint(args, "cpu_limit").map(|v| v as u32),
                    collect_files: opt_string_array(args, "collect_files").unwrap_or_default(),
                })
            }
            "cloud_cost" => Ok(Action::CloudCost {
                provider: string_arg(args, "provider").unwrap_or_else(|_| "gcp".to_string()),
                project: opt_string(args, "project"),
            }),
            "gpu_status" => Ok(Action::GpuStatus {
                host: opt_string(args, "host"),
            }),
            "safe_test" => Ok(Action::SafeTest {
                kind: string_arg(args, "kind")?,
                scope: opt_string(args, "scope"),
                path: opt_string(args, "path"),
                timeout_secs: opt_uint(args, "timeout_secs").map(|v| v as u64),
                filter: opt_string(args, "filter"),
            }),

            // Logs
            "read_logs" => Ok(Action::ReadLogs {
                path: string_arg(args, "path")?,
                lines: opt_uint(args, "lines"),
            }),

            // Human
            "ask_human" => Ok(Action::AskHuman {
                question: string_arg(args, "question")?,
            }),
            "confirm" => Ok(Action::Confirm {
                message: string_arg(args, "message")?,
            }),

            // Pipeline
            "pipeline" => {
                let steps_val = args
                    .get("steps")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| anyhow!("missing steps array"))?;
                let mut parsed_steps = Vec::new();
                for (i, s) in steps_val.iter().enumerate() {
                    let act = self
                        .parse_step(s)
                        .map_err(|e| anyhow!("step {}: {}", i, e))?;
                    parsed_steps.push(act);
                }
                Ok(Action::Pipeline {
                    steps: parsed_steps,
                    stop_on_error: opt_bool(args, "stop_on_error").unwrap_or(true),
                    snapshot_before: opt_string(args, "snapshot_before"),
                    parallel: opt_bool(args, "parallel").unwrap_or(false),
                })
            }
            "safe_change" => {
                let steps_val = args
                    .get("steps")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| anyhow!("missing steps array"))?;
                let mut parsed_steps = Vec::new();
                for (i, s) in steps_val.iter().enumerate() {
                    let act = self
                        .parse_step(s)
                        .map_err(|e| anyhow!("step {}: {}", i, e))?;
                    parsed_steps.push(act);
                }

                let mut parsed_tests = Vec::new();
                if let Some(tests_val) = args.get("tests").and_then(|e| e.as_array()) {
                    for (i, s) in tests_val.iter().enumerate() {
                        let act = self
                            .parse_step(s)
                            .map_err(|e| anyhow!("test {}: {}", i, e))?;
                        parsed_tests.push(act);
                    }
                }

                Ok(Action::SafeChange {
                    steps: parsed_steps,
                    tests: parsed_tests,
                    rollback_on_fail: opt_bool(args, "rollback_on_fail").unwrap_or(true),
                    label: opt_string(args, "label"),
                    keep_snapshot: opt_bool(args, "keep_snapshot").unwrap_or(false),
                })
            }

            // Snapshots
            "create_snapshot" => Ok(Action::CreateSnapshot {
                name: string_arg(args, "name")?,
                paths: opt_string_array(args, "paths").unwrap_or_default(),
            }),
            "restore_snapshot" => Ok(Action::RestoreSnapshot {
                name: string_arg(args, "name")?,
            }),
            "list_snapshots" => Ok(Action::ListSnapshots),
            "delete_snapshot" => Ok(Action::DeleteSnapshot {
                name: string_arg(args, "name")?,
            }),

            // Artifacts (web artifacts served via x0ne-artifact-proxy)
            "artifact_create" => Ok(Action::ArtifactCreate {
                name: opt_string(args, "name"),
                kind: opt_string(args, "kind"),
                persistent: opt_bool(args, "persistent"),
                session_id: opt_string(args, "session_id"),
            }),
            "artifact_list" => Ok(Action::ArtifactList),
            "artifact_serve" => Ok(Action::ArtifactServe {
                id: string_arg(args, "id")?,
                command: string_arg(args, "command")?,
                port: opt_uint(args, "port").map(|v| v as u16),
                env: args
                    .get("env")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "artifact_save" => Ok(Action::ArtifactSave {
                id: string_arg(args, "id")?,
                name: opt_string(args, "name"),
            }),
            "artifact_stop" => Ok(Action::ArtifactStop {
                id: string_arg(args, "id")?,
            }),
            "artifact_destroy" => Ok(Action::ArtifactDestroy {
                id: string_arg(args, "id")?,
                force: opt_bool(args, "force"),
            }),

            // Handles
            "subscribe" => Ok(Action::Subscribe {
                event_type: serde_json::from_value(
                    args.get("event_type").cloned().unwrap_or(json!({})),
                )?,
                config: args.get("config").cloned().unwrap_or(json!({})),
            }),
            "check_handle" => Ok(Action::CheckHandle {
                handle: string_arg(args, "handle")?,
            }),
            "stop_handle" => Ok(Action::StopHandle {
                handle: string_arg(args, "handle")?,
            }),
            "list_handles" => Ok(Action::ListHandles),

            // Capability
            "policy_check" => Ok(Action::PolicyCheck {
                action: args.get("action").cloned().unwrap_or(json!({})),
            }),
            "simulate" => {
                let target = args.get("target").cloned().unwrap_or(json!({}));
                let target_action: Action = serde_json::from_value(target).map_err(|e| {
                    anyhow::anyhow!("simulate: cannot parse 'target' as Action: {}", e)
                })?;
                Ok(Action::Simulate {
                    target: Box::new(target_action),
                    explain: args
                        .get("explain")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                })
            }

            // Scripted functions (Tier 1: Rhai)
            "define_function" => Ok(Action::DefineFunction {
                name: string_arg(args, "name")?,
                description: opt_string(args, "description"),
                tier: opt_string(args, "tier"),
                source: string_arg(args, "source")?,
                params: args
                    .get("params")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                returns: opt_string(args, "returns"),
            }),
            "call_function" => Ok(Action::CallFunction {
                name: string_arg(args, "name")?,
                args: args
                    .get("args")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            }),
            "list_functions" => Ok(Action::ListFunctions),
            "delete_function" => Ok(Action::DeleteFunction {
                name: string_arg(args, "name")?,
            }),
            "preview_program" => Ok(Action::PreviewProgram {
                source: string_arg(args, "source")?,
            }),
            "run_program" => Ok(Action::RunProgram {
                source: string_arg(args, "source")?,
            }),

            // Declared in the catalog but never mapped, so a client that
            // discovered `file_info` and called it got `unknown tool`.
            "file_info" => Ok(Action::FileInfo {
                path: string_arg(args, "path")?,
            }),

            // System
            "checksum" => Ok(Action::Checksum {
                path: string_arg(args, "path")?,
                algorithm: opt_string(args, "algorithm"),
            }),
            "list_processes" => Ok(Action::ListProcesses {
                filter: opt_string(args, "filter"),
            }),
            "port_check" => Ok(Action::PortCheck {
                host: string_arg(args, "host")?,
                port: uint_arg(args, "port")? as u16,
                timeout_secs: opt_uint(args, "timeout_secs").map(|v| v as u64),
            }),
            "dns_lookup" => Ok(Action::DnsLookup {
                hostname: string_arg(args, "hostname")?,
            }),

            // Desktop
            "screenshot" => Ok(Action::Screenshot {
                output: opt_string(args, "output"),
                region: opt_string(args, "region"),
            }),
            "get_window_tree" => Ok(Action::GetWindowTree),
            "get_focused_window" => Ok(Action::GetFocusedWindow),
            "move_mouse" => Ok(Action::MoveMouse {
                x: uint_arg(args, "x")? as i32,
                y: uint_arg(args, "y")? as i32,
            }),
            "click" => Ok(Action::Click {
                x: opt_uint(args, "x").map(|v| v as i32),
                y: opt_uint(args, "y").map(|v| v as i32),
                button: opt_string(args, "button"),
            }),
            "double_click" => Ok(Action::DoubleClick {
                x: opt_uint(args, "x").map(|v| v as i32),
                y: opt_uint(args, "y").map(|v| v as i32),
            }),
            "scroll" => Ok(Action::Scroll {
                direction: string_arg(args, "direction")?,
                amount: opt_uint(args, "amount").map(|v| v as i32),
            }),
            "type_text" => Ok(Action::TypeText {
                text: string_arg(args, "text")?,
            }),
            "press_key" => Ok(Action::PressKey {
                key: string_arg(args, "key")?,
            }),
            "press_key_combination" => Ok(Action::PressKeyCombination {
                keys: opt_string_array(args, "keys").unwrap_or_default(),
            }),
            "focus_window" => Ok(Action::FocusWindow {
                match_str: string_arg(args, "match_str")?,
            }),
            "send_notification" => Ok(Action::SendNotification {
                title: string_arg(args, "title")?,
                body: string_arg(args, "body")?,
                urgency: opt_string(args, "urgency"),
            }),
            "get_clipboard" => Ok(Action::GetClipboard),
            "set_clipboard" => Ok(Action::SetClipboard {
                content: string_arg(args, "content")?,
            }),

            // Math

            // String
            _ => Err(anyhow!("unknown tool: {}", name)),
        }
    }

    /// Parse a single Pipeline step.
    ///
    /// Accepts two shapes:
    ///   1. Canonical Action JSON — `{"type": "Add", "a": 1, "b": 2}` (PascalCase).
    ///   2. MCP-tool shape — `{"type": "add", "a": 1, "b": 2}` (snake_case),
    ///      same wire format as a direct `tools/call`. Routed through
    ///      `to_action` so the schema & arg coercion stay 1:1.
    ///
    /// The dispatch is by the first char of `type`: lowercase → MCP path,
    /// uppercase → serde path. We never silently mix the two (e.g. snake_case
    /// keys with PascalCase `type`) — the caller picks one shape per step.
    fn parse_step(&self, s: &Value) -> Result<Action> {
        let type_str = s
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("step missing `type` field"))?;
        let first = type_str
            .chars()
            .next()
            .ok_or_else(|| anyhow!("step `type` is empty"))?;

        if first.is_ascii_lowercase() {
            // MCP-tool shape — route through to_action() for parity with direct calls.
            self.to_action(type_str, s)
        } else {
            // Canonical Action JSON — let serde do its thing.
            serde_json::from_value::<Action>(s.clone()).map_err(|e| anyhow!("{}", e))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn string_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing or invalid string argument: {}", key))
}

fn string_array_arg(args: &Value, key: &str) -> Result<Vec<String>> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| anyhow!("missing or invalid string array argument: {}", key))
}

fn uint_arg(args: &Value, key: &str) -> Result<usize> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .ok_or_else(|| anyhow!("missing or invalid unsigned integer argument: {}", key))
}

fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn opt_uint(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|v| v as usize)
}

fn opt_u16(args: &Value, key: &str) -> Result<Option<u16>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be an integer between 0 and 65535")),
    }
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

fn opt_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_list_tools_count() {
        let registry = ToolRegistry::new();
        let list = registry.list_tools();
        assert_eq!(list.len(), registry.tool_count());
        // Currently there are roughly 48 actions map to tools
        assert!(registry.tool_count() >= 40);
    }

    #[test]
    fn test_to_action_read_file() {
        let registry = ToolRegistry::new();
        let args = json!({
            "path": "./test.txt"
        });

        let action = registry.to_action("read_file", &args).unwrap();
        match action {
            Action::ReadFile {
                path,
                offset,
                limit,
                ..
            } => {
                assert_eq!(path, "./test.txt");
                assert_eq!(offset, 0);
                assert_eq!(limit, None);
            }
            _ => panic!("Expected ReadFile action"),
        }
    }

    #[test]
    fn test_to_action_run_command() {
        let registry = ToolRegistry::new();
        let args = json!({
            "command": "cargo",
            "args": ["test", "--workspace"]
        });

        let action = registry.to_action("run_command", &args).unwrap();
        match action {
            Action::RunCommand {
                command,
                args: cmd_args,
                ..
            } => {
                assert_eq!(command, "cargo");
                assert_eq!(cmd_args, vec!["test", "--workspace"]);
            }
            _ => panic!("Expected RunCommand action"),
        }
    }

    // write_file_lines / append_lines / math-pipeline tests removed
    // along with the underlying primitives.

    #[test]
    fn pipeline_step_pascal_and_snake_case() {
        let r = ToolRegistry::new();
        let pascal = r
            .parse_step(&json!({"type": "ReadFile", "path": "/tmp/y"}))
            .unwrap();
        let snake = r
            .parse_step(&json!({"type": "read_file", "path": "/tmp/y"}))
            .unwrap();
        assert!(matches!(pascal, Action::ReadFile { ref path, .. } if path == "/tmp/y"));
        assert!(matches!(snake, Action::ReadFile { ref path, .. } if path == "/tmp/y"));
    }

    #[test]
    fn pipeline_step_snake_case_complex_action() {
        let r = ToolRegistry::new();
        let act = r
            .parse_step(&json!({"type": "read_file", "path": "/tmp/x"}))
            .unwrap();
        assert!(
            matches!(act, Action::ReadFile { ref path, offset: 0, limit: None, .. } if path == "/tmp/x")
        );
    }

    #[test]
    fn registry_builds_cloud_actions() {
        let r = ToolRegistry::new();
        let cost = r
            .to_action("cloud_cost", &json!({"provider": "gcp"}))
            .unwrap();
        assert!(matches!(cost, Action::CloudCost { .. }));
        let gpu = r.to_action("gpu_status", &json!({})).unwrap();
        assert!(matches!(gpu, Action::GpuStatus { host: None }));
    }

    #[test]
    fn cloud_tools_are_exposed_in_the_schema() {
        // to_action working is not enough: the tool must also appear in the
        // advertised schema, or the model never learns it exists.
        let r = ToolRegistry::new();
        let names: Vec<&str> = r.tools().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"cloud_cost"), "cloud_cost not advertised");
        assert!(names.contains(&"gpu_status"), "gpu_status not advertised");

        let cost = r.tools().iter().find(|t| t.name == "cloud_cost").unwrap();
        // provider is required and enumerated so the model can't invent one.
        assert_eq!(cost.input_schema["required"][0], "provider");
        assert_eq!(
            cost.input_schema["properties"]["provider"]["enum"][0],
            "gcp"
        );
    }

    #[test]
    fn cloud_cost_defaults_provider_to_gcp_when_omitted() {
        // Robustness: a model that forgets `provider` still resolves to gcp
        // rather than erroring at the registry boundary.
        let r = ToolRegistry::new();
        let cost = r.to_action("cloud_cost", &json!({})).unwrap();
        assert!(matches!(cost, Action::CloudCost { provider, .. } if provider == "gcp"));
    }

    #[test]
    fn pipeline_step_in_pipeline_call() {
        let r = ToolRegistry::new();
        let act = r
            .to_action(
                "pipeline",
                &json!({
                    "steps": [
                        {"type": "read_file", "path": "/tmp/a"},
                        {"type": "ReadFile", "path": "/tmp/b"},
                    ]
                }),
            )
            .unwrap();
        if let Action::Pipeline { steps, .. } = act {
            assert_eq!(steps.len(), 2);
            assert!(matches!(&steps[0], Action::ReadFile { path, .. } if path == "/tmp/a"));
            assert!(matches!(&steps[1], Action::ReadFile { path, .. } if path == "/tmp/b"));
        } else {
            panic!("expected Pipeline");
        }
    }

    #[test]
    fn safe_change_parses_steps_and_tests() {
        let r = ToolRegistry::new();
        let act = r
            .to_action(
                "safe_change",
                &json!({
                    "steps": [
                        {"type": "write_file", "path": "/tmp/x0ne-safe-change.txt", "content": "hello"}
                    ],
                    "tests": [
                        {"type": "read_file", "path": "/tmp/x0ne-safe-change.txt"}
                    ],
                    "label": "demo"
                }),
            )
            .unwrap();
        match act {
            Action::SafeChange {
                steps,
                tests,
                rollback_on_fail,
                label,
                keep_snapshot,
            } => {
                assert_eq!(steps.len(), 1);
                assert_eq!(tests.len(), 1);
                assert!(rollback_on_fail);
                assert_eq!(label.as_deref(), Some("demo"));
                assert!(!keep_snapshot);
                assert!(
                    matches!(&steps[0], Action::WriteFile { path, .. } if path == "/tmp/x0ne-safe-change.txt")
                );
                assert!(
                    matches!(&tests[0], Action::ReadFile { path, .. } if path == "/tmp/x0ne-safe-change.txt")
                );
            }
            _ => panic!("expected SafeChange"),
        }
    }

    #[test]
    fn pipeline_step_missing_type_errors() {
        let r = ToolRegistry::new();
        let err = r.parse_step(&json!({"a": 1.0, "b": 2.0})).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn test_to_action_unknown() {
        let registry = ToolRegistry::new();
        let args = json!({});
        let err = registry.to_action("unknown_tool", &args);
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().to_string(), "unknown tool: unknown_tool");
    }

    /// ANCHOR: one semantic operation per externally visible name. Every MCP
    /// tool we advertise must actually be callable — `file_info` was declared
    /// in the catalog but had no `to_action` arm, so a client that discovered
    /// it and called it got `unknown tool`. An advertised name that does not
    /// resolve is the same failure as a stale action name, one surface over.
    #[test]
    fn every_advertised_tool_resolves_to_an_action() {
        let reg = ToolRegistry::new();
        for tool in reg.list_tools() {
            let name = tool["name"].as_str().expect("every tool is named");
            // Empty args: we only care whether the name resolves, not whether
            // the arguments validate.
            let err = reg
                .to_action(name, &json!({}))
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                !err.contains("unknown tool"),
                "MCP advertises '{name}' but it has no mapping"
            );
        }
    }

    /// And the reverse direction: every tool must map onto a real action, so
    /// the MCP surface cannot drift away from the vocabulary the way the
    /// runtime's own catalog did.
    #[test]
    fn every_tool_maps_onto_a_live_action() {
        let reg = ToolRegistry::new();
        for tool in reg.list_tools() {
            let tool_name = tool["name"].as_str().expect("every tool is named");
            if let Ok(action) = reg.to_action(tool_name, &json!({})) {
                let action_name = action.type_name();
                assert!(
                    x0ne_actions::all_type_names().contains(&action_name),
                    "MCP tool '{tool_name}' maps to '{action_name}', which is not \
                     a runtime action"
                );
            }
        }
    }

    /// A tool declared in the catalogue but missing from `to_action` answers
    /// « unknown tool » to a client that discovered it — the defect already met
    /// with `file_info`. And the category must say the same thing as the class:
    /// an MCP client that only shows `mutable: false` tools must not be offered
    /// the commit.
    #[test]
    fn both_program_tools_map_to_their_actions_and_declare_the_right_mutability() {
        let reg = ToolRegistry::new();
        let preview = reg
            .tools()
            .iter()
            .find(|t| t.name == "preview_program")
            .expect("catalogue");
        assert_eq!(preview.category, "read");
        assert!(!preview.mutable);
        let run = reg
            .tools()
            .iter()
            .find(|t| t.name == "run_program")
            .expect("catalogue");
        assert_eq!(run.category, "modify");
        assert!(run.mutable);

        let args = json!({"source": "#!asm-ai/v0\ncount files\n"});
        assert_eq!(
            reg.to_action("preview_program", &args)
                .expect("mapping")
                .type_name(),
            "PreviewProgram"
        );
        assert_eq!(
            reg.to_action("run_program", &args)
                .expect("mapping")
                .type_name(),
            "RunProgram"
        );
    }

    /// Minimal arguments satisfying a schema's `required` list, by declared type.
    fn args_from_schema(schema: &Value) -> Value {
        let mut out = serde_json::Map::new();
        let props = schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for key in schema["required"].as_array().cloned().unwrap_or_default() {
            let key = key.as_str().unwrap().to_string();
            out.insert(key.clone(), sample_for(&props[&key]));
        }
        Value::Object(out)
    }

    fn sample_for(prop: &Value) -> Value {
        match prop["type"].as_str() {
            Some("string") => prop["enum"][0]
                .clone()
                .as_str()
                .map(Value::from)
                .unwrap_or(json!("x")),
            Some("integer") | Some("number") => json!(1),
            Some("boolean") => json!(true),
            Some("array") => json!([sample_for(&prop["items"])]),
            Some("object") => args_from_schema(prop),
            _ => json!("x"),
        }
    }

    /// Every tool's `action` field names a catalogued action; the class lookup
    /// behind `annotations()` panics otherwise.
    #[test]
    fn every_tool_action_exists_in_the_catalog() {
        for tool in ToolRegistry::new().tools() {
            assert!(
                x0ne_actions::all_type_names().contains(&tool.action),
                "tool '{}' declares action '{}', absent from GENERATED_ACTIONS",
                tool.name,
                tool.action
            );
            let _ = tool.class();
        }
    }

    /// `to_action` with the schema's required arguments must build the variant
    /// the tool declares, for all tools, so `action` cannot drift from the match.
    #[test]
    fn to_action_round_trips_to_the_declared_action_for_every_tool() {
        let reg = ToolRegistry::new();
        let overrides: std::collections::HashMap<&str, Value> = [
            (
                "pipeline",
                json!({"steps": [{"type": "read_file", "path": "/tmp/a"}]}),
            ),
            (
                "safe_change",
                json!({"steps": [{"type": "read_file", "path": "/tmp/a"}], "tests": []}),
            ),
            (
                "simulate",
                json!({"target": {"type": "ReadFile", "path": "/tmp/a"}}),
            ),
            (
                "subscribe",
                json!({"event_type": {"kind": "file_changed", "path": "/tmp/a"}}),
            ),
        ]
        .into_iter()
        .collect();
        let mut failures = Vec::new();
        for tool in reg.tools() {
            let args = overrides
                .get(tool.name.as_str())
                .cloned()
                .unwrap_or_else(|| args_from_schema(&tool.input_schema));
            match reg.to_action(&tool.name, &args) {
                Ok(action) if action.type_name() == tool.action => {}
                Ok(action) => failures.push(format!(
                    "{}: builds {} but declares {}",
                    tool.name,
                    action.type_name(),
                    tool.action
                )),
                Err(e) => failures.push(format!("{}: {args} -> {e}", tool.name)),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn annotations_follow_the_class() {
        let reg = ToolRegistry::new();
        for tool in reg.tools() {
            let a = tool.annotations();
            let class = tool.class();
            assert_eq!(
                a["readOnlyHint"],
                matches!(class, SideEffectClass::ReadOnly | SideEffectClass::Meta),
                "{}",
                tool.name
            );
            assert_eq!(
                a["destructiveHint"],
                class == SideEffectClass::Destructive,
                "{}",
                tool.name
            );
            assert_eq!(
                a["openWorldHint"],
                class == SideEffectClass::Network,
                "{}",
                tool.name
            );
            assert_eq!(
                a.get("idempotentHint").is_some(),
                class == SideEffectClass::ReadOnly,
                "{}",
                tool.name
            );
        }
    }

    /// `mutable` is hand-written; the class is derived. They disagree on nine
    /// tools (measured 2026-10-05). Pinned so the list can only shrink: the
    /// annotations follow the class, `mutable` is left for the owner to rule on.
    #[test]
    fn mutable_disagrees_with_the_class_only_on_the_known_tools() {
        let reg = ToolRegistry::new();
        let mut disagree: Vec<&str> = reg
            .tools()
            .iter()
            .filter(|t| t.mutable == t.annotations()["readOnlyHint"].as_bool().unwrap())
            .map(|t| t.name.as_str())
            .collect();
        disagree.sort_unstable();
        assert_eq!(
            disagree,
            [
                "create_snapshot",
                "dns_lookup",
                "get_clipboard",
                "get_focused_window",
                "get_window_tree",
                "port_check",
                "screenshot",
                "simulate",
                "stop_handle",
            ]
        );
    }
}
