//! Boot-time container autowrap — OPT-IN (`[bridge] mode = "auto"`,
//! `--sandbox` or `X0NE_AUTO_SANDBOX=1`).
//!
//! Plain `x0ne` does NOT run in a container: `decide_inner` step 2 returns
//! `RespectExplicit { "host (stdio default)" }` for the default `stdio` mode
//! (pinned by the tests below). The host runtime confines each
//! agent command with bubblewrap, or refuses it (`executor::isolation`). When
//! opted in, the runtime boots inside an Incus (preferred) or Podman container
//! provisioned by `x0ne setup`, launched with `--isolation container`.
//!
//! ## Flow
//!
//! 1. If the user explicitly asked for a non-default bridge (CLI `--mode`,
//!    `X0NE_BRIDGE_MODE` env, or a `[bridge] mode != "stdio"` in their config)
//!    we respect that decision and return `Decision::RespectExplicit`.
//! 2. If `X0NE_NO_SANDBOX=1`, we return `Decision::UnsandboxedExplicit` —
//!    caller prints a loud warning and continues on the host.
//! 3. Otherwise we probe the host:
//!      - Incus: `incus list x0ne-sandbox -f csv -c n` returns the name?
//!      - Podman: `podman container exists x0ne-sandbox-podman`?
//!    The first one that matches wins (Incus is cheaper to snapshot).
//!    If the container is stopped we start it here — auto-healing.
//! 4. If nothing is available, we return `Decision::MissingRuntime` with
//!    a list of what the user can install. The caller is responsible for
//!    the fatal-error message.
//!
//! The module is *pure detection + config mutation* — no I/O for spinners
//! or prompts. That keeps it testable (pure functions for the decision
//! logic, shell-out confined to `runtime_available` / `container_ready`).

use std::process::Command;

use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::sandbox_mode::{refusal, uncontained_reason, SandboxMode};

/// How we want to connect to x0ne-runtime for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// User asked for a specific bridge setup (CLI flag, env var, or a
    /// non-default `[bridge]` section in config). Honour it as-is.
    RespectExplicit { reason: &'static str },

    /// `X0NE_NO_SANDBOX=1` — host execution requested. Caller MUST print a
    /// red warning so the user knows they're outside the sandbox.
    UnsandboxedExplicit,

    /// Autowrapped — we injected a sandbox `sandbox_cmd` into the config.
    /// The `runtime` and `target` are informational (used for the boot
    /// banner and the CLI's `bridge_tag`).
    Autowrapped { runtime: Runtime, target: String },

    /// No sandbox runtime is installed/usable. The caller should fail
    /// closed unless the user sets `X0NE_NO_SANDBOX=1`.
    MissingRuntime { probed: Vec<Runtime> },

    /// A runtime is installed but the baseline container doesn't exist
    /// yet. The caller should offer `x0ne setup <runtime>` or prompt to
    /// bootstrap it now.
    NeedsBootstrap { runtime: Runtime, target: String },
}

/// Which container runtime we picked for the sandbox. Stringified in the
/// boot banner and the CLI status bar, and mirrored one-for-one with
/// `container_runtime::Backend` (we keep a separate enum here so the
/// CLI crate doesn't need to depend on container_runtime's serde shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Incus,
    Podman,
}

