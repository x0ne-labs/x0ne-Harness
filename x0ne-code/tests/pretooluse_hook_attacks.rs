//! `x0ne hook pretooluse`, attacked through the real binary with the
//! payloads Claude Code and Codex send. No model runs: each test is one
//! payload, one decision, one journal entry.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

struct Lab {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    ws: PathBuf,
    out: PathBuf,
    policy: PathBuf,
    sock: UnixDatagram,
    sock_path: PathBuf,
}

struct Answer {
    code: i32,
    decision: String,
    reason: String,
    stdout: String,
    journal: HashMap<String, String>,
}

fn policy_text(ws: &Path, mode: &str) -> String {
    format!(
        r#"[policy]
mode = "{mode}"
default_effect = "deny"
allowed_commands = ["ls", "cat", "bash", "sh", "env", "git", "find", "xargs"]
allowed_read_paths = ["{ws}"]
allowed_write_paths = ["{ws}"]

[actions]
RunCommand = true
WriteFile = true
EditFile = true
ReadFile = true
SearchCode = true
"#,
        ws = ws.display()
    )
}

impl Lab {
    fn new() -> Self {
        Self::with_mode("auto")
    }

    fn with_mode(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let ws = home.join("ws");
        let out = root.join("out");
        for d in [&ws, &out, &home.join(".ssh"), &root.join("xdg/x0ne")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".ssh/id_rsa"), "FAKE KEY").unwrap();
        std::fs::write(home.join(".bashrc"), "# rc").unwrap();
        std::fs::write(ws.join("notes.txt"), "hello").unwrap();
        let policy = root.join("xdg/x0ne/x0ne-runtime.toml");
        std::fs::write(&policy, policy_text(&ws, mode)).unwrap();
        let sock_path = root.join("journal.sock");
        let sock = UnixDatagram::bind(&sock_path).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            _dir: dir,
            root,
            home,
            ws,
            out,
            policy,
            sock,
            sock_path,
        }
    }

    fn run_raw(&self, stdin: &str, extra: &[&str], policy: Option<&Path>) -> Answer {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_x0ne"));
        cmd.args(["hook", "pretooluse", "--journal-socket"])
            .arg(&self.sock_path)
            .args(extra)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home.as_path())
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .current_dir(&self.ws)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = policy {
            cmd.arg("--policy").arg(p);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
        let v: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        let h = &v["hookSpecificOutput"];
        Answer {
            code: o.status.code().unwrap_or(-1),
            decision: h["permissionDecision"].as_str().unwrap_or("").into(),
            reason: h["permissionDecisionReason"].as_str().unwrap_or("").into(),
            stdout,
            journal: self.journal(),
        }
    }

    fn journal(&self) -> HashMap<String, String> {
        let mut buf = vec![0u8; 65536];
        let n = self.sock.recv(&mut buf).expect("no journal entry");
        parse_native(&buf[..n])
    }

    fn tool(&self, tool: &str, input: Value) -> Answer {
        self.tool_as(tool, input, &[])
    }

    fn tool_as(&self, tool: &str, input: Value, extra: &[&str]) -> Answer {
        let payload = json!({
            "session_id": "sess-524",
            "cwd": self.ws,
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": input,
            "tool_use_id": "toolu_524",
        });
        self.run_raw(&payload.to_string(), extra, Some(&self.policy))
    }

    fn bash(&self, line: &str) -> Answer {
        self.tool("Bash", json!({"command": line, "description": "t"}))
    }
}

fn parse_native(d: &[u8]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < d.len() {
        let nl = i + d[i..].iter().position(|&b| b == b'\n').unwrap();
        let line = &d[i..nl];
        if let Some(eq) = line.iter().position(|&b| b == b'=') {
            m.insert(
                String::from_utf8_lossy(&line[..eq]).into_owned(),
                String::from_utf8_lossy(&line[eq + 1..]).into_owned(),
            );
            i = nl + 1;
        } else {
            let key = String::from_utf8_lossy(line).into_owned();
            let len = u64::from_le_bytes(d[nl + 1..nl + 9].try_into().unwrap()) as usize;
            let v = &d[nl + 9..nl + 9 + len];
            m.insert(key, String::from_utf8_lossy(v).into_owned());
            i = nl + 9 + len + 1;
        }
    }
    m
}

