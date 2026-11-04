//! The canonical model-invocation boundary.
//!
//! Answers exactly one question — *ask this model for the next response* — and
//! nothing else. No parsing, no history mutation, no confirmation, no
//! dispatch, no loop detection; those belong to the iteration driver that will
//! call this.
//!
//! ## Why this exists
//!
//! Five loops invoke a model today and each hand-rolls the same sequence:
//! build a channel, `tokio::select!` over it, classify chunks, accumulate,
//! drain. They disagree on every detail. Measured at this HEAD:
//!
//! | Loop | Timeout | Cancellation | Drains after resolve | Thinking chunks | LLM-error handling |
//! |---|---|---|---|---|---|
//! | CLI `agent_loop.rs:444` | 180 s | SIGINT, drops the future | yes | shown | classified: fatal vs transient |
//! | legacy TUI `tui_mode.rs:777` | none | `Notify`, stream only | yes | shown | any error breaks the turn |
//! | TUI `tui/runner.rs:1001` | none | none (`Cancel` is a stub) | no | **discarded** | none |
//! | Matrix `matrix_mode.rs:1173` | none | keypress, stream only | yes | counted as bytes | **`sleep(2s); continue` forever** |
//! | serve `serve.rs:712`, `:1222` | none | none | n/a (buffered) | n/a | error event, return |
//!
//! Matrix retries a permanent 401 indefinitely. Only the CLI distinguishes a
//! fatal model error from a transient one. That taxonomy is the substance of
//! this module.
//!
//! ## Retry ownership
//!
//! The provider layer already retries, but unevenly — verified by reading
//! `api.rs`, not its documentation:
//!
//! | Backend | `send()` | `stream()` — what production actually calls |
//! |---|---|---|
//! | Anthropic | retries 429/5xx, ≤5 | **retries 429/5xx, ≤5** (`api.rs:819`) |
//! | OpenAI-compat | retries (`api.rs:1609`) | **none** (`api.rs:1710`) |
//! | Ollama | none | none |
//! | Subprocess (`claude-code`, `codex`) | none | none (no override) |
//!
//! So in production only Anthropic retries anything. The boundary this module
//! draws:
//!
//! - **Provider layer owns transport retries** — HTTP 429 and 5xx, with
//!   `Retry-After`, where a backend implements them.
//! - **This layer owns the semantic retry** — it retries only what surfaces
//!   *past* the provider, and only when the classification says the failure
//!   could plausibly resolve itself.
//!
//! The composition is stated rather than accidental: an Anthropic rate limit
//! can consume up to 5 provider attempts before surfacing here, and this layer
//! then adds at most [`RetryPolicy::max_attempts`] more. Worst case is finite
//! and knowable; there is no path to an unbounded loop.

use std::time::Duration;

use crate::agent::cancel::{CancelToken, Cancellable};
use crate::api::{ApiResponse, InternalRetry, LlmBackend, StreamChunk};
use crate::conversation::Message;

// ── Errors ───────────────────────────────────────────────────────────────────

/// What went wrong, in terms a caller can act on.
///
/// Every backend returns `anyhow::Error` carrying a formatted string — there
/// is no structured error type anywhere in `api.rs`. Classification therefore
/// inspects the error, and that inspection is confined to
/// [`classify`] so no other code grows a dependency on provider wording. The
/// strings matched are ones **this codebase emits** (`api.rs:746`, `:752`,
/// `:1369`…), not a third party's, which is what makes the matching
/// defensible; the underlying error is preserved on every variant so
/// diagnostics never lose the original text.
#[derive(Debug)]
pub enum ModelError {
    /// The caller cancelled. Never retried — retrying a cancellation is
    /// disobeying it.
    Cancelled,
    /// The call exceeded its deadline.
    Timeout { source: anyhow::Error },
    /// The provider asked us to slow down. `retry_after` is populated when the
    /// provider told us how long.
    RateLimited {
        retry_after: Option<Duration>,
        source: anyhow::Error,
    },
    /// A 5xx or an explicit "try later". Plausibly transient.
    TemporarilyUnavailable { source: anyhow::Error },
    /// Credentials are wrong, missing or expired. Retrying cannot fix it.
    Authentication { source: anyhow::Error },
    /// Authenticated but not allowed. Retrying cannot fix it.
    Permission { source: anyhow::Error },
    /// The request itself is malformed or names something that does not exist
    /// — a bad model id, an unsupported parameter.
    InvalidRequest { source: anyhow::Error },
    /// The conversation no longer fits. Retrying the same request cannot fix
    /// it; the caller must compact first.
    ContextLengthExceeded { source: anyhow::Error },
    /// The provider failed in a way that is not one of the above.
    ProviderError { source: anyhow::Error },
    /// The request never reached the provider.
    Transport { source: anyhow::Error },
}

