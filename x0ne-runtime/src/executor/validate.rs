//! Validation & Diff executor — structured comparison and validation primitives.

use serde_json::{json, Value};

use crate::result::{ActionOutput, ActionResult, ErrorKind};

// ── Diff ──────────────────────────────────────────────────────────────────────

/// A single line change in a diff.
#[derive(Debug)]
enum DiffOp {
    Equal(usize, String),  // line_number, content
    Add(usize, String),    // line_number (in b), content
    Remove(usize, String), // line_number (in a), content
}

/// Structured diff between two texts.
pub async fn diff(
    a: &str,
    b: &str,
    files: bool,
    context_lines: Option<usize>,
    config: &crate::config::AppConfig,
) -> ActionResult {
    let ctx = context_lines.unwrap_or(3);

    // `files: true` turns Diff into a filesystem READ of two agent-supplied
    // paths, so it must be confined like every other read primitive. It was
    // previously unconfined at BOTH layers (absent from the policy engine's
    // path extraction, and reading the raw string here).
    let read_confined = |p: &str| -> Result<String, ActionResult> {
        let c = crate::executor::confine::resolve_read(p, &config.policy)
            .map_err(|e| ActionResult::err("Diff", e.kind(), e.message(), 0))?;
        // SECURITY: refused, not truncated. Two truncated files produce a diff
        // that is *wrong* rather than short — every line past the cut reads as
        // deleted — and the model has no way to tell. Checksum refuses on the
        // same key for the same reason (`system.rs`). Measured before this
        // check: 8 458 313 bytes returned under a 65 536 policy, plus an O(N·M)
        // LCS over both files, which is the CPU half of the same defect.
        let meta = std::fs::metadata(c.as_path())
            .map_err(|e| ActionResult::err("Diff", ErrorKind::NotFound, e.to_string(), 0))?;
        if meta.len() > config.policy.max_read_bytes {
            return Err(ActionResult::err(
                "Diff",
                ErrorKind::InvalidInput,
                format!(
                    "File too large to diff: '{}' is {} bytes, max_read_bytes is {}. \
                     Diff a narrower slice (ReadFile with start_line/limit_lines) \
                     or raise the policy key.",
                    p,
                    meta.len(),
                    config.policy.max_read_bytes
                ),
                0,
            ));
        }
        let bytes = c
            .read_bytes()
            .map_err(|e| ActionResult::err("Diff", e.kind(), e.message(), 0))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };

    let (text_a, label_a) = if files {
        match read_confined(a) {
            Ok(c) => (c, a.to_string()),
            Err(res) => return res,
        }
    } else {
        (a.to_string(), "a".to_string())
    };

    let (text_b, label_b) = if files {
        match read_confined(b) {
            Ok(c) => (c, b.to_string()),
            Err(res) => return res,
        }
    } else {
        (b.to_string(), "b".to_string())
    };

    let lines_a: Vec<&str> = text_a.lines().collect();
    let lines_b: Vec<&str> = text_b.lines().collect();

    // Simple LCS-based diff
    let ops = compute_diff(&lines_a, &lines_b);

    // Build hunks with context
    let hunks = build_hunks(&ops, ctx);

    let additions = ops.iter().filter(|o| matches!(o, DiffOp::Add(..))).count();
    let deletions = ops
        .iter()
        .filter(|o| matches!(o, DiffOp::Remove(..)))
        .count();
    let identical = additions == 0 && deletions == 0;

    // Format structured output
    let hunk_values: Vec<Value> = hunks
        .iter()
        .map(|hunk| {
            let lines: Vec<Value> = hunk
                .iter()
                .map(|op| match op {
                    DiffOp::Equal(n, s) => json!({"op": " ", "line": n, "content": s}),
                    DiffOp::Add(n, s) => json!({"op": "+", "line": n, "content": s}),
                    DiffOp::Remove(n, s) => json!({"op": "-", "line": n, "content": s}),
                })
                .collect();
            json!({"lines": lines})
        })
        .collect();

    ActionResult::ok(
        "Diff",
        ActionOutput::Generic {
            data: json!({
                "identical": identical,
                "additions": additions,
                "deletions": deletions,
                "lines_a": lines_a.len(),
                "lines_b": lines_b.len(),
                "label_a": label_a,
                "label_b": label_b,
                "hunks": hunk_values,
            }),
        },
        0,
    )
}