#[track_caller]
fn refused(a: &Answer, rule: &str) {
    assert_eq!(a.decision, "deny", "not refused: {}", a.stdout);
    assert_eq!(a.journal["X0NE_OUTCOME"], "refused");
    assert_eq!(a.journal["X0NE_RULE"], rule, "reason: {}", a.reason);
    assert_eq!(a.code, 0);
}

#[track_caller]
fn allowed(a: &Answer) {
    assert_eq!(a.decision, "allow", "not allowed: {}", a.stdout);
    assert_eq!(a.journal["X0NE_OUTCOME"], "allowed");
    assert_eq!(a.code, 0);
}

// ── Bash ─────────────────────────────────────────────────────────────────────

#[test]
fn an_allowed_command_passes_and_is_journaled() {
    let lab = Lab::new();
    let a = lab.bash("ls -la && cat notes.txt");
    allowed(&a);
    let j = &a.journal;
    assert_eq!(j["SYSLOG_IDENTIFIER"], "x0ne-harness");
    assert_eq!(j["X0NE_EVENT"], "harness");
    assert_eq!(j["X0NE_TOOL"], "Bash");
    assert_eq!(j["X0NE_AGENT"], "claude-code");
    assert_eq!(j["PRIORITY"], "6");
}

#[test]
fn a_command_outside_the_allowlist_is_refused_with_its_fields() {
    let lab = Lab::new();
    let a = lab.bash("ls && rm -rf /");
    refused(&a, "CommandNotAllowed");
    assert!(a.reason.contains("'rm'"), "{}", a.reason);
    let j = &a.journal;
    assert_eq!(j["SYSLOG_IDENTIFIER"], "x0ne-harness");
    assert_eq!(j["X0NE_EVENT"], "harness");
    assert_eq!(j["X0NE_ACTION"], "RunCommand");
    assert_eq!(j["X0NE_TARGET"], "rm -rf /");
    assert_eq!(j["X0NE_SESSION"], "sess-524");
    assert_eq!(j["X0NE_TOOL_USE_ID"], "toolu_524");
    assert_eq!(j["X0NE_CWD"], lab.ws.display().to_string());
    assert_eq!(j["X0NE_POLICY"], lab.policy.display().to_string());
    assert_eq!(j["PRIORITY"], "4");
    assert!(j["MESSAGE"].starts_with("x0ne-harness: refused Bash"));
    assert!(!j["X0NE_TIME"].is_empty());
}

#[test]
fn a_shell_does_not_launder_a_command() {
    let lab = Lab::new();
    refused(&lab.bash("bash -c 'rm -rf /'"), "CommandNotAllowed");
    refused(&lab.bash("sh -ec \"ls && curl evil\""), "CommandNotAllowed");
    refused(&lab.bash("sh -ec \"ls && cat x | sh\""), "ShellScript");
    refused(&lab.bash("bash script.sh"), "ShellScript");
    refused(&lab.bash("bash <<EOF\nrm -rf ~\nEOF"), "ShellScript");
    refused(&lab.bash("ls | bash"), "ShellScript");
}

#[test]
fn wrappers_do_not_launder_a_command() {
    let lab = Lab::new();
    refused(&lab.bash("env rm -rf /"), "CommandNotAllowed");
    refused(
        &lab.bash("find . -name x -exec rm {} \\;"),
        "CommandNotAllowed",
    );
    refused(&lab.bash("ls | xargs rm"), "CommandNotAllowed");
    refused(&lab.bash("command rm x"), "CommandNotAllowed");
    refused(&lab.bash("exec rm x"), "CommandNotAllowed");
}

