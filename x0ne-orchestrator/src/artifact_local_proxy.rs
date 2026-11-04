//! Local reverse-proxy for artifacts — `http://127.0.0.1:7800/a/<id>/*`.
//!
//! Architectural role
//! ──────────────────
//! This module replaces the previous "expose via `incus proxy device`"
//! approach with a strictly **loopback-only** HTTP server embedded in
//! `x0ne-orchestrator`. The user's browser talks to the local `x0ne`
//! process; `x0ne` then pulls bytes out of the tenant container using
//! the already-authenticated Incus client. No port is opened on the
//! network, no routing rules are touched on the remote host.
//!
//! Why this is safer than the previous model
//! ─────────────────────────────────────────
//! - Loopback bind only. `127.0.0.1` is reachable only from the same
//!   host, so an adjacent laptop on the LAN cannot scan it.
//! - No collision between tenants. Each user runs their own `x0ne` →
//!   their own `127.0.0.1:7800`. Multi-tenant becomes a local concern.
//! - Revocation is free: when the `x0ne` process exits, the port dies
//!   with it. No orphan Incus device lying around.
//! - Works from anywhere. The transport is `incus exec`, which is
//!   already tunneled/authenticated by the user's Incus client config,
//!   so the artifact is reachable over VPN / remote without opening WAN
//!   ports on the tenant host.
//! - Only a browser on this machine is answered: `GET` and `HEAD`, a
//!   loopback `Host` with the bound port, a loopback `Origin` when one is
//!   sent, no cross-site embedding. Every answer carries `nosniff`,
//!   `no-referrer`, `Cross-Origin-Resource-Policy: same-origin` and
//!   `frame-ancestors 'none'` (audit of 2026-10-30, H3-1).
//!
//! Data path (static artifact)
//! ─────────────────────────
//!     browser
//!       │   GET http://127.0.0.1:7800/a/cv_abc/index.html
//!       ▼
//!     [ artifact_local_proxy (this module) ]
//!       │   lookup cv_abc in registry → { tenant, root_path }
//!       │   incus exec `<tenant>` -- cat `<root_path>`/index.html
//!       ▼
//!     container /root/.x0ne/artifacts/cv_abc/index.html
//!
//! Service artifacts (where the artifact is a long-running HTTP process
//! inside the container, not a set of static files) are reserved for a
//! later iteration — the registry entry supports `ArtifactKind::Service`
//! and the proxy returns 501 for those today.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    extract::{Path as AxPath, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use x0ne_actions::http_guard::HostPolicy;

pub const DEFAULT_BIND: &str = "127.0.0.1:7800";

/// Default address where the runtime's in-process artifact server (see
/// `x0ne-runtime/src/artifact_serve.rs`) listens. The orchestrator forwards
/// any `/a/<id>/...` request that isn't explicitly registered to this
/// upstream — that's how artifacts created in a sandbox become reachable on
/// `localhost:7800` without any host bind mount of agent-generated content.
pub const DEFAULT_RUNTIME_UPSTREAM: &str = "127.0.0.1:7900";

/// How a artifact delivers its content.
#[derive(Clone, Debug)]
pub enum ArtifactKind {
    /// Plain static directory — we pull files out of the container.
    Static,
    /// Long-running HTTP service inside the container on `port`. Not
    /// wired up yet (returns 501 until the TCP-tunnel transport lands).
    Service { port: u16 },
}

#[derive(Clone, Debug)]
pub struct ArtifactEntry {
    pub artifact_id: String,
    /// Container target understood by `backend`. For Incus this is
    /// `"remote:container"`; for Podman/Docker it's just the container
    /// name or id. `None` means "serve from the local filesystem"
    /// (used in unit tests and for future host-mode artifacts).
    pub tenant: Option<String>,
    /// Container runtime used to pull files when `tenant.is_some()`.
    /// `None` is only valid when `tenant` is also `None` (host-fs mode).
    pub backend: Option<crate::container_runtime::Backend>,
    /// Absolute path INSIDE the tenant (or on the host when `tenant`
    /// is `None`) where the artifact directory lives.
    pub root_path: String,
    pub kind: ArtifactKind,
    /// Wall-clock expiry. Entries past this are dropped on access.
    pub expires_at: Option<Instant>,
}

#[derive(Default)]
pub struct ArtifactRegistry {
    entries: RwLock<HashMap<String, ArtifactEntry>>,
}

