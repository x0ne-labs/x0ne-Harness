//! Parallel Worlds, on the canonical driver.
//!
//! `SpawnWorld` is not a concurrent `SpawnAgent`. Source archaeology settled
//! that: `SpawnAgent` runs inline on the parent's bridge and fills its result
//! before returning, while `SpawnWorld` `tokio::spawn`s a task, opens its
//! **own** runtime, and hands the caller nothing but an acknowledgement. The
//! result arrives later through the registry, which is what `WaitAgent` reads.
//!
//! That difference is the product. Several agents run at once, each isolated,
//! and the `WorldRegistry` exists to show them.
//!
//! # What changes and what does not
//!
//! The dedicated runtime stays. `bridge::create_bridge` in stdio mode spawns
//! an `x0ne-runtime` per world, and here that is deliberate rather than an
//! accident of construction — a world's isolation is part of what a world is.
//! For `SpawnAgent` the same call would have been a silent semantic change,
//! which is why the two paths differ on purpose.
//!
//! What changes is the loop and the authority. The historical task called
//! `run_agent_loop` with `config.clone()`, so a parent that had answered
//! "Always" once passed `ui.auto_confirm` into every world it started. The
//! world now runs on [`Driver`] with a sanitised config and its own
//! `ApprovalAuthority`, and nothing reaches the legacy loop.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::child_result::{agent_result_from_turn, failure_note, startup_failure};
use crate::agent::driver::{AgentState, Driver, DriverLimits, TurnContext, TurnReport};
use crate::agent::session::{sanitize_child_config, NoRecursiveSpawn};
use crate::api::LlmBackend;
use crate::config::Config;
use crate::display_sink::DisplaySink;
use crate::sysaicalls::AgentRegistry;
use crate::worlds_view::WorldEvent;

/// What the parent asked for, read off a `SpawnWorld` action.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldRequest {
    pub id: String,
    pub task: String,
    /// Dollars asked for this world. Clamped by the session's worlds budget.
    pub budget_usd: Option<f64>,
    /// Tokens asked for this world. Clamped by `cost.world_max_tokens`.
    pub max_tokens: Option<u64>,
}

impl Eq for WorldRequest {}

impl WorldRequest {
    pub fn from_action(action: &Value) -> Self {
        Self {
            id: action["id"].as_str().unwrap_or("").to_string(),
            task: action["task"].as_str().unwrap_or("").to_string(),
            budget_usd: action["budget_usd"].as_f64().filter(|v| *v > 0.0),
            max_tokens: action["max_tokens"].as_u64().filter(|v| *v > 0),
        }
    }

    /// The historical checks, in the historical order, with the historical
    /// wording — a model that learned to recover from these messages should
    /// not have to learn again.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("SpawnWorld requires a non-empty 'id'".into());
        }
        if self.task.is_empty() {
            return Err("SpawnWorld requires a non-empty 'task'".into());
        }
        Ok(())
    }
}

/// The acknowledgement a detached spawn returns. Not an outcome: the world has
/// not finished, and saying otherwise is the confusion this type prevents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldHandle {
    pub id: String,
}

/// Start a world and return.
///
/// The contract that separates this from [`SubAgentSpawner`]: `spawn_detached`
/// registers the world, starts it, and returns. Completion arrives through the
/// registry. Unifying the two traits would mean either making a world block or
/// making a sub-agent detached, and both are wrong.
///
/// [`SubAgentSpawner`]: crate::agent::spawner::SubAgentSpawner
pub trait WorldSpawner: Send {
    fn spawn_detached(&mut self, request: WorldRequest) -> Result<WorldHandle, String>;
}

/// So a factory can lend a borrowed spawner without moving it.
impl<T: WorldSpawner + ?Sized> WorldSpawner for &mut T {
    fn spawn_detached(&mut self, request: WorldRequest) -> Result<WorldHandle, String> {
        (**self).spawn_detached(request)
    }
}

