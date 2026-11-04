//! `ContainerRuntime` — thin abstraction over the host-side container CLI
//! used by the artifact subsystem (and nothing else in the orchestrator).
//!
//! Why this exists
//! ───────────────
//! `ArtifactExpose` / `ArtifactUnexpose` must talk to the tenant container to
//! pull artifact files over the already-authenticated CLI channel. Until now
//! the implementation hard-coded `incus`, which meant users running the
//! x0ne-runtime sandbox on Podman or Docker could not publish artifacts.
//!
//! This module introduces a tiny trait (`ContainerRuntime`) with three
//! concrete backends (`IncusRuntime`, `PodmanRuntime`, `DockerRuntime`),
//! plus a helper (`detect_from_sandbox_cmd`) that inspects the configured
//! `[bridge] sandbox_cmd` and picks the right one. The rest of the artifact
//! code paths operate on the trait and never spawn the CLI by name again.
//!
//! Design notes
//! ────────────
//! - We use `exec <target> -- cat <path>` (or the backend-specific
//!   equivalent) to read files. This is slightly less strict than
//!   `incus file pull` but works identically on all three backends and
//!   has no corner cases around stream multiplexing (which `docker cp -`
//!   has because it emits a tar archive, not raw bytes).
//! - Backends are cheap to instantiate and stateless; we hand out `Arc`s
//!   to let `ArtifactEntry` keep a reference without generics.
//! - Targets are opaque strings understood by the backend:
//!     * Incus  → `"remote:container"` (e.g. `"prod-infra:x0ne-sandbox"`)
//!     * Podman → `"container-name-or-id"`
//!     * Docker → `"container-name-or-id"`

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Which container runtime we're talking to. Serialisable so it can be
/// carried inside `ArtifactEntry` (which is cloned into HTTP handlers) and
/// logged/returned in JSON action results.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Incus,
    Podman,
    Docker,
}

impl Backend {
    /// Binary name resolved via `$PATH`.
    pub const fn bin(self) -> &'static str {
        match self {
            Backend::Incus => "incus",
            Backend::Podman => "podman",
            Backend::Docker => "docker",
        }
    }

    /// Best-effort parse of a backend from its binary name.
    pub fn parse(s: &str) -> Option<Backend> {
        match s {
            "incus" => Some(Backend::Incus),
            "podman" => Some(Backend::Podman),
            "docker" => Some(Backend::Docker),
            _ => None,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.bin())
    }
}

/// Normalised exec output — the three backends differ wildly in their
/// error wording but all expose an exit code + stdout + stderr.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Result of pulling a file out of a container. The second tuple slot is
/// a human-readable error reason kept alive for logging even in the Ok
/// case (some backends print warnings on stderr while still exiting 0).
pub type PullResult = (Result<Vec<u8>>, Option<String>);

/// Contract implemented by every backend. Kept intentionally small — the
/// artifact subsystem only needs `exec` (to consult the container's artifact
/// registry) and `pull_file` (to stream file bytes to the browser).
#[async_trait]
pub trait ContainerRuntime: Send + Sync + fmt::Debug {
    fn backend(&self) -> Backend;

    /// Run a command inside the container and wait for completion.
    async fn exec(&self, target: &str, argv: &[&str]) -> Result<ExecOutput>;

    /// Read an absolute file path from the container and return its raw
    /// bytes. `target` is the backend-specific container identifier
    /// (Incus `remote:container`, Podman/Docker just `container`).
    async fn pull_file(&self, target: &str, abs_path: &str) -> PullResult;
}

// ── Concrete backends ────────────────────────────────────────────────────────

