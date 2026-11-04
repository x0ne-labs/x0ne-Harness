//! The CLI's presentation adapter.
//!
//! Everything the agent loop wants a person to see or answer goes through
//! here, and nothing else does. The sink renders and it transports an answer.
//! It decides nothing.
//!
//! That sentence is the point of the file. The prompt it replaces did decide:
//! `terminal.rs:1238-1252` printed an action type and three bare letters —
//! `y  n  a` — and answering `a` set `config.ui.auto_confirm = true`, which
//! `needs_confirmation` reads first, so every subsequent action of every kind
//! skipped the gate, in this session and in every sub-agent that inherited the
//! cloned config. A renderer had been given the power to disable the security
//! boundary, and the only statement of that scope was a line printed *after*
//! the choice was made.
//!
//! Here the sink returns a [`ConfirmationResponse`] carrying the id of the
//! question it was asked and nothing more. What that answer authorises is
//! `ApprovalAuthority`'s business, and the scope is printed *before* the
//! choice, in the words the authority itself supplied.

use std::io::Write;

use colored::Colorize;

use crate::agent::approval::ConfirmationResponse;
use crate::display_sink::{ConfirmOutcome, ConfirmationRequest, DisplaySink};
use crate::tui::wire::AgentEvent;

/// Renders to the terminal the CLI already owns.
pub struct CliDisplaySink {
    /// Whether a person can be reached. Resolved once at construction from the
    /// terminal, never from a config flag: `confirm_writes` and `auto_confirm`
    /// describe what the operator wants shown, not whether anyone is there.
    interactive: bool,
}

impl Default for CliDisplaySink {
    fn default() -> Self {
        Self::new()
    }
}

impl CliDisplaySink {
    pub fn new() -> Self {
        Self {
            interactive: crate::terminal::can_prompt_interactively(),
        }
    }

    /// For tests and for frontends that supply their own answer channel.
    pub fn with_interactivity(interactive: bool) -> Self {
        Self { interactive }
    }

    /// Read one line, or `None` at EOF. EOF is not an answer.
    /// Read one of `valid` as a single keypress, with no Enter.
    ///
    /// A confirmation is a choice among three or fewer labelled options, so
    /// the Enter was pure ceremony — two keystrokes to answer a one-character
    /// question, on a prompt that fires often enough for the difference to be
    /// felt. Removing it changes nothing about *what* is being answered.
    ///
    /// Fail-closed is preserved in every branch. A key outside `valid` is
    /// ignored and the prompt keeps waiting rather than resolving to a
    /// default; only an explicit abort (Esc, `q`, Ctrl-C, Ctrl-D) or a closed
    /// stdin ends the wait, and each of those returns `None`, which the caller
    /// turns into a denial. There is no keystroke, and no absence of one, that
    /// approves.
    ///
    /// Returns `None` when raw mode is unavailable, so the caller falls back
    /// to [`Self::read_line`] rather than losing the ability to answer at all.
    fn read_choice(valid: &[char]) -> Option<Option<char>> {
        use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

        // Not a terminal, or a terminal that refuses raw mode: report that
        // this path is unusable so the caller reads a line instead. Returning
        // a denial here would make a pipe look like a refusal.
        if crossterm::terminal::enable_raw_mode().is_err() {
            return None;
        }

        let chosen = loop {
            let ev = match event::read() {
                Ok(ev) => ev,
                // stdin closed mid-prompt. Nobody is there to answer.
                Err(_) => break None,
            };
            let Event::Key(key) = ev else { continue };
            // A key held down repeats, and on Windows every press also
            // arrives as a release. Acting on anything but a press would
            // answer the prompt more than once.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                // Ctrl-C and Ctrl-D abort, as they do everywhere else.
                KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break None;
                }
                KeyCode::Char(c) if valid.contains(&c) => break Some(c),
                // Explicit refusals, so the operator is never stuck at a
                // prompt whose options they do not want.
                KeyCode::Esc | KeyCode::Char('q') => break None,
                // Anything else: keep waiting. A stray keystroke must not
                // resolve the prompt in either direction.
                _ => continue,
            }
        };

        let _ = crossterm::terminal::disable_raw_mode();

        // Raw mode echoes nothing, so the answer is printed here. Without
        // this the operator presses a key, sees an unchanged screen, and
        // cannot tell whether it registered — the same "keyboard looks dead"
        // failure that `read_line` restores echo to avoid.
        match chosen {
            Some(c) => println!("{c}"),
            None => println!(),
        }
        Some(chosen)
    }

    fn read_line() -> Option<String> {
        // Echo is suppressed for the whole turn so the operator's typing does
        // not collide with streamed output. At a PROMPT that is exactly wrong:
        // they type `1` and see nothing, the keyboard looks dead, and a blind
        // Enter fails closed into a denial that reads as a bug. Restore echo
        // for the read; the guard puts the suppression back on drop.
        let _echo = crate::input_queue::EchoRestorer::new();
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(buf.trim().to_string()),
        }
    }
}