/// Compute line-level diff using patience-like O(NM) algorithm.
fn compute_diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp> {
    let n = a.len();
    let m = b.len();

    // LCS table
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }

    // Traceback
    let mut ops = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            ops.push(DiffOp::Equal(i, a[i - 1].to_string()));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            ops.push(DiffOp::Add(j, b[j - 1].to_string()));
            j -= 1;
        } else {
            ops.push(DiffOp::Remove(i, a[i - 1].to_string()));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Group diff ops into hunks with context lines.
fn build_hunks(ops: &[DiffOp], context: usize) -> Vec<Vec<&DiffOp>> {
    if ops.is_empty() {
        return vec![];
    }

    // Find indices of changes
    let change_indices: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, DiffOp::Equal(..)))
        .map(|(i, _)| i)
        .collect();

    if change_indices.is_empty() {
        return vec![];
    }

    let mut hunks: Vec<Vec<&DiffOp>> = Vec::new();
    let mut current_hunk: Vec<&DiffOp> = Vec::new();
    let mut hunk_end: usize = 0;

    for &ci in &change_indices {
        let start = ci.saturating_sub(context);
        let end = (ci + context + 1).min(ops.len());

        if !current_hunk.is_empty() && start > hunk_end {
            hunks.push(current_hunk);
            current_hunk = Vec::new();
        }

        let actual_start = if current_hunk.is_empty() {
            start
        } else {
            hunk_end
        };
        for idx in actual_start..end {
            current_hunk.push(&ops[idx]);
        }
        hunk_end = end;
    }

    if !current_hunk.is_empty() {
        hunks.push(current_hunk);
    }
    hunks
}

// ── Validate ──────────────────────────────────────────────────────────────────

/// Structured validation against various check modes.
pub fn validate(value: &Value, check: &str, expect: Option<&Value>) -> ActionResult {
    match check {
        "type" => validate_type(value, expect),
        "regex" => validate_regex(value, expect),
        "not_empty" => validate_not_empty(value),
        "range" => validate_range(value, expect),
        "one_of" => validate_one_of(value, expect),
        "schema" => validate_schema(value, expect),
        other => ActionResult::err(
            "Validate",
            ErrorKind::InvalidInput,
            format!(
                "Unknown check mode '{}'. Valid: type, regex, not_empty, range, one_of, schema",
                other
            ),
            0,
        ),
    }
}

fn validate_type(value: &Value, expect: Option<&Value>) -> ActionResult {
    let expected_type = expect.and_then(|e| e.as_str()).unwrap_or("string");

    let actual_type = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };

    let valid = actual_type == expected_type;
    ActionResult::ok(
        "Validate",
        ActionOutput::Generic {
            data: json!({
                "valid": valid,
                "check": "type",
                "expected": expected_type,
                "actual": actual_type,
            }),
        },
        0,
    )
}

fn validate_regex(value: &Value, expect: Option<&Value>) -> ActionResult {
    let text = match value.as_str() {
        Some(s) => s,
        None => {
            return ActionResult::ok(
                "Validate",
                ActionOutput::Generic {
                    data: json!({
                        "valid": false,
                        "check": "regex",
                        "error": "value must be a string for regex check",
                    }),
                },
                0,
            );
        }
    };

    let pattern = match expect.and_then(|e| e.as_str()) {
        Some(p) => p,
        None => {
            return ActionResult::err(
                "Validate",
                ErrorKind::InvalidInput,
                "regex check requires 'expect' to be a pattern string",
                0,
            );
        }
    };

    match regex::Regex::new(pattern) {
        Ok(re) => {
            let valid = re.is_match(text);
            let captures: Vec<String> = if valid {
                re.find_iter(text).map(|m| m.as_str().to_string()).collect()
            } else {
                vec![]
            };
            ActionResult::ok(
                "Validate",
                ActionOutput::Generic {
                    data: json!({
                        "valid": valid,
                        "check": "regex",
                        "pattern": pattern,
                        "matches": captures,
                    }),
                },
                0,
            )
        }
        Err(e) => ActionResult::err(
            "Validate",
            ErrorKind::InvalidInput,
            format!("invalid regex: {}", e),
            0,
        ),
    }
}

