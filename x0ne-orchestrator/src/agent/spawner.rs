//! Starting a child agent, expressed as a capability rather than a transport.
//!
//! `SpawnAgent` is the one sysaicall that cannot be served by handing the
//! handler a slightly wider dependency. It does not *execute an action*; it
//! runs a whole agent. The historical implementation
//! (`sysaicalls::execute_spawn_agent`, removed 2026-09-23) took
//! `&mut Box<dyn Bridge>` and passed it straight into a recursive
//! `run_agent_loop`.
//!
//! # Action recursion is not agent recursion
//!
//! `Pipeline` and `Plan` nest *actions*: `agent::nested::execute_one_action`
//! is the right tool, because each child is one effect meeting one authority
//! boundary. `SpawnAgent` nests an *agent*: the child takes its own turns,
//! calls the model itself, and has a lifecycle. Substituting the nested action
//! executor for an agent loop would be a category error, so this is a separate
//! seam and lifecycle stays outside `Driver` — the driver runs a turn, the
//! spawner owns creating and repeating a child session.
//!
//! # Why this is narrower than a `Bridge`
//!
//! A `Bridge` is "send any action to the runtime": unbounded authority in one
//! method. [`SubAgentSpawner`] is "run this task as a child and tell me how it
//! went". The handler cannot reach the runtime through it, cannot choose what
//! the child executes, and cannot see the child's history, cancellation or
//! approvals. Whatever transport the implementation needs stays inside it. The
//! same shape as `CatalogQuery`: each dependency names the capability actually
//! required, and nothing else comes with it.

use serde_json::Value;

/// What the parent asks for. Derived from the fields
/// the removed `execute_spawn_agent` read off the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgentRequest {
    /// Caller-supplied identifier, used for correlation and display.
    pub id: String,
    /// The task, in the parent's words. This is the only instruction the child
    /// receives from the parent.
    pub task: String,
    /// Optional model override for the child.
    pub model: Option<String>,
}

impl SubAgentRequest {
    /// Read a request off a `SpawnAgent` action, exactly as the historical
    /// implementation does.
    pub fn from_action(action: &Value) -> Self {
        Self {
            id: action["id"].as_str().unwrap_or("").to_string(),
            task: action["task"].as_str().unwrap_or("").to_string(),
            model: action["model"].as_str().map(|s| s.to_string()),
        }
    }

    /// Whether this request is runnable. The historical path rejects an empty
    /// id or task before doing anything.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.is_empty() {
            return Err("SpawnAgent requires 'id'");
        }
        if self.task.is_empty() {
            return Err("SpawnAgent requires 'task'");
        }
        Ok(())
    }
}

/// How the child finished.
///
/// One field, because there is one record. An earlier version of this type
/// carried its own `success` / `summary` / `tokens` triple, which meant the
/// child lifecycle had two places to write statistics into and two chances to
/// disagree — and it duly shipped `tokens: 0`. `AgentResult` is what
/// `WaitAgent` and `ListAgents` already read, so it is what a finished child
/// hands back, built once by
/// [`child_result::agent_result_from_turn`](crate::agent::child_result::agent_result_from_turn).
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentOutcome {
    pub result: crate::sysaicalls::AgentResult,
}

impl SubAgentOutcome {
    /// The envelope the model sees, in the shape the historical path returned.
    pub fn to_envelope(&self) -> Value {
        crate::agent::child_result::to_inline_envelope(&self.result)
    }
}

/// Run one child agent.
///
/// The whole surface. No transport, no loop, no state — a request in, an
/// outcome out.
#[async_trait::async_trait]
pub trait SubAgentSpawner: Send {
    async fn spawn(&mut self, request: SubAgentRequest) -> SubAgentOutcome;
}

