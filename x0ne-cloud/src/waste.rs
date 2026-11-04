//! The idle-GPU waste predicate.
//!
//! Waste iff util 0% AND no attached process AND the resource is priced AND
//! idle beyond the threshold. Any other combination is not waste — a 0% GPU
//! with a process attached is a job between steps (the 2026-08-08 rule:
//! measure what is USED), and an unpriced resource can't have a dollar figure
//! claimed against it. Pure predicate; the *timing* is tracked by the caller.

use crate::gpu::GpuState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasteVerdict {
    pub is_wasting: bool,
    pub reason: String,
    pub hourly_usd: f64,
}

pub fn assess_waste(
    gpu: &GpuState,
    resource_hourly: Option<f64>,
    idle_secs: u64,
    threshold_secs: u64,
) -> WasteVerdict {
    let hourly = resource_hourly.unwrap_or(0.0);
    let is_wasting = gpu.util_pct == 0
        && gpu.processes.is_empty()
        && resource_hourly.is_some()
        && idle_secs >= threshold_secs;
    let reason = if is_wasting {
        format!(
            "GPU {} idle 0% with no process for {}min at ${:.2}/h",
            gpu.index,
            idle_secs / 60,
            hourly
        )
    } else {
        String::new()
    };
    WasteVerdict {
        is_wasting,
        reason,
        hourly_usd: hourly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{GpuProc, GpuState};

    fn gpu(util: u32, procs: Vec<GpuProc>) -> GpuState {
        GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: util,
            mem_used_mb: 0,
            mem_total_mb: 81559,
            temp_c: 33,
            power_w: 70,
            processes: procs,
        }
    }

    fn proc() -> GpuProc {
        GpuProc {
            pid: 1,
            name: "python".into(),
            mem_mb: 1000,
        }
    }

    #[test]
    fn idle_processless_priced_and_past_threshold_is_waste() {
        let v = assess_waste(&gpu(0, vec![]), Some(11.26), 25 * 60, 20 * 60);
        assert!(v.is_wasting);
        assert!((v.hourly_usd - 11.26).abs() < 1e-9);
        assert!(v.reason.contains("idle"));
    }

    #[test]
    fn idle_but_with_a_process_is_not_waste() {
        // A job between steps. The 2026-08-08 rule: measure what is USED.
        let v = assess_waste(&gpu(0, vec![proc()]), Some(11.26), 25 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn busy_gpu_is_not_waste() {
        let v = assess_waste(&gpu(87, vec![proc()]), Some(11.26), 25 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn unpriced_gpu_never_trips() {
        // Can't claim dollars wasted on a resource we couldn't price.
        let v = assess_waste(&gpu(0, vec![]), None, 25 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn below_threshold_is_not_yet_waste() {
        let v = assess_waste(&gpu(0, vec![]), Some(11.26), 5 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn exactly_at_threshold_trips() {
        // The boundary: idle_secs == threshold. `>=` means this IS waste;
        // a `>` would leave a one-tick dead zone at the threshold.
        let v = assess_waste(&gpu(0, vec![]), Some(11.26), 20 * 60, 20 * 60);
        assert!(v.is_wasting);
    }

    #[test]
    fn busy_gpu_without_a_process_is_not_waste() {
        // util > 0 alone clears it, regardless of the (odd) empty process
        // list — something is using the GPU.
        let v = assess_waste(&gpu(5, vec![]), Some(11.26), 60 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn idle_with_residual_memory_but_a_process_is_not_waste() {
        // A model loaded into VRAM, 0% util between requests: the process is
        // attached, so it is a job between steps, not waste.
        let mut g = gpu(0, vec![proc()]);
        g.mem_used_mb = 40_000;
        let v = assess_waste(&g, Some(11.26), 60 * 60, 20 * 60);
        assert!(!v.is_wasting);
    }

    #[test]
    fn reason_is_empty_when_not_wasting() {
        // The reason string feeds a human-facing Confirm; it must be empty
        // unless there is genuinely something to confirm stopping.
        let v = assess_waste(&gpu(50, vec![proc()]), Some(11.26), 60 * 60, 20 * 60);
        assert!(v.reason.is_empty());
        assert!((v.hourly_usd - 11.26).abs() < 1e-9);
    }

    #[test]
    fn unpriced_hourly_is_zero_in_the_verdict() {
        // When unpriced, the carried figure is 0 (nothing to claim), and it
        // still is not waste.
        let v = assess_waste(&gpu(0, vec![]), None, 60 * 60, 20 * 60);
        assert_eq!(v.hourly_usd, 0.0);
        assert!(!v.is_wasting);
    }
}
