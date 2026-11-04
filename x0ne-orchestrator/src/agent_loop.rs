//! Orchestrator action tables and validation-error enrichment.
//!
//! Named after the legacy agent loop that used to live here; the loop was
//! removed on 2026-09-23 (no caller). The canonical loop is `agent::driver`.

use serde_json::{json, Value};

/// Action types that the orchestrator handles locally (not forwarded to x0ne-runtime).
///
/// Routing asks `prompt::orchestrator_registry`, not this list. Read by
/// `action_source` (per-source `ActionStats`) and by the routing tests;
/// `dispatch::tests::the_registry_and_the_historical_list_agree` pins both equal.
pub const ORCHESTRATOR_ACTION_TYPES: &[&str] = &[
    "SpawnWorld",
    "SpawnAgent",
    "WaitAgent",
    "ListAgents",
    "BroadcastAgents",
    "GetBroadcasts",
    "AcknowledgeBroadcast",
    "GetBroadcastReceipts",
    "Plan",
    "Reflect",
    "Matrix",
    "ArtifactExpose",
    "ArtifactUnexpose",
    // Memory & budget introspection — callable by the agent itself
    // Appends prose to X0NE.md. Deliberately NOT named `Remember`: that is a
    // runtime action storing a key/value pair that `Recall` and `ListMemory`
    // read back. While both were called Remember, the orchestrator intercepted
    // the name, so the model was taught the runtime's key/value schema, got a
    // validation error demanding `note`, and anything it did store could never
    // be recalled.
    "ProjectNote",
    "Budget", // Read or cap the session spend (get/set budget_usd_limit)
    // Incus container management — privilege-boundary fix: the runtime
    // may run INSIDE the very container these would manage, so they
    // must be dispatched orchestrator-side. See crate::incus_actions.
    "RunIncus",
    "RunIncusGroup",
    "IncusCreateSnapshot",
    "IncusRestoreSnapshot",
    "IncusListSnapshots",
    "IncusDeleteSnapshot",
];

/// Returns `true` if this action type must be handled by the orchestrator
/// rather than forwarded to the x0ne-runtime bridge.
pub fn is_orchestrator_action(action_type: &str) -> bool {
    ORCHESTRATOR_ACTION_TYPES.contains(&action_type)
}

///
/// Why at the orchestrator boundary, not in x0ne-runtime: x0ne-runtime never
/// sees the discovered-action schema cache (that is a session-level
/// artefact built from `ListActions`), and the small models that need the
/// most help are exactly the ones that get the least mileage from a bare
/// "missing field `a`" string. By the time the result is on its way to
/// `add_action_result`, we have everything we need to make the next
/// retry a one-shot fix.
///
/// Behaviour:
/// - If the result is *not* a validation error, do nothing.
/// - Always set `error.sent` to the action JSON the LLM emitted.
/// - Set `error.expected_schema` when we have it cached (silently skip
///   when discovery wasn't run yet — the rest of the envelope is still
///   useful).
/// - Set `error.diff` to a one-line natural-language fix derived from
///   the message — covers the common "missing field", "unknown field",
///   "expected X, got Y" patterns. Falls back to a generic prompt when
///   the message shape is unrecognised.
pub fn enrich_validation_error(
    action: &Value,
    result: &mut Value,
    discovered_actions: Option<&[Value]>,
) {
    let class_is_validation = result
        .get("error")
        .and_then(|e| e.get("class"))
        .and_then(|c| c.as_str())
        == Some("validation_error");
    if !class_is_validation {
        return;
    }

    let action_type = action
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("?")
        .to_string();
    let message = result
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let diff = derive_validation_diff(&action_type, &message);
    let expected_schema: Option<Value> = discovered_actions
        .and_then(|acts| {
            acts.iter()
                .find(|a| a["type"].as_str() == Some(&action_type))
        })
        .and_then(|a| a.get("schema").cloned());

    let Some(error_obj) = result.get_mut("error").and_then(|e| e.as_object_mut()) else {
        return;
    };
    error_obj.insert("sent".into(), action.clone());
    error_obj.insert("diff".into(), Value::String(diff));
    if let Some(schema) = expected_schema {
        error_obj.insert("expected_schema".into(), schema);
    } else {
        // Even when discovery hasn't populated the cache, surface a
        // pointer to DescribeAction so the model can self-serve the
        // schema — never leave the field outright missing in the
        // envelope shape.
        error_obj.insert(
            "expected_schema".into(),
            json!({
                "lookup_hint": format!("Call DescribeAction {{\"action_type\":\"{action_type}\"}} for the canonical schema."),
            }),
        );
    }
}

/// Derive a single-line, prescriptive fix sentence from a core
/// `validation_error` message. Handles the three common shapes we see in
/// the field; falls back to a generic prompt otherwise.
fn derive_validation_diff(action_type: &str, message: &str) -> String {
    // "missing field `a`" — the model omitted a required field.
    if let Some(rest) = message.strip_prefix("missing field `") {
        if let Some(end) = rest.find('`') {
            let field = &rest[..end];
            return format!(
                "You did not include `{field}` at the root of the action. \
                 Fix: add `\"{field}\": <value>` to the canonical \
                 `{{\"type\":\"{action_type}\", …}}` payload (no nesting)."
            );
        }
    }
    // "unknown field `foo`" — extra field the schema does not declare.
    if let Some(rest) = message.strip_prefix("unknown field `") {
        if let Some(end) = rest.find('`') {
            let field = &rest[..end];
            return format!(
                "Field `{field}` is not part of the {action_type} schema. \
                 Fix: drop `{field}` (or, if you intended to nest other \
                 fields, flatten them — X0NE actions are flat, no `args`/`input`/`payload` wrapper)."
            );
        }
    }
    // "invalid type: string \"x\", expected … at field `a`"
    let lower = message.to_lowercase();
    if lower.contains("invalid type") {
        return format!(
            "A field has the wrong JSON type. The error reads: \"{message}\". \
             Fix: re-emit `{action_type}` with the field as the type the schema requires \
             (numbers must be JSON numbers, paths must be strings, etc.)."
        );
    }
    // Generic catch-all.
    format!(
        "{message}. Fix: re-issue `{action_type}` in canonical X0NE form \
         (`{{\"type\":\"{action_type}\", <fields at root> }}`); call \
         DescribeAction for the exact schema."
    )
}

