use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tracing::{debug, info, warn};

// ── Bridge trait ──────────────────────────────────────────────────────────────

/// Common interface for both stdio and HTTP bridges to x0ne-runtime.
#[async_trait]
pub trait Bridge: Send {
    /// Send an action JSON to x0ne-runtime and receive the ActionResult JSON.
    async fn send_action(&mut self, action: &Value) -> Result<Value>;

    /// Check that x0ne-runtime is alive and responding.
    ///
    /// Called once on startup. Returns `true` if healthy, `Err` if not.
    async fn health_check(&mut self) -> Result<bool>;

    /// Send a CONTROL message — something that configures the runtime rather
    /// than asking it to act. Today: `PolicyGrant` and `SessionMode`.
    ///
    /// SECURITY: the default is a refusal, and that is the whole design. A
    /// control message means "a human was shown a refusal and chose to widen
    /// it", and only the stdio bridge can honestly carry that claim: over
    /// stdio the writer of the runtime's stdin is the process that spawned it,
    /// same user, same session. `HttpBridge` talks to a socket — possibly a
    /// runtime shared by other sessions, certainly one with nobody at the
    /// keyboard — so it inherits this refusal rather than implementing it.
    ///
    /// A bridge added later gets the refusal by default, which is the
    /// fail-closed direction. The runtime refuses non-stdio modes as well
    /// (`PolicyEngine::apply_grant`), so a bridge that overrode this without
    /// thinking still would not widen anything.
    async fn send_control(&mut self, _message: &Value) -> Result<Value> {
        Err(anyhow!(
            "this bridge has no control channel: policy grants are carried over stdio only"
        ))
    }
}

// ── Stdio bridge ──────────────────────────────────────────────────────────────

/// Arguments appended to every LOCAL runtime spawn (`--isolation …`).
/// Set by a frontend from its own command line, never from config or env:
/// a project can ship `x0ne-orchestrator.toml`, and this switches confinement.
static RUNTIME_EXTRA_ARGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Replace the arguments appended to local runtime spawns.
pub fn set_runtime_extra_args(args: Vec<String>) {
    *RUNTIME_EXTRA_ARGS.lock().unwrap_or_else(|e| e.into_inner()) = args;
}

fn with_runtime_extra_args(mut args: Vec<String>) -> Vec<String> {
    args.extend(
        RUNTIME_EXTRA_ARGS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned(),
    );
    args
}

/// Communicates with x0ne-runtime by spawning it as a child process and
/// exchanging newline-delimited JSON over stdin/stdout.
pub struct StdioBridge {
    /// The child process handle — kept alive for the session.
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Saved for auto-respawn when the child process dies.
    spawn_cmd: String,
    spawn_args: Vec<String>,
    /// Working directory of the child, kept so a respawn lands in the same one.
    spawn_cwd: Option<std::path::PathBuf>,
    /// The runtime itself, started here: it inherits no model key
    /// ([`model_key_vars`]). A sandbox command keeps them, to forward the
    /// ones `X0NE_FORWARD_ENV` names.
    keyless: bool,
    /// The child's stderr log, and its length at spawn: the log is appended
    /// across runs, so only what follows is this child's.
    log_path: std::path::PathBuf,
    log_start: u64,
    /// The plan-mode control line the runtime accepted, replayed on a
    /// respawn: a fresh runtime starts in build mode.
    plan_line: Option<Value>,
}

/// The runtime refused to start (no policy, invalid policy): its own message,
/// to be shown as is. Not a crash.
#[derive(Debug)]
pub struct RuntimeRefused(pub String);

impl std::fmt::Display for RuntimeRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuntimeRefused {}

/// The model key variables a runtime started by the session does not
/// inherit: each vendor's of the key table, and the session's own compat
/// key. The runtime executes; the session talks to the models. Review of
/// the pre-release build (S1): `.env` and the shell's keys reached the runtime's `$ENV:`.
pub fn model_key_vars() -> impl Iterator<Item = &'static str> + Clone {
    crate::model_keys::KEYS
        .iter()
        .map(|r| r.var)
        .chain([crate::model_keys::COMPAT_VAR])
}

/// The refusal in a runtime's stderr: from the "refusing to start" line to
/// the end, without the `Error: ` prefix anyhow adds.
pub fn runtime_refusal(stderr: &str) -> Option<String> {
    let at = stderr.find("refusing to start")?;
    let line_start = stderr[..at].rfind('\n').map_or(0, |i| i + 1);
    let text = stderr[line_start..].trim_end();
    Some(text.strip_prefix("Error: ").unwrap_or(text).to_string())
}

