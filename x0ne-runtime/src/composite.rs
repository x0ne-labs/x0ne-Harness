//! Composite primitives — user-defined reusable action pipelines.
//!
//! A composite primitive is a named sequence of existing actions with
//! parameterised placeholders (`{param_name}`) that are resolved at call time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::action::Action;
use crate::config::AppConfig;
use crate::executor::desktop::RateLimiter;
use crate::handles::SharedHandles;
use crate::policy::PolicyEngine;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

// ── Data structures ───────────────────────────────────────────────────────────

/// A composite primitive definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositePrimitive {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, ParamDef>,
    /// Raw action JSON with `{param}` placeholders.
    pub steps: Vec<Value>,
    #[serde(default)]
    pub ephemeral: bool,
    pub created_at: String,
}

/// Parameter definition for a composite primitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDef {
    #[serde(rename = "type", default = "default_param_type")]
    pub param_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
}

fn default_param_type() -> String {
    "string".into()
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// Thread-safe store for composite primitives with disk persistence.
#[derive(Clone)]
pub struct CompositeStore {
    inner: Arc<RwLock<HashMap<String, CompositePrimitive>>>,
    primitives_dir: PathBuf,
}

impl CompositeStore {
    /// Create a new store and ensure the primitives directory exists.
    pub fn new(config: &AppConfig) -> Self {
        let dir = config.composites.resolved_primitives_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("Failed to create primitives dir {:?}: {}", dir, e);
        }
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            primitives_dir: dir,
        }
    }

    /// Load all persistent composites from disk at startup.
    pub async fn load_from_disk(&self) {
        let dir = &self.primitives_dir;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return, // no dir or empty
        };
        let mut store = self.inner.write().await;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<CompositePrimitive>(&content) {
                    Ok(prim) => {
                        info!(name = %prim.name, "Loaded composite primitive from {:?}", path);
                        store.insert(prim.name.clone(), prim);
                    }
                    Err(e) => warn!("Failed to parse {:?}: {}", path, e),
                },
                Err(e) => warn!("Failed to read {:?}: {}", path, e),
            }
        }
        if !store.is_empty() {
            info!("Loaded {} composite primitives from disk", store.len());
        }
    }

    /// Get the store for read-only queries (list, describe, etc.).
    pub async fn list(&self) -> Vec<CompositePrimitive> {
        self.inner.read().await.values().cloned().collect()
    }

    /// Check if a name exists in the store.
    pub async fn contains(&self, name: &str) -> bool {
        self.inner.read().await.contains_key(name)
    }

    /// Get a single composite by name.
    pub async fn get(&self, name: &str) -> Option<CompositePrimitive> {
        self.inner.read().await.get(name).cloned()
    }

    /// Define a new composite primitive.
    pub async fn define(&self, prim: CompositePrimitive, config: &AppConfig) -> Result<(), String> {
        // Validate name is not a native action
        if is_native_action(&prim.name) {
            return Err(format!(
                "Name '{}' collides with a native primitive — choose a different name.",
                prim.name
            ));
        }

        // Validate step count
        if prim.steps.is_empty() {
            return Err("Composite must have at least one step.".into());
        }
        if prim.steps.len() > config.composites.max_steps {
            return Err(format!(
                "Too many steps ({}) — maximum is {}.",
                prim.steps.len(),
                config.composites.max_steps
            ));
        }

        // Validate steps don't contain forbidden types
        for (i, step) in prim.steps.iter().enumerate() {
            let step_type = step["type"].as_str().unwrap_or("");
            if step_type == "DefinePrimitive" {
                return Err(format!(
                    "Step {} is DefinePrimitive — composites cannot define other composites.",
                    i
                ));
            }
            if step_type == "CallPrimitive" {
                let call_name = step["name"].as_str().unwrap_or("");
                if call_name == prim.name {
                    return Err(format!(
                        "Step {} calls '{}' (self) — recursive composites are not allowed.",
                        i, prim.name
                    ));
                }
            }
            // Validate the action type exists (native or another composite)
            if !step_type.is_empty()
                && !is_native_action(step_type)
                && step_type != "CallPrimitive"
                && !self.contains(step_type).await
            {
                return Err(format!(
                    "Step {} references unknown action type '{}'.",
                    i, step_type
                ));
            }
        }

        // Persist to disk (unless ephemeral)
        if !prim.ephemeral {
            // Security: sanitize name for filesystem use to prevent path traversal.
            let safe_name: String = prim
                .name
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(64)
                .collect();
            let filename = format!("{}.json", safe_name.to_lowercase());
            let path = self.primitives_dir.join(&filename);
            let json = serde_json::to_string_pretty(&prim)
                .map_err(|e| format!("Serialize error: {}", e))?;
            std::fs::write(&path, json)
                .map_err(|e| format!("Failed to write {:?}: {}", path, e))?;
        }

        // Store in memory
        info!(name = %prim.name, ephemeral = prim.ephemeral, "Defined composite primitive");
        self.inner.write().await.insert(prim.name.clone(), prim);
        Ok(())
    }

    /// Delete a composite primitive.
    pub async fn delete(&self, name: &str) -> bool {
        let removed = self.inner.write().await.remove(name).is_some();
        if removed {
            // Remove from disk
            let filename = format!("{}.json", name.to_lowercase());
            let path = self.primitives_dir.join(&filename);
            let _ = std::fs::remove_file(&path);
            info!(name = %name, "Deleted composite primitive");
        }
        removed
    }
}

