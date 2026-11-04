//! Bootstrap the x0ne sandbox container from scratch.
//!
//! Provisions either an Incus instance or a Podman container that will
//! serve as the default `x0ne` execution environment. This is the Rust
//! equivalent of the Makefile's `setup-incus` / `setup-podman` targets,
//! minus the glibc-2.35 cross-compile step — that's delegated to `cargo
//! zigbuild` which must be invoked separately when the user is developing
//! x0ne-runtime and wants a musl / glibc-2.35 binary.
//!
//! For a standard user who just ran `make install` (or a packaged
//! `apt install x0ne`), the binary at `~/.cargo/bin/x0ne-runtime` is
//! already compatible with a Debian 12 container — no cross-compile
//! needed.
//!
//! ## What this does NOT do (by design)
//!
//! - Version checks: we don't verify the x0ne-runtime binary's glibc
//!   requirements match the container's libc. If they don't, the first
//!   `ListActions` probe will fail with a clear error pointing here.
//! - Remote Incus: only local `incus` is supported. Remote clusters
//!   (`incus remote add`) are a `make setup-incus` concern — the Makefile
//!   has the `INCUS_REMOTE` env var for that flow.
//! - Podman baseline tagging: we use the default `localhost/x0ne-sandbox-baseline`
//!   name. Users wanting to version their baselines (`:v0.2`, `:experimental`)
//!   should use the Makefile.
//!
//! All this means: the subcommand works for 80% of users who just want
//! `x0ne` to work out of the box. The Makefile stays as the advanced-user
//! entry point.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use colored::Colorize;

use crate::autowrap::Runtime;
use x0ne_runtime::executor::podman_bin::{not_found_message, PODMAN_LOCATIONS};

/// Default basic deps installed in the sandbox container. Mirrored from
/// the Makefile's `setup-incus` / `setup-podman` apt-get invocations so
/// both paths produce identical baselines — users switching from
/// `make setup-incus` to `x0ne setup incus` won't see differences.
const BASE_PACKAGES: &[&str] = &[
    "python3",
    "python3-pip",
    "curl",
    "git",
    "jq",
    "procps",
    "net-tools",
];

/// Entry point for `x0ne setup <runtime>`. Returns `Ok(())` if the sandbox
/// is ready to be used; an `Err` with a human-readable cause otherwise.
/// Safe to re-run — every step is idempotent (existing container → noop,
/// existing snapshot → reused).
pub fn run_setup(runtime: Runtime) -> Result<()> {
    println!();
    println!(
        "  {}  {} — {}",
        "◆".truecolor(0, 200, 120).bold(),
        format!("x0ne setup {}", runtime.display_name())
            .truecolor(0, 200, 120)
            .bold(),
        "provisioning sandbox".truecolor(110, 120, 135),
    );
    println!();

    let found = match runtime {
        Runtime::Incus => "in PATH".to_string(),
        Runtime::Podman => format!("at {} (PATH is not searched)", PODMAN_LOCATIONS.join(", ")),
    };
    if !runtime.answers() {
        bail!(
            "{} not found {found} — install it first:\n    Incus:  https://linuxcontainers.org/incus/\n    Podman: https://podman.io/",
            runtime.bin(),
        );
    }
    step_ok("runtime found", &format!("{} {found}", runtime.bin()));

    let runtime_bin_src = locate_runtime_binary()?;
    step_ok(
        "x0ne-runtime binary located",
        &runtime_bin_src.to_string_lossy(),
    );

    match runtime {
        Runtime::Incus => setup_incus(&runtime_bin_src),
        Runtime::Podman => setup_podman(&runtime_bin_src),
    }
}

