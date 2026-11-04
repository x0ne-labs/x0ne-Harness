//! The `x0ne-orchestrator` binary never loads `./.env`.
//!
//! A project `.env` set `X0NE_PLAN_AUTO_APPROVE` for the gateway (measured
//! 2026-09-23). Same rule as `x0ne`: only `~/.x0ne/.env` is read.

use std::path::Path;
use std::process::{Command, Stdio};

const WARNING: &str = "X0NE_PLAN_AUTO_APPROVE=";

fn run_gateway(cwd: &Path, home: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_x0ne-orchestrator"))
        .args(["--mcp", "--downstream-runtime", "none"])
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("X0NE_RUNTIME_UPSTREAM", "127.0.0.1:1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn x0ne-orchestrator");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_project_dotenv_does_not_reach_the_gateway() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".env"),
        "X0NE_PLAN_AUTO_APPROVE=1\nX0NE_LOG=debug\n",
    )
    .unwrap();

    let stderr = run_gateway(project.path(), home.path());
    assert!(
        !stderr.contains(WARNING),
        "project .env reached the gateway:\n{stderr}"
    );
}

#[test]
fn the_user_secrets_file_is_loaded_before_anything_else() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".x0ne")).unwrap();
    std::fs::write(home.path().join(".x0ne/.env"), "X0NE_PLAN_AUTO_APPROVE=1\n").unwrap();

    let stderr = run_gateway(project.path(), home.path());
    assert!(
        stderr.contains(WARNING),
        "~/.x0ne/.env was not loaded:\n{stderr}"
    );
}
