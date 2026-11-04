//! HTTP/SSE serve mode — `x0ne --serve`.
//!
//! Exposes a stateless web API that wraps the agent loop. Designed to be put
//! behind nginx and consumed by web frontends.
//!
//! Endpoints
//! ─────────
//!   GET  /health      — liveness only (`{"ok": …}`)
//!   GET  /info        — public metadata (provider, model, available actions)
//!   POST /prompt      — Server-Sent Events stream of tokens / actions / results
//!
//! POST /prompt body
//! ─────────────────
//! ```json
//! {
//!   "prompt": "list files in current dir",
//!   "history": [                       // optional, stateless
//!     { "role": "user",      "content": "..." },
//!     { "role": "assistant", "content": "..." }
//!   ],
//!   "provider":   "ollama",            // optional override (TOML default otherwise)
//!   "model":      "qwen2.5",           // optional override
//!   "base_url":   "http://...:11434",  // optional override
//!   "max_actions": 10                  // safety cap, default 10
//! }
//! ```
//!
//! SSE event types
//! ───────────────
//!   meta    — { provider, model, x0ne_core }
//!   token   — { text }                          (LLM streamed chunk)
//!   action  — { reasoning, action: {...} }      (parsed sysaicall, before exec)
//!   result  — { result: {...}, duration_ms }    (x0ne-runtime ActionResult)
//!   done    — { text, usage: { prompt, completion, total }, actions }
//!   error   — { message, kind }
//!
//! LLM modularity (4 levels, last wins)
//! ────────────────────────────────────
//!   1. `x0ne-orchestrator.toml`  → default provider, model, endpoints
//!   2. `.env`                    → X0NE_LLM_PROVIDER / X0NE_MODEL / OLLAMA_BASE_URL / …
//!   3. `--provider`              → beats X0NE_LLM_PROVIDER
//!   4. POST body fields          → per-request choice among CONFIGURED providers,
//!                                  profiles and endpoints (`select_llm_config`)
//!
//! Every route sits behind a mandatory bearer token and a Host/Origin guard
//! (`serve_guard`).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::http::Method;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use futures::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use crate::api::{self, LlmBackend};
use crate::config::Config;
use crate::parser;
use crate::prompt::build_system_prompt_dynamic;
use crate::session_registry::SessionRegistry;
use crate::tui::wire::AgentEvent;

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HistoryMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct PromptRequest {
    prompt: String,
    #[serde(default)]
    history: Vec<HistoryMessage>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    max_actions: Option<usize>,
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Shared, immutable state cloned (cheaply, via Arc) into every request.
pub struct AppState {
    config: Config,
    /// Backend constructed once at boot from the TOML/.env defaults.
    /// Per-request overrides build a fresh backend (cheap — just an HTTP client).
    default_backend: Arc<dyn LlmBackend>,
    /// Anthropic key from the environment, if set. Needed to rebuild backends.
    anthropic_key: Option<String>,
    /// System prompt — built once with the discovered actions from x0ne-runtime.
    system_prompt: String,
    /// Base URL for x0ne-runtime (e.g. "http://127.0.0.1:7878").
    x0ne_core_url: String,
    /// Bare `host:port` of x0ne-runtime, for per-session `HttpBridge`s.
    x0ne_core_addr: String,
    /// The action catalog discovered at boot, seeded into every session.
    discovered_actions: Option<Vec<Value>>,
    /// Shared HTTP client for talking to x0ne-runtime.
    http: reqwest::Client,
    /// Bearer token, mandatory: given or generated at boot.
    auth_token: String,
    /// Hard cap on actions per request (safety net beyond the per-request value).
    server_max_actions: usize,
    /// Per-chat session state (Conversation, tenant_id, LRU). The upstream
    /// proxy MUST send `X-X0ne-Session: <opaque-id>` on every `/prompt`.
    session_registry: Arc<SessionRegistry>,
    /// Max concurrent sub-agents per session ("parallel worlds" cap). A
    /// `SpawnWorld` beyond this count returns `too_many_agents` without
    /// touching the registry. Defaults to 4 — enough for the common
    /// "explore-3-alternatives" pattern, cheap enough to prevent runaway
    /// spawning from a confused LLM.
    max_agents_per_session: usize,
}

// ── Session header constants ──────────────────────────────────────────────────

/// Header the upstream web proxy uses to identify a chat session. The value
/// is opaque to the orchestrator (typically an HMAC-derived hex string from
/// `apps/web/api/routes/x0ne.js`). Treated case-insensitively by `axum::http`.
pub const SESSION_HEADER: &str = "x-x0ne-session";

/// Hard upper bound on the session-id length we'll accept. Keeps log lines
/// tidy and prevents attempts to fill memory with 1MB header values.
pub const MAX_SESSION_ID_LEN: usize = 256;

/// Default per-session idle TTL (30 minutes). Matches the soft TTL on the
/// `X0neChatSession` Mongo collection's `updatedAt` index intent.
const DEFAULT_SESSION_TTL_SECS: u64 = 30 * 60;

/// Default hard cap on concurrent in-memory sessions. Each session is small
/// (~10 KB conversation + a few counters), but unbounded growth is bad.
const DEFAULT_MAX_SESSIONS: usize = 5_000;

/// Default cap on concurrent sub-agents per session. Tunable via
/// `X0NE_SERVE_MAX_AGENTS_PER_SESSION`.
const DEFAULT_MAX_AGENTS_PER_SESSION: usize = 4;

/// Default per sub-agent action cap. Tunable via
/// `X0NE_SERVE_SUB_AGENT_MAX_ACTIONS`.
const DEFAULT_SUB_AGENT_MAX_ACTIONS: usize = 10;

// ── Public entry point ────────────────────────────────────────────────────────

/// Boot the HTTP server. Blocks until the listener exits. `config` comes
/// resolved: the file, the environment, then the flags ([`Config::resolve`]).
pub async fn run(
    mut config: Config,
    bind_addr: SocketAddr,
    auth_token: Option<String>,
) -> Result<()> {
    // The serve mode drives an http runtime nothing here can place in a container.
    crate::sandbox_mode::require_sandbox("http", None)?;

    // ── SECURITY: a token always, loopback included ──────────────────
    // /prompt drives the full agent loop. On loopback without a token, any
    // local process — or a web page via DNS rebinding — could drive it.
    let non_loopback = !bind_addr.ip().is_loopback();
    if non_loopback && auth_token.as_deref().is_none_or(|t| t.trim().is_empty()) {
        bail!(
            "refusing to bind {} without an auth token: a non-loopback bind exposes \
             agent execution. Set a token (--serve-token / X0NE_SERVE_TOKEN) \
             or bind to 127.0.0.1.",
            bind_addr
        );
    }
    let (auth_token, token_file) = crate::serve_guard::token_or_generate(
        auth_token,
        "serve",
        bind_addr.port(),
        x0ne_actions::http_guard::token_file_path_from_env,
    )?;
    if let Some(path) = &token_file {
        eprintln!(
            "x0ne serve: no X0NE_SERVE_TOKEN given; generated a bearer token in {} \
             (send `Authorization: Bearer <contents>`)",
            path.display()
        );
    }

    // ── Connect to x0ne-runtime (HTTP only — no per-request stdio spawning) ──
    let x0ne_core_addr = std::env::var("X0NE_CORE_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| config.bridge.x0ne_core_addr.clone());
    let x0ne_core_url = format!("http://{}", x0ne_core_addr);

    // The runtime requires its own bearer.
    let mut runtime_headers = reqwest::header::HeaderMap::new();
    if let Some(t) = crate::bridge::resolve_runtime_token(&x0ne_core_addr) {
        let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
            .context("X0NE_CORE_TOKEN is not a valid header value")?;
        v.set_sensitive(true);
        runtime_headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    let http = crate::api::client_for(&x0ne_core_url)
        .timeout(Duration::from_secs(120))
        .default_headers(runtime_headers)
        .build()
        .context("failed to build HTTP client for x0ne-runtime")?;

    info!("x0ne-runtime target → {}", x0ne_core_url);
    health_probe(
        &http,
        &x0ne_core_url,
        config.bridge.health_check_timeout_secs,
    )
    .await
    .with_context(|| format!("x0ne-runtime health check failed at {}", x0ne_core_url))?;

    let actions: Vec<Value> = list_actions(&http, &x0ne_core_url)
        .await
        .unwrap_or_else(|e| {
            warn!("ListActions failed (continuing with empty registry): {}", e);
            Vec::new()
        });
    if !actions.is_empty() {
        parser::init_known_actions(&actions);
    }
    info!("discovered {} x0ne-runtime actions", actions.len());

    let system_prompt = build_system_prompt_dynamic(
        &config,
        false, // desktop disabled in serve mode
        Some("Web client"),
        Some(&actions),
    );

    // ── Build the default LLM backend (non-interactive — no wizard) ──────────
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    // ── QW2: loud warning when X0NE_PLAN_AUTO_APPROVE is set ─────────────────
    // The CLI already announces this via announce_plan_auto_approve_if_set();
    // serve mode needs its own check since it takes a different code path.
    crate::sysaicalls::announce_plan_auto_approve_if_set();

    let default_backend: Arc<dyn LlmBackend> =
        api::create_backend(&config.llm, anthropic_key.clone())
            .map(Arc::from)
            .with_context(|| {
                format!(
                    "failed to build default LLM backend (provider = {})",
                    config.llm.provider
                )
            })?;
    info!("default backend ready: {}", default_backend.display_name());
    crate::model_keys::announce(&config.llm);

    // ── QW4: LLM backend health-check at boot ───────────────────────────────
    // Probes the configured backend so a missing `ollama pull` or a
    // misconfigured endpoint surfaces immediately — not mid-demo on the first
    // user prompt. Non-fatal: we warn and continue so that a temporarily
    // unavailable backend doesn't block the server from starting.
    match config.llm.provider.as_str() {
        "ollama" => {
            let endpoint = config.llm.ollama.endpoint.clone();
            // `create_backend` above already checked this key.
            let api_key = crate::model_keys::resolve(&config.llm, None).ok().flatten();
            match api::OllamaClient::new(
                &endpoint,
                &config.llm.ollama.model,
                config.llm.ollama.max_tokens,
                api_key,
            ) {
                Ok(client) => {
                    if let Err(e) = client.health_check().await {
                        tracing::error!(
                            "[STARTUP] Ollama health-check FAILED: {}\n\
                             ┌────────────────────────────────────────────────────\n\
                             │ FIX: ollama pull {}\n\
                             └────────────────────────────────────────────────────",
                            e,
                            config.llm.ollama.model
                        );
                    } else {
                        info!(
                            "Ollama health-check OK: model '{}' is ready",
                            config.llm.ollama.model
                        );
                    }
                }
                Err(e) => warn!("Could not build Ollama client for health-check: {}", e),
            }
        }
        "openai" => match crate::model_keys::resolve(&config.llm, None).ok().flatten() {
            Some(key) => {
                match api::OpenAICompatClient::new(
                    "https://api.openai.com",
                    &config.llm.openai.model,
                    config.llm.openai.max_tokens,
                    Some(key),
                ) {
                    Ok(client) => {
                        if let Err(e) = client.health_check().await {
                            tracing::error!("[STARTUP] OpenAI health-check FAILED: {}", e);
                        } else {
                            info!(
                                "OpenAI health-check OK: model '{}' is reachable",
                                config.llm.openai.model
                            );
                        }
                    }
                    Err(e) => warn!("Could not build OpenAI client for health-check: {}", e),
                }
            }
            None => tracing::error!(
                "[STARTUP] provider = \"openai\" but OPENAI_API_KEY is not set.\n\
                     ┌────────────────────────────────────────────────────\n\
                     │ FIX: export OPENAI_API_KEY=sk-...\n\
                     └────────────────────────────────────────────────────"
            ),
        },
        "openai-compat" | "openai_compat" | "litellm" => {
            match api::OpenAICompatClient::new(
                &config.llm.openai_compat.base_url,
                &config.llm.openai_compat.model,
                config.llm.openai_compat.max_tokens,
                crate::model_keys::resolve(&config.llm, None).ok().flatten(),
            ) {
                Ok(client) => {
                    if let Err(e) = client.health_check().await {
                        tracing::error!(
                            "[STARTUP] OpenAI-compat health-check FAILED at {}: {}",
                            crate::redact::url_for_display(&config.llm.openai_compat.base_url),
                            e
                        );
                    } else {
                        info!(
                            "OpenAI-compat health-check OK: {}",
                            crate::redact::url_for_display(&config.llm.openai_compat.base_url)
                        );
                    }
                }
                Err(e) => warn!(
                    "Could not build OpenAI-compat client for health-check: {}",
                    e
                ),
            }
        }
        // anthropic / others: no startup probe (cloud API, no local state to verify)
        _ => {}
    }

    // ── Budget guard: apply X0NE_BUDGET_USD env override ─────────────────────
    if let Ok(v) = std::env::var("X0NE_BUDGET_USD") {
        if let Ok(cap) = v.parse::<f64>() {
            config.cost.budget_usd_limit = cap;
            info!("Budget cap set to ${:.2} (from X0NE_BUDGET_USD)", cap);
        }
    }
    if config.cost.budget_usd_limit > 0.0 {
        info!(
            "Budget guard active: sessions will stop at ${:.2} estimated spend",
            config.cost.budget_usd_limit
        );
    }

    let server_max_actions = std::env::var("X0NE_SERVE_MAX_ACTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20);

    let session_ttl_secs = std::env::var("X0NE_SERVE_SESSION_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SESSION_TTL_SECS);
    let max_sessions = std::env::var("X0NE_SERVE_MAX_SESSIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_SESSIONS);
    let session_registry = Arc::new(SessionRegistry::new(
        Duration::from_secs(session_ttl_secs),
        max_sessions,
    ));
    info!(
        "session-registry: ttl = {}s, max_sessions = {}",
        session_ttl_secs, max_sessions
    );

    // Background eviction task — runs forever until the process exits. Cheap
    // (one HashMap sweep every minute) so spawning detached is fine.
    {
        let reg = session_registry.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _ = reg.evict_expired().await;
            }
        });
    }

    let max_agents_per_session = std::env::var("X0NE_SERVE_MAX_AGENTS_PER_SESSION")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_AGENTS_PER_SESSION)
        .max(1);
    let _legacy_sub_agent_cap = std::env::var("X0NE_SERVE_SUB_AGENT_MAX_ACTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SUB_AGENT_MAX_ACTIONS)
        .max(1);
    info!(
        "multi-agent: max_agents_per_session = {}",
        max_agents_per_session
    );

    let state = Arc::new(AppState {
        config,
        default_backend,
        anthropic_key,
        system_prompt,
        x0ne_core_url,
        x0ne_core_addr,
        discovered_actions: if actions.is_empty() {
            None
        } else {
            Some(actions)
        },
        http,
        auth_token,
        server_max_actions,
        session_registry,
        max_agents_per_session,
    });

    let guard = crate::serve_guard::RequestGuard::from_env(bind_addr);
    if guard.origins.allows_any() {
        warn!(
            "[SECURITY] X0NE_SERVE_CORS_ORIGINS=* — any web page may call this server \
             (the bearer token still applies). Set a specific origin list."
        );
    }
    let app = build_router(state, guard);

    info!("x0ne serve listening on http://{}", bind_addr);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("cannot bind {}", bind_addr))?;
    axum::serve(listener, app)
        .await
        .context("axum serve failed")?;
    Ok(())
}

/// CORS layer mirroring the guard's origin list (empty = no browser origin).
pub(crate) fn cors_for(guard: &crate::serve_guard::RequestGuard) -> CorsLayer {
    let allowed_methods = [Method::GET, Method::POST];
    let allowed_headers: Vec<axum::http::HeaderName> = vec![
        axum::http::header::CONTENT_TYPE,
        axum::http::header::AUTHORIZATION,
        SESSION_HEADER.parse().unwrap(),
    ];
    let base = CorsLayer::new()
        .allow_methods(allowed_methods)
        .allow_headers(allowed_headers);
    if guard.origins.allows_any() {
        base.allow_origin(Any)
    } else {
        let origins: Vec<axum::http::HeaderValue> = guard
            .origins
            .allowed()
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        base.allow_origin(origins)
    }
}

/// The agent profile's routes behind the Host/Origin guard.
fn build_router(state: Arc<AppState>, guard: crate::serve_guard::RequestGuard) -> Router {
    let cors = cors_for(&guard);
    Router::new()
        .route("/health", get(handle_health))
        .route("/info", get(handle_info))
        .route("/attestation", get(handle_attestation))
        .route("/prompt", post(handle_prompt))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(guard),
            crate::serve_guard::guard_middleware,
        ))
        .layer(cors)
}