impl StdioBridge {
    /// Spawn `x0ne-runtime --mode stdio` and set up the pipes.
    /// x0ne-runtime's stderr is redirected to `~/.x0ne/logs/core.log` (not terminal).
    pub async fn spawn(x0ne_core_bin: &str) -> Result<Self> {
        let args = with_runtime_extra_args(vec!["--mode".into(), "stdio".into()]);
        Self::spawn_runtime(x0ne_core_bin, &args).await
    }

    /// Spawn the runtime binary `bin` with `args`. It inherits no model key
    /// ([`model_key_vars`]).
    pub async fn spawn_runtime(bin: &str, args: &[String]) -> Result<Self> {
        Self::spawn_with(bin, args, None, true).await
    }

    /// [`StdioBridge::spawn_runtime`] with the child started in `cwd`.
    pub async fn spawn_runtime_in(
        bin: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::spawn_with(bin, args, cwd, true).await
    }

    /// Spawn an arbitrary command as the x0ne-runtime bridge.
    /// `cmd` is the executable, `args` are its arguments.
    /// Used for sandbox mode: e.g. `ssh my-server` with args
    /// `["incus", "exec", "x0ne-sandbox", "--", "/opt/x0ne-runtime", "--mode", "stdio"]`
    pub async fn spawn_cmd(cmd: &str, args: &[String]) -> Result<Self> {
        Self::spawn_cmd_in(cmd, args, None).await
    }

    /// [`StdioBridge::spawn_cmd`] with the child started in `cwd`. A world
    /// runs its runtime in its own work dir, so relative paths resolve there.
    pub async fn spawn_cmd_in(
        cmd: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::spawn_with(cmd, args, cwd, false).await
    }

    async fn spawn_with(
        cmd: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        keyless: bool,
    ) -> Result<Self> {
        use std::process::Stdio;

        let log_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".x0ne")
            .join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join("core.log");
        let log_start = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        info!(cmd = %cmd, args = ?args, log = %log_path.display(), "spawning bridge");

