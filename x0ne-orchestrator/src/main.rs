//! X0NE Orchestrator — headless entry point.
//!
//! This binary (`x0ne-orchestrator`) provides two non-interactive entry modes:
//!   --serve                  HTTP/SSE service for browser/UI clients
//!   --mcp                    MCP (JSON-RPC over stdio) for LLM tool clients
//!                            like Claude Code, Cline, Continue
//!
//! All interactive CLI/TUI/REPL functionality lives in the `x0ne-code` crate.
//!
//! Usage:
//!   x0ne-orchestrator --serve                    # HTTP/SSE on 127.0.0.1:7700
//!   x0ne-orchestrator --serve --bind 0.0.0.0:80  # Custom bind address
//!   x0ne-orchestrator --serve --serve-token XXX  # Bearer token auth
//!   x0ne-orchestrator --mcp                      # MCP stdio server (Phase 2)

use std::path::PathBuf;

#[cfg(feature = "serve")]
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
#[cfg(feature = "serve")]
use tracing::warn;

#[cfg(feature = "serve")]
use x0ne_orchestrator::artifact_local_proxy;
use x0ne_orchestrator::config;
#[cfg(feature = "serve")]
use x0ne_orchestrator::container_runtime;
use x0ne_orchestrator::mcp_server::{DownstreamRuntime, OrchestratorMcpServer};
#[cfg(feature = "serve")]
use x0ne_orchestrator::serve;
use x0ne_orchestrator::sysaicalls;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[allow(dead_code)]
#[command(
    name = "x0ne-orchestrator",
    version,
    about = "X0NE orchestrator — headless HTTP/SSE/MCP service",
    long_about = None
)]
struct ServerCli {
    #[arg(long, short, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long)]
    mode: Option<String>,
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,
    #[arg(long, env = "X0NE_LOG", default_value = "warn")]
    log_level: String,

    /// Run as an MCP (JSON-RPC over stdio) server. Mutually exclusive with --serve.
    /// In this mode the orchestrator does not produce any human-readable output
    /// on stdout — only JSON-RPC frames. Logs go to stderr.
    #[arg(long, conflicts_with = "serve")]
    mcp: bool,

    /// Path to the x0ne-runtime binary to spawn as the downstream MCP server.
    /// Default: looks up "x0ne-runtime" on PATH. Pass "none" to disable.
    #[arg(long, value_name = "PATH", default_value = "x0ne-runtime")]
    downstream_runtime: String,

    /// Optional config file passed to the downstream runtime. e.g. point at a
    /// hardened readonly profile to give MCP clients a sandboxed surface.
    #[arg(long, value_name = "FILE")]
    downstream_config: Option<PathBuf>,

    /// Optional command prefix prepended before the downstream runtime binary.
    /// Useful to run the runtime inside an Incus / Podman / Docker container
    /// or over SSH. Whitespace-split. Example:
    ///   --downstream-prefix "incus exec x0ne-sandbox --"
    /// produces: incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode mcp ...
    #[arg(long, value_name = "STR")]
    downstream_prefix: Option<String>,

    /// Run as an HTTP/SSE service.
    #[cfg(feature = "serve")]
    #[arg(long)]
    serve: bool,

    /// Address to bind the HTTP listener (default: 127.0.0.1:7700).
    #[cfg(feature = "serve")]
    #[arg(long, value_name = "HOST:PORT", default_value = "127.0.0.1:7700")]
    bind: String,
    /// Bearer token for the serve API. If unset, one is generated into a 0600
    /// file whose path is printed at startup.
    #[cfg(feature = "serve")]
    #[arg(long, env = "X0NE_SERVE_TOKEN")]
    serve_token: Option<String>,
}

