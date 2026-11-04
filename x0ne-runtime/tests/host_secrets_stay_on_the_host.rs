//! A secret in the operator's environment never reaches a child process.
//!
//! Audit of 2026-08-14 (`AUDIT-2026-08-14.md`, section 4). A canary exported in
//! the shell that launched `x0ne` was returned verbatim by `printenv` through
//! RunCommand. Measured that day, and again on 2026-08-15 before the fix:
//!
//! ```text
//! $ export X0NE_CANARY_TEST=fuite-1786762684
//! $ echo '{"type":"RunCommand","command":"printenv"}' | x0ne-runtime --mode stdio
//! …X0NE_CANARY_TEST=fuite-1786762684…SSH_AUTH_SOCK=…CLAUDE_CODE_MESSAGING_TOKEN=…
//! ```
//!
//! Two consumers make that fatal rather than merely untidy. The stdout goes
//! back to the model (`x0ne-orchestrator`, `conversation.rs:854`), and it is
//! appended to the session log (`x0ne-orchestrator`, `session.rs:100`) as clear
//! text that outlives the session. `x0ne-code` loads `~/.x0ne/.env` into the
//! process with dotenvy (`main.rs:401`), so the set of things one `printenv`
//! away from the transcript was every API key the operator owns. `printenv` is
//! on the read-only dispensation list (`agent/readonly_cmd.rs:63`), so it did
//! not even stop at the human gate.
//!
//! A failure of any test here means X0NE exfiltrates its operator's credentials
//! on request — the exact inverse of what the product claims to be.
//!
//! Asserted through the real binary over a real bridge, for the same reason as
//! `bridge_stdio_survives_children.rs`: the property is about what a spawned
//! child inherits, and only a spawned child can answer it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const CONFIG: &str = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
# The two binaries these tests spawn, named explicitly: since 2026-08-15 an
# empty `allowed_commands` denies every command rather than allowing every
# command. This file is about what a child INHERITS in its environment, not
# about the allowlist — it had merely inherited the permissive reading of `[]`.
allowed_commands = ["printenv", "echo"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []

[actions]
RunCommand = true
"#;

/// The same, plus the operator opting one name through. Proves the escape hatch
/// is real — and, by the name it does NOT carry, that it grants only what it
/// names.
const CONFIG_WITH_PASSTHROUGH: &str = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
# The two binaries these tests spawn, named explicitly: since 2026-08-15 an
# empty `allowed_commands` denies every command rather than allowing every
# command. This file is about what a child INHERITS in its environment, not
# about the allowlist — it had merely inherited the permissive reading of `[]`.
allowed_commands = ["printenv", "echo"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []
env_passthrough = ["X0NE_CANARY_OPTED_IN"]

[actions]
RunCommand = true
"#;

/// Names planted in the runtime's own environment before it starts. Between
/// them they cover the shapes that actually leaked: the audit's canary, an
/// orchestrator secret, a model API key, and a `CLOUDSDK_*` credential whose
/// prefix is shared with configuration that IS allowed through.
const CANARIES: &[(&str, &str)] = &[
    ("X0NE_CANARY_TEST", "fuite-canary-1"),
    ("ANTHROPIC_API_KEY", "sk-ant-canary-2"),
    ("CLAUDE_CODE_MESSAGING_TOKEN", "canary-3"),
    ("CLOUDSDK_AUTH_ACCESS_TOKEN", "ya29.canary-4"),
];

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    fn spawn(tag: &str, config: &str) -> Bridge {
        let dir =
            std::env::temp_dir().join(format!("x0ne-envleak-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cfg = dir.join("x0ne-runtime.toml");
        std::fs::write(&cfg, config).expect("write config");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        cmd.args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"]);

        // The canaries go into the RUNTIME's environment, which is what an
        // operator's shell does to it. Setting them on this test process would
        // prove nothing: the runtime is a separate process either way, and the
        // question is what it hands to its own children.
        for (k, v) in CANARIES {
            cmd.env(k, v);
        }
        cmd.env("X0NE_CANARY_OPTED_IN", "explicitly-allowed");

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

    fn send(&mut self, json: &str) {
        writeln!(self.stdin, "{json}").expect("write action to bridge");
        self.stdin.flush().expect("flush bridge");
    }

    fn recv(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }

    /// Run a command and return the runtime's whole reply line. That line is
    /// also, byte for byte, what the orchestrator hands to `log_action`
    /// (`session.rs:100`) — so asserting on it asserts on the session log too,
    /// without needing an orchestrator in the loop.
    fn run(&mut self, json: &str) -> String {
        self.send(json);
        self.recv().expect("runtime must answer")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const PRINTENV: &str = r#"{"type":"RunCommand","command":"printenv","args":[],"timeout_secs":10}"#;

/// The root property, and the one the audit measured breaking.
#[test]
fn a_host_secret_never_reaches_a_child_process() {
    let mut bridge = Bridge::spawn("secret", CONFIG);
    let reply = bridge.run(PRINTENV);

    for (name, value) in CANARIES {
        assert!(
            !reply.contains(value),
            "the value of {name} came back out of a child process — every secret \
             in the operator's shell is readable by the agent and lands in the \
             session log in clear text. Reply: {reply}"
        );
        assert!(
            !reply.contains(name),
            "{name} is present in the child's environment. Even without reading \
             the value, its existence tells an attacker what to ask for next. \
             Reply: {reply}"
        );
    }
}

/// The same bytes, seen as the audit's second consumer. `session.rs:100` writes
/// the runtime's reply into `~/.x0ne/logs/*.jsonl` unredacted and append-only,
/// so a secret that reaches this line is a secret on disk forever — long after
/// the operator has rotated it and forgotten the session.
#[test]
fn the_session_log_payload_carries_no_host_secret() {
    let mut bridge = Bridge::spawn("log", CONFIG);
    let reply = bridge.run(PRINTENV);
    let logged: serde_json::Value =
        serde_json::from_str(&reply).expect("the reply is the JSON the session log stores");

    let flat = logged.to_string();
    for (name, value) in CANARIES {
        assert!(
            !flat.contains(value) && !flat.contains(name),
            "{name} would be written to the session log in clear text: {flat}"
        );
    }
}

/// The complement, and the reason this is an allowlist and not a denylist: what
/// remains must still be enough to work. A cleared environment that breaks
/// `git`, `gcloud` and every bare command name gets reverted within the week,
/// and then the leak is back.
#[test]
fn the_allowlist_still_lets_a_child_do_its_job() {
    let mut bridge = Bridge::spawn("works", CONFIG);
    let reply = bridge.run(PRINTENV);

    // PATH is what makes `Command::new("git")` resolve at all. Without it every
    // bare-command scenario becomes ENOENT.
    assert!(reply.contains("PATH="), "PATH did not survive: {reply}");
    // HOME is where gcloud's ADC and ssh's config live — the `x0ne cc` chantier
    // needs both (x0ne-code/src/cc_cmd.rs:1204-1210).
    assert!(reply.contains("HOME="), "HOME did not survive: {reply}");

    // And the commands themselves still run.
    let echo = bridge
        .run(r#"{"type":"RunCommand","command":"echo","args":["STILL-WORKS"],"timeout_secs":10}"#);
    assert!(
        echo.contains("STILL-WORKS"),
        "a bare command no longer resolves through PATH: {echo}"
    );
}

/// An action's own APPLICATION variables are the caller's stated intent and
/// must survive the clear. The ordering bug that would break it — `env_clear`
/// after `env` — both compiles and passes every other test in this file.
///
/// SECURITY: the second half of this test is the correction of 2026-08-15. As
/// written before that date it asserted only the first half, and in doing so it
/// documented the hole it was standing next to: `harden` clears PATH from the
/// allowlist and the action's map was applied afterwards WITHOUT ANY FILTER ON
/// THE KEY (`executor/process.rs:228`), so `env:{"PATH":"/tmp/evil"}` chose the
/// binary behind an approved command name. "The caller's env survives" was
/// true, and it was the wrong property to assert alone. The variables that
/// decide WHAT executes are refused — see `x0ne_actions::env_keys` and
/// `x0ne-runtime/tests/model_env_cannot_choose_the_binary.rs`.
#[test]
fn an_action_can_still_set_its_own_environment() {
    let mut bridge = Bridge::spawn("explicit", CONFIG);
    let reply = bridge.run(
        r#"{"type":"RunCommand","command":"printenv","args":["MY_VAR"],"env":{"MY_VAR":"set-by-the-action"},"timeout_secs":10}"#,
    );
    assert!(
        reply.contains("set-by-the-action"),
        "the action's explicit env map was erased by the clear — `harden` must \
         run BEFORE the caller's `.env()` calls, not after. Reply: {reply}"
    );

    for key in ["PATH", "LD_PRELOAD", "HOME", "PYTHONPATH"] {
        let refused = bridge.run(&format!(
            r#"{{"type":"RunCommand","command":"printenv","args":["{key}"],"env":{{"{key}":"/tmp/chosen-by-the-model"}},"timeout_secs":10}}"#
        ));
        assert!(
            refused.contains("\"success\":false") && refused.contains(key),
            "an action set {key} and the runtime accepted it — the env map \
             decides what runs, and it did so behind a name the gate approved. \
             Reply: {refused}"
        );
    }
}

/// `policy.env_passthrough` is the operator's escape hatch, and it grants
/// exactly what it names. The runtime here holds five extra variables; one is
/// configured through and four are not.
#[test]
fn env_passthrough_grants_only_what_the_operator_named() {
    let mut bridge = Bridge::spawn("passthrough", CONFIG_WITH_PASSTHROUGH);
    let reply = bridge.run(PRINTENV);

    assert!(
        reply.contains("explicitly-allowed"),
        "env_passthrough is dead scaffolding: the operator named \
         X0NE_CANARY_OPTED_IN and the child did not get it. Reply: {reply}"
    );
    for (name, value) in CANARIES {
        assert!(
            !reply.contains(value) && !reply.contains(name),
            "opting one variable in released {name} as well — env_passthrough \
             must widen the list by the names it holds and by nothing else. \
             Reply: {reply}"
        );
    }
}

/// The default is strict. A config that never mentions `env_passthrough` — which
/// is every config written before 2026-08-15, including the ones already on
/// operators' machines — forwards nothing. `CONFIG` above omits the key
/// entirely, and this is the test that says the omission is not an accident.
#[test]
fn a_config_that_omits_env_passthrough_forwards_nothing() {
    let mut bridge = Bridge::spawn("default", CONFIG);
    let reply = bridge.run(PRINTENV);
    assert!(
        !reply.contains("explicitly-allowed"),
        "the runtime forwarded a variable no config asked for: {reply}"
    );
}
