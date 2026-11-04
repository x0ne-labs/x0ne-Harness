//! The single execution-routing authority.
//!
//! Every action, from every frontend, passes through here. Before this, each
//! loop decided for itself where an action should go and which gates it should
//! meet, and the answers diverged: the CLI routed all 26 orchestrator
//! sysaicalls locally, serve hand-rolled an 11-arm match that omitted the
//! Incus family — defeating a routing decision documented as a
//! *privilege-boundary fix*, since the runtime may be running **inside** the
//! very container those actions manage — and TUI, Matrix and the legacy TUI
//! routed almost nothing locally at all, so the orchestrator tool surface
//! simply did not exist there.
//!
//! The order below is the whole point, and it is the same for every caller:
//!
//! ```text
//! action
//!   │
//!   ├─ 1. runtime policy preflight ── would refuse ─> RuntimePolicyRefused
//!   │        (runtime-routed actions the gate would ask about; no side effect)
//!   ├─ 2. confirmation gate ── denied ─────────────> DispatchOutcome::Denied
//!   │        (fail closed: no human ⇒ deny)
//!   ├─ 3. cancellation check ── cancelled ─────────> DispatchOutcome::Cancelled
//!   │
//!   └─ 4. route
//!          ├─ orchestrator sysaicall ──> local handler
//!          └─ runtime action ─────────> bridge, under the cancel token
//! ```
//!
//! Step 1 is the newest and the least obvious. The gate is the orchestrator's
//! authority and the runtime's policy engine is the runtime's; asking a human
//! to approve something the other authority has already decided to refuse
//! produces a refusal after a yes, which is what the operator of 2026-08-15
//! read as the gate being broken. The preflight cannot approve anything — see
//! [`crate::agent::preflight`] — it can only spare the human a question whose
//! answer could not have mattered.
//!
//! The gate runs *before* routing, deliberately. An orchestrator-dispatched
//! action never reaches the runtime, so the runtime's own policy engine can
//! never speak for it; gating after routing would leave exactly the hole that
//! let `BadgeWatch` hand a model-supplied command to `bash -c`.

use serde_json::Value;

use crate::agent::cancel::{CancelToken, Cancellable};
use crate::agent::confirm::{self, ConfirmationDecision, DenialReason};
use crate::bridge::Bridge;
use crate::config::Config;
use crate::display_sink::DisplaySink;

/// Where an action is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Forwarded to x0ne-runtime, where the policy engine is authoritative.
    Runtime,
    /// Executed by the orchestrator itself. These never reach the runtime, so
    /// this dispatcher's gate is the only one they meet.
    Orchestrator,
    /// Asks a person a question. Answered through the frontend's
    /// `DisplaySink`, which is the only thing that has a human on the other
    /// end. Never forwarded to the runtime — see [`HUMAN_ACTION_TYPES`].
    Human,
    /// Known to the vocabulary and deliberately given no route here. Reported
    /// to the model as unsupported rather than executed anywhere.
    Unsupported,
    /// A third-party MCP tool, `<connector>.<tool>` (`crate::connectors`).
    /// Gated by the connector's own policy, which yolo does not waive.
    Connector,
}

/// Actions whose whole purpose is to reach a person.
///
/// Separate from `ORCHESTRATOR_ACTION_TYPES` because the reason is different.
/// A sysaicall runs orchestrator-side because of *privilege* — the runtime may
/// be inside the container it would manage. These run orchestrator-side
/// because of *plumbing*: the runtime implements them
/// (`x0ne-runtime/src/executor/human.rs`) by reading stdin, and under the
/// bridge its stdin is the protocol pipe (`agent_loop.rs:1136-1139`). Asking a
/// question there consumes a protocol frame or waits forever for one.
///
/// The CLI has always intercepted these inline. They were simply never added
/// to the table the canonical dispatcher reads, so a frontend migrating onto
/// `dispatch` would have inherited the hang. Naming them here makes the
/// boundary explicit instead of implicit in one loop's control flow.
pub const HUMAN_ACTION_TYPES: &[&str] = &["AskHuman", "Confirm", "AskMultipleChoice"];

/// Runtime actions the orchestrator answers itself, for capability federation.
///
/// A third interception class, and the reason differs again from the other two.
/// `HUMAN_ACTION_TYPES` is intercepted for plumbing; the orchestrator registry
/// is local for privilege. These are local because **only the orchestrator can
/// see both catalogs**: the runtime knows its generated vocabulary and the
/// user's live composite primitives, the orchestrator knows its 21 sysaicalls,
/// and a caller asking "what can you do?" deserves one answer rather than half.
///
/// They are ordinary, unprivileged reads. Interception here is federation, not
/// protection, and they carry no host capability of their own.
pub const META_ACTION_TYPES: &[&str] = &["ListActions", "DescribeAction"];

/// Decide where an action goes, by positive membership on every branch.
///
/// X0NE has two vocabularies and they stay separate: `x0ne-actions` defines
/// what the runtime executes, and the orchestrator registry
/// (`prompt::orchestrator_action_metas`) defines the sysaicalls the
/// orchestrator executes itself. Both are generated or centrally held, so
/// neither is a list anyone maintains by hand here.
///
/// `Route` is the **execution destination**; an interception class is the
/// **reason** the orchestrator takes ownership. Two of the three interception
/// classes share one destination, and that is correct rather than a
/// simplification: a capability query and a container command both run
/// orchestrator-side, for entirely different reasons, and a fifth `Route`
/// variant would be documentation dressed as control flow.
///
/// 1. [`HUMAN_ACTION_TYPES`] — needs a person → `Human`.
/// 2. [`META_ACTION_TYPES`] — capability federation → `Orchestrator`.
/// 3. Orchestrator registry — a privileged host sysaicall → `Orchestrator`.
/// 4. Runtime vocabulary → `Runtime`, where policy is authoritative.
/// 5. Neither → [`Route::Unsupported`].
///
/// The order is load-bearing at step 2: `ListActions` and `DescribeAction`
/// **are** members of the runtime vocabulary, so testing membership before
/// interception would send them to the runtime and the orchestrator half of
/// the catalog would vanish. The intercepting entries come first for exactly
/// that reason.
///
/// There is no catch-all. An earlier version defaulted unknown names to the
/// runtime on the reasoning that it would reject them, which was true but made
/// the taxonomy a two-way split with a bucket: every name "had a route"
/// because every name fell into the same one, and a completeness test over
/// that is nearly a tautology. A wire name belonging to neither vocabulary is
/// now named rather than forwarded, and the model is told it is unsupported
/// instead of receiving a deserialization error from a process that should
/// never have been asked.
pub fn route_of(action_type: &str) -> Route {
    if HUMAN_ACTION_TYPES.contains(&action_type) {
        Route::Human
    } else if META_ACTION_TYPES.contains(&action_type) {
        Route::Orchestrator
    } else if crate::connectors::is_connector_tool(action_type) {
        Route::Connector
    } else if crate::prompt::is_orchestrator_sysaicall(action_type) {
        Route::Orchestrator
    } else if x0ne_actions::all_type_names().contains(&action_type) {
        Route::Runtime
    } else {
        Route::Unsupported
    }
}

/// What happened to one action.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// It ran. `result` is the runtime's `ActionResult` envelope, or the
    /// orchestrator handler's equivalent.
    Executed {
        route: Route,
        result: Value,
        duration_ms: u64,
    },
    /// Refused before execution. Nothing ran.
    Denied { reason: DenialReason },
    /// The runtime's policy engine would refuse this, so nobody was asked.
    ///
    /// Distinct from [`DispatchOutcome::Denied`], which is the *human* gate
    /// saying no (or nobody being there to say anything). This one is the
    /// other authority: a switch under `[actions]`, a command missing from
    /// `allowed_commands`, a path outside the roots. `reason` is the runtime's
    /// own message, kept verbatim because it names the TOML key that lifts the
    /// refusal — the operator of 2026-08-15 had to guess where that key lived.
    ///
    /// Nothing ran, and `gate` says whether a human was interrupted on the way
    /// here — which is the half the model is told about, and the half that was
    /// wrong until 2026-08-16.
    RuntimePolicyRefused {
        action_type: String,
        reason: String,
        gate: PolicyRefusalGate,
    },
    /// Cancellation won, either before dispatch or while the call was in
    /// flight. Nothing was left half-applied by this layer.
    Cancelled,
    /// An orchestrator sysaicall with no handler wired in this context.
    ///
    /// Distinct from an error: it means the caller has not supplied the
    /// handler, not that the action failed. Treated as a refusal so a frontend
    /// missing its handler cannot silently forward an orchestrator action to
    /// the runtime and defeat the privilege boundary.
    NoHandler { action_type: String },
    /// The bridge itself failed — the runtime is unreachable.
    BridgeError { message: String },
    /// A `[[hooks.pre_tool]]` command refused it (`crate::hooks`). Nothing ran
    /// and nobody was asked. `hook` is the command, `reason` its own words.
    HookDenied {
        action_type: String,
        hook: String,
        reason: String,
    },
    /// The session is in plan mode and this action could change the host
    /// (`crate::agent::plan_mode`). Nothing ran and nobody was asked.
    PlanModeRefused { action_type: String },
}

/// Was a human asked before this policy refusal became final?
///
/// # Why the model has to be told which one it was
///
/// A refusal the operator can lift goes to the session gate (`lift_policy`),
/// and the operator can answer no there. Until 2026-08-16 both roads produced
/// the same sentence — "…so the user was not asked" — so the operator who had
/// just pressed `3` watched the model be told nobody had asked him (G1,
/// 576c5cb). The two are different facts and they call for different next
/// moves: one is a wall the model should report and stop pushing on, the other
/// is a decision a person made and can unmake by answering differently.
///
/// SECURITY: this distinguishes *messages*, never *permissions*. Both variants
/// are refusals, nothing ran in either, and no variant here can authorize
/// anything. Only [`Self::OperatorDeclined`] may claim a human answered, and it
/// is minted at exactly one place: the arm where `decide_policy_lift` returned
/// [`crate::agent::confirm::DenialReason::HumanDeclined`], which is reached
/// only by a validated `No` from the approval authority. Every fail-closed path
/// — EOF, no channel, a stale or forged response id, a grant the runtime
/// refused, a bridge that could not carry it — lands on [`Self::NotAsked`],
/// because none of them is a person's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRefusalGate {
    /// The refusal was final before any question: not liftable, or nobody was
    /// reachable to be asked, or the grant could not be delivered.
    NotAsked,
    /// The operator was shown the refusal and answered no.
    OperatorDeclined,
}

impl DispatchOutcome {
    /// How long the action actually took, in milliseconds.
    ///
    /// Only `Executed` ran anything; every other outcome took no measurable
    /// time by definition, and reporting a duration for a refusal would
    /// invent one.
    ///
    /// This exists because the driver was emitting a hardcoded `0` into
    /// `ToolEnd` while this figure sat unused three frames up the stack —
    /// so every action in every transcript read `(<1ms)`, and a `cargo test`
    /// that took twelve seconds looked exactly like an `ls`. A duration that
    /// is always the same number is not a measurement.
    pub fn duration_ms(&self) -> u64 {
        match self {
            Self::Executed { duration_ms, .. } => *duration_ms,
            Self::Denied { .. }
            | Self::RuntimePolicyRefused { .. }
            | Self::Cancelled
            | Self::NoHandler { .. }
            | Self::BridgeError { .. }
            | Self::HookDenied { .. }
            | Self::PlanModeRefused { .. } => 0,
        }
    }

