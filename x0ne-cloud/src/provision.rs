//! What to create, as data — and the `gcloud` argv that creates it.
//!
//! Everything here is pure. A [`VmSpec`] goes in, a `Vec<String>` of arguments
//! comes out, and nothing is spawned: the impure half is a `ProcessSpawn`
//! action that routes through the human gate like any other. That split is the
//! point. Building the command is where the mistakes live — a wrong zone, a
//! missing `--maintenance-policy`, an accelerator the machine type cannot take
//! — and mistakes that cost $11.26 an hour should be catchable by a unit test
//! rather than by a bill.
//!
//! # The contract this module has to keep
//!
//! The program spec names one cross-subsystem contract and asks for it early:
//! the image and confidential flags a `VmSpec` requests are the same facts
//! attestation will later verify. A provisioner that quietly boots a different
//! image than the verifier expects produces a VM that cannot be attested, and
//! the failure surfaces two subsystems away from its cause. So [`VmSpec`]
//! carries the image explicitly, never a default filled in by `gcloud`.
//!
//! # Sources for the constraints encoded below
//!
//! Confidential VMs with an H100 are narrower than confidential VMs in
//! general, and the narrowing is not obvious from the flag names:
//!
//! - only `a3-highgpu-1g`, and only **one** H100 80GB;
//! - only three zones: `us-central1-a`, `us-east5-a`, `europe-west4-c`;
//! - **only SPOT or flex-start provisioning** — an on-demand confidential A3
//!   is refused, which is the constraint most likely to be missed because it
//!   has nothing to do with confidentiality;
//! - GPU confidential mode is enabled *after* the instance exists, so a spec
//!   that requests it is describing an intent the create call cannot fulfil on
//!   its own.
//!
//! (Google Cloud, "Create a Confidential VM instance with GPU", read
//! 2026-08-13.)

use serde::{Deserialize, Serialize};

/// CPU trusted-execution technology to request.
///
/// Named rather than boolean because the attestation evidence differs per
/// technology, and `confidential: true` would leave the verifier guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidentialTee {
    /// Intel TDX. Required for the H100 confidential configuration.
    Tdx,
    /// AMD SEV-SNP. Available on other machine families.
    SevSnp,
}

impl ConfidentialTee {
    /// The value `--confidential-compute-type` expects.
    pub fn flag_value(self) -> &'static str {
        match self {
            Self::Tdx => "TDX",
            Self::SevSnp => "SEV_SNP",
        }
    }
}

/// How the instance is provisioned, which the confidential-GPU configuration
/// constrains rather than merely prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvisioningModel {
    /// Ordinary on-demand. Refused for a confidential A3.
    Standard,
    /// Preemptible. Required for a confidential A3, and cheaper everywhere.
    Spot,
}

/// One accelerator group attached to the instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    /// `gcloud` type name, e.g. `nvidia-h100-80gb`.
    pub model: String,
    pub count: u32,
}

/// A machine to create, described completely enough to be reproducible.
///
/// "Completely" is the operative word: every field that affects the
/// attestation measurement is explicit here, so two runs of the same spec
/// produce two instances a verifier cannot tell apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    pub machine_type: String,
    pub zone: String,
    /// Image family and its project. Explicit because a floating default is
    /// exactly what breaks the measurement contract with attestation.
    pub image_family: String,
    pub image_project: String,
    pub boot_disk_gb: u32,
    /// `None` means an ordinary VM. The whole point of this program is that
    /// it is usually `Some`, but "not confidential" must remain expressible —
    /// a cheap non-confidential VM is the right control when something fails
    /// and the question is whether confidentiality is the cause.
    pub confidential: Option<ConfidentialTee>,
    pub accelerators: Vec<Accelerator>,
    pub provisioning_model: ProvisioningModel,
    /// GCE stops the instance on its own after this long, whether or not x0ne
    /// is still alive to do it. A `kill -9`, a sleeping laptop or a dropped
    /// SSH session used to leave a GPU billing with no end. A spec saved
    /// before the field existed reads back with the H100 cap, not without one.
    #[serde(default = "default_max_run_minutes")]
    pub max_run_minutes: u32,
}

fn default_max_run_minutes() -> u32 {
    120
}

/// Upper bound on [`VmSpec::max_run_minutes`]: a spec that asks for more is
/// refused, so the self-stop cannot be pushed out of reach (GCE accepts 120 days).
pub const MAX_RUN_CAP_MINUTES: u32 = 24 * 60;

