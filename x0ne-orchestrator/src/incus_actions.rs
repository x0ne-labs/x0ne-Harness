//! Orchestrator-side Incus container management.
//!
//! Container lifecycle — snapshot/restore/list/delete AND ephemeral
//! run + parallel run — lives here, not in x0ne-runtime, because the
//! runtime may itself run **inside** an Incus container. A process
//! inside a container has neither the privilege nor the topological
//! awareness to safely operate on containers; those are orchestrator
//! concerns.
//!
//! Each `execute_*` function takes the raw action JSON, calls the host's
//! `incus` CLI, and returns a JSON value in the same shape as
//! x0ne-runtime's action results so the LLM sees a unified surface.

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::config::Config;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn pick_remote(action: &Value, config: &Config) -> String {
    action
        .get("remote")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&config.incus.default_remote)
        .to_string()
}

async fn incus_cli_capture(args: &[&str]) -> Result<String, String> {
    match Command::new("incus").args(args).output().await {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => Err(format!("failed to spawn `incus`: {e}")),
    }
}

fn err_envelope(action_type: &str, message: String) -> Value {
    json!({
        "success": false,
        "action_type": action_type,
        "error": {
            "kind": "internal",
            "class": "runtime_error",
            "code": "incus_cli_error",
            "message": message,
            "action_name": action_type,
            "retryable": false,
            "recommended_next_step": "Inspect the message; verify the remote and container exist via IncusListSnapshots."
        }
    })
}

fn validation_err(action_type: &str, message: String) -> Value {
    json!({
        "success": false,
        "action_type": action_type,
        "error": {
            "kind": "invalid_input",
            "class": "validation_error",
            "code": "invalid_input",
            "message": message,
            "action_name": action_type,
            "retryable": false,
            "recommended_next_step": "Call DescribeAction to inspect the schema and fix the arguments."
        }
    })
}

fn ok_envelope(action_type: &str, output: Value, duration_ms: u64) -> Value {
    json!({
        "success": true,
        "action_type": action_type,
        "output": output,
        "duration_ms": duration_ms,
    })
}

// ── Public dispatch entry points ──────────────────────────────────────────────

pub async fn execute_incus_create_snapshot(action: &Value, config: &Config) -> Value {
    const NAME: &str = "IncusCreateSnapshot";
    let Some(container) = action.get("container").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `container`".into());
    };
    let Some(snapshot_name) = action.get("name").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `name`".into());
    };
    let remote = pick_remote(action, config);
    let target = format!("{}:{}", remote, container);

    let t0 = std::time::Instant::now();
    match incus_cli_capture(&["snapshot", "create", &target, snapshot_name]).await {
        Ok(_) => ok_envelope(
            NAME,
            json!({
                "kind": "snapshot_created",
                "name": snapshot_name,
                "container": container,
                "remote": remote,
                "method": "btrfs-subvolume",
                "scope": "incus",
                "created_at": chrono::Utc::now().to_rfc3339(),
            }),
            t0.elapsed().as_millis() as u64,
        ),
        Err(stderr) => err_envelope(
            NAME,
            format!(
                "incus snapshot create {target} {snapshot_name} failed: {}",
                stderr.trim()
            ),
        ),
    }
}

pub async fn execute_incus_restore_snapshot(action: &Value, config: &Config) -> Value {
    const NAME: &str = "IncusRestoreSnapshot";
    let Some(container) = action.get("container").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `container`".into());
    };
    let Some(snapshot_name) = action.get("name").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `name`".into());
    };
    let remote = pick_remote(action, config);
    let target = format!("{}:{}", remote, container);

    let t0 = std::time::Instant::now();
    match incus_cli_capture(&["snapshot", "restore", &target, snapshot_name]).await {
        Ok(_) => ok_envelope(
            NAME,
            json!({
                "kind": "snapshot_restored",
                "name": snapshot_name,
                "container": container,
                "remote": remote,
                "method": "btrfs-subvolume",
                "scope": "incus",
                "details": [{
                    "path": target,
                    "status": "subvolume restored (atomic)"
                }],
            }),
            t0.elapsed().as_millis() as u64,
        ),
        Err(stderr) => err_envelope(
            NAME,
            format!(
                "incus snapshot restore {target} {snapshot_name} failed: {}",
                stderr.trim()
            ),
        ),
    }
}

