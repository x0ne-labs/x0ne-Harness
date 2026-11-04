use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, warn};

use crate::conversation::{Message, Role};
use crate::redact::url_for_display;

// ── API response with token usage ─────────────────────────────────────────────

/// Structured response from any LLM backend, including token counters.
///
/// `cache_creation_input_tokens` and `cache_read_input_tokens` are Anthropic-only
/// (Ollama has no server-side prompt cache). Other backends leave them at 0.
#[derive(Debug, Clone, Default)]
pub struct ApiResponse {
    pub content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens *written* to the prompt cache on this request.
    /// Anthropic bills these at 1.25× the standard input rate.
    pub cache_creation_input_tokens: u64,
    /// Tokens *read* from the prompt cache (cache hits).
    /// Anthropic bills these at 0.10× the standard input rate — the whole point.
    pub cache_read_input_tokens: u64,
    #[allow(dead_code)] // reserved for stop/retry logic
    pub stop_reason: String,
}

// ── Usage tracker ─────────────────────────────────────────────────────────────

/// Accumulates token usage across the session for cost estimation.
#[derive(Debug, Default, Clone)]
pub struct UsageTracker {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Cumulative tokens written to cache (billed at 1.25× input).
    pub total_cache_write_tokens: u64,
    /// Cumulative tokens served from cache (billed at 0.10× input).
    pub total_cache_read_tokens: u64,
    pub total_requests: u64,
    pub model: String,
}

impl UsageTracker {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    pub fn record(&mut self, resp: &ApiResponse) {
        self.total_input_tokens += resp.input_tokens;
        self.total_output_tokens += resp.output_tokens;
        self.total_cache_write_tokens += resp.cache_creation_input_tokens;
        self.total_cache_read_tokens += resp.cache_read_input_tokens;
        self.total_requests += 1;
    }

    /// Total tokens consumed this session, cache included.
    ///
    /// Anthropic reports cached input under `cache_creation_input_tokens` /
    /// `cache_read_input_tokens`, NOT under `input_tokens`. Summing only the
    /// latter two made this function contradict its own name: with prompt
    /// caching on, nearly all input is cached, so the session counter showed a
    /// tiny fraction of real traffic. `cost()` was unaffected — it prices the
    /// cache fields explicitly — so this understated what was shown, never
    /// what was charged.
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens
            + self.total_output_tokens
            + self.total_cache_write_tokens
            + self.total_cache_read_tokens
    }

    /// Cache hit ratio over the session, in [0.0, 1.0].
    /// Returns 0 if no cacheable input has been seen yet.
    pub fn cache_hit_ratio(&self) -> f64 {
        let denom = self.total_input_tokens + self.total_cache_read_tokens;
        if denom == 0 {
            return 0.0;
        }
        self.total_cache_read_tokens as f64 / denom as f64
    }

    /// Estimated USD cost based on Sonnet pricing (or $0 for Ollama).
    ///
    /// Anthropic cache pricing: writes cost 1.25× standard input, reads cost 0.10×.
    /// `total_input_tokens` excludes cache traffic (Anthropic reports them in
    /// separate fields), so we sum all three buckets explicitly.
    pub fn estimated_cost(&self, input_per_m: f64, output_per_m: f64) -> f64 {
        let per_m = |t: u64| (t as f64) / 1_000_000.0;
        let input_cost = per_m(self.total_input_tokens) * input_per_m;
        let write_cost = per_m(self.total_cache_write_tokens) * input_per_m * 1.25;
        let read_cost = per_m(self.total_cache_read_tokens) * input_per_m * 0.10;
        let output_cost = per_m(self.total_output_tokens) * output_per_m;
        input_cost + write_cost + read_cost + output_cost
    }

    fn is_local(&self) -> bool {
        self.model.starts_with("ollama/")
            || self.model.contains("localhost")
            || self.model.contains("127.0.0.1")
            || self.model.contains("192.168.")
            || self.model.contains("10.0.")
    }

    /// What `/cost` prints: [`Self::summary`] at `self.model`'s rate in
    /// `prices`, or "unknown" when it has none.
    pub fn summary_with(&self, prices: &dyn crate::agent::pricing::PriceList) -> String {
        let cost_str = match prices.rate_for(&self.model) {
            _ if self.is_local() => "$0.00 (local)".to_string(),
            None => "unknown (no price for this model)".to_string(),
            Some(rate) => {
                let per_m = |t: u64| (t as f64) / 1_000_000.0;
                let input = rate.input_per_m;
                let cost = per_m(self.total_input_tokens) * input
                    + per_m(self.total_cache_write_tokens)
                        * input
                        * rate.cache_write_multiplier.unwrap_or(1.0)
                    + per_m(self.total_cache_read_tokens)
                        * input
                        * rate.cache_read_multiplier.unwrap_or(1.0)
                    + per_m(self.total_output_tokens) * rate.output_per_m;
                // Session totals cannot tell a long prompt from a short one.
                let tier = if rate.long_prompt.is_some() {
                    " (short-prompt rate; a longer prompt costs more)"
                } else {
                    ""
                };
                format!("${cost:.4}{tier}")
            }
        };
        self.render(cost_str)
    }

    pub fn summary(&self, input_per_m: f64, output_per_m: f64) -> String {
        let cost_str = if self.is_local() {
            "$0.00 (local)".to_string()
        } else {
            format!("${:.4}", self.estimated_cost(input_per_m, output_per_m))
        };
        self.render(cost_str)
    }

    fn render(&self, cost_str: String) -> String {
        // Cache segment only when there's something to report (keeps local / Ollama
        // output clean since those backends never touch Anthropic's prompt cache).
        let cache_str = if self.total_cache_read_tokens + self.total_cache_write_tokens > 0 {
            format!(
                "  |  Cache: {} read / {} write ({:.0}% hit)",
                self.total_cache_read_tokens,
                self.total_cache_write_tokens,
                self.cache_hit_ratio() * 100.0,
            )
        } else {
            String::new()
        };

        format!(
            "Requests: {}  |  Input: {} tok  |  Output: {} tok{}  |  Cost: {}  |  Model: {}",
            self.total_requests,
            self.total_input_tokens,
            self.total_output_tokens,
            cache_str,
            cost_str,
            self.model,
        )
    }
}

// ── Backend trait ─────────────────────────────────────────────────────────────

/// One delta from a streaming LLM response.
///
/// Splitting visible content from internal reasoning lets thinking-mode
/// models (Anthropic extended thinking, Ollama `message.thinking`,
/// OpenAI-compat `delta.reasoning_content`, GLM-5 / DeepSeek-R1) surface a
/// "🧠 reflexion…" indicator in the UI instead of appearing frozen for
/// minutes while the model thinks. Consumers that only care about the
/// answer body can pattern-match `Token` and ignore `Thinking`.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Visible content — answer text, action JSON, code, etc. The same
    /// bytes that go into `ApiResponse.content`.
    Token(String),
    /// Internal reasoning emitted by thinking-mode models. Not part of
    /// the message body. Display layers may render an indicator (elapsed
    /// time, char count) rather than the raw text.
    Thinking(String),
}

impl StreamChunk {
    /// Borrow the inner text regardless of variant. Useful when a consumer
    /// only needs to count chars / bytes (e.g. `LiveTokenCounter`).
    pub fn text(&self) -> &str {
        match self {
            StreamChunk::Token(s) | StreamChunk::Thinking(s) => s,
        }
    }
}

/// Common interface for all LLM backends.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Send the conversation and return a structured response with token usage.
    async fn send(&self, system: &str, messages: &[Message]) -> Result<ApiResponse>;

    /// Stream typed chunks via an unbounded channel as they arrive.
    ///
    /// Each backend categorises every delta as `Token` (visible content) or
    /// `Thinking` (reasoning). Returns the complete `ApiResponse` (with full
    /// visible content and token counts) when done. Default implementation:
    /// calls `send()` and sends the full text as one Token chunk.
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        let resp = self.send(system, messages).await?;
        let _ = tx.send(StreamChunk::Token(resp.content.clone()));
        Ok(resp)
    }

    /// `stream`, with the action vocabulary offered as native tools.
    /// Backends without native tool calling ignore `tools`.
    async fn stream_with_tools(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        let _ = tools;
        self.stream(system, messages, tx).await
    }

    /// Human-readable name for display.
    fn display_name(&self) -> String;

    /// Which failure classes this backend already retries internally.
    ///
    /// Declared so the agent layer does not retry the same condition a second
    /// time. Anthropic's `stream()` spends up to `MAX_RETRIES` attempts on
    /// 429/5xx with `Retry-After`; an agent-level retry on top would multiply
    /// that into fifteen network requests for one rate limit. The default is
    /// `None`, which is correct for OpenAI-compat streaming, Ollama and the
    /// subprocess backends — none of them retries anything.
    fn internal_retry(&self) -> InternalRetry {
        InternalRetry::None
    }
}

/// What a backend already handles on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalRetry {
    /// Retries nothing; the agent layer owns every retry.
    None,
    /// Retries HTTP rate limits and server errors, honouring `Retry-After`.
    /// The agent layer must not retry those classes for this backend.
    RateLimitAndServerErrors,
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// A client builder for requests to `url`. A loopback endpoint ignores
/// `HTTP_PROXY` and its kin: its prompt and key go to no proxy. Review of
/// the pre-release build (S2): the proxy saw a loopback model's Bearer.
pub fn client_for(url: &str) -> reqwest::ClientBuilder {
    let builder = Client::builder();
    if x0ne_actions::model_keys::is_loopback(url) {
        builder.no_proxy()
    } else {
        builder
    }
}

