//! `x0ne cc` — provision a confidential (TDX) VM, attest it, and say plainly
//! what was proven.
//!
//! Everything decidable is decided elsewhere: the plan is pure
//! (`x0ne_cloud::cc_plan`), the pipeline is a runner over a one-method seam
//! (`x0ne_orchestrator::cc_runner`), the quote verification is its own crate
//! (`x0ne_confidential::tdx_quote`). What is left here is the wizard: render
//! the plan, ask the human, run the steps under a budget gate, and offer the
//! exit choice.
//!
//! Three things in this file are load-bearing and easy to lose:
//!
//!   1. `BridgeExec` is the ONLY production `CcExec`. Every gcloud command
//!      goes through the ordinary runtime bridge, so it inherits the policy,
//!      the action journal and the CLI rendering the rest of the product uses
//!      — `x0ne cc` has no private path to the host.
//!   2. The pause between SSH attempts lives here rather than in the runner
//!      (see `BridgeExec::run`), while the per-command TIMEOUT is chosen by
//!      the runner and merely enforced here — it is the runner that knows a
//!      first SSH probe from a warm one.
//!   3. `run_plan` and `finish_vm` take the runner rather than doing their own
//!      I/O, because the two things worth testing in this file are "does the
//!      budget gate stop the pipeline" and "does the meter always get turned
//!      off" — neither of which is a question about a terminal.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use colored::Colorize;
use serde_json::json;
use x0ne_cloud::cc_plan::{
    build_plan, cap_status, spent_usd, status_line, CapStatus, CcConfig, CcMachine, CcStep,
    InstanceState,
};
use x0ne_orchestrator::bridge::{create_bridge, Bridge};
use x0ne_orchestrator::cc_runner::{
    render_report, render_report_json, scp_budget_secs, vllm_health_budget_secs,
    vllm_start_budget_secs, wait_ssh_budget_secs, write_last_report, CcExec, CcOutcome, CcRunner,
    CmdOut, ExecTransportDown, X0neBinaries, GPU_DRIVER_PACKAGES, LOGIND_DROPIN,
    SSH_FIRST_PROBE_TIMEOUT_SECS, SSH_PROBE_PAUSE_SECS, SSH_PROBE_TIMEOUT_SECS, SSH_RETRIES,
    TRANSFORMERS_VERSION, VLLM_PORT, VLLM_PROBE_PAUSE_SECS, VLLM_PYTHON, VLLM_VERSION,
    WAIT_SSH_DISPLAY, WAIT_VLLM_DISPLAY, X0NE_VERSION,
};
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::session::SessionLogger;
use x0ne_orchestrator::{display, terminal};

/// The single VM this command manages. Fixed, not a flag: `x0ne cc` is a
/// prover, and one name is what makes it resumable — the next run observes
/// this instance instead of creating a second one.
// Renamed with the rest of the machine surface (2026-09-18). A VM
// created under the pre-rename name is NOT resumed by this one: the owner's
// inventory held none at the time, which is what made the clean cut free.
const VM_NAME: &str = "x0ne-cc";

/// Default spend ceiling for one session, in USD.
const DEFAULT_CAP_USD: f64 = 25.0;

pub struct CcArgs {
    pub machine: Option<String>,
    pub cap: Option<f64>,
    pub yes: bool,
    /// Print the JSON report instead of the human one. The FILE is written
    /// either way — see `report_and_record`.
    pub json: bool,
}

/// `--machine` is a string at the CLI boundary and a type everywhere after.
/// Unknown values are refused, never silently defaulted: defaulting an
/// unrecognised `--machine h100` to the cheap prover would answer a question
/// the operator did not ask, and defaulting it the other way would spend
/// $11/h on a typo.
pub fn resolve_machine(arg: Option<&str>) -> Result<CcMachine, String> {
    match arg {
        None | Some("prover") => Ok(CcMachine::Prover),
        // Re-opened 2026-08-14. It was refused for one milestone, and the
        // refusal was the honest answer at the time (final review, F3): the
        // pipeline ended at CPU attestation for EVERY machine, so `--machine
        // full` would have sold the prover's own result on hardware costing
        // 15× more. That is no longer true — the full plan installs the
        // confidential-compute driver, refuses a GPU whose CC mode reads OFF,
        // serves the model out of TDX-encrypted RAM and proves the in-VM checks,
        // and every one of those steps is on screen before the gate.
        Some("full") => Ok(CcMachine::FullH100),
        Some(other) => Err(format!("unknown machine '{other}': use prover|full")),
    }
}

/// SECURITY: the budget ceiling is validated at the CLI boundary, in addition
/// to `cap_status` failing closed on the same values.
///
/// `--cap nan` and `--cap inf` both PARSE (`"nan".parse::<f64>()` is `Ok`), and
/// a NaN ceiling makes every comparison in the gate false — it disarmed the
/// cap entirely (review round 1, 2026-08-13, measured). `cap_status` now
/// refuses non-finite caps, so this check is not what makes the gate safe; it
/// exists so the operator is told their ceiling is nonsense BEFORE four gcloud
/// probes and a plan, instead of watching the pipeline die at step 1 with
/// "spend cap reached".
pub fn validate_cap(cap: f64) -> Result<f64, String> {
    if !cap.is_finite() {
        return Err(format!(
            "--cap {cap} is not a usable amount of money: pass a finite number of dollars, \
             e.g. --cap 25"
        ));
    }
    if cap <= 0.0 {
        return Err(format!(
            "--cap {cap:.2} leaves no budget to spend, so nothing can run: pass a positive \
             number of dollars, e.g. --cap 25"
        ));
    }
    Ok(cap)
}

/// #47d: every error printed on the cc path may embed text a remote party
/// wrote (gcloud relays Google's API messages; a rescue error can carry VM
/// output). `render_report` sanitizes its own surface — these direct
/// `println!` sites must too, or an ANSI escape in an API error repaints the
/// very lines the operator is reading. Weak risk (Google-controlled, not
/// VM-controlled), closed anyway: the cost is one call.
fn clean(e: impl std::fmt::Display) -> String {
    x0ne_orchestrator::text_safety::sanitize(&e.to_string())
}

/// The one sentence `Report` owes the operator when the cap blew after the
/// verdict. `None` while under the cap: the step stays silent, and it
/// NEVER fails on money — the proof is the artefact the money bought.
fn report_step_cap_note(spent_usd: f64, cap_usd: f64) -> Option<String> {
    matches!(cap_status(spent_usd, cap_usd), CapStatus::Exceeded).then(|| {
        format!(
            "⚠ cap exceeded during the run (≈ {spent_usd:.2} $ / {cap_usd:.2} $) — \
             the report stands; the VM stops on the usual path"
        )
    })
}

/// One plan line per step. Exhaustive on purpose (no `_` arm): a step added
/// to `CcStep` without a line here would execute unannounced, and what the
/// human approves is exactly this text.
///
/// `bins` is the resolution `run_cc` performed BEFORE printing the plan, and
/// it is threaded down here for repo invariant 5: `InstallX0ne` uploads two
/// files into an attested enclave, so the line that gets approved names WHICH
/// two, by path. `None` is "nobody resolved any" — the prover machine and the
/// plan-rendering tests — and then the line says what the step does without
/// pretending to know what it will send.
fn step_line(cfg: &CcConfig, step: &CcStep, bins: Option<&BinariesChoice>) -> String {
    match step {
        // Verbatim, not a summary. Repo invariant 5: approving what you
        // cannot read is not approving — and `create_vm_runs_the_exact_
        // displayed_argv` (cc_runner) pins that this string and the argv the
        // runner sends come from the same `VmSpec`.
        CcStep::CreateVm => cfg.machine.vm_spec(&cfg.vm_name).to_display_command(),
        CcStep::StartVm => format!(
            "gcloud compute instances start {} --zone={}",
            cfg.vm_name,
            cfg.machine.vm_spec(&cfg.vm_name).zone
        ),
        // Derived from the constants, never a typed-in duration: this line is
        // a promise about wall clock made to someone paying by the hour, and
        // the hand-written "~5 min" it replaces went on being printed while
        // the real ceiling was 2.6 h (review round 1).
        CcStep::WaitSsh => format!(
            "wait for SSH ({SSH_RETRIES} probes, {SSH_FIRST_PROBE_TIMEOUT_SECS} s for the first \
             then {SSH_PROBE_TIMEOUT_SECS} s, {SSH_PROBE_PAUSE_SECS} s apart — at most {} min)",
            wait_ssh_budget_secs().div_ceil(60)
        ),
        CcStep::AttestTdx => {
            "over SSH: read a TDX quote from configfs-tsm and verify it against a fresh nonce"
                .into()
        }
        // The full-machine lines. Short, but each says what runs and where, so
        // the plan the human approves still describes the whole run — repo
        // invariant 5. They get their exact commands as the runner steps land.
        // Says "reboots" because it does, and because that is the part an
        // operator watching the terminal must not mistake for a hang: the
        // LKCA modprobe rule the confidential GPU needs only takes effect from
        // a rebuilt initramfs. Repo invariant 5 — a step that reboots the
        // machine and announces itself as "install a package" is not what was
        // approved.
        CcStep::InstallGpuDriver => format!(
            "over SSH: install the NVIDIA driver ({GPU_DRIVER_PACKAGES}) and the LKCA modprobe \
             rule, then REBOOT the VM and wait for it to come back (idempotent — skipped when \
             nvidia-smi already answers)"
        ),
        CcStep::CheckGpuCc => {
            "over SSH: read the GPU confidential-compute mode; OFF is refused, never forced".into()
        }
        CcStep::AttestGpu => {
            "over SSH: record the GPU confidential-compute mode and driver, from inside the \
             attested CPU enclave"
                .into()
        }
        // Says ARM, and says that it comes after the attestation, because both
        // halves are what an operator is approving: a command that turns the
        // GPU on, and the fact that it only runs once the GPU has been
        // attested. Repo invariant 5 — a mutation announced as a check is not
        // what was approved.
        CcStep::ArmGpu => {
            "over SSH: declare the ATTESTED GPU ready (conf-compute -srs 1) so CUDA may \
             initialise on it — per boot, and only after the attestation above"
                .into()
        }
        // The transformers pin is part of what is approved, not an install
        // detail: vLLM 0.10.0 does not bound it, and the version the resolver
        // would otherwise pick kills the server at startup (measured
        // 2026-08-14). An operator reading "install vLLM 0.10.0" alone would
        // not be reading the stack that was proven.
        CcStep::InstallVllm => format!(
            "over SSH: install vLLM {VLLM_VERSION} with transformers {TRANSFORMERS_VERSION} via uv \
             on python {VLLM_PYTHON} (all pinned — the stack measured to serve correctly; \
             idempotent, skipped when that exact version answers from that exact interpreter)"
        ),
        // Repo invariant 5 — what is approved must be readable. The drop-in is
        // the only persistent SYSTEM configuration this pipeline writes outside
        // the vLLM unit, so it is named here rather than hidden inside "prepare
        // /dev/shm": an operator who reads this line has to be able to undo it.
        // The path comes from the constant the script writes, never a copy of
        // it: a literal here lets the plan announce a file the pipeline no
        // longer touches, and the mutation that renames the constant proved
        // both suites stay green through exactly that divergence.
        CcStep::FetchModel => format!(
            "over SSH: prepare /dev/shm for the weights — TDX-encrypted RAM, never the disk — \
             refuse a tmpfs too small to hold them, and write {LOGIND_DROPIN} (RemoveIPC=no) + \
             restart systemd-logind, without which logind deletes the weights when an ssh \
             session closes"
        ),
        // Says "waits", and says how long, because that is the part an operator
        // watching a 3.72 $/h terminal must not mistake for a hang: the first
        // start downloads ~15 GB of weights into RAM before anything listens.
        // Repo invariant 5 — a step announced as "start a service" that then
        // spends half an hour is not what was approved.
        //
        // And it says what the wait WATCHES since run 4 (2026-08-14). The line
        // used to promise a wait for the port, which is exactly what burned 30
        // min and 1.26 $ on a service that was crash-looping: the ceiling was
        // true and the sentence was misleading. What is approved now is a wait
        // that gives up early on a restart counter that climbs or a tmpfs that
        // stays empty.
        CcStep::ServeVllm => format!(
            "over SSH: write a systemd unit serving the model on 127.0.0.1:{VLLM_PORT} only, \
             then wait for it to bind while watching the unit's restart counter and the weights \
             landing in RAM — a crash loop or an empty tmpfs stops the wait instead of spending \
             it (first start downloads ~15 GB into RAM — at most {} min)",
            vllm_start_budget_secs().div_ceil(60)
        ),
        // Three GUARANTEES and one OBSERVATION, said apart (review round 1,
        // minor m4). The first three can fail and stop the run; the disk one
        // cannot — GCP encrypts every persistent disk at rest with no way to
        // turn it off, so listing it as a fourth thing that "must hold" sells
        // a check that can never fire. What the describe really buys is whose
        // key it is, and the report line says what that is worth.
        CcStep::ValidateEncryption => format!(
            "over SSH and gcloud: prove swap is off, {VLLM_PORT} is loopback-only and no \
             firewall rule opens it — any of the three failing stops the run — then record \
             which key encrypts the boot disk"
        ),
        CcStep::HealthCheck => format!(
            "over SSH: ask 127.0.0.1:{VLLM_PORT}/v1/models for the served model (at most {} min)",
            vllm_health_budget_secs().div_ceil(60)
        ),
        // Says what the step really does now that it does something (task 6).
        // Four things an operator must be able to refuse by reading this line:
        // that we UPLOAD binaries from this machine, that the enclave's x0ne is
        // pointed at the local vLLM and at nothing else, that the runtime
        // policy it gets is deny-by-default rather than the permissive one
        // their own workstation runs — and, since the owner's arbitration of
        // 2026-08-15, exactly WHAT that policy grants. "Deny by default" alone
        // stopped being the whole truth the day the policy started enabling
        // writes and command execution: an operator reading only the old line
        // would approve a demo agent and get one that can edit their VM's home.
        // The grant is three clauses because it has three independent limits —
        // which commands (read-only ones), where (the home), and what never
        // happens (egress).
        CcStep::InstallX0ne => format!(
            "gcloud scp: copy x0ne and x0ne-runtime {X0NE_VERSION} into the VM (at most {} \
             min), then write ~/.x0ne/config.toml pointing at the local vLLM on \
             127.0.0.1:{VLLM_PORT} and a runtime policy confined to the VM's home, deny by \
             default — read-only commands, home-wide reads/writes, no egress{}",
            scp_budget_secs().div_ceil(60),
            bins.map(BinariesChoice::plan_suffix).unwrap_or_default()
        ),
        CcStep::Report => "print the report (no cloud call)".into(),
    }
}

/// The whole plan as one block, built as a `String` so the thing shown to the
/// human before the gate is the thing a test can read.
pub fn render_plan(cfg: &CcConfig, plan: &[CcStep], bins: Option<&BinariesChoice>) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(s, "── plan ──────────────────────────────────────────────").ok();
    for (i, step) in plan.iter().enumerate() {
        writeln!(s, "  {}. {}", i + 1, step_line(cfg, step, bins)).ok();
    }
    writeln!(s, "  {}", status_line(cfg.machine, 0.0, cfg.cap_usd)).ok();
    writeln!(s, "──────────────────────────────────────────────────────").ok();
    s
}

/// What to do with the VM once the pipeline is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitChoice {
    Keep,
    Stop,
    Destroy,
    /// Open a shell into the VM running `x0ne` — the point of Increment 3.
    ///
    /// SECURITY: offered ONLY over a `Proven` outcome on the full machine, and
    /// refused by the parser otherwise. The spec's rule for the prover is "no
    /// path to a chat without a green verdict"; an `enter` printed under
    /// ATTESTATION REFUSED would be exactly that path, with the operator
    /// believing the enclave behind it was the one the report described.
    Enter,
}

