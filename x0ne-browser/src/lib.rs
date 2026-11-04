//! X0NE-Browser — CDP-driven Chromium tools for AI agents that develop
//! and test frontends.
//!
//! Architecture mirrors `x0ne-vm`: trait abstraction + MCP server binary.
//! See README + docs/browser-tools.md for the full design.

pub mod allowlist;
pub mod audit;
pub mod chromium;
pub mod error;
pub mod rate_limit;
pub mod selector;
pub mod session;

pub use chromium::ChromiumDriver;
pub use error::{BrowserError, ErrorCode};
pub use session::BrowserConfig as SessionConfig;
