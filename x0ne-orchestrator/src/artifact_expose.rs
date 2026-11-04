//! `ArtifactExpose` / `ArtifactUnexpose` — orchestrator-side primitives that
//! publish an x0ne artifact so a browser on the user's machine can reach it.
//!
//! Architectural invariant: these actions are executed on the HOST (by
//! `x0ne-orchestrator`), never inside the sandboxed container. The agent
//! running inside the sandbox only declares an intent — it has no CLI
//! credentials and cannot punch its own ports. Same shape as the Incus
//! primitives in `crate::incus_actions`: orchestrator-side because the
//! runtime can't (and shouldn't) reach outside its cage.
//!
//! Transport
//! ─────────
//! All exposure goes through `artifact_local_proxy`, a loopback-only HTTP
//! server embedded in `x0ne-orchestrator` (default `127.0.0.1:7800`).
//! The user's browser talks to `localhost`, and `x0ne` pulls bytes out
//! of the tenant container over whichever CLI is configured — Incus,
//! Podman or Docker. The choice is auto-detected from the first token
//! of `[bridge] sandbox_cmd` (see `container_runtime::detect_from_sandbox_cmd`).
//! No port is opened on the tenant host, no device/network rule is created.
//!
//! Policy (intentionally narrow — we iterate):
//! - `visibility`: "host" binds the local proxy on `127.0.0.1` (default
//!   and only supported value for now). "lan" / "public" are rejected
//!   until we add an edge-nginx story.
//! - `ttl_secs`: optional. The registry drops expired entries lazily
//!   on access, so no background task is needed.

use anyhow::Result;
use serde_json::{json, Value};
#[cfg(feature = "serve")]
use std::time::Duration;

#[cfg(feature = "serve")]
use crate::artifact_local_proxy::{self, expires_at_from_ttl, ArtifactEntry, ArtifactKind};
use crate::config::Config;
use crate::container_runtime::{self, ContainerRuntime};
use std::sync::Arc;

/// Resolve `(runtime, target)` from the configured sandbox command, if the
/// first token matches one of the three supported backends (Incus, Podman,
/// Docker). Returns `None` in stdio/http bridge modes.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
fn detect_runtime_from_config(config: &Config) -> Option<(Arc<dyn ContainerRuntime>, String)> {
    let cmd = config.bridge.sandbox_cmd.as_ref()?;
    container_runtime::detect_from_sandbox_cmd(cmd)
}