/// Every MCP tool call runs in the downstream runtime. Under
/// `X0NE_REQUIRE_SANDBOX` it must provably run in a container, checked
/// before the artifact router or the runtime starts.
fn require_sandboxed_downstream(cli: &ServerCli) -> Result<()> {
    use x0ne_orchestrator::sandbox_mode::{current, refusal, sandbox_required, uncontained_reason};
    if !cli.mcp || cli.downstream_runtime == "none" || !sandbox_required() {
        return Ok(());
    }
    let prefix = cli
        .downstream_prefix
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let why = match prefix {
        Some(p) => {
            let cmd = format!("{p} {}", cli.downstream_runtime);
            uncontained_reason("sandbox", Some(&cmd), current()).map(|_| {
                format!("--downstream-prefix {p:?} is not `incus|podman|docker exec <container>`")
            })
        }
        None => uncontained_reason("stdio", None, current()).map(|_| {
            "the MCP downstream runtime starts on this host; \
             --downstream-prefix \"incus exec x0ne-sandbox --\" puts it in a container"
                .to_string()
        }),
    };
    match why {
        Some(why) => anyhow::bail!(refusal(&why)),
        None => Ok(()),
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Same rule as `x0ne`: only the user's secrets file, first.
    // `./.env` belonged to the project and set X0NE_PLAN_AUTO_APPROVE (2026-09-23).
    let _ = dotenvy::from_path(x0ne_orchestrator::setup_menu::secrets_path());
    let cli = ServerCli::parse();

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

    sysaicalls::announce_plan_auto_approve_if_set();
    require_sandboxed_downstream(&cli)?;

    // Resolve the artifact-router upstream BEFORE booting the router (the
    // router reads X0NE_RUNTIME_UPSTREAM once on startup). When the
    // downstream runtime lives in a sandbox container, the orchestrator
    // must reach it via the container's bridge IP, not host loopback.
    #[cfg(feature = "serve")]
    {
        if std::env::var("X0NE_RUNTIME_UPSTREAM").is_err() {
            let upstream = resolve_runtime_upstream(&cli).await;
            std::env::set_var("X0NE_RUNTIME_UPSTREAM", &upstream);
            tracing::info!(upstream = %upstream, "artifact router upstream resolved");
        }
        // Boot the user-facing artifact router on `127.0.0.1:7800`. Reverse-
        // proxies all `/a/<id>/...` to the runtime — artifact files never
        // leave the runtime's trust domain (host or sandbox). Fail-soft.
        if let Err(e) = artifact_local_proxy::ensure_started().await {
            tracing::warn!("artifact router could not start: {}", e);
        }
    }

    // ── MCP mode (gateway) ────────────────────────────────────────────────────
    if cli.mcp {
        let mut config = config::Config::find_and_load(cli.config.as_ref())?;
        if let Some(mode) = cli.mode.clone() {
            config.bridge.mode = mode;
        }
        if let Some(provider) = cli.provider.clone() {
            config.llm.provider = provider;
        }

        let mut server = OrchestratorMcpServer::new(config);
        if cli.downstream_runtime != "none" {
            let mut extra_args: Vec<String> = vec![];
            if let Some(cfg_path) = cli.downstream_config.as_ref() {
                extra_args.push("--config".into());
                extra_args.push(cfg_path.to_string_lossy().to_string());
            }
            let prefix: Vec<String> = cli
                .downstream_prefix
                .as_ref()
                .map(|s| s.split_whitespace().map(|t| t.to_string()).collect())
                .unwrap_or_default();
            let downstream = DownstreamRuntime {
                bin: cli.downstream_runtime.clone(),
                extra_args,
                prefix,
            };
            server = server.with_downstream(downstream).await;
        }
        // Optional: spawn x0ne-vm-mcp as a 2nd downstream so vm_* tools show
        // up in the unified toolbelt. Trigger via env var to keep the default
        // light (libvirt is not always present).
        if std::env::var("X0NE_VM_DOWNSTREAM")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false)
        {
            let bin =
                std::env::var("X0NE_VM_MCP_BIN").unwrap_or_else(|_| "x0ne-vm-mcp".to_string());
            server = server.with_vm_downstream(&bin).await;
        }
        return server.run().await;
    }

    #[cfg(feature = "serve")]
    {
        if !cli.serve {
            anyhow::bail!(
                "x0ne-orchestrator: pass --serve (HTTP/SSE) or --mcp (MCP stdio). See --help."
            );
        }
        let bind: std::net::SocketAddr = cli
            .bind
            .parse()
            .with_context(|| format!("invalid --bind address: {}", cli.bind))?;
        // The file, the environment, then `--mode` and `--provider`.
        let mut config = config::Config::resolve(
            cli.config.as_ref(),
            |k| std::env::var(k).ok(),
            cli.mode.as_deref(),
            cli.provider.as_deref(),
            config::ModelFlags::default(),
        )?;
        // Force http bridge in serve mode — stdio/sandbox don't make sense here.
        if config.bridge.mode != "http" {
            warn!(
                "serve mode forces bridge.mode = http (was {:?})",
                config.bridge.mode
            );
            config.bridge.mode = "http".into();
        }
        return serve::run(config, bind, cli.serve_token).await;
    }

    #[cfg(not(feature = "serve"))]
    {
        anyhow::bail!("x0ne-orchestrator: --serve was not compiled in (built --no-default-features). Use --mcp, or rebuild with: cargo build -p x0ne-orchestrator --features serve");
    }
}

/// Decide where the artifact router should forward `/a/<id>/...` requests.
///
/// - No downstream prefix or non-container prefix → `127.0.0.1:7900`
///   (runtime spawns directly on the host as a child process).
/// - Sandbox prefix (`incus exec ...` / `podman exec ...` / `docker exec ...`)
///   → query the backend for the container's bridge IPv4 and use that.
///
/// Best-effort: if backend detection / IP query fails (e.g. backend CLI
/// missing, container not yet routable), we log a warning and fall back
/// to loopback. Operators can always override with `X0NE_RUNTIME_UPSTREAM`.
#[cfg(feature = "serve")]
async fn resolve_runtime_upstream(cli: &ServerCli) -> String {
    use artifact_local_proxy::DEFAULT_RUNTIME_UPSTREAM;
    let Some(prefix) = cli.downstream_prefix.as_deref() else {
        return DEFAULT_RUNTIME_UPSTREAM.to_string();
    };
    let Some((rt, target)) = container_runtime::detect_from_sandbox_cmd(prefix) else {
        return DEFAULT_RUNTIME_UPSTREAM.to_string();
    };
    match container_runtime::container_ipv4(&target, rt.backend()).await {
        Ok(ip) => format!("{}:7900", ip),
        Err(e) => {
            tracing::warn!(
                "container IP detection failed for {} ({:?}): {} — falling back to loopback. \
                 Override with X0NE_RUNTIME_UPSTREAM=<host>:<port>.",
                target,
                rt.backend(),
                e
            );
            DEFAULT_RUNTIME_UPSTREAM.to_string()
        }
    }
}
