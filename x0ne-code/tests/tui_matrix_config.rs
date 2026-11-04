//! `--tui` and `--matrix` run on the config the operator named, and
//! refuse the flags they would ignore. The real binary in a pseudo-terminal;
//! local listeners stand in for the model: one named by `-c`, one by a decoy
//! `~/.x0ne/config.toml` that must never be reached.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// Generous: a debug build on a loaded runner boots in seconds, not minutes.
const BOOT: Duration = Duration::from_secs(60);
const DECOY_MODEL: &str = "decoy-model";
const CHOSEN_MODEL: &str = "chosen-model";

/// What a fake model endpoint does with one request.
#[derive(Clone, Copy, Debug)]
enum Reply {
    /// Reads the request, then holds the socket open without a byte back.
    Silence,
    /// HTTP 503 with a JSON error body: a transient failure, every time.
    Unavailable,
    /// HTTP 401: a permanent failure.
    Unauthorized,
    /// One JSON completion whose text is [`PONG`].
    Pong,
    /// HTTP 200 with [`RUNTIME_OK`]: an HTTP runtime that accepts anything.
    Runtime,
}

/// What the runtime stand-ins answer: success, no action.
const RUNTIME_OK: &str = r#"{"success":true,"output":{"actions":[]}}"#;

/// The answer [`Reply::Pong`] carries.
const PONG: &str = "pong-970";

/// A local OpenAI-compatible endpoint that records every request body.
struct Model {
    port: u16,
    seen: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Model {
    /// `script[i]` answers request `i`; the last entry answers the rest.
    fn start(script: &[Reply]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let script = script.to_vec();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let n = {
                    let mut log = log.lock().unwrap();
                    log.push((Instant::now(), request));
                    log.len()
                };
                let reply = script[(n - 1).min(script.len() - 1)];
                std::thread::spawn(move || answer(stream, reply));
            }
        });
        Self { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// Gaps between consecutive requests.
    fn gaps(&self) -> Vec<Duration> {
        let seen = self.seen.lock().unwrap();
        seen.windows(2).map(|w| w[1].0 - w[0].0).collect()
    }

    /// The header line `--tui` and `--matrix` show for this endpoint.
    fn label(&self, model: &str) -> String {
        format!("openai-compat/{model} @ {}", self.url())
    }
}

/// Headers and body of one HTTP request, or `None` if the client hung up.
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

fn answer(stream: TcpStream, reply: Reply) {
    match reply {
        Reply::Silence => {
            std::thread::sleep(Duration::from_secs(600));
            drop(stream);
        }
        Reply::Unavailable => respond(
            stream,
            "503 Service Unavailable",
            r#"{"error":{"message":"overloaded"}}"#,
        ),
        Reply::Unauthorized => respond(
            stream,
            "401 Unauthorized",
            r#"{"error":{"message":"invalid api key"}}"#,
        ),
        Reply::Pong => respond(
            stream,
            "200 OK",
            &format!(
                r#"{{"choices":[{{"index":0,"finish_reason":"stop","message":{{"role":"assistant","content":"{PONG}"}}}}],"usage":{{"prompt_tokens":5,"completion_tokens":1}}}}"#
            ),
        ),
        Reply::Runtime => respond(stream, "200 OK", RUNTIME_OK),
    }
}

fn respond(mut stream: TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// A config that names one OpenAI-compatible endpoint, plus `extra` tables.
fn compat_config(model: &Model, name: &str, extra: &str) -> String {
    format!(
        "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\nbase_url = \"{}\"\nmodel = \"{name}\"\n\n{extra}",
        model.url()
    )
}

/// The `-c` file of a layered run: `provider` and `mode` as given, the chosen
/// endpoint as openai-compat, the decoy as Ollama.
fn layered_config(decoy: &Model, chosen: &Model, provider: &str, mode: &str) -> String {
    format!(
        "[llm]\nprovider = \"{provider}\"\n\n\
         [llm.openai_compat]\nbase_url = \"{}\"\nmodel = \"{CHOSEN_MODEL}\"\n\n\
         [llm.ollama]\nendpoint = \"{}\"\nmodel = \"{DECOY_MODEL}\"\n\n\
         [bridge]\nmode = \"{mode}\"\n",
        chosen.url(),
        decoy.url()
    )
}

/// One layered run: what `-c` names, the variables, the flags, and the
/// runtime that must start. The model is always the chosen endpoint.
struct Layers {
    what: &'static str,
    file: (&'static str, &'static str),
    vars: [(&'static str, &'static str); 2],
    flags: &'static [&'static str],
    runtime: &'static str,
}

