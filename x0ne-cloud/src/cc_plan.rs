//! The cc pipeline as data: observations in, remaining steps out.
//!
//! Everything here is pure so that "what would `x0ne cc` do right now?" is a
//! unit test, not a cloud bill. The impure half (running gcloud, ssh) lives in
//! `x0ne-orchestrator::cc_runner` and consumes exactly these types.
//!
//! Resume-by-detection (owner decision 2026-08-13): there is no state file.
//! The runner observes what exists (VM present? running?) and this module
//! decides what remains. A state file can lie after a crash; the cloud cannot.

use serde::{Deserialize, Serialize};

use crate::provision::{Accelerator, ConfidentialTee, ProvisioningModel, VmSpec};

/// The two shapes `x0ne cc` can build. Prices are dated estimates for the
/// wizard, not a bill (GCP billing lags by hours).
///
/// As of 2026-08-13: the console totals (owner's screenshots) EXCLUDE the
/// confidential-computing premium — both list it as "variable cost" outside
/// the total, which is how the first draft of this plan under-quoted. The TDX
/// premium is a flat uplift (cloud.google.com/confidential-computing/
/// confidential-vm/pricing, read 2026-08-13): $0.0033982/vCPU/h +
/// $0.0004555/GiB/h on-demand. Hence:
///   c3-standard-4  ≈ 0.22 (base) + 0.021 (TDX 4 vCPU/16 GiB)   ≈ $0.24/h
///   a3-highgpu-1g  ≈ 11.26 (base, ON-DEMAND console quote) + ≈0.20
///                    (IF the C3 rates apply to A3 — unverified)  ≈ $11.46/h
///
/// Superseded for the full machine on 2026-08-14: the confidential H100 has no
/// on-demand form at all (SPOT or flex-start only), so the $11.46 on-demand
/// quote priced a machine that cannot be created. The SPOT figure is $3.72/h —
/// see `hourly_usd`, which carries the caveat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CcMachine {
    /// The cheap attestation prover: settles the TDX question before any H100 spend.
    Prover,
    /// The target: one confidential H100. Requires SPOT (API constraint).
    FullH100,
}

impl CcMachine {
    pub fn vm_spec(&self, name: &str) -> VmSpec {
        match self {
            Self::Prover => VmSpec {
                name: name.into(),
                machine_type: "c3-standard-4".into(),
                zone: "us-central1-a".into(),
                image_family: "ubuntu-2604-lts-amd64".into(),
                image_project: "ubuntu-os-cloud".into(),
                boot_disk_gb: 10,
                confidential: Some(ConfidentialTee::Tdx),
                accelerators: vec![],
                provisioning_model: ProvisioningModel::Standard,
                // The prover boots, attests and is done in minutes.
                max_run_minutes: 60,
            },
            // SECURITY: `ProvisioningModel::Spot` is not a price choice here.
            // The confidential H100 exists in SPOT (or flex-start) form only
            // (GCP doc, read 2026-08-14) — a Standard spec is refused at create
            // time, i.e. after the operator has set up billing and approved the
            // plan. `VmSpec::validate` refuses it earlier still.
            Self::FullH100 => VmSpec {
                name: name.into(),
                machine_type: "a3-highgpu-1g".into(),
                zone: "us-central1-a".into(),
                // ubuntu-2204-lts, not the 26.04 the prover boots: the GCP
                // "Confidential VM with GPU" procedure prescribes 22.04, and
                // the NVIDIA confidential-compute driver packages are the ones
                // published for it (spec 2026-08-14). Booting a different
                // family is how the GPU driver step fails for a reason that
                // looks like a driver bug.
                image_family: "ubuntu-2204-lts".into(),
                image_project: "ubuntu-os-cloud".into(),
                // 50 GB, down from the 200 of the first draft. The weights live
                // in /dev/shm (TDX-encrypted RAM), never on disk, so
                // the disk only carries the OS, the driver and vLLM — and 150
                // unused GB is billed for the whole life of the instance.
                boot_disk_gb: 50,
                confidential: Some(ConfidentialTee::Tdx),
                accelerators: vec![Accelerator {
                    model: "nvidia-h100-80gb".into(),
                    count: 1,
                }],
                provisioning_model: ProvisioningModel::Spot,
                // August's 9 runs averaged about 15 min (≈ 8 $ at 3.72 $/h,
                // deduced, not timed): 2 h caps a forgotten run near 7.5 $.
                max_run_minutes: 120,
            },
        }
    }

