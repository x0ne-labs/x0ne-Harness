//! MCP server for x0ne-orchestrator — the gateway pattern.
//!
//! - Speaks MCP up to the client (Claude Code / Cline / Continue / …).
//! - Optionally spawns a downstream `x0ne-runtime --mode mcp` and forwards
//!   any unknown tool to it. With one MCP plug, the client gets:
//!     • the orchestrator's native tools (sysaicalls, gateway diagnostics);
//!     • the full 99-tool surface of x0ne-runtime, sandboxed by whatever
//!       runtime config the gateway points at.
//!
//! Human gate: the runtime is started with `--upstream-human-gate`,
//! so every side-effecting call forwarded here is first confirmed by the
//! user through MCP elicitation; a client without it is refused.
//!
//! Phases:
//! - 2 (done): native skeleton + handshake.
//! - 3 (this file): downstream proxy.
//! - 4 (next): policy-gated sysaicalls (Plan, SpawnAgent, BroadcastAgents).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

use x0ne_mcp::{
    BoundedLine, ChildMcpClient, JsonRpcRequest, JsonRpcResponse, INTERNAL_ERROR, INVALID_PARAMS,
    INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};

use crate::artifact_expose::{execute_artifact_expose, execute_artifact_unexpose};
use crate::config::Config;
use crate::goal::{
    AchievementCheck, Budget, GoalManager, GoalState, GoalStatus, SharedGoalManager, StepOutcome,
};
use crate::sysaicalls::{list_agents_data, AgentRegistry};

/// One native tool: stable wire shape `{ name, description, input_schema }`.
struct NativeTool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

/// Largest request line the gateway reads; a longer one is discarded and
/// answered with INVALID_REQUEST.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Largest plan `x0ne_plan` records, serialized.
pub const MAX_PLAN_BYTES: usize = 1024 * 1024;
/// Plan ledger bounds; the oldest plans are evicted past either.
pub const PLAN_LEDGER_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PLAN_LEDGER_MAX_ENTRIES: usize = 64;
/// How long a side-effecting call waits for the user's answer.
pub const ELICITATION_TIMEOUT: Duration = Duration::from_secs(120);
/// Client messages queued while a confirmation is pending; past this, requests
/// are answered busy instead of queued.
const MAX_DEFERRED_WHILE_ASKING: usize = 64;
/// Protocol versions this server speaks, newest first (MCP lifecycle:
/// echo the client's version if supported, else offer the latest).
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// `elicitation/create` exists from this revision; `mode` from the next one.
const ELICITATION_SINCE: &str = "2025-06-18";
const ELICITATION_MODE_SINCE: &str = "2025-11-25";

/// Native tools whose effect leaves the gateway's own memory: the operator's
/// host inventory and active pointer, a URL opened or closed on this machine.
/// Each is put to the human first, as the agent loop does for the same
/// actions; with no client able to ask, it is refused (H3-2, 2026-10-30).
/// The label is what the question says the tool does.
const NATIVE_TOOLS_WITH_EFFECTS: &[(&str, &str)] = &[
    (
        "x0ne_host_add",
        "adds or rewrites a host in ~/.x0ne/hosts.toml",
    ),
    ("x0ne_host_remove", "removes a host from ~/.x0ne/hosts.toml"),
    ("x0ne_host_use", "changes the active host"),
    (
        "x0ne_artifact_expose",
        "opens a loopback URL to an artifact of the sandbox",
    ),
    ("x0ne_artifact_unexpose", "closes exposed artifact URLs"),
];

/// F5: native tools that only read. `x0ne_host_active` rewrites a stale pointer.
const READ_ONLY_NATIVE_TOOLS: &[&str] = &[
    "x0ne_orchestrator_info",
    "x0ne_list_agents",
    "x0ne_goal_status",
    "x0ne_host_list",
    "x0ne_host_get",
    "x0ne_simulate",
    "x0ne_recipe",
];

fn native_tools() -> Vec<NativeTool> {
    vec![
        NativeTool {
            name: "x0ne_orchestrator_info",
            description: "Return the orchestrator's version, downstream runtime status, and exposed tool counts. Read-only, side-effect free — useful as a liveness probe and for debugging the gateway plumbing.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_list_agents",
            description: "List sub-agents tracked by the orchestrator: id, task, status (running|completed|failed), elapsed_secs, tokens_used. Empty in MCP-only mode until SpawnAgent is exposed (Phase 4 follow-up). Read-only.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_plan",
            description: "Record a structured plan WITHOUT executing it. Returns a plan_id and a human-readable summary. Use this to think out loud, surface intent to the user, and pre-policy-check a sequence of steps. Each step uses the same {type, ...} shape as a Pipeline step. Execution is NOT performed — call `x0ne_plan_execute` after a human approves.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Short description of what the plan accomplishes." },
                    "steps": {
                        "type": "array",
                        "items": { "type": "object" },
                        "description": "Array of step objects, same shape as Pipeline steps."
                    }
                },
                "required": ["label", "steps"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_plan_execute",
            description: "Execute a previously-recorded plan via the downstream runtime. Requires `approve: true` — without it, this returns a dry-run summary. A plan with any side-effecting step is then confirmed by the user through the MCP client (elicitation); a client without elicitation gets a refusal. Each step is independently policy-checked at execution time. The plan is marked executed in the ledger after a successful run; calling twice returns an error.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string", "description": "id returned by `x0ne_plan`." },
                    "approve": { "type": "boolean", "description": "Must be true to actually run. Default false = dry-run." },
                    "parallel": { "type": "boolean", "description": "Run the steps concurrently (forwarded to the underlying Pipeline). Default false." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_artifact_expose",
            description: "Publish an artifact (already created by the runtime via `artifact_create`) to the user's host browser at http://127.0.0.1:7800/a/<id>/. Pulls bytes out of the sandbox container on demand via the configured backend (Incus/Podman/Docker) — no port opened on tenant, no host filesystem pollution. Privilege-boundary primitive: only callable from the orchestrator (host); the runtime structurally cannot expose host ports. Requires sandbox mode ([bridge] sandbox_cmd configured). Idempotent — re-exposing refreshes the TTL.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "artifact_id": { "type": "string", "description": "ID returned by the runtime's artifact_create" },
                    "visibility":  { "type": "string", "enum": ["host"], "description": "Bind scope. Only 'host' (loopback) supported today; lan/public planned." },
                    "ttl_secs":    { "type": "integer", "description": "Optional TTL in seconds; the entry is dropped lazily on access after expiry." }
                },
                "required": ["artifact_id"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_artifact_unexpose",
            description: "Revoke an exposed artifact from the local proxy. With `artifact_id`: drop just that one. Without args: clear the entire registry (un-expose all artifacts of this session). Files in the sandbox are untouched.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "artifact_id": { "type": "string", "description": "Optional — if omitted, every exposed artifact is revoked." }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_set",
            description: "Set a persistent goal that survives turns and sessions. Replaces any currently-active goal. Returns the goal_id and full initial state. The goal stays active until it is `achieved` (auto via achievement_check or manual via x0ne_goal_complete), `paused`, `cleared`, `blocked`, or `budget_exhausted`. Persisted to ~/.x0ne/goals/<id>.json so a fresh session can resume the same intent.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What the agent is trying to accomplish, e.g. 'finish the auth refactor and make all tests pass'." },
                    "max_steps": { "type": "integer", "description": "Optional cap on x0ne_goal_step calls before status flips to budget_exhausted." },
                    "max_seconds": { "type": "integer", "description": "Optional wall-clock cap from goal creation." },
                    "achievement_check": {
                        "type": "object",
                        "description": "Optional self-check executed each step. Variants: {kind:'manual'}, {kind:'file_exists', path}, {kind:'file_contains', path, needle}, {kind:'command', command, args, cwd?, expected_exit?, timeout_secs?}, {kind:'action', action: <runtime Action JSON>}. Routed through the downstream runtime, so the same policy guards apply."
                    }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_status",
            description: "Return the active goal's current state: id, text, status, budget usage, recent history, and a one-line human summary. Read-only. Returns {active: false} when no goal is set. Cheap enough to call at every turn — that's how the host agent decides whether to keep working.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_step",
            description: "Record one tick of progress on the active goal: increments step counter, optionally attaches a `note` and `plan_id` to the history, runs the achievement check (if configured), and persists. Returns the new state plus whether the goal is now achieved. The host should call this AFTER it has made meaningful progress on a turn (executed a plan, ran tests, …).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Free-form summary of what was done this tick (e.g. 'wrote 3 files, ran tests, 12/12 green')." },
                    "plan_id": { "type": "string", "description": "Optional plan_id this step came from — links the goal history to the plan ledger." }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_pause",
            description: "Pause the active goal. Idempotent — pausing an already-paused goal is a no-op. Use this when the user wants to interrupt the agent without losing the goal context.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_resume",
            description: "Resume a paused or blocked goal — flips status back to active. Errors on terminal states (achieved, budget_exhausted, cleared); the user should x0ne_goal_set a fresh goal in those cases.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_complete",
            description: "Manually mark the active goal as achieved. Use this when the achievement_check is `manual` (or absent) and the agent or user has determined the goal is done. Records the optional note in history.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Optional explanation of why the goal is considered achieved." }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_goal_clear",
            description: "Drop the active goal: removes the on-disk file and clears the active pointer. Idempotent — calling with no active goal is a no-op. Other historical goal files are NOT touched.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        // ── Host inventory (fleet admin) ─────────────────────────────────────
        NativeTool {
            name: "x0ne_host_list",
            description: "List every host registered in ~/.x0ne/hosts.toml — name, target, mode (adhoc|native), tags, description, plus the currently-active host (if any). Read-only. Use as the first call when the agent starts working on infra so it sees what's reachable. Filter by tag with the `tag` arg.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tag": { "type": "string", "description": "Optional: list only hosts having this tag." }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_host_get",
            description: "Return one host's full record by name. Errors if not found. Useful before issuing SshExec to confirm the target / mode / extra ssh_options.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Host name as registered in the inventory." }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_host_add",
            description: "Add or update a host in the inventory (~/.x0ne/hosts.toml). Idempotent: an existing entry with the same name is replaced. Returns the persisted record. Validates the name (alpha-leading, [a-zA-Z0-9_-]{1,63}) and that target is non-empty. NOT enable any execution by itself — the operator still has to enable SshExec in the runtime policy.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "target": { "type": "string", "description": "ssh(1) target: user@host, alias, or just host." },
                    "mode": { "type": "string", "enum": ["adhoc", "native"], "description": "adhoc = SshExec via local OpenSSH; native = x0ne-runtime deployed on the host (typed actions). Default: adhoc." },
                    "runtime_path": { "type": "string", "description": "(native) absolute path of x0ne-runtime on the target. Default: /opt/x0ne-runtime." },
                    "policy_profile": { "type": "string", "description": "(native) name of the policy template materialised on the host (readonly | staging | prod-strict)." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "description": { "type": "string" },
                    "ssh_options": { "type": "array", "items": { "type": "string" }, "description": "Extra '-o KEY=VALUE' flags appended to every ssh invocation for this host." }
                },
                "required": ["name", "target"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_host_remove",
            description: "Remove a host from the inventory by name. Returns {removed: bool}. If the active-host pointer named this host, it is cleared as well. Files in ~/.ssh and elsewhere are untouched.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_host_use",
            description: "Set the active host (`~/.x0ne/active_host`). Subsequent SshExec calls without an explicit `host` may default to this one. Pass `name: null` (or omit) to clear the active host. The orchestrator's downstream runtime is NOT auto-respawned — for native-mode rerouting use the bridge config or x0ne-code's /host slash command.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": ["string", "null"], "description": "Host name to activate. Null/omit clears." }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_host_active",
            description: "Return the currently-active host record (or {active: false} if none). Self-heals stale pointers when the named host has been removed.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_simulate",
            description: "Pre-flight a multi-step plan WITHOUT executing it. Routes each step through the downstream runtime's `policy_check` and aggregates: per-step allow/deny/confirm decisions, all paths that would be read/written, all commands that would run, and a global verdict. Pure reasoning — no I/O, no side effects. Compose with `x0ne_plan` for the safety loop: propose → simulate → human reviews → execute.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": { "type": "object" },
                        "description": "Array of step objects, same shape as Pipeline steps. snake_case or PascalCase types both accepted."
                    },
                    "plan_id": {
                        "type": "string",
                        "description": "Alternative to `steps`: simulate a previously-recorded plan from the ledger."
                    }
                },
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_recipe",
            description: "RAG retrieval over the local trajectory corpus (~/.x0ne/dataset/index.jsonl). Given an `intent` string, returns the top-K most-similar past successful trajectories — each item carries the original goal, a compact tool-sequence summary, success outcome, and a similarity score. Use BEFORE planning: \"have I done something like this before?\" → adapt the past Pipeline instead of inventing one. Backend is auto-detected (ollama nomic-embed-text if installed, TF-IDF fallback). Returns empty array if `make dataset-index` has not been run yet.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "Natural-language description of what the agent wants to do. e.g. 'make me a fun website', 'check if python is installed', 'audit fleet uptime'."
                    },
                    "k": {
                        "type": "integer",
                        "description": "Number of recipes to return. Default 3, max 10.",
                        "default": 3
                    }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
        },
        NativeTool {
            name: "x0ne_render_template",
            description: "Render a single string template containing `{{tool(args)}}` placeholders by calling each tool exactly once and splicing the result back in. Lets the LLM emit ONE fluent sentence with the deterministic bits filled by the runtime — no second LLM round-trip needed for the final compose step. Two placeholder forms: `{{ str_count(\"strawberry\", \"r\") }}` (positional, snake_case, args are JSON literals) or `{{ StrCount {\"haystack\":\"strawberry\",\"needle\":\"r\"} }}` (JSON form, PascalCase, works for any Action). Recursion up to `max_depth` substitution passes. Returns `{ rendered, calls[], passes }` — the per-placeholder call log doubles as audit trail.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "template": {
                        "type": "string",
                        "description": "The template body, e.g. `There are {{str_count(\"strawberry\",\"r\")}} r in strawberry.`"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Max substitution passes. Default 2 (one level of nested templates). Capped at 8."
                    },
                    "strict": {
                        "type": "boolean",
                        "description": "If true, abort the whole render on the first failed placeholder. Default false: failures leave the raw `{{…}}` token in place so the caller can inspect `calls[].error` and recover.",
                        "default": false
                    }
                },
                "required": ["template"],
                "additionalProperties": false
            }),
        },
    ]
}

