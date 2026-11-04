//! Workspace trust and symbolic links that leave the folder. A cloned
//! repository can ship `AGENTS.md -> ~/.ssh/id_ed25519`; the trust question
//! used to show the NAME only, and once the human said yes the target's
//! bytes went into the model's prompt. The binary runs in a pseudo-terminal,
//! the human answers yes, a local listener stands in for the model and
//! records every request. Every "secret" is a marker file this test writes.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const BOOT: Duration = Duration::from_secs(60);
/// `AGENTS.md` links to it.
const SECRET: &str = "OUTSIDE-SECRET-7f3c";
/// `skills/leak/SKILL.md` links to it.
const SKILL_SECRET: &str = "OUTSIDE-SKILL-SECRET-91ad";
/// `.x0ne/skills/dirleak` links to the directory holding it.
const DIR_SECRET: &str = "OUTSIDE-SKILL-DIR-SECRET-c2e0";
/// A plain rule file inside the folder: proves the instructions were read.
const INSIDE: &str = "INSIDE-RULE-5b17";
/// A rule that links to another file inside the folder: still admitted.
const INSIDE_LINK: &str = "INSIDE-LINK-RULE-e48a";
const PONG: &str = "pong-trust-links";
/// `Cargo.toml` links to a git config outside the folder: the user's name.
const GIT_NAME: &str = "GITCONFIG-NAME-3d9e";
/// A file in the directory `notes` links to, outside the folder.
const HOME_FILE: &str = "HOME-FILE-NAME-8b21";
/// In the folder's own `.env`, which `X0NE.md` links to.
const ENV_SECRET: &str = "ENV-SECRET-4a7c";
/// In the folder's own `.git/config`, which `.x0ne/rules/git.md` links to.
const GIT_SECRET: &str = "GIT-CONFIG-TOKEN-60f2";
/// Written into a rule after the question is shown, before the answer.
const SWAPPED: &str = "SWAPPED-AFTER-QUESTION-d71e";

