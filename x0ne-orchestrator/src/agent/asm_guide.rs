//! The asm-ai guide injected into the system prompt.
//!
//! English, descriptive: the model reads it. Frozen once measured: a language
//! that moves kills the emission rate, because the model relearns it each
//! time. Corpus B (`tests/corpus_b.rs`) is what measures that rate.

/// The guide. Eight rules, the lexical forms, the matrix, four worked examples.
pub const GUIDE: &str = r#"
## asm-ai — one program instead of forty tool calls

Use `preview_program` when a task touches many files the same way. X0NE compiles
the program, derives which files it can read and write from the compiled module,
proves that against the policy, runs it without writing anything, and returns a
diff. Show the diff, then call `run_program` with the same source to apply it —
that second call is the one a human approves.

Eight rules:
1. A program is one pipeline: `from files` … optional `where` … one verb … `emit`.
2. There are no loops, no variables and no conditions. That is the whole language.
3. Every program starts with the exact line `#!asm-ai/v0`.
4. `from files "<glob>"` — globset syntax, relative to the project root, no `..`
   and no leading `/`.
5. `where name match /…/` filters on the path; `where lines match /…/` keeps a
   file when at least one of its lines matches. `where` is optional.
6. Verbs: `edit replace /find/ -> "replacement"`, `count files`,
   `count matches` (requires `where lines match`), `read lines /…/`.
7. Three verb/emit pairs exist and no others: edit→diff, count→value, read→table.
8. Patterns are `/…/` with an optional `i` flag, `\/` for a literal slash, and no
   backreferences or look-around. Replacements are `"…"` with `${1}`, `${2}` for
   capture groups and `$$` for a literal dollar. `${n}` must name a group the
   pattern actually has.

```asm-ai
#!asm-ai/v0
from files "tests/**/*.rs"
edit replace /fn test_(\w+)\(/ -> "fn ${1}_property("
emit diff
```

```asm-ai
#!asm-ai/v0
from files "src/**/*.rs"
where lines match /unwrap\(\)/
count matches
emit value
```

```asm-ai
#!asm-ai/v0
from files "**/*.md"
read lines /^## /
emit table
```

```asm-ai
#!asm-ai/v0
from files "src/**/*.rs"
where name match /_test\.rs$/
count files
emit value
```

If a refusal comes back, it names the rule and the fix. Apply the fix and emit
the program again. If a task does not fit the four verbs, use ordinary actions
instead — a program that is nearly right is worth less than a tool call that is.
"#;

/// Non-empty lines of the guide: what the budget counts.
pub fn line_count() -> usize {
    GUIDE.lines().filter(|l| !l.trim().is_empty()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is a product decision: the guide enters every prompt.
    #[test]
    fn the_guide_fits_its_sixty_line_budget() {
        assert!(
            line_count() <= 60,
            "the guide is {} lines, the budget is 60",
            line_count()
        );
    }

    /// The guide describes exactly the grammar the parser accepts. A guide
    /// that promises a refused form is the fastest way to sink the rate.
    #[test]
    fn every_worked_example_in_the_guide_actually_parses() {
        let mut checked = 0;
        for block in GUIDE.split("```").skip(1).step_by(2) {
            let src = block.trim_start_matches("asm-ai").trim_start();
            if !src.starts_with("#!asm-ai/v0") {
                continue;
            }
            x0ne_asm::parse(src)
                .unwrap_or_else(|r| panic!("a guide example does not parse:\n{}", r.render()));
            checked += 1;
        }
        assert_eq!(checked, 4, "the guide carries four worked examples");
    }

    /// Both halves of the two-step dance, and which one a human approves.
    #[test]
    fn the_guide_names_both_halves_of_the_two_step_dance() {
        assert!(GUIDE.contains("preview_program"));
        assert!(GUIDE.contains("run_program"));
    }

    /// Nothing tells the model it is deceived, confined, or mistaken.
    #[test]
    fn the_guide_says_nothing_hostile_about_the_model() {
        for word in [
            "sandbox",
            "you cannot",
            "you are not allowed",
            "trapped",
            "you believe",
        ] {
            assert!(
                !GUIDE.to_lowercase().contains(word),
                "the guide contains `{word}`"
            );
        }
    }
}
