//! Request-level guards shared by every HTTP serve mode.
//!
//! Pure functions over header values, so `x0ne-runtime` and
//! `x0ne-orchestrator` apply the same rule without linking each other.
//! Threat: DNS rebinding and cross-site requests from a browser on the
//! operator's machine reaching a loopback server that runs actions.

use std::path::PathBuf;

/// Why a request was refused before reaching a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardRefusal {
    /// `Host` missing or not in the allow-list (DNS rebinding).
    Host(String),
    /// `Origin` present and not explicitly allowed.
    Origin(String),
    /// Browser fetch metadata says cross-site, with no allowed `Origin`.
    BrowserContext(String),
}

impl GuardRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            GuardRefusal::Host(_) => "host_not_allowed",
            GuardRefusal::Origin(_) => "origin_not_allowed",
            GuardRefusal::BrowserContext(_) => "browser_request_refused",
        }
    }
}

impl std::fmt::Display for GuardRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardRefusal::Host(h) => write!(f, "Host header {h:?} is not allowed"),
            GuardRefusal::Origin(o) => write!(f, "Origin {o:?} is not allowed"),
            GuardRefusal::BrowserContext(s) => {
                write!(f, "browser request (Sec-Fetch-Site: {s}) is not allowed")
            }
        }
    }
}

/// Accepted `Host` values. Default: the three loopback spellings with the
/// bound port. Extra entries (`name` or `name:port`) come from the operator.
#[derive(Debug, Clone)]
pub struct HostPolicy {
    allowed: Vec<String>,
}

impl HostPolicy {
    pub fn loopback(port: u16) -> Self {
        Self {
            allowed: vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ],
        }
    }

    /// Adds operator entries: a comma-separated list or an iterator.
    pub fn with_extra<I, S>(mut self, extra: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for e in extra {
            let e = normalize_host(e.as_ref());
            if !e.is_empty() && !self.allowed.contains(&e) {
                self.allowed.push(e);
            }
        }
        self
    }

    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    pub fn check(&self, host_header: Option<&str>) -> Result<(), GuardRefusal> {
        let Some(raw) = host_header else {
            return Err(GuardRefusal::Host("<missing>".into()));
        };
        let h = normalize_host(raw);
        if self.allowed.contains(&h) {
            Ok(())
        } else {
            Err(GuardRefusal::Host(truncate(raw)))
        }
    }
}

/// Accepted browser `Origin`s. Empty by default: no web page may drive the
/// server unless the operator lists its origin (or `*`).
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    allowed: Vec<String>,
    any: bool,
}

impl OriginPolicy {
    pub fn new<I, S>(allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut p = Self::default();
        for o in allowed {
            let o = o.as_ref().trim();
            if o == "*" {
                p.any = true;
            } else if !o.is_empty() {
                p.allowed.push(normalize_origin(o));
            }
        }
        p
    }

    pub fn allows_any(&self) -> bool {
        self.any
    }

    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    pub fn check(
        &self,
        origin: Option<&str>,
        sec_fetch_site: Option<&str>,
    ) -> Result<(), GuardRefusal> {
        if let Some(o) = origin {
            let n = normalize_origin(o);
            if self.any || self.allowed.contains(&n) {
                return Ok(());
            }
            return Err(GuardRefusal::Origin(truncate(o)));
        }
        // No Origin: only a browser sends fetch metadata. `none` is a URL
        // typed by the user; anything else was issued by a page.
        match sec_fetch_site.map(|s| s.trim().to_ascii_lowercase()) {
            None => Ok(()),
            Some(s) if s == "none" || self.any => Ok(()),
            Some(s) => Err(GuardRefusal::BrowserContext(truncate(&s))),
        }
    }
}

/// Splits a comma-separated operator list.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Where an auto-generated token is written: `$XDG_RUNTIME_DIR/x0ne/` when
/// set (tmpfs, per-user), else `$HOME/.x0ne/run/`.
pub fn token_file_path(
    xdg_runtime_dir: Option<&str>,
    home: Option<&str>,
    name: &str,
    port: u16,
) -> Option<PathBuf> {
    let file = format!("{name}-{port}.token");
    if let Some(d) = xdg_runtime_dir.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("x0ne").join(file));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".x0ne").join("run").join(file))
}

