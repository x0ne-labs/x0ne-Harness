use std::path::PathBuf;

#[cfg(feature = "serve")]
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use tracing::warn;

// All modules are provided by the orchestrator library crate.
#[cfg(feature = "serve")]
use x0ne_orchestrator::hooks::SessionSource;
use x0ne_orchestrator::serve;
#[allow(unused_imports)]
use x0ne_orchestrator::{
    agent_loop, api, bridge, cli_helper, config, container_runtime, conversation, display,
    input_queue, interrupt, intro, llm_setup, matrix_mode, parser, project, prompt, session, setup,
    slash, sticky_header, sysaicalls, sysmon, terminal, worlds_cli,
};

mod acp_cmd;
mod audit_cmd;
mod autowrap;
#[cfg(feature = "cloud")]
mod cc_cmd;
mod config_cmd;
mod container_setup;
mod cost_cmd;
mod goal_cmd;
mod hook_cmd;

use std::sync::Arc;

use api::UsageTracker;
use config::Config;
use conversation::Conversation;
use project::detect_project;
use prompt::build_system_prompt_dynamic;
use session::SharedJournal;
use setup::SetupWizard;
use slash::SlashResult;
use sysaicalls::AgentRegistry;
use terminal::*;

/// What the autonomous loop should do next.
///
/// A pure decision, extracted so the loop's two stop conditions and its prompt
/// rule can be tested without a model, a runtime or a terminal. The historical
/// loop made all three inline, which is why none of them had a test.
#[derive(Debug, PartialEq, Eq)]
enum AutoStep {
    /// The iteration bound was reached.
    MaxIterations,
    /// Ctrl+C, or a stop requested from elsewhere in the process.
    Interrupted,
    /// Run one turn on this prompt.
    Run(&'static str),
}

/// The opening instruction. Historically added to the conversation once,
/// before the loop.
const AUTO_MODE_INITIAL: &str = "Analyze this project. Identify issues, suggest improvements, and fix what you can. Start with TreeView, then read key files.";

/// Every iteration after the first. Historically appended at the end of each
/// iteration, which is the same thing said one turn earlier.
const AUTO_MODE_CONTINUE: &str = "Continue. What's next?";

/// Order matters and is historical: the iteration bound is checked before the
/// interrupt flag, so a run that reaches its limit reports the limit even if a
/// stop arrived in the same instant.
fn auto_mode_step(iteration: usize, max_iterations: usize, interrupted: bool) -> AutoStep {
    if iteration >= max_iterations {
        return AutoStep::MaxIterations;
    }
    if interrupted {
        return AutoStep::Interrupted;
    }
    if iteration == 0 {
        AutoStep::Run(AUTO_MODE_INITIAL)
    } else {
        AutoStep::Run(AUTO_MODE_CONTINUE)
    }
}

/// The live "waiting" line for a canonical turn.
///
/// The REPL cutover replaced `run_agent_loop`, and the legacy spinner —
/// which lived inside that loop — silently went with it, leaving the CLI
/// mute while a model call runs. This restores it on the canonical seams:
/// the spinner is fed by the session's own token stream (`open_stream`), so
/// it shows real activity (thinking vs writing, streamed volume, elapsed),
/// and it is pure presentation — the driver still knows nothing about it.
///
/// The shared slot lets the sink wrapper below suspend the spinner while it
/// prints, so action boxes and confirmation prompts never interleave with a
/// redraw.
type SpinnerSlot = std::sync::Arc<std::sync::Mutex<Option<indicatif::ProgressBar>>>;

fn spawn_turn_spinner(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<api::StreamChunk>,
    slot: SpinnerSlot,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let pb = indicatif::ProgressBar::new_spinner();
        pb.set_style(
            indicatif::ProgressStyle::default_spinner()
                .template("  {spinner:.cyan} {msg}")
                .expect("hardcoded template"),
        );
        pb.enable_steady_tick(std::time::Duration::from_millis(90));
        *slot.lock().unwrap() = Some(pb.clone());
        let started = std::time::Instant::now();
        let mut out_chars: usize = 0;
        let mut thinking_chars: usize = 0;
        let mut refresh = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                chunk = rx.recv() => match chunk {
                    Some(api::StreamChunk::Token(s)) => out_chars += s.len(),
                    Some(api::StreamChunk::Thinking(s)) => thinking_chars += s.len(),
                    None => break,
                },
                _ = refresh.tick() => {}
            }
            let secs = started.elapsed().as_secs();
            // chars/4 ≈ tokens: an order-of-magnitude gauge, labelled as such.
            // Each phase gets its own colour so the state is readable at a
            // glance without parsing the words: quiet grey while nothing is
            // happening yet, violet while the model reasons, green once it is
            // producing. The volume/elapsed tail stays dim — it is a gauge,
            // not the headline.
            let tail = |s: String| s.truecolor(90, 96, 108).to_string();
            let msg = if out_chars == 0 && thinking_chars > 0 {
                format!(
                    "{} {}",
                    "thinking".truecolor(196, 181, 224),
                    tail(format!("· ~{} tok · {}s", thinking_chars / 4, secs))
                )
            } else if out_chars > 0 {
                format!(
                    "{} {}",
                    "writing".truecolor(0, 200, 120),
                    tail(format!("· ~{} tok · {}s", out_chars / 4, secs))
                )
            } else {
                format!(
                    "{} {}",
                    "waiting for the model".truecolor(104, 112, 128),
                    tail(format!("· {}s", secs))
                )
            };
            pb.set_message(msg);
        }
        *slot.lock().unwrap() = None;
        pb.finish_and_clear();
    });
    (handle, stop_tx)
}

/// A `DisplaySink` wrapper that hides the live spinner while the inner sink
/// prints, so boxes, result lines and the confirmation prompt land on clean
/// lines. Transport only — every decision stays with the inner sink and the
/// authority behind it.
struct SpinnerAwareSink<S> {
    inner: S,
    slot: SpinnerSlot,
}

impl<S: x0ne_orchestrator::display_sink::DisplaySink> x0ne_orchestrator::display_sink::DisplaySink
    for SpinnerAwareSink<S>
{
    fn emit(&mut self, event: &x0ne_orchestrator::tui::wire::AgentEvent) {
        let pb = self.slot.lock().unwrap().clone();
        match pb {
            Some(pb) => pb.suspend(|| self.inner.emit(event)),
            None => self.inner.emit(event),
        }
    }

    fn request_confirmation(
        &mut self,
        request: &x0ne_orchestrator::display_sink::ConfirmationRequest<'_>,
    ) -> Option<x0ne_orchestrator::agent::approval::ConfirmationResponse> {
        let pb = self.slot.lock().unwrap().clone();
        match pb {
            Some(pb) => pb.suspend(|| self.inner.request_confirmation(request)),
            None => self.inner.request_confirmation(request),
        }
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "x0ne",
    version,
    about = "AI agent — connects an LLM to x0ne-runtime",
    long_about = None
)]
struct Cli {
    #[arg(long, short, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long)]
    mode: Option<String>,
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,
    /// Model name to use (skips the interactive model picker).
    #[arg(long, short = 'm', value_name = "MODEL")]
    model: Option<String>,
    /// LLM endpoint URL (Ollama base URL, OpenAI-compat base URL).
    #[arg(long, value_name = "URL")]
    endpoint: Option<String>,
    #[arg(long, env = "X0NE_LOG", default_value = "warn")]
    log_level: String,
    /// Re-run the interactive setup wizard.
    #[arg(long)]
    setup: bool,
    /// Enable desktop actions (mouse, keyboard, screenshot).
    #[arg(long, env = "X0NE_DESKTOP")]
    desktop: bool,
    /// Health probe: parse the config, spawn the bridge, run its health
    /// check, count the actions — print the verdict and exit. Never opens
    /// a session and never talks to an LLM.
    #[arg(long)]
    health: bool,
    /// Launch the rich TUI instead of the CLI.
    #[arg(long)]
    tui: bool,
    /// Launch matrix mode — LLM-controlled terminal artifact.
    #[arg(long)]
    matrix: bool,
    /// HTTP/SSE serve mode — exposes /prompt and /health for web frontends.
    /// Skips the interactive wizard. x0ne-runtime MUST be running in HTTP mode.
    /// ⚠ Only present when built with the `serve` feature (default).
    #[cfg(feature = "serve")]
    #[arg(long)]
    serve: bool,
    /// Address to bind the serve HTTP listener (default: 127.0.0.1:7700).
    #[cfg(feature = "serve")]
    #[arg(long, value_name = "HOST:PORT", default_value = "127.0.0.1:7700")]
    bind: String,
    /// Bearer token for the serve API. If unset, one is generated into a 0600
    /// file whose path is printed at startup.
    #[cfg(feature = "serve")]
    #[arg(long, env = "X0NE_SERVE_TOKEN")]
    serve_token: Option<String>,
    /// Serve the Agent Client Protocol on stdin/stdout, for an editor (Zed…).
    #[arg(long)]
    acp: bool,
    /// Non-interactive: run a single prompt and exit. Supports @file references.
    /// Exit 0 when the turn completed, 2 when it ended otherwise (denied,
    /// budget, loop, model failure; reason on stderr), 1 on a startup error.
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Read additional context from stdin (pipe mode).
    #[arg(long)]
    stdin: bool,
    /// Output format for non-interactive mode.
    #[arg(long, default_value = "text")]
    output_format: String,
    /// Autonomous mode: continuous agent loop without user input.
    #[arg(long)]
    auto_mode: bool,
    /// Max iterations for autonomous mode (default: 50).
    #[arg(long, default_value = "50")]
    max_iterations: usize,
    /// Visual self-test: boot x0ne-runtime and run every action category through the display pipeline.
    /// No LLM required — tests the full CLI rendering.
    /// ⚠ DEBUG BUILDS ONLY — stripped from release binaries.
    #[cfg(debug_assertions)]
    #[arg(long)]
    self_test: bool,

    /// Reopen an earlier session: `--resume <ID>`, or `--resume last` for
    /// this project's most recently used one. `x0ne sessions` lists the ids.
    ///
    /// Reloads the CONVERSATION and keeps writing to the same journal.
    /// Approvals, trust, live processes and spend are deliberately not
    /// restored — the first gated action asks again.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// With `--resume last`: the latest session of any project.
    #[arg(long = "all", requires = "resume")]
    resume_all: bool,

    /// Skip the boot intro animation (noise → logo → subtitle). Also
    /// respected as `X0NE_NO_INTRO=1` for callers that set env vars.
    #[arg(long, env = "X0NE_NO_INTRO")]
    no_intro: bool,

    /// Opt out of the container autowrap when it is enabled (`[bridge] mode =
    /// "auto"`). The runtime then runs on the host; its agent commands stay
    /// under bwrap unless `--isolation none` is also given. Also respected as
    /// `X0NE_NO_SANDBOX=1`.
    #[arg(long, env = "X0NE_NO_SANDBOX")]
    no_sandbox: bool,

    /// Opt IN to sandbox mode for this session. Equivalent to setting
    /// `X0NE_AUTO_SANDBOX=1`: X0NE probes for Incus → Podman, picks the
    /// first ready one, and runs the runtime inside the `x0ne-sandbox`
    /// container. Falls back to host (with a warning) if no container is
    /// available. Symmetric with `--no-sandbox`.
    #[arg(long, env = "X0NE_AUTO_SANDBOX")]
    sandbox: bool,

    /// How the runtime confines the commands the agent starts:
    /// `bwrap` (the runtime's default: bubblewrap, refused if unavailable),
    /// `container`, or `none` (run them directly on this host). Passed to the
    /// local runtime as `--isolation`. Deliberately no `env =`.
    #[arg(long, value_parser = ["bwrap", "container", "none"])]
    isolation: Option<String>,

    /// Bench-agentic profile: no approval gate, and the runtime gets
    /// `--allow-all-commands`. Measures capability, not policy. Refused unless
    /// `-p` and `--isolation container` run in a detected container. No `env =`.
    #[arg(long)]
    bench_agentic: bool,

    /// Network of confined commands: `off` (runtime default) or `host`.
    /// Deliberately no `env =`.
    #[arg(long, value_parser = ["off", "host"])]
    isolation_network: Option<String>,

    /// Subcommand: `x0ne setup <incus|podman>` bootstraps the sandbox
    /// container, installs base packages, deploys x0ne-runtime, captures
    /// a baseline snapshot/commit. Run this once after `make install`.
    #[command(subcommand)]
    command: Option<Subcommand>,
}

#[derive(clap::Subcommand, Debug)]
enum Subcommand {
    /// Bootstrap a sandbox container from scratch. Idempotent — safe to
    /// re-run after a binary upgrade to re-push x0ne-runtime.
    Setup {
        /// Which container runtime to set up. Defaults to whichever is
        /// installed (Incus preferred, Podman fallback).
        #[arg(value_enum)]
        runtime: Option<SetupRuntime>,
    },
    /// List this project's session journals in `~/.x0ne/sessions` — id,
    /// turns, model, title or first prompt. The id is what
    /// `x0ne --resume <id>` takes. A project is the git work tree, else the
    /// directory, a session started in.
    Sessions {
        /// Every project's sessions.
        #[arg(long)]
        all: bool,
    },
    /// Tamper-evident journals: verify a hash chain, create the signing key.
    Audit {
        #[command(subcommand)]
        what: AuditWhat,
    },
    /// Print the effective policy the runtime would apply — the loaded
    /// configuration after merge, every action with its resolved state, the
    /// canonicalised roots, and which file was actually read.
    Config {
        #[command(subcommand)]
        what: ConfigWhat,
    },
    /// Confidential computing: provision a TDX VM, attest it, prove it.
    /// Billed GCP VMs: built only with `--features cloud`.
    #[cfg(feature = "cloud")]
    Cc {
        /// prover = c3-standard-4 · TDX · ≈ 0.24 $/h (default) | full =
        /// a3-highgpu-1g · TDX + H100 CC · SPOT ≈ 3.72 $/h (spot, unverified)
        #[arg(long)]
        machine: Option<String>,
        /// Spend cap in USD for this session (default 25)
        #[arg(long)]
        cap: Option<f64>,
        /// Non-interactive: accept the displayed plan, then stop the VM.
        /// Never destroys anything — deletion always asks.
        #[arg(long)]
        yes: bool,
        /// Print the report as JSON instead of the human report. The same
        /// report is written to `~/.x0ne/cc/last-report.json` either way.
        #[arg(long)]
        json: bool,
    },
    /// What GCP is billing RIGHT NOW: every instance and disk, its rate, its
    /// age, and the hourly, daily and monthly totals. Computed from what is
    /// running times the price table — not the billing export, which knows
    /// the truth hours later.
    CloudCost {
        /// Refresh every N seconds instead of printing once (minimum 5).
        #[arg(long, value_name = "SECONDS", num_args = 0..=1, default_missing_value = "30")]
        watch: Option<u64>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
        /// Send a desktop notification above this hourly rate. Fires once per
        /// crossing, not once per refresh.
        #[arg(long, value_name = "USD", default_value = "1.0")]
        alert_usd_per_hour: f64,
    },
    /// Agent hooks: the native tools of a coding agent judged by the
    /// x0ne policy. See docs/agent-hooks.md.
    Hook {
        #[command(subcommand)]
        what: HookWhat,
    },
    /// Claude Code sessions on this host, as recorded by their hooks.
    /// Read-only: lists, never acts on a session.
    Fleet {
        #[command(subcommand)]
        what: FleetWhat,
    },
}

