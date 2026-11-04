//! review: attacks on the config each surface runs on. Local listeners
//! stand in for the model, the attacker and the runtime, and record every
//! request; every key is fake. A test marked `ignore` is red and names the
//! task that will fix it.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const BOOT: Duration = Duration::from_secs(60);
/// An Anthropic key: it may only ever reach api.anthropic.com.
const ANT: &str = "sk-ant-FAKE-970review";
/// The decoy `~/.x0ne/config.toml` names this key; it must never leave.
const DECOY_KEY: &str = "sk-or-DECOY-970review";
const PONG: &str = "pong-970";
const RUNTIME_OK: &str = r#"{"success":true,"output":{"actions":[]}}"#;
const TOKEN: &str = "tok-970-review";

#[derive(Clone, Copy, Debug)]
enum Reply {
    /// OpenAI and Ollama shaped answers, by path.
    Pong,
    /// Reads the request, answers nothing, records when the client hangs up.
    Silence,
    /// HTTP 401 with a JSON error body.
    Unauthorized,
    /// An HTTP runtime that accepts everything.
    Runtime,
}

/// A local endpoint that records each request, headers included.
struct Endpoint {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    hangups: Arc<Mutex<Vec<Instant>>>,
}

impl Endpoint {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hangups = Arc::new(Mutex::new(Vec::new()));
        let (log, gone) = (Arc::clone(&seen), Arc::clone(&hangups));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                let gone = Arc::clone(&gone);
                std::thread::spawn(move || answer(stream, reply, &request, &gone));
            }
        });
        Self {
            port,
            seen,
            hangups,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn hangups(&self) -> Vec<Instant> {
        self.hangups.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
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

fn answer(mut stream: TcpStream, reply: Reply, request: &str, gone: &Mutex<Vec<Instant>>) {
    let path = request.split_whitespace().nth(1).unwrap_or("");
    let body = match reply {
        Reply::Silence => {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let started = Instant::now();
            let mut b = [0u8; 64];
            while started.elapsed() < Duration::from_secs(600) {
                match stream.read(&mut b) {
                    Ok(0) => {
                        gone.lock().unwrap().push(Instant::now());
                        return;
                    }
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => {
                        gone.lock().unwrap().push(Instant::now());
                        return;
                    }
                }
            }
            return;
        }
        Reply::Unauthorized => {
            return respond(
                stream,
                "401 Unauthorized",
                r#"{"error":{"message":"invalid api key"}}"#,
            )
        }
        Reply::Runtime => RUNTIME_OK.to_string(),
        Reply::Pong if path.starts_with("/api/tags") => {
            r#"{"models":[{"name":"chosen-model"}]}"#.to_string()
        }
        Reply::Pong if path.starts_with("/api/chat") => format!(
            r#"{{"model":"chosen-model","message":{{"role":"assistant","content":"{PONG}"}},"done":true}}"#
        ),
        Reply::Pong if path.starts_with("/v1/models") => r#"{"data":[]}"#.to_string(),
        Reply::Pong => format!(
            r#"{{"choices":[{{"index":0,"finish_reason":"stop","message":{{"role":"assistant","content":"{PONG}"}}}}],"usage":{{"prompt_tokens":5,"completion_tokens":1}}}}"#
        ),
    };
    respond(stream, "200 OK", &body);
}

fn respond(mut stream: TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn arg(p: &Path) -> String {
    p.display().to_string()
}

fn exe(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(p, text).expect("write script");
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A HOME with a decoy `~/.x0ne/config.toml`, a working directory, two
/// recording runtimes and, on demand, a fake `incus` that records its calls.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(decoy: &Endpoint, chosen: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["home/.x0ne", "work", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        let fx = Self { dir };
        std::fs::write(
            fx.path("home/.x0ne/config.toml"),
            format!(
                "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\nbase_url = \"{}\"\n\
                 model = \"decoy-model\"\napi_key = \"{DECOY_KEY}\"\n",
                decoy.url()
            ),
        )
        .expect("decoy config");
        std::fs::write(fx.chosen(), chosen).expect("chosen config");
        std::fs::write(fx.path("work/x0ne-runtime.toml"), "").expect("policy");
        for name in ["stdio-runtime", "sandbox-runtime"] {
            exe(
                &fx.path(&format!("bin/{name}")),
                &format!(
                    "#!/bin/sh\necho {name} >> '{}'\nwhile read -r line; do echo '{RUNTIME_OK}'; done\n",
                    fx.path("runtime-started").display()
                ),
            );
        }
        fx
    }

    /// A fake `incus` on PATH: `x0ne-sandbox` exists, every call is logged.
    fn with_incus(self) -> Self {
        exe(
            &self.path("bin/incus"),
            &format!(
                "#!/bin/sh\necho \"incus $*\" >> '{log}'\ncase \"$1\" in\n  --version) echo 6.0.4; exit 0;;\n  \
                 list) echo x0ne-sandbox; exit 0;;\n  exec) echo incus-exec >> '{rt}'; \
                 while read -r line; do echo '{RUNTIME_OK}'; done;;\nesac\nexit 0\n",
                log = self.path("incus.log").display(),
                rt = self.path("runtime-started").display()
            ),
        );
        self
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn chosen(&self) -> PathBuf {
        self.path("chosen.toml")
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_default()
    }

    fn env(&self) -> Vec<(String, String)> {
        let home = self.path("home");
        let h = |rel: &str| home.join(rel).display().to_string();
        vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", arg(&self.path("bin"))),
            ),
            ("HOME".into(), arg(&home)),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_STATE_HOME".into(), h(".local/state")),
            ("XDG_CACHE_HOME".into(), h(".cache")),
            (
                "X0NE_RUNTIME_BIN".into(),
                arg(&self.path("bin/stdio-runtime")),
            ),
            (
                "X0NE_SANDBOX_CMD".into(),
                arg(&self.path("bin/sandbox-runtime")),
            ),
            ("X0NE_NO_INTRO".into(), "true".into()),
            ("X0NE_LOG".into(), "error".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]
    }

    fn command(&self, args: &[String], vars: &[(String, String)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(args)
            .current_dir(self.path("work"))
            .env_clear()
            .envs(self.env())
            .envs(vars.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// The binary on pipes, run to its end or killed after `limit`.
    fn run(&self, args: &[String], vars: &[(String, String)], limit: Duration) -> Run {
        let mut child = self.command(args, vars).spawn().expect("spawn x0ne");
        let stdout = drain(child.stdout.take().expect("stdout"));
        let stderr = drain(child.stderr.take().expect("stderr"));
        let input = child.stdin.take();
        let code = wait(&mut child, limit);
        drop(input);
        Run {
            code,
            out: format!(
                "{}{}",
                stdout.join().expect("stdout"),
                stderr.join().expect("stderr")
            ),
        }
    }

    /// `x0ne --acp`: initialize, session/new, then one prompt.
    fn acp(&self, args: &[String], vars: &[(String, String)]) -> Run {
        let mut child = self.command(args, vars).spawn().expect("spawn x0ne --acp");
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (sink, eof) = (Arc::clone(&lines), Arc::clone(&closed));
        let stdout = child.stdout.take().expect("stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
            eof.store(true, Ordering::SeqCst);
        });
        let stderr = drain(child.stderr.take().expect("stderr"));
        let mut input = child.stdin.take().expect("stdin");
        let reply_to = |id: u64, limit: Duration| -> Option<serde_json::Value> {
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                let done = closed.load(Ordering::SeqCst);
                let found = lines.lock().unwrap().iter().find_map(|l| {
                    serde_json::from_str::<serde_json::Value>(l)
                        .ok()
                        .filter(|v| v["id"] == id)
                });
                // Stdout at EOF and no reply: none can come. A refused start
                // waited the full 20 s twice here (2026-10-27).
                if found.is_some() || done {
                    return found;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        };
        let cwd = self.path("work").canonicalize().expect("cwd");
        let send = |w: &mut std::process::ChildStdin, v: serde_json::Value| {
            let _ = writeln!(w, "{v}");
            let _ = w.flush();
        };
        send(
            &mut input,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
        );
        reply_to(1, Duration::from_secs(20));
        send(
            &mut input,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}),
        );
        let session = reply_to(2, Duration::from_secs(20))
            .and_then(|v| v["result"]["sessionId"].as_str().map(str::to_string));
        if let Some(id) = session {
            send(
                &mut input,
                serde_json::json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
                    "params":{"sessionId":id,"prompt":[{"type":"text","text":"hello"}]}}),
            );
            reply_to(3, Duration::from_secs(20));
        }
        drop(input);
        let code = wait(&mut child, Duration::from_secs(10));
        let out = format!(
            "{}{}",
            lines.lock().unwrap().join("\n"),
            stderr.join().expect("stderr")
        );
        Run { code, out }
    }

    /// `x0ne --serve`: wait for `/info`, then one `POST /prompt`.
    fn serve(&self, args: &[String], vars: &[(String, String)], runtime: &Endpoint) -> Run {
        let port = free_port();
        let mut argv = vec![
            "--serve".to_string(),
            "--bind".to_string(),
            format!("127.0.0.1:{port}"),
        ];
        argv.extend(args.iter().cloned());
        let mut vars = vars.to_vec();
        vars.push((
            "X0NE_CORE_ADDR".into(),
            format!("127.0.0.1:{}", runtime.port),
        ));
        vars.push(("X0NE_SERVE_TOKEN".into(), TOKEN.into()));
        let mut child = self.command(&argv, &vars).spawn().expect("spawn --serve");
        let stderr = drain(child.stderr.take().expect("stderr"));
        let _stdout = drain(child.stdout.take().expect("stdout"));
        let deadline = Instant::now() + BOOT;
        let mut code = None;
        while Instant::now() < deadline {
            if http(port, "GET", "/info", "").is_some_and(|r| r.starts_with("HTTP/1.1 200")) {
                let _ = http(port, "POST", "/prompt", r#"{"prompt":"hello"}"#);
                break;
            }
            if let Some(status) = child.try_wait().expect("try_wait") {
                code = status.code();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        Run {
            code,
            out: stderr.join().expect("stderr"),
        }
    }

    /// The binary in a 140x40 pseudo-terminal.
    fn screen(&self, args: &[String], vars: &[(String, String)]) -> Screen {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(args);
        cmd.cwd(self.path("work"));
        cmd.env_clear();
        for (k, v) in self.env().into_iter().chain(vars.iter().cloned()) {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn x0ne");
        drop(pair.slave);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(40, 140, 0)));
        let raw = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let (sink, all) = (Arc::clone(&parser), Arc::clone(&raw));
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buf[..n]);
                all.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        Screen {
            parser,
            raw,
            writer,
            child,
            _master: pair.master,
        }
    }
}

struct Run {
    code: Option<i32>,
    out: String,
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    })
}