impl ModelError {
    /// Whether trying the same request again could plausibly succeed, given
    /// what the backend already retried itself.
    ///
    /// Deliberately conservative on two axes. A category is retryable only
    /// when the failure is about the *moment* rather than the request — and
    /// only when the provider layer does not already own it. Retrying a rate
    /// limit that Anthropic already retried five times would turn one 429 into
    /// fifteen network requests.
    pub fn is_retryable_after(&self, provider: InternalRetry) -> bool {
        if provider == InternalRetry::RateLimitAndServerErrors
            && matches!(
                self,
                ModelError::RateLimited { .. } | ModelError::TemporarilyUnavailable { .. }
            )
        {
            return false;
        }
        self.is_retryable()
    }

    /// Whether the class is transient at all, ignoring provider ownership.
    pub fn is_retryable(&self) -> bool {
        match self {
            ModelError::RateLimited { .. }
            | ModelError::TemporarilyUnavailable { .. }
            | ModelError::Transport { .. }
            | ModelError::Timeout { .. } => true,

            // Retrying any of these repeats a request that cannot succeed.
            // `ContextLengthExceeded` is listed here explicitly: the caller
            // must compact the conversation first, and a blind retry is the
            // behaviour this module exists to remove.
            ModelError::Cancelled
            | ModelError::Authentication { .. }
            | ModelError::Permission { .. }
            | ModelError::InvalidRequest { .. }
            | ModelError::ContextLengthExceeded { .. }
            | ModelError::ProviderError { .. } => false,
        }
    }

    /// The original error, for logs and for a human. `None` for `Cancelled`,
    /// which has no underlying failure.
    pub fn source(&self) -> Option<&anyhow::Error> {
        match self {
            ModelError::Cancelled => None,
            ModelError::Timeout { source }
            | ModelError::RateLimited { source, .. }
            | ModelError::TemporarilyUnavailable { source }
            | ModelError::Authentication { source }
            | ModelError::Permission { source }
            | ModelError::InvalidRequest { source }
            | ModelError::ContextLengthExceeded { source }
            | ModelError::ProviderError { source }
            | ModelError::Transport { source } => Some(source),
        }
    }

    /// Short category name, for events and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            ModelError::Cancelled => "cancelled",
            ModelError::Timeout { .. } => "timeout",
            ModelError::RateLimited { .. } => "rate_limited",
            ModelError::TemporarilyUnavailable { .. } => "temporarily_unavailable",
            ModelError::Authentication { .. } => "authentication",
            ModelError::Permission { .. } => "permission",
            ModelError::InvalidRequest { .. } => "invalid_request",
            ModelError::ContextLengthExceeded { .. } => "context_length_exceeded",
            ModelError::ProviderError { .. } => "provider_error",
            ModelError::Transport { .. } => "transport",
        }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.source() {
            Some(e) => write!(f, "{}: {e}", self.kind()),
            None => write!(f, "{}", self.kind()),
        }
    }
}

