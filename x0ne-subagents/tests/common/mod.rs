//! Shared fixture: a tree rooted in a temp workspace, driven by a scripted LLM.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use x0ne_subagents::budget::Budget;
use x0ne_subagents::caps::{CapSet, Capability};
use x0ne_subagents::llm::{Decision, LlmBackend, Observation, ScriptedLlm};
use x0ne_subagents::node::{ModelTier, NodeId, NodeView, TaskSpec};
use x0ne_subagents::tree::{Tree, TreeConfig};

pub const API_KEY: &str = "sk-test-SECRET-never-in-sandbox";

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub host_workspace: PathBuf,
    pub canary: PathBuf,
    pub journal_dir: PathBuf,
    pub sandbox_dir: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let host_workspace = dir.path().join("workspace");
        std::fs::create_dir_all(host_workspace.join("src")).unwrap();
        std::fs::write(
            host_workspace.join("src/lib.rs"),
            "pub fn one() -> u8 { 1 }\n",
        )
        .unwrap();
        let canary = dir.path().join("host-canary.txt");
        std::fs::write(&canary, "HOST-CANARY-DO-NOT-READ\n").unwrap();
        let journal_dir = dir.path().join("journal");
        let sandbox_dir = dir.path().join("sandboxes");
        Self {
            dir,
            host_workspace,
            canary,
            journal_dir,
            sandbox_dir,
        }
    }

    pub fn tree(&self, llm: ScriptedLlm, cfg: TreeConfig) -> Tree {
        self.tree_with(Arc::new(llm), cfg)
    }

    pub fn tree_with(&self, llm: Arc<dyn LlmBackend>, cfg: TreeConfig) -> Tree {
        Tree::new(
            cfg,
            &self.journal_dir,
            &self.sandbox_dir,
            &self.host_workspace,
            llm,
            API_KEY.to_string(),
        )
        .expect("tree")
    }
}

pub fn root_spec(statement: &str, actions: u64) -> TaskSpec {
    TaskSpec {
        statement: statement.to_string(),
        caps: CapSet::of(&[
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Spawn,
        ]),
        budget: Budget {
            tokens: 10_000_000,
            micro_usd: 10_000_000,
            ms: 600_000,
            actions,
        },
        verify: Some("true".to_string()),
        model: ModelTier::Standard,
    }
}

pub fn child_spec(parent: &NodeView, statement: &str, divisor: u64) -> TaskSpec {
    let b = parent.remaining;
    TaskSpec {
        statement: statement.to_string(),
        caps: parent.caps.clone(),
        budget: Budget {
            tokens: b.tokens / divisor,
            micro_usd: b.micro_usd / divisor,
            ms: b.ms / divisor,
            actions: b.actions / divisor,
        },
        verify: Some("true".to_string()),
        model: ModelTier::Cheap,
    }
}

/// Count how many decisions this node has already taken (one per observation
/// of our own doing is not reliable, so scripts count via `turn`).
pub fn turn(obs: &[Observation]) -> usize {
    obs.len()
}

pub fn child_reports(obs: &[Observation]) -> Vec<&x0ne_subagents::report::ChildReport> {
    obs.iter()
        .filter_map(|o| match o {
            Observation::Children(v) => Some(v.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn id(s: &str) -> NodeId {
    NodeId::parse(s).unwrap()
}

pub fn report(summary: &str) -> Decision {
    Decision::Report(x0ne_subagents::report::Report {
        summary: summary.to_string(),
        claims_verified: false,
        as_node: None,
    })
}