fn is_native_tool(name: &str) -> bool {
    native_tools().iter().any(|t| t.name == name)
}

/// Configuration for the downstream runtime that the gateway forwards to.
pub struct DownstreamRuntime {
    /// Path or PATH-resolvable command. Default: `x0ne-runtime`.
    pub bin: String,
    /// Extra args to pass on top of `--mode mcp`. e.g. `--config /path/to/profile.toml`.
    pub extra_args: Vec<String>,
    /// Optional command prefix tokens — prepended before `bin`. Lets the
    /// runtime be wrapped by `incus exec`, `podman exec`, `ssh`, or any
    /// process-launching tool. Empty = direct local spawn.
    pub prefix: Vec<String>,
}

impl Default for DownstreamRuntime {
    fn default() -> Self {
        Self {
            bin: "x0ne-runtime".to_string(),
            extra_args: vec![],
            prefix: vec![],
        }
    }
}

/// The MCP client end: frames in (a reader thread on stdin in `run`, a
/// channel in tests), frames out, and what arrived while a confirmation
/// was pending.
pub(crate) struct ClientLink {
    inbound: Mutex<mpsc::Receiver<BoundedLine>>,
    outbound: mpsc::UnboundedSender<String>,
    deferred: std::sync::Mutex<VecDeque<BoundedLine>>,
}

impl ClientLink {
    pub(crate) fn new(
        inbound: mpsc::Receiver<BoundedLine>,
        outbound: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            inbound: Mutex::new(inbound),
            outbound,
            deferred: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    fn send(&self, frame: &impl serde::Serialize) -> bool {
        match serde_json::to_string(frame) {
            Ok(line) => self.outbound.send(line).is_ok(),
            Err(e) => {
                tracing::error!("unserializable frame: {e}");
                false
            }
        }
    }

    /// Next client message: what was queued during a confirmation first.
    async fn next(&self) -> BoundedLine {
        let queued = self.deferred.lock().unwrap().pop_front();
        if let Some(l) = queued {
            return l;
        }
        self.inbound
            .lock()
            .await
            .recv()
            .await
            .unwrap_or(BoundedLine::Eof)
    }

    /// Queue a message for after the confirmation. Returns false when full.
    fn defer(&self, input: BoundedLine) -> bool {
        let mut q = self.deferred.lock().unwrap();
        if q.len() >= MAX_DEFERRED_WHILE_ASKING && input != BoundedLine::Eof {
            return false;
        }
        q.push_back(input);
        true
    }
}

/// What the client declared at `initialize`.
#[derive(Debug, Clone, Default)]
struct ClientCaps {
    protocol_version: String,
    /// Declared `elicitation` with form mode, on a revision that has it.
    elicitation_form: bool,
}

pub struct OrchestratorMcpServer {
    /// `None` until `run()` installs stdio (or a test installs channels).
    client: Option<ClientLink>,
    client_caps: std::sync::Mutex<ClientCaps>,
    elicitation_timeout: Duration,
    config: Config,
    /// Native sub-agent registry. Empty in MCP-only mode until SpawnAgent
    /// is wired (Phase 4 follow-up).
    agents: Arc<AgentRegistry>,
    /// `None` if downstream is disabled (e.g. user passed --no-downstream-runtime).
    /// Wrapped in `Mutex` because handlers borrow `&self` and the client is
    /// inherently stateful (mutable next_id + pipes).
    downstream: Option<Mutex<ChildMcpClient>>,
    /// Cached after the first `tools/list` call. Refreshed never — the runtime
    /// is started by us, so the tool surface is fixed for the session.
    downstream_tools: Mutex<Option<Vec<Value>>>,
    /// Second downstream: x0ne-vm-mcp (libvirt + VNC VM driver). Optional —
    /// only spawned when `with_vm_downstream` is called or X0NE_VM_DOWNSTREAM=1.
    vm_downstream: Option<Mutex<ChildMcpClient>>,
    vm_tools: Mutex<Option<Vec<Value>>>,
    /// In-memory plan ledger. `x0ne_plan` records here; future
    /// `x0ne_plan_execute` would consume entries after a human approval.
    /// Bounded by PLAN_LEDGER_MAX_ENTRIES and PLAN_LEDGER_MAX_BYTES (oldest dropped).
    plan_ledger: Mutex<Vec<Value>>,
    /// Persistent goal manager — survives across sessions on disk.
    /// Initialised lazily so a missing `~` doesn't kill the gateway:
    /// goal-related calls just return an `unavailable` error.
    goals: Option<SharedGoalManager>,
}

impl OrchestratorMcpServer {
    pub fn new(config: Config) -> Self {
        let goals = match GoalManager::new_default() {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                eprintln!(
                    "[x0ne-orchestrator] goal store unavailable: {} (x0ne_goal_* tools will return unavailable)",
                    e
                );
                None
            }
        };
        Self {
            client: None,
            client_caps: std::sync::Mutex::new(ClientCaps::default()),
            elicitation_timeout: ELICITATION_TIMEOUT,
            config,
            agents: Arc::new(AgentRegistry::new()),
            downstream: None,
            downstream_tools: Mutex::new(None),
            vm_downstream: None,
            vm_tools: Mutex::new(None),
            plan_ledger: Mutex::new(Vec::new()),
            goals,
        }
    }

    /// Spawn the x0ne-vm-mcp downstream alongside x0ne-runtime so the LLM
    /// sees a unified toolbelt (134 X0NE actions + 23 vm_* tools). Routing
    /// is by name prefix in `handle_tools_call`. Best-effort: if the binary
    /// is missing the gateway still serves everything else.
    pub async fn with_vm_downstream(mut self, bin: &str) -> Self {
        match ChildMcpClient::spawn("x0ne-vm-mcp", bin, &[]).await {
            Ok(mut client) => {
                if let Err(e) = client.initialize().await {
                    tracing::warn!("x0ne-vm-mcp initialize failed: {} — vm_* tools disabled", e);
                    return self;
                }
                tracing::info!("x0ne-vm downstream ready ({})", bin);
                self.vm_downstream = Some(Mutex::new(client));
            }
            Err(e) => {
                tracing::warn!(
                    "x0ne-vm-mcp not spawned ({}): {} — vm_* tools disabled",
                    bin,
                    e
                );
            }
        }
        self
    }

    /// Spawn the downstream runtime and run its `initialize` handshake.
    /// Logs (to stderr) on failure but does not abort — the gateway can still
    /// serve native tools without a runtime.
    pub async fn with_downstream(mut self, runtime: DownstreamRuntime) -> Self {
        // Build the full argv: prefix... bin --mode mcp --upstream-human-gate [extra_args]
        // If prefix is empty, the binary itself is the cmd.
        //
        // SECURITY: `--upstream-human-gate` is this gateway asserting what is
        // in fact true — it front-ends the downstream runtime and owns the
        // human gate (repo invariant 2). Since 2026-08-15 the runtime refuses
        // to serve `--mode mcp` under `policy.mode = "auto"`, because for a
        // runtime wired *directly* into an MCP host that means no gate exists
        // anywhere. Here one does, upstream, so the downstream must stay in
        // `auto`: making it re-ask on its own would produce a refusal after a
        // human already approved, which is the double-gating regression of
        // 2026-08-12. The flag is what distinguishes the two situations.
        let runtime_args: Vec<String> = ["--mode", "mcp", "--upstream-human-gate"]
            .iter()
            .map(|s| s.to_string())
            .chain(runtime.extra_args.iter().cloned())
            .collect();

        let (cmd, args_owned): (String, Vec<String>) = if runtime.prefix.is_empty() {
            (runtime.bin.clone(), runtime_args)
        } else {
            // First prefix token is the cmd; rest become args before the bin.
            let mut iter = runtime.prefix.iter();
            let cmd = iter.next().unwrap().clone();
            let mut args: Vec<String> = iter.cloned().collect();
            args.push(runtime.bin.clone());
            args.extend(runtime_args);
            (cmd, args)
        };
        let args_borrowed: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();

        match ChildMcpClient::spawn("x0ne-runtime", &cmd, &args_borrowed).await {
            Ok(mut client) => {
                if let Err(e) = client.initialize().await {
                    tracing::warn!(
                        "downstream initialize failed: {} — gateway will run without proxy",
                        e
                    );
                    return self;
                }
                tracing::info!(
                    "downstream x0ne-runtime ready ({} {})",
                    cmd,
                    args_owned.join(" ")
                );
                self.downstream = Some(Mutex::new(client));
            }
            Err(e) => {
                tracing::warn!(
                    "could not spawn downstream `{} {}`: {} — gateway will run without proxy",
                    cmd,
                    args_owned.join(" "),
                    e
                );
            }
        }
        self
    }

    /// How long a side-effecting call waits for the user (default 120 s).
    pub fn with_elicitation_timeout(mut self, timeout: Duration) -> Self {
        self.elicitation_timeout = timeout;
        self
    }

    /// Serve the client on `link` instead of stdio.
    pub(crate) fn attach_client(&mut self, link: ClientLink) {
        self.client = Some(link);
    }