impl ArtifactRegistry {
    pub async fn register(&self, entry: ArtifactEntry) {
        let id = entry.artifact_id.clone();
        self.entries.write().await.insert(id, entry);
    }

    pub async fn unregister(&self, artifact_id: &str) -> bool {
        self.entries.write().await.remove(artifact_id).is_some()
    }

    pub async fn list(&self) -> Vec<ArtifactEntry> {
        self.entries.read().await.values().cloned().collect()
    }

    pub async fn get(&self, artifact_id: &str) -> Option<ArtifactEntry> {
        let mut guard = self.entries.write().await;
        let entry = guard.get(artifact_id).cloned()?;
        if let Some(exp) = entry.expires_at {
            if Instant::now() >= exp {
                guard.remove(artifact_id);
                return None;
            }
        }
        Some(entry)
    }
}

// ── Singleton ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct RouterState {
    pub registry: Arc<ArtifactRegistry>,
    /// Reverse-proxy target for unregistered artifacts. The runtime's
    /// `artifact_serve` HTTP app (default `127.0.0.1:7900`). When the
    /// runtime lives in a sandbox, set this to `<container-ip>:7900` so
    /// the orchestrator can reach it via the container bridge.
    pub runtime_upstream: String,
    pub http: reqwest::Client,
    /// The address served. The `Host` guard admits it and the loopback
    /// spellings of its port, nothing else.
    pub bind: SocketAddr,
}

struct ProxyState {
    inner: RouterState,
    bind: SocketAddr,
}

static PROXY: OnceLock<Arc<ProxyState>> = OnceLock::new();

/// Returns the shared registry, starting the HTTP server on first call.
/// Subsequent calls are cheap lookups.
pub async fn ensure_started() -> Result<Arc<ArtifactRegistry>> {
    ensure_started_on(DEFAULT_BIND.parse().unwrap()).await
}

/// Start (or return the already-running) proxy bound to `bind`. If the
/// proxy is already running on a different bind, the request is ignored
/// and the existing one is returned — callers should not re-bind mid-
/// session. Binding to `127.0.0.1:0` is supported for tests.
pub async fn ensure_started_on(bind: SocketAddr) -> Result<Arc<ArtifactRegistry>> {
    if let Some(state) = PROXY.get() {
        return Ok(state.inner.registry.clone());
    }
    let (listener, state) = bind_router(bind).await?;
    if PROXY.set(state.clone()).is_err() {
        // Another caller won the race: its server answers, this listener
        // closes with the function. Hand back the registry that is served.
        drop(listener);
        return Ok(PROXY
            .get()
            .expect("set by the caller that won")
            .inner
            .registry
            .clone());
    }
    spawn_serving(listener, &state);
    Ok(state.inner.registry.clone())
}

/// Binds `bind` and builds the state of a router on it. Nothing is served
/// until `spawn_serving`.
async fn bind_router(bind: SocketAddr) -> Result<(TcpListener, Arc<ProxyState>)> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow!("artifact_local_proxy: bind {} failed: {}", bind, e))?;
    let actual = listener.local_addr()?;
    let upstream = std::env::var("X0NE_RUNTIME_UPSTREAM")
        .ok()
        .unwrap_or_else(|| DEFAULT_RUNTIME_UPSTREAM.to_string());
    let inner = RouterState {
        registry: Arc::new(ArtifactRegistry::default()),
        runtime_upstream: upstream,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow!("reqwest client: {}", e))?,
        bind: actual,
    };
    Ok((
        listener,
        Arc::new(ProxyState {
            inner,
            bind: actual,
        }),
    ))
}

/// Serves the router on `listener` from the current Tokio runtime.
fn spawn_serving(listener: TcpListener, state: &Arc<ProxyState>) {
    let app = build_router(state.inner.clone());
    info!(target: "x0ne::artifact_local_proxy",
          addr = %state.bind,
          upstream = %state.inner.runtime_upstream,
          "artifact router started (loopback :7800 → runtime :7900)");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!(target: "x0ne::artifact_local_proxy", error = %e, "proxy server exited");
        }
    });
}