/// Turn a backend error into a category.
///
/// **The only place in the engine that reads provider error text.** Structured
/// signal is preferred where it survives: a `reqwest::Error` reaching us
/// through `with_context` still knows whether it timed out or failed to
/// connect, and that is checked first. The rest of `api.rs` builds fresh
/// `anyhow!` messages with no source, so those are matched on the wording this
/// repository itself emits.
pub fn classify(err: anyhow::Error) -> ModelError {
    // 1. Structured, where the chain preserved it.
    for cause in err.chain() {
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if re.is_timeout() {
                return ModelError::Timeout { source: err };
            }
            if re.is_connect() || re.is_request() {
                return ModelError::Transport { source: err };
            }
        }
    }

    let text = format!("{err:#}").to_lowercase();

    // 2. Our own wording. Ordered most-specific first: a context-length error
    //    also mentions "invalid request" on some providers.
    if text.contains("context length")
        || text.contains("context_length_exceeded")
        || text.contains("too many tokens")
        || text.contains("prompt is too long")
        // llama.cpp server (`exceed_context_size_error`), measured 2026-10-12.
        || text.contains("exceed_context_size")
        || text.contains("exceeds the available context")
    {
        return ModelError::ContextLengthExceeded { source: err };
    }
    if text.contains("authentication failed")
        || text.contains("invalid api key")
        || text.contains("unauthorized")
        || has_code(&text, "401")
    {
        return ModelError::Authentication { source: err };
    }
    if text.contains("permission") || text.contains("forbidden") || has_code(&text, "403") {
        return ModelError::Permission { source: err };
    }
    if text.contains("rate limit") || text.contains("rate-limited") || has_code(&text, "429") {
        return ModelError::RateLimited {
            retry_after: retry_after_from_text(&text),
            source: err,
        };
    }
    if text.contains("overloaded")
        || text.contains("service unavailable")
        || text.contains("temporarily")
        || has_code(&text, "500")
        || has_code(&text, "502")
        || has_code(&text, "503")
        || has_code(&text, "529")
    {
        return ModelError::TemporarilyUnavailable { source: err };
    }
    if text.contains("model_not_found")
        || text.contains("not_found_error")
        || text.contains("invalid_request")
        || has_code(&text, "404")
        || has_code(&text, "400")
    {
        return ModelError::InvalidRequest { source: err };
    }
    if text.contains("timed out") || text.contains("timeout") {
        return ModelError::Timeout { source: err };
    }
    if text.contains("connection") || text.contains("dns") || text.contains("network") {
        return ModelError::Transport { source: err };
    }
    ModelError::ProviderError { source: err }
}

/// `code` as a whole number in `text`. A substring match read "35401 tokens"
/// as HTTP 401 (bench E1 2026-10-12).
fn has_code(text: &str, code: &str) -> bool {
    let b = text.as_bytes();
    text.match_indices(code).any(|(i, _)| {
        let j = i + code.len();
        (i == 0 || !b[i - 1].is_ascii_digit()) && (j == b.len() || !b[j].is_ascii_digit())
    })
}

/// Recover a `retry-after` hint the provider layer embedded in its message.
fn retry_after_from_text(text: &str) -> Option<Duration> {
    let idx = text.find("retry after ")? + "retry after ".len();
    let rest = &text[idx..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok().map(Duration::from_secs)
}

// ── Request and response ─────────────────────────────────────────────────────

/// One model call.
///
/// `system` stays a separate field rather than a message, mirroring the
/// providers that treat it as a distinct channel. A provider adapter may map
/// it into a role, but nothing here promotes conversation text into system
/// authority — see `system_text_is_never_taken_from_messages`.
pub struct ModelRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    /// Native tool definitions; empty sends none.
    pub tools: &'a [serde_json::Value],
}

/// What a call cost, when the provider said.
///
/// Every field is optional and `None` means *unknown*. A missing cost is not
/// `$0.00`: three of the five loops sum raw integers and drop cache tokens
/// entirely, and TUI reports a hardcoded zero — turning "we do not know" into
/// "it was free" is how a budget silently stops binding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    /// `None` when no pricing is known for this model. Never defaulted to 0.
    pub cost_usd: Option<f64>,
}

/// A successful model response.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub content: String,
    pub usage: Usage,
    pub stop_reason: String,
    /// Backend display name, e.g. `anthropic/claude-sonnet-5`.
    pub provider: String,
    /// How many attempts this took, including the successful one.
    pub attempts: u32,
}

// ── Retry policy ─────────────────────────────────────────────────────────────

/// Bounded, deterministic backoff.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first. Hard bound — there is no path that
    /// exceeds it, which is the property Matrix's `sleep(2s); continue` lacked
    /// until it called [`invoke`].
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Bound on the whole call, attempts and backoff included; `None` leaves
    /// it to the transport. Elapsed, the call ends as [`ModelError::Timeout`].
    pub deadline: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // Small on purpose: the provider layer already spends up to 5
            // attempts on transport failures for Anthropic, and stacking two
            // generous budgets is how a "bounded" retry becomes a minutes-long
            // stall.
            max_attempts: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            deadline: None,
        }
    }
}

impl RetryPolicy {
    /// Backoff before attempt `n` (1-based), doubling and capped.
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let doubled = self
            .initial_backoff
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)));
        doubled.min(self.max_backoff)
    }
}

/// Narrow seam so tests do not spend real seconds sleeping.
#[async_trait::async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// Real time.
pub struct TokioSleeper;

#[async_trait::async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ── The invoker ──────────────────────────────────────────────────────────────

