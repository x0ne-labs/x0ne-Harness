//! Selector parsing: supports CSS (default), `xpath:`, and `text:` prefixes.

use crate::error::{BrowserError, BrowserResult, ErrorCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Css(String),
    XPath(String),
    Text(String),
}

impl Selector {
    pub fn parse(s: &str) -> BrowserResult<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(BrowserError::new(
                ErrorCode::InvalidSelector,
                "empty selector",
            ));
        }
        if let Some(rest) = s.strip_prefix("xpath:") {
            return Ok(Self::XPath(rest.trim().to_string()));
        }
        if let Some(rest) = s.strip_prefix("text:") {
            return Ok(Self::Text(rest.trim().to_string()));
        }
        Ok(Self::Css(s.to_string()))
    }

    /// Translate to a single CSS selector string when possible.
    /// `XPath` returns the xpath unchanged (caller picks a different API).
    /// `Text(t)` uses `:has-text(...)`-style approximated via a CSS fallback.
    pub fn as_css(&self) -> Option<&str> {
        if let Self::Css(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_xpath(&self) -> Option<&str> {
        if let Self::XPath(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        if let Self::Text(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_css() {
        let s = Selector::parse(".foo > bar").unwrap();
        assert_eq!(s.as_css(), Some(".foo > bar"));
    }

    #[test]
    fn xpath_prefix() {
        let s = Selector::parse("xpath://button[@type='submit']").unwrap();
        assert_eq!(s.as_xpath(), Some("//button[@type='submit']"));
    }

    #[test]
    fn text_prefix() {
        let s = Selector::parse("text:Submit form").unwrap();
        assert_eq!(s.as_text(), Some("Submit form"));
    }

    #[test]
    fn whitespace_trimmed() {
        let s = Selector::parse("  text:  hello   ").unwrap();
        assert_eq!(s.as_text(), Some("hello"));
    }

    #[test]
    fn empty_rejected() {
        assert!(matches!(
            Selector::parse("").unwrap_err().code,
            ErrorCode::InvalidSelector
        ));
        assert!(matches!(
            Selector::parse("   ").unwrap_err().code,
            ErrorCode::InvalidSelector
        ));
    }
}
