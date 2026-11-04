//! Exercise 03 — the hard one. `cargo test` PASSES.
//!
//! There is still a real bug. The existing tests simply never ask the
//! question that would expose it. The job is to find it, write the test that
//! fails because of it, and then fix it — in that order, so the test proves
//! the fix rather than merely accompanying it.
//!
//! Hint about method, not about the answer: read what the doc comments
//! PROMISE, then ask whether the code keeps that promise for every input the
//! promise covers.

/// A fixed-size window over the most recent samples.
///
/// Holds at most `capacity` values. Pushing into a full window drops the
/// OLDEST value, so the window always reflects the most recent `capacity`
/// pushes.
pub struct Window {
    capacity: usize,
    values: Vec<i64>,
}

impl Window {
    /// A window holding at most `capacity` samples. A zero capacity holds
    /// nothing and accepts pushes without storing them.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: Vec::new(),
        }
    }

    /// Add a sample, evicting the oldest if the window is already full.
    pub fn push(&mut self, v: i64) {
        if self.capacity == 0 {
            return;
        }
        if self.values.len() == self.capacity {
            self.values.remove(0);
        }
        self.values.push(v);
    }

    /// The samples, oldest first.
    pub fn samples(&self) -> &[i64] {
        &self.values
    }

    /// Mean of the samples, or `None` when the window is empty.
    pub fn mean(&self) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let sum: i64 = self.values.iter().sum();
        Some(sum as f64 / self.values.len() as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_up_to_capacity() {
        let mut w = Window::new(3);
        w.push(1);
        w.push(2);
        w.push(3);
        assert_eq!(w.samples(), &[1, 2, 3]);
    }

    #[test]
    fn evicts_the_oldest_when_full() {
        let mut w = Window::new(3);
        for v in [1, 2, 3, 4] {
            w.push(v);
        }
        assert_eq!(w.samples(), &[2, 3, 4]);
    }

    #[test]
    fn mean_of_an_empty_window_is_none() {
        assert_eq!(Window::new(3).mean(), None);
    }

    #[test]
    fn zero_capacity_accepts_pushes_without_storing() {
        let mut w = Window::new(0);
        w.push(1);
        w.push(2);
        assert_eq!(w.samples(), &[]);
    }

    #[test]
    fn mean_averages_the_samples() {
        let mut w = Window::new(3);
        w.push(2);
        w.push(4);
        assert_eq!(w.mean(), Some(3.0));
    }
}
