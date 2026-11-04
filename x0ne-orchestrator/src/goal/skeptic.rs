//! The skeptic pass: ask the model to name what its own report did not
//! measure, and stop when it names the same thing twice.
//!
//! SECURITY: an unreadable verdict — transport down, cancellation, malformed
//! JSON, missing array — counts as REFUTED. This is invariant 1: the absence
//! of an answer is never an approval.
//!
//! The skeptic is **a model call without the full context**: it receives
//! the goal and the report, nothing else, and returns gaps. The model that
//! answers is not fixed here — [`SkepticOracle`] is the interface, and whoever
//! builds it chooses the model. Grok Build's panel runs three of them with a
//! quorum; we take one (constraint "no cloud") while keeping their bias: a
//! skeptic that fails never counts as "validated".
//! "Skeptic = same model?" remains an owner decision (O-2): this module
//! does not settle it.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::api::LlmBackend;
use crate::conversation::{Message, Role};

/// Two identical fingerprints in a row are enough: the model has stopped
/// progressing, and a third turn would burn budget for the same report.
pub const GOAL_STALL_THRESHOLD: usize = 2;

/// Skeptic instruction. English, descriptive, and tells the model nothing
/// about its state or about any confinement (repository rule).
pub const SKEPTIC_PROMPT: &str = "\
Review the report above. List every claim that is not supported by a measurement \
you can point to, citing `file:line` or the command whose output would settle it. \
Answer as JSON: {\"gaps\": [\"...\"]}. An empty list means every claim is supported.";

/// Frame of the second pass. States what the skeptic sees — the goal and the
/// report, not the session — because it is true and it changes how it
/// answers. English, descriptive, without hostility.
pub const SKEPTIC_SYSTEM: &str = "\
You are reviewing a progress report written by an agent working towards a goal. \
You see the goal and the report, not the session that produced them, so judge \
only what the report itself states and cites.";

/// The reviewed report is bounded before it enters the prompt: a second model
/// call per turn must have a predictable cost. The cut is announced in the
/// text, never silent.
pub const MAX_REPORT_CHARS: usize = 8_000;

/// Number of gaps kept from a verdict. The rest is dropped: the list goes into
/// the goal's persisted history, and a model that emits ten thousand must not
/// grow a file in `~/.x0ne/goals/` without bound.
pub const MAX_GAPS: usize = 32;

/// Length of a kept gap. Same reason: the history is text for a human, not a
/// dumping ground.
pub const MAX_GAP_CHARS: usize = 400;

/// Length of the goal text copied into the skeptic prompt. The text comes from
/// a human, but nothing bounds what they can paste.
pub const MAX_GOAL_CHARS: usize = 400;

/// Number of citations that make up a fingerprint. Sorting then cutting keeps
/// the fingerprint deterministic; not cutting would let it grow with the
/// model's verbosity.
pub const MAX_FINGERPRINT_CITATIONS: usize = 16;

/// Length of a citation in the fingerprint.
pub const MAX_CITATION_CHARS: usize = 200;

/// What replaces a scratch root and its first segment.
const SCRATCH_PLACEHOLDER: &str = "<scratch>";

/// Temporary directory roots. Order does not matter: the cut is made on the
/// leftmost occurrence, so `/private/tmp/x` is seen as `/private/tmp/` and not
/// as the `/tmp/` it contains.
const SCRATCH_ROOTS: [&str; 3] = ["/tmp/", "/var/folders/", "/private/tmp/"];

/// What the skeptic answered.
///
/// `refuted == false` is the ONLY "report validated" state, and it requires a
/// readable answer whose gap list is empty. Everything else — failure,
/// malformed JSON, non-text entry, at least one gap — is refuted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkepticVerdict {
    pub gaps: Vec<String>,
    pub refuted: bool,
}

impl SkepticVerdict {
    /// The verdict of a skeptic that could not answer.
    ///
    /// SECURITY: this is the fallback value of every failure path —
    /// transport, cancellation, unreadable JSON. It is refuted, with no gap:
    /// we do not know what is missing, so we do not validate.
    pub fn failed() -> Self {
        Self {
            gaps: Vec::new(),
            refuted: true,
        }
    }

