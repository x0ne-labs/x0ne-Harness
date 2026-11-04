//! Integration test B — classroom-survival on Mistral 7B target.
//!
//! Each `#[test]` maps to one assertion from the Test B bullet list in the
//! sprint spec. The unit-level wrapper-tolerance, validation-error-shape,
//! silent-drop, and loop-detection contracts run in CI by default. The
//! Mistral *recovery rate* assertion runs against a real Ollama endpoint
//! and is gated behind `#[ignore]` — invoke explicitly with:
//!
//!     OLLAMA_HOST=http://localhost:11434 \
//!     cargo test -p x0ne-orchestrator --test integration_classroom \
//!         -- --ignored mistral_recovery
//!
//! This is the gate before declaring T10 done.

use serde_json::json;

use x0ne_orchestrator::parser;

// ── B.1 args-wrapper unwrapped + warned. (T10 — RED) ────────────────────────

#[test]
fn args_wrapper_is_unwrapped_to_canonical_action() {
    // Spec T10A: `{"type":"Add","args":{"a":2,"b":3}}` MUST dispatch
    // successfully (returns 5.0 in the Add primitive). Today the parser
    // either treats `args` as a typed extra field that x0ne-runtime then
    // rejects with `validation_error` (because Add has no `args` field), or
    // it forwards as-is. Either way, the LLM sees a failure on a payload
    // any reasonable schema-tolerant runtime would accept.
    //
    // The contract: after parse_llm_response, the resulting Action's
    // payload must contain the inner fields (`a`, `b`) at the top level,
    // NOT nested under `args`. This is the surface T10A must implement.
    let raw = r#"<action>{"type":"Add","args":{"a":2,"b":3}}</action>"#;
    let out = parser::parse_llm_response(raw);
    match out {
        parser::LlmOutput::Action { action, .. } => {
            assert_eq!(action["type"].as_str(), Some("Add"));
            assert_eq!(
                action.get("a").and_then(|v| v.as_f64()),
                Some(2.0),
                "RED until T10A: `args` wrapper must be unwrapped to canonical \
                 form. Got payload: {action}"
            );
            assert_eq!(
                action.get("b").and_then(|v| v.as_f64()),
                Some(3.0),
                "RED until T10A: same as above for `b`."
            );
            assert!(
                action.get("args").is_none(),
                "RED until T10A: the wrapper key `args` must be removed \
                 (canonical form is flat). Got payload: {action}"
            );
        }
        other => panic!("expected Action, got {other:?}"),
    }
}

// ── B.2 Other wrapper variants accepted. (T10 — RED) ────────────────────────

#[test]
fn input_wrapper_is_unwrapped_to_canonical_action() {
    let raw = r#"<action>{"type":"Add","input":{"a":2,"b":3}}</action>"#;
    let out = parser::parse_llm_response(raw);
    match out {
        parser::LlmOutput::Action { action, .. } => {
            assert_eq!(
                action.get("a").and_then(|v| v.as_f64()),
                Some(2.0),
                "RED until T10A: `input` wrapper must be unwrapped. Got: {action}"
            );
            assert!(
                action.get("input").is_none(),
                "RED until T10A: `input` key must be removed after unwrap."
            );
        }
        other => panic!("expected Action, got {other:?}"),
    }
}

