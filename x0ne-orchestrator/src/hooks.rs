//! Hooks: user-configured commands run on session events.
//!
//! `pre_tool` (PreToolUse) may only deny. It cannot approve, rewrite the input
//! or waive another gate. It runs before every route, connector calls
//! (`<connector>.<tool>`) included. A hook that fails, times out or answers
//! garbage denies.
//!
//! `post_tool` (PostToolUse), `session_start` (SessionStart) and `stop` (Stop)
//! only observe: their output and exit status are ignored, nothing they print
//! reaches the model, and a failure is logged without stopping the session.
//!
//! Every payload carries `hook_event_name`, `session_id` and `cwd`. Event and
//! field names follow the hook format `docs/hooks.md` documents.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

/// `[hooks]` in the user's config. Not in `PROJECT_SETTABLE`: a project file
/// cannot start a process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool: Vec<PreToolHook>,
    /// After an action ran and succeeded. `matcher` matches the action type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool: Vec<PreToolHook>,
    /// When a session starts. `matcher` matches the source: `startup`,
    /// `resume` or `clear`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_start: Vec<PreToolHook>,
    /// When a turn ends, unless the user cancelled it or the model failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<EventHook>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.pre_tool.is_empty()
            && self.post_tool.is_empty()
            && self.session_start.is_empty()
            && self.stop.is_empty()
    }
}

/// One `[[hooks.stop]]` entry: a command, no matcher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventHook {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// The fields every payload carries. `session_id` is the session log's id,
/// `null` when the session keeps no log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSession {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

impl HookSession {
    /// `session_id`, and the process's working directory.
    pub fn here(session_id: Option<String>) -> Self {
        Self {
            session_id,
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string()),
        }
    }

    fn payload(&self, event: &str) -> Value {
        json!({
            "hook_event_name": event,
            "session_id": self.session_id,
            "cwd": self.cwd,
        })
    }
}

/// How the session started, SessionStart's `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    Startup,
    Resume,
    Clear,
}

impl SessionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
        }
    }
}

/// An observing hook that did not exit 0. Logged; it changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverFailure {
    pub hook: String,
    pub error: String,
}

/// One `[[hooks.pre_tool]]`, `[[hooks.post_tool]]` or `[[hooks.session_start]]`
/// entry. Exec form only: `command` is spawned with `args`, no shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreToolHook {
    /// `|`-separated patterns; `*` matches any run of characters; `*` alone
    /// or empty matches every tool. `github.*` matches every tool of the
    /// `github` connector.
    #[serde(default)]
    pub matcher: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MAX_REASON: usize = 1000;

/// A hook said no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDenial {
    /// The hook's command, to name it in the journal.
    pub hook: String,
    pub reason: String,
}

impl PreToolHook {
    pub fn matches(&self, tool: &str) -> bool {
        let m = self.matcher.trim();
        if m.is_empty() || m == "*" {
            return true;
        }
        m.split('|')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .any(|p| glob_match(p, tool))
    }
}

/// `*` = any run of characters, everything else literal, whole string.
fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// The JSON a PreToolUse hook reads on stdin.
pub fn hook_input(session: &HookSession, tool: &str, action: &Value) -> Value {
    tool_payload(session, "PreToolUse", tool, action)
}

/// PreToolUse and PostToolUse: the tool and its input, and for a connector
/// tool its name and arguments.
fn tool_payload(session: &HookSession, event: &str, tool: &str, action: &Value) -> Value {
    let mut input = session.payload(event);
    input["tool_name"] = json!(tool);
    input["tool_input"] = action.clone();
    if let Some((connector, t)) = tool.split_once('.') {
        input["connector"] = json!({
            "name": connector,
            "tool": t,
            "arguments": crate::connectors::arguments_of(action),
        });
    }
    input
}

/// Run every hook matching `tool`, in order; the first denial wins.
pub async fn evaluate(
    cfg: &HooksConfig,
    session: &HookSession,
    tool: &str,
    action: &Value,
) -> Option<HookDenial> {
    for h in cfg.pre_tool.iter().filter(|h| h.matches(tool)) {
        if let Some(reason) = run_one(h, &hook_input(session, tool, action)).await {
            return Some(HookDenial {
                hook: h.command.clone(),
                reason: clip(&reason),
            });
        }
    }
    None
}

