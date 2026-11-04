//! Which environment variable names an *action* may set on a child process.
//!
//! Not to be confused with `x0ne-runtime`'s `child_env` allowlist, which
//! answers the opposite question: what the child INHERITS from the operator's
//! own environment. This module is about the `env` map the action carries —
//! text the model wrote.
//!
//! # The defect this closes
//!
//! Measured on 2026-08-15 (audit finding n°5). `child_env::harden` clears the
//! environment and rebuilds it from an allowlist, PATH included; the action's
//! `env` map is then applied on top with no filter on the key
//! (`x0ne-runtime/src/executor/process.rs:228` and `:309`,
//! `executor/artifact.rs:346`):
//!
//! ```text
//! {"type":"RunCommand","command":"cat","args":["f"],"env":{"PATH":"/tmp/evil"}}
//!   → PWNED-fake-cat
//! ```
//!
//! One field defeated two gates at once. The human gate is waived for reads by
//! program name — `is_read_only_command(command, args)` never saw `env`, so it
//! ruled on `cat` while a field it could not see picked the executable. The
//! runtime's `allowed_commands` list matches the same string and passed for the
//! same reason. `LD_PRELOAD` is the same defeat needing no PATH entry at all.
//!
//! # Why a closed denylist here, when `child_env` is an allowlist
//!
//! The two lists guard different things and the shape follows from that. An
//! allowlist is right for inheritance, because the thing being kept out is the
//! operator's secrets and nobody can enumerate the name of the next one. Here
//! the thing being kept out is a *mechanism* — the finite set of variables the
//! loader, the shell and the language runtimes consult to determine WHAT to
//! execute — while the thing being let through is open-ended by design: an
//! action legitimately sets `RUST_LOG`, `PORT`, `CI`, `MY_VAR`. An allowlist
//! would have to predict the application's own vocabulary, which is the part
//! nobody can enumerate.
//!
//! So this list must be complete for its family, and adding to it is cheap.
//! Every entry names the mechanism it disarms.

use std::collections::HashMap;

/// Exact names whose value changes WHICH program (or which code inside it)
/// runs. Matched case-insensitively — see [`is_program_choosing`].
///
/// SECURITY: this list is what stands between an approved command NAME and the
/// program that actually executes. Removing an entry needs the argument that
/// its variable cannot redirect execution, not that it looked harmless.
pub const KEYS_THAT_CHOOSE_THE_PROGRAM: &[(&str, &str)] = &[
    // ── The resolver, the loader, the shell ─────────────────────────────────
    (
        "PATH",
        "resolves a bare command name to a file; setting it runs a program of \
         the action's choosing behind a name the gate already approved",
    ),
    (
        "HOME",
        "every tool reads its own rc file under it (~/.gitconfig runs `pager` \
         and diff drivers, ~/.ssh/config runs ProxyCommand)",
    ),
    (
        "SHELL",
        "tools that re-exec the user's shell (git, editors, `less`) run this \
         binary rather than a shell",
    ),
    (
        "IFS",
        "changes how a shell splits words, so an argument becomes several",
    ),
    (
        "ENV",
        "sh reads and executes this file at startup, in every non-interactive \
         shell a child may open",
    ),
    (
        "BASH_ENV",
        "the same for bash: a script sourced before anything else",
    ),
    (
        "PS4",
        "bash expands it, substitutions included, when `set -x` is active — a \
         prompt string that runs commands",
    ),
    ("PROMPT_COMMAND", "bash executes it before each prompt"),
    (
        "GCONV_PATH",
        "glibc loads character-set conversion modules from it — arbitrary \
         shared objects, in any process that touches iconv",
    ),
    (
        "LOCPATH",
        "glibc loads compiled locale objects from it, before main()",
    ),
    (
        "NLSPATH",
        "glibc message catalogues, a path template expanded inside libc",
    ),
    (
        "PERL5OPT",
        "options perl applies to every script, `-M` (load a module) included",
    ),
    (
        "PERL5LIB",
        "where perl looks for the modules a script requires",
    ),
    (
        "NODE_OPTIONS",
        "node applies it to every process it starts, `--require` included",
    ),
    (
        "RUBYOPT",
        "options ruby applies to every script, `-r` included",
    ),
    (
        "RUBYLIB",
        "where ruby looks for the libraries a script requires",
    ),
    (
        "JAVA_TOOL_OPTIONS",
        "the JVM reads it on startup and it can name an agent (`-javaagent`)",
    ),
    (
        "_JAVA_OPTIONS",
        "the same mechanism under its older spelling",
    ),
    ("JDK_JAVA_OPTIONS", "and its java-11-and-later spelling"),
    (
        "CLASSPATH",
        "where the JVM loads classes from, which is where its code comes from",
    ),
];

