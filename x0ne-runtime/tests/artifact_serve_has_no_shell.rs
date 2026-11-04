//! `ArtifactServe` runs an argv, and it runs it inside its own artifact.
//!
//! Audit of 2026-08-15, finding n°6. `ArtifactServe` was the only action in the
//! runtime that reached a shell — `Command::new("sh").arg("-c")` with a line
//! the model wrote — while `policy::commands_of` gated the FIRST WHITESPACE
//! TOKEN of that line. Everything after the first separator faced no gate at
//! all. Measured against a config whose `allowed_commands` was `["python3"]`
//! and whose read and write roots were both `["/tmp/x0ne-narrow"]`:
//!
//! ```text
//! command: "python3 --version ; cat /etc/passwd > /tmp/PWNED2-outside-write"
//!   → success:true
//!   → /tmp/PWNED2-outside-write : root:x:0:0:root:/root:/bin/bash
//!
//! command: "python3 --version ; echo OWNED > /tmp/PWNED ; $(id -un)"
//!   → /tmp/PWNED : OWNED
//!   → .artifact.log : sh: 1: <user>: not found
//! ```
//!
//! Three gates walked past by one `;`: the command allowlist (`cat` was not in
//! it), the read confinement (`/etc/passwd` was not under any read root) and
//! the write confinement (the destination was not under any write root).
//!
//! The second half of the finding is the working directory. `session_id` is
//! model-supplied and was joined straight into the storage path, so
//! `session_id: "../../../../../../tmp/x0ne-traversal"` produced an artifact
//! root outside the store — and this spawn `current_dir`s into it.
//!
//! A failure in this file means a served artifact can run a program no
//! allowlist named, or serve a directory that is not its own.
//!
//! Asserted through the real binary over a real bridge: the property is about
//! what a spawned child does on the filesystem, and only a real spawn answers
//! that.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// `python3` and `echo` are allowed BY NAME — the point of every test below is
/// that naming an allowed program is not a licence for the rest of the line.
/// The read and write roots are deliberately a directory nothing here uses, so
/// any file that appears outside it is a confinement failure and not an
/// accident of the test's own layout.
const CONFIG: &str = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = ["python3", "echo", "pwd"]
allowed_read_paths = ["/tmp/x0ne-serve-test-roots"]
allowed_write_paths = ["/tmp/x0ne-serve-test-roots"]
deny_path_patterns = []
readable_env_vars = []

[actions]
ArtifactCreate = true
ArtifactServe = true
ArtifactStop = true
"#;

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    home: std::path::PathBuf,
}

impl Bridge {
    fn spawn(tag: &str) -> Bridge {
        Bridge::spawn_allowing(tag, &[])
    }