    /// The envelope handed back to the model. Every outcome produces one, so
    /// the model always learns what happened; silence is what let a stuck
    /// model keep retrying.
    /// `action` is the action this outcome came from, and `discovered_actions`
    /// the catalog the session advertised — both needed to say *which* action
    /// failed and what its schema was.
    ///
    /// This is the canonical result-normalization point (decision B2B):
    /// everything the model is told about an action passes through here, once,
    /// before history and the next request. It is deliberately a method on the
    /// outcome rather than a middleware chain — there are two normalizations
    /// and both are about making a runtime error recoverable, which is a
    /// narrow enough job to stay a function.
    ///
    /// It takes no sink. Model-facing semantics must not depend on which
    /// frontend is attached, which is exactly what would happen if enrichment
    /// lived in presentation.
    pub fn to_result_envelope(
        &self,
        action: &Value,
        discovered_actions: Option<&[Value]>,
    ) -> Value {
        match self {
            DispatchOutcome::Executed { result, .. } => {
                let mut enriched = enrich_unknown_action(result);
                // The runtime says `missing field \`path\``, which names
                // neither the action nor the schema. `agent_loop` has expanded
                // that at the orchestrator boundary since T10B; moving it here
                // means every future frontend inherits the same feedback
                // instead of each one rediscovering that small models cannot
                // recover from the bare form.
                crate::agent_loop::enrich_validation_error(
                    action,
                    &mut enriched,
                    discovered_actions,
                );
                enriched
            }
            DispatchOutcome::Denied { reason } => serde_json::json!({
                "success": false,
                "error": { "class": "policy_denied", "message": reason.feedback() },
            }),
            // Same class as the human denial — from the model's side both mean
            // "policy refused this" — but the message is the runtime's, and it
            // names the setting. A model told only "denied" retries; a model
            // told which switch is off can say so to the user.
            DispatchOutcome::RuntimePolicyRefused {
                action_type,
                reason,
                gate,
            } => serde_json::json!({
                "success": false,
                "error": {
                    "class": "policy_denied",
                    "message": match gate {
                        PolicyRefusalGate::NotAsked => format!(
                            "`{action_type}` was refused by the runtime's policy before it \
                             ran, so the user was not asked: {reason}"
                        ),
                        // The operator WAS asked, and said no. Saying "the user
                        // was not asked" here told the model the opposite of
                        // what had just happened on screen (G1, 2026-08-16).
                        PolicyRefusalGate::OperatorDeclined => format!(
                            "`{action_type}` was refused by the runtime's policy, the user \
                             was asked whether to allow it for this session, and the user \
                             answered no: {reason}"
                        ),
                    },
                },
            }),
            DispatchOutcome::HookDenied {
                action_type,
                reason,
                ..
            } => serde_json::json!({
                "success": false,
                "error": {
                    "class": "policy_denied",
                    "message": format!(
                        "`{action_type}` was refused by a hook the user configured, before \
                         it ran: {reason}"
                    ),
                },
            }),
            DispatchOutcome::PlanModeRefused { action_type } => serde_json::json!({
                "success": false,
                "error": {
                    "class": "policy_denied",
                    "message": crate::agent::plan_mode::refusal(action_type),
                },
            }),
            DispatchOutcome::Cancelled => serde_json::json!({
                "success": false,
                "error": {
                    "class": "cancelled",
                    "message": "The user cancelled this action before it completed.",
                },
            }),
            DispatchOutcome::NoHandler { action_type } => serde_json::json!({
                "success": false,
                "error": {
                    "class": "unsupported",
                    "message": format!(
                        "`{action_type}` is dispatched by the orchestrator, and this \
                         context has no handler for it."
                    ),
                },
            }),
            // The raw transport error is often a bare phrase ("connection
            // reset"). On its own that reads to a model as "try again", which
            // is exactly wrong when the runtime is gone.
            DispatchOutcome::BridgeError { message } => serde_json::json!({
                "success": false,
                "error": {
                    "class": "transport",
                    "message": format!(
                        "The x0ne-runtime bridge failed, so this action did not run: \
                         {message}. Nothing was applied. Do not retry blindly — report \
                         this to the user."
                    ),
                },
            }),
        }
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, DispatchOutcome::Executed { result, .. }
            if result.get("success").and_then(|v| v.as_bool()) != Some(false))
    }
}

/// Add a "did you mean" to the runtime's unknown-variant error.
///
/// The parser cannot adjudicate action identity: it deserialises into `Value`
/// so that a user-defined composite primitive — a name no static table can
/// know — still dispatches. Identity is therefore settled by the runtime,
/// which has both the vocabulary and the composite store. Its error is
/// correct but bare, so the suggestion is added here, drawn from the generated
/// catalog rather than from a list maintained by hand.
///
/// The orchestrator's own parser carries a suggestion path for this, but it
/// tests for "unknown variant" on an error produced by `from_str::<Value>`,
/// which only ever reports JSON syntax problems. That branch cannot fire.
fn enrich_unknown_action(result: &Value) -> Value {
    let Some(message) = result
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    else {
        return result.clone();
    };
    if !message.contains("unknown variant") {
        return result.clone();
    }
    let bad = message.split('`').nth(1).unwrap_or_default();
    let Some(near) = crate::parser::suggest_action(bad) else {
        return result.clone();
    };
    let mut enriched = result.clone();
    enriched["error"]["message"] = Value::String(format!(
        "{message} — did you mean \"{near}\"? Use `ListActions` to see the \
         vocabulary this runtime accepts."
    ));
    enriched
}

/// Executes the actions the orchestrator owns.
///
/// Kept behind a trait so this module never has to know what `SpawnWorld` or
/// `RunIncus` actually do — it only knows that they must not reach the runtime
/// and must meet the same gates. The existing implementations plug in here
/// unchanged.
#[async_trait::async_trait]
pub trait SysaicallHandler: Send {
    /// Execute `action`. `None` means this handler does not own it, which the
    /// dispatcher reports as [`DispatchOutcome::NoHandler`] rather than
    /// falling through to the runtime.
    ///
    /// `ctx` lends the capabilities this *call* needs and takes them back when
    /// it returns. A handler that stored them instead would hold borrows for
    /// the whole turn, and they derive from the same `Bridge` the runtime
    /// route needs — the compiler says so plainly:
    ///
    /// ```text
    /// error[E0499]: cannot borrow `self.bridge` as mutable more than once
    /// ```
    async fn handle(&mut self, action: &Value, ctx: &mut HandlerCallCtx<'_>) -> Option<Value>;
}

/// Builds a child-agent spawner around a borrowed runtime.
///
/// Dispatch holds the bridge; the session holds the model, the config and the
/// child context. Neither alone can build a spawner, so the session supplies
/// this and dispatch lends it the transport for the duration of the call. The
/// bridge never enters `HandlerCallCtx` and never reaches the handler.
pub trait ChildSpawnFactory: Send {
    /// `budget` is the parent's live budget. A child spends what the parent
    /// has left of the session cap and is charged to it when it returns
    /// (H2-1, audit of 2026-10-30); `None` refuses the spawn rather than
    /// handing the child a budget of its own.
    fn spawner<'a>(
        &'a mut self,
        bridge: Option<&'a mut dyn Bridge>,
        budget: Option<&'a mut dyn crate::agent::budget::BudgetControl>,
    ) -> Box<dyn crate::agent::spawner::SubAgentSpawner + 'a>;

    /// A world spawner, when this session can start worlds.
    ///
    /// Takes no bridge, and that is the point rather than an omission: a world
    /// opens its **own** runtime inside its own task, so the parent's
    /// transport must not be reachable from here. `SpawnAgent` shares the
    /// parent runtime and `SpawnWorld` does not; the two signatures are how
    /// that distinction is enforced instead of remembered.
    fn worlds<'a>(&'a mut self) -> Option<Box<dyn crate::agent::world::WorldSpawner + 'a>> {
        None
    }
}

