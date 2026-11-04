//! `Simulate` — dry-run an Action (or nested Pipeline / Try / SafeChange / If)
//! without executing it. Returns a structured projection: per-step
//! classification + policy verdict + aggregated side-effect summary.
//!
//! Zero mutations. Completes in microseconds even on deep pipelines.

use crate::action::Action;
use crate::config::AppConfig;
use crate::policy::{
    is_destructive_action, is_network_action, PolicyEffect, PolicyEngine, SideEffectClass,
};
use crate::result::{ActionOutput, ActionResult, SideEffectSummary, SimulatedStep};

/// Run a dry-run projection of `target` against `policy` and `config`.
/// No side effects; returns an `ActionResult::ok` with `SimulationResult`.
pub fn simulate(
    target: &Action,
    explain: bool,
    policy: &PolicyEngine,
    _config: &AppConfig,
) -> ActionResult {
    let mut walker = Walker {
        steps: Vec::new(),
        side_effects: SideEffectSummary::default(),
        explain,
        policy,
    };

    walker.walk(target, "root");

    // Aggregate verdict: would the policy engine allow the *root* action?
    // PolicyEngine::evaluate already recurses into Pipeline/Try/SafeChange/If
    // and surfaces the first denial — so this is the single source of truth.
    let (would_execute, blocking_reason) = match policy.evaluate(target) {
        PolicyEffect::Allow => (true, None),
        PolicyEffect::RequireConfirmation(msg) => {
            (false, Some(format!("requires_confirmation: {}", msg)))
        }
        PolicyEffect::Deny(reason) => (false, Some(reason.message())),
    };

    // Record indices of blocked steps in the side_effects summary, for at-a-glance UI.
    for (idx, step) in walker.steps.iter().enumerate() {
        if step.verdict != "allow" {
            walker.side_effects.blocked_step_indices.push(idx);
        }
    }

    let target_type = target.type_name().to_string();
    let step_count = walker.steps.len();

    ActionResult::ok(
        "Simulate",
        ActionOutput::SimulationResult {
            target_type,
            would_execute,
            blocking_reason,
            step_count,
            steps: walker.steps,
            side_effects: walker.side_effects,
        },
        0,
    )
}

struct Walker<'a> {
    steps: Vec<SimulatedStep>,
    side_effects: SideEffectSummary,
    explain: bool,
    policy: &'a PolicyEngine,
}

impl<'a> Walker<'a> {
    fn walk(&mut self, action: &Action, path: &str) {
        match action {
            // Composites: recurse, do NOT add a leaf step for the wrapper itself.
            Action::Pipeline { steps, .. } => {
                for (i, step) in steps.iter().enumerate() {
                    self.walk(step, &format!("{}/Pipeline[{}]", path, i));
                }
            }
            Action::Try { actions, .. } => {
                for (i, step) in actions.iter().enumerate() {
                    self.walk(step, &format!("{}/Try[{}]", path, i));
                }
            }
            Action::SafeChange { steps, tests, .. } => {
                for (i, step) in steps.iter().enumerate() {
                    self.walk(step, &format!("{}/SafeChange.steps[{}]", path, i));
                }
                for (i, step) in tests.iter().enumerate() {
                    self.walk(step, &format!("{}/SafeChange.tests[{}]", path, i));
                }
            }
            Action::If {
                then, else_branch, ..
            } => {
                // Both branches contribute — the policy engine preflights both,
                // so the simulation must too. Mark with branch tag in the path
                // so callers can tell which side a step came from.
                for (i, step) in then.iter().enumerate() {
                    self.walk(step, &format!("{}/If.then[{}]", path, i));
                }
                for (i, step) in else_branch.iter().enumerate() {
                    self.walk(step, &format!("{}/If.else[{}]", path, i));
                }
            }
            // Simulate-of-Simulate is silly but safe: recurse on the inner target.
            Action::Simulate { target, .. } => {
                self.walk(target, &format!("{}/Simulate.target", path));
            }
            // RenderTemplate could in theory invoke many sub-tools at runtime,
            // but it's not statically introspectable — record it as a single
            // composite leaf the agent should be aware of.
            Action::RenderTemplate { .. } => {
                self.add_leaf(action, path);
            }
            // Leaf — classify, ask policy, record.
            _ => {
                self.add_leaf(action, path);
            }
        }
    }

