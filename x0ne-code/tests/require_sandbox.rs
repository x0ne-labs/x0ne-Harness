//! Under `X0NE_REQUIRE_SANDBOX`, every way into a session is refused
//! before the runtime starts when its actions would land outside a container.
//! The real binary; a recorder stands in for the runtime; no model is reached.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x0ne_orchestrator::sandbox_mode::CONTAINER_MARKERS;

const REFUSED: &str = "X0NE_REQUIRE_SANDBOX is set";
const REQUIRE: (&str, &str) = ("X0NE_REQUIRE_SANDBOX", "1");

struct Fixture {
    dir: tempfile::TempDir,
}

struct Run {
    /// `None`: killed at the deadline, or ended by a signal.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["home", "work", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        let fx = Fixture { dir };
        fx.recorder("fake-runtime");
        fx
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// A program that appends its argv to `runtime-started`, then exits.
    fn recorder(&self, rel: &str) {
        use std::os::unix::fs::PermissionsExt;
        let at = self.path(rel);
        let marker = self.path("runtime-started");
        let script = format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
            marker.display()
        );
        std::fs::write(&at, script).expect("write recorder");
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn runtime_started(&self) -> Option<String> {
        std::fs::read_to_string(self.path("runtime-started")).ok()
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)], stdin: &[String]) -> Run {
        let home = self.path("home");
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne"))
            .args(args)
            .current_dir(self.path("work"))
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("X0NE_RUNTIME_BIN", self.path("fake-runtime"))
            .env("X0NE_LLM_PROVIDER", "openai-compat")
            .env("X0NE_MODEL", "require-sandbox-test")
            .env("X0NE_OPENAI_COMPAT_BASE_URL", "http://127.0.0.1:9/v1")
            .env("X0NE_CORE_ADDR", "127.0.0.1:9")
            // clap reads this bool as true|false: "1" is a usage error (measured).
            .env("X0NE_NO_INTRO", "true")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn x0ne");
        let stdout = drain(child.stdout.take().expect("stdout"));
        let stderr = drain(child.stderr.take().expect("stderr"));
        let mut input = child.stdin.take();
        if let Some(w) = input.as_mut() {
            for line in stdin {
                let _ = writeln!(w, "{line}");
            }
            let _ = w.flush();
        }
        // Stdin stays open until x0ne exits or its runtime starts: ACP answers
        // `session/new` asynchronously, and EOF would end the server first.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut code = None;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("try_wait") {
                code = Some(status.code());
                break;
            }
            if self.runtime_started().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(input);
        let code = code.unwrap_or_else(|| wait_or_kill(&mut child, Duration::from_secs(30)));
        Run {
            code,
            stdout: stdout.join().expect("stdout reader"),
            stderr: stderr.join().expect("stderr reader"),
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    })
}

