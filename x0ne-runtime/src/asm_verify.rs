//! The v0 verifier — **on the artifact, never on the IR**.
//!
//! Input: the module bytes + the loaded policy. Output: `Verified`, or
//! `Refused` saying *where the proof is lost*, *the rule it faces*, *the lever*.
//!
//! The shift of emphasis the technical review imposes: the **guarantee** is
//! the capability check at execution time (the host functions, T4, enforce the
//! policy whatever happens). This verifier brings **early failure and gate
//! readability** — telling the human *beforehand* what the program can touch.
//! Selling it as an absolute proof would be overselling: its TCB would contain
//! the compiler.

use crate::config::AppConfig;
use crate::executor::confine::{self, ConfinedPath};
use crate::policy::{PolicyEffect, PolicyEngine};
use x0ne_actions::Action;
use x0ne_asm::limits::MAX_FILES;
use x0ne_asm::manifest::Capability;
use x0ne_asm::read::{read_module, ModuleFacts, ReadError};

/// The pattern length the host will agree to compile, measured **in
/// characters** and **before** `RegexBuilder`.
///
/// SECURITY: must stay equal to `x0ne_asm::limits::REGEX_PATTERN_CHARS`, which
/// is what the parser enforces on the program text. The parser is not a
/// safeguard here: a forged manifest carries its regexes directly in the
/// artifact, without going through it. So the verifier must measure them, and
/// that is what `the_two_regex_length_bounds_agree` holds.
///
/// The constant lives here rather than being read from `x0ne-asm` because it is
/// a decision **of the host**: two components, two constants, and a test that
/// forces them to state the same number — the pattern the repository already
/// uses for `MAX_EMIT_BYTES` / `MAX_RESULT_BYTES`.
pub const REGEX_PATTERN_CHARS: usize = 512;

/// A proven file: the path, the manifest entry that produced it, and the
/// capability of **that** entry.
#[derive(Debug, Clone)]
pub struct VerifiedFile {
    pub path: ConfinedPath,
    /// SECURITY: the index is kept **all the way to** the host. What T3 holds is
    /// carrying it: each proven path knows which manifest entry produced it.
    /// What T5 will have to do with it — query the capability of **that**
    /// entry, never "the path is under some prefix" — is what closes the
    /// read-`src/**` / write-`out/**` hole, and is not written yet.
    /// `fs_edit_buffered` does not exist at this stage.
    pub manifest_idx: usize,
    pub capability: Capability,
}

#[derive(Debug)]
pub struct Verified {
    pub facts: ModuleFacts,
    pub files: Vec<VerifiedFile>,
    /// SECURITY: symbolic links excluded from the expansion, counted.
    ///
    /// "Never silent" doctrine: a link matching the pattern does not become a
    /// proven file — following it would leave the tree, and `resolve_write`
    /// would refuse it — but excluding it without saying so would announce
    /// "3 files" where the pattern designates 4, and the difference between
    /// the two numbers is precisely what a human must be able to question.
    /// The count travels up to the preview payload.
    pub symlinks_skipped: usize,
    /// The exact bytes that were verified, kept as is.
    ///
    /// **Invariant I-B**: the host will have to instantiate **these**, never a
    /// re-read from disk, otherwise verifier and host would prove and execute
    /// two different modules. T3 holds its half — the bytes are not re-read
    /// between `read_module` and this field, and
    /// `every_expanded_path_is_submitted_to_the_policy_engine` checks it.
    /// The other half is an obligation of T5.
    pub module: Vec<u8>,
}

#[derive(Debug)]
pub enum Refused {
    Artifact(ReadError),
    BadPattern {
        pattern: String,
        reason: String,
    },
    /// SECURITY: a manifest regex pattern exceeds what the host will compile.
    /// The parser bounds the program text; it bounds nothing of a forged
    /// artifact, whose regexes arrive directly in the custom section.
    RegexTooLong {
        chars: usize,
        max: usize,
    },
    /// SECURITY: invariant I-1 — the manifest and the imports section do not
    /// say the same thing. Detected by `read_module` (it is a property of the
    /// artifact), named here because it is the refusal that says the program
    /// **shown** is not the program **compiled**.
    CapabilityMismatch {
        manifest_claims_write: bool,
        imports_edit: bool,
    },
    /// SECURITY: invariant I-4 — the module memory can grow.
    UnboundedMemory {
        pages: Option<u64>,
        maximum: Option<u64>,
    },
    TooManyFiles {
        found: usize,
        max: usize,
    },
    /// SECURITY: the expansion is empty. **Refuse, do not approve emptiness.**
    ///
    /// A pattern matching nothing would produce a zero-file `Verified`, hence
    /// an empty diff, hence a gate asking to approve "nothing" — and a human
    /// who says yes to nothing learns to say yes. Worse: it is
    /// indistinguishable from a *correct* pattern whose files vanished in
    /// between, and from a typo. All three deserve the same treatment, and
    /// that is a refusal naming the pattern.
    NoFileMatched {
        pattern: String,
    },
    PolicyDenied {
        path: String,
        verdict: String,
    },
    NotConfined {
        path: String,
        reason: String,
    },
    ActionRefused {
        verdict: String,
    },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Artifact(e) => write!(
                f, "asm-ai refused the compiled program: {e}\n  \
                    rule: a module may only import the four asm-ai host functions, must export \
                    `memory` and `run`, and must carry an `x0ne-manifest` section\n  \
                    fix: recompile from asm-ai source; a hand-written module is never accepted"
            ),
            Refused::BadPattern { pattern, reason } => write!(
                f, "asm-ai refused the file pattern `{pattern}`: {reason}\n  \
                    rule: patterns are globset syntax, anchored at the project root\n  \
                    fix: rewrite the `from files` line with a plain glob"
            ),
            Refused::RegexTooLong { chars, max } => write!(
                f, "asm-ai refused the program: it carries a {chars}-character regex, the limit \
                    is {max}\n  \
                    rule: every pattern is measured before it is compiled, because the cost of \
                    compiling one is not bounded by its length alone\n  \
                    fix: shorten the pattern in `where` or in the verb"
            ),
            Refused::CapabilityMismatch { manifest_claims_write, imports_edit } => write!(
                f, "asm-ai refused the compiled program: its manifest claims write access on at \
                    least one pattern = {manifest_claims_write}, while its import section asks \
                    for the host's editing function = {imports_edit}\n  \
                    rule: what a program may touch is derived from BOTH its imports and its \
                    manifest, and the two must agree — otherwise what you are shown before \
                    approving is not what would run\n  \
                    fix: recompile from asm-ai source; a hand-edited module is never accepted"
            ),
            Refused::UnboundedMemory { pages, maximum } => write!(
                f, "asm-ai refused the compiled program: its memory is declared min {pages:?} \
                    max {maximum:?}, so it can grow\n  \
                    rule: a module's memory budget is decided at compile time by its verb, not \
                    at run time by the module; minimum and maximum must be equal\n  \
                    fix: recompile from asm-ai source; a hand-edited module is never accepted"
            ),
            Refused::TooManyFiles { found, max } => write!(
                f, "asm-ai refused the program: the pattern expands to {found} files, the limit is {max}\n  \
                    rule: the file set is proved one path at a time, and the proof is bounded\n  \
                    fix: narrow the `from files` pattern, or split the work into several programs"
            ),
            Refused::NoFileMatched { pattern } => write!(
                f, "asm-ai refused the program: the pattern `{pattern}` matches no file\n  \
                    rule: a program is proved against the files it would touch, and a file set \
                    that is empty proves nothing — asm-ai refuses it rather than showing you an \
                    empty diff to approve\n  \
                    fix: check the path and the extension in `from files`; patterns are relative \
                    to the project root, and `**/` is needed to descend into subdirectories"
            ),
            Refused::PolicyDenied { path, verdict } => write!(
                f, "asm-ai refused the program at `{path}`: {verdict}\n  \
                    rule: every file the program can touch is checked against the runtime policy \
                    individually, before anything runs\n  \
                    fix: restrict the `from files` pattern to paths the policy allows, or ask the \
                    operator to widen the policy"
            ),
            Refused::NotConfined { path, reason } => write!(
                f, "asm-ai refused the program at `{path}`: {reason}\n  \
                    rule: every path is resolved and confined before it is proved\n  \
                    fix: use a pattern that stays inside the project"
            ),
            Refused::ActionRefused { verdict } => write!(
                f, "asm-ai refused to compile: {verdict}\n  \
                    rule: running programs is a policy decision taken before any source is read\n  \
                    fix: the operator must enable `RunProgram` in the runtime policy"
            ),
        }
    }
}

