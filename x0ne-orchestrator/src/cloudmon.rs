//! Background cloud/GPU monitor.
//!
//! Mirrors `sysmon.rs`: a shared state other surfaces read, populated by a
//! background task that shells out to `nvidia-smi` and `gcloud` DIRECTLY — the
//! same trust basis as sysmon reading `/proc`, or the `!` shell escape being
//! the human's own hand. This is the OPERATOR's continuous dashboard of their
//! own host, not the agent acting: the agent's `CloudCost`/`GpuStatus` actions
//! stay policy-gated by `allowed_commands`; enabling the monitor
//! (`[cloud] enabled = true`) is the operator's own opt-in to run these two
//! read-only commands for the dashboard.
//!
//! Disabled by default — polling on a machine with no cloud footprint is
//! noise, and it degrades gracefully: a missing `gcloud`/`nvidia-smi` leaves
//! the corresponding field empty rather than faking a value.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::process::Command;
use x0ne_cloud::burn::BurnRate;
use x0ne_cloud::gpu::GpuState;

/// A GPU idle-and-processless for longer than this, on a priced resource, is
/// waste — the sustained-idle threshold behind the anti-waste alert.
const WASTE_THRESHOLD_SECS: u64 = 20 * 60;

/// GPU poll cadence. gcloud inventory polls at a multiple of this (below).
const GPU_POLL_SECS: u64 = 15;
/// Poll gcloud every Nth GPU tick (≈ every 60s): the inventory changes slowly
/// and a `list` call, while free, still hits the API.
const GCLOUD_EVERY_N_TICKS: u64 = 4;

#[derive(Clone, Default)]
pub struct CloudState {
    pub burn: Option<BurnRate>,
    pub gpus: Vec<GpuState>,
    /// Real billed cost for yesterday. `None` unless a billing export exists —
    /// gcloud has no "what did I spend" call, so absence is the normal case
    /// and the UI must say "estimated only" rather than invent a figure.
    pub billed_yesterday: Option<f64>,
    pub alerts: Vec<String>,
    pub pricing_as_of: String,
}

impl CloudState {
    /// One compact navbar line, e.g. `gcp 4.21$/h · gpu 87% 71°C`. Empty when
    /// nothing is known, so the navbar shows nothing rather than a zero that
    /// would read as "costs nothing".
    pub fn navbar_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(b) = &self.burn {
            parts.push(format!("gcp {:.2}$/h", b.hourly_usd));
        }
        if let Some(g) = self.gpus.first() {
            parts.push(format!("gpu {}% {}°C", g.util_pct, g.temp_c));
        }
        if let Some(a) = self.alerts.first() {
            parts.push(a.clone());
        }
        parts.join(" · ")
    }

    /// True when any alert is present — drives the red styling.
    pub fn has_alert(&self) -> bool {
        !self.alerts.is_empty()
    }

    /// Recompute `alerts` from the current GPU state, given how long each GPU
    /// index has been idle-and-processless (the poll loop tracks that timing;
    /// this method owns only the verdict, not the clock).
    ///
    /// A GPU's dollar figure is the first priced resource's rate — on a
    /// single-GPU footprint that resource IS the VM the GPU belongs to. Waste requires a
    /// priced resource, so on an unpriced footprint nothing ever trips (you
    /// can't claim dollars wasted on something you couldn't price). Stopping
    /// the VM is never done here: the reason string is what a human turns into
    /// a `Confirm` carrying the exact `gcloud … stop` command.
    pub fn refresh_alerts<F>(&mut self, threshold_secs: u64, idle_secs: F)
    where
        F: Fn(u32) -> u64,
    {
        let hourly = self
            .burn
            .as_ref()
            .and_then(|b| b.priced.first().map(|p| p.hourly_usd));
        self.alerts.clear();
        for g in &self.gpus {
            let v = x0ne_cloud::waste::assess_waste(g, hourly, idle_secs(g.index), threshold_secs);
            if v.is_wasting {
                self.alerts.push(v.reason);
            }
        }
    }
}

