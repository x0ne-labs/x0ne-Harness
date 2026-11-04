//! The parser. Line by line, without a general state machine: the grammar has
//! four clauses and a fixed order, a general-purpose lexer would be more code
//! than language.
//!
//! Every error returns a `Refusal` with its rule. There is no error recovery:
//! the first refusal stops compilation, because the reader is an LLM that
//! repairs one thing at a time and a cascade of derived refusals would send it
//! to repair the wrong one.

use crate::diag::{Refusal, RuleId};
use crate::ir::*;

const SHEBANG: &str = "#!asm-ai/v0";
/// The length bound of a pattern. `x0ne-runtime/src/asm_verify.rs` holds the same constant
/// (`limits::REGEX_PATTERN_CHARS`) and the test `the_two_regex_length_bounds_agree`
/// will compare them — it does not exist yet, nor does the host bound.
const REGEX_PATTERN_CHARS: usize = 512;

/// A meaningful line: its number (1-based) and its content, comment removed.
struct Line {
    no: usize,
    text: String,
}

pub fn parse(src: &str) -> Result<Program, Refusal> {
    let mut raw = src.lines().enumerate().map(|(i, l)| (i + 1, l));

    // ── shebang ──────────────────────────────────────────────────────────────
    let (first_no, first) = raw.next().unwrap_or((1, ""));
    if first.trim_end() != SHEBANG {
        return Err(Refusal::new(
            RuleId::MissingShebang,
            first_no,
            1,
            format!("`{}`", first.trim_end()),
            "every asm-ai program starts with the exact line `#!asm-ai/v0`",
            "put `#!asm-ai/v0` on the first line, alone",
        ));
    }

    // ── meaningful lines ─────────────────────────────────────────────────────
    // A comment runs from the first `#` outside a string and outside a regex to
    // the end of the line. It is removed here so the rest of the parser no
    // longer has to handle it — a `#` in a file pattern is text, not a comment.
    let mut lines: Vec<Line> = Vec::new();
    for (no, text) in raw {
        let cut = strip_comment(text);
        if !cut.trim().is_empty() {
            lines.push(Line { no, text: cut });
        }
    }

    let mut source: Option<(usize, Glob)> = None;
    let mut filter: Option<(usize, Filter)> = None;
    let mut verb: Option<(usize, Verb)> = None;
    let mut sink: Option<(usize, Sink)> = None;

    // First pass: each clause is read and typed where it is written. The order
    // is **not** judged here. Judging it here would conflate two defects the
    // model repairs differently: "the verb is before `from`" (move a line) and
    // "there is no `from`" (write one). A program with no `from` at all must
    // read R03, not R06 — and that is what corpus A expects
    // (`no_from_clause`).
    for line in &lines {
        let head = line.text.split_whitespace().next().unwrap_or("");
        match head {
            "from" => {
                if source.is_some() {
                    return Err(dup(line.no, "`from`"));
                }
                source = Some((line.no, parse_from(line)?));
            }
            "where" => {
                if filter.is_some() {
                    return Err(dup(line.no, "`where`"));
                }
                filter = Some((line.no, parse_where(line)?));
            }
            "edit" | "count" | "read" => {
                if verb.is_some() {
                    return Err(dup(line.no, "verb"));
                }
                verb = Some((line.no, parse_verb(line)?));
            }
            "emit" => {
                if sink.is_some() {
                    return Err(dup(line.no, "`emit`"));
                }
                sink = Some((line.no, parse_emit(line)?));
            }
            other => {
                return Err(Refusal::new(
                    RuleId::UnknownKeyword,
                    line.no,
                    1,
                    format!("`{other}`"),
                    "a clause starts with one of: from, where, edit, count, read, emit",
                    "rewrite the line as one of the six clauses; asm-ai has no others",
                ))
            }
        }
    }

    // ── presence, in grammar order ───────────────────────────────────────────
    let (from_line, source) = source.ok_or_else(|| {
        Refusal::new(
            RuleId::MissingFrom,
            lines.last().map(|l| l.no).unwrap_or(1),
            1,
            "end of program",
            "every program starts from a file set: `from files \"<glob>\"`",
            "add a line `from files \"tests/**/*.rs\"` naming the files to work on",
        )
    })?;
    let (verb_line, verb) = verb.ok_or_else(|| {
        Refusal::new(
            RuleId::MissingVerb,
            lines.last().map(|l| l.no).unwrap_or(1),
            1,
            "end of program",
            "every program has exactly one verb: edit, count or read",
            "add a verb line, e.g. `count files`",
        )
    })?;
    let (sink_line, sink) = sink.ok_or_else(|| {
        Refusal::new(
            RuleId::MissingEmit,
            verb_line,
            1,
            "end of program",
            "every program ends with `emit diff`, `emit value` or `emit table`",
            "add the `emit` line that matches the verb: edit->diff, count->value, read->table",
        )
    })?;

    // ── the order: from, where?, verb, emit ──────────────────────────────────
    if let Some((where_line, _)) = &filter {
        if *where_line < from_line {
            return Err(order(*where_line, "where", "after `from`"));
        }
        if *where_line > verb_line {
            return Err(order(*where_line, "where", "before the verb"));
        }
    }
    if verb_line < from_line {
        return Err(order(verb_line, verb_name(&verb), "after `from`"));
    }
    if sink_line < verb_line {
        return Err(order(sink_line, "emit", "after the verb"));
    }

    // ── the verb × sink matrix ───────────────────────────────────────────────
    if !Program::sink_matches_verb(&verb, sink) {
        return Err(Refusal::new(
            RuleId::IllegalVerbSinkCell,
            sink_line,
            1,
            format!("{} with {:?}", verb_name(&verb), sink),
            "only three verb/emit pairs exist: edit->diff, count->value, read->table",
            format!("change the emit line to `emit {}`", expected_sink(&verb)),
        ));
    }

    // ── `count matches` requires `where lines match` ─────────────────────────
    if matches!(verb, Verb::CountMatches) && !matches!(filter, Some((_, Filter::Lines(_)))) {
        return Err(Refusal::new(
            RuleId::CountMatchesNeedsWhere,
            verb_line,
            1,
            "`count matches`",
            "counting matches needs to know what to match: `count matches` requires a \
             `where lines match /…/` clause",
            "add `where lines match /<your pattern>/` above the verb, or use `count files`",
        ));
    }

    // ── `${n}` beyond the number of groups ───────────────────────────────────
    if let Verb::Replace { find, template } = &verb {
        let groups = count_capture_groups(&find.pattern);
        for part in &template.parts {
            if let TemplatePart::Group(n) = part {
                if *n == 0 || *n as usize > groups {
                    // With zero groups, the general form read
                    // "${1}..${0} are the only references that exist" — an
                    // empty range presented as a list. The reader is a model:
                    // a self-contradicting message cannot be repaired.
                    let (why, lever) = if groups == 0 {
                        (
                            "this pattern captures nothing: it has no `(...)` group, so there \
                             is no group for a `${n}` to name"
                                .to_string(),
                            "wrap the part you want to reuse in `(...)`, then write `${1}`"
                                .to_string(),
                        )
                    } else {
                        (
                            // With a single group, the general form returned
                            // "${1}..${1} are the only references" — the same
                            // family of nonsense as the empty range.
                            if groups == 1 {
                                "the search pattern has 1 capture group, so ${1} is the only \
                                 reference that exists"
                                    .to_string()
                            } else {
                                format!(
                                    "the search pattern has {groups} capture groups, so \
                                     ${{1}}..${{{groups}}} are the only references that exist"
                                )
                            },
                            "either reference an existing group, or add a `(...)` group to \
                             the pattern"
                                .to_string(),
                        )
                    };
                    return Err(Refusal::new(
                        RuleId::GroupOutOfRange,
                        verb_line,
                        1,
                        format!("`${{{n}}}`"),
                        why,
                        lever,
                    ));
                }
            }
        }
    }

    Ok(Program {
        source,
        filter: filter.map(|(_, f)| f),
        verb,
        sink,
    })
}

