//! In-memory registry of orchestrator sessions for the HTTP serve mode.
//!
//! Each `x0ne --serve` instance keeps one [`OrchestratorSession`] per opaque
//! `session_id` supplied by the upstream web proxy via the
//! `X-X0ne-Session` header. The session owns:
//!   * a `tenant_id` derived deterministically from `session_id` (used as a
//!     prefix for downstream x0ne-runtime resources — containers, snapshots,
//!     workspace files — to enforce per-chat isolation);
//!   * a `Conversation` that is the **server-side source of truth** for the
//!     chat history (the request body's `history` field is only used to seed a
//!     fresh session, ignored thereafter);
//!   * activity counters and an `Instant` of last access for LRU eviction.
//!
//! Sessions expire after `ttl` of inactivity (default 30 min). A background
//! task in [`crate::serve::run`] calls [`SessionRegistry::evict_expired`]
//! every minute. There is also a hard cap (`max_sessions`) — when reached, the
//! oldest session is dropped to make room.
//!
//! # Threading
//! The registry uses `tokio::sync::RwLock` for the outer map and per-session
//! `Mutex` for the session contents. This keeps reads (most requests fall on
//! existing sessions) cheap and serializes mutations to a single chat to one
//! writer at a time.
//!
//! # Why in-memory (and not Redis)?
//! For the MVP a single `x0ne --serve` process is enough. When we need
//! horizontal scale or restart-survival, the same trait surface can be backed
//! by Redis (a `serve-redis` Cargo feature would swap the inner storage).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

// ── Session ─────────────────────────────────────────────────────────────────

/// Per-chat server state. Held behind a `Mutex` inside the registry because
/// `Conversation` is mutated on every turn.
/// The pieces a serve session is assembled from: the canonical session plus
/// the two per-turn drains the SSE translator reads.
pub struct ServeAgentParts {
    pub agent: crate::agent::session::CliAgentSession,
    /// Buffered semantic events (the sink writes, `run_session` drains).
    pub events: std::sync::Arc<std::sync::Mutex<Vec<crate::tui::wire::AgentEvent>>>,
    /// The driver's token stream, drained after each turn.
    pub tokens: tokio::sync::mpsc::UnboundedReceiver<crate::api::StreamChunk>,
}

pub struct OrchestratorSession {
    /// Opaque, stable id derived from the upstream session id. Safe to use as
    /// a prefix for filesystem paths, container names, snapshot labels.
    pub tenant_id: String,
    /// The canonical session: history, `ApprovalAuthority`, live budget,
    /// per-session `AgentRegistry`, bridge — everything a turn needs, with the
    /// same isolation properties every other frontend gets. A `SpawnWorld`
    /// issued in session A is invisible from session B because the two
    /// `CliAgentSession`s hold different registries — the hard cloisonnement
    /// property the upstream proxy relies on, now provided by construction
    /// rather than by a serve-private map.
    pub agent: crate::agent::session::CliAgentSession,
    /// Buffered semantic events for this session's sink.
    events: std::sync::Arc<std::sync::Mutex<Vec<crate::tui::wire::AgentEvent>>>,
    /// The driver's token stream for this session.
    tokens: tokio::sync::mpsc::UnboundedReceiver<crate::api::StreamChunk>,
    /// First time this session was created in-memory. Read by `/admin/sessions`
    /// (when wired) and exposed in tests; kept here so the field is part of
    /// the public shape rather than a debug-only concern.
    #[allow(dead_code)]
    pub created_at: Instant,
    /// Last time a `/prompt` came in for this session. Drives LRU eviction.
    pub last_activity: Instant,
    /// Number of `/prompt` exchanges processed for this session.
    pub turn_count: u64,
}

impl OrchestratorSession {
    /// Construct a fresh session. Exposed as `pub(crate)` so the serve-mode
    /// dispatcher tests can build a session without going through the
    /// registry's async `get_or_create`.
    pub fn new(tenant_id: String, parts: ServeAgentParts) -> Self {
        let now = Instant::now();
        Self {
            tenant_id,
            agent: parts.agent,
            events: parts.events,
            tokens: parts.tokens,
            created_at: now,
            last_activity: now,
            turn_count: 0,
        }
    }

