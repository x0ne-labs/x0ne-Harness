//! Attacks on the second layer under bwrap: Landlock and rlimits.
//!
//! Run them in a container (`--security-opt unmask=ALL`, no `--privileged`):
//! they start a fork bomb and a memory balloon. Without bwrap they skip,
//! unless `X0NE_REQUIRE_BWRAP=1`, which turns the skip into a failure.

use std::path::Path;
use std::time::{Duration, Instant};

use x0ne_runtime::action::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::executor::isolation;
use x0ne_runtime::executor::process::run_command;
use x0ne_runtime::handles::new_shared;
use x0ne_runtime::result::{ActionOutput, ActionResult};

fn bwrap_or_skip(test: &str) -> bool {
    match isolation::confinement(&AppConfig::defaults().isolation) {
        Ok(isolation::Confinement::Bwrap(_)) => true,
        other => {
            assert!(
                std::env::var_os("X0NE_REQUIRE_BWRAP").is_none(),
                "{test}: bwrap required but unusable: {other:?}"
            );
            eprintln!("SKIPPED {test}: bubblewrap unusable here: {other:?}");
            false
        }
    }
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("scratch dir")
}

fn cfg(ws: &Path) -> AppConfig {
    let mut cfg = AppConfig::defaults();
    cfg.policy.allowed_write_paths = vec![ws.to_string_lossy().into_owned()];
    cfg.isolation.confine_helper_override = Some(env!("CARGO_BIN_EXE_x0ne-runtime").into());
    cfg
}