// ── Action handlers ───────────────────────────────────────────────────────────

/// Handle the `DefinePrimitive` action.
pub async fn handle_define(
    action: &Action,
    store: &CompositeStore,
    config: &AppConfig,
) -> ActionResult {
    if let Action::DefinePrimitive {
        name,
        description,
        params,
        steps,
        ephemeral,
    } = action
    {
        // Parse params from raw Value map to ParamDef map
        let parsed_params: HashMap<String, ParamDef> = params
            .iter()
            .map(|(k, v)| {
                let def: ParamDef = serde_json::from_value(v.clone()).unwrap_or(ParamDef {
                    param_type: "string".into(),
                    required: true,
                    default: None,
                });
                (k.clone(), def)
            })
            .collect();

        let prim = CompositePrimitive {
            name: name.clone(),
            description: description.clone(),
            params: parsed_params,
            steps: steps.clone(),
            ephemeral: *ephemeral,
            created_at: Utc::now().to_rfc3339(),
        };

        let param_count = prim.params.len();
        let step_count = prim.steps.len();

        match store.define(prim, config).await {
            Ok(()) => ActionResult::ok(
                "DefinePrimitive",
                ActionOutput::PrimitiveDefined {
                    name: name.clone(),
                    param_count,
                    step_count,
                    ephemeral: *ephemeral,
                },
                0,
            ),
            Err(e) => ActionResult::err("DefinePrimitive", ErrorKind::CompositeError, e, 0),
        }
    } else {
        ActionResult::err(
            "DefinePrimitive",
            ErrorKind::InvalidInput,
            "Not a DefinePrimitive action",
            0,
        )
    }
}