pub fn create_backend(
    config: &crate::config::LlmConfig,
    anthropic_key: Option<String>,
) -> Result<Box<dyn LlmBackend>> {
    match config.provider.as_str() {
        "ollama" => {
            let endpoint = config.ollama.endpoint.clone();
            let api_key = crate::model_keys::resolve(config, None)?;
            let ollama = &config.ollama;
            if ollama.num_ctx <= ollama.max_tokens {
                return Err(anyhow!(
                    "llm.ollama.num_ctx ({}) must exceed llm.ollama.max_tokens ({}): \
                     the window holds the prompt and the answer",
                    ollama.num_ctx,
                    ollama.max_tokens
                ));
            }
            let backend = OllamaClient::new(&endpoint, &ollama.model, ollama.max_tokens, api_key)?
                .with_num_ctx(ollama.num_ctx);
            Ok(Box::new(backend))
        }
        // ── Native OpenAI (api.openai.com) ───────────────────────────────
        // Reuses OpenAICompatClient — the protocol is identical, only the
        // base_url and key resolution differ.
        "openai" => {
            let key = crate::model_keys::resolve(config, None)?.ok_or_else(|| {
                anyhow!(
                    "OpenAI API key not found. Set {} (e.g. export OPENAI_API_KEY=sk-...).",
                    config.openai.key_var(),
                )
            })?;
            let backend = OpenAICompatClient::new(
                "https://api.openai.com",
                &config.openai.model,
                config.openai.max_tokens,
                Some(key),
            )?;
            Ok(Box::new(backend))
        }
        "openai-compat" | "litellm" | "openai_compat" => {
            let key = crate::model_keys::resolve(config, None)?;
            let backend = OpenAICompatClient::new(
                &config.openai_compat.base_url,
                &config.openai_compat.model,
                config.openai_compat.max_tokens,
                key,
            )?;
            Ok(Box::new(backend))
        }
        // The confidential relay, gated before every call
        // (`engine_local`). It never reaches the Anthropic arm below.
        p if crate::engine_local::is_engine_provider(p) => Ok(Box::new(
            crate::engine_local::EngineBackend::from_config(&config.x0ne_engine)?,
        )),
        // Removed backends. `claude-code`/`codex` shelled out to a full
        // agent CLI and pasted X0NE's system prompt into the user text.
        // `claude-max`/`anthropic-oauth` reused Claude Code's subscription
        // token — but Anthropic gates that token behind Claude Code's own
        // identity (the request must carry its system prompt), so serving
        // X0NE's prompt through it means impersonating Claude Code. X0NE
        // does not. Measured 2026-08-10: identical token, the only thing
        // that flips 429→200 is prepending "You are Claude Code…".
        "claude-code" | "codex" | "claude-max" | "anthropic-oauth" => anyhow::bail!(
            "the '{}' backend was removed. Use provider \"anthropic\" \
             (set ANTHROPIC_API_KEY, pay-per-token), \"ollama\" (local), \
             or \"openai-compat\". Run `/setup` for a guided panel.",
            config.provider
        ),
        _ => {
            // Anthropic API key (`provider = "anthropic"` or unrecognised):
            // `anthropic_key` (wizard input), then ANTHROPIC_API_KEY, then
            // `api_key_env` ([`crate::model_keys`]).
            if let Some(key) = crate::model_keys::resolve(config, anthropic_key)? {
                let backend = AnthropicClient::new(
                    AnthropicAuth::ApiKey(key),
                    &config.anthropic.model,
                    config.anthropic.max_tokens,
                )?;
                return Ok(Box::new(backend));
            }
            Err(anyhow!(
                "Anthropic API key not found. Set ANTHROPIC_API_KEY (a \
                 pay-per-token API account from console.anthropic.com), or \
                 run `/setup`. For local/free inference use provider \
                 \"ollama\".",
            ))
        }
    }
}

// ── Shared constants ──────────────────────────────────────────────────────────

const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 1_000;

/// How many attempts to spend on a 429 that carries no `retry-after`.
///
/// Without the header there is nothing to honour: a subscription rate limit
/// does not clear inside an exponential ramp, so the remaining attempts are
/// guaranteed to fail while the user waits ~31s in silence. One short probe
/// still catches a genuine burst spike; past that, fail fast with an
/// actionable message.
const OPAQUE_429_ATTEMPTS: u32 = 2;

/// Longest server-requested wait we will actually sleep through. Beyond this,
/// retrying in-process is worse than telling the user how long the window is —
/// they can switch provider or come back later.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Parse a `retry-after` header into a concrete delay.
///
/// The server knows when its rate-limit window resets; we do not. Blind
/// exponential backoff (1s, 2s, 4s, …) burns the whole retry budget inside a
/// window that is typically minutes long, so every attempt is guaranteed to
/// fail — and hammering an already-limited endpoint can extend the window.
/// Honour the header when it is present.
///
/// Only the `delay-seconds` form is handled (what Anthropic sends). The
/// RFC-7231 HTTP-date form returns `None`, which falls back to exponential
/// backoff — correct, just less precise.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get("retry-after")?.to_str().ok()?;
    let secs: f64 = raw.trim().parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

// ── Anthropic client ─────────────────────────────────────────────────────────
//
// One credential path: `ApiKey` — a pay-per-token API account
// (`sk-ant-api…`), sent as `x-api-key`. What `ANTHROPIC_API_KEY` resolves
// to. The subscription-OAuth path was removed 2026-08-10 (it required
// impersonating Claude Code to Anthropic's servers; see create_backend).

#[derive(Clone)]
pub enum AnthropicAuth {
    ApiKey(String),
}

impl AnthropicAuth {
    /// Apply the API-key auth header to a request.
    pub async fn apply(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match self {
            Self::ApiKey(k) => Ok(builder.header("x-api-key", k)),
        }
    }
}

pub struct AnthropicClient {
    auth: AnthropicAuth,
    model: String,
    max_tokens: u32,
    client: Client,
}

impl AnthropicClient {
    pub fn new(auth: AnthropicAuth, model: impl Into<String>, max_tokens: u32) -> Result<Self> {
        // SECURITY: reqwest drops `Authorization` on a cross-host redirect,
        // not `x-api-key` (review of the pre-release build, S5). No redirect is followed.
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build HTTP client for Anthropic")?;
        Ok(Self {
            auth,
            model: model.into(),
            max_tokens,
            client,
        })
    }
}

#[async_trait]
impl LlmBackend for AnthropicClient {
    /// Both `send` and `stream` retry 429/5xx up to `MAX_RETRIES`, honouring
    /// `Retry-After` (`api.rs:747`, `:860`). The agent layer must not retry
    /// those classes again for this backend.
    fn internal_retry(&self) -> InternalRetry {
        InternalRetry::RateLimitAndServerErrors
    }

    fn display_name(&self) -> String {
        format!("anthropic/{}", self.model)
    }

    async fn send(&self, system: &str, messages: &[Message]) -> Result<ApiResponse> {
        let mut api_messages: Vec<Value> =
            messages.iter().map(message_to_anthropic_value).collect();
        // Mark the last user message as a cache breakpoint so the conversation
        // history gets cached incrementally (next turn reads everything up to
        // here at ~10 % cost). `system` already has its own breakpoint below;
        // Anthropic allows up to 4 breakpoints per request, we use 2.
        attach_cache_to_last_user(&mut api_messages);

        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": [{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": api_messages,
        });

        let mut attempt = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        loop {
            attempt += 1;
            debug!(attempt, model = %self.model, "Calling Anthropic API");

            let builder = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "prompt-caching-2024-07-31")
                .header("content-type", "application/json")
                .json(&body);
            let response = self
                .auth
                .apply(builder)
                .await?
                .send()
                .await
                .context("HTTP request to Anthropic API failed")?;

            let status = response.status();
            // Captured before `.json()` consumes the response body.
            let server_retry_after = parse_retry_after(response.headers());
            let resp_body: Value = response
                .json()
                .await
                .context("Failed to parse Anthropic API response")?;

            if status.is_success() {
                return extract_anthropic_response(&resp_body);
            }

            let err_type = resp_body
                .pointer("/error/type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let err_msg = resp_body
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");

            match status.as_u16() {
                401 => return Err(anyhow!("Anthropic authentication failed: {}.", err_msg)),
                429 | 500..=599 if attempt <= MAX_RETRIES => {
                    // Prefer the server's own reset window over blind backoff.
                    let wait =
                        server_retry_after.unwrap_or_else(|| Duration::from_millis(backoff_ms));
                    if wait > MAX_RETRY_AFTER {
                        return Err(anyhow!(
                            "Anthropic API error {}: rate-limited for another {}s, longer \
                             than this client will wait in-process. Retry later, or run \
                             `/auth anthropic` (API key) \
                             (same subscription, separate quota).",
                            status.as_u16(),
                            wait.as_secs()
                        ));
                    }
                    warn!(
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        from_server = server_retry_after.is_some(),
                        status = status.as_u16(),
                        "Retrying"
                    );
                    tokio::time::sleep(wait).await;
                    backoff_ms = (backoff_ms * 2).min(60_000);
                    continue;
                }
                _ => {
                    return Err(anyhow!(
                        "Anthropic API error {} ({}): {}",
                        status.as_u16(),
                        err_type,
                        err_msg
                    ))
                }
            }
        }
    }

    /// SSE streaming via Anthropic Messages API with `stream: true`.
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        use futures::StreamExt;

        let mut api_messages: Vec<Value> =
            messages.iter().map(message_to_anthropic_value).collect();
        attach_cache_to_last_user(&mut api_messages);

        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": [{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": api_messages,
            "stream": true,
        });

        debug!(model = %self.model, "Streaming Anthropic API");

        // Same 429/5xx exponential backoff loop as `send()` above.
        // Without this retry the agent would surface a hard failure to
        // the user on every transient rate-limit hit.
        let mut attempt = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        let response = loop {
            attempt += 1;
            let builder = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "prompt-caching-2024-07-31")
                .header("content-type", "application/json")
                .json(&body);
            let resp = self
                .auth
                .apply(builder)
                .await?
                .send()
                .await
                .context("Streaming request to Anthropic failed")?;
            let status = resp.status();
            if status.is_success() {
                break resp;
            }
            let code = status.as_u16();
            // 401 = auth-fatal, surface immediately.
            if code == 401 {
                let body_text = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "Anthropic stream auth error {}: {}\n💡 check ANTHROPIC_API_KEY.",
                    code,
                    body_text,
                ));
            }
            if (code == 429 || (500..=599).contains(&code)) && attempt <= MAX_RETRIES {
                // Read headers BEFORE `.text()` consumes the response.
                let server_retry_after = parse_retry_after(resp.headers());
                let body_text = resp.text().await.unwrap_or_default();
                // The server knows when its window resets; blind backoff does
                // not. Without this, six attempts burn ~31s of a window that is
                // typically minutes long — every retry fails by construction,
                // and hammering a limited endpoint can extend the window.
                // A 429 with no `retry-after` is unactionable: we cannot know
                // when the window reopens, and a subscription limit does not
                // clear inside the seconds an exponential ramp covers. Retrying
                // anyway burns ~31s and fails regardless — exactly what users
                // saw. One short probe still catches a genuine burst spike;
                // past that, stop and say what actually works.
                if code == 429 && server_retry_after.is_none() && attempt > OPAQUE_429_ATTEMPTS {
                    return Err(anyhow!(
                        "Anthropic stream error 429 after {} attempts: rate-limited with no \
                         `retry-after`, so there is no wait this client can honour. \
                         Retry in a few minutes.",
                        attempt,
                    ));
                }
                let wait = server_retry_after.unwrap_or_else(|| Duration::from_millis(backoff_ms));
                if wait > MAX_RETRY_AFTER {
                    return Err(anyhow!(
                        "Anthropic stream error {}: rate-limited for another {}s, longer \
                         than this client will wait in-process.",
                        code,
                        wait.as_secs(),
                    ));
                }
                warn!(
                    attempt,
                    wait_ms = wait.as_millis() as u64,
                    from_server = server_retry_after.is_some(),
                    status = code,
                    "Stream got transient error, retrying: {}",
                    body_text.chars().take(200).collect::<String>()
                );
                // Let the user see something is happening — silence on a
                // multi-second backoff feels broken.
                let _ = tx.send(StreamChunk::Token(format!(
                    "[rate-limited (attempt {}/{}), retrying in {:.1}s…]",
                    attempt,
                    MAX_RETRIES,
                    wait.as_secs_f64(),
                )));
                tokio::time::sleep(wait).await;
                backoff_ms = (backoff_ms * 2).min(60_000);
                continue;
            }
            // Non-retryable or budget exhausted — fail with the body.
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Anthropic stream error {} after {} attempts: {}",
                code,
                attempt,
                body_text,
            ));
        };

        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut buffer = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cache_creation_input_tokens = 0u64;
        let mut cache_read_input_tokens = 0u64;
        let mut stop_reason = "end_turn".to_string();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Anthropic stream read error")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                // drain(..=pos) mutates in place — O(1) vs O(n) for buffer = buffer[pos+1..]
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim_end_matches('\n').trim_end_matches('\r').trim();

                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }

                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        let event_type = v["type"].as_str().unwrap_or("");
                        match event_type {
                            "content_block_delta" => {
                                // Anthropic extended-thinking sends a `thinking_delta`
                                // for the reasoning block, then `text_delta` for the
                                // visible answer block. Surface them as separate
                                // stream chunks so the UI can render an indicator
                                // for thinking and only fold thinking into nothing
                                // for action parsing.
                                if let Some(text) =
                                    v.pointer("/delta/text").and_then(|d| d.as_str())
                                {
                                    if !text.is_empty() {
                                        full_content.push_str(text);
                                        let _ = tx.send(StreamChunk::Token(text.to_string()));
                                    }
                                } else if let Some(t) =
                                    v.pointer("/delta/thinking").and_then(|d| d.as_str())
                                {
                                    if !t.is_empty() {
                                        let _ = tx.send(StreamChunk::Thinking(t.to_string()));
                                    }
                                }
                            }
                            "message_start" => {
                                if let Some(inp) = v
                                    .pointer("/message/usage/input_tokens")
                                    .and_then(|t| t.as_u64())
                                {
                                    input_tokens = inp;
                                }
                                // Cache metrics land in message_start for Anthropic SSE —
                                // they're known as soon as the server checks the cache.
                                if let Some(w) = v
                                    .pointer("/message/usage/cache_creation_input_tokens")
                                    .and_then(|t| t.as_u64())
                                {
                                    cache_creation_input_tokens = w;
                                }
                                if let Some(r) = v
                                    .pointer("/message/usage/cache_read_input_tokens")
                                    .and_then(|t| t.as_u64())
                                {
                                    cache_read_input_tokens = r;
                                }
                            }
                            "message_delta" => {
                                if let Some(reason) =
                                    v.pointer("/delta/stop_reason").and_then(|r| r.as_str())
                                {
                                    stop_reason = reason.to_string();
                                }
                                if let Some(out) =
                                    v.pointer("/usage/output_tokens").and_then(|t| t.as_u64())
                                {
                                    output_tokens = out;
                                }
                            }
                            _ => {} // message_stop, ping, content_block_start/stop
                        }
                    }
                }
            }
        }

        Ok(ApiResponse {
            content: full_content,
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            stop_reason,
        })
    }
}