/// Render a composite's manifest for a human, and nothing else.
///
/// Extracted from `sysaicalls::display_plan_and_confirm`, which rendered the
/// plan, read stdin, decided approval and then executed the steps — four jobs
/// with one entry point, so the only way to show a plan was to also decide it.
/// This is the first of the four, alone, so the sink can show the rich manifest
/// while the authority stays where it belongs.
///
/// Used for every composite that carries children, not only `Plan`. A `Try` is
/// approved allow-once and never for the session, which stops a grant being
/// reused — but "allow once" is only meaningful if the person can see *what*
/// they are allowing once. `action_box` renders the wrapper; a bare "Try" tells
/// a reader nothing about the writes it is about to authorise.
pub fn render_manifest(action: &serde_json::Value) -> String {
    // SECURITY (audit 2026-08-15, finding 4): the manifest is the ONLY thing a
    // person reads before approving a composite, and `kind`, `label` and every
    // child's `type` are model-authored. A `\n` in the label plants a row that
    // reads as a step nobody scheduled; `\r\x1b[2K` repaints rows already
    // drawn. `sanitize` folds both — one row per step, and the step count the
    // operator reads is the step count that runs. See `crate::text_safety`.
    use crate::text_safety::sanitize;
    let kind = sanitize(action["type"].as_str().unwrap_or("composite"));
    let label = sanitize(action["label"].as_str().unwrap_or(match kind.as_str() {
        "Plan" => "proposed plan",
        "Try" => "transaction",
        _ => "steps",
    }));
    // `Plan` and `Pipeline` name them `steps`; `Try` names them `actions`.
    let children = action["steps"]
        .as_array()
        .or_else(|| action["actions"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = format!(
        "\n  {} {} {}\n",
        format!("{}:", kind.to_lowercase()).bright_cyan().bold(),
        label.bold(),
        format!("({} steps)", children.len()).dimmed()
    );
    if action["rollback_on_fail"].as_bool() == Some(true) {
        out.push_str(&format!(
            "    {}\n",
            "rolls back entirely if any step fails".dimmed()
        ));
    }
    for (i, step) in children.iter().enumerate() {
        let child_kind = sanitize(step["type"].as_str().unwrap_or("?"));
        // `to_string()` is JSON, so serde has already escaped any control
        // character into a literal `\u001b`, which is inert. Sanitize anyway:
        // that inertness is a property of a serializer this line does not own,
        // and the row is one line, so the newline `preview_text` appends must
        // go too.
        // The step's JSON as plain text; `preview_text` is already
        // coloured, and `sanitize` turned its escapes into visible `[90m`.
        let detail = sanitize(&step.to_string());
        let detail: String = detail.chars().take(160).collect();
        out.push_str(&format!(
            "    {} {}  {}\n",
            format!("{}.", i + 1).dimmed(),
            child_kind.bold(),
            detail.dimmed()
        ));
    }
    out
}

/// Backwards-compatible name for the Plan case.
pub fn render_plan(action: &serde_json::Value) -> String {
    render_manifest(action)
}

impl DisplaySink for CliDisplaySink {
    fn emit(&mut self, event: &AgentEvent) {
        match event {
            // SECURITY: the model's answer streams straight to stdout, and the
            // confirmation prompt for the action it is about to request goes to
            // the same stream in the same turn. SGR state persists across
            // writes, so a token ending in `\x1b[8m` (conceal) leaves that
            // prompt invisible. Folding per chunk is enough: ESC is one char
            // and cannot be split across two chunks.
            AgentEvent::Token(t) => {
                print!("{}", crate::text_safety::sanitize_block(t));
                let _ = std::io::stdout().flush();
            }
            AgentEvent::AssistantEnd { .. } => println!(),
            AgentEvent::ToolStart { action, .. } => {
                print!("{}", crate::display::action_box(action));
            }
            AgentEvent::ToolEnd {
                action,
                result,
                duration_ms,
                ..
            } => {
                print!(
                    "{}",
                    crate::display::result_line(action, result, *duration_ms)
                );
            }
            AgentEvent::ActionDenied { action, reason, .. } => {
                println!("  {} {}", "✗".red(), reason);
                let _ = action;
            }
            // "turn completed" is the driver narrating its own bookkeeping.
            // The answer arriving IS the completion, and the box below says so
            // more clearly than a line above it. Every other Info still
            // prints — this suppresses one specific piece of noise, not the
            // channel. An abnormal ending is NOT an Info: it comes back as a
            // TurnOutcome the REPL renders as an error, so nothing is hidden.
            AgentEvent::Info(m) if m.starts_with("turn ") => {}
            AgentEvent::Info(m) => println!("  {} {}", "·".dimmed(), m),
            AgentEvent::Error(m) => println!("  {} {}", "✗".red().bold(), m),
            AgentEvent::BudgetUpdated { .. } | AgentEvent::HeaderTick { .. } => {}
            // Variants this frontend has no distinct rendering for are
            // deliberately silent rather than debug-printed: the CLI's own
            // presentation already covers them, and a stray `{:?}` in a
            // terminal reads as a bug to the user.
            _ => {}
        }
    }

    /// Put the question to the person, offering exactly what the authority
    /// says may be offered.
    ///
    /// `persistent_grant` is the frontend capability contract: `None` means the
    /// standing option must not appear at all, and `Some(text)` means it may,
    /// labelled with `text`. So the session-wide choice is only ever shown for
    /// an action that has a stable identity to scope it to, and the user reads
    /// what they are granting before they grant it.
    fn request_confirmation(
        &mut self,
        request: &ConfirmationRequest<'_>,
    ) -> Option<ConfirmationResponse> {
        if !self.interactive {
            return None;
        }

        // A Plan is shown in full, because approving a plan you cannot read is
        // not approval. Presentation is the frontend's business; that this
        // renders differently changes nothing about who decides.
        // Any composite carrying children shows them. Approving what you
        // cannot read is not approval, and that is as true of a Try — whose
        // whole point is a set of writes it can undo — as of a Plan.
        // ONLY composites re-render here. A simple action has already been
        // shown by the emit path immediately above the prompt (`▸ exec ·
        // RunCommand / $ …`), so printing its box again stuttered the same
        // action twice on screen (field report, 2026-08-12). A composite is
        // different: the emit path shows the wrapper, and "allow once" is
        // meaningless unless the operator can read the children it covers.
        let carries_children =
            request.action.get("steps").is_some() || request.action.get("actions").is_some();
        if carries_children {
            print!("{}", render_manifest(request.action));
        }

        // SECURITY: the reason is worded by the authority, but it interpolates
        // the action's own fields (a path, a command name) to say WHY. Those
        // are the model's. Sanitized here so no line of this prompt is exempt.
        println!(
            "  {} {}",
            "⚠".yellow().bold(),
            crate::text_safety::sanitize(request.reason).yellow()
        );
        // A policy question, not an approval question. The three choices are
        // different things — widen this process, widen the file too, refuse —
        // and each names exactly what it writes, because "add to config" over
        // a file the operator keeps commented for himself is not something to
        // agree to blind. `lift` is set only when the RUNTIME's exhaustive
        // match said this refusal is his to lift; the frontend never decides
        // that on its own.
        if let Some(lift) = request.lift {
            println!(
                "    {}  {}",
                "1".bold(),
                crate::text_safety::sanitize(&lift.session_choice())
            );
            println!(
                "       {}",
                "(this runtime process only, gone at exit)".bright_black()
            );
            println!(
                "    {}  {}",
                "2".bold(),
                crate::text_safety::sanitize(&lift.persist_choice())
            );
            println!("       {}", "(comments kept)".bright_black());
            println!("    {}  deny", "3".bold());
            print!("  {} ", "press 1/2/3 ❯".bright_black().bold());
            let _ = std::io::stdout().flush();

            let answer: Option<char> = match Self::read_choice(&['1', '2', '3']) {
                Some(key) => key,
                None => Self::read_line()?.chars().next(),
            };
            let outcome = match answer {
                Some('1') => ConfirmOutcome::Yes,
                Some('2') => ConfirmOutcome::Always,
                // SECURITY: same rule as the approval prompt below. An abort,
                // a closed stdin, an empty line or an unknown key refuses —
                // and here a refusal means the policy is not widened, which is
                // exactly the state the operator was already in.
                _ => ConfirmOutcome::No,
            };
            return Some(ConfirmationResponse {
                id: request.id,
                outcome,
            });
        }

        let (rows, valid) = approval_menu(request.persistent_grant);
        for (k, label) in &rows {
            println!("    {}  {}", k.to_string().bold(), label);
        }
        let press: Vec<String> = valid.iter().map(|c| c.to_string()).collect();
        print!(
            "  {} ",
            format!("press {} ❯", press.join("/")).bright_black().bold()
        );
        let valid = valid.as_slice();
        let _ = std::io::stdout().flush();

        // One keypress when the terminal allows it, a typed line when it does
        // not. Both produce the same character, so the decision below is
        // written once and cannot drift between the two input paths.
        let answer: Option<char> = match Self::read_choice(valid) {
            Some(key) => key,
            None => Self::read_line()?.chars().next(),
        };

        let outcome = approval_outcome(answer, request.persistent_grant.is_some());

        Some(ConfirmationResponse {
            id: request.id,
            outcome,
        })
    }
}

/// The approval choices, numbered without a gap: `1 allow once,
/// 2 deny` when no session grant is offered.
pub(crate) fn approval_menu(persistent: Option<&str>) -> (Vec<(char, String)>, Vec<char>) {
    let rows = match persistent {
        Some(scope) => vec![
            ('1', "allow once".to_string()),
            ('2', crate::tui::app::lower_first(scope)),
            ('3', "deny".to_string()),
        ],
        None => vec![('1', "allow once".to_string()), ('2', "deny".to_string())],
    };
    let keys = rows.iter().map(|(k, _)| *k).collect();
    (rows, keys)
}

/// SECURITY: only `1`, and `2` where a session grant was offered, approve.
/// Everything else denies: an abort, a closed stdin, an unknown key.
pub(crate) fn approval_outcome(answer: Option<char>, persistent: bool) -> ConfirmOutcome {
    match answer {
        Some('1') => ConfirmOutcome::Yes,
        Some('2') if persistent => ConfirmOutcome::Always,
        _ => ConfirmOutcome::No,
    }
}

/// Questions the *model* asked, as opposed to questions the *security
/// boundary* asked.
///
/// `AskHuman`, `Confirm` and `AskMultipleChoice` are interaction primitives: a
/// model wanting to know something. They are not approvals, and answering one
/// grants nothing — a model that asks "shall I continue? yes/no" and receives
/// "yes" has learned a preference, not acquired authority. Keeping them on a
/// separate trait from [`DisplaySink::request_confirmation`] means the two can
/// never be confused at a call site, because they are not the same method.
pub trait HumanChannel: Send {
    /// Free-form question. `None` when nobody could be asked.
    fn ask(&mut self, question: &str) -> Option<String>;

    /// Yes/no question. `None` when nobody could be asked — which the caller
    /// must not read as "no" or as "yes", only as "unanswered".
    fn confirm(&mut self, question: &str) -> Option<bool>;

    /// One of `choices`, by index. `None` when nobody could be asked.
    fn choose(&mut self, question: &str, choices: &[String]) -> Option<usize>;
}

/// SECURITY (audit 2026-08-15, finding 4): the model authors every one of
/// these questions, and each is followed by a prompt whose answer the model
/// then reads back — `confirm` in particular returns a boolean the agent
/// treats as a human decision. A `\r\x1b[2K` in the question repaints the
/// line the person is answering. Same sanitizer as the approval prompt above;
/// there is no reason for one of the two to be safe and the other not.
impl HumanChannel for CliDisplaySink {
    fn ask(&mut self, question: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        println!(
            "\n  {} {}",
            "?".bright_cyan().bold(),
            crate::text_safety::sanitize(question).bold()
        );
        print!("  {} ", "❯".bright_black().bold());
        let _ = std::io::stdout().flush();
        Self::read_line()
    }

    fn confirm(&mut self, question: &str) -> Option<bool> {
        if !self.interactive {
            return None;
        }
        println!(
            "\n  {} {}",
            "?".bright_cyan().bold(),
            crate::text_safety::sanitize(question).bold()
        );
        print!("  {} ", "[y/N] ❯".bright_black().bold());
        let _ = std::io::stdout().flush();
        let answer = Self::read_line()?;
        Some(matches!(
            answer.to_lowercase().as_str(),
            "y" | "yes" | "o" | "oui" | "true" | "1"
        ))
    }

    fn choose(&mut self, question: &str, choices: &[String]) -> Option<usize> {
        if !self.interactive {
            return None;
        }
        println!(
            "\n  {} {}",
            "?".bright_cyan().bold(),
            crate::text_safety::sanitize(question).bold()
        );
        for (i, c) in choices.iter().enumerate() {
            println!(
                "    {}  {}",
                (i + 1).to_string().bold(),
                crate::text_safety::sanitize(c)
            );
        }
        print!("  {} ", "choose ❯".bright_black().bold());
        let _ = std::io::stdout().flush();
        let answer = Self::read_line()?;
        answer
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1 && *n <= choices.len())
            .map(|n| n - 1)
    }
}

#[cfg(test)]
mod tests {

    // ── ─────────────────────────────────────────────────────────────

    #[test]
    fn the_pipeline_manifest_carries_no_raw_escape_codes() {
        colored::control::set_override(true);
        let pipeline = serde_json::json!({"type": "Pipeline", "steps": [
            {"type": "RunCommand", "command": "ls", "args": ["-la"]},
            {"type": "ReadFile", "path": "Cargo.toml"}]});
        let out = super::render_manifest(&pipeline);
        colored::control::unset_override();
        // Real escapes are fine; a `[90m` left without its ESC is the defect.
        let plain = out.replace("\x1b[", "");
        assert!(!plain.contains("[90m") && !plain.contains("[0m"), "{out:?}");
        assert!(out.contains("RunCommand"));
    }

    #[test]
    fn a_two_choice_menu_is_numbered_1_2() {
        let (rows, keys) = super::approval_menu(None);
        assert_eq!(
            rows.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec!['1', '2']
        );
        assert_eq!(keys, vec!['1', '2']);
        assert_eq!(
            super::approval_outcome(Some('2'), false),
            ConfirmOutcome::No
        );
        assert_eq!(
            super::approval_outcome(Some('1'), false),
            ConfirmOutcome::Yes
        );
        assert_eq!(
            super::approval_outcome(Some('3'), false),
            ConfirmOutcome::No
        );
        let (rows, _) = super::approval_menu(Some("allow it for the session"));
        assert_eq!(
            rows.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec!['1', '2', '3']
        );
        assert_eq!(
            super::approval_outcome(Some('2'), true),
            ConfirmOutcome::Always
        );
        assert_eq!(super::approval_outcome(Some('3'), true), ConfirmOutcome::No);
    }
    use super::*;
    use serde_json::json;

    fn production_code() -> String {
        let src = include_str!("cli_sink.rs");
        src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rich plan rendering survives the split. Steps, labels and count are
    /// still shown — a person approving a plan must be able to read it.
    #[test]
    fn a_plan_is_still_rendered_in_full() {
        let plan = json!({
            "type": "Plan",
            "label": "tidy the tree",
            "steps": [
                {"type": "WriteFile", "path": "/tmp/a"},
                {"type": "RunCommand", "command": "git", "args": ["status"]},
            ]
        });
        let rendered = render_plan(&plan);
        assert!(rendered.contains("tidy the tree"), "{rendered}");
        assert!(rendered.contains("2 steps"), "{rendered}");
        assert!(rendered.contains("WriteFile"), "{rendered}");
        assert!(rendered.contains("RunCommand"), "{rendered}");
    }

    /// A directly emitted `Try` must show the concrete writes it is about to
    /// authorise. Allow-once stops the grant being *reused*; it says nothing
    /// about whether the person understood what they allowed once. A bare
    /// "Try" box named the wrapper and hid every child.
    #[test]
    fn a_direct_try_exposes_its_transaction_manifest() {
        let try_action = json!({
            "type": "Try",
            "label": "swap the config",
            "rollback_on_fail": true,
            "actions": [
                {"type": "WriteFile", "path": "/etc/app.toml", "content": "new"},
                {"type": "RunCommand", "command": "systemctl", "args": ["restart", "app"]},
            ]
        });
        let rendered = render_manifest(&try_action);
        assert!(rendered.contains("swap the config"), "{rendered}");
        assert!(rendered.contains("2 steps"), "{rendered}");
        assert!(rendered.contains("WriteFile"), "{rendered}");
        assert!(rendered.contains("RunCommand"), "{rendered}");
        assert!(
            rendered.contains("/etc/app.toml"),
            "the target is not shown, so the reader cannot tell which file: {rendered}"
        );
        assert!(
            rendered.contains("rolls back entirely"),
            "the transactional promise is part of what is being approved: {rendered}"
        );
    }

    /// And the prompt reaches for the manifest, not the wrapper box, whenever
    /// an action carries children — `Plan` and `Pipeline` name them `steps`,
    /// `Try` names them `actions`.
    #[test]
    fn the_prompt_renders_a_manifest_for_any_composite() {
        let code = production_code();
        let at = code.find("fn request_confirmation").expect("the gate");
        let body = &code[at..];
        assert!(
            body.contains(r#"get("steps")"#) && body.contains(r#"get("actions")"#),
            "the prompt only recognises one composite shape; a Try would be \
             confirmed as an opaque wrapper"
        );
        assert!(body.contains("render_manifest("));
    }

    /// A SIMPLE action is not re-rendered at the prompt. The emit path already
    /// printed it immediately above; printing the box again stuttered the same
    /// action twice on screen, which is what the operator saw on 2026-08-12.
    #[test]
    fn the_prompt_does_not_reprint_a_simple_action() {
        let code = production_code();
        let at = code.find("fn request_confirmation").expect("the gate");
        let end = code[at..]
            .find("\n    fn ")
            .map(|i| at + i)
            .unwrap_or(code.len());
        let body = &code[at..end];
        assert!(
            !body.contains("action_box("),
            "the prompt re-renders a simple action the emit path already showed"
        );
    }

    /// The confirmation read restores terminal echo. Echo is suppressed for the
    /// whole turn so streamed output stays clean; at a prompt that is exactly
    /// wrong — the operator types `1`, sees nothing, and concludes the keyboard
    /// is dead (field report, 2026-08-12).
    #[test]
    fn the_confirmation_read_shows_what_the_operator_types() {
        let code = production_code();
        let at = code.find("fn read_line").expect("the reader");
        let end = code[at..]
            .find("\n    }")
            .map(|i| at + i)
            .unwrap_or(code.len());
        let body = &code[at..end];
        assert!(
            body.contains("EchoRestorer"),
            "the confirmation prompt reads with echo off — keystrokes are invisible"
        );
    }

    /// The renderer renders. It does not read, decide or execute — the four
    /// jobs `display_plan_and_confirm` had are now one.
    #[test]
    fn the_plan_renderer_neither_reads_nor_decides() {
        let code = production_code();
        let at = code.find("pub fn render_manifest").expect("the renderer");
        let end = code[at..]
            .find("\n}\n")
            .map(|i| at + i)
            .unwrap_or(code.len());
        let body = &code[at..end];
        for banned in ["stdin", "read_line", "approve", "decide", "send_action"] {
            assert!(
                !body.contains(banned),
                "the plan renderer touches `{banned}` — presentation and \
                 authority are joined again"
            );
        }
    }

    /// No naked global. The session-wide option appears only when the
    /// authority said it may, and it is labelled with the authority's own
    /// description of the scope.
    #[test]
    fn the_prompt_offers_no_unscoped_standing_approval() {
        let code = production_code();
        assert!(
            !code.contains("Always for this session") && !code.contains("[y/n/a]"),
            "the y/n/a prompt survived into the migrated sink"
        );
        assert!(
            code.contains("lower_first(scope)"),
            "the standing option is not labelled with the authority's scope"
        );
        assert!(
            code.contains("request.persistent_grant.is_some()"),
            "`2` is accepted without checking the authority offered it"
        );
    }

    /// A UI preference is not an authority. The sink must not consult
    /// `auto_confirm`: that field is how the old prompt disabled the gate for
    /// the whole session and every sub-agent.
    #[test]
    fn the_sink_never_consults_a_ui_authority_flag() {
        let code = production_code();
        for banned in [
            "auto_confirm",
            "confirm_writes",
            "ApprovalAuthority",
            "ApprovalScope",
        ] {
            assert!(
                !code.contains(banned),
                "the sink reads `{banned}` — a renderer with an opinion about \
                 authority is the defect this file exists to remove"
            );
        }
    }

    /// Silence is not consent. With no human reachable the sink returns
    /// `None`, which `confirm::decide` turns into a denial.
    #[test]
    fn a_headless_sink_answers_nothing() {
        let mut sink = CliDisplaySink::with_interactivity(false);
        let action = json!({"type": "MoveToTrash", "path": "/tmp/x"});
        // The id must come from an authority: a request nobody opened is not
        // a question, and that is the correlation this type exists to enforce.
        let mut authority = crate::agent::approval::ApprovalAuthority::default();
        let request = ConfirmationRequest {
            id: authority.open_request(),
            action: &action,
            action_type: "MoveToTrash",
            reason: "needs approval",
            persistent_grant: None,
            lift: None,
        };
        assert!(sink.request_confirmation(&request).is_none());
    }

    /// Headless human interaction fails explicitly rather than defaulting.
    /// `confirm` returning `None` must not be readable as `false` by accident,
    /// which is why it is an `Option<bool>` and not a `bool`.
    #[test]
    fn headless_human_questions_are_unanswered_not_answered_no() {
        let mut sink = CliDisplaySink::with_interactivity(false);
        assert_eq!(sink.ask("what is your name?"), None);
        assert_eq!(sink.confirm("proceed?"), None);
        assert_eq!(sink.choose("pick", &["a".into(), "b".into()]), None);
    }

    /// The adversarial one. A model asking `Confirm` and being told "yes" has
    /// learned a preference; it has not been granted anything. The two live on
    /// different traits with different return types — `HumanChannel::confirm`
    /// yields a bare `bool` with no request id, so there is nothing an
    /// authority could even record.
    #[test]
    fn answering_a_model_question_cannot_become_security_authority() {
        let code = production_code();
        let at = code
            .find("impl HumanChannel for CliDisplaySink")
            .expect("impl");
        let body = &code[at..];
        for banned in [
            "ConfirmationResponse",
            "ConfirmOutcome",
            "authority",
            "grant",
        ] {
            assert!(
                !body.contains(banned),
                "the human-question channel produces `{banned}` — answering a \
                 model's question would then confer authority"
            );
        }
        // And the converse: the security prompt does not answer model
        // questions, so a `Confirm` cannot be routed into it.
        let sec = code.find("fn request_confirmation").expect("gate");
        let sec_end = code[sec..].find("\n    }\n").unwrap() + sec;
        assert!(
            !code[sec..sec_end].contains("HumanChannel"),
            "the security prompt and the model's question channel are joined"
        );
    }
}
