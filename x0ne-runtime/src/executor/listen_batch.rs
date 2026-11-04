//! Group a stream of lines into batches, never losing anything silently.
//!
//! Pure: no clock, no I/O, no task. The caller supplies the instant on each
//! call, which makes the whole policy testable without spawning a single
//! process.
//!
//! ## Why this module exists
//!
//! The target pattern is this: a program runs, its output scrolls, and each
//! interesting line becomes a conversation turn that the agent comments on.
//! The pattern breaks on its own as soon as the stream runs away — an agent
//! that comments five hundred lines per second is not watching, it is
//! drowning, and that is precisely the outage day when it was needed.
//!
//! Three guarantees, in this order:
//!
//! 1. **Group.** Lines close in time leave together. One batch, one turn.
//! 2. **Cap, and admit it.** Beyond `max_lines`, the batch carries the
//!    number of lines it does not show. Never a silent loss — this is
//!    already the rule of `MAX_STREAM_BYTES` right next door, which emits
//!    `{"stream": "truncated", "limit_bytes": …}` rather than cutting
//!    without a word.
//! 3. **Stop, and say so.** If the rate stays above the ceiling for a
//!    duration, listening ends with a verdict that carries the numbers. A
//!    listener that drowns the conversation is worse than no listener: it
//!    teaches not to read anymore.

use serde::{Deserialize, Serialize};

/// What decides the grouping.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BatchPolicy {
    /// Grouping window. Lines arriving within this interval leave in the
    /// same batch.
    pub window_ms: u64,
    /// Ceiling of lines SHOWN per batch. Beyond it, they are counted.
    pub max_lines: usize,
    /// Rate above which the stream is considered runaway, in lines per
    /// second.
    pub rate_per_s: usize,
    /// Duration this rate must hold before stopping.
    /// A short burst is normal; a wall that lasts is not.
    pub sustained_ms: u64,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self {
            window_ms: 400,
            max_lines: 20,
            rate_per_s: 50,
            sustained_ms: 5_000,
        }
    }
}

/// A batch ready to become a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub lines: Vec<String>,
    /// Lines held back by the ceiling, counted and announced. `0` means the
    /// batch is complete.
    pub dropped: usize,
    pub first_ms: u64,
    pub last_ms: u64,
}

impl Batch {
    /// Does the batch show everything it saw?
    pub fn is_complete(&self) -> bool {
        self.dropped == 0
    }
}

/// What the batcher answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    /// Nothing to do for now: the window is still running.
    Hold,
    /// A batch is ready.
    Emit(Batch),
    /// The stream has been runaway for too long; listening must stop.
    /// The numbers come with the verdict, so the refusal is readable.
    StopFlooding {
        lines_seen: usize,
        over_ms: u64,
        rate_per_s: usize,
    },
}

/// The state machine. One call per line, plus `tick`s when nothing arrives
/// — otherwise a stream that goes quiet would hold its last batch hostage.
#[derive(Debug)]
pub struct Batcher {
    policy: BatchPolicy,
    open: Vec<String>,
    dropped: usize,
    first_ms: Option<u64>,
    last_ms: u64,
    /// Since when the rate has exceeded the ceiling. `None` as soon as it
    /// drops back: this is what distinguishes a burst from a wall.
    over_since_ms: Option<u64>,
    /// One-second sliding window, to measure the rate.
    recent: std::collections::VecDeque<u64>,
    total_seen: usize,
}

impl Batcher {
    pub fn new(policy: BatchPolicy) -> Self {
        Self {
            policy,
            open: Vec::new(),
            dropped: 0,
            first_ms: None,
            last_ms: 0,
            over_since_ms: None,
            recent: std::collections::VecDeque::new(),
            total_seen: 0,
        }
    }