        let mut command = tokio::process::Command::new(cmd);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        if keyless {
            for var in model_key_vars() {
                command.env_remove(var);
            }
        }
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log_file))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn bridge: {} {}", cmd, args.join(" ")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Could not open stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Could not open stdout pipe"))?;

        Ok(Self {
            _child: child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            spawn_cmd: cmd.to_string(),
            spawn_args: args.iter().map(|s| s.to_string()).collect(),
            spawn_cwd: cwd.map(|d| d.to_path_buf()),
            keyless,
            log_path,
            log_start,
            plan_line: None,
        })
    }

    /// Attempt to respawn the child process after a broken pipe.
    async fn respawn(&mut self) -> Result<()> {
        warn!(
            "x0ne-runtime pipe broken — respawning bridge ({})",
            self.spawn_cmd
        );
        let fresh = Self::spawn_with(
            &self.spawn_cmd,
            &self.spawn_args,
            self.spawn_cwd.as_deref(),
            self.keyless,
        )
        .await
        .context("failed to respawn x0ne-runtime after broken pipe")?;
        self._child = fresh._child;
        self.stdin = fresh.stdin;
        self.stdout = fresh.stdout;
        self.log_start = fresh.log_start;

        // Give the freshly-spawned process time to initialise its stdio
        // listener before we attempt the health-check ping. Without this
        // delay, read_line() can return an empty string (EOF) if the process
        // hasn't yet opened its stdin-read loop, causing an immediate
        // "respawn failed" error on an otherwise healthy child.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Health-check with 1 retry: covers the case where the 500ms sleep
        // wasn't quite enough on a slow/loaded host.
        let ok = self.health_check_ping().await;
        if ok.is_err() {
            warn!("first post-respawn ping failed, retrying after 1s");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            self.health_check_ping()
                .await
                .context("x0ne-runtime respawn health-check failed after retry")?;
        }

        // SECURITY: fail closed. A fresh runtime is in build mode; the action
        // that triggered the respawn is not sent unless plan mode is back.
        if let Some(plan) = self.plan_line.clone() {
            self.stdin.write_all(plan.to_string().as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await?;
            let mut resp = String::new();
            self.stdout.read_line(&mut resp).await?;
            let ok = serde_json::from_str::<Value>(resp.trim())
                .ok()
                .and_then(|v| v.get("success").and_then(Value::as_bool))
                .unwrap_or(false);
            if !ok {
                return Err(anyhow!(
                    "the respawned x0ne-runtime refused plan mode; nothing was sent: {}",
                    resp.trim()
                ));
            }
        }

        info!("x0ne-runtime respawned successfully");
        Ok(())
    }

    /// Send a ListActions ping and verify we get a non-empty response.
    /// Used by `respawn()` — extracted so we can retry it once.
    async fn health_check_ping(&mut self) -> Result<()> {
        let ping = serde_json::json!({"type": "ListActions"});
        let line = ping.to_string();
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        let mut resp = String::new();
        self.stdout.read_line(&mut resp).await?;
        if resp.trim().is_empty() {
            return Err(anyhow!(
                "respawned x0ne-runtime returned empty response to health-check ping"
            ));
        }
        Ok(())
    }

    /// Build an enriched error message for the "empty response" case.
    /// Reads the last few lines of the runtime's stderr log and tries to
    /// give the user a concrete next step (e.g. `make sync-incus`) when
    /// we can recognise the failure mode.
    /// What this child wrote to the stderr log since it was spawned.
    fn child_log(&self) -> String {
        use std::io::{Read, Seek};
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log_path) {
            if f.seek(std::io::SeekFrom::Start(self.log_start)).is_ok() {
                let mut buf = Vec::new();
                let _ = f.read_to_end(&mut buf);
                s = String::from_utf8_lossy(&buf).into_owned();
            }
        }
        s
    }

    fn diagnose_empty_response(&self) -> String {
        let mut hints: Vec<String> = Vec::new();

        // 1. Read tail of this child's stderr.
        let stderr_tail = {
            let s = self.child_log();
            let mut lines: Vec<&str> = s.lines().rev().take(8).collect();
            lines.reverse();
            lines.join("\n")
        };

        // 2. Pattern-match well-known failure modes.
        let cmd_full = format!("{} {}", self.spawn_cmd, self.spawn_args.join(" "));
        let st = stderr_tail.to_lowercase();

        // GLIBC mismatch is the trickiest case: `incus exec` reports
        // "Command not found" even though the binary IS there, because the
        // dynamic loader refuses to map it. The smoking gun is a
        // `GLIBC_2.x not found` line in stderr. Catch it first because the
        // surface symptom ("Command not found") is shared with the
        // missing-binary case below.
        let has_glibc_mismatch = st.contains("glibc_") && st.contains("not found");

        if has_glibc_mismatch {
            if cmd_full.contains("incus exec") {
                hints.push(
                    "the runtime binary in the Incus container was compiled against a newer GLIBC \
                     than the container provides — rebuild statically with `make sync-incus-musl` \
                     (or `INCUS_REMOTE=local make sync-incus-musl` for a local container)"
                        .into(),
                );
            } else if cmd_full.contains("podman exec") || cmd_full.contains("docker exec") {
                hints.push(
                    "the runtime binary was compiled against a newer GLIBC than the container — \
                     rebuild with `make sync-podman-musl`"
                        .into(),
                );
            } else {
                hints.push(
                    "GLIBC version mismatch — rebuild the runtime statically (musl target)".into(),
                );
            }
        } else if st.contains("command not found") || st.contains("no such file") {
            // "Command not found" via `incus exec` is ambiguous — it can mean
            // either (a) binary genuinely missing, or (b) binary present but
            // the dynamic loader rejected it (typically GLIBC mismatch).
            // We surface BOTH likely fixes in the order users hit them.
            if cmd_full.contains("incus exec") {
                hints.push(
                    "the runtime binary is not loadable inside the Incus container. Two common causes:\n      \
                     1. binary missing — run `INCUS_REMOTE=local make sync-incus` (or drop the prefix if remote)\n      \
                     2. binary present but compiled against a newer GLIBC than the container has — \
                        run `INCUS_REMOTE=local make sync-incus-musl` to rebuild statically\n      \
                     To diagnose, run: `incus exec <container> -- /opt/x0ne-runtime --help` — \
                     a GLIBC error means case 2".into()
                );
            } else if cmd_full.contains("podman exec") || cmd_full.contains("docker exec") {
                hints.push(
                    "the runtime binary is not loadable inside the container. Either it was never \
                     pushed (`make sync-podman`) or it was compiled against a newer GLIBC than the \
                     container provides (`make sync-podman-musl` for a static rebuild)"
                        .into(),
                );
            } else if cmd_full.contains("ssh ") {
                hints.push(
                    "the runtime binary path isn't found on the remote host — verify it's deployed and the path matches".into()
                );
            } else {
                hints.push(format!(
                    "the runtime binary at `{}` was not found in PATH",
                    self.spawn_cmd
                ));
            }
        } else if st.contains("permission denied") {
            hints.push("the runtime binary exists but is not executable — `chmod +x` it".into());
        } else if st.contains("x0ne-runtime") && st.contains("error") {
            hints.push(
                "the runtime crashed at startup — check ~/.x0ne/logs/core.log for the panic".into(),
            );
        }

        // 3. Compose the final message.
        let mut msg = String::from("x0ne-runtime returned an empty response line");
        if !hints.is_empty() {
            msg.push_str("\n\nLikely cause:\n  • ");
            msg.push_str(&hints.join("\n  • "));
        }
        if !stderr_tail.is_empty() {
            msg.push_str("\n\nstderr (last lines from ~/.x0ne/logs/core.log):\n");
            for line in stderr_tail.lines() {
                msg.push_str("  ");
                msg.push_str(line);
                msg.push('\n');
            }
        }
        msg.push_str(&format!("\nspawn command: {cmd_full}"));
        msg
    }
}

