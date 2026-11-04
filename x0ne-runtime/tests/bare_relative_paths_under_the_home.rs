//! A bare filename must work inside a home the policy grants in full.
//!
//! Field report, 2026-08-15, run 9 of the H100 pipeline — the owner's first
//! real session inside the demo enclave. `ReadFile path="X0NE.md"` came back
//! `Path 'X0NE.md' is not under any allowed path prefix.` while
//! `allowed_read_paths` was the VM's entire home and the shell's cwd was that
//! same home. His conclusion, and it was the reasonable one: "il peut rien
//! lire".
//!
//! Mechanism: `Path::new("X0NE.md").parent()` is `Some("")` and
//! `fs::canonicalize("")` is always `ENOENT`. Every resolver asks the parent
//! where a file lives — `PolicyEngine::resolve_path` to normalize before the
//! root check, `confine::resolve_read` to tell missing from forbidden,
//! `confine::resolve_write` to place a new file — and all three gave up on the
//! empty parent. The path stayed relative, and a relative path is never
//! component-wise inside an absolute root, so a MISSING file was reported as a
//! POLICY DENIAL and a new file could not be created at all. `./X0NE.md` and
//! `sub/X0NE.md` were handled correctly throughout; only the bare name broke.
//!
//! Spawning the real binary is the point. The defect lives in the agreement
//! between the process's cwd and the configured roots, and `cargo test` runs
//! the test process in the crate directory — an in-process test would either
//! prove nothing or have to mutate global cwd out from under its neighbours.
//! `Command::current_dir` gives each case its own home, exactly as the ssh
//! session gives the enclave's x0ne `$HOME`.
//!
//! A failure here means the demo enclave is mute again: the agent is told it is
//! forbidden where it is merely mistaken, and cannot create a file in a
//! directory the operator granted it whole.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The enclave's shape: reads and writes over one root, writing enabled.
/// `@HOME@` is replaced with the test's own home, the way the VM's `sed`
/// replaces `@X0NE_HOME@` with `$HOME`.
const CONFIG: &str = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "deny"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = []
allowed_read_paths = ["@HOME@"]
allowed_write_paths = ["@HOME@"]
deny_path_patterns = []
readable_env_vars = []

[actions]
WriteFile = true
"#;

struct Runtime {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Runtime {
    /// Spawn the real binary with `home` as BOTH its cwd and its only root.
    fn spawn(home: &Path) -> Runtime {
        let cfg_path = home.join("x0ne-runtime-test.toml");
        std::fs::write(&cfg_path, CONFIG.replace("@HOME@", &home.to_string_lossy()))
            .expect("write config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args(["--config", cfg_path.to_str().unwrap(), "--mode", "stdio"])
            .current_dir(home)
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

/// The exact call from run 9, on a file that IS there.
#[test]
fn a_bare_filename_in_the_home_is_read() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("X0NE.md"), "hello from the enclave\n").unwrap();
    let mut rt = Runtime::spawn(home.path());

    let reply = rt.ask(r#"{"type":"ReadFile","path":"X0NE.md"}"#);
    assert!(
        reply.contains("hello from the enclave"),
        "a bare name under the granted root must be readable: {reply}"
    );
}

/// The regression itself: absent is not forbidden.
///
/// The two verdicts carry opposite instructions for an agent — `not_found` says
/// "check the name and try again", `policy_denied` says "do not retry, stop and
/// ask the user" (`result.rs`). Collapsing them cost the owner a whole session.
#[test]
fn a_missing_bare_filename_reports_not_found_not_a_policy_denial() {
    let home = tempfile::tempdir().unwrap();
    let mut rt = Runtime::spawn(home.path());

    let reply = rt.ask(r#"{"type":"ReadFile","path":"X0NE.md"}"#);
    assert!(
        reply.contains("not_found"),
        "a missing file is missing, not forbidden: {reply}"
    );
    assert!(
        !reply.contains("not under any allowed path prefix"),
        "the exact message from the 2026-08-15 field report: {reply}"
    );
}

/// Home-wide writes have to include the obvious one: a new file, named plainly.
///
/// This is what "writes across the whole home" means to an agent that has just
/// been told its cwd. Before the fix it failed with "parent directory
/// unresolvable" — a message that describes the resolver's difficulty, not the
/// operator's policy.
#[test]
fn a_bare_filename_can_be_created_in_the_home() {
    let home = tempfile::tempdir().unwrap();
    let mut rt = Runtime::spawn(home.path());

    let reply = rt.ask(r#"{"type":"WriteFile","path":"notes.md","content":"written\n"}"#);
    assert!(
        reply.contains("\"success\":true"),
        "create a file named with a bare name, under the granted root: {reply}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("notes.md")).unwrap(),
        "written\n",
        "and it must land in the home, nowhere else"
    );
}

/// SECURITY: the barrier is the EXECUTOR, and this pins it where the gate is
/// known to be blind.
///
/// `PolicyEngine::resolve_path`'s third branch — neither target nor parent
/// resolves — strips `..` textually, and the first version of its comment cited
/// that strip as the reason the cwd anchor was safe. It is not: `is_within_roots`
/// is a component-PREFIX test that returns true as soon as the root's components
/// run out, so a `..` after them passes with or without the strip, and deleting
/// the filter changed no verdict anywhere in the crate (mutation run,
/// 2026-08-15). What refuses this write is `confine::resolve_write`, which
/// cannot canonicalize a parent that does not exist.
///
/// A failure here means the one real barrier for this shape of path is gone —
/// and no gate-side test would notice, which is exactly why this one exists.
#[test]
fn an_unresolvable_parent_outside_the_home_is_refused_by_the_executor() {
    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut rt = Runtime::spawn(&home);

    // `../nowhere/` does not exist, so neither the target nor its parent
    // canonicalizes: the gate takes the branch that only tidies the string.
    let reply = rt.ask(r#"{"type":"WriteFile","path":"../nowhere/planted.md","content":"x"}"#);
    assert!(
        reply.contains("\"success\":false"),
        "an unresolved parent outside the home must be refused: {reply}"
    );
    assert!(
        !parent.path().join("nowhere").exists(),
        "and nothing must have been created outside the root"
    );
}

/// SECURITY: the cwd anchor is not a way out.
///
/// A bare name resolves against the cwd; `..` must not turn that into a
/// staircase. If this ever passes a write to the parent directory, the
/// confinement is decorative.
#[test]
fn a_relative_climb_out_of_the_home_is_still_refused() {
    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut rt = Runtime::spawn(&home);

    let reply = rt.ask(r#"{"type":"WriteFile","path":"../escaped.md","content":"x"}"#);
    assert!(
        reply.contains("\"success\":false"),
        "`..` from the home must be refused: {reply}"
    );
    assert!(
        !parent.path().join("escaped.md").exists(),
        "and nothing must have been written outside the root"
    );
}
