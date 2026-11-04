//! With `desktop.allowed_apps` set, an agent command must not reach the
//! session: no compositor IPC, no Wayland or X11 socket, no uinput, whatever
//! the program (`sh -c`, an interpreter, an absolute path, a symlink, an
//! environment variable naming the socket). The barrier is what the child can
//! see, not a list of tool names.
//!
//! One test function: it sets process-wide environment variables, which other
//! threads must not read concurrently. Without bwrap it skips, unless
//! `X0NE_REQUIRE_BWRAP=1`.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use x0ne_runtime::action::Action;
use x0ne_runtime::config::{AppConfig, IsolationMode, IsolationNetwork};
use x0ne_runtime::executor::isolation;
use x0ne_runtime::executor::process::run_command;
use x0ne_runtime::handles::new_shared;
use x0ne_runtime::result::{ActionOutput, ActionResult, ErrorKind};

fn bwrap_or_skip() -> bool {
    match isolation::confinement(&AppConfig::defaults().isolation) {
        Ok(isolation::Confinement::Bwrap(_)) => true,
        other => {
            assert!(
                std::env::var_os("X0NE_REQUIRE_BWRAP").is_none(),
                "bwrap required but unusable: {other:?}"
            );
            eprintln!("SKIPPED desktop_out_of_reach: bubblewrap unusable: {other:?}");
            false
        }
    }
}

fn locked(ws: &Path) -> AppConfig {
    let mut cfg = AppConfig::defaults();
    cfg.policy.allowed_write_paths = vec![ws.to_string_lossy().into_owned()];
    cfg.isolation.confine_helper_override = Some(env!("CARGO_BIN_EXE_x0ne-runtime").into());
    cfg.desktop.enabled = true;
    cfg.desktop.allowed_apps = vec!["firefox".into()];
    cfg
}

async fn run(cfg: &AppConfig, command: &str, args: &[&str], cwd: &Path) -> ActionResult {
    let action = Action::RunCommand {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        env: Default::default(),
        timeout_secs: Some(20),
        stream: Some(false),
    };
    run_command(&action, cfg, new_shared(100, 60, 4)).await
}

/// Exit code and stdout, or `None` when the command was refused before running.
fn outcome(r: &ActionResult) -> Option<(i32, String)> {
    match &r.output {
        Some(ActionOutput::CommandOutput {
            exit_code, stdout, ..
        }) => Some((*exit_code, stdout.clone())),
        _ => None,
    }
}

/// Connects to a unix socket path (or `@name`, abstract) and prints REACHED.
const PROBE: &str = "import socket,sys\n\
a=sys.argv[1]\n\
a='\\0'+a[1:] if a.startswith('@') else a\n\
s=socket.socket(socket.AF_UNIX)\n\
s.connect(a)\n\
print('REACHED')";

fn reached(r: &ActionResult) -> bool {
    outcome(r).is_some_and(|(code, out)| code == 0 && out.contains("REACHED"))
}

fn listen(p: &Path) -> UnixListener {
    UnixListener::bind(p).unwrap_or_else(|e| panic!("bind {}: {e}", p.display()))
}

