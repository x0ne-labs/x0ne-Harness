#![allow(dead_code)]
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HandleType {
    Process,
    FileWatch,
    PortWatch,
    ProcessWatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HandleStatus {
    Running,
    Completed { at: String },
    Failed { at: String, error: String },
    Cancelled { at: String },
}

impl HandleStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, HandleStatus::Running)
    }

    /// RFC-3339 instant this handle reached a terminal state, if it has.
    ///
    /// Retention is measured from HERE, not from `created_at`: a job that ran
    /// for an hour and finished a second ago must not be evicted before the
    /// caller has read its output.
    pub fn finished_at(&self) -> Option<&str> {
        match self {
            HandleStatus::Running => None,
            HandleStatus::Completed { at }
            | HandleStatus::Failed { at, .. }
            | HandleStatus::Cancelled { at } => Some(at),
        }
    }
}

/// Shared stdin pipe for interactive process handles.
pub type SharedStdin = Arc<Mutex<tokio::process::ChildStdin>>;

/// A persistent reference to an ongoing task (process, file watch, port watch).
pub struct Handle {
    pub id: String,
    pub handle_type: HandleType,
    pub created_at: String,
    pub status: HandleStatus,
    /// Buffered events / output lines (cleared on drain).
    pub events: VecDeque<Value>,
    /// How many events were dropped due to buffer overflow.
    pub events_dropped: u64,
    /// Tokio task managing the background work (used for cancellation).
    pub task: Option<JoinHandle<()>>,
    /// Maximum number of events to buffer before dropping oldest.
    pub max_events: usize,
    /// Optional stdin pipe for interactive process handles (SendInput).
    pub stdin: Option<SharedStdin>,
    /// OS pid of the child a process handle drives, so `StopHandle` can ask
    /// it to leave (SIGTERM) before making it (SIGKILL). `None` for handles
    /// that are not a process.
    pub pid: Option<u32>,
}

impl Handle {
    pub fn new(id: impl Into<String>, handle_type: HandleType, max_events: usize) -> Self {
        Self {
            id: id.into(),
            handle_type,
            created_at: Utc::now().to_rfc3339(),
            status: HandleStatus::Running,
            events: VecDeque::new(),
            events_dropped: 0,
            task: None,
            max_events,
            stdin: None,
            pid: None,
        }
    }

    pub fn push_event(&mut self, event: Value) {
        if self.events.len() >= self.max_events {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(event);
    }

    pub fn drain_events(&mut self) -> Vec<Value> {
        self.events.drain(..).collect()
    }

    /// Snapshot of public fields for serialization (no task handle).
    pub fn summary(&self) -> HandleSummary {
        HandleSummary {
            id: self.id.clone(),
            handle_type: self.handle_type.clone(),
            created_at: self.created_at.clone(),
            status: self.status.clone(),
            buffered_events_count: self.events.len(),
            events_dropped: self.events_dropped,
        }
    }
}

/// Serializable summary of a handle (used for ListHandles).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandleSummary {
    pub id: String,
    pub handle_type: HandleType,
    pub created_at: String,
    pub status: HandleStatus,
    pub buffered_events_count: usize,
    pub events_dropped: u64,
}

/// Hard ceiling on retained terminal handles, independent of the time window.
///
/// Retention alone does not bound memory: a burst of short-lived handles all
/// inside the window grows the map without limit. This caps it in the worst
/// case while leaving normal use (a handful of concurrent jobs) untouched.
pub const MAX_TERMINAL_HANDLES: usize = 256;

// ── HandleManager ─────────────────────────────────────────────────────────────

/// Thread-safe manager for all active and recently completed handles.
pub struct HandleManager {
    handles: HashMap<String, Handle>,
    max_events_per_handle: usize,
    completed_retention_secs: u64,
    max_concurrent: usize,
    /// Named terminals (Term* actions). Shared so a caller can drop this lock
    /// before talking to tmux.
    pub terms: Arc<crate::executor::term::TermRegistry>,
}