    /// Reads what the model wrote. Never returns an error: unreadable text
    /// is a refuted verdict, not an exception the caller could swallow with
    /// `unwrap_or_default()` — which would mean "validated".
    ///
    /// Lenient on the envelope (```json fences, prose around the object),
    /// strict on the content: `gaps` must be an array of strings.
    pub fn parse(raw: &str) -> Self {
        let Some(value) = extract_json_object(raw) else {
            return Self::failed();
        };
        let Some(items) = value.get("gaps").and_then(|g| g.as_array()) else {
            return Self::failed();
        };

        let mut gaps = Vec::new();
        // A non-text entry cannot be ignored: the array is not the one that
        // was asked for, so the answer is not readable.
        let mut malformed = false;
        for item in items {
            match item.as_str() {
                Some(s) if !s.trim().is_empty() => {
                    if gaps.len() < MAX_GAPS {
                        gaps.push(truncate_chars(s.trim(), MAX_GAP_CHARS));
                    }
                }
                Some(_) => {}
                None => malformed = true,
            }
        }

        Self {
            refuted: malformed || !gaps.is_empty(),
            gaps,
        }
    }

    /// The fingerprint of this verdict's gaps.
    pub fn fingerprint(&self) -> String {
        gap_fingerprint(&self.gaps)
    }

    /// One line for the goal history, bounded by construction.
    pub fn summary(&self) -> String {
        if self.gaps.is_empty() {
            return if self.refuted {
                "skeptic unavailable — counted as refuted".into()
            } else {
                "skeptic found no unsupported claim".into()
            };
        }
        format!("{} gap(s): {}", self.gaps.len(), self.gaps.join(" | "))
    }
}

/// The stable fingerprint of the gaps: what stays identical when the model
/// rephrases the same objection.
///
/// Order-insensitive (sorted), deduplicated, scratch paths normalised, and
/// reduced to **citations** `path:line`: that is the verifiable part of a
/// gap. Two reports that point at the same lines yield the same fingerprint
/// whatever the surrounding prose.
///
/// SECURITY: without a citation, the fingerprint is empty, and an empty
/// fingerprint does not trigger the stagnation guard (see
/// `GoalState::record_gap_fingerprint`). Otherwise any two reports without
/// citations would always look alike, and any goal would block itself on
/// the second turn.
pub fn gap_fingerprint(gaps: &[String]) -> String {
    let mut citations: Vec<String> = gaps.iter().flat_map(|g| citations_of(g)).collect();
    citations.sort();
    citations.dedup();
    citations.truncate(MAX_FINGERPRINT_CITATIONS);
    citations.join("\n")
}

/// The citations of a gap, scratch paths already normalised.
fn citations_of(gap: &str) -> Vec<String> {
    normalize_scratch(gap)
        .split_whitespace()
        .filter_map(citation_of_token)
        .collect()
}

/// `src/policy.rs:458` → `src/policy.rs:458`. `the:12` → nothing (the left
/// segment does not look like a path), `12:34` → nothing either.
fn citation_of_token(token: &str) -> Option<String> {
    // Openers on the left, closers and trailing punctuation on the right — in
    // that order: `(src/policy.rs:458).` must yield `src/policy.rs:458`, which a
    // single `trim_matches` does not do (the final `.` stops the `)`).
    let trimmed = token
        .trim_start_matches(['(', '[', '{', '"', '\'', '`'])
        .trim_end_matches([')', ']', '}', '"', '\'', '`', ',', ';', '.', ':']);
    let (path, line) = trimmed.rsplit_once(':')?;
    if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !(path.contains('/') || path.contains('.')) {
        return None;
    }
    // Lowercase: the same file cited as `Src/Policy.rs` and `src/policy.rs` is
    // the same file, and the fingerprint must see it that way.
    Some(truncate_chars(
        &format!("{}:{}", path.to_lowercase(), line),
        MAX_CITATION_CHARS,
    ))
}

