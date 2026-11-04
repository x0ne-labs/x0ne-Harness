//! nvidia-smi parsing: GPU state joined with the attached compute processes.
//!
//! The process list is what separates an idle job (0% but a process is
//! attached — a job between steps) from waste (0% and nothing attached).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuProc {
    pub pid: u32,
    pub name: String,
    pub mem_mb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuState {
    pub index: u32,
    pub name: String,
    pub util_pct: u32,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub temp_c: u32,
    pub power_w: u32,
    pub processes: Vec<GpuProc>,
}

/// Parse the two `nvidia-smi --format=csv,noheader,nounits` outputs, joined by
/// GPU index.
///
/// `gpu_csv` columns: index,name,utilization.gpu,memory.used,memory.total,
/// temperature.gpu,power.draw
/// `apps_csv` columns: pid,process_name,used_memory  (no GPU index column in
/// older nvidia-smi; attach every process to every GPU when index is absent —
/// exact on a machine with a single GPU).
pub fn parse_nvidia_smi(gpu_csv: &str, apps_csv: &str) -> Vec<GpuState> {
    let procs: Vec<GpuProc> = apps_csv
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if f.len() < 3 {
                return None;
            }
            Some(GpuProc {
                pid: f[0].parse().ok()?,
                name: f[1].to_string(),
                mem_mb: f[2].parse().ok()?,
            })
        })
        .collect();

    gpu_csv
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if f.len() < 7 {
                return None;
            }
            // Identity and utilization are STRICT: index is the GPU's id, and
            // util drives the idle/waste verdict — a bogus 0 there would trip
            // a false waste alert, so if util is `[N/A]` we drop the whole
            // line (the caller then reports "telemetry restricted" rather
            // than a misleading idle GPU).
            //
            // The remaining fields are DISPLAY-only and frequently `[N/A]` on
            // real hardware (power.draw especially, and mem/temp under
            // restricted telemetry). An unparseable display field defaults to
            // 0 rather than deleting a GPU that is genuinely present and
            // reporting its utilization.
            Some(GpuState {
                index: f[0].parse().ok()?,
                name: f[1].to_string(),
                util_pct: f[2].parse().ok()?,
                mem_used_mb: f[3].parse().unwrap_or(0),
                mem_total_mb: f[4].parse().unwrap_or(0),
                temp_c: f[5].parse().unwrap_or(0),
                power_w: f[6].parse::<f64>().map(|w| w as u32).unwrap_or(0),
                processes: procs.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`
    const GPU_CSV: &str = "0, NVIDIA H100 80GB HBM3, 87, 41230, 81559, 71, 402\n";
    // `nvidia-smi --query-compute-apps=... --format=csv,noheader,nounits`
    const APPS_CSV: &str = "31245, python, 41000\n";

    #[test]
    fn parses_gpu_with_attached_process() {
        let out = parse_nvidia_smi(GPU_CSV, APPS_CSV);
        assert_eq!(out.len(), 1);
        let g = &out[0];
        assert_eq!(g.index, 0);
        assert!(g.name.contains("H100"));
        assert_eq!(g.util_pct, 87);
        assert_eq!(g.mem_used_mb, 41230);
        assert_eq!(g.temp_c, 71);
        assert_eq!(g.power_w, 402);
        assert_eq!(g.processes.len(), 1);
        assert_eq!(g.processes[0].pid, 31245);
    }

    #[test]
    fn idle_gpu_has_no_processes() {
        let out = parse_nvidia_smi("0, NVIDIA H100 80GB HBM3, 0, 512, 81559, 33, 71\n", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].util_pct, 0);
        assert!(out[0].processes.is_empty());
    }

    #[test]
    fn garbage_lines_are_skipped_not_panicked() {
        assert!(parse_nvidia_smi("garbage without commas", "").is_empty());
    }

    #[test]
    fn na_power_keeps_the_gpu_with_power_zero() {
        // Many real cards report power.draw as [N/A]. The GPU must still
        // appear with its util/temp — losing it would hide a running GPU.
        let out = parse_nvidia_smi("0, NVIDIA RTX 5090, 42, 1000, 32607, 55, [N/A]\n", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].util_pct, 42);
        assert_eq!(out[0].temp_c, 55);
        assert_eq!(out[0].power_w, 0);
    }

    #[test]
    fn na_utilization_drops_the_line() {
        // util drives the waste verdict; a bogus 0 would trip a false alert.
        // Restricted telemetry → drop → caller reports "restricted".
        let out = parse_nvidia_smi("0, H100, [N/A], 1000, 81559, [N/A], [N/A]\n", "");
        assert!(out.is_empty());
    }

    #[test]
    fn parses_multiple_gpus_and_attaches_all_processes() {
        let gpu = "0, H100, 90, 40000, 81559, 70, 400\n1, H100, 10, 2000, 81559, 45, 90\n";
        let apps = "111, train, 40000\n222, infer, 2000\n";
        let out = parse_nvidia_smi(gpu, apps);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].index, 0);
        assert_eq!(out[1].index, 1);
        // Older nvidia-smi has no GPU-index column in compute-apps, so we
        // attach every process to every GPU (exact on a single-GPU box; a
        // known over-attach on multi-GPU, documented in parse_nvidia_smi).
        assert_eq!(out[0].processes.len(), 2);
        assert_eq!(out[1].processes.len(), 2);
    }

    #[test]
    fn decimal_utilization_or_extra_whitespace_still_parses() {
        // nounits still yields ints for util, but power is a float; and some
        // locales pad columns. Trimming + float-parse must cope.
        let out = parse_nvidia_smi("  0 ,  H100 , 87 , 41230 , 81559 , 71 , 402.5 \n", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].power_w, 402);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse_nvidia_smi("", "").is_empty());
        assert!(parse_nvidia_smi("\n\n", "").is_empty());
    }

    #[test]
    fn malformed_process_line_is_skipped_gpu_still_parses() {
        // A junk apps line must not drop the GPU or the good processes.
        let out = parse_nvidia_smi(
            "0, H100, 50, 1000, 81559, 60, 300\n",
            "junk\n333, python, 1000\n",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].processes.len(), 1);
        assert_eq!(out[0].processes[0].pid, 333);
    }
}