// ── Auth ──────────────────────────────────────────────────────────────────────

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    crate::serve_guard::check_bearer(headers, &state.auth_token)
}

// ── /health ───────────────────────────────────────────────────────────────────

/// Liveness. Public even when a token is configured — a load balancer has no
/// bearer token — but then it says only whether the service is up.
///
/// SECURITY (measured 2026-08-16): with a token set and a non-loopback bind,
/// `GET /health` answered 200 with `provider` and `model` to anyone who asked.
/// The token was guarding `/prompt` alone, so the one thing an unauthenticated
/// stranger could not do was *use* the agent — they could still read which
/// provider and which model it runs, which is the inventory half of the
/// reconnaissance. A probe needs "is it up"; nothing else on this route is
/// needed to answer that, so nothing else is published once a token exists.
async fn handle_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let core_ok = health_probe(&state.http, &state.x0ne_core_url, 3)
        .await
        .is_ok();
    // A token always exists: details are on `/info`.
    Json(json!({ "ok": core_ok }))
}

// ── /info ─────────────────────────────────────────────────────────────────────

/// SECURITY: when a token is configured, EVERY route that says something about
/// this deployment requires it. `/info` names the provider, the model, the
/// backend and the address of `x0ne-runtime`; before 2026-08-16 it answered all
/// of it with no header at all, so a token protected the agent's actions and
/// published its inventory. The rule is now the simple one — a configured token
/// is the door, and `/health` (liveness, above) is the single deliberate
/// exception, reduced to `{"ok": …}` for exactly that reason.
async fn handle_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    check_auth(&state, &headers)?;
    Ok(Json(json!({
        "service": "x0ne-orchestrator",
        "version": env!("CARGO_PKG_VERSION"),
        "provider": state.config.llm.provider,
        "model": state.config.active_model_name(),
        "backend": state.default_backend.display_name(),
        "x0ne_core": state.x0ne_core_url,
        "max_actions_cap": state.server_max_actions,
    })))
}