/// Everything a world needs to build itself, once it is running in its own
/// task. Owned rather than borrowed, because a detached task outlives the call
/// that started it.
#[derive(Clone)]
pub struct WorldContext {
    pub config: Config,
    /// Where this world's actions are recorded. A world is a detached task,
    /// so it holds an owned handle onto the *parent's* session log: its
    /// effects are the operator's effects, and an audit that stops at the
    /// spawn point is an audit with a hole exactly where the work went.
    pub journal: Option<Arc<dyn crate::agent::journal::ActionJournal>>,
    pub backend: Arc<dyn LlmBackend>,
    pub system_prompt: String,
    pub limits: DriverLimits,
    pub bridge_mode: String,
    pub bridge_bin: String,
    pub bridge_addr: String,
    pub health_timeout: u64,
    pub sandbox_cmd: Option<String>,
    /// Where world dirs are made (`<base>/.x0ne-worlds/<id>`). `None`: the
    /// world shares the parent's directory, and `/worlds` says so.
    pub worlds_base: Option<std::path::PathBuf>,
    /// This world's own dir, set at spawn. The runtime starts in it.
    pub work_dir: Option<std::path::PathBuf>,
    /// Dollars committed to worlds, shared by every world of the session.
    pub pool: Arc<crate::worlds_workspace::WorldsBudget>,
    /// The session's dollar limit when the spawn happens (0 = none).
    pub budget_cap: f64,
    /// What the parent had spent when the spawn happens.
    pub parent_spent: f64,
    /// This world's dollar slice, set at spawn (0 = no cap).
    pub budget_usd: f64,
    /// This world's token cap, set at spawn (0 = no cap).
    pub token_cap: u64,
}

/// What a session lends its worlds besides its config: where their dirs go,
/// and the budget they share.
#[derive(Clone, Default)]
pub struct WorldsSetup {
    pub base: Option<std::path::PathBuf>,
    pub pool: Arc<crate::worlds_workspace::WorldsBudget>,
    pub budget_cap: f64,
    pub parent_spent: f64,
}

/// A backend that refuses the next call once a world has moved `cap` tokens.
pub struct TokenCappedBackend {
    pub inner: Arc<dyn LlmBackend>,
    pub cap: u64,
    pub used: std::sync::atomic::AtomicU64,
}

impl TokenCappedBackend {
    fn check(&self) -> anyhow::Result<()> {
        let used = self.used.load(std::sync::atomic::Ordering::SeqCst);
        if self.cap > 0 && used >= self.cap {
            anyhow::bail!(
                "world token budget exhausted: {used} of {} tokens",
                self.cap
            );
        }
        Ok(())
    }

