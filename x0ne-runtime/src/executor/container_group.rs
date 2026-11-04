//! Parallel Podman container executor for `RunContainerGroup`.
//!
//! Same semaphore-limited pattern as the Incus group executor:
//! - Validates all images against the allowlist upfront.
//! - Caps resource params to configured maximums.
//! - Spawns containers in parallel (up to `max_parallel`).
//! - Each task gets its own ephemeral container with RAII cleanup.

use std::time::{Duration, Instant};

use tokio::time::timeout;

use crate::action::{Action, ContainerTask};
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ContainerTaskResult, ErrorKind};

use super::sandbox::{
    image_allowed, new_container_name, podman_available, spawn_container, write_files_to_tempdir,
};

pub async fn run_container_group(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::RunContainerGroup {
        tasks,
        network,
        memory_mb,
        timeout_secs,
        max_parallel,
    } = action
    else {
        unreachable!()
    };

    let pcfg = &config.sandbox.podman;

    // 1. Verify podman
    if !podman_available().await {
        return ActionResult::err(
            "RunContainerGroup",
            ErrorKind::MissingDependency,
            super::podman_bin::not_found_message(),
            0,
        );
    }

    // 2. Validate all images upfront
    for task in tasks {
        if !image_allowed(&task.image, config) {
            return ActionResult::err(
                "RunContainerGroup",
                ErrorKind::PermissionDenied,
                format!(
                    "Task '{}': image '{}' not in sandbox.podman.allowed_images",
                    task.id, task.image
                ),
                0,
            );
        }
    }

    // 3. Cap resources
    let net = network.unwrap_or(pcfg.default_network);
    let mem = memory_mb
        .unwrap_or(pcfg.default_memory_mb)
        .min(pcfg.max_memory_mb);
    let tmo_secs = timeout_secs
        .unwrap_or(pcfg.default_timeout_secs)
        .min(pcfg.max_timeout_secs);
    let parallelism = max_parallel
        .unwrap_or(pcfg.max_parallel_instances)
        .min(pcfg.max_parallel_instances)
        .max(1);

    let total = tasks.len();
    let t_global = Instant::now();

    // 4. Semaphore-limited parallel execution
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallelism));
    let mut join_set = tokio::task::JoinSet::new();

    for task in tasks.clone() {
        let sem = semaphore.clone();
        let net = net;
        let mem = mem;
        let tmo = tmo_secs;
        let no_new_priv = pcfg.no_new_privileges;

        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            run_single_task(task, net, mem, tmo, no_new_priv).await
        });
    }

    let mut results: Vec<ContainerTaskResult> = Vec::with_capacity(total);
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(r) => results.push(r),
            Err(e) => results.push(ContainerTaskResult {
                id: "unknown".to_string(),
                container_name: String::new(),
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("Task panicked: {}", e),
                timed_out: false,
                success: false,
                duration_ms: 0,
            }),
        }
    }

    // Sort by original task order
    results.sort_by(|a, b| a.id.cmp(&b.id));

    let succeeded = results.iter().filter(|r| r.success).count();
    let failed = total - succeeded;
    let duration_ms = t_global.elapsed().as_millis() as u64;

    ActionResult::ok(
        "RunContainerGroup",
        ActionOutput::ContainerGroupOutput {
            total,
            succeeded,
            failed,
            results,
        },
        duration_ms,
    )
}

/// Execute a single ContainerTask inside its own ephemeral Podman container.
async fn run_single_task(
    task: ContainerTask,
    net: bool,
    mem: u64,
    tmo_secs: u64,
    no_new_priv: bool,
) -> ContainerTaskResult {
    let container_name = new_container_name();
    let t0 = Instant::now();

    // Prepare workspace files
    let tmp_dir = match write_files_to_tempdir(&task.files) {
        Ok(d) => d,
        Err(e) => {
            return ContainerTaskResult {
                id: task.id,
                container_name,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("Failed to prepare files: {}", e),
                timed_out: false,
                success: false,
                duration_ms: t0.elapsed().as_millis() as u64,
            };
        }
    };

    // Build podman args
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        format!("--name={}", container_name),
        "--interactive".into(),
        "--label".into(),
        "x0ne.managed=true".into(),
        "--cap-drop=ALL".into(),
    ];

    if no_new_priv {
        args.push("--security-opt=no-new-privileges".into());
    }
    if !net {
        args.push("--network=none".into());
    }
    args.push(format!("--memory={}m", mem));
    args.push(format!("--memory-swap={}m", mem));

    let host_path = tmp_dir.path().to_string_lossy().to_string();
    if !task.files.is_empty() {
        args.push(format!("-v={}:/workspace:ro", host_path));
        args.push("--workdir=/workspace".into());
    }

    args.push("--tmpfs=/tmp:rw,size=64m".into());

    // Env vars (skip invalid keys silently in group mode)
    for (k, v) in &task.env {
        let valid = !k.is_empty()
            && k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !v.contains('\0');
        if valid {
            args.push(format!("--env={}={}", k, v));
        }
    }

    args.push(task.image.clone());
    args.extend(task.command.iter().cloned());

    // Execute with timeout
    let tmo_dur = Duration::from_secs(tmo_secs);
    let (exit_code, stdout, stderr, timed_out) =
        match timeout(tmo_dur, spawn_container(&args)).await {
            Ok(Ok((code, out, err))) => (code, out, err, false),
            Ok(Err(e)) => (-1, String::new(), e.to_string(), false),
            Err(_) => (-1, String::new(), "Timed out".to_string(), true),
        };

    let success = exit_code == 0;
    let duration_ms = t0.elapsed().as_millis() as u64;

    ContainerTaskResult {
        id: task.id,
        container_name,
        exit_code,
        stdout,
        stderr,
        timed_out,
        success,
        duration_ms,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_task_result_shape() {
        let r = ContainerTaskResult {
            id: "task-1".into(),
            container_name: "x0ne-abcdef01".into(),
            exit_code: 0,
            stdout: "hello".into(),
            stderr: String::new(),
            timed_out: false,
            success: true,
            duration_ms: 42,
        };
        assert!(r.success);
        assert_eq!(r.exit_code, 0);
        assert!(r.container_name.starts_with("x0ne-"));
    }
}
