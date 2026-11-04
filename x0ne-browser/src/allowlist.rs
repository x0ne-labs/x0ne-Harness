//! Domain allowlist with simple glob semantics.
//!
//! Rules in the config (TOML or env-override) can be one of:
//!   - exact: `https://example.com`
//!   - port wildcard: `http://localhost:*`
//!   - subdomain wildcard: `https://*.example.org`
//!   - scheme wildcard: `data:*`, `about:*`
//!   - any of the above with trailing `:*` for "any port"
//!
//! Matching ignores the path/query/fragment — we only gate on origin.

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allowlist {
    pub rules: Vec<String>,
}

impl Default for Allowlist {
    fn default() -> Self {
        Self {
            rules: vec![
                "http://127.0.0.1:*".into(),
                "http://localhost:*".into(),
                "http://[::1]:*".into(),
                "https://localhost:*".into(),
                // NOTE: `data:*` is intentionally NOT in the defaults — a
                // `data:text/html,<script>…</script>` document is a cross-origin
                // JS-navigation / exfiltration staging vector. Add it explicitly
                // in ~/.x0ne/browser-tools.toml if a workflow truly needs it.
                "about:*".into(),
            ],
        }
    }
}

impl Allowlist {
    pub fn new(rules: Vec<String>) -> Self {
        Self { rules }
    }

    /// Returns true if `url` matches any rule.
    pub fn allows(&self, url: &str) -> bool {
        self.rules.iter().any(|r| rule_matches(r, url))
    }

    /// Returns the list of rules — useful for the `domain_not_allowed`
    /// error's `detail` field so the agent sees what *would* be allowed.
    pub fn rules(&self) -> &[String] {
        &self.rules
    }
}

fn rule_matches(rule: &str, url: &str) -> bool {
    // Scheme-only wildcard: "data:*", "about:*"
    if let Some(prefix) = rule.strip_suffix(":*") {
        if !prefix.contains("://") && !prefix.contains('/') && !prefix.is_empty() {
            return url.starts_with(&format!("{prefix}:"));
        }
    }

    // For URL rules we may have a wildcard port suffix (:*) — strip it before
    // parsing, since `:*` isn't a legal port number.
    let any_port = rule.ends_with(":*");
    let rule_to_parse: String = if any_port {
        rule[..rule.len() - 2].to_string()
    } else {
        rule.to_string()
    };

    let Ok(u) = Url::parse(url) else {
        return false;
    };
    let Ok(r) = Url::parse(&rule_to_parse) else {
        return false;
    };

    if u.scheme() != r.scheme() {
        return false;
    }

    let rule_host_orig = extract_host(rule).unwrap_or_default();
    let url_host = u.host_str().unwrap_or("");

    if !host_matches(&rule_host_orig, url_host) {
        return false;
    }

    if any_port {
        return true;
    }
    u.port_or_known_default() == r.port_or_known_default()
}

