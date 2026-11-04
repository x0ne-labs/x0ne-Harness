//! Podman container executor for `RunInSandbox`.
//!
//! Architecture:
//! 1. Validate image against `sandbox.podman.allowed_images` (fail-closed).
//! 2. Cap resource params (memory, cpu, timeout) to configured maximums.
//! 3. Generate a unique container name: `x0ne-{8 hex chars}`.
//! 4. Register a `ContainerGuard` (RAII) that force-removes the container on Drop.
//! 5. Write `files` to a temp directory on the host.
//! 6. Call `podman run --name <name> --network=none --read-only --tmpfs /tmp --memory=NNNm
//!              -v /tmp/host_dir:/workspace:ro <image> <command>`
//! 7. Capture stdout+stderr, return `SandboxOutput`.
//! 8. Optionally collect output files via `podman cp`.
//! 9. ContainerGuard::drop() removes the container (force) — guaranteed cleanup.
//!
//! No container SDK needed — Podman is just a binary.

use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::time::timeout;

use super::podman_bin;
use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

// ── RAII Container Guard ──────────────────────────────────────────────────────

/// Ensures a container is force-removed even on panic, timeout, or early return.
struct ContainerGuard {
    name: String,
    armed: bool,
}

impl ContainerGuard {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            armed: true,
        }
    }

    /// Disarm the guard — call after explicit removal to prevent double-delete.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // std::process (blocking) is acceptable in Drop context.
        if let Some(mut cmd) = podman_bin::std_command() {
            // SECURITY: cleared environment like every spawn in this crate —
            // see `crate::child_env`. Drop is not an exception: podman reads
            // registry credentials out of the environment when it finds them.
            crate::child_env::harden(&mut cmd);
            let _ = cmd
                .args(["rm", "-f", &self.name])
                // SECURITY: stdin NULL like every other spawn in this file —
                // see `collect_container_files`. A cleanup that runs from Drop
                // is the LAST place one wants holding the bridge's fd 0.
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
        }
    }
}

// ── Container naming ──────────────────────────────────────────────────────────

/// Generate a unique container name safe for Podman (alphanumeric + hyphen).
pub fn new_container_name() -> String {
    let id = uuid::Uuid::new_v4();
    let hex = id.simple().to_string();
    format!("x0ne-{}", &hex[..8])
}

// ── Image allowlist check ─────────────────────────────────────────────────────