/// Handle the `CallPrimitive` action.
pub async fn handle_call(
    action: &Action,
    store: &CompositeStore,
    config: &AppConfig,
    policy: &PolicyEngine,
    handles: SharedHandles,
    rate_limiter: &RateLimiter,
    depth: usize,
) -> ActionResult {
    if let Action::CallPrimitive { name, args } = action {
        // Depth check
        if depth >= config.composites.max_depth {
            return ActionResult::err(
                "CallPrimitive",
                ErrorKind::CompositeError,
                format!(
                    "Composite call depth {} exceeds maximum of {}.",
                    depth, config.composites.max_depth
                ),
                0,
            );
        }

        // Look up the composite
        let prim = match store.get(name).await {
            Some(p) => p,
            None => {
                return ActionResult::err(
                    "CallPrimitive",
                    ErrorKind::NotFound,
                    format!("No composite primitive named '{}'.", name),
                    0,
                );
            }
        };

        // Resolve parameters: check required, apply defaults
        let resolved_args = match resolve_params(&prim.params, args) {
            Ok(a) => a,
            Err(e) => {
                return ActionResult::err("CallPrimitive", ErrorKind::InvalidInput, e, 0);
            }
        };

        // Substitute params into steps and parse as Actions
        let mut actions: Vec<Action> = Vec::new();
        for (i, step_template) in prim.steps.iter().enumerate() {
            let substituted = substitute_params(step_template, &resolved_args);

            // Check if this is a CallPrimitive step — handle depth tracking
            let step_type = substituted["type"].as_str().unwrap_or("");
            if step_type == "CallPrimitive" {
                // Will be handled by recursive dispatch — just parse it
            }

            match serde_json::from_value::<Action>(substituted.clone()) {
                Ok(a) => actions.push(a),
                Err(e) => {
                    return ActionResult::err(
                        "CallPrimitive",
                        ErrorKind::InvalidInput,
                        format!("Step {} failed to parse after param substitution: {}", i, e),
                        0,
                    );
                }
            }
        }

        // Log composite call start
        info!(
            primitive = %name,
            depth = depth,
            steps = actions.len(),
            "Composite call start"
        );

        // Execute steps sequentially, like a pipeline
        let start = std::time::Instant::now();
        let mut results = Vec::new();
        let mut stopped_early = false;

        for (i, step_action) in actions.iter().enumerate() {
            // Policy check each step individually
            match policy.evaluate(step_action) {
                crate::policy::PolicyEffect::Allow => {}
                crate::policy::PolicyEffect::RequireConfirmation(msg) => {
                    let body = format!("Requires approval: {}", msg);
                    results.push(crate::result::StepResult {
                        step: i,
                        action_type: step_action.type_name().to_string(),
                        success: false,
                        output_summary: format!("err: {}", truncate_summary(&body, 60)),
                        output: None,
                        error: Some(crate::result::ActionError::new(
                            ErrorKind::PolicyDenied,
                            body,
                        )),
                        duration_ms: 0,
                    });
                    stopped_early = true;
                    break;
                }
                crate::policy::PolicyEffect::Deny(reason) => {
                    let msg = reason.message().to_string();
                    results.push(crate::result::StepResult {
                        step: i,
                        action_type: step_action.type_name().to_string(),
                        success: false,
                        output_summary: format!("err: {}", truncate_summary(&msg, 60)),
                        output: None,
                        error: Some(crate::result::ActionError::new(
                            ErrorKind::PolicyDenied,
                            msg,
                        )),
                        duration_ms: 0,
                    });
                    stopped_early = true;
                    break;
                }
            }

            // Execute the step
            let step_start = std::time::Instant::now();
            let step_result = if matches!(step_action, Action::CallPrimitive { .. }) {
                // Recursive composite call — increment depth
                Box::pin(handle_call(
                    step_action,
                    store,
                    config,
                    policy,
                    handles.clone(),
                    rate_limiter,
                    depth + 1,
                ))
                .await
            } else {
                crate::executor::execute_action(
                    step_action,
                    config,
                    handles.clone(),
                    policy,
                    rate_limiter,
                    store,
                )
                .await
            };
            let step_dur = step_start.elapsed().as_millis() as u64;

            let step_success = step_result.success;
            let output_summary = match (
                step_success,
                step_result.output.as_ref(),
                step_result.error.as_ref(),
            ) {
                (true, Some(o), _) => o.summarize(),
                (false, _, Some(e)) => format!("err: {}", truncate_summary(&e.message, 60)),
                _ => String::new(),
            };
            results.push(crate::result::StepResult {
                step: i,
                action_type: step_result.action_type.clone(),
                success: step_result.success,
                output_summary,
                output: step_result.output,
                error: step_result.error,
                duration_ms: step_dur,
            });

            if !step_success {
                stopped_early = true;
                break;
            }
        }

        let total_dur = start.elapsed().as_millis() as u64;
        info!(
            primitive = %name,
            depth = depth,
            completed = results.len(),
            stopped_early = stopped_early,
            duration_ms = total_dur,
            "Composite call end"
        );

        ActionResult::ok(
            "CallPrimitive",
            ActionOutput::PipelineResult {
                steps_total: actions.len(),
                steps_completed: results.len(),
                stopped_early,
                results,
                snapshot_name: None,
            },
            total_dur,
        )
    } else {
        ActionResult::err(
            "CallPrimitive",
            ErrorKind::InvalidInput,
            "Not a CallPrimitive action",
            0,
        )
    }
}

/// Handle the `ListCustomPrimitives` action.
pub async fn handle_list(store: &CompositeStore) -> ActionResult {
    let prims = store.list().await;
    let list: Vec<Value> = prims
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "description": p.description,
                "params": p.params,
                "step_count": p.steps.len(),
                "ephemeral": p.ephemeral,
                "created_at": p.created_at,
            })
        })
        .collect();
    ActionResult::ok(
        "ListCustomPrimitives",
        ActionOutput::PrimitiveList { primitives: list },
        0,
    )
}

