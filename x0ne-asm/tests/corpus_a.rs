//! Corpus A — parser conformance.
//!
//! 50+ text programs, one expected verdict per program. **Measures nothing
//! about the model**: corpus B (T4) carries the metric. This one pins the
//! grammar, the lexical forms and the rule codes, so that a language change
//! is a decision and not a side effect.

use serde::Deserialize;
use x0ne_asm::diag::RuleId;
use x0ne_asm::parse;

#[derive(Deserialize)]
struct Case {
    n: String,
    p: String,
    v: String,
    #[serde(default)]
    raw: bool,
}

fn cases() -> Vec<Case> {
    let text = include_str!("fixtures/corpus_a.jsonl");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("ligne JSONL invalide: {l}\n{e}"))
        })
        .collect()
}

/// Every corpus program gets exactly the expected verdict: either it parses,
/// or it is refused **by the right rule**. Expecting "a refusal" of any kind
/// would let through a parser that refuses everything for the wrong reason,
/// and the refusal message is what the LLM repairer reads.
#[test]
fn every_corpus_a_program_gets_the_expected_verdict() {
    let mut failures = Vec::new();
    for c in cases() {
        let src = if c.raw {
            c.p.clone()
        } else {
            format!("#!asm-ai/v0\n{}", c.p)
        };
        match (parse(&src), c.v.as_str()) {
            (Ok(_), "ok") => {}
            (Err(r), expected) if r.rule.code() == expected => {}
            (Ok(_), expected) => failures.push(format!(
                "{}: expected {expected}, got a successful parse",
                c.n
            )),
            (Err(r), "ok") => failures.push(format!(
                "{}: expected a successful parse, got {}\n{}",
                c.n,
                r.rule.code(),
                r.render()
            )),
            (Err(r), expected) => failures.push(format!(
                "{}: expected {expected}, got {}\n{}",
                c.n,
                r.rule.code(),
                r.render()
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
}

/// The corpus has the size the spec requires, and it has not silently shrunk.
/// A corpus emptied case by case leaves all its other tests green — it is the
/// quietest way to disarm a conformance suite.
#[test]
fn the_corpus_keeps_its_size_and_its_two_halves() {
    let all = cases();
    let ok = all.iter().filter(|c| c.v == "ok").count();
    let refused = all.len() - ok;
    assert!(
        all.len() >= 50,
        "the spec requires at least 50 programs, there are {}",
        all.len()
    );
    assert!(ok >= 20, "valid programs are required, there are {ok}");
    assert!(
        refused >= 30,
        "refused programs are required, there are {refused}"
    );
    let mut names: Vec<&str> = all.iter().map(|c| c.n.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "two corpus cases share the same name");
}

/// All 20 rules are exercised. A rule never reached is a rule nobody knows
/// works — and they are the contract T4 measures ("a plausible-but-wrong
/// program scores 0").
#[test]
fn every_refusal_rule_is_exercised_by_the_corpus() {
    let seen: std::collections::HashSet<String> = cases()
        .into_iter()
        .filter(|c| c.v != "ok")
        .map(|c| c.v)
        .collect();
    let all = [
        RuleId::MissingShebang,
        RuleId::UnknownKeyword,
        RuleId::MissingFrom,
        RuleId::MissingVerb,
        RuleId::MissingEmit,
        RuleId::ClauseOutOfOrder,
        RuleId::IllegalVerbSinkCell,
        RuleId::CountMatchesNeedsWhere,
        RuleId::BareRead,
        RuleId::GroupOutOfRange,
        RuleId::PatternEscapesRoot,
        RuleId::RegexBackreference,
        RuleId::RegexLookaround,
        RuleId::RegexTooLong,
        RuleId::UnterminatedLiteral,
        RuleId::UnknownRegexFlag,
        RuleId::DanglingDollar,
        RuleId::NonNumericGroup,
        RuleId::DuplicateClause,
        RuleId::EmptyPattern,
    ];
    // R14 is exercised by `an_overlong_pattern_is_refused_before_the_regex_builder`
    // just below, not by a line of the corpus file.
    let missing: Vec<&str> = all
        .iter()
        .map(|r| r.code())
        .filter(|c| !seen.contains(*c) && *c != "R14")
        .collect();
    assert!(missing.is_empty(), "rules never exercised: {missing:?}");
    assert_eq!(all.len(), 20, "the rule list is closed at twenty");
}

/// R14 is produced here rather than hand-written in the corpus: a 513-character
/// pattern in a JSONL file is unreadable and would get corrupted on the first
/// reformat.
#[test]
fn an_overlong_pattern_is_refused_before_the_regex_builder() {
    let long = "a".repeat(513);
    let src = format!(
        "#!asm-ai/v0\nfrom files \"src/**/*.rs\"\nwhere lines match /{long}/\ncount matches\nemit value\n"
    );
    let r = parse(&src).expect_err("a 513-character pattern must be refused");
    assert_eq!(r.rule.code(), "R14");
    assert!(r.render().contains("512"), "the message states the bound");

    // The bound itself passes: refusing at 512 would make the message lie, as
    // it announces a limit and not an exclusion threshold.
    let at_bound = "a".repeat(512);
    let src = format!(
        "#!asm-ai/v0\nfrom files \"src/**/*.rs\"\nwhere lines match /{at_bound}/\ncount matches\nemit value\n"
    );
    parse(&src).expect("a 512-character pattern is within the bound");
}

/// The rendering is stable: three lines, pattern then rule then lever. The
/// corpus compares it word for word because it is what the LLM repairer reads,
/// and a message that changes without a decision is a message nobody is
/// designing anymore.
#[test]
fn a_refusal_renders_the_pattern_then_the_rule_then_the_lever() {
    let src = "#!asm-ai/v0\nfrom files \"src/**/*.rs\"\nread\nemit table\n";
    let r = parse(src).expect_err("bare `read` is refused");
    assert_eq!(
        r.render(),
        "asm-ai R09: at line 3, column 1: `read` without `lines`\n  \
         rule: `read` always takes a pattern: `read lines /…/`. Reading whole files over a \
         glob is not a form asm-ai has\n  \
         fix: write `read lines /<what you are looking for>/`, or use `count files` if you \
         only need the number"
    );
}

/// Every corpus refusal carries all **three** elements, filled in. Testing that
/// a message is present says nothing: an empty `lever`, or one that copies the
/// rule, yields a refusal the model cannot repair — and that is exactly what
/// this crate exists to prevent.
#[test]
fn every_refusal_names_a_pattern_a_rule_and_a_lever() {
    for c in cases() {
        if c.v == "ok" {
            continue;
        }
        let src = if c.raw {
            c.p.clone()
        } else {
            format!("#!asm-ai/v0\n{}", c.p)
        };
        let r = match parse(&src) {
            Err(r) => r,
            Ok(_) => panic!("{}: expected a refusal, got a successful parse", c.n),
        };
        assert!(!r.found.trim().is_empty(), "{}: empty `found`", c.n);
        assert!(!r.why.trim().is_empty(), "{}: empty `why`", c.n);
        assert!(!r.lever.trim().is_empty(), "{}: empty `lever`", c.n);
        assert_ne!(r.why, r.lever, "{}: the lever copies the rule", c.n);
        // A lever is an action: it starts with an imperative verb. Without
        // that form, the model re-reads an observation and has nothing to do.
        let first = r.lever.split_whitespace().next().unwrap_or("");
        assert!(
            [
                "add",
                "write",
                "delete",
                "move",
                "drop",
                "rewrite",
                "close",
                "put",
                "remove",
                "split",
                "either",
                "repeat",
                "match",
                "number",
                "change",
                "reference",
                "wrap",
            ]
            .contains(&first),
            "{}: the lever does not start with an action ({first:?}) — {}",
            c.n,
            r.lever
        );
        assert!(r.line >= 1 && r.col >= 1, "{}: position 0-based", c.n);
        // The rendering carries all three, in order, and the rule code.
        let rendered = r.render();
        let pos_rule = rendered
            .find("\n  rule: ")
            .expect("the rendering carries the rule");
        let pos_fix = rendered
            .find("\n  fix: ")
            .expect("the rendering carries the lever");
        assert!(rendered.starts_with(&format!("asm-ai {}", r.rule.code())));
        assert!(pos_rule < pos_fix, "{}: rule after lever", c.n);
    }
}

/// Seen from **another crate** — this file is one. `Program` is
/// `#[non_exhaustive]`, so the struct literal does not compile there and
/// `Program::new` is the only entry point: exactly T3's position. The test
/// proves the half that is checkable at run time (the guard holds from the
/// outside); the other half, the refusal of the literal, is held by the
/// compiler and will be observed when T3 tries to write one.
#[test]
fn from_another_crate_the_constructor_is_the_only_door_and_it_guards() {
    use x0ne_asm::ir::{Glob, Program, Sink, Verb};

    Program::new(
        Glob {
            raw: "src/**/*.rs".into(),
        },
        None,
        Verb::CountFiles,
        Sink::Diff,
    )
    .expect_err("count -> diff stays refused from the outside");

    let p = Program::new(
        Glob {
            raw: "src/**/*.rs".into(),
        },
        None,
        Verb::CountFiles,
        Sink::Value,
    )
    .expect("count -> value passe");
    assert!(p.is_well_formed());
}