async fn sh(cfg: &AppConfig, script: &str, cwd: &Path, timeout: u64) -> (i32, String, String) {
    let action = Action::RunCommand {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: Some(cwd.to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(timeout),
        stream: Some(false),
    };
    let r: ActionResult = run_command(&action, cfg, new_shared(100, 60, 4)).await;
    match &r.output {
        Some(ActionOutput::CommandOutput {
            exit_code,
            stdout,
            stderr,
            ..
        }) => (*exit_code, stdout.clone(), stderr.clone()),
        other => panic!("not a command output: {other:?} / {:?}", r.error),
    }
}

/// A token under `$HOME` that no hide list names (F14 attack 1). bwrap alone
/// bind-mounts `/` read-only, so it is readable without Landlock.
#[tokio::test]
async fn a_secret_outside_every_read_root_is_unreadable() {
    if !bwrap_or_skip("a_secret_outside_every_read_root_is_unreadable") {
        return;
    }
    let home = std::fs::canonicalize(dirs::home_dir().expect("home")).expect("canonical home");
    // bwrap mounts a private tmpfs on /tmp: a vault there is absent, not denied, and
    // Landlock goes untested. Measured with HOME=/tmp/home on 2026-10-16.
    assert!(
        !home.starts_with("/tmp"),
        "HOME={} is under /tmp, which the sandbox replaces: run with HOME outside /tmp",
        home.display()
    );
    let vault = tempfile::tempdir_in(&home).unwrap();
    let token = vault.path().join("token");
    std::fs::write(&token, "UNLISTED-TOKEN-CANARY").unwrap();
    let ws = scratch();
    let c = cfg(ws.path());

    let (code, out, err) = sh(&c, &format!("cat {}", token.display()), ws.path(), 20).await;
    assert!(!out.contains("CANARY"), "read the token: {out}");
    assert_ne!(code, 0, "{err}");
    assert!(err.contains("Permission denied"), "{err}");

    // Control: the workspace and system files still work.
    std::fs::write(ws.path().join("mine"), "WS-OK").unwrap();
    let (code, out, err) = sh(
        &c,
        "cat mine && head -c0 /usr/bin/sh && echo SYS-OK && echo W > w2 && cat w2",
        ws.path(),
        20,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("WS-OK") && out.contains("SYS-OK") && out.contains('W'),
        "{out}"
    );
}

/// Audit of 2026-10-30, H1-1. The `cwd` the action names became a Landlock
/// read root on its own: `cat token` with `cwd` set to a directory under
/// `$HOME` that no policy root covers read the token. A cwd outside every
/// read and write root is now refused before anything is spawned, and the
/// refusal names the cwd; a cwd inside a root still works.
#[tokio::test]
async fn a_cwd_outside_every_root_is_refused() {
    if !bwrap_or_skip("a_cwd_outside_every_root_is_refused") {
        return;
    }
    let home = std::fs::canonicalize(dirs::home_dir().expect("home")).expect("canonical home");
    assert!(
        !home.starts_with("/tmp"),
        "HOME={} is under /tmp, which the sandbox replaces: run with HOME outside /tmp",
        home.display()
    );
    let vault = tempfile::tempdir_in(&home).unwrap();
    std::fs::write(vault.path().join("token"), "CWD-TOKEN-CANARY").unwrap();
    let ws = scratch();
    let c = cfg(ws.path());

    let action = Action::RunCommand {
        command: "sh".into(),
        args: vec!["-c".into(), "cat token".into()],
        cwd: Some(vault.path().to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(20),
        stream: Some(false),
    };
    let r: ActionResult = run_command(&action, &c, new_shared(100, 60, 4)).await;
    let shown = format!("{:?} / {:?}", r.output, r.error);
    assert!(
        !shown.contains("CANARY"),
        "read the token through the cwd: {shown}"
    );
    assert!(
        !r.success,
        "a command ran with a cwd outside every root: {shown}"
    );
    let msg = r
        .error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_default();
    assert!(
        msg.contains("cwd") && msg.contains(&vault.path().display().to_string()),
        "the refusal must name the cwd: {msg}"
    );

    // Control: a cwd inside the write root runs, and reads its own files.
    std::fs::write(ws.path().join("mine"), "WS-OK").unwrap();
    let (code, out, err) = sh(&c, "cat mine", ws.path(), 20).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("WS-OK"), "{out}");
}

/// A fork bomb stops at the pid ceiling of this sandbox.
#[tokio::test]
async fn a_fork_bomb_stops_at_the_pid_ceiling() {
    if !bwrap_or_skip("a_fork_bomb_stops_at_the_pid_ceiling") {
        return;
    }
    let ws = scratch();
    let mut c = cfg(ws.path());
    c.isolation.limits.pids = Some(32);
    let script = "n=0; while [ $n -lt 300 ]; do sleep 30 & n=$((n+1)); done; echo spawned=$n";
    let (code, out, err) = sh(&c, script, ws.path(), 20).await;
    assert!(!out.contains("spawned=300"), "300 processes started: {out}");
    assert_ne!(code, 0);
    assert!(err.to_lowercase().contains("fork"), "{err}");
}

/// A 512 MiB string under a 128 MiB data limit fails instead of growing.
#[tokio::test]
async fn a_memory_balloon_stops_at_the_data_ceiling() {
    if !bwrap_or_skip("a_memory_balloon_stops_at_the_data_ceiling") {
        return;
    }
    let ws = scratch();
    let mut c = cfg(ws.path());
    c.isolation.limits.memory_mb = Some(128);
    let script = r#"awk 'BEGIN{s="x"; for(i=0;i<29;i++) s=s s; print length(s)}'"#;
    let (code, out, err) = sh(&c, script, ws.path(), 30).await;
    assert!(!out.contains("536870912"), "the balloon grew: {out}");
    assert_ne!(code, 0, "{err}");
}

/// A busy loop is killed by RLIMIT_CPU long before the command timeout.
#[tokio::test]
async fn a_cpu_loop_is_killed_at_the_cpu_ceiling() {
    if !bwrap_or_skip("a_cpu_loop_is_killed_at_the_cpu_ceiling") {
        return;
    }
    let ws = scratch();
    let mut c = cfg(ws.path());
    c.isolation.limits.cpu_secs = Some(2);
    let t = Instant::now();
    let (code, _, err) = sh(&c, "while :; do :; done", ws.path(), 30).await;
    assert_ne!(code, 0, "{err}");
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "ran {:?}",
        t.elapsed()
    );
}

/// The helper refuses to run when the agent could have replaced it.
#[tokio::test]
async fn a_helper_inside_a_write_root_is_refused() {
    if !bwrap_or_skip("a_helper_inside_a_write_root_is_refused") {
        return;
    }
    let ws = scratch();
    let fake = ws.path().join("x0ne-runtime");
    std::fs::copy(env!("CARGO_BIN_EXE_x0ne-runtime"), &fake).unwrap();
    let mut c = cfg(ws.path());
    c.isolation.confine_helper_override = Some(fake);
    let action = Action::RunCommand {
        command: "true".into(),
        args: vec![],
        cwd: Some(ws.path().to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(10),
        stream: Some(false),
    };
    let r = run_command(&action, &c, new_shared(100, 60, 4)).await;
    assert!(!r.success, "{r:?}");
    let msg = format!("{:?}", r.error);
    assert!(msg.contains("writable root"), "{msg}");
}