#[async_trait]
impl Bridge for StdioBridge {
    async fn send_action(&mut self, action: &Value) -> Result<Value> {
        let line = action.to_string();
        debug!(action = %line, "→ x0ne-runtime");

        // Try to send; if the child is gone or the pipe is broken, respawn
        // once and retry. A dead child is checked first: a process it started
        // may still hold the read end of its stdin, so the write goes through
        // and the only answer would be EOF (measured in CI, 2026-10-31).
        let exited = matches!(self._child.try_wait(), Ok(Some(_)));
        let pipe_ok = !exited
            && self.stdin.write_all(line.as_bytes()).await.is_ok()
            && self.stdin.write_all(b"\n").await.is_ok()
            && self.stdin.flush().await.is_ok();

        if !pipe_ok {
            warn!(
                exited,
                "bridge write failed or the runtime exited, attempting respawn"
            );
            self.respawn()
                .await
                .context("bridge respawn failed after write error")?;
            // Retry the original action on the fresh process
            let line = action.to_string();
            self.stdin
                .write_all(line.as_bytes())
                .await
                .context("Failed to write action after respawn")?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await?;
        }

        let mut response_line = String::new();
        self.stdout
            .read_line(&mut response_line)
            .await
            .context("Failed to read response from x0ne-runtime stdout")?;

        let trimmed = response_line.trim();
        if trimmed.is_empty() {
            // Empty stdout usually means the spawned process exited before
            // emitting its first line. The most common cause when running
            // through `incus exec` / `ssh` / `podman exec` is a stale or
            // missing in-container binary. Surface a hint based on the
            // spawn command and the tail of the stderr log.
            if let Some(r) = runtime_refusal(&self.child_log()) {
                return Err(anyhow::Error::new(RuntimeRefused(r)));
            }
            return Err(anyhow!(self.diagnose_empty_response()));
        }
        debug!(response = %trimmed, "← x0ne-runtime");

        let value: Value = serde_json::from_str(trimmed)
            .with_context(|| format!("x0ne-runtime returned invalid JSON: {}", trimmed))?;
        Ok(value)
    }

    /// The stdio bridge is the one place a control line may travel.
    ///
    /// It reuses `send_action`'s wire discipline — one JSON line out, one JSON
    /// line back — because the runtime's read loop makes no distinction: it
    /// reads a line, notices the `control` tag, and answers with one result
    /// line like any other. There is deliberately no separate framing to keep
    /// in step.
    async fn send_control(&mut self, message: &Value) -> Result<Value> {
        let reply = self.send_action(message).await?;
        if message.get("control").and_then(Value::as_str) == Some("SessionMode")
            && reply.get("success").and_then(Value::as_bool) == Some(true)
        {
            self.plan_line = (message.get("mode").and_then(Value::as_str) == Some("plan"))
                .then(|| message.clone());
        }
        Ok(reply)
    }

    async fn health_check(&mut self) -> Result<bool> {
        // Send a meta action that is always allowed and should respond instantly.
        let ping = serde_json::json!({"type": "ListActions"});
        let result = self
            .send_action(&ping)
            .await
            .context("x0ne-runtime health check failed — no response to ListActions")?;

        if result
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            info!("x0ne-runtime health check OK (stdio)");
            Ok(true)
        } else {
            warn!("x0ne-runtime health check: unexpected response: {}", result);
            Ok(true) // still alive; policy may have blocked but process is up
        }
    }
}

// ── HTTP bridge ───────────────────────────────────────────────────────────────

/// Communicates with a running x0ne-runtime HTTP server.
pub struct HttpBridge {
    client: reqwest::Client,
    base_url: String,
    health_check_timeout_secs: u64,
    /// Bearer for the runtime's `/execute` (mandatory there).
    token: Option<String>,
}