    fn count(&self, r: &crate::api::ApiResponse) {
        let n = r.input_tokens
            + r.output_tokens
            + r.cache_creation_input_tokens
            + r.cache_read_input_tokens;
        self.used.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl LlmBackend for TokenCappedBackend {
    async fn send(
        &self,
        system: &str,
        messages: &[crate::conversation::Message],
    ) -> anyhow::Result<crate::api::ApiResponse> {
        self.check()?;
        let r = self.inner.send(system, messages).await?;
        self.count(&r);
        Ok(r)
    }

    async fn stream(
        &self,
        system: &str,
        messages: &[crate::conversation::Message],
        tx: tokio::sync::mpsc::UnboundedSender<crate::api::StreamChunk>,
    ) -> anyhow::Result<crate::api::ApiResponse> {
        self.check()?;
        let r = self.inner.stream(system, messages, tx).await?;
        self.count(&r);
        Ok(r)
    }

    fn display_name(&self) -> String {
        self.inner.display_name()
    }

    fn internal_retry(&self) -> crate::api::InternalRetry {
        self.inner.internal_retry()
    }
}

/// Build a world's sink. A world's output is its own, and a detached task
/// cannot borrow the parent's.
pub trait WorldSinkFactory: Send + Sync {
    fn sink(&self, world_id: &str) -> Box<dyn DisplaySink>;
}

/// Open a world's own runtime.
///
/// A seam, so the dedicated-runtime property can be *tested* rather than only
/// read: production opens a real one, a test opens a fake, and either way the
/// parent's transport is not reachable from here. It is also where a startup
/// failure becomes a value instead of a panic in a detached task.
#[async_trait::async_trait]
pub trait WorldRuntime: Send + Sync {
    async fn open(&self, ctx: &WorldContext) -> anyhow::Result<Box<dyn crate::bridge::Bridge>>;
}

/// The production runtime: one `x0ne-runtime` per world.
///
/// In stdio mode `create_bridge` spawns a process (`bridge.rs:437`). For a
/// world that is the point — its own runtime, its own `CompositeStore`, its own
/// filesystem view. For a sub-agent the same call would silently split state,
/// which is why `SpawnAgent` borrows the parent's instead.
pub struct DedicatedRuntime;

#[async_trait::async_trait]
impl WorldRuntime for DedicatedRuntime {
    async fn open(&self, ctx: &WorldContext) -> anyhow::Result<Box<dyn crate::bridge::Bridge>> {
        // `create_bridge_in` is not gated; same requirement as the session.
        crate::sandbox_mode::require_sandbox(&ctx.bridge_mode, ctx.sandbox_cmd.as_deref())?;
        crate::bridge::create_bridge_in(
            &ctx.bridge_mode,
            &ctx.bridge_bin,
            &ctx.bridge_addr,
            ctx.health_timeout,
            ctx.sandbox_cmd.as_deref(),
            ctx.work_dir.as_deref(),
        )
        .await
    }
}

/// Run one world to completion, on the driver, in its own runtime.
///
/// Split out from the spawning so it can be exercised without a `tokio::spawn`
/// and without opening a runtime: the security properties are in here, and a
/// test that has to race a detached task proves them badly.
pub async fn run_world(
    request: &WorldRequest,
    ctx: &WorldContext,
    bridge: Option<Box<dyn crate::bridge::Bridge>>,
    sink: &mut dyn DisplaySink,
    sysaicalls: Option<&mut dyn crate::agent::dispatch::SysaicallHandler>,
) -> TurnReport {
    run_world_metered(request, ctx, bridge, sink, sysaicalls)
        .await
        .0
}

/// The task a world's model receives: the parent's words, after where to work.
pub fn world_task(request: &WorldRequest, ctx: &WorldContext) -> String {
    match &ctx.work_dir {
        Some(dir) => format!(
            "You are parallel world `{id}`. Your working directory is {dir}, an isolated \
             copy of the project made for you alone. Create and edit files only inside it, \
             with absolute paths under {dir}. Other worlds work on the same task elsewhere.\n\n\
             Task: {task}",
            id = request.id,
            dir = dir.display(),
            task = request.task
        ),
        None => request.task.clone(),
    }
}

/// [`run_world`], plus the dollars the world spent, as its budget measured
/// them. `None` when the model has no configured rate: unknown, not zero.
pub async fn run_world_metered(
    request: &WorldRequest,
    ctx: &WorldContext,
    bridge: Option<Box<dyn crate::bridge::Bridge>>,
    sink: &mut dyn DisplaySink,
    sysaicalls: Option<&mut dyn crate::agent::dispatch::SysaicallHandler>,
) -> (TurnReport, Option<f64>) {
    // The authority change. `config.clone()` at `sysaicalls.rs:1167` carried
    // `ui.auto_confirm` into every world; sanitising keeps the ordinary
    // settings and drops the permissions.
    let world_config = sanitize_child_config(&ctx.config);
    let world_prices = crate::agent::pricing::ConfiguredPrices::for_budget(&world_config.cost);
    let mut state = AgentState::new(
        world_config.conversation.max_context_tokens,
        0.0,
        world_config.cost.budget_usd_limit,
    );
    state.budget = state.budget.with_ceiling(world_config.cost.max_session_usd);
    // The world's slice of the session budget replaces the session limit it
    // inherited: N worlds each holding the whole limit could spend N times it.
    if ctx.budget_usd > 0.0 {
        state.budget.limit_usd = ctx.budget_usd;
        state.budget = state.budget.with_ceiling(world_config.cost.max_session_usd);
    }
    state.history.add_user(&world_task(request, ctx));
    let capped: Arc<dyn LlmBackend> = if ctx.token_cap > 0 {
        Arc::new(TokenCappedBackend {
            inner: Arc::clone(&ctx.backend),
            cap: ctx.token_cap,
            used: std::sync::atomic::AtomicU64::new(0),
        })
    } else {
        Arc::clone(&ctx.backend)
    };

    let cancel = crate::agent::cancel::CancelToken::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut owned_bridge = bridge;
    // Reborrowed into this scope: taken as a parameter, its lifetime outlives
    // the locals below and would force `TurnContext` to outlive them too.
    let mut sysaicalls = sysaicalls;

    {
        let bridge_ref: Option<&mut dyn crate::bridge::Bridge> = match owned_bridge.as_mut() {
            Some(b) => Some(&mut **b),
            None => None,
        };
        let mut turn = TurnContext {
            // A world is already a child: it cannot start further children,
            // and `NoRecursiveSpawn` refuses the two actions before this
            // would ever be consulted.
            spawn_factory: None,
            // And no frontend: a detached world has no screen.
            frontend: None,
            backend: &*capped,
            config: &world_config,
            system_prompt: &ctx.system_prompt,
            bridge: bridge_ref,
            sysaicalls: match sysaicalls.as_mut() {
                Some(h) => Some(&mut **h),
                None => None,
            },
            sink: &mut *sink,
            cancel: &cancel,
            sleeper: &crate::agent::model::TokioSleeper,
            prices: &world_prices,
            limits: ctx.limits.clone(),
            chunks: tx,
            journal: ctx.journal.clone(),
            // A world does not advance the parent's goal: the step belongs
            // to the turn the human asked for, not to the detached branch
            // they opened.
            goal: None,
        };
        let report = Driver::new().run_turn(&mut state, &mut turn).await;
        use crate::agent::pricing::PriceList;
        let priced = world_prices.rate_for(&ctx.backend.display_name()).is_some();
        let spent = (report.usage.model_calls == 0 || priced).then_some(state.budget.spent_usd);
        (report, spent)
    }
}

/// The production world spawner.
///
/// Everything it holds is owned or `Arc`-shared, because a detached task
/// outlives the call that started it and cannot borrow the session. The one
/// thing it deliberately does **not** hold is the parent's `Bridge`.
pub struct SessionWorlds {
    pub ctx: WorldContext,
    /// Shared with the session: `WaitAgent`, `ListAgents` and
    /// `BroadcastAgents` all read this, and a world that registered somewhere
    /// else would be invisible to every one of them.
    pub registry: Arc<AgentRegistry>,
    pub sys_state: Arc<std::sync::RwLock<crate::sysmon::SysState>>,
    pub discovered_actions: Option<Vec<Value>>,
    pub sinks: Arc<dyn WorldSinkFactory>,
    /// How a world opens its own runtime. Never the parent's.
    pub runtime: Arc<dyn WorldRuntime>,
}

impl WorldSpawner for SessionWorlds {
    /// Everything observable happens before this returns.
    ///
    /// The ordering is the contract, not an implementation detail. A parent
    /// that gets `Ok` back and immediately calls `ListAgents` or `WaitAgent`
    /// must find the world already `Running`; if registration happened inside
    /// the spawned task, that lookup would race the scheduler and sometimes
    /// answer "not found". So: validate, claim the id, open the channel, take
    /// the inbox, register, emit — then detach.
    fn spawn_detached(&mut self, request: WorldRequest) -> Result<WorldHandle, String> {
        request.validate()?;
        let (id, task) = (request.id.clone(), request.task.clone());
        let handle_id = id.clone();
        if self.registry.status_of(&id).is_some() {
            return Err(format!("Agent '{}' already exists", id));
        }

        // The world's share of the session budget, reserved before anything
        // runs: a refusal here starts nothing.
        let mut ctx = self.ctx.clone();
        let slice = ctx.pool.reserve(
            ctx.budget_cap,
            ctx.parent_spent,
            request.budget_usd,
            crate::worlds_cli::DEFAULT_MAX_WORLDS,
        )?;
        ctx.budget_usd = slice;
        let config_cap = ctx.config.cost.world_max_tokens;
        ctx.token_cap = match (request.max_tokens, config_cap) {
            (Some(a), b) if b > 0 => a.min(b),
            (Some(a), _) => a,
            (None, b) => b,
        };

        // Its own dir. A world that cannot get one does not start in the
        // parent's: sharing it is what isolation exists to prevent.
        let dir = match &ctx.worlds_base {
            Some(base) => match crate::worlds_workspace::prepare(base, &id) {
                Ok(d) => Some(d),
                Err(e) => {
                    ctx.pool.settle(slice, 0.0);
                    return Err(format!("world {id}: no isolated dir: {e:#}"));
                }
            },
            None => None,
        };
        ctx.work_dir = dir.as_ref().map(|d| d.path.clone());

        // Claims the id and opens the result channel under one lock. `None`
        // means the id is taken, which is the historical refusal, verbatim.
        let Some(result_tx) = self.registry.try_insert_pending(&id, &task) else {
            if let Some(d) = &dir {
                let _ = crate::worlds_workspace::drop_dir(d);
            }
            ctx.pool.settle(slice, 0.0);
            return Err(format!("Agent '{}' already exists", id));
        };
        let inbox = self.registry.get_inbox(&id);
        crate::worlds_workspace::ledger().start(crate::worlds_workspace::Entry {
            id: id.clone(),
            task: task.clone(),
            dir,
            budget_usd: slice,
            token_cap: ctx.token_cap,
            status: crate::worlds_workspace::Status::Running,
            cost_usd: None,
            tokens: 0,
            actions: 0,
            duration_ms: 0,
        });

        // The UI half: `/worlds` and the TUI pane read this registry, and a
        // world that only existed in `AgentRegistry` never appeared in either.
        crate::worlds_registry::global().register(&id);
        crate::worlds_registry::global().emit(WorldEvent::ActionStarted {
            world_id: id.clone(),
            action_type: "SpawnWorld".into(),
        });

        let registry = Arc::clone(&self.registry);
        let sys_state = Arc::clone(&self.sys_state);
        let discovered = self.discovered_actions.clone();
        let mut sink = self.sinks.sink(&id);
        let runtime = Arc::clone(&self.runtime);

        tokio::spawn(async move {
            let started = std::time::Instant::now();

            // The world's own runtime. In stdio mode this spawns a dedicated
            // `x0ne-runtime` process, which is what makes a world a world.
            let bridge = match runtime.open(&ctx).await {
                Ok(b) => b,
                Err(e) => {
                    // No turn ran, so there is no report to project. The
                    // record says the world never started rather than
                    // reporting a zeroed success.
                    let result = startup_failure(
                        &id,
                        &task,
                        &format!("Bridge error: {}", e),
                        started.elapsed().as_millis() as u64,
                    );
                    registry.mark_finished(&id, result.clone(), Some(e.to_string()));
                    ctx.pool.settle(ctx.budget_usd, 0.0);
                    crate::worlds_workspace::ledger().finish(
                        &id,
                        crate::worlds_workspace::Status::Failed(format!("bridge: {e}")),
                        Some(0.0),
                        0,
                        0,
                        result.duration_ms,
                    );
                    crate::worlds_registry::global().emit(WorldEvent::WorldFailed {
                        world_id: id.clone(),
                        error: format!("bridge: {}", e),
                    });
                    // Sent last, so a waiter that wakes on it finds the
                    // registry already terminal.
                    let _ = result_tx.send(result);
                    return;
                }
            };

            // The world's own handler: ordinary orchestrator capabilities,
            // minus the two that would make it a parent. Built from owned
            // locals — a detached task has nothing to borrow.
            let world_config = sanitize_child_config(&ctx.config);
            let deps = crate::agent::sysaicall_handler::SessionDeps {
                config: &world_config,
                registry: &registry,
                llm: Arc::clone(&ctx.backend),
                sys_state: &sys_state,
                sys_prompt_base: &ctx.system_prompt,
                discovered_actions: &discovered,
                // The world's own inbox, so `BroadcastAgents` from the parent
                // reaches it and `GetBroadcasts` finds something.
                inbox: inbox.clone(),
                bridge_mode: &ctx.bridge_mode,
                bridge_bin: &ctx.bridge_bin,
                bridge_addr: &ctx.bridge_addr,
                health_timeout: ctx.health_timeout,
                sandbox_cmd: ctx.sandbox_cmd.as_deref(),
            };
            let mut handler = NoRecursiveSpawn {
                inner: Box::new(crate::agent::sysaicall_handler::OrchestratorSysaicalls::new(deps)),
            };

            let (report, spent) =
                run_world_metered(&request, &ctx, Some(bridge), &mut *sink, Some(&mut handler))
                    .await;

            let result = agent_result_from_turn(&id, &task, &report);
            let note = (!result.success).then(|| failure_note(&report));
            registry.mark_finished(&id, result.clone(), note.clone());
            ctx.pool.settle(ctx.budget_usd, spent.unwrap_or(0.0));
            crate::worlds_workspace::ledger().finish(
                &id,
                match &note {
                    None => crate::worlds_workspace::Status::Succeeded,
                    // A turn that never reached prose ended for a reason the
                    // action counts do not carry: say which.
                    Some(n) if report.response().is_none() => {
                        let detail = match &report.outcome {
                            crate::agent::driver::TurnOutcome::ModelFailure { error } => {
                                Some(error.to_string())
                            }
                            other => other.detail(),
                        };
                        crate::worlds_workspace::Status::Failed(match detail {
                            Some(d) => format!("ended: {} ({d}); {n}", report.outcome.kind()),
                            None => format!("ended: {}; {n}", report.outcome.kind()),
                        })
                    }
                    Some(n) => crate::worlds_workspace::Status::Failed(n.clone()),
                },
                spent,
                result.tokens_used,
                result.actions_executed,
                result.duration_ms,
            );

            crate::worlds_registry::global().emit(WorldEvent::ActionFinished {
                world_id: id.clone(),
                action_type: "SpawnWorld".into(),
                duration_ms: result.duration_ms,
            });
            crate::worlds_registry::global().emit(match note {
                None => WorldEvent::WorldCompleted {
                    world_id: id.clone(),
                    summary: Some(format!(
                        "{} tk · {} chars · {} actions",
                        result.tokens_used,
                        result.response.chars().count(),
                        result.actions_executed,
                    )),
                },
                Some(error) => WorldEvent::WorldFailed {
                    world_id: id.clone(),
                    error,
                },
            });

            let _ = result_tx.send(result);
        });

        Ok(WorldHandle { id: handle_id })
    }
}

/// The acknowledgement envelope, in the historical shape.
///
/// `status: "running"` is a statement about the world at the moment the parent
/// is told, and it is true because registration happened before the spawn.
pub fn handle_envelope(handle: &WorldHandle, task: &str) -> Value {
    serde_json::json!({
        "success": true,
        "action_type": "SpawnWorld",
        "output": { "id": handle.id, "status": "running", "task": task },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::approval::ApprovalScope;
    use crate::display_sink::NullDisplaySink;
    use serde_json::json;

    struct QuietBackend;

    #[async_trait::async_trait]
    impl LlmBackend for QuietBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            Ok(crate::api::ApiResponse {
                content: "world done".into(),
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "quiet".into()
        }
    }

    fn ctx_with(config: Config) -> WorldContext {
        WorldContext {
            journal: None,
            config,
            backend: Arc::new(QuietBackend),
            system_prompt: "SYS".into(),
            limits: DriverLimits::default(),
            bridge_mode: "stdio".into(),
            bridge_bin: "x0ne-runtime".into(),
            bridge_addr: String::new(),
            health_timeout: 5,
            sandbox_cmd: None,
            worlds_base: None,
            work_dir: None,
            pool: Arc::default(),
            budget_cap: 0.0,
            parent_spent: 0.0,
            budget_usd: 0.0,
            token_cap: 0,
        }
    }

    /// A world opens its runtime through `create_bridge_in`, which the
    /// session's own check does not cover.
    #[test]
    fn a_world_runtime_checks_the_sandbox_requirement_first() {
        let src = include_str!("world.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap()];
        let open = &production[production
            .find("impl WorldRuntime for DedicatedRuntime")
            .expect("DedicatedRuntime")..];
        let open = &open[..open.find("\n}\n").expect("end of impl")];
        let gate = open
            .find("require_sandbox(")
            .expect("a world opens its runtime without checking X0NE_REQUIRE_SANDBOX");
        let spawn = open.find("create_bridge_in(").expect("create_bridge_in");
        assert!(gate < spawn, "the world runtime opens before the check");
    }

    /// (3) The historical validation, verbatim.
    #[test]
    fn a_world_needs_an_id_and_a_task() {
        assert_eq!(
            WorldRequest::from_action(&json!({"task": "x"})).validate(),
            Err("SpawnWorld requires a non-empty 'id'".into())
        );
        assert_eq!(
            WorldRequest::from_action(&json!({"id": "a"})).validate(),
            Err("SpawnWorld requires a non-empty 'task'".into())
        );
        assert!(WorldRequest::from_action(&json!({"id": "a", "task": "b"}))
            .validate()
            .is_ok());
    }

    /// (6, 8) A world runs on the driver, from its own state.
    #[tokio::test]
    async fn a_world_runs_on_the_driver_with_fresh_state() {
        let request = WorldRequest::from_action(&json!({"id": "w1", "task": "do it"}));
        let report = run_world(
            &request,
            &ctx_with(Config::default()),
            None,
            &mut NullDisplaySink,
            None,
        )
        .await;
        let result = agent_result_from_turn("w1", "do it", &report);
        assert!(result.success);
        assert_eq!(result.response, "world done");
    }

    /// (9, 10, 12) The authority change, behaviourally. A parent that answered
    /// "Always" — which is what `ui.auto_confirm` records — hands the world
    /// nothing, and the world's own authority starts empty.
    #[test]
    fn a_world_inherits_no_authority_from_its_parent() {
        let mut parent_config = Config::default();
        parent_config.ui.auto_confirm = true;
        parent_config.ui.confirm_writes = false;
        // (13) An ordinary setting, to show sanitising is not blanket erasure.
        parent_config.conversation.max_context_tokens = 4321;

        let world_config = sanitize_child_config(&parent_config);
        assert!(!world_config.ui.auto_confirm);
        assert!(world_config.ui.confirm_writes);
        assert_eq!(world_config.conversation.max_context_tokens, 4321);

        let world_state = AgentState::new(
            world_config.conversation.max_context_tokens,
            0.0,
            world_config.cost.budget_usd_limit,
        );
        let scope = ApprovalScope::of(&json!({"type": "MoveToTrash", "path": "/tmp/x"})).unwrap();
        assert!(!world_state.authority.covers(&scope));
        assert_eq!(world_state.authority.grant_count(), 0);
        assert!(world_state.history.is_empty());
    }

    /// (11) And a grant recorded inside a world does not reach the parent.
    #[test]
    fn a_world_grant_does_not_reach_the_parent() {
        let parent = crate::agent::approval::ApprovalAuthority::default();
        let mut world = crate::agent::approval::ApprovalAuthority::default();
        let scope = ApprovalScope::of(&json!({"type": "MoveToTrash", "path": "/tmp/x"})).unwrap();
        let id = world.open_request();
        world
            .record_response(
                crate::agent::approval::ConfirmationResponse {
                    id,
                    outcome: crate::display_sink::ConfirmOutcome::Always,
                },
                Some(&scope),
            )
            .unwrap();
        assert!(world.covers(&scope));
        assert!(!parent.covers(&scope));
    }

    /// (7) A world runs on the driver, not on a loop of its own. Regression
    /// guard: a file that parses model output owns a loop.
    #[test]
    fn guard_the_world_path_never_enters_the_legacy_loop() {
        let src = include_str!("world.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !production.contains("parse_llm_response") && !production.contains("parse::interpret("),
            "a world parses model output itself"
        );
        assert!(
            production.contains("Driver::new().run_turn"),
            "a world must run on the canonical driver"
        );
    }

    /// (4, 5) The dedicated runtime is preserved: the world context carries
    /// the bridge *settings* rather than a borrowed parent bridge, which is
    /// what lets its task open one of its own.
    #[test]
    fn a_world_carries_its_own_runtime_settings_not_the_parents_transport() {
        let src = include_str!("world.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let at = production.find("pub struct WorldContext").unwrap();
        let end = production[at..].find("\n}\n").map(|i| at + i).unwrap();
        let body = &production[at..end];
        for field in ["bridge_mode", "bridge_bin", "bridge_addr", "sandbox_cmd"] {
            assert!(
                body.contains(field),
                "{field} missing from the world context"
            );
        }
        assert!(
            !body.contains("dyn Bridge"),
            "a world holds a parent transport; its runtime would not be its own"
        );
    }

    /// The two contracts stay apart. A world hands back a handle, a sub-agent
    /// hands back an outcome, and collapsing them would make one of the two
    /// wrong.
    #[test]
    fn the_world_contract_is_not_the_sub_agent_contract() {
        let src = include_str!("world.rs");
        let at = src.find("pub trait WorldSpawner").unwrap();
        let end = src[at..].find("\n}\n").map(|i| at + i).unwrap();
        let body = &src[at..end];
        assert!(body.contains("WorldHandle"), "a world must return a handle");
        assert!(
            !body.contains("async"),
            "spawn_detached must return before the world finishes"
        );
    }
}