/// Start the monitor. Returns the shared handle the navbar/TUI read. When
/// `enabled` is false the handle stays at default (empty) and NO task is
/// spawned: no gcloud/nvidia-smi for someone who has no cloud footprint.
///
/// # Runtime requirement
///
/// When `enabled` is true this spawns a tokio task, so it MUST be called from
/// inside a tokio runtime — `tokio::spawn` panics otherwise. The disabled path
/// spawns nothing and is safe anywhere (pinned by
/// `disabled_monitor_stays_empty`, a plain `#[test]` with no runtime).
pub fn start_monitor(enabled: bool) -> Arc<RwLock<CloudState>> {
    let state = Arc::new(RwLock::new(CloudState::default()));
    if !enabled {
        return state;
    }
    if let Ok(mut s) = state.write() {
        s.pricing_as_of = x0ne_cloud::pricing::PriceTable::current().as_of.to_string();
    }
    let bg = state.clone();
    tokio::spawn(async move {
        // Per-GPU: when did it first go idle-and-processless? Cleared when it
        // resumes. idle duration = now - first_idle, fed to the waste verdict.
        let mut first_idle: HashMap<u32, Instant> = HashMap::new();
        let mut tick: u64 = 0;
        loop {
            let gpus = collect_gpus().await;
            let burn = if should_poll_cloud(tick, GCLOUD_EVERY_N_TICKS) {
                collect_burn().await
            } else {
                // Hold the last known figure between the slower gcloud polls,
                // so the gauge stays steady instead of blinking to "unknown"
                // three ticks out of four.
                bg.read().ok().and_then(|s| s.burn.clone())
            };
            publish_tick(
                &bg,
                &mut first_idle,
                gpus,
                burn,
                Instant::now(),
                WASTE_THRESHOLD_SECS,
            );
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(GPU_POLL_SECS)).await;
        }
    });
    state
}

/// Does this tick refresh the cloud inventory? gcloud runs at a multiple of
/// the GPU cadence: the inventory changes slowly and the call, while free,
/// still hits the API. `every == 0` means every tick — defensive, since a bare
/// modulo by zero would panic the whole monitor task.
fn should_poll_cloud(tick: u64, every: u64) -> bool {
    every == 0 || tick.is_multiple_of(every)
}

/// Fold one tick's observations into the shared state: advance the per-GPU
/// idle timers, publish the readings, recompute the waste alerts.
///
/// Factored out of the spawned loop so the idle accumulation and the alert
/// transition are testable without spawning a subprocess or waiting on
/// wall-clock time — the caller supplies `now`, so a test can walk the clock
/// forward in one step.
fn publish_tick(
    state: &Arc<RwLock<CloudState>>,
    first_idle: &mut HashMap<u32, Instant>,
    gpus: Vec<GpuState>,
    burn: Option<BurnRate>,
    now: Instant,
    threshold_secs: u64,
) {
    update_idle_timers(first_idle, &gpus, now);
    let idle_secs = |idx: u32| -> u64 {
        first_idle
            .get(&idx)
            .map(|t| now.duration_since(*t).as_secs())
            .unwrap_or(0)
    };
    if let Ok(mut s) = state.write() {
        s.gpus = gpus;
        s.burn = burn;
        s.refresh_alerts(threshold_secs, idle_secs);
    }
}

/// Update per-GPU idle timers in place: a GPU that is idle-and-processless
/// gets a start instant on FIRST sight and keeps it across ticks (so the
/// duration grows); a GPU that resumes clears its timer. The `or_insert`
/// (not `insert`) is load-bearing — re-stamping every tick would reset the
/// clock and the sustained-idle threshold would never be reached.
fn update_idle_timers(first_idle: &mut HashMap<u32, Instant>, gpus: &[GpuState], now: Instant) {
    for g in gpus {
        let idle = g.util_pct == 0 && g.processes.is_empty();
        if idle {
            first_idle.entry(g.index).or_insert(now);
        } else {
            first_idle.remove(&g.index);
        }
    }
}