impl HandleManager {
    pub fn new(max_events: usize, retention_secs: u64, max_concurrent: usize) -> Self {
        Self {
            handles: HashMap::new(),
            max_events_per_handle: max_events,
            completed_retention_secs: retention_secs,
            max_concurrent,
            terms: Arc::new(crate::executor::term::TermRegistry::default()),
        }
    }

    /// Register a new running handle. Returns the handle id on success,
    /// or an error if the concurrent handle limit is reached.
    pub fn register(
        &mut self,
        id: impl Into<String>,
        handle_type: HandleType,
    ) -> Result<String, String> {
        // Sweep before admitting new work: this is the one call site every
        // handle-creating path already goes through, so the map stays bounded
        // in long-lived servers without a background task anyone can forget.
        self.gc();
        let running = self
            .handles
            .values()
            .filter(|h| matches!(h.status, HandleStatus::Running))
            .count();
        if running >= self.max_concurrent {
            return Err(format!(
                "Too many concurrent handles ({}/{}). Use StopHandle to free one.",
                running, self.max_concurrent
            ));
        }
        let id = id.into();
        self.handles.insert(
            id.clone(),
            Handle::new(id.clone(), handle_type, self.max_events_per_handle),
        );
        Ok(id)
    }

    /// Attach the background tokio task to an existing handle.
    pub fn set_task(&mut self, id: &str, task: JoinHandle<()>) {
        if let Some(h) = self.handles.get_mut(id) {
            h.task = Some(task);
        }
    }

    /// Record the child's pid on a process handle.
    pub fn set_pid(&mut self, id: &str, pid: Option<u32>) {
        if let Some(h) = self.handles.get_mut(id) {
            h.pid = pid;
        }
    }

    /// The child's pid, if this is a process handle still worth signalling.
    pub fn pid_of(&self, id: &str) -> Option<u32> {
        self.handles.get(id).and_then(|h| h.pid)
    }

    /// Attach a stdin pipe to an existing handle (for SendInput).
    pub fn set_stdin(&mut self, id: &str, stdin: SharedStdin) {
        if let Some(h) = self.handles.get_mut(id) {
            h.stdin = Some(stdin);
        }
    }

    /// Get a clone of the stdin pipe for a handle (if it has one).
    pub fn get_stdin(&self, id: &str) -> Option<SharedStdin> {
        self.handles.get(id).and_then(|h| h.stdin.clone())
    }

    /// Push an event into a handle's buffer.
    pub fn push_event(&mut self, id: &str, event: Value) {
        if let Some(h) = self.handles.get_mut(id) {
            h.push_event(event);
        }
    }

    /// Drain all buffered events and return them (buffer is cleared).
    pub fn drain_events(&mut self, id: &str) -> Option<Vec<Value>> {
        self.handles.get_mut(id).map(|h| h.drain_events())
    }

    pub fn get_status(&self, id: &str) -> Option<HandleStatus> {
        self.handles.get(id).map(|h| h.status.clone())
    }

    pub fn set_status(&mut self, id: &str, status: HandleStatus) {
        if let Some(h) = self.handles.get_mut(id) {
            h.status = status;
        }
    }

    pub fn events_dropped(&self, id: &str) -> u64 {
        self.handles.get(id).map(|h| h.events_dropped).unwrap_or(0)
    }

    /// Cancel a handle: abort its task, set status to Cancelled.
    pub fn cancel(&mut self, id: &str) -> bool {
        if let Some(h) = self.handles.get_mut(id) {
            if let Some(task) = h.task.take() {
                task.abort();
            }
            h.status = HandleStatus::Cancelled {
                at: Utc::now().to_rfc3339(),
            };
            true
        } else {
            false
        }
    }

    pub fn exists(&self, id: &str) -> bool {
        self.handles.contains_key(id)
    }

