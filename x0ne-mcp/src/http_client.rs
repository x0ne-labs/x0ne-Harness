//! Streamable-HTTP MCP client: JSON or SSE responses, session header kept.
//! Errors never carry header values or the URL (it may hold a token).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;

pub struct HttpMcpClient {
    url: String,
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    session: Option<String>,
    next_id: u64,
}

impl HttpMcpClient {
    pub fn new(url: &str, headers: &BTreeMap<String, String>) -> Result<Self> {
        Self::with_timeout(url, headers, Duration::from_secs(120))
    }

    pub fn with_timeout(
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| anyhow!("invalid header name `{k}`"))?;
            let mut value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|_| anyhow!("invalid value for header `{k}`"))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // 2026-10-06: mcp.exa.ai answered 403 to a `Python-urllib` agent, 200 to `x0ne/0.2`.
            .user_agent(concat!("x0ne/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            url: url.to_string(),
            http,
            headers: map,
            session: None,
            next_id: 1,
        })
    }

    async fn post(&mut self, body: &Value) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .headers(self.headers.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        if let Some(s) = &self.session {
            req = req.header("Mcp-Session-Id", s);
        }
        // `without_url`: a URL may carry a token in its query string.
        let resp = req
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("http request failed: {}", e.without_url()))?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session = Some(s.to_string());
        }
        Ok(resp)
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "x0ne", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let _ = self.post(&note).await?;
        Ok(())
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.post(&body).await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{method}: http status {status}");
        }
        let sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("{method}: reading the body failed: {}", e.without_url()))?;
        if bytes.len() > MAX_HTTP_BODY {
            bail!("{method}: response exceeds {MAX_HTTP_BODY} bytes");
        }
        let text = String::from_utf8_lossy(&bytes);
        let frames: Vec<Value> = if sse {
            sse_data(&text)
                .iter()
                .filter_map(|d| serde_json::from_str(d).ok())
                .collect()
        } else {
            vec![serde_json::from_str(&text).map_err(|_| anyhow!("{method}: body is not JSON"))?]
        };
        for f in frames {
            if f.get("method").is_some() || f.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(err) = f.get("error") {
                bail!(
                    "{method} failed: {} (code {})",
                    err.get("message").and_then(Value::as_str).unwrap_or("?"),
                    err.get("code").and_then(Value::as_i64).unwrap_or(0)
                );
            }
            return Ok(f.get("result").cloned().unwrap_or(Value::Null));
        }
        bail!("{method}: no response with id {id}")
    }
}

/// The `data:` payloads of an SSE body, one per event.
pub fn sse_data(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if !cur.is_empty() {
                out.push(cur.join("\n"));
                cur.clear();
            }
        } else if let Some(d) = line.strip_prefix("data:") {
            cur.push(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    if !cur.is_empty() {
        out.push(cur.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_are_split_and_joined() {
        let body = "event: message\ndata: {\"a\":1}\n\ndata: {\"b\":\ndata: 2}\n\n";
        assert_eq!(
            sse_data(body),
            vec!["{\"a\":1}".to_string(), "{\"b\":\n2}".to_string()]
        );
    }
}