/// The runtime's bearer: `X0NE_CORE_TOKEN`, else the file a loopback runtime
/// generated (`runtime-<port>.token`, see `x0ne_actions::http_guard`).
pub fn resolve_runtime_token(addr: &str) -> Option<String> {
    if let Some(t) = std::env::var("X0NE_CORE_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Some(t);
    }
    let (host, port) = addr.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return None;
    }
    let path = x0ne_actions::http_guard::token_file_path_from_env("runtime", port.parse().ok()?)?;
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

impl HttpBridge {
    /// Connect to an x0ne-runtime HTTP server at `addr` (e.g. `"127.0.0.1:7878"`),
    /// with the token from [`resolve_runtime_token`].
    pub fn new(addr: &str, health_check_timeout_secs: u64) -> Result<Self> {
        Self::with_token(addr, health_check_timeout_secs, resolve_runtime_token(addr))
    }

    /// Same, with an explicit bearer.
    pub fn with_token(
        addr: &str,
        health_check_timeout_secs: u64,
        token: Option<String>,
    ) -> Result<Self> {
        // No OVERALL request timeout, deliberately. The runtime already
        // bounds every action it executes (per-action `timeout_secs`, 300 s
        // for a gcloud create) — a second, shorter bound here (it was 120 s)
        // reclassified a legitimately slow action as `ExecTransportDown`:
        // wrong diagnosis, aborted run (review round 3, I2). The transport's
        // own health is what this client must bound, and that is the CONNECT:
        // a dead or unreachable server fails in seconds at the socket, while
        // a live server answering slowly is the runtime's business, not ours.
        let base_url = format!("http://{}", addr);
        let client = crate::api::client_for(&base_url)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client for bridge")?;

        info!("HTTP bridge → {}", base_url);

        if token.is_none() {
            warn!(
                "no x0ne-runtime token (X0NE_CORE_TOKEN unset, no token file): \
                 /execute will answer 401"
            );
        }
        Ok(Self {
            client,
            base_url,
            health_check_timeout_secs,
            token,
        })
    }
}

#[async_trait]
impl Bridge for HttpBridge {
    async fn send_action(&mut self, action: &Value) -> Result<Value> {
        debug!(action = %action, "→ x0ne-runtime (HTTP)");

        let mut req = self
            .client
            .post(format!("{}/execute", self.base_url))
            .json(action);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let response = req
            .send()
            .await
            .with_context(|| format!("HTTP request to {} failed", self.base_url))?;

        let status = response.status();
        let body: Value = response
            .json()
            .await
            .context("x0ne-runtime HTTP response was not valid JSON")?;
        debug!(status = %status, response = %body, "← x0ne-runtime (HTTP)");

        Ok(body)
    }

    async fn health_check(&mut self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.health_check_timeout_secs),
            self.client.get(&url).send(),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "x0ne-runtime health check timed out after {}s at {} — is it running?",
                self.health_check_timeout_secs,
                url
            )
        })?
        .with_context(|| format!("Cannot reach x0ne-runtime at {} — is it running?", url))?;

        if result.status().is_success() {
            info!("x0ne-runtime health check OK (HTTP) → {}", url);
            Ok(true)
        } else {
            Err(anyhow!(
                "x0ne-runtime returned HTTP {} on /health — check the server.",
                result.status()
            ))
        }
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

/// [`create_bridge`] from a [`crate::config::BridgeConfig`], honouring
/// `runtime_config`: in stdio mode the runtime is started with
/// `--config <path>` (relative to the working directory — the directory a
/// project-local `x0ne-orchestrator.toml` was read from), so a project can
/// carry its own runtime policy. The frontends use this; the other call sites
/// (sub-agents, worlds, matrix, `x0ne cc`) keep the positional form and the
/// runtime's own resolution.
/// A `./x0ne-runtime.toml` the runtime will not read (no `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1`),
/// said on the terminal: the runtime's own warning lands in `~/.x0ne/logs/core.log`.
/// Measured 2026-10-10: the file was written per docs/mcp.md and silently ignored.
pub fn cwd_runtime_policy_notice(
    cfg: &crate::config::BridgeConfig,
    cwd: &std::path::Path,
    opted_in: bool,
) -> Option<String> {
    if cfg.mode == "http" || cfg.mode == "sandbox" || cfg.runtime_config.is_some() || opted_in {
        return None;
    }
    let file = cwd.join("x0ne-runtime.toml");
    if !file.is_file() {
        return None;
    }
    Some(format!(
        "{} is present but not read: the runtime takes its policy from \
         ~/.config/x0ne/x0ne-runtime.toml. To use this file, set \
         X0NE_RUNTIME_ALLOW_CWD_CONFIG=1 (you vouch for it), or name it as \
         `[bridge] runtime_config` in a trusted project's x0ne-orchestrator.toml.",
        file.display()
    ))
}