/// PostToolUse, after `tool` ran and succeeded: `tool_response` is its result
/// envelope, `duration_ms` how long it ran.
pub async fn post_tool(
    cfg: &HooksConfig,
    session: &HookSession,
    tool: &str,
    action: &Value,
    response: &Value,
    duration_ms: u64,
) -> Vec<ObserverFailure> {
    let hooks: Vec<_> = cfg.post_tool.iter().filter(|h| h.matches(tool)).collect();
    if hooks.is_empty() {
        return Vec::new();
    }
    let mut input = tool_payload(session, "PostToolUse", tool, action);
    input["tool_response"] = response.clone();
    input["duration_ms"] = json!(duration_ms);
    observe(
        hooks
            .iter()
            .map(|h| (h.command.as_str(), h.args.as_slice(), h.timeout_secs)),
        &input,
    )
    .await
}

/// SessionStart. `model` is the backend's display name.
pub async fn session_start(
    cfg: &HooksConfig,
    session: &HookSession,
    source: SessionSource,
    model: &str,
) -> Vec<ObserverFailure> {
    let mut input = session.payload("SessionStart");
    input["source"] = json!(source.as_str());
    input["model"] = json!(model);
    observe(
        cfg.session_start
            .iter()
            .filter(|h| h.matches(source.as_str()))
            .map(|h| (h.command.as_str(), h.args.as_slice(), h.timeout_secs)),
        &input,
    )
    .await
}

/// Stop. `last_assistant_message` is the turn's answer, `None` when the turn
/// ended without one (a refusal, a budget). x0ne never continues a turn from a
/// Stop hook, so `stop_hook_active` is always `false`.
pub async fn stop(
    cfg: &HooksConfig,
    session: &HookSession,
    last_assistant_message: Option<&str>,
) -> Vec<ObserverFailure> {
    let mut input = session.payload("Stop");
    input["stop_hook_active"] = json!(false);
    input["last_assistant_message"] = json!(last_assistant_message);
    observe(
        cfg.stop
            .iter()
            .map(|h| (h.command.as_str(), h.args.as_slice(), h.timeout_secs)),
        &input,
    )
    .await
}

/// Run observing hooks in order. What they print is never read: an observer
/// cannot decide. Anything but exit 0 is returned, for the log.
async fn observe<'a>(
    hooks: impl Iterator<Item = (&'a str, &'a [String], Option<u64>)>,
    input: &Value,
) -> Vec<ObserverFailure> {
    let mut failures = Vec::new();
    for (command, args, timeout) in hooks {
        let secs = timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let error = match spawn_with_input(command, args, secs, input).await {
            Err(e) => e,
            Ok(out) if out.status.success() => continue,
            Ok(out) => match out.status.code() {
                Some(c) => format!(
                    "hook exited {c}: {}",
                    clip(&String::from_utf8_lossy(&out.stderr))
                ),
                None => "hook killed by a signal".into(),
            },
        };
        failures.push(ObserverFailure {
            hook: command.to_string(),
            error,
        });
    }
    failures
}

fn clip(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(MAX_REASON) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// `Some(reason)` = deny. Exit 2, or exit 0 with a JSON `deny`. Exit 0 with
/// nothing or `allow` = no decision. Anything else fails closed.
async fn run_one(h: &PreToolHook, input: &Value) -> Option<String> {
    let secs = h.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let out = match spawn_with_input(&h.command, &h.args, secs, input).await {
        Ok(o) => o,
        Err(reason) => return Some(reason),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(2) => Some(if stderr.trim().is_empty() {
            "denied by hook (exit 2)".into()
        } else {
            stderr.into_owned()
        }),
        Some(0) => decision(&stdout),
        Some(c) => Some(format!("hook exited {c}: {}", stderr.trim())),
        None => Some("hook killed by a signal".into()),
    }
}

/// Spawn `command` with `input` on stdin and wait, at most `secs`. `Err` says
/// why it did not finish; the child is killed when the wait is dropped.
async fn spawn_with_input(
    command: &str,
    args: &[String],
    secs: u64,
    input: &Value,
) -> Result<std::process::Output, String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("hook could not start: {e}"))?;
    // Was `child.stdin.take()?` in `run_one`: a `None` there returned "no
    // decision", an allow. Unreachable with a piped stdin; now a failure.
    let mut stdin = child.stdin.take().ok_or("hook has no stdin")?;
    let bytes = input.to_string().into_bytes();
    // A hook that never reads stdin must not wedge the write: the write is
    // inside the timeout and its error is ignored.
    let run = async move {
        let _ = stdin.write_all(&bytes).await;
        drop(stdin);
        child.wait_with_output().await
    };
    match tokio::time::timeout(Duration::from_secs(secs), run).await {
        Err(_) => Err(format!("hook timed out after {secs}s")),
        Ok(Err(e)) => Err(format!("hook failed: {e}")),
        Ok(Ok(o)) => Ok(o),
    }
}