/// (T5) Returns the `source` label for a given action type — `"orchestrator"`
/// when the orchestrator handles the action locally, `"core"` otherwise.
/// Read by `Conversation` for the per-source split of `ActionStats`.
pub fn action_source(action_type: &str) -> &'static str {
    if is_orchestrator_action(action_type) {
        "orchestrator"
    } else {
        "core"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Incus orchestrator-side tests ───────────────────────────────────────
    // Incus primitives now live in the orchestrator (privilege boundary fix);
    // is_orchestrator_action() must include them so Pipeline detection /
    // intercept dispatch route them to crate::incus_actions and never to
    // the runtime bridge.

    #[test]
    fn is_orchestrator_action_includes_all_incus_types() {
        for name in [
            "RunIncus",
            "RunIncusGroup",
            "IncusCreateSnapshot",
            "IncusRestoreSnapshot",
            "IncusListSnapshots",
            "IncusDeleteSnapshot",
        ] {
            assert!(
                is_orchestrator_action(name),
                "{name} must be orchestrator-side (privilege boundary)",
            );
        }
    }

    #[test]
    fn is_orchestrator_action_still_rejects_runtime_primitives() {
        for name in ["ReadFile", "WriteFile", "RunCommand", "Pipeline", ""] {
            assert!(!is_orchestrator_action(name), "{name} must be runtime-side");
        }
    }

    #[test]
    fn artifact_actions_are_orchestrator_actions() {
        // Regression: ArtifactExpose / ArtifactUnexpose are host-side and must
        // never be forwarded to the x0ne-runtime bridge (which knows nothing
        // about the local artifact proxy and would return a parse error like
        // "unknown variant `ArtifactExpose`").
        assert!(is_orchestrator_action("ArtifactExpose"));
        assert!(is_orchestrator_action("ArtifactUnexpose"));
    }

    /// Multi-agent and artifact actions are host-side: the live router must
    /// never send them to the runtime bridge, which answers
    /// "unknown variant `ArtifactExpose`".
    #[test]
    fn multi_agent_and_artifact_actions_never_route_to_the_bridge() {
        use crate::agent::dispatch::{route_of, Route};
        for t in [
            "SpawnWorld",
            "SpawnAgent",
            "WaitAgent",
            "ListAgents",
            "BroadcastAgents",
            "GetBroadcasts",
            "AcknowledgeBroadcast",
            "GetBroadcastReceipts",
            "ArtifactExpose",
            "ArtifactUnexpose",
        ] {
            assert_eq!(
                route_of(t),
                Route::Orchestrator,
                "{t} would reach the bridge"
            );
        }
        assert_eq!(route_of("ReadFile"), Route::Runtime);
        assert_eq!(route_of("RunCommand"), Route::Runtime);
    }

    #[test]
    fn pipeline_with_incus_triggers_orchestrator_dispatch() {
        // A Pipeline containing RunIncus should trigger orchestrator-side
        // dispatch (has_sysaicall = true) so the incus bridge gets used.
        let steps = [
            json!({"type": "ReadFile", "path": "/tmp/test"}),
            json!({"type": "RunIncus", "image": "images:alpine/edge", "command": ["echo", "hello"]}),
        ];
        let has_sysaicall = steps.iter().any(|s| {
            let st = s["type"].as_str().unwrap_or("");
            is_orchestrator_action(st)
        });
        assert!(
            has_sysaicall,
            "Pipeline with RunIncus must trigger orchestrator dispatch"
        );
    }

    #[test]
    fn pipeline_without_special_actions_stays_in_core() {
        // Pure-system Pipelines should NOT trigger orchestrator dispatch.
        let steps = [
            json!({"type": "ReadFile", "path": "/tmp/test"}),
            json!({"type": "WriteFile", "path": "/tmp/out", "content": "ok"}),
        ];
        let has_sysaicall = steps.iter().any(|s| {
            let st = s["type"].as_str().unwrap_or("");
            is_orchestrator_action(st)
        });
        assert!(
            !has_sysaicall,
            "Pure-system Pipeline must go to x0ne-runtime directly"
        );
    }

    /// (T5) `action_source` feeds the per-source split in `ActionStats`.
    #[test]
    fn action_source_tags_orchestrator_actions() {
        for orch in [
            "SpawnWorld",
            "SpawnAgent",
            "WaitAgent",
            "ListAgents",
            "BroadcastAgents",
            "GetBroadcasts",
            "AcknowledgeBroadcast",
            "GetBroadcastReceipts",
            "Plan",
            "Reflect",
            "Matrix",
        ] {
            assert_eq!(action_source(orch), "orchestrator", "{orch}");
        }
    }

    #[test]
    fn action_source_tags_everything_else_as_core() {
        for core_action in [
            "ReadFile",
            "WriteFile",
            "ListDir",
            "RunCommand",
            "Pipeline",
            "ParallelGroup",
            "SystemInfo",
            "Add",
            "?",
        ] {
            assert_eq!(action_source(core_action), "core", "{core_action}");
        }
    }
}
