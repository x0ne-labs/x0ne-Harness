//! Commands the agent starts run under bubblewrap by default.
//!
//! Each test runs a real child through `RunCommand`. Where bwrap cannot run
//! (no user namespaces, not installed), the confinement tests print SKIPPED and
//! return; the refusal test runs everywhere.

use std::path::{Path, PathBuf};

use x0ne_runtime::action::Action;
use x0ne_runtime::config::{AppConfig, IsolationNetwork};
use x0ne_runtime::executor::isolation;
use x0ne_runtime::executor::process::run_command;
use x0ne_runtime::handles::new_shared;
use x0ne_runtime::result::{ActionOutput, ActionResult};

fn bwrap_or_skip(test: &str) -> bool {
    match isolation::confinement(&AppConfig::defaults().isolation) {
        Ok(isolation::Confinement::Bwrap(_)) => true,
        other => {
            eprintln!("SKIPPED {test}: bubblewrap unusable here: {other:?}");
            false
        }
    }
}

/// A scratch directory OUTSIDE /tmp: the sandbox's /tmp is private, so a write
/// "outside the workspace" under /tmp would land in a tmpfs and prove nothing.
fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("scratch dir")
}

fn cfg_with_workspace(ws: &Path) -> AppConfig {
    let mut cfg = AppConfig::defaults();
    cfg.policy.allowed_write_paths = vec![ws.to_string_lossy().into_owned()];
    cfg.isolation.confine_helper_override = Some(env!("CARGO_BIN_EXE_x0ne-runtime").into());
    cfg
}

async fn sh(cfg: &AppConfig, script: &str, cwd: &Path) -> ActionResult {
    let action = Action::RunCommand {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: Some(cwd.to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(20),
        stream: Some(false),
    };
    run_command(&action, cfg, new_shared(100, 60, 4)).await
}

fn output(r: &ActionResult) -> (i32, String, String) {
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

#[tokio::test]
async fn an_isolated_command_cannot_read_a_hidden_secret() {
    if !bwrap_or_skip("an_isolated_command_cannot_read_a_hidden_secret") {
        return;
    }
    let ws = scratch();
    let vault = scratch();
    let secret = vault.path().join("id_ed25519");
    std::fs::write(&secret, "PRIVATE-KEY-CANARY").unwrap();
    let mut cfg = cfg_with_workspace(ws.path());
    cfg.isolation.hidden_paths = vec![vault.path().to_string_lossy().into_owned()];
    // A declared read root, so Landlock allows it: only the hide stops it.
    cfg.policy.allowed_read_paths = vec![vault.path().to_string_lossy().into_owned()];

    let r = sh(&cfg, &format!("cat {}", secret.display()), ws.path()).await;
    let (code, stdout, _) = output(&r);
    assert_ne!(code, 0, "the hidden file was readable");
    assert!(!stdout.contains("CANARY"), "{stdout}");

    // Control: the same read with the directory NOT hidden succeeds, so the
    // refusal above is the hide and not a broken sandbox.
    cfg.isolation.hidden_paths.clear();
    let (code, stdout, stderr) =
        output(&sh(&cfg, &format!("cat {}", secret.display()), ws.path()).await);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("CANARY"));
}

/// The workspace CONTAINS the secret directory (the "$HOME as workspace"
/// case): the hide must still win over the read-write bind.
#[tokio::test]
async fn a_secret_inside_the_workspace_stays_hidden() {
    if !bwrap_or_skip("a_secret_inside_the_workspace_stays_hidden") {
        return;
    }
    let ws = scratch();
    let vault = ws.path().join(".ssh");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(vault.join("id_rsa"), "PRIVATE-KEY-CANARY").unwrap();
    let mut cfg = cfg_with_workspace(ws.path());
    cfg.isolation.hidden_paths = vec![vault.to_string_lossy().into_owned()];

    let (code, stdout, _) =
        output(&sh(&cfg, "cat .ssh/id_rsa; echo x > .ssh/planted", ws.path()).await);
    assert_ne!(code, 0);
    assert!(!stdout.contains("CANARY"), "{stdout}");
    assert!(!vault.join("planted").exists(), "wrote through the hide");
}

/// bwrap keeps supplementary groups: an `incus-admin` operator reached the
/// Incus socket from inside the sandbox (measured 2026-09-23). Runs only where
/// the socket exists.
#[tokio::test]
async fn a_daemon_socket_granted_by_group_is_not_reachable() {
    let sock = Path::new("/var/lib/incus/unix.socket");
    if !sock.exists() || !bwrap_or_skip("a_daemon_socket_granted_by_group_is_not_reachable") {
        eprintln!("SKIPPED a_daemon_socket_granted_by_group_is_not_reachable: no incus socket");
        return;
    }
    let ws = scratch();
    let cfg = cfg_with_workspace(ws.path());
    let (code, _, _) = output(&sh(&cfg, "test -e /var/lib/incus/unix.socket", ws.path()).await);
    assert_ne!(code, 0, "the incus socket is visible inside the sandbox");
}

#[test]
fn the_default_list_hides_ssh_and_the_x0ne_home() {
    let home = scratch();
    for d in [".ssh", ".x0ne", ".config/gcloud", ".aws"] {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    std::fs::write(home.path().join(".netrc"), "machine x").unwrap();
    let real = std::fs::canonicalize(home.path()).unwrap();
    let hidden: Vec<PathBuf> = isolation::hidden_paths(Some(home.path()), &[])
        .into_iter()
        .map(|h| h.path)
        .collect();
    for d in [".ssh", ".x0ne", ".config/gcloud", ".aws", ".netrc"] {
        assert!(hidden.contains(&real.join(d)), "{d} not hidden: {hidden:?}");
    }
}

#[tokio::test]
async fn an_isolated_command_cannot_write_outside_the_workspace() {
    if !bwrap_or_skip("an_isolated_command_cannot_write_outside_the_workspace") {
        return;
    }
    let ws = scratch();
    let outside = scratch();
    let cfg = cfg_with_workspace(ws.path());
    let target = outside.path().join("escaped");

    let r = sh(
        &cfg,
        &format!(
            "echo in > {}/inside; echo out > {}",
            ws.path().display(),
            target.display()
        ),
        ws.path(),
    )
    .await;
    let (code, _, stderr) = output(&r);
    assert_ne!(code, 0);
    assert!(stderr.contains("Read-only"), "{stderr}");
    assert!(!target.exists(), "the sandbox wrote outside the workspace");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("inside")).unwrap(),
        "in\n",
        "the workspace itself must stay writable"
    );
}

