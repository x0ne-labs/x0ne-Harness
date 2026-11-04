//! End-to-end integration test for the parallel-worlds pipeline.
//!
//! Exercises the full chain that both UIs rely on:
//!
//!   a producer emitting `WorldEvent`s (here: scripted, no LLM, no bridge)
//!     → worlds_registry (broadcast + replay)
//!       → WorldsRenderer::apply   (same model the CLI renderer uses)
//!         → render_lines()        (same function the TUI pane uses)
//!
//! This is the single test that guarantees the CLI and the TUI stay
//! visually consistent: if either renderer drifts, this snapshot will
//! fail. In production the producer is the world task started by
//! `SessionWorlds::spawn_detached` (see `x0ne_orchestrator::agent::world`).

use std::time::Duration;

use x0ne_orchestrator::{
    worlds_registry,
    worlds_view::{WorldEvent, WorldStatus, WorldsRenderer},
};

// All tests in this file touch the process-global worlds registry.
// Serialise them so they don't cross-contaminate each other's state.
static REG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Block until every registered world reaches a terminal state (or `timeout`
/// elapses). Returns true on success.
async fn wait_until_idle(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if worlds_registry::global().total() > 0 && worlds_registry::global().active_count() == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Register `n` worlds now and drive each through a short scripted
/// trajectory in a background task: world-1 completes, world-2 rolls back,
/// world-3 fails, world-4+ complete.
fn spawn_scripted(n: usize) {
    use WorldEvent::*;
    let ids: Vec<String> = (1..=n).map(|i| format!("world-{i}")).collect();
    worlds_registry::global().register_many(ids.clone());
    for (idx, id) in ids.into_iter().enumerate() {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(idx as u64 * 40)).await;
            for (action, dur) in [("ListDir", 30u64), ("ReadFile", 40), ("RunCommand", 60)] {
                worlds_registry::global().emit(ActionStarted {
                    world_id: id.clone(),
                    action_type: action.into(),
                });
                tokio::time::sleep(Duration::from_millis(dur)).await;
                worlds_registry::global().emit(ActionFinished {
                    world_id: id.clone(),
                    action_type: action.into(),
                    duration_ms: dur,
                });
            }
            let tail = id.rsplit('-').next().unwrap_or("0");
            worlds_registry::global().emit(match tail {
                "2" => WorldRolledBack {
                    world_id: id.clone(),
                    snapshot: Some("pre-task".into()),
                },
                "3" => WorldFailed {
                    world_id: id.clone(),
                    error: "scripted: terminal failure".into(),
                },
                _ => WorldCompleted {
                    world_id: id.clone(),
                    summary: Some(format!("{id} — done")),
                },
            });
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_worlds_flow_through_registry_into_renderer() {
    let _g = REG.lock().unwrap_or_else(|e| e.into_inner());
    worlds_registry::global().reset();

    // Subscribe *before* spawning, so we catch every event live.
    let (_replay, mut rx) = worlds_registry::global().subscribe();

    // Kick off 4 scripted worlds.
    spawn_scripted(4);
    assert_eq!(
        worlds_registry::global().total(),
        4,
        "register_many should register 4 worlds immediately"
    );

    // Drain live events into a renderer, the exact same way the TUI's
    // live-subscriber task does it. We stop when every world has reached
    // a terminal state.
    let ids = worlds_registry::global().ids();
    let mut renderer = WorldsRenderer::new(ids.clone(), 120);

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(ev)) => renderer.apply(ev),
            Ok(Err(_)) => break,
            Err(_) => {
                // No event in 500ms — check if we're done.
                if renderer.all_terminal() {
                    break;
                }
            }
        }
    }

    assert!(
        wait_until_idle(Duration::from_secs(2)).await,
        "registry active_count should drop to 0 after all worlds terminate"
    );
    assert!(
        renderer.all_terminal(),
        "renderer should reflect terminal states"
    );

    // Deterministic outcome shape (set in spawn_scripted):
    //   world-1 → Completed, world-2 → RolledBack, world-3 → Failed, world-4 → Completed
    assert_eq!(
        renderer.worlds().get("world-1").map(|w| w.status),
        Some(WorldStatus::Completed)
    );
    assert_eq!(
        renderer.worlds().get("world-2").map(|w| w.status),
        Some(WorldStatus::RolledBack)
    );
    assert_eq!(
        renderer.worlds().get("world-3").map(|w| w.status),
        Some(WorldStatus::Failed)
    );
    assert_eq!(
        renderer.worlds().get("world-4").map(|w| w.status),
        Some(WorldStatus::Completed)
    );

    // The mixed tally (completed, rolled_back, failed, killed).
    assert_eq!(renderer.tally(), (2, 1, 1, 0));

    // `render_lines()` (used by the TUI pane) must contain the same cues as
    // the plain CLI `render()` (used by the inline renderer). We only check
    // the content of the `Line`s — styling is verified in worlds_view tests.
    let lines = renderer.render_lines();
    let joined: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let plain = renderer.render(false);

    for marker in ["ok", "rollback", "fail", "parallel worlds (4)"] {
        assert!(
            joined.contains(marker),
            "TUI frame missing `{}`:\n{}",
            marker,
            joined
        );
        assert!(
            plain.contains(marker),
            "CLI frame missing `{}`:\n{}",
            marker,
            plain
        );
    }

    // Final housekeeping — don't leak state to other tests.
    worlds_registry::global().reset();
}

#[tokio::test]
async fn registry_replay_is_race_free_for_late_subscribers() {
    let _g = REG.lock().unwrap_or_else(|e| e.into_inner());
    // Emit some events, THEN subscribe, THEN emit more. The late subscriber
    // should see all past events via the replay vec and all future events
    // via the broadcast channel, with no overlap.
    worlds_registry::global().reset();

    worlds_registry::global().emit(WorldEvent::ActionStarted {
        world_id: "w1".into(),
        action_type: "Past".into(),
    });
    worlds_registry::global().emit(WorldEvent::ActionFinished {
        world_id: "w1".into(),
        action_type: "Past".into(),
        duration_ms: 1,
    });

    let (replay, mut rx) = worlds_registry::global().subscribe();
    assert_eq!(replay.len(), 2);

    worlds_registry::global().emit(WorldEvent::WorldCompleted {
        world_id: "w1".into(),
        summary: None,
    });

    let live = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");
    assert!(matches!(live, WorldEvent::WorldCompleted { .. }));

    // No spurious extra events.
    let extra = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(extra.is_err(), "no more events should be pending");

    worlds_registry::global().reset();
}

#[tokio::test]
async fn reset_is_safe_with_active_subscribers() {
    let _g = REG.lock().unwrap_or_else(|e| e.into_inner());
    worlds_registry::global().reset();

    let (_replay, mut rx) = worlds_registry::global().subscribe();

    worlds_registry::global().register("w1");
    worlds_registry::global().emit(WorldEvent::ActionStarted {
        world_id: "w1".into(),
        action_type: "X".into(),
    });
    let _ = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;

    worlds_registry::global().reset();

    // Subscriber is still alive and picks up new events on the same channel.
    worlds_registry::global().emit(WorldEvent::ActionStarted {
        world_id: "w2".into(),
        action_type: "Y".into(),
    });
    let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");
    assert_eq!(ev.world_id(), "w2");

    worlds_registry::global().reset();
}
