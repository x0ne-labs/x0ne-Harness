//! Re-export of the shared action vocabulary.
//!
//! `Action` and its side-effect semantics moved to the `x0ne-actions` crate so
//! the orchestrator can depend on the same types instead of matching on JSON
//! strings. This shim keeps `crate::action::…` paths working across the
//! runtime; new code should prefer `x0ne_actions::…` directly.

pub use x0ne_actions::*;
