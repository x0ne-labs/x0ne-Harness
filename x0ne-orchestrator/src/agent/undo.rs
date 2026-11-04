//! `/undo`: restore the files the agent's last write touched, from a snapshot
//! taken just before it.
//!
//! `/undo` used to run `git checkout -- .`, which also destroyed every
//! uncommitted change the agent had not made. Now the dispatcher calls
//! [`UndoJournal::checkpoint`] before each file-writing action, which snapshots
//! that action's target files through the runtime's own `CreateSnapshot`
//! (reflink when the filesystem allows), and `/undo` restores the latest one.
//! Nothing else in the tree is touched. No snapshot, no undo: git is never run.
//!
//! A `RunCommand` that is not a known read is covered by a workspace
//! checkpoint in a sandbox (`destructive::guard`); on the host, a
//! destructive one is. Not covered: files the action created — a
//! snapshot cannot hold a file that did not exist, so they are left in place
//! and named.

use serde_json::{json, Value};

use crate::bridge::Bridge;

/// Snapshot names this module owns. Anything else is the user's.
pub const UNDO_PREFIX: &str = "x0ne-undo-";
/// Undo points kept per session; older ones are deleted from the store.
pub const MAX_POINTS: usize = 5;
/// When a checkpoint fails, undo points of other sessions older than this are
/// pruned and the snapshot retried once, so crashed sessions cannot hold the
/// runtime's `max_snapshots` (which `Try` needs too).
const STALE_SECS: i64 = 12 * 3600;
const MAX_DEPTH: usize = 4;

/// The state just before one file-writing action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPoint {
    pub snapshot: String,
    pub action_type: String,
    pub targets: Vec<String>,
    /// Files that existed and were saved; `< targets.len()` means some were
    /// about to be created.
    pub files_saved: usize,
    /// Targets listed from disk (workspace checkpoint): none was created.
    pub all_existed: bool,
    /// How the runtime saved the files: "reflink", "copy" or "mixed".
    pub method: Option<String>,
}

/// What `/undo` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// No undo point: nothing was sent to the runtime. `reason` is the last
    /// checkpoint failure, if any; `unprotected` the commands that ran
    /// without an undo point.
    NothingToUndo {
        reason: Option<String>,
        unprotected: Vec<String>,
    },
    /// Restored. `created` lists targets that did not exist before the action
    /// and were left as they are.
    Restored {
        point: UndoPoint,
        files_restored: usize,
        files_failed: usize,
        created: Vec<String>,
        /// Undo points still on the stack after this one.
        remaining: usize,
    },
    /// The runtime refused or failed; the point is kept for another try.
    Failed { point: UndoPoint, message: String },
}

/// Per-session stack of undo points. Lives on `AgentState`.
#[derive(Debug, Default)]
pub struct UndoJournal {
    points: Vec<UndoPoint>,
    last_error: Option<String>,
    counter: u64,
    pruned: bool,
    unprotected: Vec<String>,
    /// Snapshots that failed this session.
    failures: u32,
    /// The runtime refused `CreateSnapshot` by policy: nothing will change
    /// mid-session, so no further snapshot is attempted.
    unavailable: bool,
}

/// Commands kept in [`UndoJournal::note_unprotected`]'s list.
const MAX_UNPROTECTED: usize = 5;

impl UndoJournal {
    pub fn points(&self) -> &[UndoPoint] {
        &self.points
    }

    /// Snapshots that failed so far this session.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// True once the runtime has refused `CreateSnapshot` by policy.
    pub fn unavailable(&self) -> bool {
        self.unavailable
    }

    /// `label` ran with no undo point before it (host, or a failed snapshot).
    pub fn note_unprotected(&mut self, label: &str) {
        self.unprotected.retain(|l| l != label);
        self.unprotected.push(label.to_string());
        if self.unprotected.len() > MAX_UNPROTECTED {
            self.unprotected.remove(0);
        }
    }

