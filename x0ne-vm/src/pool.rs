//! Pool-of-clones state machine — pure logic, no I/O.
//!
//! `x0ne-vmd` is the long-running daemon that uses this to maintain a
//! warm pool of cloned VMs. Each pool member is a linked CoW clone of a
//! single `base` VM (typically the user's golden image). The daemon
//! pre-creates `target` clones up-front so an agent's `acquire` request
//! returns in ~50ms instead of triggering a fresh 30s boot.
//!
//! Lifecycle:
//!   - `Free`     created, ready to hand out
//!   - `Acquired` currently held by an agent
//!   - `Recycling` being reverted to golden + returning to Free
//!   - `Reaped`   destroyed (clone disk gone); slot is empty, pool can refill
//!
//! State is in-memory + periodically persisted to disk for restart resilience.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    Free,
    Acquired { by: String, since_ms: u64 },
    Recycling,
    Reaped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    pub state: SlotState,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Name of the libvirt VM whose disk all clones link off.
    pub base: String,
    /// Target number of warm Free slots maintained by the daemon.
    pub target: usize,
    /// Naming prefix for clones — `<prefix>-<n>`. Default: `pool`.
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Hard cap on slots; daemon refuses to scale beyond this.
    #[serde(default = "default_max")]
    pub max: usize,
}

fn default_prefix() -> String {
    "pool".to_string()
}
fn default_max() -> usize {
    64
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            base: "x0ne-firefox".to_string(),
            target: 3,
            prefix: default_prefix(),
            max: default_max(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PoolState {
    pub config: PoolConfig,
    pub slots: BTreeMap<String, Slot>,
}

impl PoolState {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            slots: BTreeMap::new(),
        }
    }

    pub fn count_state(&self, state: &SlotState) -> usize {
        self.slots
            .values()
            .filter(|s| std::mem::discriminant(&s.state) == std::mem::discriminant(state))
            .count()
    }

    pub fn free(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s.state, SlotState::Free))
            .count()
    }
    pub fn acquired(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s.state, SlotState::Acquired { .. }))
            .count()
    }

    /// Pick the first Free slot in name order (deterministic for tests).
    pub fn first_free(&self) -> Option<&str> {
        self.slots
            .iter()
            .find(|(_, s)| matches!(s.state, SlotState::Free))
            .map(|(k, _)| k.as_str())
    }

    /// Mark a slot as acquired. Returns the slot if available.
    pub fn acquire(&mut self, by: &str, now_ms: u64) -> Option<&Slot> {
        let pick = self.first_free()?.to_string();
        let slot = self.slots.get_mut(&pick)?;
        slot.state = SlotState::Acquired {
            by: by.to_string(),
            since_ms: now_ms,
        };
        Some(&*slot)
    }

    /// Mark a slot as recycling (ready to be reverted to golden + freed).
    pub fn start_recycle(&mut self, name: &str) -> Result<(), &'static str> {
        let slot = self.slots.get_mut(name).ok_or("unknown slot")?;
        match slot.state {
            SlotState::Acquired { .. } => {
                slot.state = SlotState::Recycling;
                Ok(())
            }
            _ => Err("slot is not Acquired"),
        }
    }

    /// Finish recycling — slot becomes Free again.
    pub fn finish_recycle(&mut self, name: &str) -> Result<(), &'static str> {
        let slot = self.slots.get_mut(name).ok_or("unknown slot")?;
        match slot.state {
            SlotState::Recycling => {
                slot.state = SlotState::Free;
                Ok(())
            }
            _ => Err("slot is not Recycling"),
        }
    }

    /// How many fresh slots should we create to hit the target?
    /// Negative = no need (we already have target or more free).
    pub fn deficit(&self) -> i64 {
        self.config.target as i64 - self.free() as i64
    }

    /// Next available name in the `prefix-N` sequence.
    pub fn next_name(&self) -> Option<String> {
        if self.slots.len() >= self.config.max {
            return None;
        }
        for n in 1..=self.config.max {
            let candidate = format!("{}-{n}", self.config.prefix);
            if !self.slots.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Add a freshly cloned slot to the pool. Idempotent on duplicates.
    pub fn add_free(&mut self, name: &str, now_ms: u64) {
        self.slots.insert(
            name.to_string(),
            Slot {
                name: name.to_string(),
                state: SlotState::Free,
                created_ms: now_ms,
            },
        );
    }

    /// Remove a slot entirely (after destroy_clone). Frees the name for re-use.
    pub fn reap(&mut self, name: &str) {
        self.slots.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(target: usize) -> PoolState {
        PoolState::new(PoolConfig {
            base: "golden".into(),
            target,
            prefix: "pool".into(),
            max: 8,
        })
    }

    #[test]
    fn empty_pool_has_deficit_of_target() {
        let p = pool(3);
        assert_eq!(p.deficit(), 3);
        assert_eq!(p.next_name().as_deref(), Some("pool-1"));
    }

    #[test]
    fn fills_and_acquires() {
        let mut p = pool(3);
        p.add_free("pool-1", 100);
        p.add_free("pool-2", 200);
        p.add_free("pool-3", 300);
        assert_eq!(p.free(), 3);
        assert_eq!(p.deficit(), 0);
        let slot = p.acquire("agent-A", 400).unwrap();
        assert_eq!(slot.name, "pool-1"); // first free
        assert_eq!(p.acquired(), 1);
        assert_eq!(p.free(), 2);
        assert_eq!(p.deficit(), 1); // need one more to maintain target
    }

    #[test]
    fn name_sequencing_skips_used() {
        let mut p = pool(3);
        p.add_free("pool-1", 1);
        p.add_free("pool-3", 1);
        assert_eq!(p.next_name().as_deref(), Some("pool-2"));
        p.add_free("pool-2", 1);
        assert_eq!(p.next_name().as_deref(), Some("pool-4"));
    }

    #[test]
    fn recycle_lifecycle() {
        let mut p = pool(2);
        p.add_free("pool-1", 1);
        p.acquire("agent-A", 10);
        p.start_recycle("pool-1").unwrap();
        assert!(matches!(p.slots["pool-1"].state, SlotState::Recycling));
        p.finish_recycle("pool-1").unwrap();
        assert!(matches!(p.slots["pool-1"].state, SlotState::Free));
    }

    #[test]
    fn cannot_recycle_non_acquired() {
        let mut p = pool(1);
        p.add_free("pool-1", 1);
        assert!(p.start_recycle("pool-1").is_err());
        assert!(p.start_recycle("pool-2").is_err());
    }

    #[test]
    fn max_bound_respected() {
        let mut p = pool(8);
        p.config.max = 3;
        for n in 1..=3 {
            p.add_free(&format!("pool-{n}"), 1);
        }
        assert!(p.next_name().is_none(), "should refuse to scale beyond max");
    }

    #[test]
    fn reap_frees_the_name_slot() {
        let mut p = pool(2);
        p.add_free("pool-1", 1);
        p.add_free("pool-2", 1);
        p.reap("pool-1");
        assert_eq!(p.next_name().as_deref(), Some("pool-1"));
    }

    #[test]
    fn empty_pool_acquire_returns_none() {
        let mut p = pool(2);
        assert!(p.acquire("a", 0).is_none());
    }
}
