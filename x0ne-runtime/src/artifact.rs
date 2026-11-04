//! Artifact model + storage layout — moved to the standalone `x0ne-artifact`
//! crate so the proxy daemon (`x0ne-artifact-proxy`) doesn't pull the entire
//! runtime as a build dep. This module is now a thin re-export so existing
//! `crate::artifact::…` imports across the runtime keep compiling.
//!
//! New code should prefer `use x0ne_artifact::…` directly.

pub use x0ne_artifact::*;