/// Run `nvidia-smi` (state + compute-apps queries) directly and parse. Empty
/// on any failure — a missing GPU or a restricted-telemetry enclave leaves the
/// list empty rather than lying.
async fn collect_gpus() -> Vec<GpuState> {
    let gpu_csv = run_capture(
        "nvidia-smi",
        &[
            "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ],
    )
    .await;
    if gpu_csv.trim().is_empty() {
        return Vec::new();
    }
    let apps_csv = run_capture(
        "nvidia-smi",
        &[
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ],
    )
    .await;
    x0ne_cloud::gpu::parse_nvidia_smi(&gpu_csv, &apps_csv)
}

/// Run `gcloud compute instances list` directly, price it. `None` on any
/// failure (no gcloud, not authed, no project) — the UI then shows no cost
/// rather than a fabricated zero.
async fn collect_burn() -> Option<BurnRate> {
    burn_from_json(&run_capture("gcloud", &["compute", "instances", "list", "--format=json"]).await)
}

/// What a gcloud answer MEANS, separated from the act of getting one.
///
/// Split out on 2026-08-14, and not for abstraction's sake: the test of this
/// rule used to call the real `gcloud` and assert that it failed, which held
/// only on a machine nobody had logged in. The H100 milestone logged one in,
/// `collect_burn_is_none_without_an_authenticated_gcloud` started failing on
/// the owner's own box, and because a failing target makes `cargo test` abandon
/// the targets after it, the workspace count fell from 2086 to 1589 — a test
/// that reads the machine it runs on does not merely report wrong, it hides
/// the rest of the suite.
///
/// The seam is at the OUTPUT rather than at the binary name on purpose. A test
/// that writes its own fake `gcloud` and executes it hits ETXTBSY perhaps one
/// run in three (measured here the same day: another thread's `fork` inherits
/// the write fd for the instant before its `exec`, and the kernel then refuses
/// to run the file) — trading a machine-dependent test for a thread-scheduling
/// dependent one. Nothing is lost: the `run_capture` tests below cover its
/// three failure arms — a spawn that fails, a non-zero exit, a timeout — and
/// pin that all three arrive here as `""`. The non-zero exit is the one an
/// unauthenticated gcloud produces, and it is the arm that was untested until
/// review round 3 asked what the claim rested on.
///
/// `None` on every failure — no gcloud, not authed, no project, no instance.
/// The UI then shows no cost at all rather than a fabricated zero: `0.00 $/h`
/// printed over a running H100 is the worst output this gauge can produce.
fn burn_from_json(json: &str) -> Option<BurnRate> {
    if json.trim().is_empty() {
        return None;
    }
    let resources = x0ne_cloud::inventory::parse_gcloud_instances(json);
    if resources.is_empty() {
        return None;
    }
    Some(x0ne_cloud::burn::compute_burn(
        &resources,
        &x0ne_cloud::pricing::PriceTable::current(),
    ))
}

/// Hard ceiling on a probe. `nvidia-smi` is known to hang when the driver is
/// wedged or the GPU falls off the PCIe bus (Xid errors), and `gcloud` can
/// stall on a network hiccup. Without a bound, one hung probe freezes the poll
/// loop forever and the navbar keeps showing a cost that is no longer true —
/// exactly the lie this gauge exists to prevent.
const PROBE_TIMEOUT_SECS: u64 = 10;

/// Spawn a command and return its stdout, or "" on any error / non-zero exit /
/// timeout. Host-side operator dashboard — no policy gate, same trust basis as
/// sysmon reading `/proc`.
///
/// stdin is NULL on purpose: an unauthenticated `gcloud` will try to prompt,
/// and an inherited stdin would let it steal the operator's keystrokes from
/// readline/the TUI, or block forever waiting on input nobody is typing.
async fn run_capture(bin: &str, args: &[&str]) -> String {
    let fut = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), fut).await {
        // `kill_on_drop` reaps the child when the timeout drops the future.
        Err(_) => String::new(),
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x0ne_cloud::burn::BurnRate;
    use x0ne_cloud::gpu::GpuState;

    fn gpu(util: u32, temp: u32) -> GpuState {
        GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: util,
            mem_used_mb: 40000,
            mem_total_mb: 81559,
            temp_c: temp,
            power_w: 402,
            processes: vec![],
        }
    }

    #[test]
    fn navbar_line_shows_burn_and_gpu() {
        let s = CloudState {
            burn: Some(BurnRate {
                hourly_usd: 4.21,
                per_day_usd: 101.0,
                priced: vec![],
                unpriced: vec![],
            }),
            gpus: vec![gpu(87, 71)],
            billed_yesterday: None,
            alerts: vec![],
            pricing_as_of: "2026-08-10".into(),
        };
        let line = s.navbar_line();
        assert!(line.contains("4.21$/h"), "{line}");
        assert!(line.contains("87%"), "{line}");
        assert!(line.contains("71"), "{line}");
    }

    #[test]
    fn navbar_line_empty_when_nothing_known() {
        let s = CloudState::default();
        assert_eq!(s.navbar_line(), "");
        assert!(!s.has_alert());
    }

    #[test]
    fn alert_surfaces_in_the_line() {
        let s = CloudState {
            alerts: vec!["gpu idle 34min".into()],
            ..Default::default()
        };
        assert!(s.navbar_line().contains("gpu idle 34min"));
        assert!(s.has_alert());
    }

    #[test]
    fn disabled_monitor_stays_empty() {
        let h = start_monitor(false);
        let s = h.read().unwrap();
        assert!(s.navbar_line().is_empty());
        assert!(s.pricing_as_of.is_empty());
    }

    #[test]
    fn refresh_alerts_trips_only_on_priced_idle_processless_gpu() {
        use x0ne_cloud::burn::{BurnRate, PricedResource};
        use x0ne_cloud::inventory::{CloudResource, ResourceKind};

        let priced = PricedResource {
            resource: CloudResource {
                name: "gpu-h100".into(),
                kind: ResourceKind::Instance,
                machine_type: Some("a3-highgpu-1g".into()),
                zone: "us-central1-a".into(),
                status: "RUNNING".into(),
                accelerators: vec![],
                created_iso: "2026-08-10T09:00:00Z".into(),
            },
            hourly_usd: 11.26,
        };
        let mut s = CloudState {
            burn: Some(BurnRate {
                hourly_usd: 11.26,
                per_day_usd: 270.0,
                priced: vec![priced],
                unpriced: vec![],
            }),
            gpus: vec![gpu(0, 33)], // idle, no processes
            ..Default::default()
        };

        // Idle 25min, threshold 20min → waste, reason carries the dollars.
        s.refresh_alerts(20 * 60, |_| 25 * 60);
        assert!(s.has_alert());
        assert!(s.alerts[0].contains("11.26"));
        assert!(s.navbar_line().contains(&s.alerts[0]));

        // Below threshold → cleared.
        s.refresh_alerts(20 * 60, |_| 5 * 60);
        assert!(!s.has_alert());
    }

    #[tokio::test]
    async fn run_capture_returns_empty_on_a_hung_probe() {
        // A wedged nvidia-smi must not freeze the poll loop forever. `sleep 60`
        // stands in for the hang; the timeout must cut it and yield "" so the
        // caller reports "unknown" instead of pinning a stale cost on screen.
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(PROBE_TIMEOUT_SECS + 5),
            run_capture("sleep", &["60"]),
        )
        .await
        .expect("run_capture must return on its own, not need an outer timeout");
        assert_eq!(out, "");
        assert!(
            started.elapsed() < Duration::from_secs(PROBE_TIMEOUT_SECS + 3),
            "took {:?} — the internal timeout did not fire",
            started.elapsed()
        );
    }

    /// A burn carrying one priced resource at `hourly` — what the waste
    /// verdict needs to be able to quote dollars.
    fn priced_burn(hourly: f64) -> BurnRate {
        use x0ne_cloud::burn::PricedResource;
        use x0ne_cloud::inventory::{CloudResource, ResourceKind};
        BurnRate {
            hourly_usd: hourly,
            per_day_usd: hourly * 24.0,
            priced: vec![PricedResource {
                resource: CloudResource {
                    name: "gpu-h100".into(),
                    kind: ResourceKind::Instance,
                    machine_type: Some("a3-highgpu-1g".into()),
                    zone: "us-central1-a".into(),
                    status: "RUNNING".into(),
                    accelerators: vec![],
                    created_iso: "t".into(),
                },
                hourly_usd: hourly,
            }],
            unpriced: vec![],
        }
    }

    // ── The poll loop's own logic ────────────────────────────────────
    //
    // The loop body used to be untestable inside the spawn. These exercise
    // the two decisions it makes — cadence and publication — plus the
    // multi-tick accumulation that is the whole point of the feature.

    #[test]
    fn cloud_inventory_polls_on_every_nth_tick() {
        assert!(should_poll_cloud(0, 4), "first tick must prime the cost");
        assert!(!should_poll_cloud(1, 4));
        assert!(!should_poll_cloud(3, 4));
        assert!(should_poll_cloud(4, 4));
        assert!(should_poll_cloud(8, 4));
        // A zero cadence must not panic the monitor task on the modulo.
        assert!(should_poll_cloud(7, 0));
    }

    #[test]
    fn a_gpu_left_idle_trips_the_alert_exactly_at_the_threshold() {
        // End-to-end of the anti-waste path across ticks: idle accrues, the
        // alert appears at the threshold and not before, and resuming work
        // clears it. This is the behaviour the $11/h guardrail rests on.
        let state = Arc::new(RwLock::new(CloudState::default()));
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        let threshold = 20 * 60u64;
        let burn = Some(priced_burn(11.26));

        // Tick 0 — the GPU goes idle. Nothing wasted yet.
        publish_tick(
            &state,
            &mut timers,
            vec![gpu(0, 33)],
            burn.clone(),
            t0,
            threshold,
        );
        assert!(!state.read().unwrap().has_alert(), "alerted immediately");

        // One second short of the threshold — still quiet.
        publish_tick(
            &state,
            &mut timers,
            vec![gpu(0, 33)],
            burn.clone(),
            t0 + Duration::from_secs(threshold - 1),
            threshold,
        );
        assert!(
            !state.read().unwrap().has_alert(),
            "alerted one second early"
        );

        // At the threshold — the alert appears, quoting the rate.
        publish_tick(
            &state,
            &mut timers,
            vec![gpu(0, 33)],
            burn.clone(),
            t0 + Duration::from_secs(threshold),
            threshold,
        );
        let s = state.read().unwrap().clone();
        assert!(s.has_alert(), "no alert at the threshold");
        assert!(
            s.alerts[0].contains("11.26"),
            "alert must quote the rate: {:?}",
            s.alerts
        );

        // The GPU resumes — the alert clears and the clock restarts, so a
        // job that ran for one tick does not leave a stale warning up.
        publish_tick(
            &state,
            &mut timers,
            vec![gpu(90, 70)],
            burn.clone(),
            t0 + Duration::from_secs(threshold + 60),
            threshold,
        );
        assert!(
            !state.read().unwrap().has_alert(),
            "alert survived the GPU resuming"
        );
        assert!(!timers.contains_key(&0), "idle clock not restarted");
    }

    #[test]
    fn burn_holds_steady_between_cloud_polls() {
        // Mirrors the loop's cadence decision over eight ticks with a stub
        // that only "answers" on cloud ticks. The published cost must stay
        // populated throughout — blinking to unknown three ticks out of four
        // would make the navbar flicker and read as "cost went away".
        let state = Arc::new(RwLock::new(CloudState::default()));
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        for tick in 0..8u64 {
            let burn = if should_poll_cloud(tick, 4) {
                Some(priced_burn(11.26))
            } else {
                state.read().unwrap().burn.clone()
            };
            publish_tick(&state, &mut timers, vec![], burn, t0, 20 * 60);
            assert!(
                state.read().unwrap().burn.is_some(),
                "cost blinked to unknown on tick {tick}"
            );
        }
    }

    #[test]
    fn publish_tick_replaces_readings_rather_than_accumulating() {
        // Each tick publishes the CURRENT world, so a GPU that disappears
        // (driver reset, restricted telemetry) must vanish from the state
        // instead of lingering as a stale reading.
        let state = Arc::new(RwLock::new(CloudState::default()));
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        publish_tick(&state, &mut timers, vec![gpu(50, 60)], None, t0, 20 * 60);
        assert_eq!(state.read().unwrap().gpus.len(), 1);
        publish_tick(&state, &mut timers, vec![], None, t0, 20 * 60);
        assert!(
            state.read().unwrap().gpus.is_empty(),
            "stale GPU reading kept"
        );
        assert_eq!(state.read().unwrap().navbar_line(), "");
    }

    // ── Collectors (thin I/O wrappers over tested parsers) ───────────

    /// An unauthenticated (or absent) gcloud means "no cost known", never a
    /// fabricated zero — a navbar reading `0.00 $/h` over a running H100 is
    /// the worst output this gauge can produce.
    ///
    /// Hermetic since 2026-08-14, and that is the whole point of the rewrite:
    /// this test used to call the real `gcloud` and assert that it failed,
    /// which held only on a machine nobody had ever logged in on. The day the
    /// H100 milestone authenticated one, it started failing on the owner's own
    /// box and took the targets after it down with it (2086 tests counted as
    /// 1589). It now states the three answers and what each means, and asks
    /// nothing of the machine it runs on.
    ///
    /// That an unauthenticated gcloud arrives here as `""` is not assumed: it
    /// is what `run_capture` returns on a non-zero exit, on a missing binary
    /// and on a timeout, pinned by the `run_capture` tests below — including
    /// the non-zero exit, which is the shape this test is named after.
    #[test]
    fn collect_burn_is_none_without_an_authenticated_gcloud() {
        // Not logged in / no binary / hung probe — `run_capture` folds all
        // three into an empty capture.
        assert!(burn_from_json("").is_none());
        assert!(burn_from_json("   \n").is_none());
        // Authenticated, and the project simply has no instance. An empty
        // inventory is "nothing is burning", which is still not a price.
        assert!(burn_from_json("[]").is_none());
        // Not JSON at all — a gcloud that printed a diagnostic on stdout, or a
        // truncated capture. Unreadable is unknown, never zero.
        assert!(
            burn_from_json("ERROR: (gcloud.compute.instances.list) not authenticated").is_none()
        );
    }

    /// The positive control the test above needs to be worth anything: given an
    /// answer, a running instance must produce a burn rate. Without this, a
    /// `burn_from_json` hard-wired to `None` would satisfy every assertion up
    /// there — the gauge would be gone and the suite green.
    #[test]
    fn collect_burn_prices_an_instance_a_working_gcloud_reports() {
        // The shape `gcloud compute instances list --format=json` really
        // returns: zone and machineType as full self-links, which the parser
        // reduces to their leaf.
        let json = r#"[{"name":"x0ne-cc","status":"RUNNING","creationTimestamp":"2026-08-14T09:00:00Z","zone":"https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a","machineType":"https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a/machineTypes/c3-standard-4"}]"#;
        let burn = burn_from_json(json).expect("a running instance must be priced");
        assert!(
            burn.hourly_usd > 0.0,
            "a running VM cannot burn nothing: {}",
            burn.hourly_usd
        );
        assert!(
            burn.unpriced.is_empty(),
            "c3-standard-4 is in the price table"
        );
    }

    #[tokio::test]
    async fn collect_gpus_is_well_formed_or_empty() {
        // Tolerant by design: CI has no GPU, this dev box has one. Either is
        // fine — what must hold is that we never panic and never emit a
        // half-parsed entry.
        for g in collect_gpus().await {
            assert!(!g.name.is_empty(), "a parsed GPU must carry its name");
            assert!(
                g.util_pct <= 100,
                "utilization out of range: {}",
                g.util_pct
            );
            assert!(g.mem_total_mb >= g.mem_used_mb, "used memory exceeds total");
        }
    }

    #[test]
    fn idle_timer_records_on_first_idle_and_keeps_the_original_instant() {
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();

        // First idle tick: record t0.
        update_idle_timers(&mut timers, &[gpu(0, 33)], t0);
        assert_eq!(timers.get(&0), Some(&t0));

        // Still idle a tick later: the ORIGINAL instant must be kept, not
        // re-stamped — otherwise the sustained-idle clock never advances.
        let t1 = t0 + Duration::from_secs(15);
        update_idle_timers(&mut timers, &[gpu(0, 33)], t1);
        assert_eq!(
            timers.get(&0),
            Some(&t0),
            "timer was reset — clock would never advance"
        );
    }

    #[test]
    fn idle_timer_clears_when_the_gpu_resumes() {
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        update_idle_timers(&mut timers, &[gpu(0, 33)], t0);
        assert!(timers.contains_key(&0));

        // GPU goes busy → timer cleared, so idleness must accrue from scratch
        // next time (a job that ran for a second resets the waste clock).
        update_idle_timers(&mut timers, &[gpu(80, 60)], t0 + Duration::from_secs(15));
        assert!(!timers.contains_key(&0));
    }

    #[test]
    fn idle_timer_clears_when_a_process_attaches() {
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        update_idle_timers(&mut timers, &[gpu(0, 33)], t0);
        assert!(timers.contains_key(&0));

        // 0% util but a process attached (model loaded, between requests):
        // not idle, timer cleared.
        let mut g = gpu(0, 33);
        g.processes = vec![x0ne_cloud::gpu::GpuProc {
            pid: 1,
            name: "vllm".into(),
            mem_mb: 40000,
        }];
        update_idle_timers(&mut timers, &[g], t0 + Duration::from_secs(15));
        assert!(!timers.contains_key(&0));
    }

    #[test]
    fn idle_timers_track_multiple_gpus_independently() {
        let mut timers: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        // gpu0 idle, gpu1 busy.
        let mut g1 = gpu(90, 70);
        g1.index = 1;
        update_idle_timers(&mut timers, &[gpu(0, 33), g1], t0);
        assert!(timers.contains_key(&0));
        assert!(!timers.contains_key(&1));
    }

    #[tokio::test]
    async fn run_capture_returns_empty_for_a_missing_binary() {
        // No gcloud / no nvidia-smi is the common case on a laptop: empty, not
        // a panic, and definitely not a fabricated zero.
        assert_eq!(run_capture("x0ne-no-such-binary-xyz", &[]).await, "");
    }

    #[tokio::test]
    async fn run_capture_reads_stdout_of_a_real_command() {
        let out = run_capture("echo", &["hello-probe"]).await;
        assert!(out.contains("hello-probe"), "got {out:?}");
    }

    /// A binary that EXISTS and exits non-zero — the shape an unauthenticated
    /// `gcloud` actually has, and the case that was missing when
    /// `burn_from_json`'s doc-comment started leaning on these tests (review
    /// round 3, M3). With it, the four of them cover `run_capture`'s three
    /// failure arms exactly: a spawn that fails, a non-zero exit, a timeout.
    #[tokio::test]
    async fn run_capture_returns_empty_for_a_command_that_fails() {
        assert_eq!(run_capture("false", &[]).await, "");
    }

    #[test]
    fn refresh_alerts_never_trips_on_unpriced_footprint() {
        // No priced resource → no dollar figure → never waste.
        let mut s = CloudState {
            gpus: vec![gpu(0, 33)],
            ..Default::default()
        };
        s.refresh_alerts(20 * 60, |_| 60 * 60);
        assert!(!s.has_alert());
    }
}