/// Removes the comment: first `#` that is neither in a `"…"` string nor in a
/// `/…/` regex, honouring `\` as the escape in both.
fn strip_comment(text: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let mut in_rx = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_str || in_rx => {
                out.push(c);
                escaped = true;
            }
            '"' if !in_rx => {
                in_str = !in_str;
                out.push(c);
            }
            '/' if !in_str => {
                in_rx = !in_rx;
                out.push(c);
            }
            '#' if !in_str && !in_rx => break,
            _ => out.push(c),
        }
    }
    out
}

fn dup(no: usize, what: &str) -> Refusal {
    Refusal::new(
        RuleId::DuplicateClause,
        no,
        1,
        format!("a second {what} clause"),
        "a program has at most one clause of each kind",
        format!("delete one of the two {what} lines"),
    )
}

fn order(no: usize, what: &str, where_: &str) -> Refusal {
    Refusal::new(
        RuleId::ClauseOutOfOrder,
        no,
        1,
        format!("`{what}` here"),
        format!("clause order is fixed: from, where?, verb, emit — `{what}` goes {where_}"),
        "move the line to its place in that order",
    )
}

fn verb_name(v: &Verb) -> &'static str {
    match v {
        Verb::Replace { .. } => "edit replace",
        Verb::CountFiles => "count files",
        Verb::CountMatches => "count matches",
        Verb::ReadLines(_) => "read lines",
    }
}

fn expected_sink(v: &Verb) -> &'static str {
    match v {
        Verb::Replace { .. } => "diff",
        Verb::CountFiles | Verb::CountMatches => "value",
        Verb::ReadLines(_) => "table",
    }
}

