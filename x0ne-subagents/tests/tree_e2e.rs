//! End-to-end: a scripted LLM drives a recursive tree (depth 5, width 3).
//! No real API. Proves: total spend <= root budget, caps never widen,
//! cascade cancel, loop cut, host/sibling isolation at depth 5.

mod common;

use std::time::Duration;

use common::*;
use x0ne_subagents::budget::Budget;
use x0ne_subagents::caps::{CapSet, Capability};
use x0ne_subagents::llm::{Decision, Observation, ScriptedLlm};
use x0ne_subagents::node::{NodeState, NodeView};
use x0ne_subagents::tree::TreeConfig;

fn cfg() -> TreeConfig {
    TreeConfig {
        max_children_per_node: 3,
        max_concurrent_sandboxes: 4,
        ..TreeConfig::default()
    }
}

/// Every node writes `<id>.txt`; non-leaf nodes spawn three children, the
/// first of which recurses. Depth 5 is reached along root/1/1/1/1/1.
fn recursive_script(view: &NodeView, obs: &[Observation]) -> Decision {
    let t = turn(obs);
    let me = view.id.to_string().replace('/', "_");
    let recurse = view.depth < 5 && (view.depth == 0 || view.statement.starts_with("recurse"));
    match t {
        0 => Decision::Write {
            path: format!("{me}.txt"),
            content: format!("written by {}\n", view.id),
        },
        1 if recurse => Decision::Spawn(child_spec(view, &format!("recurse below {}", view.id), 4)),
        2 if recurse => Decision::Spawn(child_spec(view, &format!("leaf a of {}", view.id), 4)),
        3 if recurse => Decision::Spawn(child_spec(view, &format!("leaf b of {}", view.id), 4)),
        4 if recurse => Decision::Wait,
        _ => report(&format!("{} done", view.id)),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_five_width_three_is_bounded_monotone_and_merged() {
    let fx = Fixture::new();
    let tree = fx.tree(ScriptedLlm::new(recursive_script), cfg());
    let root_budget = Budget {
        tokens: 10_000_000,
        micro_usd: 10_000_000,
        ms: 600_000,
        actions: 16384,
    };
    let mut spec = root_spec("build the thing", 16384);
    spec.budget = root_budget;
    let outcome = tree.run_root(spec).await.expect("root ran");
    assert!(outcome.verified, "root must be verified: {outcome:?}");

    let nodes = tree.snapshot();
    assert_eq!(
        nodes.len(),
        16,
        "root + 5 levels x 3 children: {:?}",
        nodes.keys()
    );
    assert!(nodes.contains_key(&id("root/1/1/1/1/1")));
    assert_eq!(nodes[&id("root/1/1/1/1/1")].depth, 5);
    for (nid, n) in &nodes {
        assert_eq!(n.state, NodeState::Verified, "{nid} is {:?}", n.state);
    }

    // Budget: total spent across the tree never exceeds the root's allowance,
    // and nothing leaks (conservation: initial = remaining + spent + lent).
    let spent = tree.total_spent();
    assert!(
        spent.fits_in(&root_budget),
        "spent {spent:?} > root {root_budget:?}"
    );
    let root = &nodes[&id("root")];
    assert_eq!(root.ledger.initial, root_budget);
    assert_eq!(root.ledger.initial, root.ledger.remaining.plus(&spent));
    for n in nodes.values() {
        assert!(
            n.ledger.spent.fits_in(&n.ledger.initial),
            "{} overspent",
            n.id
        );
    }

    // Capabilities: every child's set is a subset of its parent's.
    assert!(tree.caps_monotone());

    // Merge went all the way up: the root clone holds all 16 files.
    let root_dir = tree.root_workdir().unwrap();
    for nid in nodes.keys() {
        let f = root_dir.join(format!("{}.txt", nid.to_string().replace('/', "_")));
        assert!(f.exists(), "missing merged file {}", f.display());
    }
    // The host workspace is untouched until the explicit root export.
    assert!(!fx.host_workspace.join("root.txt").exists());
    let export = tree.export_root().expect("export");
    assert_eq!(export.applied.len(), 16);
    assert!(fx.host_workspace.join("root_1_1_1_1_1.txt").exists());

    // Journals: one chain per node, each verifiable, each linked to its parent.
    for nid in nodes.keys() {
        let entries = tree.journal().verify_chain(nid).expect("chain intact");
        assert!(entries > 0);
        if nid.depth() > 0 {
            let first = tree
                .journal()
                .read(nid)
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(first.parent.as_ref(), nid.parent().as_ref());
        }
    }
}

/// No depth cap at all: every node spawns three children with a quarter of
/// its remaining budget. The tree stops because budgets are debited, not
/// because anyone counted levels. Total spend stays under the root's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbounded_fanout_terminates_by_budget_alone() {
    let fx = Fixture::new();
    let script = |view: &NodeView, obs: &[Observation]| -> Decision {
        match turn(obs) {
            0..=2 => Decision::Spawn(child_spec(
                view,
                &format!("fan out from {} #{}", view.id, turn(obs)),
                4,
            )),
            3 => Decision::Wait,
            _ => report("fanned"),
        }
    };
    let mut c = cfg();
    c.max_concurrent_sandboxes = 8;
    let tree = fx.tree(ScriptedLlm::new(script), c);
    let root_budget = Budget {
        tokens: 10_000_000,
        micro_usd: 10_000_000,
        ms: 600_000,
        actions: 4096,
    };
    let mut spec = root_spec("fan out", 4096);
    spec.budget = root_budget;
    let outcome = tokio::time::timeout(Duration::from_secs(120), tree.run_root(spec))
        .await
        .expect("terminates")
        .unwrap();
    assert!(outcome.verified);
    let nodes = tree.snapshot();
    let max_depth = nodes.values().map(|n| n.depth).max().unwrap();
    eprintln!(
        "fanout: {} nodes, max depth {max_depth}, spent {:?}",
        nodes.len(),
        tree.total_spent()
    );
    assert!(
        nodes.len() > 100 && max_depth >= 4,
        "{} nodes, depth {max_depth}",
        nodes.len()
    );
    assert!(nodes
        .values()
        .any(|n| matches!(&n.state, NodeState::Failed(r) if r.contains("budget"))));
    let spent = tree.total_spent();
    assert!(spent.fits_in(&root_budget), "{spent:?}");
    let root = &nodes[&id("root")];
    assert_eq!(root.ledger.initial, root.ledger.remaining.plus(&spent));
    assert_eq!(root.ledger.lent, Budget::default());
    assert!(tree.caps_monotone());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_cascades_to_a_whole_subtree_only() {
    let fx = Fixture::new();
    // root/1 spawns three busy children that loop on `sleep`; root/2 is quick.
    let script = |view: &NodeView, obs: &[Observation]| -> Decision {
        let t = turn(obs);
        match (view.id.to_string().as_str(), t) {
            ("root", 0) => Decision::Spawn(child_spec(view, "busy subtree", 3)),
            ("root", 1) => Decision::Spawn(child_spec(view, "quick sibling", 3)),
            ("root", 2) => Decision::Wait,
            ("root", _) => report("root done"),
            ("root/1", 0..=2) => Decision::Spawn(child_spec(view, &format!("busy worker {t}"), 4)),
            ("root/1", 3) => Decision::Wait,
            ("root/1", _) => report("busy done"),
            ("root/2", _) => report("quick done"),
            _ => Decision::Exec {
                cmd: "sleep 0.2".to_string(),
            },
        }
    };
    let tree = fx.tree(ScriptedLlm::new(script), cfg());
    let t2 = tree.clone();
    let runner = tokio::spawn(async move { t2.run_root(root_spec("cancel me", 4096)).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    tree.cancel(&id("root/1")).expect("cancel");
    let outcome = tokio::time::timeout(Duration::from_secs(10), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(outcome.verified);
    let nodes = tree.snapshot();
    for n in ["root/1", "root/1/1", "root/1/2", "root/1/3"] {
        assert_eq!(nodes[&id(n)].state, NodeState::Cancelled, "{n}");
    }
    assert_eq!(nodes[&id("root/2")].state, NodeState::Verified);
    assert_eq!(nodes[&id("root")].state, NodeState::Verified);
    let ev = tree.journal().read(&id("root/1/2")).unwrap();
    assert!(ev.iter().any(|e| e.event.kind == "cancelled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respawned_statement_is_a_loop_and_is_cut() {
    let fx = Fixture::new();
    let script = |view: &NodeView, obs: &[Observation]| -> Decision {
        let t = turn(obs);
        match (view.depth, t) {
            // root asks a child to do the exact same thing root was asked.
            (0, 0) => Decision::Spawn(child_spec(view, &view.statement, 2)),
            (0, 1) => Decision::Spawn(child_spec(view, "something else", 2)),
            (0, 2) => Decision::Wait,
            (0, _) => report("root done"),
            // the grandchild re-asks the grandparent's statement, normalised.
            (1, 0) => Decision::Spawn(child_spec(view, "  SOMETHING   else ", 2)),
            (1, _) => report("child done"),
            _ => report("leaf"),
        }
    };
    let tree = fx.tree(ScriptedLlm::new(script), cfg());
    let outcome = tree.run_root(root_spec("Refactor auth", 64)).await.unwrap();
    assert!(outcome.verified);
    let nodes = tree.snapshot();
    assert_eq!(nodes.len(), 2, "{:?}", nodes.keys());
    let root_ev = tree.journal().read(&id("root")).unwrap();
    assert!(root_ev.iter().any(|e| e.event.kind == "spawn_rejected"
        && e.event.data["reason"].as_str().unwrap().contains("loop")));
    let child_ev = tree.journal().read(&id("root/1")).unwrap();
    assert!(child_ev.iter().any(|e| e.event.kind == "spawn_rejected"
        && e.event.data["reason"].as_str().unwrap().contains("loop")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn width_limit_rejects_a_fourth_child() {
    let fx = Fixture::new();
    let script = |view: &NodeView, obs: &[Observation]| -> Decision {
        let t = turn(obs);
        match (view.depth, t) {
            (0, 0..=3) => Decision::Spawn(child_spec(view, &format!("child {t}"), 8)),
            (0, 4) => Decision::Wait,
            (0, _) => report("root done"),
            _ => report("leaf"),
        }
    };
    let tree = fx.tree(ScriptedLlm::new(script), cfg());
    tree.run_root(root_spec("fan out", 256)).await.unwrap();
    assert_eq!(tree.snapshot().len(), 4);
    let ev = tree.journal().read(&id("root")).unwrap();
    assert!(ev.iter().any(|e| e.event.kind == "spawn_rejected"
        && e.event.data["reason"].as_str().unwrap().contains("width")));
}

/// A node at depth 5 tries to leave its clone, reach the network and read a
/// host canary. Every attempt fails, and every failure is journaled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depth_five_node_cannot_escape_its_sandbox() {
    let fx = Fixture::new();
    let canary = fx.canary.to_string_lossy().to_string();
    let outside = fx
        .dir
        .path()
        .join("escaped.txt")
        .to_string_lossy()
        .to_string();
    let script = move |view: &NodeView, obs: &[Observation]| -> Decision {
        let t = turn(obs);
        if view.depth < 5 {
            return match t {
                0 => Decision::Spawn(child_spec(view, &format!("go deeper from {}", view.id), 2)),
                1 => Decision::Wait,
                _ => report("relay"),
            };
        }
        match t {
            0 => Decision::Write {
                path: "../../escape.txt".into(),
                content: "x".into(),
            },
            1 => Decision::Write {
                path: outside.clone(),
                content: "x".into(),
            },
            2 => Decision::Read {
                path: canary.clone(),
            },
            3 => Decision::Exec {
                cmd: format!("cat {canary}"),
            },
            4 => Decision::Exec {
                cmd: format!("echo x > {outside}"),
            },
            5 => Decision::Exec {
                cmd: "bash -c 'exec 3<>/dev/tcp/127.0.0.1/9' 2>&1".into(),
            },
            6 => Decision::Exec {
                cmd: "ls / ; echo ok-inside > inside.txt".into(),
            },
            _ => report("attacker done"),
        }
    };
    let tree = fx.tree(ScriptedLlm::new(script), cfg());
    tree.run_root(root_spec("deep escape", 512)).await.unwrap();
    let deep = id("root/1/1/1/1/1");
    assert_eq!(tree.snapshot()[&deep].depth, 5);
    assert!(!fx.dir.path().join("escaped.txt").exists());
    assert!(!fx.dir.path().join("escape.txt").exists());
    assert_eq!(
        std::fs::read_to_string(&fx.canary).unwrap(),
        "HOST-CANARY-DO-NOT-READ\n"
    );
    let ev = tree.journal().read(&deep).unwrap();
    let denied = ev
        .iter()
        .filter(|e| e.event.kind == "policy_denied")
        .count();
    assert!(
        denied >= 3,
        "two writes + one read must be denied, got {denied}"
    );
    let execs: Vec<_> = ev.iter().filter(|e| e.event.kind == "exec").collect();
    assert_eq!(execs.len(), 4);
    for e in &execs[..3] {
        assert_ne!(
            e.event.data["exit"].as_i64().unwrap(),
            0,
            "{:?}",
            e.event.data
        );
        assert!(!e.event.data["stdout"].as_str().unwrap().contains("CANARY"));
    }
    assert_eq!(execs[3].event.data["exit"].as_i64().unwrap(), 0);
    assert!(!execs[3].event.data["stdout"]
        .as_str()
        .unwrap()
        .contains("home"));
    assert!(tree.root_workdir().unwrap().join("inside.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn siblings_do_not_see_each_other() {
    let fx = Fixture::new();
    let sb = fx.sandbox_dir.to_string_lossy().to_string();
    let script = move |view: &NodeView, obs: &[Observation]| -> Decision {
        let t = turn(obs);
        match (view.id.to_string().as_str(), t) {
            ("root", 0) => Decision::Spawn(child_spec(view, "writer", 3)),
            ("root", 1) => Decision::Wait,
            ("root", 2) => Decision::Spawn(child_spec(view, "snooper", 3)),
            ("root", 3) => Decision::Wait,
            ("root", _) => report("root"),
            ("root/1", 0) => Decision::Write {
                path: "from-one.txt".into(),
                content: "1".into(),
            },
            ("root/1", _) => report("writer"),
            ("root/2", 0) => Decision::Exec { cmd: "ls".into() },
            ("root/2", 1) => Decision::Exec {
                cmd: format!("ls {sb}"),
            },
            ("root/2", _) => report("snooper"),
            _ => report("leaf"),
        }
    };
    let mut c = cfg();
    c.max_children_per_node = 2;
    let tree = fx.tree(ScriptedLlm::new(script), c);
    tree.run_root(root_spec("siblings", 128)).await.unwrap();
    let ev = tree.journal().read(&id("root/2")).unwrap();
    let execs: Vec<_> = ev.iter().filter(|e| e.event.kind == "exec").collect();
    // root/2 was cloned after root/1 merged, so it sees the merged file via
    // the parent, never the sibling's clone itself.
    assert!(execs[0].event.data["stdout"]
        .as_str()
        .unwrap()
        .contains("from-one.txt"));
    assert_ne!(execs[1].event.data["exit"].as_i64().unwrap(), 0);
}

#[test]
fn caps_subset_is_the_only_direction() {
    let parent = CapSet::of(&[Capability::Read, Capability::Exec]);
    assert!(CapSet::of(&[Capability::Read]).is_subset_of(&parent));
    assert!(parent.is_subset_of(&parent));
    assert!(!CapSet::of(&[Capability::Read, Capability::Network]).is_subset_of(&parent));
}
