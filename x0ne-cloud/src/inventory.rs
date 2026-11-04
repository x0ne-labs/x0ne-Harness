//! Typed inventory of billable cloud resources, parsed from `gcloud` output.
//!
//! Disks are modelled as a distinct kind because a *stopped* VM still bills
//! its boot disk — omitting disks is the classic surprise-bill hole.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Instance,
    /// A persistent disk, with what it takes to price one: GCP bills disks by
    /// provisioned capacity and type, per GiB-month, whether or not anything
    /// is attached to them.
    ///
    /// Measured 2026-08-22: nothing in this repository ever constructed a
    /// `Disk`. The kind was declared, documented ("omitting disks is the
    /// classic surprise-bill hole"), matched on by the price table — and had
    /// no producer, so the hole it names was still open. It carries its data
    /// now, and `parse_gcloud_disks` produces it.
    Disk {
        size_gb: u32,
        /// `pd-balanced`, `pd-ssd`, `pd-standard`, `pd-extreme`…
        disk_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accel {
    pub model: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudResource {
    pub name: String,
    pub kind: ResourceKind,
    pub machine_type: Option<String>,
    pub zone: String,
    pub status: String,
    pub accelerators: Vec<Accel>,
    pub created_iso: String,
}

/// Last path segment of a gcloud self-link URL (`.../machineTypes/a3-...` →
/// `a3-...`). gcloud returns full resource URLs; we only want the leaf.
fn leaf(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// Parse `gcloud compute instances list --format=json`. Returns empty on any
/// parse failure — a monitor that panics on a gcloud quirk is worse than one
/// that shows nothing for one tick.
pub fn parse_gcloud_instances(json: &str) -> Vec<CloudResource> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let accelerators = v
                .get("guestAccelerators")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|g| {
                            Some(Accel {
                                model: leaf(g.get("acceleratorType")?.as_str()?).to_string(),
                                count: g.get("acceleratorCount")?.as_u64()? as u32,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(CloudResource {
                name,
                kind: ResourceKind::Instance,
                machine_type: v
                    .get("machineType")
                    .and_then(|m| m.as_str())
                    .map(|s| leaf(s).to_string()),
                zone: v
                    .get("zone")
                    .and_then(|z| z.as_str())
                    .map(|s| leaf(s).to_string())
                    .unwrap_or_default(),
                status: v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string(),
                accelerators,
                created_iso: v
                    .get("creationTimestamp")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// Parse `gcloud compute disks list --format=json`.
///
/// Same failure discipline as the instance parser: empty on any surprise. A
/// monitor that panics on a gcloud quirk is worse than one that shows nothing
/// for one tick — but note the asymmetry that makes this safe: showing
/// nothing UNDERSTATES, so the caller must render a parse failure, not a
/// zero. `spend::report` keeps the distinction.
pub fn parse_gcloud_disks(json: &str) -> Vec<CloudResource> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            Some(CloudResource {
                name: v.get("name")?.as_str()?.to_string(),
                kind: ResourceKind::Disk {
                    // gcloud renders sizeGb as a STRING ("50"), not a number.
                    size_gb: v.get("sizeGb").and_then(|g| {
                        g.as_u64()
                            .or_else(|| g.as_str().and_then(|s| s.parse().ok()))
                    })? as u32,
                    disk_type: leaf(v.get("type").and_then(|t| t.as_str())?).to_string(),
                },
                machine_type: None,
                zone: v
                    .get("zone")
                    .and_then(|z| z.as_str())
                    .map(|s| leaf(s).to_string())
                    .unwrap_or_default(),
                status: v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string(),
                accelerators: vec![],
                created_iso: v
                    .get("creationTimestamp")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but real-shaped `gcloud compute instances list --format=json`
    // element: one running H100 VM.
    const SAMPLE: &str = r#"[
      {
        "name": "gpu-h100",
        "machineType": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a/machineTypes/a3-highgpu-1g",
        "zone": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a",
        "status": "RUNNING",
        "creationTimestamp": "2026-08-10T09:00:00.000-07:00",
        "guestAccelerators": [
          { "acceleratorType": "https://.../acceleratorTypes/nvidia-h100-80gb", "acceleratorCount": 1 }
        ]
      }
    ]"#;

    #[test]
    fn parses_one_running_h100_instance() {
        let out = parse_gcloud_instances(SAMPLE);
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.name, "gpu-h100");
        assert_eq!(r.kind, ResourceKind::Instance);
        assert_eq!(r.machine_type.as_deref(), Some("a3-highgpu-1g"));
        assert_eq!(r.zone, "us-central1-a");
        assert_eq!(r.status, "RUNNING");
        assert_eq!(
            r.accelerators,
            vec![Accel {
                model: "nvidia-h100-80gb".into(),
                count: 1
            }]
        );
    }

    #[test]
    fn malformed_json_yields_empty_not_panic() {
        assert!(parse_gcloud_instances("not json").is_empty());
        assert!(parse_gcloud_instances("").is_empty());
    }

    #[test]
    fn empty_array_is_empty() {
        // `gcloud … list` on a project with no instances returns `[]`.
        assert!(parse_gcloud_instances("[]").is_empty());
    }

    #[test]
    fn non_array_top_level_yields_empty() {
        // A single object (or an error object) instead of a list must not
        // panic or half-parse.
        assert!(parse_gcloud_instances(r#"{"name":"x"}"#).is_empty());
        assert!(parse_gcloud_instances("42").is_empty());
    }

    #[test]
    fn instance_without_accelerators_parses_with_empty_vec() {
        // A CPU-only VM has no guestAccelerators key at all.
        let json = r#"[{
            "name": "web",
            "machineType": "https://x/zones/us-central1-a/machineTypes/c3-standard-4",
            "zone": "https://x/zones/us-central1-a",
            "status": "RUNNING",
            "creationTimestamp": "2026-08-10T09:00:00Z"
        }]"#;
        let out = parse_gcloud_instances(json);
        assert_eq!(out.len(), 1);
        assert!(out[0].accelerators.is_empty());
        assert_eq!(out[0].machine_type.as_deref(), Some("c3-standard-4"));
    }

    #[test]
    fn entry_missing_name_is_skipped_not_defaulted() {
        // name is the only field we hard-require (it's the resource id and
        // what a stop command needs). An entry without it is dropped, not
        // given a blank name that a later `gcloud stop ""` could act on.
        let json = r#"[
            { "status": "RUNNING", "zone": "https://x/zones/z/z" },
            { "name": "real", "zone": "https://x/zones/us-central1-a", "status": "RUNNING", "creationTimestamp": "t" }
        ]"#;
        let out = parse_gcloud_instances(json);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "real");
    }

    #[test]
    fn missing_optional_fields_get_safe_defaults() {
        // Only name present: machine_type None (→ unpriced), status UNKNOWN
        // (not a crash), zone/created empty strings.
        let out = parse_gcloud_instances(r#"[{"name":"bare"}]"#);
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.name, "bare");
        assert_eq!(r.machine_type, None);
        assert_eq!(r.status, "UNKNOWN");
        assert_eq!(r.zone, "");
        assert_eq!(r.created_iso, "");
        assert!(r.accelerators.is_empty());
    }

    #[test]
    fn accelerator_missing_a_field_is_dropped_not_zeroed() {
        // A guestAccelerators entry without a count would otherwise parse as
        // count 0 and silently vanish from the cost. Drop the malformed entry
        // instead — better a shorter list than a phantom free GPU.
        let json = r#"[{
            "name": "h100-vm",
            "machineType": "https://x/zones/z/machineTypes/a3-highgpu-1g",
            "zone": "https://x/zones/us-central1-a",
            "status": "RUNNING",
            "creationTimestamp": "t",
            "guestAccelerators": [
                { "acceleratorType": "https://x/acceleratorTypes/nvidia-h100-80gb" }
            ]
        }]"#;
        let out = parse_gcloud_instances(json);
        assert_eq!(out.len(), 1);
        assert!(out[0].accelerators.is_empty());
    }

    #[test]
    fn leaf_handles_a_bare_name_without_slashes() {
        // gcloud usually returns full URLs, but a plain machine type should
        // pass through untouched rather than being mangled.
        let json = r#"[{"name":"x","machineType":"a3-highgpu-1g","zone":"us-central1-a","status":"RUNNING","creationTimestamp":"t"}]"#;
        let out = parse_gcloud_instances(json);
        assert_eq!(out[0].machine_type.as_deref(), Some("a3-highgpu-1g"));
        assert_eq!(out[0].zone, "us-central1-a");
    }

    /// The real shape, captured from `gcloud compute disks list --format=json`
    /// on 2026-08-22 against the project's own disk. Two details that a
    /// hand-written fixture would have got wrong, and both cost money:
    /// `sizeGb` is a JSON **string**, and `creationTimestamp` carries a local
    /// offset rather than `Z`.
    #[test]
    fn a_real_gcloud_disk_is_parsed_with_its_size_and_type() {
        const REAL: &str = r#"[
          {
            "creationTimestamp": "2026-08-21T02:39:29.893-07:00",
            "name": "x0ne-infer-m2",
            "sizeGb": "50",
            "status": "READY",
            "type": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a/diskTypes/pd-balanced",
            "zone": "https://www.googleapis.com/compute/v1/projects/p/zones/us-central1-a"
          }
        ]"#;
        let disks = parse_gcloud_disks(REAL);
        assert_eq!(disks.len(), 1);
        let d = &disks[0];
        assert_eq!(d.name, "x0ne-infer-m2");
        assert_eq!(d.zone, "us-central1-a");
        assert_eq!(
            d.kind,
            ResourceKind::Disk {
                size_gb: 50,
                disk_type: "pd-balanced".into()
            }
        );
    }

    /// A disk row missing what it takes to price it is DROPPED, not invented
    /// with a default. A disk of size 0 or of unknown type would be priced at
    /// zero and vanish from the bill — the failure mode this whole module
    /// exists to prevent.
    #[test]
    fn a_disk_without_a_size_or_a_type_is_dropped_rather_than_defaulted() {
        assert!(parse_gcloud_disks(r#"[{"name":"x","type":"a/pd-ssd"}]"#).is_empty());
        assert!(parse_gcloud_disks(r#"[{"name":"x","sizeGb":"10"}]"#).is_empty());
        assert!(parse_gcloud_disks("not json").is_empty());
    }
}