/// Counts **capturing** groups: `(` not followed by `?`. Sufficient for the
/// syntax the language allows, since backreferences and look-around are
/// refused (R12, R13) and `(?:` is the only other form of `(?`.
fn count_capture_groups(pattern: &str) -> usize {
    let b = pattern.as_bytes();
    let mut n = 0usize;
    let mut i = 0usize;
    let mut in_class = false;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => {
                // `(?P<x>…)` and `(?<x>…)` capture, and the `regex` crate gives
                // them a number like the others: `${1}` references them
                // legitimately. Counting them as zero returned "this pattern
                // has 0 groups" on a pattern that has one — the refusal was
                // right, its reason was wrong. `(?:` does not capture;
                // look-arounds are refused before reaching this point.
                let named = match (b.get(i + 2), b.get(i + 3)) {
                    (Some(&b'P'), Some(&b'<')) => true,
                    (Some(&b'<'), next) => !matches!(next, Some(&b'=') | Some(&b'!')),
                    _ => false,
                };
                if b.get(i + 1) != Some(&b'?') || named {
                    n += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn parse_from(line: &Line) -> Result<Glob, Refusal> {
    let rest = expect_words(line, &["from", "files"], "from files \"<glob>\"")?;
    let raw = parse_quoted(line, rest)?;
    if raw.is_empty() {
        return Err(Refusal::new(
            RuleId::EmptyPattern,
            line.no,
            1,
            "`\"\"`",
            "the file pattern selects the files the program may touch; an empty one selects nothing",
            "write a glob, e.g. \"tests/**/*.rs\"",
        ));
    }
    // SECURITY: `..` and the absolute path are refused lexically, before any
    // resolution. The real confinement remains `ConfinedPath` on the runtime
    // side — this is an **early, readable** refusal, not the safeguard. A
    // pattern that climbs is almost always a model error, and answering it at
    // compile time costs one turn instead of an opaque policy refusal.
    //
    // The absolute case deserves its own refusal because without it it yields
    // silent nonsense: the emitter anchors with `format!("{root}/{raw}")`, so
    // `/etc/**` would become `/w//etc/**`, a pattern that exists nowhere.
    // The program would be refused — but for "no file matches", which teaches
    // the model nothing about what it did wrong.
    if raw.starts_with('/') {
        return Err(Refusal::new(
            RuleId::PatternEscapesRoot,
            line.no,
            1,
            format!("`{raw}`"),
            "file patterns are relative to the project root; an absolute path is not a \
             pattern asm-ai has",
            "drop the leading `/` and write the path from the project root, e.g. \
             \"src/**/*.rs\"",
        ));
    }
    if climbs_out(&raw) {
        return Err(Refusal::new(
            RuleId::PatternEscapesRoot,
            line.no,
            1,
            format!("`{raw}`"),
            "file patterns are relative to the project root and may not climb out of it with `..`",
            "write the path from the project root, without any `..` segment",
        ));
    }
    Ok(Glob { raw })
}

/// Does a pattern segment climb one level?
///
/// SECURITY: segment boundaries are not only `/`. The language accepts brace
/// alternatives (`src/**/*.{rs,toml}`), and a `..` can hide there:
/// `src/{..,a}/*` passed the original check, which only split on `/`
/// (measured by the 2026-08-15 review, with `{a,..}` and `{..}` — three
/// forms, three false greens). So the split is also on `{`, `,` and `}`.
///
/// The `*` suffix is handled too: `..*` is a pattern that *matches* `..`, so
/// it climbs just as well as a bare `..`. `..?` only matches `..x` and is not
/// targeted.
///
/// This check remains an **early, readable** refusal, not the safeguard: the
/// real confinement is `ConfinedPath` on the runtime side, on the expanded
/// paths (T3). What is fixed here is that a line announcing "`..` is
/// refused" actually does it.
fn climbs_out(raw: &str) -> bool {
    raw.split(['/', '{', ',', '}'])
        .any(|seg| seg == ".." || (seg.starts_with("..") && seg[2..].chars().all(|c| c == '*')))
}

fn parse_where(line: &Line) -> Result<Filter, Refusal> {
    let words: Vec<&str> = line.text.split_whitespace().collect();
    match words.get(1).copied() {
        Some("name") => {
            let rest = expect_words(line, &["where", "name", "match"], "where name match /…/")?;
            Ok(Filter::Name(parse_regex(line, rest)?))
        }
        Some("lines") => {
            let rest = expect_words(line, &["where", "lines", "match"], "where lines match /…/")?;
            Ok(Filter::Lines(parse_regex(line, rest)?))
        }
        other => Err(Refusal::new(
            RuleId::UnknownKeyword,
            line.no,
            1,
            format!("`{}`", other.unwrap_or("end of line")),
            "a `where` clause filters on `name` (the file path) or on `lines` (the file content)",
            "write `where name match /…/` or `where lines match /…/`",
        )),
    }
}

fn parse_verb(line: &Line) -> Result<Verb, Refusal> {
    let words: Vec<&str> = line.text.split_whitespace().collect();
    match (words[0], words.get(1).copied()) {
        ("edit", Some("replace")) => {
            let rest = expect_words(line, &["edit", "replace"], "edit replace /…/ -> \"…\"")?;
            let (rx, after) = take_regex(line, rest)?;
            let after = after.trim_start();
            let after = after.strip_prefix("->").ok_or_else(|| {
                Refusal::new(
                    RuleId::UnknownKeyword,
                    line.no,
                    1,
                    format!("`{after}`"),
                    "the replacement is introduced by `->`",
                    "write `edit replace /pattern/ -> \"replacement\"`",
                )
            })?;
            let template_raw = parse_quoted(line, after)?;
            Ok(Verb::Replace {
                find: rx,
                template: parse_template(line, &template_raw)?,
            })
        }
        ("count", Some("files")) => {
            expect_end(line, &words, 2)?;
            Ok(Verb::CountFiles)
        }
        ("count", Some("matches")) => {
            expect_end(line, &words, 2)?;
            Ok(Verb::CountMatches)
        }
        ("read", Some("lines")) => {
            let rest = expect_words(line, &["read", "lines"], "read lines /…/")?;
            Ok(Verb::ReadLines(parse_regex(line, rest)?))
        }
        // SECURITY: the bare `read` form would be a `cat` over a glob — forty
        // files poured into the model's context at once. It is refused at
        // compile time, not filtered at run time.
        ("read", _) => Err(Refusal::new(
            RuleId::BareRead,
            line.no,
            1,
            "`read` without `lines`",
            "`read` always takes a pattern: `read lines /…/`. Reading whole files over a \
             glob is not a form asm-ai has",
            "write `read lines /<what you are looking for>/`, or use `count files` if you \
             only need the number",
        )),
        (v, arg) => Err(Refusal::new(
            RuleId::UnknownKeyword,
            line.no,
            1,
            format!("`{v} {}`", arg.unwrap_or("")),
            "the verbs are `edit replace`, `count files`, `count matches` and `read lines`",
            "rewrite the verb line as one of those four",
        )),
    }
}

fn parse_emit(line: &Line) -> Result<Sink, Refusal> {
    let words: Vec<&str> = line.text.split_whitespace().collect();
    match words.get(1).copied() {
        Some("diff") => expect_end(line, &words, 2).map(|()| Sink::Diff),
        Some("value") => expect_end(line, &words, 2).map(|()| Sink::Value),
        Some("table") => expect_end(line, &words, 2).map(|()| Sink::Table),
        other => Err(Refusal::new(
            RuleId::UnknownKeyword,
            line.no,
            1,
            format!("`{}`", other.unwrap_or("end of line")),
            "`emit` takes one of: diff, value, table",
            "write `emit diff` after an edit, `emit value` after a count, `emit table` after a read",
        )),
    }
}

/// Checks the expected keywords and returns the rest of the line.
fn expect_words<'a>(line: &'a Line, words: &[&str], shape: &str) -> Result<&'a str, Refusal> {
    let mut rest = line.text.trim_start();
    for w in words {
        // A keyword ends at whitespace: `filesX` is not `files` followed by
        // `X`. Measured on 2026-10-05: `from filesX "…"` was refused R15
        // (unterminated string) instead of R02.
        let after = rest
            .strip_prefix(*w)
            .filter(|a| a.is_empty() || a.starts_with(char::is_whitespace));
        rest = after.ok_or_else(|| {
            Refusal::new(
                RuleId::UnknownKeyword,
                line.no,
                1,
                format!("`{rest}`"),
                format!("this clause is written `{shape}`"),
                format!("rewrite the line as `{shape}`"),
            )
        })?;
        rest = rest.trim_start();
    }
    Ok(rest)
}

/// A clause made only of words stops at the last one: `count files in src` is
/// refused rather than read as `count files`, otherwise the program executed
/// is not the one that was written.
fn expect_end(line: &Line, words: &[&str], expected: usize) -> Result<(), Refusal> {
    if words.len() > expected {
        return Err(trailing(line, &words[expected..].join(" ")));
    }
    Ok(())
}

/// `"…"` with `\"` and `\\` escaped. Nothing else is escaped: the fewer
/// sequences there are, the fewer the model invents. Also returns what follows
/// the closing quote, which the caller must examine.
fn take_quoted<'a>(line: &Line, rest: &'a str) -> Result<(String, &'a str), Refusal> {
    let rest = rest.trim_start();
    let body = rest
        .strip_prefix('"')
        .ok_or_else(|| unterminated(line, "a double-quoted string", rest))?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    loop {
        match chars.next() {
            Some((_, '\\')) => match chars.next() {
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                Some((_, c)) => {
                    out.push('\\');
                    out.push(c);
                }
                None => return Err(unterminated(line, "a double-quoted string", rest)),
            },
            Some((i, '"')) => return Ok((out, &body[i + 1..])),
            Some((_, c)) => out.push(c),
            None => return Err(unterminated(line, "a double-quoted string", rest)),
        }
    }
}