/// A loopback model that records each request, body included.
struct Model {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Model {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                answer(stream, &request);
            }
        });
        Self { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> String {
        self.seen.lock().unwrap().join("\n")
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

fn answer(mut stream: TcpStream, request: &str) {
    let path = request.split_whitespace().nth(1).unwrap_or("");
    let body = if path.starts_with("/v1/models") {
        r#"{"data":[]}"#.to_string()
    } else {
        format!(
            r#"{{"choices":[{{"index":0,"finish_reason":"stop","message":{{"role":"assistant","content":"{PONG}"}}}}],"usage":{{"prompt_tokens":5,"completion_tokens":1}}}}"#
        )
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn link(target: &str, at: &Path) {
    std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
    symlink(target, at).expect("symlink");
}

/// A HOME whose config names the loopback model, a fake runtime, and a
/// working folder whose project files link out of it, as a clone can.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(model: &Model) -> Self {
        let fx = Self {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        write(
            &fx.path("home/.x0ne/config.toml"),
            &format!(
                "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\nbase_url = \"{}\"\n\
                 model = \"chosen-model\"\n",
                model.url()
            ),
        );
        let runtime = fx.path("bin/stdio-runtime");
        write(
            &runtime,
            "#!/bin/sh\nwhile read -r line; do echo '{\"success\":true,\"output\":{\"actions\":[]}}'; done\n",
        );
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        // Outside the folder: what a link must never bring into the prompt.
        write(
            &fx.path("outside/secret.md"),
            &format!("# notes\n{SECRET}\n"),
        );
        write(
            &fx.path("outside/skill.md"),
            &format!("---\nname: leak\ndescription: {SKILL_SECRET}\n---\n{SKILL_SECRET}\n"),
        );
        write(
            &fx.path("outside/skilldir/SKILL.md"),
            &format!("---\nname: dirleak\ndescription: {DIR_SECRET}\n---\n"),
        );
        // The folder: three links out, one link in, one plain file. Its own
        // runtime policy keeps the setup wizard from asking on the terminal.
        let work = fx.path("work");
        write(&work.join("x0ne-runtime.toml"), "");
        link("../outside/secret.md", &work.join("AGENTS.md"));
        link(
            "../../../outside/skill.md",
            &work.join("skills/leak/SKILL.md"),
        );
        link(
            "../../../outside/skilldir",
            &work.join(".x0ne/skills/dirleak"),
        );
        write(&work.join(".x0ne/rules/style.md"), &format!("{INSIDE}\n"));
        write(&work.join("docs/shared.md"), &format!("{INSIDE_LINK}\n"));
        link("../../docs/shared.md", &work.join(".x0ne/rules/shared.md"));
        // Read before any trust: the manifest and the tree.
        write(
            &fx.path("outside/gitconfig"),
            &format!("[user]\n\tname = {GIT_NAME}\n\temail = someone@example.com\n"),
        );
        link("../outside/gitconfig", &work.join("Cargo.toml"));
        write(&fx.path(&format!("outside/homedir/{HOME_FILE}.txt")), "");
        link("../outside/homedir", &work.join("notes"));
        // Links that stay inside, to the folder's own secrets.
        write(&work.join(".env"), &format!("API_KEY={ENV_SECRET}\n"));
        link(".env", &work.join("X0NE.md"));
        write(
            &work.join(".git/config"),
            &format!("[remote \"origin\"]\n\turl = https://u:{GIT_SECRET}@example.com/r.git\n"),
        );
        link("../../.git/config", &work.join(".x0ne/rules/git.md"));
        fx
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn env(&self) -> Vec<(String, String)> {
        let home = self.path("home");
        let h = |rel: &str| home.join(rel).display().to_string();
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.display().to_string()),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_STATE_HOME".into(), h(".local/state")),
            ("XDG_CACHE_HOME".into(), h(".cache")),
            (
                "X0NE_RUNTIME_BIN".into(),
                self.path("bin/stdio-runtime").display().to_string(),
            ),
            ("X0NE_NO_INTRO".into(), "true".into()),
            ("X0NE_LOG".into(), "error".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]
    }

    /// `x0ne -p hello` in a 200x50 pseudo-terminal, from the folder.
    fn screen(&self) -> Screen {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 50,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(["-p", "hello"]);
        cmd.cwd(self.path("work"));
        cmd.env_clear();
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn x0ne");
        drop(pair.slave);
        let raw = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let all = Arc::clone(&raw);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                all.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        Screen {
            raw,
            writer,
            child,
            _master: pair.master,
        }
    }
}

struct Screen {
    raw: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl Screen {
    /// Everything printed so far, colour codes removed.
    fn text(&self) -> String {
        strip_ansi(&String::from_utf8_lossy(&self.raw.lock().unwrap()))
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

/// CSI sequences (`ESC [ ... final`) out; everything else kept.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for f in chars.by_ref() {
                if ('@'..='~').contains(&f) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The trust question as printed, then every request the model received
/// after the human answered yes.
fn trust_with_yes() -> (String, String) {
    trust_with("y\r", |_| {})
}

/// [`trust_with_yes`] with `answer`, and `meanwhile` run while the question
/// waits for it.
fn trust_with(answer: &str, meanwhile: impl FnOnce(&Fixture)) -> (String, String) {
    let model = Model::start();
    let fx = Fixture::new(&model);
    let mut screen = fx.screen();
    assert!(
        screen.wait_for("Trust this folder?", BOOT),
        "no trust question:\n{}",
        screen.text()
    );
    let question = screen.text();
    meanwhile(&fx);
    screen.send(answer);
    let deadline = Instant::now() + BOOT;
    while Instant::now() < deadline
        && screen.alive()
        && !model.requests().contains("chat/completions")
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    // The one-shot ends after its turn; give the last request time to land.
    let settle = Instant::now() + Duration::from_secs(5);
    while Instant::now() < settle && screen.alive() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let seen = model.requests();
    assert!(
        seen.contains("chat/completions"),
        "the model was never called:\n{}",
        screen.text()
    );
    (question, seen)
}

/// Red on 2026-10-27: `AGENTS.md -> ../outside/secret.md` was fingerprinted,
/// and once trusted its target's text reached the model, as did the
/// description of a skill whose `SKILL.md`, or whose directory, links out.
#[test]
fn a_link_out_of_the_folder_never_reaches_the_prompt() {
    let (_, seen) = trust_with_yes();
    assert!(
        seen.contains(INSIDE),
        "the trusted instructions never reached the model: the test proves nothing"
    );
    let leaked: Vec<&str> = [
        ("AGENTS.md", SECRET),
        ("skills/leak/SKILL.md", SKILL_SECRET),
        (".x0ne/skills/dirleak", DIR_SECRET),
    ]
    .into_iter()
    .filter(|(_, marker)| seen.contains(marker))
    .map(|(what, _)| what)
    .collect();
    assert!(
        leaked.is_empty(),
        "these link out of the folder and their target reached the model: {leaked:?}"
    );
    assert!(
        seen.contains(INSIDE_LINK),
        "a link that stays inside the folder is refused"
    );
}

/// Red on 2026-10-27: the question listed `AGENTS.md` and never said where it
/// pointed. Each link out is named with its target and refused on screen.
#[test]
fn the_trust_question_shows_each_link_out_of_the_folder() {
    let (question, _) = trust_with_yes();
    for name in [
        "AGENTS.md",
        "skills/leak/SKILL.md",
        ".x0ne/skills/dirleak/SKILL.md",
    ] {
        assert!(
            question
                .lines()
                .any(|l| l.contains(name) && l.contains("outside this folder")),
            "{name}: no refusal on the trust question:\n{question}"
        );
    }
    assert!(
        !question
            .lines()
            .any(|l| l.contains(".x0ne/rules/shared.md") && l.contains("outside this folder")),
        "a link inside the folder is shown as refused:\n{question}"
    );
}

/// Red on 2026-10-27: `Cargo.toml -> ~/.gitconfig` put the user's name in the
/// project line, and a directory linked out put its file names in the tree,
/// whatever the answer: neither waits for trust.
#[test]
fn the_project_line_and_tree_never_follow_a_link_out() {
    let mut leaked = Vec::new();
    for answer in ["n\r", "y\r"] {
        let (_, seen) = trust_with(answer, |_| {});
        assert!(
            seen.contains("docs/shared.md"),
            "answer {answer:?}: the tree never reached the model: the test proves nothing"
        );
        for (what, marker) in [
            ("Cargo.toml -> ../outside/gitconfig, its name", GIT_NAME),
            ("notes -> ../outside/homedir, its file names", HOME_FILE),
        ] {
            if seen.contains(marker) {
                leaked.push(format!("answer {answer:?}: {what}"));
            }
        }
    }
    assert!(
        leaked.is_empty(),
        "these link out of the folder and reached the model: {leaked:#?}"
    );
}

/// Red on 2026-10-27: `X0NE.md -> .env` stayed inside the folder, so it was
/// read once trusted, and the question showed `X0NE.md` alone. Every link is
/// shown with its target, and a `.env*` or `.git/` target is refused.
#[test]
fn a_link_to_the_folders_own_secrets_is_shown_and_refused() {
    let (question, seen) = trust_with_yes();
    assert!(
        seen.contains(INSIDE),
        "the trusted instructions never reached the model: the test proves nothing"
    );
    let mut wrong = Vec::new();
    for (secret, what) in [
        (ENV_SECRET, "X0NE.md -> .env"),
        (GIT_SECRET, "git.md -> .git/config"),
    ] {
        if seen.contains(secret) {
            wrong.push(format!("{what}: its target reached the model"));
        }
    }
    for (name, target) in [("X0NE.md", ".env"), (".x0ne/rules/git.md", ".git/config")] {
        if !question
            .lines()
            .any(|l| l.contains(&format!("{name} -> {target}")) && l.contains("never read"))
        {
            wrong.push(format!(
                "{name}: not refused with its target on the question"
            ));
        }
    }
    if !question
        .lines()
        .any(|l| l.contains(".x0ne/rules/shared.md -> docs/shared.md"))
    {
        wrong.push("shared.md: a link inside is not shown with its target".to_string());
    }
    assert!(wrong.is_empty(), "{wrong:#?}\n{question}");
}

/// Red on 2026-10-27: the fingerprint was taken before the question and the
/// files read again after it. A rule rewritten while the question waited
/// reached the model, in place of the text that was trusted.
#[test]
fn what_reaches_the_model_is_what_was_trusted() {
    let (_, seen) = trust_with("y\r", |fx| {
        write(
            &fx.path("work/.x0ne/rules/style.md"),
            &format!("{SWAPPED}\n"),
        );
    });
    assert!(
        seen.contains(INSIDE_LINK),
        "the trusted rules never reached the model: the test proves nothing"
    );
    assert!(
        !seen.contains(SWAPPED),
        "a rule rewritten after the question reached the model"
    );
}
