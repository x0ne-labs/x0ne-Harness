//! The `env` map an action carries cannot decide WHICH program runs.
//!
//! Audit of 2026-08-15, finding n°5. `child_env::harden` clears the operator's
//! environment and rebuilds it from an allowlist — PATH included
//! (`child_env.rs:60`). The action's own `env` map is then applied on top
//! (`executor/process.rs:228` blocking, `:309` streaming, `artifact.rs:346`
//! for ArtifactServe) with **no filter on the key**. So the model chose PATH,
//! and PATH chooses the binary:
//!
//! ```text
//! {"type":"RunCommand","command":"cat","args":["f"],"env":{"PATH":"/tmp/evil"}}
//!   → PWNED-fake-cat        (measured 2026-08-15, before this fix)
//! ```
//!
//! Two gates were defeated at once by that one field. The human gate is waived
//! for reads by name — `is_read_only_command(command, args)`
//! (`x0ne-orchestrator/src/agent/readonly_cmd.rs:223`) never receives `env`, so
//! it was ruling on `cat` while a field it cannot see picked the executable.
//! And the runtime's `allowed_commands` list matches the same string, so
//! naming `cat` passed it too. `LD_PRELOAD` is the same defeat without even
//! needing a PATH entry: it only requires the loader to read a file, and `/tmp`
//! is inside `allowed_write_paths` on the shipped profiles.
//!
//! A failure here means the model can run any code it can write to disk while
//! every log line, every allowlist and every approval prompt says `cat`.
//!
//! Asserted through the real binary over a real bridge: the property is about
//! which executable image a spawned child ends up with, and only a real spawn
//! answers that.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// `cat` and `env` are allowed by name — the point is that naming them is not
/// the same as running them.
const CONFIG: &str = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = ["cat", "env", "printenv", "echo"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []

[actions]
RunCommand = true
"#;

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    fn spawn(tag: &str) -> Bridge {
        let dir =
            std::env::temp_dir().join(format!("x0ne-envkey-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cfg = dir.join("x0ne-runtime.toml");
        std::fs::write(&cfg, CONFIG).expect("write config");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        cmd.args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"]);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Bridge {
            child,
            stdin,
            stdout,
        }
    }

    fn run(&mut self, json: &str) -> String {
        writeln!(self.stdin, "{json}").expect("write action");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("runtime must answer");
        line
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A directory holding a `cat` that is not cat.
fn plant_a_fake_cat(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("x0ne-fakebin-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).expect("fake bin dir");
    let fake = dir.join("cat");
    std::fs::write(&fake, "#!/bin/sh\necho PWNED-fake-cat\n").expect("write fake cat");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod +x");
    dir
}

/// The measurement of 2026-08-15, as a property: a PATH the model supplied does
/// not get to pick the executable behind a name the gate already approved.
#[test]
fn a_model_supplied_path_cannot_choose_the_binary() {
    let dir = plant_a_fake_cat("blocking");
    let mut bridge = Bridge::spawn("path");
    let reply = bridge.run(&format!(
        r#"{{"type":"RunCommand","command":"cat","args":["/etc/hostname"],"env":{{"PATH":"{}"}},"timeout_secs":10}}"#,
        dir.display()
    ));

    assert!(
        !reply.contains("PWNED-fake-cat"),
        "the action's `env` map chose the binary: the human approved `cat` and \
         a program of the model's choosing ran instead. Reply: {reply}"
    );
    assert!(
        reply.contains("PATH"),
        "the refusal must NAME the key the model tried to set — a silent skip \
         leaves the operator with no way to know it was attempted. Reply: {reply}"
    );
    assert!(
        reply.contains("\"success\":false"),
        "setting PATH must be a refusal, not a dropped field. Reply: {reply}"
    );
}

/// Streaming mode is a second, separately written spawn site (`process.rs:309`).
/// The repository's recurring defect is the site written afterwards that nobody
/// applied the fix to — so it is asserted, not assumed.
#[test]
fn a_model_supplied_path_cannot_choose_the_binary_in_streaming_mode() {
    let dir = plant_a_fake_cat("streaming");
    let mut bridge = Bridge::spawn("path-stream");
    let reply = bridge.run(&format!(
        r#"{{"type":"RunCommand","command":"cat","args":["/etc/hostname"],"env":{{"PATH":"{}"}},"stream":true,"timeout_secs":10}}"#,
        dir.display()
    ));

    assert!(
        !reply.contains("\"success\":true"),
        "streaming mode accepted a model-supplied PATH: {reply}"
    );
    assert!(
        reply.contains("PATH"),
        "the refusal must name the key: {reply}"
    );
}

/// `LD_PRELOAD` needs no PATH entry and no write to a program's directory: the
/// loader maps the named object into every child. It is a refusal, and it says
/// so — a silently dropped key would leave the model free to retry forever and
/// the operator unaware that the attempt happened at all.
#[test]
fn ld_preload_is_refused_not_skipped() {
    let mut bridge = Bridge::spawn("ldpreload");
    let reply = bridge.run(
        r#"{"type":"RunCommand","command":"echo","args":["hi"],"env":{"LD_PRELOAD":"/tmp/evil.so"},"timeout_secs":10}"#,
    );

    assert!(
        reply.contains("\"success\":false"),
        "LD_PRELOAD was accepted: {reply}"
    );
    assert!(
        reply.contains("LD_PRELOAD"),
        "the refusal must name the key: {reply}"
    );
}

/// The complement, and the reason this is a denylist of a closed family rather
/// than an allowlist: an action's ordinary application variables must still
/// arrive. Breaking that gets the check reverted, and then the hole is back.
#[test]
fn an_ordinary_application_variable_still_reaches_the_child() {
    let mut bridge = Bridge::spawn("ordinary");
    let reply = bridge.run(
        r#"{"type":"RunCommand","command":"printenv","args":["MY_VAR"],"env":{"MY_VAR":"set-by-the-action"},"timeout_secs":10}"#,
    );
    assert!(
        reply.contains("set-by-the-action"),
        "an ordinary variable was refused; the filter must name only the \
         variables that change WHAT executes. Reply: {reply}"
    );
}