// ── /attestation ──────────────────────────────────────────────────────────────

/// Is the model behind this server running inside a proven enclave?
///
/// Answers from `~/.x0ne/cc/last-report.json` — the record every `x0ne cc` run
/// writes — through [`attestation_view`], which is where the whole policy
/// lives: what a missing report means, what a stale one means, and why the
/// answer is a named state rather than a boolean. This handler does two things
/// and nothing else: check the token, read the file.
///
/// SECURITY: behind the token like every other informative route. The report
/// names the VM, the machine type and the model served; `/health` is the only
/// deliberate exception and it says nothing but "up".
async fn handle_attestation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    check_auth(&state, &headers)?;
    let report = crate::cc_runner::read_last_report();
    Ok(Json(crate::cc_runner::attestation_view(
        report.as_ref(),
        chrono::Utc::now(),
    )))
}

// ── /prompt (SSE) ─────────────────────────────────────────────────────────────

async fn handle_prompt(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PromptRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    check_auth(&state, &headers)?;

    if req.prompt.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "prompt is empty".into()));
    }

    let session_id = extract_session_id(&headers)?;
    // Refused before streaming: a 400, not an SSE error after a 200.
    let backend =
        resolve_backend(&state, &req).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let stream = async_stream::stream! {
        match run_session(state.clone(), session_id, req, backend).await {
            Ok(events) => {
                for ev in events {
                    yield Ok(ev);
                }
            }
            Err(e) => {
                yield Ok(error_event("session crashed", &e.to_string()));
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ── Session-id extraction ─────────────────────────────────────────────────────

/// Extract and validate the `X-X0ne-Session` header. Returns a `400` if the
/// header is missing, empty, too long, or contains anything outside a strict
/// allowlist (alphanumerics, `-`, `_`).
///
/// The strict allowlist matters: the `session_id` is later passed to
/// `derive_tenant_id`, whose output we use as a filesystem/container prefix.
/// Rejecting weird characters here keeps every downstream boundary simple.
fn extract_session_id(headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let raw = headers
        .get(SESSION_HEADER)
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!("missing required header `{}`", SESSION_HEADER),
        ))?
        .to_str()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("`{}` must be ASCII", SESSION_HEADER),
            )
        })?
        .trim();

    if raw.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("`{}` must be non-empty", SESSION_HEADER),
        ));
    }
    if raw.len() > MAX_SESSION_ID_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "`{}` exceeds {} characters",
                SESSION_HEADER, MAX_SESSION_ID_LEN
            ),
        ));
    }
    if !raw.chars().all(is_session_id_char) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "`{}` contains illegal characters (allowed: a-z, A-Z, 0-9, -, _)",
                SESSION_HEADER
            ),
        ));
    }
    Ok(raw.to_string())
}

