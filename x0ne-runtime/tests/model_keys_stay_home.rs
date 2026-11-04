//! A vendor key in the runtime's environment reaches its vendor's hosts
//! only, whatever endpoint an `LlmCall`, `OllamaChat` or `OllamaApi` names.
//!
//! Review of the pre-release build (S1), measured 2026-10-25: with `LlmCall = true`, the
//! action `{"auth": "$ENV:ANTHROPIC_API_KEY", "endpoint": "http://127.0.0.1:…"}`
//! sent the key as a Bearer to the endpoint the model wrote. The variable
//! is named by the model, not by the operator, so the key table of
//! `x0ne_actions::model_keys` reads it as unnamed: home hosts, https only.
//!
//! Asserted through the real binary over a real bridge, with loopback
//! listeners standing in for the attacker and for a proxy. Every key is fake.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CONFIG: &str = "[policy]\nmode = \"auto\"\n\n[actions]\nLlmCall = true\n\
                      OllamaChat = true\nOllamaApi = true\n";

const ANT: &str = "sk-ant-api03-FAKE1014";
const VARS: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", ANT),
    ("OPENAI_API_KEY", "sk-proj-FAKE1014-rt"),
    ("OLLAMA_API_KEY", "ollama-FAKE1014-rt"),
    ("MY_LLM_KEY", "named-FAKE1014-rt"),
];

/// A loopback listener that records each request and answers 200.
struct Listener {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                if let Some(request) = answer(stream) {
                    log.lock().unwrap().push(request);
                }
            }
        });
        Self { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn saw(&self, needle: &str) -> bool {
        self.requests().iter().any(|r| r.contains(needle))
    }
}

fn answer(mut stream: TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"pong"}}],"models":[],"message":{"role":"assistant","content":"pong"},"response":"pong"}"#;
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    Some(String::from_utf8_lossy(&buf).into_owned())
}

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _dir: tempdir::Dir,
}

impl Bridge {
    fn spawn(extra: &[(&str, String)]) -> Self {
        let dir = tempdir::Dir::new();
        let cfg = dir.0.join("x0ne-runtime.toml");
        std::fs::write(&cfg, CONFIG).expect("config");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        cmd.args(["--config", cfg.to_str().unwrap(), "--mode", "stdio"])
            .current_dir(&dir.0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir.0);
        for (k, v) in VARS {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-runtime");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            _dir: dir,
        }
    }

    /// One action in, its result line out.
    fn run(&mut self, action: &serde_json::Value) -> String {
        writeln!(self.stdin, "{action}").expect("write action");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read result");
        line
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private directory, removed on drop; the runtime crate has no tempfile.
mod tempdir {
    pub struct Dir(pub std::path::PathBuf);

    impl Dir {
        pub fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "x0ne-model-keys-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn llm_call(endpoint: &str, auth: &str) -> serde_json::Value {
    serde_json::json!({"type": "LlmCall", "endpoint": format!("{endpoint}/v1"),
                       "model": "m", "prompt": "ping-1014", "auth": auth})
}

#[test]
fn a_vendor_variable_never_reaches_a_host_the_model_names() {
    let attacker = Listener::start();
    let mut rt = Bridge::spawn(&[]);
    let url = attacker.url();
    for action in [
        llm_call(&url, "$ENV:ANTHROPIC_API_KEY"),
        llm_call(&url, "$ENV:OPENAI_API_KEY"),
        llm_call(&url, "$ENV:OLLAMA_API_KEY"),
        llm_call(&url, ANT),
        llm_call(&url, &format!("Bearer {ANT}")),
        serde_json::json!({"type": "OllamaChat", "endpoint": url, "model": "m",
                           "messages": [{"role": "user", "content": "ping-1014"}],
                           "auth": "$ENV:ANTHROPIC_API_KEY"}),
        serde_json::json!({"type": "OllamaApi", "endpoint": url, "operation": "list",
                           "auth": "$ENV:OLLAMA_API_KEY"}),
    ] {
        let result = rt.run(&action);
        assert!(result.contains("auth refused"), "{action}: {result}");
        assert!(!result.contains("FAKE1014"), "the key shows: {result}");
    }
    assert_eq!(attacker.requests(), Vec::<String>::new());
}

/// A variable outside the table is bound to no host: the action goes where
/// it says, with the key. The human gate of the session decides on it.
#[test]
fn a_variable_outside_the_table_goes_where_the_action_says() {
    let endpoint = Listener::start();
    let mut rt = Bridge::spawn(&[]);
    let result = rt.run(&llm_call(&endpoint.url(), "$ENV:MY_LLM_KEY"));
    assert!(!result.contains("auth refused"), "{result}");
    assert!(
        endpoint.saw("authorization: Bearer named-FAKE1014-rt"),
        "{:?}",
        endpoint.requests()
    );
}

/// Review of the pre-release build (S2): a proxy saw what was meant for a loopback model.
#[test]
fn a_loopback_model_call_skips_the_proxy() {
    let proxy = Listener::start();
    let model = Listener::start();
    let mut rt = Bridge::spawn(&[("http_proxy", proxy.url()), ("HTTP_PROXY", proxy.url())]);
    let result = rt.run(&llm_call(&model.url(), "$ENV:MY_LLM_KEY"));
    assert!(model.saw("ping-1014"), "{result} {:?}", model.requests());
    assert_eq!(proxy.requests(), Vec::<String>::new(), "{result}");
}