    /// Everything the sink recorded since the last drain, in order.
    pub fn drain_events(&mut self) -> Vec<crate::tui::wire::AgentEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    /// Every token streamed since the last drain.
    pub fn drain_tokens(&mut self) -> Vec<crate::api::StreamChunk> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.tokens.try_recv() {
            out.push(chunk);
        }
        out
    }

    /// Mark a turn as just having happened — the registry calls this from
    /// the request path to keep the LRU honest.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
        self.turn_count += 1;
    }

    pub fn idle(&self) -> Duration {
        self.last_activity.elapsed()
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, Arc<Mutex<OrchestratorSession>>>>,
    /// Idle TTL — sessions untouched for longer are dropped.
    ttl: Duration,
    /// Hard cap. When reached, the LRU session is evicted to make room.
    max_sessions: usize,
}

impl SessionRegistry {
    pub fn new(ttl: Duration, max_sessions: usize) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl,
            max_sessions: max_sessions.max(1),
        }
    }

    /// Look up an existing session by id, or create one by calling
    /// `init_agent` (only invoked on miss). Returns the locked session pointer.
    ///
    /// `session_id` MUST be non-empty and is treated as opaque. The returned
    /// `tenant_id` (visible via `session.lock().await.tenant_id`) is
    /// deterministically derived from it.
    pub async fn get_or_create<F>(
        &self,
        session_id: &str,
        init_agent: F,
    ) -> Arc<Mutex<OrchestratorSession>>
    where
        F: FnOnce() -> ServeAgentParts,
    {
        debug_assert!(
            !session_id.is_empty(),
            "session_id must be non-empty (callers MUST validate)"
        );

        if let Some(existing) = self.sessions.read().await.get(session_id).cloned() {
            return existing;
        }

        // Slow path: write lock and double-check (another task may have raced).
        let mut map = self.sessions.write().await;
        if let Some(existing) = map.get(session_id).cloned() {
            return existing;
        }

        if map.len() >= self.max_sessions {
            evict_one_lru(&mut map);
        }

        let tenant_id = derive_tenant_id(session_id);
        let session = Arc::new(Mutex::new(OrchestratorSession::new(
            tenant_id,
            init_agent(),
        )));
        map.insert(session_id.to_string(), session.clone());
        info!(
            "session-registry: new session (sessions={}, tenant_id={})",
            map.len(),
            session
                .try_lock()
                .map(|s| s.tenant_id.clone())
                .unwrap_or_else(|_| "?".into()),
        );
        session
    }

    /// Drop a session by id. Returns `true` if it was present.
    /// Currently only invoked from tests; will be the entry point for an
    /// upcoming `/admin/sessions/:id DELETE` endpoint.
    #[allow(dead_code)]
    pub async fn drop_session(&self, session_id: &str) -> bool {
        self.sessions.write().await.remove(session_id).is_some()
    }

    /// Sweep idle sessions. Returns the number evicted.
    pub async fn evict_expired(&self) -> usize {
        let mut map = self.sessions.write().await;
        let before = map.len();
        let ttl = self.ttl;
        map.retain(|_id, s| {
            // If we can't take the lock, the session is in active use → keep it.
            match s.try_lock() {
                Ok(g) => g.idle() < ttl,
                Err(_) => true,
            }
        });
        let removed = before - map.len();
        if removed > 0 {
            info!(
                "session-registry: evicted {} idle sessions ({} remain)",
                removed,
                map.len()
            );
        }
        removed
    }

    /// Number of sessions currently in memory. Used by tests and by the
    /// background eviction logger.
    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// Pick the LRU session and remove it. Caller already holds the write lock.
/// Skips sessions currently being mutated (their lock is held), to avoid
/// dropping the session out from under an in-flight request.
fn evict_one_lru(map: &mut HashMap<String, Arc<Mutex<OrchestratorSession>>>) {
    let oldest_id = map
        .iter()
        .filter_map(|(id, s)| {
            // try_lock fails when a writer holds it → do not evict in-flight sessions.
            s.try_lock().ok().map(|g| (id.clone(), g.last_activity))
        })
        .min_by_key(|(_, t)| *t)
        .map(|(id, _)| id);

    match oldest_id {
        Some(id) => {
            map.remove(&id);
            warn!(
                "session-registry: capacity reached, evicted LRU session ({} remain)",
                map.len()
            );
        }
        None => {
            warn!(
                "session-registry: capacity reached but every session is in-flight; not evicting"
            );
        }
    }
}