/// Asks the engine what it thinks of `RunProgram` **before** compiling
/// anything.
///
/// This is not the `PreviewProgram` gate — that one has already passed, in
/// `PolicyEngine::evaluate`, before the executor calls `preview`. It is an
/// **honest saving**: why expand a pattern, open forty files and compute a
/// diff that the policy will never let be applied?
/// The refusal comes right away and names what the operator must enable.
///
/// **What this saving does not do**, measured on 2026-08-15:
/// `PolicyEngine::evaluate` handles the mode **before** the per-action switch
/// (`policy.rs`, "Mode: Readonly & Confirm" then "1. Per-action enable
/// switch"). In `confirm` mode — the `PolicyConfig` default — a modifying
/// action returns `RequireConfirmation`, and a `RunProgram = false` cuts
/// nothing there: the preview continues and reads the files. The saving bites
/// in `readonly` mode, and in permissive mode where the switch is consulted.
/// `readonly_mode_stops_a_preview_before_it_reads_anything` pins both
/// halves, the one that works and the one that does not.
pub fn program_execution_is_permitted(policy: &PolicyEngine, source: &str) -> Result<(), Refused> {
    match policy.evaluate(&Action::RunProgram {
        source: source.to_string(),
    }) {
        PolicyEffect::Allow | PolicyEffect::RequireConfirmation(_) => Ok(()),
        PolicyEffect::Deny(reason) => Err(Refused::ActionRefused {
            verdict: reason.message(),
        }),
    }
}

pub fn verify(
    module: &[u8],
    policy: &PolicyEngine,
    config: &AppConfig,
) -> Result<Verified, Refused> {
    // SECURITY: two artifact refusals get their own variant here rather than
    // drowning in `Refused::Artifact`. They are detected by the reader —
    // they are artifact properties, so they belong to the pure crate, where
    // corpus C bites them with no policy and no disk — but they are the two
    // whose message matters most at the gate, because they say the program
    // shown is not the program compiled.
    let facts = read_module(module).map_err(|e| match e {
        ReadError::CapabilityMismatch {
            manifest_claims_write,
            imports_edit,
        } => Refused::CapabilityMismatch {
            manifest_claims_write,
            imports_edit,
        },
        ReadError::UnboundedMemory { pages, maximum } => {
            Refused::UnboundedMemory { pages, maximum }
        }
        other => Refused::Artifact(other),
    })?;

    // SECURITY: the manifest regexes come from the **artifact**, not the
    // parser. A lying compiler can put a one-megabyte pattern there that the
    // parser never saw, and the host is what would compile it. The
    // **number** of patterns is bounded by `manifest::validate` — bounding
    // one without the other bounds nothing.
    for rx in &facts.manifest.regexes {
        let chars = rx.pattern.chars().count();
        if chars > REGEX_PATTERN_CHARS {
            return Err(Refused::RegexTooLong {
                chars,
                max: REGEX_PATTERN_CHARS,
            });
        }
    }

    // ── Phase A: expansion, dedup, strongest capability ─────────────────────
    //
    // SECURITY: **a concrete path appears only once.** Without dedup, two
    // manifest entries on the same pattern — one `Read`, the other
    // `ReadWrite` — produced two `VerifiedFile` for a single file:
    // "4 files" shown at the gate for 2 real files, inflatable up to
    // ×`MAX_MANIFEST_ENTRIES`, and the same path listed both as read and as
    // write. A proven number that does not count files proves nothing.
    //
    // Dedup rather than refuse duplicated patterns: two **distinct** patterns
    // overlap quite legitimately (`src/**/*.rs` and `**/lib.rs`), and
    // refusing literal equality would have closed only the crudest case while
    // leaving the hole open. The capability kept is the **strongest**, with
    // the entry that granted it: that is the one the host will query, so it is
    // the one that describes what can happen to the file.
    let mut claims: std::collections::BTreeMap<String, (usize, Capability)> =
        std::collections::BTreeMap::new();
    // SECURITY: "never silent" doctrine. A symbolic link matching the pattern
    // is excluded — `resolve_write` would refuse it anyway, and following it
    // would leave the tree — but excluding it **without saying so** would
    // announce "3 files" where the pattern designates 4. The count travels
    // up to the preview payload.
    let mut symlinks_skipped = 0usize;
    // The same normalized roots `resolve_read`/`resolve_write` will use in
    // phase B. Here they serve to **order the refusals**, not to replace
    // them: a path outside the roots must be told it is outside the roots,
    // not "too many files" because 512 others preceded it in the walk.
    // Measured: without this check, a `/etc/**` manifest returned
    // `TooManyFiles` — a correct refusal, with a message that helps no one.
    let read_roots = confine::normalize_roots(&config.policy.allowed_read_paths);
    let write_roots = confine::normalize_roots(&config.policy.allowed_write_paths);

    for (idx, entry) in facts.manifest.entries.iter().enumerate() {
        // SECURITY: `literal_separator(true)`. The `globset` default lets `*`
        // cross `/`, so `tests/*.rs` would match `tests/a/b/c.rs`: a program
        // would prove, and touch, a whole subtree its author believed
        // excluded. `**/` remains the way to descend, and still matches zero
        // levels.
        let glob = globset::GlobBuilder::new(&entry.pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| Refused::BadPattern {
                pattern: entry.pattern.clone(),
                reason: e.to_string(),
            })?;
        let matcher = glob.compile_matcher();
        // The walk root is the longest literal prefix of the pattern:
        // walking from `/` would cost the whole disk for a pattern that
        // starts with `/w/tests/`.
        let base = literal_prefix(&entry.pattern);
        // Counted **per entry** and independently of dedup: a pattern whose
        // files are all already claimed by another entry did match, and that
        // is not a `NoFileMatched`.
        let mut matched_here = 0usize;

        for found in walkdir::WalkDir::new(&base)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !matcher.is_match(found.path()) {
                continue;
            }
            if found.file_type().is_symlink() {
                symlinks_skipped += 1;
                continue;
            }
            if !found.file_type().is_file() {
                continue;
            }
            matched_here += 1;

            // Canonicalize **before** deduplicating: two different raw paths
            // can designate the same file, and it is the file that is
            // counted, not the way it is written.
            let canonical =
                std::fs::canonicalize(found.path()).map_err(|e| Refused::NotConfined {
                    path: found.path().display().to_string(),
                    reason: e.to_string(),
                })?;
            let key = canonical.display().to_string();

            // The real confinement is in phase B, on the token that will open
            // the file. This one is the **same predicate**, applied to the same
            // canonical path, only so that the refusal comes at the first
            // offending path and names it.
            let (roots, kind) = if entry.capability.may_write() {
                (&write_roots, "write")
            } else {
                (&read_roots, "read")
            };
            if !confine::is_within_roots(&canonical, roots) {
                let reason = if roots.is_empty() {
                    confine::ConfineError::NoRootsConfigured { kind }
                } else {
                    confine::ConfineError::OutsideRoots {
                        path: key.clone(),
                        kind,
                    }
                };
                return Err(Refused::NotConfined {
                    path: key,
                    reason: reason.message(),
                });
            }

            match claims.get_mut(&key) {
                Some(slot) => {
                    if entry.capability.may_write() && !slot.1.may_write() {
                        *slot = (idx, entry.capability);
                    }
                }
                None => {
                    claims.insert(key, (idx, entry.capability));
                    if claims.len() > MAX_FILES {
                        return Err(Refused::TooManyFiles {
                            found: claims.len(),
                            max: MAX_FILES,
                        });
                    }
                }
            }
        }

        // SECURITY: **per entry**, not globally. A two-pattern manifest where
        // one matches nothing describes a program half of which is dead: the
        // human would see a diff that only speaks of the other half, and the
        // number shown would not say which. Refuse, and name the pattern.
        if matched_here == 0 {
            return Err(Refused::NoFileMatched {
                pattern: entry.pattern.clone(),
            });
        }
    }

    // ── Phase B: confinement and proof, once per file ───────────────────────
    let mut files: Vec<VerifiedFile> = Vec::with_capacity(claims.len());
    for (raw, (manifest_idx, capability)) in claims {
        // The confinement token first: it resolves, canonicalizes and
        // checks identity. It is the same one that will open the file, so
        // the gate and the execution cannot diverge.
        let confined = if capability.may_write() {
            confine::resolve_write(&raw, &config.policy)
        } else {
            confine::resolve_read(&raw, &config.policy)
        }
        // `message()` and not `{e:?}`: this text goes to a model, which repairs
        // three times better with a sentence than with a variant's `Debug`.
        .map_err(|e| Refused::NotConfined {
            path: raw.clone(),
            reason: e.message(),
        })?;

        // SECURITY: **one synthetic action per path**, submitted to the real
        // engine. This is what populates `paths_read`/`paths_written` as
        // *proven*, and what dissolves glob-versus-prefix.
        //
        // Two probes for a write entry, and it is not belt-and-braces:
        // `fs_edit_buffered` **reads** the file then buffers its rewrite, so
        // an `edit` is a `ReadFile` and a `WriteFile`. It also has a measured
        // consequence: in `confirm` mode, `evaluate` returns
        // `RequireConfirmation` for a modifying action **before** reaching
        // `matches_deny_pattern` and the allowlist; the `WriteFile` probe
        // alone would thus have "proven" a `~/.ssh/id_rsa` located inside the
        // write roots. The `ReadFile` probe is not modifying and goes through
        // these checks in every mode.
        let mut probes: Vec<Action> = Vec::with_capacity(2);
        probes.push(Action::ReadFile {
            path: confined.display(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        });
        if capability.may_write() {
            probes.push(Action::WriteFile {
                path: confined.display(),
                content: String::new(),
            });
        }
        for probe in &probes {
            match policy.evaluate(probe) {
                PolicyEffect::Allow => {}
                PolicyEffect::RequireConfirmation(msg) => {
                    // Confirmation is the gate of the whole program, not of
                    // one file: at the path level, "it would need asking" is a
                    // conditional green light, and the light is already
                    // requested.
                    tracing::debug!(
                        target: "x0ne-runtime::asm",
                        path = %confined.display(),
                        %msg,
                        "path would confirm"
                    );
                }
                PolicyEffect::Deny(reason) => {
                    return Err(Refused::PolicyDenied {
                        path: confined.display(),
                        verdict: reason.message(),
                    })
                }
            }
        }

        files.push(VerifiedFile {
            path: confined,
            manifest_idx,
            capability,
        });
    }

    // Stable order: the diff and the proof header list files, and a list
    // whose order changes between two runs makes the diff unreadable.
    // The `BTreeMap` already returns its keys sorted; the explicit sort pins
    // the contract rather than depending on the chosen container.
    files.sort_by_key(|f| f.path.display());

    Ok(Verified {
        facts,
        files,
        symlinks_skipped,
        module: module.to_vec(),
    })
}

