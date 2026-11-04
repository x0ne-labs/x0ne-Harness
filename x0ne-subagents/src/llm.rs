//! The LLM boundary. Nodes never hold the API key: the orchestrator's broker
//! calls the backend, debits the node's budget and journals the call.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::node::{NodeView, TaskSpec};
use crate::report::{ChildReport, Report};

/// One step of a node. Exactly one `Observation` comes back per decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Spawn(TaskSpec),
    Exec {
        cmd: String,
    },
    Write {
        path: String,
        content: String,
    },
    Read {
        path: String,
    },
    /// Block until every child so far has finished; their reports come back
    /// as one `Observation::Children`, and verified changes are merged in.
    Wait,
    Report(Report),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Observation {
    SpawnAccepted {
        id: String,
    },
    SpawnRejected {
        reason: String,
    },
    Exec {
        cmd: String,
        exit: i32,
        stdout: String,
        stderr: String,
    },
    Wrote {
        path: String,
    },
    Read {
        path: String,
        content: String,
    },
    Denied {
        what: String,
        reason: String,
    },
    Children(Vec<ChildReport>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub tokens: u64,
    pub micro_usd: u64,
}

/// A failed call. `usage` is what the provider billed anyway (a reply with
/// no tool call is still paid for); the tree debits it before failing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmError {
    pub message: String,
    pub usage: Usage,
}

impl From<String> for LlmError {
    fn from(message: String) -> Self {
        LlmError {
            message,
            usage: Usage::default(),
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[async_trait::async_trait]
pub trait LlmBackend: Send + Sync {
    async fn next(
        &self,
        view: &NodeView,
        obs: &[Observation],
    ) -> Result<(Decision, Usage), LlmError>;
    fn name(&self) -> &'static str {
        "backend"
    }
}

/// Deterministic backend for tests: a closure decides from the node's view
/// and everything it has observed so far. Fixed cost per call.
pub struct ScriptedLlm {
    script: Box<dyn Fn(&NodeView, &[Observation]) -> Decision + Send + Sync>,
    pub usage: Usage,
}

impl ScriptedLlm {
    pub fn new<F>(script: F) -> Self
    where
        F: Fn(&NodeView, &[Observation]) -> Decision + Send + Sync + 'static,
    {
        ScriptedLlm {
            script: Box::new(script),
            usage: Usage {
                tokens: 100,
                micro_usd: 500,
            },
        }
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedLlm {
    async fn next(
        &self,
        view: &NodeView,
        obs: &[Observation],
    ) -> Result<(Decision, Usage), LlmError> {
        Ok(((self.script)(view, obs), self.usage))
    }
    fn name(&self) -> &'static str {
        "scripted"
    }
}

/// Holds the only copy of the API key. Nothing here is ever serialised.
pub struct Broker {
    backend: Arc<dyn LlmBackend>,
    api_key: String,
}

impl Broker {
    pub fn new(backend: Arc<dyn LlmBackend>, api_key: String) -> Self {
        Broker { backend, api_key }
    }

    pub async fn next(
        &self,
        view: &NodeView,
        obs: &[Observation],
    ) -> Result<(Decision, Usage), LlmError> {
        // A real backend would receive `self.api_key` here, in-process only.
        let _ = self.api_key.len();
        self.backend.next(view, obs).await
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Broker({})", self.backend.name())
    }
}
