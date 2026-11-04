//! Request guard and token handling shared by the serve profiles.
//!
//! Rules live in `x0ne_actions::http_guard` (same as the runtime); this file
//! is the axum glue and the token file I/O.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use tracing::warn;
use x0ne_actions::http_guard::{self, HostPolicy, OriginPolicy};

/// Host allow-list + browser-origin refusal.
#[derive(Debug, Clone)]
pub struct RequestGuard {
    pub hosts: HostPolicy,
    pub origins: OriginPolicy,
}

impl RequestGuard {
    /// Loopback names with `port`, the literal bind address when specific,
    /// plus `X0NE_SERVE_ALLOWED_HOSTS`; origins from `X0NE_SERVE_CORS_ORIGINS`
    /// (empty by default: no web page may call the server).
    pub fn from_env(bind: std::net::SocketAddr) -> Self {
        let hosts_env = std::env::var("X0NE_SERVE_ALLOWED_HOSTS").unwrap_or_default();
        let origins_env = std::env::var("X0NE_SERVE_CORS_ORIGINS").unwrap_or_default();
        Self::new(
            bind,
            &http_guard::split_list(&hosts_env),
            &http_guard::split_list(&origins_env),
        )
    }

    pub fn new(bind: std::net::SocketAddr, extra_hosts: &[String], origins: &[String]) -> Self {
        let mut hosts = HostPolicy::loopback(bind.port()).with_extra(extra_hosts);
        if !bind.ip().is_unspecified() {
            // SocketAddr's Display brackets IPv6, which is the Host spelling.
            hosts = hosts.with_extra([bind.to_string()]);
        }
        Self {
            hosts,
            origins: OriginPolicy::new(origins),
        }
    }
}

/// Axum middleware: 403 with `{error, message}` when the guard refuses.
pub async fn guard_middleware(
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
                Json(json!({ "error": refusal.code(), "message": refusal.to_string() })),
            )
                .into_response()
        }
    }
}

/// `Authorization: Bearer` check against a mandatory token.
pub fn check_bearer(
    headers: &axum::http::HeaderMap,
    expected: &str,
) -> Result<(), (StatusCode, String)> {
    let supplied = headers.get("authorization").and_then(|v| v.to_str().ok());
    if http_guard::bearer_matches(supplied, expected) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".into(),
        ))
    }
}

/// The token a serve profile runs with: `supplied` when non-empty, else a
/// fresh one written to `path(port)` (0600). Returns the file when generated.
pub fn token_or_generate(
    supplied: Option<String>,
    name: &str,
    port: u16,
    resolve: impl FnOnce(&str, u16) -> Option<PathBuf>,
) -> Result<(String, Option<PathBuf>)> {
    if let Some(t) = supplied
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Ok((t, None));
    }
    let path = resolve(name, port).ok_or_else(|| {
        anyhow!("no {name} token given and neither XDG_RUNTIME_DIR nor HOME is set: pass one")
    })?;
    let token = generate_token();
    write_token_file(&path, &token)?;
    Ok((token, Some(path)))
}

/// 256 random bits, hex.
pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    http_guard::token_from_bytes(&bytes)
}

/// Writes `token` to `path`, parents 0700, file 0600 on unix.
pub fn write_token_file(path: &Path, token: &str) -> Result<()> {
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
        .map_err(|e| anyhow!("cannot write token file {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(token.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_serve_token_is_generated_into_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (token, path) = token_or_generate(None, "serve", 7700, |n, p| {
            Some(root.join(format!("{n}-{p}.token")))
        })
        .unwrap();
        let path = path.expect("generated");
        assert_eq!(path, dir.path().join("serve-7700.token"));
        assert_eq!(token.len(), 64);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let (t2, p2) =
            token_or_generate(Some(" given ".into()), "serve", 1, |_, _| panic!()).unwrap();
        assert_eq!((t2.as_str(), p2), ("given", None));
        // An empty token is not a token.
        let (t3, p3) = token_or_generate(Some("  ".into()), "serve", 1, |_, _| {
            Some(dir.path().join("x.token"))
        })
        .unwrap();
        assert!(!t3.is_empty() && p3.is_some());
    }

    #[test]
    fn a_specific_bind_is_reachable_under_its_address() {
        let g = RequestGuard::new("10.0.0.2:7700".parse().unwrap(), &[], &[]);
        assert!(g.hosts.check(Some("10.0.0.2:7700")).is_ok());
        let g = RequestGuard::new("0.0.0.0:7700".parse().unwrap(), &[], &[]);
        assert!(g.hosts.check(Some("0.0.0.0:7700")).is_err());
        assert!(g.hosts.check(Some("127.0.0.1:7700")).is_ok());
    }
}
