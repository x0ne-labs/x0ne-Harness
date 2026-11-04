//! `x0ne-orchestrator --mcp` runs every MCP tool call in its downstream
//! runtime. Under `X0NE_REQUIRE_SANDBOX` it starts only one that runs in a
//! container. The real binary; recorders stand in for the runtime and wrappers.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REFUSED: &str = "X0NE_REQUIRE_SANDBOX is set";

struct Fixture {
    dir: tempfile::TempDir,
}

struct Run {
    code: Option<i32>,
    stderr: String,
}

impl Fixture {
    /// `x0ne-runtime`, `podman` and `ssh` recorders, each writing `<name>-started`.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["bin", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        let fx = Fixture { dir };
        for name in ["x0ne-runtime", "podman", "ssh"] {
            fx.recorder(name);
        }
        fx
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn recorder(&self, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let at = self.path(&format!("bin/{name}"));
        let marker = self.path(&format!("{name}-started"));
        let script = format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
            marker.display()
        );
        std::fs::write(&at, script).expect("write recorder");
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn started(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.path(&format!("{name}-started"))).ok()
    }

    fn gateway(&self, args: &[&str], require: bool) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-orchestrator"));
        cmd.arg("--mcp")
            .args(args)
            .current_dir(self.path("home"))
            .env_clear()
            .env("PATH", self.path("bin"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            // Skips the container IP lookup the artifact router would run.
            .env("X0NE_RUNTIME_UPSTREAM", "127.0.0.1:9")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if require {
            cmd.env("X0NE_REQUIRE_SANDBOX", "1");
        }
        let mut child = cmd.spawn().expect("spawn x0ne-orchestrator");
        let code = wait_or_kill(&mut child, Duration::from_secs(30));
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        Run { code, stderr }
    }
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
    for name in ["x0ne-runtime", "podman", "ssh"] {
        assert_eq!(fx.started(name), None, "{what}: {name} ran");
    }
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

#[test]
fn a_downstream_runtime_on_the_host_is_refused() {
    let fx = Fixture::new();
    let bin = fx.path("bin/x0ne-runtime").display().to_string();
    let run = fx.gateway(&["--downstream-runtime", &bin], true);
    assert_refused(&fx, &run, "--downstream-runtime <host binary>");
}

#[test]
fn a_prefix_that_is_not_a_container_exec_is_refused() {
    for prefix in ["ssh build-host", "   "] {
        let fx = Fixture::new();
        let run = fx.gateway(&["--downstream-prefix", prefix], true);
        assert_refused(&fx, &run, &format!("--downstream-prefix {prefix:?}"));
    }
}

#[test]
fn a_container_exec_prefix_starts_the_downstream_in_the_container() {
    let fx = Fixture::new();
    let run = fx.gateway(
        &["--downstream-prefix", "podman exec -i x0ne-sandbox-podman"],
        true,
    );
    assert!(!run.stderr.contains(REFUSED), "refused:\n{}", run.stderr);
    let started = fx.started("podman").unwrap_or_default();
    assert!(
        started.contains("exec -i x0ne-sandbox-podman x0ne-runtime --mode mcp"),
        "podman was not started: {started:?}\nstderr:\n{}",
        run.stderr
    );
    assert_eq!(
        fx.started("x0ne-runtime"),
        None,
        "the runtime ran on the host"
    );
}

#[test]
fn without_the_variable_the_host_downstream_starts() {
    let fx = Fixture::new();
    let bin = fx.path("bin/x0ne-runtime").display().to_string();
    let run = fx.gateway(&["--downstream-runtime", &bin], false);
    assert!(
        fx.started("x0ne-runtime").is_some(),
        "the downstream never started; stderr:\n{}",
        run.stderr
    );
}

/// No downstream, no runtime: nothing for the requirement to refuse.
#[test]
fn a_gateway_without_a_downstream_is_not_refused() {
    let fx = Fixture::new();
    let run = fx.gateway(&["--downstream-runtime", "none"], true);
    assert!(!run.stderr.contains(REFUSED), "refused:\n{}", run.stderr);
}
