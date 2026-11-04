//! The bounds, named and quantified. v0 values: revisable **in the plan**,
//! never silently.
//!
//! Wasmtime provides the *step* bounds (fuel, epoch). These constants bound
//! the **effects**: host functions do not consume fuel, so the loop that is
//! feared — forty writes turned into four million — is bounded here and
//! nowhere else.

/// Size of the artifact accepted by the verifier.
pub const MAX_MODULE_BYTES: usize = 256 * 1024;
/// Entries of the import section, **all kinds combined** (Func, Memory,
/// Global, Table, Tag) — counting only functions would let an imported
/// memory in.
pub const MAX_IMPORTS: usize = 8;
/// Patterns claimed by the manifest.
pub const MAX_MANIFEST_ENTRIES: usize = 16;
/// SECURITY: regex patterns carried by the manifest. Same reasoning as
/// `REGEX_PATTERN_CHARS`: a **forged** manifest does not go through the parser,
/// so nothing bounds the number of patterns it carries. Measured on
/// 2026-08-15: 400 patterns of 512 characters fit in a 206 KB module
/// and passed the verifier; the host would compile them, i.e. ~400 Mi of
/// regex budget. Bounding the length without bounding the count bounds nothing.
pub const MAX_MANIFEST_REGEXES: usize = 16;
/// SECURITY: replacement templates carried by the manifest. Same reason, and
/// the same number: a thousand `${99}` cost as much as one wants at run time.
pub const MAX_MANIFEST_TEMPLATES: usize = 16;
/// Paths after expansion. **Refusal beyond, never truncation**: truncating
/// would produce a diff that does not say what it left out.
pub const MAX_FILES: usize = 512;
/// One file read or buffered.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Read budget for the whole run.
///
/// **Enforced by the host, in T5** — `fs_read` keeps the running total. At this
/// stage nothing reads it: it is declared here because the value is a
/// decision of the plan, not of the host implementer. Saying so is what
/// distinguishes it from dead scaffolding; forgetting it is what produced the four
/// previous cases.
pub const MAX_BYTES_READ_TOTAL: u64 = 64 * 1024 * 1024;
/// Buffered write budget for the whole run.
///
/// **Enforced by the host, in T5** — `fs_edit_buffered` keeps the running total.
/// Nothing reads it at this stage.
pub const MAX_BYTES_BUFFERED: u64 = 64 * 1024 * 1024;
/// Bytes the module returns to the terminal and to the context, via `emit`.
///
/// SECURITY: must stay **equal** to `x0ne_actions::MAX_RESULT_BYTES`, which bounds
/// the total `diff + table + emitted` returned by a preview. The two
/// channels — the one the module controls and the one the host fills — have no
/// reason to have different caps, and two different caps
/// would give a bound believed to be enforced that is only half enforced.
/// The equality is held by `the_two_result_bounds_agree` (in `x0ne-runtime`,
/// the only crate that depends on both — `x0ne-asm` stays pure and does not know
/// the action vocabulary).
pub const MAX_EMIT_BYTES: usize = 256 * 1024;
/// Pattern length, measured **before** the regex builder.
pub const REGEX_PATTERN_CHARS: usize = 512;
/// `RegexBuilder::size_limit`.
///
/// **Enforced by the host, in T5**, when it compiles the manifest
/// patterns. Nothing reads it at this stage: the verifier bounds the
/// **length** of patterns and their **count**, not the compilation cost, which
/// only the builder knows.
pub const REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// `RegexBuilder::dfa_size_limit`.
///
/// **Enforced by the host, in T5**, same place and same remark as
/// `REGEX_SIZE_LIMIT`.
pub const DFA_SIZE_LIMIT: usize = 8 * 1024 * 1024;

/// Manifest format version. The manifest is versioned because it is
/// read by two components (verifier and host) that must be at the
/// same version — a lenient decoder would make the proof diverge from the execution.
pub const MANIFEST_VERSION: u16 = 1;

// ── Errno table, closed and named ───────────────────────────────────────────
//
// Host functions return `n >= 0` on success and `-E_*` on
// failure. Closed: an unknown code means nothing to the module, which has
// no branch to handle it.
//
// **Returned by the host, in T5.** None of the six is read at this stage — there is
// no host function yet to return them, nor an executed module to
// receive them. They are here because the table must be closed and decided in one
// piece: an errno added piecemeal, while the host is being written, is
// an errno the module cannot handle. `the_errno_table_is_closed_and_distinct`
// holds the property the plan wanted; usage will come with T5.

/// Index out of bounds: file id, manifest index, regex index.
pub const E_BAD_INDEX: i32 = 1;
/// The manifest entry that produced this id does not carry the requested capability.
pub const E_CAPABILITY: i32 = 2;
/// An effects budget is exhausted.
pub const E_BUDGET: i32 = 3;
/// The file is not valid UTF-8 and the verb requires it.
pub const E_UTF8: i32 = 4;
/// The buffer provided by the module is too small.
pub const E_2BIG: i32 = 5;
/// Internal host error.
pub const E_INTERNAL: i32 = 6;

#[cfg(test)]
mod tests {
    /// The two places that bound a pattern's length — the parser (T2,
    /// to return a readable `Refusal`) and the host (T5, before the builder) —
    /// must state the same number. Two different bounds mean a program
    /// accepted at compile time and refused at run time, with a message that
    /// nobody wrote to be read.
    #[test]
    fn the_two_regex_length_bounds_agree() {
        let parse_src = include_str!("parse.rs");
        assert!(
            parse_src.contains("const REGEX_PATTERN_CHARS: usize = 512;"),
            "parse.rs changed its pattern bound without limits.rs following"
        );
        assert_eq!(super::REGEX_PATTERN_CHARS, 512);
    }

    /// The errno table is **closed and distinct**: two equal codes would make
    /// a module say "index out of bounds" where the host meant "budget
    /// exhausted", and the module's error branch would handle the wrong case.
    ///
    /// A failure here means an errno was duplicated while adding one.
    #[test]
    fn the_errno_table_is_closed_and_distinct() {
        use super::*;
        let all = [
            E_BAD_INDEX,
            E_CAPABILITY,
            E_BUDGET,
            E_UTF8,
            E_2BIG,
            E_INTERNAL,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "two errno share a value");
        // Positive: host functions return `-E_*`, so `0` would be
        // indistinguishable from a success and a negative from a double-negated success.
        assert!(all.iter().all(|e| *e > 0), "an errno must be positive");
    }
}