/// Ask the tenant's x0ne-runtime for the artifact's on-disk root. We go
/// through the registry file that `ArtifactCreate` writes, which already
/// exists as a contract between runtime and orchestrator.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
async fn lookup_artifact_in_container(
    rt: &Arc<dyn ContainerRuntime>,
    target: &str,
    artifact_id: &str,
) -> Result<Option<(String, Option<u16>)>> {
    let out = rt
        .exec(
            target,
            &[
                "sh",
                "-c",
                "cat /root/.x0ne/artifacts/registry.json 2>/dev/null || echo '{}'",
            ],
        )
        .await?;
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_else(|_| json!({}));
    let entry = v.pointer(&format!("/artifacts/{}", artifact_id));
    let Some(entry) = entry else {
        return Ok(None);
    };
    let root_path = entry
        .get("root_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("/root/.x0ne/artifacts/ephemeral/orphan/{}", artifact_id));
    let service_port = entry.get("port").and_then(|v| v.as_u64()).map(|p| p as u16);
    Ok(Some((root_path, service_port)))
}

/// `ArtifactExpose` — register the artifact with the local proxy so the
/// user's browser can reach `http://127.0.0.1:7800/a/<artifact_id>/`.
///
/// Idempotent: re-exposing a registered artifact is a no-op that refreshes
/// the TTL.
#[cfg(not(feature = "serve"))]
pub async fn execute_artifact_expose(_action: &Value, _config: &Config) -> Value {
    json!({
        "success": false,
        "error": {"message":
            "ArtifactExpose requires the `serve` feature (enabled by default). \
             Rebuild without --no-default-features."}
    })
}

#[cfg(not(feature = "serve"))]
pub async fn execute_artifact_unexpose(_action: &Value, _config: &Config) -> Value {
    json!({
        "success": false,
        "error": {"message": "ArtifactUnexpose requires the `serve` feature"}
    })
}

#[cfg(feature = "serve")]
pub async fn execute_artifact_expose(action: &Value, config: &Config) -> Value {
    let Some(artifact_id) = action.get("artifact_id").and_then(|v| v.as_str()) else {
        return json!({
            "success": false,
            "error": {"message": "ArtifactExpose requires `artifact_id`"}
        });
    };
    let visibility = action
        .get("visibility")
        .and_then(|v| v.as_str())
        .unwrap_or("host");
    let ttl_secs = action.get("ttl_secs").and_then(|v| v.as_u64());

    if visibility != "host" {
        return json!({
            "success": false,
            "error": {"message": format!(
                "visibility={:?} not supported yet; only 'host' (loopback) is available. \
                 'lan' / 'public' will come with the edge-nginx iteration.",
                visibility
            )}
        });
    }

    let Some((rt, target)) = detect_runtime_from_config(config) else {
        return json!({
            "success": false,
            "error": {"message":
                "ArtifactExpose requires sandbox mode. Configure [bridge] mode = \"sandbox\" \
                 with a sandbox_cmd whose first token is one of `incus`, `podman`, `docker`."}
        });
    };
    let backend = rt.backend();

    let (root_path, service_port) = match lookup_artifact_in_container(&rt, &target, artifact_id)
        .await
    {
        Ok(Some(v)) => v,
        Ok(None) => {
            return json!({
                "success": false,
                "error": {"message": format!(
                    "artifact '{}' not found in {} container '{}' — create it with ArtifactCreate first",
                    artifact_id, backend, target
                )}
            })
        }
        Err(e) => {
            return json!({
                "success": false,
                "error": {"message": format!("registry lookup failed: {}", e)}
            })
        }
    };

    let registry = match artifact_local_proxy::ensure_started().await {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "success": false,
                "error": {"message": format!("local proxy start failed: {}", e)}
            })
        }
    };

    let kind = match service_port {
        Some(port) => ArtifactKind::Service { port },
        None => ArtifactKind::Static,
    };

    registry
        .register(ArtifactEntry {
            artifact_id: artifact_id.to_string(),
            tenant: Some(target.clone()),
            backend: Some(backend),
            root_path: root_path.clone(),
            kind: kind.clone(),
            expires_at: expires_at_from_ttl(ttl_secs),
        })
        .await;

    // If a TTL was given we do NOT spawn a revocation task — the registry
    // drops expired entries lazily on access. That keeps memory bounded
    // under the "agent spams ArtifactExpose with tiny TTLs" failure mode.
    // We do, however, schedule a best-effort cleanup so the entry doesn't
    // linger forever if nobody ever hits the URL.
    if let Some(s) = ttl_secs {
        let cid = artifact_id.to_string();
        let reg = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(s) + Duration::from_secs(1)).await;
            reg.unregister(&cid).await;
        });
    }

    let url = artifact_local_proxy::url_for(artifact_id)
        .unwrap_or_else(|| format!("http://127.0.0.1:7800/a/{}/", artifact_id));

    let expires_at = ttl_secs.map(|s| {
        let t = chrono::Utc::now() + chrono::Duration::seconds(s as i64);
        t.to_rfc3339()
    });

    let kind_str = match kind {
        ArtifactKind::Static => "static",
        ArtifactKind::Service { .. } => "service",
    };

    json!({
        "success": true,
        "output": {
            "kind": "artifact_exposed",
            "artifact_id": artifact_id,
            "url": url,
            "visibility": visibility,
            "tenant": target,
            "backend": backend,
            "artifact_kind": kind_str,
            "expires_at": expires_at,
            "revoke_with": { "type": "ArtifactUnexpose", "artifact_id": artifact_id },
            "transport": format!(
                "loopback (x0ne-orchestrator → {} exec ... cat)",
                backend
            ),
        }
    })
}

/// `ArtifactUnexpose` — remove a artifact from the local proxy registry.
///
/// Without arguments, un-exposes the entire session (clears the registry).
/// With `artifact_id`, un-exposes only that artifact.
#[cfg(feature = "serve")]
pub async fn execute_artifact_unexpose(action: &Value, _config: &Config) -> Value {
    let Some(registry) = artifact_local_proxy::registry() else {
        return json!({
            "success": true,
            "output": {
                "kind": "artifact_unexposed",
                "note": "local proxy was not running — nothing to revoke"
            }
        });
    };

    if let Some(id) = action.get("artifact_id").and_then(|v| v.as_str()) {
        let removed = registry.unregister(id).await;
        return json!({
            "success": true,
            "output": {
                "kind": "artifact_unexposed",
                "artifact_id": id,
                "removed": removed,
            }
        });
    }

    let all = registry.list().await;
    let n = all.len();
    for e in all {
        registry.unregister(&e.artifact_id).await;
    }
    json!({
        "success": true,
        "output": {
            "kind": "artifact_unexposed",
            "all": true,
            "removed_count": n,
        }
    })
}

