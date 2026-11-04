//! The IR. One shape, only one: `source → filter? → verb → sink`.
//!
//! There is no loop, no variable, no condition — **the shape of the language is the
//! bound**. This type cannot represent a program that iterates, so no
//! downstream path has to defend against it.

use serde::{Deserialize, Serialize};

/// A complete, typed asm-ai program.
///
/// SECURITY: the deserialization guard holds **two** rules, and only
/// those: the verb × sink matrix (R07) and the line filter required by
/// `count matches` (R08). They are here because they are structural —
/// they concern the combination of fields, not their content.
///
/// Everything else lives in the text parser and **is not replayed** on
/// re-read: a deserialized `Program` may carry a glob that climbs up
/// (`../../etc/**`), an absolute glob, an empty glob, a `${9}` with no matching
/// group or a regex with a backreference. This is measured, pinned by
/// `a_deserialized_program_is_not_revalidated_against_the_text_rules`, and it
/// means one thing for the emitter: a `Program` coming from JSON is not a
/// verified program. The safeguard remains `PolicyEngine::evaluate` on **every
/// expanded path**, never the shape of this type.
///
/// The previous sentence promised "refuses any malformed program". It
/// was wrong on eighteen rules out of twenty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawProgram")]
#[non_exhaustive]
pub struct Program {
    pub source: Glob,
    pub filter: Option<Filter>,
    pub verb: Verb,
    pub sink: Sink,
}

/// The same shape, without the guard: the only role of this type is to carry the
/// fields up to `TryFrom`, which decides whether they make a program.
///
/// SECURITY: `deny_unknown_fields` because an unknown field is either a
/// version this crate does not understand, or someone writing beside the
/// schema — in both cases, accepting it silently amounts to executing a
/// program part of which nobody has read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProgram {
    source: Glob,
    filter: Option<Filter>,
    verb: Verb,
    sink: Sink,
}

impl TryFrom<RawProgram> for Program {
    type Error = String;

    fn try_from(raw: RawProgram) -> Result<Self, Self::Error> {
        Program::new(raw.source, raw.filter, raw.verb, raw.sink)
    }
}

/// The `from files` pattern, as written. Relative to the project root; the
/// resolution to a canonical absolute path belongs to the emitter, not here: this
/// crate does not touch the disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Glob {
    pub raw: String,
}

/// `where` filters at the **file** level, never the line level. `Lines` keeps a file
/// as soon as at least one of its lines matches; it does not restrict the edit to the
/// matched lines — the opposite confusion would produce a diff that does not match
/// what the rule announces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    Name(RegexLit),
    Lines(RegexLit),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verb {
    Replace {
        find: RegexLit,
        template: Template,
    },
    CountFiles,
    /// Requires `where lines match` (rule R08): counting occurrences without saying
    /// which ones makes no sense, and letting the bare form through would teach the
    /// model a shortcut that means nothing.
    CountMatches,
    /// `read lines REGEX`. The bare `read` form is forbidden (R09): it would be
    /// a `cat` over a glob, i.e. exfiltration in one line.
    ReadLines(RegexLit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sink {
    Diff,
    Value,
    Table,
}

/// A regex as written: `/…/` + optional `i` flag. Never compiled
/// here — compilation is bounded three times and lives on the host side (T5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegexLit {
    pub pattern: String,
    pub case_insensitive: bool,
}

/// The replacement, split into literals and group references. Splitting at
/// compile time rather than at run time allows refusing `${3}` on a regex with
/// two groups **before** anything runs (R10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub parts: Vec<TemplatePart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplatePart {
    Literal(String),
    Group(u32),
}