/// Shared spawn-and-collect helper. Backends differ only in the
/// positioning of the `--` separator, so the body is reused.
async fn run_capture(bin: &'static str, args: &[&str]) -> Result<ExecOutput> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("spawn `{} {}`", bin, args.join(" ")))?;
    Ok(ExecOutput {
        rc: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Streamed file read. We spawn the backend with piped stdout/stderr so
/// large artifacts don't materialise twice in memory via `.output()`.
async fn run_pull(bin: &'static str, args: &[&str]) -> PullResult {
    let spawn = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => return (Err(anyhow!("spawn {}: {}", bin, e)), Some(format!("{}", e))),
    };

    let mut buf = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        if let Err(e) = stdout.read_to_end(&mut buf).await {
            return (Err(anyhow!("read stdout: {}", e)), Some(format!("{}", e)));
        }
    }
    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => return (Err(anyhow!("wait {}: {}", bin, e)), Some(format!("{}", e))),
    };
    if !status.success() {
        let mut err_buf = String::new();
        if let Some(mut se) = child.stderr.take() {
            let _ = se.read_to_string(&mut err_buf).await;
        }
        return (
            Err(anyhow!("{} pull failed: {}", bin, err_buf.trim())),
            Some(err_buf.trim().to_string()),
        );
    }
    (Ok(buf), None)
}

/// Incus backend. Uses `incus exec <target> -- <argv...>` and
/// `incus file pull <target><path> -` (the native streamed-file command).
#[derive(Debug, Default, Clone, Copy)]
pub struct IncusRuntime;

#[async_trait]
impl ContainerRuntime for IncusRuntime {
    fn backend(&self) -> Backend {
        Backend::Incus
    }

    async fn exec(&self, target: &str, argv: &[&str]) -> Result<ExecOutput> {
        let mut args: Vec<&str> = vec!["exec", target, "--"];
        args.extend_from_slice(argv);
        run_capture("incus", &args).await
    }

    async fn pull_file(&self, target: &str, abs_path: &str) -> PullResult {
        // `incus file pull remote:container/abs/path -`. Note the lack of
        // colon between target and path — Incus uses direct concatenation.
        let arg = format!("{}{}", target, abs_path);
        run_pull("incus", &["file", "pull", &arg, "-"]).await
    }
}

/// Podman backend. Uses `podman exec <target> <argv...>` (no `--` needed).
///
/// For file reads we intentionally avoid `podman cp container:/path -`,
/// which emits a *tar archive* on stdout rather than raw bytes. Using
/// `exec … cat` keeps the transport symmetric across all three backends
/// and gives us a real exit code when the file is missing.
#[derive(Debug, Default, Clone, Copy)]
pub struct PodmanRuntime;

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    fn backend(&self) -> Backend {
        Backend::Podman
    }

    async fn exec(&self, target: &str, argv: &[&str]) -> Result<ExecOutput> {
        let mut args: Vec<&str> = vec!["exec", target];
        args.extend_from_slice(argv);
        run_capture("podman", &args).await
    }

    async fn pull_file(&self, target: &str, abs_path: &str) -> PullResult {
        // `podman exec <c> sh -c 'cat -- <path> || exit 2'`. The `sh -c`
        // wrapper lets us quote the path safely and keeps behaviour
        // identical to the Incus `cat` fallback below.
        let inner = format!("cat -- {}", shell_single_quote(abs_path));
        run_pull("podman", &["exec", target, "sh", "-c", &inner]).await
    }
}

/// Docker backend. Same shape as Podman (the CLI is a drop-in replacement
/// for most operations), just pointed at a different binary.
#[derive(Debug, Default, Clone, Copy)]
pub struct DockerRuntime;

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    fn backend(&self) -> Backend {
        Backend::Docker
    }

    async fn exec(&self, target: &str, argv: &[&str]) -> Result<ExecOutput> {
        let mut args: Vec<&str> = vec!["exec", target];
        args.extend_from_slice(argv);
        run_capture("docker", &args).await
    }

    async fn pull_file(&self, target: &str, abs_path: &str) -> PullResult {
        let inner = format!("cat -- {}", shell_single_quote(abs_path));
        run_pull("docker", &["exec", target, "sh", "-c", &inner]).await
    }
}

/// Return an `Arc<dyn ContainerRuntime>` for the requested backend.
pub fn for_backend(b: Backend) -> Arc<dyn ContainerRuntime> {
    match b {
        Backend::Incus => Arc::new(IncusRuntime),
        Backend::Podman => Arc::new(PodmanRuntime),
        Backend::Docker => Arc::new(DockerRuntime),
    }
}