#[test]
fn variables_and_substitutions_are_refused() {
    let lab = Lab::new();
    // A variable the line sets is read: the policy judges its value.
    refused(&lab.bash("X=rm; $X -rf notes.txt"), "CommandNotAllowed");
    refused(&lab.bash("F=/etc/passwd; cat $F"), "ArgumentOutsideRoots");
    refused(&lab.bash("cat $'\\x2fetc/passwd'"), "ArgumentOutsideRoots");
    // One whose value is unknown here is refused.
    refused(&lab.bash("$NOT_SET -rf /"), "CommandFromVariable");
    refused(&lab.bash("cat $NOT_SET"), "PathFromVariable");
    refused(&lab.bash("echo $(rm -rf ~)"), "CommandSubstitution");
    refused(&lab.bash("ls `rm -rf ~`"), "CommandSubstitution");
    refused(&lab.bash("LD_PRELOAD=/tmp/x.so ls"), "EnvironmentVariable");
    refused(&lab.bash("PATH=/tmp/evil:$PATH ls"), "EnvironmentVariable");
    refused(
        &lab.bash("export BASH_ENV=/tmp/x; bash -c ls"),
        "EnvironmentVariable",
    );
    refused(&lab.bash("eval 'rm -rf /'"), "ShellSyntax");
    refused(&lab.bash("/bin/rm -rf x"), "CommandNotAllowed");
}

#[test]
fn heredocs_are_read() {
    let lab = Lab::new();
    // Quoted: the body is text, `$(…)` included. Written inside the workspace.
    allowed(&lab.bash("cat > notes2.txt <<'EOF'\n$(rm -rf ~)\nEOF"));
    // Unquoted: the shell would run the substitution.
    refused(
        &lab.bash("cat <<EOF\n$(rm -rf ~)\nEOF"),
        "CommandSubstitution",
    );
    // The redirect target is a write like any other.
    refused(
        &lab.bash("cat > ../../out/x <<'EOF'\ndata\nEOF"),
        "PathNotInAllowlist",
    );
}

#[test]
fn redirections_outside_the_write_roots_are_refused() {
    let lab = Lab::new();
    allowed(&lab.bash("ls > listing.txt 2>&1"));
    refused(&lab.bash("ls > ../../out/x"), "PathNotInAllowlist");
    refused(&lab.bash("cd .. && ls > x"), "PathNotInAllowlist");
    refused(&lab.bash("ls >> ~/.bashrc"), "PathNotInAllowlist");
    allowed(&lab.bash("ls 2>/dev/null"));
}

#[test]
fn secrets_named_in_arguments_are_refused() {
    let lab = Lab::new();
    refused(&lab.bash("cat ~/.ssh/id_rsa"), "PathMatchesDenyPattern");
    refused(
        &lab.bash("cat \"$HOME/.ssh/id_rsa\""),
        "PathMatchesDenyPattern",
    );
    refused(&lab.bash("cat $SOMEWHERE/.ssh/id_rsa"), "PathFromVariable");
    // A link in the workspace to the key: resolved before matching.
    std::os::unix::fs::symlink(lab.home.join(".ssh/id_rsa"), lab.ws.join("k")).unwrap();
    refused(&lab.bash("cat k"), "PathMatchesDenyPattern");
}

#[test]
fn paths_named_in_arguments_stay_under_the_roots() {
    let lab = Lab::new();
    allowed(&lab.bash("cat notes.txt ./notes.txt /usr/share/x"));
    refused(&lab.bash("cat /etc/passwd"), "ArgumentOutsideRoots");
    refused(
        &lab.bash("ls | cat > notes.txt ../../out/x"),
        "ArgumentOutsideRoots",
    );
    refused(&lab.bash("git -C ../../out status"), "ArgumentOutsideRoots");
    refused(
        &lab.bash("git --git-dir=/srv/other status"),
        "ArgumentOutsideRoots",
    );
    std::os::unix::fs::symlink(&lab.out, lab.ws.join("o")).unwrap();
    refused(&lab.bash("cat o/x"), "ArgumentOutsideRoots");
    // A glob is judged by its fixed directory and by what it matches now.
    refused(&lab.bash("cat ../../out/*"), "ArgumentOutsideRoots");
    std::fs::write(lab.out.join("leak"), "x").unwrap();
    refused(&lab.bash("cat o/*"), "ArgumentOutsideRoots");
    refused(&lab.bash("cat ~/.ss?/id_*"), "ArgumentOutsideRoots");
    std::fs::create_dir_all(lab.ws.join(".ssh")).unwrap();
    std::fs::write(lab.ws.join(".ssh/id_ed25519"), "FAKE").unwrap();
    refused(&lab.bash("cat .ss?/*"), "PathMatchesDenyPattern");
    allowed(&lab.bash("cat *.txt"));
}