#[inline]
fn is_session_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

// ── Session runner ────────────────────────────────────────────────────────────
//
// We collect events into a Vec rather than yielding directly from the async_stream
// because the headless agent loop owns multiple .await points and itself uses
// channels for token streaming. Buffering inside the session keeps the SSE
// generator simple and easy to reason about. For a typical exchange (a few KB),
// the overhead is negligible.

/// One canonical turn for one server session.
///
/// The serve brain is gone: a request locks its session's
/// [`CliAgentSession`], runs one `Driver` turn on it, and translates the
/// buffered semantic events into the SSE wire the web client already speaks.
/// Everything the local frontends get — canonical History, live Reflect,
/// ApprovalAuthority, live Budget, child/world isolation, unknown actions
/// executing nowhere — a server session now gets by construction.
///
/// ## Remote authority — fail closed, no exceptions
///
/// The sink installed for a server session has no confirmation channel:
/// `request_confirmation` answers `None`, which the canonical gate turns into
/// a denial. A remote request is not a human, and a bearer token is transport
/// authentication, not action authorization. Nothing here fakes an approval,
/// and there is no header, body field or environment variable that can grant
/// one. When remote approval becomes a real product need, it enters as an
/// explicit scoped capability (see `docs/CONFIDENTIAL_RUNTIME.md`), not as a
/// prompt shim.
///
/// ## What replaced the serve-only mechanisms
///
/// - the private parse→dispatch loop      → `Driver::run_turn`
/// - the serve dispatch table             → canonical routing (`route_of`)
/// - `spawn_agent_http`/`run_sub_agent_loop` → `SessionSpawner`/`SessionWorlds`
///   over the session's own `HttpBridge`
/// - the per-request `max_actions` cap    → `DriverLimits::max_actions_per_turn`
/// - the per-session agent cap            → `CliAgentSession::max_children`
/// - Plan rejected by a serve-side stub   → canonical gate: no human → denied
async fn run_session(
    state: Arc<AppState>,
    session_id: String,
    req: PromptRequest,
    backend: Arc<dyn LlmBackend>,
) -> Result<Vec<Event>> {
    let provider = req
        .provider
        .clone()
        .unwrap_or_else(|| state.config.llm.provider.clone());
    let model = req
        .model
        .clone()
        .unwrap_or_else(|| state.config.active_model_name());

    let session = state
        .session_registry
        .get_or_create(&session_id, || build_serve_agent(&state, &session_id))
        .await;
    let mut lock = session.lock().await;

    let tenant_id = lock.tenant_id.clone();
    let was_fresh = lock.agent.state.history.is_empty();

    let mut events: Vec<Event> = Vec::new();
    events.push(json_event(
        "meta",
        json!({
            "provider": provider,
            "model": model,
            "backend": backend.display_name(),
            "x0ne_core": state.x0ne_core_url,
            "session": {
                "tenant_id": tenant_id,
                "turn_count": lock.turn_count,
                "fresh": was_fresh,
            },
        }),
    ));

    // Stateless-client compatibility: a fresh session may be seeded with the
    // client-side transcript. Seeded as *context*, never as authority —
    // History cannot grant anything regardless of what the text claims.
    if was_fresh {
        for h in &req.history {
            match h.role.as_str() {
                "user" => lock.agent.state.history.add_user(&h.content),
                "assistant" => lock.agent.state.history.add_assistant(&h.content),
                _ => {} // ignore "system" / unknown
            }
        }
    }
    lock.touch();

    // The request's own cap, bounded by the server's. Applied to the live
    // limits each turn since each request may choose its own.
    let max_actions = req
        .max_actions
        .unwrap_or(10)
        .clamp(0, state.server_max_actions);
    lock.agent.limits.max_actions_per_turn = Some(max_actions as u32);
    // Per-request model override, on the session seam that preserves
    // history, spend and approvals.
    lock.agent.set_backend(backend);

    let report = lock.agent.run_turn(&req.prompt).await;

    // ── Translate the semantic record into the SSE wire ──────────────────
    //
    // Buffered, as the historical implementation was: the old loop also
    // collected every event and replayed them after the turn. One ordering
    // difference, documented: tokens now arrive as one run before the
    // action/result events instead of interleaved between them.
    for chunk in lock.drain_tokens() {
        if let crate::api::StreamChunk::Token(text) = chunk {
            events.push(json_event("token", json!({ "text": text })));
        }
    }
    for ev in lock.drain_events() {
        match ev {
            AgentEvent::ToolStart { action, .. } => {
                events.push(json_event(
                    "action",
                    json!({ "reasoning": "", "action": action }),
                ));
            }
            AgentEvent::ToolEnd {
                result,
                duration_ms,
                ..
            } => {
                events.push(json_event(
                    "result",
                    json!({ "result": result, "duration_ms": duration_ms }),
                ));
            }
            AgentEvent::ActionDenied { name, reason, .. } => {
                events.push(json_event(
                    "result",
                    json!({
                        "result": {
                            "success": false,
                            "error": { "kind": "denied", "action_type": name, "message": reason },
                        },
                        "duration_ms": 0,
                    }),
                ));
            }
            AgentEvent::Error(message) => {
                events.push(error_event("agent_error", &message));
            }
            _ => {}
        }
    }

    match report.response() {
        Some(text) => {
            events.push(json_event(
                "done",
                json!({
                    "text": text,
                    "usage": {
                        "prompt": report.usage.input_tokens,
                        "completion": report.usage.output_tokens,
                        "total": report.usage.total_tokens(),
                    },
                    "actions": report.actions.total,
                }),
            ));
        }
        None => {
            // The turn ended without an answer. Same wire kinds the old
            // implementation used where an equivalent existed.
            let kind = match &report.outcome {
                crate::agent::driver::TurnOutcome::ActionCapExceeded { .. } => {
                    "max_actions_exceeded"
                }
                other => other.kind(),
            };
            events.push(error_event(
                kind,
                &format!("turn ended: {}", report.outcome.kind()),
            ));
        }
    }

    Ok(events)
}