    /// Snapshot the files `action` is about to write. Never blocks the action:
    /// a failure is remembered and reported by the next `/undo`.
    pub async fn checkpoint(&mut self, action: &Value, bridge: &mut dyn Bridge) {
        let targets = write_targets(action);
        if targets.is_empty() {
            return;
        }
        let action_type = action
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let _ = self.snapshot(action_type, targets, false, bridge).await;
    }

    /// Before a destructive shell command or any project code:
    /// snapshot every top-level entry of `workspace` but the sandbox home (the
    /// session log must not be rolled back). `Ok(Some(method))` names how the
    /// runtime saved it, `Ok(None)` an empty workspace; `Err` means there is
    /// no undo point for this command.
    pub async fn checkpoint_workspace(
        &mut self,
        action: &Value,
        why: &str,
        workspace: &std::path::Path,
        bridge: &mut dyn Bridge,
    ) -> Result<Option<String>, String> {
        let targets = workspace_targets(workspace)
            .map_err(|e| format!("cannot list {}: {e}", workspace.display()))?;
        if targets.is_empty() {
            return Ok(None);
        }
        let action_type = format!(
            "{} ({why})",
            action.get("type").and_then(Value::as_str).unwrap_or("?")
        );
        self.snapshot(action_type, targets, true, bridge).await?;
        Ok(Some(
            self.points
                .last()
                .and_then(|p| p.method.clone())
                .unwrap_or_else(|| "no file to save".into()),
        ))
    }