fn decision(stdout: &str) -> Option<String> {
    let t = stdout.trim();
    if t.is_empty() {
        return None;
    }
    let v: Value = match serde_json::from_str(t) {
        Ok(v) => v,
        Err(e) => return Some(format!("hook printed invalid JSON: {e}")),
    };
    let out = v.get("hookSpecificOutput").unwrap_or(&v);
    match out.get("permissionDecision").and_then(Value::as_str) {
        Some("deny") => Some(
            out.get("permissionDecisionReason")
                .and_then(Value::as_str)
                .unwrap_or("denied by hook")
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook that runs `body` through `/bin/sh -c`. It deliberately does NOT
    /// write an executable and exec it: a freshly written+chmod'd script hit
    /// ETXTBSY under the parallel test runner (a sibling test's fork inherited
    /// the still-open writable fd between fork and exec), which `run_one` fails
    /// closed on, so the reason came back "hook could not start: Text file
    /// busy" instead of the decision (measured 2026-10-16: 7/30 loop runs, on
    /// json_deny_on_exit_0_denies and its neighbours). `/bin/sh` is never
    /// written by a test, so the race cannot occur.
    fn sh_hook(matcher: &str, body: &str) -> HooksConfig {
        cmd_hook(matcher, "/bin/sh", &["-c", body])
    }

    fn cmd_hook(matcher: &str, command: &str, args: &[&str]) -> HooksConfig {
        HooksConfig {
            pre_tool: vec![PreToolHook {
                matcher: matcher.into(),
                command: command.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                timeout_secs: Some(5),
            }],
            ..HooksConfig::default()
        }
    }

    fn here() -> HookSession {
        HookSession::here(Some("2026-10-27_12-00-00".into()))
    }

    async fn evaluate(cfg: &HooksConfig, tool: &str, action: &Value) -> Option<HookDenial> {
        super::evaluate(cfg, &here(), tool, action).await
    }

    fn sh(body: &str, timeout_secs: u64) -> PreToolHook {
        PreToolHook {
            matcher: String::new(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), body.into()],
            timeout_secs: Some(timeout_secs),
        }
    }

    fn sh_event(body: &str, timeout_secs: u64) -> EventHook {
        EventHook {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), body.into()],
            timeout_secs: Some(timeout_secs),
        }
    }

    fn read_json(path: &std::path::Path) -> Value {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("the hook wrote nothing to {}: {e}", path.display()));
        serde_json::from_str(&text).unwrap()
    }

    /// Each event hands its hook the common fields and its own.
    #[tokio::test]
    async fn every_payload_carries_the_session_and_the_event_fields() {
        let d = tempfile::tempdir().unwrap();
        let out = |n: &str| d.path().join(n);
        let cat = |n: &str| format!("cat > '{}'", out(n).display());
        let cfg = HooksConfig {
            pre_tool: vec![sh(&cat("pre"), 5)],
            post_tool: vec![sh(&cat("post"), 5)],
            session_start: vec![sh(&cat("start"), 5)],
            stop: vec![sh_event(&cat("stop"), 5)],
        };
        let s = here();
        let action = json!({"type": "ReadFile", "path": "/srv/data/notes.txt"});
        let result = json!({"success": true, "output": {"kind": "generic", "data": {}}});
        assert!(super::evaluate(&cfg, &s, "ReadFile", &action)
            .await
            .is_none());
        assert!(post_tool(&cfg, &s, "ReadFile", &action, &result, 12)
            .await
            .is_empty());
        assert!(
            session_start(&cfg, &s, SessionSource::Resume, "scripted/model")
                .await
                .is_empty()
        );
        assert!(stop(&cfg, &s, Some("the plan")).await.is_empty());

        let cwd = std::env::current_dir().unwrap().display().to_string();
        for (file, event) in [
            ("pre", "PreToolUse"),
            ("post", "PostToolUse"),
            ("start", "SessionStart"),
            ("stop", "Stop"),
        ] {
            let v = read_json(&out(file));
            assert_eq!(v["hook_event_name"], event, "{v}");
            assert_eq!(v["session_id"], "2026-10-27_12-00-00", "{event}: {v}");
            assert_eq!(v["cwd"], cwd.as_str(), "{event}: {v}");
        }
        let pre = read_json(&out("pre"));
        assert_eq!(
            (&pre["tool_name"], &pre["tool_input"]),
            (&json!("ReadFile"), &action)
        );
        let post = read_json(&out("post"));
        assert_eq!(post["tool_name"], "ReadFile");
        assert_eq!(post["tool_input"], action);
        assert_eq!(post["tool_response"], result);
        assert_eq!(post["duration_ms"], 12);
        let start = read_json(&out("start"));
        assert_eq!(
            (&start["source"], &start["model"]),
            (&json!("resume"), &json!("scripted/model"))
        );
        let stop_v = read_json(&out("stop"));
        assert_eq!(stop_v["stop_hook_active"], false);
        assert_eq!(stop_v["last_assistant_message"], "the plan");
    }

    /// An observer that denies, crashes, hangs or is missing changes nothing
    /// and blocks nothing past its timeout; its failure is only reported.
    #[tokio::test]
    async fn observers_cannot_decide_and_their_failures_do_not_stop_anything() {
        let deny = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"},"decision":"block"}'; exit 0"#;
        let cfg = HooksConfig {
            post_tool: vec![
                sh(deny, 5),
                sh("echo no >&2; exit 2", 5),
                sh("exit 1", 5),
                sh("sleep 30", 1),
                PreToolHook {
                    matcher: String::new(),
                    command: "/nonexistent/x0ne-hook".into(),
                    args: vec![],
                    timeout_secs: Some(5),
                },
            ],
            stop: vec![sh_event("exit 2", 5), sh_event("sleep 30", 1)],
            ..HooksConfig::default()
        };
        let t = std::time::Instant::now();
        let f = post_tool(
            &cfg,
            &here(),
            "WriteFile",
            &json!({}),
            &json!({"success": true}),
            0,
        )
        .await;
        let hooks: Vec<_> = f.iter().map(|f| f.hook.as_str()).collect();
        assert_eq!(
            f.len(),
            4,
            "the deny JSON on exit 0 is not a failure: {f:?}"
        );
        assert!(hooks.contains(&"/nonexistent/x0ne-hook"));
        assert!(f.iter().any(|f| f.error.contains("timed out")), "{f:?}");
        assert_eq!(stop(&cfg, &here(), None).await.len(), 2);
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    /// The matcher filters PostToolUse by tool and SessionStart by source.
    #[tokio::test]
    async fn matchers_filter_the_observers() {
        let d = tempfile::tempdir().unwrap();
        let mark = d.path().join("ran");
        let touch = format!("echo x >> '{}'", mark.display());
        let mut post = sh(&touch, 5);
        post.matcher = "Write*".into();
        let mut start = sh(&touch, 5);
        start.matcher = "resume".into();
        let cfg = HooksConfig {
            post_tool: vec![post],
            session_start: vec![start],
            ..HooksConfig::default()
        };
        post_tool(&cfg, &here(), "ReadFile", &json!({}), &json!({}), 0).await;
        session_start(&cfg, &here(), SessionSource::Startup, "m").await;
        assert!(
            !mark.exists(),
            "a hook ran on an event its matcher excludes"
        );
        post_tool(&cfg, &here(), "WriteFile", &json!({}), &json!({}), 0).await;
        session_start(&cfg, &here(), SessionSource::Resume, "m").await;
        assert_eq!(std::fs::read_to_string(&mark).unwrap().lines().count(), 2);
    }

    #[test]
    fn matcher_covers_connector_tools() {
        let h = |m: &str| PreToolHook {
            matcher: m.into(),
            command: "x".into(),
            args: vec![],
            timeout_secs: None,
        };
        assert!(h("github.*").matches("github.create_issue"));
        assert!(!h("github.*").matches("gitlab.create_issue"));
        assert!(h("*.publish*").matches("slack.publish_message"));
        assert!(h("shell|github.delete_repo").matches("github.delete_repo"));
        assert!(!h("shell").matches("shell_bg"));
        assert!(h("").matches("anything"));
        assert!(h("*").matches("a.b"));
        assert!(!h("a*b*c").matches("ab"));
        assert!(h("a*b*c").matches("aXbYc"));
    }

    #[tokio::test]
    async fn exit_2_denies_with_stderr_as_reason() {
        let out = evaluate(
            &sh_hook("github.*", "echo 'no writes to prod' >&2; exit 2"),
            "github.create_issue",
            &json!({}),
        )
        .await;
        assert_eq!(out.unwrap().reason, "no writes to prod");
    }

    #[tokio::test]
    async fn json_deny_on_exit_0_denies() {
        let cmd = sh_hook(
            "*",
            r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"blocked"}}'"#,
        );
        let out = evaluate(&cmd, "shell", &json!({"type":"shell"})).await;
        assert_eq!(out.unwrap().reason, "blocked");
    }

    #[tokio::test]
    async fn the_hook_reads_the_tool_call_on_stdin() {
        // Denies only when stdin names the connector and carries the argument.
        let cfg = sh_hook(
            "github.*",
            r#"in=$(cat); case "$in" in *'"name":"github"'*'"repo":"prod"'*) echo prod >&2; exit 2;; esac; exit 0"#,
        );
        let action = json!({"type":"github.delete_repo","arguments":{"repo":"prod"}});
        assert!(evaluate(&cfg, "github.delete_repo", &action)
            .await
            .is_some());
        let other = json!({"type":"github.delete_repo","arguments":{"repo":"scratch"}});
        assert!(evaluate(&cfg, "github.delete_repo", &other).await.is_none());
    }

    #[tokio::test]
    async fn allow_and_silence_are_not_decisions() {
        let allow = sh_hook(
            "*",
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#,
        );
        assert!(evaluate(&allow, "shell", &json!({})).await.is_none());
        let quiet = sh_hook("*", "exit 0");
        assert!(evaluate(&quiet, "shell", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn a_hook_that_does_not_match_is_not_run() {
        let cmd = sh_hook("github.*", "exit 2");
        assert!(evaluate(&cmd, "shell", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn failures_deny() {
        let crash = sh_hook("*", "exit 1");
        assert!(evaluate(&crash, "shell", &json!({})).await.is_some());
        let garbage = sh_hook("*", "echo '{not json'");
        assert!(evaluate(&garbage, "shell", &json!({})).await.is_some());
        // A command that is not on disk: the genuine "hook could not start"
        // path, a deny, not the ETXTBSY race (that path is never written).
        let d = tempfile::tempdir().unwrap();
        let absent = d.path().join("absent");
        let missing = cmd_hook("*", &absent.to_string_lossy(), &[]);
        assert!(evaluate(&missing, "shell", &json!({})).await.is_some());
        let mut cfg = sh_hook("*", "sleep 30");
        cfg.pre_tool[0].timeout_secs = Some(1);
        let t = std::time::Instant::now();
        let out = evaluate(&cfg, "shell", &json!({})).await;
        assert!(out.unwrap().reason.contains("timed out"));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn config_parses_and_rejects_unknown_keys() {
        let c: crate::config::Config = toml::from_str(
            "[[hooks.pre_tool]]\nmatcher = \"github.*\"\ncommand = \"/bin/false\"\n",
        )
        .unwrap();
        assert_eq!(c.hooks.pre_tool.len(), 1);
        assert!(toml::from_str::<crate::config::Config>(
            "[[hooks.pre_tool]]\ncommand = \"x\"\nallow = true\n"
        )
        .is_err());
        let c: crate::config::Config = toml::from_str(
            "[[hooks.post_tool]]\nmatcher = \"Write*\"\ncommand = \"/bin/true\"\n\
             [[hooks.session_start]]\ncommand = \"/bin/true\"\n\
             [[hooks.stop]]\ncommand = \"/bin/true\"\nargs = [\"x\"]\n",
        )
        .unwrap();
        assert_eq!(
            (
                c.hooks.post_tool.len(),
                c.hooks.session_start.len(),
                c.hooks.stop.len()
            ),
            (1, 1, 1)
        );
        // Stop has no matcher; a typo is refused, not ignored.
        for bad in [
            "[[hooks.stop]]\nmatcher = \"*\"\ncommand = \"x\"\n",
            "[[hooks.post_tool]]\ncommand = \"x\"\nfeedback = true\n",
            "[[hooks.on_stop]]\ncommand = \"x\"\n",
        ] {
            assert!(
                toml::from_str::<crate::config::Config>(bad).is_err(),
                "{bad}"
            );
        }
    }
}