/// Machine types that come with a Local SSD whether or not the create asks
/// for one: `a3-highgpu-*` (measured 2026-08-14, see `to_stop_argv`) and the
/// `-lssd` variants. gcloud refuses the auto-stop's Local SSD flag on any other.
fn bundles_local_ssd(machine_type: &str) -> bool {
    machine_type.starts_with("a3-highgpu-") || machine_type.ends_with("-lssd")
}

/// A reason a spec cannot be created, found before spending anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecRejected {
    /// Confidential H100 exists in three zones only.
    ZoneDoesNotSupportConfidentialGpu { zone: String },
    /// Confidential H100 exists on one machine type only.
    MachineTypeDoesNotSupportConfidentialGpu { machine_type: String },
    /// One GPU, no more.
    TooManyConfidentialGpus { count: u32 },
    /// On-demand confidential A3 is refused by the API.
    ConfidentialGpuRequiresSpot,
    /// A name `gcloud` will not accept, caught here rather than after the
    /// round trip.
    InvalidName { name: String },
    /// TDX exists on specific machine series only. The owner hit this live in
    /// the console on 2026-08-13 — "Intel TDX is not compatible with the
    /// selected machine series" — which is precisely the kind of
    /// round-trip this validator exists to spare.
    TeeNotSupportedOnMachineSeries {
        tee: ConfidentialTee,
        machine_type: String,
    },
    /// No self-stop, or one too far away to protect anything.
    MaxRunOutOfRange { minutes: u32 },
}

impl SpecRejected {
    /// What the operator is told. Each says what is wrong and what to change,
    /// because a rejection that only says "invalid" costs a round of guessing.
    pub fn message(&self) -> String {
        match self {
            Self::ZoneDoesNotSupportConfidentialGpu { zone } => format!(
                "Confidential H100 is available in {} only — '{zone}' is not one of them.",
                CONFIDENTIAL_GPU_ZONES.join(", ")
            ),
            Self::MachineTypeDoesNotSupportConfidentialGpu { machine_type } => format!(
                "Confidential H100 requires machine type {CONFIDENTIAL_GPU_MACHINE_TYPE}, \
                 not '{machine_type}'."
            ),
            Self::TooManyConfidentialGpus { count } => {
                format!("Confidential H100 supports exactly 1 GPU, not {count}.")
            }
            Self::ConfidentialGpuRequiresSpot => {
                "A confidential A3 instance must use SPOT provisioning; on-demand is refused \
                 by the API. Set provisioning_model = Spot."
                    .to_string()
            }
            Self::InvalidName { name } => format!(
                "'{name}' is not a valid instance name: lowercase letters, digits and hyphens, \
                 starting with a letter, 63 characters at most."
            ),
            Self::TeeNotSupportedOnMachineSeries { tee, machine_type } => format!(
                "{:?} is not available on '{machine_type}' — TDX runs on the C3 series \
                 (c3-*) and on {CONFIDENTIAL_GPU_MACHINE_TYPE}; SEV-SNP runs on N2D/C2D/C3D.",
                tee
            ),
            Self::MaxRunOutOfRange { minutes } => format!(
                "max_run_minutes is {minutes}: it must be between 1 and {MAX_RUN_CAP_MINUTES}, \
                 so that GCE stops the instance even if x0ne is no longer there to."
            ),
        }
    }
}

/// The three zones where a confidential H100 exists.
pub const CONFIDENTIAL_GPU_ZONES: &[&str] = &["us-central1-a", "us-east5-a", "europe-west4-c"];
/// The only machine type that takes a confidential H100.
pub const CONFIDENTIAL_GPU_MACHINE_TYPE: &str = "a3-highgpu-1g";

