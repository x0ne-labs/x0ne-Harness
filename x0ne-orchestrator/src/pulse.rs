/// /pulse — System health snapshot, fully formatted by the orchestrator.
///
/// Runs a series of x0ne-runtime actions in sequence, collects the results,
/// and renders a compact, cinematic system briefing — no LLM required.
use colored::Colorize;
use serde_json::Value;
use std::time::Instant;

use crate::bridge::Bridge;

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run_pulse(bridge: &mut Box<dyn Bridge>) {
    let t0 = Instant::now();

    println!();
    println!(
        "  {}{}{}",
        "╭── ".dimmed(),
        "System Pulse".bold().bright_cyan(),
        " ─────────────────────────────────".dimmed()
    );

    // 1. Host summary — read from the OS, not fetched as an action. This used
    // a `SystemInfo` action that no longer exists, so the header had been
    // rendering "?" for every field.
    let host = crate::sysmon::read_host_summary().await;
    print_system_info(&host);

    // 2. Disk usage
    let disk = send(
        bridge,
        serde_json::json!({
            "type": "RunCommand",
            "command": "df",
            "args": ["-h", "--output=target,pcent,avail", "-x", "tmpfs", "-x", "devtmpfs"]
        }),
    )
    .await;
    print_disk(&disk);

    // 3. Failed systemd services
    let failed = send(
        bridge,
        serde_json::json!({
            "type": "RunCommand",
            "command": "systemctl",
            "args": ["--failed", "--no-pager", "--no-legend", "--plain"]
        }),
    )
    .await;
    let failed_services = parse_failed_services(&failed);

    // 4. Top 5 processes by memory
    let procs = send(bridge, serde_json::json!({"type": "ListProcesses"})).await;
    print_processes(&procs, &failed_services);

    // 5. Recent errors in system log
    let logs = send(
        bridge,
        serde_json::json!({
            "type": "RunCommand",
            "command": "journalctl",
            "args": ["-p", "err", "-n", "8", "--no-pager", "--no-hostname", "-o", "short"]
        }),
    )
    .await;
    let log_errors = parse_log_errors(&logs);
    print_log_errors(&log_errors);

    // ── Summary (derived from data, no LLM needed) ────────────────────────────
    let issues = build_issues(&host, &disk, &failed_services, &log_errors);
    print_summary(&issues, t0.elapsed().as_millis());

    println!(
        "  {}",
        "╰────────────────────────────────────────────────".dimmed()
    );
    println!();
}

// ── Section renderers ─────────────────────────────────────────────────────────

fn print_system_info(host: &crate::sysmon::HostSummary) {
    println!(
        "  {}  {}  {}  {}",
        "│".dimmed(),
        host.hostname.bold().white(),
        host.os.dimmed(),
        format!(
            "up {}  mem {}%",
            format_uptime(host.uptime_seconds),
            host.mem_pct()
        )
        .dimmed()
    );
    println!("  {}", "│".dimmed());
}

fn print_disk(disk: &Value) {
    let stdout = disk["output"]["stdout"].as_str().unwrap_or("");
    if stdout.is_empty() {
        return;
    }

    println!("  {}  {}", "│".dimmed(), "Disk".bold());
    for line in stdout.lines().skip(1) {
        // skip header
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let (mount, pct_str, avail) = (parts[0], parts[1], parts[2]);
        // Skip irrelevant mounts
        if mount.starts_with("/snap") || mount.starts_with("/boot/efi") {
            continue;
        }
        let pct: u64 = pct_str.trim_end_matches('%').parse().unwrap_or(0);
        let warn = if pct >= 90 {
            " ⚠".red().to_string()
        } else if pct >= 75 {
            " ⚠".yellow().to_string()
        } else {
            String::new()
        };
        println!(
            "  {}  {:20}  {}  {} free{}",
            "│".dimmed(),
            mount.dimmed(),
            colorize_pct(pct, 75, 90),
            avail.white(),
            warn
        );
    }
    println!("  {}", "│".dimmed());
}

fn parse_failed_services(result: &Value) -> Vec<String> {
    let stdout = result["output"]["stdout"].as_str().unwrap_or("");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
        .collect()
}

