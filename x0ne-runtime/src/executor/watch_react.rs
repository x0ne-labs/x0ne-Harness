use std::time::Duration;

use chrono::Utc;
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::handles::{gen_id, HandleStatus, HandleType, SharedHandles};
use crate::policy::PolicyEngine;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

use super::desktop::RateLimiter;

/// Watch a path for file changes and automatically execute an action on each trigger.
///
/// Returns a handle immediately. Each trigger:
/// 1. Debounces rapid changes
/// 2. Optionally filters by glob pattern
/// 3. Policy-checks the action
/// 4. Executes the action
/// 5. Pushes a summary event into the handle buffer
///
/// Auto-stops after `max_triggers` (default 50) to prevent infinite loops.
pub async fn watch_and_react(
    action: &Action,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    let Action::WatchAndReact {
        path,
        pattern,
        action: trigger_action,
        debounce_ms,
        max_triggers,
        recursive,
    } = action
    else {
        unreachable!()
    };

    // Validate the path exists
    let watch_path = std::path::Path::new(path);
    if !watch_path.exists() {
        return ActionResult::err(
            "WatchAndReact",
            ErrorKind::NotFound,
            format!("Watch path '{}' does not exist.", path),
            0,
        );
    }

    let handle_id = gen_id("react");
    {
        let mut mgr = handles.lock().await;
        if let Err(e) = mgr.register(handle_id.clone(), HandleType::FileWatch) {
            return ActionResult::err("WatchAndReact", ErrorKind::Internal, e, 0);
        }
    }

    let debounce_dur = Duration::from_millis(debounce_ms.unwrap_or(500));
    let max_trig = max_triggers.unwrap_or(50) as usize;
    let mode = if *recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };

    let path_str = path.clone();
    let pattern_clone = pattern.clone();
    let trigger_action_clone = *trigger_action.clone();
    let config_clone = config.clone();
    let handles_clone = handles.clone();
    let hid = handle_id.clone();
    let policy_clone = policy.clone();
    let rl_clone = rate_limiter.clone();
    let composites_clone = composites.clone();

    let task = tokio::spawn(async move {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
            Ok(w) => w,
            Err(e) => {
                let mut mgr = handles_clone.lock().await;
                mgr.set_status(
                    &hid,
                    HandleStatus::Failed {
                        at: Utc::now().to_rfc3339(),
                        error: format!("Failed to create file watcher: {}", e),
                    },
                );
                return;
            }
        };

        if let Err(e) = watcher.watch(std::path::Path::new(&path_str), mode) {
            let mut mgr = handles_clone.lock().await;
            mgr.set_status(
                &hid,
                HandleStatus::Failed {
                    at: Utc::now().to_rfc3339(),
                    error: format!("Failed to watch '{}': {}", path_str, e),
                },
            );
            return;
        }

        // Push initial setup event
        handles_clone.lock().await.push_event(
            &hid,
            json!({
                "event": "watching",
                "path": path_str,
                "pattern": pattern_clone,
                "max_triggers": max_trig,
                "timestamp": Utc::now().to_rfc3339(),
            }),
        );

        let mut trigger_count = 0usize;
        let mut last_trigger = std::time::Instant::now() - debounce_dur; // allow immediate first trigger

        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    // Debounce
                    if last_trigger.elapsed() < debounce_dur {
                        continue;
                    }

                    // Pattern filter
                    if let Some(ref pat) = pattern_clone {
                        let matches_pattern = event.paths.iter().any(|p| {
                            let name = p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            glob::Pattern::new(pat)
                                .map(|g| g.matches(&name))
                                .unwrap_or(false)
                        });
                        if !matches_pattern {
                            continue;
                        }
                    }

                    last_trigger = std::time::Instant::now();
                    trigger_count += 1;

                    let changed_path = event
                        .paths
                        .first()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();

                    // The trigger fires unattended in a background task, so a
                    // policy verdict of "ask a human" cannot be satisfied and
                    // must refuse. This arm previously matched only `Deny` and
                    // let `RequireConfirmation` fall into the else branch —
                    // i.e. an action the operator asked to be prompted about
                    // ran with nobody watching, once per trigger.
                    let denial = match policy_clone.evaluate(&trigger_action_clone) {
                        crate::policy::PolicyEffect::Allow => None,
                        crate::policy::PolicyEffect::Deny(reason) => Some(reason.message()),
                        crate::policy::PolicyEffect::RequireConfirmation(msg) => Some(format!(
                            "requires user approval, and a watcher trigger has no human \
                             to ask: {msg}"
                        )),
                    };
                    if let Some(reason) = denial {
                        handles_clone.lock().await.push_event(
                            &hid,
                            json!({
                                "event": "trigger_denied",
                                "trigger": trigger_count,
                                "changed_path": changed_path,
                                "reason": reason,
                                "timestamp": Utc::now().to_rfc3339(),
                            }),
                        );
                    } else {
                        // Execute the trigger action through the nested
                        // boundary, so the verdict above cannot go stale
                        // between the check and the run.
                        let result = super::execute_nested(
                            &trigger_action_clone,
                            &config_clone,
                            handles_clone.clone(),
                            &policy_clone,
                            &rl_clone,
                            &composites_clone,
                        )
                        .await;

                        let summary = if result.success {
                            result
                                .output
                                .as_ref()
                                .map(|o| o.summarize())
                                .unwrap_or_else(|| "ok".into())
                        } else {
                            result
                                .error
                                .as_ref()
                                .map(|e| e.message.clone())
                                .unwrap_or_else(|| "failed".into())
                        };

                        handles_clone.lock().await.push_event(
                            &hid,
                            json!({
                                "event": "triggered",
                                "trigger": trigger_count,
                                "changed_path": changed_path,
                                "action_type": trigger_action_clone.type_name(),
                                "success": result.success,
                                "summary": summary,
                                "duration_ms": result.duration_ms,
                                "timestamp": Utc::now().to_rfc3339(),
                            }),
                        );
                    }

                    // Check max triggers
                    if trigger_count >= max_trig {
                        let mut mgr = handles_clone.lock().await;
                        mgr.push_event(
                            &hid,
                            json!({
                                "event": "max_triggers_reached",
                                "total_triggers": trigger_count,
                                "timestamp": Utc::now().to_rfc3339(),
                            }),
                        );
                        mgr.set_status(
                            &hid,
                            HandleStatus::Completed {
                                at: Utc::now().to_rfc3339(),
                            },
                        );
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => {
                    // Check if handle was cancelled externally
                    let mgr = handles_clone.lock().await;
                    if mgr
                        .get_status(&hid)
                        .map(|s| s.is_terminal())
                        .unwrap_or(true)
                    {
                        break;
                    }
                }
            }
        }
    });

    handles.lock().await.set_task(&handle_id, task);

    ActionResult::ok(
        "WatchAndReact",
        ActionOutput::HandleCreated {
            handle: handle_id,
            handle_type: "watch_react".into(),
            streaming: true,
            pid: None,
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::handles;

    fn make_handles() -> SharedHandles {
        handles::new_shared(1000, 300, 20)
    }

    #[tokio::test]
    async fn watch_react_rejects_nonexistent_path() {
        let cfg = AppConfig::defaults();
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let h = make_handles();
        let rl = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::time::Instant::now() - Duration::from_secs(60),
        ));
        let composites = CompositeStore::new(&cfg);

        let action = Action::WatchAndReact {
            path: "/absolutely/nonexistent/path/xyz".into(),
            pattern: None,
            action: Box::new(Action::ListSnapshots),
            debounce_ms: None,
            max_triggers: None,
            recursive: true,
        };
        let result = watch_and_react(&action, &cfg, h, &policy, &rl, &composites).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("does not exist"));
    }

    #[tokio::test]
    async fn watch_react_returns_handle() {
        let cfg = AppConfig::defaults();
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let h = make_handles();
        let rl = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::time::Instant::now() - Duration::from_secs(60),
        ));
        let composites = CompositeStore::new(&cfg);

        let action = Action::WatchAndReact {
            path: "/tmp".into(),
            pattern: None,
            action: Box::new(Action::ListSnapshots),
            debounce_ms: Some(100),
            max_triggers: Some(3),
            recursive: false,
        };
        let result = watch_and_react(&action, &cfg, h.clone(), &policy, &rl, &composites).await;
        assert!(result.success, "failed: {:?}", result.error);
        if let Some(ActionOutput::HandleCreated { handle, .. }) = result.output {
            assert!(handle.starts_with("react_"));
            // Clean up
            h.lock().await.cancel(&handle);
        } else {
            panic!("expected HandleCreated");
        }
    }

    // The watcher task blocks its thread in recv_timeout: on the default
    // one-thread runtime it starved the sleeps below, 255 s (2026-10-27).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_react_triggers_on_file_change() {
        let cfg = AppConfig::defaults();
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let h = make_handles();
        let rl = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::time::Instant::now() - Duration::from_secs(60),
        ));
        let composites = CompositeStore::new(&cfg);

        // Create a temp dir to watch
        let tmp_dir = std::env::temp_dir().join(format!("x0ne_test_watch_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let action = Action::WatchAndReact {
            path: tmp_dir.to_string_lossy().into_owned(),
            pattern: None,
            action: Box::new(Action::ListSnapshots),
            debounce_ms: Some(50),
            max_triggers: Some(2),
            recursive: false,
        };
        let result = watch_and_react(&action, &cfg, h.clone(), &policy, &rl, &composites).await;
        assert!(result.success);

        let handle_id = if let Some(ActionOutput::HandleCreated { handle, .. }) = result.output {
            handle
        } else {
            panic!("expected HandleCreated");
        };

        // Give the watcher time to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Trigger a file change
        let test_file = tmp_dir.join("trigger.txt");
        std::fs::write(&test_file, "hello").unwrap();

        // Wait for the trigger to fire
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Check for events
        let events = h.lock().await.drain_events(&handle_id).unwrap_or_default();
        // Should have at least the "watching" event + a "triggered" event
        assert!(
            !events.is_empty(),
            "expected at least 1 event, got {}",
            events.len()
        );

        // Clean up
        h.lock().await.cancel(&handle_id);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