    /// Serve stdio until EOF. stdin is read on its own thread so a pending
    /// confirmation can time out and read the client's answer.
    pub async fn run(&mut self) -> Result<()> {
        let (in_tx, in_rx) = mpsc::channel::<BoundedLine>(16);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            loop {
                let input = match x0ne_mcp::read_bounded_line(&mut stdin, MAX_REQUEST_BYTES) {
                    Ok(l) => l,
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                        tracing::error!("Failed to read JSON-RPC: {}", e);
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("stdin failed: {}", e);
                        BoundedLine::Eof
                    }
                };
                let eof = input == BoundedLine::Eof;
                if in_tx.blocking_send(input).is_err() || eof {
                    break;
                }
            }
        });
        let writer = std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            while let Some(mut line) = out_rx.blocking_recv() {
                line.push('\n');
                if let Err(e) = stdout
                    .write_all(line.as_bytes())
                    .and_then(|_| stdout.flush())
                {
                    tracing::error!("Failed to write JSON-RPC frame: {}", e);
                    break;
                }
            }
        });
        self.attach_client(ClientLink::new(in_rx, out_tx));

        if let Some(link) = self.client.as_ref() {
            loop {
                let input = link.next().await;
                if input == BoundedLine::Eof {
                    break;
                }
                if let Some(resp) = self.handle_input(input).await {
                    if !link.send(&resp) {
                        break;
                    }
                }
            }
        }
        // Dropping the link closes the outbound channel; the writer drains and ends.
        self.client = None;
        let _ = tokio::task::spawn_blocking(move || writer.join()).await;
        Ok(())
    }

    /// A bounded line from stdin: an oversized one is refused, not parsed.
    pub(crate) async fn handle_input(&self, input: BoundedLine) -> Option<JsonRpcResponse> {
        match input {
            BoundedLine::Line(l) => self.handle_line(&l).await,
            BoundedLine::TooLong { bytes } => {
                tracing::warn!("MCP request of {bytes} bytes refused (limit {MAX_REQUEST_BYTES})");
                Some(JsonRpcResponse::error(
                    Value::Null,
                    INVALID_REQUEST,
                    &format!("request too large: {bytes} bytes, limit {MAX_REQUEST_BYTES}"),
                ))
            }
            BoundedLine::Eof => None,
        }
    }

    /// One line of stdin in, at most one frame out. Malformed input is
    /// answered (JSON-RPC 2.0 §5.1), not dropped: the client would wait forever.
    pub(crate) async fn handle_line(&self, line: &str) -> Option<JsonRpcResponse> {
        if line.trim().is_empty() {
            return None;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to read JSON-RPC: {}", e);
                return Some(JsonRpcResponse::error(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                ));
            }
        };
        if is_response(&value) {
            // JSON-RPC 2.0 §5: a response is never answered. One arriving here
            // matches no pending confirmation (those are read in `ask_human`).
            tracing::warn!(
                "unsolicited response from the client dropped: id {}",
                value["id"]
            );
            return None;
        }
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        match serde_json::from_value::<JsonRpcRequest>(value) {
            Ok(req) => self.handle_message(req).await,
            Err(e) => Some(JsonRpcResponse::error(
                id,
                INVALID_REQUEST,
                &format!("invalid request: {e}"),
            )),
        }
    }

    async fn handle_message(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        if req.is_notification() {
            return None;
        }
        let id = req.id.clone().unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => Some(JsonRpcResponse::ok(
                id,
                json!({
                    "protocolVersion": self.negotiate(req.params.as_ref()),
                    "capabilities": {
                        "tools": { "listChanged": false },
                        "resources": { "subscribe": false, "listChanged": false },
                        "prompts": { "listChanged": false },
                        "logging": {}
                    },
                    "serverInfo": {
                        "name": "x0ne-orchestrator",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )),
            "ping" => Some(JsonRpcResponse::ok(id, json!({}))),
            "tools/list" => Some(self.handle_tools_list(id).await),
            "tools/call" => Some(self.handle_tools_call(id, &req).await),
            "resources/list" => Some(JsonRpcResponse::ok(id, json!({ "resources": [] }))),
            "resources/templates/list" => {
                Some(JsonRpcResponse::ok(id, json!({ "resourceTemplates": [] })))
            }
            "prompts/list" => Some(JsonRpcResponse::ok(id, json!({ "prompts": [] }))),
            "logging/setLevel" => Some(JsonRpcResponse::ok(id, json!({}))),
            _ => Some(JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                &format!("method not found: {}", req.method),
            )),
        }
    }

    async fn handle_tools_list(&self, id: Value) -> JsonRpcResponse {
        // 1. Native tools.
        let mut tools: Vec<Value> = native_tools()
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "annotations": x0ne_mcp::tool_hints(READ_ONLY_NATIVE_TOOLS.contains(&t.name)),
                })
            })
            .collect();

        // 2. Downstream tools — fetched lazily, cached for the session.
        if let Some(client) = &self.downstream {
            let mut cache = self.downstream_tools.lock().await;
            if cache.is_none() {
                let mut c = client.lock().await;
                match c.tools_list().await {
                    Ok(t) => *cache = Some(t),
                    Err(e) => {
                        tracing::warn!("downstream tools/list failed: {}", e);
                        *cache = Some(vec![]);
                    }
                }
            }
            if let Some(downstream) = cache.as_ref() {
                // Skip any name collision: native tools win. (Currently no
                // collisions — the only native is `x0ne_orchestrator_info`.)
                for t in downstream {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if !is_native_tool(name) {
                        tools.push(t.clone());
                    }
                }
            }
        }

        // 3. x0ne-vm tools — same cache pattern, prefix `vm_` is the routing key.
        if let Some(client) = &self.vm_downstream {
            let mut cache = self.vm_tools.lock().await;
            if cache.is_none() {
                let mut c = client.lock().await;
                match c.tools_list().await {
                    Ok(t) => *cache = Some(t),
                    Err(e) => {
                        tracing::warn!("x0ne-vm tools/list failed: {}", e);
                        *cache = Some(vec![]);
                    }
                }
            }
            if let Some(vm) = cache.as_ref() {
                for t in vm {
                    let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    // Native tools win, and avoid duplicates if same tool appears in both downstreams.
                    if !is_native_tool(name)
                        && !tools.iter().any(|existing| {
                            existing.get("name").and_then(|v| v.as_str()) == Some(name)
                        })
                    {
                        tools.push(t.clone());
                    }
                }
            }
        }

        JsonRpcResponse::ok(id, json!({ "tools": tools }))
    }

    async fn handle_tools_call(&self, id: Value, req: &JsonRpcRequest) -> JsonRpcResponse {
        let params = match req.params.as_ref() {
            Some(p) => p,
            None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing params"),
        };
        let tool_name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing tool name"),
        };
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        // 1. Native dispatch.
        if is_native_tool(&tool_name) {
            return self.dispatch_native(id, &tool_name, arguments).await;
        }

        // 2. Pick the downstream: `vm_*` → x0ne-vm-mcp, the rest → x0ne-runtime.
        let is_vm = tool_name.starts_with("vm_");
        let client = if is_vm {
            match &self.vm_downstream {
                Some(c) => c,
                None => {
                    return JsonRpcResponse::ok(
                        id,
                        json!({
                            "content": [{"type":"text","text": "vm_* tool requested but x0ne-vm-mcp downstream not enabled. Re-run x0ne-orchestrator with X0NE_VM_DOWNSTREAM=1"}],
                            "isError": true,
                        }),
                    )
                }
            }
        } else {
            match &self.downstream {
                Some(c) => c,
                None => {
                    return JsonRpcResponse::ok(
                        id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": format!(
                                    "Tool `{}` is not a native orchestrator tool, and no downstream runtime is connected. \
                                     Start the gateway with --downstream-runtime <PATH> (default: x0ne-runtime in PATH).",
                                    tool_name
                                )
                            }],
                            "isError": true
                        }),
                    )
                }
            }
        };

        // 3. The human gate the runtime was told we own (--upstream-human-gate).
        if forwarded_call_needs_human(&tool_name, &arguments) {
            let summary = describe_call(&tool_name, &arguments);
            if let Err(reason) = self.ask_human(&id, &summary).await {
                return human_gate_refusal(id, &tool_name, &reason);
            }
        }

        // 4. Forward.
        let mut c = client.lock().await;
        match c.tools_call(&tool_name, arguments).await {
            Ok(result) => JsonRpcResponse::ok(id, result),
            Err(e) => {
                let text = if is_vm {
                    format!("x0ne-vm error: {e}")
                } else {
                    format!("Downstream forward failed for tool `{}`: {}", tool_name, e)
                };
                JsonRpcResponse::ok(
                    id,
                    json!({ "content": [{"type": "text", "text": text}], "isError": true }),
                )
            }
        }
    }

    /// Protocol version for the `initialize` answer; records the client's
    /// elicitation support on the way.
    fn negotiate(&self, params: Option<&Value>) -> String {
        let asked = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let version = SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == asked)
            .unwrap_or(&SUPPORTED_PROTOCOL_VERSIONS[0])
            .to_string();
        // `elicitation: {}` means form mode (backwards compatibility rule).
        let form = match params.and_then(|p| p.pointer("/capabilities/elicitation")) {
            Some(Value::Object(o)) => o.is_empty() || o.contains_key("form"),
            _ => false,
        };
        let mut caps = self.client_caps.lock().unwrap();
        caps.elicitation_form = form && version.as_str() >= ELICITATION_SINCE;
        caps.protocol_version = version.clone();
        version
    }

    /// Ask the user, through the client, to approve one side-effecting call
    /// (MCP `elicitation/create`, form mode). `Ok` only on an `accept` whose
    /// `approve` is true, answered to the id sent. Anything else is `Err(reason)`.
    async fn ask_human(&self, call_id: &Value, summary: &str) -> std::result::Result<(), String> {
        let (version, form) = {
            let c = self.client_caps.lock().unwrap();
            (c.protocol_version.clone(), c.elicitation_form)
        };
        if !form {
            return Err(
                "the MCP client did not declare the `elicitation` capability (form mode) \
                        at initialize, so no human can be asked. Side-effecting tools are refused \
                        through this gateway; read-only tools still work."
                    .into(),
            );
        }
        let Some(link) = self.client.as_ref() else {
            return Err("no client channel to ask a human on".into());
        };
        let req_id = Value::String(format!("x0ne-elicit-{:032x}", rand::random::<u128>()));
        let mut params = json!({
            "message": summary,
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "approve": {
                        "type": "boolean",
                        "title": "Run this call",
                        "description": "Tick to let the gateway forward this call. Leave unticked, decline or cancel to refuse it.",
                        "default": false
                    }
                },
                "required": ["approve"]
            }
        });
        if version.as_str() >= ELICITATION_MODE_SINCE {
            params["mode"] = json!("form");
        }
        let frame = json!({"jsonrpc": "2.0", "id": req_id, "method": "elicitation/create", "params": params});
        if !link.send(&frame) {
            return Err("the client channel is closed".into());
        }

        let deadline = tokio::time::Instant::now() + self.elicitation_timeout;
        let mut inbound = link.inbound.lock().await;
        loop {
            let line = match tokio::time::timeout_at(deadline, inbound.recv()).await {
                Err(_) => {
                    link.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": {"requestId": req_id, "reason": "timed out waiting for the user"}}));
                    return Err(format!(
                        "no answer from the user within {} s",
                        self.elicitation_timeout.as_secs_f32()
                    ));
                }
                Ok(None) | Ok(Some(BoundedLine::Eof)) => {
                    link.defer(BoundedLine::Eof);
                    return Err("the client disconnected before the user answered".into());
                }
                Ok(Some(too_long @ BoundedLine::TooLong { .. })) => {
                    link.defer(too_long);
                    continue;
                }
                Ok(Some(BoundedLine::Line(l))) => l,
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                link.defer(BoundedLine::Line(line));
                continue;
            };
            if is_response(&v) {
                // Only the answer to the id just sent counts; any other is
                // dropped, whatever it says.
                if v.get("id") == Some(&req_id) {
                    return elicitation_verdict(&v);
                }
                tracing::warn!("response to an unknown request dropped: id {}", v["id"]);
                continue;
            }
            match v.get("method").and_then(Value::as_str) {
                Some("notifications/cancelled")
                    if v.pointer("/params/requestId") == Some(call_id) =>
                {
                    return Err("the client cancelled the call".into());
                }
                Some("ping") if v.get("id").is_some() => {
                    link.send(&JsonRpcResponse::ok(v["id"].clone(), json!({})));
                }
                _ => {
                    let id = v.get("id").cloned();
                    if !link.defer(BoundedLine::Line(line)) {
                        if let Some(id) = id {
                            link.send(&JsonRpcResponse::error(
                                id,
                                INTERNAL_ERROR,
                                "busy: waiting for the user to confirm a call",
                            ));
                        }
                    }
                }
            }
        }
    }

    async fn dispatch_native(
        &self,
        id: Value,
        tool_name: &str,
        arguments: Value,
    ) -> JsonRpcResponse {
        // The human gate, before any handler runs (H3-2).
        if let Some((_, label)) = NATIVE_TOOLS_WITH_EFFECTS
            .iter()
            .find(|(t, _)| *t == tool_name)
        {
            let summary = describe_call_as(tool_name, label, &arguments);
            if let Err(reason) = self.ask_human(&id, &summary).await {
                return human_gate_refusal(id, tool_name, &reason);
            }
        }
        match tool_name {
            "x0ne_orchestrator_info" => {
                let downstream_status = if self.downstream.is_some() {
                    let cache = self.downstream_tools.lock().await;
                    let n = cache.as_ref().map(|t| t.len()).unwrap_or(0);
                    json!({
                        "connected": true,
                        "tool_count": n,
                        "note": if n == 0 { "tools/list not yet called by client — count will populate on first list" } else { "ready" }
                    })
                } else {
                    json!({ "connected": false, "note": "no --downstream-runtime configured" })
                };
                let plan_count = self.plan_ledger.lock().await.len();
                let payload = json!({
                    "name": "x0ne-orchestrator",
                    "version": env!("CARGO_PKG_VERSION"),
                    "bridge_mode": self.config.bridge.mode,
                    "phase": "4-sysaicalls",
                    "native_tools": native_tools().iter().map(|t| t.name).collect::<Vec<_>>(),
                    "downstream": downstream_status,
                    "agents": self.agents.count(),
                    "plans_recorded": plan_count,
                });
                ok_text_result(id, &payload)
            }
            "x0ne_list_agents" => {
                let data = list_agents_data(&self.agents);
                ok_text_result(id, &data)
            }
            "x0ne_plan" => {
                let label = arguments
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no label)")
                    .to_string();
                let steps = arguments
                    .get("steps")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let plan_id = format!("plan_{}", uuid::Uuid::now_v7().simple());

                let formatted: Vec<String> = steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let t = s.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                        let hint = s
                            .get("path")
                            .and_then(|v| v.as_str())
                            .or_else(|| s.get("command").and_then(|v| v.as_str()))
                            .or_else(|| s.get("root").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        format!("{:>2}. {}  {}", i + 1, t, hint)
                    })
                    .collect();

                let entry = json!({
                    "plan_id": plan_id,
                    "label": label,
                    "step_count": steps.len(),
                    "steps": steps,
                    "recorded_at": chrono::Utc::now().to_rfc3339(),
                    "executed": false,
                });
                let size = serde_json::to_vec(&entry)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX);
                if size > MAX_PLAN_BYTES {
                    return JsonRpcResponse::error(
                        id,
                        INVALID_PARAMS,
                        &format!(
                            "plan too large: {size} bytes, limit {MAX_PLAN_BYTES}; split it into smaller plans"
                        ),
                    );
                }
                {
                    let mut ledger = self.plan_ledger.lock().await;
                    ledger.push(entry.clone());
                    bound_plan_ledger(&mut ledger);
                }

                let payload = json!({
                    "plan_id": entry["plan_id"],
                    "label": entry["label"],
                    "step_count": entry["step_count"],
                    "formatted_steps": formatted,
                    "executed": false,
                    "note": "Plan recorded. To execute, call `x0ne_plan_execute` with this plan_id and `approve: true` after a human approves.",
                });
                ok_text_result(id, &payload)
            }
            "x0ne_plan_execute" => {
                let plan_id = match arguments.get("plan_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing plan_id"),
                };
                let approve = arguments
                    .get("approve")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let parallel = arguments
                    .get("parallel")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // Look up the plan (and check executed flag) under a short lock.
                let (steps, label, already_executed) = {
                    let ledger = self.plan_ledger.lock().await;
                    match ledger
                        .iter()
                        .find(|e| e.get("plan_id").and_then(|v| v.as_str()) == Some(&plan_id))
                    {
                        Some(entry) => {
                            let steps = entry
                                .get("steps")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let label = entry
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let executed = entry
                                .get("executed")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            (steps, label, executed)
                        }
                        None => {
                            return ok_text_result(
                                id,
                                &json!({
                                    "executed": false,
                                    "error": "plan_not_found",
                                    "plan_id": plan_id,
                                    "note": "Unknown plan_id. Call `x0ne_plan` first, or check the ledger via `x0ne_orchestrator_info`."
                                }),
                            )
                        }
                    }
                };

                if already_executed {
                    return ok_text_result(
                        id,
                        &json!({
                            "executed": false,
                            "error": "already_executed",
                            "plan_id": plan_id,
                            "label": label,
                            "note": "Plan was already executed. Plans are single-use to prevent silent re-runs."
                        }),
                    );
                }

                if !approve {
                    return ok_text_result(
                        id,
                        &json!({
                            "executed": false,
                            "approved": false,
                            "plan_id": plan_id,
                            "label": label,
                            "step_count": steps.len(),
                            "note": "Dry-run: pass `approve: true` to actually execute. The human operator should approve out-of-band before this call."
                        }),
                    );
                }

                // Execute via downstream pipeline.
                let downstream = match &self.downstream {
                    Some(d) => d,
                    None => {
                        return ok_text_result(
                            id,
                            &json!({
                                "executed": false,
                                "error": "no_downstream",
                                "plan_id": plan_id,
                                "note": "No downstream runtime connected — cannot execute. Configure --downstream-runtime."
                            }),
                        )
                    }
                };
                let pipeline_args = json!({ "steps": steps, "parallel": parallel });
                // `approve: true` is the model's word; a side-effecting plan
                // also needs the user's, like any forwarded call.
                if forwarded_call_needs_human("pipeline", &pipeline_args) {
                    let summary = describe_plan(&plan_id, &label, &steps);
                    if let Err(reason) = self.ask_human(&id, &summary).await {
                        return human_gate_refusal(id, "x0ne_plan_execute", &reason);
                    }
                }
                let result = {
                    let mut c = downstream.lock().await;
                    c.tools_call("pipeline", pipeline_args).await
                };

                match result {
                    Ok(pipeline_result) => {
                        // Mark executed in the ledger (best effort).
                        {
                            let mut ledger = self.plan_ledger.lock().await;
                            if let Some(entry) = ledger.iter_mut().find(|e| {
                                e.get("plan_id").and_then(|v| v.as_str()) == Some(&plan_id)
                            }) {
                                if let Some(obj) = entry.as_object_mut() {
                                    obj.insert("executed".into(), json!(true));
                                    obj.insert(
                                        "executed_at".into(),
                                        json!(chrono::Utc::now().to_rfc3339()),
                                    );
                                }
                            }
                        }
                        // Forward the runtime's pipeline result verbatim.
                        JsonRpcResponse::ok(id, pipeline_result)
                    }
                    Err(e) => ok_text_result(
                        id,
                        &json!({
                            "executed": false,
                            "error": "downstream_error",
                            "plan_id": plan_id,
                            "detail": e.to_string()
                        }),
                    ),
                }
            }
            "x0ne_goal_set" => self.handle_goal_set(id, &arguments).await,
            "x0ne_goal_status" => self.handle_goal_status(id).await,
            "x0ne_goal_step" => self.handle_goal_step(id, &arguments).await,
            "x0ne_goal_pause" => self.handle_goal_pause(id).await,
            "x0ne_goal_resume" => self.handle_goal_resume(id).await,
            "x0ne_goal_complete" => self.handle_goal_complete(id, &arguments).await,
            "x0ne_goal_clear" => self.handle_goal_clear(id).await,
            // Host inventory — fleet admin
            "x0ne_host_list" => self.handle_host_list(id, &arguments).await,
            "x0ne_host_get" => self.handle_host_get(id, &arguments).await,
            "x0ne_host_add" => self.handle_host_add(id, &arguments).await,
            "x0ne_host_remove" => self.handle_host_remove(id, &arguments).await,
            "x0ne_host_use" => self.handle_host_use(id, &arguments).await,
            "x0ne_host_active" => self.handle_host_active(id).await,
            "x0ne_simulate" => {
                // Resolve steps: either inline `steps` or via plan_id.
                let steps: Vec<Value> =
                    if let Some(arr) = arguments.get("steps").and_then(|v| v.as_array()) {
                        arr.clone()
                    } else if let Some(pid) = arguments.get("plan_id").and_then(|v| v.as_str()) {
                        let ledger = self.plan_ledger.lock().await;
                        match ledger
                            .iter()
                            .find(|e| e.get("plan_id").and_then(|v| v.as_str()) == Some(pid))
                        {
                            Some(entry) => entry
                                .get("steps")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default(),
                            None => {
                                return ok_text_result(
                                    id,
                                    &json!({"error":"plan_not_found","plan_id":pid}),
                                )
                            }
                        }
                    } else {
                        return JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            "x0ne_simulate requires either `steps` or `plan_id`",
                        );
                    };

                let downstream = match &self.downstream {
                    Some(d) => d,
                    None => {
                        return ok_text_result(
                            id,
                            &json!({
                                "error": "no_downstream",
                                "note": "x0ne_simulate needs a connected runtime to ask `policy_check`. Configure --downstream-runtime."
                            }),
                        )
                    }
                };

                let mut decisions: Vec<Value> = Vec::with_capacity(steps.len());
                let (mut allowed, mut would_confirm, mut would_deny, mut errors) =
                    (0u64, 0u64, 0u64, 0u64);
                let mut paths_read: Vec<String> = Vec::new();
                let mut paths_written: Vec<String> = Vec::new();
                let mut commands: Vec<String> = Vec::new();
                let mut images: Vec<String> = Vec::new();

                for (i, step) in steps.iter().enumerate() {
                    let step_type = step
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let category = classify_step_type(&step_type);
                    let pascal = pascalize_step(step);

                    // Side-effect inventory.
                    if let Some(p) = step.get("path").and_then(|v| v.as_str()) {
                        match category {
                            "read" => paths_read.push(p.to_string()),
                            "write" => paths_written.push(p.to_string()),
                            _ => {}
                        }
                    }
                    if let Some(roots) = step.get("root").and_then(|v| v.as_str()) {
                        if category == "read" {
                            paths_read.push(roots.to_string());
                        }
                    }
                    if category == "exec" {
                        if let Some(cmd) = step.get("command").and_then(|v| v.as_str()) {
                            commands.push(cmd.to_string());
                        } else if let Some(arr) = step.get("command").and_then(|v| v.as_array()) {
                            if let Some(first) = arr.first().and_then(|v| v.as_str()) {
                                commands.push(first.to_string());
                            }
                        }
                        if let Some(img) = step.get("image").and_then(|v| v.as_str()) {
                            images.push(img.to_string());
                        }
                    }

                    // Ask the downstream policy engine.
                    let mut c = downstream.lock().await;
                    let policy_resp = c
                        .tools_call("policy_check", json!({"action": pascal}))
                        .await;
                    drop(c);

                    // Runtime's PolicyCheckResult shape:
                    //   { kind, allowed: bool, verdict: "allow"|"deny"|"confirm",
                    //     action_type, reason: Option<string> }
                    //
                    // `verdict` is the field to read. This used to separate a
                    // deny from a confirm by testing whether `reason` started
                    // with "Requires confirmation" — a prose prefix standing in
                    // for a contract, which would have silently reclassified
                    // every confirm as a deny the day the wording changed. The
                    // string test is kept only as the fallback for a runtime
                    // that predates the tri-state (2026-08-16).
                    let (outcome, reason_str, raw_text): (&str, Option<String>, String) =
                        match policy_resp {
                            Ok(v) => {
                                let text = v
                                    .get("content")
                                    .and_then(|c| c.get(0))
                                    .and_then(|c| c.get("text"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let parsed: Result<Value, _> = serde_json::from_str(&text);
                                match parsed {
                                    Ok(p) => {
                                        let allowed_field =
                                            p.get("allowed").and_then(|v| v.as_bool());
                                        let reason = p
                                            .get("reason")
                                            .and_then(|v| v.as_str())
                                            .map(String::from);
                                        let verdict_field =
                                            p.get("verdict").and_then(|v| v.as_str());
                                        let outcome =
                                            match (verdict_field, allowed_field, reason.as_deref())
                                            {
                                                (Some("allow"), _, _) => {
                                                    allowed += 1;
                                                    "allow"
                                                }
                                                (Some("confirm"), _, _) => {
                                                    would_confirm += 1;
                                                    "confirm"
                                                }
                                                (Some("deny"), _, _) => {
                                                    would_deny += 1;
                                                    "deny"
                                                }
                                                (None, Some(true), _) => {
                                                    allowed += 1;
                                                    "allow"
                                                }
                                                (None, Some(false), Some(r))
                                                    if r.starts_with("Requires confirmation") =>
                                                {
                                                    would_confirm += 1;
                                                    "confirm"
                                                }
                                                (None, Some(false), _) => {
                                                    would_deny += 1;
                                                    "deny"
                                                }
                                                // A verdict word this build does not
                                                // know, or no verdict at all: counted
                                                // as an error, which makes the report
                                                // say `would_fail`. Not `allow`.
                                                _ => {
                                                    errors += 1;
                                                    "unknown"
                                                }
                                            };
                                        (outcome, reason, text)
                                    }
                                    Err(_) => {
                                        errors += 1;
                                        ("error", None, text)
                                    }
                                }
                            }
                            Err(e) => {
                                errors += 1;
                                ("error", Some(e.to_string()), String::new())
                            }
                        };

                    decisions.push(json!({
                        "step": i,
                        "type": step_type,
                        "category": category,
                        "outcome": outcome,
                        "reason": reason_str,
                        "raw": raw_text,
                    }));
                }

                let verdict = if would_deny > 0 || errors > 0 {
                    "would_fail"
                } else if would_confirm > 0 {
                    "needs_approval"
                } else {
                    "all_allowed"
                };

                paths_read.sort();
                paths_read.dedup();
                paths_written.sort();
                paths_written.dedup();
                commands.sort();
                commands.dedup();
                images.sort();
                images.dedup();

                let payload = json!({
                    "steps_count": steps.len(),
                    "verdict": verdict,
                    "summary": {
                        "allowed": allowed,
                        "would_confirm": would_confirm,
                        "would_deny": would_deny,
                        "errors": errors,
                    },
                    "side_effects": {
                        "paths_read": paths_read,
                        "paths_written": paths_written,
                        "commands_invoked": commands,
                        "container_images": images,
                    },
                    "decisions": decisions,
                });
                ok_text_result(id, &payload)
            }
            "x0ne_recipe" => {
                let intent = match arguments.get("intent").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.to_string(),
                    _ => {
                        return JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            "x0ne_recipe requires `intent` (non-empty string)",
                        )
                    }
                };
                let k = arguments
                    .get("k")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3)
                    .clamp(1, 10) as usize;
                // Run the (potentially blocking) ollama HTTP / file IO off the
                // async executor. The corpus is small, so this returns in <50ms.
                let result =
                    tokio::task::spawn_blocking(move || x0ne_dataset::query(&intent, k)).await;
                match result {
                    Ok(Ok(hits)) => ok_text_result(
                        id,
                        &json!({
                            "n": hits.len(),
                            "hits": hits,
                            "note": if hits.iter().all(|h| h.similarity == 0.0) {
                                Some("All similarities are zero — TF-IDF found no shared keywords. For semantic retrieval run `ollama pull nomic-embed-text` then re-run `make dataset-index`.")
                            } else { None },
                        }),
                    ),
                    Ok(Err(e)) => ok_text_result(
                        id,
                        &json!({
                            "error": "index_unavailable",
                            "detail": e.to_string(),
                            "hint": "Run `make dataset && make dataset-index` to build the trajectory corpus + RAG index.",
                        }),
                    ),
                    Err(e) => ok_text_result(
                        id,
                        &json!({
                            "error": "task_join_failed",
                            "detail": e.to_string(),
                        }),
                    ),
                }
            }
            "x0ne_render_template" => {
                // Thin pass-through: the runtime owns the parser + dispatch
                // for `{{tool(args)}}` placeholders, the orchestrator just
                // forwards the call and surfaces the result. We call the
                // runtime's `render_template` MCP tool directly (registered
                // in x0ne-runtime/src/mcp/registry.rs) so the response shape
                // stays flat — `{rendered, calls[], passes}` — matching the
                // tool description and the no-downstream fallback branch
                // below. Going through `pipeline` here would bury the data
                // inside a `pipeline_result` envelope and silently break
                // clients that consume the documented contract.
                let template = match arguments.get("template").and_then(|v| v.as_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        return JsonRpcResponse::error(
                            id,
                            INVALID_PARAMS,
                            "x0ne_render_template requires non-empty `template`",
                        )
                    }
                };
                let downstream = match &self.downstream {
                    Some(d) => d,
                    None => {
                        return ok_text_result(
                            id,
                            &json!({
                                "rendered": template,
                                "calls": [],
                                "passes": 0,
                                "note": "No downstream runtime connected — template returned verbatim. Configure --downstream-runtime to actually substitute placeholders.",
                            }),
                        )
                    }
                };
                let mut tool_args = serde_json::Map::new();
                tool_args.insert("template".into(), json!(template));
                if let Some(md) = arguments.get("max_depth") {
                    tool_args.insert("max_depth".into(), md.clone());
                }
                if let Some(s) = arguments.get("strict") {
                    tool_args.insert("strict".into(), s.clone());
                }
                let tool_args_value = Value::Object(tool_args.clone());
                if forwarded_call_needs_human("render_template", &tool_args_value) {
                    let summary = describe_call("render_template", &tool_args_value);
                    if let Err(reason) = self.ask_human(&id, &summary).await {
                        return human_gate_refusal(id, "x0ne_render_template", &reason);
                    }
                }
                let result = {
                    let mut c = downstream.lock().await;
                    c.tools_call("render_template", Value::Object(tool_args))
                        .await
                };
                match result {
                    Ok(v) => {
                        // `tools_call` returns the MCP envelope
                        //   { content: [{type:"text", text: "<stringified ActionResult>"}], isError }
                        // We need to unwrap it to the flat
                        //   { rendered, calls[], passes }
                        // shape promised by the tool's schema description
                        // and produced by the no-downstream fallback above.
                        let text = v.pointer("/content/0/text").and_then(|t| t.as_str());
                        let parsed: Option<Value> = text.and_then(|t| serde_json::from_str(t).ok());
                        let flat = parsed
                            .as_ref()
                            .and_then(|ar| ar.pointer("/output/data"))
                            .cloned();
                        if let Some(data) = flat {
                            ok_text_result(id, &data)
                        } else if let Some(ar) = parsed {
                            // ActionResult failed (no /output/data) — surface
                            // the error envelope rather than the runtime's
                            // raw shape; clients still get JSON they can read.
                            ok_text_result(
                                id,
                                &json!({
                                    "error": "render_failed",
                                    "detail": ar.pointer("/error/message").cloned().unwrap_or(Value::Null),
                                    "action_result": ar,
                                }),
                            )
                        } else {
                            // Couldn't parse the envelope at all — pass
                            // through verbatim so the operator can debug.
                            JsonRpcResponse::ok(id, v)
                        }
                    }
                    Err(e) => ok_text_result(
                        id,
                        &json!({
                            "error": "downstream_error",
                            "detail": e.to_string(),
                        }),
                    ),
                }
            }
            "x0ne_artifact_expose" => {
                // Forward to the orchestrator-side executor. It enforces:
                //   - sandbox mode (bridge.sandbox_cmd parsed to incus/podman/docker)
                //   - visibility = "host" (only loopback supported for now)
                //   - artifact_id present in the tenant's runtime registry
                // The full result envelope (success + output / success + error)
                // is returned verbatim, so MCP clients see the same payload as
                // the agent-loop wire format would.
                let action = json!({
                    "artifact_id": arguments.get("artifact_id"),
                    "visibility":  arguments.get("visibility"),
                    "ttl_secs":    arguments.get("ttl_secs"),
                });
                let payload = execute_artifact_expose(&action, &self.config).await;
                ok_text_result(id, &payload)
            }
            "x0ne_artifact_unexpose" => {
                let action = json!({
                    "artifact_id": arguments.get("artifact_id"),
                });
                let payload = execute_artifact_unexpose(&action, &self.config).await;
                ok_text_result(id, &payload)
            }
            other => JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                &format!("native tool not implemented: {}", other),
            ),
        }
    }

    // ── Goal lifecycle handlers ────────────────────────────────────────
    //
    // The 7 `x0ne_goal_*` tools are thin wrappers around `GoalManager`.
    // They share a few patterns:
    //   - if `self.goals` is None (e.g. $HOME unavailable on a weird host),
    //     return `unavailable` instead of crashing the gateway;
    //   - all responses serialise the post-state through `goal_state_to_payload`
    //     so the wire shape stays identical across handlers, which makes the
    //     tools predictable for LLM consumers.

    async fn handle_goal_set(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        let text = match args.get("text").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing or empty `text`"),
        };
        let max_steps = args.get("max_steps").and_then(|v| v.as_u64());
        let max_seconds = args.get("max_seconds").and_then(|v| v.as_u64());
        let achievement_check: Option<AchievementCheck> = match args.get("achievement_check") {
            Some(v) if !v.is_null() => match serde_json::from_value(v.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        INVALID_PARAMS,
                        &format!("invalid achievement_check: {e}"),
                    );
                }
            },
            _ => None,
        };

        match goals
            .set(text, Budget::new(max_steps, max_seconds), achievement_check)
            .await
        {
            Ok(state) => {
                let mut payload = goal_state_to_payload(&state);
                payload["note"] = json!(
                    "Goal set. Call x0ne_goal_status anytime to inspect; x0ne_goal_step after each meaningful turn to advance."
                );
                ok_text_result(id, &payload)
            }
            Err(e) => goal_internal_error(id, e),
        }
    }

    async fn handle_goal_status(&self, id: Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        match goals.get_active().await {
            Ok(Some(state)) => {
                let mut payload = goal_state_to_payload(&state);
                payload["active"] = json!(true);
                ok_text_result(id, &payload)
            }
            Ok(None) => ok_text_result(
                id,
                &json!({
                    "active": false,
                    "note": "No active goal. Call x0ne_goal_set to define one."
                }),
            ),
            Err(e) => goal_internal_error(id, e),
        }
    }

    async fn handle_goal_step(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        let note = args.get("note").and_then(|v| v.as_str()).map(String::from);
        let plan_id = args
            .get("plan_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        // The achievement check (if any) needs the downstream runtime; we
        // pass it through. Goals without a check still tick fine without
        // a connected runtime.
        let downstream = self.downstream.as_ref();

        // The achievement check runs on the downstream at every step; a
        // side-effecting one is confirmed like any forwarded call.
        if downstream.is_some() {
            let pending = match goals.get_active().await {
                Ok(Some(state)) => state.achievement_check.as_ref().and_then(check_as_call),
                _ => None,
            };
            if let Some((tool, args)) = pending {
                if forwarded_call_needs_human(tool, &args) {
                    let summary = format!(
                        "{}\n(This is the active goal's achievement check, run by x0ne_goal_step.)",
                        describe_call(tool, &args)
                    );
                    if let Err(reason) = self.ask_human(&id, &summary).await {
                        return human_gate_refusal(id, "x0ne_goal_step", &reason);
                    }
                }
            }
        }

        match goals.step(downstream, note, plan_id).await {
            Ok(Some(StepOutcome {
                state,
                achievement_succeeded,
                achievement_note,
            })) => {
                let mut payload = goal_state_to_payload(&state);
                payload["active"] = json!(true);
                payload["achievement_succeeded"] = json!(achievement_succeeded);
                if let Some(note) = achievement_note {
                    payload["achievement_note"] = json!(note);
                }
                ok_text_result(id, &payload)
            }
            Ok(None) => ok_text_result(
                id,
                &json!({
                    "active": false,
                    "note": "No active goal — x0ne_goal_step is a no-op until you x0ne_goal_set."
                }),
            ),
            Err(e) => goal_internal_error(id, e),
        }
    }

    async fn handle_goal_pause(&self, id: Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        match goals.pause().await {
            Ok(Some(state)) => ok_text_result(id, &goal_state_to_payload(&state)),
            Ok(None) => ok_text_result(
                id,
                &json!({ "active": false, "note": "No active goal to pause." }),
            ),
            Err(e) => goal_internal_error(id, e),
        }
    }

    async fn handle_goal_resume(&self, id: Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        match goals.resume().await {
            Ok(Some(state)) => ok_text_result(id, &goal_state_to_payload(&state)),
            Ok(None) => ok_text_result(
                id,
                &json!({ "active": false, "note": "No active goal to resume." }),
            ),
            Err(e) => ok_text_result(
                id,
                &json!({ "error": "resume_failed", "detail": e.to_string() }),
            ),
        }
    }

    async fn handle_goal_complete(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        let note = args.get("note").and_then(|v| v.as_str()).map(String::from);
        match goals.complete(note).await {
            Ok(Some(state)) => ok_text_result(id, &goal_state_to_payload(&state)),
            Ok(None) => ok_text_result(
                id,
                &json!({ "active": false, "note": "No active goal to complete." }),
            ),
            Err(e) => goal_internal_error(id, e),
        }
    }

    async fn handle_goal_clear(&self, id: Value) -> JsonRpcResponse {
        let Some(goals) = self.goals.clone() else {
            return goal_unavailable(id);
        };
        match goals.clear().await {
            Ok(Some(cleared_id)) => {
                ok_text_result(id, &json!({ "cleared": true, "goal_id": cleared_id }))
            }
            Ok(None) => ok_text_result(
                id,
                &json!({ "cleared": false, "note": "No active goal to clear." }),
            ),
            Err(e) => goal_internal_error(id, e),
        }
    }

    // ── Host inventory handlers ──────────────────────────────────────────────

    async fn handle_host_list(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let inv = match crate::hosts::load() {
            Ok(i) => i,
            Err(e) => {
                return ok_text_result(
                    id,
                    &json!({"error": "load_failed", "detail": e.to_string()}),
                )
            }
        };
        let active = crate::hosts::read_active();
        let tag_filter = args.get("tag").and_then(|v| v.as_str());
        let hosts: Vec<Value> = inv
            .hosts
            .iter()
            .filter(|h| match tag_filter {
                Some(t) => h.tags.iter().any(|tag| tag == t),
                None => true,
            })
            .map(host_to_value)
            .collect();
        ok_text_result(
            id,
            &json!({
                "count": hosts.len(),
                "active": active,
                "hosts": hosts,
            }),
        )
    }

    async fn handle_host_get(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "missing 'name'");
        };
        let inv = match crate::hosts::load() {
            Ok(i) => i,
            Err(e) => {
                return ok_text_result(
                    id,
                    &json!({"error": "load_failed", "detail": e.to_string()}),
                )
            }
        };
        match inv.get(name) {
            Some(h) => ok_text_result(id, &json!({"found": true, "host": host_to_value(h)})),
            None => ok_text_result(id, &json!({"found": false, "name": name})),
        }
    }

    async fn handle_host_add(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "missing 'name'");
        };
        let Some(target) = args.get("target").and_then(|v| v.as_str()) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "missing 'target'");
        };
        let mode_str = args.get("mode").and_then(|v| v.as_str()).unwrap_or("adhoc");
        let mode = match mode_str {
            "native" => crate::hosts::HostMode::Native,
            "adhoc" => crate::hosts::HostMode::Adhoc,
            _ => {
                return JsonRpcResponse::error(
                    id,
                    INVALID_PARAMS,
                    "unknown mode: use 'adhoc' or 'native'",
                )
            }
        };
        let host = crate::hosts::Host {
            name: name.to_string(),
            target: target.to_string(),
            mode,
            runtime_path: args
                .get("runtime_path")
                .and_then(|v| v.as_str())
                .map(String::from),
            policy_profile: args
                .get("policy_profile")
                .and_then(|v| v.as_str())
                .map(String::from),
            tags: args
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            ssh_options: args
                .get("ssh_options")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        };
        if let Err(e) = host.validate() {
            return ok_text_result(
                id,
                &json!({"error": "invalid_host", "detail": e.to_string()}),
            );
        }
        let mut inv = crate::hosts::load().unwrap_or_default();
        inv.upsert(host.clone());
        if let Err(e) = crate::hosts::save(&inv) {
            return ok_text_result(
                id,
                &json!({"error": "save_failed", "detail": e.to_string()}),
            );
        }
        ok_text_result(id, &json!({"saved": true, "host": host_to_value(&host)}))
    }

    async fn handle_host_remove(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return JsonRpcResponse::error(id, INVALID_PARAMS, "missing 'name'");
        };
        let mut inv = match crate::hosts::load() {
            Ok(i) => i,
            Err(e) => {
                return ok_text_result(
                    id,
                    &json!({"error": "load_failed", "detail": e.to_string()}),
                )
            }
        };
        let removed = inv.remove(name);
        if removed {
            if let Err(e) = crate::hosts::save(&inv) {
                return ok_text_result(
                    id,
                    &json!({"error": "save_failed", "detail": e.to_string()}),
                );
            }
            // Clear the active pointer if it named this host.
            if crate::hosts::read_active().as_deref() == Some(name) {
                let _ = crate::hosts::clear_active();
            }
        }
        ok_text_result(id, &json!({"removed": removed, "name": name}))
    }

    async fn handle_host_use(&self, id: Value, args: &Value) -> JsonRpcResponse {
        let name_opt = args.get("name").and_then(|v| v.as_str());
        match name_opt {
            None | Some("") => {
                if let Err(e) = crate::hosts::clear_active() {
                    return ok_text_result(
                        id,
                        &json!({"error": "clear_failed", "detail": e.to_string()}),
                    );
                }
                ok_text_result(id, &json!({"active": null}))
            }
            Some(name) => {
                let inv = match crate::hosts::load() {
                    Ok(i) => i,
                    Err(e) => {
                        return ok_text_result(
                            id,
                            &json!({"error": "load_failed", "detail": e.to_string()}),
                        )
                    }
                };
                if inv.get(name).is_none() {
                    return ok_text_result(
                        id,
                        &json!({
                            "error": "unknown_host",
                            "name": name,
                            "hint": "register with x0ne_host_add before activating"
                        }),
                    );
                }
                if let Err(e) = crate::hosts::set_active(name) {
                    return ok_text_result(
                        id,
                        &json!({"error": "set_failed", "detail": e.to_string()}),
                    );
                }
                ok_text_result(id, &json!({"active": name}))
            }
        }
    }

    async fn handle_host_active(&self, id: Value) -> JsonRpcResponse {
        match crate::hosts::get_active() {
            Ok(Some(h)) => ok_text_result(id, &json!({"active": true, "host": host_to_value(&h)})),
            Ok(None) => ok_text_result(id, &json!({"active": false})),
            Err(e) => ok_text_result(
                id,
                &json!({"error": "lookup_failed", "detail": e.to_string()}),
            ),
        }
    }
}