pub async fn execute_incus_list_snapshots(action: &Value, config: &Config) -> Value {
    const NAME: &str = "IncusListSnapshots";
    let Some(container) = action.get("container").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `container`".into());
    };
    let remote = pick_remote(action, config);
    let target = format!("{}:{}", remote, container);

    let t0 = std::time::Instant::now();
    match incus_cli_capture(&["snapshot", "list", &target, "-f", "json"]).await {
        Ok(stdout) => {
            let parsed: Vec<Value> = serde_json::from_str(&stdout).unwrap_or_default();
            let snapshots: Vec<Value> = parsed
                .into_iter()
                .filter_map(|v| {
                    let name = v.get("name")?.as_str()?.to_string();
                    let created_at = v
                        .get("created_at")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(json!({
                        "name": name,
                        "created_at": created_at,
                        "files_count": 0,
                        "total_bytes": 0,
                    }))
                })
                .collect();
            ok_envelope(
                NAME,
                json!({
                    "kind": "snapshot_list",
                    "container": container,
                    "remote": remote,
                    "scope": "incus",
                    "snapshots": snapshots,
                }),
                t0.elapsed().as_millis() as u64,
            )
        }
        Err(stderr) => err_envelope(
            NAME,
            format!("incus snapshot list {target} failed: {}", stderr.trim()),
        ),
    }
}

pub async fn execute_incus_delete_snapshot(action: &Value, config: &Config) -> Value {
    const NAME: &str = "IncusDeleteSnapshot";
    let Some(container) = action.get("container").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `container`".into());
    };
    let Some(snapshot_name) = action.get("name").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `name`".into());
    };
    let remote = pick_remote(action, config);
    let target = format!("{}:{}", remote, container);

    let t0 = std::time::Instant::now();
    match incus_cli_capture(&["snapshot", "delete", &target, snapshot_name]).await {
        Ok(_) => ok_envelope(
            NAME,
            json!({
                "kind": "snapshot_deleted",
                "name": snapshot_name,
                "container": container,
                "remote": remote,
                "scope": "incus",
                "deleted": true,
            }),
            t0.elapsed().as_millis() as u64,
        ),
        Err(stderr) => err_envelope(
            NAME,
            format!(
                "incus snapshot delete {target} {snapshot_name} failed: {}",
                stderr.trim()
            ),
        ),
    }
}

// ── Container lifecycle helpers ───────────────────────────────────────────────

/// RAII guard that force-deletes an Incus container on Drop. Survives
/// panics and timeouts: every code path that allocated a container
/// implicitly cleans it up unless `disarm()` was called explicitly.
struct ContainerGuard {
    remote: String,
    name: String,
    armed: bool,
}

impl ContainerGuard {
    fn new(remote: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            name: name.into(),
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
    fn full_name(&self) -> String {
        format!("{}:{}", self.remote, self.name)
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if self.armed {
            // std::process (blocking) is acceptable in Drop context.
            let _ = std::process::Command::new("incus")
                .args(["delete", &self.full_name(), "--force"])
                .output();
        }
    }
}

fn new_container_name() -> String {
    // v7 is timestamp-prefixed + random — naturally sortable and unique.
    let id = uuid::Uuid::now_v7();
    let hex = id.simple().to_string();
    format!("x0ne-{}", &hex[..8])
}

fn image_allowed(image: &str, config: &Config) -> bool {
    let allowed = &config.incus.allowed_images;
    if allowed.is_empty() {
        // fail-closed: empty list = all denied
        return false;
    }
    allowed.iter().any(|a| {
        if a.contains('*') {
            let prefix = a.trim_end_matches('*');
            image.starts_with(prefix)
        } else {
            a == image
        }
    })
}

fn is_safe_filename(name: &str) -> bool {
    let p = std::path::Path::new(name);
    !p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && !name.is_empty()
}

// ── Low-level CLI wrappers ────────────────────────────────────────────────────

async fn incus_launch(
    remote: &str,
    name: &str,
    image: &str,
    network: &str,
    memory_mb: u64,
    cpu_limit: u32,
) -> std::io::Result<()> {
    let full_name = format!("{}:{}", remote, name);
    let mem_arg = format!("limits.memory={}MiB", memory_mb);
    let cpu_arg = format!("limits.cpu={}", cpu_limit);
    let mut args = vec![
        "launch".to_string(),
        image.to_string(),
        full_name,
        "--ephemeral".to_string(),
        "-c".to_string(),
        mem_arg,
        "-c".to_string(),
        cpu_arg,
    ];
    if network == "none" {
        args.push("-c".to_string());
        args.push("raw.lxc=lxc.net.0.type=empty".to_string());
    }
    let status = Command::new("incus").args(&args).status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "incus launch failed (exit {:?})",
            status.code()
        )))
    }
}

