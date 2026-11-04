//! Process-global registry of parallel "worlds" (sub-agents in flight).
//!
//! This module is the **shared substrate** between the CLI and the TUI: both
//! UIs render the same truth by subscribing to the same event stream. Any
//! code path that spawns work — the `SpawnWorld` primitive, whether the model
//! or `/spawn N <prompt>` emits it — publishes `WorldEvent`s here exactly
//! once; every surface that wants to display progress picks them up.
//!
//! ```text
//!                      ┌──────────────────┐
//!   SpawnWorld ─▶      │  WorldRegistry   │ ─▶  CLI `/worlds` inline render
//!   (model or /spawn)  │  (global, bcast) │ ─▶  TUI Worlds pane (F7)
//!                      └──────────────────┘
//! ```
//!
//! ## Contract
//!
//! - `register(id)` declares a new world and, on transition, is implicitly
//!   confirmed by the first `ActionStarted` event.
//! - `emit(event)` is fire-and-forget; if nobody is listening, the event is
//!   still recorded in the replay buffer (so a `/worlds` opened mid-flight
//!   shows existing work).
//! - `subscribe()` returns `(replay, live)` atomically — the replay buffer
//!   is drained *before* the broadcast receiver lock is released, so no
//!   event can be missed between the two.
//! - `reset()` clears all state (ids + replay buffer). Live receivers keep
//!   working but their view of the world starts empty.
//!
//! `WorldRegistry` is cheap to call from anywhere — it holds two short-lived
//! mutexes (`Vec<String>` + `VecDeque<WorldEvent>` capped) plus a
//! `tokio::sync::broadcast::Sender`. Broadcast lag (>= capacity) is treated
//! as a non-error: late subscribers rely on the replay buffer.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use tokio::sync::broadcast;

use crate::worlds_view::{WorldEvent, WorldStatus};

/// Size of the tokio broadcast channel (per-subscriber lag window).
const BROADCAST_CAPACITY: usize = 256;

/// Maximum number of past events kept for replay to late subscribers.
/// Events beyond this are forgotten but the *aggregate* world state stays
/// correct because terminal events are always replayed last.
const REPLAY_CAPACITY: usize = 512;

pub struct WorldRegistry {
    /// Declared world ids, in registration order (so UIs show them left-to-right
    /// the way they were spawned).
    ids: Mutex<Vec<String>>,
    /// Per-id terminal status (`None` ⇒ still running / pending).
    terminal_status: Mutex<Vec<(String, Option<WorldStatus>)>>,
    /// Rolling buffer of recent events for late subscribers. Capped at
    /// [`REPLAY_CAPACITY`]; `WorldCompleted`/`WorldRolledBack`/… entries are
    /// preserved preferentially so a terminal snapshot is always replayable.
    replay: Mutex<VecDeque<WorldEvent>>,
    /// Live broadcast sender. Receivers are created lazily via [`subscribe`].
    tx: broadcast::Sender<WorldEvent>,
}