const LAYERS: [Layers; 2] = [
    Layers {
        what: "a flag beats the environment",
        file: ("ollama", "sandbox"),
        vars: [
            ("X0NE_LLM_PROVIDER", "ollama"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
        ],
        flags: &["--provider", "openai-compat", "--mode", "stdio"],
        runtime: "stdio-runtime",
    },
    Layers {
        what: "without a flag the environment beats -c",
        file: ("ollama", "stdio"),
        vars: [
            ("X0NE_LLM_PROVIDER", "openai-compat"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
        ],
        flags: &[],
        runtime: "sandbox-runtime",
    },
];

impl Layers {
    /// The fixture of this case: the decoy fails at once, the chosen
    /// endpoint answers `chosen`.
    fn fixture(&self, chosen: &[Reply]) -> (Model, Model, Fixture) {
        let decoy = Model::start(&[Reply::Unavailable]);
        let chosen = Model::start(chosen);
        let fx = Fixture::layered(&decoy, &chosen, self.file.0, self.file.1);
        (decoy, chosen, fx)
    }

    /// `-c FILE`, then `surface`, then this case's flags.
    fn args(&self, fx: &Fixture, surface: &[&str]) -> Vec<String> {
        let mut args = vec!["-c".to_string(), arg(&fx.chosen())];
        args.extend(surface.iter().chain(self.flags).map(|a| (*a).to_string()));
        args
    }

    /// The chosen model was called, never the decoy, and only this case's
    /// runtime started.
    fn check(&self, surface: &str, fx: &Fixture, decoy: &Model, chosen: &Model, seen: &str) {
        let what = self.what;
        assert!(
            decoy.requests().is_empty(),
            "{surface}, {what}: the decoy was called:\n{seen}"
        );
        let first = chosen.requests().first().cloned().unwrap_or_default();
        assert!(
            first.contains(&format!("\"model\":\"{CHOSEN_MODEL}\"")),
            "{surface}, {what}: request {first:?}\n{seen}"
        );
        let started = fx.runtimes_started();
        assert!(
            !started.is_empty() && started.lines().all(|l| l == self.runtime),
            "{surface}, {what}: runtimes started {started:?}, expected {}\n{seen}",
            self.runtime
        );
    }
}

/// The binary on pipes, run to its end.
struct Run {
    /// `None`: killed at the deadline, or ended by a signal.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    })
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// `decoy` goes to `~/.x0ne/config.toml`, `chosen` to the file `-c` names.
    fn new(decoy: &Model, chosen: &Model, extra: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in ["home/.x0ne", "work", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        let fx = Fixture { dir };
        std::fs::write(
            fx.path("home/.x0ne/config.toml"),
            compat_config(decoy, DECOY_MODEL, extra),
        )
        .expect("decoy config");
        std::fs::write(fx.chosen(), compat_config(chosen, CHOSEN_MODEL, extra))
            .expect("chosen config");
        // A policy in the working directory keeps the first-run wizard away.
        std::fs::write(fx.path("work/x0ne-runtime.toml"), "").expect("policy");
        // The runtime: exits at once, so the session runs without tools.
        let runtime = fx.path("bin/fake-runtime");
        std::fs::write(&runtime, "#!/bin/sh\nexit 1\n").expect("fake runtime");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        fx
    }

    /// [`Fixture::new`] with `-c` naming [`layered_config`], and two runtimes
    /// that answer every line and record which of them started.
    fn layered(decoy: &Model, chosen: &Model, provider: &str, mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let fx = Self::new(decoy, chosen, "");
        std::fs::write(fx.chosen(), layered_config(decoy, chosen, provider, mode))
            .expect("layered config");
        for name in ["stdio-runtime", "sandbox-runtime"] {
            let at = fx.path(&format!("bin/{name}"));
            let script = format!(
                "#!/bin/sh\necho {name} >> '{}'\nwhile read -r line; do echo '{RUNTIME_OK}'; done\n",
                fx.path("runtime-started").display()
            );
            std::fs::write(&at, script).expect("runtime");
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        fx
    }

    /// One line per runtime started, its name.
    fn runtimes_started(&self) -> String {
        std::fs::read_to_string(self.path("runtime-started")).unwrap_or_default()
    }

    /// The variables of a layered run: both runtimes, then `vars`.
    fn layered_env(&self, vars: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "X0NE_RUNTIME_BIN".to_string(),
                arg(&self.path("bin/stdio-runtime")),
            ),
            (
                "X0NE_SANDBOX_CMD".to_string(),
                arg(&self.path("bin/sandbox-runtime")),
            ),
        ];
        env.extend(
            vars.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        env
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn chosen(&self) -> PathBuf {
        self.path("chosen.toml")
    }

    fn env(&self) -> Vec<(String, String)> {
        let home = self.path("home");
        let h = |rel: &str| home.join(rel).display().to_string();
        vec![
            ("PATH".into(), self.path("bin").display().to_string()),
            ("HOME".into(), home.display().to_string()),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_STATE_HOME".into(), h(".local/state")),
            ("XDG_CACHE_HOME".into(), h(".cache")),
            (
                "X0NE_RUNTIME_BIN".into(),
                self.path("bin/fake-runtime").display().to_string(),
            ),
            ("X0NE_NO_INTRO".into(), "true".into()),
            ("X0NE_LOG".into(), "error".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]
    }

    /// The binary in a 140x40 pseudo-terminal.
    fn screen(&self, args: &[&str]) -> Screen {
        self.screen_with(args, &[])
    }

    /// [`Fixture::screen`], `extra` variables over the fixture's.
    fn screen_with<S: AsRef<str>>(&self, args: &[S], extra: &[(String, String)]) -> Screen {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(args.iter().map(AsRef::<str>::as_ref));
        cmd.cwd(self.path("work"));
        cmd.env_clear();
        for (k, v) in self.env().into_iter().chain(extra.iter().cloned()) {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn x0ne");
        drop(pair.slave);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(40, 140, 0)));
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let sink = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        Screen {
            parser,
            writer,
            child,
            _master: pair.master,
        }
    }

    /// The binary on pipes, for what is refused before any screen.
    fn run(&self, args: &[&str]) -> (Option<i32>, String) {
        let run = self.run_with(args, &[], &[]);
        (run.code, run.stderr)
    }

    /// The binary on pipes, `extra` variables over the fixture's. `stdin`
    /// is written, then held open until x0ne exits or a runtime starts.
    fn run_with<S: AsRef<str>>(
        &self,
        args: &[S],
        extra: &[(String, String)],
        stdin: &[String],
    ) -> Run {
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne"))
            .args(args.iter().map(AsRef::<str>::as_ref))
            .current_dir(self.path("work"))
            .env_clear()
            .envs(self.env())
            .envs(extra.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn x0ne");
        let stdout = drain(child.stdout.take().expect("stdout"));
        let stderr = drain(child.stderr.take().expect("stderr"));
        let mut input = child.stdin.take();
        if let Some(w) = input.as_mut() {
            for line in stdin {
                let _ = writeln!(w, "{line}");
            }
            let _ = w.flush();
        }
        let deadline = Instant::now() + BOOT;
        let code = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status.code();
            }
            if !self.runtimes_started().is_empty() {
                drop(input.take());
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        Run {
            code,
            stdout: stdout.join().expect("stdout reader"),
            stderr: stderr.join().expect("stderr reader"),
        }
    }
}

struct Screen {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl Screen {
    fn text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Waits for `what` on screen; fails at once if the decoy shows up.
    fn wait_for(&self, what: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let text = self.text();
            assert!(
                !text.contains(DECOY_MODEL),
                "the decoy config reached the screen:\n{text}"
            );
            if text.contains(what) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{what}` never appeared; screen:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Screen {
    /// Waits for `what` on screen; fails at once if `decoy` is called.
    fn wait_for_without(&self, what: &str, decoy: &Model, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.text().contains(what) {
            assert!(
                decoy.requests().is_empty(),
                "the decoy was called before `{what}`; screen:\n{}",
                self.text()
            );
            assert!(
                Instant::now() < deadline,
                "`{what}` never appeared; screen:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `done` until it holds; fails at once if the decoy was called.
fn wait_for_request(chosen: &Model, decoy: &Model, screen: &Screen) {
    let deadline = Instant::now() + BOOT;
    loop {
        assert!(
            decoy.requests().is_empty(),
            "the decoy endpoint was called; screen:\n{}",
            screen.text()
        );
        if !chosen.requests().is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the endpoint named by -c was never called; screen:\n{}",
            screen.text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn arg(p: &Path) -> String {
    p.display().to_string()
}

/// `x0ne --tui -c FILE`: the header names FILE's model and endpoint, and the
/// first turn goes there. Previously the TUI reread `~/.x0ne/config.toml`.
#[test]
fn the_tui_runs_on_the_config_named_by_c() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Silence]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let mut screen = fx.screen(&["--tui", "-c", &arg(&fx.chosen())]);
    screen.wait_for(&chosen.label(CHOSEN_MODEL), BOOT);
    screen.send("hello\r");
    wait_for_request(&chosen, &decoy, &screen);
    assert!(
        chosen.requests()[0].contains(&format!("\"model\":\"{CHOSEN_MODEL}\"")),
        "request: {}",
        chosen.requests()[0]
    );
}

/// `x0ne --matrix -c FILE`: same property, on the canvas. Previously,
/// `--matrix` without `--provider` reread `~/.x0ne/config.toml`.
#[test]
fn matrix_runs_on_the_config_named_by_c() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Silence]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let mut screen = fx.screen(&["--matrix", "-c", &arg(&fx.chosen())]);
    screen.wait_for("select", BOOT);
    screen.send("1\r");
    wait_for_request(&chosen, &decoy, &screen);
    assert!(
        chosen.requests()[0].contains(&format!("\"model\":\"{CHOSEN_MODEL}\"")),
        "request: {}",
        chosen.requests()[0]
    );
    screen.wait_for(&chosen.label(CHOSEN_MODEL), BOOT);
}

/// A model that sends nothing is cut at `[ui] model_timeout_secs`, the
/// error is on screen, and the next prompt is answered. Before, the TUI
/// waited on the transport: 300 s per attempt, 3 attempts.
#[test]
fn the_tui_bounds_a_silent_model_and_stays_usable() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Silence, Reply::Pong]);
    let fx = Fixture::new(&decoy, &chosen, "[ui]\nmodel_timeout_secs = 2\n");
    let mut screen = fx.screen(&["--tui", "-c", &arg(&fx.chosen())]);
    screen.wait_for(&chosen.label(CHOSEN_MODEL), BOOT);
    screen.send("hello\r");
    wait_for_request(&chosen, &decoy, &screen);
    let asked = Instant::now();
    screen.wait_for("model error · timeout: no answer within 2 s", BOOT);
    let waited = asked.elapsed();
    assert!(
        waited >= Duration::from_millis(1800) && waited < Duration::from_secs(15),
        "the error came after {waited:?}"
    );
    screen.send("again\r");
    screen.wait_for(PONG, BOOT);
    assert_eq!(chosen.requests().len(), 2, "screen:\n{}", screen.text());
}

/// Why the model failed is on screen, not only "turn model_failure",
/// and the session takes the next prompt.
#[test]
fn the_tui_shows_why_the_model_failed() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Unauthorized, Reply::Pong]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let mut screen = fx.screen(&["--tui", "-c", &arg(&fx.chosen())]);
    screen.wait_for(&chosen.label(CHOSEN_MODEL), BOOT);
    screen.send("hello\r");
    screen.wait_for(
        "model error · authentication: OpenAI-compat stream error 401",
        BOOT,
    );
    screen.send("again\r");
    screen.wait_for(PONG, BOOT);
}

/// Ctrl+C during a turn stops the turn, not the TUI; a second Ctrl+C
/// within 2 s would quit. Before, the first Ctrl+C quit.
#[test]
fn ctrl_c_stops_the_turn_and_the_tui_stays_open() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Silence, Reply::Pong]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let mut screen = fx.screen(&["--tui", "-c", &arg(&fx.chosen())]);
    screen.wait_for(&chosen.label(CHOSEN_MODEL), BOOT);
    screen.send("hello\r");
    wait_for_request(&chosen, &decoy, &screen);
    screen.send("\x03");
    screen.wait_for("turn cancelled", Duration::from_secs(10));
    assert!(screen.alive(), "Ctrl+C quit; screen:\n{}", screen.text());
    std::thread::sleep(Duration::from_millis(2100));
    screen.send("again\r");
    screen.wait_for(PONG, BOOT);
    assert!(screen.alive());
}

/// A model that keeps failing gets a bounded number of attempts, each
/// gap longer than the last, then an error on screen; nothing is sent while
/// it is shown. Before, matrix called it again every 2 s, forever.
#[test]
fn matrix_gives_up_on_a_failing_model_and_says_so() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Unavailable]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let mut screen = fx.screen(&["--matrix", "-c", &arg(&fx.chosen())]);
    screen.wait_for("select", BOOT);
    screen.send("1\r");
    let deadline = Instant::now() + BOOT;
    while !screen.text().contains("gave up after 3 attempts") {
        assert!(
            chosen.requests().len() <= 3,
            "a 4th attempt; screen:\n{}",
            screen.text()
        );
        assert!(
            Instant::now() < deadline,
            "no final error on screen; screen:\n{}",
            screen.text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = screen.text();
    for what in [
        "503",
        "temporarily_unavailable",
        &chosen.label(CHOSEN_MODEL),
    ] {
        assert!(text.contains(what), "`{what}` missing; screen:\n{text}");
    }
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(chosen.requests().len(), 3, "screen:\n{}", screen.text());
    let gaps = chosen.gaps();
    assert!(
        gaps[0] >= Duration::from_millis(900) && gaps[1] > gaps[0],
        "the delay does not grow: {gaps:?}"
    );
    assert!(decoy.requests().is_empty());
}

/// A flag `--tui` or `--matrix` would drop is refused, exit 1, before any
/// model is called.
#[test]
fn a_flag_the_tui_or_matrix_would_ignore_is_refused() {
    let decoy = Model::start(&[Reply::Silence]);
    let chosen = Model::start(&[Reply::Silence]);
    let fx = Fixture::new(&decoy, &chosen, "");
    let cases: [(&[&str], &str, &str); 4] = [
        (&["--tui", "-p", "hi"], "--prompt", "--tui"),
        (&["--matrix", "--auto-mode"], "--auto-mode", "--matrix"),
        (&["--tui", "--matrix"], "--tui", "--matrix"),
        (
            &["--tui", "--output-format", "json"],
            "--output-format",
            "--tui",
        ),
    ];
    for (args, flag, surface) in cases {
        let (code, stderr) = fx.run(args);
        assert_eq!(code, Some(1), "{args:?}: exit code; stderr:\n{stderr}");
        assert!(
            stderr.contains(flag) && stderr.contains(surface),
            "{args:?}: the refusal must name {flag} and {surface}:\n{stderr}"
        );
    }
    assert!(decoy.requests().is_empty() && chosen.requests().is_empty());
}

/// `-p` takes `--provider` over `X0NE_LLM_PROVIDER` and `--mode` over
/// `X0NE_BRIDGE_MODE`, and those over `-c`. Before, the variables won.
#[test]
fn one_shot_puts_a_flag_over_the_environment_over_the_file() {
    for case in &LAYERS {
        let (decoy, chosen, fx) = case.fixture(&[Reply::Pong]);
        let run = fx.run_with(
            &case.args(&fx, &["-p", "hello"]),
            &fx.layered_env(&case.vars),
            &[],
        );
        let seen = format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            run.code, run.stdout, run.stderr
        );
        case.check("-p", &fx, &decoy, &chosen, &seen);
        assert!(run.stdout.contains(PONG), "-p, {}: {seen}", case.what);
        assert_eq!(run.code, Some(0), "-p, {}: {seen}", case.what);
    }
}

/// The REPL, same layers as `-p`.
#[test]
fn the_repl_puts_a_flag_over_the_environment_over_the_file() {
    for case in &LAYERS {
        let (decoy, chosen, fx) = case.fixture(&[Reply::Pong]);
        let mut screen = fx.screen_with(&case.args(&fx, &[]), &fx.layered_env(&case.vars));
        screen.wait_for_without("❯", &decoy, BOOT);
        screen.send("hello\r");
        wait_for_request(&chosen, &decoy, &screen);
        screen.wait_for(PONG, BOOT);
        case.check("the REPL", &fx, &decoy, &chosen, &screen.text());
    }
}

/// `--tui`, same layers as `-p`.
#[test]
fn the_tui_puts_a_flag_over_the_environment_over_the_file() {
    for case in &LAYERS {
        let (decoy, chosen, fx) = case.fixture(&[Reply::Pong]);
        let mut screen = fx.screen_with(&case.args(&fx, &["--tui"]), &fx.layered_env(&case.vars));
        screen.wait_for_without(&chosen.label(CHOSEN_MODEL), &decoy, BOOT);
        screen.send("hello\r");
        wait_for_request(&chosen, &decoy, &screen);
        case.check("--tui", &fx, &decoy, &chosen, &screen.text());
    }
}

/// `--matrix`, same layers as `-p`. Before, `--matrix --provider`
/// beat the variable while the REPL let the variable win.
#[test]
fn matrix_puts_a_flag_over_the_environment_over_the_file() {
    for case in &LAYERS {
        let (decoy, chosen, fx) = case.fixture(&[Reply::Silence]);
        let mut screen =
            fx.screen_with(&case.args(&fx, &["--matrix"]), &fx.layered_env(&case.vars));
        screen.wait_for("select", BOOT);
        screen.send("1\r");
        wait_for_request(&chosen, &decoy, &screen);
        case.check("--matrix", &fx, &decoy, &chosen, &screen.text());
    }
}

/// `--acp` builds its model client before the protocol and its runtime at
/// `session/new`. `openai` has no key here: resolving it ends the server.
const ACP_LAYERS: [Layers; 2] = [
    Layers {
        what: "a flag beats the environment",
        file: ("openai", "sandbox"),
        vars: [
            ("X0NE_LLM_PROVIDER", "openai"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
        ],
        flags: &["--provider", "openai-compat", "--mode", "stdio"],
        runtime: "stdio-runtime",
    },
    Layers {
        what: "without a flag the environment beats -c",
        file: ("openai", "stdio"),
        vars: [
            ("X0NE_LLM_PROVIDER", "openai-compat"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
        ],
        flags: &[],
        runtime: "sandbox-runtime",
    },
];

/// `--acp`, same layers as `-p`.
#[test]
fn acp_puts_a_flag_over_the_environment_over_the_file() {
    for case in &ACP_LAYERS {
        let (_decoy, _chosen, fx) = case.fixture(&[Reply::Silence]);
        let cwd = fx.path("work").canonicalize().expect("cwd");
        let lines = [
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": 1}})
            .to_string(),
            serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": cwd, "mcpServers": []}})
            .to_string(),
        ];
        let run = fx.run_with(
            &case.args(&fx, &["--acp"]),
            &fx.layered_env(&case.vars),
            &lines,
        );
        let started = fx.runtimes_started();
        assert!(
            !started.is_empty() && started.lines().all(|l| l == case.runtime),
            "--acp, {}: runtimes started {started:?}, expected {}; exit {:?}\nstderr:\n{}",
            case.what,
            case.runtime,
            run.code,
            run.stderr
        );
    }
}

/// A free loopback port, released for the binary to bind.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port()
}

/// `GET path` with `token` as bearer: the response, once it is a 200.
fn get(port: u16, path: &str, token: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    out.starts_with("HTTP/1.1 200").then_some(out)
}

/// A child process killed when dropped, a failed assertion included.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `--serve`: `--provider` beats `X0NE_LLM_PROVIDER`, which beats `-c`;
/// `/info` names the provider that runs. Its bridge is always http.
#[test]
fn serve_puts_a_flag_over_the_environment_over_the_file() {
    const TOKEN: &str = "test-token-970";
    for case in &LAYERS {
        let (decoy, _chosen, fx) = case.fixture(&[Reply::Unavailable]);
        let runtime = Model::start(&[Reply::Runtime]);
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let mut env = fx.layered_env(&case.vars);
        env.push((
            "X0NE_CORE_ADDR".into(),
            format!("127.0.0.1:{}", runtime.port),
        ));
        env.push(("X0NE_SERVE_TOKEN".into(), TOKEN.into()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_x0ne"))
            .args(case.args(&fx, &["--serve", "--bind", &bind]))
            .current_dir(fx.path("work"))
            .env_clear()
            .envs(fx.env())
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn x0ne --serve");
        let stderr = drain(child.stderr.take().expect("stderr"));
        let mut serve = Daemon(child);
        let deadline = Instant::now() + BOOT;
        let info = loop {
            assert!(
                decoy.requests().is_empty(),
                "--serve, {}: the decoy was called",
                case.what
            );
            if let Some(info) = get(port, "/info", TOKEN) {
                break info;
            }
            if let Some(status) = serve.0.try_wait().expect("try_wait") {
                let stderr = stderr.join().expect("stderr reader");
                panic!("--serve, {}: {status}\nstderr:\n{stderr}", case.what);
            }
            assert!(
                Instant::now() < deadline,
                "--serve, {}: no /info",
                case.what
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(
            info.contains("\"provider\":\"openai-compat\""),
            "--serve, {}: {info}",
            case.what
        );
    }
}

/// With the flags on openai-compat and its endpoint, an Anthropic key
/// from `-c`, `X0NE_OPENAI_COMPAT_KEY` or `OPENAI_API_KEY` is refused before
/// any call, and `ANTHROPIC_API_KEY` is never sent there.
#[test]
fn an_anthropic_key_never_reaches_the_endpoint_the_flags_name() {
    const KEY: &str = "sk-ant-FAKE-970";
    let cases: [(&str, bool, &[(&str, &str)], bool); 4] = [
        ("-c", true, &[], true),
        (
            "X0NE_OPENAI_COMPAT_KEY",
            false,
            &[("X0NE_OPENAI_COMPAT_KEY", KEY)],
            true,
        ),
        ("OPENAI_API_KEY", false, &[("OPENAI_API_KEY", KEY)], true),
        (
            "ANTHROPIC_API_KEY",
            false,
            &[("ANTHROPIC_API_KEY", KEY)],
            false,
        ),
    ];
    for (from, in_file, vars, refused) in cases {
        let decoy = Model::start(&[Reply::Unavailable]);
        let target = Model::start(&[Reply::Pong]);
        let fx = Fixture::layered(&decoy, &decoy, "ollama", "stdio");
        let key_line = if in_file {
            format!("api_key = \"{KEY}\"\n")
        } else {
            String::new()
        };
        std::fs::write(
            fx.chosen(),
            format!(
                "[llm]\nprovider = \"ollama\"\n\n[llm.openai_compat]\nbase_url = \"{}\"\n\
                 model = \"{CHOSEN_MODEL}\"\n{key_line}\n[llm.ollama]\nendpoint = \"{}\"\n\
                 model = \"{DECOY_MODEL}\"\n",
                decoy.url(),
                decoy.url()
            ),
        )
        .expect("config");
        let mut env = fx.layered_env(&[("X0NE_LLM_PROVIDER", "ollama")]);
        env.extend(
            vars.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        let url = target.url();
        let args = [
            "-c",
            &arg(&fx.chosen()),
            "-p",
            "hello",
            "--provider",
            "openai-compat",
            "--endpoint",
            &url,
        ];
        let run = fx.run_with(&args, &env, &[]);
        let seen = format!(
            "{from}: exit {:?}\nstdout:\n{}\nstderr:\n{}",
            run.code, run.stdout, run.stderr
        );
        assert!(decoy.requests().is_empty(), "the decoy was called; {seen}");
        let sent = target.requests();
        assert!(
            sent.iter().all(|r| !r.contains("sk-ant-")),
            "an Anthropic key left; {seen}"
        );
        if refused {
            assert!(sent.is_empty(), "{} request(s) sent; {seen}", sent.len());
            // With no terminal to ask on, the refusal is the error.
            assert!(
                run.stderr.contains("refusing to send an Anthropic key")
                    && !run.stdout.contains("refusing"),
                "no refusal on stderr; {seen}"
            );
            assert_ne!(run.code, Some(0), "{seen}");
        } else {
            assert!(!sent.is_empty() && run.stdout.contains(PONG), "{seen}");
        }
    }
}
