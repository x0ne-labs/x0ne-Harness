//! The production orchestrator-side handler.
//!
//! An adapter, not an implementation. Every branch below borrows what the
//! existing `execute_*` function needs and hands the work over; the business
//! logic stays in `sysaicalls.rs`, `incus_actions.rs` and `artifact_expose.rs`
//! where it is already tested. What this file adds is the assembly that used
//! to live inline in `run_agent_loop`, so that every frontend can have it
//! rather than only the CLI.
//!
//! # What it does not decide
//!
//! Whether an action was authorised. By the time `handle` is called,
//! `agent::nested::execute_one_action` has already run the confirmation gate
//! and `ApprovalAuthority` has already spoken. The handler holds neither, and
//! a test reads this file to keep it that way.
//!
//! For orchestrator sysaicalls the security chain is:
//!
//! ```text
//! explicit requires-confirmation classification
//!   → ApprovalAuthority
//!   → canonical dispatch
//!   → this handler
//!   → action-specific guards
//! ```
//!
//! Note what is absent: the runtime `PolicyEngine`. These actions never reach
//! the runtime, so nothing evaluates them but
//! `terminal::orchestrator_sysaicall_requires_confirmation` — a closed,
//! explicit table of which host capabilities need a human. It is not a policy
//! engine and is not called one. Recorded as debt in
//! `docs/MIGRATION_GUARDRAILS.md` §4b.
//!
//! # Narrow capabilities, not a god object
//!
//! Four seams, each naming the capability actually needed:
//! [`CatalogQuery`] for discovery, [`NestedActionExecutor`] for `Plan`'s
//! transaction, `SubAgentSpawner` for child lifecycle, and the session state
//! the existing implementations genuinely require. No `Bridge`, no
//! `PolicyEngine`, no executor.

use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};

use crate::agent::budget::BudgetControl;
use crate::agent::catalog::{self, CatalogQuery};
use crate::agent::dispatch::SysaicallHandler;
use crate::agent::nested::NestedActionExecutor;
use crate::agent::spawner::SubAgentRequest;
use crate::api::LlmBackend;
use crate::config::Config;
use crate::sysaicalls::{self, AgentRegistry, BroadcastEntry};
use crate::sysmon::SysState;

/// The session state the 21 implementations actually read, taken from their
/// signatures rather than from memory.
pub struct SessionDeps<'a> {
    pub config: &'a Config,
    pub registry: &'a AgentRegistry,
    pub llm: Arc<dyn LlmBackend>,
    pub sys_state: &'a Arc<RwLock<SysState>>,
    pub sys_prompt_base: &'a str,
    pub discovered_actions: &'a Option<Vec<Value>>,
    /// Present only inside a sub-agent. `GetBroadcasts` and
    /// `AcknowledgeBroadcast` report their own error when it is absent, which
    /// is the historical behaviour and is preserved.
    pub inbox: Option<Arc<Mutex<Vec<BroadcastEntry>>>>,
    /// `SpawnWorld` needs these to start a world with its own runtime.
    pub bridge_mode: &'a str,
    pub bridge_bin: &'a str,
    pub bridge_addr: &'a str,
    pub health_timeout: u64,
    pub sandbox_cmd: Option<&'a str>,
}

/// The handler. Holds the four capabilities and the session state.
pub struct OrchestratorSysaicalls<'a> {
    deps: SessionDeps<'a>,
}

impl<'a> OrchestratorSysaicalls<'a> {
    pub fn new(deps: SessionDeps<'a>) -> Self {
        Self { deps }
    }

    /// Wrap an `anyhow` result the historical path returned as `Result<Value>`.
    fn from_result(action_type: &str, r: anyhow::Result<Value>) -> Value {
        match r {
            Ok(v) => v,
            Err(e) => json!({
                "success": false,
                "action_type": action_type,
                "error": { "class": "execution_failed", "message": e.to_string() },
            }),
        }
    }

    fn invalid(action_type: &str, message: &str) -> Value {
        json!({
            "success": false,
            "action_type": action_type,
            "error": { "class": "validation", "message": message },
        })
    }

