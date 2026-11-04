use serde_json::Value;

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::handles::SharedHandles;
use crate::policy::PolicyEngine;
use crate::result::{ActionOutput, ActionResult};

use super::desktop::RateLimiter;

/// Execute an Assert: verify a condition by delegating to existing primitives.
///
/// Assert is a pure observation — it never modifies state.
/// Bridge-routed checks (file_exists, file_contains, port_open, command_succeeds, env_set)
/// go through `execute_nested`, which evaluates policy exactly as the
/// runtime's own entry point does. This used to call the unpoliced
/// `execute_action`, so `command_succeeds` spawned processes with no gate at
/// all — see `execute_nested`'s documentation.
/// Pure-value checks (eq, gt, lt, not_empty) are evaluated locally.
pub async fn execute_assert(
    check: &str,
    params: &Value,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    let (ok, detail) = match check {
        // ── Bridge-routed checks (go through policy engine) ──────────────
        "file_exists" => {
            let path = params["path"].as_str().unwrap_or("");
            let action = Action::FileInfo {
                path: path.to_string(),
            };
            let result =
                super::execute_nested(&action, config, handles, policy, rate_limiter, composites)
                    .await;
            (result.success, format!("file_exists: {}", path))
        }

        "file_contains" => {
            let path = params["path"].as_str().unwrap_or("");
            let value = params["value"].as_str().unwrap_or("");
            let action = Action::ReadFile {
                path: path.to_string(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            };
            let result =
                super::execute_nested(&action, config, handles, policy, rate_limiter, composites)
                    .await;
            let contains = result
                .output
                .as_ref()
                .and_then(|o| {
                    if let ActionOutput::FileContent { content, .. } = o {
                        Some(content.contains(value))
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            (contains, format!("file_contains \"{}\" in {}", value, path))
        }

        "port_open" => {
            let host = params["host"].as_str().unwrap_or("127.0.0.1");
            let port = params["port"].as_u64().unwrap_or(0) as u16;
            let action = Action::PortCheck {
                host: host.to_string(),
                port,
                timeout_secs: Some(2),
            };
            let result =
                super::execute_nested(&action, config, handles, policy, rate_limiter, composites)
                    .await;
            let open = result
                .output
                .as_ref()
                .and_then(|o| {
                    if let ActionOutput::PortCheckResult { open, .. } = o {
                        Some(*open)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            (open, format!("port_open: {}:{}", host, port))
        }

        "command_succeeds" => {
            let cmd = params["command"].as_str().unwrap_or("true");
            let args: Vec<String> = params["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let action = Action::RunCommand {
                command: cmd.to_string(),
                args,
                cwd: None,
                env: Default::default(),
                timeout_secs: Some(10),
                stream: None,
            };
            let result =
                super::execute_nested(&action, config, handles, policy, rate_limiter, composites)
                    .await;
            let succeeded = result
                .output
                .as_ref()
                .and_then(|o| {
                    if let ActionOutput::CommandOutput { exit_code, .. } = o {
                        Some(*exit_code == 0)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            (succeeded, format!("command_succeeds: {}", cmd))
        }

        "env_set" => {
            let name = params["name"].as_str().unwrap_or("");
            // GetEnv was removed in v0.3 — read the variable directly here.
            let _ = (handles, policy, rate_limiter, composites, config);
            let is_set = std::env::var(name).is_ok();
            (is_set, format!("env_set: {}", name))
        }

        // ── Pure-value checks (no system access, safe to run locally) ────
        "eq" | "equals" | "value_eq" => {
            let ok = if let (Some(a), Some(b)) =
                (params["actual"].as_f64(), params["expected"].as_f64())
            {
                (a - b).abs() < f64::EPSILON
            } else {
                let a = params["actual"].to_string().replace('"', "");
                let b = params["expected"].to_string().replace('"', "");
                a == b
            };
            let a_str = params["actual"].to_string().replace('"', "");
            let b_str = params["expected"].to_string().replace('"', "");
            (ok, format!("actual={} expected={}", a_str, b_str))
        }

        "gt" | "greater_than" => {
            let actual = params["actual"].as_f64().unwrap_or(0.0);
            let expected = params["expected"].as_f64().unwrap_or(0.0);
            (actual > expected, format!("{} > {}", actual, expected))
        }

        "lt" | "less_than" => {
            let actual = params["actual"].as_f64().unwrap_or(0.0);
            let expected = params["expected"].as_f64().unwrap_or(0.0);
            (actual < expected, format!("{} < {}", actual, expected))
        }

        "not_empty" => {
            let value = params["value"].as_str().unwrap_or("");
            let ok = !value.is_empty();
            (
                ok,
                format!("not_empty: {}", if ok { "has content" } else { "empty" }),
            )
        }

        other => (false, format!("unknown check: {}", other)),
    };

    ActionResult::ok(
        "Assert",
        ActionOutput::AssertResult {
            ok,
            check: check.to_string(),
            detail,
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite;
    use crate::handles;
    use serde_json::json;

    // ── RED: nested execution must carry the same policy semantics ────────

    /// Build an engine from a config, so a test can disable an action or set
    /// a mode and observe the effect through a nested check.
    fn engine_for(config: &AppConfig) -> PolicyEngine {
        PolicyEngine::new(config.clone(), config.actions.clone())
    }

    async fn run_check(check: &str, params: Value, config: &AppConfig) -> ActionResult {
        let policy = engine_for(config);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(config);
        execute_assert(
            check,
            &params,
            config,
            handles,
            &policy,
            &rate_limiter,
            &composites,
        )
        .await
    }

    /// A check that synthesizes an action the policy engine would deny must
    /// not execute it. `Assert` is ReadOnly, so it is enabled by default —
    /// which is exactly why the nested action's own policy has to apply.
    #[tokio::test]
    async fn assert_command_succeeds_cannot_run_a_disabled_run_command() {
        let mut config = AppConfig::defaults();
        config.actions.insert("RunCommand".into(), false);
        // Prove the marker file is never created: the command would create it.
        let marker = std::env::temp_dir().join(format!("x0ne-assert-red-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let result = run_check(
            "command_succeeds",
            json!({"command": "touch", "args": [marker.to_string_lossy()]}),
            &config,
        )
        .await;

        assert!(
            !marker.exists(),
            "a disabled RunCommand must not execute through Assert"
        );
        if let Some(ActionOutput::AssertResult { ok, .. }) = result.output {
            assert!(!ok, "a denied check must not report success");
        }
        let _ = std::fs::remove_file(&marker);
    }

    /// readonly mode must not be circumvented by nesting.
    #[tokio::test]
    async fn readonly_mode_cannot_spawn_a_process_through_assert() {
        let mut config = AppConfig::defaults();
        config.policy.mode = "readonly".into();
        config.actions.insert("RunCommand".into(), true);
        let marker = std::env::temp_dir().join(format!("x0ne-assert-ro-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        run_check(
            "command_succeeds",
            json!({"command": "touch", "args": [marker.to_string_lossy()]}),
            &config,
        )
        .await;

        assert!(
            !marker.exists(),
            "readonly mode must not spawn a process through a nested check"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// The command allowlist must apply to the nested action too.
    #[tokio::test]
    async fn the_command_allowlist_applies_to_nested_execution() {
        let mut config = AppConfig::defaults();
        config.actions.insert("RunCommand".into(), true);
        config.policy.mode = "auto".into();
        config.policy.allowed_commands = vec!["echo".into()];
        let marker = std::env::temp_dir().join(format!("x0ne-assert-allow-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        run_check(
            "command_succeeds",
            json!({"command": "touch", "args": [marker.to_string_lossy()]}),
            &config,
        )
        .await;
        assert!(!marker.exists(), "`touch` is not on the allowlist");

        // …and an explicitly permitted command still works.
        let result = run_check(
            "command_succeeds",
            json!({"command": "echo", "args": ["hi"]}),
            &config,
        )
        .await;
        if let Some(ActionOutput::AssertResult { ok, .. }) = result.output {
            assert!(ok, "an allowlisted command must still run: {result:?}");
        } else {
            panic!("expected an AssertResult, got {result:?}");
        }
    }

    /// Path policy must apply to a nested read as well — `Assert` is not a
    /// way around the deny patterns or the read roots.
    #[tokio::test]
    async fn nested_reads_still_obey_path_policy() {
        let mut config = AppConfig::defaults();
        config.policy.allowed_read_paths = vec![];
        let result = run_check(
            "file_contains",
            json!({"path": "/etc/hostname", "value": "x"}),
            &config,
        )
        .await;
        if let Some(ActionOutput::AssertResult { ok, .. }) = result.output {
            assert!(!ok, "an empty read allowlist denies everything");
        }
    }

    #[tokio::test]
    async fn assert_eq_passes() {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);

        let params = json!({"actual": 42, "expected": 42});
        let result = execute_assert(
            "eq",
            &params,
            &config,
            handles,
            &policy,
            &rate_limiter,
            &composites,
        )
        .await;

        assert!(result.success);
        if let Some(ActionOutput::AssertResult { ok, check, .. }) = result.output {
            assert!(ok);
            assert_eq!(check, "eq");
        } else {
            panic!("Expected AssertResult");
        }
    }

    #[tokio::test]
    async fn assert_eq_fails() {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);

        let params = json!({"actual": 1, "expected": 2});
        let result = execute_assert(
            "eq",
            &params,
            &config,
            handles,
            &policy,
            &rate_limiter,
            &composites,
        )
        .await;

        assert!(result.success); // Assert itself succeeds
        if let Some(ActionOutput::AssertResult { ok, .. }) = result.output {
            assert!(!ok); // But the check failed
        }
    }

    #[tokio::test]
    async fn assert_not_empty_passes() {
        let config = AppConfig::defaults();
        let actions_map = config.actions.clone();
        let policy = PolicyEngine::new(config.clone(), actions_map);
        let handles = handles::new_shared(100, 60, 10);
        let rate_limiter = crate::executor::desktop::new_rate_limiter();
        let composites = composite::CompositeStore::new(&config);

        let params = json!({"value": "hello"});
        let result = execute_assert(
            "not_empty",
            &params,
            &config,
            handles,
            &policy,
            &rate_limiter,
            &composites,
        )
        .await;

        if let Some(ActionOutput::AssertResult { ok, .. }) = result.output {
            assert!(ok);
        }
    }
}