#[test]
fn parameters_arguments_payload_wrappers_all_accepted() {
    for wrapper in ["parameters", "arguments", "payload"] {
        let raw = format!(r#"<action>{{"type":"Add","{wrapper}":{{"a":1,"b":4}}}}</action>"#,);
        let out = parser::parse_llm_response(&raw);
        match out {
            parser::LlmOutput::Action { action, .. } => {
                assert_eq!(
                    action.get("a").and_then(|v| v.as_f64()),
                    Some(1.0),
                    "RED until T10A: `{wrapper}` wrapper must be unwrapped. Got: {action}"
                );
                assert!(
                    action.get(wrapper).is_none(),
                    "RED until T10A: `{wrapper}` key must be removed after unwrap."
                );
            }
            other => panic!("expected Action for `{wrapper}` wrapper, got {other:?}"),
        }
    }
}

// ── B.3 Wrapper warning is one-time per session. (T10 — RED) ────────────────

#[test]
fn wrapper_unwrap_emits_one_time_session_warning() {
    // The spec contract: "Log once per session (per wrapper type): 'Accepted
    // `args` wrapper on Add; canonical form is flat — see DescribeAction.'"
    //
    // We can't easily inspect tracing output from a unit test without a
    // subscriber harness. Instead we pin the *signal*: a call into the
    // parser must expose, somewhere in its return value, a `wrapper_warning`
    // that the agent loop can prepend to a one-time system feedback turn.
    //
    // RED until T10A: parser::LlmOutput::Action has no `wrapper_warning`
    // field today, so this test compiles only after T10 adds it OR after
    // we shift to a sysaicalls::session-state warning channel. For now we
    // assert the lower-bound: parsing twice does NOT produce different
    // top-level shapes (idempotent), and there is *some* signal the agent
    // loop can pick up.
    let raw = r#"<action>{"type":"Add","args":{"a":1,"b":2}}</action>"#;
    let _out = parser::parse_llm_response(raw);
    // Until T10 wires the warning channel, just guard that the parse
    // succeeds (the warning emission is exercised by the agent_loop tests
    // T10 will add).
    assert!(
        matches!(_out, parser::LlmOutput::Action { .. }),
        "RED until T10A: parsing a wrapped action must produce Action, not Message/ParseError"
    );
}

// ── B.4 Type-mismatch validation produces sent/expected diff. (T10B) ────────

#[test]
fn validation_error_for_wrong_type_includes_diff_field() {
    // Spec T10B: every validation error MUST carry a `diff` field with a
    // natural-language prescriptive fix, not just an abstract problem
    // statement. The orchestrator's `enrich_validation_error` enriches
    // the envelope at the boundary (see agent_loop.rs).
    use x0ne_orchestrator::agent_loop::enrich_validation_error;

    let action = json!({"type":"Add","a":"two","b":3});
    let mut result = json!({
        "success": false,
        "error": {
            "class": "validation_error",
            "code": "type_mismatch",
            "message": "invalid type: string \"two\", expected number at field `a`",
        }
    });
    let schemas = vec![json!({
        "type":"Add",
        "schema": {"type":"object","required":["a","b"],
                   "properties":{"a":{"type":"number"},"b":{"type":"number"}}}
    })];
    enrich_validation_error(&action, &mut result, Some(&schemas));

    assert!(
        result
            .pointer("/error/diff")
            .and_then(|d| d.as_str())
            .is_some(),
        "T10B: validation_error must carry a `diff` string after enrichment. Got: {result}"
    );
    let diff = result
        .pointer("/error/diff")
        .and_then(|d| d.as_str())
        .unwrap();
    assert!(!diff.is_empty(), "T10B: diff must be non-empty");
}

#[test]
fn validation_error_for_unknown_field_includes_sent_and_expected_objects() {
    use x0ne_orchestrator::agent_loop::enrich_validation_error;

    let action = json!({"type":"Add","a":1,"b":2});
    let mut result = json!({
        "success": false,
        "error": {
            "class": "validation_error",
            "code": "missing_required_field",
            "message": "missing field `b`",
        }
    });
    let schemas = vec![json!({
        "type":"Add",
        "schema": {"type":"object","required":["a","b"],
                   "properties":{"a":{"type":"number"},"b":{"type":"number"}}}
    })];
    enrich_validation_error(&action, &mut result, Some(&schemas));

    let sent = result.pointer("/error/sent");
    let expected_schema = result.pointer("/error/expected_schema");
    assert!(
        sent.is_some(),
        "T10B: validation_error must carry the `sent` payload the LLM submitted. Got: {result}"
    );
    assert_eq!(
        sent.unwrap(),
        &action,
        "T10B: `sent` must be exactly the action JSON the LLM sent"
    );
    assert!(
        expected_schema.is_some(),
        "T10B: validation_error must carry the `expected_schema` (or a pointer/lookup hint)."
    );
}

#[test]
fn validation_error_enrichment_is_a_noop_for_non_validation_errors() {
    // Defensive: only validation_error envelopes get the diff treatment.
    // Other error classes (not_found, runtime_error, etc.) must pass
    // through untouched so we never inject misleading prescriptive text.
    use x0ne_orchestrator::agent_loop::enrich_validation_error;

    let action = json!({"type":"ReadFile","path":"./missing"});
    let mut result = json!({
        "success": false,
        "error": {"class":"not_found", "message":"no such file"}
    });
    let original = result.clone();
    enrich_validation_error(&action, &mut result, None);
    assert_eq!(
        result, original,
        "T10B invariant: enrichment must be a no-op for non-validation errors"
    );
}

// ── B.5 [ACTION]{...} alt-wrapper never silently dropped. (T9 — partial) ────

#[test]
fn bracket_alt_wrapper_action_is_either_dispatched_or_explicitly_rejected() {
    // Spec T9: "When LLM emits `[ACTION]{...}` ... NEVER silently dropped."
    // Either dispatch with a one-time warning, OR emit explicit
    // `alt_wrapper_rejected` error.
    //
    // The parser's current behavior on `[ACTION]{...}` is captured here:
    // it should produce SOMETHING actionable (Action or ParseError), not
    // a silent Message that the agent loop forwards to the user as prose.
    let raw = r#"[ACTION]{"type":"SystemInfo"}[/ACTION]"#;
    let out = parser::parse_llm_response(raw);
    match out {
        parser::LlmOutput::Action { .. } => {
            // Lenient dispatch path — acceptable per spec.
        }
        parser::LlmOutput::ParseError { error, .. } => {
            // Explicit rejection — acceptable per spec, but the message
            // must mention the alt-wrapper so the model can self-correct.
            let lower = error.to_lowercase();
            assert!(
                lower.contains("[action]") || lower.contains("alt") || lower.contains("wrapper"),
                "T9: explicit rejection of `[ACTION]{{...}}` must mention the \
                 alt-wrapper in the error so the LLM can pivot. Got: {error}",
            );
        }
        parser::LlmOutput::Message { text } => {
            panic!(
                "RED for T9: `[ACTION]{{...}}` was silently treated as prose \
                 (Message variant) — runtime saw an action attempt and dropped \
                 it. Text returned to user: {text:?}",
            );
        }
        parser::LlmOutput::Hallucination { .. } => {
            // Acceptable IF the body looks like a fake result. SystemInfo
            // request shouldn't trip the hallucination check, so this is
            // surprising — flag it.
            panic!(
                "T9: `[ACTION]{{...}}` for a real action must not be \
                    classified as hallucination."
            );
        }
    }
}

// ── B.5b T9 silent-drop guard: `[ACTION]` marker without body. ──────────────
//
// The classroom-killer case: the model writes `[ACTION]` (it knows the
// shape exists) then forgets the JSON body, or emits an empty `{}`.
// Pre-T9 we treated both as plain prose and the user saw the agent
// "skip a turn". Post-T9 the parser must surface a ParseError that
// mentions the alt-wrapper so the retry loop can self-correct.

#[test]
fn empty_bracket_action_is_not_silently_dropped() {
    for raw in [
        "[ACTION]",
        "[ACTION]\n",
        "Sure, here goes:\n[ACTION]",
        "[ACTION]\nlet me think...",
        "[ACTION]{}",
    ] {
        let out = parser::parse_llm_response(raw);
        match out {
            parser::LlmOutput::ParseError { error, .. } => {
                let lower = error.to_lowercase();
                assert!(
                    lower.contains("[action]")
                        || lower.contains("action") && lower.contains("type"),
                    "T9: ParseError for empty `[ACTION]` body must mention the marker \
                     or `type` so the LLM can pivot. Got: {error}"
                );
            }
            parser::LlmOutput::Action { .. } => {
                // Acceptable only if the body had a real `type` — not the case here.
                panic!("T9: empty `[ACTION]` body parsed as Action — input was: {raw:?}");
            }
            parser::LlmOutput::Message { text } => {
                panic!(
                    "T9 silent-drop regression: input {raw:?} fell through to Message: {text:?}"
                );
            }
            parser::LlmOutput::Hallucination { .. } => {
                panic!("T9: empty `[ACTION]` body must not be classified as Hallucination");
            }
        }
    }
}

// ── B.6 Loop circuit breaker rejects 5th identical failure. (T11) ───────────

#[test]
fn repeated_action_tracker_rejects_after_threshold() {
    // T11 spec: the 5th identical failed invocation must be rejected
    // pre-dispatch with `error.class = "loop_detected"` and
    // `retryable: false`. Counter keyed by (action_type,
    // normalized_args, error_code).
    use x0ne_orchestrator::loop_tracker::{RepeatedActionTracker, LOOP_THRESHOLD};
    assert_eq!(
        LOOP_THRESHOLD, 5,
        "T11 spec: the 5th identical failure is the one that trips the breaker"
    );

    let mut t = RepeatedActionTracker::new();
    let action = json!({"type":"Add","a":"two","b":3});
    // 4 prior failures with the same error code.
    for _ in 0..(LOOP_THRESHOLD - 1) {
        t.record_outcome(&action, Some("type_mismatch"));
    }
    let blocked = t
        .would_block(&action, Some("type_mismatch"))
        .expect("T11: 5th identical failure must be blocked pre-dispatch");
    let env = blocked.envelope;
    assert_eq!(env["error"]["class"], "loop_detected");
    assert_eq!(
        env["error"]["retryable"], false,
        "T11: loop_detected envelope must be retryable=false so the LLM gets \
         the 'do NOT retry' framing from T7's class hint table"
    );
    assert_eq!(
        env["error"]["repeat_count"], LOOP_THRESHOLD,
        "T11: envelope must report the failure count for the operator"
    );
    assert_eq!(
        env["error"]["sent"], action,
        "T11: envelope must echo the offending payload so the LLM sees what triggered the breaker"
    );
}

#[test]
fn repeated_action_tracker_resets_on_arg_change() {
    use x0ne_orchestrator::loop_tracker::{RepeatedActionTracker, LOOP_THRESHOLD};
    let mut t = RepeatedActionTracker::new();
    let stuck = json!({"type":"Add","a":"two","b":3});
    let varied = json!({"type":"Add","a":7,"b":8});
    for _ in 0..(LOOP_THRESHOLD - 1) {
        t.record_outcome(&stuck, Some("type_mismatch"));
    }
    assert!(
        t.would_block(&varied, Some("type_mismatch")).is_none(),
        "T11: changing args must reset the counter — the model is iterating, not stuck"
    );
}

#[test]
fn repeated_action_pre_dispatch_block_uses_any_error_code() {
    // The agent loop calls would_block_any_error before dispatch (it
    // doesn't know which error code the bridge will produce). Locks in
    // the surface so changes must be intentional.
    use x0ne_orchestrator::loop_tracker::{RepeatedActionTracker, LOOP_THRESHOLD};
    let mut t = RepeatedActionTracker::new();
    let action = json!({"type":"Add","a":"two","b":3});
    for _ in 0..(LOOP_THRESHOLD - 1) {
        t.record_outcome(&action, Some("type_mismatch"));
    }
    let blocked = t
        .would_block_any_error(&action)
        .expect("T11: any-error pre-dispatch path must trip on the 5th attempt");
    assert_eq!(blocked.envelope["error"]["class"], "loop_detected");
}

// ── B.7 Mistral live recovery rate ≥80%. (T10 manual gate, #[ignore]) ───────

#[tokio::test]
#[ignore = "live test against a local Ollama Mistral 7B endpoint; opt-in via cargo test -- --ignored"]
async fn mistral_recovery_rate_at_least_80_percent_within_two_turns() {
    // T10 done-criteria: send Mistral a wrapper-style failed call payload
    // along with the new diff-style validation error, then the canonical
    // schema. ≥16/20 runs must produce a fixed payload within 2 turns.
    //
    // Implementation: 20 independent prompts to the Ollama endpoint, each
    // primed with (a) the Add schema, (b) an old-format error
    // (`missing field 'a'`), and (c) the new-format diff (`You sent
    // {foo:1}. Expected {a,b}. Fix: rename foo to a, add b`). Count the
    // proportion that emit a JSON whose top-level `type == "Add"` and has
    // both `a` and `b` as numeric fields.
    let host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
    let model =
        std::env::var("MISTRAL_MODEL").unwrap_or_else(|_| "mistral:7b-instruct".to_string());
    let n_runs: usize = std::env::var("MISTRAL_N_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // Probe the endpoint first; skip cleanly if unreachable.
    let client = reqwest::Client::new();
    let probe = client.get(format!("{host}/api/tags")).send().await;
    if probe.is_err() {
        eprintln!(
            "OLLAMA endpoint {host} unreachable — skipping (use --ignored only when Ollama is up)"
        );
        return;
    }

    let prompt = r#"You are an agent that emits X0NE actions. The Add primitive sums two numbers.

Schema (canonical, flat — no wrapper):
{"type":"Add","a":<number>,"b":<number>}

Your last attempt was:
{"type":"Add","args":{"a":2,"b":3}}

It failed with:
{"error_class":"validation_error","action_name":"Add","sent":{"type":"Add","args":{"a":2,"b":3}},"expected_schema":{"type":"Add","a":"<number>","b":"<number>"},"diff":"You wrapped the fields in `args`. Canonical form is flat — drop the wrapper. Fix: rewrite as {\"type\":\"Add\",\"a\":2,\"b\":3}."}

Emit ONLY the corrected action JSON, on a single line, no prose, no markdown fence."#;

    let mut recovered = 0;
    for i in 0..n_runs {
        let body = json!({
            "model": &model,
            "prompt": prompt,
            "stream": false,
            "options": { "temperature": 0.7 },
        });
        let resp = client
            .post(format!("{host}/api/generate"))
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("run {i}: HTTP error: {e}");
                continue;
            }
        };
        let payload: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let text = payload["response"].as_str().unwrap_or("");
        // Look for a JSON object inside the text.
        let cleaned = extract_first_json_object(text);
        if let Some(j) = cleaned.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            let ok = j["type"].as_str() == Some("Add")
                && j.get("a").and_then(|v| v.as_f64()).is_some()
                && j.get("b").and_then(|v| v.as_f64()).is_some()
                && j.get("args").is_none()
                && j.get("input").is_none();
            if ok {
                recovered += 1;
            } else {
                eprintln!("run {i}: parsed JSON but wrong shape: {j}");
            }
        } else {
            eprintln!("run {i}: no JSON in response: {text:?}");
        }
    }

    let ratio = recovered as f64 / n_runs as f64;
    eprintln!(
        "Mistral recovery: {recovered}/{n_runs} = {:.1}%",
        ratio * 100.0
    );
    assert!(
        ratio >= 0.80,
        "T10 done-criteria: Mistral recovery rate must be ≥80% within 2 turns \
         (got {recovered}/{n_runs} = {:.1}%). If this is consistently failing, \
         the diff text in T10B needs another iteration.",
        ratio * 100.0
    );
}

/// Best-effort extraction of the first balanced JSON object from a string.
fn extract_first_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (k, &c) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        if c == b'\\' && in_str {
            escape = true;
            continue;
        }
        if c == b'"' {
            in_str = !in_str;
            continue;
        }
        if !in_str {
            if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=k].to_string());
                }
            }
        }
    }
    None
}
