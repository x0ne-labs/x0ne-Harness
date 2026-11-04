//! Capability discovery, and the narrowest door it needs.
//!
//! `ListActions` and `DescribeAction` are the only orchestrator-side
//! operations that must ask the runtime a question. Everything else the
//! orchestrator handles, it handles alone.
//!
//! The reason they are special is that the runtime's catalog is **live**.
//! `x0ne-runtime/src/executor/meta.rs:47` answers `ListActions` from
//! `GENERATED_ACTIONS` *plus* the user's own composite primitives, which are
//! held in a `CompositeStore` the orchestrator cannot see. Rebuilding the
//! runtime half from `x0ne_actions` alone would compile, pass, and quietly
//! stop telling the model about every primitive the user defined — a
//! capability disappearing with nothing failing.
//!
//! So the handler gets a bridge dependency. The question is how to grant it
//! without granting execution, and the answer here is shape rather than
//! discipline: [`CatalogQuery`] has two methods, neither takes an action, and
//! the only two action values [`BridgeCatalog`] can construct are written as
//! literals in this file. There is no parameter through which a caller could
//! smuggle a third. A handler holding a `&mut dyn CatalogQuery` cannot reach
//! the bridge for anything else, because the trait offers nothing else.

use serde_json::{json, Value};

use crate::bridge::Bridge;

/// Read-only capability discovery against the runtime.
///
/// Deliberately not `send_action`. The two methods below are the entire
/// surface, and both are `Meta`-class reads with no side effect.
#[async_trait::async_trait]
pub trait CatalogQuery: Send {
    /// The runtime's live action catalog: generated vocabulary plus composite
    /// primitives. `None` when the runtime is unreachable.
    async fn runtime_actions(&mut self) -> Option<Value>;

    /// The runtime's description of one action, native or composite. `None`
    /// when the runtime is unreachable or does not know the name.
    async fn describe_runtime_action(&mut self, action_type: &str) -> Option<Value>;
}

/// The bridge-backed implementation.
///
/// Holds a mutable borrow of the bridge for as long as it lives, which is what
/// keeps it honest: the handler is handed this rather than the bridge, so the
/// wider surface is not merely discouraged, it is not in scope.
pub struct BridgeCatalog<'a> {
    bridge: &'a mut dyn Bridge,
}

