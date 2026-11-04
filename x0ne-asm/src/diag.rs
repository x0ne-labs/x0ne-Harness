//! The refusal message is a **first-class product surface**.
//!
//! The "developer" doing the repair is an LLM. The eBPF data gives 0 to 37 %
//! repair from a raw log; a message that says *where the proof was lost*,
//! *which rule it hit* and *which lever* does much better. Hence the three
//! fields, in this order, and a stable rendering that corpus A pins.

use serde::{Deserialize, Serialize};

/// The twenty rules the parser can refuse. Closed and numbered: a stable code
/// is what lets corpus A expect a *specific* refusal rather than any refusal,
/// and lets the corpus B harness (T4) count repairs per rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleId {
    MissingShebang,         // R01
    UnknownKeyword,         // R02
    MissingFrom,            // R03
    MissingVerb,            // R04
    MissingEmit,            // R05
    ClauseOutOfOrder,       // R06
    IllegalVerbSinkCell,    // R07
    CountMatchesNeedsWhere, // R08
    BareRead,               // R09
    GroupOutOfRange,        // R10
    PatternEscapesRoot,     // R11
    RegexBackreference,     // R12
    RegexLookaround,        // R13
    RegexTooLong,           // R14
    UnterminatedLiteral,    // R15
    UnknownRegexFlag,       // R16
    DanglingDollar,         // R17
    NonNumericGroup,        // R18
    DuplicateClause,        // R19
    EmptyPattern,           // R20
}

impl RuleId {
    pub fn code(&self) -> &'static str {
        match self {
            RuleId::MissingShebang => "R01",
            RuleId::UnknownKeyword => "R02",
            RuleId::MissingFrom => "R03",
            RuleId::MissingVerb => "R04",
            RuleId::MissingEmit => "R05",
            RuleId::ClauseOutOfOrder => "R06",
            RuleId::IllegalVerbSinkCell => "R07",
            RuleId::CountMatchesNeedsWhere => "R08",
            RuleId::BareRead => "R09",
            RuleId::GroupOutOfRange => "R10",
            RuleId::PatternEscapesRoot => "R11",
            RuleId::RegexBackreference => "R12",
            RuleId::RegexLookaround => "R13",
            RuleId::RegexTooLong => "R14",
            RuleId::UnterminatedLiteral => "R15",
            RuleId::UnknownRegexFlag => "R16",
            RuleId::DanglingDollar => "R17",
            RuleId::NonNumericGroup => "R18",
            RuleId::DuplicateClause => "R19",
            RuleId::EmptyPattern => "R20",
        }
    }
}

/// A refusal. English and descriptive: the model is the one reading it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub rule: RuleId,
    /// 1-based, like a compiler.
    pub line: usize,
    /// **Not computed in v0: always 1.** The field exists because the shape of
    /// a compiler diagnostic is what the model knows how to read, and because
    /// the column will be computed one day without changing the type.
    ///
    /// It is not computed because a half-column would be worse than no column:
    /// a refusal born inside a literal knows its offset in the *reconstructed*
    /// text — `\/` counts there as one character, not two — and that offset
    /// does not translate into a source column without keeping the mapping.
    /// Those refusals therefore quote the faulty form in `found`
    /// (`` `$1` ``, `` a backreference `\1` ``), which locates better than a
    /// wrong number. A field that promises more than it delivers is the defect
    /// this repository calls dead scaffolding.
    pub col: usize,
    /// What was read at that location — *where the proof was lost*.
    pub found: String,
    /// The rule, stated — *the rule it hit*.
    pub why: String,
    /// The concrete action that repairs it — *the lever*.
    pub lever: String,
}

impl Refusal {
    pub fn new(
        rule: RuleId,
        line: usize,
        col: usize,
        found: impl Into<String>,
        why: impl Into<String>,
        lever: impl Into<String>,
    ) -> Self {
        Self {
            rule,
            line,
            col,
            found: found.into(),
            why: why.into(),
            lever: lever.into(),
        }
    }

    /// Three lines, always in this order. The rendering is stable because
    /// corpus A compares it word for word: a message that changes without
    /// anyone deciding it is a message nobody is designing anymore.
    pub fn render(&self) -> String {
        format!(
            "asm-ai {code}: at line {line}, column {col}: {found}\n  rule: {why}\n  fix: {lever}",
            code = self.rule.code(),
            line = self.line,
            col = self.col,
            found = self.found,
            why = self.why,
            lever = self.lever,
        )
    }
}
