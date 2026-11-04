//! The canonical agent engine.
//!
//! One implementation of execution semantics, shared by every frontend. The
//! five loops that preceded it had drifted apart in ways nothing detected:
//! two of five gated destructive actions, three of five could detect a model
//! looping, one of five could cancel an action already in flight, and one
//! advertised a spend ceiling it never enforced. `docs/agent-loop-parity.md`
//! records that matrix and classifies each divergence, so the migration
//! cannot quietly lose a behaviour or quietly inherit a bug.
//!
//! The split is deliberate: each component below owns one decision and can be
//! tested without a model, a bridge or a terminal. Moving the old 2000-line
//! function into a new file would have preserved the coupling that made the
//! divergence possible in the first place.

pub mod approval;
pub mod asm_guide;
pub mod budget;
pub mod cancel;
pub mod catalog;
pub mod child_result;
pub mod cli_sink;
pub mod confirm;
pub mod destructive;
pub mod dispatch;
pub mod driver;
pub mod history;
pub mod honesty;
pub mod journal;
pub mod lease;
pub mod loop_detect;
pub mod model;
pub mod nested;
pub mod observe;
pub mod parse;
pub mod plan_mode;
#[cfg(test)]
mod playground_e2e;
pub mod preflight;
pub mod pricing;
pub mod readonly_cmd;
pub mod readonly_net;
pub mod session;
pub mod slim;
pub mod spawner;
pub mod sysaicall_handler;
pub mod trust;
pub mod undo;
pub mod world;
