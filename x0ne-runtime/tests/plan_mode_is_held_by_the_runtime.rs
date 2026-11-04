//! Plan mode is the runtime's, not the prompt's.
//!
//! The orchestrator sends `{"control":"SessionMode","mode":"plan"}` when the
//! human types `/plan`. From then on the runtime itself refuses every action
//! whose class touches the host, as `[policy] mode = "readonly"` does, whoever
//! sends it. These tests talk to the real binary over stdio and skip the
//! orchestrator entirely: they are the "orchestrator bypassed" case.

mod support;

use serde_json::json;
use support::Runtime;

fn plan(rt: &mut Runtime) {
    let r = rt.send(json!({"control": "SessionMode", "mode": "plan"}));
    assert_eq!(r["success"], true, "plan mode refused: {r}");
    assert_eq!(r["action_type"], "SessionMode", "{r}");
    assert_eq!(r["output"]["data"]["mode"], "plan", "{r}");
}

fn assert_plan_refusal(r: &serde_json::Value, what: &str) {
    assert_ne!(r["success"], true, "{what} ran in plan mode: {r}");
    let msg = r["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("plan mode"),
        "{what}: not a plan-mode refusal: {r}"
    );
    assert!(
        r["error"]["liftable"].is_null(),
        "{what}: a plan-mode refusal offered a grant: {r}"
    );
}

/// SECURITY: an action sent straight to the runtime in plan mode does not
/// write, run or delete. A failure means plan mode is a sentence in the prompt
/// and anything that reaches the runtime's stdin still changes the host.
#[test]
fn a_direct_call_to_the_runtime_in_plan_mode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("keep.txt"), "kept").unwrap();
    let mut rt = Runtime::start(d, &["echo", "touch"]);
    plan(&mut rt);

    let write = d.join("written.txt");
    let marker = d.join("touched");
    let attempts = [
        (
            "WriteFile",
            json!({"type": "WriteFile", "path": write.display().to_string(), "content": "x"}),
        ),
        (
            "RunCommand",
            json!({"type": "RunCommand", "command": "touch", "args": [marker.display().to_string()]}),
        ),
        (
            "MoveToTrash",
            json!({"type": "MoveToTrash", "path": d.join("keep.txt").display().to_string()}),
        ),
    ];
    for (what, action) in attempts {
        let r = rt.send(action);
        assert_plan_refusal(&r, what);
    }
    let pipeline = rt.send(json!({"type": "Pipeline", "steps": [
        {"type": "WriteFile", "path": write.display().to_string(), "content": "x"}
    ]}));
    assert_ne!(
        pipeline["success"], true,
        "a Pipeline wrote in plan mode: {pipeline}"
    );

    assert!(!write.exists(), "a file was written in plan mode");
    assert!(!marker.exists(), "a command ran in plan mode");
    assert_eq!(std::fs::read_to_string(d.join("keep.txt")).unwrap(), "kept");

    // Reading is what plan mode is for.
    let read =
        rt.send(json!({"type": "ReadFile", "path": d.join("keep.txt").display().to_string()}));
    assert_eq!(
        read["success"], true,
        "a read was refused in plan mode: {read}"
    );
}

/// SECURITY: a session grant given before `/plan` does not open a write in
/// plan mode. A failure means "allow touch for the session" outlives the
/// switch to plan mode.
#[test]
fn a_grant_given_before_plan_mode_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("touched");
    let mut rt = Runtime::start(dir.path(), &["echo"]);
    let g = rt.send(json!({"control": "PolicyGrant", "kind": "command", "value": "touch"}));
    assert_eq!(g["success"], true, "{g}");
    plan(&mut rt);
    let r = rt.send(
        json!({"type": "RunCommand", "command": "touch", "args": [marker.display().to_string()]}),
    );
    assert_plan_refusal(&r, "touch granted before plan mode");
    assert!(!marker.exists());
}

/// SECURITY: nothing shaped like an action leaves plan mode. A failure means
/// the model can end plan mode by writing JSON.
#[test]
fn an_action_cannot_leave_plan_mode() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("f"), "x").unwrap();
    let mut rt = Runtime::start(d, &["echo"]);
    plan(&mut rt);
    for line in [
        json!({"type": "SessionMode", "mode": "build"}),
        json!({"type": "ReadFile", "path": d.join("f").display().to_string(),
               "control": "SessionMode", "mode": "build"}),
        json!({"type": "PolicyCheck", "action": {"control": "SessionMode", "mode": "build"}}),
    ] {
        let r = rt.send(line.clone());
        assert_ne!(r["success"], true, "{line} was accepted in plan mode: {r}");
    }
    let write = d.join("after.txt");
    let r =
        rt.send(json!({"type": "WriteFile", "path": write.display().to_string(), "content": "x"}));
    assert_plan_refusal(&r, "WriteFile after the attempts");
    assert!(!write.exists());
}

/// `/build` gives the configured policy back, unchanged.
#[test]
fn build_mode_gives_the_policy_back() {
    let dir = tempfile::tempdir().unwrap();
    let write = dir.path().join("w.txt");
    let mut rt = Runtime::start(dir.path(), &["echo"]);
    plan(&mut rt);
    let r = rt.send(json!({"control": "SessionMode", "mode": "build"}));
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["output"]["data"]["mode"], "build", "{r}");
    let r =
        rt.send(json!({"type": "WriteFile", "path": write.display().to_string(), "content": "x"}));
    assert_eq!(r["success"], true, "{r}");
    assert!(write.exists());
}

/// A mode the runtime does not know, or a field it does not expect, changes
/// nothing.
#[test]
fn an_unknown_mode_is_refused_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let write = dir.path().join("w.txt");
    let mut rt = Runtime::start(dir.path(), &["echo"]);
    plan(&mut rt);
    for line in [
        json!({"control": "SessionMode", "mode": "yolo"}),
        json!({"control": "SessionMode", "mode": "build", "persist": true}),
        json!({"control": "SessionMode"}),
    ] {
        let r = rt.send(line.clone());
        assert_ne!(r["success"], true, "{line} accepted: {r}");
    }
    let r =
        rt.send(json!({"type": "WriteFile", "path": write.display().to_string(), "content": "x"}));
    assert_plan_refusal(&r, "WriteFile after a refused switch");
}
