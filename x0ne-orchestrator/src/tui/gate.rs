//! The blocking half of the fullscreen confirmation round-trip.
//!
//! The canonical gate is synchronous: `confirm::decide` calls
//! `DisplaySink::request_confirmation` and expects an answer before it
//! returns. A fullscreen TUI answers asynchronously: it renders a modal and,
//! some seconds later, a key press produces a `TuiCommand::ConfirmationAnswer`
//! on a channel. This module is the adapter between those two shapes — the
//! owner-selected Option A: the sink emits the question as an event, then
//! **blocks its own thread** until the router feeds the answer back.
//!
//! ## Threading contract
//!
//! `wait()` parks the calling OS thread. That is safe here for one reason:
//! the turn runs inside `tokio::task::spawn` on a **multi-thread** runtime,
//! while the terminal event loop and the command router live on other tasks —
//! so the UI keeps rendering, keys keep arriving, and one worker thread
//! sleeping is the whole cost. On a current-thread runtime the same call
//! would park the only thread that could ever deliver the answer, so `wait()`
//! refuses to block there and fails closed instead. The guard is in code, not
//! in a comment.
//!
//! ## Fail-closed properties
//!
//! - No answer channel (router dropped, UI gone): `wait` returns `None`.
//! - Cancellation: the router calls [`HumanGate::abandon`], every parked
//!   waiter wakes with `None`, and the canonical gate turns that into a
//!   denial.
//! - Stale answer: an id that does not match the outstanding question is
//!   dropped by the router before it ever reaches a waiter — and the
//!   canonical `ApprovalAuthority` re-validates its own request id after
//!   that, so correlation is enforced twice.
//! - One question at a time: opening a new question abandons the previous
//!   one, mirroring `ApprovalAuthority::open_request` superseding the
//!   outstanding request.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;

/// What the human said, in wire terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireAnswer {
    pub approved: bool,
    pub always: bool,
}

/// What the human answered on an `AskUser` modal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AskAnswer {
    pub cancelled: bool,
    pub selected: Vec<String>,
    pub custom_text: Option<String>,
}

struct PendingQuestion<T> {
    wire_id: String,
    tx: SyncSender<T>,
}

/// The shared slot between a blocking waiter (the sink or the AskUser
/// executor, on the turn's thread) and the command router (on its own task).
///
/// Generic over the answer type: confirmations carry a [`WireAnswer`], the
/// AskUser modal an [`AskAnswer`]. Same correlation, staleness and
/// abandonment mechanics for both, implemented once.
pub struct Gate<T> {
    pending: Mutex<Option<PendingQuestion<T>>>,
    counter: std::sync::atomic::AtomicU64,
}

/// The confirmation gate.
pub type HumanGate = Gate<WireAnswer>;
/// The AskUser modal gate.
pub type AskGate = Gate<AskAnswer>;