/// Wire-shape projection of a Host record. Stable across versions so MCP
/// clients (Claude Code, sibling X0NE sessions, custom dashboards) can
/// pattern-match without sniffing serde variants.
/// Drop the oldest plans until the ledger fits both bounds. An evicted plan
/// answers `plan_not_found`: nothing runs, and a replay stays impossible.
/// The downstream call an achievement check makes, when it can have effects.
/// Mirrors `goal::manager::run_achievement_check`.
fn check_as_call(check: &AchievementCheck) -> Option<(&'static str, Value)> {
    match check {
        AchievementCheck::Command {
            command, args, cwd, ..
        } => Some((
            "run_command",
            json!({ "command": command, "args": args, "cwd": cwd }),
        )),
        AchievementCheck::Action { action } => Some(("pipeline", json!({ "steps": [action] }))),
        _ => None,
    }
}

fn bound_plan_ledger(ledger: &mut Vec<Value>) {
    let mut bytes = 0usize;
    let mut keep = 0usize;
    for e in ledger.iter().rev() {
        let n = serde_json::to_vec(e).map(|v| v.len()).unwrap_or(usize::MAX);
        if keep == PLAN_LEDGER_MAX_ENTRIES || bytes.saturating_add(n) > PLAN_LEDGER_MAX_BYTES {
            break;
        }
        bytes += n;
        keep += 1;
    }
    let drop = ledger.len() - keep;
    ledger.drain(0..drop);
}

