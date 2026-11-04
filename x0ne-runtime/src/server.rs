use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::executor;
use crate::handles::SharedHandles;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionResult, ErrorKind};
use x0ne_actions::http_guard;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<AppConfig>,
    pub policy: Arc<PolicyEngine>,
    pub handles: SharedHandles,
    pub rate_limiter: Arc<tokio::sync::Mutex<std::time::Instant>>,
    pub composites: CompositeStore,
}

// ── Bearer token authentication middleware ────────────────────────────────────

/// Bearer check. An empty or absent token refuses everything: the old
/// pass-through let any local process drive `/execute`.
async fn auth_middleware(State(state): State<ServerState>, req: Request, next: Next) -> Response {
    let expected_token = state
        .config
        .runtime
        .http
        .as_ref()
        .and_then(|h| h.auth_token.as_deref())
        .unwrap_or("");
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if http_guard::bearer_matches(auth_header, expected_token) {
        return next.run(req).await;
    }
    warn!("Unauthorized request — invalid or missing Bearer token");
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "unauthorized",
            "message": "Missing or invalid Authorization: Bearer <token> header"
        })),
    )
        .into_response()
}

/// Host allow-list + browser-origin refusal, on every route.
#[derive(Clone)]
struct RequestGuard {
    hosts: http_guard::HostPolicy,
    origins: http_guard::OriginPolicy,
}

async fn guard_middleware(
    State(guard): State<Arc<RequestGuard>>,
    req: Request,
    next: Next,
) -> Response {
    let h = req.headers();
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
    let verdict = guard
        .hosts
        .check(get("host"))
        .and_then(|_| guard.origins.check(get("origin"), get("sec-fetch-site")));
    match verdict {
        Ok(()) => next.run(req).await,
        Err(refusal) => {
            warn!(
                code = refusal.code(),
                "request refused by the HTTP guard: {refusal}"
            );
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": refusal.code(),
                    "message": refusal.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Fills `[runtime.http] auth_token` when absent: a random token written to a
/// 0600 file (`$XDG_RUNTIME_DIR/x0ne/runtime-<port>.token` or
/// `~/.x0ne/run/…`). The path, never the token, is printed on stderr.
pub fn ensure_http_token(config: &mut AppConfig) -> anyhow::Result<Option<std::path::PathBuf>> {
    ensure_http_token_with(config, |port| {
        http_guard::token_file_path_from_env("runtime", port)
    })
}

fn ensure_http_token_with(
    config: &mut AppConfig,
    resolve: impl FnOnce(u16) -> Option<std::path::PathBuf>,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let http = config.runtime.http.get_or_insert_with(|| {
        toml::from_str::<crate::config::HttpConfig>("").expect("HttpConfig has defaults")
    });
    if http
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return Ok(None);
    }
    let path = resolve(http.port).ok_or_else(|| {
        anyhow::anyhow!(
            "no [runtime.http] auth_token and neither XDG_RUNTIME_DIR nor HOME is set: \
             set auth_token explicitly"
        )
    })?;
    let token = generate_token();
    write_token_file(&path, &token)?;
    eprintln!(
        "x0ne-runtime: no [runtime.http] auth_token configured; generated one in {} \
         (clients send `Authorization: Bearer <contents>`; the orchestrator reads it \
         from there or from X0NE_CORE_TOKEN)",
        path.display()
    );
    http.auth_token = Some(token);
    Ok(Some(path))
}

fn generate_token() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut bytes = a.as_bytes().to_vec();
    bytes.extend_from_slice(b.as_bytes());
    http_guard::token_from_bytes(&bytes)
}

/// Writes `token` to `path`, creating parents; 0600 on unix.
pub fn write_token_file(path: &std::path::Path, token: &str) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| anyhow::anyhow!("cannot write token file {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(token.as_bytes())?;
    Ok(())
}

// ── HTTP server ───────────────────────────────────────────────────────────────

pub async fn serve_http(state: ServerState) -> anyhow::Result<()> {
    let config = state.config.clone();
    let http_cfg = config.runtime.http.as_ref();
    let host = http_cfg.map(|h| h.host.as_str()).unwrap_or("127.0.0.1");
    let port = http_cfg.map(|h| h.port).unwrap_or(7878);
    let addr = format!("{}:{}", host, port);

    // SECURITY: no token, no server. `main` generates one through
    // `ensure_http_token`; reaching here without is a wiring error.
    let has_auth = http_cfg
        .and_then(|h| h.auth_token.as_deref())
        .is_some_and(|t| !t.trim().is_empty());
    if !has_auth {
        anyhow::bail!(
            "refusing to serve x0ne-runtime HTTP on {addr} without an auth token: \
             set [runtime.http] auth_token (or let `x0ne-runtime --mode http` generate one)"
        );
    }

    let allowed_origins = http_cfg
        .map(|h| h.allowed_origins.clone())
        .unwrap_or_default();
    let mut hosts = http_guard::HostPolicy::loopback(port).with_extra(
        http_cfg
            .map(|h| h.allowed_hosts.clone())
            .unwrap_or_default(),
    );
    // A specific non-loopback bind is reached under its own address.
    if !matches!(host, "0.0.0.0" | "::" | "[::]") {
        let literal = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        hosts = hosts.with_extra([literal]);
    }
    let guard = Arc::new(RequestGuard {
        hosts,
        origins: http_guard::OriginPolicy::new(&allowed_origins),
    });

    let cors = if allowed_origins.iter().any(|o| o == "*") {
        CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    } else {
        let origins: Vec<_> = allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    };

    // Auth-protected routes
    let protected = Router::new()
        .route("/execute", post(handle_execute))
        .route("/handles", get(handle_list_handles))
        .route("/handles/{id}", get(handle_check_handle))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Public routes (health check does not require auth). The guard wraps
    // everything, CORS preflights included.
    let router = Router::new()
        .route("/health", get(handle_health))
        .merge(protected)
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            guard_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state);

    // SECURITY: the `--mode mcp` sibling refuses `policy.mode = "auto"`
    // outright (`AppConfig::check_headless_posture`); HTTP does not, because
    // its client is the orchestrator, which holds the human gate. With the
    // token now mandatory, only a holder of the token reaches `auto`.

    info!(
        "x0ne-runtime HTTP server listening on http://{} (auth: bearer, hosts: {:?})",
        addr,
        guard.hosts.allowed()
    );
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "x0ne-runtime" }))
}