pub async fn create_bridge_for(cfg: &crate::config::BridgeConfig) -> Result<Box<dyn Bridge>> {
    // Every session runtime opens here; refused before any spawn or socket.
    crate::sandbox_mode::require_sandbox(&cfg.mode, cfg.sandbox_cmd.as_deref())?;
    if cfg.mode != "http" && cfg.mode != "sandbox" {
        if let Some(policy) = cfg.runtime_config.as_deref() {
            let abs = std::fs::canonicalize(policy)
                .with_context(|| format!("bridge.runtime_config: {policy} does not exist"))?;
            let args = with_runtime_extra_args(vec![
                "--mode".into(),
                "stdio".into(),
                "--config".into(),
                abs.to_string_lossy().to_string(),
            ]);
            info!(policy = %abs.display(), "runtime policy from bridge.runtime_config");
            let bridge = StdioBridge::spawn_runtime(&cfg.x0ne_core_bin, &args).await?;
            return Ok(Box::new(bridge));
        }
    }
    create_bridge(
        &cfg.mode,
        &cfg.x0ne_core_bin,
        &cfg.x0ne_core_addr,
        cfg.health_check_timeout_secs,
        cfg.sandbox_cmd.as_deref(),
    )
    .await
}

/// Create the appropriate bridge based on config.
pub async fn create_bridge(
    mode: &str,
    x0ne_core_bin: &str,
    x0ne_core_addr: &str,
    health_check_timeout_secs: u64,
    sandbox_cmd: Option<&str>,
) -> Result<Box<dyn Bridge>> {
    create_bridge_in(
        mode,
        x0ne_core_bin,
        x0ne_core_addr,
        health_check_timeout_secs,
        sandbox_cmd,
        None,
    )
    .await
}

/// [`create_bridge`] with a spawned runtime started in `cwd`. Refused in
/// http mode: a remote runtime cannot be moved into a local directory.
pub async fn create_bridge_in(
    mode: &str,
    x0ne_core_bin: &str,
    x0ne_core_addr: &str,
    health_check_timeout_secs: u64,
    sandbox_cmd: Option<&str>,
    cwd: Option<&std::path::Path>,
) -> Result<Box<dyn Bridge>> {
    if mode == "http" && cwd.is_some() {
        return Err(anyhow!(
            "a world needs its own work dir, and bridge.mode = \"http\" runs the runtime elsewhere"
        ));
    }
    match mode {
        "http" => {
            let bridge = HttpBridge::new(x0ne_core_addr, health_check_timeout_secs)?;
            Ok(Box::new(bridge))
        }
        "sandbox" => {
            let cmd_str = sandbox_cmd.ok_or_else(|| {
                anyhow!(
                "bridge.mode = \"sandbox\" requires bridge.sandbox_cmd to be set. \
                 Example: 'ssh my-server incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode stdio'"
            )
            })?;
            // Split the command string: first token = binary, rest = args
            let mut parts = cmd_str.split_whitespace();
            let exe = parts
                .next()
                .ok_or_else(|| anyhow!("sandbox_cmd is empty"))?;
            let args: Vec<String> = parts.map(String::from).collect();
            info!(cmd = %cmd_str, "sandbox bridge via custom command");
            let bridge = StdioBridge::spawn_cmd_in(exe, &args, cwd).await?;
            Ok(Box::new(bridge))
        }
        _ => {
            // Default: stdio with local x0ne-runtime binary
            let args = with_runtime_extra_args(vec!["--mode".into(), "stdio".into()]);
            let bridge = StdioBridge::spawn_runtime_in(x0ne_core_bin, &args, cwd).await?;
            Ok(Box::new(bridge))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn a_refusal_is_cut_from_its_line_to_the_end() {
        let log = "INFO starting\nError: x0ne-runtime: refusing to start: no policy.\n\nWrite it at:\n  /x\n";
        assert_eq!(
            super::runtime_refusal(log).as_deref(),
            Some("x0ne-runtime: refusing to start: no policy.\n\nWrite it at:\n  /x")
        );
        assert_eq!(super::runtime_refusal("thread 'main' panicked"), None);
    }

    /// The runtime refuses `/execute` without a bearer; the bridge
    /// must present the one it was given.
    #[tokio::test]
    async fn the_http_bridge_sends_the_runtime_token() {
        use super::Bridge;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                if n == 0 || String::from_utf8_lossy(&got).contains("\r\n\r\n{") {
                    break;
                }
            }
            let body = "{\"success\":true}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&got).to_lowercase()
        });
        let mut bridge =
            super::HttpBridge::with_token(&addr, 2, Some("tok-bridge".into())).unwrap();
        let out = bridge
            .send_action(&serde_json::json!({"type": "ListActions"}))
            .await
            .unwrap();
        assert_eq!(out["success"], true);
        let request = server.await.unwrap();
        assert!(
            request.contains("authorization: bearer tok-bridge"),
            "no bearer sent: {request}"
        );
    }

    use super::*;

    /// The frontend's `--isolation` reaches the local runtime's argv.
    /// Uses the runtime's default value, so a parallel spawn that sees it
    /// behaves exactly as without it.
    #[test]
    fn the_isolation_flag_reaches_the_local_runtime_argv() {
        set_runtime_extra_args(vec!["--isolation".into(), "bwrap".into()]);
        let args = with_runtime_extra_args(vec!["--mode".into(), "stdio".into()]);
        set_runtime_extra_args(Vec::new());
        assert_eq!(args, ["--mode", "stdio", "--isolation", "bwrap"]);
    }

    #[test]
    fn http_bridge_constructs_without_panic() {
        // Just verify construction does not error
        let bridge = HttpBridge::new("127.0.0.1:7878", 5);
        assert!(bridge.is_ok());
    }

    #[test]
    fn http_bridge_base_url_formed_correctly() {
        let bridge = HttpBridge::new("10.0.0.1:9090", 5).unwrap();
        assert_eq!(bridge.base_url, "http://10.0.0.1:9090");
    }

    #[tokio::test]
    async fn http_bridge_health_check_fails_gracefully_when_not_running() {
        // Port 19999 should not be listening — health check should return Err
        let mut bridge = HttpBridge::new("127.0.0.1:19999", 2).unwrap();
        let result = bridge.health_check().await;
        assert!(
            result.is_err(),
            "Expected health check to fail for unreachable server"
        );
        let msg = result.unwrap_err().to_string();
        // Should mention timeout or connection failure
        assert!(
            msg.contains("timed out") || msg.contains("Cannot reach") || msg.contains("connect"),
            "Unexpected error: {}",
            msg
        );
    }
}