/// Handle the `DeletePrimitive` action.
pub async fn handle_delete(action: &Action, store: &CompositeStore) -> ActionResult {
    if let Action::DeletePrimitive { name } = action {
        let deleted = store.delete(name).await;
        ActionResult::ok(
            "DeletePrimitive",
            ActionOutput::PrimitiveDeleted {
                name: name.clone(),
                deleted,
            },
            0,
        )
    } else {
        ActionResult::err(
            "DeletePrimitive",
            ErrorKind::InvalidInput,
            "Not a DeletePrimitive action",
            0,
        )
    }
}

// ── Param resolution ──────────────────────────────────────────────────────────

/// Resolve call arguments against the parameter definitions.
/// Returns the final map of param_name → Value to substitute.
fn resolve_params(
    defs: &HashMap<String, ParamDef>,
    args: &HashMap<String, Value>,
) -> Result<HashMap<String, String>, String> {
    let mut resolved = HashMap::new();

    for (name, def) in defs {
        if let Some(val) = args.get(name) {
            resolved.insert(name.clone(), value_to_string(val));
        } else if let Some(ref default) = def.default {
            resolved.insert(name.clone(), value_to_string(default));
        } else if def.required {
            return Err(format!("Missing required parameter '{}'.", name));
        }
        // Not required + no default + not provided = skip
    }

    // Also pass through any args not in defs (extra args — permissive)
    for (name, val) in args {
        if !resolved.contains_key(name) {
            resolved.insert(name.clone(), value_to_string(val));
        }
    }

    Ok(resolved)
}

/// Convert a JSON Value to a string for substitution.
fn value_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(), // arrays/objects stay as JSON
    }
}

/// Substitute `{param_name}` placeholders in a JSON value tree.
fn substitute_params(template: &Value, params: &HashMap<String, String>) -> Value {
    match template {
        Value::String(s) => {
            let mut result = s.clone();
            for (key, val) in params {
                let placeholder = format!("{{{}}}", key);
                result = result.replace(&placeholder, val);
            }
            Value::String(result)
        }
        Value::Object(map) => {
            let new_map: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), substitute_params(v, params)))
                .collect();
            Value::Object(new_map)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| substitute_params(v, params)).collect())
        }
        other => other.clone(),
    }
}

/// Truncate a string for inline summaries (T2). Mirrors the helper in
/// `result.rs` so this module stays self-contained.
fn truncate_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

// ── Native action check ───────────────────────────────────────────────────────

/// Returns true if the given name matches a native (built-in) action type.
///
/// Derived from `x0ne_actions::Action` rather than from a transcribed list.
/// The transcription had fallen ~37 names behind the enum, so a user-defined
/// primitive could take the name of a real action — `Simulate`, `Try`,
/// `SafeChange`, `Remember` and others — and this guard would allow it.
/// Composite lookup happens before native dispatch, so the shadow would win.
///
/// The orchestrator-only sysaicalls are included too: they are dispatched
/// before the runtime ever sees them, so a primitive named after one would be
/// unreachable at best and confusing at worst.
fn is_native_action(name: &str) -> bool {
    x0ne_actions::all_type_names().contains(&name) || ORCHESTRATOR_RESERVED_NAMES.contains(&name)
}

