//! Resources × price table → burn rate.

use crate::inventory::CloudResource;
use crate::pricing::PriceTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricedResource {
    pub resource: CloudResource,
    pub hourly_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRate {
    pub hourly_usd: f64,
    pub per_day_usd: f64,
    pub priced: Vec<PricedResource>,
    pub unpriced: Vec<CloudResource>,
}

/// Unpriced resources are collected, never dropped, so the UI can flag
/// "price unknown" instead of understating cost.
pub fn compute_burn(resources: &[CloudResource], table: &PriceTable) -> BurnRate {
    let mut priced = Vec::new();
    let mut unpriced = Vec::new();
    for r in resources {
        match table.hourly_usd(r) {
            Some(h) => priced.push(PricedResource {
                resource: r.clone(),
                hourly_usd: h,
            }),
            None => unpriced.push(r.clone()),
        }
    }
    let hourly_usd: f64 = priced.iter().map(|p| p.hourly_usd).sum();
    BurnRate {
        hourly_usd,
        per_day_usd: hourly_usd * 24.0,
        priced,
        unpriced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{CloudResource, ResourceKind};
    use crate::pricing::PriceTable;

    fn res(machine: Option<&str>) -> CloudResource {
        CloudResource {
            name: "r".into(),
            kind: ResourceKind::Instance,
            machine_type: machine.map(|s| s.into()),
            zone: "us-central1-a".into(),
            status: "RUNNING".into(),
            accelerators: vec![],
            created_iso: "2026-08-10T09:00:00Z".into(),
        }
    }

    #[test]
    fn sums_priced_and_lists_unpriced_separately() {
        let table = PriceTable::current();
        let b = compute_burn(
            &[res(Some("c3-standard-4")), res(Some("z99-unknown"))],
            &table,
        );
        assert!((b.hourly_usd - 0.15).abs() < 1e-9);
        assert!((b.per_day_usd - 0.15 * 24.0).abs() < 1e-6);
        assert_eq!(b.priced.len(), 1);
        assert_eq!(b.unpriced.len(), 1);
        assert_eq!(b.unpriced[0].machine_type.as_deref(), Some("z99-unknown"));
    }

    #[test]
    fn empty_inventory_is_zero_burn_no_lists() {
        let b = compute_burn(&[], &PriceTable::current());
        assert_eq!(b.hourly_usd, 0.0);
        assert_eq!(b.per_day_usd, 0.0);
        assert!(b.priced.is_empty());
        assert!(b.unpriced.is_empty());
    }

    #[test]
    fn all_unpriced_footprint_reports_zero_but_lists_everything() {
        // The dangerous case: every resource is an unknown type. Burn is 0 —
        // but the resources are ALL listed as unpriced, so the UI shows
        // "price unknown" for each rather than implying a free footprint.
        let b = compute_burn(
            &[res(Some("z1-unknown")), res(Some("z2-unknown"))],
            &PriceTable::current(),
        );
        assert_eq!(b.hourly_usd, 0.0);
        assert!(b.priced.is_empty());
        assert_eq!(b.unpriced.len(), 2);
    }

    #[test]
    fn multiple_priced_resources_sum() {
        let b = compute_burn(
            &[res(Some("c3-standard-4")), res(Some("n2d-standard-2"))],
            &PriceTable::current(),
        );
        assert!((b.hourly_usd - (0.15 + 0.09)).abs() < 1e-9);
        assert_eq!(b.priced.len(), 2);
        assert!(b.unpriced.is_empty());
    }

    #[test]
    fn priced_resource_carries_its_own_rate_and_identity() {
        let b = compute_burn(&[res(Some("c3-standard-4"))], &PriceTable::current());
        assert_eq!(b.priced[0].resource.name, "r");
        assert!((b.priced[0].hourly_usd - 0.15).abs() < 1e-9);
    }
}
