//! X0NE's pure cloud-cost + GPU model. Zero I/O: every function takes the text
//! that `gcloud`/`nvidia-smi` already produced and returns typed state, so the
//! whole crate is unit-tested on fixtures without a cloud account or a GPU.
//!
//! The same crate later runs inside the confidential VM's runtime to measure
//! that node's own GPU — which is why nothing here may touch the filesystem,
//! the network, or a subprocess.
pub mod burn;
pub mod cc_plan;
pub mod gpu;
pub mod inventory;
pub mod pricing;
pub mod provision;
pub mod spend;
pub mod waste;

#[cfg(test)]
mod serde_contract {
    //! These types serialize into the JSON an LLM reads from a `CloudCost` /
    //! `GpuStatus` result. A field rename or a shape change silently breaks
    //! the model's ability to reason about cost; these round-trips make such
    //! a change fail the build instead.

    use crate::burn::{compute_burn, BurnRate};
    use crate::gpu::{GpuProc, GpuState};
    use crate::inventory::{parse_gcloud_instances, CloudResource};
    use crate::pricing::PriceTable;
    use crate::waste::assess_waste;

    #[test]
    fn gpu_state_round_trips_with_expected_field_names() {
        let g = GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: 87,
            mem_used_mb: 40000,
            mem_total_mb: 81559,
            temp_c: 71,
            power_w: 402,
            processes: vec![GpuProc {
                pid: 1,
                name: "python".into(),
                mem_mb: 1000,
            }],
        };
        let v = serde_json::to_value(&g).unwrap();
        // The exact keys the prompt/UI depend on.
        for key in [
            "index",
            "name",
            "util_pct",
            "mem_used_mb",
            "mem_total_mb",
            "temp_c",
            "power_w",
            "processes",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(v["processes"][0]["pid"], 1);
        let back: GpuState = serde_json::from_value(v).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn burn_rate_round_trips() {
        let resources: Vec<CloudResource> = parse_gcloud_instances(
            r#"[{"name":"x","machineType":"https://x/machineTypes/c3-standard-4","zone":"https://x/zones/us-central1-a","status":"RUNNING","creationTimestamp":"t"}]"#,
        );
        let b = compute_burn(&resources, &PriceTable::current());
        let v = serde_json::to_value(&b).unwrap();
        for key in ["hourly_usd", "per_day_usd", "priced", "unpriced"] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        let back: BurnRate = serde_json::from_value(v).unwrap();
        assert_eq!(back.priced.len(), b.priced.len());
        assert_eq!(back.hourly_usd, b.hourly_usd);
    }

    #[test]
    fn waste_verdict_round_trips() {
        let g = GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: 0,
            mem_used_mb: 0,
            mem_total_mb: 81559,
            temp_c: 33,
            power_w: 70,
            processes: vec![],
        };
        let v = assess_waste(&g, Some(11.26), 25 * 60, 20 * 60);
        let j = serde_json::to_value(&v).unwrap();
        for key in ["is_wasting", "reason", "hourly_usd"] {
            assert!(j.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(j["is_wasting"], true);
    }
}
