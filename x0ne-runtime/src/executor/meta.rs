use serde_json::{json, Value};

use crate::config::AppConfig;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionOutput, ActionResult, ErrorKind, PolicyVerdict};

// ── Action registry ───────────────────────────────────────────────────────────

// ── Action registry ───────────────────────────────────────────────────────────
//
// Derived from `x0ne_actions::GENERATED_ACTIONS`, which the shared crate emits
// from the `Action` enum at build time. It used to be a 129-entry hand-written
// table, and by the time it was replaced it named 54 actions that had been
// removed and was missing 8 that existed — including `Simulate`, the primitive
// whose whole purpose is previewing a risky plan safely. Nothing failed: the
// table simply stopped describing the runtime, and every consumer downstream
// inherited that.
//
// The descriptions are the variants' own doc comments and the schemas are
// their own field declarations, so the catalog cannot describe an action the
// deserializer would reject, nor omit one it would accept.

/// Render one generated action as the discovery envelope callers expect.
fn native_action_json(meta: &x0ne_actions::ActionMeta) -> Value {
    let mut properties = serde_json::Map::new();
    for (field, json_type) in meta.field_types {
        properties.insert((*field).to_string(), json!({ "type": json_type }));
    }
    json!({
        "type": meta.name,
        "description": meta.summary,
        "category": meta.class,
        // Anything that is not a pure read or pure introspection changes
        // something a caller may care about.
        "mutable": !matches!(meta.class, "ReadOnly" | "Meta"),
        "schema": {
            "type": "object",
            "required": meta.required_fields,
            "properties": Value::Object(properties),
        },
        "source": "native",
    })
}

// ── Action handlers ───────────────────────────────────────────────────────────

pub async fn list_actions(
    _config: &AppConfig,
    composites: &crate::composite::CompositeStore,
) -> ActionResult {
    let mut actions: Vec<Value> = x0ne_actions::GENERATED_ACTIONS
        .iter()
        .map(native_action_json)
        .collect();

    // Append custom composite primitives
    for prim in composites.list().await {
        let params_schema: Value = prim
            .params
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    json!({"type": v.param_type, "required": v.required}),
                )
            })
            .collect::<serde_json::Map<String, Value>>()
            .into();

        actions.push(json!({
            "type": prim.name,
            "description": prim.description.unwrap_or_default(),
            "category": "composite",
            "mutable": true,
            "schema": {"type": "object", "properties": params_schema},
            "source": "custom",
            "step_count": prim.steps.len(),
        }));
    }

    ActionResult::ok("ListActions", ActionOutput::ActionList { actions }, 0)
}

pub async fn describe_action(
    action_type: &str,
    _config: &AppConfig,
    composites: &crate::composite::CompositeStore,
) -> ActionResult {
    // Native vocabulary first.
    if let Some(m) = x0ne_actions::action_meta(action_type) {
        let rendered = native_action_json(m);
        return ActionResult::ok(
            "DescribeAction",
            ActionOutput::ActionDescription {
                action_type: m.name.to_string(),
                description: m.summary.to_string(),
                category: m.class.to_string(),
                mutable: !matches!(m.class, "ReadOnly" | "Meta"),
                schema: rendered["schema"].clone(),
                // Examples were hand-written per action and drifted with the
                // rest of the table. A caller that needs the exact shape has
                // the field list and their types above.
                examples: Vec::new(),
            },
            0,
        );
    }

    // Check composite store
    if let Some(prim) = composites.get(action_type).await {
        let params_schema: Value = prim
            .params
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    json!({
                        "type": v.param_type,
                        "required": v.required,
                        "default": v.default,
                    }),
                )
            })
            .collect::<serde_json::Map<String, Value>>()
            .into();

        let step_summary: Vec<Value> = prim
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                json!({
                    "step": i,
                    "type": s["type"],
                })
            })
            .collect();

        return ActionResult::ok(
            "DescribeAction",
            ActionOutput::ActionDescription {
                action_type: prim.name.clone(),
                description: prim.description.unwrap_or_default(),
                category: "composite".to_string(),
                mutable: true,
                schema: json!({
                    "type": "object",
                    "properties": params_schema,
                    "steps": step_summary,
                    "source": "custom",
                    "ephemeral": prim.ephemeral,
                    "created_at": prim.created_at,
                }),
                examples: vec![json!({"type": "CallPrimitive", "name": prim.name, "args": {}})],
            },
            0,
        );
    }

    ActionResult::err(
        "DescribeAction",
        ErrorKind::NotFound,
        format!("Unknown action type '{}'.", action_type),
        0,
    )
}

