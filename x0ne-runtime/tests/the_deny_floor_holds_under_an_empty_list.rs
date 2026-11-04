//! (H-04, audit 2026-10-15): `deny_path_patterns = []` emptied the one
//! list a prompt cannot lift. The defaults now hold at load; one is lifted
//! only by naming it in `deny_path_exceptions`.

use std::path::{Path, PathBuf};

use x0ne_actions::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::policy::{DenyReason, PolicyEffect, PolicyEngine};

const ENV: &str = ".env";
const OTHER_SECRETS: [&str; 3] = ["config/server.pem", "secrets/a", "home/.ssh/id_ed25519"];

/// A canonical root holding a README and the four secrets of the audit.
struct Root {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn root() -> Root {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    for f in ["README.md", ENV].iter().chain(OTHER_SECRETS.iter()) {
        let p = path.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x").unwrap();
    }
    Root { _dir: dir, path }
}

/// Reads granted under the root, `deny` as the deny lines, loaded the way
/// `x0ne-runtime --config <file>` loads it.
fn load(r: &Root, deny: &str) -> anyhow::Result<AppConfig> {
    let toml = format!(
        "[runtime]\nmode = \"stdio\"\n\n[policy]\nmode = \"auto\"\n\
         default_effect = \"deny\"\nmax_read_bytes = 1024\nmax_list_entries = 10\n\
         allowed_commands = []\nallowed_read_paths = [{:?}]\nallowed_write_paths = []\n\
         {deny}\n",
        r.path.display().to_string()
    );
    let file = r.path.join("x0ne-runtime.toml");
    std::fs::write(&file, toml).unwrap();
    AppConfig::find_and_load_with_source(Some(&file)).map(|(cfg, _)| cfg)
}

fn read(engine: &PolicyEngine, file: &Path) -> PolicyEffect {
    engine.evaluate(&Action::ReadFile {
        path: file.display().to_string(),
        offset: 0,
        limit: None,
        start_line: None,
        limit_lines: None,
    })
}

fn engine(cfg: &AppConfig) -> PolicyEngine {
    PolicyEngine::new(cfg.clone(), cfg.actions.clone())
}

fn assert_passes(engine: &PolicyEngine, file: &Path, label: &str) {
    match read(engine, file) {
        PolicyEffect::Allow => {}
        other => panic!("{label}: ReadFile {} must pass: {other:?}", file.display()),
    }
}

/// The refusal, which must come from a deny pattern and nothing else.
fn assert_denied(engine: &PolicyEngine, file: &Path, label: &str) -> DenyReason {
    match read(engine, file) {
        PolicyEffect::Deny(reason @ DenyReason::PathMatchesDenyPattern { .. }) => reason,
        other => panic!(
            "{label}: ReadFile {} must meet a deny pattern: {other:?}",
            file.display()
        ),
    }
}

#[test]
fn an_empty_list_still_refuses_every_default() {
    let r = root();
    let cfg = load(&r, "deny_path_patterns = []").expect("loads");
    let e = engine(&cfg);
    assert_passes(&e, &r.path.join("README.md"), "empty list");
    let reason = assert_denied(&e, &r.path.join(ENV), "empty list");
    assert!(
        reason.message().contains("deny_path_exceptions"),
        "the refusal must name the lever that lifts a default: {}",
        reason.message()
    );
    for f in OTHER_SECRETS {
        assert_denied(&e, &r.path.join(f), "empty list");
    }
}

#[test]
fn an_exception_lifts_the_default_it_names_and_no_other() {
    let r = root();
    for (label, deny) in [
        ("empty list", "deny_path_patterns = []"),
        // The wizard writes the defaults out: the exception still wins.
        ("listed", r#"deny_path_patterns = ["**/.env*", "**/*.pem"]"#),
    ] {
        let lines = format!("{deny}\ndeny_path_exceptions = [\"**/.env*\"]");
        let cfg = load(&r, &lines).expect("loads");
        let e = engine(&cfg);
        assert_passes(&e, &r.path.join(ENV), label);
        for f in OTHER_SECRETS {
            assert_denied(&e, &r.path.join(f), label);
        }
        assert!(
            cfg.policy_warnings()
                .iter()
                .any(|w| w.contains("deny_path_exceptions") && w.contains("**/.env*")),
            "{label}: a lifted default is announced at startup: {:?}",
            cfg.policy_warnings()
        );
    }
}

#[test]
fn an_exception_that_names_no_default_is_refused_at_load() {
    let r = root();
    let err = load(
        &r,
        "deny_path_patterns = []\ndeny_path_exceptions = [\"**/.env\"]",
    )
    .expect_err("`**/.env` is not a default: the file claims a lift it does not get");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("deny_path_exceptions") && msg.contains("**/.env*"),
        "the refusal must name the field and the defaults: {msg}"
    );
}

/// A pattern glob cannot compile was dropped at match time: the file said
/// `secrets` were denied and nothing was (2026-11-04).
#[test]
fn a_deny_pattern_that_does_not_compile_is_refused_at_load() {
    let r = root();
    let err = load(&r, r#"deny_path_patterns = ["**/[secrets"]"#)
        .expect_err("a pattern that denies nothing must not load");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("deny_path_patterns") && msg.contains("**/[secrets"),
        "the refusal must name the field and the pattern: {msg}"
    );
}

/// The shipped references keep their intent under the floor: the host one
/// lifts nothing, the sandbox one (the container is the boundary) lifts all.
#[test]
fn the_shipped_references_state_their_exceptions() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let host = AppConfig::load(&dir.join("x0ne-runtime.toml")).expect("reference loads");
    assert!(host.policy.deny_path_exceptions.is_empty());
    let sandbox = AppConfig::load(&dir.join("x0ne-runtime-sandbox.toml")).expect("sandbox loads");
    assert_eq!(
        sandbox.policy.deny_path_exceptions,
        x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS
    );
    assert!(sandbox.policy.deny_path_patterns.is_empty());
}

#[test]
fn a_pattern_of_the_file_still_applies_beside_the_defaults() {
    let r = root();
    let cfg = load(&r, r#"deny_path_patterns = ["**/README.md"]"#).expect("loads");
    let e = engine(&cfg);
    assert_denied(&e, &r.path.join("README.md"), "own pattern");
    assert_denied(&e, &r.path.join(ENV), "own pattern");
    assert_eq!(
        cfg.policy.deny_path_patterns.first().map(String::as_str),
        Some("**/README.md"),
        "the file's own patterns come first, as written"
    );
}