    pub fn summaries(&self) -> Vec<HandleSummary> {
        self.handles.values().map(|h| h.summary()).collect()
    }

    /// Drop terminal handles past the retention window, then enforce the
    /// absolute cap. Running handles are never touched.
    ///
    /// Called automatically from `register()`, so any path that creates a
    /// handle also bounds the map — no background task, no caller to forget.
    pub fn gc(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::seconds(self.completed_retention_secs as i64);
        self.handles.retain(|_, h| {
            // Running handles are live state, never collectable.
            let Some(at) = h.status.finished_at() else {
                return true;
            };
            // Unparseable timestamp: keep (fail safe — never discard state we
            // cannot reason about). The cap below still bounds memory.
            chrono::DateTime::parse_from_rfc3339(at)
                .map(|t| t.with_timezone(&Utc) > cutoff)
                .unwrap_or(true)
        });

        // Time-based retention cannot bound a burst that fits inside the
        // window, so cap the terminal set by dropping the oldest first.
        let mut terminal: Vec<(String, String)> = self
            .handles
            .iter()
            .filter_map(|(id, h)| {
                h.status
                    .finished_at()
                    .map(|at| (id.clone(), at.to_string()))
            })
            .collect();
        if terminal.len() > MAX_TERMINAL_HANDLES {
            terminal.sort_by(|a, b| a.1.cmp(&b.1)); // oldest first
            let excess = terminal.len() - MAX_TERMINAL_HANDLES;
            for (id, _) in terminal.into_iter().take(excess) {
                self.handles.remove(&id);
            }
        }
    }
}

/// Shared handle manager — passed into executors and the HTTP server.
pub type SharedHandles = Arc<Mutex<HandleManager>>;

/// Create the shared handle manager from config values.
pub fn new_shared(max_events: usize, retention_secs: u64, max_concurrent: usize) -> SharedHandles {
    Arc::new(Mutex::new(HandleManager::new(
        max_events,
        retention_secs,
        max_concurrent,
    )))
}

// ── ID generation ─────────────────────────────────────────────────────────────

