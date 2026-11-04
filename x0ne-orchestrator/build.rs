//! Build script that bakes a short build stamp into the binary so
//! the tui header can display a "you're running the build from
//! X" line. The stamp = `<UTC YYMMDDhhmm>-<8-char git rev or ts>`.
//!
//! We don't depend on chrono here (build scripts shouldn't grow
//! transitive deps); the SystemTime → UTC string is hand-rolled
//! against the standard library. Output is shipped via the
//! `X0NE_BUILD_STAMP` cargo:rustc-env, available at runtime via
//! `env!("X0NE_BUILD_STAMP")`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Bust the cache every wall-clock second so `cargo build` always
    // re-runs this script and re-emits a fresh stamp. Without this,
    // cargo would keep the previous stamp and the user's "version
    // change every build" expectation would silently fail.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // SOURCE_DATE_EPOCH (reproducible-builds.org) wins over the wall clock:
    // the release job builds twice and compares the binaries byte for byte.
    let ts = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let date_part = fmt_yymmddhhmm(ts);

    // Short git rev — graceful no-op if git or the repo is missing.
    let git_rev = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    // Distinguish a dirty tree so two builds from the same commit
    // (one before, one after an edit) get different stamps.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let stamp = match git_rev {
        Some(rev) => {
            if dirty {
                format!("{}·{}*", date_part, rev)
            } else {
                format!("{}·{}", date_part, rev)
            }
        }
        // No git or no commits — fall back to a build timestamp tail.
        None => format!("{}·{:08x}", date_part, ts as u32),
    };

    println!("cargo:rustc-env=X0NE_BUILD_STAMP={}", stamp);
}

/// Format a UTC unix timestamp as `YYMMDDhhmm` (10 chars). Pure
/// integer math; we avoid pulling chrono into the build script.
fn fmt_yymmddhhmm(unix_ts: u64) -> String {
    // Algorithm: split seconds into days + time-of-day, then walk
    // the calendar from 1970 to find Y/M/D. UTC only; we don't
    // care about local tz inside a build artefact name.
    let secs_per_day: u64 = 86_400;
    let mut days = (unix_ts / secs_per_day) as i64;
    let rem = unix_ts % secs_per_day;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;

    let mut year: i64 = 1970;
    loop {
        let yd = if is_leap(year) { 366 } else { 365 };
        if days < yd {
            break;
        }
        days -= yd;
        year += 1;
    }
    let months: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month: i64 = 0;
    for (i, &m) in months.iter().enumerate() {
        let mlen = if i == 1 && is_leap(year) { 29 } else { m };
        if days < mlen {
            month = i as i64 + 1;
            break;
        }
        days -= mlen;
    }
    let day = days + 1;
    format!("{:02}{:02}{:02}{:02}{:02}", year % 100, month, day, hh, mm)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
