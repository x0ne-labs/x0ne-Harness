//! Typed error codes returned to MCP callers — the agent can branch on
//! `code` rather than parsing a free-form message. Mirrors the discipline
//! used by x0ne-runtime's action errors.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable error codes — values must NOT change between versions, only added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Navigation/interaction refused because target host isn't in the allowlist.
    DomainNotAllowed,
    /// Caller used a CSS/XPath/text selector that returned no element.
    SelectorNotFound,
    /// `wait_for` exceeded its timeout.
    Timeout,
    /// `goto` failed (DNS, TLS, 5xx — message has the http detail).
    NavigationFailed,
    /// The abort switch file `~/.x0ne/browser-tools.kill` exists.
    KilledByOperator,
    /// Caller exceeded the per-session rate limit.
    RateLimited,
    /// Action requires explicit confirmation (e.g. submit to cross-domain).
    /// Caller can retry with `force_cross_domain: true` (or similar).
    NeedsConfirmation,
    /// Chromium subprocess hasn't been started yet or has crashed.
    NotConnected,
    /// Selector syntax is malformed (missing closing bracket, etc.).
    InvalidSelector,
    /// `eval_js`, file upload, etc. — explicitly disabled in v1.
    NotImplemented,
    /// Any other internal failure with detail in `message`.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DomainNotAllowed => "domain_not_allowed",
            Self::SelectorNotFound => "selector_not_found",
            Self::Timeout => "timeout",
            Self::NavigationFailed => "navigation_failed",
            Self::KilledByOperator => "killed_by_operator",
            Self::RateLimited => "rate_limited",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::NotConnected => "not_connected",
            Self::InvalidSelector => "invalid_selector",
            Self::NotImplemented => "not_implemented",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Error)]
#[error("{code:?}: {message}")]
pub struct BrowserError {
    pub code: ErrorCode,
    pub message: String,
    /// Free-form structured detail (e.g. allowlist contents on a domain refusal).
    pub detail: Option<serde_json::Value>,
}

impl BrowserError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

impl From<BrowserError> for serde_json::Value {
    fn from(e: BrowserError) -> Self {
        let mut obj = serde_json::Map::new();
        obj.insert("ok".into(), serde_json::Value::Bool(false));
        obj.insert(
            "code".into(),
            serde_json::Value::String(e.code.as_str().into()),
        );
        obj.insert("message".into(), serde_json::Value::String(e.message));
        if let Some(d) = e.detail {
            obj.insert("detail".into(), d);
        }
        serde_json::Value::Object(obj)
    }
}

impl From<anyhow::Error> for BrowserError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(ErrorCode::Internal, e.to_string())
    }
}

pub type BrowserResult<T> = Result<T, BrowserError>;
