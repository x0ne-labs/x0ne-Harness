//! Policy load matrix (T-A).
//!
//! # Why this module exists
//!
//! The repository let through three dead wizard profiles and three
//! policy keys without effect because its tests **parsed** a TOML
//! fragment instead of **loading** it. A `toml::from_str` tests serde; it
//! sees neither the `[actions]` merge (`config.rs:675`), nor the fail-closed
//! refusal of an unknown action key (`config.rs:649`), nor the way the engine
//! is built in production.
//!
//! SECURITY: `load` is the ONLY path allowed in this module. Any matrix cell
//! that built an `AppConfig` by hand would test a policy the runtime never
//! executes.

use crate::config::AppConfig;
use crate::policy::PolicyEngine;

/// A configuration written to disk, loaded by the real loader, and the
/// engine built exactly like `main.rs:158`.
pub(crate) struct Loaded {
    pub cfg: AppConfig,
    pub engine: PolicyEngine,
    /// Kept alive: the temporary directory dies with the struct.
    _dir: tempfile::TempDir,
}

/// Writes `toml` into a fresh directory and loads it through `AppConfig::load`.
pub(crate) fn load(toml: &str) -> anyhow::Result<Loaded> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("x0ne-runtime.toml");
    std::fs::write(&path, toml)?;
    let cfg = AppConfig::load(&path)?;
    // SECURITY: identical to production (`x0ne-runtime/src/main.rs:158`).
    // Building the engine any other way would test a phantom policy.
    let engine = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
    Ok(Loaded {
        cfg,
        engine,
        _dir: dir,
    })
}

/// The body of `[policy]`: every required v0.3 key, nothing permissive.
/// A constant and not a function so that the following T-A tasks compose
/// the same skeleton, word for word.
///
/// SECURITY: `mode = "auto"` is written here, and it is not a detail. The
/// compiled default of `policy.mode` is `"confirm"` (`config.rs:234`), and
/// `evaluate` handles the modes BEFORE the per-action switch
/// (`policy.rs:534-544`): under `"confirm"`, every modifying action returns
/// `RequireConfirmation` without ever consulting `[actions]`. A matrix
/// built on that default would measure the mode and nothing else — the ~550
/// `[actions] × default_effect × action_defaults` cells would all be
/// identical. `"auto"` is the canonical posture of `--mode stdio` (« applies
/// the lists, the orchestrator holds the human gate », `config.rs:842`),
/// so it is the one that isolates the switch, as `policy.rs:1835` already
/// does.
pub(crate) const BASE_POLICY: &str = r#"mode = "auto"
default_effect = "deny"
max_read_bytes = 65536
max_list_entries = 100
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []
"#;

/// The minimal v0.3 skeleton: every required `[policy]` key, nothing
/// permissive. `extra` is appended INSIDE `[policy]`.
pub(crate) fn policy_toml(mode: &str, extra: &str) -> String {
    format!("\n[runtime]\nmode = \"{mode}\"\n\n[policy]\n{BASE_POLICY}{extra}\n")
}

/// The key of a `key = value` line. An empty line or a table header have
/// no key in this module's sense and return a string that collides with
/// nothing useful.
fn toml_key(line: &str) -> &str {
    match line.split_once('=') {
        Some((k, _)) => k.trim(),
        None => "",
    }
}

/// Like `policy_toml`, but `line` **replaces** the skeleton line with the
/// same key instead of being appended to it.
///
/// Measured red on 2026-08-16: `BASE_POLICY` already carries `mode`,
/// `max_read_bytes`, `max_list_entries` and `allowed_commands`, and TOML
/// refuses a duplicate key in a table. A matrix cell appended to the skeleton
/// would therefore not measure its key, it would measure the parser — and it
/// would fail for a reason that says nothing about the runtime. Replacement
/// keeps the only property that matters: the written file declares the value
/// **once**, and that is the one required back.
pub(crate) fn policy_toml_overriding(mode: &str, line: &str) -> String {
    policy_toml_overriding_lines(mode, &[line])
}

/// `policy_toml_overriding` for several keys at once.
///
/// A path cell needs two at a time — `allowed_read_paths` and
/// `deny_path_patterns` must coexist to measure which of the two wins —
/// and both are already in `BASE_POLICY`. Passing the lines one by one
/// does not work: each call removes only its own key from the skeleton,
/// so the second comes back duplicated and the file no longer loads.
pub(crate) fn policy_toml_overriding_lines(mode: &str, lines: &[&str]) -> String {
    let keys: Vec<&str> = lines.iter().map(|l| toml_key(l)).collect();
    let body: String = BASE_POLICY
        .lines()
        .filter(|l| !keys.contains(&toml_key(l)))
        .map(|l| format!("{l}\n"))
        .collect();
    let extra: String = lines.iter().map(|l| format!("{l}\n")).collect();
    format!("\n[runtime]\nmode = \"{mode}\"\n\n[policy]\n{body}{extra}")
}

#[cfg(test)]
mod harness {
    use super::*;
    use crate::policy::{DenyReason, PolicyEffect};
    use x0ne_actions::Action;

    fn run_echo() -> Action {
        Action::RunCommand {
            command: "echo".into(),
            args: vec![],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        }
    }

    /// The harness must build the engine like production does.
    ///
    /// A failure would mean the ~1,500 cells of the following tasks
    /// measure a policy the runtime does not execute — that is,
    /// nothing at all.
    ///
    /// The REASON for the refusal is what proves it, not the refusal. An
    /// engine built with an empty action map — the exact error this test
    /// exists to catch — would ALSO refuse `RunCommand`, but through
    /// `DefaultDeny`, falling back to `default_effect`. Only
    /// `ActionDisabled` says that `cfg.actions` made the trip.
    #[test]
    fn the_engine_is_built_from_the_actions_the_loader_resolved() {
        let l = load(&policy_toml("stdio", "")).expect("the v0.3 skeleton must load");
        // `RunCommand` is OFF by compiled default and no `[actions]` turns it
        // on: the engine must refuse, and it can only know that because
        // `load` passed it `cfg.actions`.
        let effect = l.engine.evaluate(&run_echo());
        assert!(
            matches!(effect, PolicyEffect::Deny(DenyReason::ActionDisabled)),
            "RunCommand must be refused BY ITS SWITCH under the minimal \
             skeleton, got {effect:?}"
        );
    }

    /// The merge of `[actions]` with the defaults (`config.rs:675`) must be
    /// in the path, and a `toml::from_str` would not do it.
    ///
    /// A failure would mean the engine sees the WRITTEN `[actions]` table
    /// (here: a single key) instead of the RESOLVED table — so every matrix
    /// cell measuring an action not listed would measure `DefaultDeny`
    /// instead of the real policy.
    #[test]
    fn the_merge_of_the_actions_table_is_in_the_path() {
        // A single key written, and it does not mention `ReadFile`. Without
        // the merge, `ReadFile` would be missing from the map and would fall
        // back to `default_effect = "deny"`.
        let toml = format!(
            "{}\n[actions]\nRunCommand = false\n",
            policy_toml("stdio", "")
        );
        let l = load(&toml).expect("a partial [actions] table must load");
        let effect = l.engine.evaluate(&Action::ReadFile {
            path: "/etc/hostname".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        });
        assert!(
            !matches!(effect, PolicyEffect::Deny(DenyReason::DefaultDeny)),
            "ReadFile must be known to the merged map; got {effect:?} — \
             the merge at config.rs:675 is not in the path"
        );
        // And it stays refused, because `allowed_read_paths` is empty: the
        // switch is open, confinement closes. This is the proof that the
        // previous assertion is not an « everything passes ».
        assert!(
            matches!(
                effect,
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "ReadFile must fall through to path confinement, got {effect:?}"
        );
    }
}

#[cfg(test)]
mod degradations {
    use super::*;

    /// A broken configuration, and the only thing required of it.
    struct Broken {
        name: &'static str,
        toml: String,
    }

    fn broken_cases() -> Vec<Broken> {
        let b = |name, toml: String| Broken { name, toml };
        vec![
            b("empty file", String::new()),
            b("invalid toml", "[policy\n".into()),
            b(
                "v0.2 without default_effect",
                r#"
[policy]
max_read_bytes = 65536
max_list_entries = 100
allowed_commands = []
allowed_read_paths = ["/"]
allowed_write_paths = ["/"]
deny_path_patterns = []
"#
                .into(),
            ),
            b("[runtime] without mode", "[runtime]\nhttp = {}\n".into()),
            b(
                "[executor] truncated",
                format!(
                    "{}\n[executor]\ndefault_timeout_secs = 30\n",
                    policy_toml("stdio", "")
                ),
            ),
            b("unknown mode", policy_toml("htpp", "")),
            b(
                "wrong type on max_read_bytes",
                policy_toml("stdio", "max_read_bytes = \"beaucoup\""),
            ),
            b(
                "unknown action in [actions]",
                format!("{}\n[actions]\nReadFyle = true\n", policy_toml("stdio", "")),
            ),
            b(
                "unknown default_effect",
                policy_toml("stdio", "").replace("\"deny\"", "\"maybe\""),
            ),
        ]
    }

    /// SECURITY: every degradation closes, none opens.
    ///
    /// A failure here would mean that a damaged policy file — by a typo,
    /// a failed migration, a truncated `scp` — grants more than the healthy
    /// file it replaces. This is the costliest possible defect class in this
    /// repository: it is silent.
    #[test]
    fn no_broken_configuration_ever_widens_the_policy() {
        for case in broken_cases() {
            match load(&case.toml) {
                // Refusing to load is the right answer.
                Err(_) => {}
                // Loading is accepted IF and ONLY IF the result opens nothing
                // more than the fail-closed skeleton.
                Ok(l) => {
                    assert_eq!(
                        l.cfg.policy.default_effect, "deny",
                        "{}: loaded, but default_effect is no longer deny",
                        case.name
                    );
                    assert!(
                        l.cfg.policy.allowed_write_paths.is_empty(),
                        "{}: loaded, but write roots appeared",
                        case.name
                    );
                    assert!(
                        !l.cfg.actions.get("RunCommand").copied().unwrap_or(false),
                        "{}: loaded, but RunCommand became true",
                        case.name
                    );
                }
            }
        }
    }

