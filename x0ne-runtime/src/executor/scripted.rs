//! Executor for the `*Function` action family — register / call / list / delete
//! user-defined scripted primitives. The on-disk model + Rhai engine live in
//! `crate::scripted`; this file is just the thin glue from `Action` →
//! `ActionResult`, with policy-friendly error mapping.

use chrono::Utc;
use serde_json::{json, Value};

use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};
use crate::scripted::{self, FunctionParam, FunctionTier, ScriptError, ScriptedFunction};

fn err(action: &str, kind: ErrorKind, msg: impl Into<String>) -> ActionResult {
    ActionResult::err(action, kind, msg, 0)
}

fn parse_tier(s: Option<&str>) -> Result<FunctionTier, String> {
    match s.unwrap_or("rhai").to_ascii_lowercase().as_str() {
        "rhai" => Ok(FunctionTier::Rhai),
        "wasm" => Ok(FunctionTier::Wasm),
        other => Err(format!("unknown tier '{}': use 'rhai' or 'wasm'", other)),
    }
}

fn parse_params(raw: &[Value]) -> Result<Vec<FunctionParam>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for (i, v) in raw.iter().enumerate() {
        // Accept either a bare string ("name") or an object {"name": "...", "type": "..."}.
        let p = if let Some(s) = v.as_str() {
            FunctionParam {
                name: s.to_string(),
                r#type: "any".into(),
            }
        } else if let Some(obj) = v.as_object() {
            let name = obj
                .get("name")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| format!("param #{}: missing 'name' field", i))?;
            let ty = obj
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("any")
                .to_string();
            FunctionParam { name, r#type: ty }
        } else {
            return Err(format!(
                "param #{}: must be a string or {{name, type}} object",
                i
            ));
        };
        out.push(p);
    }
    Ok(out)
}

fn function_to_summary(f: &ScriptedFunction) -> Value {
    json!({
        "name": f.name,
        "description": f.description,
        "tier": match f.tier { FunctionTier::Rhai => "rhai", FunctionTier::Wasm => "wasm" },
        "params": f.params.iter().map(|p| json!({"name": p.name, "type": p.r#type})).collect::<Vec<_>>(),
        "returns": f.returns,
        "source_hash": f.source_hash,
        "created_at": f.created_at.to_rfc3339(),
        // We deliberately do NOT echo the source here — it can be large,
        // and `ListFunctions` is meant to be a compact catalog. The agent
        // can re-read source via the registry file directly if needed.
    })
}

// ── DefineFunction ───────────────────────────────────────────────────────────

pub async fn define_function(action: &Action, _cfg: &AppConfig) -> ActionResult {
    let Action::DefineFunction {
        name,
        description,
        tier,
        source,
        params,
        returns,
    } = action
    else {
        return err(
            "DefineFunction",
            ErrorKind::Internal,
            "wrong action variant",
        );
    };

    if let Err(m) = scripted::validate_name(name) {
        return err("DefineFunction", ErrorKind::InvalidInput, m);
    }
    let tier = match parse_tier(tier.as_deref()) {
        Ok(t) => t,
        Err(m) => return err("DefineFunction", ErrorKind::InvalidInput, m),
    };
    let params = match parse_params(params) {
        Ok(p) => p,
        Err(m) => return err("DefineFunction", ErrorKind::InvalidInput, m),
    };
    if matches!(tier, FunctionTier::Rhai) {
        if let Err(m) = scripted::validate_source(source) {
            return err("DefineFunction", ErrorKind::InvalidInput, m);
        }
    }

    let function = ScriptedFunction {
        name: name.clone(),
        description: description.clone(),
        tier,
        source: source.clone(),
        params,
        returns: returns.clone().unwrap_or_else(|| "any".to_string()),
        source_hash: scripted::hash_source(source),
        created_at: Utc::now(),
    };

    if let Err(e) = scripted::with_registry(|reg| {
        reg.functions
            .insert(function.name.clone(), function.clone());
    }) {
        return err(
            "DefineFunction",
            ErrorKind::ExecutionFailed,
            format!("registry: {}", e),
        );
    }

    ActionResult::ok(
        "DefineFunction",
        ActionOutput::FunctionRegistered {
            name: function.name.clone(),
            tier: match function.tier {
                FunctionTier::Rhai => "rhai".into(),
                FunctionTier::Wasm => "wasm".into(),
            },
            param_count: function.params.len(),
            source_hash: function.source_hash.clone(),
        },
        0,
    )
}

// ── CallFunction ─────────────────────────────────────────────────────────────

pub async fn call_function(action: &Action, _cfg: &AppConfig) -> ActionResult {
    let Action::CallFunction { name, args } = action else {
        return err("CallFunction", ErrorKind::Internal, "wrong action variant");
    };

    let func = match scripted::load_registry() {
        Ok(reg) => match reg.functions.get(name).cloned() {
            Some(f) => f,
            None => {
                return err(
                    "CallFunction",
                    ErrorKind::InvalidInput,
                    format!(
                        "unknown function '{}' — define it first with DefineFunction",
                        name
                    ),
                );
            }
        },
        Err(e) => {
            return err(
                "CallFunction",
                ErrorKind::ExecutionFailed,
                format!("registry: {}", e),
            )
        }
    };

    if !func.params.is_empty() && func.params.len() != args.len() {
        return err(
            "CallFunction",
            ErrorKind::InvalidInput,
            format!(
                "function '{}' takes {} param(s), got {} arg(s)",
                name,
                func.params.len(),
                args.len()
            ),
        );
    }

    match scripted::call_function(&func, args) {
        Ok(r) => ActionResult::ok(
            "CallFunction",
            ActionOutput::FunctionCallResult {
                name: name.clone(),
                value: r.value,
                duration_us: r.duration_us,
            },
            0,
        ),
        Err(ScriptError::Compile(m)) => err("CallFunction", ErrorKind::InvalidInput, m),
        Err(ScriptError::Marshal(m)) => err("CallFunction", ErrorKind::InvalidInput, m),
        Err(ScriptError::Runtime(m)) => err("CallFunction", ErrorKind::ExecutionFailed, m),
    }
}

// ── ListFunctions ────────────────────────────────────────────────────────────

pub async fn list_functions(_action: &Action, _cfg: &AppConfig) -> ActionResult {
    let reg = match scripted::load_registry() {
        Ok(r) => r,
        Err(e) => {
            return err(
                "ListFunctions",
                ErrorKind::ExecutionFailed,
                format!("registry: {}", e),
            )
        }
    };
    let summaries: Vec<Value> = reg
        .list_sorted()
        .into_iter()
        .map(function_to_summary)
        .collect();
    ActionResult::ok(
        "ListFunctions",
        ActionOutput::FunctionListing {
            count: summaries.len(),
            functions: summaries,
        },
        0,
    )
}

// ── DeleteFunction ───────────────────────────────────────────────────────────

pub async fn delete_function(action: &Action, _cfg: &AppConfig) -> ActionResult {
    let Action::DeleteFunction { name } = action else {
        return err(
            "DeleteFunction",
            ErrorKind::Internal,
            "wrong action variant",
        );
    };
    match scripted::delete_function(name) {
        Ok(true) => ActionResult::ok(
            "DeleteFunction",
            ActionOutput::FunctionDeleted {
                name: name.clone(),
                removed: true,
            },
            0,
        ),
        Ok(false) => err(
            "DeleteFunction",
            ErrorKind::InvalidInput,
            format!("unknown function '{}'", name),
        ),
        Err(e) => err(
            "DeleteFunction",
            ErrorKind::ExecutionFailed,
            format!("registry: {}", e),
        ),
    }
}
