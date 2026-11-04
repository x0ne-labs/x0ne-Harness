//! `RenderTemplate` — single-pass placeholder substitution.
//!
//! The LLM emits one string sprinkled with `{{tool(args)}}` placeholders;
//! we parse, dispatch each placeholder through the regular action executor,
//! and splice the live result back in. Net result: one LLM round-trip
//! produces a fully grounded answer, no re-prompt to "compose the final
//! response" from tool outputs.
//!
//! Two placeholder syntaxes coexist:
//!
//!   Call form: `{{ str_count("strawberry", "r") }}`
//!     - snake_case tool name + parenthesised JSON-literal args
//!     - args bind positionally against `POSITIONAL_BINDINGS` (below)
//!
//!   JSON form: `{{ StrCount {"haystack":"strawberry","needle":"r"} }}`
//!     - PascalCase Action name + the same JSON params block you'd put
//!       inside `Action::StrCount`
//!     - escape hatch for any Action not in the positional table
//!
//! Recursion: a tool's rendered string can itself contain `{{…}}` markers
//! (e.g. a `ReadFile` of a template file). We re-run the pass up to
//! `max_depth` times before bailing — bounded at 8 to keep latency
//! predictable.

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::executor::desktop::RateLimiter;
use crate::executor::execute_action_at_depth;
use crate::handles::SharedHandles;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionOutput, ActionResult, ErrorKind};
use serde_json::{json, Value};
use x0ne_dataset::pascalize;

/// Positional → named-field bindings for the most common pure-function
/// Actions. Order MUST match the field order in `Action` so that
/// `{{ tool(arg0, arg1, …) }}` calls bind in the natural left-to-right
/// reading order. Anything not in this table can still be invoked via
/// JSON-form (`{{ Pascal { …params… } }}`).
///
/// Keep ALPHABETICAL within sections so missing entries are easy to spot.
const POSITIONAL_BINDINGS: &[(&str, &[&str])] = &[
    // Math / String / Comparison primitives removed in v0.3 — use shell.
    // System / FS (read-only, side-effect-free, safe in a fluent template)
    ("checksum", &["path", "algorithm"]),
    ("dns_lookup", &["hostname"]),
    ("file_info", &["path"]),
    ("read_file", &["path"]),
];

/// Maximum allowed `max_depth`. Past this, even a benign template can
/// burn measurable CPU on repeated parser passes. 8 is generous —
/// real-world templates settle in 1–2 passes.
const MAX_DEPTH_CAP: u32 = 8;

/// One parsed placeholder, captured by the parser before dispatch.
#[derive(Debug, Clone)]
struct Placeholder {
    /// Inclusive start byte index in the template (the first `{` of `{{`).
    start: usize,
    /// Exclusive end byte index (just past the final `}` of `}}`).
    end: usize,
    /// snake_case (call form) or PascalCase (JSON form) identifier.
    name: String,
    /// Parsed args:
    ///   call form  → ordered Vec<Value> (positional)
    ///   JSON form  → single-element Vec carrying the params object
    args: Vec<Value>,
    /// True if the placeholder used the JSON-form brace block, in which
    /// case `args[0]` is the full params object and the name is already
    /// PascalCase.
    is_json_form: bool,
}