    /// An unknown action in `[actions]` must refuse the load AND name the
    /// fault — that is what `config.rs:649-673` promises.
    #[test]
    fn an_unknown_action_key_refuses_the_load_and_names_it() {
        let toml = format!("{}\n[actions]\nReadFyle = true\n", policy_toml("stdio", ""));
        // `expect_err` would require `Loaded: Debug`, and `PolicyEngine` has
        // none: the harness derives nothing for an error message.
        let err = match load(&toml) {
            Ok(_) => panic!("an unknown action key must refuse loading"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ReadFyle"),
            "the error must name the offending key: {msg}"
        );
    }

    /// SECURITY: a `default_effect` the runtime does not know must refuse
    /// the load, and name the value.
    ///
    /// A failure would mean that a typo in ONE word — `"Deny"`, `"denny"` —
    /// reads as « not deny » at `policy.rs:554`, and that an action missing
    /// from the `[actions]` map goes through the per-action switch instead of
    /// being refused there. Under `action_defaults = "replace"`, the map only
    /// contains what the operator listed: that is every action they did not
    /// name. Measured red on 2026-08-16 — the loader accepted `"maybe"`
    /// without a word.
    #[test]
    fn an_unknown_default_effect_refuses_the_load_and_names_it() {
        let toml = policy_toml("stdio", "").replace(r#""deny""#, r#""maybe""#);
        let err = match load(&toml) {
            Ok(_) => panic!("an unknown default_effect must refuse loading"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("maybe"),
            "the error must name the offending value: {msg}"
        );
        // And the three legitimate values load, otherwise this refusal would
        // be a refusal of everything.
        for good in ["allow", "deny", "confirm"] {
            let toml = policy_toml("stdio", "").replace(r#""deny""#, &format!("\"{good}\""));
            assert!(
                load(&toml).is_ok(),
                "default_effect = \"{good}\" must stay loadable"
            );
        }
    }
}

#[cfg(test)]
mod provenance {
    use super::*;
    use crate::config::{AppConfig, ConfigSource};

    /// SECURITY: `./x0ne-runtime.toml` is read ONLY under
    /// `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1` (`config.rs:19-21`). A failure here
    /// would mean a cloned repository can supply its own policy to an agent
    /// launched inside it — an escalation by a mere `git clone`.
    ///
    /// The test contains BOTH directions, and the second is not decorative:
    /// without it, a `find_and_load_with_source` that had stopped looking at
    /// the current directory at all would pass the first silently, and the
    /// negative would prove nothing anymore.
    ///
    /// This test manipulates the current directory and two environment
    /// variables, so it is serialized by its own lock: two tests that change
    /// `cwd` in parallel corrupt each other.
    ///
    /// `XDG_CONFIG_HOME` is redirected to an empty directory for the duration
    /// of the test. Without that, the XDG branch finds the policy ACTUALLY
    /// installed on the machine — measured on 2026-08-16: the first draft of
    /// this test failed by reading the owner's
    /// `~/.config/x0ne/x0ne-runtime.toml`, that is, it measured the workstation
    /// and not the code.
    /// Shared by every test here that changes `cwd` or the environment.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_cwd_config_is_ignored_unless_the_operator_opted_in() {
        let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // A file that would open everything if it were read: the whole root
        // writable. `extra` cannot do it — `BASE_POLICY` already carries the
        // key and TOML refuses a duplicate —, so the value is replaced.
        let widening = policy_toml("stdio", "")
            .replace("allowed_write_paths = []", r#"allowed_write_paths = ["/"]"#);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x0ne-runtime.toml"), &widening).unwrap();
        assert!(
            load(&widening)
                .expect("the widening file must be loadable")
                .cfg
                .policy
                .allowed_write_paths
                .contains(&"/".to_string()),
            "the planted file must actually widen, otherwise this test proves nothing"
        );

        // An empty XDG: with the cwd file ignored there is no policy at all,
        // and the search must refuse rather than fall back.
        let empty_xdg = tempfile::tempdir().unwrap();
        let previous_dir = std::env::current_dir().unwrap();
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let previous_optin = std::env::var_os(crate::config::ALLOW_CWD_CONFIG_ENV);
        std::env::set_var("XDG_CONFIG_HOME", empty_xdg.path());
        std::env::set_current_dir(dir.path()).unwrap();

        std::env::remove_var(crate::config::ALLOW_CWD_CONFIG_ENV);
        let without = AppConfig::find_and_load_with_source(None);

        std::env::set_var(crate::config::ALLOW_CWD_CONFIG_ENV, "1");
        let with = AppConfig::find_and_load_with_source(None);

        // Restored BEFORE any assertion: a panic must not leave the test
        // process in a deleted directory nor under a redirected XDG — the
        // binary's other tests run in parallel.
        std::env::set_current_dir(previous_dir).unwrap();
        match previous_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match previous_optin {
            Some(v) => std::env::set_var(crate::config::ALLOW_CWD_CONFIG_ENV, v),
            None => std::env::remove_var(crate::config::ALLOW_CWD_CONFIG_ENV),
        }

        match without {
            Ok((_, src)) => panic!("without opt-in nothing may load, got {src}"),
            Err(e) => assert!(
                e.to_string().contains("no policy file"),
                "without opt-in, the refusal must say no policy file was found: {e:#}"
            ),
        }

        let (cfg, src) = with.expect("under opt-in, the cwd file must load");
        assert!(
            matches!(src, ConfigSource::Cwd(_)),
            "under opt-in, the policy must come from the current directory, got {src}"
        );
        assert!(
            cfg.policy.allowed_write_paths.contains(&"/".to_string()),
            "under opt-in, the planted file must be the one in force"
        );
    }

    /// Attack: a hostile checkout plants a three-line `./x0ne-runtime.toml`
    /// — a PARTIAL `[policy]`, now that partial tables complete with defaults
    /// — that would open every path and flip `default_effect`. Without the
    /// opt-in nothing loads: no policy file, no start.
    /// Same cwd/env discipline as the test above, same lock.
    #[test]
    fn a_partial_cwd_policy_cannot_widen_anything_without_opt_in() {
        let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let hostile = "[policy]\ndefault_effect = \"allow\"\nallowed_write_paths = [\"/\"]\n\
                       allowed_commands = [\"bash\"]\n[actions]\nRunCommand = true\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x0ne-runtime.toml"), hostile).unwrap();
        let planted = AppConfig::load(&dir.path().join("x0ne-runtime.toml"))
            .expect("the hostile partial file must itself be loadable, or this proves nothing");
        assert_eq!(planted.policy.default_effect, "allow");
        assert!(planted
            .policy
            .allowed_write_paths
            .contains(&"/".to_string()));

        let empty_xdg = tempfile::tempdir().unwrap();
        let previous_dir = std::env::current_dir().unwrap();
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let previous_optin = std::env::var_os(crate::config::ALLOW_CWD_CONFIG_ENV);
        std::env::set_var("XDG_CONFIG_HOME", empty_xdg.path());
        std::env::set_current_dir(dir.path()).unwrap();
        std::env::remove_var(crate::config::ALLOW_CWD_CONFIG_ENV);
        let loaded = AppConfig::find_and_load_with_source(None);
        // `0` is not an opt-in either (`config.rs`, `cwd_config_is_allowed`).
        std::env::set_var(crate::config::ALLOW_CWD_CONFIG_ENV, "0");
        let loaded_zero = AppConfig::find_and_load_with_source(None);

        std::env::set_current_dir(previous_dir).unwrap();
        match previous_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match previous_optin {
            Some(v) => std::env::set_var(crate::config::ALLOW_CWD_CONFIG_ENV, v),
            None => std::env::remove_var(crate::config::ALLOW_CWD_CONFIG_ENV),
        }

        for loaded in [loaded, loaded_zero] {
            match loaded {
                Ok((cfg, src)) => panic!(
                    "the planted file was obeyed from {src} (default_effect={})",
                    cfg.policy.default_effect
                ),
                Err(e) => assert!(e.to_string().contains("no policy file"), "{e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod keys_by_mode {
    use super::*;

    /// A configuration key, the TOML that declares it, and what the runtime
    /// must have made of it once the file is loaded.
    ///
    /// `check` is a **function pointer** and the expected value it carries is
    /// a literal constant: the table stays LITERAL, never derived from a
    /// configuration field. A matrix built dynamically from a constant is the
    /// memory bomb of 2026-08-16 (fixture sized on a mutated constant → 9 EB
    /// to allocate); an expected value that restates the tested code is fault
    /// P2 of the spec (a test that re-implements the logic it tests can no
    /// longer contradict it).
    ///
    /// `line` is the text the cell writes: a `key = value` line for the
    /// `[policy]` cells, a complete block of sections for the others.
    pub(super) struct KeyCell {
        pub name: &'static str,
        pub line: &'static str,
        pub check: fn(&Loaded),
    }

    /// The three modes `main.rs:194-203` can select.
    ///
    /// SECURITY: the list is literal and closed. `main.rs` ends its `match`
    /// with `"stdio" | _`, so a fourth mode does not exist — it falls back to
    /// stdio. Crossing every key with these three is what prevents a key
    /// from being live in stdio and decorative in http.
    const MODES: &[&str] = &["stdio", "http", "mcp"];

    /// Like `policy_toml`, but `sections` is a complete block of sections
    /// appended AFTER `[policy]`.
    pub(super) fn config_with_sections(mode: &str, sections: &str) -> String {
        format!("{}\n{}", super::policy_toml(mode, ""), sections)
    }

    /// The `[policy]` keys (`config.rs:170`), one cell per key.
    ///
    /// `default_effect`, `allowed_read_paths`, `allowed_write_paths` and
    /// `deny_path_patterns` are absent on purpose: the first has its own
    /// degradation test (`an_unknown_default_effect_refuses_the_load…`) and
    /// the other three are the « Paths » cell of the T-A spec, which requires
    /// an action evaluation and not a field read.
    fn policy_cells() -> &'static [KeyCell] {
        &[
            KeyCell {
                name: "policy.mode",
                line: r#"mode = "readonly""#,
                check: |l| assert_eq!(l.cfg.policy.mode, "readonly"),
            },
            KeyCell {
                name: "policy.max_read_bytes",
                line: "max_read_bytes = 4096",
                check: |l| assert_eq!(l.cfg.policy.max_read_bytes, 4096),
            },
            KeyCell {
                name: "policy.max_list_entries",
                line: "max_list_entries = 7",
                check: |l| assert_eq!(l.cfg.policy.max_list_entries, 7),
            },
            KeyCell {
                name: "policy.max_command_timeout_secs",
                line: "max_command_timeout_secs = 12",
                check: |l| assert_eq!(l.cfg.policy.max_command_timeout_secs, 12),
            },
            KeyCell {
                name: "policy.allowed_commands",
                line: r#"allowed_commands = ["git", "ls"]"#,
                check: |l| assert_eq!(l.cfg.policy.allowed_commands, vec!["git", "ls"]),
            },
            KeyCell {
                name: "policy.allowed_images",
                line: r#"allowed_images = ["docker.io/library/alpine:3"]"#,
                check: |l| {
                    assert_eq!(
                        l.cfg.policy.allowed_images,
                        vec!["docker.io/library/alpine:3"]
                    )
                },
            },
            KeyCell {
                name: "policy.readable_env_vars",
                line: r#"readable_env_vars = ["LANG"]"#,
                check: |l| assert_eq!(l.cfg.policy.readable_env_vars, vec!["LANG"]),
            },
            KeyCell {
                name: "policy.env_passthrough",
                line: r#"env_passthrough = ["CI"]"#,
                check: |l| assert_eq!(l.cfg.policy.env_passthrough, vec!["CI"]),
            },
            KeyCell {
                name: "policy.action_defaults",
                line: r#"action_defaults = "replace""#,
                check: |l| assert_eq!(l.cfg.policy.action_defaults, "replace"),
            },
        ]
    }

    /// Every `[policy]` key is loaded at its value, in all three modes.
    ///
    /// A failure would mean a policy key is decorative in at least one
    /// service mode — the exact definition of the dead scaffolding this
    /// repository produces in series.
    #[test]
    fn every_policy_key_survives_the_load_in_every_mode() {
        for mode in MODES {
            for cell in policy_cells() {
                let l = load(&policy_toml_overriding(mode, cell.line))
                    .unwrap_or_else(|e| panic!("{}/{}: {e:#}", mode, cell.name));
                (cell.check)(&l);
                assert_eq!(
                    l.cfg.runtime.mode, *mode,
                    "{}: the mode was lost",
                    cell.name
                );
            }
        }
    }

    /// The keys of the ten other sections (`config.rs:98`), one cell per key.
    ///
    /// `[executor]` (`:286`) is the special case of the list: its container
    /// has **no** `#[serde(default)]` and none of its five fields has one,
    /// so writing the section forces writing it WHOLE. Every `executor.*`
    /// cell therefore carries all five keys, and checks only one — this is the
    /// same property as the « truncated `[executor]` » degradation test,
    /// taken from the other end.
    fn section_cells() -> &'static [KeyCell] {
        &[
            // ── [executor] (config.rs:286) — section indivisible ─────────────
            KeyCell {
                name: "executor.default_timeout_secs",
                line: "[executor]\n\
                       default_timeout_secs = 17\n\
                       max_file_read_bytes = 102400\n\
                       trash_dir = \"~/.x0ne-trash\"\n\
                       max_search_results = 50\n\
                       max_log_lines = 100\n",
                check: |l| assert_eq!(l.cfg.executor.default_timeout_secs, 17),
            },
            KeyCell {
                name: "executor.max_file_read_bytes",
                line: "[executor]\n\
                       default_timeout_secs = 30\n\
                       max_file_read_bytes = 4097\n\
                       trash_dir = \"~/.x0ne-trash\"\n\
                       max_search_results = 50\n\
                       max_log_lines = 100\n",
                check: |l| assert_eq!(l.cfg.executor.max_file_read_bytes, 4097),
            },
            KeyCell {
                name: "executor.trash_dir",
                line: "[executor]\n\
                       default_timeout_secs = 30\n\
                       max_file_read_bytes = 102400\n\
                       trash_dir = \"/tmp/x0ne-matrix-trash\"\n\
                       max_search_results = 50\n\
                       max_log_lines = 100\n",
                check: |l| assert_eq!(l.cfg.executor.trash_dir, "/tmp/x0ne-matrix-trash"),
            },
            KeyCell {
                name: "executor.max_search_results",
                line: "[executor]\n\
                       default_timeout_secs = 30\n\
                       max_file_read_bytes = 102400\n\
                       trash_dir = \"~/.x0ne-trash\"\n\
                       max_search_results = 3\n\
                       max_log_lines = 100\n",
                check: |l| assert_eq!(l.cfg.executor.max_search_results, 3),
            },
            KeyCell {
                name: "executor.max_log_lines",
                line: "[executor]\n\
                       default_timeout_secs = 30\n\
                       max_file_read_bytes = 102400\n\
                       trash_dir = \"~/.x0ne-trash\"\n\
                       max_search_results = 50\n\
                       max_log_lines = 11\n",
                check: |l| assert_eq!(l.cfg.executor.max_log_lines, 11),
            },
            // ── [snapshots] (config.rs:310) ──────────────────────────────────
            KeyCell {
                name: "snapshots.dir",
                line: "[snapshots]\ndir = \"/tmp/x0ne-matrix-snapshots\"\n",
                check: |l| assert_eq!(l.cfg.snapshots.dir, "/tmp/x0ne-matrix-snapshots"),
            },
            KeyCell {
                name: "snapshots.max_snapshots",
                line: "[snapshots]\nmax_snapshots = 3\n",
                check: |l| assert_eq!(l.cfg.snapshots.max_snapshots, 3),
            },
            KeyCell {
                name: "snapshots.max_snapshot_size_mb",
                line: "[snapshots]\nmax_snapshot_size_mb = 7\n",
                check: |l| assert_eq!(l.cfg.snapshots.max_snapshot_size_mb, 7),
            },
            // ── [handles] (config.rs:337) ────────────────────────────────────
            KeyCell {
                name: "handles.max_concurrent",
                line: "[handles]\nmax_concurrent = 4\n",
                check: |l| assert_eq!(l.cfg.handles.max_concurrent, 4),
            },
            KeyCell {
                name: "handles.completed_retention_secs",
                line: "[handles]\ncompleted_retention_secs = 61\n",
                check: |l| assert_eq!(l.cfg.handles.completed_retention_secs, 61),
            },
            // ── [events] (config.rs:355) ─────────────────────────────────────
            KeyCell {
                name: "events.max_subscriptions",
                line: "[events]\nmax_subscriptions = 5\n",
                check: |l| assert_eq!(l.cfg.events.max_subscriptions, 5),
            },
            KeyCell {
                name: "events.max_events_per_handle",
                line: "[events]\nmax_events_per_handle = 13\n",
                check: |l| assert_eq!(l.cfg.events.max_events_per_handle, 13),
            },
            KeyCell {
                name: "events.debounce_ms",
                line: "[events]\ndebounce_ms = 250\n",
                check: |l| assert_eq!(l.cfg.events.debounce_ms, 250),
            },
            KeyCell {
                name: "events.port_check_interval_secs",
                line: "[events]\nport_check_interval_secs = 9\n",
                check: |l| assert_eq!(l.cfg.events.port_check_interval_secs, 9),
            },
            KeyCell {
                name: "events.port_check_timeout_secs",
                line: "[events]\nport_check_timeout_secs = 44\n",
                check: |l| assert_eq!(l.cfg.events.port_check_timeout_secs, 44),
            },
            // ── [system] (config.rs:379) ─────────────────────────────────────
            // The compiled default is `true`; the cell writes `false`, so a
            // loader that lost the key would return `true` and the test fails.
            KeyCell {
                name: "system.enable_process_list",
                line: "[system]\nenable_process_list = false\n",
                check: |l| assert!(!l.cfg.system.enable_process_list),
            },
            // ── [desktop] (config.rs:395) ────────────────────────────────────
            KeyCell {
                name: "desktop.enabled",
                line: "[desktop]\nenabled = true\n",
                check: |l| assert!(l.cfg.desktop.enabled),
            },
            KeyCell {
                name: "desktop.allowed_apps",
                line: "[desktop]\nallowed_apps = [\"firefox\"]\n",
                check: |l| assert_eq!(l.cfg.desktop.allowed_apps, vec!["firefox"]),
            },
            KeyCell {
                name: "desktop.screenshot_confirm",
                line: "[desktop]\nscreenshot_confirm = false\n",
                check: |l| assert!(!l.cfg.desktop.screenshot_confirm),
            },
            KeyCell {
                name: "desktop.input_rate_limit_ms",
                line: "[desktop]\ninput_rate_limit_ms = 250\n",
                check: |l| assert_eq!(l.cfg.desktop.input_rate_limit_ms, 250),
            },
            KeyCell {
                name: "desktop.max_input_actions_per_minute",
                line: "[desktop]\nmax_input_actions_per_minute = 7\n",
                check: |l| assert_eq!(l.cfg.desktop.max_input_actions_per_minute, 7),
            },
            KeyCell {
                name: "desktop.screenshot_dir",
                line: "[desktop]\nscreenshot_dir = \"/tmp/x0ne-matrix-shots\"\n",
                check: |l| assert_eq!(l.cfg.desktop.screenshot_dir, "/tmp/x0ne-matrix-shots"),
            },
            // ── [composites] (config.rs:433) ─────────────────────────────────
            KeyCell {
                name: "composites.max_depth",
                line: "[composites]\nmax_depth = 5\n",
                check: |l| assert_eq!(l.cfg.composites.max_depth, 5),
            },
            KeyCell {
                name: "composites.max_steps",
                line: "[composites]\nmax_steps = 9\n",
                check: |l| assert_eq!(l.cfg.composites.max_steps, 9),
            },
            KeyCell {
                name: "composites.primitives_dir",
                line: "[composites]\nprimitives_dir = \"/tmp/x0ne-matrix-primitives\"\n",
                check: |l| {
                    assert_eq!(
                        l.cfg.composites.primitives_dir,
                        "/tmp/x0ne-matrix-primitives"
                    )
                },
            },
            // ── [sandbox] (config.rs:463) ────────────────────────────────────
            KeyCell {
                name: "sandbox.default_backend",
                line: "[sandbox]\ndefault_backend = \"incus\"\n",
                check: |l| assert_eq!(l.cfg.sandbox.default_backend, "incus"),
            },
            // ── [sandbox.incus] (config.rs:485) ──────────────────────────────
            KeyCell {
                name: "sandbox.incus.default_remote",
                line: "[sandbox.incus]\ndefault_remote = \"matrix-remote\"\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_remote, "matrix-remote"),
            },
            KeyCell {
                name: "sandbox.incus.allowed_images",
                line: "[sandbox.incus]\nallowed_images = [\"images:debian/13\"]\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.allowed_images, vec!["images:debian/13"]),
            },
            KeyCell {
                name: "sandbox.incus.default_memory_mb",
                line: "[sandbox.incus]\ndefault_memory_mb = 333\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_memory_mb, 333),
            },
            KeyCell {
                name: "sandbox.incus.max_memory_mb",
                line: "[sandbox.incus]\nmax_memory_mb = 8192\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.max_memory_mb, 8192),
            },
            KeyCell {
                name: "sandbox.incus.default_cpu_limit",
                line: "[sandbox.incus]\ndefault_cpu_limit = 3\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_cpu_limit, 3),
            },
            KeyCell {
                name: "sandbox.incus.default_disk_mb",
                line: "[sandbox.incus]\ndefault_disk_mb = 4096\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_disk_mb, 4096),
            },
            KeyCell {
                name: "sandbox.incus.default_timeout_secs",
                line: "[sandbox.incus]\ndefault_timeout_secs = 77\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_timeout_secs, 77),
            },
            KeyCell {
                name: "sandbox.incus.max_timeout_secs",
                line: "[sandbox.incus]\nmax_timeout_secs = 999\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.max_timeout_secs, 999),
            },
            KeyCell {
                name: "sandbox.incus.max_parallel_instances",
                line: "[sandbox.incus]\nmax_parallel_instances = 6\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.max_parallel_instances, 6),
            },
            KeyCell {
                name: "sandbox.incus.security_profile",
                line: "[sandbox.incus]\nsecurity_profile = \"unconfined\"\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.security_profile, "unconfined"),
            },
            // The compiled default is `true`: the cell writes `false`, the only
            // value that distinguishes « the key was read » from « the key was
            // lost ».
            KeyCell {
                name: "sandbox.incus.no_new_privileges",
                line: "[sandbox.incus]\nno_new_privileges = false\n",
                check: |l| assert!(!l.cfg.sandbox.incus.no_new_privileges),
            },
            KeyCell {
                name: "sandbox.incus.default_network",
                line: "[sandbox.incus]\ndefault_network = \"bridge\"\n",
                check: |l| assert_eq!(l.cfg.sandbox.incus.default_network, "bridge"),
            },
            // ── [sandbox.podman] (config.rs:537) ─────────────────────────────
            KeyCell {
                name: "sandbox.podman.allowed_images",
                line: "[sandbox.podman]\nallowed_images = [\"docker.io/library/alpine:3\"]\n",
                check: |l| {
                    assert_eq!(
                        l.cfg.sandbox.podman.allowed_images,
                        vec!["docker.io/library/alpine:3"]
                    )
                },
            },
            KeyCell {
                name: "sandbox.podman.default_memory_mb",
                line: "[sandbox.podman]\ndefault_memory_mb = 384\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.default_memory_mb, 384),
            },
            KeyCell {
                name: "sandbox.podman.max_memory_mb",
                line: "[sandbox.podman]\nmax_memory_mb = 3072\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.max_memory_mb, 3072),
            },
            KeyCell {
                name: "sandbox.podman.default_cpu_limit",
                line: "[sandbox.podman]\ndefault_cpu_limit = 2\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.default_cpu_limit, 2),
            },
            KeyCell {
                name: "sandbox.podman.max_cpu_limit",
                line: "[sandbox.podman]\nmax_cpu_limit = 5\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.max_cpu_limit, 5),
            },
            KeyCell {
                name: "sandbox.podman.default_timeout_secs",
                line: "[sandbox.podman]\ndefault_timeout_secs = 42\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.default_timeout_secs, 42),
            },
            KeyCell {
                name: "sandbox.podman.max_timeout_secs",
                line: "[sandbox.podman]\nmax_timeout_secs = 555\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.max_timeout_secs, 555),
            },
            KeyCell {
                name: "sandbox.podman.max_parallel_instances",
                line: "[sandbox.podman]\nmax_parallel_instances = 6\n",
                check: |l| assert_eq!(l.cfg.sandbox.podman.max_parallel_instances, 6),
            },
            // `default_network` is a `bool` on the podman side and a string on
            // the incus side (`config.rs:537` versus `:485`). The podman
            // default is `false` — no network —, so the cell writes `true`.
            KeyCell {
                name: "sandbox.podman.default_network",
                line: "[sandbox.podman]\ndefault_network = true\n",
                check: |l| assert!(l.cfg.sandbox.podman.default_network),
            },
            KeyCell {
                name: "sandbox.podman.no_new_privileges",
                line: "[sandbox.podman]\nno_new_privileges = false\n",
                check: |l| assert!(!l.cfg.sandbox.podman.no_new_privileges),
            },
        ]
    }

    /// Every key of the ten sections other than `[policy]` is loaded at its
    /// value, in all three modes.
    ///
    /// A failure would mean a declared ceiling — a sandbox's memory, number
    /// of parallel containers, `no_new_privileges` — does not cross the
    /// loader in at least one service mode. These keys are not cosmetic:
    /// `no_new_privileges` and `default_network` are confinement switches,
    /// and a confinement key that gets lost silently is exactly the
    /// enclave-write or SendInput defect.
    #[test]
    fn every_section_key_survives_the_load_in_every_mode() {
        for mode in MODES {
            for cell in section_cells() {
                let l = load(&config_with_sections(mode, cell.line))
                    .unwrap_or_else(|e| panic!("{}/{}: {e:#}", mode, cell.name));
                (cell.check)(&l);
                assert_eq!(
                    l.cfg.runtime.mode, *mode,
                    "{}: the mode was lost",
                    cell.name
                );
            }
        }
    }

    /// The four keys of `[runtime.http]` (`config.rs:141`).
    ///
    /// `auth_token` is the only field of the whole schema whose value is a
    /// secret; it is here so that its disappearance shows, and the tool that
    /// will print it one day (`x0ne config inspect`, O-4) must redact it.
    fn http_cells() -> &'static [KeyCell] {
        &[
            KeyCell {
                name: "runtime.http.host",
                // 192.0.2.0/24 is TEST-NET-1 (RFC 5737): an address that routes
                // nowhere, so nothing here can listen by accident.
                line: "[runtime.http]\nhost = \"192.0.2.7\"\n",
                check: |l| {
                    assert_eq!(
                        l.cfg
                            .runtime
                            .http
                            .as_ref()
                            .expect("[runtime.http] absent")
                            .host,
                        "192.0.2.7"
                    )
                },
            },
            KeyCell {
                name: "runtime.http.port",
                line: "[runtime.http]\nport = 9191\n",
                check: |l| {
                    assert_eq!(
                        l.cfg
                            .runtime
                            .http
                            .as_ref()
                            .expect("[runtime.http] absent")
                            .port,
                        9191
                    )
                },
            },
            KeyCell {
                name: "runtime.http.auth_token",
                line: "[runtime.http]\nauth_token = \"matrix-token\"\n",
                check: |l| {
                    assert_eq!(
                        l.cfg
                            .runtime
                            .http
                            .as_ref()
                            .expect("[runtime.http] absent")
                            .auth_token
                            .as_deref(),
                        Some("matrix-token")
                    )
                },
            },
            KeyCell {
                name: "runtime.http.allowed_origins",
                line: "[runtime.http]\nallowed_origins = [\"https://example.invalid\"]\n",
                check: |l| {
                    assert_eq!(
                        l.cfg
                            .runtime
                            .http
                            .as_ref()
                            .expect("[runtime.http] absent")
                            .allowed_origins,
                        vec!["https://example.invalid"]
                    )
                },
            },
            KeyCell {
                name: "runtime.http.allowed_hosts",
                line: "[runtime.http]\nallowed_hosts = [\"runtime.example.invalid\"]\n",
                check: |l| {
                    assert_eq!(
                        l.cfg
                            .runtime
                            .http
                            .as_ref()
                            .expect("[runtime.http] absent")
                            .allowed_hosts,
                        vec!["runtime.example.invalid"]
                    )
                },
            },
        ]
    }

    /// Every `[runtime.http]` key is loaded at its value in http mode.
    ///
    /// A failure would mean the HTTP server listens differently from what
    /// the operator wrote — on another port, without their token, or with
    /// the default origin list instead of theirs.
    #[test]
    fn every_http_key_survives_the_load_in_http_mode() {
        for cell in http_cells() {
            let l = load(&config_with_sections("http", cell.line))
                .unwrap_or_else(|e| panic!("http/{}: {e:#}", cell.name));
            (cell.check)(&l);
            assert_eq!(
                l.cfg.runtime.mode, "http",
                "{}: the mode was lost",
                cell.name
            );
        }
    }

    /// `[runtime.http]` is loaded in stdio mode, and serves no purpose there.
    ///
    /// This is not a complaint, it is a bound placed on the reading of the
    /// two previous tests: the `match app_config.runtime.mode` of
    /// `main.rs:194` only builds the HTTP server on `"http"`, so the section
    /// is **inert** elsewhere — loading it opens no port. The test exists so
    /// that nobody concludes from `every_http_key_survives_the_load…` that
    /// the section is active everywhere, and so that the day a mode starts
    /// reading it, someone comes back to reread this sentence.
    #[test]
    fn the_http_section_loads_in_stdio_mode_where_nothing_reads_it() {
        let l = load(&config_with_sections(
            "stdio",
            "[runtime.http]\nport = 9191\n",
        ))
        .expect("[runtime.http] must stay loadable outside http mode");
        assert_eq!(l.cfg.runtime.mode, "stdio");
        assert_eq!(
            l.cfg
                .runtime
                .http
                .as_ref()
                .expect("the section is loaded, even inert")
                .port,
            9191,
            "the section is parsed: the mode selector ignores it, \
             the loader does not drop it"
        );
    }

    /// The matrix skeleton declares NONE of the values it requires.
    ///
    /// SECURITY: this is the guard that keeps this whole family from becoming
    /// a tautological test. Every cell asserts « the written value is the
    /// obtained value »; if the written value were already the compiled
    /// default, the assertion would pass with a loader that throws the file
    /// away. This test loads the skeleton ALONE — no section added — and
    /// requires every `check` to **fail** on it. A `check` that passes on the
    /// bare skeleton is a `check` that measures nothing.
    ///
    /// It is also the only place in the module where a `catch_unwind` is
    /// justified: we want an assertion's failure as data, not as a verdict.
    #[test]
    fn no_cell_asserts_a_value_the_bare_skeleton_already_has() {
        let bare = load(&policy_toml("stdio", "")).expect("the v0.3 skeleton must load");
        for cell in policy_cells()
            .iter()
            .chain(section_cells())
            .chain(http_cells())
        {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (cell.check)(&bare)));
            assert!(
                outcome.is_err(),
                "{}: this cell's expectation already holds without the file — \
                 it would pass even if the loader ignored the section",
                cell.name
            );
        }
    }

    /// The guard above must be able to fail, and that must be shown.
    ///
    /// An anti-tautology guard that was itself tautological would be
    /// worthless: if `catch_unwind` returned `Err` no matter what — a
    /// miswired `check`, a badly built `Loaded` —,
    /// `no_cell_asserts_a_value_the_bare_skeleton_already_has` would pass on
    /// any table and would never have caught anything.
    ///
    /// The rule of the week forbids mutations, including one-off ones.
    /// This test is what replaces them: it carries a DELIBERATELY
    /// tautological cell and requires the guard to see it pass, next to a
    /// real cell it must see fail. Both directions, in the same test.
    ///
    /// This is not a hypothesis: the first draft of `section_cells`
    /// (2026-08-16) contained exactly this fault three times — 256 MB,
    /// 2048 MB and 8 parallel instances are the compiled defaults of
    /// `PodmanConfig` (`config.rs:560-574`), so three cells asserted a value
    /// that a mute loader would have returned just as well.
    #[test]
    fn the_tautology_guard_tells_a_measuring_cell_from_a_decorative_one() {
        let bare = load(&policy_toml("stdio", "")).expect("the v0.3 skeleton must load");

        // Decorative: 256 IS the compiled default of `podman.default_memory_mb`.
        let decorative = KeyCell {
            name: "decorative cell",
            line: "[sandbox.podman]\ndefault_memory_mb = 256\n",
            check: |l| assert_eq!(l.cfg.sandbox.podman.default_memory_mb, 256),
        };
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (decorative.check)(&bare)));
        assert!(
            outcome.is_ok(),
            "the decorative cell must pass on the bare skeleton: that is what \
             defines \"decorative\", and the guard catches nothing if it does not \
             see it pass"
        );

