use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing::info;

use x0ne_runtime::config::{self, AppConfig};
use x0ne_runtime::handles::new_shared;
use x0ne_runtime::mcp;
use x0ne_runtime::policy::PolicyEngine;
use x0ne_runtime::server::{self, ServerState};

/// x0ne-runtime, secure local capability runtime for AI agents.
#[derive(Parser, Debug)]
#[command(
    name = "x0ne-runtime",
    version,
    about = "Secure local capability runtime for AI agents",
    long_about = None,
)]
struct Cli {
    #[arg(long, short, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long, value_name = "MODE")]
    mode: Option<String>,
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,
    #[arg(long, env = "X0NE_LOG", default_value = "info")]
    log_level: String,

    /// Assert that the component launching this runtime front-ends it and
    /// owns the human confirmation gate.
    ///
    /// SECURITY: this is the ONLY thing that re-authorizes `policy.mode =
    /// "auto"` under `--mode mcp`. `x0ne-orchestrator --mcp
    /// --downstream-runtime` passes it because it does hold the gate; an MCP
    /// host wiring `x0ne-runtime` directly must not, and gets a refusal
    /// instead of an unattended shell. See
    /// `AppConfig::check_headless_posture`.
    #[arg(long)]
    upstream_human_gate: bool,

    /// How commands the agent starts are confined. `bwrap` (default):
    /// bubblewrap, refused when it cannot run. `container`: this runtime already
    /// runs inside an Incus/Podman sandbox. `none`: run them directly on this
    /// host.
    ///
    /// SECURITY: a flag only — no `env =`, no TOML key. A project can supply
    /// the runtime's TOML and a shell can inherit variables; neither may turn
    /// the confinement off.
    #[arg(long, value_enum, default_value = "bwrap")]
    isolation: IsolationArg,

    /// Any program passes the command allowlist. Refused unless `--isolation
    /// container` runs in a detected container: the bench-agentic profile,
    /// never a user default. Flag only, like `--isolation`.
    #[arg(long)]
    allow_all_commands: bool,

    /// Network of isolated commands: `off` (default, loopback only) or `host`.
    /// Flag only, for the same reason as `--isolation`.
    #[arg(long, value_enum, default_value = "off")]
    isolation_network: NetworkArg,

    /// Run the built-in self-test / diagnostic and exit.  Reports which
    /// primitive families are compiled into the binary and probes each one
    /// with a tiny smoke test.  Exit code is 0 if every active family
    /// passed, 1 otherwise.
    ///
    /// Always available in debug builds. In release, requires
    /// `--features self_test` at build time.
    #[cfg(any(debug_assertions, feature = "self_test"))]
    #[arg(long)]
    self_test: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum IsolationArg {
    Bwrap,
    Container,
    None,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum NetworkArg {
    Off,
    Host,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv
        .get(1)
        .map(|a| a == x0ne_runtime::executor::sandbox_child::ARG)
        == Some(true)
    {
        x0ne_runtime::executor::sandbox_child::main(&argv[2..]);
    }
    if let Ok(me) = std::env::current_exe() {
        x0ne_runtime::executor::isolation::register_confine_helper(me);
    }
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                cli.log_level
                    .parse()
                    .unwrap_or_else(|_| "info".parse().unwrap())
            }),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    info!("x0ne-runtime v{} starting", env!("CARGO_PKG_VERSION"));

    // Honor X0NE_SCRIPTED_ANSWERS for non-interactive testing (self-test).
    // Real stdin is owned by the bridge protocol, so interactive primitives
    // rely on canned answers seeded via env.
    //
    // Gated with the same predicate as the self-test entry point twelve lines
    // below. It was not, and the asymmetry was the defect: a release binary
    // shipped the answer source without the feature that needed it, so an
    // environment variable could answer AskHuman, Confirm and
    // AskMultipleChoice on a human's behalf. The call is compiled out here and
    // the function does not exist there, so this is a compile-time absence
    // rather than a check that could later be skipped.
    #[cfg(any(debug_assertions, feature = "self_test"))]
    if x0ne_runtime::executor::human::install_scripted_from_env() {
        info!("X0NE_SCRIPTED_ANSWERS honored — human prompts will use canned answers");
    }

    // ── Self-test short-circuit ──────────────────────────────────────────────
    // Runs the diagnostic and exits without starting any server. Only
    // compiled when the corresponding cfg/feature is on (see `Cli`).
    #[cfg(any(debug_assertions, feature = "self_test"))]
    if cli.self_test {
        let report = x0ne_runtime::self_test::run_self_test().await;
        print!("{}", report.render());
        std::process::exit(if report.all_passed() { 0 } else { 1 });
    }

    // No policy file, or one that cannot be honoured: no start.
    let (mut app_config, config_source) = AppConfig::find_and_load_with_source(cli.config.as_ref())
        .map_err(|e| anyhow::anyhow!("x0ne-runtime: refusing to start: {e:#}"))?;
    // The orchestrator asks us which file to persist a grant into rather than
    // recomputing the search order; this is where the answer is recorded.
    config::record_loaded_config_path(&config_source);

    if let Some(mode) = &cli.mode {
        app_config.runtime.mode = mode.clone();
    }
    if let Some(bind) = &cli.bind {
        let mut parts = bind.splitn(2, ':');
        let host = parts.next().unwrap_or("127.0.0.1").to_string();
        let port: u16 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(7878);
        app_config.runtime.http = Some(config::HttpConfig {
            host,
            port,
            auth_token: None,
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
        });
    }

