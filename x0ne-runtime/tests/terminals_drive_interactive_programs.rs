//! Term* actions: named terminals the agent drives like a human at tmux.
//!
//! Every test runs twice, once per backend (tmux on a private socket, native
//! PTY). A real program is started each time; nothing is mocked.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use x0ne_runtime::action::Action;
use x0ne_runtime::config::{AppConfig, IsolationMode};
use x0ne_runtime::executor::term;
use x0ne_runtime::handles::{new_shared, SharedHandles};
use x0ne_runtime::policy::{DenyReason, PolicyEffect, PolicyEngine};
use x0ne_runtime::result::{ActionResult, ErrorKind};

const TMUX_LOCATIONS: &[&str] = &["/usr/bin/tmux", "/usr/local/bin/tmux", "/bin/tmux"];

fn tmux_present() -> bool {
    TMUX_LOCATIONS.iter().any(|p| Path::new(p).is_file())
}

struct Rig {
    cfg: AppConfig,
    policy: PolicyEngine,
    handles: SharedHandles,
    _rec: tempfile::TempDir,
    rec_root: PathBuf,
}

fn cfg(force_pty: bool) -> (AppConfig, tempfile::TempDir) {
    let rec = tempfile::tempdir().expect("recordings dir");
    let mut cfg = AppConfig::defaults();
    cfg.policy.mode = "auto".into();
    cfg.policy.allowed_commands = ["python3", "bash", "sh", "sleep"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    cfg.isolation.mode = IsolationMode::None;
    for name in [
        "TermOpen",
        "TermSend",
        "TermRead",
        "TermWait",
        "TermResize",
        "TermSnapshot",
        "TermList",
        "TermClose",
    ] {
        cfg.actions.insert(name.into(), true);
    }
    cfg.term.recordings_root = Some(rec.path().to_path_buf());
    cfg.term.force_pty = force_pty;
    (cfg, rec)
}

impl Rig {
    fn new(force_pty: bool) -> Rig {
        Rig::with(cfg(force_pty))
    }

    fn with((cfg, rec): (AppConfig, tempfile::TempDir)) -> Rig {
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let rec_root = rec.path().to_path_buf();
        Rig {
            cfg,
            policy,
            handles: new_shared(100, 60, 20),
            _rec: rec,
            rec_root,
        }
    }

    /// Policy first, then the executor: the path a real call takes.
    async fn act(&self, v: Value) -> ActionResult {
        let action: Action = serde_json::from_value(v).expect("valid action");
        match self.policy.evaluate(&action) {
            PolicyEffect::Allow => term::execute(&action, &self.cfg, self.handles.clone()).await,
            other => panic!("policy refused {}: {other:?}", action.type_name()),
        }
    }

    async fn ok(&self, v: Value) -> Value {
        let r = self.act(v.clone()).await;
        assert!(r.success, "{v} failed: {:?}", r.error);
        data(&r)
    }

    async fn close_all(&self) {
        let reg = self.handles.lock().await.terms.clone();
        reg.close_all();
    }
}

fn data(r: &ActionResult) -> Value {
    serde_json::to_value(&r.output).expect("output serializes")["data"].clone()
}

fn backends() -> Vec<bool> {
    // false = tmux when present, true = native PTY.
    vec![false, true]
}

fn expected_backend(force_pty: bool) -> &'static str {
    if !force_pty && tmux_present() {
        "tmux"
    } else {
        "pty"
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

async fn gone_within(pid: u64, ms: u64) -> bool {
    for _ in 0..(ms / 50) {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !pid_alive(pid)
}

fn casts_in(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "cast").unwrap_or(false))
        .collect()
}

/// The owner's first case: open a Python REPL, type, wait for the answer.
#[tokio::test]
async fn a_python_repl_is_driven_to_42_and_recorded() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        let opened = rig
            .ok(json!({"type":"TermOpen","name":"py","command":"python3","args":["-i","-q"]}))
            .await;
        assert_eq!(opened["backend"], expected_backend(force_pty));
        assert_eq!(opened["cols"], 200);
        assert_eq!(opened["rows"], 50);

        let w = rig
            .ok(json!({"type":"TermWait","name":"py","pattern":">>> ","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "{w}");

        rig.ok(json!({"type":"TermSend","name":"py","text":"print(6*7)","keys":["Enter"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"py","pattern":"(?m)^42$","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "backend pty={force_pty}: {w}");

        let r = rig.ok(json!({"type":"TermRead","name":"py"})).await;
        assert!(r["text"].as_str().unwrap().contains("42"), "{r}");
        assert!(
            !r["text"].as_str().unwrap().contains('\u{1b}'),
            "ANSI must be stripped"
        );
        assert!(r["cursor"]["y"].is_number() || force_pty, "{r}");

        let closed = rig.ok(json!({"type":"TermClose","name":"py"})).await;
        let cast = PathBuf::from(closed["recording"].as_str().expect("recording path"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let body = std::fs::read_to_string(&cast).expect("cast exists");
        let mut lines = body.lines();
        let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2, "asciicast v2 header: {header}");
        let events: Vec<Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(
            events
                .iter()
                .any(|e| e[1] == "o" && e[2].as_str().unwrap().contains("42")),
            "output events carry the answer"
        );
        assert!(
            events
                .iter()
                .any(|e| e[1] == "i" && e[2].as_str().unwrap().contains("print(6*7)")),
            "input events carry what was typed"
        );
        let audit = std::fs::read_to_string(cast.parent().unwrap().join("audit.jsonl"))
            .expect("audit journal");
        assert!(audit.contains("\"TermOpen\"") && audit.contains("print(6*7)"));
        rig.close_all().await;
    }
}

/// The second case: a `[y/N]` prompt answered, then the exit status read.
#[tokio::test]
async fn a_yes_no_prompt_is_answered_and_the_exit_status_returned() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        rig.ok(json!({"type":"TermOpen","name":"ask","command":"bash","args":["-c",
            "read -r -p 'Delete everything? [y/N] ' a; echo \"answer=$a\"; [ \"$a\" = y ] && exit 7; exit 0"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"ask","pattern":"\\[y/N\\] $","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "{w}");
        rig.ok(json!({"type":"TermSend","name":"ask","text":"y","keys":["Enter"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"ask","exit":true,"timeout_ms":10000,"diff":false}))
            .await;
        assert_eq!(w["condition"], "exit", "pty={force_pty}: {w}");
        assert_eq!(w["exit_status"], 7, "pty={force_pty}: {w}");
        assert!(w["text"].as_str().unwrap().contains("answer=y"), "{w}");
        rig.close_all().await;
    }
}

/// Named keys reach the program: C-c interrupts a running loop.
#[tokio::test]
async fn control_c_interrupts_the_foreground_program() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        rig.ok(json!({"type":"TermOpen","name":"loop","command":"python3","args":["-q","-i"]}))
            .await;
        rig.ok(json!({"type":"TermWait","name":"loop","pattern":">>> ","timeout_ms":10000}))
            .await;
        rig.ok(json!({"type":"TermSend","name":"loop","text":"import time\nwhile True: time.sleep(1)\n","keys":["Enter"]}))
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        rig.ok(json!({"type":"TermSend","name":"loop","keys":["C-c"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"loop","pattern":"KeyboardInterrupt","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "pty={force_pty}: {w}");
        rig.close_all().await;
    }
}

/// A wait returns only what changed since the last read: fewer tokens.
#[tokio::test]
async fn a_wait_returns_the_diff_since_the_last_read() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        rig.ok(
            json!({"type":"TermOpen","name":"d","command":"bash","args":["--norc","--noprofile"]}),
        )
        .await;
        rig.ok(json!({"type":"TermWait","name":"d","stable_ms":300,"timeout_ms":5000}))
            .await;
        rig.ok(json!({"type":"TermRead","name":"d"})).await;
        rig.ok(
            json!({"type":"TermSend","name":"d","text":"echo marker-$((40+2))","keys":["Enter"]}),
        )
        .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"d","pattern":"marker-42","timeout_ms":10000}))
            .await;
        let diff = w["diff"].as_array().expect("diff rows");
        assert!(!diff.is_empty() && diff.len() <= 4, "pty={force_pty}: {w}");
        assert!(diff
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("marker-42")));
        rig.close_all().await;
    }
}

/// Resize, list, snapshot.
#[tokio::test]
async fn resize_list_and_snapshot() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        rig.ok(json!({"type":"TermOpen","name":"s","command":"bash","args":["--norc","--noprofile"],"cols":80,"rows":24}))
            .await;
        rig.ok(json!({"type":"TermResize","name":"s","cols":100,"rows":30}))
            .await;
        rig.ok(json!({"type":"TermSend","name":"s","text":"stty size; printf '\\033[31mred\\033[0m\\n'","keys":["Enter"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"s","pattern":"(?m)^30 100$","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "pty={force_pty}: {w}");
        let l = rig.ok(json!({"type":"TermList"})).await;
        let t = &l["terminals"][0];
        assert_eq!(
            (t["name"].as_str(), t["cols"].as_u64(), t["rows"].as_u64()),
            (Some("s"), Some(100), Some(30))
        );
        assert_eq!(t["alive"], true);
        let plain = rig.ok(json!({"type":"TermSnapshot","name":"s"})).await;
        assert!(plain["text"].as_str().unwrap().contains("red"));
        assert!(!plain["text"].as_str().unwrap().contains('\u{1b}'));
        let ansi = rig
            .ok(json!({"type":"TermSnapshot","name":"s","ansi":true}))
            .await;
        assert!(
            ansi["text"].as_str().unwrap().contains("\u{1b}["),
            "pty={force_pty}"
        );
        rig.close_all().await;
    }
}

/// The number of terminals is capped, and `close_all` leaves nothing running.
#[tokio::test]
async fn the_terminal_count_is_capped_and_close_all_kills_every_program() {
    for force_pty in backends() {
        let (mut c, rec) = cfg(force_pty);
        c.term.max_sessions = 2;
        let rig = Rig::with((c, rec));
        let mut pids = Vec::new();
        for n in ["a", "b"] {
            let o = rig
                .ok(json!({"type":"TermOpen","name":n,"command":"sleep","args":["300"]}))
                .await;
            pids.push(o["pid"].as_u64().expect("pid"));
        }
        let third = rig
            .act(json!({"type":"TermOpen","name":"c","command":"sleep","args":["300"]}))
            .await;
        assert!(!third.success);
        assert!(
            third
                .error
                .as_ref()
                .unwrap()
                .message
                .contains("max_sessions"),
            "the refusal names the knob: {:?}",
            third.error
        );
        let dup = rig
            .act(json!({"type":"TermOpen","name":"a","command":"sleep","args":["1"]}))
            .await;
        assert!(!dup.success, "a name is unique among open terminals");
        for pid in &pids {
            assert!(pid_alive(*pid), "pid {pid} should run before close_all");
        }
        rig.close_all().await;
        for pid in &pids {
            assert!(
                gone_within(*pid, 5000).await,
                "pid {pid} survived close_all (pty={force_pty})"
            );
        }
        let l = rig.ok(json!({"type":"TermList"})).await;
        assert_eq!(l["terminals"].as_array().unwrap().len(), 0);
        assert_eq!(
            casts_in(&rig.rec_root).len(),
            2,
            "one recording per terminal"
        );
    }
}

/// Bad input is refused before anything runs.
#[tokio::test]
async fn names_and_keys_are_validated() {
    let rig = Rig::new(false);
    let bad = rig
        .act(json!({"type":"TermOpen","name":"a;b","command":"sleep","args":["1"]}))
        .await;
    assert!(!bad.success);
    rig.ok(json!({"type":"TermOpen","name":"k","command":"sleep","args":["30"]}))
        .await;
    let bad = rig
        .act(json!({"type":"TermSend","name":"k","keys":["Enter; kill-server"]}))
        .await;
    assert!(!bad.success);
    assert!(bad.error.unwrap().message.contains("unknown key"));
    let missing = rig.act(json!({"type":"TermRead","name":"nope"})).await;
    assert!(!missing.success);
    let no_cond = rig.act(json!({"type":"TermWait","name":"k"})).await;
    assert!(!no_cond.success, "a wait needs a condition");
    rig.close_all().await;
}

/// The wait ceiling is a resource guard, and it is configurable.
#[tokio::test]
async fn the_wait_is_bounded_by_the_configured_ceiling() {
    let (mut c, rec) = cfg(false);
    c.term.max_wait_ms = 300;
    let rig = Rig::with((c, rec));
    rig.ok(json!({"type":"TermOpen","name":"w","command":"sleep","args":["30"]}))
        .await;
    let t0 = std::time::Instant::now();
    let w = rig
        .act(json!({"type":"TermWait","name":"w","pattern":"never","timeout_ms":60000}))
        .await;
    assert!(t0.elapsed() < Duration::from_secs(3));
    // A wait that never saw its condition is a failure, not a ✓.
    assert!(!w.success, "timeout reported as success");
    let e = w.error.unwrap();
    assert_eq!(e.kind, ErrorKind::Timeout);
    assert!(e.message.contains("never"), "{}", e.message);
    rig.close_all().await;
}

/// Output is bounded in bytes; the tail is kept and the cut is said.
#[tokio::test]
async fn a_read_is_bounded_in_bytes() {
    for force_pty in backends() {
        let (mut c, rec) = cfg(force_pty);
        c.term.max_read_bytes = 1024;
        let rig = Rig::with((c, rec));
        rig.ok(json!({"type":"TermOpen","name":"big","command":"bash","args":["-c","seq 1 3000; sleep 30"]}))
            .await;
        rig.ok(json!({"type":"TermWait","name":"big","pattern":"(?m)^3000$","timeout_ms":10000}))
            .await;
        let s = rig.ok(json!({"type":"TermSnapshot","name":"big"})).await;
        assert!(s["text"].as_str().unwrap().len() <= 1200, "pty={force_pty}");
        assert_eq!(s["truncated"], true);
        assert!(
            s["text"].as_str().unwrap().contains("3000"),
            "the tail is kept"
        );
        rig.close_all().await;
    }
}

// ── Policy ──────────────────────────────────────────────────────────────────

fn engine(mutate: impl FnOnce(&mut AppConfig)) -> PolicyEngine {
    let (mut c, _rec) = cfg(false);
    mutate(&mut c);
    PolicyEngine::new(c.clone(), c.actions.clone())
}

fn act(v: Value) -> Action {
    serde_json::from_value(v).unwrap()
}

/// On the host, TermOpen faces the command allowlist exactly like RunCommand.
#[test]
fn on_the_host_term_open_faces_the_command_allowlist() {
    let e = engine(|_| {});
    let open = |cmd: &str| act(json!({"type":"TermOpen","name":"x","command":cmd}));
    assert!(matches!(e.evaluate(&open("python3")), PolicyEffect::Allow));
    assert!(matches!(
        e.evaluate(&open("vim")),
        PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
    ));
}

/// In a detected container TermOpen still faces the allowlist.
#[test]
fn in_a_detected_container_term_open_still_faces_the_allowlist() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("run")).unwrap();
    std::fs::write(root.path().join("run/.containerenv"), "").unwrap();
    let e = engine(|c| {
        c.isolation.mode = IsolationMode::Container;
        c.isolation.container_probe_root = Some(root.path().to_path_buf());
    });
    let open = act(json!({"type":"TermOpen","name":"x","command":"vim"}));
    assert!(matches!(
        e.evaluate(&open),
        PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
    ));
    let listed = act(json!({"type":"TermOpen","name":"x","command":"python3"}));
    assert!(matches!(e.evaluate(&listed), PolicyEffect::Allow));

    // Nor without the markers.
    let empty = tempfile::tempdir().unwrap();
    let e = engine(|c| {
        c.isolation.mode = IsolationMode::Container;
        c.isolation.container_probe_root = Some(empty.path().to_path_buf());
    });
    assert!(matches!(
        e.evaluate(&open),
        PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
    ));
}

/// Typing into a terminal is execution: refused in readonly, asked in confirm.
#[test]
fn term_send_is_execution_for_the_global_modes() {
    let send = act(json!({"type":"TermSend","name":"x","text":"rm -rf /","keys":["Enter"]}));
    let read = act(json!({"type":"TermRead","name":"x"}));
    let ro = engine(|c| c.policy.mode = "readonly".into());
    assert!(matches!(
        ro.evaluate(&send),
        PolicyEffect::Deny(DenyReason::ReadonlyMode)
    ));
    assert!(matches!(ro.evaluate(&read), PolicyEffect::Allow));
    let confirm = engine(|c| c.policy.mode = "confirm".into());
    assert!(matches!(
        confirm.evaluate(&send),
        PolicyEffect::RequireConfirmation(_)
    ));
}

/// Out of the box, opening and typing are off; reading is on.
#[test]
fn defaults_switch_off_what_executes() {
    let d = AppConfig::defaults();
    for (name, on) in [
        ("TermOpen", false),
        ("TermSend", false),
        ("TermResize", false),
        ("TermClose", false),
        ("TermRead", true),
        ("TermWait", true),
        ("TermList", true),
        ("TermSnapshot", true),
    ] {
        assert_eq!(d.actions.get(name), Some(&on), "{name}");
    }
}

/// tmux splits its argv on a trailing `;`. Arguments and typed text that end
/// in `;` must reach the program intact, and must not run a tmux command.
#[tokio::test]
async fn a_trailing_semicolon_reaches_the_program_intact() {
    for force_pty in backends() {
        let rig = Rig::new(force_pty);
        rig.ok(json!({"type":"TermOpen","name":"semi","command":"bash",
            "args":["--norc","--noprofile","-c","echo \"[$0]\"; exec bash --norc --noprofile","kill-server;"]}))
            .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"semi","pattern":"\\[kill-server;\\]","timeout_ms":10000}))
            .await;
        assert_eq!(w["condition"], "pattern", "pty={force_pty}: {w}");
        rig.ok(
            json!({"type":"TermSend","name":"semi","text":"echo typed';'end;","keys":["Enter"]}),
        )
        .await;
        let w = rig
            .ok(json!({"type":"TermWait","name":"semi","pattern":"(?m)^typed;end$","timeout_ms":10000,"diff":false}))
            .await;
        assert_eq!(w["condition"], "pattern", "pty={force_pty}: {w}");
        let l = rig.ok(json!({"type":"TermList"})).await;
        assert_eq!(l["terminals"][0]["alive"], true, "the server survived: {l}");
        rig.close_all().await;
    }
}