impl VmSpec {
    /// Check the spec against the constraints that would otherwise be
    /// discovered as an API error — or, worse, as a running instance in the
    /// wrong configuration.
    ///
    /// Runs before the human is asked to approve anything, so the prompt never
    /// shows a command that cannot succeed.
    pub fn validate(&self) -> Result<(), SpecRejected> {
        if !is_valid_instance_name(&self.name) {
            return Err(SpecRejected::InvalidName {
                name: self.name.clone(),
            });
        }
        if !(1..=MAX_RUN_CAP_MINUTES).contains(&self.max_run_minutes) {
            return Err(SpecRejected::MaxRunOutOfRange {
                minutes: self.max_run_minutes,
            });
        }

        // TEE ↔ machine-series compatibility, before anything else: the
        // console refuses this combination outright, so a spec carrying it can
        // never be created no matter what else is right.
        if let Some(tee) = self.confidential {
            let series_ok = match tee {
                ConfidentialTee::Tdx => {
                    self.machine_type.starts_with("c3-")
                        || self.machine_type == CONFIDENTIAL_GPU_MACHINE_TYPE
                }
                ConfidentialTee::SevSnp => {
                    self.machine_type.starts_with("n2d-")
                        || self.machine_type.starts_with("c2d-")
                        || self.machine_type.starts_with("c3d-")
                }
            };
            if !series_ok {
                return Err(SpecRejected::TeeNotSupportedOnMachineSeries {
                    tee,
                    machine_type: self.machine_type.clone(),
                });
            }
        }

        let gpu_count: u32 = self.accelerators.iter().map(|a| a.count).sum();
        let confidential_with_gpu = self.confidential.is_some() && gpu_count > 0;
        if !confidential_with_gpu {
            return Ok(());
        }

        // Everything below applies ONLY to the confidential-plus-GPU shape.
        // A confidential VM without a GPU (the cheap prover) and an ordinary
        // GPU VM are both unconstrained here, and conflating the three is how
        // a validator starts refusing valid work.
        if !CONFIDENTIAL_GPU_ZONES.contains(&self.zone.as_str()) {
            return Err(SpecRejected::ZoneDoesNotSupportConfidentialGpu {
                zone: self.zone.clone(),
            });
        }
        if self.machine_type != CONFIDENTIAL_GPU_MACHINE_TYPE {
            return Err(SpecRejected::MachineTypeDoesNotSupportConfidentialGpu {
                machine_type: self.machine_type.clone(),
            });
        }
        if gpu_count != 1 {
            return Err(SpecRejected::TooManyConfidentialGpus { count: gpu_count });
        }
        if self.provisioning_model != ProvisioningModel::Spot {
            return Err(SpecRejected::ConfidentialGpuRequiresSpot);
        }
        Ok(())
    }

    /// The `gcloud` argv that creates this instance.
    ///
    /// Returns argv, not a shell string, and that is deliberate: the
    /// `RunCommand` this feeds has no shell
    /// (`x0ne-runtime/src/executor/process.rs`), so a value containing a space
    /// is one argument rather than an injection point. Rendering to a string
    /// for display is the caller's business and is lossy by nature.
    ///
    /// Order is fixed so the output is diffable between runs and a test can
    /// pin it. Flags a human would read first come first.
    pub fn to_gcloud_argv(&self) -> Vec<String> {
        let mut v: Vec<String> = vec![
            "compute".into(),
            "instances".into(),
            "create".into(),
            self.name.clone(),
            format!("--zone={}", self.zone),
            format!("--machine-type={}", self.machine_type),
            format!("--image-family={}", self.image_family),
            format!("--image-project={}", self.image_project),
            format!("--boot-disk-size={}GB", self.boot_disk_gb),
        ];

        if let Some(tee) = self.confidential {
            v.push(format!("--confidential-compute-type={}", tee.flag_value()));
            // A confidential instance cannot live-migrate: the memory is
            // encrypted to a key the destination host does not have. Omitting
            // this is not a style choice — the API refuses the create.
            v.push("--maintenance-policy=TERMINATE".into());
            // Measured boot. Attestation reads what these produce, so a
            // confidential VM without them is one that cannot be verified;
            // they belong to the spec's contract, not to a hardening pass.
            v.push("--shielded-secure-boot".into());
            v.push("--shielded-vtpm".into());
            v.push("--shielded-integrity-monitoring".into());
        }

        for a in &self.accelerators {
            v.push(format!("--accelerator=type={},count={}", a.model, a.count));
        }

        if self.provisioning_model == ProvisioningModel::Spot {
            v.push("--provisioning-model=SPOT".into());
        }
        // One termination action for Spot preemption and the self-stop below,
        // written out rather than inherited. STOP keeps the boot disk that
        // resume-by-detection relies on (the DELETE behaviour belongs to the
        // legacy preemptible model); gcloud requires the flag with
        // --max-run-duration on a non-Spot instance (SDK 585.0.0 --help).
        v.push("--instance-termination-action=STOP".into());
        // SECURITY (money): GCE stops the instance by itself after this,
        // whether or not x0ne is alive to run `to_stop_argv`.
        v.push(format!("--max-run-duration={}m", self.max_run_minutes));
        if bundles_local_ssd(&self.machine_type) {
            // gcloud refuses the self-stop of a Local-SSD machine without it,
            // and refuses the flag on a machine without one (SDK 585.0.0 --help).
            v.push("--discard-local-ssds-at-termination-timestamp=true".into());
        }

        v
    }

