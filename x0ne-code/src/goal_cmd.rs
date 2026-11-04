//! `/until <cmd>`: the goal command as an action of the session.
//!
//! The command ran with `sh -c` in this process until 2026-10-31 (audit of
//! 2026-10-30, H4-2): on the host, with the operator's whole environment,
//! outside the policy and the isolation every other command goes through, and
//! again after every turn. The code it runs is the workspace's, which the
//! model writes. It is now a `RunCommand` the session runs through its
//! ordinary gates: the human gate, the runtime's policy, the runtime's
//! isolation. Nothing in this module executes anything.

use serde_json::{json, Value};

/// The action for a goal command.
///
/// Plain words become `command` and `args`, so the gate, the trust roots and
/// the policy see the program itself. Shell syntax (pipes, redirections,
/// substitutions, globs, an assignment) is one argument to `sh -c`, so they
/// see a shell and judge it as one.
pub fn action_for(cmd: &str) -> Value {
    match words_of(cmd) {
        Some(words) if !words.is_empty() => json!({
            "type": "RunCommand",
            "command": words[0],
            "args": words[1..],
        }),
        _ => json!({"type": "RunCommand", "command": "sh", "args": ["-c", cmd]}),
    }
}

/// What the runtime said about one run of the goal command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCheck {
    /// Exit 0.
    Passed,
    /// A non-zero exit; `output` is stderr, or stdout when stderr is empty.
    Failed { code: i64, output: String },
    /// The command did not run: refused by the gate or the policy, no
    /// runtime, or an answer without an exit code. Never read as passed.
    Refused(String),
}

/// Reads the runtime's envelope. An exit code decides; anything else is a
/// refusal, in the runtime's own words when it gave any.
pub fn check_of(envelope: &Value) -> GoalCheck {
    let output = &envelope["output"];
    match output.get("exit_code").and_then(Value::as_i64) {
        Some(0) => GoalCheck::Passed,
        Some(code) => {
            let text = |key: &str| output.get(key).and_then(Value::as_str).unwrap_or("");
            let stderr = text("stderr");
            let shown = if stderr.is_empty() {
                text("stdout")
            } else {
                stderr
            };
            GoalCheck::Failed {
                code,
                output: shown.to_string(),
            }
        }
        None => GoalCheck::Refused(
            envelope["error"]["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("the runtime returned no exit code")
                .to_string(),
        ),
    }
}

/// The first `n` lines.
pub fn head(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Splits on blanks, honouring `'…'`, `"…"` and a backslash. `None` when the
/// line needs a shell: an operator or a glob outside quotes, an expansion
/// inside double quotes, an unbalanced quote, an assignment in first position.
fn words_of(cmd: &str) -> Option<Vec<String>> {
    const SHELL: &[char] = &[
        '|', '&', ';', '<', '>', '(', ')', '$', '`', '*', '?', '[', ']', '{', '}', '~', '#', '!',
    ];
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return None,
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`') => return None,
                        Some('\\') => word.push(chars.next()?),
                        Some(c) => word.push(c),
                        None => return None,
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c if SHELL.contains(&c) => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.first().is_some_and(|w| w.contains('=')) {
        return None;
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_name_the_program_and_its_arguments() {
        let a = action_for("cargo test --workspace");
        assert_eq!(a["type"], "RunCommand");
        assert_eq!(a["command"], "cargo");
        assert_eq!(a["args"], json!(["test", "--workspace"]));
    }

    #[test]
    fn quotes_and_backslashes_group_a_word() {
        let a = action_for(r#"grep -r 'a b' "c d" e\ f src"#);
        assert_eq!(a["command"], "grep");
        assert_eq!(a["args"], json!(["-r", "a b", "c d", "e f", "src"]));
    }

    /// Only a shell can run these, so a shell is what the gate is shown.
    #[test]
    fn shell_syntax_is_handed_to_a_shell_as_one_argument() {
        for cmd in [
            "cargo test | tail -1",
            "make && ./run",
            "FOO=1 ./check",
            "echo $HOME",
            "echo \"$HOME\"",
            "ls *.rs",
            "cat < in > out",
            "echo 'unbalanced",
            "./run; rm -rf .",
        ] {
            let a = action_for(cmd);
            assert_eq!(a["command"], "sh", "{cmd}");
            assert_eq!(a["args"], json!(["-c", cmd]), "{cmd}");
        }
    }

    #[test]
    fn the_exit_code_decides_and_nothing_else_passes() {
        assert_eq!(
            check_of(&json!({"success": true, "output": {"exit_code": 0}})),
            GoalCheck::Passed
        );
        assert_eq!(
            check_of(&json!({"success": true, "output": {
                "exit_code": 101, "stdout": "running", "stderr": "error[E0425]"}})),
            GoalCheck::Failed {
                code: 101,
                output: "error[E0425]".into()
            }
        );
        assert_eq!(
            check_of(
                &json!({"success": true, "output": {"exit_code": 2, "stdout": "FAIL t1", "stderr": ""}})
            ),
            GoalCheck::Failed {
                code: 2,
                output: "FAIL t1".into()
            }
        );
        assert_eq!(
            check_of(&json!({"success": false, "error": {"message": "needs a human's approval"}})),
            GoalCheck::Refused("needs a human's approval".into())
        );
        // A success envelope without an exit code is not a pass.
        assert_eq!(
            check_of(&json!({"success": true, "output": {"actions": []}})),
            GoalCheck::Refused("the runtime returned no exit code".into())
        );
    }

    #[test]
    fn head_keeps_the_first_lines() {
        assert_eq!(head("a\nb\nc", 2), "a\nb");
        assert_eq!(head("", 3), "");
    }
}
