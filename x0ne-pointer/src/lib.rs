//! Deterministic pointer trajectories.
//!
//! Turn a start and an end into the sequence of `mousemove` events a real
//! pointer would produce along the way. Pure: no I/O, no clock, no randomness
//! the caller cannot reproduce.
//!
//! ## Why this exists
//!
//! Both drivers in this repository teleported. `x0ne-browser` clicked through
//! chromiumoxide's `element.click()`, which scrolls the element into view and
//! dispatches a click at its centre — **no intermediate movement at all**.
//! `x0ne-vm` sent a single RFB `PointerEvent` at the destination. Measured
//! 2026-09-10, both.
//!
//! A pointer that teleports cannot drive:
//!
//! - a menu that opens on `mouseover` and closes when the pointer leaves;
//! - a drag, which needs `mousedown`, then movement, then `mouseup` — the
//!   deltas in between are the gesture;
//! - a canvas, where the path IS the input;
//! - anything that reacts to pointer velocity or direction.
//!
//! That is what this module buys: **correctness on interfaces that depend on
//! movement**. It is not a way to look human to a bot detector, and it should
//! not be used as one — a detector that a Bezier curve fools was not
//! detecting anything.
//!
//! ## Why deterministic
//!
//! A path built from a fresh RNG cannot be tested: you can assert it starts
//! and ends in the right place and nothing else. Every path here is a pure
//! function of `(from, to, profile, seed)`, so a test can pin an exact
//! sequence, and a failure that only shows up on one path can be replayed by
//! writing down its seed.

use serde::{Deserialize, Serialize};

/// One step of a trajectory: where the pointer is, and how long since the
/// previous step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub x: i32,
    pub y: i32,
    /// Milliseconds to wait BEFORE dispatching this step.
    ///
    /// NOT zero on the first, and that is deliberate: at `t = 0` the pointer
    /// is still at `from` and no event is owed. The first emitted position is
    /// one interval later, because it is a position the pointer has MOVED to.
    /// (This doc-comment said "zero on the first" when it was written, and a
    /// test asserted it — both were wrong about the code beneath them.)
    pub dt_ms: u32,
}

/// How the pointer moves. The defaults describe an unhurried, accurate hand.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Pixels per step at cruising speed. Smaller = more events = smoother
    /// and slower to dispatch.
    pub px_per_step: f64,
    /// Total travel time for a 500 px move, in milliseconds. Shorter moves
    /// take proportionally less, but never less than `min_total_ms`.
    pub ms_per_500px: f64,
    /// Floor on the total duration, so a two-pixel nudge still produces a
    /// gesture rather than a teleport with extra steps.
    pub min_total_ms: f64,
    /// How far the path bows away from the straight line, as a fraction of
    /// the distance. `0.0` is a ruler; humans are not rulers.
    pub bow: f64,
    /// Peak perpendicular wobble in pixels, on top of the bow.
    pub jitter_px: f64,
    /// Hard cap on the number of steps, so a 4000 px drag across two
    /// monitors cannot emit a thousand events.
    pub max_steps: usize,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            px_per_step: 12.0,
            ms_per_500px: 420.0,
            min_total_ms: 60.0,
            bow: 0.08,
            jitter_px: 1.5,
            max_steps: 64,
        }
    }
}

impl Profile {
    /// No bow, no wobble, minimum steps: the old teleporting behaviour, kept
    /// addressable so a caller that wants speed over fidelity can ask for it
    /// by name instead of bypassing this module.
    pub fn direct() -> Self {
        Self {
            bow: 0.0,
            jitter_px: 0.0,
            px_per_step: f64::MAX,
            // And FAST. The first version kept the default profile's
            // duration: two steps spread over a second and a half, which is
            // neither a path nor a jump. A profile that announces « speed
            // over fidelity » and takes the same time does not keep its
            // word.
            ms_per_500px: 0.0,
            min_total_ms: 0.0,
            ..Self::default()
        }
    }
}

/// The rectangle the pointer may not leave, in device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    fn clamp(&self, x: f64, y: f64) -> (i32, i32) {
        let cx = x.round().clamp(0.0, (self.width - 1).max(0) as f64) as i32;
        let cy = y.round().clamp(0.0, (self.height - 1).max(0) as f64) as i32;
        (cx, cy)
    }
}

