//! The bridge stdio channel must outlive the commands it runs.
//!
//! Field report, 2026-08-14 ~13:32 UTC, H100 milestone. A `gcloud compute ssh`
//! post-reboot probe hit its 20 s timeout, the runtime SIGKILLed it, logged
//! `Action complete … duration_ms=20010`, and then died on the very next line
//! with `Error: Resource temporarily unavailable (os error 11)` — EAGAIN. The
//! 26 following actions went into the void, and the automatic safety stop
//! failed with "x0ne-runtime returned an empty response line" while an H100 VM
//! at 3.72 $/h kept billing.
//!
//! Mechanism: `spawn_capture` piped stdout/stderr but left stdin INHERITED, so
//! the child got the runtime's own fd 0 — the bridge pipe. `O_NONBLOCK` is a
//! property of the open file *description* shared across the fork, ssh sets it
//! on the fds it drives, and a killed ssh never restores it. The runtime's next
//! blocking `read_line` on the bridge then returned EAGAIN.
//!
//! A failure here means the orchestrator can lose its runtime — and with it the
//! only channel it has to stop a runaway machine.
//!
//! Refs: openssh-portable#246 (fixed by 7be4ac8),
//! jdebp.info/FGA/dont-set-shared-file-descriptors-to-non-blocking-mode.html

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
# Every binary this file spawns: an empty list denies all since 2026-08-15, and the
# subject here is fd 0, not the allowlist. `readlink` runs inside an `sh -c` line,
# whose sub-commands each face the list.
allowed_commands = ["python3", "echo", "sh", "readlink"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []

[actions]
RunCommand = true
"#;

/// A child that flips its own fds to non-blocking and then hangs, so the
/// timeout has to kill it while the flag is still set — the ssh behaviour,
/// reduced to its essence and with no network.
const POISONER: &str = "import fcntl,os,time;\
                        [fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) | os.O_NONBLOCK) \
                        for fd in (0,1,2)];\
                        time.sleep(30)";

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    /// Spawn the real binary in stdio mode behind real pipes. The pipe matters:
    /// with an inherited fd 0 the child would be poisoning *this* pipe, exactly
    /// as it poisoned the orchestrator's in production.
    fn spawn(tag: &str) -> Bridge {
        let dir =
            std::env::temp_dir().join(format!("x0ne-bridge-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cfg = dir.join("x0ne-runtime.toml");
        std::fs::write(&cfg, CONFIG).expect("write config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
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

    /// `None` means the runtime's stdout closed — i.e. the process is gone,
    /// which is the production symptom ("empty response line").
    fn recv(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn have_python3() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("import fcntl")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The root property: RunCommand's children get `/dev/null` on fd 0, never the
/// bridge pipe. Asserted from inside a live bridge, because that is the only
/// place fd 0 really is the protocol channel — `cargo test` hands the test
/// process a `/dev/null` stdin of its own, so checking it in-process would pass
/// with or without the fix and prove nothing.
#[test]
fn a_child_never_receives_the_bridge_stdin() {
    let mut bridge = Bridge::spawn("stdin");
    bridge.send(
        r#"{"type":"RunCommand","command":"sh","args":["-c","readlink /proc/self/fd/0"],"timeout_secs":10}"#,
    );
    let reply = bridge.recv().expect("runtime must answer");
    assert!(
        reply.contains("/dev/null"),
        "a RunCommand child inherited the bridge's stdin instead of /dev/null — \
         it can now poison the orchestrator's channel. Reply: {reply}"
    );
    assert!(
        !reply.contains("pipe:"),
        "the child's fd 0 is a pipe, i.e. the bridge itself. Reply: {reply}"
    );
}

/// The property that was violated in production: a child killed by its timeout
/// must not be able to take the bridge down with it. Sends a poisoning child
/// that gets SIGKILLed, then an ordinary `echo` — the echo must be answered.
#[test]
fn a_killed_child_cannot_poison_the_bridge_stdio() {
    if !have_python3() {
        eprintln!("skipping: python3 with fcntl unavailable, cannot build the poisoning child");
        return;
    }

    let mut bridge = Bridge::spawn("poison");
    let poison = serde_json::json!({
        "type": "RunCommand",
        "command": "python3",
        "args": ["-c", POISONER],
        "timeout_secs": 2,
    })
    .to_string();

    bridge.send(&poison);
    let first = bridge
        .recv()
        .expect("runtime must answer the timed-out action");
    assert!(
        first.contains("\"timed_out\":true"),
        "the poisoning child was expected to hit its timeout and be killed: {first}"
    );

    // The runtime must be parked in `read_line` on an EMPTY pipe when the next
    // action arrives. This is not decoration: a non-blocking read that finds
    // bytes already waiting succeeds normally, so a harness quick enough to
    // have the next line buffered hides the bug entirely — the first version of
    // this test did exactly that and passed against the unfixed runtime. In
    // production an LLM turn sits in this gap, so the empty pipe is the real
    // case, not the fast one.
    std::thread::sleep(std::time::Duration::from_millis(400));
    bridge
        .send(r#"{"type":"RunCommand","command":"echo","args":["STILL-ALIVE"],"timeout_secs":10}"#);
    let second = bridge.recv().expect(
        "the runtime died after killing a child that had touched the bridge fds — \
         this is the 2026-08-14 EAGAIN bug: every later action goes nowhere and the \
         orchestrator's safety stop can no longer reach the machine it must shut down",
    );
    assert!(
        second.contains("STILL-ALIVE"),
        "bridge answered but not with the echo's output: {second}"
    );
}