    /// The command as a human reads it, for the approval prompt.
    ///
    /// Display only. Anything that consumes this to re-execute has lost the
    /// argv boundary and reintroduced the quoting bugs it was there to prevent.
    pub fn to_display_command(&self) -> String {
        format!("gcloud {}", self.to_gcloud_argv().join(" "))
    }

    /// The `gcloud` argv that STOPS this instance — the one lever the budget
    /// gate has, and the only thing that turns the meter off.
    ///
    /// SECURITY (money): `--discard-local-ssd=true` is not a tuning knob.
    /// Measured in production on 2026-08-14, first real `--machine full` run:
    ///
    /// ```text
    /// $ gcloud compute instances stop x0ne-cc --zone=us-central1-a
    /// HTTPError 400: VM has a Local SSD attached but an undefined value for
    /// discard-local-ssd.
    /// ```
    ///
    /// An `a3-highgpu-1g` carries a Local SSD by machine type, whether or not
    /// the create asked for one, so the stop that the budget gate issues could
    /// not stop the machine the gate was watching — at 3.72 $/h, with nobody
    /// necessarily in front of the terminal.
    ///
    /// `true` (discard) rather than `false` (preserve), and that is a decision
    /// about what the disk holds: nothing. The weights live in `/dev/shm`
    /// (TDX-encrypted RAM) and never touch it, this spec is SPOT so
    /// a preemption already discards it, and preserving Local SSD data across a
    /// stop is billed (gcloud `stop --help`, SDK 580.0.0, read 2026-08-14).
    /// Paying to keep scratch nothing reads would be the worse default.
    ///
    /// Unconditional rather than keyed on the machine type: gcloud takes the
    /// flag on every instance (same `--help`) and GCE only validates it when a
    /// Local SSD is actually present, so a machine-type test here would be one
    /// more thing to keep in sync — and the failure mode of forgetting it is a
    /// VM nobody can stop. `delete` needs no counterpart: it has no such flag
    /// (`gcloud compute instances delete --help`, same version, read the same
    /// day), because deleting the instance takes its Local SSD with it.
    pub fn to_stop_argv(&self) -> Vec<String> {
        vec![
            "compute".into(),
            "instances".into(),
            "stop".into(),
            self.name.clone(),
            format!("--zone={}", self.zone),
            "--discard-local-ssd=true".into(),
        ]
    }

    /// The stop as a human reads it — for the operator lever printed when our
    /// own stop failed.
    ///
    /// Derived from `to_stop_argv` rather than typed out again: a hand-written
    /// lever is a second source of truth for the same command, and the version
    /// that got typed out is precisely the one that would go on missing
    /// `--discard-local-ssd` and hand the operator a command failing exactly
    /// the way ours did.
    pub fn to_stop_command(&self) -> String {
        format!("gcloud {}", self.to_stop_argv().join(" "))
    }
}