#[cfg(test)]
mod cwd_runtime_policy_notice_tests {
    use super::cwd_runtime_policy_notice;
    use crate::config::BridgeConfig;

    fn dir_with_file() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x0ne-runtime.toml"), "[policy]\n").unwrap();
        dir
    }

    #[test]
    fn a_present_cwd_policy_the_runtime_will_ignore_is_announced() {
        let dir = dir_with_file();
        let cfg = BridgeConfig {
            mode: "stdio".into(),
            ..Default::default()
        };
        let notice = cwd_runtime_policy_notice(&cfg, dir.path(), false)
            .expect("the operator must be told the file is not read");
        assert!(
            notice.contains("X0NE_RUNTIME_ALLOW_CWD_CONFIG=1"),
            "{notice}"
        );
        assert!(notice.contains("[bridge] runtime_config"), "{notice}");
        assert!(notice.contains("x0ne-runtime.toml"), "{notice}");
    }

    #[test]
    fn nothing_is_said_when_the_file_is_read_or_irrelevant() {
        let dir = dir_with_file();
        let stdio = BridgeConfig {
            mode: "stdio".into(),
            ..Default::default()
        };
        assert!(cwd_runtime_policy_notice(&stdio, dir.path(), true).is_none());
        let named = BridgeConfig {
            mode: "stdio".into(),
            runtime_config: Some("x0ne-runtime.toml".into()),
            ..Default::default()
        };
        assert!(cwd_runtime_policy_notice(&named, dir.path(), false).is_none());
        for remote in ["http", "sandbox"] {
            let cfg = BridgeConfig {
                mode: remote.into(),
                ..Default::default()
            };
            assert!(cwd_runtime_policy_notice(&cfg, dir.path(), false).is_none());
        }
        let empty = tempfile::tempdir().unwrap();
        assert!(cwd_runtime_policy_notice(&stdio, empty.path(), false).is_none());
    }
}

/// Plan mode across a respawn of the stdio runtime.
#[cfg(test)]
mod plan_mode_respawn_tests {
    use super::*;

    /// A stand-in runtime: logs each line it reads, answers success, and
    /// refuses `SessionMode` once the file `fail` exists.
    async fn fake_runtime(dir: &std::path::Path) -> StdioBridge {
        let script = r#"while IFS= read -r l; do
  printf '%s\n' "$l" >> "$1"
  case "$l" in
    *SessionMode*) if [ -e "$2" ]; then echo '{"success":false}'; else echo '{"success":true}'; fi ;;
    *) echo '{"success":true}' ;;
  esac
