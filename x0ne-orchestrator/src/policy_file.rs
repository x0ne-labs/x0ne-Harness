//! Writing one granted line into the operator's `x0ne-runtime.toml`.
//!
//! # Why the orchestrator writes it and the runtime does not
//!
//! Two authorities, and the split is the one the whole product is built on
//! (docs/invariants.md, invariant 2): the runtime is a confined executor and has no
//! human; the orchestrator has one. "Add this to my config" is a sentence a
//! person said. So the orchestrator writes the file, and the runtime's
//! in-memory grant — a separate mechanism, gone at exit — is what authorizes
//! the action right now. A grant can therefore never become permanent by
//! accident: two different things have to happen.
//!
//! # Which file
//!
//! The one the runtime ACTUALLY loaded, reported back in the grant response.
//! Never a path recomputed here. Both sides derive the same search convention
//! independently (`x0ne-runtime/src/config.rs` vs `setup_menu::
//! runtime_config_path`), and they part company the moment `--config`, the
//! legacy XDG layout or the cwd opt-in is involved. Writing a file the runtime
//! never read would tell the operator his config was updated and then ask him
//! the same question next session — the dead scaffold, in the one place where
//! it also silently authors security policy.
//!
//! # `toml_edit`, never `toml`
//!
//! `~/.config/x0ne/x0ne-runtime.toml` is a commented file, written to be read
//! by a human — every list in it carries the paragraph explaining what an
//! empty value means. `toml::to_string` would return the file with every
//! comment gone. See `persisting_a_grant_keeps_the_operators_comments`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use x0ne_actions::policy_grant::{GrantKind, PolicyGrant};

/// Add one entry to the operator's policy file, keeping everything else —
/// comments, ordering, formatting — exactly as he left it.
///
/// Returns a one-line description of what was written, for the journal and
/// for what the operator is told.
///
/// SECURITY: this only ever ADDS. It never removes an entry, never rewrites a
/// list, never touches `deny_path_patterns`, `mode` or `default_effect`. A
/// prompt that could narrow the file as well as widen it would be a prompt
/// that can disarm the policy, and there is no answer at the prompt that means
/// "remove something".
pub fn persist_grant(config_path: &Path, grant: &PolicyGrant) -> Result<String> {
    if !config_path.exists() {
        bail!(
            "{} does not exist. The running runtime is on its compiled defaults, so there is \
             no file to extend — the grant holds for this session only.",
            config_path.display()
        );
    }
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("cannot read {}", config_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .with_context(|| format!("{} is not valid TOML", config_path.display()))?;

    let written = match grant.kind {
        GrantKind::Action => {
            let table = table_mut(&mut doc, "actions")?;
            table[grant.value.as_str()] = value(true);
            format!("[actions] {} = true", grant.value)
        }
        _ => {
            let (section, key) = grant.kind.toml_location();
            let table = table_mut(&mut doc, section)?;
            let entry = table.entry(key).or_insert_with(|| value(Array::new()));
            let Some(array) = entry.as_array_mut() else {
                bail!(
                    "[{section}] {key} is not a list in {}",
                    config_path.display()
                );
            };
            if array
                .iter()
                .any(|v| v.as_str() == Some(grant.value.as_str()))
            {
                return Ok(format!(
                    "[{section}] {key} already contains '{}'",
                    grant.value
                ));
            }
            array.push(grant.value.as_str());
            // A list the operator formatted one-entry-per-line stays that way.
            if raw.contains(&format!("{key} = [\n")) {
                for v in array.iter_mut() {
                    v.decor_mut().set_prefix("\n    ");
                }
                array.set_trailing("\n");
                array.set_trailing_comma(true);
            }
            format!("[{section}] {key} += \"{}\"", grant.value)
        }
    };

    // Write through a temp file in the same directory, then rename: a crash
    // mid-write must not leave the operator with a truncated policy, which
    // would be a policy file that fails to parse and a runtime that will not
    // start.
    let tmp = config_path.with_extension("toml.x0ne-grant-tmp");
    std::fs::write(&tmp, doc.to_string())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(config_path) {
            let _ = std::fs::set_permissions(
                &tmp,
                std::fs::Permissions::from_mode(meta.permissions().mode() & 0o7777),
            );
        }
    }
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("cannot replace {}", config_path.display()))?;

    Ok(written)
}

/// Get a table, creating it if the operator never wrote one.
fn table_mut<'d>(doc: &'d mut DocumentMut, name: &str) -> Result<&'d mut Table> {
    if doc.get(name).is_none() {
        doc[name] = Item::Table(Table::new());
    }
    doc[name]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[{name}] exists but is not a table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATOR_FILE: &str = r#"# ── Policy ───────────────────────────────────
[policy]
# The orchestrator owns the human gate; this runtime enforces the lists.
mode = "auto"
default_effect = "allow"

# Commands the LLM is allowed to invoke via RunCommand.
# An EMPTY list denies every command.
allowed_commands = []

# Directories the agent may READ from.
allowed_read_paths = [
    "~/code",
]

