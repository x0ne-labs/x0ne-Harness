//! `@file` and `/read`: the user names a file of the working folder, and a
//! link there must not hand the model a file outside it. One test in this
//! binary: it changes the process's working directory.

#![cfg(unix)]

use std::os::unix::fs::symlink;
use x0ne_orchestrator::bridge::Bridge;

struct NoRuntime;

#[async_trait::async_trait]
impl Bridge for NoRuntime {
    async fn send_action(&mut self, _: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no runtime in this test")
    }
    async fn health_check(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

/// What `/read <arg>` put in the conversation.
fn slash_read(arg: &str) -> String {
    let mut history = x0ne_orchestrator::agent::history::History::new(100_000);
    let mut bridge: Box<dyn Bridge> = Box::new(NoRuntime);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(x0ne_orchestrator::slash::handle_slash_command(
        &format!("/read {arg}"),
        &mut history,
        &mut bridge,
        &x0ne_orchestrator::api::UsageTracker::new("test"),
        &mut x0ne_orchestrator::config::Config::default(),
        None,
        &mut x0ne_orchestrator::agent::undo::UndoJournal::default(),
    ));
    format!("{:?}", history.messages())
}

/// Red on 2026-10-28: both read `README.md -> ../outside/secret.md` through
/// the link, and `.env` or a file over 1 MiB whole.
#[test]
fn a_reference_is_read_as_trust_reads_a_project_file() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(work.join("docs")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "REF-OUT-1c4d\n").unwrap();
    std::fs::write(outside.join("plain.md"), "REF-NAMED-OUT-9e02\n").unwrap();
    symlink("../outside/secret.md", work.join("README.md")).unwrap();
    symlink(&outside, work.join("vendored")).unwrap();
    std::fs::write(work.join(".env"), "REF-ENV-77aa\n").unwrap();
    std::fs::write(work.join("docs/notes.md"), "REF-INSIDE-3f81\n").unwrap();
    symlink("docs/notes.md", work.join("NOTES.md")).unwrap();
    std::env::set_current_dir(&work).unwrap();

    let mut wrong = Vec::new();
    for typed in ["README.md", "./README.md", "vendored/secret.md", ".env"] {
        let at = x0ne_orchestrator::project::expand_at_references(&format!("see @{typed}"), false);
        let read = slash_read(typed);
        for (how, text) in [("@", &at), ("/read ", &read)] {
            if text.contains("REF-OUT-1c4d") || text.contains("REF-ENV-77aa") {
                wrong.push(format!("{how}{typed} read: {text}"));
            }
        }
    }
    // A link inside is read, and named with its target.
    let at = x0ne_orchestrator::project::expand_at_references("see @NOTES.md", false);
    if !(at.contains("REF-INSIDE-3f81") && at.contains("NOTES.md -> docs/notes.md")) {
        wrong.push(format!("@NOTES.md: {at}"));
    }
    let read = slash_read("NOTES.md");
    if !(read.contains("REF-INSIDE-3f81") && read.contains("NOTES.md -> docs/notes.md")) {
        wrong.push(format!("/read NOTES.md: {read}"));
    }
    // A path the user writes outside the folder is his to read.
    let named = outside.join("plain.md");
    let at = x0ne_orchestrator::project::expand_at_references(
        &format!("see @{}", named.display()),
        false,
    );
    if !at.contains("REF-NAMED-OUT-9e02") {
        wrong.push(format!("@{}: {at}", named.display()));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