done"#;
        let log = dir.join("lines").display().to_string();
        let fail = dir.join("fail").display().to_string();
        StdioBridge::spawn_cmd(
            "/bin/sh",
            &["-c".into(), script.into(), "sh".into(), log, fail],
        )
        .await
        .unwrap()
    }

    /// The same stand-in, whose stdin stays open after it dies: a program
    /// it started (`sleep`) holds the read end, as a runtime's own children do.
    async fn fake_runtime_with_holder(dir: &std::path::Path) -> StdioBridge {
        let script = r#"exec 3<&0
sleep 3 <&3 &
while IFS= read -r l; do
  printf '%s\n' "$l" >> "$1"
  echo '{"success":true}'
done"#;
        let log = dir.join("lines").display().to_string();
        StdioBridge::spawn_cmd("/bin/sh", &["-c".into(), script.into(), "sh".into(), log])
            .await
            .unwrap()
    }

    fn lines(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("lines"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    async fn kill(bridge: &mut StdioBridge) {
        bridge._child.start_kill().unwrap();
        let _ = bridge._child.wait().await;
    }

    /// SECURITY: a runtime respawned after a broken pipe is put back in plan
    /// mode before the action is sent. A failure means a crash of the runtime
    /// ends plan mode silently: the fresh process starts in build mode.
    #[tokio::test]
    async fn a_respawned_runtime_is_put_back_in_plan_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = fake_runtime(dir.path()).await;
        let plan = serde_json::json!({"control": "SessionMode", "mode": "plan"});
        let r = bridge.send_control(&plan).await.unwrap();
        assert_eq!(r["success"], true);
        kill(&mut bridge).await;

        let write = serde_json::json!({"type": "WriteFile", "path": "/x", "content": "y"});
        bridge
            .send_action(&write)
            .await
            .unwrap_or_else(|e| panic!("{e:#}"));
        let seen = lines(dir.path());
        let at = seen
            .iter()
            .rposition(|l| l.contains("WriteFile"))
            .expect("the action reached the fresh runtime");
        let replayed = seen
            .iter()
            .skip(1)
            .position(|l| l.contains("SessionMode") && l.contains("plan"))
            .map(|i| i + 1);
        assert!(
            matches!(replayed, Some(i) if i < at),
            "plan mode was not replayed before the action: {seen:?}"
        );
    }

    /// SECURITY: if the fresh runtime refuses plan mode, the action is not
    /// sent at all.
    #[tokio::test]
    async fn no_action_reaches_a_respawned_runtime_that_refused_plan_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = fake_runtime(dir.path()).await;
        let plan = serde_json::json!({"control": "SessionMode", "mode": "plan"});
        bridge.send_control(&plan).await.unwrap();
        std::fs::write(dir.path().join("fail"), "").unwrap();
        kill(&mut bridge).await;

        let write = serde_json::json!({"type": "WriteFile", "path": "/x", "content": "y"});
        assert!(bridge.send_action(&write).await.is_err());
        let seen = lines(dir.path());
        assert!(
            !seen.iter().any(|l| l.contains("WriteFile")),
            "the action was sent to a runtime outside plan mode: {seen:?}"
        );
    }

    /// A dead runtime is respawned even when the write to its stdin goes
    /// through: a process it started still holds the read end of the pipe,
    /// so the write does not fail, and the only answer would be EOF.
    #[tokio::test]
    async fn a_dead_runtime_is_respawned_when_its_pipe_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = fake_runtime_with_holder(dir.path()).await;
        let ping = serde_json::json!({"type": "ListActions"});
        assert_eq!(bridge.send_action(&ping).await.unwrap()["success"], true);
        kill(&mut bridge).await;

        let write = serde_json::json!({"type": "WriteFile", "path": "/x", "content": "y"});
        let r = bridge
            .send_action(&write)
            .await
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(r["success"], true);
        let seen = lines(dir.path());
        assert!(
            seen.iter().any(|l| l.contains("WriteFile")),
            "the action did not reach a fresh runtime: {seen:?}"
        );
    }

    /// `/build` ends the replay: a respawn after it starts in build mode.
    #[tokio::test]
    async fn build_mode_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut bridge = fake_runtime(dir.path()).await;
        for mode in ["plan", "build"] {
            let m = serde_json::json!({"control": "SessionMode", "mode": mode});
            bridge.send_control(&m).await.unwrap();
        }
        kill(&mut bridge).await;
        let write = serde_json::json!({"type": "WriteFile", "path": "/x", "content": "y"});
        bridge
            .send_action(&write)
            .await
            .unwrap_or_else(|e| panic!("{e:#}"));
        let seen = lines(dir.path());
        assert_eq!(
            seen.iter().filter(|l| l.contains("SessionMode")).count(),
            2,
            "{seen:?}"
        );
    }
}
