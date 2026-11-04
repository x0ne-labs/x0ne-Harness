//! Which vendor key may reach which host. [`KEYS`](crate::model_keys::KEYS)
//! is the one table, read by the session's own model calls
//! (`x0ne-orchestrator`, `model_keys`) and by the `auth` of the
//! runtime's `LlmCall`, `OllamaChat` and `OllamaApi`. Adding a vendor
//! is adding a row.
//!
//! - A key read from a row's variable without anyone naming it for this
//!   request goes through the row's provider or, over https, to its hosts.
//!   Anywhere else it is refused before any request.
//! - A key that holds a pinned row's prefix anywhere goes through that row's
//!   provider only, named or not.
//! - A row's provider and hosts take no key of another format.
//!
//! Lives here, in the pure vocabulary crate, for the reason [`crate::env_keys`]
//! does: the orchestrator does not link the runtime, and the two must give
//! the same answer.

use url::{Host, Url};

/// A vendor: the variable its key is read from, where that key may go, and
/// what its keys look like.
#[derive(Debug)]
pub struct KeyRow {
    /// Read for a key when the config names none.
    pub var: &'static str,
    pub vendor: &'static str,
    /// The vendor's hosts, reached over https.
    pub hosts: &'static [&'static str],
    /// The session provider whose endpoint is fixed to `hosts`.
    pub provider: Option<&'static str>,
    /// Every key of this vendor starts so ...
    pub prefix: &'static str,
    /// ... and none starts so.
    pub except: &'static [&'static str],
    /// No other vendor's key looks like this vendor's: one goes through
    /// `provider` only, even named.
    pub pinned: bool,
}

impl KeyRow {
    /// "an Anthropic key", "a Groq key".
    pub fn a_key(&self) -> String {
        let vowel = self.vendor.starts_with(['A', 'E', 'I', 'O', 'U']);
        format!("{} {} key", if vowel { "an" } else { "a" }, self.vendor)
    }

    /// Does `key` hold this vendor's prefix anywhere? A pinned row tests
    /// this: on the pre-release build, `Bearer sk-ant-…` and a leading U+FEFF or U+200B
    /// passed `takes` and reached a compat host (pre-release review).
    pub fn carries(&self, key: &str) -> bool {
        !self.prefix.is_empty() && key.contains(self.prefix)
    }

    /// Does `key` look like this vendor's?
    pub fn takes(&self, key: &str) -> bool {
        let k = key.trim();
        k.len() > self.prefix.len()
            && k.starts_with(self.prefix)
            && !self.except.iter().any(|e| k.starts_with(e))
    }
}

pub const KEYS: &[KeyRow] = &[
    KeyRow {
        var: "ANTHROPIC_API_KEY",
        vendor: "Anthropic",
        hosts: &["api.anthropic.com"],
        provider: Some("anthropic"),
        prefix: "sk-ant-",
        except: &[],
        pinned: true,
    },
    KeyRow {
        var: "OPENAI_API_KEY",
        vendor: "OpenAI",
        hosts: &["api.openai.com"],
        provider: Some("openai"),
        prefix: "sk-",
        except: &["sk-ant-", "sk-or-"],
        pinned: false,
    },
    // docs.ollama.com/cloud (read 2026-10-25): OLLAMA_API_KEY, https://ollama.com/api.
    KeyRow {
        var: "OLLAMA_API_KEY",
        vendor: "Ollama",
        hosts: &["ollama.com"],
        provider: None,
        prefix: "",
        except: &[],
        pinned: false,
    },
];

/// The row of `var`, if it is a vendor's variable.
pub fn row(var: &str) -> Option<&'static KeyRow> {
    KEYS.iter().find(|r| r.var == var)
}