/// Same, from the process environment.
pub fn token_file_path_from_env(name: &str, port: u16) -> Option<PathBuf> {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let home = std::env::var("HOME").ok();
    token_file_path(xdg.as_deref(), home.as_deref(), name, port)
}

/// Hex-encodes random bytes supplied by the caller (this crate has no RNG).
pub fn token_from_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison for bearer tokens (length is not secret).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// `Authorization: Bearer <t>` matches `expected`. An empty `expected`
/// never matches: a missing token is a closed door, not an open one.
pub fn bearer_matches(authorization: Option<&str>, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    authorization
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.trim().as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

fn normalize_host(h: &str) -> String {
    h.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn normalize_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn truncate(s: &str) -> String {
    s.chars().take(100).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_with_the_bound_port_pass() {
        let p = HostPolicy::loopback(7700);
        for h in [
            "127.0.0.1:7700",
            "localhost:7700",
            "[::1]:7700",
            "LOCALHOST:7700",
        ] {
            assert!(p.check(Some(h)).is_ok(), "{h} refused");
        }
    }

    /// DNS rebinding: the page's own name reaches 127.0.0.1 with its Host.
    #[test]
    fn a_rebound_name_or_wrong_port_is_refused() {
        let p = HostPolicy::loopback(7700);
        for h in [
            "attacker.example:7700",
            "127.0.0.1:7701",
            "localhost",
            "127.0.0.1",
            "localhost.attacker.example:7700",
            "",
        ] {
            assert_eq!(
                p.check(Some(h)).unwrap_err().code(),
                "host_not_allowed",
                "{h} accepted"
            );
        }
        assert!(p.check(None).is_err(), "a missing Host was accepted");
    }

    #[test]
    fn the_operator_allow_list_extends_the_hosts() {
        let p =
            HostPolicy::loopback(7700).with_extra(split_list("example.com, api.example.com:8443"));
        assert!(p.check(Some("example.com")).is_ok());
        assert!(p.check(Some("api.example.com:8443")).is_ok());
        assert!(p.check(Some("api.example.com")).is_err());
        assert!(p.check(Some("evil.co")).is_err());
    }

    #[test]
    fn no_origin_and_no_fetch_metadata_is_a_non_browser_client() {
        let p = OriginPolicy::default();
        assert!(p.check(None, None).is_ok());
        assert!(p.check(None, Some("none")).is_ok(), "typed URL");
    }

    #[test]
    fn any_origin_is_refused_unless_listed() {
        let p = OriginPolicy::default();
        assert_eq!(
            p.check(Some("http://localhost:3000"), None)
                .unwrap_err()
                .code(),
            "origin_not_allowed"
        );
        assert!(p.check(Some("null"), None).is_err());
        let p = OriginPolicy::new(["https://x0ne.co/"]);
        assert!(p.check(Some("https://x0ne.co"), None).is_ok());
        assert!(p.check(Some("https://evil.co"), None).is_err());
    }

    #[test]
    fn cross_site_fetch_metadata_without_origin_is_refused() {
        let p = OriginPolicy::default();
        for s in ["cross-site", "same-site", "same-origin"] {
            assert_eq!(
                p.check(None, Some(s)).unwrap_err().code(),
                "browser_request_refused"
            );
        }
        assert!(OriginPolicy::new(["*"])
            .check(None, Some("cross-site"))
            .is_ok());
    }

    #[test]
    fn token_path_prefers_the_runtime_dir() {
        assert_eq!(
            token_file_path(Some("/run/user/1000"), Some("/home/u"), "serve", 7700),
            Some(PathBuf::from("/run/user/1000/x0ne/serve-7700.token"))
        );
        assert_eq!(
            token_file_path(None, Some("/home/u"), "runtime", 7878),
            Some(PathBuf::from("/home/u/.x0ne/run/runtime-7878.token"))
        );
        assert_eq!(token_file_path(Some(""), None, "x", 1), None);
    }

    #[test]
    fn an_empty_expected_token_never_matches() {
        assert!(!bearer_matches(Some("Bearer "), ""));
        assert!(!bearer_matches(None, "t"));
        assert!(!bearer_matches(Some("Bearer u"), "t"));
        assert!(!bearer_matches(Some("t"), "t"));
        assert!(bearer_matches(Some("Bearer t"), "t"));
        assert_eq!(token_from_bytes(&[0, 255, 16]), "00ff10");
    }
}