#[test]
fn the_hook_and_the_policy_cannot_be_rewritten_by_the_agent() {
    let lab = Lab::new();
    refused(
        &lab.bash("echo '{}' > .claude/settings.json"),
        "ProtectedPath",
    );
    refused(
        &lab.tool(
            "Write",
            json!({"file_path": lab.ws.join(".claude/settings.local.json"), "content": "{}"}),
        ),
        "ProtectedPath",
    );
    refused(
        &lab.tool(
            "Edit",
            json!({"file_path": lab.policy, "old_string": "deny", "new_string": "allow"}),
        ),
        "ProtectedPath",
    );
    refused(
        &lab.bash(&format!("cat x > {}", lab.policy.display())),
        "ProtectedPath",
    );
}

// ── Edit / Write / Read ──────────────────────────────────────────────────────

#[test]
fn writes_follow_the_write_roots() {
    let lab = Lab::new();
    allowed(&lab.tool(
        "Write",
        json!({"file_path": lab.ws.join("a.rs"), "content": "x"}),
    ));
    allowed(&lab.tool(
        "Edit",
        json!({"file_path": lab.ws.join("notes.txt"), "old_string": "a", "new_string": "b"}),
    ));
    let a = lab.tool(
        "Write",
        json!({"file_path": lab.out.join("a.rs"), "content": "x"}),
    );
    refused(&a, "PathNotInAllowlist");
    assert_eq!(a.journal["X0NE_ACTION"], "WriteFile");
    refused(
        &lab.tool(
            "Edit",
            json!({"file_path": lab.home.join(".bashrc"), "old_string": "a", "new_string": "b"}),
        ),
        "PathNotInAllowlist",
    );
    refused(
        &lab.tool(
            "NotebookEdit",
            json!({"notebook_path": lab.out.join("n.ipynb"), "new_source": "x"}),
        ),
        "PathNotInAllowlist",
    );
}

#[test]
fn relative_paths_and_links_do_not_escape() {
    let lab = Lab::new();
    // `..` in an absolute path.
    let dotdot = format!("{}/../../out/x", lab.ws.display());
    refused(
        &lab.tool("Write", json!({"file_path": dotdot, "content": "x"})),
        "PathNotInAllowlist",
    );
    // Relative path (Claude Code sends absolute ones; Codex patches are relative).
    refused(
        &lab.tool("Write", json!({"file_path": "../../out/x", "content": "x"})),
        "PathNotInAllowlist",
    );
    // A link to a directory outside.
    std::os::unix::fs::symlink(&lab.out, lab.ws.join("escape")).unwrap();
    refused(
        &lab.tool(
            "Write",
            json!({"file_path": lab.ws.join("escape/x"), "content": "x"}),
        ),
        "PathNotInAllowlist",
    );
    // A link to a file outside.
    std::os::unix::fs::symlink(lab.home.join(".bashrc"), lab.ws.join("rc")).unwrap();
    refused(
        &lab.tool(
            "Edit",
            json!({"file_path": lab.ws.join("rc"), "old_string": "a", "new_string": "b"}),
        ),
        "PathNotInAllowlist",
    );
}

#[test]
fn reads_follow_the_read_roots_and_deny_patterns() {
    let lab = Lab::new();
    allowed(&lab.tool("Read", json!({"file_path": lab.ws.join("notes.txt")})));
    refused(
        &lab.tool("Read", json!({"file_path": lab.home.join(".ssh/id_rsa")})),
        "PathMatchesDenyPattern",
    );
    refused(
        &lab.tool("Read", json!({"file_path": lab.out.join("x")})),
        "PathNotInAllowlist",
    );
    allowed(&lab.tool("Grep", json!({"pattern": "x"})));
    refused(
        &lab.tool("Glob", json!({"pattern": "*", "path": "/etc"})),
        "PathNotInAllowlist",
    );
    refused(
        &lab.tool("Glob", json!({"pattern": "../../out/**"})),
        "PathNotInAllowlist",
    );
    refused(
        &lab.tool("Glob", json!({"pattern": "/etc/*.conf"})),
        "PathNotInAllowlist",
    );
    refused(
        &lab.tool("Grep", json!({"pattern": "x", "glob": "/etc/**"})),
        "PathNotInAllowlist",
    );
}