/// Clone an ephemeral container from an existing snapshot (CoW on btrfs).
/// `base_snapshot` format: "container-name/snapshot-name" (on the same remote).
async fn incus_copy_snapshot(
    remote: &str,
    name: &str,
    base_snapshot: &str,
    memory_mb: u64,
    cpu_limit: u32,
) -> std::io::Result<()> {
    let source = format!("{}:{}", remote, base_snapshot);
    let dest = format!("{}:{}", remote, name);
    let mem_arg = format!("limits.memory={}MiB", memory_mb);
    let cpu_arg = format!("limits.cpu={}", cpu_limit);
    let status = Command::new("incus")
        .args([
            "copy",
            &source,
            &dest,
            "--ephemeral",
            "-c",
            &mem_arg,
            "-c",
            &cpu_arg,
            "--mode",
            "pull",
        ])
        .status()
        .await?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "incus copy failed (source: {})",
            base_snapshot
        )));
    }
    let full_name = format!("{}:{}", remote, name);
    let start = Command::new("incus")
        .args(["start", &full_name])
        .status()
        .await?;
    if start.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("incus start failed after copy"))
    }
}

async fn incus_push_file(
    remote: &str,
    container: &str,
    filename: &str,
    content: &str,
) -> std::io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new()?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    let dest = format!("{}:{}/workspace/{}", remote, container, filename);
    let status = Command::new("incus")
        .args([
            "file",
            "push",
            tmp.path().to_str().unwrap_or(""),
            &dest,
            "--create-dirs",
        ])
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "file push failed: {}",
            filename
        )))
    }
}

async fn incus_exec(
    remote: &str,
    container: &str,
    command: &[String],
    env: &HashMap<String, String>,
) -> std::io::Result<(i32, String, String)> {
    let full_name = format!("{}:{}", remote, container);
    let mut args = vec!["exec".to_string(), full_name, "--".to_string()];
    if !env.is_empty() {
        // SECURITY: a refusal, not the silent skip this used to do. Dropping the
        // key quietly runs the command anyway with a different environment than
        // the one asked for — the caller reads a success and cannot tell that
        // `LD_PRELOAD` was attempted, which is precisely the event worth seeing.
        if let Err(why) = x0ne_actions::env_keys::check_caller_env(env) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why));
        }
        args.push("env".to_string());
        for (k, v) in env {
            args.push(format!("{}={}", k, v));
        }
    }
    args.extend(command.iter().cloned());
    let output = Command::new("incus").args(&args).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);
    Ok((exit_code, stdout, stderr))
}

async fn incus_pull_file(
    remote: &str,
    container: &str,
    container_path: &str,
) -> std::io::Result<String> {
    let source = format!("{}:{}{}", remote, container, container_path);
    let tmp = tempfile::NamedTempFile::new()?;
    let tmp_path = tmp.path().to_str().unwrap_or("").to_string();
    let status = Command::new("incus")
        .args(["file", "pull", &source, &tmp_path])
        .status()
        .await?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Could not pull {} from container", container_path),
        ));
    }
    std::fs::read_to_string(&tmp_path)
}