fn validate_not_empty(value: &Value) -> ActionResult {
    let valid = match value {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true, // numbers and bools are never "empty"
    };
    ActionResult::ok(
        "Validate",
        ActionOutput::Generic {
            data: json!({
                "valid": valid,
                "check": "not_empty",
            }),
        },
        0,
    )
}

fn validate_range(value: &Value, expect: Option<&Value>) -> ActionResult {
    let num = match value.as_f64() {
        Some(n) => n,
        None => {
            return ActionResult::ok(
                "Validate",
                ActionOutput::Generic {
                    data: json!({
                        "valid": false,
                        "check": "range",
                        "error": "value must be a number for range check",
                    }),
                },
                0,
            );
        }
    };

    let expect = match expect {
        Some(e) => e,
        None => {
            return ActionResult::err(
                "Validate",
                ErrorKind::InvalidInput,
                "range check requires 'expect' with min/max",
                0,
            )
        }
    };

    let min = expect.get("min").and_then(|v| v.as_f64());
    let max = expect.get("max").and_then(|v| v.as_f64());

    let valid = min.map(|m| num >= m).unwrap_or(true) && max.map(|m| num <= m).unwrap_or(true);
    ActionResult::ok(
        "Validate",
        ActionOutput::Generic {
            data: json!({
                "valid": valid,
                "check": "range",
                "value": num,
                "min": min,
                "max": max,
            }),
        },
        0,
    )
}

fn validate_one_of(value: &Value, expect: Option<&Value>) -> ActionResult {
    let allowed = match expect.and_then(|e| e.as_array()) {
        Some(a) => a,
        None => {
            return ActionResult::err(
                "Validate",
                ErrorKind::InvalidInput,
                "one_of check requires 'expect' to be an array",
                0,
            )
        }
    };

    let valid = allowed.contains(value);
    ActionResult::ok(
        "Validate",
        ActionOutput::Generic {
            data: json!({
                "valid": valid,
                "check": "one_of",
                "value": value,
                "allowed": allowed,
            }),
        },
        0,
    )
}

