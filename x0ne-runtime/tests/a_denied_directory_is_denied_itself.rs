//! `**/.ssh/**` denied every file under `~/.ssh` and not the directory: glob
//! needs the trailing separator. `FileInfo`, `MoveToTrash` and a search
//! rooted there passed the gate (audit 2026-11-02, A2).

use std::path::PathBuf;

use x0ne_actions::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::policy::{DenyReason, PolicyEffect, PolicyEngine};

/// A home with `.ssh`, `.gnupg` and `proj/secrets`, granted for reads and
/// writes, the actions under test switched on, the default deny list.
fn setup() -> (tempfile::TempDir, PathBuf, PolicyEngine) {
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    for d in [".ssh", ".gnupg", "proj/secrets"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
        std::fs::write(home.join(d).join("x"), "x").unwrap();
    }
    let mut cfg = AppConfig::defaults();
    cfg.policy.mode = "auto".into();
    cfg.policy.allowed_read_paths = vec![home.display().to_string()];
    cfg.policy.allowed_write_paths = vec![home.display().to_string()];
    for a in ["FileInfo", "MoveToTrash", "SearchCode", "StatFiles"] {
        cfg.actions.insert(a.into(), true);
    }
    let engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
    (dir, home, engine)
}

fn assert_pattern(effect: PolicyEffect, what: &str) {
    match effect {
        PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. }) => {}
        other => panic!("{what} must meet a deny pattern: {other:?}"),
    }
}

#[test]
fn the_bare_secret_directories_are_denied() {
    let (_dir, home, engine) = setup();
    for d in [".ssh", ".gnupg", "proj/secrets"] {
        let path = home.join(d).display().to_string();
        assert_pattern(
            engine.evaluate(&Action::FileInfo { path: path.clone() }),
            &format!("FileInfo {d}"),
        );
        assert_pattern(
            engine.evaluate(&Action::MoveToTrash { path: path.clone() }),
            &format!("MoveToTrash {d}"),
        );
        assert_pattern(
            engine.evaluate(&Action::SearchCode {
                root: path.clone(),
                pattern: "x".into(),
                patterns: vec![],
                file_glob: None,
                limit: None,
            }),
            &format!("SearchCode {d}"),
        );
        assert_pattern(
            engine.evaluate(&Action::StatFiles {
                paths: vec![path],
                hash: None,
            }),
            &format!("StatFiles {d}"),
        );
    }
    // The home itself and a public directory still pass.
    let ok = engine.evaluate(&Action::FileInfo {
        path: home.join("proj").display().to_string(),
    });
    assert!(matches!(ok, PolicyEffect::Allow), "{ok:?}");
}
