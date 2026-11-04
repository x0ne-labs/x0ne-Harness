//! In stdio mode fd 0 is the protocol channel. A human primitive that
//! reads it swallows the next request as the "answer" and that request is
//! never executed. Here: AskHuman inside a Pipeline, then an echo.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CONFIG: &str = r#"
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

[actions]
RunCommand = true
"#;

#[test]
fn a_human_prompt_never_consumes_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("x0ne-runtime.toml");
    std::fs::write(&cfg, CONFIG).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
        .args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
        .args(["--isolation", "none"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn x0ne-runtime");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if tx.send(line).is_err() {
            break;
        }
    });

    let ask = r#"{"type":"Pipeline","steps":[{"type":"AskHuman","question":"which branch?"}]}"#;
    let echo =
        r#"{"type":"RunCommand","command":"echo","args":["SECOND-REQUEST-RAN"],"timeout_secs":10}"#;
    writeln!(stdin, "{ask}").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    writeln!(stdin, "{echo}").unwrap();
    stdin.flush().unwrap();

    let mut replies = Vec::new();
    while let Ok(l) = rx.recv_timeout(Duration::from_secs(15)) {
        replies.push(l);
        if replies.len() == 2 {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let all = replies.join("\n");
    assert!(
        !all.contains(r#""answer":"{"#),
        "the protocol line was read as the human's answer: {all}"
    );
    assert_eq!(
        replies.len(),
        2,
        "the second request was never answered: {all}"
    );
    assert!(all.contains("SECOND-REQUEST-RAN"), "{all}");
}