[actions]
# ── Read-only — enabled by default ──
ReadFile      = true
"#;

    fn write_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x0ne-runtime.toml");
        std::fs::write(&p, OPERATOR_FILE).unwrap();
        (dir, p)
    }

    /// The operator's comments survive a grant.
    ///
    /// A failure means answering "2" at a prompt silently strips every
    /// explanation from the one file in X0NE that is written to be read by a
    /// human — including the paragraphs that say what an empty list means,
    /// which are the difference between a policy he understands and a policy
    /// he obeys. `toml` (as opposed to `toml_edit`) does exactly this.
    #[test]
    fn persisting_a_grant_keeps_the_operators_comments() {
        let (_d, path) = write_fixture();
        persist_grant(
            &path,
            &PolicyGrant {
                kind: GrantKind::Command,
                value: "ls".into(),
                persist: true,
            },
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        for comment in [
            "# The orchestrator owns the human gate",
            "# Commands the LLM is allowed to invoke via RunCommand.",
            "# An EMPTY list denies every command.",
            "# Directories the agent may READ from.",
            "# ── Read-only — enabled by default ──",
        ] {
            assert!(
                after.contains(comment),
                "the grant erased a comment: {comment:?}\n--- file after ---\n{after}"
            );
        }
        assert!(
            after.contains("\"ls\""),
            "the command was not added:\n{after}"
        );
        // And the file is still loadable as policy.
        let doc: DocumentMut = after.parse().expect("the file no longer parses");
        assert_eq!(doc["policy"]["default_effect"].as_str(), Some("allow"));
    }

    /// Each kind lands in the list it named, and nowhere else.
    ///
    /// A failure means a read grant widened writes, or an action grant landed
    /// in `[policy]` — a widening the operator did not agree to, written into
    /// the file that outlives the session.
    #[test]
    fn a_grant_writes_only_the_list_it_named() {
        for (kind, value_, needle, forbidden) in [
            (
                GrantKind::Command,
                "cargo",
                "allowed_commands",
                "allowed_write_paths",
            ),
            (
                GrantKind::ReadPath,
                "/srv/data",
                "allowed_read_paths",
                "allowed_write_paths",
            ),
            (
                GrantKind::WritePath,
                "/srv/out",
                "allowed_write_paths",
                "allowed_commands",
            ),
        ] {
            let (_d, path) = write_fixture();
            persist_grant(
                &path,
                &PolicyGrant {
                    kind,
                    value: value_.into(),
                    persist: true,
                },
            )
            .unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            let doc: DocumentMut = after.parse().unwrap();
            let list = doc["policy"][needle].as_array().unwrap();
            assert!(
                list.iter().any(|v| v.as_str() == Some(value_)),
                "{kind:?}: {value_} is not in {needle}:\n{after}"
            );
            if let Some(other) = doc["policy"].get(forbidden).and_then(|i| i.as_array()) {
                assert!(
                    !other.iter().any(|v| v.as_str() == Some(value_)),
                    "{kind:?}: {value_} leaked into {forbidden}"
                );
            }
        }

        let (_d, path) = write_fixture();
        persist_grant(
            &path,
            &PolicyGrant {
                kind: GrantKind::Action,
                value: "RunCommand".into(),
                persist: true,
            },
        )
        .unwrap();
        let doc: DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["actions"]["RunCommand"].as_bool(), Some(true));
        assert_eq!(
            doc["actions"]["ReadFile"].as_bool(),
            Some(true),
            "an existing switch was disturbed"
        );
    }

    /// Granting the same thing twice does not duplicate the line.
    ///
    /// A failure means a config that grows an entry per session until the
    /// operator cannot read his own allowlist.
    #[test]
    fn granting_twice_writes_one_entry() {
        let (_d, path) = write_fixture();
        let grant = PolicyGrant {
            kind: GrantKind::Command,
            value: "ls".into(),
            persist: true,
        };
        persist_grant(&path, &grant).unwrap();
        let second = persist_grant(&path, &grant).unwrap();
        assert!(second.contains("already contains"), "{second}");
        let doc: DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let list = doc["policy"]["allowed_commands"].as_array().unwrap();
        assert_eq!(list.iter().filter(|v| v.as_str() == Some("ls")).count(), 1);
    }

    /// A runtime on compiled defaults has no file to extend, and the operator
    /// is told so rather than handed a new file he never wrote.
    ///
    /// A failure means X0NE authors a policy file out of nowhere — and a file
    /// that exists is a file the next runtime will load, so an invented one is
    /// an invented policy.
    #[test]
    fn a_missing_config_is_never_invented() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let err = persist_grant(
            &path,
            &PolicyGrant {
                kind: GrantKind::Command,
                value: "ls".into(),
                persist: true,
            },
        )
        .expect_err("a config file was created from nothing");
        assert!(!path.exists(), "the file was written anyway");
        assert!(
            err.to_string().contains("session only"),
            "the operator is not told what he actually got: {err}"
        );
    }
}
