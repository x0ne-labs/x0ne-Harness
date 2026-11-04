//! `create_bridge_for` opens the runtime of every session (REPL, `-p`,
//! ACP, TUI, `/target`). Under `X0NE_REQUIRE_SANDBOX` it starts nothing that
//! would act outside a container. This binary runs as the host (no `X0NE_SANDBOX`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use x0ne_orchestrator::bridge::create_bridge_for;
use x0ne_orchestrator::config::BridgeConfig;

const REFUSED: &str = "X0NE_REQUIRE_SANDBOX is set";

/// The process environment, set once before any test reads it. Fake `podman`
/// and `ssh` first on PATH, each recording its argv in `<name>-started`.
fn env() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        for name in ["podman", "ssh"] {
            recorder(&bin.join(name), &dir.path().join(format!("{name}-started")));
        }
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
        std::env::set_var("HOME", dir.path());
        std::env::set_var("X0NE_REQUIRE_SANDBOX", "1");
        std::env::remove_var("X0NE_SANDBOX");
        dir
    })
    .path()
}

fn recorder(at: &Path, marker: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
        marker.display()
    );
    std::fs::write(at, script).expect("write recorder");
    std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A recorder standing in for `x0ne-runtime`, with its own marker.
struct FakeRuntime {
    dir: tempfile::TempDir,
}

impl FakeRuntime {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        recorder(
            &dir.path().join("x0ne-runtime"),
            &dir.path().join("started"),
        );
        std::fs::write(dir.path().join("policy.toml"), "").expect("policy");
        FakeRuntime { dir }
    }
    fn bin(&self) -> PathBuf {
        self.dir.path().join("x0ne-runtime")
    }
    fn policy(&self) -> String {
        self.dir.path().join("policy.toml").display().to_string()
    }
    fn started(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("started")).ok()
    }
}

fn refusal(result: anyhow::Result<Box<dyn x0ne_orchestrator::bridge::Bridge>>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(e) => Some(format!("{e:#}")),
    }
}

/// The default profile, `/target local`, an empty mode, a runtime policy.
#[tokio::test]
async fn a_runtime_on_the_host_is_refused_before_it_starts() {
    env();
    for (mode, with_policy) in [
        ("stdio", false),
        ("local", false),
        ("", false),
        ("stdio", true),
    ] {
        let rt = FakeRuntime::new();
        let cfg = BridgeConfig {
            mode: mode.into(),
            x0ne_core_bin: rt.bin().display().to_string(),
            runtime_config: with_policy.then(|| rt.policy()),
            ..BridgeConfig::default()
        };
        let err = refusal(create_bridge_for(&cfg).await);
        assert!(
            err.as_deref().is_some_and(|e| e.contains(REFUSED)),
            "mode {mode:?} (policy: {with_policy}) was not refused: {err:?}"
        );
        assert_eq!(rt.started(), None, "mode {mode:?}: the runtime started");
    }
}

/// `--mode http`: the runtime sits behind a socket nothing here can inspect.
#[tokio::test]
async fn an_http_runtime_is_refused() {
    env();
    let cfg = BridgeConfig {
        mode: "http".into(),
        x0ne_core_addr: "127.0.0.1:9".into(),
        ..BridgeConfig::default()
    };
    let err = refusal(create_bridge_for(&cfg).await);
    assert!(
        err.as_deref().is_some_and(|e| e.contains(REFUSED)),
        "http was not refused: {err:?}"
    );
}

/// `/target ssh …`, and any `sandbox_cmd` whose executed program is not
/// `incus`, `podman` or `docker` running `exec`.
#[tokio::test]
async fn an_unverified_sandbox_cmd_is_refused_before_it_starts() {
    let dir = env();
    let rt = FakeRuntime::new();
    let cases = [
        format!("{} exec x0ne-sandbox /opt/x0ne-runtime", rt.bin().display()),
        "ssh build-host incus exec x0ne-sandbox -- /opt/x0ne-runtime".to_string(),
        // The detector skips a leading KEY=VALUE; the bridge would execute it.
        "A=1 podman exec x0ne-sandbox /opt/x0ne-runtime".to_string(),
    ];
    for cmd in cases {
        let cfg = BridgeConfig {
            mode: "sandbox".into(),
            sandbox_cmd: Some(cmd.clone()),
            ..BridgeConfig::default()
        };
        let err = refusal(create_bridge_for(&cfg).await);
        assert!(
            err.as_deref().is_some_and(|e| e.contains(REFUSED)),
            "{cmd:?} was not refused: {err:?}"
        );
    }
    assert_eq!(rt.started(), None, "the runtime started");
    assert!(
        !dir.join("ssh-started").exists(),
        "ssh ran: {:?}",
        std::fs::read_to_string(dir.join("ssh-started"))
    );
}

/// What autowrap writes for Podman: allowed, and the container command runs.
#[tokio::test]
async fn a_container_exec_sandbox_cmd_starts_through_the_container() {
    let dir = env();
    let cfg = BridgeConfig {
        mode: "sandbox".into(),
        sandbox_cmd: Some(
            "podman exec -i --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman /opt/x0ne-runtime \
             --config /root/x0ne-runtime.toml --mode stdio --isolation container"
                .into(),
        ),
        ..BridgeConfig::default()
    };
    let mut bridge = create_bridge_for(&cfg)
        .await
        .expect("a podman exec bridge is allowed");
    // The recorder exits at once: the health check reads EOF after it wrote.
    let _ = bridge.health_check().await;
    let started = std::fs::read_to_string(dir.join("podman-started")).unwrap_or_default();
    assert!(
        started.contains("exec -i --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman"),
        "podman was not started: {started:?}"
    );
}
