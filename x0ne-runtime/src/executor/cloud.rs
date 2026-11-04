//! Cloud cost + GPU status handlers.
//!
//! Both spawn a CLI (`gcloud` / `nvidia-smi`), which is why the actions are
//! classified `ProcessSpawn` and gated by `policy.allowed_commands` — the same
//! gate as `RunCommand`. The parsing and the money live in the pure
//! `x0ne-cloud` crate; this module only does the I/O and the assembly.
//!
//! Deliberately absent: any action that STOPS a VM. Stopping goes through the
//! existing `RunCommand gcloud compute instances stop …`, which already
//! crosses the human confirmation gate. One canonical execution boundary.

use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// The single CLI a provider needs on PATH. Only gcp is implemented; an
/// unknown provider returns None and the handler rejects it.
pub(crate) fn provider_binary(provider: &str) -> Option<&'static str> {
    match provider {
        "gcp" => Some("gcloud"),
        _ => None,
    }
}

/// Reject before spawning if `bin` is not allowlisted. These handlers are
/// ProcessSpawn, so the same `allowed_commands` gate as RunCommand applies; we
/// check it explicitly to give a clear message instead of an opaque ENOENT.
fn ensure_allowed(
    action_name: &'static str,
    bin: &str,
    config: &AppConfig,
) -> Option<ActionResult> {
    if config.policy.allowed_commands.iter().any(|c| c == bin) {
        None
    } else {
        Some(ActionResult::err(
            action_name,
            ErrorKind::PolicyDenied,
            format!("`{bin}` is not in policy.allowed_commands. Add it to enable monitoring."),
            0,
        ))
    }
}

/// Serialize a waste verdict for the action payload, so the model can read it
/// and propose the stop itself.
pub(crate) fn waste_json(v: &x0ne_cloud::waste::WasteVerdict) -> serde_json::Value {
    json!({
        "is_wasting": v.is_wasting,
        "reason": v.reason,
        "hourly_usd": v.hourly_usd,
    })
}

pub async fn cloud_cost(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::CloudCost { provider, project } = action else {
        unreachable!()
    };
    let Some(bin) = provider_binary(provider) else {
        return ActionResult::err(
            "CloudCost",
            ErrorKind::InvalidInput,
            format!("unsupported provider '{provider}' (only 'gcp')"),
            0,
        );
    };
    if let Some(denied) = ensure_allowed("CloudCost", bin, config) {
        return denied;
    }

    let mut args = vec![
        "compute".to_string(),
        "instances".to_string(),
        "list".to_string(),
        "--format=json".to_string(),
    ];
    if let Some(p) = project {
        args.push(format!("--project={p}"));
    }

    let cap = match crate::executor::process::spawn_capture(
        bin,
        &args,
        &None,
        &HashMap::new(),
        Duration::from_secs(30),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            return ActionResult::err("CloudCost", ErrorKind::ExecutionFailed, e.to_string(), 0)
        }
    };
    if cap.exit_code != 0 {
        return ActionResult::err(
            "CloudCost",
            ErrorKind::ExecutionFailed,
            format!("gcloud exited {}: {}", cap.exit_code, cap.stderr),
            0,
        );
    }

    let resources = x0ne_cloud::inventory::parse_gcloud_instances(&cap.stdout);
    let table = x0ne_cloud::pricing::PriceTable::current();
    let burn = x0ne_cloud::burn::compute_burn(&resources, &table);

    // When nvidia-smi is also allowlisted, attach the local GPU snapshot and
    // an instantaneous waste verdict. `idle_secs = 0` on purpose: a single
    // snapshot cannot prove SUSTAINED idleness, so this field can only say
    // `is_wasting: false` with the current util/process facts — the
    // sustained-idle alert (with real timing) is the monitor's job. The model
    // still gets everything it needs to reason: util, processes, $/h.
    let waste = if config
        .policy
        .allowed_commands
        .iter()
        .any(|c| c == "nvidia-smi")
    {
        let gpu_args = vec![
            "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw"
                .to_string(),
            "--format=csv,noheader,nounits".to_string(),
        ];
        let gpu_csv = crate::executor::process::spawn_capture(
            "nvidia-smi",
            &gpu_args,
            &None,
            &HashMap::new(),
            Duration::from_secs(10),
        )
        .await
        .map(|c| c.stdout)
        .unwrap_or_default();
        let gpus = x0ne_cloud::gpu::parse_nvidia_smi(&gpu_csv, "");
        let hourly = burn.priced.first().map(|p| p.hourly_usd);
        gpus.first()
            .map(|g| waste_json(&x0ne_cloud::waste::assess_waste(g, hourly, 0, u64::MAX)))
    } else {
        None
    };

    ActionResult::ok(
        "CloudCost",
        ActionOutput::Generic {
            data: json!({
                "provider": provider,
                "pricing_as_of": table.as_of,
                "estimated": true,
                "hourly_usd": burn.hourly_usd,
                "per_day_usd": burn.per_day_usd,
                "priced": burn.priced,
                "unpriced": burn.unpriced,
                "waste": waste,
            }),
        },
        0,
    )
}