/// Scan the template and return every well-formed top-level `{{…}}`
/// placeholder. Malformed candidates are ignored (treated as literal text)
/// so a stray `{{` in prose doesn't blow up the whole render.
fn extract_placeholders(template: &str) -> Vec<Placeholder> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(ph) = try_parse_placeholder(template, i) {
                i = ph.end;
                out.push(ph);
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Attempt to parse one placeholder starting at byte index `start` (which
/// must point at `{{`). Returns `None` if the content isn't a valid
/// `{{ name(args) }}` or `{{ Name {params} }}`.
fn try_parse_placeholder(template: &str, start: usize) -> Option<Placeholder> {
    let rest = &template[start + 2..]; // past the `{{`
    let close = find_balanced_close(rest)?;
    let inner = rest[..close].trim();
    let absolute_end = start + 2 + close + 2; // include the closing `}}`

    // Split into name + body. Name is everything up to first `(` or `{`.
    let (name, body) = split_name_body(inner)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    if body.starts_with('(') {
        // Call form: name(arg1, arg2, ...)
        if !body.ends_with(')') {
            return None;
        }
        let args_src = &body[1..body.len() - 1].trim();
        let args = parse_call_args(args_src)?;
        Some(Placeholder {
            start,
            end: absolute_end,
            name: name.to_string(),
            args,
            is_json_form: false,
        })
    } else if body.starts_with('{') {
        // JSON form: Name {…}
        let params: Value = serde_json::from_str(body).ok()?;
        if !params.is_object() {
            return None;
        }
        Some(Placeholder {
            start,
            end: absolute_end,
            name: name.to_string(),
            args: vec![params],
            is_json_form: true,
        })
    } else if body.is_empty() {
        // Bare name (zero-arg call), e.g. `{{ system_info }}`.
        Some(Placeholder {
            start,
            end: absolute_end,
            name: name.to_string(),
            args: vec![],
            is_json_form: false,
        })
    } else {
        None
    }
}

/// Find the byte index of the matching `}}` in `s`, respecting brace
/// balance so `{{ Name {"a": {"b": 1}} }}` works. Returns the index of
/// the first `}` of the closing `}}` pair (not past it).
fn find_balanced_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                i += 1;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    i += 1;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                    return Some(i);
                } else {
                    return None;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Split `inner` (already trimmed) into `(name, body)` where `body` is
/// the first contiguous run starting at `(` or `{`, OR the empty string
/// if the inner is just a bare identifier.
fn split_name_body(inner: &str) -> Option<(&str, &str)> {
    let mut split_at = inner.len();
    for (i, c) in inner.char_indices() {
        if c == '(' || c == '{' {
            split_at = i;
            break;
        }
    }
    let (n, b) = inner.split_at(split_at);
    Some((n, b))
}

/// Parse comma-separated JSON literal args by wrapping in `[…]` and
/// letting serde handle the heavy lifting. Empty `()` → empty vec.
fn parse_call_args(src: &str) -> Option<Vec<Value>> {
    if src.is_empty() {
        return Some(vec![]);
    }
    let wrapped = format!("[{}]", src);
    let parsed: Value = serde_json::from_str(&wrapped).ok()?;
    parsed.as_array().cloned()
}

/// Turn a `Placeholder` into a valid `Action` JSON object. Returns an
/// error string if the tool name has no positional binding AND no JSON
/// params block was provided.
fn placeholder_to_action_json(ph: &Placeholder) -> Result<Value, String> {
    if ph.is_json_form {
        // Name is already PascalCase (caller convention); just stamp the
        // type field on top of the params object.
        let mut obj = ph.args[0].as_object().cloned().unwrap_or_default();
        obj.insert("type".into(), Value::String(ph.name.clone()));
        return Ok(Value::Object(obj));
    }
    // Call form — look up positional binding.
    let binding = POSITIONAL_BINDINGS
        .iter()
        .find(|(k, _)| *k == ph.name)
        .map(|(_, fields)| *fields);
    let pascal = pascalize(&ph.name);
    match binding {
        Some(fields) => {
            if ph.args.len() > fields.len() {
                return Err(format!(
                    "tool `{}` expects at most {} positional args, got {}",
                    ph.name,
                    fields.len(),
                    ph.args.len()
                ));
            }
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String(pascal));
            for (field, value) in fields.iter().zip(ph.args.iter()) {
                obj.insert((*field).to_string(), value.clone());
            }
            Ok(Value::Object(obj))
        }
        None => {
            if ph.args.is_empty() {
                // Zero-arg, no binding needed (e.g. `{{ system_info }}`).
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String(pascal));
                Ok(Value::Object(obj))
            } else {
                Err(format!(
                    "tool `{}` has no positional binding — use JSON form: \
                     {{{{ {} {{\"field\":value, …}} }}}}",
                    ph.name, pascal
                ))
            }
        }
    }
}

