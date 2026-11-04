//! Re-export shim: the lease model graduated to the `x0ne-confidential`
//! crate (the trust base X0NE shares with provider adapters and, later, the
//! runtime inside the attested environment). Call sites keep their paths.
pub use x0ne_confidential::lease::*;
