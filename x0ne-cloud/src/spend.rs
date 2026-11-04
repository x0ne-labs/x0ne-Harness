//! Live spend: what is billing RIGHT NOW, computed from what is running.
//!
//! Requested by the owner on 2026-08-21: « I want a way to monitor GCP
//! spending live ». The distinction that took a correction to get
//! right: the BigQuery billing export gives the amount actually charged, but
//! hours late — that is for reconciling, not for watching. The live figure is
//! computed, not fetched: what is running × the rate.
//!
//! Everything here is pure. The caller runs `gcloud`, this turns its JSON
//! into a report.

use crate::inventory::{CloudResource, ResourceKind};
use crate::pricing::PriceTable;
use serde::{Deserialize, Serialize};

/// One billable resource, as a reader needs to see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendLine {
    pub name: String,
    pub zone: String,
    /// `instance a3-highgpu-1g RUNNING`, `disk pd-balanced 50 GiB` — one
    /// string because its shape depends on the kind and a reader wants to
    /// read it, not destructure it.
    pub what: String,
    /// The rate this resource WOULD bill at, whether or not it is billing:
    /// a stopped H100 still reads 10.98, and `billing_now` is what says it is
    /// not being charged. Keeping the two apart is what lets a reader see
    /// what restarting the machine would cost.
    ///
    /// `None` means the price table does not know this resource. A caller
    /// MUST render that as "unknown", never as zero: the resources we cannot
    /// price are exactly the new and expensive ones.
    pub hourly_usd: Option<f64>,
    /// `false` for a stopped instance: it bills no compute, only its disks,
    /// which gcloud lists separately and this report prices separately.
    pub billing_now: bool,
    /// Hours since creation. `None` if the timestamp was unreadable.
    pub age_hours: Option<f64>,
    /// Cost if this resource had been in its CURRENT state since creation.
    ///
    /// An upper bound, and named so. A VM created a week ago and stopped for
    /// six days of it did not burn its hourly rate for a week; nothing in
    /// `gcloud compute instances list` says when it last started. For disks
    /// the bound is tight — a disk bills from creation whatever happens
    /// around it.
    pub accrued_upper_bound_usd: Option<f64>,
    /// This resource lives outside the region the price table was captured
    /// in, so its rate is borrowed from another region and is an ESTIMATE.
    ///
    /// Flagged rather than dropped: a caller in europe-west4 wants a number,
    /// and refusing to give one would push them back to guessing. But the
    /// borrowed number usually understates — Europe and Asia cost more than
    /// Iowa — so it must never pass for a measurement.
    pub priced_from_another_region: bool,
}

/// The whole picture at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendReport {
    pub as_of_unix: u64,
    pub price_table_as_of: String,
    pub lines: Vec<SpendLine>,
    /// Sum over resources that are billing now AND that we can price.
    pub hourly_usd: f64,
    pub per_day_usd: f64,
    pub per_month_usd: f64,
    /// Names of resources billing now that the table cannot price. Non-empty
    /// means `hourly_usd` is a FLOOR, not a total, and the caller must say so.
    pub unpriced: Vec<String>,
    /// The region every rate came from, so a reader can see whether it is
    /// their own.
    pub priced_region: String,
    /// Names of billing resources priced with another region's rates.
    pub out_of_region: Vec<String>,
}

impl SpendReport {
    /// Is the headline number the whole story? `false` when something billing
    /// could not be priced — the one case where a spend monitor lies by
    /// omission if it stays quiet.
    pub fn is_complete(&self) -> bool {
        self.unpriced.is_empty()
    }

    /// Is every priced line priced with its OWN region's rates? `false` means
    /// the totals are an estimate that most likely understates.
    pub fn is_regionally_exact(&self) -> bool {
        self.out_of_region.is_empty()
    }
}

/// `us-central1-a` → `us-central1`. A GCP zone is its region plus a
/// single-letter suffix; anything that does not look like one yields `None`
/// rather than a guess.
fn region_of(zone: &str) -> Option<&str> {
    let (region, suffix) = zone.rsplit_once('-')?;
    (!region.is_empty() && suffix.len() == 1 && suffix.chars().all(|c| c.is_ascii_alphabetic()))
        .then_some(region)
}

/// Does this status bill compute?
///
/// `TERMINATED` and `SUSPENDED` do not — that is the whole point of stopping
/// a VM, and reporting an H100 at 11 $/h while it is switched off would make
/// the monitor useless within a day. Every other status bills or may bill
/// (`PROVISIONING`, `STAGING`, `RUNNING`, `STOPPING`, `REPAIRING`), so the
/// default is to charge: for a spend monitor, overstating is recoverable and
/// understating is the failure that costs money.
fn instance_bills(status: &str) -> bool {
    !matches!(status, "TERMINATED" | "SUSPENDED")
}