async fn handle_list_handles(State(state): State<ServerState>) -> impl IntoResponse {
    let action = Action::ListHandles;
    let result = executor::execute(
        &action,
        &state.config,
        state.handles.clone(),
        &state.policy,
        &state.rate_limiter,
        &state.composites,
    )
    .await;
    Json(result)
}

async fn handle_check_handle(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let action = Action::CheckHandle { handle: id };
    let result = executor::execute(
        &action,
        &state.config,
        state.handles.clone(),
        &state.policy,
        &state.rate_limiter,
        &state.composites,
    )
    .await;
    Json(result)
}

async fn handle_execute(
    State(state): State<ServerState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<ActionResult>) {
    process_action_value(body, &state).await
}

/// Repair action shapes models reach for by reflex, before deserializing.
///
/// Models trained on other agent toolkits emit `EditFile` as a FLAT single
/// edit — `{path, old_str, new_str}` — instead of X0NE's `{path, edits:[…]}`.
/// Rejecting that costs a whole turn to rediscover, and one live session died
/// `deadline_exceeded` doing exactly that (2026-08-12). The two shapes mean
/// the same thing, so accept both and normalise here — one place, before the
/// typed vocabulary, so no executor learns about the alternative spelling.
fn repair_known_shapes(mut body: Value) -> Value {
    if body.get("type").and_then(|t| t.as_str()) != Some("EditFile") {
        return body;
    }
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    if obj.contains_key("edits") {
        return body; // already canonical
    }
    let old = obj
        .remove("old_str")
        .or_else(|| obj.remove("old"))
        .or_else(|| obj.remove("oldText"));
    let new = obj
        .remove("new_str")
        .or_else(|| obj.remove("new"))
        .or_else(|| obj.remove("newText"));
    if let (Some(old), Some(new)) = (old, new) {
        let mut edit = serde_json::Map::new();
        edit.insert("old".into(), old);
        edit.insert("new".into(), new);
        if let Some(all) = obj.remove("replace_all") {
            edit.insert("replace_all".into(), all);
        }
        obj.insert("edits".into(), Value::Array(vec![Value::Object(edit)]));
    }
    body
}