/// Find the x0ne-runtime binary to deploy. Priority order matches what
/// a user is most likely to want:
///
/// 1. `X0NE_RUNTIME_BIN` env var — explicit override (CI, cross-compiled
///    glibc-2.35 build, distro packaging).
/// 2. `~/.cargo/bin/x0ne-runtime` — the path `make install` writes to.
/// 3. `./target/release/x0ne-runtime` — source-tree fallback for devs
///    running `x0ne setup` without an install step.
/// 4. Current dir's `x0ne-runtime` — last resort.
///
/// If none exist, we bail with a clear error pointing to `make install` or
/// `cargo install --path x0ne-runtime`.
fn locate_runtime_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("X0NE_RUNTIME_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
        bail!(
            "X0NE_RUNTIME_BIN points to a missing path: {}",
            pb.display()
        );
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_bin = home.as_ref().map(|h| h.join(".cargo/bin/x0ne-runtime"));

    let candidates: Vec<PathBuf> = [
        cargo_bin,
        Some(PathBuf::from("target/release/x0ne-runtime")),
        Some(PathBuf::from("./x0ne-runtime")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }

    Err(anyhow!(
        "x0ne-runtime binary not found. Run one of:\n    \
        make install                            # installs to ~/.cargo/bin/\n    \
        cargo install --path x0ne-runtime       # single-crate install\n    \
        X0NE_RUNTIME_BIN=/path/to/x0ne-runtime x0ne setup {}",
        "<runtime>",
    ))
}

/// Find the hardened sandbox config shipped with x0ne-runtime: the user's
/// config directory (`make install` deploys the TOML to `~/.config/x0ne/`),
/// then the system-wide copy.
///
/// Returns `None` if no copy is available; the caller falls back to
/// skipping the config push and lets the runtime boot with defaults.
fn locate_sandbox_runtime_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    sandbox_config_candidates(xdg)
        .into_iter()
        .find(|c| c.exists())
}

