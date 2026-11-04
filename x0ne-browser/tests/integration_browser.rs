//! Integration tests that don't need real Chromium — exercise the pure
//! security and config parts via the public API.

use serde_json::json;
use x0ne_browser::allowlist::Allowlist;
use x0ne_browser::audit::redact;
use x0ne_browser::rate_limit::RateLimiter;
use x0ne_browser::selector::Selector;
use x0ne_browser::session::BrowserConfig;

#[test]
fn allowlist_default_is_loopback_and_about() {
    let al = Allowlist::default();
    assert!(al.allows("http://localhost:3000/"));
    assert!(al.allows("http://127.0.0.1:8080/health"));
    assert!(!al.allows("https://app.example.org/login"));
    assert!(al.allows("about:blank"));
    // SECURITY: data: URIs are NOT allowed by default — a data:text/html doc is
    // a cross-origin JS-navigation / exfiltration staging vector. Must be opted
    // into explicitly via config.
    assert!(!al.allows("data:text/html,<h1>hi"));
    assert!(!al.allows("https://evil.com/phish"));
    assert!(!al.allows("https://cdn.example.org.example.com"));
}

#[test]
fn audit_redacts_sensitive_keys() {
    let r = redact(&json!({
        "url": "https://app/login",
        "form": {"email": "a@b.c", "password": "hunter2", "api_key": "sk-x"}
    }));
    assert_eq!(r["url"], "https://app/login");
    assert_eq!(r["form"]["email"], "a@b.c");
    assert_eq!(r["form"]["password"], "<redacted>");
    assert_eq!(r["form"]["api_key"], "<redacted>");
}

#[test]
fn rate_limiter_blocks_after_capacity() {
    let rl = RateLimiter::new(3);
    assert!(rl.try_consume().is_ok());
    assert!(rl.try_consume().is_ok());
    assert!(rl.try_consume().is_ok());
    assert!(rl.try_consume().is_err());
    let (tokens, cap) = rl.snapshot();
    assert!(tokens < 1.0);
    assert_eq!(cap, 3);
}

#[test]
fn selector_parses_three_modes() {
    let css = Selector::parse("button.primary").unwrap();
    assert_eq!(css.as_css(), Some("button.primary"));

    let xp = Selector::parse("xpath://button[1]").unwrap();
    assert_eq!(xp.as_xpath(), Some("//button[1]"));

    let tx = Selector::parse("text:Log in").unwrap();
    assert_eq!(tx.as_text(), Some("Log in"));
}

#[test]
fn config_defaults_are_safe() {
    let c = BrowserConfig::default();
    assert!(!c.is_headless());
    assert_eq!(c.rate_limit_per_minute, 60);
    assert_eq!(c.default_timeout_ms, 10_000);
    assert!(c.max_screenshot_bytes >= 1024 * 1024);
    assert!(c.max_text_bytes >= 10_000);
}

#[test]
fn allowlist_rejects_unknown_scheme() {
    let al = Allowlist::default();
    assert!(!al.allows("ftp://localhost/"));
    assert!(!al.allows("file:///etc/passwd"));
    assert!(!al.allows("javascript:alert(1)"));
}

#[test]
fn allowlist_matches_specific_subdomain_only() {
    // Subdomain wildcard must not match a hostname that *contains* but
    // doesn't *end with* the suffix — guards against typosquat tricks.
    let al = Allowlist::new(vec!["https://*.example.org".into()]);
    assert!(!al.allows("https://example.org.evil.com"));
    assert!(al.allows("https://api.example.org"));
    assert!(al.allows("https://example.org"));
}

#[test]
fn allowlist_port_exact_match_when_no_wildcard() {
    let al = Allowlist::new(vec!["http://api.local:8000".into()]);
    assert!(al.allows("http://api.local:8000/v1/users"));
    assert!(!al.allows("http://api.local:8001"));
    assert!(!al.allows("http://api.local"));
}

#[test]
fn audit_doesnt_redact_innocuous_keys() {
    use serde_json::Value;
    let r = redact(&json!({
        "ok": true,
        "url": "https://app/login",
        "method": "POST",
        "user_agent": "agent/1.0",
        "count": 42,
    }));
    assert_eq!(r["ok"], Value::Bool(true));
    assert_eq!(r["url"], "https://app/login");
    assert_eq!(r["method"], "POST");
    assert_eq!(r["user_agent"], "agent/1.0");
    assert_eq!(r["count"], 42);
}

#[test]
fn audit_log_path_is_under_x0ne_home() {
    use x0ne_browser::audit::log_path;
    let p = log_path();
    let s = p.to_string_lossy();
    assert!(s.contains(".x0ne"));
    assert!(s.contains("logs"));
    assert!(s.contains("browser"));
    assert!(s.ends_with(".jsonl"));
}