async fn process_action_value(
    body: Value,
    state: &ServerState,
) -> (StatusCode, Json<ActionResult>) {
    let mut body = repair_known_shapes(body);
    // A one-string command line becomes argv before the policy sees it,
    // so the gate, the grant offer and the executor all name `argv[0]`.
    if let Err(e) = x0ne_actions::command_line::normalize_action_value(&mut body) {
        let action_type = body
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        warn!(action_type, "command line refused: {}", e.message());
        let result = ActionResult::err(action_type, ErrorKind::InvalidInput, e.message(), 0);
        return (StatusCode::BAD_REQUEST, Json(result));
    }
    let action: Action = match serde_json::from_value(body.clone()) {
        Ok(a) => a,
        Err(e) => {
            warn!("Invalid action JSON: {}", e);
            let err_str = e.to_string();
            let action_type = body
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            // Detect common mistake: params wrapped in a sub-object
            let has_params_wrapper = body.get("params").is_some();
            let structural_hint = if has_params_wrapper && err_str.contains("missing field") {
                format!(
                    " HINT: You wrapped fields inside \"params\" — put them at the ROOT level instead. \
                     Example: {{\"type\":\"{}\", \"field\": value}} not {{\"type\":\"{}\", \"params\": {{\"field\": value}}}}",
                    action_type, action_type
                )
            } else {
                String::new()
            };

            let result = ActionResult::err(
                action_type,
                ErrorKind::InvalidInput,
                format!("Failed to parse action: {}{}", e, structural_hint),
                0,
            );
            return (StatusCode::BAD_REQUEST, Json(result));
        }
    };

    info!(action_type = action.type_name(), "Received action");

    match state.policy.evaluate(&action) {
        PolicyEffect::Allow => {}
        PolicyEffect::RequireConfirmation(msg) => {
            warn!(
                action_type = action.type_name(),
                "Action requires user approval"
            );
            let result = ActionResult::err(
                action.type_name(),
                ErrorKind::PolicyDenied,
                format!("Action requires user approval: {}", msg),
                0,
            );
            return (StatusCode::FORBIDDEN, Json(result));
        }
        PolicyEffect::Deny(reason) => {
            warn!(action_type = action.type_name(), reason = ?reason, "Action denied by policy");
            // SECURITY: `policy_denied`, not `err` — this is the ONE refusal
            // envelope the orchestrator sees for a top-level action, and it is
            // where `liftable` has to be attached. The field report of
            // 2026-08-16 came through exactly here: `ls` was waived by the
            // read-only dispensation, so P8a never ran, and the only thing the
            // operator ever saw was this envelope.
            let result = ActionResult::policy_denied(action.type_name(), &reason, 0);
            return (StatusCode::FORBIDDEN, Json(result));
        }
    }

    let result = executor::execute(
        &action,
        &state.config,
        state.handles.clone(),
        &state.policy,
        &state.rate_limiter,
        &state.composites,
    )
    .await;
    let status = if result.success {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    info!(
        action_type = result.action_type,
        success = result.success,
        duration_ms = result.duration_ms,
        "Action complete"
    );
    (status, Json(result))
}

// ── Stdio server ──────────────────────────────────────────────────────────────

/// Clear `O_NONBLOCK` on a raw fd. Returns true if the flag was set and is now
/// cleared — i.e. the caller's retry has a reason to succeed.
///
/// `O_NONBLOCK` lives on the open file *description*, which a forked child
/// shares with us, so a child can flip our own stdin/stdout to non-blocking and
/// die (or be killed) without ever putting it back. See `spawn_capture` in
/// executor/process.rs for the incident this comes from.
#[cfg(unix)]
fn clear_nonblocking(fd: std::os::unix::io::RawFd) -> bool {
    // SAFETY: fcntl on a fd we own, with no memory arguments.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || flags & libc::O_NONBLOCK == 0 {
            return false;
        }
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) == 0
    }
}

#[cfg(not(unix))]
fn clear_nonblocking(_fd: i32) -> bool {
    false
}