/// The same, but the string must end the clause.
///
/// SECURITY: without this check, `from files "src/**" "/etc/**"` reads as
/// `from files "src/**"` and the second pattern disappears without a word. The
/// danger is not that `/etc` is read — it is not — it is that a program
/// approved on what is shown of it is not the one the model wrote. The
/// grammar is closed: what it does not understand, it refuses, it does not
/// ignore it.
fn parse_quoted(line: &Line, rest: &str) -> Result<String, Refusal> {
    let (out, after) = take_quoted(line, rest)?;
    if !after.trim().is_empty() {
        return Err(trailing(line, after.trim()));
    }
    Ok(out)
}

/// `/…/` + optional `i` flag, `\/` escapes the slash.
fn take_regex<'a>(line: &Line, rest: &'a str) -> Result<(RegexLit, &'a str), Refusal> {
    let rest = rest.trim_start();
    let body = rest
        .strip_prefix('/')
        .ok_or_else(|| unterminated(line, "a /regex/", rest))?;
    let mut pattern = String::new();
    let mut idx = 0usize;
    let mut escaped = false;
    let mut closed = false;
    for (i, c) in body.char_indices() {
        idx = i + c.len_utf8();
        if escaped {
            if c != '/' {
                pattern.push('\\');
            }
            pattern.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '/' => {
                closed = true;
                break;
            }
            _ => pattern.push(c),
        }
    }
    if !closed {
        return Err(unterminated(line, "a /regex/", rest));
    }
    let after = &body[idx..];
    // The flag block is read in full before being judged. Reading it character
    // by character returned R02 ("a stray `m`") on `/x/im` where the model
    // wrote two flags and must read R16 — a refusal that names the wrong rule
    // sends the repair to the wrong place.
    let flags_end = after
        .char_indices()
        .find(|(_, c)| !c.is_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    let (flags, after) = after.split_at(flags_end);
    let case_insensitive = match flags {
        "" => false,
        "i" => true,
        other => {
            return Err(Refusal::new(
                RuleId::UnknownRegexFlag,
                line.no,
                1,
                format!("`{other}`"),
                "the only regex flag is `i` (case-insensitive), and it is written once, \
                 right after the closing `/`",
                "remove the flag, or keep only `i`",
            ))
        }
    };

    if pattern.is_empty() {
        return Err(Refusal::new(
            RuleId::EmptyPattern,
            line.no,
            1,
            "`//`",
            "an empty pattern matches everywhere, which is never what a program means",
            "write the text or the shape you are looking for between the slashes",
        ));
    }
    if pattern.chars().count() > REGEX_PATTERN_CHARS {
        return Err(Refusal::new(
            RuleId::RegexTooLong,
            line.no,
            1,
            format!("a {}-character pattern", pattern.chars().count()),
            format!("patterns are limited to {REGEX_PATTERN_CHARS} characters"),
            "split the work into two programs, or write a shorter pattern",
        ));
    }
    // SECURITY: backreferences and look-around are outside the `regex` crate
    // (linear time is only guaranteed without them), and are the classic entry
    // point for ReDoS. Refused here, at compile time, rather than discovered by
    // the host-side builder: the model needs to know *which* form to remove.
    if let Some(digit) = find_backreference(&pattern) {
        return Err(Refusal::new(
            RuleId::RegexBackreference,
            line.no,
            1,
            format!("a backreference `\\{digit}`"),
            "asm-ai patterns have no backreferences: matching is linear-time by construction",
            "repeat the text you want instead of referring back to a group",
        ));
    }
    // Every `(?` directive outside a character class, judged in one place.
    // Look-around (R13), named backreference (R12), inline flags (R16) and
    // anything else the `regex` crate would reject (R16): refused here, not by
    // the host builder.
    for d in paren_directives(&pattern) {
        match d.kind {
            Directive::Allowed => {}
            Directive::Lookaround => {
                return Err(Refusal::new(
                    RuleId::RegexLookaround,
                    line.no,
                    1,
                    format!("a look-around group `{}`", d.text),
                    "asm-ai patterns have no look-ahead or look-behind",
                    "match the surrounding text directly, and put the part you keep in a \
                     `(...)` group",
                ))
            }
            Directive::NamedBackreference => {
                return Err(Refusal::new(
                    RuleId::RegexBackreference,
                    line.no,
                    1,
                    format!("a named backreference `{}`", d.text),
                    "asm-ai patterns have no backreferences: matching is linear-time by \
                     construction",
                    "repeat the text you want instead of referring back to a group",
                ))
            }
            // SECURITY: an inline flag makes the IR **lie**. `/(?i)todo/` parsed
            // with `case_insensitive: false` whereas the pattern is indeed
            // case-insensitive: everything that reads the IR downstream — the
            // capability proof, the approval rendering — would describe
            // something other than what runs. The flag has one place, and only
            // one, after the closing slash.
            Directive::InlineFlags => {
                return Err(Refusal::new(
                    RuleId::UnknownRegexFlag,
                    line.no,
                    1,
                    format!("`{}`", d.text),
                    "flags are not set inside an asm-ai pattern: the only flag is `i`, written \
                     after the closing `/`",
                    "remove the inline group and write `/…/i` if you meant case-insensitive",
                ))
            }
            Directive::Unknown => {
                return Err(Refusal::new(
                    RuleId::UnknownRegexFlag,
                    line.no,
                    1,
                    format!("`{}`", d.text),
                    "`(?` opens only a non-capturing group `(?:...)` or a named group \
                     `(?P<name>...)`; asm-ai patterns have no other `(?` form",
                    "write `(?:...)` to group without capturing, or remove the `(?` form",
                ))
            }
        }
    }
    Ok((
        RegexLit {
            pattern,
            case_insensitive,
        },
        after,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    /// `(?:` and the named groups `(?P<n>` / `(?<n>`.
    Allowed,
    Lookaround,
    NamedBackreference,
    /// `(?i)`, `(?im)`, `(?i:…)`, `(?-i)`.
    InlineFlags,
    /// `(?)`, `(?#…)`, and whatever else the `regex` crate would reject.
    Unknown,
}

struct ParenDirective<'a> {
    kind: Directive,
    /// The head of the directive, quoted to the model as-is.
    text: &'a str,
}

/// Every `(?` outside a character class and not escaped, with its kind.
///
/// Class-aware: `[(?i)]` is four literal characters. Measured on 2026-10-05:
/// `a[(?i)]b` was refused R16 and `a[(?=]b` R13 by a plain substring search.
fn paren_directives(pattern: &str) -> Vec<ParenDirective<'_>> {
    let b = pattern.as_bytes();
    let mut out = Vec::new();
    let mut in_class = false;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class && b.get(i + 1) == Some(&b'?') => {
                let tail = &pattern[i + 2..];
                let (kind, head_len) = classify_directive(tail);
                out.push(ParenDirective {
                    kind,
                    text: &pattern[i..i + 2 + head_len],
                });
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// `tail` follows a `(?`. Returns the kind and how many bytes of `tail` form
/// the head worth quoting.
fn classify_directive(tail: &str) -> (Directive, usize) {
    let t = tail.as_bytes();
    if t.first() == Some(&b':') {
        return (Directive::Allowed, 1);
    }
    if tail.starts_with("P<")
        || (t.first() == Some(&b'<') && !tail.starts_with("<=") && !tail.starts_with("<!"))
    {
        return (Directive::Allowed, 1);
    }
    for m in ["<=", "<!", "=", "!"] {
        if tail.starts_with(m) {
            return (Directive::Lookaround, m.len());
        }
    }
    if tail.starts_with("P=") {
        let end = tail.find(')').map(|i| i + 1).unwrap_or(tail.len());
        return (Directive::NamedBackreference, end);
    }
    // `R` is the CRLF flag, added by `regex` in 1.9: the crate compiles it,
    // so a `(?R)` got through the gate and the host would have honoured it.
    let n = tail
        .find(|c: char| !matches!(c, 'i' | 'm' | 's' | 'x' | 'u' | 'U' | 'R' | '-'))
        .unwrap_or(tail.len());
    if n > 0 && matches!(t.get(n), Some(b')') | Some(b':')) {
        return (Directive::InlineFlags, n + 1);
    }
    // Quote `(?` plus the next character: `(?)`, `(?#`.
    let next = tail.chars().next().map(char::len_utf8).unwrap_or(0);
    (Directive::Unknown, next)
}

fn parse_regex(line: &Line, rest: &str) -> Result<RegexLit, Refusal> {
    let (rx, after) = take_regex(line, rest)?;
    if !after.trim().is_empty() {
        return Err(trailing(line, after.trim()));
    }
    Ok(rx)
}

/// What trails after a complete clause. A single message for the five places
/// where it can happen: the model learns one rule, not five.
fn trailing(line: &Line, extra: &str) -> Refusal {
    Refusal::new(
        RuleId::UnknownKeyword,
        line.no,
        1,
        format!("`{extra}`"),
        "a clause ends where its shape ends: asm-ai has no options, no modifiers and no \
         second argument",
        "delete what follows on this line, or move it to its own clause",
    )
}

/// `\1` … `\9`: a backreference. Returns the **digit**, not its position: the
/// position in the reconstructed pattern does not translate to a source column
/// (see `Refusal::col`), whereas the digit can be quoted as-is to the model.
fn find_backreference(pattern: &str) -> Option<char> {
    let b = pattern.as_bytes();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] == b'\\' {
            if b[i + 1].is_ascii_digit() && b[i + 1] != b'0' {
                return Some(b[i + 1] as char);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    None
}

/// `${n}` for groups (numeric only), `$$` for a literal `$`. Any other `$` is
/// refused: letting it through as a literal would invite the model to write
/// `$1` (the form used by other tools) and get a literal `$1` in the file,
/// without ever seeing anything fail.
fn parse_template(line: &Line, raw: &str) -> Result<Template, Refusal> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            lit.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                lit.push('$');
            }
            Some('{') => {
                chars.next();
                let mut num = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    num.push(c);
                }
                if !closed {
                    return Err(unterminated(line, "a `${n}` group reference", raw));
                }
                // Digits that overflow `u32` are a range error, not a naming
                // error: `${99999999999999}` names no group any pattern has.
                if !num.is_empty()
                    && num.bytes().all(|c| c.is_ascii_digit())
                    && num.parse::<u32>().is_err()
                {
                    return Err(Refusal::new(
                        RuleId::GroupOutOfRange,
                        line.no,
                        1,
                        format!("`${{{num}}}`"),
                        "no pattern has that many capture groups: references are small \
                         numbers, ${1} for the first `(...)`",
                        "reference an existing group: count the `(...)` of the pattern \
                         from the left, starting at ${1}",
                    ));
                }
                let n: u32 = num.parse().map_err(|_| {
                    Refusal::new(
                        RuleId::NonNumericGroup,
                        line.no,
                        1,
                        format!("`${{{num}}}`"),
                        "group references are numeric: ${1}, ${2}, … — asm-ai has no named groups",
                        "number the group instead: the first `(...)` of the pattern is ${1}",
                    )
                })?;
                if !lit.is_empty() {
                    parts.push(TemplatePart::Literal(std::mem::take(&mut lit)));
                }
                parts.push(TemplatePart::Group(n));
            }
            next => {
                // Quote what the model wrote — `$1`, the form used by other
                // tools — rather than an abstract "a lone `$`": that quote is
                // what makes it see its own habit.
                let found = match next {
                    Some(c) => format!("`${c}`"),
                    None => "a trailing `$`".to_string(),
                };
                return Err(Refusal::new(
                    RuleId::DanglingDollar,
                    line.no,
                    1,
                    found,
                    "in a replacement, `$` introduces `${n}`; a literal dollar is written `$$`",
                    "write `$$` for a literal dollar, or `${1}` to insert the first group",
                ));
            }
        }
    }
    if !lit.is_empty() {
        parts.push(TemplatePart::Literal(lit));
    }
    Ok(Template { parts })
}