/// Names the orchestrator dispatches itself. They have no `Action` variant, so
/// the derived catalog cannot cover them, but they are just as reserved.
static ORCHESTRATOR_RESERVED_NAMES: &[&str] = &[
    "Plan",
    "SpawnWorld",
    "SpawnAgent",
    "WaitAgent",
    "ListAgents",
    "BroadcastAgents",
    "GetBroadcasts",
    "AcknowledgeBroadcast",
    "GetBroadcastReceipts",
    "Reflect",
    "Matrix",
    "ArtifactExpose",
    "ArtifactUnexpose",
    "Budget",
    "ProjectNote",
    "RunIncus",
    "RunIncusGroup",
    "IncusCreateSnapshot",
    "IncusRestoreSnapshot",
    "IncusListSnapshots",
    "IncusDeleteSnapshot",
    "ListActions",
    "DescribeAction",
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ActionOutput;

    fn test_config() -> AppConfig {
        AppConfig::defaults()
    }

    /// Build an `AppConfig` whose `composites.primitives_dir` points at a
    /// freshly-created tempdir.  The returned `TempDir` must be kept alive
    /// for the duration of the test (drop = recursive delete).  Use this
    /// for any test that touches `ephemeral: false` or `load_from_disk`.
    fn test_config_with_tempdir() -> (AppConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = AppConfig::defaults();
        cfg.composites.primitives_dir = dir.path().to_string_lossy().into_owned();
        (cfg, dir)
    }

    #[test]
    fn param_substitution_exact() {
        let template = json!({"type": "ReadFile", "path": "{file_path}"});
        let mut params = HashMap::new();
        params.insert("file_path".into(), "/tmp/test.rs".into());
        let result = substitute_params(&template, &params);
        assert_eq!(result["path"], "/tmp/test.rs");
    }

    #[test]
    fn param_substitution_partial() {
        let template = json!({"type": "HttpRequest", "url": "{url}/api/health"});
        let mut params = HashMap::new();
        params.insert("url".into(), "https://example.com".into());
        let result = substitute_params(&template, &params);
        assert_eq!(result["url"], "https://example.com/api/health");
    }

    #[test]
    fn param_substitution_nested_array() {
        let template = json!({"type": "Pipeline", "steps": [
            {"type": "ReadFile", "path": "{path}"}
        ]});
        let mut params = HashMap::new();
        params.insert("path".into(), "/foo".into());
        let result = substitute_params(&template, &params);
        assert_eq!(result["steps"][0]["path"], "/foo");
    }

    #[test]
    fn resolve_params_with_defaults() {
        let mut defs = HashMap::new();
        defs.insert(
            "url".into(),
            ParamDef {
                param_type: "string".into(),
                required: true,
                default: None,
            },
        );
        defs.insert(
            "timeout".into(),
            ParamDef {
                param_type: "number".into(),
                required: false,
                default: Some(json!(30)),
            },
        );

        let mut args = HashMap::new();
        args.insert("url".into(), json!("https://x.com"));

        let resolved = resolve_params(&defs, &args).unwrap();
        assert_eq!(resolved["url"], "https://x.com");
        assert_eq!(resolved["timeout"], "30");
    }

    #[test]
    fn resolve_params_missing_required() {
        let mut defs = HashMap::new();
        defs.insert(
            "url".into(),
            ParamDef {
                param_type: "string".into(),
                required: true,
                default: None,
            },
        );

        let args = HashMap::new();
        let result = resolve_params(&defs, &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing required"));
    }

    /// RED (catalog drift, August 2026): the collision guard walked a
    /// hand-maintained name list that had fallen ~37 names behind the `Action`
    /// enum. A user-defined primitive could therefore take the name of a real
    /// action — including `Simulate`, the primitive whose whole job is to
    /// preview a plan safely — and the guard would wave it through.
    #[test]
    fn native_action_check_covers_the_whole_vocabulary() {
        for name in x0ne_actions::all_type_names() {
            assert!(
                is_native_action(name),
                "'{name}' is a real action but the collision guard does not know it, \
                 so a custom primitive could shadow it"
            );
        }
    }

    #[test]
    fn is_native_action_check() {
        assert!(is_native_action("ReadFile"));
        assert!(is_native_action("Pipeline"));
        assert!(is_native_action("DefinePrimitive"));
        assert!(!is_native_action("HealthCheck"));
        assert!(!is_native_action("MyCustomThing"));
    }

    #[tokio::test]
    async fn define_rejects_name_collision() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let prim = CompositePrimitive {
            name: "ReadFile".into(),
            description: None,
            params: HashMap::new(),
            steps: vec![json!({"type": "ListSnapshots"})],
            ephemeral: true,
            created_at: Utc::now().to_rfc3339(),
        };
        let result = store.define(prim, &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("collides with a native"));
    }

    #[tokio::test]
    async fn define_rejects_self_reference() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let prim = CompositePrimitive {
            name: "MyAction".into(),
            description: None,
            params: HashMap::new(),
            steps: vec![json!({"type": "CallPrimitive", "name": "MyAction"})],
            ephemeral: true,
            created_at: Utc::now().to_rfc3339(),
        };
        let result = store.define(prim, &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("recursive"));
    }

    #[tokio::test]
    async fn define_rejects_define_in_steps() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let prim = CompositePrimitive {
            name: "BadComposite".into(),
            description: None,
            params: HashMap::new(),
            steps: vec![json!({"type": "DefinePrimitive", "name": "Nested", "steps": []})],
            ephemeral: true,
            created_at: Utc::now().to_rfc3339(),
        };
        let result = store.define(prim, &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot define other"));
    }

    #[tokio::test]
    async fn define_and_list() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let prim = CompositePrimitive {
            name: "TestAction".into(),
            description: Some("A test composite".into()),
            params: HashMap::new(),
            steps: vec![json!({"type": "ListSnapshots"})],
            ephemeral: true,
            created_at: Utc::now().to_rfc3339(),
        };
        store.define(prim, &config).await.unwrap();
        let list = store.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "TestAction");
    }

    #[tokio::test]
    async fn define_and_delete() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let prim = CompositePrimitive {
            name: "ToDelete".into(),
            description: None,
            params: HashMap::new(),
            steps: vec![json!({"type": "ListSnapshots"})],
            ephemeral: true,
            created_at: Utc::now().to_rfc3339(),
        };
        store.define(prim, &config).await.unwrap();
        assert!(store.contains("ToDelete").await);
        let deleted = store.delete("ToDelete").await;
        assert!(deleted);
        assert!(!store.contains("ToDelete").await);
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let deleted = store.delete("DoesNotExist").await;
        assert!(!deleted);
    }

    #[test]
    fn value_to_string_types() {
        assert_eq!(value_to_string(&json!("hello")), "hello");
        assert_eq!(value_to_string(&json!(42)), "42");
        assert_eq!(value_to_string(&json!(true)), "true");
        assert_eq!(value_to_string(&json!(null)), "null");
    }

    // ── handle_define-level tests ────────────────────────────────────────────
    //
    // The tests above all drive `store.define()` directly. The block below
    // exercises the public `handle_define(action, store, config)` handler
    // — the actual entry point invoked by the executor dispatch — so that
    // the `Action::DefinePrimitive → ActionResult` shape is locked down.

    /// Build an `Action::DefinePrimitive` from the loosely-typed pieces a
    /// caller would put on the wire.  The `params` map mirrors the JSON
    /// shape (each value is a `ParamDef`-shaped object, not a `ParamDef`).
    fn make_define_action(
        name: &str,
        params: serde_json::Map<String, Value>,
        steps: Vec<Value>,
        ephemeral: bool,
    ) -> Action {
        Action::DefinePrimitive {
            name: name.to_string(),
            description: Some(format!("test composite {name}")),
            params: params.into_iter().collect(),
            steps,
            ephemeral,
        }
    }

    /// Happy path through the public handler: a minimal valid composite
    /// returns success and a `PrimitiveDefined` output whose counters
    /// match the input shape.
    #[tokio::test]
    async fn handle_define_returns_primitive_defined_output() {
        let (config, _tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);

        let mut params = serde_json::Map::new();
        params.insert("url".into(), json!({ "type": "string", "required": true }));
        params.insert(
            "timeout".into(),
            json!({ "type": "number", "required": false, "default": 30 }),
        );

        let action = make_define_action(
            "handler_happy_path",
            params,
            vec![
                json!({ "type": "FetchUrl", "url": "{url}" }),
                json!({ "type": "ListSnapshots" }),
            ],
            true,
        );

        let result = handle_define(&action, &store, &config).await;

        assert!(
            result.success,
            "expected success, got error: {:?}",
            result.error
        );
        assert_eq!(result.action_type, "DefinePrimitive");
        match result.output {
            Some(ActionOutput::PrimitiveDefined {
                name,
                param_count,
                step_count,
                ephemeral,
            }) => {
                assert_eq!(name, "handler_happy_path");
                assert_eq!(param_count, 2);
                assert_eq!(step_count, 2);
                assert!(ephemeral);
            }
            other => panic!("expected PrimitiveDefined output, got {other:?}"),
        }

        // And the store must actually contain the new composite.
        assert!(store.contains("handler_happy_path").await);
    }

    /// The handler is variant-guarded: feeding it any other `Action`
    /// must yield an `InvalidInput` error rather than panicking. This
    /// pins down the safety net in case dispatch ever mis-routes.
    #[tokio::test]
    async fn handle_define_on_wrong_action_returns_invalid_input() {
        let config = test_config();
        let store = CompositeStore::new(&config);
        let wrong = Action::ListSnapshots;

        let result = handle_define(&wrong, &store, &config).await;
        assert!(!result.success);
        let err = result.error.expect("expected an error payload");
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
        assert!(err.message.contains("Not a DefinePrimitive"));
    }

    /// `steps: []` is rejected: a composite with no body would silently
    /// succeed at call time, masking caller mistakes. Verify the error
    /// kind and the human-readable message.
    #[tokio::test]
    async fn define_rejects_empty_steps() {
        let (config, _tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);
        let action = make_define_action(
            "empty_body",
            serde_json::Map::new(),
            vec![], // <- the offence
            true,
        );

        let result = handle_define(&action, &store, &config).await;
        assert!(!result.success);
        let err = result.error.expect("expected error");
        assert!(matches!(err.kind, ErrorKind::CompositeError));
        assert!(
            err.message.contains("at least one step"),
            "msg: {}",
            err.message
        );
        assert!(
            !store.contains("empty_body").await,
            "store must not retain rejected composite"
        );
    }

    /// Number of steps is capped by `composites.max_steps`. Build a
    /// composite with one step over the limit and assert the structured
    /// error mentions both the actual count and the cap.
    #[tokio::test]
    async fn define_rejects_steps_over_max() {
        let (mut config, _tmp) = test_config_with_tempdir();
        // Lower the cap so we don't have to build 21 dummy steps.
        config.composites.max_steps = 3;
        let store = CompositeStore::new(&config);

        let too_many: Vec<Value> = (0..(config.composites.max_steps + 1))
            .map(|i| json!({ "type": "ListSnapshots", "n": i }))
            .collect();

        let action = make_define_action("too_long", serde_json::Map::new(), too_many, true);
        let result = handle_define(&action, &store, &config).await;
        assert!(!result.success);
        let err = result.error.expect("expected error");
        assert!(matches!(err.kind, ErrorKind::CompositeError));
        assert!(
            err.message.contains("Too many steps"),
            "msg: {}",
            err.message
        );
        assert!(err.message.contains("4"));
        assert!(err.message.contains("3"));
    }

    /// A step whose `type` is neither a native action nor an existing
    /// composite must be rejected at definition time, before any side
    /// effect can be triggered.
    #[tokio::test]
    async fn define_rejects_unknown_action_type_in_steps() {
        let (config, _tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);

        let action = make_define_action(
            "uses_unknown_action",
            serde_json::Map::new(),
            vec![
                json!({ "type": "ListSnapshots" }),               // legit
                json!({ "type": "TotallyMadeUpAction", "x": 1 }), // bogus
            ],
            true,
        );
        let result = handle_define(&action, &store, &config).await;
        assert!(!result.success);
        let err = result.error.expect("expected error");
        assert!(matches!(err.kind, ErrorKind::CompositeError));
        assert!(
            err.message.contains("unknown action type")
                && err.message.contains("TotallyMadeUpAction"),
            "msg: {}",
            err.message
        );
        assert!(!store.contains("uses_unknown_action").await);
    }

    /// A composite *can* reference another, already-defined composite as
    /// a step type (this is what enables building higher-level pipelines
    /// out of named sub-pipelines). Without this test, the
    /// `is_native_action || is_composite` branch on l.162 is silent.
    #[tokio::test]
    async fn define_accepts_step_referencing_known_composite() {
        let (config, _tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);

        // First, define a "leaf" composite.
        let leaf = make_define_action(
            "leaf_action",
            serde_json::Map::new(),
            vec![json!({ "type": "ListSnapshots" })],
            true,
        );
        let r1 = handle_define(&leaf, &store, &config).await;
        assert!(r1.success);

        // Then define a composite whose step type is the leaf composite.
        let parent = make_define_action(
            "parent_action",
            serde_json::Map::new(),
            vec![json!({ "type": "leaf_action" })],
            true,
        );
        let r2 = handle_define(&parent, &store, &config).await;
        assert!(
            r2.success,
            "expected parent to accept reference to known composite, got: {:?}",
            r2.error
        );
    }

    /// `ephemeral: false` MUST persist the composite as a JSON file in
    /// `composites.primitives_dir`. The filename is the lowercased
    /// composite name + `.json`.
    #[tokio::test]
    async fn define_persistent_writes_to_disk() {
        let (config, tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);

        let action = make_define_action(
            "PersistedThing",
            serde_json::Map::new(),
            vec![json!({ "type": "ListSnapshots" })],
            false, // <- persistent
        );
        let result = handle_define(&action, &store, &config).await;
        assert!(result.success, "{:?}", result.error);

        let expected = tmp.path().join("persistedthing.json");
        assert!(expected.exists(), "expected file at {:?}", expected);

        // And the on-disk JSON should round-trip back to a CompositePrimitive.
        let raw = std::fs::read_to_string(&expected).expect("read");
        let parsed: CompositePrimitive = serde_json::from_str(&raw).expect("parse");
        assert_eq!(parsed.name, "PersistedThing");
        assert!(!parsed.ephemeral);
        assert_eq!(parsed.steps.len(), 1);
    }

    /// `ephemeral: true` must NOT touch the disk — the temp dir stays
    /// empty (modulo what `CompositeStore::new` may have done, which is
    /// just `mkdir -p` of the dir itself, not writing files inside).
    #[tokio::test]
    async fn define_ephemeral_does_not_write_to_disk() {
        let (config, tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);
        let action = make_define_action(
            "vanish",
            serde_json::Map::new(),
            vec![json!({ "type": "ListSnapshots" })],
            true,
        );
        let result = handle_define(&action, &store, &config).await;
        assert!(result.success);

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert!(
            entries.is_empty(),
            "ephemeral define wrote files: {:?}",
            entries
        );
    }

    /// End-to-end persistence round-trip: define a non-ephemeral
    /// composite, drop the in-memory store, instantiate a fresh store
    /// over the same primitives_dir, call `load_from_disk`, and assert
    /// the composite is back.
    #[tokio::test]
    async fn load_from_disk_round_trip() {
        let (config, tmp) = test_config_with_tempdir();

        // First lifecycle: define + drop.
        {
            let store = CompositeStore::new(&config);
            let action = make_define_action(
                "ReloadMe",
                serde_json::Map::new(),
                vec![json!({ "type": "ListSnapshots" })],
                false,
            );
            assert!(handle_define(&action, &store, &config).await.success);
        }

        // Second lifecycle: fresh store on the same dir.
        let store2 = CompositeStore::new(&config);
        assert!(
            !store2.contains("ReloadMe").await,
            "in-memory should be empty before load"
        );
        store2.load_from_disk().await;
        assert!(
            store2.contains("ReloadMe").await,
            "expected ReloadMe to be loaded from {:?}",
            tmp.path()
        );

        // And `delete` should clean up the disk file too.
        assert!(store2.delete("ReloadMe").await);
        let leftover = tmp.path().join("reloadme.json");
        assert!(!leftover.exists(), "delete() must remove the disk file");
    }

    /// Param map values that don't match the `ParamDef` shape (e.g. the
    /// caller passes a bare string instead of `{"type":"string", ...}`)
    /// fall back to `ParamDef { type: "string", required: true }` rather
    /// than rejecting the whole definition. Pin the lenient behavior so
    /// it isn't accidentally tightened later.
    #[tokio::test]
    async fn handle_define_param_fallback_on_invalid_paramdef() {
        let (config, _tmp) = test_config_with_tempdir();
        let store = CompositeStore::new(&config);

        let mut params = serde_json::Map::new();
        // Definitely not a ParamDef shape — should hit the unwrap_or branch.
        params.insert("misshaped".into(), json!("just a string"));

        let action = make_define_action(
            "lenient_params",
            params,
            vec![json!({ "type": "ListSnapshots" })],
            true,
        );
        let result = handle_define(&action, &store, &config).await;
        assert!(result.success, "{:?}", result.error);

        let stored = store
            .get("lenient_params")
            .await
            .expect("composite registered");
        let p = stored
            .params
            .get("misshaped")
            .expect("param was registered");
        assert_eq!(p.param_type, "string", "fallback type must be 'string'");
        assert!(p.required, "fallback must be required=true");
        assert!(p.default.is_none(), "fallback must have no default");
    }
}