#[tokio::test]
async fn an_agent_command_cannot_reach_the_desktop_session() {
    if !bwrap_or_skip() {
        return;
    }
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ws = base.path().join("ws");
    let run_dir = base.path().join("run");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    let ws = ws.canonicalize().unwrap();
    let run_dir = run_dir.canonicalize().unwrap();
    let cfg = locked(&ws);

    // Real X11 session first, with the real environment: read-only probe.
    if std::env::var_os("DISPLAY").is_some() && Path::new("/usr/bin/xdotool").is_file() {
        let host = std::process::Command::new("/usr/bin/xdotool")
            .arg("getmouselocation")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if host {
            let mut net = cfg.clone();
            net.isolation.network = IsolationNetwork::Host;
            for c in [&cfg, &net] {
                let r = run(c, "xdotool", &["getmouselocation"], &ws).await;
                assert!(
                    outcome(&r).is_none_or(|(code, _)| code != 0),
                    "xdotool reached the X server (network {:?}): {r:?}",
                    c.isolation.network
                );
            }
        } else {
            eprintln!("xdotool cannot reach the display from the host: X11 probe skipped");
        }
    }

    // Stand-ins for the session sockets, outside every policy root.
    let sway = run_dir.join("sway-ipc.sock");
    let i3 = run_dir.join("i3-ipc.sock");
    let wayland = run_dir.join("wayland-9");
    let ydotool = run_dir.join("ydotool.sock");
    let xauth = run_dir.join("Xauthority");
    let _l: Vec<UnixListener> = [&sway, &i3, &wayland, &ydotool]
        .iter()
        .map(|p| listen(p))
        .collect();
    std::fs::write(&xauth, "XAUTH-CANARY").unwrap();
    // Control: the probe reaches a socket in the workspace.
    let mine = ws.join("mine.sock");
    let _m = listen(&mine);
    let abstract_name = format!("x0ne-desktop-test-{}", std::process::id());
    let _a = {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&abstract_name).unwrap();
        UnixListener::bind_addr(&addr).unwrap()
    };

    std::env::set_var("SWAYSOCK", &sway);
    std::env::set_var("I3SOCK", &i3);
    std::env::set_var("WAYLAND_DISPLAY", &wayland);
    std::env::set_var("YDOTOOL_SOCKET", &ydotool);
    std::env::set_var("XAUTHORITY", &xauth);

    let r = run(
        &cfg,
        "python3",
        &["-c", PROBE, &mine.to_string_lossy()],
        &ws,
    )
    .await;
    assert!(reached(&r), "control: workspace socket unreachable: {r:?}");

    let link = ws.join("link.sock");
    std::os::unix::fs::symlink(&sway, &link).unwrap();
    let targets: Vec<(&str, PathBuf)> = vec![
        ("SWAYSOCK", sway.clone()),
        ("I3SOCK", i3.clone()),
        ("WAYLAND_DISPLAY", wayland.clone()),
        ("YDOTOOL_SOCKET", ydotool.clone()),
        ("symlink in the workspace", link.clone()),
    ];
    for (what, p) in &targets {
        let p = p.to_string_lossy().into_owned();
        let shell = format!("python3 -c \"$1\" {p}");
        let attempts: Vec<(&str, Vec<&str>)> = vec![
            ("python3", vec!["-c", PROBE, &p]),
            ("/usr/bin/python3", vec!["-c", PROBE, &p]),
            ("sh", vec!["-c", &shell, "sh", PROBE]),
        ];
        for (prog, args) in attempts {
            let r = run(&cfg, prog, &args, &ws).await;
            assert!(!reached(&r), "{prog} reached {what} ({p}): {r:?}");
        }
    }
    // The variable itself, as a tool reads it.
    let r = run(
        &cfg,
        "sh",
        &["-c", "python3 -c \"$1\" \"$SWAYSOCK\"", "sh", PROBE],
        &ws,
    )
    .await;
    assert!(!reached(&r), "reached $SWAYSOCK: {r:?}");

    let r = run(&cfg, "cat", &[&xauth.to_string_lossy()], &ws).await;
    assert!(
        outcome(&r).is_none_or(|(_, out)| !out.contains("XAUTH-CANARY")),
        "read the X11 cookie: {r:?}"
    );

    let r = run(
        &cfg,
        "sh",
        &[
            "-c",
            "ls /dev/uinput /dev/input 2>/dev/null && echo SEEN; exit 0",
        ],
        &ws,
    )
    .await;
    assert!(
        outcome(&r).is_some_and(|(_, out)| !out.contains("SEEN")),
        "an input device is visible: {r:?}"
    );

    // Abstract sockets (X11 listens on one) with the host network shared.
    let mut net = cfg.clone();
    net.isolation.network = IsolationNetwork::Host;
    let at = format!("@{abstract_name}");
    let r = run(&net, "python3", &["-c", PROBE, &at], &ws).await;
    assert!(!reached(&r), "reached an abstract socket: {r:?}");

    // A session socket inside a write root could be renamed by a file action
    // and reached under another name: refused while the list is set.
    let mut inside = cfg.clone();
    inside
        .policy
        .allowed_write_paths
        .push(run_dir.to_string_lossy().into_owned());
    let r = run(&inside, "true", &[], &ws).await;
    assert!(!r.success, "ran with SWAYSOCK inside a write root: {r:?}");

    // No confinement, no desktop lock: refused, not run on the host.
    for mode in [IsolationMode::None, IsolationMode::Container] {
        let mut host = cfg.clone();
        host.isolation.mode = mode;
        if mode == IsolationMode::Container {
            let fake = base.path().join("fakeroot");
            std::fs::create_dir_all(fake.join("run")).unwrap();
            std::fs::write(fake.join("run/.containerenv"), "").unwrap();
            host.isolation.container_probe_root = Some(fake);
        }
        let r = run(
            &host,
            "python3",
            &["-c", PROBE, &sway.to_string_lossy()],
            &ws,
        )
        .await;
        assert!(!reached(&r), "{mode:?} reached SWAYSOCK: {r:?}");
        assert!(
            r.error
                .as_ref()
                .is_some_and(|e| e.kind == ErrorKind::MissingDependency
                    && e.message.contains("allowed_apps")),
            "{mode:?}: not a named refusal: {r:?}"
        );
        // Control: without the list, the opt-out is the human's choice.
        host.desktop.allowed_apps.clear();
        let r = run(&host, "true", &[], &ws).await;
        assert!(r.success, "{mode:?} without allowed_apps: {r:?}");
    }

    // Control: an ordinary command still runs under the lock.
    let r = run(&cfg, "sh", &["-c", "echo ok > out && cat out"], &ws).await;
    assert!(
        outcome(&r).is_some_and(|(c, o)| c == 0 && o.contains("ok")),
        "{r:?}"
    );
}
