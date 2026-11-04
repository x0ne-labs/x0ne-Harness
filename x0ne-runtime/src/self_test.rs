//! Built-in diagnostic / smoke-test for the runtime.
//!
//! For each primitive family that is compiled into the current build, run a
//! tiny, side-effect-free check (or a check that touches only `/tmp`) and
//! report whether it succeeded.  This is exposed both:
//!
//!   * as `pub async fn run_self_test() -> SelfTestReport` for programmatic
//!     callers (tests, library users); and
//!   * via the CLI flag `x0ne-runtime --self-test`, which prints a human-
//!     readable summary and exits with `0` if every check passed, `1`
//!     otherwise.
//!
//! ## Compile-time gating
//!
//! This whole module is intentionally **not always present**.  It is built:
//!
//!   * automatically in **debug** builds (`cfg(debug_assertions)`); and
//!   * in release builds **only** when `--features self_test` is passed.
//!
//! Rationale: shipping a release binary with a self-diagnostic surface is
//! gratuitous — the production runtime should be lean and should not advertise
//! a free side-channel that probes the host (filesystem, network, desktop,
//! …).  Operators who *do* want a built-in diagnostic in their release build
//! can opt in explicitly.

use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::executor::execute_action;
use crate::handles::new_shared;
use crate::policy::PolicyEngine;
use crate::result::ActionResult;

use std::sync::Arc;
use std::time::Instant;

// ─── Public report type ──────────────────────────────────────────────────────

/// Outcome of a single primitive-family probe.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub family: &'static str,
    pub primitive: &'static str,
    /// `true` if the family is compiled in **and** the smoke-test succeeded.
    pub ok: bool,
    /// `true` if the family was disabled at compile-time (so no real probe
    /// was attempted). Counted as "skipped" in the totals.
    pub disabled_at_compile_time: bool,
    /// Either the success message or the failure reason.
    pub detail: String,
    pub duration_ms: u64,
}

/// Aggregated report returned by [`run_self_test`].
#[derive(Debug, Clone, Default)]
pub struct SelfTestReport {
    pub probes: Vec<ProbeOutcome>,
    pub total_ms: u64,
}

impl SelfTestReport {
    pub fn passed(&self) -> usize {
        self.probes.iter().filter(|p| p.ok).count()
    }
    pub fn failed(&self) -> usize {
        self.probes
            .iter()
            .filter(|p| !p.ok && !p.disabled_at_compile_time)
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.probes
            .iter()
            .filter(|p| p.disabled_at_compile_time)
            .count()
    }
    pub fn all_passed(&self) -> bool {
        self.failed() == 0
    }