impl Runtime {
    pub fn bin(self) -> &'static str {
        match self {
            Runtime::Incus => "incus",
            Runtime::Podman => "podman",
        }
    }

    /// The runtime's CLI. Podman only from a fixed path
    /// (`x0ne_runtime::executor::podman_bin`), `None` when absent there.
    pub fn command(self) -> Option<Command> {
        match self {
            Runtime::Incus => Some(Command::new("incus")),
            Runtime::Podman => x0ne_runtime::executor::podman_bin::std_command(),
        }
    }

    /// `<cli> --version` exits 0.
    pub fn answers(self) -> bool {
        let Some(mut cmd) = self.command() else {
            return false;
        };
        cmd.arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Runtime::Incus => "incus",
            Runtime::Podman => "podman",
        }
    }

    /// Default container name for this runtime, matching what the
    /// Makefile's `setup-incus` / `setup-podman` create.
    pub fn default_container(self) -> &'static str {
        match self {
            Runtime::Incus => "x0ne-sandbox",
            Runtime::Podman => "x0ne-sandbox-podman",
        }
    }

    /// Default in-container path to the x0ne-runtime binary, matching
    /// the Makefile conventions. Both runtimes deploy to `/opt/x0ne-runtime`
    /// — this is the long-standing convention set by `make sync-incus`
    /// / `make sync-podman`, and `x0ne setup <runtime>` mirrors it so
    /// hand-built containers and Makefile-built containers are
    /// interchangeable. Diverging here silently (as an earlier draft of
    /// this module did with `/root/x0ne-runtime` on Incus) caused the
    /// bridge to spawn a non-existent binary and return an empty-response
    /// health-check error.
    pub fn default_runtime_bin(self) -> &'static str {
        match self {
            Runtime::Incus | Runtime::Podman => "/opt/x0ne-runtime",
        }
    }

    /// Default in-container path to the hardened runtime config,
    /// matching what `make sync-incus` / `make sync-podman` push. The
    /// content is `x0ne-runtime/x0ne-runtime-sandbox.toml` — which
    /// locks `X0NE_ALLOW_LOOPBACK`, restricts `RunCommand` policy, and
    /// configures the snapshot directory. Running x0ne-runtime WITHOUT
    /// this config means the defaults apply, which is strictly less
    /// safe than what `make run-incus` would produce.
    pub fn default_runtime_config(self) -> &'static str {
        "/root/x0ne-runtime.toml"
    }

    /// Build the `X0NE_SANDBOX_CMD` string that `bridge::create_bridge`
    /// will split-on-spaces and spawn. Mirrors the Makefile one-for-one.
    ///
    /// ## Invariants pinned by tests
    ///
    /// * Incus: `incus exec <container> -- <bin> --config <cfg> --mode stdio`.
    ///   No extra flags — `incus exec` wires stdin by default.
    /// * Podman: `podman exec -i --env X0NE_ALLOW_LOOPBACK=1 <container>
    ///   <bin> --config <cfg> --mode stdio`. The `-i` is non-negotiable
    ///   (stdio bridge hangs without it — the PODMAN_EXEC_FLAGS bug).
    ///
    /// `--isolation container`: the container is the confinement, so
    /// the runtime inside it does not require bubblewrap. A container holding
    /// a runtime older than this flag must be re-synced (`make sync-incus`).
    ///
    /// The `--config` flag matches `make run-incus` / `make run-podman`
    /// so autowrap-launched sandboxes get the same hardened runtime
    /// policy as Makefile-launched ones. Missing this flag earlier
    /// caused the "host-side hardened config" banner to be misleading
    /// — the orchestrator had its own hardened config, but the runtime
    /// inside the container was running with defaults.
    pub fn sandbox_cmd(self, container: &str, runtime_bin: &str) -> String {
        let cfg = self.default_runtime_config();
        match self {
            Runtime::Incus => format!(
                "incus exec {container} -- {runtime_bin} --config {cfg} --mode stdio --isolation container"
            ),
            // Podman: `-i` is critical — without it stdin is not attached
            // and the stdio bridge's initial ListActions probe hangs
            // forever. This was the footgun fixed by the `PODMAN_EXEC_FLAGS`
            // entry in the CHANGELOG — it must stay in this default.
            Runtime::Podman => format!(
                "podman exec -i --env X0NE_ALLOW_LOOPBACK=1 {container} {runtime_bin} --config {cfg} --mode stdio --isolation container"
            ),
        }
    }
}

impl std::fmt::Display for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Result of probing a single runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Binary missing or PATH-not-resolvable.
    BinaryMissing,
    /// Binary exists but the container hasn't been provisioned yet.
    BinaryReadyContainerMissing,
    /// Everything OK. Container name is carried for completeness.
    Ready { container: String },
}