fn unterminated(line: &Line, what: &str, found: &str) -> Refusal {
    Refusal::new(
        RuleId::UnterminatedLiteral,
        line.no,
        1,
        format!("`{}`", found.trim()),
        format!("this clause expects {what}, closed on the same line"),
        "close the literal on the same line; asm-ai has no multi-line literals",
    )
}

#[cfg(test)]
mod tests {
    use crate::ir::*;
    use crate::parse;

    /// The program of the core scene. It carries **no** `where` clause: the
    /// spec says so explicitly, a redundant clause in the canonical example
    /// would teach the model to always emit it.
    #[test]
    fn the_canonical_program_parses_into_the_typed_ir() {
        let src = "#!asm-ai/v0\n\
                   from  files \"tests/**/*.rs\"\n\
                   edit  replace /fn test_(\\w+)\\(/ -> \"fn ${1}_property(\"\n\
                   emit  diff\n";
        let p = parse(src).expect("the canonical program must parse");
        assert_eq!(p.source.raw, "tests/**/*.rs");
        assert!(p.filter.is_none());
        match &p.verb {
            Verb::Replace { find, template } => {
                assert_eq!(find.pattern, r"fn test_(\w+)\(");
                assert!(!find.case_insensitive);
                assert_eq!(
                    template.parts,
                    vec![
                        TemplatePart::Literal("fn ".into()),
                        TemplatePart::Group(1),
                        TemplatePart::Literal("_property(".into()),
                    ]
                );
            }
            other => panic!("expected Replace, got {other:?}"),
        }
        assert_eq!(p.sink, Sink::Diff);
    }

