//! What a node says (Report) and what its parent is allowed to see
//! (ChildReport): typed, bounded data. `verified` is set by the verifier.

use serde::{Deserialize, Serialize};

use crate::node::NodeId;

/// Emitted by a node's LLM. `claims_verified` is ignored by design;
/// `as_node` exists so a forged identity can be detected and journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub summary: String,
    pub claims_verified: bool,
    pub as_node: Option<NodeId>,
}

/// The only thing that crosses from a child to its parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildReport {
    pub node: NodeId,
    pub state: String,
    pub verified: bool,
    pub summary: String,
    pub evidence: String,
}

/// Cut on a char boundary, never on a byte.
pub fn bound(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_is_by_chars() {
        assert_eq!(bound("héllo", 10), "héllo");
        assert_eq!(bound("héllo", 4), "h...");
        assert_eq!(bound("", 0), "");
    }
}