/// Ask the model for the next response.
///
/// Streaming chunks go to `chunks` as they arrive. `stream()` is used
/// unconditionally because that is what every production loop calls, and the
/// trait's default implementation falls back to `send()` for backends that do
/// not stream — one path rather than two.
pub async fn invoke(
    backend: &dyn LlmBackend,
    request: ModelRequest<'_>,
    chunks: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    policy: &RetryPolicy,
    cancel: &CancelToken,
    sleeper: &dyn Sleeper,
) -> Result<ModelResponse, ModelError> {
    let attempts = invoke_attempts(backend, request, chunks, policy, cancel, sleeper);
    let Some(limit) = policy.deadline else {
        return attempts.await;
    };
    match tokio::time::timeout(limit, attempts).await {
        Ok(result) => result,
        Err(_) => Err(ModelError::Timeout {
            source: anyhow::anyhow!(
                "no answer within {} from {}",
                human_duration(limit),
                backend.display_name()
            ),
        }),
    }
}

/// `2 s`, or `50 ms` below a second.
fn human_duration(d: Duration) -> String {
    if d >= Duration::from_secs(1) && d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// [`invoke`] without the deadline: the bounded attempts and their backoff.
async fn invoke_attempts(
    backend: &dyn LlmBackend,
    request: ModelRequest<'_>,
    chunks: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
    policy: &RetryPolicy,
    cancel: &CancelToken,
    sleeper: &dyn Sleeper,
) -> Result<ModelResponse, ModelError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        // Checked before every attempt, so a cancellation that arrived during
        // the previous backoff cannot buy one more call.
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled);
        }

        let call = backend.stream_with_tools(
            request.system,
            request.messages,
            request.tools,
            chunks.clone(),
        );
        let outcome = match cancel.run(call).await {
            Cancellable::Cancelled => return Err(ModelError::Cancelled),
            Cancellable::Completed(r) => r,
        };

        let err = match outcome {
            Ok(response) => {
                return Ok(ModelResponse {
                    content: response.content.clone(),
                    usage: usage_of(&response),
                    stop_reason: response.stop_reason.clone(),
                    provider: backend.display_name(),
                    attempts: attempt,
                })
            }
            Err(e) => classify(e),
        };

        if !err.is_retryable_after(backend.internal_retry()) || attempt >= policy.max_attempts {
            return Err(err);
        }

        // Honour the provider's own hint when it gave one, otherwise back off.
        //
        // A hint longer than the policy allows is **not** clamped: retrying
        // sooner than a rate-limited provider asked is how a client turns a
        // pause into a ban. The existing provider layer makes the same choice
        // (`api.rs:751` refuses rather than shortening), so an over-long wait
        // surfaces as a failure the caller can report.
        let wait = match &err {
            ModelError::RateLimited {
                retry_after: Some(d),
                ..
            } => {
                if *d > policy.max_backoff {
                    tracing::warn!(
                        wait_s = d.as_secs(),
                        cap_s = policy.max_backoff.as_secs(),
                        "provider asked for a longer wait than this turn allows — not retrying"
                    );
                    return Err(err);
                }
                *d
            }
            _ => policy.backoff_for(attempt),
        };
        tracing::warn!(
            kind = err.kind(),
            attempt,
            max = policy.max_attempts,
            wait_ms = wait.as_millis() as u64,
            "model call failed, retrying"
        );

        // Cancellable backoff. Sleeping through a cancellation would make the
        // user wait out a delay for a call that will never happen.
        if cancel.run(sleeper.sleep(wait)).await.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
    }
}