/// Why a key may not go where it was headed, and the row that says so.
#[derive(Debug, Clone, Copy)]
pub enum Refusal {
    /// The key holds a pinned row's prefix, and the request does not go
    /// through that row's provider.
    Pinned(&'static KeyRow),
    /// The request goes through a row's provider or to one of its hosts,
    /// and the key has another format.
    Format(&'static KeyRow),
    /// The key was read from a row's variable that nobody named, and the
    /// request goes neither through the row's provider nor to its hosts over
    /// https.
    Implicit(&'static KeyRow),
}

/// The host `url` connects to, lowercased, and whether it is https: the URL
/// parser of reqwest reads it, not the text.
pub fn host_of(url: &str) -> Option<(String, bool)> {
    let u = Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some((host, u.scheme() == "https"))
}

/// The provider whose fixed endpoint `url` is: the row whose hosts serve it
/// over https. `""` when none; a runtime action has no provider of its own.
pub fn provider_at(url: &str) -> &'static str {
    match host_of(url) {
        Some((host, true)) => KEYS
            .iter()
            .find(|r| r.hosts.contains(&host.as_str()))
            .and_then(|r| r.provider)
            .unwrap_or(""),
        _ => "",
    }
}

/// Why `key` may not be sent to `url` through `provider` (`""`: none), or
/// `None` when it may. `implicit`: the variable it was read from, when
/// nobody named that variable for this request. Called before any request.
pub fn refusal(key: &str, implicit: Option<&str>, provider: &str, url: &str) -> Option<Refusal> {
    let (host, tls) = host_of(url).unwrap_or_default();
    let through = |r: &KeyRow| !provider.is_empty() && r.provider == Some(provider);
    let at = |r: &KeyRow| r.hosts.contains(&host.as_str());
    let k = key.trim();
    for r in KEYS {
        if r.pinned && r.carries(k) && !through(r) {
            return Some(Refusal::Pinned(r));
        }
        if (through(r) || at(r)) && !r.takes(k) {
            return Some(Refusal::Format(r));
        }
    }
    let r = row(implicit?)?;
    (!through(r) && !(tls && at(r))).then_some(Refusal::Implicit(r))
}

/// `…` and the last four characters of `key`; `…` alone for a key of eight
/// characters or fewer. All a refusal or a log line may show of a key.
pub fn mask(key: &str) -> String {
    let k = key.trim();
    let n = k.chars().count();
    if n <= 8 {
        return "…".into();
    }
    let tail: String = k.chars().skip(n - 4).collect();
    format!("…{tail}")
}

/// Does `url` name a loopback host (`localhost`, 127.0.0.0/8, `::1`)? A
/// request to one has no reason to leave the machine, through a proxy or
/// otherwise (review of the pre-release build: `HTTP_PROXY` saw a loopback model's key).
pub fn is_loopback(url: &str) -> bool {
    let Ok(u) = Url::parse(url.trim()) else {
        return false;
    };
    match u.host() {
        Some(Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.to_canonical().is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANT: &str = "sk-ant-api03-FAKE-abcd";

    #[test]
    fn a_pinned_key_goes_to_its_provider_or_its_https_host_only() {
        let home = "https://api.anthropic.com";
        assert!(refusal(ANT, None, provider_at(home), home).is_none());
        assert!(refusal(ANT, None, "anthropic", "").is_none());
        for key in [
            ANT.to_string(),
            format!("Bearer {ANT}"),
            format!("\u{feff}{ANT}"),
        ] {
            // Decorated, it reaches home in a format Anthropic does not take.
            if key != ANT {
                assert!(matches!(
                    refusal(&key, None, provider_at(home), home),
                    Some(Refusal::Format(_))
                ));
            }
            for (p, url) in [
                ("", "http://api.anthropic.com"),
                ("", "https://127.0.0.1:4000"),
                ("", "https://api.anthropic.com.evil.example"),
                ("openai-compat", "https://openrouter.ai/api"),
            ] {
                assert!(
                    matches!(refusal(&key, None, p, url), Some(Refusal::Pinned(_))),
                    "{key:?} {url}"
                );
            }
        }
    }

    #[test]
    fn a_vendor_host_takes_its_own_format_only() {
        let at = |key: &str, url: &str| refusal(key, None, provider_at(url), url);
        assert!(at("sk-proj-abcd", "https://api.openai.com/v1").is_none());
        assert!(matches!(
            at("gsk_abcd", "https://api.openai.com/v1"),
            Some(Refusal::Format(_))
        ));
        assert!(matches!(
            at("sk-proj-abcd", "https://api.anthropic.com"),
            Some(Refusal::Format(_))
        ));
        // A third-party host keeps its own formats.
        assert!(at("gsk_abcd", "https://api.groq.com/openai").is_none());
    }

    #[test]
    fn an_unnamed_variable_goes_home_over_https_only() {
        for r in KEYS {
            let key = format!("{}FAKE-1014-wxyz", r.prefix);
            let home = format!("https://{}", r.hosts[0]);
            let p = provider_at(&home);
            assert!(refusal(&key, Some(r.var), p, &home).is_none(), "{}", r.var);
            for away in [
                format!("http://{}", r.hosts[0]),
                "http://127.0.0.1:11434".to_string(),
                format!("https://{}.evil.example", r.hosts[0]),
                format!("https://evil.example\\@{}", r.hosts[0]),
            ] {
                assert!(
                    refusal(&key, Some(r.var), provider_at(&away), &away).is_some(),
                    "{} to {away}",
                    r.var
                );
            }
            // Named for this request: no implicit rule.
            if !r.pinned {
                assert!(refusal(&key, None, "", "http://127.0.0.1:11434").is_none());
            }
        }
    }

    #[test]
    fn the_mask_keeps_four_characters_at_most() {
        assert_eq!(mask(ANT), "…abcd");
        assert_eq!(mask("short"), "…");
        assert!(!mask(ANT).contains("FAKE"));
    }

    #[test]
    fn loopback_is_read_from_the_parsed_host() {
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:4000/v1",
            "http://LOCALHOST./v1",
            "http://[::1]:8080",
            "http://[::ffff:127.0.0.1]:8080",
            "http://127.1.2.3",
            " http://user:pw@127.0.0.1:1",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in [
            "https://api.openai.com",
            "http://localhost.evil.example",
            "http://evil.example\\@127.0.0.1",
            "http://127.0.0.1.evil.example",
            "localhost:11434",
            "",
        ] {
            assert!(!is_loopback(url), "{url}");
        }
    }
}
