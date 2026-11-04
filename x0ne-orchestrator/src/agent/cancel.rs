//! One cancellation primitive for the whole engine.
//!
//! There were two, with different reach. The CLI had a global `Interrupt`
//! driven by SIGINT, and it was the only loop that could stop an action
//! *already in flight* — it wrapped the bridge call in a `tokio::select!`.
//! The TUIs had a per-app `Notify` that only ever interrupted model streaming,
//! so a 30-second `RunCommand` could not be called off once dispatched. serve
//! had nothing at all, and its sub-agents were detached, surviving the session
//! that spawned them.
//!
//! This token reaches all three: model streaming, an in-flight bridge call,
//! and the iteration itself. Frontends signal it and nothing more.
//!
//! It deliberately pairs a flag with the notifier. `Notify` alone drops a
//! signal that arrives before a waiter registers, which is precisely the race
//! that matters here: the user presses Ctrl-C in the window between deciding
//! to dispatch and awaiting the result, and a bare `Notify` would swallow it
//! and run the action anyway.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// The outcome of running something under a cancellation token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cancellable<T> {
    /// The work finished.
    Completed(T),
    /// Cancellation won. The work was dropped, not awaited.
    Cancelled,
}

impl<T> Cancellable<T> {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Cancellable::Cancelled)
    }

    pub fn completed(self) -> Option<T> {
        match self {
            Cancellable::Completed(v) => Some(v),
            Cancellable::Cancelled => None,
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// A cancellation signal shared between a frontend and the engine.
///
/// Cloning shares the signal; every clone observes the same cancellation.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
    /// Set on a child. Read-only: a child observes its parent's cancellation
    /// and never writes to it. A watcher task would have worked too, but a
    /// flag is deterministic — no scheduling window in which a cancelled
    /// parent still has a running child.
    parent: Option<Arc<Inner>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent, and safe to call from any task.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        // Wake everyone: several places may be waiting on the same token
        // (the stream, the bridge call, the iteration guard).
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
            || self
                .parent
                .as_ref()
                .is_some_and(|p| p.cancelled.load(Ordering::SeqCst))
    }

    /// A token for work this session owns, cancelled when this one is but not
    /// the reverse.
    ///
    /// Cloning shares the signal in both directions, which is wrong for a
    /// child agent: a child that stops must not stop its parent, and a sibling
    /// that stops must not stop its siblings. A child therefore owns a
    /// separate flag and reads its parent's, so the link runs one way only.
    pub fn child(&self) -> CancelToken {
        CancelToken {
            inner: Arc::new(Inner::default()),
            parent: Some(Arc::clone(&self.inner)),
        }
    }

    /// Resolve when cancellation is requested — immediately if it already was.
    ///
    /// Checking the flag first is what closes the race a bare `Notify` loses.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        loop {
            let waiter = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiter.await;
            if self.is_cancelled() {
                return;
            }
        }
    }

    /// Run `fut` unless cancellation wins.
    ///
    /// This is how in-flight cancellation becomes available everywhere rather
    /// than only at the one CLI call site that hand-rolled the `select!`.
    pub async fn run<F: std::future::Future>(&self, fut: F) -> Cancellable<F::Output> {
        // Check before starting: a token cancelled a moment ago must not
        // dispatch anything, however fast `fut` would have been.
        if self.is_cancelled() {
            return Cancellable::Cancelled;
        }
        tokio::select! {
            biased;
            _ = self.cancelled() => Cancellable::Cancelled,
            out = fut => Cancellable::Completed(out),
        }
    }

    /// Clear the signal for a new turn.
    ///
    /// Without this a single Ctrl-C would poison every later turn in the
    /// session, since the token outlives the iteration it interrupted.
    pub fn reset(&self) {
        self.inner.cancelled.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_fresh_token_is_not_cancelled() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        assert!(matches!(
            t.run(async { 7 }).await,
            Cancellable::Completed(7)
        ));
    }

    /// The race that a bare `Notify` loses: cancellation arrives before
    /// anything is awaiting it. It must still be observed.
    #[test]
    fn cancellation_before_any_waiter_is_not_lost() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let t = CancelToken::new();
            t.cancel(); // nobody is waiting yet
            tokio::time::timeout(Duration::from_millis(200), t.cancelled())
                .await
                .expect("a signal sent before the waiter registered must still arrive");
        });
    }

    /// The property only the CLI had: an action already dispatched can be
    /// called off, rather than the user waiting out a 30-second command.
    #[tokio::test]
    async fn work_already_in_flight_can_be_cancelled() {
        let t = CancelToken::new();
        let signal = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.cancel();
        });
        let outcome = t
            .run(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "finished"
            })
            .await;
        assert!(outcome.is_cancelled(), "the in-flight call must be dropped");
    }

    /// A token cancelled a moment ago must not dispatch anything, even work
    /// that would resolve instantly.
    #[tokio::test]
    async fn an_already_cancelled_token_refuses_to_start_work() {
        let t = CancelToken::new();
        t.cancel();
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let outcome = t
            .run(async move {
                flag.store(true, Ordering::SeqCst);
            })
            .await;
        assert!(outcome.is_cancelled());
        assert!(
            !ran.load(Ordering::SeqCst),
            "no work may start once cancellation is pending"
        );
    }

    /// Cancelling one turn must not poison the next.
    #[tokio::test]
    async fn reset_restores_the_token_for_the_next_turn() {
        let t = CancelToken::new();
        t.cancel();
        assert!(t.run(async { 1 }).await.is_cancelled());
        t.reset();
        assert_eq!(t.run(async { 1 }).await.completed(), Some(1));
    }

    /// Frontends hold clones; all of them must see the same signal.
    #[tokio::test]
    async fn clones_share_one_signal() {
        let a = CancelToken::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
        assert!(a.run(async { 1 }).await.is_cancelled());
    }

    /// Several awaiters — the stream, the bridge call, the iteration guard —
    /// must all wake, not just the first.
    #[tokio::test]
    async fn every_waiter_wakes() {
        let t = CancelToken::new();
        let mut handles = Vec::new();
        for _ in 0..3 {
            let t = t.clone();
            handles.push(tokio::spawn(async move {
                t.cancelled().await;
                true
            }));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        t.cancel();
        for h in handles {
            let woke = tokio::time::timeout(Duration::from_millis(200), h)
                .await
                .expect("waiter did not wake")
                .expect("task panicked");
            assert!(woke);
        }
    }
}