fn wait_or_kill(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn assert_refused(fx: &Fixture, run: &Run, what: &str) {
    assert!(
        fx.runtime_started().is_none(),
        "{what}: the runtime started outside a sandbox: {:?}\nstderr:\n{}",
        fx.runtime_started(),
        run.stderr
    );
    assert!(
        run.stderr.contains(REFUSED),
        "{what}: no refusal on stderr:\n{}",
        run.stderr
    );
    assert_eq!(
        run.code,
        Some(1),
        "{what}: exit code; stderr:\n{}",
        run.stderr
    );
}

fn assert_allowed(fx: &Fixture, run: &Run, what: &str) {
    assert!(
        !run.stderr.contains(REFUSED),
        "{what}: refused:\n{}",
        run.stderr
    );
    assert!(
        fx.runtime_started().is_some(),
        "{what}: the runtime never started; stderr:\n{}",
        run.stderr
    );
}

/// The audit case (2026-10-06): plain `x0ne`, no container runtime installed.
#[test]
fn plain_x0ne_without_a_container_runtime_is_refused() {
    let fx = Fixture::new();
    let run = fx.run(&[], &[REQUIRE], &[]);
    assert_refused(&fx, &run, "x0ne");
}

/// `--sandbox` with no runtime: refused for any value but "" and "0".
#[test]
fn a_missing_container_runtime_is_refused_whatever_the_value() {
    for value in ["true", "yes", "1"] {
        let fx = Fixture::new();
        let run = fx.run(
            &["--sandbox", "-p", "hi"],
            &[("X0NE_REQUIRE_SANDBOX", value)],
            &[],
        );
        assert_refused(
            &fx,
            &run,
            &format!("X0NE_REQUIRE_SANDBOX={value} x0ne --sandbox -p"),
        );
    }
}

/// The host profile, chosen on the command line or in the environment.
#[test]
fn the_host_profile_is_refused() {
    let cases: [(&[&str], Vec<(&str, &str)>, &str); 2] = [
        (
            &["--mode", "stdio", "-p", "hi"],
            vec![REQUIRE],
            "--mode stdio",
        ),
        (
            &["-p", "hi"],
            vec![REQUIRE, ("X0NE_BRIDGE_MODE", "local")],
            "X0NE_BRIDGE_MODE=local",
        ),
    ];
    for (args, env, what) in cases {
        let fx = Fixture::new();
        let run = fx.run(args, &env, &[]);
        assert_refused(&fx, &run, what);
    }
}

/// Options that take the sandbox away, or claim one that is not there.
#[test]
fn an_option_that_disables_the_sandbox_is_refused() {
    let cases: [&[&str]; 3] = [
        &["--sandbox", "--no-sandbox", "-p", "hi"],
        &["--isolation", "none", "-p", "hi"],
        &["--isolation", "container", "-p", "hi"],
    ];
    for args in cases {
        let fx = Fixture::new();
        let run = fx.run(args, &[REQUIRE], &[]);
        assert_refused(&fx, &run, &args.join(" "));
    }
}

/// `-p`: a startup error (exit 1), and nothing a caller could read as an answer.
#[test]
fn the_one_shot_mode_is_refused() {
    let fx = Fixture::new();
    let run = fx.run(&["-p", "hi"], &[REQUIRE], &[]);
    assert_refused(&fx, &run, "x0ne -p");
    assert!(run.stdout.trim().is_empty(), "stdout: {}", run.stdout);
}

/// `--acp`: refused before the protocol starts; stdout carries no frame.
#[test]
fn acp_is_refused_before_the_protocol_starts() {
    let fx = Fixture::new();
    let cwd = fx.path("work").canonicalize().expect("cwd");
    let lines = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1}})
        .to_string(),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": cwd, "mcpServers": []}})
        .to_string(),
    ];
    let run = fx.run(&["--acp"], &[REQUIRE], &lines);
    assert_refused(&fx, &run, "x0ne --acp");
    assert!(
        run.stdout.is_empty(),
        "protocol bytes on stdout: {}",
        run.stdout
    );
}

/// `--serve` drives its tools through an HTTP runtime, which nothing verifies.
#[test]
fn serve_is_refused() {
    let fx = Fixture::new();
    let run = fx.run(&["--serve", "--bind", "127.0.0.1:0"], &[REQUIRE], &[]);
    assert_refused(&fx, &run, "x0ne --serve");
}

/// `--self-test` (debug builds): every action on a host runtime, policy open.
#[cfg(debug_assertions)]
#[test]
fn the_self_test_is_refused() {
    let fx = Fixture::new();
    let run = fx.run(&["--self-test"], &[REQUIRE], &[]);
    assert_refused(&fx, &run, "x0ne --self-test");
}

/// Without the variable nothing changes: the host runtime starts.
#[test]
fn without_the_variable_the_host_runtime_starts() {
    let fx = Fixture::new();
    let run = fx.run(&["-p", "hi"], &[], &[]);
    assert_allowed(&fx, &run, "x0ne -p, variable unset");
}

/// A `sandbox_cmd` that runs `podman exec` passes, and is what gets started.
#[test]
fn a_container_exec_bridge_is_allowed() {
    let fx = Fixture::new();
    fx.recorder("bin/podman");
    let run = fx.run(
        &["--mode", "sandbox", "-p", "hi"],
        &[
            REQUIRE,
            (
                "X0NE_SANDBOX_CMD",
                "podman exec -i x0ne-sandbox-podman /opt/x0ne-runtime --mode stdio",
            ),
        ],
        &[],
    );
    assert_allowed(&fx, &run, "--mode sandbox, podman exec");
    let started = fx.runtime_started().unwrap_or_default();
    assert!(
        started.contains("exec -i x0ne-sandbox-podman /opt/x0ne-runtime"),
        "{started}"
    );
}

/// `X0NE_SANDBOX` counts only where a container marker confirms it.
#[test]
fn a_declared_sandbox_counts_only_inside_a_container() {
    let in_container = CONTAINER_MARKERS.iter().any(|m| Path::new(m).exists());
    let fx = Fixture::new();
    let run = fx.run(&["-p", "hi"], &[REQUIRE, ("X0NE_SANDBOX", "podman")], &[]);
    if in_container {
        assert_allowed(&fx, &run, "X0NE_SANDBOX=podman, in a container");
    } else {
        assert_refused(&fx, &run, "X0NE_SANDBOX=podman, on the host");
    }
}
