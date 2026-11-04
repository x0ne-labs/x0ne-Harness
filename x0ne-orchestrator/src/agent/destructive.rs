//! Deterministic recognition of shell commands that destroy files in bulk:
//! `rm -r`, a mass `mv`, `git clean`, `git reset --hard`,
//! `find -delete`, `truncate`, `dd`. In a sandbox the dispatcher snapshots the
//! workspace before one of these runs, so `/undo` can bring it back. The
//! same snapshot precedes any command that is not a known read ([`runs_code`]).
//! On the host, a destructive command is snapshotted the same way, before
//! the human gate; when it cannot be, the gate says so ([`guard`]).
//!
//! A text match, not a parser: it errs towards "destructive" (a snapshot too
//! many costs a reflink), and it never runs anything.

use serde_json::Value;

const MAX_DEPTH: usize = 4;

/// Why `action` is destructive (e.g. `"rm -r"`), or `None`.
pub fn classify(action: &Value) -> Option<String> {
    classify_at(action, 0)
}

fn classify_at(action: &Value, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let str_of = |k: &str| action.get(k).and_then(Value::as_str).unwrap_or("");
    match str_of("type") {
        "RunCommand" => {
            let mut line = str_of("command").to_string();
            for a in action
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                line.push(' ');
                line.push_str(a.as_str().unwrap_or(""));
            }
            classify_line(&line)
        }
        "TermSend" => classify_line(str_of("text")),
        "Pipeline" | "Plan" | "Try" | "SafeChange" | "If" => {
            ["steps", "actions", "tests", "then", "else"]
                .iter()
                .flat_map(|k| {
                    action
                        .get(*k)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find_map(|child| classify_at(child, depth + 1))
        }
        _ => None,
    }
}

/// The first destructive simple command in a shell line.
pub fn classify_line(line: &str) -> Option<String> {
    let cleaned: String = line
        .chars()
        .map(|c| match c {
            '"' | '\'' | '`' | '(' | ')' | '{' | '}' => ' ',
            ';' | '|' | '&' | '\n' => ';',
            c => c,
        })
        .collect();
    cleaned.split(';').find_map(|seg| {
        let words: Vec<&str> = seg.split_whitespace().collect();
        classify_words(&words, 0)
    })
}

fn classify_words(words: &[&str], depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut i = 0;
    // Wrappers that run the command after them.
    while i < words.len() {
        let w = words[i].rsplit('/').next().unwrap_or(words[i]);
        match w {
            "sudo" | "env" | "nohup" | "time" | "nice" | "command" | "exec" | "xargs"
            | "timeout" | "doas" => i += 1,
            _ if w.contains('=') && !w.starts_with('-') => i += 1,
            _ if w.starts_with('-') && i > 0 => i += 1,
            "sh" | "bash" | "dash" | "zsh" if words.get(i + 1) == Some(&"-c") => {
                return classify_words(&words[i + 2..], depth + 1);
            }
            _ => break,
        }
    }
    let verb = words.get(i)?.rsplit('/').next()?;
    let rest = &words[i + 1..];
    let flags = || rest.iter().filter(|a| a.starts_with('-'));
    let operands: Vec<&&str> = rest.iter().filter(|a| !a.starts_with('-')).collect();
    let glob = || operands.iter().any(|a| a.contains('*') || a.contains('?'));
    let found = |s: &str| Some(s.to_string());
    match verb {
        "rm" => {
            let recursive = flags().any(|f| {
                *f == "--recursive"
                    || (!f.starts_with("--") && (f.contains('r') || f.contains('R')))
            });
            if recursive {
                found("rm -r")
            } else if glob() || words[..i].iter().any(|w| w.ends_with("xargs")) {
                found("rm")
            } else {
                None
            }
        }
        "mv" if operands.len() > 2 || glob() => found("mv"),
        "git" => {
            let sub = operands.first().map(|s| **s)?;
            match sub {
                "clean" => found("git clean"),
                "reset" if rest.contains(&"--hard") => found("git reset --hard"),
                "checkout" if rest.contains(&"--") || operands.get(1) == Some(&&".") => {
                    found("git checkout")
                }
                "restore" => found("git restore"),
                _ => None,
            }
        }
        "find" => {
            if rest.contains(&"-delete") {
                found("find -delete")
            } else if rest.windows(2).any(|w| {
                matches!(w[0], "-exec" | "-execdir" | "-ok")
                    && w[1].rsplit('/').next() == Some("rm")
            }) {
                found("find -exec rm")
            } else {
                None
            }
        }
        "truncate" => found("truncate"),
        "dd" if rest.iter().any(|a| a.starts_with("of=")) => found("dd"),
        "shred" => found("shred"),
        "rsync" if rest.iter().any(|a| a.starts_with("--delete")) => found("rsync --delete"),
        _ => None,
    }
}

/// The first action in `action` that runs a program and is not a known read
/// (`readonly_cmd`), as a short label (`"make build"`), or `None`.
/// What that program does is invisible here: a Makefile, an npm script or a
/// build.rs may delete files without any of it showing on the command line.
pub fn runs_code(action: &Value) -> Option<String> {
    runs_code_at(action, 0)
}

const LABEL_MAX: usize = 80;

fn label(line: &str) -> String {
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= LABEL_MAX {
        return line;
    }
    let cut: String = line.chars().take(LABEL_MAX).collect();
    format!("{cut}…")
}

fn runs_code_at(action: &Value, depth: usize) -> Option<String> {
    let str_of = |k: &str| action.get(k).and_then(Value::as_str).unwrap_or("");
    let kind = str_of("type");
    if depth > MAX_DEPTH {
        return Some(kind.to_string());
    }
    let strings = |k: &str| -> Vec<String> {
        action
            .get(k)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| a.as_str().unwrap_or("").to_string())
            .collect()
    };
    match kind {
        "RunCommand" | "TermOpen" | "Listen" => {
            let (command, args) = (str_of("command"), strings("args"));
            let env: std::collections::HashMap<String, String> = action
                .get("env")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
                .unwrap_or_default();
            let cwd = action.get("cwd").and_then(Value::as_str);
            if crate::agent::readonly_cmd::is_read_only_command(command, &args, &env, cwd) {
                None
            } else {
                Some(label(&format!("{command} {}", args.join(" "))))
            }
        }
        "TermSend" => match action.get("text").and_then(Value::as_str) {
            Some(text) if !typed_line_is_read_only(text) => Some(label(text)),
            Some(_) => None,
            None => strings("keys")
                .iter()
                .any(|k| matches!(k.as_str(), "Enter" | "Return" | "C-m" | "C-j" | "KPEnter"))
                .then(|| "TermSend Enter".to_string()),
        },
        "SendInput" | "SafeTest" => Some(kind.to_string()),
        "Pipeline" | "Plan" | "Try" | "SafeChange" | "If" => {
            ["steps", "actions", "tests", "then", "else"]
                .iter()
                .flat_map(|k| {
                    action
                        .get(*k)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find_map(|child| runs_code_at(child, depth + 1))
        }
        _ => None,
    }
}

/// A line typed into a shell: read-only only without any shell grammar and
/// with an argv `readonly_cmd` dispenses.
fn typed_line_is_read_only(text: &str) -> bool {
    let line = text.trim_end_matches(['\n', '\r']);
    if line.chars().any(|c| "\n\r;|&<>$`(){}*?~!'\"\\".contains(c)) {
        return false;
    }
    let mut words = line.split_whitespace();
    let Some(cmd) = words.next() else {
        return true;
    };
    let args: Vec<String> = words.map(str::to_string).collect();
    crate::agent::readonly_cmd::is_read_only_command(cmd, &args, &Default::default(), None)
}

/// The short label of an action that runs code or destroys files, as
/// [`guard`] shows it, or `None` for anything else.
pub fn label_of(action: &Value) -> Option<String> {
    runs_code(action).or_else(|| classify(action))
}

/// The dispatcher's step before an action that runs code. A destructive
/// command or any project code snapshots `workspace` first in a
/// sandbox. `Ok(Some(line))` is for the operator; `Err(reason)` means the
/// command must not run (a destructive one whose snapshot failed).
///
/// On the host a destructive command is snapshotted too, and the
/// dispatcher calls this BEFORE the human gate: `Err(reason)` then means there
/// is no undo point, and the gate must say so ([`crate::agent::confirm::decide_without_undo`]).
/// Other code on the host is not snapshotted and is recorded as run without
/// an undo point.
pub async fn guard(
    action: &Value,
    in_sandbox: bool,
    workspace: &std::path::Path,
    undo: &mut crate::agent::undo::UndoJournal,
    bridge: &mut dyn crate::bridge::Bridge,
) -> Result<Option<String>, String> {
    let destructive = classify(action);
    let Some(shown) = runs_code(action).or_else(|| destructive.clone()) else {
        return Ok(None);
    };
    if !in_sandbox {
        let Some(why) = destructive else {
            undo.note_unprotected(&shown);
            return Ok(None);
        };
        return host_checkpoint(action, &shown, &why, workspace, undo, bridge).await;
    }
    let why = destructive.clone().unwrap_or_else(|| shown.clone());
    match undo
        .checkpoint_workspace(action, &why, workspace, bridge)
        .await
    {
        Ok(Some(mode)) => Ok(Some(format!(
            "`{shown}`: {} snapshotted first ({mode}); /undo restores it",
            workspace.display()
        ))),
        Ok(None) => Ok(None),
        Err(e) => match destructive {
            Some(why) => Err(format!(
                "`{why}` was not run: it may delete files in {}, and the snapshot that \
                 /undo would need could not be taken ({e}). Narrow the command to the \
                 files concerned, or ask the user to run it.",
                workspace.display()
            )),
            None => {
                undo.note_unprotected(&shown);
                Ok(Some(format!("`{shown}` runs without an undo point: {e}")))
            }
        },
    }
}

/// The workspace snapshot before a destructive command on the host.
/// `Err` is the reason there is no undo point, worded for the gate: a target
/// outside `workspace`, a secret the runtime will not copy, a refused or
/// oversized snapshot (`[snapshots] max_snapshot_size_mb`, 100 MB by default).
async fn host_checkpoint(
    action: &Value,
    shown: &str,
    why: &str,
    workspace: &std::path::Path,
    undo: &mut crate::agent::undo::UndoJournal,
    bridge: &mut dyn crate::bridge::Bridge,
) -> Result<Option<String>, String> {
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    if let Some(reason) = outside_workspace(action, &canonical, 0) {
        return Err(format!("`{why}` {reason}"));
    }
    let targets = crate::agent::undo::workspace_targets(workspace)
        .map_err(|e| format!("`{why}`: cannot list {}: {e}", workspace.display()))?;
    // The runtime refuses the whole snapshot over one of these, and a policy
    // refusal stops every later undo point of the session (`UndoJournal`).
    if let Some(secret) = targets
        .iter()
        .find(|t| x0ne_actions::deny_paths::matches_default_deny(t).is_some())
    {
        return Err(format!(
            "`{why}`: {secret} is under deny_path_patterns, so no snapshot of {} can hold it",
            workspace.display()
        ));
    }
    match undo
        .checkpoint_workspace(action, why, workspace, bridge)
        .await
    {
        Ok(Some(mode)) => Ok(Some(format!(
            "`{shown}`: {} snapshotted first ({mode}); /undo restores it",
            workspace.display()
        ))),
        Ok(None) => Ok(None),
        Err(e) => Err(format!(
            "`{why}`: the snapshot of {} that /undo would need could not be taken ({e})",
            workspace.display()
        )),
    }
}

/// Why the targets of a destructive `action` are not provably inside
/// `workspace`, or `None`. Fails closed: a variable, a command substitution,
/// a `cd`, `xargs` or a line typed into a terminal is not followed.
fn outside_workspace(action: &Value, workspace: &std::path::Path, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return Some("is nested too deep to follow".into());
    }
    let str_of = |k: &str| action.get(k).and_then(Value::as_str).unwrap_or("");
    match str_of("type") {
        "RunCommand" if classify(action).is_some() => {
            let mut line = str_of("command").to_string();
            for a in action
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                line.push(' ');
                line.push_str(a.as_str().unwrap_or(""));
            }
            // `git reset --hard` rewrites the whole work tree, wherever it starts.
            if classify(action).as_deref() == Some("git reset --hard")
                && !workspace.join(".git").exists()
            {
                return Some(format!(
                    "resets a git work tree whose top is not {}",
                    workspace.display()
                ));
            }
            let cwd = action.get("cwd").and_then(Value::as_str);
            line_outside(&line, cwd, workspace)
        }
        "TermSend" if classify(action).is_some() => {
            Some("is typed into a terminal whose directory is not known".into())
        }
        "Pipeline" | "Plan" | "Try" | "SafeChange" | "If" => {
            ["steps", "actions", "tests", "then", "else"]
                .iter()
                .flat_map(|k| {
                    action
                        .get(*k)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .find_map(|child| outside_workspace(child, workspace, depth + 1))
        }
        _ => None,
    }
}

/// Every word after each program name must resolve under `workspace`
/// (`readonly_cmd`'s rule); `key=value` is judged by its value and a
/// redirection by its target. `/dev/null` is the one target allowed outside.
fn line_outside(line: &str, cwd: Option<&str>, workspace: &std::path::Path) -> Option<String> {
    if line.contains('$') || line.contains('`') {
        return Some(
            "expands a variable or a command, so its targets are known only once it runs".into(),
        );
    }
    let cleaned: String = line
        .chars()
        .map(|c| match c {
            '"' | '\'' | '(' | ')' | '{' | '}' | '<' | '>' => ' ',
            ';' | '|' | '&' | '\n' => ';',
            c => c,
        })
        .collect();
    for seg in cleaned.split(';') {
        let words: Vec<&str> = seg.split_whitespace().collect();
        if words.iter().any(|w| {
            matches!(
                w.rsplit('/').next().unwrap_or(w),
                "cd" | "pushd" | "popd" | "xargs"
            )
        }) {
            return Some(
                "changes directory or reads its targets from its input, so they are known only once it runs"
                    .into(),
            );
        }
        let operands: Vec<String> = words
            .iter()
            .skip(1)
            .map(|w| match w.split_once('=') {
                Some((_, v)) if !w.starts_with('-') => v.to_string(),
                _ => (*w).to_string(),
            })
            .filter(|w| w != "/dev/null" && !w.is_empty())
            .collect();
        if crate::agent::readonly_cmd::resolved_operands(&operands, cwd, workspace).is_none() {
            return Some(format!(
                "names a path outside {}, which no snapshot of it covers",
                workspace.display()
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn in_a_sandbox_a_destructive_command_runs_only_after_a_snapshot() {
        use crate::agent::undo::{tests::SnapshotBridge, UndoJournal};
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("x")).unwrap();
        std::fs::write(work.join("x/a.txt"), "alpha").unwrap();
        let mut bridge = SnapshotBridge::new(&work, &tmp.path().join("snaps"));
        let mut undo = UndoJournal::default();
        let rm = run("rm", &["-rf", "x"]);

        let ls = guard(&run("ls", &[]), true, &work, &mut undo, &mut bridge).await;
        assert_eq!(ls, Ok(None));
        assert!(undo.points().is_empty());

        let line = guard(&rm, true, &work, &mut undo, &mut bridge)
            .await
            .unwrap()
            .unwrap();
        assert!(line.contains("rm -r") && line.contains("/undo"), "{line}");
        assert_eq!(undo.points().len(), 1);

        bridge.config.snapshots.max_snapshot_size_mb = 0;
        let err = guard(&rm, true, &work, &mut undo, &mut bridge)
            .await
            .unwrap_err();
        assert!(err.contains("not run") && err.contains("rm -r"), "{err}");
    }

    fn sha_tree(root: &std::path::Path) -> Vec<(String, String)> {
        use sha2::{Digest, Sha256};
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let h = Sha256::digest(std::fs::read(&p).unwrap());
                    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
                    out.push((p.display().to_string(), hex));
                }
            }
        }
        out.sort();
        out
    }

    /// A project under `work/proj` and a sandboxed journal on the runtime's
    /// own snapshot code.
    fn project() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        crate::agent::undo::tests::SnapshotBridge,
    ) {
        use crate::agent::undo::tests::SnapshotBridge;
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("proj/src")).unwrap();
        std::fs::write(work.join("proj/src/main.c"), "int main(){return 0;}\n").unwrap();
        std::fs::write(work.join("proj/data.bin"), [0u8, 1, 2, 255, 254]).unwrap();
        let bridge = SnapshotBridge::new(&work, &tmp.path().join("snaps"));
        (tmp, work, bridge)
    }

    fn run_in(command: &str, args: &[&str], cwd: &std::path::Path) -> Value {
        json!({"type": "RunCommand", "command": command, "args": args,
               "cwd": cwd.display().to_string()})
    }

    /// Measured 2026-10-06: `make build` ran a Makefile with
    /// `rm -rf /work/proj`, no snapshot was taken, `/undo` had nothing.
    #[tokio::test]
    async fn a_makefile_that_deletes_the_project_is_undone_byte_for_byte() {
        let (_tmp, work, mut bridge) = project();
        std::fs::create_dir_all(work.join("mk")).unwrap();
        std::fs::write(work.join("mk/Makefile"), "build:\n\trm -rf ../proj\n").unwrap();
        let before = sha_tree(&work.join("proj"));
        let make = run_in("make", &["build"], &work.join("mk"));
        let mut undo = crate::agent::undo::UndoJournal::default();

        let line = guard(&make, true, &work, &mut undo, &mut bridge)
            .await
            .expect("make may run")
            .expect("and is announced");
        assert!(line.contains("make build"), "{line}");
        assert!(line.contains("reflink") || line.contains("copy"), "{line}");
        let st = std::process::Command::new("make")
            .arg("build")
            .current_dir(work.join("mk"))
            .status()
            .unwrap();
        assert!(st.success() && !work.join("proj").exists());

        let outcome = undo.undo_last(&mut bridge).await;
        assert!(
            matches!(outcome, crate::agent::undo::UndoOutcome::Restored { .. }),
            "{outcome:?}"
        );
        assert_eq!(sha_tree(&work.join("proj")), before);
    }

    #[tokio::test]
    async fn a_script_or_an_npm_run_is_undone_byte_for_byte() {
        let (_tmp, work, mut bridge) = project();
        std::fs::write(work.join("clean.sh"), "rm -rf proj\n").unwrap();
        let before = sha_tree(&work.join("proj"));
        let mut undo = crate::agent::undo::UndoJournal::default();

        let sh = run_in("sh", &["clean.sh"], &work);
        guard(&sh, true, &work, &mut undo, &mut bridge)
            .await
            .unwrap()
            .unwrap();
        let st = std::process::Command::new("sh")
            .arg("clean.sh")
            .current_dir(&work)
            .status()
            .unwrap();
        assert!(st.success() && !work.join("proj").exists());
        undo.undo_last(&mut bridge).await;
        assert_eq!(sha_tree(&work.join("proj")), before);

        // npm is not in the build image: the script's effect is replayed.
        let npm = run_in("npm", &["run", "build"], &work);
        guard(&npm, true, &work, &mut undo, &mut bridge)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(work.join("proj")).unwrap();
        undo.undo_last(&mut bridge).await;
        assert_eq!(sha_tree(&work.join("proj")), before);

        let term = json!({"type": "TermSend", "name": "t", "text": "cargo test\n"});
        guard(&term, true, &work, &mut undo, &mut bridge)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            undo.points().len(),
            1,
            "a line typed in a shell is code too"
        );
    }

    #[tokio::test]
    async fn a_known_read_takes_no_snapshot() {
        let (_tmp, work, mut bridge) = project();
        let root = crate::agent::readonly_cmd::workspace_root();
        let mut undo = crate::agent::undo::UndoJournal::default();
        for action in [
            run_in("ls", &["-la"], &root),
            run_in("cat", &["Cargo.toml"], &root),
            run_in("git", &["status"], &root),
            json!({"type": "TermSend", "name": "t", "text": "ls -la\n"}),
            json!({"type": "ReadFile", "path": "a"}),
        ] {
            let got = guard(&action, true, &work, &mut undo, &mut bridge).await;
            assert_eq!(got, Ok(None), "{action}");
        }
        assert!(undo.points().is_empty());
        assert!(!bridge.types().iter().any(|t| t == "CreateSnapshot"));
        assert!(describe_nothing(&mut undo, &mut bridge)
            .await
            .contains("nothing to undo"));
    }

    async fn describe_nothing(
        undo: &mut crate::agent::undo::UndoJournal,
        bridge: &mut crate::agent::undo::tests::SnapshotBridge,
    ) -> String {
        let o = undo.undo_last(bridge).await;
        crate::agent::undo::describe(&o)
    }

    /// "nothing to undo: no file was written by the agent" was false after a
    /// `make build`: the message names what ran without an undo point.
    #[tokio::test]
    async fn undo_names_the_commands_that_ran_without_an_undo_point() {
        let (_tmp, work, mut bridge) = project();
        let mut undo = crate::agent::undo::UndoJournal::default();
        let make = run_in("make", &["build"], &work);
        // On the host there is no workspace checkpoint (the human gate is).
        assert_eq!(
            guard(&make, false, &work, &mut undo, &mut bridge).await,
            Ok(None)
        );
        let line = describe_nothing(&mut undo, &mut bridge).await;
        assert!(
            line.contains("no undo point; commands that ran without one: make build"),
            "{line}"
        );

        // In a sandbox, a checkpoint that fails does not stop a build, and says so.
        let mut undo = crate::agent::undo::UndoJournal::default();
        bridge.config.snapshots.max_snapshot_size_mb = 0;
        let cargo = run_in("cargo", &["test"], &work);
        let warn = guard(&cargo, true, &work, &mut undo, &mut bridge)
            .await
            .expect("not refused")
            .expect("announced");
        assert!(warn.contains("without an undo point"), "{warn}");
        let line = describe_nothing(&mut undo, &mut bridge).await;
        assert!(line.contains("cargo test"), "{line}");
    }

    fn run(command: &str, args: &[&str]) -> Value {
        json!({"type": "RunCommand", "command": command, "args": args})
    }

    /// Measured 2026-10-16 (3 runs): on the host an approved `rm -rf`
    /// ran with no snapshot, and `/undo` answered "no undo point".
    #[tokio::test]
    async fn on_the_host_an_approved_rm_rf_is_undone_byte_for_byte() {
        let (_tmp, work, mut bridge) = project();
        let before = sha_tree(&work.join("proj"));
        let rm = run_in("rm", &["-rf", "proj"], &work);
        let mut undo = crate::agent::undo::UndoJournal::default();

        let line = guard(&rm, false, &work, &mut undo, &mut bridge)
            .await
            .expect("an undo point on the host")
            .expect("and it is announced");
        assert!(
            line.contains("rm -rf proj") && line.contains("/undo"),
            "{line}"
        );
        let st = std::process::Command::new("rm")
            .args(["-rf", "proj"])
            .current_dir(&work)
            .status()
            .unwrap();
        assert!(st.success() && !work.join("proj").exists());

        let outcome = undo.undo_last(&mut bridge).await;
        assert!(
            matches!(outcome, crate::agent::undo::UndoOutcome::Restored { .. }),
            "{}",
            crate::agent::undo::describe(&outcome)
        );
        assert_eq!(sha_tree(&work.join("proj")), before);
    }

    /// Targets the workspace snapshot holds: each gets an undo point.
    #[tokio::test]
    async fn on_the_host_a_target_inside_the_workspace_gets_an_undo_point() {
        let (_tmp, work, mut bridge) = project();
        let mut undo = crate::agent::undo::UndoJournal::default();
        for (i, action) in [
            run_in("rm", &["-rf", "proj/src"], &work),
            run_in("rm", &["-rf", "."], &work),
            run_in("find", &[".", "-name", "*.o", "-delete"], &work),
            run_in("sh", &["-c", "rm -rf proj 2>/dev/null"], &work),
            run_in(
                "rm",
                &["-rf", &work.join("proj").display().to_string()],
                &work,
            ),
        ]
        .iter()
        .enumerate()
        {
            let got = guard(action, false, &work, &mut undo, &mut bridge).await;
            assert!(matches!(got, Ok(Some(_))), "{action}: {got:?}");
            assert_eq!(undo.points().len(), i + 1, "{action}");
        }
    }

    /// No undo point is possible, so `guard` says why instead of letting
    /// the gate ask as if there were one. Nothing is snapshotted.
    #[tokio::test]
    async fn on_the_host_a_target_outside_the_workspace_has_no_undo_point() {
        let (tmp, work, mut bridge) = project();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let far = elsewhere.display().to_string();
        let mut undo = crate::agent::undo::UndoJournal::default();
        for (action, why) in [
            (run_in("rm", &["-rf", &far], &work), "outside"),
            (run_in("rm", &["-rf", "../elsewhere"], &work), "outside"),
            (run_in("rm", &["-rf", "proj"], &elsewhere), "outside"),
            (run_in("rm", &["-rf", "/*"], &work), "outside"),
            (
                run("sh", &["-c", "cd .. && rm -rf elsewhere"]),
                "changes directory",
            ),
            (run("rm", &["-rf", "$HOME/x"]), "variable"),
            (run_in("git", &["reset", "--hard"], &work), "git work tree"),
            (
                json!({"type": "TermSend", "name": "t", "text": "rm -rf proj\n"}),
                "terminal",
            ),
        ] {
            let err = guard(&action, false, &work, &mut undo, &mut bridge)
                .await
                .expect_err("no undo point is possible");
            assert!(err.contains(why), "{action}: {err}");
        }
        assert!(undo.points().is_empty());
        assert!(!bridge.types().iter().any(|t| t == "CreateSnapshot"));
        assert!(elsewhere.exists());
    }

    /// Too big for `[snapshots] max_snapshot_size_mb`: no undo point, said.
    #[tokio::test]
    async fn on_the_host_a_workspace_too_big_to_snapshot_has_no_undo_point() {
        let (_tmp, work, mut bridge) = project();
        bridge.config.snapshots.max_snapshot_size_mb = 0;
        let mut undo = crate::agent::undo::UndoJournal::default();
        let err = guard(
            &run_in("rm", &["-rf", "proj"], &work),
            false,
            &work,
            &mut undo,
            &mut bridge,
        )
        .await
        .expect_err("no undo point");
        assert!(err.contains("size limit"), "{err}");
        assert!(undo.points().is_empty());
    }

    /// A `.env` at the top would make the runtime refuse the snapshot by
    /// policy, which stops every later undo point: it is not even sent.
    #[tokio::test]
    async fn on_the_host_a_secret_at_the_top_is_named_and_not_sent() {
        let (_tmp, work, mut bridge) = project();
        std::fs::write(work.join(".env"), "TOKEN=x").unwrap();
        let mut undo = crate::agent::undo::UndoJournal::default();
        let err = guard(
            &run_in("rm", &["-rf", "proj"], &work),
            false,
            &work,
            &mut undo,
            &mut bridge,
        )
        .await
        .expect_err("no undo point");
        assert!(
            err.contains(".env") && err.contains("deny_path_patterns"),
            "{err}"
        );
        assert!(!bridge.types().iter().any(|t| t == "CreateSnapshot"));
        assert!(!undo.unavailable());
    }

    #[test]
    fn bulk_deletions_and_rewrites_are_recognised() {
        let cases: &[(Value, &str)] = &[
            (run("rm", &["-rf", "/work/x"]), "rm -r"),
            (run("rm", &["-r", "x"]), "rm -r"),
            (run("rm", &["-fR", "x"]), "rm -r"),
            (run("rm", &["--recursive", "x"]), "rm -r"),
            (run("rm", &["*.rs"]), "rm"),
            (run("rm -rf /work/x", &[]), "rm -r"),
            (run("sh", &["-c", "cd /work && rm -rf x"]), "rm -r"),
            (run("bash -c \"sudo rm -rf /work\"", &[]), "rm -r"),
            (run("mv", &["a", "b", "c", "dir/"]), "mv"),
            (run("mv", &["src/*", "/tmp/"]), "mv"),
            (run("git", &["clean", "-fdx"]), "git clean"),
            (
                run("git", &["reset", "--hard", "HEAD~3"]),
                "git reset --hard",
            ),
            (run("git", &["checkout", "--", "."]), "git checkout"),
            (run("git", &["restore", "."]), "git restore"),
            (
                run("find", &[".", "-name", "*.o", "-delete"]),
                "find -delete",
            ),
            (
                run("find", &[".", "-exec", "rm", "{}", ";"]),
                "find -exec rm",
            ),
            (run("truncate", &["-s", "0", "a"]), "truncate"),
            (run("dd", &["if=/dev/zero", "of=/work/a"]), "dd"),
            (run("ls | xargs rm -f", &[]), "rm"),
            (
                run("rsync", &["-a", "--delete", "a/", "b/"]),
                "rsync --delete",
            ),
            (run("shred", &["-u", "a"]), "shred"),
            (
                json!({"type": "Pipeline", "steps": [run("rm", &["-rf", "x"])]}),
                "rm -r",
            ),
            (
                json!({"type": "TermSend", "name": "t", "text": "rm -rf /work\n"}),
                "rm -r",
            ),
        ];
        for (action, want) in cases {
            assert_eq!(classify(action).as_deref(), Some(*want), "{action}");
        }
    }

    #[test]
    fn ordinary_commands_are_left_alone() {
        for action in [
            run("ls", &["-la"]),
            run("rm", &["a.txt"]),
            run("rm", &["-f", "a.txt"]),
            run("mv", &["a", "b"]),
            run("git", &["status"]),
            run("git", &["reset", "HEAD", "a"]),
            run("git", &["checkout", "-b", "feature"]),
            run("cargo", &["build", "--release"]),
            run("find", &[".", "-name", "*.rs"]),
            run("echo", &["rm -rf is dangerous"]),
            run("grep", &["-r", "rm", "."]),
            json!({"type": "WriteFile", "path": "a", "content": "rm -rf /"}),
            json!({"type": "ReadFile", "path": "a"}),
            json!({"type": "TermSend", "name": "t", "text": "cargo test\n"}),
        ] {
            assert_eq!(classify(&action), None, "{action}");
        }
    }
}
