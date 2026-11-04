//! WebSearch through Exa's public MCP endpoint, the backend fallback chain,
//! FetchUrl's 15-minute cache, and citation ids for every URL seen.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// No key is sent: the endpoint answered without one on 2026-10-06.
pub(crate) const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const EXA_TIMEOUT: Duration = Duration::from_secs(25);
const EXA_CONTEXT_CHARS: usize = 10_000;

/// One `web_search_exa` call over streamable HTTP, parsed to `{title, url, snippet}`.
pub(crate) async fn exa_search(
    endpoint: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let mut client = x0ne_mcp::HttpMcpClient::with_timeout(endpoint, &BTreeMap::new(), EXA_TIMEOUT)
        .map_err(|e| format!("exa: {e}"))?;
    // Stateless endpoint: one tools/call, no initialize round-trip (measured 2026-10-06).
    let result = client
        .call(
            "tools/call",
            json!({"name": "web_search_exa", "arguments": {
                "query": query,
                "numResults": limit,
                "type": "auto",
                "livecrawl": "fallback",
                "contextMaxCharacters": EXA_CONTEXT_CHARS,
            }}),
        )
        .await
        .map_err(|e| format!("exa: {e}"))?;
    let text = result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n---\n")
        })
        .unwrap_or_default();
    if result["isError"].as_bool() == Some(true) {
        let head: String = text.chars().take(200).collect();
        return Err(format!("exa: tool error: {head}"));
    }
    parse_exa_text(&text, limit)
}

/// Exa answers one text block per hit: `Title:`, `URL:`, `Published:`,
/// then `Highlights:` or `Text:`, blocks separated by `---`.
pub(crate) fn parse_exa_text(text: &str, limit: usize) -> Result<Vec<Value>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for block in text.split("\n---\n") {
        let (head, body) = ["\nHighlights:\n", "\nText:"]
            .iter()
            .find_map(|m| block.find(m).map(|i| (&block[..i], &block[i + m.len()..])))
            .unwrap_or((block, ""));
        let field = |name: &str| {
            head.lines()
                .find_map(|l| l.trim().strip_prefix(name))
                .map(str::trim)
                .filter(|v| !v.is_empty() && *v != "N/A")
        };
        let Some(url) = field("URL:").filter(|u| u.starts_with("http")) else {
            continue;
        };
        let mut hit = json!({
            "title": field("Title:").unwrap_or(url),
            "url": url,
            "snippet": body.trim(),
        });
        if let Some(p) = field("Published:") {
            hit["published"] = json!(p);
        }
        out.push(hit);
        if out.len() >= limit {
            break;
        }
    }
    if out.is_empty() {
        return Err(format!("exa: no result in a {}-byte answer", text.len()));
    }
    Ok(out)
}

pub(crate) type BackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Value>, String>> + Send + 'a>>;

