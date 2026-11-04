//! `list_processes` over MCP under the shipped read-only profile. The
//! command line of another program, `--password=` and URL credentials
//! included, reached the client verbatim.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn send(stdin: &mut impl Write, v: Value) {
    writeln!(stdin, "{v}").unwrap();
    stdin.flush().unwrap();
}

fn reply(stdout: &mut impl BufRead) -> Value {
    let mut line = String::new();
    stdout.read_line(&mut line).expect("reply");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
}

#[test]
fn the_readonly_profile_masks_credentials_in_other_command_lines() {
    let profile =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../x0ne-runtime-mcp-readonly.toml");
    let home = tempfile::tempdir().expect("home");
    let mut canary = Command::new("sh")
        .args([
            "-c",
            "read -r line",
            "x0ne-612",
            "--password=CANARY-612",
            "https://u:CANARY-612@example.invalid/path",
            "--verbose",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sh");
    let mut runtime = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
        .args(["--mode", "mcp", "--config", profile.to_str().unwrap()])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn x0ne-runtime");
    let mut stdin = runtime.stdin.take().unwrap();
    let mut stdout = BufReader::new(runtime.stdout.take().unwrap());

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}}),
    );
    let init = reply(&mut stdout);
    assert!(init.get("result").is_some(), "initialize: {init}");
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"list_processes","arguments":{"filter":"sh"}}}),
    );
    let r = reply(&mut stdout);
    let _ = canary.kill();
    let _ = canary.wait();
    let _ = runtime.kill();
    let _ = runtime.wait();

    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert!(!r.to_string().contains("CANARY-612"), "leaked: {text}");
    assert!(
        text.contains(
            "sh -c read -r line x0ne-612 --password=[redacted] \
             https://[redacted]@example.invalid/path --verbose"
        ),
        "{text}"
    );
}
