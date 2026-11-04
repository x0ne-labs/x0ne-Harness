use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::action::Action;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

// ── HumanPrompt trait ─────────────────────────────────────────────────────────
//
// Interactive primitives (AskHuman, Confirm) need to read from stdin in
// production and from a scripted source in tests. We expose a process-global
// swappable HumanPrompt so tests can inject deterministic responses without
// having to thread an extra argument through every executor signature.

/// Outcome of an interactive multiple-choice question.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    /// 0-based index into the original `choices` slice. None when `skipped` or `other`.
    pub index: Option<usize>,
    /// The chosen label, or the free-form text when `other=true`. None when `skipped`.
    pub value: Option<String>,
    /// True when the operator picked the "Skip" escape hatch.
    pub skipped: bool,
    /// True when the operator picked "Other (specify)" and typed a custom string.
    pub other: bool,
}

/// Anything that can answer questions on behalf of a human operator.
pub trait HumanPrompt: Send + Sync {
    /// Answer a freeform question. Should return the typed answer (no trailing newline).
    fn ask(&self, question: &str) -> io::Result<String>;

    /// Answer a yes/no question. Should return whether the user confirmed.
    /// `message` is the wording shown to the user (without the [y/N] suffix).
    fn confirm(&self, message: &str) -> io::Result<bool>;

    /// Answer a multiple-choice question. Implementors must validate the
    /// returned ChoiceAnswer is internally consistent (exactly one of
    /// `index`/`skipped`/`other` is set).
    fn choose(
        &self,
        question: &str,
        choices: &[String],
        allow_skip: bool,
        allow_other: bool,
    ) -> io::Result<ChoiceAnswer>;
}

/// Default implementation: write the prompt to stderr, read a line from stdin.
pub struct StdioPrompt;

static STDIN_IS_PROTOCOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by `main` in stdio and MCP modes, where fd 0 carries the protocol.
pub fn mark_stdin_as_protocol() {
    STDIN_IS_PROTOCOL.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Read on the bridge, a prompt took the next request as its answer
/// (measured 2026-10-05). No terminal on fd 0 means no human to ask.
fn human_on_stdin() -> io::Result<()> {
    use std::io::IsTerminal;
    if STDIN_IS_PROTOCOL.load(std::sync::atomic::Ordering::SeqCst) || !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no human is reachable from the runtime: stdin is not a terminal \
             (in stdio/MCP mode it carries the protocol)",
        ));
    }
    Ok(())
}

impl HumanPrompt for StdioPrompt {
    fn ask(&self, question: &str) -> io::Result<String> {
        human_on_stdin()?;
        let stderr = io::stderr();
        let mut err = stderr.lock();
        writeln!(err, "\n[X0NE] ❓ {}", question)?;
        write!(err, "[X0NE] > ")?;
        err.flush()?;

        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }

    fn confirm(&self, message: &str) -> io::Result<bool> {
        human_on_stdin()?;
        let stderr = io::stderr();
        let mut err = stderr.lock();
        writeln!(err, "\n[X0NE] ⚡ Confirm: {} [y/N]", message)?;
        err.flush()?;

        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        Ok(parse_yes_no(&line))
    }