impl<'a> BridgeCatalog<'a> {
    pub fn new(bridge: &'a mut dyn Bridge) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl CatalogQuery for BridgeCatalog<'_> {
    async fn runtime_actions(&mut self) -> Option<Value> {
        // The action is a literal. Nothing from the caller reaches it.
        self.bridge
            .send_action(&json!({"type": "ListActions"}))
            .await
            .ok()
    }

    async fn describe_runtime_action(&mut self, action_type: &str) -> Option<Value> {
        // The only caller-supplied value is a name, and it lands in a field
        // whose meaning is "which action to describe". It cannot change the
        // verb, because the verb is not a parameter.
        self.bridge
            .send_action(&json!({"type": "DescribeAction", "action_type": action_type}))
            .await
            .ok()
    }
}

/// Answer `ListActions` with the complete capability view.
///
/// Two authoritative sources, neither of them a list kept here:
///
/// * the runtime's own reply — generated vocabulary **and** live composites;
/// * `prompt::orchestrator_action_metas` — the 21 sysaicalls.
///
/// The merge itself already existed and is reused
/// (`prompt::merge_orchestrator_into_list_result`), so the surface stays
/// identical to the one the historical loop produced. What changes is where it
/// happens: the CLI used to forward to the bridge and then merge in its own
/// loop body, which is why TUI, Matrix and serve never got the orchestrator
/// half at all.
pub async fn list_actions(catalog: &mut dyn CatalogQuery) -> Value {
    let mut result = match catalog.runtime_actions().await {
        Some(r) => r,
        // The orchestrator half is still worth answering with. A model that
        // learns it can spawn agents while the runtime is down is better off
        // than one told nothing.
        None => json!({"success": true, "output": {"actions": []}}),
    };
    crate::prompt::merge_orchestrator_into_list_result(&mut result);
    result
}

/// Answer `DescribeAction`, orchestrator registry first.
///
/// Order matters and is the one the decision fixed: the registry owns the
/// sysaicalls, and the runtime owns everything else — including composite
/// primitives, which is why this cannot be answered from `x0ne_actions`.
/// A name in neither is reported as unsupported rather than guessed at.
pub async fn describe_action(catalog: &mut dyn CatalogQuery, action_type: &str) -> Value {
    // Already a complete envelope — `{success, action_type, output}`. Wrapping
    // it again would bury the schema at `output.output.schema`, which every
    // consumer would then have to know about.
    if let Some(envelope) = crate::prompt::describe_orchestrator_action(action_type) {
        return envelope;
    }
    match catalog.describe_runtime_action(action_type).await {
        Some(result) => result,
        None => json!({
            "success": false,
            "error": {
                "class": "unsupported",
                "message": format!(
                    "`{action_type}` is not a known action. Use ListActions to see \
                     what is available."
                ),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog that answers like the runtime does: generated vocabulary
    /// plus one primitive the user defined at runtime.
    #[derive(Default)]
    struct FakeRuntime {
        /// Every action value this fake was asked to execute. The point of the
        /// design is that this can only ever hold catalog reads.
        seen: Vec<String>,
    }

    #[async_trait::async_trait]
    impl CatalogQuery for FakeRuntime {
        async fn runtime_actions(&mut self) -> Option<Value> {
            self.seen.push("ListActions".into());
            Some(json!({
                "success": true,
                "output": { "actions": [
                    {"type": "ReadFile", "source": "native"},
                    {"type": "WriteFile", "source": "native"},
                    {"type": "selftest-greet", "source": "custom", "step_count": 2},
                ]}
            }))
        }

        async fn describe_runtime_action(&mut self, action_type: &str) -> Option<Value> {
            self.seen.push(format!("DescribeAction:{action_type}"));
            match action_type {
                "ReadFile" => Some(json!({
                    "success": true,
                    "output": {"action_type": "ReadFile", "source": "native"}
                })),
                "selftest-greet" => Some(json!({
                    "success": true,
                    "output": {"action_type": "selftest-greet", "source": "custom"}
                })),
                _ => None,
            }
        }
    }

    fn names(result: &Value) -> Vec<String> {
        result["output"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["type"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn the_catalog_carries_native_runtime_actions() {
        let listed = names(&list_actions(&mut FakeRuntime::default()).await);
        assert!(listed.contains(&"ReadFile".to_string()));
        assert!(listed.contains(&"WriteFile".to_string()));
    }

    /// The reason this module talks to the bridge at all. A composite is
    /// defined at runtime and lives in a store the orchestrator cannot read;
    /// answering from `x0ne_actions` would drop it silently.
    #[tokio::test]
    async fn a_composite_defined_at_runtime_survives_discovery() {
        let listed = names(&list_actions(&mut FakeRuntime::default()).await);
        assert!(
            listed.contains(&"selftest-greet".to_string()),
            "a user-defined primitive vanished from discovery: {listed:?}"
        );
    }

    #[tokio::test]
    async fn the_catalog_carries_orchestrator_sysaicalls() {
        let listed = names(&list_actions(&mut FakeRuntime::default()).await);
        for expected in ["SpawnWorld", "Plan", "RunIncus", "ProjectNote"] {
            assert!(
                listed.contains(&expected.to_string()),
                "{expected} missing from the combined view"
            );
        }
    }

    #[tokio::test]
    async fn the_combined_view_has_no_duplicates() {
        let listed = names(&list_actions(&mut FakeRuntime::default()).await);
        let unique: std::collections::BTreeSet<_> = listed.iter().collect();
        assert_eq!(unique.len(), listed.len(), "duplicate names in {listed:?}");
    }

    #[tokio::test]
    async fn describe_resolves_a_native_runtime_action() {
        let r = describe_action(&mut FakeRuntime::default(), "ReadFile").await;
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["action_type"], "ReadFile");
    }

    #[tokio::test]
    async fn describe_resolves_a_live_composite() {
        let r = describe_action(&mut FakeRuntime::default(), "selftest-greet").await;
        assert_eq!(r["success"], true);
        assert_eq!(r["output"]["source"], "custom");
    }

    #[tokio::test]
    async fn describe_resolves_an_orchestrator_sysaicall_without_asking_the_runtime() {
        let mut fake = FakeRuntime::default();
        let r = describe_action(&mut fake, "SpawnWorld").await;
        assert_eq!(r["success"], true);
        assert!(
            fake.seen.is_empty(),
            "the registry owns this name; the runtime was asked anyway: {:?}",
            fake.seen
        );
    }

    #[tokio::test]
    async fn describe_reports_an_unknown_name_as_unsupported() {
        let r = describe_action(&mut FakeRuntime::default(), "NotAnAction").await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"]["class"], "unsupported");
    }

    /// The structural claim, checked structurally. `CatalogQuery` has two
    /// methods, neither takes an action, and the two action literals live in
    /// this file. A handler given a `&mut dyn CatalogQuery` has no expression
    /// it could write to execute something else.
    #[test]
    fn the_catalog_door_cannot_carry_an_arbitrary_action() {
        let src = include_str!("catalog.rs");
        // Comments excluded: the module documentation names `send_action`
        // precisely in order to say the trait does not offer it.
        let production: String = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let production = production.as_str();

        let sends: Vec<_> = production.match_indices("send_action").collect();
        assert_eq!(
            sends.len(),
            2,
            "the catalog reaches the bridge {} times; it may do so exactly twice, \
             once per catalog operation",
            sends.len()
        );

        for verb in ["\"ListActions\"", "\"DescribeAction\""] {
            assert!(
                production.contains(verb),
                "{verb} is not a literal here — if the verb became a parameter, \
                 this door would execute anything"
            );
        }

        // No method on the trait accepts an action, so nothing caller-supplied
        // can become one.
        let trait_body = &production[production.find("pub trait CatalogQuery").unwrap()
            ..production.find("pub struct BridgeCatalog").unwrap()];
        assert!(
            !trait_body.contains("Value)") && !trait_body.contains("action: &Value"),
            "a CatalogQuery method takes an action; the door is no longer narrow"
        );
    }
}