pub async fn gpu_status(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::GpuStatus { host } = action else {
        unreachable!()
    };

    // Local vs remote: this handler owns local. Remote runs through the
    // existing SshExec path so there is exactly one SSH implementation.
    if host.is_some() {
        return ActionResult::err(
            "GpuStatus",
            ErrorKind::InvalidInput,
            "remote GpuStatus runs via SshExec: call SshExec with command \
             `nvidia-smi` and the same --query-gpu/--format args",
            0,
        );
    }
    if let Some(denied) = ensure_allowed("GpuStatus", "nvidia-smi", config) {
        return denied;
    }

    let gpu_args = vec![
        "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw"
            .to_string(),
        "--format=csv,noheader,nounits".to_string(),
    ];
    let apps_args = vec![
        "--query-compute-apps=pid,process_name,used_memory".to_string(),
        "--format=csv,noheader,nounits".to_string(),
    ];
    let gpu_cap = crate::executor::process::spawn_capture(
        "nvidia-smi",
        &gpu_args,
        &None,
        &HashMap::new(),
        Duration::from_secs(10),
    )
    .await;
    let apps_cap = crate::executor::process::spawn_capture(
        "nvidia-smi",
        &apps_args,
        &None,
        &HashMap::new(),
        Duration::from_secs(10),
    )
    .await;
    let gpu_csv = gpu_cap.map(|c| c.stdout).unwrap_or_default();
    let apps_csv = apps_cap.map(|c| c.stdout).unwrap_or_default();

    let gpus = x0ne_cloud::gpu::parse_nvidia_smi(&gpu_csv, &apps_csv);
    // An empty list with non-empty output means nvidia-smi answered in a shape
    // we do not understand — say so rather than reporting "no GPUs", which
    // would read as "nothing is running" when the truth is "we cannot tell".
    // Confidential-computing mode may restrict telemetry this way.
    let note = if gpus.is_empty() && !gpu_csv.trim().is_empty() {
        "nvidia-smi returned output this build could not parse (telemetry may be restricted)"
    } else {
        ""
    };

    ActionResult::ok(
        "GpuStatus",
        ActionOutput::Generic {
            data: json!({ "host": "local", "gpus": gpus, "note": note }),
        },
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn cloud_cost_denied_without_gcloud_in_allowlist() {
        // Default config has an empty allowed_commands = deny-all. The handler
        // must refuse before spawning, with a message naming gcloud.
        let cfg = AppConfig::defaults();
        let action = Action::CloudCost {
            provider: "gcp".into(),
            project: None,
        };
        let res = cloud_cost(&action, &cfg).await;
        assert!(!res.success);
        let msg = res.error.as_ref().unwrap().message.to_lowercase();
        assert!(msg.contains("gcloud"), "got: {msg}");
    }

    #[tokio::test]
    async fn gpu_status_denied_without_nvidia_smi_in_allowlist() {
        let cfg = AppConfig::defaults();
        let action = Action::GpuStatus { host: None };
        let res = gpu_status(&action, &cfg).await;
        assert!(!res.success);
        let err = res.error.as_ref().unwrap();
        assert!(err.message.to_lowercase().contains("nvidia-smi"));
        assert_eq!(err.action_name, "GpuStatus");
    }

    #[tokio::test]
    async fn remote_gpu_status_points_at_ssh_exec() {
        let cfg = AppConfig::defaults();
        let action = Action::GpuStatus {
            host: Some("gpu-h100".into()),
        };
        let res = gpu_status(&action, &cfg).await;
        assert!(!res.success);
        assert!(res.error.as_ref().unwrap().message.contains("SshExec"));
    }

    #[test]
    fn unknown_provider_is_rejected() {
        assert!(provider_binary("aws").is_none());
        assert_eq!(provider_binary("gcp"), Some("gcloud"));
    }

    #[tokio::test]
    async fn cloud_cost_rejects_unsupported_provider_before_touching_policy() {
        // An unknown provider must fail as InvalidInput regardless of the
        // allowlist — we never spawn for a provider we do not implement.
        let cfg = AppConfig::defaults();
        let action = Action::CloudCost {
            provider: "aws".into(),
            project: None,
        };
        let res = cloud_cost(&action, &cfg).await;
        assert!(!res.success);
        let err = res.error.as_ref().unwrap();
        assert_eq!(err.kind, crate::result::ErrorKind::InvalidInput);
        assert!(err.message.contains("aws"));
    }

    #[test]
    fn cloud_cost_output_carries_waste_field() {
        use x0ne_cloud::gpu::GpuState;
        let gpu = GpuState {
            index: 0,
            name: "H100".into(),
            util_pct: 0,
            mem_used_mb: 0,
            mem_total_mb: 81559,
            temp_c: 33,
            power_w: 70,
            processes: vec![],
        };
        let v = x0ne_cloud::waste::assess_waste(&gpu, Some(11.26), 25 * 60, 20 * 60);
        let data = waste_json(&v);
        assert_eq!(data["is_wasting"], true);
        assert!(data["reason"].as_str().unwrap().contains("idle"));
    }
}