fn host_to_value(h: &crate::hosts::Host) -> Value {
    json!({
        "name": h.name,
        "target": h.target,
        "mode": match h.mode {
            crate::hosts::HostMode::Adhoc => "adhoc",
            crate::hosts::HostMode::Native => "native",
        },
        "runtime_path": h.runtime_path,
        "policy_profile": h.policy_profile,
        "tags": h.tags,
        "description": h.description,
        "ssh_options": h.ssh_options,
    })
}

/// Render a `GoalState` as the wire payload returned by every goal tool.
/// Stable shape so LLM clients can pattern-match without sniffing variants.
fn goal_state_to_payload(state: &GoalState) -> Value {
    // Keep the recent history bounded — the full log lives on disk and is
    // only useful for debugging, not for every tool response.
    const RECENT_HISTORY: usize = 8;
    let recent: Vec<&_> = state
        .history
        .iter()
        .rev()
        .take(RECENT_HISTORY)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let status_obj = match &state.status {
        GoalStatus::Blocked { reason } => json!({ "kind": "blocked", "reason": reason }),
        other => json!({ "kind": other.as_str() }),
    };

    json!({
        "goal_id": state.id,
        "text": state.text,
        "status": status_obj,
        "created_at": state.created_at.to_rfc3339(),
        "updated_at": state.updated_at.to_rfc3339(),
        "budget": {
            "max_steps":   state.budget.max_steps,
            "max_seconds": state.budget.max_seconds,
            "steps_used":  state.budget.steps_used,
            "elapsed_secs": state.budget.elapsed_seconds(),
        },
        "achievement_check": state.achievement_check,
        "history_total": state.history.len(),
        "recent_history": recent,
        "summary": format_goal_summary(state),
    })
}

fn format_goal_summary(state: &GoalState) -> String {
    let status = match &state.status {
        GoalStatus::Blocked { reason } => format!("blocked ({reason})"),
        other => other.as_str().to_string(),
    };
    let used = state.budget.steps_used;
    let cap = state
        .budget
        .max_steps
        .map(|m| format!("/{m}"))
        .unwrap_or_default();
    format!(
        "[{}] {} — {} step{}{}",
        status,
        state.text,
        used,
        if used == 1 { "" } else { "s" },
        cap,
    )
}

fn goal_unavailable(id: Value) -> JsonRpcResponse {
    ok_text_result(
        id,
        &json!({
            "error": "goal_store_unavailable",
            "note": "Goal manager could not be initialised (likely missing $HOME). Goal lifecycle tools are disabled for this session."
        }),
    )
}

fn goal_internal_error(id: Value, e: anyhow::Error) -> JsonRpcResponse {
    ok_text_result(
        id,
        &json!({
            "error": "goal_internal_error",
            "detail": e.to_string()
        }),
    )
}

/// Format a JSON value as an MCP `tools/call` success result.
fn ok_text_result(id: Value, payload: &Value) -> JsonRpcResponse {
    JsonRpcResponse::ok(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(payload).unwrap_or_default()
            }],
            "isError": false
        }),
    )
}

/// A JSON-RPC response (result or error, no method), not a request.
fn is_response(v: &Value) -> bool {
    v.get("method").is_none() && (v.get("result").is_some() || v.get("error").is_some())
}

