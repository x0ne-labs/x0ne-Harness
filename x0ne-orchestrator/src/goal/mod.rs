//! Persistent agent goals — survive across turns and sessions.
//!
//! A `Goal` is a long-running intent that the host agent keeps working on
//! until it is `achieved`, `paused`, `cleared`, `blocked`, or
//! `budget_exhausted`. The orchestrator owns the persistence + state
//! machine, and exposes the lifecycle as 5 native MCP tools so that any
//! MCP-aware host (Claude Code, Codex, Cursor, …) gains the same
//! capability through one plug.
//!
//! Public surface used by `mcp_server.rs`:
//!
//! ```ignore
//! use crate::goal::{GoalManager, GoalStore, Budget, AchievementCheck};
//! ```

pub mod manager;
pub mod skeptic;
pub mod state;
pub mod store;

pub use manager::{GoalManager, SharedGoalManager, SkepticOutcome, StepOutcome};
pub use skeptic::{
    gap_fingerprint, skeptic_prompt, BorrowedSkeptic, SkepticOracle, SkepticVerdict,
    GOAL_STALL_THRESHOLD, SKEPTIC_PROMPT, SKEPTIC_SYSTEM,
};
pub use state::{
    AchievementCheck, Budget, GoalState, GoalStatus, HistoryEntry, HistoryKind, SkepticCadence,
};
pub use store::GoalStore;