/// Convert a plain `Message` into an Anthropic API message value.
/// Supports multi-part content blocks (vision): when `m.images` is non-empty,
/// emits `[{type:"text"}, {type:"image", source:{type:"base64", ...}}, ...]`.
pub fn message_to_anthropic_value(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    if m.images.is_empty() {
        json!({ "role": role, "content": m.content })
    } else {
        let mut blocks: Vec<Value> = vec![json!({
            "type": "text",
            "text": m.content,
        })];
        for img in &m.images {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": img.data,
                }
            }));
        }
        json!({ "role": role, "content": blocks })
    }
}

/// Convert a plain `Message` into an Ollama `/api/chat` message value.
///
/// For text-only messages, returns `{role, content}`. For vision messages
/// (`m.images` non-empty), returns `{role, content, images: [<base64>, ...]}` —
/// the format llava / bakllava / moondream / minicpm-v expect. Note that
/// Ollama's vision API ignores `media_type`; only the raw base64 bytes
/// matter, but we still preserve `media_type` upstream for backend
/// neutrality (Anthropic / OpenAI need it).
pub fn message_to_ollama_value(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    if m.images.is_empty() {
        json!({ "role": role, "content": m.content })
    } else {
        let img_data: Vec<&str> = m.images.iter().map(|i| i.data.as_str()).collect();
        json!({
            "role": role,
            "content": m.content,
            "images": img_data,
        })
    }
}

/// Convert a plain `Message` into an OpenAI-compat `/v1/chat/completions`
/// message value.
///
/// For text-only messages, returns `{role, content: "..."}`. For vision
/// messages, returns `{role, content: [{type:"text"}, {type:"image_url",
/// image_url: {url: "data:<mime>;base64,<data>"}}, ...]}` — the data-URL
/// form accepted by GPT-4V / GPT-4o / Mistral-Vision / vLLM / LiteLLM.
pub fn message_to_openai_value(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    if m.images.is_empty() {
        json!({ "role": role, "content": m.content })
    } else {
        let mut blocks: Vec<Value> = vec![json!({ "type": "text", "text": m.content })];
        for img in &m.images {
            blocks.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) }
            }));
        }
        json!({ "role": role, "content": blocks })
    }
}

/// Attach `cache_control: ephemeral` to the *last user message* in an
/// Anthropic messages array.
///
/// Anthropic treats a `cache_control` marker as "cache the full prefix up to
/// and including this block". Putting it on the most recent user turn means
/// every previous turn (system + all user/assistant pairs) becomes a cache
/// read on the *next* request that shares the same prefix.
///
/// We intentionally only mark the last user message (not assistant replies)
/// because:
/// 1. The turn immediately after this one will always start by replaying
///    everything up to and including this user message — that's the cache
///    prefix we want to hit.
/// 2. Marking further back (older messages) would burn additional
///    breakpoints with no benefit (Anthropic allows max 4 per request, we
///    already spend 1 on `system`).
///
/// Converts string content into `[{ type:"text", text, cache_control }]`,
/// or appends cache_control to the last block of an existing block array.
/// No-op if the array is empty or ends on an assistant message (defensive).
fn attach_cache_to_last_user(api_messages: &mut [Value]) {
    // Walk backwards to find the most recent user message.
    for msg in api_messages.iter_mut().rev() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(obj) = msg.as_object_mut() else {
            return;
        };
        match obj.get_mut("content") {
            Some(Value::String(s)) => {
                let text = std::mem::take(s);
                obj.insert(
                    "content".into(),
                    json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": { "type": "ephemeral" },
                    }]),
                );
            }
            Some(Value::Array(blocks)) => {
                if let Some(last) = blocks.last_mut() {
                    if let Some(block_obj) = last.as_object_mut() {
                        block_obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                    }
                }
            }
            _ => {}
        }
        return;
    }
}