/// Families whose members cannot be enumerated, blocked by prefix.
///
/// SECURITY: a prefix here is deliberately wide. The cost of a false refusal is
/// one error message the model can work around by renaming its variable; the
/// cost of a miss is arbitrary code in every child process.
pub const PREFIXES_THAT_CHOOSE_THE_PROGRAM: &[(&str, &str)] = &[
    (
        "LD_",
        "the dynamic loader's whole namespace — LD_PRELOAD and LD_AUDIT map an \
         object of the caller's choosing into every child, LD_LIBRARY_PATH \
         redirects the libraries it links against",
    ),
    (
        "DYLD_",
        "the same namespace on macOS (DYLD_INSERT_LIBRARIES), listed because \
         the action text is portable even when this host is not",
    ),
    (
        "PYTHON",
        "PYTHONPATH, PYTHONHOME and PYTHONSTARTUP all decide which code the \
         interpreter imports and runs before the script does",
    ),
    (
        "MALLOC_",
        "glibc's allocator tunables run inside libc before main() — \
         MALLOC_TRACE names a file libc opens on its own",
    ),
];

/// Is `key` one of the variables that decide what executes?
///
/// Matched on the UPPERCASED name. Unix variables are case-sensitive and the
/// loader only reads the canonical spelling, so this is strictly more
/// conservative than it needs to be. The cost is a refused `python_version`;
/// the alternative is guessing which host tool upcases before it looks.
pub fn is_program_choosing(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    KEYS_THAT_CHOOSE_THE_PROGRAM
        .iter()
        .any(|(n, _)| *n == upper)
        || PREFIXES_THAT_CHOOSE_THE_PROGRAM
            .iter()
            .any(|(p, _)| upper.starts_with(p))
}

/// The stated reason a key is refused, or `None` if it is acceptable.
fn why_refused(key: &str) -> Option<String> {
    if key.is_empty() {
        return Some("an environment variable with an empty name cannot be set".to_string());
    }
    if key.contains('\0') {
        return Some(format!(
            "the name of an environment variable cannot contain a NUL byte \
             (key {key:?})"
        ));
    }
    // Charset before family, so a name like `PATH\0X` or `pa-th` is reported as
    // malformed rather than sneaking past an exact-name comparison.
    let mut chars = key.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !head_ok || !tail_ok {
        return Some(format!(
            "environment variable name {key:?} is not of the form \
             [A-Za-z_][A-Za-z0-9_]* — anything else is a name the shell and the \
             loader disagree about"
        ));
    }

    let upper = key.to_ascii_uppercase();
    if let Some((_, why)) = KEYS_THAT_CHOOSE_THE_PROGRAM
        .iter()
        .find(|(n, _)| *n == upper)
    {
        return Some(format!(
            "setting {key} is refused: {why}. An action's `env` may carry \
             ordinary application variables (FOO=bar); it may not carry the \
             ones that decide what runs."
        ));
    }
    if let Some((prefix, why)) = PREFIXES_THAT_CHOOSE_THE_PROGRAM
        .iter()
        .find(|(p, _)| upper.starts_with(*p))
    {
        return Some(format!(
            "setting {key} is refused: the {prefix}* family is {why}. An \
             action's `env` may carry ordinary application variables \
             (FOO=bar); it may not carry the ones that decide what runs."
        ));
    }
    None
}

/// Whether a caller-supplied name is acceptable — the predicate form, for
/// call sites that only need a yes/no (`x0ne-orchestrator`'s incus wrapper).
pub fn is_safe_caller_env_key(key: &str) -> bool {
    why_refused(key).is_none()
}

