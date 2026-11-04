//! `search_code` and `search_and_read` over MCP under the shipped read-only
//! profile. Both confined their walk root and then read every regular file
//! under it: `ReadFile ~/.ssh/id_ed25519` was refused, while a search rooted
//! at `~` returned the key line, `.env` and `secrets/db` (audit 2026-11-02,
//! B2). Every file of the walk now faces the gate's own read refusal.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

const CANARY: &str = "CANARY-B2";

/// The secrets of the audit, each holding the canary, and one public file.
const SECRETS: [&str; 4] = [
    ".ssh/id_ed25519",
    "proj/.env",
    "proj/secrets/db",
    "proj/config/server.pem",
];
const PUBLIC: &str = "proj/README.md";

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(home: &Path) -> Self {
        let profile =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../x0ne-runtime-mcp-readonly.toml");
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"))
            .args(["--mode", "mcp", "--config", profile.to_str().unwrap()])
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Mcp {
            child,
            stdin,
            stdout,
            next: 1,
        };
        let init = mcp.request(
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},
                   "clientInfo":{"name":"test","version":"0"}}),
        );
        assert!(init.get("result").is_some(), "initialize: {init}");
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        mcp
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("reply");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fake home: the secrets above and a public README, all with the canary.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("home");
    for f in SECRETS.iter().chain(std::iter::once(&PUBLIC)) {
        let p = home.path().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("line one\nsecret={CANARY} in {f}\n")).unwrap();
    }
    home
}

fn text_of(reply: &Value) -> String {
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The reply names the public file and none of the secrets, and it says that
/// files were withheld.
fn assert_only_public(reply: &Value, label: &str) {
    let text = text_of(reply);
    for secret in SECRETS {
        assert!(
            !reply.to_string().contains(&format!("in {secret}")),
            "{label}: {secret} came back: {text}"
        );
    }
    assert!(
        text.contains(&format!("in {PUBLIC}")),
        "{label}: the README must still be found: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let omitted = body
        .get("omitted_by_policy")
        .or_else(|| body["data"].get("omitted_by_policy"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        omitted > 0,
        "{label}: the reply must say files were withheld: {text}"
    );
}

#[test]
fn search_code_rooted_at_home_returns_no_denied_file() {
    let home = home();
    let mut mcp = Mcp::start(home.path());
    // Precondition of the audit: the same files are refused one by one.
    for secret in SECRETS {
        let r = mcp.call(
            "read_file",
            json!({"path": home.path().join(secret).to_str().unwrap()}),
        );
        assert!(
            !r.to_string().contains(CANARY),
            "read_file {secret} must be refused: {r}"
        );
    }
    let r = mcp.call(
        "search_code",
        json!({"root": home.path().to_str().unwrap(), "pattern": CANARY}),
    );
    assert_only_public(&r, "search_code ~");
    let r = mcp.call(
        "search_code",
        json!({"root": home.path().join("proj").to_str().unwrap(), "pattern": CANARY}),
    );
    assert_only_public(&r, "search_code ~/proj");
}

#[test]
fn search_and_read_rooted_at_home_returns_no_denied_file() {
    let home = home();
    let mut mcp = Mcp::start(home.path());
    let r = mcp.call(
        "search_and_read",
        json!({"root": home.path().to_str().unwrap(), "pattern": CANARY, "context_lines": 2}),
    );
    assert_only_public(&r, "search_and_read ~");
    let r = mcp.call(
        "search_and_read",
        json!({"root": home.path().join("proj").to_str().unwrap(), "pattern": CANARY}),
    );
    assert_only_public(&r, "search_and_read ~/proj");
}