/// SECURITY: fail closed. Only an explicit `y`/`yes` approves. EOF (which
/// `read_line_raw` reports as `/quit`), an empty line, and anything
/// unrecognised are refusals — the same rule as `agent/confirm.rs`, whose
/// whole subject is that absence of an answer is never an approval.
fn parse_confirm(line: &str) -> bool {
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// SECURITY: fail closed, and note what "closed" means for money. Keeping the
/// VM burns $/h and destroying it is irreversible, so the safe answer to
/// "nobody said anything" is `Stop`: billing ends, the disk survives, and the
/// next `x0ne cc` resumes from it. Only an explicit `d`/`destroy` reaches
/// `Destroy`, and even then a second confirmation stands behind it.
///
/// SECURITY: `offer_enter` is the PROOF, carried down to the parser. `e` on a
/// run that was not proven does not fall through to some milder reading of the
/// operator's intent — it lands on `Stop`, the fail-safe of this function,
/// because the one thing it must never do is open a session into an enclave
/// nothing vouched for. The menu does not print the option either; both are
/// checked, because a menu that hides an option a parser still accepts is a
/// gate that exists only on screen.
fn parse_exit_choice(line: &str, offer_enter: bool) -> ExitChoice {
    match line.trim().to_lowercase().as_str() {
        "k" | "keep" => ExitChoice::Keep,
        "d" | "destroy" => ExitChoice::Destroy,
        "e" | "enter" if offer_enter => ExitChoice::Enter,
        _ => ExitChoice::Stop,
    }
}

/// Ask the human, once. Named "one key" because that is the whole
/// interaction: `y` + Enter. Line-based rather than raw-mode for the same
/// reason as `terminal::confirm_action` — this command shares its terminal
/// with gcloud, which expects a cooked tty.
fn confirm_one_key(question: &str) -> Result<bool> {
    if !terminal::can_prompt_interactively() {
        // No tty on both ends: piped, headless, CI. There is nobody to ask,
        // so this is a refusal — never a default yes. Generic wording on
        // purpose: this helper guards both the plan and the deletion, and a
        // message naming one of them would be a lie at the other call site.
        println!(
            "  {} no interactive terminal — refusing",
            "✗".truecolor(220, 80, 80)
        );
        return Ok(false);
    }
    print!("  {question} [y/N] ❯ ");
    terminal::flush_stdout();
    Ok(parse_confirm(&terminal::read_line_raw()?))
}

fn ask_keep_stop_destroy(hourly_usd: f64, offer_enter: bool) -> Result<ExitChoice> {
    if !terminal::can_prompt_interactively() {
        return Ok(ExitChoice::Stop);
    }
    if offer_enter {
        println!(
            "  {}  enter the enclave — x0ne talks to the local vLLM, nothing leaves the VM",
            "e".truecolor(0, 200, 220).bold(),
        );
    }
    println!(
        "  {}  keep (≈ {hourly_usd:.2} $/h) · {}top billing (default) · {}estroy",
        "k".truecolor(0, 180, 220).bold(),
        "s".truecolor(0, 200, 120).bold(),
        "d".truecolor(220, 80, 80).bold(),
    );
    print!("  choice [{}k/S/d] ❯ ", if offer_enter { "e/" } else { "" });
    terminal::flush_stdout();
    Ok(parse_exit_choice(&terminal::read_line_raw()?, offer_enter))
}

/// The command that puts the operator inside the enclave, as argv.
///
/// A pure function so the thing a test reads is the thing that gets executed.
/// `-t` after the `--` is the load-bearing flag: it forces a PTY on the remote
/// side, and x0ne is a terminal program that reads keys — without it the
/// session comes up with no tty and the REPL exits on its first read.
fn enclave_ssh_argv(cfg: &CcConfig) -> Vec<String> {
    vec![
        "compute".into(),
        "ssh".into(),
        cfg.vm_name.clone(),
        format!("--zone={}", cfg.machine.vm_spec(&cfg.vm_name).zone),
        "--".into(),
        "-t".into(),
        // Relative to the home directory, where `InstallX0ne` put it: an ssh
        // command starts in $HOME, and the home directory is not on the PATH of
        // a non-interactive shell (the reason `VLLM_INSTALL_SCRIPT` spells out
        // `$HOME/.local/bin/uv`).
        "./x0ne".into(),
    ]
}

/// The line shown before the session opens, from the argv that opens it.
///
/// The brief asks that `x0ne cc` PRINT the command and then run it, and a pure
/// function is the only way a test can hold the printed text to the executed
/// argv: taken from the same `Vec`, they cannot drift into a display that
/// flatters a command nobody sent.
fn enclave_ssh_display(argv: &[String]) -> String {
    format!("gcloud {}", argv.join(" "))
}

/// Show the command, THEN run it — in one function, so nothing can do the
/// second without the first.
///
/// The seam exists because the brief's requirement is an ORDER ("print the
/// command then RUN it"), and an order is not provable against a bare
/// `println!` inside a function that spawns a process: deleting the line left
/// every test green (measured 2026-08-14, mutant S10). With the writer and the
/// spawn both passed in, a test can assert that the operator had the command in
/// front of them BEFORE the session opened — which is the whole point of
/// printing it, on the one action in `x0ne cc` that hands a terminal to a remote
/// machine.
fn show_and_run_enclave(
    w: &mut dyn std::io::Write,
    argv: &[String],
    run: &mut dyn FnMut(&[String]) -> Result<()>,
) -> Result<()> {
    writeln!(
        w,
        "\n  {}\n",
        enclave_ssh_display(argv).truecolor(110, 120, 135)
    )?;
    run(argv)
}

/// Where the report lands inside the VM, relative to the remote home.
///
/// Relative for the reason `enclave_ssh_argv` uses `./x0ne`: an ssh command
/// starts in `$HOME`, and the in-VM x0ne reads `~/.x0ne/cc/last-report.json`
/// through `dirs::home_dir()`. The two agree because they name the same place
/// from the two sides.
const ENCLAVE_REPORT_REMOTE: &str = ".x0ne/cc/last-report.json";

/// The two commands that carry this run's report into the VM, as argv.
///
/// # Why the report has to travel at all
///
/// `x0ne cc` runs on the operator's laptop and writes its report there. The
/// session `enter` opens is `gcloud compute ssh … -- -t ./x0ne`: a different
/// process, on the machine that was attested, with no access to the laptop's
/// disk. The in-VM x0ne derives its enclave badge from a report file
/// (`EnclaveStatus::for_session`), so without this copy the badge inside a
/// proven enclave reads `confidential GPU disabled` — which is how it read for the
/// whole time the indicator had no producer at all.
///
/// SECURITY: `scp` does not create directories, so the `mkdir` is first and
/// both are pure argv — two constants, `cfg.vm_name` (itself the fixed
/// `VM_NAME`), the zone from the machine spec, and a local path this program
/// computed. No remote text, no model output and no operator-typed string
/// reaches either line, which is the same argument `enter_enclave` makes for
/// the ssh it runs.
fn enclave_report_upload_argv(cfg: &CcConfig, local: &std::path::Path) -> [Vec<String>; 2] {
    let zone = format!("--zone={}", cfg.machine.vm_spec(&cfg.vm_name).zone);
    [
        vec![
            "compute".into(),
            "ssh".into(),
            cfg.vm_name.clone(),
            zone.clone(),
            "--".into(),
            "mkdir".into(),
            "-p".into(),
            ".x0ne/cc".into(),
        ],
        vec![
            "compute".into(),
            "scp".into(),
            local.to_string_lossy().into_owned(),
            format!("{}:{ENCLAVE_REPORT_REMOTE}", cfg.vm_name),
            zone,
        ],
    ]
}

/// Carry the report in, and say so if it does not arrive.
///
/// Failures are NOT fatal and must not be: the enclave was proven, the report
/// is on screen, and the operator asked to go in. What a failed copy costs is
/// the badge — the in-VM session will read `confidential GPU disabled`, which is the
/// honest thing for a session that cannot show its evidence. Turning a failed
/// `scp` into a refusal to enter would trade a true report for no session.
fn carry_report_into_enclave(
    cfg: &CcConfig,
    run: &mut dyn FnMut(&[String]) -> Result<()>,
) -> Result<()> {
    upload_report(cfg, &write_last_report_path()?, run)
}

/// The two commands, in order, on a report path the caller already resolved.
///
/// Split from the resolution so a test can watch the ORDER without a report
/// file on the machine running it: `scp` does not create directories, so a
/// `mkdir` that stops running is a copy that stops arriving, and the badge
/// goes red for a reason nothing in the argv builder can show.
fn upload_report(
    cfg: &CcConfig,
    local: &std::path::Path,
    run: &mut dyn FnMut(&[String]) -> Result<()>,
) -> Result<()> {
    for argv in enclave_report_upload_argv(cfg, local) {
        run(&argv)?;
    }
    Ok(())
}

/// The report file this run wrote, or an error naming why there is none.
fn write_last_report_path() -> Result<std::path::PathBuf> {
    let path = x0ne_orchestrator::cc_runner::last_report_path()
        .ok_or_else(|| anyhow::anyhow!("no home directory: there is no report to carry"))?;
    if !path.exists() {
        anyhow::bail!("no report at {}", path.display());
    }
    Ok(path)
}

/// Run that command in the FOREGROUND, inheriting this terminal.
///
/// SECURITY, and it is the one command in `x0ne cc` that does not go through
/// the bridge: `RunCommand` captures stdout and gives the child no tty
/// (`executor/process.rs`), which is right for every gcloud call this pipeline
/// makes and impossible for an interactive session. What justifies the
/// exception is what the argv is made of — two constants and `cfg.vm_name`,
/// itself the fixed `VM_NAME` — plus the fact that it runs only after an
/// explicit keypress, on a run whose enclave was proven. No remote text, no
/// model output and no operator-typed string reaches this line.
///
/// Errors are returned rather than propagated out of the menu: the VM is still
/// running, and a failed `gcloud ssh` must land the operator back on the exit
/// choice — where they can stop the meter — instead of exiting the program with
/// a VM billing behind it.
fn enter_enclave(cfg: &CcConfig) -> Result<()> {
    let mut gcloud = |argv: &[String]| -> Result<()> {
        let status = std::process::Command::new("gcloud").args(argv).status()?;
        if !status.success() {
            anyhow::bail!("the session ended with {status}");
        }
        Ok(())
    };
    if let Err(e) = carry_report_into_enclave(cfg, &mut gcloud) {
        println!(
            "  {} the attestation report could not be carried in ({e}); the session's \n     enclave badge will read \"confidential GPU disabled\"",
            "·".truecolor(110, 120, 135)
        );
    }
    show_and_run_enclave(&mut std::io::stdout(), &enclave_ssh_argv(cfg), &mut gcloud)
}

/// Where the meter stands when the report is printed.
///
/// An enum rather than the `&str` the sketch used: the exit choice branches
/// on this, and `vm_state == "running"` is one typo away from offering
/// "destroy" for a VM that was never created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VmEnd {
    /// No step ever brought it up — nothing is billing, nothing to offer.
    NeverUp,
    Running,
    Stopped,
    /// We asked it to stop and gcloud refused. The operator must know: this
    /// is the one state where the meter is still running and nobody expects
    /// it to be.
    StopFailed,
    /// The bridge to `x0ne-runtime` died mid-run, so the stop this run owed
    /// could not travel through it — and a fresh runtime, brought up for that
    /// single command, stopped the VM.
    ///
    /// A state of its own rather than plain `Stopped`, because the two are not
    /// the same news: the meter is off either way, but this one also says the
    /// runtime died under the pipeline, which is the fact an operator needs to
    /// go look at `~/.x0ne/logs/core.log` for.
    StoppedAfterBridgeDeath,
    /// `create`/`start` failed AND the follow-up observation also failed, so
    /// we genuinely do not know. Reached only when both questions go
    /// unanswered — a failed start alone is settled by `observe_instance`.
    Uncertain,
}

impl VmEnd {
    /// The CLAIM about the meter, with no remediation text mixed in.
    ///
    /// Split from the lever (review round 2, 2026-08-13) so the property that
    /// matters can be asserted precisely. The test says "a state that may
    /// still be billing must not read as stopped"; once the lever
    /// `gcloud compute instances stop …` was appended to the same string, a
    /// substring assertion over the whole label was testing the command text
    /// as much as the verdict, and would have to be weakened to keep passing.
    /// Tightening the target beats weakening the property.
    fn verdict(self) -> &'static str {
        match self {
            VmEnd::NeverUp => "not running (no step brought it up)",
            VmEnd::Running => "running",
            VmEnd::Stopped => "stopped",
            VmEnd::StopFailed => "STILL RUNNING — the stop command failed",
            VmEnd::StoppedAfterBridgeDeath => {
                "stopped through a fresh runtime — the bridge to x0ne-runtime had died"
            }
            VmEnd::Uncertain => "unknown — create/start failed and the follow-up probe failed too",
        }
    }

    /// The command that resolves this state, when there is one.
    ///
    /// Review round 1, finding I4: the most expensive state in the program was
    /// the only one without an operator lever, which is precisely backwards.
    fn lever(self, cfg: &CcConfig) -> Option<String> {
        let spec = cfg.machine.vm_spec(&cfg.vm_name);
        match self {
            // The command we issued ourselves, rendered — not a retyped one.
            // Retyped, it lost `--discard-local-ssd` and handed the operator a
            // stop that fails on an a3 exactly the way ours did (2026-08-14).
            VmEnd::StopFailed => Some(format!("retry: {}", spec.to_stop_command())),
            VmEnd::Uncertain => Some(format!(
                "check: gcloud compute instances list --filter=name={}",
                cfg.vm_name
            )),
            // Nothing to fix about the meter — it is off. What the operator
            // may still want to look at (why the runtime died) is a log, not a
            // command this function can name.
            VmEnd::NeverUp | VmEnd::Running | VmEnd::Stopped | VmEnd::StoppedAfterBridgeDeath => {
                None
            }
        }
    }

    /// What the report prints: the verdict, then the lever if one exists.
    fn label(self, cfg: &CcConfig) -> String {
        match self.lever(cfg) {
            Some(lever) => format!("{}; {lever}", self.verdict()),
            None => self.verdict().to_string(),
        }
    }
}

/// What an observation of the instance says about the meter. Pure.
///
/// `None` means the probe itself failed — a question that went unanswered, not
/// an answer. Two callers rely on this: the failed-create path (review round 1,
/// finding I5) and the Ctrl+C path (final review, F1), which face the same
/// question — "is anything billing right now?" — and must not answer it
/// differently.
fn settle_vm_end(observed: Option<&InstanceState>) -> VmEnd {
    match observed {
        Some(InstanceState::Running { .. }) => VmEnd::Running,
        Some(InstanceState::Stopped { .. }) => VmEnd::Stopped,
        Some(InstanceState::Absent) => VmEnd::NeverUp,
        // Mid-transition is the case that can still end up RUNNING and
        // billing, so it is an unknown, not a reassurance.
        Some(InstanceState::Transitional { .. }) | None => VmEnd::Uncertain,
    }
}

/// Ask the cloud what is running and turn the meter off if anything is.
///
/// The whole Ctrl+C contract in one place (final review, F1): the spec asks
/// for a recap "whatever the path (success, failure, Ctrl+C, cap)", and the
/// milestone checklist says "Ctrl+C during attestation → the VM is stopped".
/// Takes the runner, so a scripted exec can prove it without a signal.
async fn settle_after_interrupt(runner: &mut CcRunner<'_>, cfg: &CcConfig) -> VmEnd {
    match settle_vm_end(runner.observe_instance().await.ok().as_ref()) {
        VmEnd::Running => stop_and_report(runner, cfg).await,
        settled => settled,
    }
}

/// Stop the VM and report honestly whether it worked.
///
/// One function for every stop in this file (review round 1, finding I3): the
/// interactive `Stop` branch — the DEFAULT answer — used `?` and propagated
/// the raw error, so the single most likely way to leave a meter running was
/// also the one path that never printed "STILL RUNNING".
async fn stop_and_report(runner: &mut CcRunner<'_>, cfg: &CcConfig) -> VmEnd {
    match runner.stop_vm().await {
        Ok(()) => {
            println!("  VM stopped · billing stopped");
            VmEnd::Stopped
        }
        Err(e) => {
            println!(
                "  {} stop failed: {}",
                "✗".truecolor(220, 80, 80),
                clean(&e)
            );
            println!("  {}", VmEnd::StopFailed.label(cfg).truecolor(220, 80, 80));
            VmEnd::StopFailed
        }
    }
}

/// SECURITY (money): the safety stop, issued through a runtime that is NOT the
/// one that died.
///
/// The failure mode this closes is the whole of run 1 of the H100 milestone
/// (2026-08-14): the bridge died, the pipeline failed, and the stop that a
/// failed run owes the operator was sent down the same dead pipe — so it failed
/// too, and a 3.72 $/h machine went on billing behind a report that could only
/// say STILL RUNNING. The spec's promise is that the cap protects an operator
/// who is not watching; a budget net that dies with its transport does not.
///
/// ONE fresh bridge, ONE stop on it. If that fails as well the honest answer is
/// the existing `StopFailed` — verdict plus lever — and not a third runtime: a
/// resurrection loop would spend an unbounded number of processes on a machine
/// that may not be reachable at all.
///
/// `spawn` is a factory rather than a bridge because it must be called ONLY
/// when the rescue is due — bringing up a second runtime on every run would
/// cost a process for nothing. It is also what makes this testable: the half
/// that cannot be tested (a real `create_bridge` spawning a real process)
/// stays in `run_cc`, in one line.
async fn rescue_stop_if_the_bridge_died<F, Fut>(
    vm: VmEnd,
    transport_down: bool,
    cfg: &CcConfig,
    spawn: F,
) -> VmEnd
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Box<dyn Bridge>>>,
{
    // Both halves are load-bearing. `transport_down` alone would re-stop a VM
    // the pipeline already stopped; `vm == Running` alone would respawn a
    // runtime for an ordinary failure, whose stop went through perfectly well
    // on the bridge it already had.
    if vm != VmEnd::Running || !transport_down {
        return vm;
    }
    println!(
        "\n  {} the bridge to x0ne-runtime died — starting a fresh one to stop the VM",
        "⚠".truecolor(220, 180, 60)
    );
    let mut fresh = match spawn().await {
        Ok(b) => b,
        // No runtime at all: say what is true. The VM is up, we could not turn
        // it off, and the operator has the command.
        Err(e) => {
            println!(
                "  {} no runtime could be started to stop it: {}",
                "✗".truecolor(220, 80, 80),
                clean(&e)
            );
            println!("  {}", VmEnd::StopFailed.label(cfg).truecolor(220, 80, 80));
            return VmEnd::StopFailed;
        }
    };
    let mut exec = BridgeExec {
        bridge: fresh.as_mut(),
        logger: None,
    };
    let mut runner = CcRunner::new(&mut exec, cfg.clone());
    match runner.stop_vm().await {
        Ok(()) => {
            println!("  VM stopped through a fresh runtime · billing stopped");
            VmEnd::StoppedAfterBridgeDeath
        }
        Err(e) => {
            println!(
                "  {} the fresh runtime could not stop it either: {e}",
                "✗".truecolor(220, 80, 80)
            );
            println!("  {}", VmEnd::StopFailed.label(cfg).truecolor(220, 80, 80));
            VmEnd::StopFailed
        }
    }
}

/// The pipeline: budget gate, steps, and the stop that a broken run owes the
/// operator. Returns where the meter ended up and what failed, if anything.
///
/// Takes the runner instead of building one, and does not ask anything, so a
/// scripted `CcExec` can drive the whole loop in a unit test. Before this was
/// extracted, the most expensive logic in the command had no test at all.
///
/// The plan is a slice and stays one, INCLUDING after the post-SSH re-probe
/// (2026-08-14). What the re-probe produces is a set of steps to skip, not a
/// new list: `in_flight` publishes an index into this exact slice for the
/// Ctrl+C path to name the interrupted step, and a plan that shrank under it
/// would make that name wrong — the interrupt report is the one place where
/// being approximately right about what was running is worse than useless.
async fn run_plan(
    runner: &mut CcRunner<'_>,
    cfg: &CcConfig,
    plan: &[CcStep],
    vm_initial: VmEnd,
    started: Instant,
    in_flight: &AtomicUsize,
) -> (VmEnd, Option<(CcStep, String)>) {
    let mut vm = vm_initial;
    let mut failed: Option<(CcStep, String)> = None;
    // The steps still to run, as re-planned once ssh answered. `None` until
    // then, meaning "run everything that was approved".
    let mut remaining: Option<std::collections::VecDeque<CcStep>> = None;
    // #47e: each warning threshold speaks ONCE. The gate runs before every
    // step, so past 50% every remaining step reprinted the same line — a
    // warning that repeats becomes wallpaper, and the 80% line must still
    // land as news.
    let mut warned = (false, false); // (50%, 80%)

    for (i, step) in plan.iter().enumerate() {
        // Resume-by-detection, applied to a machine that came back from a SPOT
        // preemption. `remaining` is a SUBSEQUENCE of the approved plan
        // (`CcRunner::replan_after_ssh` guarantees it), so matching it greedily
        // against the approved steps is exact: a step at the front is due, a
        // step that is not is one the VM already carries.
        if let Some(rem) = remaining.as_mut() {
            if rem.front() == Some(step) {
                rem.pop_front();
            } else {
                println!(
                    "  {} {step:?} — already done on this VM, skipped",
                    "✓".truecolor(0, 200, 120)
                );
                continue;
            }
        }
        // Published before the step runs, so that if this future is dropped
        // mid-step (Ctrl+C, below) the caller can still name what was in
        // flight. An index into `plan`, not shared mutable state in any
        // meaningful sense — it is write-here, read-there, and it survives the
        // drop precisely because it is NOT inside the future.
        in_flight.store(i, Ordering::Relaxed);
        // The budget gate guards SPENDING, and `Report` spends nothing — it
        // is declared "no cloud call" in the plan and issues none. Gating it
        // meant a run that had already earned a verdict could cross the cap in
        // the seconds after `AttestTdx` and have its proof converted into
        // `Failed{Report}` (final review, F4): the money was spent, the
        // attestation succeeded, and the one artefact the operator paid for
        // was thrown away by the gate meant to protect them. Skipping it costs
        // nothing — the VM is stopped immediately after, on the same paths as
        // before.
        if !matches!(step, CcStep::Report) {
            let spent = spent_usd(cfg.machine.hourly_usd(), started.elapsed().as_secs());
            match cap_status(spent, cfg.cap_usd) {
                CapStatus::Exceeded => {
                    // Do not announce a stop that will not happen: the cap can
                    // fire on the very first step, before anything is billing,
                    // and a line claiming an action nobody took is how an
                    // operator learns to stop reading these.
                    let tail = if vm == VmEnd::Running {
                        " — stopping the VM"
                    } else {
                        " — nothing was started"
                    };
                    println!(
                        "  {} cap reached (≈ {spent:.2} $ / {:.2} $){tail}",
                        "✗".truecolor(220, 80, 80),
                        cfg.cap_usd
                    );
                    failed = Some((step.clone(), "spend cap reached".into()));
                    break;
                }
                CapStatus::Warn80 if !warned.1 => {
                    warned.1 = true;
                    println!("  ⚠ 80% of the cap (≈ {spent:.2} $)");
                }
                CapStatus::Warn50 if !warned.0 => {
                    warned.0 = true;
                    println!("  ⚠ 50% of the cap (≈ {spent:.2} $)");
                }
                CapStatus::Warn80 | CapStatus::Warn50 | CapStatus::Ok => {}
            }
        } else {
            // `Report` sits outside the gate (a proof already earned is
            // never thrown away for money spent), but the gate was the only
            // place that PRINTED the cap — so a cap crossed in the seconds
            // after `AttestTdx` produced a Proven report, a stopped VM, and a
            // "keep?" menu without one word about the blown budget. Message
            // only: the step never fails on it.
            let spent = spent_usd(cfg.machine.hourly_usd(), started.elapsed().as_secs());
            if let Some(note) = report_step_cap_note(spent, cfg.cap_usd) {
                println!("  {note}");
            }
        }

        match runner.run_step(step).await {
            Ok(()) => {
                // These two are what put the meter in motion; nothing else
                // does. Tracked so a later failure knows what to stop.
                if matches!(step, CcStep::CreateVm | CcStep::StartVm) {
                    vm = VmEnd::Running;
                }
                // The first moment the in-VM state is BOTH readable and worth
                // reading: ssh answers (WaitSsh) and the kernel that answers
                // has a verified quote (AttestTdx). Hooked to the second, not
                // the first (review round 1, finding I3) — what is read here
                // removes steps from an approved plan, so it must come from a
                // machine the pipeline has vouched for. See
                // `CcRunner::replan_after_ssh`.
                if matches!(step, CcStep::AttestTdx) {
                    remaining = Some(runner.replan_after_ssh(&plan[i + 1..]).await.into());
                }
            }
            Err(e) => {
                if vm == VmEnd::NeverUp && matches!(step, CcStep::CreateVm | CcStep::StartVm) {
                    // Do not guess (review round 1, finding I5). A failed
                    // `start` is NOT the same as a stopped VM: our timeout can
                    // fire while GCE goes on booting the instance, which then
                    // bills. The cloud knows; ask it. Only if that question
                    // also goes unanswered do we admit we do not know.
                    vm = settle_vm_end(runner.observe_instance().await.ok().as_ref());
                }
                failed = Some((step.clone(), e.to_string()));
                break;
            }
        }
    }

    // Owner decision 2026-08-13: stop at the first failure — never leave the
    // meter running on a broken pipeline. This is also why a failed run gets
    // no exit menu: the safe answer was already applied.
    //
    // Except through a dead transport. The stop cannot travel down the pipe
    // that just died, and issuing it anyway achieves nothing but a second
    // error and a `StopFailed` on a VM that could still have been stopped
    // (2026-08-14). The still-Running state is handed back to the caller,
    // which is the layer that can start a fresh runtime — see
    // `rescue_stop_if_the_bridge_died`, which `run_cc` calls on this exact
    // pair (Running + transport down).
    if failed.is_some() && vm == VmEnd::Running && !runner.transport_is_down() {
        vm = stop_and_report(runner, cfg).await;
    }

    (vm, failed)
}

/// The three human-facing decisions of the exit menu, injected so the
/// money-relevant behaviour is testable: does `--yes` stop the meter, does a
/// refused deletion leave the VM alone, and does `enter` ever run on a VM whose
/// enclave was not proven?
///
/// A struct rather than three more parameters, and not only for the argument
/// count: they are one thing — the terminal — and production passes the same
/// three every time.
struct ExitUi<'a> {
    ask: &'a mut dyn FnMut(f64, bool) -> Result<ExitChoice>,
    confirm: &'a mut dyn FnMut(&str) -> Result<bool>,
    enter: &'a mut dyn FnMut(&CcConfig) -> Result<()>,
}