    /// Une ligne arrive.
    pub fn push(&mut self, line: String, now_ms: u64) -> Verdict {
        self.total_seen += 1;
        self.recent.push_back(now_ms);
        while self
            .recent
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) > 1_000)
        {
            self.recent.pop_front();
        }

        // The rate, measured over the elapsed second.
        if self.recent.len() > self.policy.rate_per_s {
            let since = *self.over_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= self.policy.sustained_ms {
                return Verdict::StopFlooding {
                    lines_seen: self.total_seen,
                    over_ms: now_ms.saturating_sub(since),
                    rate_per_s: self.recent.len(),
                };
            }
        } else {
            self.over_since_ms = None;
        }

        if self.first_ms.is_none() {
            self.first_ms = Some(now_ms);
        }
        self.last_ms = now_ms;

        if self.open.len() < self.policy.max_lines {
            self.open.push(line);
        } else {
            // Counted, not lost. The batch will say how many.
            self.dropped += 1;
        }

        self.tick(now_ms)
    }

    /// Since when a batch has been open, if there is one. This is what lets
    /// the caller sleep until the window's deadline instead of polling at a
    /// fixed interval — a silent stream then costs nothing.
    pub fn open_since_ms(&self) -> Option<u64> {
        self.first_ms
    }

    /// Time passes, with no line. Call regularly: without it, a stream that
    /// goes quiet right after a line would keep its batch indefinitely.
    pub fn tick(&mut self, now_ms: u64) -> Verdict {
        let Some(first) = self.first_ms else {
            return Verdict::Hold;
        };
        if now_ms.saturating_sub(first) < self.policy.window_ms {
            return Verdict::Hold;
        }
        Verdict::Emit(self.take(now_ms))
    }

    /// Flush what remains, whatever happens — at the end of the stream, or
    /// when listening stops. Returns `None` if there is nothing.
    pub fn flush(&mut self, now_ms: u64) -> Option<Batch> {
        self.first_ms?;
        Some(self.take(now_ms))
    }

    fn take(&mut self, now_ms: u64) -> Batch {
        Batch {
            lines: std::mem::take(&mut self.open),
            dropped: std::mem::take(&mut self.dropped),
            first_ms: self.first_ms.take().unwrap_or(now_ms),
            last_ms: self.last_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> BatchPolicy {
        BatchPolicy {
            window_ms: 400,
            max_lines: 3,
            rate_per_s: 10,
            sustained_ms: 2_000,
        }
    }

    /// Close lines leave together: one batch, one turn. Without this,
    /// ten server startup lines would make ten turns.
    #[test]
    fn lines_close_in_time_leave_in_one_batch() {
        let mut b = Batcher::new(p());
        assert_eq!(b.push("a".into(), 0), Verdict::Hold);
        assert_eq!(b.push("b".into(), 100), Verdict::Hold);
        let Verdict::Emit(batch) = b.push("c".into(), 400) else {
            panic!("the window has elapsed, the batch must go out");
        };
        assert_eq!(batch.lines, ["a", "b", "c"]);
        assert!(batch.is_complete());
    }

    /// Read SAFETY: beyond the ceiling, lines are COUNTED.
    ///
    /// A failure here means a batch shows three lines out of fifty without
    /// saying so — and the agent would comment on an excerpt believing it
    /// comments on the whole. Same rule as `MAX_STREAM_BYTES`, which emits
    /// `{"stream": "truncated"}` rather than cutting silently.
    #[test]
    fn a_batch_declares_the_lines_it_does_not_show() {
        let mut b = Batcher::new(p());
        for i in 0..10 {
            let _ = b.push(format!("l{i}"), i * 10);
        }
        let batch = b.flush(500).expect("a batch");
        assert_eq!(batch.lines.len(), 3, "the cap holds");
        assert_eq!(batch.dropped, 7, "and the other seven are counted");
        assert!(!batch.is_complete());
    }

    /// A wall that lasts stops listening, with the numbers. A listener that
    /// drowns the conversation is worse than no listener: it teaches not to
    /// read anymore.
    #[test]
    fn a_sustained_flood_stops_the_listener_and_says_so() {
        let mut b = Batcher::new(p());
        let mut last = Verdict::Hold;
        // 100 lines/s for 3 s: ten times the ceiling.
        for i in 0..300u64 {
            last = b.push(format!("flood{i}"), i * 10);
            if matches!(last, Verdict::StopFlooding { .. }) {
                break;
            }
        }
        let Verdict::StopFlooding {
            lines_seen,
            over_ms,
            rate_per_s,
        } = last
        else {
            panic!("the stream has been runaway for 3 s, listening must stop");
        };
        assert!(over_ms >= p().sustained_ms);
        assert!(rate_per_s > p().rate_per_s);
        assert!(lines_seen > 0);
    }

    /// A BURST is not a wall. A hundred lines at once when a server starts,
    /// then calm: listening continues. Without this distinction, every
    /// `npm install` would kill the monitor.
    #[test]
    fn a_short_burst_is_not_a_flood_and_does_not_stop_anything() {
        let mut b = Batcher::new(p());
        for i in 0..100u64 {
            let v = b.push(format!("burst{i}"), i);
            assert!(
                !matches!(v, Verdict::StopFlooding { .. }),
                "a 100 ms burst is not a sustained rate"
            );
        }
        // Then calm: one line per second.
        for i in 0..5u64 {
            let v = b.push(format!("calme{i}"), 1_000 + i * 1_000);
            assert!(!matches!(v, Verdict::StopFlooding { .. }));
        }
    }

    /// A stream that goes quiet does not hold its batch hostage: `tick`
    /// releases it once the window has passed.
    #[test]
    fn a_stream_that_goes_quiet_still_releases_its_batch() {
        let mut b = Batcher::new(p());
        assert_eq!(b.push("alone".into(), 0), Verdict::Hold);
        assert_eq!(b.tick(100), Verdict::Hold, "the window is still running");
        let Verdict::Emit(batch) = b.tick(400) else {
            panic!("the window has passed");
        };
        assert_eq!(batch.lines, ["alone"]);
    }

    /// `tick` on an empty batcher does not produce an empty batch — otherwise
    /// the agent would be woken for nothing, in a loop.
    #[test]
    fn nothing_to_say_wakes_nobody() {
        let mut b = Batcher::new(p());
        for t in [0, 1_000, 10_000, 100_000] {
            assert_eq!(b.tick(t), Verdict::Hold);
        }
        assert!(b.flush(200_000).is_none());
    }

    /// The count is exact over several consecutive batches: what is held
    /// back in one batch does not carry over to the next.
    #[test]
    fn the_dropped_count_does_not_leak_from_one_batch_to_the_next() {
        let mut b = Batcher::new(p());
        for i in 0..6u64 {
            let _ = b.push(format!("a{i}"), i);
        }
        let Verdict::Emit(first) = b.push("declencheur".into(), 400) else {
            panic!("the window has passed");
        };
        assert_eq!(first.dropped, 4, "6 + 1 lines, 3 shown");
        let _ = b.push("b0".into(), 500);
        let batch = b.flush(1_000).expect("a second batch");
        assert_eq!(batch.dropped, 0, "the second batch starts from zero");
        assert_eq!(batch.lines, ["b0"]);
    }
}