#[tokio::test]
async fn an_isolated_command_cannot_reach_the_network() {
    if !bwrap_or_skip("an_isolated_command_cannot_reach_the_network") {
        return;
    }
    // A listener on the HOST loopback. The sandbox has its own network
    // namespace, so its 127.0.0.1 is a different interface with nothing on it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ws = scratch();
    let mut cfg = cfg_with_workspace(ws.path());
    let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
    let action = |script: &str| Action::RunCommand {
        command: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: Some(ws.path().to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(20),
        stream: Some(false),
    };

    let r = run_command(&action(&probe), &cfg, new_shared(100, 60, 4)).await;
    let (code, stdout, _) = output(&r);
    assert_ne!(code, 0);
    assert!(
        !stdout.contains("CONNECTED"),
        "the sandbox reached the host"
    );

    // Control: `--isolation-network host` connects, so the probe itself works.
    cfg.isolation.network = IsolationNetwork::Host;
    let r = run_command(&action(&probe), &cfg, new_shared(100, 60, 4)).await;
    let (code, stdout, stderr) = output(&r);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("CONNECTED"));
}

#[tokio::test]
async fn without_isolation_the_command_is_refused_and_never_runs() {
    let ws = scratch();
    let mut cfg = cfg_with_workspace(ws.path());
    cfg.isolation.bwrap_override = Some(PathBuf::from("/nonexistent/x0ne-test/bwrap"));
    let marker = ws.path().join("ran");

    for stream in [false, true] {
        let action = Action::RunCommand {
            command: "touch".into(),
            args: vec![marker.to_string_lossy().into_owned()],
            cwd: None,
            env: Default::default(),
            timeout_secs: Some(5),
            stream: Some(stream),
        };
        let r = run_command(&action, &cfg, new_shared(100, 60, 4)).await;
        assert!(!r.success, "stream={stream}: ran without isolation");
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains("missing_dependency"), "{text}");
        assert!(text.contains("--isolation none"), "{text}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!marker.exists(), "the command reached the host");
}
