//! Plan mode, the orchestrator's half.
//!
//! `/plan` (REPL, TUI, Shift+Tab in the TUI) puts the session in plan mode and
//! `/build` takes it out. The runtime holds the line:
//! [`crate::agent::session::CliAgentSession::set_plan_mode`] sends it a
//! `SessionMode` control line first, and from then on `x0ne-runtime` refuses
//! every action whose class touches the host (`DenyReason::PlanMode`), whoever
//! sends it. This module refuses the same actions earlier, in `dispatch`,
//! before any gate, so nobody is asked about an action that cannot run; and it
//! covers what never reaches the runtime: orchestrator sysaicalls, connector
//! tools, frontend claims.
//!
//! Only a human switches. No action, sysaicall or config key leads to
//! `set_plan_mode`; `only_the_frontends_switch_plan_mode` pins its callers.

use serde_json::Value;

use crate::agent::dispatch::Route;

/// Orchestrator-executed actions that only read, and so run in plan mode.
/// Every other sysaicall is refused: it spawns, writes, publishes or runs a
/// program on the host outside the runtime's policy.
pub const READ_ONLY_SYSAICALLS: &[&str] = &[
    "ListActions",
    "DescribeAction",
    "Reflect",
    "Matrix",
    "ListAgents",
    "WaitAgent",
    "GetBroadcasts",
    "GetBroadcastReceipts",
];

/// Appended to the system prompt of every model call while plan mode is on.
pub const SYSTEM_NOTICE: &str = "\
# Plan mode

The user put this session in plan mode. The runtime refuses every write, \
command, network call and other action that changes the host, whatever was \
allowed or approved before. Read and search what you need, then answer with a \
plan: the steps, the files each step changes, and how to check the result. Do \
not try to carry the plan out. Only the user leaves plan mode, with /build; no \
action of yours can.";

/// Put in the conversation when the human switches, so the model sees the
/// change where it happened.
pub fn switch_notice(plan: bool) -> &'static str {
    if plan {
        "Plan mode is on: the user typed /plan. The runtime now refuses every action \
         that changes the host. Read what you need, then answer with a plan."
    } else {
        "Plan mode is off: the user typed /build. The configured policy applies again."
    }
}

/// Told to the model when plan mode refuses `action_type`.
pub fn refusal(action_type: &str) -> String {
    format!(
        "`{action_type}` was refused: the session is in plan mode, chosen by the user. \
         Nothing that writes, runs a command or otherwise changes the host runs until \
         the user types /build. Read what you need, then answer with your plan."
    )
}

/// Does plan mode let this action through?
///
/// `claimed` is the class a frontend gave the action when it executes it
/// itself. SECURITY: fail closed. An action the runtime vocabulary cannot
/// parse is refused, since its effects cannot be bounded.
pub fn permits(
    action: &Value,
    route: Route,
    claimed: Option<x0ne_actions::SideEffectClass>,
) -> bool {
    if let Some(class) = claimed {
        return !class.touches_host();
    }
    let action_type = action.get("type").and_then(Value::as_str).unwrap_or("");
    match route {
        // A question to the person at the keyboard, or a name nobody executes.
        Route::Human | Route::Unsupported => true,
        // A third party's tool: its effects are not ours to bound.
        Route::Connector => false,
        Route::Orchestrator => {
            READ_ONLY_SYSAICALLS.contains(&action_type)
                || (action_type == "Budget"
                    && action
                        .get("action")
                        .and_then(Value::as_str)
                        .is_none_or(|a| a == "get"))
        }
        Route::Runtime => match serde_json::from_value::<x0ne_actions::Action>(action.clone()) {
            Ok(typed) => !modifies(&typed),
            Err(_) => false,
        },
    }
}