/// Seconds since the Unix epoch for an ISO-8601 timestamp, or `None`.
///
/// gcloud renders `creationTimestamp` as `2026-08-20T09:00:00.000-07:00` —
/// local time with an offset, not UTC, and with fractional seconds. Handling
/// only `Z` would silently mis-date every resource by the zone's offset.
///
/// Written out rather than pulled from `chrono` because this crate is pure
/// serde by design; a date parser is exactly the kind of thing it should own
/// and test rather than depend on.
pub fn parse_iso8601_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }

    // Days from the civil date (Howard Hinnant's algorithm, the standard
    // branch-free one; correct for the proleptic Gregorian calendar).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut epoch = days * 86_400 + h * 3_600 + mi * 60 + sec;

    // The offset: `Z`, or `±HH:MM` / `±HHMM` at the very end. A local time
    // is EARLIER in UTC by its offset, so we subtract it.
    let tail = &s[19..];
    let tail = tail.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if tail != "Z" && !tail.is_empty() {
        let sign = match tail.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let digits: String = tail[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() != 4 {
            return None;
        }
        let oh: i64 = digits[..2].parse().ok()?;
        let om: i64 = digits[2..].parse().ok()?;
        if oh > 23 || om > 59 {
            return None;
        }
        epoch -= sign * (oh * 3_600 + om * 60);
    }
    Some(epoch)
}