    fn ok(body: &str) -> Program {
        parse(&format!("#!asm-ai/v0\n{body}")).expect("this program must parse")
    }

    /// Corpus A only judges a **verdict**: it says a program passes, not what
    /// it became. The following tests carry the missing half — without them, a
    /// parser that always returned `case_insensitive: false`, or that confused
    /// `where name` and `where lines`, would stay green end to end. That is the
    /// exact shape of dead scaffolding: the flag exists, it is read, it changes
    /// nothing.
    #[test]
    fn the_case_insensitive_flag_reaches_the_ir() {
        let with =
            ok("from files \"d/*.md\"\nwhere lines match /todo/i\ncount matches\nemit value\n");
        let without =
            ok("from files \"d/*.md\"\nwhere lines match /todo/\ncount matches\nemit value\n");
        match (with.filter, without.filter) {
            (Some(Filter::Lines(a)), Some(Filter::Lines(b))) => {
                assert!(a.case_insensitive, "the `i` flag must reach the IR");
                assert!(!b.case_insensitive, "its absence too");
                assert_eq!(a.pattern, "todo", "the flag does not stay in the pattern");
                assert_eq!(b.pattern, "todo");
            }
            other => panic!("expected two line filters, got {other:?}"),
        }
    }

    /// `where name` filters the path, `where lines` filters the content.
    /// Confusing them would read files to determine what a name test would have
    /// settled without opening them — two different effects under the same
    /// word.
    #[test]
    fn where_name_and_where_lines_are_not_the_same_filter() {
        let by_name =
            ok("from files \"**/*.rs\"\nwhere name match /tests/\ncount files\nemit value\n");
        let by_lines =
            ok("from files \"**/*.rs\"\nwhere lines match /tests/\ncount files\nemit value\n");
        assert!(matches!(by_name.filter, Some(Filter::Name(_))));
        assert!(matches!(by_lines.filter, Some(Filter::Lines(_))));
    }

