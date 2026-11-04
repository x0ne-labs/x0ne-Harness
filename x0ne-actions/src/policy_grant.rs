//! The vocabulary of a *policy grant* — the operator widening his own policy,
//! for one command, one directory or one action, from the prompt.
//!
//! # The field report this exists for
//!
//! 2026-08-16, session `2026-08-16_01-11_111b`. The operator asked the agent
//! to look inside `./playground`. `RunCommand ls` was refused because his
//! `allowed_commands` is empty, and `FileInfo playground` because the path was
//! outside `allowed_read_paths`. Both refusals were correct, both named the
//! TOML key that lifts them — and neither was ever put to him as a question.
//! He wrote: « why don't I get a gate? … can you gate that for me? ».
//!
//! A refusal that says "add this line to your own file" is not a security
//! boundary being held; it is a decision the operator is entitled to make,
//! deferred to a text editor. This module names the four decisions that can
//! be put to him, and — just as importantly — is the reason the ones that
//! cannot are enumerated somewhere else, one by one, in an exhaustive match
//! (`x0ne_runtime::policy::DenyReason::liftable`).
//!
//! # Why this lives in `x0ne-actions` and not in either end
//!
//! `x0ne-orchestrator` deliberately does not depend on `x0ne-runtime`; this
//! crate is the seam between them (see the note at the top of `lib.rs`). The
//! runtime decides what a refusal *is*, the orchestrator decides whether to
//! ask a human about it, and both have to name the same four things.
//!
//! # What a grant is NOT
//!
//! SECURITY: `PolicyGrant` is a control message, never an `crate::Action`.
//! It is deliberately absent from the action enum, from the generated
//! catalogue, from `ListActions` and from `DescribeAction`, so a model that
//! emits `{"type":"PolicyGrant",…}` is rejected as an unknown action by the
//! runtime's `from_value::<Action>` like any other invented verb. The only
//! thing that can produce one is the orchestrator, after a human answered a
//! prompt. See `the_model_cannot_emit_a_policy_grant`.

use serde::{Deserialize, Serialize};

/// What kind of policy entry a grant adds.
///
/// Four kinds and no more: each maps onto exactly one list an operator already
/// maintains by hand in `x0ne-runtime.toml`. Anything that would require a new
/// kind is, by construction, not something this mechanism knows how to lift —
/// which is why the mapping from refusals to grants is an exhaustive match
/// rather than a fallible lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantKind {
    /// One entry in `[policy] allowed_commands`.
    Command,
    /// One entry in `[policy] allowed_read_paths`.
    ReadPath,
    /// One entry in `[policy] allowed_write_paths`.
    WritePath,
    /// One `Name = true` under `[actions]`.
    Action,
}

impl GrantKind {
    /// The TOML table this grant is written into, and the key inside it.
    ///
    /// Used both to write the file and to tell the operator, in the prompt,
    /// exactly which line of his own config he is about to author. A prompt
    /// that says "add to config" without naming the key asks him to trust the
    /// machine with a file he keeps commented for himself.
    pub fn toml_location(self) -> (&'static str, &'static str) {
        match self {
            GrantKind::Command => ("policy", "allowed_commands"),
            GrantKind::ReadPath => ("policy", "allowed_read_paths"),
            GrantKind::WritePath => ("policy", "allowed_write_paths"),
            GrantKind::Action => ("actions", ""),
        }
    }

    /// How the prompt names the thing being widened, in English.
    pub fn noun(self) -> &'static str {
        match self {
            GrantKind::Command => "command",
            GrantKind::ReadPath => "directory (read)",
            GrantKind::WritePath => "directory (write)",
            GrantKind::Action => "action",
        }
    }
}

/// A refusal the operator can lift, and what it would take to lift it.
///
/// Produced by the runtime beside a `policy_denied` refusal. Its presence is
/// the whole signal: a refusal without one is a refusal the prompt must never
/// offer to waive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiftableDenial {
    pub kind: GrantKind,
    /// The exact entry that would have to exist: the bare command name, the
    /// canonical directory, or the action type name.
    pub value: String,
}

impl LiftableDenial {
    pub fn new(kind: GrantKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }

    /// The one-line headline the prompt shows above the three choices.
    ///
    /// English, because the operator's CLI is English and because the same
    /// string travels to a frontend that is not a terminal.
    pub fn headline(&self) -> String {
        match self.kind {
            GrantKind::Command => {
                format!("'{}' is not in your allowed_commands", self.value)
            }
            GrantKind::ReadPath => {
                format!("'{}' is not under any allowed_read_paths root", self.value)
            }
            GrantKind::WritePath => {
                format!("'{}' is not under any allowed_write_paths root", self.value)
            }
            GrantKind::Action => format!("'{}' is disabled under [actions]", self.value),
        }
    }

    /// Label of choice 1 — widen the running runtime only.
    pub fn session_choice(&self) -> String {
        match self.kind {
            GrantKind::Command => format!("allow '{}' for this session and run it", self.value),
            // "(this directory)" because the value IS a directory and the
            // grant covers everything under it. The operator was reading
            // "allow reading '…/tickets/ticket-002.txt' for this session" and
            // answering a question about one file (field report 2026-08-16);
            // the runtime was granting a root either way.
            GrantKind::ReadPath => format!(
                "allow reading '{}' (this directory) for the session",
                self.value
            ),
            GrantKind::WritePath => format!(
                "allow writing '{}' (this directory) for the session",
                self.value
            ),
            GrantKind::Action => format!("enable '{}' for this session", self.value),
        }
    }