/// A router of its own on an ephemeral port, outside the singleton: each
/// test serves from its own runtime and its own registry.
#[cfg(test)]
async fn start_private() -> (SocketAddr, Arc<ArtifactRegistry>) {
    let (listener, state) = bind_router("127.0.0.1:0".parse().unwrap())
        .await
        .expect("an ephemeral loopback port");
    spawn_serving(listener, &state);
    (state.bind, state.inner.registry.clone())
}

/// Override the runtime upstream address — call this after detecting a
/// sandbox container's bridge IP, before traffic starts flowing. Has no
/// effect if the proxy hasn't been started yet (set the env var
/// `X0NE_RUNTIME_UPSTREAM` for that case).
pub fn set_runtime_upstream(_addr: String) {
    // The router holds an Arc<RouterState> by value — once axum is serving
    // we can't mutate it without a RwLock. The current state is fine for
    // host mode (default 127.0.0.1:7900). Sandbox-mode IP detection is
    // wired in via the X0NE_RUNTIME_UPSTREAM env var that bridge.rs sets
    // before calling `ensure_started`. Keep this as a documented stub.
    debug!(target: "x0ne::artifact_local_proxy",
           "set_runtime_upstream called post-start — set X0NE_RUNTIME_UPSTREAM env before bridge spawn instead");
}

/// Public URL for a artifact. Returns `None` if the proxy hasn't been
/// started yet (callers should call `ensure_started()` first).
pub fn url_for(artifact_id: &str) -> Option<String> {
    let state = PROXY.get()?;
    Some(format!("http://{}/a/{}/", state.bind, artifact_id))
}

pub fn bind_addr() -> Option<SocketAddr> {
    PROXY.get().map(|s| s.bind)
}

pub fn registry() -> Option<Arc<ArtifactRegistry>> {
    PROXY.get().map(|s| s.inner.registry.clone())
}

// ── Router ───────────────────────────────────────────────────────────────────

fn build_router(state: RouterState) -> Router {
    Router::new()
        .route("/a/{id}", any(serve_root))
        .route("/a/{id}/", any(serve_root))
        .route("/a/{id}/{*path}", any(serve_file))
        .route("/healthz", any(healthz))
        .layer(middleware::from_fn_with_state(state.clone(), router_guard))
        .with_state(state)
}

/// Audit of 2026-10-30, H3-1. The router answered whatever reached its
/// port: a page whose name resolves to 127.0.0.1 (its own `Host`), a
/// cross-origin fetch, a `POST` with a 16 MB body relayed to the runtime.
/// It now admits what the user's own browser sends from this machine and
/// adds the headers that keep an artifact page in its place.
async fn router_guard(State(state): State<RouterState>, req: Request, next: Next) -> Response {
    if let Err(refused) = admit(state.bind, req.method(), req.headers()) {
        return refused;
    }
    let mut resp = next.run(req).await;
    protect(resp.headers_mut());
    resp
}

/// `GET` or `HEAD`; a loopback `Host` with the bound port; an `Origin`, when
/// the browser sends one, on that loopback; without one, a cross-site
/// request only as a top-level navigation (a link the user follows), never
/// as an embedded image, script or frame.
fn admit(bind: SocketAddr, method: &Method, headers: &HeaderMap) -> Result<(), Response> {
    if *method != Method::GET && *method != Method::HEAD {
        return Err((
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
            "artifacts are read with GET or HEAD only",
        )
            .into_response());
    }
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let hosts = HostPolicy::loopback(bind.port()).with_extra([bind.to_string()]);
    if let Err(e) = hosts.check(get("host")) {
        return Err((StatusCode::FORBIDDEN, e.to_string()).into_response());
    }
    match get("origin") {
        Some(origin) => {
            let local = origin
                .strip_prefix("http://")
                .is_some_and(|h| hosts.allowed().iter().any(|a| a.eq_ignore_ascii_case(h)));
            if !local {
                return Err((
                    StatusCode::FORBIDDEN,
                    "artifacts answer this machine's browser only (Origin)",
                )
                    .into_response());
            }
        }
        None => {
            let cross_site =
                get("sec-fetch-site").is_some_and(|v| v.eq_ignore_ascii_case("cross-site"));
            let navigation = get("sec-fetch-mode")
                .is_some_and(|v| v.eq_ignore_ascii_case("navigate"))
                && get("sec-fetch-dest").is_some_and(|v| v.eq_ignore_ascii_case("document"));
            if cross_site && !navigation {
                return Err((
                    StatusCode::FORBIDDEN,
                    "a page from another site cannot embed or read an artifact",
                )
                    .into_response());
            }
        }
    }
    Ok(())
}

