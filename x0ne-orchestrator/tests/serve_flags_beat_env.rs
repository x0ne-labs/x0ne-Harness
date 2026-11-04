//! `x0ne-orchestrator --serve` resolves the file, the environment, then
//! `--provider`, and `/info` names the provider that runs. The real binary;
//! local listeners stand in for the runtime and for a decoy Ollama.

#![cfg(feature = "serve")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token-970";

/// A loopback HTTP endpoint that answers `status` and `body` to everything
/// and counts the requests.
struct Stub {
    port: u16,
    hits: Arc<Mutex<usize>>,
}

impl Stub {
    fn start(status: &'static str, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let hits = Arc::new(Mutex::new(0));
        let count = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                if !matches!(stream.read(&mut buf), Ok(n) if n > 0) {
                    continue;
                }
                *count.lock().unwrap() += 1;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { port, hits }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn hits(&self) -> usize {
        *self.hits.lock().unwrap()
    }
}

/// A child process killed when dropped, a failed assertion included.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `GET /info` with the bearer: the response, once it is a 200.
fn info(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET /info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    out.starts_with("HTTP/1.1 200").then_some(out)
}

/// `-c` names Ollama at the decoy; the cases name openai-compat by
/// `X0NE_LLM_PROVIDER` alone, or by `--provider` over `X0NE_LLM_PROVIDER=ollama`.
#[test]
fn serve_puts_a_flag_over_the_environment_over_the_file() {
    let cases: [(&str, &str, &[&str]); 2] = [
        (
            "a flag beats the environment",
            "ollama",
            &["--provider", "openai-compat"],
        ),
        (
            "without a flag the environment beats -c",
            "openai-compat",
            &[],
        ),
    ];
    for (what, var, flags) in cases {
        let decoy = Stub::start("503 Service Unavailable", r#"{"error":"decoy"}"#);
        let compat = Stub::start("503 Service Unavailable", r#"{"error":"down"}"#);
        let runtime = Stub::start("200 OK", r#"{"success":true,"output":{"actions":[]}}"#);
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            format!(
                "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\nendpoint = \"{}\"\n\n\
                 [llm.openai_compat]\nbase_url = \"{}\"\nmodel = \"m\"\n",
                decoy.url(),
                compat.url()
            ),
        )
        .expect("config");
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let bind = format!("127.0.0.1:{port}");
        let child = Command::new(env!("CARGO_BIN_EXE_x0ne-orchestrator"))
            .args([
                "--serve",
                "--bind",
                &bind,
                "-c",
                &file.display().to_string(),
            ])
            .args(flags)
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("X0NE_CORE_ADDR", format!("127.0.0.1:{}", runtime.port))
            .env("X0NE_SERVE_TOKEN", TOKEN)
            .env("X0NE_LLM_PROVIDER", var)
            .env("X0NE_BRIDGE_MODE", "sandbox")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn x0ne-orchestrator --serve");
        let mut serve = Daemon(child);
        let deadline = Instant::now() + Duration::from_secs(60);
        let info = loop {
            assert_eq!(decoy.hits(), 0, "{what}: the decoy Ollama was called");
            if let Some(info) = info(port) {
                break info;
            }
            if let Some(status) = serve.0.try_wait().expect("try_wait") {
                panic!("{what}: --serve ended: {status}");
            }
            assert!(Instant::now() < deadline, "{what}: no /info");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(
            info.contains("\"provider\":\"openai-compat\""),
            "{what}: {info}"
        );
    }
}