    async fn snapshot(
        &mut self,
        action_type: String,
        targets: Vec<String>,
        all_existed: bool,
        bridge: &mut dyn Bridge,
    ) -> Result<(), String> {
        if self.unavailable {
            tracing::debug!(
                "undo checkpoint skipped: CreateSnapshot is refused by the runtime's policy"
            );
            return Err(self
                .last_error
                .clone()
                .unwrap_or_else(|| "CreateSnapshot is refused by the runtime's policy".into()));
        }
        self.counter += 1;
        let name = format!(
            "{UNDO_PREFIX}{}-{}-{}",
            chrono::Utc::now().timestamp(),
            std::process::id(),
            self.counter
        );
        let request = json!({"type": "CreateSnapshot", "name": name, "paths": targets});
        let mut sent = bridge.send_action(&request).await;
        if !ok(&sent) && !self.pruned {
            self.pruned = true;
            prune_stale(bridge).await;
            sent = bridge.send_action(&request).await;
        }
        let msg = match sent {
            Ok(r) if r.get("success").and_then(Value::as_bool) == Some(true) => {
                let files_saved = r
                    .pointer("/output/files_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let method = r
                    .pointer("/output/method")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.points.push(UndoPoint {
                    snapshot: name,
                    action_type,
                    targets,
                    files_saved,
                    all_existed,
                    method,
                });
                self.last_error = None;
                while self.points.len() > MAX_POINTS {
                    let old = self.points.remove(0);
                    delete(bridge, &old.snapshot).await;
                }
                return Ok(());
            }
            Ok(r) => {
                if refused_by_policy(&r) {
                    self.unavailable = true;
                }
                error_message(&r)
            }
            Err(e) => format!("{e:#}"),
        };
        self.failures += 1;
        // Debug, not warn: a warn was printed raw over the TUI. The
        // failure reaches the operator through `/undo` and the dispatcher.
        tracing::debug!("undo checkpoint before {action_type} failed: {msg}");
        let reason = format!("snapshot before {action_type} failed: {msg}");
        self.last_error = Some(reason.clone());
        Err(reason)
    }

    /// Drop the latest undo point unrestored: the action it was taken for did
    /// not run (the gate said no after a host checkpoint).
    pub async fn discard_last(&mut self, bridge: &mut dyn Bridge) {
        if let Some(point) = self.points.pop() {
            delete(bridge, &point.snapshot).await;
        }
    }

    /// Restore the latest undo point, then drop it from the store.
    pub async fn undo_last(&mut self, bridge: &mut dyn Bridge) -> UndoOutcome {
        let Some(point) = self.points.last().cloned() else {
            return UndoOutcome::NothingToUndo {
                reason: self.last_error.clone(),
                unprotected: self.unprotected.clone(),
            };
        };
        let request = json!({"type": "RestoreSnapshot", "name": point.snapshot});
        let r = match bridge.send_action(&request).await {
            Ok(r) => r,
            Err(e) => {
                return UndoOutcome::Failed {
                    point,
                    message: e.to_string(),
                }
            }
        };
        if r.get("success").and_then(Value::as_bool) != Some(true) {
            return UndoOutcome::Failed {
                point,
                message: error_message(&r),
            };
        }
        self.points.pop();
        delete(bridge, &point.snapshot).await;
        let restored: Vec<String> = r
            .pointer("/output/details")
            .and_then(Value::as_array)
            .map(|d| {
                d.iter()
                    .filter_map(|e| e.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let created = point
            .targets
            .iter()
            .filter(|_| !point.all_existed)
            .filter(|t| {
                let t = t.trim_start_matches("./");
                !restored
                    .iter()
                    .any(|p| p.ends_with(t) || std::path::Path::new(p).starts_with(t))
            })
            .cloned()
            .collect();
        let count = |k: &str| {
            r.pointer(&format!("/output/{k}"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        };
        UndoOutcome::Restored {
            files_restored: count("files_restored"),
            files_failed: count("files_failed"),
            point,
            created,
            remaining: self.points.len(),
        }
    }
}

/// One line for the operator, shared by the CLI and the canvas.
pub fn describe(outcome: &UndoOutcome) -> String {
    match outcome {
        UndoOutcome::NothingToUndo {
            reason,
            unprotected,
        } if !unprotected.is_empty() => {
            let why = reason
                .as_ref()
                .map(|r| format!("; {r}"))
                .unwrap_or_default();
            format!(
                "no undo point; commands that ran without one: {}{why} (git is not touched)",
                unprotected.join(", ")
            )
        }
        UndoOutcome::NothingToUndo { reason: None, .. } => {
            "nothing to undo: no file was written and no command that could write \
             was run by the agent in this session (git is not touched)"
                .into()
        }
        UndoOutcome::NothingToUndo {
            reason: Some(r), ..
        } => {
            format!("nothing to undo: {r} (git is not touched)")
        }
        UndoOutcome::Restored {
            point,
            files_restored,
            files_failed,
            created,
            remaining,
        } => {
            let mut s = format!(
                "undid {}: {} file(s) restored from {}",
                point.action_type, files_restored, point.snapshot
            );
            if *files_failed > 0 {
                s.push_str(&format!(", {files_failed} FAILED"));
            }
            if !created.is_empty() {
                s.push_str(&format!(
                    "; created by the agent and left in place: {}",
                    created.join(", ")
                ));
            }
            if *remaining > 0 {
                s.push_str(&format!(
                    "; {remaining} earlier undo point(s) left: /undo again to go further back"
                ));
            }
            s
        }
        UndoOutcome::Failed { point, message } => {
            format!(
                "undo of {} failed, nothing popped: {message}",
                point.action_type
            )
        }
    }
}

/// The snapshot traffic [`UndoJournal`] sends on the ordinary path. Test
/// bridges skip it the way they skip preflight probes: it is not the agent's
/// work. (The `ListSnapshots` of a prune only follows a failed checkpoint.)
pub fn is_bookkeeping(action: &Value) -> bool {
    match action.get("type").and_then(Value::as_str) {
        Some("CreateSnapshot" | "RestoreSnapshot" | "DeleteSnapshot") => action
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.starts_with(UNDO_PREFIX)),
        _ => false,
    }
}

/// Entries of the workspace never put in a workspace snapshot: the sandbox
/// home holds the session log and the runtime policy (`run-sandboxed.sh`).
pub const WORKSPACE_EXCLUDES: &[&str] = &[".x0ne-home"];

/// The absolute paths of `workspace`'s top-level entries, minus the excludes.
pub fn workspace_targets(workspace: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut out: Vec<String> = std::fs::read_dir(workspace)?
        .filter_map(Result::ok)
        .filter(|e| !WORKSPACE_EXCLUDES.contains(&e.file_name().to_string_lossy().as_ref()))
        .map(|e| e.path().display().to_string())
        .collect();
    out.sort();
    Ok(out)
}

/// The files `action` may write, composites included. Empty for anything else.
pub fn write_targets(action: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(action, 0, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn collect(action: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match action.get("type").and_then(Value::as_str).unwrap_or("") {
        "WriteFile" | "EditFile" | "ApplyPatch" | "MoveToTrash" => {
            out.extend(s(action, "path").filter(|p| !p.is_empty()))
        }
        "PatchSet" => {
            for p in action
                .get("patches")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.extend(s(p, "path").filter(|p| !p.is_empty()));
            }
        }
        "Pipeline" | "Plan" | "Try" | "SafeChange" | "If" => {
            for key in ["steps", "actions", "tests", "then", "else"] {
                for child in action
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    collect(child, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

fn ok(sent: &anyhow::Result<Value>) -> bool {
    matches!(sent, Ok(r) if r.get("success").and_then(Value::as_bool) == Some(true))
}

/// The runtime refused the snapshot by policy (`CreateSnapshot` disabled or
/// denied): permanent for the session, unlike a full or unwritable store.
fn refused_by_policy(r: &Value) -> bool {
    matches!(
        r.pointer("/error/code").and_then(Value::as_str),
        Some("policy_denied" | "action_disabled")
    ) || error_message(r).contains("disabled by configuration")
}

fn error_message(r: &Value) -> String {
    r.pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("the runtime refused")
        .to_string()
}

async fn delete(bridge: &mut dyn Bridge, name: &str) {
    let _ = bridge
        .send_action(&json!({"type": "DeleteSnapshot", "name": name}))
        .await;
}

/// Delete other sessions' undo points older than [`STALE_SECS`], by the time
/// in their name. Snapshots without the prefix are never touched.
async fn prune_stale(bridge: &mut dyn Bridge) {
    let Ok(list) = bridge.send_action(&json!({"type": "ListSnapshots"})).await else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let names: Vec<String> = list
        .pointer("/output/snapshots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .filter(|n| {
            n.strip_prefix(UNDO_PREFIX)
                .and_then(|rest| rest.split('-').next())
                .and_then(|t| t.parse::<i64>().ok())
                .is_some_and(|t| now - t > STALE_SECS)
        })
        .map(str::to_string)
        .collect();
    for n in names {
        delete(bridge, &n).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// Executes writes itself and snapshots with the RUNTIME's own code
    /// (`x0ne_runtime::executor::snapshot`, a dev-dependency), on a temp dir.
    pub(crate) struct SnapshotBridge {
        pub(crate) config: x0ne_runtime::config::AppConfig,
        pub(crate) seen: Arc<Mutex<Vec<Value>>>,
        /// Answer every snapshot action with the runtime's policy refusal.
        pub(crate) snapshots_disabled: bool,
    }

    impl SnapshotBridge {
        pub(crate) fn new(work: &Path, snaps: &Path) -> Self {
            let mut config = x0ne_runtime::config::AppConfig::defaults();
            config.snapshots.dir = snaps.display().to_string();
            config.policy.allowed_write_paths = vec![work.display().to_string()];
            Self {
                config,
                seen: Arc::default(),
                snapshots_disabled: false,
            }
        }
        pub(crate) fn types(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|a| a["type"].as_str().unwrap_or("").to_string())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl Bridge for SnapshotBridge {
        async fn send_action(&mut self, a: &Value) -> anyhow::Result<Value> {
            use x0ne_runtime::executor::snapshot as snap;
            self.seen.lock().unwrap().push(a.clone());
            let name = a["name"].as_str().unwrap_or("");
            let kind = a["type"].as_str().unwrap_or("");
            if self.snapshots_disabled && kind.contains("Snapshot") {
                // Shape of `ActionResult::policy_denied` for `DenyReason::ActionDisabled`.
                return Ok(json!({"success": false, "error": {
                    "kind": "policy_denied", "class": "policy_denied", "code": "policy_denied",
                    "message": "This action type is disabled by configuration. Enable it under [actions] in x0ne-runtime.toml, answer 1 or 2 at the prompt, or run `x0ne --setup`.",
                    "action_name": kind}}));
            }
            let result = match kind {
                "WriteFile" => {
                    std::fs::write(a["path"].as_str().unwrap(), a["content"].as_str().unwrap())?;
                    return Ok(json!({"success": true, "output": {}}));
                }
                "CreateSnapshot" => {
                    let paths: Vec<String> = serde_json::from_value(a["paths"].clone())?;
                    snap::action_create_snapshot(name, &paths, &self.config).await
                }
                "RestoreSnapshot" => snap::action_restore_snapshot(name, &self.config).await,
                "DeleteSnapshot" => snap::action_delete_snapshot(name, &self.config).await,
                "ListSnapshots" => snap::action_list_snapshots(&self.config).await,
                "PolicyCheck" => return Ok(json!({"success": true, "output": {}})),
                other => anyhow::bail!("unexpected action {other}"),
            };
            Ok(serde_json::to_value(result)?)
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, SnapshotBridge) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let bridge = SnapshotBridge::new(&work, &tmp.path().join("snaps"));
        (tmp, work, bridge)
    }

    fn write(path: &Path, content: &str) -> Value {
        json!({"type": "WriteFile", "path": path.display().to_string(), "content": content})
    }

    #[test]
    fn write_targets_names_what_a_write_touches_and_nothing_else() {
        assert_eq!(
            write_targets(&json!({"type": "WriteFile", "path": "a"})),
            ["a"]
        );
        assert_eq!(
            write_targets(&json!({"type": "PatchSet", "patches": [{"path": "a"}, {"path": "b"}]})),
            ["a", "b"]
        );
        let nested = json!({"type": "Plan", "steps": [
            {"type": "Try", "actions": [{"type": "EditFile", "path": "x"}]},
            {"type": "If", "check": "c", "then": [{"type": "MoveToTrash", "path": "y"}],
             "else": [{"type": "WriteFile", "path": "x"}]},
            {"type": "SafeChange", "steps": [{"type": "ApplyPatch", "path": "z"}], "tests": []},
        ]});
        assert_eq!(write_targets(&nested), ["x", "y", "z"]);
        for read in [
            json!({"type": "ReadFile", "path": "a"}),
            json!({"type": "RunCommand", "command": "ls"}),
            json!({"type": "Pipeline", "steps": [{"type": "ReadFile", "path": "a"}]}),
        ] {
            assert!(write_targets(&read).is_empty(), "{read}");
        }
    }

    #[tokio::test]
    async fn undo_restores_the_agent_write_and_keeps_changes_it_did_not_make() {
        let (_tmp, work, mut bridge) = setup();
        let edited = work.join("edited.txt");
        let other = work.join("other.txt");
        // Uncommitted user work, before and after the agent's write.
        std::fs::write(&edited, "user work in progress").unwrap();
        std::fs::write(&other, "user file").unwrap();
        let mut journal = UndoJournal::default();

        let action = write(&edited, "agent rewrite");
        journal.checkpoint(&action, &mut bridge).await;
        bridge.send_action(&action).await.unwrap();
        std::fs::write(&other, "user kept typing").unwrap();

        let outcome = journal.undo_last(&mut bridge).await;

        assert!(
            matches!(
                outcome,
                UndoOutcome::Restored {
                    files_restored: 1,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "user work in progress"
        );
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "user kept typing");
        assert!(!bridge.types().iter().any(|t| t == "RunCommand"));
        assert!(journal.points().is_empty(), "consumed");
    }

    #[tokio::test]
    async fn undo_walks_back_one_write_at_a_time() {
        let (_tmp, work, mut bridge) = setup();
        let f = work.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut journal = UndoJournal::default();
        for v in ["v1", "v2"] {
            let a = write(&f, v);
            journal.checkpoint(&a, &mut bridge).await;
            bridge.send_action(&a).await.unwrap();
        }
        journal.undo_last(&mut bridge).await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        journal.undo_last(&mut bridge).await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v0");
        assert!(matches!(
            journal.undo_last(&mut bridge).await,
            UndoOutcome::NothingToUndo { .. }
        ));
    }

    #[tokio::test]
    async fn a_file_the_agent_created_is_named_not_deleted() {
        let (_tmp, work, mut bridge) = setup();
        let new = work.join("new.txt");
        let mut journal = UndoJournal::default();
        let a = write(&new, "fresh");
        journal.checkpoint(&a, &mut bridge).await;
        bridge.send_action(&a).await.unwrap();

        let UndoOutcome::Restored { created, .. } = journal.undo_last(&mut bridge).await else {
            panic!("expected a restore");
        };
        assert_eq!(created, vec![new.display().to_string()]);
        assert!(new.exists());
    }

    #[tokio::test]
    async fn nothing_to_undo_sends_nothing() {
        let (_tmp, _work, mut bridge) = setup();
        let mut journal = UndoJournal::default();
        assert_eq!(
            journal.undo_last(&mut bridge).await,
            UndoOutcome::NothingToUndo {
                reason: None,
                unprotected: vec![]
            }
        );
        assert!(bridge.types().is_empty());
    }

    #[tokio::test]
    async fn a_failed_checkpoint_is_reported_by_undo() {
        let (_tmp, work, mut bridge) = setup();
        // Outside allowed_write_paths is fine for create; force a failure
        // with a snapshot store that cannot be created instead.
        let blocker = work.join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        bridge.config.snapshots.dir = blocker.join("snaps").display().to_string();
        let f = work.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut journal = UndoJournal::default();
        journal.checkpoint(&write(&f, "v1"), &mut bridge).await;
        let UndoOutcome::NothingToUndo {
            reason: Some(r), ..
        } = journal.undo_last(&mut bridge).await
        else {
            panic!("expected a refusal with a reason");
        };
        assert!(r.contains("WriteFile"), "{r}");
    }

    /// `CreateSnapshot` disabled in the runtime's `[actions]`: one failure
    /// reported, then the journal stops asking. Measured 2026-10-10: the
    /// warning fired on every WriteFile of a `-p` run without Snapshot.
    #[tokio::test]
    async fn a_disabled_snapshot_action_is_reported_once_and_not_retried() {
        let (_tmp, work, mut bridge) = setup();
        bridge.snapshots_disabled = true;
        let f = work.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut journal = UndoJournal::default();
        for i in 0..3 {
            journal
                .checkpoint(&write(&f, &i.to_string()), &mut bridge)
                .await;
        }
        let creates = bridge
            .types()
            .iter()
            .filter(|t| *t == "CreateSnapshot")
            .count();
        assert!(
            creates <= 2,
            "{creates} CreateSnapshot sent; at most the first try and its prune retry"
        );
        assert!(journal.unavailable());
        assert_eq!(journal.failures(), 1);
        let UndoOutcome::NothingToUndo {
            reason: Some(r), ..
        } = journal.undo_last(&mut bridge).await
        else {
            panic!("expected a refusal with a reason");
        };
        assert!(r.contains("disabled"), "{r}");
    }

    /// A failure that is not a policy refusal (store not writable) keeps
    /// being retried — it may clear — but is counted so it is warned once.
    #[tokio::test]
    async fn a_transient_checkpoint_failure_is_retried_and_counted() {
        let (_tmp, work, mut bridge) = setup();
        let blocker = work.join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        bridge.config.snapshots.dir = blocker.join("snaps").display().to_string();
        let f = work.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut journal = UndoJournal::default();
        for i in 0..3 {
            journal
                .checkpoint(&write(&f, &i.to_string()), &mut bridge)
                .await;
        }
        let creates = bridge
            .types()
            .iter()
            .filter(|t| *t == "CreateSnapshot")
            .count();
        assert!(
            creates >= 3,
            "{creates} CreateSnapshot sent; a transient failure is retried"
        );
        assert!(!journal.unavailable());
        assert_eq!(journal.failures(), 3);
    }

    #[tokio::test]
    async fn only_the_last_points_are_kept_in_the_store() {
        let (tmp, work, mut bridge) = setup();
        let f = work.join("f.txt");
        std::fs::write(&f, "v").unwrap();
        let mut journal = UndoJournal::default();
        for i in 0..MAX_POINTS + 3 {
            journal
                .checkpoint(&write(&f, &i.to_string()), &mut bridge)
                .await;
        }
        assert_eq!(journal.points().len(), MAX_POINTS);
        let on_disk = std::fs::read_dir(tmp.path().join("snaps")).unwrap().count();
        assert_eq!(on_disk, MAX_POINTS);
    }

    #[tokio::test]
    async fn a_full_store_prunes_stale_undo_points_and_keeps_user_snapshots() {
        let (tmp, work, mut bridge) = setup();
        bridge.config.snapshots.max_snapshots = 3;
        let f = work.join("f.txt");
        std::fs::write(&f, "v").unwrap();
        let old = format!(
            "{UNDO_PREFIX}{}-1-1",
            chrono::Utc::now().timestamp() - STALE_SECS - 60
        );
        let fresh = format!("{UNDO_PREFIX}{}-1-1", chrono::Utc::now().timestamp() - 60);
        for n in [old.as_str(), fresh.as_str(), "mine"] {
            let a =
                json!({"type": "CreateSnapshot", "name": n, "paths": [f.display().to_string()]});
            bridge.send_action(&a).await.unwrap();
        }
        let mut journal = UndoJournal::default();
        journal.checkpoint(&write(&f, "x"), &mut bridge).await;
        assert_eq!(journal.points().len(), 1, "retried after the prune");
        let snaps = tmp.path().join("snaps");
        assert!(!snaps.join(&old).exists());
        assert!(snaps.join(&fresh).exists());
        assert!(snaps.join("mine").exists());
    }

    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(d: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push((p.display().to_string(), std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    /// `rm -rf` in the workspace, then `/undo`, gives back every byte.
    #[tokio::test]
    async fn a_workspace_checkpoint_brings_back_what_rm_rf_deleted() {
        let (_tmp, work, mut bridge) = setup();
        std::fs::create_dir_all(work.join("x/deep")).unwrap();
        std::fs::write(work.join("x/a.txt"), "alpha").unwrap();
        std::fs::write(work.join("x/deep/b.bin"), [0u8, 1, 2, 255]).unwrap();
        std::fs::write(work.join("top.txt"), "top").unwrap();
        std::fs::create_dir_all(work.join(".x0ne-home")).unwrap();
        std::fs::write(work.join(".x0ne-home/session.jsonl"), "s1").unwrap();
        let before = tree(&work.join("x"));
        let rm = json!({"type": "RunCommand", "command": "rm", "args": ["-rf", "x"]});
        let mut journal = UndoJournal::default();

        journal
            .checkpoint_workspace(&rm, "rm -r", &work, &mut bridge)
            .await
            .expect("snapshot taken");
        std::fs::remove_dir_all(work.join("x")).unwrap();
        // The session keeps writing its log; undo must not roll it back.
        std::fs::write(work.join(".x0ne-home/session.jsonl"), "s1 s2").unwrap();

        let outcome = journal.undo_last(&mut bridge).await;
        let UndoOutcome::Restored { created, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(created.is_empty(), "{created:?}");
        assert_eq!(tree(&work.join("x")), before);
        assert_eq!(
            std::fs::read_to_string(work.join("top.txt")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(work.join(".x0ne-home/session.jsonl")).unwrap(),
            "s1 s2"
        );
        assert!(
            describe(&outcome).contains("rm -r"),
            "{}",
            describe(&outcome)
        );
    }

    /// Session of 2026-10-06: `rm` emptied `proj`, then a second destructive
    /// step ran. The first `/undo` named `proj` "created by the agent" and did
    /// not say the user's files were one more `/undo` away.
    #[tokio::test]
    async fn a_second_destructive_step_says_more_undo_is_needed_and_blames_nothing() {
        let (_tmp, work, mut bridge) = setup();
        std::fs::create_dir_all(work.join("proj")).unwrap();
        std::fs::write(work.join("proj/notes.txt"), "keep me").unwrap();
        let rm = json!({"type": "RunCommand", "command": "rm", "args": ["-rf", "proj/notes.txt"]});
        let mut journal = UndoJournal::default();
        journal
            .checkpoint_workspace(&rm, "rm -r", &work, &mut bridge)
            .await
            .unwrap();
        std::fs::remove_file(work.join("proj/notes.txt")).unwrap();
        journal
            .checkpoint_workspace(&rm, "rm -r", &work, &mut bridge)
            .await
            .unwrap();

        let first = journal.undo_last(&mut bridge).await;
        let UndoOutcome::Restored {
            created, remaining, ..
        } = &first
        else {
            panic!("{first:?}");
        };
        assert!(created.is_empty(), "proj existed before: {created:?}");
        assert_eq!(*remaining, 1);
        assert!(
            describe(&first).contains("/undo again"),
            "{}",
            describe(&first)
        );
        assert!(!work.join("proj/notes.txt").exists());

        let second = journal.undo_last(&mut bridge).await;
        assert!(
            matches!(second, UndoOutcome::Restored { remaining: 0, .. }),
            "{second:?}"
        );
        assert!(!describe(&second).contains("/undo again"));
        assert_eq!(
            std::fs::read_to_string(work.join("proj/notes.txt")).unwrap(),
            "keep me"
        );
    }

    /// Hostile restore (audit 2026-11-02): before `/undo`, the agent turns a
    /// snapshotted directory into a symlink that leaves the project. The
    /// restore must neither write through it nor clean up behind it.
    #[tokio::test]
    async fn undo_never_writes_through_a_directory_swapped_for_a_symlink() {
        let (tmp, work, mut bridge) = setup();
        std::fs::create_dir_all(work.join("x")).unwrap();
        std::fs::write(work.join("x/a.txt"), "alpha").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.txt"), "outside a").unwrap();
        std::fs::write(outside.join("victim.txt"), "victim").unwrap();
        let before = tree(&outside);
        let rm = json!({"type": "RunCommand", "command": "rm", "args": ["-rf", "x"]});
        let mut journal = UndoJournal::default();
        journal
            .checkpoint_workspace(&rm, "rm -r", &work, &mut bridge)
            .await
            .expect("snapshot taken");
        std::fs::remove_dir_all(work.join("x")).unwrap();
        std::os::unix::fs::symlink(&outside, work.join("x")).unwrap();

        let outcome = journal.undo_last(&mut bridge).await;

        assert_eq!(tree(&outside), before, "restore wrote outside: {outcome:?}");
        assert!(matches!(outcome, UndoOutcome::Failed { .. }), "{outcome:?}");
        assert!(describe(&outcome).contains("outside allowed_write_paths"));
    }

    /// Same, with the file itself swapped for a symlink to a file outside.
    #[tokio::test]
    async fn undo_never_writes_through_a_file_swapped_for_a_symlink() {
        let (tmp, work, mut bridge) = setup();
        let f = work.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("target.txt"), "outside").unwrap();
        let before = tree(&outside);
        let mut journal = UndoJournal::default();
        let a = write(&f, "v1");
        journal.checkpoint(&a, &mut bridge).await;
        bridge.send_action(&a).await.unwrap();
        std::fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink(outside.join("target.txt"), &f).unwrap();

        let outcome = journal.undo_last(&mut bridge).await;

        assert_eq!(tree(&outside), before, "restore wrote outside: {outcome:?}");
        assert!(matches!(outcome, UndoOutcome::Failed { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_workspace_too_big_to_snapshot_is_an_error_not_a_silent_pass() {
        let (_tmp, work, mut bridge) = setup();
        bridge.config.snapshots.max_snapshot_size_mb = 0;
        std::fs::write(work.join("a.txt"), "alpha").unwrap();
        let rm = json!({"type": "RunCommand", "command": "rm", "args": ["-rf", "."]});
        let mut journal = UndoJournal::default();
        let err = journal
            .checkpoint_workspace(&rm, "rm -r", &work, &mut bridge)
            .await
            .expect_err("no snapshot, no green light");
        assert!(err.contains("size limit"), "{err}");
        assert!(journal.points().is_empty());
    }
}
