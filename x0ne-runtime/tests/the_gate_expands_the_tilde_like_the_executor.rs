//! The gate and the executor must read `~` the same way.
//!
//! The executor expands a leading `~` to `$HOME` before it confines a path
//! (`confine::resolve_read`); the gate did not (`PolicyEngine::resolve_path`).
//! It anchored `~/private/notes.txt` on the cwd, matched the deny patterns
//! against `<cwd>/~/private/notes.txt`, and an operator's pattern written as
//! an absolute path never saw the file the executor then opened. Measured
//! 2026-11-02 with the cwd inside the read root: the file came back.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const CANARY: &str = "CANARY-TILDE";

struct Runtime {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Runtime {
    /// `home` is the process's HOME, its cwd and its only read root; the
    /// operator denies `<home>/private/**` by its absolute path.
    fn spawn(home: &Path) -> Runtime {
        let h = home.display();
        let cfg = home.join("x0ne-runtime-test.toml");
        std::fs::write(
            &cfg,
            format!(
                "[runtime]\nmode = \"stdio\"\n\n[policy]\nmode = \"auto\"\n\
                 default_effect = \"deny\"\nallowed_commands = []\n\
                 allowed_read_paths = [\"{h}\"]\nallowed_write_paths = []\n\
                 deny_path_patterns = [\"{h}/private/**\"]\n\n\
                 [actions]\nReadFile = true\nSearchCode = true\n"
            ),
        )
        .expect("write config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Runtime {
            child,
            stdin,
            stdout,
        }
    }

    fn ask(&mut self, json: &str) -> String {
        writeln!(self.stdin, "{json}").expect("write action");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("runtime must answer");
        line
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(home.path()).unwrap();
    std::fs::create_dir_all(canonical.join("private")).unwrap();
    std::fs::write(
        canonical.join("private/notes.txt"),
        format!("{CANARY} private\n"),
    )
    .unwrap();
    std::fs::write(canonical.join("README.md"), format!("{CANARY} public\n")).unwrap();
    home
}

#[test]
fn a_tilde_path_meets_the_operators_absolute_deny_pattern() {
    let home = home();
    let canonical = std::fs::canonicalize(home.path()).unwrap();
    let mut rt = Runtime::spawn(&canonical);

    let absolute = format!(
        r#"{{"type":"ReadFile","path":"{}/private/notes.txt"}}"#,
        canonical.display()
    );
    let reply = rt.ask(&absolute);
    assert!(!reply.contains(CANARY), "control, absolute path: {reply}");

    let reply = rt.ask(r#"{"type":"ReadFile","path":"~/private/notes.txt"}"#);
    assert!(
        !reply.contains(CANARY),
        "~/private/notes.txt passed the deny pattern: {reply}"
    );
    assert!(reply.contains("policy_denied"), "{reply}");
}

#[test]
fn a_search_rooted_at_the_tilde_is_judged_on_the_home() {
    let home = home();
    let canonical = std::fs::canonicalize(home.path()).unwrap();
    let mut rt = Runtime::spawn(&canonical);

    let reply = rt.ask(&format!(
        r#"{{"type":"SearchCode","root":"~","pattern":"{CANARY}"}}"#
    ));
    assert!(reply.contains(&format!("{CANARY} public")), "{reply}");
    assert!(!reply.contains(&format!("{CANARY} private")), "{reply}");
}