/// Replaces `/tmp/<seg>`, `/var/folders/<seg>`, `/private/tmp/<seg>` with
/// `<scratch>`. The first segment goes with the root: it is the one drawn at
/// random on each run, and it is the one that would make two identical passes
/// unrecognisable from each other.
fn normalize_scratch(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let hit = SCRATCH_ROOTS
            .iter()
            .filter_map(|root| rest.find(root).map(|i| (i, *root)))
            .min_by_key(|(i, root)| (*i, std::cmp::Reverse(root.len())));
        let Some((at, root)) = hit else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push_str(SCRATCH_PLACEHOLDER);
        let after = &rest[at + root.len()..];
        // The segment runs up to the next `/` or to the end of the word.
        let cut = after
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(after.len());
        rest = &after[cut..];
    }
}

/// The first JSON object of an answer, Markdown fences and prose included.
/// `None` as soon as there is none — the caller turns it into a refuted verdict.
fn extract_json_object(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&trimmed[start..=end])
        .ok()
        .filter(serde_json::Value::is_object)
}

/// Cuts on a character boundary, never a byte one — the text comes from a
/// model and contains arbitrary UTF-8.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((idx, _)) => format!("{}…", &s[..idx]),
    }
}

/// The second-pass prompt: the goal, the report (bounded), the instruction.
///
/// No session context goes in — that is what makes the pass "skeptical"
/// rather than a second opinion from the same thread.
pub fn skeptic_prompt(goal_text: &str, report: &str) -> String {
    format!(
        "Goal: {}\n\nReport:\n{}\n\n{}",
        truncate_chars(goal_text.trim(), MAX_GOAL_CHARS),
        truncate_chars(report.trim(), MAX_REPORT_CHARS),
        SKEPTIC_PROMPT,
    )
}

/// The skeptic's model call, behind an interface.
///
/// Two reasons not to call a backend directly from the manager: the test
/// doubles the answer without network, and the **choice of model stays with
/// whoever builds the oracle** — the question "skeptic = same model?" is
/// open (owner decision, O-2) and this code must not freeze it.
#[async_trait]
pub trait SkepticOracle: Send + Sync {
    /// Returns the raw text of the answer. An `Err` is a failed skeptic,
    /// hence a refuted verdict — never a validated report.
    async fn review(&self, prompt: &str) -> Result<String>;

    /// Who answered, for the history. Used only for human audit.
    fn model_label(&self) -> String;
}

/// The oracle of the **current turn**, mounted on the backend the driver holds.
///
/// Why a borrowed backend rather than an owned one:
///
/// 1. `TurnContext` carries a `&dyn LlmBackend`, not an `Arc` — the turn does
///    not outlive the borrow, and nothing justifies reference counting for a
///    call that finishes before the turn.
/// 2. **It records what the call cost.** `SkepticOracle::review` returns only
///    text; the tokens would be lost there. A model call that enters no
///    counter is an off-budget expense, and the budget belongs to the
///    operator.
pub struct BorrowedSkeptic<'a> {
    backend: &'a dyn LlmBackend,
    /// What the last call consumed. `None` as long as no call has succeeded
    /// — a transport failure does not produce tokens.
    usage: std::sync::Mutex<Option<crate::agent::model::Usage>>,
}

impl<'a> BorrowedSkeptic<'a> {
    pub fn new(backend: &'a dyn LlmBackend) -> Self {
        Self {
            backend,
            usage: std::sync::Mutex::new(None),
        }
    }

    /// The pass's tokens, for the caller to add to the turn counter. `None`
    /// if the model did not answer.
    pub fn usage(&self) -> Option<crate::agent::model::Usage> {
        self.usage.lock().ok().and_then(|u| u.clone())
    }
}

