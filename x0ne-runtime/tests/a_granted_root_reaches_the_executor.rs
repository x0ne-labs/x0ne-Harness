//! A directory granted at the prompt is readable by the ACTION, not only by
//! the gate.
//!
//! Field report 2026-08-16, session `2026-08-16_15-17_6b1c`: the journal
//! carries a `policy_grant` of kind `directory (read)` at 15:18:07.653358 and
//! the `policy_denied` for the very same `ReadFile` at 15:18:07.653476 — 0.1 ms
//! later, with "no read paths are configured". Two enforcement points, one
//! consulted the session grants and the other did not.
//!
//! This is an integration test on purpose. `config_with_session_grants` has a
//! unit test beside it, and a unit test would have passed just as happily
//! while `executor::execute` kept handing the on-disk config down — which is
//! exactly the "dead scaffolding" shape of docs/invariants.md. What
//! is under test here is that the widened config is IN THE PATH.

use std::path::PathBuf;
use std::time::Instant;

use x0ne_actions::policy_grant::{GrantKind, PolicyGrant};
use x0ne_actions::Action;
use x0ne_runtime::composite::CompositeStore;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::policy::{PolicyEffect, PolicyEngine};
use x0ne_runtime::result::ActionResult;
use x0ne_runtime::{executor, handles};

fn config_toml(root: &str) -> String {
    format!(
        r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 65536
max_list_entries = 512
readable_env_vars = []
# The shape of the field report: the operator's list resolves to nothing.
allowed_read_paths = []
allowed_write_paths = ["{root}"]
allowed_commands = []
deny_path_patterns = []

[actions]
ReadFile = true
WriteFile = true
"#
    )
}

struct Env {
    config: AppConfig,
    root: PathBuf,
}

fn setup() -> Env {
    let root = std::env::temp_dir().join(format!("x0ne-grant-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let cfg_path = root.join("x0ne-runtime.toml");
    std::fs::write(&cfg_path, config_toml(&root.to_string_lossy())).expect("config");
    let config = AppConfig::load(&cfg_path).expect("config must load");
    Env { config, root }
}

async fn run(action: &Action, env: &Env, policy: &PolicyEngine) -> ActionResult {
    let h = handles::new_shared(100, 300, 20);
    let rate_limiter: executor::desktop::RateLimiter = std::sync::Arc::new(
        tokio::sync::Mutex::new(Instant::now() - std::time::Duration::from_secs(60)),
    );
    let composites = CompositeStore::new(&env.config);
    executor::execute(action, &env.config, h, policy, &rate_limiter, &composites).await
}

/// A failure means a refusal AFTER a yes: the operator granted the directory,
/// the gate agreed, and the action came back `policy_denied` anyway.
#[tokio::test]
async fn a_granted_directory_is_readable_by_the_action_and_not_only_by_the_gate() {
    let env = setup();
    let file = env.root.join("ticket-002.txt");
    std::fs::write(&file, b"ticket: 002\n").expect("fixture");

    let policy = PolicyEngine::new(env.config.clone(), env.config.actions.clone());
    let action = Action::ReadFile {
        path: file.to_string_lossy().to_string(),
        limit: None,
        offset: 0,
        start_line: None,
        limit_lines: None,
    };

    // Before the grant: refused, and that is the premise.
    let before = run(&action, &env, &policy).await;
    assert!(
        !before.success,
        "the premise is wrong: the read was allowed"
    );

    policy
        .apply_grant(&PolicyGrant {
            kind: GrantKind::ReadPath,
            value: env.root.to_string_lossy().to_string(),
            persist: false,
        })
        .expect("the runtime refused the grant");

    // The gate agrees…
    assert!(
        matches!(policy.evaluate(&action), PolicyEffect::Allow),
        "the gate refuses a directory it was told to allow"
    );
    // …and so must the action.
    let after = run(&action, &env, &policy).await;
    assert!(
        after.success,
        "the executor refused a path the operator granted: {:?}",
        after.error
    );

    // The grant is a root, not one file: a sibling in the same directory is
    // readable too, which is what "directory (read)" promises.
    let sibling = env.root.join("ticket-003.txt");
    std::fs::write(&sibling, b"ticket: 003\n").expect("fixture");
    let sibling_read = run(
        &Action::ReadFile {
            path: sibling.to_string_lossy().to_string(),
            limit: None,
            offset: 0,
            start_line: None,
            limit_lines: None,
        },
        &env,
        &policy,
    )
    .await;
    assert!(
        sibling_read.success,
        "a granted directory did not cover its own contents: {:?}",
        sibling_read.error
    );

    let _ = std::fs::remove_dir_all(&env.root);
}