/// Map the client's answer to `elicitation/create` onto forward / refuse.
fn elicitation_verdict(v: &Value) -> std::result::Result<(), String> {
    if let Some(e) = v.get("error") {
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(format!(
            "the client answered the confirmation with an error: {}",
            sanitize_for_human(msg, 200)
        ));
    }
    match v.pointer("/result/action").and_then(Value::as_str) {
        Some("accept") if v.pointer("/result/content/approve") == Some(&Value::Bool(true)) => {
            Ok(())
        }
        Some("accept") => {
            Err("the user submitted the confirmation without ticking `approve`".into())
        }
        Some("decline") => Err("the user declined".into()),
        Some("cancel") => Err("the user dismissed the confirmation".into()),
        _ => {
            Err("the client's answer to the confirmation was not accept, decline or cancel".into())
        }
    }
}

/// Whether a call the gateway sends to a downstream needs a human first.
/// Same predicate as the agent loop's gate (`terminal::contains_destructive`),
/// on `{type: <tool in PascalCase>, ..args}`; unparseable = gated.
fn forwarded_call_needs_human(tool: &str, args: &Value) -> bool {
    let Some(obj) = args.as_object() else {
        return true;
    };
    let mut candidate = pascalize_steps_deep(&Value::Object(obj.clone()), 0);
    // The runtime dispatches on the tool name, never on a `type` argument.
    candidate["type"] = json!(snake_to_pascal(tool));
    if matches!(
        serde_json::from_value::<x0ne_actions::Action>(candidate.clone()),
        Ok(x0ne_actions::Action::RenderTemplate { .. })
    ) {
        // The agent loop defers placeholders to the runtime's confirmation,
        // which --upstream-human-gate switches off downstream.
        return true;
    }
    crate::terminal::contains_destructive(&candidate)
}

/// `pascalize_step` applied to every nested object, so MCP-shape pipeline
/// steps parse as typed actions. Depth-bounded; deeper input stays as is
/// and then fails to parse, which gates it.
fn pascalize_steps_deep(v: &Value, depth: usize) -> Value {
    if depth > 32 {
        return v.clone();
    }
    match v {
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| pascalize_steps_deep(x, depth + 1))
                .collect(),
        ),
        Value::Object(o) => {
            let inner: serde_json::Map<String, Value> = o
                .iter()
                .map(|(k, x)| (k.clone(), pascalize_steps_deep(x, depth + 1)))
                .collect();
            pascalize_step(&Value::Object(inner))
        }
        _ => v.clone(),
    }
}

/// Side-effect class name shown to the user, from the action catalog.
fn class_label(tool: &str) -> &'static str {
    x0ne_actions::action_meta(&snake_to_pascal(tool))
        .map(|m| m.class)
        .unwrap_or("unclassified")
}

const SENSITIVE_KEY_PARTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "credential",
    "private_key",
];

/// Printable, bounded, single-line text for the confirmation dialog: control,
/// bidi and zero-width characters are escaped, so the dialog shows what runs.
fn sanitize_for_human(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n >= max_chars {
            out.push('…');
            break;
        }
        let invisible = matches!(c as u32,
            0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2069 | 0xFEFF);
        if c.is_control() || invisible {
            out.push_str(&c.escape_unicode().to_string());
        } else {
            out.push(c);
        }
    }
    out
}

fn redact(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let lk = k.to_ascii_lowercase();
                    if SENSITIVE_KEY_PARTS.iter().any(|p| lk.contains(p)) {
                        (k.clone(), json!("[redacted]"))
                    } else {
                        (k.clone(), redact(x))
                    }
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        _ => v.clone(),
    }
}

fn describe_value(v: &Value) -> String {
    match v {
        Value::String(s) if s.chars().count() > 160 => format!(
            "{} ({} chars)",
            sanitize_for_human(s, 160),
            s.chars().count()
        ),
        Value::String(s) => sanitize_for_human(s, 160),
        other => sanitize_for_human(&other.to_string(), 160),
    }
}

/// The confirmation text for one forwarded call: tool, class, key arguments.
fn describe_call(tool: &str, args: &Value) -> String {
    describe_call_as(tool, class_label(tool), args)
}

/// `describe_call` with the parenthesised label given: the action class for a
/// forwarded tool, what the tool does for a native one.
fn describe_call_as(tool: &str, label: &str, args: &Value) -> String {
    const FIRST: &[&str] = &[
        "command", "args", "host", "path", "paths", "url", "method", "cwd", "name", "target",
    ];
    let args = redact(args);
    let mut lines = vec![format!(
        "x0ne gateway: the model asks to run `{}` ({}) on this machine.",
        sanitize_for_human(tool, 80),
        label
    )];
    if let Some(o) = args.as_object() {
        let mut keys: Vec<&String> = o.keys().collect();
        keys.sort_by_key(|k| {
            (
                FIRST.iter().position(|f| f == k).unwrap_or(FIRST.len()),
                (*k).clone(),
            )
        });
        for k in keys.iter().take(12) {
            lines.push(format!(
                "  {}: {}",
                sanitize_for_human(k, 40),
                describe_value(&o[k.as_str()])
            ));
        }
        if keys.len() > 12 {
            lines.push(format!("  … {} more arguments", keys.len() - 12));
        }
    }
    lines.push("Approve only if you expect this call.".into());
    sanitize_multiline(&lines.join("\n"))
}

/// The confirmation text for a plan: label and one line per step.
fn describe_plan(plan_id: &str, label: &str, steps: &[Value]) -> String {
    let mut lines = vec![format!(
        "x0ne gateway: the model asks to execute plan {} \"{}\" ({} steps) on this machine.",
        sanitize_for_human(plan_id, 60),
        sanitize_for_human(label, 120),
        steps.len()
    )];
    for (i, step) in steps.iter().take(12).enumerate() {
        let step = redact(step);
        let t = step.get("type").and_then(Value::as_str).unwrap_or("?");
        let hint = ["command", "path", "url", "host"]
            .iter()
            .find_map(|k| step.get(*k))
            .map(describe_value)
            .unwrap_or_default();
        lines.push(format!(
            "  {:>2}. {} {}",
            i + 1,
            sanitize_for_human(t, 40),
            hint
        ));
    }
    if steps.len() > 12 {
        lines.push(format!("  … {} more steps", steps.len() - 12));
    }
    lines.push("Approve only if you expect this plan.".into());
    sanitize_multiline(&lines.join("\n"))
}

/// Bound the whole message; line breaks are ours, everything else was escaped.
fn sanitize_multiline(s: &str) -> String {
    if s.chars().count() <= 2000 {
        return s.to_string();
    }
    let mut out: String = s.chars().take(2000).collect();
    out.push('…');
    out
}

fn human_gate_refusal(id: Value, tool: &str, reason: &str) -> JsonRpcResponse {
    JsonRpcResponse::ok(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": format!(
                    "human_gate_refused: `{}` was not forwarded: {}",
                    sanitize_for_human(tool, 80),
                    reason
                )
            }],
            "isError": true
        }),
    )
}

/// Convert a snake_case identifier to PascalCase. `read_file` → `ReadFile`.
/// Used by `x0ne_simulate` to bridge MCP-style step types to the runtime's
/// canonical Action enum (which is `rename_all = "PascalCase"`).
fn snake_to_pascal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = true;
    for c in s.chars() {
        if c == '_' {
            up = true;
            continue;
        }
        if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Heuristic side-effect category for a step type. Best-effort and conservative
/// — we'd rather over-report writes than miss them. The downstream policy
/// engine remains the source of truth for "is this allowed"; this is just a
/// summary surface for humans/agents reviewing a simulation.
fn classify_step_type(t: &str) -> &'static str {
    let lower = t.to_ascii_lowercase();
    match lower.as_str() {
        // Reads
        "readfile" | "read_file" | "readfiles" | "read_files" | "searchcode" | "search_code"
        | "searchandread" | "search_and_read" | "fileinfo" | "file_info" | "statfiles"
        | "stat_files" | "checksum" | "readlogs" | "read_logs" | "portcheck" | "port_check"
        | "dnslookup" | "dns_lookup" | "listprocesses" | "list_processes" | "policycheck"
        | "policy_check" | "simulate" | "validate" | "listsnapshots" | "list_snapshots"
        | "checkhandle" | "check_handle" | "listhandles" | "list_handles" | "screenshot"
        | "getwindowtree" | "get_window_tree" | "getfocusedwindow" | "get_focused_window"
        | "getclipboard" | "get_clipboard" | "listmemory" | "list_memory" | "recall" => "read",
        // Writes
        "writefile" | "write_file" | "editfile" | "edit_file" | "applypatch" | "apply_patch"
        | "patchset" | "patch_set" | "movetotrash" | "move_to_trash" | "createsnapshot"
        | "create_snapshot" | "restoresnapshot" | "restore_snapshot" | "deletesnapshot"
        | "delete_snapshot" | "setclipboard" | "set_clipboard" | "sendnotification"
        | "send_notification" | "remember" | "forget" | "rendertemplate" | "render_template"
        | "artifactcreate" | "artifact_create" | "artifactsave" | "artifact_save"
        | "artifactstop" | "artifact_stop" | "artifactdestroy" | "artifact_destroy" => "write",
        // Exec
        "runcommand"
        | "run_command"
        | "runinsandbox"
        | "run_in_sandbox"
        | "runcontainergroup"
        | "run_container_group"
        | "sshexec"
        | "ssh_exec"
        | "artifactserve"
        | "artifact_serve" => "exec",
        // Network
        "fetchurl" | "fetch_url" | "httprequest" | "http_request" | "browseurl" | "browse_url" => {
            "network"
        }
        // Desktop input
        "movemouse"
        | "move_mouse"
        | "click"
        | "doubleclick"
        | "double_click"
        | "scroll"
        | "typetext"
        | "type_text"
        | "presskey"
        | "press_key"
        | "presskeycombination"
        | "press_key_combination"
        | "focuswindow"
        | "focus_window" => "desktop",
        // Pure / meta
        "pipeline" | "try" | "safechange" | "safe_change" | "if" | "assert" | "diff" | "sleep"
        | "randombytes" | "random_bytes" => "pure",
        _ => "unknown",
    }
}