/// How many times ONE bridge read may re-arm its fd before giving up.
///
/// Per read, not per process: the counter is created inside the loop body and
/// dies with the line it was reading, so a session of a thousand actions gets
/// three re-arms for each of them. That is the intent — the fault is caused by
/// a child spawned for THIS action, so it recurs per action rather than
/// accumulating — but the doc-comment used to read as a global ceiling
/// (review round 3, M2). Bounded so a genuinely broken channel still
/// terminates instead of spinning; one re-arm is enough for the real fault,
/// the rest is margin.
const MAX_NONBLOCK_REARMS: u32 = 3;

/// SECURITY: put the bridge's own fds back in blocking mode after running an
/// action. This is the moment the damage happens — a child we just spawned and
/// possibly SIGKILLed is the only thing that can flip our fds — so repairing
/// here covers reads *and* writes without a reactive retry on the write path,
/// where a partial write followed by EAGAIN could not be retried without
/// risking duplicated bytes on the wire.
fn restore_bridge_blocking() {
    // 0/1 are the protocol itself; 2 is the log, and a non-blocking stderr
    // drops diagnostics silently — exactly when they are most needed.
    for fd in 0..=2 {
        if clear_nonblocking(fd) {
            warn!(
                fd,
                "a child process left this bridge fd non-blocking; restored to \
                 blocking so the orchestrator's channel survives"
            );
        }
    }
}

/// One parsed stdio line: a control message or an action, judged by the same
/// policy engine. `serve_stdio` calls only this; in-process bridges in tests
/// call it too, so they cross the real gate.
pub async fn handle_stdio_message(body: Value, state: &ServerState) -> ActionResult {
    if crate::control::is_mixed_line(&body) {
        return crate::control::mixed_line_refusal();
    }
    if crate::control::is_control_line(&body) {
        crate::control::handle_control(&body, &state.policy)
    } else {
        let (_status, Json(result)) = process_action_value(body, state).await;
        result
    }
}