/// The longest directory prefix with no glob metacharacter.
fn literal_prefix(pattern: &str) -> String {
    let mut base = std::path::PathBuf::from("/");
    for seg in pattern.split('/').skip(1) {
        if seg.contains(['*', '?', '[', '{']) {
            break;
        }
        base.push(seg);
    }
    base.display().to_string()
}

/// The root the `from files` patterns are anchored to.
///
/// SECURITY: a wrong anchoring produces **refusals**, never extra capability
/// — each expanded path then goes again through `resolve_read`/`resolve_write`
/// **and** through `PolicyEngine::evaluate`. That is what makes it acceptable
/// to take it from the environment: at worst, a pattern matches nothing
/// (`NoFileMatched`) or falls outside the roots (`NotConfined`).
///
/// `X0NE_ASM_ROOT` exists for benches and scenarios, which run from the
/// repository root but whose policy only opens a disposable tree. Without it,
/// no program could succeed in the scenario harness, and a happy path that
/// cannot be tested is a happy path that is not tested.
pub fn project_root() -> std::io::Result<String> {
    let raw = match std::env::var("X0NE_ASM_ROOT") {
        Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::env::current_dir()?,
    };
    Ok(raw.canonicalize()?.display().to_string())
}

/// What a preview returns, after capping.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bounded {
    pub diff: String,
    pub table: Vec<(String, usize, String)>,
    pub emitted: String,
    /// SECURITY: truncation is **declared**. An output cut silently is an
    /// output that lies by omission — the model reading it concludes the file
    /// ends there, and the approving human believes they saw everything.
    pub truncated: bool,
    pub omitted_bytes: usize,
}

/// SECURITY: caps `diff + table + emitted` **in aggregate** at
/// `x0ne_actions::MAX_RESULT_BYTES`.
///
/// In aggregate, because three separate caps are bypassed by spreading: a
/// 250 Ki `read → table` plus a 250 Ki `emit` makes 500 Ki in the context,
/// each under its bound. The review's exploit is
/// `from files "**/*"` + `read lines /./` + `emit table`: a legal program,
/// that writes nothing, passes the verifier, and whose output is the whole
/// repository.
///
/// The capping order states the priority: the **diff** first (it is the
/// approved object, cutting it would cut the question itself), the **table**
/// next, the **emitted** last — those bytes are controlled by the module,
/// hence the least trustworthy of the three.
pub fn bound_result(diff: &str, table: &[(String, usize, String)], emitted: &str) -> Bounded {
    let max = x0ne_actions::MAX_RESULT_BYTES;
    let mut budget = max;
    let mut omitted = 0usize;

    let (diff_out, cut) = take_bytes(diff, budget);
    budget -= diff_out.len();
    omitted += cut;

    // SECURITY: the cost is the line's **real serialized size**, not
    // `path.len() + text.len()`. What goes to the model is JSON: quotes,
    // commas, the line number and above all escapes (`\n`, `\"`, `\uXXXX`)
    // are context bytes like any others, and a text full of line breaks
    // doubles its size in JSON. Underestimating the cost is a bound believed
    // enforced that lets more through than its number.
    //
    // And a `break`, not a `continue`: a table with holes — line 12 kept,
    // 13 skipped, 14 kept because it is shorter — reads as a complete
    // table. The model would conclude the file has no line 13. The tail is
    // cut, and `omitted_bytes` says how much.
    let mut table_out = Vec::new();
    let mut cut_from = None;
    for (i, row) in table.iter().enumerate() {
        let cost = serialized_cost(row);
        if cost > budget {
            cut_from = Some(i);
            break;
        }
        budget -= cost;
        table_out.push(row.clone());
    }
    if let Some(i) = cut_from {
        omitted += table[i..].iter().map(serialized_cost).sum::<usize>();
    }

    let (emitted_out, cut) = take_bytes(emitted, budget);
    omitted += cut;

    Bounded {
        diff: diff_out,
        table: table_out,
        emitted: emitted_out,
        truncated: omitted > 0,
        omitted_bytes: omitted,
    }
}