/// Validate the whole `env` map an action carries.
///
/// SECURITY: a refusal, never a silent skip. Dropping the key quietly would
/// leave the model free to retry the same trick forever and the operator with
/// no record that it was tried — and a prompt-injected agent trying to set
/// `LD_PRELOAD` is exactly the event an operator must be told about.
///
/// Offending keys are reported in sorted order so the message does not depend
/// on `HashMap` iteration order, which changes between runs.
pub fn check_caller_env(env: &HashMap<String, String>) -> Result<(), String> {
    let mut refusals: Vec<(String, String)> = env
        .keys()
        .filter_map(|k| why_refused(k).map(|why| (k.clone(), why)))
        .collect();
    if refusals.is_empty() {
        // The value is not a key, but a NUL in it makes the spawn fail with an
        // error that names nothing useful. Reported here, where the cause is.
        let mut bad: Vec<&String> = env
            .iter()
            .filter(|(_, v)| v.contains('\0'))
            .map(|(k, _)| k)
            .collect();
        bad.sort();
        if let Some(k) = bad.first() {
            return Err(format!(
                "the value of environment variable {k} contains a NUL byte; \
                 an environment entry is NUL-terminated and cannot hold one"
            ));
        }
        return Ok(());
    }
    refusals.sort_by(|a, b| a.0.cmp(&b.0));
    Err(refusals
        .into_iter()
        .map(|(_, why)| why)
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The measurement of 2026-08-15, as a unit property.
    #[test]
    fn the_variables_that_choose_the_binary_are_refused() {
        for key in [
            "PATH",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "DYLD_INSERT_LIBRARIES",
            "HOME",
            "SHELL",
            "IFS",
            "PS4",
            "BASH_ENV",
            "ENV",
            "PROMPT_COMMAND",
            "PERL5OPT",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "NODE_OPTIONS",
            "RUBYOPT",
            "JAVA_TOOL_OPTIONS",
            "GCONV_PATH",
            "MALLOC_TRACE",
            "LOCPATH",
        ] {
            assert!(
                check_caller_env(&map(&[(key, "x")])).is_err(),
                "{key} would let an action choose what executes"
            );
        }
    }

    /// The complement. A filter that refuses ordinary variables gets reverted,
    /// and then the hole is back — this is why the list is a closed family and
    /// not "names that look dangerous".
    #[test]
    fn ordinary_application_variables_pass() {
        for key in [
            "MY_VAR",
            "FOO123",
            "RUST_LOG",
            "PORT",
            "CI",
            "ARTIFACT_ID",
            "DATABASE_URL",
            "_private",
            "NODE_ENV",
        ] {
            assert!(
                check_caller_env(&map(&[(key, "x")])).is_ok(),
                "{key} is an ordinary variable and must still be settable"
            );
        }
    }

    /// The refusal must name the key AND the mechanism. An operator reading
    /// "denied" learns nothing; the point of refusing rather than skipping is
    /// that somebody finds out what was attempted.
    #[test]
    fn the_refusal_names_the_key_and_the_reason() {
        let err = check_caller_env(&map(&[("LD_PRELOAD", "/tmp/evil.so")])).unwrap_err();
        assert!(err.contains("LD_PRELOAD"), "{err}");
        assert!(err.contains("LD_"), "{err}");
        assert!(err.len() > 60, "the message must say why: {err}");
    }

    /// Several bad keys report deterministically. `HashMap` iteration order is
    /// randomised per process, so an unsorted message differs between runs and
    /// nobody can write a test — or a bug report — against it.
    #[test]
    fn several_refusals_are_reported_in_a_stable_order() {
        let m = map(&[("PATH", "/tmp"), ("LD_PRELOAD", "/tmp/x.so"), ("OK", "1")]);
        let first = check_caller_env(&m).unwrap_err();
        for _ in 0..8 {
            assert_eq!(check_caller_env(&m).unwrap_err(), first);
        }
        assert!(first.find("LD_PRELOAD") < first.find("PATH"), "{first}");
    }

    /// The charset is part of the guard, not tidiness: `PATH=x` as a *key* and
    /// a NUL-bearing name are both attempts to have the parser and the kernel
    /// read different things.
    #[test]
    fn a_malformed_name_is_refused_before_the_family_is_consulted() {
        for key in ["with-dash", "", "1LEADING", "a b", "PATH=x", "VAR\0X", "é"] {
            assert!(
                check_caller_env(&map(&[(key, "x")])).is_err(),
                "{key:?} is not a well-formed environment variable name"
            );
        }
    }

    /// A NUL in the value makes `spawn` fail with an error naming nothing.
    #[test]
    fn a_nul_in_a_value_is_refused_where_the_cause_is_visible() {
        let err = check_caller_env(&map(&[("MY_VAR", "a\0b")])).unwrap_err();
        assert!(err.contains("MY_VAR"), "{err}");
    }

    /// Every entry says which mechanism it disarms. The justification is the
    /// only thing keeping this list from growing into superstition.
    #[test]
    fn every_entry_names_the_mechanism_it_disarms() {
        for (name, why) in KEYS_THAT_CHOOSE_THE_PROGRAM
            .iter()
            .chain(PREFIXES_THAT_CHOOSE_THE_PROGRAM)
        {
            assert!(!name.is_empty());
            assert!(why.len() > 30, "{name} is refused with no stated mechanism");
        }
    }

    /// The predicate and the map check agree. Two spellings of one rule is how
    /// a call site ends up guarded by the lenient half.
    #[test]
    fn the_predicate_and_the_map_check_are_the_same_rule() {
        for key in ["PATH", "LD_PRELOAD", "MY_VAR", "with-dash", "PORT"] {
            assert_eq!(
                is_safe_caller_env_key(key),
                check_caller_env(&map(&[(key, "v")])).is_ok(),
                "the two forms disagree about {key}"
            );
        }
    }
}