/// Where the sandbox config may come from, in order.
fn sandbox_config_candidates(xdg: Option<PathBuf>) -> Vec<PathBuf> {
    [
        // XDG base dir spec: a relative value is invalid and ignored.
        xdg.filter(|x| x.is_absolute())
            .map(|x| x.join("x0ne/x0ne-runtime-sandbox.toml")),
        // System-wide.
        Some(PathBuf::from("/etc/x0ne/x0ne-runtime-sandbox.toml")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

// ── Incus ────────────────────────────────────────────────────────────────────

fn setup_incus(runtime_bin_src: &PathBuf) -> Result<()> {
    let container = "x0ne-sandbox";

    // 1. Launch or note that the container already exists.
    let exists = incus_container_exists(container);
    if exists {
        step_ok(
            "container",
            &format!("'{container}' already exists — reusing"),
        );
    } else {
        println!(
            "  {}  launching {} from {}…",
            "◇".truecolor(0, 180, 220),
            container.truecolor(220, 220, 220).bold(),
            "images:debian/13".truecolor(110, 120, 135),
        );
        run(
            Runtime::Incus,
            &["launch", "images:debian/13", container],
            "launch incus container",
        )?;
    }

    // 2. Install base deps. Skip if `python3` is already there (our canary).
    if !incus_exec_ok(container, &["which", "python3"]) {
        println!(
            "  {}  installing base packages ({})…",
            "◇".truecolor(0, 180, 220),
            BASE_PACKAGES.join(", ").truecolor(110, 120, 135),
        );
        run(
            Runtime::Incus,
            &["exec", container, "--", "apt-get", "update"],
            "apt-get update",
        )?;
        let mut args: Vec<&str> = vec!["exec", container, "--", "apt-get", "install", "-y"];
        args.extend(BASE_PACKAGES);
        run(Runtime::Incus, &args, "apt-get install")?;
        step_ok("base packages", "installed");
    } else {
        step_ok("base packages", "already present");
    }

    // 3. Deploy the x0ne-runtime binary to `/opt/x0ne-runtime`. This
    //    path is the long-standing Makefile convention (`make sync-incus`
    //    pushes there, `make run-incus` spawns there) — diverging here
    //    (e.g. `/root/x0ne-runtime`) breaks interop with anyone who used
    //    the Makefile to provision their container, and produces an
    //    empty-response health-check error on `x0ne` startup.
    run(
        Runtime::Incus,
        &[
            "file",
            "push",
            &runtime_bin_src.to_string_lossy(),
            &format!("{container}/opt/x0ne-runtime"),
        ],
        "incus file push",
    )?;
    let _ = Command::new("incus")
        .args(["exec", container, "--", "chmod", "+x", "/opt/x0ne-runtime"])
        .stdin(Stdio::null())
        .status();
    step_ok(
        "x0ne-runtime deployed",
        &format!("{}:/opt/x0ne-runtime", container),
    );

    // 4. Deploy the hardened sandbox config to `/root/x0ne-runtime.toml`
    //    so the runtime inside boots with the same policy that `make
    //    run-incus` applies. If the host's copy of the config is missing
    //    (unusual post `make install` but possible for source-less
    //    packager builds), we fall back to skipping — the runtime will
    //    boot with its defaults, which are less hardened but still
    //    functional. We log the choice either way.
    if let Some(cfg_src) = locate_sandbox_runtime_config() {
        run(
            Runtime::Incus,
            &[
                "file",
                "push",
                &cfg_src.to_string_lossy(),
                &format!("{container}/root/x0ne-runtime.toml"),
            ],
            "incus file push (sandbox config)",
        )?;
        step_ok(
            "sandbox config deployed",
            &format!("{}:/root/x0ne-runtime.toml", container),
        );
    } else {
        step_ok(
            "sandbox config",
            "host copy missing — runtime will use defaults (less hardened)",
        );
    }

    // 5. Snapshot (idempotent — Incus refuses duplicates, which is fine).
    let snap_exists = incus_snapshot_exists(container, "baseline");
    if snap_exists {
        step_ok("baseline snapshot", "already exists");
    } else {
        run(
            Runtime::Incus,
            &["snapshot", "create", container, "baseline"],
            "create baseline snapshot",
        )?;
        step_ok("baseline snapshot", "captured");
    }

    println!();
    println!(
        "  {}  {}",
        "✓".truecolor(0, 200, 120).bold(),
        "Incus sandbox ready — just run `x0ne`"
            .truecolor(0, 200, 120)
            .bold(),
    );
    println!();
    Ok(())
}

fn incus_container_exists(name: &str) -> bool {
    let out = Command::new("incus")
        .args(["list", name, "-f", "csv", "-c", "n"])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().any(|l| l.trim() == name)
        }
        Err(_) => false,
    }
}

fn incus_exec_ok(container: &str, argv: &[&str]) -> bool {
    let mut full: Vec<&str> = vec!["exec", container, "--"];
    full.extend(argv);
    Command::new("incus")
        .args(&full)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn incus_snapshot_exists(container: &str, snap: &str) -> bool {
    // `incus snapshot list --format csv` prints one `name,taken_at,expires,stateful`
    // row per snapshot. The previous implementation grepped `incus info` for
    // `- baseline (…)` lines; Incus 6.x prints that section as a TABLE
    // (`| baseline | 2026/08/16 … |`), so the check never matched and every
    // re-run of `x0ne setup` died on `UNIQUE constraint failed:
    // instances_snapshots` (owner's machine, 2026-08-16). Idempotence is the
    // documented contract of `x0ne setup`; a format-fragile probe broke it.
    Command::new("incus")
        .args(["snapshot", "list", container, "--format", "csv"])
        .stdin(Stdio::null())
        .output()
        .map(|o| snapshot_listed(&String::from_utf8_lossy(&o.stdout), snap))
        .unwrap_or(false)
}

/// Does the CSV output of `incus snapshot list --format csv` name `snap`?
/// The first column is the name; a header line, if any, is skipped by the
/// same rule (it does not equal `snap`).
fn snapshot_listed(csv: &str, snap: &str) -> bool {
    csv.lines()
        .filter_map(|l| l.split(',').next())
        .any(|name| name.trim() == snap)
}

// ── Podman ───────────────────────────────────────────────────────────────────

fn setup_podman(runtime_bin_src: &PathBuf) -> Result<()> {
    let container = "x0ne-sandbox-podman";
    let baseline_img = "localhost/x0ne-sandbox-baseline:latest";

    // 1. Run (or start) the container.
    let exists = cli(Runtime::Podman)?
        .args(["container", "exists", container])
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if exists {
        step_ok(
            "container",
            &format!("'{container}' already exists — starting"),
        );
        // `podman start` is idempotent — succeeds if already running.
        let _ = cli(Runtime::Podman)?
            .args(["start", container])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        println!(
            "  {}  launching {} from {}…",
            "◇".truecolor(0, 180, 220),
            container.truecolor(220, 220, 220).bold(),
            "debian:12".truecolor(110, 120, 135),
        );
        run(
            Runtime::Podman,
            &[
                "run",
                "--name",
                container,
                "-d",
                "debian:12",
                "sleep",
                "infinity",
            ],
            "podman run",
        )?;
    }

    // 2. Install base deps (same canary as Incus).
    let has_python = cli(Runtime::Podman)?
        .args(["exec", container, "which", "python3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !has_python {
        println!(
            "  {}  installing base packages ({})…",
            "◇".truecolor(0, 180, 220),
            BASE_PACKAGES.join(", ").truecolor(110, 120, 135),
        );
        run(
            Runtime::Podman,
            &["exec", container, "apt-get", "update"],
            "apt-get update",
        )?;
        let mut args: Vec<&str> = vec!["exec", container, "apt-get", "install", "-y"];
        args.extend(BASE_PACKAGES);
        run(Runtime::Podman, &args, "apt-get install")?;
        step_ok("base packages", "installed");
    } else {
        step_ok("base packages", "already present");
    }

    // 3. Deploy the x0ne-runtime binary.
    run(
        Runtime::Podman,
        &[
            "cp",
            &runtime_bin_src.to_string_lossy(),
            &format!("{container}:/opt/x0ne-runtime"),
        ],
        "podman cp",
    )?;
    // Make sure it's executable — `podman cp` preserves permissions from
    // the host, but some distros' `install` invocations strip +x.
    let _ = cli(Runtime::Podman)?
        .args(["exec", container, "chmod", "+x", "/opt/x0ne-runtime"])
        .stdin(Stdio::null())
        .status();
    step_ok(
        "x0ne-runtime deployed",
        &format!("{container}:/opt/x0ne-runtime"),
    );

    // 3b. Deploy the hardened sandbox config so autowrap's `--config
    //     /root/x0ne-runtime.toml` in sandbox_cmd actually resolves.
    //     Without this, the runtime boots with defaults and the
    //     orchestrator's "hardened" banner is misleading.
    if let Some(cfg_src) = locate_sandbox_runtime_config() {
        run(
            Runtime::Podman,
            &[
                "cp",
                &cfg_src.to_string_lossy(),
                &format!("{container}:/root/x0ne-runtime.toml"),
            ],
            "podman cp (sandbox config)",
        )?;
        step_ok(
            "sandbox config deployed",
            &format!("{container}:/root/x0ne-runtime.toml"),
        );
    } else {
        step_ok(
            "sandbox config",
            "host copy missing — runtime will use defaults (less hardened)",
        );
    }

    // 4. Commit the baseline image so `reset-podman`-style flows have a
    // known-good starting point. Re-committing overwrites the tag, which
    // is what we want if the user re-runs setup after a binary rebuild.
    run(
        Runtime::Podman,
        &["commit", container, baseline_img],
        "podman commit baseline",
    )?;
    step_ok("baseline image", baseline_img);

    println!();
    println!(
        "  {}  {}",
        "✓".truecolor(0, 200, 120).bold(),
        "Podman sandbox ready — just run `x0ne`"
            .truecolor(0, 200, 120)
            .bold(),
    );
    println!();
    Ok(())
}

// ── Shell helpers ────────────────────────────────────────────────────────────

/// The runtime's CLI; podman only from a fixed path (`Runtime::command`).
fn cli(rt: Runtime) -> Result<Command> {
    rt.command().ok_or_else(|| anyhow!(not_found_message()))
}

fn run(rt: Runtime, args: &[&str], label: &str) -> Result<()> {
    let bin = rt.bin();
    let status = cli(rt)?
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("spawn `{bin} {}`", args.join(" ")))?;
    if !status.success() {
        bail!(
            "{label} failed (exit {}): `{bin} {}`",
            status.code().unwrap_or(-1),
            args.join(" "),
        );
    }
    Ok(())
}

fn step_ok(label: &str, detail: &str) {
    println!(
        "  {}  {}  {}",
        "✓".truecolor(0, 200, 120).bold(),
        label.truecolor(220, 220, 220),
        detail.truecolor(110, 120, 135),
    );
}

#[cfg(test)]
mod tests {
    /// If this fails, `x0ne setup` is no longer idempotent: a second run
    /// would try to re-create the `baseline` snapshot and die on Incus's
    /// UNIQUE constraint — exactly what happened on 2026-08-16.
    #[test]
    fn a_snapshot_present_in_the_csv_listing_is_detected() {
        let csv = "baseline,2026/08/16 03:49 CEST,\" \",NO\n";
        assert!(super::snapshot_listed(csv, "baseline"));
        assert!(!super::snapshot_listed(csv, "other"));
        assert!(!super::snapshot_listed("", "baseline"));
        // A header row or an unrelated first column never matches.
        assert!(!super::snapshot_listed(
            "NAME,TAKEN AT\nbase,x\n",
            "baseline"
        ));
    }

    use super::*;

    /// The sandbox config pushed into the container decides the policy of
    /// the runtime there. A relative candidate was read from the cwd, so a
    /// cloned repository could ship its own (audit 2026-11-02, A11).
    #[test]
    fn the_sandbox_config_is_never_read_from_the_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let hostile = cwd.path().join("x0ne-runtime/x0ne-runtime-sandbox.toml");
        std::fs::create_dir_all(hostile.parent().unwrap()).unwrap();
        std::fs::write(&hostile, "[policy]\nmode = \"auto\"\n").unwrap();
        let home = tempfile::tempdir().unwrap();
        for xdg in [
            Some(home.path().join(".config")),
            Some(PathBuf::from(".")),
            Some(PathBuf::from("")),
            None,
        ] {
            for c in sandbox_config_candidates(xdg.clone()) {
                assert!(c.is_absolute(), "{xdg:?}: relative candidate {c:?}");
                assert_ne!(cwd.path().join(&c), hostile, "{xdg:?}: {c:?}");
            }
        }
    }

    /// `X0NE_RUNTIME_BIN` pointing at a missing file must fail with a
    /// clear error (not silently fall through to the PATH candidates).
    /// This preserves "explicit configuration wins; if it's wrong, tell me".
    #[test]
    fn locate_runtime_binary_rejects_missing_env_override() {
        std::env::set_var("X0NE_RUNTIME_BIN", "/tmp/does/not/exist-x0ne-runtime");
        let r = locate_runtime_binary();
        std::env::remove_var("X0NE_RUNTIME_BIN");
        assert!(r.is_err(), "missing X0NE_RUNTIME_BIN path must fail");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("does/not/exist"),
            "error must echo path, got: {msg}"
        );
    }

    /// BASE_PACKAGES must stay in sync with the Makefile's
    /// setup-incus/setup-podman targets — the canary check
    /// (`which python3`) assumes python3 is always first/present.
    #[test]
    fn base_packages_include_canary() {
        assert!(
            BASE_PACKAGES.contains(&"python3"),
            "python3 is the canary used by setup_incus/setup_podman to determine whether to re-run apt-get install"
        );
    }

    /// Podman comes from a fixed path only (`Runtime::command`). Whitespace
    /// is stripped first, so a reformatted call is still seen.
    #[test]
    fn podman_is_never_spawned_by_name() {
        let files = [
            ("autowrap.rs", include_str!("autowrap.rs")),
            ("container_setup.rs", include_str!("container_setup.rs")),
        ];
        for (file, src) in files {
            let head = &src[..src.find("#[cfg(test)]").expect("test module")];
            let flat: String = head.split_whitespace().collect();
            for spawn in [
                "Command::new(\"podman\")",
                "run(\"podman\"",
                "Command::new(name)",
                "Command::new(bin)",
            ] {
                assert!(!flat.contains(spawn), "{file} spawns `{spawn}`");
            }
        }
    }
}