fn extract_anthropic_response(response: &Value) -> Result<ApiResponse> {
    let content = response
        .pointer("/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            let preview = response.to_string();
            anyhow!(
                "Unexpected Anthropic response — missing content[0].text: {}",
                &preview[..preview.len().min(300)]
            )
        })?
        .to_string();

    let input_tokens = response
        .pointer("/usage/input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = response
        .pointer("/usage/output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_input_tokens = response
        .pointer("/usage/cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_input_tokens = response
        .pointer("/usage/cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let stop_reason = response
        .pointer("/stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    if cache_read_input_tokens > 0 || cache_creation_input_tokens > 0 {
        debug!(
            input = input_tokens,
            output = output_tokens,
            cache_read = cache_read_input_tokens,
            cache_write = cache_creation_input_tokens,
            "Anthropic usage"
        );
    }

    Ok(ApiResponse {
        content,
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        stop_reason,
    })
}

// ── Ollama client ─────────────────────────────────────────────────────────────

pub struct OllamaClient {
    endpoint: String,
    model: String,
    max_tokens: u32,
    /// `options.num_ctx` of every call.
    num_ctx: u32,
    api_key: Option<String>,
    client: Client,
    /// Cleared once Ollama answers that the model does not support tools.
    native_tools: std::sync::atomic::AtomicBool,
}

impl OllamaClient {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
        api_key: Option<String>,
    ) -> Result<Self> {
        let endpoint = endpoint.into();
        let client = client_for(&endpoint)
            .timeout(Duration::from_secs(300))
            .build()
            .context("Failed to build HTTP client for Ollama")?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.into(),
            max_tokens,
            num_ctx: crate::config::OllamaConfig::default().num_ctx,
            api_key,
            client,
            native_tools: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// The context window asked of the server, prompt and answer together.
    pub fn with_num_ctx(mut self, num_ctx: u32) -> Self {
        self.num_ctx = num_ctx;
        self
    }

    /// Without `num_ctx` Ollama sizes the window from the GPU's memory
    /// (4096, 32768 or 262144 tokens): 32768 on the 5090 on 2026-10-16,
    /// under a harness budget of 100k.
    fn options(&self) -> Value {
        json!({ "num_predict": self.max_tokens, "num_ctx": self.num_ctx })
    }

    /// Health-check the Ollama server: verifies it is reachable and that the
    /// configured model is already pulled. Called at `--serve` boot so a
    /// missing `ollama pull` surfaces immediately rather than at the first prompt.
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.endpoint);
        let mut req = self.client.get(&url);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }
        let resp = req.send().await.with_context(|| {
            format!(
                "Cannot reach Ollama at {}. Is it running? (`ollama serve`)",
                url_for_display(&self.endpoint)
            )
        })?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "Ollama server returned status {} — is it running correctly?",
                resp.status()
            ));
        }
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse Ollama /api/tags response")?;
        let models = body["models"].as_array().cloned().unwrap_or_default();
        let model_names: Vec<&str> = models.iter().filter_map(|m| m["name"].as_str()).collect();
        let target = &self.model;
        let found = model_names.iter().any(|n| {
            n == target
                || n.trim_end_matches(":latest") == target.as_str()
                || *n == format!("{}:latest", target)
        });
        if !found {
            let available = if model_names.is_empty() {
                "none".to_string()
            } else {
                model_names.join(", ")
            };
            return Err(anyhow!(
                "Model '{}' not found in Ollama. Available: {}.\nPull it with: ollama pull {}",
                target,
                available,
                target
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl LlmBackend for OllamaClient {
    fn display_name(&self) -> String {
        format!("ollama/{}", self.model)
    }

    async fn send(&self, system: &str, messages: &[Message]) -> Result<ApiResponse> {
        let mut api_messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];
        for msg in messages {
            api_messages.push(message_to_ollama_value(msg));
        }
        let body = json!({
            "model": self.model,
            "messages": api_messages,
            "stream": false,
            "options": self.options(),
        });

        let url = format!("{}/api/chat", self.endpoint);
        debug!(url = %url_for_display(&url), model = %self.model, "Calling Ollama API");

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let response = req.send().await.with_context(|| {
            format!("HTTP request to Ollama failed ({})", url_for_display(&url))
        })?;
        let status = response.status();
        let resp_body: Value = response
            .json()
            .await
            .context("Failed to parse Ollama response as JSON")?;

        if !status.is_success() {
            let err = resp_body["error"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("Ollama error {}: {}", status.as_u16(), err));
        }

        extract_ollama_response(&resp_body)
    }

    /// NDJSON streaming via Ollama `/api/chat` with `stream: true` (default).
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        self.stream_inner(system, messages, &[], tx).await
    }

    /// Tools in Ollama's `/api/chat` format; the same fallback as
    /// openai-compat when the model has no tool support.
    async fn stream_with_tools(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        use std::sync::atomic::Ordering;
        if tools.is_empty() || !self.native_tools.load(Ordering::Relaxed) {
            return self.stream_inner(system, messages, &[], tx).await;
        }
        match self.stream_inner(system, messages, tools, tx.clone()).await {
            Err(e) if e.downcast_ref::<ToolsRefused>().is_some() => {
                warn!(
                    model = %self.model,
                    "ollama: {e}. Native tools are off for this session: \
                     actions go through the text protocol."
                );
                self.native_tools.store(false, Ordering::Relaxed);
                self.stream_inner(system, messages, &[], tx).await
            }
            other => other,
        }
    }
}

impl OllamaClient {
    /// Each line is a JSON object; `done: true` alone on the last one.
    async fn stream_inner(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        use futures::StreamExt;

        let mut api_messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];
        for msg in messages {
            api_messages.push(message_to_ollama_value(msg));
        }
        let mut body = json!({
            "model": self.model,
            "messages": api_messages,
            "stream": true,
            "options": self.options(),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }

        let url = format!("{}/api/chat", self.endpoint);
        debug!(url = %url_for_display(&url), model = %self.model, "Streaming Ollama API");

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let response = req.send().await.with_context(|| {
            format!(
                "Streaming request to Ollama failed ({})",
                url_for_display(&url)
            )
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body_text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(body_text);
            // server/images.go CheckCapabilities: 400 "<model> does not support tools".
            if !tools.is_empty()
                && status == reqwest::StatusCode::BAD_REQUEST
                && message.contains("does not support tools")
            {
                return Err(anyhow::Error::new(ToolsRefused(format!(
                    "{status}: {message}"
                ))));
            }
            return Err(anyhow!("Ollama stream error {}: {}", status, message));
        }

        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut buffer = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut stop_reason = "stop".to_string();
        let mut calls: Vec<crate::native_tools::ToolCall> = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Ollama stream read error")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Ollama sends NDJSON — one JSON object per line
            while let Some(pos) = buffer.find('\n') {
                // drain(..=pos) mutates in place — O(1) vs O(n) for buffer = buffer[pos+1..]
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                // A failure after the 200 comes as `{"error": …}` on its own line.
                if let Some(err) = v.get("error").and_then(Value::as_str) {
                    return Err(anyhow!("Ollama stream error: {err}"));
                }
                if let Some(message) = v.get("message") {
                    // Visible answer in `content`, reasoning (qwen3, deepseek-r1)
                    // in `thinking`: each to its own variant.
                    if let Some(text) = message.get("content").and_then(Value::as_str) {
                        if !text.is_empty() {
                            full_content.push_str(text);
                            let _ = tx.send(StreamChunk::Token(text.to_string()));
                        }
                    }
                    if let Some(t) = message.get("thinking").and_then(Value::as_str) {
                        if !t.is_empty() {
                            let _ = tx.send(StreamChunk::Thinking(t.to_string()));
                        }
                    }
                    // Whole calls, `arguments` an object (api/types.go ToolCall).
                    calls.extend(crate::native_tools::tool_calls_of_message(message));
                }
                if v["done"].as_bool().unwrap_or(false) {
                    input_tokens = v["prompt_eval_count"].as_u64().unwrap_or(0);
                    output_tokens = v["eval_count"].as_u64().unwrap_or(0);
                    stop_reason = v["done_reason"].as_str().unwrap_or("stop").to_string();
                }
            }
        }

        if let Some(action) = crate::native_tools::tool_calls_to_action_text(&calls) {
            let _ = tx.send(StreamChunk::Token(action.clone()));
            full_content = with_native_action(action, &full_content);
        }

        Ok(ApiResponse {
            content: full_content,
            input_tokens,
            output_tokens,
            stop_reason,
            ..Default::default()
        })
    }
}

fn extract_ollama_response(response: &Value) -> Result<ApiResponse> {
    let content = response
        .pointer("/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            let preview = response.to_string();
            anyhow!(
                "Unexpected Ollama response shape — missing message.content: {}",
                &preview[..preview.len().min(300)]
            )
        })?
        .to_string();

    // Ollama native API: eval_count = output tokens, prompt_eval_count = input tokens
    let input_tokens = response
        .pointer("/prompt_eval_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = response
        .pointer("/eval_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let stop_reason = response
        .pointer("/done_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();

    Ok(ApiResponse {
        content,
        input_tokens,
        output_tokens,
        stop_reason,
        ..Default::default()
    })
}

// ── OpenAI-compatible client ──────────────────────────────────────────────────

/// Strip trailing `/v1` (and trailing slashes) from a base URL.
/// Users commonly set `base_url = "https://api.openai.com/v1"` which would
/// produce `https://api.openai.com/v1/v1/chat/completions`. This normalizes it.
fn normalize_base_url(url: String) -> String {
    let mut s = url.trim_end_matches('/').to_string();
    if s.ends_with("/v1") {
        s.truncate(s.len() - 3);
    }
    s
}

pub struct OpenAICompatClient {
    base_url: String,
    model: String,
    max_tokens: u32,
    api_key: Option<String>,
    client: Client,
    /// Cleared once the server refuses `tools`; then text protocol only.
    native_tools: std::sync::atomic::AtomicBool,
}

/// The server answered an error that names tools to a request carrying them.
#[derive(Debug)]
struct ToolsRefused(String);

impl std::fmt::Display for ToolsRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused native tools: {}", self.0)
    }
}

impl std::error::Error for ToolsRefused {}

impl OpenAICompatClient {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
        api_key: Option<String>,
    ) -> Result<Self> {
        let base_url = base_url.into();
        let client = client_for(&base_url)
            .timeout(Duration::from_secs(300))
            .build()
            .context("Failed to build HTTP client for OpenAI-compat backend")?;
        Ok(Self {
            base_url: normalize_base_url(base_url),
            model: model.into(),
            max_tokens,
            api_key,
            client,
            native_tools: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// No native tools, ever, and the caller's HTTP client (`x0ne-engine`:
    /// no proxy, no redirect).
    pub fn text_only(
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
        api_key: Option<String>,
        client: Client,
    ) -> Self {
        Self {
            base_url: normalize_base_url(base_url.into()),
            model: model.into(),
            max_tokens,
            api_key,
            client,
            native_tools: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Health-check the OpenAI-compat server: verifies it is reachable.
    /// Called at `--serve` boot for early failure on misconfigured endpoints.
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }
        let resp = req.send().await.with_context(|| {
            format!(
                "Cannot reach OpenAI-compat server at {}/v1/models. Is it running?",
                url_for_display(&self.base_url)
            )
        })?;
        if !resp.status().is_success() {
            warn!(status = %resp.status(), "/v1/models returned non-200; proceeding anyway");
        }
        Ok(())
    }
}

#[async_trait]
impl LlmBackend for OpenAICompatClient {
    fn display_name(&self) -> String {
        format!(
            "openai-compat/{} @ {}",
            self.model,
            url_for_display(&self.base_url)
        )
    }

    async fn send(&self, system: &str, messages: &[Message]) -> Result<ApiResponse> {
        let mut api_messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];
        for msg in messages {
            api_messages.push(message_to_openai_value(msg));
        }
        let body = json!({
            "model": self.model,
            "messages": api_messages,
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        let url = format!("{}/v1/chat/completions", self.base_url);
        debug!(url = %url_for_display(&url), model = %self.model, "Calling OpenAI-compat API");

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let mut attempt = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        loop {
            attempt += 1;
            let response = req
                .try_clone()
                .ok_or_else(|| anyhow!("Failed to clone request"))?
                .send()
                .await
                .with_context(|| format!("HTTP request to {} failed", url_for_display(&url)))?;

            let status = response.status();
            // Captured before `.json()` consumes the response body.
            let server_retry_after = parse_retry_after(response.headers());
            let resp_body: Value = response
                .json()
                .await
                .context("Failed to parse OpenAI-compat response as JSON")?;

            if status.is_success() {
                return extract_openai_compat_response(&resp_body);
            }

            let err_msg = resp_body
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| resp_body["error"].as_str())
                .unwrap_or("unknown error");

            match status.as_u16() {
                401 | 403 => {
                    return Err(anyhow!(
                        "Authentication failed ({}): {}.",
                        status.as_u16(),
                        err_msg
                    ))
                }
                429 | 500..=599 if attempt <= MAX_RETRIES => {
                    // Prefer the server's own reset window over blind backoff.
                    let wait =
                        server_retry_after.unwrap_or_else(|| Duration::from_millis(backoff_ms));
                    if wait > MAX_RETRY_AFTER {
                        return Err(anyhow!(
                            "Anthropic API error {}: rate-limited for another {}s, longer \
                             than this client will wait in-process. Retry later, or run \
                             `/auth anthropic` (API key) \
                             (same subscription, separate quota).",
                            status.as_u16(),
                            wait.as_secs()
                        ));
                    }
                    warn!(
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        from_server = server_retry_after.is_some(),
                        status = status.as_u16(),
                        "Retrying"
                    );
                    tokio::time::sleep(wait).await;
                    backoff_ms = (backoff_ms * 2).min(60_000);
                    continue;
                }
                _ => {
                    return Err(anyhow!(
                        "OpenAI-compat API error {}: {}",
                        status.as_u16(),
                        err_msg
                    ))
                }
            }
        }
    }

    /// SSE streaming via OpenAI-compat `/v1/chat/completions` with `stream: true`.
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        self.stream_inner(system, messages, &[], tx).await
    }

    async fn stream_with_tools(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        use std::sync::atomic::Ordering;
        if tools.is_empty() || !self.native_tools.load(Ordering::Relaxed) {
            return self.stream_inner(system, messages, &[], tx).await;
        }
        match self.stream_inner(system, messages, tools, tx.clone()).await {
            Err(e) if e.downcast_ref::<ToolsRefused>().is_some() => {
                warn!(error = %e, "native tools refused; text action protocol from now on");
                self.native_tools.store(false, Ordering::Relaxed);
                self.stream_inner(system, messages, &[], tx).await
            }
            other => other,
        }
    }
}

impl OpenAICompatClient {
    async fn stream_inner(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
        tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<ApiResponse> {
        use futures::StreamExt;

        let mut api_messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];
        for msg in messages {
            api_messages.push(message_to_openai_value(msg));
        }
        let mut body = json!({
            "model": self.model,
            "messages": api_messages,
            "max_tokens": self.max_tokens,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = json!("auto");
        }
        let url = format!("{}/v1/chat/completions", self.base_url);
        debug!(url = %url_for_display(&url), model = %self.model, "Streaming OpenAI-compat API");

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let response = req
            .send()
            .await
            .with_context(|| format!("Streaming request to {} failed", url_for_display(&url)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            // llama.cpp without --jinja: "tools param requires --jinja flag".
            let names_tools = body_text.to_ascii_lowercase().contains("tool");
            if !tools.is_empty() && names_tools && !matches!(status.as_u16(), 401 | 403 | 429) {
                return Err(anyhow::Error::new(ToolsRefused(format!(
                    "{status}: {body_text}"
                ))));
            }
            return Err(anyhow!(
                "OpenAI-compat stream error {}: {}",
                status,
                body_text
            ));
        }

        // A server may ignore `stream` and answer one JSON completion
        // (measured 2026-09-27); parsed as SSE it was an empty reply every call.
        let is_json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().starts_with("application/json"));
        if is_json {
            let body: Value = response
                .json()
                .await
                .context("Failed to parse OpenAI-compat response as JSON")?;
            let resp = extract_openai_compat_response(&body)?;
            if !resp.content.is_empty() {
                let _ = tx.send(StreamChunk::Token(resp.content.clone()));
            }
            return Ok(resp);
        }

        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut buffer = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut stop_reason = "stop".to_string();
        let mut tool_calls = crate::native_tools::ToolCallAccumulator::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Stream read error")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = buffer.find('\n') {
                // drain(..=pos) mutates in place — O(1) vs O(n) for buffer = buffer[pos+1..]
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim_end_matches('\n').trim_end_matches('\r').trim();

                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        continue;
                    }

                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(delta) = v.pointer("/choices/0/delta") {
                            tool_calls.push_delta(delta);
                        }
                        // Extract token delta from `content`. Some thinking-
                        // mode models (Z.ai GLM-4/5, DeepSeek R1) leak
                        // reasoning into `content` as standalone "thinking:"
                        // marker chunks alternating with actual content
                        // tokens. Surface those leaked markers as Thinking
                        // chunks (so the UI shows a "🧠 reflexion" indicator)
                        // instead of dropping them silently — keeps the
                        // visible message body clean either way.
                        if let Some(delta) = v
                            .pointer("/choices/0/delta/content")
                            .and_then(|d| d.as_str())
                        {
                            if !delta.is_empty() {
                                if is_reasoning_marker(delta) {
                                    let _ = tx.send(StreamChunk::Thinking(delta.to_string()));
                                } else {
                                    full_content.push_str(delta);
                                    let _ = tx.send(StreamChunk::Token(delta.to_string()));
                                }
                            }
                        }
                        // Providers that route reasoning to a separate
                        // `reasoning_content` (DeepSeek-R1 native, some
                        // OpenAI-compat proxies) — propagate as Thinking.
                        if let Some(t) = v
                            .pointer("/choices/0/delta/reasoning_content")
                            .and_then(|d| d.as_str())
                        {
                            if !t.is_empty() {
                                let _ = tx.send(StreamChunk::Thinking(t.to_string()));
                            }
                        }
                        // Extract finish reason
                        if let Some(reason) = v
                            .pointer("/choices/0/finish_reason")
                            .and_then(|r| r.as_str())
                        {
                            if reason != "null" {
                                stop_reason = reason.to_string();
                            }
                        }
                        // Extract usage if present (some servers send on final chunk)
                        if let Some(prompt) =
                            v.pointer("/usage/prompt_tokens").and_then(|t| t.as_u64())
                        {
                            input_tokens = prompt;
                        }
                        if let Some(completion) = v
                            .pointer("/usage/completion_tokens")
                            .and_then(|t| t.as_u64())
                        {
                            output_tokens = completion;
                        }
                    }
                }
            }
        }

        if let Some(action) = crate::native_tools::tool_calls_to_action_text(&tool_calls.finish()) {
            let _ = tx.send(StreamChunk::Token(action.clone()));
            full_content = with_native_action(action, &full_content);
        }

        Ok(ApiResponse {
            content: full_content,
            input_tokens,
            output_tokens,
            stop_reason,
            ..Default::default()
        })
    }
}