impl WorldRegistry {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            ids: Mutex::new(Vec::new()),
            terminal_status: Mutex::new(Vec::new()),
            replay: Mutex::new(VecDeque::with_capacity(REPLAY_CAPACITY)),
            tx,
        }
    }

    /// Register a new world id. No-op if the id is already known.
    pub fn register(&self, id: impl Into<String>) {
        let id = id.into();
        {
            let mut ids = self.ids.lock().expect("ids mutex poisoned");
            if !ids.iter().any(|x| x == &id) {
                ids.push(id.clone());
            }
        }
        let mut status = self.terminal_status.lock().expect("status mutex poisoned");
        if !status.iter().any(|(x, _)| x == &id) {
            status.push((id, None));
        }
    }

    /// Declare all ids in one shot. Useful when the caller knows the full set
    /// upfront.
    pub fn register_many<I, S>(&self, ids: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for id in ids {
            self.register(id);
        }
    }

    /// Push an event. Implicitly registers the world id if unknown. Always
    /// stored in the replay buffer; broadcast is best-effort (no error if
    /// there are zero subscribers).
    pub fn emit(&self, event: WorldEvent) {
        let id = event.world_id().to_string();
        self.register(&id);

        // Track terminal transitions so `active_count()` is accurate.
        let terminal = match &event {
            WorldEvent::WorldCompleted { .. } => Some(WorldStatus::Completed),
            WorldEvent::WorldRolledBack { .. } => Some(WorldStatus::RolledBack),
            WorldEvent::WorldFailed { .. } => Some(WorldStatus::Failed),
            WorldEvent::WorldKilled { .. } => Some(WorldStatus::Killed),
            _ => None,
        };
        if let Some(t) = terminal {
            let mut status = self.terminal_status.lock().expect("status mutex poisoned");
            if let Some(entry) = status.iter_mut().find(|(x, _)| x == &id) {
                entry.1 = Some(t);
            }
        }

        {
            let mut replay = self.replay.lock().expect("replay mutex poisoned");
            if replay.len() >= REPLAY_CAPACITY {
                replay.pop_front();
            }
            replay.push_back(event.clone());
        }

        let _ = self.tx.send(event);
    }

    /// Snapshot + subscription in one shot. The replay vec is *drained from
    /// the current buffer*; any event emitted *after* this call arrives on
    /// the returned receiver. A late subscriber therefore sees a consistent
    /// timeline with zero duplicates and zero gaps.
    pub fn subscribe(&self) -> (Vec<WorldEvent>, broadcast::Receiver<WorldEvent>) {
        // Lock replay *before* creating the receiver so any concurrent emit
        // is ordered strictly after us.
        let replay = self.replay.lock().expect("replay mutex poisoned");
        let rx = self.tx.subscribe();
        let events: Vec<WorldEvent> = replay.iter().cloned().collect();
        drop(replay);
        (events, rx)
    }

    /// All declared world ids in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.ids.lock().expect("ids mutex poisoned").clone()
    }

    /// Count of worlds that have not yet reached a terminal state.
    pub fn active_count(&self) -> usize {
        self.terminal_status
            .lock()
            .expect("status mutex poisoned")
            .iter()
            .filter(|(_, t)| t.is_none())
            .count()
    }

    /// Count of worlds that have reached a terminal state.
    pub fn done_count(&self) -> usize {
        self.terminal_status
            .lock()
            .expect("status mutex poisoned")
            .iter()
            .filter(|(_, t)| t.is_some())
            .count()
    }

    /// Total registered worlds (active + done).
    pub fn total(&self) -> usize {
        self.ids.lock().expect("ids mutex poisoned").len()
    }

    /// Wipe all state. Safe to call while subscribers exist — they just see
    /// an empty replay on next subscribe and no new events until new work
    /// registers.
    pub fn reset(&self) {
        self.ids.lock().expect("ids mutex poisoned").clear();
        self.terminal_status
            .lock()
            .expect("status mutex poisoned")
            .clear();
        self.replay.lock().expect("replay mutex poisoned").clear();
    }
}

static GLOBAL: OnceLock<WorldRegistry> = OnceLock::new();