/// Does this action, or anything nested in it, touch the host?
fn modifies(action: &x0ne_actions::Action) -> bool {
    action.side_effect_class().touches_host()
        || crate::terminal::composite_children(action)
            .iter()
            .any(modifies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_pass_and_writes_do_not() {
        let r = Route::Runtime;
        assert!(permits(&json!({"type": "ReadFile", "path": "/x"}), r, None));
        assert!(permits(
            &json!({"type": "SearchCode", "pattern": "fn"}),
            r,
            None
        ));
        for a in [
            json!({"type": "WriteFile", "path": "/x", "content": "y"}),
            json!({"type": "RunCommand", "command": "ls"}),
            json!({"type": "MoveToTrash", "path": "/x"}),
            json!({"type": "FetchUrl", "url": "https://example.com"}),
        ] {
            assert!(!permits(&a, r, None), "{a}");
        }
    }

    /// A write parked in a composite is a write.
    #[test]
    fn a_composite_answers_for_its_children() {
        let r = Route::Runtime;
        let read = json!({"type": "ReadFile", "path": "/x"});
        let write = json!({"type": "WriteFile", "path": "/x", "content": "y"});
        assert!(permits(
            &json!({"type": "Pipeline", "steps": [read]}),
            r,
            None
        ));
        for a in [
            json!({"type": "Pipeline", "steps": [read, write]}),
            json!({"type": "Try", "actions": [write]}),
            json!({"type": "SafeChange", "steps": [read], "tests": [write]}),
        ] {
            assert!(!permits(&a, r, None), "{a}");
        }
    }

    /// SECURITY: what cannot be classified does not run.
    #[test]
    fn an_unparseable_runtime_action_is_refused() {
        assert!(!permits(
            &json!({"type": "WriteFile"}),
            Route::Runtime,
            None
        ));
        assert!(!permits(
            &json!({"type": "NoSuchAction"}),
            Route::Runtime,
            None
        ));
    }

    #[test]
    fn only_reading_sysaicalls_pass() {
        let o = Route::Orchestrator;
        for t in READ_ONLY_SYSAICALLS {
            assert!(permits(&json!({"type": t}), o, None), "{t}");
        }
        assert!(permits(&json!({"type": "Budget"}), o, None));
        assert!(!permits(
            &json!({"type": "Budget", "action": "set", "limit": 9.0}),
            o,
            None
        ));
        for t in [
            "SpawnAgent",
            "SpawnWorld",
            "Plan",
            "ProjectNote",
            "BroadcastAgents",
            "AcknowledgeBroadcast",
            "ArtifactExpose",
            "RunIncus",
            "IncusCreateSnapshot",
            "IncusListSnapshots",
        ] {
            assert!(!permits(&json!({"type": t}), o, None), "{t}");
        }
        assert!(!permits(
            &json!({"type": "gh.create_issue"}),
            Route::Connector,
            None
        ));
        assert!(permits(&json!({"type": "AskHuman"}), Route::Human, None));
    }

    /// Every sysaicall the handler implements is decided here on purpose: a
    /// new one is refused in plan mode until someone lists it as a read.
    #[test]
    fn every_listed_sysaicall_is_one_the_handler_implements() {
        let implemented = crate::agent::sysaicall_handler::implemented_action_types();
        for t in READ_ONLY_SYSAICALLS {
            assert!(implemented.iter().any(|i| i == t), "{t} is not implemented");
        }
    }

    #[test]
    fn a_frontend_claim_answers_by_its_class() {
        use x0ne_actions::SideEffectClass as C;
        let a = json!({"type": "PaintBadge"});
        assert!(permits(&a, Route::Runtime, Some(C::ReadOnly)));
        assert!(!permits(&a, Route::Runtime, Some(C::ProcessSpawn)));
    }

    /// SECURITY: no configuration key names plan mode, so neither a user nor
    /// a project `x0ne-orchestrator.toml`, trusted or privileged, starts a
    /// session in it or out of it. A failure means someone added such a key:
    /// it must stay out of `PROJECT_SETTABLE`, and this test must say why.
    #[test]
    fn no_config_key_names_plan_mode() {
        fn keys(v: &Value, path: &str, out: &mut Vec<String>) {
            if let Value::Object(m) = v {
                for (k, v) in m {
                    let p = format!("{path}.{k}");
                    out.push(p.clone());
                    keys(v, &p, out);
                }
            }
        }
        let mut all = Vec::new();
        keys(
            &serde_json::to_value(crate::config::Config::default()).unwrap(),
            "",
            &mut all,
        );
        assert!(all.len() > 50, "precondition: {} keys", all.len());
        let named: Vec<_> = all
            .iter()
            .filter(|k| {
                let l = k.to_ascii_lowercase();
                l.contains("plan_mode") || l.ends_with(".plan") || l.contains("session_mode")
            })
            .collect();
        assert!(named.is_empty(), "{named:?}");
    }

    /// SECURITY: the only writer of plan mode is `set_plan_mode`, and only
    /// the frontends call it. A failure means something else, reachable from
    /// a model or a project file, can now switch the mode.
    #[test]
    fn only_the_frontends_switch_plan_mode() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let allowed = ["agent/session.rs", "agent/approval.rs", "tui/runner.rs"];
        let mut stack = vec![root.clone()];
        let mut found = Vec::new();
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") || p.ends_with("plan_mode.rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&p).unwrap();
                let head = src.split("#[cfg(test)]").next().unwrap_or("");
                if head.contains("set_plan_mode(") {
                    let rel = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
                    if !allowed.contains(&rel.as_str()) {
                        found.push(rel);
                    }
                }
            }
        }
        assert!(found.is_empty(), "set_plan_mode called from {found:?}");
    }
}