    fn add_leaf(&mut self, action: &Action, path: &str) {
        let class = self.policy.classify(action);
        let (verdict, reason) = match self.policy.evaluate(action) {
            PolicyEffect::Allow => ("allow", None),
            PolicyEffect::RequireConfirmation(msg) => ("require_confirmation", Some(msg)),
            PolicyEffect::Deny(reason) => ("deny", Some(reason.message())),
        };

        let target = guess_target(action);

        // Accumulate aggregate side-effects.
        match class {
            SideEffectClass::ReadOnly => {
                if let Some(t) = &target {
                    self.side_effects.paths_read.push(t.clone());
                }
            }
            SideEffectClass::FsWrite => {
                if let Some(t) = &target {
                    self.side_effects.paths_written.push(t.clone());
                }
            }
            SideEffectClass::ProcessSpawn => {
                if let Some(t) = &target {
                    self.side_effects.commands_spawned.push(t.clone());
                }
            }
            SideEffectClass::Network => {
                if let Some(t) = &target {
                    self.side_effects.hosts_contacted.push(t.clone());
                }
            }
            SideEffectClass::Desktop => {
                self.side_effects.desktop_actions += 1;
            }
            SideEffectClass::Destructive => {
                if let Some(t) = &target {
                    self.side_effects.paths_destroyed.push(t.clone());
                }
            }
            SideEffectClass::Composite | SideEffectClass::Meta => {}
        }

        // Suppress per-step summary text unless the caller asked for it —
        // saves a fair bit of payload when the agent is just polling.
        let summary = if self.explain {
            Some(summarize(action, class))
        } else {
            None
        };

        self.steps.push(SimulatedStep {
            path: path.to_string(),
            action_type: action.type_name().to_string(),
            class: class.as_str().to_string(),
            summary,
            verdict: verdict.to_string(),
            verdict_reason: reason,
            target,
        });

        // Unused at-the-callsite warning silencers — but actually used for
        // future hooks that may want to slice by class.
        let _ = is_network_action;
        let _ = is_destructive_action;
    }
}

/// Best-effort identification of the resource each action would touch.
/// Returns `None` for actions whose target is dynamic or doesn't apply.
///
/// Deliberately conservative — only variants whose field shape we're sure of.
/// `None` is a perfectly fine answer; the class still tells the agent what
/// kind of side effect to expect.
fn guess_target(action: &Action) -> Option<String> {
    match action {
        // Filesystem reads
        Action::ReadFile { path, .. } => Some(path.clone()),
        Action::ReadFiles { paths, .. } | Action::StatFiles { paths, .. } => Some(paths.join(", ")),

        // Filesystem writes
        Action::WriteFile { path, .. }
        | Action::EditFile { path, .. }
        | Action::ApplyPatch { path, .. }
        | Action::MoveToTrash { path } => Some(path.clone()),

        // Process spawn
        Action::RunCommand { command, args, .. } => Some({
            let mut s = command.clone();
            if !args.is_empty() {
                s.push(' ');
                s.push_str(&args.join(" "));
            }
            s
        }),
        Action::RunInSandbox { image, command, .. } => Some({
            if command.is_empty() {
                image.clone()
            } else {
                format!("{} :: {}", image, command.join(" "))
            }
        }),

        // Network
        Action::HttpRequest { url, .. } | Action::FetchUrl { url, .. } => Some(url.clone()),
        Action::DnsLookup { hostname } => Some(hostname.clone()),
        Action::PortCheck { host, port, .. } => Some(format!("{}:{}", host, port)),
        Action::SshExec { host, command, .. } => Some(format!("{} :: {}", host, command)),
        Action::LlmCall { model, .. } => Some(model.clone()),

        // Snapshots
        Action::CreateSnapshot { name, .. }
        | Action::RestoreSnapshot { name }
        | Action::DeleteSnapshot { name } => Some(name.clone()),

        // Artifacts
        Action::ArtifactStop { id } | Action::ArtifactDestroy { id, .. } => Some(id.clone()),
        Action::ArtifactSave { id, .. } | Action::ArtifactServe { id, .. } => Some(id.clone()),

        // Search / inspection
        Action::SearchCode { pattern, .. } | Action::SearchAndRead { pattern, .. } => {
            Some(pattern.clone())
        }
        Action::FileInfo { path } | Action::Checksum { path, .. } => Some(path.clone()),

        // Memory
        Action::Remember { key, .. } | Action::Recall { key, .. } | Action::Forget { key, .. } => {
            Some(key.clone())
        }

        // Misc
        Action::CheckHandle { handle } | Action::StopHandle { handle } => Some(handle.clone()),

        _ => None,
    }
}