    /// The three legal cells of the matrix each yield their sink, and the verb
    /// that goes with it. Checking `Sink::Diff` alone would let through a
    /// `parse_emit` that always returned `Diff`.
    #[test]
    fn each_legal_cell_of_the_matrix_yields_its_own_verb_and_sink() {
        let count = ok("from files \"s/*.rs\"\ncount files\nemit value\n");
        assert_eq!(count.verb, Verb::CountFiles);
        assert_eq!(count.sink, Sink::Value);

        let matches =
            ok("from files \"s/*.rs\"\nwhere lines match /x/\ncount matches\nemit value\n");
        assert_eq!(matches.verb, Verb::CountMatches);
        assert_eq!(matches.sink, Sink::Value);

        let read = ok("from files \"s/*.rs\"\nread lines /TODO/\nemit table\n");
        assert_eq!(
            read.verb,
            Verb::ReadLines(RegexLit {
                pattern: "TODO".into(),
                case_insensitive: false
            })
        );
        assert_eq!(read.sink, Sink::Table);
    }

    /// The lexical forms yield the text they promise. `\/` is one slash and
    /// not two characters, `\"` is one quote, and `$$` is **one** literal `$`
    /// — not a group reference. A `$$` that became `Group` would write the
    /// content of a capture where the model wanted a dollar, silently.
    #[test]
    fn the_lexical_forms_unescape_to_what_they_promise() {
        let p = ok(
            "from files \"s/*.rs\"\nwhere lines match /http:\\/\\/x/\ncount matches\nemit value\n",
        );
        match p.filter {
            Some(Filter::Lines(rx)) => assert_eq!(rx.pattern, "http://x"),
            other => panic!("expected a line filter, got {other:?}"),
        }

        let p =
            ok("from files \"s/*.sh\"\nedit replace /H/ -> \"$${HOME} \\\"q\\\"\"\nemit diff\n");
        match p.verb {
            Verb::Replace { template, .. } => assert_eq!(
                template.parts,
                vec![TemplatePart::Literal("${HOME} \"q\"".into())],
                "a `$$` is a literal dollar, not a group"
            ),
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    /// The comment stops at literals. A `#` in a glob or in a regex is text:
    /// cutting it would truncate the pattern silently, and the program would
    /// select files other than those read before approving.
    #[test]
    fn a_hash_inside_a_literal_is_text_and_not_a_comment() {
        let p = ok("from files \"docs/#drafts/*.md\" # only drafts\ncount files\nemit value\n");
        assert_eq!(p.source.raw, "docs/#drafts/*.md");

        let p = ok(
            "from files \"s/*.rs\"\nwhere lines match /#\\[test\\]/\ncount matches\nemit value\n",
        );
        match p.filter {
            Some(Filter::Lines(rx)) => assert_eq!(rx.pattern, r"#\[test\]"),
            other => panic!("expected a line filter, got {other:?}"),
        }
    }

    /// With zero groups, refusal R10 read "${1}..${0} are the only
    /// references that exist": an empty range given as a list. The corpus
    /// harness could not see it — it compares the rule code, not the
    /// sentence. A self-contradicting message is not repairable.
    #[test]
    fn a_pattern_without_groups_says_so_plainly_instead_of_naming_an_empty_range() {
        let r = parse("#!asm-ai/v0\nfrom files \"s/*.rs\"\nedit replace /fn test/ -> \"fn ${1}\"\nemit diff\n")
            .expect_err("`${1}` without a group is refused");
        assert_eq!(r.rule.code(), "R10");
        let rendered = r.render();
        assert!(
            !rendered.contains("${1}..${0}"),
            "the message states an empty range: {rendered}"
        );
        assert!(rendered.contains("no `(...)` group"), "{rendered}");
        assert!(
            rendered.contains("wrap"),
            "the lever says what to do: {rendered}"
        );
    }

    /// A named group **captures** — the `regex` crate gives it a number like
    /// the others, so `${1}` really references it. Counting them as zero
    /// returned a right refusal for a wrong reason ("this pattern has 0
    /// groups" on a pattern that has one), and the reason is what the model reads.
    #[test]
    fn a_named_group_counts_as_the_capture_group_it_is() {
        let p = ok("from files \"s/*.rs\"\nedit replace /(?P<name>\\w+)/ -> \"${1}\"\nemit diff\n");
        assert!(matches!(p.verb, Verb::Replace { .. }));
        let p = ok("from files \"s/*.rs\"\nedit replace /(?<name>\\w+)/ -> \"${1}\"\nemit diff\n");
        assert!(matches!(p.verb, Verb::Replace { .. }));

        let r = parse("#!asm-ai/v0\nfrom files \"s/*.rs\"\nedit replace /(?P<name>\\w+)/ -> \"${2}\"\nemit diff\n")
            .expect_err("`${2}` exceeds the pattern's only group");
        assert_eq!(r.rule.code(), "R10");
        assert!(
            r.render().contains("has 1 capture group"),
            "the announced count must be the real one: {}",
            r.render()
        );
    }

    /// A flag set inside the pattern made the IR **lie**:
    /// `case_insensitive: false` on a case-insensitive pattern. What the IR
    /// says about the program is what will be proven and shown to the human.
    #[test]
    fn an_inline_flag_group_cannot_contradict_the_ir() {
        for pattern in ["(?i)todo", "(?i:todo)", "(?im)todo", "x(?-i)y", "(?s).*"] {
            let src = format!(
                "#!asm-ai/v0\nfrom files \"s/*.rs\"\nwhere lines match /{pattern}/\ncount matches\nemit value\n"
            );
            let r = parse(&src).unwrap_err();
            assert_eq!(r.rule.code(), "R16", "pattern {pattern}: {}", r.render());
        }
        // `(?:` sets no flag and stays allowed — the corpus relies on it, and
        // forbidding it would remove the only way to group without
        // capturing.
        ok("from files \"s/*.rs\"\nwhere lines match /(?:a|b)c/\ncount matches\nemit value\n");
    }

    /// The three brace forms measured by the review, plus the `..*` that
    /// matches `..`. A check that announces "`..` is refused" must refuse
    /// `..` wherever the glob accepts one, not only between two `/`.
    #[test]
    fn a_dotdot_is_refused_wherever_a_glob_boundary_puts_one() {
        for glob in [
            "../x",
            "src/../x",
            "src/{..,a}/*",
            "src/{a,..}/*",
            "{..}/x",
            "src/..*/x",
            "..",
        ] {
            let src = format!("#!asm-ai/v0\nfrom files \"{glob}\"\ncount files\nemit value\n");
            let r = match parse(&src) {
                Err(r) => r,
                Ok(_) => panic!("{glob} must be refused"),
            };
            assert_eq!(r.rule.code(), "R11", "pattern {glob}");
        }
        // And what contains dots without climbing stays accepted: a check
        // that refuses everything protects nothing, it breaks the language.
        for glob in ["src/**/*.rs", "a..b/*.rs", ".../x", "src/{a,b}/..c"] {
            let src = format!("#!asm-ai/v0\nfrom files \"{glob}\"\ncount files\nemit value\n");
            parse(&src).unwrap_or_else(|e| panic!("{glob} must pass: {}", e.render()));
        }
    }

    /// The matrix has **four** legal verb/sink pairs and not one more (both
    /// count verbs land on `value`). The test enumerates the twelve possible
    /// pairs so that adding a verb or a sink without an explicit decision shows up here.
    #[test]
    fn the_verb_sink_matrix_has_exactly_four_legal_pairs() {
        let verbs = [
            Verb::Replace {
                find: RegexLit {
                    pattern: "a".into(),
                    case_insensitive: false,
                },
                template: Template { parts: vec![] },
            },
            Verb::CountFiles,
            Verb::CountMatches,
            Verb::ReadLines(RegexLit {
                pattern: "a".into(),
                case_insensitive: false,
            }),
        ];
        let legal: Vec<(usize, Sink)> = verbs
            .iter()
            .enumerate()
            .flat_map(|(i, v)| {
                [Sink::Diff, Sink::Value, Sink::Table]
                    .into_iter()
                    .filter(move |s| Program::sink_matches_verb(v, *s))
                    .map(move |s| (i, s))
            })
            .collect();
        assert_eq!(
            legal,
            vec![
                (0, Sink::Diff),
                (1, Sink::Value),
                (2, Sink::Value),
                (3, Sink::Table)
            ],
            "four verbs, three sinks, exactly one legal pair per verb"
        );
    }
}