    fn choose(
        &self,
        question: &str,
        choices: &[String],
        allow_skip: bool,
        allow_other: bool,
    ) -> io::Result<ChoiceAnswer> {
        human_on_stdin()?;
        let stderr = io::stderr();
        let mut err = stderr.lock();
        writeln!(err, "\n[X0NE] ❓ {}", question)?;
        for (i, c) in choices.iter().enumerate() {
            writeln!(err, "   {}) {}", i + 1, c)?;
        }
        if allow_other {
            writeln!(err, "   o) Other (specify)")?;
        }
        if allow_skip {
            writeln!(err, "   s) Skip")?;
        }
        write!(err, "[X0NE] > ")?;
        err.flush()?;
        drop(err);

        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let raw = line.trim().to_lowercase();

        if allow_skip && (raw == "s" || raw == "skip") {
            return Ok(ChoiceAnswer {
                index: None,
                value: None,
                skipped: true,
                other: false,
            });
        }
        if allow_other && (raw == "o" || raw == "other") {
            // Two-step: prompt for the free-form value
            let mut err2 = io::stderr().lock();
            write!(err2, "[X0NE] specify > ")?;
            err2.flush()?;
            drop(err2);
            let mut other_line = String::new();
            io::stdin().lock().read_line(&mut other_line)?;
            return Ok(ChoiceAnswer {
                index: None,
                value: Some(other_line.trim().to_string()),
                skipped: false,
                other: true,
            });
        }
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= choices.len() => Ok(ChoiceAnswer {
                index: Some(n - 1),
                value: Some(choices[n - 1].clone()),
                skipped: false,
                other: false,
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid choice {:?} (expected 1..={}{}{})",
                    raw,
                    choices.len(),
                    if allow_other { ", o" } else { "" },
                    if allow_skip { ", s" } else { "" }
                ),
            )),
        }
    }
}

/// Test prompt: drains a queue of pre-recorded responses.
///
/// Each call to `ask` consumes one queued string verbatim.
/// Each call to `confirm` consumes one queued string and parses it as yes/no.
/// If the queue is empty, returns an `UnexpectedEof` error so the test fails loudly
/// rather than blocking on real stdin.
pub struct ScriptedPrompt {
    queue: Mutex<VecDeque<String>>,
}

impl ScriptedPrompt {
    pub fn new<I, S>(responses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let queue: VecDeque<String> = responses.into_iter().map(|s| s.into()).collect();
        Self {
            queue: Mutex::new(queue),
        }
    }

    fn pop(&self, ctx: &str) -> io::Result<String> {
        self.queue.lock().unwrap().pop_front().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("ScriptedPrompt: no response queued for {}", ctx),
            )
        })
    }
}

impl HumanPrompt for ScriptedPrompt {
    fn ask(&self, question: &str) -> io::Result<String> {
        let answer = self.pop(&format!("ask({:?})", question))?;
        Ok(answer.trim().to_string())
    }

    fn confirm(&self, message: &str) -> io::Result<bool> {
        let raw = self.pop(&format!("confirm({:?})", message))?;
        Ok(parse_yes_no(&raw))
    }

    fn choose(
        &self,
        question: &str,
        choices: &[String],
        allow_skip: bool,
        allow_other: bool,
    ) -> io::Result<ChoiceAnswer> {
        let raw = self.pop(&format!("choose({:?})", question))?;
        parse_choice(&raw, choices, allow_skip, allow_other)
    }
}

/// Parse a scripted choice response. Accepts (case-insensitive, trimmed):
/// - `"1"`, `"2"`, ... `"N"` → numeric pick (1-based)
/// - `"s"` or `"skip"` → skipped (only when `allow_skip`)
/// - `"o:my answer"` or `"other:my answer"` → other (only when `allow_other`)
///   The text after the colon becomes the free-form value.
fn parse_choice(
    raw: &str,
    choices: &[String],
    allow_skip: bool,
    allow_other: bool,
) -> io::Result<ChoiceAnswer> {
    let trimmed = raw.trim();
    let lowered = trimmed.to_lowercase();

    if allow_skip && (lowered == "s" || lowered == "skip") {
        return Ok(ChoiceAnswer {
            index: None,
            value: None,
            skipped: true,
            other: false,
        });
    }
    if allow_other {
        if let Some(rest) = trimmed
            .strip_prefix("o:")
            .or_else(|| trimmed.strip_prefix("O:"))
        {
            return Ok(ChoiceAnswer {
                index: None,
                value: Some(rest.trim().to_string()),
                skipped: false,
                other: true,
            });
        }
        if let Some(rest) = trimmed
            .strip_prefix("other:")
            .or_else(|| trimmed.strip_prefix("Other:"))
        {
            return Ok(ChoiceAnswer {
                index: None,
                value: Some(rest.trim().to_string()),
                skipped: false,
                other: true,
            });
        }
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n >= 1 && n <= choices.len() => Ok(ChoiceAnswer {
            index: Some(n - 1),
            value: Some(choices[n - 1].clone()),
            skipped: false,
            other: false,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Invalid choice {:?} (expected 1..={}{}{})",
                trimmed,
                choices.len(),
                if allow_other { ", o:<text>" } else { "" },
                if allow_skip { ", s" } else { "" }
            ),
        )),
    }
}