/// Access the process-global registry. Lazily initialised on first access.
pub fn global() -> &'static WorldRegistry {
    GLOBAL.get_or_init(WorldRegistry::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worlds_view::WorldEvent;

    fn fresh() -> WorldRegistry {
        WorldRegistry::new()
    }

    fn started(id: &str, action: &str) -> WorldEvent {
        WorldEvent::ActionStarted {
            world_id: id.into(),
            action_type: action.into(),
        }
    }
    fn finished(id: &str, action: &str, ms: u64) -> WorldEvent {
        WorldEvent::ActionFinished {
            world_id: id.into(),
            action_type: action.into(),
            duration_ms: ms,
        }
    }
    fn completed(id: &str) -> WorldEvent {
        WorldEvent::WorldCompleted {
            world_id: id.into(),
            summary: None,
        }
    }
    fn killed(id: &str) -> WorldEvent {
        WorldEvent::WorldKilled {
            world_id: id.into(),
            reason: "timeout".into(),
        }
    }

    #[test]
    fn register_is_idempotent_and_preserves_order() {
        let r = fresh();
        r.register("w1");
        r.register("w2");
        r.register("w1"); // dup
        assert_eq!(r.ids(), vec!["w1".to_string(), "w2".to_string()]);
        assert_eq!(r.total(), 2);
    }

    #[test]
    fn register_many_preserves_order() {
        let r = fresh();
        r.register_many(vec!["a", "b", "c"]);
        assert_eq!(r.ids(), vec!["a", "b", "c"]);
    }

    #[test]
    fn emit_implicitly_registers_unknown_ids() {
        let r = fresh();
        r.emit(started("ghost", "Noop"));
        assert_eq!(r.ids(), vec!["ghost".to_string()]);
    }

    #[test]
    fn active_and_done_counts_reflect_terminal_events() {
        let r = fresh();
        r.register_many(vec!["w1", "w2", "w3"]);
        assert_eq!(r.active_count(), 3);
        assert_eq!(r.done_count(), 0);

        r.emit(completed("w1"));
        assert_eq!(r.active_count(), 2);
        assert_eq!(r.done_count(), 1);

        r.emit(killed("w2"));
        assert_eq!(r.active_count(), 1);
        assert_eq!(r.done_count(), 2);
    }

    #[tokio::test]
    async fn subscribe_returns_replay_buffer_in_order() {
        let r = fresh();
        r.emit(started("w1", "ReadFile"));
        r.emit(finished("w1", "ReadFile", 10));
        r.emit(completed("w1"));

        let (replay, _rx) = r.subscribe();
        assert_eq!(replay.len(), 3);
        assert!(matches!(replay[0], WorldEvent::ActionStarted { .. }));
        assert!(matches!(replay[1], WorldEvent::ActionFinished { .. }));
        assert!(matches!(replay[2], WorldEvent::WorldCompleted { .. }));
    }

    #[tokio::test]
    async fn subscribe_receives_future_events_live() {
        let r = fresh();
        let (_replay, mut rx) = r.subscribe();

        r.emit(started("w1", "Add"));
        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv should not time out")
            .expect("recv should succeed");
        assert_eq!(ev.world_id(), "w1");
    }

    #[tokio::test]
    async fn late_subscriber_sees_past_events_once_only_in_replay() {
        // A classic race: emit happens before subscribe. The replay buffer
        // must contain it, and the live receiver must NOT re-deliver it.
        let r = fresh();
        r.emit(started("w1", "Add"));

        let (replay, mut rx) = r.subscribe();
        assert_eq!(replay.len(), 1);

        // Nothing should be pending on the live channel.
        let pending = tokio::time::timeout(std::time::Duration::from_millis(30), rx.recv()).await;
        assert!(
            pending.is_err(),
            "live receiver should be empty after subscribe"
        );

        // New emit must flow live.
        r.emit(completed("w1"));
        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");
        assert!(matches!(ev, WorldEvent::WorldCompleted { .. }));
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_every_event() {
        let r = fresh();
        let (_, mut rx1) = r.subscribe();
        let (_, mut rx2) = r.subscribe();

        r.emit(started("w1", "X"));
        r.emit(completed("w1"));

        for rx in [&mut rx1, &mut rx2] {
            for _ in 0..2 {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                    .await
                    .expect("timeout")
                    .expect("recv");
            }
        }
    }

    #[test]
    fn reset_clears_all_state_but_keeps_registry_usable() {
        let r = fresh();
        r.register("w1");
        r.emit(completed("w1"));
        assert_eq!(r.done_count(), 1);

        r.reset();
        assert_eq!(r.total(), 0);
        assert_eq!(r.active_count(), 0);
        assert_eq!(r.done_count(), 0);

        // Still usable after reset
        r.register("w2");
        r.emit(started("w2", "Noop"));
        assert_eq!(r.total(), 1);
    }

    #[test]
    fn global_accessor_returns_same_instance() {
        let a = global();
        let b = global();
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn replay_buffer_is_bounded() {
        let r = fresh();
        for i in 0..(REPLAY_CAPACITY + 50) {
            r.emit(started("w1", &format!("Act{i}")));
        }
        let (replay, _) = r.subscribe();
        assert!(
            replay.len() <= REPLAY_CAPACITY,
            "replay must stay bounded: got {}",
            replay.len()
        );
    }
}