/// Free-form "would do" sentence — one line per leaf.
fn summarize(action: &Action, class: SideEffectClass) -> String {
    let t = guess_target(action).unwrap_or_else(|| "<no target>".into());
    let verb = match class {
        SideEffectClass::ReadOnly => "would read",
        SideEffectClass::FsWrite => "would write",
        SideEffectClass::ProcessSpawn => "would spawn",
        SideEffectClass::Network => "would contact",
        SideEffectClass::Desktop => "would drive desktop:",
        SideEffectClass::Destructive => "would destroy",
        SideEffectClass::Composite => "composite —",
        SideEffectClass::Meta => "meta —",
    };
    format!("{} {} ({})", verb, t, action.type_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::CompositeStore;
    use crate::config::AppConfig;
    use serde_json::Value;
    use std::collections::HashMap;

    fn permissive_engine() -> (PolicyEngine, AppConfig) {
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = String::new();
        cfg.policy.default_effect = "allow".into();
        cfg.policy.deny_path_patterns = vec![];
        cfg.policy.allowed_commands = vec!["echo".into(), "ls".into()];
        cfg.policy.allowed_read_paths = vec!["/tmp/".into()];
        cfg.policy.allowed_write_paths = vec!["/tmp/".into()];
        // Enable a permissive action map so writes pass the per-action gate.
        let mut enabled: HashMap<String, bool> = HashMap::new();
        for k in [
            "ReadFile",
            "WriteFile",
            "RunCommand",
            "HttpRequest",
            "Pipeline",
            "Try",
            "SafeChange",
            "MoveToTrash",
            "Simulate",
            "ListDir",
        ] {
            enabled.insert(k.into(), true);
        }
        cfg.actions = enabled.clone();
        let engine = PolicyEngine::new(cfg.clone(), enabled);
        (engine, cfg)
    }

    fn read(path: &str) -> Action {
        Action::ReadFile {
            path: path.into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }
    }
    fn write(path: &str) -> Action {
        Action::WriteFile {
            path: path.into(),
            content: "hi".into(),
        }
    }

    #[test]
    fn single_read_classifies_as_read_only_and_allowed() {
        let (engine, cfg) = permissive_engine();
        let r = simulate(&read("/tmp/foo.txt"), true, &engine, &cfg);
        assert!(r.success);
        if let Some(ActionOutput::SimulationResult {
            target_type,
            would_execute,
            step_count,
            steps,
            side_effects,
            ..
        }) = r.output
        {
            assert_eq!(target_type, "ReadFile");
            assert!(would_execute);
            assert_eq!(step_count, 1);
            assert_eq!(steps[0].class, "read_only");
            assert_eq!(steps[0].verdict, "allow");
            assert_eq!(steps[0].target.as_deref(), Some("/tmp/foo.txt"));
            assert_eq!(side_effects.paths_read, vec!["/tmp/foo.txt"]);
            assert!(side_effects.paths_written.is_empty());
        } else {
            panic!("expected SimulationResult");
        }
    }

    #[test]
    fn pipeline_walks_and_classifies_each_step() {
        let (engine, cfg) = permissive_engine();
        let pipe = Action::Pipeline {
            steps: vec![
                read("/tmp/in.txt"),
                write("/tmp/out.txt"),
                Action::RunCommand {
                    command: "echo".into(),
                    args: vec!["hello".into()],
                    cwd: None,
                    env: std::collections::HashMap::new(),
                    timeout_secs: None,
                    stream: None,
                },
            ],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let r = simulate(&pipe, true, &engine, &cfg);
        if let Some(ActionOutput::SimulationResult {
            step_count,
            steps,
            side_effects,
            would_execute,
            ..
        }) = r.output
        {
            assert!(would_execute);
            assert_eq!(step_count, 3);
            assert_eq!(steps[0].class, "read_only");
            assert_eq!(steps[1].class, "fs_write");
            assert_eq!(steps[2].class, "process_spawn");
            assert!(steps[0].path.starts_with("root/Pipeline[0]"));
            assert!(steps[2]
                .summary
                .as_deref()
                .unwrap_or("")
                .contains("echo hello"));
            assert_eq!(side_effects.paths_read.len(), 1);
            assert_eq!(side_effects.paths_written.len(), 1);
            assert_eq!(side_effects.commands_spawned.len(), 1);
        } else {
            panic!("expected SimulationResult");
        }
    }

    #[test]
    fn deny_propagates_to_aggregate_verdict() {
        // Build a policy that allows ReadFile but denies WriteFile.
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = String::new();
        cfg.policy.default_effect = "allow".into();
        // Grant the paths explicitly: this test is about the per-action enable
        // map denying WriteFile, not about path confinement (empty roots would
        // now deny step 0 as well and mask what is being asserted).
        cfg.policy.allowed_read_paths = vec!["/tmp".into()];
        cfg.policy.allowed_write_paths = vec!["/tmp".into()];
        let mut enabled: HashMap<String, bool> = HashMap::new();
        enabled.insert("ReadFile".into(), true);
        enabled.insert("WriteFile".into(), false); // <- denied
        enabled.insert("Pipeline".into(), true);
        cfg.actions = enabled.clone();
        let engine = PolicyEngine::new(cfg.clone(), enabled);

        let pipe = Action::Pipeline {
            steps: vec![read("/tmp/x"), write("/tmp/y")],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let r = simulate(&pipe, false, &engine, &cfg);
        if let Some(ActionOutput::SimulationResult {
            would_execute,
            blocking_reason,
            side_effects,
            steps,
            ..
        }) = r.output
        {
            assert!(!would_execute, "denied write should block aggregate");
            assert!(blocking_reason.is_some());
            assert_eq!(steps[0].verdict, "allow");
            assert_eq!(steps[1].verdict, "deny");
            assert_eq!(side_effects.blocked_step_indices, vec![1usize]);
        } else {
            panic!("expected SimulationResult");
        }
    }

    #[test]
    fn destructive_ops_land_in_paths_destroyed() {
        let (engine, cfg) = permissive_engine();
        let act = Action::MoveToTrash {
            path: "/tmp/old.txt".into(),
        };
        let r = simulate(&act, false, &engine, &cfg);
        if let Some(ActionOutput::SimulationResult {
            steps,
            side_effects,
            ..
        }) = r.output
        {
            assert_eq!(steps[0].class, "destructive");
            assert_eq!(side_effects.paths_destroyed, vec!["/tmp/old.txt"]);
        } else {
            panic!("expected SimulationResult")
        }
    }

    #[test]
    fn nested_safe_change_walks_both_steps_and_tests() {
        let (engine, cfg) = permissive_engine();
        let sc = Action::SafeChange {
            steps: vec![write("/tmp/a"), write("/tmp/b")],
            tests: vec![read("/tmp/a"), read("/tmp/b")],
            rollback_on_fail: true,
            label: None,
            keep_snapshot: false,
        };
        let r = simulate(&sc, false, &engine, &cfg);
        if let Some(ActionOutput::SimulationResult {
            step_count, steps, ..
        }) = r.output
        {
            assert_eq!(step_count, 4);
            assert!(steps.iter().any(|s| s.path.contains("SafeChange.steps[0]")));
            assert!(steps.iter().any(|s| s.path.contains("SafeChange.tests[1]")));
        } else {
            panic!("expected SimulationResult")
        }
    }

    #[test]
    fn explain_false_suppresses_summaries() {
        let (engine, cfg) = permissive_engine();
        let r = simulate(&read("/tmp/x"), false, &engine, &cfg);
        if let Some(ActionOutput::SimulationResult { steps, .. }) = r.output {
            assert!(steps[0].summary.is_none());
        } else {
            panic!("expected SimulationResult")
        }
        // Sanity: ensure CompositeStore type still imports OK so tests
        // catch a future API break locally.
        let _ = CompositeStore::new(&cfg);
        let _: Value = Value::Null;
    }
}