/// Parse a yes/no answer. Accepts (case-insensitively, after trim):
/// `y`, `yes`, `o`, `oui`, `true`, `1` → true; everything else → false.
fn parse_yes_no(raw: &str) -> bool {
    matches!(
        raw.trim().to_lowercase().as_str(),
        "y" | "yes" | "o" | "oui" | "true" | "1"
    )
}

// ── Global prompt slot ────────────────────────────────────────────────────────

static PROMPT: OnceLock<RwLock<Arc<dyn HumanPrompt>>> = OnceLock::new();

fn slot() -> &'static RwLock<Arc<dyn HumanPrompt>> {
    PROMPT.get_or_init(|| RwLock::new(Arc::new(StdioPrompt) as Arc<dyn HumanPrompt>))
}

/// Install a custom prompt globally. Returns the previous prompt for restoration.
/// Tests should call this before exercising AskHuman/Confirm and restore on teardown.
pub fn install_prompt(prompt: Arc<dyn HumanPrompt>) -> Arc<dyn HumanPrompt> {
    let mut guard = slot().write().unwrap();
    std::mem::replace(&mut *guard, prompt)
}

/// If `X0NE_SCRIPTED_ANSWERS` is set, parse it as a `;`-separated list of
/// canned responses and install a `ScriptedPrompt`. Returns `true` when the
/// var was honored.
///
/// Used by `x0ne --self-test`: interactive primitives (AskHuman, Confirm,
/// AskMultipleChoice) cannot read from real stdin because the bridge
/// protocol already owns it, so the self-test seeds canned answers via env.
///
/// Example: `X0NE_SCRIPTED_ANSWERS="42;yes;2;o:hack"` queues four responses
/// consumed in order by successive prompt calls.
///
/// **Not compiled into a normal release build.** This function is the bridge
/// from an environment variable to the process-global prompt slot, which makes
/// it an authority source: whoever sets the variable answers every
/// confirmation the runtime asks. It used to be compiled unconditionally,
/// directly above a self-test entry point that was not — so the affordance
/// outlived the only thing that needed it, in every shipped binary.
///
/// The predicate is the one the repository already uses for exactly this
/// distinction (`main.rs:71`, `lib.rs:30`, `self_test.rs:18`), so the
/// affordance and the self-test it serves now appear and disappear together.
/// Enable deliberately with `cargo build --release --features self_test`.
///
/// `ScriptedPrompt` and `install_prompt` are intentionally **not** gated:
/// `x0ne-scenario-runner` supplies answers from its own code, which is a
/// different thing from an environment variable a caller can set on a
/// process they did not build.
#[cfg(any(test, debug_assertions, feature = "self_test"))]
pub fn install_scripted_from_env() -> bool {
    match std::env::var("X0NE_SCRIPTED_ANSWERS") {
        Ok(raw) if !raw.is_empty() => {
            let answers: Vec<String> = raw.split(';').map(|s| s.to_string()).collect();
            install_prompt(Arc::new(ScriptedPrompt::new(answers)));
            true
        }
        _ => false,
    }
}

/// No reachable human is a missing dependency, not a runtime bug.
fn prompt_error_kind(e: &io::Error) -> ErrorKind {
    match e.kind() {
        io::ErrorKind::Unsupported => ErrorKind::MissingDependency,
        _ => ErrorKind::Internal,
    }
}

fn current_prompt() -> Arc<dyn HumanPrompt> {
    slot().read().unwrap().clone()
}