impl<T> Default for Gate<T> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(None),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl<T: Send> Gate<T> {
    /// Mint a wire id. Opaque and session-local; correlation is by equality.
    pub fn next_wire_id(&self, prefix: &str) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{prefix}-{n}")
    }

    /// Open a question and hand back the receiver its answer will arrive on.
    ///
    /// Supersedes any previous outstanding question: its waiter wakes with
    /// `None` (sender dropped), which it must treat as a denial.
    pub fn open(&self, wire_id: String) -> Receiver<T> {
        let (tx, rx) = sync_channel(1);
        *self.pending.lock().unwrap() = Some(PendingQuestion { wire_id, tx });
        rx
    }

    /// Deliver an answer from the router. A stale or unknown id is dropped —
    /// the question it answers no longer exists, and honouring it late would
    /// approve something that was already treated as refused.
    ///
    /// Returns whether the answer was accepted, so the router can log drops.
    pub fn answer(&self, wire_id: &str, answer: T) -> bool {
        let mut guard = self.pending.lock().unwrap();
        match guard.as_ref() {
            Some(p) if p.wire_id == wire_id => {
                let p = guard.take().expect("just matched");
                // A full buffer means the waiter already gave up; dropping the
                // answer is then correct for the same staleness reason.
                p.tx.try_send(answer).is_ok()
            }
            _ => false,
        }
    }

    /// Abandon the outstanding question, waking its waiter with `None`.
    /// Used on cancellation and shutdown. Nothing is granted.
    pub fn abandon(&self) {
        self.pending.lock().unwrap().take();
    }

    /// Block until the answer arrives, the question is abandoned, or the
    /// router disappears. `None` is always a denial for the caller.
    ///
    /// Refuses to block on a current-thread tokio runtime: parking the only
    /// worker would also park the router that must deliver the answer, so the
    /// wait could never end. Failing closed is the only correct behaviour
    /// there, and it is enforced here rather than documented and hoped for.
    pub fn wait(&self, rx: Receiver<T>) -> Option<T> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
                self.abandon();
                return None;
            }
        }
        rx.recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn an_answer_reaches_the_waiter() {
        let gate = Arc::new(HumanGate::default());
        let rx = gate.open("q-1".into());
        assert!(gate.answer(
            "q-1",
            WireAnswer {
                approved: true,
                always: false
            }
        ));
        assert_eq!(
            rx.recv().ok(),
            Some(WireAnswer {
                approved: true,
                always: false
            })
        );
    }

    /// The stale-answer property: an id that is not the outstanding question
    /// is dropped, and the real question still gets its real answer.
    #[test]
    fn a_stale_answer_is_dropped_not_applied() {
        let gate = HumanGate::default();
        let rx = gate.open("q-2".into());
        assert!(
            !gate.answer(
                "q-1",
                WireAnswer {
                    approved: true,
                    always: true
                }
            ),
            "an answer to an abandoned question was accepted"
        );
        assert!(gate.answer(
            "q-2",
            WireAnswer {
                approved: false,
                always: false
            }
        ));
        assert_eq!(
            rx.recv().ok(),
            Some(WireAnswer {
                approved: false,
                always: false
            })
        );
    }

    /// Abandonment wakes the waiter with nothing, which the caller must read
    /// as a denial. This is the cancellation and shutdown path.
    #[test]
    fn abandoning_wakes_the_waiter_with_a_denial() {
        let gate = HumanGate::default();
        let rx = gate.open("q-3".into());
        gate.abandon();
        assert_eq!(rx.recv().ok(), None);
        // And a late answer to the abandoned question is refused.
        assert!(!gate.answer(
            "q-3",
            WireAnswer {
                approved: true,
                always: false
            }
        ));
    }

    /// A second question supersedes the first: the first waiter is woken with
    /// a denial rather than left parked forever, and only the new id answers.
    #[test]
    fn a_new_question_supersedes_the_old_one() {
        let gate = HumanGate::default();
        let old_rx = gate.open("q-4".into());
        let new_rx = gate.open("q-5".into());
        assert_eq!(
            old_rx.recv().ok(),
            None,
            "the superseded waiter kept waiting"
        );
        assert!(!gate.answer(
            "q-4",
            WireAnswer {
                approved: true,
                always: false
            }
        ));
        assert!(gate.answer(
            "q-5",
            WireAnswer {
                approved: true,
                always: false
            }
        ));
        assert!(new_rx.recv().is_ok());
    }

    /// The threading guard: on a current-thread runtime, blocking would park
    /// the only thread able to deliver the answer, so the gate fails closed
    /// instead of deadlocking.
    #[tokio::test]
    async fn wait_fails_closed_on_a_current_thread_runtime() {
        let gate = HumanGate::default();
        let rx = gate.open("q-6".into());
        assert_eq!(gate.wait(rx), None);
        // The question was abandoned, so even an immediate answer is stale.
        assert!(!gate.answer(
            "q-6",
            WireAnswer {
                approved: true,
                always: false
            }
        ));
    }

    /// And on a multi-thread runtime the same call genuinely waits and gets
    /// the answer another thread delivers — the production shape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_receives_an_answer_delivered_from_another_task() {
        let gate = Arc::new(HumanGate::default());
        let rx = gate.open("q-7".into());
        let answering = Arc::clone(&gate);
        let answerer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(answering.answer(
                "q-7",
                WireAnswer {
                    approved: true,
                    always: true
                }
            ));
        });
        let got = tokio::task::spawn_blocking({
            let gate = Arc::clone(&gate);
            move || gate.wait(rx)
        })
        .await
        .unwrap();
        assert_eq!(
            got,
            Some(WireAnswer {
                approved: true,
                always: true
            })
        );
        answerer.await.unwrap();
    }
}