        // Measuring: the module's real cell, which declares 384.
        let measuring = section_cells()
            .iter()
            .find(|c| c.name == "sandbox.podman.default_memory_mb")
            .expect("the podman.default_memory_mb cell must exist in the table");
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (measuring.check)(&bare)));
        assert!(
            outcome.is_err(),
            "the real cell must fail on the bare skeleton, otherwise it is \
             decorative too"
        );
    }
}

#[cfg(test)]
mod actions_switch {
    use super::*;
    use crate::policy::{DenyReason, PolicyEffect};
    use serde_json::Value;
    use std::collections::HashMap;
    use x0ne_actions::{Action, ChatMessage, ContainerTask, Edit, EventType, PatchSetItem};

    /// A minimal action per type name, to SUBMIT to the engine instead of
    /// reading the map.
    ///
    /// SECURITY: reading `cfg.actions["X"]` proves nothing — it is exactly
    /// the error of the SendInput defect, where `[actions] SendInput = false` was written,
    /// readable, loaded, and without effect because `ALWAYS_ALLOW` decided
    /// before it. Only the evaluation of a real action sees that list.
    ///
    /// Every probe is a LITERAL. None is derived from a configuration field
    /// or from a constant of the tested code: a fixture computed from a
    /// mutable constant is the machine crash of 2026-08-16, and an expected
    /// value that restates the tested code is fault P2 of the spec.
    ///
    /// No probe is executed: `evaluate` classifies, reads paths and returns
    /// a verdict. The paths named here need not exist, the addresses are
    /// TEST-NET-1 (RFC 5737) and the host names are under `.invalid`
    /// (RFC 2606) — no fixture can reach anything if one day someone passed
    /// a probe to an executor.
    pub(super) fn probe_action(name: &str) -> Option<Action> {
        Some(match name {
            // ── Filesystem ───────────────────────────────────────────────────
            "ReadFile" => Action::ReadFile {
                path: "/etc/hostname".into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            },
            "ReadFiles" => Action::ReadFiles {
                paths: vec!["/etc/hostname".into()],
                max_bytes_per_file: None,
            },
            "WriteFile" => Action::WriteFile {
                path: "/tmp/x0ne-probe".into(),
                content: "x".into(),
            },
            "EditFile" => Action::EditFile {
                path: "/tmp/x0ne-probe".into(),
                edits: vec![Edit {
                    old: "a".into(),
                    new: "b".into(),
                    replace_all: None,
                }],
                create_if_missing: None,
            },
            "ApplyPatch" => Action::ApplyPatch {
                path: "/tmp/x0ne-probe".into(),
                patch: "".into(),
                reverse: None,
            },
            "StatFiles" => Action::StatFiles {
                paths: vec!["/etc/hostname".into()],
                hash: None,
            },
            "PatchSet" => Action::PatchSet {
                patches: vec![PatchSetItem {
                    path: "/tmp/x0ne-probe".into(),
                    patch: "".into(),
                    reverse: None,
                }],
                stop_on_error: None,
            },
            "SearchAndRead" => Action::SearchAndRead {
                root: "/tmp".into(),
                pattern: "x".into(),
                patterns: vec![],
                file_glob: None,
                context_lines: None,
                limit: None,
            },
            "SafeTest" => Action::SafeTest {
                kind: "unit".into(),
                scope: None,
                path: None,
                timeout_secs: None,
                filter: None,
            },
            "MoveToTrash" => Action::MoveToTrash {
                path: "/tmp/x0ne-probe".into(),
            },
            "SearchCode" => Action::SearchCode {
                root: "/tmp".into(),
                pattern: "x".into(),
                patterns: vec![],
                file_glob: None,
                limit: None,
            },
            // ── Network ──────────────────────────────────────────────────────
            "FetchUrl" => Action::FetchUrl {
                url: "http://192.0.2.7/".into(),
                headers: None,
                format: None,
                timeout: None,
                offset: None,
            },
            "WebSearch" => Action::WebSearch {
                query: "x".into(),
                limit: 1,
            },
            "HttpRequest" => Action::HttpRequest {
                url: "http://192.0.2.7/".into(),
                method: "GET".into(),
                headers: HashMap::new(),
                body: None,
                timeout_secs: None,
            },
            // ── Processus ────────────────────────────────────────────────────
            "RunCommand" => Action::RunCommand {
                command: "echo".into(),
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                timeout_secs: None,
                stream: None,
            },
            "Listen" => Action::Listen {
                command: "echo".into(),
                args: vec![],
                filter: None,
                window_ms: None,
                max_lines: None,
                persistent: false,
            },
            "SshExec" => Action::SshExec {
                host: "192.0.2.7".into(),
                command: "echo".into(),
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                timeout_secs: None,
                ssh_options: vec![],
            },
            "CloudCost" => Action::CloudCost {
                provider: "gcp".into(),
                project: None,
            },
            "GpuStatus" => Action::GpuStatus { host: None },
            "RunInSandbox" => Action::RunInSandbox {
                image: "docker.io/library/alpine:3".into(),
                command: vec![],
                files: HashMap::new(),
                env: HashMap::new(),
                timeout_secs: None,
                network: None,
                memory_mb: None,
                cpu_limit: None,
                collect_files: vec![],
            },
            "RunContainerGroup" => Action::RunContainerGroup {
                tasks: vec![ContainerTask {
                    id: "t".into(),
                    image: "docker.io/library/alpine:3".into(),
                    command: vec![],
                    files: HashMap::new(),
                    env: HashMap::new(),
                }],
                network: None,
                memory_mb: None,
                timeout_secs: None,
                max_parallel: None,
            },
            "ReadLogs" => Action::ReadLogs {
                path: "/var/log/x0ne-probe.log".into(),
                lines: None,
            },
            // ── Humain ───────────────────────────────────────────────────────
            "AskHuman" => Action::AskHuman {
                question: "?".into(),
            },
            "Confirm" => Action::Confirm {
                message: "?".into(),
            },
            "AskMultipleChoice" => Action::AskMultipleChoice {
                question: "?".into(),
                choices: vec!["a".into()],
                allow_skip: false,
                allow_other: false,
            },
            // ── Composites ───────────────────────────────────────────────────
            //
            // The children are EMPTY on purpose. Invariant 4 (« a composite
            // is worth what it contains ») is measured elsewhere; here, a
            // refused child would return `PipelineStepDenied` and would mask
            // the only thing this matrix measures — the composite's own
            // switch, consulted BEFORE the descent (`policy.rs:544` then
            // `:562`).
            "Pipeline" => Action::Pipeline {
                steps: vec![],
                stop_on_error: true,
                snapshot_before: None,
                parallel: false,
            },
            "Try" => Action::Try {
                actions: vec![],
                rollback_on_fail: true,
                label: None,
                keep_snapshot: false,
            },
            "SafeChange" => Action::SafeChange {
                steps: vec![],
                tests: vec![],
                rollback_on_fail: true,
                label: None,
                keep_snapshot: false,
            },
            "If" => Action::If {
                check: "exists".into(),
                path: Some("/etc/hostname".into()),
                value: None,
                host: None,
                port: None,
                command: None,
                args: None,
                name: None,
                actual: None,
                expected: None,
                then: vec![],
                else_branch: vec![],
            },
            "RenderTemplate" => Action::RenderTemplate {
                template: "x".into(),
                max_depth: None,
                strict: None,
            },
            "Assert" => Action::Assert {
                check: "exists".into(),
                path: Some("/etc/hostname".into()),
                value: None,
                host: None,
                port: None,
                command: None,
                args: None,
                name: None,
                actual: None,
                expected: None,
            },
            // ── Snapshots ────────────────────────────────────────────────────
            "CreateSnapshot" => Action::CreateSnapshot {
                name: "probe".into(),
                paths: vec!["/tmp/x0ne-probe".into()],
            },
            "RestoreSnapshot" => Action::RestoreSnapshot {
                name: "probe".into(),
            },
            "ListSnapshots" => Action::ListSnapshots,
            "DeleteSnapshot" => Action::DeleteSnapshot {
                name: "probe".into(),
            },
            // ── Events and handles ───────────────────────────────────────────
            //
            // `ProcessExited` rather than a file event: the two file variants
            // go through a path check (`policy.rs:768`) that would refuse for
            // a reason unrelated to the switch.
            "Subscribe" => Action::Subscribe {
                event_type: EventType::ProcessExited { pid: 1 },
                config: Value::Null,
            },
            "CheckHandle" => Action::CheckHandle { handle: "h".into() },
            "StopHandle" => Action::StopHandle { handle: "h".into() },
            "ListHandles" => Action::ListHandles,
            "SendInput" => Action::SendInput {
                handle: "h".into(),
                input: "x".into(),
            },
            // ── Terminals ────────────────────────────────────────────────────
            "TermOpen" => Action::TermOpen {
                name: "probe".into(),
                command: "echo".into(),
                args: vec![],
                cwd: None,
                cols: None,
                rows: None,
            },
            "TermSend" => Action::TermSend {
                name: "probe".into(),
                text: Some("x".into()),
                keys: vec![],
            },
            "TermRead" => Action::TermRead {
                name: "probe".into(),
                history: None,
                diff: false,
            },
            "TermWait" => Action::TermWait {
                name: "probe".into(),
                pattern: Some("x".into()),
                stable_ms: None,
                exit: false,
                timeout_ms: None,
                diff: true,
            },
            "TermResize" => Action::TermResize {
                name: "probe".into(),
                cols: 80,
                rows: 24,
            },
            "TermSnapshot" => Action::TermSnapshot {
                name: "probe".into(),
                ansi: false,
            },
            "TermList" => Action::TermList,
            "TermClose" => Action::TermClose {
                name: "probe".into(),
            },
            "WatchAndReact" => Action::WatchAndReact {
                path: "/tmp".into(),
                pattern: None,
                action: Box::new(Action::ListActions),
                debounce_ms: None,
                max_triggers: None,
                recursive: true,
            },
            // ── Introspection ────────────────────────────────────────────────
            "PolicyCheck" => Action::PolicyCheck {
                action: Value::Null,
            },
            "FileInfo" => Action::FileInfo {
                path: "/etc/hostname".into(),
            },
            "Checksum" => Action::Checksum {
                path: "/etc/hostname".into(),
                algorithm: None,
            },
            "ListProcesses" => Action::ListProcesses { filter: None },
            "PortCheck" => Action::PortCheck {
                host: "192.0.2.7".into(),
                port: 9,
                timeout_secs: None,
            },
            "DnsLookup" => Action::DnsLookup {
                hostname: "probe.invalid".into(),
            },
            // ── Memory ───────────────────────────────────────────────────────
            "Remember" => Action::Remember {
                key: "k".into(),
                value: Value::Null,
                namespace: None,
            },
            "Recall" => Action::Recall {
                key: "k".into(),
                namespace: None,
            },
            "Forget" => Action::Forget {
                key: "k".into(),
                namespace: None,
            },
            "ListMemory" => Action::ListMemory { namespace: None },
            // ── Divers ───────────────────────────────────────────────────────
            "Diff" => Action::Diff {
                a: "a".into(),
                b: "b".into(),
                files: false,
                context_lines: None,
            },
            "Simulate" => Action::Simulate {
                target: Box::new(Action::ListActions),
                explain: false,
            },
            "Validate" => Action::Validate {
                value: Value::Null,
                check: "is_string".into(),
                expect: None,
            },
            "RandomBytes" => Action::RandomBytes {
                count: 1,
                encoding: None,
            },
            // ── Bureau ───────────────────────────────────────────────────────
            "Screenshot" => Action::Screenshot {
                output: None,
                region: None,
            },
            "GetWindowTree" => Action::GetWindowTree,
            "GetFocusedWindow" => Action::GetFocusedWindow,
            "MoveMouse" => Action::MoveMouse { x: 0, y: 0 },
            "Click" => Action::Click {
                x: None,
                y: None,
                button: None,
            },
            "DoubleClick" => Action::DoubleClick { x: None, y: None },
            "Scroll" => Action::Scroll {
                direction: "down".into(),
                amount: None,
            },
            "TypeText" => Action::TypeText { text: "x".into() },
            "PressKey" => Action::PressKey { key: "a".into() },
            "PressKeyCombination" => Action::PressKeyCombination {
                keys: vec!["ctrl".into(), "a".into()],
            },
            "FocusWindow" => Action::FocusWindow {
                match_str: "x".into(),
            },
            // « TEST » in the title: a notification outlives its context,
            // and a fixture must never be readable as a real alert (global
            // rule, incident of 2026-08-09). This one is never emitted —
            // `evaluate` does not execute it.
            "SendNotification" => Action::SendNotification {
                title: "TEST".into(),
                body: "matrix probe".into(),
                urgency: None,
            },
            "GetClipboard" => Action::GetClipboard,
            "SetClipboard" => Action::SetClipboard {
                content: "x".into(),
            },
            // ── Primitives and functions ──────────────────────────────────────
            "DefinePrimitive" => Action::DefinePrimitive {
                name: "p".into(),
                description: None,
                params: HashMap::new(),
                steps: vec![],
                ephemeral: false,
            },
            "CallPrimitive" => Action::CallPrimitive {
                name: "p".into(),
                args: HashMap::new(),
            },
            "ListCustomPrimitives" => Action::ListCustomPrimitives,
            "DeletePrimitive" => Action::DeletePrimitive { name: "p".into() },
            "DefineFunction" => Action::DefineFunction {
                name: "f".into(),
                description: None,
                tier: None,
                source: "".into(),
                params: vec![],
                returns: None,
            },
            "CallFunction" => Action::CallFunction {
                name: "f".into(),
                args: vec![],
            },
            "ListFunctions" => Action::ListFunctions,
            "DeleteFunction" => Action::DeleteFunction { name: "f".into() },
            "PreviewProgram" => Action::PreviewProgram { source: "".into() },
            "RunProgram" => Action::RunProgram { source: "".into() },
            // ── Models ───────────────────────────────────────────────────────
            "LlmCall" => Action::LlmCall {
                endpoint: "http://192.0.2.7/".into(),
                model: "m".into(),
                prompt: "p".into(),
                system: None,
                temperature: None,
                max_tokens: None,
                auth: None,
            },
            "OllamaApi" => Action::OllamaApi {
                endpoint: "http://192.0.2.7/".into(),
                operation: "list".into(),
                model: None,
                modelfile: None,
                auth: None,
            },
            "DatasetGenerate" => Action::DatasetGenerate {
                endpoint: "http://192.0.2.7/".into(),
                model: "m".into(),
                topic: "t".into(),
                count: 1,
                output: "/tmp/x0ne-probe".into(),
                style: None,
            },
            "DocumentExtract" => Action::DocumentExtract {
                input: "/tmp/x0ne-probe".into(),
                output: "/tmp/x0ne-probe.out".into(),
                format: "raw".into(),
                llm_endpoint: None,
                llm_model: None,
                chunk_size: None,
            },
            "OllamaChat" => Action::OllamaChat {
                endpoint: "http://192.0.2.7/".into(),
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "x".into(),
                    images: vec![],
                }],
                temperature: None,
                max_tokens: None,
                auth: None,
            },
            // ── Artefacts ────────────────────────────────────────────────────
            "ArtifactCreate" => Action::ArtifactCreate {
                name: None,
                kind: None,
                persistent: None,
                session_id: None,
            },
            "ArtifactList" => Action::ArtifactList,
            "ArtifactServe" => Action::ArtifactServe {
                id: "a".into(),
                command: "echo".into(),
                port: None,
                env: HashMap::new(),
            },
            "ArtifactSave" => Action::ArtifactSave {
                id: "a".into(),
                name: None,
            },
            "ArtifactStop" => Action::ArtifactStop { id: "a".into() },
            "ArtifactDestroy" => Action::ArtifactDestroy {
                id: "a".into(),
                force: None,
            },
            // ── Vocabulaire ──────────────────────────────────────────────────
            "ListActions" => Action::ListActions,
            "DescribeAction" => Action::DescribeAction {
                name: "ReadFile".into(),
            },
            "PaintBadge" => Action::PaintBadge {
                payload: Value::Null,
            },
            _ => return None,
        })
    }

    /// The actions for which no static probe exists.
    ///
    /// SECURITY: this list is an admission, not an exemption. A test freezes
    /// it so that a new action does not get added to it silently — otherwise
    /// « all actions covered » would become false without anything failing.
    /// It is EMPTY as of 2026-08-16: the 92 variants of `Action` are built
    /// from literals, none requires live state.
    const NO_PROBE: &[&str] = &[];

    /// The actions that `ALWAYS_ALLOW` (`policy.rs:301`) exempts from the
    /// ENTIRE gate — per-action switch included.
    ///
    /// SECURITY: this list is the test's working copy, and it is there to be
    /// contradicted. `the_total_dispense_covers_exactly_the_actions_it_names`
    /// pins it in BOTH directions: an extra entry on the production side
    /// fails the « refused » branch, a missing entry fails the « allowed »
    /// branch. This is the net the SendInput defect lacked, where `SendInput` stayed exempted
    /// for months with `[actions] SendInput = false` written in the file.
    const DISPENSED: &[&str] = &[
        "PolicyCheck",
        "Simulate",
        "CheckHandle",
        "ListHandles",
        "ListCustomPrimitives",
        "ListFunctions",
    ];

    /// The v0.3 skeleton, plus the three things this matrix varies:
    /// `default_effect`, `action_defaults`, and the switch of ONE action.
    ///
    /// `[desktop] enabled = true` is not an ornament. The desktop gate
    /// (`policy.rs:524`) decides BEFORE the per-action switch: without it,
    /// the fourteen desktop actions would return `DesktopDisabled` and the
    /// fourteen cells would measure that gate instead of their own.
    fn policy_with_switch(
        default_effect: &str,
        action_defaults: &str,
        name: &str,
        enabled: bool,
    ) -> String {
        format!(
            "{}action_defaults = \"{action_defaults}\"\n\
             \n[desktop]\nenabled = true\n\
             \n[actions]\n{name} = {enabled}\n",
            policy_toml_overriding("stdio", &format!("default_effect = \"{default_effect}\""))
        )
    }

    /// Every action in the vocabulary has a probe, or a written reason.
    ///
    /// A failure would mean a new action entered the vocabulary without
    /// this module's matrices seeing it — and they would keep announcing
    /// a total coverage they no longer have.
    #[test]
    fn every_action_type_has_a_probe_or_a_written_reason() {
        let mut missing = Vec::new();
        for name in x0ne_actions::all_type_names() {
            if probe_action(name).is_none() && !NO_PROBE.contains(name) {
                missing.push(*name);
            }
        }
        assert!(
            missing.is_empty(),
            "actions with neither probe nor justification: {missing:?}"
        );
    }

    /// A probe answers to the name it was asked for.
    ///
    /// A failure would mean a cell measures the switch of ANOTHER action
    /// than the one it names: `probe_action("ArtifactSave")` returning an
    /// `ArtifactStop` would make both matrices pass while never testing
    /// `ArtifactSave`. Ninety-two hand-written arms call this check, and it
    /// cannot be done by rereading.
    #[test]
    fn every_probe_answers_to_the_name_it_was_asked_for() {
        for name in x0ne_actions::all_type_names() {
            if let Some(action) = probe_action(name) {
                assert_eq!(
                    action.type_name(),
                    *name,
                    "the probe for {name} returns a {}",
                    action.type_name()
                );
            }
        }
    }

    /// A `false` written in `[actions]` refuses the action, in all four
    /// combinations of `default_effect` and `action_defaults` — and the
    /// reason for the refusal is the switch itself.
    ///
    /// SECURITY: a failure would mean an operator who writes `X = false` gets
    /// a runtime that executes X. That is the SendInput defect, found again by another
    /// path. The REASON is what proves it, not the refusal: under
    /// `default_effect = "deny"`, an engine that had thrown away the
    /// `[actions]` table would ALSO refuse, through `DefaultDeny`. Only
    /// `ActionDisabled` says the written line was read.
    #[test]
    fn a_disabled_action_is_denied_by_its_own_switch_under_every_combination() {
        let mut checked = 0usize;
        for default_effect in ["allow", "deny"] {
            for action_defaults in ["merge", "replace"] {
                for name in x0ne_actions::all_type_names() {
                    let Some(action) = probe_action(name) else {
                        continue;
                    };
                    if DISPENSED.contains(name) {
                        continue;
                    }
                    let toml = policy_with_switch(default_effect, action_defaults, name, false);
                    let l = load(&toml).unwrap_or_else(|e| panic!("{name}: {e:#}"));
                    let effect = l.engine.evaluate(&action);
                    assert!(
                        matches!(effect, PolicyEffect::Deny(DenyReason::ActionDisabled)),
                        "{name} disabled under default_effect={default_effect} \
                         action_defaults={action_defaults}: got {effect:?}"
                    );
                    checked += 1;
                }
            }
        }
        // Literal, and it matters: an empty loop passes. If
        // `all_type_names()` returned a truncated list — the very defect the
        // generated catalogue exists to prevent —, this matrix would assert
        // nothing at all and say so in green.
        assert_eq!(
            checked,
            4 * 95,
            "95 non-exempt actions × 4 combinations expected; got {checked}"
        );
    }

    /// A `true` written in `[actions]` prevents the refusal by switch, in
    /// the same four combinations.
    ///
    /// SECURITY: this is the anti-tautology guard of this family, and without
    /// it the previous matrix would measure almost nothing. The compiled
    /// default of `[actions]` is `false` for everything that writes, executes,
    /// destroys, reaches the network or drives the desktop (`config.rs:944`)
    /// — sixty-odd actions. For those, requiring `ActionDisabled` on
    /// `X = false` would ALSO pass with a loader that throws the `[actions]`
    /// table away entirely. The two halves cannot pass together under a mute
    /// loader: it is the pair that proves the written value decides, not the
    /// compiled default.
    ///
    /// The assertion is narrow on purpose — « not refused BY THE SWITCH ».
    /// An action can stay refused for a reason that is not its own:
    /// `allowed_read_paths` is empty, `allowed_commands` is empty.
    /// Requiring `Allow` would measure confinement, not the switch.
    #[test]
    fn an_enabled_action_is_never_denied_by_its_switch_under_every_combination() {
        let mut checked = 0usize;
        for default_effect in ["allow", "deny"] {
            for action_defaults in ["merge", "replace"] {
                for name in x0ne_actions::all_type_names() {
                    let Some(action) = probe_action(name) else {
                        continue;
                    };
                    if DISPENSED.contains(name) {
                        continue;
                    }
                    let toml = policy_with_switch(default_effect, action_defaults, name, true);
                    let l = load(&toml).unwrap_or_else(|e| panic!("{name}: {e:#}"));
                    let effect = l.engine.evaluate(&action);
                    assert!(
                        !matches!(effect, PolicyEffect::Deny(DenyReason::ActionDisabled)),
                        "{name} enabled under default_effect={default_effect} \
                         action_defaults={action_defaults} is refused by its \
                         switch: got {effect:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(
            checked,
            4 * 95,
            "95 non-exempt actions × 4 combinations expected; got {checked}"
        );
    }

    /// Neither half of the pair is true of the bare skeleton.
    ///
    /// SECURITY: this is the proof, permanent and mutation-free, that the two
    /// matrices above measure the FILE. The argument is structural — an
    /// action's compiled default is a single value, so it cannot satisfy both
    /// « `false` refuses » and « `true` does not refuse » — and this test
    /// makes it measurable instead of leaving it in a comment: it loads the
    /// skeleton WITHOUT any `[actions]` table and requires each half to fail
    /// there on an action chosen for that purpose.
    ///
    /// `ReadFile` carries `true` as compiled default (class `ReadOnly`,
    /// `config.rs:944`): under the bare skeleton it is NOT `ActionDisabled`,
    /// so the expected value of the « disabled » matrix cannot come from the
    /// default. `RunCommand` carries `false` (class `ProcessSpawn`): under the
    /// bare skeleton it IS `ActionDisabled`, so the expected value of the
    /// « enabled » matrix cannot come from it either.
    ///
    /// A failure would mean the compiled defaults moved under both matrices,
    /// and that one half of the pair has become decorative — the « config
    /// matrix » form of dead scaffolding, the one Task 2 caught three times
    /// in a single draft.
    #[test]
    fn neither_half_of_the_pair_is_already_true_of_the_bare_skeleton() {
        let bare = load(&policy_toml("stdio", "")).expect("the v0.3 skeleton must load");

        let read = bare
            .engine
            .evaluate(&probe_action("ReadFile").expect("known probe"));
        assert!(
            !matches!(read, PolicyEffect::Deny(DenyReason::ActionDisabled)),
            "without an [actions] table, ReadFile must not be refused by its \
             switch — otherwise the \"disabled\" matrix expectation would \
             already hold for the compiled default; got {read:?}"
        );

        let run = bare
            .engine
            .evaluate(&probe_action("RunCommand").expect("known probe"));
        assert!(
            matches!(run, PolicyEffect::Deny(DenyReason::ActionDisabled)),
            "without an [actions] table, RunCommand must be refused by its \
             switch — otherwise the \"enabled\" matrix expectation would \
             already hold for the compiled default; got {run:?}"
        );
    }

    /// The total exemption covers exactly the actions named here, and not
    /// one more.
    ///
    /// SECURITY: `ALWAYS_ALLOW` short-circuits the entire gate
    /// (`policy.rs:522`) — switch, `readonly`/`confirm` modes, paths.
    /// A failure in the « allowed while expected refused » direction says an
    /// action entered it without anyone writing it here: that is the SendInput defect, where
    /// `SendInput` wrote to the stdin of a live process while exempted, and
    /// where `[actions] SendInput = false` changed nothing. In the other
    /// direction, it says an action left it — less serious, never silent.
    #[test]
    fn the_total_dispense_covers_exactly_the_actions_it_names() {
        let mut seen_dispensed = Vec::new();
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            let toml = policy_with_switch("deny", "merge", name, false);
            let l = load(&toml).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let effect = l.engine.evaluate(&action);
            let dispensed = matches!(effect, PolicyEffect::Allow);
            assert_eq!(
                dispensed,
                DISPENSED.contains(name),
                "{name}: `X = false` returns {effect:?}, but DISPENSED says {} — \
                 this test's list and `ALWAYS_ALLOW` have diverged",
                DISPENSED.contains(name)
            );
            if dispensed {
                seen_dispensed.push(*name);
            }
        }
        assert_eq!(
            seen_dispensed.len(),
            DISPENSED.len(),
            "exempt actions seen: {seen_dispensed:?}"
        );
    }

    /// `action_defaults = "replace"` is a strict mode: what the file does not
    /// enumerate does not exist, and `default_effect` decides in its place.
    ///
    /// SECURITY: measured by EVALUATION, not by reading `cfg.actions`. Under
    /// `merge`, an action not listed carries its compiled default and gets
    /// refused by its switch (`ActionDisabled`); under `replace`, it is no
    /// longer in the map at all and goes through the switch to land on
    /// `default_effect` (`DefaultDeny`). The two refusals look alike and do
    /// not mean the same thing: it is the difference that proves
    /// `action_defaults` is READ by the path that decides, and not merely
    /// loaded.
    ///
    /// A failure would say the « strict allowlist » mode lets the compiled
    /// defaults in — so it is not strict.
    #[test]
    fn replace_mode_drops_every_action_the_file_does_not_list() {
        let write = probe_action("WriteFile").expect("known probe");

        let strict = load(&policy_with_switch("deny", "replace", "ReadFile", true))
            .expect("strict mode must load");
        assert!(
            matches!(
                strict.engine.evaluate(&write),
                PolicyEffect::Deny(DenyReason::DefaultDeny)
            ),
            "under replace, an unlisted action must fall to default_effect, \
             got {:?}",
            strict.engine.evaluate(&write)
        );

        let merged = load(&policy_with_switch("deny", "merge", "ReadFile", true))
            .expect("merged mode must load");
        assert!(
            matches!(
                merged.engine.evaluate(&write),
                PolicyEffect::Deny(DenyReason::ActionDisabled)
            ),
            "under merge, the same unlisted action must carry its compiled \
             default and be refused BY ITS SWITCH, got {:?}",
            merged.engine.evaluate(&write)
        );
    }

    /// Under `replace`, an action not enumerated is decided by
    /// `default_effect` ALONE — including when it is `"allow"`.
    ///
    /// SECURITY: this test documents an interaction, it does not fix it.
    /// `[policy] action_defaults = "replace"` with `default_effect = "allow"`
    /// is an allowlist that lets through everything it does not enumerate:
    /// the switch no longer objects (the map does not contain the key) and
    /// `default_effect` opens. Only path confinement still stops the write —
    /// and it does not cover actions without a path. This is the reason
    /// `config.rs:684` refuses an unknown `default_effect`: a typo reads as
    /// « not deny » and produces exactly this posture.
    ///
    /// A failure would mean the pair changed direction without anyone
    /// rereading this sentence.
    #[test]
    fn under_replace_an_unlisted_action_is_decided_by_default_effect_alone() {
        let write = probe_action("WriteFile").expect("known probe");
        let l = load(&policy_with_switch("allow", "replace", "ReadFile", true))
            .expect("the posture must load");
        let effect = l.engine.evaluate(&write);
        assert!(
            matches!(
                effect,
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "unlisted WriteFile must PASS the switch and be stopped \
             only by path confinement, got {effect:?}"
        );
    }
}

#[cfg(test)]
mod paths {
    use super::*;
    use crate::policy::{DenyReason, PathAccess, PolicyEffect};
    use x0ne_actions::Action;

    fn read(path: &str) -> Action {
        Action::ReadFile {
            path: path.into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }
    }
    fn write(path: &str) -> Action {
        Action::WriteFile {
            path: path.into(),
            content: "x".into(),
        }
    }

    /// The v0.3 skeleton, its path lines replaced, and the two probe
    /// switches turned on.
    ///
    /// Measured red on 2026-08-16: without `[actions] WriteFile = true`, a
    /// write returns `Deny(ActionDisabled)` — `WriteFile` is OFF in the
    /// compiled defaults — and the cell measures the switch, never
    /// confinement. This is Task 3's anti-tautology guard applied to paths:
    /// a refusal proves nothing until every gate located BEFORE the one we
    /// think we measure has been ruled out (`policy.rs:522-544`).
    fn path_policy(lines: &[&str]) -> String {
        format!(
            "{}\n[actions]\nReadFile = true\nWriteFile = true\n",
            policy_toml_overriding_lines("stdio", lines)
        )
    }

    /// A policy that grants `root` for READING only.
    ///
    /// The write list stays the skeleton's — empty, hence deny-all —, which
    /// is exactly the asymmetry several cells measure.
    fn read_root(root: &str) -> String {
        path_policy(&[&format!("allowed_read_paths = [\"{root}\"]")])
    }

    /// A temporary directory, its canonical form, and its string.
    ///
    /// `/tmp` is a symbolic link on some machines, and the engine compares
    /// CANONICAL paths (`policy.rs:694`). A root written in its unresolved
    /// form and an action written in the other would never meet, and the
    /// cell would measure the test's plumbing.
    fn canonical_tempdir() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let canonical = std::fs::canonicalize(dir.path())
            .expect("a freshly created directory canonicalizes")
            .display()
            .to_string();
        (dir, canonical)
    }

    /// This module's frame allows what its cells measure as refused.
    ///
    /// SECURITY: this is the anti-tautology guard. All cells below are
    /// refusals, and the bare skeleton already refuses EVERYTHING (empty root
    /// list = deny-all, `confine.rs:241`). Without this cell, the other eight
    /// would turn green with an engine that refused for any reason — action
    /// switch, `default_effect`, mode — and would never have touched a path.
    /// A failure here invalidates the whole module: it says the frame refuses
    /// on its own, so nothing in it measures confinement.
    #[test]
    fn the_frame_allows_the_very_accesses_the_other_cells_see_refused() {
        let (dir, root) = canonical_tempdir();
        std::fs::write(dir.path().join("inside.txt"), "x").unwrap();
        let l = load(&path_policy(&[
            &format!("allowed_read_paths = [\"{root}\"]"),
            &format!("allowed_write_paths = [\"{root}\"]"),
        ]))
        .expect("loads");

        for a in [
            read(&format!("{root}/inside.txt")),
            write(&format!("{root}/new.txt")),
        ] {
            assert_eq!(
                l.engine.evaluate(&a),
                PolicyEffect::Allow,
                "root granted for read AND write: {a:?} should have passed"
            );
        }
    }

    /// An empty root list refuses everything — it does not allow everything.
    ///
    /// A failure would mean a policy that enumerates no root opens the whole
    /// disk. This is the costliest possible inversion and it is undetectable
    /// by reading the file.
    ///
    /// The REASON is required, not only the refusal: under the bare skeleton
    /// a write is already refused by its switch, and `Deny(_)` would have
    /// been green without any path list being consulted.
    #[test]
    fn an_empty_root_list_denies_instead_of_allowing() {
        let l = load(&path_policy(&[])).expect("loads");
        for (a, expected) in [
            (read("/etc/hostname"), PathAccess::Read),
            (write("/tmp/x"), PathAccess::Write),
        ] {
            match l.engine.evaluate(&a) {
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { access, .. }) => {
                    assert_eq!(access, expected, "empty list: {a:?}");
                }
                other => {
                    panic!("empty list: {a:?} should have been refused by its list, got {other:?}")
                }
            }
        }
    }

    /// A path refusal names the list that would lift it, and the right one.
    ///
    /// A failure would mean a grant built from this refusal guesses between
    /// `allowed_read_paths` and `allowed_write_paths` — so either a « yes »
    /// that does not work, or, far worse, a read refusal answered with a
    /// write permission (`policy.rs:20-25`).
    #[test]
    fn a_path_refusal_names_the_one_list_that_would_lift_it() {
        let (dir, root) = canonical_tempdir();
        std::fs::write(dir.path().join("in.txt"), "x").unwrap();
        let l = load(&read_root(&root)).expect("loads");

        // The same root, granted read-only: the read passes.
        // Without this check, the write refusal below could come from the
        // root itself and not from the read/write asymmetry.
        assert_eq!(
            l.engine.evaluate(&read(&format!("{root}/in.txt"))),
            PolicyEffect::Allow,
            "the root is in allowed_read_paths: the read should have passed"
        );

        match l.engine.evaluate(&write(&format!("{root}/out.txt"))) {
            PolicyEffect::Deny(DenyReason::PathNotInAllowlist { access, .. }) => {
                assert_eq!(
                    access,
                    PathAccess::Write,
                    "a write must cite the write list"
                );
            }
            other => panic!("expected PathNotInAllowlist(Write), got {other:?}"),
        }
    }

    /// A deny pattern wins over an allowed root.
    ///
    /// A failure would mean `deny_path_patterns` — the only defence cited by
    /// SECURITY.md for `.ssh` and `.env` — can be bypassed by widening a
    /// root.
    #[test]
    fn a_deny_pattern_beats_an_allowed_root() {
        let (dir, root) = canonical_tempdir();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".ssh/id_ed25519"), "secret").unwrap();
        std::fs::write(dir.path().join("plain.txt"), "x").unwrap();

        let l = load(&path_policy(&[
            &format!("allowed_read_paths = [\"{root}\"]"),
            "deny_path_patterns = [\"**/.ssh/**\"]",
        ]))
        .expect("loads");

        // Control: the same root, a file the pattern does not target.
        assert_eq!(
            l.engine.evaluate(&read(&format!("{root}/plain.txt"))),
            PolicyEffect::Allow,
            "the pattern targets only .ssh: the rest of the root should have passed"
        );

        match l.engine.evaluate(&read(&format!("{root}/.ssh/id_ed25519"))) {
            PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. }) => {}
            other => panic!("the deny pattern should have won, got {other:?}"),
        }
    }

    /// A root that does not exist is not dropped silently.
    ///
    /// Regression of b02726c: a nonexistent root disappeared from the list,
    /// and an emptied list refuses everything — the refusal was right, the
    /// reason was wrong, and the operator looked in the wrong place.
    #[test]
    fn a_root_that_does_not_exist_is_reported_not_discarded() {
        let l = load(&path_policy(&[
            "allowed_read_paths = [\"/definitely/not/here/x0ne\"]",
        ]))
        .expect("loads");
        let warnings = l.cfg.policy_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("/definitely/not/here/x0ne")),
            "the missing root must be named in the warnings: {warnings:?}"
        );
        // And the warning is about something real: the read under this root,
        // written in black and white, is indeed refused.
        assert!(
            matches!(
                l.engine.evaluate(&read("/definitely/not/here/x0ne/x")),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "a root that does not resolve grants nothing"
        );
    }

    /// A relative root is anchored on the runtime's cwd, and it grants
    /// exactly what it contains.
    ///
    /// A failure would mean one of two things: either a relative root grants
    /// nothing (it is then dead scaffolding in a policy file — the enclave-write/SendInput
    /// pattern, in its « config » form), or it grants beyond what it names.
    /// It is also the only form of root whose meaning depends on a state
    /// external to the file: the process's current directory.
    #[test]
    fn a_relative_root_is_anchored_on_the_runtime_cwd() {
        let cwd = std::env::current_dir().expect("cwd");
        let inside = cwd.join("src").join("policy_matrix.rs");
        assert!(
            inside.exists(),
            "this cell assumes cargo runs the test from the package \
             root (cwd = {}): that is where `src` makes sense",
            cwd.display()
        );

        let l = load(&path_policy(&["allowed_read_paths = [\"src\"]"])).expect("loads");

        assert_eq!(
            l.engine.evaluate(&read(&inside.display().to_string())),
            PolicyEffect::Allow,
            "`src` must resolve to {}/src and grant what it contains",
            cwd.display()
        );
        assert!(
            matches!(
                l.engine.evaluate(&read("/etc/hostname")),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "a relative root grants nothing outside the cwd"
        );
    }

    /// A root with `~` is expanded, not taken literally.
    ///
    /// A failure would mean a `~` stays literal: no directory is named that,
    /// `normalize_roots` would drop it, and the operator who wrote
    /// `allowed_read_paths = ["~/code"]` would get a deny-all that nothing in
    /// their file accounts for. This is the field report of 2026-08-16
    /// (`confine.rs:53-66`).
    #[test]
    fn a_tilde_root_is_expanded_before_it_is_resolved() {
        let home = std::env::var("HOME").expect("HOME");
        assert_ne!(
            home.as_str(),
            "/",
            "this cell says nothing if the home is the disk root"
        );

        let l = load(&path_policy(&["allowed_read_paths = [\"~\"]"])).expect("loads");

        assert_eq!(
            l.engine.evaluate(&read(&home)),
            PolicyEffect::Allow,
            "`~` must designate {home}"
        );
        assert!(
            l.cfg.policy_warnings().iter().all(|w| !w.contains("~")),
            "`~` resolved: no warning may declare it dead — {:?}",
            l.cfg.policy_warnings()
        );
        assert!(
            matches!(
                l.engine.evaluate(&read("/etc/hostname")),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "expanding `~` does not open the disk"
        );
    }

    /// A symbolic link is judged on its target, not on its location.
    ///
    /// A failure would mean the root can be bypassed with an `ln -s`: drop a
    /// link INSIDE the granted root and read anything outside. The
    /// canonicalization in `paths_of` (`policy.rs:850`) is what prevents it
    /// AT THE GATE; `executor::confine` does it again at open time, and the
    /// two must say the same thing.
    #[test]
    fn a_symlink_out_of_the_root_is_judged_on_its_target() {
        let (dir, root) = canonical_tempdir();
        let (outside_dir, _outside) = canonical_tempdir();
        std::fs::write(outside_dir.path().join("secret"), "s").unwrap();
        std::fs::write(dir.path().join("inside.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside_dir.path().join("secret"), dir.path().join("escape"))
            .unwrap();

        let l = load(&read_root(&root)).expect("loads");

        // Control: a real file at the same place passes. The refusal that
        // follows therefore comes from the link, not from the root.
        assert_eq!(
            l.engine.evaluate(&read(&format!("{root}/inside.txt"))),
            PolicyEffect::Allow,
            "a real file inside the root should have passed"
        );
        // The link must REALLY point at the secret: a broken link would be
        // refused too, and the cell would no longer say anything about symlinks.
        let link = format!("{root}/escape");
        assert!(
            std::fs::read_to_string(&link).is_ok_and(|c| c == "s"),
            "{link} must traverse to the neighbour's secret"
        );
        assert!(
            matches!(
                l.engine.evaluate(&read(&link)),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "a link whose target leaves the root should have been refused"
        );
    }

    /// A `..` is resolved, then judged where it lands.
    ///
    /// A failure in the « allowed » direction would mean the root can be
    /// climbed out of by hand. In the other direction — a `..` that stays
    /// inside and gets refused — it would mean the syntax is filtered instead
    /// of the path being resolved: a cosmetic refusal, bypassable by a link,
    /// and one that breaks legitimate paths (`policy.rs:865-875`).
    #[test]
    fn a_dotdot_is_resolved_and_judged_where_it_lands() {
        let (dir, root) = canonical_tempdir();
        let (outside_dir, outside) = canonical_tempdir();
        std::fs::write(outside_dir.path().join("secret"), "s").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("inside.txt"), "x").unwrap();

        let l = load(&read_root(&root)).expect("loads");

        // A `..` that lands back INSIDE the root stays allowed.
        assert_eq!(
            l.engine
                .evaluate(&read(&format!("{root}/sub/../inside.txt"))),
            PolicyEffect::Allow,
            "a `..` that stays inside the root must change nothing"
        );
        // The two temporary directories are siblings: « .. » from the root
        // leads to the neighbour. The cell would be empty if the constructed
        // path designated nothing — a nonexistent path is refused too, and
        // for another reason — so it is required to exist.
        let neighbour = std::path::Path::new(&outside)
            .file_name()
            .expect("the temporary directory has a name")
            .to_string_lossy()
            .into_owned();
        let escaping = format!("{root}/../{neighbour}/secret");
        assert!(
            std::path::Path::new(&escaping).exists(),
            "{escaping} must designate the neighbour's secret, otherwise the cell measures nothing"
        );
        assert!(
            matches!(
                l.engine.evaluate(&read(&escaping)),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "a `..` climbing out of the root should have been refused"
        );
    }

    /// The root itself is inside the root.
    ///
    /// A failure would mean granting a directory does not allow designating
    /// it — the edge case of a component-wise comparison that would require
    /// one more component. The operator who grants `~/code` would see
    /// `SearchCode { root: "~/code" }` refused by the list that names it.
    #[test]
    fn the_root_itself_is_inside_the_root() {
        let (_dir, root) = canonical_tempdir();
        let l = load(&read_root(&root)).expect("loads");
        assert_eq!(
            l.engine.evaluate(&read(&root)),
            PolicyEffect::Allow,
            "the granted root must be addressable as is"
        );
    }

    /// A neighbour whose name EXTENDS the root's name is outside.
    ///
    /// A failure would mean the return of string `starts_with`
    /// (`policy.rs:684`): `/srv/rootEVIL` counted within `/srv/root`. It
    /// would then suffice to create a directory next to the granted root,
    /// with the right prefix, to be inside.
    #[test]
    fn a_sibling_whose_name_extends_the_root_is_outside_it() {
        let (parent, parent_path) = canonical_tempdir();
        std::fs::create_dir_all(parent.path().join("root")).unwrap();
        std::fs::write(parent.path().join("root/ok.txt"), "x").unwrap();
        std::fs::create_dir_all(parent.path().join("rootEVIL")).unwrap();
        std::fs::write(parent.path().join("rootEVIL/secret"), "s").unwrap();

        let l = load(&path_policy(&[&format!(
            "allowed_read_paths = [\"{parent_path}/root\"]"
        )]))
        .expect("loads");

        assert_eq!(
            l.engine
                .evaluate(&read(&format!("{parent_path}/root/ok.txt"))),
            PolicyEffect::Allow,
            "the granted root should have passed"
        );
        assert!(
            matches!(
                l.engine
                    .evaluate(&read(&format!("{parent_path}/rootEVIL/secret"))),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "`rootEVIL` is not inside `root`: the comparison must be done \
             by components, not by string prefix"
        );
    }

    /// A root nested inside another narrows neither.
    ///
    /// A failure would mean adding a more precise entry to the list removes a
    /// broader one — an allow list that behaves like a restriction list. The
    /// operator who writes both would get fewer rights than by writing only
    /// the first, and nothing in the file would say so.
    #[test]
    fn a_nested_root_narrows_neither_itself_nor_the_one_containing_it() {
        let (dir, outer) = canonical_tempdir();
        std::fs::create_dir_all(dir.path().join("inner")).unwrap();
        std::fs::write(dir.path().join("inner/deep.txt"), "x").unwrap();
        std::fs::write(dir.path().join("shallow.txt"), "x").unwrap();
        let (sibling_dir, sibling) = canonical_tempdir();
        std::fs::write(sibling_dir.path().join("secret"), "s").unwrap();

        let l = load(&path_policy(&[&format!(
            "allowed_read_paths = [\"{outer}\", \"{outer}/inner\"]"
        )]))
        .expect("loads");

        for p in [
            format!("{outer}/shallow.txt"),
            format!("{outer}/inner/deep.txt"),
        ] {
            assert_eq!(
                l.engine.evaluate(&read(&p)),
                PolicyEffect::Allow,
                "{p}: the nested entry must take nothing away from the one containing it"
            );
        }
        assert!(
            matches!(
                l.engine.evaluate(&read(&format!("{sibling}/secret"))),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "two nested roots still grant only what they contain"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  The policy lists, crossed with each action's class
// ═══════════════════════════════════════════════════════════════════════════
//
// `side_effect_class` is an exhaustive match: a new action does not compile
// until it is classified. The LISTS of `policy.rs` (`commands_of`,
// `paths_of`, `is_write_action`) do not have that property — the compiler
// does not see that a variant is missing. `Listen` (2026-09-10) then eleven
// other actions (2026-09-11) slipped past `readonly` mode exactly that way.
// This module does to the lists what the exhaustive match does to the class:
// for every variant of the vocabulary, it requires the list to say what the
// class implies.
#[cfg(test)]
mod class_vs_lists {
    use super::actions_switch::probe_action;
    use super::*;
    use crate::policy::{DenyReason, PolicyEffect};
    use x0ne_actions::{Action, SideEffectClass};

    /// An engine where everything is permitted EXCEPT what the mode refuses:
    /// all actions enabled, the desktop enabled, a command allowlist that
    /// contains what the probes launch, wide roots. So the only possible
    /// refusal is the mode's — and that is what is measured.
    fn engine_in_mode(mode: &str) -> Loaded {
        let mode_line = format!("mode = \"{mode}\"");
        let mut toml = policy_toml_overriding_lines(
            "stdio",
            &[
                &mode_line,
                "allowed_commands = [\"echo\", \"sh\", \"gcloud\", \"nvidia-smi\", \"python3\", \"cargo\", \"ls\"]",
                "allowed_read_paths = [\"/\"]",
                "allowed_write_paths = [\"/\"]",
            ],
        );
        toml.push_str("\n[desktop]\nenabled = true\n\n[actions]\n");
        for name in x0ne_actions::all_type_names() {
            toml.push_str(&format!("{name} = true\n"));
        }
        load(&toml).expect("the test policy loads")
    }

    /// `readonly` mode refuses EVERY action whose class touches the host, and
    /// NO other — derived from the class, never from a list.
    ///
    /// SECURITY: a failure in the first direction is the hole of 2026-09-11:
    /// an action that writes, executes, goes out to the network or destroys,
    /// and that a runtime declared `readonly` executes anyway. Eleven actions
    /// were in that case, including `DeleteSnapshot` and `SshExec`. A failure
    /// in the second direction is a read refused in `readonly`, that is, a
    /// mode that no longer serves any purpose.
    #[test]
    fn readonly_refuses_exactly_the_actions_whose_class_touches_the_host() {
        let l = engine_in_mode("readonly");
        let mut refused = Vec::new();
        let mut let_through = Vec::new();
        let mut wrongly_refused = Vec::new();
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            if crate::policy::is_always_allowed(name) {
                continue;
            }
            let class = action.side_effect_class();
            let verdict = l.engine.evaluate(&action);
            let is_readonly_refusal =
                matches!(verdict, PolicyEffect::Deny(DenyReason::ReadonlyMode));
            if class.touches_host() {
                if is_readonly_refusal {
                    refused.push(*name);
                } else {
                    let_through.push((*name, class.as_str(), format!("{verdict:?}")));
                }
            } else if class != SideEffectClass::Composite && is_readonly_refusal {
                wrongly_refused.push((*name, class.as_str()));
            }
        }
        assert!(
            let_through.is_empty(),
            "these actions touch the host and pass in readonly: {let_through:#?}"
        );
        assert!(
            wrongly_refused.is_empty(),
            "these actions do not touch the host and are refused in readonly: {wrongly_refused:#?}"
        );
        assert!(
            refused.len() >= 40,
            "precondition: the matrix must have seen dozens of modifying actions, not {}",
            refused.len()
        );
    }

    /// Plan mode refuses every action whose class touches the host, and no
    /// other, on an engine where the file allows everything and a session
    /// grant was given before the switch.
    ///
    /// SECURITY: a failure in the first direction is a write, a command or a
    /// network call that runs while the human believes the session only
    /// reads. In the second, plan mode refuses reads and serves no purpose.
    #[test]
    fn plan_mode_refuses_exactly_the_actions_whose_class_touches_the_host() {
        let l = engine_in_mode("auto");
        l.engine
            .apply_grant(&x0ne_actions::policy_grant::PolicyGrant {
                kind: x0ne_actions::policy_grant::GrantKind::Command,
                value: "touch".into(),
                persist: false,
            })
            .expect("precondition: a grant before plan mode");
        l.engine
            .set_plan_mode(true)
            .expect("stdio switches plan mode");
        let mut let_through = Vec::new();
        let mut wrongly_refused = Vec::new();
        let mut refused = 0;
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            if crate::policy::is_always_allowed(name) {
                continue;
            }
            let class = action.side_effect_class();
            let verdict = l.engine.evaluate(&action);
            let is_plan_refusal = matches!(verdict, PolicyEffect::Deny(DenyReason::PlanMode));
            if class.touches_host() {
                if is_plan_refusal {
                    refused += 1;
                } else {
                    let_through.push((*name, class.as_str(), format!("{verdict:?}")));
                }
            } else if class != SideEffectClass::Composite && is_plan_refusal {
                wrongly_refused.push((*name, class.as_str()));
            }
        }
        assert!(
            let_through.is_empty(),
            "these actions touch the host and pass in plan mode: {let_through:#?}"
        );
        assert!(
            wrongly_refused.is_empty(),
            "these actions do not touch the host and are refused in plan mode: {wrongly_refused:#?}"
        );
        assert!(
            refused >= 40,
            "precondition: only {refused} modifying actions seen"
        );

        l.engine.set_plan_mode(false).unwrap();
        let echo: Action =
            serde_json::from_value(serde_json::json!({"type": "RunCommand", "command": "echo"}))
                .unwrap();
        let back = l.engine.evaluate(&echo);
        assert!(matches!(back, PolicyEffect::Allow), "build mode: {back:?}");
    }

    /// `confirm` mode asks for exactly the same actions that `readonly`
    /// refuses.
    #[test]
    fn confirm_asks_for_exactly_the_actions_readonly_refuses() {
        let l = engine_in_mode("confirm");
        let mut wrong = Vec::new();
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            if crate::policy::is_always_allowed(name) {
                continue;
            }
            let class = action.side_effect_class();
            if class == SideEffectClass::Composite {
                continue;
            }
            let asks = matches!(
                l.engine.evaluate(&action),
                PolicyEffect::RequireConfirmation(_)
            );
            if asks != class.touches_host() {
                wrong.push((*name, class.as_str(), asks));
            }
        }
        assert!(wrong.is_empty(), "(action, classe, demande ?) : {wrong:#?}");
    }

    /// Every action of class `ProcessSpawn` names the program it launches,
    /// so that `allowed_commands` sees it — or appears here with the written
    /// reason why it cannot.
    ///
    /// SECURITY: `Listen` was missing from `commands_of` for a day and the
    /// same argv that `RunCommand` refused passed under `Allow`. A new action
    /// that launches a process fails here until the list names it.
    #[test]
    fn every_process_spawn_action_names_the_program_it_runs() {
        // (action, why `commands_of` is empty for it)
        const NAMED_ELSEWHERE: &[(&str, &str)] = &[
            (
                "SafeTest",
                "derives a FIXED command from `kind` (cargo/pytest/npm), never from model text",
            ),
            (
                "DocumentExtract",
                "pdftotext/ffmpeg/tesseract fixed by input type; checked by the executor (`executor/ml.rs`)",
            ),
            (
                "SendInput",
                "launches nothing: writes to the stdin of a handle that already went through the gate",
            ),
            (
                "TermSend",
                "launches nothing itself: types into a terminal whose program went through TermOpen's gate",
            ),
            (
                "TermResize",
                "launches nothing: resizes a terminal opened through TermOpen's gate",
            ),
        ];
        let l = engine_in_mode("auto");
        let mut unnamed = Vec::new();
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            if action.side_effect_class() != SideEffectClass::ProcessSpawn {
                continue;
            }
            if NAMED_ELSEWHERE.iter().any(|(n, _)| n == name) {
                continue;
            }
            // The matrix probes carry no command (nothing runs them); a
            // spawn with an empty argv names nothing, legitimately. Give
            // those three a program so the question is about the LIST.
            let action = loaded_spawn(&action);
            if l.engine.commands_of(&action).is_empty() {
                unnamed.push(*name);
            }
        }
        assert!(
            unnamed.is_empty(),
            "these actions launch a process that `allowed_commands` does not see: {unnamed:?}"
        );
        // And the admission cannot grow silently: every entry must
        // designate an action that still exists and is indeed ProcessSpawn.
        for (n, _) in NAMED_ELSEWHERE {
            let a = probe_action(n).expect("the exempt action exists");
            assert_eq!(a.side_effect_class(), SideEffectClass::ProcessSpawn, "{n}");
        }
    }

    /// The probe, with a program where the probe left the argv empty.
    fn loaded_spawn(action: &Action) -> Action {
        match action {
            Action::RunInSandbox { .. } => {
                let mut a = action.clone();
                if let Action::RunInSandbox { command, .. } = &mut a {
                    *command = vec!["echo".into()];
                }
                a
            }
            Action::RunContainerGroup { .. } => {
                let mut a = action.clone();
                if let Action::RunContainerGroup { tasks, .. } = &mut a {
                    for t in tasks.iter_mut() {
                        t.command = vec!["echo".into()];
                    }
                }
                a
            }
            Action::WatchAndReact { .. } => {
                let mut a = action.clone();
                if let Action::WatchAndReact { action: inner, .. } = &mut a {
                    **inner = Action::RunCommand {
                        command: "echo".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                        timeout_secs: None,
                        stream: None,
                    };
                }
                a
            }
            other => other.clone(),
        }
    }

    /// Every path field an action carries is seen by `paths_of` — hence
    /// by the roots and by `deny_path_patterns`.
    ///
    /// SECURITY: `WatchAndReact.path` was missing until 2026-09-11; a watch
    /// on `~/.ssh` was accepted. The field names below are those the
    /// vocabulary uses for a host path; an action that carries one and that
    /// `paths_of` ignores fails here.
    #[test]
    fn every_path_field_in_the_vocabulary_reaches_the_path_gate() {
        const PATH_FIELDS: &[&str] = &["path", "paths", "root", "cwd", "input", "output"];
        // (action, why this field is not a host path to confine)
        const NOT_A_HOST_PATH: &[(&str, &str)] = &[
            ("RunInSandbox", "`cwd` is inside the container, another trust domain"),
            ("RunContainerGroup", "same, paths internal to the containers"),
            ("SshExec", "paths on the REMOTE host"),
            ("ArtifactCreate", "`path` is relative to the artifact store, confined by it"),
            ("ArtifactSave", "same"),
            ("PreviewProgram", "`path` is resolved by the verifier, not by the gate"),
            ("RunProgram", "same"),
            ("Diff", "`files = false` in the probe: `a`/`b` are texts"),
            ("Subscribe", "the path is in `event_type`, extracted by `paths_of` separately"),
            (
                "If",
                "each check builds a REAL action (FileInfo, ReadFile, RunCommand, PortCheck) and runs it through the policy at execution — `executor/assert.rs`",
            ),
            ("Assert", "same, same executor"),
            ("SendInput", "`input` is the text written to stdin, not a path"),
        ];
        let l = engine_in_mode("auto");
        let mut unseen = Vec::new();
        for name in x0ne_actions::all_type_names() {
            let Some(action) = probe_action(name) else {
                continue;
            };
            if NOT_A_HOST_PATH.iter().any(|(n, _)| n == name) {
                continue;
            }
            let json = serde_json::to_value(&action).expect("serialise");
            let Some(obj) = json.as_object() else {
                continue;
            };
            let carries_path = PATH_FIELDS.iter().any(|f| {
                obj.get(*f)
                    .map(|v| v.is_string() || v.as_array().is_some_and(|a| !a.is_empty()))
                    .unwrap_or(false)
            });
            if carries_path && l.engine.paths_of(&action).is_empty() {
                unseen.push(*name);
            }
        }
        assert!(
            unseen.is_empty(),
            "these actions carry a path the gate does not look at: {unseen:?}"
        );
    }
}