fn extract_host(rule: &str) -> Option<String> {
    let s = rule.split("://").nth(1)?;
    let s = s.split('/').next()?;
    // IPv6 hosts come wrapped in brackets: [::1]:8080 → host part is "[::1]".
    if let Some(close) = s.find(']') {
        return Some(s[..=close].to_string());
    }
    // Otherwise strip trailing `:port` or `:*` on a rightmost-colon basis.
    let host = s.rsplit_once(':').map(|(h, _)| h).unwrap_or(s);
    Some(host.to_string())
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim_start_matches('[').trim_end_matches(']');
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..]; // ".example.org"
        return host == &pattern[2..] || host.ends_with(suffix);
    }
    pattern.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(rules: &[&str]) -> Allowlist {
        Allowlist::new(rules.iter().map(|s| s.to_string()).collect())
    }

    // ── SSRF regression ────────────────────────────────────────────────────

    /// Cloud metadata must never be reachable, in any spelling. These are the
    /// URLs a prompt-injected page would try to make the agent open.
    #[test]
    fn ssrf_metadata_endpoints_are_denied() {
        let a = Allowlist::default();
        for url in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://169.254.169.254/",
            "https://169.254.169.254/computeMetadata/v1/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata/computeMetadata/v1/",
            "http://metadata.goog/",
        ] {
            assert!(!a.allows(url), "metadata endpoint must be denied: {url}");
        }
    }

    /// IPv6 spellings of the same targets, including the IPv4-mapped form that
    /// defeats a naive v4-only string check.
    #[test]
    fn ssrf_ipv6_forms_are_denied() {
        let a = Allowlist::default();
        for url in [
            "http://[::ffff:169.254.169.254]/latest/meta-data/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::]/",
            "http://[2001:db8::1]/",
        ] {
            assert!(!a.allows(url), "IPv6 target must be denied: {url}");
        }
    }

    /// Private and link-local ranges are not reachable by default either.
    #[test]
    fn ssrf_private_ranges_are_denied() {
        let a = Allowlist::default();
        for url in [
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.1.1/",
            "http://0.0.0.0/",
        ] {
            assert!(!a.allows(url), "private range must be denied: {url}");
        }
    }

    /// `data:` is an exfiltration vector (a page can encode stolen content into
    /// a data URL) and is deliberately NOT in the defaults, despite the README
    /// historically claiming otherwise.
    #[test]
    fn ssrf_data_and_file_schemes_are_denied() {
        let a = Allowlist::default();
        for url in [
            "data:text/html,<script>fetch('http://evil')</script>",
            "data:text/plain;base64,c2VjcmV0",
            "file:///etc/passwd",
            "file:///home/user/.ssh/id_rsa",
        ] {
            assert!(!a.allows(url), "scheme must be denied: {url}");
        }
    }

    /// A redirect or JS pivot lands on a *different* origin; whatever it lands
    /// on is re-checked through this same predicate, so an allowed prefix must
    /// not authorise an attacker-controlled host that merely embeds it.
    #[test]
    fn ssrf_lookalike_hosts_are_denied() {
        let a = Allowlist::new(vec!["https://*.example.org".into()]);
        for url in [
            "https://example.org.evil.com/",
            "https://evil.com/?next=https://example.org",
            "https://notexample.org/",
            "http://example.org/",
        ] {
            assert!(!a.allows(url), "look-alike must be denied: {url}");
        }
        assert!(
            a.allows("https://app.example.org/x"),
            "genuine subdomain still allowed"
        );
    }

    #[test]
    fn localhost_with_any_port_is_allowed() {
        let al = a(&["http://localhost:*"]);
        assert!(al.allows("http://localhost:3000"));
        assert!(al.allows("http://localhost:8080/foo/bar"));
        assert!(!al.allows("http://google.com:3000"));
    }

    #[test]
    fn loopback_v6() {
        let al = a(&["http://[::1]:*"]);
        assert!(al.allows("http://[::1]:8080/health"));
    }

    #[test]
    fn subdomain_wildcard() {
        let al = a(&["https://*.example.org"]);
        assert!(al.allows("https://app.example.org/page"));
        assert!(al.allows("https://api.example.org"));
        assert!(al.allows("https://example.org")); // bare domain matches *.example.org rule
        assert!(!al.allows("https://x0ne.com"));
        assert!(!al.allows("http://app.example.org")); // scheme differs
    }

    #[test]
    fn data_and_about_schemes() {
        let al = a(&["data:*", "about:*"]);
        assert!(al.allows("data:text/html,<h1>hi</h1>"));
        assert!(al.allows("about:blank"));
        assert!(!al.allows("https://example.com"));
    }

    #[test]
    fn default_allowlist_has_useful_baseline() {
        let al = Allowlist::default();
        assert!(al.allows("http://localhost:3000/"));
        assert!(al.allows("about:blank"));
        assert!(!al.allows("https://app.example.org/foo"));
        assert!(!al.allows("https://evil.com/phish"));
    }

    #[test]
    fn unparseable_url_is_refused() {
        let al = Allowlist::default();
        assert!(!al.allows("not a url"));
    }
}