// ── tenant_id derivation ────────────────────────────────────────────────────

/// Derive a deterministic, filesystem-safe `tenant_id` from a session id.
///
/// Callers downstream (x0ne-runtime action prefixing, container names, snapshot
/// labels) need a stable token that:
///   * has the same value for the same `session_id` (so the agent can keep
///     working on the same workspace across turns);
///   * does not leak the session id verbatim into logs or container names;
///   * fits container/path naming constraints (alphanumerics, dashes, ≤ 32 ch).
///
/// Implementation: hex-encoded `DefaultHasher` digest, prefixed with `t-`.
/// `DefaultHasher` is SipHash, which is collision-resistant for this purpose
/// (the session id is itself opaque/HMAC-derived upstream — we are not relying
/// on this to be cryptographically strong, just stable + opaque).
pub fn derive_tenant_id(session_id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    "x0ne-tenant-v1".hash(&mut h);
    session_id.hash(&mut h);
    let lo = h.finish();

    let mut h2 = DefaultHasher::new();
    "x0ne-tenant-v1-salt".hash(&mut h2);
    session_id.hash(&mut h2);
    "round2".hash(&mut h2);
    let hi = h2.finish();

    format!("t-{:016x}{:016x}", lo, hi)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct SilentBackend;

    #[async_trait::async_trait]
    impl crate::api::LlmBackend for SilentBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            Ok(crate::api::ApiResponse {
                content: "done".into(),
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "silent".into()
        }
    }

    fn fresh_conv() -> ServeAgentParts {
        let mut agent = crate::agent::session::CliAgentSession::new(
            crate::config::Config::default(),
            std::sync::Arc::new(SilentBackend),
            "SYS",
            None,
            Box::new(crate::display_sink::NullDisplaySink),
        );
        let tokens = agent.open_stream();
        ServeAgentParts {
            agent,
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            tokens,
        }
    }

    #[test]
    fn tenant_id_is_deterministic_and_opaque() {
        let a1 = derive_tenant_id("session-abc");
        let a2 = derive_tenant_id("session-abc");
        let b = derive_tenant_id("session-xyz");
        assert_eq!(a1, a2, "same session_id MUST yield same tenant_id");
        assert_ne!(
            a1, b,
            "different session_ids MUST yield different tenant_ids"
        );
        assert!(a1.starts_with("t-"));
        assert_eq!(a1.len(), 2 + 32, "tenant_id should be `t-` + 32 hex chars");
        // Body (everything after the `t-` prefix) is strictly hex.
        assert!(a1[2..].chars().all(|c| c.is_ascii_hexdigit()));
        // Opaqueness: the session_id MUST NOT appear verbatim in the tenant_id.
        assert!(!a1.contains("session-abc"));
    }

    #[tokio::test]
    async fn get_or_create_returns_same_session_for_same_id() {
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let s1 = reg.get_or_create("session-a", fresh_conv).await;
        let s2 = reg.get_or_create("session-a", fresh_conv).await;
        assert!(
            Arc::ptr_eq(&s1, &s2),
            "second call MUST return the same Arc"
        );
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn distinct_sessions_have_distinct_tenants() {
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let a = reg.get_or_create("session-a", fresh_conv).await;
        let b = reg.get_or_create("session-b", fresh_conv).await;
        let ta = a.lock().await.tenant_id.clone();
        let tb = b.lock().await.tenant_id.clone();
        assert_ne!(ta, tb);
        assert_eq!(reg.len().await, 2);
    }

    #[tokio::test]
    async fn isolation_session_a_cannot_see_session_b_messages() {
        // Cloisonnement test: a write to session A's conversation must NOT be
        // visible from session B. This is the core property the proxy relies on.
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        {
            let a = reg.get_or_create("session-a", fresh_conv).await;
            a.lock().await.agent.state.history.add_user("secret from A");
        }
        let b = reg.get_or_create("session-b", fresh_conv).await;
        let b_lock = b.lock().await;
        assert!(
            b_lock.agent.state.history.messages().is_empty(),
            "session B MUST start empty"
        );
    }

    #[tokio::test]
    async fn touch_updates_activity_and_turn_count() {
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let s = reg.get_or_create("session-x", fresh_conv).await;
        let initial_turns = s.lock().await.turn_count;
        s.lock().await.touch();
        s.lock().await.touch();
        let after = s.lock().await;
        assert_eq!(after.turn_count, initial_turns + 2);
    }

    #[tokio::test]
    async fn evict_expired_drops_idle_sessions_only() {
        // TTL of 0ms means "evict anything that isn't currently held".
        let reg = SessionRegistry::new(Duration::from_millis(0), 100);
        let _s = reg.get_or_create("session-old", fresh_conv).await;
        // Drop the local Arc so the lock is released immediately.
        drop(_s);
        // Sleep a tick so `idle()` becomes > 0ms.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let removed = reg.evict_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn capacity_eviction_drops_oldest_first() {
        let reg = SessionRegistry::new(Duration::from_secs(60), 2);
        let _a = reg.get_or_create("a", fresh_conv).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let _b = reg.get_or_create("b", fresh_conv).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        // Drop arcs so try_lock in evict_one_lru can succeed.
        drop(_a);
        drop(_b);
        // Adding a 3rd session should evict "a" (the oldest).
        let _c = reg.get_or_create("c", fresh_conv).await;
        assert_eq!(reg.len().await, 2);
        // Re-fetching "a" should yield a fresh session (so a new conv).
        let a_again = reg.get_or_create("a", fresh_conv).await;
        assert!(a_again
            .lock()
            .await
            .agent
            .state
            .history
            .messages()
            .is_empty());
    }

    #[tokio::test]
    async fn distinct_sessions_have_distinct_agent_registries() {
        // Phase 3 core cloisonnement: a sub-agent spawned in session A must
        // NOT be visible from session B, and vice versa. The proxy trusts
        // this property — if it breaks, one user's parallel worlds leak
        // into another user's chat.
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let a = reg.get_or_create("session-a", fresh_conv).await;
        let b = reg.get_or_create("session-b", fresh_conv).await;

        let reg_a = a.lock().await.agent.registry.clone();
        let reg_b = b.lock().await.agent.registry.clone();

        // The two registries MUST be distinct Arcs — a shared one would be a
        // silent cross-tenant leak even if no agent is ever spawned.
        assert!(
            !Arc::ptr_eq(&reg_a, &reg_b),
            "session A and session B must NOT share an AgentRegistry"
        );

        // And operationally: inserting an agent into A does not show in B.
        reg_a.insert_completed_for_test(crate::sysaicalls::AgentResult {
            id: "alt-1".into(),
            task: "compute".into(),
            response: "42".into(),
            tokens_used: 0,
            actions_executed: 0,
            actions_breakdown: crate::sysaicalls::AgentResult::empty_breakdown(),
            success: true,
            duration_ms: 0,
        });
        assert_eq!(reg_a.count(), 1);
        assert_eq!(
            reg_b.count(),
            0,
            "session B must not see session A's agents"
        );
    }

    #[tokio::test]
    async fn same_session_keeps_agent_registry_across_turns() {
        // When the proxy sends two `/prompt` requests on the same session
        // id, the second turn MUST still see the agents spawned in the first.
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let first = reg.get_or_create("session-keep", fresh_conv).await;
        first.lock().await.agent.registry.insert_completed_for_test(
            crate::sysaicalls::AgentResult {
                id: "persistent".into(),
                task: "t".into(),
                response: "r".into(),
                tokens_used: 0,
                actions_executed: 0,
                actions_breakdown: crate::sysaicalls::AgentResult::empty_breakdown(),
                success: true,
                duration_ms: 0,
            },
        );
        let second = reg.get_or_create("session-keep", fresh_conv).await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.lock().await.agent.registry.count(), 1);
    }

    #[tokio::test]
    async fn drop_session_removes_it() {
        let reg = SessionRegistry::new(Duration::from_secs(60), 100);
        let _ = reg.get_or_create("doomed", fresh_conv).await;
        assert_eq!(reg.len().await, 1);
        assert!(reg.drop_session("doomed").await);
        assert_eq!(reg.len().await, 0);
        assert!(
            !reg.drop_session("doomed").await,
            "second drop returns false"
        );
    }
}