/// Capabilities lent to one handler invocation.
///
/// A capability bundle, not a session object and not a resource bundle. It
/// carries no `Bridge`, no `ApprovalAuthority`, no `PolicyEngine` and no
/// executor: the handler is called *after* authority has spoken and must not
/// be able to revisit that.
///
/// The variants are exclusive because the capabilities are. `BridgeCatalog`
/// and the nested executor both derive from the dispatcher's single `&mut
/// Bridge`, so at most one can exist at a time. Since no action needs both —
/// discovery reads a catalog, `Plan` runs a transaction — an action-specific
/// context costs nothing and makes the exclusivity a type rather than a rule.
pub enum HandlerCallCtx<'a> {
    /// Nothing lent. Most sysaicalls need only their own session state.
    None,
    /// Capability discovery, for the two Meta interceptions.
    Catalog(&'a mut dyn crate::agent::catalog::CatalogQuery),
    /// Nested typed-Action execution, for `Plan`'s transaction.
    Nested(&'a mut dyn crate::agent::nested::NestedActionExecutor),
    /// Child-agent lifecycle, for `SpawnAgent`.
    Spawn(&'a mut dyn crate::agent::spawner::SubAgentSpawner),
    /// World lifecycle, for `SpawnWorld`.
    ///
    /// A separate variant from [`HandlerCallCtx::Spawn`] because the two
    /// lifecycles are separate products: a sub-agent blocks on the parent's
    /// runtime, a world detaches onto its own. Collapsing them would force one
    /// of the two to be wrong, and the type is where that stays visible.
    World(&'a mut dyn crate::agent::world::WorldSpawner),
    /// The live session budget, for `Budget`.
    Budget(&'a mut dyn crate::agent::budget::BudgetControl),
    /// The canonical transcript as it stands *right now*, for `Reflect`.
    ///
    /// Shared, not mutable: `Reflect` reads what has happened and returns
    /// prose about it. A `&mut` here would let an introspection sysaicall
    /// rewrite the record it was asked to describe.
    ///
    /// A per-call capability rather than a field on the handler because the
    /// driver owns `AgentState` and mutates the history between actions. A
    /// handler holding a borrow across the turn would either freeze that or
    /// need a lock; lent for the one call that reads it, neither is true and
    /// the view cannot be stale — it is taken at the instant `Reflect` runs.
    History(&'a crate::conversation::Conversation),
}

impl<'a> HandlerCallCtx<'a> {
    /// The catalog, when this call was given one.
    pub fn catalog(&mut self) -> Option<&mut dyn crate::agent::catalog::CatalogQuery> {
        match self {
            HandlerCallCtx::Catalog(c) => Some(&mut **c),
            _ => None,
        }
    }

    /// The nested executor, when this call was given one.
    pub fn nested(&mut self) -> Option<&mut dyn crate::agent::nested::NestedActionExecutor> {
        match self {
            HandlerCallCtx::Nested(n) => Some(&mut **n),
            _ => None,
        }
    }

    /// The live budget, when this call was given one.
    pub fn budget(&mut self) -> Option<&mut dyn crate::agent::budget::BudgetControl> {
        match self {
            HandlerCallCtx::Budget(b) => Some(&mut **b),
            _ => None,
        }
    }

    /// The child spawner, when this call was given one.
    pub fn spawner(&mut self) -> Option<&mut dyn crate::agent::spawner::SubAgentSpawner> {
        match self {
            HandlerCallCtx::Spawn(s) => Some(&mut **s),
            _ => None,
        }
    }

    /// The world spawner, when this call was given one.
    pub fn worlds(&mut self) -> Option<&mut dyn crate::agent::world::WorldSpawner> {
        match self {
            HandlerCallCtx::World(w) => Some(&mut **w),
            _ => None,
        }
    }

    /// The canonical transcript, when this call was given it. `&self`, so a
    /// holder of this context cannot reach a mutable transcript through it.
    pub fn history(&self) -> Option<&crate::conversation::Conversation> {
        match self {
            HandlerCallCtx::History(h) => Some(h),
            _ => None,
        }
    }
}

/// Actions a frontend executes itself — badge painting, a modal question, the
/// task panel, cron-light. They are presentation-adjacent primitives with no
/// runtime meaning, so the runtime's vocabulary does not grow to describe
/// them; the frontend claims them here instead.
///
/// The contract is typed, not stringly: `claims` returns the action's
/// [`x0ne_actions::SideEffectClass`] from an exhaustive table (see
/// `tui::intercept`), and the dispatcher applies the canonical human gate to
/// exactly the classes that touch the host. A `Meta`-classed claim is UI
/// state — the same trust level as rendering — and runs ungated; a
/// `ProcessSpawn` claim meets `confirm::decide` like any other host effect.
///
/// A frontend is trusted code: this is a capability seam like
/// [`crate::display_sink::DisplaySink`], not a security boundary against the
/// frontend itself. The boundary it preserves is against the *model*: a name
/// the frontend does not claim follows the ordinary route table, so nothing
/// here widens what a model can reach.
pub trait FrontendActions: Send {
    /// The effect class this frontend claims for `action_type`, or `None`
    /// when this frontend does not execute it.
    fn claims(&self, action_type: &str) -> Option<x0ne_actions::SideEffectClass>;

    /// Execute a claimed action and return its result envelope.
    ///
    /// Only called after `claims` returned `Some` and the dispatcher applied
    /// the gate its class requires. May block on a human (a modal), under the
    /// same threading contract as `DisplaySink::request_confirmation`.
    fn execute(&mut self, action: &Value) -> Value;
}

/// Everything dispatch needs, assembled by the caller.
pub struct DispatchContext<'a> {
    pub config: &'a Config,
    pub bridge: Option<&'a mut dyn Bridge>,
    pub sink: &'a mut dyn DisplaySink,
    pub cancel: &'a CancelToken,
    pub sysaicalls: Option<&'a mut dyn SysaicallHandler>,
    /// Owns session approval state. Not the sink, not the transcript — a
    /// renderer must not decide and a model can write text.
    pub authority: &'a mut crate::agent::confirm::ApprovalAuthority,
    /// Supplied by a session that can run child agents. `None` means
    /// `SpawnAgent` is refused rather than silently unavailable.
    pub spawn_factory: Option<&'a mut dyn ChildSpawnFactory>,
    /// The turn's live budget. Lent by the driver, which owns `AgentState`;
    /// `AgentState`'s fields borrow independently, so this coexists with the
    /// authority borrow rather than competing with it.
    pub budget: Option<&'a mut dyn crate::agent::budget::BudgetControl>,
    /// The canonical transcript, read-only, for the one sysaicall that reads
    /// it. `None` means `Reflect` has nothing to look at and says so.
    pub history: Option<&'a crate::conversation::Conversation>,
    /// Frontend-claimed actions, when the frontend has any. `None` means an
    /// action only a frontend could execute is unsupported here — the
    /// fail-closed default for `-p`, children, worlds and the REPL.
    pub frontend: Option<&'a mut dyn FrontendActions>,
    /// The audit trail, for the one thing dispatch does that the driver cannot
    /// see afterwards: a policy grant.
    ///
    /// SECURITY: a grant widens the policy in the middle of a session and
    /// leaves no other trace — the runtime's copy is in memory, and a session
    /// grant is gone at exit. Without this line an auditor reading the session
    /// log sees an action that was refused and then succeeded, with nothing
    /// between the two. `None` (every test that asserts on outcomes rather
    /// than on the log) means the grant still happens; it is the record that
    /// is missing, which is why the wiring is asserted by
    /// `every_grant_is_journaled` rather than left to the type.
    pub journal: Option<std::sync::Arc<dyn crate::agent::journal::ActionJournal>>,
    /// The session's `/undo` points. `Some` for a top-level action of a
    /// session: each file-writing action is snapshotted first.
    pub undo: Option<&'a mut crate::agent::undo::UndoJournal>,
}

/// What the runtime says would lift this refusal, if anything.
///
/// SECURITY: reads the STRUCTURED field the runtime attached, never the
/// wording of `error.message`. The message is presentation and has been
/// rewritten twice this month; the decision about what may be waived is the
/// runtime's exhaustive match on `DenyReason`, and this side of the process
/// boundary only carries it. A refusal without the field is a wall — which is
/// also what an older runtime produces, so the fail-closed direction is the
/// default.
fn liftable_of(result: &Value) -> Option<x0ne_actions::policy_grant::LiftableDenial> {
    if result.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let error = result.get("error")?;
    if error.get("class").and_then(Value::as_str) != Some("policy_denied") {
        return None;
    }
    serde_json::from_value(error.get("liftable")?.clone()).ok()
}

/// What the session gate did with a liftable refusal.
///
/// SECURITY: only [`Self::Granted`] lets the action proceed, and every other
/// value is a refusal — the caller must treat `Declined` and `NotLifted`
/// identically as far as permission goes. They differ in one thing only: what
/// the model is told about whether a person answered. See
/// [`PolicyRefusalGate`].
enum LiftResult {
    /// The operator said yes AND the runtime accepted the grant. Retry.
    Granted,
    /// The operator was asked and answered no.
    Declined,
    /// No answer from a person, or the grant could not be delivered.
    NotLifted,
}

impl LiftResult {
    fn gate(&self) -> PolicyRefusalGate {
        match self {
            // Never read on this path (the caller falls through instead), but
            // an honest mapping beats an `unreachable!` that a refactor turns
            // into a panic on a security path.
            Self::Granted | Self::NotLifted => PolicyRefusalGate::NotAsked,
            Self::Declined => PolicyRefusalGate::OperatorDeclined,
        }
    }
}

/// Drop the undo point 1b took for an action that will not run.
async fn discard_host_point(ctx: &mut DispatchContext<'_>) {
    if let (Some(undo), Some(bridge)) = (ctx.undo.as_mut(), ctx.bridge.as_mut()) {
        undo.discard_last(&mut **bridge).await;
    }
}

/// Put a liftable refusal to the operator and, if he says yes, widen the
/// running runtime's policy.
///
/// Every failure returns a non-`Granted` value, and every non-`Granted` value
/// means the caller reports the refusal exactly as it did before this feature
/// existed. A grant that the runtime rejects, a bridge with no control channel,
/// a config file that could not be written: none of them may be mistaken for
/// permission.
async fn lift_policy(
    action: &Value,
    lift: &x0ne_actions::policy_grant::LiftableDenial,
    ctx: &mut DispatchContext<'_>,
) -> LiftResult {
    use crate::agent::confirm::{DenialReason, LiftDecision};

    let decision = crate::agent::confirm::decide_policy_lift(action, lift, ctx.authority, ctx.sink);
    let persist = match decision {
        LiftDecision::GrantForSession => false,
        LiftDecision::GrantAndPersist => true,
        // SECURITY: `HumanDeclined` is the ONLY reason that may be reported as
        // a person's answer. `decide_policy_lift` mints it from a validated
        // `No` and nowhere else; NoHumanChannel covers EOF, a closed stdin, a
        // sub-agent, `--serve`, and a rejected or stale response id — none of
        // which is somebody saying no, and all of which keep the older wording.
        LiftDecision::Refused(DenialReason::HumanDeclined) => return LiftResult::Declined,
        LiftDecision::Refused(_) => return LiftResult::NotLifted,
    };

    let grant = lift.clone().into_grant(persist);
    let message = serde_json::json!({
        "control": "PolicyGrant",
        "kind": grant.kind,
        "value": grant.value,
        "persist": grant.persist,
    });

    let Some(bridge) = ctx.bridge.as_mut() else {
        return LiftResult::NotLifted;
    };
    let Ok(envelope) = bridge.send_control(&message).await else {
        // No control channel — an HTTP bridge, a sandbox transport that does
        // not carry one, a dead pipe. The operator said yes to something that
        // cannot happen, and the refusal stands.
        tracing::warn!(
            kind = ?grant.kind,
            value = %grant.value,
            "policy grant could not be delivered; the refusal stands"
        );
        return LiftResult::NotLifted;
    };
    if envelope.get("success").and_then(Value::as_bool) != Some(true) {
        tracing::warn!(
            kind = ?grant.kind,
            value = %grant.value,
            envelope = %envelope,
            "the runtime refused the policy grant"
        );
        return LiftResult::NotLifted;
    }

    // Persistence is a SECOND, independent step, and it comes after: the
    // in-memory grant is what authorizes the action now, the file only decides
    // what the NEXT session starts with. Doing it in this order also means the
    // path written to is the one the runtime reported having loaded, rather
    // than one recomputed here from the same convention — see
    // `crate::policy_file`.
    if persist {
        // "Add to config" is also a standing yes for this action this session.
        if let Some(scope) = crate::agent::approval::ApprovalScope::of(action) {
            ctx.authority.grant(scope);
        }
    }
    let mut persisted: Option<String> = None;
    if persist {
        let config_path = envelope
            .get("output")
            .and_then(|o| o.get("data"))
            .and_then(|d| d.get("config_path"))
            .and_then(Value::as_str);
        match config_path {
            Some(path) => {
                match crate::policy_file::persist_grant(std::path::Path::new(path), &grant) {
                    Ok(what) => persisted = Some(what),
                    Err(e) => {
                        // The session grant already took effect, so the action
                        // will run. Only the permanence failed, and saying so is
                        // the difference between "it worked" and "it worked, and
                        // you will be asked again tomorrow".
                        tracing::warn!(error = %e, path, "could not persist the policy grant");
                    }
                }
            }
            None => {
                tracing::warn!(
                    "the runtime reports no policy file (compiled defaults); the grant holds \
                     for this session only"
                );
            }
        }
    }

    if let Some(journal) = ctx.journal.clone() {
        journal
            .journal_policy_grant(
                action,
                grant.kind.noun(),
                &grant.value,
                persist,
                persisted.as_deref(),
            )
            .await;
    }
    LiftResult::Granted
}

/// Route and execute one action, applying every gate on the way.
///
/// PostToolUse hooks run after an action that ran and succeeded, never after
/// a refusal or a failure. They observe: the outcome is returned as it came,
/// whatever they print or exit with, and a failing one is only logged.
pub async fn dispatch(action: &Value, ctx: &mut DispatchContext<'_>) -> DispatchOutcome {
    let outcome = dispatch_gated(action, ctx).await;
    if ctx.config.hooks.post_tool.is_empty() || !outcome.succeeded() {
        return outcome;
    }
    if let DispatchOutcome::Executed {
        result,
        duration_ms,
        ..
    } = &outcome
    {
        let tool = action.get("type").and_then(Value::as_str).unwrap_or("");
        let failures = crate::hooks::post_tool(
            &ctx.config.hooks,
            &hook_session(ctx),
            tool,
            action,
            result,
            *duration_ms,
        )
        .await;
        for f in failures {
            tracing::warn!(hook = %f.hook, "PostToolUse hook failed: {}", f.error);
        }
    }
    outcome
}

/// `session_id` and `cwd` for a hook payload.
fn hook_session(ctx: &DispatchContext<'_>) -> crate::hooks::HookSession {
    crate::hooks::HookSession::here(ctx.journal.as_ref().and_then(|j| j.session_id()))
}

async fn dispatch_gated(action: &Value, ctx: &mut DispatchContext<'_>) -> DispatchOutcome {
    let action_type = action
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Hooks first: the only step every route crosses, connectors included.
    if let Some(d) =
        crate::hooks::evaluate(&ctx.config.hooks, &hook_session(ctx), &action_type, action).await
    {
        return DispatchOutcome::HookDenied {
            action_type,
            hook: d.hook,
            reason: d.reason,
        };
    }

    // Plan mode, before every gate: an action that cannot run is not put to
    // anyone. The runtime refuses the same actions on its own (`PlanMode`).
    if ctx.authority.plan_mode() {
        let claimed = ctx.frontend.as_ref().and_then(|f| f.claims(&action_type));
        if !crate::agent::plan_mode::permits(action, route_of(&action_type), claimed) {
            return DispatchOutcome::PlanModeRefused { action_type };
        }
    }

    // 0. Frontend-claimed actions. Checked before the universal gate because
    //    the gate cannot classify them — an unknown name is assumed
    //    destructive, which would put a human prompt in front of every badge
    //    repaint. The claim carries its own typed class instead, and the gate
    //    is applied to exactly the classes with host effects — the same rule
    //    the runtime policy engine applies to its own vocabulary.
    if let Some(class) = ctx.frontend.as_ref().and_then(|f| f.claims(&action_type)) {
        // SECURITY: `SideEffectClass::touches_host` is the single definition,
        // shared with `terminal::action_requires_confirmation` and
        // `tui::intercept`. The list used to be recopied here; the three
        // copies disagreed about `Network` (P8b, 2026-08-16).
        if class.touches_host() {
            match confirm::decide(action, ctx.config, ctx.authority, ctx.sink) {
                ConfirmationDecision::Denied(reason) => return DispatchOutcome::Denied { reason },
                ConfirmationDecision::NotRequired | ConfirmationDecision::Approved { .. } => {}
            }
        }
        if ctx.cancel.is_cancelled() {
            return DispatchOutcome::Cancelled;
        }
        let started = std::time::Instant::now();
        let result = ctx
            .frontend
            .as_mut()
            .expect("checked above")
            .execute(action);
        return DispatchOutcome::Executed {
            route: Route::Orchestrator,
            result,
            duration_ms: started.elapsed().as_millis() as u64,
        };
    }

    // 0b. Connector tools carry their own gate, which yolo does not waive
    //     (publishing to a third party leaves the sandbox), so they skip the
    //     generic one rather than being asked twice.
    if route_of(&action_type) == Route::Connector {
        return crate::connectors::dispatch(action, ctx.authority, ctx.sink, ctx.cancel).await;
    }

    // 1. Runtime-policy preflight, BEFORE the human gate.
    //
    //    The two authorities were consulted in the wrong order: the gate
    //    asked, the operator said yes, and the runtime then refused for a
    //    reason it had known all along — an action switched off under
    //    `[actions]`, a command absent from `allowed_commands`. Measured on
    //    the owner's machine 2026-08-15; his conclusion was that the gate does
    //    not work. See `agent::preflight` for the full account.
    //
    //    Scope, and it is deliberately narrow: only actions bound for the
    //    runtime, and only those the gate is about to ask about. An action the
    //    gate waives — a read, a read-only command — is not preflighted,
    //    because there is no prompt for a later refusal to contradict, and
    //    preflighting it would cost a round trip on the commonest path.
    //
    //    SECURITY: `Unknown` — a dead bridge, an unparseable answer, a runtime
    //    that predates the tri-state — falls through to the gate. Preflight
    //    may withhold a question; it must never grant one.
    let mut lifted_by_operator = false;
    if route_of(&action_type) == Route::Runtime && confirm::requires_approval(action, ctx.config) {
        let refusal = if let Some(bridge) = ctx.bridge.as_mut() {
            let verdict = crate::agent::preflight::check(action, &mut **bridge).await;
            verdict
                .refusal()
                .map(|r| (r.to_string(), verdict.liftable().cloned()))
        } else {
            None
        };
        if let Some((reason, liftable)) = refusal {
            // A refusal the operator can lift is a decision, not a wall
            // (field report 2026-08-16). Everything else keeps the behaviour
            // this branch has had since P8a: refuse before prompting.
            let gate = match liftable {
                Some(lift) => match lift_policy(action, &lift, ctx).await {
                    LiftResult::Granted => {
                        // The operator's yes named this action ("allow
                        // … and run it"); asking again was two prompts per
                        // command (2026-10-06).
                        lifted_by_operator = true;
                        None
                    }
                    // Refused either way. The value only decides the WORDING
                    // the model gets: an operator who answered no must not be
                    // reported to the model as never having been asked (G1,
                    // 2026-08-16).
                    other => Some(other.gate()),
                },
                // Not liftable at all: no question existed to be answered.
                None => Some(PolicyRefusalGate::NotAsked),
            };
            if let Some(gate) = gate {
                return DispatchOutcome::RuntimePolicyRefused {
                    action_type,
                    reason,
                    gate,
                };
            }
        }
    }

    // 1b. On the host, a destructive command gets its undo point BEFORE
    //     the gate, so the gate can say when there is none. Measured 2026-10-16:
    //     an approved `rm -rf` ran with no snapshot and `/undo` had nothing.
    let in_sandbox = crate::sandbox_mode::current().in_sandbox();
    let host_undo = if !in_sandbox && crate::agent::destructive::classify(action).is_some() {
        Some(match (ctx.undo.as_mut(), ctx.bridge.as_mut()) {
            (Some(undo), Some(bridge)) => {
                let ws = crate::agent::readonly_cmd::workspace_root();
                crate::agent::destructive::guard(action, false, &ws, undo, &mut **bridge).await
            }
            _ => Err("this session keeps no undo points".to_string()),
        })
    } else {
        None
    };
    let host_point = matches!(host_undo, Some(Ok(Some(_))));

    // 2. Human approval, before routing, so orchestrator-dispatched actions
    //    are gated too — the runtime will never see those.
    // A yolo lift asked nobody; after an injection it must not stand in for the ask.
    let tainted = ctx.authority.injection().is_some()
        && crate::injection_guard::tainted_gate(action).is_some();
    if let Some(Err(why)) = &host_undo {
        // No undo point: asked every time, whatever was granted before.
        if let ConfirmationDecision::Denied(reason) =
            confirm::decide_without_undo(action, why, ctx.authority, ctx.sink)
        {
            return DispatchOutcome::Denied { reason };
        }
    } else if !lifted_by_operator || tainted {
        match confirm::decide(action, ctx.config, ctx.authority, ctx.sink) {
            ConfirmationDecision::Denied(reason) => {
                if host_point {
                    discard_host_point(ctx).await;
                }
                return DispatchOutcome::Denied { reason };
            }
            ConfirmationDecision::NotRequired | ConfirmationDecision::Approved { .. } => {}
        }
    }

    // 3. A cancellation that arrived while the user was being asked must be
    //    honoured before anything runs.
    if ctx.cancel.is_cancelled() {
        if host_point {
            discard_host_point(ctx).await;
        }
        return DispatchOutcome::Cancelled;
    }

    // 4. The `/undo` point: the files this action is about to write, snapshotted
    //    by the runtime. After the gate (a refused action writes nothing).
    if let (Some(undo), Some(bridge)) = (ctx.undo.as_mut(), ctx.bridge.as_mut()) {
        undo.checkpoint(action, &mut **bridge).await;
    }

    // 4b. In a sandbox /work is the host's real directory. Any
    //     command that is not a known read snapshots it first; a destructive
    //     one does not run without the snapshot. On the host the
    //     destructive case was settled in 1b.
    match host_undo {
        Some(Ok(Some(line))) => ctx.sink.emit(&crate::tui::wire::AgentEvent::Info(line)),
        Some(Ok(None)) => {}
        Some(Err(why)) => {
            let label =
                crate::agent::destructive::label_of(action).unwrap_or_else(|| action_type.clone());
            if let Some(undo) = ctx.undo.as_mut() {
                undo.note_unprotected(&label);
            }
            ctx.sink.emit(&crate::tui::wire::AgentEvent::Info(format!(
                "`{label}` runs with no undo point: {why}"
            )));
        }
        None => {
            if let (Some(undo), Some(bridge)) = (ctx.undo.as_mut(), ctx.bridge.as_mut()) {
                let ws = crate::agent::readonly_cmd::workspace_root();
                match crate::agent::destructive::guard(action, in_sandbox, &ws, undo, &mut **bridge)
                    .await
                {
                    Ok(None) => {}
                    Ok(Some(line)) => ctx.sink.emit(&crate::tui::wire::AgentEvent::Info(line)),
                    Err(reason) => {
                        return DispatchOutcome::RuntimePolicyRefused {
                            action_type,
                            reason,
                            gate: PolicyRefusalGate::NotAsked,
                        }
                    }
                }
            }
        }
    }

    let started = std::time::Instant::now();
    let route = route_of(&action_type);
    match route {
        // A question for a person and a privileged sysaicall are executed by
        // the same frontend-supplied handler — the frontend is what has both a
        // human and the host — but they are separate categories on the way in,
        // so "never the runtime" is a property of the routing table rather
        // than of one branch happening to fall the right way.
        //
        // With no handler, both yield `NoHandler`. For a question that is the
        // fail-closed answer: nobody was reachable, and the model is told so.
        // It is emphatically not the runtime's stdin.
        Route::Orchestrator | Route::Human => {
            // Derive only what this action needs, from the borrow this arm is
            // not otherwise using: the runtime route is the other branch, so
            // `ctx.bridge` is free here and the capability dies when the call
            // returns.
            let mut catalog;
            let mut nested;
            let mut spawner;
            let mut worlds;
            let mut call_ctx = if META_ACTION_TYPES.contains(&action_type.as_str()) {
                match ctx.bridge.as_mut() {
                    Some(b) => {
                        catalog = crate::agent::catalog::BridgeCatalog::new(&mut **b);
                        HandlerCallCtx::Catalog(&mut catalog)
                    }
                    // No runtime: the handler still answers with the
                    // orchestrator half, which is better than nothing.
                    None => HandlerCallCtx::None,
                }
            } else if action_type == "Reflect" {
                // Live by construction. The driver records each action's
                // result into the canonical history before the next action is
                // dispatched, so the borrow taken here already contains
                // everything this turn has done.
                match ctx.history {
                    Some(h) => HandlerCallCtx::History(h),
                    None => HandlerCallCtx::None,
                }
            } else if action_type == "Budget" {
                match ctx.budget.take() {
                    Some(b) => HandlerCallCtx::Budget(b),
                    None => HandlerCallCtx::None,
                }
            } else if action_type == "SpawnAgent" {
                // `take` rather than `as_mut`: the factory and the bridge must
                // share one lifetime, and a reborrow of each gives two nested
                // ones the trait cannot unify.
                match ctx.spawn_factory.take() {
                    Some(f) => {
                        let bridge = ctx.bridge.take();
                        // The parent's budget goes with its runtime: the
                        // child spends what the parent has left, and the
                        // parent is billed for it (H2-1).
                        let budget = ctx.budget.take();
                        spawner = f.spawner(bridge, budget);
                        HandlerCallCtx::Spawn(&mut *spawner)
                    }
                    None => HandlerCallCtx::None,
                }
            } else if action_type == "SpawnWorld" {
                // No `ctx.bridge.take()` here, deliberately. A world opens its
                // own runtime; handing it the parent's would collapse it into
                // a detached `SpawnAgent` and delete the isolation that is the
                // whole product.
                match ctx.spawn_factory.take().and_then(|f| f.worlds()) {
                    Some(w) => {
                        worlds = w;
                        HandlerCallCtx::World(&mut *worlds)
                    }
                    None => HandlerCallCtx::None,
                }
            } else if action_type == "Plan" {
                // `Plan` runs its steps as a transaction, so it is lent nested
                // execution built from the borrows this arm is not otherwise
                // using. `DispatchNested` passes `sysaicalls: None`, which is
                // sound here and only here: `Plan` is statically runtime-only
                // — `plan_transaction_blocker` refuses a plan naming an
                // orchestrator sysaicall before anything runs — so there is no
                // legitimate orchestrator child to recurse into.
                nested = crate::agent::nested::DispatchNested {
                    config: ctx.config,
                    bridge: match ctx.bridge.as_mut() {
                        Some(b) => Some(&mut **b),
                        None => None,
                    },
                    sink: &mut *ctx.sink,
                    cancel: ctx.cancel,
                    authority: ctx.authority,
                };
                HandlerCallCtx::Nested(&mut nested)
            } else {
                HandlerCallCtx::None
            };
            let Some(handler) = ctx.sysaicalls.as_mut() else {
                return DispatchOutcome::NoHandler { action_type };
            };
            match ctx.cancel.run(handler.handle(action, &mut call_ctx)).await {
                Cancellable::Cancelled => DispatchOutcome::Cancelled,
                Cancellable::Completed(None) => DispatchOutcome::NoHandler { action_type },
                Cancellable::Completed(Some(result)) => DispatchOutcome::Executed {
                    route,
                    result,
                    duration_ms: started.elapsed().as_millis() as u64,
                },
            }
        }
        Route::Unsupported => DispatchOutcome::NoHandler { action_type },
        Route::Connector => {
            crate::connectors::dispatch(action, ctx.authority, ctx.sink, ctx.cancel).await
        }
        Route::Runtime => {
            if ctx.bridge.is_none() {
                return DispatchOutcome::BridgeError {
                    message: "no bridge is connected in this context".into(),
                };
            }
            // Under the cancel token: this is the in-flight cancellation the
            // CLI had at one hand-rolled call site and no other loop had at
            // all. A 30-second command can now be called off from any
            // frontend.
            let first = {
                let bridge = ctx.bridge.as_mut().expect("checked just above");
                ctx.cancel.run(bridge.send_action(action)).await
            };
            match first {
                Cancellable::Cancelled => DispatchOutcome::Cancelled,
                Cancellable::Completed(Ok(result)) => {
                    // THE FIELD-REPORT PATH. `ls` is waived by
                    // `readonly_cmd`, so P8a above never ran for it; the
                    // refusal arrives here, after the action was sent, as a
                    // `policy_denied` envelope. A session gate hung only on
                    // preflight would not have fired for the very case that
                    // was reported (2026-08-16, session `…_01-11_111b`).
                    //
                    // The retry is bounded to ONE attempt by construction:
                    // `lift_policy` reaches a human, and the second envelope
                    // is returned whatever it says. A refusal that survives a
                    // grant — a deny pattern the operator did not see, a
                    // second missing entry — is reported, not re-asked.
                    match liftable_of(&result) {
                        Some(lift)
                            if matches!(
                                lift_policy(action, &lift, ctx).await,
                                LiftResult::Granted
                            ) =>
                        {
                            let retry = {
                                let bridge = ctx.bridge.as_mut().expect("checked just above");
                                ctx.cancel.run(bridge.send_action(action)).await
                            };
                            match retry {
                                Cancellable::Cancelled => DispatchOutcome::Cancelled,
                                Cancellable::Completed(Ok(result)) => DispatchOutcome::Executed {
                                    route: Route::Runtime,
                                    result,
                                    duration_ms: started.elapsed().as_millis() as u64,
                                },
                                Cancellable::Completed(Err(e)) => DispatchOutcome::BridgeError {
                                    message: e.to_string(),
                                },
                            }
                        }
                        _ => DispatchOutcome::Executed {
                            route: Route::Runtime,
                            result,
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    }
                }
                Cancellable::Completed(Err(e)) => DispatchOutcome::BridgeError {
                    message: e.to_string(),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// A refusal took no time, and an execution reports what it measured.
    ///
    /// The driver used to emit a hardcoded `0` here, so every action in every
    /// transcript rendered `(<1ms)` — a twelve-second `cargo test` was
    /// indistinguishable from an `ls`, and the operator had no way to see
    /// which step of a slow turn was the slow one. A failure of this test
    /// means that blindness is back.
    #[test]
    fn only_an_executed_action_reports_a_duration() {
        use super::DispatchOutcome as D;
        assert_eq!(
            D::Executed {
                route: Route::Runtime,
                result: serde_json::json!({"success": true}),
                duration_ms: 12_400,
            }
            .duration_ms(),
            12_400
        );
        for refused in [
            D::Denied {
                reason: crate::agent::confirm::DenialReason::HumanDeclined,
            },
            D::Cancelled,
            D::NoHandler {
                action_type: "Whatever".into(),
            },
            D::BridgeError {
                message: "unreachable".into(),
            },
        ] {
            assert_eq!(
                refused.duration_ms(),
                0,
                "nothing ran, so there is no duration to report"
            );
        }
    }

    use super::*;
    use crate::display_sink::{ConfirmOutcome, NullDisplaySink, RecordingDisplaySink};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct OkBridge {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Bridge for OkBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            // A preflight probe is a question, not work: it is neither
            // counted nor recorded, and it is answered with nothing that
            // parses as a verdict, so the human gate stays in charge exactly
            // as it did before the preflight existed.
            if crate::agent::preflight::is_probe(action)
                || crate::agent::undo::is_bookkeeping(action)
            {
                return Ok(serde_json::json!({"success": true, "output": {}}));
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"success": true, "output": {}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// Records each question's reason and whether a standing grant was
    /// offered, and answers from a script (exhausted: nobody there).
    #[derive(Default)]
    struct ReasonSink {
        answers: std::collections::VecDeque<ConfirmOutcome>,
        reasons: Vec<String>,
        persistent: Vec<bool>,
    }

    impl DisplaySink for ReasonSink {
        fn emit(&mut self, _e: &crate::tui::wire::AgentEvent) {}
        fn request_confirmation(
            &mut self,
            request: &crate::display_sink::ConfirmationRequest<'_>,
        ) -> Option<crate::agent::approval::ConfirmationResponse> {
            self.reasons.push(request.reason.to_string());
            self.persistent.push(request.persistent_grant.is_some());
            self.answers
                .pop_front()
                .map(|outcome| crate::agent::approval::ConfirmationResponse {
                    id: request.id,
                    outcome,
                })
        }
    }

    /// Measured 2026-10-16: on the host an approved `rm -rf` ran with
    /// no undo point and the prompt did not say so. Outside the workspace no
    /// snapshot can hold it: the gate says so, offers no standing grant, and
    /// with nobody to answer the command does not run.
    #[tokio::test]
    async fn on_the_host_rm_rf_outside_the_workspace_is_asked_as_having_no_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim").display().to_string();
        let rm = json!({"type": "RunCommand", "command": "rm", "args": ["-rf", victim]});
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut undo = crate::agent::undo::UndoJournal::default();

        for (answer, runs) in [(None, false), (Some(ConfirmOutcome::Yes), true)] {
            let mut sink = ReasonSink {
                answers: answer.into_iter().collect(),
                ..Default::default()
            };
            let mut authority = crate::agent::confirm::ApprovalAuthority::default();
            let mut ctx = DispatchContext {
                undo: Some(&mut undo),
                journal: None,
                frontend: None,
                history: None,
                config: &config,
                bridge: Some(&mut bridge),
                sink: &mut sink,
                cancel: &cancel,
                sysaicalls: None,
                authority: &mut authority,
                spawn_factory: None,
                budget: None,
            };
            let out = dispatch(&rm, &mut ctx).await;
            assert_eq!(out.succeeded(), runs, "{out:?}");
            assert_eq!(sink.reasons.len(), 1, "{:?}", sink.reasons);
            assert!(
                sink.reasons[0].starts_with("NO UNDO POSSIBLE")
                    && sink.reasons[0].contains("outside"),
                "{}",
                sink.reasons[0]
            );
            assert_eq!(sink.persistent, vec![false], "no standing grant offered");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only the approved one ran");
        let line = crate::agent::undo::describe(&undo.undo_last(&mut bridge).await);
        assert!(
            line.contains("no undo point") && line.contains("rm -rf"),
            "{line}"
        );
    }

    struct DeadBridge;

    #[async_trait::async_trait]
    impl Bridge for DeadBridge {
        async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
            Err(anyhow::anyhow!("runtime went away"))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    struct SlowBridge;

    #[async_trait::async_trait]
    impl Bridge for SlowBridge {
        async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(json!({"success": true}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct RecordingSysaicalls {
        seen: Vec<String>,
    }

    #[async_trait::async_trait]
    impl SysaicallHandler for RecordingSysaicalls {
        async fn handle(
            &mut self,
            action: &Value,
            _ctx: &mut crate::agent::dispatch::HandlerCallCtx<'_>,
        ) -> Option<Value> {
            let t = action["type"].as_str().unwrap_or("").to_string();
            self.seen.push(t);
            Some(json!({"success": true, "output": {"local": true}}))
        }
    }

    fn read() -> Value {
        json!({"type": "ReadFile", "path": "/etc/hostname"})
    }

    fn destructive() -> Value {
        json!({"type": "MoveToTrash", "path": "/tmp/x"})
    }

    #[test]
    fn routing_sends_runtime_actions_to_the_runtime() {
        assert_eq!(route_of("ReadFile"), Route::Runtime);
        assert_eq!(route_of("WriteFile"), Route::Runtime);
    }

    /// The privilege boundary serve's hand-rolled table lost: the runtime may
    /// be running inside the container these manage, so they must stay
    /// orchestrator-side.
    #[test]
    fn routing_keeps_orchestrator_sysaicalls_local() {
        for name in [
            "SpawnWorld",
            "Plan",
            "RunIncus",
            "IncusRestoreSnapshot",
            "ProjectNote",
        ] {
            assert_eq!(
                route_of(name),
                Route::Orchestrator,
                "{name} must stay local"
            );
        }
    }

    /// An unrecognised name resolves to `Unsupported` and goes nowhere.
    ///
    /// This replaces `an_unknown_name_routes_to_the_runtime`. The old default
    /// was defensible — the runtime rejects what it does not know — but it
    /// made routing a two-way split with a catch-all, so the completeness test
    /// was nearly tautological: everything "had a route" because everything
    /// fell into the same bucket. Membership is now positive on both sides,
    /// and a name that belongs to neither vocabulary is named as such.
    #[test]
    fn an_unknown_name_is_unsupported_and_goes_nowhere() {
        assert_eq!(route_of("NotAnAction"), Route::Unsupported);
        assert_eq!(route_of(""), Route::Unsupported);
    }

    /// Every member of the runtime vocabulary resolves to `Runtime`, except
    /// the three the orchestrator intercepts for plumbing reasons.
    #[test]
    fn every_runtime_action_resolves_to_runtime_or_its_human_interception() {
        for name in x0ne_actions::all_type_names() {
            let expected = if HUMAN_ACTION_TYPES.contains(name) {
                Route::Human
            } else if META_ACTION_TYPES.contains(name) {
                Route::Orchestrator
            } else {
                Route::Runtime
            };
            assert_eq!(route_of(name), expected, "for runtime action {name}");
        }
    }

    /// Every member of the orchestrator registry resolves to `Orchestrator`.
    /// The registry is the authority, not the hand-written list beside it.
    #[test]
    fn every_orchestrator_sysaicall_resolves_to_orchestrator() {
        for meta in crate::prompt::orchestrator_action_metas() {
            let name = meta["type"].as_str().expect("registry entry has a type");
            assert_eq!(
                route_of(name),
                Route::Orchestrator,
                "for orchestrator sysaicall {name}"
            );
        }
    }

    /// Routing asks the registry; the list in `agent_loop` still feeds
    /// `action_source` (per-source `ActionStats`). A sysaicall added to one and
    /// not the other would be counted on the wrong side.
    #[test]
    fn the_registry_and_the_historical_list_agree() {
        let registry: std::collections::BTreeSet<&str> = crate::prompt::orchestrator_action_metas()
            .iter()
            .map(|m| m["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .iter()
            .map(|s| Box::leak(s.clone().into_boxed_str()) as &str)
            .collect();
        let historical: std::collections::BTreeSet<&str> =
            crate::agent_loop::ORCHESTRATOR_ACTION_TYPES
                .iter()
                .copied()
                .collect();
        assert_eq!(
            registry, historical,
            "the orchestrator registry and ORCHESTRATOR_ACTION_TYPES have drifted"
        );
    }

    // ── Interception reason vs execution destination ─────────────────────

    /// Capability federation, not privilege. These reach `Orchestrator`
    /// because only the orchestrator can see both catalogs, and a caller
    /// asking what the system can do deserves one answer rather than half.
    #[test]
    fn meta_discovery_is_intercepted_and_runs_orchestrator_side() {
        for name in META_ACTION_TYPES {
            assert_eq!(route_of(name), Route::Orchestrator, "for {name}");
        }
    }

    /// The interception is deliberate, not a gap in the runtime vocabulary.
    /// These *are* runtime actions; the orchestrator takes them anyway, and
    /// that is why membership order matters in `route_of`.
    #[test]
    fn the_intercepted_meta_actions_are_real_runtime_actions() {
        let runtime = x0ne_actions::all_type_names();
        for name in META_ACTION_TYPES {
            assert!(
                runtime.contains(name),
                "`{name}` is intercepted as Meta but the runtime does not define \
                 it, so there is nothing to federate with"
            );
        }
    }

    /// Nothing else got swept up. Every other runtime action still goes to the
    /// runtime, so interception stayed a list of two rather than a habit.
    #[test]
    fn no_other_runtime_action_is_intercepted_as_meta() {
        for name in x0ne_actions::all_type_names() {
            if META_ACTION_TYPES.contains(name) || HUMAN_ACTION_TYPES.contains(name) {
                continue;
            }
            assert_eq!(
                route_of(name),
                Route::Runtime,
                "{name} was intercepted without being named in an interception class"
            );
        }
    }

    /// The three interception classes are pairwise disjoint. An action with two
    /// reasons would route by whichever branch ran first, and the branches
    /// disagree about the destination.
    #[test]
    fn the_interception_classes_are_disjoint() {
        for name in META_ACTION_TYPES {
            assert!(
                !HUMAN_ACTION_TYPES.contains(name),
                "`{name}` is both a human question and a capability query"
            );
            assert!(
                !crate::prompt::is_orchestrator_sysaicall(name),
                "`{name}` is both intercepted for federation and a registered \
                 host sysaicall"
            );
        }
    }

    /// The two namespaces do not collide. If one wire name existed on both
    /// sides, the route would depend on which table was consulted first — and
    /// one of those answers crosses a privilege boundary.
    #[test]
    fn the_two_vocabularies_do_not_collide() {
        let runtime = x0ne_actions::all_type_names();
        for meta in crate::prompt::orchestrator_action_metas() {
            let name = meta["type"].as_str().unwrap();
            assert!(
                !runtime.contains(&name),
                "`{name}` is defined in both vocabularies"
            );
        }
    }

    // ── The human boundary ───────────────────────────────────────────────
    //
    // `AskHuman`, `Confirm` and `AskMultipleChoice` ask a person a question.
    // The CLI has always intercepted them inline (`agent_loop.rs:1140,1177,
    // 1212`), but they were never added to the routing table the canonical
    // dispatcher reads — so `route_of` sent them to the runtime.
    //
    // That is not a missing feature, it is a hang. The runtime implements them
    // (`executor/human.rs::ask_human`) by reading stdin, and under the bridge
    // its stdin *is* the protocol pipe (`agent_loop.rs:1136-1139`). The prompt
    // would consume a protocol frame, or wait forever for one.

    /// A question for a person is answered by the frontend that has one.
    #[test]
    fn human_questions_never_reach_the_runtime() {
        for name in ["AskHuman", "Confirm", "AskMultipleChoice"] {
            assert_eq!(
                route_of(name),
                Route::Human,
                "{name} routed to the runtime, whose stdin is the bridge pipe — \
                 this deadlocks rather than asking anyone"
            );
        }
    }

    /// Every name the vocabulary defines has a route. The vocabulary is the
    /// generated catalog, so this test grows by itself: an Action added to
    /// `x0ne-actions` appears here on the next build without anyone
    /// remembering to update a list.
    ///
    /// A new Action lands on `Runtime`, and that default is safe rather than
    /// silent — the runtime's policy engine is fail-closed for an action it
    /// has no configuration for, so an unrouted newcomer is denied there, not
    /// executed here. What the test forbids is a name that reaches *no*
    /// category at all.
    #[test]
    fn every_action_in_the_vocabulary_has_exactly_one_route() {
        for name in x0ne_actions::all_type_names() {
            let route = route_of(name);
            let categories = [
                matches!(route, Route::Human),
                matches!(route, Route::Orchestrator),
                matches!(route, Route::Runtime),
                matches!(route, Route::Unsupported),
            ];
            assert_eq!(
                categories.iter().filter(|c| **c).count(),
                1,
                "{name} does not resolve to exactly one route category"
            );
        }
    }

    /// The two tables draw from different vocabularies, and that is the point.
    ///
    /// `x0ne-actions` is the **runtime's** vocabulary. All 21 orchestrator
    /// sysaicalls are absent from it — they are defined in the orchestrator's
    /// own registry (`prompt::orchestrator_action_metas`, already pinned
    /// there). The human actions are the opposite case: they *are* runtime
    /// actions, defined in `x0ne-actions` and implemented in
    /// `x0ne-runtime/src/executor/human.rs`, which the orchestrator must
    /// nevertheless intercept because the runtime's stdin is the bridge pipe.
    ///
    /// So the invariant is not "both tables are subsets of one vocabulary" —
    /// it is that each names something real, from the right side, and the
    /// interception of the human three is deliberate rather than accidental.
    #[test]
    fn each_route_table_names_actions_from_its_own_vocabulary() {
        let runtime_vocabulary = x0ne_actions::all_type_names();

        for name in HUMAN_ACTION_TYPES {
            assert!(
                runtime_vocabulary.contains(name),
                "HUMAN_ACTION_TYPES names `{name}`, which the runtime vocabulary \
                 does not define — an interception with nothing to intercept"
            );
        }

        for name in crate::agent_loop::ORCHESTRATOR_ACTION_TYPES {
            assert!(
                !runtime_vocabulary.contains(name),
                "`{name}` is both an orchestrator sysaicall and a runtime action. \
                 Two implementations of one wire name is how a privilege boundary \
                 gets crossed by whichever table is consulted first"
            );
        }
    }

    /// The two explicit tables are disjoint: an action cannot be both a
    /// question for a person and a sysaicall the orchestrator runs.
    #[test]
    fn the_explicit_route_tables_do_not_overlap() {
        for name in HUMAN_ACTION_TYPES {
            assert!(
                !crate::agent_loop::ORCHESTRATOR_ACTION_TYPES.contains(name),
                "`{name}` appears in both routing tables"
            );
        }
    }

    #[tokio::test]
    async fn a_harmless_action_reaches_the_bridge() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&read(), &mut ctx).await;
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// THE invariant, now at the dispatch layer: with no human reachable, a
    /// destructive action is refused and never reaches the bridge.
    #[tokio::test]
    async fn a_destructive_action_is_denied_when_nobody_can_approve() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&destructive(), &mut ctx).await;
        assert!(matches!(
            out,
            DispatchOutcome::Denied {
                reason: DenialReason::NoHumanChannel
            }
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a denied action must not reach the runtime"
        );
    }

    // Runs the deny body through `/bin/sh -c` rather than writing an executable
    // and exec'ing it: a freshly written script hit ETXTBSY under the parallel
    // test runner (a sibling fork held its still-open writable fd), which the
    // hook fails closed on, so the reason came back "hook could not start"
    // (measured 2026-10-16). `/bin/sh` is never written, so the race is gone.
    fn deny_hook(matcher: &str) -> Config {
        let mut config = Config::default();
        config.hooks.pre_tool.push(crate::hooks::PreToolHook {
            matcher: matcher.into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "echo 'hook says no' >&2; exit 2".into()],
            timeout_secs: Some(5),
        });
        config
    }

    /// A read runs with no prompt; a matching hook still stops it at the door.
    #[tokio::test]
    async fn a_hook_denies_before_the_runtime_is_reached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = deny_hook("*");
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&read(), &mut ctx).await;
        assert!(
            matches!(&out, DispatchOutcome::HookDenied { reason, .. } if reason == "hook says no"),
            "{out:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let env = out.to_result_envelope(&read(), None);
        assert_eq!(env["error"]["class"], "policy_denied");
    }

    /// PostToolUse sees an action that ran and succeeded, after it ran, with
    /// the session's id; a deny or an exit 2 from it changes nothing. A failed
    /// action, or one refused before it ran, fires nothing.
    #[tokio::test]
    async fn a_post_tool_hook_observes_and_changes_nothing() {
        struct FailBridge;
        #[async_trait::async_trait]
        impl Bridge for FailBridge {
            async fn send_action(&mut self, _: &Value) -> anyhow::Result<Value> {
                Ok(json!({"success": false, "error": {"class": "io", "message": "nope"}}))
            }
            async fn health_check(&mut self) -> anyhow::Result<bool> {
                Ok(true)
            }
        }
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("post");
        let mut config = Config::default();
        config.hooks.post_tool.push(crate::hooks::PreToolHook {
            matcher: String::new(),
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "cat >> '{0}'; echo >> '{0}'; \
                     echo '{{\"decision\":\"block\",\"hookSpecificOutput\":{{\"permissionDecision\":\"deny\"}}}}'; \
                     echo no >&2; exit 2",
                    log.display()
                ),
            ],
            timeout_secs: Some(5),
        });
        let journal: Arc<dyn crate::agent::journal::ActionJournal> =
            Arc::new(crate::agent::journal::NamedJournal("s-1083".into()));
        async fn run(
            config: &Config,
            journal: &Arc<dyn crate::agent::journal::ActionJournal>,
            bridge: &mut dyn Bridge,
            action: Value,
            plan: bool,
        ) -> DispatchOutcome {
            let mut sink = NullDisplaySink;
            let cancel = CancelToken::new();
            let mut authority = crate::agent::confirm::ApprovalAuthority::default();
            authority.set_plan_mode(plan);
            let mut ctx = DispatchContext {
                undo: None,
                journal: Some(journal.clone()),
                frontend: None,
                history: None,
                config,
                bridge: Some(bridge),
                sink: &mut sink,
                cancel: &cancel,
                sysaicalls: None,
                authority: &mut authority,
                spawn_factory: None,
                budget: None,
            };
            dispatch(&action, &mut ctx).await
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ok = OkBridge {
            calls: calls.clone(),
        };
        let out = run(&config, &journal, &mut ok, read(), false).await;
        assert!(
            matches!(&out, DispatchOutcome::Executed { result, .. }
                if *result == json!({"success": true, "output": {}})),
            "{out:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = run(&config, &journal, &mut FailBridge, read(), false).await;
        let refused = run(&config, &journal, &mut ok, destructive(), true).await;
        assert!(matches!(refused, DispatchOutcome::PlanModeRefused { .. }));

        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        let v = &lines[0];
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["session_id"], "s-1083");
        assert_eq!(v["tool_name"], "ReadFile");
        assert_eq!(v["tool_input"], read());
        assert_eq!(v["tool_response"], json!({"success": true, "output": {}}));
    }

    /// A frontend that counts what it executes.
    struct CountingFrontend {
        ran: Vec<String>,
    }

    impl FrontendActions for CountingFrontend {
        fn claims(&self, action_type: &str) -> Option<x0ne_actions::SideEffectClass> {
            match action_type {
                "PaintBadge" => Some(x0ne_actions::SideEffectClass::ReadOnly),
                "BadgeWatch" => Some(x0ne_actions::SideEffectClass::ProcessSpawn),
                _ => None,
            }
        }
        fn execute(&mut self, action: &Value) -> Value {
            self.ran
                .push(action["type"].as_str().unwrap_or("").to_string());
            json!({"success": true})
        }
    }

    /// SECURITY: in plan mode, dispatch refuses a write before any gate and
    /// any route, under yolo, against a runtime that would have run it. A
    /// failure means plan mode rests on the runtime alone, and the actions the
    /// orchestrator executes itself are not covered at all.
    #[tokio::test]
    async fn plan_mode_refuses_before_the_gate_and_the_route() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mode = crate::sandbox_mode::SandboxMode::Yolo {
            runtime: "podman".into(),
        };
        let mut authority = crate::agent::confirm::ApprovalAuthority::for_mode(&mode, "plan");
        authority.set_plan_mode(true);
        let mut frontend = CountingFrontend { ran: Vec::new() };
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: Some(&mut frontend),
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let write = json!({"type": "WriteFile", "path": "/tmp/x", "content": "y"});
        let out = dispatch(&write, &mut ctx).await;
        assert!(
            matches!(out, DispatchOutcome::PlanModeRefused { .. }),
            "{out:?}"
        );
        let env = out.to_result_envelope(&write, None);
        assert_eq!(env["error"]["class"], "policy_denied");
        assert!(env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("plan mode"));
        let out = dispatch(&json!({"type": "BadgeWatch", "command": "id"}), &mut ctx).await;
        assert!(
            matches!(out, DispatchOutcome::PlanModeRefused { .. }),
            "{out:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the runtime was reached");

        // Reads still run, and a claim that only reads still executes.
        let out = dispatch(&read(), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::Executed { .. }), "{out:?}");
        let out = dispatch(&json!({"type": "PaintBadge"}), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::Executed { .. }), "{out:?}");
        drop(ctx);
        assert_eq!(frontend.ran, vec!["PaintBadge".to_string()]);
    }

    /// Plan mode suspends standing grants and yolo, and `/build` gives them
    /// back unchanged.
    #[test]
    fn plan_mode_suspends_the_standing_grants() {
        let mode = crate::sandbox_mode::SandboxMode::Yolo {
            runtime: "podman".into(),
        };
        let mut authority = crate::agent::confirm::ApprovalAuthority::for_mode(&mode, "plan");
        let scope = crate::agent::approval::ApprovalScope::of(&json!({
            "type": "RunCommand", "command": "touch", "args": ["x"]
        }));
        if let Some(scope) = scope.as_ref() {
            assert!(authority.covers(scope));
            authority.set_plan_mode(true);
            assert!(!authority.covers(scope));
            authority.set_plan_mode(false);
            assert!(authority.covers(scope));
        } else {
            panic!("precondition: RunCommand has a scope");
        }
    }

    /// A `<connector>.<tool>` name is matched before routing, so a hook covers
    /// connector calls even though they skip every later gate.
    #[tokio::test]
    async fn a_hook_covers_connector_tool_names() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = deny_hook("github.*");
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let action = json!({"type": "github.delete_repo", "arguments": {"repo": "prod"}});
        let out = dispatch(&action, &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::HookDenied { .. }), "{out:?}");
        // A non-matching name still goes its usual way.
        let out = dispatch(&read(), &mut ctx).await;
        assert!(
            !matches!(out, DispatchOutcome::HookDenied { .. }),
            "{out:?}"
        );
    }

    /// And the gate covers the orchestrator route too — the one the runtime
    /// can never speak for.
    #[tokio::test]
    async fn an_orchestrator_sysaicall_is_gated_before_routing() {
        let mut handler = RecordingSysaicalls { seen: Vec::new() };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: None,
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: Some(&mut handler),
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        // RunIncus executes code outside the runtime policy engine, so the
        // confirmation registry gates it.
        let out = dispatch(&json!({"type": "RunIncus"}), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::Denied { .. }), "{out:?}");
        assert!(
            handler.seen.is_empty(),
            "a denied sysaicall must not run locally"
        );
    }

    #[tokio::test]
    async fn an_approved_sysaicall_runs_locally_and_never_touches_the_bridge() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut handler = RecordingSysaicalls { seen: Vec::new() };
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Yes].into_iter().collect(),
            ..Default::default()
        };
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: Some(&mut handler),
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&json!({"type": "RunIncus"}), &mut ctx).await;
        assert!(matches!(
            out,
            DispatchOutcome::Executed {
                route: Route::Orchestrator,
                ..
            }
        ));
        assert_eq!(handler.seen, vec!["RunIncus"]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the privilege boundary means this must not be forwarded"
        );
    }

    /// A frontend that has not wired its handler must not silently forward an
    /// orchestrator action to the runtime — that is how serve defeated the
    /// Incus privilege boundary.
    #[tokio::test]
    async fn a_missing_handler_refuses_rather_than_forwarding() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Yes].into_iter().collect(),
            ..Default::default()
        };
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&json!({"type": "RunIncus"}), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::NoHandler { .. }), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// In-flight cancellation, now available to every frontend rather than at
    /// one hand-rolled CLI call site.
    #[tokio::test]
    async fn an_in_flight_runtime_call_can_be_cancelled() {
        let mut bridge = SlowBridge;
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let signal = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            signal.cancel();
        });
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&read(), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::Cancelled), "{out:?}");
    }

    /// A token cancelled while the human was being asked must stop the action
    /// before it starts.
    #[tokio::test]
    async fn cancellation_during_the_prompt_stops_dispatch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bridge = OkBridge {
            calls: calls.clone(),
        };
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Yes].into_iter().collect(),
            ..Default::default()
        };
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&destructive(), &mut ctx).await;
        assert!(matches!(out, DispatchOutcome::Cancelled), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_dead_bridge_is_reported_not_swallowed() {
        let mut bridge = DeadBridge;
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let out = dispatch(&read(), &mut ctx).await;
        assert!(
            matches!(out, DispatchOutcome::BridgeError { .. }),
            "{out:?}"
        );
        assert_eq!(
            out.to_result_envelope(&json!({"type": "ReadFile"}), None)["error"]["class"],
            "transport"
        );
    }

    /// Every outcome must produce something the model can act on. Matrix's
    /// habit of returning nothing is what let a stuck model keep retrying.
    #[test]
    fn every_outcome_yields_an_envelope_the_model_can_read() {
        for out in [
            DispatchOutcome::Denied {
                reason: DenialReason::NoHumanChannel,
            },
            DispatchOutcome::Cancelled,
            DispatchOutcome::NoHandler {
                action_type: "RunIncus".into(),
            },
            DispatchOutcome::BridgeError {
                message: "gone".into(),
            },
        ] {
            let env = out.to_result_envelope(&json!({"type": "ReadFile"}), None);
            assert_eq!(env["success"], false, "{out:?}");
            let msg = env["error"]["message"].as_str().unwrap_or_default();
            assert!(msg.len() > 20, "unhelpfully terse for {out:?}: {msg}");
        }
    }

    /// The runtime settles identity; the suggestion is added on the way back,
    /// from the generated catalog. The parser's own suggestion path tests for
    /// "unknown variant" on a JSON-syntax error and can never fire.
    #[test]
    fn an_unknown_action_error_gains_a_suggestion() {
        let raw = json!({
            "success": false,
            "error": {"class": "validation", "message": "unknown variant `ReadFiles2`"},
        });
        let out = DispatchOutcome::Executed {
            route: Route::Runtime,
            result: raw,
            duration_ms: 1,
        };
        let msg = out.to_result_envelope(&json!({"type": "ReadFile"}), None)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(msg.contains("did you mean"), "no suggestion added: {msg}");
        assert!(msg.contains("ReadFiles"), "wrong suggestion: {msg}");
    }

    /// A successful result must pass through untouched.
    #[test]
    fn a_normal_result_is_not_rewritten() {
        let raw = json!({"success": true, "output": {"content": "hi"}});
        let out = DispatchOutcome::Executed {
            route: Route::Runtime,
            result: raw.clone(),
            duration_ms: 1,
        };
        assert_eq!(
            out.to_result_envelope(&json!({"type": "ReadFile"}), None),
            raw
        );
    }

    // ── Model-facing result normalization (decision B2B) ─────────────────
    //
    // The runtime reports a validation failure as `missing field \`a\``. That
    // is correct and nearly useless to a small model: it names neither the
    // action, nor what was sent, nor what was expected. `agent_loop` has
    // enriched it at the orchestrator boundary since (`:1750`), and the
    // comment there says the classroom-survival rate on Mistral 7B depends on
    // it. That is semantic feedback, not presentation, so it belongs on the
    // canonical path and not in a sink.

    fn validation_failure() -> DispatchOutcome {
        DispatchOutcome::Executed {
            route: Route::Runtime,
            result: json!({
                "success": false,
                "error": { "class": "validation_error", "message": "missing field `path`" },
            }),
            duration_ms: 1,
        }
    }

    /// The enrichment happens where every frontend will inherit it.
    #[test]
    fn a_validation_error_is_enriched_on_the_canonical_path() {
        let action = json!({"type": "ReadFile"});
        let envelope = validation_failure().to_result_envelope(&action, None);
        let error = &envelope["error"];
        assert!(
            error.get("diff").is_some(),
            "no plain-language fix was added: {envelope}"
        );
        assert!(
            error.get("sent").is_some() || error.get("expected").is_some(),
            "the model was not shown what it sent or what was expected: {envelope}"
        );
    }

    /// Identical to what the historical loop produces, so the migrated path
    /// does not quietly change what the model is told.
    #[test]
    fn enrichment_matches_the_historical_loop_exactly() {
        let action = json!({"type": "ReadFile"});
        let mut expected = json!({
            "success": false,
            "error": { "class": "validation_error", "message": "missing field `path`" },
        });
        crate::agent_loop::enrich_validation_error(&action, &mut expected, None);
        assert_eq!(
            validation_failure().to_result_envelope(&action, None),
            expected
        );
    }

    /// A success is still passed through untouched — enrichment is for
    /// failures the model has to recover from, not a general rewrite.
    #[test]
    fn enrichment_leaves_a_non_validation_result_alone() {
        let action = json!({"type": "ReadFile", "path": "/tmp/x"});
        let raw = json!({"success": false, "error": {"class": "io", "message": "nope"}});
        let out = DispatchOutcome::Executed {
            route: Route::Runtime,
            result: raw.clone(),
            duration_ms: 1,
        };
        assert_eq!(out.to_result_envelope(&action, None), raw);
    }

    /// Enrichment is not the sink's business. `DisplaySink` has no method that
    /// could perform it, and the normalization point takes no sink — so a
    /// frontend cannot opt out of the semantics by rendering differently.
    #[test]
    fn the_sink_is_not_involved_in_enrichment() {
        let src = include_str!("dispatch.rs");
        let head = &src[..src.find("#[cfg(test)]").unwrap()];
        let at = head
            .find("pub fn to_result_envelope")
            .expect("the normalization point");
        let end = head[at..]
            .find("\n    }\n")
            .map(|i| at + i)
            .unwrap_or(head.len());
        assert!(
            !head[at..end].contains("sink"),
            "the normalization point consults a sink — model-facing semantics \
             would then depend on which frontend is attached"
        );
    }
}

