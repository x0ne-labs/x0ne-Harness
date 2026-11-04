//! `x0ne cloud-cost` — what GCP is billing right now, in a terminal.
//!
//! Requested by the owner on 2026-08-21: « I want a way to monitor GCP
//! spending live ». The model was already complete — `x0ne-cloud`
//! parses, prices and totals — and reachable from the agent and from MCP.
//! What was missing was a door: `grep cloud_cost x0ne-code/src/main.rs`
//! returned nothing, so a human at a prompt had no way in.
//!
//! Live means COMPUTED, not fetched. The BigQuery billing export knows what
//! was actually charged, hours later; that is for reconciling. This is what
//! is running times the rate, now.

use anyhow::{Context, Result};
use colored::Colorize;
use std::io::Write;
use x0ne_cloud::inventory::{parse_gcloud_disks, parse_gcloud_instances, CloudResource};
use x0ne_cloud::pricing::PriceTable;
use x0ne_cloud::spend::{report, SpendReport};

/// Read-only `gcloud` listings.
///
/// The argv is entirely constant — no operator input, no model output and no
/// remote text reaches this line — and every command listed here only reads.
/// That is what justifies going straight to the process rather than through
/// the runtime bridge: a spend monitor that needs a runtime, a policy file
/// and an API key to answer "what is billing?" would not be run, and a
/// monitor that is not run is worth nothing.
fn gcloud_json(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("gcloud")
        .args(args)
        .output()
        .context("gcloud is not on PATH; install the Google Cloud CLI")?;
    if !out.status.success() {
        anyhow::bail!(
            "gcloud {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Everything gcloud will tell us about what can bill.
fn collect() -> Result<Vec<CloudResource>> {
    let instances = gcloud_json(&["compute", "instances", "list", "--format=json"])?;
    let disks = gcloud_json(&["compute", "disks", "list", "--format=json"])?;
    let mut all = parse_gcloud_instances(&instances);
    all.extend(parse_gcloud_disks(&disks));
    Ok(all)
}

fn usd(v: f64) -> String {
    if v >= 1.0 {
        format!("{v:.2} $")
    } else if v >= 0.01 {
        format!("{v:.3} $")
    } else {
        format!("{v:.5} $")
    }
}

fn age(hours: Option<f64>) -> String {
    match hours {
        None => "?".into(),
        Some(h) if h < 1.0 => format!("{:.0} min", h * 60.0),
        Some(h) if h < 48.0 => format!("{h:.1} h"),
        Some(h) => format!("{:.1} d", h / 24.0),
    }
}

/// The human report. Pure: takes a report, returns the text. Every assertion
/// a test makes about this command's output is made here, with no gcloud in
/// the room.
pub fn render(r: &SpendReport) -> String {
    let mut s = String::new();
    if r.lines.is_empty() {
        s.push_str("Nothing exists in this project that can bill.\n");
        return s;
    }

    let name_w = r
        .lines
        .iter()
        .map(|l| l.name.len())
        .max()
        .unwrap_or(4)
        .max(8);
    let what_w = r
        .lines
        .iter()
        .map(|l| l.what.len())
        .max()
        .unwrap_or(4)
        .max(10);
    s.push_str(&format!(
        "  {:<name_w$}  {:<what_w$}  {:>12}  {:>9}  {:>12}\n",
        "NAME", "WHAT", "PER HOUR", "AGE", "ACCRUED ≤"
    ));
    for l in &r.lines {
        let rate = match (l.hourly_usd, l.billing_now) {
            (_, false) => "not billing".dimmed().to_string(),
            (None, true) => "UNKNOWN".red().bold().to_string(),
            (Some(h), true) => usd(h),
        };
        // Width has to be applied before colouring: escape codes count as
        // characters to `{:>12}` and would shear the column.
        let plain = match (l.hourly_usd, l.billing_now) {
            (_, false) => "not billing".to_string(),
            (None, true) => "UNKNOWN".to_string(),
            (Some(h), true) => usd(h),
        };
        // A borrowed rate is marked where it is read, not only in a footnote.
        let plain = if l.priced_from_another_region {
            format!("~{plain}")
        } else {
            plain
        };
        let rate = if l.priced_from_another_region {
            format!("~{rate}")
        } else {
            rate
        };
        let pad = " ".repeat(12usize.saturating_sub(plain.chars().count()));
        s.push_str(&format!(
            "  {:<name_w$}  {:<what_w$}  {pad}{rate}  {:>9}  {:>12}\n",
            l.name,
            l.what,
            age(l.age_hours),
            l.accrued_upper_bound_usd.map(usd).unwrap_or("?".into()),
        ));
    }

    s.push('\n');
    s.push_str(&format!(
        "  {}  {} per hour   {} per day   {} per month\n",
        if r.is_complete() { "TOTAL" } else { "FLOOR" },
        usd(r.hourly_usd),
        usd(r.per_day_usd),
        usd(r.per_month_usd),
    ));
    if !r.is_complete() {
        // The one thing a spend monitor must never do quietly.
        s.push_str(&format!(
            "  {} {} billing now cannot be priced ({}), so the figures above are a\n  floor, not a total. Resources missing from the table are the new ones.\n",
            "!".red().bold(),
            r.unpriced.len(),
            r.unpriced.join(", ")
        ));
    }
    if !r.is_regionally_exact() {
        // Understating is the direction that costs money, and borrowing
        // us-central1 rates for Europe or Asia understates.
        s.push_str(&format!(
            "  {} {} priced with {} rates while living elsewhere ({}), marked ~ above.\n  Most regions cost MORE than {}, so those lines likely understate.\n",
            "~".yellow().bold(),
            r.out_of_region.len(),
            r.priced_region,
            r.out_of_region.join(", "),
            r.priced_region,
        ));
    }
    s.push_str(&format!(
        "  {} rates as of {}; accrued is an upper bound (a stopped VM does not\n  burn its rate while stopped, and nothing here says when it last ran).\n",
        r.priced_region, r.price_table_as_of
    ));
    s
}

/// The notification text, per the global rule 7: it is READ ALOUD, so no
/// markup, no paths, no resource names (they are full of hyphens, which the
/// speech synthesiser reads out or stumbles on), and one idea. Numbers speak
/// well. Returns title and body.
pub fn alert_text(r: &SpendReport) -> (String, String) {
    let dollars = r.hourly_usd.round() as i64;
    let title = if dollars >= 1 {
        format!("GCP burns {dollars} dollars per hour")
    } else {
        "GCP bills less than one dollar per hour".to_string()
    };
    let billing = r.lines.iter().filter(|l| l.billing_now).count();
    let mut body = if billing == 1 {
        "One resource is billing.".to_string()
    } else {
        format!("{billing} resources are billing.")
    };
    if !r.is_complete() {
        body.push_str(" The total is a floor, a resource has no known price.");
    }
    if !r.is_regionally_exact() {
        // Spoken, so one short clause. It fires only above the alert
        // threshold, which is exactly when the number needs to be right.
        body.push_str(
            " Some rates come from another region, the figure is probably an underestimate.",
        );
    }
    body.push_str(&format!(
        " That is {} dollars per day.",
        r.per_day_usd.round()
    ));
    (title, body)
}

fn notify(title: &str, body: &str) {
    // Best effort: a monitor that fails because a desktop bus is absent is
    // worse than a monitor that prints and moves on.
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "x0ne", "-u", "critical", title, body])
        .status();
}

pub struct CostArgs {
    /// Refresh every N seconds instead of printing once.
    pub watch: Option<u64>,
    pub json: bool,
    /// Notify above this hourly rate. The owner's GCP budget alerts fire at
    /// 50 $ and 25 €, and they fire AFTER the fact; this one fires while the
    /// meter is still running.
    pub alert_usd_per_hour: f64,
}

pub fn run(args: CostArgs) -> Result<()> {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    let table = PriceTable::current();

    let once = |already_alerted: &mut bool| -> Result<()> {
        let r = report(&collect()?, &table, now());
        if args.json {
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            print!("{}", render(&r));
        }
        // Once per crossing, not once per tick: a notification that repeats
        // every refresh stops being heard, and this one is spoken out loud.
        if r.hourly_usd >= args.alert_usd_per_hour {
            if !*already_alerted {
                let (t, b) = alert_text(&r);
                notify(&t, &b);
                *already_alerted = true;
            }
        } else {
            *already_alerted = false;
        }
        Ok(())
    };

    match args.watch {
        None => once(&mut false),
        Some(secs) => {
            let secs = secs.max(5); // gcloud is rate-limited and not free to call
            let mut alerted = false;
            loop {
                // Clear and home, so the reading stays in one place.
                print!("\x1b[2J\x1b[H");
                let _ = std::io::stdout().flush();
                if let Err(e) = once(&mut alerted) {
                    println!("  {} {e}", "gcloud failed:".red());
                }
                println!("\n  refreshing every {secs} s — Ctrl-C to stop");
                let _ = std::io::stdout().flush();
                std::thread::sleep(std::time::Duration::from_secs(secs));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x0ne_cloud::inventory::{Accel, ResourceKind};

    fn h100(status: &str) -> CloudResource {
        CloudResource {
            name: "x0ne-infer-m2".into(),
            kind: ResourceKind::Instance,
            machine_type: Some("a3-highgpu-1g".into()),
            zone: "us-central1-a".into(),
            status: status.into(),
            accelerators: vec![Accel {
                model: "nvidia-h100-80gb".into(),
                count: 1,
            }],
            created_iso: "2026-08-21T00:00:00Z".into(),
        }
    }

    fn disk() -> CloudResource {
        CloudResource {
            name: "x0ne-infer-m2".into(),
            kind: ResourceKind::Disk {
                size_gb: 50,
                disk_type: "pd-balanced".into(),
            },
            machine_type: None,
            zone: "us-central1-a".into(),
            status: "READY".into(),
            accelerators: vec![],
            created_iso: "2026-08-21T00:00:00Z".into(),
        }
    }

    fn at(rs: &[CloudResource]) -> SpendReport {
        report(rs, &PriceTable::current(), 1_787_356_800)
    }

    /// The state the owner is actually in most of the time: nothing running,
    /// one disk quietly billing. The report must show it rather than say
    /// zero — an orphan or forgotten disk is the bill nobody notices.
    #[test]
    fn a_stopped_vm_and_its_disk_render_as_not_billing_and_billing() {
        let out = render(&at(&[h100("TERMINATED"), disk()]));
        assert!(out.contains("not billing"), "{out}");
        assert!(out.contains("disk pd-balanced 50 GiB"), "{out}");
        assert!(out.contains("TOTAL"), "the total is complete: {out}");
        // 50 GiB × 0.10 ÷ 730 = 0.00685 $/h → 0.164 $/day.
        assert!(out.contains("0.164 $ per day"), "{out}");
    }

    /// A borrowed rate is visible where it is read AND explained at the
    /// bottom. A flag only the JSON carries is a flag nobody sees.
    #[test]
    fn a_rate_borrowed_from_another_region_is_marked_in_the_table_and_explained() {
        let mut abroad = h100("RUNNING");
        abroad.name = "amsterdam".into();
        abroad.zone = "europe-west4-a".into();
        let out = render(&at(&[abroad]));
        assert!(out.contains("~"), "the borrowed line must be marked: {out}");
        assert!(
            out.contains("likely understate"),
            "the direction of the error must be named: {out}"
        );
        assert!(out.contains("amsterdam"), "{out}");
        // The ordinary case stays unmarked, or the mark means nothing.
        let clean = render(&at(&[h100("RUNNING")]));
        assert!(
            !clean.contains("likely understate"),
            "a same-region report must not cry wolf: {clean}"
        );
    }

    /// SECURITY of the wallet: an unpriced resource that is BILLING turns the
    /// headline into an admitted floor, names what is missing, and never
    /// prints a zero in its rate column. A monitor that silently omits the
    /// row it cannot price understates exactly where the money is.
    #[test]
    fn an_unpriced_billing_resource_makes_the_headline_say_floor() {
        let mut mystery = h100("RUNNING");
        mystery.name = "mystery".into();
        mystery.machine_type = Some("z9-ultra-2048".into());
        let out = render(&at(&[mystery]));
        assert!(out.contains("FLOOR"), "{out}");
        assert!(!out.contains("TOTAL"), "{out}");
        assert!(
            out.contains("UNKNOWN"),
            "the rate column must not read 0: {out}"
        );
        assert!(
            out.contains("mystery"),
            "the unpriced resource must be named: {out}"
        );
    }

    /// The notification is written to be READ ALOUD by a text-to-speech
    /// rule of the notification daemon. A failure here means the speech synthesiser will be handed a
    /// hyphen, a path or a symbol — `x0ne-infer-m2` is spoken as "x0ne dash
    /// infer dash m two", and the whole point of the rule is that a spoken
    /// notification carries one idea a human can act on.
    #[test]
    fn the_alert_carries_no_hyphen_no_path_and_no_resource_name() {
        let (title, body) = alert_text(&at(&[h100("RUNNING"), disk()]));
        for text in [&title, &body] {
            assert!(!text.contains('-'), "hyphen in {text:?}");
            assert!(!text.contains('—'), "em dash in {text:?}");
            assert!(!text.contains('/'), "path in {text:?}");
            assert!(!text.contains("x0ne"), "resource name in {text:?}");
            assert!(text.is_ascii(), "non-ascii in {text:?}");
        }
        // The title must carry the information, not announce it.
        assert!(
            title.contains("11"),
            "the rate belongs in the title: {title}"
        );
        assert!(body.contains("2 resources"), "{body}");
    }

    /// The floor caveat reaches the spoken alert too. Hearing a number that
    /// is quietly incomplete is worse than hearing no number.
    #[test]
    fn the_alert_says_when_the_figure_is_only_a_floor() {
        let mut mystery = h100("RUNNING");
        mystery.machine_type = Some("z9-ultra-2048".into());
        let (_, body) = alert_text(&at(&[mystery]));
        assert!(body.contains("floor"), "{body}");
    }

    /// The spoken alert also says when the rates were borrowed. It fires only
    /// above the threshold, which is when a wrong number matters most.
    #[test]
    fn the_alert_says_when_the_rates_came_from_another_region() {
        let mut abroad = h100("RUNNING");
        abroad.zone = "europe-west4-a".into();
        let (title, body) = alert_text(&at(&[abroad]));
        assert!(body.contains("underestimate"), "{body}");
        for text in [&title, &body] {
            assert!(!text.contains('-'), "hyphen in {text:?}");
            assert!(text.is_ascii(), "non-ascii in {text:?}");
        }
    }

    /// An empty project says so, rather than printing an empty table with a
    /// confident 0 under it.
    #[test]
    fn an_empty_project_says_nothing_can_bill() {
        let out = render(&at(&[]));
        assert!(out.contains("Nothing exists"), "{out}");
    }
}