/// Generate a human-readable handle ID with a prefix, e.g. "proc_a1b2c3d4".
pub fn gen_id(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let short = &id.replace('-', "")[..8];
    format!("{}_{}", prefix, short)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── GC / retention ─────────────────────────────────────────────────────

    /// Retention must be measured from when a handle FINISHED, not when it
    /// started. Measuring from `created_at` evicts a long-running job the
    /// instant it completes, discarding output the caller never read.
    #[test]
    fn gc_retains_recently_finished_long_running_handle() {
        let mut m = HandleManager::new(100, 300, 20);
        let id = m.register("proc_long", HandleType::Process).unwrap();
        // Started well beyond the retention window, finished just now.
        m.handles.get_mut(&id).unwrap().created_at =
            (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339();
        m.set_status(
            &id,
            HandleStatus::Completed {
                at: Utc::now().to_rfc3339(),
            },
        );
        m.gc();
        assert!(
            m.get_status(&id).is_some(),
            "a job that finished seconds ago must survive GC regardless of when it started"
        );
    }

    /// The point of GC: terminal handles past the window are collected.
    #[test]
    fn gc_collects_expired_terminal_handles() {
        let mut m = HandleManager::new(100, 300, 20);
        let id = m.register("proc_old", HandleType::Process).unwrap();
        m.set_status(
            &id,
            HandleStatus::Completed {
                at: Utc::now().to_rfc3339(),
            },
        );
        m.set_status(
            &id,
            HandleStatus::Completed {
                at: (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339(),
            },
        );
        m.gc();
        assert!(
            m.get_status(&id).is_none(),
            "expired terminal handle must be collected"
        );
    }

    /// A running handle is never collected, however old.
    #[test]
    fn gc_never_collects_live_handles() {
        let mut m = HandleManager::new(100, 300, 20);
        let id = m.register("proc_live", HandleType::Process).unwrap();
        m.handles.get_mut(&id).unwrap().created_at =
            (Utc::now() - chrono::Duration::seconds(86_400)).to_rfc3339();
        m.gc();
        assert!(
            m.get_status(&id).is_some(),
            "a running handle must never be collected"
        );
    }

    /// GC must run on its own: nothing else in the runtime calls it, so an
    /// unbounded session would otherwise grow the map forever.
    #[test]
    fn register_collects_expired_handles_automatically() {
        let mut m = HandleManager::new(100, 300, 20);
        let old = m.register("proc_stale", HandleType::Process).unwrap();
        m.set_status(
            &old,
            HandleStatus::Completed {
                at: Utc::now().to_rfc3339(),
            },
        );
        m.set_status(
            &old,
            HandleStatus::Completed {
                at: (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339(),
            },
        );

        // Registering anything at all should sweep the expired entry.
        let _new = m.register("proc_new", HandleType::Process).unwrap();
        assert!(
            m.get_status(&old).is_none(),
            "register() must sweep expired handles so long sessions stay bounded"
        );
    }

    /// Retention alone does not bound memory: a burst of short-lived handles
    /// inside the window would still grow without limit.
    #[test]
    fn gc_enforces_absolute_cap_on_terminal_handles() {
        let mut m = HandleManager::new(100, 300, 20);
        for i in 0..(MAX_TERMINAL_HANDLES + 50) {
            let id = m
                .register(format!("proc_{i}"), HandleType::Process)
                .unwrap();
            m.set_status(
                &id,
                HandleStatus::Completed {
                    at: Utc::now().to_rfc3339(),
                },
            );
        }
        m.gc();
        let terminal = m
            .handles
            .values()
            .filter(|h| h.status.is_terminal())
            .count();
        assert!(
            terminal <= MAX_TERMINAL_HANDLES,
            "terminal handles must be capped, got {terminal}"
        );
    }

    fn make_mgr() -> HandleManager {
        HandleManager::new(100, 300, 20)
    }

    #[test]
    fn register_and_push_event() {
        let mut mgr = make_mgr();
        let id = mgr.register("proc_test", HandleType::Process).unwrap();
        mgr.push_event(&id, json!({"line": "hello"}));
        mgr.push_event(&id, json!({"line": "world"}));
        let events = mgr.drain_events(&id).unwrap();
        assert_eq!(events.len(), 2);
        // Buffer should now be empty
        assert_eq!(mgr.drain_events(&id).unwrap().len(), 0);
    }

    #[test]
    fn cancel_sets_status() {
        let mut mgr = make_mgr();
        let id = mgr.register("watch_test", HandleType::FileWatch).unwrap();
        assert!(mgr.cancel(&id));
        assert!(matches!(
            mgr.get_status(&id),
            Some(HandleStatus::Cancelled { .. })
        ));
    }

    #[test]
    fn max_concurrent_enforced() {
        let mut mgr = HandleManager::new(100, 300, 2);
        mgr.register("a", HandleType::Process).unwrap();
        mgr.register("b", HandleType::Process).unwrap();
        assert!(mgr.register("c", HandleType::Process).is_err());
    }

    #[test]
    fn buffer_overflow_drops_oldest() {
        let mut mgr = HandleManager::new(3, 300, 20);
        let id = mgr.register("p", HandleType::Process).unwrap();
        for i in 0..5u64 {
            mgr.push_event(&id, json!({"i": i}));
        }
        let h = mgr.handles.get(&id).unwrap();
        assert_eq!(h.events.len(), 3);
        assert_eq!(h.events_dropped, 2);
    }

    #[test]
    fn summaries_returns_all() {
        let mut mgr = make_mgr();
        mgr.register("p1", HandleType::Process).unwrap();
        mgr.register("p2", HandleType::FileWatch).unwrap();
        assert_eq!(mgr.summaries().len(), 2);
    }
}