async fn incus_snapshot_after_run(
    remote: &str,
    container: &str,
    snapshot_name: &str,
) -> std::io::Result<()> {
    let full_name = format!("{}:{}", remote, container);
    let status = Command::new("incus")
        .args(["snapshot", "create", &full_name, snapshot_name])
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("snapshot create failed"))
    }
}

// ── RunIncus (single ephemeral container) ─────────────────────────────────────

pub async fn execute_run_incus(action: &Value, config: &Config) -> Value {
    const NAME: &str = "RunIncus";
    let icfg = &config.incus;

    let Some(image) = action.get("image").and_then(|v| v.as_str()) else {
        return validation_err(NAME, "missing required field `image`".into());
    };

    if !image_allowed(image, config) {
        return json!({
            "success": false, "action_type": NAME,
            "error": {
                "kind": "incus_image_denied",
                "class": "policy_denied",
                "code": "incus_image_denied",
                "message": format!(
                    "Image '{image}' is not in incus.allowed_images. \
                     Add it to the orchestrator's IncusConfig to permit use."),
                "action_name": NAME,
                "retryable": false,
                "recommended_next_step": "Pick an allowed image, or have the operator add this image to incus.allowed_images."
            }
        });
    }

    let command: Vec<String> = action
        .get("command")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let files: HashMap<String, String> = action
        .get("files")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let env: HashMap<String, String> = action
        .get("env")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let collect_files: Vec<String> = action
        .get("collect_files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let base_snapshot = action
        .get("base_snapshot")
        .and_then(|v| v.as_str())
        .map(String::from);
    let snapshot_on_exit = action
        .get("snapshot_on_exit")
        .and_then(|v| v.as_str())
        .map(String::from);

    let remote_name = pick_remote(action, config);
    let net_mode = action
        .get("network")
        .and_then(|v| v.as_str())
        .unwrap_or(&icfg.default_network)
        .to_string();
    let mem = action
        .get("memory_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(icfg.default_memory_mb)
        .min(icfg.max_memory_mb);
    let cpus = action
        .get("cpu_limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(icfg.default_cpu_limit);
    let timeout_dur = Duration::from_secs(
        action
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(icfg.default_timeout_secs)
            .min(icfg.max_timeout_secs),
    );

    let container = new_container_name();
    let mut guard = ContainerGuard::new(&remote_name, &container);
    let t_start = Instant::now();

    let effective_snapshot = base_snapshot.as_deref().filter(|s| !s.is_empty());
    let launch_result = if let Some(snap) = effective_snapshot {
        timeout(
            Duration::from_secs(30),
            incus_copy_snapshot(&remote_name, &container, snap, mem, cpus),
        )
        .await
    } else {
        timeout(
            Duration::from_secs(120),
            incus_launch(&remote_name, &container, image, &net_mode, mem, cpus),
        )
        .await
    };

    let startup_ms = t_start.elapsed().as_millis() as u64;
    match launch_result {
        Err(_) => {
            guard.disarm();
            return json!({
                "success": false, "action_type": NAME,
                "error": {
                    "kind": "timeout", "class": "runtime_error", "code": "timeout",
                    "message": "Container failed to start within time limit",
                    "action_name": NAME, "retryable": true,
                    "recommended_next_step": "Increase timeout_secs or check the Incus host's load."
                },
                "duration_ms": startup_ms
            });
        }
        Ok(Err(e)) => {
            guard.disarm();
            return json!({
                "success": false, "action_type": NAME,
                "error": {
                    "kind": "execution_failed", "class": "runtime_error", "code": "execution_failed",
                    "message": format!("Launch failed: {}", e),
                    "action_name": NAME, "retryable": false,
                    "recommended_next_step": "Inspect the error message; verify the image and remote are reachable."
                },
                "duration_ms": startup_ms
            });
        }
        Ok(Ok(())) => {}
    }

    for (name, content) in &files {
        if !is_safe_filename(name) {
            continue;
        }
        if let Err(e) = incus_push_file(&remote_name, &container, name, content).await {
            return json!({
                "success": false, "action_type": NAME,
                "error": {
                    "kind": "execution_failed", "class": "runtime_error", "code": "execution_failed",
                    "message": format!("Failed to inject file '{}': {}", name, e),
                    "action_name": NAME, "retryable": false,
                    "recommended_next_step": "Check the filename for forbidden patterns (absolute, ..)."
                },
                "duration_ms": startup_ms
            });
        }
    }

    let exec_result = if command.is_empty() {
        Ok((0i32, String::new(), String::new()))
    } else {
        timeout(
            timeout_dur,
            incus_exec(&remote_name, &container, &command, &env),
        )
        .await
        .unwrap_or_else(|_| Ok((-1, String::new(), "Execution timed out".to_string())))
    };
    let (exit_code, stdout, stderr) = exec_result.unwrap_or((-1, String::new(), String::new()));
    let timed_out = exit_code == -1 && !command.is_empty() && stderr.contains("timed out");

    let mut collected: HashMap<String, String> = HashMap::new();
    for path in &collect_files {
        if let Ok(content) = incus_pull_file(&remote_name, &container, path).await {
            collected.insert(path.clone(), content);
        }
    }

    let snapshot_saved = if let Some(snap_name) = snapshot_on_exit.as_deref() {
        match incus_snapshot_after_run(&remote_name, &container, snap_name).await {
            Ok(()) => Some(snap_name.to_string()),
            Err(_) => None,
        }
    } else {
        None
    };

    let duration_ms = t_start.elapsed().as_millis() as u64;
    drop(guard); // container deletion (if not already auto-deleted)

    json!({
        "success": true, "action_type": NAME,
        "output": {
            "kind": "incus_sandbox_output",
            "image": image,
            "remote": remote_name,
            "container_name": container,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": timed_out,
            "startup_ms": startup_ms,
            "collected_files": collected,
            "snapshot_saved": snapshot_saved,
            "scope": "incus",
        },
        "duration_ms": duration_ms,
    })
}

// ── RunIncusGroup (parallel containers) ───────────────────────────────────────

pub async fn execute_run_incus_group(action: &Value, config: &Config) -> Value {
    const NAME: &str = "RunIncusGroup";
    let icfg = &config.incus;

    let Some(tasks) = action.get("tasks").and_then(|v| v.as_array()) else {
        return validation_err(
            NAME,
            "missing required field `tasks` (must be an array)".into(),
        );
    };

    // Validate all images upfront — fail-closed on the whole group.
    for task in tasks {
        let image = task.get("image").and_then(|v| v.as_str()).unwrap_or("");
        let id = task.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        if !image_allowed(image, config) {
            return json!({
                "success": false, "action_type": NAME,
                "error": {
                    "kind": "incus_image_denied", "class": "policy_denied",
                    "code": "incus_image_denied",
                    "message": format!("Task '{id}': image '{image}' not in incus.allowed_images"),
                    "action_name": NAME, "retryable": false,
                    "recommended_next_step": "Use only allowed images, or update incus.allowed_images."
                }
            });
        }
    }

    let remote_name = pick_remote(action, config);
    let net_mode = action
        .get("network")
        .and_then(|v| v.as_str())
        .unwrap_or(&icfg.default_network)
        .to_string();
    let mem = action
        .get("memory_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(icfg.default_memory_mb)
        .min(icfg.max_memory_mb);
    let tmo_secs = action
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(icfg.default_timeout_secs)
        .min(icfg.max_timeout_secs);
    let parallelism = action
        .get("max_parallel")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(icfg.max_parallel_instances)
        .min(icfg.max_parallel_instances)
        .max(1);

    let total = tasks.len();
    let t_global = Instant::now();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallelism));
    let mut join_set = tokio::task::JoinSet::new();

    for task in tasks.iter().cloned() {
        let sem = semaphore.clone();
        let remote = remote_name.clone();
        let net = net_mode.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            run_single_task(task, remote, net, mem, tmo_secs).await
        });
    }

    let mut results: Vec<Value> = Vec::with_capacity(total);
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(r) => results.push(r),
            Err(e) => results.push(json!({
                "id": "unknown", "container_name": "",
                "exit_code": -1, "stdout": "",
                "stderr": format!("Task panicked: {}", e),
                "timed_out": false, "success": false, "duration_ms": 0,
            })),
        }
    }
    results.sort_by(|a, b| {
        a.get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
    });

    let succeeded = results
        .iter()
        .filter(|r| r.get("success").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let failed = total - succeeded;
    let duration_ms = t_global.elapsed().as_millis() as u64;

    json!({
        "success": true, "action_type": NAME,
        "output": {
            "kind": "incus_group_output",
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
            "results": results,
            "scope": "incus",
        },
        "duration_ms": duration_ms,
    })
}