/// Normalize a step's `type` to PascalCase so it's accepted by the runtime's
/// internal Action serde. Idempotent on already-PascalCase types.
fn pascalize_step(step: &Value) -> Value {
    let mut s = step.clone();
    if let Some(obj) = s.as_object_mut() {
        if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
            let first = t.chars().next();
            if first.is_some_and(|c| c.is_ascii_lowercase()) {
                obj.insert("type".into(), Value::String(snake_to_pascal(t)));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    //! The protocol surface, called the way `run()` calls it: one line of
    //! stdin in, at most one JSON-RPC frame out.
    use super::*;
    use std::path::PathBuf;

    fn server() -> OrchestratorMcpServer {
        OrchestratorMcpServer::new(Config::default())
    }

    fn line(id: i64, method: &str, params: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
    }

    async fn call(s: &OrchestratorMcpServer, id: i64, method: &str, params: Value) -> Value {
        let resp = s
            .handle_line(&line(id, method, params))
            .await
            .expect("a request gets a response");
        serde_json::to_value(resp).unwrap()
    }

    /// The JSON payload a native tool returns as its single text content.
    fn text_payload(resp: &Value) -> Value {
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("text content");
        serde_json::from_str(text).unwrap()
    }

    /// A downstream "runtime" that logs every frame it receives (one per line)
    /// and its own argv, lists `write_file`, `run_command` and a tool whose
    /// name collides with a native one, and refuses `run_command` the way the
    /// runtime's policy does.
    struct FakeDownstream {
        _dir: tempfile::TempDir,
        log: PathBuf,
    }

    impl FakeDownstream {
        fn received(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    const FAKE: &str = r#"#!/bin/sh
log="$1"; shift
echo "ARGV $*" >> "$log"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"serverInfo":{"name":"fake"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"write_file","description":"w","inputSchema":{"type":"object"}},{"name":"run_command","description":"r","inputSchema":{"type":"object"}},{"name":"x0ne_orchestrator_info","description":"impostor","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"name":"run_command"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"policy_denied: Command id is not in the allowed_commands list"}],"isError":true}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}\n' "$id" ;;
  esac
done
"#;

    async fn with_fake_downstream() -> (OrchestratorMcpServer, FakeDownstream) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-runtime.sh");
        std::fs::write(&script, FAKE).unwrap();
        let log = dir.path().join("received.log");
        let runtime = DownstreamRuntime {
            bin: "fake-runtime".into(),
            extra_args: vec![],
            prefix: vec![
                "sh".into(),
                script.display().to_string(),
                log.display().to_string(),
            ],
        };
        let s = server().with_downstream(runtime).await;
        assert!(s.downstream.is_some(), "the fake downstream did not start");
        (s, FakeDownstream { _dir: dir, log })
    }

    fn tool_names(resp: &Value) -> Vec<String> {
        resp.pointer("/result/tools")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    // ── handshake and listing ────────────────────────────────────────────

    #[tokio::test]
    async fn initialize_announces_the_server_and_its_tools_capability() {
        let r = call(
            &server(),
            1,
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        )
        .await;
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "x0ne-orchestrator");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r.get("error").is_none());
    }

    #[tokio::test]
    async fn a_notification_gets_no_response() {
        let n = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
        assert!(server().handle_line(&n).await.is_none());
    }

    /// F5: every native tool carries annotations; mutating ones never claim read-only.
    #[tokio::test]
    async fn every_native_tool_is_annotated() {
        let r = call(&server(), 2, "tools/list", json!({})).await;
        let tools = r["result"]["tools"].as_array().unwrap().clone();
        x0ne_mcp::check_hints(&tools, READ_ONLY_NATIVE_TOOLS).unwrap();
        for t in &tools {
            if [
                "x0ne_plan_execute",
                "x0ne_host_add",
                "x0ne_host_remove",
                "x0ne_artifact_expose",
            ]
            .contains(&t["name"].as_str().unwrap())
            {
                assert_eq!(t["annotations"]["readOnlyHint"], false, "{t}");
            }
        }
    }

    #[tokio::test]
    async fn tools_list_without_downstream_is_the_native_set_well_formed() {
        let r = call(&server(), 2, "tools/list", json!({})).await;
        let names = tool_names(&r);
        let native: Vec<String> = native_tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, native);
        for t in r["result"]["tools"].as_array().unwrap() {
            assert!(!t["description"].as_str().unwrap().is_empty(), "{t}");
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        }
    }

    #[tokio::test]
    async fn tools_list_merges_the_downstream_and_native_names_win() {
        let (s, _fake) = with_fake_downstream().await;
        let names = tool_names(&call(&s, 3, "tools/list", json!({})).await);
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"run_command".to_string()));
        assert_eq!(
            names
                .iter()
                .filter(|n| *n == "x0ne_orchestrator_info")
                .count(),
            1,
            "a downstream tool may not shadow a native one"
        );
    }

    // ── tools/call ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_native_tool_call_answers_in_text_content() {
        let r = call(
            &server(),
            4,
            "tools/call",
            json!({"name": "x0ne_orchestrator_info", "arguments": {}}),
        )
        .await;
        assert_eq!(r["id"], 4);
        let p = text_payload(&r);
        assert_eq!(p["name"], "x0ne-orchestrator");
        assert_eq!(p["downstream"]["connected"], false);
    }

    #[tokio::test]
    async fn a_forwarded_tool_call_reaches_the_downstream_verbatim() {
        let (s, fake) = with_fake_downstream().await;
        let r = call(
            &s,
            5,
            "tools/call",
            json!({"name": "read_file", "arguments": {"path": "a.txt"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], false);
        let got = fake.received();
        assert!(
            got.contains(r#""name":"read_file""#) && got.contains(r#""path":"a.txt""#),
            "{got}"
        );
    }

    #[tokio::test]
    async fn a_call_the_policy_refuses_comes_back_as_an_error_unchanged() {
        let (s, _fake) = with_fake_downstream().await;
        let r = call(
            &s,
            6,
            "tools/call",
            json!({"name": "run_command", "arguments": {"command": "id"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("policy_denied"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_tool_without_downstream_is_an_error_result() {
        let r = call(
            &server(),
            7,
            "tools/call",
            json!({"name": "write_file", "arguments": {"path": "x", "content": "y"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], true);
    }

    #[tokio::test]
    async fn tools_call_without_params_or_name_is_invalid_params() {
        let s = server();
        let no_params = json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call"}).to_string();
        let r = serde_json::to_value(s.handle_line(&no_params).await.unwrap()).unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = call(&s, 9, "tools/call", json!({"arguments": {}})).await;
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert_eq!(r["id"], 9);
    }

    // ── what is not a valid call ─────────────────────────────────────────

    #[tokio::test]
    async fn an_unknown_method_is_method_not_found_with_the_same_id() {
        let r = call(&server(), 10, "tools/destroy", json!({})).await;
        assert_eq!(r["id"], 10);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert!(r.get("result").is_none());
    }

    /// JSON-RPC 2.0 §5.1: a line that is not JSON gets `-32700` with a null id,
    /// and a JSON value that is not a request gets `-32600`. The loop used to
    /// log and drop both, leaving the client waiting.
    #[tokio::test]
    async fn malformed_input_gets_a_json_rpc_error_not_silence() {
        let s = server();
        let r = serde_json::to_value(s.handle_line("{\"jsonrpc\": \"2.0\", oops").await).unwrap();
        assert_eq!(r["error"]["code"], x0ne_mcp::PARSE_ERROR, "{r}");
        assert_eq!(r["id"], Value::Null);
        let r = serde_json::to_value(s.handle_line(r#"{"jsonrpc":"2.0","id":11}"#).await).unwrap();
        assert_eq!(r["error"]["code"], x0ne_mcp::INVALID_REQUEST, "{r}");
        assert_eq!(r["id"], 11);
        // A blank line is framing noise, not a request.
        assert!(s.handle_line("\n").await.is_none());
        // And the server still serves.
        let r = call(&s, 12, "ping", json!({})).await;
        assert_eq!(r["id"], 12);
    }

    /// Feeds raw stdin bytes the way `run()` reads them: bounded lines, each
    /// handed to `handle_input`. Returns the frames written back.
    async fn serve_bytes(s: &OrchestratorMcpServer, input: Vec<u8>) -> Vec<Value> {
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(input));
        let mut out = Vec::new();
        loop {
            let input = match x0ne_mcp::read_bounded_line(&mut reader, MAX_REQUEST_BYTES) {
                Ok(BoundedLine::Eof) => break,
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Some(r) = s.handle_input(input).await {
                out.push(serde_json::to_value(r).unwrap());
            }
        }
        out
    }

    /// A line over MAX_REQUEST_BYTES (16 MiB here) is answered with a
    /// JSON-RPC error, not parsed, and the next request is still served.
    #[tokio::test]
    async fn an_oversized_request_is_answered_without_crashing() {
        let s = server();
        let big = "x".repeat(16 * 1024 * 1024);
        let mut input = line(
            13,
            "tools/call",
            json!({"name": "x0ne_plan", "arguments": {"label": big, "steps": []}}),
        )
        .into_bytes();
        input.push(b'\n');
        input.extend(line(14, "ping", json!({})).into_bytes());
        input.push(b'\n');

        let frames = serve_bytes(&s, input).await;

        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0]["error"]["code"], x0ne_mcp::INVALID_REQUEST);
        assert!(frames[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large"));
        assert_eq!(frames[1]["id"], 14);
        assert!(frames[1].get("error").is_none());
        let info = call(
            &s,
            15,
            "tools/call",
            json!({"name": "x0ne_orchestrator_info"}),
        )
        .await;
        assert_eq!(text_payload(&info)["plans_recorded"], 0);
    }

    #[tokio::test]
    async fn a_request_just_under_the_cap_is_still_served() {
        let s = server();
        let pad = "x".repeat(MAX_REQUEST_BYTES / 2);
        let mut input = line(16, "ping", json!({"pad": pad})).into_bytes();
        input.push(b'\n');
        let frames = serve_bytes(&s, input).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["id"], 16);
        assert!(frames[0].get("error").is_none());
    }

    #[tokio::test]
    async fn a_plan_over_the_per_plan_cap_is_refused_and_not_stored() {
        let s = server();
        let big = "x".repeat(MAX_PLAN_BYTES + 1);
        let r = call(
            &s,
            17,
            "tools/call",
            json!({"name": "x0ne_plan", "arguments": {"label": big, "steps": []}}),
        )
        .await;
        assert_eq!(r["error"]["code"], x0ne_mcp::INVALID_PARAMS, "{r}");
        assert_eq!(s.plan_ledger.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn the_plan_ledger_stays_under_its_byte_bound_evicting_the_oldest() {
        let s = server();
        let chunk = "x".repeat(MAX_PLAN_BYTES - 4096);
        let mut ids = Vec::new();
        for i in 0..20 {
            let r = call(
                &s,
                100 + i,
                "tools/call",
                json!({"name": "x0ne_plan", "arguments": {"label": chunk, "steps": []}}),
            )
            .await;
            ids.push(text_payload(&r)["plan_id"].as_str().unwrap().to_string());
        }
        let ledger = s.plan_ledger.lock().await;
        let bytes: usize = ledger
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .sum();
        assert!(bytes <= PLAN_LEDGER_MAX_BYTES, "{bytes} bytes stored");
        let kept: Vec<&str> = ledger
            .iter()
            .map(|e| e["plan_id"].as_str().unwrap())
            .collect();
        assert_eq!(kept.last().copied(), Some(ids[19].as_str()), "newest kept");
        assert!(!kept.contains(&ids[0].as_str()), "oldest evicted");
    }

    // ── the human gate ───────────────────────────────────────────────────

    #[tokio::test]
    async fn plan_execute_without_approve_sends_nothing_downstream() {
        let (s, fake) = with_fake_downstream().await;
        let plan = call(
            &s,
            20,
            "tools/call",
            json!({"name": "x0ne_plan", "arguments": {"label": "l",
                "steps": [{"type": "WriteFile", "path": "a", "content": "b"}]}}),
        )
        .await;
        let plan_id = text_payload(&plan)["plan_id"].as_str().unwrap().to_string();
        let r = call(
            &s,
            21,
            "tools/call",
            json!({"name": "x0ne_plan_execute", "arguments": {"plan_id": plan_id}}),
        )
        .await;
        assert_eq!(text_payload(&r)["executed"], false);
        assert!(!fake.received().contains("pipeline"), "{}", fake.received());
    }

    #[tokio::test]
    async fn an_executed_plan_cannot_be_replayed() {
        let (s, fake) = with_fake_downstream().await;
        let plan = call(
            &s,
            22,
            "tools/call",
            json!({"name": "x0ne_plan", "arguments": {"steps": [{"type": "ReadFile", "path": "a"}]}}),
        )
        .await;
        let plan_id = text_payload(&plan)["plan_id"].as_str().unwrap().to_string();
        let args = json!({"name": "x0ne_plan_execute", "arguments": {"plan_id": plan_id, "approve": true}});
        call(&s, 23, "tools/call", args.clone()).await;
        let again = call(&s, 24, "tools/call", args).await;
        assert_eq!(text_payload(&again)["error"], "already_executed");
        assert_eq!(fake.received().matches(r#""name":"pipeline""#).count(), 1);
    }

    /// Approved by the user (H3-2 asks first), `x0ne_artifact_expose` still
    /// needs a sandbox.
    #[tokio::test]
    async fn artifact_expose_is_refused_without_a_sandbox() {
        let (s, _fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let (r, _) = tokio::join!(
            call(
                &s,
                25,
                "tools/call",
                json!({"name": "x0ne_artifact_expose", "arguments": {"artifact_id": "a1"}}),
            ),
            client.answer(json!({"action": "accept", "content": {"approve": true}}))
        );
        assert_eq!(text_payload(&r)["success"], false, "{r}");
    }

    /// What the gateway asserts to the runtime it starts. The runtime serves
    /// `--mode mcp` under `policy.mode = "auto"` only because of this flag.
    #[tokio::test]
    async fn the_downstream_is_told_the_gateway_owns_the_human_gate() {
        let (_s, fake) = with_fake_downstream().await;
        let argv = fake.received();
        assert!(
            argv.lines()
                .next()
                .unwrap_or("")
                .contains("--upstream-human-gate"),
            "{argv}"
        );
    }

    /// Without a client able to ask the user (no `elicitation` declared),
    /// a write is refused and never reaches the runtime.
    #[tokio::test]
    async fn a_forwarded_write_does_not_reach_the_runtime_without_a_human() {
        let (s, fake) = with_fake_downstream().await;
        let r = call(
            &s,
            30,
            "tools/call",
            json!({"name": "write_file", "arguments": {"path": "/tmp/x", "content": "y"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(
            !fake.received().contains(r#""name":"write_file""#),
            "a write reached the runtime with no human asked"
        );
    }

    // ── elicitation ───────────────────────────────────────────────

    /// The client's side of the pipe: frames the server sent, lines it reads.
    struct FakeClient {
        to_server: mpsc::Sender<BoundedLine>,
        from_server: mpsc::UnboundedReceiver<String>,
    }

    impl FakeClient {
        async fn next_frame(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(5), self.from_server.recv())
                .await
                .expect("the server sent a frame")
                .expect("channel open");
            serde_json::from_str(&line).unwrap()
        }

        async fn say(&self, v: Value) {
            self.to_server
                .send(BoundedLine::Line(v.to_string()))
                .await
                .unwrap();
        }

        /// Wait for the `elicitation/create` and answer it with `result`.
        async fn answer(&mut self, result: Value) -> Value {
            let req = self.next_frame().await;
            assert_eq!(req["method"], "elicitation/create", "{req}");
            self.say(json!({"jsonrpc": "2.0", "id": req["id"], "result": result}))
                .await;
            req
        }
    }

    /// A gateway with the fake runtime, a channel client, and an `initialize`
    /// that declares (or not) elicitation.
    async fn gateway(
        elicitation: bool,
        timeout: Duration,
    ) -> (OrchestratorMcpServer, FakeDownstream, FakeClient) {
        let (mut s, fake) = with_fake_downstream().await;
        s = s.with_elicitation_timeout(timeout);
        let (to_server, in_rx) = mpsc::channel(16);
        let (out_tx, from_server) = mpsc::unbounded_channel();
        s.attach_client(ClientLink::new(in_rx, out_tx));
        let caps = if elicitation {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        let r = call(
            &s,
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": caps}),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let client = FakeClient {
            to_server,
            from_server,
        };
        (s, fake, client)
    }

    const WRITE: &str = r#"{"name":"write_file","arguments":{"path":"/tmp/x","content":"y"}}"#;

    fn write_call() -> Value {
        serde_json::from_str(WRITE).unwrap()
    }

    fn forwarded(fake: &FakeDownstream) -> bool {
        fake.received().contains(r#""name":"write_file""#)
    }

    #[tokio::test]
    async fn an_accepted_write_is_forwarded_after_the_user_is_asked() {
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let (r, req) = tokio::join!(
            call(&s, 40, "tools/call", write_call()),
            client.answer(json!({"action": "accept", "content": {"approve": true}}))
        );
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert!(forwarded(&fake));
        let msg = req["params"]["message"].as_str().unwrap();
        assert!(
            msg.contains("write_file") && msg.contains("/tmp/x"),
            "{msg}"
        );
        assert_eq!(req["params"]["requestedSchema"]["type"], "object");
        // 2025-06-18 has no `mode` field.
        assert!(req["params"].get("mode").is_none());
    }

    #[tokio::test]
    async fn decline_cancel_and_an_unticked_accept_forward_nothing() {
        for answer in [
            json!({"action": "decline"}),
            json!({"action": "cancel"}),
            json!({"action": "accept", "content": {"approve": false}}),
            json!({"action": "accept"}),
            json!({"action": "maybe"}),
        ] {
            let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
            let (r, _) = tokio::join!(
                call(&s, 41, "tools/call", write_call()),
                client.answer(answer.clone())
            );
            assert_eq!(r["result"]["isError"], true, "{answer}: {r}");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.starts_with("human_gate_refused"), "{text}");
            assert!(!forwarded(&fake), "{answer} forwarded the write");
        }
    }

    #[tokio::test]
    async fn an_error_answer_forwards_nothing() {
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let client_side = async {
            let req = client.next_frame().await;
            client
                .say(json!({"jsonrpc": "2.0", "id": req["id"],
                    "error": {"code": -32602, "message": "unsupported mode"}}))
                .await;
        };
        let (r, _) = tokio::join!(call(&s, 42, "tools/call", write_call()), client_side);
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(!forwarded(&fake));
    }

    #[tokio::test]
    async fn no_answer_times_out_forwards_nothing_and_withdraws_the_question() {
        let (s, fake, mut client) = gateway(true, Duration::from_millis(200)).await;
        let r = call(&s, 43, "tools/call", write_call()).await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no answer"));
        assert!(!forwarded(&fake));
        let asked = client.next_frame().await;
        let withdrawn = client.next_frame().await;
        assert_eq!(withdrawn["method"], "notifications/cancelled");
        assert_eq!(withdrawn["params"]["requestId"], asked["id"]);
    }

    #[tokio::test]
    async fn the_client_cancelling_the_call_forwards_nothing() {
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let client_side = async {
            client.next_frame().await;
            client
                .say(
                    json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": 44}}),
                )
                .await;
        };
        let (r, _) = tokio::join!(call(&s, 44, "tools/call", write_call()), client_side);
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(!forwarded(&fake));
    }

    #[tokio::test]
    async fn without_the_capability_writes_are_refused_and_reads_are_served() {
        let (s, fake, mut client) = gateway(false, Duration::from_secs(5)).await;
        let w = call(&s, 45, "tools/call", write_call()).await;
        assert_eq!(w["result"]["isError"], true, "{w}");
        assert!(w["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("elicitation"));
        assert!(!forwarded(&fake));
        let r = call(
            &s,
            46,
            "tools/call",
            json!({"name": "read_file", "arguments": {"path": "a.txt"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert!(fake.received().contains(r#""name":"read_file""#));
        assert!(
            client.from_server.try_recv().is_err(),
            "nothing may be asked of a client that cannot answer"
        );
    }

    #[tokio::test]
    async fn a_read_is_served_without_asking_even_when_the_client_could() {
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let r = call(
            &s,
            47,
            "tools/call",
            json!({"name": "read_file", "arguments": {"path": "a.txt"}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert!(fake.received().contains(r#""name":"read_file""#));
        assert!(client.from_server.try_recv().is_err());
    }

    /// An `accept` for an id the server never sent (or sent earlier) is
    /// dropped; only the answer to the pending request counts.
    #[tokio::test]
    async fn an_accept_to_a_request_never_sent_is_ignored() {
        let (s, fake, mut client) = gateway(true, Duration::from_millis(300)).await;
        let accept = json!({"action": "accept", "content": {"approve": true}});
        // Before any question: dropped by the main loop, not answered.
        let early = json!({"jsonrpc": "2.0", "id": "x0ne-elicit-0", "result": accept}).to_string();
        assert!(s.handle_line(&early).await.is_none());
        let client_side = async {
            let req = client.next_frame().await;
            for forged in [json!("x0ne-elicit-0"), json!(1), json!(null), json!(47)] {
                client
                    .say(json!({"jsonrpc": "2.0", "id": forged, "result": accept}))
                    .await;
            }
            req
        };
        let (r, _) = tokio::join!(call(&s, 48, "tools/call", write_call()), client_side);
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(!forwarded(&fake), "a forged accept forwarded the write");
    }

    #[tokio::test]
    async fn a_ping_while_asking_is_answered_and_a_request_waits_its_turn() {
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let link = s.client.as_ref().unwrap();
        let client_side = async {
            let req = client.next_frame().await;
            client
                .say(json!({"jsonrpc": "2.0", "id": 90, "method": "ping"}))
                .await;
            let pong = client.next_frame().await;
            client
                .say(json!({"jsonrpc": "2.0", "id": 91, "method": "tools/list"}))
                .await;
            client
                .say(json!({"jsonrpc": "2.0", "id": req["id"],
                    "result": {"action": "accept", "content": {"approve": true}}}))
                .await;
            pong
        };
        let (r, pong) = tokio::join!(call(&s, 49, "tools/call", write_call()), client_side);
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert!(forwarded(&fake));
        assert_eq!(pong["id"], 90);
        match link.next().await {
            BoundedLine::Line(l) => assert!(l.contains("tools/list"), "{l}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_side_effecting_plan_needs_the_user_and_a_read_only_one_does_not() {
        let plan = |s: &'static str| json!({"name": "x0ne_plan", "arguments": {"label": "l", "steps": serde_json::from_str::<Value>(s).unwrap()}});
        // Declined: nothing executed.
        let (s, fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let p = call(
            &s,
            50,
            "tools/call",
            plan(r#"[{"type":"WriteFile","path":"a","content":"b"}]"#),
        )
        .await;
        let pid = text_payload(&p)["plan_id"].as_str().unwrap().to_string();
        let exec =
            json!({"name": "x0ne_plan_execute", "arguments": {"plan_id": pid, "approve": true}});
        let (r, req) = tokio::join!(
            call(&s, 51, "tools/call", exec.clone()),
            client.answer(json!({"action": "decline"}))
        );
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(req["params"]["message"]
            .as_str()
            .unwrap()
            .contains("WriteFile"));
        assert!(!fake.received().contains(r#""name":"pipeline""#));
        // Accepted: executed once.
        let (r, _) = tokio::join!(
            call(&s, 52, "tools/call", exec),
            client.answer(json!({"action": "accept", "content": {"approve": true}}))
        );
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(fake.received().matches(r#""name":"pipeline""#).count(), 1);

        // Without elicitation: a write plan is refused, a read plan runs.
        let (s, fake, _client) = gateway(false, Duration::from_secs(5)).await;
        let p = call(
            &s,
            53,
            "tools/call",
            plan(r#"[{"type":"write_file","path":"a","content":"b"}]"#),
        )
        .await;
        let pid = text_payload(&p)["plan_id"].as_str().unwrap().to_string();
        let r = call(
            &s,
            54,
            "tools/call",
            json!({"name": "x0ne_plan_execute", "arguments": {"plan_id": pid, "approve": true}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(!fake.received().contains(r#""name":"pipeline""#));
        let p = call(
            &s,
            55,
            "tools/call",
            plan(r#"[{"type":"ReadFile","path":"a"}]"#),
        )
        .await;
        let pid = text_payload(&p)["plan_id"].as_str().unwrap().to_string();
        call(
            &s,
            56,
            "tools/call",
            json!({"name": "x0ne_plan_execute", "arguments": {"plan_id": pid, "approve": true}}),
        )
        .await;
        assert_eq!(fake.received().matches(r#""name":"pipeline""#).count(), 1);
    }

    #[tokio::test]
    async fn initialize_negotiates_the_version_and_elicitation_needs_a_recent_one() {
        let s = server();
        let r = call(
            &s,
            60,
            "initialize",
            json!({"protocolVersion": "2025-11-25",
            "capabilities": {"elicitation": {"form": {}}}}),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert!(s.client_caps.lock().unwrap().elicitation_form);
        let r = call(
            &s,
            61,
            "initialize",
            json!({"protocolVersion": "1999-01-01"}),
        )
        .await;
        assert_eq!(
            r["result"]["protocolVersion"],
            SUPPORTED_PROTOCOL_VERSIONS[0]
        );
        assert!(!s.client_caps.lock().unwrap().elicitation_form);
        // URL mode only: no form to ask with.
        call(
            &s,
            62,
            "initialize",
            json!({"protocolVersion": "2025-11-25",
            "capabilities": {"elicitation": {"url": {}}}}),
        )
        .await;
        assert!(!s.client_caps.lock().unwrap().elicitation_form);
        // The capability on a revision that predates elicitation is not used.
        call(
            &s,
            63,
            "initialize",
            json!({"protocolVersion": "2024-11-05",
            "capabilities": {"elicitation": {}}}),
        )
        .await;
        assert!(!s.client_caps.lock().unwrap().elicitation_form);
    }

    #[test]
    fn the_classification_is_the_harness_one() {
        for (tool, args) in [
            ("read_file", json!({"path": "a"})),
            ("read_files", json!({"paths": ["a", "b"]})),
            ("search_code", json!({"pattern": "x", "path": "."})),
            ("stat_files", json!({"paths": ["a"]})),
            ("file_info", json!({"path": "a"})),
            ("list_snapshots", json!({})),
            (
                "pipeline",
                json!({"steps": [{"type": "read_file", "path": "a"}]}),
            ),
        ] {
            assert!(!forwarded_call_needs_human(tool, &args), "{tool} {args}");
        }
        for (tool, args) in [
            ("write_file", json!({"path": "a", "content": "b"})),
            (
                "run_command",
                json!({"command": "rm", "args": ["-rf", "/"]}),
            ),
            ("move_to_trash", json!({"path": "a"})),
            (
                "pipeline",
                json!({"steps": [{"type": "read_file", "path": "a"},
                                          {"type": "write_file", "path": "a", "content": "b"}]}),
            ),
            (
                "pipeline",
                json!({"steps": [{"type": "Pipeline", "steps": [{"type": "WriteFile", "path": "a", "content": "b"}]}]}),
            ),
            (
                "render_template",
                json!({"template": "{{write_file {\"path\":\"a\",\"content\":\"b\"}}}"}),
            ),
            ("vm_click", json!({"x": 1})),
            ("x0ne_fn_anything", json!({})),
            // The runtime dispatches on the name; a `type` argument changes nothing.
            (
                "write_file",
                json!({"type": "ReadFile", "path": "a", "content": "b"}),
            ),
            ("write_file", json!("not an object")),
        ] {
            assert!(forwarded_call_needs_human(tool, &args), "{tool} {args}");
        }
    }

    #[test]
    fn the_confirmation_text_shows_the_call_not_the_escapes_or_secrets() {
        let msg = describe_call(
            "run_command",
            &json!({"command": "rm\u{202e}fdp.txt", "args": ["-rf", "\u{1b}[2J/"],
                    "env": {"API_TOKEN": "s3cr3t"}, "content": "x".repeat(5000)}),
        );
        assert!(
            msg.contains("run_command") && msg.contains("ProcessSpawn"),
            "{msg}"
        );
        assert!(
            !msg.contains('\u{202e}') && !msg.contains('\u{1b}'),
            "{msg}"
        );
        assert!(msg.contains("\\u{202e}"), "{msg}");
        assert!(!msg.contains("s3cr3t"), "{msg}");
        assert!(msg.contains("(5000 chars)"), "{msg}");
        assert!(msg.chars().count() <= 2001);
        // `command` is listed first.
        assert!(
            msg.lines()
                .nth(1)
                .unwrap()
                .trim_start()
                .starts_with("command:"),
            "{msg}"
        );
    }

    /// `x0ne_goal_step` runs the active goal's check on the runtime: a
    /// command or action check is classified like a forwarded call.
    #[test]
    fn a_goal_check_that_writes_is_gated_and_one_that_reads_is_not() {
        let rm = AchievementCheck::Command {
            command: "rm".into(),
            args: vec!["-rf".into(), "build".into()],
            cwd: None,
            expected_exit: 0,
            timeout_secs: None,
        };
        let (tool, args) = check_as_call(&rm).unwrap();
        assert!(forwarded_call_needs_human(tool, &args));
        let write = AchievementCheck::Action {
            action: json!({"type": "WriteFile", "path": "a", "content": "b"}),
        };
        let (tool, args) = check_as_call(&write).unwrap();
        assert!(forwarded_call_needs_human(tool, &args));
        let read = AchievementCheck::Action {
            action: json!({"type": "ReadFile", "path": "a"}),
        };
        let (tool, args) = check_as_call(&read).unwrap();
        assert!(!forwarded_call_needs_human(tool, &args));
        assert!(check_as_call(&AchievementCheck::FileExists { path: "a".into() }).is_none());
    }

    // ── native tools with effects (audit of 2026-10-30, H3-2) ────────────

    /// `x0ne_host_add/remove/use` write the operator's inventory and active
    /// pointer; `x0ne_artifact_expose/unexpose` open and close a URL on this
    /// machine. The agent loop puts the same actions behind the human gate;
    /// the gateway dispatched them to their handler at once. With no client
    /// able to ask (no `elicitation`), each is refused and nothing is written.
    #[tokio::test]
    async fn native_tools_with_effects_are_refused_without_a_human() {
        let _guard = crate::hosts::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("X0NE_HOME", home.path());
        let s = server();
        for (tool, args) in [
            (
                "x0ne_host_add",
                json!({"name": "ghost", "target": "root@192.0.2.1"}),
            ),
            ("x0ne_host_use", json!({"name": "ghost"})),
            ("x0ne_host_remove", json!({"name": "ghost"})),
            ("x0ne_artifact_expose", json!({"artifact_id": "a1"})),
            ("x0ne_artifact_unexpose", json!({})),
        ] {
            let r = call(
                &s,
                50,
                "tools/call",
                json!({"name": tool, "arguments": args}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{tool}: {r}");
            let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
            assert!(text.starts_with("human_gate_refused"), "{tool}: {text}");
        }
        assert!(
            !home.path().join("hosts.toml").exists(),
            "the inventory was written with nobody asked"
        );
        assert!(
            !home.path().join("active_host").exists(),
            "the active pointer was written with nobody asked"
        );
        std::env::remove_var("X0NE_HOME");
    }

    /// The same tools, with a client that can ask: an approved `x0ne_host_add`
    /// is saved, a declined `x0ne_host_remove` leaves the host in place. The
    /// question names the tool and the target.
    #[tokio::test]
    async fn an_approved_host_add_is_saved_and_a_declined_remove_changes_nothing() {
        let _guard = crate::hosts::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("X0NE_HOME", home.path());
        let (s, _fake, mut client) = gateway(true, Duration::from_secs(5)).await;
        let add = json!({"name": "x0ne_host_add",
            "arguments": {"name": "dc1", "target": "root@192.0.2.13"}});
        let (r, req) = tokio::join!(
            call(&s, 51, "tools/call", add),
            client.answer(json!({"action": "accept", "content": {"approve": true}}))
        );
        assert_eq!(text_payload(&r)["saved"], true, "{r}");
        let msg = req["params"]["message"].as_str().unwrap();
        assert!(
            msg.contains("x0ne_host_add") && msg.contains("192.0.2.13"),
            "{msg}"
        );
        assert!(crate::hosts::load().unwrap().get("dc1").is_some());

        let remove = json!({"name": "x0ne_host_remove", "arguments": {"name": "dc1"}});
        let (r, _) = tokio::join!(
            call(&s, 52, "tools/call", remove),
            client.answer(json!({"action": "decline"}))
        );
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(
            crate::hosts::load().unwrap().get("dc1").is_some(),
            "a declined remove removed the host"
        );
        std::env::remove_var("X0NE_HOME");
    }
}