/// Turn an inventory into the report a human reads.
pub fn report(resources: &[CloudResource], table: &PriceTable, now_unix: u64) -> SpendReport {
    let mut lines = Vec::new();
    let mut hourly = 0.0;
    let mut unpriced = Vec::new();
    let mut out_of_region = Vec::new();

    for r in resources {
        let (what, billing_now) = match &r.kind {
            ResourceKind::Instance => (
                format!(
                    "instance {} {}",
                    r.machine_type.as_deref().unwrap_or("?"),
                    r.status
                ),
                instance_bills(&r.status),
            ),
            ResourceKind::Disk { size_gb, disk_type } => {
                // A disk bills whether or not anything is attached to it.
                // That is the orphan-disk bill, and it is the reason this
                // kind exists.
                (format!("disk {disk_type} {size_gb} GiB"), true)
            }
        };
        let hourly_usd = table.hourly_usd(r);
        let age_hours = parse_iso8601_unix(&r.created_iso)
            .map(|created| (now_unix as i64 - created).max(0) as f64 / 3_600.0);
        let accrued = match (hourly_usd, age_hours) {
            (Some(h), Some(a)) if billing_now => Some(h * a),
            (_, _) if !billing_now => Some(0.0),
            _ => None,
        };
        // An unreadable zone counts as elsewhere: not knowing where a
        // resource lives is not a reason to price it as if it were here.
        let elsewhere = region_of(&r.zone) != Some(table.region);
        if billing_now {
            match hourly_usd {
                Some(h) => {
                    hourly += h;
                    if elsewhere {
                        out_of_region.push(r.name.clone());
                    }
                }
                None => unpriced.push(r.name.clone()),
            }
        }
        lines.push(SpendLine {
            name: r.name.clone(),
            zone: r.zone.clone(),
            what,
            hourly_usd,
            billing_now,
            age_hours,
            accrued_upper_bound_usd: accrued,
            priced_from_another_region: hourly_usd.is_some() && elsewhere,
        });
    }

    SpendReport {
        as_of_unix: now_unix,
        price_table_as_of: table.as_of.to_string(),
        lines,
        hourly_usd: hourly,
        per_day_usd: hourly * 24.0,
        per_month_usd: hourly * 730.0,
        unpriced,
        priced_region: table.region.to_string(),
        out_of_region,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Accel, CloudResource, ResourceKind};

    fn instance(name: &str, status: &str, created: &str) -> CloudResource {
        CloudResource {
            name: name.into(),
            kind: ResourceKind::Instance,
            machine_type: Some("a3-highgpu-1g".into()),
            zone: "us-central1-a".into(),
            status: status.into(),
            accelerators: vec![Accel {
                model: "nvidia-h100-80gb".into(),
                count: 1,
            }],
            created_iso: created.into(),
        }
    }

    fn disk(name: &str, size_gb: u32, disk_type: &str, created: &str) -> CloudResource {
        CloudResource {
            name: name.into(),
            kind: ResourceKind::Disk {
                size_gb,
                disk_type: disk_type.into(),
            },
            machine_type: None,
            zone: "us-central1-a".into(),
            status: "READY".into(),
            accelerators: vec![],
            created_iso: created.into(),
        }
    }

    /// The offset is not decoration: gcloud emits local time. Reading
    /// `2026-08-20T09:00:00.000-07:00` as if it ended in `Z` mis-dates the
    /// resource by seven hours, which is seven hours of an H100 in the
    /// accrued column.
    #[test]
    fn timestamps_are_read_with_their_offset_not_as_utc() {
        // 2026-08-20T09:00:00-07:00 is 16:00:00Z the same day.
        let local = parse_iso8601_unix("2026-08-20T09:00:00.000-07:00").expect("parses");
        let utc = parse_iso8601_unix("2026-08-20T16:00:00Z").expect("parses");
        assert_eq!(local, utc, "the offset must be applied");
        // And a positive offset goes the other way.
        assert_eq!(
            parse_iso8601_unix("2026-08-20T18:00:00+02:00"),
            parse_iso8601_unix("2026-08-20T16:00:00Z")
        );
    }

    /// Known epochs, so the civil-date arithmetic is pinned rather than
    /// trusted — including a leap day and a century that is not a leap year
    /// on the far side of the century rule.
    #[test]
    fn the_epoch_arithmetic_matches_known_dates() {
        assert_eq!(parse_iso8601_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_unix("2000-03-01T00:00:00Z"),
            Some(951_868_800)
        );
        assert_eq!(
            parse_iso8601_unix("2024-02-29T12:00:00Z"),
            Some(1_709_208_000)
        );
        assert_eq!(
            parse_iso8601_unix("2026-08-22T00:00:00Z"),
            Some(1_787_356_800)
        );
    }

    /// Garbage is `None`, never a plausible-looking date. A monitor that
    /// invents a creation time invents an accrued cost.
    #[test]
    fn an_unreadable_timestamp_is_none_not_a_guess() {
        for bad in [
            "",
            "not a date",
            "2026-13-01T00:00:00Z",
            "2026-08-22",
            "2026-08-22T25:00:00Z",
        ] {
            assert_eq!(parse_iso8601_unix(bad), None, "{bad:?} must not parse");
        }
    }

    /// SECURITY of the wallet, and the point of the whole module: a STOPPED
    /// H100 bills no compute. Reporting its 10.98 $/h while it is switched
    /// off would make every reading useless — and worse, it would train the
    /// reader to ignore the number, which is exactly when a real one appears.
    #[test]
    fn a_stopped_instance_bills_nothing_while_its_disk_still_does() {
        let now = parse_iso8601_unix("2026-08-22T00:00:00Z").unwrap() as u64;
        let day_ago = "2026-08-21T00:00:00Z";
        let r = report(
            &[
                instance("m2", "TERMINATED", day_ago),
                disk("m2", 50, "pd-balanced", day_ago),
            ],
            &PriceTable::current(),
            now,
        );
        // 50 GiB × 0.10 $/GiB-month ÷ 730 h = 0.006849 $/h.
        assert!(
            (r.hourly_usd - 0.006_849).abs() < 1e-5,
            "only the disk bills: {}",
            r.hourly_usd
        );
        assert!(r.is_complete(), "both resources are priced");
        let vm = &r.lines[0];
        assert!(!vm.billing_now);
        assert_eq!(vm.accrued_upper_bound_usd, Some(0.0));
        let d = &r.lines[1];
        assert!(d.billing_now);
        assert!((d.age_hours.unwrap() - 24.0).abs() < 1e-9);
        assert!((d.accrued_upper_bound_usd.unwrap() - 0.164_38).abs() < 1e-4);
    }

    /// A running H100 is the number the owner needs to see immediately.
    #[test]
    fn a_running_h100_reports_its_real_rate_per_hour_day_and_month() {
        let now = parse_iso8601_unix("2026-08-22T02:00:00Z").unwrap() as u64;
        let r = report(
            &[instance("burner", "RUNNING", "2026-08-22T00:00:00Z")],
            &PriceTable::current(),
            now,
        );
        assert!((r.hourly_usd - 10.98).abs() < 1e-9, "{}", r.hourly_usd);
        assert!((r.per_day_usd - 263.52).abs() < 1e-9);
        assert!((r.per_month_usd - 8_015.4).abs() < 1e-6);
        assert!((r.lines[0].accrued_upper_bound_usd.unwrap() - 21.96).abs() < 1e-9);
    }

    /// SECURITY of the wallet: a resource we cannot price is billing anyway.
    /// The headline must be a FLOOR that says so, never a total that quietly
    /// omits the row — the resources absent from the table are the new ones,
    /// which is to say the expensive ones.
    #[test]
    fn an_unpriced_running_resource_makes_the_total_an_admitted_floor() {
        let now = parse_iso8601_unix("2026-08-22T00:00:00Z").unwrap() as u64;
        let mut mystery = instance("mystery", "RUNNING", "2026-08-21T00:00:00Z");
        mystery.machine_type = Some("z9-ultra-2048".into());
        let r = report(&[mystery], &PriceTable::current(), now);
        assert_eq!(r.hourly_usd, 0.0);
        assert!(!r.is_complete(), "the report must admit it is incomplete");
        assert_eq!(r.unpriced, vec!["mystery".to_string()]);
        assert_eq!(r.lines[0].hourly_usd, None, "never zero for unknown");
        assert_eq!(r.lines[0].accrued_upper_bound_usd, None);
    }

    /// A stopped instance we cannot price does NOT make the report
    /// incomplete: it bills nothing, so nothing is missing from the total.
    /// Otherwise every stopped experiment would raise a false alarm, and a
    /// monitor that cries wolf stops being read.
    #[test]
    fn an_unpriced_stopped_instance_does_not_raise_a_false_alarm() {
        let now = parse_iso8601_unix("2026-08-22T00:00:00Z").unwrap() as u64;
        let mut mystery = instance("old", "TERMINATED", "2026-08-01T00:00:00Z");
        mystery.machine_type = Some("z9-ultra-2048".into());
        let r = report(&[mystery], &PriceTable::current(), now);
        assert!(r.is_complete());
        assert_eq!(r.hourly_usd, 0.0);
    }

    /// SECURITY of the wallet: every rate in the table is us-central1's, and
    /// a resource somewhere else is priced by BORROWING them.
    ///
    /// The silence was the defect: a Dutch H100 got an Iowa price and nothing
    /// said so. Europe and Asia are typically dearer than us-central1, so the
    /// borrowed number understates — the direction that costs money. A
    /// failure here means the report is confidently pricing machines it has
    /// no rates for.
    #[test]
    fn a_resource_outside_the_priced_region_is_flagged_not_silently_borrowed() {
        let now = parse_iso8601_unix("2026-08-22T00:00:00Z").unwrap() as u64;
        let mut abroad = instance("amsterdam", "RUNNING", "2026-08-21T00:00:00Z");
        abroad.zone = "europe-west4-a".into();
        let home = instance("iowa", "RUNNING", "2026-08-21T00:00:00Z");
        let r = report(&[abroad, home], &PriceTable::current(), now);

        assert_eq!(r.priced_region, "us-central1");
        assert!(
            !r.is_regionally_exact(),
            "the report must admit it borrowed"
        );
        assert_eq!(r.out_of_region, vec!["amsterdam".to_string()]);
        assert!(r.lines[0].priced_from_another_region);
        assert!(!r.lines[1].priced_from_another_region);
        // It is still counted: refusing a number pushes the reader back to
        // guessing, which is worse than a flagged estimate.
        assert!((r.hourly_usd - 21.96).abs() < 1e-9, "{}", r.hourly_usd);
        // And nothing here is "incomplete" — that word is reserved for
        // resources we cannot price at all.
        assert!(r.is_complete());
    }

    /// A zone we cannot read counts as elsewhere. Not knowing where a
    /// resource lives is not a reason to price it as if it were here.
    #[test]
    fn an_unreadable_zone_is_treated_as_another_region() {
        let now = parse_iso8601_unix("2026-08-22T00:00:00Z").unwrap() as u64;
        for bad_zone in ["", "us-central1", "weird", "us-central1-ab"] {
            let mut odd = instance("odd", "RUNNING", "2026-08-21T00:00:00Z");
            odd.zone = bad_zone.into();
            let r = report(&[odd], &PriceTable::current(), now);
            assert!(
                !r.is_regionally_exact(),
                "zone {bad_zone:?} must not pass for us-central1"
            );
        }
        // …and the real shape does pass, or the check above would be vacuous.
        let r = report(
            &[instance("ok", "RUNNING", "2026-08-21T00:00:00Z")],
            &PriceTable::current(),
            now,
        );
        assert!(r.is_regionally_exact());
    }

    /// An unreadable creation date costs us the accrued column and nothing
    /// else: the rate is still known, and the rate is what live monitoring is
    /// for.
    #[test]
    fn an_unreadable_creation_date_still_yields_the_rate() {
        let r = report(
            &[instance("odd", "RUNNING", "whenever")],
            &PriceTable::current(),
            1_787_356_800,
        );
        assert!((r.hourly_usd - 10.98).abs() < 1e-9);
        assert_eq!(r.lines[0].age_hours, None);
        assert_eq!(r.lines[0].accrued_upper_bound_usd, None);
    }
}