/// Map a provider response onto the canonical usage shape.
///
/// Token counts are reported as given. Cost is deliberately left `None`: the
/// only model→rate table in the repository is marooned inside the CLI loop
/// (`agent_loop.rs:1411-1430`) and covers three models, so this layer would be
/// inventing a number for everything else. The driver will supply pricing.
fn usage_of(response: &ApiResponse) -> Usage {
    Usage {
        input_tokens: Some(response.input_tokens),
        output_tokens: Some(response.output_tokens),
        cache_write_tokens: Some(response.cache_creation_input_tokens),
        cache_read_tokens: Some(response.cache_read_input_tokens),
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// Deterministic backend: replays a script of outcomes and counts calls.
    struct ScriptedBackend {
        script: Mutex<Vec<Result<ApiResponse, String>>>,
        calls: Arc<AtomicU32>,
        /// Blocks forever on the first call, for cancellation tests.
        hang: bool,
    }

    impl ScriptedBackend {
        fn new(script: Vec<Result<ApiResponse, String>>) -> (Self, Arc<AtomicU32>) {
            let calls = Arc::new(AtomicU32::new(0));
            (
                Self {
                    script: Mutex::new(script),
                    calls: calls.clone(),
                    hang: false,
                },
                calls,
            )
        }

        fn hanging() -> (Self, Arc<AtomicU32>) {
            let calls = Arc::new(AtomicU32::new(0));
            (
                Self {
                    script: Mutex::new(vec![]),
                    calls: calls.clone(),
                    hang: true,
                },
                calls,
            )
        }
    }

    fn ok_response(content: &str) -> ApiResponse {
        ApiResponse {
            content: content.into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 4,
            stop_reason: "end_turn".into(),
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for ScriptedBackend {
        async fn send(&self, _system: &str, _messages: &[Message]) -> anyhow::Result<ApiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                std::future::pending::<()>().await;
            }
            let next = self.script.lock().unwrap().remove(0);
            next.map_err(|e| anyhow::anyhow!(e))
        }
        fn display_name(&self) -> String {
            "scripted/test".into()
        }
    }

    /// Records what it was asked to wait, and returns instantly.
    #[derive(Default)]
    struct RecordingSleeper {
        waits: Mutex<Vec<Duration>>,
    }

    #[async_trait::async_trait]
    impl Sleeper for RecordingSleeper {
        async fn sleep(&self, duration: Duration) {
            self.waits.lock().unwrap().push(duration);
        }
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            deadline: None,
        }
    }

    /// The deadline bounds the whole call and ends a silent model as a
    /// `Timeout` naming the model and the bound; without it, nothing changes.
    #[tokio::test]
    async fn a_deadline_ends_a_silent_call_as_a_timeout() {
        let (b, calls) = ScriptedBackend::hanging();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let policy = RetryPolicy {
            deadline: Some(Duration::from_millis(50)),
            ..fast_policy()
        };
        let started = std::time::Instant::now();
        let request = ModelRequest {
            system: "sys",
            messages: &[],
            tools: &[],
        };
        let e = invoke(&b, request, tx, &policy, &CancelToken::new(), &TokioSleeper)
            .await
            .expect_err("a silent model cannot answer");
        assert_eq!(e.kind(), "timeout");
        assert_eq!(
            e.to_string(),
            "timeout: no answer within 50 ms from scripted/test"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(human_duration(Duration::from_secs(300)), "300 s");

        let (b, _) =
            ScriptedBackend::new(vec![Err("503 overloaded".into()), Ok(ok_response("ok"))]);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default()).await;
        assert_eq!(r.expect("no deadline, retried").attempts, 2);
    }

    async fn run(
        backend: &ScriptedBackend,
        cancel: &CancelToken,
        sleeper: &dyn Sleeper,
    ) -> Result<ModelResponse, ModelError> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        invoke(
            backend,
            ModelRequest {
                system: "sys",
                messages: &[],
                tools: &[],
            },
            tx,
            &fast_policy(),
            cancel,
            sleeper,
        )
        .await
    }

    #[tokio::test]
    async fn an_immediate_success_is_returned() {
        let (b, calls) = ScriptedBackend::new(vec![Ok(ok_response("hello"))]);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect("success");
        assert_eq!(r.content, "hello");
        assert_eq!(r.attempts, 1);
        assert_eq!(r.provider, "scripted/test");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn usage_metadata_is_returned() {
        let (b, _) = ScriptedBackend::new(vec![Ok(ok_response("x"))]);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .unwrap();
        assert_eq!(r.usage.input_tokens, Some(10));
        assert_eq!(r.usage.output_tokens, Some(20));
        assert_eq!(r.usage.cache_write_tokens, Some(3));
        assert_eq!(r.usage.cache_read_tokens, Some(4));
    }

    /// Unknown must stay unknown. Turning it into `$0.00` is how a budget
    /// stops binding without anyone noticing.
    #[tokio::test]
    async fn unknown_cost_stays_unknown() {
        let (b, _) = ScriptedBackend::new(vec![Ok(ok_response("x"))]);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .unwrap();
        assert_eq!(r.usage.cost_usd, None, "no pricing known ⇒ not zero");
    }

    #[tokio::test]
    async fn a_transient_error_is_retried_then_succeeds() {
        let (b, calls) = ScriptedBackend::new(vec![
            Err("service unavailable (503)".into()),
            Ok(ok_response("second time")),
        ]);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect("recovers");
        assert_eq!(r.content, "second time");
        assert_eq!(r.attempts, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_rate_limit_is_retried_and_honours_the_hint() {
        let (b, calls) = ScriptedBackend::new(vec![
            Err("rate limit reached, retry after 2 seconds".into()),
            Ok(ok_response("ok")),
        ]);
        let sleeper = RecordingSleeper::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_secs(30),
            deadline: None,
        };
        let r = invoke(
            &b,
            ModelRequest {
                system: "sys",
                messages: &[],
                tools: &[],
            },
            tx,
            &policy,
            &CancelToken::new(),
            &sleeper,
        )
        .await
        .expect("recovers");
        assert_eq!(r.attempts, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            sleeper.waits.lock().unwrap().as_slice(),
            &[Duration::from_secs(2)],
            "the provider's own hint wins over the backoff curve"
        );
    }

    /// A wait longer than the turn allows is refused rather than shortened —
    /// retrying sooner than a rate-limited provider asked turns a pause into
    /// a ban.
    #[tokio::test]
    async fn an_over_long_retry_after_is_refused_not_clamped() {
        let (b, calls) = ScriptedBackend::new(vec![
            Err("rate limit, retry after 900 seconds".into()),
            Ok(ok_response("must not be reached")),
        ]);
        let sleeper = RecordingSleeper::default();
        let e = run(&b, &CancelToken::new(), &sleeper)
            .await
            .expect_err("refuses");
        assert_eq!(e.kind(), "rate_limited");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second attempt");
        assert!(sleeper.waits.lock().unwrap().is_empty(), "and no wait");
    }

    /// The bound is hard. Matrix's `sleep(2s); continue` has no such property.
    #[tokio::test]
    async fn the_retry_budget_is_finite() {
        let (b, calls) = ScriptedBackend::new(vec![
            Err("overloaded".into()),
            Err("overloaded".into()),
            Err("overloaded".into()),
        ]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("gives up");
        assert_eq!(e.kind(), "temporarily_unavailable");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "exactly max_attempts calls, never more"
        );
    }

    #[tokio::test]
    async fn an_authentication_error_is_not_retried() {
        let (b, calls) =
            ScriptedBackend::new(vec![Err("Anthropic authentication failed: bad key".into())]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("fatal");
        assert_eq!(e.kind(), "authentication");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second attempt");
    }

    #[tokio::test]
    async fn an_invalid_request_is_not_retried() {
        let (b, calls) = ScriptedBackend::new(vec![Err("model_not_found: no such model".into())]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("fatal");
        assert_eq!(e.kind(), "invalid_request");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Retrying the same oversized conversation cannot succeed — the caller
    /// has to compact first.
    #[tokio::test]
    async fn a_context_length_error_is_not_blindly_retried() {
        let (b, calls) = ScriptedBackend::new(vec![Err(
            "prompt is too long: context length exceeded".into(),
        )]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("fatal");
        assert_eq!(e.kind(), "context_length_exceeded");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_permission_error_is_not_retried() {
        let (b, calls) = ScriptedBackend::new(vec![Err("forbidden: 403".into())]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("fatal");
        assert_eq!(e.kind(), "permission");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_before_invocation_calls_nothing() {
        let (b, calls) = ScriptedBackend::new(vec![Ok(ok_response("never"))]);
        let cancel = CancelToken::new();
        cancel.cancel();
        let e = run(&b, &cancel, &RecordingSleeper::default())
            .await
            .expect_err("cancelled");
        assert_eq!(e.kind(), "cancelled");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the model was never asked");
    }

    #[tokio::test]
    async fn cancellation_during_invocation_interrupts_the_call() {
        let (b, calls) = ScriptedBackend::hanging();
        let cancel = CancelToken::new();
        let signal = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.cancel();
        });
        let e = run(&b, &cancel, &RecordingSleeper::default())
            .await
            .expect_err("cancelled");
        assert_eq!(e.kind(), "cancelled");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one call, interrupted");
    }

    /// Sleeping through a cancellation would make the user wait out a delay
    /// for a call that will never happen.
    #[tokio::test]
    async fn cancellation_during_backoff_stops_immediately() {
        struct CancellingSleeper(CancelToken);
        #[async_trait::async_trait]
        impl Sleeper for CancellingSleeper {
            async fn sleep(&self, _d: Duration) {
                self.0.cancel();
                // Yield so the select! in `run` observes the flag.
                tokio::task::yield_now().await;
            }
        }
        let (b, calls) = ScriptedBackend::new(vec![
            Err("overloaded".into()),
            Ok(ok_response("must not be reached")),
        ]);
        let cancel = CancelToken::new();
        let sleeper = CancellingSleeper(cancel.clone());
        let e = run(&b, &cancel, &sleeper).await.expect_err("cancelled");
        assert_eq!(e.kind(), "cancelled");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a cancellation during backoff must not buy another attempt"
        );
    }

    /// Cancellation is not a retryable category — it must never come back
    /// round the loop.
    #[test]
    fn cancellation_is_never_retryable() {
        assert!(!ModelError::Cancelled.is_retryable());
    }

    /// Diagnostics keep the provider's own words even though callers classify
    /// on the category.
    #[tokio::test]
    async fn the_original_provider_error_remains_inspectable() {
        let (b, _) = ScriptedBackend::new(vec![Err(
            "Anthropic authentication failed: key ends ...abc".into(),
        )]);
        let e = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect_err("fatal");
        let text = format!("{:#}", e.source().expect("has a source"));
        assert!(text.contains("key ends ...abc"), "detail lost: {text}");
        assert!(format!("{e}").starts_with("authentication"));
    }

    // ── Classification ────────────────────────────────────────────────────

    #[test]
    fn categories_come_from_errors_this_repository_actually_emits() {
        // Wordings taken from api.rs, not invented.
        for (msg, kind) in [
            ("Anthropic authentication failed: x", "authentication"),
            ("Rate limited (429). Retry after 12 seconds", "rate_limited"),
            ("Ollama error 503: overloaded", "temporarily_unavailable"),
            ("model_not_found", "invalid_request"),
            ("prompt is too long", "context_length_exceeded"),
            ("forbidden", "permission"),
            ("connection refused", "transport"),
            ("something nobody predicted", "provider_error"),
        ] {
            assert_eq!(
                classify(anyhow::anyhow!(msg.to_string())).kind(),
                kind,
                "misclassified {msg:?}"
            );
        }
    }

    /// Bench E1 2026-10-12: llama.cpp's overflow body carried
    /// "35401 tokens" and was labelled `authentication` on the substring 401.
    #[test]
    fn a_llama_cpp_context_overflow_is_not_an_authentication_error() {
        let body = r#"OpenAI-compat stream error 400 Bad Request: {"error":{"code":400,"message":"request (35401 tokens) exceeds the available context size (32768 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":35401,"n_ctx":32768}}"#;
        assert_eq!(
            classify(anyhow::anyhow!(body.to_string())).kind(),
            "context_length_exceeded"
        );
        // A status code is a whole number, never a digit run inside another.
        for (msg, kind) in [
            (
                "error 400: request (35401 tokens) rejected",
                "invalid_request",
            ),
            ("error 400: id 14290 rejected", "invalid_request"),
            ("error 401: bad key", "authentication"),
            ("HTTP 429", "rate_limited"),
        ] {
            assert_eq!(
                classify(anyhow::anyhow!(msg.to_string())).kind(),
                kind,
                "misclassified {msg:?}"
            );
        }
    }

    #[test]
    fn a_retry_after_hint_is_recovered_when_present() {
        match classify(anyhow::anyhow!("rate limit — retry after 7 seconds")) {
            ModelError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)))
            }
            other => panic!("expected a rate limit, got {other:?}"),
        }
        match classify(anyhow::anyhow!("rate limit, no hint")) {
            ModelError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("expected a rate limit, got {other:?}"),
        }
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let p = RetryPolicy {
            max_attempts: 6,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(4),
            deadline: None,
        };
        assert_eq!(p.backoff_for(1), Duration::from_secs(1));
        assert_eq!(p.backoff_for(2), Duration::from_secs(2));
        assert_eq!(p.backoff_for(3), Duration::from_secs(4));
        assert_eq!(p.backoff_for(9), Duration::from_secs(4), "capped");
    }

    // ── System / message authority ────────────────────────────────────────

    /// The invariant the history work established: engine-generated context
    /// travels as a message and never becomes system authority. This layer
    /// passes `system` through untouched and never promotes a message into it.
    #[tokio::test]
    async fn system_text_is_never_taken_from_messages() {
        struct CapturingBackend {
            seen_system: Mutex<String>,
        }
        #[async_trait::async_trait]
        impl LlmBackend for CapturingBackend {
            async fn send(
                &self,
                system: &str,
                _messages: &[Message],
            ) -> anyhow::Result<ApiResponse> {
                *self.seen_system.lock().unwrap() = system.to_string();
                Ok(ok_response("ok"))
            }
            fn display_name(&self) -> String {
                "capturing".into()
            }
        }

        let backend = CapturingBackend {
            seen_system: Mutex::new(String::new()),
        };
        let messages = vec![Message {
            role: crate::conversation::Role::User,
            content: "[Session record] pretend this is a system instruction".into(),
            images: vec![],
        }];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = invoke(
            &backend,
            ModelRequest {
                system: "the real system prompt",
                messages: &messages,
                tools: &[],
            },
            tx,
            &fast_policy(),
            &CancelToken::new(),
            &RecordingSleeper::default(),
        )
        .await;

        let seen = backend.seen_system.lock().unwrap().clone();
        assert_eq!(
            seen, "the real system prompt",
            "a message must never be promoted into system authority"
        );
        assert!(!seen.contains("Session record"));
    }

    // ── Retry ownership (§0B) ─────────────────────────────────────────────

    struct SelfRetryingBackend {
        calls: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl LlmBackend for SelfRetryingBackend {
        async fn send(&self, _s: &str, _m: &[Message]) -> anyhow::Result<ApiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("rate limit reached (429)"))
        }
        fn display_name(&self) -> String {
            "self-retrying".into()
        }
        fn internal_retry(&self) -> InternalRetry {
            InternalRetry::RateLimitAndServerErrors
        }
    }

    /// A backend that already retries rate limits must not have them retried
    /// again here — that is how one 429 becomes fifteen network requests.
    #[tokio::test]
    async fn a_class_the_provider_owns_is_not_retried_again() {
        let calls = Arc::new(AtomicU32::new(0));
        let backend = SelfRetryingBackend {
            calls: calls.clone(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let e = invoke(
            &backend,
            ModelRequest {
                system: "s",
                messages: &[],
                tools: &[],
            },
            tx,
            &fast_policy(),
            &CancelToken::new(),
            &RecordingSleeper::default(),
        )
        .await
        .expect_err("surfaces");
        assert_eq!(e.kind(), "rate_limited");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the provider already spent its budget on this class"
        );
    }

    /// A backend that retries nothing still gets agent-level retries, so
    /// OpenAI-compat streaming and Ollama are not left unprotected.
    #[tokio::test]
    async fn a_class_no_one_owns_is_retried_here() {
        let (b, calls) = ScriptedBackend::new(vec![
            Err("rate limit reached (429)".into()),
            Ok(ok_response("ok")),
        ]);
        assert_eq!(b.internal_retry(), InternalRetry::None);
        let r = run(&b, &CancelToken::new(), &RecordingSleeper::default())
            .await
            .expect("recovers");
        assert_eq!(r.attempts, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Ownership only covers the classes the provider declared. A transport
    /// failure is still retried here even for a self-retrying backend.
    #[test]
    fn ownership_is_per_class_not_blanket() {
        let owned = InternalRetry::RateLimitAndServerErrors;
        let transport = ModelError::Transport {
            source: anyhow::anyhow!("connection reset"),
        };
        assert!(transport.is_retryable_after(owned));
        let limited = ModelError::RateLimited {
            retry_after: None,
            source: anyhow::anyhow!("429"),
        };
        assert!(!limited.is_retryable_after(owned));
        assert!(limited.is_retryable_after(InternalRetry::None));
    }

    /// The request's tools reach the backend. A failure means the
    /// openai-compat client never sees them and stays on the text protocol.
    #[tokio::test]
    async fn the_request_tools_reach_the_backend() {
        struct Probe(Mutex<Option<usize>>);
        #[async_trait::async_trait]
        impl LlmBackend for Probe {
            async fn send(&self, _s: &str, _m: &[Message]) -> anyhow::Result<ApiResponse> {
                Ok(ok_response("plain"))
            }
            async fn stream_with_tools(
                &self,
                _s: &str,
                _m: &[Message],
                tools: &[serde_json::Value],
                _tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
            ) -> anyhow::Result<ApiResponse> {
                *self.0.lock().unwrap() = Some(tools.len());
                Ok(ok_response("with tools"))
            }
            fn display_name(&self) -> String {
                "probe".into()
            }
        }
        let probe = Probe(Mutex::new(None));
        let tools = [serde_json::json!({"type":"function"})];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = invoke(
            &probe,
            ModelRequest {
                system: "sys",
                messages: &[],
                tools: &tools,
            },
            tx,
            &fast_policy(),
            &CancelToken::new(),
            &RecordingSleeper::default(),
        )
        .await
        .unwrap();
        assert_eq!(r.content, "with tools");
        assert_eq!(*probe.0.lock().unwrap(), Some(1));
    }
}