/// What a table line really costs in the model's context: its size once
/// serialized. `to_string` on a tuple cannot fail for these three types; the
/// pessimistic bound of `unwrap_or` is there so that the day this changes, the
/// line is excluded rather than counted as zero.
fn serialized_cost(row: &(String, usize, String)) -> usize {
    serde_json::to_string(row)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

/// Cuts on a character boundary — never in the middle of a code point, which
/// would produce a string that `String::from_utf8` refuses.
fn take_bytes(s: &str, budget: usize) -> (String, usize) {
    if s.len() <= budget {
        return (s.to_string(), 0);
    }
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), s.len() - end)
}

/// The `PreviewProgram` action.
///
/// SECURITY: it is classed `ReadOnly`, hence not gated — and that is why it
/// must be **switchable**, which it is because it is a named action:
/// `PolicyEngine::evaluate` has already consulted the per-action switch
/// (`policy.rs:165-172`) before this body runs. The contrast is the old
/// design: `Simulate { target: RunProgram }` would have been exempted from
/// the whole gate by `ALWAYS_ALLOW`.
///
/// This body therefore does NOT re-test `PreviewProgram` — the executor already
/// did. It tests `RunProgram`: why read forty files and compute a diff that the
/// policy will never let be applied? An immediate refusal saves the work and
/// tells the operator what to enable.
///
/// **State at T3:** nothing is executed. The body compiles, verifies, and
/// returns the list of **proven** paths. `bounded` is empty and `value` is
/// `None`: T6 plugs buffered execution in there (diff, table, value), T8 the
/// pending transaction. The shape of the return value no longer changes.
pub fn preview(
    source: &str,
    policy: &PolicyEngine,
    config: &AppConfig,
) -> crate::result::ActionResult {
    use crate::result::{ActionOutput, ActionResult, ErrorKind};

    if let Err(r) = program_execution_is_permitted(policy, source) {
        return ActionResult::err("PreviewProgram", ErrorKind::PolicyDenied, r.to_string(), 0);
    }
    let program = match x0ne_asm::parse(source) {
        Ok(p) => p,
        Err(refusal) => {
            return ActionResult::err(
                "PreviewProgram",
                ErrorKind::InvalidInput,
                refusal.render(),
                0,
            )
        }
    };
    let root = match project_root() {
        Ok(p) => p,
        Err(e) => {
            return ActionResult::err("PreviewProgram", ErrorKind::Internal, e.to_string(), 0)
        }
    };
    let module = match x0ne_asm::emit::compile(&program, &root) {
        Ok(m) => m,
        Err(refusal) => {
            return ActionResult::err(
                "PreviewProgram",
                ErrorKind::InvalidInput,
                refusal.render(),
                0,
            )
        }
    };
    let verified = match verify(&module, policy, config) {
        Ok(v) => v,
        Err(r) => {
            return ActionResult::err("PreviewProgram", ErrorKind::PolicyDenied, r.to_string(), 0)
        }
    };

    // T3 stops here: the paths are proven, nothing has been executed. T6
    // replaces this block with a call to `run_buffered`, T8 adds the pending
    // transaction. The shape of the return value no longer changes.
    let mut summary = crate::result::SideEffectSummary::default();
    for f in &verified.files {
        if f.capability.may_write() {
            summary.paths_written.push(f.path.display());
        } else {
            summary.paths_read.push(f.path.display());
        }
    }
    let bounded = bound_result("", &[], "");
    ActionResult::ok(
        "PreviewProgram",
        ActionOutput::ProgramPreview {
            program_hash: crate::scripted::hash_source(source),
            files: verified.files.iter().map(|f| f.path.display()).collect(),
            symlinks_skipped: verified.symlinks_skipped,
            value: None,
            bounded,
            side_effects: summary,
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Tests that touch `X0NE_ASM_ROOT` are serialized: the environment is
    /// process-global, and `cargo test` runs tests in parallel.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn engine_over(root: &std::path::Path) -> (PolicyEngine, AppConfig) {
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec![root.join("tests").display().to_string()];
        cfg.policy.allowed_write_paths = vec![root.join("tests").display().to_string()];
        cfg.actions.insert("RunProgram".into(), true);
        let enabled: HashMap<String, bool> = cfg.actions.clone();
        (PolicyEngine::new(cfg.clone(), enabled), cfg)
    }

    fn compiled(src: &str, root: &std::path::Path) -> Vec<u8> {
        let program = x0ne_asm::parse(src).expect("the program is legal");
        x0ne_asm::emit::compile(&program, &root.display().to_string()).expect("compile")
    }

    /// SECURITY: verification submits **one synthetic action per concrete
    /// path** to the real policy engine.
    ///
    /// This is spec decision C5, and it dissolves two ambiguities at once:
    /// glob-versus-prefix, and deny-pattern-versus-glob. The number shown in
    /// the proof header becomes a **proven** number — no more mixing of
    /// proven and observed.
    ///
    /// A failure here means the gate announces paths the policy never saw.
    #[test]
    fn every_expanded_path_is_submitted_to_the_policy_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for n in ["a", "b", "c"] {
            std::fs::write(
                root.join("tests").join(format!("{n}.rs")),
                "fn test_x() {}\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("src").join("lib.rs"), "fn main() {}\n").unwrap();

        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\nedit replace /fn test_(\\w+)/ -> \"fn ${1}_property\"\nemit diff\n",
            &root,
        );

        let (policy, cfg) = engine_over(&root);
        let v = verify(&module, &policy, &cfg).expect("the program is inside the roots");
        assert_eq!(v.files.len(), 3, "three proven files, not a pattern");
        assert!(v.files.iter().all(|f| f.capability.may_write()));
        assert!(
            v.files
                .iter()
                .all(|f| f.path.as_path().starts_with(root.join("tests"))),
            "`src/lib.rs` is not in the expansion"
        );
        // The order is stable: the diff and the proof header list these
        // paths, and a list whose order changes makes the diff unreadable.
        let mut sorted: Vec<String> = v.files.iter().map(|f| f.path.display()).collect();
        let seen = sorted.clone();
        sorted.sort();
        assert_eq!(seen, sorted);
        // SECURITY: the proven bytes are the ones kept — invariant I-B.
        assert_eq!(v.module, module, "the host will receive these exact bytes");
    }

    /// A pattern that leaves the roots is refused **per path**, with the
    /// offending path named: that is what the LLM repairer needs to know.
    ///
    /// The refusal comes from **confinement**, not from the policy engine, and
    /// that is the intended order: the confinement token is taken first
    /// because it is the same one that will open the file — so the gate and
    /// the execution cannot diverge. `PolicyDenied` covers paths that are
    /// *inside* the roots and that the policy refuses anyway
    /// (see `a_write_path_matching_a_deny_pattern_is_refused_even_in_confirm_mode`).
    #[test]
    fn a_path_outside_the_write_roots_is_named_in_the_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("lib.rs"), "x\n").unwrap();

        let module = compiled(
            "#!asm-ai/v0\nfrom files \"src/**/*.rs\"\nedit replace /x/ -> \"y\"\nemit diff\n",
            &root,
        );
        let (policy, cfg) = engine_over(&root); // only `tests/**` is open
        match verify(&module, &policy, &cfg) {
            Err(Refused::NotConfined { path, .. }) => {
                assert!(path.ends_with("src/lib.rs"), "got {path}")
            }
            other => panic!("expected a path refusal, got {other:?}"),
        }
    }

    /// SECURITY: a write path matching a deny pattern is refused **even in
    /// `confirm` mode**, which is the default mode.
    ///
    /// Measured on 2026-08-15: `PolicyEngine::evaluate` handles the mode before
    /// the paths (`policy.rs`, "Mode: Readonly & Confirm", before "3–4.
    /// Path-level checks"). In `confirm` mode, a modifying action returns
    /// `RequireConfirmation` and **never reaches** `matches_deny_pattern` nor
    /// the read allowlist. A write manifest entry submitted to `WriteFile`
    /// alone would thus have "proven" a `~/.ssh/id_rsa` located in the write
    /// roots, and the gate would have displayed it as an approved file.
    ///
    /// Hence **two probes** per write path, and it is not a workaround:
    /// `fs_edit_buffered` reads the file then buffers its rewrite. An `edit`
    /// is a `ReadFile` **and** a `WriteFile`, and submitting both describes
    /// what will happen.
    ///
    /// A failure here means an operator deny pattern is inert for asm-ai in
    /// the default configuration.
    #[test]
    fn a_write_path_matching_a_deny_pattern_is_refused_even_in_confirm_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        // `**/id_rsa*` is in `deny_path_patterns` by default.
        std::fs::write(root.join("tests").join("id_rsa.rs"), "x\n").unwrap();

        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\nedit replace /x/ -> \"y\"\nemit diff\n",
            &root,
        );
        let (policy, cfg) = engine_over(&root);
        assert_eq!(cfg.policy.mode, "confirm", "the repository's default mode");
        match verify(&module, &policy, &cfg) {
            Err(Refused::PolicyDenied { path, verdict }) => {
                assert!(path.ends_with("id_rsa.rs"), "got {path}");
                assert!(verdict.contains("deny pattern"), "got {verdict}");
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    /// SECURITY: a **read-only** program is submitted to `ReadFile` alone —
    /// the proof must not ask for more than what the program can do. A
    /// `WriteFile` probe on a `count` would refuse harmless programs in a
    /// repository opened read-only, and would teach the operator to widen the
    /// write roots for nothing.
    #[test]
    fn a_read_only_program_is_never_probed_for_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "x\n").unwrap();

        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n",
            &root,
        );
        let (mut cfg, _) = {
            let (p, c) = engine_over(&root);
            (c, p)
        };
        // No write root: the repository is opened read-only.
        cfg.policy.allowed_write_paths = vec![];
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let v = verify(&module, &policy, &cfg).expect("a `count` writes nothing");
        assert_eq!(v.files.len(), 1);
        assert!(!v.files[0].capability.may_write());
    }

    /// Beyond `MAX_FILES` we **refuse**, we do not truncate. Truncating would
    /// produce a silently partial diff — approved for forty files, applied to
    /// five hundred.
    #[test]
    fn an_expansion_beyond_max_files_is_refused_not_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for i in 0..(x0ne_asm::limits::MAX_FILES + 1) {
            std::fs::write(root.join("tests").join(format!("f{i}.rs")), "x\n").unwrap();
        }
        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n",
            &root,
        );
        let (policy, cfg) = engine_over(&root);
        assert!(matches!(
            verify(&module, &policy, &cfg),
            Err(Refused::TooManyFiles { .. })
        ));
    }

    /// SECURITY: an empty expansion is a **refusal**, not an approval of
    /// nothing.
    ///
    /// Without this refusal, a pattern matching nothing would produce a
    /// zero-file `Verified`, hence an empty diff, hence a gate asking to
    /// approve "nothing" — and a human made to say yes to nothing learns to
    /// say yes. Three very different causes give the same empty expansion: a
    /// typo in the pattern, a `*` where `**` was needed, and files that really
    /// disappeared. All three deserve to be said.
    ///
    /// A failure here means an approval is requested on an empty set.
    #[test]
    fn a_pattern_that_matches_nothing_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "fn test_x() {}\n").unwrap();

        // The directory exists, the pattern is well formed, and yet no
        // `.py` file lives there.
        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.py\"\ncount files\nemit value\n",
            &root,
        );
        let (policy, cfg) = engine_over(&root);
        match verify(&module, &policy, &cfg) {
            Err(Refused::NoFileMatched { pattern }) => {
                assert!(pattern.ends_with("tests/**/*.py"), "got {pattern}")
            }
            other => panic!("expected NoFileMatched, got {other:?}"),
        }
    }

    /// SECURITY: `**/` descends, `*` does not. `globset` decides it through
    /// `literal_separator`, and the crate default is **wrong** for a file
    /// pattern: without it, `tests/*.rs` matches `tests/a/b/c.rs`, so a
    /// program would prove — and touch — a whole subtree its author believed
    /// excluded.
    ///
    /// A failure here means a capability wider than what the program states.
    #[test]
    fn a_single_star_does_not_cross_a_directory_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests").join("deep")).unwrap();
        std::fs::write(root.join("tests").join("top.rs"), "x\n").unwrap();
        std::fs::write(root.join("tests").join("deep").join("low.rs"), "x\n").unwrap();

        let (policy, cfg) = engine_over(&root);

        let flat = compiled(
            "#!asm-ai/v0\nfrom files \"tests/*.rs\"\ncount files\nemit value\n",
            &root,
        );
        let v = verify(&flat, &policy, &cfg).expect("`top.rs` matches");
        assert_eq!(v.files.len(), 1, "`*` does not cross `/`");
        assert!(v.files[0].path.display().ends_with("top.rs"));

        let deep = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n",
            &root,
        );
        let v = verify(&deep, &policy, &cfg).expect("both match");
        assert_eq!(
            v.files.len(),
            2,
            "`**/` descends, and also matches zero levels"
        );
    }

    /// SECURITY: an **absolute pattern outside the roots** is refused. It
    /// passes the manifest decoder — it is absolute and does not go up — so it
    /// is indeed the policy, and the policy alone, that stops it. This is the
    /// case corpus C cannot carry: it requires a `PolicyEngine`.
    #[test]
    fn an_absolute_pattern_outside_the_roots_is_refused_by_the_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "x\n").unwrap();

        // The module is forged: its manifest claims `/etc/**`, which no asm-ai
        // program can write (R11 refuses the absolute glob at the parser).
        // This is the lying compiler of the threat model.
        let manifest = x0ne_asm::manifest::Manifest {
            version: x0ne_asm::limits::MANIFEST_VERSION,
            entries: vec![x0ne_asm::manifest::ManifestEntry {
                pattern: "/etc/**".into(),
                capability: x0ne_asm::manifest::Capability::Read,
            }],
            regexes: vec![],
            templates: vec![],
        };
        let module = forged_module(&manifest);
        assert!(
            x0ne_asm::read::read_module(&module).is_ok(),
            "the artifact reader does not judge the roots: the policy does"
        );

        let (policy, cfg) = engine_over(&root);
        match verify(&module, &policy, &cfg) {
            Err(Refused::NotConfined { path, .. }) => assert!(path.starts_with("/etc/")),
            // Depending on what `/etc` contains on the machine, the first file
            // encountered may also be refused by a deny pattern.
            Err(Refused::PolicyDenied { path, .. }) => assert!(path.starts_with("/etc/")),
            Err(Refused::NoFileMatched { pattern }) => {
                panic!("`/etc/**` should match something, got {pattern}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// SECURITY: a **manifest** regex is measured too.
    ///
    /// The parser bounds the pattern to `REGEX_PATTERN_CHARS` in the program
    /// *text* (R14). That bounds nothing at all for a forged artifact: its
    /// regexes arrive directly in the custom section, never going through the
    /// parser, and the host is what would compile them. The verifier is the
    /// only place where both paths meet.
    ///
    /// A failure here means a forged module makes the host compile a pattern
    /// nobody measured.
    #[test]
    fn a_forged_manifest_regex_beyond_the_length_bound_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "x\n").unwrap();

        let long = "a".repeat(REGEX_PATTERN_CHARS + 1);
        let manifest = x0ne_asm::manifest::Manifest {
            version: x0ne_asm::limits::MANIFEST_VERSION,
            entries: vec![x0ne_asm::manifest::ManifestEntry {
                pattern: root
                    .join("tests")
                    .join("**")
                    .join("*.rs")
                    .display()
                    .to_string(),
                capability: x0ne_asm::manifest::Capability::Read,
            }],
            regexes: vec![x0ne_asm::ir::RegexLit {
                pattern: long,
                case_insensitive: false,
            }],
            templates: vec![],
        };
        let module = forged_module(&manifest);
        let (policy, cfg) = engine_over(&root);
        match verify(&module, &policy, &cfg) {
            Err(Refused::RegexTooLong { chars, max }) => {
                assert_eq!(chars, REGEX_PATTERN_CHARS + 1);
                assert_eq!(max, REGEX_PATTERN_CHARS);
            }
            other => panic!("expected RegexTooLong, got {other:?}"),
        }

        // And the legal length passes: the bound refuses what exceeds it, not
        // what reaches it.
        let mut ok_manifest = manifest.clone();
        ok_manifest.regexes[0].pattern = "a".repeat(REGEX_PATTERN_CHARS);
        assert!(verify(&forged_module(&ok_manifest), &policy, &cfg).is_ok());
    }

    /// A pure module — no import, no claim — has **nothing** to prove, and is
    /// therefore not refused for empty expansion. `NoFileMatched` speaks of a
    /// pattern matching nothing; a manifest with no pattern is not the same
    /// object, and conflating them would refuse the case the pitch puts
    /// forward.
    #[test]
    fn a_pure_module_proves_nothing_and_is_not_a_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let program =
            x0ne_asm::parse("#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n")
                .unwrap();
        let module = x0ne_asm::emit::compile_pure(&program).unwrap();
        let (policy, cfg) = engine_over(&root);
        let v = verify(&module, &policy, &cfg).expect("a pure module passes");
        assert!(v.files.is_empty());
        assert!(v.facts.imports.is_empty());
    }

    /// An artifact the reader refuses never becomes a `Verified` — the
    /// refusal travels up as is, with the reader's message inside.
    #[test]
    fn an_artifact_the_reader_refuses_never_reaches_the_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (policy, cfg) = engine_over(&root);
        assert!(matches!(
            verify(b"not a wasm module at all", &policy, &cfg),
            Err(Refused::Artifact(_))
        ));
    }

    /// SECURITY: the two output bounds state the same number.
    ///
    /// `x0ne_asm::limits::MAX_EMIT_BYTES` bounds what the **module** returns;
    /// `x0ne_actions::MAX_RESULT_BYTES` bounds what the **preview** returns.
    /// They live in two crates because `x0ne-asm` is pure and does not know
    /// the action vocabulary; `x0ne-runtime` is the only one depending on
    /// both, so this is where equality is checked. Two different caps would
    /// give a bound believed enforced that is only half enforced.
    #[test]
    fn the_two_result_bounds_agree() {
        assert_eq!(
            x0ne_asm::limits::MAX_EMIT_BYTES,
            x0ne_actions::MAX_RESULT_BYTES
        );
    }

    /// The parser's pattern length bound and the host's state the same
    /// number. The parser (T2) refuses a too-long pattern to return a readable
    /// `Refusal`; the host (T4) will refuse it before `RegexBuilder`. Two
    /// different bounds mean a program accepted at compile time and refused
    /// at execution, with a message nobody wrote to be read.
    #[test]
    fn the_two_regex_length_bounds_agree() {
        assert_eq!(REGEX_PATTERN_CHARS, x0ne_asm::limits::REGEX_PATTERN_CHARS);
        let long = "a".repeat(REGEX_PATTERN_CHARS + 1);
        let src = format!(
            "#!asm-ai/v0\nfrom files \"s/*.rs\"\nwhere lines match /{long}/\ncount matches\nemit value\n"
        );
        let refusal = x0ne_asm::parse(&src).expect_err("the parser refuses the overlong pattern");
        assert_eq!(refusal.rule, x0ne_asm::diag::RuleId::RegexTooLong);
    }

    /// SECURITY: what the preview returns to the model is **bounded and says
    /// so**.
    ///
    /// The exploit that motivated this bound: `from files "**/*"` + `read lines /./`
    /// + `emit table` is a perfectly legal program whose output is the whole
    /// repository. It passes the verifier (all paths are in the read roots),
    /// it writes nothing, and it would dump everything into the model's
    /// context. Rule R09 closed the *bare* read; this one closes the *bulky*
    /// read.
    ///
    /// Truncation is **declared**: `truncated` and `omitted_bytes` travel in
    /// the payload. An output cut without saying so is an output that lies by
    /// omission, and the model reading it would conclude the file ends there.
    ///
    /// A failure here means a program can dump a whole repository into a
    /// context, or cuts it silently.
    #[test]
    fn a_preview_never_returns_more_than_max_result_bytes_to_the_model() {
        let big = "x".repeat(x0ne_actions::MAX_RESULT_BYTES);
        let bounded = bound_result(&big, &[("a.rs".into(), 1, big.clone())], &big);
        let total = bounded.diff.len()
            + bounded.emitted.len()
            + bounded
                .table
                .iter()
                .map(|(p, _, l)| p.len() + l.len())
                .sum::<usize>();
        assert!(
            total <= x0ne_actions::MAX_RESULT_BYTES,
            "{total} bytes returned, the bound is {}",
            x0ne_actions::MAX_RESULT_BYTES
        );
        assert!(bounded.truncated, "the truncation is declared");
        assert!(bounded.omitted_bytes > 0, "and it says how much it cut");

        // And the nominal case does not lie either: nothing cut, nothing declared.
        let small = bound_result("a diff", &[], "");
        assert!(!small.truncated);
        assert_eq!(small.omitted_bytes, 0);
        assert_eq!(small.diff, "a diff");
    }

    /// SECURITY: the cap is **aggregated**, not three separate caps. Three
    /// caps are bypassed by spreading: a 250 Ki `read → table` plus a 250 Ki
    /// `emit` make 500 Ki in the context, each under its bound.
    #[test]
    fn the_three_output_channels_share_one_budget() {
        let max = x0ne_actions::MAX_RESULT_BYTES;
        let half = "y".repeat(max / 2 + 1);
        let bounded = bound_result(&half, &[("p".into(), 1, half.clone())], &half);
        let total = bounded.diff.len()
            + bounded.emitted.len()
            + bounded
                .table
                .iter()
                .map(|(p, _, l)| p.len() + l.len())
                .sum::<usize>();
        assert!(total <= max, "{total} > {max}");
        assert!(bounded.truncated);
        // The priority is written down: the diff is the approved object, it passes whole.
        assert_eq!(bounded.diff.len(), half.len());
    }

    /// The cut falls on a character boundary. Cutting in the middle of a code
    /// point would produce a string that `String::from_utf8` refuses — hence a
    /// preview that fails on a file with accented characters.
    #[test]
    fn a_truncated_output_is_still_valid_utf8() {
        let s = "é".repeat(x0ne_actions::MAX_RESULT_BYTES);
        let bounded = bound_result(&s, &[], "");
        assert!(bounded.truncated);
        assert!(bounded.diff.len() <= x0ne_actions::MAX_RESULT_BYTES);
        assert!(bounded.diff.chars().all(|c| c == 'é'));
    }

    /// SECURITY: in `readonly` mode, `RunProgram` is refused, and the preview
    /// stops **before** opening anything. Why read forty files to show a diff
    /// that the policy will never let be applied?
    ///
    /// What this test also pins is the **limit** of this saving: in `confirm`
    /// mode — the repository default — `PolicyEngine::evaluate` returns
    /// `RequireConfirmation` for any modifying action **before** looking at
    /// the per-action switch (`policy.rs`, "Mode" before "1. Per-action
    /// enable switch"). A `RunProgram = false` therefore cuts nothing there,
    /// and the preview continues. Saying the opposite in a doc-comment would
    /// promise more than the code holds.
    #[test]
    fn readonly_mode_stops_a_preview_before_it_reads_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (_, mut cfg) = engine_over(&root);

        cfg.policy.mode = "readonly".into();
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        assert!(program_execution_is_permitted(&policy, "#!asm-ai/v0\n").is_err());

        // And the limit, measured: in `confirm`, the switch cuts nothing.
        cfg.policy.mode = "confirm".into();
        cfg.actions.insert("RunProgram".into(), false);
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        assert!(
            program_execution_is_permitted(&policy, "#!asm-ai/v0\n").is_ok(),
            "in confirm mode the mode decides before the switch — measured, not intended"
        );
    }

    /// The preview returns a `ProgramPreview` listing the **proven** paths,
    /// and has written nothing. This is the return contract T6 and T8 will
    /// grow without changing its shape.
    #[test]
    fn a_preview_returns_the_proved_paths_and_writes_nothing() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "fn test_x() {}\n").unwrap();
        let before = std::fs::read_to_string(root.join("tests").join("a.rs")).unwrap();

        std::env::set_var("X0NE_ASM_ROOT", root.display().to_string());
        let (policy, cfg) = engine_over(&root);
        let src = "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\nedit replace /test_/ -> \"prop_\"\nemit diff\n";
        let result = preview(src, &policy, &cfg);
        std::env::remove_var("X0NE_ASM_ROOT");

        assert!(result.success, "got {:?}", result.error);
        assert_eq!(result.action_type, "PreviewProgram");
        match result.output {
            Some(crate::result::ActionOutput::ProgramPreview {
                files,
                side_effects,
                bounded,
                program_hash,
                ..
            }) => {
                assert_eq!(files.len(), 1);
                assert!(files[0].ends_with("a.rs"));
                assert_eq!(side_effects.paths_written, files);
                assert!(side_effects.paths_read.is_empty());
                assert!(!bounded.truncated);
                assert_eq!(program_hash, crate::scripted::hash_source(src));
            }
            other => panic!("expected ProgramPreview, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(root.join("tests").join("a.rs")).unwrap(),
            before,
            "SECURITY: a preview writes nothing"
        );
    }

    /// A source that is not a program is refused by the parser, and the
    /// refusal that travels up is the parser's — three lines, with the rule
    /// and the lever. That is what the LLM repairer reads.
    #[test]
    fn a_preview_of_an_illegal_program_returns_the_parser_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (policy, cfg) = engine_over(&root);
        let result = preview("not a program\n", &policy, &cfg);
        assert!(!result.success);
        let msg = result.error.expect("a message").message;
        assert!(msg.contains("asm-ai R01"), "got {msg}");
        assert!(msg.contains("rule:") && msg.contains("fix:"), "got {msg}");
    }

    /// `project_root` takes `X0NE_ASM_ROOT` when it is set, and the current
    /// directory otherwise. In both cases the value is **canonicalized**: a
    /// pattern anchored on a non-canonical path would designate files that
    /// confinement speaks of in canonical form.
    #[test]
    fn the_project_root_is_canonical() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();

        std::env::set_var("X0NE_ASM_ROOT", root.join("a").join("..").join("a"));
        let got = project_root().unwrap();
        std::env::remove_var("X0NE_ASM_ROOT");
        assert_eq!(got, root.join("a").display().to_string());

        let cwd = project_root().unwrap();
        assert!(std::path::Path::new(&cwd).is_absolute());
    }

    /// SECURITY: invariant I-2 — **one real file, one entry**.
    ///
    /// Measured before the fix: two manifest entries on the same pattern,
    /// `Read` then `ReadWrite`, returned `files.len() == 4` for 2 files, and
    /// each path appeared both in `paths_read` and in `paths_written`.
    /// Inflatable up to ×`MAX_MANIFEST_ENTRIES`, i.e. ×16.
    /// "N proven files" is the sentence the human reads before approving:
    /// a number that counts claims rather than files proves nothing, and an
    /// inflated number teaches not to read it.
    ///
    /// The capability kept is the **strongest**, with the entry that granted
    /// it — that is the one the host will query (T5), hence the one describing
    /// what can happen to the file.
    #[test]
    fn two_manifest_entries_over_the_same_file_prove_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for n in ["a", "b"] {
            std::fs::write(root.join("tests").join(format!("{n}.rs")), "x\n").unwrap();
        }
        let pattern = root
            .join("tests")
            .join("**")
            .join("*.rs")
            .display()
            .to_string();

        let manifest = x0ne_asm::manifest::Manifest {
            version: x0ne_asm::limits::MANIFEST_VERSION,
            entries: vec![
                x0ne_asm::manifest::ManifestEntry {
                    pattern: pattern.clone(),
                    capability: x0ne_asm::manifest::Capability::Read,
                },
                x0ne_asm::manifest::ManifestEntry {
                    pattern,
                    capability: x0ne_asm::manifest::Capability::ReadWrite,
                },
            ],
            regexes: vec![],
            templates: vec![],
        };
        let module = forged_editor(&manifest);
        let (policy, cfg) = engine_over(&root);
        let v = verify(&module, &policy, &cfg).expect("both entries are inside the roots");

        assert_eq!(v.files.len(), 2, "two real files, not four entries");
        let mut paths: Vec<String> = v.files.iter().map(|f| f.path.display()).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 2, "no path listed twice");
        // The strongest wins, and it keeps the entry that granted it.
        assert!(v.files.iter().all(|f| f.capability.may_write()));
        assert!(v.files.iter().all(|f| f.manifest_idx == 1));

        // And the second entry, whose files were all already claimed, is not
        // a `NoFileMatched`: it did match.
        assert!(verify(&module, &policy, &cfg).is_ok());
    }

    /// The count the preview returns is that of real files, and a path is
    /// never both read and written in it.
    #[test]
    fn the_preview_never_lists_a_path_as_both_read_and_written() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "fn test_x() {}\n").unwrap();

        std::env::set_var("X0NE_ASM_ROOT", root.display().to_string());
        let (policy, cfg) = engine_over(&root);
        let result = preview(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\nedit replace /test_/ -> \"prop_\"\nemit diff\n",
            &policy,
            &cfg,
        );
        std::env::remove_var("X0NE_ASM_ROOT");

        match result.output {
            Some(crate::result::ActionOutput::ProgramPreview {
                files,
                side_effects,
                ..
            }) => {
                assert_eq!(files.len(), 1);
                let both: Vec<_> = side_effects
                    .paths_read
                    .iter()
                    .filter(|p| side_effects.paths_written.contains(p))
                    .collect();
                assert!(both.is_empty(), "a path on both sides: {both:?}");
            }
            other => panic!("expected ProgramPreview, got {other:?}"),
        }
    }

    /// SECURITY: invariant I-1 — the disagreement between manifest and imports
    /// travels up to the gate with **its own** message, not drowned in a
    /// generic `Artifact(...)`. It is the refusal that says the program shown
    /// is not the program compiled.
    #[test]
    fn a_manifest_that_contradicts_the_imports_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let manifest = x0ne_asm::manifest::Manifest {
            version: x0ne_asm::limits::MANIFEST_VERSION,
            entries: vec![x0ne_asm::manifest::ManifestEntry {
                pattern: root.join("tests").join("*.rs").display().to_string(),
                capability: x0ne_asm::manifest::Capability::Read,
            }],
            regexes: vec![],
            templates: vec![],
        };
        // An artifact importing the editor, a manifest claiming only read:
        // the gate would have announced "read".
        let module = forged_editor(&manifest);
        let (policy, cfg) = engine_over(&root);
        match verify(&module, &policy, &cfg) {
            Err(e @ Refused::CapabilityMismatch { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains("imports"), "got {msg}");
                assert!(msg.contains("rule:") && msg.contains("fix:"), "got {msg}");
            }
            other => panic!("expected CapabilityMismatch, got {other:?}"),
        }
    }

    /// SECURITY: invariant I-4 — a memory that can grow is refused, and the
    /// refusal comes **before** any path is expanded.
    ///
    /// Measured before the fix: `the_module_memory_cannot_grow` pinned only
    /// the emitter; `read_module` filled `memory_maximum_pages` and `verify`
    /// never consulted it. A forged `(memory (export "memory") 1)` passed
    /// both.
    #[test]
    fn a_module_whose_memory_can_grow_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "x\n").unwrap();

        let manifest = x0ne_asm::manifest::Manifest {
            version: x0ne_asm::limits::MANIFEST_VERSION,
            entries: vec![x0ne_asm::manifest::ManifestEntry {
                pattern: root.join("tests").join("*.rs").display().to_string(),
                capability: x0ne_asm::manifest::Capability::Read,
            }],
            regexes: vec![],
            templates: vec![],
        };
        let module = forge(
            r#"(module (memory (export "memory") 1) (func (export "run")))"#,
            &manifest,
        );
        let (policy, cfg) = engine_over(&root);
        match verify(&module, &policy, &cfg) {
            Err(e @ Refused::UnboundedMemory { .. }) => {
                assert!(e.to_string().contains("grow"), "got {e}");
            }
            other => panic!("expected UnboundedMemory, got {other:?}"),
        }
    }

    /// SECURITY: a symbolic link matching the pattern is excluded — following
    /// it would leave the tree — but it is **counted and declared**.
    ///
    /// "Never silent" doctrine: without the counter, a pattern designating
    /// four entries one of which is a link would announce "3 files", and the
    /// difference between the two numbers would be invisible to the approver.
    #[test]
    fn a_skipped_symlink_is_counted_and_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests").join("a.rs"), "x\n").unwrap();
        std::os::unix::fs::symlink(
            root.join("tests").join("a.rs"),
            root.join("tests").join("link.rs"),
        )
        .unwrap();

        let module = compiled(
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n",
            &root,
        );
        let (policy, cfg) = engine_over(&root);
        let v = verify(&module, &policy, &cfg).expect("the real file is proven");
        assert_eq!(v.files.len(), 1, "the link is not a proven file");
        assert_eq!(v.symlinks_skipped, 1, "and it is counted");
    }

    /// SECURITY: the cost of a table line is its **serialized** size.
    ///
    /// What goes to the model is JSON: quotes, commas, line number and above
    /// all escapes. A text full of line breaks doubles its size once encoded,
    /// so `path.len() + text.len()` underestimated the cost — a bound believed
    /// enforced that lets more through than its number.
    #[test]
    fn the_table_budget_counts_the_bytes_that_actually_travel() {
        let max = x0ne_actions::MAX_RESULT_BYTES;
        // Each line is a text of line breaks: its raw size is half its JSON
        // size.
        let row_text = "\n".repeat(1024);
        let rows: Vec<(String, usize, String)> = (0..1024)
            .map(|i| (format!("f{i}.rs"), i, row_text.clone()))
            .collect();
        let bounded = bound_result("", &rows, "");
        let real = serde_json::to_string(&bounded.table).unwrap().len();
        assert!(
            real <= max,
            "{real} serialized bytes actually returned, the bound is {max}"
        );
        assert!(bounded.truncated && bounded.omitted_bytes > 0);
    }

    /// SECURITY: the table is cut **at the tail**, never with holes.
    ///
    /// A table whose line 13 is missing because it was too long, but whose
    /// line 14 is there because it fit, reads as a complete table. The model
    /// would conclude the file has no line 13 — an output that lies by
    /// omission, without even a hint of a cut.
    #[test]
    fn a_truncated_table_is_cut_at_the_tail_and_never_holed() {
        let max = x0ne_actions::MAX_RESULT_BYTES;
        let big = "z".repeat(max / 2);
        let rows = vec![
            ("a.rs".to_string(), 1, big.clone()),
            ("b.rs".to_string(), 2, big.clone()),
            ("c.rs".to_string(), 3, big),
            // Short: it would fit in what remains, and that is exactly what
            // must not be done.
            ("d.rs".to_string(), 4, "x".to_string()),
        ];
        let bounded = bound_result("", &rows, "");
        assert!(bounded.truncated);
        let kept: Vec<usize> = bounded.table.iter().map(|(_, l, _)| *l).collect();
        assert_eq!(
            kept,
            (1..=kept.len()).collect::<Vec<_>>(),
            "the kept lines are a prefix: got {kept:?}"
        );
        assert!(
            !kept.contains(&4) || kept.len() == 4,
            "`d.rs` must not skip over a dropped line"
        );
    }

    /// A WASM module carrying the desired manifest — the lying compiler of the
    /// threat model, without going through the emitter.
    ///
    /// SECURITY: the default form imports **nothing**, so its manifest can
    /// only claim read (invariant I-1). Forging a write claim requires
    /// `forged_editor`, which really imports `fs_edit_buffered` — and that is
    /// the point: an artifact whose manifest and imports tell two stories can
    /// no longer be manufactured.
    fn forged_module(m: &x0ne_asm::manifest::Manifest) -> Vec<u8> {
        forge(
            r#"(module (memory (export "memory") 1 1) (func (export "run")))"#,
            m,
        )
    }

    /// The same, but importing the write function: the only artifact whose
    /// manifest is allowed to claim `ReadWrite`.
    fn forged_editor(m: &x0ne_asm::manifest::Manifest) -> Vec<u8> {
        forge(
            r#"(module
                 (import "x0ne" "fs_list" (func (param i32 i32 i32) (result i32)))
                 (import "x0ne" "fs_edit_buffered" (func (param i32 i32 i32) (result i32)))
                 (memory (export "memory") 1 1)
                 (func (export "run")))"#,
            m,
        )
    }

    fn forge(wat_src: &str, m: &x0ne_asm::manifest::Manifest) -> Vec<u8> {
        let mut bytes = wat::parse_str(wat_src).expect("WAT valide");
        let payload = x0ne_asm::manifest::encode_unchecked(m);
        let name = x0ne_asm::manifest::SECTION_NAME;
        let mut body = Vec::new();
        leb128(&mut body, name.len() as u64);
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(&payload);
        bytes.push(0u8);
        leb128(&mut bytes, body.len() as u64);
        bytes.extend_from_slice(&body);
        bytes
    }

    fn leb128(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
}