    /// Estimated, dated. Displayed with `≈` only.
    ///
    /// Prover: 0.24 (2026-08-13, owner's console + the TDX premium table).
    ///
    /// FullH100: 3.72 — SPOT a3-highgpu-1g, estimate of 2026-08-14, **unverified
    /// until the first real run**. It is not the $11.46 this returned before:
    /// that was the ON-DEMAND console quote for a machine whose confidential
    /// form cannot be created on demand. The number matters beyond display,
    /// because `spent_usd` feeds it to the budget gate — quoting the machine
    /// 3× over would make the gate cut a run that was still well inside its
    /// cap. The real rate gets measured at the H100 milestone with the owner.
    pub fn hourly_usd(&self) -> f64 {
        match self {
            Self::Prover => 0.24,
            Self::FullH100 => 3.72,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CcConfig {
    pub machine: CcMachine,
    pub vm_name: String,
    pub cap_usd: f64,
}

// NOTE: CcConfig derives PartialEq only (not Eq) because cap_usd is f64.
// Rust: f64 is PartialEq but not Eq.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    Absent,
    Running {
        zone: String,
    },
    Stopped {
        zone: String,
    },
    /// STOPPING / PROVISIONING / STAGING…: acting on a moving target races.
    Transitional {
        status: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcObservations {
    pub auth_account: Option<String>,
    pub project: Option<String>,
    /// None = not probed yet; Some(false) = probed and absent.
    pub image_family_exists: Option<bool>,
    pub instance: InstanceState,
    /// The four in-VM observations. They are read over SSH, so a well-behaved
    /// producer leaves them `None` unless the instance is Running — and
    /// `full_h100_steps` ENFORCES that rather than trusting it: on anything
    /// other than Running they are ignored outright. Review I1 (2026-08-14):
    /// this paragraph used to state the rule and no code applied it.
    ///
    /// `None` means "not probed", NOT "absent", and `build_plan` treats it as
    /// "do the work". Running an idempotent install twice costs a minute;
    /// skipping it because a probe never ran costs a broken pipeline several
    /// steps later.
    ///
    /// `nvidia-smi` answers.
    pub gpu_driver_ready: Option<bool>,
    /// The GPU reports confidential-compute mode ON.
    pub gpu_cc_on: Option<bool>,
    /// vLLM answers on `127.0.0.1:8000/v1/models` — which implies it is
    /// installed, has its weights, and is served.
    pub vllm_healthy: Option<bool>,
    /// The `x0ne` binaries are present in the VM at the expected version.
    pub x0ne_ready: Option<bool>,
}

/// One unit of work the pipeline may run.
///
/// The variants after `AttestTdx` belong to the full machine only (spec
/// 2026-08-14). Matches on this enum are exhaustive with no `_` arm on
/// purpose, here and in `cc_runner::run_step` and `cc_cmd::step_line`: a step
/// added without a runner arm would be a plan line that does nothing, and a
/// step added without a plan line would execute without being shown to the
/// human who approved the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CcStep {
    CreateVm,
    StartVm,
    WaitSsh,
    AttestTdx,
    /// NVIDIA driver install (idempotent). Reboots the VM: the LKCA modprobe
    /// rule the confidential GPU needs only takes effect from a rebuilt
    /// initramfs (GCP procedure, read 2026-08-14).
    InstallGpuDriver,
    /// Read back the GPU's confidential-compute mode; OFF is a refusal, not
    /// something to force at runtime.
    ///
    /// Named `CheckGpuCc`, not `EnableGpuCc` (task 3 review, minor M3): the
    /// step is a LECTURE and the name is the thing that keeps it one. CC mode
    /// on a GCP confidential GPU is fixed when the instance is created and
    /// there is no supported way to turn it on afterwards, so a name promising
    /// "enable" invites the next contributor to make the step keep its promise
    /// — by adding a mutation, on a GPU, in the middle of an attestation
    /// pipeline. What the step decides is whether we may serve a model, and a
    /// step that both decides and mutates is one no reader can audit.
    CheckGpuCc,
    /// The GPU half of the proof, observed from the attested CPU enclave.
    AttestGpu,
    /// Declare the confidential GPU READY, so CUDA may initialise on it
    /// (`nvidia-smi conf-compute -srs 1`).
    ///
    /// Run 4 of the H100 milestone, 2026-08-14, measured: without it vLLM
    /// crash-looped 46 times on `RuntimeError: cudaGetDeviceCount ... Error
    /// 802: system not yet initialized`, `conf-compute -grs` answered
    /// `not-ready`, and a single `-srs 1` made the 802 disappear.
    ///
    /// SECURITY, and it is the reason this is a step of its own placed HERE:
    /// NVIDIA's confidential mode ships the GPU disarmed on purpose, so that
    /// the guest may refuse to hand it any work until the guest has attested
    /// it. Arming it is therefore the consequence of a proof, never its
    /// prelude — `AttestGpu` first, `ArmGpu` after, and `full_h100_steps`
    /// pins that order. Its own name is a MUTATION verb for the reason
    /// `CheckGpuCc` is a reading verb (task 3 review, minor M3): a reader must
    /// be able to tell which steps change the machine from the list alone.
    ///
    /// The ready state is PER BOOT: it does not survive the reboot
    /// `InstallGpuDriver` performs, and it cannot be carried over from an
    /// earlier run of this pipeline. Hence unconditional in the plan, with a
    /// live `-grs` probe inside the step as its only acquis.
    ArmGpu,
    InstallVllm,
    /// Prepare the tmpfs the weights live in (TDX-encrypted RAM,
    /// never the disk).
    FetchModel,
    ServeVllm,
    /// Swap off, port 8000 bound to loopback only, firewall closed,
    /// boot disk encrypted. Any `false` is a refusal.
    ValidateEncryption,
    HealthCheck,
    InstallX0ne,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRefused {
    NotAuthenticated,
    NoProject,
    ImageFamilyMissing {
        family: String,
    },
    InstanceInTransition {
        status: String,
    },
    /// A VM with our name exists, but somewhere else.
    ///
    /// Final review, F2 (2026-08-13): `parse_instance_state` had captured
    /// `zone` since day one and NOTHING read it — every later command
    /// (`ssh`, `stop`, `delete`) targets `spec.zone` unconditionally. So a
    /// hand-made `x0ne-cc` in another zone made the plan skip creation (the
    /// VM "exists"), then aim every step at the wrong zone: the SSH probes
    /// blame the boot, and the stop fails against an instance that is not
    /// there. Refusing is the only safe reading — and it is what gives the
    /// `zone` field a consumer.
    ZoneMismatch {
        observed: String,
        expected: String,
    },
}

impl PlanRefused {
    /// Every refusal names the command that unblocks it. A refusal without a
    /// lever costs the operator a research round-trip (field lesson,
    /// 2026-08-13: the `add "?" to allowed_dirs` hint).
    pub fn message(&self) -> String {
        match self {
            Self::NotAuthenticated => "gcloud is not authenticated. Run: gcloud auth login".into(),
            Self::NoProject => {
                "No gcloud project set. Run: gcloud config set project <PROJECT_ID>".into()
            }
            Self::ImageFamilyMissing { family } => format!(
                "Image family '{family}' not found. Inspect what exists with: \
                 gcloud compute images list --filter=\"family~ubuntu\" --project ubuntu-os-cloud"
            ),
            Self::InstanceInTransition { status } => format!(
                "The VM is currently {status}; acting on a moving instance races. \
                 Wait a minute and re-run `x0ne cc`."
            ),
            Self::ZoneMismatch { observed, expected } => format!(
                "A VM named 'x0ne-cc' already exists in zone {observed}, but this plan \
                 targets {expected}, and every step (ssh, stop, delete) would aim at \
                 {expected}. Delete the other one, or move out of its way: \
                 gcloud compute instances delete x0ne-cc --zone={observed}"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsParseError {
    pub what: &'static str,
    pub detail: String,
}

/// SECURITY: unparseable output is an error, never `Absent`. Misreading the
/// list as "no VM" would create a second VM — double spend, or a name clash
/// half-way through creation.
pub fn parse_instance_state(
    instances_json: &str,
    name: &str,
) -> Result<InstanceState, ObsParseError> {
    let v: serde_json::Value = serde_json::from_str(instances_json).map_err(|e| ObsParseError {
        what: "instances list",
        detail: e.to_string(),
    })?;
    let arr = v.as_array().ok_or_else(|| ObsParseError {
        what: "instances list",
        detail: "expected a JSON array".into(),
    })?;
    for inst in arr {
        if inst["name"].as_str() != Some(name) {
            continue;
        }
        // #47c: a zone we cannot read used to become the literal "?" and
        // resurface as an unusable lever (`--zone=?`). Unreadable output is
        // an ERROR of observation — same doctrine as the parser above.
        let zone = match inst["zone"].as_str().and_then(|z| z.rsplit('/').next()) {
            Some(z) if !z.is_empty() => z.to_string(),
            _ => {
                return Err(ObsParseError {
                    what: "instance zone",
                    detail: format!("instance {name} has no readable zone field"),
                })
            }
        };
        let Some(status) = inst["status"].as_str() else {
            return Err(ObsParseError {
                what: "instance status",
                detail: format!("instance {name} has no readable status field"),
            });
        };
        return Ok(match status {
            "RUNNING" => InstanceState::Running { zone },
            // gcloud calls a stopped VM TERMINATED. It is restartable.
            "TERMINATED" | "SUSPENDED" => InstanceState::Stopped { zone },
            other => InstanceState::Transitional {
                status: other.to_string(),
            },
        });
    }
    Ok(InstanceState::Absent)
}

pub fn parse_auth_account(auth_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(auth_json).ok()?;
    v.as_array()?
        .iter()
        .find(|a| a["status"].as_str() == Some("ACTIVE"))
        .and_then(|a| a["account"].as_str())
        .map(String::from)
}

pub fn parse_image_family_exists(images_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(images_json)
        .ok()
        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The in-VM probes.
//
// Everything below reads stdout produced INSIDE the VM and relayed by ssh.
// Two consequences shape all of it. First, nobody reads this text: it is
// consumed by code and summarised in one report line, so a wrong reading is
// silent. Second, its producer is the machine we are trying to make a
// statement about — a compromised VM writes the evidence of its own
// innocence. Hence the shared rule of `parse_instance_state`: output we
// cannot read becomes an `Err`, never the reassuring half of a verdict.
// ---------------------------------------------------------------------------

/// Read the GPU's confidential-compute mode from `nvidia-smi conf-compute -f`.
///
/// Fixture source: GCP, "Verify Confidential Computing is enabled on GPUs"
/// (cloud.google.com/confidential-computing/confidential-vm/docs/
/// verify-confidential-computing-mode-on-gpu, read 2026-08-14). It prescribes
/// `sudo nvidia-smi conf-compute -f` and shows one line: `CC status: ON`.
/// A GPU or driver without the feature prints a "not supported" sentence and
/// no `CC status:` line at all, which lands in the error arm — which is why
/// the exact unsupported wording does not have to be pinned here.
///
/// SECURITY: `Ok(false)` and `Err` are different facts and the caller must be
/// able to tell them apart. `Ok(false)` is "the GPU answered OFF", which
/// `CheckGpuCc` turns into a refusal to serve a model. `Err` is "nothing
/// answered", a pipeline fault to surface. Collapsing the second into the
/// first would report a mode nobody measured.
///
/// Two anchors keep a hostile VM from flipping the verdict by printing text:
///   1. the marker is matched at the START of a line, so `CC status: ON`
///      inside another field (a process name, a `-q` dump, an error message
///      quoting the command) is not a verdict;
///   2. two lines that disagree are an error, not a majority vote — appending
///      `CC status: ON` after a real `CC status: OFF` must not win.
pub fn parse_nvidia_cc_mode(stdout: &str) -> Result<bool, ObsParseError> {
    const WHAT: &str = "nvidia-smi conf-compute -f";
    let mut verdict: Option<bool> = None;
    for line in stdout.lines() {
        let Some(value) = line.trim().strip_prefix("CC status:") else {
            continue;
        };
        let value = value.trim();
        let seen = if value.eq_ignore_ascii_case("ON") {
            true
        } else if value.eq_ignore_ascii_case("OFF") {
            false
        } else {
            return Err(ObsParseError {
                what: WHAT,
                detail: format!("unreadable CC status value {:?}", quoted_excerpt(value)),
            });
        };
        if matches!(verdict, Some(previous) if previous != seen) {
            return Err(ObsParseError {
                what: WHAT,
                detail: "two 'CC status:' lines disagree".into(),
            });
        }
        verdict = Some(seen);
    }
    verdict.ok_or(ObsParseError {
        what: WHAT,
        detail: "no 'CC status:' line".into(),
    })
}

/// The marker `nvidia-smi conf-compute -grs` prints its ready state behind.
///
/// Matched case-insensitively at the start of a line, exactly as
/// `parse_nvidia_cc_mode` matches `CC status:` and for the same reason: the
/// same sentence inside another field is not a verdict.
const READY_STATE_MARKER: &str = "confidential compute gpus ready state:";

/// Read the confidential GPU's READY state from `nvidia-smi conf-compute -grs`.
///
/// Fixture sources, both read 2026-08-14:
///   * GCP, "Verify Confidential Computing is enabled on GPUs" — prescribes
///     `sudo nvidia-smi conf-compute -grs` and shows
///     `Confidential Compute GPUs Ready state: ready`;
///   * NVIDIA/nvtrust#37 — the same command on a disarmed GPU:
///     `Confidential Compute GPUs Ready state: not-ready`.
///
/// Why this parser exists at all (run 4 of the H100 milestone, 2026-08-14,
/// measured on the real VM): a GPU in confidential mode refuses CUDA until the
/// guest declares it ready. vLLM crash-looped 46 times on `cudaGetDeviceCount
/// ... Error 802: system not yet initialized`, `-grs` said `not-ready`, and
/// `-srs 1` ended it. So this reading gates ~15 GB of download and half an hour
/// of a3-highgpu-1g.
///
/// SECURITY: same three rules as `parse_nvidia_cc_mode`, same reasons.
/// `Ok(false)` ("the GPU says it is not armed") and `Err` ("nothing readable
/// answered") are different facts; the marker is anchored at the start of a
/// line; and two lines that disagree are an error rather than a majority vote.
/// A value that is neither ready nor not-ready is an `Err` — this pipeline does
/// not guess which half of a spelling it is looking at.
///
/// `0` and `1` are accepted alongside the words because `-srs` takes them as
/// arguments and nvidia-smi's output wording is not a contract we control; the
/// SET of accepted spellings is closed, which is the property that matters.
pub fn parse_gpu_ready_state(stdout: &str) -> Result<bool, ObsParseError> {
    const WHAT: &str = "nvidia-smi conf-compute -grs";
    let mut verdict: Option<bool> = None;
    for line in stdout.lines() {
        let lowered = line.trim().to_ascii_lowercase();
        let Some(value) = lowered.strip_prefix(READY_STATE_MARKER) else {
            continue;
        };
        let value = value.trim();
        let seen = match value {
            "ready" | "1" => true,
            "not-ready" | "not ready" | "notready" | "0" => false,
            _ => {
                return Err(ObsParseError {
                    what: WHAT,
                    detail: format!("unreadable ready state {:?}", quoted_excerpt(value)),
                })
            }
        };
        if matches!(verdict, Some(previous) if previous != seen) {
            return Err(ObsParseError {
                what: WHAT,
                detail: "two 'Ready state:' lines disagree".into(),
            });
        }
        verdict = Some(seen);
    }
    verdict.ok_or(ObsParseError {
        what: WHAT,
        detail: "no 'Ready state:' line".into(),
    })
}

/// Longest excerpt of remote text an `ObsParseError` quotes back.
///
/// SECURITY (review round 1, finding I2): every `detail` built from remote
/// text ends up inside an error message, which ends up inside the report an
/// operator reads to learn that something was refused. Measured on
/// `CC status: ` + 50 000 × 'A': the refusal came out 50 000 characters long
/// and pushed its own verdict off the screen. Neutralising control characters
/// (`cc_runner::sanitize`) does nothing about that — a wall of harmless 'A' is
/// as effective at hiding a line as an ANSI escape. The value's expected size
/// is "ON" or "OFF"; 80 characters is generous for showing what came instead.
const EXCERPT_MAX_CHARS: usize = 80;

/// Bound a piece of remote text before it is quoted in an error. Truncation is
/// VISIBLE (`…`), so nobody reads a cut string as the whole answer.
fn quoted_excerpt(s: &str) -> String {
    if s.chars().count() <= EXCERPT_MAX_CHARS {
        return s.to_string();
    }
    s.chars()
        .take(EXCERPT_MAX_CHARS - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// Is `nvidia-smi` answering at all — i.e. is the driver loaded?
///
/// A probe of an *acquis*, not a proof: it gates whether `InstallGpuDriver`
/// still has work to do. `bool`, not `Result`, because "no" and "unreadable"
/// lead to the same idempotent re-install.
///
/// The trap this closes, and the reason it is not `contains("NVIDIA-SMI")`:
/// the classic failure message is `NVIDIA-SMI has failed because it couldn't
/// communicate with the NVIDIA driver…` — it CONTAINS the marker. The table
/// header of a working driver (fixture below, driver 550.x) is the only form
/// that also carries `Driver Version:`, on that same line.
pub fn parse_nvidia_smi_ready(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|l| l.contains("NVIDIA-SMI") && l.contains("Driver Version"))
}

/// Is there no active swap? (`swapon --show`)
///
/// The weights live in `/dev/shm`, i.e. in TDX-encrypted RAM,
/// and never on disk. Swap is the one path that moves RAM to a disk behind
/// everyone's back, so the pipeline REFUSES a machine that has any — it does
/// not turn it off (this comment claimed it did; review round 1 of task 4).
/// `ValidateEncryption` names `swapoff -a` and stops, because disabling swap
/// under a running system is a change to someone's machine that a validation
/// step has no business making, and because a machine configured to swap will
/// be configured to swap again at the next boot.
///
/// Fixture source: util-linux `swapon(8)` — with no swap area active the
/// table is empty and NOTHING is printed, not even the NAME/TYPE header.
/// So "no output" is the documented success shape, not an accident.
///
/// `bool` without an error arm is safe here because both readings fail
/// closed: anything unrecognised is non-empty text, hence "swap is on", hence
/// the pipeline refuses the machine and names `swapoff` for the operator (it
/// never alters a machine's memory policy itself — re-review round 1,
/// 2026-08-14, caught this line still claiming the opposite of the paragraph
/// above). `true` is the only answer that can let weights reach a disk, and
/// it takes literally empty output to obtain it.
pub fn parse_swapon_empty(stdout: &str) -> bool {
    stdout.trim().is_empty()
}

/// Free space in KiB, from `df --output=avail -k <dir>`.
///
/// Fixture source: coreutils `df(1)`, `--output=avail` (read 2026-08-14). It
/// prints one header line — the field name, `Avail` — then one value line per
/// filesystem. `-k` is not decoration: without it the unit follows the
/// environment (`DF_BLOCK_SIZE`, `BLOCK_SIZE`, `POSIXLY_CORRECT`), so the same
/// number would mean different amounts on two machines.
///
/// `Err`, never a number nobody measured. This gates whether ~15 GB of model
/// weights fit in the tmpfs they must live in, and the two ways to
/// be wrong are not symmetric: a defaulted 0 refuses a perfectly good machine
/// (annoying), a defaulted `u64::MAX` lets the download start on a filesystem
/// that cannot hold it and die at 90 %, after minutes of billed H100.
///
/// Two value lines are an error too. `df` prints one per filesystem, so two
/// means the caller asked about something other than the single directory it
/// thinks it asked about, and picking either one would be a guess.
pub fn parse_df_avail_kib(stdout: &str) -> Result<u64, ObsParseError> {
    const WHAT: &str = "df --output=avail -k";
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let values: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.eq_ignore_ascii_case("Avail"))
        .collect();
    let [value] = values[..] else {
        return Err(ObsParseError {
            what: WHAT,
            detail: format!("expected one value line, got {}", values.len()),
        });
    };
    value.parse::<u64>().map_err(|e| ObsParseError {
        what: WHAT,
        detail: format!("unreadable free space {:?}: {e}", quoted_excerpt(value)),
    })
}

/// Section marker: the `busctl … RemoveIPC` answer of the tmpfs step.
pub const TMPFS_PROBE_MARK_REMOVEIPC: &str = "===x0ne:removeipc===";
/// Section marker: the `df --output=avail -k /dev/shm` output.
pub const TMPFS_PROBE_MARK_AVAIL: &str = "===x0ne:tmpfs===";

/// What one probe of the tmpfs step brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsProbe {
    /// Will logind still delete this user's `/dev/shm` files when the last ssh
    /// session closes? `true` means the purge is ARMED, i.e. the step failed to
    /// disarm it — never a state to continue from.
    pub remove_ipc: bool,
    /// Free space in the tmpfs, KiB.
    pub avail_kib: u64,
}

/// Split and read the tmpfs probe: is the purge disarmed, and is there room?
///
/// SECURITY, in this pipeline's sense — fail-closed: an unreadable answer is an
/// `Err` and the caller fails the step. The asymmetry is measured (run 5,
/// 2026-08-14). Reading "disarmed" on garbage hands 15 GB of download to a
/// machine that deletes them out from under the server at the next ssh
/// disconnect, and the symptom that comes back is a slow network — the run 4
/// misdiagnosis, at ~3.72 $/h. Reading "still armed" on a machine that is in
/// fact fine costs one re-run of a pipeline that resumes by observation.
///
/// What it does NOT claim: both figures come from the machine under test, so a
/// compromised VM can forge them. It can only make itself look healthier, and
/// nothing here decides whether the model server is exposed — that verdict is
/// re-read alone by `ValidateEncryption`.
pub fn parse_tmpfs_probe(stdout: &str) -> Result<TmpfsProbe, ObsParseError> {
    const WHAT: &str = "tmpfs probe";
    let ipc_text = marked_section(
        stdout,
        WHAT,
        TMPFS_PROBE_MARK_REMOVEIPC,
        Some(TMPFS_PROBE_MARK_AVAIL),
    )?;
    let avail_text = marked_section(stdout, WHAT, TMPFS_PROBE_MARK_AVAIL, None)?;

    // `busctl get-property … RemoveIPC` prints the type then the value:
    // `b true` / `b false` (systemd 249, read on x0ne-cc 2026-08-14). The TYPE
    // is matched, not just the word: `true` on its own line could come from any
    // command, and this answer decides whether 15 GB survive the next probe.
    let remove_ipc = {
        let values: Vec<&str> = ipc_text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("b "))
            .map(str::trim)
            .collect();
        let [value] = values[..] else {
            return Err(ObsParseError {
                what: WHAT,
                detail: format!("expected one `b <bool>` line, got {}", values.len()),
            });
        };
        match value {
            "true" => true,
            "false" => false,
            other => {
                return Err(ObsParseError {
                    what: WHAT,
                    detail: format!("unreadable RemoveIPC {:?}", quoted_excerpt(other)),
                })
            }
        }
    };

    Ok(TmpfsProbe {
        remove_ipc,
        avail_kib: parse_df_avail_kib(&avail_text)?,
    })
}

/// The body between `marker` and `next` (or the end), or an `Err` naming what
/// was wrong with the shape.
///
/// Exactly one marker line. Zero means the command did not run what we asked;
/// twice means something else is writing our markers, and picking one would be
/// a guess — which is the whole point of marking sections rather than counting
/// lines.
fn marked_section(
    stdout: &str,
    what: &'static str,
    marker: &str,
    next: Option<&str>,
) -> Result<String, ObsParseError> {
    let starts: Vec<usize> = stdout
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim() == marker)
        .map(|(i, _)| i)
        .collect();
    let [start] = starts[..] else {
        return Err(ObsParseError {
            what,
            detail: format!("expected 1 {marker} line, got {}", starts.len()),
        });
    };
    let body: Vec<&str> = stdout
        .lines()
        .skip(start + 1)
        .take_while(|l| next.is_none_or(|n| l.trim() != n))
        .collect();
    Ok(body.join("\n"))
}

/// Does `port` listen on loopback ONLY? (`ss -tlnp`)
///
/// Fixture source: `ss -tlnp` from iproute2, columns
/// `State Recv-Q Send-Q Local Address:Port Peer Address:Port Process`.
///
/// SECURITY: this is the verdict deciding whether an unauthenticated model
/// server on a public cloud IP is reachable from the internet. Three rules
/// hold it up:
///
///   1. **The address is read from a fixed column, never searched for in the
///      line.** `ss` prints the process — `users:(("vllm",pid=2011,fd=5))` —
///      AFTER the address, and a process is named by whoever started it. A
///      `stdout.contains("127.0.0.1:8000")` reading is flipped either way by a
///      process named `127.0.0.1:8000`, and the dangerous direction (a real
///      `0.0.0.0` bind reported as loopback) is the easy one to forge.
///   2. **The port matches exactly.** `18000` ends with `8000` and is a
///      different service; a suffix match would answer for the wrong socket.
///   3. **IPv6 is not IPv4 with brackets.** `[::1]` is loopback, `[::]` is
///      every interface — one character apart, opposite verdicts.
///
/// Absence is an `Err`, not `Ok(true)`. No LISTEN line for the port means
/// nothing is serving, which is a different problem (vLLM did not start) from
/// "bound to loopback"; answering "safe, nothing exposed" would sign a gate
/// off on a machine that was never actually checked.
///
/// Known over-approximation, in the safe direction: an IPv4-mapped form such
/// as `[::ffff:127.0.0.1]` reads as public. It costs a false alarm; the
/// converse would cost an open port.
pub fn parse_ss_localhost_only(stdout: &str, port: u16) -> Result<bool, ObsParseError> {
    let mut seen = false;
    let mut loopback_only = true;
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Column 0 anchors the line as a socket row: it rejects the header and
        // anything a process wrote into stdout on its own.
        if fields.len() < 4 || fields[0] != "LISTEN" {
            continue;
        }
        let Some((addr, port_text)) = fields[3].rsplit_once(':') else {
            continue;
        };
        if !matches!(port_text.parse::<u16>(), Ok(p) if p == port) {
            continue;
        }
        seen = true;
        if !is_loopback_bind(addr) {
            loopback_only = false;
        }
    }
    if !seen {
        return Err(ObsParseError {
            what: "ss -tlnp",
            detail: format!("no LISTEN line for port {port}"),
        });
    }
    Ok(loopback_only)
}

/// `127.0.0.0/8` and `[::1]`, and nothing else. `0.0.0.0`, `*` and `[::]` are
/// wildcards, i.e. the public interface too.
fn is_loopback_bind(addr: &str) -> bool {
    addr == "[::1]"
        || addr
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|v4| v4.is_loopback())
}

// ── What the vLLM wait looks at, besides the port ────────────────────────
//
// Run 4 of the H100 milestone, 2026-08-14, measured: the wait loop probed the
// PORT and nothing else for 30 minutes and 1.26 $ while the unit crash-looped
// 46 times behind it, and the run reported a slow download. A port that is not
// bound is compatible with two opposite situations — a server that is working
// and one that is dying every twenty seconds — and the loop could not tell them
// apart because it never asked anything that could.
//
// So one probe now carries three answers back: the listening sockets, the
// unit's restart counter, and how much of the model has landed in the tmpfs.
// ONE command and not three, because this loop runs up to 36 times and each
// answer costs an ssh session: three commands would triple a wall clock that is
// already the most expensive ceiling of the pipeline (`VLLM_START_RETRIES`).
//
// The three markers below are shared with the command that produces them
// (`cc_runner::vllm_wait_probe_cmd`) rather than spelled twice: a marker that
// drifts on one side turns every probe into an `Err`, which is the failure this
// repo has shipped before under the name "dead scaffolding".

/// Section marker: the `ss -tlnp` output starts after this line.
pub const VLLM_PROBE_MARK_LISTEN: &str = "===x0ne:listen===";
/// Section marker: the `systemctl show … -p NRestarts` output.
pub const VLLM_PROBE_MARK_RESTARTS: &str = "===x0ne:restarts===";
/// Section marker: the `du -sk <model dir>` output.
pub const VLLM_PROBE_MARK_MODELDIR: &str = "===x0ne:modeldir===";

/// What one probe of the vLLM wait brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VllmWaitProbe {
    /// The `ss -tlnp` section, verbatim, to be read by
    /// `parse_ss_localhost_only`. Kept as text because the socket question has
    /// exactly one reader and it already exists.
    pub listen: String,
    /// `NRestarts` of the vLLM unit. A RISE between two probes is a crash
    /// loop; the absolute value says nothing (a VM resumed from preemption
    /// carries the restarts of its previous life).
    pub restarts: u64,
    /// Size of the model directory in KiB, as `du -sk` reports it.
    pub model_dir_kib: u64,
}

/// Split and read one combined wait probe.
///
/// SECURITY, in the sense this pipeline uses the word — fail-closed on doubt:
/// every unreadable shape is an `Err`, and the caller fails the step on it.
/// The reason is asymmetric cost, and it is worth stating because the
/// convenient reading is available here. Both figures exist to STOP a run
/// early; a defaulted "0 restarts, plenty downloaded" would restore exactly the
/// 30-minute blind wait this probe was added to end, and nobody would see it
/// happen. A false alarm, on the other hand, costs one re-run of a pipeline
/// that resumes by observation.
///
/// What it does NOT claim: these two figures are read from the machine under
/// test, so a compromised VM can forge them. It can only make itself look
/// healthier — the socket verdict that gates serving is re-read alone by
/// `ValidateEncryption`, and nothing here can turn an exposed bind into a
/// loopback one.
pub fn parse_vllm_wait_probe(stdout: &str) -> Result<VllmWaitProbe, ObsParseError> {
    const WHAT: &str = "vLLM wait probe";
    let section = |marker: &str, next: Option<&str>| marked_section(stdout, WHAT, marker, next);

    let listen = section(VLLM_PROBE_MARK_LISTEN, Some(VLLM_PROBE_MARK_RESTARTS))?;
    let restarts_text = section(VLLM_PROBE_MARK_RESTARTS, Some(VLLM_PROBE_MARK_MODELDIR))?;
    let modeldir_text = section(VLLM_PROBE_MARK_MODELDIR, None)?;

    // `systemctl show <unit> -p NRestarts` prints exactly `NRestarts=<n>`
    // (systemd 249 on Ubuntu 22.04, read 2026-08-14). The KEY is matched, not
    // just the number: `--value` output cannot be told apart from garbage, and
    // an unknown unit prints an empty value rather than failing.
    let restarts = {
        let values: Vec<&str> = restarts_text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("NRestarts="))
            .collect();
        let [value] = values[..] else {
            return Err(ObsParseError {
                what: WHAT,
                detail: format!("expected one NRestarts= line, got {}", values.len()),
            });
        };
        value.trim().parse::<u64>().map_err(|e| ObsParseError {
            what: WHAT,
            detail: format!(
                "unreadable NRestarts {:?}: {e}",
                quoted_excerpt(value.trim())
            ),
        })?
    };

    // `du -sk <dir>` prints `<kib>\t<dir>` — one line, size first.
    let model_dir_kib = {
        let lines: Vec<&str> = modeldir_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let [line] = lines[..] else {
            return Err(ObsParseError {
                what: WHAT,
                detail: format!("expected one `du -sk` line, got {}", lines.len()),
            });
        };
        let field = line.split_whitespace().next().unwrap_or("");
        field.parse::<u64>().map_err(|e| ObsParseError {
            what: WHAT,
            detail: format!("unreadable `du -sk` size {:?}: {e}", quoted_excerpt(field)),
        })?
    };

    Ok(VllmWaitProbe {
        listen,
        restarts,
        model_dir_kib,
    })
}

/// Does any firewall rule open `port` inbound?
/// (`gcloud compute firewall-rules list --format=json`)
///
/// Fixture source: gcloud JSON, fields `allowed` / `denied`, `direction`,
/// `disabled`, `allowed[].IPProtocol`, `allowed[].ports` (read 2026-08-14).
///
/// SECURITY: the port is bound to loopback, so an open firewall is not by
/// itself a breach — it is the second lock, and the gate refuses when it is
/// open. Therefore every uncertainty resolves towards `true` (exposed) or
/// `Err`, never towards a soothing `false`:
///   * `tcp` with no `ports` list means EVERY tcp port, which is how a rule
///     that looks empty is the widest one there is;
///   * `IPProtocol: "all"` covers tcp without naming it;
///   * a port spec we cannot parse is an `Err`, not a rule we skip.
///
/// A `denied` rule opens nothing, a `disabled` rule enforces nothing, and an
/// `EGRESS` rule is about outbound traffic — those three are the only ways to
/// reach `false` with a matching port.
///
/// The protocol path splits the two shapes of doubt, because they are not the
/// same doubt (review round 1, 2026-08-14: this whole paragraph used to be one
/// `continue`, and four well-formed rules that DO open the port read as
/// `false` — the exact opposite of what the two paragraphs around it promise).
///   * **A protocol we do not recognise is read as if it were tcp**, and its
///     port list still bounds it. `IPProtocol` is a name OR a number in gcloud
///     (`--allow=6:8000` opens tcp 8000 and shows up as `"6"`), so the set of
///     spellings that mean tcp is not closed and treating an unknown one as
///     harmless is how a rule opens a port silently. The named protocols that
///     cannot carry a tcp port — udp, icmp, esp, ah, sctp, ipip and their
///     numbers — are listed below and skipped: that list is what keeps the
///     `default-allow-icmp` of every GCP project from raising a false alarm.
///   * **A malformed field is an `Err`**, not `true`: "I did not understand
///     this rule" is not "this rule exposes the port", and the caller must be
///     able to tell a hole from a parser that lost its footing. Malformed
///     means: a rule that is not an object, `allowed` present but not an
///     array, `IPProtocol` missing or not a string, a port spec that is not a
///     string or does not parse. `allowed` ABSENT is not malformed — that is
///     a plain DENY rule, and it opens nothing.
///
/// Deliberate over-approximation: `sourceRanges` and `targetTags` are NOT
/// read, so a rule scoped to an internal range still counts as exposing. This
/// parser cannot know whether our VM carries the tag or whether the range
/// reaches the operator, and a hole we name that turns out to be narrow costs
/// one question — the reverse costs the model server.
pub fn parse_firewall_exposes(firewall_json: &str, port: u16) -> Result<bool, ObsParseError> {
    const WHAT: &str = "firewall-rules list";
    let v: serde_json::Value = serde_json::from_str(firewall_json).map_err(|e| ObsParseError {
        what: WHAT,
        detail: e.to_string(),
    })?;
    let rules = v.as_array().ok_or(ObsParseError {
        what: WHAT,
        detail: "expected a JSON array".into(),
    })?;
    for rule in rules {
        if !rule.is_object() {
            return Err(ObsParseError {
                what: WHAT,
                detail: "a rule is not a JSON object".into(),
            });
        }
        if rule["disabled"].as_bool() == Some(true) {
            continue;
        }
        // A missing `direction` reads as INGRESS: gcloud always emits it, and
        // the failure mode of guessing "EGRESS" would be to skip a rule.
        if matches!(rule["direction"].as_str(), Some(d) if d != "INGRESS") {
            continue;
        }
        let allowed = match &rule["allowed"] {
            // No allow clause at all: a DENY rule. It opens nothing.
            serde_json::Value::Null => continue,
            serde_json::Value::Array(entries) => entries,
            // Present but not a list — e.g. the string "tcp:8000". We cannot
            // read the rule, so we do not get to call it closed.
            _ => {
                return Err(ObsParseError {
                    what: WHAT,
                    detail: "'allowed' is present but not an array".into(),
                })
            }
        };
        for entry in allowed {
            let Some(proto) = entry["IPProtocol"].as_str() else {
                return Err(ObsParseError {
                    what: WHAT,
                    detail: "an allow entry has no string 'IPProtocol'".into(),
                });
            };
            if proto.eq_ignore_ascii_case("all") {
                return Ok(true);
            }
            if is_never_tcp(proto) {
                continue;
            }
            match entry["ports"].as_array() {
                // No list, or an empty one: every port of the protocol.
                None => return Ok(true),
                Some(specs) if specs.is_empty() => return Ok(true),
                Some(specs) => {
                    for spec in specs {
                        let spec = spec.as_str().ok_or(ObsParseError {
                            what: WHAT,
                            detail: "a port spec is not a string".into(),
                        })?;
                        if port_spec_contains(spec, port)? {
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

/// The protocols that cannot carry a tcp port, by every spelling gcloud
/// accepts for them (name or IANA number, `gcloud compute firewall-rules
/// create --allow`, read 2026-08-14). Deliberately a list of the HARMLESS
/// ones: `tcp`, `6` and anything unrecognised fall through to the port check,
/// so a spelling nobody anticipated errs towards "exposed".
fn is_never_tcp(proto: &str) -> bool {
    const NEVER_TCP: [&str; 14] = [
        "udp", "17", "icmp", "1", "icmpv6", "58", "esp", "50", "ah", "51", "ipip", "4", "sctp",
        "132",
    ];
    NEVER_TCP.iter().any(|p| proto.eq_ignore_ascii_case(p))
}

/// `"8000"` or `"7000-9000"`, the two forms gcloud emits. Anything else is an
/// error rather than a rule silently skipped.
///
/// SECURITY: `spec` is remote text — a field of a JSON document produced by
/// gcloud, i.e. by whoever can create a firewall rule in the project — and both
/// error arms quote it back into a message an operator reads to learn that the
/// pipeline REFUSED. So it goes through `quoted_excerpt` like every other
/// remote value this module quotes (finding I2 of round 1, closed there for
/// `parse_nvidia_cc_mode` and left open here because nothing called this yet).
/// Unbounded, a rule named with 50 000 characters of port spec pushes the
/// refusal that names it off the screen.
fn port_spec_contains(spec: &str, port: u16) -> Result<bool, ObsParseError> {
    let spec = spec.trim();
    let (low, high) = spec.split_once('-').unwrap_or((spec, spec));
    let parse = |s: &str| {
        s.trim().parse::<u16>().map_err(|e| ObsParseError {
            what: "firewall-rules list",
            detail: format!("unreadable port spec {:?}: {e}", quoted_excerpt(spec)),
        })
    };
    let (low, high) = (parse(low)?, parse(high)?);
    if low > high {
        return Err(ObsParseError {
            what: "firewall-rules list",
            detail: format!("inverted port range {:?}", quoted_excerpt(spec)),
        });
    }
    Ok((low..=high).contains(&port))
}

/// Which key encrypts a persistent disk at rest.
///
/// There is no `NotEncrypted` variant, and that is the honest shape rather than
/// a missing case: Compute Engine encrypts every persistent disk at rest,
/// always, with no way to turn it off (cloud.google.com/compute/docs/disks/
/// customer-managed-encryption, read 2026-08-14). The only question the API can
/// answer is WHOSE key, which is exactly what these two variants say — and what
/// the report line must not overstate into "the disk is safe from the host".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskEncryption {
    /// No `diskEncryptionKey` on the resource: the default, a Google-managed
    /// key. Protects the media (a stolen or decommissioned drive), not the VM
    /// against the platform that runs it.
    GoogleManaged,
    /// A `diskEncryptionKey` is present — a customer-managed (CMEK) or
    /// customer-supplied (CSEK) key.
    CustomerKey,
}

/// Read the encryption of one disk from
/// `gcloud compute disks describe <name> --format=json(name,diskEncryptionKey)`.
///
/// SECURITY: the `name` check is the point of taking `disk_name` at all. An
/// empty answer, a projection that dropped the field, or a describe that
/// answered about ANOTHER disk must not read as "our boot disk is fine" — that
/// is the `parse_instance_state` rule (unreadable is an error, never the
/// reassuring half of a verdict) applied to the disk.
pub fn parse_disk_encryption(
    describe_json: &str,
    disk_name: &str,
) -> Result<DiskEncryption, ObsParseError> {
    const WHAT: &str = "disks describe";
    let v: serde_json::Value = serde_json::from_str(describe_json).map_err(|e| ObsParseError {
        what: WHAT,
        detail: e.to_string(),
    })?;
    if !v.is_object() {
        return Err(ObsParseError {
            what: WHAT,
            detail: "expected a JSON object".into(),
        });
    }
    match v["name"].as_str() {
        Some(n) if n == disk_name => {}
        Some(other) => {
            return Err(ObsParseError {
                what: WHAT,
                detail: format!(
                    "answered about disk {:?}, not {disk_name:?}",
                    quoted_excerpt(other)
                ),
            })
        }
        None => {
            return Err(ObsParseError {
                what: WHAT,
                detail: "no string 'name' field".into(),
            })
        }
    }
    match &v["diskEncryptionKey"] {
        serde_json::Value::Null => Ok(DiskEncryption::GoogleManaged),
        serde_json::Value::Object(_) => Ok(DiskEncryption::CustomerKey),
        // Present but not an object: we cannot say whose key it is, so we do
        // not get to say either answer.
        _ => Err(ObsParseError {
            what: WHAT,
            detail: "'diskEncryptionKey' is present but not an object".into(),
        }),
    }
}

/// The model ids vLLM serves, from its OpenAI-compatible `GET /v1/models`.
///
/// Fixture source: vLLM's OpenAI-compatible server, `{"object":"list",
/// "data":[{"id":…,"object":"model","owned_by":"vllm",…}]}` (shape of the
/// OpenAI list endpoint, read 2026-08-14).
///
/// `Ok(vec![])` for an empty `data` is deliberate and is NOT an error: the
/// server answered, it simply has nothing loaded. That distinction belongs to
/// the runner — "vLLM is up but serving nothing" and "vLLM is not up" call for
/// different actions, and an error here would erase it.
///
/// An id that is not a string, or a missing `data`, is an error rather than a
/// shorter list: dropping an entry we failed to read would understate what the
/// machine is serving. Ids carrying control characters are refused for the
/// same reason `cc_runner::sanitize` exists (2026-08-14): they end up in the
/// report, where an embedded newline or ESC forges lines. Second layer, on
/// purpose — this one refuses, that one neutralises.
pub fn parse_vllm_models(models_json: &str) -> Result<Vec<String>, ObsParseError> {
    const WHAT: &str = "vllm /v1/models";
    let v: serde_json::Value = serde_json::from_str(models_json).map_err(|e| ObsParseError {
        what: WHAT,
        detail: e.to_string(),
    })?;
    let data = v["data"].as_array().ok_or(ObsParseError {
        what: WHAT,
        detail: "no 'data' array".into(),
    })?;
    let mut ids = Vec::with_capacity(data.len());
    for model in data {
        let id = model["id"].as_str().ok_or(ObsParseError {
            what: WHAT,
            detail: "a model has no string 'id'".into(),
        })?;
        if id.chars().any(char::is_control) {
            return Err(ObsParseError {
                what: WHAT,
                detail: "a model id carries control characters".into(),
            });
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

pub fn spent_usd(hourly_usd: f64, elapsed_secs: u64) -> f64 {
    hourly_usd * (elapsed_secs as f64) / 3600.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapStatus {
    Ok,
    Warn50,
    Warn80,
    Exceeded,
}

/// SECURITY: fail closed. A cap of 0 or less means "no budget", and no budget
/// means no spend — the gate refuses rather than treating nonsense as ∞.
///
/// The `is_finite` guard is not defensive decoration, it closes a measured
/// fail-OPEN (review round 1, 2026-08-13). `"nan".parse::<f64>()` succeeds, so
/// `--cap nan` reached here as a value for which EVERY comparison below is
/// false: `cap <= 0.0` false, `spent >= cap` false, `cap * 0.8` NaN — the gate
/// answered `Ok` forever and the ceiling was gone. `inf` disarmed it the same
/// way, while claiming in this very doc-comment to refuse exactly that. A
/// budget that cannot be compared is not a budget.
///
/// `spent` needs no such guard: it is `hourly_usd() × secs / 3600` where the
/// rate comes from a match on `CcMachine`, so it is finite by construction.
pub fn cap_status(spent: f64, cap: f64) -> CapStatus {
    if !cap.is_finite() || cap <= 0.0 || spent >= cap {
        CapStatus::Exceeded
    } else if spent >= cap * 0.8 {
        CapStatus::Warn80
    } else if spent >= cap * 0.5 {
        CapStatus::Warn50
    } else {
        CapStatus::Ok
    }
}

/// The permanent line. `≈` everywhere: GCP billing lags by hours, this is
/// price-table × uptime, and presenting it as a bill would be a lie the
/// operator eventually catches — after which they stop trusting the gate.
pub fn status_line(machine: CcMachine, spent: f64, cap: f64) -> String {
    let hourly = machine.hourly_usd();
    let left_h = if hourly > 0.0 {
        ((cap - spent).max(0.0)) / hourly
    } else {
        0.0
    };
    format!("≈ {hourly:.2} $/h · spent ≈ {spent:.2} $ · cap {cap:.0} $ · ≈ {left_h:.0} h left")
}

/// Observations in, remaining steps out. Pure, total, boring — by design.
pub fn build_plan(cfg: &CcConfig, obs: &CcObservations) -> Result<Vec<CcStep>, PlanRefused> {
    if obs.auth_account.is_none() {
        return Err(PlanRefused::NotAuthenticated);
    }
    if obs.project.is_none() {
        return Err(PlanRefused::NoProject);
    }
    if obs.image_family_exists == Some(false) {
        return Err(PlanRefused::ImageFamilyMissing {
            family: cfg.machine.vm_spec(&cfg.vm_name).image_family,
        });
    }
    // SECURITY (F2): an existing VM is only OUR VM if it is where we will aim.
    // Checked before the state dispatch below, because both `Stopped` and
    // `Running` would otherwise queue steps pointing at the wrong zone.
    let expected_zone = cfg.machine.vm_spec(&cfg.vm_name).zone;
    if let InstanceState::Running { zone } | InstanceState::Stopped { zone } = &obs.instance {
        if zone != &expected_zone {
            return Err(PlanRefused::ZoneMismatch {
                observed: zone.clone(),
                expected: expected_zone,
            });
        }
    }

    let mut plan = Vec::new();
    match &obs.instance {
        InstanceState::Absent => plan.push(CcStep::CreateVm),
        InstanceState::Stopped { .. } => plan.push(CcStep::StartVm),
        InstanceState::Running { .. } => {}
        InstanceState::Transitional { status } => {
            return Err(PlanRefused::InstanceInTransition {
                status: status.clone(),
            });
        }
    }
    plan.push(CcStep::WaitSsh);
    plan.push(CcStep::AttestTdx);
    // Everything the full machine adds hangs off AttestTdx, and hangs off it
    // AFTER it: no GPU work happens until the CPU enclave is proven, because
    // an unproven CPU means the "GPU confidential mode is ON" we would read
    // next is a claim made by an untrusted kernel about itself.
    match cfg.machine {
        CcMachine::Prover => {}
        CcMachine::FullH100 => plan.extend(full_h100_steps(obs)),
    }
    plan.push(CcStep::Report);
    Ok(plan)
}

/// What the full machine does once the CPU enclave is proven.
///
/// Two rules, deliberately asymmetric:
///
/// * an INSTALL is skipped once observed done. apt, `uv` and a systemd unit are
///   idempotent, so re-running them is harmless — but on a $3.72/h machine
///   every redundant minute is billed, and that is the whole point of
///   resume-by-detection.
/// * a PROOF is never skipped. `AttestGpu` and `ValidateEncryption` run on
///   every pass, whatever was observed, because their output is the evidence
///   the report prints. Evidence carried over from an earlier run describes a
///   machine that may since have been preempted, restarted, and rebuilt — and
///   this instance is SPOT, so that is the expected case, not the exotic one.
///   This is the prover's rule (spec 2026-08-13, "a proof is replayed") applied
///   to the GPU and to the encryption checks.
///
/// And a skip is only allowed while the instance is RUNNING — see `acquired`
/// below.
fn full_h100_steps(obs: &CcObservations) -> Vec<CcStep> {
    // SECURITY: an in-VM observation describes THIS instance, and only while
    // it runs. On `Absent` the plan is about to create a machine that has
    // never been touched; on `Stopped` and `Transitional` there was no SSH to
    // probe through. So anything other than `Running` means "nothing is
    // acquired", whatever the four fields say.
    //
    // Review I1, 2026-08-14, and it is the house defect: `CcObservations`
    // DOCUMENTED "all None unless the instance is Running" and nothing in the
    // code enforced it — an invariant stated in prose and applied by nobody.
    // Measured consequence: `Absent` plus four `Some(true)` skipped the driver,
    // vLLM and x0ne installs and ran `AttestGpu` against a brand-new VM.
    //
    // The state is read from `obs` itself, not taken as a parameter: a guard
    // whose input the caller picks is a guard the caller can accidentally
    // disarm (re-review round 1).
    let instance = &obs.instance;
    let acquired = matches!(instance, InstanceState::Running { .. });
    let done = |observed: Option<bool>| acquired && observed == Some(true);

    let mut s = Vec::new();
    if !done(obs.gpu_driver_ready) {
        s.push(CcStep::InstallGpuDriver);
    }
    if !done(obs.gpu_cc_on) {
        s.push(CcStep::CheckGpuCc);
    }
    // SECURITY: unconditional. Making this line conditional on `gpu_cc_on`
    // would let the report print a GPU verdict that was never measured on the
    // machine currently running.
    s.push(CcStep::AttestGpu);
    // SECURITY: after the proof, never before — see `CcStep::ArmGpu`. The GPU
    // ships disarmed so that a guest can refuse to use one it has not attested;
    // arming it first would throw that away for nothing.
    //
    // Unconditional for a different reason from the two proofs around it: the
    // ready state is per BOOT (run 4, 2026-08-14), so no observation of an
    // earlier run — nor of an earlier boot of this run, `InstallGpuDriver`
    // reboots — can say anything about it. The step's own `-grs` probe is the
    // acquis, and it costs one ssh round trip.
    s.push(CcStep::ArmGpu);
    // One observation retires three steps: a vLLM answering /v1/models is
    // necessarily installed, holds its weights, and is being served.
    if !done(obs.vllm_healthy) {
        s.push(CcStep::InstallVllm);
        s.push(CcStep::FetchModel);
        s.push(CcStep::ServeVllm);
    }
    // SECURITY: unconditional, same reason as AttestGpu, and it must come
    // BEFORE the health check — a serving vLLM whose port is bound to 0.0.0.0
    // is exactly the shape we refuse to call healthy.
    s.push(CcStep::ValidateEncryption);
    if !done(obs.vllm_healthy) {
        s.push(CcStep::HealthCheck);
    }
    if !done(obs.x0ne_ready) {
        s.push(CcStep::InstallX0ne);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_ready() -> CcObservations {
        CcObservations {
            auth_account: Some("operator@example.com".into()),
            project: Some("example-project-123456".into()),
            image_family_exists: Some(true),
            instance: InstanceState::Absent,
            gpu_driver_ready: None,
            gpu_cc_on: None,
            vllm_healthy: None,
            x0ne_ready: None,
        }
    }
    fn cfg() -> CcConfig {
        CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        }
    }
    fn cfg_full() -> CcConfig {
        CcConfig {
            machine: CcMachine::FullH100,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        }
    }
    /// A running full machine with nothing acquired yet — the state every
    /// resume test starts from, since the in-VM probes only exist while the
    /// instance runs.
    fn running_nothing_acquired() -> CcObservations {
        let mut o = obs_ready();
        o.instance = InstanceState::Running {
            zone: "us-central1-a".into(),
        };
        o
    }

    /// #47c: an unreadable zone is an observation ERROR, never the literal
    /// « ? » — which resurfaced as an unusable `--zone=?` lever.
    #[test]
    fn an_instance_without_a_readable_zone_is_a_parse_error_not_a_question_mark() {
        let json = r#"[{"name": "x0ne-cc", "status": "RUNNING"}]"#;
        let err = parse_instance_state(json, "x0ne-cc").unwrap_err();
        assert_eq!(err.what, "instance zone");
        let json = r#"[{"name": "x0ne-cc", "zone": "z/europe-west4-a"}]"#;
        let err = parse_instance_state(json, "x0ne-cc").unwrap_err();
        assert_eq!(err.what, "instance status");
    }

    #[test]
    fn a_fresh_project_plans_the_full_pipeline() {
        let plan = build_plan(&cfg(), &obs_ready()).unwrap();
        assert_eq!(
            plan,
            vec![
                CcStep::CreateVm,
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::Report
            ]
        );
    }

    /// Spec invariant no. 1: a stopped VM is RESTARTED, never recreated —
    /// recreating would lose the disk and pay for the boot again.
    #[test]
    fn a_stopped_vm_is_started_not_recreated() {
        let mut obs = obs_ready();
        obs.instance = InstanceState::Stopped {
            zone: "us-central1-a".into(),
        };
        let plan = build_plan(&cfg(), &obs).unwrap();
        assert_eq!(plan[0], CcStep::StartVm);
        assert!(!plan.contains(&CcStep::CreateVm));
    }

    /// Invariant no. 2: a VM already RUNNING skips creation AND start — it goes
    /// straight to waiting for SSH, then attesting. This is what makes `x0ne cc`
    /// re-runnable after a Ctrl+C without paying again for completed steps.
    #[test]
    fn a_running_vm_skips_straight_to_ssh() {
        let mut obs = obs_ready();
        obs.instance = InstanceState::Running {
            zone: "us-central1-a".into(),
        };
        let plan = build_plan(&cfg(), &obs).unwrap();
        assert_eq!(
            plan,
            vec![CcStep::WaitSsh, CcStep::AttestTdx, CcStep::Report]
        );
    }

    /// A VM in transition (STOPPING, PROVISIONING) is neither to be created nor
    /// started: the plan refuses outright rather than stacking a command on a
    /// moving state. The operator re-runs in a minute.
    #[test]
    fn a_transitional_vm_refuses_rather_than_racing() {
        let mut obs = obs_ready();
        obs.instance = InstanceState::Transitional {
            status: "STOPPING".into(),
        };
        assert!(build_plan(&cfg(), &obs).is_err());
    }

    /// A VM with the same name in ANOTHER zone is not our VM. The `zone`
    /// field was captured from day one and read by nobody (final review,
    /// F2): every following command targets the hard-coded `spec.zone`. A
    /// failure of this test would mean the plan skips creation because
    /// « the VM exists », then aims at the wrong one — the 27 SSH probes
    /// blame the boot, and the stop fails on an absent instance.
    #[test]
    fn a_vm_in_another_zone_refuses_rather_than_aiming_at_the_wrong_one() {
        for state in [
            InstanceState::Running {
                zone: "europe-west4-b".into(),
            },
            InstanceState::Stopped {
                zone: "europe-west4-b".into(),
            },
        ] {
            let mut obs = obs_ready();
            obs.instance = state.clone();
            let e = build_plan(&cfg(), &obs).unwrap_err();
            assert_eq!(
                e,
                PlanRefused::ZoneMismatch {
                    observed: "europe-west4-b".into(),
                    expected: "us-central1-a".into()
                },
                "state {state:?}"
            );
            // Both zones are named, and so is the lever: without the observed
            // zone, the operator cannot even delete the intruder.
            let m = e.message();
            assert!(m.contains("europe-west4-b"), "{m}");
            assert!(m.contains("us-central1-a"), "{m}");
            assert!(
                m.contains("gcloud compute instances delete x0ne-cc --zone=europe-west4-b"),
                "{m}"
            );
        }
        // The same zone keeps planning normally.
        let mut obs = obs_ready();
        obs.instance = InstanceState::Stopped {
            zone: "us-central1-a".into(),
        };
        assert_eq!(build_plan(&cfg(), &obs).unwrap()[0], CcStep::StartVm);
    }

    /// The preflight fails BEFORE any spending, and each refusal names the
    /// command the operator must run — a refusal without a lever costs one
    /// more round trip (lesson of the `add "?" to allowed_dirs` hint).
    #[test]
    fn preflight_failures_name_the_operator_command() {
        let mut obs = obs_ready();
        obs.auth_account = None;
        let e = build_plan(&cfg(), &obs).unwrap_err();
        assert!(
            e.message().contains("gcloud auth login"),
            "got: {}",
            e.message()
        );

        let mut obs = obs_ready();
        obs.project = None;
        let e = build_plan(&cfg(), &obs).unwrap_err();
        assert!(
            e.message().contains("gcloud config set project"),
            "got: {}",
            e.message()
        );

        let mut obs = obs_ready();
        obs.image_family_exists = Some(false);
        let e = build_plan(&cfg(), &obs).unwrap_err();
        assert!(
            e.message().contains("gcloud compute images list"),
            "got: {}",
            e.message()
        );
    }

    /// The prover is the C3 from the owner's console (2026-08-13,
    /// ~0.22 $/h); the resulting VmSpec must pass the existing validation —
    /// otherwise the wizard would display a plan that cannot be created.
    #[test]
    fn the_prover_vm_spec_is_valid_and_tdx() {
        let spec = CcMachine::Prover.vm_spec("x0ne-cc");
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(
            spec.confidential,
            Some(crate::provision::ConfidentialTee::Tdx)
        );
        assert_eq!(spec.machine_type, "c3-standard-4");
        assert!(spec.accelerators.is_empty());
    }

    #[test]
    fn the_full_machine_is_the_h100_shape() {
        let spec = CcMachine::FullH100.vm_spec("x0ne-cc");
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(spec.machine_type, "a3-highgpu-1g");
        assert_eq!(
            spec.provisioning_model,
            crate::provision::ProvisioningModel::Spot
        );
        // 22.04 is what the GCP confidential-GPU procedure prescribes, and the
        // family name the image probe will look up; 50 GB because the weights
        // live in tmpfs, so a bigger disk is pure billed waste (spec 2026-08-14).
        assert_eq!(spec.image_family, "ubuntu-2204-lts");
        assert_eq!(spec.boot_disk_gb, 50);
    }

    /// SECURITY: the confidential H100 only exists as SPOT (GCP doc, 2026-08-14).
    /// A Standard full spec would be refused at create time after billing setup —
    /// or worse, silently create a non-confidential machine at 3x the price.
    #[test]
    fn the_full_machine_is_spot_tdx_and_lives_in_a_supported_zone() {
        let spec = CcMachine::FullH100.vm_spec("x0ne-cc");
        let argv = spec.to_gcloud_argv();
        assert!(argv.contains(&"--provisioning-model=SPOT".to_string()));
        assert!(argv.contains(&"--confidential-compute-type=TDX".to_string()));
        assert!(argv.contains(&"--zone=us-central1-a".to_string()));
        assert!(argv.iter().any(|a| a.contains("a3-highgpu-1g")));
        // And the preemption action is WRITTEN, not inherited: the disk holds
        // all completed work, and resume-by-observation relies on it surviving.
        assert!(argv.contains(&"--instance-termination-action=STOP".to_string()));
    }

    /// The displayed price is also the one the budget gate consumes
    /// (`spent_usd(hourly_usd(), …)`). An on-demand rate left on a machine
    /// that only exists as SPOT does more than misinform: it makes the gate
    /// cut off at a third of the cap actually spent.
    #[test]
    fn the_full_machine_is_priced_at_the_spot_rate_not_the_on_demand_one() {
        assert!((CcMachine::FullH100.hourly_usd() - 3.72).abs() < 1e-9);
        assert!(status_line(CcMachine::FullH100, 0.0, 25.0).contains("3.72"));
    }

    /// The order is the security guarantee of full mode: no GPU work before
    /// the CPU enclave is proven (otherwise « the GPU CC mode is ON » is a
    /// claim by an unattested kernel about itself), no model on an unproven
    /// GPU, and the streams validated before health is declared.
    #[test]
    fn the_full_plan_stacks_gpu_model_and_x0ne_steps_after_the_cpu_attestation() {
        // VM absent, nothing acquired.
        let plan = build_plan(&cfg_full(), &obs_ready()).unwrap();
        let idx = |s: &CcStep| {
            plan.iter()
                .position(|p| p == s)
                .unwrap_or_else(|| panic!("{s:?} missing from {plan:?}"))
        };
        assert!(
            idx(&CcStep::AttestTdx) < idx(&CcStep::AttestGpu),
            "no GPU work before the CPU enclave is proven"
        );
        assert!(
            idx(&CcStep::InstallGpuDriver) > idx(&CcStep::AttestTdx),
            "no GPU work before the CPU enclave is proven"
        );
        assert!(
            idx(&CcStep::AttestGpu) < idx(&CcStep::InstallVllm),
            "no model on an unproven GPU"
        );
        // SECURITY: arm AFTER attesting, never before. The confidential GPU
        // arrives unarmed precisely so the guest can refuse to hand it work
        // before attesting it; arming first would throw that option away for
        // nothing. A failure here means the pipeline turns the GPU on based on
        // nothing.
        assert!(
            idx(&CcStep::AttestGpu) < idx(&CcStep::ArmGpu),
            "attest, THEN arm — never the other way round"
        );
        assert!(
            idx(&CcStep::ArmGpu) < idx(&CcStep::InstallVllm),
            "CUDA answers Error 802 on a GPU nobody armed (run 4, 2026-08-14)"
        );
        assert!(idx(&CcStep::ValidateEncryption) < idx(&CcStep::HealthCheck));
        assert_eq!(*plan.last().unwrap(), CcStep::Report);
        // The whole list, hard-coded: this is what the human approves.
        assert_eq!(
            plan,
            vec![
                CcStep::CreateVm,
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::InstallGpuDriver,
                CcStep::CheckGpuCc,
                CcStep::AttestGpu,
                CcStep::ArmGpu,
                CcStep::InstallVllm,
                CcStep::FetchModel,
                CcStep::ServeVllm,
                CcStep::ValidateEncryption,
                CcStep::HealthCheck,
                CcStep::InstallX0ne,
                CcStep::Report,
            ]
        );
    }

    /// Resume-by-completed-work, applied to the full steps. An install already
    /// done is not redone — on a 3.72 $/h machine every redundant minute is
    /// billed. But a PROOF is always replayed: a failure of the second half
    /// of this test would mean « GPU proven » or « streams encrypted » is
    /// printed based on an observation from a previous run, that is, of a
    /// SPOT machine that may well have been preempted, restarted and rebuilt
    /// in the meantime.
    #[test]
    fn acquired_gpu_and_vllm_steps_are_skipped_on_resume() {
        let mut obs = running_nothing_acquired();
        obs.gpu_driver_ready = Some(true);
        obs.gpu_cc_on = Some(true);
        obs.vllm_healthy = Some(true);
        obs.x0ne_ready = Some(true);
        let plan = build_plan(&cfg_full(), &obs).unwrap();
        // All completed work is skipped, the two proofs remain.
        assert_eq!(
            plan,
            vec![
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::AttestGpu,
                // Never acquired, never skipped: the GPU « ready » state is
                // PER BOOT (run 4, 2026-08-14), so no observation from a
                // previous run speaks to it. Its `-grs` probe lives in the step.
                CcStep::ArmGpu,
                CcStep::ValidateEncryption,
                CcStep::Report,
            ]
        );

        // The control that keeps the assertions above from being true for the
        // wrong reason: with nothing acquired, these same steps ARE planned.
        let fresh = build_plan(&cfg_full(), &running_nothing_acquired()).unwrap();
        for planned in [
            CcStep::InstallGpuDriver,
            CcStep::CheckGpuCc,
            CcStep::InstallVllm,
            CcStep::FetchModel,
            CcStep::ServeVllm,
            CcStep::HealthCheck,
            CcStep::InstallX0ne,
        ] {
            assert!(
                fresh.contains(&planned),
                "{planned:?} missing from {fresh:?}"
            );
        }

        // And each flag is read SEPARATELY, removing only what belongs to it.
        // This is the counter to the repository's recurring defect
        // (« dead scaffolding »): a field added to `CcObservations` and
        // never consulted would pass the two blocks above without a flinch.
        let one = |set: fn(&mut CcObservations)| {
            let mut o = running_nothing_acquired();
            set(&mut o);
            build_plan(&cfg_full(), &o).unwrap()
        };
        let p = one(|o| o.gpu_driver_ready = Some(true));
        assert!(
            !p.contains(&CcStep::InstallGpuDriver),
            "gpu_driver_ready not read: {p:?}"
        );
        assert!(
            p.contains(&CcStep::CheckGpuCc),
            "it removes only the driver install: {p:?}"
        );

        let p = one(|o| o.gpu_cc_on = Some(true));
        assert!(
            !p.contains(&CcStep::CheckGpuCc),
            "gpu_cc_on not read: {p:?}"
        );
        assert!(
            p.contains(&CcStep::AttestGpu),
            "an attestation is replayed: {p:?}"
        );
        assert!(
            p.contains(&CcStep::InstallGpuDriver),
            "it removes only the mode read: {p:?}"
        );

        let p = one(|o| o.vllm_healthy = Some(true));
        for gone in [
            CcStep::InstallVllm,
            CcStep::FetchModel,
            CcStep::ServeVllm,
            CcStep::HealthCheck,
        ] {
            assert!(
                !p.contains(&gone),
                "vllm_healthy must remove {gone:?}: {p:?}"
            );
        }
        assert!(
            p.contains(&CcStep::ValidateEncryption),
            "a check is replayed: {p:?}"
        );
        assert!(p.contains(&CcStep::InstallGpuDriver) && p.contains(&CcStep::InstallX0ne));

        let p = one(|o| o.x0ne_ready = Some(true));
        assert!(
            !p.contains(&CcStep::InstallX0ne),
            "x0ne_ready not read: {p:?}"
        );
        assert!(
            p.contains(&CcStep::ServeVllm),
            "it removes only the x0ne install: {p:?}"
        );
    }

    /// Review I1 (2026-08-14), measured by the reviewer: an in-VM probe
    /// describes THIS instance, and only while it runs. `Absent` plus four
    /// `Some(true)` skipped the driver, vLLM and x0ne installs, then ran
    /// `AttestGpu` on a machine that had just been created and had never
    /// seen an NVIDIA package. The `CcObservations` doc-comment PROMISED
    /// « all None unless Running » and nothing enforced it: an invariant in
    /// prose on nobody's path, the in-house defect. A failure here means a
    /// fresh VM inherits another one's completed work.
    #[test]
    fn a_vm_about_to_be_created_ignores_in_vm_observations() {
        let acquired_everything = |instance: InstanceState| {
            let mut o = obs_ready();
            o.instance = instance;
            o.gpu_driver_ready = Some(true);
            o.gpu_cc_on = Some(true);
            o.vllm_healthy = Some(true);
            o.x0ne_ready = Some(true);
            build_plan(&cfg_full(), &o).unwrap()
        };

        // To be created: the full plan, all installs included.
        assert_eq!(
            acquired_everything(InstanceState::Absent),
            vec![
                CcStep::CreateVm,
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::InstallGpuDriver,
                CcStep::CheckGpuCc,
                CcStep::AttestGpu,
                CcStep::ArmGpu,
                CcStep::InstallVllm,
                CcStep::FetchModel,
                CcStep::ServeVllm,
                CcStep::ValidateEncryption,
                CcStep::HealthCheck,
                CcStep::InstallX0ne,
                CcStep::Report,
            ]
        );

        // Stopped: nobody could probe over SSH, so nothing is acquired either —
        // only the first step changes (StartVm instead of CreateVm).
        let stopped = acquired_everything(InstanceState::Stopped {
            zone: "us-central1-a".into(),
        });
        assert_eq!(stopped[0], CcStep::StartVm);
        for planned in [
            CcStep::InstallGpuDriver,
            CcStep::CheckGpuCc,
            CcStep::InstallVllm,
            CcStep::FetchModel,
            CcStep::ServeVllm,
            CcStep::HealthCheck,
            CcStep::InstallX0ne,
        ] {
            assert!(
                stopped.contains(&planned),
                "{planned:?} missing from {stopped:?}"
            );
        }

        // The control: on a VM that is RUNNING, the same observations do skip
        // these steps. Without it, this test would pass just as well if the
        // four fields were no longer read at all.
        let running = acquired_everything(InstanceState::Running {
            zone: "us-central1-a".into(),
        });
        assert!(!running.contains(&CcStep::InstallGpuDriver), "{running:?}");
        assert!(!running.contains(&CcStep::InstallX0ne), "{running:?}");
    }

    /// Regression. The prover ran for real on 2026-08-14 (ENCLAVE PROVEN):
    /// its plan is a field result, not a draft. The full steps stack behind
    /// `CcMachine::FullH100` and behind nothing else — a failure here means a
    /// 0.24 $/h machine started planning an NVIDIA driver install on a VM
    /// that has no GPU.
    #[test]
    fn the_prover_plan_is_unchanged_by_the_new_steps() {
        let absent = build_plan(&cfg(), &obs_ready()).unwrap();
        assert_eq!(
            absent,
            vec![
                CcStep::CreateVm,
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::Report
            ]
        );

        let mut stopped = obs_ready();
        stopped.instance = InstanceState::Stopped {
            zone: "us-central1-a".into(),
        };
        assert_eq!(
            build_plan(&cfg(), &stopped).unwrap(),
            vec![
                CcStep::StartVm,
                CcStep::WaitSsh,
                CcStep::AttestTdx,
                CcStep::Report
            ]
        );

        // Including when the full probes have, for whatever reason, brought
        // back values: on the prover they change nothing.
        let mut running = running_nothing_acquired();
        running.gpu_driver_ready = Some(false);
        running.vllm_healthy = Some(true);
        assert_eq!(
            build_plan(&cfg(), &running).unwrap(),
            vec![CcStep::WaitSsh, CcStep::AttestTdx, CcStep::Report]
        );
    }

    // Fixtures: real gcloud --format=json output (measured on example-project-123456, 2026-08-13).
    const INSTANCES_ONE_RUNNING: &str = r#"[{"name":"x0ne-cc","status":"RUNNING",
        "zone":"https://www.googleapis.com/compute/v1/projects/example-project-123456/zones/us-central1-a"}]"#;
    const INSTANCES_ONE_TERMINATED: &str = r#"[{"name":"x0ne-cc","status":"TERMINATED",
        "zone":"https://www.googleapis.com/compute/v1/projects/example-project-123456/zones/us-central1-a"}]"#;
    const AUTH_ONE_ACTIVE: &str = r#"[{"account":"operator@example.com","status":"ACTIVE"}]"#;

    #[test]
    fn instance_state_reads_running_and_terminated_and_absent() {
        assert_eq!(
            parse_instance_state(INSTANCES_ONE_RUNNING, "x0ne-cc").unwrap(),
            InstanceState::Running {
                zone: "us-central1-a".into()
            }
        );
        assert_eq!(
            parse_instance_state(INSTANCES_ONE_TERMINATED, "x0ne-cc").unwrap(),
            InstanceState::Stopped {
                zone: "us-central1-a".into()
            }
        );
        assert_eq!(
            parse_instance_state("[]", "x0ne-cc").unwrap(),
            InstanceState::Absent
        );
        // A different VM in the project is NOT our VM.
        assert_eq!(
            parse_instance_state(INSTANCES_ONE_RUNNING, "autre-vm").unwrap(),
            InstanceState::Absent
        );
    }

    #[test]
    fn an_unknown_status_is_transitional_never_guessed() {
        let j = r#"[{"name":"x0ne-cc","status":"REPAIRING","zone":"z/us-central1-a"}]"#;
        assert!(matches!(
            parse_instance_state(j, "x0ne-cc").unwrap(),
            InstanceState::Transitional { .. }
        ));
    }

    /// Garbage in → error out, never an invented state. Creating a VM because
    /// the list was misread = paying twice or overwriting a disk.
    #[test]
    fn garbage_json_is_an_error_not_absent() {
        assert!(parse_instance_state("not json", "x0ne-cc").is_err());
    }

    #[test]
    fn auth_account_reads_the_active_account() {
        assert_eq!(
            parse_auth_account(AUTH_ONE_ACTIVE).as_deref(),
            Some("operator@example.com")
        );
        assert_eq!(parse_auth_account("[]"), None);
        assert_eq!(parse_auth_account("garbage"), None);
    }

    #[test]
    fn image_family_exists_is_a_nonempty_list() {
        assert!(parse_image_family_exists(r#"[{"name":"ubuntu-2604-x"}]"#));
        assert!(!parse_image_family_exists("[]"));
        assert!(!parse_image_family_exists("garbage"));
    }

    // ————— In-VM probes. The dated source of each format is at the
    // top of the matching parser; here the EXACT shape is kept, columns and
    // spacing included, because that is precisely what the parser reads.

    /// GCP, "Verify Confidential Computing is enabled on GPUs" (read 2026-08-14).
    const NVIDIA_CC_ON: &str = "CC status: ON\n";
    const NVIDIA_CC_OFF: &str = "CC status: OFF\n";
    /// A GPU (or a driver) without the feature: no `CC status:` line.
    const NVIDIA_CC_UNSUPPORTED: &str = "Confidential Compute is not supported on this GPU.\n";

    #[test]
    fn nvidia_cc_mode_reads_on_and_off() {
        assert!(parse_nvidia_cc_mode(NVIDIA_CC_ON).unwrap());
        assert!(!parse_nvidia_cc_mode(NVIDIA_CC_OFF).unwrap());
        // As ssh sometimes returns it, in CRLF and with the indentation of a -q.
        assert!(parse_nvidia_cc_mode("   CC status: ON\r\n").unwrap());
    }

    /// SECURITY: same rule as parse_instance_state (prover review, 2026-08-13):
    /// garbage from the remote side must become an error, never a verdict.
    /// A hostile VM that prints "CC status: ON" garbage into an unrelated field
    /// must not flip the parser.
    #[test]
    fn nvidia_cc_mode_garbage_is_an_error_not_a_verdict() {
        assert!(parse_nvidia_cc_mode("").is_err());
        assert!(parse_nvidia_cc_mode("command not found").is_err());
        assert!(parse_nvidia_cc_mode(NVIDIA_CC_UNSUPPORTED).is_err());
        // The marker IN THE MIDDLE of a line is not a verdict. The case that
        // matters is the one where what follows the marker is a VALID value:
        // there, only anchoring at line start makes the difference, and a
        // `find("CC status:")` answers Ok(true). This is the shape a motd, an
        // ssh banner or a log line interleaved in the stream can have.
        assert!(
            parse_nvidia_cc_mode("Last login: Thu Aug 14 09:12 -- CC status: ON").is_err(),
            "a line that ENDS with the marker and a valid value \
             is a verdict only to an unanchored parser"
        );
        // The next two do NOT test anchoring (measured at review round 1:
        // they pass under an unanchored parser, because what follows the
        // marker there is an invalid value anyway — « ON) | » and
        // « ON: command not found »). They pin the strict reading of the
        // VALUE, on two real shapes: an nvidia-smi table and an error
        // message that quotes the command. Anchoring is carried by the
        // assertion above and by it alone.
        assert!(parse_nvidia_cc_mode("| 0 NVIDIA H100 80GB (CC status: ON) |").is_err());
        assert!(
            parse_nvidia_cc_mode("bash: line 1: CC status: ON: command not found").is_err(),
            "an error message that quotes the expected output is not the output"
        );
        // And a value that cannot be read stays unreadable.
        assert!(parse_nvidia_cc_mode("CC status: MAYBE").is_err());
        assert!(parse_nvidia_cc_mode("CC status:").is_err());
        // An ANSI escape in front of the marker does not decorate it, it
        // makes it unreadable: `trim()` only eats whitespace, on purpose.
        assert!(parse_nvidia_cc_mode("\u{1b}[32mCC status: ON\u{1b}[0m").is_err());
    }

    /// REAL shapes of `nvidia-smi conf-compute -grs`, both sourced:
    /// « ready » comes from the GCP page « Verify Confidential Computing is
    /// enabled on GPUs », « not-ready » from NVIDIA/nvtrust#37 (read
    /// 2026-08-14).
    const GPU_READY: &str = "Confidential Compute GPUs Ready state: ready\n";
    const GPU_NOT_READY: &str = "Confidential Compute GPUs Ready state: not-ready\n";

    /// The GPU armed state is read, in both directions.
    ///
    /// A failure of the second half would mean an unarmed GPU passes for
    /// armed: the pipeline would skip the `-srs 1`, and vLLM would go back
    /// into the 46 restarts on CUDA Error 802 measured in run 4 (2026-08-14).
    #[test]
    fn the_gpu_ready_state_reads_armed_and_disarmed() {
        assert!(parse_gpu_ready_state(GPU_READY).unwrap());
        assert!(!parse_gpu_ready_state(GPU_NOT_READY).unwrap());
        // As ssh sometimes returns them: CRLF, case and indentation.
        assert!(
            parse_gpu_ready_state("  CONFIDENTIAL COMPUTE GPUS READY STATE: Ready\r\n").unwrap()
        );
        assert!(!parse_gpu_ready_state("Confidential Compute GPUs Ready state: 0").unwrap());
    }

    /// SECURITY: unreadable stays unreadable — never « armed ».
    ///
    /// The two errors do not cost the same. Reading « unarmed » from noise
    /// replays an idempotent `-srs 1`; reading « armed » from noise skips
    /// arming and sends the vLLM loop back into the crash-loop it just left.
    /// This is why there is no path from `Err` to `true`.
    #[test]
    fn an_unreadable_ready_state_is_an_error_not_an_armed_gpu() {
        for garbage in [
            "",
            "bash: nvidia-smi: command not found",
            "Confidential Compute GPUs Ready state: perhaps",
            "Confidential Compute GPUs Ready state:",
            // Anchoring: the marker in the middle of a line, followed by a
            // VALID value — the only shape an unanchored parser reads as a
            // verdict (motd, ssh banner, interleaved log line).
            "Last login: Thu -- Confidential Compute GPUs Ready state: ready",
        ] {
            assert!(parse_gpu_ready_state(garbage).is_err(), "{garbage:?}");
        }
        // Two lines that contradict each other: an error, not a vote.
        let both = format!("{GPU_NOT_READY}{GPU_READY}");
        assert!(
            parse_gpu_ready_state(&both).is_err(),
            "appending `ready` after a real `not-ready` must not win"
        );
    }

    /// The vLLM wait probe returns THREE answers, and all three are read.
    ///
    /// This is the run 4 measurement (2026-08-14) put into code: 30 minutes
    /// and 1.26 $ spent probing a port while the unit restarted 46 times
    /// behind it. An unbound port is consistent with a server that is working
    /// AND with a server that is dying; only the two other columns tell them
    /// apart.
    #[test]
    fn one_wait_probe_carries_the_socket_the_restarts_and_the_download() {
        let stdout = format!(
            "{VLLM_PROBE_MARK_LISTEN}\n\
             State  Recv-Q Send-Q Local Address:Port\n\
             LISTEN 0      128        0.0.0.0:22       0.0.0.0:*\n\
             {VLLM_PROBE_MARK_RESTARTS}\n\
             NRestarts=46\n\
             {VLLM_PROBE_MARK_MODELDIR}\n\
             4194304\t/dev/shm/x0ne-models\n"
        );
        let probe = parse_vllm_wait_probe(&stdout).unwrap();
        assert_eq!(probe.restarts, 46);
        assert_eq!(probe.model_dir_kib, 4194304);
        // The socket section goes as-is to its only reader.
        assert!(
            parse_ss_localhost_only(&probe.listen, 8000).is_err(),
            "nothing listens on 8000"
        );
        assert!(probe.listen.contains("0.0.0.0:22"), "{:?}", probe.listen);
    }

    /// SECURITY (fail-closed): an unreadable probe is an error, never
    /// « zero restarts, nothing to report ».
    ///
    /// The two figures exist only to STOP a run early. A reassuring default
    /// neutralizes them silently and brings back the blind 30-minute wait
    /// this probe exists to end — the in-house defect, in a new costume. A
    /// failure here means a VM that answers anything gets the patience given
    /// to a VM that is working.
    #[test]
    fn an_unreadable_wait_probe_is_an_error_never_a_reassuring_default() {
        let ok = format!(
            "{VLLM_PROBE_MARK_LISTEN}\n{VLLM_PROBE_MARK_RESTARTS}\nNRestarts=0\n\
             {VLLM_PROBE_MARK_MODELDIR}\n12\t/dev/shm/x0ne-models\n"
        );
        assert!(
            parse_vllm_wait_probe(&ok).is_ok(),
            "the control case must pass"
        );

        for garbage in [
            String::new(),
            "bash: ss: command not found".into(),
            // A missing marker.
            format!("{VLLM_PROBE_MARK_LISTEN}\n{VLLM_PROBE_MARK_RESTARTS}\nNRestarts=0\n"),
            // Empty NRestarts: what `systemctl show` prints for an unknown
            // unit, exiting 0.
            format!(
                "{VLLM_PROBE_MARK_LISTEN}\n{VLLM_PROBE_MARK_RESTARTS}\nNRestarts=\n\
                 {VLLM_PROBE_MARK_MODELDIR}\n12\tx\n"
            ),
            // Silent `du`: the directory is gone.
            format!(
                "{VLLM_PROBE_MARK_LISTEN}\n{VLLM_PROBE_MARK_RESTARTS}\nNRestarts=0\n\
                 {VLLM_PROBE_MARK_MODELDIR}\n"
            ),
            // The same marker twice: choosing would be guessing.
            format!(
                "{VLLM_PROBE_MARK_LISTEN}\n{VLLM_PROBE_MARK_RESTARTS}\nNRestarts=0\n\
                 {VLLM_PROBE_MARK_RESTARTS}\nNRestarts=9\n{VLLM_PROBE_MARK_MODELDIR}\n12\tx\n"
            ),
        ] {
            assert!(parse_vllm_wait_probe(&garbage).is_err(), "{garbage:?}");
        }
    }

    /// The tmpfs probe reads both answers, as the VM returns them.
    ///
    /// Fixture source: `busctl get-property … RemoveIPC` and
    /// `df --output=avail -k /dev/shm`, read on x0ne-cc on 2026-08-14.
    #[test]
    fn the_tmpfs_probe_reads_the_purge_state_and_the_free_space() {
        let stdout = format!(
            "{TMPFS_PROBE_MARK_REMOVEIPC}\nb true\n{TMPFS_PROBE_MARK_AVAIL}\n     Avail\n 122683392\n"
        );
        let probe = parse_tmpfs_probe(&stdout).unwrap();
        assert!(probe.remove_ipc, "b true = logind still purges");
        assert_eq!(probe.avail_kib, 122_683_392);

        let disarmed = stdout.replace("b true", "b false");
        assert!(!parse_tmpfs_probe(&disarmed).unwrap().remove_ipc);
    }

    /// SECURITY (fail-closed): unreadable is never « the purge is disarmed ».
    ///
    /// The two errors do not cost the same. Reading « still armed » from
    /// noise replays an idempotent step; reading « disarmed » from noise
    /// starts a 15 GB download into a directory the next ssh disconnection
    /// wipes, at 3.72 $/h, and the symptom that surfaces is a slow network.
    /// This is why no path from `Err` leads to `false`.
    #[test]
    fn an_unreadable_tmpfs_probe_is_an_error_never_a_disarmed_purge() {
        let ok =
            format!("{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n");
        assert!(parse_tmpfs_probe(&ok).is_ok(), "the control case must pass");

        for garbage in [
            String::new(),
            "bash: busctl: command not found".into(),
            // A missing marker.
            format!("{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n"),
            // The answer without its type: a bare `true` can come from any
            // command, and this line decides 15 GB.
            format!("{TMPFS_PROBE_MARK_REMOVEIPC}\nfalse\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n"),
            // A boolean nobody can read.
            format!("{TMPFS_PROBE_MARK_REMOVEIPC}\nb maybe\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n"),
            // Policy error: busctl writes to stderr and exits 1, the section
            // stays empty.
            format!("{TMPFS_PROBE_MARK_REMOVEIPC}\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n"),
            // The same marker twice: choosing would be guessing.
            format!(
                "{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n{TMPFS_PROBE_MARK_REMOVEIPC}\nb true\n\
                 {TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n"
            ),
            // Two answers in the same section.
            format!(
                "{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\nb true\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n12\n"
            ),
        ] {
            assert!(parse_tmpfs_probe(&garbage).is_err(), "{garbage:?}");
        }
    }

    /// SECURITY (finding I2, review round 1): an error's `detail` quotes
    /// remote text, and an unbounded quote is a way to erase the refusal
    /// that contains it.
    ///
    /// Measured: `CC status: ` followed by 50,000 « A » produced a 50,000
    /// character message, which pushed its own verdict off the screen.
    /// `sanitize` can do nothing about it — a wall of « A » is harmless
    /// character by character. A failure here means the VM decides the
    /// length of the messages by which it is refused.
    #[test]
    fn an_unreadable_cc_status_value_is_quoted_but_bounded() {
        let hostile = format!("CC status: {}", "A".repeat(50_000));
        let e = parse_nvidia_cc_mode(&hostile).unwrap_err();
        assert!(
            e.detail.chars().count() <= EXCERPT_MAX_CHARS + 40,
            "a refusal must fit on screen, {} characters",
            e.detail.chars().count()
        );
        assert!(
            e.detail.contains('…'),
            "the truncation must be visible: {}",
            e.detail
        );
        // A short value is quoted WHOLE: the bound must not cut off the
        // useful information of the normal case.
        let e = parse_nvidia_cc_mode("CC status: MAYBE").unwrap_err();
        assert!(e.detail.contains("MAYBE"), "{}", e.detail);
        assert!(!e.detail.contains('…'), "{}", e.detail);
    }

    /// Two lines that contradict each other are not put to a vote. The
    /// scenario: the real output says OFF, something appends an ON line after
    /// it. A parser that keeps the last (or first) value read leaves the
    /// choice of verdict to whoever writes last.
    #[test]
    fn two_disagreeing_cc_status_lines_are_an_error_not_a_vote() {
        assert!(parse_nvidia_cc_mode("CC status: OFF\nCC status: ON\n").is_err());
        assert!(parse_nvidia_cc_mode("CC status: ON\nCC status: OFF\n").is_err());
        // Two agreeing lines stay readable (multi-GPU).
        assert!(parse_nvidia_cc_mode("CC status: ON\nCC status: ON\n").unwrap());
    }

    /// Real `nvidia-smi` header (driver 550.x, shape stable for years): it
    /// is the only shape that carries `Driver Version:`.
    const NVIDIA_SMI_TABLE: &str = "\
Thu Aug 14 09:12:44 2026
+-----------------------------------------------------------------------------------------+
| NVIDIA-SMI 550.90.07              Driver Version: 550.90.07      CUDA Version: 12.4      |
|-----------------------------------------+------------------------+----------------------+
| GPU  Name                 Persistence-M | Bus-Id          Disp.A | Volatile Uncorr. ECC |
|   0  NVIDIA H100 80GB HBM3          On  |   00000000:00:04.0 Off |                    0 |
+-----------------------------------------+------------------------+----------------------+
";

    /// The trap: the failure message CONTAINS `NVIDIA-SMI`. A
    /// `contains("NVIDIA-SMI")` therefore declares the driver ready exactly
    /// when it is not — and the plan skips `InstallGpuDriver`, then fails three
    /// steps later on an error that no longer mentions the driver.
    #[test]
    fn nvidia_smi_ready_is_not_fooled_by_the_driver_failure_message() {
        assert!(parse_nvidia_smi_ready(NVIDIA_SMI_TABLE));
        assert!(!parse_nvidia_smi_ready(
            "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver. \
             Make sure that the latest NVIDIA driver is installed and running."
        ));
        assert!(!parse_nvidia_smi_ready(""));
        assert!(!parse_nvidia_smi_ready(
            "bash: nvidia-smi: command not found"
        ));
    }

    /// util-linux `swapon --show` with an active swapfile. Without swap, the
    /// output is empty — not even the header.
    const SWAPON_ONE_FILE: &str = "\
NAME      TYPE      SIZE USED PRIO
/swapfile file        2G   0B   -2
";

    #[test]
    fn swap_is_absent_only_when_nothing_at_all_is_printed() {
        assert!(parse_swapon_empty(""));
        assert!(parse_swapon_empty("\n"));
        assert!(!parse_swapon_empty(SWAPON_ONE_FILE));
        // Even a lone header counts as « there is swap »: `true` is the only
        // answer that can let weights go to disk, it is earned by a literally
        // empty output.
        assert!(!parse_swapon_empty("NAME      TYPE      SIZE USED PRIO\n"));
    }

    /// `ss -tlnp` (iproute2, columns read 2026-08-14) on the full VM as
    /// intended: vLLM on loopback v4 and v6, public sshd on 22.
    const SS_VLLM_LOOPBACK: &str = r#"State  Recv-Q Send-Q   Local Address:Port    Peer Address:Port Process
LISTEN 0      128          127.0.0.1:8000         0.0.0.0:*     users:(("vllm",pid=2011,fd=5))
LISTEN 0      128              [::1]:8000            [::]:*     users:(("vllm",pid=2011,fd=6))
LISTEN 0      128            0.0.0.0:22             0.0.0.0:*     users:(("sshd",pid=901,fd=3))
"#;

    #[test]
    fn ss_reads_a_loopback_only_bind() {
        assert!(parse_ss_localhost_only(SS_VLLM_LOOPBACK, 8000).unwrap());
        // Port 22, for its part, is indeed public in the same output.
        assert!(!parse_ss_localhost_only(SS_VLLM_LOOPBACK, 22).unwrap());
    }

    // `assert_eq!(…, false)` rather than `assert!(!…)`: this test is taken
    // word for word from the requirements (task-2-brief.md), and keeping it
    // comparable to the spec matters more than pleasing the lint.
    #[allow(clippy::bool_assert_comparison)]
    #[test]
    fn ss_output_with_a_public_bind_fails_the_localhost_check() {
        let out = "LISTEN 0 128 0.0.0.0:8000 users:((\"vllm\",pid=1,fd=3))";
        assert_eq!(parse_ss_localhost_only(out, 8000).unwrap(), false);
    }

    /// SECURITY: the four listen shapes, including the two public ones whose
    /// spelling makes them easy to mistake for loopback. `[::]` and `[::1]`
    /// differ by one character and have opposite verdicts; `*` is the
    /// wildcard `ss` displays for « all interfaces ». A failure here = an
    /// unauthenticated model server declared local while it listens on the
    /// VM's public IP.
    #[test]
    fn every_wildcard_bind_reads_as_public_including_the_ipv6_ones() {
        let cases = [
            ("LISTEN 0 128 127.0.0.1:8000 0.0.0.0:*", true),
            ("LISTEN 0 128 127.0.0.53:8000 0.0.0.0:*", true), // all of 127.0.0.0/8
            ("LISTEN 0 128 [::1]:8000 [::]:*", true),
            ("LISTEN 0 128 0.0.0.0:8000 0.0.0.0:*", false),
            ("LISTEN 0 128 [::]:8000 [::]:*", false),
            ("LISTEN 0 511 *:8000 *:*", false),
            ("LISTEN 0 128 10.128.0.7:8000 0.0.0.0:*", false), // the internal IP too
            // Accepted and documented over-approximation: the v4-mapped form
            // is loopback, it is read as public. A false alarm costs a
            // question; the reverse costs the port.
            ("LISTEN 0 128 [::ffff:127.0.0.1]:8000 [::]:*", false),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse_ss_localhost_only(line, 8000).unwrap(),
                expected,
                "wrong verdict for {line}"
            );
        }
    }

    /// One public listener anywhere is enough: loopback does not
    /// « compensate ». This is the real output of a vLLM started without
    /// `--host 127.0.0.1` after a first correct start.
    #[test]
    fn one_public_bind_is_enough_to_fail_even_next_to_a_loopback_one() {
        let out = "LISTEN 0 128 127.0.0.1:8000 0.0.0.0:*\nLISTEN 0 128 0.0.0.0:8000 0.0.0.0:*\n";
        assert!(!parse_ss_localhost_only(out, 8000).unwrap());
    }

    /// SECURITY: the process column is named by whoever starts the process.
    /// Both directions matter, and the second is the costly one: a really
    /// exposed service that a process name passes off as local. A parser that
    /// looks for `127.0.0.1:8000` in the line instead of reading column 4 is
    /// turned around by five characters.
    #[test]
    fn a_process_name_cannot_forge_the_bind_address() {
        let forges_local =
            "LISTEN 0 128 0.0.0.0:8000 0.0.0.0:* users:((\"127.0.0.1:8000\",pid=1,fd=3))";
        assert!(
            !parse_ss_localhost_only(forges_local, 8000).unwrap(),
            "a process named 127.0.0.1:8000 does not make a 0.0.0.0 bind local"
        );
        let forges_public =
            "LISTEN 0 128 127.0.0.1:8000 0.0.0.0:* users:((\"0.0.0.0:8000\",pid=1,fd=3))";
        assert!(
            parse_ss_localhost_only(forges_public, 8000).unwrap(),
            "nor does it make a loopback bind public"
        );
    }

    /// The port is compared whole, never as a suffix: 18000 is not 8000.
    /// Otherwise the probe answers for a service other than the one validated.
    #[test]
    fn the_port_must_match_exactly_not_by_suffix() {
        let out = "LISTEN 0 128 0.0.0.0:18000 0.0.0.0:*\n";
        assert!(
            parse_ss_localhost_only(out, 8000).is_err(),
            "18000 must not answer for 8000"
        );
        assert!(!parse_ss_localhost_only(out, 18000).unwrap());
    }

    /// Absence is not a verdict. Nothing listening on 8000 means vLLM is not
    /// started — a different problem from « exposed ». Answering `Ok(true)`
    /// would make the encryption gate sign off on a machine where the check
    /// saw nothing.
    #[test]
    fn no_listener_for_the_port_is_an_error_not_a_clean_bill() {
        assert!(parse_ss_localhost_only(SS_VLLM_LOOPBACK, 9999).is_err());
        assert!(parse_ss_localhost_only("", 8000).is_err());
        assert!(parse_ss_localhost_only("bash: ss: command not found", 8000).is_err());
        // An ESTABLISHED line is not a listener.
        assert!(
            parse_ss_localhost_only("ESTAB 0 0 10.128.0.7:8000 10.128.0.9:5555", 8000).is_err()
        );
    }

    // Firewall fixtures: `gcloud compute firewall-rules list --format=json`.
    const FW_ALLOWS_8000: &str = r#"[{"allowed":[{"IPProtocol":"tcp","ports":["8000"]}],
        "direction":"INGRESS","disabled":false,"name":"allow-vllm",
        "network":"projects/example-project-123456/global/networks/default",
        "sourceRanges":["0.0.0.0/0"]}]"#;
    const FW_SSH_ONLY: &str = r#"[{"allowed":[{"IPProtocol":"tcp","ports":["22"]}],
        "direction":"INGRESS","disabled":false,"name":"default-allow-ssh",
        "sourceRanges":["0.0.0.0/0"]},
        {"allowed":[{"IPProtocol":"icmp"}],"direction":"INGRESS","disabled":false,
        "name":"default-allow-icmp","sourceRanges":["0.0.0.0/0"]}]"#;

    #[test]
    fn firewall_exposes_the_port_it_allows_and_no_other() {
        assert!(parse_firewall_exposes(FW_ALLOWS_8000, 8000).unwrap());
        assert!(!parse_firewall_exposes(FW_ALLOWS_8000, 8001).unwrap());
        assert!(!parse_firewall_exposes(FW_SSH_ONLY, 8000).unwrap());
        assert!(parse_firewall_exposes(FW_SSH_ONLY, 22).unwrap());
        assert!(!parse_firewall_exposes("[]", 8000).unwrap());
    }

    /// SECURITY: the three ways a broad rule goes unseen. `tcp` without
    /// `ports` is not an empty rule, it is ALL ports; `all` covers tcp without
    /// naming it; a range contains its interior. A failure here = the
    /// encryption gate declares the firewall closed on a rule that opens
    /// everything.
    #[test]
    fn a_rule_without_a_port_list_opens_every_port() {
        let no_ports = r#"[{"allowed":[{"IPProtocol":"tcp"}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(no_ports, 8000).unwrap());
        let empty_ports =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":[]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(empty_ports, 8000).unwrap());
        let all_protocols = r#"[{"allowed":[{"IPProtocol":"all"}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(all_protocols, 8000).unwrap());
        let range =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["7000-9000"]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(range, 8000).unwrap());
        let other_range =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["8001-8100"]}],"direction":"INGRESS"}]"#;
        assert!(!parse_firewall_exposes(other_range, 8000).unwrap());
    }