// ── Action executors ──────────────────────────────────────────────────────────

/// Present a question to the human operator and read their answer.
pub async fn ask_human(action: &Action) -> ActionResult {
    let Action::AskHuman { question } = action else {
        unreachable!()
    };

    match current_prompt().ask(question) {
        Ok(answer) => ActionResult::ok(
            "AskHuman",
            ActionOutput::HumanAnswer {
                question: question.clone(),
                answer,
            },
            0,
        ),
        Err(e) => ActionResult::err("AskHuman", prompt_error_kind(&e), e.to_string(), 0),
    }
}

/// Ask the human operator for a yes/no confirmation.
pub async fn confirm(action: &Action) -> ActionResult {
    let Action::Confirm { message } = action else {
        unreachable!()
    };

    match current_prompt().confirm(message) {
        Ok(confirmed) => ActionResult::ok(
            "Confirm",
            ActionOutput::HumanConfirmed {
                message: message.clone(),
                confirmed,
            },
            0,
        ),
        Err(e) => ActionResult::err("Confirm", prompt_error_kind(&e), e.to_string(), 0),
    }
}

/// Ask the human operator a multiple-choice question (QCM).
pub async fn ask_multiple_choice(action: &Action) -> ActionResult {
    let Action::AskMultipleChoice {
        question,
        choices,
        allow_skip,
        allow_other,
    } = action
    else {
        unreachable!()
    };

    if choices.is_empty() {
        return ActionResult::err(
            "AskMultipleChoice",
            ErrorKind::InvalidInput,
            "AskMultipleChoice requires at least one choice.",
            0,
        );
    }
    if choices.len() > 20 {
        return ActionResult::err(
            "AskMultipleChoice",
            ErrorKind::InvalidInput,
            format!(
                "AskMultipleChoice supports up to 20 choices, got {}.",
                choices.len()
            ),
            0,
        );
    }

    match current_prompt().choose(question, choices, *allow_skip, *allow_other) {
        Ok(answer) => ActionResult::ok(
            "AskMultipleChoice",
            ActionOutput::HumanChoice {
                question: question.clone(),
                choices: choices.clone(),
                choice_index: answer.index,
                answer: answer.value,
                skipped: answer.skipped,
                other: answer.other,
            },
            0,
        ),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => ActionResult::err(
            "AskMultipleChoice",
            ErrorKind::InvalidInput,
            e.to_string(),
            0,
        ),
        Err(e) => ActionResult::err("AskMultipleChoice", prompt_error_kind(&e), e.to_string(), 0),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Cargo runs tests in parallel by default. Since `install_prompt` mutates
    /// process-global state, every test that touches the prompt must acquire
    /// this lock to avoid racing siblings.
    static PROMPT_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ── The environment is not an authority ──────────────────────────────
    //
    // `X0NE_SCRIPTED_ANSWERS` exists so `x0ne --self-test` can exercise
    // AskHuman, Confirm and AskMultipleChoice: the bridge protocol owns real
    // stdin, so those primitives have no human to reach. That is a legitimate
    // development affordance. It became a production defect because the bridge
    // from the variable to the prompt slot was compiled into every build,
    // while the self-test entry point it serves was not — so a shipped
    // `x0ne-runtime` would answer every confirmation from an environment
    // variable an attacker could set.

    /// The source of the file, comments stripped and tests removed. Comments
    /// naming the banned construct in order to say we no longer compile it
    /// must not trip the scan.
    fn production_code() -> String {
        let full = include_str!("human.rs");
        let head = &full[..full.find("#[cfg(test)]").unwrap_or(full.len())];
        head.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The gate is the repository's existing one — the same predicate that
    /// already guards the self-test entry point at `main.rs:71` and
    /// `lib.rs:30`. Reusing it means the affordance and the thing it serves
    /// appear and disappear together, which is what went wrong.
    #[test]
    fn the_environment_bridge_is_compiled_out_of_a_release_build() {
        let code = production_code();
        let at = code
            .find("pub fn install_scripted_from_env")
            .expect("the env bridge");
        let preceding = &code[..at];
        assert!(
            preceding
                .trim_end()
                .ends_with(r#"#[cfg(any(test, debug_assertions, feature = "self_test"))]"#),
            "`install_scripted_from_env` is not guarded by the self-test cfg. \
             A release binary would let X0NE_SCRIPTED_ANSWERS answer AskHuman, \
             Confirm and AskMultipleChoice."
        );
    }

    /// No replacement authority slipped in beside it. The production half of
    /// this file may read exactly one environment variable, inside the gated
    /// function — and after the gate, zero in a release build.
    #[test]
    fn no_other_environment_variable_can_supply_answers() {
        let code = production_code();
        let reads: Vec<_> = code.match_indices("env::var").collect();
        assert_eq!(
            reads.len(),
            1,
            "human.rs reads {} environment variables; the prompt slot takes \
             its authority from code, not from the environment",
            reads.len()
        );
        let gate = code
            .find(r#"#[cfg(any(test, debug_assertions, feature = "self_test"))]"#)
            .expect("the gate");
        assert!(
            reads[0].0 > gate,
            "the environment read sits outside the gated region"
        );
    }

    #[test]
    fn parse_yes_no_accepts_common_yes_forms() {
        for s in ["y", "Y", "yes", "YES", " yes ", "o", "oui", "TRUE", "1"] {
            assert!(parse_yes_no(s), "expected {:?} → true", s);
        }
    }

    #[test]
    fn parse_yes_no_rejects_no_and_garbage() {
        for s in ["n", "no", "NO", "", "   ", "maybe", "yep", "0", "false"] {
            assert!(!parse_yes_no(s), "expected {:?} → false", s);
        }
    }

    #[test]
    fn scripted_prompt_drains_queue_in_order() {
        let p = ScriptedPrompt::new(["hello", "world"]);
        assert_eq!(p.ask("q1").unwrap(), "hello");
        assert_eq!(p.ask("q2").unwrap(), "world");
        assert!(p.ask("q3").is_err());
    }

    #[test]
    fn scripted_prompt_parses_confirm_answers() {
        let p = ScriptedPrompt::new(["yes", "n", "garbage"]);
        assert!(p.confirm("a").unwrap());
        assert!(!p.confirm("b").unwrap());
        assert!(!p.confirm("c").unwrap());
    }

    #[tokio::test]
    async fn ask_human_uses_installed_prompt() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new(["42"])));
        let action = Action::AskHuman {
            question: "what?".into(),
        };
        let res = ask_human(&action).await;
        assert!(res.success);
        if let Some(ActionOutput::HumanAnswer { answer, .. }) = res.output {
            assert_eq!(answer, "42");
        } else {
            panic!("unexpected output");
        }
        install_prompt(prev);
    }

    #[test]
    fn parse_choice_accepts_numeric_pick() {
        let choices = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let r = parse_choice("2", &choices, false, false).unwrap();
        assert_eq!(r.index, Some(1));
        assert_eq!(r.value.as_deref(), Some("B"));
        assert!(!r.skipped && !r.other);
    }

    #[test]
    fn parse_choice_rejects_out_of_range() {
        let choices = vec!["A".to_string(), "B".to_string()];
        assert!(parse_choice("0", &choices, false, false).is_err());
        assert!(parse_choice("3", &choices, false, false).is_err());
        assert!(parse_choice("garbage", &choices, false, false).is_err());
    }

    #[test]
    fn parse_choice_skip_requires_allow_skip() {
        let choices = vec!["A".to_string()];
        assert!(parse_choice("s", &choices, false, false).is_err());
        assert!(parse_choice("skip", &choices, false, false).is_err());
        let r = parse_choice("s", &choices, true, false).unwrap();
        assert!(r.skipped);
        assert!(r.index.is_none() && r.value.is_none() && !r.other);
    }

    #[test]
    fn parse_choice_other_carries_freeform_value() {
        let choices = vec!["A".to_string()];
        assert!(parse_choice("o:custom", &choices, false, false).is_err());
        let r = parse_choice("o:my custom answer", &choices, false, true).unwrap();
        assert!(r.other);
        assert_eq!(r.value.as_deref(), Some("my custom answer"));
        assert!(r.index.is_none() && !r.skipped);

        let r2 = parse_choice("other:another one", &choices, false, true).unwrap();
        assert!(r2.other);
        assert_eq!(r2.value.as_deref(), Some("another one"));
    }

    #[tokio::test]
    async fn ask_multiple_choice_picks_by_index() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new(["2"])));
        let action = Action::AskMultipleChoice {
            question: "pick one".into(),
            choices: vec!["alpha".into(), "beta".into(), "gamma".into()],
            allow_skip: false,
            allow_other: false,
        };
        let res = ask_multiple_choice(&action).await;
        assert!(res.success);
        if let Some(ActionOutput::HumanChoice {
            choice_index,
            answer,
            skipped,
            other,
            ..
        }) = res.output
        {
            assert_eq!(choice_index, Some(1));
            assert_eq!(answer.as_deref(), Some("beta"));
            assert!(!skipped && !other);
        } else {
            panic!("unexpected output");
        }
        install_prompt(prev);
    }

    #[tokio::test]
    async fn ask_multiple_choice_supports_skip() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new(["s"])));
        let res = ask_multiple_choice(&Action::AskMultipleChoice {
            question: "?".into(),
            choices: vec!["a".into(), "b".into()],
            allow_skip: true,
            allow_other: false,
        })
        .await;
        assert!(res.success);
        match res.output {
            Some(ActionOutput::HumanChoice {
                skipped,
                choice_index,
                answer,
                ..
            }) => {
                assert!(skipped);
                assert!(choice_index.is_none() && answer.is_none());
            }
            _ => panic!("unexpected"),
        }
        install_prompt(prev);
    }

    #[tokio::test]
    async fn ask_multiple_choice_supports_other() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new(["o:none of the above"])));
        let res = ask_multiple_choice(&Action::AskMultipleChoice {
            question: "?".into(),
            choices: vec!["a".into(), "b".into()],
            allow_skip: false,
            allow_other: true,
        })
        .await;
        assert!(res.success);
        match res.output {
            Some(ActionOutput::HumanChoice {
                other,
                answer,
                choice_index,
                skipped,
                ..
            }) => {
                assert!(other && !skipped);
                assert_eq!(answer.as_deref(), Some("none of the above"));
                assert!(choice_index.is_none());
            }
            _ => panic!("unexpected"),
        }
        install_prompt(prev);
    }

    #[tokio::test]
    async fn ask_multiple_choice_rejects_empty_choices() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new::<[&str; 0], _>([])));
        let res = ask_multiple_choice(&Action::AskMultipleChoice {
            question: "?".into(),
            choices: vec![],
            allow_skip: false,
            allow_other: false,
        })
        .await;
        assert!(!res.success);
        install_prompt(prev);
    }

    #[tokio::test]
    async fn confirm_uses_installed_prompt() {
        let _guard = PROMPT_TEST_LOCK.lock().unwrap();
        let prev = install_prompt(Arc::new(ScriptedPrompt::new(["yes", "n"])));
        let yes = confirm(&Action::Confirm {
            message: "ok?".into(),
        })
        .await;
        let no = confirm(&Action::Confirm {
            message: "ok?".into(),
        })
        .await;
        assert!(yes.success && no.success);
        match yes.output {
            Some(ActionOutput::HumanConfirmed { confirmed, .. }) => assert!(confirmed),
            _ => panic!("unexpected"),
        }
        match no.output {
            Some(ActionOutput::HumanConfirmed { confirmed, .. }) => assert!(!confirmed),
            _ => panic!("unexpected"),
        }
        install_prompt(prev);
    }
}