fn print_processes(procs: &Value, failed_services: &[String]) {
    // Print failed services first
    if !failed_services.is_empty() {
        println!("  {}  {}", "│".dimmed(), "Services".bold());
        for svc in failed_services {
            println!(
                "  {}  {} {} {}",
                "│".dimmed(),
                "✗".red().bold(),
                svc.red(),
                "(failed)".red().dimmed()
            );
        }
        println!("  {}", "│".dimmed());
    }

    // Top 5 processes by memory
    let empty = vec![];
    let mut proc_list: Vec<&Value> = procs["output"]["processes"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .collect();

    proc_list.sort_by(|a, b| {
        let ma = a["memory_mb"]
            .as_f64()
            .or_else(|| a["memory_kb"].as_f64().map(|v| v / 1024.0))
            .unwrap_or(0.0);
        let mb = b["memory_mb"]
            .as_f64()
            .or_else(|| b["memory_kb"].as_f64().map(|v| v / 1024.0))
            .unwrap_or(0.0);
        mb.partial_cmp(&ma).unwrap_or(std::cmp::Ordering::Equal)
    });

    if !proc_list.is_empty() {
        println!("  {}  {}", "│".dimmed(), "Top processes".bold());
        for proc in proc_list.iter().take(5) {
            let name = proc["name"]
                .as_str()
                .or_else(|| proc["cmd"].as_str())
                .unwrap_or("?");
            let mem = proc["memory_mb"]
                .as_f64()
                .or_else(|| proc["memory_kb"].as_f64().map(|v| v / 1024.0))
                .unwrap_or(0.0);
            let mem_str = if mem >= 1024.0 {
                format!("{:.1} GB", mem / 1024.0)
            } else {
                format!("{:.0} MB", mem)
            };
            let cpu = proc["cpu_percent"].as_f64().unwrap_or(0.0);
            println!(
                "  {}  {:25}  {:>8}  cpu {:.1}%",
                "│".dimmed(),
                name.white(),
                mem_str.bright_cyan(),
                cpu
            );
        }
        println!("  {}", "│".dimmed());
    }
}

fn parse_log_errors(result: &Value) -> Vec<String> {
    let stdout = result["output"]["stdout"].as_str().unwrap_or("");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            // Trim timestamp prefix to save space
            let parts: Vec<&str> = l.splitn(4, ' ').collect();
            if parts.len() >= 4 {
                parts[3..].join(" ")
            } else {
                l.to_string()
            }
        })
        .take(6)
        .collect()
}

fn print_log_errors(errors: &[String]) {
    if errors.is_empty() {
        return;
    }
    println!("  {}  {}", "│".dimmed(), "Recent errors".bold());
    for err in errors {
        // Byte-budget truncation clamped to the nearest UTF-8 boundary —
        // log lines routinely carry emoji (systemd status indicators,
        // x0ne's own ✓/✗, etc.), and `&err[..77]` panics when byte 77
        // lands mid-codepoint. See agent_loop.rs:643 for the original bug.
        let display = if err.len() > 80 {
            let mut cut = 77;
            while cut > 0 && !err.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}...", &err[..cut])
        } else {
            err.clone()
        };
        println!("  {}  {}", "│".dimmed(), display.red().dimmed());
    }
    println!("  {}", "│".dimmed());
}

// ── Issues + summary ──────────────────────────────────────────────────────────

fn build_issues(
    host: &crate::sysmon::HostSummary,
    disk: &Value,
    failed: &[String],
    log_errors: &[String],
) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();

    // Disk warnings
    let stdout = disk["output"]["stdout"].as_str().unwrap_or("");
    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        if parts[0].starts_with("/snap") {
            continue;
        }
        let pct: u64 = parts[1].trim_end_matches('%').parse().unwrap_or(0);
        if pct >= 90 {
            issues.push(format!("{} is critically full ({}%)", parts[0], pct));
        } else if pct >= 75 {
            issues.push(format!("{} is getting full ({}%)", parts[0], pct));
        }
    }

    // Memory
    if host.mem_total_mb > 0 && host.mem_pct() >= 90 {
        issues.push(format!("memory pressure: {}% used", host.mem_pct()));
    }

    // Failed services
    for svc in failed {
        issues.push(format!("{} is down", svc));
    }

    // Log errors
    if log_errors.len() >= 5 {
        issues.push(format!("{} system errors in recent logs", log_errors.len()));
    }

    issues
}

fn print_summary(issues: &[String], elapsed_ms: u128) {
    println!("  {}  {}", "│".dimmed(), "Summary".bold());
    if issues.is_empty() {
        println!(
            "  {}  {} all systems nominal",
            "│".dimmed(),
            "✓".bright_green().bold()
        );
    } else {
        for issue in issues {
            println!("  {}  {} {}", "│".dimmed(), "›".yellow(), issue.yellow());
        }
    }
    println!(
        "  {}  {}",
        "│".dimmed(),
        format!("pulse completed in {}ms", elapsed_ms).dimmed()
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn send(bridge: &mut Box<dyn Bridge>, action: Value) -> Value {
    match bridge.send_action(&action).await {
        Ok(v) => v,
        Err(_) => serde_json::json!({"output": {}, "success": false}),
    }
}

fn format_uptime(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m", secs / 60)
    }
}

fn colorize_pct(pct: u64, warn: u64, crit: u64) -> colored::ColoredString {
    let s = format!("{:>3}%", pct);
    if pct >= crit {
        s.red().bold()
    } else if pct >= warn {
        s.yellow()
    } else {
        s.bright_green()
    }
}