impl Program {
    /// The three legal cells of the verb × sink matrix.
    ///
    /// `edit → diff`: what is approved is a diff. `count → value`: a
    /// number. `read → table`: lines. Everything else is a compile
    /// error, and that is deliberate — `edit → value` would return a count
    /// of writes without showing the writes, which violates "what one approves,
    /// one must be able to read".
    /// Builds a program, or says why it is not one.
    ///
    /// SECURITY: `Program` is `#[non_exhaustive]`, so outside this crate the
    /// struct literal does not compile and this function is the **only**
    /// entry. `x0ne-runtime` is another crate: without this, it could write an
    /// illegal cell by hand with nothing to stop it. What `new` holds
    /// is what `is_well_formed` holds, no more (see the type's SECURITY:).
    pub fn new(
        source: Glob,
        filter: Option<Filter>,
        verb: Verb,
        sink: Sink,
    ) -> Result<Self, String> {
        let p = Program {
            source,
            filter,
            verb,
            sink,
        };
        if !Program::sink_matches_verb(&p.verb, p.sink) {
            return Err(format!(
                "asm-ai has three verb/emit pairs (edit->diff, count->value, read->table); \
                 this program pairs {:?} with emit {:?}",
                p.verb, p.sink
            ));
        }
        if matches!(p.verb, Verb::CountMatches) && !matches!(p.filter, Some(Filter::Lines(_))) {
            return Err(
                "`count matches` requires a `where lines match /…/` clause: counting matches \
                 needs to know what to match"
                    .to_string(),
            );
        }
        Ok(p)
    }

    /// Does the program respect the bounds the language imposes?
    ///
    /// Two conditions, the same ones `parse` enforces: the
    /// cell of the verb × sink matrix (R07) and the line filter
    /// required by `count matches` (R08). This is what deserialization checks.
    ///
    /// What the method cannot do: prevent the **construction** of a
    /// malformed program in Rust, since the fields are public and the emitter
    /// needs to read them. The guard holds on both *entry* paths —
    /// text and JSON —, not against code in this crate that would write an
    /// inconsistent `Program` by hand.
    pub fn is_well_formed(&self) -> bool {
        Program::sink_matches_verb(&self.verb, self.sink)
            && (!matches!(self.verb, Verb::CountMatches)
                || matches!(self.filter, Some(Filter::Lines(_))))
    }