// ── Modes, failures, unknowns ────────────────────────────────────────────────

#[test]
fn confirm_mode_asks_and_readonly_mode_refuses() {
    let lab = Lab::with_mode("confirm");
    let a = lab.bash("ls");
    assert_eq!(a.decision, "ask", "{}", a.stdout);
    assert_eq!(a.journal["X0NE_OUTCOME"], "asked");
    // The lists still refuse first: a question is never offered for a refusal.
    refused(&lab.bash("rm x"), "CommandNotAllowed");

    let lab = Lab::with_mode("readonly");
    refused(
        &lab.tool(
            "Write",
            json!({"file_path": lab.ws.join("a"), "content": ""}),
        ),
        "ReadonlyMode",
    );
    allowed(&lab.tool("Read", json!({"file_path": lab.ws.join("notes.txt")})));
}

#[test]
fn no_policy_blocks_everything() {
    let lab = Lab::new();
    let payload = json!({"hook_event_name": "PreToolUse", "cwd": lab.ws, "tool_name": "Bash", "tool_input": {"command": "ls"}});
    // Explicit file missing.
    let a = lab.run_raw(
        &payload.to_string(),
        &[],
        Some(&lab.root.join("missing.toml")),
    );
    assert_eq!(a.code, 2);
    assert_eq!(a.decision, "deny");
    assert_eq!(a.journal["X0NE_RULE"], "NoPolicy");
    // Nothing found by the search.
    std::fs::remove_file(&lab.policy).unwrap();
    let a = lab.run_raw(&payload.to_string(), &[], None);
    assert_eq!(a.code, 2);
    assert_eq!(a.journal["X0NE_POLICY"], "none");
    // An empty file is no policy.
    std::fs::write(&lab.policy, "# nothing\n").unwrap();
    assert_eq!(lab.run_raw(&payload.to_string(), &[], None).code, 2);
}

#[test]
fn bad_input_blocks() {
    let lab = Lab::new();
    for bad in ["not json", "{}", r#"{"tool_name":"Bash"}"#] {
        let a = lab.run_raw(bad, &[], Some(&lab.policy));
        assert_eq!(a.code, 2, "{bad}");
        assert_eq!(a.journal["X0NE_RULE"], "BadInput");
    }
}

#[test]
fn an_unmapped_tool_is_refused() {
    let lab = Lab::new();
    refused(
        &lab.tool("WebFetch", json!({"url": "https://x"})),
        "UnknownTool",
    );
}

// ── Codex ────────────────────────────────────────────────────────────────────

#[test]
fn codex_shell_and_apply_patch() {
    let lab = Lab::new();
    let codex = ["--agent", "codex"];
    let a = lab.tool_as("Bash", json!({"command": "ls"}), &codex);
    assert_eq!((a.code, a.stdout.trim()), (0, ""), "Codex allow is silent");
    assert_eq!(a.journal["X0NE_AGENT"], "codex");
    refused(
        &lab.tool_as("Bash", json!({"command": "rm -rf /"}), &codex),
        "CommandNotAllowed",
    );
    refused(
        &lab.tool_as(
            "Bash",
            json!({"command": ["bash", "-lc", "rm -rf /"]}),
            &codex,
        ),
        "CommandNotAllowed",
    );
    let patch = "*** Begin Patch\n*** Add File: ../../out/evil\n+x\n*** End Patch";
    refused(
        &lab.tool_as("apply_patch", json!({"command": patch}), &codex),
        "PathNotInAllowlist",
    );
    let ok = "*** Begin Patch\n*** Update File: notes.txt\n@@\n-hello\n+bye\n*** End Patch";
    let a = lab.tool_as("apply_patch", json!({"command": ok}), &codex);
    assert_eq!(a.journal["X0NE_OUTCOME"], "allowed");

    // Codex cannot ask: a question becomes a refusal.
    let lab = Lab::with_mode("confirm");
    let a = lab.tool_as("Bash", json!({"command": "ls"}), &codex);
    assert_eq!(a.decision, "deny");
    assert_eq!(a.journal["X0NE_OUTCOME"], "asked");
}