    /// Label of choice 2 — widen the running runtime AND the file on disk.
    pub fn persist_choice(&self) -> String {
        let (table, key) = self.kind.toml_location();
        match self.kind {
            GrantKind::Action => {
                format!(
                    "add to config and run it (writes [{table}] {} = true)",
                    self.value
                )
            }
            _ => format!("add to config and run it (writes [{table}] {key})"),
        }
    }

    /// The grant that lifts this refusal.
    pub fn into_grant(self, persist: bool) -> PolicyGrant {
        PolicyGrant {
            kind: self.kind,
            value: self.value,
            persist,
        }
    }
}

/// The control message the orchestrator sends to widen a running runtime's
/// policy for the rest of the process's life.
///
/// SECURITY: accepted over the stdio bridge ONLY. See
/// `x0ne_runtime::control` for the reasoning and the enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyGrant {
    pub kind: GrantKind,
    pub value: String,
    /// Whether the operator also asked for this to be written to his
    /// `x0ne-runtime.toml`. The runtime never writes that file; this flag
    /// only travels so the journal and the response can say what was asked.
    #[serde(default)]
    pub persist: bool,
}

/// Why a grant value was refused before it ever reached the policy engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantValueError {
    Empty,
    /// A command name containing a path separator.
    CommandHasPath,
    /// A command name containing whitespace, a NUL, or a shell metacharacter.
    CommandNotBare(char),
    /// An action name no build of this binary knows.
    UnknownAction,
}

impl GrantValueError {
    pub fn message(&self) -> String {
        match self {
            GrantValueError::Empty => "a grant value cannot be empty".into(),
            GrantValueError::CommandHasPath =>
                "a granted command must be a bare name: an absolute or relative path would authorize \
                 whatever binary happens to sit there. Edit allowed_commands by hand for that."
                    .into(),
            GrantValueError::CommandNotBare(c) =>
                format!("a granted command must be a bare name; '{c}' is not part of one"),
            GrantValueError::UnknownAction =>
                "no action of that name exists in this runtime's vocabulary".into(),
        }
    }
}

/// Characters that can never appear in a command name granted from a prompt.
///
/// SECURITY: this is the same posture as `serve_command`: refuse rather than
/// escape. A granted name goes into `allowed_commands`, which is matched by
/// exact string equality against `argv[0]`, and `argv[0]` is executed with
/// `Command::new` and no shell (invariant 6). So none of these characters
/// could *do* anything today. They are refused because an entry containing
/// them is either a typo or an attempt to author something other than a
/// program name, and because `allowed_commands` is a file a human reads.
const NOT_IN_A_COMMAND_NAME: &[char] = &[
    ' ', '\t', '\n', '\r', '\0', ';', '&', '|', '$', '`', '(', ')', '<', '>', '*', '?', '[', ']',
    '{', '}', '\'', '"', '\\', '!', '#', '~',
];

/// Validate the *shape* of a command grant.
///
/// This mirrors what an `allowed_commands` entry must look like to ever match:
/// a bare name, no separator (see the SECURITY note on
/// `PolicyEngine::check_command_allowed` — matching on `Path::file_name()`
/// once authorized `/tmp/evil/git` for `allowed_commands = ["git"]`).
pub fn validate_command_grant(value: &str) -> Result<(), GrantValueError> {
    if value.is_empty() {
        return Err(GrantValueError::Empty);
    }
    if value.contains('/') || value.contains('\\') {
        return Err(GrantValueError::CommandHasPath);
    }
    if let Some(c) = value.chars().find(|c| NOT_IN_A_COMMAND_NAME.contains(c)) {
        return Err(GrantValueError::CommandNotBare(c));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A granted command name is a bare program name and nothing else.
    ///
    /// A failure means the prompt can author an `allowed_commands` entry that
    /// names a path or carries shell punctuation — an entry no human reading
    /// the file would recognise as something he agreed to.
    #[test]
    fn a_granted_command_is_a_bare_name() {
        assert!(validate_command_grant("ls").is_ok());
        assert!(validate_command_grant("cargo").is_ok());
        assert!(validate_command_grant("python3").is_ok());
        assert!(validate_command_grant("g++").is_ok());

        assert_eq!(
            validate_command_grant(""),
            Err(GrantValueError::Empty),
            "the empty string is not a command"
        );
        assert_eq!(
            validate_command_grant("/tmp/evil/ls"),
            Err(GrantValueError::CommandHasPath)
        );
        assert_eq!(
            validate_command_grant("./ls"),
            Err(GrantValueError::CommandHasPath)
        );
        for bad in ["ls; rm -rf /", "ls|cat", "ls $(id)", "ls&", "l s", "ls\n"] {
            assert!(
                validate_command_grant(bad).is_err(),
                "{bad:?} was accepted as a command name"
            );
        }
    }

    /// Every kind names a real table and key, so a prompt can always tell the
    /// operator which line of his own file he is authoring.
    ///
    /// A failure means "add to config" becomes an unexplained write.
    #[test]
    fn every_grant_kind_names_the_line_it_writes() {
        for kind in [
            GrantKind::Command,
            GrantKind::ReadPath,
            GrantKind::WritePath,
            GrantKind::Action,
        ] {
            let (table, _key) = kind.toml_location();
            assert!(!table.is_empty(), "{kind:?} names no TOML table");
            let d = LiftableDenial::new(kind, "x");
            assert!(!d.headline().is_empty());
            assert!(d.session_choice().contains("session"));
            assert!(
                d.persist_choice().contains(table),
                "{kind:?}: the persist choice does not name its table: {}",
                d.persist_choice()
            );
        }
    }
}