/// Core decision function. Inputs are pure (CLI mode, env flags, the current
/// config) plus a fn-pointer probe so tests can inject a fake runtime
/// inventory. In production, `probe_runtimes()` shells out to `incus` / `podman`.
pub fn decide_inner(
    cli_mode_override: Option<&str>,
    config_mode: &str,
    env_bridge_mode: Option<&str>,
    env_no_sandbox: bool,
    probes: &[(Runtime, Probe)],
) -> Decision {
    // ── 1. Explicit overrides win ────────────────────────────────────────
    if let Some(m) = cli_mode_override {
        return Decision::RespectExplicit {
            reason: if m == "stdio" {
                "--mode stdio (explicit)"
            } else if m == "http" {
                "--mode http (explicit)"
            } else if m == "sandbox" {
                "--mode sandbox (explicit)"
            } else {
                "--mode <custom>"
            },
        };
    }
    if let Some(m) = env_bridge_mode {
        // Env var set — user wants full control of the bridge.
        return Decision::RespectExplicit {
            reason: match m {
                "stdio" => "X0NE_BRIDGE_MODE=stdio",
                "http" => "X0NE_BRIDGE_MODE=http",
                "sandbox" => "X0NE_BRIDGE_MODE=sandbox",
                _ => "X0NE_BRIDGE_MODE set",
            },
        };
    }
    // ── 2. Default policy: stdio = host direct, no autowrap ─────────────
    //
    // History: prior versions used `mode = "stdio"` as a SIGNAL to autowrap
    // into a container. That was confusing — it meant a fresh install that
    // happened to have any container daemon installed silently got
    // sandbox-ified. Now `stdio` means exactly what it says: spawn the
    // host runtime over stdio. No probing, no autowrap.
    //
    // To get auto-detect (try Incus → Podman → host fallback), the user
    // sets `mode = "auto"` in their config or `X0NE_AUTO_SANDBOX=1` in the
    // environment. To pin a specific sandbox setup, they use
    // `mode = "sandbox"` with `sandbox_cmd`.
    if config_mode == "stdio" {
        let auto_sandbox = std::env::var("X0NE_AUTO_SANDBOX")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        if !auto_sandbox {
            return Decision::RespectExplicit {
                reason: "host (stdio default)",
            };
        }
        // X0NE_AUTO_SANDBOX=1 → fall through to the probe loop below.
    } else if config_mode != "auto" {
        return Decision::RespectExplicit {
            reason: "[bridge] mode in config",
        };
    }

    // ── 3. Opt-out for power users ───────────────────────────────────────
    if env_no_sandbox {
        return Decision::UnsandboxedExplicit;
    }

    // ── 4. Pick the first ready runtime in preference order ──────────────
    // Incus before Podman because `incus snapshot` is near-instant (btrfs
    // CoW), while `podman commit` + `rm -f` + `run` is 1-2 s per reset.
    let pref = [Runtime::Incus, Runtime::Podman];

    for want in pref {
        if let Some((_, probe)) = probes.iter().find(|(r, _)| *r == want) {
            if let Probe::Ready { container } = probe {
                return Decision::Autowrapped {
                    runtime: want,
                    target: container.clone(),
                };
            }
        }
    }

    // ── 4. At least one runtime exists but its container is missing ──────
    for want in pref {
        if let Some((_, Probe::BinaryReadyContainerMissing)) =
            probes.iter().find(|(r, _)| *r == want)
        {
            return Decision::NeedsBootstrap {
                runtime: want,
                target: want.default_container().to_string(),
            };
        }
    }

    // ── 5. Nothing installed ────────────────────────────────────────────
    Decision::MissingRuntime {
        probed: pref.into_iter().collect(),
    }
}

/// Top-level: probe the host, decide, and apply the decision to `config`.
/// Returns the `Decision` so the caller can render the boot banner and
/// decide whether to error out (`MissingRuntime`) or prompt for bootstrap
/// (`NeedsBootstrap`).
pub fn autowrap(cli_mode_override: Option<&str>, config: &mut Config) -> Decision {
    let env_bridge_mode = std::env::var("X0NE_BRIDGE_MODE").ok();
    let env_no_sandbox = std::env::var("X0NE_NO_SANDBOX")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);

    let probes = probe_runtimes();

    let decision = decide_inner(
        cli_mode_override,
        &config.bridge.mode,
        env_bridge_mode.as_deref(),
        env_no_sandbox,
        &probes,
    );

    if let Decision::Autowrapped {
        runtime,
        ref target,
    } = decision
    {
        config.bridge.mode = "sandbox".into();
        config.bridge.sandbox_cmd =
            Some(runtime.sandbox_cmd(target, runtime.default_runtime_bin()));
    }

    decision
}