/// The exit choice, asked in a LOOP because `enter` returns.
///
/// `offer_enter` is the proof, and it is a parameter rather than something
/// recomputed here: `run_cc` derives it from the outcome it just rendered, so
/// the option on screen and the verdict in the report cannot disagree. Passing
/// it down also makes the fail-safe testable at both layers — the parser
/// refuses `e` without it, and this function refuses an `ExitChoice::Enter` it
/// somehow receives without it.
async fn finish_vm(
    runner: &mut CcRunner<'_>,
    cfg: &CcConfig,
    vm: VmEnd,
    yes: bool,
    offer_enter: bool,
    ui: &mut ExitUi<'_>,
) -> VmEnd {
    if vm != VmEnd::Running {
        return vm;
    }
    if yes {
        // Non-interactive: stop the meter. `--yes` approves the plan that was
        // displayed; it never approves a deletion, it never opens a session,
        // and it must not leave a VM running in a script nobody is watching.
        return stop_and_report(runner, cfg).await;
    }
    loop {
        let choice = match (ui.ask)(cfg.machine.hourly_usd(), offer_enter) {
            Ok(c) => c,
            // An unreadable answer is not a licence to keep billing.
            Err(_) => ExitChoice::Stop,
        };
        match choice {
            ExitChoice::Keep => {
                println!(
                    "  VM kept · ≈ {:.2} $/h while it runs · `x0ne cc` resumes from it",
                    cfg.machine.hourly_usd()
                );
                return VmEnd::Running;
            }
            ExitChoice::Stop => return stop_and_report(runner, cfg).await,
            // SECURITY: the second half of the gate, and not a redundant one.
            // The parser cannot mint this variant without the proof, but this
            // function does not TRUST the parser — a prompter is one refactor
            // away from being replaced, and the failure mode of that refactor
            // is a chat inside an enclave whose report said REFUSED. Landing on
            // `Stop` also keeps the fail-safe consistent: whatever we could not
            // honour, the meter goes off.
            ExitChoice::Enter if !offer_enter => return stop_and_report(runner, cfg).await,
            ExitChoice::Enter => {
                if let Err(e) = (ui.enter)(cfg) {
                    println!(
                        "  {} could not enter the enclave: {}",
                        "✗".truecolor(220, 80, 80),
                        clean(&e)
                    );
                }
                // Back from the session — and the VM is STILL RUNNING, which is
                // the one fact an operator who just spent ten minutes chatting
                // is likely to have stopped thinking about. Say it, with the
                // price, before asking again.
                println!(
                    "\n  {} is still RUNNING · ≈ {:.2} $/h",
                    cfg.vm_name,
                    cfg.machine.hourly_usd()
                );
                continue;
            }
            ExitChoice::Destroy => {
                // SECURITY: destruction is the one irreversible step, and the
                // plan approval above does not cover it — it was never in the
                // plan. It asks its own question, here, and a refusal leaves
                // the VM running rather than guessing a milder action the
                // operator did not choose.
                return match (ui.confirm)("really DELETE the VM and its disk?") {
                    Ok(true) => match runner.delete_vm().await {
                        Ok(()) => {
                            println!("  VM deleted");
                            VmEnd::NeverUp
                        }
                        Err(e) => {
                            println!(
                                "  {} delete failed: {}",
                                "✗".truecolor(220, 80, 80),
                                clean(&e)
                            );
                            VmEnd::Running
                        }
                    },
                    _ => {
                        println!("  kept — deletion refused");
                        VmEnd::Running
                    }
                };
            }
        }
    }
}

/// Where the two binaries `InstallX0ne` uploads were found — which is the same
/// question as "what libc were they linked against".
///
/// SECURITY-adjacent, and measured: an ELF built on this workstation carries
/// the version tags of the glibc it was linked against, and the enclave's
/// loader refuses anything newer than its own. Run 8 (2026-08-15) spent a full
/// H100 pipeline — attestation, driver, 15 GB of weights, a served model — to
/// die on the last step with `x0ne --version` exiting 1, because the host is
/// Debian 13 (glibc 2.41), the binary needed `GLIBC_2.39`, and the VM image is
/// Ubuntu 22.04 (glibc 2.35). The origin travels with the paths so the plan
/// line can say which of the two cases the operator is about to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinariesOrigin {
    /// `target/enclave/`, produced by `make enclave-binaries` — a container
    /// build against an old glibc, made for this exact purpose.
    BuiltForTheEnclave,
    /// The running executable and its sibling runtime. Correct on a host whose
    /// glibc is no newer than the VM image's, a loader refusal otherwise.
    ThisHost,
}

/// The binaries `InstallX0ne` will upload, and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinariesChoice {
    pub bins: X0neBinaries,
    pub origin: BinariesOrigin,
}

impl BinariesChoice {
    /// What the plan line appends about these two files, before the human gate.
    ///
    /// Repo invariant 5: what is approved must be readable. The paths are
    /// printed because "THIS x0ne" was not enough — an operator reading it had
    /// no way to know whether the enclave build was picked up, and the run 8
    /// failure is invisible until the last step of a ~30 min pipeline. The
    /// host case therefore reads as a RISK with its two levers, not as an
    /// incident report.
    fn plan_suffix(&self) -> String {
        match self.origin {
            BinariesOrigin::BuiltForTheEnclave => format!(
                ". The files that go are {} and {} — built for the enclave by \
                 `make enclave-binaries` (old-glibc container), which is what the VM image can \
                 load",
                self.bins.x0ne.display(),
                self.bins.runtime.display()
            ),
            BinariesOrigin::ThisHost => format!(
                ". The files that go are {} and {} — built on THIS host, so they carry its \
                 glibc. GLIBC RISK: a VM image older than this workstation refuses them at load \
                 and the step fails after everything else succeeded (measured run 8, \
                 2026-08-15: GLIBC_2.39 not found on Ubuntu 22.04, which has 2.35). Build \
                 loadable ones with `make enclave-binaries` (they land in target/enclave/ and \
                 are picked up from the repo you run in), or point X0NE_ENCLAVE_BINARIES at a \
                 directory holding both",
                self.bins.x0ne.display(),
                self.bins.runtime.display()
            ),
        }
    }
}

/// The two binaries `InstallX0ne` uploads: the enclave build if there is one,
/// the running executable otherwise.
///
/// Every input is a PARAMETER — `exe` is `std::env::current_exe()`, `cwd` is
/// `std::env::current_dir()` and `override_dir` is `$X0NE_ENCLAVE_BINARIES` at
/// the one production call site — so the resolution is a pure function with
/// tests. The paths are what ends up running inside an attested enclave, and
/// "it worked on my machine" is not a property. It is also literally the bug:
/// what worked on this machine is exactly what the enclave refused to load.
///
/// The search order, and it is the whole contract:
///
///   1. `$X0NE_ENCLAVE_BINARIES` when set — the directory must hold BOTH
///      `x0ne` and `x0ne-runtime`, and a set-but-incomplete override REFUSES
///      rather than falling back. An operator who names a directory has said
///      which binaries they mean; silently uploading two others instead is the
///      one outcome an explicit override must never produce.
///   2. `target/enclave/` under the current directory or any of its ancestors
///      — i.e. `make enclave-binaries` then `x0ne cc` from anywhere in the
///      repo. Both files must be there for the directory to count; a half-built
///      one is skipped and the walk continues, because the plan line names the
///      paths either way and a partial enclave build must not masquerade as a
///      complete one.
///   3. The running executable plus its SIBLING runtime — the historical
///      behaviour, and the layout both `cargo build --release` and
///      `make install` produce. The plan line says what that costs.
///
/// The cwd, not the executable's directory, is what step 2 walks: `x0ne` runs
/// from `~/.cargo/bin` after `make install`, and `~/.cargo/bin/../target` is
/// not a thing. The operator's working directory is the repo — that is where
/// `make enclave-binaries` was typed — and the rule is stated in the plan line
/// so nobody has to guess it.
///
/// Step 3 keeps copying THIS executable rather than something found by name:
/// `X0NE_VERSION` is `env!("CARGO_PKG_VERSION")` of this build and the VM-side
/// acquis probe compares against it, so copying any other binary would produce
/// a step that installs one thing and verifies another. Steps 1 and 2 break
/// that tie deliberately — a cross-built binary of the same version is the
/// point — and `install_x0ne`'s verification is what re-establishes the link,
/// by asking the copied binary for its version once it is in the VM.
///
/// The refusal names `cargo build --release` on purpose (same rule as
/// `PlanRefused::message`): the operator who hits this is one command away from
/// a working run, and a refusal they cannot act on costs a research round trip.
fn resolve_x0ne_binaries(
    exe: &std::path::Path,
    cwd: &std::path::Path,
    override_dir: Option<&std::path::Path>,
) -> Result<BinariesChoice, String> {
    if let Some(dir) = override_dir {
        return match pair_in(dir) {
            Some(bins) => Ok(BinariesChoice {
                bins,
                origin: BinariesOrigin::BuiltForTheEnclave,
            }),
            None => Err(format!(
                "X0NE_ENCLAVE_BINARIES points at {}, which does not hold both `x0ne` and \
                 `x0ne-runtime`. An explicit override is never silently replaced by other \
                 binaries: build them with `make enclave-binaries` (they land in \
                 target/enclave/), or unset the variable to copy the running x0ne and its \
                 sibling",
                dir.display()
            )),
        };
    }

    for ancestor in cwd.ancestors() {
        if let Some(bins) = pair_in(&ancestor.join("target").join("enclave")) {
            return Ok(BinariesChoice {
                bins,
                origin: BinariesOrigin::BuiltForTheEnclave,
            });
        }
    }

    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let runtime = dir.join("x0ne-runtime");
    for (what, path) in [("x0ne", exe), ("x0ne-runtime", runtime.as_path())] {
        if !path.is_file() {
            return Err(format!(
                "`{what}` is not at {} — `x0ne cc --machine full` copies THIS x0ne and its \
                 sibling runtime into the VM, so both must sit side by side. Build them with \
                 `cargo build --release` (or install them with `make install`) and re-run",
                path.display()
            ));
        }
    }
    Ok(BinariesChoice {
        bins: X0neBinaries {
            x0ne: exe.to_path_buf(),
            runtime,
        },
        origin: BinariesOrigin::ThisHost,
    })
}

/// Both binaries in one directory, or nothing. `None` for a directory holding
/// one of the two: half a payload is not a payload.
fn pair_in(dir: &std::path::Path) -> Option<X0neBinaries> {
    let x0ne = dir.join("x0ne");
    let runtime = dir.join("x0ne-runtime");
    (x0ne.is_file() && runtime.is_file()).then_some(X0neBinaries { x0ne, runtime })
}

/// How long this layer pauses after a probe, from the label the runner passed
/// and the exit code the command returned. `None` means "do not sleep".
///
/// A pure function rather than two `if`s inside `BridgeExec::run`, because
/// `BridgeExec` needs a live `Bridge` and therefore has no unit test at all —
/// which was measured, not assumed: deleting the vLLM pause left every test in
/// the workspace green (task 4 mutation M18). The pause is the ONLY thing
/// making a retry loop wait, so a deletion here turns 36 probes spread over
/// 30 minutes into 36 probes in one second, minutes before a model download can
/// possibly have finished — the round-1 SSH bug, which cost a review round.
/// Extracting the decision does not test the wiring; it tests the rule, and
/// leaves the wiring one obvious line.
///
/// The asymmetry between the two labels is deliberate and is the whole content
/// of this function. An SSH probe that SUCCEEDS ends the wait, so pausing after
/// it would burn 10 s for nothing. A vLLM probe that succeeds proves nothing of
/// the sort: `ss` exits 0 while saying nothing about our port, and a curl can
/// exit 0 against a server that is up and serving nothing. The exec cannot tell
/// those apart without reading remote text, which is not its job — so it pauses
/// on every one of them, and the cost is one 20 s pause after the last,
/// successful probe (≈ 2 cents at 3.72 $/h, counted in the budget).
fn probe_pause_secs(display_label: &str, exit_code: i32) -> Option<u64> {
    if display_label == WAIT_SSH_DISPLAY && exit_code != 0 {
        return Some(SSH_PROBE_PAUSE_SECS);
    }
    // A PREFIX, not an equality, since run 4 (2026-08-14): the vLLM wait now
    // appends the download figure to its label ("wait for vLLM — 4.2G/15G
    // downloaded") so the operator can see that patience is justified rather
    // than merely spent. Under an equality test that suffix would silently turn
    // the pause off and burn 36 probes in one second — the round-1 SSH bug, in
    // the costume of a progress indicator. `starts_with` is what keeps the
    // label human-readable and load-bearing at the same time.
    if display_label.starts_with(WAIT_VLLM_DISPLAY) {
        return Some(VLLM_PROBE_PAUSE_SECS);
    }
    None
}

/// The production `CcExec`: one gcloud command per call, over the ordinary
/// runtime bridge.
struct BridgeExec<'a> {
    bridge: &'a mut dyn Bridge,
    logger: Option<SessionLogger>,
}

#[async_trait]
impl CcExec for BridgeExec<'_> {
    async fn run(
        &mut self,
        display_label: &str,
        argv: &[String],
        timeout_secs: u64,
    ) -> Result<CmdOut> {
        // The budget is the runner's decision, not ours: it knows whether this
        // is `instances create` (300 s), a warm SSH probe (20 s), or the first
        // SSH probe, which has a key to generate and push (90 s). This layer
        // only enforces it. Deciding here — by sniffing the label — is what
        // made the first probe inherit the wrong number twice running.
        let action = json!({
            "type": "RunCommand",
            "command": "gcloud",
            "args": argv,
            "timeout_secs": timeout_secs,
        });

        // Same rendering as every other action in the product: the label, the
        // action box before, the result line after, and `log_action` for the
        // audit trail (self-test pattern, main.rs:2914-2925). A cloud spend
        // the session log cannot account for is exactly what the journal is
        // for.
        println!("  {}", display_label.truecolor(120, 130, 150));
        print!("{}", display::action_box(&action));
        let started = Instant::now();
        // On `StdioBridge` — the bridge `x0ne cc` runs on — every error out of
        // `send_action` is the TRANSPORT, never the command: a policy denial, a
        // non-zero exit, a runtime-side timeout all come back as `Ok` carrying
        // a JSON envelope with `success: false`, handled twenty lines below. An
        // `Err` is a broken pipe the bridge already tried to respawn through,
        // an empty response line (the runtime exited before writing one), an
        // unreadable stdout, or a reply that is not JSON — `bridge.rs:257-300`,
        // which review round 3 checked line by line before this comment was
        // allowed to say "every". None become true on a retry, so the
        // runner's wait loops must be told to stop rather than spend their
        // attempts on a dead pipe (2026-08-14: 26 of them, ~10 min at 3.72 $/h,
        // and a final message that accused the VM).
        //
        // SCOPED to stdio on purpose, and this is the limit of the claim
        // (review round 3, I2): `HttpBridge::send_action` (bridge.rs:354-374)
        // turns ANY reqwest failure into `Err`, and its client timeout is 120 s
        // (:338) against the 300 s this layer grants a `gcloud instances
        // create`. On `bridge.mode = "http"`, a create that legitimately takes
        // more than two minutes would therefore be reported as a dead bridge —
        // a false diagnosis and an aborted run, not a leaked meter (the rescue
        // then reconnects to a server that is alive and its stop goes through).
        // Not fixed here because it is a defect of the HTTP bridge's budget,
        // not of this classification; it is tracked apart.
        let result = match self.bridge.send_action(&action).await {
            Ok(r) => r,
            Err(e) => return Err(ExecTransportDown::raise(format!("{e:#}"))),
        };
        let ms = started.elapsed().as_millis() as u64;
        print!("{}", display::result_line(&action, &result, ms));
        if let Some(l) = self.logger.as_mut() {
            let _ = l.log_action(&action, &result, ms).await;
        }

        let stdout = result["output"]["stdout"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mut stderr = result["output"]["stderr"]
            .as_str()
            .unwrap_or("")
            .to_string();
        // No `output` at all means the command never ran — policy denial or a
        // transport error. `-1` keeps the runner fail-closed (every caller
        // treats non-zero as failure), but on its own it tells the operator
        // nothing, so the runtime's own error travels out through stderr.
        let exit_code = result["output"]["exit_code"].as_i64().unwrap_or(-1) as i32;
        if !result["success"].as_bool().unwrap_or(false) {
            let kind = result["error"]["kind"].as_str().unwrap_or("unknown");
            let msg = result["error"]["message"]
                .as_str()
                .unwrap_or("(no message)");
            stderr = format!("{stderr}[{kind}] {msg}");
            if kind == "policy_denied" {
                // Name the lever. A refusal the operator cannot act on costs
                // a research round-trip (same lesson as `PlanRefused::message`).
                stderr.push_str(
                    " — the runtime policy must allow gcloud: add \"gcloud\" to \
                     allowed_commands in ~/.config/x0ne/x0ne-runtime.toml",
                );
            }
        }

        // WHY THE PAUSE LIVES HERE. `wait_ssh` and the two vLLM waits retry
        // with no sleep at all: a clock in the runner would make their tests
        // ("gives up after exactly N probes", "stops at the first success")
        // take half an hour, so Task 6 ruled the runner never sleeps. In
        // production the retries would then all fire in under a second, long
        // before sshd is up or a 15 GB download can have landed. So the wait is
        // the exec layer's job, and WHICH wait applies is `probe_pause_secs`,
        // where it can be unit-tested — see its doc-comment for why that
        // extraction is not ceremony. The labels are shared constants and not
        // literals on each side, because a silent typo here would leave a flag
        // that is read and changes nothing: this repo's signature defect.
        if let Some(secs) = probe_pause_secs(display_label, exit_code) {
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }

        Ok(CmdOut {
            stdout,
            stderr,
            exit_code,
        })
    }
}