/// Best-effort IPv4 of the named container on its default bridge network.
///
/// Used by the orchestrator's artifact router to learn where to forward
/// `/a/<id>/...` requests when the runtime lives in a sandbox: instead of
/// `127.0.0.1:7900` we want `<container-ip>:7900`. Falls back to an `Err`
/// if the backend CLI is unavailable or the container has no v4 IP yet
/// (still in `BUILDING`, network bridge offline, …) — caller should log
/// and use loopback default.
pub async fn container_ipv4(target: &str, backend: Backend) -> Result<String> {
    match backend {
        Backend::Incus => {
            // `incus list <name> -f csv -c 4` → "10.174.92.16 (eth0)\n10.174.92.16 (eth1)" etc.
            let out = Command::new("incus")
                .args(["list", target, "-f", "csv", "-c", "4"])
                .stdin(Stdio::null())
                .output()
                .await
                .with_context(|| "spawn `incus list`")?;
            if !out.status.success() {
                return Err(anyhow!(
                    "incus list {} failed: {}",
                    target,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let ip = stdout
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .find(|tok| !tok.is_empty() && tok.contains('.'))
                .ok_or_else(|| anyhow!("no IPv4 in `incus list {}` output", target))?;
            Ok(ip.to_string())
        }
        Backend::Podman | Backend::Docker => {
            let bin = if matches!(backend, Backend::Podman) {
                "podman"
            } else {
                "docker"
            };
            // Podman/Docker default-bridge IP. Newer Podman with rootless
            // networking via netavark may report empty here — caller falls
            // back to loopback in that case.
            let out = Command::new(bin)
                .args(["inspect", target, "-f", "{{.NetworkSettings.IPAddress}}"])
                .stdin(Stdio::null())
                .output()
                .await
                .with_context(|| format!("spawn `{} inspect`", bin))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "{} inspect {} failed: {}",
                    bin,
                    target,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let ip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ip.is_empty() {
                return Err(anyhow!(
                    "{} inspect {} returned empty IPAddress (rootless/netavark?)",
                    bin,
                    target
                ));
            }
            Ok(ip)
        }
    }
}

// ── Detection from sandbox_cmd ───────────────────────────────────────────────

/// Inspect a `[bridge] sandbox_cmd` string and return `(runtime, target)`
/// when the first bare token matches a supported backend.
///
/// Accepted shapes (examples, all valid):
/// ```text
/// incus  exec --env A=1 prod-infra:x0ne-sandbox -- /opt/x0ne-runtime --mode stdio
/// podman exec -it mycontainer /opt/x0ne-runtime --mode stdio
/// docker exec --user root  worker-42 /opt/x0ne-runtime --mode stdio
/// ```
///
/// Returns `None` if the command does not start with `incus|podman|docker`,
/// or if we cannot find a positional token that looks like the container
/// target before the inner command. In both cases the caller should error
/// out with a clear "ArtifactExpose requires sandbox mode" message.
pub fn detect_from_sandbox_cmd(cmd: &str) -> Option<(Arc<dyn ContainerRuntime>, String)> {
    // Skip any leading `KEY=VAL` environment assignments the caller may
    // have prefixed (e.g. `X0NE_BRIDGE_MODE=sandbox incus exec ...`). They
    // show up in env files and shouldn't confuse the parser.
    let mut tokens = cmd.split_whitespace().peekable();
    while let Some(&tok) = tokens.peek() {
        let env_assignment = tok.contains('=') && !tok.starts_with("--") && !tok.starts_with('-');
        if env_assignment {
            tokens.next();
        } else {
            break;
        }
    }

    let first = tokens.next()?;
    let backend = Backend::parse(first)?;

    // Require the "exec" verb next — that's what all three backends spell
    // the same and it's what we'll actually use at runtime.
    if tokens.next()? != "exec" {
        return None;
    }

    // Skip flags. Flags with a value can be written either `--flag value`
    // or `--flag=value`. We model the former by tracking a pending
    // `skip_next_value` state driven by the set of flags we know take
    // arguments (`--env`, `-e`, `--user`, `--workdir`, `--volume`, `-v`).
    // The existing Incus parser used a narrower set; we broaden it a bit
    // to be tolerant of realistic podman/docker invocations.
    let flags_with_value: &[&str] = &[
        "--env",
        "-e",
        "--user",
        "-u",
        "--workdir",
        "-w",
        "--volume",
        "-v",
    ];
    let mut skip_next_value = false;

    for t in tokens {
        if skip_next_value {
            skip_next_value = false;
            continue;
        }
        if t == "--" {
            // End of options without finding a positional — give up.
            return None;
        }
        if flags_with_value.contains(&t) {
            skip_next_value = true;
            continue;
        }
        if t.starts_with("--") || t.starts_with('-') {
            // Standalone boolean flag (--rm, -it, --privileged, …).
            continue;
        }
        // First bare token = the container target.
        return Some((for_backend(backend), t.to_string()));
    }
    None
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Quote a string as a POSIX single-quoted literal. Handles the embedded-
/// single-quote case via the `'\''` trick. Used to safely inject absolute
/// file paths inside `sh -c` wrappers without opening shell-injection
/// holes via the artifact registry (attacker-supplied root_path values).
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Backend enum ────────────────────────────────────────────────────

    #[test]
    fn backend_bin_matches_expected_strings() {
        assert_eq!(Backend::Incus.bin(), "incus");
        assert_eq!(Backend::Podman.bin(), "podman");
        assert_eq!(Backend::Docker.bin(), "docker");
    }

    #[test]
    fn backend_parse_roundtrip() {
        for b in [Backend::Incus, Backend::Podman, Backend::Docker] {
            assert_eq!(Backend::parse(b.bin()), Some(b));
        }
        assert!(Backend::parse("kubectl").is_none());
        assert!(Backend::parse("").is_none());
    }

    #[test]
    fn backend_json_roundtrip() {
        // Serialisation discipline: JSON uses lower-case so action results
        // ship `"backend": "podman"` instead of `"Podman"`. Tests pin that.
        let s = serde_json::to_string(&Backend::Podman).unwrap();
        assert_eq!(s, "\"podman\"");
        let b: Backend = serde_json::from_str("\"docker\"").unwrap();
        assert_eq!(b, Backend::Docker);
    }

    // ── detect_from_sandbox_cmd ─────────────────────────────────────────

    #[test]
    fn detect_plain_incus() {
        let (rt, target) = detect_from_sandbox_cmd(
            "incus exec prod-infra:x0ne-sandbox -- /opt/x0ne-runtime --mode stdio",
        )
        .expect("incus detected");
        assert_eq!(rt.backend(), Backend::Incus);
        assert_eq!(target, "prod-infra:x0ne-sandbox");
    }

    #[test]
    fn detect_incus_with_env_flags() {
        let (rt, target) = detect_from_sandbox_cmd(
            "incus exec --env X0NE_ALLOW_LOOPBACK=1 --env FOO=bar prod-infra:x0ne-sandbox \
             -- /opt/x0ne-runtime --mode stdio",
        )
        .expect("env-prefixed incus detected");
        assert_eq!(rt.backend(), Backend::Incus);
        assert_eq!(target, "prod-infra:x0ne-sandbox");
    }

    #[test]
    fn detect_plain_podman() {
        let (rt, target) =
            detect_from_sandbox_cmd("podman exec mycontainer /opt/x0ne-runtime --mode stdio")
                .expect("podman detected");
        assert_eq!(rt.backend(), Backend::Podman);
        assert_eq!(target, "mycontainer");
    }

    #[test]
    fn detect_podman_with_boolean_and_value_flags() {
        let (rt, target) = detect_from_sandbox_cmd(
            "podman exec -it --user root --workdir /opt x0ne-sandbox \
             /opt/x0ne-runtime --mode stdio",
        )
        .expect("podman with flags detected");
        assert_eq!(rt.backend(), Backend::Podman);
        assert_eq!(target, "x0ne-sandbox");
    }

    /// Regression: `make run-podman` passes `-i` alone (not `-it`) to keep
    /// stdin attached in non-TTY environments like stdio-mode. The parser
    /// must treat it as a standalone boolean flag rather than consume the
    /// next token as its value — otherwise the container target would be
    /// eaten and detection would fall through to `None`, silently
    /// disabling artifact exposure.
    #[test]
    fn detect_podman_with_standalone_i_flag() {
        let (rt, target) = detect_from_sandbox_cmd(
            "podman exec -i --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman \
             /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio",
        )
        .expect("podman -i detected");
        assert_eq!(rt.backend(), Backend::Podman);
        assert_eq!(target, "x0ne-sandbox-podman");
    }

    #[test]
    fn detect_podman_with_equals_env() {
        // `--env=KEY=VAL` form must NOT consume the next token (it's
        // already complete). Regression guard for the common mistake.
        let (rt, target) = detect_from_sandbox_cmd(
            "podman exec --env=X0NE_ALLOW_LOOPBACK=1 mycontainer /opt/x0ne-runtime",
        )
        .expect("podman with --env= detected");
        assert_eq!(rt.backend(), Backend::Podman);
        assert_eq!(target, "mycontainer");
    }

    #[test]
    fn detect_plain_docker() {
        let (rt, target) =
            detect_from_sandbox_cmd("docker exec x0ne-sandbox /opt/x0ne-runtime --mode stdio")
                .expect("docker detected");
        assert_eq!(rt.backend(), Backend::Docker);
        assert_eq!(target, "x0ne-sandbox");
    }

    #[test]
    fn detect_docker_with_leading_env_assignment() {
        // Mimic `env X0NE_BRIDGE_MODE=sandbox docker exec …` patterns
        // users sometimes paste into their shell rc files.
        let (rt, target) = detect_from_sandbox_cmd(
            "X0NE_BRIDGE_MODE=sandbox FOO=1 docker exec worker-42 /opt/x0ne-runtime",
        )
        .expect("leading env stripped");
        assert_eq!(rt.backend(), Backend::Docker);
        assert_eq!(target, "worker-42");
    }

    #[test]
    fn detect_rejects_non_container_cli() {
        assert!(detect_from_sandbox_cmd("ssh host -- /opt/x0ne-runtime --mode stdio").is_none());
        assert!(detect_from_sandbox_cmd("").is_none());
        assert!(detect_from_sandbox_cmd("incus").is_none()); // no "exec"
    }

    #[test]
    fn detect_rejects_non_exec_subcommand() {
        // `incus shell` is a real subcommand but not one we support for
        // artifact exposure (no target-then-cmd shape).
        assert!(detect_from_sandbox_cmd("incus shell prod-infra:x0ne-sandbox").is_none());
    }

    #[test]
    fn detect_gives_up_when_only_flags_follow_exec() {
        // Edge case: user typo'd the command and forgot the target. We
        // must return None rather than silently claim the first flag
        // value is the target.
        assert!(detect_from_sandbox_cmd("podman exec --rm -it --").is_none());
    }

    // ── shell_single_quote ─────────────────────────────────────────────

    #[test]
    fn single_quote_wraps_plain_path() {
        assert_eq!(shell_single_quote("/root/foo"), "'/root/foo'");
    }

    #[test]
    fn single_quote_escapes_embedded_quote() {
        // The path `O'Brien/file` must become `'O'\''Brien/file'`.
        assert_eq!(shell_single_quote("O'Brien/file"), "'O'\\''Brien/file'");
    }

    #[test]
    fn single_quote_handles_empty_string() {
        assert_eq!(shell_single_quote(""), "''");
    }

    // ── Backend dispatch sanity (argv shape) ──────────────────────────

    // We can't actually execute incus/podman/docker in unit tests (CI
    // doesn't have them installed), but we can at least verify that
    // `for_backend` yields a trait object of the expected backend.
    #[test]
    fn for_backend_returns_matching_backend() {
        for b in [Backend::Incus, Backend::Podman, Backend::Docker] {
            let rt = for_backend(b);
            assert_eq!(rt.backend(), b, "mismatch for {:?}", b);
        }
    }
}

// ── Containment: the one answer both the badge and the gate read ─────────────

/// Whether this session's actions land in a container, and in which one.
///
/// There is exactly one producer of this answer, and both the header badge and
/// (later) the approval authority read it. That is deliberate: a badge saying
/// "podman" while the gate believed something else would be worse than no badge
/// at all, because the operator would be trusting a claim nobody enforced.
///
/// # Why this is derived and not configured
///
/// Security research published in 2026 found a coding agent bypassing its own
/// denylist by path tricks and then, when the sandbox caught it, **disabling
/// the sandbox** and re-running the command. The lesson generalises: a
/// containment signal must come from something the agent cannot reach. This
/// one is read from the operator's own config file at startup, which the agent
/// has no action to rewrite (`x0ne-runtime.toml` is outside every write root
/// unless the operator puts it inside one — and if they do, that is their
/// decision, made explicitly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Containment {
    /// Actions run directly on the operator's machine. No blast-radius bound
    /// beyond the policy's own path and command rules.
    Host,
    /// Actions run inside a container, named by the backend that provides it.
    Contained { backend: Backend, target: String },
    /// The bridge claims a sandbox, but the command does not name a backend we
    /// can recognise. Reported separately from `Host` because "we do not know"
    /// and "we know it is the host" call for different answers — and because
    /// treating an unparseable claim as containment is how a sandbox becomes
    /// decorative.
    ClaimedButUnverified,
}

impl Containment {
    /// Read the containment from the bridge configuration.
    ///
    /// `mode` is the operator's claim; `sandbox_cmd` is the command that will
    /// actually be executed, and is what gets parsed. A mode of `sandbox` with
    /// no recognisable command is `ClaimedButUnverified`, never `Contained`.
    pub fn detect(mode: &str, sandbox_cmd: Option<&str>) -> Self {
        if mode != "sandbox" {
            return Self::Host;
        }
        match sandbox_cmd.and_then(detect_from_sandbox_cmd_backend) {
            Some((backend, target)) => Self::Contained { backend, target },
            None => Self::ClaimedButUnverified,
        }
    }

    /// True only when actions provably land inside a container.
    ///
    /// The single question the approval authority will ask. `ClaimedButUnverified`
    /// answers `false`, because a claim is not a container.
    pub fn is_contained(&self) -> bool {
        matches!(self, Self::Contained { .. })
    }

    /// Glyph, label and colour for the header strip.
    pub fn badge(&self) -> (&'static str, String, (u8, u8, u8)) {
        match self {
            Self::Contained { backend, .. } => ("●", backend.to_string(), (0, 200, 120)),
            Self::ClaimedButUnverified => ("◐", "unverified".to_string(), (215, 165, 60)),
            Self::Host => ("○", "host".to_string(), (150, 120, 90)),
        }
    }
}

/// Backend + target from a sandbox command, without building a runtime.
///
/// [`detect_from_sandbox_cmd`] returns a live `ContainerRuntime`, which is more
/// than a label needs and drags in construction that can fail. This shares the
/// same parsing rules and returns only what the caller asked for.
fn detect_from_sandbox_cmd_backend(cmd: &str) -> Option<(Backend, String)> {
    detect_from_sandbox_cmd(cmd).map(|(rt, target)| (rt.backend(), target))
}

#[cfg(test)]
mod containment_tests {
    use super::*;

    #[test]
    fn a_non_sandbox_mode_is_the_host() {
        assert_eq!(Containment::detect("stdio", None), Containment::Host);
        assert_eq!(
            Containment::detect("http", Some("podman exec box /bin/rt")),
            Containment::Host,
            "the command is irrelevant when the bridge is not sandboxed"
        );
    }

    #[test]
    fn a_recognised_backend_is_containment() {
        let c = Containment::detect(
            "sandbox",
            Some("podman exec x0ne-sandbox /opt/x0ne-runtime"),
        );
        assert!(c.is_contained());
        let (_, label, rgb) = c.badge();
        assert_eq!(label, "podman");
        assert!(rgb.1 > rgb.0, "containment must render green, got {rgb:?}");
    }

    /// The property the whole design rests on: a sandbox that cannot be
    /// verified is not a sandbox. Treating an unparseable claim as containment
    /// is exactly how a security control becomes decorative — this repository
    /// has four such flags already.
    #[test]
    fn a_claim_without_a_recognisable_backend_is_not_containment() {
        for cmd in [
            None,
            Some(""),
            Some("some-wrapper --magic"),
            Some("podman"),           // no `exec` verb
            Some("ssh host /bin/rt"), // not a container backend
        ] {
            let c = Containment::detect("sandbox", cmd);
            assert!(
                !c.is_contained(),
                "{cmd:?} must not count as containment, got {c:?}"
            );
            assert_eq!(c, Containment::ClaimedButUnverified);
        }
    }
}
