//! Term* through the real binary, as an MCP client (Claude Code) sees them.
//!
//! Two terminals talk through a file; the recordings must exist afterwards;
//! every program must be gone once the runtime exits, cleanly or killed.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

const CONFIG: &str = r#"
[runtime]
mode = "mcp"

[policy]
mode = "auto"
default_effect = "deny"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = ["python3", "bash", "sleep"]
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
readable_env_vars = []

[term]
max_sessions = 4

[actions]
TermOpen = true
TermSend = true
TermRead = true
TermWait = true
TermResize = true
TermSnapshot = true
TermList = true
TermClose = true
"#;

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    home: tempfile::TempDir,
}

impl Mcp {
    fn spawn() -> Mcp {
        let home = tempfile::tempdir().expect("home");
        let cfg = home.path().join("x0ne-runtime.toml");
        std::fs::write(&cfg, CONFIG).expect("config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "--mode",
                "mcp",
                "--isolation",
                "none",
                "--upstream-human-gate",
            ])
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp {
            child,
            stdin,
            stdout,
            next_id: 1,
            home,
        };
        let init = m.request(
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},
                   "clientInfo":{"name":"test","version":"0"}}),
        );
        assert!(init.get("result").is_some(), "initialize: {init}");
        m.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        m
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("answer");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }

    /// Call a tool; return the `data` of its output, panicking on isError.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let res = &r["result"];
        let text = res["content"][0]["text"].as_str().unwrap_or("").to_string();
        assert_eq!(res["isError"], false, "{tool} {args}: {text}");
        let out: Value = serde_json::from_str(&text).expect("output is JSON");
        out["data"].clone()
    }

    fn recordings(&self) -> PathBuf {
        self.home.path().join(".x0ne/term")
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pid_alive(pid: u64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s
            .rsplit(')')
            .next()
            .map(|t| t.trim_start().starts_with('Z'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn gone_within(pid: u64, ms: u64) -> bool {
    for _ in 0..(ms / 50) {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !pid_alive(pid)
}

fn files_with_ext(root: &Path, ext: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == ext).unwrap_or(false))
        .collect()
}

/// The owner's scenario: two terminals, a REPL and a shell, exchanging
/// through a file, then closed; the replayable recordings remain.
#[test]
fn an_mcp_client_drives_two_terminals_that_talk_through_a_file() {
    let mut m = Mcp::spawn();
    let tools = m.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for t in [
        "term_open",
        "term_send",
        "term_read",
        "term_wait",
        "term_resize",
        "term_snapshot",
        "term_list",
        "term_close",
    ] {
        assert!(names.contains(&t), "{t} missing from tools/list");
    }

    let shared = tempfile::tempdir_in("/tmp").unwrap();
    let msg = shared.path().join("msg");

    m.call(
        "term_open",
        json!({"name":"py","command":"python3","args":["-q","-i"]}),
    );
    m.call(
        "term_open",
        json!({"name":"sh","command":"bash","args":["--norc","--noprofile"]}),
    );
    m.call(
        "term_wait",
        json!({"name":"py","pattern":">>> ","timeout_ms":10000}),
    );

    m.call(
        "term_send",
        json!({"name":"sh","text":format!("echo from-bash > {}", msg.display()),"keys":["Enter"]}),
    );
    let w = m.call(
        "term_wait",
        json!({"name":"sh","stable_ms":300,"timeout_ms":10000}),
    );
    assert_ne!(w["condition"], "timeout", "{w}");
    for _ in 0..100 {
        if msg.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    m.call(
        "term_send",
        json!({"name":"py","text":format!("print(open('{}').read().strip()[::-1])", msg.display()),"keys":["Enter"]}),
    );
    let w = m.call(
        "term_wait",
        json!({"name":"py","pattern":"hsab-morf","timeout_ms":10000}),
    );
    assert_eq!(w["condition"], "pattern", "{w}");
    let r = m.call("term_read", json!({"name":"py"}));
    assert!(r["text"].as_str().unwrap().contains("hsab-morf"), "{r}");

    let l = m.call("term_list", json!({}));
    assert_eq!(l["terminals"].as_array().unwrap().len(), 2, "{l}");

    m.call("term_close", json!({"name":"py"}));
    m.call("term_close", json!({"name":"sh"}));
    std::thread::sleep(Duration::from_millis(300));

    let casts = files_with_ext(&m.recordings(), "cast");
    assert_eq!(
        casts.len(),
        2,
        "one replayable recording per terminal: {casts:?}"
    );
    for c in &casts {
        let body = std::fs::read_to_string(c).unwrap();
        let header: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert!(body.lines().count() > 2, "{c:?} holds events");
    }
    let audit = files_with_ext(&m.recordings(), "jsonl");
    assert_eq!(audit.len(), 1, "{audit:?}");
    let journal = std::fs::read_to_string(&audit[0]).unwrap();
    assert!(journal.contains("from-bash"), "TermSend is journaled");
    assert!(journal.contains("\"python3\""), "TermOpen is journaled");
}

fn open_sleeper(m: &mut Mcp, name: &str) -> u64 {
    let o = m.call(
        "term_open",
        json!({"name":name,"command":"sleep","args":["300"]}),
    );
    o["pid"].as_u64().expect("pid")
}

/// End of stdin: the runtime exits and takes every terminal with it.
#[test]
fn every_terminal_dies_when_the_runtime_exits() {
    let mut m = Mcp::spawn();
    let pids = [open_sleeper(&mut m, "a"), open_sleeper(&mut m, "b")];
    drop(m.stdin.take());
    let _ = m.child.wait();
    for pid in pids {
        assert!(gone_within(pid, 5000), "pid {pid} outlived the runtime");
    }
}

/// SIGKILL gives no chance to clean up: the kernel must still do it.
#[test]
fn every_terminal_dies_when_the_runtime_is_killed() {
    let mut m = Mcp::spawn();
    let pids = [open_sleeper(&mut m, "a"), open_sleeper(&mut m, "b")];
    let _ = m.child.kill();
    let _ = m.child.wait();
    for pid in pids {
        assert!(
            gone_within(pid, 5000),
            "pid {pid} outlived a killed runtime"
        );
    }
}
