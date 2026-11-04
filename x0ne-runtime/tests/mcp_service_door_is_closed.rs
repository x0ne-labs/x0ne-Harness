//! SECURITY: the MCP service door.
//!
//! Reproduces, on the REAL binary, the chain found by the pre-publication
//! audit (2026-08-15). Every link came from a git-tracked file:
//!
//!   1. `.mcp.json` (tracked then, no longer shipped) and `make claude-add`
//!      registered `x0ne-runtime --mode mcp --log-level error` — with NO `--config`.
//!   2. `AppConfig::find_and_load(None)` (config.rs:579) then falls through to
//!      `$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`.
//!   3. `make install` (Makefile:164-171) deposits `x0ne-runtime/x0ne-runtime.toml`
//!      at exactly that path, and that reference file carried
//!      `mode = "auto"`, `default_effect = "allow"`, `allowed_commands = []`,
//!      `RunCommand = true`.
//!   4. `check_command_allowed` (policy.rs:586) returned `None` — "no
//!      restriction configured" — for an EMPTY allowlist.
//!
//! Net effect: policy mode `auto` never yields `RequireConfirmation`, so the
//! fail-closed refusal at mcp/server.rs:194 never fires, and any MCP client
//! got arbitrary `run_command` on the host with no human in the loop.
//!
//! A failure of these tests means that door is open again.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The args the audited `.mcp.json` carried, verbatim: every spawn below
/// starts from the invocation that was open.
const MCP_JSON_ARGS: &[&str] = &["--mode", "mcp", "--log-level", "error"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// A throwaway HOME that looks like a fresh `make install`: the reference
/// TOML copied to `$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`, and a CWD that
/// deliberately holds NO `x0ne-runtime.toml` so the cwd rung of
/// `find_and_load` is not what we are measuring.
struct FreshInstall {
    dir: PathBuf,
}

impl FreshInstall {
    fn new(tag: &str) -> FreshInstall {
        let dir = std::env::temp_dir().join(format!(
            "x0ne-mcp-door-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config").join("x0ne")).expect("temp config dir");
        std::fs::create_dir_all(dir.join("cwd")).expect("temp cwd");
        FreshInstall { dir }
    }

    /// Exactly what `make install` does: copy the repo's reference TOML to the
    /// canonical XDG path. Copied, never inlined, so the test measures the file
    /// the repo actually ships.
    fn install_reference_config(&self) -> &FreshInstall {
        let src = workspace_root()
            .join("x0ne-runtime")
            .join("x0ne-runtime.toml");
        let dst = self.config_dir().join("x0ne-runtime.toml");
        std::fs::copy(&src, &dst).expect("copy reference config as make install does");
        self
    }

    fn write_config(&self, name: &str, body: &str) -> PathBuf {
        let p = self.config_dir().join(name);
        std::fs::write(&p, body).expect("write config");
        p
    }

    fn config_dir(&self) -> PathBuf {
        // This test must read the same directory as
        // production, otherwise it passes vacuously.
        self.dir.join(".config").join("x0ne")
    }

    fn cwd(&self) -> PathBuf {
        self.dir.join("cwd")
    }

    fn canary(&self) -> PathBuf {
        self.dir.join("PWNED")
    }
}

impl Drop for FreshInstall {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct McpClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    /// Spawn the runtime the way an MCP host does. `extra_args` lets a test
    /// add `--config`; passing none reproduces the audited invocation byte for byte.
    fn spawn(install: &FreshInstall, extra_args: &[&str]) -> McpClient {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        cmd.args(MCP_JSON_ARGS);
        cmd.args(extra_args);
        // The CWD an MCP host launches from — and it holds no x0ne-runtime.toml.
        cmd.current_dir(install.cwd());
        // A pristine HOME: this is the "fresh install" the audit describes.
        cmd.env("HOME", &install.dir);
        cmd.env("XDG_CONFIG_HOME", install.dir.join(".config"));
        // Without PATH, `Command::new("touch")` cannot resolve and the test
        // would pass for the wrong reason.
        cmd.env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        );

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn x0ne-runtime");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        McpClient {
            child: Some(child),
            stdin: Some(stdin),
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        // A runtime that refused to start has a closed stdin: that is a
        // legitimate outcome, not a test error.
        let stdin = self.stdin.as_mut()?;
        if writeln!(stdin, "{req}").is_err() || stdin.flush().is_err() {
            return None;
        }
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&line).ok(),
        }
    }

    fn initialize(&mut self) -> Option<serde_json::Value> {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "audit-repro", "version": "0"}
            }),
        )
    }

    fn call_tool(&mut self, name: &str, args: serde_json::Value) -> Option<serde_json::Value> {
        self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        )
    }

    /// Everything the child wrote to stderr, after closing its stdin.
    fn finish(&mut self) -> String {
        self.stdin.take();
        match self.child.take() {
            Some(c) => {
                let out = c.wait_with_output().expect("wait for runtime");
                String::from_utf8_lossy(&out.stderr).into_owned()
            }
            None => String::new(),
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// A complete, minimal `[policy]` table. Several fields have no serde default,
/// so a partial table is a parse error rather than the posture under test —
/// this keeps each test's inline config down to the two lines it is about.
fn policy_toml(policy_mode: &str, default_effect: &str, extra: &str) -> String {
    format!(
        r#"
[runtime]
mode = "mcp"

[policy]
mode = "{policy_mode}"
default_effect = "{default_effect}"
max_read_bytes = 1048576
max_list_entries = 512
allowed_commands = []
allowed_read_paths = ["/tmp"]
allowed_write_paths = ["/tmp"]
deny_path_patterns = []
{extra}
"#
    )
}

/// Did the reply carry an executed command's output rather than a refusal?
fn reply_text(reply: &serde_json::Value) -> String {
    reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════
//  1. The reproduction: the exact `.mcp.json` invocation, on a fresh install.
// ═══════════════════════════════════════════════════════════════════════════

/// SECURITY: an MCP runtime launched the way the audited `.mcp.json` launched it must
/// NEVER run an arbitrary host command without a human.
///
/// A failure here means any MCP client — any prompt-injected agent behind one
/// — has a shell on the host. This is the audit's finding n°1, reproduced end
/// to end on the real binary: no `--config`, a CWD with no policy file, and a
/// HOME carrying the reference config that `make install` deposits.
///
/// After the fix the runtime refuses to *start* in this configuration, so
/// there is no session in which to ask. Either way the assertion is the same
/// and it is the one that matters: `id` did not run, `touch` did not land.
#[test]
fn mcp_launched_as_the_manifest_does_never_executes_an_arbitrary_command() {
    let install = FreshInstall::new("repro");
    install.install_reference_config();

    let mut client = McpClient::spawn(&install, &[]);
    client.initialize();

    // The audit's probe, verbatim.
    let id_reply = client.call_tool("run_command", serde_json::json!({"command": "id"}));
    let id_text = id_reply.as_ref().map(reply_text).unwrap_or_default();

    // A second probe whose success leaves physical evidence on the host,
    // independent of how the reply happens to be formatted.
    let canary = install.canary();
    client.call_tool(
        "run_command",
        serde_json::json!({
            "command": "touch",
            "args": [canary.to_str().unwrap()]
        }),
    );

    let stderr = client.finish();

    assert!(
        !canary.exists(),
        "SECURITY: an MCP client created {} on the host with no human in the \
         loop. `run_command` executed. stderr:\n{stderr}",
        canary.display()
    );
    assert!(
        !id_text.contains("uid="),
        "SECURITY: `run_command {{\"command\":\"id\"}}` EXECUTED and returned \
         host identity to an unauthenticated MCP client: {id_text}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. Layer (b): a runtime with no reachable human refuses policy mode `auto`.
// ═══════════════════════════════════════════════════════════════════════════

/// SECURITY: `mode = "auto"` means "enforce the lists, the ORCHESTRATOR owns
/// the human gate" (invariant 2). In `--mode mcp` there is no orchestrator and
/// no human: `auto` there means "no gate at all". The runtime must refuse to
/// start rather than serve.
#[test]
fn mcp_mode_refuses_to_start_under_policy_mode_auto() {
    let install = FreshInstall::new("auto-refused");
    let cfg = install.write_config("auto.toml", &policy_toml("auto", "deny", ""));

    let mut client = McpClient::spawn(&install, &["--config", cfg.to_str().unwrap()]);
    let init = client.initialize();
    let stderr = client.finish();

    assert!(
        init.is_none(),
        "SECURITY: the runtime completed an MCP handshake while in policy mode \
         `auto` — nothing can ever require confirmation, so mcp/server.rs's \
         fail-closed refusal is dead code. Reply: {init:?}"
    );
    assert!(
        stderr.contains("policy.mode"),
        "the refusal must name the lever the operator has to move \
         (`policy.mode`). stderr was:\n{stderr}"
    );
}

/// The two modes that DO have a defined meaning without a human must still
/// start — a fail-closed check that refuses everything is a broken runtime,
/// not a secure one.
#[test]
fn mcp_mode_starts_under_confirm_and_readonly() {
    for mode in ["confirm", "readonly"] {
        let install = FreshInstall::new(&format!("ok-{mode}"));
        let cfg = install.write_config("ok.toml", &policy_toml(mode, "deny", ""));

        let mut client = McpClient::spawn(&install, &["--config", cfg.to_str().unwrap()]);
        let init = client.initialize();
        assert!(
            init.is_some(),
            "policy mode `{mode}` is a legitimate MCP posture and must start"
        );
    }
}

/// SECURITY: under `confirm`, the gate at mcp/server.rs:194 is finally
/// reachable — and it refuses, because no confirmation can ever arrive over a
/// stdio MCP pipe. This is the fail-closed behaviour invariant 1 demands.
#[test]
fn a_writing_action_is_refused_rather_than_confirmed_over_mcp() {
    let install = FreshInstall::new("confirm-refuses");
    let canary = install.canary();
    let cfg = install.write_config(
        "confirm.toml",
        &(policy_toml("confirm", "allow", "")
            .replace("allowed_commands = []", "allowed_commands = [\"touch\"]")
            + "\n[actions]\nRunCommand = true\n"),
    );

    let mut client = McpClient::spawn(&install, &["--config", cfg.to_str().unwrap()]);
    client.initialize();
    let reply = client.call_tool(
        "run_command",
        serde_json::json!({"command": "touch", "args": [canary.to_str().unwrap()]}),
    );
    let text = reply.as_ref().map(reply_text).unwrap_or_default();
    client.finish();

    assert!(
        !canary.exists(),
        "SECURITY: a RunCommand that requires confirmation EXECUTED over MCP, \
         where no confirmation can ever be delivered."
    );
    assert!(
        text.contains("requires user approval"),
        "the refusal should say an approval was required and could not be \
         obtained. Got: {text}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. Layer (c): the CWD does not get to write the policy.
// ═══════════════════════════════════════════════════════════════════════════

/// SECURITY: `./x0ne-runtime.toml` used to be the SECOND rung of
/// `find_and_load`, ahead of the XDG install. An agent that can write a file
/// in the directory a runtime is launched from could therefore author that
/// runtime's own policy. Convenient for development; a privilege escalation
/// everywhere else. It is now opt-in.
#[test]
fn a_policy_file_in_the_cwd_is_ignored_without_the_opt_in() {
    let install = FreshInstall::new("cwd-ignored");
    let canary = install.canary();
    let planted = install.cwd().join("x0ne-runtime.toml");
    // The policy an agent would plant: wide open, and bootable, so that the
    // ONLY thing standing between it and a host command is the cwd rung.
    std::fs::write(
        &planted,
        policy_toml("auto", "allow", "")
            .replace("allowed_commands = []", "allowed_commands = [\"touch\"]")
            + "\n[actions]\nRunCommand = true\n",
    )
    .expect("plant cwd config");
    // And a legitimate, hardened install alongside it.
    install.write_config("x0ne-runtime.toml", &policy_toml("readonly", "deny", ""));

    // `--upstream-human-gate` neutralizes the OTHER defence (the headless
    // posture check would refuse this planted `auto` config outright). Without
    // it this test passes whether or not the cwd rung is closed, which is how
    // the first version of it survived a mutation that re-opened the cwd —
    // the same "flag is in the code but not in the path" trap the repo has
    // hit four times. Here the cwd rung is the only variable.
    let mut client = McpClient::spawn(&install, &["--upstream-human-gate"]);
    client.initialize();
    client.call_tool(
        "run_command",
        serde_json::json!({"command": "touch", "args": [canary.to_str().unwrap()]}),
    );
    let stderr = client.finish();

    assert!(
        !canary.exists(),
        "SECURITY: a planted ./x0ne-runtime.toml became the runtime's policy \
         and authorized host command execution. stderr:\n{stderr}"
    );
    // And the direct statement of the property, independent of what either
    // file happens to permit: the runtime announced the installed policy, not
    // the planted one.
    assert!(
        !stderr.contains(planted.to_str().unwrap()),
        "the runtime loaded the planted cwd config. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(install.config_dir().to_str().unwrap()),
        "the runtime should have fallen through to the XDG install. stderr:\n{stderr}"
    );
}

/// The developer convenience still exists — behind an explicit environment
/// opt-in, so using it is a decision someone made rather than a default.
#[test]
fn the_cwd_config_is_honoured_when_explicitly_opted_in() {
    let install = FreshInstall::new("cwd-optin");
    std::fs::write(
        install.cwd().join("x0ne-runtime.toml"),
        policy_toml("readonly", "deny", ""),
    )
    .expect("plant cwd config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
    cmd.args(MCP_JSON_ARGS)
        .current_dir(install.cwd())
        .env("HOME", &install.dir)
        .env("XDG_CONFIG_HOME", install.dir.join(".config"))
        .env("X0NE_RUNTIME_ALLOW_CWD_CONFIG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    let got = stdout.read_line(&mut line).map(|n| n > 0).unwrap_or(false);
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        got,
        "with the opt-in set, the cwd config must be loaded and the runtime \
         must start. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("x0ne-runtime.toml"),
        "the runtime must announce WHICH policy file it loaded, by absolute \
         path. stderr:\n{stderr}"
    );
}

/// SECURITY: whatever it loads, the runtime must say so out loud. A silent
/// fallback to compiled-in defaults, or to a file the operator forgot about,
/// is how the audit's chain stayed invisible for so long.
#[test]
fn the_runtime_announces_which_policy_file_it_loaded() {
    let install = FreshInstall::new("announce");
    let cfg = install.write_config("named.toml", &policy_toml("readonly", "deny", ""));

    let mut client = McpClient::spawn(&install, &["--config", cfg.to_str().unwrap()]);
    client.initialize();
    let stderr = client.finish();

    assert!(
        stderr.contains(cfg.to_str().unwrap()),
        "the loaded policy path must appear in the startup log, absolute. \
         stderr:\n{stderr}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. Layer (a): the tracked entry points name a policy file.
// ═══════════════════════════════════════════════════════════════════════════

/// SECURITY: every git-tracked way of registering this runtime as an MCP
/// server must pass `--config`.
///
/// This is the layer that failed. `.mcp.json` and `make claude-add` both
/// launched `x0ne-runtime --mode mcp --log-level error` with no config, so the
/// search path silently landed on the permissive CLI reference profile. The
/// hardened profile had been in this repo the whole time and nothing pointed
/// at it. Code review did not catch it for months, so a test does.
///
/// A failure here means a fresh clone once again ships an MCP entry point that
/// picks its policy by accident.
#[test]
fn every_tracked_mcp_entry_point_names_its_policy_file() {
    let root = workspace_root();

    // `.mcp.json` is not shipped (a fresh clone has none); a local one is
    // still held to the rule.
    if let Ok(mcp_json) = std::fs::read_to_string(root.join(".mcp.json")) {
        let parsed: serde_json::Value = serde_json::from_str(&mcp_json).expect("parse .mcp.json");
        let args = parsed["mcpServers"]["x0ne"]["args"]
            .as_array()
            .expect(".mcp.json must define the `x0ne` server args");
        let args: Vec<&str> = args.iter().filter_map(|a| a.as_str()).collect();
        assert!(
            args.contains(&"--config"),
            ".mcp.json registers x0ne-runtime without --config: it will inherit whatever \
             ~/.config/x0ne/x0ne-runtime.toml happens to say. Args were: {args:?}"
        );
    }

    // `make claude-add` — the other registration path, and the one that writes
    // into the user's real Claude Code config.
    let makefile = std::fs::read_to_string(root.join("Makefile")).expect("read Makefile");
    let add_line = makefile
        .lines()
        .find(|l| l.contains("claude mcp add x0ne ") && l.contains("x0ne-runtime"))
        .expect("Makefile must still register the `x0ne` MCP server");
    assert!(
        add_line.contains("--config"),
        "`make claude-add` registers x0ne-runtime without --config: {add_line}"
    );
}

/// SECURITY: `make install` must actually deposit the profile the entry points
/// name. A `--config` pointing at a file nobody installs is a runtime that
/// fails to start — better than a hole, but it means the hardened path was
/// never really wired, which is this repo's recurring defect.
#[test]
fn make_install_deploys_the_profile_the_entry_points_name() {
    let root = workspace_root();
    let makefile = std::fs::read_to_string(root.join("Makefile")).expect("read Makefile");
    assert!(
        makefile.contains("x0ne-runtime-mcp.toml"),
        "`make install` never deposits x0ne-runtime-mcp.toml, which the MCP entries name"
    );
    assert!(
        makefile.contains("x0ne-runtime-mcp-readonly.toml"),
        "`make install` must copy the hardened profile from the repo"
    );
    assert!(
        root.join("x0ne-runtime-mcp-readonly.toml").exists(),
        "the hardened profile the Makefile copies must exist in the repo"
    );
}