/// Small helper for telemetry / future logging.
#[allow(dead_code)]
pub fn describe_policy(visibility: &str, ttl_secs: Option<u64>) -> String {
    format!(
        "visibility={} ttl={}",
        visibility,
        ttl_secs
            .map(|s| format!("{}s", s))
            .unwrap_or_else(|| "unbounded".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::container_runtime::Backend;

    fn cfg_with_sandbox_cmd(cmd: &str) -> Config {
        let mut c = Config::default();
        c.bridge.sandbox_cmd = Some(cmd.to_string());
        c
    }

    fn detect_target(cmd: &str) -> Option<(Backend, String)> {
        let mut c = Config::default();
        c.bridge.sandbox_cmd = Some(cmd.to_string());
        detect_runtime_from_config(&c).map(|(rt, t)| (rt.backend(), t))
    }

    #[test]
    fn detect_parses_plain_incus_exec() {
        let (b, t) =
            detect_target("incus exec prod-infra:x0ne-sandbox -- /opt/x0ne-runtime --mode stdio")
                .expect("incus detected");
        assert_eq!(b, Backend::Incus);
        assert_eq!(t, "prod-infra:x0ne-sandbox");
    }

    #[test]
    fn detect_parses_with_env_flags() {
        let (b, t) = detect_target(
            "incus exec --env X0NE_ALLOW_LOOPBACK=1 --env FOO=bar prod-infra:x0ne-sandbox \
             -- /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio",
        )
        .expect("incus-with-env detected");
        assert_eq!(b, Backend::Incus);
        assert_eq!(t, "prod-infra:x0ne-sandbox");
    }

    #[test]
    fn detect_parses_podman_exec() {
        let (b, t) = detect_target("podman exec -it x0ne-sandbox /opt/x0ne-runtime --mode stdio")
            .expect("podman detected");
        assert_eq!(b, Backend::Podman);
        assert_eq!(t, "x0ne-sandbox");
    }

    #[test]
    fn detect_parses_docker_exec() {
        let (b, t) =
            detect_target("docker exec --user root worker-42 /opt/x0ne-runtime --mode stdio")
                .expect("docker detected");
        assert_eq!(b, Backend::Docker);
        assert_eq!(t, "worker-42");
    }

    #[test]
    fn detect_returns_none_when_not_supported_runtime() {
        // ssh, kubectl and friends are intentionally rejected — we do not
        // attempt to interpret their argument shape.
        assert!(detect_target("ssh host -- /opt/x0ne-runtime --mode stdio").is_none());
        assert!(detect_target("kubectl exec pod/x0ne -- /opt/x0ne-runtime").is_none());
    }

    #[test]
    fn describe_policy_includes_ttl() {
        assert!(describe_policy("host", Some(60)).contains("ttl=60s"));
        assert!(describe_policy("lan", None).contains("unbounded"));
    }

    #[tokio::test]
    async fn expose_rejects_non_host_visibility() {
        let cfg = cfg_with_sandbox_cmd("incus exec prod-infra:x0ne-sandbox -- /bin/sh");
        let res =
            execute_artifact_expose(&json!({"artifact_id": "cv_x", "visibility": "lan"}), &cfg)
                .await;
        assert_eq!(res["success"], json!(false));
        let msg = res["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains("not supported"), "got: {}", msg);
    }

    #[tokio::test]
    async fn expose_rejects_missing_artifact_id() {
        let cfg = cfg_with_sandbox_cmd("incus exec prod-infra:x0ne-sandbox -- /bin/sh");
        let res = execute_artifact_expose(&json!({}), &cfg).await;
        assert_eq!(res["success"], json!(false));
    }

    #[tokio::test]
    async fn expose_errors_when_not_sandbox() {
        let c = Config::default(); // no sandbox_cmd
        let res = execute_artifact_expose(&json!({"artifact_id": "cv_x"}), &c).await;
        assert_eq!(res["success"], json!(false));
        assert!(res["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sandbox mode"));
    }

    #[tokio::test]
    async fn unexpose_is_idempotent_when_proxy_never_started() {
        // In test environments `artifact_local_proxy::registry()` can be
        // `None` when nothing has started the proxy. Unexpose must still
        // succeed — it's documented as idempotent.
        let c = Config::default();
        let res = execute_artifact_unexpose(&json!({}), &c).await;
        assert_eq!(res["success"], json!(true));
    }

    #[tokio::test]
    async fn unexpose_with_specific_artifact_id_shape() {
        let c = Config::default();
        let res = execute_artifact_unexpose(&json!({"artifact_id": "cv_nonexistent"}), &c).await;
        assert_eq!(res["success"], json!(true));
        // Either the proxy isn't started (output.note present) or the id
        // wasn't registered (output.removed=false). Both are fine.
        assert_eq!(res["output"]["kind"], json!("artifact_unexposed"));
    }
}