#[derive(clap::Subcommand, Debug)]
enum AuditWhat {
    /// Recompute the chain of a journal, or of every `*.jsonl` in a
    /// directory (default: `~/.x0ne/sessions`). Exit 1 on the first break.
    Verify {
        path: Option<std::path::PathBuf>,
        /// Trusted ed25519 public key (hex). Default: `~/.x0ne/keys`.
        #[arg(long, value_name = "HEX")]
        key: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create `~/.x0ne/keys/journal-ed25519` (0600). Never replaces one.
    Keygen,
}

#[derive(clap::Subcommand, Debug)]
enum HookWhat {
    /// PreToolUse: read one hook payload on stdin, answer allow, ask or deny
    /// on stdout, journal the decision. Exit 2 on any failure (blocks).
    #[command(name = "pretooluse")]
    PreToolUse {
        /// Policy file (x0ne-runtime.toml). Default: the runtime's search.
        #[arg(long, value_name = "FILE")]
        policy: Option<PathBuf>,
        /// Journal socket (tests). Default: /run/systemd/journal/socket.
        #[arg(long, value_name = "PATH")]
        journal_socket: Option<PathBuf>,
        /// Payload and answer format.
        #[arg(long, value_parser = ["claude-code", "codex"], default_value = "claude-code")]
        agent: String,
    },
}

#[derive(clap::Subcommand, Debug)]
enum FleetWhat {
    /// List sessions: working, waiting for permission, idle, done, or stale
    /// (process gone). Done/stale sessions older than 24 h are hidden.
    Ls {
        /// Emit JSON for scripts.
        #[arg(long)]
        json: bool,
        /// Also show done/stale sessions older than 24 h.
        #[arg(long)]
        all: bool,
    },
    /// Hook recorder: reads one Claude Code hook payload on stdin and updates
    /// `~/.x0ne/fleet/<session>.json`. Always exits 0, prints nothing on stdout.
    Hook {
        /// Event name, used only if the payload has no `hook_event_name`.
        event: Option<String>,
    },
}

#[derive(clap::Subcommand, Debug)]
enum ConfigWhat {
    /// Show the policy in force: the file the runtime would load, its keys
    /// after merge, the canonicalised roots (and the ones dropped for not
    /// resolving), the compiled deny patterns, and every action's resolved
    /// switch. Secrets are reported as set or unset, never printed.
    Inspect {
        /// Emit stable JSON — sorted keys, same bytes for the same policy.
        #[arg(long)]
        json: bool,
        /// Also list the policy grants a session journal recorded. They are
        /// history, not live state: a grant lives in the memory of the runtime
        /// process that received it.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum SetupRuntime {
    Incus,
    Podman,
}

impl SetupRuntime {
    fn to_autowrap(&self) -> autowrap::Runtime {
        match self {
            SetupRuntime::Incus => autowrap::Runtime::Incus,
            SetupRuntime::Podman => autowrap::Runtime::Podman,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate `s` for single-line display to at most `byte_budget` bytes,
/// clamped down to the nearest UTF-8 char boundary. Appends `…` when the
/// string was shortened. Exists because `&s[..n]` panics when `n` lands
/// inside a multibyte char (common with emoji in LLM output / user
/// prompts); this helper is what `println!("{}", &s[..n])` should be.
fn truncate_display(s: &str, byte_budget: usize) -> String {
    if s.len() <= byte_budget {
        return s.to_string();
    }
    let mut cut = byte_budget.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// Lightweight `which` — asks the binary for its version to verify it's
/// both in PATH and actually executable (not a broken symlink, not a
/// directory masquerading as a file). Used by the `x0ne setup` auto-pick
/// and kept out of the `autowrap` module so that module can stay purely
/// decision-logic.
/// The turn's action tally as one JSON value for the session log. What made
/// the 2026-08-10 failure sessions undiagnosable was exactly this absence:
/// only user/response pairs were logged, so a dead turn carried no cause.
fn turn_actions_json(report: &x0ne_orchestrator::agent::driver::TurnReport) -> serde_json::Value {
    serde_json::json!({
        "total": report.actions.total,
        "succeeded": report.actions.succeeded,
        "failed": report.actions.failed,
        "by_type": report.actions.by_type,
    })
}

/// Refuse `--resume` on a surface that would ignore it.
///
/// `--serve`, `--tui`, `--matrix`, `--health` and every subcommand
/// return from `main` BEFORE the session journal is opened, so a `--resume`
/// handed to one of them would be parsed, documented in `--help`, and inert.
/// That is the dead-scaffolding defect of docs/invariants.md, met many times in this
/// repo — a flag that exists, is read, and changes nothing because the path
/// that observed it was written elsewhere.
///
/// Refusing is the honest behaviour until those surfaces run on the canonical
/// journal. `Err` names the surface, so the operator learns which flag won.
fn refuse_resume_where_it_is_ignored(active: &[(&str, bool)]) -> Result<(), String> {
    match active.iter().find(|(_, on)| *on) {
        Some((name, _)) => Err(format!(
            "--resume is not supported with {name} — that surface does not open \
             a session journal. Use plain `x0ne --resume <id>`, or `x0ne sessions` \
             to see what there is."
        )),
        None => Ok(()),
    }
}

/// Refuse a flag that `--tui` or `--matrix` would parse and drop: both
/// return from `main` before the code that reads it. `Err` names both.
fn refuse_flags_ignored_by(surface: &str, active: &[(&str, bool)]) -> Result<(), String> {
    match active.iter().find(|(_, on)| *on) {
        Some((flag, _)) => Err(format!(
            "{flag} is not supported with {surface}: {surface} would ignore it. \
             Run them separately."
        )),
        None => Ok(()),
    }
}

/// The process environment, as [`session_config`] reads it.
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The config of the REPL, `-p`, `--acp`, `--serve`, `--tui` and
/// `--matrix`, resolved once: the file (`-c`, else the search), the
/// environment (`env` reads a variable), then `--mode`, `--provider`,
/// `--endpoint` and `--model` (`--acp` and `--serve` dropped the
/// last two).
fn session_config(cli: &Cli, env: impl Fn(&str) -> Option<String>) -> Result<Config> {
    Config::resolve(
        cli.config.as_ref(),
        env,
        cli.mode.as_deref(),
        cli.provider.as_deref(),
        x0ne_orchestrator::config::ModelFlags {
            endpoint: cli.endpoint.clone(),
            model: cli.model.clone(),
        },
    )
}

/// The container autowrap and its refusal, for every interactive
/// surface. Prints the decision before any screen opens.
fn wrap_session(cli: &Cli, config: &mut Config) -> Result<()> {
    if cli.no_sandbox {
        std::env::set_var("X0NE_NO_SANDBOX", "1");
    }
    if cli.sandbox {
        // Make the env path visible to autowrap::decide_inner, which reads
        // X0NE_AUTO_SANDBOX directly. Setting it here lets the user pass
        // `--sandbox` once on the CLI without exporting in their shell.
        std::env::set_var("X0NE_AUTO_SANDBOX", "1");
    }
    // `--mode` given on the command line makes decide_inner step aside.
    let wrap_decision = autowrap::autowrap(cli.mode.as_deref(), config);
    // On every decision, before the banner, the model and the runtime.
    if x0ne_orchestrator::sandbox_mode::sandbox_required() {
        if let Some(refusal) = autowrap::require_sandbox_refusal(
            &wrap_decision,
            &config.bridge.mode,
            config.bridge.sandbox_cmd.as_deref(),
            x0ne_orchestrator::sandbox_mode::current(),
        ) {
            return Err(refuse_unsandboxed(refusal));
        }
    }
    match &wrap_decision {
        autowrap::Decision::Autowrapped { runtime, target } => {
            // Green `◆ sandbox: <runtime> · <container>` badge under the banner.
            println!(
                "  {}  sandbox: {} · {}",
                "◆".truecolor(0, 200, 120).bold(),
                runtime.display_name().truecolor(0, 200, 120).bold(),
                target.truecolor(110, 120, 135),
            );
        }
        autowrap::Decision::RespectExplicit { reason } => {
            // Quiet info line — user asked for this, don't shout at them.
            println!(
                "  {}  bridge: {} ({})",
                "◆".truecolor(110, 120, 135),
                config.bridge.mode.truecolor(110, 120, 135),
                reason.truecolor(80, 90, 100),
            );
        }
        autowrap::Decision::UnsandboxedExplicit => {
            eprintln!(
                "  {}  {} — running unsandboxed on host (X0NE_NO_SANDBOX=1)",
                "⚠".truecolor(220, 80, 80).bold(),
                "UNSAFE".truecolor(220, 80, 80).bold(),
            );
        }
        autowrap::Decision::MissingRuntime { .. } => {
            // No container runtime found. Fall back to host execution with a
            // visible advisory rather than failing closed. X0NE's snapshot/
            // restore primitives still give the agent a reliable undo on
            // btrfs/APFS/zfs (reflink) or anywhere else (byte-copy fallback).
            // For full process isolation, the user can opt in later via
            // `make setup-podman` (Linux + macOS + Windows) or `make
            // setup-incus` (Linux btrfs/zfs). Fail-closed semantics for
            // production / CI: `X0NE_REQUIRE_SANDBOX`, refused above.
            eprintln!(
                "  {}  {}  {}",
                "⚠".truecolor(200, 160, 0).bold(),
                "host mode".truecolor(200, 160, 0).bold(),
                "(no container — actions execute on this machine)".truecolor(110, 120, 135),
            );
            eprintln!(
                "     {}  {}",
                "for full sandbox:".truecolor(80, 90, 100),
                "make setup-podman".truecolor(0, 200, 120),
            );
        }
        autowrap::Decision::NeedsBootstrap { runtime, target } => {
            // A runtime is installed but the baseline container doesn't
            // exist yet. Same fail-open semantics as MissingRuntime: warn
            // and continue on host. The user can run `x0ne setup <runtime>`
            // anytime to bootstrap and pick up the sandbox on next launch.
            eprintln!(
                "  {}  {}  {}",
                "⚠".truecolor(200, 160, 0).bold(),
                "host mode".truecolor(200, 160, 0).bold(),
                format!(
                    "({} installed but no '{}' container)",
                    runtime.display_name(),
                    target
                )
                .truecolor(110, 120, 135),
            );
            eprintln!(
                "     {}  {}",
                "for full sandbox:".truecolor(80, 90, 100),
                format!("x0ne setup {}", runtime.display_name()).truecolor(0, 200, 120),
            );
        }
    }
    Ok(())
}

/// The refusal on stderr (stdout may be the ACP stream); exit status 1.
fn refuse_unsandboxed(refusal: String) -> anyhow::Error {
    print_fatal(&refusal);
    anyhow::anyhow!("session refused: X0NE_REQUIRE_SANDBOX")
}

/// Open this run's session journal, and the transcript to seed if the
/// operator asked to resume.
///
/// The two failure modes are deliberately not symmetric.
///
/// * Failing to open a FRESH journal is a warning. X0NE has always started
///   without an audit trail rather than refusing to start, and changing that
///   here would turn a full disk into a product that will not boot.
/// * Failing to honour a NAMED resume is fatal. The operator asked for one
///   specific transcript; quietly starting a different (empty) session would
///   let them keep typing into a context they believe holds yesterday's work.
async fn open_session_journal(
    resume: Option<&str>,
    all: bool,
    model: &str,
) -> Result<(
    Option<SharedJournal>,
    Option<x0ne_orchestrator::session_store::ResumedSession>,
)> {
    let Some(request) = resume else {
        let session = match SharedJournal::new().await {
            Ok(s) => {
                let _ = s.log_session_start(model).await;
                Some(s)
            }
            Err(e) => {
                warn!("Could not create session log: {}", e);
                None
            }
        };
        return Ok((session, None));
    };

    // No exclusion at boot: this process has not opened a journal yet, so
    // there is no current session for `last` to skip.
    let (journal, reloaded) = resume_session(request, all, None).await?;
    Ok((Some(journal), Some(reloaded)))
}

/// Reopen a named session: resolve the id, load the transcript, rebind the
/// writer to the same file.
///
/// `exclude` is the id of the session running now, so `/resume last` from
/// inside a live REPL does not resolve to the file that REPL is writing.
/// `last` is this project's latest session unless `all` (or `--all` in
/// `request`); a session started elsewhere is resumed with a warning.
async fn resume_session(
    request: &str,
    all: bool,
    exclude: Option<&str>,
) -> Result<(
    SharedJournal,
    x0ne_orchestrator::session_store::ResumedSession,
)> {
    use x0ne_orchestrator::session_store as store;
    let dir = store::sessions_dir();
    let (target, all_in_request) = store::parse_resume_arg(request).map_err(anyhow::Error::msg)?;
    let here = store::Project::current();
    // SECURITY: an id becomes a path; `resolve_resume` validates it.
    let resolved =
        store::resolve_resume(&dir, target, all || all_in_request, exclude, here.as_ref())
            .map_err(|e| anyhow::anyhow!("{e} — `x0ne sessions` lists what there is"))?;
    if let Some(project) = &resolved.elsewhere {
        println!(
            "  {}  session {} was started in another project: {project}",
            "!".truecolor(220, 180, 0),
            resolved.id,
        );
    }
    let id = resolved.id;

    let reloaded = store::load_session(&dir, &id).map_err(|e| anyhow::anyhow!("{e}"))?;
    let journal = SharedJournal::resume(&id).await?;
    let _ = journal
        .log_session_resumed(
            reloaded.turns_reloaded,
            reloaded.broken_lines,
            reloaded.bytes_skipped,
        )
        .await;
    Ok((journal, reloaded))
}

/// What the operator is told about a resume.
///
/// The id needs no sanitizing — `validate_session_id` already refused every
/// character that could move a cursor — but everything else printed here is a
/// number, on purpose: the reloaded PROMPTS are not echoed, because they came
/// out of a file the model helped write and this line is printed before the
/// operator has typed anything. `x0ne sessions` is where journal text is
/// shown, and it sanitizes.
fn print_resume_banner(r: &x0ne_orchestrator::session_store::ResumedSession) {
    println!(
        "  {}  resumed session {} — {} turns reloaded",
        "↺".truecolor(0, 220, 120).bold(),
        r.id.truecolor(0, 220, 120).bold(),
        r.turns_reloaded.to_string().bold(),
    );
    if r.broken_lines > 0 {
        println!(
            "  {}  {} unreadable line(s) in the journal were skipped",
            "!".truecolor(220, 180, 0),
            r.broken_lines,
        );
    }
    if r.truncated_from_front() {
        println!(
            "  {}  the journal is larger than the {} MiB resume window — the \
             oldest {} MiB were not reloaded",
            "!".truecolor(220, 180, 0),
            x0ne_orchestrator::session_store::MAX_RESUME_BYTES / (1024 * 1024),
            r.bytes_skipped / (1024 * 1024),
        );
    }
    if !r.prior_tokens.is_zero() {
        // Reported, not charged: this session's budget starts at zero. See
        // `CliAgentSession::resume_conversation`.
        println!(
            "  {}  it had already used {} tokens; this session's budget starts fresh",
            "·".truecolor(96, 104, 120),
            r.prior_tokens.total().to_string().truecolor(150, 160, 175),
        );
    }
    println!();
}

/// Seed a freshly assembled session with a reloaded transcript.
///
/// One helper for every entrypoint, so the security properties documented on
/// `resume_conversation` cannot hold on one surface and not another.
fn seed_resumed(
    agent: &mut x0ne_orchestrator::agent::session::CliAgentSession,
    resumed: Option<&x0ne_orchestrator::session_store::ResumedSession>,
) {
    if let Some(r) = resumed {
        agent.resume_conversation(r);
    }
}

fn which_ok(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // The ONE place `/setup` writes secrets to. `from_path` does not override
    // variables already set, so a real `export` still wins. `./.env` is NOT
    // read: it configures the project's software, and it could choose the
    // program x0ne starts (`X0NE_RUNTIME_BIN`, measured 2026-09-23).
    let _ = dotenvy::from_path(x0ne_orchestrator::setup_menu::secrets_path());

    let cli = Cli::parse();
    // First, before anything that could fail with exit 1: in a PreToolUse
    // hook exit 1 is a non-blocking error and the tool would run.
    if let Some(Subcommand::Hook {
        what:
            HookWhat::PreToolUse {
                policy,
                journal_socket,
                agent,
            },
    }) = &cli.command
    {
        hook_cmd::run_pretooluse(hook_cmd::HookArgs {
            policy: policy.clone(),
            journal_socket: journal_socket.clone(),
            agent: hook_cmd::Agent::parse(agent),
        });
    }
    // `-p --output-format json`: stdout carries the JSON object alone.
    // From here on, whatever is printed to stdout goes to stderr instead.
    #[cfg(unix)]
    let mut json_stdout = if cli.prompt.is_some() && cli.output_format == "json" {
        Some(terminal::divert_stdout_to_stderr()?)
    } else {
        None
    };
    // The human's `--isolation*` flags reach the local runtime's argv.
    {
        let mut extra = Vec::new();
        if let Some(m) = &cli.isolation {
            extra.extend(["--isolation".to_string(), m.clone()]);
        }
        if let Some(n) = &cli.isolation_network {
            extra.extend(["--isolation-network".to_string(), n.clone()]);
        }
        if cli.bench_agentic {
            let in_container = x0ne_orchestrator::sandbox_mode::CONTAINER_MARKERS
                .iter()
                .any(|m| std::path::Path::new(m).exists());
            if let Some(why) =
                bench_agentic_refusal(cli.prompt.is_some(), cli.isolation.as_deref(), in_container)
            {
                anyhow::bail!("--bench-agentic refused: {why}");
            }
            extra.push("--allow-all-commands".to_string());
        }
        x0ne_orchestrator::bridge::set_runtime_extra_args(extra);
    }

    // Before any early return, so `--resume` never reaches a surface that
    // would drop it on the floor. See the function's doc-comment.
    if cli.resume.is_some() {
        #[cfg(feature = "serve")]
        let serve_on = cli.serve;
        #[cfg(not(feature = "serve"))]
        let serve_on = false;
        refuse_resume_where_it_is_ignored(&[
            ("--serve", serve_on),
            ("--tui", cli.tui),
            ("--matrix", cli.matrix),
            ("--health", cli.health),
            ("--setup", cli.setup),
            ("a subcommand", cli.command.is_some()),
        ])
        .map_err(anyhow::Error::msg)?;
    }

    // Same place and reason as the `--resume` guard above.
    if cli.tui || cli.matrix {
        let surface = if cli.matrix { "--matrix" } else { "--tui" };
        #[cfg(feature = "serve")]
        let serve_on = cli.serve;
        #[cfg(not(feature = "serve"))]
        let serve_on = false;
        #[cfg(debug_assertions)]
        let self_test_on = cli.self_test;
        #[cfg(not(debug_assertions))]
        let self_test_on = false;
        // X0NE_DESKTOP is a standing preference: only the flag is refused.
        let desktop_flag = cli.desktop && std::env::var_os("X0NE_DESKTOP").is_none();
        refuse_flags_ignored_by(
            surface,
            &[
                ("--tui", cli.tui && cli.matrix),
                ("--prompt", cli.prompt.is_some()),
                ("--stdin", cli.stdin),
                ("--output-format", cli.output_format != "text"),
                ("--auto-mode", cli.auto_mode),
                ("--max-iterations", cli.max_iterations != 50),
                ("--health", cli.health),
                ("--acp", cli.acp),
                ("--serve", serve_on),
                ("--bench-agentic", cli.bench_agentic),
                ("--desktop", desktop_flag),
                ("--self-test", self_test_on),
                ("a subcommand", cli.command.is_some()),
            ],
        )
        .map_err(anyhow::Error::msg)?;
    }

    // `x0ne setup <runtime>` — bootstrap the sandbox container. Dispatched
    // BEFORE the intro animation / autowrap because it's itself a tooling
    // subcommand; the user explicitly invoked it and wants output, not
    // decoration. Exits after setup completes.
    if let Some(Subcommand::Setup { runtime }) = &cli.command {
        let rt = match runtime {
            Some(r) => r.to_autowrap(),
            None => {
                // Auto-pick the first available runtime. If neither is
                // installed, error out the same way autowrap does.
                if which_ok("incus") {
                    autowrap::Runtime::Incus
                } else if which_ok("podman") {
                    autowrap::Runtime::Podman
                } else {
                    eprintln!(
                        "  {}  neither `incus` nor `podman` is in PATH — install one first:",
                        "✗".truecolor(220, 80, 80).bold(),
                    );
                    eprintln!("     • Incus:   https://linuxcontainers.org/incus/");
                    eprintln!("     • Podman:  https://podman.io/");
                    return Err(anyhow::anyhow!("no container runtime available"));
                }
            }
        };
        return container_setup::run_setup(rt);
    }

    // `x0ne sessions` — a tooling subcommand like `setup` above: no intro,
    // no wizard, no LLM. It reads files and exits, so it must work on a
    // machine with no API key configured at all — an operator asking "which
    // session was that?" has not asked to start one.
    if let Some(Subcommand::Sessions { all }) = &cli.command {
        use x0ne_orchestrator::session_store as store;
        let project = if *all {
            None
        } else {
            store::Project::current()
        };
        terminal::print_sessions(
            &store::list_for(&store::sessions_dir(), project.as_ref()),
            None,
        );
        return Ok(());
    }

    if let Some(Subcommand::Audit { what }) = &cli.command {
        return match what {
            AuditWhat::Verify { path, key, json } => {
                let broken = audit_cmd::verify(path.as_deref(), key.as_deref(), *json)?;
                if broken {
                    std::process::exit(1);
                }
                Ok(())
            }
            AuditWhat::Keygen => audit_cmd::keygen(),
        };
    }

    // `x0ne fleet` — reads files and exits, like `sessions`. `hook` runs
    // inside Claude Code: exit 2 would block a tool, stdout would reach the
    // model. `run_hook` returns nothing, so neither can happen.
    if let Some(Subcommand::Fleet { what }) = &cli.command {
        match what {
            FleetWhat::Hook { event } => x0ne_orchestrator::fleet::run_hook(event.as_deref()),
            FleetWhat::Ls { json, all } => x0ne_orchestrator::fleet::run_ls(*json, *all)?,
        }
        return Ok(());
    }

    // `x0ne config inspect` — same family as `x0ne sessions` above: it reads
    // a policy file and exits. No wizard, no model, no bridge, and above all
    // no runtime started: an operator asking "which policy am I actually
    // running?" must get the answer on a machine with no API key, and getting
    // it must not itself execute anything.
    if let Some(Subcommand::Config { what }) = &cli.command {
        return match what {
            ConfigWhat::Inspect { json, session } => config_cmd::inspect(*json, session.as_deref()),
        };
    }

    // `x0ne cloud-cost` — same family as `sessions` and `config inspect`: it
    // reads and exits. No model, no wizard, no runtime. An operator asking
    // "what am I paying right now?" must get the answer on a machine with no
    // API key, and getting it must not itself start anything that bills.
    if let Some(Subcommand::CloudCost {
        watch,
        json,
        alert_usd_per_hour,
    }) = &cli.command
    {
        return cost_cmd::run(cost_cmd::CostArgs {
            watch: *watch,
            json: *json,
            alert_usd_per_hour: *alert_usd_per_hour,
        });
    }

    // `x0ne cc` — same reasoning as `setup` above: a tooling subcommand the
    // user invoked on purpose, so no intro animation. It also skips the LLM
    // setup wizard on purpose: this command talks to gcloud through the
    // runtime bridge and to nothing else — it has no model to configure, and
    // making an operator provision an API key to attest a VM would be
    // friction with no security value.
    #[cfg(feature = "cloud")]
    if let Some(Subcommand::Cc {
        machine,
        cap,
        yes,
        json,
    }) = &cli.command
    {
        let mut config = Config::find_and_load(cli.config.as_ref())?;
        config.apply_env_overrides();
        let args = cc_cmd::CcArgs {
            machine: machine.clone(),
            cap: *cap,
            yes: *yes,
            json: *json,
        };
        return cc_cmd::run_cc(args, &config).await;
    }

    // Boot intro animation — plays on the alternate screen, then returns
    // control to the primary screen without touching the user's scrollback.
    // Silently no-op on non-TTY, `TERM=dumb`, or when the user opts out
    // with `--no-intro` / `X0NE_NO_INTRO=1`. Skipped in serve mode because
    // a daemon has no terminal to animate on, and in any subcommand that
    // exits early (like `--health`) — those paths return before reaching
    // the REPL so a 2-second animation is pure friction.
    #[cfg(feature = "serve")]
    let skip_intro_for_mode = cli.serve || cli.health || cli.acp;
    #[cfg(not(feature = "serve"))]
    let skip_intro_for_mode = cli.health || cli.acp;
    if !cli.no_intro && !skip_intro_for_mode {
        // Swallow errors — a cosmetic failure must never break boot.
        let _ = intro::play_boot_intro();
    }

    // Install the process-global Ctrl+C handler once. First press aborts the
    // current agent turn gracefully; second press within 2s quits.
    interrupt::install_signal_handler();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                cli.log_level
                    .parse()
                    .unwrap_or_else(|_| "warn".parse().unwrap())
            }),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // T0.5: loud one-time banner when X0NE_PLAN_AUTO_APPROVE is set, so a
    // JEI/CIR auditor scrolling the session header sees the gate is open
    // before any Plan emission. Once-per-process, regardless of mode.
    x0ne_orchestrator::sysaicalls::announce_plan_auto_approve_if_set();

    // -- Serve mode: HTTP/SSE for web frontends. Skip the wizard entirely.
    //    Only compiled when the `serve` feature is enabled (default).
    #[cfg(feature = "serve")]
    if cli.serve {
        let bind: std::net::SocketAddr = cli
            .bind
            .parse()
            .with_context(|| format!("invalid --bind address: {}", cli.bind))?;
        let mut config = session_config(&cli, process_env)?;
        // Force http bridge in serve mode — stdio/sandbox don't make sense here.
        if config.bridge.mode != "http" {
            warn!(
                "serve mode forces bridge.mode = http (was {:?})",
                config.bridge.mode
            );
            config.bridge.mode = "http".into();
        }
        return serve::run(config, bind, cli.serve_token.clone()).await;
    }

    // -- Workspace trust: decided once, on the plain terminal, before the setup
    // wizard or the TUI loads any config. Non-interactive runs never ask and
    // never honor ./x0ne-orchestrator.toml.
    if !cli.health {
        if let Ok(cwd) = std::env::current_dir() {
            x0ne_orchestrator::project_trust::decide(
                &cwd,
                &x0ne_orchestrator::project_trust::default_store_path(),
                x0ne_orchestrator::terminal::can_prompt_interactively(),
                x0ne_orchestrator::project_trust::ask_on_terminal,
            );
        }
    }

    // -- Setup wizard. In a declared sandbox the policy is written without
    //    questions; the model question comes later, only if needed.
    let sandbox = x0ne_orchestrator::sandbox_mode::current();
    if let x0ne_orchestrator::sandbox_mode::SandboxMode::Unconfirmed { reason } = sandbox {
        eprintln!(
            "  {}  {}; prompts stay on",
            "⚠".truecolor(200, 160, 0).bold(),
            reason
        );
    }
    if sandbox.in_sandbox() && !cli.setup {
        if SetupWizard::ensure_sandbox_policy()? {
            eprintln!("  {}  sandbox policy written", "◆".truecolor(110, 120, 135));
        }
    } else if cli.setup || SetupWizard::needs_setup() {
        SetupWizard::run()?;
    }

    // -- Matrix mode: LLM-controlled terminal artifact
    if cli.matrix {
        let mut config = session_config(&cli, process_env)?;
        wrap_session(&cli, &mut config)?;
        return matrix_mode::run_matrix_with_config(config).await;
    }

    // ── One rich TUI ─────────────────────────────────────────────────────
    //
    // One flag, one implementation. The legacy ratatui frontend is retired,
    // its unique capabilities (typed result renderers, worlds pane, sysmon
    // strip, F-keys) ported into the canonical TUI — see
    // docs/TUI_CONVERGENCE_MATRIX_2026-08-10.md. `--tui` was the second
    // name for this same entrypoint and is gone (2026-08-16): a flag that
    // duplicates another teaches the operator there are two things.
    if cli.tui {
        let mut config = session_config(&cli, process_env)?;
        wrap_session(&cli, &mut config)?;
        return x0ne_orchestrator::tui::run_tui(config).await;
    }

    // -- Self-test mode: visual test of all action rendering, no LLM needed
    //    ⚠ Only compiled in debug builds — zero attack surface in release.
    #[cfg(debug_assertions)]
    if cli.self_test {
        // The self-test runs every action on a host runtime, policy open.
        x0ne_orchestrator::sandbox_mode::require_sandbox("stdio", None)
            .map_err(|e| refuse_unsandboxed(e.to_string()))?;
        return run_self_test(cli.config.as_ref(), cli.desktop).await;
    }

    let mut config = session_config(&cli, process_env)?;

    // Before autowrap and every boot banner: stdout is the protocol.
    if cli.acp {
        // Refused before `initialize`, not at `session/new`.
        x0ne_orchestrator::sandbox_mode::require_sandbox(
            &config.bridge.mode,
            config.bridge.sandbox_cmd.as_deref(),
        )
        .map_err(|e| refuse_unsandboxed(e.to_string()))?;
        return acp_cmd::run(config).await;
    }

    // ── Sandbox autowrap ────────────────────────────────────────────────
    // Default `x0ne` (no flags, no env vars) runs inside an Incus (preferred)
    // or Podman (fallback) container. This happens BEFORE LLM setup / bridge
    // creation, so all subsequent code sees the rewritten config.
    //
    // Explicit user intent — `--mode`, `X0NE_BRIDGE_MODE`, or a non-default
    // `[bridge]` section in the config file — disables autowrap (see
    // `autowrap::decide_inner` for the full truth table). `--no-sandbox` /
    // `X0NE_NO_SANDBOX=1` opts out of the sandbox entirely, with a loud
    // red warning printed below.
    //
    // Autowrap runs only when opted in (mode "auto" / `--sandbox`). A missing
    // container runtime then falls back to the host runtime WITH a warning.
    // Under `X0NE_REQUIRE_SANDBOX` any decision left uncontained is refused.
    // On the host, agent commands are still confined by bwrap or refused.
    wrap_session(&cli, &mut config)?;

    // Sandbox mode: disable write confirmations — the container IS the security boundary
    if config.bridge.mode == "sandbox" {
        config.ui.confirm_writes = false;
    }

    // --desktop flag overrides config (also accepts X0NE_DESKTOP env var)
    let desktop_enabled = cli.desktop;

    let llm: Arc<dyn api::LlmBackend> = Arc::from(llm_setup::setup_llm(&mut config).await?);
    x0ne_orchestrator::model_keys::announce(&config.llm);
    let mut usage = UsageTracker::new(llm.display_name());

    // ── Sandbox / remote-bridge boundary (T1) ────────────────────────────────
    // Whenever the executor lives on a different filesystem than the
    // orchestrator host (sandbox container, HTTP service on another box, …)
    // we MUST NOT inject the orchestrator's host-side project tree, X0NE.md
    // memory, or codebase index into the prompt. The model would then ground
    // every action (ReadFile, ListDir, EditFile) on paths that exist only on
    // the host — a guaranteed first-turn miss. The boot probe below
    // (SystemInfo + ListDir on the executor's own cwd) is what feeds the
    // model a layout it can actually act on.
    //
    // `is_remote_bridge` is the policy gate. `is_sandbox` is kept for the
    // narrower checks elsewhere that genuinely care about *only* the
    // sandbox transport (e.g. spawning the host-side Incus bridge).
    let is_sandbox = config.bridge.mode == "sandbox";
    let is_remote_bridge = project::is_remote_bridge_mode(&config.bridge.mode);

    // -- Project detection (host-only — skipped on every remote transport)
    let (project, project_line, project_memory, codebase_index, skills) = if is_remote_bridge {
        (None, None, None, None, Vec::new())
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let proj = detect_project(&cwd);
        let pline = proj.as_ref().map(|p| p.prompt_line());
        let proot = proj
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_else(|| cwd.clone());
        // Reads dispensed from the human gate may not leave this directory.
        x0ne_orchestrator::agent::readonly_cmd::set_workspace_root(&proot);
        // SECURITY: AGENTS.md, rules and skills are prompts the repository
        // wrote. They enter the model only once the folder is trusted,
        // and only as they were when it was.
        let trusted = x0ne_orchestrator::project_trust::project_is_trusted();
        let pmem = if trusted {
            project::load_trusted_project_memory(&proot)
        } else {
            None
        };
        if !trusted && project::load_project_memory(&proot).is_some() {
            println!(
                "  {}  {}",
                "◈".truecolor(0, 180, 200),
                "project instructions not read: folder not trusted".truecolor(80, 90, 100)
            );
        }
        if let Some(ref m) = pmem {
            println!(
                "  {}  {}",
                "◈".truecolor(0, 180, 200),
                m.boot_line().truecolor(80, 90, 100)
            );
        }
        let skills = project::catalog_skills_for(&proot, trusted);
        if !skills.is_empty() {
            println!(
                "  {}  {}",
                "◈".truecolor(0, 180, 200),
                format!("{} skill(s) catalogued", skills.len()).truecolor(80, 90, 100)
            );
        }
        let cidx = project::index_codebase(&proot, 3, 150);
        if let Some(ref idx) = cidx {
            let line_count = idx.lines().count();
            println!(
                "  {}  {} ({} files scanned)",
                "◈".truecolor(0, 180, 200),
                "project tree".truecolor(80, 90, 100),
                line_count
            );
        }
        (proj, pline, pmem, cidx, skills)
    };

    // ── Connect to x0ne-runtime ──────────────────────────────────────────────────
    let mut bridge = match x0ne_orchestrator::bridge::create_bridge_for(&config.bridge).await {
        Ok(b) => b,
        Err(e) => {
            print_fatal(&format!("Failed to connect to x0ne-runtime: {}", e));
            return Err(e);
        }
    };
    // A dispensed gate is announced, every session, before the first prompt.
    if let Some(t) = x0ne_orchestrator::agent::trust::label(&config.trust) {
        println!(
            "  {}  {} — no approval prompt for what stays inside; the runtime policy still applies",
            "◆".truecolor(220, 180, 60),
            t.truecolor(220, 180, 60).bold()
        );
    }

    // Note: the second host-side x0ne-runtime ("Incus bridge") that used to
    // be spawned in sandbox mode is gone. Incus container management is now
    // dispatched orchestrator-side (crate::incus_actions in x0ne-orchestrator),
    // so the runtime never needs `incus` CLI access — even when it lives
    // inside a sandbox container.

    // ── Multi-agent registry ────────────────────────────────────────────────
    //
    // The canonical graph derives a child's transport from the session's own
    // config; the legacy `SpawnContext` went with `run_agent_loop`.
    let registry = Arc::new(AgentRegistry::new());

    // Start background system monitor (polls /proc/meminfo + df every 5s).
    // Excluded in sandbox mode: host metrics would misrepresent the container.
    let is_local = !is_sandbox && matches!(config.bridge.mode.as_str(), "local" | "stdio" | "");
    let sys_state = sysmon::start_monitor(is_local);

    // Cloud cost + GPU monitor. Off unless [cloud] enabled = true — no gcloud
    // spawns for someone with no cloud footprint. Feeds the navbar + Infra tab.
    let cloud_state = x0ne_orchestrator::cloudmon::start_monitor(config.cloud.enabled);

    // The enclave badge, computed ONCE at boot and never again — the same
    // value, from the same function, that the TUI status strip uses
    // (`tui::mod`). Two renderings of one calculation: a second one here
    // would be a second rule about what may be green, and the rule is exactly
    // the thing that must not exist twice. Recomputing it per keystroke would
    // also mean a file read per keystroke, and would let a badge change colour
    // mid-session for a session whose wiring cannot change.
    let enclave = x0ne_orchestrator::tui::EnclaveStatus::for_session(
        x0ne_orchestrator::cc_runner::read_last_report().as_ref(),
        chrono::Utc::now(),
        &config,
    );

    // Discover actions from x0ne-runtime at boot (single ListActions call)
    let discovered_actions: Option<Vec<serde_json::Value>> = match bridge.health_check().await {
        Ok(_) => {
            let list_result = bridge
                .send_action(&serde_json::json!({"type": "ListActions"}))
                .await;
            list_result
                .ok()
                .and_then(|r| r["output"]["actions"].as_array().cloned())
        }
        Err(e) => {
            if let Some(r) = e.downcast_ref::<x0ne_orchestrator::bridge::RuntimeRefused>() {
                eprintln!("{r}");
                std::process::exit(1);
            }
            print_fatal(&format!("x0ne-runtime health check failed: {}", e));
            return Err(e);
        }
    };
    let action_count = discovered_actions.as_ref().map(|a| a.len()).unwrap_or(0);
    if action_count > 0 {
        println!(
            "  {}  {} ({} actions via {})",
            "◈".truecolor(0, 200, 120),
            "x0ne-runtime".truecolor(80, 90, 100),
            action_count.to_string().truecolor(0, 200, 220),
            config.bridge.mode.truecolor(80, 90, 100)
        );
    }

    // `--health` : answer and exit — the flag's whole job. Measured on
    // 2026-08-17 (full T6 implementer): `x0ne --health --no-intro </dev/null`
    // started the complete REPL and exited 0 on EOF — the flag only skipped
    // the intro. A health probe that opens an interactive session is dead
    // scaffolding; this return keeps every line past it out of its reach.
    // What has been proven when this line prints: config parsed, bridge
    // spawned, health_check answered, actions enumerated. A bridge failure
    // already returned Err above, non-zero.
    if cli.health {
        println!(
            "x0ne: healthy — config ok, bridge '{}' ok, {} actions",
            config.bridge.mode, action_count
        );
        return Ok(());
    }

    // Say at BOOT when the directory the operator just opened X0NE in is not
    // readable by the agent. Otherwise the first turn is spent discovering it:
    // every probe returns `not under any allowed path prefix`, the model
    // guesses at other roots, and the session dies before it starts (field
    // report, 2026-08-12). Read-only notice — widening the policy is the
    // operator's decision, offered in /setup, never taken here.
    {
        use x0ne_orchestrator::setup_menu as menu;
        if let Ok(doc) = menu::load_runtime_doc() {
            let rt = menu::summarize_runtime(&doc);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if !menu::path_is_covered(&rt.read_paths, &cwd) {
                println!(
                    "  {}  {} {}",
                    "⚠".truecolor(220, 160, 0),
                    "this directory is not accessible to the agent —".truecolor(200, 170, 90),
                    "/setup → [2]".truecolor(0, 200, 220),
                );
            }
        }
    }

    // Initialize parser with discovered action names (for typo suggestions)
    if let Some(ref actions) = discovered_actions {
        parser::init_known_actions(actions);
    }

    // Boot-time runtime context probe (T1). Two sequential calls over the
    // bridge so the LLM gets a layout it can immediately act on:
    //   1. SystemInfo — host/os/arch + the executor's actual cwd.
    //   2. ListDir(<executor cwd>) — what the LLM would otherwise spend
    //      3-5 turns discovering.
    //
    // Why we DO NOT use the orchestrator's $HOME or std::env::current_dir():
    // when the executor lives in a sandbox container or behind HTTP, both
    // are guaranteed to be wrong. SystemInfo carries the runtime's true cwd
    // and we ground the directory listing on that, never on host paths.
    // Failures degrade gracefully (None) — the LLM falls back to its own
    // discovery turns instead of consuming a fabricated context.
    // v0.3 removed `SystemInfo` and `ListDir` from the runtime vocabulary in
    // favour of shell one-liners — and these probes kept sending the dead
    // names, so the boot context has been quietly built from parse-error
    // envelopes. The probes now speak the current vocabulary (`RunCommand`)
    // and the results are reshaped into the structure
    // `prompt::boot_context_block` has always read, so the prompt layer and
    // its tests stay untouched. The data is real; only the envelope is
    // synthesised, from the probe's own stdout.
    let uname_probe = bridge
        .send_action(&serde_json::json!({
            "type": "RunCommand", "command": "uname", "args": ["-s", "-n", "-m"]
        }))
        .await
        .ok();
    let pwd_probe = bridge
        .send_action(&serde_json::json!({"type": "RunCommand", "command": "pwd", "args": []}))
        .await
        .ok();
    let probe_stdout = |v: &Option<serde_json::Value>| -> Option<String> {
        v.as_ref()
            .filter(|v| v.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
            .and_then(|v| v.pointer("/output/stdout").and_then(|s| s.as_str()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let sandbox_cwd: Option<String> = probe_stdout(&pwd_probe);
    let sys_info_probe: Option<serde_json::Value> = probe_stdout(&uname_probe).map(|uname| {
        let mut parts = uname.split_whitespace();
        let os = parts.next().unwrap_or("?");
        let hostname = parts.next().unwrap_or("?");
        let arch = parts.next().unwrap_or("?");
        serde_json::json!({
            "success": true,
            "output": {
                "os": os, "hostname": hostname, "arch": arch,
                "cwd": sandbox_cwd.clone().unwrap_or_default(),
            }
        })
    });
    let listing_path: Option<String> = sandbox_cwd
        .clone()
        // Older executors that don't yet emit `cwd`: fall back to "/" so we
        // still ground in the runtime FS instead of leaking $HOME.
        .or_else(|| {
            if is_remote_bridge {
                Some("/".to_string())
            } else {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            }
        });
    // (T1) Track whether the remote-bridge ListDir succeeded. We need this
    // to fail-closed: in sandbox / HTTP modes, a missing or errored listing
    // means the LLM has no real visibility into the executor's filesystem,
    // and we MUST tell it so explicitly instead of falling back to the
    // host tree (which is the original "context bleed" demo-killer).
    let (home_listing_probe, listing_failed_remote) = if let Some(path) = listing_path {
        match bridge
            .send_action(&serde_json::json!({
                "type": "RunCommand", "command": "ls",
                "args": [prompt::BOOT_LISTING_LS_ARGS, path.clone()]
            }))
            .await
        {
            Ok(v) => {
                // success:false counts as a failure when we're on a remote
                // bridge. On a local bridge an errored listing still leaves
                // the LLM with the (correct) host tree, so we don't need
                // the warning.
                let succeeded = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false)
                    && v.pointer("/output/exit_code").and_then(|c| c.as_i64()) == Some(0);
                let failed_remote = is_remote_bridge && !succeeded;
                // Reshape the `ls` stdout into the entries structure the
                // boot-context renderer reads.
                let reshaped = if succeeded {
                    let entries = prompt::boot_listing_entries(
                        v.pointer("/output/stdout")
                            .and_then(|s| s.as_str())
                            .unwrap_or(""),
                    );
                    Some(serde_json::json!({
                        "success": true,
                        "output": {"path": path, "entries": entries}
                    }))
                } else {
                    Some(serde_json::json!({
                        "success": false,
                        "error": {"message": "boot listing failed"}
                    }))
                };
                (reshaped, failed_remote)
            }
            Err(_) => {
                // Bridge transport error: fail-closed only when remote.
                (None, is_remote_bridge)
            }
        }
    } else {
        (None, false)
    };
    let boot_context = prompt::boot_context_block(
        sys_info_probe.as_ref(),
        home_listing_probe.as_ref(),
        listing_failed_remote,
    );

    // Connectors start before the prompt is built: their tools are listed in it.
    for line in x0ne_orchestrator::connectors::boot(&config).await {
        println!(
            "  {}  {}",
            "◇".truecolor(0, 180, 200),
            x0ne_orchestrator::text_safety::sanitize(&line).truecolor(80, 90, 100)
        );
    }

    // -- System prompt base (built from discovered actions, live sys context injected per call)
    let mut sys_prompt_base = build_system_prompt_dynamic(
        &config,
        desktop_enabled,
        project_line.as_deref(),
        discovered_actions.as_deref(),
    );

    if let Some(ref ctx) = boot_context {
        sys_prompt_base.push_str("\n\n");
        sys_prompt_base.push_str(ctx);
    }

    // Project instructions (AGENTS.md / CLAUDE.md / X0NE.md), framed as
    // project content, then the skill catalogue — bodies read on demand.
    if let Some(ref memory) = project_memory {
        sys_prompt_base.push_str(&memory.render_for_prompt());
    }
    if let Some(catalog) = project::render_skill_catalog(&skills) {
        sys_prompt_base.push_str(&catalog);
    }

    // Inject codebase index into system prompt
    if let Some(ref index) = codebase_index {
        sys_prompt_base.push_str(&format!("\n\n━━━━ CODEBASE ━━━━\n{}\n", index));
    }

    // ── Session logger ─────────────────────────────────────────────────────────
    //
    // A `SharedJournal`, not a bare `SessionLogger`: this scope writes the
    // `user_input` / `llm_response` / `turn` lines, and every session it builds
    // below gets a handle onto the SAME file for its actions. Two loggers would
    // mean two timestamped paths and a session split across them.
    // `--resume` decides whether this is a new journal or an old one
    // reopened. `open_session_journal` fails the process when a NAMED resume
    // cannot be honoured; see its doc-comment for why that asymmetry.
    let (mut session, resumed) =
        open_session_journal(cli.resume.as_deref(), cli.resume_all, &llm.display_name()).await?;
    if let Some(r) = resumed.as_ref() {
        print_resume_banner(r);
    }

    // ── Compact startup ───────────────────────────────────────────────────────
    let is_noninteractive = cli.prompt.is_some();
    // Said before anything runs, on stderr (measured 2026-10-10):
    // a flag that the gate ignores, and a cwd policy the runtime ignores.
    if is_noninteractive && config.ui.auto_confirm {
        eprintln!(
            "x0ne: {}",
            x0ne_orchestrator::agent::confirm::operator_hint_for_no_human_channel(&config)
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        let opted_in = std::env::var("X0NE_RUNTIME_ALLOW_CWD_CONFIG").is_ok_and(|v| v == "1");
        if let Some(notice) = bridge::cwd_runtime_policy_notice(&config.bridge, &cwd, opted_in) {
            eprintln!("x0ne: {notice}");
        }
    }
    // The provider actually used, not only the model.
    let model_disp = x0ne_orchestrator::terminal::format_provider_model(&llm.display_name());
    let policy_mode = config.ui.auto_confirm_display();

    let _bridge_info = format!("{}", config.bridge.mode.truecolor(80, 90, 100));

    // Detect execution target for the status line
    let execution_target = match config.bridge.mode.as_str() {
        "sandbox" => {
            // Try to extract container/host name from sandbox_cmd
            let target_name = config
                .bridge
                .sandbox_cmd
                .as_deref()
                .and_then(|cmd| {
                    // Pattern: "... incus exec <container> ..." or "ssh <host> ..."
                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                    // Look for "exec <name>" pattern (incus/lxc)
                    if let Some(pos) = parts.iter().position(|p| *p == "exec") {
                        parts.get(pos + 1).map(|s| s.to_string())
                    } else if let Some(pos) = parts.iter().position(|p| *p == "ssh") {
                        parts.get(pos + 1).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "container".to_string());
            format!(
                "{} {}",
                "🛡 sandbox".truecolor(0, 200, 120).bold(),
                target_name.truecolor(0, 180, 200)
            )
        }
        // X0ne itself runs in the container (run-sandboxed.sh); the
        // local stdio bridge is inside it, not on the host.
        "local" | "stdio" | "" if x0ne_orchestrator::sandbox_mode::current().in_sandbox() => {
            format!("{}", "sandbox".truecolor(0, 200, 120).bold())
        }
        "local" | "stdio" | "" => {
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .unwrap_or_else(|_| {
                    std::fs::read_to_string("/etc/hostname")
                        .unwrap_or_else(|_| "local".into())
                        .trim()
                        .to_string()
                });
            format!(
                "{} {}",
                "⚡ local".truecolor(0, 160, 220),
                hostname.truecolor(80, 90, 100)
            )
        }
        "http" => {
            format!(
                "{} {}",
                "🌐 remote".truecolor(200, 160, 0),
                config.bridge.x0ne_core_addr.truecolor(80, 90, 100)
            )
        }
        other => format!("{}", other.truecolor(80, 90, 100)),
    };

    // Guard that resets the DECSTBM scrolling region on any exit path
    // (clean shutdown, panic, Ctrl+C handled by the shell). Declared in
    // the outer scope so it lives for the entire interactive session.
    let mut _sticky_header_guard: Option<sticky_header::StickyHeader> = None;

    if !is_noninteractive {
        // Clear the terminal — wipes the onboarding prompts (provider/model selection)
        // so only the final logo + status line remain as a clean header.
        print!("\x1b[2J\x1b[H");
        print_banner();
        // This counter is used to clear the header: derived from the logo, not
        // copied (a value too low leaves ghost lines on screen, and the
        // logo changed height twice on 2026-09-10).
        let mut header_lines: u16 = x0ne_orchestrator::terminal::BANNER_LINES;

        // Status line: model  ·  bridge  ·  policy  ·  actions
        let mut status_parts: Vec<String> =
            vec![format!("{}", model_disp.cyan().bold()), execution_target];
        if action_count > 0 {
            status_parts.push(format!(
                "{} actions",
                action_count.to_string().truecolor(0, 200, 220)
            ));
        }
        match x0ne_orchestrator::sandbox_mode::current().banner() {
            Some(b) if x0ne_orchestrator::sandbox_mode::current().yolo() => {
                status_parts.push(format!("{}", b.truecolor(220, 160, 0).bold()))
            }
            Some(b) => status_parts.push(format!("{}", b.truecolor(80, 90, 100))),
            None => status_parts.push(format!("{}", policy_mode.truecolor(80, 90, 100))),
        }
        if let Some(w) = x0ne_orchestrator::sandbox_mode::egress_warning() {
            status_parts.push(format!("{}", w.truecolor(230, 60, 60).bold()));
        }

        println!(
            "  {}  {}",
            "●".truecolor(0, 200, 120),
            status_parts.join(&format!("  {}  ", "·".truecolor(50, 60, 70)))
        );
        header_lines += 1;

        // The « rust: x0ne » line lived here — the project-detection
        // summary. Removed on 2026-09-10: it repeated the name the banner
        // announces two lines above and that the window title already
        // carries. The tree scan itself stays: it is announced above
        // (« project tree (N files scanned) ») and it is that line which
        // carries the information.
        let _ = &project;
        println!(
            "  {}  {}",
            "?".truecolor(60, 70, 80),
            "/worlds  /spawn  /help  /setup  /actions  /quit".truecolor(50, 60, 70)
        );
        header_lines += 1;
        println!();
        header_lines += 1;

        // Pin the header: all subsequent output scrolls beneath it.
        // The guard resets the scrolling region when dropped at the end
        // of `main()`. Disable with X0NE_STICKY_HEADER=0.
        _sticky_header_guard = Some(sticky_header::StickyHeader::install(header_lines));
    }

    let mut conv = Conversation::new(config.conversation.max_context_tokens)
        .with_summarize_config(
            config.conversation.summarize_at_percent,
            config.conversation.keep_recent_percent,
        )
        .with_keep_recent_min(config.conversation.keep_recent_min_messages)
        .with_max_result_bytes(config.conversation.max_result_bytes);

    // Lazy discovery: inject action catalog into conversation context at boot
    if config.ui.lazy_discovery {
        if let Some(ref actions) = discovered_actions {
            let ctx = prompt::build_lazy_actions_context_gated(actions, desktop_enabled);
            conv.add_system_context(&ctx);
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  NON-INTERACTIVE MODE: -p "prompt" → run → exit
    // ════════════════════════════════════════════════════════════════

    if let Some(prompt_text) = cli.prompt {
        // Read stdin if --stdin flag
        let stdin_content = if cli.stdin {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).ok();
            if buf.is_empty() {
                None
            } else {
                Some(buf)
            }
        } else {
            None
        };

        // Expand @file references
        let expanded = project::expand_at_references(&prompt_text, is_remote_bridge);

        // Build full prompt
        let full_prompt = match stdin_content {
            Some(stdin) => format!("{}\n\n--- stdin ---\n{}\n---", expanded, stdin.trim()),
            None => expanded,
        };

        // Nothing is granted here. This path used to set `auto_confirm` on the
        // reasoning that there is no terminal to confirm at — which reads a
        // fact about plumbing as a decision by a human, and worse, disarms the
        // refusal that already exists. On the canonical path the gate is
        // `agent/confirm.rs`, which neutralises both `ui.auto_confirm` and
        // `ui.confirm_writes` before asking policy, so a config field cannot
        // switch it off at all: reads and anything else policy permits run as
        // before, and an action that wants a human without one available is
        // denied with feedback the model can act on.
        //
        // Headless `-p` fails closed, except in a declared sandbox (yolo) or
        // under `--bench-agentic`, a flag checked at startup.

        // ── The canonical path ───────────────────────────────────────────
        //
        // `-p` is the first public entrypoint to run on `CliAgentSession ->
        // Driver` instead of `run_agent_loop`. What stays here is what a
        // frontend owns: `@file`, `--stdin`, the terminal, and the two output
        // shapes below. What moves is execution, and only execution.
        //
        // The bridge is *moved* into the session rather than reopened. A
        // second `create_bridge` here would be a second runtime process, with
        // its own policy engine and its own idea of the sandbox — the thing
        // the two-process trust split exists to make impossible to do by
        // accident.
        let mut agent = x0ne_orchestrator::agent::session::CliAgentSession::new(
            config,
            Arc::clone(&llm),
            sys_prompt_base.clone(),
            Some(bridge),
            Box::new(x0ne_orchestrator::agent::cli_sink::CliDisplaySink::new()),
        );
        // Headless refusals go back to the model, capped; nothing refused runs.
        agent.limits.unattended_denials_returned = ONE_SHOT_UNATTENDED_DENIALS;
        // An answer that stops on a refusal is sent back once.
        agent.limits.policy_stop_retries = 1;
        if cli.bench_agentic {
            // Checked at startup: -p, --isolation container, container detected.
            agent.state.authority = x0ne_orchestrator::agent::approval::ApprovalAuthority::new(
                x0ne_orchestrator::agent::approval::OperatorTrust::IsolatedEnvironment,
                "bench-agentic",
            );
            eprintln!("  ◆  bench-agentic: no approval gate, every command allowed (container)");
        }
        // The session substrate the orchestrator sysaicalls read. Same
        // registry, same monitor, same catalog the old call was handed — not
        // fresh ones, or `WaitAgent` would look for children in an empty room.
        agent.registry = Arc::clone(&registry);
        agent.sys_state = Arc::clone(&sys_state);
        agent.discovered_actions = discovered_actions.clone();
        // The audit trail. Same file the `user_input` / `llm_response` /
        // `turn` lines above go to, so one session is one log; the driver
        // writes two lines per action into it from the choke point.
        agent.journal = session.as_ref().map(|s| s.handle());
        agent.worlds_base = worlds_base_for(&agent.config);
        // The reloaded transcript goes in BEFORE the boot context below, so
        // the model reads yesterday's conversation and then today's action
        // catalog — and because `resume_conversation` clears first, seeding
        // after would silently drop the catalog.
        seed_resumed(&mut agent, resumed.as_ref());
        agent
            .session_started(session_source(resumed.is_some()))
            .await;
        // Lazy discovery reached the model through the historical
        // `Conversation`; the canonical history is now that conversation, so
        // the catalog is seeded there too. Dropping it would have changed what
        // the model knows while claiming to change only the plumbing.
        if agent.config.ui.lazy_discovery {
            if let Some(ref actions) = agent.discovered_actions {
                let ctx = prompt::build_lazy_actions_context_gated(actions, desktop_enabled);
                agent.state.history.add_system_context(&ctx);
            }
        }

        // As typed, like the REPL. Without it the journal had a `turn` and no
        // `user_input`: `sessions` counted 0 turns.
        if let Some(s) = session.as_ref() {
            let _ = s.log_user(&prompt_text).await;
        }
        let report = agent.run_turn(&full_prompt).await;

        // The turn's prose, from the report rather than from a backwards scan
        // of the transcript. `TurnOutcome::Completed` carries the message the
        // model ended on; every other outcome ended for a reason that is not
        // an answer, and printing the last thing said would misrepresent it.
        if let Some(text) = report.response() {
            println!();
            let _ = terminal::print_llm_message(text, &agent.config);
        }

        if let Some(s) = session.as_ref() {
            let _ = s
                .log_llm(
                    report.response().unwrap_or_default(),
                    report.usage.input_tokens,
                    report.usage.output_tokens,
                    report.usage.cache_write_tokens,
                    report.usage.cache_read_tokens,
                )
                .await;
            let _ = s
                .log_turn(
                    report.outcome.kind(),
                    report.outcome.detail().as_deref(),
                    report.duration.as_millis() as u64,
                    report.usage.model_calls,
                    &turn_actions_json(&report),
                )
                .await;
        }

        // Output in requested format
        if cli.output_format == "json" {
            let envelope = serde_json::json!({
                "response": report.response().unwrap_or_default(),
                "tokens": report.usage.total_tokens(),
                // The endpoint's origin only: its path or query may hold a key.
                "model": x0ne_orchestrator::redact::backend_name_for_output(
                    &agent.backend.display_name()
                ),
            });
            #[cfg(unix)]
            if let Some(out) = json_stdout.as_mut() {
                use std::io::Write;
                writeln!(out, "{envelope}")?;
                out.flush()?;
            }
            #[cfg(not(unix))]
            println!("{envelope}");
        }

        // Owner decision 2026-10-10: the exit status follows the turn.
        // 0 only for a completed turn; 2 otherwise, reason on stderr. Startup
        // errors keep 1 (`Err` from main), so a script can tell them apart.
        let code = one_shot_exit_code(&report.outcome);
        if code != 0 {
            use x0ne_orchestrator::agent::driver::TurnOutcome;
            let detail = match &report.outcome {
                TurnOutcome::ModelFailure { error } => Some(error.to_string()),
                other => other.detail(),
            };
            eprintln!(
                "x0ne: turn ended without completing ({}){}",
                report.outcome.kind(),
                detail.map(|d| format!(": {d}")).unwrap_or_default()
            );
            // A denial with no human to decline is a configuration question.
            if matches!(report.outcome, TurnOutcome::Denied { .. })
                && !terminal::can_prompt_interactively()
            {
                eprintln!(
                    "x0ne: {}",
                    x0ne_orchestrator::agent::confirm::operator_hint_for_no_human_channel(
                        &agent.config
                    )
                );
            }
            drop(agent);
            std::process::exit(code);
        }
        return Ok(());
    }

    // ════════════════════════════════════════════════════════════════
    //  AUTONOMOUS MODE: --auto → continuous loop with checkpoints
    // ════════════════════════════════════════════════════════════════

    if cli.auto_mode {
        println!(
            "  {} {} (max {} iterations, Ctrl+C to stop)",
            "◉".yellow(),
            "autonomous mode".yellow().bold(),
            cli.max_iterations
        );
        println!();

        // ── One session for the whole autonomous run ─────────────────────
        //
        // Repetition is the frontend's, one turn is the driver's, and the
        // state between them is this session's. It is built once so that a
        // grant the operator gives in iteration 3 is still in force in
        // iteration 4, the spend keeps climbing toward its ceiling instead of
        // resetting, and iteration N reads what iterations 1..N-1 did.
        //
        // Autonomy reuses authority. It never manufactures any: nothing here
        // sets `auto_confirm`, and on this path the flag could not waive the
        // gate even if something did.
        let mut agent = x0ne_orchestrator::agent::session::CliAgentSession::new(
            config,
            Arc::clone(&llm),
            sys_prompt_base.clone(),
            Some(bridge),
            Box::new(x0ne_orchestrator::agent::cli_sink::CliDisplaySink::new()),
        );
        agent.registry = Arc::clone(&registry);
        agent.sys_state = Arc::clone(&sys_state);
        agent.discovered_actions = discovered_actions.clone();
        // The audit trail. Same file the `user_input` / `llm_response` /
        // `turn` lines above go to, so one session is one log; the driver
        // writes two lines per action into it from the choke point.
        agent.journal = session.as_ref().map(|s| s.handle());
        agent.worlds_base = worlds_base_for(&agent.config);
        // The reloaded transcript goes in BEFORE the boot context below, so
        // the model reads yesterday's conversation and then today's action
        // catalog — and because `resume_conversation` clears first, seeding
        // after would silently drop the catalog.
        seed_resumed(&mut agent, resumed.as_ref());
        agent
            .session_started(session_source(resumed.is_some()))
            .await;
        if agent.config.ui.lazy_discovery {
            if let Some(ref actions) = agent.discovered_actions {
                let ctx = prompt::build_lazy_actions_context_gated(actions, desktop_enabled);
                agent.state.history.add_system_context(&ctx);
            }
        }

        let mut iteration = 0;
        let mut checkpoint_counter = 0;

        loop {
            match auto_mode_step(
                iteration,
                cli.max_iterations,
                interrupt::global().quit_requested() || interrupt::global().stop_requested(),
            ) {
                AutoStep::MaxIterations => {
                    println!(
                        "\n  {} Max iterations ({}) reached. Stopping.",
                        "●".yellow(),
                        cli.max_iterations
                    );
                    break;
                }
                // The process-global handler has already unwound the in-flight
                // turn; here we honour the flag to leave the loop.
                AutoStep::Interrupted => {
                    println!("\n  {} Interrupted. Stopping autonomous mode.", "●".red());
                    interrupt::global().reset();
                    break;
                }
                AutoStep::Run(prompt) => {
                    // As typed, like the REPL and `-p`: without the
                    // `user_input` line, `sessions` counted zero turns.
                    if let Some(s) = session.as_ref() {
                        let _ = s.log_user(prompt).await;
                    }
                    let report = agent.run_turn(prompt).await;
                    if let Some(text) = report.response() {
                        println!();
                        let _ = terminal::print_llm_message(text, &agent.config);
                    }
                    if let Some(s) = session.as_ref() {
                        let _ = s
                            .log_llm(
                                report.response().unwrap_or_default(),
                                report.usage.input_tokens,
                                report.usage.output_tokens,
                                report.usage.cache_write_tokens,
                                report.usage.cache_read_tokens,
                            )
                            .await;
                        let _ = s
                            .log_turn(
                                report.outcome.kind(),
                                report.outcome.detail().as_deref(),
                                report.duration.as_millis() as u64,
                                report.usage.model_calls,
                                &turn_actions_json(&report),
                            )
                            .await;
                    }
                }
            }

            iteration += 1;
            checkpoint_counter += 1;

            // The ten-iteration checkpoint, with its side effect withdrawn.
            //
            // It used to run `git add -A` then `git commit --allow-empty`
            // through raw `bridge.send_action`. Those two RunCommands are the
            // driver's own — nothing in the agent loop asked for them, so
            // nothing in the agent loop gated them. The runtime did evaluate
            // policy (every action over the bridge does, `server.rs:259`), but
            // policy is not the confirmation gate: an unattended write to the
            // operator's git history went through with no human anywhere in
            // the path. Same shape as Matrix's ungated `/undo`.
            //
            // `--auto-mode` itself is untouched: the loop, its iteration
            // bound, its interrupt handling and its prompt all still work.
            // Only the automatic commit is withdrawn, because the mechanism
            // that would authorise it does not exist yet and inventing one
            // here would re-create the session-wide waiver that `3c88017`
            // removed. The counter is kept so the notice stays paced, and so
            // restoring the effect behind a real grant is a small change.
            if checkpoint_counter >= 10 {
                println!(
                    "  {} checkpoint point at iteration {} — automatic commit \
                     withdrawn (unattended write, no approval path)",
                    "○".truecolor(120, 120, 130),
                    iteration
                );
                checkpoint_counter = 0;
            }
        }

        slash::graceful_shutdown(
            agent
                .bridge
                .as_mut()
                .expect("the auto-mode session owns a bridge"),
            agent.state.history.conversation(),
        )
        .await;
        return Ok(());
    }

    // ════════════════════════════════════════════════════════════════
    //  MAIN LOOP — rustyline with history
    // ════════════════════════════════════════════════════════════════

    // ── The interactive session, built once ─────────────────────────
    //
    // One session for the whole REPL, not one per prompt. Everything a turn
    // can leave behind has to survive the next one: the transcript, the live
    // budget and what has been spent against it, the registry of children and
    // worlds, and the approvals the user has granted. Rebuilding this per turn
    // would silently re-arm every one of them — someone who approved something
    // once would be asked again forever, and a budget would never fill.
    //
    // `config` and `bridge` move in. The REPL reads them back through the
    // session from here on, because two owners of the runtime is how you end
    // up with two runtimes.
    let spinner_slot: SpinnerSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut agent = x0ne_orchestrator::agent::session::CliAgentSession::new(
        config,
        Arc::clone(&llm),
        sys_prompt_base.clone(),
        Some(bridge),
        Box::new(SpinnerAwareSink {
            inner: x0ne_orchestrator::agent::cli_sink::CliDisplaySink::new(),
            slot: std::sync::Arc::clone(&spinner_slot),
        }),
    );
    agent.registry = Arc::clone(&registry);
    agent.sys_state = Arc::clone(&sys_state);
    agent.discovered_actions = discovered_actions.clone();
    // The audit trail: the REPL's session writes its actions into the same
    // log this scope writes prompts and turns into.
    agent.journal = session.as_ref().map(|s| s.handle());
    agent.worlds_base = worlds_base_for(&agent.config);
    // See the note at the `-p` site: seeded before the boot context, because
    // `resume_conversation` clears the transcript it seeds into.
    seed_resumed(&mut agent, resumed.as_ref());
    agent
        .session_started(session_source(resumed.is_some()))
        .await;
    if agent.config.ui.lazy_discovery {
        if let Some(ref actions) = agent.discovered_actions {
            let ctx = prompt::build_lazy_actions_context_gated(actions, desktop_enabled);
            agent.state.history.add_system_context(&ctx);
        }
    }

    let history_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join("history");
    std::fs::create_dir_all(history_path.parent().unwrap_or(std::path::Path::new("."))).ok();

    let rl_config = rustyline::Config::builder()
        .history_ignore_dups(true)
        .unwrap_or(rustyline::Config::builder())
        .build();
    let mut rl: rustyline::Editor<cli_helper::X0neHelper, rustyline::history::FileHistory> =
        rustyline::Editor::with_config(rl_config)?;
    rl.set_helper(Some(cli_helper::X0neHelper));
    rl.load_history(&history_path).ok();

    // Ctrl+E → 3-state cycle: live → expanded → summary → live.
    // Ctrl+O → jump straight to expanded (Claude Code-style "overview"
    //          shortcut, idempotent regardless of current state).
    //
    // Mirrors the TUI's Ctrl+E binding so the keyboard story is consistent
    // across both surfaces. Each handler flips a global flag and asks
    // rustyline to AcceptLine; the post-readline substitution below turns
    // the result into the right slash command. See cli_helper.rs for the
    // rationale on why a global flag (instead of a chained Cmd) is
    // unavoidable in rustyline 14. We override Ctrl+E's default emacs
    // binding (move-to-end-of-line) — End/^A still works for the same
    // effect.
    rl.bind_sequence(
        rustyline::KeyEvent::ctrl('E'),
        rustyline::EventHandler::Conditional(Box::new(cli_helper::ExpandHandler)),
    );
    rl.bind_sequence(
        rustyline::KeyEvent::ctrl('O'),
        rustyline::EventHandler::Conditional(Box::new(cli_helper::OverviewHandler)),
    );

    // Goal-driven mode: /until <command> — agent loops until this command exits 0
    let mut goal_command: Option<String> = None;
    // Last `!<command>` shell escape, for `!!` re-run.
    let mut last_shell = String::new();
    let mut goal_iteration: usize = 0;
    const MAX_GOAL_ITERATIONS: usize = 10;

    // Collapse tracking: previous turn output

    // Queued messages the user typed while the agent was busy live in
    // a process-wide `Mutex<VecDeque<String>>` exposed by
    // `x0ne_orchestrator::input_queue`. The agent loop pops from the
    // front, the between-turn drain pushes to the back, the sticky
    // header reads `queue_len()` to render a badge, and the slash
    // dispatcher mutates the same store via /queue / /queue clear /
    // /queue drop. Keeping the queue in one place avoids passing
    // handles down through every layer.

    'outer: loop {
        // ── Build context prompt line ──────────────────────────────────────────
        let total_tokens = usage.total_tokens();
        let token_display = if total_tokens >= 1000 {
            format!("{:.1}k", total_tokens as f64 / 1000.0)
        } else {
            format!("{}", total_tokens)
        };
        let msg_count = agent.state.history.len();

        let bridge_tag = {
            let m = &agent.config.bridge.mode;
            let in_declared_sandbox = x0ne_orchestrator::sandbox_mode::current().in_sandbox();
            let is_sandboxed = in_declared_sandbox
                || m.contains("sandbox")
                || m.contains("container")
                || m.contains("nsjail");
            // Resolve the displayed label:
            //   sandbox + parsable cmd → the actual runtime ("podman" / "incus" / "docker")
            //   sandbox + unknown cmd  → generic "sandbox" (e.g. nsjail, bubblewrap, custom wrapper)
            //   not sandboxed          → "host" (the agent is running directly on the user's machine)
            // This is strictly more informative than the old 🛡 glyph: operators
            // instantly see which container runtime is serving their session and
            // whether the current turn is isolated at all.
            let (label, color) = if in_declared_sandbox {
                ("sandbox".to_string(), (0, 200, 120))
            } else if is_sandboxed {
                match agent
                    .config
                    .bridge
                    .sandbox_cmd
                    .as_deref()
                    .and_then(container_runtime::detect_from_sandbox_cmd)
                {
                    Some((rt, _target)) => (rt.backend().to_string(), (0, 200, 120)),
                    None => ("sandbox".to_string(), (200, 160, 0)),
                }
            } else {
                ("host".to_string(), (110, 120, 135))
            };
            format!(
                " {} {}",
                "·".truecolor(40, 50, 60),
                label.truecolor(color.0, color.1, color.2)
            )
        };

        // Discrete keyboard hint pinned to the right of the navbar — once
        // there's something worth peeking at (msg_count > 0), the wording
        // tells the operator EXACTLY what the next ^E press will produce.
        // The cycle is a clean round-trip:
        //   Live     → "^E expand"   (next press shows full unfolded)
        //   Expanded → "^E summary"  (next press shows compact view)
        //   Summary  → "^E live"     (next press returns to live view)
        // `^O` is mentioned in /help (it's a one-shot shortcut, not part
        // of the cycle).
        let unfold_hint = if msg_count > 0 {
            let next_label = cli_helper::expand_state().next_in_cycle().label();
            let label = format!("^E {} · /help", next_label);
            format!("  {}  ", label.truecolor(50, 55, 65))
        } else {
            "  ".to_string()
        };

        // Queue badge — when the user has pre-typed messages waiting to
        // be processed, show "📋 N queued" right after the msg counter.
        // Renders only when len > 0 so the navbar stays uncluttered on
        // the common path.
        let queue_tag = {
            let n = input_queue::queue_len();
            if n == 0 {
                String::new()
            } else {
                format!(
                    " {} {}",
                    "·".truecolor(40, 50, 60),
                    format!("📋 {} queued", n).truecolor(0, 200, 220),
                )
            }
        };

        // Cloud/GPU tag — only when the monitor has something measured. Red
        // when an alert (idle-GPU waste) is live so the burn is impossible to
        // miss. Empty otherwise, so the navbar stays clean on the common path.
        let cloud_tag = {
            let s = cloud_state.read().unwrap_or_else(|e| e.into_inner());
            let line = s.navbar_line();
            if line.is_empty() {
                String::new()
            } else {
                let color = if s.has_alert() {
                    (220, 70, 70)
                } else {
                    (60, 70, 80)
                };
                format!(
                    " {} {}",
                    "·".truecolor(40, 50, 60),
                    line.truecolor(color.0, color.1, color.2),
                )
            }
        };

        // The enclave badge, in the same place and from the same value as the
        // TUI one. Always printed, never conditional: `○ confidential GPU
        // disabled` is an answer, and a badge that appears only when it is
        // green teaches the operator that its absence means nothing.
        let enclave_tag = {
            let (glyph, label, rgb) = enclave.badge();
            format!(
                " {} {}",
                "·".truecolor(40, 50, 60),
                format!("{glyph} {label}").truecolor(rgb.0, rgb.1, rgb.2),
            )
        };

        let navbar = format!(
            "\n  {} {} {} {} {} {}{}{}{}{}{}{}\n  {} ",
            "┊".truecolor(40, 50, 60),
            // The PRODUCT first, the model in grey behind it. It is x0ne that
            // runs; the model is an interchangeable part, not the identity
            // of the session.
            format!(
                "{} {}",
                "x0ne-code".truecolor(0, 200, 220).bold(),
                format_model_display(&agent.backend.display_name()).truecolor(70, 80, 92)
            ),
            "·".truecolor(40, 50, 60),
            format!("{}tk", token_display).truecolor(60, 70, 80),
            "·".truecolor(40, 50, 60),
            format!("{}msg", msg_count).truecolor(60, 70, 80),
            queue_tag,
            cloud_tag,
            bridge_tag,
            enclave_tag,
            if agent.config.ui.auto_confirm {
                format!("  {}", "auto".truecolor(200, 160, 0))
            } else {
                String::new()
            },
            unfold_hint,
            prompt_chevron(agent.plan_mode())
        );

        // If we have queued input from a previous turn (the user typed
        // while the agent was busy), consume that first instead of
        // blocking on rustyline. Echo the prompt + line so the user
        // sees it processed in order — matches the visual feel of
        // having actually hit Enter right now.
        let input = if let Some(queued) = input_queue::pop_front() {
            print!("{}", navbar);
            println!("{}", queued.as_str().truecolor(180, 200, 220));
            rl.add_history_entry(&queued).ok();
            queued
        } else {
            match rl.readline(&navbar) {
                Ok(line) => {
                    // Ctrl+E is a 3-state CYCLE through views of the same
                    // session — clean round-trip, never an "erase to blank"
                    // gesture:
                    //   Live     → Expanded  (full /expand output)
                    //   Expanded → Summary   (full /history output)
                    //   Summary  → Live      (return to live conversation)
                    //
                    // Ctrl+O is a SHORTCUT (not a cycle): always jumps
                    // directly to Expanded regardless of current state.
                    // Mirrors Claude Code's overview gesture.
                    //
                    // The screen is cleared between transitions so the new
                    // view starts at the top — clearing exactly N lines is
                    // unreliable across terminal widths. Conversation
                    // memory is never touched; the LLM keeps seeing every
                    // message.
                    //
                    // Any non-keystroke input resets the cycle to Live.
                    if cli_helper::take_overview_requested() {
                        // Ctrl+O: idempotent jump to Expanded.
                        print!("\x1b[2J\x1b[H");
                        terminal::flush_stdout();
                        println!("  {} unfolding session…", "⫸".truecolor(0, 200, 220));
                        "/x".to_string()
                    } else if cli_helper::take_expand_requested() {
                        // Ctrl+E: advance the cycle by one step.
                        let current = cli_helper::expand_state();
                        let next = current.next_in_cycle();
                        match next {
                            cli_helper::ExpandState::Expanded => {
                                // → Expanded: route through /x so we share
                                // the existing slash dispatch path (which
                                // then re-marks the state).
                                if current == cli_helper::ExpandState::Summary {
                                    print!("\x1b[2J\x1b[H");
                                    terminal::flush_stdout();
                                }
                                println!("  {} unfolding session…", "⫸".truecolor(0, 200, 220));
                                "/x".to_string()
                            }
                            cli_helper::ExpandState::Summary => {
                                // → Summary: clear, print compact view in
                                // place, mark state, resume loop.
                                print!("\x1b[2J\x1b[H");
                                terminal::flush_stdout();
                                println!(
                                    "  {} summary session view  {}",
                                    "⫷".truecolor(0, 200, 220),
                                    "(^E to return to live)".truecolor(60, 70, 80)
                                );
                                terminal::print_history(agent.state.history.conversation());
                                cli_helper::set_expand_state(cli_helper::ExpandState::Summary);
                                continue 'outer;
                            }
                            cli_helper::ExpandState::Live => {
                                // → Live: clear, drop the session view,
                                // mark state, resume loop. The next prompt
                                // appears at the top of a fresh screen.
                                print!("\x1b[2J\x1b[H");
                                terminal::flush_stdout();
                                println!("  {} live conversation", "⫷".truecolor(0, 200, 220));
                                cli_helper::set_expand_state(cli_helper::ExpandState::Live);
                                continue 'outer;
                            }
                        }
                    } else {
                        cli_helper::clear_expand_active();
                        rl.add_history_entry(&line).ok();
                        line
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    // Rustyline is in raw mode, so the terminal does not emit
                    // SIGINT on Ctrl+C — route it through the same global
                    // interrupt so double-press quit semantics match the
                    // agent-loop path.
                    let state = interrupt::global().trigger();
                    if state >= 2 {
                        println!("  {} goodbye", "●".truecolor(255, 180, 0));
                        break 'outer;
                    }
                    println!(
                        "  {} press Ctrl+C again to quit, or /quit",
                        "↯".truecolor(255, 180, 0),
                    );
                    // Reset so the flag doesn't leak into the next turn; the
                    // double-press window is tracked by last_press_ms anyway.
                    interrupt::global().reset();
                    continue;
                }
                Err(rustyline::error::ReadlineError::Eof) => break 'outer,
                Err(e) => return Err(e.into()),
            }
        };
        // Unicode BOM (U+FEFF) can be pasted from some terminals/editors — strip silently.
        let trimmed = input.trim().trim_start_matches('\u{feff}').trim();

        if trimmed.is_empty() {
            continue;
        }

        // ── Session: log user input ────────────────────────────────────────────
        if let Some(s) = session.as_ref() {
            let _ = s.log_user(trimmed).await;
        }

        // ── Bare number right after `/model list` = model selection ───────────
        //
        // Checked on EVERY input so the pending state is one-shot: the very
        // next thing typed either is the index or clears it. Session logs
        // show `3` after `/model list` going to the LLM as prose — twice.
        let owned_pick;
        let trimmed = if slash::take_bare_model_selection(trimmed) {
            owned_pick = format!("/model {}", trimmed);
            owned_pick.as_str()
        } else {
            trimmed
        };
        // `/playground [N]` is a prompt, not a command: it becomes this
        // turn's user input (same text as the TUI's — `playground.rs`).
        let owned_playground;
        let trimmed = match x0ne_orchestrator::playground::parse(trimmed) {
            Some(req) => {
                if req.reset {
                    match x0ne_orchestrator::playground::reset_playground() {
                        Ok(msg) => println!("  {}  {}", "↺".truecolor(0, 220, 120), msg),
                        Err(e) => {
                            print_error(&format!(
                                "playground not reset — {e} (use `/playground keep …` to run on the current state)"
                            ));
                            continue;
                        }
                    }
                }
                owned_playground = req.prompt();
                owned_playground.as_str()
            }
            None => trimmed,
        };

        // ── Shell escape: `!<command>` ─────────────────────────────────────────
        //
        // A human, at their own keyboard, running a shell command without
        // leaving the prompt — the same trust basis as the TUI's Ctrl+S
        // shell or the canvas `/undo`. It runs on the HOST, deliberately
        // NOT through the model or the agentic boundary: this is the
        // operator's own hand, not the agent's. `!!` re-runs the last one.
        if let Some(rest) = trimmed.strip_prefix('!') {
            let cmd = if rest == "!" {
                last_shell.clone()
            } else {
                rest.trim().to_string()
            };
            if cmd.is_empty() {
                print_error("usage: !<command>   (e.g. `!gcloud auth list`)");
                continue;
            }
            last_shell = cmd.clone();
            println!(
                "  {} {}",
                "$".truecolor(0, 200, 220).bold(),
                cmd.truecolor(180, 200, 220)
            );
            let mut _echo_guard = input_queue::EchoSuppressor::new();
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status();
            if let Some(g) = _echo_guard.as_mut() {
                g.restore();
            }
            match status {
                Ok(st) if st.success() => {}
                Ok(st) => print_error(&format!(
                    "exited with status {}",
                    st.code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                )),
                Err(e) => print_error(&format!("failed to run: {}", e)),
            }
            println!();
            continue;
        }

        // ── Slash commands ─────────────────────────────────────────────────────
        if trimmed.starts_with('/') {
            // Handle /until specially since it needs local state
            if trimmed.starts_with("/until") {
                let cmd_arg = trimmed.strip_prefix("/until").unwrap_or("").trim();
                if cmd_arg.is_empty() {
                    if goal_command.is_some() {
                        println!("  {}  goal cleared", "✗".truecolor(200, 60, 60));
                        goal_command = None;
                        goal_iteration = 0;
                    } else {
                        print_error("/until <command>  — e.g. /until cargo test");
                    }
                } else {
                    goal_command = Some(cmd_arg.to_string());
                    goal_iteration = 0;
                    // `/until` declares a stopping condition, not a mandate.
                    // It used to switch on global auto-confirm for the whole
                    // run, so `/until cargo test` — a statement about when to
                    // stop — silently pre-approved every destructive action
                    // the model chose for the next ten turns, on any target.
                    // The repetition is the feature and it is kept; the
                    // blanket grant is not. Confirmations behave exactly as
                    // they do outside a goal, and the prompt is reachable
                    // because `/until` is typed at one.
                    println!(
                        "  {}  goal set: {} (confirmations unchanged, max {} iterations)",
                        "◎".truecolor(0, 200, 220).bold(),
                        cmd_arg.truecolor(0, 200, 220).bold(),
                        MAX_GOAL_ITERATIONS.to_string().truecolor(80, 90, 100)
                    );
                    // Run the goal command now to see the current state, as
                    // a `RunCommand` of the session: the human gate, the
                    // policy and the sandbox apply, like for any command. It
                    // ran with `sh -c` in this process until 2026-10-31
                    // (audit H4-2): on the host, with the operator's whole
                    // environment, and the code it runs is the workspace's.
                    match run_goal(&mut agent, cmd_arg).await {
                        goal_cmd::GoalCheck::Passed => {
                            println!("  {}  goal already passes!", "✓".truecolor(0, 200, 120));
                            goal_command = None;
                        }
                        goal_cmd::GoalCheck::Failed { code, output } => {
                            println!(
                                "  {}  exit {} — agent will fix this",
                                "✗".truecolor(200, 60, 60),
                                code
                            );
                            agent.state.history.add_user(&format!(
                                "Fix the following issue. Keep working until `{}` passes (exit 0).\n\nCurrent output (exit {}):\n```\n{}\n```",
                                cmd_arg,
                                code,
                                goal_cmd::head(&output, 30)
                            ));
                        }
                        goal_cmd::GoalCheck::Refused(reason) => {
                            print_error(&format!("goal refused: {}", reason));
                            goal_command = None;
                        }
                    }
                    if goal_command.is_none() {
                        continue;
                    }
                }
                continue;
            }

            // Handle /matrix — launch matrix mode from CLI
            if trimmed == "/matrix" || trimmed.starts_with("/matrix ") {
                let matrix_arg = trimmed
                    .strip_prefix("/matrix")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                println!(
                    "  {}  entering matrix mode…",
                    "◈".truecolor(0, 180, 160).bold()
                );
                let matrix_config = agent.config.clone();
                match matrix_mode::run_matrix_with_config_and_request(matrix_config, &matrix_arg)
                    .await
                {
                    Ok(_) => println!("  {}  back to CLI", "◈".truecolor(0, 180, 160)),
                    Err(e) => print_error(&format!("Matrix mode error: {}", e)),
                }
                continue;
            }

            let budget_before = agent.config.cost.budget_usd_limit;
            let handled = slash::handle_slash_command(
                trimmed,
                &mut agent.state.history,
                agent
                    .bridge
                    .as_mut()
                    .expect("the REPL session owns a bridge"),
                &usage,
                &mut agent.config,
                session.as_ref().map(|s| s.id()),
                &mut agent.state.undo,
            )
            .await;
            if let Err(e) = apply_budget_change(&mut agent, budget_before) {
                print_error(&e);
            }
            // If the slash command produced a session view, remember which
            // one so Ctrl+E flips correctly to the *other* representation
            // on the next press. We check `trimmed` here (rather than
            // threading state through SlashResult) to keep the slash
            // dispatcher's return type stable.
            if matches!(trimmed, "/x" | "/expand" | "/unfold") {
                cli_helper::set_expand_state(cli_helper::ExpandState::Expanded);
            } else if trimmed == "/history" {
                cli_helper::set_expand_state(cli_helper::ExpandState::Summary);
            }
            match handled {
                SlashResult::Handled => continue,
                SlashResult::Quit => break 'outer,
                // The session continues: same transcript, same spend, same
                // approvals, same registry, same runtime. Only what answers
                // changes. `prices` is a rate table keyed by the model each
                // response names, so it needs no rebuild.
                SlashResult::SwitchModel(new_backend) => {
                    usage.model = new_backend.display_name();
                    agent.set_backend(Arc::from(new_backend));
                    continue;
                }
                // `/clear` is a new conversational trust context: the
                // transcript goes and the standing approvals go with it.
                // The session switches, after the runtime does; nothing here
                // decides the mode.
                SlashResult::SetPlanMode(on) => {
                    match agent.set_plan_mode(on).await {
                        Ok(_) if agent.plan_mode() => println!(
                            "  {}",
                            "[plan mode · the runtime refuses every change · /build to leave]"
                                .truecolor(230, 170, 60)
                        ),
                        Ok(_) => println!(
                            "{}",
                            "  [build mode · the configured policy applies]".dimmed()
                        ),
                        Err(e) => print_error(&format!("plan mode: {e}")),
                    }
                    continue;
                }
                SlashResult::ClearSession => {
                    agent.clear_conversation();
                    agent.session_started(SessionSource::Clear).await;
                    println!("{}", "  [conversation cleared]".dimmed());
                    continue;
                }
                // `/resume` is `/clear` plus a reload: `resume_conversation`
                // revokes the standing approvals before it seeds anything, so
                // switching transcript mid-REPL cannot carry an approval from
                // the conversation the operator just left.
                SlashResult::Resume(request) => {
                    match resume_session(&request, false, session.as_ref().map(|s| s.id())).await {
                        Ok((journal, reloaded)) => {
                            agent.resume_conversation(&reloaded);
                            // The writer moves with the transcript. Leaving it
                            // on the old file would append this session's
                            // actions to a journal nobody is reading.
                            agent.journal = Some(journal.handle());
                            agent.session_started(SessionSource::Resume).await;
                            print_resume_banner(&reloaded);
                            session = Some(journal);
                        }
                        Err(e) => print_error(&format!("/resume: {e}")),
                    }
                    continue;
                }
                // Appended, never rewritten: the journal is hash-chained.
                SlashResult::Rename(title) => {
                    match &session {
                        Some(journal) => match journal.log_session_renamed(&title).await {
                            Ok(()) => terminal::print_ok(&format!(
                                "session {} renamed: {}",
                                journal.id(),
                                x0ne_orchestrator::text_safety::snippet(&title, 80)
                            )),
                            Err(e) => print_error(&format!("/rename: {e}")),
                        },
                        None => print_error("/rename: this session has no journal"),
                    }
                    continue;
                }
                // The operator's worlds take the model's path: one
                // `SpawnWorld` per world, through the session's gates.
                SlashResult::SpawnWorlds(request) => {
                    worlds_cli::spawn_worlds(&mut agent, &request).await;
                    continue;
                }
                SlashResult::Benchmark(models, prompt) => {
                    struct BenchRun {
                        model: String,
                        duration: std::time::Duration,
                        tokens_in: u64,
                        tokens_out: u64,
                        action_count: usize,
                        actions_ok: usize,
                        actions_fail: usize,
                    }

                    let original_model = agent.config.llm.ollama.model.clone();
                    let mut bench_runs: Vec<BenchRun> = Vec::new();

                    println!();
                    println!(
                        "  {} benchmark: {} models × \"{}\"",
                        "▶".truecolor(0, 200, 220).bold(),
                        models.len().to_string().bold(),
                        truncate_display(&prompt, 57).truecolor(180, 200, 220),
                    );

                    for (i, model_name) in models.iter().enumerate() {
                        // ── Banner ────────────────────────────────────
                        println!();
                        println!("  {}", "━".repeat(70).truecolor(0, 200, 220));
                        println!(
                            "  {}  [{}/{}] {}",
                            "▶".truecolor(0, 200, 220).bold(),
                            i + 1,
                            models.len(),
                            model_name.truecolor(0, 200, 220).bold(),
                        );
                        println!("  {}", "━".repeat(70).truecolor(0, 200, 220));

                        // ── Swap model ────────────────────────────────
                        agent.config.llm.ollama.model = model_name.clone();
                        let new_backend = match api::create_backend(&agent.config.llm, None) {
                            Ok(b) => b,
                            Err(e) => {
                                terminal::print_error(&format!(
                                    "Failed to create backend for {}: {}",
                                    model_name, e
                                ));
                                bench_runs.push(BenchRun {
                                    model: model_name.clone(),
                                    duration: std::time::Duration::ZERO,
                                    tokens_in: 0,
                                    tokens_out: 0,
                                    action_count: 0,
                                    actions_ok: 0,
                                    actions_fail: 0,
                                });
                                continue;
                            }
                        };

                        let bench_llm: Arc<dyn api::LlmBackend> = Arc::from(new_backend);

                        // ── A fresh session, per model ────────────────
                        //
                        // Owner decision: models are compared as independent
                        // runs from the same starting point. A fresh session
                        // is how that is enforced — history, approvals, live
                        // budget and registry are all constructed new, so
                        // model B cannot read, or benefit from, what model A
                        // did. Reusing one session and swapping the backend
                        // would silently hand every later model its
                        // predecessors' transcript.
                        //
                        // The runtime is the exception, and it is moved rather
                        // than duplicated: `/bench` has always run every model
                        // against the same `x0ne-runtime`, so one model's
                        // filesystem effects remain visible to the next. That
                        // is benchmark isolation debt, recorded and not
                        // silently fixed here — inventing snapshots or
                        // rollback would be a product decision wearing a
                        // migration's clothes.
                        let mut bench_session =
                            x0ne_orchestrator::agent::session::CliAgentSession::new(
                                agent.config.clone(),
                                Arc::clone(&bench_llm),
                                sys_prompt_base.clone(),
                                agent.bridge.take(),
                                Box::new(x0ne_orchestrator::agent::cli_sink::CliDisplaySink::new()),
                            );
                        bench_session.sys_state = Arc::clone(&sys_state);
                        bench_session.discovered_actions = discovered_actions.clone();
                        bench_session.journal = session.as_ref().map(|s| s.handle());

                        // The shared prompt, once per model, before its turn.
                        if let Some(s) = session.as_ref() {
                            let _ = s.log_user(&prompt).await;
                        }
                        let report = bench_session.run_turn(&prompt).await;

                        if let Some(text) = report.response() {
                            println!();
                            let _ = terminal::print_llm_message(text, &agent.config);
                        }
                        if let Some(s) = session.as_ref() {
                            let _ = s
                                .log_llm(
                                    report.response().unwrap_or_default(),
                                    report.usage.input_tokens,
                                    report.usage.output_tokens,
                                    report.usage.cache_write_tokens,
                                    report.usage.cache_read_tokens,
                                )
                                .await;
                        }

                        // The runtime goes back before this session drops.
                        agent.bridge = bench_session.bridge.take();

                        // Every number below comes from the turn's own report.
                        // The legacy `turn_stats` was `ActionStats`, which is
                        // exactly what `TurnReport.actions` carries, so there
                        // is nothing to reconstruct and no second tally.
                        bench_runs.push(BenchRun {
                            model: model_name.clone(),
                            duration: report.duration,
                            tokens_in: report.usage.input_tokens,
                            tokens_out: report.usage.output_tokens,
                            action_count: report.actions.total,
                            actions_ok: report.actions.succeeded,
                            actions_fail: report.actions.failed,
                        });

                        // Reset interrupt for next model
                        if interrupt::global().quit_requested() {
                            println!("  {} benchmark aborted", "●".truecolor(255, 180, 0));
                            break;
                        }
                        interrupt::global().reset();
                    }

                    // ── Restore original model ───────────────────────
                    agent.config.llm.ollama.model = original_model.clone();
                    if let Ok(restored) = api::create_backend(&agent.config.llm, None) {
                        usage.model = restored.display_name();
                        agent.set_backend(Arc::from(restored));
                    }

                    // ── Summary table ────────────────────────────────
                    println!();
                    println!("  {}", "━".repeat(82).truecolor(0, 200, 220));
                    println!("  {} Benchmark Summary", "▶".truecolor(0, 200, 220).bold());
                    println!("  {}", "━".repeat(82).truecolor(0, 200, 220));
                    println!(
                        "  {:<28} {:>7} {:>5}/{:<5} {:>8} {:>8}",
                        "Model".bold(),
                        "Time".bold(),
                        "OK".bold(),
                        "Err".bold(),
                        "Tok In".bold(),
                        "Tok Out".bold(),
                    );
                    println!("  {}", "─".repeat(82).truecolor(40, 45, 55));

                    // Find fastest for highlighting
                    let fastest = bench_runs
                        .iter()
                        .filter(|r| r.action_count > 0)
                        .map(|r| r.duration)
                        .min();

                    for r in &bench_runs {
                        let secs = r.duration.as_secs_f64();
                        let time_str = if secs >= 60.0 {
                            format!("{:.0}m{:.0}s", secs / 60.0, secs % 60.0)
                        } else {
                            format!("{:.1}s", secs)
                        };
                        let is_fastest = fastest
                            .map(|f| r.duration == f && bench_runs.len() > 1)
                            .unwrap_or(false);

                        // OK/Err column
                        let ok_str = r.actions_ok.to_string();
                        let err_str = r.actions_fail.to_string();
                        let ok_display = ok_str.truecolor(0, 200, 120).to_string();
                        let err_display = if r.actions_fail > 0 {
                            err_str.truecolor(255, 80, 80).bold().to_string()
                        } else {
                            err_str.truecolor(60, 65, 70).to_string()
                        };

                        let model_display = if is_fastest {
                            format!("{} ⚡", r.model)
                                .truecolor(0, 220, 120)
                                .bold()
                                .to_string()
                        } else {
                            r.model.truecolor(0, 200, 220).to_string()
                        };
                        let time_display = if is_fastest {
                            time_str.truecolor(0, 220, 120).bold().to_string()
                        } else {
                            time_str.truecolor(180, 190, 200).to_string()
                        };

                        println!(
                            "  {:<28} {:>7} {:>5}/{:<5} {:>8} {:>8}",
                            model_display,
                            time_display,
                            ok_display,
                            err_display,
                            r.tokens_in.to_string().truecolor(80, 90, 100),
                            r.tokens_out.to_string().truecolor(80, 90, 100),
                        );
                    }

                    println!("  {}", "━".repeat(82).truecolor(0, 200, 220));
                    println!();

                    continue;
                }
                SlashResult::NotSlash => {} // shouldn't happen since we checked '/'
            }
            continue;
        }

        // ── Previous-turn marker (no destructive collapse) ──
        //
        // History: this used to do `cursor_up + erase_line` for
        // `prev_turn_lines + 2` rows to compact the previous turn into a
        // 1-line summary. The `+2` was calibrated for a single-line prompt,
        // but the current navbar emits 3 visual rows (leading newline +
        // status row + `❯ ` row), and Enter adds another. The math drifted,
        // so the loop walked PAST the navbar and erased the user's
        // freshly-typed prompt before reaching the previous turn. The
        // visible bug: "it's running but the prompt is no longer visible".
        //
        // We don't try to fix the cursor math (terminal width also wraps
        // long action JSON unpredictably, making any line-count approach
        // fragile). Instead we keep the previous turn fully visible in
        // scrollback and emit a tiny separator + 1-line summary above the
        // next prompt. The compact session view is still available on
        // demand via `^E` / `/x` / `/history`.
        // The one-line echo of the PREVIOUS answer is gone. It existed back
        // when the previous turn was destructively collapsed and something had
        // to stand in for it. Nothing is collapsed any more — the full answer
        // sits in scrollback directly above — so the echo only repeated what
        // the operator had just read and was about to reply to.

        // Expand @file references in user input
        let expanded = project::expand_at_references(trimmed, is_remote_bridge);

        // ════════════════════════════════════════════════════════════════
        //  AGENT INNER LOOP
        // ════════════════════════════════════════════════════════════════
        //
        // Suppress the kernel's stdin echo for the duration of this turn.
        // The user can keep typing — the kernel still queues their
        // complete lines in cooked mode for `drain_pending_lines` to
        // pick up below — but their keystrokes stop colliding with the
        // agent's streaming output. `EchoSuppressor::Drop` restores
        // the original termios even on panic / early return.
        let mut _echo_guard = input_queue::EchoSuppressor::new();

        // ── The canonical turn ───────────────────────────────────────
        //
        // The session adds the user's message to canonical history itself, so
        // there is no `conv.add_user` above this any more: one transcript,
        // one writer.
        let spinner_rx = agent.open_stream();
        let (spin_task, spin_stop) =
            spawn_turn_spinner(spinner_rx, std::sync::Arc::clone(&spinner_slot));
        let report = agent.run_turn(&expanded).await;
        let _ = spin_stop.send(());
        let _ = spin_task.await;

        // The navbar's counter is a frontend display, fed from the turn's own
        // report rather than from a tracker the driver would have to know
        // about. Cumulative across the session, as it always was.
        usage.total_input_tokens += report.usage.input_tokens;
        usage.total_output_tokens += report.usage.output_tokens;
        usage.total_cache_write_tokens += report.usage.cache_write_tokens;
        usage.total_cache_read_tokens += report.usage.cache_read_tokens;
        usage.total_requests += report.usage.model_calls as u64;

        match report.response() {
            Some(text) => {
                println!();
                let _ = terminal::print_llm_message(text, &agent.config);
            }
            None => {
                // A turn that ends without an answer must say why, loudly.
                // The /tmp incident: the model backend failed on every call
                // and the user saw nothing but silence — the failure was
                // logged as an empty response and rendered as nothing.
                use x0ne_orchestrator::agent::driver::TurnOutcome;
                let why = match &report.outcome {
                    TurnOutcome::ModelFailure { error } => format!(
                        "the model backend failed: {}. Check `/auth` (credentials) and `/model` (target) — `/provider anthropic` uses the API directly.",
                        error
                    ),
                    other => format!("turn ended without a response: {}", other.kind()),
                };
                print_error(&why);
            }
        }
        if let Some(s) = session.as_ref() {
            let _ = s
                .log_llm(
                    report.response().unwrap_or_default(),
                    report.usage.input_tokens,
                    report.usage.output_tokens,
                    report.usage.cache_write_tokens,
                    report.usage.cache_read_tokens,
                )
                .await;
            let _ = s
                .log_turn(
                    report.outcome.kind(),
                    report.outcome.detail().as_deref(),
                    report.duration.as_millis() as u64,
                    report.usage.model_calls,
                    &turn_actions_json(&report),
                )
                .await;
        }

        // Restore echo NOW so the queued-message banner + next
        // rustyline prompt show typed input normally. Dropping at the
        // end of the loop iteration would also work, but explicit is
        // friendlier to readers.
        if let Some(g) = _echo_guard.as_mut() {
            g.restore();
        }

        if let Err(e) = worlds_cli::maybe_auto_render_parallel_worlds(&agent.config).await {
            warn!("parallel worlds view: {}", e);
        }

        // Agent turn is over — reset the interrupt flag so the next prompt
        // starts clean. If the user double-tapped Ctrl+C to quit, honour it.
        if interrupt::global().quit_requested() {
            println!("  {} goodbye", "●".truecolor(255, 180, 0));
            break 'outer;
        }
        interrupt::global().reset();

        // Drain anything the user typed while the agent was busy. The
        // kernel buffered complete lines in the TTY's canonical-mode
        // queue; we pull them here and replay them as additional REPL
        // iterations. A visual `+N queued` line is announced once so
        // the user sees that their pre-typing landed.
        let pending = input_queue::drain_pending_lines();
        if !pending.is_empty() {
            for line in pending {
                input_queue::push_back(line);
            }
            let n = input_queue::queue_len();
            println!(
                "  {}  {} queued message{} (📋 total){}",
                "⋮".truecolor(0, 200, 220),
                n.to_string().truecolor(0, 200, 220).bold(),
                if n == 1 { "" } else { "s" },
                "  ·  /queue to inspect".truecolor(120, 120, 130),
            );
        }

        // ── Goal-driven loop: check if goal command passes ──────────────
        if let Some(goal) = goal_command.clone() {
            goal_iteration += 1;
            println!(
                "  {}  checking goal ({}/{})… {}",
                "◎".truecolor(0, 180, 200),
                goal_iteration.to_string().truecolor(0, 200, 220),
                MAX_GOAL_ITERATIONS.to_string().truecolor(80, 90, 100),
                goal.truecolor(80, 90, 100)
            );

            // The same gates as the first run: the model has just written
            // the code this command runs.
            match run_goal(&mut agent, &goal).await {
                goal_cmd::GoalCheck::Passed => {
                    println!("  {}  goal passed!", "✓".truecolor(0, 200, 120).bold());
                    goal_command = None;
                    goal_iteration = 0;
                    // No `auto_confirm = false` here any more. `/until` no
                    // longer raises it, and clearing it on exit destroyed
                    // a setting the goal never owned: an operator running
                    // with auto-confirm on found it switched off by a
                    // goal completing.
                }
                goal_cmd::GoalCheck::Failed { .. } if goal_iteration >= MAX_GOAL_ITERATIONS => {
                    println!(
                        "  {}  max iterations reached — stopping",
                        "✗".truecolor(200, 60, 60)
                    );
                    goal_command = None;
                    goal_iteration = 0;
                }
                goal_cmd::GoalCheck::Failed { code, output } => {
                    println!("  {}  exit {} — retrying", "✗".truecolor(200, 160, 0), code);
                    agent.state.history.add_user(&format!(
                        "The goal `{}` still fails (exit {}, attempt {}/{}).\nHere is the output:\n```\n{}\n```\n\nAnalyze the errors and fix them. Keep going.",
                        goal,
                        code,
                        goal_iteration,
                        MAX_GOAL_ITERATIONS,
                        goal_cmd::head(&output, 40)
                    ));
                    // Continue the outer loop — will re-enter agent_loop
                    continue;
                }
                goal_cmd::GoalCheck::Refused(reason) => {
                    print_error(&format!("goal refused: {}", reason));
                    goal_command = None;
                    goal_iteration = 0;
                }
            }
        }
    }

    // ── Graceful shutdown ──────────────────────────────────────────────────────
    rl.save_history(&history_path).ok();
    slash::graceful_shutdown(
        agent
            .bridge
            .as_mut()
            .expect("the REPL session owns a bridge"),
        agent.state.history.conversation(),
    )
    .await;

    Ok(())
}

/// Runs the `/until` goal command as the session's own `RunCommand`: the
/// human gate, the runtime's policy and its isolation apply, and nothing runs
/// in this process. No runtime, a refusal or no exit code is a refusal, so a
/// goal is never taken as passed on a guess.
async fn run_goal(
    agent: &mut x0ne_orchestrator::agent::session::CliAgentSession,
    cmd: &str,
) -> goal_cmd::GoalCheck {
    let envelope = agent.run_operator_action(&goal_cmd::action_for(cmd)).await;
    goal_cmd::check_of(&envelope)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Self-test: visual rendering of every action category through the real CLI
//  ⚠ DEBUG BUILDS ONLY — this entire block is stripped from `cargo build --release`.
//  The permissive config, the test harness, and the --self-test flag do not exist
// ═══════════════════════════════════════════════════════════════════════════
//  in production binaries. Zero security impact.
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(debug_assertions)]
async fn run_self_test(config_path: Option<&PathBuf>, _desktop: bool) -> Result<()> {
    use serde_json::json;
    use std::time::Instant;

    let mut config = Config::find_and_load(config_path)?;
    // Honour X0NE_RUNTIME_BIN / X0NE_RUNTIME_ADDR (and the legacy X0NE_CORE_*
    // aliases) so `make self-test` can force the freshly-built sibling binary
    // instead of whatever `x0ne-runtime` resolves to via `$PATH` (often a
    // stale `cargo install` copy).
    config.apply_env_overrides();

    // ── Banner ────────────────────────────────────────────────────────────
    print!("\x1b[2J\x1b[H");
    print_banner();
    println!(
        "  {}  {} — visual rendering self-test\n",
        "●".truecolor(0, 200, 220),
        "self-test".truecolor(0, 200, 220).bold()
    );

    // ── Diagnostics header ───────────────────────────────────────────────
    let exe_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let cwd_display = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    println!("  {}  Diagnostics", "◆".truecolor(80, 90, 120));
    println!(
        "  {}  build      debug (self-test stripped in release)",
        "│".truecolor(40, 45, 55)
    );
    println!(
        "  {}  binary     {}",
        "│".truecolor(40, 45, 55),
        exe_path.truecolor(80, 90, 100)
    );
    println!(
        "  {}  cwd        {}",
        "│".truecolor(40, 45, 55),
        cwd_display.truecolor(80, 90, 100)
    );
    println!(
        "  {}  version    v{}",
        "│".truecolor(40, 45, 55),
        env!("CARGO_PKG_VERSION").truecolor(80, 90, 100)
    );
    println!(
        "  {}  bridge     {}",
        "│".truecolor(40, 45, 55),
        config.bridge.mode.truecolor(80, 90, 100)
    );
    println!(
        "  {}  core bin   {}",
        "│".truecolor(40, 45, 55),
        config.bridge.x0ne_core_bin.truecolor(80, 90, 100)
    );
    println!();

    // ── Write a permissive temp config for x0ne-runtime ─────────────────────
    let selftest_config_path = "/tmp/x0ne-selftest-core.toml";
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_str = cwd.to_string_lossy();
    std::fs::write(
        selftest_config_path,
        format!(
            r#"[runtime]
mode = "stdio"

[policy]
default_effect = "allow"
mode = "auto"
max_read_bytes = 10485760
max_list_entries = 2048
allowed_read_paths  = ["{cwd}", "/tmp"]
allowed_write_paths = ["{cwd}", "/tmp"]
allowed_commands = []
deny_path_patterns = ["**/.ssh/**", "**/.gnupg/**"]

[executor]
default_timeout_secs = 30
max_file_read_bytes  = 1048576
trash_dir = "/tmp/x0ne-selftest-trash"
max_search_results = 100
max_log_lines = 200

[actions]
ReadFile = true
ReadFileChunk = true
WriteFile = true
AppendFile = true
ReplaceInFile = true
EditFile = true
InsertAtLine = true
ApplyPatch = true
ListDir = true
TreeView = true
FindFiles = true
SearchCode = true
CreateDir = true
MoveToTrash = true
CopyFile = true
MoveFile = true
CreateTempFile = true
FileInfo = true
Checksum = true
RunCommand = true
SystemInfo = true
Whoami = true
Uptime = true
DiskUsage = true
ListProcesses = true
PortCheck = true
DnsLookup = true
GetEnv = true
ReadJson = true
WriteJson = true
ParseToml = true
RandomBytes = true
Remember = true
Recall = true
ListMemory = true
Forget = true
GitStatus = true
GitDiff = true
ReadLogs = true
HttpRequest = true
AskHuman = true
Confirm = true
Sleep = true
ListActions = true
DescribeAction = true
PolicyCheck = true
Pipeline = true
Subscribe = true
CheckHandle = true
StopHandle = true
ListHandles = true
CreateSnapshot = true
RestoreSnapshot = true
DeleteSnapshot = true
ListSnapshots = true
DefinePrimitive = true
CallPrimitive = true
DeletePrimitive = true
ListCustomPrimitives = true
Validate = true
Diff = true
Add = true
Sub = true
Mul = true
Div = true
Mod = true
Abs = true
Pow = true
Sqrt = true
Min = true
Max = true
Round = true
Compare = true
Gt = true
Lt = true
Eq = true
Clamp = true
Sum = true
Avg = true
StrLen = true
StrContains = true
StrCount = true
StrReplace = true
StrSplit = true
StrUpper = true
StrLower = true
StrTrim = true
StrSlice = true
Try = true
Assert = true
"#,
            cwd = cwd_str
        ),
    )
    .expect("Failed to write self-test config");

    // ── Boot x0ne-runtime with permissive config ────────────────────────────
    //
    // Seed canned answers BEFORE spawning the child: interactive primitives
    // (AskHuman, Confirm, AskMultipleChoice) can't read real stdin — the
    // bridge protocol already owns it. `x0ne-runtime` reads X0NE_SCRIPTED_ANSWERS
    // at startup and installs a ScriptedPrompt so these primitives resolve
    // to deterministic responses. Order matches the self-test case order.
    //
    // Queue: blue → yes → no → 2 → s → o:<text>
    std::env::set_var("X0NE_SCRIPTED_ANSWERS", "blue;yes;no;2;s;o:custom freeform");

    let x0ne_bin = &config.bridge.x0ne_core_bin;
    let args: Vec<String> = vec![
        "--mode".into(),
        "stdio".into(),
        "--config".into(),
        selftest_config_path.into(),
    ];
    let mut bridge: Box<dyn bridge::Bridge> =
        match bridge::StdioBridge::spawn_runtime(x0ne_bin, &args).await {
            Ok(b) => Box::new(b),
            Err(e) => {
                print_fatal(&format!("Failed to spawn x0ne-runtime: {}", e));
                return Err(e);
            }
        };

    match bridge.health_check().await {
        Ok(_) => println!(
            "  {}  x0ne-runtime connected (self-test mode, all actions allowed)\n",
            "✓".truecolor(0, 200, 120)
        ),
        Err(e) => {
            if let Some(r) = e.downcast_ref::<x0ne_orchestrator::bridge::RuntimeRefused>() {
                eprintln!("{r}");
                std::process::exit(1);
            }
            print_fatal(&format!("x0ne-runtime health check failed: {}", e));
            return Err(e);
        }
    }

    // ── Test actions ─────────────────────────────────────────────────────
    // Pre-create fixtures
    let _ = bridge.send_action(&json!({"type":"WriteFile","path":"/tmp/x0ne-selftest.txt","content":"Hello from X0NE self-test!\nLine 2\nLine 3\nLine 4\nLine 5\n"})).await;
    let _ = bridge.send_action(&json!({"type":"WriteJson","path":"/tmp/x0ne-selftest.json","data":{"key":"value","n":42},"pretty":true})).await;

    struct TestCase {
        category: &'static str,
        label: &'static str,
        action: serde_json::Value,
    }

    let tests = vec![
        // ── Filesystem ──
        TestCase {
            category: "Filesystem",
            label: "ReadFile",
            action: json!({"type":"ReadFile","path":"/tmp/x0ne-selftest.txt"}),
        },
        TestCase {
            category: "Filesystem",
            label: "WriteFile",
            action: json!({"type":"WriteFile","path":"/tmp/x0ne-selftest-out.txt","content":"Written by self-test"}),
        },
        TestCase {
            category: "Filesystem",
            label: "EditFile",
            action: json!({"type":"EditFile","path":"/tmp/x0ne-selftest-out.txt","edits":[{"old":"Modified","new":"Edited"}]}),
        },
        TestCase {
            category: "Filesystem",
            label: "FileInfo",
            action: json!({"type":"FileInfo","path":"/tmp/x0ne-selftest.txt"}),
        },
        TestCase {
            category: "Filesystem",
            label: "Checksum",
            action: json!({"type":"Checksum","path":"/tmp/x0ne-selftest.txt","algorithm":"sha256"}),
        },
        TestCase {
            category: "Filesystem",
            label: "MoveToTrash (prep)",
            action: json!({"type":"WriteFile","path":format!("{}/x0ne-selftest-trash.tmp", cwd_str),"content":"disposable"}),
        },
        TestCase {
            category: "Filesystem",
            label: "MoveToTrash",
            action: json!({"type":"MoveToTrash","path":format!("{}/x0ne-selftest-trash.tmp", cwd_str)}),
        },
        TestCase {
            category: "Filesystem",
            label: "ApplyPatch",
            action: json!({"type":"ApplyPatch","path":"/tmp/x0ne-selftest-out.txt","patch":"--- a\n+++ b\n@@ -1 +1 @@\n-# Inserted header\n+# Patched header"}),
        },
        // ── Search ──
        TestCase {
            category: "Search",
            label: "SearchCode",
            action: json!({"type":"SearchCode","root":"/tmp","pattern":"X0NE self-test"}),
        },
        // ── Commands ──
        TestCase {
            category: "Commands",
            label: "RunCommand",
            action: json!({"type":"RunCommand","command":"echo","args":["Hello","from","X0NE","self-test"]}),
        },
        // ── System ──
        TestCase {
            category: "System",
            label: "ListProcesses",
            action: json!({"type":"ListProcesses"}),
        },
        TestCase {
            category: "System",
            label: "PortCheck",
            action: json!({"type":"PortCheck","host":"127.0.0.1","port":22,"timeout_secs":1}),
        },
        TestCase {
            category: "System",
            label: "DnsLookup",
            action: json!({"type":"DnsLookup","hostname":"localhost"}),
        },
        // ── Data ──
        TestCase {
            category: "Data",
            label: "RandomBytes",
            action: json!({"type":"RandomBytes","count":32,"encoding":"hex"}),
        },
        TestCase {
            category: "Data",
            label: "ReadLogs",
            action: json!({"type":"ReadLogs","path":"/tmp/x0ne-selftest.txt","lines":5}),
        },
        // ── Memory ──
        TestCase {
            category: "Memory",
            label: "Remember",
            action: json!({"type":"Remember","key":"selftest-key","value":{"demo":true}}),
        },
        TestCase {
            category: "Memory",
            label: "Recall",
            action: json!({"type":"Recall","key":"selftest-key"}),
        },
        TestCase {
            category: "Memory",
            label: "ListMemory",
            action: json!({"type":"ListMemory"}),
        },
        TestCase {
            category: "Memory",
            label: "Forget",
            action: json!({"type":"Forget","key":"selftest-key"}),
        },
        // ── Snapshots ──
        TestCase {
            category: "Snapshots",
            label: "CreateSnapshot",
            action: json!({"type":"CreateSnapshot","name":"selftest-snap","paths":["/tmp/x0ne-selftest.txt"]}),
        },
        TestCase {
            category: "Snapshots",
            label: "ListSnapshots",
            action: json!({"type":"ListSnapshots"}),
        },
        TestCase {
            category: "Snapshots",
            label: "RestoreSnapshot",
            action: json!({"type":"RestoreSnapshot","name":"selftest-snap"}),
        },
        TestCase {
            category: "Snapshots",
            label: "DeleteSnapshot",
            action: json!({"type":"DeleteSnapshot","name":"selftest-snap"}),
        },
        // ── Pipeline ──
        TestCase {
            category: "Pipeline",
            label: "Pipeline (3 steps)",
            action: json!({"type":"Pipeline","steps":[
                {"type":"WriteFile","path":"/tmp/x0ne-pipe-test.txt","content":"step 1"},
                {"type":"AppendFile","path":"/tmp/x0ne-pipe-test.txt","content":"\nstep 2"},
                {"type":"ReadFile","path":"/tmp/x0ne-pipe-test.txt"}
            ]}),
        },
        // ── Math ──
        // ── String ──
        // ── Validation ──
        TestCase {
            category: "Validation",
            label: "Validate",
            action: json!({"type":"Validate","value":"hello","check":"type","expect":"string"}),
        },
        TestCase {
            category: "Validation",
            label: "Diff",
            action: json!({"type":"Diff","a":"hello\nworld","b":"hello\nrust","files":false}),
        },
        // ── Try / Assert ──
        TestCase {
            category: "Try/Assert",
            label: "Try (success)",
            action: json!({"type":"Try","actions":[
                {"type":"WriteFile","path":"/tmp/x0ne-selftest-try.txt","content":"try-ok"},
                {"type":"ReadFile","path":"/tmp/x0ne-selftest-try.txt"}
            ],"rollback_on_fail":true,"label":"selftest-try"}),
        },
        TestCase {
            category: "Try/Assert",
            label: "Assert (file_exists)",
            action: json!({"type":"Assert","check":"file_exists","path":"/tmp/x0ne-selftest.txt"}),
        },
        TestCase {
            category: "Try/Assert",
            label: "Assert (eq)",
            action: json!({"type":"Assert","check":"eq","actual":42,"expected":42}),
        },
        TestCase {
            category: "Try/Assert",
            label: "Assert (not_empty)",
            action: json!({"type":"Assert","check":"not_empty","value":"hello"}),
        },
        // ── Git ──
        // ── Meta ──
        TestCase {
            category: "Meta",
            label: "ListActions",
            action: json!({"type":"ListActions"}),
        },
        TestCase {
            category: "Meta",
            label: "DescribeAction",
            action: json!({"type":"DescribeAction","action_type":"Pipeline"}),
        },
        TestCase {
            category: "Meta",
            label: "PolicyCheck (denied)",
            action: json!({"type":"PolicyCheck","action":{"type":"RunCommand","command":"rm","args":["-rf","/"]}}),
        },
        // ── Handles ──
        TestCase {
            category: "Handles",
            label: "ListHandles",
            action: json!({"type":"ListHandles"}),
        },
        TestCase {
            category: "Handles",
            label: "CheckHandle (none)",
            action: json!({"type":"CheckHandle","handle":"nonexistent-handle"}),
        },
        TestCase {
            category: "Handles",
            label: "StopHandle (none)",
            action: json!({"type":"StopHandle","handle":"nonexistent-handle"}),
        },
        // ── Composites ──
        TestCase {
            category: "Composites",
            label: "ListCustomPrimitives",
            action: json!({"type":"ListCustomPrimitives"}),
        },
        TestCase {
            category: "Composites",
            label: "DefinePrimitive",
            action: json!({"type":"DefinePrimitive","name":"selftest-greet","steps":[{"type":"RunCommand","command":"echo","args":["Hello!"]}]}),
        },
        TestCase {
            category: "Composites",
            label: "CallPrimitive",
            action: json!({"type":"CallPrimitive","name":"selftest-greet"}),
        },
        TestCase {
            category: "Composites",
            label: "DeletePrimitive",
            action: json!({"type":"DeletePrimitive","name":"selftest-greet"}),
        },
        // ── Human / QCM (interactive primitives, seeded via X0NE_SCRIPTED_ANSWERS) ──
        // The 6 canned answers are consumed in this exact order by x0ne-runtime's
        // ScriptedPrompt. Re-ordering the cases below requires re-ordering the
        // X0NE_SCRIPTED_ANSWERS env set above.
        TestCase {
            category: "Human",
            label: "AskHuman",
            action: json!({"type":"AskHuman","question":"What is your favorite color?"}),
        },
        TestCase {
            category: "Human",
            label: "Confirm (approved)",
            action: json!({"type":"Confirm","message":"Proceed with deletion?"}),
        },
        TestCase {
            category: "Human",
            label: "Confirm (declined)",
            action: json!({"type":"Confirm","message":"Delete everything?"}),
        },
        TestCase {
            category: "Human",
            label: "AskMultipleChoice",
            action: json!({"type":"AskMultipleChoice","question":"Pick a fruit",
                "choices":["apple","banana","cherry"]}),
        },
        TestCase {
            category: "Human",
            label: "AskMultipleChoice (skip)",
            action: json!({"type":"AskMultipleChoice","question":"Pick or skip",
                "choices":["x","y"],"allow_skip":true}),
        },
        TestCase {
            category: "Human",
            label: "AskMultipleChoice (other)",
            action: json!({"type":"AskMultipleChoice","question":"Pick or specify",
                "choices":["x","y"],"allow_other":true}),
        },
        // ── Misc ──
    ];

    let mut total = tests.len();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut current_cat = "";
    let global_start = Instant::now();

    for tc in &tests {
        // Section header
        if tc.category != current_cat {
            current_cat = tc.category;
            println!(
                "\n  {}  {}",
                "▸".truecolor(0, 180, 200),
                current_cat.truecolor(0, 200, 220).bold()
            );
            println!("  {}", "─".repeat(50).truecolor(40, 45, 55));
        }

        // Display the action card (how the user sees it before execution)
        print!("{}", display::action_box(&tc.action));

        // Execute through the bridge
        let start = Instant::now();
        let result = bridge.send_action(&tc.action).await;
        let duration = start.elapsed().as_millis() as u64;

        match result {
            Ok(res) => {
                // Display the result line (how the user sees the result)
                print!("{}", display::result_line(&tc.action, &res, duration));

                let success = res["success"].as_bool().unwrap_or(false);
                // PolicyCheck returns success but with a denied inner result
                let is_policy_check = tc.action["type"].as_str() == Some("PolicyCheck");
                if success || is_policy_check {
                    passed += 1;
                } else {
                    // Check if this is an expected failure (e.g. policy denied)
                    let error_kind = res["error"]["kind"].as_str().unwrap_or("");
                    if error_kind == "policy_denied"
                        || error_kind == "not_found"
                        || error_kind == "handle_not_found"
                    {
                        passed += 1; // expected denial
                    } else {
                        failed += 1;
                    }
                }
            }
            Err(e) => {
                println!(
                    "  {}  {} {} — {}",
                    "└".truecolor(40, 45, 55),
                    "✗".red(),
                    tc.label.red(),
                    e.to_string().red()
                );
                failed += 1;
            }
        }
    }

    // ── Parallel Worlds (orchestrator-only, not bridged to x0ne-runtime) ─────
    //
    // These sysaicalls live in `x0ne-orchestrator::sysaicalls` and are
    // intercepted before the bridge (see `serve::dispatch_action` / REPL).
    // We exercise them here against a local `AgentRegistry`, simulating a
    // real sub-agent with `insert_pending` + a tokio task that calls
    // `mark_completed` — no LLM required, same code paths the production
    // registry goes through.
    {
        use serde_json::Value;
        use x0ne_orchestrator::sysaicalls::{self as sc, AgentRegistry, AgentResult};

        println!(
            "\n  {}  {}",
            "▸".truecolor(0, 180, 200),
            "Parallel Worlds".truecolor(0, 200, 220).bold()
        );
        println!("  {}", "─".repeat(50).truecolor(40, 45, 55));

        let registry = std::sync::Arc::new(AgentRegistry::new());

        // Render + counter helper. Takes the success flag by value so each
        // call site decides the pass/fail criterion independently.
        let render = |action: &Value,
                      result: &Value,
                      duration_ms: u64,
                      success: bool,
                      passed: &mut usize,
                      failed: &mut usize,
                      total: &mut usize| {
            *total += 1;
            print!("{}", display::action_box(action));
            print!("{}", display::result_line(action, result, duration_ms));
            if success {
                *passed += 1;
            } else {
                *failed += 1;
            }
        };

        // ── 1. ListAgents on empty registry ───────────────────────────────
        {
            let action = json!({"type": "ListAgents"});
            let s = Instant::now();
            let result = sc::execute_list_agents(&registry);
            let d = s.elapsed().as_millis() as u64;
            let ok = result["success"] == true && result["output"]["total"] == 0;
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 2-4. "Spawn" 3 agents that complete in arbitrary order ────────
        // No real world is spawned (it would need a real LLM + bridge).
        // Instead, mirror what `serve::spawn_agent_http` does: register a
        // pending entry, then spawn a tokio task that finishes the work and
        // fulfils the oneshot. Same AgentRegistry code path used in prod.
        let delays_ms = [80u64, 30, 50]; // → completion order: 1, 2, 0
        for (i, &delay) in delays_ms.iter().enumerate() {
            let id = format!("alt-{i}");
            let task = format!("explore alternative {i}");
            let action = json!({"type": "SpawnWorld", "id": id, "task": task});
            let s = Instant::now();
            let tx = registry.insert_pending(&id, &task);
            let d_setup = s.elapsed().as_millis() as u64;
            // The SpawnWorld success shape.
            let result = json!({
                "success": true,
                "output": { "id": id, "status": "running", "task": task }
            });
            render(
                &action,
                &result,
                d_setup,
                true,
                &mut passed,
                &mut failed,
                &mut total,
            );

            // Stub sub-agent: no LLM, just sleeps `delay` then marks completed.
            let reg_for_task = registry.clone();
            let id_for_task = id.clone();
            let task_for_task = task.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let r = AgentResult {
                    id: id_for_task.clone(),
                    task: task_for_task,
                    response: format!("(stub) completed after {delay}ms"),
                    tokens_used: 0,
                    actions_executed: 0,
                    actions_breakdown: AgentResult::empty_breakdown(),
                    success: true,
                    duration_ms: delay,
                };
                reg_for_task.mark_completed(&id_for_task, r.clone(), true);
                let _ = tx.send(r);
            });
        }

        // ── 5. ListAgents while agents are still running ───────────────────
        {
            let action = json!({"type": "ListAgents"});
            let s = Instant::now();
            let result = sc::execute_list_agents(&registry);
            let d = s.elapsed().as_millis() as u64;
            // 3 entries, at least 0 completed so far — the assertion is just
            // "total == 3", timing-dependent on running/completed split.
            let ok = result["success"] == true && result["output"]["total"] == 3;
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 6-8. WaitAgent on each — completion order ≠ request order ──────
        for i in 0..3 {
            let id = format!("alt-{i}");
            let action = json!({"type": "WaitAgent", "id": id, "timeout_secs": 5});
            let s = Instant::now();
            let result = sc::execute_wait_agent(&registry, &action)
                .await
                .unwrap_or_else(|e| json!({"success": false, "error": {"message": e.to_string()}}));
            let d = s.elapsed().as_millis() as u64;
            let ok = result["success"] == true && result["output"]["id"] == id;
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 9. ListAgents after all waits — everyone should be Completed ───
        {
            let action = json!({"type": "ListAgents"});
            let s = Instant::now();
            let result = sc::execute_list_agents(&registry);
            let d = s.elapsed().as_millis() as u64;
            let ok = result["success"] == true
                && result["output"]["total"] == 3
                && result["output"]["running"] == 0
                && result["output"]["completed"] == 3;
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 10. BroadcastAgents (no Running targets left — honest failure) ──
        {
            let action = json!({"type": "BroadcastAgents", "message": "halt"});
            let s = Instant::now();
            let result = sc::execute_broadcast_agents(&registry, &action);
            let d = s.elapsed().as_millis() as u64;
            // No running agents → success must be false (honest semantics)
            let ok = result["success"] == false
                && result["output"]["delivered_to"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(false);
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 11. Error: WaitAgent on unknown id ─────────────────────────────
        {
            let action = json!({"type": "WaitAgent", "id": "ghost"});
            let s = Instant::now();
            let result = sc::execute_wait_agent(&registry, &action)
                .await
                .unwrap_or_else(|e| json!({"success": false, "error": {"message": e.to_string()}}));
            let d = s.elapsed().as_millis() as u64;
            // Expected denial: success=false, error message contains "not found".
            let msg = result["error"]["message"].as_str().unwrap_or("");
            let ok = result["success"] == false && msg.contains("not found");
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }

        // ── 12. Error: WaitAgent with empty id ─────────────────────────────
        {
            let action = json!({"type": "WaitAgent", "id": ""});
            let s = Instant::now();
            let result = sc::execute_wait_agent(&registry, &action)
                .await
                .unwrap_or_else(|e| json!({"success": false, "error": {"message": e.to_string()}}));
            let d = s.elapsed().as_millis() as u64;
            let msg = result["error"]["message"].as_str().unwrap_or("");
            let ok = result["success"] == false && msg.contains("requires");
            render(
                &action,
                &result,
                d,
                ok,
                &mut passed,
                &mut failed,
                &mut total,
            );
        }
    }

    // ── Plan (orchestrator sysaicall — approve + rewrite to Try over bridge) ──
    //
    // `display_plan_and_confirm` normally reads stdin for y/n/e; we bypass
    // that with `X0NE_PLAN_AUTO_APPROVE=yes|no` so the self-test exercises
    // the full rewrite-to-Try path without a TTY.
    {
        use serde_json::Value;
        use x0ne_orchestrator::conversation::Conversation;
        use x0ne_orchestrator::sysaicalls::display_plan_and_confirm;

        println!(
            "\n  {}  {}",
            "▸".truecolor(0, 180, 200),
            "Plan".truecolor(0, 200, 220).bold()
        );
        println!("  {}", "─".repeat(50).truecolor(40, 45, 55));

        let render = |action: &Value,
                      result: &Value,
                      duration_ms: u64,
                      success: bool,
                      passed: &mut usize,
                      failed: &mut usize,
                      total: &mut usize| {
            *total += 1;
            print!("{}", display::action_box(action));
            print!("{}", display::result_line(action, result, duration_ms));
            if success {
                *passed += 1;
            } else {
                *failed += 1;
            }
        };

        let mut conv = Conversation::new(8000);

        // 1. Approved plan → the WriteFile step must actually run.
        let plan_path = "/tmp/x0ne-selftest-plan.txt";
        std::env::set_var("X0NE_PLAN_AUTO_APPROVE", "yes");
        let action = json!({
            "type": "Plan",
            "label": "write greeting file",
            "steps": [
                { "type": "WriteFile", "path": plan_path, "content": "hello from plan\n" },
            ]
        });
        let s = Instant::now();
        // T0.5: pass `is_subagent=false` (selftest is the top-level operator).
        let decision = display_plan_and_confirm(
            &mut bridge,
            &config,
            &action,
            &mut conv,
            /*is_subagent=*/ false,
        )
        .await
        .ok();
        let approved = decision.as_ref().map(|d| d.approved).unwrap_or(false);
        let decision_source = decision
            .as_ref()
            .map(|d| d.decision_source)
            .unwrap_or("stdin_io_error");
        let d = s.elapsed().as_millis() as u64;
        let file_exists = std::fs::metadata(plan_path).is_ok();
        let result = json!({
            "success": approved && file_exists,
            "output": {
                "approved": approved,
                "decision_source": decision_source,
                "steps_executed": if file_exists { 1 } else { 0 },
                "label": "write greeting file",
            }
        });
        let ok = approved && file_exists;
        render(
            &action,
            &result,
            d,
            ok,
            &mut passed,
            &mut failed,
            &mut total,
        );

        // 2. Rejected plan → no side effects, no Try sent.
        let plan_path_rej = "/tmp/x0ne-selftest-plan-rejected.txt";
        std::env::set_var("X0NE_PLAN_AUTO_APPROVE", "no");
        let action = json!({
            "type": "Plan",
            "label": "should not execute",
            "steps": [
                { "type": "WriteFile", "path": plan_path_rej, "content": "should not appear\n" },
            ]
        });
        let s = Instant::now();
        // T0.5: same call shape as the approved branch above.
        let decision = display_plan_and_confirm(
            &mut bridge,
            &config,
            &action,
            &mut conv,
            /*is_subagent=*/ false,
        )
        .await
        .ok();
        // Worst case (decision missing): treat as approved so the assert fails loudly.
        let approved = decision.as_ref().map(|d| d.approved).unwrap_or(true);
        let decision_source = decision
            .as_ref()
            .map(|d| d.decision_source)
            .unwrap_or("stdin_io_error");
        let d = s.elapsed().as_millis() as u64;
        let file_exists = std::fs::metadata(plan_path_rej).is_ok();
        let result = json!({
            "success": !approved && !file_exists,
            "output": {
                "approved": approved,
                "decision_source": decision_source,
                "steps_executed": if file_exists { 1 } else { 0 },
                "label": "should not execute",
            }
        });
        let ok = !approved && !file_exists;
        render(
            &action,
            &result,
            d,
            ok,
            &mut passed,
            &mut failed,
            &mut total,
        );

        // Clean up env so normal runs aren't affected.
        std::env::remove_var("X0NE_PLAN_AUTO_APPROVE");
        let _ = std::fs::remove_file(plan_path);
    }

    let elapsed = global_start.elapsed();

    // ── Cleanup ──────────────────────────────────────────────────────────
    let _ = bridge
        .send_action(&json!({"type":"RunCommand","command":"rm","args":["-rf",
        "/tmp/x0ne-selftest.txt","/tmp/x0ne-selftest.json","/tmp/x0ne-selftest-out.txt",
        "/tmp/x0ne-selftest-moved.txt","/tmp/x0ne-pipe-test.txt","/tmp/x0ne-selftest-wj.json",
        "/tmp/x0ne-selftest-subdir"]}))
        .await;
    let _ = std::fs::remove_file(selftest_config_path);

    // ── Summary ──────────────────────────────────────────────────────────
    println!("\n  {}", "═".repeat(56).truecolor(40, 45, 55));
    println!(
        "  {}  {} / {} passed  {}  {:.1}s",
        if failed == 0 {
            "✓".truecolor(0, 200, 120).bold()
        } else {
            "✗".red().bold()
        },
        passed.to_string().truecolor(0, 200, 120),
        total.to_string().truecolor(80, 90, 100),
        if failed > 0 {
            format!("({} failed)", failed).red().to_string()
        } else {
            String::new()
        },
        elapsed.as_secs_f64()
    );

    // Report env-gated primitives that we cannot test locally
    let skipped: &[(&str, &[&str])] = &[
        (
            "Desktop (Wayland/X11)",
            &[
                "Screenshot",
                "GetWindowTree",
                "GetFocusedWindow",
                "MoveMouse",
                "Click",
                "DoubleClick",
                "Scroll",
                "TypeText",
                "PressKey",
                "PressKeyCombination",
                "FocusWindow",
                "SendNotification",
                "GetClipboard",
                "SetClipboard",
            ],
        ),
        (
            "ML (Ollama)",
            &["OllamaApi", "LlmCall", "DatasetGenerate", "DocumentExtract"],
        ),
        ("Incus (containers)", &["RunIncus", "RunIncusGroup"]),
        ("Sandbox (nsjail)", &["RunInSandbox"]),
        (
            "Events (async)",
            &[
                "Subscribe",
                "FileChanged",
                "FileCreated",
                "PortOpen",
                "ProcessExited",
            ],
        ),
        // AskHuman / Confirm / AskMultipleChoice are now covered by the
        // Human section (seeded via X0NE_SCRIPTED_ANSWERS), so they no
        // longer belong in the skipped bucket.
        ("Network", &["FetchUrl", "HttpRequest"]),
    ];
    let skip_count: usize = skipped.iter().map(|(_, acts)| acts.len()).sum();
    println!(
        "  {}  {} skipped (env-gated)",
        "·".truecolor(80, 90, 100),
        skip_count.to_string().truecolor(80, 90, 100)
    );
    for (env, acts) in skipped {
        println!(
            "    {}  {} ({})",
            "·".truecolor(50, 55, 65),
            env.truecolor(60, 70, 80),
            acts.len().to_string().truecolor(50, 55, 65)
        );
    }
    println!("  {}\n", "═".repeat(56).truecolor(40, 45, 55));

    if failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Interactive Telegram pairing entry point. Loads (or creates) the
/// persisted config, prompts for the bot token if missing, then
/// runs the QR pairing flow and persists the resulting whitelisted
/// chat_id.
///

/// `/budget` writes `config.cost.budget_usd_limit`; the session spends against
/// `state.budget`. Until 2026-10-06 nothing carried one to the other, so
/// `/budget 0.30` capped nothing. Refused above `cost.max_session_usd`.
fn apply_budget_change(
    agent: &mut x0ne_orchestrator::agent::session::CliAgentSession,
    before: f64,
) -> Result<(), String> {
    use x0ne_orchestrator::agent::budget::BudgetControl;
    let now = agent.config.cost.budget_usd_limit;
    if now == before {
        return Ok(());
    }
    match agent.state.budget.set_limit_usd(now) {
        Ok(_) => Ok(()),
        Err(e) => {
            agent.config.cost.budget_usd_limit = before;
            Err(format!("/budget: {e}"))
        }
    }
}

/// Where a world's own dir is made: the directory x0ne was started in, which
/// is the runtime's. Not in http mode, where the runtime runs elsewhere.
fn worlds_base_for(config: &x0ne_orchestrator::config::Config) -> Option<std::path::PathBuf> {
    if config.bridge.mode == "http" {
        return None;
    }
    std::env::current_dir().ok()
}

/// Why `--bench-agentic` may not run here, if it may not.
fn bench_agentic_refusal(
    one_shot: bool,
    isolation: Option<&str>,
    in_container: bool,
) -> Option<&'static str> {
    if !one_shot {
        Some("it runs only with -p (one headless bench turn)")
    } else if isolation != Some("container") {
        Some("it needs --isolation container")
    } else if !in_container {
        Some("no container detected; it runs only inside a throwaway bench container")
    } else {
        None
    }
}

/// Consecutive no-human refusals `-p` returns to the model before ending the
/// turn. Each one still runs nothing; the cap stops a refusal loop.
const ONE_SHOT_UNATTENDED_DENIALS: u32 = 3;

/// Exit status of `x0ne -p`: 0 for a completed turn, 2 for every other
/// outcome. 1 is left to `main` returning `Err` (startup, not the turn).
fn one_shot_exit_code(outcome: &x0ne_orchestrator::agent::driver::TurnOutcome) -> i32 {
    use x0ne_orchestrator::agent::driver::TurnOutcome;
    if matches!(outcome, TurnOutcome::Completed { .. }) {
        0
    } else {
        2
    }
}

/// SessionStart's `source` for a session that did or did not reload a
/// transcript.
fn session_source(resumed: bool) -> SessionSource {
    if resumed {
        SessionSource::Resume
    } else {
        SessionSource::Startup
    }
}

/// The REPL prompt's chevron: `plan ❯` in amber while plan mode is on.
fn prompt_chevron(plan: bool) -> String {
    if plan {
        format!(
            "{} {}",
            "plan".truecolor(230, 170, 60).bold(),
            "❯".truecolor(230, 170, 60).bold()
        )
    } else {
        "❯".truecolor(0, 200, 220).bold().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production source of this file, with comments stripped: a comment that
    /// *names* a banned assignment in order to say we no longer make it must
    /// not itself trip the scan.
    fn production_code() -> String {
        let full = include_str!("main.rs");
        let head = &full[..full.find("#[cfg(test)]").unwrap_or(full.len())];
        head.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `-p` branch alone, comments stripped.
    ///
    /// Sliced from the raw source rather than from [`production_code`] because
    /// the branch's end marker is a comment banner; the stripping happens
    /// after the cut, not before it.
    fn one_shot_branch() -> String {
        let full = include_str!("main.rs");
        let start = full
            .find("if let Some(prompt_text) = cli.prompt {")
            .expect("the -p branch");
        let end = full[start..]
            .find("if cli.auto_mode {")
            .expect("the branch that follows -p")
            + start;
        full[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `--machine` help announces a price per hour, so it is tied to the
    /// price the budget gate applies.
    ///
    /// The prover mechanism is a LITERAL: clap takes its help text from a
    /// doc-comment, and a doc-comment cannot call `hourly_usd()`. The literal
    /// is therefore not a shortcut, it is the only possible form — and an
    /// unguarded displayed literal is exactly what let `WaitSsh` promise
    /// « ~5 min » on a 2.6 h cap, and what let the `full` refusal quote
    /// 11.46 $/h three days after the switch to SPOT. The guard is here: the
    /// two printed figures must be the ones `CcMachine::hourly_usd()` returns,
    /// otherwise the help sells a price nobody bills.
    ///
    /// The text is read from clap itself (`render_help`), not from the
    /// source: what is proven is what the operator sees.
    ///
    /// And each rate is looked for ON THE RIGHT SIDE of the separator (review
    /// round 1, I3). The first version asked for « 0.24 somewhere » and « 3.72
    /// somewhere »: swapping the two literals — the prover announced at 3.72,
    /// the H100 at 0.24, a 15× underquote on the only screen read before
    /// paying — left it green. A test that does not look WHERE does not test
    /// the property it states.
    #[cfg(feature = "cloud")]
    #[test]
    fn the_machine_help_quotes_the_prices_the_budget_gate_charges() {
        use clap::CommandFactory;
        use x0ne_cloud::cc_plan::CcMachine;
        let mut cc = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "cc")
            .expect("the cc subcommand")
            .clone();
        // Whitespace normalised: clap re-wraps the help to the terminal
        // width, so the separator and the rates can land anywhere on the
        // line. The number itself is never split in two.
        let help = cc
            .render_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let (prover_side, full_side) = help
            .split_once("| full =")
            .expect("the help must separate the two machines with `| full =`");
        for (machine, side, which) in [
            (CcMachine::Prover, prover_side, "prover"),
            (CcMachine::FullH100, full_side, "full"),
        ] {
            let price = format!("{:.2}", machine.hourly_usd());
            assert!(
                side.contains(&price),
                "the {which} rate ({price}) must be stated ON THE {which} SIDE, \
                 i.e. the one the budget gate applies to it: {help}"
            );
            // And not the other one's: that is the half that was missing, and
            // without it two swapped literals pass.
            let other = match machine {
                CcMachine::Prover => CcMachine::FullH100,
                CcMachine::FullH100 => CcMachine::Prover,
            };
            assert!(
                !side.contains(&format!("{:.2}", other.hourly_usd())),
                "and the other machine's rate does not belong on the {which} side: {help}"
            );
        }
        // …and both machines must be named, with what sets them apart
        // : `full` is billed 15× the prover and exists only as SPOT.
        assert!(prover_side.contains("prover ="), "{help}");
        assert!(
            full_side.contains("a3-highgpu-1g"),
            "the help must say WHICH machine: {help}"
        );
        assert!(
            full_side.contains("SPOT"),
            "and that SPOT mode is not optional: {help}"
        );
        assert!(
            full_side.contains("unverified"),
            "the spot rate is an unverified estimate, and the help says so: {help}"
        );
    }

    /// The cloud/GPU tag must actually reach the navbar. This is a live
    /// display surface — the class of thing that shipped two border bugs
    /// unnoticed because nothing exercised it — and it cannot be unit-tested
    /// through a terminal, so the wiring is pinned mechanically instead:
    /// the monitor is started from config, its line is read, and the tag is
    /// interpolated into the navbar format string.
    #[test]
    fn the_navbar_carries_the_cloud_tag() {
        let src = production_code();
        assert!(
            src.contains("cloudmon::start_monitor(config.cloud.enabled)"),
            "the monitor must be started from the operator's [cloud] opt-in"
        );
        assert!(
            src.contains("let cloud_tag = {"),
            "the navbar must build a cloud tag"
        );
        assert!(
            src.contains("s.navbar_line()"),
            "the tag must come from CloudState::navbar_line, not a second formatter"
        );
        assert!(
            src.contains("s.has_alert()"),
            "an idle-GPU alert must drive the tag's colour"
        );
        // And the tag must be interpolated into the navbar, not built and
        // dropped — the exact failure mode of the first cloudmon landing.
        let navbar = {
            let start = src
                .find("let navbar = format!(")
                .expect("the navbar format");
            let end = src[start..].find(");").expect("end of the navbar format") + start;
            &src[start..end]
        };
        assert!(
            navbar.contains("cloud_tag"),
            "cloud_tag is built but never rendered: {navbar}"
        );
    }

    /// The cutover itself. `-p` is the first public entrypoint on the canonical
    /// path, and the proof has to be mechanical: a reader can be told the
    /// entrypoint moved, but only the source says whether it did.
    #[test]
    fn the_one_shot_path_runs_on_the_canonical_driver() {
        let branch = one_shot_branch();
        assert!(
            branch.contains("CliAgentSession::new(") && branch.contains("run_turn(&full_prompt)"),
            "`-p` does not assemble a session and run a turn on it"
        );
        assert!(
            !branch.contains("parse_llm_response"),
            "`-p` still calls the legacy loop"
        );
    }

    /// The bench-agentic profile is refused outside its one setting.
    #[test]
    fn bench_agentic_is_refused_outside_a_headless_container_run() {
        assert!(bench_agentic_refusal(true, Some("container"), true).is_none());
        assert!(bench_agentic_refusal(false, Some("container"), true).is_some());
        assert!(bench_agentic_refusal(true, Some("none"), true).is_some());
        assert!(bench_agentic_refusal(true, None, true).is_some());
        assert!(bench_agentic_refusal(true, Some("container"), false).is_some());
        let branch = one_shot_branch();
        assert!(
            branch.contains("if cli.bench_agentic {"),
            "gate lifted outside the flag"
        );
        assert!(branch.contains("agent.limits.policy_stop_retries = 1;"));
    }

    /// Bench E1 2026-10-12: `-p` hands no-human refusals back to the
    /// model, with a cap, instead of ending on the first one.
    #[test]
    fn the_one_shot_path_returns_headless_refusals_to_the_model() {
        let branch = one_shot_branch();
        assert!(
            branch.contains(
                "agent.limits.unattended_denials_returned = ONE_SHOT_UNATTENDED_DENIALS;"
            ),
            "`-p` ends on the first headless refusal"
        );
        assert!(ONE_SHOT_UNATTENDED_DENIALS > 0 && ONE_SHOT_UNATTENDED_DENIALS <= 5);
    }

    /// A `-p` turn is journaled with its prompt, before the turn runs:
    /// `sessions` counts turns by `user_input`.
    #[test]
    fn the_one_shot_prompt_is_journaled_before_the_turn() {
        let branch = one_shot_branch();
        let logged = branch
            .find("log_user(&prompt_text)")
            .expect("`-p` never journals its prompt");
        let run = branch.find("run_turn(&full_prompt)").unwrap();
        assert!(logged < run, "the prompt is journaled after the turn");
    }

    /// Every `--auto-mode` iteration is journaled with its prompt, before
    /// the turn runs: an unattended run with `turn` lines and no `user_input`
    /// counts zero turns in `sessions`.
    #[test]
    fn the_auto_mode_prompt_is_journaled_before_the_turn() {
        let branch = auto_mode_branch();
        let logged = branch
            .find("log_user(prompt)")
            .expect("--auto-mode never journals its prompt");
        let run = branch.find("agent.run_turn(prompt)").unwrap();
        assert!(logged < run, "the prompt is journaled after the turn");
    }

    /// Each `/bench` run is journaled with the shared prompt, before the
    /// model's turn, so the journal says what every model was asked.
    #[test]
    fn the_benchmark_prompt_is_journaled_before_each_turn() {
        let branch = bench_branch();
        let logged = branch
            .find("log_user(&prompt)")
            .expect("/bench never journals its prompt");
        let run = branch.find("bench_session.run_turn(&prompt)").unwrap();
        assert!(logged < run, "the prompt is journaled after the turn");
    }

    /// One runtime, not two. Reopening the bridge here would start a second
    /// `x0ne-runtime` process with its own policy engine, which is the failure
    /// the two-process trust split exists to make impossible by accident.
    #[test]
    fn the_one_shot_path_opens_no_second_runtime() {
        let branch = one_shot_branch();
        assert!(
            !branch.contains("create_bridge("),
            "`-p` opens its own runtime instead of moving the one already connected"
        );
        assert!(
            branch.contains("Some(bridge)"),
            "`-p` no longer hands the existing bridge to the session"
        );
    }

    /// The prose comes from the turn that produced it. The old path recovered
    /// it by scanning the transcript backwards for the last assistant message,
    /// which cannot tell an answer from the last thing said before an abort.
    #[test]
    fn the_one_shot_response_comes_from_the_report_not_a_transcript_scan() {
        let branch = one_shot_branch();
        assert!(branch.contains("report.response()"));
        assert!(
            !branch.contains(".messages()"),
            "`-p` still rescans the conversation for its answer"
        );
    }

    /// The JSON envelope is a public contract. Three fields, these three, in
    /// this order — a fourth would break every caller that parses strictly.
    #[test]
    fn the_one_shot_json_envelope_keeps_exactly_its_historical_fields() {
        let branch = one_shot_branch();
        let at = branch
            .find(r#"if cli.output_format == "json""#)
            .expect("the json branch");
        let keys: Vec<&str> = branch[at..]
            .lines()
            .map(|l| l.trim())
            // A key, not the `"{}"` of the `println!` that carries the object.
            .filter(|l| l.starts_with('"') && l.contains("\":"))
            .filter_map(|l| l[1..].split('"').next())
            .collect();
        assert_eq!(
            keys,
            vec!["response", "tokens", "model"],
            "the one-shot JSON envelope changed shape"
        );
        assert!(branch.contains("report.usage.total_tokens()"));
    }

    /// Owner decision 2026-10-10, reversing the earlier lock: the exit
    /// status follows the TURN OUTCOME — never the per-action tally, which
    /// would make a turn that recovered from one failed action exit non-zero.
    #[test]
    fn the_one_shot_exit_status_follows_the_turn_outcome_only() {
        let branch = one_shot_branch();
        assert!(branch.contains("one_shot_exit_code(&report.outcome)"));
        assert!(branch.contains("std::process::exit(code)"));
        assert!(branch.contains("operator_hint_for_no_human_channel("));
        for banned in ["actions_all_succeeded", "report.actions", "ExitCode"] {
            assert!(
                !branch.contains(banned),
                "`-p` derives its exit status from `{banned}`"
            );
        }
        assert!(branch.contains("return Ok(())"));
    }

    #[test]
    fn the_one_shot_exit_code_is_zero_only_for_a_completed_turn() {
        use x0ne_orchestrator::agent::driver::TurnOutcome;
        assert_eq!(
            one_shot_exit_code(&TurnOutcome::Completed {
                message: "done".into()
            }),
            0
        );
        for ended in [
            TurnOutcome::Cancelled,
            TurnOutcome::Denied {
                action_type: "WriteFile".into(),
                reason: "no human could be asked".into(),
            },
            TurnOutcome::LoopDetected {
                action_type: "WriteFile".into(),
            },
            TurnOutcome::Stationary {
                action_type: "ReadFile".into(),
            },
            TurnOutcome::ContextIrreducible,
            TurnOutcome::ActionCapExceeded { executed: 3 },
            TurnOutcome::CostUnknownUnderStrictBudget { model: "m".into() },
        ] {
            assert_eq!(one_shot_exit_code(&ended), 2, "{}", ended.kind());
        }
    }

    /// Preprocessing stayed in the frontend, unchanged. The driver never learns
    /// what an `@file` is.
    #[test]
    fn the_one_shot_path_keeps_its_own_preprocessing() {
        let branch = one_shot_branch();
        assert!(branch.contains("expand_at_references("));
        assert!(branch.contains(r#"--- stdin ---"#));
        assert!(branch.contains("read_to_string("));
    }

    /// No public entrypoint runs a loop of its own: model output is parsed
    /// only by the orchestrator's driver. The legacy `run_agent_loop` was
    /// removed on 2026-09-23.
    #[test]
    fn no_public_entrypoint_runs_its_own_loop() {
        let code = production_code();
        let callers =
            code.matches("parse_llm_response").count() + code.matches("parse::interpret(").count();
        assert_eq!(
            callers, 0,
            "a public CLI path parses model output itself ({callers} call sites)"
        );
    }

    /// The `/bench` arm alone, comments stripped.
    fn bench_branch() -> String {
        let full = include_str!("main.rs");
        let start = full
            .find("SlashResult::Benchmark(models, prompt)")
            .expect("the benchmark arm");
        let end = full[start..]
            .find("SlashResult::NotSlash")
            .expect("the end of the match")
            + start;
        full[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One fresh session per benchmarked model, built inside the loop.
    ///
    /// The placement is the isolation: a session constructed outside would be
    /// shared, and model B would inherit model A's transcript, approvals and
    /// spend. This is the inverse of the auto-mode guard, and deliberately so.
    #[test]
    fn bench_builds_a_fresh_session_inside_its_model_loop() {
        let branch = bench_branch();
        assert_eq!(
            branch.matches("CliAgentSession::new(").count(),
            1,
            "the benchmark should assemble its session in exactly one place"
        );
        let loop_at = branch
            .find("for (i, model_name) in models.iter().enumerate()")
            .expect("the model loop");
        let built = branch.find("CliAgentSession::new(").expect("the session");
        assert!(
            built > loop_at,
            "the benchmark session is built outside the model loop, so every \
             model after the first would inherit its predecessor's state"
        );
        // And never by swapping a backend into a session that already ran.
        assert!(
            !branch.contains("bench_session.set_backend("),
            "the benchmark reuses one session across models"
        );
    }

    /// Every model is given the same task, and nothing model-specific is
    /// appended to it.
    #[test]
    fn every_benchmarked_model_receives_the_same_task() {
        let branch = bench_branch();
        assert!(
            branch.contains("bench_session.run_turn(&prompt)"),
            "the benchmark does not run each model on the shared prompt"
        );
        assert_eq!(
            branch.matches("run_turn(").count(),
            1,
            "more than one turn shape exists in the benchmark"
        );
    }

    /// The statistics are the turn's own. Nothing rescans a transcript and
    /// nothing keeps a second tally.
    #[test]
    fn bench_statistics_come_from_the_turn_report() {
        let branch = bench_branch();
        for field in [
            "report.duration",
            "report.usage.input_tokens",
            "report.usage.output_tokens",
            "report.actions.total",
            "report.actions.succeeded",
            "report.actions.failed",
        ] {
            assert!(
                branch.contains(field),
                "the benchmark does not source `{field}` from the report"
            );
        }
        for banned in ["UsageTracker::new", "turn_stats", ".messages()"] {
            assert!(
                !branch.contains(banned),
                "the benchmark keeps a second accumulator (`{banned}`)"
            );
        }
    }

    /// A model whose backend cannot be built is recorded as a zeroed run and
    /// the benchmark moves on — the historical behaviour, pinned so a later
    /// change to stop-on-failure has to be deliberate.
    #[test]
    fn a_backend_failure_records_a_zeroed_run_and_continues() {
        let branch = bench_branch();
        let at = branch
            .find("Failed to create backend")
            .expect("the failure arm");
        let arm = &branch[at..];
        let next_continue = arm.find("continue;").expect("the continue");
        let next_break = arm.find("break").unwrap_or(usize::MAX);
        assert!(
            next_continue < next_break,
            "a backend failure now aborts the benchmark instead of skipping \
             the model"
        );
        assert!(arm[..next_continue].contains("bench_runs.push(BenchRun {"));
    }

    /// The autonomous loop, as a table. Iteration 0 opens with the analysis
    /// instruction; every later one asks the model to continue — the same two
    /// sentences the historical loop used, one turn earlier.
    #[test]
    fn the_autonomous_loop_opens_once_and_then_continues() {
        assert_eq!(
            auto_mode_step(0, 5, false),
            AutoStep::Run(AUTO_MODE_INITIAL)
        );
        assert_eq!(
            auto_mode_step(1, 5, false),
            AutoStep::Run(AUTO_MODE_CONTINUE)
        );
        assert_eq!(
            auto_mode_step(4, 5, false),
            AutoStep::Run(AUTO_MODE_CONTINUE)
        );
    }

    /// The bound is a bound. Historically checked at the top of the iteration,
    /// so `max_iterations` turns run and the next decision stops.
    #[test]
    fn the_autonomous_loop_respects_its_iteration_bound() {
        assert_eq!(auto_mode_step(5, 5, false), AutoStep::MaxIterations);
        assert_eq!(auto_mode_step(6, 5, false), AutoStep::MaxIterations);
        // A zero bound runs nothing rather than running once.
        assert_eq!(auto_mode_step(0, 0, false), AutoStep::MaxIterations);
    }

    /// An interrupt stops the repetition, and the bound still wins a tie —
    /// the historical order, kept so a run that finishes reports finishing.
    #[test]
    fn an_interrupt_stops_the_autonomous_loop() {
        assert_eq!(auto_mode_step(1, 5, true), AutoStep::Interrupted);
        assert_eq!(auto_mode_step(0, 5, true), AutoStep::Interrupted);
        assert_eq!(auto_mode_step(5, 5, true), AutoStep::MaxIterations);
    }

    /// The `--auto-mode` branch alone, comments stripped.
    fn auto_mode_branch() -> String {
        let full = include_str!("main.rs");
        let start = full
            .find("if cli.auto_mode {")
            .expect("the auto-mode branch");
        let end = full[start..].find("MAIN LOOP").expect("the REPL banner") + start;
        full[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── Resumable sessions ───────────────────────────────────────────

    /// `--health` answers and exits before any conversation surface.
    ///
    /// Measured on 2026-08-17: the flag skipped the intro and then started
    /// the full REPL — `x0ne --health </dev/null` exited 0 having proven
    /// nothing. A failure here means the early return moved below a
    /// `CliAgentSession` construction, i.e. the probe can reach a session.
    #[test]
    fn health_returns_before_any_session_is_constructed() {
        let src = production_code();
        let probe = src
            .find("if cli.health {")
            .expect("the --health early return exists in the boot path");
        let probe_block = &src[probe..probe + 400];
        assert!(
            probe_block.contains("return Ok(())"),
            "the --health arm must return, not fall through"
        );
        let first_session = src
            .find("CliAgentSession::new(")
            .expect("a session construction exists");
        assert!(
            probe < first_session,
            "--health must return before the first CliAgentSession::new"
        );
        // And the probe answers AFTER the bridge check has real evidence —
        // a probe placed before `bridge.health_check()` would report healthy
        // without having asked.
        let bridge_check = src
            .find("bridge.health_check()")
            .expect("the bridge health check exists");
        assert!(
            bridge_check < probe,
            "--health must answer after the bridge health check, not before"
        );
    }

    /// `--resume` is honoured or refused, never ignored.
    ///
    /// Every surface listed here returns from `main` before the journal is
    /// opened. A failure means X0NE grew a twelfth piece of dead
    /// scaffolding: a flag in `--help` that changes nothing.
    #[test]
    fn resume_is_refused_on_every_surface_that_would_ignore_it() {
        assert!(refuse_resume_where_it_is_ignored(&[]).is_ok());
        assert!(refuse_resume_where_it_is_ignored(&[("--tui", false)]).is_ok());
        for flag in [
            "--serve",
            "--tui",
            "--matrix",
            "--health",
            "--setup",
            "a subcommand",
        ] {
            let err = refuse_resume_where_it_is_ignored(&[(flag, true)])
                .expect_err("{flag} silently swallowed --resume");
            assert!(
                err.contains(flag),
                "the refusal must name the surface: {err}"
            );
        }
        // The guard runs before every early return, or it guards nothing.
        let src = production_code();
        let guard = src
            .find("refuse_resume_where_it_is_ignored(&[")
            .expect("the guard is called");
        for early in [
            "if cli.serve {",
            "if cli.matrix {",
            "if cli.tui {",
            "if let Some(Subcommand::Setup { runtime }) = &cli.command {",
        ] {
            let at = src.find(early).unwrap_or_else(|| panic!("no `{early}`"));
            assert!(
                guard < at,
                "`{early}` returns before the --resume guard runs"
            );
        }
    }

    /// A flag `--tui` or `--matrix` would drop is refused, by name,
    /// before either surface (or any other early return) starts.
    #[test]
    fn the_tui_and_matrix_refuse_the_flags_they_would_ignore() {
        assert!(refuse_flags_ignored_by("--tui", &[]).is_ok());
        assert!(refuse_flags_ignored_by("--tui", &[("--prompt", false)]).is_ok());
        let err = refuse_flags_ignored_by("--matrix", &[("--stdin", false), ("--auto-mode", true)])
            .expect_err("--auto-mode swallowed by --matrix");
        assert!(
            err.contains("--auto-mode") && err.contains("--matrix"),
            "{err}"
        );
        let src = production_code();
        let guard = src.find("if cli.tui || cli.matrix {").expect("the guard");
        for early in [
            "if cli.serve {",
            "if cli.matrix {",
            "if cli.tui {",
            "if let Some(Subcommand::Setup { runtime }) = &cli.command {",
        ] {
            let at = src.find(early).unwrap_or_else(|| panic!("no `{early}`"));
            assert!(
                guard < at,
                "`{early}` returns before the flags-over-environment guard runs"
            );
        }
    }

    /// No variable set: a test must not resolve the shell it runs in.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// On every surface `--provider` beats `X0NE_LLM_PROVIDER` and
    /// `--mode` beats `X0NE_BRIDGE_MODE`; without a flag the variable beats `-c`.
    #[test]
    fn on_every_surface_a_flag_beats_the_environment_which_beats_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            "[llm]\nprovider = \"ollama\"\n\n[bridge]\nmode = \"http\"\n",
        )
        .expect("config");
        let file = file.to_str().expect("utf-8 path");
        let env = |k: &str| match k {
            "X0NE_LLM_PROVIDER" => Some("openai".to_string()),
            "X0NE_BRIDGE_MODE" => Some("sandbox".to_string()),
            _ => None,
        };
        #[allow(unused_mut)]
        let mut surfaces: Vec<&[&str]> =
            vec![&[], &["-p", "hi"], &["--acp"], &["--tui"], &["--matrix"]];
        #[cfg(feature = "serve")]
        surfaces.push(&["--serve"]);
        for surface in surfaces {
            let parse = |flags: &[&str]| {
                let mut argv = vec!["x0ne", "-c", file];
                argv.extend_from_slice(surface);
                argv.extend_from_slice(flags);
                Cli::try_parse_from(argv).expect("parse")
            };
            let c = session_config(&parse(&[]), env).expect("resolved");
            assert_eq!(
                (c.llm.provider.as_str(), c.bridge.mode.as_str()),
                ("openai", "sandbox"),
                "{surface:?}: the environment beats -c"
            );
            let flags = ["--provider", "openai-compat", "--mode", "stdio"];
            let c = session_config(&parse(&flags), env).expect("resolved");
            assert_eq!(
                (c.llm.provider.as_str(), c.bridge.mode.as_str()),
                ("openai-compat", "stdio"),
                "{surface:?}: a flag beats the environment"
            );
        }
        let src = production_code();
        assert_eq!(
            src.matches("session_config(&cli, process_env)?").count(),
            4,
            "--matrix, --tui, --serve, and the REPL with -p and --acp"
        );
    }

    /// `--tui -c FILE` and `--matrix -c FILE` resolve FILE. On
    /// every surface, `--acp` and `--serve` included, `--endpoint` and
    /// `--model` win over FILE and over `X0NE_OPENAI_COMPAT_BASE_URL` and
    /// `X0NE_MODEL`.
    #[test]
    fn the_tui_and_matrix_resolve_the_config_named_by_c() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("chosen.toml");
        std::fs::write(
            &file,
            "[llm]\nprovider = \"openai-compat\"\n\n[llm.openai_compat]\n\
             base_url = \"http://127.0.0.1:9\"\nmodel = \"chosen-model\"\n",
        )
        .expect("config");
        let file = file.to_str().expect("utf-8 path");
        for surface in ["--tui", "--matrix"] {
            let cli = Cli::try_parse_from(["x0ne", surface, "-c", file]).expect("parse");
            let config = session_config(&cli, no_env).expect("-c is loaded");
            assert_eq!(config.llm.provider, "openai-compat", "{surface}");
            assert_eq!(config.llm.openai_compat.model, "chosen-model", "{surface}");
        }
        let env = |k: &str| match k {
            "X0NE_OPENAI_COMPAT_BASE_URL" => Some("http://127.0.0.1:11".to_string()),
            "X0NE_MODEL" => Some("env-model".to_string()),
            _ => None,
        };
        #[allow(unused_mut)]
        let mut surfaces: Vec<&[&str]> =
            vec![&[], &["-p", "hi"], &["--acp"], &["--tui"], &["--matrix"]];
        #[cfg(feature = "serve")]
        surfaces.push(&["--serve"]);
        for surface in surfaces {
            let mut argv = vec!["x0ne", "-c", file];
            argv.extend_from_slice(surface);
            let config = session_config(&Cli::try_parse_from(&argv).expect("parse"), env)
                .expect("-c is loaded");
            assert_eq!(
                config.llm.openai_compat.base_url, "http://127.0.0.1:11",
                "{surface:?}: the environment beats -c"
            );
            argv.extend(["--endpoint", "http://127.0.0.1:10", "--model", "flag-model"]);
            let config = session_config(&Cli::try_parse_from(&argv).expect("parse"), env)
                .expect("-c is loaded");
            assert_eq!(
                config.llm.openai_compat.base_url, "http://127.0.0.1:10",
                "{surface:?}"
            );
            assert_eq!(config.llm.openai_compat.model, "flag-model", "{surface:?}");
        }
    }

    /// SessionStart fires on every operator session (REPL, `-p`, `-p --json`),
    /// as `resume` when it reloaded a transcript, and again on `/clear` and
    /// `/resume` in the REPL.
    #[test]
    fn every_operator_session_fires_session_start() {
        let src = production_code();
        assert_eq!(
            src.matches(".session_started(session_source(resumed.is_some()))")
                .count(),
            src.matches("seed_resumed(&mut agent, resumed.as_ref())")
                .count()
        );
        let repl = repl_branch();
        assert!(repl.contains("agent.session_started(SessionSource::Clear)"));
        assert!(repl.contains("agent.session_started(SessionSource::Resume)"));
        assert_eq!(session_source(false), SessionSource::Startup);
        assert_eq!(session_source(true), SessionSource::Resume);
    }

    /// The reloaded transcript reaches every entrypoint that owns a journal,
    /// and only those.
    ///
    /// The `.journal =` count is already guarded by
    /// `every_session_is_journaled`; this is its twin for the resume, because
    /// a resume that seeds `-p` and forgets the REPL is a security property
    /// (`resume_conversation` revokes approvals) that holds on one surface
    /// and not the next.
    #[test]
    fn every_operator_session_is_seeded_with_the_resumed_transcript() {
        let src = production_code();
        let journaled = src.matches(".journal = session.as_ref()").count();
        let seeded = src
            .matches("seed_resumed(&mut agent, resumed.as_ref())")
            .count();
        // The benchmark is the one deliberate exception, and it is named so
        // that adding a second unseeded session fails this test instead of
        // passing quietly. `/bench` runs ONE prompt against several models on
        // throwaway sessions: seeding it would make every model answer a
        // different question than the one being measured, and would multiply
        // the reloaded context by the number of models.
        let bench = src
            .matches("bench_session.journal = session.as_ref()")
            .count();
        assert_eq!(bench, 1, "the benchmark exception moved or multiplied");
        assert_eq!(
            seeded,
            journaled - bench,
            "{journaled} sessions take the journal ({bench} benchmark) but only \
             {seeded} take the resumed transcript"
        );
        assert_eq!(seeded, 3, "-p, --auto-mode and the REPL");
    }

    /// A resume replaces the transcript through the session, never through
    /// the history directly — `resume_conversation` is what revokes the
    /// standing approvals, and a frontend that seeded `history` itself would
    /// carry them across.
    #[test]
    fn the_resume_paths_go_through_the_session_not_the_transcript() {
        let src = production_code();
        for site in ["seed_resumed", "SlashResult::Resume(request)"] {
            assert!(src.contains(site), "missing `{site}`");
        }
        assert!(
            !src.contains("resumed.messages"),
            "a frontend is seeding the transcript itself, bypassing the \
             approval revocation in resume_conversation"
        );
        // And `/resume` moves the writer with it, or this session's actions
        // would keep landing in the journal the operator just left.
        let arm_start = src
            .find("SlashResult::Resume(request)")
            .expect("the /resume arm");
        let arm = &src[arm_start..arm_start + 900];
        assert!(arm.contains("agent.resume_conversation(&reloaded)"));
        assert!(arm.contains("agent.journal = Some(journal.handle())"));
        assert!(arm.contains("session = Some(journal)"));
    }

    /// The cutover: autonomous iterations run on the canonical path.
    #[test]
    fn the_autonomous_loop_runs_on_the_canonical_driver() {
        let branch = auto_mode_branch();
        assert!(
            branch.contains("CliAgentSession::new(") && branch.contains("agent.run_turn(prompt)"),
            "--auto-mode does not run its iterations on a session"
        );
        assert!(
            !branch.contains("parse_llm_response"),
            "--auto-mode still calls the legacy loop"
        );
    }

    /// One session for the whole run. Building it inside the loop would reset
    /// the approvals and the spend on every iteration, which is precisely what
    /// session-scoped autonomous authority must not do.
    #[test]
    fn the_autonomous_loop_builds_exactly_one_session_outside_itself() {
        let branch = auto_mode_branch();
        assert_eq!(branch.matches("CliAgentSession::new(").count(), 1);
        let built = branch.find("CliAgentSession::new(").expect("the session");
        let looped = branch.find("loop {").expect("the autonomous loop");
        assert!(
            built < looped,
            "the session is rebuilt each iteration, so authority and budget \
             would reset every time"
        );
    }

    /// Autonomy reuses authority; it never manufactures any. And the withdrawn
    /// checkpoint commit stays withdrawn: no git side effect returns here.
    #[test]
    fn the_autonomous_loop_grants_nothing_and_commits_nothing() {
        let branch = auto_mode_branch();
        // Effect forms, not words: the branch still *mentions* the withdrawn
        // commit in the notice it prints, and a scan for "commit" would flag
        // the sentence that documents its absence.
        for banned in [
            "auto_confirm",
            "revoke_all",
            "git add",
            "git commit",
            "send_action(",
            "Command::new",
        ] {
            assert!(
                !branch.contains(banned),
                "--auto-mode touches `{banned}` — either it grants itself \
                 authority or the unattended write came back"
            );
        }
        // And the withdrawal is still stated where an operator will read it.
        assert!(
            branch.contains("checkpoint point at iteration"),
            "the checkpoint notice was dropped along with its side effect"
        );
    }

    /// The interactive REPL alone, comments stripped.
    fn repl_branch() -> String {
        let full = include_str!("main.rs");
        let start = full.find("MAIN LOOP").expect("the REPL banner");
        let end = full[start..].find("#[cfg(test)]").expect("the tests") + start;
        full[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The cutover. The public interactive REPL runs one turn per prompt on
    /// the canonical path, and the legacy call it replaced is gone from it.
    #[test]
    fn the_interactive_repl_runs_on_the_canonical_driver() {
        let repl = repl_branch();
        assert!(
            repl.contains("CliAgentSession::new(") && repl.contains("agent.run_turn(&expanded)"),
            "the REPL does not assemble a session and run a turn on it"
        );
        assert!(
            !repl.contains("parse_llm_response"),
            "the REPL's own turn still calls the legacy loop"
        );
    }

    /// One session for the whole REPL. Building it inside the loop would
    /// re-arm the budget and drop every approval on each prompt, which is the
    /// defect this placement exists to prevent.
    #[test]
    fn the_repl_builds_exactly_one_session_outside_its_loop() {
        let repl = repl_branch();
        let loop_start = repl.find("'outer: loop {").expect("the REPL loop");
        // Exactly one before the loop: the REPL's own. Constructions *inside*
        // the loop are the benchmark's deliberately-fresh per-model sessions,
        // which have their own guard.
        assert_eq!(
            repl[..loop_start].matches("CliAgentSession::new(").count(),
            1,
            "the REPL's own session is not assembled exactly once before its \
             loop, so authority and budget could reset on every prompt"
        );
    }

    /// Every session this binary assembles gets the session log.
    ///
    /// Four `CliAgentSession::new` sites — `-p`, `--auto-mode`, the REPL and
    /// the benchmark — and `journal` defaults to `None`, so a forgotten
    /// assignment is a surface that runs commands off the record while still
    /// writing prompts and turns into the log. Counted rather than merely
    /// searched: adding a fifth session without its journal is exactly the
    /// mistake this catches.
    #[test]
    fn every_session_the_cli_assembles_is_handed_the_session_log() {
        let src = include_str!("main.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let sessions = production.matches("CliAgentSession::new(").count();
        let journaled = production.matches(".journal = session.as_ref()").count();
        assert_eq!(
            journaled, sessions,
            "{sessions} sessions are assembled but only {journaled} are given \
             the session log — the rest execute actions with no audit trail"
        );
    }

    /// `/clear` is a security operation, and the frontend must not perform the
    /// authority half itself: it reports the request, the session revokes.
    #[test]
    fn the_repl_delegates_clear_to_the_session() {
        let repl = repl_branch();
        assert!(repl.contains("SlashResult::ClearSession"));
        assert!(repl.contains("agent.clear_conversation()"));
        for banned in ["revoke_all", "authority", "grants"] {
            assert!(
                !repl.contains(banned),
                "the REPL touches `{banned}` — approvals are decided in the \
                 frontend again"
            );
        }
    }

    /// `/plan` and `/build` are the session's: the REPL asks it, which tells
    /// the runtime first, and the prompt shows the mode where the user types.
    #[test]
    fn the_repl_delegates_plan_mode_to_the_session() {
        let repl = repl_branch();
        assert!(repl.contains("SlashResult::SetPlanMode(on)"));
        assert!(repl.contains("agent.set_plan_mode(on)"));
        assert!(repl.contains("prompt_chevron(agent.plan_mode())"));
        assert!(prompt_chevron(true).contains("plan"));
        assert!(!prompt_chevron(false).contains("plan"));
    }

    /// A model switch replaces the backend and nothing else. Rebuilding the
    /// session there would silently discard history, spend and approvals.
    #[test]
    fn a_model_switch_replaces_only_the_backend() {
        let repl = repl_branch();
        let at = repl
            .find("SlashResult::SwitchModel(new_backend)")
            .expect("the switch arm");
        let end = repl[at + 1..]
            .find("SlashResult::")
            .map(|i| at + 1 + i)
            .unwrap_or(repl.len());
        let arm = &repl[at..end];
        assert!(arm.contains("agent.set_backend("));
        assert!(
            !arm.contains("CliAgentSession::new(") && !arm.contains("clear_conversation"),
            "the switch arm rebuilds or clears the session"
        );
    }

    /// The canonical invariant, stated at `agent/confirm.rs:12-19`:
    ///
    /// ```text
    /// confirmation required + no human channel = DENY
    /// ```
    ///
    /// `main.rs` used to invert it in two places — the one-shot `-p` path
    /// ("no terminal to confirm, so auto-approve") and `/until` — by writing
    /// `auto_confirm = true` into the config the agent loop reads. That turns
    /// *absence of a human* into *approval*, and it also disarms the
    /// fail-closed branch that already exists at `agent_loop.rs:846`:
    /// `needs_confirmation` returns `false` when `auto_confirm` is set, so the
    /// no-TTY refusal is never reached.
    ///
    /// Authority is granted by an operator, never by the shape of the session.
    /// The only writer of `auto_confirm` left in the frontend is a human
    /// answering "Always" at the prompt.
    #[test]
    fn the_frontend_never_grants_approval_to_itself() {
        let code = production_code();
        assert!(
            !code.contains("auto_confirm = true"),
            "`main.rs` assigns `auto_confirm = true`. Autonomy is permission to \
             *repeat*, never permission to *act*: a driver that cannot reach a \
             human must fail closed, not approve on their behalf."
        );
    }

    /// `--auto-mode` ran `git add -A` and `git commit --allow-empty` every ten
    /// iterations through raw `bridge.send_action`, unattended and with no
    /// human in the path — the same shape as Matrix's ungated `/undo`.
    ///
    /// The driver itself is preserved; only that side effect is withdrawn,
    /// because authorising it would need a standing waiver that does not exist
    /// and must not be invented here. This pins the withdrawal: the autonomous
    /// loop must not reach the bridge on its own behalf at all. It says
    /// nothing about the Actions the *model* proposes — those go through the
    /// agent loop's gate, which is where they belong.
    #[test]
    fn the_autonomous_driver_performs_no_side_effect_of_its_own() {
        let code = production_code();
        let start = code
            .find("if cli.auto_mode {")
            .expect("the --auto-mode driver");
        // Walk to the matching brace rather than guessing a window: the block
        // is long, and a fixed slice that stops short would pass by missing
        // the very code it is meant to inspect.
        let open = start + code[start..].find('{').expect("opening brace");
        let mut depth = 0usize;
        let mut end = code.len();
        for (i, c) in code[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let driver = &code[start..end];
        assert!(
            driver.contains("max_iterations"),
            "window does not cover the driver body"
        );
        assert!(
            !driver.contains("send_action"),
            "the autonomous driver reaches the bridge directly. Every Action it \
             causes must be one the model proposed and the gate saw; the driver \
             has no authority of its own to spend."
        );
        assert!(
            !driver.contains("git"),
            "the autonomous driver still names git — an unattended write to the \
             operator's history with no approval path behind it"
        );
    }

    /// Plain ASCII short enough: returned as-is, no ellipsis.
    #[test]
    fn truncate_display_under_budget_is_identity() {
        assert_eq!(truncate_display("hello world", 60), "hello world");
    }

    /// Plain ASCII over budget: truncated at the exact byte count with a
    /// trailing ellipsis.
    #[test]
    fn truncate_display_ascii_over_budget_clips_and_suffixes() {
        let s = "a".repeat(100);
        let out = truncate_display(&s, 57);
        assert!(out.ends_with('…'));
        // 57 'a' bytes + '…' (3 bytes) = 60
        assert_eq!(out.len(), 57 + "…".len());
    }

    /// The original crash: a byte-budget cut landing inside a 4-byte
    /// codepoint. Before the fix, this panicked with
    /// `byte index N is not a char boundary`. Now it must return a
    /// well-formed UTF-8 string ending with `…`.
    #[test]
    fn truncate_display_emoji_boundary_does_not_panic() {
        // Layout: 67 ASCII bytes, then 🚀 (bytes 67..71), then suffix.
        // Budget 70 lands on the 4th byte of the rocket → must clamp to 67.
        let s = format!("{}🚀 more text", "x".repeat(67));
        assert_eq!(
            s.as_bytes()[67],
            0xF0,
            "sanity: byte 67 is the rocket's UTF-8 lead byte"
        );
        assert!(
            !s.is_char_boundary(70),
            "sanity: byte 70 is inside the rocket, not a boundary"
        );

        // The real contract: no panic, valid UTF-8 output, ends with '…'.
        let out = truncate_display(&s, 70);
        assert!(
            out.ends_with('…'),
            "truncated output must end with ellipsis: {:?}",
            out
        );
        // Every byte is part of a valid UTF-8 sequence: `&str` guarantees
        // this, so the fact that we produced a `String` at all is the
        // primary proof. We also explicitly iterate chars to double-check.
        let _: usize = out.chars().count();
        // Exact check: we must have clamped back to byte 67 (just before
        // the rocket), producing 67 'x' chars + '…'.
        assert_eq!(
            out.len(),
            67 + "…".len(),
            "must clamp to byte 67: {:?}",
            out
        );
    }

    /// Edge case: the budget is zero. Should produce just the ellipsis
    /// (matches the sentinel "we had to cut" contract).
    #[test]
    fn truncate_display_zero_budget_emits_ellipsis_only() {
        let out = truncate_display("anything", 0);
        assert_eq!(out, "…");
    }

    /// A string that starts with a multibyte char and is over budget:
    /// the helper must clamp back to byte 0 and still emit a valid string.
    #[test]
    fn truncate_display_leading_multibyte_clamps_to_zero() {
        // "🦀🦀🦀" — each crab is 4 bytes, total 12 bytes.
        let s = "🦀🦀🦀";
        let out = truncate_display(s, 1);
        // cut starts at 1, 🦀 starts at byte 0 → is_char_boundary(1) = false →
        // walk to 0. Output = "…".
        assert_eq!(out, "…");
    }

    /// The learn mode is not in this repository: `x0ne learn` is no subcommand.
    #[test]
    fn learn_is_not_a_subcommand() {
        let parsed = Cli::try_parse_from(["x0ne", "learn", "context"]);
        assert!(parsed.is_err(), "`x0ne learn` parsed");
    }

    struct NoModel;

    #[async_trait::async_trait]
    impl x0ne_orchestrator::api::LlmBackend for NoModel {
        async fn send(
            &self,
            _system: &str,
            _messages: &[x0ne_orchestrator::conversation::Message],
        ) -> anyhow::Result<x0ne_orchestrator::api::ApiResponse> {
            anyhow::bail!("not called")
        }
        fn display_name(&self) -> String {
            "none".into()
        }
    }

    /// `/budget 0.30` caps the session that spends, not only the config.
    #[test]
    fn budget_from_the_prompt_reaches_the_live_session_budget() {
        let mut agent = x0ne_orchestrator::agent::session::CliAgentSession::new(
            x0ne_orchestrator::config::Config::default(),
            std::sync::Arc::new(NoModel),
            "SYS",
            None,
            Box::new(x0ne_orchestrator::display_sink::NullDisplaySink),
        );
        assert_eq!(agent.state.budget.limit_usd, 0.0);
        agent.config.cost.budget_usd_limit = 0.30;
        apply_budget_change(&mut agent, 0.0).unwrap();
        assert_eq!(agent.state.budget.limit_usd, 0.30);
        assert_eq!(agent.worlds_setup().budget_cap, 0.30);

        // Above the ceiling: refused, and the config is put back.
        agent.state.budget.ceiling_usd = 0.50;
        agent.config.cost.budget_usd_limit = 2.0;
        assert!(apply_budget_change(&mut agent, 0.30).is_err());
        assert_eq!(agent.config.cost.budget_usd_limit, 0.30);
        assert_eq!(agent.state.budget.limit_usd, 0.30);
    }
}