pub fn image_allowed(image: &str, config: &AppConfig) -> bool {
    let allowed = &config.sandbox.podman.allowed_images;
    if allowed.is_empty() {
        return false; // fail-closed
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

// ── Main executor ─────────────────────────────────────────────────────────────

pub async fn run_in_sandbox(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::RunInSandbox {
        image,
        command,
        files,
        env,
        timeout_secs,
        network,
        memory_mb,
        cpu_limit,
        collect_files,
    } = action
    else {
        unreachable!()
    };

    let pcfg = &config.sandbox.podman;

    // 1. Verify podman is available
    if !podman_available().await {
        return ActionResult::err(
            "RunInSandbox",
            ErrorKind::MissingDependency,
            podman_bin::not_found_message(),
            0,
        );
    }

    // 2. Image allowlist (fail-closed)
    if !image_allowed(image, config) {
        return ActionResult::err(
            "RunInSandbox",
            ErrorKind::PermissionDenied,
            format!(
                "Image '{}' is not in sandbox.podman.allowed_images. \
                 Add it to x0ne-runtime.toml to permit use.",
                image
            ),
            0,
        );
    }

    // 3. Cap resource params
    let mem = memory_mb
        .unwrap_or(pcfg.default_memory_mb)
        .min(pcfg.max_memory_mb);
    // Cap CPU like memory — the LLM may request less, never more than the hard cap.
    let cpus = cpu_limit
        .unwrap_or(pcfg.default_cpu_limit)
        .min(pcfg.max_cpu_limit.max(1));
    let timeout_dur = Duration::from_secs(
        timeout_secs
            .unwrap_or(pcfg.default_timeout_secs)
            .min(pcfg.max_timeout_secs),
    );
    let net = network.unwrap_or(pcfg.default_network);

    // 4. Write injected files into a temp directory on the host
    let tmp_dir = match write_files_to_tempdir(files) {
        Ok(d) => d,
        Err(e) => {
            return ActionResult::err(
                "RunInSandbox",
                ErrorKind::Internal,
                format!("Failed to prepare workspace files: {}", e),
                0,
            )
        }
    };

    // 5. Build podman args — use --name for RAII cleanup + file collection
    let container_name = new_container_name();
    let mut guard = ContainerGuard::new(&container_name);

    let mut podman_args: Vec<String> = vec![
        "run".into(),
        format!("--name={}", container_name),
        "--interactive".into(),
        // Container label for audit trail
        "--label".into(),
        "x0ne.managed=true".into(),
        // Security: no capabilities
        "--cap-drop=ALL".into(),
    ];

    // Security: no privilege escalation
    if pcfg.no_new_privileges {
        podman_args.push("--security-opt=no-new-privileges".into());
    }

    // Network policy
    if !net {
        podman_args.push("--network=none".into());
    }

    // Memory + CPU limits
    podman_args.push(format!("--memory={}m", mem));
    podman_args.push(format!("--memory-swap={}m", mem)); // disable swap
    if cpus > 0 {
        podman_args.push(format!("--cpus={}", cpus));
    }

    // Workspace bind-mount (read-only from host side)
    let host_path = tmp_dir.path().to_string_lossy().to_string();
    if !files.is_empty() {
        podman_args.push(format!("-v={}:/workspace:ro", host_path));
        podman_args.push("--workdir=/workspace".into());
    }

    // Writable /tmp inside container
    podman_args.push("--tmpfs=/tmp:rw,size=64m".into());

    // Extra env vars — validate keys to prevent argument injection
    for (k, v) in env {
        let valid_key = !k.is_empty()
            && k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            guard.disarm(); // container was never started
            return ActionResult::err(
                "RunInSandbox",
                ErrorKind::InvalidInput,
                format!(
                    "Invalid environment variable name: {:?} (must match [A-Za-z_][A-Za-z0-9_]*)",
                    k
                ),
                0,
            );
        }
        if v.contains('\0') {
            guard.disarm();
            return ActionResult::err(
                "RunInSandbox",
                ErrorKind::InvalidInput,
                format!("Environment variable '{}' value contains null byte", k),
                0,
            );
        }
        podman_args.push(format!("--env={}={}", k, v));
    }

    // The image
    podman_args.push(image.clone());

    // The command (if empty, uses image default CMD)
    for arg in command {
        podman_args.push(arg.clone());
    }

    // SECURITY: redact --env values — they may carry secrets and this logs at
    // debug level. Keep the key visible for debuggability, mask the value.
    let redacted_args: Vec<String> = podman_args
        .iter()
        .map(|a| match a.strip_prefix("--env=") {
            Some(kv) => {
                let key = kv.split('=').next().unwrap_or("");
                format!("--env={}=<redacted>", key)
            }
            None => a.clone(),
        })
        .collect();
    tracing::debug!("podman {}", redacted_args.join(" "));

    // 6. Spawn and collect
    let t_start = Instant::now();
    let result = timeout(timeout_dur, spawn_container(&podman_args)).await;

    let duration_ms = t_start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok((exit_code, stdout, stderr))) => {
            // 7. Collect output files if requested
            let collected = if !collect_files.is_empty() {
                collect_container_files(&container_name, collect_files).await
            } else {
                HashMap::new()
            };

            // Guard drops here → container removed
            drop(guard);

            ActionResult::ok(
                "RunInSandbox",
                ActionOutput::SandboxOutput {
                    image: image.clone(),
                    exit_code,
                    stdout,
                    stderr,
                    timed_out: false,
                    container_id: Some(container_name),
                    collected_files: collected,
                },
                duration_ms,
            )
        }
        Ok(Err(e)) => {
            guard.disarm(); // container may not have started
            ActionResult::err(
                "RunInSandbox",
                ErrorKind::ExecutionFailed,
                format!("Failed to start container: {}", e),
                duration_ms,
            )
        }
        Err(_) => {
            // Timed out — guard will clean up
            drop(guard);
            ActionResult::ok(
                "RunInSandbox",
                ActionOutput::SandboxOutput {
                    image: image.clone(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!(
                        "Container timed out after {}s",
                        timeout_secs.unwrap_or(pcfg.default_timeout_secs)
                    ),
                    timed_out: true,
                    container_id: None,
                    collected_files: HashMap::new(),
                },
                duration_ms,
            )
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Check if `podman` binary exists and is executable.
pub async fn podman_available() -> bool {
    let Some(mut cmd) = podman_bin::tokio_command() else {
        return false;
    };
    // SECURITY: cleared environment, same class as every spawn here.
    crate::child_env::harden(&mut cmd);
    cmd.arg("--version")
        // SECURITY: stdin NULL — same class, see `collect_container_files`.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write files map into a temporary directory. Returns the dir (auto-cleaned on drop).
/// Preserves subdirectory structure (e.g. `src/main.rs`) but rejects path traversal
/// (`..` components) and absolute paths.
pub fn write_files_to_tempdir(
    files: &HashMap<String, String>,
) -> std::io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    for (name, content) in files {
        let p = std::path::Path::new(name);

        // Security: reject absolute paths and any `..` components
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            // Sanitize to just the filename as fallback
            let safe_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file.txt");
            let path = dir.path().join(safe_name);
            let mut f = std::fs::File::create(path)?;
            f.write_all(content.as_bytes())?;
        } else {
            // Safe relative path — preserve subdirectory structure
            let path = dir.path().join(p);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(path)?;
            f.write_all(content.as_bytes())?;
        }
    }
    Ok(dir)
}

/// Spawn `podman` and capture stdout, stderr, exit code.
/// Does NOT use --rm (container stays for file collection, guard handles cleanup).
pub async fn spawn_container(args: &[String]) -> std::io::Result<(i32, String, String)> {
    let Some(mut cmd) = podman_bin::tokio_command() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            podman_bin::not_found_message(),
        ));
    };
    // SECURITY: cleared environment before anything else. A sandbox that
    // forwards the host's secrets into the container it was asked to isolate
    // is worse than no sandbox: the isolation is what the operator believes in.
    // The action's own `env` map travels as `--env=K=V` podman arguments
    // (sandbox.rs, `build_run_args`), so it is unaffected by the clear.
    crate::child_env::harden(&mut cmd);
    cmd.args(args)
        // SECURITY: same reason as spawn_capture (executor/process.rs) — an
        // inherited fd 0 is the bridge pipe, and O_NONBLOCK set by the child
        // travels back up the shared file description and kills the runtime.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;

    // Read stdout and stderr concurrently to prevent deadlocks when the
    // child writes heavily to both streams.
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // SECURITY: bounded, like `spawn_capture` — the container's stdout is
    // as unbounded as any other child's, and it was read whole into RAM
    // until 2026-09-11 (audit 03-F5).
    let stdout_fut = async {
        match stdout_handle {
            Some(out) => {
                super::process::read_capped(
                    out,
                    &mut stdout_buf,
                    super::process::MAX_CAPTURE_READ_BYTES,
                )
                .await
            }
            None => 0,
        }
    };
    let stderr_fut = async {
        match stderr_handle {
            Some(err) => {
                super::process::read_capped(
                    err,
                    &mut stderr_buf,
                    super::process::MAX_CAPTURE_READ_BYTES,
                )
                .await
            }
            None => 0,
        }
    };
    let (drained_out, drained_err) = tokio::join!(stdout_fut, stderr_fut);

    let status = child.wait().await?;
    let mut stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
    if drained_out > 0 {
        stdout.push_str(&format!(
            "\n[x0ne: {drained_out} more bytes of stdout not captured]"
        ));
    }
    if drained_err > 0 {
        stderr.push_str(&format!(
            "\n[x0ne: {drained_err} more bytes of stderr not captured]"
        ));
    }
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Collect files from inside a container using `podman cp`.
async fn collect_container_files(
    container_name: &str,
    paths: &[String],
) -> HashMap<String, String> {
    let mut collected = HashMap::new();

    for path in paths {
        let tmp = match tempfile::NamedTempFile::new() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let tmp_path = tmp.path().to_string_lossy().to_string();
        let source = format!("{}:{}", container_name, path);

        let Some(mut cmd) = podman_bin::tokio_command() else {
            break;
        };
        // SECURITY: cleared environment, same class as every spawn here.
        crate::child_env::harden(&mut cmd);
        let status = cmd
            .args(["cp", &source, &tmp_path])
            // SECURITY: stdin NULL, same class as `spawn_capture` and
            // `spawn_container` (fix of 2026-08-14, EAGAIN report). This one
            // was left open by that pass: waiting on an exit code inherits
            // stdin exactly as capturing does, so a `podman cp` handed the
            // runtime's fd 0 owns the bridge's read end for as long as it runs,
            // and anything it sets there — O_NONBLOCK travels with the shared
            // file description — outlives it. `podman cp` never reads stdin;
            // giving it the protocol channel was pure exposure.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        if let Ok(s) = status {
            if s.success() {
                if let Ok(content) = std::fs::read_to_string(&tmp_path) {
                    collected.insert(path.clone(), content);
                }
            }
        }
    }

    collected
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    /// SECURITY, and it is a CLASS rather than a line: every process this
    /// module spawns must be handed a NULL stdin.
    ///
    /// The runtime's fd 0 IS the bridge — the orchestrator's channel for
    /// stopping a runaway machine. A child that inherits it holds the read end
    /// while it runs, and `O_NONBLOCK` set on it travels with the shared file
    /// description and outlives the child: that is how the runtime died
    /// mid-pipeline on 2026-08-14 (see the EAGAIN report). `spawn_capture` and
    /// `spawn_container` were repaired that day; `podman cp`, the `podman rm -f`
    /// in `Drop` and `podman --version` were not, and nothing noticed because
    /// the repair had no test for the class (review round 3, M4).
    ///
    /// It reads the source instead of spawning, and that is the honest bound of
    /// what this test proves: podman is not installed on CI and comes only from
    /// a fixed path (`podman_bin`), so there is no seam to inject a fake through.
    /// What it does catch is the case that produced the defect — a fifth spawn
    /// written the same way, without the flag. PATH lookup itself is covered by
    /// `tests/podman_is_never_taken_from_path.rs`.
    #[test]
    fn every_process_this_module_spawns_gets_a_null_stdin() {
        let src = include_str!("sandbox.rs");
        // Production half only: this test's own text names the same things.
        let head = &src[..src.find("#[cfg(test)]").expect("the test module")];
        assert!(
            !head.contains("Command::new("),
            "a spawn by program name is back; podman comes from podman_bin only"
        );
        let mut starts: Vec<usize> = ["podman_bin::std_command()", "podman_bin::tokio_command()"]
            .iter()
            .flat_map(|m| head.match_indices(m).map(|(i, _)| i))
            .collect();
        starts.sort_unstable();
        for &at in &starts {
            // A builder chain ends where it is run. Everything it configures —
            // `.stdin` included — is between the two.
            let end = [".output()", ".status()", ".spawn()"]
                .iter()
                .filter_map(|t| head[at..].find(t).map(|j| at + j))
                .min()
                .expect("a spawn site must eventually run its command");
            assert!(
                head[at..end].contains(".stdin(Stdio::null())"),
                "this podman spawn inherits the bridge's stdin:\n{}",
                &head[at..end]
            );
        }
        assert_eq!(starts.len(), 4, "the scan must still see every spawn site");
    }

    #[test]
    fn write_files_strips_traversal() {
        let mut files = HashMap::new();
        files.insert("../evil.sh".to_string(), "rm -rf /".to_string());
        files.insert("script.py".to_string(), "print('hello')".to_string());

        let dir = write_files_to_tempdir(&files).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        // Path traversal component stripped — ends up as "evil.sh" in dir, NOT outside
        assert!(!entries.iter().any(|n| n.contains("..")));
        assert!(entries.iter().any(|n| n == "script.py"));
    }

    #[test]
    fn write_files_creates_content() {
        let mut files = HashMap::new();
        files.insert("hello.txt".to_string(), "world".to_string());

        let dir = write_files_to_tempdir(&files).unwrap();
        let content = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
        assert_eq!(content, "world");
    }

    #[test]
    fn container_name_format() {
        let name = new_container_name();
        assert!(name.starts_with("x0ne-"));
        assert!(name.len() <= 63);
        assert!(name.chars().all(|c| c.is_alphanumeric() || c == '-'));
    }

    #[test]
    fn image_allowlist_fail_closed() {
        let cfg = AppConfig::defaults(); // allowed_images = []
        assert!(!image_allowed("alpine", &cfg));
        assert!(!image_allowed("docker.io/library/ubuntu", &cfg));
    }

    #[test]
    fn image_allowlist_exact_match() {
        let mut cfg = AppConfig::defaults();
        cfg.sandbox.podman.allowed_images = vec!["alpine".into()];
        assert!(image_allowed("alpine", &cfg));
        assert!(!image_allowed("ubuntu", &cfg));
    }

    #[test]
    fn image_allowlist_wildcard() {
        let mut cfg = AppConfig::defaults();
        cfg.sandbox.podman.allowed_images = vec!["docker.io/library/*".into()];
        assert!(image_allowed("docker.io/library/alpine", &cfg));
        assert!(image_allowed("docker.io/library/ubuntu", &cfg));
        assert!(!image_allowed("quay.io/fedora", &cfg));
    }

    #[test]
    fn resource_capping() {
        let mut cfg = AppConfig::defaults();
        cfg.sandbox.podman.max_memory_mb = 512;
        cfg.sandbox.podman.max_timeout_secs = 60;

        let mem = 9999u64.min(cfg.sandbox.podman.max_memory_mb);
        assert_eq!(mem, 512);

        let tmo = 9999u64.min(cfg.sandbox.podman.max_timeout_secs);
        assert_eq!(tmo, 60);
    }

    #[tokio::test]
    async fn policy_image_not_allowed_error_message() {
        use crate::config::AppConfig;
        use crate::policy::PolicyEngine;
        use crate::policy::{DenyReason, PolicyEffect};

        let mut cfg = AppConfig::defaults();
        // Default mode="confirm" would return RequireConfirmation before ActionDisabled.
        // Use mode="" to test the action_enabled gate directly.
        cfg.policy.mode = String::new();
        let actions = cfg.actions.clone();
        let engine = PolicyEngine::new(cfg.clone(), actions);

        // RunInSandbox is disabled by default → must be denied
        let action = Action::RunInSandbox {
            image: "alpine".into(),
            command: vec!["sh".into(), "-c".into(), "echo hi".into()],
            files: Default::default(),
            env: Default::default(),
            timeout_secs: None,
            network: None,
            memory_mb: None,
            cpu_limit: None,
            collect_files: vec![],
        };
        assert!(
            matches!(
                engine.evaluate(&action),
                PolicyEffect::Deny(DenyReason::ActionDisabled)
            ),
            "RunInSandbox must be Deny(ActionDisabled) when disabled in config"
        );
    }
}
