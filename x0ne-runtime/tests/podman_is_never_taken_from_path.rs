//! `RunInSandbox` never runs a `podman` found through PATH.
//!
//! `child_env::harden` keeps PATH, and the four podman spawns of
//! `executor/sandbox.rs` named the program bare: the first `podman` in PATH
//! ran, with the container's arguments. Asserted through the real binary over
//! a real bridge, with a fake `podman` first in the runtime's PATH.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const IMAGE: &str = "localhost/x0ne-no-such-image:0";

fn config() -> String {
    format!(
        r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = ["echo"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []

[sandbox.podman]
allowed_images = ["{IMAGE}"]

[actions]
RunInSandbox = true
RunContainerGroup = true
"#
    )
}

/// Written by a /bin/sh child: a write fd opened in this process would leak
/// into a concurrent fork and the exec would fail with ETXTBSY
/// (rust-lang/rust#114554).
fn write_executable(path: &Path, content: &str) {
    const WRITE: &str = r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#;
    let ok = Command::new("/bin/sh")
        .args(["-c", WRITE, "sh", content])
        .arg(path)
        .status()
        .expect("spawn /bin/sh")
        .success();
    assert!(ok, "could not write {}", path.display());
}

#[test]
fn a_podman_planted_in_path_is_never_launched() {
    let dir = std::env::temp_dir().join(format!("x0ne-podman-path-{}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("fake bin dir");
    let marker = dir.join("fake-podman-ran");
    write_executable(
        &bin.join("podman"),
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho 'podman version 9.9.9'\nexit 0\n",
            marker.display()
        ),
    );
    let cfg = dir.join("x0ne-runtime.toml");
    std::fs::write(&cfg, config()).expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
        .args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn x0ne-runtime");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |json: String| {
        writeln!(stdin, "{json}").expect("write action");
        stdin.flush().expect("flush");
        let mut line = String::new();
        stdout.read_line(&mut line).expect("runtime must answer");
        line
    };

    let single = ask(format!(
        r#"{{"type":"RunInSandbox","image":"{IMAGE}","command":["echo","hi"],"timeout_secs":10,"collect_files":["/tmp/out"]}}"#
    ));
    let group = ask(format!(
        r#"{{"type":"RunContainerGroup","tasks":[{{"id":"a","image":"{IMAGE}","command":["echo","hi"]}}],"timeout_secs":10}}"#
    ));
    let _ = child.kill();
    let _ = child.wait();

    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        ran.is_empty(),
        "the podman found through PATH was launched with: {ran}\n\
         RunInSandbox: {single}RunContainerGroup: {group}"
    );
    let installed = ["/usr/bin/podman", "/usr/local/bin/podman", "/bin/podman"]
        .iter()
        .any(|p| Path::new(p).is_file());
    if !installed {
        for reply in [&single, &group] {
            assert!(
                reply.contains("/usr/bin/podman"),
                "the refusal must name where podman was looked for: {reply}"
            );
        }
    }
}