/// A tiny deterministic generator.
///
/// Not for cryptography and never used for it — this decides where a wobble
/// goes. What matters is that the same seed yields the same path on every
/// machine and every run, which `rand`'s thread RNG would not give us.
fn noise(seed: u64, i: u64) -> f64 {
    // SplitMix64, canonical form: the counter is ADDED to the multiple of the
    // golden constant, not multiplied by it.
    //
    // The first version computed `(seed + GOLDEN) * (i + 1)`. For `i = 0`
    // that equals `seed + GOLDEN` — nearly constant on small seeds, and the
    // mixing rounds on neighbouring inputs yield correlated outputs.
    // Measured on 2026-09-10: the bow side, drawn from `noise(seed, 0)`,
    // fell to the RIGHT for 10 seeds out of 12. That was not a draw, it was
    // a bias, and it was visible to the eye on two side-by-side paths.
    let mut z = seed.wrapping_add(i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // → [-1.0, 1.0)
    //
    // `>> 11` leaves 53 bits, so the division is by 2^53. The first version
    // divided by 2^52: the range was [-1, 3) and the output was negative
    // only a quarter of the time. Visible consequences, measured on
    // 2026-09-10: the bow went right 3 times out of 4, and the wobble leaned
    // the same way. No endpoint test could see it.
    (z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

/// Ease in and out: slow at both ends, quick in the middle.
///
/// A hand accelerates away from where it was and decelerates onto the
/// target; a linear ramp reads as machine-driven to anything that measures
/// velocity, and — more to the point — never dwells near the target, which
/// is where hover-triggered UI needs it to dwell.
fn ease(t: f64) -> f64 {
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
    }
}

/// The trajectory from `from` to `to`.
///
/// The last step is ALWAYS exactly `to`, whatever the bow and the wobble did
/// on the way — a path that lands one pixel off its target is a bug, not a
/// flourish, and it would make every click flaky.
///
/// Returns an empty vector when the pointer is already there: nothing to
/// dispatch, and emitting a zero-length move would fire spurious `mousemove`
/// handlers.
pub fn path(from: (i32, i32), to: (i32, i32), bounds: Bounds, p: &Profile, seed: u64) -> Vec<Step> {
    let (x0, y0) = (from.0 as f64, from.1 as f64);
    let (x1, y1) = (to.0 as f64, to.1 as f64);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 0.5 {
        return Vec::new();
    }

    let steps = ((dist / p.px_per_step).ceil() as usize).clamp(2, p.max_steps.max(2));
    let total_ms = (dist / 500.0 * p.ms_per_500px).max(p.min_total_ms);

    // The control point sits off to one side of the midpoint, so the path
    // bows instead of ruling. Which side is decided by the seed, not by the
    // geometry, or every path in a session would curve the same way.
    let (nx, ny) = (-dy / dist, dx / dist);
    let side = if noise(seed, 0) < 0.0 { -1.0 } else { 1.0 };
    let bow = dist * p.bow * side;
    let (cx, cy) = ((x0 + x1) / 2.0 + nx * bow, (y0 + y1) / 2.0 + ny * bow);

    let mut out = Vec::with_capacity(steps);
    let mut prev_ms = 0.0f64;
    for i in 1..=steps {
        let t = ease(i as f64 / steps as f64);
        // Quadratic Bezier through the offset control point.
        let u = 1.0 - t;
        let mut px = u * u * x0 + 2.0 * u * t * cx + t * t * x1;
        let mut py = u * u * y0 + 2.0 * u * t * cy + t * t * y1;

        // The wobble fades to nothing as we approach the target, so the last
        // steps converge instead of hunting around it.
        if i < steps {
            let fade = 1.0 - t;
            let w = noise(seed, i as u64) * p.jitter_px * fade;
            px += nx * w;
            py += ny * w;
        }

        let (mut ix, mut iy) = bounds.clamp(px, py);
        if i == steps {
            // Non-negotiable: we land ON the target.
            ix = to.0;
            iy = to.1;
        }
        let at_ms = total_ms * (i as f64 / steps as f64);
        out.push(Step {
            x: ix,
            y: iy,
            dt_ms: (at_ms - prev_ms).round().max(0.0) as u32,
        });
        prev_ms = at_ms;
    }
    out
}

/// Total wall time a trajectory will take, for a caller that needs to budget
/// it (a timeout, a progress line) without walking the steps.
pub fn duration_ms(steps: &[Step]) -> u32 {
    steps.iter().map(|s| s.dt_ms).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Bounds = Bounds {
        width: 1920,
        height: 1080,
    };

    /// The one property no caller may ever have to check: the pointer ends
    /// exactly where it was told, whatever the bow and the wobble did.
    ///
    /// A failure here makes every click in both drivers flaky by a pixel or
    /// two, which is the worst kind of bug — it works on the developer's
    /// machine and fails on a dense element.
    #[test]
    fn the_last_step_is_always_exactly_the_target() {
        let p = Profile::default();
        for (from, to) in [
            ((0, 0), (1919, 1079)),
            ((960, 540), (961, 541)),
            ((1919, 1079), (0, 0)),
            ((100, 100), (100, 900)),
            ((500, 500), (5, 500)),
        ] {
            for seed in 0..8u64 {
                let steps = path(from, to, B, &p, seed);
                let last = steps.last().expect("a move must produce steps");
                assert_eq!((last.x, last.y), to, "from {from:?} seed {seed}");
            }
        }
    }

    /// Same inputs, same path — on every machine and every run.
    ///
    /// This is what makes the rest of the tests possible. A path built from a
    /// thread RNG could only ever be asserted on its endpoints.
    #[test]
    fn the_same_seed_yields_the_same_path() {
        let p = Profile::default();
        let a = path((10, 10), (800, 600), B, &p, 42);
        let b = path((10, 10), (800, 600), B, &p, 42);
        assert_eq!(a, b);
        let c = path((10, 10), (800, 600), B, &p, 43);
        assert_ne!(
            a, c,
            "a different seed must bow the other way or wobble differently"
        );
    }

    /// The bow side is really drawn, not biased.
    ///
    /// The first version of `noise` was `(seed + GOLDEN) * (i + 1)`: for
    /// `i = 0` the input was nearly constant and the side fell to the right
    /// 10 times out of 12. Two side-by-side paths curved the same way, and
    /// it was visible to the eye on a screenshot.
    ///
    /// A failure here means the mixer has become degenerate again on its
    /// first call — which breaks no endpoint test, only the appearance.
    #[test]
    fn the_bow_picks_its_side_without_a_bias() {
        let p = Profile::default();
        let mut left = 0;
        let n = 200u64;
        for seed in 0..n {
            let s = path((100, 500), (900, 500), B, &p, seed);
            // A horizontal path: the midpoint passes above or below.
            let mid = s[s.len() / 2];
            if mid.y < 500 {
                left += 1;
            }
        }
        assert!(
            (n / 4..=3 * n / 4).contains(&left),
            "{left} upward arcs out of {n} — the draw is biased"
        );
    }

    /// A pointer already at its target emits nothing.
    ///
    /// Dispatching a zero-length move would fire `mousemove` handlers for a
    /// gesture that did not happen — an agent asking "hover this, it is
    /// already hovered" must not re-trigger the page.
    #[test]
    fn a_move_to_where_we_already_are_emits_nothing() {
        let p = Profile::default();
        assert!(path((640, 480), (640, 480), B, &p, 1).is_empty());
    }

    /// The path never leaves the viewport, even when the bow would push it
    /// out. A negative coordinate is silently dropped by CDP and rejected by
    /// RFB — either way the gesture breaks, far from here.
    #[test]
    fn the_path_stays_inside_the_bounds() {
        let p = Profile {
            bow: 0.9,
            jitter_px: 20.0,
            ..Profile::default()
        };
        for seed in 0..16u64 {
            // A move hugging the top edge: the bow wants to go above y = 0.
            for s in path((10, 2), (1900, 2), B, &p, seed) {
                assert!(
                    (0..B.width).contains(&s.x) && (0..B.height).contains(&s.y),
                    "step {s:?} left the viewport (seed {seed})"
                );
            }
        }
    }

    /// Time only moves forward, and a longer move takes longer.
    #[test]
    fn time_is_monotonic_and_grows_with_distance() {
        let p = Profile::default();
        let short = path((0, 0), (60, 0), B, &p, 7);
        let long = path((0, 0), (1600, 900), B, &p, 7);
        assert!(duration_ms(&short) < duration_ms(&long));
        assert!(duration_ms(&short) >= p.min_total_ms as u32 - 1);
        assert!(
            short[0].dt_ms > 0,
            "the first step is a position already MOVED to, so it owes a delay"
        );
    }

    /// Step count follows distance, and the cap holds. Without the cap a
    /// drag across a 4K screen would emit hundreds of events and take
    /// seconds of round trips.
    #[test]
    fn step_count_follows_distance_up_to_the_cap() {
        let p = Profile::default();
        let near = path((0, 0), (30, 0), B, &p, 3).len();
        let far = path((0, 0), (1900, 1000), B, &p, 3).len();
        assert!(near < far, "{near} !< {far}");
        assert!(far <= p.max_steps);
        let capped = Profile { max_steps: 5, ..p };
        assert!(path((0, 0), (1900, 1000), B, &capped, 3).len() <= 5);
    }

    /// The escape hatch is honest: `direct` still lands on target and still
    /// respects the minimum duration — it just stops pretending to be a hand.
    #[test]
    fn the_direct_profile_is_a_straight_line_that_still_lands() {
        let p = Profile::direct();
        let steps = path((0, 0), (400, 300), B, &p, 5);
        assert_eq!(
            (steps.last().unwrap().x, steps.last().unwrap().y),
            (400, 300)
        );
        // Straight: every step sits on the line within a rounding pixel.
        for s in &steps {
            let t = s.x as f64 / 400.0;
            assert!(
                (s.y as f64 - t * 300.0).abs() <= 1.0,
                "step {s:?} is off the straight line"
            );
        }
    }

    /// The wobble fades out: the last few steps converge on the target
    /// instead of circling it. Measured as "the second-to-last step is
    /// closer than the third-to-last".
    #[test]
    fn the_wobble_fades_as_the_pointer_arrives() {
        let p = Profile {
            jitter_px: 8.0,
            ..Profile::default()
        };
        let to = (1200, 800);
        for seed in 0..8u64 {
            let s = path((0, 0), to, B, &p, seed);
            let d = |k: usize| {
                let st = s[s.len() - k];
                (((st.x - to.0).pow(2) + (st.y - to.1).pow(2)) as f64).sqrt()
            };
            assert!(d(2) < d(3), "seed {seed}: {} !< {}", d(2), d(3));
        }
    }
}
