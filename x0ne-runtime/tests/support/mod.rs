//! The real `x0ne-runtime` binary over stdio, as the orchestrator drives it:
//! one JSON line out, one JSON line back.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct Runtime {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<String>,
}

impl Runtime {
    /// A stdio runtime whose read and write roots are `dir`, with
    /// `allowed_commands` set to `commands`.
    pub fn start(dir: &std::path::Path, commands: &[&str]) -> Self {
        let cfg = dir.join("x0ne-runtime.toml");
        let d = dir.display();
        let commands = commands
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            &cfg,
            format!(
                r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
allowed_commands = [{commands}]
allowed_read_paths = ["{d}"]
allowed_write_paths = ["{d}"]
deny_path_patterns = []
readable_env_vars = []

[actions]
RunCommand = true
WriteFile = true
MoveToTrash = true
Pipeline = true
"#
            ),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
            .args(["--isolation", "none"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if tx.send(line).is_err() {
                break;
            }
        });
        Self {
            child,
            stdin,
            replies,
        }
    }

    /// One line out, one reply back.
    pub fn send(&mut self, line: serde_json::Value) -> serde_json::Value {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let reply = self
            .replies
            .recv_timeout(Duration::from_secs(20))
            .expect("the runtime answered");
        serde_json::from_str(&reply).expect("one JSON reply")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
