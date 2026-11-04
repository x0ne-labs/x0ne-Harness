//! A git dependency moves only through `scripts/repin-public.sh`, which
//! checks that the new commit is public and makes a code change a reviewed
//! one. Dependabot would propose the head of the default branch every week
//! and GOVERNANCE merges its PRs on green CI (audit 2026-11-02, A6).

use std::fs;
use std::path::Path;

#[test]
fn dependabot_ignores_every_git_dependency() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    let git: Vec<&String> = manifest["workspace"]["dependencies"]
        .as_table()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.get("git").is_some())
        .map(|(k, _)| k)
        .collect();
    assert!(
        git.iter().any(|k| *k == "x0ne-confidential"),
        "the Engine is a git dependency: {git:?}"
    );

    let bot = fs::read_to_string(root.join(".github/dependabot.yml")).unwrap();
    let cargo = bot
        .split("- package-ecosystem:")
        .find(|b| b.trim_start().starts_with("cargo"))
        .expect("a cargo block in dependabot.yml");
    for name in git {
        assert!(
            cargo.lines().any(|l| {
                let l = l.trim();
                l == format!("- dependency-name: \"{name}\"")
                    || l == format!("- dependency-name: {name}")
            }),
            "dependabot.yml must ignore the git dependency {name}"
        );
    }
}