    pub fn sink_matches_verb(verb: &Verb, sink: Sink) -> bool {
        matches!(
            (verb, sink),
            (Verb::Replace { .. }, Sink::Diff)
                | (Verb::CountFiles, Sink::Value)
                | (Verb::CountMatches, Sink::Value)
                | (Verb::ReadLines(_), Sink::Table)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY: the verb × sink matrix is a **security bound** — `edit`
    /// with `emit value` would return a count of writes without showing the
    /// writes, which violates "what one approves, one must be able to read".
    /// The parser enforces it, but callers re-read `Program`s through
    /// `Deserialize`, and that path does not go through the parser: an illegal
    /// cell deserialized there without a word (measured by the review of
    /// 2026-08-15, equal round-trip). An invariant that holds on only one of the
    /// two entry paths is not an invariant.
    #[test]
    fn an_illegal_cell_cannot_be_deserialized() {
        let legal =
            r#"{"source":{"raw":"s/*.rs"},"filter":null,"verb":"CountFiles","sink":"Value"}"#;
        let p: Program = serde_json::from_str(legal).expect("a legal cell reads back");
        assert_eq!(p.sink, Sink::Value);

        let illegal = r#"{"source":{"raw":"s/*.rs"},"filter":null,"verb":{"Replace":{"find":{"pattern":"a","case_insensitive":false},"template":{"parts":[]}}},"sink":"Value"}"#;
        let err = serde_json::from_str::<Program>(illegal)
            .expect_err("edit -> value must not deserialize");
        assert!(
            err.to_string().contains("emit"),
            "the error names the expected sink: {err}"
        );
    }

    /// `count matches` without `where lines match` is refused by the parser
    /// (R08); it must also be refused on re-read, for the same reason.
    #[test]
    fn counting_matches_without_a_line_filter_cannot_be_deserialized() {
        let src =
            r#"{"source":{"raw":"s/*.rs"},"filter":null,"verb":"CountMatches","sink":"Value"}"#;
        serde_json::from_str::<Program>(src)
            .expect_err("`count matches` without a line filter does not read back");

        let with = r#"{"source":{"raw":"s/*.rs"},"filter":{"Lines":{"pattern":"x","case_insensitive":false}},"verb":"CountMatches","sink":"Value"}"#;
        serde_json::from_str::<Program>(with).expect("with the filter, it reads back");
    }

    /// An unknown field was accepted silently. It means either a version
    /// this crate does not understand, or writing beside the schema:
    /// in both cases, executing a program part of which nobody has
    /// read.
    #[test]
    fn an_unknown_field_cannot_be_deserialized() {
        let evil = r#"{"source":{"raw":"s/*.rs"},"filter":null,"verb":"CountFiles","sink":"Value","evil":true}"#;
        let err = serde_json::from_str::<Program>(evil).expect_err("an unknown field is refused");
        assert!(
            err.to_string().contains("evil"),
            "the error names it: {err}"
        );
    }

    /// The guard's **limit**, pinned so that it stops being overestimated.
    ///
    /// These five programs are refused by the text parser (R11, R20, R10,
    /// R12) and accepted by deserialization, which holds only R07 and R08. This
    /// test will fail the day someone widens the guard — and that will be the right
    /// time to fix the type's doc-comment at the same time. It says what
    /// **is**, not what one would like.
    #[test]
    fn a_deserialized_program_is_not_revalidated_against_the_text_rules() {
        for raw in ["../../etc/**", "/etc/shadow", ""] {
            let json = format!(
                r#"{{"source":{{"raw":"{raw}"}},"filter":null,"verb":"CountFiles","sink":"Value"}}"#
            );
            serde_json::from_str::<Program>(&json).unwrap_or_else(|e| {
                panic!("the guard does not check the glob, and no longer claims to: {e}")
            });
        }
        let group = r#"{"source":{"raw":"s"},"filter":null,"verb":{"Replace":{"find":{"pattern":"a","case_insensitive":false},"template":{"parts":[{"Group":9}]}}},"sink":"Diff"}"#;
        serde_json::from_str::<Program>(group).expect("nor group references");
        let backref = r#"{"source":{"raw":"s"},"filter":{"Lines":{"pattern":"(a)\\1","case_insensitive":false}},"verb":"CountFiles","sink":"Value"}"#;
        serde_json::from_str::<Program>(backref).expect("nor backreferences");
    }

    /// `new` is the only entry from outside the crate, so it must refuse what
    /// deserialization refuses — same guard, two doors.
    #[test]
    fn the_constructor_refuses_what_deserialization_refuses() {
        let err = Program::new(
            Glob { raw: "s".into() },
            None,
            Verb::CountMatches,
            Sink::Value,
        )
        .expect_err("`count matches` without a line filter");
        assert!(err.contains("where lines match"), "{err}");

        Program::new(Glob { raw: "s".into() }, None, Verb::CountFiles, Sink::Diff)
            .expect_err("count -> diff is not a cell");
        Program::new(
            Glob { raw: "s".into() },
            None,
            Verb::CountFiles,
            Sink::Value,
        )
        .expect("count -> value is one");
    }

    /// The round-trip of a well-formed program stays the identity — the guard must
    /// not shrink what is legitimate.
    #[test]
    fn a_well_formed_program_survives_a_round_trip() {
        let p = Program {
            source: Glob {
                raw: "s/*.rs".into(),
            },
            filter: Some(Filter::Name(RegexLit {
                pattern: "t".into(),
                case_insensitive: true,
            })),
            verb: Verb::ReadLines(RegexLit {
                pattern: "TODO".into(),
                case_insensitive: false,
            }),
            sink: Sink::Table,
        };
        assert!(p.is_well_formed());
        let back: Program = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, back);
    }
}
