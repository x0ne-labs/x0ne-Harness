//! A loopback model, a recording stdio runtime and an isolated HOME for
//! driving the `x0ne` binary end to end. Every key and answer is fake.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// An OpenAI-compatible endpoint on 127.0.0.1 that answers every chat
/// completion with `answer`.
pub struct Model {
    pub port: u16,
}

impl Model {
    pub fn start(answer: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let body = if path.starts_with("/v1/models") {
                    r#"{"data":[]}"#.to_string()
                } else {
                    serde_json::json!({
                        "choices": [{"index": 0, "finish_reason": "stop",
                                     "message": {"role": "assistant", "content": answer}}],
                        "usage": {"prompt_tokens": 5, "completion_tokens": 1}
                    })
                    .to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { port }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return (!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned());
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
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A HOME, a working directory, a config naming the model, and a stdio
/// runtime that answers every action with success.
pub struct Fixture {
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new(model: &Model) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["home/.x0ne", "home/.config/x0ne", "work", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        let fx = Self { dir };
        std::fs::write(
            fx.path("chosen.toml"),
            format!(
                "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\nbase_url = \"{}\"\n\
                 model = \"chosen-model\"\n\n[bridge]\nmode = \"stdio\"\n",
                model.url()
            ),
        )
        .expect("config");
        let runtime = fx.path("bin/stdio-runtime");
        std::fs::write(
            &runtime,
            "#!/bin/sh\nwhile read -r line; do echo '{\"success\":true,\"output\":{\"actions\":[]}}'; done\n",
        )
        .expect("runtime");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        fx
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// `x0ne <args>` from `cwd`, in the isolated HOME; stdio left to the caller.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let home = self.path("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("X0NE_RUNTIME_BIN", self.path("bin/stdio-runtime"))
            .env("X0NE_NO_INTRO", "true")
            .env("X0NE_LOG", "error")
            .env("TERM", "dumb");
        cmd
    }

    /// `x0ne <args>` from `cwd`, on pipes, killed after `limit`.
    pub fn run(&self, cwd: &Path, args: &[&str], limit: Duration) -> Output {
        self.run_with_input(cwd, args, "", limit)
    }

    /// [`Fixture::run`], `input` written to its stdin, which then closes.
    #[allow(dead_code)] // not every test binary types into the REPL
    pub fn run_with_input(
        &self,
        cwd: &Path,
        args: &[&str],
        input: &str,
        limit: Duration,
    ) -> Output {
        let mut cmd = self.command(cwd, args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn x0ne");
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("stdin");
            let _ = stdin.write_all(input.as_bytes());
        }
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        child.wait_with_output().expect("output")
    }

    /// `x0ne -c chosen.toml -p <prompt> <extra>` from `cwd`.
    pub fn one_shot(&self, cwd: &Path, prompt: &str, extra: &[&str]) -> Output {
        let cfg = self.path("chosen.toml").display().to_string();
        let mut args = vec!["-c", cfg.as_str(), "-p", prompt];
        args.extend_from_slice(extra);
        self.run(cwd, &args, Duration::from_secs(90))
    }
}