    app_config.isolation.mode = match cli.isolation {
        IsolationArg::Bwrap => config::IsolationMode::Bwrap,
        IsolationArg::Container => config::IsolationMode::Container,
        IsolationArg::None => config::IsolationMode::None,
    };
    app_config.isolation.network = match cli.isolation_network {
        NetworkArg::Off => config::IsolationNetwork::Off,
        NetworkArg::Host => config::IsolationNetwork::Host,
    };
    app_config.policy.allow_all_commands = cli.allow_all_commands;
    app_config.check_allow_all_commands()?;
    if app_config.policy.allow_all_commands {
        eprintln!("x0ne-runtime: --allow-all-commands (bench-agentic): every program passes the command check");
    }

    // SECURITY: say out loud which policy is being enforced, and say it on
    // stderr rather than through `tracing`. The MCP entries of
    // `make claude-add` and `make codex-add` pass `--log-level error`, which
    // filters INFO and WARN — so the existing
    // "loading config from …" line was invisible in precisely the deployment
    // that turned out to be wide open (audit 2026-08-15). A runtime nobody can
    // interrogate about its own policy is a runtime nobody can audit; this
    // line is that answer, unconditional and one per process.
    eprintln!(
        "x0ne-runtime: mode={} policy={} (mode={}, default_effect={})",
        app_config.runtime.mode,
        config_source,
        app_config.policy.mode,
        app_config.policy.default_effect
    );

    // Say what the empty lists in that policy actually mean, once, next to the
    // line that named the file. Since the deny-all change, an empty allowlist denies
    // everything; before it, it allowed everything. The operator of
    // 2026-08-15 met the new semantics one refusal at a time and concluded the
    // approval gate was broken. Warnings only — a runtime with no commands is
    // a legitimate posture, so nothing here refuses to start.
    app_config.emit_policy_warnings(&mut std::io::stderr());
    eprintln!(
        "x0ne-runtime: isolation={}",
        x0ne_runtime::executor::isolation::describe(&app_config.isolation)
    );

    // SECURITY: a headless MCP endpoint may not run a policy that can never
    // ask. Checked AFTER `--mode` is folded in, because `--mode mcp` on the
    // command line is exactly how an MCP client's entry gets there, and BEFORE any
    // server starts, so a refusal means no session ever existed.
    app_config.check_headless_posture(cli.upstream_human_gate)?;

    // SECURITY: hand the operator's `policy.env_passthrough` to the child-env
    // gate before anything can spawn. Skipping this call cannot open a hole —
    // the slot defaults to empty, which is the strict list — but it would
    // silently ignore a passthrough the operator did configure, so it belongs
    // here, next to the config that carries it, and not deeper.
    x0ne_runtime::child_env::install_passthrough(&app_config.policy.env_passthrough);

    info!(mode = %app_config.runtime.mode, "Runtime mode");

    // Build policy engine (takes full AppConfig)
    let policy_engine = PolicyEngine::new(app_config.clone(), app_config.actions.clone());

    // Build shared handle manager
    let handles = new_shared(
        app_config.events.max_events_per_handle,
        app_config.handles.completed_retention_secs,
        app_config.handles.max_concurrent,
    );

    // Desktop rate limiter
    let rate_limiter = Arc::new(tokio::sync::Mutex::new(
        std::time::Instant::now() - std::time::Duration::from_secs(60),
    ));

    // Composite primitives store — load persisted definitions from disk
    let composites = x0ne_runtime::composite::CompositeStore::new(&app_config);
    composites.load_from_disk().await;

    // The HTTP mode always has a token; generated (0600 file) if absent.
    if app_config.runtime.mode == "http" {
        server::ensure_http_token(&mut app_config)?;
    }

    let terms = handles.clone();
    let state = ServerState {
        config: Arc::new(app_config.clone()),
        policy: Arc::new(policy_engine),
        handles,
        rate_limiter,
        composites,
    };

    // Boot the in-process artifact serving HTTP app. It listens on
    // 127.0.0.1:7900 (host) or 0.0.0.0:7900 (sandbox, so the host orchestrator
    // can reach it via the container bridge IP). Fail-soft: if the port is
    // busy, log + continue — artifact creation still works, the URLs simply
    // won't resolve until the user fixes the conflict.
    match x0ne_runtime::artifact_serve::spawn().await {
        Ok(addr) => info!(%addr, "artifact serve up"),
        Err(e) => tracing::warn!("artifact serve disabled: {}", e),
    }

    if app_config.runtime.mode != "http" {
        x0ne_runtime::executor::human::mark_stdin_as_protocol();
    }
    let served = match app_config.runtime.mode.as_str() {
        "http" => server::serve_http(state).await,
        "mcp" => {
            let registry = mcp::registry::ToolRegistry::new();
            tracing::info!("{} MCP tools registered", registry.tool_count());
            let mut server = mcp::server::McpServer::new(registry, state);
            server.run().await
        }
        "stdio" | _ => server::serve_stdio(state).await,
    };

    // Terminals end with the session; their recordings stay.
    let terms = terms.lock().await.terms.clone();
    terms.close_all();
    if let Some(dir) = terms.recordings_dir() {
        eprintln!("x0ne-runtime: terminal recordings in {}", dir.display());
    }
    served?;
    Ok(())
}
