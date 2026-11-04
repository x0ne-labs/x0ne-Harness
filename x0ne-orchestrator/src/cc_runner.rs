//! The impure half of `x0ne cc`: runs the plan, one observable step at a time.
//!
//! Testability rule: everything in here goes through `CcExec`, a one-method
//! trait. Production (`x0ne-code`) implements it over the existing Bridge so
//! every command inherits the action journal and the CLI rendering
//! (`bridge.rs:12-20`, pattern of the self-test at `main.rs:2912-2922`);
//! tests implement it with a scripted queue. The runner itself never touches
//! a socket — `cargo test` proves the pipeline without spending a cent.

use async_trait::async_trait;

use x0ne_cloud::cc_plan::{
    build_plan, parse_auth_account, parse_disk_encryption, parse_firewall_exposes,
    parse_gpu_ready_state, parse_image_family_exists, parse_instance_state, parse_nvidia_cc_mode,
    parse_nvidia_smi_ready, parse_ss_localhost_only, parse_swapon_empty, parse_tmpfs_probe,
    parse_vllm_models, parse_vllm_wait_probe, CcConfig, CcMachine, CcObservations, CcStep,
    DiskEncryption, InstanceState, TMPFS_PROBE_MARK_AVAIL, TMPFS_PROBE_MARK_REMOVEIPC,
    VLLM_PROBE_MARK_LISTEN, VLLM_PROBE_MARK_MODELDIR, VLLM_PROBE_MARK_RESTARTS,
};
use x0ne_confidential::model::Verdict;
use x0ne_confidential::tdx_quote::{self, TdxQuoteClaims};

pub struct CmdOut {
    pub stdout: String,
    /// SECURITY (review finding I3, 2026-08-13): gcloud/ssh route their
    /// diagnostics to stderr, not stdout — `attest_tdx` needs a stdout that
    /// is PURE base64 (nothing else may share the stream), so stderr has to
    /// travel separately or a diagnostic line printed on stdout would land
    /// inside the decode and silently corrupt the quote instead of erroring
    /// where an operator can read it.
    pub stderr: String,
    pub exit_code: i32,
}

/// The TRANSPORT is gone — as opposed to a command that ran and answered
/// badly.
///
/// Measured on 2026-08-14, first real `--machine full` run: `x0ne-runtime`
/// died mid-pipeline and the retry loops below went on probing through the
/// corpse — 26 `▸ exec` with no answer, ~10 min of a 3.72 $/h machine, and a
/// final message blaming the VM ("the VM did not come back from its reboot")
/// for a failure that never left this process. Every wait loop here treats an
/// `Err` as "the machine is not ready yet, ask again", which is the right
/// reading of a probe that REACHED the VM and the wrong reading of one that
/// could not be sent at all.
///
/// A distinct error type rather than a substring the loops sniff for: a
/// message-matching test is the shape of scaffolding this repo keeps growing —
/// silently true until someone rewords the message, and then silently false.
/// Travelling inside `anyhow::Error` keeps every `CcExec` signature unchanged;
/// only the loops that CHOOSE to retry have to ask the question.
#[derive(Debug)]
pub struct ExecTransportDown {
    detail: String,
}

impl ExecTransportDown {
    /// Wrap the transport's own error. Named `raise` rather than `new`
    /// because it hands back an `anyhow::Error`, not a `Self`: every caller is
    /// in `Result<_, anyhow::Error>` territory and the type only ever travels
    /// inside one.
    pub fn raise(detail: impl std::fmt::Display) -> anyhow::Error {
        anyhow::Error::new(Self {
            detail: detail.to_string(),
        })
    }

    /// Is this failure one that no further attempt can fix? Asked by the retry
    /// loops, and by nothing else — a step that already fails on the first
    /// error needs no question.
    pub fn is_fatal(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.is::<Self>())
    }
}

impl std::fmt::Display for ExecTransportDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Says WHAT broke (not the VM), and admits what is now unknown: the
        // pipeline can no longer ask the cloud anything, including whether the
        // meter is still running. Naming the check is the difference between a
        // diagnosis and an alarm.
        write!(
            f,
            "the bridge to x0ne-runtime died — the VM state is UNKNOWN, check by hand: \
             gcloud compute instances list. Transport error: {}",
            self.detail
        )
    }
}

impl std::error::Error for ExecTransportDown {}

/// The seam. `async` because the only production implementation talks to the
/// runtime through `Bridge::send_action`, which is async, and `x0ne cc` is
/// awaited from the tokio main — a `block_on` inside a sync `run` panics with
/// "cannot block on the current runtime" (Task 7, 2026-08-13). Making the
/// trait async is the mechanical fix; the alternative (running the whole
/// wizard on a spare `std::thread`) buys nothing and costs a thread.
#[async_trait]
pub trait CcExec {
    /// Run `gcloud <argv>`. `display` is the human-facing line already shown.
    ///
    /// `timeout_secs` is decided by the RUNNER, not the exec, because only the
    /// runner knows which command it is issuing and how long that particular
    /// one may legitimately take — most sharply, the first SSH probe, which
    /// has a key to generate (see `SSH_FIRST_PROBE_TIMEOUT_SECS`). An exec
    /// inferring the budget from the label would be guessing at policy it
    /// does not own.
    async fn run(
        &mut self,
        display: &str,
        argv: &[String],
        timeout_secs: u64,
    ) -> anyhow::Result<CmdOut>;
}

/// The `display` string `wait_ssh` passes for its probe, and the ONLY thing
/// that tells an exec layer "this failure is expected, the VM is still
/// booting". A shared constant rather than a literal on both sides: the
/// production exec pauses on it (`cc_cmd.rs`), and a typo would silently turn
/// the pause off — the exact shape of this repo's dead-scaffolding defect.
pub const WAIT_SSH_DISPLAY: &str = "wait for SSH";

/// The `display` string the two vLLM waits pass, and — like `WAIT_SSH_DISPLAY`
/// — the ONLY thing that tells the exec layer to pause between attempts.
///
/// It gets its own label rather than borrowing the SSH one because the pause
/// differs in LENGTH (a model download is not a boot) and, more sharply, in
/// TRIGGER. The SSH pause fires on a failed probe; both vLLM waits are the
/// opposite case — `ss` exits 0 with a perfectly readable answer that simply
/// does not contain the port yet, and a curl can exit 0 on a server that is up
/// and serving nothing. Keyed on failure, `VLLM_START_RETRIES` attempts would
/// burn in under a second, minutes before the weights land: the round-1 SSH bug
/// in a new costume. So the exec pauses on EVERY call carrying this label, and
/// the price is one 20 s pause after the last, successful probe — 2 cents.
pub const WAIT_VLLM_DISPLAY: &str = "wait for vLLM";

pub struct CcRunner<'a> {
    exec: &'a mut dyn CcExec,
    pub config: CcConfig,
    verdict: Option<Verdict>,
    claims: Option<TdxQuoteClaims>,
    nonce_hex: String,
    /// Set by `attest_gpu` alone. `None` means the GPU half of the proof was
    /// never measured on THIS run — which the report renders as "exposed",
    /// never as absence of a problem.
    gpu: Option<GpuEvidence>,
    /// Set by `validate_encryption` alone. `None` means the in-VM checks were
    /// never completed on THIS run, which the report renders as "unverified" —
    /// never as absence of a problem.
    encryption: Option<EncryptionChecks>,
    /// The pre-flight observation, kept so `replan_after_ssh` can rebuild a
    /// plan from the same facts plus the fresh in-VM ones. `None` means
    /// `observe()` never ran, and then nothing gets re-planned: pruning an
    /// approved plan against observations we cannot reproduce is guessing.
    preflight: Option<CcObservations>,
    /// The local binaries `InstallX0ne` uploads. A PARAMETER, never a path
    /// compiled in: production resolves them from the running executable
    /// (`cc_cmd::resolve_x0ne_binaries`) and tests script them into a temp
    /// directory. `None` means nobody supplied them, and then `InstallX0ne`
    /// REFUSES — it never falls back to a guessed `target/release`, which
    /// would upload whatever an unrelated build left there.
    binaries: Option<X0neBinaries>,
    /// The transport under this runner has died — see `ExecTransportDown`.
    ///
    /// A property of the RUNNER and not of the step that noticed, because what
    /// it says is "the exec I hold can no longer carry anything, including the
    /// stop this run owes the operator". Read by the caller, which is the only
    /// layer that can bring up a fresh runtime; set nowhere but `note_
    /// transport`, and only ever from `false` to `true` — a transport does not
    /// come back to life under the same `CcExec`.
    transport_down: bool,
}

/// Where the two binaries `InstallX0ne` copies into the VM live on THIS
/// machine.
///
/// They are what the enclave will run, so they are a decision of the caller,
/// not of the runner: `x0ne cc` copies the very executable it is running plus
/// its sibling runtime, and refuses when either is missing. See
/// `cc_cmd::resolve_x0ne_binaries`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X0neBinaries {
    pub x0ne: std::path::PathBuf,
    pub runtime: std::path::PathBuf,
}

/// What the GPU answered, measured from inside the attested CPU enclave.
///
/// SECURITY: same pattern as `ProofBundle`, for the same reason. The fields
/// are private and `attest_gpu` is the only code that fills them, so a
/// `GpuEvidence` existing means two commands really ran on the machine and
/// two parsers really read their output. `pub` fields would let any caller in
/// scope write `GpuEvidence { cc_mode_on: true, .. }` and have the report
/// print a GPU verdict nobody measured — the GPU-side version of printing
/// PROVEN over nothing.
///
/// `cc_mode_on` is stored as MEASURED, including when it is `false`, so that
/// the report's rule is "print the combined verdict only for evidence that
/// says ON" rather than "print it for any evidence that exists" — one is a
/// check, the other is a coincidence of which values ever get built. The step
/// fails on `false` anyway, so a Proven outcome cannot carry it in practice;
/// the report is tested against that state regardless, because the day
/// someone adds a second way to build this struct, the check is what holds.
///
/// A run that measured OFF and stopped now RENDERS that reading:
/// `CcOutcome::Failed` carries the evidence (task 4, review round 1, finding
/// I1), so the report says "the GPU answered OFF (driver …)" rather than "no
/// GPU measurement was obtained". Until that fix, storing the value bought only
/// the report's rule, and the sentence describing the failure path was one the
/// code could not honour.
#[derive(Debug, Clone)]
pub struct GpuEvidence {
    cc_mode_on: bool,
    driver_version: String,
}

impl GpuEvidence {
    pub fn cc_mode_on(&self) -> bool {
        self.cc_mode_on
    }

    /// Already `sanitize`d and length-capped at construction — see
    /// `attest_gpu`. Callers get a string that is safe to print in a terminal
    /// report because it was made safe at the single door, not at each use.
    pub fn driver_version(&self) -> &str {
        &self.driver_version
    }
}

/// As four measurements: what the machine answered when asked
/// whether anything it holds can leave it in the clear.
///
/// SECURITY: same shape and same reason as `GpuEvidence` — private fields, one
/// constructor (`validate_encryption`), so a value of this type existing means
/// four commands really ran and four parsers really read their output. The
/// report prints one line per field ("every check = one line of the
/// report"), and a `pub` field would let any caller in scope write the
/// reassuring answer into a report nobody measured.
///
/// The values are stored AS MEASURED, `false` included, and the step refuses
/// afterwards. The rule the report applies is then "print this line from what
/// was read", not "print it because a value exists" — the distinction task 3
/// drew for `cc_mode_on`, for the day someone adds a second constructor.
///
/// And storing them buys a second thing, which review round 1 of this task had
/// to point out: `CcOutcome::Failed` CARRIES them, so the refusal report prints
/// the four lines as measured — "swap ACTIVE" next to "bind loopback-only" and
/// "firewall closed". That is the run where those lines matter most, and it was
/// the run the report used to describe as "unverified".
#[derive(Debug, Clone)]
pub struct EncryptionChecks {
    swap_off: bool,
    localhost_only: bool,
    firewall_closed: bool,
    disk: DiskEncryption,
}

impl EncryptionChecks {
    pub fn swap_off(&self) -> bool {
        self.swap_off
    }

    pub fn localhost_only(&self) -> bool {
        self.localhost_only
    }

    pub fn firewall_closed(&self) -> bool {
        self.firewall_closed
    }

    pub fn disk(&self) -> DiskEncryption {
        self.disk
    }

    /// Did every check that CAN fail pass? `disk` is not in the conjunction on
    /// purpose: both of its values mean "encrypted at rest", so there is no
    /// answer to refuse — Compute Engine has no unencrypted persistent disk to
    /// report. Folding it in would put a check in the gate that can never fire,
    /// which reads as a fourth guarantee and is a third.
    pub fn all_pass(&self) -> bool {
        self.swap_off && self.localhost_only && self.firewall_closed
    }
}

/// A proof that can only be built from a verified verdict.
///
/// SECURITY: the single constructor (`from_verdict`) is the whole point. The
/// fields are private on purpose — `pub` fields would let any code in scope
/// build a `ProofBundle { .. }` literal directly, walking straight around
/// `from_verdict` and printing PROVEN over nothing. Private fields plus
/// accessors keep `from_verdict` the only door. If a `ProofBundle` exists, a
/// `Verdict::Verified` existed — there is no other path, so `render_report`
/// cannot print PROVEN over a rejection. Same pattern as
/// `EnclaveStatus::from_verdict` (2026-08-13).
///
/// # SECURITY: `Serialize`, and deliberately NOT `Deserialize`
///
/// The JSON report (`--json`, `~/.x0ne/cc/last-report.json`) has to publish
/// these measurements, and publishing them must not open a second door.
/// `Serialize` READS a bundle that exists; `Deserialize` would BUILD one from
/// whatever text was on the wire, which is precisely the `pub` field problem
/// wearing a different hat — a forged `last-report.json` would then mint a
/// proof, and `EnclaveStatus::from_outcome` would light the badge green over
/// it. The absence of that derive is checked by the `compile_fail` doctest
/// below, so deleting this paragraph and adding `Deserialize` breaks a test
/// rather than a deployment.
///
/// The bundle carries the verdict it was minted from, so
/// `EnclaveStatus::from_outcome` can go through `from_verdict` instead of
/// re-deriving "this was verified" from the bundle's mere existence — one
/// door, and it is the documented one.
///
/// ```
/// # use x0ne_orchestrator::cc_runner::ProofBundle;
/// # use x0ne_confidential::model::{Verdict, VerifierStrength, WorkloadIdentity};
/// // Serializing a bundle that exists is fine, and is what the report does.
/// fn reads(p: &ProofBundle) -> String { serde_json::to_string(p).unwrap() }
/// ```
///
/// ```compile_fail
/// # use x0ne_orchestrator::cc_runner::ProofBundle;
/// // SECURITY: building one from JSON must not compile. A failure of this
/// // doctest means a `Deserialize` derive was added and a proof can now be
/// // typed by hand into a file.
/// let forged: ProofBundle = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProofBundle {
    mr_td_hex: String,
    rtmr_hex: [String; 4],
    nonce_hex: String,
    verifier: String,
    /// The verdict this bundle was minted from. Kept so the badge can be
    /// derived through `EnclaveStatus::from_verdict` rather than around it.
    ///
    /// Not serialized: the report publishes the measurements and the STRENGTH
    /// (see `strength`), not the verifier's internal record — which also
    /// carries a `WorkloadIdentity` that says nothing a reader of this report
    /// can check.
    #[serde(skip)]
    verdict: Verdict,
}

impl ProofBundle {
    pub fn from_verdict(
        verdict: &Verdict,
        claims: Option<&TdxQuoteClaims>,
        nonce_hex: String,
    ) -> Option<Self> {
        match verdict {
            Verdict::Verified { verifier, .. } => {
                let c = claims?;
                Some(Self {
                    mr_td_hex: tdx_quote::hex(&c.mr_td),
                    rtmr_hex: std::array::from_fn(|i| tdx_quote::hex(&c.rtmr[i])),
                    nonce_hex,
                    verifier: verifier.clone(),
                    verdict: verdict.clone(),
                })
            }
            Verdict::Rejected { .. } => None,
        }
    }

    pub fn mr_td_hex(&self) -> &str {
        &self.mr_td_hex
    }

    pub fn rtmr_hex(&self) -> &[String; 4] {
        &self.rtmr_hex
    }

    pub fn nonce_hex(&self) -> &str {
        &self.nonce_hex
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The verdict this bundle was minted from — always `Verified` by
    /// construction, and the value `EnclaveStatus::from_verdict` reads.
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// How strong the word "verified" is here, as one lowercase token.
    ///
    /// SECURITY: read from the verdict, never hardcoded. `"structural"` is
    /// what this milestone can honestly say, and the day a cryptographic
    /// verifier is wired in, the reports change with it instead of continuing
    /// to under-claim — or, far worse, continuing to say `"cryptographic"`
    /// after someone typed it in by hand.
    pub fn strength(&self) -> &'static str {
        use x0ne_confidential::model::VerifierStrength;
        match &self.verdict {
            Verdict::Verified { strength, .. } => match strength {
                VerifierStrength::Structural => "structural",
                VerifierStrength::Cryptographic => "cryptographic",
            },
            // Unreachable by construction (`from_verdict` refuses a rejection),
            // and the honest word for it if construction ever changes.
            Verdict::Rejected { .. } => "none",
        }
    }
}

pub enum CcOutcome {
    /// The CPU enclave is proven. `gpu` carries the GPU half when it was
    /// measured — a separate field rather than a second `Proven` variant,
    /// because "proven CPU, unmeasured GPU" is a real state (the prover
    /// machine has no GPU at all) and the report has to be able to say so.
    Proven {
        proof: ProofBundle,
        gpu: Option<GpuEvidence>,
        /// The in-VM checks, when the run got far enough to make them. `None`
        /// renders as "unverified", the same fail-loud default as `gpu`.
        encryption: Option<EncryptionChecks>,
    },
    Refused {
        reason: String,
    },
    /// A step failed. It carries what had ALREADY been measured when the run
    /// stopped, and that is not symmetry for its own sake (first review,
    /// finding I1): a failed `ValidateEncryption` is the one run where
    /// the in-VM check lines have something to say — "swap ACTIVE, bind loopback,
    /// firewall closed" names the hole and clears the other two. Without these
    /// fields the report of a refusal said "unverified — were not proven on
    /// this run" while `CcRunner::encryption` held the three measurements, and
    /// the failure branches of those lines were code no production path could
    /// reach (two mutants survived on them).
    ///
    /// `Refused` deliberately has no such fields: it is only ever reached by a
    /// rejected TDX verdict or by an attestation that never ran, and both are
    /// upstream of every measurement below — there is nothing to carry.
    Failed {
        step: CcStep,
        error: String,
        /// The CPU attestation, when the run got one before it broke.
        ///
        /// SECURITY (review round 1 of task 5, I1+I2): the proof of a step is
        /// not undone by the failure of a LATER step, and on `--machine full`
        /// this is not a corner case — it is the ONLY report the machine
        /// produces today, since the pipeline stops at `InstallX0ne`. Without
        /// this field the operator paid for a TDX attestation, watched the
        /// error say "the CPU enclave is proven", and got no verifier, no
        /// nonce, no mr_td and no rtmr to back it. `render_report` prints the
        /// measurement block from this field for any variant that has one; the
        /// word PROVEN stays in the `Proven` arm alone.
        proof: Option<ProofBundle>,
        gpu: Option<GpuEvidence>,
        encryption: Option<EncryptionChecks>,
    },
}

/// Attempts `wait_ssh` makes. Trimmed from 30 (review round 2, 2026-08-13) so
/// that the longer first-probe budget below still fits under a 15 min ceiling.
/// Costs nothing in practice: a booting VM refuses connections in
/// milliseconds, so the typical wall clock is the pauses, not the timeouts.
pub const SSH_RETRIES: usize = 27;

/// Seconds allowed for one ordinary gcloud call.
pub const GCLOUD_TIMEOUT_SECS: u64 = 300;

/// Seconds allowed for ONE ssh probe after the first.
///
/// Deliberately NOT the general gcloud timeout. Measured at review round 1
/// (2026-08-13): the probe was inheriting the 300 s budget meant for
/// `instances create`, so a VM whose port 22 is filtered — dropped packets,
/// no RST — hangs the full 300 s per attempt, and `SSH_RETRIES` of those is
/// 2.6 HOURS. On the full machine that is ~30 $ burned inside a single step,
/// where the budget gate (which only runs between steps) can do nothing about
/// it. A warm probe either answers quickly or is not ready; 20 s is generous
/// for a TCP handshake plus SSH banner.
pub const SSH_PROBE_TIMEOUT_SECS: u64 = 20;

/// Seconds allowed for the FIRST ssh probe, which is a different command
/// wearing the same name.
///
/// Review round 2 (2026-08-13), and it was already the deferred minor M4 of
/// Task 6: on a machine that has never run `gcloud compute ssh`, the first
/// invocation generates `~/.ssh/google_compute_engine`, pushes the public key
/// into the project metadata, and waits for it to propagate. That routinely
/// takes more than 20 s. Since the runtime HARD-KILLS at the deadline, a 20 s
/// budget does not merely time out — it kills the key push mid-flight, so all
/// `SSH_RETRIES` attempts then fail for a reason that has nothing to do with
/// the VM, and `WaitSsh` blames the boot. The short timeout of round 1 is what
/// turned that latent minor into a blocker; the fix is to let the first
/// attempt do the one-time work.
pub const SSH_FIRST_PROBE_TIMEOUT_SECS: u64 = 90;

/// Pause between two probes, enforced by the exec layer. See `wait_ssh`.
pub const SSH_PROBE_PAUSE_SECS: u64 = 10;

/// Seconds allowed for the driver install.
///
/// Not the 300 s of an ordinary gcloud call, and not a round number picked for
/// comfort: this one command runs `apt-get update`, pulls ~500 MB of driver
/// packages, builds the open kernel module through DKMS against the running
/// kernel, and rebuilds the initramfs. Ten minutes is ordinary on a cold
/// image. The runtime HARD-KILLS at the deadline, so a budget that is merely
/// tight does not retry — it leaves a half-configured DKMS tree behind and the
/// next `nvidia-smi` fails for a reason that looks like a driver bug.
///
/// The cost of the ceiling is bounded and small: 20 min of a3-highgpu-1g SPOT
/// is ≈ 1.24 $. The cost of being 60 s short is the whole run.
pub const GPU_DRIVER_TIMEOUT_SECS: u64 = 1200;

/// Seconds allowed for one in-VM probe (`nvidia-smi`, a curl to localhost, an
/// `x0ne --version`). Every one of them either answers in under a second or is
/// telling us something is wrong; the short budget is what keeps a hung probe
/// from spending the pipeline's money without the budget gate — which only
/// runs BETWEEN steps — ever getting a say.
pub const IN_VM_PROBE_TIMEOUT_SECS: u64 = 60;

/// Seconds allowed for the vLLM install.
///
/// `uv tool install vllm` downloads the CUDA build of torch and its stack —
/// several GB of wheels — and links them. Ten to fifteen minutes is ordinary on
/// a cold VM; 30 is the ceiling.
///
/// COST, and it is the number the budget gate cannot protect you from: the gate
/// runs only BETWEEN steps, so this single step can spend that time past the cap
/// before anyone checks again. That is the shape of finding I1 of the prover
/// review (a step whose ceiling nobody had multiplied by the hourly rate), so
/// the multiplication is written down here rather than left for the next reader
/// to do — and it counts the WHOLE step, the way `GPU_DRIVER_TIMEOUT_SECS`'s
/// 2250 s counts its reboot and its probes (review round 1 of task 4, minor
/// m3): the acquis probe (60 s) + this install (1800 s) + the verification
/// probe (60 s) = 1920 s ≈ **1.98 $**.
pub const VLLM_INSTALL_TIMEOUT_SECS: u64 = 1800;

/// Seconds allowed for ONE vLLM probe — `ss -tlnp` or a curl to loopback. Both
/// answer in milliseconds on a live machine; the budget covers the ssh session
/// around them, not the command.
pub const VLLM_PROBE_TIMEOUT_SECS: u64 = 30;

/// Seconds allowed for the step that writes the unit and restarts it.
///
/// NOT `IN_VM_PROBE_TIMEOUT_SECS`, and run 6 (2026-08-14) is why: this step
/// stopped being a probe the day it gained a real `systemctl restart`. A
/// restart is a stop THEN a start, and the stop of this unit is not instant —
/// the boot's autostart was mid-download, systemd sent SIGTERM and waited out
/// its grace period. At 60 s the ssh session was killed while the stop was
/// still legitimately running, and the step failed on a machine where nothing
/// was wrong.
///
/// 180 s = the worst-case stop this pipeline now allows (`TimeoutStopSec=15`
/// in the unit it writes) + the start + room for a slow ssh session. The two
/// numbers are a pair: the bound in the unit is what makes this budget
/// sufficient rather than hopeful, so neither moves alone.
///
/// COST: 180 s ≈ 0.19 $ at 3.72 $/h, and it is counted in the `ServeVllm`
/// total written on `VLLM_START_RETRIES`.
pub const VLLM_UNIT_WRITE_TIMEOUT_SECS: u64 = 180;

/// Pause between two vLLM probes, enforced by the exec layer. See
/// `WAIT_VLLM_DISPLAY`.
pub const VLLM_PROBE_PAUSE_SECS: u64 = 20;

/// Probes `ServeVllm` makes while waiting for vLLM to bind its port.
///
/// This is where the model download is paid for. `FetchModel` only prepares the
/// tmpfs — the weights arrive when vLLM first starts, which means ~15 GB into
/// `/dev/shm` plus engine startup before anything LISTENs. Measured components
/// (all estimates until the H100 milestone): the download is minutes at GCP
/// egress-in rates, the engine start is single-digit minutes for a 7B on an
/// H100. 36 × (30 + 20) = 1800 s = **30 min** is a ceiling with room, not a
/// prediction.
///
/// COST, counting the whole step and not just the wait (minor m3, review round
/// 1): the acquis probe (60 s) + writing and restarting the unit (180 s, see
/// `VLLM_UNIT_WRITE_TIMEOUT_SECS` — it was 60 until run 6 showed a real restart
/// does not fit there) + this wait (1800 s) = 2040 s ≈ **2.11 $ spent past the
/// cap inside one step**, since the budget gate only runs between steps. Still
/// under the driver step's own ≈ 2.33 $, which is the comparison that decides
/// whether this ceiling is defensible.
/// It is a CEILING, not a schedule: since run 4 the wait also reads the unit's
/// restart counter and the tmpfs, so the two failures that used to consume all
/// 36 probes — a crash loop and a download that never starts — now stop it in
/// two and six. What is left for the full 36 is the case they were meant for: a
/// server that is genuinely downloading and loading.
pub const VLLM_START_RETRIES: usize = 36;

/// KiB in the model directory above which "something is being downloaded" is a
/// measurement rather than a hope.
///
/// 64 MiB. Run 4, 2026-08-14: the failing run spent 30 min waiting on a
/// directory that never left ~0 while the service crash-looped. Any real
/// download passes 64 MiB within one probe at GCP ingress rates, and a
/// half-written config file does not. It is deliberately not "the size grew":
/// a download that has FINISHED sits flat at 15 GB for the whole engine load,
/// and reading that as a stall would refuse the machine at the last minute.
const MODEL_DOWNLOAD_FLOOR_KIB: u64 = 64 * 1024;

/// Consecutive probes allowed under that floor, with the port still unbound,
/// before the wait gives up.
///
/// 6 × (30 + 20) = 300 s = 5 min ≈ 0.31 $ spent proving that nothing is
/// happening, instead of 30 min ≈ 1.86 $ spent hoping. Generous on purpose:
/// vLLM resolves the model on the hub, and the wait must survive a slow first
/// minute without calling it a stall.
pub const VLLM_DOWNLOAD_STALL_PROBES: usize = 6;

/// The weights' size, for the progress line only — never for a decision.
///
/// ~15 GB of safetensors for Qwen2.5-7B-Instruct (`VLLM_MODEL`). It is the
/// denominator of "4.2G/15G downloaded", which exists because the owner's
/// requirement for this wait was that patience be JUSTIFIED and shown, not
/// merely spent.
const MODEL_WEIGHTS_GIB: u64 = 15;

/// Characters of journal quoted in a crash-loop failure.
///
/// Bounded like every other piece of remote text this program prints
/// (`CC_MODE_ECHO_MAX_CHARS`, same reflex): 40 journal lines is what we ask
/// for, and a unit that answers with a megabyte would push its own verdict off
/// the operator's screen. Larger than the value caps because this one is a
/// DIAGNOSTIC — the useful line is usually the last, and the point is to read
/// it here rather than over a second ssh session.
const JOURNAL_ECHO_MAX_CHARS: usize = 1500;

/// Probes `HealthCheck` makes.
///
/// Far fewer than `VLLM_START_RETRIES`, and that asymmetry is the design: by
/// the time the health check runs, the port is bound — vLLM's HTTP server comes
/// up AFTER the engine is loaded, so a bound port means the weights are in RAM
/// and `/v1/models` answers immediately. These six probes cover the seconds
/// around it, not a download. 6 × (30 + 20) = 300 s = 5 min ≈ 0.31 $, and here
/// the wait IS the whole step.
///
/// The last ceiling of this increment has no constant of its own because it is
/// four ordinary calls: `ValidateEncryption` = two in-VM probes at
/// `IN_VM_PROBE_TIMEOUT_SECS` + two gcloud calls at `GCLOUD_TIMEOUT_SECS` =
/// 720 s ≈ 0.74 $. Written down here rather than nowhere (minor m3, review
/// round 1): a step with no named budget is a step whose worst case nobody has
/// multiplied by the hourly rate.
pub const VLLM_HEALTH_RETRIES: usize = 6;

/// The port vLLM is served on, loopback-only.
///
/// A constant, and it must be THE constant: `parse_ss_localhost_only` and
/// `parse_firewall_exposes` both take the port as a parameter, and a
/// validation that checks 8000 while the server listens on 8001 proves
/// nothing about the machine it ran on (task 2 report, "concerns"). Every
/// command below that names the port builds it from here — the systemd unit
/// included, which is why `serve_vllm_script` is a function and not a literal.
pub const VLLM_PORT: u16 = 8000;

/// The vLLM version this pipeline installs, pinned.
///
/// 0.10.0, and the number went DOWN on purpose. 0.27.1 was the latest release
/// (read 2026-08-14) and it does not work on this machine: it binds the port
/// and then answers `!!!!!` — a server that looks healthy to every check this
/// pipeline makes while computing wrong. The elimination that established it is
/// in the run 5 session, step 6 of the spec, and it ended on the only stack
/// measured to answer correctly here: `vllm==0.10.0` with
/// `transformers==4.53.2`, no sampler flag of any kind. Greedy decoding answers
/// "Four" to 2+2 and "Salut `<name>` !" to a French hello.
///
/// Pinned rather than floating, for the reason the driver is pinned: what runs
/// inside an enclave whose measurements end up in a report is not a version
/// chosen by whoever published last night. This one is pinned for a second
/// reason now — the newest release is the one that was wrong.
pub const VLLM_VERSION: &str = "0.10.0";

/// The transformers version `vllm==0.10.0` needs, and does NOT pin itself.
///
/// Measured 2026-08-14: `uv tool install vllm==0.10.0` alone resolves a
/// transformers the release predates, and the server dies at startup on
/// `AttributeError: … all_special_tokens_extended`. The pin is not tidiness,
/// it is the difference between a serving enclave and a crash loop.
pub const TRANSFORMERS_VERSION: &str = "4.53.2";

/// The model served, and a LOCAL constant on purpose.
///
/// SECURITY: this name reaches the systemd unit, so it decides what the enclave
/// downloads and executes. It may never come from remote text, from an
/// argument, or from anything the VM printed — a model name is a code path.
///
/// Qwen2.5-7B-Instruct is ungated (no HF token; gated models are out of scope,
/// spec 2026-08-14) and its weights are ~15 GB, which is the figure
/// `MODEL_TMPFS_MIN_KIB` and the start budget are sized against.
const VLLM_MODEL: &str = "Qwen/Qwen2.5-7B-Instruct";

/// The systemd unit this pipeline writes, reloads and starts.
const VLLM_UNIT: &str = "x0ne-vllm.service";

/// Where the weights live, and the reason this whole machine exists.
///
/// `/dev/shm` is a tmpfs, i.e. RAM, and the RAM of a TDX VM is
/// encrypted by the CPU with a key the host does not hold. The boot disk is a
/// different thing wearing a similar word — encrypted at rest by GCP, with a
/// key GCP holds (see `DiskEncryption`). Weights written to the disk are
/// readable by the platform; weights in `/dev/shm` are not. The price is that
/// they do not survive a reboot, which is why the one step that reboots
/// (`InstallGpuDriver`) runs before this one, and why a second reboot must
/// never be inserted after it (task 3 report, concern 4).
const MODEL_DIR: &str = "/dev/shm/x0ne-models";

/// Free space `FetchModel` requires in the tmpfs, in KiB (30 GiB).
///
/// Qwen2.5-7B-Instruct is ~15 GB of safetensors and the HF cache keeps one
/// copy, so 30 GiB is a 2× margin. It is a check rather than a formality
/// because `/dev/shm` defaults to HALF the RAM: 117 GiB on the 234 GiB
/// a3-highgpu-1g, ample — but that default is an image decision, and the
/// failure it prevents (a 15 GB download dying at 90 %) costs minutes of billed
/// H100 and reports itself as a network error.
const MODEL_TMPFS_MIN_KIB: u64 = 30 * 1024 * 1024;

// ── The in-VM commands ───────────────────────────────────────────────────
//
// Every remote command is a named constant and every answer is read by a
// parser from `x0ne_cloud::cc_plan` — the shape `attest_tdx` set. Nothing
// here reads remote text by hand: the machine printing it is the machine we
// are trying to make a statement about, so a `stdout.contains("ON")` is one
// process name away from being forged by its own subject.

/// The NVIDIA driver, pinned, in the ONE form a confidential GPU accepts.
///
/// `-open`, not the proprietary module: CC mode is a feature of the open GPU
/// kernel modules. `580`, not the `570` the task brief suggested: GCP's own
/// "Create a Confidential VM instance with GPU" procedure (docs.cloud.google
/// .com/confidential-computing/confidential-vm/docs/create-a-confidential-vm-
/// instance-with-gpu, read 2026-08-14) prescribes "version 580 or later of the
/// NVIDIA open kernel module drivers", and jammy carries
/// `nvidia-driver-580-server-open` 580.126.09-0ubuntu0.22.04.1 (launchpad.net/
/// ubuntu/jammy/+package/nvidia-driver-580-server-open, read 2026-08-14).
///
/// There is deliberately NO `|| ubuntu-drivers install --gpgpu` fallback. That
/// command installs the driver Ubuntu recommends, which may be the CLOSED
/// module — a driver that cannot enter CC mode. Substituting an unpinned,
/// unmeasured driver for the pinned one, silently, at 3.72 $/h, is exactly the
/// shape this repo keeps paying for. If the package name ever moves, apt says
/// so on stderr and the step fails where an operator can read it.
///
/// SECURITY: `linux-modules-nvidia-580-server-open-gcp` is not an extra, it is
/// what makes the other two do anything. We create the VM with
/// `--shielded-secure-boot` (our own argv, defence in depth), and Secure Boot
/// refuses to load a kernel module that carries no trusted signature. The
/// generic `nvidia-driver-…` package builds its module through DKMS, on the
/// machine, signed by nobody — so the module is REFUSED at load time while apt
/// exits 0. Measured on the real H100, run 3, 2026-08-14: apt said "already the
/// newest version", `update-initramfs` passed, the reboot was proven by a
/// changed boot_id, and `nvidia-smi` still exited 9 "couldn't communicate with
/// the NVIDIA driver" on BOTH sides of that reboot. Nothing in the install
/// output says why; only the missing signature does.
///
/// The fix is the package Canonical pre-signs and matches to the kernel
/// flavour: GCP's own "Install GPU drivers" page states that its general
/// instructions do not work under Secure Boot because "kernel modules built by
/// dkms don't work […] because they aren't properly signed by default", and
/// points at pre-signed drivers (docs.cloud.google.com/compute/docs/gpus/
/// install-drivers-gpu, read 2026-08-14). Ubuntu Server's NVIDIA page gives the
/// exact form — `linux-modules-nvidia-${BRANCH}${SERVER}-${FLAVOUR}` — and says
/// the DKMS route "\[is\] not signed with Canonical's key and thus do\[es\] not
/// support secure boot" (ubuntu.com/server/docs/how-to/graphics/
/// install-nvidia-drivers, read 2026-08-14). Our flavour is `gcp`
/// (`6.8.0-1064-gcp`), so the name is `-580-server-open-gcp`; it exists in
/// jammy and jammy-updates at 6.8.0-1065.73~22.04.1+1 (packages.ubuntu.com/
/// jammy/linux-modules-nvidia-580-server-open-gcp, read 2026-08-14 — verified,
/// not guessed).
///
/// The other two stay: `nvidia-driver-580-server-open` brings the userspace and
/// the dependency web, `nvidia-utils-580-server` brings `nvidia-smi` itself.
/// The meta package brings the ABI-pinned module —
/// `linux-modules-nvidia-580-server-open-6.8.0-1065-gcp` today — which in turn
/// pulls the matching kernel image, so the reboot this step already performs is
/// also what lands us on the kernel the signed module was built for.
pub const GPU_DRIVER_PACKAGES: &str = "nvidia-driver-580-server-open nvidia-utils-580-server \
     linux-modules-nvidia-580-server-open-gcp";

/// Install the driver and the two things a CONFIDENTIAL GPU additionally
/// needs, both from the GCP procedure above (read 2026-08-14):
///
///   * the LKCA modprobe rule — the driver negotiates its encrypted session
///     with the GPU through the kernel crypto API, and the rule is what loads
///     `ecdsa_generic`/`ecdh` before `nvidia`. It lives in the initramfs,
///     hence `update-initramfs -u`, hence the reboot in `install_gpu_driver`;
///   * UVM persistence mode, so the GPU's confidential state is not torn down
///     and rebuilt between processes.
///
/// `set -e` because every line is a precondition of the next: a failed
/// `apt-get install` followed by a successful `update-initramfs` would exit 0
/// and report a driver nobody installed.
///
/// `-k all`, not the bare `-u` of the GCP page: since the signed-module fix
/// (see `GPU_DRIVER_PACKAGES`) the apt line can pull a NEWER kernel — the one
/// its ABI-pinned module was built for — and that kernel's initramfs is
/// generated by apt, before the line below writes `nvidia-lkca.conf`. A bare
/// `-u` rebuilds only the RUNNING kernel's image, so the machine would reboot
/// into the new kernel with an initramfs that has never seen the LKCA rule, and
/// the driver would come up without its crypto channel to the GPU.
const GPU_DRIVER_INSTALL_SCRIPT: &str = "sudo sh -c 'set -e; \
     export DEBIAN_FRONTEND=noninteractive; \
     apt-get update -q; \
     apt-get install -y -q linux-headers-$(uname -r) build-essential pkg-config; \
     apt-get install -y -q nvidia-driver-580-server-open nvidia-utils-580-server \
     linux-modules-nvidia-580-server-open-gcp; \
     echo \"install nvidia /sbin/modprobe ecdsa_generic; /sbin/modprobe ecdh; \
/sbin/modprobe --ignore-install nvidia\" > /etc/modprobe.d/nvidia-lkca.conf; \
     update-initramfs -u -k all; \
     if test -f /usr/lib/systemd/system/nvidia-persistenced.service; then \
       sed -i s/no-persistence-mode/uvm-persistence-mode/g \
         /usr/lib/systemd/system/nvidia-persistenced.service; \
       systemctl daemon-reload; \
     fi'";

/// Reboot, detached, so the ssh command we are inside can exit 0 first.
///
/// A bare `sudo reboot` kills its own connection and comes back as exit 255 —
/// indistinguishable from "the VM was unreachable", which is the one thing
/// this step must not confuse. Backgrounding a 2 s delay lets the command
/// return normally; `wait_reboot` then proves the machine really went down and
/// came back, rather than assuming it.
const GPU_REBOOT_SCRIPT: &str = "sudo sh -c 'nohup sh -c \"sleep 2; reboot\" >/dev/null 2>&1 &'";

/// The kernel's per-boot identifier. Changes on every boot and on nothing
/// else, which is what makes "did it actually reboot?" a question with an
/// answer instead of a sleep.
const BOOT_ID_CMD: &str = "cat /proc/sys/kernel/random/boot_id";

/// The same read, with a pause in front of it, used while waiting for the
/// reboot.
///
/// The pause is REMOTE for two reasons. The exec layer's own pause only fires
/// on a failed probe (`cc_cmd.rs`, keyed on `WAIT_SSH_DISPLAY`), and the case
/// this loop has to survive is the opposite one: ssh answering perfectly on a
/// VM that has simply not gone down yet. And putting the boot id we are
/// comparing against into the remote command line — the other way to make the
/// probe fail on its own — would splice text the VM produced into a shell
/// command running as root on that VM. The comparison stays in Rust.
const BOOT_ID_WAIT_CMD: &str = "sh -c 'sleep 5; cat /proc/sys/kernel/random/boot_id'";

/// Is the driver loaded and answering? Read by `parse_nvidia_smi_ready`, which
/// is deliberately harder to fool than a `contains("NVIDIA-SMI")` — the classic
/// failure message contains that marker too.
const NVIDIA_SMI_CMD: &str = "nvidia-smi";

/// The GPU's confidential-compute mode. `sudo`, as GCP's verification page
/// prescribes (read 2026-08-14); read by `parse_nvidia_cc_mode`.
const GPU_CC_MODE_CMD: &str = "sudo nvidia-smi conf-compute -f";

/// The driver version that ends up in the report. `--format=csv,noheader` so
/// the answer is the value and nothing else.
const GPU_DRIVER_VERSION_CMD: &str = "nvidia-smi --query-gpu=driver_version --format=csv,noheader";

/// Is the confidential GPU ARMED — i.e. may CUDA initialise on it? Read by
/// `parse_gpu_ready_state`. `sudo`, like every other `conf-compute` read.
const GPU_READY_STATE_CMD: &str = "sudo nvidia-smi conf-compute -grs";

/// Arm it.
///
/// Run 4 of the H100 milestone, 2026-08-14, measured on the real VM: vLLM
/// crash-looped 46 times on `RuntimeError: cudaGetDeviceCount ... Error 802:
/// system not yet initialized`; `-grs` answered `not-ready`; this one command
/// made the 802 disappear. In NVIDIA's confidential mode the GPU refuses CUDA
/// until the GUEST declares it ready, which is the whole point — the guest is
/// the party that can attest it first.
///
/// SECURITY: `1` is a literal, never a variable, and this command runs only
/// from `CcStep::ArmGpu`, which the plan places AFTER `AttestGpu`. The order is
/// the security property (see that variant's doc-comment): attest, then arm.
const GPU_ARM_CMD: &str = "sudo nvidia-smi conf-compute -srs 1";

// ── vLLM: install, weights, unit, and the two things the in-VM checks read ──

/// Install `uv`, then vLLM at the pinned version.
///
/// `uv` rather than pip-in-a-venv: it resolves and links the CUDA torch stack
/// in a fraction of the time, and `uv tool install` puts a single entry point
/// in `~/.local/bin` with its own isolated environment — no interpreter state
/// shared with the system Python, nothing to activate from a systemd unit.
///
/// `$HOME/.local/bin/uv` is spelled out because the command runs through a
/// NON-interactive shell: `~/.profile`, which is what puts `~/.local/bin` on
/// the PATH of an Ubuntu login, is not sourced. A bare `uv` here works when
/// tried by hand and fails from the pipeline.
///
/// `set -e` for the reason the driver script has it: a failed `curl | sh`
/// followed by a successful anything would exit 0 and report an install nobody
/// performed.
/// `--python {VLLM_PYTHON}` is the load-bearing flag, and it is a function
/// rather than a literal for the reason `serve_vllm_script` is: the version and
/// the interpreter must reach the install and the acquis probe from the SAME
/// place, or the probe blesses an environment the install never built.
///
/// `--with transformers==…` because `vllm==0.10.0` does not put an upper bound
/// on transformers, so the resolver picks a version published long after it and
/// the server dies at startup on `AttributeError: …
/// all_special_tokens_extended` (measured 2026-08-14 on `x0ne-cc`: 0.10.0
/// without the pin crash-loops, with it it serves). `--with` rather than a
/// second `tool install`: it lands in the SAME isolated environment as the
/// entry point, which is the only environment the unit ever runs.
fn vllm_install_script() -> String {
    format!(
        "sh -c 'set -e; \
         curl -LsSf https://astral.sh/uv/install.sh | sh; \
         \"$HOME/.local/bin/uv\" tool install --force --python {VLLM_PYTHON} \
         --with \"transformers=={TRANSFORMERS_VERSION}\" \
         vllm=={VLLM_VERSION}'"
    )
}

/// Is the PINNED vLLM already installed?
///
/// The version is checked, not just the binary's existence, and the comparison
/// happens in the remote shell so that no Rust code reads the answer — the
/// signal is the exit code, the same design as the `x0ne --version` probe.
/// Checking only "does vllm answer" would let a VM whose disk carries an
/// earlier pin skip the install and serve a version this pipeline never chose.
///
/// Known imprecision, in the harmless direction: `grep -F` is a substring
/// match, so a hypothetical 0.10.01 would satisfy a 0.10.0 pin. The converse
/// error — a false "not installed" — only re-runs an idempotent install.
///
/// What it does NOT check is `transformers`, and that gap is deliberate rather
/// than forgotten: `--with` puts the pin in the same environment as the entry
/// point, so an install that produced this `vllm` produced that `transformers`
/// too. If the two ever come apart, the symptom is loud — the server dies at
/// startup — which is the failure mode this probe is allowed to leave to the
/// crash-loop detector.
///
/// It asks TWO questions since run 4 (2026-08-14), and the second one is the
/// fix. `vllm --version` answers identically from a 3.10 environment and from a
/// 3.12 one, and the 3.10 one is broken: on Ubuntu 22.04's system python,
/// importing flashinfer dies with `TypeError: 'type' object is not
/// subscriptable` (`fd_exchange.py`, the annotation `array.array[int]`, which
/// needs PEP 585 generics on `array`). Measured on the real VM: rebuilt with
/// `--python 3.12`, the import passes. A probe that only asked about the
/// version would let a VM carrying the old environment — a resumed SPOT disk,
/// which is the normal case — skip the reinstall and crash again, which is the
/// dead-scaffolding shape: an acquis that answers a question next to the one
/// that matters.
///
/// `$HOME/.local/share/uv/tools/vllm/bin/python` is where `uv tool install`
/// puts the tool's own interpreter (read on the VM, 2026-08-14). The comparison
/// stays in the remote shell, like the version one: the signal is the exit
/// code, and no Rust code reads remote text to determine.
fn vllm_version_cmd() -> String {
    format!(
        "sh -c '\"$HOME/.local/bin/vllm\" --version 2>/dev/null | grep -qF {VLLM_VERSION} && \
         \"{VLLM_TOOL_PYTHON}\" --version 2>/dev/null | grep -qF \"Python {VLLM_PYTHON}.\"'"
    )
}

/// The Python `uv` must build the vLLM tool environment on, pinned.
///
/// 3.12 and not "whatever the image ships": Ubuntu 22.04 ships 3.10, and the
/// vLLM 0.27.1 the pipeline used to install pulled a flashinfer whose
/// `fd_exchange.py` annotates `array.array[int]` — a PEP 585 subscript that
/// raises `TypeError: 'type' object is not subscriptable` there. Measured at
/// run 4, 2026-08-14: crash at import on 3.10, clean import after `uv tool
/// install --force --python 3.12`.
///
/// The pin OUTLIVED its first reason, and stays. The proven stack
/// (`VLLM_VERSION`) ships no flashinfer at all, so that particular import
/// cannot happen again — but 3.12 is the interpreter the working configuration
/// was measured on, and "the version we proved" includes the interpreter that
/// ran it. Reverting to the image's python would be a change nobody measured.
///
/// It costs NOTHING on the system: `uv` downloads a managed CPython into its
/// own directory. No apt, no deadsnakes PPA, no `/usr/bin/python3` touched —
/// which matters on a machine whose whole claim is a minimal attested surface.
pub const VLLM_PYTHON: &str = "3.12";

/// Where `uv tool install` puts the tool's own interpreter. Read on the VM at
/// run 4 (2026-08-14).
const VLLM_TOOL_PYTHON: &str = "$HOME/.local/share/uv/tools/vllm/bin/python";

/// Prepare the tmpfs the weights live in, and print what is free in it.
///
/// `sudo mkdir` then `chown` to the ssh user: the directory has to be writable
/// by the account the systemd unit runs as, and that account is not root.
/// Nothing is downloaded here — see `FetchModel`'s doc-comment for where the
/// 15 GB is actually paid for.
///
/// `RemoveIPC=no` FIRST, and it is the finding of run 5 (2026-08-14): without
/// it the pipeline deletes its own weights. logind ships `RemoveIPC=yes` by
/// default, which removes every POSIX IPC object owned by a user when that
/// user's LAST session ends — and a file under `/dev/shm` is a POSIX IPC
/// object. The unit runs as the ssh user but a service is not a session, so
/// nothing holds the account logged in: every probe this pipeline makes is an
/// ssh session whose close wipes `/dev/shm`. That is the 14.2 GB → 20 bytes
/// seen between two probes of run 5, and it is very probably what run 4 read as
/// a "slow download": the 15 GB never survived long enough to finish.
///
/// Measured on `x0ne-cc`, 2026-08-14: 10 MB written to `/dev/shm/ipc-test`,
/// ssh session closed, next session found `No such file or directory`;
/// `busctl … login1.Manager RemoveIPC` answered `b true`. The unit itself was
/// NOT the culprit — `systemctl show x0ne-vllm -p RemoveIPC` said `no`.
/// Sources: <https://github.com/systemd/systemd/issues/4532>,
/// <https://wiki.postgresql.org/wiki/Systemd> (same fix, same reason).
///
/// It lives HERE rather than next to the unit because it has to be true before
/// the first byte is downloaded, and `FetchModel` is the last step that runs
/// before `ServeVllm` pays for the 15 GB. Idempotent: writing the same drop-in
/// is a no-op, and restarting `systemd-logind` does not close open sessions.
/// A drop-in rather than an edit of `logind.conf`: the file is the distro's,
/// and `toml_edit`-style respect for a user's commented config is the repo rule
/// even when the format is `.conf`.
///
/// And it is RE-READ, from the live daemon, in the same probe: `busctl` asks
/// logind itself rather than the file we just wrote. Writing a drop-in proves
/// nothing about the process that had already read its configuration — the rule
/// `arm_gpu` states at length and `install_vllm` follows too. A file on disk and
/// a daemon in the state we wanted are two different facts.
fn fetch_model_script() -> String {
    format!(
        "sh -c 'set -e; \
         sudo mkdir -p /etc/systemd/logind.conf.d; \
         printf \"%s\\n\" \"[Login]\" \"RemoveIPC=no\" \
           | sudo tee {LOGIND_DROPIN} >/dev/null; \
         sudo systemctl restart systemd-logind; \
         sudo mkdir -p {MODEL_DIR}; \
         sudo chown \"$(id -un)\" {MODEL_DIR}; \
         echo {TMPFS_PROBE_MARK_REMOVEIPC}; {LOGIND_REMOVEIPC_CMD}; \
         echo {TMPFS_PROBE_MARK_AVAIL}; df --output=avail -k /dev/shm'"
    )
}

/// Where the `RemoveIPC=no` drop-in goes. `10-` so a later drop-in can still
/// override it: this is the pipeline's floor, not a claim on the file's name.
///
/// `pub` for the same reason as `VLLM_PORT`: the plan line an operator approves
/// names this file, and a literal copy on that side would let the plan announce
/// a path the pipeline no longer writes. Measured by mutation (review of
/// `c9a2808`): renaming the constant left both suites green while the two
/// diverged.
pub const LOGIND_DROPIN: &str = "/etc/systemd/logind.conf.d/10-x0ne-keep-shm.conf";

/// Ask the LIVE logind what it will do, not what its config file says.
const LOGIND_REMOVEIPC_CMD: &str = "busctl get-property org.freedesktop.login1 \
     /org/freedesktop/login1 org.freedesktop.login1.Manager RemoveIPC";

/// The listening sockets, read by `parse_ss_localhost_only`. No `sudo`: the
/// process column it would add is precisely the column that parser refuses to
/// read (a process can be named `127.0.0.1:8000` by whoever starts it).
const SS_LISTEN_CMD: &str = "ss -tlnp";

/// Active swap areas, read by `parse_swapon_empty`. Empty output is the
/// documented shape of "there is none" (util-linux `swapon(8)`).
const SWAPON_CMD: &str = "swapon --show";

/// The three questions ONE probe of the vLLM wait asks, in one ssh session.
///
/// Run 4, 2026-08-14, measured: the wait watched the PORT and nothing else for
/// 30 minutes and 1.26 $ while the unit crash-looped 46 times behind it, and
/// the run blamed a slow download. An unbound port is compatible with a server
/// that is working and with one that is dying every twenty seconds; nothing the
/// loop asked could tell them apart.
///
/// ONE command rather than three, and the reason is the money: this loop runs
/// up to `VLLM_START_RETRIES` times and every extra ssh session multiplies the
/// most expensive ceiling of the pipeline. Three commands would turn a 30 min
/// ceiling into 90.
///
/// `|| echo 0 {MODEL_DIR}` rather than letting `du` fail: a model directory
/// that is not there yet is "nothing has been downloaded", which the stall
/// detector already names precisely. Sending an exit code instead would make
/// the whole probe unreadable and report the wrong thing.
fn vllm_wait_probe_cmd() -> String {
    format!(
        "sh -c 'echo {VLLM_PROBE_MARK_LISTEN}; {SS_LISTEN_CMD}; \
         echo {VLLM_PROBE_MARK_RESTARTS}; systemctl show {VLLM_UNIT} -p NRestarts; \
         echo {VLLM_PROBE_MARK_MODELDIR}; du -sk {MODEL_DIR} 2>/dev/null || echo 0 {MODEL_DIR}'"
    )
}

/// What the unit has been saying while nobody was reading. Quoted — bounded —
/// in the crash-loop failure, because a diagnosis that says "it restarts" and
/// not "why" sends the operator back over ssh for the line we already had.
///
/// `sudo` because the ssh user's group membership on the journal is an image
/// decision, and this runs at the one moment where a permission error would
/// cost the diagnosis.
fn vllm_journal_cmd() -> String {
    format!("sudo journalctl -u {VLLM_UNIT} -n 40 --no-pager")
}

/// Ask vLLM what it serves. `-f` so an HTTP error is a non-zero exit rather
/// than an error page landing in the JSON parser; `--max-time 5` so a hung
/// server costs a probe, not the ssh budget.
fn vllm_models_cmd() -> String {
    format!("curl -fsS --max-time 5 http://127.0.0.1:{VLLM_PORT}/v1/models")
}

/// Write the systemd unit, byte for byte, then load and start it.
///
/// A function, not a constant, because `VLLM_PORT` and `MODEL_DIR` must reach
/// the unit from the same place `ValidateEncryption` reads them (ledger point 3
/// of task 4): a unit serving 8000 while the validation checks 8001 would prove
/// nothing about the machine it ran on, and a literal is exactly how the two
/// drift apart.
///
/// `printf '%s\n' <line> <line> …` rather than a heredoc: every line is a
/// separate argv element, so no line of the unit can be produced by expansion
/// of another, and the whole thing survives being carried through gcloud, ssh
/// and a remote shell as one argument. The two expansions that DO happen are
/// deliberate and local to the VM — `$(id -un)` and `$HOME`, because systemd
/// expands neither, and the account that owns `~/.local/bin/vllm` and the tmpfs
/// is the ssh user rather than root.
///
/// `--host 127.0.0.1` is the load-bearing flag of the in-VM checks: vLLM's default is
/// `0.0.0.0`, i.e. the external IP of a cloud VM, i.e. an unauthenticated model
/// server on the internet. `ValidateEncryption` re-reads the actual socket
/// rather than trusting this line.
///
/// NO sampler flag, and that absence is the RESULT, not an oversight. The unit
/// carried `Environment=VLLM_USE_FLASHINFER_SAMPLER=0` from run 4 to run 5; the
/// path that flag forces is the broken one. `CUDA_LAUNCH_BLOCKING=1` named the
/// kernel on 2026-08-14 — `topk_topp_sampler.py:394`, the top-k `gather` of
/// vLLM 0.27.1's torch fallback, a real bug in that fallback — and every fix
/// above it only moved the failure: with prebuilt flashinfer kernels the server
/// bound its port and answered `!!!!!`.
///
/// What followed is the elimination that decided this whole file, all of it
/// measured on `x0ne-cc` the same evening. Greedy decoding was degenerate too,
/// so the sampler was not the cause. A clean re-download (14.2 GiB exactly,
/// against 16.9 carrying debris from interrupted resumes) produced the same
/// gibberish, so the weights were not the cause. Bare torch — matmul, argmax,
/// softmax — was EXACT, so the platform was not the cause. `--enforce-eager`
/// changed nothing (not CUDA Graphs), `TORCH_SDPA` changed nothing (not the
/// attention backend), and plain `transformers` on the same GPU answered
/// "Four" correctly. That is the verdict: the native vLLM 0.27.1 stack
/// (torch 2.13+cu130) computes silently WRONG on this platform, and no flag in
/// this unit could have fixed it.
///
/// So the fix is the stack, not the unit: `vllm==0.10.0` with
/// `transformers==4.53.2`, no flashinfer, no sampler flag — measured serving
/// "Four" and "Salut <name> !". Adding an `Environment=` line back here is
/// therefore a regression unless a measure says otherwise: the proven
/// configuration is the one with nothing in it.
///
/// `rm -rf …/{VLLM_UNIT}.d` before the reload, because this pipeline owns ALL
/// of the unit's state and a drop-in it did not write is state it does not
/// control. Concretely: a diagnostic override left at `=1` on the VM's disk
/// survives every rewrite of the unit file and silently wins over it (measured
/// 2026-08-14 — such residue was sitting there). The unit file being right is
/// not the same fact as the unit being right, and this is the whole class of
/// ghost drop-ins, not one stale file.
///
/// `reset-failed` between the reload and the start, because the crash-loop
/// detector reads `NRestarts` and that counter survives every restart WITHIN a
/// boot: the 43 run 5 opened with was the residue of the boot before it.
/// Measured on `x0ne-cc`, 2026-08-14: `NRestarts=0` on a fresh boot, then 3
/// three minutes later on the same boot. The detector compares two readings, so
/// it survived a dirty start — but it is one logic slip away from calling a
/// crash-loop on a counter nobody in this run incremented, and the fix is one
/// word.
///
/// `enable` then `restart`, NEVER `enable --now`, and this is the step where
/// everything above it becomes real or does not. `--now` starts a unit that is
/// stopped and is a no-op on one that is already running — and this unit IS
/// already running by the time we get here: it was enabled by an earlier run,
/// so systemd started it at boot, with the file and the drop-ins that were on
/// the disk THEN. `daemon-reload` does not touch running units either
/// (systemd#31541). Without an explicit `restart`, this script would write a
/// clean unit, purge the overrides, reload — and leave the enclave serving the
/// environment nobody in this run measured, with the correct file asleep on
/// disk. That is precisely the dead-scaffolding shape, applied to the invariant
/// the paragraphs above just established: a fix that exists, is documented, is
/// even ON the machine, and changes nothing.
///
/// `TimeoutStopSec=15` is what makes that restart affordable, and run 6
/// (2026-08-14) is the measure: the restart above inherited systemd's default
/// 90 s stop grace, the unit being stopped was mid-download, and the step's own
/// ssh budget (60 s then) died waiting on a stop that was behaving normally.
/// Bounding the stop is legitimate here in a way it would not be for a database:
/// a vLLM being stopped holds NOTHING worth preserving. The weights are in
/// `HF_HOME` and re-verifiable, the KV cache is disposable by definition, and
/// there is no write path to interrupt. So SIGKILL at 15 s costs nothing and
/// bounds every future stop — including the ones no code here performs, like an
/// operator's `systemctl stop` or a shutdown. The step budget
/// (`VLLM_UNIT_WRITE_TIMEOUT_SECS`) is sized on this number; the two move
/// together or not at all.
fn serve_vllm_script() -> String {
    format!(
        "sh -c 'set -e; printf \"%s\\n\" \
         \"[Unit]\" \
         \"Description=x0ne confidential vLLM ({VLLM_MODEL})\" \
         \"After=network-online.target\" \
         \"\" \
         \"[Service]\" \
         \"User=$(id -un)\" \
         \"Environment=HF_HOME={MODEL_DIR}\" \
         \"ExecStart=$HOME/.local/bin/vllm serve {VLLM_MODEL} --host 127.0.0.1 --port {VLLM_PORT}\" \
         \"Restart=on-failure\" \
         \"TimeoutStopSec=15\" \
         \"\" \
         \"[Install]\" \
         \"WantedBy=multi-user.target\" \
         | sudo tee /etc/systemd/system/{VLLM_UNIT} >/dev/null; \
         sudo rm -rf /etc/systemd/system/{VLLM_UNIT}.d; \
         sudo systemctl daemon-reload; \
         sudo systemctl reset-failed {VLLM_UNIT}; \
         sudo systemctl enable {VLLM_UNIT}; \
         sudo systemctl restart {VLLM_UNIT}'"
    )
}

// ── x0ne itself, inside the enclave (Increment 3) ────────────────────────

/// The version the binaries copied into the VM must answer with.
///
/// `env!` of THIS crate, and the workspace is versioned as one
/// (`version.workspace = true` in all ten crates, read 2026-08-14), so the
/// number an `x0ne` built from this tree prints is this number. It is a LOCAL
/// constant for the reason `VLLM_MODEL` is one: it decides what counts as
/// "already installed", so it may never come from the VM.
pub const X0NE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Is the x0ne of THIS build installed and configured in the VM?
///
/// SECURITY, and it is the exact lesson of finding I2 of task 4 (the foreign
/// model): "installed" is a question about the VERSION, never about a file
/// existing. A boot disk that came back from a SPOT preemption carries the
/// binaries of whatever run last touched it, and a probe reading "there is an
/// `x0ne` in the home directory" would drop `InstallX0ne` from the plan and
/// leave the operator talking to a binary this pipeline never copied.
///
/// The comparison happens in the REMOTE shell and the signal is the exit code
/// alone — no Rust code reads a byte of what the VM printed, the design of
/// `VLLM_VERSION_CMD`. Known imprecision, in the harmless direction and shared
/// with that constant: `grep -F` is a substring match, so a hypothetical 0.2.01
/// would satisfy a 0.2.0 pin; the converse error only re-runs an idempotent
/// copy.
///
/// The two config files are part of the question, not a separate one. A run
/// interrupted between the scp and the config write leaves binaries at the
/// right version and no config at all — the state where a probe about the
/// binary alone says "acquired" and the enclave's x0ne then boots into the
/// setup wizard against no model. One command, one answer, used verbatim by
/// both the step and the re-plan probe (`probe_in_vm`), so the two cannot
/// drift into disagreeing about what "ready" means.
///
/// `--version` and NOT `--health`, which is what the task asked for, and the
/// substitution is deliberate: `--health` does not do what its own help text
/// says. `x0ne-code/src/main.rs:228` documents "run a quick health check of LLM
/// and Bridge connectivity and exit", and `main.rs:464` is the ONLY place the
/// flag is ever read — it suppresses the boot animation and nothing else. There
/// is no early return behind it. Measured 2026-08-14, `x0ne --health --no-intro
/// </dev/null`: full boot, the REPL banner, and exit 0 at EOF.
///
/// So the brief's acquis would have been the worst of both — over ssh with no
/// tty it boots a REPL, sits there until `IN_VM_PROBE_TIMEOUT_SECS` or reads
/// EOF, and exits 0 either way. A probe that returns 0 because a binary STARTED
/// is a false green about the one step the operator is asked to trust. It is the
/// repo's named defect, "dead scaffolding", instance five: a flag that exists,
/// is documented, is read, and changes nothing. Fixing `--health` is not this
/// task's business; relying on it would have been.
fn x0ne_ready_cmd() -> String {
    x0ne_ready_cmd_with(StderrOfVersion::Discarded)
}

/// The same four checks, with the `--version` diagnostics LEFT ALONE — the
/// form `install_x0ne` verifies with.
///
/// Run 8 (2026-08-15) is why this exists. The step copied two binaries built
/// on a Debian 13 host (glibc 2.41) into an Ubuntu 22.04 VM (glibc 2.35); the
/// loader refused them with `version 'GLIBC_2.39' not found`, on STDERR, and
/// the `2>/dev/null` below deleted the sentence on the VM before ssh could
/// carry it home. What reached the operator was "exit 1", after ~30 min of a
/// 3.72 $/h pipeline that had otherwise passed every check — the diagnosis
/// then cost a manual `objdump -T` session on the host. The probe's quiet form
/// is kept (it runs on VMs where the binary legitimately is not there yet, and
/// its answer is an exit code), but a VERIFICATION that fails owes its reason.
///
/// Same checks, byte for byte, save the redirection: the acquis probe and the
/// verification must still recognise the same state, or a run that did the
/// work and a run that skipped it stop being indistinguishable to the next
/// `x0ne cc` — which is what `install_x0ne`'s doc-comment means by resumable.
fn x0ne_verify_cmd() -> String {
    x0ne_ready_cmd_with(StderrOfVersion::Kept)
}

/// Whether the `--version` calls get to speak. See `x0ne_verify_cmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StderrOfVersion {
    Discarded,
    Kept,
}

/// The one definition of "this VM has THIS x0ne, ready" — parameterised only
/// by what happens to the two `--version` diagnostics, so the probe and the
/// verification cannot drift into checking different things.
fn x0ne_ready_cmd_with(stderr: StderrOfVersion) -> String {
    let quiet = match stderr {
        StderrOfVersion::Discarded => " 2>/dev/null",
        StderrOfVersion::Kept => "",
    };
    format!(
        "sh -c '\"$HOME/x0ne\" --version{quiet} | grep -qF {X0NE_VERSION} && \
         \"$HOME/x0ne-runtime\" --version{quiet} | grep -qF {X0NE_VERSION} && \
         test -f \"$HOME/.x0ne/config.toml\" && \
         test -f \"$HOME/.config/x0ne/x0ne-runtime.toml\"'"
    )
}

/// The placeholder the VM's own `$HOME` replaces in both config files.
///
/// The home directory of the ssh account is not knowable from here — `gcloud
/// compute ssh` derives the remote user from the local one, from
/// `--ssh-key-file`, or from OS Login, and none of those is a path this side
/// can compute. So the files travel with a token and the REMOTE shell expands
/// it, the same way `serve_vllm_script` lets `$(id -un)` and `$HOME` expand on
/// the machine that knows them.
const ENCLAVE_HOME_TOKEN: &str = "@X0NE_HOME@";

/// `~/.x0ne/config.toml` for the enclave: one backend, and it is on loopback.
///
/// SECURITY: every value here is a local constant. The endpoint is the vLLM
/// this pipeline started and PROVED is bound to 127.0.0.1 only
/// (`validate_encryption`), and the model is the one `health_check` matched by
/// name. Nothing in this file points off the VM: an x0ne started in the
/// enclave talks to a server on its own loopback, over RAM the host cannot
/// read, and needs no API key to do it.
fn enclave_x0ne_config() -> String {
    format!(
        "# x0ne — written by `x0ne cc` when it installed this binary in the enclave.\n\
         #\n\
         # The model is served by the x0ne-vllm systemd unit on THIS machine, on\n\
         # 127.0.0.1 only, out of /dev/shm (TDX-encrypted RAM). No key, no egress:\n\
         # a conversation held here does not leave the VM.\n\
         \n\
         [llm]\n\
         provider = \"openai-compat\"\n\
         \n\
         [llm.openai_compat]\n\
         name = \"enclave-vllm\"\n\
         base_url = \"http://127.0.0.1:{VLLM_PORT}/v1\"\n\
         model = \"{VLLM_MODEL}\"\n\
         max_tokens = 4096\n\
         \n\
         [bridge]\n\
         mode = \"stdio\"\n\
         # Absolute, because the binary lands in the home directory and a\n\
         # command run over ssh does not source ~/.profile — a bare name would\n\
         # be looked up on a PATH that never had the home directory on it.\n\
         x0ne_core_bin = \"{ENCLAVE_HOME_TOKEN}/x0ne-runtime\"\n"
    )
}

/// `~/.config/x0ne/x0ne-runtime.toml` for the enclave: policy schema v0.3,
/// deny by default, confined to the VM's home.
///
/// SECURITY, and this is the point of the whole increment: what runs in the
/// enclave is a DEMO OF CONFINEMENT, not an open sandbox. `default_effect =
/// "deny"` means an action this runtime does not know about is refused rather
/// than waved through, the read and write roots are the home directory alone,
/// and the command list is the short read-only set. The VM is disposable, but
/// "it is disposable" has never been a reason to ship a permissive policy —
/// the operator watching this demo is being shown what x0ne's confinement
/// looks like.
///
/// # v2, owner's arbitration of 2026-08-15
///
/// The owner held his first real session in the demo enclave (run 9, the one
/// that printed ENCLAVE PROVEN) and reported: "I find it's pretty locked
/// down … like it can't read anything … couldn't we maybe free it up a bit
/// more?". Two things were measured against this very file, with the real
/// `x0ne-runtime` binary, on 2026-08-15:
///
/// 1. `allowed_write_paths = ["$HOME"]` was DEAD SCAFFOLDING — the repo's
///    recurring defect, a flag that is written, documented, read, and changes
///    nothing. There was no `[actions]` table, so `AppConfig::load` merged the
///    compiled-in defaults (`config.rs:546-551`), which derive each switch from
///    its side-effect class: everything in `FsWrite` is OFF. The policy
///    announced home-wide writes and the agent could not write one byte —
///    every `WriteFile` came back "This action type is disabled by
///    configuration". The write root was never reached.
/// 2. `RunCommand` was off for the same reason (`ProcessSpawn` is OFF by
///    class), so `uname -a` was refused before `allowed_commands` was ever
///    consulted. The read-only command list, too, was scaffolding.
///
/// So v2 states the five switches EXPLICITLY rather than inheriting them: the
/// four confined-path writers, and `RunCommand`. Everything else keeps the
/// class default — snapshots, trash, the destructives, the desktop and every
/// network action stay shut, and `default_effect = "deny"` still catches an
/// action this build does not know about.
///
/// SECURITY: in THIS policy the frontier is EGRESS, not paths. The whole VM is
/// the enclave: memory is TDX-encrypted, the model is served on 127.0.0.1 out
/// of `/dev/shm`, and nothing in either config file points off the machine.
///
/// So the repo's known debt — "`RunCommand` bypasses path confinement, `cat`
/// reads what `ReadFile` refuses" (docs/invariants.md) — is ACCEPTED HERE
/// by the owner's arbitration of 2026-08-15, and it is accepted KNOWING WHAT
/// IT COSTS. An earlier version of this comment said the debt cost nothing new
/// because "the whole home is already readable, so `cat` reaches no byte
/// `ReadFile` would not hand over". That was false, and measured false on this
/// very policy on 2026-08-15:
///
/// ```text
/// ReadFile   <home>/.ssh/id_rsa          → DENIED, matches '**/.ssh/**'
/// RunCommand cat <home>/.ssh/id_rsa      → SUCCESS, key printed
/// RunCommand cat <outside-the-root>/x    → SUCCESS
/// ```
///
/// `cat`, `head`, `tail` and `grep` reach BOTH the `deny_path_patterns` and
/// everything outside the read root. `deny_path_patterns` binds `ReadFile`
/// only; it is not a property of the VM, and nothing here should be read as
/// keeping the keys out of the agent's reach in general.
///
/// What is actually load-bearing is the command list, and only its content:
/// nothing in it may be a way OUT (`curl`, `wget`, `ssh`, `scp`, `gcloud`,
/// `nc`), an interpreter (`sh`, `bash`, `python3` — an arbitrary program, and
/// therefore both), or a WRITER. `sort` was in this list until 2026-08-15,
/// when `sort -o <outside-the-root>/planted.txt <home>/.ssh/id_rsa` was
/// measured copying the key out; it is gone, and `agent/readonly_cmd.rs` now
/// gates `sort -o`, `uniq IN OUT` and `tree -o` on argv.
///
/// `find` stays, and it is the sharp edge left. The RUNTIME allowlist matches
/// the program name and never looks at argv, so `find . -exec /bin/sh -c …`
/// passes this policy — measured planting a file outside the root on
/// 2026-08-15. The only barrier is the orchestrator's human gate, which
/// refuses `-exec`/`-delete`/`-fprintf` (`readonly_cmd.rs`, `FIND_ACTIONS`).
/// That gate is THE barrier for `find` here, not a second line of defence.
///
/// `mode = "auto"`, NOT `"confirm"`, and that is not a relaxation. In the
/// RUNTIME, `mode = "confirm"` means "deny every modifying action": the runtime
/// is a confined executor with nobody to ask, so `PolicyEffect::
/// RequireConfirmation` comes back to the caller as `PolicyDenied`
/// (`x0ne-runtime/src/executor/mod.rs:178`, `policy.rs:157`). That shipped once
/// — the wizard wrote it, and every approved `RunCommand` was refused with
/// "Action requires user approval" AFTER the operator had approved it (field
/// report 2026-08-12, quoted in `setup.rs:19-28`). It is repo invariant 2, the
/// double-gating: the human gate lives in the ORCHESTRATOR, which here runs on
/// the same VM, in the same terminal, in front of the same human.
///
/// Schema v0.3: `default_effect`, `max_read_bytes`, `max_list_entries`,
/// `allowed_commands`, `allowed_read_paths`, `allowed_write_paths` and
/// `deny_path_patterns` are all REQUIRED (no serde default), and the v0.2 key
/// `allowed_dirs` no longer exists. A file missing one of them is a runtime
/// that does not boot — the 2026-08-10 incident, on every fresh install.
fn enclave_runtime_policy() -> String {
    format!(
        "# x0ne-runtime — written by `x0ne cc` (InstallX0ne) inside the TDX enclave.\n\
         #\n\
         # This VM exists to show what a confined agent runtime looks like: deny\n\
         # by default, the home directory and nothing else, a read-only command\n\
         # list. The agent may read and write anywhere under the home and run the\n\
         # commands below; it cannot reach the network, the desktop, the\n\
         # snapshots or anything destructive.\n\
         #\n\
         # The line that must not move is EGRESS. The whole VM is the enclave —\n\
         # encrypted memory, a model served on 127.0.0.1 out of /dev/shm — so a\n\
         # conversation held here does not leave the machine. Adding curl, wget,\n\
         # ssh, scp, gcloud or nc to allowed_commands breaks that, and so does\n\
         # adding an interpreter (sh, bash, python3), which is any program at all.\n\
         #\n\
         # mode = \"auto\" and not \"confirm\": in the RUNTIME, \"confirm\" means\n\
         # deny — it has no human to ask (field report 2026-08-12). The human\n\
         # gate is the orchestrator's, and the orchestrator is right here.\n\
         \n\
         [policy]\n\
         mode = \"auto\"\n\
         default_effect = \"deny\"\n\
         max_read_bytes = 1048576\n\
         max_list_entries = 512\n\
         allowed_read_paths = [\"{ENCLAVE_HOME_TOKEN}\"]\n\
         allowed_write_paths = [\"{ENCLAVE_HOME_TOKEN}\"]\n\
         deny_path_patterns = [\"**/.ssh/**\", \"**/.gnupg/**\", \"**/id_rsa*\", \
         \"**/id_ed25519*\", \"**/*.key\", \"**/*.pem\", \"**/secrets/**\"]\n\
         # No writer, no interpreter, no way out. `sort` was here until\n\
         # 2026-08-15: `sort -o` writes, and it was measured copying an ssh key\n\
         # outside the write root. `find` stays because the orchestrator's gate\n\
         # refuses -exec/-delete/-fprintf on argv — the runtime only matches the\n\
         # program name, so that gate is the ONLY barrier for it.\n\
         allowed_commands = [\"ls\", \"cat\", \"head\", \"tail\", \"grep\", \"find\", \
         \"wc\", \"du\", \"df\", \"free\", \"ps\", \"id\", \"whoami\", \
         \"hostname\", \"uname\", \"date\", \"ss\", \"nvidia-smi\"]\n\
         \n\
         # Stated, not inherited. Without this table every one of these is OFF:\n\
         # the merged defaults derive each switch from the action's side-effect\n\
         # class, and FsWrite and ProcessSpawn are off by class. That is what\n\
         # made allowed_write_paths and allowed_commands above unreachable until\n\
         # 2026-08-15. Everything NOT listed here keeps its class default —\n\
         # network, desktop, snapshots and destructives stay shut.\n\
         [actions]\n\
         WriteFile = true\n\
         EditFile = true\n\
         ApplyPatch = true\n\
         PatchSet = true\n\
         RunCommand = true\n\
         # The two the compiled defaults leave ON as \"outbound but payload-free\".\n\
         # That trade is right on a workstation and wrong here: a DNS query\n\
         # carries whatever you put in the name, and a port probe is a reachable\n\
         # host plus one bit per attempt. Both are packets leaving the VM, which\n\
         # is the one thing this enclave promises never happens.\n\
         DnsLookup = false\n\
         PortCheck = false\n\
         \n\
         [runtime]\n\
         mode = \"stdio\"\n\
         \n\
         [runtime.http]\n\
         host = \"127.0.0.1\"\n\
         port = 7878\n\
         \n\
         [desktop]\n\
         enabled = false\n"
    )
}

/// Write both config files, byte for byte, and make the copied binaries
/// executable.
///
/// The files travel BASE64-ENCODED, and that is the quoting decision of this
/// step. Both are TOML, i.e. both are full of double quotes, and they have to
/// survive a Rust literal, a gcloud argument, an ssh command line and two
/// levels of remote shell. `printf '%s\n' <line> <line>` — the device
/// `serve_vllm_script` uses — costs one escape per quote at every level, and
/// the failure mode of getting one wrong is a config file that parses into
/// something subtly different from what this file says. Base64 has no
/// metacharacter at all: what arrives is what was encoded, or `base64 -d`
/// fails and `set -e` stops the step.
///
/// The ONE substitution is `$HOME`, done by `sed` on the VM — see
/// `ENCLAVE_HOME_TOKEN`. It is an expansion into a FILE, never into a command,
/// and nothing the VM printed is read back here.
///
/// `chmod +x` because a binary that arrives without its exec bit fails at the
/// exact moment the operator types `e` — long after this step reported
/// success. scp does carry the source mode, so this is belt and braces; it
/// costs nothing and the failure it covers is the one nobody would suspect.
fn write_enclave_configs_script() -> String {
    use base64::Engine;
    let cfg = base64::engine::general_purpose::STANDARD.encode(enclave_x0ne_config());
    let policy = base64::engine::general_purpose::STANDARD.encode(enclave_runtime_policy());
    format!(
        "sh -c 'set -e; \
         chmod +x \"$HOME/x0ne\" \"$HOME/x0ne-runtime\"; \
         mkdir -p \"$HOME/.x0ne\" \"$HOME/.config/x0ne\"; \
         printf %s \"{cfg}\" | base64 -d | sed \"s#{ENCLAVE_HOME_TOKEN}#$HOME#g\" \
         > \"$HOME/.x0ne/config.toml\"; \
         printf %s \"{policy}\" | base64 -d | sed \"s#{ENCLAVE_HOME_TOKEN}#$HOME#g\" \
         > \"$HOME/.config/x0ne/x0ne-runtime.toml\"; \
         chmod 600 \"$HOME/.x0ne/config.toml\"'"
    )
}

/// Largest upload `InstallX0ne` budgets for, in MiB, both binaries together.
///
/// Measured on this workspace 2026-08-14: `target/release/x0ne` is 20.2 MiB and
/// `target/release/x0ne-runtime` 20.2 MiB, so 40.5 MiB together. 200 is a 5×
/// margin, which is what covers a debug-symbol build or a binary that grows a
/// feature between now and the H100 milestone.
pub const X0NE_BINARIES_MAX_MIB: u64 = 200;

/// The slowest uplink this budget is willing to survive, in KiB/s.
///
/// 512 KiB/s ≈ 4 Mbit/s of UPSTREAM, which is a domestic ADSL line. The number
/// that matters is not the typical case (a fibre link does 40 MiB in seconds)
/// but the one where the runtime HARD-KILLS a transfer at 95 % and the step
/// reports a network error nobody can act on.
pub const SCP_MIN_KIB_PER_SEC: u64 = 512;

/// The ssh half of an scp: handshake, host key, and — on a project whose key
/// was never pushed — the same one-time work `SSH_FIRST_PROBE_TIMEOUT_SECS`
/// exists for.
pub const SCP_HANDSHAKE_SECS: u64 = 60;

/// Worst-case wall clock of the binary upload, in seconds. Derived, never
/// typed: this figure is quoted in the plan line an operator reads at
/// 3.72 $/h, and a promise typed as a literal stops tracking the code that
/// makes it (`wait_ssh_budget_secs`, and the "~5 min" that was really 2.6 h).
///
/// 200 MiB ÷ 512 KiB/s + 60 s = 460 s ≈ 7.7 min ≈ **0.48 $**. COST of the whole
/// step, since the budget gate only runs BETWEEN steps: the acquis probe (60 s)
/// + this upload (460 s) + the config write (60 s) + the verification probe
/// (60 s) = 640 s ≈ **0.66 $**, the cheapest of the four ceilings in this
/// pipeline.
pub const fn scp_budget_secs() -> u64 {
    X0NE_BINARIES_MAX_MIB * 1024 / SCP_MIN_KIB_PER_SEC + SCP_HANDSHAKE_SECS
}

/// Longest driver version string the report will print.
///
/// `nvidia-smi` prints something like `580.126.09`. The cap is not about that
/// — it is about the other producer of this string, a VM that has every reason
/// to make the report unreadable. `sanitize` already removes the control
/// characters; nothing else bounds the LENGTH, and a megabyte of `A` inside a
/// report line is as effective at hiding a verdict as an ANSI escape.
const DRIVER_VERSION_MAX_CHARS: usize = 64;

/// Longest echo of the command WE sent that a step failure quotes back.
///
/// Not a trust bound — these scripts are built from local constants. A
/// legibility bound: `write_enclave_configs_script` carries two base64 blobs
/// (~2.5 KiB), and a failure that reprints them in full pushes the VM's own two
/// lines of diagnosis off the screen. 300 chars keeps the `sh -c 'set -e; …`
/// opening, which is what identifies the step.
const REMOTE_CMD_ECHO_MAX_CHARS: usize = 300;
/// The kept tail of a failure's stdout/stderr — enough for several
/// lines of diagnostics, too little to flood the screen.
const REMOTE_FAILURE_TAIL_MAX_CHARS: usize = 2000;

/// Longest echo of an unreadable `conf-compute -f` answer the refusal message
/// quotes back. Enough to recognise "not supported" or a truncated line; not
/// enough for the VM to bury the refusal under its own output.
const CC_MODE_ECHO_MAX_CHARS: usize = 200;

/// Same bound, other producer: the last thing vLLM answered before the health
/// wait gave up. Some of what lands here is already bounded by the parser's own
/// `quoted_excerpt`, but not all of it — a list of served model ids is remote
/// text of the machine's own choosing — and a give-up message that scrolls its
/// own next step off the screen is a message nobody acts on.
const VLLM_ECHO_MAX_CHARS: usize = 200;

/// Same reflex, larger bound, and it is the one message of this pipeline that
/// EARNED its length: what `install_x0ne`'s verification failed with.
///
/// It carries the echoed remote command (already bounded at
/// `REMOTE_CMD_ECHO_MAX_CHARS`) plus both streams of a `--version` that did not
/// answer — and since run 8 (2026-08-15) that is where the loader writes
/// `version 'GLIBC_2.39' not found`, i.e. the entire diagnosis of a run that
/// otherwise passed every check. 1200 characters leaves ~900 for the VM's own
/// words, ~3× a loader refusal.
const X0NE_VERIFY_ECHO_MAX_CHARS: usize = 1200;

/// Worst-case wall clock of the `WaitSsh` step, in seconds.
///
/// Derived, never written by hand — including the first probe's larger budget:
/// the plan text and the failure message both quote this duration to someone
/// paying by the hour, and they used to say "~5 min" because that was true of
/// an earlier set of constants. A promise typed as a literal stops tracking
/// the code that makes it; that one went on being printed while the real
/// ceiling was 2.6 h.
pub const fn wait_ssh_budget_secs() -> u64 {
    SSH_FIRST_PROBE_TIMEOUT_SECS
        + (SSH_RETRIES as u64 - 1) * SSH_PROBE_TIMEOUT_SECS
        + SSH_RETRIES as u64 * SSH_PROBE_PAUSE_SECS
}

/// Worst-case wall clock of the reboot wait, in seconds.
///
/// NOT `wait_ssh_budget_secs()` (review round 1, minor M-a): that one includes
/// `SSH_FIRST_PROBE_TIMEOUT_SECS`, the 90 s granted to an ssh invocation that
/// has a key to generate and push. By the time a VM is rebooting, that work
/// happened long ago — `WaitSsh` did it — so `wait_reboot` gives every probe
/// the warm budget, and quoting the other figure overstated the ceiling by
/// more than a minute to someone paying by the hour. Same lesson as the
/// hand-typed "~5 min" it replaced: a promise that stops tracking the code
/// that makes it goes on being printed.
pub const fn reboot_wait_budget_secs() -> u64 {
    SSH_RETRIES as u64 * (SSH_PROBE_TIMEOUT_SECS + SSH_PROBE_PAUSE_SECS)
}

/// Worst-case wall clock of the wait for vLLM to bind its port, in seconds.
///
/// Derived, and quoted by both the plan line and the give-up message, for the
/// reason `wait_ssh_budget_secs` exists: this is the longest wait in the whole
/// pipeline and it is stated to someone paying 3.72 $/h. Every attempt costs
/// its pause, including the last one, because the exec pauses on every call
/// carrying `WAIT_VLLM_DISPLAY` — see that constant.
pub const fn vllm_start_budget_secs() -> u64 {
    VLLM_START_RETRIES as u64 * (VLLM_PROBE_TIMEOUT_SECS + VLLM_PROBE_PAUSE_SECS)
}

/// Worst-case wall clock of the health wait, in seconds. Same derivation, a
/// sixth of the attempts — see `VLLM_HEALTH_RETRIES` for why the port being
/// bound is what makes the short budget honest.
pub const fn vllm_health_budget_secs() -> u64 {
    VLLM_HEALTH_RETRIES as u64 * (VLLM_PROBE_TIMEOUT_SECS + VLLM_PROBE_PAUSE_SECS)
}

impl<'a> CcRunner<'a> {
    pub fn new(exec: &'a mut dyn CcExec, config: CcConfig) -> Self {
        Self {
            exec,
            config,
            verdict: None,
            claims: None,
            nonce_hex: String::new(),
            gpu: None,
            encryption: None,
            preflight: None,
            binaries: None,
            transport_down: false,
        }
    }

    /// Has the transport under this runner died?
    ///
    /// SECURITY (money): the caller asks this to determine whether the safety stop
    /// can be issued through THIS exec or needs a fresh runtime. Answering
    /// `false` when the bridge is dead sends the stop into a corpse and leaves
    /// the meter running — the exact outcome of the first real H100 run
    /// (2026-08-14), where the bridge died and the stop died with it.
    pub fn transport_is_down(&self) -> bool {
        self.transport_down
    }

    /// Record a transport death, if that is what this result is.
    ///
    /// Called on the two entry points a caller can drive — `run_step` and
    /// `stop_vm` — rather than around each of the nine `exec.run` sites: an
    /// inner probe whose error is swallowed on purpose (`ssh_probe`) must not
    /// set this flag from underneath a step that went on to succeed.
    fn note_transport<T>(&mut self, r: &anyhow::Result<T>) {
        if let Err(e) = r {
            if ExecTransportDown::is_fatal(e) {
                self.transport_down = true;
            }
        }
    }

    /// Hand the runner the binaries `InstallX0ne` is to upload.
    ///
    /// Separate from `new` so the twelve other call sites — every one of which
    /// runs a pipeline that has no `InstallX0ne` in it — keep their signature,
    /// and so that "the paths were never supplied" stays a state the step can
    /// refuse rather than a `PathBuf::new()` it would try to send.
    pub fn set_x0ne_binaries(&mut self, binaries: X0neBinaries) {
        self.binaries = Some(binaries);
    }

    fn ssh_cmd(&self, remote: &str) -> Vec<String> {
        let spec = self.config.machine.vm_spec(&self.config.vm_name);
        vec![
            "compute".into(),
            "ssh".into(),
            self.config.vm_name.clone(),
            format!("--zone={}", spec.zone),
            "--command".into(),
            remote.into(),
        ]
    }

    async fn gcloud(&mut self, display: &str, argv: &[&str]) -> anyhow::Result<CmdOut> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let out = self.exec.run(display, &argv, GCLOUD_TIMEOUT_SECS).await?;
        if out.exit_code != 0 {
            anyhow::bail!(
                "`gcloud {}` exited {}: {} {}",
                argv.join(" "),
                out.exit_code,
                out.stdout,
                out.stderr
            );
        }
        Ok(out)
    }

    /// The three probes.
    ///
    /// Every `--format` is a PROJECTION (`json(field,…)`), never a bare
    /// `json`, and that is a correctness requirement rather than a taste:
    /// `RunCommand` caps captured output at 24 000 bytes
    /// (`executor/process.rs:156`) and splices a "bytes truncated" marker
    /// into the middle of what is left. A single Ubuntu image record is
    /// ~31 KB of JSON (measured on ubuntu-os-cloud, 2026-08-13), so the
    /// unprojected `--format=json` came back as invalid JSON,
    /// `parse_image_family_exists` read that as "absent", and `x0ne cc`
    /// refused to plan on a project where the family was right there. The
    /// projection also states, in the command itself, exactly which fields
    /// the parsers below depend on.
    pub async fn observe(&mut self) -> anyhow::Result<CcObservations> {
        let auth = self
            .gcloud(
                "probe auth",
                &[
                    "auth",
                    "list",
                    "--filter=status:ACTIVE",
                    "--format=json(account,status)",
                ],
            )
            .await?;
        let project = self
            .gcloud("probe project", &["config", "get-value", "project"])
            .await?;
        let spec = self.config.machine.vm_spec(&self.config.vm_name);
        let images = self
            .gcloud(
                "probe image family",
                &[
                    "compute",
                    "images",
                    "list",
                    "--project",
                    &spec.image_project,
                    &format!("--filter=family={}", spec.image_family),
                    "--format=json(name)",
                    "--limit=1",
                ],
            )
            .await?;
        let instance = self.observe_instance().await?;
        let project_str = project.stdout.trim().to_string();
        let obs = CcObservations {
            auth_account: parse_auth_account(&auth.stdout),
            project: (!project_str.is_empty() && project_str != "(unset)").then_some(project_str),
            image_family_exists: Some(parse_image_family_exists(&images.stdout)),
            instance,
            // The four in-VM probes stay `None` HERE, on purpose, and that is
            // not an unwired probe — it is where they cannot be read. This
            // runs before the VM is up: on a fresh or stopped instance there
            // is no ssh to ask through, and `full_h100_steps` ignores these
            // fields outright unless the instance is Running.
            //
            // They are read by `replan_after_ssh`, once ssh answers. The two
            // together give the property this pipeline needs: the plan the
            // human approves is the MAXIMAL one (every install listed, nothing
            // hidden), and the re-probe can only ever remove steps from it.
            gpu_driver_ready: None,
            gpu_cc_on: None,
            vllm_healthy: None,
            x0ne_ready: None,
        };
        self.preflight = Some(obs.clone());
        Ok(obs)
    }

    /// The second half of `observe`: what is already done INSIDE the VM, read
    /// once the CPU enclave is PROVEN, and the plan rebuilt from it.
    ///
    /// SECURITY (review round 1, finding I3): the caller runs this after
    /// `AttestTdx`, not after `WaitSsh`, and the difference is one ssh session
    /// of cost for the whole trust story. Everything read here is an assertion
    /// the VM makes about itself, and those assertions REMOVE steps from a plan
    /// a human approved. Made before the quote verifies, they are the claims of
    /// a machine nothing has vouched for yet — the same reason `build_plan`
    /// hangs every GPU step off `AttestTdx` rather than beside it. After it,
    /// they are the claims of an attested kernel, which is the strongest
    /// statement this milestone has to offer.
    ///
    /// Why this exists at all (task 3 review, pin 1 — it is the fifth dead
    /// scaffolding of this repo, caught before it was built). The four in-VM
    /// observations are only meaningful while the instance is Running, and
    /// `full_h100_steps` enforces that by ignoring them otherwise. But the
    /// case this machine is BUILT for is SPOT preemption: GCP stops the VM,
    /// the disk survives, and the next `x0ne cc` observes `Stopped`. At that
    /// moment every in-VM field is `None` — correctly — so the plan contains
    /// every install again. If nothing re-probed after `StartVm` + `WaitSsh`,
    /// the pipeline would reinstall a driver that is already there, refetch
    /// weights that are already there, and the resume-by-detection design
    /// would be decorative on the one machine it was designed for.
    ///
    /// Re-planning rather than patching a step list: `build_plan` already
    /// knows how to turn observations into steps, and it is pure and tested.
    /// Calling it a second time with fresher facts is cheaper and far more
    /// honest than a second, imperative implementation of the same rules
    /// living in the runner — which is how the two would drift apart.
    ///
    /// SECURITY: the result is always a SUBSEQUENCE of `approved_tail`. The
    /// human approved a printed list of steps (repo invariant 5); a re-plan
    /// may drop work that is already done, but it must never ADD a step
    /// nobody read — least of all one derived from text the VM itself
    /// printed. Anything unexpected (no pre-flight to rebuild from, a refusal,
    /// a plan that is not a subsequence) returns the approved tail unchanged:
    /// doing the approved work twice costs minutes, doing unapproved work
    /// costs the invariant.
    pub async fn replan_after_ssh(&mut self, approved_tail: &[CcStep]) -> Vec<CcStep> {
        let unchanged = approved_tail.to_vec();
        // The prover machine has no in-VM acquis in its plan at all: four ssh
        // round trips would answer a question nobody asked.
        if self.config.machine != CcMachine::FullH100 || approved_tail.is_empty() {
            return unchanged;
        }
        let Some(preflight) = self.preflight.clone() else {
            return unchanged;
        };

        let fresh = CcObservations {
            // ssh just answered, so the instance is Running and it is ours —
            // `build_plan` refused a zone mismatch before any of this ran.
            instance: InstanceState::Running {
                zone: self.config.machine.vm_spec(&self.config.vm_name).zone,
            },
            ..self.probe_in_vm(preflight).await
        };
        let Ok(plan) = build_plan(&self.config, &fresh) else {
            // Unreachable in practice: the refusals are about auth, project,
            // image family and zone, and those came from the same pre-flight
            // that already produced an accepted plan. If it ever happens, the
            // approved plan is the safe answer, not a refusal invented here.
            return unchanged;
        };
        // Everything up to and including AttestTdx has already run — that is
        // where the caller stands when it asks. `build_plan` emits exactly one
        // of each of these and always as the prefix, so dropping them is
        // dropping the past, not filtering the future.
        let tail: Vec<CcStep> = plan
            .into_iter()
            .skip_while(|s| {
                matches!(
                    s,
                    CcStep::CreateVm | CcStep::StartVm | CcStep::WaitSsh | CcStep::AttestTdx
                )
            })
            .collect();
        if is_subsequence(&tail, approved_tail) {
            tail
        } else {
            unchanged
        }
    }

    /// The four in-VM acquis probes. Read-only, and none of them can fail the
    /// pipeline: a probe that cannot be read leaves its field `None`, which
    /// `build_plan` reads as "not observed" and plans the work for.
    ///
    /// SECURITY: `.ok()` here is NOT the `unwrap_or` the task-2 report warns
    /// about, and the difference is the whole design. An unreadable answer
    /// becomes "not observed" — the step runs, and the step is where the
    /// verdict is made: `check_gpu_cc` re-reads the same command and FAILS on
    /// the same unreadable output. No verdict is ever defaulted here; only
    /// work is scheduled.
    async fn probe_in_vm(&mut self, base: CcObservations) -> CcObservations {
        let smi = self.ssh_probe("probe GPU driver", NVIDIA_SMI_CMD).await;
        let cc = self.ssh_probe("probe GPU cc-mode", GPU_CC_MODE_CMD).await;
        let models = self.ssh_probe("probe vLLM", &vllm_models_cmd()).await;
        let x0ne = self.ssh_probe("probe x0ne", &x0ne_ready_cmd()).await;

        CcObservations {
            gpu_driver_ready: smi.map(|o| parse_nvidia_smi_ready(&o.stdout)),
            gpu_cc_on: cc.and_then(|o| parse_nvidia_cc_mode(&o.stdout).ok()),
            // SECURITY (task 4, review round 1, finding I2): "healthy" is
            // "serving OUR model", not "serving something".
            //
            // This one observation retires `ServeVllm` AND `HealthCheck`
            // (`full_h100_steps`), and `HealthCheck` is the step whose whole
            // job is checking the model's IDENTITY. Read as "answering with a
            // non-empty list", a VM already serving `evil/backdoored-13b` on
            // 127.0.0.1:8000 dropped both steps from the plan: nothing would
            // ever have written our unit, and nothing would ever have compared
            // the name — the pipeline would go on to install `x0ne` against a
            // model nobody chose. Measured on the re-plan, and it is only not a
            // live hole today because `InstallX0ne` still fails as
            // unimplemented, i.e. by accident.
            //
            // An empty `data` remains unhealthy for the original reason: a
            // server that came up without its weights is exactly what the
            // fetch/serve steps exist to fix.
            vllm_healthy: models
                .and_then(|o| parse_vllm_models(&o.stdout).ok())
                .map(|ids| ids.iter().any(|id| id == VLLM_MODEL)),
            // No parser: the signal IS the exit code of `x0ne_ready_cmd`, which
            // is the step's own definition of ready — the RIGHT VERSION plus
            // its two config files, not "a file called x0ne exists". `Some
            // (false)` when the VM has none of that or has an older build,
            // `None` only when ssh itself did not answer.
            x0ne_ready: x0ne.map(|o| o.exit_code == 0),
            ..base
        }
    }

    /// The instance probe alone.
    ///
    /// Split out of `observe` (review round 1, finding I5) for the error path:
    /// when `create`/`start` fails, whether the meter is running is a
    /// QUESTION WITH AN ANSWER, and the caller was guessing instead of asking.
    /// Asking it through the full `observe` would re-run auth/project/image —
    /// three calls that cannot have changed, and any of which failing would
    /// bury the one answer we came for.
    pub async fn observe_instance(&mut self) -> anyhow::Result<InstanceState> {
        let instances = self
            .gcloud(
                "probe instance",
                &[
                    "compute",
                    "instances",
                    "list",
                    &format!("--filter=name={}", self.config.vm_name),
                    "--format=json(name,status,zone)",
                ],
            )
            .await?;
        parse_instance_state(&instances.stdout, &self.config.vm_name)
            .map_err(|e| anyhow::anyhow!("{}: {}", e.what, e.detail))
    }

    /// Run one step, and remember if the transport died doing it.
    ///
    /// The wrapper exists so `transport_down` has ONE writer on this path. The
    /// dispatch below is unchanged; splitting it out is what lets the flag be
    /// set from the single place every step's result passes through, rather
    /// than from each arm — where the next step added would forget it, which
    /// is how this repo grows flags nobody sets.
    pub async fn run_step(&mut self, step: &CcStep) -> anyhow::Result<()> {
        let outcome = self.dispatch_step(step).await;
        self.note_transport(&outcome);
        outcome
    }

    async fn dispatch_step(&mut self, step: &CcStep) -> anyhow::Result<()> {
        match step {
            CcStep::CreateVm => {
                let spec = self.config.machine.vm_spec(&self.config.vm_name);
                let argv = spec.to_gcloud_argv();
                let display = spec.to_display_command();
                let out = self.exec.run(&display, &argv, GCLOUD_TIMEOUT_SECS).await?;
                if out.exit_code != 0 {
                    anyhow::bail!(
                        "create failed ({}): {} {}",
                        out.exit_code,
                        out.stdout,
                        out.stderr
                    );
                }
                Ok(())
            }
            CcStep::StartVm => {
                let spec = self.config.machine.vm_spec(&self.config.vm_name);
                let vm_name = self.config.vm_name.clone();
                let zone_arg = format!("--zone={}", spec.zone);
                self.gcloud(
                    "start VM",
                    &["compute", "instances", "start", &vm_name, &zone_arg],
                )
                .await?;
                Ok(())
            }
            CcStep::WaitSsh => self.wait_ssh().await,
            CcStep::AttestTdx => self.attest_tdx().await,
            CcStep::InstallGpuDriver => self.install_gpu_driver().await,
            CcStep::CheckGpuCc => self.check_gpu_cc().await,
            CcStep::AttestGpu => self.attest_gpu().await,
            CcStep::ArmGpu => self.arm_gpu().await,
            CcStep::InstallVllm => self.install_vllm().await,
            CcStep::FetchModel => self.fetch_model().await,
            CcStep::ServeVllm => self.serve_vllm().await,
            CcStep::ValidateEncryption => self.validate_encryption().await,
            CcStep::HealthCheck => self.health_check().await,
            CcStep::InstallX0ne => self.install_x0ne().await,
            CcStep::Report => Ok(()), // rendered by the caller
        }
    }

    async fn wait_ssh(&mut self) -> anyhow::Result<()> {
        // The exec layer owns real sleeping — the production impl pauses
        // `SSH_PROBE_PAUSE_SECS` on a failed probe, keyed on
        // `WAIT_SSH_DISPLAY`; the runner only counts attempts, which is what
        // makes this testable without a clock.
        for attempt in 0..SSH_RETRIES {
            // The first probe is doing one-time work the others are not:
            // generating the SSH key and pushing it to project metadata. It
            // gets its own budget — see `SSH_FIRST_PROBE_TIMEOUT_SECS`. The
            // attempt index is the whole signal, passed as an argument rather
            // than through any shared state.
            let timeout_secs = if attempt == 0 {
                SSH_FIRST_PROBE_TIMEOUT_SECS
            } else {
                SSH_PROBE_TIMEOUT_SECS
            };
            let argv = self.ssh_cmd("true");
            match self.exec.run(WAIT_SSH_DISPLAY, &argv, timeout_secs).await {
                Ok(out) if out.exit_code == 0 => return Ok(()),
                // The VM answered "not yet". That is what the retries are for.
                Ok(_) => {}
                // The bridge is dead: the probe never left this process, so no
                // number of further probes can reach the machine. 26 of them
                // is what the first real run spent finding that out, and the
                // message it ended on accused the VM. See `ExecTransportDown`.
                Err(e) if ExecTransportDown::is_fatal(&e) => return Err(e),
                // Any other error is a probe that failed on its way — retryable
                // exactly as before.
                Err(_) => {}
            }
        }
        anyhow::bail!(
            "SSH not reachable after {SSH_RETRIES} attempts (up to {} min). The VM may still \
             be booting; probe by hand with: gcloud compute ssh {} --command true",
            wait_ssh_budget_secs().div_ceil(60),
            self.config.vm_name
        )
    }

    /// SECURITY: a fresh nonce per attempt is the anti-replay half of the
    /// proof; the quote must echo it in report_data or it is someone else's
    /// (or yesterday's) quote.
    fn fresh_nonce() -> anyhow::Result<[u8; 64]> {
        use std::io::Read;
        let mut b = [0u8; 64];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
        Ok(b)
    }

    async fn attest_tdx(&mut self) -> anyhow::Result<()> {
        let nonce = Self::fresh_nonce()?;
        let nonce_hex = tdx_quote::hex(&nonce);
        // configfs-tsm is the kernel's standard TDX quote interface (>= 6.7).
        // If the path is absent on this image, the error IS the measurement —
        // it answers the Ubuntu 26.04-vs-24.04 question the program spec asks.
        let remote = format!(
            "sudo sh -c 'mkdir -p /sys/kernel/config/tsm/report/x0ne && \
             printf %s {nonce_hex} | xxd -r -p > /sys/kernel/config/tsm/report/x0ne/inblob && \
             base64 -w0 /sys/kernel/config/tsm/report/x0ne/outblob'"
        );
        let argv = self.ssh_cmd(&remote);
        let out = self
            .exec
            .run("fetch TDX quote (configfs-tsm)", &argv, GCLOUD_TIMEOUT_SECS)
            .await?;
        if out.exit_code != 0 {
            anyhow::bail!(
                "quote fetch failed on the VM: {} {}",
                out.stdout.trim(),
                out.stderr.trim()
            );
        }
        // Decode stdout ALONE: it must be pure base64 (see the SECURITY note
        // on CmdOut::stderr). Any diagnostic text the remote shell wrote goes
        // to stderr and never touches this decode.
        use base64::Engine;
        let quote = base64::engine::general_purpose::STANDARD
            .decode(out.stdout.trim())
            .map_err(|e| anyhow::anyhow!("quote is not valid base64: {e}"))?;
        let (claims, verdict) = tdx_quote::verify(&quote, &nonce);
        self.claims = claims;
        self.nonce_hex = nonce_hex;
        // SECURITY (review round 1, finding C1): a REJECTED verdict fails the
        // STEP. Storing it and returning `Ok(())` was the bug — the loop only
        // ever looks at the step's result, so nothing downstream consulted
        // `verdict` and the pipeline walked straight past an unproven enclave:
        // a 37 min driver install, a reboot, and the whole vLLM chain on a VM
        // whose quote did not verify. `build_plan` says in as many words that
        // no GPU work happens until the CPU enclave is proven, and the owner's
        // rule (2026-08-13) is "never proceed without proof"; both were
        // enforced by nobody. Failing here puts them back in the code: the
        // step fails, `run_plan` stops and turns the meter off, and `outcome`
        // renders REFUSED rather than PROVEN.
        let rejection = match &verdict {
            Verdict::Rejected { reason, .. } => Some(reason.clone()),
            Verdict::Verified { .. } => None,
        };
        self.verdict = Some(verdict);
        if let Some(reason) = rejection {
            anyhow::bail!("attestation refused: {reason}");
        }
        Ok(())
    }

    /// One ssh command, failing the step on a non-zero exit.
    ///
    /// Both streams travel in the error, for the reason `CmdOut::stderr`
    /// documents: apt, nvidia-smi and curl write their diagnostics to stderr,
    /// and an error message that only exists on stderr used to be swallowed.
    ///
    /// The COMMAND is bounded like everything else this message quotes, and it
    /// is not about trust — `remote` is ours, built from local constants. It is
    /// about legibility: `write_enclave_configs_script` is ~2.5 KiB of base64,
    /// and a failure that pastes all of it in front of the two lines the VM
    /// actually said hides the diagnosis behind the payload. Same reflex as
    /// `CC_MODE_ECHO_MAX_CHARS`, other producer.
    async fn ssh_step(
        &mut self,
        display: &str,
        remote: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<CmdOut> {
        let argv = self.ssh_cmd(remote);
        let out = self.exec.run(display, &argv, timeout_secs).await?;
        if out.exit_code != 0 {
            // The VM's output is bounded by its TAIL (the diagnosis is
            // usually the last line), never unbounded — a megabyte of noise
            // pushed the verdict off screen — and never head-truncated, which
            // would eat the diagnosis instead. See `tail_snippet`.
            anyhow::bail!(
                "`{}` failed on the VM (exit {}): {} {}",
                snippet(remote, REMOTE_CMD_ECHO_MAX_CHARS),
                out.exit_code,
                crate::text_safety::tail_snippet(out.stdout.trim(), REMOTE_FAILURE_TAIL_MAX_CHARS),
                crate::text_safety::tail_snippet(out.stderr.trim(), REMOTE_FAILURE_TAIL_MAX_CHARS)
            );
        }
        Ok(out)
    }

    /// One ssh command that is allowed to fail.
    ///
    /// `None` means the command layer itself did not answer (no runtime, no
    /// ssh, a timeout). A non-zero exit is NOT `None`: `nvidia-smi` exits
    /// non-zero precisely when it has something to say about a missing driver,
    /// and that output is the probe's answer.
    ///
    /// An `ExecTransportDown` is swallowed here like any other failure, and
    /// that is deliberate rather than an oversight of the 2026-08-14 fix — but
    /// the honest cost is not "one command", as this comment claimed until
    /// review round 3 counted the call sites:
    ///
    ///   * the four `if let Some(out) = ssh_probe(…)` acquis checks each stand
    ///     immediately before an `ssh_step` that propagates, so a dead
    ///     transport costs ONE doomed command there;
    ///   * `probe_in_vm` runs FOUR in a row (:1309-1312) with no step between
    ///     them, so a transport that dies at the first costs four. What they
    ///     produce is still safe — every field lands `None`, `build_plan` reads
    ///     that as "not observed" and prunes NOTHING, and the next step's
    ///     `ssh_step` is what reports the death.
    ///
    /// Four wasted probes against the cost of teaching an `Option`-returning
    /// helper to distinguish two kinds of failure. What made the wait loops
    /// worth converting was never the swallow itself, it was the twenty-six
    /// attempts and the diagnosis that blamed the VM.
    async fn ssh_probe(&mut self, display: &str, remote: &str) -> Option<CmdOut> {
        let argv = self.ssh_cmd(remote);
        self.exec
            .run(display, &argv, IN_VM_PROBE_TIMEOUT_SECS)
            .await
            .ok()
    }

    /// Install the NVIDIA driver, configure LKCA, reboot, and prove the driver
    /// came back.
    ///
    /// The reboot is not caution, it is the procedure (GCP, read 2026-08-14):
    /// the LKCA modprobe rule lives in the initramfs, and until the machine
    /// boots on the rebuilt one the driver has no encrypted channel to the
    /// GPU. Doing it inside this step rather than as a plan step of its own
    /// keeps `CcStep` matching the operator's mental model — "install the
    /// driver" is one thing that happens — and the plan line says so.
    ///
    /// Idempotent by observation, twice over: `replan_after_ssh` usually drops
    /// this step before it runs, and if it runs anyway it starts by asking the
    /// machine. On a 3.72 $/h instance a redundant driver install is ~10 min
    /// of billed time.
    async fn install_gpu_driver(&mut self) -> anyhow::Result<()> {
        if let Some(out) = self.ssh_probe("check GPU driver", NVIDIA_SMI_CMD).await {
            if parse_nvidia_smi_ready(&out.stdout) {
                return Ok(());
            }
        }
        self.ssh_step(
            &format!("install the NVIDIA driver ({GPU_DRIVER_PACKAGES}) and LKCA"),
            GPU_DRIVER_INSTALL_SCRIPT,
            GPU_DRIVER_TIMEOUT_SECS,
        )
        .await?;

        // Read BEFORE the reboot: this is the value `wait_reboot` must see
        // change. Asking afterwards would compare the new boot to itself.
        let before = self
            .ssh_step("read the boot id", BOOT_ID_CMD, IN_VM_PROBE_TIMEOUT_SECS)
            .await?
            .stdout
            .trim()
            .to_string();
        if before.is_empty() {
            anyhow::bail!(
                "cannot read {BOOT_ID_CMD} on the VM, so a reboot could not be told from a \
                 machine that never went down — refusing to continue"
            );
        }
        self.ssh_step(
            "reboot for the LKCA initramfs",
            GPU_REBOOT_SCRIPT,
            IN_VM_PROBE_TIMEOUT_SECS,
        )
        .await?;
        self.wait_reboot(&before).await?;

        // The step's own verdict, from the machine, after the reboot. An
        // `apt-get` that exits 0 and a driver that answers are two different
        // facts, and only the second one is the one the next steps need.
        let out = self
            .ssh_step(
                "verify the driver answers",
                NVIDIA_SMI_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        if !parse_nvidia_smi_ready(&out.stdout) {
            anyhow::bail!(
                "the driver packages installed but `{NVIDIA_SMI_CMD}` does not report a loaded \
                 driver after the reboot — check `dkms status` and \
                 /var/log/nvidia-installer.log on the VM"
            );
        }
        Ok(())
    }

    /// Wait until the VM comes back on a DIFFERENT boot.
    ///
    /// SECURITY-adjacent, and the reason this is not a `sleep`: every step
    /// after this one reads the GPU and turns what it reads into the report.
    /// A probe that answered from the machine as it was BEFORE the reboot
    /// would let the pipeline attest a kernel that is about to be replaced.
    /// The boot id is the kernel's own statement about which boot answered.
    async fn wait_reboot(&mut self, before: &str) -> anyhow::Result<()> {
        for _ in 0..SSH_RETRIES {
            // WAIT_SSH_DISPLAY on purpose: while the machine is down the probe
            // fails, and that label is what makes the production exec pause
            // between attempts instead of burning the retries in a second.
            let argv = self.ssh_cmd(BOOT_ID_WAIT_CMD);
            match self
                .exec
                .run(WAIT_SSH_DISPLAY, &argv, SSH_PROBE_TIMEOUT_SECS)
                .await
            {
                Ok(out) => {
                    let now = out.stdout.trim();
                    if out.exit_code == 0 && !now.is_empty() && now != before {
                        return Ok(());
                    }
                }
                // THE loop that produced the lying diagnosis of 2026-08-14:
                // the bridge died during the driver reboot and this loop ran
                // to exhaustion, then reported that the VM had not come back.
                // It had not been asked. See `ExecTransportDown`.
                Err(e) if ExecTransportDown::is_fatal(&e) => return Err(e),
                Err(_) => {}
            }
        }
        anyhow::bail!(
            "the VM did not come back from its reboot after {SSH_RETRIES} attempts (up to {} \
             min). Check it by hand with: gcloud compute ssh {} --command '{BOOT_ID_CMD}'",
            reboot_wait_budget_secs().div_ceil(60),
            self.config.vm_name
        )
    }

    /// Read the GPU's confidential-compute mode. A LECTURE — see
    /// `CcStep::CheckGpuCc` for why the step is not named "enable".
    ///
    /// SECURITY: three answers, three different outcomes, and none of them is
    /// a default.
    ///   * `Ok(true)` — the GPU says ON, the pipeline may go on to serve a
    ///     model on it.
    ///   * `Ok(false)` — the GPU says OFF. A refusal, never something to force
    ///     at runtime: CC mode on a GCP confidential GPU is decided when the
    ///     instance is created, and the fix is a new instance, not a command.
    ///   * `Err` — nobody answered. Also a refusal, and deliberately NOT
    ///     folded into the previous one (task 2 report): an `unwrap_or(false)`
    ///     here would report a mode nobody measured, and an `unwrap_or(true)`
    ///     would serve a model on an unprotected GPU.
    async fn check_gpu_cc(&mut self) -> anyhow::Result<()> {
        let out = self
            .ssh_step(
                "read GPU cc-mode",
                GPU_CC_MODE_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        match parse_nvidia_cc_mode(&out.stdout) {
            Ok(true) => Ok(()),
            Ok(false) => anyhow::bail!(
                "gpu exposed: `{GPU_CC_MODE_CMD}` reports confidential-compute mode OFF. On GCP \
                 that mode is set when the instance is created (a confidential A3 with an H100) \
                 and x0ne never forces it at runtime — recreate the VM rather than serve a model \
                 on a GPU whose memory is readable from the host"
            ),
            Err(e) => anyhow::bail!(
                "gpu exposed: cannot read the GPU confidential-compute mode. {}: {} (`{}` \
                 answered: {})",
                e.what,
                e.detail,
                GPU_CC_MODE_CMD,
                snippet(out.stdout.trim(), CC_MODE_ECHO_MAX_CHARS)
            ),
        }
    }

    /// The GPU half of the proof, measured from inside the attested CPU
    /// enclave, and the ONLY constructor of `GpuEvidence`.
    ///
    /// It re-reads the mode instead of trusting `check_gpu_cc`: the evidence
    /// the report prints must come from a command that ran in this step, on
    /// this instance — the prover's rule that a proof is replayed rather than
    /// remembered (spec 2026-08-13), applied to the GPU.
    async fn attest_gpu(&mut self) -> anyhow::Result<()> {
        let cc = self
            .ssh_step(
                "attest GPU cc-mode",
                GPU_CC_MODE_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        // `?` on purpose: an unreadable answer fails the step. There is no
        // reading of this output that produces evidence.
        let cc_mode_on = parse_nvidia_cc_mode(&cc.stdout)
            .map_err(|e| anyhow::anyhow!("gpu exposed: {}: {}", e.what, e.detail))?;

        let ver = self
            .ssh_step(
                "read GPU driver version",
                GPU_DRIVER_VERSION_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        // Remote text on its way to a terminal report: first line only (a
        // multi-GPU machine prints one per GPU, and only the first is this
        // report's subject), control characters folded, length capped. Same
        // treatment as every other remote string in `render_report`, applied
        // at the door rather than at each use.
        let driver_version = snippet(
            ver.stdout.lines().next().unwrap_or("").trim(),
            DRIVER_VERSION_MAX_CHARS,
        );
        if driver_version.trim().is_empty() {
            anyhow::bail!(
                "gpu exposed: `{GPU_DRIVER_VERSION_CMD}` printed no driver version, so there is \
                 nothing to attest about the GPU"
            );
        }

        // Stored before the refusal below, and it buys two things. First, the
        // report's rule reads "print the combined verdict for evidence that
        // says ON" instead of "for evidence that exists" — a check, not a
        // coincidence about which values ever get built
        // (`a_proven_cpu_with_an_exposed_gpu…` pins that distinction). Second,
        // since task 4's review round 1, `CcOutcome::Failed` carries the
        // evidence, so the refusal report prints the mode that was actually
        // read instead of "no GPU measurement was obtained". Minor M-b of task
        // 3 said this store bought only the first; that was true of the code as
        // it then stood, and the fix was to make the second true rather than to
        // keep documenting its absence.
        self.gpu = Some(GpuEvidence {
            cc_mode_on,
            driver_version,
        });
        if !cc_mode_on {
            anyhow::bail!(
                "gpu exposed: the GPU reports confidential-compute mode OFF (`{GPU_CC_MODE_CMD}`)"
            );
        }
        Ok(())
    }

    /// Declare the attested GPU READY, so CUDA may initialise on it.
    ///
    /// Run 4 of the H100 milestone, 2026-08-14, measured end to end on the real
    /// VM: `x0ne-vllm` restarted 46 times on `RuntimeError: cudaGetDeviceCount
    /// ... Error 802: system not yet initialized`, `conf-compute -grs` said
    /// `not-ready`, and `-srs 1` ended it. In NVIDIA's confidential mode the
    /// GPU refuses CUDA until the guest arms it.
    ///
    /// SECURITY, and it is the whole reason this is a separate step placed
    /// where it is: the disarmed state exists so that a guest can refuse to
    /// hand work to a GPU it has not attested. `full_h100_steps` therefore puts
    /// `AttestGpu` first and this second — attest, THEN arm — and
    /// `the_full_plan_stacks…` pins that order. Arming before the proof would
    /// still boot vLLM; it would simply throw away the one guarantee the order
    /// buys, silently.
    ///
    /// The acquis is a LIVE `-grs`, never an observation carried in from
    /// `CcObservations`: the ready state is per BOOT, so an earlier run — or an
    /// earlier boot of this run, since `InstallGpuDriver` reboots — says
    /// nothing about it. And the step re-reads `-grs` after `-srs 1` rather
    /// than trusting exit 0, the same rule `install_vllm` and the driver step
    /// follow: a command that succeeded and a machine that is in the state we
    /// wanted are two different facts.
    async fn arm_gpu(&mut self) -> anyhow::Result<()> {
        if let Some(out) = self
            .ssh_probe("check the GPU ready state", GPU_READY_STATE_CMD)
            .await
        {
            if out.exit_code == 0 && matches!(parse_gpu_ready_state(&out.stdout), Ok(true)) {
                return Ok(());
            }
        }
        self.ssh_step(
            "arm the confidential GPU (conf-compute -srs 1) — after its attestation, never before",
            GPU_ARM_CMD,
            IN_VM_PROBE_TIMEOUT_SECS,
        )
        .await?;
        let back = self
            .ssh_step(
                "verify the GPU ready state",
                GPU_READY_STATE_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        // `?`, never a default: an unreadable answer is not an armed GPU. The
        // asymmetry is measured — a false "armed" hands the next 30 minutes and
        // ~15 GB of download to a GPU that will answer CUDA Error 802 to every
        // one of them.
        let ready = parse_gpu_ready_state(&back.stdout).map_err(|e| {
            anyhow::anyhow!(
                "cannot read the GPU ready state after `{GPU_ARM_CMD}`. {}: {} (`{}` answered: {})",
                e.what,
                e.detail,
                GPU_READY_STATE_CMD,
                snippet(back.stdout.trim(), CC_MODE_ECHO_MAX_CHARS)
            )
        })?;
        if !ready {
            anyhow::bail!(
                "`{GPU_ARM_CMD}` exited 0 but `{GPU_READY_STATE_CMD}` still reports the GPU as \
                 not-ready. CUDA answers `Error 802: system not yet initialized` in that state, so \
                 serving a model on it would crash-loop rather than fail (measured 2026-08-14, 46 \
                 restarts). Read the driver's own view with: gcloud compute ssh {} --command \
                 'sudo nvidia-smi conf-compute -f'",
                self.config.vm_name
            );
        }
        Ok(())
    }

    /// Install vLLM at the pinned version, idempotently.
    ///
    /// The acquis probe asks about the VERSION, not about the binary: a disk
    /// that came back from a preemption may carry an earlier pin, and "vllm
    /// answers" would then skip the install and serve a version this pipeline
    /// never chose. See `VLLM_VERSION_CMD`.
    async fn install_vllm(&mut self) -> anyhow::Result<()> {
        let acquis = vllm_version_cmd();
        if let Some(out) = self.ssh_probe("check vLLM", &acquis).await {
            if out.exit_code == 0 {
                return Ok(());
            }
        }
        self.ssh_step(
            &format!("install vLLM {VLLM_VERSION} with uv, on python {VLLM_PYTHON}"),
            &vllm_install_script(),
            VLLM_INSTALL_TIMEOUT_SECS,
        )
        .await?;
        // An installer that exits 0 and a binary that answers are two different
        // facts — the driver step's lesson (`nvidia-smi` after the reboot),
        // applied to the one thing that will hold the weights.
        self.ssh_step("verify vLLM answers", &acquis, IN_VM_PROBE_TIMEOUT_SECS)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "the vLLM install exited 0 but the tool does not answer {VLLM_VERSION} on \
                     python {VLLM_PYTHON}: {e}"
                )
            })?;
        Ok(())
    }

    /// Prepare the tmpfs the weights live in, and refuse a machine that cannot
    /// hold them.
    ///
    /// It downloads NOTHING, and the name is the one the spec gave the step, so
    /// this is where that has to be written down: the weights arrive when vLLM
    /// first starts, because `vllm serve` fetches what `HF_HOME` does not
    /// already have. The wall clock and the money for those ~15 GB are
    /// therefore accounted in `ServeVllm` (`vllm_start_budget_secs()`, ≈ 1.86 $
    /// worst case), not here — this step is a `mkdir` and a `df`, and its
    /// budget is one in-VM probe.
    ///
    /// Idempotent by construction (`mkdir -p`, `chown`, a drop-in whose content
    /// never varies), so it carries no acquis probe: asking would cost an ssh
    /// round trip to save a `mkdir`. It does re-read what it changed, which is
    /// a different question — see `fetch_model_script`.
    async fn fetch_model(&mut self) -> anyhow::Result<()> {
        let out = self
            .ssh_step(
                &format!(
                    "prepare {MODEL_DIR} — tmpfs, i.e. TDX-encrypted RAM — and stop \
                     logind from deleting the weights at the next ssh disconnect"
                ),
                &fetch_model_script(),
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        // `?`, never a default: a free-space figure nobody measured is how a
        // 15 GB download dies at 90 % and reports itself as a network error.
        let probe = parse_tmpfs_probe(&out.stdout).map_err(|e| {
            anyhow::anyhow!(
                "cannot read the tmpfs probe of {MODEL_DIR}. {}: {} (answered: {})",
                e.what,
                e.detail,
                snippet(out.stdout.trim(), CC_MODE_ECHO_MAX_CHARS)
            )
        })?;
        // The drop-in is written, logind is restarted — and this is the answer
        // of the daemon that is actually running. A run that continued here
        // would download 15 GB into a directory the next ssh disconnect erases,
        // and report it as a slow network (run 4's misdiagnosis, run 5's
        // measurement).
        if probe.remove_ipc {
            anyhow::bail!(
                "logind still answers RemoveIPC=yes after writing {LOGIND_DROPIN} and restarting \
                 it. In that state it deletes every POSIX IPC object this user owns when the \
                 user's last session closes — and a file under /dev/shm is one of them, so each \
                 ssh probe of this pipeline would erase the ~15 GB it is waiting for (measured \
                 2026-08-14: 10 MB written, one session closed, gone). Read what the daemon \
                 thinks with: gcloud compute ssh {} --command '{LOGIND_REMOVEIPC_CMD}'",
                self.config.vm_name
            );
        }
        let avail_kib = probe.avail_kib;
        if avail_kib < MODEL_TMPFS_MIN_KIB {
            anyhow::bail!(
                "/dev/shm has {} GiB free and the weights need {} GiB. They are NOT going to \
                 the disk instead — the whole point of this machine is that they stay in \
                 TDX-encrypted RAM — so this is a refusal: grow the tmpfs with \
                 `sudo mount -o remount,size=64G /dev/shm`, or serve a smaller model.",
                avail_kib / (1024 * 1024),
                MODEL_TMPFS_MIN_KIB / (1024 * 1024)
            );
        }
        Ok(())
    }

    /// Write the systemd unit, start it, and wait until the server has bound
    /// its port.
    ///
    /// The wait belongs to THIS step and not to the validation that follows it,
    /// and that is the ordering finding of task 4. `ValidateEncryption` reads
    /// the socket; on a fresh VM there is no socket for the first ~15 minutes,
    /// because vLLM's HTTP server only comes up once the weights are downloaded
    /// and loaded. A validation asked about a socket that does not exist yet
    /// has exactly two moves: invent a verdict (the `Err`-as-`false` trap the in-VM checks
    /// exist to close) or refuse a machine that is merely still starting. So
    /// the step that STARTS the server is the step that waits for it, and the
    /// step that JUDGES only ever sees a socket that exists.
    ///
    /// The acquis probe is the socket, deliberately not `systemctl is-active`:
    /// a unit is "active" for the whole download, which is precisely the state
    /// where skipping the wait would hand `ValidateEncryption` nothing to read.
    /// A socket bound to the WRONG address also counts as acquired here — this
    /// step's job is that a server runs, and no amount of waiting turns a
    /// `0.0.0.0` bind into a loopback one. The next step refuses it, loudly.
    async fn serve_vllm(&mut self) -> anyhow::Result<()> {
        if let Some(out) = self.ssh_probe("check the vLLM socket", SS_LISTEN_CMD).await {
            // `.is_ok()` reads as "something is listening on the port", and
            // discards WHERE. That is not the `unwrap_or` the task-2 report
            // warns about: no verdict is defaulted here, only work is skipped,
            // and `validate_encryption` re-reads the same command to determine.
            if out.exit_code == 0 && parse_ss_localhost_only(&out.stdout, VLLM_PORT).is_ok() {
                return Ok(());
            }
        }
        self.ssh_step(
            &format!("write and restart {VLLM_UNIT} — {VLLM_MODEL} on 127.0.0.1:{VLLM_PORT}"),
            &serve_vllm_script(),
            VLLM_UNIT_WRITE_TIMEOUT_SECS,
        )
        .await?;
        self.wait_vllm_bound().await
    }

    /// Wait until something LISTENs on `VLLM_PORT` — while watching whether
    /// the server is alive and whether anything is being downloaded.
    ///
    /// The pause between attempts is the exec layer's, keyed on
    /// `WAIT_VLLM_DISPLAY` — and unlike the SSH wait it must fire on a
    /// SUCCESSFUL probe too, because `ss` answers perfectly well while saying
    /// nothing about our port. See that constant. The label is a PREFIX since
    /// run 4, so the download figure can ride along in it without turning the
    /// pause off (`cc_cmd::probe_pause_secs`).
    ///
    /// Run 4 of the H100 milestone, 2026-08-14, is why the loop reads three
    /// things instead of one. It watched the port and nothing else for 30
    /// minutes and 1.26 $ while `x0ne-vllm` restarted 46 times behind it; the
    /// report then blamed a slow download, because the port is silent in both
    /// situations and nothing else was asked. The owner's instruction was
    /// explicit — the wait must be complementary, not a single signal.
    ///
    /// So, per probe, in one ssh session (`vllm_wait_probe_cmd`):
    ///
    ///   * the SOCKET, unchanged, and it is still the only way out through
    ///     success;
    ///   * `NRestarts` of the unit. A RISE between two probes is a crash loop
    ///     and fails the step at once, with the journal attached. The absolute
    ///     value proves nothing — a disk resumed from preemption carries the
    ///     restarts of its previous life — so it takes two probes to conclude,
    ///     and two is what it takes;
    ///   * the size of the model directory. Below `MODEL_DOWNLOAD_FLOOR_KIB`
    ///     for `VLLM_DOWNLOAD_STALL_PROBES` probes in a row, with nothing
    ///     bound, means nothing is arriving; above it, the wait is justified
    ///     and says so in its own label.
    ///
    /// Both new signals only ever make the step fail EARLIER. Neither can turn
    /// an unbound port into success, and the socket verdict that gates serving
    /// is re-read alone by `ValidateEncryption` — so a VM that forges these
    /// figures buys itself the 30-minute wait it already had.
    async fn wait_vllm_bound(&mut self) -> anyhow::Result<()> {
        let probe_cmd = vllm_wait_probe_cmd();
        // `None` until the first readable probe: a rise needs two readings, and
        // "no previous reading" is not zero.
        let mut previous_restarts: Option<u64> = None;
        let mut probes_below_floor = 0usize;
        let mut last_kib = 0u64;
        for attempt in 0..VLLM_START_RETRIES {
            // Built BEFORE the call, so it carries the previous probe's figure
            // — the only one that exists yet. The prefix is what the exec layer
            // keys its pause on; everything after it is for the human paying
            // 3.72 $/h to watch.
            let display = if attempt == 0 || last_kib < MODEL_DOWNLOAD_FLOOR_KIB {
                WAIT_VLLM_DISPLAY.to_string()
            } else {
                format!(
                    "{WAIT_VLLM_DISPLAY} — {:.1}G/{MODEL_WEIGHTS_GIB}G downloaded",
                    last_kib as f64 / (1024.0 * 1024.0)
                )
            };
            let argv = self.ssh_cmd(&probe_cmd);
            match self
                .exec
                .run(&display, &argv, VLLM_PROBE_TIMEOUT_SECS)
                .await
            {
                // A non-zero exit is a probe that did not answer — the ssh
                // session lost, the VM mid-reboot. Retried, exactly as before.
                Ok(out) if out.exit_code != 0 => {}
                Ok(out) => {
                    // Fail-closed: a probe that exited 0 and cannot be read is
                    // an error, never a reassuring default. Both figures below
                    // exist only to STOP this loop early, so defaulting them
                    // restores the blind 30-minute wait — silently, which is
                    // this repo's own recurring defect.
                    let probe = parse_vllm_wait_probe(&out.stdout).map_err(|e| {
                        anyhow::anyhow!(
                            "the vLLM wait probe answered something it could not read. {}: {} \
                             (`{}` answered: {})",
                            e.what,
                            e.detail,
                            snippet(&probe_cmd, REMOTE_CMD_ECHO_MAX_CHARS),
                            snippet(out.stdout.trim(), CC_MODE_ECHO_MAX_CHARS)
                        )
                    })?;

                    // The socket is still the only success.
                    if parse_ss_localhost_only(&probe.listen, VLLM_PORT).is_ok() {
                        return Ok(());
                    }

                    // (a) A crash loop, named in at most two probes instead of
                    // thirty-six minutes.
                    if let Some(before) = previous_restarts {
                        if probe.restarts > before {
                            return Err(self.crash_loop_error(before, probe.restarts).await);
                        }
                    }
                    previous_restarts = Some(probe.restarts);

                    // (b) Nothing is arriving.
                    last_kib = probe.model_dir_kib;
                    if probe.model_dir_kib < MODEL_DOWNLOAD_FLOOR_KIB {
                        probes_below_floor += 1;
                    } else {
                        probes_below_floor = 0;
                    }
                    if probes_below_floor >= VLLM_DOWNLOAD_STALL_PROBES {
                        anyhow::bail!(
                            "nothing is being downloaded: after {VLLM_DOWNLOAD_STALL_PROBES} \
                             probes over {} min, {MODEL_DIR} still holds {} KiB and nothing \
                             listens on 127.0.0.1:{VLLM_PORT}. A first start pulls ~{MODEL_WEIGHTS_GIB} \
                             GB of weights, so an empty tmpfs is not a slow download — it is a \
                             server that is not downloading. Read why with: gcloud compute ssh {} \
                             --command '{}'",
                            (VLLM_DOWNLOAD_STALL_PROBES as u64
                                * (VLLM_PROBE_TIMEOUT_SECS + VLLM_PROBE_PAUSE_SECS))
                                .div_ceil(60),
                            probe.model_dir_kib,
                            self.config.vm_name,
                            vllm_journal_cmd()
                        );
                    }
                }
                // Same reading as the two SSH waits: a dead bridge is not a
                // server that has not finished starting. This loop is the
                // longest of the three (a 15 GB download), so burning it on a
                // corpse is also the most expensive.
                Err(e) if ExecTransportDown::is_fatal(&e) => return Err(e),
                Err(_) => {}
            }
        }
        anyhow::bail!(
            "vLLM never bound 127.0.0.1:{VLLM_PORT} after {VLLM_START_RETRIES} probes (up to {} \
             min — a first start downloads ~15 GB of weights into /dev/shm; {} KiB had landed in \
             {MODEL_DIR}). Read what the server is doing with: gcloud compute ssh {} --command \
             '{}'",
            vllm_start_budget_secs().div_ceil(60),
            last_kib,
            self.config.vm_name,
            vllm_journal_cmd()
        )
    }

    /// The failure a rising `NRestarts` produces, with the unit's own last
    /// words attached.
    ///
    /// Separate from the loop because it makes an extra ssh call and the loop
    /// is already the densest thing in this file. `ssh_probe`, not `ssh_step`:
    /// the diagnosis is a bonus, and a journal we cannot read must not replace
    /// the failure we CAN name. Bounded by `snippet`, like every other piece of
    /// remote text this program prints.
    async fn crash_loop_error(&mut self, before: u64, now: u64) -> anyhow::Error {
        let journal = self
            .ssh_probe("read the vLLM journal", &vllm_journal_cmd())
            .await
            .map(|out| {
                let both = format!("{}\n{}", out.stdout.trim(), out.stderr.trim());
                snippet(both.trim(), JOURNAL_ECHO_MAX_CHARS)
            })
            .unwrap_or_else(|| "(the journal could not be read)".into());
        anyhow::anyhow!(
            "{VLLM_UNIT} is crash-looping: systemd counted {before} restarts, then {now}, while \
             nothing listened on 127.0.0.1:{VLLM_PORT}. Waiting longer only bills the wait — at \
             run 4 (2026-08-14) that was 46 restarts, 30 min and 1.26 $ reported as a slow \
             download. Its last words:\n{journal}"
        )
    }

    /// executed: four questions, four report lines, and a refusal on
    /// anything that is not a clean answer.
    ///
    /// SECURITY, and this is the semantics of the step: a failed check is a
    /// REFUSAL, not a breakdown. `Ok(false)` from a parser and `Err` from a
    /// parser both fail the step — one says "the machine answered, and the
    /// answer is a hole", the other says "nobody answered", and neither is
    /// permission to serve a model. No `unwrap_or` appears below for that
    /// reason: an `unwrap_or(true)` on the socket check would serve an
    /// unauthenticated 7B on a public cloud IP, which is the single worst thing
    /// this program could do.
    ///
    /// Two of the four are read from the VM (swap, socket) and two from the
    /// HOST (firewall, disk). The split is not incidental: a firewall rule
    /// asked about over ssh is a statement the VM makes about the network it
    /// sits behind, and the VM is the subject of the sentence. gcloud is the
    /// only side that can answer it without asking the accused.
    ///
    /// It never runs as an acquis — `full_h100_steps` emits it unconditionally
    /// — because what it produces is evidence for the report, and evidence
    /// carried over from an earlier run describes a machine that may since have
    /// been preempted and rebuilt. On a SPOT instance that is the expected
    /// case.
    ///
    /// The four checks SHORT-CIRCUIT on `Err` and only on `Err`: an unreadable
    /// answer stops the step where it happens, so a socket nobody could read
    /// means the firewall and the disk are never asked. That is deliberate and
    /// it is the honest half of "any `false` is a refusal" — a `false` is a
    /// measurement, and all four are collected before the refusal is composed,
    /// so the report can say which one failed; an `Err` is the absence of a
    /// measurement, and continuing past it would build an `EncryptionChecks`
    /// with a hole in it. The cost is that one unreadable answer hides whatever
    /// the two host-side checks would have said, which is the right trade: the
    /// operator's next move is to make that command readable, not to read a
    /// partial verdict.
    async fn validate_encryption(&mut self) -> anyhow::Result<()> {
        // 1. Swap, in the VM. `bool` and not `Result` by design
        // (`parse_swapon_empty`): every unrecognised output is non-empty text,
        // hence "there is swap", hence a refusal. `true` takes literally empty
        // output.
        let swap = self
            .ssh_step(
                "check that swap is off",
                SWAPON_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        let swap_off = parse_swapon_empty(&swap.stdout);

        // 2. The socket, in the VM.
        let ss = self
            .ssh_step(
                "check where vLLM is bound",
                SS_LISTEN_CMD,
                IN_VM_PROBE_TIMEOUT_SECS,
            )
            .await?;
        let localhost_only = parse_ss_localhost_only(&ss.stdout, VLLM_PORT).map_err(|e| {
            anyhow::anyhow!(
                "REFUSED: cannot tell where port {VLLM_PORT} is bound, so it does not get to \
                 count as loopback-only. {}: {}",
                e.what,
                e.detail
            )
        })?;

        // 3. The firewall, from the HOST.
        let fw = self
            .gcloud(
                "read the project firewall rules (host-side)",
                &[
                    "compute",
                    "firewall-rules",
                    "list",
                    "--format=json(name,allowed,direction,disabled)",
                ],
            )
            .await?;
        let firewall_closed = !parse_firewall_exposes(&fw.stdout, VLLM_PORT).map_err(|e| {
            anyhow::anyhow!(
                "REFUSED: cannot read the project firewall rules, so they do not get to count \
                 as closed. {}: {}",
                e.what,
                e.detail
            )
        })?;

        // 4. The boot disk, from the HOST. Its name is the instance name: the
        // create call sets no `--boot-disk-device-name` (`VmSpec::to_gcloud_
        // argv`), and GCE then names the disk after the instance. If that ever
        // changes, `parse_disk_encryption` fails on the name it reads back
        // rather than reporting about somebody else's disk.
        let vm_name = self.config.vm_name.clone();
        let zone_arg = format!("--zone={}", self.config.machine.vm_spec(&vm_name).zone);
        let disk_out = self
            .gcloud(
                "read the boot disk encryption (host-side)",
                &[
                    "compute",
                    "disks",
                    "describe",
                    &vm_name,
                    &zone_arg,
                    "--format=json(name,diskEncryptionKey)",
                ],
            )
            .await?;
        let disk = parse_disk_encryption(&disk_out.stdout, &vm_name).map_err(|e| {
            anyhow::anyhow!(
                "REFUSED: cannot read the boot disk. {}: {}",
                e.what,
                e.detail
            )
        })?;

        // Stored BEFORE the refusal, exactly as `attest_gpu` stores what it
        // measured: the report's rule is then "print these lines from what was
        // read", not "print them because a value exists".
        let checks = EncryptionChecks {
            swap_off,
            localhost_only,
            firewall_closed,
            disk,
        };
        self.encryption = Some(checks.clone());
        if checks.all_pass() {
            return Ok(());
        }

        let mut refusals: Vec<String> = Vec::new();
        if !swap_off {
            refusals.push(format!(
                "swap is ACTIVE (`{SWAPON_CMD}` printed a table), so the kernel may write pages \
                 of TDX-encrypted RAM — the weights among them — to the boot disk in the clear. \
                 Turn it off with `sudo swapoff -a`"
            ));
        }
        if !localhost_only {
            refusals.push(format!(
                "port {VLLM_PORT} is NOT bound to loopback only (`{SS_LISTEN_CMD}`), so an \
                 unauthenticated model server is reachable from this VM's network interface"
            ));
        }
        if !firewall_closed {
            refusals.push(format!(
                "a project firewall rule opens INGRESS on port {VLLM_PORT}. Find it with \
                 `gcloud compute firewall-rules list --format='table(name,allowed,direction)'` \
                 and delete or disable it"
            ));
        }
        anyhow::bail!("REFUSED: {}", refusals.join(" — and — "));
    }

    /// Ask vLLM what it serves, until it says the model this pipeline started.
    ///
    /// The check is the model NAME, not merely "something answered". By the
    /// time this runs the port is bound and `ValidateEncryption` has said it is
    /// bound to loopback — but a socket says nothing about what is behind it,
    /// and the plan the human approved names one model. Serving a different one
    /// is a state to report, not to pass.
    ///
    /// `Err` from the parser does not become `false` here either: it becomes
    /// "keep waiting", and the loop's own give-up is the refusal — with the
    /// last unreadable answer quoted, bounded, in the message. A parse error on
    /// probe 3 of 6 is a server mid-restart; treating it as a verdict would
    /// refuse a machine that is one probe from healthy.
    ///
    /// KNOWN LIMIT, and it is the one that cost this pipeline a whole session:
    /// a model NAME says nothing about what the model COMPUTES. On 2026-08-14
    /// vLLM 0.27.1 passed every check in this file — port bound, loopback,
    /// `/v1/models` naming the right model, unit active, zero restarts — while
    /// answering `!!!!!` to every prompt. Nothing here can see that, by
    /// construction. A candidate fix (a greedy canary
    /// completion compared against a constant) is not free.
    async fn health_check(&mut self) -> anyhow::Result<()> {
        let cmd = vllm_models_cmd();
        let mut last: Option<String> = None;
        for _ in 0..VLLM_HEALTH_RETRIES {
            let argv = self.ssh_cmd(&cmd);
            let out = match self
                .exec
                .run(WAIT_VLLM_DISPLAY, &argv, VLLM_PROBE_TIMEOUT_SECS)
                .await
            {
                Ok(out) => out,
                // THE FOURTH WAIT LOOP. It hid behind a `let … else` while the
                // other three were converted (review round 3, C1), and the
                // omission was not merely six wasted probes: the `bail!` below
                // builds a NEW error, so the `ExecTransportDown` never reached
                // `note_transport`, `transport_down` stayed false, `run_plan`
                // sent its stop into the corpse and the rescue never fired —
                // the whole of the 2026-08-14 failure, restored, on the step
                // that follows a 15 GB download.
                Err(e) if ExecTransportDown::is_fatal(&e) => return Err(e),
                Err(_) => {
                    last = Some("the command layer did not answer".into());
                    continue;
                }
            };
            if out.exit_code != 0 {
                last = Some(format!("curl exited {}", out.exit_code));
                continue;
            }
            match parse_vllm_models(&out.stdout) {
                Ok(ids) if ids.iter().any(|id| id == VLLM_MODEL) => return Ok(()),
                Ok(ids) if ids.is_empty() => {
                    last = Some("the server answered, serving nothing".into())
                }
                Ok(ids) => last = Some(format!("the server serves {}", ids.join(", "))),
                Err(e) => last = Some(format!("{}: {}", e.what, e.detail)),
            }
        }
        anyhow::bail!(
            "vLLM never reported {VLLM_MODEL} on 127.0.0.1:{VLLM_PORT} after \
             {VLLM_HEALTH_RETRIES} probes (up to {} min). Last answer: {}. Read the server's own \
             log with: gcloud compute ssh {} --command 'journalctl -u {VLLM_UNIT} -n 50'",
            vllm_health_budget_secs().div_ceil(60),
            snippet(
                last.as_deref().unwrap_or("nothing at all"),
                VLLM_ECHO_MAX_CHARS
            ),
            self.config.vm_name
        )
    }

    /// Put THIS x0ne in the enclave: copy the two binaries, write a confined
    /// config, and prove the binary that answers is the one that was sent.
    ///
    /// It is the last step of the pipeline and the point of the whole machine:
    /// from here an operator can talk to a 7B model whose weights sit in
    /// TDX-encrypted RAM, over a loopback socket this run has proved is
    /// loopback, with a runtime whose policy this run wrote. Nothing in the
    /// conversation leaves the VM.
    ///
    /// The acquis probe and the final verification are THE SAME command
    /// (`x0ne_ready_cmd`), and that is deliberate: the step ends in exactly the
    /// state its own detection recognises, so a run that skipped the work and a
    /// run that did it are indistinguishable to the next `x0ne cc` — which is
    /// what "resumable" has to mean. It is also the property that makes the
    /// verification worth its 60 s: an scp that exits 0 and a binary that
    /// answers are two different facts, the lesson `install_vllm` learned from
    /// `install_gpu_driver`.
    async fn install_x0ne(&mut self) -> anyhow::Result<()> {
        let ready = x0ne_ready_cmd();
        if let Some(out) = self.ssh_probe("check x0ne", &ready).await {
            if out.exit_code == 0 {
                return Ok(());
            }
        }

        // No paths, no upload. Refusing beats inventing a `target/release`:
        // this is the binary the operator is about to trust inside an attested
        // enclave, and guessing which one it is would be the one place in this
        // pipeline where a path nobody read decides what runs.
        let Some(bins) = self.binaries.clone() else {
            anyhow::bail!(
                "no local x0ne binaries were supplied to the runner, so there is nothing to \
                 install. Build them with `cargo build --release` and re-run `x0ne cc`"
            );
        };
        // `to_str`, not `to_string_lossy`: a path that is not UTF-8 would be
        // silently REWRITTEN by the lossy form, and scp would then upload some
        // other file — or nothing — under a name that looks right in the log.
        let (Some(x0ne), Some(runtime)) = (bins.x0ne.to_str(), bins.runtime.to_str()) else {
            anyhow::bail!(
                "the x0ne binaries are at paths this pipeline cannot spell on a command line \
                 (not valid UTF-8): {} and {}",
                bins.x0ne.display(),
                bins.runtime.display()
            );
        };

        let spec = self.config.machine.vm_spec(&self.config.vm_name);
        let argv: Vec<String> = vec![
            "compute".into(),
            "scp".into(),
            x0ne.into(),
            runtime.into(),
            // `~/` and not an absolute path: the home directory of the ssh
            // account is the VM's business (see `ENCLAVE_HOME_TOKEN`), and scp
            // expands the tilde on the side that knows it.
            format!("{}:~/", self.config.vm_name),
            format!("--zone={}", spec.zone),
        ];
        let out = self
            .exec
            .run(
                &format!("copy x0ne and x0ne-runtime {X0NE_VERSION} into the VM"),
                &argv,
                scp_budget_secs(),
            )
            .await?;
        if out.exit_code != 0 {
            anyhow::bail!(
                "copying the x0ne binaries failed (exit {}): {} {}",
                out.exit_code,
                out.stdout.trim(),
                out.stderr.trim()
            );
        }

        self.ssh_step(
            "write the enclave config: openai-compat on loopback, runtime policy deny-by-default",
            &write_enclave_configs_script(),
            IN_VM_PROBE_TIMEOUT_SECS,
        )
        .await?;

        // `x0ne_verify_cmd`, NOT `ready`: the acquis probe above is allowed to
        // fail quietly (it runs before anything was installed), the
        // verification is not. Its stderr carries the loader's refusal, which
        // is the one sentence that names a glibc mismatch — run 8, 2026-08-15.
        //
        // Bounded, because a VM is a producer that does not share our idea of
        // how long an error is (`snippet`, and its head-vs-tail
        // arbitration this bound inherits). 1200 characters is ~3× what a
        // loader refusal plus the echoed command occupy.
        self.ssh_step(
            &format!("verify x0ne {X0NE_VERSION} answers in the VM"),
            &x0ne_verify_cmd(),
            IN_VM_PROBE_TIMEOUT_SECS,
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "the binaries were copied but the VM does not report a working x0ne \
                 {X0NE_VERSION} with its two config files: {}",
                snippet(&e.to_string(), X0NE_VERIFY_ECHO_MAX_CHARS)
            )
        })?;
        Ok(())
    }

    /// SECURITY (money): the argv is `VmSpec::to_stop_argv`'s and is not
    /// rebuilt here. It carries `--discard-local-ssd=true`, without which GCE
    /// refuses to stop any machine that has a Local SSD — measured on the real
    /// a3-highgpu-1g, 2026-08-14, `HTTPError 400`. See that function for the
    /// full derivation; the point of calling it rather than listing the flags
    /// again is that the budget gate and the operator lever cannot drift into
    /// issuing two different commands.
    pub async fn stop_vm(&mut self) -> anyhow::Result<()> {
        let spec = self.config.machine.vm_spec(&self.config.vm_name);
        let argv = spec.to_stop_argv();
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = self.gcloud("stop VM (billing stops)", &argv).await;
        // A stop that dies of the transport marks the runner too: the rescue
        // stop runs on a FRESH runner, so this can only ever be the first
        // attempt failing — and a caller that retried on this same runner
        // would be retrying into the corpse.
        self.note_transport(&out);
        out.map(|_| ())
    }

    /// SECURITY: the one irreversible step of `x0ne cc` — it takes the boot
    /// disk with it, so nothing that ran on the VM survives. It is never part
    /// of a plan and never covered by the plan's approval (repo invariant 7:
    /// trust waives what undoes itself, never what removes the safety net);
    /// the only caller must ask its own dedicated confirmation first.
    /// `--quiet` suppresses gcloud's own prompt, which cannot be answered
    /// through the bridge — the human gate already happened, above.
    pub async fn delete_vm(&mut self) -> anyhow::Result<()> {
        let spec = self.config.machine.vm_spec(&self.config.vm_name);
        let vm_name = self.config.vm_name.clone();
        let zone_arg = format!("--zone={}", spec.zone);
        self.gcloud(
            "DELETE VM and its disk (irreversible)",
            &[
                "compute",
                "instances",
                "delete",
                &vm_name,
                &zone_arg,
                "--quiet",
            ],
        )
        .await?;
        Ok(())
    }

    /// The CPU proof, if this run earned one. THE single construction site of
    /// a `ProofBundle` in the runner, called from both the `Proven` and the
    /// `Failed` arms of `outcome` — a second, "equivalent" derivation is how
    /// the two would come to disagree about what was proven.
    ///
    /// SECURITY: no shortcut. `ProofBundle::from_verdict` is the only door and
    /// it returns `None` for anything but a `Verdict::Verified` carrying
    /// claims, so a failed or rejected run cannot acquire measurements it never
    /// made.
    fn cpu_proof(&self) -> Option<ProofBundle> {
        let v = self.verdict.as_ref()?;
        ProofBundle::from_verdict(v, self.claims.as_ref(), self.nonce_hex.clone())
    }

    pub fn outcome(&self, failed: Option<(CcStep, String)>) -> CcOutcome {
        // A rejected verdict outranks the step failure it caused (review round
        // 1, C1). Since `attest_tdx` now fails on `Rejected`, the run arrives
        // here with BOTH a rejection and a `failed`; "PIPELINE FAILED at
        // AttestTdx / error attestation refused: nonce mismatch" names the
        // shadow, "ATTESTATION REFUSED / reason nonce mismatch" names the
        // thing. Only a verdict can reach this arm, so nothing else is
        // reclassified: a driver install that fails after a VERIFIED quote
        // still reports as the failure it is.
        if let Some(Verdict::Rejected { reason, .. }) = &self.verdict {
            return CcOutcome::Refused {
                reason: reason.clone(),
            };
        }
        if let Some((step, error)) = failed {
            return CcOutcome::Failed {
                step,
                error,
                // SECURITY (review round 1 of task 5, I1+I2): the CPU proof
                // survives the failure of a LATER step, and it is built here by
                // the same call `Proven` uses — not summarised, not re-derived.
                // Measured on the very first complete `--machine full` run
                // (~2.79 $): it stops at `InstallX0ne`, so the report is a
                // `Failed`, and it printed the GPU verdict and the four in-VM
                // checks while printing NOT ONE line of the TDX measurement
                // that had genuinely been made minutes earlier. The one report
                // the full machine produces today said "the CPU enclave is
                // proven" in an error string with nothing to back it. This is
                // the finding I1 of task 4 (`Failed` must carry what was
                // measured) one level up: the attestation is a measurement too.
                proof: self.cpu_proof(),
                gpu: self.gpu.clone(),
                encryption: self.encryption.clone(),
            };
        }
        match &self.verdict {
            // Through `cpu_proof()`, not a second inline `from_verdict`: the
            // doc-comment above it claims a single construction site, and the
            // re-review of round 1 measured that claim false while this arm
            // derived its own — gutting `cpu_proof()` left every test green.
            Some(v) => match self.cpu_proof() {
                // SECURITY: on the full machine, a proven CPU with NO GPU
                // measurement is not a success, and this is where that is
                // decided — upstream of the report, upstream of the exit
                // status, in the one function that mints `Proven`.
                //
                // What it closes (minor M-f of task 3, re-judged at task 5 now
                // that `--machine full` is reachable again): `gpu_line` renders
                // exactly this state as "gpu exposed — no GPU measurement was
                // obtained on this run", while `exit_status_for` sees only the
                // variant and returns 0. `x0ne cc --machine full --yes && echo
                // ok` would print "ok" over a report saying the GPU is in the
                // clear — a script cannot read a report, which is the finding
                // `exit_status_for` was written for (review round 1, I2), one
                // layer deeper.
                //
                // It is not reachable today: `full_h100_steps` pushes
                // `AttestGpu` unconditionally, `replan_after_ssh` may only DROP
                // steps that are subsequences of an approved plan, and any
                // failure carries a `failed` that returns above. That is
                // precisely why the guard belongs here rather than in a test:
                // the day someone makes the GPU attestation skippable — the
                // exact shape of this repo's recurring defect — the pipeline
                // must fail closed instead of exiting 0 with an honest report
                // nobody parses. `Failed` rather than `Refused` because the
                // encryption measurements have to survive into the report; a
                // refusal carries none.
                Some(p) if self.config.machine == CcMachine::FullH100 && self.gpu.is_none() => {
                    CcOutcome::Failed {
                        step: CcStep::AttestGpu,
                        error: "the CPU enclave is proven but the GPU was never measured on this \
                                run: on --machine full the GPU half is part of what is being \
                                proven, so this run proves nothing about the accelerator the \
                                model would have run on"
                            .into(),
                        // The CPU half WAS proven, and the error says so — so
                        // the report has to show the measurements behind it,
                        // exactly as it would under a `Proven` header.
                        proof: Some(p),
                        gpu: None,
                        encryption: self.encryption.clone(),
                    }
                }
                Some(p) => CcOutcome::Proven {
                    proof: p,
                    gpu: self.gpu.clone(),
                    encryption: self.encryption.clone(),
                },
                None => CcOutcome::Refused {
                    reason: match v {
                        Verdict::Rejected { reason, .. } => reason.clone(),
                        Verdict::Verified { .. } => "verified but no claims (internal)".into(),
                    },
                },
            },
            None => CcOutcome::Refused {
                reason: "attestation never ran".into(),
            },
        }
    }
}

/// The shared terminal sanitizer, re-exported under the name this file has
/// used since review finding I1 (round 2, 2026-08-13).
///
/// SECURITY: the implementation moved to `crate::text_safety` on 2026-08-15.
/// It was written here, for the attestation report alone, while every
/// approval prompt in the product interpolated model-authored strings raw —
/// audit finding 4. There is now one sanitizer and it is imported, not
/// copied: a second copy is a second place to forget.
use crate::text_safety::{sanitize, snippet};

/// Is `small` a subsequence of `big` — the same steps, in the same order,
/// possibly with some missing?
///
/// SECURITY: this is the property that keeps `replan_after_ssh` from turning
/// into a second, unapproved planner. A human read a printed list of steps and
/// said yes to it (repo invariant 5). Re-planning may DROP work already done;
/// anything else — a step the plan never had, two steps swapped — means the
/// re-plan disagrees with what was approved, and the approved list wins.
fn is_subsequence(small: &[CcStep], big: &[CcStep]) -> bool {
    let mut it = big.iter();
    small.iter().all(|s| it.any(|b| b == s))
}

/// The GPU line of the report, or `None` for a machine that has no GPU.
///
/// SECURITY: on the full machine this line is ALWAYS printed, and its default
/// is "exposed". Absence of evidence is rendered as an exposed GPU rather than
/// as no line at all, because a missing line reads as "nothing to report" —
/// and the state it describes (a run that stopped before, or during, the GPU
/// attestation) is precisely the one where a model must not be served.
///
/// The words `cc-mode` appear only next to a measurement that said ON. The
/// exposed forms name the driver when one was read, so an operator can tell
/// "the GPU answered OFF" from "nothing answered", but they never carry the
/// vocabulary of the proof.
fn gpu_line(machine: CcMachine, gpu: Option<&GpuEvidence>) -> Option<String> {
    match machine {
        CcMachine::Prover => None,
        CcMachine::FullH100 => Some(match gpu {
            Some(g) if g.cc_mode_on() => {
                format!("  gpu        cc-mode ON · driver {}", g.driver_version())
            }
            Some(g) => format!(
                "  gpu        exposed — the GPU answered OFF (driver {})",
                g.driver_version()
            ),
            None => "  gpu        exposed — no GPU measurement was obtained on this run".into(),
        }),
    }
}

/// The in-VM check lines of the report — one per check, which is the form the
/// spec asks for: "every check = one line of the report".
///
/// SECURITY: on the full machine these lines are ALWAYS present, and their
/// default is "unverified", for the same reason `gpu_line` always prints and
/// defaults to "exposed". A missing line reads as "nothing to report", and the
/// state it would be hiding — a run that stopped before or during the checks —
/// is exactly the one where nothing may be trusted to be private.
///
/// The disk line is the honest one and it took the most care to write. GCP
/// encrypts every persistent disk at rest and there is no way to turn that off,
/// so the line cannot be a check that can fail; what it can do is say what the
/// property is worth. It protects the media — a drive pulled from a rack — and
/// not the VM against the platform that runs it, which is precisely why the
/// weights live in `/dev/shm` and why the report says so on the
/// same line rather than letting the word "encrypted" do work it cannot do.
fn encryption_lines(machine: CcMachine, checks: Option<&EncryptionChecks>) -> Vec<String> {
    match machine {
        CcMachine::Prover => vec![],
        CcMachine::FullH100 => match checks {
            None => vec![
                "  encryption unverified — swap, binding, firewall and boot disk were not"
                    .to_string(),
                "             proven on this run".to_string(),
            ],
            Some(c) => {
                let mut v = vec![
                    format!(
                        "  swap       {}",
                        if c.swap_off() {
                            "off — no page of the model can reach a disk in the clear"
                        } else {
                            "ACTIVE — pages of RAM can be written to the disk in the clear"
                        }
                    ),
                    format!(
                        "  bind       {}",
                        if c.localhost_only() {
                            format!("127.0.0.1:{VLLM_PORT} only — nothing off-machine can reach it")
                        } else {
                            format!("port {VLLM_PORT} is NOT loopback-only — reachable from the network")
                        }
                    ),
                    format!(
                        "  firewall   {}",
                        if c.firewall_closed() {
                            format!("no INGRESS rule opens {VLLM_PORT} (read from the host)")
                        } else {
                            format!("an INGRESS rule OPENS {VLLM_PORT}")
                        }
                    ),
                    format!(
                        "  disk       {}",
                        match c.disk() {
                            DiskEncryption::GoogleManaged =>
                                "encrypted at rest by GCP, Google-managed key",
                            DiskEncryption::CustomerKey =>
                                "encrypted at rest with a customer key (CMEK/CSEK)",
                        }
                    ),
                ];
                v.push(
                    "             — protects the media against theft, NOT the VM against".into(),
                );
                v.push("             its host: that is why the weights live in /dev/shm".into());
                v
            }
        },
    }
}

/// Renders the terminal report for `x0ne cc`. Never writes PROVEN outside
/// the `Proven` arm — `ProofBundle` fields are private, so this reads them
/// only through the accessors (`mr_td_hex()` &c.), same as any other caller;
/// there is no shortcut that would let a Refused/Failed report borrow the
/// word.
///
/// SECURITY (review finding I1, 2026-08-13): `reason` (Refused), `error`
/// (Failed), and `vm_state` can all carry text that started life as remote
/// output — `gcloud` stdout/stderr, an `InstanceState` derived from gcloud's
/// own JSON, or worse, whatever the attested VM printed before `attest_tdx`
/// gave up on it. That text is NOT trusted formatting. All three go through
/// `sanitize()` before interpolation, which guarantees exactly two things —
/// no more, no less: (1) none of these three inputs can ever START a new
/// line (`\n`/`\r` are folded to spaces), so a `grep '^ENCLAVE PROVEN'`
/// against the rendered report will never match a forged line; and (2) no
/// terminal escape sequence (`\x1b[...`) survives, so remote text cannot
/// move the cursor, clear a line, or hide output. It does NOT guarantee the
/// literal substring "ENCLAVE PROVEN" is absent from the report — that text
/// can still appear mid-line inside a `reason`/`error`/`vm_state` value; a
/// plain `grep PROVEN` (no `^` anchor) is not the property this defends.
/// One limitation of this milestone, written once and rendered twice.
///
/// # Why the text is a slice of lines rather than a sentence
///
/// The text report wraps these under a hanging indent, and the JSON report
/// wants one sentence. Storing the WRAPPED lines and joining them for JSON
/// means the two renderings cannot drift: there is nothing to keep in sync,
/// because there is only one text. A `limits` array in the JSON that said
/// something the printed report did not say would be the worse of the two
/// failures — a machine-readable claim is the one that gets believed without
/// being read.
struct Limit {
    lines: &'static [&'static str],
}

impl Limit {
    /// The same words as the report, on one line.
    fn sentence(&self) -> String {
        self.lines.join(" ")
    }
}

/// The CPU half. Printed whenever there is a proof at all.
const LIMIT_PCS: Limit = Limit {
    lines: &[
        "structural check only — the Intel PCS signature",
        "chain is NOT verified at this milestone",
    ],
};

/// The GPU half. A different limit, and it must be stated separately: a report
/// that names one and not the other reads as if the unnamed claim were the
/// stronger of the two.
const LIMIT_NRAS: Limit = Limit {
    lines: &[
        "the GPU verdict is nvidia-smi's own conf-compute",
        "report; the NVIDIA attestation quote (NRAS) is NOT",
        "verified at this milestone either",
    ],
};

/// Which limitations apply to a run that produced `proof` and read `gpu_cc_on`.
///
/// SECURITY: the list is a function of what was MEASURED, never of the verdict.
/// A proven run and a failed run that both obtained a TDX quote carry the same
/// PCS caveat, because the caveat is about how the quote was checked.
fn limits_of(has_proof: bool, gpu_cc_on: bool) -> Vec<&'static Limit> {
    let mut v = Vec::new();
    if has_proof {
        v.push(&LIMIT_PCS);
        if gpu_cc_on {
            v.push(&LIMIT_NRAS);
        }
    }
    v
}

pub fn render_report(
    outcome: &CcOutcome,
    spent: f64,
    machine: CcMachine,
    vm_state: &str,
) -> String {
    let mut r = String::new();
    use std::fmt::Write;
    // A FAILED run reports what it had already measured, exactly like a proven
    // one (review round 1 of task 4, finding I1). The step that fails is very
    // often the step that measured — `attest_gpu` stores the mode it read and
    // then refuses it, `validate_encryption` stores four checks and then
    // refuses on one of them — so reading these only from `Proven` left the
    // report saying "nothing was measured" about the runs where the
    // measurement IS the news. `Refused` has nothing to carry: it is reached
    // only from a verdict, upstream of all of this.
    let (gpu, encryption) = match outcome {
        CcOutcome::Proven {
            gpu, encryption, ..
        }
        | CcOutcome::Failed {
            gpu, encryption, ..
        } => (gpu.as_ref(), encryption.as_ref()),
        CcOutcome::Refused { .. } => (None, None),
    };
    // The CPU measurements, wherever they ended up. Read from the outcome as a
    // FIELD rather than from the `Proven` arm (review round 1 of task 5,
    // I1+I2): a proof made at step 3 is still a proof when step 12 fails, and
    // on the full machine that is not an edge case but the only report the
    // pipeline produces at this milestone. `Refused` has no proof by
    // construction — `ProofBundle::from_verdict` refuses a rejected verdict.
    let proof = match outcome {
        CcOutcome::Proven { proof, .. } => Some(proof),
        CcOutcome::Failed { proof, .. } => proof.as_ref(),
        CcOutcome::Refused { .. } => None,
    };
    match outcome {
        CcOutcome::Proven { .. } => {
            // SECURITY: the combined verdict, and the only line in the program
            // that claims both halves. It requires a `GpuEvidence` that exists
            // (only `attest_gpu` builds one) AND says ON. A proven CPU with a
            // failed, refused or unmeasured GPU prints the cpu-only header and
            // an "exposed" gpu line below — never `cpu tdx-structural + gpu`.
            //
            // Both headers stay inside this arm, and that is the whole reason
            // the block below could be lifted out of it: the MEASUREMENTS are
            // facts about the run, the word PROVEN is a verdict about it.
            if gpu.is_some_and(GpuEvidence::cc_mode_on) {
                writeln!(r, "ENCLAVE PROVEN (cpu tdx-structural + gpu cc-mode)").ok();
            } else {
                writeln!(r, "ENCLAVE PROVEN (cpu tdx-structural)").ok();
            }
        }
        CcOutcome::Refused { reason } => {
            writeln!(r, "ATTESTATION REFUSED").ok();
            writeln!(r, "  reason     {}", sanitize(reason)).ok();
        }
        CcOutcome::Failed { step, error, .. } => {
            writeln!(r, "PIPELINE FAILED at {step:?}").ok();
            writeln!(r, "  error      {}", sanitize(error)).ok();
        }
    }
    if let Some(p) = proof {
        writeln!(r, "  verifier   {}", p.verifier()).ok();
        writeln!(r, "  nonce      {}", p.nonce_hex()).ok();
        writeln!(r, "  mr_td      {}", p.mr_td_hex()).ok();
        for (i, h) in p.rtmr_hex().iter().enumerate() {
            writeln!(r, "  rtmr{i}      {h}").ok();
        }
        // The GPU half has its own limit and it is NOT the same one. What
        // the pipeline read is the driver's answer about its own GPU; the
        // NVIDIA attestation quote (NRAS / nvtrust) is a later milestone.
        // The spec (2026-08-14) asks for it to be printed "as for
        // Intel PCS", and it has to be: a report that states one limit and
        // not the other reads as if the second claim were the stronger of
        // the two.
        //
        // Both come from `limits_of`, which the JSON report reads too, so the
        // two renderings cannot state different limits (2026-08-16).
        let mut first = true;
        for limit in limits_of(true, gpu.is_some_and(GpuEvidence::cc_mode_on)) {
            for line in limit.lines {
                if first {
                    writeln!(r, "  limit      {line}").ok();
                    first = false;
                } else {
                    writeln!(r, "             {line}").ok();
                }
            }
        }
    }
    if let Some(line) = gpu_line(machine, gpu) {
        writeln!(r, "{line}").ok();
    }
    for line in encryption_lines(machine, encryption) {
        writeln!(r, "{line}").ok();
    }
    writeln!(r, "  spent      ≈ {spent:.2} $").ok();
    writeln!(
        r,
        "  vm         {} · ≈ {:.2} $/h if left running",
        sanitize(vm_state),
        machine.hourly_usd()
    )
    .ok();
    r
}

// ── The machine-readable report ───────────────────────────────────────────────

/// Where every `x0ne cc` run leaves its report for other programs to read.
///
/// Under `~/.x0ne` rather than `~/.config`: this is a RECORD of a run, not a
/// setting, and it sits beside the other things the orchestrator writes about
/// itself. `--serve`'s `GET /attestation` reads exactly this file, so a console
/// asking "is the model I am talking to inside an enclave" is answered by the
/// last measurement rather than by a flag somebody set.
pub fn last_report_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".x0ne").join("cc").join("last-report.json"))
}

/// How long a report may stand in for a live measurement.
///
/// One hour, and the number is a policy choice rather than a fact about the
/// hardware, so here is the reasoning. The thing the file attests is "the VM
/// this report names was measured and found to be an enclave". That does not
/// decay on its own — a TDX quote is not a session token. What decays is the
/// RELATIONSHIP between the report and the machine now serving: the VM can be
/// stopped, recreated, or replaced by another at the same name, and nothing in
/// the file would change. An hour is short enough that a console cannot show a
/// green badge for a machine destroyed this morning, and long enough that a
/// working session does not have to re-attest — the pipeline costs money and
/// about ten minutes.
///
/// SECURITY: the direction of the failure is what matters. A stale report is
/// reported as `standard`/"confidential GPU disabled", never as `unknown` and never as
/// the last known good value. `x0ne-confidential` states the rule this follows:
/// absence of a claim is not a weak claim.
pub const ATTESTATION_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// The report as JSON: the same run, the same verdict, the same limits.
///
/// # Why this is not `#[derive(Serialize)]` on `CcOutcome`
///
/// A derived shape would publish the enum's field names and nothing else, and
/// the two things a reader most needs are not fields of it: the STATE (the
/// three-valued badge, see [`crate::tui::app::EnclaveStatus`]) and the LIMITS
/// (what this milestone does not check). Both are computed here from exactly
/// the sources the text report uses — `EnclaveStatus::from_outcome` and
/// `limits_of` — so `--json` and the printed report cannot disagree about
/// the verdict or about what it is worth.
///
/// SECURITY: nothing here can build anything. It reads an outcome that already
/// exists; `ProofBundle` is `Serialize` and not `Deserialize` precisely so this
/// direction stays one-way.
pub fn render_report_json(
    outcome: &CcOutcome,
    spent: f64,
    machine: CcMachine,
    vm_state: &str,
) -> serde_json::Value {
    use crate::tui::app::EnclaveStatus;

    let (gpu, encryption) = match outcome {
        CcOutcome::Proven {
            gpu, encryption, ..
        }
        | CcOutcome::Failed {
            gpu, encryption, ..
        } => (gpu.as_ref(), encryption.as_ref()),
        CcOutcome::Refused { .. } => (None, None),
    };
    let proof = match outcome {
        CcOutcome::Proven { proof, .. } => Some(proof),
        CcOutcome::Failed { proof, .. } => proof.as_ref(),
        CcOutcome::Refused { .. } => None,
    };

    // SECURITY: `verdict` is derived from the variant, so a `Refused` outcome
    // cannot serialize as `proven` however much evidence it happens to carry.
    let verdict = match outcome {
        CcOutcome::Proven { .. } => "proven",
        CcOutcome::Refused { .. } => "refused",
        CcOutcome::Failed { .. } => "failed",
    };
    let status = EnclaveStatus::from_outcome(outcome);
    let (_glyph, label, _rgb) = status.badge();

    let mut v = serde_json::json!({
        "verdict": verdict,
        "state": status.wire_state(),
        "label": label,
        "machine": match machine {
            CcMachine::Prover => "prover",
            CcMachine::FullH100 => "full-h100",
        },
        // Only the full machine serves anything. Saying "Qwen…" for the prover
        // would name a model that was never downloaded, let alone served.
        "model": match machine {
            CcMachine::Prover => serde_json::Value::Null,
            CcMachine::FullH100 => serde_json::Value::String(VLLM_MODEL.to_string()),
        },
        "measured_at": chrono::Utc::now().to_rfc3339(),
        "spent_usd": spent,
        // Same `sanitize` as the text report, and for the same reason: this
        // string started life as gcloud output.
        "vm": sanitize(vm_state),
        "limits": limits_of(proof.is_some(), gpu.is_some_and(GpuEvidence::cc_mode_on))
            .iter()
            .map(|l| l.sentence())
            .collect::<Vec<_>>(),
    });

    match outcome {
        CcOutcome::Refused { reason } => {
            v["reason"] = serde_json::Value::String(sanitize(reason));
        }
        CcOutcome::Failed { step, error, .. } => {
            v["step"] = serde_json::Value::String(format!("{step:?}"));
            v["error"] = serde_json::Value::String(sanitize(error));
        }
        CcOutcome::Proven { .. } => {}
    }

    if let Some(p) = proof {
        let mut pj = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = pj.as_object_mut() {
            obj.insert(
                "strength".into(),
                serde_json::Value::String(p.strength().into()),
            );
        }
        v["proof"] = pj;
    }
    if let Some(g) = gpu {
        v["gpu"] = serde_json::json!({
            "cc_on": g.cc_mode_on(),
            "driver_version": g.driver_version(),
        });
    }
    if let Some(c) = encryption {
        v["encryption"] = serde_json::json!({
            "swap_off": c.swap_off(),
            "localhost_only": c.localhost_only(),
            "firewall_closed": c.firewall_closed(),
            "disk": match c.disk() {
                DiskEncryption::GoogleManaged => "google-managed-key",
                DiskEncryption::CustomerKey => "customer-key",
            },
            "all_pass": c.all_pass(),
        });
    }
    v
}

/// Write the JSON report to [`last_report_path`], mode 0600.
///
/// Written on EVERY run, `--json` or not: it is a report, and the operator who
/// wants yesterday's answer should not have had to know to ask for it
/// yesterday. A refused or failed run is written too — that is the run whose
/// record matters most, and a directory holding only successes describes a
/// machine that has never failed.
///
/// SECURITY: 0600, because the file names the VM, the model, and the
/// measurements of a machine the operator pays for. It is created with the mode
/// rather than chmod'ed afterwards, so there is no window in which it is
/// world-readable. A failure to write is returned, never swallowed into a
/// silent success — a report nobody can read is not a report.
pub fn write_last_report(report: &serde_json::Value) -> anyhow::Result<std::path::PathBuf> {
    let path = last_report_path()
        .ok_or_else(|| anyhow::anyhow!("no home directory: nowhere to write the report"))?;
    write_report_to(&path, report)
}

/// The write itself, on a path the caller chose.
///
/// Split from [`write_last_report`] so the 0600 can be asserted by a test that
/// does not have to write into the operator's real home to do it — the mode is
/// the security property here, and a property nothing checks is one that drifts
/// (mutant M20, 2026-08-16: `0o600` → `0o644` survived every test).
pub fn write_report_to(
    path: &std::path::Path,
    report: &serde_json::Value,
) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Two steps, and the ORDER is the point. `.mode(0o600)` applies only when
    // the file is CREATED, so it closes the window on a first run. `truncate`
    // reuses an existing inode and its permissions, so a file left behind at
    // 0644 would keep it — hence the explicit `set_permissions`, and hence it
    // runs BEFORE the content is written rather than after: chmod'ing
    // afterwards would leave the report world-readable for the length of the
    // write.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(serde_json::to_string_pretty(report)?.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(path.to_path_buf())
}

/// What `GET /attestation` publishes, from the last report on disk.
///
/// # Why this returns a STATE and not a boolean
///
/// A front end handed `verified: true/false` computes the badge colour itself,
/// and the rule for what may be green then lives in a browser — where it will
/// be re-derived, differently, by the next person who touches the CSS. Three
/// named states plus a label leave nothing to compute: the caller renders what
/// it is given. `cpu_only` exists for exactly this reason; it is the state a
/// boolean has to round, and rounding it either lies or hides a real result.
///
/// # SECURITY: nothing in the file is echoed, everything is re-derived
///
/// `~/.x0ne/cc/last-report.json` is a file, and a file is something an attacker
/// with local write access can rewrite. So:
///
/// * `state` is parsed through `EnclaveStatus::from_wire` and an unknown
///   token is NOT a state — it is `none`, the same as no file at all.
/// * `label` is derived from the parsed state, never read from the file. A
///   forged report cannot pair `state: "standard"` with `label:
///   "confidential"`, which is the pairing a careless front end would print.
/// * `limits` are carried through as strings, because they can only weaken a
///   claim; a forged report that omits them still cannot strengthen `state`.
///
/// # Freshness
///
/// A report older than [`ATTESTATION_TTL`] is `standard` / "confidential GPU disabled",
/// and keeps its `measured_at` so a caller can say how old it is. It is not
/// `unknown` and not the last known good value: `x0ne-confidential` states the
/// rule this follows — absence of a claim is not a weak claim.
///
/// `none` and `standard` are different facts and both are honest: `none` is
/// "no measurement exists", `standard` is "a measurement exists and it does not
/// say confidential". Both carry the same label, because to a reader deciding
/// whether to stake anything they mean the same thing.
pub fn attestation_view(
    report: Option<&serde_json::Value>,
    now: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    use crate::tui::app::EnclaveStatus;

    let nothing = || {
        serde_json::json!({
            "state": "none",
            "label": EnclaveStatus::Standard.badge().1,
            "limits": [],
            "measured_at": serde_json::Value::Null,
        })
    };

    let Some(r) = report else { return nothing() };
    // No readable timestamp means the report cannot be aged, and a claim that
    // cannot be aged is not one this route will publish.
    let Some(measured_at) = r.get("measured_at").and_then(serde_json::Value::as_str) else {
        return nothing();
    };
    let Ok(when) = chrono::DateTime::parse_from_rfc3339(measured_at) else {
        return nothing();
    };
    let limits = r
        .get("limits")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let age = now.signed_duration_since(when.with_timezone(&chrono::Utc));
    // A negative age (a report stamped in the future — a clock skew, or a
    // forged file) is not fresh either: `age` below zero fails the range check
    // rather than passing it, which is why this is a range and not a `<`.
    let fresh = age >= chrono::Duration::zero()
        && age <= chrono::Duration::from_std(ATTESTATION_TTL).unwrap_or(chrono::Duration::zero());

    let status = r
        .get("state")
        .and_then(serde_json::Value::as_str)
        .and_then(EnclaveStatus::from_wire);
    let status = match (status, fresh) {
        (Some(s), true) => s,
        // Stale, or a state token this build does not know: the measurement
        // exists, and it does not stand for now.
        _ => EnclaveStatus::Standard,
    };
    serde_json::json!({
        "state": status.wire_state(),
        "label": status.badge().1,
        "limits": limits,
        "measured_at": measured_at,
    })
}

/// Read [`last_report_path`] as JSON, or `None` when there is nothing usable.
///
/// Absent, unreadable and malformed collapse to the same answer on purpose:
/// each of them means "no measurement this process can stand behind", and
/// telling them apart on a public route would report the filesystem.
pub fn read_last_report() -> Option<serde_json::Value> {
    let path = last_report_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use x0ne_confidential::model::Verdict;

    #[derive(Default)]
    struct ScriptedExec {
        responses: std::collections::VecDeque<CmdOut>,
        pub sent: Vec<Vec<String>>,
        /// The `display` label of each call, recorded because it is not
        /// decoration: the production exec keys its SSH pause on it.
        pub displays: Vec<String>,
        /// The budget each call was given. Recorded because the first SSH
        /// probe must get a different one from the rest.
        pub timeouts: Vec<u64>,
    }
    /// The sentinel that makes this exec play the part a real TDX VM plays: on
    /// it, the answer is a quote built around the nonce found in the command it
    /// was handed. Same device as `cc_cmd`'s scripted exec, for the same
    /// reason — since finding C1, `attest_tdx` draws a fresh nonce and FAILS on
    /// a quote that does not echo it, so nothing scripted can get past
    /// attestation without answering it.
    const HONEST_QUOTE: &str = "<a quote echoing the nonce we were sent>";

    fn honest_quote() -> CmdOut {
        out(HONEST_QUOTE)
    }

    #[async_trait]
    impl CcExec for ScriptedExec {
        /// Answers instantly and NEVER sleeps: the SSH pause belongs to the
        /// production exec (`cc_cmd.rs`). A `wait_ssh` test that slept would
        /// take a quarter of an hour.
        async fn run(
            &mut self,
            display: &str,
            argv: &[String],
            timeout_secs: u64,
        ) -> anyhow::Result<CmdOut> {
            self.sent.push(argv.to_vec());
            self.displays.push(display.to_string());
            self.timeouts.push(timeout_secs);
            let mut out = self
                .responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("script exhausted"))?;
            if out.stdout == HONEST_QUOTE {
                // Mirrors the `printf %s {nonce_hex}` of `attest_tdx`.
                let hex = argv
                    .iter()
                    .find(|a| a.contains("inblob"))
                    .and_then(|c| c.split("printf %s ").nth(1))
                    .and_then(|c| c.split_whitespace().next())
                    .expect("HONEST_QUOTE scripted for a non-attest command");
                let nonce: [u8; 64] = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
                    .collect::<Vec<u8>>()
                    .try_into()
                    .expect("a 64-byte nonce");
                out.stdout = base64_encode(&synthetic_quote_bytes(&nonce));
            }
            Ok(out)
        }
    }
    fn out(stdout: &str) -> CmdOut {
        CmdOut {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// THE lock of the spec, prover side: a "PROVEN" report cannot be
    /// constructed without Verdict::Verified. The same pattern as
    /// EnclaveStatus::from_verdict — a failure here would mean that a proof
    /// report can exist without a proof.
    #[test]
    fn a_proof_bundle_cannot_exist_without_a_verified_verdict() {
        let rejected = Verdict::Rejected {
            reason: "x".into(),
            verifier: "t".into(),
        };
        assert!(ProofBundle::from_verdict(&rejected, None, "aa".into()).is_none());
    }

    #[tokio::test]
    async fn observe_issues_the_three_probes_and_reads_them() {
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#), // auth list
                out("example-project-123456\n"),                   // config get-value project
                out(r#"[{"name":"img"}]"#),                        // images list
                out("[]"),                                         // instances list
            ]
            .into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        let obs = runner.observe().await.unwrap();
        assert_eq!(obs.auth_account.as_deref(), Some("a@b.c"));
        assert_eq!(obs.instance, InstanceState::Absent);
        assert_eq!(exec.sent.len(), 4);
        assert_eq!(exec.sent[0][0], "auth"); // gcloud auth list …
    }

    /// Measured on 2026-08-13 on a real project: `gcloud compute images list
    /// --format=json` returns ~31 KB for ONE image, `RunCommand` truncates at
    /// 24,000 bytes by inserting a marker in the middle, and the truncated
    /// JSON is no longer JSON — `parse_image_family_exists` read "absent" and
    /// `x0ne cc` refused to plan on a perfectly valid project.
    /// A failure of this test would mean the probes went back to an
    /// unprojected `--format=json`, so the command becomes unusable again as
    /// soon as the output exceeds the capture cap.
    #[tokio::test]
    async fn the_probes_project_their_json_so_it_survives_the_capture_cap() {
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("example-project-123456\n"),
                out(r#"[{"name":"img"}]"#),
                out("[]"),
            ]
            .into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        CcRunner::new(&mut exec, cfg).observe().await.unwrap();
        for argv in &exec.sent {
            for arg in argv {
                assert_ne!(
                    arg, "--format=json",
                    "a bare --format=json is truncatable; project the fields: {argv:?}"
                );
            }
        }
        assert!(
            exec.sent[2].iter().any(|a| a == "--format=json(name)"),
            "{:?}",
            exec.sent[2]
        );
        assert!(
            exec.sent[3]
                .iter()
                .any(|a| a == "--format=json(name,status,zone)"),
            "the instance parser reads name/status/zone: {:?}",
            exec.sent[3]
        );
    }

    /// The CreateVm step sends EXACTLY the argv of VmSpec::to_gcloud_argv —
    /// what the wizard displayed is what goes out. Repo invariant 5:
    /// approving what you do not see is not approving.
    #[tokio::test]
    async fn create_vm_runs_the_exact_displayed_argv() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let expected = cfg.machine.vm_spec("x0ne-cc").to_gcloud_argv();
        let mut exec = ScriptedExec {
            responses: [out("done")].into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        runner.run_step(&CcStep::CreateVm).await.unwrap();
        assert_eq!(exec.sent[0], expected);
    }

    /// A non-zero exit is never swallowed: the step fails with the stdout/err
    /// in the error, and it is the upper level that stops the VM and reports.
    /// Fixture deliberately with EMPTY stdout, stderr carrying the message: gcloud
    /// writes its diagnostics to stderr, not stdout (finding I3, review
    /// 2026-08-13) — a failure here would mean an error message that
    /// arrives only through stderr is silently swallowed.
    #[tokio::test]
    async fn a_failed_command_fails_the_step_with_its_output() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = ScriptedExec {
            responses: [CmdOut {
                stdout: String::new(),
                stderr: "Quota GPUS exceeded".into(),
                exit_code: 1,
            }]
            .into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        let err = runner
            .run_step(&CcStep::CreateVm)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Quota"), "the operator must see WHY: {err}");
    }

    /// Same synthetic TD Quote v4 layout as `x0ne_confidential::tdx_quote`'s
    /// own tests (header 48B, body 584B, mr_td @136, report_data @520) —
    /// duplicated here rather than exported, because only tests need it.
    fn synthetic_quote_bytes(nonce: &[u8; 64]) -> Vec<u8> {
        let mut q = vec![0u8; 48 + 584 + 8];
        q[0] = 4;
        q[4..8].copy_from_slice(&0x81u32.to_le_bytes());
        q[48 + 136..48 + 184].copy_from_slice(&[0xAB; 48]);
        q[48 + 520..48 + 584].copy_from_slice(nonce);
        q
    }

    fn base64_encode(data: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    /// SECURITY (finding C1, review round 1): a REJECTED quote fails the
    /// STEP, not only the report.
    ///
    /// The step stored the verdict and returned `Ok(())`. Since the loop only
    /// looks at the step's result, nothing consulted that verdict anymore:
    /// on `--machine full`, a replayed nonce let the driver install run
    /// (≈ 37 min + reboot) and then the whole vLLM chain on an enclave that
    /// nobody proved — exactly what the SECURITY comment of `build_plan`
    /// forbids, and the owner's decision "never proceed without
    /// proof". A failure of this test means that path is reopened.
    #[tokio::test]
    async fn a_replayed_quote_fails_the_step_and_reads_as_refused() {
        let stale = {
            // same synthetic layout as x0ne-confidential tdx_quote tests
            let mut q = vec![0u8; 48 + 584 + 8];
            q[0] = 4;
            q[4..8].copy_from_slice(&0x81u32.to_le_bytes());
            // report_data left zeroed — cannot match a fresh /dev/urandom nonce
            base64_encode(&q)
        };
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = ScriptedExec {
            responses: [out(&stale)].into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        let err = runner
            .run_step(&CcStep::AttestTdx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("nonce"),
            "the step must fail, and say why: {err}"
        );

        // And the report says REFUSED, not "PIPELINE FAILED at AttestTdx":
        // the rejection outranks the step failure it caused, even though the
        // loop hands both in.
        match runner.outcome(Some((CcStep::AttestTdx, err))) {
            CcOutcome::Refused { reason } => assert!(reason.contains("nonce"), "{reason}"),
            _ => panic!("a stale quote must refuse"),
        }
    }

    /// wait_ssh retries then gives up with a message that says what to do.
    /// `assert_eq!(exec.sent.len(), SSH_RETRIES)` is the point of the test
    /// (finding I2, review 2026-08-13): without it, a `wait_ssh` that
    /// gave up after 1 attempt or looped forever would still pass —
    /// only counting the commands actually sent proves that
    /// the loop ran EXACTLY SSH_RETRIES rounds, no more, no fewer.
    #[tokio::test]
    async fn wait_ssh_gives_up_with_a_named_next_step() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let fails: Vec<CmdOut> = (0..SSH_RETRIES)
            .map(|_| CmdOut {
                stdout: "connection refused".into(),
                stderr: String::new(),
                exit_code: 255,
            })
            .collect();
        let mut exec = ScriptedExec {
            responses: fails.into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        let err = runner
            .run_step(&CcStep::WaitSsh)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("gcloud compute ssh"),
            "name the manual probe: {err}"
        );
        // "up to 15 min" is a hand-computed literal (880 s ceiling rounded
        // UP), not a recomputation of the code's rounding: when this message
        // truncated to "14 min", no test caught it — the only assertion on
        // the figure recomputed the same division (final review, 2026-08-14).
        assert!(
            err.contains("up to 15 min"),
            "state the real ceiling: {err}"
        );
        assert_eq!(
            exec.sent.len(),
            SSH_RETRIES,
            "the loop must run exactly SSH_RETRIES attempts"
        );
        // The probe must announce itself with WAIT_SSH_DISPLAY, because that
        // label is the ONLY thing telling the production exec to pause
        // before the next attempt. Rename it here without renaming it there
        // and all SSH_RETRIES attempts fire in under a second, minutes early —
        // a flag read by nobody, this repo's recurring defect. The runner
        // side is what this test can pin; the exec side pauses on the same
        // constant (`cc_cmd.rs`).
        assert!(
            exec.displays.iter().all(|d| d == WAIT_SSH_DISPLAY),
            "every SSH probe must carry the label the pause keys on: {:?}",
            exec.displays
        );
    }

    /// The FIRST SSH probe is not the same command as the following ones:
    /// on a machine that has never run `gcloud compute ssh`, it generates
    /// `~/.ssh/google_compute_engine` and pushes the public key into the
    /// project metadata, which commonly takes more than 20 s. The runtime kills
    /// HARD at the deadline: a short budget does not just expire, it kills the
    /// key push mid-flight, and the 27 attempts then fail for
    /// a reason unrelated to the VM — `WaitSsh` then blames the
    /// boot. A failure of this test would mean the first invocation on
    /// a fresh machine can no longer succeed (minor M4 of T6, which became blocking
    /// with the short timeout of review round 1).
    #[tokio::test]
    async fn the_first_ssh_probe_is_given_time_to_generate_its_key() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let fails: Vec<CmdOut> = (0..SSH_RETRIES)
            .map(|_| CmdOut {
                stdout: String::new(),
                stderr: "connection refused".into(),
                exit_code: 255,
            })
            .collect();
        let mut exec = ScriptedExec {
            responses: fails.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        let _ = runner.run_step(&CcStep::WaitSsh).await;

        assert_eq!(exec.timeouts.len(), SSH_RETRIES);
        assert_eq!(
            exec.timeouts[0], SSH_FIRST_PROBE_TIMEOUT_SECS,
            "the first probe has a key to generate and push"
        );
        assert!(
            exec.timeouts[1..]
                .iter()
                .all(|t| *t == SSH_PROBE_TIMEOUT_SECS),
            "every later probe is a warm one: {:?}",
            exec.timeouts
        );
        // The generous first probe must not reopen finding I1: the whole step
        // stays bounded, and the advertised ceiling accounts for it.
        assert!(
            wait_ssh_budget_secs() <= 15 * 60,
            "WaitSsh must stay under 15 min, got {} s",
            wait_ssh_budget_secs()
        );
        assert!(
            wait_ssh_budget_secs() >= SSH_FIRST_PROBE_TIMEOUT_SECS,
            "the advertised ceiling must include the first probe's larger budget"
        );
    }

    /// The loop must stop at the first success, neither continue nor
    /// stop before: two failures then an exit 0 → Ok after exactly
    /// 3 attempts sent. A failure of this test would mean wait_ssh
    /// ignores a late success, or sends extra attempts after
    /// having already succeeded.
    #[tokio::test]
    async fn wait_ssh_succeeds_after_transient_failures() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let responses: Vec<CmdOut> = vec![
            CmdOut {
                stdout: "connection refused".into(),
                stderr: String::new(),
                exit_code: 255,
            },
            CmdOut {
                stdout: "connection refused".into(),
                stderr: String::new(),
                exit_code: 255,
            },
            CmdOut {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            },
        ];
        let mut exec = ScriptedExec {
            responses: responses.into(),
            sent: vec![],
            displays: vec![],
            timeouts: vec![],
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        runner.run_step(&CcStep::WaitSsh).await.unwrap();
        assert_eq!(
            exec.sent.len(),
            3,
            "must stop at the first success, not loop past it"
        );
    }

    /// SECURITY (money): the stop the budget gate issues must be a stop that
    /// WORKS on the machine it is watching.
    ///
    /// An `a3-highgpu-1g` carries a Local SSD, and GCE refuses to stop such an
    /// instance unless the caller states what to do with it. Measured in
    /// production on 2026-08-14: `HTTPError 400: VM has a Local SSD attached
    /// but an undefined value for discard-local-ssd`. A failure here means the
    /// cap can fire, the code can report a stop, and a 3.72 $/h machine keeps
    /// billing.
    #[tokio::test]
    async fn the_stop_can_actually_stop_the_machine_the_budget_gate_watches() {
        let cfg = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = ScriptedExec {
            responses: vec![out("")].into(),
            ..Default::default()
        };
        {
            let mut runner = CcRunner::new(&mut exec, cfg);
            runner.stop_vm().await.expect("the scripted stop exits 0");
        }
        assert_eq!(exec.sent.len(), 1, "one stop, one command");
        let argv = &exec.sent[0];
        assert!(
            argv.iter().any(|a| a == "--discard-local-ssd=true"),
            "without this flag GCE refuses to stop an a3: {argv:?}"
        );
        assert!(argv.iter().any(|a| a == "stop") && argv.iter().any(|a| a == "x0ne-cc"));
    }

    /// An exec whose transport dies on its `die_at`-th call and stays dead.
    ///
    /// Separate from `ScriptedExec` rather than a flag on it: `ScriptedExec` is
    /// built by struct literal in a dozen tests, and this one has to answer a
    /// question the scripted queue cannot express — "the command was never
    /// sent", as opposed to "it was sent and came back badly".
    struct DyingExec {
        calls: usize,
        die_at: usize,
    }

    #[async_trait]
    impl CcExec for DyingExec {
        async fn run(&mut self, _d: &str, _argv: &[String], _t: u64) -> anyhow::Result<CmdOut> {
            self.calls += 1;
            if self.calls >= self.die_at {
                return Err(ExecTransportDown::raise(
                    "x0ne-runtime returned an empty response line",
                ));
            }
            Err(anyhow::anyhow!(
                "ssh: connect to host port 22: Connection refused"
            ))
        }
    }

    /// THE fix of the first real H100 run: a dead bridge stops the wait
    /// instead of being probed twenty-six more times.
    ///
    /// Measured 2026-08-14 — the runtime died, `wait_ssh` spent every one of
    /// its attempts on a pipe nobody was reading, ~10 min at 3.72 $/h, and the
    /// step then blamed the VM. A failure here means either the short-circuit
    /// is gone (the count climbs back to SSH_RETRIES) or the diagnosis lies
    /// again (the message stops naming the bridge and the unknown VM state).
    #[tokio::test]
    async fn a_dead_bridge_ends_the_ssh_wait_instead_of_probing_a_corpse() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = DyingExec {
            calls: 0,
            die_at: 3,
        };
        let err = {
            let mut runner = CcRunner::new(&mut exec, cfg);
            let e = runner
                .run_step(&CcStep::WaitSsh)
                .await
                .unwrap_err()
                .to_string();
            // SECURITY (money): the runner must KNOW its transport is gone —
            // that flag is what tells the caller the safety stop needs a fresh
            // runtime instead of this dead one (`cc_cmd::rescue_stop_if_the_
            // bridge_died`). Silently `false` here and the stop goes into the
            // corpse, which is run 1 of the H100 milestone exactly.
            assert!(
                runner.transport_is_down(),
                "a dead transport must be remembered"
            );
            e
        };
        assert_eq!(
            exec.calls, 3,
            "the wait must stop ON the fatal probe, not after {SSH_RETRIES}"
        );
        assert!(
            exec.calls < SSH_RETRIES,
            "SSH_RETRIES is what this test exists to NOT spend"
        );
        assert!(
            err.contains("bridge"),
            "the report must name what died: {err}"
        );
        assert!(
            err.contains("UNKNOWN"),
            "the VM state is no longer knowable, say so: {err}"
        );
        assert!(
            err.contains("gcloud compute instances list"),
            "an operator with an unknown VM needs the command that answers: {err}"
        );
        assert!(
            !err.contains("SSH not reachable"),
            "the bridge died; blaming the VM's boot is the lie we removed: {err}"
        );
    }

    /// The other half of the seam, and the one that keeps the fix honest: an
    /// ORDINARY failure is still retried. Twenty-six of these are what the
    /// 27 attempts are for — a probe that reached the VM and found it not
    /// ready. If this test ever fails, the short-circuit has grown too wide
    /// and `WaitSsh` gives up on the first refused connection.
    #[tokio::test]
    async fn an_ordinary_probe_failure_is_still_retried() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        // `die_at` past the retry budget: every call is an ordinary error.
        let mut exec = DyingExec {
            calls: 0,
            die_at: SSH_RETRIES + 1,
        };
        let err = {
            let mut runner = CcRunner::new(&mut exec, cfg);
            let e = runner
                .run_step(&CcStep::WaitSsh)
                .await
                .unwrap_err()
                .to_string();
            // And the flag stays down. A VM that never came up is not a dead
            // bridge: raising it here would spawn a second runtime on every
            // ordinary failed run, and would let the rescue path claim a
            // transport death that never happened.
            assert!(
                !runner.transport_is_down(),
                "an unreachable VM is not a dead transport"
            );
            e
        };
        assert_eq!(
            exec.calls, SSH_RETRIES,
            "an unreachable VM still gets all its attempts"
        );
        assert!(err.contains("SSH not reachable"), "{err}");
    }

    /// The reboot wait carries the same reading, and it is the loop that
    /// produced the lying message: on 2026-08-14 the bridge died during the
    /// driver reboot and the operator was told the VM had not come back.
    #[tokio::test]
    async fn a_dead_bridge_ends_the_reboot_wait_and_does_not_blame_the_vm() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = DyingExec {
            calls: 0,
            die_at: 1,
        };
        let err = {
            let mut runner = CcRunner::new(&mut exec, cfg);
            runner
                .wait_reboot("old-boot-id")
                .await
                .unwrap_err()
                .to_string()
        };
        assert_eq!(exec.calls, 1);
        assert!(err.contains("bridge") && err.contains("UNKNOWN"), "{err}");
        assert!(
            !err.contains("come back from its reboot"),
            "the VM was never asked: {err}"
        );
    }

    /// And the vLLM wait — the longest of the three, so the most expensive to
    /// spend on a dead pipe.
    #[tokio::test]
    async fn a_dead_bridge_ends_the_vllm_wait() {
        let cfg = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = DyingExec {
            calls: 0,
            die_at: 2,
        };
        let err = {
            let mut runner = CcRunner::new(&mut exec, cfg);
            runner.wait_vllm_bound().await.unwrap_err().to_string()
        };
        assert_eq!(
            exec.calls, 2,
            "one ordinary retry, then the fatal one ends it"
        );
        assert!(err.contains("bridge") && err.contains("UNKNOWN"), "{err}");
        assert!(!err.contains("never bound"), "vLLM was never asked: {err}");
    }

    /// The FOURTH wait loop, found by review round 3 hiding behind a
    /// `let … else` while the other three were converted.
    ///
    /// Its omission was worse than six wasted probes on the most expensive step
    /// of the pipeline (it runs after a 15 GB download): `health_check` ends on
    /// a `bail!` that builds a NEW error, so the `ExecTransportDown` never
    /// reached `note_transport`, `transport_down` stayed false, and both halves
    /// of the round-2 fix went with it — `run_plan` sent its stop into the
    /// corpse and the rescue runtime was never started. A failure here is the
    /// whole 2026-08-14 money bug, restored.
    ///
    /// Driven through `run_step` rather than `health_check` directly, because
    /// the flag is the point and `run_step` is where it is written.
    #[tokio::test]
    async fn a_dead_bridge_ends_the_health_check_and_still_raises_the_flag() {
        let cfg = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = DyingExec {
            calls: 0,
            die_at: 2,
        };
        let err = {
            let mut runner = CcRunner::new(&mut exec, cfg);
            let e = runner
                .run_step(&CcStep::HealthCheck)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                runner.transport_is_down(),
                "the flag the rescue stop reads must survive this step's own error"
            );
            e
        };
        assert_eq!(
            exec.calls, 2,
            "one ordinary retry, then the fatal probe ends it"
        );
        assert!(
            exec.calls < VLLM_HEALTH_RETRIES,
            "the health budget is what this must NOT spend"
        );
        assert!(err.contains("bridge") && err.contains("UNKNOWN"), "{err}");
        assert!(
            !err.contains("never reported"),
            "vLLM was never asked; blaming the server is the lie this closes: {err}"
        );
    }

    /// The report of a Refused/Failed NEVER contains the word PROVEN, and
    /// always states the final state of the VM and the cost if it stays on.
    #[test]
    fn the_report_never_says_proven_without_a_bundle() {
        let r = render_report(
            &CcOutcome::Refused {
                reason: "nonce mismatch".into(),
            },
            0.11,
            CcMachine::Prover,
            "stopped",
        );
        assert!(!r.contains("PROVEN"));
        assert!(r.contains("nonce mismatch"));
        assert!(r.contains("stopped"));
        let r2 = render_report(
            &CcOutcome::Failed {
                step: CcStep::WaitSsh,
                error: "timeout".into(),
                proof: None,
                gpu: None,
                encryption: None,
            },
            0.05,
            CcMachine::Prover,
            "stopped",
        );
        assert!(!r2.contains("PROVEN"));
    }

    /// SECURITY (finding I1, review 2026-08-13): `reason`/`error` can
    /// carry text coming from remote stdout (VM or gcloud). A
    /// multi-line string must not be able to plant an `ENCLAVE PROVEN
    /// (structural)` line at column 0 of a Refused/Failed report — a "grep
    /// PROVEN" or a human reading the end of the report would take it for a
    /// real proof. A failure here would mean the report is forgeable
    /// by anyone who controls the stdout of the attested VM.
    #[test]
    fn a_forged_line_in_the_reason_cannot_fake_a_proven_report() {
        // \n plants a fake line at column 0; \r rewrites the current
        // terminal line from column 0 WITHOUT needing a newline; \x1b[2K is
        // the ANSI "clear line" escape — all three are control characters,
        // and the fix (round 2, finding I1) neutralizes the whole class, not
        // just '\n'.
        let hostile =
            "nonce mismatch\nENCLAVE PROVEN (structural)\r\x1b[2K  verifier   totally-legit";
        let r = render_report(
            &CcOutcome::Refused {
                reason: hostile.into(),
            },
            0.11,
            CcMachine::Prover,
            "stopped",
        );
        assert!(
            !r.contains("\nENCLAVE PROVEN"),
            "attacker-controlled newlines must not start a fake report line: {r}"
        );
        // NOT `!r.contains("\rENCLAVE")` — in `hostile`, \r is followed by
        // \x1b, never by "ENCLAVE", so that substring check is vacuously
        // true whether or not \r survives sanitize() (caught by mutation:
        // deleting \r from `sanitize`'s is_control() set still passed this
        // assertion). The real property is that no bare \r reaches the
        // output at all.
        assert!(!r.contains('\r'), "a bare CR must not survive either: {r}");
        assert!(!r.contains('\x1b'), "ANSI escapes must not survive: {r}");

        let r2 = render_report(
            &CcOutcome::Failed {
                step: CcStep::WaitSsh,
                error: hostile.into(),
                proof: None,
                gpu: None,
                encryption: None,
            },
            0.05,
            CcMachine::Prover,
            "stopped",
        );
        assert!(
            !r2.contains("\nENCLAVE PROVEN"),
            "same guarantee for the Failed arm: {r2}"
        );
        assert!(
            !r2.contains('\r'),
            "same guarantee for the Failed arm: {r2}"
        );
        assert!(
            !r2.contains('\x1b'),
            "same guarantee for the Failed arm: {r2}"
        );
    }

    /// `vm_state` is exposed too: in task 7 its natural source is
    /// `InstanceState::Transitional { status }`, a field taken directly from the
    /// gcloud JSON. The same sanitize() must apply to it, before
    /// task 7 wires the real value — a failure here would mean we
    /// hardened two of the three inputs of the report and forgot the third.
    #[test]
    fn vm_state_control_characters_are_neutralized_too() {
        let r = render_report(
            &CcOutcome::Refused {
                reason: "nonce mismatch".into(),
            },
            0.11,
            CcMachine::Prover,
            "stopped\nENCLAVE PROVEN (structural)\r\x1b[2K",
        );
        assert!(!r.contains("\nENCLAVE PROVEN"), "{r}");
        assert!(!r.contains('\r'), "{r}"); // real property, see the sibling test's comment
        assert!(!r.contains('\x1b'), "{r}");
    }

    /// The report of a proof carries mr_td, the nonce, the verifier, AND the
    /// structural limit in plain text — the demo must say what it proves.
    #[test]
    fn a_proven_report_carries_measurements_and_the_stated_limit() {
        let nonce = [3u8; 64];
        let quote = synthetic_quote_bytes(&nonce);
        let (claims, verdict) = x0ne_confidential::tdx_quote::verify(&quote, &nonce);
        let bundle = ProofBundle::from_verdict(
            &verdict,
            claims.as_ref(),
            x0ne_confidential::tdx_quote::hex(&nonce),
        )
        .unwrap();
        let r = render_report(
            &CcOutcome::Proven {
                proof: bundle,
                gpu: None,
                encryption: None,
            },
            0.11,
            CcMachine::Prover,
            "running",
        );
        assert!(r.contains("PROVEN"));
        assert!(r.contains("mr_td"));
        assert!(r.contains("structural"), "the limit must be printed: {r}");
        assert!(
            r.contains("Intel PCS"),
            "what is NOT verified must be named: {r}"
        );
        assert!(
            !r.contains("gpu"),
            "the prover has no GPU; a gpu line here would be a measurement of nothing: {r}"
        );
    }

    // ── The GPU half (task 3, 2026-08-14) ────────────────────────────────

    fn full() -> CcConfig {
        CcConfig {
            machine: CcMachine::FullH100,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        }
    }

    /// A real, verified `ProofBundle` — the CPU half genuinely proven, built
    /// the way `a_proven_report_carries_measurements_and_the_stated_limit`
    /// builds one. Used by the tests that need a proven CPU next to a GPU that
    /// is not.
    fn proven_cpu() -> ProofBundle {
        let nonce = [3u8; 64];
        let quote = synthetic_quote_bytes(&nonce);
        let (claims, verdict) = x0ne_confidential::tdx_quote::verify(&quote, &nonce);
        ProofBundle::from_verdict(&verdict, claims.as_ref(), tdx_quote::hex(&nonce)).unwrap()
    }

    // ── The JSON report ───────────────────────────────────────────────────

    /// SECURITY: `--json` and the printed report describe the same run.
    ///
    /// A machine-readable claim is the one that gets believed without being
    /// read: a console lighting a badge from `state` never sees the text a
    /// human would have compared it against. A failure here means the two
    /// renderings can say different things about the same outcome — the
    /// version that says "proven" wins by being the one nobody checks.
    #[test]
    fn the_json_report_says_the_same_thing_as_the_text_report() {
        let checks = EncryptionChecks {
            swap_off: true,
            localhost_only: true,
            firewall_closed: true,
            disk: DiskEncryption::GoogleManaged,
        };
        let cases = [
            CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: Some(GpuEvidence {
                    cc_mode_on: true,
                    driver_version: "580.126.09".into(),
                }),
                encryption: Some(checks.clone()),
            },
            CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: None,
                encryption: None,
            },
            CcOutcome::Refused {
                reason: "the quote did not echo the nonce".into(),
            },
            CcOutcome::Failed {
                step: CcStep::AttestGpu,
                error: "the GPU answered OFF".into(),
                proof: Some(proven_cpu()),
                gpu: Some(GpuEvidence {
                    cc_mode_on: false,
                    driver_version: "580.126.09".into(),
                }),
                encryption: None,
            },
        ];
        for outcome in cases {
            let text = render_report(&outcome, 1.25, CcMachine::FullH100, "running");
            let json = render_report_json(&outcome, 1.25, CcMachine::FullH100, "running");

            // The verdict, in the words each rendering uses for it.
            let headline = match json["verdict"].as_str().unwrap() {
                "proven" => "ENCLAVE PROVEN",
                "refused" => "ATTESTATION REFUSED",
                "failed" => "PIPELINE FAILED",
                other => panic!("unknown verdict {other}"),
            };
            assert!(
                text.contains(headline),
                "json says {} and the text does not: {text}",
                json["verdict"]
            );

            // The limits, line for line. The JSON joins the report's own
            // wrapped lines, so every line it joined must be findable in the
            // printed report — that is what "the same phrases" means here.
            let limits = json["limits"].as_array().unwrap();
            for limit in limits {
                let sentence = limit.as_str().unwrap();
                let source = [&LIMIT_PCS, &LIMIT_NRAS]
                    .into_iter()
                    .find(|l| l.sentence() == sentence)
                    .expect("a limit the JSON states that no constant defines");
                for line in source.lines {
                    assert!(
                        text.contains(line),
                        "a limit the JSON states is absent from the text: {line}\n{text}"
                    );
                }
            }
            // And the other direction: a text report that states a limit the
            // JSON omits is the failure that matters most.
            assert_eq!(
                text.contains("Intel PCS"),
                limits
                    .iter()
                    .any(|l| l.as_str().unwrap().contains("Intel PCS")),
                "the two renderings disagree about the Intel PCS limit: {text}"
            );
            assert_eq!(
                text.contains("NRAS"),
                limits.iter().any(|l| l.as_str().unwrap().contains("NRAS")),
                "the two renderings disagree about the NRAS limit: {text}"
            );
        }
    }

    /// SECURITY: a rejected verdict must serialize as a refusal, always.
    ///
    /// This is the JSON half of the rule `render_report` already holds for the
    /// text: the word PROVEN lives in the `Proven` arm alone. A failure means
    /// a console reading `state` or `verdict` can be handed "proven" for a run
    /// that proved nothing — and unlike the printed report, nobody would be
    /// there to notice the header said otherwise.
    #[test]
    fn a_rejected_verdict_serializes_as_refused_never_as_proven() {
        let rejected = Verdict::Rejected {
            reason: "the quote did not echo the nonce".into(),
            verifier: "tdx-structural".into(),
        };
        assert!(
            ProofBundle::from_verdict(&rejected, None, "aa".into()).is_none(),
            "the only door must stay shut on a rejection"
        );

        let outcome = CcOutcome::Refused {
            reason: "the quote did not echo the nonce".into(),
        };
        let json = render_report_json(&outcome, 0.4, CcMachine::FullH100, "stopped");
        assert_eq!(json["verdict"], serde_json::json!("refused"));
        assert_eq!(json["state"], serde_json::json!("standard"));
        assert_eq!(
            json["label"],
            serde_json::json!("confidential GPU disabled")
        );
        assert!(
            json.get("proof").is_none(),
            "a refusal has no proof to publish"
        );
        assert!(json["limits"].as_array().unwrap().is_empty());

        // A run that FAILED after measuring a CPU proof publishes the proof —
        // it is a measurement — and is still not `proven`, and still not green.
        let failed = CcOutcome::Failed {
            step: CcStep::AttestGpu,
            error: "the GPU answered OFF".into(),
            proof: Some(proven_cpu()),
            gpu: Some(GpuEvidence {
                cc_mode_on: false,
                driver_version: "580.126.09".into(),
            }),
            encryption: None,
        };
        let json = render_report_json(&failed, 0.4, CcMachine::FullH100, "stopped");
        assert_eq!(json["verdict"], serde_json::json!("failed"));
        assert_eq!(
            json["state"],
            serde_json::json!("standard"),
            "a failed run must never light the badge"
        );
        assert!(json["proof"]["mr_td_hex"].is_string());
    }

    /// SECURITY: the proof is serialize-only, and the derive must stay that way.
    ///
    /// The compiler-checked half is the `compile_fail` doctest on
    /// `ProofBundle`. This is the half that says why, and it fails loudly if
    /// someone adds `Deserialize` while rewriting the doc comment that
    /// explains the absence: with it, a hand-typed `~/.x0ne/cc/last-report.json`
    /// would mint a `ProofBundle`, and `EnclaveStatus::from_outcome` would
    /// light the badge green over a file anyone can write.
    #[test]
    fn a_proof_bundle_cannot_be_built_from_json() {
        let src = include_str!("cc_runner.rs");
        let at = src
            .find("pub struct ProofBundle {")
            .expect("the struct must still be named that");
        let derive_line = src[..at]
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with("#[derive"))
            .expect("ProofBundle must carry a derive attribute");
        assert!(
            derive_line.contains("Serialize"),
            "the report needs to read a bundle: {derive_line}"
        );
        assert!(
            !derive_line.contains("Deserialize"),
            "a proof must never be constructible from text: {derive_line}"
        );

        // And the behavioural half: the one door still refuses a rejection.
        let rejected = Verdict::Rejected {
            reason: "no".into(),
            verifier: "tdx-structural".into(),
        };
        assert!(ProofBundle::from_verdict(&rejected, None, "aa".into()).is_none());
    }

    /// SECURITY: the report file is the operator's alone.
    ///
    /// It names the VM, the machine type, the model served and the
    /// measurements of a machine that costs money to run. A failure means any
    /// account on the host reads it — and, worse in the other direction, that
    /// a file left behind with a wider mode keeps it, since `truncate` reuses
    /// the inode and its permissions.
    #[test]
    fn the_report_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cc").join("last-report.json");

        let written = write_report_to(&path, &serde_json::json!({"state": "standard"})).unwrap();
        assert_eq!(written, path, "the path written is the path returned");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");

        // A pre-existing, wider file is tightened rather than inherited.
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644)).unwrap();
        write_report_to(&path, &serde_json::json!({"state": "attested"})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an existing file kept its wider mode: {mode:o}"
        );

        // And what it wrote is what it can read back through the same door.
        let back =
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap();
        assert_eq!(back["state"], serde_json::json!("attested"));
    }

    // ── GET /attestation ──────────────────────────────────────────────────

    fn fresh_report(state: &str, when: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
        serde_json::json!({
            "verdict": "proven",
            "state": state,
            "label": "confidential",
            "limits": [LIMIT_PCS.sentence()],
            "measured_at": when.to_rfc3339(),
        })
    }

    /// SECURITY: no report means NOT confidential, and it must say so.
    ///
    /// The tempting answer is "unknown", and it is the wrong one:
    /// `x0ne-confidential` states the rule — absence of a claim is not a weak
    /// claim — and a grey indicator is read by an operator as "probably fine".
    /// A failure here means a machine that was never attested is displayed as
    /// something other than ordinary infrastructure.
    #[test]
    fn an_absent_report_is_not_confidential_not_grey() {
        let v = attestation_view(None, chrono::Utc::now());
        assert_eq!(v["state"], serde_json::json!("none"));
        assert_eq!(v["label"], serde_json::json!("confidential GPU disabled"));
        assert!(v["measured_at"].is_null());
        assert!(v["limits"].as_array().unwrap().is_empty());

        // A report with no readable timestamp cannot be aged, and a claim that
        // cannot be aged is not published.
        let undated = serde_json::json!({"state": "attested", "label": "confidential"});
        assert_eq!(
            attestation_view(Some(&undated), chrono::Utc::now())["state"],
            serde_json::json!("none"),
            "an undated report must not stand in for a measurement"
        );
    }

    /// SECURITY: an old proof does not describe the machine serving now.
    ///
    /// The VM can be stopped, recreated, or replaced by another at the same
    /// name, and nothing in the file would change. A failure means a console
    /// shows green for hardware that was destroyed this morning — the worst
    /// shape of this bug, because the claim was true when it was written.
    #[test]
    fn a_stale_report_is_not_confidential() {
        let now = chrono::Utc::now();
        let old = now
            - chrono::Duration::from_std(ATTESTATION_TTL).unwrap()
            - chrono::Duration::seconds(1);
        let v = attestation_view(Some(&fresh_report("attested", old)), now);
        assert_eq!(v["state"], serde_json::json!("standard"));
        assert_eq!(v["label"], serde_json::json!("confidential GPU disabled"));
        // The timestamp survives, so a caller can say HOW old.
        assert_eq!(v["measured_at"], serde_json::json!(old.to_rfc3339()));

        // Just inside the window, the same report is published as measured.
        let recent = now - chrono::Duration::seconds(30);
        let v = attestation_view(Some(&fresh_report("attested", recent)), now);
        assert_eq!(v["state"], serde_json::json!("attested"));
        assert_eq!(v["label"], serde_json::json!("confidential"));

        // SECURITY: a report stamped in the FUTURE is not fresh either. A
        // clock skew is honest and a forged timestamp is not, and neither may
        // buy a green badge that outlives the TTL.
        let future = now + chrono::Duration::hours(48);
        assert_eq!(
            attestation_view(Some(&fresh_report("attested", future)), now)["state"],
            serde_json::json!("standard")
        );
    }

    /// SECURITY: the route publishes a STATE, and derives the label from it.
    ///
    /// A front end handed `verified: true/false` computes the colour itself,
    /// and then the rule for what may be green lives in a browser. Worse, a
    /// forged `last-report.json` could pair a harmless state with a
    /// reassuring label, and a front end printing the file's own `label`
    /// would print "confidential" over `standard`. A failure means either the
    /// state is a boolean again, or the label is echoed rather than derived.
    #[test]
    fn the_attestation_json_exposes_state_not_verdict() {
        let now = chrono::Utc::now();

        // No boolean anywhere, and no raw verdict either.
        let v = attestation_view(Some(&fresh_report("attested", now)), now);
        for forbidden in ["verified", "ok", "verdict", "proven"] {
            assert!(
                v.get(forbidden).is_none(),
                "`{forbidden}` is published: {v}"
            );
        }
        assert!(v["state"].is_string());

        // The label is derived from the state, never taken from the file.
        let lying = serde_json::json!({
            "state": "standard",
            "label": "confidential",
            "measured_at": now.to_rfc3339(),
            "limits": [],
        });
        let v = attestation_view(Some(&lying), now);
        assert_eq!(v["state"], serde_json::json!("standard"));
        assert_eq!(
            v["label"],
            serde_json::json!("confidential GPU disabled"),
            "the file's own label was echoed: {v}"
        );

        // An unknown state token is not a state. It is `standard`, and it is
        // certainly not treated as the strongest thing it resembles.
        let bogus = serde_json::json!({
            "state": "attested-plus",
            "measured_at": now.to_rfc3339(),
            "limits": [],
        });
        assert_eq!(
            attestation_view(Some(&bogus), now)["state"],
            serde_json::json!("standard")
        );

        // And the three real states survive the round trip.
        for (wire, label) in [
            ("attested", "confidential"),
            ("cpu_only", "cpu enclave, gpu exposed"),
            ("standard", "confidential GPU disabled"),
        ] {
            let v = attestation_view(Some(&fresh_report(wire, now)), now);
            assert_eq!(v["state"], serde_json::json!(wire));
            assert_eq!(v["label"], serde_json::json!(label));
        }
    }

    /// A JSON report of a run that measured nothing still says what it is.
    ///
    /// A failure means an absent measurement is published as an absent FIELD,
    /// and a reader that treats "missing" as "fine" reads a prover run as a
    /// confidential one.
    #[test]
    fn the_prover_json_names_no_model_and_no_gpu() {
        let outcome = CcOutcome::Proven {
            proof: proven_cpu(),
            gpu: None,
            encryption: None,
        };
        let json = render_report_json(&outcome, 0.11, CcMachine::Prover, "running");
        assert_eq!(json["machine"], serde_json::json!("prover"));
        assert!(json["model"].is_null(), "the prover serves nothing");
        assert_eq!(
            json["state"],
            serde_json::json!("cpu_only"),
            "a proven CPU with no GPU is amber, never green"
        );
        assert!(json.get("gpu").is_none());
    }

    fn ready_smi() -> CmdOut {
        out("Thu Aug 14 09:12:33 2026\n\
             +----------------------------------------------------------------+\n\
             | NVIDIA-SMI 580.126.09    Driver Version: 580.126.09   CUDA: 12.8 |")
    }

    /// `nvidia-smi conf-compute -grs`, the two real answers (GCP page and
    /// NVIDIA/nvtrust#37, read on 2026-08-14).
    fn grs_ready() -> CmdOut {
        out("Confidential Compute GPUs Ready state: ready\n")
    }
    fn grs_not_ready() -> CmdOut {
        out("Confidential Compute GPUs Ready state: not-ready\n")
    }

    /// The GPU gets armed, and the step VERIFIES it instead of trusting an exit 0.
    ///
    /// Run 4, 2026-08-14, measured on the real VM: vLLM restarted 46 times on
    /// `cudaGetDeviceCount ... Error 802: system not yet initialized`, `-grs`
    /// answered `not-ready`, and an `-srs 1` made the 802 disappear. In
    /// confidential mode the GPU refuses CUDA until the guest has declared it
    /// ready. A failure of this test means the pipeline goes back into that
    /// crash-loop, on a machine billed 3.72 $/h.
    #[tokio::test]
    async fn a_disarmed_gpu_is_armed_and_the_arming_is_read_back() {
        let mut exec = ScriptedExec {
            responses: [grs_not_ready(), out(""), grs_ready()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::ArmGpu).await.unwrap();
        let sent: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        assert_eq!(
            exec.sent.len(),
            3,
            "sonde, armement, relecture: {:?}",
            exec.sent
        );
        assert!(sent.iter().any(|a| a.contains("-srs 1")), "{sent:?}");
        // The re-read is the third command, and that is the point: a
        // command that succeeded and a machine in the wanted state are two
        // different facts — the lesson of the driver after the reboot.
        assert!(
            exec.sent[2].iter().any(|a| a.contains("-grs")),
            "{:?}",
            exec.sent[2]
        );
    }

    /// The acquis: a GPU already armed is not re-armed.
    ///
    /// And that acquis is a LIVE probe, never an observation from
    /// `CcObservations`: the "ready" state is PER BOOT, it does not survive the
    /// reboot that `InstallGpuDriver` triggers, let alone a previous
    /// run. A failure here means paying one more ssh round-trip
    /// per run, which is the lesser evil — the serious error is in the other
    /// direction and the next test guards it.
    #[tokio::test]
    async fn an_already_armed_gpu_is_not_armed_again() {
        let mut exec = ScriptedExec {
            responses: [grs_ready()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::ArmGpu).await.unwrap();
        assert_eq!(
            exec.sent.len(),
            1,
            "one probe, nothing else: {:?}",
            exec.sent
        );
        let sent: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        assert!(
            !sent.iter().any(|a| a.contains("-srs")),
            "nothing is mutated: {sent:?}"
        );
    }

    /// SECURITY (fail-closed): an `-srs 1` that exits 0 without arming the GPU
    /// FAILS the step, and so does an unreadable answer.
    ///
    /// The two errors are asymmetric. A false "not armed" replays an
    /// idempotent `-srs 1`; a false "armed" hands the next 30 minutes and
    /// the ~15 GB of download to a GPU that will answer CUDA Error 802 to
    /// every one of them. So there is no path from an output we cannot
    /// read to a GPU deemed ready.
    #[tokio::test]
    async fn an_arming_that_did_not_take_fails_the_step_instead_of_being_assumed() {
        // It exits 0 and the GPU stays disarmed.
        let mut exec = ScriptedExec {
            responses: [grs_not_ready(), out(""), grs_not_ready()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ArmGpu)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("still reports the GPU as not-ready"), "{err}");
        assert!(
            err.contains("Error 802"),
            "say what that state costs: {err}"
        );

        // And an answer we cannot read is not an armed GPU.
        for hostile in [
            "Confidential Compute GPUs Ready state: perhaps",
            "Last login: Thu -- Confidential Compute GPUs Ready state: ready",
            "",
        ] {
            let mut exec = ScriptedExec {
                responses: [grs_not_ready(), out(""), out(hostile)].into(),
                ..Default::default()
            };
            let mut runner = CcRunner::new(&mut exec, full());
            assert!(
                runner.run_step(&CcStep::ArmGpu).await.is_err(),
                "{hostile:?}"
            );
        }
    }

    /// THE lock of the spec, GPU side, and the exact counterpart of
    /// `a_proof_bundle_cannot_exist_without_a_verified_verdict`: a proven
    /// CPU is not enough to write the combined verdict. Two forms, because
    /// a GPU can be missing in two ways — measured OFF, or not measured at
    /// all — and both must read "exposed".
    ///
    /// A failure here would mean `x0ne cc` can announce a confidential
    /// H100 while the GPU's memory is readable from the host,
    /// that is, sell exactly what it has not proven.
    #[test]
    fn a_proven_cpu_with_an_exposed_gpu_never_reads_as_fully_proven() {
        // This literal is only constructible because the test lives in the
        // same module — which is the point of the private fields. No caller
        // outside `cc_runner` can put a GPU verdict into a report.
        let measured_off = GpuEvidence {
            cc_mode_on: false,
            driver_version: "580.126.09".into(),
        };
        for gpu in [Some(measured_off), None] {
            let r = render_report(
                &CcOutcome::Proven {
                    proof: proven_cpu(),
                    gpu,
                    encryption: None,
                },
                0.40,
                CcMachine::FullH100,
                "running",
            );
            assert!(
                !r.contains("cpu tdx-structural + gpu"),
                "the combined verdict needs BOTH halves: {r}"
            );
            assert!(
                r.contains("cpu tdx-structural"),
                "the CPU half is real, say it: {r}"
            );
            assert!(
                r.contains("gpu        exposed"),
                "the GPU half must be named exposed: {r}"
            );
        }

        // And the same run with the GPU actually measured ON is the one shape
        // that earns the combined verdict — without this half, the assertions
        // above would also pass on a report that never prints it at all.
        let proven = GpuEvidence {
            cc_mode_on: true,
            driver_version: "580.126.09".into(),
        };
        let r = render_report(
            &CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: Some(proven),
                encryption: None,
            },
            0.40,
            CcMachine::FullH100,
            "running",
        );
        assert!(
            r.contains("ENCLAVE PROVEN (cpu tdx-structural + gpu cc-mode)"),
            "{r}"
        );
        assert!(r.contains("driver 580.126.09"), "{r}");
        // A report that claims two proofs owes two limits. The GPU one is a
        // different limit from Intel PCS — what was read is the driver's
        // answer about its own GPU, not an NVIDIA-signed quote — and a report
        // that states one and not the other reads as if the unqualified claim
        // were the stronger one (spec 2026-08-14).
        assert!(r.contains("Intel PCS"), "the CPU limit stays printed: {r}");
        assert!(r.contains("NRAS"), "the GPU limit must be printed too: {r}");
    }

    /// The runner path of the same property: the GPU answers OFF, the step
    /// fails NAMING "gpu exposed", and the failure's report carries the
    /// gpu line. That is what the operator sees; the VM is stopped by the
    /// existing failure path of `run_plan`.
    #[tokio::test]
    async fn a_gpu_that_answers_off_refuses_instead_of_forcing_the_mode() {
        let mut exec = ScriptedExec {
            responses: [out("CC status: OFF")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::CheckGpuCc)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("gpu exposed"),
            "the outcome must name it: {err}"
        );
        assert!(
            err.contains("conf-compute"),
            "name the command that said so: {err}"
        );
        assert!(
            err.contains("recreate the VM"),
            "name the only real fix: {err}"
        );
        // Not one command beyond the read: CC mode is never forced at runtime.
        assert_eq!(exec.sent.len(), 1, "a read is a read: {:?}", exec.sent);
        assert!(
            !exec.sent[0]
                .iter()
                .any(|a| a.contains("-srs") || a.contains("conf-compute -s")),
            "no mutation of the GPU may hide in this step: {:?}",
            exec.sent[0]
        );

        let r = render_report(
            &CcOutcome::Failed {
                step: CcStep::CheckGpuCc,
                error: err,
                proof: None,
                gpu: None,
                encryption: None,
            },
            0.40,
            CcMachine::FullH100,
            "stopped",
        );
        assert!(r.contains("gpu        exposed"), "{r}");
        assert!(!r.contains("cpu tdx-structural + gpu"), "{r}");
    }

    /// SECURITY (task 2 concern, pin 3): `Err` is not `false`. An unreadable
    /// output — GPU without the feature, nvidia-smi missing, truncated
    /// line — FAILS the step with the error. An `unwrap_or(false)`
    /// would report an "OFF" nobody measured; an `unwrap_or(true)`
    /// would serve a model on an unprotected GPU. A failure of this test
    /// means one of the two is back.
    #[tokio::test]
    async fn an_unreadable_cc_mode_fails_the_step_instead_of_becoming_a_verdict() {
        for hostile in [
            "GPU 0: not supported",          // no CC status line at all
            "CC status: ON\nCC status: OFF", // two lines that disagree
            "Last login: -- CC status: ON",  // the marker mid-line, valid value
        ] {
            let mut exec = ScriptedExec {
                responses: [out(hostile)].into(),
                ..Default::default()
            };
            let mut runner = CcRunner::new(&mut exec, full());
            let err = runner
                .run_step(&CcStep::CheckGpuCc)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("gpu exposed"),
                "unreadable is a refusal too: {err}"
            );

            // The same output through AttestGpu must not build evidence. The
            // script deliberately HAS a valid driver version waiting behind
            // the unreadable mode: without it the step would fail for lack of
            // a scripted answer, and the assertions below would hold for a
            // reason that has nothing to do with the property (caught by
            // mutation — `unwrap_or(false)` survived the first version of this
            // test, which is the exact defect the task-2 report warned about).
            let mut exec = ScriptedExec {
                responses: [out(hostile), out("580.126.09")].into(),
                ..Default::default()
            };
            let mut runner = CcRunner::new(&mut exec, full());
            assert!(
                runner.run_step(&CcStep::AttestGpu).await.is_err(),
                "{hostile}"
            );
            assert!(
                runner.gpu.is_none(),
                "no evidence may exist without a measurement"
            );
            assert_eq!(
                exec.sent.len(),
                1,
                "an unreadable mode stops the step there — the version is not even asked for"
            );
        }

        // The refusal quotes what the VM answered, so the VM decides how long
        // that quote is unless something bounds it. A refusal buried under a
        // wall of its own output is a refusal nobody reads.
        //
        // The fixture MUST carry a real `CC status:` prefix (review round 1,
        // finding I2). Without it the parser stops at "no 'CC status:' line",
        // a short constant, and this assertion passes on a message that was
        // never built from remote text — the same shape as the M5 survivor one
        // site over. With the prefix, 50 000 characters travel through
        // `ObsParseError::detail` and both bounds are actually exercised: the
        // parser's excerpt AND the runner's echo.
        let hostile_value = format!("CC status: {}", "A".repeat(50_000));
        let mut exec = ScriptedExec {
            responses: [out(&hostile_value)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::CheckGpuCc)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.chars().count() < 600,
            "the refusal must stay on screen, got {} chars",
            err.chars().count()
        );
        assert!(
            err.contains("gpu exposed"),
            "and the verdict must still be in it: {err}"
        );
    }

    /// SECURITY (finding I1, review round 1): an unreadable `boot_id` fails
    /// the step BEFORE the reboot.
    ///
    /// The guard is load-bearing and had no test: with an empty `before`,
    /// `now != before` is true for any answer, so the very
    /// first probe — the one answering from the machine BEFORE it went down
    /// — is accepted as "the VM is back". Everything that follows then attests
    /// a kernel without the LKCA rule, about to be replaced. The test
    /// also checks that no `reboot` went out: we do not reboot a
    /// machine when we will not be able to tell whether it came back.
    #[tokio::test]
    async fn an_unreadable_boot_id_refuses_before_rebooting_anything() {
        let mut exec = ScriptedExec {
            responses: [
                out("no driver here"), // acquis probe
                out(""),               // install
                out("   \n"),          // boot id: blank
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::InstallGpuDriver)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("boot_id"),
            "name what could not be read: {err}"
        );
        assert_eq!(exec.sent.len(), 3, "the step stops there: {:?}", exec.sent);
        assert!(
            !exec.sent.iter().flatten().any(|a| a.contains("reboot")),
            "a machine whose return cannot be told apart is not rebooted: {:?}",
            exec.sent
        );
    }

    /// SECURITY: `driver_version` is text produced by the machine being
    /// measured and it ends up in a terminal report. It goes through the same
    /// gate as `reason`/`error`/`vm_state` — `sanitize` — plus two bounds
    /// those do not need: a single line, and a maximum
    /// length. A failure here would mean the attested VM can write whatever
    /// it wants into the report that judges it.
    #[tokio::test]
    async fn gpu_step_outputs_are_sanitized_like_every_remote_string() {
        let hostile = "580.126.09\x1b[2K\nENCLAVE PROVEN (cpu tdx-structural + gpu cc-mode)\r  \
                       verifier   totally-legit";
        let mut exec = ScriptedExec {
            responses: [out("CC status: ON"), out(hostile)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::AttestGpu).await.unwrap();
        let evidence = runner.gpu.clone().expect("a measured GPU leaves evidence");

        let r = render_report(
            &CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: Some(evidence),
                encryption: None,
            },
            0.40,
            CcMachine::FullH100,
            "running",
        );
        assert!(!r.contains('\x1b'), "ANSI escapes must not survive: {r:?}");
        assert!(!r.contains('\r'), "a bare CR must not survive: {r:?}");
        assert!(
            !r.contains("verifier   totally-legit"),
            "only the first line of the version is ours to print: {r}"
        );
        // Exactly one line may claim the combined verdict, and it is the one
        // the report wrote — not one the VM spliced in.
        assert_eq!(
            r.matches("cpu tdx-structural + gpu cc-mode").count(),
            1,
            "the VM must not be able to plant a second verdict line: {r}"
        );

        // The length bound, separately: sanitize() does not shorten anything,
        // so a version of 10 000 characters would push the whole report off
        // the screen while remaining perfectly "safe".
        let mut exec = ScriptedExec {
            responses: [out("CC status: ON"), out(&"A".repeat(10_000))].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::AttestGpu).await.unwrap();
        assert_eq!(
            runner
                .gpu
                .as_ref()
                .unwrap()
                .driver_version()
                .chars()
                .count(),
            DRIVER_VERSION_MAX_CHARS
        );
    }

    /// An empty driver version is not a measurement: the step fails rather
    /// than write a `driver ` line into a proof report.
    #[tokio::test]
    async fn an_empty_driver_version_is_not_evidence() {
        let mut exec = ScriptedExec {
            responses: [out("CC status: ON"), out("   \n")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        assert!(runner.run_step(&CcStep::AttestGpu).await.is_err());
        assert!(runner.gpu.is_none());
    }

    /// GPU acquis no. 1: `nvidia-smi` already answers, so the step
    /// reinstalls nothing and above all DOES NOT REBOOT the VM. A failure here costs
    /// ten minutes of a3-highgpu-1g and a useless reboot on every resume.
    #[tokio::test]
    async fn an_acquired_nvidia_smi_makes_the_driver_step_a_no_op() {
        let mut exec = ScriptedExec {
            responses: [ready_smi()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::InstallGpuDriver).await.unwrap();
        assert_eq!(
            exec.sent.len(),
            1,
            "one probe, nothing else: {:?}",
            exec.sent
        );
        assert!(
            !exec
                .sent
                .iter()
                .flatten()
                .any(|a| a.contains("apt-get") || a.contains("reboot")),
            "nothing may be installed or rebooted on an acquired driver: {:?}",
            exec.sent
        );
    }

    /// nvidia-smi's classic failure message CONTAINS "NVIDIA-SMI".
    /// So the step must install, and the test proves it by counting the
    /// commands. That is the trap `parse_nvidia_smi_ready` closes, checked
    /// here from the runner side.
    #[tokio::test]
    async fn a_driver_that_only_prints_its_own_failure_is_installed() {
        let responses = [
            out("NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver"),
            out(""),       // install
            out("boot-a"), // boot id before
            out(""),       // reboot
            out("boot-b"), // first probe after the reboot: new boot
            ready_smi(),   // verification
        ];
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::InstallGpuDriver).await.unwrap();
        let all: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        assert!(
            all.iter().any(|a| a.contains(GPU_DRIVER_PACKAGES)),
            "{all:?}"
        );
        assert!(
            all.iter().any(|a| a.contains("nvidia-lkca.conf")),
            "LKCA is the point: {all:?}"
        );
        assert!(
            all.iter().any(|a| a.contains("update-initramfs")),
            "{all:?}"
        );
        assert!(all.iter().any(|a| a.contains("reboot")), "{all:?}");
    }

    /// SECURITY-adjacent: the step accepts the VM as back only on a
    /// DIFFERENT boot_id. Here the machine answers perfectly well — with its
    /// old boot_id, that is, without having rebooted: the LKCA rule is
    /// not loaded. A failure of this test would mean we attest a kernel
    /// that is about to be replaced, which a `sleep` or a plain
    /// `wait_ssh` would not distinguish.
    #[tokio::test]
    async fn the_driver_step_only_trusts_a_boot_that_is_not_the_one_it_rebooted() {
        let mut responses = vec![
            out("no driver here"), // probe
            out(""),               // install
            out("boot-a"),         // boot id before
            out(""),               // reboot
        ];
        // Every probe answers, promptly, with the SAME boot id.
        responses.extend((0..SSH_RETRIES).map(|_| out("boot-a")));
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::InstallGpuDriver)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not come back"), "{err}");
        // "14 min" is hand-computed (27 × (20 + 10) = 810 s, rounded UP), not
        // a recomputation of the code's own division — the trap the WaitSsh
        // message fell into. It must NOT be the 15 min of `wait_ssh_budget_
        // secs()`: that figure includes the 90 s first probe, which is work
        // `WaitSsh` already did and this loop never repeats (minor M-a).
        assert!(
            err.contains("up to 14 min"),
            "state this loop's own ceiling: {err}"
        );
        assert_eq!(
            exec.sent.len(),
            4 + SSH_RETRIES,
            "the wait must run exactly SSH_RETRIES probes: {:?}",
            exec.sent.len()
        );
        // The pause between probes is keyed on this label in the production
        // exec; without it the whole wait burns in under a second.
        assert!(
            exec.displays[4..].iter().all(|d| d == WAIT_SSH_DISPLAY),
            "the reboot wait must carry the label the pause keys on: {:?}",
            exec.displays
        );
    }

    /// The driver installs, the VM reboots, and `nvidia-smi` still does not
    /// answer: the step fails. An `apt-get` that exited 0 and a driver that
    /// answers are two different facts, and only the second serves the
    /// following steps.
    #[tokio::test]
    async fn an_install_that_exits_zero_without_a_working_driver_still_fails() {
        let responses = [
            out("no driver here"),
            out(""),
            out("boot-a"),
            out(""),
            out("boot-b"),
            out("NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver"),
        ];
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::InstallGpuDriver)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not report a loaded driver"), "{err}");
    }

    /// THE test of resuming after a SPOT preemption, runner side (pin 1).
    ///
    /// A `Stopped` VM whose disk already carries the driver: the preflight
    /// cannot know about it (no ssh on a stopped VM), so the approved
    /// plan contains the install. Once SSH is back, the re-probe
    /// removes it. A failure here would mean the acquis flag exists, is
    /// documented, is read — and changes nothing: the fifth dead scaffolding
    /// of this repo, on the 3.72 $/h machine.
    #[tokio::test]
    async fn a_stopped_vm_that_already_has_its_driver_does_not_reinstall_it() {
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("example-project-123456\n"),
                out(r#"[{"name":"img"}]"#),
                out(
                    r#"[{"name":"x0ne-cc","status":"TERMINATED","zone":"http://x/zones/us-central1-a"}]"#,
                ),
                // The four in-VM probes, once ssh answers.
                ready_smi(),
                out("CC status: ON"),
                out("curl: (7) Failed to connect"), // no vLLM yet
                CmdOut { stdout: String::new(), stderr: "x0ne: not found".into(), exit_code: 127 },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let obs = runner.observe().await.unwrap();
        assert_eq!(
            obs.gpu_driver_ready, None,
            "a stopped VM cannot be probed from outside"
        );
        let approved = build_plan(&full(), &obs).unwrap();
        assert_eq!(approved[0], CcStep::StartVm);
        assert!(
            approved.contains(&CcStep::InstallGpuDriver),
            "the approved plan does it all"
        );

        // StartVm, WaitSsh and AttestTdx have run — the re-probe happens once
        // the enclave is proven (finding I3) — so this is what remains of the
        // approval at the moment the caller asks.
        assert_eq!(approved[2], CcStep::AttestTdx);
        let tail = &approved[3..];
        let replanned = runner.replan_after_ssh(tail).await;

        assert!(
            !replanned.contains(&CcStep::InstallGpuDriver),
            "an installed driver must not be installed again: {replanned:?}"
        );
        assert!(
            !replanned.contains(&CcStep::CheckGpuCc),
            "a mode already read as ON is an acquis too: {replanned:?}"
        );
        assert!(
            replanned.contains(&CcStep::AttestGpu),
            "a PROOF is replayed on every pass, never inherited: {replanned:?}"
        );
        assert!(
            replanned.contains(&CcStep::InstallVllm),
            "vLLM is NOT up, so its install stays: {replanned:?}"
        );
        assert!(
            is_subsequence(&replanned, tail),
            "{replanned:?} vs {tail:?}"
        );
    }

    /// SECURITY: the re-probe can only REMOVE steps. It runs on
    /// observations produced by the VM itself; if it could add
    /// some, a compromised machine would write steps into a plan that a
    /// human approved by reading it (repo invariant 5). Here the approved plan
    /// is artificially short, so the re-planning proposes
    /// steps that are not in it: it must be rejected wholesale.
    #[tokio::test]
    async fn the_replan_can_only_remove_steps_never_add_one() {
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("p\n"),
                out(r#"[{"name":"img"}]"#),
                out(r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"http://x/zones/us-central1-a"}]"#),
                out("no driver"),
                out("nothing"),
                out("nothing"),
                CmdOut { stdout: String::new(), stderr: String::new(), exit_code: 127 },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.observe().await.unwrap();
        let approved = vec![CcStep::AttestGpu, CcStep::Report];
        assert_eq!(
            runner.replan_after_ssh(&approved).await,
            approved,
            "a re-plan that is not a subsequence of the approval is discarded whole"
        );

        // And the pure predicate underneath, on the three shapes that matter.
        let big = [
            CcStep::AttestTdx,
            CcStep::InstallGpuDriver,
            CcStep::AttestGpu,
        ];
        assert!(is_subsequence(
            &[CcStep::AttestTdx, CcStep::AttestGpu],
            &big
        ));
        assert!(is_subsequence(&[], &big));
        assert!(
            !is_subsequence(&[CcStep::AttestGpu, CcStep::AttestTdx], &big),
            "order counts"
        );
        assert!(
            !is_subsequence(&[CcStep::Report], &big),
            "an unapproved step is refused"
        );
    }

    /// The "prover" machine has no GPU: sending it four in-VM probes
    /// would cost four ssh round-trips to answer a question that
    /// its plan does not ask.
    #[tokio::test]
    async fn the_prover_machine_is_not_probed_for_a_gpu_it_does_not_have() {
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut exec = ScriptedExec::default();
        let mut runner = CcRunner::new(&mut exec, cfg);
        let approved = vec![CcStep::AttestTdx, CcStep::Report];
        assert_eq!(runner.replan_after_ssh(&approved).await, approved);
        assert!(exec.sent.is_empty(), "not one probe: {:?}", exec.sent);
    }

    /// The driver is pinned, and pinned on the ONLY form a confidential
    /// GPU accepts: the open kernel module, 580 or later (GCP
    /// procedure read on 2026-08-14). A failure of this test means we install a
    /// driver that cannot enter CC mode — the next step would refuse,
    /// after ten billed minutes of installation.
    #[test]
    fn the_pinned_driver_is_an_open_kernel_module() {
        assert!(
            GPU_DRIVER_PACKAGES.contains("-open"),
            "{GPU_DRIVER_PACKAGES}"
        );
        assert!(GPU_DRIVER_PACKAGES.contains("580"), "{GPU_DRIVER_PACKAGES}");
        assert!(
            GPU_DRIVER_INSTALL_SCRIPT.contains(GPU_DRIVER_PACKAGES),
            "the script must install the pinned packages and no others"
        );
        assert!(
            !GPU_DRIVER_INSTALL_SCRIPT.contains("ubuntu-drivers"),
            "no unpinned fallback: it may install the CLOSED module, which cannot do CC"
        );
    }

    /// The VM is created with `--shielded-secure-boot`, so the kernel refuses any
    /// unsigned module. The generic DKMS package builds one, signed by
    /// nobody: measured on the real H100 on 2026-08-14 (run 3), apt exited 0
    /// and `nvidia-smi` exited 9 on BOTH sides of a proven reboot. A failure of
    /// this test means we are back to that silence: ten billed minutes
    /// of installation for a module the kernel throws away.
    #[test]
    fn the_kernel_module_is_the_one_canonical_signed_for_the_gcp_flavour() {
        const SIGNED: &str = "linux-modules-nvidia-580-server-open-gcp";
        assert!(
            GPU_DRIVER_PACKAGES.contains(SIGNED),
            "{GPU_DRIVER_PACKAGES}"
        );
        assert!(
            GPU_DRIVER_INSTALL_SCRIPT.contains(SIGNED),
            "without the pre-signed package, Secure Boot rejects the module and nvidia-smi exits 9"
        );
        // The flavour matters as much as the branch: the signed module is built
        // for a specific kernel, and ours is `-gcp`. A `-generic` package
        // would install without error and never load.
        assert!(SIGNED.ends_with("-gcp"), "the GCE kernel flavour is `gcp`");
        // The new kernel the module pulls in has its initramfs built by apt,
        // BEFORE the script writes the LKCA rule. Only `-k all` puts it in
        // the image the VM will reboot on.
        assert!(
            GPU_DRIVER_INSTALL_SCRIPT.contains("update-initramfs -u -k all"),
            "the LKCA rule must reach the initramfs of every installed kernel"
        );
    }

    // ── vLLM, tmpfs and proven encryption (task 4, 2026-08-14) ───────

    /// `ss -tlnp` on the VM as we want it. The port comes from `VLLM_PORT`,
    /// never from a literal: a fixture hardcoding 8000 would keep
    /// passing the day the constant changes, testing a port the code
    /// no longer uses.
    fn ss_loopback() -> CmdOut {
        out(&format!(
            "State  Recv-Q Send-Q   Local Address:Port    Peer Address:Port Process\n\
             LISTEN 0      128          127.0.0.1:{VLLM_PORT}         0.0.0.0:*     \
             users:((\"vllm\",pid=2011,fd=5))\n\
             LISTEN 0      128            0.0.0.0:22             0.0.0.0:*     \
             users:((\"sshd\",pid=901,fd=3))\n"
        ))
    }

    /// The same output, but vLLM listens on all interfaces — the default
    /// of `vllm serve`, and the thing the in-VM checks exist to catch.
    fn ss_all_interfaces() -> CmdOut {
        out(&format!(
            "State  Recv-Q Send-Q   Local Address:Port    Peer Address:Port Process\n\
             LISTEN 0      128            0.0.0.0:{VLLM_PORT}         0.0.0.0:*     \
             users:((\"vllm\",pid=2011,fd=5))\n"
        ))
    }

    /// The tmpfs probe as `fetch_model_script` produces it: logind
    /// disarmed, then the free space.
    fn df_roomy() -> CmdOut {
        out(&format!(
            "{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n{TMPFS_PROBE_MARK_AVAIL}\n     Avail\n 122683392\n"
        ))
    }

    /// A vLLM wait probe, as `vllm_wait_probe_cmd` produces it:
    /// the three sections, in order, with the shared markers.
    ///
    /// Built from the marker constants, never from
    /// literals: a fixture writing its own markers would keep
    /// passing the day the command changes its own, testing a format
    /// the VM no longer produces.
    fn wait_probe(listen: &str, restarts: u64, kib: u64) -> CmdOut {
        out(&format!(
            "{VLLM_PROBE_MARK_LISTEN}\n{listen}\
             {VLLM_PROBE_MARK_RESTARTS}\nNRestarts={restarts}\n\
             {VLLM_PROBE_MARK_MODELDIR}\n{kib}\t{MODEL_DIR}\n"
        ))
    }

    /// Nothing listens yet: `ss` answers perfectly, without our port.
    fn wait_nothing_bound(restarts: u64, kib: u64) -> CmdOut {
        wait_probe(
            "State  Recv-Q Send-Q Local Address:Port\n\
             LISTEN 0      128            0.0.0.0:22             0.0.0.0:*\n",
            restarts,
            kib,
        )
    }

    /// The server listens, on the loopback.
    fn wait_bound() -> CmdOut {
        wait_probe(
            &format!(
                "State  Recv-Q Send-Q Local Address:Port\n\
                 LISTEN 0      128          127.0.0.1:{VLLM_PORT}         0.0.0.0:*     \
                 users:((\"vllm\",pid=2011,fd=5))\n"
            ),
            0,
            15 * 1024 * 1024,
        )
    }

    fn disk_ok() -> CmdOut {
        out(r#"{"name":"x0ne-cc"}"#)
    }

    fn models_serving() -> CmdOut {
        out(&format!(
            r#"{{"object":"list","data":[{{"id":"{VLLM_MODEL}","object":"model","owned_by":"vllm"}}]}}"#
        ))
    }

    /// The answers of the full green path, InstallVllm → HealthCheck.
    fn green_vllm_script() -> Vec<CmdOut> {
        vec![
            // InstallVllm: the pinned version is not there, we install, we verify.
            CmdOut {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
            },
            out(""),
            out(""),
            // FetchModel: the tmpfs has room.
            df_roomy(),
            // ServeVllm: nothing listens, we write the unit, it ends up listening.
            out(""),
            out(""),
            wait_bound(),
            // ValidateEncryption: no swap, loopback, empty firewall, disk is ours.
            out(""),
            ss_loopback(),
            out("[]"),
            disk_ok(),
            // HealthCheck.
            models_serving(),
        ]
    }

    const VLLM_STEPS: [CcStep; 5] = [
        CcStep::InstallVllm,
        CcStep::FetchModel,
        CcStep::ServeVllm,
        CcStep::ValidateEncryption,
        CcStep::HealthCheck,
    ];

    /// The green path, end to end: pinned vLLM, weights in tmpfs, systemd
    /// unit on 127.0.0.1, and the four in-VM check lines in the report.
    ///
    /// This test pins what the machine REALLY receives — the commands, not
    /// a summary. A failure means one of the four promises displayed
    /// in the plan (encrypted RAM, loopback port, pinned version, named
    /// model) is no longer the one that goes out on the wire.
    #[tokio::test]
    async fn the_full_vllm_path_serves_from_tmpfs_on_loopback_only() {
        let mut exec = ScriptedExec {
            responses: green_vllm_script().into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        for step in &VLLM_STEPS {
            runner
                .run_step(step)
                .await
                .unwrap_or_else(|e| panic!("{step:?}: {e}"));
        }
        let checks = runner
            .encryption
            .clone()
            .expect("a validated run leaves its checks");
        assert!(checks.all_pass(), "{checks:?}");

        let all: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        let joined = all.join(" ");
        // The weights live in RAM encrypted by TDX, and
        // the server's environment points to it. Without HF_HOME, vLLM writes
        // into ~/.cache/huggingface — on the disk, which the host can read.
        assert!(
            joined.contains(MODEL_DIR),
            "the tmpfs must be prepared: {joined}"
        );
        // Run 5: what protects the weights must go out ON THE WIRE, not just
        // exist in a constant — and go out before the step that downloads.
        assert!(
            joined.contains("RemoveIPC=no"),
            "logind purgerait /dev/shm: {joined}"
        );
        // The ORDER is read in what WENT OUT, not in the step array:
        // a well-ordered plan that sent the drop-in after the unit would
        // still download 15 GB into a directory that can still be purged. The
        // two markers are the two real commands — the drop-in, then the
        // unit's HF_HOME, which is the moment the download starts.
        let dropin_at = all.iter().position(|c| c.contains(LOGIND_DROPIN));
        let download_at = all
            .iter()
            .position(|c| c.contains(&format!("HF_HOME={MODEL_DIR}")));
        assert!(dropin_at.is_some() && download_at.is_some(), "{all:?}");
        assert!(
            dropin_at < download_at,
            "the purge is disarmed BEFORE the first byte: {all:?}"
        );
        assert!(
            joined.contains(&format!("Environment=HF_HOME={MODEL_DIR}")),
            "the unit must point HF_HOME at the tmpfs: {joined}"
        );
        // The unit's port is THE ONE the validation reads. This assertion
        // is built from the constant: if the script carried a literal 8000,
        // it would fail at the first change of `VLLM_PORT`.
        assert!(
            joined.contains(&format!("--host 127.0.0.1 --port {VLLM_PORT}")),
            "the unit must bind loopback on THE port: {joined}"
        );
        assert!(
            !joined.contains("--host 0.0.0.0"),
            "vLLM's own default is 0.0.0.0 and it must never be what we write: {joined}"
        );
        assert!(
            joined.contains(VLLM_MODEL),
            "the model is a local constant: {joined}"
        );

        let r = render_report(
            &CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: None,
                encryption: Some(checks),
            },
            0.40,
            CcMachine::FullH100,
            "running",
        );
        // Every check is one line of the report.
        assert!(r.contains("  swap       off"), "{r}");
        assert!(
            r.contains(&format!("  bind       127.0.0.1:{VLLM_PORT} only")),
            "{r}"
        );
        assert!(r.contains("  firewall   no INGRESS rule opens"), "{r}");
        assert!(r.contains("  disk       encrypted at rest by GCP"), "{r}");
        // And the disk line says what it does not prove. That is
        // the disk-honesty rule: "encrypted" without that sentence reads as a
        // protection against the host, which encryption at rest is not.
        assert!(
            r.contains("NOT the VM against"),
            "the disk line must state its limit: {r}"
        );
    }

    /// SECURITY: active swap REFUSES the machine.
    ///
    /// It is the most discreet leak of the lot: the weights are in RAM encrypted
    /// by TDX, and the kernel can write pages of it to the boot disk,
    /// in the clear, without anything signalling it. A failure of this test means
    /// a model can be served on a machine that copies its memory onto
    /// a disk the host reads.
    #[tokio::test]
    async fn an_active_swap_refuses_the_machine_and_says_so() {
        let swapon_table =
            out("NAME      TYPE      SIZE USED PRIO\n/swapfile file        2G   0B   -2\n");
        let mut exec = ScriptedExec {
            responses: [swapon_table, ss_loopback(), out("[]"), disk_ok()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("REFUSED"),
            "a failed check is a refusal, not a fault: {err}"
        );
        assert!(
            err.contains("swap is ACTIVE"),
            "name the check that failed: {err}"
        );
        assert!(err.contains("swapoff"), "name the lever: {err}");
        // The failure's report carries the failure, and the measurements stay readable.
        let checks = runner
            .encryption
            .clone()
            .expect("what was measured is kept");
        assert!(!checks.swap_off(), "stored as measured");
        assert!(checks.localhost_only(), "the other checks are still read");
    }

    /// SECURITY: a server bound to 0.0.0.0 is refused.
    ///
    /// It is the default of `vllm serve`, and on a GCP VM it means an
    /// unauthenticated 7B reachable from the public IP. A failure of this
    /// test means `x0ne cc` can finish green on that machine.
    #[tokio::test]
    async fn a_server_bound_to_every_interface_is_refused() {
        let mut exec = ScriptedExec {
            responses: [out(""), ss_all_interfaces(), out("[]"), disk_ok()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("loopback"), "name what is wrong: {err}");
        assert!(
            runner
                .encryption
                .as_ref()
                .is_some_and(|c| !c.localhost_only()),
            "the measurement is kept as measured"
        );
    }

    /// SECURITY: a firewall rule that opens the port refuses,
    /// and it is read on the HOST side.
    ///
    /// The port is on loopback, so the rule alone is not a breach —
    /// it is the second lock, and the pipeline refuses when it is open. The
    /// read goes through gcloud and not ssh: asking the VM which firewall
    /// protects it is asking the accused to produce its alibi.
    #[tokio::test]
    async fn a_firewall_rule_that_opens_the_port_refuses_and_is_read_from_the_host() {
        let open_rule = out(&format!(
            r#"[{{"name":"open-vllm","allowed":[{{"IPProtocol":"tcp","ports":["{VLLM_PORT}"]}}],"direction":"INGRESS"}}]"#
        ));
        let mut exec = ScriptedExec {
            responses: [out(""), ss_loopback(), open_rule, disk_ok()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("INGRESS"), "{err}");
        // The firewall command is a gcloud, not an ssh: no `compute
        // ssh` carries `firewall-rules`.
        let fw_call = exec
            .sent
            .iter()
            .find(|argv| argv.iter().any(|a| a == "firewall-rules"))
            .expect("the firewall must be read at all");
        assert!(
            !fw_call.iter().any(|a| a == "ssh"),
            "the firewall is read from the host, never through the VM: {fw_call:?}"
        );
        // And the json projection is projected: bare `--format=json` is
        // truncatable (measured 2026-08-13) and the parser would read "nothing opens".
        assert!(
            fw_call.iter().any(|a| a.starts_with("--format=json(")),
            "a bare --format=json is truncatable: {fw_call:?}"
        );
    }

    /// SECURITY: `Err` is not `false`, on the three reads of the in-VM checks that
    /// can fail.
    ///
    /// An unreadable output never becomes the reassuring half of a
    /// verdict: it fails the step. An `unwrap_or(true)` on the socket
    /// read would serve an unauthenticated 7B on a public IP, which
    /// is the worst thing this program could do; an `unwrap_or(false)`
    /// would refuse a healthy machine saying it is breached. A failure of
    /// this test means one of the two is back.
    #[tokio::test]
    async fn unreadable_encryption_answers_fail_the_step_instead_of_becoming_verdicts() {
        // 1. `ss` shows no LISTEN line for the port: nothing serves.
        let mut exec = ScriptedExec {
            responses: [
                out(""),
                out("State Recv-Q Send-Q Local Address:Port\n"),
                out("[]"),
                disk_ok(),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("cannot tell where"), "{err}");
        assert!(
            runner.encryption.is_none(),
            "no checks may exist without a measurement"
        );

        // 2. The firewall answers something we cannot read.
        let mut exec = ScriptedExec {
            responses: [out(""), ss_loopback(), out("{\"rules\":[]}"), disk_ok()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("firewall"), "{err}");
        assert!(runner.encryption.is_none());

        // 3. The `describe` talks about another disk. An answer about the
        // neighbour's disk is not an answer about ours.
        let mut exec = ScriptedExec {
            responses: [
                out(""),
                ss_loopback(),
                out("[]"),
                out(r#"{"name":"someone-else"}"#),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("boot disk"), "{err}");
        assert!(runner.encryption.is_none());
    }

    /// Health that never comes: failure after EXACTLY
    /// `VLLM_HEALTH_RETRIES` probes, proven by counting.
    ///
    /// Same pattern as `wait_ssh_gives_up_with_a_named_next_step`, and for the
    /// same reason: without the counting, a loop that gives up at the first
    /// attempt or runs forever would pass this test. And the assertion on
    /// the label is what guarantees the pause on the exec side — without it the six
    /// probes go out within one second, well before a server could answer.
    #[tokio::test]
    async fn a_health_check_that_never_goes_green_gives_up_after_exactly_n_probes() {
        let fails: Vec<CmdOut> = (0..VLLM_HEALTH_RETRIES)
            .map(|_| CmdOut {
                stdout: String::new(),
                stderr: "curl: (7) Failed to connect".into(),
                exit_code: 7,
            })
            .collect();
        let mut exec = ScriptedExec {
            responses: fails.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::HealthCheck)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            exec.sent.len(),
            VLLM_HEALTH_RETRIES,
            "exactly VLLM_HEALTH_RETRIES probes, no more and no fewer"
        );
        assert!(err.contains("journalctl"), "name what to read next: {err}");
        // 6 × (30 + 20) = 300 s, rounded up to the minute: 5 min. Computed
        // by hand, not by replaying the code's division — the trap the
        // `WaitSsh` message had fallen into.
        assert!(
            err.contains("up to 5 min"),
            "state this loop's own ceiling: {err}"
        );
        assert!(
            exec.displays.iter().all(|d| d == WAIT_VLLM_DISPLAY),
            "every probe must carry the label the pause keys on: {:?}",
            exec.displays
        );
    }

    /// A server that answers but serves SOMETHING ELSE is not healthy.
    ///
    /// The socket says nothing about what is behind it, and the plan a
    /// human approved names a model. Serving another one is a state to
    /// report, not to validate — and the message must say what was served,
    /// bounded, because it is text the machine chooses.
    #[tokio::test]
    async fn a_server_that_serves_another_model_is_not_healthy() {
        let other = out(r#"{"object":"list","data":[{"id":"some/other-model","object":"model"}]}"#);
        let responses: Vec<CmdOut> = (0..VLLM_HEALTH_RETRIES)
            .map(|_| other.stdout.clone())
            .map(|s| out(&s))
            .collect();
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::HealthCheck)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(VLLM_MODEL), "say what was expected: {err}");
        assert!(
            err.contains("some/other-model"),
            "say what was served: {err}"
        );

        // And the quoted remote text is bounded: the machine does not decide the
        // length of the message by which it is refused.
        let huge = format!(
            r#"{{"object":"list","data":[{{"id":"{}","object":"model"}}]}}"#,
            "A".repeat(50_000)
        );
        let responses: Vec<CmdOut> = (0..VLLM_HEALTH_RETRIES).map(|_| out(&huge)).collect();
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::HealthCheck)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.chars().count() < 600,
            "the refusal must stay on screen, got {} chars",
            err.chars().count()
        );

        // And an unreadable answer never becomes "healthy". The server exits
        // 0 and writes anything: the loop waits, then refuses. An
        // `unwrap_or` here would declare served a model nobody read.
        let responses: Vec<CmdOut> = (0..VLLM_HEALTH_RETRIES)
            .map(|_| out("<html>502 Bad Gateway</html>"))
            .collect();
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        assert!(
            runner.run_step(&CcStep::HealthCheck).await.is_err(),
            "an unreadable answer is not a served model"
        );
        assert_eq!(
            exec.sent.len(),
            VLLM_HEALTH_RETRIES,
            "and it waited, rather than deciding"
        );
    }

    /// The server that never binds: same property, on the loop that
    /// carries the REAL budget — the one waiting for the 15 GB of weights.
    ///
    /// A failure here means a 3.72 $/h VM can wait indefinitely for a
    /// server that will not come, without the budget gate — which only runs
    /// between steps — having a say.
    #[tokio::test]
    async fn a_vllm_that_never_binds_gives_up_after_exactly_n_probes() {
        let mut responses = vec![
            out(""), // acquis: nothing listens
            out(""), // unit write
        ];
        // The server is working — the download progresses, nothing
        // restarts — but nothing ever listens. It is the only case still
        // allowed to consume the 36 probes since run 4: the two others
        // (crash-loop, empty tmpfs) stop well before, and that is the subject of the
        // next two tests.
        responses.extend(
            (0..VLLM_START_RETRIES).map(|i| wait_nothing_bound(0, (i as u64 + 1) * 512 * 1024)),
        );
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ServeVllm)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            exec.sent.len(),
            2 + VLLM_START_RETRIES,
            "the wait must run exactly VLLM_START_RETRIES probes"
        );
        // 36 × (30 + 20) = 1800 s: 30 min, computed by hand.
        assert!(
            err.contains("up to 30 min"),
            "state the ceiling to whoever pays it: {err}"
        );
        assert!(err.contains("15 GB"), "say what the wait is FOR: {err}");
        assert!(
            exec.displays[2..]
                .iter()
                .all(|d| d.starts_with(WAIT_VLLM_DISPLAY)),
            "the bind wait must carry the label the pause keys on: {:?}",
            exec.displays
        );
        // The patience is JUSTIFIED and DISPLAYED (owner's requirement,
        // run 4): from the moment the download exceeds the floor,
        // the label carries the figure. A failure here means we again ask
        // the operator to wait half an hour without showing them that something
        // is arriving.
        assert!(
            exec.displays.iter().any(|d| d.contains("G/15G downloaded")),
            "the wait must show what has landed: {:?}",
            exec.displays
        );
    }

    /// SECURITY (money): a service restarting in a loop is named within
    /// TWO probes, not thirty minutes.
    ///
    /// It is the measurement of run 4 (2026-08-14): 30 minutes and 1.26 $ spent
    /// probing a port while `x0ne-vllm` restarted 46 times on CUDA
    /// Error 802, and a final report that blamed a slow download. The
    /// restart counter is what distinguishes "it is working" from "it is
    /// dying"; the port cannot, it is silent in both cases.
    ///
    /// A failure of this test means the loop has gone blind again — it
    /// does not cost a false verdict, it costs billed time.
    #[tokio::test]
    async fn a_crash_looping_unit_is_named_in_two_probes_with_its_journal() {
        let mut exec = ScriptedExec {
            responses: [
                out(""),                  // acquis: nothing listens
                out(""),                  // unit write
                wait_nothing_bound(3, 0), // 1st probe: 3 restarts
                wait_nothing_bound(4, 0), // 2nd: 4 — it is rising
                out("cudaGetDeviceCount ... Error 802: system not yet initialized"),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ServeVllm)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            exec.sent.len(),
            5,
            "baseline, unit, two probes, the journal — and nothing else: {:?}",
            exec.sent
        );
        assert!(
            err.contains("crash-looping"),
            "name what is happening: {err}"
        );
        assert!(
            err.contains("3 restarts, then 4"),
            "say what was measured: {err}"
        );
        // The journal travels with the failure: a diagnostic that says "it
        // is restarting" without saying why sends the operator back over ssh
        // for the line we already had.
        assert!(
            err.contains("Error 802"),
            "the unit's last words must travel: {err}"
        );
        assert!(
            exec.sent[4].iter().any(|a| a.contains("journalctl")),
            "the journal is read from the VM: {:?}",
            exec.sent[4]
        );
    }

    /// A JUMP counts as much as a step: 0 → 2 is a crash-loop.
    ///
    /// The 2026-08-14 review showed it by mutation: replacing `> before`
    /// with `== before + 1` SURVIVED the whole suite, because every
    /// fixture went up by exactly one. Yet it is the common case — two
    /// restarts between two probes spaced by the wait period is
    /// nothing exotic, and `Restart=on-failure` has no reason to align
    /// with our rhythm. A failure here means the loop only sees
    /// a unit that dies FAST, and waits the full 30 minutes.
    ///
    /// Starting at 0 is the second edge, and it is measured: since the
    /// `reset-failed` of `serve_vllm_script`, the first probe of a fresh run
    /// does read 0 (x0ne-cc, 2026-08-14, fresh boot).
    #[tokio::test]
    async fn a_counter_that_jumps_by_two_is_a_crash_loop_like_any_other() {
        let mut exec = ScriptedExec {
            responses: [
                out(""),
                out(""),
                wait_nothing_bound(0, 0),
                wait_nothing_bound(2, 0),
                out("torch.AcceleratorError: CUDA error: device-side assert triggered"),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ServeVllm)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("crash-looping"), "{err}");
        assert!(
            err.contains("0 restarts, then 2"),
            "say what was measured: {err}"
        );
    }

    /// A STABLE counter is not a crash-loop.
    ///
    /// The control for the previous test, and it is not decorative: a resumed SPOT
    /// VM carries the restarts of its previous life. Reading the absolute
    /// VALUE rather than the INCREASE would fail every resume, that is,
    /// the normal case of this machine.
    #[tokio::test]
    async fn restarts_inherited_from_an_earlier_life_are_not_a_crash_loop() {
        let mut exec = ScriptedExec {
            responses: [
                out(""),
                out(""),
                wait_nothing_bound(46, 4 * 1024 * 1024),
                wait_nothing_bound(46, 8 * 1024 * 1024),
                wait_bound(),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner
            .run_step(&CcStep::ServeVllm)
            .await
            .expect("46 restarts that no longer move");
        assert_eq!(exec.sent.len(), 5, "{:?}", exec.sent);
    }

    /// A tmpfs that stays empty FAILS the wait, with the right diagnostic.
    ///
    /// The other half of the owner's requirement at run 4: the probe must
    /// be complementary, "like network throughput, size of occupied memory
    /// space". A first start pulls ~15 GB; a directory that stays at
    /// zero for five minutes is not a slow download, it is a
    /// server that is not downloading. A failure here means we pay 30
    /// minutes of H100 to learn it.
    #[tokio::test]
    async fn a_download_that_never_starts_fails_the_wait_instead_of_spending_it() {
        let mut responses = vec![out(""), out("")];
        responses.extend((0..VLLM_DOWNLOAD_STALL_PROBES).map(|_| wait_nothing_bound(0, 0)));
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ServeVllm)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            exec.sent.len(),
            2 + VLLM_DOWNLOAD_STALL_PROBES,
            "give up at the stall threshold, not at the ceiling: {:?}",
            exec.sent.len()
        );
        assert!(
            VLLM_DOWNLOAD_STALL_PROBES < VLLM_START_RETRIES,
            "otherwise the threshold is useless"
        );
        assert!(
            err.contains("nothing is being downloaded"),
            "name the diagnosis: {err}"
        );
        assert!(err.contains(MODEL_DIR), "name where nothing arrived: {err}");
        assert!(err.contains("journalctl"), "name the lever: {err}");
    }

    /// A download that PROGRESSES buys patience, not a failure.
    ///
    /// The control for the previous test. The threshold is on "~0", never on
    /// "it has not grown": a FINISHED download stays flat at 15 GB
    /// during the whole engine load, and reading that as stagnation
    /// would refuse the machine at the last minute.
    #[tokio::test]
    async fn a_download_in_progress_buys_the_patience_it_is_given() {
        let mut responses = vec![out(""), out("")];
        // Twice as many probes as the stagnation threshold, all above
        // the floor and all FLAT: the counter must never trigger.
        responses.extend(
            (0..VLLM_DOWNLOAD_STALL_PROBES * 2).map(|_| wait_nothing_bound(0, 15 * 1024 * 1024)),
        );
        responses.push(wait_bound());
        let mut exec = ScriptedExec {
            responses: responses.into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner
            .run_step(&CcStep::ServeVllm)
            .await
            .expect("15 GB in place, the engine loads");
    }

    /// SECURITY (fail-closed): an unreadable probe fails the step.
    ///
    /// The two signals added at run 4 exist only to STOP the loop
    /// early. Reading them by default — "zero restarts, plenty of data" — gives back
    /// exactly the blind wait they remove, and nobody would
    /// see it: it is the repo's dead scaffolding, applied to a budget
    /// safeguard.
    #[tokio::test]
    async fn an_unreadable_wait_probe_fails_the_step_rather_than_being_ignored() {
        let mut exec = ScriptedExec {
            responses: [out(""), out(""), out("ss: command not found\n")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ServeVllm)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            exec.sent.len(),
            3,
            "stops on the unreadable probe: {:?}",
            exec.sent
        );
        assert!(err.contains("could not read"), "{err}");
        // The control: a probe that EXITS non-zero stays a failed probe, not
        // an error — it is the lost ssh session case, and it is retried.
        let mut exec = ScriptedExec {
            responses: [
                out(""),
                out(""),
                CmdOut {
                    stdout: String::new(),
                    stderr: "kex_exchange".into(),
                    exit_code: 255,
                },
                wait_bound(),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner
            .run_step(&CcStep::ServeVllm)
            .await
            .expect("a failed probe is retried");
    }

    /// The acquis of `ServeVllm`: something already listens, so the step
    /// writes no unit and restarts nothing.
    ///
    /// On a VM back from preemption, rewriting the unit and waiting again
    /// costs half an hour of a3-highgpu-1g. And the probe is the SOCKET, not
    /// `systemctl is-active`: a unit is "active" during the whole
    /// download, that is, exactly in the state where skipping the wait
    /// would hand `ValidateEncryption` a socket that does not exist yet.
    #[tokio::test]
    async fn an_already_bound_port_makes_the_serve_step_a_no_op() {
        let mut exec = ScriptedExec {
            responses: [ss_loopback()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::ServeVllm).await.unwrap();
        assert_eq!(
            exec.sent.len(),
            1,
            "one probe, nothing else: {:?}",
            exec.sent
        );
        let all: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        assert!(
            !all.iter()
                .any(|a| a.contains("systemctl") || a.contains("tee")),
            "nothing may be written or started on an acquired socket: {all:?}"
        );
    }

    /// The acquis of `InstallVllm` is about the VERSION, not the presence of the
    /// binary.
    ///
    /// A disk back from preemption can carry an earlier pin; "vllm
    /// answers" would then skip the install and serve a version this
    /// pipeline did not choose. A failure of this test means "pinned"
    /// is no longer true on resumed VMs, that is, on the normal case
    /// of a SPOT machine.
    #[tokio::test]
    async fn the_vllm_acquis_is_the_pinned_version_not_merely_a_binary() {
        let mut exec = ScriptedExec {
            responses: [out("")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::InstallVllm).await.unwrap();
        assert_eq!(exec.sent.len(), 1, "one probe, no install: {:?}", exec.sent);
        assert!(
            exec.sent[0].iter().any(|a| a.contains(VLLM_VERSION)),
            "the probe must ask about the pinned version: {:?}",
            exec.sent[0]
        );
        // …and about the INTERPRETER, since run 4: "vllm answers 0.27.1" is
        // true on both sides of the flashinfer bug.
        assert!(
            exec.sent[0].iter().any(|a| a.contains(VLLM_TOOL_PYTHON)),
            "the probe must ask which python the tool runs on: {:?}",
            exec.sent[0]
        );
    }

    /// The pinned version is the same in the three places that name it.
    /// A failure means we install one version and verify another
    /// — the install would then run on every execution, or worse, the
    /// verification would pass on a version that is not installed.
    #[test]
    fn the_pinned_vllm_version_is_installed_and_checked_as_one_value() {
        let install = vllm_install_script();
        let check = vllm_version_cmd();
        assert!(
            install.contains(&format!("vllm=={VLLM_VERSION}")),
            "{install}"
        );
        assert!(check.contains(VLLM_VERSION), "{check}");
        // transformers is pinned IN the same isolated environment as the
        // entry point (`--with`), because 0.10.0 does not bound it and the
        // resolver then takes a version published long after it: the server
        // dies at startup on `AttributeError: … all_special_tokens_extended`
        // (measured 2026-08-14). A failure here brings back the startup crash-loop.
        assert!(
            install.contains(&format!("--with \"transformers=={TRANSFORMERS_VERSION}\"")),
            "{install}"
        );
        // And the install goes through uv, by an absolute path: the command
        // runs in a NON-interactive shell, where `~/.profile` — what puts
        // `~/.local/bin` in the PATH of an Ubuntu session — is not read. A
        // bare `uv` works by hand and fails from the pipeline.
        assert!(install.contains("$HOME/.local/bin/uv"), "{install}");
        assert!(install.contains("tool install"), "{install}");
    }

    /// The interpreter is pinned like the version, and the acquis VERIFIES it.
    ///
    /// Run 4, 2026-08-14, measured: on Ubuntu 22.04's python 3.10, importing
    /// flashinfer dies on `TypeError: 'type' object is not subscriptable`
    /// (`fd_exchange.py`, annotation `array.array[int]`); rebuilt with
    /// `--python 3.12`, it passes. Both halves matter:
    ///
    ///   * without `--python 3.12` at install, we reinstall the same broken env;
    ///   * without the question asked by the acquis, a resumed SPOT VM with the old
    ///     env answers "0.27.1" and SKIPS the reinstall — the acquis
    ///     would answer next to the question that matters, which is the
    ///     exact shape of dead scaffolding.
    ///
    /// A failure here means `x0ne cc --machine full` can go back into
    /// the import crash on a machine billed 3.72 $/h.
    #[test]
    fn the_vllm_environment_is_pinned_to_its_interpreter_at_install_and_at_check() {
        let install = vllm_install_script();
        let check = vllm_version_cmd();
        assert!(
            install.contains(&format!("--python {VLLM_PYTHON}")),
            "{install}"
        );
        // `--force`: without it, `uv tool install` on a tool already present does
        // not rebuild the environment, so python 3.10 would remain.
        assert!(install.contains("--force"), "{install}");
        // The acquis queries the TOOL's interpreter, not the system python.
        assert!(check.contains(VLLM_TOOL_PYTHON), "{check}");
        assert!(check.contains(&format!("Python {VLLM_PYTHON}.")), "{check}");
        // And it does ask TWO questions: the version AND the interpreter.
        assert!(
            check.contains("&&"),
            "both conditions hold together: {check}"
        );
    }

    /// The unit carries NO sampling flag — and that is the opposite of what
    /// this test required yesterday.
    ///
    /// It required `VLLM_USE_FLASHINFER_SAMPLER=0`, the run 4 fix against the
    /// nvcc JIT. The final session of 2026-08-14 showed that the path forced
    /// by that flag is the BUGGY path: `CUDA_LAUNCH_BLOCKING=1` named
    /// `topk_topp_sampler.py:394` (the top-k `gather` of the torch fallback of
    /// vLLM 0.27.1), and the series of eliminations cleared the sampler, the
    /// weights, the platform, CUDA Graphs and attention, ending on the
    /// stack itself: vLLM 0.27.1 computed wrong silently here. The
    /// PROVEN configuration (0.10.0 + pinned transformers) carries no
    /// flag.
    ///
    /// A failure here means an `Environment=` came back into the unit without
    /// a measurement to justify it — that is, we took back the line that
    /// cost two runs, believing it a fix.
    #[test]
    fn the_unit_carries_no_sampler_flag_because_the_proven_stack_needs_none() {
        let unit = serve_vllm_script();
        assert!(!unit.contains("VLLM_USE_FLASHINFER_SAMPLER"), "{unit}");
        assert!(!unit.contains("FLASHINFER"), "{unit}");
        // The only `Environment=` left is the one that decides WHERE the
        // weights live: there are not two.
        assert_eq!(unit.matches("Environment=").count(), 1, "{unit}");
        assert!(
            unit.contains(&format!("Environment=HF_HOME={MODEL_DIR}")),
            "{unit}"
        );
        // And no environment file either: `EnvironmentFile=`
        // loads content this function does not show, so nobody
        // rereads it in review — the `Environment=` count above would pass
        // without seeing anything.
        assert!(!unit.contains("EnvironmentFile"), "{unit}");
    }

    /// The pipeline owns ALL of the unit's state, drop-ins included.
    ///
    /// Measured on 2026-08-14: diagnostic overrides at `=1` were lying around on
    /// the VM's disk. A drop-in wins over the unit file and survives
    /// every rewrite of it, so "the file is right" and "the unit
    /// is right" are two different facts. A failure here means a
    /// setting nobody wrote in this run can decide what the enclave
    /// serves — the whole class of ghost drop-ins, not a residue.
    #[test]
    fn writing_the_unit_purges_the_drop_ins_nobody_in_this_run_wrote() {
        let unit = serve_vllm_script();
        assert!(
            unit.contains(&format!("rm -rf /etc/systemd/system/{VLLM_UNIT}.d")),
            "{unit}"
        );
        // BEFORE the reload: purging after would let systemd load the
        // overrides, and the active unit would carry what the disk just
        // lost.
        let purge = unit.find(".d").expect("the purge");
        let reload = unit.find("daemon-reload").expect("the reload");
        assert!(purge < reload, "{unit}");
    }

    /// The unit RESTARTS, otherwise nothing above reaches the server.
    ///
    /// `enable --now` starts a stopped unit and does NOTHING on a unit
    /// already active — and it is active when we get here: a previous run
    /// enabled it, so systemd started it at boot, with the file and the
    /// drop-ins present AT THAT MOMENT. `daemon-reload` does not touch
    /// running units either (systemd#31541). Without an explicit `restart`, this script
    /// would write a clean unit, purge the overrides, reload — and
    /// leave the enclave serving the environment nobody measured,
    /// while the right file sleeps on the disk.
    ///
    /// A failure here is dead scaffolding applied to the fix that was just
    /// put in place: present, documented, on the machine, and without effect.
    #[test]
    fn the_unit_is_restarted_because_enable_now_does_nothing_to_a_running_one() {
        let unit = serve_vllm_script();
        assert!(unit.contains(&format!("restart {VLLM_UNIT}")), "{unit}");
        assert!(
            !unit.contains("--now"),
            "`enable --now` restarts nothing: {unit}"
        );
        // The order IS the property: restarting before the purge or before the
        // reload would relaunch the server on what we just replaced.
        let purge = unit.find("rm -rf").expect("the purge");
        let reload = unit.find("daemon-reload").expect("the reload");
        let restart = unit.find("restart").expect("the restart");
        assert!(purge < restart && reload < restart, "{unit}");
        // And the unit stays `enable`d: that is what brings it back after a
        // SPOT VM resume.
        assert!(unit.contains(&format!("enable {VLLM_UNIT}")), "{unit}");
    }

    /// The stop is BOUNDED, and the step's budget is sized on that bound.
    ///
    /// Run 6, 2026-08-14, measured: the `restart` above is a stop followed by a
    /// start; the unit to stop was mid-download; systemd
    /// sent SIGTERM then waited its default grace (90 s); the step's ssh budget
    /// was 60 s, so the session was killed during a stop that was
    /// behaving normally, and the step failed on a machine where nothing
    /// was wrong.
    ///
    /// The two numbers form a pair: without the bound in the unit, the
    /// step's budget becomes a hope again. A failure here means
    /// one of the two moved alone.
    #[test]
    fn the_stop_is_bounded_and_the_step_budget_is_sized_on_that_bound() {
        let unit = serve_vllm_script();
        assert!(unit.contains("TimeoutStopSec=15"), "{unit}");
        // The budget covers the worst allowed stop, and room remains for
        // the start and the slowness of an ssh session — the margin, not
        // the equality, is the property.
        assert!(
            VLLM_UNIT_WRITE_TIMEOUT_SECS >= 180,
            "a restart does not fit in a probe budget: {VLLM_UNIT_WRITE_TIMEOUT_SECS}"
        );
        assert!(
            VLLM_UNIT_WRITE_TIMEOUT_SECS > IN_VM_PROBE_TIMEOUT_SECS,
            "this step is no longer a probe since it actually restarts"
        );
    }

    /// The budget is the one the STEP receives, not the one a constant
    /// announces.
    ///
    /// Without this test, the call site can fall back to
    /// `IN_VM_PROBE_TIMEOUT_SECS` while `VLLM_UNIT_WRITE_TIMEOUT_SECS`
    /// stays correct, documented and without effect — the mutation was played, it
    /// survived. It is this repo's class of defect: a setting that exists
    /// and is not IN the path. Here it is read where it lands, in the
    /// budget the exec actually received.
    #[tokio::test]
    async fn the_write_unit_step_is_given_the_budget_a_restart_needs() {
        let mut exec = ScriptedExec {
            // acquis: nothing listens; unit write; then the port is there.
            responses: [out(""), out(""), wait_bound()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner
            .run_step(&CcStep::ServeVllm)
            .await
            .expect("the green path");
        // The unit write is the first call that is not the acquis.
        let write_at = exec
            .sent
            .iter()
            .position(|argv| argv.iter().any(|a| a.contains("TimeoutStopSec")))
            .expect("the unit write");
        assert_eq!(
            exec.timeouts[write_at], VLLM_UNIT_WRITE_TIMEOUT_SECS,
            "the restart must get ITS budget: {:?}",
            exec.timeouts
        );
    }

    /// The pipeline no longer erases the weights it is waiting for.
    ///
    /// Run 5, 2026-08-14: `RemoveIPC=yes` (logind default) deletes a user's
    /// POSIX IPC objects when their LAST session closes,
    /// and a file in `/dev/shm` is one. Each pipeline probe being an
    /// ssh session, each probe destroyed the 15 GB downloaded. Measured: 10
    /// MB written to `/dev/shm/ipc-test`, session closed, next session
    /// `No such file or directory`.
    ///
    /// A failure here means the pipeline started erasing its own
    /// download again — the symptom will read as a slow network, never as
    /// what it is.
    #[test]
    fn the_tmpfs_step_stops_logind_from_deleting_the_weights() {
        let s = fetch_model_script();
        assert!(s.contains(LOGIND_DROPIN), "{s}");
        assert!(s.contains("[Login]"), "{s}");
        assert!(s.contains("RemoveIPC=no"), "{s}");
        // Writing the file is not enough: logind reads it at startup.
        assert!(s.contains("restart systemd-logind"), "{s}");
        // And it applies BEFORE the weights directory exists: the order is the
        // property, not the presence of the lines.
        let dropin = s.find("10-x0ne-keep-shm.conf").expect("the drop-in");
        let mkdir = s.find(&format!("mkdir -p {MODEL_DIR}")).expect("the tmpfs");
        assert!(
            dropin < mkdir,
            "the purge must be disarmed before the first byte: {s}"
        );
        // And it is REREAD on the live daemon, after the restart: a written file
        // says nothing about the process that had already read its configuration.
        let restart = s.find("restart systemd-logind").expect("the restart");
        let reread = s.find(LOGIND_REMOVEIPC_CMD).expect("the reread");
        assert!(restart < reread, "reread happens AFTER the restart: {s}");
    }

    /// SECURITY (fail-closed): a logind that still purges STOPS the step.
    ///
    /// It is the reread of review I3: installing the drop-in is not the
    /// same thing as the daemon applying it. A failure here means a run
    /// can again download 15 GB into a directory that the next
    /// ssh disconnection erases — and report it as a slow network.
    #[tokio::test]
    async fn a_logind_that_still_purges_fails_the_step_instead_of_downloading() {
        let armed = format!(
            "{TMPFS_PROBE_MARK_REMOVEIPC}\nb true\n{TMPFS_PROBE_MARK_AVAIL}\nAvail\n{}\n",
            MODEL_TMPFS_MIN_KIB * 2
        );
        let mut exec = ScriptedExec {
            responses: [out(&armed)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::FetchModel)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("RemoveIPC"), "{err}");
        // The message must give the command that shows the state, not only
        // the verdict: without it the operator goes back over ssh for the line.
        assert!(err.contains("busctl"), "{err}");

        // The control: the SAME free space passes when logind no longer purges.
        let disarmed = armed.replace("b true", "b false");
        let mut exec = ScriptedExec {
            responses: [out(&disarmed)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner
            .run_step(&CcStep::FetchModel)
            .await
            .expect("logind disarmed, the space is there");
    }

    /// The unit starts on a clean restart counter.
    ///
    /// `NRestarts` survives the `restart`s of a single boot: the 43 of run 5
    /// came from the previous boot. The crash-loop detector compares two
    /// readings, so it held — a failure here means it becomes
    /// possible again to pronounce a verdict on a counter this run did not
    /// increment.
    #[test]
    fn the_unit_starts_from_a_restart_counter_this_run_owns() {
        let unit = serve_vllm_script();
        assert!(
            unit.contains(&format!("reset-failed {VLLM_UNIT}")),
            "{unit}"
        );
        // After the daemon-reload and BEFORE the start: resetting the counter to
        // zero after the `restart` would erase a failure this run just
        // caused.
        let reset = unit.find("reset-failed").expect("the reset");
        let reload = unit.find("daemon-reload").expect("the reload");
        let start = unit.find("restart").expect("the start");
        assert!(reload < reset && reset < start, "{unit}");
    }

    /// SECURITY: the systemd unit and the validation read THE SAME port.
    ///
    /// A validation that checks 8000 while the server listens on 8001
    /// proves nothing about the machine it runs on (concern no. 1 of the report of
    /// task 3). Both assertions are built from `VLLM_PORT`:
    /// if either carried a literal, they would diverge at the first
    /// change of the constant.
    #[test]
    fn the_unit_and_the_validation_agree_on_the_port_by_construction() {
        let unit = serve_vllm_script();
        assert!(unit.contains(&format!("--port {VLLM_PORT}")), "{unit}");
        assert!(unit.contains("--host 127.0.0.1"), "{unit}");
        assert!(
            vllm_models_cmd().contains(&format!(":{VLLM_PORT}/v1/models")),
            "{}",
            vllm_models_cmd()
        );
        // The model is a local constant: nothing remote decides what
        // the enclave downloads and runs.
        assert!(unit.contains(VLLM_MODEL), "{unit}");
    }

    /// A tmpfs too small REFUSES, it does not fall back to disk.
    ///
    /// It is the project's arbitration written in code: the weights live in RAM
    /// encrypted by TDX or do not live. A failure of this test means a
    /// model could land on a disk the host reads, because RAM
    /// was short.
    #[tokio::test]
    async fn a_tmpfs_too_small_refuses_rather_than_falling_back_to_the_disk() {
        let cramped = format!(
            "{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n{TMPFS_PROBE_MARK_AVAIL}\n     Avail\n 2097152\n"
        );
        let mut exec = ScriptedExec {
            responses: [out(&cramped)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::FetchModel)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 GiB free"), "say what was measured: {err}");
        assert!(
            err.contains("NOT going to the disk"),
            "say what will NOT happen: {err}"
        );
        assert!(err.contains("remount"), "name the lever: {err}");

        // And unreadable free space is an error, never a default number.
        let unreadable = format!(
            "{TMPFS_PROBE_MARK_REMOVEIPC}\nb false\n{TMPFS_PROBE_MARK_AVAIL}\ndf: /dev/shm: No such file\n"
        );
        let mut exec = ScriptedExec {
            responses: [out(&unreadable)].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::FetchModel)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot read the tmpfs probe"), "{err}");
    }

    /// SECURITY, and it is THE class of defect of this repo: what
    /// `ValidateEncryption` measures must REACH the report.
    ///
    /// The field exists, it is documented, it is filled — and if `outcome()` did not
    /// pass it on, the pipeline would go green printing
    /// nothing: the sixth dead scaffolding. This test walks the real path —
    /// verified quote, measured checks, report rendered from `outcome()` — and
    /// not from a value the test would have fabricated itself.
    ///
    /// `AttestGpu` has been part of the path since task 5: on the full
    /// machine, `outcome()` no longer returns `Proven` without GPU proof, so a run that
    /// skipped that step would no longer test the PROVEN path.
    #[tokio::test]
    async fn what_the_checks_measure_reaches_the_report_through_outcome() {
        let mut exec = ScriptedExec {
            responses: [
                honest_quote(),
                out("CC status: ON"),
                out("580.126.09"),
                out(""),
                ss_loopback(),
                out("[]"),
                disk_ok(),
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::AttestTdx).await.unwrap();
        runner.run_step(&CcStep::AttestGpu).await.unwrap();
        runner.run_step(&CcStep::ValidateEncryption).await.unwrap();
        let r = render_report(&runner.outcome(None), 0.4, CcMachine::FullH100, "running");
        assert!(r.contains("PROVEN"), "{r}");
        assert!(
            r.contains("  swap       off"),
            "the measurement must reach the report: {r}"
        );
        assert!(
            r.contains(&format!("  bind       127.0.0.1:{VLLM_PORT} only")),
            "{r}"
        );
        assert!(!r.contains("encryption unverified"), "{r}");
    }

    /// SECURITY (minor M-f of task 3, re-judged at task 5): on
    /// `--machine full`, a proven CPU enclave WITHOUT a GPU measurement is not a
    /// success, and `outcome()` does not return that `Proven`.
    ///
    /// The hole was latent as long as `--machine full` was refused; it becomes
    /// real the day the option reopens. `gpu_line` prints exactly that
    /// state — "gpu exposed — no GPU measurement was obtained on this run" —
    /// while `exit_status_for` only looks at the variant and returns 0. A
    /// `x0ne cc --machine full --yes && echo ok` would display "ok" above
    /// a report saying the GPU is uncovered, and a script does not read
    /// a report.
    ///
    /// The counter-test matters as much as the test: the prover HAS NO
    /// GPU, so the same absence must stay a `Proven` there. A guard that
    /// refused both would break the machine that works.
    #[tokio::test]
    async fn a_full_run_that_never_measured_the_gpu_is_not_proven() {
        let mut exec = ScriptedExec {
            responses: [honest_quote()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.run_step(&CcStep::AttestTdx).await.unwrap();
        let outcome = runner.outcome(None);
        match &outcome {
            CcOutcome::Failed { step, error, .. } => {
                assert_eq!(*step, CcStep::AttestGpu);
                assert!(
                    error.contains("never measured"),
                    "say what is missing: {error}"
                );
            }
            _ => panic!("a proven CPU enclave without GPU proof is not a successful full run"),
        }
        // And the report does not carry the word: PROVEN and "gpu exposed" cannot
        // coexist, that is the whole property.
        let r = render_report(&outcome, 0.4, CcMachine::FullH100, "stopped");
        assert!(!r.contains("PROVEN"), "{r}");
        assert!(r.contains("no GPU measurement was obtained"), "{r}");
        // …but the CPU half was indeed measured, and the error says so:
        // the report must therefore show it (review round 1 of task 5,
        // I1+I2). A sentence "the CPU enclave is proven" without mr_td
        // below it is a claim with no attachment.
        assert!(
            r.contains(&"ab".repeat(48)),
            "the measured mr_td must be printed: {r}"
        );

        // The counter-test: same absence, machine without GPU, PROVEN verdict.
        let mut exec = ScriptedExec {
            responses: [honest_quote()].into(),
            ..Default::default()
        };
        let cfg = CcConfig {
            machine: CcMachine::Prover,
            vm_name: "x0ne-cc".into(),
            cap_usd: 25.0,
        };
        let mut runner = CcRunner::new(&mut exec, cfg);
        runner.run_step(&CcStep::AttestTdx).await.unwrap();
        assert!(
            matches!(runner.outcome(None), CcOutcome::Proven { gpu: None, .. }),
            "the prover has no GPU: its absence is not a missing proof"
        );
    }

    /// SECURITY (finding I1, review round 1): the report of a REFUSAL says what
    /// was measured, not "nothing was proven".
    ///
    /// The only run where the in-VM check lines have something to say is the one that
    /// fails: "swap ACTIVE" next to "bind loopback" and "firewall
    /// closed" names the hole AND clears the two others. As long as
    /// `CcOutcome::Failed` did not carry the measurements, those three failure
    /// texts were code no production path reached —
    /// two mutants survived on them at the reviewer's, which is the
    /// operational definition of "dead".
    ///
    /// A failure of this test means `x0ne cc` refuses a machine without saying
    /// which of the four properties was missing, while also claiming it measured
    /// nothing when it holds the measurements.
    #[tokio::test]
    async fn the_report_of_a_refusal_says_which_check_failed() {
        let swapon_table =
            out("NAME      TYPE      SIZE USED PRIO\n/swapfile file        2G   0B   -2\n");
        let mut exec = ScriptedExec {
            responses: [swapon_table, ss_loopback(), out("[]"), disk_ok()].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::ValidateEncryption)
            .await
            .unwrap_err()
            .to_string();
        let r = render_report(
            &runner.outcome(Some((CcStep::ValidateEncryption, err))),
            0.40,
            CcMachine::FullH100,
            "stopped",
        );
        assert!(r.contains("PIPELINE FAILED at ValidateEncryption"), "{r}");
        // THE line: it names the hole…
        assert!(
            r.contains("  swap       ACTIVE"),
            "the failing check must be named in the report, not only in the error: {r}"
        );
        // …and it clears the two others, which a global "unverified"
        // made impossible to say.
        assert!(
            r.contains(&format!("  bind       127.0.0.1:{VLLM_PORT} only")),
            "{r}"
        );
        assert!(r.contains("  firewall   no INGRESS rule opens"), "{r}");
        assert!(
            !r.contains("encryption unverified"),
            "a run that measured three checks did not measure nothing: {r}"
        );

        // The TWO other failure branches, because fixing the wiring without
        // exercising them leaves exactly the defect we just fixed: the
        // mutation "the line always says loopback" and its firewall
        // equivalent SURVIVED the first version of this test, which only
        // covered swap (measured, review round 1). Three branches, three
        // scenarios, otherwise two of them remain text that nothing
        // renders.
        let cases: [(CmdOut, CmdOut, &str, &str); 2] = [
            (
                ss_all_interfaces(),
                out("[]"),
                "  bind       port 8000 is NOT loopback-only",
                "  firewall   no INGRESS rule opens",
            ),
            (
                ss_loopback(),
                out(&format!(
                    r#"[{{"name":"open","allowed":[{{"IPProtocol":"tcp","ports":["{VLLM_PORT}"]}}],"direction":"INGRESS"}}]"#
                )),
                "  firewall   an INGRESS rule OPENS",
                &format!("  bind       127.0.0.1:{VLLM_PORT} only"),
            ),
        ];
        for (ss, fw, must_say, must_clear) in cases {
            let mut exec = ScriptedExec {
                responses: [out(""), ss, fw, disk_ok()].into(),
                ..Default::default()
            };
            let mut runner = CcRunner::new(&mut exec, full());
            let err = runner
                .run_step(&CcStep::ValidateEncryption)
                .await
                .unwrap_err()
                .to_string();
            let r = render_report(
                &runner.outcome(Some((CcStep::ValidateEncryption, err))),
                0.40,
                CcMachine::FullH100,
                "stopped",
            );
            assert!(r.contains(must_say), "the failing check must be named: {r}");
            assert!(r.contains(must_clear), "and the passing ones cleared: {r}");
            assert!(r.contains("  swap       off"), "{r}");
        }

        // The GPU counterpart, same fix (concern M-b of task 3): a mode
        // read OFF then refused prints as read, and not "no measurement".
        let mut exec = ScriptedExec {
            responses: [out("CC status: OFF"), out("580.126.09")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::AttestGpu)
            .await
            .unwrap_err()
            .to_string();
        let r = render_report(
            &runner.outcome(Some((CcStep::AttestGpu, err))),
            0.40,
            CcMachine::FullH100,
            "stopped",
        );
        assert!(
            r.contains("the GPU answered OFF (driver 580.126.09)"),
            "a measured OFF must read as measured: {r}"
        );
        assert!(
            !r.contains("no GPU measurement was obtained"),
            "that line is for runs that measured nothing: {r}"
        );
    }

    /// SECURITY (finding I2, review round 1): a FOREIGN model already served does
    /// not remove `ServeVllm` or `HealthCheck` from the plan.
    ///
    /// A single observation removes both steps, and `HealthCheck` is
    /// precisely the one that checks the model's IDENTITY. Read as "the
    /// server answers something", a VM serving `evil/backdoored-13b` on
    /// 127.0.0.1:8000 made both disappear: our unit
    /// would never have been written, the name never compared, and Increment 3
    /// would install `x0ne` against a model nobody chose. It is
    /// not a live hole today only because `InstallX0ne` still fails
    /// as unimplemented — that is, by accident.
    #[tokio::test]
    async fn a_vm_serving_someone_elses_model_still_gets_our_unit_and_our_health_check() {
        let foreign =
            out(r#"{"object":"list","data":[{"id":"evil/backdoored-13b","object":"model"}]}"#);
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("p\n"),
                out(r#"[{"name":"img"}]"#),
                out(r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"http://x/zones/us-central1-a"}]"#),
                ready_smi(),
                out("CC status: ON"),
                foreign,
                CmdOut { stdout: String::new(), stderr: String::new(), exit_code: 127 },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let obs = runner.observe().await.unwrap();
        let approved = build_plan(&full(), &obs).unwrap();
        let replanned = runner.replan_after_ssh(&approved[2..]).await;
        assert!(
            replanned.contains(&CcStep::ServeVllm),
            "a server we did not start is not our server: {replanned:?}"
        );
        assert!(
            replanned.contains(&CcStep::HealthCheck),
            "the step that checks the model's identity may not be skipped by a model we \
             never checked: {replanned:?}"
        );
        // And the same probe, with OUR model, does remove both: without
        // this half, the assertion above would also hold for an acquis
        // that no longer works at all.
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("p\n"),
                out(r#"[{"name":"img"}]"#),
                out(r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"http://x/zones/us-central1-a"}]"#),
                ready_smi(),
                out("CC status: ON"),
                models_serving(),
                CmdOut { stdout: String::new(), stderr: String::new(), exit_code: 127 },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let obs = runner.observe().await.unwrap();
        let approved = build_plan(&full(), &obs).unwrap();
        let replanned = runner.replan_after_ssh(&approved[2..]).await;
        assert!(!replanned.contains(&CcStep::ServeVllm), "{replanned:?}");
        assert!(!replanned.contains(&CcStep::HealthCheck), "{replanned:?}");
        assert!(
            replanned.contains(&CcStep::ValidateEncryption),
            "a PROOF is replayed on every pass, whatever is acquired: {replanned:?}"
        );
    }

    /// On the full machine, the in-VM check lines are ALWAYS there, and their default
    /// is "unverified". A missing line reads "nothing to report", while
    /// the state described is the one where nothing was proven confidential.
    #[test]
    fn the_full_machine_report_always_carries_the_encryption_lines() {
        for outcome in [
            CcOutcome::Refused {
                reason: "nonce mismatch".into(),
            },
            CcOutcome::Failed {
                step: CcStep::ValidateEncryption,
                error: "REFUSED: swap".into(),
                proof: None,
                gpu: None,
                encryption: None,
            },
            CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: None,
                encryption: None,
            },
        ] {
            let r = render_report(&outcome, 0.4, CcMachine::FullH100, "stopped");
            assert!(r.contains("encryption unverified"), "{r}");
            assert!(
                !r.contains("  swap       off"),
                "nothing may read as proven: {r}"
            );
        }
        // And the prover machine has no model to serve: those lines have
        // nothing to say there, and four "unverified" lines on a report that
        // never had anything to validate teach the operator to ignore them.
        let r = render_report(
            &CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: None,
                encryption: None,
            },
            0.1,
            CcMachine::Prover,
            "stopped",
        );
        assert!(!r.contains("encryption"), "{r}");
    }

    /// On the full machine, the gpu line is ALWAYS there, and its default
    /// is "exposed". A missing line reads "nothing to report", while
    /// the state it describes is the one where no model may run.
    #[test]
    fn the_full_machine_report_always_carries_a_gpu_line() {
        for outcome in [
            CcOutcome::Refused {
                reason: "nonce mismatch".into(),
            },
            CcOutcome::Failed {
                step: CcStep::WaitSsh,
                error: "timeout".into(),
                proof: None,
                gpu: None,
                encryption: None,
            },
            CcOutcome::Proven {
                proof: proven_cpu(),
                gpu: None,
                encryption: None,
            },
        ] {
            let r = render_report(&outcome, 0.4, CcMachine::FullH100, "stopped");
            assert!(r.contains("gpu        exposed"), "{r}");
            assert!(
                !r.contains("cc-mode"),
                "no proof vocabulary without a measurement: {r}"
            );
        }
    }

    // ── x0ne in the enclave (task 6, 2026-08-14) ────────────────────────

    fn fake_binaries() -> X0neBinaries {
        X0neBinaries {
            x0ne: "/build/x0ne".into(),
            runtime: "/build/x0ne-runtime".into(),
        }
    }

    /// The green path of `InstallX0ne`: BOTH binaries go up, BOTH
    /// configs come down, and the verification is the acquis probe itself.
    ///
    /// The test pins what really goes out on the wire — the `scp` argv and the
    /// budget given to it — because it is the only step that sends a
    /// file from this machine to the enclave. A failure means we
    /// copy something other than what the operator read in the plan, or that a
    /// 40 MB transfer inherits the budget of a 60 s probe.
    #[tokio::test]
    async fn install_x0ne_copies_both_binaries_then_writes_both_configs() {
        let mut exec = ScriptedExec {
            responses: [
                CmdOut {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 1,
                }, // not installed
                out(""), // scp
                out(""), // the configs
                out(""), // the verification
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.set_x0ne_binaries(fake_binaries());
        runner
            .run_step(&CcStep::InstallX0ne)
            .await
            .expect("the green path");

        let scp = &exec.sent[1];
        assert_eq!(
            scp[0..2],
            ["compute".to_string(), "scp".to_string()],
            "{scp:?}"
        );
        assert!(scp.contains(&"/build/x0ne".to_string()), "{scp:?}");
        assert!(scp.contains(&"/build/x0ne-runtime".to_string()), "{scp:?}");
        assert!(
            scp.contains(&"x0ne-cc:~/".to_string()),
            "the destination is the VM's HOME: {scp:?}"
        );
        assert!(scp.iter().any(|a| a.starts_with("--zone=")), "{scp:?}");
        // The transfer's budget is the transfer's. Built from the
        // function, never hardcoded: an assertion on "460" would pass
        // still the day the floor throughput changes and the plan's promise
        // would no longer match what the runtime kills.
        assert_eq!(exec.timeouts[1], scp_budget_secs(), "{:?}", exec.timeouts);
        assert!(
            scp_budget_secs() > IN_VM_PROBE_TIMEOUT_SECS,
            "40 MB do not upload within a probe"
        );

        // The configs travel as base64: what is sent is exactly what
        // this file writes, or `base64 -d` fails.
        use base64::Engine;
        let b64 = |s: String| base64::engine::general_purpose::STANDARD.encode(s);
        let written = exec.sent[2].join(" ");
        assert!(
            written.contains(&b64(enclave_x0ne_config())),
            "the x0ne config: {written}"
        );
        assert!(
            written.contains(&b64(enclave_runtime_policy())),
            "the runtime policy: {written}"
        );
        assert!(
            written.contains("chmod +x"),
            "a binary without the exec bit breaks at `e`: {written}"
        );
        // The token substitution, once per file. Nothing checked it:
        // removing the `| sed` from `config.toml` alone left everything green (measured
        // 2026-08-14, mutant V2), and the enclave started on an
        // `x0ne_core_bin = "@X0NE_HOME@/x0ne-runtime"` — a literal path that
        // does not exist, so a bridge that does not launch, at the moment
        // the operator types `e`. It is a check of the gesture's EXISTENCE,
        // not of its result: the result lives on the VM (see the caveats of the
        // task report).
        assert_eq!(
            written
                .matches(&format!("sed \"s#{ENCLAVE_HOME_TOKEN}#$HOME#g\""))
                .count(),
            2,
            "BOTH configs must get their token substituted: {written}"
        );

        // The last command is the acquis probe EXCEPT FOR DIAGNOSTICS:
        // the same four checks — the step ends in the state its own
        // detection recognizes — but the verification lets the
        // loader speak. Until run 8 (2026-08-15), the probe's `2>/dev/null`
        // erased "GLIBC_2.39 not found" on the VM, before ssh could
        // report it. Equality is therefore stated on the normalized form, and no longer
        // word for word.
        let probe = exec.sent[0].join(" ");
        let verify = exec.sent[3].join(" ");
        assert!(
            !verify.contains("2>/dev/null"),
            "the verification must surface the loader's error: {verify}"
        );
        assert_eq!(
            verify.replace("--version", "--version 2>/dev/null"),
            probe,
            "verify ≠ detect would be two definitions of \"ready\""
        );
    }

    /// What the VM says when the copied binary does not LOAD reaches the
    /// report.
    ///
    /// It is the failure of run 8 (2026-08-15), and the fix that would have
    /// named it: binaries built on Debian 13 (glibc 2.41) copied into an
    /// Ubuntu 22.04 (glibc 2.35), `x0ne --version` refused by the loader, and
    /// a probe that threw the message away on the VM. What came back was
    /// "exit 1", at the end of a ~30 min pipeline at 3.72 $/h that had passed
    /// everything else; the cause cost an `objdump` session on the host.
    /// A failure here means we are back to a silent failure.
    #[tokio::test]
    async fn a_binary_the_enclave_cannot_load_says_so_in_the_failure() {
        let loader = "/home/op/x0ne: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not \
                      found (required by /home/op/x0ne)";
        let mut exec = ScriptedExec {
            responses: [
                CmdOut {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 1,
                }, // not installed
                out(""), // scp
                out(""), // the configs
                // the verification: the loader refuses the freshly copied binary
                CmdOut {
                    stdout: String::new(),
                    stderr: loader.into(),
                    exit_code: 1,
                },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.set_x0ne_binaries(fake_binaries());
        let err = runner
            .run_step(&CcStep::InstallX0ne)
            .await
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("GLIBC_2.39"),
            "the required version must be readable: {err}"
        );
        assert!(
            err.contains("libc.so.6"),
            "and that it is the loader that refuses: {err}"
        );
        assert!(
            err.contains(X0NE_VERSION),
            "the message keeps what it was checking: {err}"
        );
        assert!(
            err.chars().count() < X0NE_VERIFY_ECHO_MAX_CHARS + 200,
            "bounded regardless ({} characters)",
            err.chars().count()
        );
    }

    /// The failure of an ssh step quotes the BOUNDED command, not the 2.5 KB of
    /// base64 it carries.
    ///
    /// It is not a question of trust — the script is built from local
    /// constants — but of readability, and it has a real cost: `install_x0ne` is
    /// the only step whose command weighs two base64 blobs, and a message that
    /// copies them in full pushes off the screen the two lines the VM
    /// actually said. A failure here means we made unreadable the
    /// diagnostic of the step that installs the binary the operator will trust.
    #[tokio::test]
    async fn a_failed_config_write_quotes_a_bounded_command_not_the_whole_payload() {
        let mut exec = ScriptedExec {
            responses: [
                CmdOut {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 1,
                }, // not installed
                out(""), // scp
                // the config write fails, and the VM says why
                CmdOut {
                    stdout: String::new(),
                    stderr: "sed: couldn't open file: Read-only file system".into(),
                    exit_code: 1,
                },
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.set_x0ne_binaries(fake_binaries());
        let err = runner
            .run_step(&CcStep::InstallX0ne)
            .await
            .unwrap_err()
            .to_string();

        // What the VM said survives: it is the only actionable content.
        assert!(
            err.contains("Read-only file system"),
            "the VM's diagnostic must remain: {err}"
        );
        // The script, on the other hand, is cut. Its size is the measure: the two
        // base64 blobs alone weigh thousands of characters.
        let script = write_enclave_configs_script();
        assert!(
            script.len() > 2000,
            "test premise: the script is large ({} B)",
            script.len()
        );
        assert!(
            !err.contains(&script),
            "the whole script ({} B) must not be copied into the error: {} B",
            script.len(),
            err.len()
        );
        assert!(
            err.len() < script.len(),
            "the error ({} B) must stay shorter than the command it quotes ({} B)",
            err.len(),
            script.len()
        );
    }

    /// SECURITY, exact analogue of I2 of task 4 (the foreign model):
    /// "installed" is a question of VERSION, never of a file's presence.
    ///
    /// The disk of a preempted VM carries the previous run's binaries. A
    /// probe reading "there is an x0ne in the home" would remove `InstallX0ne`
    /// from the plan and leave the operator talking to a binary this pipeline
    /// never copied — in an enclave whose report says PROVEN.
    ///
    /// The second assertion is the one that matters and it is measured, not
    /// assumed: the re-planning and the step must ask THE SAME question.
    /// Two equivalent-but-different commands are two definitions of
    /// "ready" that diverge at the first change to one of them.
    #[tokio::test]
    async fn a_stale_x0ne_from_an_earlier_run_is_never_taken_for_installed() {
        let cmd = x0ne_ready_cmd();
        assert!(
            cmd.contains(X0NE_VERSION),
            "the version is in the question: {cmd}"
        );
        assert!(cmd.contains("$HOME/x0ne\" --version"), "{cmd}");
        assert!(
            cmd.contains("$HOME/x0ne-runtime\" --version"),
            "BOTH binaries: {cmd}"
        );
        assert!(cmd.contains(".x0ne/config.toml"), "and both configs: {cmd}");
        assert!(cmd.contains(".config/x0ne/x0ne-runtime.toml"), "{cmd}");

        // What the re-probe sends.
        let mut exec = ScriptedExec {
            responses: [
                out(r#"[{"account":"a@b.c","status":"ACTIVE"}]"#),
                out("p\n"),
                out(r#"[{"name":"img"}]"#),
                out(r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"http://x/zones/us-central1-a"}]"#),
                out("no driver"),
                out("nothing"),
                out("nothing"),
                out("x0ne 0.0.1-old"), // exit 0: a binary answers…
            ]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.observe().await.unwrap();
        let _ = runner
            .replan_after_ssh(&[CcStep::InstallX0ne, CcStep::Report])
            .await;
        let replan_probe = exec.sent.last().expect("the x0ne probe").clone();

        // What the step sends.
        let mut exec2 = ScriptedExec {
            responses: [out("x0ne 0.0.1-old")].into(),
            ..Default::default()
        };
        let mut runner2 = CcRunner::new(&mut exec2, full());
        runner2.set_x0ne_binaries(fake_binaries());
        runner2.run_step(&CcStep::InstallX0ne).await.unwrap();
        assert_eq!(
            replan_probe, exec2.sent[0],
            "the re-probe and the step ask the same question"
        );
    }

    /// An x0ne already installed AT THE RIGHT VERSION does not upload 40 MB again: the acquis
    /// cuts the step after a single command. It is the counter-test of the
    /// previous test — without it, a permanently broken probe would pass both.
    #[tokio::test]
    async fn an_x0ne_already_at_this_version_is_not_uploaded_again() {
        let mut exec = ScriptedExec {
            responses: [out("")].into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        runner.set_x0ne_binaries(fake_binaries());
        runner.run_step(&CcStep::InstallX0ne).await.unwrap();
        assert_eq!(
            exec.sent.len(),
            1,
            "one probe, and nothing else: {:?}",
            exec.sent
        );
        assert!(
            !exec.sent.iter().flatten().any(|a| a == "scp"),
            "nothing is uploaded to an up-to-date VM: {:?}",
            exec.sent
        );
    }

    /// No paths, no upload: the step REFUSES instead of inventing a
    /// `target/release`. It is the binary the operator is about to launch
    /// in an attested enclave; guessing which one it is would be the only place
    /// in the pipeline where a path nobody read decides what runs.
    ///
    /// The message names the command that fixes it, repo rule: a refusal
    /// you cannot act on costs a search round-trip.
    #[tokio::test]
    async fn install_x0ne_refuses_rather_than_guess_where_the_binaries_are() {
        let mut exec = ScriptedExec {
            responses: [CmdOut {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
            }]
            .into(),
            ..Default::default()
        };
        let mut runner = CcRunner::new(&mut exec, full());
        let err = runner
            .run_step(&CcStep::InstallX0ne)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cargo build --release"),
            "name the lever: {err}"
        );
        assert_eq!(
            exec.sent.len(),
            1,
            "nothing is sent after the refusal: {:?}",
            exec.sent
        );
    }

    /// The config the enclave receives points at ITS vLLM and at nothing else,
    /// and it is the orchestrator itself that rereads it here — not a substring
    /// assertion on a text nobody parses.
    ///
    /// A failure means the enclave's x0ne starts on the default backend
    /// (Anthropic), that is, it leaves the VM at the first
    /// question asked: the exact opposite of what the machine demonstrates.
    #[test]
    fn the_enclave_config_is_parsed_by_the_orchestrator_and_points_at_loopback() {
        let text = enclave_x0ne_config();
        let cfg: crate::config::Config = toml::from_str(&text)
            .expect("the orchestrator must be able to read what is written for it");
        assert_eq!(cfg.llm.provider, "openai-compat");
        assert_eq!(
            cfg.llm.openai_compat.base_url,
            format!("http://127.0.0.1:{VLLM_PORT}/v1")
        );
        assert_eq!(
            cfg.llm.openai_compat.model, VLLM_MODEL,
            "the model is the one HealthCheck named"
        );
        // The token SPELLED OUT, never `ENCLAVE_HOME_TOKEN`. Compared with
        // itself, it validates anything: set to "/", the constant went
        // into the policy as `allowed_*_paths = ["/"]` AND the remote
        // `sed s#/#$HOME#g` rewrote every slash of both files — after the
        // PROVEN report, without a word (measured 2026-08-14, mutant V1).
        assert_eq!(cfg.bridge.x0ne_core_bin, "@X0NE_HOME@/x0ne-runtime");
        assert!(
            !text.contains("api.anthropic.com") && !text.contains("api.openai.com"),
            "nothing may leave the VM: {text}"
        );
    }

    /// SECURITY: the policy written into the enclave is a DEMO OF
    /// CONFINEMENT — deny by default, the home and nothing else — and it is the
    /// real `AppConfig::load` of x0ne-runtime that rereads it.
    ///
    /// Two distinct failures are possible here and both have already happened
    /// in this repo. A file with the v0.2 schema (`allowed_dirs`, no
    /// `default_effect`) produces a runtime that does not start — incident of
    /// 2026-08-10, on every fresh install. And `mode = "confirm"` in the
    /// RUNTIME means "refuse every modifying action": it has nobody to
    /// ask (`executor/mod.rs:178`), so every approved command
    /// came back "Action requires user approval" AFTER the operator's yes —
    /// field report of 2026-08-12, the double-gating of invariant 2.
    #[test]
    fn the_enclave_policy_is_deny_by_default_and_the_runtime_accepts_it() {
        let text = enclave_runtime_policy();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x0ne-runtime.toml");
        std::fs::write(&path, &text).unwrap();
        let cfg = x0ne_runtime::config::AppConfig::load(&path)
            .expect("the runtime must start on this file (schema v0.3)");

        assert_eq!(
            cfg.policy.default_effect, "deny",
            "a confinement demo denies by default"
        );
        // Literal, not the constant: see `the_enclave_config_is_parsed…`.
        // A token set to "/" would give `["/"]` here on both sides of the `==`,
        // that is, a policy open on the whole machine, validated by a
        // green assertion (mutant V1).
        assert_eq!(
            cfg.policy.allowed_read_paths,
            vec!["@X0NE_HOME@".to_string()]
        );
        assert_eq!(
            cfg.policy.allowed_write_paths,
            vec!["@X0NE_HOME@".to_string()]
        );
        assert_ne!(
            cfg.policy.mode, "confirm",
            "runtime-side \"confirm\" means \"deny everything\": report of 2026-08-12"
        );
        assert!(
            !text.contains("allowed_dirs"),
            "v0.2 key: it no longer exists (2026-08-10)"
        );

        // SECURITY: the EXACT set, not a denylist of four names.
        // `allowed_commands` is THE ONLY control of `RunCommand` — the known
        // gap of docs/invariants.md, `cat` reads what `ReadFile` refuses — and a
        // denylist says nothing about the names it does not enumerate: adding
        // "sh", "bash", "python3", "ssh", "scp", "nc" went through it without a
        // sound (measured 2026-08-14, mutant T1). Worse, `sh` in this list
        // cancels repo invariant 6 — `RunCommand` has no shell, and
        // the whole read-only exemption of `readonly_cmd.rs` rests on that.
        // A failure here means the enclave's execution surface was widened:
        // it is a decision, and it is taken by changing THIS test.
        //
        // It was taken on 2026-08-15, OWNER'S ARBITRATION after his
        // first real session in the enclave (run 9): "it's pretty locked
        // down … like it can't read anything … couldn't we maybe free it up
        // a bit more". Ten names are added to the nine of v1, all
        // readers of machine state (`du`, `df`, `free`, `ps`, `id`,
        // `whoami`, `hostname`, `uname`, `date`, `ss`). None leaves the VM,
        // and that is the frontier here: `ss` READS sockets, it does not open any.
        //
        // SECURITY: `sort` was REMOVED the same day, a few hours later.
        // `sort -o FILE INPUT` writes, and the review measured it end to end
        // on this policy — `sort -o <outside-root>/planted.txt
        // <home>/.ssh/id_rsa` succeeded, the key copied outside every root,
        // without a human being consulted (`readonly_cmd` exempted it from the
        // gate). No "read-only" holds for a
        // program that takes an output file as an argument.
        //
        // A caveat on three names that remain, so that this comment does not
        // reassure wrongly. `date -s` sets the clock and `hostname NAME`
        // changes the machine's name; `nvidia-smi -r` resets a GPU and
        // `-pl` changes its power limit. All four require
        // root privilege, which the enclave's agent does not have (measured non-root on
        // 2026-08-15: "Operation not permitted"). They are readers
        // OF STATE UNDER AN UNPRIVILEGED ACCOUNT, not readers by nature. If
        // the enclave's x0ne ever ran as root, these four are the
        // first to remove.
        assert_eq!(
            cfg.policy.allowed_commands,
            vec![
                "ls",
                "cat",
                "head",
                "tail",
                "grep",
                "find",
                "wc",
                "du",
                "df",
                "free",
                "ps",
                "id",
                "whoami",
                "hostname",
                "uname",
                "date",
                "ss",
                "nvidia-smi"
            ],
            "the enclave command list is closed, with no writer and no output"
        );
        // SECURITY: by name, the writer that had slipped in. The equality
        // above fails for any change; this one says which one
        // must never come back, and why.
        for writer in ["sort", "uniq", "tree", "tee", "sed", "awk", "dd", "split"] {
            assert!(
                !cfg.policy.allowed_commands.iter().any(|c| c == writer),
                "{writer} can write a file named as an argument: {:?}",
                cfg.policy.allowed_commands
            );
        }
        // The denylist stays, as a complement: it names what we refuse and
        // why, where the equality above only says "it is different".
        // `wget`, `scp` and `gcloud` are added in v2: since
        // `RunCommand` is really active, this list is what keeps
        // egress closed, and egress is THE red line of this policy.
        for dangerous in [
            "rm", "sudo", "curl", "wget", "chmod", "sh", "bash", "python3", "ssh", "scp", "gcloud",
            "nc",
        ] {
            assert!(
                !cfg.policy.allowed_commands.iter().any(|c| c == dangerous),
                "the command list is a demo's, not an open sandbox's: {:?}",
                cfg.policy.allowed_commands
            );
        }
        // The two other values the policy carries and that nothing read:
        // `deny_path_patterns` emptied (S2) and `max_read_bytes` multiplied by a hundred (S3)
        // both passed. The first is what keeps the keys out of
        // reach of a `ReadFile` under the home; the second bounds what a
        // single read can pull in. The defaults the file omits
        // follow its own list at load.
        assert_eq!(
            cfg.policy.deny_path_patterns,
            x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            "the deny patterns are what keep the keys out of the readable home"
        );
        assert_eq!(
            cfg.policy.max_read_bytes, 1048576,
            "1 MiB per read, no more"
        );
        assert_eq!(cfg.policy.max_list_entries, 512);
    }

    /// SECURITY: the write roots and the command list are
    /// REACHABLE — the `[actions]` switches that govern them are
    /// in the file, not inherited.
    ///
    /// Dead scaffolding, the repo's recurring defect, had come back here:
    /// `allowed_write_paths = ["$HOME"]` and `allowed_commands` were written,
    /// documented, reread — and changed nothing. Without an `[actions]` table,
    /// `AppConfig::load` merges the compiled defaults (`config.rs:546-551`),
    /// which derive each switch from the action's effect class:
    /// `FsWrite` and `ProcessSpawn` are `false`. The policy therefore announced
    /// writes across the whole home and the agent could not write one byte;
    /// `RunCommand uname -a` was refused "This action type is disabled by
    /// configuration" BEFORE `allowed_commands` was consulted (measured
    /// on the real binary, 2026-08-15, the owner's run 9).
    ///
    /// A failure in the first block means the enclave became a mute demo
    /// again. A failure in the second means we opened a
    /// surface the owner explicitly left closed on
    /// 2026-08-15: network, desktop, snapshots, destructives.
    #[test]
    fn the_enclave_write_and_command_switches_are_stated_not_inherited() {
        let text = enclave_runtime_policy();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x0ne-runtime.toml");
        std::fs::write(&path, &text).unwrap();
        let cfg = x0ne_runtime::config::AppConfig::load(&path).expect("the runtime must start");

        // What v2 opens: the four confined-path writers, and
        // execution — bounded by `allowed_commands`, tested just above.
        for on in [
            "WriteFile",
            "EditFile",
            "ApplyPatch",
            "PatchSet",
            "RunCommand",
        ] {
            assert_eq!(
                cfg.actions.get(on),
                Some(&true),
                "{on} must be enabled, otherwise the write root and the command \
                 list are dead scaffolding"
            );
        }

        // SECURITY: what v2 leaves closed, by name. The frontier of
        // this policy is EGRESS, so any action that talks to the network
        // by itself would short-circuit the command list without ever
        // crossing it. Snapshots and trash stay closed by
        // the arbitration of 2026-08-13 (invariant 7): what removes the safety net
        // always asks, and here there is nobody to ask.
        for off in [
            "HttpRequest",
            "FetchUrl",
            "WebSearch",
            "SshExec",
            "LlmCall",
            "OllamaApi",
            // The two that the compiled defaults leave `true`,
            // calling them "outbound, but without payload": an arbitration
            // right on a workstation, wrong here. A domain name carries
            // what you put in it, and a port probe reports a reachable host
            // plus one bit per attempt. These are packets that LEAVE the
            // VM. Pre-existing hole, found and closed on 2026-08-15 while writing
            // the `[actions]` table out in full.
            "DnsLookup",
            "PortCheck",
            "RunInSandbox",
            "RunContainerGroup",
            "ArtifactServe",
            "WatchAndReact",
            "CreateSnapshot",
            "RestoreSnapshot",
            "DeleteSnapshot",
            "MoveToTrash",
            "Screenshot",
            "TypeText",
            "SetClipboard",
        ] {
            assert_eq!(
                cfg.actions.get(off),
                Some(&false),
                "{off} must stay closed: {:?}",
                cfg.actions.get(off)
            );
        }
    }

    /// The transfer budget covers the MEASURED binaries, and its worst case in
    /// dollars is bounded.
    ///
    /// The first version of this test restated the derivation
    /// (`scp_budget_secs() == MAX_MIB * 1024 / MIN_KIB + HANDSHAKE`): a
    /// mutant replacing the function body with the literal `460`
    /// SURVIVED it, and had to — both expressions are equal, a
    /// derivation is not proven by its own value. What this test now
    /// says can fail: that the cap really covers what we send,
    /// and that it does not drift toward a price that would no longer be the cheapest of the
    /// pipeline. The promise made to the operator ("at most 8 min") is kept
    /// elsewhere, by `the_install_x0ne_line_says_it_uploads…`.
    ///
    /// The second version was still blind in one eye, and mutation showed it:
    /// it wrote `SCP_MIN_KIB_PER_SEC` on BOTH sides of the `>=`. Raising the
    /// floor from 512 to 51200 KiB/s — assuming a datacenter link at the
    /// operator's — divided both the budget AND the requirement by a hundred, and the test
    /// stayed green (measured 2026-08-14, mutant M23 surviving). The budget fell
    /// to 64 s for a transfer that needs 142 on a real slow link: the
    /// exact failure the constant exists to prevent, the runtime kill at
    /// 95 % of the transfer.
    ///
    /// Hence the literal below. The reference throughput is a fact of the
    /// WORLD, not of the code under test: a test that rereads the constant it judges
    /// judges nothing.
    #[test]
    fn the_upload_budget_covers_the_measured_binaries_at_the_floor_throughput() {
        // 20.2 + 20.2 = 40.5 MiB measured on this repo on 2026-08-14
        // (`target/release/x0ne` and `x0ne-runtime`), rounded up.
        const MEASURED_MIB: u64 = 41;
        // 512 KiB/s ≈ 4 Mbit/s UPSTREAM: a home ADSL line. Spelled
        // out and never read from `SCP_MIN_KIB_PER_SEC`, otherwise both
        // sides move together and the comparison is a tautology.
        const DOMESTIC_UPLINK_KIB_PER_SEC: u64 = 512;
        // The time that is NOT transfer: pushing the project key,
        // accepting the host key, resolving OS Login. A literal for the same
        // reason as the throughput — `SCP_HANDSHAKE_SECS` appeared on both sides of the
        // `>=`, where `400 + H >= 82 + H` simplifies to `400 >= 82` and stays true
        // for ANY H, zero included (measured 2026-08-14, mutants U1 and U2).
        const HANDSHAKE_FLOOR_SECS: u64 = 30;
        assert!(
            scp_budget_secs()
                >= MEASURED_MIB * 1024 / DOMESTIC_UPLINK_KIB_PER_SEC + HANDSHAKE_FLOOR_SECS,
            "the budget must cover the real {MEASURED_MIB} MiB over home ADSL, it is {} s",
            scp_budget_secs()
        );
        assert!(
            SCP_HANDSHAKE_SECS >= HANDSHAKE_FLOOR_SECS,
            "the handshake needs its own time, it is {SCP_HANDSHAKE_SECS} s"
        );
        // And the budget must CARRY this allocation on top of the worst transfer,
        // not merely declare it in a neighbouring constant: removing the
        // `+ SCP_HANDSHAKE_SECS` term from `scp_budget_secs` leaves the constant
        // intact and both assertions above green (mutant U2). Here the
        // cap and the floor are pinned independently further down, so
        // rereading them is not the tautology this test just fixed.
        assert!(
            scp_budget_secs()
                >= X0NE_BINARIES_MAX_MIB * 1024 / SCP_MIN_KIB_PER_SEC + HANDSHAKE_FLOOR_SECS,
            "the budget must add the handshake TO the transfer of the ceiling, it is {} s",
            scp_budget_secs()
        );
        assert!(
            SCP_MIN_KIB_PER_SEC <= DOMESTIC_UPLINK_KIB_PER_SEC,
            "the throughput floor must not assume better than home ADSL, it is {SCP_MIN_KIB_PER_SEC} KiB/s"
        );
        assert!(
            X0NE_BINARIES_MAX_MIB >= MEASURED_MIB,
            "the size ceiling must be a margin"
        );
        // The worst case in dollars, at the SPOT rate of the full machine.
        let worst_usd = CcMachine::FullH100.hourly_usd() * scp_budget_secs() as f64 / 3600.0;
        assert!(
            worst_usd < 1.0,
            "the cheapest step of the pipeline: {worst_usd:.2} $"
        );
    }
}