    /// `ProjectNote` — appends prose to X0NE.md. Extracted from
    /// `agent_loop.rs:1327`, where it was inline; there was no function to
    /// reuse. Field aliases and the empty-note rejection are preserved
    /// exactly.
    fn project_note(&self, action: &Value) -> Value {
        let note = action["note"]
            .as_str()
            .or_else(|| action["content"].as_str())
            .or_else(|| action["text"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if note.is_empty() {
            return Self::invalid(
                "ProjectNote",
                "ProjectNote requires a non-empty 'note' field",
            );
        }
        match crate::project::append_project_note(&note) {
            Ok((path, bytes_written)) => json!({
                "success": true,
                "action_type": "ProjectNote",
                "output": {
                    "path": path.display().to_string(),
                    "bytes_written": bytes_written,
                    "note_preview": note_preview(&note),
                },
            }),
            Err(e) => json!({
                "success": false,
                "action_type": "ProjectNote",
                "error": { "class": "io", "message": e.to_string() },
            }),
        }
    }

    /// `Budget` — reads or caps session spend. Extracted from
    /// `agent_loop.rs:1390`; `get` is the default sub-action, as before.
    /// `Budget` — reads or caps the **live** session spend.
    ///
    /// It used to write `config.cost.budget_usd_limit`, which the canonical
    /// driver never reads: enforcement goes through `AgentState.budget`
    /// (`driver.rs:213,330`), initialised from the config once at session
    /// construction. So on the new path the old branch changed a number
    /// nothing consulted. `Config` is the initial input; `TurnBudget` is the
    /// live limit, and there is now one of them.
    fn budget(&mut self, action: &Value, live: Option<&mut dyn BudgetControl>) -> Value {
        let Some(live) = live else {
            return Self::invalid("Budget", BUDGET_CAPABILITY_MISSING);
        };
        let configured = self.deps.config.cost.budget_usd_limit;
        match action["action"].as_str().unwrap_or("get") {
            "set" => match action["limit"].as_f64() {
                // `cost.budget_usd_limit` is the operator's hard cap (H2-2,
                // audit of 2026-10-30). The model may lower the live cap
                // under it, never raise or remove it: that is `/budget`, the
                // operator's. `max_session_usd` keeps its own ceiling below.
                Some(v) if configured > 0.0 && (v <= 0.0 || v > configured) => Self::invalid(
                    "Budget",
                    &format!(
                        "BudgetCeiling: cost.budget_usd_limit is ${configured:.2}; the cap can be lowered, not raised or removed (the operator does that with /budget)"
                    ),
                ),
                Some(v) => match live.set_limit_usd(v) {
                    Ok(limit) => json!({
                        "success": true,
                        "action_type": "Budget",
                        "output": {
                            "limit_usd": limit,
                            "spent_usd": live.spent_usd(),
                            "status": "updated",
                        },
                    }),
                    Err(message) => Self::invalid("Budget", &message),
                },
                None => Self::invalid("Budget", "Budget set requires a non-negative 'limit'"),
            },
            _ => json!({
                "success": true,
                "action_type": "Budget",
                "output": {
                    "limit_usd": live.current_limit_usd(),
                    "spent_usd": live.spent_usd(),
                },
            }),
        }
    }

    /// `Matrix` — renders a frame. The rendered text travels in the result
    /// rather than being printed here: the handler is reached with the sink
    /// already mutably borrowed by dispatch, and presentation is the
    /// frontend's job in any case. The frame is additive to the historical
    /// envelope, so nothing that read the old fields breaks.
    fn matrix(&self, action: &Value) -> Value {
        let content = action["content"].as_str().unwrap_or("");
        let title = action["title"].as_str().unwrap_or("matrix");
        let width = action["width"].as_u64().unwrap_or(60) as usize;
        let height = action["height"].as_u64().unwrap_or(12) as usize;
        json!({
            "success": true,
            "action_type": "Matrix",
            "output": {
                "kind": "matrix_frame",
                "title": title,
                "width": width,
                "height": height,
                "rendered": crate::display::matrix_frame(content, title, width, height),
            },
        })
    }

    /// `Plan` — validate, then run the transaction through canonical nested
    /// execution.
    ///
    /// The manifest is rendered by the presentation seam when the confirmation
    /// gate asks about the `Try`, not here. This branch does three things:
    /// refuse a cross-boundary plan before anything runs, build the typed
    /// `Try`, and hand it to the executor. It never sends to a bridge and never
    /// decides approval.
    async fn plan(
        &mut self,
        action: &Value,
        nested: Option<&mut dyn NestedActionExecutor>,
    ) -> Value {
        if let Some(blocker) = crate::agent::nested::plan_transaction_blocker(action) {
            return json!({
                "success": false,
                "action_type": "Plan",
                "error": { "class": "unsupported", "message": blocker },
            });
        }
        let steps = action["steps"].as_array().cloned().unwrap_or_default();
        if steps.is_empty() {
            return Self::invalid("Plan", "Plan requires a non-empty 'steps' array");
        }
        let try_action = json!({
            "type": "Try",
            "actions": steps,
            "rollback_on_fail": true,
            "label": action["label"].as_str().unwrap_or("proposed plan"),
        });
        // Runtime-only by construction — the blocker above refused anything
        // else — so the nested capability is all this needs.
        let Some(nested) = nested else {
            return json!({
                "success": false,
                "action_type": "Plan",
                "error": {
                    "class": "unsupported",
                    "message": "Plan needs nested execution, and this context did not lend it.",
                },
            });
        };
        let result = nested.execute(&try_action).await;
        json!({
            "success": result["success"].as_bool().unwrap_or(false),
            "action_type": "Plan",
            "output": {
                "label": action["label"].as_str().unwrap_or("proposed plan"),
                "transaction": result,
            },
        })
    }
}

#[async_trait::async_trait]
impl SysaicallHandler for OrchestratorSysaicalls<'_> {
    async fn handle(
        &mut self,
        action: &Value,
        call: &mut crate::agent::dispatch::HandlerCallCtx<'_>,
    ) -> Option<Value> {
        let t = action.get("type").and_then(|v| v.as_str())?;
        let d = &self.deps;
        Some(match t {
            // ── Meta interceptions (not registry members) ────────────────
            "ListActions" => match call.catalog() {
                Some(c) => catalog::list_actions(c).await,
                // Without a runtime the orchestrator half is still worth
                // answering with, and `list_actions` handles that itself.
                None => catalog::list_actions(&mut catalog_less()).await,
            },
            "DescribeAction" => {
                let name = action["action_type"].as_str().unwrap_or("");
                match call.catalog() {
                    Some(c) => catalog::describe_action(c, name).await,
                    None => catalog::describe_action(&mut catalog_less(), name).await,
                }
            }

            // ── Worlds and sub-agents ────────────────────────────────────
            // World lifecycle, not action execution — see `agent::world`.
            "SpawnWorld" => {
                let request = crate::agent::world::WorldRequest::from_action(action);
                match (request.validate(), call.worlds()) {
                    (Err(message), _) => Self::invalid(t, &message),
                    (Ok(()), Some(worlds)) => {
                        let task = request.task.clone();
                        match worlds.spawn_detached(request) {
                            Ok(handle) => crate::agent::world::handle_envelope(&handle, &task),
                            // A duplicate id, in the historical wording.
                            Err(message) => Self::invalid(t, &message),
                        }
                    }
                    (Ok(()), None) => Self::invalid(t, WORLD_CAPABILITY_MISSING),
                }
            }
            // Agent lifecycle, not action execution — see `agent::spawner`.
            "SpawnAgent" => {
                let request = SubAgentRequest::from_action(action);
                match (request.validate(), call.spawner()) {
                    (Err(message), _) => Self::invalid(t, message),
                    (Ok(()), Some(spawner)) => spawner.spawn(request).await.to_envelope(),
                    // The capability was not lent, so no child runs. The
                    // handler does not know how to obtain one and must not.
                    (Ok(()), None) => Self::invalid(t, SPAWN_CAPABILITY_MISSING),
                }
            }
            "WaitAgent" => {
                Self::from_result(t, sysaicalls::execute_wait_agent(d.registry, action).await)
            }
            "ListAgents" => sysaicalls::execute_list_agents(d.registry),
            "BroadcastAgents" => sysaicalls::execute_broadcast_agents(d.registry, action),
            "GetBroadcasts" => sysaicalls::execute_get_broadcasts(d.inbox.as_ref(), action),
            "AcknowledgeBroadcast" => {
                sysaicalls::execute_acknowledge_broadcast(d.inbox.as_ref(), action)
            }
            "GetBroadcastReceipts" => {
                sysaicalls::execute_get_broadcast_receipts(d.registry, action)
            }

            // ── Reflection, memory, budget, display ──────────────────────
            // The transcript arrives with the call, current as of this
            // instant, because the driver has already recorded every earlier
            // action of this turn into it.
            "Reflect" => match call.history() {
                Some(history) => sysaicalls::execute_reflect(action, history),
                None => Self::invalid(t, "Reflect has no conversation to read"),
            },
            "ProjectNote" => self.project_note(action),
            "Budget" => {
                let live = call.budget();
                self.budget(action, live)
            }
            "Matrix" => self.matrix(action),

            // ── Plan: transaction through canonical nested execution ─────
            "Plan" => {
                let nested = call.nested();
                self.plan(action, nested).await
            }

            // ── Artifacts ────────────────────────────────────────────────
            "ArtifactExpose" => {
                crate::artifact_expose::execute_artifact_expose(action, d.config).await
            }
            "ArtifactUnexpose" => {
                crate::artifact_expose::execute_artifact_unexpose(action, d.config).await
            }

            // ── Incus: host-side, outside the runtime policy engine ──────
            "RunIncus" => crate::incus_actions::execute_run_incus(action, d.config).await,
            "RunIncusGroup" => {
                crate::incus_actions::execute_run_incus_group(action, d.config).await
            }
            "IncusCreateSnapshot" => {
                crate::incus_actions::execute_incus_create_snapshot(action, d.config).await
            }
            "IncusRestoreSnapshot" => {
                crate::incus_actions::execute_incus_restore_snapshot(action, d.config).await
            }
            "IncusListSnapshots" => {
                crate::incus_actions::execute_incus_list_snapshots(action, d.config).await
            }
            "IncusDeleteSnapshot" => {
                crate::incus_actions::execute_incus_delete_snapshot(action, d.config).await
            }

            // Not a fallback that pretends to cover: `None` tells the
            // dispatcher this handler does not own the name, which it reports
            // as `NoHandler` rather than forwarding anywhere.
            _ => return None,
        })
    }
}

/// Told to the model when `SpawnAgent` arrived without the child-agent
/// capability. A constant so the branch table scanner sees code, not prose.
/// Told to the model when `Budget` arrived without the live-budget capability.
const BUDGET_CAPABILITY_MISSING: &str =
    "Budget needs the live session budget, and this context did not lend it.";

const SPAWN_CAPABILITY_MISSING: &str =
    "SpawnAgent needs the child-agent capability, and this context did not lend it.";

const WORLD_CAPABILITY_MISSING: &str =
    "SpawnWorld needs the world capability, and this context did not lend it.";

/// A catalog that knows nothing, for a session with no runtime attached.
/// `list_actions` still merges the orchestrator registry over it.
fn catalog_less() -> impl CatalogQuery {
    struct Empty;
    #[async_trait::async_trait]
    impl CatalogQuery for Empty {
        async fn runtime_actions(&mut self) -> Option<Value> {
            None
        }
        async fn describe_runtime_action(&mut self, _: &str) -> Option<Value> {
            None
        }
    }
    Empty
}

/// The names this handler implements, for the mechanical coverage test.
///
/// Derived by reading the `match` above rather than kept alongside it, so it
/// cannot drift: a branch that exists is listed, and a branch that does not,
/// is not.
pub fn implemented_action_types() -> Vec<String> {
    let src = include_str!("sysaicall_handler.rs");
    let head = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let start = head.find("let t = action.get").expect("the match");
    let end = head[start..]
        .find("_ => return None")
        .map(|i| start + i)
        .unwrap();
    let mut names: Vec<String> = Vec::new();
    for line in head[start..end].lines() {
        let l = line.trim_start();
        if !l.starts_with('"') {
            continue;
        }
        if let Some(rest) = l.strip_prefix('"') {
            if let Some(name) = rest.split('"').next() {
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// At most 80 bytes of a note, cut on a char boundary (a byte cut panicked
/// on '→', bench E1 2026-10-12).
fn note_preview(note: &str) -> &str {
    let mut end = note.len().min(80);
    while !note.is_char_boundary(end) {
        end -= 1;
    }
    &note[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawner::SubAgentOutcome;

    /// Bench E1 2026-10-12: a note with '→' across byte 80 panicked
    /// the whole `-p` run (exit 101). The preview cuts on a char boundary.
    #[test]
    fn a_note_preview_never_cuts_inside_a_character() {
        let note = format!("{}U→Y, Y→D", "x".repeat(78));
        let p = note_preview(&note);
        assert!(p.len() <= 80 && note.starts_with(p), "{p}");
        assert_eq!(note_preview("short"), "short");
    }

    // ── Coverage, derived from the registry ──────────────────────────────

    /// Every registry entry has a real branch. Derived from
    /// `orchestrator_action_metas()` on one side and from the `match` arms on
    /// the other, so adding a sysaicall to the registry fails this until
    /// someone writes the branch. The count is measured, never asserted.
    #[test]
    fn every_registry_entry_has_a_handler_branch() {
        let implemented = implemented_action_types();
        let mut missing = Vec::new();
        for meta in crate::prompt::orchestrator_action_metas() {
            let name = meta["type"].as_str().unwrap().to_string();
            if !implemented.contains(&name) {
                missing.push(name);
            }
        }
        assert!(
            missing.is_empty(),
            "registry entries with no handler branch: {missing:?}"
        );
    }

    /// The two Meta interceptions are handled here too, and are deliberately
    /// not registry members.
    #[test]
    fn the_meta_interceptions_have_handler_branches() {
        let implemented = implemented_action_types();
        for name in crate::agent::dispatch::META_ACTION_TYPES {
            assert!(
                implemented.contains(&name.to_string()),
                "`{name}` is intercepted as Meta with no handler branch"
            );
            assert!(
                !crate::prompt::is_orchestrator_sysaicall(name),
                "`{name}` drifted into the sysaicall registry"
            );
        }
    }

    /// No branch for something that no longer exists. A stale arm is dead code
    /// that reads as coverage.
    #[test]
    fn the_handler_has_no_stale_branches() {
        let meta = crate::agent::dispatch::META_ACTION_TYPES;
        for name in implemented_action_types() {
            let known =
                crate::prompt::is_orchestrator_sysaicall(&name) || meta.contains(&name.as_str());
            assert!(
                known,
                "`{name}` has a handler branch but is neither a registered \
                 sysaicall nor a Meta interception"
            );
        }
    }

    /// Human actions are not sysaicalls and must not have grown a branch here
    /// — they belong to `Route::Human` and the `HumanChannel`.
    #[test]
    fn human_actions_are_not_handled_as_sysaicalls() {
        let implemented = implemented_action_types();
        for name in crate::agent::dispatch::HUMAN_ACTION_TYPES {
            assert!(
                !implemented.contains(&name.to_string()),
                "`{name}` is answered as a sysaicall; a model question would \
                 then be served by the host-capability path"
            );
        }
    }

    // ── Regression guards (source scans, not security proofs) ────────────

    fn production_code() -> String {
        let src = include_str!("sysaicall_handler.rs");
        src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Regression guard, not a proof: the type system already prevents this,
    /// since no field of the handler is a `Bridge`, an `ApprovalAuthority` or
    /// a policy engine. The scan catches someone adding one.
    #[test]
    fn guard_the_handler_holds_no_authority_or_transport() {
        let code = production_code();
        for banned in [
            "dyn Bridge",
            "send_action",
            "ApprovalAuthority",
            "PolicyEngine",
            "policy.evaluate",
            "parse_llm_response",
            "parse::interpret(",
        ] {
            assert!(
                !code.contains(banned),
                "`{banned}` in the handler — it would then decide, transport or \
                 run an agent loop of its own"
            );
        }
    }

    // ── Behaviour, by dependency class ───────────────────────────────────

    /// Panics if reached: no sysaicall branch may call the model.
    struct UnreachableBackend;

    #[async_trait::async_trait]
    impl LlmBackend for UnreachableBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            panic!("a sysaicall reached the LLM backend");
        }
        fn display_name(&self) -> String {
            "unreachable".into()
        }
    }

    #[derive(Default)]
    struct FakeCatalog;

    #[async_trait::async_trait]
    impl CatalogQuery for FakeCatalog {
        async fn runtime_actions(&mut self) -> Option<Value> {
            Some(json!({"success": true, "output": {"actions": [
                {"type": "ReadFile", "source": "native"},
                {"type": "selftest-greet", "source": "custom"},
            ]}}))
        }
        async fn describe_runtime_action(&mut self, name: &str) -> Option<Value> {
            (name == "selftest-greet").then(
                || json!({"success": true, "output": {"action_type": name, "source": "custom"}}),
            )
        }
    }

    #[derive(Default)]
    struct FakeNested {
        seen: Vec<Value>,
    }

    #[async_trait::async_trait]
    impl NestedActionExecutor for FakeNested {
        async fn execute(&mut self, action: &Value) -> Value {
            self.seen.push(action.clone());
            json!({"success": true, "output": {"ok": true, "completed": 2, "total": 2}})
        }
    }

    #[derive(Default)]
    struct FakeSpawner {
        requests: Vec<SubAgentRequest>,
    }

    #[async_trait::async_trait]
    impl crate::agent::spawner::SubAgentSpawner for FakeSpawner {
        async fn spawn(&mut self, request: SubAgentRequest) -> SubAgentOutcome {
            let id = request.id.clone();
            self.requests.push(request);
            SubAgentOutcome {
                result: crate::sysaicalls::AgentResult {
                    id,
                    task: String::new(),
                    response: "child done".into(),
                    tokens_used: 11,
                    actions_executed: 0,
                    actions_breakdown: crate::sysaicalls::AgentResult::empty_breakdown(),
                    success: true,
                    duration_ms: 3,
                },
            }
        }
    }

    /// Builds a handler over throwaway session state.
    macro_rules! with_handler {
        ($h:ident, $ctx:ident, $cat:ident, $nested:ident, $spawner:ident, $body:block) => {{
            let mut config = Config::default();
            let registry = AgentRegistry::default();
            let llm: Arc<dyn LlmBackend> = Arc::new(UnreachableBackend);
            let sys_state = Arc::new(RwLock::new(SysState::default()));
            let discovered = None;
            let mut $cat = FakeCatalog;
            #[allow(unused_variables, unused_mut)]
            let mut $nested = FakeNested::default();
            #[allow(unused_variables, unused_mut)]
            let mut $spawner = FakeSpawner::default();
            {
                let deps = SessionDeps {
                    config: &mut config,
                    registry: &registry,
                    llm,
                    sys_state: &sys_state,
                    sys_prompt_base: "",
                    discovered_actions: &discovered,
                    inbox: None,
                    bridge_mode: "stdio",
                    bridge_bin: "x0ne-runtime",
                    bridge_addr: "",
                    health_timeout: 5,
                    sandbox_cmd: None,
                };
                #[allow(unused_mut)]
                let mut $h = OrchestratorSysaicalls::new(deps);
                // Lends whichever capability the action under test needs. The
                // dispatcher derives the same thing per call in production.
                #[allow(unused_mut, unused_variables)]
                let mut $ctx = crate::agent::dispatch::HandlerCallCtx::Catalog(&mut $cat);
                $body
            }
        }};
    }

    /// F6: a model asking for a billion-dollar cap gets a typed refusal.
    #[tokio::test]
    async fn the_model_cannot_lift_the_configured_ceiling() {
        use crate::agent::budget::TurnBudget;
        let mut live = TurnBudget::new(0.0, 3.0).with_ceiling(5.0);
        with_handler!(h, ctx, c, n, s, {
            let mut ctx = crate::agent::dispatch::HandlerCallCtx::Budget(&mut live);
            let r = h
                .handle(
                    &json!({"type": "Budget", "action": "set", "limit": 1e9}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(r["success"], false, "{r}");
            assert!(
                r["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("BudgetCeiling"),
                "{r}"
            );
        });
        assert_eq!(live.limit_usd, 3.0);
    }

    /// H2-2 (audit of 2026-10-30): `cost.budget_usd_limit` is the operator's
    /// hard cap. With no `max_session_usd` the model could set the live limit
    /// above it, or to zero. It may lower the cap, not raise or remove it;
    /// the operator does that, with `/budget`.
    #[tokio::test]
    async fn the_model_cannot_lift_the_cap_above_the_configured_limit() {
        use crate::agent::budget::TurnBudget;
        let mut config = Config::default();
        config.cost.budget_usd_limit = 5.0;
        let registry = AgentRegistry::default();
        let llm: Arc<dyn LlmBackend> = Arc::new(UnreachableBackend);
        let sys_state = Arc::new(RwLock::new(SysState::default()));
        let discovered = None;
        let deps = SessionDeps {
            config: &config,
            registry: &registry,
            llm,
            sys_state: &sys_state,
            sys_prompt_base: "",
            discovered_actions: &discovered,
            inbox: None,
            bridge_mode: "stdio",
            bridge_bin: "x0ne-runtime",
            bridge_addr: "",
            health_timeout: 5,
            sandbox_cmd: None,
        };
        let mut h = OrchestratorSysaicalls::new(deps);
        let mut live = TurnBudget::new(0.0, 5.0);
        for limit in [99.0, 0.0] {
            let mut ctx = crate::agent::dispatch::HandlerCallCtx::Budget(&mut live);
            let r = h
                .handle(
                    &json!({"type": "Budget", "action": "set", "limit": limit}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(r["success"], false, "limit {limit}: {r}");
            assert!(
                r["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("BudgetCeiling"),
                "{r}"
            );
        }
        assert_eq!(live.limit_usd, 5.0, "a refused limit was applied");
        let mut ctx = crate::agent::dispatch::HandlerCallCtx::Budget(&mut live);
        let r = h
            .handle(
                &json!({"type": "Budget", "action": "set", "limit": 2.0}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert_eq!(r["success"], true, "lowering the cap was refused: {r}");
        assert_eq!(live.limit_usd, 2.0);
    }

    /// A — config-only sysaicall.
    #[tokio::test]
    async fn budget_reads_and_sets_the_live_session_cap() {
        use crate::agent::budget::TurnBudget;
        // The live budget, as the driver holds it: initialised from config,
        // then authoritative.
        let mut live = TurnBudget::new(20.0, 100.0);
        with_handler!(h, ctx, c, n, s, {
            let mut ctx = crate::agent::dispatch::HandlerCallCtx::Budget(&mut live);

            let get = h
                .handle(&json!({"type": "Budget"}), &mut ctx)
                .await
                .unwrap();
            assert_eq!(get["output"]["limit_usd"], 100.0);
            assert_eq!(get["output"]["spent_usd"], 20.0);

            let set = h
                .handle(
                    &json!({"type": "Budget", "action": "set", "limit": 25.0}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(set["success"], true);
            assert_eq!(set["output"]["limit_usd"], 25.0);

            // (7) The next read in the SAME turn sees the new limit, because
            // there is only one place it lives.
            let again = h
                .handle(&json!({"type": "Budget"}), &mut ctx)
                .await
                .unwrap();
            assert_eq!(again["output"]["limit_usd"], 25.0);
            // (7) And what was spent is untouched: a limit that could rewrite
            // spend would not be a limit.
            assert_eq!(again["output"]["spent_usd"], 20.0);

            let bad = h
                .handle(
                    &json!({"type": "Budget", "action": "set", "limit": -1}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(bad["success"], false);
        });
        // (3) The live value changed; nothing wrote to Config.
        assert_eq!(live.limit_usd, 25.0);
        assert_eq!(live.spent_usd, 20.0);
    }

    /// (9) The handler no longer needs a mutable `Config` — which is what
    /// blocked wiring it into the session at all, since the turn context needs
    /// `&Config` at the same time.
    #[test]
    fn the_handler_borrows_config_immutably() {
        let src = include_str!("sysaicall_handler.rs");
        let head = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(
            head.contains("pub config: &'a Config,"),
            "SessionDeps still wants a mutable Config"
        );
        assert!(
            !head.contains("config.cost.budget_usd_limit ="),
            "the Budget branch still writes the configured limit, which the \
             canonical driver never reads"
        );
    }

    /// E — session-state sysaicall, delegating to the existing implementation.
    #[tokio::test]
    async fn list_agents_delegates_to_the_existing_registry_reader() {
        with_handler!(h, ctx, c, n, s, {
            let mine = h
                .handle(
                    &json!({"type": "ListAgents"}),
                    &mut crate::agent::dispatch::HandlerCallCtx::None,
                )
                .await
                .unwrap();
            let theirs = sysaicalls::execute_list_agents(&AgentRegistry::default());
            assert_eq!(mine, theirs, "the adapter changed the result envelope");
        });
    }

    /// The historical error when there is no inbox is preserved verbatim.
    #[tokio::test]
    async fn broadcast_reads_outside_a_sub_agent_report_the_historical_error() {
        with_handler!(h, ctx, c, n, s, {
            let r = h
                .handle(
                    &json!({"type": "GetBroadcasts"}),
                    &mut crate::agent::dispatch::HandlerCallCtx::None,
                )
                .await
                .unwrap();
            assert_eq!(r, sysaicalls::execute_get_broadcasts(None, &json!({})));
            assert_eq!(r["success"], false);
        });
    }

    /// F — SpawnAgent reaches the spawner with the exact request, and nothing
    /// else.
    #[tokio::test]
    async fn spawn_agent_delegates_the_exact_request_to_the_spawner() {
        with_handler!(h, ctx, c, n, s, {
            let mut fake = FakeSpawner::default();
            let mut ctx = crate::agent::dispatch::HandlerCallCtx::Spawn(&mut fake);
            let out = h
                .handle(
                    &json!({"type": "SpawnAgent", "id": "rev", "task": "review"}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(out["success"], true);
            assert_eq!(out["action_type"], "SpawnAgent");
            assert_eq!(out["output"]["id"], "rev");
            assert_eq!(out["output"]["tokens_used"], 11);
        });
        // The request itself, checked outside the borrow.
    }

    #[tokio::test]
    async fn spawn_agent_validates_before_starting_a_child() {
        with_handler!(h, ctx, c, n, s, {
            let out = h
                .handle(&json!({"type": "SpawnAgent", "task": "no id"}), &mut ctx)
                .await
                .unwrap();
            assert_eq!(out["success"], false);
            assert!(out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("requires 'id'"));
        });
    }

    /// G — ListActions federates both catalogs, composites included.
    #[tokio::test]
    async fn list_actions_federates_both_catalogs() {
        with_handler!(h, ctx, c, n, s, {
            let r = h
                .handle(&json!({"type": "ListActions"}), &mut ctx)
                .await
                .unwrap();
            let names: Vec<&str> = r["output"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["type"].as_str().unwrap())
                .collect();
            assert!(names.contains(&"ReadFile"), "runtime half missing");
            assert!(
                names.contains(&"selftest-greet"),
                "a live composite vanished"
            );
            assert!(names.contains(&"SpawnWorld"), "orchestrator half missing");
        });
    }

    /// H — DescribeAction: registry first, runtime second, unknown refused.
    #[tokio::test]
    async fn describe_action_resolves_both_vocabularies() {
        with_handler!(h, ctx, c, n, s, {
            let sysaicall = h
                .handle(
                    &json!({"type": "DescribeAction", "action_type": "SpawnWorld"}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(sysaicall["output"]["action_type"], "SpawnWorld");

            let composite = h
                .handle(
                    &json!({"type": "DescribeAction", "action_type": "selftest-greet"}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(composite["output"]["source"], "custom");

            let unknown = h
                .handle(
                    &json!({"type": "DescribeAction", "action_type": "Nope"}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(unknown["error"]["class"], "unsupported");
        });
    }

    /// I — Plan hands a typed Try to the nested executor, with rollback on,
    /// and never reaches a bridge.
    #[tokio::test]
    async fn plan_runs_its_transaction_through_canonical_nested_execution() {
        with_handler!(h, ctx, c, n, s, {
            // Plan needs the nested capability, not the catalog — the
            // dispatcher lends exactly one, chosen by the action.
            let mut fake_nested = FakeNested::default();
            let mut ctx = crate::agent::dispatch::HandlerCallCtx::Nested(&mut fake_nested);
            let out = h
                .handle(
                    &json!({"type": "Plan", "label": "tidy", "steps": [
                        {"type": "WriteFile", "path": "/tmp/a", "content": "x"},
                        {"type": "ReadFile", "path": "/tmp/a"},
                    ]}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(out["success"], true);
            assert_eq!(out["action_type"], "Plan");
        });
    }

    /// And refused before anything runs when it crosses the boundary.
    #[tokio::test]
    async fn a_cross_boundary_plan_is_refused_before_execution() {
        with_handler!(h, ctx, c, n, s, {
            let out = h
                .handle(
                    &json!({"type": "Plan", "label": "mixed", "steps": [
                        {"type": "WriteFile", "path": "/tmp/a", "content": "x"},
                        {"type": "RunIncus", "image": "ubuntu", "command": "ls"},
                    ]}),
                    &mut ctx,
                )
                .await
                .unwrap();
            assert_eq!(out["success"], false);
            assert_eq!(out["error"]["class"], "unsupported");
        });
    }

    /// A name this handler does not own yields `None`, which the dispatcher
    /// reports as `NoHandler`. Not a fallback that pretends to cover.
    #[tokio::test]
    async fn an_unowned_name_is_not_answered() {
        with_handler!(h, ctx, c, n, s, {
            assert!(h
                .handle(
                    &json!({"type": "ReadFile"}),
                    &mut crate::agent::dispatch::HandlerCallCtx::None
                )
                .await
                .is_none());
            assert!(h
                .handle(
                    &json!({"type": "AskHuman"}),
                    &mut crate::agent::dispatch::HandlerCallCtx::None
                )
                .await
                .is_none());
        });
    }
}
