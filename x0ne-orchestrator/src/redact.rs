//! Moved to `x0ne-audit` so the runtime journal masks with the same rules.
pub use x0ne_audit::redact::*;

/// A provider URL as it may be shown: its userinfo (`user:password@`, or a
/// lone `token@`) becomes `***@`; scheme, host, port and path stay. Every
/// header, error line and log line that names a model endpoint goes through
/// this.
///
/// The text is cut first, then read back by the URL parser of reqwest: a
/// userinfo the parser still finds is replaced on the parsed URL, and text
/// it cannot parse loses all before its last `@`. Review of the pre-release build,
/// 2026-10-25: ` http://u:p@h`, `http:/u:p@h`, `http:\\u:p@h` and a password
/// holding `/`, `#` or `?` were shown whole.
pub fn url_for_display(url: &str) -> String {
    let shown = cut_userinfo(url);
    let parsed = reqwest::Url::parse(url.trim());
    let Ok(mut parsed) = parsed else {
        return match url.rfind('@') {
            Some(at) if shown == url => format!("***{}", &url[at..]),
            _ => shown,
        };
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return match url.rfind('@') {
            // A scheme without authority (`user:pw@host`) reads no userinfo.
            Some(at) if parsed.cannot_be_a_base() && shown == url => format!("***{}", &url[at..]),
            _ => shown,
        };
    }
    let clean = reqwest::Url::parse(shown.trim())
        .is_ok_and(|u| u.password().is_none() && matches!(u.username(), "" | "***"));
    if clean {
        return shown;
    }
    let _ = parsed.set_password(None);
    if parsed.set_username("***").is_err() {
        return "***".to_string();
    }
    parsed.to_string()
}

/// `url` reduced to its origin, `scheme://host:port`, for output that leaves
/// the terminal: a path or a query (`?key=`) may hold a secret. Text the
/// URL parser cannot read keeps what precedes its first `/`, `?` or `#`.
pub fn url_origin(url: &str) -> String {
    let shown = url_for_display(url);
    if let Ok(parsed) = reqwest::Url::parse(shown.trim()) {
        let origin = parsed.origin();
        if origin.is_tuple() {
            return origin.ascii_serialization();
        }
    }
    let start = shown.find("://").map_or(0, |i| i + 3);
    let end = shown[start..]
        .find(['/', '?', '#', '\\'])
        .map_or(shown.len(), |i| start + i);
    shown[..end].to_string()
}

/// A backend's `display_name` with its endpoint cut to [`url_origin`]:
/// `openai-compat/m @ http://h:1/v1?key=…` becomes `openai-compat/m @ http://h:1`.
pub fn backend_name_for_output(display: &str) -> String {
    match display.split_once(" @ ") {
        Some((head, url)) => format!("{head} @ {}", url_origin(url)),
        None => display.to_string(),
    }
}

/// The userinfo of `url` as written, cut on the text alone.
fn cut_userinfo(url: &str) -> String {
    let start = match url.find("://") {
        Some(i)
            if i > 0
                && url[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            i + 3
        }
        _ => 0,
    };
    let rest = &url[start..];
    // `\` ends the authority too: the URL parser of reqwest reads it as `/`.
    let authority_end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}***{}", &url[..start], &rest[at..]),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::url_for_display;

    #[test]
    fn the_userinfo_is_masked_and_the_rest_is_kept() {
        for (url, shown) in [
            (
                "http://user:hunter2@127.0.0.1:8000/v1",
                "http://***@127.0.0.1:8000/v1",
            ),
            ("https://tok3n@host.example", "https://***@host.example"),
            ("http://a:b@c@host:1/p?q=1", "http://***@host:1/p?q=1"),
            ("http://u:p@host\\x", "http://***@host\\x"),
            ("user:hunter2@host:11434", "***@host:11434"),
            (
                "http://api.anthropic.com@127.0.0.1:9",
                "http://***@127.0.0.1:9",
            ),
            // What reqwest reads past the text cut: shown as parsed.
            (" http://user:hunter2@host:1/v1", "http://***@host:1/v1"),
            ("http:/user:hunter2@host:1", "http://***@host:1/"),
            ("http:\\\\user:hunter2@host:1", "http://***@host:1/"),
            ("http:user:hunter2@host", "http://***@host/"),
            // Not a URL: all before the last '@' goes.
            ("http://user:hunter2/x@host:1", "***@host:1"),
            ("http://user:hunter2#x@host:1", "***@host:1"),
            ("http://user:hunter2?x@host:1", "***@host:1"),
            ("user:hunter2/x@host", "***@host"),
        ] {
            assert_eq!(url_for_display(url), shown, "{url}");
        }
    }

    #[test]
    fn output_names_the_origin_only() {
        for (url, origin) in [
            (
                "http://127.0.0.1:8000/v1?key=s3cret",
                "http://127.0.0.1:8000",
            ),
            ("https://user:pw@host/api/tok/v1", "https://host"),
            ("https://host:443/v1#frag", "https://host"),
            ("localhost:11434/v1?key=s3cret", "localhost:11434"),
            ("http://user:hunter2/x@host:1?key=s3cret", "***@host:1"),
        ] {
            assert_eq!(super::url_origin(url), origin, "{url}");
        }
        assert_eq!(
            super::backend_name_for_output("openai-compat/m @ http://h:1/v1?key=s3cret"),
            "openai-compat/m @ http://h:1"
        );
        assert_eq!(super::backend_name_for_output("anthropic/m"), "anthropic/m");
    }

    #[test]
    fn a_url_without_userinfo_is_unchanged() {
        for url in [
            "http://127.0.0.1:8000/v1",
            "https://openrouter.ai/api/v1",
            "http://host/models/@latest",
            "http://host?next=a@b",
            "http://host#me@x",
            "https://evil.example\\@api.openai.com/v1",
            "localhost:11434",
            "",
        ] {
            assert_eq!(url_for_display(url), url, "{url}");
        }
    }
}