/// What a production implementation must do, written down before one exists.
///
/// Recorded here so session assembly cannot quietly choose something weaker.
/// The child is a *new* agent, not a continuation of the parent, and the
/// invariant that matters is short:
///
/// ```text
/// parent authority != child authority
/// ```
///
/// This is not a hypothetical. Answering "Always" once used to set
/// `config.ui.auto_confirm`, and `config.clone()` carried it into
/// every sub-agent — one keypress in the parent silently disarmed the gate in
/// every child. A spawner that clones parent state reproduces exactly that.
///
/// The requirements, each a thing a lazy implementation would skip:
///
/// * **Fresh `AgentState`/`History`.** The child starts from its task, not
///   from the parent's transcript.
/// * **Fresh `ApprovalAuthority`.** No inherited grants, no inherited standing
///   approvals, no inherited `config.ui.auto_confirm`, no global "Always".
/// * **A child cancellation scope.** Cancelling the parent must stop the
///   child; cancelling the child must not stop the parent.
/// * **Bounded child budget and action limits.**
/// * **Only explicitly intended task and context data.**
///
/// And one requirement about *where* the child runs: it must go through
/// canonical `Driver` execution. Wiring a production spawner onto any second
/// agent loop (the historical `run_agent_loop`, removed 2026-09-23, was one)
/// would let a gated parent escape into an ungated child. If a
/// Driver-backed child session cannot be built safely at session assembly, the
/// correct outcome is that Stage 1 stays blocked.
pub const PRODUCTION_SPAWNER_REQUIREMENTS: &[&str] = &[
    "fresh AgentState and History",
    "fresh ApprovalAuthority, no inherited grants",
    "no inherited config.ui.auto_confirm or global Always",
    "child cancellation scope",
    "bounded child budget and action limits",
    "only explicitly intended task and context data",
    "child runs through canonical Driver execution, never a second agent loop",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Records what it was asked for, and nothing can ask it for more.
    #[derive(Default)]
    pub struct FakeSpawner {
        pub requests: Vec<SubAgentRequest>,
    }

    #[async_trait::async_trait]
    impl SubAgentSpawner for FakeSpawner {
        async fn spawn(&mut self, request: SubAgentRequest) -> SubAgentOutcome {
            let (id, task) = (request.id.clone(), request.task.clone());
            self.requests.push(request);
            SubAgentOutcome {
                result: crate::sysaicalls::AgentResult {
                    id,
                    task,
                    response: "child finished".into(),
                    tokens_used: 42,
                    actions_executed: 0,
                    actions_breakdown: crate::sysaicalls::AgentResult::empty_breakdown(),
                    success: true,
                    duration_ms: 7,
                },
            }
        }
    }

    #[test]
    fn a_request_is_read_off_the_action_exactly_as_the_historical_path_does() {
        let action = json!({
            "type": "SpawnAgent",
            "id": "reviewer",
            "task": "review the diff",
            "model": "claude-sonnet-5",
        });
        let req = SubAgentRequest::from_action(&action);
        assert_eq!(req.id, "reviewer");
        assert_eq!(req.task, "review the diff");
        assert_eq!(req.model.as_deref(), Some("claude-sonnet-5"));
        assert!(req.validate().is_ok());
    }

    #[test]
    fn an_incomplete_request_is_rejected_before_a_child_is_started() {
        for (action, expected) in [
            (json!({"task": "x"}), "SpawnAgent requires 'id'"),
            (json!({"id": "a"}), "SpawnAgent requires 'task'"),
        ] {
            assert_eq!(
                SubAgentRequest::from_action(&action).validate(),
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn the_spawner_receives_the_task_and_returns_an_outcome() {
        let mut spawner = FakeSpawner::default();
        let req = SubAgentRequest::from_action(&json!({"id": "a", "task": "do it"}));
        let outcome = spawner.spawn(req.clone()).await;

        assert_eq!(spawner.requests, vec![req]);
        assert_eq!(outcome.result.id, "a");
        let envelope = outcome.to_envelope();
        assert_eq!(envelope["success"], true);
        assert_eq!(envelope["action_type"], "SpawnAgent");
        assert_eq!(envelope["output"]["tokens_used"], 42);
    }

    /// The trait carries no transport, no loop and no state. This is the
    /// property that makes it narrower than a `Bridge`: a holder cannot reach
    /// the runtime through it, cannot choose what the child executes, and
    /// cannot see the child's history, cancellation or approvals.
    #[test]
    fn the_spawner_surface_exposes_no_transport_or_agent_internals() {
        let src = include_str!("spawner.rs");
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        let at = production.find("pub trait SubAgentSpawner").unwrap();
        let end = production[at..]
            .find("\n}\n")
            .map(|i| at + i)
            .unwrap_or(production.len());
        let trait_body = &production[at..end];
        for banned in [
            "Bridge",
            "Driver",
            "History",
            "ApprovalAuthority",
            "CancelToken",
            "send_action",
        ] {
            assert!(
                !trait_body.contains(banned),
                "`{banned}` appears in the SubAgentSpawner surface — the handler \
                 would then know how children are run, which is the thing this \
                 seam exists to hide"
            );
        }
    }

    /// The production contract is written down before anything implements it,
    /// so session assembly cannot pick something weaker by omission. The
    /// authority clause is the one that has already been violated once:
    /// `3c88017` found `config.clone()` carrying a parent's "Always" into every
    /// child.
    #[test]
    fn the_production_contract_forbids_inheriting_parent_authority() {
        let joined = PRODUCTION_SPAWNER_REQUIREMENTS.join(" | ");
        for required in [
            "fresh ApprovalAuthority",
            "no inherited grants",
            "auto_confirm",
            "cancellation scope",
            "canonical Driver execution",
        ] {
            assert!(
                joined.contains(required),
                "the production spawner contract does not require `{required}`"
            );
        }
        assert!(
            joined.contains("never a second agent loop"),
            "a production spawner could be wired onto another loop, letting a \
             gated parent escape into an ungated child"
        );
        // The clause is enforced on the code by
        // `session::tests::guard_the_crate_has_exactly_one_agent_loop`.
    }
}