    /// The only three ways to open nothing while naming the port: a DENY
    /// rule, a disabled rule, an egress rule.
    #[test]
    fn deny_disabled_and_egress_rules_expose_nothing() {
        let denied =
            r#"[{"denied":[{"IPProtocol":"tcp","ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(!parse_firewall_exposes(denied, 8000).unwrap());
        let disabled = r#"[{"allowed":[{"IPProtocol":"tcp","ports":["8000"]}],
            "direction":"INGRESS","disabled":true}]"#;
        assert!(!parse_firewall_exposes(disabled, 8000).unwrap());
        let egress =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["8000"]}],"direction":"EGRESS"}]"#;
        assert!(!parse_firewall_exposes(egress, 8000).unwrap());
    }

    /// SECURITY: the four shapes measured at review round 1 (2026-08-14).
    /// All carry `ports:["8000"]` and therefore REALLY open the port; the
    /// parser returned `Ok(false)`, « closed », because any unrecognized
    /// protocol hit `continue` and vanished from the verdict. That is the
    /// exact opposite of what the SECURITY doc-comment promises, and the
    /// opposite of the direction the file's other approximations lean.
    /// A failure here = the encryption gate declares an open firewall
    /// closed, on a rule gcloud accepts without a flinch.
    #[test]
    fn a_rule_that_really_opens_the_port_is_never_read_as_closed() {
        // 1. The protocol by its NUMBER: `--allow=6:8000` opens tcp 8000.
        let numeric =
            r#"[{"allowed":[{"IPProtocol":"6","ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(
            parse_firewall_exposes(numeric, 8000).unwrap(),
            "6 is the tcp protocol number"
        );
        // An unknown number is read as tcp, not as harmless.
        let unknown =
            r#"[{"allowed":[{"IPProtocol":"47","ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(unknown, 8000).unwrap());

        // 2-4. The three malformed fields: the rule is not understood, so it
        // is not declared closed. Err, never Ok(false).
        let no_proto = r#"[{"allowed":[{"ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(
            parse_firewall_exposes(no_proto, 8000).is_err(),
            "IPProtocol absent"
        );
        let int_proto =
            r#"[{"allowed":[{"IPProtocol":6,"ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(
            parse_firewall_exposes(int_proto, 8000).is_err(),
            "IPProtocol as an integer"
        );
        let not_a_list = r#"[{"allowed":"tcp:8000","direction":"INGRESS"}]"#;
        assert!(
            parse_firewall_exposes(not_a_list, 8000).is_err(),
            "allowed not an array"
        );
        // And a rule that is not even an object is not silently skipped.
        assert!(parse_firewall_exposes(r#"["allow-vllm"]"#, 8000).is_err());
    }

    /// The counterpart of the previous test: the list of protocols that can
    /// NOT carry a tcp port must stay a list, otherwise the
    /// `default-allow-icmp` present in every GCP project raises an alert on
    /// every run — and a permanent alert is no longer read.
    #[test]
    fn protocols_that_cannot_carry_a_tcp_port_do_not_raise_a_false_alarm() {
        for proto in ["udp", "17", "icmp", "1", "esp", "ah", "sctp", "ipip"] {
            let json = format!(
                r#"[{{"allowed":[{{"IPProtocol":"{proto}","ports":["8000"]}}],"direction":"INGRESS"}}]"#
            );
            assert!(
                !parse_firewall_exposes(&json, 8000).unwrap(),
                "{proto} does not open tcp 8000"
            );
        }
        // ABSENT `allowed` stays a legitimate DENY rule, not a malformed field.
        let denied =
            r#"[{"denied":[{"IPProtocol":"tcp","ports":["8000"]}],"direction":"INGRESS"}]"#;
        assert!(!parse_firewall_exposes(denied, 8000).unwrap());
    }

    /// Garbage in → error out. Including garbage that is garbage in only one
    /// field: an unreadable port spec is not silently skipped, otherwise the
    /// rule that carries it vanishes from the verdict.
    #[test]
    fn unreadable_firewall_output_is_an_error_not_a_closed_verdict() {
        assert!(parse_firewall_exposes("not json", 8000).is_err());
        assert!(parse_firewall_exposes(r#"{"rules":[]}"#, 8000).is_err());
        let bad_spec =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["80 00"]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(bad_spec, 8000).is_err());
        let inverted =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["9000-7000"]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(inverted, 8000).is_err());
        let not_a_string =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":[8000]}],"direction":"INGRESS"}]"#;
        assert!(parse_firewall_exposes(not_a_string, 8000).is_err());
    }

    /// SECURITY: the same bound as `an_unreadable_cc_status_value_is_quoted_
    /// but_bounded`, on the other remote value this module quotes in a
    /// refusal — the port spec of a firewall rule.
    ///
    /// It was unbounded until now and nobody reached it: nothing called
    /// `parse_firewall_exposes` before `ValidateEncryption` was wired
    /// (task 4). The producer of the text is anyone who can create a rule in
    /// the project, and the consumer is a message telling an operator that
    /// the pipeline REFUSED. A failure of this test means a rule named with a
    /// 50,000-character spec pushes the refusal that names it off the
    /// screen — `sanitize` can do nothing about it, a wall of « 0 » is
    /// harmless character by character.
    ///
    /// Both error arms are exercised, and BOTH must go through the bound:
    /// the first on an unreadable spec, the second on an inverted range
    /// whose two bounds are, for their part, perfectly readable (leading
    /// zeros are accepted by `u16::from_str`, which is exactly what makes
    /// the case reachable).
    #[test]
    fn an_unreadable_port_spec_is_quoted_but_bounded() {
        let bound = |detail: &str| {
            assert!(
                // + 80: the bounded quote, plus the prefix and the
                // `ParseIntError` message that follow it. Measured at 143 chars.
                detail.chars().count() <= EXCERPT_MAX_CHARS + 80,
                "a refusal must fit on screen, {} characters",
                detail.chars().count()
            );
            assert!(
                detail.contains('…'),
                "the truncation must be visible: {detail}"
            );
        };

        let huge = "9".repeat(50_000);
        let json = format!(
            r#"[{{"allowed":[{{"IPProtocol":"tcp","ports":["{huge}"]}}],"direction":"INGRESS"}}]"#
        );
        let e = parse_firewall_exposes(&json, 8000).unwrap_err();
        bound(&e.detail);

        // Inverted range: `000…0009000-000…0001`, two readable bounds, low > high.
        let zeros = "0".repeat(25_000);
        let inverted = format!("{zeros}9000-{zeros}1");
        let json = format!(
            r#"[{{"allowed":[{{"IPProtocol":"tcp","ports":["{inverted}"]}}],"direction":"INGRESS"}}]"#
        );
        let e = parse_firewall_exposes(&json, 8000).unwrap_err();
        bound(&e.detail);

        // And the bound does not cut the normal case: a short spec is quoted whole.
        let short =
            r#"[{"allowed":[{"IPProtocol":"tcp","ports":["80 00"]}],"direction":"INGRESS"}]"#;
        let e = parse_firewall_exposes(short, 8000).unwrap_err();
        assert!(e.detail.contains("80 00"), "{}", e.detail);
        assert!(!e.detail.contains('…'), "{}", e.detail);
    }

    /// `df --output=avail -k /dev/shm` on the full VM: header + one value.
    /// 117 GB free, half of the a3-highgpu-1g's 234 GB of RAM.
    const DF_SHM: &str = "     Avail\n 122683392\n";

    #[test]
    fn df_reads_the_free_space_under_its_header() {
        assert_eq!(parse_df_avail_kib(DF_SHM).unwrap(), 122_683_392);
        // Without header (df -k --output=avail | tail -1): same reading.
        assert_eq!(parse_df_avail_kib("122683392\n").unwrap(), 122_683_392);
    }

    /// Free space that cannot be read is an ERROR, never a number.
    ///
    /// The two possible defaults are not symmetric: a default of 0 refuses a
    /// perfectly healthy machine, a default of `u64::MAX` lets a 15 GB
    /// download start on a filesystem that cannot hold it — and it dies at
    /// 90 %, after minutes of billed H100. A failure of this test means one
    /// of the two came back.
    #[test]
    fn unreadable_free_space_is_an_error_not_a_number() {
        assert!(parse_df_avail_kib("").is_err());
        assert!(
            parse_df_avail_kib("     Avail\n").is_err(),
            "a header alone is not a value"
        );
        assert!(parse_df_avail_kib("df: /dev/shm: No such file or directory\n").is_err());
        // Two filesystems: choosing one of them would be guessing.
        assert!(parse_df_avail_kib("     Avail\n 122683392\n 1024\n").is_err());
        // And the quote of remote text is bounded, as everywhere else.
        let e = parse_df_avail_kib(&"9".repeat(50_000)).unwrap_err();
        assert!(
            e.detail.chars().count() <= EXCERPT_MAX_CHARS + 80,
            "{}",
            e.detail.len()
        );
    }

    /// `gcloud compute disks describe x0ne-cc --format=json(name,diskEncryptionKey)`
    /// on an ordinary disk: GCP encrypts at rest with its own key and then
    /// emits NO `diskEncryptionKey` field.
    const DISK_DEFAULT: &str = r#"{"name":"x0ne-cc"}"#;

    #[test]
    fn a_disk_without_a_key_field_is_the_google_managed_default() {
        assert_eq!(
            parse_disk_encryption(DISK_DEFAULT, "x0ne-cc").unwrap(),
            DiskEncryption::GoogleManaged
        );
        let cmek = r#"{"name":"x0ne-cc","diskEncryptionKey":{"kmsKeyName":"projects/p/locations/l/keyRings/r/cryptoKeys/k"}}"#;
        assert_eq!(
            parse_disk_encryption(cmek, "x0ne-cc").unwrap(),
            DiskEncryption::CustomerKey
        );
    }

    /// SECURITY: an answer that is not about OUR disk is not an answer about
    /// our disk. An empty output, a projection that lost the field, a
    /// `describe` aimed at another disk: all must fail, and none must read as
    /// « the boot disk is fine ». This is the `parse_instance_state` rule
    /// applied to the disk — a failure here means the report can assert a
    /// property of the disk without having read it.
    #[test]
    fn a_describe_that_is_not_about_our_disk_is_an_error() {
        assert!(parse_disk_encryption("", "x0ne-cc").is_err());
        assert!(
            parse_disk_encryption("[]", "x0ne-cc").is_err(),
            "an array is not a disk"
        );
        assert!(
            parse_disk_encryption("{}", "x0ne-cc").is_err(),
            "no name field"
        );
        assert!(
            parse_disk_encryption(r#"{"name":"other-disk"}"#, "x0ne-cc").is_err(),
            "another disk says nothing about ours"
        );
        assert!(
            parse_disk_encryption(r#"{"name":"x0ne-cc","diskEncryptionKey":"yes"}"#, "x0ne-cc")
                .is_err(),
            "a field that cannot be read decides neither way"
        );
    }

    /// Real `GET /v1/models` answer from an OpenAI-compatible vLLM server.
    const VLLM_ONE_MODEL: &str = r#"{"object":"list","data":[{"id":"meta-llama/Llama-3.1-8B-Instruct",
        "object":"model","created":1755158400,"owned_by":"vllm",
        "root":"meta-llama/Llama-3.1-8B-Instruct","parent":null,"max_model_len":8192,
        "permission":[{"id":"modelperm-0","object":"model_permission","created":1755158400,
        "allow_sampling":true,"allow_logprobs":true,"allow_view":true,"is_blocking":false}]}]}"#;

    #[test]
    fn vllm_models_reads_the_served_ids() {
        assert_eq!(
            parse_vllm_models(VLLM_ONE_MODEL).unwrap(),
            vec!["meta-llama/Llama-3.1-8B-Instruct".to_string()]
        );
    }

    /// An empty list is NOT an error: the server answered, it serves
    /// nothing. « vLLM answers but serves nothing » and « vLLM does not
    /// answer » are handled differently, and an error here would erase the
    /// difference.
    #[test]
    fn an_empty_model_list_is_an_answer_not_a_failure() {
        assert_eq!(
            parse_vllm_models(r#"{"object":"list","data":[]}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    /// An unreadable entry is not skipped: a shorter list would under-declare
    /// what the machine serves. And an id carrying control characters is
    /// refused here before being neutralized by `cc_runner::sanitize` — the
    /// report is terminal text, a `\n` or an ESC in an id forges lines there
    /// (review round 2, 2026-08-14).
    #[test]
    fn an_unreadable_model_entry_is_an_error_not_a_shorter_list() {
        assert!(parse_vllm_models("not json").is_err());
        assert!(parse_vllm_models(r#"{"object":"list"}"#).is_err());
        assert!(parse_vllm_models(r#"{"data":[{"object":"model"}]}"#).is_err());
        assert!(parse_vllm_models(r#"{"data":[{"id":42}]}"#).is_err());
        assert!(
            parse_vllm_models("{\"data\":[{\"id\":\"llama\\n  vm_state  ENCLAVE PROVEN\"}]}")
                .is_err()
        );
        assert!(parse_vllm_models("{\"data\":[{\"id\":\"llama\\u001b[2K\"}]}").is_err());
    }

    #[test]
    fn spent_is_hourly_times_elapsed() {
        assert!((spent_usd(11.26, 3600) - 11.26).abs() < 1e-9);
        assert!((spent_usd(0.22, 1800) - 0.11).abs() < 1e-9);
        assert_eq!(spent_usd(0.22, 0), 0.0);
    }

    /// The spec thresholds: warn at 50 % and 80 %, cut off at 100 %.
    /// A failure here = either the operator is harassed or left to overspend.
    #[test]
    fn cap_thresholds_are_50_80_100() {
        assert_eq!(cap_status(12.4, 25.0), CapStatus::Ok); // 49.6 %
        assert_eq!(cap_status(12.5, 25.0), CapStatus::Warn50); // 50 %
        assert_eq!(cap_status(19.9, 25.0), CapStatus::Warn50); // 79.6 %
        assert_eq!(cap_status(20.0, 25.0), CapStatus::Warn80); // 80 %
        assert_eq!(cap_status(25.0, 25.0), CapStatus::Exceeded);
        assert_eq!(cap_status(30.0, 25.0), CapStatus::Exceeded);
    }

    /// A zero or negative cap is Exceeded outright: the budget gate must be
    /// fail-closed, not fail-open on an absurd config.
    #[test]
    fn a_nonpositive_cap_is_always_exceeded() {
        assert_eq!(cap_status(0.0, 0.0), CapStatus::Exceeded);
        assert_eq!(cap_status(0.0, -1.0), CapStatus::Exceeded);
    }

    /// THE fail-open measured at review round 1 (2026-08-13). `--cap nan` and
    /// `--cap inf` both parse as valid f64, and NaN makes every comparison of
    /// the gate FALSE: `cap <= 0.0`, `spent >= cap`, and the thresholds via
    /// `cap * 0.8`. The cap therefore answered `Ok` whatever the spending. A
    /// failure of this test would mean three characters on the command line
    /// disarm the only thing that stops the bill — on the full machine,
    /// 3.72 $/h without bound.
    #[test]
    fn a_cap_that_is_not_a_finite_number_is_exceeded_not_infinite() {
        for absurd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                cap_status(1.0, absurd),
                CapStatus::Exceeded,
                "cap {absurd} must refuse, never wave everything through"
            );
            // And at zero spending too: that is where the loop queries the
            // gate for the first time, before the first paid step.
            assert_eq!(
                cap_status(0.0, absurd),
                CapStatus::Exceeded,
                "cap {absurd} at zero spend"
            );
        }
        // The string as it arrives from clap, not only the constant.
        assert_eq!(cap_status(0.0, "nan".parse().unwrap()), CapStatus::Exceeded);
        assert_eq!(cap_status(0.0, "inf".parse().unwrap()), CapStatus::Exceeded);
    }

    #[test]
    fn the_status_line_is_an_estimate_never_a_bill() {
        let line = status_line(CcMachine::Prover, 0.07, 25.0);
        assert!(
            line.contains('≈'),
            "estimates must read as estimates: {line}"
        );
        assert!(line.contains("0.24"));
        assert!(line.contains("25"));
    }
}