/// Headers on every answer of the router, static or forwarded.
///
/// `Cross-Origin-Resource-Policy: same-origin`: measured on Chromium 156
/// (2026-10-31), `same-site` already kept a `localhost` page from embedding
/// a `127.0.0.1` artifact and the reverse (two sites to the browser), but
/// let a `localhost` page of any other port embed one; an artifact's files
/// are its own.
fn protect(h: &mut HeaderMap) {
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    h.append(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// Liveness only. The ids of the registered artifacts are their URLs: they
/// are not listed here (audit of 2026-10-30, H3-1).
async fn healthz(State(state): State<RouterState>) -> impl IntoResponse {
    let registered = state.registry.list().await.len();
    axum::Json(serde_json::json!({ "ok": true, "registered": registered }))
}

async fn serve_root(
    State(state): State<RouterState>,
    AxPath(id): AxPath<String>,
    req: Request,
) -> Response {
    serve_impl(state, id, String::new(), req).await
}

async fn serve_file(
    State(state): State<RouterState>,
    AxPath((id, path)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    serve_impl(state, id, path, req).await
}

async fn serve_impl(
    state: RouterState,
    artifact_id: String,
    path: String,
    req: Request,
) -> Response {
    // Coarse traversal guard. The downstream runtime serve also enforces
    // canonicalised path-confinement, but reject obvious cases here so we
    // don't even bother forwarding them.
    if path
        .split('/')
        .any(|seg| seg == ".." || seg.starts_with('\0'))
    {
        return bad_request("path traversal rejected");
    }

    // Legacy path: if the agent explicitly registered this id via
    // `ArtifactExpose` (older flow), serve from the registered entry. New
    // code shouldn't need this — `ArtifactCreate` alone is enough now.
    if let Some(entry) = state.registry.get(&artifact_id).await {
        let rel_path = if path.is_empty() {
            "index.html".to_string()
        } else {
            path.clone()
        };
        return match entry.kind {
            ArtifactKind::Static => serve_static(entry, &rel_path).await,
            ArtifactKind::Service { .. } => (
                StatusCode::NOT_IMPLEMENTED,
                "service artifacts via legacy ArtifactExpose are not routable; \
                 use ArtifactCreate (auto-routed via runtime serve)",
            )
                .into_response(),
        };
    }

    // Default: transparent reverse-proxy to the runtime's HTTP server. The
    // runtime owns the artifact files (in its own trust domain — host or
    // sandbox) and serves them itself. The orchestrator only forwards
    // bytes; agent-generated content never crosses into orchestrator
    // memory or disk.
    forward_to_runtime(&state, &artifact_id, &path, req).await
}

/// Request headers that travel to the runtime. The browser's cookies,
/// credentials and anything else it attaches stay here.
const FORWARDED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "range",
    "if-none-match",
    "if-modified-since",
    "user-agent",
];

/// Response headers the browser gets back from the runtime. `router_guard`
/// adds the protective set on top.
const FORWARDED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "content-security-policy",
];

/// Reverse-proxy `GET`/`HEAD /a/<id>/<path>` to `http://<runtime_upstream>/a/<id>/<path>`.
/// Status and body come back verbatim, headers through the two allowlists;
/// the request body is never read (audit of 2026-10-30, H3-1).
async fn forward_to_runtime(
    state: &RouterState,
    artifact_id: &str,
    rest: &str,
    req: Request,
) -> Response {
    let upstream_path = if rest.is_empty() {
        format!("/a/{}/", artifact_id)
    } else {
        format!("/a/{}/{}", artifact_id, rest)
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let upstream = format!(
        "http://{}{}{}",
        state.runtime_upstream, upstream_path, query
    );

    let method = if req.method() == Method::HEAD {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };
    let mut req_b = state.http.request(method, &upstream);
    for name in FORWARDED_REQUEST_HEADERS {
        for v in req.headers().get_all(*name) {
            if let Ok(val) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
                req_b = req_b.header(*name, val);
            }
        }
    }

    let resp = match req_b.send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!(
                    "runtime upstream {} unreachable: {}",
                    state.runtime_upstream, e
                ),
            )
                .into_response()
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    for name in FORWARDED_RESPONSE_HEADERS {
        for v in resp.headers().get_all(*name) {
            if let Ok(val) = HeaderValue::from_bytes(v.as_bytes()) {
                out_headers.append(axum::http::HeaderName::from_static(name), val);
            }
        }
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("upstream body: {}", e)).into_response();
        }
    };

    (status, out_headers, Body::from(bytes)).into_response()
}

