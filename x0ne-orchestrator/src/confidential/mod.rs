//! Re-export shim: the confidential-runtime model graduated to the
//! `x0ne-confidential` crate. Call sites keep their paths; the crate is the
//! home. See `docs/CONFIDENTIAL_RUNTIME.md`.
pub use x0ne_confidential::model::*;
pub use x0ne_confidential::policy;
