//! The default deny list holds the credential stores of x0ne itself and of
//! the agents and tools next to it, under a config that names none of them
//! (audit 2026-11-02, A1: the journal key, the HTTP tokens, the connector
//! secrets, the orchestrator config with its model key, `.claude`, `.codex`,
//! `gh`, `.netrc`, `.git-credentials`, `.kube` were readable by `ReadFile`
//! under a home read root, and bwrap hid them from commands only).

use std::path::{Path, PathBuf};

use x0ne_actions::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::executor::isolation::DEFAULT_HIDDEN_HOME_PATHS;
use x0ne_runtime::policy::{DenyReason, PolicyEffect, PolicyEngine};

/// One file per store, relative to the home, by family.
const FAMILIES: [(&str, &[&str]); 4] = [
    (
        "x0ne",
        &[
            ".x0ne/keys/journal-ed25519",
            ".x0ne/run/runtime-7878.token",
            ".x0ne/config.toml",
            ".x0ne/trusted-projects.toml",
            ".x0ne/snapshots/s1/0.dat",
            ".config/x0ne/connector-secrets.toml",
            ".config/x0ne-orchestrator/config.toml",
        ],
    ),
    ("agents", &[".claude/.credentials.json", ".codex/auth.json"]),
    (
        "forges",
        &[".config/gh/hosts.yml", ".netrc", ".git-credentials"],
    ),
    ("clusters", &[".kube/config", ".aws/credentials"]),
];

/// A home holding every store and a README, read-granted whole, loaded from
/// a file whose `deny_path_patterns` is empty: only the defaults stand.
fn setup() -> (tempfile::TempDir, PathBuf, PolicyEngine) {
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    for (_, files) in FAMILIES {
        for f in files.iter().chain(std::iter::once(&"README.md")) {
            let p = home.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
    }
    let file = home.join("policy.toml");
    std::fs::write(
        &file,
        format!(
            "[runtime]\nmode = \"stdio\"\n\n[policy]\nmode = \"auto\"\n\
             default_effect = \"deny\"\nallowed_commands = []\n\
             allowed_read_paths = [{:?}]\nallowed_write_paths = [{:?}]\n\
             deny_path_patterns = []\n\n[actions]\nWriteFile = true\n",
            home.display().to_string(),
            home.display().to_string()
        ),
    )
    .unwrap();
    let (cfg, _) = AppConfig::find_and_load_with_source(Some(&file)).unwrap();
    let engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
    (dir, home, engine)
}

fn read(engine: &PolicyEngine, p: &Path) -> PolicyEffect {
    engine.evaluate(&Action::ReadFile {
        path: p.display().to_string(),
        offset: 0,
        limit: None,
        start_line: None,
        limit_lines: None,
    })
}

#[test]
fn each_family_of_credential_store_is_denied_to_read_file() {
    let (_dir, home, engine) = setup();
    for (family, files) in FAMILIES {
        for f in files {
            match read(&engine, &home.join(f)) {
                PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. }) => {}
                other => panic!("{family}: ReadFile {f} must meet a deny pattern: {other:?}"),
            }
        }
    }
    assert!(matches!(
        read(&engine, &home.join("README.md")),
        PolicyEffect::Allow
    ));
}

#[test]
fn a_write_into_the_x0ne_config_stores_is_denied() {
    let (_dir, home, engine) = setup();
    for f in [
        ".x0ne/config.toml",
        ".x0ne/trusted-projects.toml",
        ".config/x0ne/x0ne-orchestrator.toml",
        ".config/x0ne-orchestrator/config.toml",
    ] {
        let effect = engine.evaluate(&Action::WriteFile {
            path: home.join(f).display().to_string(),
            content: "[hooks]".into(),
        });
        assert!(
            matches!(
                effect,
                PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. })
            ),
            "WriteFile {f}: {effect:?}"
        );
    }
}

/// Every store bwrap hides from a command is denied to the runtime's own
/// reads too, so the two lists cannot drift apart. The exemptions are named
/// with their reason.
#[test]
fn every_hidden_home_store_is_denied_to_reads() {
    const EXEMPT: &[(&str, &str)] = &[
        (".x0ne", "shares its name with a project's .x0ne/ (rules, skills); its stores are denied one by one"),
        (".docker", "a project's .docker/ holds build files; config.json is denied by name"),
        (".terraform.d", "a plugin cache; credentials.tfrc.json is denied by name"),
        (".npmrc", "a project's .npmrc is configuration the agent edits"),
        (".mozilla", "a browser profile, not a credential file"),
        (".thunderbird", "a mail profile, not a credential file"),
        (".config/google-chrome", "a browser profile, not a credential file"),
        (".config/chromium", "a browser profile, not a credential file"),
        (".config/BraveSoftware", "a browser profile, not a credential file"),
    ];
    for store in DEFAULT_HIDDEN_HOME_PATHS {
        if EXEMPT.iter().any(|(e, _)| e == store) {
            continue;
        }
        let file = format!("/home/u/{store}/x");
        let bare = format!("/home/u/{store}");
        assert!(
            x0ne_actions::deny_paths::matches_default_deny(&bare).is_some()
                || x0ne_actions::deny_paths::matches_default_deny(&file).is_some(),
            "{store} is hidden from commands and readable by ReadFile"
        );
    }
}

/// The read-only MCP profile grants reads under `~`: the browser and mail
/// profiles (cookies, saved logins) and `.npmrc` (registry tokens) stay out
/// of it, as bwrap keeps them out of a command (2026-11-04).
#[test]
fn the_read_only_mcp_profile_denies_browser_profiles_and_npmrc() {
    let profile =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../x0ne-runtime-mcp-readonly.toml");
    let mut cfg = AppConfig::load(&profile).expect("the shipped profile loads");
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    let stores = [
        ".mozilla/firefox/a.default/cookies.sqlite",
        ".thunderbird/a.default/logins.json",
        ".config/google-chrome/Default/Cookies",
        ".config/chromium/Default/Login Data",
        ".config/BraveSoftware/Brave-Browser/Default/Cookies",
        "Library/Application Support/Google/Chrome/Default/Cookies",
        "Library/Application Support/Firefox/Profiles/a.default/key4.db",
        ".npmrc",
    ];
    for f in stores.iter().chain(std::iter::once(&"README.md")) {
        let p = home.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "x").unwrap();
    }
    // The profile reads `~`; the test home stands in for it.
    cfg.policy.allowed_read_paths = vec![home.display().to_string()];
    let engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
    for f in stores {
        match read(&engine, &home.join(f)) {
            PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. }) => {}
            other => panic!("ReadFile ~/{f} under the read-only MCP profile: {other:?}"),
        }
    }
    assert!(matches!(
        read(&engine, &home.join("README.md")),
        PolicyEffect::Allow
    ));
}

/// The other agents' credential files are hidden from a command as well:
/// `cat ~/.claude/.credentials.json` under bwrap reads `/dev/null`.
#[test]
fn the_agents_credential_files_are_hidden_from_commands() {
    let (_dir, home, _engine) = setup();
    let hidden = x0ne_runtime::executor::isolation::hidden_paths(Some(&home), &[]);
    for f in [".claude/.credentials.json", ".codex/auth.json"] {
        assert!(
            hidden.iter().any(|h| h.path == home.join(f) && !h.is_dir),
            "{f} is not hidden from commands: {hidden:?}"
        );
    }
}
