//! When the runtime refuses to start (no policy file, invalid policy), `x0ne`
//! prints that refusal and nothing that calls it a crash.
//!
//! A stand-in `x0ne-runtime` on PATH writes the refusal the real one writes
//! (`tests/no_policy_file_no_start.rs` in x0ne-runtime pins that text).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fake_runtime(dir: &Path, stderr: &str) {
    let p = dir.join("x0ne-runtime");
    std::fs::write(
        &p,
        format!("#!/bin/sh\ncat >&2 <<'X'\n{stderr}\nX\nexit 1\n"),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `x0ne -p hello` with an isolated HOME; returns stdout + stderr.
fn x0ne_p(home: &Path, bin: &Path) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne"))
        .args(["-p", "hello"])
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("ANTHROPIC_API_KEY", "sk-ant-test-not-a-key")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("x0ne -p did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "x0ne -p succeeded without a runtime");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_runtime_refusal_is_printed_alone() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    fake_runtime(
        bin.path(),
        "INFO x0ne-runtime starting\nError: x0ne-runtime: refusing to start: no policy file. REFUSAL-CANARY\n\nWrite it at:\n  /somewhere/x0ne-runtime.toml",
    );
    let out = x0ne_p(home.path(), bin.path());
    assert!(
        out.contains("refusing to start: no policy file. REFUSAL-CANARY"),
        "{out}"
    );
    assert!(out.contains("Write it at:"), "{out}");
    for noise in ["crashed", "empty response", "health check failed", "panic"] {
        assert!(!out.contains(noise), "'{noise}' printed:\n{out}");
    }
    assert_eq!(out.matches("REFUSAL-CANARY").count(), 1, "{out}");
}

/// The log is appended across runs: a refusal from an earlier run is not
/// this run's cause.
#[test]
fn an_old_refusal_in_the_log_is_not_reported() {
    let home = tempfile::tempdir().unwrap();
    let logs = home.path().join(".x0ne/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("core.log"),
        "Error: x0ne-runtime: refusing to start: OLD-CANARY\n",
    )
    .unwrap();
    let bin = tempfile::tempdir().unwrap();
    fake_runtime(
        bin.path(),
        "thread 'main' panicked at src/main.rs:1:1: NEW-CANARY",
    );
    let out = x0ne_p(home.path(), bin.path());
    assert!(!out.contains("OLD-CANARY"), "{out}");
    assert!(out.contains("NEW-CANARY"), "{out}");
}
