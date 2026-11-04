//! axum 0.8 changed path parameters from `/:id` and `/*rest` to `/{id}` and
//! `/{*rest}` — and a route written the old way does not fail to COMPILE, it
//! panics when the router is built, i.e. when the server starts. The compiler
//! and clippy are both silent about it, so this test is the only leg of the
//! loop that can see it (#102m, 2026-09-18).
//!
//! The reverse mistake is just as quiet and had already happened: before the
//! upgrade `x0ne-runtime/src/server.rs` declared `/handles/{id}` under axum
//! 0.7, where braces are literal characters, so the route could only ever
//! match the path `/handles/{id}` itself. Measured after the upgrade, on a
//! running runtime: `GET /handles/abc` reaches `handle_check_handle`.

use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The literal path of every `.route("…"` call in `source`.
fn route_paths(source: &str) -> Vec<&str> {
    source
        .split(".route(\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect()
}

fn uses_the_pre_0_8_syntax(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment.starts_with(':') || segment.starts_with('*'))
}

#[test]
fn the_detector_sees_the_old_syntax_and_accepts_the_new() {
    assert!(uses_the_pre_0_8_syntax("/a/:id"));
    assert!(uses_the_pre_0_8_syntax("/a/{id}/*rest"));
    assert!(!uses_the_pre_0_8_syntax("/a/{id}/{*rest}"));
    assert!(!uses_the_pre_0_8_syntax("/v1/chat/completions"));
    assert_eq!(
        route_paths(r#"Router::new().route("/a/:id", any(f)).route("/b", get(g))"#),
        ["/a/:id", "/b"]
    );
}

/// A failure means a server of this workspace panics at start-up, on a line
/// that compiles and lints clean.
#[test]
fn no_route_in_the_workspace_uses_the_syntax_that_panics_at_startup() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate lives in the workspace");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(workspace)
        .expect("workspace readable")
        .flatten()
    {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("x0ne-") {
            rust_sources(&entry.path().join("src"), &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "scanned {} files — wrong root?",
        files.len()
    );

    let mut routes = 0;
    let mut offenders = Vec::new();
    for file in &files {
        let Ok(source) = std::fs::read_to_string(file) else {
            continue;
        };
        for path in route_paths(&source) {
            routes += 1;
            if uses_the_pre_0_8_syntax(path) {
                offenders.push(format!("{}: {path}", file.display()));
            }
        }
    }
    // A scan that finds no route at all proves nothing.
    assert!(
        routes >= 10,
        "only {routes} routes found — the scan is broken"
    );
    assert!(offenders.is_empty(), "{offenders:#?}");
}
