//! Token bucket rate limiter — N calls per minute, with a soft throttle when
//! the bucket is empty (caller is asked to slow down rather than refused).

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    /// Max tokens (= max calls in any 60s window).
    capacity: u32,
    /// Refill rate, tokens per second.
    refill_per_sec: f64,
    state: Mutex<State>,
}

struct State {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// `per_minute = 60` means 60 calls/min, 1 token/sec refill.
    pub fn new(per_minute: u32) -> Self {
        let capacity = per_minute.max(1);
        let refill_per_sec = capacity as f64 / 60.0;
        Self {
            capacity,
            refill_per_sec,
            state: Mutex::new(State {
                tokens: capacity as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Try to consume one token. Returns:
    ///   `Ok(())` — consumed
    ///   `Err(retry_after)` — bucket empty, wait `retry_after` before next try.
    pub fn try_consume(&self) -> Result<(), Duration> {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        let dt = now.duration_since(s.last_refill).as_secs_f64();
        s.tokens = (s.tokens + dt * self.refill_per_sec).min(self.capacity as f64);
        s.last_refill = now;
        if s.tokens >= 1.0 {
            s.tokens -= 1.0;
            Ok(())
        } else {
            let need = 1.0 - s.tokens;
            Err(Duration::from_secs_f64(need / self.refill_per_sec))
        }
    }

    pub fn snapshot(&self) -> (f64, u32) {
        let s = self.state.lock().unwrap();
        (s.tokens, self.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn consumes_up_to_capacity() {
        let rl = RateLimiter::new(5); // 5/min ≈ 1/12s
        for _ in 0..5 {
            assert!(rl.try_consume().is_ok());
        }
        assert!(rl.try_consume().is_err()); // bucket empty
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new(60); // 60/min = 1/sec
        for _ in 0..60 {
            let _ = rl.try_consume();
        }
        sleep(Duration::from_millis(1100));
        // After ~1.1s we should have ~1.1 token regenerated
        assert!(rl.try_consume().is_ok());
    }

    #[test]
    fn retry_after_is_reasonable() {
        let rl = RateLimiter::new(60);
        for _ in 0..60 {
            let _ = rl.try_consume();
        }
        let err = rl.try_consume().unwrap_err();
        // 1/sec rate, we asked just after spending the last token, retry
        // should be approximately 1 second (well under 5s)
        assert!(err.as_secs_f64() < 5.0);
        assert!(err.as_secs_f64() > 0.0);
    }
}
