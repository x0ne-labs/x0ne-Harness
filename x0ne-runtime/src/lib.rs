//! X0NE Runtime — secure local capability runtime for AI agents.
//!
//! This library exposes all runtime internals so that both the main binary
//! and the scenario runner can share the same types and logic.

pub mod action;
pub mod artifact;
pub mod artifact_serve;
pub mod asm_verify;
pub mod child_env;
pub mod composite;
pub mod config;
pub mod control;
pub mod dataset_cli;
pub mod docs;
pub mod executor;
pub mod handles;
pub mod mcp;
pub mod policy;
pub mod result;
pub mod scripted;
/// asm-ai host. Behind `asm-exec` (off by default): the default binary links
/// no wasm engine. Even when built, nothing here runs a program (T6+).
#[cfg(feature = "asm-exec")]
pub mod scripted_wasm;
pub mod server;

// Policy load matrix: it LOADS policy files through `AppConfig::load` instead
// of parsing fragments. Test-only — the harness has no reason to exist in a
// shipped binary.
#[cfg(test)]
mod policy_matrix;

// Re-export the dataset crate so existing internal callers can keep using
// `x0ne_runtime::dataset::*` (build/index/query bins).
pub use x0ne_dataset as dataset;

// Diagnostic / smoke-test surface.
//
// Always present in debug builds (so `cargo run` and `cargo test` both see
// it). In release builds it must be opted in explicitly with
// `--features self_test`. See `x0ne-runtime/src/self_test.rs` for rationale.
#[cfg(any(debug_assertions, feature = "self_test"))]
pub mod self_test;