pub async fn run_cc(args: CcArgs, config: &Config) -> Result<()> {
    let machine = resolve_machine(args.machine.as_deref()).map_err(|e| anyhow::anyhow!(e))?;
    let cap_usd =
        validate_cap(args.cap.unwrap_or(DEFAULT_CAP_USD)).map_err(|e| anyhow::anyhow!(e))?;
    let cfg = CcConfig {
        machine,
        vm_name: VM_NAME.into(),
        cap_usd,
    };

    println!(
        "\n  {}  {} — confidential VM, attested\n",
        "◆".truecolor(0, 200, 220),
        "x0ne cc".truecolor(0, 200, 220).bold()
    );

    // This command runs its runtime ON THE HOST, with `--isolation none` (set
    // below), unlike an `x0ne` session, whose agent commands run under bwrap.
    // That is not an oversight: gcloud and its Application Default Credentials live in the
    // operator's `~/.config/gcloud`, and a sandboxed runtime cannot see them —
    // the pipeline would fail its first probe. The trade is real and belongs
    // on screen, not in a comment, so it gets the same badge line the autowrap
    // decision prints (main.rs:610-627).
    println!(
        "  {}  bridge: {} · runtime on THIS host (gcloud + ADC live here, not in a sandbox)",
        "◆".truecolor(110, 120, 135),
        config.bridge.mode.truecolor(110, 120, 135),
    );

    // Gcloud needs the operator's credentials and the network, both of
    // which the default confinement withholds. The argv is built by this
    // code, never by a model, and the banner above says it runs on the host.
    x0ne_orchestrator::bridge::set_runtime_extra_args(vec!["--isolation".into(), "none".into()]);
    let mut bridge = create_bridge(
        &config.bridge.mode,
        &config.bridge.x0ne_core_bin,
        &config.bridge.x0ne_core_addr,
        config.bridge.health_check_timeout_secs,
        config.bridge.sandbox_cmd.as_deref(),
    )
    .await?;
    bridge.health_check().await?;

    let mut exec = BridgeExec {
        bridge: bridge.as_mut(),
        logger: SessionLogger::new().await.ok(),
    };
    let mut runner = CcRunner::new(&mut exec, cfg.clone());

    // ── 1. Observe, then plan ────────────────────────────────────────────
    let obs = runner.observe().await?;
    let plan = match build_plan(&cfg, &obs) {
        Ok(p) => p,
        Err(refused) => {
            println!("\n  {} {}", "✗".truecolor(220, 80, 80), refused.message());
            // Nothing spent, nothing to clean up — but see `exit_status_for`:
            // a preflight refusal is still "not proven", and exiting 0 here
            // would make `x0ne cc --yes && echo proven` print "proven"
            // because gcloud was not logged in.
            anyhow::bail!("preflight refused — nothing was attested");
        }
    };

    // The binaries `InstallX0ne` will upload, resolved BEFORE the gate and
    // before a cent is spent. A pipeline that creates an H100, attests it,
    // installs a driver and serves a model, only to discover at the last step
    // that there is no `x0ne-runtime` next to the binary it is running, has
    // burned ~30 min of SPOT to report a local build problem. Asked only when
    // the plan actually contains the step: on the prover, and on a full machine
    // that already carries this version, there is nothing to copy.
    let binaries = if plan.contains(&CcStep::InstallX0ne) {
        // The three inputs of the search, read HERE and passed down, so the
        // rule itself stays a pure function: the running binary, the directory
        // the operator typed the command in (which is where `target/enclave/`
        // is looked for), and the explicit override.
        let over = std::env::var_os("X0NE_ENCLAVE_BINARIES").map(std::path::PathBuf::from);
        let resolved = std::env::current_exe()
            .map_err(|e| format!("cannot locate the running x0ne binary: {e}"))
            .and_then(|exe| {
                let cwd = std::env::current_dir().unwrap_or_default();
                resolve_x0ne_binaries(&exe, &cwd, over.as_deref())
            });
        match resolved {
            Ok(b) => Some(b),
            Err(msg) => {
                println!("\n  {} {}", "✗".truecolor(220, 80, 80), clean(&msg));
                anyhow::bail!("no x0ne binaries to install — nothing was attested");
            }
        }
    } else {
        None
    };
    if let Some(b) = &binaries {
        runner.set_x0ne_binaries(b.bins.clone());
    }

    // Seeded from the observation, not from the steps we are about to run: a
    // VM that was ALREADY running is already billing, so a failure at the
    // very first step still owes the operator a stop. `build_plan` skips
    // CreateVm/StartVm in exactly that case, which is why nothing later in
    // this function would otherwise know the meter was on.
    let vm_initial = if matches!(obs.instance, InstanceState::Running { .. }) {
        VmEnd::Running
    } else {
        VmEnd::NeverUp
    };

    // ── 2. Show the plan IN FULL, then ask ───────────────────────────────
    // This display plus one key is the human gate for every step listed.
    // Destruction is not in the plan and is never covered by this approval.
    println!("\n{}", render_plan(&cfg, &plan, binaries.as_ref()));
    if !args.yes && !confirm_one_key("proceed?")? {
        println!("  cancelled — no step was run");
        if vm_initial == VmEnd::Running {
            // Refusing the plan means we touch nothing, including a VM that
            // was ALREADY up when we observed it. Saying so is the difference
            // between "nothing happened" and "nothing happened and you are
            // still paying".
            // Same source as the stop we would have issued (`to_stop_command`),
            // for the same reason as `VmEnd::lever`: a command printed for a
            // human must be one that works — the retyped one omitted
            // `--discard-local-ssd` and fails on an a3.
            println!(
                "  note: {} was already running · ≈ {:.2} $/h · stop it with: {}",
                cfg.vm_name,
                cfg.machine.hourly_usd(),
                cfg.machine.vm_spec(&cfg.vm_name).to_stop_command()
            );
        }
        anyhow::bail!("cancelled — nothing was attested");
    }

    // ── 3. Run, with the budget gate between steps ───────────────────────
    // The clock starts here, not at the wizard: reading a plan is free.
    let started = Instant::now();
    let in_flight = AtomicUsize::new(0);

    // SECURITY (final review, F1): Ctrl+C must stop the METER, not just the
    // process. `x0ne cc` is dispatched before `interrupt::install_signal_
    // handler()` and had no handler of its own, so a Ctrl+C during the 14 min
    // WaitSsh killed the process and left the VM RUNNING — with the budget
    // gate gone too, since the gate lives in the process that just died.
    //
    // Dropping the pipeline future does NOT cancel the gcloud command already
    // in flight inside the runtime; that command runs to completion. That is
    // acceptable — the action that matters is the stop we issue next, and the
    // observation before it reports what actually exists rather than what we
    // assume.
    let pipeline = tokio::select! {
        r = run_plan(&mut runner, &cfg, &plan, vm_initial, started, &in_flight) => Some(r),
        _ = tokio::signal::ctrl_c() => None,
    };

    let (vm, failed) = match pipeline {
        Some(end) => end,
        None => {
            let step = plan
                .get(in_flight.load(Ordering::Relaxed))
                .cloned()
                .unwrap_or(CcStep::Report);
            println!(
                "\n  {} interrupted during {step:?} — checking the VM before exit",
                "⚠".truecolor(220, 180, 60)
            );
            // A FRESH bridge, deliberately, and this is not belt-and-braces.
            // The pipeline's bridge was abandoned mid-request: the stdio
            // protocol is newline-delimited with no request ids
            // (`bridge.rs:257-300`), so the reply to the interrupted command is
            // still sitting in the pipe and the next read on THAT bridge would
            // return it. Reusing it would make this shutdown report the
            // previous command's result — the report lying about the meter,
            // which is the one thing this branch has been hardened against
            // twice. A second runtime costs milliseconds and answers about the
            // command we actually sent.
            let settled = match create_bridge(
                &config.bridge.mode,
                &config.bridge.x0ne_core_bin,
                &config.bridge.x0ne_core_addr,
                config.bridge.health_check_timeout_secs,
                config.bridge.sandbox_cmd.as_deref(),
            )
            .await
            {
                Ok(mut rescue) => {
                    let mut rescue_exec = BridgeExec {
                        bridge: rescue.as_mut(),
                        logger: None,
                    };
                    let mut rescue_runner = CcRunner::new(&mut rescue_exec, cfg.clone());
                    // The first Ctrl+C was consumed by the select above,
                    // and this settle can sit up to 300 s on a gcloud — during
                    // which a second press did NOTHING (tokio's ctrl_c owns
                    // SIGINT for the rest of the process; Ctrl+\ was the only
                    // exit). Same escalation as interrupt.rs: the second press
                    // abandons the settle, and the report that follows renders
                    // `Uncertain` WITH the check command — never a claimed
                    // state for a VM nobody observed.
                    tokio::select! {
                        settled = settle_after_interrupt(&mut rescue_runner, &cfg) => settled,
                        _ = tokio::signal::ctrl_c() => {
                            println!(
                                "  {} second Ctrl+C — leaving without settling; the VM state is unverified",
                                "✗".truecolor(220, 80, 80)
                            );
                            VmEnd::Uncertain
                        }
                    }
                }
                // No runtime to ask with: say so rather than claim a state.
                Err(e) => {
                    println!(
                        "  {} could not reach a runtime to stop the VM: {}",
                        "✗".red(),
                        clean(&e)
                    );
                    VmEnd::Uncertain
                }
            };
            (settled, Some((step, "interrupted by Ctrl+C".into())))
        }
    };

    // ── 3 bis. The stop the pipeline could not issue ─────────────────────
    //
    // SECURITY (money): `run_plan` skips its own stop when the transport died,
    // because that stop would go into the corpse. This is where it gets issued
    // instead — on ONE fresh runtime, exactly as the Ctrl+C branch above brings
    // one up, and for the same reason: the bridge we hold cannot answer. A
    // no-op on every other path, `create_bridge` included, since the factory is
    // only called when the rescue is due.
    let vm = rescue_stop_if_the_bridge_died(vm, runner.transport_is_down(), &cfg, || {
        create_bridge(
            &config.bridge.mode,
            &config.bridge.x0ne_core_bin,
            &config.bridge.x0ne_core_addr,
            config.bridge.health_check_timeout_secs,
            config.bridge.sandbox_cmd.as_deref(),
        )
    })
    .await;

    // ── 4. Report, on every path ─────────────────────────────────────────
    let spent = spent_usd(cfg.machine.hourly_usd(), started.elapsed().as_secs());
    let outcome = runner.outcome(failed);
    report_and_record(&outcome, spent, cfg.machine, &vm.label(&cfg), args.json);

    // ── 5. Exit choice, only while something is still billing ────────────
    //
    // Derived from the outcome that was just rendered, so the option on screen
    // and the verdict above it come from the same value. See
    // `offers_enclave_entry`.
    let offer_enter = offers_enclave_entry(&outcome, cfg.machine);
    finish_vm(
        &mut runner,
        &cfg,
        vm,
        args.yes,
        offer_enter,
        &mut ExitUi {
            ask: &mut ask_keep_stop_destroy,
            confirm: &mut |q| confirm_one_key(q),
            enter: &mut enter_enclave,
        },
    )
    .await;

    // ── 6. The exit status must agree with the report ────────────────────
    exit_status_for(&outcome)
}

/// Print the report the operator asked for, and record the machine-readable
/// one whatever they asked for.
///
/// # Why the file is written on every run rather than behind a flag
///
/// It is a report, not an export. The operator who wants to know what
/// yesterday's run measured should not have had to know to ask yesterday, and
/// `--serve`'s `GET /attestation` needs a file that exists without anyone
/// having remembered a flag. A refused or failed run is recorded too: that is
/// the run whose record matters most, and a directory holding only successes
/// describes a machine that has never failed.
///
/// A write failure is printed and swallowed — deliberately, and this is the
/// one place in this file that swallows anything. The report on screen is the
/// operator's answer; losing the copy must not turn a proven enclave into a
/// non-zero exit, because `exit_status_for` means "the enclave was proven, and
/// nothing else" (see its note). The failure is still said out loud.
fn report_and_record(
    outcome: &CcOutcome,
    spent: f64,
    machine: CcMachine,
    vm_state: &str,
    as_json: bool,
) {
    let json = render_report_json(outcome, spent, machine, vm_state);
    if as_json {
        // `to_string_pretty` cannot fail on a value serde built, and a report
        // is no place for an `unwrap` — the fallback is the value's own Display.
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
        );
    } else {
        println!("\n{}", render_report(outcome, spent, machine, vm_state));
    }
    if let Err(e) = write_last_report(&json) {
        eprintln!("  could not write the report file: {e}");
    }
}

/// SECURITY: may this run's exit menu offer a session INSIDE the VM?
///
/// The conjunction is the whole content of this function, and both halves are
/// load-bearing.
///
/// The PROOF, because the spec's rule for the prover is that there is no path
/// to a chat without a green verdict. An `enter` printed under ATTESTATION
/// REFUSED would be that path, and the operator taking it would be typing into
/// a machine the report had just declined to vouch for — the badge, the report
/// and this menu have to agree, or the two that agree are decoration.
///
/// The MACHINE, because `Proven` on the prover is a real, green result about a
/// VM that has no GPU, no vLLM and no x0ne in it: `full_h100_steps` is what
/// puts `InstallX0ne` in a plan, and it only ever runs for `FullH100`. Offering
/// an entry there would open a shell onto a bare Ubuntu and call it an enclave
/// session.
fn offers_enclave_entry(outcome: &CcOutcome, machine: CcMachine) -> bool {
    matches!(outcome, CcOutcome::Proven { .. }) && machine == CcMachine::FullH100
}

/// SECURITY: **exit 0 means the enclave was proven, and nothing else.**
///
/// `--yes` is documented for scripts and CI, and `x0ne cc --yes && echo
/// proven` printed "proven" over ATTESTATION REFUSED for as long as every
/// path returned `Ok` (review round 1, finding I2). Every other outcome —
/// refused, failed, and (at the call sites above) a preflight refusal or a
/// human cancellation — is a non-zero exit, because none of them attested
/// anything and a shell cannot read a report.
///
/// The message names only the outcome and the step, never the reason: the
/// detail is already in the rendered report, and `reason`/`error` can carry
/// text that started life as remote output (see `render_report`'s SECURITY
/// note). Nothing attacker-controlled reaches this line.
fn exit_status_for(outcome: &CcOutcome) -> Result<()> {
    match outcome {
        // Deliberately blind to the GPU half, and it can afford to be: on the
        // full machine, `CcRunner::outcome` never MINTS a `Proven` without GPU
        // evidence (it returns `Failed{AttestGpu}` instead), and a GPU that
        // answered OFF fails its step, so both roads to "exit 0 over a report
        // saying the GPU is exposed" are closed upstream. Re-checking the
        // machine here would be a second, weaker copy of that rule — the kind
        // that drifts.
        CcOutcome::Proven { .. } => Ok(()),
        CcOutcome::Refused { .. } => {
            anyhow::bail!("attestation refused — see the report above")
        }
        CcOutcome::Failed { step, .. } => {
            anyhow::bail!("pipeline failed at {step:?} — see the report above")
        }
    }
}

#[cfg(test)]
mod tests {
    /// #47b: the BridgeExec seam — building the JSON action and
    /// passing the timeout — proven by a fake Bridge, no longer only by
    /// cross-reading. A failure would mean a gcloud sent without its budget
    /// (the runtime would apply its default, not the one the runner chose)
    /// or arguments rewritten on the way.
    #[tokio::test]
    async fn bridge_exec_mounts_the_action_json_and_passes_the_timeout_through() {
        use x0ne_orchestrator::bridge::Bridge;
        struct FakeBridge {
            seen: Vec<serde_json::Value>,
            reply: serde_json::Value,
        }
        #[async_trait::async_trait]
        impl Bridge for FakeBridge {
            async fn send_action(
                &mut self,
                action: &serde_json::Value,
            ) -> anyhow::Result<serde_json::Value> {
                self.seen.push(action.clone());
                Ok(self.reply.clone())
            }
            async fn health_check(&mut self) -> anyhow::Result<bool> {
                Ok(true)
            }
        }

        let mut bridge = FakeBridge {
            seen: vec![],
            reply: serde_json::json!({
                "success": true,
                "output": {"stdout": "RUNNING", "stderr": "", "exit_code": 0}
            }),
        };
        let argv: Vec<String> = ["compute", "instances", "list"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        {
            let mut exec = BridgeExec {
                bridge: &mut bridge,
                logger: None,
            };
            let out = exec.run("observe", &argv, 42).await.unwrap();
            assert_eq!(out.exit_code, 0);
            assert_eq!(out.stdout, "RUNNING");
        }
        let action = &bridge.seen[0];
        assert_eq!(action["type"], "RunCommand");
        assert_eq!(action["command"], "gcloud");
        assert_eq!(
            action["args"],
            serde_json::json!(["compute", "instances", "list"])
        );
        assert_eq!(
            action["timeout_secs"], 42,
            "the runner budget must travel as-is"
        );

        // And the failure envelope: kind + message in stderr, the lever
        // named on policy_denied, exit -1 fail-closed without output.
        bridge.reply = serde_json::json!({
            "success": false,
            "error": {"kind": "policy_denied", "message": "Command 'gcloud' is not allowed"}
        });
        let out = {
            let mut exec = BridgeExec {
                bridge: &mut bridge,
                logger: None,
            };
            exec.run("observe", &argv, 42).await.unwrap()
        };
        assert_eq!(out.exit_code, -1, "no output => -1, fail-closed");
        assert!(out.stderr.contains("[policy_denied]"), "{}", out.stderr);
        assert!(
            out.stderr.contains("allowed_commands"),
            "the lever must be named: {}",
            out.stderr
        );
    }

    /// The cap blown AFTER the verdict is announced at the Report step —
    /// message only, never a failure. A failure of this test would mean
    /// either the silence is back (Proven report, VM stopped, « keep » menu,
    /// not a word about the budget), or a Report that rejects a proof
    /// paid for with money already spent.
    #[test]
    fn a_cap_blown_after_the_verdict_is_announced_at_report_never_fatal() {
        use super::report_step_cap_note;
        let note = report_step_cap_note(30.0, 25.0).expect("cap blown => one sentence");
        assert!(note.contains("30.00") && note.contains("25.00"), "{note}");
        assert!(note.contains("report stands"), "{note}");
        assert!(
            report_step_cap_note(10.0, 25.0).is_none(),
            "under the cap: silence"
        );
        // A non-finite cap stays fail-closed via cap_status: the note goes out.
        assert!(report_step_cap_note(0.01, f64::NAN).is_some());
    }

    use super::*;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use x0ne_cloud::cc_plan::CcObservations;
    use x0ne_cloud::provision::ProvisioningModel;
    use x0ne_orchestrator::cc_runner::{GCLOUD_TIMEOUT_SECS, SSH_RETRIES};

    fn cfg() -> CcConfig {
        CcConfig {
            machine: CcMachine::Prover,
            vm_name: VM_NAME.into(),
            cap_usd: 25.0,
        }
    }

    fn cfg_full() -> CcConfig {
        CcConfig {
            machine: CcMachine::FullH100,
            vm_name: VM_NAME.into(),
            cap_usd: DEFAULT_CAP_USD,
        }
    }

    /// A ready project and no instance: the observation from which the
    /// longest plan is built.
    fn obs_fresh() -> CcObservations {
        CcObservations {
            auth_account: Some("a@b.c".into()),
            project: Some("example-project-123456".into()),
            image_family_exists: Some(true),
            instance: InstanceState::Absent,
            gpu_driver_ready: None,
            gpu_cc_on: None,
            vllm_healthy: None,
            x0ne_ready: None,
        }
    }

    /// A `CcExec` that answers from a script and records what was asked. The
    /// runner's own test module has one, but it is `#[cfg(test)]`-private to
    /// that crate; this is the local equivalent, and it is what lets the
    /// budget loop be tested without a bridge, a VM or a cent.
    #[derive(Default)]
    struct ScriptedExec {
        responses: VecDeque<CmdOut>,
        sent: Vec<Vec<String>>,
    }

    impl ScriptedExec {
        fn with(responses: Vec<CmdOut>) -> Self {
            Self {
                responses: responses.into(),
                sent: vec![],
            }
        }
        /// True when some command's argv contains all of `needles` in order of
        /// appearance anywhere — enough to say "a stop was issued".
        fn issued(&self, needles: &[&str]) -> bool {
            self.sent
                .iter()
                .any(|argv| needles.iter().all(|n| argv.iter().any(|a| a == n)))
        }
    }

    /// Scripted stdout that means "answer this attestation honestly".
    ///
    /// A verified quote cannot be written into a script: `attest_tdx` draws a
    /// fresh nonce from `/dev/urandom` and the quote has to ECHO it. So the
    /// exec plays the part a real TDX VM plays — it reads the nonce out of the
    /// command it was handed and builds a quote carrying it. Every test that
    /// needs the pipeline to get PAST attestation goes through here; the
    /// stale-quote path (a plain base64 string) still tests the refusal.
    const HONEST_QUOTE: &str = "<a quote echoing the nonce we were sent>";

    /// The nonce `attest_tdx` wrote into its remote command, if this is that
    /// command. Mirrors the `printf %s {nonce_hex}` of `cc_runner`.
    fn nonce_in(argv: &[String]) -> Option<[u8; 64]> {
        let cmd = argv
            .iter()
            .find(|a| a.contains("/sys/kernel/config/tsm/report/x0ne/inblob"))?;
        let hex = cmd.split("printf %s ").nth(1)?.split_whitespace().next()?;
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
            .collect();
        bytes.try_into().ok()
    }

    #[async_trait]
    impl CcExec for ScriptedExec {
        async fn run(
            &mut self,
            _display: &str,
            argv: &[String],
            _timeout_secs: u64,
        ) -> Result<CmdOut> {
            self.sent.push(argv.to_vec());
            let mut out = self
                .responses
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("script exhausted"))?;
            if out.stdout == HONEST_QUOTE {
                let nonce = nonce_in(argv).expect("HONEST_QUOTE scripted for a non-attest command");
                let mut q = vec![0u8; 48 + 584 + 8];
                q[0] = 4;
                q[4..8].copy_from_slice(&0x81u32.to_le_bytes());
                q[48 + 136..48 + 184].copy_from_slice(&[0xAB; 48]);
                q[48 + 520..48 + 584].copy_from_slice(&nonce);
                use base64::Engine;
                out.stdout = base64::engine::general_purpose::STANDARD.encode(&q);
            }
            Ok(out)
        }
    }