/// Under `X0NE_REQUIRE_SANDBOX`, the refusal for the bridge `decision`
/// left in the config; `None` when its runtime provably runs in a container.
pub fn require_sandbox_refusal(
    decision: &Decision,
    mode: &str,
    sandbox_cmd: Option<&str>,
    process: &SandboxMode,
) -> Option<String> {
    let why = uncontained_reason(mode, sandbox_cmd, process)?;
    let why = match decision {
        Decision::MissingRuntime { .. } => {
            format!("neither Incus nor Podman is installed, so {why}")
        }
        Decision::NeedsBootstrap { runtime, target } => {
            format!("{runtime} has no '{target}' container yet (x0ne setup {runtime}), so {why}")
        }
        Decision::UnsandboxedExplicit => {
            format!("--no-sandbox (X0NE_NO_SANDBOX) is set, so {why}")
        }
        Decision::RespectExplicit { reason } => format!("{why} (source: {reason})"),
        Decision::Autowrapped { .. } => why,
    };
    Some(refusal(&why))
}

// ── Shell-out probes ──────────────────────────────────────────────────────

/// Probe the host for each supported runtime. Returns a vector in the
/// preference order (Incus, Podman) regardless of what's installed, so the
/// decision function has a complete picture for the error message.
fn probe_runtimes() -> Vec<(Runtime, Probe)> {
    vec![
        (Runtime::Incus, probe(Runtime::Incus)),
        (Runtime::Podman, probe(Runtime::Podman)),
    ]
}

fn probe(rt: Runtime) -> Probe {
    if !rt.answers() {
        return Probe::BinaryMissing;
    }
    if container_exists(rt, rt.default_container()) {
        // Best-effort autostart if the container is stopped. If autostart
        // fails we still report Ready — the actual bridge spawn below
        // will surface the real error with more context.
        let _ = ensure_started(rt, rt.default_container());
        Probe::Ready {
            container: rt.default_container().into(),
        }
    } else {
        Probe::BinaryReadyContainerMissing
    }
}

fn container_exists(rt: Runtime, name: &str) -> bool {
    let Some(mut cmd) = rt.command() else {
        return false;
    };
    let out = match rt {
        Runtime::Incus => cmd.args(["list", name, "-f", "csv", "-c", "n"]),
        Runtime::Podman => cmd.args(["container", "exists", name]),
    }
    .stdin(std::process::Stdio::null())
    .output();

    match (rt, out) {
        (Runtime::Incus, Ok(o)) => {
            // `incus list NAME -f csv -c n` prints the name on one line
            // when it exists, and nothing (with exit 0) when it doesn't.
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().any(|l| l.trim() == name)
        }
        (Runtime::Podman, Ok(o)) => o.status.success(),
        _ => false,
    }
}