/// GCE instance names: lowercase letter first, then lowercase, digits or
/// hyphens, 63 characters at most.
fn is_valid_instance_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheap prover: a confidential C3 with no GPU, which settles the
    /// attestation question before any H100 is billed. Taken from the owner's
    /// own console configuration on 2026-08-13 ($0.22/h) so the fixture and
    /// the intent cannot drift apart.
    #[test]
    fn a_spec_saved_before_the_self_stop_existed_still_gets_one() {
        let mut v = serde_json::to_value(cheap_prover()).unwrap();
        v.as_object_mut().unwrap().remove("max_run_minutes");
        let spec: VmSpec = serde_json::from_value(v).unwrap();
        assert_eq!(spec.max_run_minutes, 120);
        assert_eq!(spec.validate(), Ok(()));
    }

    #[test]
    fn every_instance_stops_itself_even_if_x0ne_is_gone() {
        for spec in [cheap_prover(), h100()] {
            let argv = spec.to_gcloud_argv();
            assert!(
                argv.contains(&"--max-run-duration=60m".to_string()),
                "{argv:?}"
            );
            assert!(argv.contains(&"--instance-termination-action=STOP".to_string()));
            assert_eq!(
                argv.iter()
                    .filter(|a| a.starts_with("--instance-termination-action"))
                    .count(),
                1,
                "one termination action, not two"
            );
        }
    }

    #[test]
    fn the_local_ssd_flag_goes_with_a3_only() {
        let flag = "--discard-local-ssds-at-termination-timestamp=true".to_string();
        assert!(
            h100().to_gcloud_argv().contains(&flag),
            "a3 has a Local SSD"
        );
        assert!(
            !cheap_prover().to_gcloud_argv().contains(&flag),
            "gcloud refuses it on c3"
        );
    }

    #[test]
    fn a_spec_without_a_reachable_self_stop_is_refused() {
        for minutes in [0, MAX_RUN_CAP_MINUTES + 1] {
            let spec = VmSpec {
                max_run_minutes: minutes,
                ..cheap_prover()
            };
            assert_eq!(
                spec.validate(),
                Err(SpecRejected::MaxRunOutOfRange { minutes })
            );
        }
    }

    fn cheap_prover() -> VmSpec {
        VmSpec {
            name: "x0ne-attest-probe".into(),
            machine_type: "c3-standard-4".into(),
            zone: "us-central1-a".into(),
            image_family: "ubuntu-2604-lts-amd64".into(),
            image_project: "ubuntu-os-cloud".into(),
            boot_disk_gb: 10,
            confidential: Some(ConfidentialTee::Tdx),
            accelerators: vec![],
            provisioning_model: ProvisioningModel::Standard,
            max_run_minutes: 60,
        }
    }

    /// The target: one H100, the only shape that exists.
    fn h100() -> VmSpec {
        VmSpec {
            name: "x0ne-h100".into(),
            machine_type: CONFIDENTIAL_GPU_MACHINE_TYPE.into(),
            zone: "us-central1-a".into(),
            image_family: "ubuntu-2604-lts-amd64".into(),
            image_project: "ubuntu-os-cloud".into(),
            boot_disk_gb: 200,
            confidential: Some(ConfidentialTee::Tdx),
            accelerators: vec![Accelerator {
                model: "nvidia-h100-80gb".into(),
                count: 1,
            }],
            provisioning_model: ProvisioningModel::Spot,
            max_run_minutes: 60,
        }
    }

    /// THE budget property, at the level of the argv: the stop must carry
    /// `--discard-local-ssd`, or it cannot stop an `a3`.
    ///
    /// A failure here means the run of 2026-08-14 repeats: `gcloud compute
    /// instances stop x0ne-cc --zone=us-central1-a` answered `HTTPError 400:
    /// VM has a Local SSD attached but an undefined value for
    /// discard-local-ssd`, and a 3.72 $/h machine kept billing while the
    /// budget gate reported that it had turned it off.
    #[test]
    fn the_stop_argv_can_actually_stop_a_machine_with_a_local_ssd() {
        let argv = h100().to_stop_argv();
        assert_eq!(
            argv,
            vec![
                "compute",
                "instances",
                "stop",
                "x0ne-h100",
                "--zone=us-central1-a",
                "--discard-local-ssd=true",
            ]
        );
        // The lever an operator is handed is the same command, letter for
        // letter — a printed stop that omits the flag fails the way ours did.
        assert_eq!(
            h100().to_stop_command(),
            "gcloud compute instances stop x0ne-h100 --zone=us-central1-a --discard-local-ssd=true"
        );
        // Unconditional, machine type included: see `to_stop_argv` for why a
        // per-series test would be one more thing to forget.
        assert!(cheap_prover()
            .to_stop_argv()
            .contains(&"--discard-local-ssd=true".to_string()));
    }

    #[test]
    fn the_cheap_prover_builds_the_command_the_console_shows() {
        let spec = cheap_prover();
        assert_eq!(spec.validate(), Ok(()));
        let argv = spec.to_gcloud_argv();
        assert_eq!(
            argv,
            vec![
                "compute",
                "instances",
                "create",
                "x0ne-attest-probe",
                "--zone=us-central1-a",
                "--machine-type=c3-standard-4",
                "--image-family=ubuntu-2604-lts-amd64",
                "--image-project=ubuntu-os-cloud",
                "--boot-disk-size=10GB",
                "--confidential-compute-type=TDX",
                "--maintenance-policy=TERMINATE",
                "--shielded-secure-boot",
                "--shielded-vtpm",
                "--shielded-integrity-monitoring",
                "--instance-termination-action=STOP",
                "--max-run-duration=60m",
            ]
        );
    }

    /// A confidential instance that could live-migrate would be one whose
    /// memory key does not travel with it. The API refuses the create without
    /// this flag, so its absence is a failed round trip, not a hardening gap.
    #[test]
    fn confidential_always_terminates_on_maintenance() {
        for spec in [cheap_prover(), h100()] {
            assert!(spec
                .to_gcloud_argv()
                .contains(&"--maintenance-policy=TERMINATE".to_string()));
        }
    }

    /// Measured boot is what attestation reads. A confidential VM without the
    /// shielded flags boots fine and cannot be verified — the failure lands
    /// two subsystems away from its cause, which is why it is pinned here.
    #[test]
    fn confidential_requests_the_measurements_attestation_will_read() {
        let argv = h100().to_gcloud_argv();
        for flag in [
            "--shielded-secure-boot",
            "--shielded-vtpm",
            "--shielded-integrity-monitoring",
        ] {
            assert!(argv.contains(&flag.to_string()), "missing {flag}");
        }
    }

    /// An ordinary VM gets none of the confidential machinery. Emitting it
    /// anyway would make every non-confidential create fail for a reason the
    /// operator never asked for.
    #[test]
    fn a_plain_vm_carries_no_confidential_flags() {
        let mut spec = cheap_prover();
        spec.confidential = None;
        let argv = spec.to_gcloud_argv();
        assert!(!argv.iter().any(|a| a.contains("confidential-compute-type")));
        assert!(!argv.iter().any(|a| a.contains("shielded")));
        assert!(!argv.iter().any(|a| a.contains("maintenance-policy")));
    }

    /// The constraint most likely to be missed, because it has nothing to do
    /// with confidentiality: an on-demand confidential A3 is refused by the
    /// API. Catching it here costs a unit test; catching it in production
    /// costs a failed create against an $11.26/h reservation.
    #[test]
    fn a_confidential_gpu_on_demand_is_refused_before_spending() {
        let mut spec = h100();
        spec.provisioning_model = ProvisioningModel::Standard;
        assert_eq!(
            spec.validate(),
            Err(SpecRejected::ConfidentialGpuRequiresSpot)
        );
        assert!(spec.validate().unwrap_err().message().contains("SPOT"));
    }

    /// The termination action is stated in the create call, not inherited.
    ///
    /// It happens to match the current GCP default for Spot, so this pins an
    /// intent rather than closing a hole: the confidential H100 has no non-Spot
    /// form, preemption is an ordinary event on it, and resume-by-detection
    /// depends entirely on the boot disk outliving one. A failure here means
    /// that dependency went back to resting on someone else's default — which
    /// would surface as a next `x0ne cc` observing `Absent` and paying to
    /// install the driver, vLLM and the weights all over again.
    #[test]
    fn a_spot_instance_states_its_termination_action_instead_of_inheriting_one() {
        assert!(h100()
            .to_gcloud_argv()
            .contains(&"--instance-termination-action=STOP".to_string()));
        // A Standard instance carries it too: gcloud refuses the
        // flag on one WITHOUT --max-run-duration, and requires it WITH one
        // (SDK 585.0.0 --help), and every spec now has a self-stop.
        assert!(cheap_prover()
            .to_gcloud_argv()
            .contains(&"--instance-termination-action=STOP".to_string()));
    }

    #[test]
    fn a_confidential_gpu_outside_the_three_zones_is_refused() {
        let mut spec = h100();
        spec.zone = "europe-west1-b".into();
        assert!(matches!(
            spec.validate(),
            Err(SpecRejected::ZoneDoesNotSupportConfidentialGpu { .. })
        ));
        // The message names the alternatives, so the operator does not have to
        // go and look them up.
        assert!(spec
            .validate()
            .unwrap_err()
            .message()
            .contains("us-central1-a"));
    }

    #[test]
    fn a_confidential_gpu_beyond_one_is_refused() {
        let mut spec = h100();
        spec.accelerators[0].count = 2;
        assert_eq!(
            spec.validate(),
            Err(SpecRejected::TooManyConfidentialGpus { count: 2 })
        );
    }

    #[test]
    fn a_confidential_gpu_on_the_wrong_machine_type_is_refused() {
        let mut spec = h100();
        spec.machine_type = "a3-highgpu-8g".into();
        // The TEE-series rule fires first — TDX does not exist on the 8g
        // either — and its message already names the one machine type that
        // works, so the operator is pointed at the same fix.
        let err = spec.validate().unwrap_err();
        assert!(matches!(
            err,
            SpecRejected::TeeNotSupportedOnMachineSeries { .. }
        ));
        assert!(err.message().contains(CONFIDENTIAL_GPU_MACHINE_TYPE));
    }

    /// The narrow rules apply to the confidential-plus-GPU shape and to
    /// nothing else. A validator that also refused the cheap prover — no GPU,
    /// standard provisioning, a C3 — would block the very step meant to
    /// de-risk the expensive one.
    #[test]
    fn the_narrow_rules_do_not_leak_onto_other_shapes() {
        assert_eq!(cheap_prover().validate(), Ok(()));

        let mut plain_gpu = h100();
        plain_gpu.confidential = None;
        plain_gpu.provisioning_model = ProvisioningModel::Standard;
        plain_gpu.zone = "europe-west1-b".into();
        assert_eq!(
            plain_gpu.validate(),
            Ok(()),
            "a non-confidential GPU VM is not subject to the confidential rules"
        );
    }

    /// The rejection the owner hit live in the console on 2026-08-13:
    /// TDX on a machine series that does not carry it. The console catches it
    /// with a red banner; a headless `gcloud` catches it with a failed round
    /// trip; this validator catches it before the prompt is even shown.
    #[test]
    fn tdx_on_an_unsupported_series_is_refused_like_the_console_does() {
        let mut spec = cheap_prover();
        spec.machine_type = "e2-standard-4".into();
        assert!(matches!(
            spec.validate(),
            Err(SpecRejected::TeeNotSupportedOnMachineSeries { .. })
        ));
        // And the message names the series that DO work, as the console does.
        assert!(spec.validate().unwrap_err().message().contains("c3-"));

        // SEV-SNP has its own series; a C3 is not one of them.
        let mut snp = cheap_prover();
        snp.confidential = Some(ConfidentialTee::SevSnp);
        assert!(matches!(
            snp.validate(),
            Err(SpecRejected::TeeNotSupportedOnMachineSeries { .. })
        ));
        let mut snp_ok = cheap_prover();
        snp_ok.confidential = Some(ConfidentialTee::SevSnp);
        snp_ok.machine_type = "n2d-standard-4".into();
        assert_eq!(snp_ok.validate(), Ok(()));
    }

    #[test]
    fn an_invalid_instance_name_is_caught_before_the_round_trip() {
        for bad in [
            "",
            "X0ne",
            "9lives",
            "x0ne_probe",
            "x0ne.probe",
            &"a".repeat(64),
        ] {
            let mut spec = cheap_prover();
            spec.name = bad.to_string();
            assert!(
                matches!(spec.validate(), Err(SpecRejected::InvalidName { .. })),
                "{bad:?} should be rejected"
            );
        }
        for good in ["a", "x0ne-h100", "x0ne-probe-2"] {
            let mut spec = cheap_prover();
            spec.name = good.to_string();
            assert_eq!(spec.validate(), Ok(()), "{good:?} should be accepted");
        }
    }

    /// Argv, not a shell string. `RunCommand` has no shell, so a value with a
    /// space is one argument; joining for display is lossy and one-way.
    #[test]
    fn the_command_is_argv_and_the_string_is_only_for_reading() {
        let spec = h100();
        let argv = spec.to_gcloud_argv();
        assert!(argv.iter().all(|a| !a.is_empty()));
        assert!(spec
            .to_display_command()
            .starts_with("gcloud compute instances create"));
        // Every argv element appears in the rendered line, so what the human
        // approves is what will run.
        for a in &argv {
            assert!(spec.to_display_command().contains(a.as_str()));
        }
    }

    /// The cross-subsystem contract the program spec asks to get right early:
    /// the image is never left to a `gcloud` default, because attestation
    /// verifies a measurement of the image that actually booted.
    #[test]
    fn the_image_is_always_explicit() {
        for spec in [cheap_prover(), h100()] {
            let argv = spec.to_gcloud_argv();
            assert!(argv.iter().any(|a| a.starts_with("--image-family=")));
            assert!(argv.iter().any(|a| a.starts_with("--image-project=")));
        }
    }
}