#[async_trait]
impl SkepticOracle for BorrowedSkeptic<'_> {
    async fn review(&self, prompt: &str) -> Result<String> {
        let messages = vec![Message {
            role: Role::User,
            content: prompt.to_string(),
            images: Vec::new(),
        }];
        let resp = self.backend.send(SKEPTIC_SYSTEM, &messages).await?;
        if let Ok(mut slot) = self.usage.lock() {
            *slot = Some(crate::agent::model::Usage {
                input_tokens: Some(resp.input_tokens),
                output_tokens: Some(resp.output_tokens),
                cache_write_tokens: Some(resp.cache_creation_input_tokens),
                cache_read_tokens: Some(resp.cache_read_input_tokens),
                // The price is computed by the caller, which has the price table.
                cost_usd: None,
            });
        }
        Ok(resp.content)
    }

    fn model_label(&self) -> String {
        self.backend.display_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::manager::tests::make_manager;
    use crate::goal::state::{Budget, GoalState, GoalStatus, HistoryKind, SkepticCadence};
    use std::sync::Mutex;

    /// The fingerprint is insensitive to order and to the scratch directory.
    ///
    /// Without this normalisation, two identical passes would produce two
    /// different fingerprints as soon as a temporary path changes, and the
    /// stop on stagnation would never trigger.
    #[test]
    fn the_gap_fingerprint_ignores_order_and_scratch_paths() {
        let a = vec![
            "src/policy.rs:458 is never called".to_string(),
            "/tmp/x0ne-xyz/report.md:12 lacks a measurement".to_string(),
        ];
        let b = vec![
            "/tmp/x0ne-abc/report.md:12 lacks a measurement".to_string(),
            "src/policy.rs:458 is never called".to_string(),
        ];
        assert_eq!(gap_fingerprint(&a), gap_fingerprint(&b));
        assert!(
            !gap_fingerprint(&a).is_empty(),
            "two well-formed citations must produce a fingerprint"
        );
    }

    /// The other scratch roots normalize like `/tmp/`.
    #[test]
    fn every_scratch_root_normalizes_to_the_same_fingerprint() {
        let reference = vec!["/tmp/run-1/notes.md:7 has no measurement".to_string()];
        for other in [
            "/tmp/run-2/notes.md:7 has no measurement",
            "/var/folders/zz/notes.md:7 has no measurement",
            "/private/tmp/run-3/notes.md:7 has no measurement",
        ] {
            assert_eq!(
                gap_fingerprint(&reference),
                gap_fingerprint(&[other.to_string()]),
                "{other} must yield the same fingerprint as the reference"
            );
        }
    }

    /// The prose around a citation does not count: rephrasing the same
    /// objection is not progress, and the fingerprint must see that.
    #[test]
    fn rewording_the_same_citations_keeps_the_same_fingerprint() {
        let first = vec!["nothing measures src/policy.rs:458 today".to_string()];
        let second = vec!["I still have no measurement for (src/policy.rs:458).".to_string()];
        assert_eq!(gap_fingerprint(&first), gap_fingerprint(&second));
    }

    /// Two reports WITHOUT citations do not look alike by default.
    ///
    /// SECURITY: an empty fingerprint means "no stable fingerprint", and the
    /// guard does not apply. A failure would mean a goal blocks on the second
    /// turn solely because no report cites a file.
    #[test]
    fn an_empty_fingerprint_never_triggers_the_stall_guard() {
        assert_eq!(gap_fingerprint(&[]), "");
        let mut g = GoalState::new("g".into(), "t".into(), Budget::new(Some(9), None), None);
        assert!(!g.record_gap_fingerprint(""));
        assert!(!g.record_gap_fingerprint(""));
        assert!(
            !g.record_gap_fingerprint(""),
            "an empty fingerprint must never block"
        );
    }

    /// A pure-prose gap — no citation — has no fingerprint.
    #[test]
    fn a_gap_without_a_citation_has_no_fingerprint() {
        let gaps = vec![
            "the report says the service is slow but shows no measurement".to_string(),
            "no timing anywhere: 12:34 is a clock, not a citation".to_string(),
        ];
        assert_eq!(gap_fingerprint(&gaps), "");
    }

    /// Two consecutive identical fingerprints block the goal.
    #[test]
    fn two_identical_fingerprints_in_a_row_block_the_goal() {
        assert_eq!(GOAL_STALL_THRESHOLD, 2);
        let mut g = GoalState::new("g".into(), "t".into(), Budget::new(Some(9), None), None);
        assert!(!g.record_gap_fingerprint("a"));
        assert!(
            g.record_gap_fingerprint("a"),
            "the second identical one must block"
        );
    }

    /// A different fingerprint resets the streak.
    #[test]
    fn a_different_fingerprint_resets_the_stall_streak() {
        let mut g = GoalState::new("g".into(), "t".into(), Budget::new(Some(9), None), None);
        assert!(!g.record_gap_fingerprint("a"));
        assert!(!g.record_gap_fingerprint("b"));
        assert!(!g.record_gap_fingerprint("a"));
    }

    /// A pass without a fingerprint does not break a running streak: it says
    /// neither "the same thing" nor "something else". The counter stays put.
    #[test]
    fn an_unfingerprintable_pass_leaves_the_streak_untouched() {
        let mut g = GoalState::new("g".into(), "t".into(), Budget::new(Some(9), None), None);
        assert!(!g.record_gap_fingerprint("a"));
        assert!(!g.record_gap_fingerprint(""));
        assert!(
            g.record_gap_fingerprint("a"),
            "the streak resumes where it stopped"
        );
    }

    /// The stagnation counter is persisted: the guard survives re-reading the
    /// goal, otherwise a restart between two turns would cancel it.
    #[test]
    fn the_stall_counter_survives_a_round_trip_through_json() {
        let mut g = GoalState::new("g".into(), "t".into(), Budget::new(Some(9), None), None);
        assert!(!g.record_gap_fingerprint("src/policy.rs:458"));
        let wire = serde_json::to_string(&g).unwrap();
        let mut back: GoalState = serde_json::from_str(&wire).unwrap();
        assert!(
            back.record_gap_fingerprint("src/policy.rs:458"),
            "the streak must be the same after reload"
        );
    }

    /// A goal written before this task reads back without a counter, and without error.
    #[test]
    fn a_goal_written_before_the_skeptic_still_reads_back() {
        let legacy = r#"{"id":"g1","text":"t","status":{"kind":"active"},
            "created_at":"2026-08-16T00:00:00Z","updated_at":"2026-08-16T00:00:00Z",
            "budget":{"started_at":"2026-08-16T00:00:00Z","steps_used":0},
            "history":[]}"#;
        let mut g: GoalState = serde_json::from_str(legacy).unwrap();
        assert_eq!(g.gap_repeat_count, 0);
        assert_eq!(g.last_gap_fingerprint, None);
        assert!(!g.record_gap_fingerprint("a"));
    }

    /// A skeptic that fails counts as REFUTED, never validated.
    ///
    /// SECURITY: invariant 1. A failure would mean that a transport failure,
    /// a cancellation or malformed JSON counts as approval — the absence of an
    /// answer would become an approval, through the quietest possible path.
    #[test]
    fn a_skeptic_that_fails_counts_as_refuted() {
        for broken in ["", "not json", "{}", "{\"gaps\": null}"] {
            let v = SkepticVerdict::parse(broken);
            assert!(v.refuted, "{broken:?} must count as refuted");
        }
    }

    /// The extended corpus of unreadable answers. Same invariant, more shapes.
    ///
    /// SECURITY: each of these answers must stay refuted. A failure would
    /// mean a stammering model validates a report.
    #[test]
    fn every_unreadable_answer_counts_as_refuted() {
        for broken in [
            "   ",
            "[]",
            "[\"gaps\"]",
            "null",
            "42",
            "\"gaps\"",
            "{\"gaps\": {}}",
            "{\"gaps\": \"none\"}",
            "{\"gap\": []}",
            "{\"gaps\": [1, 2]}",
            "{\"gaps\": [null]}",
            "{\"gaps\": [[\"src/x.rs:1\"]]}",
            "{\"gaps\":",
        ] {
            let v = SkepticVerdict::parse(broken);
            assert!(v.refuted, "{broken:?} must count as refuted");
        }
    }

    /// An empty array, and only that, counts as "report supported".
    #[test]
    fn only_an_explicitly_empty_gap_list_validates_a_report() {
        let ok = SkepticVerdict::parse("{\"gaps\": []}");
        assert!(!ok.refuted);
        assert!(ok.gaps.is_empty());

        let with_gap = SkepticVerdict::parse("{\"gaps\": [\"src/policy.rs:458 is never called\"]}");
        assert!(with_gap.refuted, "a named gap refutes the report");
        assert_eq!(with_gap.gaps.len(), 1);
    }

    /// The usual model envelope — Markdown fence, surrounding prose — is
    /// read; what is strict is the content of `gaps`.
    #[test]
    fn a_json_object_wrapped_in_prose_or_fences_still_reads() {
        let fenced = "Here is my review:\n```json\n{\"gaps\": [\"src/a.rs:1 unproven\"]}\n```\n";
        let v = SkepticVerdict::parse(fenced);
        assert!(v.refuted);
        assert_eq!(v.gaps, vec!["src/a.rs:1 unproven".to_string()]);
    }

    /// A verbose verdict does not grow the goal file without bound.
    ///
    /// FIXED-size fixtures, and bounds are checked by comparison: a mutated
    /// constant shows up in an `assert_eq!`, not in an allocation sized by it
    /// (repository rule, 2026-08-16).
    #[test]
    fn a_verbose_verdict_is_bounded_before_it_reaches_the_history() {
        assert_eq!(MAX_GAPS, 32);
        assert_eq!(MAX_GAP_CHARS, 400);
        let gaps: Vec<String> = (0..100)
            .map(|i| format!("\"gap {i} {}\"", "x".repeat(900)))
            .collect();
        let raw = format!("{{\"gaps\": [{}]}}", gaps.join(","));
        let v = SkepticVerdict::parse(&raw);
        assert!(v.refuted);
        assert_eq!(v.gaps.len(), 32, "the number of kept gaps is bounded");
        for gap in &v.gaps {
            assert!(
                gap.chars().count() <= 401,
                "a kept gap is bounded in length"
            );
        }
    }

    /// The fingerprint stays bounded too, and deterministic: we sort BEFORE
    /// cutting, so the same verdict always yields the same fingerprint.
    #[test]
    fn the_fingerprint_is_bounded_and_deterministic() {
        assert_eq!(MAX_FINGERPRINT_CITATIONS, 16);
        let gaps: Vec<String> = (0..60)
            .map(|i| format!("src/file{i:03}.rs:{i} unproven"))
            .collect();
        let first = gap_fingerprint(&gaps);
        assert_eq!(first.lines().count(), 16);
        let mut shuffled = gaps.clone();
        shuffled.reverse();
        assert_eq!(first, gap_fingerprint(&shuffled));
    }

    /// The skeptic prompt is in English, descriptive, and bounded.
    ///
    /// The repository rule: any text the model reads is in English, and never
    /// tells it that it is confined or deceived.
    #[test]
    fn the_skeptic_prompt_is_english_bounded_and_never_hostile() {
        assert_eq!(MAX_REPORT_CHARS, 8_000);
        assert!(SKEPTIC_PROMPT.is_ascii() && SKEPTIC_SYSTEM.is_ascii());
        for hostile in ["trapped", "sandbox", "you believe", "deceive", "locked"] {
            assert!(
                !SKEPTIC_PROMPT.to_lowercase().contains(hostile)
                    && !SKEPTIC_SYSTEM.to_lowercase().contains(hostile),
                "the prompt must not contain \"{hostile}\""
            );
        }
        let long_report = "z".repeat(20_000);
        let prompt = skeptic_prompt("ship the auth refactor", &long_report);
        assert!(prompt.contains("ship the auth refactor"));
        assert!(prompt.contains("gaps"));
        assert!(
            prompt.chars().count() < 9_000,
            "the reviewed report must be truncated, got {} characters",
            prompt.chars().count()
        );
    }

    /// A scripted skeptic. It does NOT LOOP: the exhausted script returns an
    /// error, never the last answer forever (machine crash of 2026-08-15,
    /// docs/invariants.md).
    struct ScriptedSkeptic {
        replies: Mutex<std::collections::VecDeque<std::result::Result<String, String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedSkeptic {
        fn new(replies: Vec<std::result::Result<String, String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl SkepticOracle for ScriptedSkeptic {
        async fn review(&self, prompt: &str) -> Result<String> {
            self.calls.lock().unwrap().push(prompt.to_string());
            match self.replies.lock().unwrap().pop_front() {
                Some(Ok(s)) => Ok(s),
                Some(Err(e)) => Err(anyhow::anyhow!(e)),
                None => Err(anyhow::anyhow!("skeptic script exhausted")),
            }
        }
        fn model_label(&self) -> String {
            "scripted/skeptic".into()
        }
    }

    #[tokio::test]
    async fn a_skeptic_pass_records_its_verdict_in_the_goal_history() {
        let (_dir, m) = make_manager();
        m.set(
            "diagnose the slow service".into(),
            Budget::new(Some(9), None),
            None,
        )
        .await
        .unwrap();
        let oracle = ScriptedSkeptic::new(vec![Ok(
            "{\"gaps\": [\"src/policy.rs:458 is never called\"]}".into(),
        )]);

        let out = m
            .skeptic_pass(&oracle, "everything looks fine")
            .await
            .unwrap()
            .unwrap();

        assert!(out.verdict.refuted);
        assert!(!out.stalled);
        assert_eq!(out.fingerprint, "src/policy.rs:458");
        assert!(matches!(out.state.status, GoalStatus::Active));
        assert!(
            out.state
                .history
                .iter()
                .any(|h| matches!(h.kind, HistoryKind::Skeptic)),
            "the pass must leave a typed trace in the history"
        );
        // The skeptic sees the goal and the report, nothing else.
        let prompt = oracle.calls.lock().unwrap()[0].clone();
        assert!(prompt.contains("diagnose the slow service"));
        assert!(prompt.contains("everything looks fine"));
    }

    /// Skeptic off: the model is NOT called.
    ///
    /// A second pass is a second model call per turn, hence money on every
    /// turn. A failure would mean the goal setting is read after the expense,
    /// i.e. not read at all: the operator would pay for a pass they explicitly
    /// turned off.
    #[tokio::test]
    async fn a_goal_whose_skeptic_is_off_is_never_sent_to_the_model() {
        let (_dir, m) = make_manager();
        m.set(
            "diagnose the slow service".into(),
            Budget::new(Some(9), None),
            None,
        )
        .await
        .unwrap();
        m.set_skeptic(SkepticCadence::Off).await.unwrap().unwrap();

        let oracle = ScriptedSkeptic::new(vec![Ok("{\"gaps\": []}".into())]);
        let out = m.skeptic_pass(&oracle, "a report").await.unwrap();

        assert!(out.is_none(), "a disabled skeptic returns no verdict");
        assert_eq!(oracle.call_count(), 0, "the model was called anyway");
        let after = m.get_active().await.unwrap().unwrap();
        assert!(
            !after
                .history
                .iter()
                .any(|h| matches!(h.kind, HistoryKind::Skeptic)),
            "a disabled pass writes no pass entry"
        );
    }

    /// An unreadable cadence reads back as the default, and the default reviews.
    ///
    /// SECURITY: same choice as `GoalStatus` (T15) — a damaged snapshot must
    /// not REMOVE a check. `"off"` is the only value that turns the skeptic
    /// off, and it has to have been written on purpose.
    #[test]
    fn an_unreadable_skeptic_cadence_reads_back_as_every_turn() {
        // Fixed, literal size: no fixture derives from a constant.
        for raw in [
            "\"martian\"",
            "\"\"",
            "\"OFF\"",
            "42",
            "null",
            "true",
            "[]",
            "{}",
            "{\"kind\":\"off\"}",
        ] {
            let parsed: SkepticCadence = serde_json::from_str(raw).unwrap_or_else(|e| {
                panic!("an unreadable cadence must not be an error: {raw} → {e}")
            });
            assert_eq!(
                parsed,
                SkepticCadence::EveryTurn,
                "{raw} disabled the skeptic"
            );
        }
        // Control: the only form that really turns it off, and the round trip.
        assert_eq!(
            serde_json::from_str::<SkepticCadence>("\"off\"").unwrap(),
            SkepticCadence::Off
        );
        for cadence in [SkepticCadence::EveryTurn, SkepticCadence::Off] {
            let text = serde_json::to_string(&cadence).unwrap();
            assert_eq!(
                serde_json::from_str::<SkepticCadence>(&text).unwrap(),
                cadence,
                "{cadence:?} does not read back as itself ({text})"
            );
        }
    }

    /// The same gaps twice: the goal blocks, and the budget is preserved —
    /// we stop spinning, we do not punish the goal.
    #[tokio::test]
    async fn the_same_gaps_twice_block_the_goal_without_spending_budget() {
        let (_dir, m) = make_manager();
        m.set(
            "diagnose the slow service".into(),
            Budget::new(Some(9), None),
            None,
        )
        .await
        .unwrap();
        let answer = "{\"gaps\": [\"/tmp/run-a/report.md:12 lacks a measurement\"]}";
        let second = "{\"gaps\": [\"/tmp/run-b/report.md:12 still lacks a measurement\"]}";
        let oracle = ScriptedSkeptic::new(vec![Ok(answer.into()), Ok(second.into())]);

        let first = m
            .skeptic_pass(&oracle, "report one")
            .await
            .unwrap()
            .unwrap();
        assert!(!first.stalled);
        let repeat = m
            .skeptic_pass(&oracle, "report two")
            .await
            .unwrap()
            .unwrap();

        assert!(repeat.stalled, "the same fingerprint twice must stop");
        assert!(
            matches!(repeat.state.status, GoalStatus::Blocked { .. }),
            "got {:?}",
            repeat.state.status
        );
        assert_eq!(
            repeat.state.budget.steps_used, 0,
            "the budget is preserved: blocking is not spending"
        );
        assert_eq!(oracle.call_count(), 2);
    }

    /// A failed skeptic neither blocks nor validates: it is refuted, without
    /// a fingerprint, and the goal continues.
    ///
    /// SECURITY: if a failure produced a fingerprint, two failures in a row
    /// would stop a healthy goal; if it validated, the absence of an answer
    /// would count as approval. Neither.
    #[tokio::test]
    async fn a_broken_skeptic_neither_validates_nor_stalls_the_goal() {
        let (_dir, m) = make_manager();
        m.set(
            "diagnose the slow service".into(),
            Budget::new(Some(9), None),
            None,
        )
        .await
        .unwrap();
        let oracle = ScriptedSkeptic::new(vec![
            Err("connection reset".into()),
            Err("connection reset".into()),
        ]);

        let first = m
            .skeptic_pass(&oracle, "report one")
            .await
            .unwrap()
            .unwrap();
        let second = m
            .skeptic_pass(&oracle, "report two")
            .await
            .unwrap()
            .unwrap();

        for out in [&first, &second] {
            assert!(out.verdict.refuted, "a failure counts as refuted");
            assert!(out.verdict.gaps.is_empty());
            assert_eq!(out.fingerprint, "");
            assert!(!out.stalled);
        }
        assert!(matches!(second.state.status, GoalStatus::Active));
    }

    /// No active goal: no model call. A second pass costs a call, and a call
    /// without a goal reviews nothing.
    #[tokio::test]
    async fn a_skeptic_pass_without_an_active_goal_calls_nothing() {
        let (_dir, m) = make_manager();
        let oracle = ScriptedSkeptic::new(vec![Ok("{\"gaps\": []}".into())]);
        assert!(m.skeptic_pass(&oracle, "report").await.unwrap().is_none());
        assert_eq!(oracle.call_count(), 0);

        m.set("t".into(), Budget::new(Some(9), None), None)
            .await
            .unwrap();
        m.pause().await.unwrap();
        assert!(
            m.skeptic_pass(&oracle, "report").await.unwrap().is_none(),
            "a paused goal has no report to review"
        );
        assert_eq!(oracle.call_count(), 0);
    }
}