async fn serve_static(entry: ArtifactEntry, rel_path: &str) -> Response {
    let file_path = format!("{}/{}", entry.root_path.trim_end_matches('/'), rel_path);
    debug!(target: "x0ne::artifact_local_proxy",
           artifact=%entry.artifact_id, target=?entry.tenant, file=%file_path,
           "serve static");

    let (bytes, err) = match (&entry.tenant, entry.backend) {
        (Some(tenant), Some(backend)) => {
            let rt = crate::container_runtime::for_backend(backend);
            rt.pull_file(tenant, &file_path).await
        }
        (Some(_), None) => (
            Err(anyhow!(
                "artifact entry has a tenant but no backend — registration is inconsistent"
            )),
            Some("missing backend on container-backed artifact entry".to_string()),
        ),
        (None, _) => match pull_from_host(&entry.root_path, &file_path).await {
            HostFile::Found(b) => (Ok(b), None),
            HostFile::Missing(e) => (Err(anyhow!("{}", e)), Some(e)),
            HostFile::OutsideRoot => {
                return (
                    StatusCode::FORBIDDEN,
                    "path leaves the artifact root (symlink)",
                )
                    .into_response();
            }
        },
    };

    let bytes = match bytes {
        Ok(b) => b,
        Err(_) => {
            return not_found(err.unwrap_or_else(|| {
                format!(
                    "file {} not found in artifact {}",
                    rel_path, entry.artifact_id
                )
            }));
        }
    };

    let mut res = Response::new(Body::from(bytes));
    let ct = guess_content_type(rel_path);
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(ct).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // Artifacts are ephemeral preview artefacts — never cache.
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

enum HostFile {
    Found(Vec<u8>),
    Missing(String),
    OutsideRoot,
}

/// Host-filesystem variant of `pull_file`. Used only when a artifact is
/// registered with `tenant = None` (unit tests and future host-mode
/// artifacts). Container-backed artifacts dispatch through the
/// `ContainerRuntime` trait instead. The file is canonicalised and must
/// stay under the canonical root: a symlink the agent placed in the
/// artifact does not reach out (audit of 2026-10-30, H3-5).
async fn pull_from_host(root: &str, abs_path: &str) -> HostFile {
    let root = match tokio::fs::canonicalize(root).await {
        Ok(r) => r,
        Err(e) => return HostFile::Missing(format!("artifact root: {}", e)),
    };
    let file = match tokio::fs::canonicalize(PathBuf::from(abs_path)).await {
        Ok(f) => f,
        Err(e) => return HostFile::Missing(e.to_string()),
    };
    if !file.starts_with(&root) {
        return HostFile::OutsideRoot;
    }
    match tokio::fs::read(&file).await {
        Ok(b) => HostFile::Found(b),
        Err(e) => HostFile::Missing(e.to_string()),
    }
}

fn not_found(msg: impl Into<String>) -> Response {
    (StatusCode::NOT_FOUND, msg.into()).into_response()
}
fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

fn guess_content_type(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

// ── Duration parsing helper (used by ArtifactExpose TTL) ───────────────────────

/// Compute the expiry `Instant` from a TTL in seconds. `None` = no expiry.
pub fn expires_at_from_ttl(ttl_secs: Option<u64>) -> Option<Instant> {
    ttl_secs.map(|s| Instant::now() + Duration::from_secs(s))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_content_types() {
        assert_eq!(guess_content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            guess_content_type("a/b/style.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            guess_content_type("nested/app.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(guess_content_type("logo.png"), "image/png");
        assert_eq!(
            guess_content_type("weird.unknownext"),
            "application/octet-stream"
        );
        assert_eq!(guess_content_type("noext"), "application/octet-stream");
    }

    #[tokio::test]
    async fn registry_register_get_expire() {
        let reg = ArtifactRegistry::default();
        let e = ArtifactEntry {
            artifact_id: "cv_abc".into(),
            tenant: None,
            backend: None,
            root_path: "/tmp/whatever".into(),
            kind: ArtifactKind::Static,
            expires_at: Some(Instant::now() - Duration::from_secs(1)),
        };
        reg.register(e).await;
        // Already expired → get should evict and return None.
        assert!(reg.get("cv_abc").await.is_none());
    }

    #[tokio::test]
    async fn registry_re_register_overwrites() {
        let reg = ArtifactRegistry::default();
        let make = |rp: &str| ArtifactEntry {
            artifact_id: "cv_same".into(),
            tenant: None,
            backend: None,
            root_path: rp.to_string(),
            kind: ArtifactKind::Static,
            expires_at: None,
        };
        reg.register(make("/first")).await;
        reg.register(make("/second")).await;
        let got = reg.get("cv_same").await.unwrap();
        assert_eq!(got.root_path, "/second");
    }

    #[tokio::test]
    async fn registry_unregister_returns_bool() {
        let reg = ArtifactRegistry::default();
        let e = ArtifactEntry {
            artifact_id: "cv_u".into(),
            tenant: None,
            backend: None,
            root_path: "/tmp".into(),
            kind: ArtifactKind::Static,
            expires_at: None,
        };
        reg.register(e).await;
        assert!(
            reg.unregister("cv_u").await,
            "first unregister returns true"
        );
        assert!(
            !reg.unregister("cv_u").await,
            "second unregister returns false"
        );
        assert!(
            !reg.unregister("never-seen").await,
            "unknown id returns false"
        );
    }

    #[test]
    fn expires_at_from_ttl_returns_none_when_no_ttl() {
        assert!(expires_at_from_ttl(None).is_none());
    }

    #[test]
    fn expires_at_from_ttl_is_in_the_future() {
        let exp = expires_at_from_ttl(Some(60)).expect("some instant");
        assert!(exp > Instant::now());
        // And not absurdly far (guards against overflow bugs).
        assert!(exp < Instant::now() + Duration::from_secs(61));
    }

    #[tokio::test]
    async fn serves_static_from_host_filesystem() {
        // Write a temp artifact on the host fs, register it with `tenant=None`
        // and hit the proxy. This exercises the full router + Static path
        // without requiring incus.
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("index.html");
        std::fs::write(&idx, b"<h1>ok</h1>").unwrap();
        let nested = dir.path().join("sub").join("s.css");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"body{color:red}").unwrap();

        let (addr, reg) = start_private().await;

        reg.register(ArtifactEntry {
            artifact_id: "cv_test".into(),
            tenant: None,
            backend: None,
            root_path: dir.path().to_string_lossy().into_owned(),
            kind: ArtifactKind::Static,
            expires_at: None,
        })
        .await;

        // index.html via /a/cv_test/
        let body = reqwest::get(format!("http://{}/a/cv_test/", addr))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("<h1>ok</h1>"));

        // nested file with content-type
        let r = reqwest::get(format!("http://{}/a/cv_test/sub/s.css", addr))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css"));
        assert!(r.text().await.unwrap().contains("color:red"));

        // traversal rejected
        let r = reqwest::get(format!("http://{}/a/cv_test/..%2fetc%2fpasswd", addr))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        // Note: unknown-artifact behaviour (auto-forward to runtime upstream)
        // is environmental — the test's status depends on whether anything is
        // listening on 127.0.0.1:7900. We cover the auto-forward path in the
        // artifact_serve live smoke test, not here.
    }

    /// One HTTP/1.1 request on a fresh connection, so `Host` and the browser
    /// headers are exactly what the test says. `(status, headers, body)`,
    /// the headers lower-cased.
    async fn raw(addr: SocketAddr, lines: &str) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("{lines}Connection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
            .await
            .expect("the server answers")
            .unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (status, head.to_ascii_lowercase(), body.to_string())
    }

    /// Audit of 2026-10-30, H3-1. The router answered any request reaching
    /// 127.0.0.1:7800: a page at `evil.example` whose DNS now points at
    /// 127.0.0.1 (its own `Host`), a cross-origin fetch, any method with a 16
    /// MB body relayed to the runtime, and `/healthz` listing every artifact.
    /// It now serves GET and HEAD to a local browser context, nothing else,
    /// and every answer carries the headers that keep a page in its place.
    #[tokio::test]
    async fn the_router_serves_only_local_get_and_head() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>guarded</h1>").unwrap();
        let (addr, reg) = start_private().await;
        let port = addr.port();
        reg.register(ArtifactEntry {
            artifact_id: "cv_guard".into(),
            tenant: None,
            backend: None,
            root_path: dir.path().to_string_lossy().into_owned(),
            kind: ArtifactKind::Static,
            expires_at: None,
        })
        .await;

        // DNS rebinding: the browser sends the attacker's Host.
        let (st, _, body) = raw(addr, "GET /a/cv_guard/ HTTP/1.1\r\nHost: evil.example\r\n").await;
        assert_eq!(st, 403, "{body}");
        assert!(
            !body.contains("guarded"),
            "served under a foreign Host: {body}"
        );
        // A fetch from a remote page: Host right, Origin remote.
        let (st, _, body) = raw(
            addr,
            &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\n"),
        )
        .await;
        assert_eq!(st, 403, "{body}");
        // A remote page embedding the artifact (img, script, iframe): no
        // Origin, but the browser says cross-site and not a navigation.
        let (st, _, body) = raw(
            addr,
            &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: localhost:{port}\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: image\r\n"),
        )
        .await;
        assert_eq!(st, 403, "{body}");
        // A method the router used to relay, body included.
        let (st, _, body) = raw(
            addr,
            &format!("POST /a/cv_guard/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 2\r\n\r\nhi"),
        )
        .await;
        assert_eq!(st, 405, "{body}");
        // The inventory of artifacts is not for whoever asks.
        let (st, _, body) = raw(
            addr,
            &format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"),
        )
        .await;
        assert_eq!(st, 200, "{body}");
        assert!(
            !body.contains("cv_guard"),
            "healthz lists the artifacts: {body}"
        );

        // The user's own browser: typed URL, a link followed, the page's own
        // subresources. Served, with the protective headers.
        for extra in [
            "",
            "Sec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
            "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n",
            "Sec-Fetch-Site: same-origin\r\nSec-Fetch-Mode: cors\r\nSec-Fetch-Dest: script\r\n",
        ] {
            let (st, head, body) = raw(
                addr,
                &format!("GET /a/cv_guard/ HTTP/1.1\r\nHost: localhost:{port}\r\n{extra}"),
            )
            .await;
            assert_eq!(st, 200, "{extra:?}: {body}");
            assert!(body.contains("guarded"), "{extra:?}: {body}");
            for h in [
                "x-content-type-options: nosniff",
                "referrer-policy: no-referrer",
                "cross-origin-resource-policy: same-origin",
                "content-security-policy: frame-ancestors",
                "cache-control: no-store",
            ] {
                assert!(head.contains(h), "{extra:?}: missing {h} in {head}");
            }
        }
        let (st, _, body) = raw(
            addr,
            &format!("HEAD /a/cv_guard/ HTTP/1.1\r\nHost: [::1]:{port}\r\n"),
        )
        .await;
        assert_eq!(st, 200, "{body}");
    }

    /// Audit of 2026-10-30, H3-5. A registered artifact served from the host
    /// filesystem followed symlinks out of its root: a link to a host file
    /// placed inside the artifact served that file. The path is now
    /// canonicalised and must stay under the root, like the runtime's server.
    #[tokio::test]
    async fn a_symlink_out_of_a_registered_artifact_is_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("hostfile"), b"HOST-FILE-CANARY").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>ok</h1>").unwrap();
        std::os::unix::fs::symlink(outside.path().join("hostfile"), dir.path().join("leak"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).unwrap();
        let (addr, reg) = start_private().await;
        reg.register(ArtifactEntry {
            artifact_id: "cv_link".into(),
            tenant: None,
            backend: None,
            root_path: dir.path().to_string_lossy().into_owned(),
            kind: ArtifactKind::Static,
            expires_at: None,
        })
        .await;
        for path in ["leak", "dir/hostfile"] {
            let r = reqwest::get(format!("http://{addr}/a/cv_link/{path}"))
                .await
                .unwrap();
            let st = r.status().as_u16();
            let body = r.text().await.unwrap();
            assert!(
                !body.contains("CANARY"),
                "{path}: served the host file: {body}"
            );
            assert_eq!(st, 403, "{path}: {body}");
        }
        let r = reqwest::get(format!("http://{addr}/a/cv_link/"))
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }
}