    fn ok_out() -> CmdOut {
        CmdOut {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn honest_quote_out() -> CmdOut {
        CmdOut {
            stdout: HONEST_QUOTE.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn fail_out(msg: &str) -> CmdOut {
        CmdOut {
            stdout: String::new(),
            stderr: msg.into(),
            exit_code: 1,
        }
    }

    #[test]
    fn machine_defaults_to_the_cheap_prover() {
        assert_eq!(resolve_machine(None).unwrap(), CcMachine::Prover);
        assert_eq!(resolve_machine(Some("prover")).unwrap(), CcMachine::Prover);
    }

    /// The INVERSE of `full_is_refused_until_its_milestone_and_says_why`, which
    /// pinned the refusal until 2026-08-14: `--machine full` is a machine
    /// again, and it is indeed the a3 + H100 in SPOT that it designates.
    ///
    /// The test does not disappear with the refusal, it changes meaning: what
    /// mattered then was « we do not sell the prover's result at the H100's
    /// price », what matters now is « `full` designates exactly the
    /// machine the plan describes ». A failure here means either that the option
    /// was closed again without saying so, or that it points to a machine other
    /// than the one whose price the help announces.
    #[test]
    fn full_is_a_machine_again_and_it_is_the_spot_h100() {
        assert_eq!(resolve_machine(Some("full")).unwrap(), CcMachine::FullH100);
        let spec = CcMachine::FullH100.vm_spec(VM_NAME);
        assert_eq!(spec.machine_type, "a3-highgpu-1g");
        // SPOT is not a price choice: the confidential form of the H100
        // exists only in SPOT (GCP docs, read on 2026-08-14). A Standard one is
        // refused at creation, that is AFTER the plan's approval.
        assert_eq!(spec.provisioning_model, ProvisioningModel::Spot);
        assert!(
            spec.accelerators.iter().any(|a| a.model.contains("h100")),
            "{spec:?}"
        );
    }

    /// An unknown `--machine` is refused, never rounded to a neighbour. A
    /// failure here would mean a typo picks a machine for the operator — in
    /// one direction it answers a question they did not ask, in the other it
    /// bills $11/h.
    #[test]
    fn an_unknown_machine_is_refused_not_defaulted() {
        let e = resolve_machine(Some("h100")).unwrap_err();
        assert!(
            e.contains("prover"),
            "the refusal must name the option that works: {e}"
        );
        assert!(resolve_machine(Some("")).is_err());
        assert!(
            resolve_machine(Some("PROVER")).is_err(),
            "no case-insensitive guessing"
        );
    }

    /// `--cap nan` and `--cap inf` both parse into valid f64 and used to
    /// disarm the ceiling entirely. They are refused at the boundary, and the
    /// refusal names the value so the operator sees WHICH argument is
    /// nonsense. A failure here means three characters on the command line
    /// remove the only thing that stops the bill.
    #[test]
    fn a_cap_that_is_not_a_finite_amount_of_money_is_refused() {
        for absurd in ["nan", "inf", "-inf"] {
            let v: f64 = absurd.parse().unwrap();
            assert!(validate_cap(v).is_err(), "--cap {absurd} must be refused");
        }
        assert!(validate_cap(0.0).is_err());
        assert!(validate_cap(-1.0).is_err());
        assert_eq!(validate_cap(25.0), Ok(25.0));
        assert_eq!(validate_cap(0.01), Ok(0.01));
        // The message has to name a usable alternative, not just say no.
        assert!(validate_cap(f64::NAN).unwrap_err().contains("--cap 25"));
    }

    /// Repo invariant 5: the human approves what they can read. The plan must
    /// carry the create command VERBATIM — the same string `VmSpec` gives the
    /// runner — not a summary of it. A failure here means `x0ne cc` asks for
    /// approval of a command it never showed.
    #[test]
    fn the_plan_shows_the_create_command_verbatim() {
        let cfg = cfg();
        let verbatim = cfg.machine.vm_spec(VM_NAME).to_display_command();
        let rendered = render_plan(
            &cfg,
            &[CcStep::CreateVm, CcStep::WaitSsh, CcStep::Report],
            None,
        );
        assert!(
            rendered.contains(&verbatim),
            "plan must quote the command:\n{rendered}"
        );
        // The flags that make the VM confidential are part of what is being
        // approved — a plan that hid them would be approving a plain VM.
        assert!(
            rendered.contains("--confidential-compute-type=TDX"),
            "{rendered}"
        );
        assert!(rendered.contains("--shielded-secure-boot"), "{rendered}");
    }

    /// Every step gets a line, and the cost line comes with it: nothing runs
    /// that was not listed, and the price is on screen before the keypress.
    #[test]
    fn the_plan_lists_every_step_and_the_cost() {
        let plan = vec![
            CcStep::StartVm,
            CcStep::WaitSsh,
            CcStep::AttestTdx,
            CcStep::Report,
        ];
        let rendered = render_plan(&cfg(), &plan, None);
        for n in 1..=plan.len() {
            assert!(
                rendered.contains(&format!("  {n}. ")),
                "step {n} missing:\n{rendered}"
            );
        }
        assert!(rendered.contains("instances start"), "{rendered}");
        assert!(rendered.contains("configfs-tsm"), "{rendered}");
        assert!(
            rendered.contains("0.24 $/h"),
            "the price must be shown: {rendered}"
        );
        assert!(
            rendered.contains("cap 25 $"),
            "the cap must be shown: {rendered}"
        );
    }

    /// THE complete plan of `--machine full`, rendered in full — the M2 test
    /// deferred from task 1, due as soon as `full` becomes reachable again.
    ///
    /// Repository invariant 5: what is approved must be readable. The
    /// full machine's plan has fourteen steps since run 4
    /// (2026-08-14, `ArmGpu`), including a REBOOT, a GPU mutation, an
    /// install of 15 GB of weights and four confidentiality
    /// checks; `step_line` is exhaustive (no `_` arm), so a
    /// step without text would not compile — but a step rendered EMPTY, or
    /// two steps merged on one line, compile just fine and get a
    /// shorter list approved than the one that runs.
    ///
    /// The plan comes from `build_plan`, not from an array written here: what the
    /// test reads is the plan the command would build for a new project.
    #[test]
    fn the_full_plan_renders_fourteen_numbered_lines_none_of_them_empty() {
        let cfg = cfg_full();
        let plan = build_plan(&cfg, &obs_fresh()).unwrap();
        assert_eq!(plan.len(), 14, "the complete full plan: {plan:?}");
        let rendered = render_plan(&cfg, &plan, None);

        for n in 1..=plan.len() {
            let prefix = format!("  {n}. ");
            let line = rendered
                .lines()
                .find(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| panic!("step {n} has no line:\n{rendered}"));
            assert!(
                line[prefix.len()..].trim().len() > 10,
                "step {n} is announced by an empty or silent line: {line:?}"
            );
        }
        assert_eq!(
            rendered
                .lines()
                .filter(|l| l.trim_start().starts_with(char::is_numeric))
                .count(),
            14,
            "no extra step and no two steps merged on one line:\n{rendered}"
        );

        // The SPOT price is on the screen before the key press. 3.72 is the
        // expected figure, written by hand here so that this test fails if
        // `hourly_usd()` moves without anyone rereading what the plan
        // displays — replaying `hourly_usd()` would make the test say what the
        // code says (the div_ceil lesson, paid twice on `WaitSsh`).
        // SPOT spelled out, required by the brief and until now covered
        // only transitively (by the verbatim creation line).
        // It is not a price, it is an API constraint: the confidential form
        // of the H100 exists only in SPOT, so the instance can
        // be preempted at any moment, and the operator approves that too.
        assert!(
            rendered.contains("--provisioning-model=SPOT"),
            "the plan must say the machine is SPOT: {rendered}"
        );
        assert!(
            rendered.contains("3.72 $/h"),
            "the SPOT price must be shown: {rendered}"
        );
        assert!(
            (CcMachine::FullH100.hourly_usd() - 3.72).abs() < 1e-9,
            "and it must be THE ONE the budget gate uses, which reads hourly_usd()"
        );
        assert!(rendered.contains("cap 25 $"), "{rendered}");
    }

    /// The `ArmGpu` line announces a MUTATION of the GPU, and its order.
    ///
    /// Repository invariant 5: what is approved must be readable. Two
    /// things must be refusable on reading this line — that a
    /// command turns the GPU on, and that it runs only AFTER the attestation that
    /// precedes it. The latter is the security property:
    /// the confidential GPU arrives disarmed so that the guest can refuse to
    /// hand it work before having attested it. A failure here means
    /// that the plan gets a read approved where there is a mutation.
    #[test]
    fn the_arm_gpu_line_announces_a_mutation_and_its_order() {
        let rendered = render_plan(&cfg_full(), &[CcStep::ArmGpu], None);
        assert!(rendered.contains("-srs 1"), "the exact command: {rendered}");
        assert!(
            rendered.contains("ATTESTED"),
            "the order is the guarantee: {rendered}"
        );
        assert!(rendered.contains("per boot"), "and its scope: {rendered}");
    }

    /// The `InstallX0ne` line describes what the step REALLY does since
    /// task 6, and not what it promised when it was failing.
    ///
    /// Repository invariant 5: what is approved must be readable.
    /// Four things must be refusable on reading this line —
    /// that a binary from this machine is UPLOADED, that the enclave's x0ne
    /// is wired to the local vLLM and to nothing else, that its runtime
    /// policy is deny-by-default rather than the permissive one of the
    /// operator's workstation, and what that policy GRANTS. A failure here means
    /// that the plan announces a file copy where it installs and configures
    /// an agent.
    ///
    /// The fourth point dates from 2026-08-15. « Deny by default » stopped
    /// being the whole truth the day the policy enabled writing and
    /// execution: an operator who read only the old line approved
    /// a demo and received an agent able to edit his VM's home. The
    /// three clauses hold the three independent limits — which
    /// commands, where, and what never happens.
    #[test]
    fn the_install_x0ne_line_says_it_uploads_a_binary_and_confines_it() {
        let rendered = render_plan(&cfg_full(), &[CcStep::InstallX0ne], None);
        assert!(
            rendered.contains("scp"),
            "the upload must be announced: {rendered}"
        );
        assert!(
            rendered.contains(X0NE_VERSION),
            "and the copied version: {rendered}"
        );
        assert!(
            rendered.contains(&format!("127.0.0.1:{VLLM_PORT}")),
            "{rendered}"
        );
        assert!(
            rendered.contains("deny by default"),
            "the policy too: {rendered}"
        );
        assert!(
            rendered.contains("read-only commands"),
            "execution is granted, the operator must read it: {rendered}"
        );
        assert!(
            rendered.contains("home-wide reads/writes"),
            "writing is granted, the operator must read it: {rendered}"
        );
        assert!(
            rendered.contains("no egress"),
            "this policy's red line is readable in the plan: {rendered}"
        );
        // The cap, computed by hand from the constants (460 s → 8 min),
        // NEVER replayed with the same expression as `step_line`: the test that
        // recomputes the code's rounding agrees with it while both are
        // wrong, which happened twice on `WaitSsh`.
        assert!(rendered.contains("at most 8 min"), "{rendered}");
        assert!(
            8 * 60 >= scp_budget_secs(),
            "the announced 8 min must cover the real cap of {} s",
            scp_budget_secs()
        );
    }

    /// The SSH line quotes a wall-clock ceiling to someone paying by the
    /// hour, so it must be DERIVED from the constants that produce it. The
    /// literal it replaced said "~5 min" while the real ceiling was 2.6 h. A
    /// failure here means the plan is making a promise the code does not keep.
    #[test]
    fn the_wait_ssh_line_states_the_real_ceiling_not_a_stale_literal() {
        let rendered = render_plan(&cfg(), &[CcStep::WaitSsh], None);
        // "15 min" is hand-computed from the constants (90 s first probe +
        // 26 × 20 s warm probes + 27 × 10 s pauses = 880 s, rounded UP), NOT
        // derived from wait_ssh_budget_secs(): recomputing the rounding the
        // way `step_line` does made code and test agree while both were
        // wrong, twice — the truncating `/ 60` announced "14 min" over a
        // 14 min 40 s ceiling, and the `minutes * 60 >= budget` assert that
        // replaced it holds for every u64 under div_ceil, so it could never
        // fail (final review, 2026-08-14).
        assert!(rendered.contains("at most 15 min"), "{rendered}");
        assert!(
            15 * 60 >= wait_ssh_budget_secs(),
            "the announced 15 min must cover the real {} s ceiling",
            wait_ssh_budget_secs()
        );
        assert!(
            rendered.contains(&format!("{SSH_RETRIES} probes")),
            "{rendered}"
        );
        // Both budgets are stated, because they differ and the operator is
        // watching a clock: the first probe generates an SSH key, the rest do
        // not (review round 2).
        assert!(
            rendered.contains(&format!("{SSH_FIRST_PROBE_TIMEOUT_SECS} s for the first")),
            "the first probe's larger budget must be stated: {rendered}"
        );
        assert!(
            rendered.contains(&format!("then {SSH_PROBE_TIMEOUT_SECS} s")),
            "the warm-probe timeout is what bounds a filtered port: {rendered}"
        );
        // The bound that finding I1 was about: no probe may cost the 300 s
        // that `instances create` is allowed, first one included.
        assert!(
            SSH_PROBE_TIMEOUT_SECS < GCLOUD_TIMEOUT_SECS
                && SSH_FIRST_PROBE_TIMEOUT_SECS < GCLOUD_TIMEOUT_SECS,
            "no SSH probe may inherit the create timeout"
        );
        // …and the concession to the first probe must not have quietly
        // reopened it: the whole step stays under a quarter of an hour.
        assert!(
            wait_ssh_budget_secs() <= 15 * 60,
            "the WaitSsh ceiling must stay bounded, got {} s",
            wait_ssh_budget_secs()
        );
    }

    /// The `ServeVllm` plan line announces the wait AND its duration.
    ///
    /// It is the longest wait of the pipeline — the first start
    /// downloads ~15 GB of weights into RAM before anything listens — and
    /// a step announced as « starts a service » that then takes half
    /// an hour on a 3.72 $/h machine is not what was approved
    /// (repository invariant 5). The duration is DERIVED from
    /// `vllm_start_budget_secs()`, never typed by hand: it is the
    /// « ~5 min » literal of `WaitSsh` that cost us this rule.
    ///
    /// « 30 min » is computed by hand (36 × (30 + 20) = 1800 s), not by
    /// replaying the code's division — the trap into which the
    /// `WaitSsh` assertion had fallen twice.
    #[test]
    fn the_vllm_plan_lines_state_their_real_ceilings_and_the_port() {
        let full = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: VM_NAME.into(),
            cap_usd: DEFAULT_CAP_USD,
        };
        let rendered = render_plan(
            &full,
            &[
                CcStep::InstallVllm,
                CcStep::FetchModel,
                CcStep::ServeVllm,
                CcStep::ValidateEncryption,
                CcStep::HealthCheck,
            ],
            None,
        );
        assert!(rendered.contains("at most 30 min"), "{rendered}");
        assert!(
            30 * 60 >= vllm_start_budget_secs(),
            "the announced ceiling must cover the real one"
        );
        assert!(rendered.contains("at most 5 min"), "{rendered}");
        assert!(
            5 * 60 >= vllm_health_budget_secs(),
            "same for the health wait"
        );
        assert!(
            rendered.contains("15 GB"),
            "say what the wait is for: {rendered}"
        );
        // What the wait WATCHES, since run 4 (2026-08-14). The old
        // line promised a wait on the port: the cap was accurate and the
        // sentence misleading, and that is what made 30 minutes and 1.26 $ go
        // into probing a port while the service restarted 46 times. What
        // is approved now is a wait that gives up early.
        assert!(rendered.contains("restart counter"), "{rendered}");
        assert!(
            rendered.contains("a crash loop or an empty tmpfs stops the wait"),
            "the wait must announce that it can give up early: {rendered}"
        );
        // And the pinned interpreter is part of what is approved: it is
        // what decides whether vLLM starts (run 4, flashinfer on python 3.10).
        assert!(
            rendered.contains(&format!("python {VLLM_PYTHON}")),
            "{rendered}"
        );
        // The pinned version and the port are in the approved text, and the
        // port comes from the constant: a literal would diverge the day
        // `VLLM_PORT` changes, which is exactly what this assertion
        // catches.
        assert!(rendered.contains(VLLM_VERSION), "{rendered}");
        // The transformers pin is part of what is approved: without
        // it the server dies at startup, so « install vLLM 0.10.0 »
        // alone would describe a stack that has never served (measured 2026-08-14).
        assert!(rendered.contains(TRANSFORMERS_VERSION), "{rendered}");
        assert!(
            rendered.contains(&format!("127.0.0.1:{VLLM_PORT} only")),
            "{rendered}"
        );
        // What the plan promises, spelled out.
        assert!(
            rendered.contains("TDX-encrypted RAM, never the disk"),
            "{rendered}"
        );
        // Invariant 5, run 5 (2026-08-14): the logind drop-in is the ONLY
        // persistent system configuration this pipeline writes outside
        // the vLLM unit. A human approves what he reads; if it is not named
        // here, it is named nowhere, and nobody will know how to undo it.
        // The path comes from THE constant the script writes: a literal here
        // would let the plan announce a file the pipeline no longer touches
        // (a mutation measured in review — both suites stayed
        // green while the two diverged).
        assert!(rendered.contains(LOGIND_DROPIN), "{rendered}");
        assert!(rendered.contains("RemoveIPC=no"), "{rendered}");
        // And the restart: it is the ACTING half of what is approved.
        // Writing the file changes nothing until the daemon has reread its
        // configuration, so a plan line that omits the restart announces an
        // inert operation.
        assert!(rendered.contains("restart systemd-logind"), "{rendered}");
        // Three guarantees and one observation, stated separately (minor m4, review
        // round 1). The disk can NOT fail — GCP encrypts every persistent
        // disk at rest with no way to disable it — so announcing it as
        // a fourth thing that « must hold » sells a check that can
        // never trigger. A failure here means that the plan promises
        // one more guarantee than it carries.
        assert!(
            rendered.contains("any of the three failing stops the run"),
            "{rendered}"
        );
        assert!(
            rendered.contains("record which key encrypts the boot disk"),
            "the disk read is an observation and must read as one: {rendered}"
        );
    }

    /// The pause between two probes, that is the ONLY thing that makes
    /// a retry loop wait.
    ///
    /// Measured (mutation M18, task 4): deleting the vLLM branch left
    /// the WHOLE suite green. `BridgeExec` requires a live `Bridge`, so nothing
    /// tests it; the rule, however, can be tested. A failure here means that one of
    /// the two budget sizings — 15 min of SSH, 30 min of vLLM
    /// startup — is consumed in one second, which is exactly the defect that
    /// review round 1 paid for once.
    ///
    /// The two labels do NOT behave the same, and that is the whole content of
    /// the function: a successful SSH probe ends the wait, a successful vLLM
    /// probe proves nothing (`ss` exits 0 without mentioning our port).
    #[test]
    fn the_probe_pause_waits_for_vllm_even_when_the_probe_succeeded() {
        assert_eq!(
            probe_pause_secs(WAIT_SSH_DISPLAY, 255),
            Some(SSH_PROBE_PAUSE_SECS)
        );
        assert_eq!(
            probe_pause_secs(WAIT_SSH_DISPLAY, 0),
            None,
            "a successful SSH probe ends the wait: pausing after it burns 10 s for nothing"
        );
        assert_eq!(
            probe_pause_secs(WAIT_VLLM_DISPLAY, 0),
            Some(VLLM_PROBE_PAUSE_SECS),
            "THE case: `ss` exits 0 while saying nothing about our port, so a successful \
             probe is not a finished wait"
        );
        assert_eq!(
            probe_pause_secs(WAIT_VLLM_DISPLAY, 7),
            Some(VLLM_PROBE_PAUSE_SECS)
        );
        // The vLLM wait label carries the download figure
        // since run 4 (2026-08-14): « wait for vLLM — 4.2G/15G
        // downloaded ». Under an equality check, this suffix would silently switch off the
        // pause and the 36 probes would burn in one second — the SSH bug of
        // review round 1, disguised as a progress indicator. A failure here
        // means that showing progress costs the wait itself.
        assert_eq!(
            probe_pause_secs(&format!("{WAIT_VLLM_DISPLAY} — 4.2G/15G downloaded"), 0),
            Some(VLLM_PROBE_PAUSE_SECS),
            "the progress suffix must not switch off the pause"
        );
        // And an ordinary command never sleeps: `instances create` already pays
        // for its time, and 20 s more per gcloud call are billed.
        assert_eq!(probe_pause_secs("probe instance", 0), None);
        assert_eq!(probe_pause_secs("probe instance", 1), None);
        // The prefix stays a PREFIX, not a substring: a command that
        // mentions the wait elsewhere in its label does not sleep.
        assert_eq!(
            probe_pause_secs(&format!("about to {WAIT_VLLM_DISPLAY}"), 0),
            None
        );
    }

    /// THE gate, in its pure form. Everything that is not an explicit yes is
    /// a no — EOF included, which `read_line_raw` delivers as "/quit". A
    /// failure here means a closed stdin, a stray newline or an unknown key
    /// creates a paying VM without a human saying so.
    #[test]
    fn only_an_explicit_yes_approves_the_plan() {
        assert!(parse_confirm("y"));
        assert!(parse_confirm("Y\n"));
        assert!(parse_confirm(" yes \n"));
        for refusal in [
            "", "\n", "n", "no", "/quit", "sure", "ok", "1", "yeah", "yy",
        ] {
            assert!(!parse_confirm(refusal), "{refusal:?} must not approve");
        }
    }

    /// The exit choice fails closed towards the cheap, reversible action.
    /// Silence stops the meter; only an explicit `d` even reaches the second
    /// confirmation. A failure here means an unreadable answer either keeps
    /// billing or deletes a disk.
    #[test]
    fn an_unreadable_exit_answer_stops_the_vm() {
        // Read with the entry OFFERED, so that what this test pins is the
        // fallback and not the gate: `e` gets its own test below.
        assert_eq!(parse_exit_choice("k", true), ExitChoice::Keep);
        assert_eq!(parse_exit_choice("KEEP\n", true), ExitChoice::Keep);
        assert_eq!(parse_exit_choice("d", true), ExitChoice::Destroy);
        assert_eq!(parse_exit_choice("destroy", true), ExitChoice::Destroy);
        for silence in ["", "\n", "/quit", "s", "stop", "x", "delete", "yes"] {
            assert_eq!(
                parse_exit_choice(silence, true),
                ExitChoice::Stop,
                "{silence:?} must fall back to stopping"
            );
        }
    }

    /// The `vm` line of the report is the one an operator reads to know
    /// whether they are still paying, so no state may borrow another's word,
    /// and the two states that cost money must name the command that fixes
    /// them. A failure here means the report says "stopped" over a meter that
    /// is still running — the most expensive thing this command can lie about.
    #[test]
    fn no_vm_state_can_borrow_another_states_claim() {
        let cfg = cfg();
        let states = [
            VmEnd::NeverUp,
            VmEnd::Running,
            VmEnd::Stopped,
            VmEnd::StopFailed,
            VmEnd::StoppedAfterBridgeDeath,
            VmEnd::Uncertain,
        ];
        for (i, a) in states.iter().enumerate() {
            for b in &states[i + 1..] {
                assert_ne!(
                    a.label(&cfg),
                    b.label(&cfg),
                    "{a:?} and {b:?} must not read alike"
                );
            }
        }

        // THE property, and it is not the pairwise check above: distinctness
        // only proves the five differ from each OTHER, so "stopped (probably)"
        // would sail through it. A state where the meter may still be running
        // must not contain the word at all. Asserted on the VERDICT, so the
        // `…instances stop…` lever cannot satisfy or defeat it, and folded to
        // lowercase so "Stopped" cannot slip past either.
        for billing in [VmEnd::Running, VmEnd::StopFailed, VmEnd::Uncertain] {
            assert!(
                !billing.verdict().to_lowercase().contains("stopped"),
                "{billing:?} may still be billing and must not read as stopped: {}",
                billing.verdict()
            );
        }
        assert!(VmEnd::StopFailed.verdict().contains("STILL RUNNING"));

        // …and the two costly states still name a command the operator can run.
        assert!(
            VmEnd::StopFailed
                .lever(&cfg)
                .unwrap()
                .contains("gcloud compute instances stop x0ne-cc"),
            "the costliest state must name its lever"
        );
        assert!(
            VmEnd::Uncertain
                .lever(&cfg)
                .unwrap()
                .contains("gcloud compute instances list"),
            "an unknown must name its lever"
        );
        // The label is verdict + lever, so the report carries both.
        for s in [VmEnd::StopFailed, VmEnd::Uncertain] {
            let label = s.label(&cfg);
            assert!(label.contains(s.verdict()), "{label}");
            assert!(label.contains(&s.lever(&cfg).unwrap()), "{label}");
        }
        // A state with nothing to fix must not invent a command.
        assert_eq!(VmEnd::Stopped.lever(&cfg), None);
        assert_eq!(VmEnd::Stopped.label(&cfg), "stopped");
        // The rescue state is one of those: the meter is off, so it says so
        // plainly and offers no command — but it still names what went wrong,
        // because a runtime that died is the thing the operator has to go read
        // a log about.
        assert_eq!(VmEnd::StoppedAfterBridgeDeath.lever(&cfg), None);
        let rescued = VmEnd::StoppedAfterBridgeDeath.verdict();
        assert!(rescued.contains("stopped"), "{rescued}");
        assert!(rescued.contains("bridge"), "{rescued}");
    }

    /// `x0ne cc --yes && echo proven` must not print "proven" over a refusal.
    /// Exit 0 is reserved for `Proven` — a shell reads the status, not the
    /// report. A failure here means CI treats a rejected attestation, a
    /// broken pipeline, or an unauthenticated gcloud as success, which is the
    /// only thing this whole command exists to rule out.
    ///
    /// The `Proven` bundle is built the way `cc_runner`'s own tests build one
    /// (synthetic TD Quote v4, nonce echoed in report_data) — no VM, no spend.
    #[test]
    fn exit_zero_is_reserved_for_a_proven_enclave() {
        use x0ne_orchestrator::cc_runner::ProofBundle;

        let nonce = [7u8; 64];
        let mut quote = vec![0u8; 48 + 584 + 8];
        quote[0] = 4;
        quote[4..8].copy_from_slice(&0x81u32.to_le_bytes());
        quote[48 + 136..48 + 184].copy_from_slice(&[0xAB; 48]);
        quote[48 + 520..48 + 584].copy_from_slice(&nonce);
        let (claims, verdict) = x0ne_confidential::tdx_quote::verify(&quote, &nonce);
        let bundle = ProofBundle::from_verdict(
            &verdict,
            claims.as_ref(),
            x0ne_confidential::tdx_quote::hex(&nonce),
        )
        .expect("a synthetic quote with a matching nonce must verify");

        assert!(exit_status_for(&CcOutcome::Proven {
            proof: bundle,
            gpu: None,
            encryption: None
        })
        .is_ok());
        assert!(exit_status_for(&CcOutcome::Refused {
            reason: "nonce mismatch".into()
        })
        .is_err());
        assert!(exit_status_for(&CcOutcome::Failed {
            step: CcStep::WaitSsh,
            error: "timeout".into(),
            proof: None,
            gpu: None,
            encryption: None
        })
        .is_err());

        // The error line must not carry the remote-controlled text; that is
        // what the sanitized report is for.
        let err = exit_status_for(&CcOutcome::Refused {
            reason: "\nENCLAVE PROVEN".into(),
        })
        .unwrap_err()
        .to_string();
        assert!(
            !err.contains("PROVEN"),
            "the reason must not reach the error line: {err}"
        );
    }

    // ── The budget loop and the exit path ────────────────────────────────

    /// A clean run leaves the VM up (nothing stopped it) and reports no
    /// failure — the baseline the other loop tests are read against.
    #[tokio::test]
    async fn a_clean_run_leaves_the_vm_running_and_stops_nothing() {
        let plan = vec![CcStep::CreateVm, CcStep::WaitSsh, CcStep::Report];
        // create ok, ssh probe ok. Report issues no command.
        let mut exec = ScriptedExec::with(vec![ok_out(), ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (vm, failed) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(vm, VmEnd::Running);
        assert!(failed.is_none());
        assert!(
            !exec.issued(&["stop"]),
            "a successful run must not stop the VM"
        );
    }

    /// An exec whose bridge dies after `alive_for` commands and stays dead —
    /// what `BridgeExec` produces once `send_action` starts returning `Err`.
    struct DeadBridgeExec {
        calls: usize,
        alive_for: usize,
    }

    #[async_trait]
    impl CcExec for DeadBridgeExec {
        async fn run(&mut self, _d: &str, argv: &[String], _t: u64) -> Result<CmdOut> {
            self.calls += 1;
            if self.calls > self.alive_for {
                return Err(ExecTransportDown::raise(
                    "x0ne-runtime returned an empty response line",
                ));
            }
            let _ = argv;
            Ok(ok_out())
        }
    }

    /// A dead runtime ends the RUN, and the report says what died.
    ///
    /// Measured 2026-08-14, first real `--machine full`: the runtime died,
    /// `WaitSsh` spent 26 probes on a pipe nobody was reading (~10 min at
    /// 3.72 $/h) and the pipeline then reported that the VM had not come back
    /// from its reboot — a diagnosis about a machine nobody had managed to ask.
    /// A failure here means either the run is burning its retries on a corpse
    /// again, or the operator is being told the wrong thing broke.
    #[tokio::test]
    async fn a_dead_bridge_ends_the_run_and_the_report_does_not_blame_the_vm() {
        let plan = vec![CcStep::CreateVm, CcStep::WaitSsh, CcStep::Report];
        // The create goes through; everything after it meets a dead bridge.
        let mut exec = DeadBridgeExec {
            calls: 0,
            alive_for: 1,
        };
        let (vm, failed) = {
            let mut runner = CcRunner::new(&mut exec, cfg());
            run_plan(
                &mut runner,
                &cfg(),
                &plan,
                VmEnd::NeverUp,
                Instant::now(),
                &AtomicUsize::new(0),
            )
            .await
        };
        let (step, why) = failed.expect("a dead bridge must fail the run");
        assert_eq!(step, CcStep::WaitSsh);
        assert!(
            why.contains("bridge"),
            "the report must name what died: {why}"
        );
        assert!(
            why.contains("UNKNOWN"),
            "and admit the VM state is no longer known: {why}"
        );
        assert!(
            why.contains("gcloud compute instances list"),
            "and hand over the command that answers it: {why}"
        );
        assert!(
            !why.contains("SSH not reachable"),
            "blaming the VM's boot for a transport failure is the lie removed on 2026-08-14: {why}"
        );
        // The create and the ONE probe that found the corpse. Not 1 +
        // SSH_RETRIES, and not a third: the stop this failure owes must NOT be
        // sent into the dead bridge — see the rescue test below.
        assert_eq!(
            exec.calls, 2,
            "the run must stop on the fatal probe, not spend {SSH_RETRIES}"
        );
        // Still running, and handed back saying so. The caller is the layer
        // that can start a fresh runtime; claiming `Stopped` here would be the
        // report lying about the meter.
        assert_eq!(vm, VmEnd::Running);
    }

    /// A bridge that answers every action with a fixed exit code, and keeps
    /// what it was asked. Enough to drive `BridgeExec` — the rescue path builds
    /// a real one of those over a real `Box<dyn Bridge>`, so the test covers
    /// the same code production runs, minus the process spawn.
    struct FakeBridge {
        sent: Arc<Mutex<Vec<Value>>>,
        exit_code: i64,
    }

    #[async_trait]
    impl Bridge for FakeBridge {
        async fn send_action(&mut self, action: &Value) -> Result<Value> {
            self.sent.lock().unwrap().push(action.clone());
            Ok(json!({
                "success": true,
                "output": {"stdout": "", "stderr": "", "exit_code": self.exit_code},
            }))
        }
        async fn health_check(&mut self) -> Result<bool> {
            Ok(true)
        }
    }

    /// THE budget property of round 2: when the bridge dies, the stop still
    /// happens — through a runtime that is not the dead one.
    ///
    /// Run 1 of the H100 milestone (2026-08-14) failed this: the runtime died,
    /// the pipeline's own stop went down the same dead pipe, and the a3 went on
    /// billing at 3.72 $/h behind a report that could only say STILL RUNNING.
    /// A failure here means the cap no longer protects an operator who is not
    /// watching, which is the promise the whole command is built on.
    #[tokio::test]
    async fn a_dead_bridge_gets_the_vm_stopped_through_a_fresh_runtime() {
        let plan = vec![CcStep::CreateVm, CcStep::WaitSsh, CcStep::Report];
        let mut dead = DeadBridgeExec {
            calls: 0,
            alive_for: 1,
        };
        let (vm, failed) = {
            let mut runner = CcRunner::new(&mut dead, cfg());
            let out = run_plan(
                &mut runner,
                &cfg(),
                &plan,
                VmEnd::NeverUp,
                Instant::now(),
                &AtomicUsize::new(0),
            )
            .await;
            // The runner is the one that knows its transport is gone; that is
            // what the caller reads to determine. Checked here, inside the same
            // borrow, because it is the input to the rescue below.
            assert!(
                runner.transport_is_down(),
                "the runner must know its transport died"
            );
            out
        };
        assert!(failed.is_some());
        assert_eq!(
            vm,
            VmEnd::Running,
            "the pipeline leaves the meter for the caller to settle"
        );
        assert_eq!(
            dead.calls, 2,
            "not one command may be sent into the corpse after the fatal one"
        );

        // The caller's half, on a fresh bridge that works.
        let sent = Arc::new(Mutex::new(Vec::new()));
        let spawned = AtomicUsize::new(0);
        let settled = rescue_stop_if_the_bridge_died(vm, true, &cfg(), || {
            spawned.fetch_add(1, Ordering::Relaxed);
            let sent = Arc::clone(&sent);
            async move { Ok(Box::new(FakeBridge { sent, exit_code: 0 }) as Box<dyn Bridge>) }
        })
        .await;

        assert_eq!(
            spawned.load(Ordering::Relaxed),
            1,
            "one fresh runtime, not a resurrection loop"
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "one stop on it, and only one");
        let args = sent[0]["args"]
            .as_array()
            .expect("the stop carries its argv");
        assert!(args.iter().any(|a| a == "stop"), "{sent:?}");
        assert!(
            args.iter().any(|a| a == "--discard-local-ssd=true"),
            "the rescue stop is the same command as the normal one, flag included: {sent:?}"
        );
        assert_eq!(settled, VmEnd::StoppedAfterBridgeDeath);
        assert!(
            settled.label(&cfg()).contains("fresh runtime"),
            "{}",
            settled.label(&cfg())
        );
        assert!(
            !settled.verdict().to_lowercase().contains("still running"),
            "the meter is off and the report must say so"
        );
    }

    /// The guard, at its FOUR corners — not on the diagonal.
    ///
    /// Review round 3, finding I1: the round-2 tests all sat on
    /// (Running, down) and one (Stopped, alive), which is the diagonal, and
    /// both mutations survived it — deleting `vm != Running` and deleting
    /// `!transport_down` each left 94/94 green. A condition is only pinned by
    /// the cases where its two halves DISAGREE, so those are the two this test
    /// adds:
    ///
    ///   * (Running, alive) — an ordinary broken run. The pipeline stopped the
    ///     VM on the bridge it already had; respawning here would cost a
    ///     process per failed run for nothing.
    ///   * (Stopped, down) — the transport died, but the meter is already off.
    ///     Respawning here would issue a second stop against a stopped VM, on
    ///     the strength of a flag that says nothing about the meter.
    #[tokio::test]
    async fn the_rescue_fires_on_both_halves_of_its_guard_and_on_nothing_else() {
        let spawned = AtomicUsize::new(0);
        let spawn = || {
            spawned.fetch_add(1, Ordering::Relaxed);
            async move {
                Ok(Box::new(FakeBridge {
                    sent: Arc::new(Mutex::new(vec![])),
                    exit_code: 0,
                }) as Box<dyn Bridge>)
            }
        };

        // Corner 1 — transport alive, VM up: `run_plan` already stopped it, or
        // chose not to. Not our business.
        assert_eq!(
            rescue_stop_if_the_bridge_died(VmEnd::Running, false, &cfg(), spawn).await,
            VmEnd::Running,
            "an ordinary failure must not start a second runtime"
        );

        // Corner 2 — transport down, meter already off. The flag alone is not a
        // reason to stop anything.
        assert_eq!(
            rescue_stop_if_the_bridge_died(VmEnd::Stopped, true, &cfg(), spawn).await,
            VmEnd::Stopped,
            "a dead bridge over a stopped VM must not re-stop it"
        );

        // Corner 3 — neither: the ordinary end of an ordinary run.
        assert_eq!(
            rescue_stop_if_the_bridge_died(VmEnd::Stopped, false, &cfg(), spawn).await,
            VmEnd::Stopped
        );

        assert_eq!(
            spawned.load(Ordering::Relaxed),
            0,
            "not one runtime may be started outside (Running, transport down)"
        );
        // The fourth corner — (Running, down), the one that DOES fire — is
        // `a_dead_bridge_gets_the_vm_stopped_through_a_fresh_runtime`.
    }

    /// One attempt, and the truth if it fails. A fresh runtime that cannot stop
    /// the VM either lands on the state that says STILL RUNNING and hands over
    /// the command — never on a third bridge, and never on a claim of
    /// "stopped".
    #[tokio::test]
    async fn a_rescue_that_fails_too_reports_still_running_with_its_lever() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let spawned = AtomicUsize::new(0);
        let settled = rescue_stop_if_the_bridge_died(VmEnd::Running, true, &cfg(), || {
            spawned.fetch_add(1, Ordering::Relaxed);
            let sent = Arc::clone(&sent);
            // gcloud itself refuses — the VM is up and stays up.
            async move { Ok(Box::new(FakeBridge { sent, exit_code: 1 }) as Box<dyn Bridge>) }
        })
        .await;
        assert_eq!(settled, VmEnd::StopFailed);
        assert_eq!(
            spawned.load(Ordering::Relaxed),
            1,
            "one bridge, then we stop trying"
        );
        assert_eq!(sent.lock().unwrap().len(), 1, "one stop, no retry loop");
        assert!(settled
            .label(&cfg())
            .contains("gcloud compute instances stop x0ne-cc"));
    }

    /// And if no runtime can be started at all, the report says the meter is
    /// still running rather than inventing a state.
    #[tokio::test]
    async fn a_rescue_with_no_runtime_at_all_still_tells_the_truth() {
        let settled = rescue_stop_if_the_bridge_died(VmEnd::Running, true, &cfg(), || async {
            Err(anyhow::anyhow!("x0ne-runtime binary not found"))
        })
        .await;
        assert_eq!(settled, VmEnd::StopFailed);
        assert!(settled.verdict().contains("STILL RUNNING"));
    }

    /// THE budget property. With no budget left the loop must refuse BEFORE
    /// the first paying step: nothing is created, and the failure names the
    /// cap. A failure here means the ceiling is decorative.
    #[tokio::test]
    async fn an_exhausted_cap_refuses_before_the_first_paying_step() {
        let plan = vec![CcStep::CreateVm, CcStep::WaitSsh, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![ok_out(), ok_out()]);
        let broke = CcConfig {
            cap_usd: 0.0,
            ..cfg()
        };
        let mut runner = CcRunner::new(&mut exec, broke.clone());
        let (vm, failed) = run_plan(
            &mut runner,
            &broke,
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(vm, VmEnd::NeverUp);
        let (step, why) = failed.expect("an exhausted cap must fail the run");
        assert_eq!(step, CcStep::CreateVm);
        assert!(why.contains("cap"), "the failure must name the cap: {why}");
        assert!(
            exec.sent.is_empty(),
            "not one command may be issued with no budget"
        );
    }

    /// Ctrl+C must stop the METER, not just the process. Before this, `x0ne
    /// cc` was dispatched before the signal handler and had none of its own,
    /// so an interrupt during the 14 min SSH wait killed the process and left
    /// the VM RUNNING — with the budget gate gone too, since the gate lives
    /// in the process that just died.
    ///
    /// This is the decision half, which is what can be tested without sending
    /// a signal: what the cloud reports becomes what the report claims, and
    /// anything found running gets stopped. A failure here means Ctrl+C leaves
    /// a paying VM behind, or claims a state nobody verified.
    #[tokio::test]
    async fn an_interrupt_settles_the_meter_from_what_the_cloud_reports() {
        // Found RUNNING → stopped, and the stop really is issued.
        let running = CmdOut {
            stdout: r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"z/us-central1-a"}]"#.into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let mut exec = ScriptedExec::with(vec![running, ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        assert_eq!(
            settle_after_interrupt(&mut runner, &cfg()).await,
            VmEnd::Stopped
        );
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);

        // Found absent → nothing to stop, and nothing is pretended.
        let mut exec = ScriptedExec::with(vec![CmdOut {
            stdout: "[]".into(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        assert_eq!(
            settle_after_interrupt(&mut runner, &cfg()).await,
            VmEnd::NeverUp
        );
        assert!(!exec.issued(&["instances", "stop"]));

        // The probe itself fails → we say we do not know, we do not guess.
        let mut exec = ScriptedExec::with(vec![fail_out("API unreachable")]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        assert_eq!(
            settle_after_interrupt(&mut runner, &cfg()).await,
            VmEnd::Uncertain
        );
    }

    /// The mapping both the interrupt path and the failed-create path share.
    /// They ask the same question — "is anything billing?" — and must not
    /// answer it differently. `Transitional` is an unknown, not a
    /// reassurance: it is the state that can still become RUNNING.
    #[test]
    fn an_unanswered_or_moving_observation_is_never_read_as_idle() {
        assert_eq!(
            settle_vm_end(Some(&InstanceState::Running { zone: "z".into() })),
            VmEnd::Running
        );
        assert_eq!(
            settle_vm_end(Some(&InstanceState::Stopped { zone: "z".into() })),
            VmEnd::Stopped
        );
        assert_eq!(settle_vm_end(Some(&InstanceState::Absent)), VmEnd::NeverUp);
        assert_eq!(
            settle_vm_end(Some(&InstanceState::Transitional {
                status: "STAGING".into()
            })),
            VmEnd::Uncertain
        );
        assert_eq!(
            settle_vm_end(None),
            VmEnd::Uncertain,
            "a failed probe is not an answer"
        );
    }

    /// The caller names the interrupted step from `in_flight`, so the recap
    /// says "interrupted during WaitSsh" instead of guessing. A failure here
    /// means the index is stale and the report blames the wrong step.
    #[tokio::test]
    async fn the_in_flight_index_tracks_the_step_being_run() {
        let plan = vec![CcStep::CreateVm, CcStep::AttestTdx, CcStep::Report];
        let in_flight = AtomicUsize::new(0);
        let mut exec = ScriptedExec::with(vec![ok_out(), fail_out("boom"), ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let _ = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &in_flight,
        )
        .await;
        assert_eq!(
            plan[in_flight.load(Ordering::Relaxed)],
            CcStep::AttestTdx,
            "the index must point at the step that was running when it stopped"
        );
    }

    /// The cap guards spending, and `Report` spends nothing. A run that has
    /// already earned a verdict can cross the ceiling in the seconds after
    /// `AttestTdx`; gating the final step then converted the outcome into
    /// `Failed{Report}` and threw the proof away — the money was spent, the
    /// attestation succeeded, and the gate meant to protect the operator
    /// destroyed the one artefact they paid for.
    ///
    /// The test shows the discrimination, not just the permission: with the
    /// budget exhausted, a paying step IS refused and `Report` is NOT.
    #[tokio::test]
    async fn an_exhausted_cap_gates_paying_steps_but_never_the_report() {
        let broke = CcConfig {
            cap_usd: 0.0,
            ..cfg()
        };

        // A paying step is still refused, cap exhausted.
        let mut exec = ScriptedExec::default();
        let mut runner = CcRunner::new(&mut exec, broke.clone());
        let (_, failed) = run_plan(
            &mut runner,
            &broke,
            &[CcStep::AttestTdx],
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(
            failed.expect("a paying step must be gated").0,
            CcStep::AttestTdx
        );

        // `Report` is not: it issues no command and costs nothing, so the
        // verdict already earned survives to be printed.
        let mut exec = ScriptedExec::default();
        let mut runner = CcRunner::new(&mut exec, broke.clone());
        let (vm, failed) = run_plan(
            &mut runner,
            &broke,
            &[CcStep::Report],
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert!(
            failed.is_none(),
            "an exhausted cap must not convert a finished run into a failure at Report"
        );
        assert_eq!(vm, VmEnd::NeverUp);
        assert!(
            exec.sent.is_empty(),
            "Report issues no cloud call, gated or not"
        );
    }

    /// A VM that was already billing when the cap fired gets stopped — the
    /// gate is not just a refusal to start, it turns the meter off.
    #[tokio::test]
    async fn an_exhausted_cap_stops_a_vm_that_was_already_billing() {
        let plan = vec![CcStep::WaitSsh, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![ok_out()]); // the stop
        let broke = CcConfig {
            cap_usd: 0.0,
            ..cfg()
        };
        let mut runner = CcRunner::new(&mut exec, broke.clone());
        let (vm, failed) = run_plan(
            &mut runner,
            &broke,
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert!(failed.is_some());
        assert_eq!(vm, VmEnd::Stopped);
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// THE wiring test of the replan (task 3, pin 1). `CcRunner::replan_
    /// after_ssh` has its own tests; this one proves the only thing they
    /// cannot prove — that the loop that spends the money calls it,
    /// and that a removed step really is a step that does not go out.
    ///
    /// The scenario is the one this machinery exists for: a preempted SPOT
    /// VM. GCP stopped it, the disk survived with its driver; the
    /// preflight cannot know that (no ssh on a stopped VM), so the
    /// approved plan contains the install. After StartVm + WaitSsh, the
    /// re-probe removes it.
    ///
    /// A failure here would mean the repository's fifth dead scaffolding: an
    /// existing acquired-state flag, documented, unit-tested, and read by
    /// nobody in the path that bills 3.72 $/h.
    #[tokio::test]
    async fn a_preempted_vm_does_not_reinstall_what_its_disk_already_carries() {
        let full = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: VM_NAME.into(),
            cap_usd: 25.0,
        };
        let smi_ready = CmdOut {
            stdout: "| NVIDIA-SMI 580.126.09   Driver Version: 580.126.09  |".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let cc_off = || CmdOut {
            stdout: "CC status: OFF".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let mut exec = ScriptedExec::with(vec![
            // observe(): auth, project, image family, instance (TERMINATED).
            CmdOut { stdout: r#"[{"account":"a@b.c","status":"ACTIVE"}]"#.into(), stderr: String::new(), exit_code: 0 },
            CmdOut { stdout: "example-project-123456\n".into(), stderr: String::new(), exit_code: 0 },
            CmdOut { stdout: r#"[{"name":"img"}]"#.into(), stderr: String::new(), exit_code: 0 },
            CmdOut {
                stdout: r#"[{"name":"x0ne-cc","status":"TERMINATED","zone":"http://x/zones/us-central1-a"}]"#.into(),
                stderr: String::new(),
                exit_code: 0,
            },
            ok_out(),            // StartVm
            ok_out(),            // WaitSsh
            honest_quote_out(),  // AttestTdx — VERIFIED, or nothing below runs
            smi_ready,           // re-probe: the driver survived the preemption
            cc_off(),            // re-probe: the GPU says OFF
            fail_out("curl: (7) Failed to connect"), // re-probe: no vLLM
            fail_out("x0ne: not found"),             // re-probe: no x0ne
            // InstallGpuDriver is SKIPPED here — no response consumed.
            cc_off(),            // CheckGpuCc: reads OFF, refuses
            ok_out(),            // the stop that a failed run owes the operator
        ]);
        let mut runner = CcRunner::new(&mut exec, full.clone());
        let obs = runner.observe().await.unwrap();
        let plan = build_plan(&full, &obs).unwrap();
        assert!(
            plan.contains(&CcStep::InstallGpuDriver),
            "the approved plan does it all: {plan:?}"
        );

        let (vm, failed) = run_plan(
            &mut runner,
            &full,
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;

        let sent: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        assert!(
            !sent.iter().any(|a| a.contains("apt-get")),
            "the driver install must never be sent to a VM that already has it: {sent:?}"
        );
        assert!(
            !sent.iter().any(|a| a.contains("reboot")),
            "and it must not reboot a machine that needed nothing: {sent:?}"
        );
        // The run went on past the skipped step and refused on the GPU.
        let (step, why) = failed.expect("a GPU in the clear must fail the run");
        assert_eq!(step, CcStep::CheckGpuCc);
        assert!(
            why.contains("gpu exposed"),
            "the failure must name it: {why}"
        );
        assert_eq!(
            vm,
            VmEnd::Stopped,
            "a failed run never leaves the meter running"
        );
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// SECURITY (finding C1, review round 1): a REJECTED TDX verdict cuts the
    /// pipeline before the first GPU command.
    ///
    /// It is the property the loop must carry, and it did not carry it:
    /// `attest_tdx` stored the verdict and returned `Ok(())`, nobody
    /// reread it, and on `--machine full` a replayed nonce let the driver
    /// install run (≈ 37 min billed, plus a reboot) then the whole
    /// vLLM chain on an unproven enclave. The command count is
    /// the core of the test: an assertion on `failed` alone would also pass if
    /// the loop had done ten minutes of work before stopping.
    #[tokio::test]
    async fn a_rejected_tdx_verdict_stops_before_any_gpu_step() {
        let full = CcConfig {
            machine: CcMachine::FullH100,
            vm_name: VM_NAME.into(),
            cap_usd: 25.0,
        };
        // report_data left zeroed: it cannot match a fresh /dev/urandom nonce.
        let stale_quote = {
            use base64::Engine;
            let mut q = vec![0u8; 48 + 584 + 8];
            q[0] = 4;
            q[4..8].copy_from_slice(&0x81u32.to_le_bytes());
            CmdOut {
                stdout: base64::engine::general_purpose::STANDARD.encode(&q),
                stderr: String::new(),
                exit_code: 0,
            }
        };
        let plan = vec![
            CcStep::WaitSsh,
            CcStep::AttestTdx,
            CcStep::InstallGpuDriver,
            CcStep::CheckGpuCc,
            CcStep::AttestGpu,
            CcStep::Report,
        ];
        let mut exec = ScriptedExec::with(vec![
            ok_out(),    // WaitSsh
            stale_quote, // AttestTdx → Rejected
            ok_out(),    // the stop
        ]);
        let mut runner = CcRunner::new(&mut exec, full.clone());
        let (vm, failed) = run_plan(
            &mut runner,
            &full,
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;

        let step = failed
            .as_ref()
            .expect("a rejected quote must fail the run")
            .0
            .clone();
        // The report says REFUSED — the rejection, not the step failure it
        // caused — and the exit status agrees with it. Read before the
        // assertions on `exec`, which needs the runner's borrow released.
        let outcome = runner.outcome(failed);
        assert!(
            matches!(outcome, CcOutcome::Refused { .. }),
            "must read as refused"
        );
        assert!(exit_status_for(&outcome).is_err(), "and exit non-zero");

        assert_eq!(step, CcStep::AttestTdx);
        assert_eq!(
            exec.sent.len(),
            3,
            "ssh probe, quote fetch, stop — and NOTHING else: {:?}",
            exec.sent
        );
        let sent: Vec<String> = exec.sent.iter().flatten().cloned().collect();
        for forbidden in ["nvidia-smi", "apt-get", "conf-compute", "reboot"] {
            assert!(
                !sent.iter().any(|a| a.contains(forbidden)),
                "no GPU work may follow an unproven enclave, found {forbidden}: {sent:?}"
            );
        }
        assert_eq!(vm, VmEnd::Stopped, "and the meter is off");
    }

    /// SECURITY (review round 1 of task 5, I1+I2): the CPU proof made at
    /// step 3 is still a proof when step 12 fails, and the report
    /// prints it.
    ///
    /// This was not an edge case: until task 6 it was THE report that
    /// `--machine full` produced, the pipeline stopping at `InstallX0ne` for lack
    /// of an implementation. It then printed the GPU verdict and the four in-VM
    /// checks without ONE line of the TDX measurement — « the CPU enclave is proven » written
    /// in an error message with nothing behind it. Measured by the reviewer on
    /// a real run at ≈ 2.79 $.
    ///
    /// The step now exists (task 6), so the failure played here is the one
    /// it can really produce — a `scp` that breaks — and the property
    /// tested is unchanged: what was proven before stays proven after.
    ///
    /// The test reads `mr_td` by its VALUE (`ab…`, the one the fake VM
    /// sealed in its quote), not by its label: a « mr_td
    /// (not measured) » line would reassure as much as it would lie.
    #[tokio::test]
    async fn a_full_run_that_breaks_late_still_prints_the_cpu_measurements() {
        let full = cfg_full();
        let plan = vec![
            CcStep::WaitSsh,
            CcStep::AttestTdx,
            CcStep::AttestGpu,
            CcStep::InstallX0ne,
            CcStep::Report,
        ];
        let mut exec = ScriptedExec::with(vec![
            ok_out(),           // WaitSsh
            honest_quote_out(), // AttestTdx — the proof, made here
            CmdOut {
                stdout: "CC status: ON".into(),
                stderr: String::new(),
                exit_code: 0,
            },
            CmdOut {
                stdout: "580.126.09".into(),
                stderr: String::new(),
                exit_code: 0,
            },
            // InstallX0ne: nothing is installed, then the scp breaks.
            fail_out("x0ne: not found"),
            fail_out("scp: Connection closed"),
            ok_out(), // the stop a broken run owes the operator
        ]);
        let mut runner = CcRunner::new(&mut exec, full.clone());
        runner.set_x0ne_binaries(X0neBinaries {
            x0ne: "/nowhere/x0ne".into(),
            runtime: "/nowhere/x0ne-runtime".into(),
        });
        let (vm, failed) = run_plan(
            &mut runner,
            &full,
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(
            failed.as_ref().expect("the scp failed").0,
            CcStep::InstallX0ne
        );
        assert_eq!(vm, VmEnd::Stopped, "and the meter is stopped");

        let outcome = runner.outcome(failed);
        let r = render_report(&outcome, 2.79, full.machine, "stopped");
        assert!(r.contains("PIPELINE FAILED at InstallX0ne"), "{r}");
        // THE property: the measurements the operator paid for are there.
        assert!(
            r.contains(&"ab".repeat(48)),
            "the measured mr_td must be printed: {r}"
        );
        assert!(r.contains("  verifier   "), "{r}");
        assert!(r.contains("  nonce      "), "{r}");
        assert!(r.contains("  rtmr0      "), "{r}");
        assert!(
            r.contains("structural check only"),
            "and the limit of that proof with it: {r}"
        );
        // The GPU half too, and its own cap.
        assert!(r.contains("gpu        cc-mode ON"), "{r}");
        assert!(r.contains("NRAS"), "{r}");
        // But the word PROVEN stays with the verdict, which is not rendered here.
        assert!(
            !r.contains("PROVEN"),
            "a broken run does not carry the verdict: {r}"
        );
        assert!(
            exit_status_for(&outcome).is_err(),
            "and the exit status follows the failure"
        );
    }

    /// The counterpart: no phantom proof. A run that breaks BEFORE `AttestTdx`
    /// has measured nothing, and the report must carry none of the four
    /// labels — not even empty, not even « not measured ». A block of measurements
    /// printed blank is more dangerous than an absent block: it has the shape
    /// of a proof.
    #[tokio::test]
    async fn a_run_that_broke_before_attestation_prints_no_measurement_block() {
        let full = cfg_full();
        let plan = vec![CcStep::WaitSsh, CcStep::AttestTdx, CcStep::Report];
        // 27 SSH probes that fail, then the stop.
        let mut responses: Vec<CmdOut> = (0..SSH_RETRIES)
            .map(|_| fail_out("connection refused"))
            .collect();
        responses.push(ok_out());
        let mut exec = ScriptedExec::with(responses);
        let mut runner = CcRunner::new(&mut exec, full.clone());
        let (_vm, failed) = run_plan(
            &mut runner,
            &full,
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(
            failed.as_ref().expect("SSH never answered").0,
            CcStep::WaitSsh
        );

        let r = render_report(&runner.outcome(failed), 0.05, full.machine, "stopped");
        assert!(r.contains("PIPELINE FAILED at WaitSsh"), "{r}");
        for absent in [
            "verifier",
            "nonce",
            "mr_td",
            "rtmr0",
            "structural check only",
        ] {
            assert!(
                !r.contains(absent),
                "no measurement was taken: the report must not look like it has one ({absent}): {r}"
            );
        }
    }

    /// THE ledger: `exit 0` means « the enclave is proven », and on
    /// `--machine full` the GPU half is part of it.
    ///
    /// Minor M-f of task 3, made real in task 5: as long as `full`
    /// was refused, a `Proven{gpu:None}` on this machine was
    /// unconstructible. It no longer is. The scenario played here is the
    /// future regression, not today's path — a full plan from which the
    /// GPU steps have disappeared (which `build_plan` never produces: it
    /// pushes `AttestGpu` unconditionally). The loop runs through it without failure,
    /// so nothing upstream signals anything; it is `outcome()` that
    /// refuses to stamp a `Proven` without GPU proof, and the exit status
    /// follows.
    ///
    /// A failure here means that `x0ne cc --machine full --yes && echo proven`
    /// writes « proven » above a report saying « gpu exposed ».
    #[tokio::test]
    async fn a_full_run_with_no_gpu_step_left_exits_non_zero() {
        let full = cfg_full();
        // No `observe()` beforehand: without a preflight, `replan_after_ssh` returns the
        // approved plan as is and probes nothing.
        let plan = vec![CcStep::WaitSsh, CcStep::AttestTdx, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![ok_out(), honest_quote_out()]);
        let mut runner = CcRunner::new(&mut exec, full.clone());
        let (_vm, failed) = run_plan(
            &mut runner,
            &full,
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert!(
            failed.is_none(),
            "no step failed: that is the whole trap: {failed:?}"
        );

        let outcome = runner.outcome(failed);
        assert!(
            !matches!(outcome, CcOutcome::Proven { .. }),
            "a proven CPU enclave without a GPU measurement is not a successful full run"
        );
        assert!(
            exit_status_for(&outcome).is_err(),
            "and the shell must learn it"
        );
        // The counter-check, on the prover: same plan, same absence of GPU,
        // and there it is a success — the guard must not close the machine that
        // works.
        let mut exec = ScriptedExec::with(vec![ok_out(), honest_quote_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (_vm, failed) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::Running,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        let outcome = runner.outcome(failed);
        assert!(
            matches!(outcome, CcOutcome::Proven { .. }),
            "the prover has no GPU"
        );
        assert!(exit_status_for(&outcome).is_ok());
    }

    /// A step that fails mid-pipeline stops the VM: never leave the meter
    /// running on a broken run (owner decision 2026-08-13).
    #[tokio::test]
    async fn a_failed_step_stops_the_meter() {
        let plan = vec![CcStep::CreateVm, CcStep::AttestTdx, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![
            ok_out(),                 // create succeeds → meter on
            fail_out("no such file"), // attest fails
            ok_out(),                 // the stop
        ]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (vm, failed) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(failed.unwrap().0, CcStep::AttestTdx);
        assert_eq!(vm, VmEnd::Stopped);
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// The worst case: the run failed AND the stop failed. The report must
    /// say STILL RUNNING rather than inherit "stopped" — this is the one
    /// state where the operator believes billing ended and it has not.
    #[tokio::test]
    async fn a_stop_that_fails_is_reported_as_still_running() {
        let plan = vec![CcStep::CreateVm, CcStep::AttestTdx, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![
            ok_out(),
            fail_out("boom"),
            fail_out("instance is in a transitional state"), // the stop fails
        ]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (vm, _) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(vm, VmEnd::StopFailed);
        assert!(vm.label(&cfg()).contains("STILL RUNNING"));
    }

    /// A failed `create`/`start` is not a stopped VM: our timeout can fire
    /// while GCE goes on booting the instance, which then bills. The loop must
    /// ASK the cloud instead of guessing, and stop what it finds running. A
    /// failure here means a create that "failed" leaves a paying VM behind
    /// with the report claiming nothing was brought up.
    #[tokio::test]
    async fn a_failed_create_asks_the_cloud_instead_of_assuming_nothing_runs() {
        let plan = vec![CcStep::CreateVm, CcStep::Report];
        let running = CmdOut {
            stdout: r#"[{"name":"x0ne-cc","status":"RUNNING","zone":"z/us-central1-a"}]"#.into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let mut exec = ScriptedExec::with(vec![
            fail_out("timed out"), // create "fails"…
            running,               // …but the instance probe says it is up
            ok_out(),              // so it gets stopped
        ]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (vm, failed) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(failed.unwrap().0, CcStep::CreateVm);
        assert_eq!(
            vm,
            VmEnd::Stopped,
            "a VM found running must be stopped, not assumed absent"
        );
        assert!(
            exec.issued(&["instances", "list"]),
            "the cloud must be asked: {:?}",
            exec.sent
        );
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// Only when the follow-up probe ALSO fails do we say we do not know.
    #[tokio::test]
    async fn uncertain_is_reserved_for_a_question_that_went_unanswered() {
        let plan = vec![CcStep::CreateVm, CcStep::Report];
        let mut exec = ScriptedExec::with(vec![fail_out("timed out"), fail_out("API unreachable")]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let (vm, _) = run_plan(
            &mut runner,
            &cfg(),
            &plan,
            VmEnd::NeverUp,
            Instant::now(),
            &AtomicUsize::new(0),
        )
        .await;
        assert_eq!(vm, VmEnd::Uncertain);
    }

    /// `--yes` is for scripts, and a script nobody is watching must not leave
    /// a VM billing. It stops the meter without asking, and it never reaches
    /// the deletion branch.
    #[tokio::test]
    async fn yes_stops_the_vm_and_never_asks_anything() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let mut asked = false;
        let mut entered = false;
        let vm = finish_vm(
            &mut runner,
            &cfg(),
            VmEnd::Running,
            true,
            // Even offered — `--yes` must not take it: there is nobody at the
            // terminal to hold the session it would open.
            true,
            &mut ExitUi {
                ask: &mut |_, _| {
                    asked = true;
                    Ok(ExitChoice::Keep)
                },
                confirm: &mut |_| Ok(true),
                enter: &mut |_| {
                    entered = true;
                    Ok(())
                },
            },
        )
        .await;
        assert_eq!(vm, VmEnd::Stopped);
        assert!(!asked, "--yes must not consult a human that is not there");
        assert!(!entered, "--yes must never open a session");
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// Destruction is gated by its OWN confirmation, and refusing it leaves
    /// the VM alone rather than falling through to some milder action. A
    /// failure here means a disk is deleted after the operator said no.
    #[tokio::test]
    async fn a_refused_deletion_deletes_nothing() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let vm = finish_vm(
            &mut runner,
            &cfg(),
            VmEnd::Running,
            false,
            false,
            &mut ExitUi {
                ask: &mut |_, _| Ok(ExitChoice::Destroy),
                confirm: &mut |_| Ok(false), // the human says no
                enter: &mut |_| panic!("no session is part of a deletion"),
            },
        )
        .await;
        assert_eq!(vm, VmEnd::Running);
        assert!(
            exec.sent.is_empty(),
            "nothing may be issued after a refused deletion"
        );
    }

    /// …and approving it does delete, so the test above is not passing for
    /// want of a working path.
    #[tokio::test]
    async fn an_approved_deletion_deletes() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let vm = finish_vm(
            &mut runner,
            &cfg(),
            VmEnd::Running,
            false,
            false,
            &mut ExitUi {
                ask: &mut |_, _| Ok(ExitChoice::Destroy),
                confirm: &mut |_| Ok(true),
                enter: &mut |_| panic!("no session is part of a deletion"),
            },
        )
        .await;
        assert_eq!(vm, VmEnd::NeverUp);
        assert!(
            exec.issued(&["instances", "delete", "--quiet"]),
            "sent: {:?}",
            exec.sent
        );
    }

    /// An error reading the answer is not a licence to keep billing, and
    /// `Keep` is the only answer that leaves the meter on.
    #[tokio::test]
    async fn an_unreadable_answer_stops_and_keep_is_the_only_one_that_does_not() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        let vm = finish_vm(
            &mut runner,
            &cfg(),
            VmEnd::Running,
            false,
            false,
            &mut ExitUi {
                ask: &mut |_, _| Err(anyhow::anyhow!("stdin died")),
                confirm: &mut |_| Ok(false),
                enter: &mut |_| panic!("an unreadable answer opens nothing"),
            },
        )
        .await;
        assert_eq!(vm, VmEnd::Stopped);
        assert!(exec.issued(&["instances", "stop"]));

        let mut exec = ScriptedExec::default();
        let mut runner = CcRunner::new(&mut exec, cfg());
        let vm = finish_vm(
            &mut runner,
            &cfg(),
            VmEnd::Running,
            false,
            false,
            &mut ExitUi {
                ask: &mut |_, _| Ok(ExitChoice::Keep),
                confirm: &mut |_| Ok(false),
                enter: &mut |_| panic!("keep opens nothing"),
            },
        )
        .await;
        assert_eq!(vm, VmEnd::Running);
        assert!(exec.sent.is_empty(), "keep must issue nothing");
    }

    // ── entering the enclave (task 6, 2026-08-14) ───────────────────────

    /// SECURITY: the in-enclave prompter is gated on the proof exactly like the
    /// badge and the report — a Refused outcome must never offer `enter`.
    ///
    /// Three layers, and three are needed: the derivation (`offers_enclave_
    /// entry`) that decides, the parser that refuses the key, and `finish_vm` which
    /// does not trust the parser. A failure here means that an operator
    /// types `e` under a report saying ATTESTATION REFUSED and ends up in
    /// a machine that nothing vouched for — the chat path without
    /// a green verdict that the prover spec forbids.
    #[tokio::test]
    async fn a_refused_outcome_never_offers_the_enclave_entry() {
        // 1. The derivation, on the three outcomes that are not a proof.
        for outcome in [
            CcOutcome::Refused {
                reason: "nonce mismatch".into(),
            },
            CcOutcome::Failed {
                step: CcStep::AttestTdx,
                error: "boom".into(),
                proof: None,
                gpu: None,
                encryption: None,
            },
        ] {
            assert!(
                !offers_enclave_entry(&outcome, CcMachine::FullH100),
                "no entry without proof"
            );
        }

        // 2. The parser: `e` without the proof does not fall onto a softer
        // reading of the intent, it falls onto the fail-safe.
        assert_eq!(parse_exit_choice("e", false), ExitChoice::Stop);
        assert_eq!(parse_exit_choice("enter", false), ExitChoice::Stop);
        assert_eq!(
            parse_exit_choice("e", true),
            ExitChoice::Enter,
            "and it works when offered"
        );

        // 3. `finish_vm` does not trust the prompter: an `Enter` that
        // arrived anyway is refused, and the meter is cut.
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg_full());
        let mut entered = false;
        let mut answers = VecDeque::from(vec![ExitChoice::Enter]);
        let vm = finish_vm(
            &mut runner,
            &cfg_full(),
            VmEnd::Running,
            false,
            false, // the proof is not there
            &mut ExitUi {
                // A single answer in the queue: if the guard gives way, the menu
                // asks again and the test fails HERE, cleanly. Without it it
                // loops — an `Enter` always accepted and always replayed is
                // an infinite loop, and a hanging test teaches nothing to
                // whoever broke it (measured: mutant M2 killed the suite by
                // OOM before this queue was bounded).
                ask: &mut |_, _| {
                    answers.pop_front().ok_or_else(|| {
                        anyhow::anyhow!("the menu asked again after a refused Enter")
                    })
                },
                confirm: &mut |_| Ok(true),
                enter: &mut |_| {
                    entered = true;
                    Ok(())
                },
            },
        )
        .await;
        assert!(!entered, "a session in an unproven enclave is never opened");
        assert_eq!(vm, VmEnd::Stopped, "and the fail-safe stays `stop`");
    }

    /// The counter-check, and it is not symmetric: a PROVEN run on the
    /// prover machine does not offer entry either.
    ///
    /// The verdict there is green and honest, but the machine has no GPU, no vLLM, no
    /// x0ne — `InstallX0ne` is never in its plan. An `e` there would open a
    /// shell on a bare Ubuntu while calling it « the enclave ». A failure here
    /// means that the « machine » half of the guard was removed as
    /// redundant; it is not.
    #[tokio::test]
    async fn a_proven_prover_is_still_not_an_enclave_to_enter() {
        // A REAL proof, produced by the pipeline: `ProofBundle` has only one
        // door (`from_verdict`), and a test that built one by hand
        // would test an object production never builds.
        let mut exec = ScriptedExec::with(vec![ok_out(), honest_quote_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg());
        for step in [CcStep::WaitSsh, CcStep::AttestTdx] {
            runner.run_step(&step).await.unwrap();
        }
        let outcome = runner.outcome(None);
        assert!(
            matches!(outcome, CcOutcome::Proven { .. }),
            "the prover verdict is green"
        );
        assert!(
            !offers_enclave_entry(&outcome, CcMachine::Prover),
            "and it opens nothing"
        );
        // The same outcome on the full machine, however, opens: what separates
        // the two lines is the MACHINE, so it is indeed that half of the
        // guard that the line above exercises.
        assert!(offers_enclave_entry(&outcome, CcMachine::FullH100));
    }

    /// Coming back from the enclave does not end the command: it falls back to the
    /// menu, and the menu says again that the VM is running and what it costs.
    ///
    /// This is the property that protects the wallet: someone who has just
    /// chatted for ten minutes with a model has stopped thinking about the meter.
    /// A failure means either that `enter` leaves the menu (VM left running
    /// without being offered a shutdown), or that it loops without ever
    /// asking again.
    #[tokio::test]
    async fn coming_back_from_the_enclave_returns_to_the_menu_and_stops_the_meter() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg_full());
        let mut answers = VecDeque::from(vec![ExitChoice::Enter, ExitChoice::Stop]);
        let mut entered = 0usize;
        let vm = finish_vm(
            &mut runner,
            &cfg_full(),
            VmEnd::Running,
            false,
            true,
            &mut ExitUi {
                ask: &mut |_, offer| {
                    assert!(offer, "the menu must know that entry is offered");
                    Ok(answers.pop_front().expect("the menu asked once too often"))
                },
                confirm: &mut |_| Ok(false),
                enter: &mut |_| {
                    entered += 1;
                    Ok(())
                },
            },
        )
        .await;
        assert_eq!(entered, 1, "one session, then the menu");
        assert!(
            answers.is_empty(),
            "the menu must have asked again after the session"
        );
        assert_eq!(vm, VmEnd::Stopped);
        assert!(exec.issued(&["instances", "stop"]), "sent: {:?}", exec.sent);
    }

    /// A session that breaks does not exit the command with a VM
    /// running: it falls back to the menu, where the operator can cut the meter.
    #[tokio::test]
    async fn a_session_that_fails_to_open_still_lands_on_the_exit_menu() {
        let mut exec = ScriptedExec::with(vec![ok_out()]);
        let mut runner = CcRunner::new(&mut exec, cfg_full());
        let mut answers = VecDeque::from(vec![ExitChoice::Enter, ExitChoice::Stop]);
        let vm = finish_vm(
            &mut runner,
            &cfg_full(),
            VmEnd::Running,
            false,
            true,
            &mut ExitUi {
                ask: &mut |_, _| Ok(answers.pop_front().expect("once too often")),
                confirm: &mut |_| Ok(false),
                enter: &mut |_| anyhow::bail!("gcloud: command not found"),
            },
        )
        .await;
        assert_eq!(
            vm,
            VmEnd::Stopped,
            "a failed session does not leave the meter running"
        );
    }

    /// The command that puts the operator in the enclave, pinned as it
    /// goes out: `-t` (without it, x0ne starts without a tty and exits at the first
    /// read), `./x0ne` (the home is not in the PATH of a non-interactive
    /// shell), and the zone of the full machine.
    #[test]
    fn the_enclave_session_forces_a_tty_and_runs_the_x0ne_we_installed() {
        let argv = enclave_ssh_argv(&cfg_full());
        assert_eq!(
            argv[0..3],
            [
                "compute".to_string(),
                "ssh".to_string(),
                VM_NAME.to_string()
            ]
        );
        assert!(argv.contains(&"-t".to_string()), "{argv:?}");
        assert!(argv.contains(&"./x0ne".to_string()), "{argv:?}");
        assert!(
            argv.contains(&format!(
                "--zone={}",
                cfg_full().machine.vm_spec(VM_NAME).zone
            )),
            "{argv:?}"
        );
        // The `--` separates gcloud options from ssh ones: without it, `-t`
        // is read by gcloud, which does not know it.
        let dashdash = argv.iter().position(|a| a == "--").expect("{argv:?}");
        let t = argv.iter().position(|a| a == "-t").expect("{argv:?}");
        assert!(dashdash < t, "-t belongs to ssh, not to gcloud: {argv:?}");

        // The displayed line is derived from the executed argv, so it cannot
        // describe a command other than the one that goes out.
        let shown = enclave_ssh_display(&argv);
        assert!(
            shown.starts_with("gcloud "),
            "the binary is shown too: {shown}"
        );
        for a in &argv {
            assert!(
                shown.contains(a.as_str()),
                "argument {a} is not shown: {shown}"
            );
        }
    }

    /// The report travels BEFORE the session, and its failure does not cancel it.
    ///
    /// `enter` opens `gcloud compute ssh … -- -t ./x0ne`: ANOTHER process,
    /// on the attested machine, which has no access to the disk of the laptop where
    /// `x0ne cc` wrote the report. Without this copy, the enclave badge of a
    /// session in a proven enclave reads « confidential GPU disabled » — exactly
    /// what it read when the indicator had no producer.
    ///
    /// A failure of this test means either that the report no longer goes out (red
    /// badge in a green enclave, and the operator learns to ignore the
    /// badge), or that `scp` goes out before the `mkdir` that creates its directory.
    #[test]
    fn the_report_travels_into_the_enclave_before_the_session_opens() {
        let cfg = cfg_full();
        let zone = format!("--zone={}", cfg.machine.vm_spec(VM_NAME).zone);
        let local = std::path::Path::new("/home/someone/.x0ne/cc/last-report.json");
        let [mkdir, scp] = enclave_report_upload_argv(&cfg, local);

        // The directory first: `scp` does not create it.
        assert_eq!(
            mkdir[0..3],
            [
                "compute".to_string(),
                "ssh".to_string(),
                VM_NAME.to_string()
            ]
        );
        let dashdash = mkdir.iter().position(|a| a == "--").expect("{mkdir:?}");
        assert_eq!(
            &mkdir[dashdash + 1..],
            &["mkdir", "-p", ".x0ne/cc"],
            "{mkdir:?}"
        );
        assert!(mkdir.contains(&zone), "{mkdir:?}");

        // Then the copy, to the path the VM's x0ne reads.
        assert_eq!(scp[0..2], ["compute".to_string(), "scp".to_string()]);
        assert!(
            scp.contains(&local.to_string_lossy().into_owned()),
            "{scp:?}"
        );
        assert!(
            scp.contains(&format!("{VM_NAME}:{ENCLAVE_REPORT_REMOTE}")),
            "{scp:?}"
        );
        assert!(scp.contains(&zone), "{scp:?}");

        // And BOTH go out, in this order, when nothing fails. The argv
        // alone does not prove it: it is `upload_report` that chains them.
        let mut sent: Vec<Vec<String>> = Vec::new();
        upload_report(&cfg, local, &mut |argv| {
            sent.push(argv.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sent,
            vec![mkdir, scp],
            "the mkdir precedes the scp: {sent:?}"
        );
    }

    /// A report that does not arrive costs the badge, never the session.
    ///
    /// The enclave was proven, the report is on screen, and the operator
    /// asked to enter. A failure here means that a failed `scp` trades a
    /// true report for no session at all.
    #[test]
    fn a_report_that_cannot_travel_does_not_cancel_the_session() {
        let cfg = cfg_full();
        let local = std::path::Path::new("/nowhere/last-report.json");
        let mut calls = 0usize;
        let err = upload_report(&cfg, local, &mut |_argv| {
            calls += 1;
            anyhow::bail!("gcloud: Permission denied")
        });
        // It returns an error — which `enter_enclave` prints and absorbs — and
        // stops at the first failure rather than going on to a `scp` whose
        // directory does not exist.
        assert!(
            err.is_err(),
            "the failure must propagate, not be swallowed here"
        );
        assert_eq!(
            calls, 1,
            "the second call follows the first, it does not overtake it"
        );

        // And the function that opens the session does not consult this result:
        // the only call to `carry_report_into_enclave` in `enter_enclave`
        // is inside an `if let Err`, never a `?`.
        let src = include_str!("cc_cmd.rs");
        let body = &src[src.find("fn enter_enclave(").expect("enter_enclave")..];
        let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
        assert!(
            body.contains("if let Err(e) = carry_report_into_enclave"),
            "a `?` here would turn a failed copy into a refusal to enter: {body}"
        );
    }

    /// The command is SHOWN before being launched, and it is an ORDER, not a
    /// coincidence: the operator must have read what is going to run before the
    /// terminal goes off into the VM.
    ///
    /// A failure means that `x0ne cc` opens a session in a remote machine
    /// without saying which one — the only action of this command that
    /// hands the terminal to something other than itself.
    #[test]
    fn the_enclave_command_is_shown_before_the_session_opens() {
        /// A buffer shared by the test and the writer, so that the content
        /// is readable WHILE `show_and_run_enclave` holds the writer.
        /// A plain `Vec<u8>` would be mutably borrowed for the whole duration
        /// of the call, and the moment we care about is precisely inside it.
        #[derive(Clone)]
        struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let argv = enclave_ssh_argv(&cfg_full());
        let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
        let seen = buf.clone();
        let mut at_launch = String::new();
        let mut ran = 0usize;
        show_and_run_enclave(&mut buf.clone(), &argv, &mut |got| {
            // What the screen carries AT THE MOMENT of launch. Read here and not
            // after: a display made on RETURN from the session would pass an
            // assertion placed at the end, and it would have arrived too late to
            // be of any use.
            at_launch = String::from_utf8_lossy(&seen.0.borrow()).into_owned();
            assert_eq!(got, argv.as_slice(), "the argv launched is the one shown");
            ran += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(ran, 1, "the session opens once");
        assert!(
            at_launch.contains("gcloud"),
            "nothing was displayed at launch: {at_launch:?}"
        );
        for a in &argv {
            assert!(
                at_launch.contains(a.as_str()),
                "argument {a} was missing from the screen: {at_launch:?}"
            );
        }
    }

    /// The binaries to copy are resolved from the current executable, and their
    /// absence is a REFUSAL that names the command that fixes it.
    ///
    /// This refusal comes before the budget gate in `run_cc`: a pipeline that
    /// creates an H100, attests it, installs a driver and serves a model before
    /// discovering that there is no `x0ne-runtime` next to it has burned half
    /// an hour of SPOT to report a local build problem.
    #[test]
    fn the_binaries_to_copy_are_the_running_one_and_its_sibling_or_it_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("x0ne");
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(&exe, "binary").unwrap();

        let err = resolve_x0ne_binaries(&exe, elsewhere.path(), None).unwrap_err();
        assert!(
            err.contains("x0ne-runtime"),
            "say which one is missing: {err}"
        );
        assert!(
            err.contains("cargo build --release"),
            "name the lever: {err}"
        );

        std::fs::write(dir.path().join("x0ne-runtime"), "binary").unwrap();
        let got = resolve_x0ne_binaries(&exe, elsewhere.path(), None).expect("both are present");
        assert_eq!(
            got.bins.x0ne, exe,
            "the RUNNING one is copied, not a namesake found elsewhere"
        );
        assert_eq!(got.bins.runtime, dir.path().join("x0ne-runtime"));
        assert_eq!(
            got.origin,
            BinariesOrigin::ThisHost,
            "and their origin is known"
        );
    }

    /// Writes two fake binaries into `<root>/target/enclave/`, as
    /// `make enclave-binaries` does.
    fn enclave_pair(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("target").join("enclave");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x0ne"), "cross-built").unwrap();
        std::fs::write(dir.join("x0ne-runtime"), "cross-built").unwrap();
        dir
    }

    /// `target/enclave/` wins over the current executable, and it is looked for
    /// from the WORKING DIRECTORY, including from a subdirectory.
    ///
    /// This is the fix from run 8 (2026-08-15): the installed x0ne runs
    /// from `~/.cargo/bin`, where there is no `target/`, and the host's
    /// binaries (Debian 13, glibc 2.41) do not load in an Ubuntu
    /// 22.04 VM (glibc 2.35). A failure here means that `make enclave-binaries`
    /// produces binaries the pipeline does not send — the very definition
    /// of this repository's dead scaffolding.
    #[test]
    fn the_enclave_build_wins_over_the_running_binary_and_is_found_from_the_cwd() {
        let repo = tempfile::tempdir().unwrap();
        let enclave = enclave_pair(repo.path());
        // The exe is elsewhere, with its sibling: the fallback is available and
        // must still lose.
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("x0ne");
        std::fs::write(&exe, "host").unwrap();
        std::fs::write(bin.path().join("x0ne-runtime"), "host").unwrap();

        let sub = repo.path().join("x0ne-code").join("src");
        std::fs::create_dir_all(&sub).unwrap();
        for cwd in [repo.path(), sub.as_path()] {
            let got = resolve_x0ne_binaries(&exe, cwd, None).expect("the enclave build is present");
            assert_eq!(
                got.origin,
                BinariesOrigin::BuiltForTheEnclave,
                "from {}",
                cwd.display()
            );
            assert_eq!(got.bins.x0ne, enclave.join("x0ne"));
            assert_eq!(got.bins.runtime, enclave.join("x0ne-runtime"));
        }

        // A half-built `target/enclave/` does not count: it falls back
        // to the host, and the plan line will say so.
        std::fs::remove_file(enclave.join("x0ne-runtime")).unwrap();
        let got = resolve_x0ne_binaries(&exe, repo.path(), None).expect("fallback");
        assert_eq!(got.origin, BinariesOrigin::ThisHost);
        assert_eq!(got.bins.x0ne, exe);
    }

    /// `X0NE_ENCLAVE_BINARIES` designates, or refuses. It never silently falls
    /// back to binaries other than the ones the operator named.
    ///
    /// A failure here means that a variable pointing at an empty directory
    /// gets the host's x0ne uploaded into an attested enclave while
    /// the operator believes he is sending his cross-compilation there.
    #[test]
    fn an_explicit_override_designates_or_refuses_but_never_falls_back() {
        let bin = tempfile::tempdir().unwrap();
        let exe = bin.path().join("x0ne");
        std::fs::write(&exe, "host").unwrap();
        std::fs::write(bin.path().join("x0ne-runtime"), "host").unwrap();
        let repo = tempfile::tempdir().unwrap();
        enclave_pair(repo.path());

        let empty = tempfile::tempdir().unwrap();
        let err = resolve_x0ne_binaries(&exe, repo.path(), Some(empty.path())).unwrap_err();
        assert!(
            err.contains("X0NE_ENCLAVE_BINARIES"),
            "name the variable: {err}"
        );
        assert!(
            err.contains("make enclave-binaries"),
            "name the lever: {err}"
        );

        let chosen = tempfile::tempdir().unwrap();
        std::fs::write(chosen.path().join("x0ne"), "chosen").unwrap();
        std::fs::write(chosen.path().join("x0ne-runtime"), "chosen").unwrap();
        let got =
            resolve_x0ne_binaries(&exe, repo.path(), Some(chosen.path())).expect("designated");
        assert_eq!(
            got.bins.x0ne,
            chosen.path().join("x0ne"),
            "the override beats target/enclave"
        );
        assert_eq!(got.origin, BinariesOrigin::BuiltForTheEnclave);
    }

    /// The recipe that builds the enclave binaries exists, it writes where
    /// `resolve_x0ne_binaries` looks, and it carries its own check.
    ///
    /// Deliberate exception to « we do not read the Makefile from tests »
    /// (`terminal.rs:2766`): here the two halves are not a duplicate that gets
    /// resynchronised by hand, it is ONE path cut in two — make produces
    /// `target/enclave/`, `x0ne cc` goes to fetch it there — and this repository's
    /// dead scaffolding is born exactly from that kind of half that moves without the other.
    /// The `objdump` check is the target's fail-closed: without it,
    /// a newer base image silently gives back binaries that the
    /// enclave refuses, and run 8 gets paid for again. A failure here means one of
    /// these two regressions.
    #[test]
    fn the_enclave_recipe_writes_where_the_search_looks_and_checks_its_own_output() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let makefile =
            std::fs::read_to_string(root.join("Makefile")).expect("the repository Makefile");

        assert!(
            makefile.contains("\nenclave-binaries:"),
            "the target must exist"
        );
        assert!(
            makefile.contains("ENCLAVE_OUT       := target/enclave\n"),
            "the output must be the one `resolve_x0ne_binaries` looks for"
        );
        assert!(
            makefile.contains("ENCLAVE_MAX_GLIBC ?= 2.35"),
            "the ceiling is that of the jammy image the pipeline boots"
        );
        // The measurement and its fail-closed are looked for in the BODY of the
        // recipe, not in the file: the first version of this test read
        // the whole Makefile and stayed green after the check was
        // deleted — the comment above the target says « objdump »,
        // and the `exit 1` of the podman check is elsewhere in the same
        // recipe. An assertion satisfied by the documentation of the thing
        // it checks checks nothing (measured, mutant M3).
        let recipe = makefile
            .split("\nenclave-binaries:")
            .nth(1)
            .expect("the recipe body");
        let recipe = recipe.split("\n\n").next().unwrap();
        let after_objdump = recipe
            .split("objdump -T")
            .nth(1)
            .expect("the recipe must measure the produced glibc");
        assert!(
            after_objdump.contains("exit 1"),
            "a glibc that is too recent must STOP the recipe, not annotate it: {after_objdump}"
        );
        // And it stays out of the install path: ~10 min of container
        // are paid only when targeting the enclave.
        let install = makefile
            .lines()
            .find(|l| l.starts_with("install:"))
            .expect("the install target");
        assert!(
            !install.contains("enclave"),
            "make install must not depend on the container build"
        );
    }

    /// The plan line names BOTH files that go out, and states the glibc
    /// risk when they come from the host.
    ///
    /// Repository invariant 5, applied to the run 8 failure: « THIS x0ne » did not
    /// make it possible to see that a `make enclave-binaries` had not been
    /// picked up. A failure here means that an operator approves an upload
    /// without being able to know which file goes out nor whether it will load there.
    #[test]
    fn the_install_x0ne_line_names_the_files_that_go_and_the_glibc_risk() {
        let host = BinariesChoice {
            bins: X0neBinaries {
                x0ne: "/home/op/.cargo/bin/x0ne".into(),
                runtime: "/home/op/.cargo/bin/x0ne-runtime".into(),
            },
            origin: BinariesOrigin::ThisHost,
        };
        let rendered = render_plan(&cfg_full(), &[CcStep::InstallX0ne], Some(&host));
        assert!(
            rendered.contains("/home/op/.cargo/bin/x0ne-runtime"),
            "{rendered}"
        );
        assert!(
            rendered.contains("GLIBC RISK"),
            "the risk must be readable: {rendered}"
        );
        assert!(
            rendered.contains("make enclave-binaries"),
            "and the lever: {rendered}"
        );
        assert!(
            rendered.contains("X0NE_ENCLAVE_BINARIES"),
            "and the override: {rendered}"
        );

        let cross = BinariesChoice {
            bins: X0neBinaries {
                x0ne: "/home/op/x0ne/target/enclave/x0ne".into(),
                runtime: "/home/op/x0ne/target/enclave/x0ne-runtime".into(),
            },
            origin: BinariesOrigin::BuiltForTheEnclave,
        };
        let rendered = render_plan(&cfg_full(), &[CcStep::InstallX0ne], Some(&cross));
        assert!(
            rendered.contains("target/enclave/x0ne-runtime"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("GLIBC RISK"),
            "no alarm on the good case: {rendered}"
        );
    }

    /// Nothing is offered, and nothing is issued, when the meter is already
    /// off — a stopped or never-created VM has no exit choice to make.
    #[tokio::test]
    async fn a_vm_that_is_not_billing_gets_no_exit_menu() {
        for state in [
            VmEnd::NeverUp,
            VmEnd::Stopped,
            VmEnd::StopFailed,
            VmEnd::Uncertain,
        ] {
            let mut exec = ScriptedExec::default();
            let mut runner = CcRunner::new(&mut exec, cfg());
            let vm = finish_vm(
                &mut runner,
                &cfg(),
                state,
                false,
                true,
                &mut ExitUi {
                    ask: &mut |_, _| panic!("must not ask about {state:?}"),
                    confirm: &mut |_| Ok(true),
                    enter: &mut |_| panic!("must not enter a VM that is not running"),
                },
            )
            .await;
            assert_eq!(vm, state);
            assert!(exec.sent.is_empty());
        }
    }
}