fn wait(child: &mut std::process::Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port()
}

fn http(port: u16, method: &str, path: &str, body: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\n\
         X-X0ne-Session: review-970\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

struct Screen {
    parser: Arc<Mutex<vt100::Parser>>,
    /// Everything written to the terminal, alternate screen included; bytes,
    /// so that a character split across two reads stays whole.
    raw: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl Screen {
    fn text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().unwrap()).into_owned()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    fn wait_for(&self, what: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.text().contains(what) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| (*a).to_string()).collect()
}

fn compat(url: &str, extra: &str) -> String {
    format!(
        "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\nbase_url = \"{url}\"\n\
         model = \"chosen-model\"\n{extra}\n"
    )
}

/// One way to put an Anthropic key next to a host that is not Anthropic's.
struct Attack {
    what: &'static str,
    file: String,
    vars: Vec<(String, String)>,
    flags: Vec<String>,
}

/// `att`: the attacker's endpoint. `legit`: the one the operator meant.
fn attacks(att: &str, legit: &str) -> Vec<Attack> {
    let to_att = |extra: &[&str]| {
        let mut f = args(&["--provider", "openai-compat", "--endpoint", att]);
        f.extend(args(extra));
        f
    };
    vec![
        Attack {
            what: "-c names the attacker with an Anthropic key in the file",
            file: compat(att, &format!("api_key = \"{ANT}\"")),
            vars: vec![],
            flags: vec![],
        },
        Attack {
            what: "ANTHROPIC_API_KEY set, the flags move to openai-compat at the attacker",
            file: "[llm]\nprovider = \"anthropic\"\n".into(),
            vars: vars(&[("ANTHROPIC_API_KEY", ANT)]),
            flags: to_att(&[]),
        },
        Attack {
            what: "X0NE_OPENAI_COMPAT_KEY holds the Anthropic key",
            file: compat(legit, ""),
            vars: vars(&[("X0NE_OPENAI_COMPAT_KEY", ANT)]),
            flags: to_att(&[]),
        },
        Attack {
            what: "OPENAI_API_KEY holds the Anthropic key",
            file: compat(legit, ""),
            vars: vars(&[("OPENAI_API_KEY", ANT)]),
            flags: to_att(&[]),
        },
        Attack {
            what: "OLLAMA_BASE_URL at the attacker, OLLAMA_API_KEY is Anthropic's",
            file: compat(legit, ""),
            vars: vars(&[("OLLAMA_BASE_URL", att), ("OLLAMA_API_KEY", ANT)]),
            flags: args(&["--provider", "ollama"]),
        },
        Attack {
            what: "-c: ollama at the attacker reads ANTHROPIC_API_KEY",
            file: format!(
                "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\nendpoint = \"{att}\"\n\
                 model = \"chosen-model\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n"
            ),
            vars: vars(&[("ANTHROPIC_API_KEY", ANT)]),
            flags: vec![],
        },
        Attack {
            what: "the environment says anthropic, the flag (now first) says the attacker",
            file: compat(legit, ""),
            vars: vars(&[
                ("X0NE_LLM_PROVIDER", "anthropic"),
                ("ANTHROPIC_API_KEY", ANT),
            ]),
            flags: to_att(&[]),
        },
        Attack {
            what: "all from the environment: compat at the attacker, Anthropic compat key",
            file: "[llm]\nprovider = \"anthropic\"\n".into(),
            vars: vars(&[
                ("X0NE_LLM_PROVIDER", "openai-compat"),
                ("X0NE_OPENAI_COMPAT_BASE_URL", att),
                ("X0NE_OPENAI_COMPAT_KEY", ANT),
            ]),
            flags: vec![],
        },
        Attack {
            what: "userinfo naming api.anthropic.com in --endpoint",
            file: compat(legit, &format!("api_key = \"{ANT}\"")),
            vars: vec![],
            flags: args(&[
                "--provider",
                "openai-compat",
                "--endpoint",
                &att.replace("http://", "http://api.anthropic.com@"),
            ]),
        },
    ]
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    OneShot,
    Tui,
    Matrix,
    Acp,
    Serve,
}

/// Runs `attack` on `surface`; panics if an Anthropic key or the decoy's
/// key reached any listener, or if the decoy was called.
fn assert_no_key_leaves(surface: Surface, attack: &Attack) {
    let attacker = Endpoint::start(Reply::Pong);
    let legit = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let runtime = Endpoint::start(Reply::Runtime);
    let mut all = attacks(&attacker.url(), &legit.url());
    let index = all
        .iter()
        .position(|a| a.what == attack.what)
        .expect("attack");
    let attack = all.swap_remove(index);
    let fx = Fixture::new(&decoy, &attack.file);
    let mut argv = vec!["-c".to_string(), arg(&fx.chosen())];
    argv.extend(attack.flags.iter().cloned());
    let seen = match surface {
        Surface::OneShot => {
            argv.extend(args(&["-p", "hello"]));
            fx.run(&argv, &attack.vars, BOOT).out
        }
        Surface::Acp => {
            argv.insert(0, "--acp".into());
            fx.acp(&argv, &attack.vars).out
        }
        Surface::Serve => fx.serve(&argv, &attack.vars, &runtime).out,
        Surface::Tui | Surface::Matrix => {
            let flag = if matches!(surface, Surface::Tui) {
                "--tui"
            } else {
                "--matrix"
            };
            argv.insert(0, flag.into());
            let mut screen = fx.screen(&argv, &attack.vars);
            std::thread::sleep(Duration::from_secs(6));
            screen.send(if matches!(surface, Surface::Tui) {
                "hello\r"
            } else {
                "1\r"
            });
            let settle = if matches!(surface, Surface::Tui) {
                8
            } else {
                40
            };
            std::thread::sleep(Duration::from_secs(settle));
            screen.raw()
        }
    };
    let what = attack.what;
    assert!(
        decoy.requests().is_empty(),
        "{surface:?}, {what}: the decoy was called\n{seen}"
    );
    for (name, ep) in [
        ("attacker", &attacker),
        ("legit", &legit),
        ("decoy", &decoy),
    ] {
        for r in ep.requests() {
            assert!(
                !r.contains("sk-ant-") && !r.contains(DECOY_KEY),
                "{surface:?}, {what}: a key reached the {name} endpoint:\n{r}\n{seen}"
            );
        }
    }
}

/// Every surface, every attack: no Anthropic key reaches a foreign host,
/// and the decoy `~/.x0ne/config.toml` is never what runs.
#[test]
fn no_anthropic_key_leaves_from_the_one_shot() {
    for a in attacks("http://att", "http://legit") {
        assert_no_key_leaves(Surface::OneShot, &a);
    }
}

#[test]
fn no_anthropic_key_leaves_from_the_tui() {
    all_at_once(Surface::Tui, &attacks("http://att", "http://legit"));
}

#[test]
fn no_anthropic_key_leaves_from_acp() {
    for a in attacks("http://att", "http://legit") {
        assert_no_key_leaves(Surface::Acp, &a);
    }
}

#[test]
fn no_anthropic_key_leaves_from_serve() {
    for a in attacks("http://att", "http://legit") {
        assert_no_key_leaves(Surface::Serve, &a);
    }
}

/// Matrix waits ~40 s for its entrance: two attacks only.
#[test]
fn no_anthropic_key_leaves_from_matrix() {
    let two: Vec<Attack> = attacks("http://att", "http://legit")
        .into_iter()
        .filter(|a| a.what.starts_with("-c names") || a.what.starts_with("all from"))
        .collect();
    all_at_once(Surface::Matrix, &two);
}

/// [`assert_no_key_leaves`] for each attack, all at once: each run owns its
/// listeners, fixture and child, and keeps its waits. One by one, the TUI
/// took 126 s and the matrix 92 s (2026-10-27).
fn all_at_once(surface: Surface, list: &[Attack]) {
    std::thread::scope(|s| {
        let runs: Vec<_> = list
            .iter()
            .map(|a| s.spawn(move || assert_no_key_leaves(surface, a)))
            .collect();
        for run in runs {
            if let Err(panic) = run.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
}

/// `X0NE_REQUIRE_SANDBOX` is checked on the mode that runs: `--mode stdio`
/// now beats `X0NE_BRIDGE_MODE=sandbox`, and is refused on every surface
/// before any runtime or model.
#[test]
fn require_sandbox_refuses_the_mode_a_flag_chose() {
    let model = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let runtime = Endpoint::start(Reply::Runtime);
    let surfaces: [&[&str]; 4] = [&[], &["--tui"], &["--matrix"], &["--acp"]];
    for surface in surfaces {
        let fx = Fixture::new(&decoy, &compat(&model.url(), "")).with_incus();
        let mut argv = args(surface);
        argv.extend(args(&["-c", &arg(&fx.chosen()), "--mode", "stdio"]));
        if surface.is_empty() {
            argv.extend(args(&["-p", "hello"]));
        }
        let env = vars(&[
            ("X0NE_REQUIRE_SANDBOX", "1"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
            (
                "X0NE_SANDBOX_CMD",
                "incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode stdio",
            ),
        ]);
        let run = fx.run(&argv, &env, Duration::from_secs(30));
        assert_eq!(run.code, Some(1), "{surface:?}:\n{}", run.out);
        assert!(
            run.out.contains("X0NE_REQUIRE_SANDBOX is set"),
            "{surface:?}:\n{}",
            run.out
        );
        assert_eq!(fx.read("runtime-started"), "", "{surface:?}");
        assert!(model.requests().is_empty(), "{surface:?}: a model call");
    }
    let fx = Fixture::new(&decoy, &compat(&model.url(), ""));
    let run = fx.serve(
        &args(&["-c", &arg(&fx.chosen()), "--mode", "stdio"]),
        &vars(&[("X0NE_REQUIRE_SANDBOX", "1")]),
        &runtime,
    );
    assert!(
        run.out.contains("X0NE_REQUIRE_SANDBOX is set"),
        "{}",
        run.out
    );
}

/// Ctrl+C during a silent turn closes the model connection at once and no
/// other request follows; the TUI stays open.
#[test]
fn ctrl_c_leaves_no_request_running() {
    let model = Endpoint::start(Reply::Silence);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, &compat(&model.url(), ""));
    let mut screen = fx.screen(&args(&["--tui", "-c", &arg(&fx.chosen())]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    screen.send("hello\r");
    let deadline = Instant::now() + BOOT;
    while model.requests().is_empty() {
        assert!(Instant::now() < deadline, "no request:\n{}", screen.text());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(1));
    let pressed = Instant::now();
    screen.send("\x03");
    std::thread::sleep(Duration::from_secs(5));
    let hangups = model.hangups();
    assert_eq!(
        hangups.len(),
        1,
        "connection still open:\n{}",
        screen.text()
    );
    assert!(hangups[0].saturating_duration_since(pressed) < Duration::from_secs(2));
    assert_eq!(model.requests().len(), 1, "a request after Ctrl+C");
    assert!(screen.alive(), "Ctrl+C quit the TUI");
}

/// The new error lines name the failure, never the key nor the request.
#[test]
fn a_model_error_shows_no_key_and_no_request() {
    let model = Endpoint::start(Reply::Unauthorized);
    let decoy = Endpoint::start(Reply::Pong);
    let key = "sk-or-FAKE-970-errln";
    let fx = Fixture::new(
        &decoy,
        &compat(&model.url(), &format!("api_key = \"{key}\"")),
    );
    let mut screen = fx.screen(&args(&["--tui", "-c", &arg(&fx.chosen())]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    screen.send("hello-secret-prompt\r");
    assert!(
        screen.wait_for("model error", BOOT),
        "no error line:\n{}",
        screen.text()
    );
    let raw = screen.raw().to_ascii_lowercase();
    for leak in ["fake-970-errln", "bearer", "authorization", "\"messages\""] {
        assert!(
            !raw.contains(leak),
            "`{leak}` on screen:\n{}",
            screen.text()
        );
    }
}

/// The header ("provider/model @ url") and the timeout line show
/// `base_url` with its userinfo masked. Red on the pre-release build: the password was
/// on screen.
#[test]
fn url_credentials_stay_off_the_screen() {
    let model = Endpoint::start(Reply::Silence);
    let decoy = Endpoint::start(Reply::Pong);
    let url = model.url().replace("http://", "http://user:hunter2@");
    let fx = Fixture::new(&decoy, &compat(&url, "\n[ui]\nmodel_timeout_secs = 2\n"));
    let mut screen = fx.screen(&args(&["--tui", "-c", &arg(&fx.chosen())]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    screen.send("hello\r");
    assert!(screen.wait_for("timeout", BOOT), "{}", screen.text());
    assert!(!screen.raw().contains("hunter2"), "{}", screen.text());
}

/// Review of the pre-release build (B1): reqwest trims a leading space, the text cut did
/// not, so the header and the timeout line showed the password.
#[test]
fn a_spaced_url_keeps_its_password_off_the_screen() {
    let model = Endpoint::start(Reply::Silence);
    let decoy = Endpoint::start(Reply::Pong);
    let url = model.url().replace("http://", " http://user:hunter2@");
    let fx = Fixture::new(&decoy, &compat(&url, "\n[ui]\nmodel_timeout_secs = 2\n"));
    let mut screen = fx.screen(&args(&["--tui", "-c", &arg(&fx.chosen())]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    screen.send("hello\r");
    assert!(screen.wait_for("timeout", BOOT), "{}", screen.text());
    assert!(!screen.raw().contains("hunter2"), "{}", screen.text());
}

/// Red on the pre-release build: autowrap probes before it decides, so a stdio session
/// with `incus` on PATH runs `incus start x0ne-sandbox`, unannounced; the TUI
/// and matrix do it too. Fix: probe only when the decision needs it
/// (autowrap.rs:306-316).
#[test]
#[ignore = "open defect: autowrap probes before it decides"]
fn a_stdio_session_starts_no_container() {
    let model = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, &compat(&model.url(), "")).with_incus();
    let argv = args(&["-c", &arg(&fx.chosen()), "--mode", "stdio", "-p", "hello"]);
    let run = fx.run(&argv, &[], BOOT);
    assert!(
        !fx.read("incus.log").contains("start"),
        "incus calls:\n{}\n{}",
        fx.read("incus.log"),
        run.out
    );
}

/// `--acp` resolves `--endpoint` and `--model` with the rest of the
/// config. Red on the pre-release build: it dropped them and talked to the `-c` endpoint.
#[test]
fn acp_never_talks_to_an_endpoint_the_flags_replaced() {
    let replaced = Endpoint::start(Reply::Pong);
    let named = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, &compat(&replaced.url(), ""));
    let argv = args(&[
        "--acp",
        "-c",
        &arg(&fx.chosen()),
        "--endpoint",
        &named.url(),
    ]);
    let run = fx.acp(&argv, &[]);
    assert!(
        replaced.requests().is_empty(),
        "the replaced endpoint got the prompt:\n{}",
        run.out
    );
}

/// Red on the pre-release builds: with no key in `[llm.openai_compat]`, the
/// session falls back to `OPENAI_API_KEY` and sends it to whatever host
/// `base_url` names. Fix: take that fallback only for api.openai.com
/// (llm_setup.rs:104-109). Red on the pre-release build, green with the fix.
#[test]
fn an_openai_key_stays_with_openai() {
    let other = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let key = "sk-proj-FAKE-970-oai";
    let fx = Fixture::new(&decoy, &compat(&other.url(), ""));
    let argv = args(&["-c", &arg(&fx.chosen()), "-p", "hello"]);
    let run = fx.run(&argv, &vars(&[("OPENAI_API_KEY", key)]), BOOT);
    for r in other.requests() {
        assert!(
            !r.contains(key),
            "the OpenAI key left for {}:\n{r}\n{}",
            other.url(),
            run.out
        );
    }
    assert_refused_before_any_request(&other, &run.out, "OPENAI_API_KEY", key);
}

/// The refusal comes before any request, says which line to add,
/// and shows no more of the key than its last four characters.
fn assert_refused_before_any_request(ep: &Endpoint, out: &str, var: &str, key: &str) {
    assert!(
        ep.requests().is_empty(),
        "a request left before the refusal:\n{:?}\n{out}",
        ep.requests()
    );
    assert!(
        out.contains(&format!(
            "name the key in the config: api_key_env = \"{var}\""
        )),
        "no refusal naming the line to add:\n{out}"
    );
    assert!(
        !out.contains(&key[..key.len() - 4]),
        "the key shows:\n{out}"
    );
}

/// The `OLLAMA_API_KEY` row: read without the config naming it, the
/// key goes to ollama.com only. Red on the pre-release build: the probe, then the chat
/// with the key, went to the loopback endpoint.
#[test]
fn an_ollama_key_stays_with_ollama_com() {
    let local = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let key = "ollama-FAKE-1009-key";
    let fx = Fixture::new(
        &decoy,
        &format!(
            "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\nendpoint = \"{}\"\n\
             model = \"chosen-model\"\n",
            local.url()
        ),
    );
    let argv = args(&["-c", &arg(&fx.chosen()), "-p", "hello"]);
    let run = fx.run(&argv, &vars(&[("OLLAMA_API_KEY", key)]), BOOT);
    assert_refused_before_any_request(&local, &run.out, "OLLAMA_API_KEY", key);
    assert!(decoy.requests().is_empty(), "the decoy was called");
}

/// An `sk-ant-` key named in the config, inline or by
/// `api_key_env`, never reaches a host that is not Anthropic's. The inline
/// case is green on the pre-release build (the vendor-host guard); the `api_key_env` case was
/// green there only because `[llm.openai_compat] api_key_env` was not read.
#[test]
fn a_named_anthropic_key_never_leaves_anthropic() {
    for named in [
        format!("api_key = \"{ANT}\""),
        "api_key_env = \"MY_ANT_KEY\"".to_string(),
    ] {
        let third = Endpoint::start(Reply::Pong);
        let decoy = Endpoint::start(Reply::Pong);
        let fx = Fixture::new(&decoy, &compat(&third.url(), &named));
        let argv = args(&["-c", &arg(&fx.chosen()), "-p", "hello"]);
        let run = fx.run(&argv, &vars(&[("MY_ANT_KEY", ANT)]), BOOT);
        assert!(
            third.requests().is_empty(),
            "{named}: a request left:\n{:?}\n{}",
            third.requests(),
            run.out
        );
        assert!(
            run.out.contains("refusing to send an Anthropic key"),
            "{named}: no refusal:\n{}",
            run.out
        );
        assert!(
            !run.out.contains("FAKE-970review"),
            "{named}: the key shows"
        );
    }
}

/// A key the config names goes to the config's host, and `surface`
/// says once at startup where it came from and where it goes, never the key.
/// Red on the pre-release build: no such line, and `api_key_env` was not read.
fn assert_the_named_key_is_announced(surface: Surface) {
    let model = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let runtime = Endpoint::start(Reply::Runtime);
    let key = "sk-proj-FAKE-970-oai";
    let fx = Fixture::new(
        &decoy,
        &compat(&model.url(), "api_key_env = \"OPENAI_API_KEY\""),
    );
    let env = vars(&[("OPENAI_API_KEY", key)]);
    let mut argv = args(&["-c", &arg(&fx.chosen())]);
    let out = match surface {
        Surface::OneShot => {
            argv.extend(args(&["-p", "hello"]));
            fx.run(&argv, &env, BOOT).out
        }
        Surface::Acp => {
            argv.insert(0, "--acp".into());
            fx.acp(&argv, &env).out
        }
        Surface::Serve => fx.serve(&argv, &env, &runtime).out,
        Surface::Matrix => {
            // Matrix asks what to build, then connects to the model.
            argv.insert(0, "--matrix".into());
            let mut screen = fx.screen(&argv, &env);
            assert!(
                screen.wait_for("select", BOOT),
                "no menu:\n{}",
                screen.text()
            );
            screen.send("1\r");
            let deadline = Instant::now() + BOOT;
            while !screen.raw().contains("key from") && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            screen.raw()
        }
        Surface::Tui => {
            panic!("the TUI case is the_tui_takes_an_endpoint_flag_over_ollama_base_url")
        }
    };
    let line = format!("key from OPENAI_API_KEY → 127.0.0.1:{}", model.port);
    assert_eq!(
        out.matches(&line).count(),
        1,
        "{surface:?}: not announced once ({line}):\n{out}"
    );
    assert!(
        !out.contains("FAKE-970"),
        "{surface:?}: the key shows:\n{out}"
    );
    if !matches!(surface, Surface::Matrix) {
        let sent = chats(&model);
        assert!(!sent.is_empty(), "{surface:?}: no chat:\n{out}");
        assert!(
            sent.iter().all(|r| r.contains(&format!("Bearer {key}"))),
            "{surface:?}: the named key was not sent:\n{sent:?}"
        );
    }
    assert!(
        decoy.requests().is_empty(),
        "{surface:?}: the decoy was called"
    );
}

#[test]
fn the_one_shot_announces_a_named_key() {
    assert_the_named_key_is_announced(Surface::OneShot);
}

#[test]
fn acp_announces_a_named_key() {
    assert_the_named_key_is_announced(Surface::Acp);
}

#[test]
fn serve_announces_a_named_key() {
    assert_the_named_key_is_announced(Surface::Serve);
}

#[test]
fn matrix_announces_a_named_key() {
    assert_the_named_key_is_announced(Surface::Matrix);
}

/// `OLLAMA_BASE_URL` is an environment layer and `--endpoint` beats
/// it. Red on the pre-release builds: the prompt went to the host the
/// environment named, not the one the command line named.
#[test]
fn an_endpoint_flag_beats_ollama_base_url() {
    let from_env = Endpoint::start(Reply::Pong);
    let named = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, "[llm]\nprovider = \"ollama\"\n");
    let argv = args(&[
        "-c",
        &arg(&fx.chosen()),
        "--provider",
        "ollama",
        "--endpoint",
        &named.url(),
        "--model",
        "chosen-model",
        "-p",
        "hello",
    ]);
    let env_url = from_env.url();
    let run = fx.run(&argv, &vars(&[("OLLAMA_BASE_URL", env_url.as_str())]), BOOT);
    assert!(
        from_env.requests().is_empty(),
        "the environment's host got the prompt:\n{}",
        run.out
    );
}

/// No provider anywhere: startup looks for Ollama where `--endpoint`
/// says, not at `OLLAMA_BASE_URL`, and the session runs there.
#[test]
fn startup_probes_the_endpoint_flag_not_ollama_base_url() {
    let from_env = Endpoint::start(Reply::Pong);
    let named = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, "[ui]\nconfirm_writes = true\n");
    let argv = args(&[
        "-c",
        &arg(&fx.chosen()),
        "--endpoint",
        &named.url(),
        "-p",
        "hello",
    ]);
    let env_url = from_env.url();
    let run = fx.run(&argv, &vars(&[("OLLAMA_BASE_URL", env_url.as_str())]), BOOT);
    assert!(
        from_env.requests().is_empty(),
        "the environment's host was called:\n{:?}\n{}",
        from_env.requests(),
        run.out
    );
    assert!(
        !chats(&named).is_empty(),
        "no chat at --endpoint:\n{}",
        run.out
    );
}

/// A model that cannot be reached: the error line names the
/// endpoint, its password masked, on `-p`, `--tui` and `--matrix`. Red on
/// the pre-release build: "Streaming request to http://user:hunter2@… failed".
#[test]
fn url_credentials_stay_off_every_error_line() {
    let decoy = Endpoint::start(Reply::Pong);
    let url = format!("http://user:hunter2@127.0.0.1:{}", free_port());
    let fx = Fixture::new(&decoy, &compat(&url, ""));
    let chosen = arg(&fx.chosen());
    let run = fx.run(&args(&["-c", &chosen, "-p", "hello"]), &[], BOOT);
    assert!(
        run.out.contains("***@127.0.0.1"),
        "-p: no error line:\n{}",
        run.out
    );
    assert!(!run.out.contains("hunter2"), "-p:\n{}", run.out);
    let mut tui = fx.screen(&args(&["--tui", "-c", &chosen]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    tui.send("hello\r");
    assert!(tui.wait_for("model error", BOOT), "--tui:\n{}", tui.text());
    assert!(!tui.raw().contains("hunter2"), "--tui:\n{}", tui.text());
    let mut matrix = fx.screen(&args(&["--matrix", "-c", &chosen]), &[]);
    std::thread::sleep(Duration::from_secs(6));
    matrix.send("1\r");
    assert!(
        matrix.wait_for("model error", Duration::from_secs(120)),
        "--matrix:\n{}",
        matrix.text()
    );
    matrix.send("q");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !matrix.raw().contains("hunter2"),
        "--matrix:\n{}",
        matrix.text()
    );
    assert!(decoy.requests().is_empty(), "the decoy was called");
}

/// The chat requests `ep` received: the prompt, not the probes.
fn chats(ep: &Endpoint) -> Vec<String> {
    ep.requests()
        .into_iter()
        .filter(|r| r.starts_with("POST /api/chat") || r.starts_with("POST /v1/chat/completions"))
        .collect()
}

/// `argv` on `surface` (`-p` or `--tui`) with one prompt, until `model`
/// has a chat request or [`BOOT`] runs out. Returns what the binary printed.
fn one_prompt(
    fx: &Fixture,
    surface: Surface,
    argv: &[String],
    env: &[(String, String)],
    model: &Endpoint,
) -> String {
    match surface {
        Surface::OneShot => {
            let mut argv = argv.to_vec();
            argv.extend(args(&["-p", "hello"]));
            fx.run(&argv, env, BOOT).out
        }
        Surface::Tui => {
            let mut full = args(&["--tui"]);
            full.extend(argv.iter().cloned());
            let mut screen = fx.screen(&full, env);
            std::thread::sleep(Duration::from_secs(6));
            screen.send("hello\r");
            let deadline = Instant::now() + BOOT;
            while chats(model).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(Duration::from_secs(1));
            screen.text()
        }
        other => panic!("one_prompt: {other:?} is not -p or --tui"),
    }
}

/// With no provider in `-c`, the session picks Ollama because it
/// answers at `OLLAMA_BASE_URL`, then applies `X0NE_MODEL` again; `--model`
/// must still name the model.
fn assert_the_model_flag_beats_x0ne_model(surface: Surface) {
    let model = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(&decoy, "[ui]\nconfirm_writes = true\n");
    let url = model.url();
    let env = vars(&[
        ("OLLAMA_BASE_URL", url.as_str()),
        ("X0NE_MODEL", "chosen-model"),
    ]);
    let argv = args(&["-c", &arg(&fx.chosen()), "--model", "flag-model"]);
    let out = one_prompt(&fx, surface, &argv, &env, &model);
    let sent = chats(&model);
    assert!(!sent.is_empty(), "{surface:?}: no chat request:\n{out}");
    for r in &sent {
        assert!(
            r.contains(r#""model":"flag-model""#),
            "{surface:?}: the chat did not name --model:\n{r}\n{out}"
        );
    }
    assert!(
        decoy.requests().is_empty(),
        "{surface:?}: the decoy was called"
    );
}

/// Red on the pre-release build: the chat named `X0NE_MODEL`'s model.
#[test]
fn the_one_shot_takes_a_model_flag_over_x0ne_model() {
    assert_the_model_flag_beats_x0ne_model(Surface::OneShot);
}

/// Red on the pre-release build: the chat named `X0NE_MODEL`'s model.
#[test]
fn the_tui_takes_a_model_flag_over_x0ne_model() {
    assert_the_model_flag_beats_x0ne_model(Surface::Tui);
}

/// `--tui`, same as `-p` in [`an_endpoint_flag_beats_ollama_base_url`]:
/// `--endpoint` beats `OLLAMA_BASE_URL`, and `OLLAMA_API_KEY` goes nowhere
/// else. Red on the pre-release build. Now the config names the key, and the TUI says
/// where it goes.
#[test]
fn the_tui_takes_an_endpoint_flag_over_ollama_base_url() {
    let from_env = Endpoint::start(Reply::Pong);
    let named = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let fx = Fixture::new(
        &decoy,
        "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\napi_key_env = \"OLLAMA_API_KEY\"\n",
    );
    let argv = args(&[
        "-c",
        &arg(&fx.chosen()),
        "--endpoint",
        &named.url(),
        "--model",
        "chosen-model",
    ]);
    let env_url = from_env.url();
    let env = vars(&[
        ("OLLAMA_BASE_URL", env_url.as_str()),
        ("OLLAMA_API_KEY", "ollama-FAKE-970-key"),
    ]);
    let out = one_prompt(&fx, Surface::Tui, &argv, &env, &named);
    assert!(
        from_env.requests().is_empty(),
        "the environment's host was called:\n{out}"
    );
    assert!(!chats(&named).is_empty(), "no chat at --endpoint:\n{out}");
    assert!(
        out.contains(&format!(
            "key from OLLAMA_API_KEY → 127.0.0.1:{}",
            named.port
        )),
        "the TUI did not say where the key goes:\n{out}"
    );
    assert!(!out.contains("FAKE-970"), "the key shows:\n{out}");
}

/// `--acp` and `--serve` apply `--endpoint` and `--model`: the prompt
/// goes to the endpoint the flag names, with the model the flag names, and
/// never to the endpoint of `-c`.
fn assert_the_flags_choose_the_model(surface: Surface) {
    let replaced = Endpoint::start(Reply::Pong);
    let named = Endpoint::start(Reply::Pong);
    let decoy = Endpoint::start(Reply::Pong);
    let runtime = Endpoint::start(Reply::Runtime);
    let fx = Fixture::new(&decoy, &compat(&replaced.url(), ""));
    let argv = args(&[
        "-c",
        &arg(&fx.chosen()),
        "--endpoint",
        &named.url(),
        "--model",
        "flag-model",
    ]);
    let run = match surface {
        Surface::Acp => {
            let mut full = args(&["--acp"]);
            full.extend(argv);
            fx.acp(&full, &[])
        }
        Surface::Serve => fx.serve(&argv, &[], &runtime),
        other => panic!("{other:?} is not --acp or --serve"),
    };
    assert!(
        replaced.requests().is_empty(),
        "{surface:?}: the replaced endpoint was called:\n{}",
        run.out
    );
    let sent = chats(&named);
    assert!(
        !sent.is_empty(),
        "{surface:?}: no chat at --endpoint:\n{}",
        run.out
    );
    for r in &sent {
        assert!(
            r.contains(r#""model":"flag-model""#),
            "{surface:?}: the chat did not name --model:\n{r}"
        );
    }
    assert!(
        decoy.requests().is_empty(),
        "{surface:?}: the decoy was called"
    );
}

/// Red on the pre-release build: `--acp` used the endpoint and model of `-c`.
#[test]
fn acp_sends_the_prompt_where_the_flags_say() {
    assert_the_flags_choose_the_model(Surface::Acp);
}

/// Red on the pre-release build: `--serve` used the endpoint and model of `-c`.
#[test]
fn serve_sends_the_prompt_where_the_flags_say() {
    assert_the_flags_choose_the_model(Surface::Serve);
}