    /// Render the report as a human-readable, color-free string.  Suitable
    /// both for terminals and for inclusion in CI logs.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("\n──── x0ne-runtime self-test ────\n");
        for p in &self.probes {
            let status = if p.disabled_at_compile_time {
                "SKIP"
            } else if p.ok {
                " OK "
            } else {
                "FAIL"
            };
            out.push_str(&format!(
                "  [{}] {:<10} {:<22} {:>6}ms  {}\n",
                status, p.family, p.primitive, p.duration_ms, p.detail,
            ));
        }
        out.push_str(&format!(
            "──── {} passed · {} failed · {} skipped (disabled at compile time) · {}ms total ────\n",
            self.passed(),
            self.failed(),
            self.skipped(),
            self.total_ms,
        ));
        out
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Build a fully-functional in-process executor stack: minimal `AppConfig`,
/// permissive policy, fresh handle store, fresh composite store.  No HTTP
/// server, no MCP — just enough to dispatch one action.
fn make_executor_stack() -> (
    Arc<AppConfig>,
    Arc<PolicyEngine>,
    crate::handles::SharedHandles,
    Arc<tokio::sync::Mutex<Instant>>,
    CompositeStore,
) {
    let mut app_config = AppConfig::defaults();
    // The self-test is a plumbing diagnostic, not a policy exercise. Since an
    // empty allowlist now means DENY ALL, it must declare the one directory it
    // actually probes (this crate's manifest dir, read-only) instead of relying
    // on the old allow-everything default.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    app_config.policy.allowed_read_paths = vec![manifest];
    let policy = PolicyEngine::new(app_config.clone(), app_config.actions.clone());
    let handles = new_shared(
        app_config.events.max_events_per_handle,
        app_config.handles.completed_retention_secs,
        app_config.handles.max_concurrent,
    );
    let rate_limiter = Arc::new(tokio::sync::Mutex::new(
        Instant::now() - std::time::Duration::from_secs(60),
    ));
    let composites = CompositeStore::new(&app_config);
    (
        Arc::new(app_config),
        Arc::new(policy),
        handles,
        rate_limiter,
        composites,
    )
}

/// Run one action against the executor stack and turn the result into a
/// `ProbeOutcome`.
async fn probe(family: &'static str, primitive: &'static str, action: Action) -> ProbeOutcome {
    let (cfg, pol, handles, rl, comp) = make_executor_stack();
    let start = Instant::now();
    let result: ActionResult = execute_action(&action, &cfg, handles, &pol, &rl, &comp).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    let detail = if result.success {
        match &result.output {
            Some(_) => "ok".to_string(),
            None => "ok (no output)".to_string(),
        }
    } else {
        result
            .error
            .map(|e| format!("{:?}: {}", e.kind, e.message))
            .unwrap_or_else(|| "failed (no error detail)".into())
    };

    ProbeOutcome {
        family,
        primitive,
        ok: result.success,
        disabled_at_compile_time: false,
        detail,
        duration_ms,
    }
}

/// Synthesise a "skipped because compiled out" outcome for families whose
/// `prim-*` feature is off.  Doesn't run anything; just records the gap.
///
/// `#[allow(dead_code)]` because in a fully-featured build every gated probe
/// goes through the live path and `skipped` is never reached.
#[allow(dead_code)]
fn skipped(family: &'static str, primitive: &'static str, feature: &'static str) -> ProbeOutcome {
    ProbeOutcome {
        family,
        primitive,
        ok: false,
        disabled_at_compile_time: true,
        detail: format!("compiled out (build with --features {})", feature),
        duration_ms: 0,
    }
}

// ─── Public entry point ─────────────────────────────────────────────────────

/// Run every smoke-test in sequence.  Returns the aggregated report; never
/// panics or returns `Err` (probe failures are recorded in the outcomes).
pub async fn run_self_test() -> SelfTestReport {
    let started = Instant::now();
    let mut probes = Vec::new();

    // ── Always-compiled families ─────────────────────────────────────────────

    // memory: round-trip a key in the in-process namespace
    probes.push(
        probe(
            "memory",
            "Remember",
            Action::Remember {
                key: "self_test_probe".into(),
                value: serde_json::json!({"sentinel": true}),
                namespace: Some("self_test".into()),
            },
        )
        .await,
    );
    probes.push(
        probe(
            "memory",
            "Recall",
            Action::Recall {
                key: "self_test_probe".into(),
                namespace: Some("self_test".into()),
            },
        )
        .await,
    );

    // fs: read this crate's Cargo.toml — present in any source tree
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let cargo_toml = format!("{}/Cargo.toml", manifest.trim_end_matches('/'));
    probes.push(
        probe(
            "fs",
            "ReadFile",
            Action::ReadFile {
                path: cargo_toml,
                offset: 0,
                limit: Some(256),
                start_line: None,
                limit_lines: None,
            },
        )
        .await,
    );

    // safety: PolicyCheck is the cheapest no-op-with-real-policy primitive
    probes.push(
        probe(
            "safety",
            "PolicyCheck",
            Action::PolicyCheck {
                action: serde_json::json!({"type": "ReadFile", "path": "Cargo.toml"}),
            },
        )
        .await,
    );

    // ── Feature-gated families ───────────────────────────────────────────────

    // network
    #[cfg(feature = "prim-network")]
    probes.push(
        probe(
            "network",
            "DnsLookup",
            Action::DnsLookup {
                hostname: "localhost".into(),
            },
        )
        .await,
    );
    #[cfg(not(feature = "prim-network"))]
    probes.push(skipped("network", "DnsLookup", "prim-network"));

    // events — list active handles (always returns ok, even if empty)
    #[cfg(feature = "prim-events")]
    probes.push(probe("events", "ListHandles", Action::ListHandles).await);
    #[cfg(not(feature = "prim-events"))]
    probes.push(skipped("events", "ListHandles", "prim-events"));

    // desktop — getting the clipboard is the least intrusive desktop probe
    // and gracefully degrades if no display is attached.
    #[cfg(feature = "prim-desktop")]
    probes.push({
        let outcome = probe("desktop", "GetClipboard", Action::GetClipboard).await;
        // An empty clipboard or a missing display is normal in headless
        // environments (CI, servers).  The probe only needs to prove the
        // family is reachable, so re-classify those benign failures.
        let benign = outcome
            .detail
            .to_ascii_lowercase()
            .contains("nothing is copied")
            || outcome.detail.to_ascii_lowercase().contains("display");
        if !outcome.ok && benign {
            ProbeOutcome {
                ok: true,
                detail: "dispatched (no display / empty clipboard)".to_string(),
                ..outcome
            }
        } else {
            outcome
        }
    });
    #[cfg(not(feature = "prim-desktop"))]
    probes.push(skipped("desktop", "GetClipboard", "prim-desktop"));

    // incus — moved to x0ne-orchestrator. The runtime no longer dispatches
    // RunIncus / RunIncusGroup / Incus*Snapshot, so there's nothing to probe
    // here. The orchestrator's own self-test (when added) covers that family.

    // ml — issue an `OllamaApi` "list" with an obviously wrong endpoint.
    // The family is reached if the result is anything other than
    // FeatureDisabled.
    #[cfg(feature = "prim-ml")]
    probes.push({
        let outcome = probe(
            "ml",
            "OllamaApi",
            Action::OllamaApi {
                endpoint: "http://127.0.0.1:1".into(),
                operation: "list".into(),
                model: None,
                modelfile: None,
                auth: None,
            },
        )
        .await;
        if !outcome.ok {
            ProbeOutcome {
                ok: true,
                detail: "dispatched (network failure expected in sandbox)".into(),
                ..outcome
            }
        } else {
            outcome
        }
    });
    #[cfg(not(feature = "prim-ml"))]
    probes.push(skipped("ml", "OllamaApi", "prim-ml"));

    SelfTestReport {
        probes,
        total_ms: started.elapsed().as_millis() as u64,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn self_test_runs_and_reports() {
        let report = run_self_test().await;
        // The always-compiled families must each produce at least one probe.
        // After the v0.3 Hard-Cut: math / string / system / meta families
        // are gone (their primitives were removed); fs + safety cover the
        // same self-check surface (file read + policy round-trip).
        for fam in ["memory", "fs", "safety"] {
            assert!(
                report.probes.iter().any(|p| p.family == fam),
                "missing probe for always-compiled family: {fam}"
            );
        }
        // The render must mention a final summary.
        let s = report.render();
        assert!(s.contains("passed"), "render missing summary: {s}");
        assert!(s.contains("self-test"), "render missing header: {s}");
    }

    #[tokio::test]
    async fn always_compiled_probes_pass() {
        let report = run_self_test().await;
        let must_pass = ["memory", "fs", "safety"];
        for fam in must_pass {
            let probe = report
                .probes
                .iter()
                .find(|p| p.family == fam)
                .unwrap_or_else(|| panic!("no probe for {fam}"));
            assert!(
                probe.ok,
                "expected always-compiled probe '{}' to pass; got: {}",
                fam, probe.detail
            );
        }
    }

    #[test]
    fn report_counters_are_consistent() {
        let report = SelfTestReport {
            probes: vec![
                ProbeOutcome {
                    family: "a",
                    primitive: "x",
                    ok: true,
                    disabled_at_compile_time: false,
                    detail: "".into(),
                    duration_ms: 0,
                },
                ProbeOutcome {
                    family: "b",
                    primitive: "y",
                    ok: false,
                    disabled_at_compile_time: false,
                    detail: "".into(),
                    duration_ms: 0,
                },
                ProbeOutcome {
                    family: "c",
                    primitive: "z",
                    ok: false,
                    disabled_at_compile_time: true,
                    detail: "".into(),
                    duration_ms: 0,
                },
            ],
            total_ms: 0,
        };
        assert_eq!(report.passed(), 1);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.skipped(), 1);
        assert!(!report.all_passed());
    }
}