/// Assemble the canonical session a server chat runs on.
///
/// Everything a local session gets, plus the server's bounds: a fail-closed
/// sink, a per-session `HttpBridge` to the runtime, and the child cap. The
/// event buffer and token stream are drained per turn by `run_session`.
fn build_serve_agent(
    state: &AppState,
    session_id: &str,
) -> crate::session_registry::ServeAgentParts {
    let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = ServeSink {
        events: Arc::clone(&events),
    };
    let bridge: Option<Box<dyn crate::bridge::Bridge>> =
        match crate::bridge::HttpBridge::new(&state.x0ne_core_addr, 5) {
            Ok(b) => Some(Box::new(b)),
            Err(e) => {
                warn!("serve session without runtime bridge: {}", e);
                None
            }
        };
    let mut agent = crate::agent::session::CliAgentSession::new(
        state.config.clone(),
        Arc::clone(&state.default_backend),
        state.system_prompt.clone(),
        bridge,
        Box::new(sink),
    );
    agent.discovered_actions = state.discovered_actions.clone();
    agent.max_children = Some(state.max_agents_per_session);
    // The audit trail. A headless server is the surface where it matters most
    // — no human sees the actions go by — and it was the surface with no
    // session log at all: `serve` never built one. One file per HTTP session,
    // named after it, so a remote turn can be reconstructed from disk.
    // Best-effort: a server that cannot open its log still serves, and says so
    // in the process log rather than failing the request.
    match crate::session::SharedJournal::for_session(session_id) {
        Ok(journal) => agent.journal = Some(journal.handle()),
        Err(e) => warn!("serve session without an audit journal: {}", e),
    }
    let tokens = agent.open_stream();
    crate::session_registry::ServeAgentParts {
        agent,
        events,
        tokens,
    }
}

/// The server's `DisplaySink`: buffers semantic events for SSE translation
/// and answers no confirmation — silence a remote client cannot fill, which
/// the canonical gate reads as a denial.
struct ServeSink {
    events: Arc<std::sync::Mutex<Vec<AgentEvent>>>,
}