/// The first backend that answered, and every one that failed before it.
#[derive(Debug)]
pub(crate) struct Searched {
    pub source: &'static str,
    pub results: Vec<Value>,
    pub failures: Vec<(&'static str, String)>,
}

/// Await backends in order; the first `Ok` wins. Futures never awaited never run.
pub(crate) async fn first_answer(
    backends: Vec<(&'static str, BackendFuture<'_>)>,
) -> Result<Searched, Vec<(&'static str, String)>> {
    let mut failures = Vec::new();
    for (name, fut) in backends {
        match fut.await {
            Ok(results) => {
                return Ok(Searched {
                    source: name,
                    results,
                    failures,
                })
            }
            Err(e) => failures.push((name, e)),
        }
    }
    Err(failures)
}

/// Error text when every backend failed; names each cause.
pub(crate) fn all_failed(failures: &[(&'static str, String)]) -> String {
    let causes: Vec<String> = failures.iter().map(|(b, e)| format!("{b}: {e}")).collect();
    format!("every search backend failed · {}", causes.join(" · "))
}

// ── Citation ids ─────────────────────────────────────────────────────────────

const MAX_CITED: usize = 10_000;
static CITED: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Stable `S<n>` id for `url` in this runtime process; `None` once the registry is full.
pub(crate) fn cite(url: &str) -> Option<String> {
    let mut cited = CITED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = cited.iter().position(|u| u == url) {
        return Some(format!("S{}", i + 1));
    }
    if cited.len() >= MAX_CITED {
        return None;
    }
    cited.push(url.to_string());
    Some(format!("S{}", cited.len()))
}

// ── FetchUrl cache ───────────────────────────────────────────────────────────

pub(crate) const CACHE_TTL: Duration = Duration::from_mins(15);
const CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Cached {
    pub status: u16,
    pub text: String,
    pub at: Instant,
}

/// Converted bodies by `format + url`, bounded in age and in total bytes.
#[derive(Default)]
pub(crate) struct FetchCache {
    entries: HashMap<String, Cached>,
    bytes: usize,
}

impl FetchCache {
    pub fn get(&mut self, key: &str, now: Instant) -> Option<Cached> {
        let fresh = self
            .entries
            .get(key)
            .map(|c| now.saturating_duration_since(c.at) <= CACHE_TTL)?;
        if fresh {
            return self.entries.get(key).cloned();
        }
        self.remove(key);
        None
    }

    pub fn put(&mut self, key: String, status: u16, text: String, now: Instant) {
        if text.len() > CACHE_BYTES {
            return;
        }
        self.remove(&key);
        self.entries
            .retain(|_, c| now.saturating_duration_since(c.at) <= CACHE_TTL);
        self.bytes = self.entries.values().map(|c| c.text.len()).sum();
        while self.bytes + text.len() > CACHE_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, c)| c.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.bytes += text.len();
        self.entries.insert(
            key,
            Cached {
                status,
                text,
                at: now,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(c) = self.entries.remove(key) {
            self.bytes -= c.text.len();
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

pub(crate) static FETCH_CACHE: LazyLock<Mutex<FetchCache>> =
    LazyLock::new(|| Mutex::new(FetchCache::default()));

pub(crate) fn cache_key(url: &str, format: &str) -> String {
    format!("{format} {url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXA_SSE: &str = include_str!("../../tests/fixtures/web/exa_response.sse");

    fn exa_text() -> String {
        let data = x0ne_mcp::sse_data(EXA_SSE);
        let v: Value = serde_json::from_str(&data[0]).unwrap();
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn exa_answer_parses_to_results() {
        let r = parse_exa_text(&exa_text(), 8).expect("parsed");
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!(r[0]["url"], "https://tokio.rs/");
        assert_eq!(r[0]["title"], "Tokio - An asynchronous Rust runtime");
        assert!(r[0]["snippet"]
            .as_str()
            .unwrap()
            .starts_with("Tokio - An asynchronous Rust runtime"));
        assert!(!r[0]["snippet"].as_str().unwrap().contains("URL:"));
        assert_eq!(parse_exa_text(&exa_text(), 1).unwrap().len(), 1);
    }

    #[test]
    fn exa_empty_answer_is_zero_results_and_garbage_is_an_error() {
        assert!(parse_exa_text("  ", 8).unwrap().is_empty());
        assert!(parse_exa_text("Rate limit exceeded", 8).is_err());
    }

    /// Fake MCP endpoint on loopback; serves the captured SSE answer.
    async fn fake_exa(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<Mutex<Vec<Value>>>) {
        use axum::{http::StatusCode, routing::post, Json, Router};
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let app = Router::new().route(
            "/mcp",
            post(move |Json(req): Json<Value>| {
                let log = log.clone();
                async move {
                    log.lock().unwrap().push(req);
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn exa_search_speaks_mcp_and_sends_no_key() {
        let (url, seen) = fake_exa(200, EXA_SSE).await;
        let r = exa_search(&url, "tokio rust async runtime", 8)
            .await
            .expect("answer");
        assert_eq!(r.len(), 3);
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1, "no initialize round-trip: {reqs:?}");
        assert_eq!(reqs[0]["method"], "tools/call");
        assert_eq!(reqs[0]["params"]["name"], "web_search_exa");
        let args = &reqs[0]["params"]["arguments"];
        assert_eq!(args["query"], "tokio rust async runtime");
        assert_eq!(args["numResults"], 8);
        assert_eq!(args["livecrawl"], "fallback");
        assert!(!reqs[0].to_string().to_lowercase().contains("key"));
    }

    #[tokio::test]
    async fn exa_http_error_is_an_error() {
        let (url, _) = fake_exa(429, "").await;
        let e = exa_search(&url, "q", 8).await.unwrap_err();
        assert!(e.contains("exa") && e.contains("429"), "{e}");
    }

    fn ok(r: Vec<Value>) -> BackendFuture<'static> {
        Box::pin(async move { Ok(r) })
    }
    fn fail(e: &'static str) -> BackendFuture<'static> {
        Box::pin(async move { Err(e.to_string()) })
    }

    #[tokio::test]
    async fn first_answer_falls_through_and_names_each_failure() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let never: BackendFuture<'static> = Box::pin(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![])
        });
        let s = first_answer(vec![
            ("exa", fail("exa: http status 503")),
            ("brave", ok(vec![json!({"url": "https://a"})])),
            ("ddg", never),
        ])
        .await
        .expect("brave answers");
        assert_eq!(s.source, "brave");
        assert_eq!(s.results.len(), 1);
        assert_eq!(
            s.failures,
            vec![("exa", "exa: http status 503".to_string())]
        );
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "ddg must not run"
        );
    }

    #[tokio::test]
    async fn every_backend_failing_keeps_every_cause() {
        let f = first_answer(vec![
            ("exa", fail("timeout")),
            ("brave", fail("HTTP 429")),
            ("ddg", fail("DuckDuckGo blocked the request")),
        ])
        .await
        .unwrap_err();
        let msg = all_failed(&f);
        for s in ["exa: timeout", "brave: HTTP 429", "ddg: DuckDuckGo blocked"] {
            assert!(msg.contains(s), "{s} missing from {msg}");
        }
    }

    #[test]
    fn citation_ids_are_stable_per_url() {
        let a = cite("https://cite.test/a").unwrap();
        let b = cite("https://cite.test/b").unwrap();
        assert_ne!(a, b);
        assert_eq!(cite("https://cite.test/a").unwrap(), a);
        assert!(a.starts_with('S'));
    }

    #[test]
    fn cache_expires_after_fifteen_minutes() {
        let mut c = FetchCache::default();
        let t0 = Instant::now();
        c.put("k".into(), 200, "body".into(), t0);
        assert_eq!(
            c.get("k", t0 + Duration::from_secs(60)).unwrap().text,
            "body"
        );
        assert!(c
            .get("k", t0 + CACHE_TTL + Duration::from_secs(1))
            .is_none());
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn cache_stays_under_its_byte_cap() {
        let mut c = FetchCache::default();
        let t0 = Instant::now();
        let big = "x".repeat(CACHE_BYTES / 3);
        for i in 0..5 {
            c.put(
                format!("k{i}"),
                200,
                big.clone(),
                t0 + Duration::from_secs(i),
            );
        }
        assert!(c.bytes() <= CACHE_BYTES, "{}", c.bytes());
        assert!(c.get("k0", t0).is_none(), "oldest evicted first");
        assert!(c.get("k4", t0).is_some());
        c.put("huge".into(), 200, "x".repeat(CACHE_BYTES + 1), t0);
        assert!(c.get("huge", t0).is_none());
    }
}