/// Render a `f64` cleanly: integers stay integer-formatted (no trailing
/// `.0`), fractions use Rust's default float formatting.
fn format_num(n: f64) -> String {
    if n.is_finite() && n == n.trunc() && n.abs() < 1e16 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Coerce an action's `ActionOutput` into the string we splice back into
/// the template. Strategy:
///   1. Scalar-bearing outputs (counts, lengths, math results, strings)
///      use their natural representation.
///   2. Anything else falls back to compact JSON of the `data` payload
///      via `Generic` — predictable, never panics.
fn output_to_string(out: &ActionOutput) -> String {
    use ActionOutput::*;
    match out {
        // Math — every variant collapses to its primary scalar.
        MathResult { result } => format_num(*result),
        MathExactResult { result, .. } => format_num(*result),
        MathBigResult { result, .. } => result.clone(),
        MathBoolResult { result } => result.to_string(),
        MathCompareResult { result, .. } => result.clone(),
        MathMinMaxResult { result, .. } => format_num(*result),
        MathAggregateResult { result, .. } => format_num(*result),
        MathEqResult { result, .. } => result.to_string(),
        // String
        StringLength { length } => length.to_string(),
        StringCount { count } => count.to_string(),
        StringMatch { matches, .. } => matches.to_string(),
        StringResult { result } => result.clone(),
        StringParts { parts, .. } => parts.join(","),
        // Files / system — give a short, useful representation rather than
        // raw bytes / huge tree dumps.
        FileContent { content, .. } => content.clone(),
        Generic { data } => match data {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        },
        // Fallback — serialise the variant verbatim so the caller can
        // inspect it. Better than panicking.
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unserializable>".into()),
    }
}

/// Public entry point — wired in `executor::mod`. Mirrors the signature
/// of every other async action handler.
pub async fn render_template(
    template: &str,
    max_depth: Option<u32>,
    strict: Option<bool>,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
    depth: usize,
) -> ActionResult {
    let start = std::time::Instant::now();
    let strict = strict.unwrap_or(false);
    let max_depth = max_depth.unwrap_or(2).min(MAX_DEPTH_CAP).max(1);

    let mut current = template.to_string();
    let mut call_log: Vec<Value> = Vec::new();
    let mut passes = 0u32;

    for _ in 0..max_depth {
        let placeholders = extract_placeholders(&current);
        if placeholders.is_empty() {
            break;
        }
        passes += 1;

        // Walk placeholders left-to-right, building a new string by
        // splicing rendered values between literal segments.
        let mut next = String::with_capacity(current.len());
        let mut cursor = 0;
        let mut had_failure = false;

        for ph in &placeholders {
            // Literal slice before this placeholder.
            next.push_str(&current[cursor..ph.start]);

            let call_start = std::time::Instant::now();
            let raw_token = &current[ph.start..ph.end];
            match placeholder_to_action_json(ph) {
                Ok(action_json) => {
                    match serde_json::from_value::<Action>(action_json.clone()) {
                        Ok(action) => {
                            // SECURITY: a placeholder can embed an arbitrary action (including
                            // the JSON form `{{ Name {..} }}`). It MUST pass the same policy gate
                            // as every other nested action — otherwise RenderTemplate is a full
                            // bypass of the command/path allowlists, deny patterns, and the
                            // per-action enable map (see pipeline.rs / composite.rs which do the
                            // same preflight).
                            let policy_block = match policy.evaluate(&action) {
                                PolicyEffect::Allow => None,
                                PolicyEffect::Deny(r) => {
                                    Some(format!("denied by policy — {}", r.message()))
                                }
                                PolicyEffect::RequireConfirmation(_) => Some(
                                    "requires confirmation (not available inside RenderTemplate)"
                                        .to_string(),
                                ),
                            };
                            if let Some(reason) = policy_block {
                                had_failure = true;
                                if strict {
                                    return ActionResult::err(
                                        "RenderTemplate",
                                        ErrorKind::PolicyDenied,
                                        format!("placeholder `{}` blocked: {}", ph.name, reason),
                                        start.elapsed().as_millis() as u64,
                                    );
                                }
                                next.push_str(raw_token);
                                call_log.push(json!({
                                    "tool": ph.name,
                                    "args": ph.args,
                                    "ok": false,
                                    "error": reason,
                                    "duration_ms": call_start.elapsed().as_millis() as u64,
                                }));
                            } else {
                                let result = execute_action_at_depth(
                                    &action,
                                    config,
                                    handles.clone(),
                                    policy,
                                    rate_limiter,
                                    composites,
                                    depth + 1,
                                )
                                .await;
                                if result.success {
                                    let rendered = result
                                        .output
                                        .as_ref()
                                        .map(output_to_string)
                                        .unwrap_or_default();
                                    next.push_str(&rendered);
                                    call_log.push(json!({
                                        "tool": ph.name,
                                        "args": ph.args,
                                        "ok": true,
                                        "value": rendered,
                                        "duration_ms": call_start.elapsed().as_millis() as u64,
                                    }));
                                } else {
                                    had_failure = true;
                                    let err_msg = result
                                        .error
                                        .as_ref()
                                        .map(|e| e.message.clone())
                                        .unwrap_or_else(|| "action failed".into());
                                    if strict {
                                        return ActionResult::err(
                                            "RenderTemplate",
                                            ErrorKind::ExecutionFailed,
                                            format!(
                                                "placeholder `{}` failed: {}",
                                                ph.name, err_msg
                                            ),
                                            start.elapsed().as_millis() as u64,
                                        );
                                    }
                                    // Non-strict: leave the raw token in place.
                                    next.push_str(raw_token);
                                    call_log.push(json!({
                                        "tool": ph.name,
                                        "args": ph.args,
                                        "ok": false,
                                        "error": err_msg,
                                        "duration_ms": call_start.elapsed().as_millis() as u64,
                                    }));
                                }
                            } // end policy-allowed branch
                        }
                        Err(e) => {
                            had_failure = true;
                            if strict {
                                return ActionResult::err(
                                    "RenderTemplate",
                                    ErrorKind::InvalidInput,
                                    format!(
                                        "placeholder `{}` produced invalid Action JSON: {}",
                                        ph.name, e
                                    ),
                                    start.elapsed().as_millis() as u64,
                                );
                            }
                            next.push_str(raw_token);
                            call_log.push(json!({
                                "tool": ph.name,
                                "args": ph.args,
                                "ok": false,
                                "error": format!("invalid action JSON: {e}"),
                            }));
                        }
                    }
                }
                Err(e) => {
                    had_failure = true;
                    if strict {
                        return ActionResult::err(
                            "RenderTemplate",
                            ErrorKind::InvalidInput,
                            e,
                            start.elapsed().as_millis() as u64,
                        );
                    }
                    next.push_str(raw_token);
                    call_log.push(json!({
                        "tool": ph.name,
                        "args": ph.args,
                        "ok": false,
                        "error": e,
                    }));
                }
            }
            cursor = ph.end;
        }
        // Trailing literal.
        next.push_str(&current[cursor..]);

        // Idempotence: if nothing changed (e.g. every placeholder failed
        // in non-strict mode), stop early to avoid a spurious final pass.
        if next == current && had_failure {
            current = next;
            break;
        }
        current = next;
    }

    ActionResult::ok(
        "RenderTemplate",
        ActionOutput::Generic {
            data: json!({
                "rendered": current,
                "calls": call_log,
                "passes": passes,
            }),
        },
        start.elapsed().as_millis() as u64,
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite;
    use crate::handles;

    fn make_ctx() -> (
        AppConfig,
        SharedHandles,
        PolicyEngine,
        RateLimiter,
        CompositeStore,
    ) {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);
        (config, handles, policy, rate_limiter, composites)
    }

    #[test]
    fn parse_call_form_with_string_args() {
        let phs = extract_placeholders(r#"hi {{checksum("/tmp/x","sha256")}} there"#);
        assert_eq!(phs.len(), 1);
        assert_eq!(phs[0].name, "checksum");
        assert!(!phs[0].is_json_form);
        assert_eq!(phs[0].args.len(), 2);
        assert_eq!(phs[0].args[0], json!("/tmp/x"));
        assert_eq!(phs[0].args[1], json!("sha256"));
    }

    #[test]
    fn parse_call_form_with_number_args() {
        // Numeric literals still parse the same way — exercise via a hypothetical
        // tool name; the parser doesn't care about binding existence.
        let phs = extract_placeholders("{{some_tool(2, 3)}}");
        assert_eq!(phs.len(), 1);
        assert_eq!(phs[0].args[0], json!(2));
        assert_eq!(phs[0].args[1], json!(3));
    }

    #[test]
    fn parse_zero_arg() {
        let phs = extract_placeholders("{{whoami}}");
        assert_eq!(phs.len(), 1);
        assert_eq!(phs[0].name, "whoami");
        assert!(phs[0].args.is_empty());
    }

    #[test]
    fn parse_json_form_with_nested_object() {
        let phs = extract_placeholders(r#"{{Checksum {"path":"/tmp/x","algorithm":"sha256"}}}"#);
        assert_eq!(phs.len(), 1);
        assert!(phs[0].is_json_form);
        assert_eq!(phs[0].name, "Checksum");
        assert_eq!(phs[0].args[0]["path"], json!("/tmp/x"));
    }

    #[test]
    fn parse_multiple_placeholders_in_one_template() {
        let phs =
            extract_placeholders(r#"{{checksum("/tmp/a","sha256")}} and {{file_info("/tmp/b")}}"#);
        assert_eq!(phs.len(), 2);
        assert_eq!(phs[0].name, "checksum");
        assert_eq!(phs[1].name, "file_info");
    }

    #[test]
    fn ignore_lone_braces_and_unclosed_placeholders() {
        // A single `{` or `{{` without a matching `}}` is treated as literal.
        let phs = extract_placeholders("price is {12.50} and {{not_closed");
        assert!(phs.is_empty());
    }

    #[test]
    fn placeholder_to_action_positional_string() {
        let ph = Placeholder {
            start: 0,
            end: 0,
            name: "checksum".into(),
            args: vec![json!("/tmp/x"), json!("sha256")],
            is_json_form: false,
        };
        let act = placeholder_to_action_json(&ph).unwrap();
        assert_eq!(act["type"], "Checksum");
        assert_eq!(act["path"], "/tmp/x");
        assert_eq!(act["algorithm"], "sha256");
    }

    #[test]
    fn placeholder_to_action_json_form() {
        let ph = Placeholder {
            start: 0,
            end: 0,
            name: "Checksum".into(),
            args: vec![json!({"path":"/tmp/x","algorithm":"sha256"})],
            is_json_form: true,
        };
        let act = placeholder_to_action_json(&ph).unwrap();
        assert_eq!(act["type"], "Checksum");
        assert_eq!(act["path"], "/tmp/x");
    }

    #[test]
    fn unknown_tool_with_positional_args_errors() {
        let ph = Placeholder {
            start: 0,
            end: 0,
            name: "no_such_tool".into(),
            args: vec![json!(1), json!(2)],
            is_json_form: false,
        };
        let e = placeholder_to_action_json(&ph).unwrap_err();
        assert!(e.contains("no positional binding"));
        assert!(e.contains("NoSuchTool")); // pascalized hint
    }

    #[test]
    fn unknown_tool_zero_arg_succeeds() {
        // Zero-arg falls through — `{{ no_such_tool }}` produces
        // {"type":"NoSuchTool"} and the failure surfaces at dispatch time
        // (where it gets per-placeholder logged), not here.
        let ph = Placeholder {
            start: 0,
            end: 0,
            name: "no_such_tool".into(),
            args: vec![],
            is_json_form: false,
        };
        let act = placeholder_to_action_json(&ph).unwrap();
        assert_eq!(act["type"], "NoSuchTool");
    }

    // E2E tests that used to verify str_count / add at runtime are removed
    // along with those primitives. Equivalent coverage via checksum/file_info
    // is implicit in the parser/dispatch unit tests above; full runtime e2e
    // happens through orchestrator integration tests.

    #[tokio::test]
    async fn e2e_non_strict_unknown_tool_keeps_raw_token() {
        let (cfg, handles, policy, rate, comps) = make_ctx();
        let tpl = r#"the answer is {{no_such_tool("x","y")}} or maybe not"#;
        let res = render_template(
            tpl,
            None,
            Some(false),
            &cfg,
            handles,
            &policy,
            &rate,
            &comps,
            0,
        )
        .await;
        assert!(res.success, "non-strict should always succeed");
        let data = match res.output.as_ref().unwrap() {
            ActionOutput::Generic { data } => data,
            _ => panic!(),
        };
        // Failed placeholder is preserved verbatim.
        assert!(data["rendered"]
            .as_str()
            .unwrap()
            .contains(r#"{{no_such_tool("x","y")}}"#));
        assert_eq!(data["calls"][0]["ok"], false);
    }
}