fn validate_schema(value: &Value, expect: Option<&Value>) -> ActionResult {
    let schema = match expect {
        Some(s) => s,
        None => {
            return ActionResult::err(
                "Validate",
                ErrorKind::InvalidInput,
                "schema check requires 'expect' to be a JSON Schema",
                0,
            )
        }
    };

    // Lightweight schema validation: check type, required fields, properties
    let mut errors: Vec<String> = Vec::new();

    // Check "type" constraint
    if let Some(expected_type) = schema.get("type").and_then(|t| t.as_str()) {
        let actual_type = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if actual_type != expected_type {
            errors.push(format!(
                "expected type '{}', got '{}'",
                expected_type, actual_type
            ));
        }
    }

    // Check "required" fields
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        if let Some(obj) = value.as_object() {
            for req in required {
                if let Some(field) = req.as_str() {
                    if !obj.contains_key(field) {
                        errors.push(format!("missing required field '{}'", field));
                    }
                }
            }
        }
    }

    // Check "properties" — validate nested types
    if let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(|p| p.as_object()),
        value.as_object(),
    ) {
        for (key, prop_schema) in props {
            if let Some(field_val) = obj.get(key) {
                if let Some(prop_type) = prop_schema.get("type").and_then(|t| t.as_str()) {
                    let actual = match field_val {
                        Value::Null => "null",
                        Value::Bool(_) => "boolean",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    };
                    if actual != prop_type {
                        errors.push(format!(
                            "field '{}': expected '{}', got '{}'",
                            key, prop_type, actual
                        ));
                    }
                }
            }
        }
    }

    // Check "minLength" / "maxLength" for strings
    if let Some(s) = value.as_str() {
        if let Some(min) = schema.get("minLength").and_then(|v| v.as_u64()) {
            if (s.len() as u64) < min {
                errors.push(format!("string too short: {} < {}", s.len(), min));
            }
        }
        if let Some(max) = schema.get("maxLength").and_then(|v| v.as_u64()) {
            if (s.len() as u64) > max {
                errors.push(format!("string too long: {} > {}", s.len(), max));
            }
        }
    }

    let valid = errors.is_empty();
    ActionResult::ok(
        "Validate",
        ActionOutput::Generic {
            data: json!({
                "valid": valid,
                "check": "schema",
                "errors": errors,
                "error_count": errors.len(),
            }),
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_type_string() {
        let r = validate(&json!("hello"), "type", Some(&json!("string")));
        assert!(r.success);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
        }
    }

    #[test]
    fn validate_type_mismatch() {
        let r = validate(&json!(42), "type", Some(&json!("string")));
        assert!(r.success);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
            assert_eq!(data["actual"], "number");
        }
    }

    #[test]
    fn validate_regex_match() {
        let r = validate(&json!("hello123"), "regex", Some(&json!("^[a-z]+\\d+$")));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
        }
    }

    #[test]
    fn validate_regex_no_match() {
        let r = validate(&json!("HELLO"), "regex", Some(&json!("^[a-z]+$")));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
        }
    }

    #[test]
    fn validate_not_empty_string() {
        let r = validate(&json!("hello"), "not_empty", None);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
        }
    }

    #[test]
    fn validate_not_empty_empty_string() {
        let r = validate(&json!(""), "not_empty", None);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
        }
    }

    #[test]
    fn validate_range_in() {
        let r = validate(&json!(50), "range", Some(&json!({"min": 0, "max": 100})));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
        }
    }

    #[test]
    fn validate_range_out() {
        let r = validate(&json!(150), "range", Some(&json!({"min": 0, "max": 100})));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
        }
    }

    #[test]
    fn validate_one_of_valid() {
        let r = validate(
            &json!("red"),
            "one_of",
            Some(&json!(["red", "green", "blue"])),
        );
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
        }
    }

    #[test]
    fn validate_one_of_invalid() {
        let r = validate(
            &json!("purple"),
            "one_of",
            Some(&json!(["red", "green", "blue"])),
        );
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
        }
    }

    #[test]
    fn validate_schema_basic() {
        let schema = json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "number"},
            }
        });
        let value = json!({"name": "Alice", "age": 30});
        let r = validate(&value, "schema", Some(&schema));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], true);
            assert_eq!(data["error_count"], 0);
        }
    }

    #[test]
    fn validate_schema_missing_field() {
        let schema = json!({
            "type": "object",
            "required": ["name", "email"],
        });
        let value = json!({"name": "Bob"});
        let r = validate(&value, "schema", Some(&schema));
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["valid"], false);
        }
    }

    #[tokio::test]
    async fn diff_identical_strings() {
        let r = diff(
            "hello\nworld",
            "hello\nworld",
            false,
            None,
            &crate::config::AppConfig::defaults(),
        )
        .await;
        assert!(r.success);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["identical"], true);
            assert_eq!(data["additions"], 0);
            assert_eq!(data["deletions"], 0);
        }
    }

    #[tokio::test]
    async fn diff_with_changes() {
        let r = diff(
            "line1\nline2\nline3",
            "line1\nmodified\nline3",
            false,
            None,
            &crate::config::AppConfig::defaults(),
        )
        .await;
        assert!(r.success);
        if let Some(ActionOutput::Generic { data }) = &r.output {
            assert_eq!(data["identical"], false);
            assert!(
                data["additions"].as_u64().unwrap() > 0 || data["deletions"].as_u64().unwrap() > 0
            );
        }
    }
}
