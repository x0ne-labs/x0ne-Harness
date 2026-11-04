//! How the single `command` string of [`crate::Action::ArtifactServe`] becomes
//! an argv — and which lines are refused outright.
//!
//! # The defect this closes
//!
//! Measured on 2026-08-15 (audit finding n°6). `ArtifactServe` was the only
//! action in the runtime that reached a shell: `Command::new("sh").arg("-c")`
//! with a line the model wrote (`x0ne-runtime/src/executor/artifact.rs`). The
//! policy engine gated the FIRST WHITESPACE TOKEN of that line
//! (`x0ne-runtime/src/policy.rs`), so everything after the first separator was
//! never seen by any gate. With `allowed_commands = ["python3"]`,
//! `allowed_read_paths = ["/tmp/x0ne-narrow"]` and
//! `allowed_write_paths = ["/tmp/x0ne-narrow"]`:
//!
//! ```text
//! {"type":"ArtifactServe","id":"cv_…",
//!  "command":"python3 --version ; cat /etc/passwd > /tmp/PWNED2-outside-write"}
//!   → success:true
//!   → /tmp/PWNED2-outside-write contains root:x:0:0:root:/root:/bin/bash
//! ```
//!
//! `;`, `>` and `$(…)` all evaluated; `cat` was not in `allowed_commands`,
//! `/etc/passwd` was not in `allowed_read_paths`, and the destination was not
//! in `allowed_write_paths`. Three gates, one separator.
//!
//! It also falsified invariant 6 of `docs/invariants.md` — "RunCommand has no
//! shell" — on which the read-only dispensation of
//! `x0ne-orchestrator/src/agent/readonly_cmd.rs` rests. That dispensation was
//! never wrong for `RunCommand`; the invariant simply was not true of the
//! runtime as a whole.
//!
//! # Why refuse the metacharacters instead of quoting them
//!
//! Two designs were available. Keep the shell and escape what goes into it, or
//! drop the shell. Escaping keeps a parser in the trust path whose grammar is
//! `sh`'s, and it leaves the gate and the executor holding two different
//! answers to "what does this line run" — the shape of defect this repository
//! has already paid for repeatedly.
//!
//! Dropping the shell costs nothing here: the one shell feature the action
//! documented, `$PORT`, is a plain string substitution the runtime performs
//! itself, not something `sh` was doing. The real commands
//! (`python3 -m http.server $PORT`, `node server.js`, `npm run dev`) are argv
//! and nothing else.
//!
//! Quotes and backslashes are refused as well, and that is what makes the
//! tokenisation trustworthy rather than merely convenient: with no quoting
//! syntax to interpret, splitting on whitespace is unambiguous, and
//! `parse` is the ONLY implementation — the policy gate and the executor
//! call the same function, so they cannot disagree about which program runs.
//!
//! # What this does not do
//!
//! It constrains the *shape* of the line, never the intent. `argv[0]` is still
//! whatever the model named; the operator's `allowed_commands` list is what
//! decides whether that program may run. An argument that legitimately
//! contains a space cannot be expressed — write a wrapper script inside the
//! artifact and serve that.

/// The one substitution the runtime performs on a serve command line. It is
/// replaced with the allocated port before [`parse`] inspects anything, so it
/// is also the only `$` form a serve command may contain.
pub const PORT_PLACEHOLDER: &str = "$PORT";

/// Characters a serve command line may not contain, each with the shell
/// capability it would carry if a shell ever saw this line again.
///
/// SECURITY: this table is what keeps one action out of one line. Removing an
/// entry needs the argument that its character cannot chain, substitute,
/// redirect, or hide a token boundary — not that it looked harmless.
pub const REFUSED_CHARACTERS: &[(char, &str)] = &[
    (';', "chains a second command"),
    ('&', "chains or backgrounds a second command"),
    ('|', "pipes into a second command"),
    ('$', "substitutes a variable or a command ($PORT excepted)"),
    ('`', "substitutes the output of a command"),
    ('(', "opens a subshell"),
    (')', "closes a subshell"),
    ('<', "redirects a file onto stdin"),
    ('>', "redirects stdout onto a file"),
    (
        '"',
        "quotes, so whitespace would no longer separate arguments",
    ),
    (
        '\'',
        "quotes, so whitespace would no longer separate arguments",
    ),
    (
        '\\',
        "escapes, so whitespace would no longer separate arguments",
    ),
];