    /// `extra_allowed` is appended to `allowed_commands`. It exists for one
    /// test: the operator who allowlists a whole shell line verbatim, which is
    /// the only way to reach the executor's own refusal past the policy gate.
    fn spawn_allowing(tag: &str, extra_allowed: &[&str]) -> Bridge {
        let dir =
            std::env::temp_dir().join(format!("x0ne-serve-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::create_dir_all("/tmp/x0ne-serve-test-roots").expect("policy roots");
        let cfg = dir.join("x0ne-runtime.toml");
        let mut text = CONFIG.to_string();
        if !extra_allowed.is_empty() {
            let extras: Vec<String> = extra_allowed
                .iter()
                .map(|c| serde_json::Value::String(c.to_string()).to_string())
                .collect();
            text = text.replace(
                r#"allowed_commands = ["python3", "echo", "pwd"]"#,
                &format!(
                    r#"allowed_commands = ["python3", "echo", "pwd", {}]"#,
                    extras.join(", ")
                ),
            );
        }
        std::fs::write(&cfg, &text).expect("write config");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("x0ne home");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        cmd.args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"]);
        // Every artifact this bridge creates lives under here, so the store
        // root the executor confines to is the test's own directory.
        cmd.env("X0NE_HOME", &home);
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
            home,
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

    /// Create a `service` artifact and return its id and on-disk root.
    fn create_service(&mut self) -> (String, std::path::PathBuf) {
        let reply = self.run(r#"{"type":"ArtifactCreate","kind":"service","name":"t"}"#);
        let v: serde_json::Value = serde_json::from_str(&reply).expect("json reply");
        assert_eq!(v["success"], true, "ArtifactCreate failed: {reply}");
        let id = v["output"]["id"].as_str().expect("id").to_string();
        let root = v["output"]["root_path"].as_str().expect("root").to_string();
        (id, std::path::PathBuf::from(root))
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for a predicate for up to ~4s. Spawned services are asynchronous; a
/// bare sleep is either flaky or slow.
fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..80 {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// SECURITY: the 2026-08-15 measurement, as a property. A serve command line is
/// an argv; the characters that turn one command into two are refused, and
/// nothing runs.
///
/// The canary is written OUTSIDE `allowed_write_paths` and reads a file outside
/// `allowed_read_paths` with a program outside `allowed_commands`, so its mere
/// existence proves all three gates were bypassed — not just that a second
/// process ran.
///
/// A failure here means the runtime has a shell again, and with it the
/// bypass of the command allowlist and of both path confinements.
#[test]
fn artifact_serve_has_no_shell() {
    let mut bridge = Bridge::spawn("noshell");
    let (id, _root) = bridge.create_service();

    let canary = std::env::temp_dir().join(format!("x0ne-PWNED-serve-{}", std::process::id()));
    let _ = std::fs::remove_file(&canary);

    for payload in [
        format!("python3 --version ; cat /etc/passwd > {}", canary.display()),
        format!(
            "python3 --version && cat /etc/passwd > {}",
            canary.display()
        ),
        format!("python3 --version | tee {}", canary.display()),
        format!("python3 -c $(cat /etc/passwd > {})", canary.display()),
    ] {
        let reply = bridge.run(&format!(
            r#"{{"type":"ArtifactServe","id":"{id}","command":{}}}"#,
            serde_json::Value::String(payload.clone())
        ));
        assert!(
            reply.contains("\"success\":false"),
            "a shell line was served: {payload:?} → {reply}"
        );
        // SECURITY: the refusal must be about the WHOLE line. The defect was
        // precisely that the gate ruled on `python3` and said nothing about the
        // rest, so a refusal naming only the first token would be the old
        // behaviour with a different verdict — and would come back the moment
        // `python3` were allowlisted.
        assert!(
            reply.contains(&payload) || reply.contains("refused"),
            "the refusal names only the first token, not the line: {reply}"
        );
    }

    // Give any child that might have been spawned time to do its damage.
    assert!(
        !wait_until(|| canary.exists()),
        "a second command ran from a serve line: {} exists, which means the \
         command allowlist, the read confinement and the write confinement \
         were all bypassed",
        canary.display()
    );
}

/// SECURITY: the executor refuses the shape on its own, and not because the
/// policy gate happened to refuse first.
///
/// With the hostile line itself in `allowed_commands`, the gate passes it —
/// `commands_of` hands the whole line to the allowlist and the allowlist
/// contains it. Everything downstream is then the executor's own check. Without
/// this test, deleting `serve_command::parse` from the executor would still
/// leave every other test in this file green, because the gate would be doing
/// all the work; that is this repository's named recurring defect in its
/// "second site nobody applied the fix to" form.
///
/// A failure here means the shell is reachable by an operator's own allowlist
/// entry — the one place an operator has no reason to expect a shell.
#[test]
fn the_executor_refuses_a_shell_line_even_when_the_operator_allowlists_it() {
    let canary =
        std::env::temp_dir().join(format!("x0ne-PWNED-allowlisted-{}", std::process::id()));
    let _ = std::fs::remove_file(&canary);
    let line = format!("echo hi ; cat /etc/passwd > {}", canary.display());

    let mut bridge = Bridge::spawn_allowing("allowlisted", &[&line]);
    let (id, _root) = bridge.create_service();
    let reply = bridge.run(&format!(
        r#"{{"type":"ArtifactServe","id":"{id}","command":{}}}"#,
        serde_json::Value::String(line.clone())
    ));

    assert!(
        reply.contains("\"success\":false"),
        "an allowlisted shell line was served: {reply}"
    );
    assert!(
        reply.contains("refused") && reply.contains("';'"),
        "the executor's refusal must name the character it refused: {reply}"
    );
    assert!(
        !wait_until(|| canary.exists()),
        "a second command ran: {}",
        canary.display()
    );
}

/// The absence of a shell is a positive fact, not only a set of refusals: a
/// program receives its arguments as written. `~` is the discriminator because
/// it is NOT in the refused set (it cannot chain, substitute or redirect) and
/// because every shell expands it. Unexpanded `~` in the log is a direct
/// observation that no shell stood between the runtime and the program.
///
/// A failure here means a shell is back in the path even though the refusals
/// still pass — which is exactly how the escaping design would fail.
#[test]
fn a_served_command_receives_its_arguments_unexpanded() {
    let mut bridge = Bridge::spawn("literal");
    let (id, root) = bridge.create_service();

    let reply = bridge.run(&format!(
        r#"{{"type":"ArtifactServe","id":"{id}","command":"echo ~ a*b"}}"#
    ));
    assert!(reply.contains("\"success\":true"), "serve failed: {reply}");

    let log = root.join(".artifact.log");
    assert!(
        wait_until(|| std::fs::read_to_string(&log)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)),
        "the served program wrote nothing to {}",
        log.display()
    );
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(
        text.contains("~ a*b"),
        "the arguments were expanded before the program saw them — a shell is \
         back in the path. Log: {text:?}"
    );
}

/// SECURITY: the artifact a serve command is given is the directory it runs in.
///
/// `session_id` is model-supplied and is joined into the storage path. It let
/// an artifact be created outside the store, and this spawn would have made
/// that directory the child's cwd (measured 2026-08-15: the id
/// `"../../../../../../tmp/x0ne-traversal"` produced a real directory outside
/// `…/artifacts/`).
///
/// A failure here means a model can point an artifact at any directory on the
/// host and serve it.
///
/// Limit, stated because the test cannot cover it: this pins WHERE the child
/// starts, not what it opens. A program told `--directory /` still serves `/`.
/// `allowed_commands` is the control for that, and it is the only one.
#[test]
fn an_artifact_cannot_be_served_from_outside_its_directory() {
    let mut bridge = Bridge::spawn("traversal");

    for hostile in ["../../../../../../tmp/x0ne-traversal", "..", ".", "a/b", ""] {
        let reply = bridge.run(&format!(
            r#"{{"type":"ArtifactCreate","kind":"service","session_id":{}}}"#,
            serde_json::Value::String(hostile.to_string())
        ));
        assert!(
            reply.contains("\"success\":false"),
            "session_id {hostile:?} was accepted: {reply}"
        );
        assert!(
            reply.contains("session_id"),
            "the refusal must name the field: {reply}"
        );
    }

    // Nothing escaped: the store holds exactly the sessions it should.
    let ephemeral = bridge.home.join("artifacts").join("ephemeral");
    if ephemeral.exists() {
        for entry in std::fs::read_dir(&ephemeral).expect("read ephemeral") {
            let name = entry.expect("entry").file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("..") && !name.contains('/'),
                "a session directory escaped the store: {name}"
            );
        }
    }

    // And a legitimate artifact runs in its own root: `pwd` reports the child's
    // working directory into the artifact's own log, which is the direct
    // observation rather than an inference from where the log file sits.
    let (id, root) = bridge.create_service();
    let reply = bridge.run(&format!(
        r#"{{"type":"ArtifactServe","id":"{id}","command":"pwd"}}"#
    ));
    assert!(reply.contains("\"success\":true"), "serve failed: {reply}");
    let log = root.join(".artifact.log");
    assert!(
        wait_until(|| std::fs::read_to_string(&log)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)),
        "the served program wrote nothing to {}",
        log.display()
    );
    let reported = std::fs::read_to_string(&log).expect("read log");
    let canonical = std::fs::canonicalize(&root).expect("canonical root");
    assert_eq!(
        reported.trim(),
        canonical.to_string_lossy(),
        "the child started somewhere other than its own artifact root"
    );
}

/// SECURITY: the working directory is re-resolved against the artifact store at
/// the moment of use, not trusted from what the registry recorded.
///
/// `ArtifactCreate` now refuses a climbing `session_id`, and that alone hides
/// this check: the traversal test above stays green with the confinement
/// deleted (measured while mutating, 2026-08-15 — a survivor until this test
/// existed). So the registry is edited directly, which is what any other write
/// to `root_path` — a future action, a migration, a hand-edited file — amounts
/// to. Validating on the way in and enforcing at the point of use are two
/// different guarantees, and only the second one holds when the first is
/// bypassed.
///
/// A failure here means an artifact whose recorded root points anywhere on the
/// host gets a process started there.
#[test]
fn a_registry_pointing_outside_the_store_does_not_get_a_process() {
    let mut bridge = Bridge::spawn("planted");
    let (id, _root) = bridge.create_service();

    let outside = std::env::temp_dir().join(format!("x0ne-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).expect("outside dir");

    let registry_path = bridge.home.join("artifacts").join("registry.json");
    let mut registry: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&registry_path).expect("read registry"))
            .expect("parse registry");
    registry["artifacts"][&id]["root_path"] =
        serde_json::Value::String(outside.to_string_lossy().into_owned());
    std::fs::write(
        &registry_path,
        serde_json::to_string(&registry).expect("serialise"),
    )
    .expect("write registry");

    let reply = bridge.run(&format!(
        r#"{{"type":"ArtifactServe","id":"{id}","command":"pwd"}}"#
    ));
    assert!(
        reply.contains("\"success\":false"),
        "a process was started outside the artifact store: {reply}"
    );
    assert!(
        reply.contains("outside the artifact store"),
        "the refusal must say the root is outside the store: {reply}"
    );
    // And nothing ran there: no log file was opened in the planted directory.
    assert!(
        !wait_until(|| outside.join(".artifact.log").exists()),
        "a child ran in {}",
        outside.display()
    );
}

/// The nominal case, end to end: a real artifact, a real server, a real HTTP
/// request, a real stop. Without this the safe move is to refuse everything,
/// and a feature that no longer works is a check that gets reverted.
#[test]
fn a_real_artifact_is_still_served_over_http() {
    let mut bridge = Bridge::spawn("nominal");
    let (id, root) = bridge.create_service();
    std::fs::write(root.join("index.html"), "<h1>served</h1>").expect("write index");

    let reply = bridge.run(&format!(
        r#"{{"type":"ArtifactServe","id":"{id}","command":"python3 -m http.server $PORT --bind 127.0.0.1"}}"#
    ));
    let v: serde_json::Value = serde_json::from_str(&reply).expect("json reply");
    assert_eq!(v["success"], true, "nominal serve failed: {reply}");
    let port = v["output"]["service_port"].as_u64().expect("service_port");
    let pid = v["output"]["service_pid"].as_u64().expect("service_pid");

    // Probe with a plain TCP request — no HTTP client dependency needed.
    let body = fetch(port);
    assert!(
        body.contains("served"),
        "the artifact was not served on 127.0.0.1:{port}. Got: {body:?}. Log: {:?}",
        std::fs::read_to_string(root.join(".artifact.log")).unwrap_or_default()
    );

    let stop = bridge.run(&format!(r#"{{"type":"ArtifactStop","id":"{id}"}}"#));
    assert!(stop.contains("\"success\":true"), "stop failed: {stop}");
    assert!(
        wait_until(|| !process_alive(pid as u32)),
        "pid {pid} survived ArtifactStop"
    );
}

/// GET `/index.html`, retrying while the server comes up.
fn fetch(port: u64) -> String {
    use std::io::Read;
    for _ in 0..80 {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port as u16)) {
            let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
            if s.write_all(b"GET /index.html HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
                .is_ok()
            {
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                if !buf.is_empty() {
                    return buf;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    String::new()
}

/// Alive means *running*, not *present in `/proc`*.
///
/// A served process is a direct child of the runtime and nothing reaps it, so
/// after SIGTERM it stays in `/proc` as a zombie. `x0ne_artifact::pid_alive`
/// tests `/proc/<pid>` existence and therefore calls a reaped-nothing zombie
/// alive — a pre-existing wart, unrelated to the shell removal, and the reason
/// this helper reads `State:` instead. Measured 2026-08-15: `State: Z (zombie)`
/// one second after `ArtifactStop`.
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(s) => !s
            .lines()
            .any(|l| l.starts_with("State:") && l.contains('Z')),
        Err(_) => false,
    }
}
