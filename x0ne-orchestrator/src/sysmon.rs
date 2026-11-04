/// Background system monitor — polls machine state every 5s without using the bridge.
///
/// Reads directly from /proc/meminfo, /proc/loadavg, df —
/// so it works independently of x0ne-runtime and doesn't compete with the bridge.
use std::sync::{Arc, RwLock};
use tokio::time::Duration;

#[derive(Clone, Default)]
pub struct SysState {
    pub mem_pct: u64,
    pub mem_used_gb: f64,
    pub mem_total_gb: f64,
    pub disk_pct: u64,
    pub disk_mount: String,
    pub load_1: f64,
    pub alerts: Vec<String>,
}

impl SysState {
    /// Single-line status for the spinner/navbar, e.g. "mem 42% · disk 38% · load 0.5"
    pub fn status_line(&self) -> String {
        let mem = format!("mem {}%", self.mem_pct);
        let disk = if self.disk_mount.is_empty() {
            String::new()
        } else {
            format!(" · disk {}%", self.disk_pct)
        };
        let load = format!(" · load {:.1}", self.load_1);
        let alerts: String = self.alerts.iter().map(|a| format!(" ⚠ {}", a)).collect();
        format!("{}{}{}{}", mem, disk, load, alerts)
    }

    /// Multi-word context line injected into the LLM system prompt.
    pub fn context_for_llm(&self) -> String {
        if self.mem_total_gb == 0.0 {
            return String::new();
        }
        let mut parts = vec![format!(
            "memory {:.1}GB/{:.1}GB ({}%)",
            self.mem_used_gb, self.mem_total_gb, self.mem_pct
        )];
        if !self.disk_mount.is_empty() {
            parts.push(format!("disk {} {}% used", self.disk_mount, self.disk_pct));
        }
        parts.push(format!("load avg {:.1}", self.load_1));
        if !self.alerts.is_empty() {
            parts.push(format!("ALERTS: {}", self.alerts.join(", ")));
        }
        parts.join(", ")
    }
}

// ── Public API ─────────────────────────────────────────────────────────────────

/// Start the background monitor. Returns a shared state handle.
/// The first poll happens immediately; subsequent polls every 5s.
pub fn start_monitor(is_local: bool) -> Arc<RwLock<SysState>> {
    let state: Arc<RwLock<SysState>> = Arc::new(RwLock::new(SysState::default()));
    let state_bg = state.clone();

    if is_local {
        tokio::spawn(async move {
            // First poll immediately
            if let Ok(s) = collect_state().await {
                if let Ok(mut w) = state_bg.write() {
                    *w = s;
                }
            }
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Ok(s) = collect_state().await {
                    if let Ok(mut w) = state_bg.write() {
                        *w = s;
                    }
                }
            }
        });
    }

    state
}

// ── Collection ─────────────────────────────────────────────────────────────────

async fn collect_state() -> anyhow::Result<SysState> {
    let (mem_pct, mem_used_gb, mem_total_gb) = read_meminfo().await?;
    let (disk_pct, disk_mount) = read_df().await.unwrap_or((0, String::new()));
    let load_1 = read_loadavg().await.unwrap_or(0.0);

    let mut alerts = Vec::new();
    if mem_pct >= 90 {
        alerts.push(format!("memory {}%", mem_pct));
    }
    if disk_pct >= 90 {
        alerts.push(format!("{} disk {}%", disk_mount, disk_pct));
    }

    Ok(SysState {
        mem_pct,
        mem_used_gb,
        mem_total_gb,
        disk_pct,
        disk_mount,
        load_1,
        alerts,
    })
}

async fn read_meminfo() -> anyhow::Result<(u64, f64, f64)> {
    let text = tokio::fs::read_to_string("/proc/meminfo").await?;
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_kb(rest);
        }
    }

    if total_kb == 0 {
        anyhow::bail!("no MemTotal");
    }
    let used_kb = total_kb.saturating_sub(avail_kb);
    let pct = used_kb * 100 / total_kb;
    let to_gb = |kb: u64| kb as f64 / 1024.0 / 1024.0;
    Ok((pct, to_gb(used_kb), to_gb(total_kb)))
}

async fn read_df() -> anyhow::Result<(u64, String)> {
    let out = tokio::process::Command::new("df")
        .args(["-P", "-x", "tmpfs", "-x", "devtmpfs", "-x", "squashfs"])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut worst_pct = 0u64;
    let mut worst_mount = String::new();

    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let mount = parts[5];
        // Skip irrelevant mounts
        if mount.starts_with("/snap") || mount.starts_with("/boot/efi") || mount.starts_with("/run")
        {
            continue;
        }
        let pct: u64 = parts[4].trim_end_matches('%').parse().unwrap_or(0);
        if pct > worst_pct {
            worst_pct = pct;
            worst_mount = mount.to_string();
        }
    }

    Ok((worst_pct, worst_mount))
}

async fn read_loadavg() -> anyhow::Result<f64> {
    let text = tokio::fs::read_to_string("/proc/loadavg").await?;
    let first = text.split_whitespace().next().unwrap_or("0");
    Ok(first.parse().unwrap_or(0.0))
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

// ── DOING animation ────────────────────────────────────────────────────────────

/// Generate the "DOING" frame: a single uppercase letter scans from right to left.
///
/// Frame 0: `d o i n G`   (rightmost lit)
/// Frame 1: `d o i N g`
/// Frame 2: `d o I n g`
/// Frame 3: `d O i n g`
/// Frame 4: `D o i n g`   (leftmost lit)
/// then repeats
pub fn doing_frame(frame: usize) -> String {
    const WORD: &[u8] = b"doing";
    let len = WORD.len();
    let bright_pos = len - 1 - (frame % len); // scans right to left
    let s: String = WORD
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if i == bright_pos {
                (c as char).to_uppercase().next().unwrap_or(c as char)
            } else {
                c as char
            }
        })
        .collect();
    s
}

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Local host identity and uptime, read from the OS rather than fetched
/// through an action.
///
/// Presentation and telemetry must not drive the model-callable vocabulary.
/// These fields used to come from a `SystemInfo` action; when that action was
/// removed the callers kept asking for it and kept getting `unknown variant`,
/// so the hostname, the remote telemetry sync and the System Pulse header all
/// degraded silently. Reading `/proc` and `uname` here keeps the feature
/// working without the trusted vocabulary growing a presentation concern.
pub struct HostSummary {
    pub hostname: String,
    pub os: String,
    pub uptime_seconds: u64,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
}

impl HostSummary {
    pub fn mem_pct(&self) -> u64 {
        (self.mem_used_mb * 100)
            .checked_div(self.mem_total_mb)
            .unwrap_or_default()
    }
}

pub async fn read_host_summary() -> HostSummary {
    let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let os = tokio::fs::read_to_string("/proc/sys/kernel/osrelease")
        .await
        .map(|s| format!("Linux {}", s.trim()))
        .unwrap_or_else(|_| std::env::consts::OS.to_string());
    let uptime_seconds = tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|t| {
            t.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|f| f as u64)
        .unwrap_or(0);
    let (_, used_gb, total_gb) = read_meminfo().await.unwrap_or((0, 0.0, 0.0));
    HostSummary {
        hostname,
        os,
        uptime_seconds,
        mem_used_mb: (used_gb * 1024.0) as u64,
        mem_total_mb: (total_gb * 1024.0) as u64,
    }
}
