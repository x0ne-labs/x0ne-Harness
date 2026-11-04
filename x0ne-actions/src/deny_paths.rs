//! The default deny-path patterns and their matcher — ONE copy, two callers.
//!
//! `deny_path_patterns` is the one list in the whole policy that a human
//! "yes" cannot lift (`x0ne-runtime/src/policy.rs`, `liftable`). Its default
//! contents used to live only in the runtime's config defaults, which meant
//! the orchestrator's read-only dispensation (`readonly_cmd`) could not see
//! them without duplicating the list — and a duplicated security list drifts
//! (the repository's recorded failure mode). So the list and its matcher
//! live here, in the shared vocabulary crate, and BOTH sides call the same
//! function — the same shape as `serve_command`.
//!
//! This module is pure: strings in, verdict out. Canonicalization and tilde
//! expansion are the caller's business — the runtime matches canonicalized
//! paths, `readonly_cmd` expands what it can before asking.

/// What the runtime denies by default: the material an attacker needs to
/// stop being an attacker on this host. Mirrored by
/// `AppConfig::default().policy.deny_path_patterns` — via this constant,
/// never by a second list.
pub const DEFAULT_DENY_PATH_PATTERNS: &[&str] = &[
    "**/.ssh/**",
    "**/.gnupg/**",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/*.key",
    "**/*.pem",
    "**/secrets/**",
    // `.env`, `.env.local`, `.envrc`: where projects and x0ne keep secrets.
    // A false positive on `.env.example` only asks a human.
    "**/.env*",
    // The environment of any process or thread, the orchestrator's included.
    "/proc/**/environ",
    // x0ne's own stores (audit 2026-11-02, A1): the journal signing key, the
    // HTTP tokens, the user configs that hold a model key or the hooks, the
    // project trust store, the snapshot copies. Named one by one: a project's
    // own `.x0ne/` (rules, skills) stays readable.
    "**/.x0ne/keys/**",
    "**/.x0ne/run/**",
    "/run/user/*/x0ne/**",
    "**/.x0ne/config.toml",
    "**/.x0ne/trusted-projects.toml",
    "**/.x0ne/snapshots/**",
    "**/.x0ne-secrets/**",
    "**/.config/x0ne/**",
    "**/.config/x0ne-orchestrator/**",
    // Other agents' credentials.
    "**/.claude/.credentials.json",
    "**/.codex/auth.json",
    // Forges and git.
    "**/.config/gh/**",
    "**/.config/hub",
    "**/.netrc",
    "**/.git-credentials",
    // Clusters and clouds. `.docker/` is a build directory in many projects:
    // only its config.json holds credentials.
    "**/.kube/**",
    "**/.aws/**",
    "**/.azure/**",
    "**/.config/gcloud/**",
    "**/.docker/config.json",
    // Other tools: the stores `executor::isolation` hides from commands.
    "**/.pgpass",
    "**/.pypirc",
    "**/.cargo/credentials*",
    "**/.vault-token",
    "**/.terraform.d/credentials.tfrc.json",
    "**/.password-store/**",
    "**/.local/share/keyrings/**",
];

/// A compiled deny list and its matcher: the runtime gate, the search
/// walkers, the orchestrator's read-only dispensation and the PreToolUse
/// hook all match through it, so they cannot disagree on what a pattern
/// means. A pattern that does not compile is skipped here: the runtime
/// refuses the config that holds one ([`invalid_patterns`]).
#[derive(Debug, Clone, Default)]
pub struct DenyList {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    written: String,
    pattern: glob::Pattern,
    /// `<dir>/**` also denies `<dir>`: glob needs the trailing separator, so
    /// `**/.ssh/**` missed `/home/u/.ssh` itself (audit 2026-11-02, A2).
    dir: Option<glob::Pattern>,
}

impl DenyList {
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries = patterns
            .into_iter()
            .filter_map(|p| {
                let written = p.as_ref().to_string();
                let pattern = glob::Pattern::new(&written).ok()?;
                let dir = written
                    .strip_suffix("/**")
                    .filter(|d| !d.is_empty())
                    .and_then(|d| glob::Pattern::new(d).ok());
                Some(Entry {
                    written,
                    pattern,
                    dir,
                })
            })
            .collect();
        Self { entries }
    }

    /// The defaults, compiled once.
    pub fn defaults() -> &'static DenyList {
        static DEFAULTS: std::sync::OnceLock<DenyList> = std::sync::OnceLock::new();
        DEFAULTS.get_or_init(|| DenyList::new(DEFAULT_DENY_PATH_PATTERNS))
    }

    /// The first pattern `path` matches, as written in the list.
    pub fn first_match(&self, path: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.pattern.matches(path) || e.dir.as_ref().is_some_and(|d| d.matches(path)))
            .map(|e| e.written.as_str())
    }
}

/// The first default pattern `path` matches, if any, by [`DenyList`].
pub fn matches_default_deny(path: &str) -> Option<&'static str> {
    DenyList::defaults().first_match(path)
}

