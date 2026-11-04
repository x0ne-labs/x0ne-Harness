//! One place to turn a model-supplied size into an operator-approved one.
//!
//! SECURITY: `policy.max_read_bytes` is the operator's answer to "how much of
//! this machine may leave in one action". Before 2026-08-15 each reader
//! answered it separately, and they disagreed: `ReadFiles` clamped
//! (`efficient.rs:24-26`), `ReadFile` four files away did not, and `ReadLogs`,
//! `SearchCode`, `SearchAndRead` and `Diff{files:true}` had no byte ceiling at
//! all — only a line COUNT, which bounds nothing when the lines are long.
//! Measured against `max_read_bytes = 65536` on a 4 MiB file: 4 195 453 bytes
//! from `ReadFile`, 2 098 275 from `ReadLogs`, 87 297 702 from `SearchAndRead`.
//!
//! Everything that hands file *content* back to the model goes through here.
//! A new reader that does not is caught by
//! `tests/model_supplied_limits_are_clamped.rs`.

use crate::config::PolicyConfig;

/// The byte ceiling for one read: what the model asked for, never more than
/// what the operator allows.
///
/// SECURITY: `requested` is model-written text. The `.min()` is the whole
/// point of this function — an argument swap reads identically and inverts it.
pub fn effective_read_limit(requested: Option<u64>, policy: &PolicyConfig) -> u64 {
    requested
        .unwrap_or(policy.max_read_bytes)
        .min(policy.max_read_bytes)
}

/// A spending account for readers that assemble their answer piece by piece —
/// log tails, search hits, diff hunks — where no single `.min()` can bound the
/// total.
///
/// SECURITY: the counter is what bounds the payload; a line-count cap does not.
/// 512 matches of 4 KiB is 2 MiB, thirty times a 64 KiB budget, and that is
/// exactly what `SearchCode` returned before this existed.
pub struct ByteBudget {
    limit: usize,
    used: usize,
    exhausted: bool,
}

impl ByteBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit: limit as usize,
            used: 0,
            exhausted: false,
        }
    }

    /// True once a piece has been refused. Callers stop walking here and report
    /// `truncated` — the answer is short, and says so.
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Charge `n` bytes. Returns false — and latches `exhausted` — when the
    /// budget cannot cover them. Nothing partial is charged: a piece is either
    /// paid for whole or refused, so the caller never emits half a match.
    pub fn charge(&mut self, n: usize) -> bool {
        if self.exhausted || self.used.saturating_add(n) > self.limit {
            self.exhausted = true;
            return false;
        }
        self.used += n;
        true
    }
}

/// Cut a single oversized piece down to `max` bytes on a char boundary and say
/// so. Used where dropping the piece entirely would be worse than shortening it
/// — the last line of a log tail, a matched line in a search hit.
///
/// LIMIT, deliberately: this bounds what is *returned*, not what was *read*.
/// The line has already been assembled in memory by `BufRead::lines` /
/// `read_to_string`, so a single newline-free multi-gigabyte file still costs
/// that memory. Bounding the read itself is a separate change to every reader's
/// I/O layer; see the P7 review note.
pub fn truncate_to(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} bytes truncated]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_max(max: u64) -> PolicyConfig {
        PolicyConfig {
            max_read_bytes: max,
            ..PolicyConfig::default()
        }
    }

    /// A model asking for more than the policy allows gets the policy's number.
    /// A failure means `max_read_bytes` is advisory, which is not what an
    /// operator writing it believes.
    #[test]
    fn a_requested_limit_never_exceeds_the_policy_ceiling() {
        let p = policy_with_max(1000);
        assert_eq!(effective_read_limit(Some(u64::MAX), &p), 1000);
        assert_eq!(effective_read_limit(Some(1001), &p), 1000);
        assert_eq!(effective_read_limit(Some(10), &p), 10);
        assert_eq!(effective_read_limit(None, &p), 1000);
    }

    /// The budget latches: once refused, it stays refused even for a piece that
    /// would fit. A failure means a walker could emit small matches forever
    /// after a big one, which is the unbounded case wearing a smaller hat.
    #[test]
    fn an_exhausted_budget_stays_exhausted() {
        let mut b = ByteBudget::new(100);
        assert!(b.charge(60));
        assert!(!b.charge(60));
        assert!(b.exhausted());
        assert!(!b.charge(1));
    }

    /// A piece larger than the whole budget is refused, not truncated in place.
    #[test]
    fn a_piece_bigger_than_the_budget_is_refused_whole() {
        let mut b = ByteBudget::new(10);
        assert!(!b.charge(11));
        assert!(b.exhausted());
    }

    /// An oversized piece comes back shortened and labelled, never whole.
    /// A failure means a single 4 MiB line — one log record, one minified
    /// bundle — is returned in full by every caller that shortens rather than
    /// drops, and the budget those callers charge would be charged AFTER the
    /// fact, too late to bound anything.
    #[test]
    fn an_oversized_piece_comes_back_shortened_and_says_so() {
        let long = "x".repeat(1000);
        let cut = truncate_to(&long, 100);
        assert!(cut.len() < 200, "still {} bytes", cut.len());
        assert!(cut.starts_with(&"x".repeat(100)));
        assert!(
            cut.contains("truncated"),
            "truncation must be announced: {cut}"
        );
        assert_eq!(
            truncate_to("short", 100),
            "short",
            "a piece that fits is untouched"
        );
    }

    /// Cutting never lands inside a character. A failure is a panic on a
    /// slice boundary — a denial of service reachable by a log line with an
    /// accent in the wrong place.
    #[test]
    fn truncation_lands_on_a_character_boundary() {
        let s = "é".repeat(50); // 2 bytes each
        for max in 0..=100 {
            let _ = truncate_to(&s, max); // must not panic
        }
        // "aé" is 3 bytes; a cut at 2 lands inside the 'é', so it backs off to
        // 1 and the count reports the 2 bytes actually dropped.
        assert_eq!(truncate_to("aé", 2), "a…[2 bytes truncated]");
    }
}
