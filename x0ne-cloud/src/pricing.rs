//! Embedded GCP on-demand price table.
//!
//! Unknown types return `None` so callers surface "price unknown" rather than
//! lying with zero. Rates are us-central1 on-demand, USD/hour.

use crate::inventory::{CloudResource, ResourceKind};

/// Embedded price table with a visible capture date.
pub struct PriceTable {
    /// The date these rates were captured — shown in the UI so a stale table
    /// is visible, not silent.
    pub as_of: &'static str,
    /// The GCP region these rates were captured IN.
    ///
    /// Every rate here is regional, and the table has exactly one region's
    /// worth. Carrying it lets a caller say so instead of pricing a Dutch
    /// H100 at Iowa rates in silence — and the silence was the bug: European
    /// and Asian regions are typically DEARER than us-central1, so borrowing
    /// these numbers understates, which is the direction that costs money.
    pub region: &'static str,
}

/// Machine-type hourly rate (vCPU+RAM bundle), USD. Source: GCP console
/// estimate, 2026-08-10 (a3-highgpu-1g quoted at ~$11.26/h total incl. 1 H100;
/// the machine bundle alone is ~$1.18/h, the H100 accelerator ~$9.80/h).
fn machine_hourly(machine_type: &str) -> Option<f64> {
    match machine_type {
        "a3-highgpu-1g" => Some(1.18),
        "c3-standard-4" => Some(0.15),
        "n2d-standard-2" => Some(0.09),
        _ => None,
    }
}

/// Accelerator hourly rate, USD per unit.
fn accel_hourly(model: &str) -> Option<f64> {
    match model {
        "nvidia-h100-80gb" => Some(9.80),
        _ => None,
    }
}

/// Persistent-disk rate, USD per GiB-month.
///
/// MEASURED, not recalled: Cloud Billing Catalog API, service 6F81-5844-456A
/// (Compute Engine), `serviceRegions` containing `us-central1`, OnDemand,
/// read on 2026-08-22. The SKU descriptions are quoted so the next reader can
/// find the same rows.
///
/// `pd-standard` is free below 30 GiB and 0.04 above ("Storage PD Capacity"
/// has two tiers: 0 GiB → 0.00, 30 GiB → 0.04). We charge the upper tier
/// throughout: for a small disk that OVERSTATES by at most 1.20 $/month,
/// and overstating is the safe direction for a spend monitor. Reading only
/// the first tier — which is what a naive query returns — would have priced
/// every standard disk at zero.
fn disk_gib_month_usd(disk_type: &str) -> Option<f64> {
    match disk_type {
        "pd-balanced" => Some(0.10),          // "Balanced PD Capacity"
        "pd-ssd" => Some(0.17),               // "SSD backed PD Capacity"
        "pd-extreme" => Some(0.125),          // "Extreme PD Capacity"
        "pd-standard" => Some(0.04),          // "Storage PD Capacity", tier 2
        "regional-pd-balanced" => Some(0.20), // "Regional Balanced PD Capacity"
        "regional-pd-ssd" => Some(0.34),      // "Regional SSD backed PD Capacity"
        _ => None,
    }
}

/// Hours in the month GCP bills against. GCP prorates GiB-month by the hour
/// on a 730-hour month (365 × 24 ÷ 12), which is the figure their own
/// documentation and calculator use.
const HOURS_PER_MONTH: f64 = 730.0;

impl PriceTable {
    pub fn current() -> Self {
        Self {
            // Two capture dates, said as two, because they are: the compute
            // rates come from the GCP console on 2026-08-10, the disk rates
            // from the Cloud Billing Catalog API on 2026-08-22. Printing the
            // older one alone would understate the table's freshness;
            // printing the newer one alone would overstate it, which is the
            // direction that matters.
            as_of: "2026-08-10 (compute), 2026-08-22 (disks)",
            region: "us-central1",
        }
    }