/// Each pattern of `patterns` that glob cannot compile, with its reason.
/// Such a pattern denies nothing, so a loader must refuse it.
pub fn invalid_patterns<I, S>(patterns: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    patterns
        .into_iter()
        .filter_map(|p| {
            let p = p.as_ref();
            glob::Pattern::new(p).err().map(|e| format!("{p:?} ({e})"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list guards what it says it guards. A failure means the default
    /// boundary moved without this test moving with it — which is the drift
    /// this module exists to prevent.
    #[test]
    fn the_default_patterns_cover_the_material_they_name() {
        for hit in [
            "/home/u/.ssh/id_rsa",
            "/home/u/.ssh/config",
            "/home/u/.gnupg/private-keys-v1.d/x.key",
            "/etc/ssl/private/server.pem",
            "/srv/app/secrets/db-password",
            "/home/u/backup/id_ed25519.old",
            "/home/u/proj/.env",
            "/home/u/proj/api/.env.production",
            "/home/u/proj/.envrc",
            "/home/u/.x0ne/.env",
            "/proc/self/environ",
            "/proc/12345/task/12346/environ",
        ] {
            assert!(matches_default_deny(hit).is_some(), "should match: {hit}");
        }
        for miss in [
            "/home/u/README.md",
            "/home/u/src/main.rs",
            "/var/log/syslog",
            "/home/u/proj/environment.md",
            "/home/u/proj/src/env.rs",
            "/proc/self/status",
        ] {
            assert!(
                matches_default_deny(miss).is_none(),
                "should not match: {miss}"
            );
        }
    }

    /// The credential stores of x0ne itself, of other agents and of the
    /// usual developer tools, one family per array (audit 2026-11-02, A1).
    #[test]
    fn the_defaults_cover_the_credential_stores() {
        let families: [(&str, &[&str]); 5] = [
            (
                "x0ne",
                &[
                    "/home/u/.x0ne/keys/journal-ed25519",
                    "/home/u/.x0ne/run/runtime-7878.token",
                    "/run/user/1000/x0ne/serve-7700.token",
                    "/home/u/.x0ne/config.toml",
                    "/home/u/.x0ne/trusted-projects.toml",
                    "/home/u/.x0ne/snapshots/s1/0.dat",
                    "/home/u/.config/x0ne/connector-secrets.toml",
                    "/home/u/.config/x0ne/x0ne-orchestrator.toml",
                    "/home/u/.config/x0ne-orchestrator/config.toml",
                ],
            ),
            (
                "agents",
                &[
                    "/home/u/.claude/.credentials.json",
                    "/home/u/.codex/auth.json",
                ],
            ),
            (
                "forges",
                &[
                    "/home/u/.config/gh/hosts.yml",
                    "/home/u/.config/hub",
                    "/home/u/.netrc",
                    "/home/u/.git-credentials",
                ],
            ),
            (
                "clusters and clouds",
                &[
                    "/home/u/.kube/config",
                    "/home/u/.aws/credentials",
                    "/home/u/.azure/msal_token_cache.json",
                    "/home/u/.config/gcloud/credentials.db",
                    "/home/u/.docker/config.json",
                ],
            ),
            (
                "other tools",
                &[
                    "/home/u/.pgpass",
                    "/home/u/.pypirc",
                    "/home/u/.cargo/credentials.toml",
                    "/home/u/.vault-token",
                    "/home/u/.password-store/bank.gpg",
                    "/home/u/.local/share/keyrings/login.keyring",
                ],
            ),
        ];
        for (family, paths) in families {
            for p in paths {
                assert!(matches_default_deny(p).is_some(), "{family}: {p}");
            }
        }
        // A project's own `.x0ne/` (rules, skills) and `.claude/` stay readable.
        for miss in [
            "/home/u/proj/.x0ne/rules/style.md",
            "/home/u/proj/.x0ne/X0NE.md",
            "/home/u/proj/.claude/settings.json",
            "/home/u/.x0ne/screenshots/s.png",
        ] {
            assert_eq!(matches_default_deny(miss), None, "{miss}");
        }
    }

    /// A `<dir>/**` pattern denies the directory itself, not only what is
    /// under it: glob needs the trailing separator, and `/home/u/.ssh` could
    /// be listed, stat'ed and moved to the trash (audit 2026-11-02, A2).
    #[test]
    fn a_directory_pattern_denies_the_bare_directory() {
        for (dir, pat) in [
            ("/home/u/.ssh", "**/.ssh/**"),
            ("/home/u/.gnupg", "**/.gnupg/**"),
            ("/srv/app/secrets", "**/secrets/**"),
        ] {
            assert_eq!(matches_default_deny(dir), Some(pat), "{dir}");
        }
        for miss in ["/home/u/.sshd", "/home/u/my.ssh", "/srv/app/secrets.md"] {
            assert_eq!(matches_default_deny(miss), None, "{miss}");
        }
        let own = DenyList::new(["/srv/private/**"]);
        assert_eq!(own.first_match("/srv/private"), Some("/srv/private/**"));
        assert_eq!(own.first_match("/srv/private/a"), Some("/srv/private/**"));
        assert_eq!(own.first_match("/srv/privateer"), None);
    }

    /// `DenyList::new` drops what glob cannot compile; the loader learns it here.
    #[test]
    fn a_pattern_that_does_not_compile_is_reported() {
        let bad = invalid_patterns(["**/.ssh/**", "**/[unclosed", "***"]);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad[0].contains("**/[unclosed"), "{bad:?}");
        assert!(invalid_patterns(DEFAULT_DENY_PATH_PATTERNS).is_empty());
    }
}