pub async fn serve_stdio(state: ServerState) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    info!("x0ne-runtime starting in stdio mode. Send JSON actions on stdin.");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = tokio::io::BufWriter::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        // SECURITY: this loop is the orchestrator's safety net — the channel it
        // uses to stop a runaway machine. It must not die of an EAGAIN it can
        // repair. `read_line` truncates `line` back to its original length on
        // io error (tokio io/util/read_line.rs), so the retry starts clean.
        let n = {
            let mut rearms = 0u32;
            loop {
                match reader.read_line(&mut line).await {
                    Ok(k) => break k,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && rearms < MAX_NONBLOCK_REARMS
                            && clear_nonblocking(0) =>
                    {
                        rearms += 1;
                        warn!(
                            "bridge stdin was left non-blocking by a child process; \
                             re-armed as blocking and retrying (attempt {})",
                            rearms
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let body: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                error!("Invalid JSON on stdin: {}", e);
                let result = ActionResult::err(
                    "unknown",
                    ErrorKind::InvalidInput,
                    format!("Invalid JSON: {}", e),
                    0,
                );
                // SECURITY: repair before writing, exactly like the action path
                // below. This branch runs no action, so in the ordinary case
                // the fds are already blocking — but "ordinary case" is the
                // assumption that produced the original EAGAIN death, and the
                // write here is on the same channel. One call, no cost when
                // there is nothing to repair (review round 3, M1).
                restore_bridge_blocking();
                let out = serde_json::to_string(&result).unwrap_or_default();
                writer.write_all(out.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                continue;
            }
        };

        // SECURITY: the control vocabulary is parsed HERE and nowhere else.
        // `process_action_value` is shared with the HTTP handler, so putting
        // this branch there would have made `PolicyGrant` reachable over a
        // socket; keeping it in the stdio read loop makes "stdio only" a
        // property of the code shape rather than of a flag someone can flip.
        // `PolicyEngine::apply_grant` refuses non-stdio modes as well — two
        // enforcements, because this repository has repeatedly lost the single
        // one to a refactor (see `control.rs`).
        let result = handle_stdio_message(body, &state).await;
        restore_bridge_blocking();

        let out = serde_json::to_string(&result).unwrap_or_default();
        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod nonblock_repair_tests {
    use super::clear_nonblocking;

    /// The repair the stdio loop leans on. A failure means a child that flips
    /// the bridge to non-blocking can still kill the runtime with EAGAIN, and
    /// the orchestrator loses the channel it stops runaway machines with
    /// (2026-08-14, H100 milestone).
    #[test]
    fn a_poisoned_fd_is_disarmed_once_and_reports_it() {
        let mut fds = [0i32; 2];
        // SAFETY: pipe(2) into a two-element array, the shape it documents.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // A clean fd has nothing to repair — the loop must not log noise or
        // loop forever on an error that is not this one.
        assert!(
            !clear_nonblocking(read_fd),
            "a blocking fd was reported as repaired"
        );

        // SAFETY: fcntl on a fd we just created.
        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL);
            libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        assert!(
            clear_nonblocking(read_fd),
            "a poisoned fd was not repaired — the runtime would die on the next read"
        );
        // SAFETY: reading back the flags we just wrote.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        assert_eq!(
            flags & libc::O_NONBLOCK,
            0,
            "O_NONBLOCK survived the repair"
        );
        assert!(
            !clear_nonblocking(read_fd),
            "the repair is not idempotent — a second call claimed work it did not do"
        );

        // SAFETY: closing fds this test owns.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }
}

#[cfg(test)]
mod shape_repair_tests {
    use super::repair_known_shapes;
    use crate::action::Action;
    use serde_json::json;

    #[test]
    fn a_flat_edit_becomes_the_canonical_edits_array() {
        // The exact shape a live session emitted before dying on it.
        let repaired = repair_known_shapes(json!({
            "type": "EditFile",
            "path": "src/lib.rs",
            "old_str": "let a = 1;",
            "new_str": "let a = 2;"
        }));
        let action: Action = serde_json::from_value(repaired).expect("must parse now");
        let Action::EditFile { path, edits, .. } = action else {
            panic!("wrong variant")
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, "let a = 1;");
        assert_eq!(edits[0].new, "let a = 2;");
    }

    #[test]
    fn a_canonical_edit_is_left_alone() {
        let original = json!({
            "type": "EditFile",
            "path": "p",
            "edits": [{"old": "a", "new": "b"}]
        });
        assert_eq!(repair_known_shapes(original.clone()), original);
    }

    #[test]
    fn replace_all_survives_the_repair() {
        let repaired = repair_known_shapes(json!({
            "type": "EditFile", "path": "p",
            "old_str": "a", "new_str": "b", "replace_all": true
        }));
        let action: Action = serde_json::from_value(repaired).unwrap();
        let Action::EditFile { edits, .. } = action else {
            panic!()
        };
        assert_eq!(edits[0].replace_all, Some(true));
    }

    #[test]
    fn other_actions_are_untouched() {
        let read = json!({"type": "ReadFile", "path": "x", "old_str": "noise"});
        assert_eq!(repair_known_shapes(read.clone()), read);
    }

    #[test]
    fn a_half_flat_edit_is_not_invented() {
        // Only `old_str` and no replacement: repairing it would guess. Leave
        // it alone so the parse error names the real problem.
        let half = json!({"type": "EditFile", "path": "p", "old_str": "a"});
        let out = repair_known_shapes(half);
        assert!(out.get("edits").is_none());
    }
}

/// The HTTP mode must be closed to other local processes and to web
/// pages (DNS rebinding, cross-site fetch), whatever the config says.
#[cfg(test)]
mod http_guard_tests {
    use super::*;
    use crate::config::HttpConfig;
    use crate::handles::new_shared;

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn state(port: u16, extra_http_toml: &str, dir: &std::path::Path) -> ServerState {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "http".into();
        let http: HttpConfig = toml::from_str(&format!(
            "host = \"127.0.0.1\"\nport = {port}\n{extra_http_toml}"
        ))
        .unwrap();
        cfg.runtime.http = Some(http);
        cfg.snapshots.dir = dir.join("snap").to_string_lossy().to_string();
        cfg.composites.primitives_dir = dir.join("prim").to_string_lossy().to_string();
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let handles = new_shared(
            cfg.events.max_events_per_handle,
            cfg.handles.completed_retention_secs,
            cfg.handles.max_concurrent,
        );
        let composites = CompositeStore::new(&cfg);
        ServerState {
            config: Arc::new(cfg),
            policy: Arc::new(policy),
            handles,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(std::time::Instant::now())),
            composites,
        }
    }

    /// Starts `serve_http` on a free port; waits until it accepts.
    async fn start(extra: &str) -> (u16, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let st = state(port, extra, dir.path());
        tokio::spawn(serve_http(st));
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return (port, dir);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("runtime HTTP did not start");
    }

    async fn execute(port: u16, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut rb = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/execute"))
            .json(&serde_json::json!({"type": "ListActions"}));
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        rb.send().await.unwrap()
    }

    #[test]
    fn a_missing_token_is_generated_into_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AppConfig::defaults();
        cfg.runtime.http = None;
        let target = dir.path().join("x0ne").join("runtime-7878.token");
        let t2 = target.clone();
        let written = ensure_http_token_with(&mut cfg, move |port| {
            assert_eq!(port, 7878);
            Some(t2)
        })
        .unwrap();
        assert_eq!(written.as_deref(), Some(target.as_path()));
        let token = cfg
            .runtime
            .http
            .as_ref()
            .unwrap()
            .auth_token
            .clone()
            .unwrap();
        assert_eq!(token.len(), 64, "256 bits, hex");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file readable by others");
        }
        // An operator token is never replaced.
        let again = ensure_http_token_with(&mut cfg, |_| panic!("must not resolve")).unwrap();
        assert!(again.is_none());
        assert_eq!(cfg.runtime.http.unwrap().auth_token.unwrap(), token);
    }

    #[tokio::test]
    async fn configured_hosts_and_origins_are_let_through() {
        let (port, _d) = start(
            "auth_token = \"tok-178-runtime\"\n\
             allowed_hosts = [\"runtime.lan\"]\n\
             allowed_origins = [\"https://console.example\"]",
        )
        .await;
        let r = execute(port, &[BEARER, ("host", "runtime.lan")]).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = execute(port, &[BEARER, ("origin", "https://console.example")]).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = execute(port, &[BEARER, ("origin", "https://other.example")]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    const TOK: &str = "auth_token = \"tok-178-runtime\"";
    const BEARER: (&str, &str) = ("authorization", "Bearer tok-178-runtime");

    /// A runtime with no token used to run any local process's `/execute`.
    #[tokio::test]
    async fn serve_http_refuses_to_start_without_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(free_port(), "", dir.path());
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), serve_http(st)).await;
        match r {
            Ok(Err(e)) => assert!(e.to_string().contains("token"), "{e}"),
            Ok(Ok(())) => panic!("served and returned without a token"),
            Err(_) => panic!("served /execute without a token"),
        }
    }

    #[tokio::test]
    async fn the_token_and_a_loopback_host_pass() {
        let (port, _d) = start(TOK).await;
        let r = execute(port, &[BEARER]).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = execute(port, &[]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    /// DNS rebinding: the attacker's page reaches 127.0.0.1 with its own Host.
    #[tokio::test]
    async fn a_rebound_host_is_refused_even_with_the_token() {
        let (port, _d) = start(TOK).await;
        let host = format!("attacker.example:{port}");
        let r = execute(port, &[BEARER, ("host", &host)]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let wrong_port = format!("127.0.0.1:{}", port + 1);
        let r = execute(port, &[BEARER, ("host", &wrong_port)]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_browser_origin_is_refused_unless_configured() {
        let (port, _d) = start(TOK).await;
        let r = execute(port, &[BEARER, ("origin", "http://evil.example")]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = execute(port, &[BEARER, ("sec-fetch-site", "cross-site")]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // The old default CORS list must not survive as a browser allow-list.
        let r = execute(port, &[BEARER, ("origin", "http://localhost")]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}

/// A one-string `RunCommand` is decided on `argv[0]`, end to end
/// through the entry the orchestrator's bridge uses.
#[cfg(test)]
mod command_line_tests {
    use super::*;
    use crate::handles::new_shared;
    use serde_json::json;

    fn state(allowed: &[&str], dir: &std::path::Path) -> ServerState {
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = String::new();
        cfg.policy.default_effect = "allow".into();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = allowed.iter().map(|s| s.to_string()).collect();
        cfg.policy.allowed_read_paths = vec!["/".into()];
        cfg.policy.allowed_write_paths = vec![dir.to_string_lossy().to_string()];
        cfg.snapshots.dir = dir.join("snap").to_string_lossy().to_string();
        cfg.composites.primitives_dir = dir.join("prim").to_string_lossy().to_string();
        // No bubblewrap in the test container: confinement by a marked root.
        std::fs::create_dir_all(dir.join("probe/run")).unwrap();
        std::fs::write(dir.join("probe/run/.containerenv"), "").unwrap();
        cfg.isolation.mode = crate::config::IsolationMode::Container;
        cfg.isolation.container_probe_root = Some(dir.join("probe"));
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let handles = new_shared(
            cfg.events.max_events_per_handle,
            cfg.handles.completed_retention_secs,
            cfg.handles.max_concurrent,
        );
        let composites = CompositeStore::new(&cfg);
        ServerState {
            config: Arc::new(cfg),
            policy: Arc::new(policy),
            handles,
            rate_limiter: Arc::new(tokio::sync::Mutex::new(std::time::Instant::now())),
            composites,
        }
    }

    async fn run(allowed: &[&str], line: &str) -> (StatusCode, Value, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let st = state(allowed, dir.path());
        let (status, Json(result)) =
            process_action_value(json!({"type":"RunCommand","command":line}), &st).await;
        (status, serde_json::to_value(result).unwrap(), dir)
    }

    /// The measured loop of 2026-10-10. A failure means the "allow for
    /// session" offer names the whole line, and the grant is refused.
    #[tokio::test]
    async fn the_refusal_offers_the_program_not_the_line() {
        let (status, r, _d) = run(&[], "python3 /work/p/fib.py").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{r}");
        let offered = r["error"]["liftable"]["value"].as_str().unwrap_or_default();
        assert_eq!(offered, "python3", "{r}");
        assert!(x0ne_actions::policy_grant::validate_command_grant(offered).is_ok());
    }

    #[tokio::test]
    async fn an_allowed_program_runs_with_its_arguments() {
        let (status, r, _d) = run(&["echo"], "echo hello 'big world'").await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(
            r["output"]["stdout"].as_str().unwrap().trim(),
            "hello big world"
        );
    }

    /// SECURITY: an allowed first word does not carry a second command.
    #[tokio::test]
    async fn an_allowed_program_cannot_chain_a_second_one() {
        for tail in [
            "; touch {m}",
            "&& touch {m}",
            "| touch {m}",
            "$(touch {m})",
            "`touch {m}`",
            "> {m}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("pwned");
            let line = format!("echo hi {}", tail.replace("{m}", &marker.to_string_lossy()));
            let st = state(&["echo", "touch"], dir.path());
            let (status, Json(result)) =
                process_action_value(json!({"type":"RunCommand","command":line}), &st).await;
            let r = serde_json::to_value(result).unwrap();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{line}: {r}");
            assert!(
                r["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("shell syntax"),
                "{r}"
            );
            assert!(!marker.exists(), "{line} ran a second command");
        }
    }

    /// SECURITY: a path in program position is not the allowed bare name, and
    /// it is not grantable from the prompt either.
    #[tokio::test]
    async fn a_relative_path_is_not_the_allowed_program() {
        let (status, r, _d) = run(&["python3"], "../bin/python3 x.py").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{r}");
        let offered = r["error"]["liftable"]["value"].as_str().unwrap();
        assert_eq!(offered, "../bin/python3");
        assert!(x0ne_actions::policy_grant::validate_command_grant(offered).is_err());
    }

    /// Steps of a composite go through the same normalization.
    #[tokio::test]
    async fn a_pipeline_step_is_normalized_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = state(&["echo"], dir.path());
        Arc::get_mut(&mut st.config)
            .unwrap()
            .actions
            .insert("Pipeline".into(), true);
        st.policy = Arc::new(PolicyEngine::new(
            (*st.config).clone(),
            st.config.actions.clone(),
        ));
        let body = json!({"type":"Pipeline","steps":[{"type":"RunCommand","command":"echo a b"}]});
        let (status, Json(result)) = process_action_value(body, &st).await;
        let r = serde_json::to_value(result).unwrap();
        assert_eq!(status, StatusCode::OK, "{r}");
        let hostile =
            json!({"type":"Pipeline","steps":[{"type":"RunCommand","command":"echo a; id"}]});
        let (status, _) = process_action_value(hostile, &st).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