/// Why a serve command line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeCommandError {
    /// Nothing to run once whitespace is removed.
    Empty,
    /// A shell metacharacter, quote or escape.
    Refused { ch: char, why: &'static str },
    /// A control character (newline included): it would be an invisible token
    /// boundary here and a statement separator in any shell.
    ControlCharacter { code: u32 },
}

impl ServeCommandError {
    /// Operator-facing text. It names the offending character, because a
    /// refusal that does not is one the model retries blindly and the operator
    /// cannot diagnose.
    pub fn message(&self) -> String {
        match self {
            ServeCommandError::Empty => "serve command must contain a program to run".to_string(),
            ServeCommandError::Refused { ch, why } => format!(
                "serve command refused: '{}' {} — ArtifactServe runs an argv, \
                 not a shell line, so this character would not do what it \
                 looks like it does. Use $PORT for the allocated port; put \
                 anything else in a script inside the artifact.",
                ch, why
            ),
            ServeCommandError::ControlCharacter { code } => format!(
                "serve command refused: control character U+{:04X} — \
                 ArtifactServe runs an argv, not a shell line.",
                code
            ),
        }
    }
}

/// Substitute `$PORT`, refuse anything a shell would read as syntax, and split
/// the rest on whitespace.
///
/// `port` only ever contributes decimal digits, so it cannot introduce a
/// refused character nor move a token boundary: the verdict is the same for
/// every port. That is what lets the policy gate call this with a placeholder
/// port before one is allocated and still reach the executor's answer — see
/// `the_allocated_port_cannot_change_the_verdict`.
///
/// SECURITY: the substitution happens FIRST and is the reason `$` is otherwise
/// refused. Reordering it would either make `$PORT` unusable or let `$HOME`
/// through as a literal that a later shell would expand.
pub fn parse(line: &str, port: u16) -> Result<Vec<String>, ServeCommandError> {
    let substituted = line.replace(PORT_PLACEHOLDER, &port.to_string());

    for ch in substituted.chars() {
        if let Some((_, why)) = REFUSED_CHARACTERS.iter().find(|(c, _)| *c == ch) {
            return Err(ServeCommandError::Refused { ch, why });
        }
        // Tabs and spaces are separators; every other control character is
        // refused. `\n` is the one that matters — it is a statement separator
        // in every shell and invisible in a log line.
        if ch.is_control() && ch != '\t' {
            return Err(ServeCommandError::ControlCharacter { code: ch as u32 });
        }
    }

    let argv: Vec<String> = substituted.split_whitespace().map(str::to_string).collect();
    if argv.is_empty() {
        return Err(ServeCommandError::Empty);
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY: the measurement of 2026-08-15, as a property. Every character
    /// that lets one line become two commands is refused, and the refusal says
    /// which one it was.
    ///
    /// A failure here means a serve command can once again run a program no
    /// allowlist named, read a file no read root covers, or write outside every
    /// write root — the three gates the `;` in the repro walked past at once.
    #[test]
    fn a_serve_command_line_cannot_chain_substitute_or_redirect() {
        let payloads = [
            "python3 --version ; cat /etc/passwd > /tmp/leak",
            "python3 --version && rm -rf /tmp/x",
            "python3 --version | tee /tmp/leak",
            "python3 -m http.server $(id -un)",
            "python3 -m http.server `id -un`",
            "python3 -m http.server $HOME",
            "python3 -m http.server < /etc/passwd",
            "sh -c 'cat /etc/passwd'",
            "sh -c \"cat /etc/passwd\"",
            "python3\nrm -rf /tmp/x",
            "python3 --version \\; rm",
            "python3 (subshell)",
        ];
        for payload in payloads {
            let verdict = parse(payload, 7810);
            assert!(
                verdict.is_err(),
                "a shell line was accepted as an argv: {payload:?} → {verdict:?}"
            );
            let msg = verdict.unwrap_err().message();
            assert!(
                msg.contains("refused"),
                "the refusal must say so, and name the character: {msg}"
            );
        }
    }

    /// SECURITY: each character is refused ON ITS OWN.
    ///
    /// The list here is written out by hand rather than read from
    /// [`REFUSED_CHARACTERS`], and that is the whole point: a test that
    /// iterates the table it is checking loses a case exactly when an entry is
    /// deleted, and stays green. Measured while mutating on 2026-08-15 —
    /// dropping `;` from the table left the payload-based test above green,
    /// because every payload in it also carried a `>`.
    ///
    /// A failure here means one shell capability came back.
    #[test]
    fn every_shell_metacharacter_is_refused_on_its_own() {
        for ch in [
            ';', '&', '|', '$', '`', '(', ')', '<', '>', '"', '\'', '\\', '\n', '\r', '\0',
        ] {
            let line = format!("echo a{ch}b");
            let verdict = parse(&line, 7810);
            assert!(
                verdict.is_err(),
                "{ch:?} alone was accepted in {line:?} → {verdict:?}"
            );
        }
    }

    /// The complement, and the reason this is a refusal of shell syntax rather
    /// than an allowlist of programs: the commands people actually serve must
    /// still parse, or the check gets reverted and the hole comes back.
    #[test]
    fn the_commands_people_actually_serve_still_parse() {
        assert_eq!(
            parse("python3 -m http.server $PORT", 7810).unwrap(),
            vec!["python3", "-m", "http.server", "7810"]
        );
        assert_eq!(
            parse("node server.js", 7810).unwrap(),
            vec!["node", "server.js"]
        );
        assert_eq!(
            parse("npm run dev", 7810).unwrap(),
            vec!["npm", "run", "dev"]
        );
        // Repeated placeholders, and one glued to other text: both are plain
        // string substitution, and neither is shell syntax.
        assert_eq!(
            parse("serve -l $PORT --url http://127.0.0.1:$PORT/", 7899).unwrap(),
            vec!["serve", "-l", "7899", "--url", "http://127.0.0.1:7899/"]
        );
    }

    /// SECURITY: the policy gate runs BEFORE a port exists, so it parses with a
    /// placeholder while the executor parses with the allocated one. If the
    /// port could change the verdict or the program, the gate would be ruling
    /// on a different command line than the one that runs — this repository's
    /// named recurring defect, in its two-implementations form.
    ///
    /// A failure here means the gate and the executor can disagree.
    #[test]
    fn the_allocated_port_cannot_change_the_verdict() {
        for line in [
            "python3 -m http.server $PORT",
            "python3 --version ; rm -rf /tmp/x",
            "node server.js",
            "$PORT",
            "",
            "   ",
        ] {
            let with_placeholder = parse(line, 0);
            for port in [1u16, 7810, 7899, 65535] {
                let with_real = parse(line, port);
                assert_eq!(
                    with_placeholder.is_ok(),
                    with_real.is_ok(),
                    "port {port} changed the verdict for {line:?}"
                );
                // Where the placeholder is absent, the argv itself must be
                // identical — the port is not allowed to touch anything it was
                // not asked to.
                if !line.contains(PORT_PLACEHOLDER) {
                    assert_eq!(
                        with_placeholder, with_real,
                        "port {port} changed the argv for {line:?}"
                    );
                }
            }
        }
    }

    /// An empty line is a refusal, not a spawn of nothing.
    #[test]
    fn an_empty_serve_command_is_refused() {
        assert_eq!(parse("", 7810), Err(ServeCommandError::Empty));
        assert_eq!(parse("   \t ", 7810), Err(ServeCommandError::Empty));
    }

    /// Characters that a shell treats as globs or as `~` expansion are NOT
    /// refused: with no shell they are literal argv bytes, and they cannot
    /// chain, substitute or redirect. Pinned so the refusal list stays a list
    /// of capabilities rather than growing into a list of punctuation — and
    /// because `artifact_serve_has_no_shell` proves the absence of a shell by
    /// showing `~` arriving unexpanded.
    #[test]
    fn characters_that_cannot_run_a_second_program_are_left_alone() {
        assert_eq!(parse("echo ~", 7810).unwrap(), vec!["echo", "~"]);
        assert_eq!(parse("echo a*b", 7810).unwrap(), vec!["echo", "a*b"]);
        assert_eq!(parse("echo {a,b}", 7810).unwrap(), vec!["echo", "{a,b}"]);
        assert_eq!(parse("echo #x", 7810).unwrap(), vec!["echo", "#x"]);
    }
}