pub fn policy_check(
    raw_action: &Value,
    policy: &PolicyEngine,
    _config: &AppConfig,
) -> ActionResult {
    let action: crate::action::Action = match serde_json::from_value(raw_action.clone()) {
        Ok(a) => a,
        Err(e) => {
            return ActionResult::err(
                "PolicyCheck",
                ErrorKind::InvalidInput,
                format!("Cannot parse action: {}", e),
                0,
            );
        }
    };
    let action_type = action.type_name().to_string();
    // SECURITY: the three arms must map onto three distinct `verdict` values.
    // Before 2026-08-16 they mapped onto two — `allowed: false` for both
    // refusals — and the only thing separating a `Deny` from a
    // `RequireConfirmation` was the prefix of a human-readable sentence. The
    // orchestrator's preflight refuses before prompting on `Deny` and on
    // `Confirm` alike; collapsing either back onto `Allow` would send an
    // action the runtime will refuse through the human gate first, which is
    // the defect this field exists to close.
    match policy.evaluate(&action) {
        PolicyEffect::Allow => ActionResult::ok(
            "PolicyCheck",
            ActionOutput::PolicyCheckResult {
                allowed: true,
                verdict: PolicyVerdict::Allow,
                action_type,
                reason: None,
                liftable: None,
            },
            0,
        ),
        PolicyEffect::RequireConfirmation(msg) => ActionResult::ok(
            "PolicyCheck",
            ActionOutput::PolicyCheckResult {
                allowed: false,
                verdict: PolicyVerdict::Confirm,
                action_type,
                reason: Some(format!("Requires confirmation: {}", msg)),
                // A `Confirm` is a refusal by a runtime with nobody to ask
                // (`result.rs`), not a widenable list. Nothing to grant.
                liftable: None,
            },
            0,
        ),
        PolicyEffect::Deny(reason) => {
            // SECURITY: the exhaustive match decides, here as everywhere else.
            let liftable = reason.liftable(&action_type);
            ActionResult::ok(
                "PolicyCheck",
                ActionOutput::PolicyCheckResult {
                    allowed: false,
                    verdict: PolicyVerdict::Deny,
                    action_type,
                    reason: Some(reason.message()),
                    liftable,
                },
                0,
            )
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::CompositeStore;
    use crate::config::AppConfig;
    use crate::policy::PolicyEngine;

    fn make_engine(cfg: &AppConfig) -> PolicyEngine {
        PolicyEngine::new(cfg.clone(), cfg.actions.clone())
    }

    #[tokio::test]
    async fn list_actions_returns_all_known_types() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = list_actions(&cfg, &composites).await;
        assert!(result.success);
        if let Some(ActionOutput::ActionList { actions }) = result.output {
            // Must contain at least the known types
            let types: Vec<&str> = actions
                .iter()
                .filter_map(|a| a.get("type").and_then(|v| v.as_str()))
                .collect();
            assert!(types.contains(&"ReadFile"), "ReadFile missing");
            assert!(types.contains(&"Pipeline"), "Pipeline missing");
            assert!(types.contains(&"If"), "If missing");
            assert!(types.contains(&"Screenshot"), "Screenshot missing");
            assert!(types.contains(&"CheckHandle"), "CheckHandle missing");
            assert!(
                types.contains(&"DefinePrimitive"),
                "DefinePrimitive missing"
            );
            assert!(types.contains(&"CallPrimitive"), "CallPrimitive missing");
            assert!(types.contains(&"ArtifactCreate"), "ArtifactCreate missing");
            assert!(types.contains(&"ArtifactList"), "ArtifactList missing");
            // Registry should be non-trivially large
            assert!(
                types.len() >= 30,
                "Expected at least 30 action types, got {}",
                types.len()
            );
            // All native actions have source: native
            for a in &actions {
                if a.get("source").and_then(|s| s.as_str()) == Some("native") {
                    assert!(a.get("type").is_some());
                }
            }
        } else {
            panic!("unexpected output");
        }
    }

    #[tokio::test]
    async fn describe_action_read_file() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = describe_action("ReadFile", &cfg, &composites).await;
        assert!(result.success);
        if let Some(ActionOutput::ActionDescription {
            action_type,
            category,
            mutable,
            examples,
            ..
        }) = result.output
        {
            assert_eq!(action_type, "ReadFile");
            // Category is now the side-effect class, the same taxonomy every
            // security decision uses, rather than a second hand-assigned one.
            assert_eq!(category, "ReadOnly");
            assert!(!mutable);
            // Examples were hand-written per action and drifted with the rest
            // of the old table; the field list and its types stand in.
            assert!(examples.is_empty());
        } else {
            panic!("unexpected output");
        }
    }

    #[tokio::test]
    async fn describe_action_artifact_create() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = describe_action("ArtifactCreate", &cfg, &composites).await;
        assert!(result.success, "{:?}", result.error);
        if let Some(ActionOutput::ActionDescription { action_type, .. }) = result.output {
            assert_eq!(action_type, "ArtifactCreate");
        } else {
            panic!("unexpected output");
        }
    }

    /// ANCHOR: everything discovery advertises must be an action the runtime
    /// actually accepts. The old registry advertised 54 removed actions, so a
    /// model that trusted discovery emitted things the deserializer rejected.
    #[tokio::test]
    async fn every_advertised_action_exists() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = list_actions(&cfg, &composites).await;
        let actions = match result.output {
            Some(ActionOutput::ActionList { actions }) => actions,
            _ => panic!("unexpected output"),
        };
        for a in &actions {
            if a.get("source").and_then(|s| s.as_str()) != Some("native") {
                continue; // composites are user-defined, not vocabulary
            }
            let name = a["type"].as_str().expect("every entry names a type");
            assert!(
                x0ne_actions::all_type_names().contains(&name),
                "discovery advertises '{name}', which is not a runtime action"
            );
        }
    }

    /// ANCHOR: and the converse — discovery must not omit real actions. The
    /// old registry was missing 8, including `Simulate`, so the safest
    /// primitive in the system was invisible to anything that discovered its
    /// capabilities.
    #[tokio::test]
    async fn list_actions_covers_the_whole_vocabulary() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = list_actions(&cfg, &composites).await;
        let actions = match result.output {
            Some(ActionOutput::ActionList { actions }) => actions,
            _ => panic!("unexpected output"),
        };
        let advertised: Vec<&str> = actions
            .iter()
            .filter(|a| a.get("source").and_then(|s| s.as_str()) == Some("native"))
            .filter_map(|a| a["type"].as_str())
            .collect();
        for name in x0ne_actions::all_type_names() {
            assert!(
                advertised.contains(name),
                "'{name}' is a runtime action but discovery does not advertise it"
            );
        }
    }

    /// Discovery has to describe itself, or a caller cannot bootstrap.
    #[tokio::test]
    async fn discovery_actions_are_themselves_discoverable() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        for name in ["ListActions", "DescribeAction"] {
            let r = describe_action(name, &cfg, &composites).await;
            assert!(r.success, "{name} is not describable: {:?}", r.error);
        }
    }

    #[tokio::test]
    async fn describe_action_unknown_errors() {
        let cfg = AppConfig::defaults();
        let composites = CompositeStore::new(&cfg);
        let result = describe_action("NonExistentAction", &cfg, &composites).await;
        assert!(!result.success);
        assert!(matches!(result.error.unwrap().kind, ErrorKind::NotFound));
    }

    #[test]
    fn policy_check_allowed_action() {
        let mut cfg = AppConfig::defaults();
        // /etc/hostname is the fixture; grant it explicitly now that an empty
        // allowlist means deny-all.
        cfg.policy.allowed_read_paths = vec!["/etc".into()];
        let policy = make_engine(&cfg);
        let raw = json!({"type": "ReadFile", "path": "/etc/hostname"});
        let result = policy_check(&raw, &policy, &cfg);
        assert!(result.success);
        if let Some(ActionOutput::PolicyCheckResult {
            allowed, reason, ..
        }) = result.output
        {
            assert!(allowed, "reason: {:?}", reason);
        }
    }

    #[test]
    fn policy_check_denied_action() {
        let cfg = AppConfig::defaults(); // WriteFile disabled by default
        let policy = make_engine(&cfg);
        let raw = json!({"type": "WriteFile", "path": "/tmp/test", "content": "x"});
        let result = policy_check(&raw, &policy, &cfg);
        assert!(result.success); // PolicyCheck itself always succeeds
        if let Some(ActionOutput::PolicyCheckResult {
            allowed, reason, ..
        }) = result.output
        {
            assert!(!allowed);
            assert!(reason.is_some());
        }
    }

    #[test]
    fn policy_check_invalid_json_errors() {
        let cfg = AppConfig::defaults();
        let policy = make_engine(&cfg);
        let raw = json!({"bad": "payload"});
        let result = policy_check(&raw, &policy, &cfg);
        assert!(!result.success);
        assert!(matches!(
            result.error.unwrap().kind,
            ErrorKind::InvalidInput
        ));
    }

    // ── The tri-state verdict (P8a, 2026-08-16) ──────────────────────────

    /// SECURITY: `Deny` and `RequireConfirmation` leave this process as two
    /// different words.
    ///
    /// They used to leave it as one — `allowed: false` — with only the prose
    /// of `reason` telling them apart, by the prefix "Requires confirmation:".
    /// The orchestrator's preflight refuses before prompting on both, so the
    /// distinction is not cosmetic: it is the difference between telling an
    /// operator which switch is off and telling him the runtime wants a
    /// confirmation it can never accept.
    ///
    /// A failure means the two have collapsed again, and the only thing
    /// separating a refusal from a request is the wording of a sentence.
    #[test]
    fn the_three_policy_effects_leave_the_runtime_as_three_verdicts() {
        // Allow: a read inside the granted root.
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_read_paths = vec!["/etc".into()];
        let out = verdict_for(&cfg, json!({"type": "ReadFile", "path": "/etc/hostname"}));
        assert_eq!(out.0, PolicyVerdict::Allow);
        assert!(out.1, "an allow verdict must carry allowed = true");

        // Deny: a command absent from an empty allowlist.
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "auto".into();
        cfg.policy.allowed_commands = vec![];
        cfg.actions.insert("RunCommand".into(), true);
        let out = verdict_for(
            &cfg,
            json!({"type": "RunCommand", "command": "cargo", "args": ["build"]}),
        );
        assert_eq!(out.0, PolicyVerdict::Deny);
        assert!(!out.1);
        assert!(
            out.2.unwrap_or_default().contains("allowed_commands"),
            "the reason must name the list"
        );

        // Confirm: policy.mode = "confirm" over a modifying action.
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "confirm".into();
        cfg.policy.allowed_commands = vec!["cargo".into()];
        cfg.actions.insert("RunCommand".into(), true);
        let out = verdict_for(
            &cfg,
            json!({"type": "RunCommand", "command": "cargo", "args": ["build"]}),
        );
        assert_eq!(
            out.0,
            PolicyVerdict::Confirm,
            "mode = confirm must be distinguishable from a deny"
        );
        assert!(!out.1, "a confirm is not permission either");
    }

    /// The verdict is what the wire carries, spelled the way the orchestrator
    /// reads it. A failure means the field is present in Rust and absent — or
    /// differently spelled — in the JSON the bridge actually delivers.
    #[test]
    fn the_verdict_reaches_the_wire_as_a_lowercase_word() {
        // `mode = "auto"` explicitly: the compiled default is `"confirm"`,
        // which would answer `confirm` for every write and never reach the
        // per-action switch this case is about.
        let mut cfg = AppConfig::defaults(); // WriteFile disabled by default
        cfg.policy.mode = "auto".into();
        let policy = make_engine(&cfg);
        let result = policy_check(
            &json!({"type": "WriteFile", "path": "/tmp/x", "content": "x"}),
            &policy,
            &cfg,
        );
        let wire: Value = serde_json::to_value(&result).expect("serializable");
        assert_eq!(wire["output"]["verdict"], "deny");
        assert_eq!(wire["output"]["allowed"], false);
        assert_eq!(wire["output"]["kind"], "policy_check_result");
    }

    /// A composite is worth what it contains, at the preflight too: the
    /// verdict for a Pipeline is the verdict of its first refused step, and it
    /// names that step.
    ///
    /// This is what lets the orchestrator preflight a composite by asking
    /// about the root alone. A failure means it would have to walk the tree
    /// itself — a second implementation of a rule that already exists here,
    /// which is how the two drift apart.
    #[test]
    fn a_pipeline_verdict_is_the_verdict_of_its_worst_step() {
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "auto".into();
        cfg.policy.allowed_read_paths = vec!["/etc".into()];
        cfg.policy.allowed_commands = vec![];
        cfg.actions.insert("RunCommand".into(), true);
        let out = verdict_for(
            &cfg,
            json!({"type": "Pipeline", "steps": [
                {"type": "ReadFile", "path": "/etc/hostname"},
                {"type": "RunCommand", "command": "cargo", "args": ["build"]},
            ]}),
        );
        assert_eq!(out.0, PolicyVerdict::Deny);
        let reason = out.2.unwrap_or_default();
        assert!(reason.contains("step 1"), "must name the child: {reason}");
        assert!(reason.contains("RunCommand"), "{reason}");
    }

    /// The verdict, the `allowed` flag and the reason, for one action under
    /// one config.
    fn verdict_for(cfg: &AppConfig, action: Value) -> (PolicyVerdict, bool, Option<String>) {
        let policy = make_engine(cfg);
        let result = policy_check(&action, &policy, cfg);
        assert!(result.success, "PolicyCheck itself always succeeds");
        match result.output {
            Some(ActionOutput::PolicyCheckResult {
                verdict,
                allowed,
                reason,
                ..
            }) => (verdict, allowed, reason),
            other => panic!("expected a PolicyCheckResult, got {other:?}"),
        }
    }
}