    /// Hourly cost of a resource. `None` if the machine type is unknown —
    /// callers must render that as "price unknown", never as 0. Status is
    /// deliberately not the gate: a stopped VM keeps billing storage, so
    /// pricing by machine type and letting the caller weigh status is the
    /// honest default.
    pub fn hourly_usd(&self, r: &CloudResource) -> Option<f64> {
        // A disk is priced by capacity and type, and it bills while nothing
        // is attached to it — which is the whole reason the kind exists.
        if let ResourceKind::Disk { size_gb, disk_type } = &r.kind {
            let per_gib_month = disk_gib_month_usd(disk_type)?;
            return Some(per_gib_month * *size_gb as f64 / HOURS_PER_MONTH);
        }
        let machine = r.machine_type.as_deref()?;
        let base = machine_hourly(machine)?;
        // An UNKNOWN accelerator makes the whole resource unpriced. The
        // alternative — pricing it at 0 — would quietly understate exactly
        // the resources where the money is (the GPU is 87% of the H100 VM's
        // hourly rate), which is the "never 0 for unknown" rule's worst case.
        let mut accel = 0.0;
        for a in &r.accelerators {
            accel += accel_hourly(&a.model)? * a.count as f64;
        }
        Some(base + accel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Accel, CloudResource, ResourceKind};

    fn h100_vm() -> CloudResource {
        CloudResource {
            name: "h100-vm".into(),
            kind: ResourceKind::Instance,
            machine_type: Some("a3-highgpu-1g".into()),
            zone: "us-central1-a".into(),
            status: "RUNNING".into(),
            accelerators: vec![Accel {
                model: "nvidia-h100-80gb".into(),
                count: 1,
            }],
            created_iso: "2026-08-10T09:00:00Z".into(),
        }
    }

    fn t_price(r: &CloudResource) -> Option<f64> {
        PriceTable::current().hourly_usd(r)
    }

    #[test]
    fn prices_the_h100_config_close_to_the_console_estimate() {
        let hourly = t_price(&h100_vm()).expect("a3-highgpu-1g must be priced");
        // GCP console quoted ~$11.26/h for a3-highgpu-1g + 1x H100. Allow slack
        // for disk lines the console adds; assert the machine+GPU core.
        assert!((10.0..=12.5).contains(&hourly), "got {hourly}");
    }

    #[test]
    fn unknown_machine_type_is_none_never_zero() {
        let mut r = h100_vm();
        r.machine_type = Some("z99-imaginary".into());
        r.accelerators.clear();
        assert_eq!(t_price(&r), None);
    }

    #[test]
    fn unknown_accelerator_makes_the_whole_resource_unpriced() {
        // The bug this pins: accel_hourly(...).unwrap_or(0.0) priced an
        // unknown GPU at $0 — quietly understating exactly the resources
        // where the money is (the GPU is ~87% of the H100 VM's rate). A
        // machine with an unpriceable accelerator must be UNPRICED, so it
        // lands in the visible "price unknown" list instead of shrinking
        // the burn figure.
        let mut r = h100_vm();
        r.accelerators = vec![Accel {
            model: "nvidia-b300-ultra".into(),
            count: 1,
        }];
        assert_eq!(t_price(&r), None);
    }

    #[test]
    fn accelerator_count_multiplies() {
        let mut r = h100_vm();
        r.accelerators[0].count = 2;
        let two = t_price(&r).unwrap();
        r.accelerators[0].count = 1;
        let one = t_price(&r).unwrap();
        assert!(two > one + 9.0, "two H100s must add a second GPU rate");
    }

    #[test]
    fn machine_without_accelerators_prices_the_base_alone() {
        let mut r = h100_vm();
        r.machine_type = Some("c3-standard-4".into());
        r.accelerators.clear();
        let hourly = t_price(&r).unwrap();
        assert!((hourly - 0.15).abs() < 1e-9, "got {hourly}");
    }

    #[test]
    fn resource_with_no_machine_type_is_unpriced() {
        // Disks parse with machine_type = None: they must land in the
        // "price unknown" list, never silently at 0.
        let mut r = h100_vm();
        r.machine_type = None;
        assert_eq!(t_price(&r), None);
    }

    #[test]
    fn stopped_instance_still_prices_nonzero_if_known() {
        // A stopped VM keeps billing its disk in reality; at minimum it must
        // not price to 0 just because status != RUNNING — status is not the
        // gate, machine type is.
        let mut r = h100_vm();
        r.status = "TERMINATED".into();
        assert!(t_price(&r).is_some());
    }
}