impl crate::display_sink::DisplaySink for ServeSink {
    fn emit(&mut self, event: &AgentEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
    // request_confirmation: default None — fail closed.
}

fn resolve_backend(state: &AppState, req: &PromptRequest) -> Result<Arc<dyn LlmBackend>> {
    let no_override = req.provider.is_none() && req.model.is_none() && req.base_url.is_none();
    if no_override {
        return Ok(state.default_backend.clone());
    }
    let llm_cfg = select_llm_config(
        &state.config.llm,
        req.provider.as_deref(),
        req.model.as_deref(),
        req.base_url.as_deref(),
    )?;
    let bk = api::create_backend(&llm_cfg, state.anthropic_key.clone()).with_context(|| {
        format!(
            "failed to build per-request backend for provider {}",
            llm_cfg.provider
        )
    })?;
    Ok(Arc::from(bk))
}

/// The operator's LLM config narrowed by a caller's choice.
///
/// SECURITY: the caller picks among what the operator configured —
/// a provider name, a saved compat profile, a model name — and never an
/// address. A `base_url` is accepted only if it IS one of the configured
/// endpoints; before, any public host passed the SSRF guard and received the
/// operator's API key as `Authorization`.
pub(crate) fn select_llm_config(
    base: &crate::config::LlmConfig,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
) -> Result<crate::config::LlmConfig> {
    const BUILTIN: [&str; 6] = [
        "anthropic",
        "ollama",
        "openai",
        "openai-compat",
        "openai_compat",
        "litellm",
    ];
    let mut cfg = base.clone();
    if let Some(p) = provider.map(str::trim).filter(|p| !p.is_empty()) {
        if BUILTIN.contains(&p) {
            cfg.provider = p.to_string();
        } else if let Some(profile) = base
            .compat_profiles
            .iter()
            .find(|c| c.name.as_deref() == Some(p))
        {
            cfg.provider = "openai-compat".into();
            cfg.openai_compat = profile.clone();
        } else {
            bail!("provider {p:?} is not configured on this server");
        }
    }
    if let Some(u) = base_url.map(str::trim).filter(|u| !u.is_empty()) {
        let want = u.trim_end_matches('/');
        let same = |configured: &str| configured.trim_end_matches('/') == want;
        match cfg.provider.as_str() {
            "ollama" if same(&cfg.ollama.endpoint) => {}
            "ollama" => bail!("base_url is not the configured ollama endpoint"),
            "openai-compat" | "openai_compat" | "litellm" => {
                if !same(&cfg.openai_compat.base_url) {
                    // A saved profile at that address, with ITS key.
                    let profile = base
                        .compat_profiles
                        .iter()
                        .find(|c| same(&c.base_url))
                        .ok_or_else(|| {
                            anyhow!("base_url is not an endpoint configured on this server")
                        })?;
                    cfg.openai_compat = profile.clone();
                }
            }
            other => bail!("provider {other:?} has no configurable base_url"),
        }
    }
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        if m.len() > 200 || m.chars().any(|c| c.is_control()) {
            bail!("invalid model name");
        }
        match cfg.provider.as_str() {
            "ollama" => cfg.ollama.model = m.to_string(),
            "openai" => cfg.openai.model = m.to_string(),
            "openai-compat" | "openai_compat" | "litellm" => {
                cfg.openai_compat.model = m.to_string()
            }
            _ => cfg.anthropic.model = m.to_string(),
        }
    }
    Ok(cfg)
}

async fn execute_action(client: &reqwest::Client, base_url: &str, action: &Value) -> Result<Value> {
    let resp = client
        .post(format!("{}/execute", base_url))
        .json(action)
        .send()
        .await
        .with_context(|| format!("POST {}/execute failed", base_url))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .context("x0ne-runtime response is not JSON")?;
    if !status.is_success() {
        return Err(anyhow!("x0ne-runtime returned HTTP {}: {}", status, body));
    }
    Ok(body)
}

async fn list_actions(client: &reqwest::Client, base_url: &str) -> Result<Vec<Value>> {
    let resp = execute_action(client, base_url, &json!({ "type": "ListActions" })).await?;
    let actions = resp["output"]["actions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(actions)
}

async fn health_probe(client: &reqwest::Client, base_url: &str, timeout_secs: u64) -> Result<()> {
    let url = format!("{}/health", base_url);
    let resp = tokio::time::timeout(Duration::from_secs(timeout_secs), client.get(&url).send())
        .await
        .map_err(|_| anyhow!("health check timed out after {}s", timeout_secs))?
        .with_context(|| format!("cannot reach x0ne-runtime at {}", url))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "x0ne-runtime returned HTTP {} on /health",
            resp.status()
        ));
    }
    Ok(())
}

// ── SSE event helpers ─────────────────────────────────────────────────────────

fn json_event(kind: &str, data: Value) -> Event {
    Event::default().event(kind).data(data.to_string())
}

