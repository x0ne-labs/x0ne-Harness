//! `x0ne sessions` lists the current project's sessions; `--all` lists
//! every one. A project is the git work tree a session started in, else its
//! directory, as asked in https://github.com/charmbracelet/crush/issues/492.
//! Before 2026-10-27 every session was listed everywhere and the header
//! recorded no directory.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{Fixture, Model};

fn sessions(fx: &Fixture, cwd: &std::path::Path, all: bool) -> String {
    let mut args = vec!["sessions"];
    if all {
        args.push("--all");
    }
    let out = fx.run(cwd, &args, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn sessions_are_listed_by_project_and_all_with_all() {
    let model = Model::start("pong-sessions-1084");
    let fx = Fixture::new(&model);
    let repo = fx.path("work");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let elsewhere = fx.path("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = fx.one_shot(&repo.join("src"), "hello", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let journals: Vec<_> = std::fs::read_dir(fx.path("home/.x0ne/sessions"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(journals.len(), 1, "{journals:?}");
    let id = journals[0]
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // The header places the session in the repository it ran in.
    let text = std::fs::read_to_string(&journals[0]).unwrap();
    let start: serde_json::Value = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["type"] == "session_start")
        .unwrap();
    let repo = std::fs::canonicalize(&repo).unwrap();
    assert_eq!(start["git_root"], repo.to_string_lossy().as_ref());
    assert_eq!(start["cwd"], repo.join("src").to_string_lossy().as_ref());

    // From anywhere in the repository it is listed.
    assert!(sessions(&fx, &repo, false).contains(&id));
    // Elsewhere it is counted, not listed, and --all lists it.
    let other = sessions(&fx, &elsewhere, false);
    assert!(!other.contains(&id), "{other}");
    assert!(
        other.contains("1 more") && other.contains("--all"),
        "{other}"
    );
    assert!(sessions(&fx, &elsewhere, true).contains(&id));

    // /rename in the REPL appends a title; the listing shows it.
    let cfg = fx.path("chosen.toml").display().to_string();
    let repl = fx.run_with_input(
        &repo,
        &["-c", &cfg, "--resume", &id],
        "/rename parser rewrite\n/quit\n",
        Duration::from_secs(60),
    );
    let after = std::fs::read_to_string(&journals[0]).unwrap();
    assert!(
        after.contains("\"session_renamed\""),
        "no rename event\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&repl.stdout),
        String::from_utf8_lossy(&repl.stderr)
    );
    assert!(after.starts_with(&text), "an earlier line was rewritten");
    assert!(sessions(&fx, &repo, false).contains("parser rewrite"));

    // The journal still verifies.
    let path = journals[0].to_string_lossy().into_owned();
    let verify = fx.run(
        &elsewhere,
        &["audit", "verify", &path],
        Duration::from_secs(30),
    );
    assert!(
        verify.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
}

fn journal_ids(fx: &Fixture) -> Vec<String> {
    std::fs::read_dir(fx.path("home/.x0ne/sessions"))
        .unwrap()
        .flatten()
        .map(|e| e.path().file_stem().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// `--resume last` reopens this project's latest session; `--all` reaches
/// every project's; an id from another project is resumed with a warning.
/// Review of 2026-10-27: `last` took the newest journal of any project.
#[test]
fn resume_last_stays_in_the_project() {
    let model = Model::start("pong-resume-1084");
    let fx = Fixture::new(&model);
    let here = fx.path("work");
    std::fs::create_dir_all(here.join(".git")).unwrap();
    let there = fx.path("elsewhere");
    std::fs::create_dir_all(there.join(".git")).unwrap();
    let ok = |out: &std::process::Output| {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{text}");
        text
    };

    ok(&fx.one_shot(&here, "hello", &[]));
    let mine = journal_ids(&fx).pop().unwrap();
    // Ids and mtimes have a one-second grain.
    std::thread::sleep(Duration::from_millis(1100));
    ok(&fx.one_shot(&there, "hello", &[]));
    let theirs = journal_ids(&fx).into_iter().find(|id| *id != mine).unwrap();

    // The newest of all is the other project's: only --all reaches it.
    let all = ok(&fx.one_shot(&here, "again", &["--resume", "last", "--all"]));
    assert!(all.contains(&format!("resumed session {theirs}")), "{all}");
    std::thread::sleep(Duration::from_millis(1100));
    let last = ok(&fx.one_shot(&here, "again", &["--resume", "last"]));
    assert!(last.contains(&format!("resumed session {mine}")), "{last}");
    assert!(!last.contains("another project"), "{last}");

    // Named, it is resumed, and said to belong elsewhere.
    let named = ok(&fx.one_shot(&here, "again", &["--resume", &theirs]));
    assert!(
        named.contains(&format!("resumed session {theirs}")),
        "{named}"
    );
    assert!(named.contains("another project"), "{named}");
}