/// The native call first, so the parser takes it over any action in prose.
fn with_native_action(action: String, prose: &str) -> String {
    let prose = prose.trim();
    if prose.is_empty() {
        action
    } else {
        format!("{action}\n{prose}")
    }
}

/// Detect a "thinking:" / "reasoning:" marker chunk emitted by some
/// thinking-mode models (Z.ai GLM-4/5, DeepSeek R1, Yi-Lightning) when
/// the upstream proxy puts chain-of-thought into `delta.content` instead
/// of `delta.reasoning_content`. The chunks observed in the wild are
/// short standalone tokens like `"thinking:"`, `"thinking: "`,
/// `"<think>"`, or `"</think>"` — we drop them so the visible message
/// body stays clean. Only triggers on these exact short markers so it
/// can never clobber legitimate text containing the word "thinking".
fn is_reasoning_marker(s: &str) -> bool {
    let t = s.trim();
    matches!(
        t,
        "thinking:"
            | "Thinking:"
            | "THINKING:"
            | "reasoning:"
            | "Reasoning:"
            | "REASONING:"
            | "<think>"
            | "</think>"
            | "<thinking>"
            | "</thinking>"
            | "<reasoning>"
            | "</reasoning>"
    )
}

fn extract_openai_compat_response(response: &Value) -> Result<ApiResponse> {
    let message = response.pointer("/choices/0/message");
    let text = message
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str());
    let calls = message
        .map(crate::native_tools::tool_calls_of_message)
        .unwrap_or_default();
    let content = match crate::native_tools::tool_calls_to_action_text(&calls) {
        Some(action) => with_native_action(action, text.unwrap_or("")),
        None => text
            .ok_or_else(|| {
                let preview = response.to_string();
                anyhow!(
                    "Unexpected OpenAI-compat response — missing choices[0].message.content: {}",
                    &preview[..preview.len().min(400)]
                )
            })?
            .to_string(),
    };

    let input_tokens = response
        .pointer("/usage/prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = response
        .pointer("/usage/completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let stop_reason = response
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();
    // OpenAI prompt caching — gpt-4o and gpt-4o-mini report cached_tokens inside
    // usage.prompt_tokens_details. We surface it as cache_read_input_tokens so the
    // UsageTracker can compute hit ratios and accurate cost estimates.
    let cache_read_input_tokens = response
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(ApiResponse {
        content,
        input_tokens,
        output_tokens,
        stop_reason,
        cache_read_input_tokens,
        ..Default::default()
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── A key goes only to the vendor that issued it ─────────────────

    #[test]
    fn create_backend_refuses_an_anthropic_key_for_openai() {
        let mut llm = crate::config::Config::default().llm;
        llm.provider = "openai-compat".into();
        llm.openai_compat.base_url = "https://api.openai.com".into();
        llm.openai_compat.api_key = Some("sk-ant-api03-zzzz-abcd".into());
        let e = create_backend(&llm, None).err().expect("refused");
        assert!(format!("{e:#}").contains("api.openai.com"), "{e:#}");
    }

    #[test]
    fn create_backend_refuses_a_foreign_key_for_anthropic() {
        let mut llm = crate::config::Config::default().llm;
        llm.provider = "anthropic".into();
        assert!(create_backend(&llm, Some("sk-proj-abcd".into())).is_err());
        assert!(create_backend(&llm, Some("sk-ant-api03-abcd".into())).is_ok());
    }

    // ── retry-after ────────────────────────────────────────────────────────

    fn hdrs(value: &str) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", value.parse().unwrap());
        h
    }

    /// The server tells us when its window resets; blind backoff does not.
    #[test]
    fn retry_after_parses_delay_seconds() {
        assert_eq!(
            parse_retry_after(&hdrs("60")),
            Some(Duration::from_secs(60))
        );
        assert_eq!(parse_retry_after(&hdrs("0")), Some(Duration::ZERO));
        // Some gateways send fractional seconds.
        assert_eq!(
            parse_retry_after(&hdrs("1.5")),
            Some(Duration::from_millis(1500))
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(
            parse_retry_after(&hdrs(" 30 ")),
            Some(Duration::from_secs(30))
        );
    }

    /// Anything we cannot interpret must fall back to exponential backoff
    /// rather than panic or sleep for a nonsense duration.
    #[test]
    fn retry_after_rejects_unparseable_values() {
        assert_eq!(
            parse_retry_after(&reqwest::header::HeaderMap::new()),
            None,
            "absent header"
        );
        assert_eq!(parse_retry_after(&hdrs("soon")), None, "non-numeric");
        assert_eq!(parse_retry_after(&hdrs("-5")), None, "negative");
        assert_eq!(parse_retry_after(&hdrs("")), None, "empty");
        // RFC-7231 HTTP-date form is not handled; must degrade, not panic.
        assert_eq!(
            parse_retry_after(&hdrs("Wed, 21 Oct 2015 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(&hdrs("NaN")), None, "non-finite");
        assert_eq!(parse_retry_after(&hdrs("inf")), None, "infinite");
    }

    // ── LiveTokenCounter ──────────────────────────────────────────────────

    // ── existing tests ────────────────────────────────────────────────────

    #[test]
    fn extract_anthropic_response_happy_path() {
        let r = json!({
            "content": [{"type": "text", "text": "Hello!"}],
            "usage": {"input_tokens": 15, "output_tokens": 5},
            "stop_reason": "end_turn"
        });
        let resp = extract_anthropic_response(&r).unwrap();
        assert_eq!(resp.content, "Hello!");
        assert_eq!(resp.input_tokens, 15);
        assert_eq!(resp.output_tokens, 5);
        assert_eq!(resp.stop_reason, "end_turn");
    }

    #[test]
    fn extract_anthropic_response_missing_errors() {
        let r = json!({"error": {"type": "invalid_request", "message": "bad"}});
        assert!(extract_anthropic_response(&r).is_err());
    }

    #[test]
    fn extract_anthropic_response_missing_usage_defaults_to_zero() {
        let r = json!({ "content": [{"type": "text", "text": "hi"}] });
        let resp = extract_anthropic_response(&r).unwrap();
        assert_eq!(resp.input_tokens, 0);
        assert_eq!(resp.output_tokens, 0);
    }

    #[test]
    fn extract_ollama_response_happy_path() {
        let r = json!({
            "message": {"role": "assistant", "content": "Hi from Ollama!"},
            "prompt_eval_count": 10,
            "eval_count": 8,
            "done_reason": "stop"
        });
        let resp = extract_ollama_response(&r).unwrap();
        assert_eq!(resp.content, "Hi from Ollama!");
        assert_eq!(resp.input_tokens, 10);
        assert_eq!(resp.output_tokens, 8);
    }

    #[test]
    fn extract_ollama_response_missing_tokens_defaults_to_zero() {
        let r = json!({"message": {"role": "assistant", "content": "Hi"}});
        let resp = extract_ollama_response(&r).unwrap();
        assert_eq!(resp.input_tokens, 0);
        assert_eq!(resp.output_tokens, 0);
    }

    #[test]
    fn extract_ollama_response_missing_errors() {
        let r = json!({"error": "model not found"});
        assert!(extract_ollama_response(&r).is_err());
    }

    #[test]
    fn stream_chunk_text_borrows_inner_for_both_variants() {
        let t = StreamChunk::Token("hello".into());
        let r = StreamChunk::Thinking("…thinking…".into());
        assert_eq!(t.text(), "hello");
        assert_eq!(r.text(), "…thinking…");
        assert!(matches!(t, StreamChunk::Token(_)));
        assert!(matches!(r, StreamChunk::Thinking(_)));
    }

    #[test]
    fn is_reasoning_marker_drops_known_markers() {
        assert!(is_reasoning_marker("thinking:"));
        assert!(is_reasoning_marker("Thinking: "));
        assert!(is_reasoning_marker("<think>"));
        assert!(is_reasoning_marker("</think>"));
        assert!(is_reasoning_marker("reasoning:"));
        assert!(is_reasoning_marker("<reasoning>"));
    }

    #[test]
    fn is_reasoning_marker_preserves_legitimate_text() {
        // Substring matches must not trigger — only exact short markers.
        assert!(!is_reasoning_marker("I was thinking: maybe try X"));
        assert!(!is_reasoning_marker("thinking"));
        assert!(!is_reasoning_marker("I'm thinking about it"));
        assert!(!is_reasoning_marker("Hello world"));
        assert!(!is_reasoning_marker(""));
    }

    #[test]
    fn extract_openai_compat_response_happy_path() {
        let r = json!({
            "choices": [{"message": {"role": "assistant", "content": "Hello from LiteLLM!"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        });
        let resp = extract_openai_compat_response(&r).unwrap();
        assert_eq!(resp.content, "Hello from LiteLLM!");
        assert_eq!(resp.input_tokens, 20);
        assert_eq!(resp.output_tokens, 10);
        assert_eq!(resp.stop_reason, "stop");
    }

    #[test]
    fn extract_openai_compat_response_missing_usage_defaults_to_zero() {
        let r = json!({"choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}]});
        let resp = extract_openai_compat_response(&r).unwrap();
        assert_eq!(resp.input_tokens, 0);
        assert_eq!(resp.output_tokens, 0);
    }

    #[test]
    fn extract_openai_compat_response_missing_errors() {
        let r = json!({"error": {"message": "model not found"}});
        assert!(extract_openai_compat_response(&r).is_err());
    }

    #[test]
    fn usage_tracker_accumulates() {
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        let r1 = ApiResponse {
            input_tokens: 100,
            output_tokens: 50,
            stop_reason: "stop".into(),
            ..Default::default()
        };
        let r2 = ApiResponse {
            input_tokens: 200,
            output_tokens: 75,
            stop_reason: "stop".into(),
            ..Default::default()
        };
        tracker.record(&r1);
        tracker.record(&r2);
        assert_eq!(tracker.total_input_tokens, 300);
        assert_eq!(tracker.total_output_tokens, 125);
        assert_eq!(tracker.total_requests, 2);
    }

    #[test]
    fn usage_tracker_accumulates_cache_tokens() {
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        tracker.record(&ApiResponse {
            input_tokens: 50,
            output_tokens: 10,
            cache_creation_input_tokens: 4_000, // first call writes the cache
            cache_read_input_tokens: 0,
            stop_reason: "stop".into(),
            ..Default::default()
        });
        tracker.record(&ApiResponse {
            input_tokens: 60,
            output_tokens: 12,
            cache_creation_input_tokens: 200, // tiny extension
            cache_read_input_tokens: 4_000,   // full prefix hit
            stop_reason: "stop".into(),
            ..Default::default()
        });
        assert_eq!(tracker.total_input_tokens, 110);
        assert_eq!(tracker.total_cache_write_tokens, 4_200);
        assert_eq!(tracker.total_cache_read_tokens, 4_000);
        // Hit ratio = reads / (fresh input + reads) = 4000 / (110 + 4000) ≈ 0.973
        assert!((tracker.cache_hit_ratio() - 0.973).abs() < 0.01);
    }

    #[test]
    fn usage_tracker_cost_counts_cache_at_anthropic_rates() {
        // Anthropic: writes cost 1.25× input, reads cost 0.10× input.
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        tracker.total_input_tokens = 1_000_000; // 1M × $3       = $3.00
        tracker.total_output_tokens = 1_000_000; // 1M × $15      = $15.00
        tracker.total_cache_write_tokens = 1_000_000; // 1M × $3 × 1.25 = $3.75
        tracker.total_cache_read_tokens = 1_000_000; // 1M × $3 × 0.10 = $0.30
        let cost = tracker.estimated_cost(3.0, 15.0);
        // 3 + 15 + 3.75 + 0.30 = 22.05
        assert!((cost - 22.05).abs() < 0.001, "Expected 22.05, got {}", cost);
    }

    /// `/cost` priced every remote model at the `[cost]` Sonnet rate: an
    /// Opus session showed $3 per million input tokens, an endpoint with no
    /// known price a made-up dollar figure (measured 2026-10-27, before the fix).
    #[test]
    fn cost_summary_uses_the_list_price_or_says_unknown() {
        let prices = crate::agent::pricing::ConfiguredPrices::from_config(
            &crate::config::CostConfig::default(),
        );
        let mut opus = UsageTracker::new("anthropic/claude-opus-5");
        opus.total_input_tokens = 1_000_000;
        let s = opus.summary_with(&prices);
        assert!(s.contains("Cost: $5.0000"), "{s}");

        let mut remote = UsageTracker::new("openai-compat/m @ https://llm.example");
        remote.total_input_tokens = 1_000_000;
        let s = remote.summary_with(&prices);
        assert!(s.contains("Cost: unknown"), "{s}");

        let local = UsageTracker::new("ollama/qwen3:8b");
        assert!(local.summary_with(&prices).contains("$0.00 (local)"));
    }

    #[test]
    fn summary_omits_cache_segment_when_no_cache_traffic() {
        let mut tracker = UsageTracker::new("ollama/qwen2.5");
        tracker.record(&ApiResponse {
            input_tokens: 100,
            output_tokens: 50,
            stop_reason: "stop".into(),
            ..Default::default()
        });
        let s = tracker.summary(3.0, 15.0);
        assert!(
            !s.contains("Cache"),
            "Ollama / no-cache sessions should not mention Cache: {}",
            s
        );
    }

    #[test]
    fn summary_reports_cache_hit_percentage() {
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        tracker.record(&ApiResponse {
            input_tokens: 20,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 80,
            stop_reason: "stop".into(),
            ..Default::default()
        });
        let s = tracker.summary(3.0, 15.0);
        assert!(
            s.contains("Cache"),
            "Anthropic cache reads must surface in summary: {}",
            s
        );
        // 80 / (20 + 80) = 80 %
        assert!(
            s.contains("80% hit"),
            "summary should show 80% hit rate: {}",
            s
        );
    }

    #[test]
    fn attach_cache_wraps_string_content_on_last_user() {
        let mut msgs = vec![
            json!({"role": "user",      "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
            json!({"role": "user",      "content": "what's up"}),
        ];
        attach_cache_to_last_user(&mut msgs);

        // Last user message is now a block array with cache_control.
        let last_content = &msgs[2]["content"];
        assert!(
            last_content.is_array(),
            "last user content should be a block array"
        );
        let last_block = &last_content[0];
        assert_eq!(last_block["type"], "text");
        assert_eq!(last_block["text"], "what's up");
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");

        // First user and assistant untouched — would burn extra breakpoints otherwise.
        assert_eq!(msgs[0]["content"], "hi");
        assert_eq!(msgs[1]["content"], "hello");
    }

    #[test]
    fn attach_cache_extends_existing_block_array() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "look at this screenshot:" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "…" } }
            ]
        })];
        attach_cache_to_last_user(&mut msgs);
        let blocks = msgs[0]["content"].as_array().expect("still an array");
        // cache_control applied to the *last* block only (image here) — that's the
        // whole-prefix marker Anthropic expects.
        assert!(blocks[0].get("cache_control").is_none());
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn attach_cache_is_noop_when_no_user_message() {
        let mut msgs = vec![json!({"role": "assistant", "content": "orphan"})];
        attach_cache_to_last_user(&mut msgs);
        assert_eq!(
            msgs[0]["content"], "orphan",
            "should not mutate assistant content"
        );
    }

    #[test]
    fn attach_cache_handles_empty_array() {
        let mut msgs: Vec<Value> = vec![];
        attach_cache_to_last_user(&mut msgs); // must not panic
        assert!(msgs.is_empty());
    }

    #[test]
    fn extract_anthropic_response_captures_cache_fields() {
        let r = json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "cache_creation_input_tokens": 1234,
                "cache_read_input_tokens": 5678
            }
        });
        let resp = extract_anthropic_response(&r).expect("parse");
        assert_eq!(resp.input_tokens, 5);
        assert_eq!(resp.output_tokens, 2);
        assert_eq!(resp.cache_creation_input_tokens, 1234);
        assert_eq!(resp.cache_read_input_tokens, 5678);
    }

    #[test]
    fn extract_anthropic_response_defaults_cache_fields_to_zero() {
        // Older responses (or ones below the cache size threshold) omit cache fields.
        let r = json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        });
        let resp = extract_anthropic_response(&r).expect("parse");
        assert_eq!(resp.cache_creation_input_tokens, 0);
        assert_eq!(resp.cache_read_input_tokens, 0);
    }

    #[test]
    fn usage_tracker_cost_calculation() {
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        // 1M input tokens at $3/M = $3, 1M output at $15/M = $15 → total $18
        tracker.total_input_tokens = 1_000_000;
        tracker.total_output_tokens = 1_000_000;
        let cost = tracker.estimated_cost(3.0, 15.0);
        assert!((cost - 18.0).abs() < 0.001, "Expected 18.0, got {}", cost);
    }

    #[test]
    fn anthropic_client_builds_api_key_path() {
        assert!(AnthropicClient::new(
            AnthropicAuth::ApiKey("sk-dummy".into()),
            "claude-sonnet-4-20250514",
            4096
        )
        .is_ok());
    }

    #[tokio::test]
    async fn anthropic_auth_apply_uses_x_api_key_for_api_key() {
        let c = reqwest::Client::new();
        let auth = AnthropicAuth::ApiKey("sk-ant-api03-xxx".into());
        let req = auth
            .apply(c.post("https://api.anthropic.com/v1/messages"))
            .await
            .unwrap()
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(
            h.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-api03-xxx")
        );
        assert!(
            h.get("Authorization").is_none(),
            "API-key path must NOT carry Bearer"
        );
        // Regression guard: the removed OAuth path's Claude-Code identity
        // headers must never come back on the API-key path — they'd
        // impersonate Claude Code to Anthropic's servers.
        assert!(h.get("anthropic-dangerous-direct-browser-access").is_none());
        assert!(h.get("x-app").is_none());
    }

    /// Review of the pre-release build (S5): reqwest keeps `x-api-key` across a redirect
    /// to another host. The Anthropic client follows none.
    #[tokio::test]
    async fn the_anthropic_client_follows_no_redirect() {
        use std::io::{Read, Write};
        let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let to = elsewhere.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let log = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = elsewhere.accept() {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let from = first.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = first.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{to}/v1/messages\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        let c = AnthropicClient::new(
            AnthropicAuth::ApiKey("sk-ant-api03-FAKE-abcd".into()),
            "m",
            16,
        )
        .unwrap();
        let req = c
            .auth
            .apply(
                c.client
                    .post(format!("http://127.0.0.1:{from}/v1/messages")),
            )
            .await
            .unwrap();
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 307);
        assert!(
            !seen.lock().unwrap().contains("FAKE"),
            "{}",
            seen.lock().unwrap()
        );
    }

    #[test]
    fn anthropic_display_name_for_api_key() {
        let api =
            AnthropicClient::new(AnthropicAuth::ApiKey("k".into()), "claude-sonnet-4-6", 1024)
                .unwrap();
        assert_eq!(api.display_name(), "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn ollama_client_builds() {
        assert!(OllamaClient::new("http://localhost:11434", "llama3.2", 4096, None).is_ok());
    }

    #[test]
    fn ollama_display_name() {
        let c = OllamaClient::new("http://localhost:11434", "mistral", 4096, None).unwrap();
        assert_eq!(c.display_name(), "ollama/mistral");
    }

    #[test]
    fn openai_compat_client_builds_no_key() {
        assert!(OpenAICompatClient::new("http://localhost:4000", "gpt-4o", 4096, None).is_ok());
    }

    #[test]
    fn openai_compat_client_builds_with_key() {
        assert!(OpenAICompatClient::new(
            "https://api.openai.com",
            "gpt-4o",
            4096,
            Some("sk-dummy".into())
        )
        .is_ok());
    }

    /// One HTTP answer on a loopback port; returns the root URL.
    async fn serve_once(content_type: &'static str, body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        });
        root
    }

    async fn stream_from(root: String) -> (ApiResponse, String) {
        let c = OpenAICompatClient::new(root, "m", 16, None).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let msgs = vec![Message {
            role: Role::User,
            content: "hi".into(),
            images: vec![],
        }];
        let resp = c.stream("sys", &msgs, tx).await.unwrap();
        let mut seen = String::new();
        while let Ok(StreamChunk::Token(t)) = rx.try_recv() {
            seen.push_str(&t);
        }
        (resp, seen)
    }

    #[tokio::test]
    async fn openai_compat_stream_accepts_a_plain_json_answer() {
        // The answer of a server that ignores `stream: true`.
        let body = json!({"choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "Hello there"}}],
            "usage": {"prompt_tokens": 14, "completion_tokens": 3}});
        let root = serve_once("application/json", body.to_string()).await;
        let (resp, seen) = stream_from(root).await;
        assert_eq!(resp.content, "Hello there");
        assert_eq!(seen, "Hello there");
        assert_eq!((resp.input_tokens, resp.output_tokens), (14, 3));
    }

    #[tokio::test]
    async fn openai_compat_stream_still_reads_sse() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n"
            .to_string();
        let root = serve_once("text/event-stream", body).await;
        let (resp, seen) = stream_from(root).await;
        assert_eq!(resp.content, "Hello");
        assert_eq!(seen, "Hello");
    }

    // ── Native tools on openai-compat ─────────────────────────────────

    /// Scripted loopback server: one answer per request, in order. Returns the
    /// root URL and the request bodies it received.
    async fn serve_script(
        answers: Vec<(u16, &'static str, String)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for (status, ctype, body) in answers {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 16 * 1024];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            let req: Value = serde_json::from_slice(&buf[h + 4..h + 4 + len])
                                .unwrap_or(Value::Null);
                            seen2.lock().unwrap().push(req);
                            break;
                        }
                    }
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                sock.write_all(head.as_bytes()).await.unwrap();
                sock.write_all(body.as_bytes()).await.unwrap();
                let _ = sock.shutdown().await;
            }
        });
        (root, seen)
    }

    fn hi() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: "hi".into(),
            images: vec![],
        }]
    }

    fn parsed(content: &str) -> Value {
        match crate::parser::parse_llm_response(content) {
            crate::parser::LlmOutput::Action { action, .. } => action,
            other => panic!("expected an action from {content:?}, got {other:?}"),
        }
    }

    /// Tools go out, `tool_calls` come back as the canonical action.
    #[tokio::test]
    async fn native_tool_calls_come_back_as_a_canonical_action() {
        let args = json!({"path":"/w/fib.py","content":"a = 1\nprint(a)\n"}).to_string();
        let body = json!({"choices":[{"index":0,"finish_reason":"tool_calls",
            "message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"WriteFile","arguments":args}}]}}],
            "usage":{"prompt_tokens":5,"completion_tokens":7}});
        let (root, seen) = serve_script(vec![(200, "application/json", body.to_string())]).await;
        let c = OpenAICompatClient::new(root, "m", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = crate::native_tools::action_tools();
        let resp = c.stream_with_tools("sys", &hi(), tools, tx).await.unwrap();
        let a = parsed(&resp.content);
        assert_eq!(a["type"], "WriteFile");
        assert_eq!(a["content"], "a = 1\nprint(a)\n");
        let req = &seen.lock().unwrap()[0];
        assert_eq!(req["tools"].as_array().map(|t| t.len()), Some(tools.len()));
        assert_eq!(req["tool_choice"], "auto");
    }

    #[tokio::test]
    async fn streamed_tool_calls_come_back_as_a_canonical_action() {
        let d1 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function",
            "function":{"name":"RunCommand","arguments":"{\"command\":"}}]}}]});
        let d2 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,
            "function":{"arguments":"\"python3\",\"args\":[\"/w/fib.py\"]}"}}]},"finish_reason":"tool_calls"}]});
        let body = format!("data: {d1}\n\ndata: {d2}\n\ndata: [DONE]\n\n");
        let (root, _seen) = serve_script(vec![(200, "text/event-stream", body)]).await;
        let c = OpenAICompatClient::new(root, "m", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = c
            .stream_with_tools("sys", &hi(), crate::native_tools::action_tools(), tx)
            .await
            .unwrap();
        let a = parsed(&resp.content);
        assert_eq!(a["type"], "RunCommand");
        assert_eq!(a["args"], json!(["/w/fib.py"]));
    }

    /// Other consumers (assistant, matrix, skeptic) call `stream`/`send` and
    /// have their own protocol: they must not receive the action tools.
    #[tokio::test]
    async fn plain_stream_sends_no_tools() {
        let body = json!({"choices":[{"message":{"content":"ok"}}]}).to_string();
        let (root, seen) = serve_script(vec![(200, "application/json", body)]).await;
        let c = OpenAICompatClient::new(root, "m", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        c.stream("sys", &hi(), tx).await.unwrap();
        assert!(seen.lock().unwrap()[0].get("tools").is_none());
    }

    /// llama.cpp without `--jinja` answers an error to `tools`. The call is
    /// replayed once without them, and the client stops sending them.
    #[tokio::test]
    async fn a_server_that_refuses_tools_falls_back_to_the_text_protocol() {
        let refusal = json!({"error":{"message":"tools param requires --jinja flag"}}).to_string();
        let ok = json!({"choices":[{"message":{"content":"<action>{\"type\":\"ReadFile\",\"path\":\"/a\"}</action>"}}]}).to_string();
        let (root, seen) = serve_script(vec![
            (500, "application/json", refusal),
            (200, "application/json", ok.clone()),
            (200, "application/json", ok),
        ])
        .await;
        let c = OpenAICompatClient::new(root, "m", 16, None).unwrap();
        let tools = crate::native_tools::action_tools();
        for _ in 0..2 {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let resp = c.stream_with_tools("sys", &hi(), tools, tx).await.unwrap();
            assert_eq!(parsed(&resp.content)["type"], "ReadFile");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].get("tools").is_some());
        assert!(seen[1].get("tools").is_none(), "the replay must drop tools");
        assert!(
            seen[2].get("tools").is_none(),
            "the refusal must be remembered"
        );
    }

    #[test]
    fn a_null_content_with_tool_calls_is_not_an_error() {
        let r = json!({"choices":[{"message":{"content":null,"tool_calls":[
            {"function":{"name":"ReadFile","arguments":"{\"path\":\"/a\"}"}}]}}]});
        let resp = extract_openai_compat_response(&r).unwrap();
        assert_eq!(parsed(&resp.content)["path"], "/a");
    }

    #[test]
    fn openai_compat_display_name() {
        let c = OpenAICompatClient::new("http://localhost:4000", "mistral", 4096, None).unwrap();
        assert_eq!(
            c.display_name(),
            "openai-compat/mistral @ http://localhost:4000"
        );
    }

    /// The header, the timeout line and the matrix error screen read
    /// `display_name`; a password in `base_url` stays off all three.
    #[test]
    fn openai_compat_display_name_hides_url_credentials() {
        let c = OpenAICompatClient::new("http://user:hunter2@localhost:4000/v1", "m", 16, None)
            .unwrap();
        let shown = c.display_name();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.ends_with("@ http://***@localhost:4000"), "{shown}");
    }

    #[test]
    fn usage_tracker_summary_format() {
        let mut tracker = UsageTracker::new("claude-sonnet-4");
        tracker.total_input_tokens = 1000;
        tracker.total_output_tokens = 500;
        tracker.total_requests = 3;
        let summary = tracker.summary(3.0, 15.0);
        assert!(summary.contains("Requests: 3"));
        assert!(summary.contains("Input: 1000"));
        assert!(summary.contains("Output: 500"));
        assert!(summary.contains("claude-sonnet-4"));
    }

    #[test]
    fn normalize_base_url_strips_v1() {
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1".into()),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/".into()),
            "https://api.openai.com"
        );
    }

    #[test]
    fn normalize_base_url_preserves_clean() {
        assert_eq!(
            normalize_base_url("https://api.openai.com".into()),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_base_url("http://localhost:4000".into()),
            "http://localhost:4000"
        );
    }

    #[test]
    fn normalize_base_url_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url("http://localhost:4000/".into()),
            "http://localhost:4000"
        );
    }

    // ── Multimodal: build_screenshot_message ─────────────────────────────────

    // ── Multimodal: backend message converters ───────────────────────────────

    fn img(media: &str, data: &str) -> crate::conversation::ImageAttachment {
        crate::conversation::ImageAttachment {
            media_type: media.into(),
            data: data.into(),
        }
    }

    #[test]
    fn message_to_anthropic_value_text_only_uses_string_content() {
        let m = Message {
            role: Role::User,
            content: "hi".into(),
            images: vec![],
        };
        let v = message_to_anthropic_value(&m);
        assert_eq!(v["role"], "user");
        assert_eq!(
            v["content"], "hi",
            "text-only must keep string content shape"
        );
    }

    #[test]
    fn message_to_anthropic_value_with_images_emits_blocks() {
        let m = Message {
            role: Role::User,
            content: "look".into(),
            images: vec![img("image/png", "AAAA"), img("image/jpeg", "BBBB")],
        };
        let v = message_to_anthropic_value(&m);
        let blocks = v["content"]
            .as_array()
            .expect("content is array for vision");
        assert_eq!(blocks.len(), 3, "1 text + 2 images");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "look");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
        assert_eq!(blocks[2]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[2]["source"]["data"], "BBBB");
    }

    #[test]
    fn message_to_ollama_value_text_only() {
        let m = Message {
            role: Role::Assistant,
            content: "ok".into(),
            images: vec![],
        };
        let v = message_to_ollama_value(&m);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "ok");
        assert!(v.get("images").is_none(), "no images field for text-only");
    }

    #[test]
    fn message_to_ollama_value_with_images_emits_flat_array() {
        let m = Message {
            role: Role::User,
            content: "[Screenshot]".into(),
            images: vec![img("image/png", "PNG_DATA"), img("image/jpeg", "JPG_DATA")],
        };
        let v = message_to_ollama_value(&m);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "[Screenshot]");
        let arr = v["images"].as_array().expect("images is array");
        assert_eq!(arr.len(), 2);
        // Ollama receives raw base64 strings only (no MIME, no wrapper)
        assert_eq!(arr[0], "PNG_DATA");
        assert_eq!(arr[1], "JPG_DATA");
    }

    #[test]
    fn message_to_openai_value_text_only() {
        let m = Message {
            role: Role::User,
            content: "ping".into(),
            images: vec![],
        };
        let v = message_to_openai_value(&m);
        assert_eq!(v["role"], "user");
        assert_eq!(
            v["content"], "ping",
            "text-only must stay as string, not blocks"
        );
    }

    #[test]
    fn message_to_openai_value_with_images_emits_data_url_blocks() {
        let m = Message {
            role: Role::User,
            content: "compare".into(),
            images: vec![img("image/png", "AAAA"), img("image/webp", "WWWW")],
        };
        let v = message_to_openai_value(&m);
        let blocks = v["content"].as_array().expect("vision => array of blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "compare");
        assert_eq!(blocks[1]["type"], "image_url");
        assert_eq!(
            blocks[1]["image_url"]["url"], "data:image/png;base64,AAAA",
            "OpenAI requires data-URL form"
        );
        assert_eq!(
            blocks[2]["image_url"]["url"], "data:image/webp;base64,WWWW",
            "MIME must propagate per-image"
        );
    }

    #[test]
    fn assistant_role_propagates_in_all_backends() {
        let m = Message {
            role: Role::Assistant,
            content: "reply".into(),
            images: vec![],
        };
        assert_eq!(message_to_anthropic_value(&m)["role"], "assistant");
        assert_eq!(message_to_ollama_value(&m)["role"], "assistant");
        assert_eq!(message_to_openai_value(&m)["role"], "assistant");
    }

    // ── Ollama, native tools and num_ctx ──────────────────────────────

    /// One request read whole (headers, then `content-length` bytes of JSON).
    async fn read_json_request(sock: &mut tokio::net::TcpStream) -> Value {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                return Value::Null;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(h) = text.find("\r\n\r\n") {
                let len = text[..h]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= h + 4 + len {
                    return serde_json::from_slice(&buf[h + 4..h + 4 + len]).unwrap_or(Value::Null);
                }
            }
        }
    }

    /// A fake Ollama `/api/chat`. A 200 answer streams like Ollama: chunked
    /// `application/x-ndjson`, one JSON object per line, one write per line,
    /// `done:true` alone on the last line. Other statuses answer one JSON body.
    async fn fake_ollama(
        answers: Vec<(u16, Vec<Value>)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for (status, lines) in answers {
                let (mut sock, _) = listener.accept().await.unwrap();
                let req = read_json_request(&mut sock).await;
                seen2.lock().unwrap().push(req);
                if status != 200 {
                    let body = lines[0].to_string();
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    sock.write_all(head.as_bytes()).await.unwrap();
                    sock.write_all(body.as_bytes()).await.unwrap();
                    let _ = sock.shutdown().await;
                    continue;
                }
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                for line in lines {
                    let line = format!("{line}\n");
                    let frame = format!("{:x}\r\n{line}\r\n", line.len());
                    sock.write_all(frame.as_bytes()).await.unwrap();
                    sock.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                sock.write_all(b"0\r\n\r\n").await.unwrap();
                let _ = sock.shutdown().await;
            }
        });
        (root, seen)
    }

    fn ollama_line(message: Value) -> Value {
        json!({"model": "qwen3:4b", "created_at": "2026-10-16T21:00:00Z",
               "message": message, "done": false})
    }

    /// The last line of an Ollama stream: empty content, the counts.
    fn ollama_done() -> Value {
        json!({"model": "qwen3:4b", "created_at": "2026-10-16T21:00:01Z",
               "message": {"role": "assistant", "content": ""},
               "done_reason": "stop", "done": true,
               "prompt_eval_count": 169, "eval_count": 15})
    }

    fn ollama_text(text: &str) -> Vec<Value> {
        vec![
            ollama_line(json!({"role": "assistant", "content": text})),
            ollama_done(),
        ]
    }

    /// Captures what `warn!` prints during a test (current-thread runtime).
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 16/10 capture: `options` held `num_predict` alone, no `tools`.
    #[tokio::test]
    async fn the_ollama_request_carries_the_tools_and_num_ctx() {
        let (root, seen) = fake_ollama(vec![(200, ollama_text("ok"))]).await;
        let c = OllamaClient::new(root, "qwen3:4b", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = crate::native_tools::action_tools();
        c.stream_with_tools("sys", &hi(), tools, tx).await.unwrap();
        let req = &seen.lock().unwrap()[0];
        assert_eq!(req["tools"].as_array(), Some(&tools.to_vec()), "{req}");
        assert_eq!(req["options"]["num_ctx"], 32768, "{req}");
        assert_eq!(req["options"]["num_predict"], 16, "{req}");
        assert_eq!(req["stream"], true);
    }

    #[tokio::test]
    async fn an_ollama_streamed_tool_call_becomes_an_action() {
        let call = ollama_line(json!({"role": "assistant", "content": "",
            "tool_calls": [{"id": "call_1", "function": {"index": 0, "name": "RunCommand",
                "arguments": {"command": "python3", "args": ["fib.py"]}}}]}));
        let lines = vec![
            ollama_line(json!({"role": "assistant", "content": "", "thinking": "run it"})),
            call,
            ollama_done(),
        ];
        let (root, _seen) = fake_ollama(vec![(200, lines)]).await;
        let c = OllamaClient::new(root, "qwen3:4b", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = c
            .stream_with_tools("sys", &hi(), crate::native_tools::action_tools(), tx)
            .await
            .unwrap();
        let a = parsed(&resp.content);
        assert_eq!(a["type"], "RunCommand");
        assert_eq!(a["command"], "python3");
        assert_eq!(a["args"], json!(["fib.py"]));
        assert_eq!((resp.input_tokens, resp.output_tokens), (169, 15));
    }

    /// Ollama answers 400 "<model> does not support tools" (server/images.go,
    /// CheckCapabilities). Replayed once without tools, remembered, and said.
    #[tokio::test]
    async fn an_ollama_model_without_tools_falls_back_and_says_so() {
        let refusal =
            json!({"error": "registry.ollama.ai/library/gemma3:1b does not support tools"});
        let action = "<action>{\"type\":\"ReadFile\",\"path\":\"a.txt\"}</action>";
        let (root, seen) = fake_ollama(vec![
            (400, vec![refusal]),
            (200, ollama_text(action)),
            (200, ollama_text(action)),
        ])
        .await;
        let cap = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let c = OllamaClient::new(root, "gemma3:1b", 16, None).unwrap();
        let tools = crate::native_tools::action_tools();
        for _ in 0..2 {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let resp = c.stream_with_tools("sys", &hi(), tools, tx).await.unwrap();
            assert_eq!(parsed(&resp.content)["type"], "ReadFile");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].get("tools").is_some());
        assert!(seen[1].get("tools").is_none(), "the replay must drop tools");
        assert!(
            seen[2].get("tools").is_none(),
            "the refusal must be remembered"
        );
        let said = String::from_utf8_lossy(&cap.0.lock().unwrap()).to_string();
        assert!(said.contains("does not support tools"), "{said}");
        assert!(said.contains("text"), "{said}");
    }

    /// Only the explicit refusal turns tools off; a missing model stays an error.
    #[tokio::test]
    async fn another_ollama_error_is_not_taken_for_a_tools_refusal() {
        let missing = json!({"error": "model \"qwen3:4b\" not found, try pulling it first"});
        let (root, seen) = fake_ollama(vec![(404, vec![missing])]).await;
        let c = OllamaClient::new(root, "qwen3:4b", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = c
            .stream_with_tools("sys", &hi(), crate::native_tools::action_tools(), tx)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
        assert_eq!(seen.lock().unwrap().len(), 1, "no replay");
    }

    #[tokio::test]
    async fn an_ollama_stream_is_read_line_by_line() {
        let lines = vec![
            ollama_line(json!({"role": "assistant", "content": "", "thinking": "hmm"})),
            ollama_line(json!({"role": "assistant", "content": "Hel"})),
            ollama_line(json!({"role": "assistant", "content": "lo"})),
            ollama_done(),
        ];
        let (root, seen) = fake_ollama(vec![(200, lines)]).await;
        let c = OllamaClient::new(root, "qwen3:4b", 16, None).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = c.stream("sys", &hi(), tx).await.unwrap();
        assert_eq!(resp.content, "Hello");
        assert_eq!((resp.input_tokens, resp.output_tokens), (169, 15));
        let mut tokens = String::new();
        let mut thought = false;
        while let Ok(ch) = rx.try_recv() {
            match ch {
                StreamChunk::Token(t) => tokens.push_str(&t),
                StreamChunk::Thinking(_) => thought = true,
            }
        }
        assert_eq!(tokens, "Hello");
        assert!(thought);
        assert!(
            seen.lock().unwrap()[0].get("tools").is_none(),
            "plain stream: no tools"
        );
    }

    /// An error line mid-stream is an error, not an empty answer.
    #[tokio::test]
    async fn an_ollama_error_line_mid_stream_is_an_error() {
        let lines = vec![
            ollama_line(json!({"role": "assistant", "content": "par"})),
            json!({"error": "error parsing tool call: raw='{\"name\"'"}),
        ];
        let (root, _seen) = fake_ollama(vec![(200, lines)]).await;
        let c = OllamaClient::new(root, "qwen3:4b", 16, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = c
            .stream_with_tools("sys", &hi(), crate::native_tools::action_tools(), tx)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("error parsing tool call"),
            "{err:#}"
        );
    }
}
