//! Node identity (hierarchical path), state, task contract and the view a
//! node's LLM gets of itself.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::caps::CapSet;

/// `root`, `root/1`, `root/1/3/2`. Children are numbered from 1 in spawn order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(Vec<u32>);

impl NodeId {
    pub fn root() -> Self {
        NodeId(Vec::new())
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = s.split('/');
        if parts.next() != Some("root") {
            return Err(format!("node id must start with root: {s}"));
        }
        let mut v = Vec::new();
        for p in parts {
            let n: u32 = p
                .parse()
                .map_err(|_| format!("bad node id segment {p:?} in {s}"))?;
            if n == 0 {
                return Err(format!("segments start at 1: {s}"));
            }
            v.push(n);
        }
        Ok(NodeId(v))
    }

    pub fn child(&self, n: u32) -> NodeId {
        let mut v = self.0.clone();
        v.push(n);
        NodeId(v)
    }

    pub fn parent(&self) -> Option<NodeId> {
        if self.0.is_empty() {
            None
        } else {
            Some(NodeId(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    pub fn is_ancestor_of(&self, other: &NodeId) -> bool {
        other.0.len() > self.0.len() && other.0[..self.0.len()] == self.0[..]
    }

    /// Ancestors from the parent up to the root.
    pub fn ancestors(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut cur = self.parent();
        while let Some(p) = cur {
            cur = p.parent();
            out.push(p);
        }
        out
    }

    /// Safe file stem: `root_1_3`.
    pub fn file_stem(&self) -> String {
        self.to_string().replace('/', "_")
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "root")?;
        for n in &self.0 {
            write!(f, "/{n}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for NodeId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        NodeId::parse(&s)
    }
}

impl From<NodeId> for String {
    fn from(n: NodeId) -> String {
        n.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTier {
    Cheap,
    Standard,
    Strong,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Queued,
    Running,
    Verified,
    /// Reported, but no verifier was declared: the parent sees no proof.
    Unverified,
    Failed(String),
    Cancelled,
}

impl NodeState {
    pub fn label(&self) -> String {
        match self {
            NodeState::Queued => "queued".into(),
            NodeState::Running => "running".into(),
            NodeState::Verified => "verified".into(),
            NodeState::Unverified => "unverified".into(),
            NodeState::Failed(r) => format!("failed: {r}"),
            NodeState::Cancelled => "cancelled".into(),
        }
    }
}

/// The contract a parent writes for a child. The orchestrator checks it
/// against the parent's own caps and ledger before anything runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub statement: String,
    pub caps: CapSet,
    pub budget: Budget,
    /// Shell command run in the child's sandbox after it reports; exit 0 = verified.
    pub verify: Option<String>,
    pub model: ModelTier,
}

/// What a node's LLM is told about itself. No key, no paths, no siblings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeView {
    pub id: NodeId,
    pub depth: usize,
    pub statement: String,
    pub caps: CapSet,
    pub remaining: Budget,
    pub model: ModelTier,
}

/// Normalised statement, the key for loop detection.
pub fn normalise_statement(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_nest() {
        let n = NodeId::parse("root/1/3/2").unwrap();
        assert_eq!(n.to_string(), "root/1/3/2");
        assert_eq!(n.depth(), 3);
        assert_eq!(n.parent().unwrap().to_string(), "root/1/3");
        assert!(NodeId::root().is_ancestor_of(&n));
        assert!(!n.is_ancestor_of(&NodeId::root()));
        assert_eq!(n.ancestors().len(), 3);
        assert_eq!(n.file_stem(), "root_1_3_2");
        assert!(NodeId::parse("root/0").is_err());
        assert!(NodeId::parse("x/1").is_err());
        let j = serde_json::to_string(&n).unwrap();
        assert_eq!(j, "\"root/1/3/2\"");
        assert_eq!(serde_json::from_str::<NodeId>(&j).unwrap(), n);
    }

    #[test]
    fn statements_normalise() {
        assert_eq!(normalise_statement("  Refactor   AUTH "), "refactor auth");
    }
}