fn error_event(kind: &str, message: &str) -> Event {
    json_event("error", json!({ "kind": kind, "message": message }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    // ── The caller chooses among configured providers only ─────────

    fn llm_with_key() -> crate::config::LlmConfig {
        let mut llm = Config::default().llm;
        llm.provider = "openai-compat".into();
        llm.openai_compat.base_url = "http://127.0.0.1:4000".into();
        llm.openai_compat.api_key = Some("sk-operator".into());
        let mut kimi = llm.openai_compat.clone();
        kimi.name = Some("kimi".into());
        kimi.base_url = "https://api.moonshot.example".into();
        kimi.api_key = Some("sk-kimi".into());
        llm.compat_profiles = vec![kimi];
        llm
    }

    /// Red on 2026-09-23 with the old rule (URL taken as given, SSRF guard
    /// passing a public IP): a client sending `sk-operator` there was built.
    #[test]
    fn a_caller_base_url_never_receives_the_operator_key() {
        let state = state_with_llm(llm_with_key());
        let req = PromptRequest {
            prompt: "x".into(),
            history: vec![],
            provider: Some("openai-compat".into()),
            model: None,
            base_url: Some("http://93.184.216.34:8080".into()),
            max_actions: None,
        };
        let err = resolve_backend(&state, &req)
            .err()
            .expect("an unconfigured base_url was accepted");
        assert!(err.to_string().contains("configured"), "{err}");
    }

    #[test]
    fn configured_endpoints_and_profiles_stay_selectable() {
        let llm = llm_with_key();
        // The configured endpoint itself (trailing slash tolerated).
        let c = select_llm_config(&llm, None, None, Some("http://127.0.0.1:4000/")).unwrap();
        assert_eq!(c.openai_compat.api_key.as_deref(), Some("sk-operator"));
        // A saved profile by name brings its own URL and key.
        let c = select_llm_config(&llm, Some("kimi"), Some("k2"), None).unwrap();
        assert_eq!(c.openai_compat.base_url, "https://api.moonshot.example");
        assert_eq!(c.openai_compat.api_key.as_deref(), Some("sk-kimi"));
        assert_eq!(c.openai_compat.model, "k2");
        // …and by its address, still with ITS key, never the active one.
        let c = select_llm_config(&llm, None, None, Some("https://api.moonshot.example")).unwrap();
        assert_eq!(c.openai_compat.api_key.as_deref(), Some("sk-kimi"));
    }

    #[test]
    fn unknown_providers_and_foreign_urls_are_refused() {
        let llm = llm_with_key();
        assert!(select_llm_config(&llm, Some("evil-provider"), None, None).is_err());
        assert!(select_llm_config(&llm, None, None, Some("http://169.254.169.254")).is_err());
        assert!(
            select_llm_config(&llm, Some("anthropic"), None, Some("https://x.example")).is_err(),
            "anthropic has no configurable URL: silently ignoring it hid the request"
        );
        assert!(
            select_llm_config(&llm, Some("ollama"), None, Some("http://10.0.0.5:11434")).is_err()
        );
        assert!(select_llm_config(&llm, None, Some("bad\nmodel"), None).is_err());
    }

    /// A server session's sink has no confirmation channel, so a sensitive
    /// action through the real canonical path is denied and never reaches the
    /// runtime. Transport authentication is not action authorization.
    #[tokio::test]
    async fn a_remote_session_fails_closed_on_sensitive_actions() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut parts = scripted_parts(
            vec![
                "<action>{\"type\": \"MoveToTrash\", \"path\": \"/tmp/x\"}</action>".into(),
                "done".into(),
            ],
            std::sync::Arc::clone(&seen),
        );
        let report = parts.agent.run_turn("trash it").await;
        assert_eq!(report.outcome.kind(), "denied");
        assert!(
            seen.lock().unwrap().is_empty(),
            "a denied action reached the runtime"
        );
        // And the sink recorded the denial for the SSE translator.
        let events = parts.events.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ActionDenied { .. })));
    }

    /// Two server sessions are two sessions: separate histories, separate
    /// authorities, separate registries. The cloisonnement the proxy relies
    /// on, now by construction.
    #[tokio::test]
    async fn two_serve_sessions_share_nothing() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let a = scripted_parts(vec!["a done".into()], std::sync::Arc::clone(&seen));
        let b = scripted_parts(vec!["b done".into()], seen);
        assert!(!std::sync::Arc::ptr_eq(
            &a.agent.registry,
            &b.agent.registry
        ));
        assert_eq!(b.agent.state.authority.grant_count(), 0);
        assert!(b.agent.state.history.is_empty());
        drop(a);
    }

    /// The per-session child cap: at the cap, the next turn runs without the
    /// spawn capability and a SpawnAgent is refused rather than registered.
    #[tokio::test]
    async fn the_child_cap_withdraws_the_spawn_capability() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut parts = scripted_parts(
            vec![
                "<action>{\"type\": \"SpawnAgent\", \"id\": \"c1\", \"task\": \"x\"}</action>"
                    .into(),
                "done".into(),
            ],
            seen,
        );
        parts.agent.max_children = Some(0);
        let _ = parts.agent.run_turn("spawn one").await;
        assert_eq!(
            parts.agent.registry.count(),
            0,
            "a spawn beyond the cap grew the registry"
        );
    }

    /// The action cap ends the turn with the canonical outcome the SSE
    /// translator maps to the historical `max_actions_exceeded` wire kind.
    #[tokio::test]
    async fn the_request_action_cap_bounds_the_turn() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut parts = scripted_parts(
            vec![
                "<action>{\"type\": \"ReadFile\", \"path\": \"/a\"}</action>".into(),
                "<action>{\"type\": \"ReadFile\", \"path\": \"/b\"}</action>".into(),
                "<action>{\"type\": \"ReadFile\", \"path\": \"/c\"}</action>".into(),
                "done".into(),
            ],
            std::sync::Arc::clone(&seen),
        );
        parts.agent.limits.max_actions_per_turn = Some(2);
        let report = parts.agent.run_turn("read three").await;
        assert_eq!(report.outcome.kind(), "action_cap_exceeded");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the cap did not bound execution"
        );
    }

    // ── The token is the door on every informative route ──────────────

    /// An `AppState` with `auth_token` set or not, and nothing else that
    /// matters to the two routes below.
    fn state_with_token(token: Option<&str>) -> Arc<AppState> {
        let mut config = Config::default();
        config.llm.provider = "a-provider-nobody-should-learn".into();
        state_from(config, token.unwrap_or("default-test-token"))
    }

    fn state_with_llm(llm: crate::config::LlmConfig) -> Arc<AppState> {
        let mut config = Config::default();
        config.llm = llm;
        state_from(config, "t")
    }

    fn state_from(config: Config, token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            config,
            default_backend: Arc::new(ScriptedBackend {
                replies: std::sync::Mutex::new(Default::default()),
            }),
            anthropic_key: None,
            system_prompt: String::new(),
            x0ne_core_url: "http://127.0.0.1:7878".into(),
            x0ne_core_addr: "127.0.0.1:7878".into(),
            discovered_actions: None,
            http: reqwest::Client::new(),
            auth_token: token.to_string(),
            server_max_actions: 10,
            session_registry: Arc::new(SessionRegistry::new(Duration::from_secs(60), 8)),
            max_agents_per_session: 2,
        })
    }

    async fn body_of(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("a body");
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }

    /// SECURITY: with a token configured, nothing says which model runs here
    /// until the token is presented.
    ///
    /// Measured 2026-08-16: `GET /info` answered 200 with `provider`, `model`,
    /// `backend` and the runtime's address, with no header at all. The token
    /// guarded `/prompt` alone, so a non-loopback bind — which the startup
    /// check permits precisely BECAUSE a token is set — published the whole
    /// inventory to anyone who asked. A failure of this test means that is
    /// true again, and the flag an operator set to close the door leaves a
    /// window open beside it.
    #[tokio::test]
    async fn info_never_leaks_the_model_without_the_token() {
        let state = state_with_token(Some("s3cret"));

        let denied = handle_info(State(state.clone()), HeaderMap::new()).await;
        let (code, _) = denied.err().expect("no token must not be answered");
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        let wrong = handle_info(State(state.clone()), bearer("not-it")).await;
        assert!(wrong.is_err(), "a wrong token must not be answered either");

        let ok = handle_info(State(state.clone()), bearer("s3cret"))
            .await
            .expect("the right token opens it")
            .into_response();
        let body = body_of(ok).await;
        assert_eq!(body["provider"], json!("a-provider-nobody-should-learn"));
    }

    /// Liveness stays public, and says only that.
    ///
    /// A load balancer has no bearer token, so `/health` is the one deliberate
    /// exception to the rule above — which is exactly why it must stop
    /// answering the question it was never asked. A failure means the route a
    /// probe needs is again the route that names the model.
    #[tokio::test]
    async fn health_stays_public_and_stops_naming_the_model() {
        let gated = handle_health(State(state_with_token(Some("s3cret"))))
            .await
            .into_response();
        let body = body_of(gated).await;
        assert!(body["ok"].is_boolean(), "a probe still gets its answer");
        for leaked in ["provider", "model", "x0ne_core"] {
            assert!(
                body.get(leaked).is_none(),
                "`{leaked}` is published to an unauthenticated caller: {body}"
            );
        }

        // There is no tokenless server any more, so no fuller payload.
        let open = handle_health(State(state_with_token(None)))
            .await
            .into_response();
        let body = body_of(open).await;
        assert!(body.get("provider").is_none(), "{body}");
    }

    // ── Host / Origin guard on the agent routes ──────────────────

    async fn serve_router(state: Arc<AppState>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(state, crate::serve_guard::RequestGuard::new(addr, &[], &[]));
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    async fn get_info(addr: SocketAddr, headers: &[(&str, String)]) -> reqwest::StatusCode {
        let mut rb = reqwest::Client::new().get(format!("http://{addr}/info"));
        for (k, v) in headers {
            rb = rb.header(*k, v);
        }
        rb.send().await.unwrap().status()
    }

    #[tokio::test]
    async fn the_agent_routes_refuse_rebound_hosts_and_browser_origins() {
        let addr = serve_router(state_with_token(Some("s3cret"))).await;
        let auth = ("authorization", "Bearer s3cret".to_string());
        assert_eq!(get_info(addr, std::slice::from_ref(&auth)).await, 200);
        assert_eq!(get_info(addr, &[]).await, 401);
        let rebound = ("host", format!("evil.example:{}", addr.port()));
        assert_eq!(get_info(addr, &[auth.clone(), rebound]).await, 403);
        let origin = ("origin", "http://localhost".to_string());
        assert_eq!(get_info(addr, &[auth.clone(), origin]).await, 403);
        let site = ("sec-fetch-site", "cross-site".to_string());
        assert_eq!(get_info(addr, &[auth, site]).await, 403);
    }

    /// SECURITY: the attestation route is informative, so it is behind the
    /// token like `/info`.
    ///
    /// The report names the VM, the machine type and the model served. A
    /// failure means an unauthenticated caller learns whether this deployment
    /// runs confidential hardware and what it serves — the reconnaissance half
    /// of the leak `/info` had.
    #[tokio::test]
    async fn attestation_is_behind_the_token_too() {
        let state = state_with_token(Some("s3cret"));
        assert!(handle_attestation(State(state.clone()), HeaderMap::new())
            .await
            .is_err());
        let ok = handle_attestation(State(state), bearer("s3cret"))
            .await
            .expect("the right token opens it")
            .into_response();
        let body = body_of(ok).await;
        // Whatever is on this machine's disk, the shape is the contract.
        assert!(body["state"].is_string(), "{body}");
        assert!(body["label"].is_string(), "{body}");
    }

    // ── Test scaffolding ─────────────────────────────────────────────

    struct ScriptedBackend {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    #[async_trait]
    impl LlmBackend for ScriptedBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> Result<crate::api::ApiResponse> {
            Ok(crate::api::ApiResponse {
                content: self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "done".into()),
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "scripted".into()
        }
    }

    struct CountingBridge {
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl crate::bridge::Bridge for CountingBridge {
        async fn send_action(&mut self, action: &Value) -> Result<Value> {
            // A preflight probe is a question, not work: it is neither
            // counted nor recorded, and it is answered with nothing that
            // parses as a verdict, so the human gate stays in charge exactly
            // as it did before the preflight existed.
            if crate::agent::preflight::is_probe(action)
                || crate::agent::undo::is_bookkeeping(action)
            {
                return Ok(serde_json::json!({"success": true, "output": {}}));
            }
            let t = action["type"].as_str().unwrap_or("?").to_string();
            self.seen.lock().unwrap().push(t.clone());
            Ok(json!({"success": true, "action_type": t, "output": {}}))
        }
        async fn health_check(&mut self) -> Result<bool> {
            Ok(true)
        }
    }

    /// The headless surface must journal too.
    ///
    /// `serve` runs turns on the canonical session with no human watching and,
    /// until 2026-08-15, no session log of any kind — the one surface where an
    /// audit trail is the *only* record. `journal` defaults to `None`, so this
    /// guards the assignment rather than the machinery behind it. Where the
    /// file lands, given a client-chosen session id, is proven in
    /// `session::tests::a_client_supplied_session_id_cannot_steer_the_log_out_of_its_directory`.
    #[test]
    fn a_serve_session_is_handed_an_audit_journal() {
        let src = include_str!("serve.rs");
        let production = &src[..src.find("mod tests").unwrap_or(src.len())];
        assert!(
            production.contains("agent.journal = Some(journal.handle())"),
            "serve assembles a session with no audit journal"
        );
    }

    /// The production serve assembly with a scripted model and a counting
    /// runtime — `build_serve_agent`'s shape, minus the HTTP endpoints.
    fn scripted_parts(
        replies: Vec<String>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> crate::session_registry::ServeAgentParts {
        let events: std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = ServeSink {
            events: std::sync::Arc::clone(&events),
        };
        let mut agent = crate::agent::session::CliAgentSession::new(
            Config::default(),
            Arc::new(ScriptedBackend {
                replies: std::sync::Mutex::new(replies.into()),
            }),
            "SYS",
            Some(Box::new(CountingBridge { seen })),
            Box::new(sink),
        );
        let tokens = agent.open_stream();
        crate::session_registry::ServeAgentParts {
            agent,
            events,
            tokens,
        }
    }
}