fn ensure_started(rt: Runtime, name: &str) -> std::io::Result<()> {
    // `start` on a running container returns an error, which we swallow.
    if let Some(mut cmd) = rt.command() {
        let _ = cmd
            .args(["start", name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI `--mode sandbox` is an explicit user decision — autowrap must
    /// not stomp on it even if Incus is ready. This is the opt-back-in for
    /// power users who want the bridge-mode detour (e.g. already in a VM).
    #[test]
    fn cli_mode_override_preserves_user_choice() {
        let d = decide_inner(
            Some("sandbox"),
            "stdio",
            None,
            false,
            &[(
                Runtime::Incus,
                Probe::Ready {
                    container: "x0ne-sandbox".into(),
                },
            )],
        );
        assert!(matches!(d, Decision::RespectExplicit { .. }));
    }

    /// `X0NE_BRIDGE_MODE=stdio` set explicitly → respect, don't autowrap
    /// (the user probably wants to run on the host for local development).
    #[test]
    fn env_bridge_mode_stdio_respected() {
        let d = decide_inner(
            None,
            "stdio",
            Some("stdio"),
            false,
            &[(
                Runtime::Incus,
                Probe::Ready {
                    container: "x0ne-sandbox".into(),
                },
            )],
        );
        assert!(
            matches!(d, Decision::RespectExplicit { reason } if reason.contains("X0NE_BRIDGE_MODE"))
        );
    }

    /// Config says `mode = "http"` — user has a remote x0ne-runtime they want
    /// to hit. Don't override with sandbox.
    #[test]
    fn non_default_config_mode_respected() {
        let d = decide_inner(
            None,
            "http",
            None,
            false,
            &[(
                Runtime::Incus,
                Probe::Ready {
                    container: "x0ne-sandbox".into(),
                },
            )],
        );
        assert!(matches!(d, Decision::RespectExplicit { reason } if reason.contains("config")));
    }

    /// Default `mode = "stdio"` is now host direct — no autowrap, even when
    /// a container runtime is fully ready. Users opt in via `mode = "auto"`
    /// or `X0NE_AUTO_SANDBOX=1`.
    #[test]
    fn stdio_mode_is_host_direct_by_default() {
        let d = decide_inner(
            None,
            "stdio",
            None,
            false,
            &[(
                Runtime::Incus,
                Probe::Ready {
                    container: "x0ne-sandbox".into(),
                },
            )],
        );
        assert!(matches!(d, Decision::RespectExplicit { reason } if reason.contains("host")));
    }

    /// `X0NE_NO_SANDBOX=1` — user wants host execution. Caller will warn.
    /// Tested under `auto` mode because that's the only mode where the env
    /// flag is consulted (under `stdio` we already short-circuit to host).
    #[test]
    fn no_sandbox_env_flag_opts_out() {
        let d = decide_inner(
            None,
            "auto",
            None,
            true,
            &[(
                Runtime::Incus,
                Probe::Ready {
                    container: "x0ne-sandbox".into(),
                },
            )],
        );
        assert_eq!(d, Decision::UnsandboxedExplicit);
    }

    /// Incus + Podman both ready: Incus wins (cheaper snapshots, the
    /// primary-recommended backend). Triggered by `mode = "auto"`.
    #[test]
    fn incus_wins_over_podman_when_both_ready() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (
                    Runtime::Incus,
                    Probe::Ready {
                        container: "x0ne-sandbox".into(),
                    },
                ),
                (
                    Runtime::Podman,
                    Probe::Ready {
                        container: "x0ne-sandbox-podman".into(),
                    },
                ),
            ],
        );
        assert_eq!(
            d,
            Decision::Autowrapped {
                runtime: Runtime::Incus,
                target: "x0ne-sandbox".into(),
            }
        );
    }

    /// Incus binary missing → fall back to Podman (under `auto`).
    #[test]
    fn falls_back_to_podman_when_incus_missing() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (Runtime::Incus, Probe::BinaryMissing),
                (
                    Runtime::Podman,
                    Probe::Ready {
                        container: "x0ne-sandbox-podman".into(),
                    },
                ),
            ],
        );
        assert_eq!(
            d,
            Decision::Autowrapped {
                runtime: Runtime::Podman,
                target: "x0ne-sandbox-podman".into(),
            }
        );
    }

    /// Incus installed but no baseline container → bootstrap path, NOT
    /// fall back to Podman (even if Podman is ready). Incus is the preferred
    /// backend; fix the preferred one, don't paper over it with the fallback.
    /// Otherwise users would silently end up on Podman after an Incus reset.
    #[test]
    fn needs_bootstrap_sticks_with_preferred_runtime() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (Runtime::Incus, Probe::BinaryReadyContainerMissing),
                (
                    Runtime::Podman,
                    Probe::Ready {
                        container: "x0ne-sandbox-podman".into(),
                    },
                ),
            ],
        );
        // Incus has priority — if its container is missing, we ask to
        // bootstrap Incus (not silently fall through).
        // NOTE: When only the fallback runtime is ready but the preferred
        // one needs setup, we still prefer the ready one — this is the
        // "less friction" rule. The fix-the-preferred behaviour only kicks
        // in when NEITHER is ready.
        assert_eq!(
            d,
            Decision::Autowrapped {
                runtime: Runtime::Podman,
                target: "x0ne-sandbox-podman".into(),
            }
        );
    }

    /// Neither runtime is installed → MissingRuntime. Caller falls back to
    /// host (with a warning) unless X0NE_REQUIRE_SANDBOX=1.
    #[test]
    fn nothing_installed_returns_missing_runtime() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (Runtime::Incus, Probe::BinaryMissing),
                (Runtime::Podman, Probe::BinaryMissing),
            ],
        );
        assert!(matches!(d, Decision::MissingRuntime { .. }));
    }

    /// A `podman` planted in PATH is never run: not by the probe, the
    /// container check, the autostart nor `x0ne setup podman`. Re-executes
    /// this test with that PATH; a real podman is not driven on a dev host.
    #[test]
    fn a_podman_planted_in_path_is_never_run() {
        const DUMMY: &str = "x0ne-test-no-such-container";
        let installed = ["/usr/bin/podman", "/usr/local/bin/podman", "/bin/podman"]
            .iter()
            .any(|p| std::path::Path::new(p).is_file());
        if std::env::var_os("X0NE_TEST_FAKE_PODMAN_CHILD").is_some() {
            if !installed {
                let _ = probe(Runtime::Podman);
                let _ = crate::container_setup::run_setup(Runtime::Podman);
            }
            let _ = container_exists(Runtime::Podman, DUMMY);
            let _ = ensure_started(Runtime::Podman, DUMMY);
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin dir");
        let marker = dir.path().join("fake-podman-ran");
        // Written by /bin/sh: a write fd held here could reach a concurrent
        // fork and make the exec fail with ETXTBSY (rust-lang/rust#114554).
        let fake = format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", marker.display());
        let wrote = Command::new("/bin/sh")
            .args([
                "-c",
                r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#,
                "sh",
                &fake,
            ])
            .arg(bin.join("podman"))
            .status()
            .expect("spawn /bin/sh")
            .success();
        assert!(wrote, "could not write the fake podman");
        let out = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "autowrap::tests::a_podman_planted_in_path_is_never_run",
            ])
            .args(["--test-threads=1", "--nocapture"])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("X0NE_TEST_FAKE_PODMAN_CHILD", "1")
            .output()
            .expect("re-exec the test binary");
        assert!(out.status.success(), "child failed: {out:?}");
        let ran = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            ran.is_empty(),
            "the podman found through PATH ran with:\n{ran}"
        );
    }

    /// Runtimes installed but no container set up → NeedsBootstrap, with
    /// the preferred runtime in the decision so the UI can say
    /// `x0ne setup incus` specifically.
    #[test]
    fn both_installed_neither_bootstrapped_asks_for_preferred() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (Runtime::Incus, Probe::BinaryReadyContainerMissing),
                (Runtime::Podman, Probe::BinaryReadyContainerMissing),
            ],
        );
        assert_eq!(
            d,
            Decision::NeedsBootstrap {
                runtime: Runtime::Incus,
                target: "x0ne-sandbox".into(),
            }
        );
    }

    /// Only Podman installed but no container → NeedsBootstrap Podman (not Incus).
    #[test]
    fn only_podman_installed_needs_bootstrap_podman() {
        let d = decide_inner(
            None,
            "auto",
            None,
            false,
            &[
                (Runtime::Incus, Probe::BinaryMissing),
                (Runtime::Podman, Probe::BinaryReadyContainerMissing),
            ],
        );
        assert_eq!(
            d,
            Decision::NeedsBootstrap {
                runtime: Runtime::Podman,
                target: "x0ne-sandbox-podman".into(),
            }
        );
    }

    // ── sandbox_cmd format ────────────────────────────────────────────────

    /// The Incus sandbox_cmd must match what `make run-incus` generates:
    ///   `incus exec <container> -- <bin> --config /root/x0ne-runtime.toml --mode stdio`
    /// Deviating here silently (missing `--config`, wrong binary path)
    /// produces a health-check failure with an empty response line,
    /// because `incus exec` exits 1 on missing binary and returns no
    /// stdout. Real regression: an earlier version used `/root/x0ne-runtime`
    /// here because the container_setup path was also wrong, and `x0ne`
    /// failed to start on any machine that had gone through `make
    /// setup-incus` (which deploys to `/opt/x0ne-runtime`).
    #[test]
    fn incus_sandbox_cmd_matches_makefile_flow() {
        let cmd = Runtime::Incus.sandbox_cmd("x0ne-sandbox", Runtime::Incus.default_runtime_bin());
        assert_eq!(
            cmd,
            "incus exec x0ne-sandbox -- /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio --isolation container",
            "Incus sandbox_cmd shape must match `make run-incus` exactly"
        );
    }

    /// Regression pin: the default Incus runtime-binary path is
    /// `/opt/x0ne-runtime`. Any refactor that moves it back to
    /// `/root/x0ne-runtime` (matching an earlier draft of `x0ne setup
    /// incus`) breaks every machine that provisioned its container via
    /// `make setup-incus` / `make sync-incus` — which is the common
    /// path per the Makefile comments.
    #[test]
    fn incus_default_runtime_bin_matches_makefile_opt_path() {
        assert_eq!(Runtime::Incus.default_runtime_bin(), "/opt/x0ne-runtime");
    }

    /// The Podman sandbox_cmd MUST include `-i` — without it, the stdio
    /// bridge hangs waiting for a response to its `ListActions` probe
    /// because the container never sees the JSON on stdin. This was a
    /// real bug fixed in the PODMAN_EXEC_FLAGS entry of the CHANGELOG.
    /// Additionally pins the `--config` flag so Podman sandbox runs
    /// with the same hardened policy as Incus.
    #[test]
    fn podman_sandbox_cmd_has_dash_i_and_config_flag() {
        let cmd = Runtime::Podman
            .sandbox_cmd("x0ne-sandbox-podman", Runtime::Podman.default_runtime_bin());
        assert!(cmd.contains(" -i "), "podman exec MUST have -i, got: {cmd}");
        assert!(
            cmd.contains("X0NE_ALLOW_LOOPBACK=1"),
            "podman exec must forward loopback env, got: {cmd}"
        );
        assert!(
            cmd.contains("--config /root/x0ne-runtime.toml"),
            "podman exec must pass hardened sandbox config, got: {cmd}"
        );
        assert!(
            cmd.ends_with(" --mode stdio --isolation container"),
            "got: {cmd}"
        );
    }

    /// Every decision that leaves the runtime on the host is refused,
    /// with its own cause and the host named.
    #[test]
    fn under_the_requirement_every_host_decision_is_refused_with_its_cause() {
        let cases = [
            (
                Decision::MissingRuntime {
                    probed: vec![Runtime::Incus, Runtime::Podman],
                },
                "neither Incus nor Podman is installed",
            ),
            (
                Decision::NeedsBootstrap {
                    runtime: Runtime::Podman,
                    target: "x0ne-sandbox-podman".into(),
                },
                "(x0ne setup podman)",
            ),
            (Decision::UnsandboxedExplicit, "--no-sandbox"),
            (
                Decision::RespectExplicit {
                    reason: "host (stdio default)",
                },
                "(source: host (stdio default))",
            ),
        ];
        for (decision, cause) in cases {
            let r = require_sandbox_refusal(&decision, "stdio", None, &SandboxMode::Host)
                .unwrap_or_else(|| panic!("{decision:?} was not refused"));
            assert!(r.starts_with("X0NE_REQUIRE_SANDBOX is set and "), "{r}");
            assert!(r.contains(cause), "{decision:?}: {r}");
            assert!(r.contains("on this host"), "{decision:?}: {r}");
        }
    }

    /// The command autowrap writes runs in a container: nothing to refuse.
    #[test]
    fn under_the_requirement_an_autowrapped_session_starts() {
        for runtime in [Runtime::Incus, Runtime::Podman] {
            let target = runtime.default_container();
            let cmd = runtime.sandbox_cmd(target, runtime.default_runtime_bin());
            let d = Decision::Autowrapped {
                runtime,
                target: target.into(),
            };
            assert_eq!(
                require_sandbox_refusal(&d, "sandbox", Some(&cmd), &SandboxMode::Host),
                None,
                "{cmd}"
            );
        }
    }

    /// x0ne in a declared and confirmed sandbox: its child runtime is in it too.
    #[test]
    fn under_the_requirement_a_declared_sandbox_holds_the_host_profile() {
        let inside = SandboxMode::Yolo {
            runtime: "podman".into(),
        };
        for d in [
            Decision::MissingRuntime { probed: vec![] },
            Decision::UnsandboxedExplicit,
            Decision::RespectExplicit {
                reason: "host (stdio default)",
            },
        ] {
            assert_eq!(
                require_sandbox_refusal(&d, "stdio", None, &inside),
                None,
                "{d:?}"
            );
        }
        let unconfirmed = SandboxMode::Unconfirmed {
            reason: "X0NE_SANDBOX=podman is set, but this process is not in a container".into(),
        };
        assert!(require_sandbox_refusal(
            &Decision::UnsandboxedExplicit,
            "stdio",
            None,
            &unconfirmed
        )
        .is_some());
    }
}