async fn run_single_task(
    task: Value,
    remote: String,
    net: String,
    mem: u64,
    timeout_secs: u64,
) -> Value {
    let id = task
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let image = task
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let command: Vec<String> = task
        .get("command")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let files: HashMap<String, String> = task
        .get("files")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let env: HashMap<String, String> = task
        .get("env")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let base_snapshot = task
        .get("base_snapshot")
        .and_then(|v| v.as_str())
        .map(String::from);

    let container = new_container_name();
    let mut guard = ContainerGuard::new(&remote, &container);
    let t0 = Instant::now();

    let launch = if let Some(snap) = base_snapshot.as_deref().filter(|s| !s.is_empty()) {
        timeout(
            Duration::from_secs(30),
            incus_copy_snapshot(&remote, &container, snap, mem, 1),
        )
        .await
    } else {
        timeout(
            Duration::from_secs(120),
            incus_launch(&remote, &container, &image, &net, mem, 1),
        )
        .await
    };
    let launch_ok = matches!(launch, Ok(Ok(())));
    if !launch_ok {
        guard.disarm();
        return json!({
            "id": id, "container_name": container,
            "exit_code": -1, "stdout": "",
            "stderr": "Container failed to start",
            "timed_out": false, "success": false,
            "duration_ms": t0.elapsed().as_millis() as u64,
        });
    }

    for (name, content) in &files {
        if is_safe_filename(name) {
            let _ = incus_push_file(&remote, &container, name, content).await;
        }
    }

    let tmo = Duration::from_secs(timeout_secs);
    let (exit_code, stdout, stderr, timed_out) = if command.is_empty() {
        (0, String::new(), String::new(), false)
    } else {
        match timeout(tmo, incus_exec(&remote, &container, &command, &env)).await {
            Ok(Ok((code, out, err))) => (code, out, err, false),
            Ok(Err(e)) => (-1, String::new(), e.to_string(), false),
            Err(_) => (-1, String::new(), "Timed out".to_string(), true),
        }
    };

    let success = exit_code == 0;
    let duration_ms = t0.elapsed().as_millis() as u64;
    drop(guard);

    json!({
        "id": id, "container_name": container,
        "exit_code": exit_code, "stdout": stdout, "stderr": stderr,
        "timed_out": timed_out, "success": success, "duration_ms": duration_ms,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_remote(r: &str) -> Config {
        let mut c = Config::default();
        c.incus.default_remote = r.into();
        c
    }

    #[test]
    fn pick_remote_uses_action_when_set() {
        let action = json!({"container":"a","name":"s","remote":"prod-infra"});
        assert_eq!(
            pick_remote(&action, &cfg_with_remote("local")),
            "prod-infra"
        );
    }

    #[test]
    fn pick_remote_falls_back_to_config() {
        let action = json!({"container":"a","name":"s"});
        assert_eq!(pick_remote(&action, &cfg_with_remote("local")), "local");
    }

    #[test]
    fn pick_remote_ignores_empty_string() {
        let action = json!({"container":"a","name":"s","remote":""});
        assert_eq!(pick_remote(&action, &cfg_with_remote("local")), "local");
    }

    #[tokio::test]
    async fn create_snapshot_rejects_missing_container() {
        let r = execute_incus_create_snapshot(&json!({"name":"snap1"}), &Config::default()).await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "validation_error");
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("container"));
    }

    #[tokio::test]
    async fn restore_snapshot_rejects_missing_name() {
        let r = execute_incus_restore_snapshot(
            &json!({"container":"x0ne-sandbox"}),
            &Config::default(),
        )
        .await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "validation_error");
    }

    #[tokio::test]
    async fn list_snapshots_rejects_missing_container() {
        let r = execute_incus_list_snapshots(&json!({}), &Config::default()).await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "validation_error");
    }

    #[test]
    fn image_allowlist_fail_closed() {
        // Empty allowlist denies everything by design.
        let cfg = Config::default();
        assert!(!image_allowed("images:ubuntu/24.04", &cfg));
        assert!(!image_allowed("images:alpine/edge", &cfg));
    }

    #[test]
    fn image_allowlist_exact_match() {
        let mut cfg = Config::default();
        cfg.incus.allowed_images = vec!["images:ubuntu/24.04".into()];
        assert!(image_allowed("images:ubuntu/24.04", &cfg));
        assert!(!image_allowed("images:ubuntu/22.04", &cfg));
        assert!(!image_allowed("images:alpine/edge", &cfg));
    }

    #[test]
    fn image_allowlist_wildcard() {
        let mut cfg = Config::default();
        cfg.incus.allowed_images = vec!["images:ubuntu/*".into()];
        assert!(image_allowed("images:ubuntu/24.04", &cfg));
        assert!(image_allowed("images:ubuntu/22.04", &cfg));
        assert!(!image_allowed("images:alpine/edge", &cfg));
    }

    #[test]
    fn safe_filename_rejects_traversal_and_absolute() {
        assert!(is_safe_filename("hello.py"));
        assert!(is_safe_filename("subdir/nested.py"));
        assert!(!is_safe_filename("/etc/passwd"));
        assert!(!is_safe_filename("../escape.py"));
        assert!(!is_safe_filename("ok/../bad"));
        assert!(!is_safe_filename(""));
    }

    /// The property this file used to assert against a private copy of the
    /// rule. It now asserts the shared one still holds it — the copy was
    /// deleted on 2026-08-15 because four copies existed and the two host
    /// spawns (RunCommand, ArtifactServe) were covered by none of them.
    #[test]
    fn safe_env_key_blocks_known_attack_vectors() {
        use x0ne_actions::env_keys::is_safe_caller_env_key as ok;
        assert!(ok("MY_VAR"));
        assert!(ok("FOO123"));
        assert!(!ok("LD_PRELOAD"));
        assert!(!ok("DYLD_INSERT_LIBRARIES"));
        assert!(!ok("PATH"));
        assert!(!ok("HOME"));
        assert!(!ok("with-dash"));
        assert!(!ok(""));
        // Names the deleted copy let through: it tested `is_alphanumeric`,
        // which is Unicode-wide, and knew nothing of the shell or interpreter
        // families.
        assert!(!ok("PS4"));
        assert!(!ok("BASH_ENV"));
        assert!(!ok("PYTHONPATH"));
        assert!(!ok("é"));
    }

    #[tokio::test]
    async fn run_incus_rejects_missing_image() {
        let r = execute_run_incus(&json!({"command":["echo","hi"]}), &Config::default()).await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "validation_error");
    }

    #[tokio::test]
    async fn run_incus_denies_image_outside_allowlist() {
        // Default config has empty allowlist → everything denied.
        let r =
            execute_run_incus(&json!({"image":"images:ubuntu/24.04"}), &Config::default()).await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "policy_denied");
        assert_eq!(r["error"]["code"], "incus_image_denied");
    }

    #[tokio::test]
    async fn run_incus_group_rejects_missing_tasks() {
        let r = execute_run_incus_group(&json!({}), &Config::default()).await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "validation_error");
    }

    #[tokio::test]
    async fn run_incus_group_denies_disallowed_image_for_any_task() {
        // First task has an image NOT in the allowlist → whole group rejected.
        let mut cfg = Config::default();
        cfg.incus.allowed_images = vec!["images:ubuntu/*".into()];
        let r = execute_run_incus_group(
            &json!({"tasks":[
                {"id":"a","image":"images:ubuntu/24.04","command":["true"]},
                {"id":"b","image":"images:alpine/edge","command":["true"]}
            ]}),
            &cfg,
        )
        .await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "policy_denied");
        assert!(r["error"]["message"].as_str().unwrap().contains("alpine"));
    }
}
