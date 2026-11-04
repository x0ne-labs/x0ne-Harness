//! Capabilities a node may use. A child's set must be a subset of its
//! parent's: checked by the orchestrator at spawn, never by the node.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    Read,
    Write,
    Exec,
    Network,
    Spawn,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapSet(BTreeSet<Capability>);

impl CapSet {
    pub fn of(caps: &[Capability]) -> Self {
        CapSet(caps.iter().copied().collect())
    }

    pub fn has(&self, c: Capability) -> bool {
        self.0.contains(&c)
    }

    pub fn is_subset_of(&self, other: &CapSet) -> bool {
        self.0.is_subset(&other.0)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }
}