// ── The runtime-policy preflight (P8a, 2026-08-16) ──────────────────────────
//
// The measured defect, on the owner's machine, 2026-08-15: the gate asked, he
// pressed `1`, and the runtime answered "This action type is disabled by
// configuration". The gate was fine — it was asked a question whose answer
// could not matter. These tests pin the order of the two authorities.
#[cfg(test)]
mod preflight_gate {
    use super::*;
    use crate::agent::cancel::CancelToken;
    use crate::agent::preflight::RuntimeVerdict;
    use crate::config::Config;
    use crate::display_sink::{ConfirmOutcome, NullDisplaySink, RecordingDisplaySink};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A runtime whose policy engine answers `verdict` to every `PolicyCheck`,
    /// and refuses every real action for the same reason — which is exactly
    /// what the measured runtime does (`x0ne-runtime` stdio, 2026-08-16:
    /// `PolicyCheck` → `reason`, then the action → the same `reason`).
    struct PolicyBridge {
        verdict: &'static str,
        reason: &'static str,
        /// Every action type this bridge was handed, `PolicyCheck` included.
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl PolicyBridge {
        fn new(verdict: &'static str, reason: &'static str) -> (Self, Arc<Mutex<Vec<String>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    verdict,
                    reason,
                    seen: seen.clone(),
                },
                seen,
            )
        }
    }

    #[async_trait::async_trait]
    impl Bridge for PolicyBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            let t = action["type"].as_str().unwrap_or("").to_string();
            self.seen.lock().unwrap().push(t.clone());
            if t == "PolicyCheck" {
                return Ok(json!({
                    "success": true,
                    "action_type": "PolicyCheck",
                    "output": {
                        "kind": "policy_check_result",
                        "allowed": self.verdict == "allow",
                        "verdict": self.verdict,
                        "action_type": action["action"]["type"],
                        "reason": self.reason,
                    }
                }));
            }
            if self.verdict == "allow" {
                return Ok(json!({"success": true, "action_type": t, "output": {}}));
            }
            Ok(json!({
                "success": false,
                "action_type": t,
                "error": {"class": "policy_denied", "message": self.reason},
            }))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// Dispatch `action` against a runtime answering `verdict`, with a human
    /// who would say yes to anything. Returns the outcome, the questions the
    /// human was actually asked, and what the bridge saw.
    async fn run(
        action: Value,
        verdict: &'static str,
        reason: &'static str,
    ) -> (DispatchOutcome, Vec<String>, Vec<String>) {
        let (mut bridge, seen) = PolicyBridge::new(verdict, reason);
        let mut sink = RecordingDisplaySink {
            scripted_answers: [
                ConfirmOutcome::Yes,
                ConfirmOutcome::Yes,
                ConfirmOutcome::Yes,
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let outcome = dispatch(&action, &mut ctx).await;
        let asked = sink.confirmations_requested.clone();
        let bridge_saw = seen.lock().unwrap().clone();
        (outcome, asked, bridge_saw)
    }

    fn notification() -> Value {
        json!({"type": "SendNotification", "title": "t", "body": "m"})
    }

    /// SECURITY: the property the whole fix exists for.
    ///
    /// An action the runtime's policy engine will refuse must not be put in
    /// front of a human first. A failure means the operator is once again
    /// approving something that then comes back refused — the pattern he read,
    /// correctly from where he sat, as "the gate does not work" — and every
    /// such prompt trains him to stop reading the ones that matter.
    #[tokio::test]
    async fn an_action_the_runtime_would_deny_is_never_asked_to_the_human() {
        let (outcome, asked, bridge_saw) = run(
            notification(),
            "deny",
            "This action type is disabled by configuration.",
        )
        .await;
        assert!(
            asked.is_empty(),
            "the human was asked about an action the runtime had already refused: {asked:?}"
        );
        assert!(
            matches!(&outcome, DispatchOutcome::RuntimePolicyRefused { action_type, .. }
                if action_type == "SendNotification"),
            "{outcome:?}"
        );
        assert_eq!(
            bridge_saw,
            vec!["PolicyCheck".to_string()],
            "the action itself must never have been sent"
        );
    }

    /// The runtime's own words reach the model, and they name the lever.
    ///
    /// A failure means the model is told "denied" with no way to tell the user
    /// which setting to change — which is how a refusal becomes a retry loop.
    #[tokio::test]
    async fn the_refusal_carries_the_runtimes_reason_to_the_model() {
        let (outcome, _, _) = run(
            json!({"type": "RunCommand", "command": "cargo", "args": ["build"]}),
            "deny",
            "Command 'cargo' is not in the allowed_commands list. Add `\"cargo\"` to \
             [policy] allowed_commands in x0ne-runtime.toml, or run `x0ne --setup`.",
        )
        .await;
        let envelope = outcome.to_result_envelope(&json!({}), None);
        let msg = envelope["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(envelope["error"]["class"], "policy_denied");
        assert!(msg.contains("allowed_commands"), "{msg}");
        assert!(msg.contains("x0ne --setup"), "{msg}");
        assert!(
            msg.contains("the user was not asked"),
            "the model must be told nobody was interrupted: {msg}"
        );
        assert!(!outcome.succeeded());
        assert_eq!(outcome.duration_ms(), 0, "nothing ran");
    }

    /// SECURITY: a runtime that demands a confirmation it cannot obtain is a
    /// runtime that will refuse, so it too is refused before the prompt.
    ///
    /// `policy.mode = "confirm"` yields `RequireConfirmation`, and every
    /// execution entry point in the runtime turns that into a `PolicyDenied`
    /// error — measured on this binary 2026-08-16: `RunCommand cargo
    /// --version` under `mode = "confirm"` → `success: false`, "Action
    /// requires user approval". Asking the human first and executing after
    /// would reproduce P8a exactly, one config away.
    ///
    /// A failure here means a `confirm` verdict is being read as "ask, then it
    /// will run", which is the one thing it never means.
    #[tokio::test]
    async fn a_confirm_verdict_the_runtime_cannot_honour_is_refused_before_the_prompt() {
        let (outcome, asked, bridge_saw) = run(
            notification(),
            "confirm",
            "Requires confirmation: SendNotification requires confirmation",
        )
        .await;
        assert!(asked.is_empty(), "nobody may be asked: {asked:?}");
        assert!(
            matches!(outcome, DispatchOutcome::RuntimePolicyRefused { .. }),
            "{outcome:?}"
        );
        assert_eq!(bridge_saw, vec!["PolicyCheck".to_string()]);
    }

    /// No double gate. When the runtime would allow it, the human is asked
    /// exactly once and then it runs.
    ///
    /// A failure means the preflight has become a second question — the
    /// double-gating regression of 2026-08-12 rebuilt from the other side —
    /// or that a permitted action stopped running at all.
    #[tokio::test]
    async fn an_action_the_runtime_would_allow_is_asked_once_and_then_runs() {
        let (outcome, asked, bridge_saw) = run(notification(), "allow", "").await;
        assert_eq!(
            asked,
            vec!["SendNotification".to_string()],
            "exactly one question, asked by the canonical gate"
        );
        assert!(outcome.succeeded(), "{outcome:?}");
        assert_eq!(
            bridge_saw,
            vec!["PolicyCheck".to_string(), "SendNotification".to_string()],
            "the preflight precedes the action and does not replace it"
        );
    }

    /// SECURITY: fail closed. A preflight that produces no verdict must fall
    /// through to the human gate, never past it.
    ///
    /// A failure means a dead bridge, a truncated answer or an older runtime
    /// becomes a way to have a destructive action execute — or be approved —
    /// without the policy engine ever having spoken.
    #[tokio::test]
    async fn a_failed_preflight_never_grants() {
        struct MuteBridge {
            seen: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl Bridge for MuteBridge {
            async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
                let t = action["type"].as_str().unwrap_or("").to_string();
                self.seen.lock().unwrap().push(t.clone());
                if t == "PolicyCheck" {
                    // The shapes a broken or older runtime produces: an error,
                    // an envelope with no verdict, nothing recognisable.
                    return Err(anyhow::anyhow!("runtime went away"));
                }
                Ok(json!({"success": true, "action_type": t, "output": {}}))
            }
            async fn health_check(&mut self) -> anyhow::Result<bool> {
                Ok(true)
            }
        }

        // With nobody to ask, the fall-through is a denial — not an execution.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut bridge = MuteBridge { seen: seen.clone() };
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        let outcome = dispatch(&notification(), &mut ctx).await;
        assert!(
            matches!(
                outcome,
                DispatchOutcome::Denied {
                    reason: DenialReason::NoHumanChannel
                }
            ),
            "a preflight with no answer must leave the fail-closed gate in \
             charge, not wave the action through: {outcome:?}"
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec!["PolicyCheck".to_string()],
            "the action must not have run"
        );

        // And the unit-level statement of the same rule: no failure shape
        // reads as permission.
        for bad in [
            json!({"success": false, "error": {"message": "boom"}}),
            json!({"success": true}),
            json!({"success": true, "output": {"allowed": false, "reason": "?"}}),
        ] {
            assert_eq!(
                crate::agent::preflight::verdict_of(&bad),
                RuntimeVerdict::Unknown
            );
        }
    }

    /// Repo invariant 5, at the preflight: a composite is worth what it
    /// contains, and one refused child refuses the whole thing before anyone
    /// is asked.
    ///
    /// The runtime's policy engine walks `Pipeline`, `Try`, `SafeChange` and
    /// `If` and returns the first refusal (`policy.rs`, "preflight all
    /// steps"), so preflighting the root is preflighting the tree. A failure
    /// means an operator can be asked to approve a five-step pipeline whose
    /// second step was never going to run — the worst version of the defect,
    /// because the approval covers work that silently stops half way.
    #[tokio::test]
    async fn a_composite_with_one_denied_child_is_refused_before_the_prompt() {
        let pipeline = json!({
            "type": "Pipeline",
            "steps": [
                {"type": "ReadFile", "path": "/etc/hostname"},
                {"type": "RunCommand", "command": "cargo", "args": ["build"]},
            ]
        });
        let (outcome, asked, bridge_saw) = run(
            pipeline,
            "deny",
            "Pipeline rejected: step 1 (RunCommand) denied — Command 'cargo' is not \
             in the allowed_commands list.",
        )
        .await;
        assert!(
            asked.is_empty(),
            "the operator was asked to approve a pipeline that cannot run: {asked:?}"
        );
        assert!(
            matches!(&outcome, DispatchOutcome::RuntimePolicyRefused { reason, .. }
                if reason.contains("step 1")),
            "the refusal must name the child that caused it: {outcome:?}"
        );
        assert_eq!(bridge_saw, vec!["PolicyCheck".to_string()]);
    }

    /// The preflight is scoped to what the gate would ask about, and costs
    /// nothing anywhere else.
    ///
    /// A read is dispensed from the human gate, so there is no prompt for a
    /// later refusal to contradict; sending a `PolicyCheck` for it would add a
    /// round trip to the commonest action in the product and widen what is
    /// consulted for no gain. A failure means the preflight has become a
    /// blanket oracle rather than a fix for a specific ordering defect.
    #[tokio::test]
    async fn an_action_the_gate_waives_is_not_preflighted() {
        let (outcome, asked, bridge_saw) = run(
            json!({"type": "ReadFile", "path": "/etc/hostname"}),
            "allow",
            "",
        )
        .await;
        assert!(asked.is_empty(), "a read must not interrupt anyone");
        assert!(outcome.succeeded(), "{outcome:?}");
        assert_eq!(
            bridge_saw,
            vec!["ReadFile".to_string()],
            "a waived action goes straight to the runtime, with no probe"
        );
    }

    /// The preflight only speaks for the authority that will actually judge
    /// the action. An orchestrator sysaicall never reaches the runtime, so its
    /// policy engine has no opinion to offer and must not be consulted.
    ///
    /// A failure means the orchestrator is asking the runtime to adjudicate
    /// something the runtime will never see — an answer that is either
    /// meaningless or, worse, a refusal of an action that was always going to
    /// run locally.
    #[tokio::test]
    async fn an_orchestrator_sysaicall_is_not_preflighted_against_the_runtime() {
        let (mut bridge, seen) = PolicyBridge::new("deny", "denied");
        let mut sink = NullDisplaySink;
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(&mut bridge),
            sink: &mut sink,
            cancel: &cancel,
            sysaicalls: None,
            authority: &mut authority,
            spawn_factory: None,
            budget: None,
        };
        // `RunIncus` is gated by the orchestrator's own table and routes local.
        let outcome = dispatch(&json!({"type": "RunIncus"}), &mut ctx).await;
        assert!(
            matches!(outcome, DispatchOutcome::Denied { .. }),
            "the human gate, not the runtime, is what refuses this: {outcome:?}"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "the runtime was consulted about an action it will never see"
        );
    }
}

/// What the model is told about a refusal the operator DECIDED.
///
/// # The defect, measured
///
/// G1, 2026-08-16, on HEAD 576c5cb. The runtime refuses `RunCommand`, the
/// session gate puts it to the operator, the operator presses `3` (deny), and
/// the model is handed:
///
/// ```text
/// `RunCommand` was refused by the runtime's policy before it ran,
/// so the user was not asked: …
/// ```
///
/// The last clause is false, and it is false about the one thing the operator
/// had just done. A model told "nobody asked" reasonably assumes an
/// unattended wall and starts looking for a way around it; a model told "the
/// user said no" has been told the truth and can say so back.
#[cfg(test)]
mod session_gate_reporting {
    use super::*;
    use crate::agent::cancel::CancelToken;
    use crate::config::Config;
    use crate::display_sink::{ConfirmOutcome, RecordingDisplaySink};
    use serde_json::json;

    /// A runtime that refuses `RunCommand` and says the refusal is liftable —
    /// the shape the real binary produces since the session-gate lot.
    struct LiftableBridge {
        /// `false` drops the `liftable` field: an older runtime, or a refusal
        /// that is a genuine wall.
        liftable: bool,
    }

    #[async_trait::async_trait]
    impl Bridge for LiftableBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            let t = action["type"].as_str().unwrap_or("");
            if t != "PolicyCheck" {
                anyhow::bail!("nothing may be executed on this path");
            }
            let mut output = json!({
                "kind": "policy_check_result",
                "allowed": false,
                "verdict": "deny",
                "reason": "Command 'cargo' is not in the allowed_commands list.",
            });
            if self.liftable {
                output["liftable"] = json!({"kind": "command", "value": "cargo"});
            }
            Ok(json!({"success": true, "action_type": "PolicyCheck", "output": output}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    fn cargo_build() -> Value {
        json!({"type": "RunCommand", "command": "cargo", "args": ["build"]})
    }

    /// Dispatch a liftable refusal against a human who answers `answer`
    /// (or nobody at all, when it is `None`). Returns the outcome and the
    /// envelope the model would receive.
    async fn refuse_with(
        answer: Option<ConfirmOutcome>,
        liftable: bool,
    ) -> (DispatchOutcome, Value) {
        let mut bridge = LiftableBridge { liftable };
        let mut sink = RecordingDisplaySink {
            scripted_answers: answer.into_iter().collect(),
            ..Default::default()
        };
        let cancel = CancelToken::new();
        let config = Config::default();
        let mut authority = crate::agent::confirm::ApprovalAuthority::default();
        let action = cargo_build();
        let outcome = {
            let mut ctx = DispatchContext {
                undo: None,
                journal: None,
                frontend: None,
                history: None,
                config: &config,
                bridge: Some(&mut bridge),
                sink: &mut sink,
                cancel: &cancel,
                sysaicalls: None,
                authority: &mut authority,
                spawn_factory: None,
                budget: None,
            };
            dispatch(&action, &mut ctx).await
        };
        let envelope = outcome.to_result_envelope(&action, None);
        (outcome, envelope)
    }

    fn message(envelope: &Value) -> String {
        envelope["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    /// SECURITY: a refusal the operator chose must be reported as his choice.
    ///
    /// A failure means the model is told again that nobody was asked about an
    /// action a person had just declined on screen — the report writes itself
    /// into the transcript, so the operator reads his own decision denied
    /// twice: once by the machine and once by the account of it.
    #[tokio::test]
    async fn a_denied_session_gate_is_reported_as_the_operators_decision() {
        let (outcome, envelope) = refuse_with(Some(ConfirmOutcome::No), true).await;
        assert!(
            matches!(
                &outcome,
                DispatchOutcome::RuntimePolicyRefused {
                    gate: PolicyRefusalGate::OperatorDeclined,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let m = message(&envelope);
        assert!(
            !m.contains("was not asked"),
            "the operator answered, and the model was told he was not asked: {m}"
        );
        assert!(
            m.contains("the user answered no"),
            "the model must be told whose decision this was: {m}"
        );
        // Still a refusal, and still the same class: only the wording moved.
        assert_eq!(envelope["success"], json!(false));
        assert_eq!(envelope["error"]["class"], json!("policy_denied"));
    }

    /// The other half, and the one that must not drift: a wall is still a wall.
    ///
    /// A failure means an unliftable refusal — nothing to widen, no question
    /// possible — is reported as somebody's answer, which invents a decision
    /// no one made.
    #[tokio::test]
    async fn an_unliftable_refusal_still_says_nobody_was_asked() {
        let (outcome, envelope) = refuse_with(Some(ConfirmOutcome::Yes), false).await;
        assert!(
            matches!(
                &outcome,
                DispatchOutcome::RuntimePolicyRefused {
                    gate: PolicyRefusalGate::NotAsked,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(message(&envelope).contains("the user was not asked"));
    }

    /// SECURITY: silence is not an answer, and must not be reported as one.
    ///
    /// EOF, a closed stdin, a sub-agent, `--serve`: `decide_policy_lift`
    /// returns `NoHumanChannel`, and no one declined anything. A failure here
    /// would put words in the operator's mouth on precisely the paths where
    /// there is no operator — the same confusion in the opposite direction.
    #[tokio::test]
    async fn a_gate_nobody_answered_is_not_reported_as_a_decision() {
        let (outcome, envelope) = refuse_with(None, true).await;
        assert!(
            matches!(
                &outcome,
                DispatchOutcome::RuntimePolicyRefused {
                    gate: PolicyRefusalGate::NotAsked,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let m = message(&envelope);
        assert!(
            !m.contains("answered no"),
            "nobody was there, and the model was told somebody said no: {m}"
        );
    }
}

/// One question per command. A lift the operator granted is his
/// approval of the action it names; "add to config" is not asked again in
/// the session; sandbox yolo asks nothing at all.
#[cfg(test)]
mod one_question {
    use super::*;
    use crate::agent::approval::{ApprovalAuthority, OperatorTrust};
    use crate::agent::cancel::CancelToken;
    use crate::config::Config;
    use crate::display_sink::{ConfirmOutcome, RecordingDisplaySink};
    use serde_json::json;

    /// Refuses `cargo` (liftable) until a PolicyGrant arrives, then allows
    /// and runs it.
    #[derive(Default)]
    struct GrantingBridge {
        granted: bool,
        ran: usize,
    }

    #[async_trait::async_trait]
    impl Bridge for GrantingBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            if action["type"] == "PolicyCheck" {
                let output = if self.granted {
                    json!({"kind": "policy_check_result", "allowed": true, "verdict": "allow"})
                } else {
                    json!({"kind": "policy_check_result", "allowed": false, "verdict": "deny",
                           "reason": "Command 'cargo' is not in the allowed_commands list.",
                           "liftable": {"kind": "command", "value": "cargo"}})
                };
                return Ok(
                    json!({"success": true, "action_type": "PolicyCheck", "output": output}),
                );
            }
            self.ran += 1;
            Ok(json!({"success": true, "action_type": "RunCommand", "output": {"stdout": "ok"}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn send_control(&mut self, _m: &Value) -> anyhow::Result<Value> {
            self.granted = true;
            Ok(json!({"success": true, "output": {"data": {}}}))
        }
    }

    async fn run(
        bridge: &mut GrantingBridge,
        sink: &mut RecordingDisplaySink,
        authority: &mut ApprovalAuthority,
    ) -> DispatchOutcome {
        let cancel = CancelToken::new();
        let config = Config::default();
        let action = json!({"type": "RunCommand", "command": "cargo", "args": ["build"]});
        let mut ctx = DispatchContext {
            undo: None,
            journal: None,
            frontend: None,
            history: None,
            config: &config,
            bridge: Some(bridge),
            sink,
            cancel: &cancel,
            sysaicalls: None,
            authority,
            spawn_factory: None,
            budget: None,
        };
        dispatch(&action, &mut ctx).await
    }

    #[tokio::test]
    async fn a_lifted_command_is_asked_once() {
        let mut bridge = GrantingBridge::default();
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Yes].into_iter().collect(),
            ..Default::default()
        };
        let mut authority = ApprovalAuthority::default();
        let out = run(&mut bridge, &mut sink, &mut authority).await;
        assert!(matches!(out, DispatchOutcome::Executed { .. }), "{out:?}");
        assert_eq!(
            sink.confirmations_requested.len(),
            1,
            "{:?}",
            sink.confirmations_requested
        );
        assert_eq!(bridge.ran, 1);
    }

    #[tokio::test]
    async fn add_to_config_is_not_asked_again_this_session() {
        let mut bridge = GrantingBridge::default();
        let mut sink = RecordingDisplaySink {
            scripted_answers: [ConfirmOutcome::Always].into_iter().collect(),
            ..Default::default()
        };
        let mut authority = ApprovalAuthority::default();
        let first = run(&mut bridge, &mut sink, &mut authority).await;
        assert!(
            matches!(first, DispatchOutcome::Executed { .. }),
            "{first:?}"
        );
        let second = run(&mut bridge, &mut sink, &mut authority).await;
        assert!(
            matches!(second, DispatchOutcome::Executed { .. }),
            "{second:?}"
        );
        assert_eq!(
            sink.confirmations_requested.len(),
            1,
            "{:?}",
            sink.confirmations_requested
        );
        assert_eq!(bridge.ran, 2);
    }

    /// In sandbox yolo a policy refusal stands unless a human lifts it.
    #[tokio::test]
    async fn sandbox_yolo_does_not_lift_a_policy_refusal() {
        let mut bridge = GrantingBridge::default();
        let mut sink = RecordingDisplaySink::default();
        let mut authority = ApprovalAuthority::new(OperatorTrust::IsolatedEnvironment, "sbx");
        let out = run(&mut bridge, &mut sink, &mut authority).await;
        assert!(
            matches!(out, DispatchOutcome::RuntimePolicyRefused { .. }),
            "{out:?}"
        );
        assert_eq!(bridge.ran, 0);
        assert!(!bridge.granted);
    }
}
