// Config fields marked as dead_code are intentional — they are read from TOML
// and reserved for future executor use (e.g. max_log_lines, screenshot_confirm).
#![allow(dead_code)]
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Environment variable that re-enables the working-directory config.
///
/// SECURITY: development convenience only. See [`AppConfig::find_and_load_with_source`].
pub const ALLOW_CWD_CONFIG_ENV: &str = "X0NE_RUNTIME_ALLOW_CWD_CONFIG";

/// Is `./x0ne-runtime.toml` allowed to act as the policy?
///
/// Opt-in is exact: `1`. Not "any non-empty value" — `X0NE_RUNTIME_ALLOW_CWD_CONFIG=0`
/// must not turn the door back on, and an operator who exports the name to
/// document it should not be surprised.
fn cwd_config_is_allowed() -> bool {
    std::env::var(ALLOW_CWD_CONFIG_ENV).is_ok_and(|v| v == "1")
}

/// Where the policy the runtime is enforcing actually came from.
///
/// SECURITY: this exists to be *printed*. A runtime that cannot say which
/// file it is obeying cannot be audited, and the 2026-08-15 finding lived
/// entirely in that blind spot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config <path>`: the operator named it.
    Explicit(PathBuf),
    /// `./x0ne-runtime.toml`, honored only under the explicit opt-in.
    Cwd(PathBuf),
    /// `$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml` — what `make install` deposits.
    Xdg(PathBuf),
    /// `$XDG_CONFIG_HOME/x0ne-runtime/config.toml` — pre-0.2 layout.
    LegacyXdg(PathBuf),
}

impl ConfigSource {
    /// The file backing this policy. There always is one: without a file
    /// the runtime does not start.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::Explicit(p)
            | ConfigSource::Cwd(p)
            | ConfigSource::Xdg(p)
            | ConfigSource::LegacyXdg(p) => Some(p.as_path()),
        }
    }
}

/// The policy file this process actually loaded, once it is known.
///
/// SECURITY / why a global: the orchestrator persists a grant into the
/// operator's `x0ne-runtime.toml`, and the ONLY party that knows which file
/// that is, is this process. Both sides derive the same search convention
/// independently (`find_and_load_with_source` here, `setup_menu::
/// runtime_config_path` there) and they diverge the moment `--config`, the
/// legacy XDG location or the cwd opt-in is in play. The orchestrator
/// therefore asks rather than computes; this is the answer. `None` means a
/// process that never recorded one (test binaries): there is no file to write
/// and persistence is refused rather than inventing one.
static LOADED_CONFIG_PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Record which file this process is obeying. Called once, from `main`.
pub fn record_loaded_config_path(source: &ConfigSource) {
    let _ = LOADED_CONFIG_PATH.set(source.path().map(|p| p.to_path_buf()));
}

/// The policy file this process loaded, as a string, or `None` for compiled
/// defaults / a runtime that never recorded one (every test binary).
pub fn loaded_config_path() -> Option<String> {
    LOADED_CONFIG_PATH
        .get()
        .and_then(|p| p.as_ref())
        .map(|p| p.to_string_lossy().to_string())
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Explicit(p) => write!(f, "{} (--config)", p.display()),
            ConfigSource::Cwd(p) => write!(f, "{} (working directory, opted in)", p.display()),
            ConfigSource::Xdg(p) => write!(f, "{}", p.display()),
            ConfigSource::LegacyXdg(p) => write!(f, "{} (legacy path)", p.display()),
        }
    }
}

/// Top-level TOML configuration for x0ne-runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub actions: HashMap<String, bool>,
    #[serde(default)]
    pub executor: ExecutorConfig,
    #[serde(default)]
    pub snapshots: SnapshotConfig,
    #[serde(default)]
    pub handles: HandlesConfig,
    #[serde(default)]
    pub events: EventsConfig,
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub desktop: DesktopConfig,
    #[serde(default)]
    pub composites: CompositeConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub isolation: IsolationConfig,
    #[serde(default)]
    pub term: TermConfig,
}

// ── Runtime ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig {
    pub mode: String,
    pub http: Option<HttpConfig>,
    /// Which setup-wizard profile wrote this file (`developer`, `readonly`,
    /// `full`, or `custom` for "keep what I have"). `None` means nobody
    /// chose: the file is the shipped reference, or hand-written.
    ///
    /// This key is what arms the profile wizard. Until 2026-09-11 the wizard
    /// keyed on the FILE's existence, and `make install` deposits the
    /// reference file — so on the documented install path the profile
    /// question was never asked, and every operator inherited
    /// `default_effect = "allow"` and `[desktop] enabled = true` without
    /// having said so (audit 04-F02). The runtime itself does not read it.
    #[serde(default)]
    pub profile: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            mode: "stdio".into(),
            http: None,
            profile: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_host")]
    pub host: String,
    #[serde(default = "default_http_port")]
    pub port: u16,
    /// Bearer token for the HTTP API. Always required: when absent,
    /// one is generated at startup and written to a 0600 file whose path is
    /// printed on stderr.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Browser origins allowed to call the API. Empty by default: any request
    /// carrying another `Origin` (or cross-site fetch metadata) gets 403.
    #[serde(default = "default_cors_origins")]
    pub allowed_origins: Vec<String>,
    /// Extra accepted `Host` values (`name` or `name:port`), beyond
    /// 127.0.0.1 / localhost / `[::1]` with the bound port. Anti DNS rebinding.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

fn default_http_host() -> String {
    "127.0.0.1".into()
}
fn default_http_port() -> u16 {
    7878
}
fn default_cors_origins() -> Vec<String> {
    Vec::new()
}

// ── Policy ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
// Struct-level default: a partial `[policy]` completes with `Default`, which
// is fail-closed (deny, empty allowlists, shared deny patterns). Measured
// 2026-10-10: a one-key table was rejected as "missing field".
#[serde(default)]
pub struct PolicyConfig {
    #[serde(default = "default_policy_mode")]
    pub mode: String,
    pub default_effect: String,
    pub max_read_bytes: u64,
    pub max_list_entries: usize,
    /// Hard cap on the `timeout_secs` a model may request for `RunCommand`.
    ///
    /// SECURITY: `RunCommand.timeout_secs` is model-written text. Until
    /// 2026-08-15 it reached `Duration::from_secs` with nothing between the
    /// two — measured: `sleep 30` with `timeout_secs: 1_000_000_000` returned
    /// after 30.001 s, i.e. the runtime had agreed to wait 31 years for a child
    /// that never answers. `PodmanConfig` and `IncusConfig` already carry a
    /// `max_timeout_secs` described as "hard cap on timeout the LLM can
    /// request"; the least sandboxed spawn of the three had none.
    ///
    /// It lives in `[policy]` and not in `ExecutorConfig` — which owns a
    /// `default_timeout_secs` nobody reads (`security-mechanisms-inventory.md`
    /// n°24: five fields, zero readers) — because a ceiling with no reader is
    /// the defect this repository keeps rediscovering.
    ///
    /// The default is 3600 s, the largest timeout the runtime already grants
    /// itself: `run_command_streaming` defaults to it. The ceiling therefore
    /// admits every value the current code produces on its own and truncates
    /// only values a model invented beyond it. Operators who need longer raise
    /// the key; policy is the operator's authority, so no constant overrides it.
    #[serde(default = "default_max_command_timeout_secs")]
    pub max_command_timeout_secs: u64,
    pub allowed_commands: Vec<String>,
    pub allowed_read_paths: Vec<String>,
    pub allowed_write_paths: Vec<String>,
    pub deny_path_patterns: Vec<String>,
    /// Default deny patterns this policy lifts, each named exactly as in
    /// `DEFAULT_DENY_PATH_PATTERNS`. The defaults hold even when
    /// `deny_path_patterns` omits them; this list is the only way out.
    #[serde(default)]
    pub deny_path_exceptions: Vec<String>,
    /// Allowed container images for RunInSandbox. Empty list = all denied.
    #[serde(default)]
    pub allowed_images: Vec<String>,
    /// Directories whose executables may be run by absolute path, honoured
    /// only under `--isolation container` with a detected container.
    #[serde(default)]
    pub allowed_exec_roots: Vec<String>,
    /// `--allow-all-commands`: any program name passes the command check.
    /// Flag only, never TOML; honoured only in a detected container.
    #[serde(skip)]
    pub allow_all_commands: bool,
    /// Environment variables the LLM is allowed to read via GetEnv.
    #[serde(default = "default_readable_env_vars")]
    pub readable_env_vars: Vec<String>,
    /// Extra environment variables a spawned child may inherit, on top of
    /// `child_env::ENV_ALLOWLIST`.
    ///
    /// SECURITY: the default is EMPTY — strict. A child sees the allowlist and
    /// nothing else, because before 2026-08-15 it saw everything: a canary in
    /// the operator's shell came back out of `printenv` and into both the model
    /// context and the session log (`AUDIT-2026-08-14.md`, section 4). Each name
    /// added here is handed to every command the agent can start, prompt
    /// injection included, so `serde(default)` must stay the empty vector: a
    /// config that omits the key is a config that forwards nothing.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// How to combine the user's `[actions]` table with the built-in defaults.
    /// "merge" (default): user values override defaults; missing keys fall through.
    /// "replace": only the user's `[actions]` table is honored — strict whitelist mode.
    #[serde(default = "default_action_defaults")]
    pub action_defaults: String,
}

/// A `[policy]` that names allowlist entries but no `default_effect` is the
/// shape of a truncated or pre-0.2 file (`policy_matrix::degradations`); it
/// is refused by name rather than completed. Every other omission completes.
fn refuse_grants_without_default_effect(content: &str) -> Result<()> {
    let raw: toml::Value = toml::from_str(content)?;
    let Some(policy) = raw.get("policy").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    if policy.contains_key("default_effect") {
        return Ok(());
    }
    let named: Vec<&str> = [
        "allowed_commands",
        "allowed_read_paths",
        "allowed_write_paths",
    ]
    .into_iter()
    .filter(|k| {
        policy
            .get(*k)
            .and_then(toml::Value::as_array)
            .is_some_and(|a| !a.is_empty())
    })
    .collect();
    if named.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "[policy] names {} but omits default_effect. A policy that grants paths or \
         commands must state its default effect: add `default_effect = \"deny\"` \
         under [policy] (other omitted keys take the fail-closed defaults).",
        named.join(", ")
    )
}

fn default_action_defaults() -> String {
    "merge".into()
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            mode: "confirm".into(),
            default_effect: "deny".into(),
            max_read_bytes: 1_048_576,
            max_list_entries: 512,
            max_command_timeout_secs: default_max_command_timeout_secs(),
            allowed_commands: vec![],
            allowed_read_paths: vec![],
            allowed_write_paths: vec![],
            // SECURITY: the list itself lives in x0ne-actions::deny_paths —
            // one copy, shared with the orchestrator's read-only
            // dispensation. Editing the defaults means editing it
            // THERE, where both callers see the change.
            deny_path_patterns: x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            deny_path_exceptions: vec![],
            allowed_images: vec![],
            allowed_exec_roots: vec![],
            allow_all_commands: false,
            readable_env_vars: default_readable_env_vars(),
            env_passthrough: vec![],
            action_defaults: default_action_defaults(),
        }
    }
}

impl PolicyConfig {
    /// Makes `deny_path_patterns` the enforced list: the file's own patterns,
    /// then every default it omits, minus the ones `deny_path_exceptions`
    /// names. An exception that names no default is refused, and so is a
    /// pattern that does not compile: it would deny nothing.
    pub fn apply_deny_floor(&mut self) -> Result<()> {
        let floor = x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS;
        let unknown: Vec<&str> = self
            .deny_path_exceptions
            .iter()
            .map(String::as_str)
            .filter(|e| !floor.contains(e))
            .collect();
        if !unknown.is_empty() {
            anyhow::bail!(
                "[policy] deny_path_exceptions names {}, which is not a default deny \
                 pattern. An exception lifts one default, written exactly as one of: {}. \
                 A pattern of your own is lifted by removing it from deny_path_patterns.",
                unknown.join(", "),
                floor.join(", ")
            );
        }
        let invalid = x0ne_actions::deny_paths::invalid_patterns(&self.deny_path_patterns);
        if !invalid.is_empty() {
            anyhow::bail!(
                "[policy] deny_path_patterns holds a pattern that does not compile: {}. \
                 It would deny nothing: fix it or remove it.",
                invalid.join(", ")
            );
        }
        let mut enforced: Vec<String> = Vec::new();
        for p in self
            .deny_path_patterns
            .iter()
            .map(String::as_str)
            .chain(floor.iter().copied())
        {
            let lifted = self.deny_path_exceptions.iter().any(|e| e == p);
            if !lifted && !enforced.iter().any(|e| e == p) {
                enforced.push(p.to_string());
            }
        }
        self.deny_path_patterns = enforced;
        Ok(())
    }
}

fn default_policy_mode() -> String {
    "confirm".into()
}

/// One hour. See `PolicyConfig::max_command_timeout_secs` for why this exact
/// number and not a smaller one.
fn default_max_command_timeout_secs() -> u64 {
    3600
}

fn default_readable_env_vars() -> Vec<String> {
    vec![
        "PATH".into(),
        "HOME".into(),
        "CARGO_HOME".into(),
        "RUSTUP_HOME".into(),
        "NODE_PATH".into(),
        "GOPATH".into(),
        "SHELL".into(),
        "LANG".into(),
        "USER".into(),
    ]
}

// ── Isolation ──────────────────────────────────────────────────────────

/// How commands the agent starts are confined. See `executor::isolation`.
///
/// SECURITY: only `hidden_paths` and `limits` are read from TOML; the first
/// can only hide more, the second only lower the ceilings.
/// A project may ship its own runtime TOML (`bridge.runtime_config`), so every
/// weakening knob is `serde(skip)` and set from the runtime's command line.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IsolationConfig {
    /// Extra paths hidden from isolated commands, on top of the built-in list.
    pub hidden_paths: Vec<String>,
    /// `--isolation`. Never from TOML or the environment.
    #[serde(skip)]
    pub mode: IsolationMode,
    /// `--isolation-network`. Never from TOML or the environment.
    #[serde(skip)]
    pub network: IsolationNetwork,
    /// Test seam: the bubblewrap binary to use instead of the fixed locations.
    #[serde(skip)]
    pub bwrap_override: Option<PathBuf>,
    /// Test seam: the directory searched for container markers instead of `/`.
    #[serde(skip)]
    pub container_probe_root: Option<PathBuf>,
    /// rlimits inside bwrap. TOML may only lower them (`sandbox_child`).
    pub limits: ResourceLimits,
    /// Test seam: the binary bwrap starts to apply rlimits and Landlock.
    #[serde(skip)]
    pub confine_helper_override: Option<PathBuf>,
}

/// `[isolation.limits]`. Each value is clamped to a built-in ceiling.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub pids: Option<u64>,
    pub cpu_secs: Option<u64>,
    pub file_size_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationMode {
    /// bubblewrap is required; without it agent commands are refused.
    #[default]
    Bwrap,
    /// The runtime itself runs in an Incus/Podman container that confines it.
    Container,
    /// Host execution, typed by a human (`--isolation none`).
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationNetwork {
    /// Own network namespace, loopback only.
    #[default]
    Off,
    /// Share the host network (`--isolation-network host`).
    Host,
}

// ── Terminals ─────────────────────────────────────────────────────────────────

/// `[term]`: resource ceilings of the Term* actions, not functional limits.
/// Recording has no switch: every terminal is recorded (owner's rule, 2026-10-06).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TermConfig {
    /// Open terminals at once.
    pub max_sessions: usize,
    /// Bytes returned by one TermRead / TermWait / TermSnapshot.
    pub max_read_bytes: usize,
    /// Longest TermWait, in milliseconds.
    pub max_wait_ms: u64,
    /// Test seam: where recordings go instead of `~/.x0ne/term/`.
    #[serde(skip)]
    pub recordings_root: Option<PathBuf>,
    /// Test seam: skip tmux and use the native PTY backend.
    #[serde(skip)]
    pub force_pty: bool,
}

impl Default for TermConfig {
    fn default() -> Self {
        Self {
            max_sessions: 8,
            max_read_bytes: 64 * 1024,
            max_wait_ms: 300_000,
            recordings_root: None,
            force_pty: false,
        }
    }
}

// ── Executor ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutorConfig {
    pub default_timeout_secs: u64,
    pub max_file_read_bytes: u64,
    pub trash_dir: String,
    pub max_search_results: usize,
    pub max_log_lines: usize,
    /// Where output overflows land. `~/.x0ne/outputs` by default.
    ///
    /// SECURITY: this directory is NOT auto-allowed for reading. The model
    /// receives the path; the read goes back through `ReadFile`, hence through
    /// the policy and the gate (decision § 8-1 of the spec).
    #[serde(default = "default_outputs_dir")]
    pub outputs_dir: String,
}

fn default_outputs_dir() -> String {
    "~/.x0ne/outputs".into()
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 30,
            max_file_read_bytes: 102_400,
            trash_dir: "~/.x0ne-trash".into(),
            max_search_results: 50,
            max_log_lines: 100,
            outputs_dir: default_outputs_dir(),
        }
    }
}

impl ExecutorConfig {
    /// Resolve the spill directory, expanding `~` with the helper every other
    /// configured path goes through — a literal `~` in a filesystem path
    /// creates a directory named `~` in the cwd instead.
    pub fn resolved_outputs_dir(&self) -> PathBuf {
        expand_tilde(&self.outputs_dir)
    }
}

// ── Snapshots ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SnapshotConfig {
    pub dir: String,
    pub max_snapshots: usize,
    pub max_snapshot_size_mb: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            dir: "~/.x0ne/snapshots".into(),
            max_snapshots: 50,
            max_snapshot_size_mb: 100,
        }
    }
}

impl SnapshotConfig {
    /// Resolve the snapshot directory, expanding `~` to the home dir.
    pub fn resolved_dir(&self) -> PathBuf {
        expand_tilde(&self.dir)
    }
}

// ── Handles ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HandlesConfig {
    pub max_concurrent: usize,
    pub completed_retention_secs: u64,
}

impl Default for HandlesConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 20,
            completed_retention_secs: 300,
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    pub max_subscriptions: usize,
    pub max_events_per_handle: usize,
    pub debounce_ms: u64,
    pub port_check_interval_secs: u64,
    pub port_check_timeout_secs: u64,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            max_subscriptions: 20,
            max_events_per_handle: 1000,
            debounce_ms: 500,
            port_check_interval_secs: 2,
            port_check_timeout_secs: 300,
        }
    }
}

// ── System ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    pub enable_process_list: bool,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            enable_process_list: true,
        }
    }
}

// ── Desktop ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    /// Must be explicitly set to true to enable desktop actions.
    pub enabled: bool,
    /// If non-empty, every input gesture (pointer, keyboard, FocusWindow,
    /// SetClipboard) must land on a window whose app_id or X11 class equals
    /// one entry (case-insensitive); GetClipboard is refused. Empty: no limit.
    pub allowed_apps: Vec<String>,
    /// Require Confirm action before taking screenshots.
    pub screenshot_confirm: bool,
    /// Minimum milliseconds between input actions (anti-runaway).
    pub input_rate_limit_ms: u64,
    /// Hard cap on input actions per minute.
    pub max_input_actions_per_minute: u32,
    /// Directory for screenshots.
    pub screenshot_dir: String,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_apps: vec![],
            screenshot_confirm: true,
            input_rate_limit_ms: 100,
            max_input_actions_per_minute: 120,
            screenshot_dir: "~/.x0ne/screenshots".into(),
        }
    }
}

impl DesktopConfig {
    pub fn resolved_screenshot_dir(&self) -> PathBuf {
        expand_tilde(&self.screenshot_dir)
    }
}

// ── Composites ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompositeConfig {
    /// Maximum call depth for nested composite primitives. Default: 3.
    pub max_depth: usize,
    /// Maximum number of steps in a single composite definition. Default: 20.
    pub max_steps: usize,
    /// Directory for persistent composite definitions.
    pub primitives_dir: String,
}

impl Default for CompositeConfig {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_steps: 20,
            primitives_dir: "~/.config/x0ne/primitives".into(),
        }
    }
}

impl CompositeConfig {
    pub fn resolved_primitives_dir(&self) -> PathBuf {
        expand_tilde(&self.primitives_dir)
    }
}

// ── Sandbox / Container backends ─────────────────────────────────────────────

/// Top-level sandbox section — selects backend and holds per-backend config.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Default sandbox backend: "podman" or "incus".
    pub default_backend: String,
    /// Incus-specific settings.
    pub incus: IncusConfig,
    /// Podman/Docker-specific settings.
    pub podman: PodmanConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default_backend: "podman".into(),
            incus: IncusConfig::default(),
            podman: PodmanConfig::default(),
        }
    }
}

/// Configuration for the Incus (LXC) sandboxing backend.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IncusConfig {
    /// Remote to use when LLM doesn't specify one (must be in `incus remote list`).
    pub default_remote: String,
    /// Image allowlist. **Empty = all images denied** (fail-closed — unlike Podman quirk).
    pub allowed_images: Vec<String>,
    /// Default RAM limit per container in MB (LLM can request less, never more).
    pub default_memory_mb: u64,
    /// Hard cap on RAM the LLM can request.
    pub max_memory_mb: u64,
    /// Default vCPU limit.
    pub default_cpu_limit: u32,
    /// Default disk quota in MB.
    pub default_disk_mb: u64,
    /// Default execution timeout in seconds.
    pub default_timeout_secs: u64,
    /// Hard cap on timeout the LLM can request.
    pub max_timeout_secs: u64,
    /// Maximum number of parallel containers in a RunIncusGroup.
    pub max_parallel_instances: usize,
    /// AppArmor security profile: "default" | "unconfined" | custom name.
    pub security_profile: String,
    /// Prevent privilege escalation inside containers.
    pub no_new_privileges: bool,
    /// Default network mode: "none" (isolated) | "bridge".
    pub default_network: String,
}

impl Default for IncusConfig {
    fn default() -> Self {
        Self {
            default_remote: "local".into(),
            allowed_images: vec![], // fail-closed: must be explicitly populated
            default_memory_mb: 512,
            max_memory_mb: 4096,
            default_cpu_limit: 1,
            default_disk_mb: 2048,
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            max_parallel_instances: 10,
            security_profile: "default".into(),
            no_new_privileges: true,
            default_network: "none".into(),
        }
    }
}

/// Configuration for the Podman (OCI) sandboxing backend.
///
/// Also works with Docker (drop-in CLI compatible). Podman is preferred
/// because it runs rootless by default and doesn't require a daemon.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PodmanConfig {
    /// Image allowlist. **Empty = all images denied** (fail-closed).
    /// Supports wildcards: `"docker.io/library/*"`, `"alpine*"`.
    pub allowed_images: Vec<String>,
    /// Default RAM limit per container in MB.
    pub default_memory_mb: u64,
    /// Hard cap on RAM the LLM can request.
    pub max_memory_mb: u64,
    /// Default vCPU limit (--cpus flag).
    pub default_cpu_limit: u32,
    /// Hard cap on vCPUs the LLM can request (mirrors max_memory_mb).
    pub max_cpu_limit: u32,
    /// Default execution timeout in seconds.
    pub default_timeout_secs: u64,
    /// Hard cap on timeout the LLM can request.
    pub max_timeout_secs: u64,
    /// Maximum number of parallel containers in a RunContainerGroup.
    pub max_parallel_instances: usize,
    /// Default network access: false = --network=none (safest).
    pub default_network: bool,
    /// Prevent privilege escalation (--security-opt=no-new-privileges).
    pub no_new_privileges: bool,
}

impl Default for PodmanConfig {
    fn default() -> Self {
        Self {
            allowed_images: vec![], // fail-closed
            default_memory_mb: 256,
            max_memory_mb: 2048,
            default_cpu_limit: 1,
            max_cpu_limit: 4,
            default_timeout_secs: 30,
            max_timeout_secs: 300,
            max_parallel_instances: 8,
            default_network: false,
            no_new_privileges: true,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Closest real action name to `bad`, for the "did you mean" hint.
///
/// Derived from the vocabulary, so a suggestion can never point at an action
/// that no longer exists — which is precisely what the orchestrator's own
/// typo-suggestion list did for two releases.
fn nearest_action_name(bad: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let a = a.to_lowercase();
        let b: Vec<char> = b.to_lowercase().chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut curr = vec![0usize; b.len() + 1];
        for (i, ca) in a.chars().enumerate() {
            curr[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                curr[j + 1] = if ca == *cb {
                    prev[j]
                } else {
                    1 + prev[j].min(prev[j + 1]).min(curr[j])
                };
            }
            std::mem::swap(&mut prev, &mut curr);
        }
        prev[b.len()]
    }
    x0ne_actions::all_type_names()
        .iter()
        .map(|n| (*n, distance(bad, n)))
        .filter(|(_, d)| *d <= 4)
        .min_by_key(|(_, d)| *d)
        .map(|(n, _)| n)
}

// ── AppConfig loading ─────────────────────────────────────────────────────────

/// Present = anything at that path, a dangling link included: `exists()`
/// follows the link and would skip to the next, wider, location.
fn is_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// What to write, and where, when no policy file was found.
fn no_policy_message(config_dir: Option<&Path>) -> String {
    let target = match config_dir {
        Some(dir) => dir
            .join("x0ne")
            .join("x0ne-runtime.toml")
            .display()
            .to_string(),
        None => "$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml (no config directory: \
                 neither XDG_CONFIG_HOME nor HOME resolves)"
            .to_string(),
    };
    format!(
        "no policy file. x0ne-runtime enforces the policy written in \
         x0ne-runtime.toml and does not run without one.\n\
         \n\
         Write it at:\n  {target}\n\
         with `x0ne --setup` (asks a few questions, writes that file), or by copying \
         the reference profile x0ne-runtime/x0ne-runtime.toml from the source tree \
         and editing it. Or pass --config <file>.\n\
         \n\
         ./x0ne-runtime.toml is read only when {ALLOW_CWD_CONFIG_ENV}=1."
    )
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let mut config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        refuse_grants_without_default_effect(&content)
            .with_context(|| format!("Refusing config file: {}", path.display()))?;
        // Merge defaults under the user's [actions] table. The user's explicit
        // values always win; missing keys fall through to defaults. This avoids
        // the "I added [actions] WriteFile=true and lost everything else"
        // footgun we hit live during gateway smoke tests with the readonly
        // profile and Pipeline. New native sysaicalls / control-flow primitives
        // become reachable in custom profiles without each profile having to
        // re-list them. To opt out (strict whitelist mode) set
        // `[policy] action_defaults = "replace"` (default is "merge").
        // FAIL CLOSED on stale or misspelled action names. An unknown key used
        // to be accepted and ignored, so a profile could claim to disable
        // something it did not disable — the shipped read-only profile listed
        // 34 actions that had been removed, and an operator reading it would
        // reasonably believe those were the ones being denied. Silence is the
        // worst outcome for a security control: it looks exactly like success.
        let mut unknown: Vec<&str> = config
            .actions
            .keys()
            .map(|k| k.as_str())
            .filter(|k| !x0ne_actions::all_type_names().contains(k))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            let hints: Vec<String> = unknown
                .iter()
                .map(|k| match nearest_action_name(k) {
                    Some(near) => format!("  {k} — unknown; did you mean {near}?"),
                    None => format!("  {k} — unknown; no similar action exists"),
                })
                .collect();
            anyhow::bail!(
                "{}: [actions] names {} action(s) that do not exist in this runtime:\n{}\n\
                 Remove them, or correct the spelling. `ListActions` reports the \
                 vocabulary this runtime actually accepts.",
                path.display(),
                unknown.len(),
                hints.join("\n")
            );
        }

        // SECURITY: fail closed on a `default_effect` this runtime does not
        // know. The value is compared to the literal "deny" and to nothing else
        // (`policy.rs:554`), so ANY other spelling — `"Deny"`, `"denny"`,
        // `"maybe"` — is read as "not deny", and an action absent from the
        // resolved `[actions]` map then falls THROUGH the per-action switch
        // instead of being refused by it. Under `action_defaults = "replace"`,
        // where the map holds only what the operator listed, that is every
        // action he did not name: one typo in one word and a strict whitelist
        // stops being a whitelist. Measured 2026-08-16 by the load matrix
        // (`policy_matrix::degradations`), which is the first test in this repo
        // to have LOADED a policy carrying a bad value instead of parsing one.
        //
        // The same treatment as the unknown `[actions]` key above, for the same
        // reason: a security control that accepts what it cannot honor fails
        // silently, and silence looks exactly like success.
        const KNOWN_DEFAULT_EFFECTS: [&str; 3] = ["allow", "deny", "confirm"];
        if !KNOWN_DEFAULT_EFFECTS.contains(&config.policy.default_effect.as_str()) {
            anyhow::bail!(
                "{}: [policy] default_effect = \"{}\" is not a value this runtime \
                 understands. Use one of: {}. Anything else is read as \"not deny\" \
                 and silently widens the policy.",
                path.display(),
                config.policy.default_effect,
                KNOWN_DEFAULT_EFFECTS.join(", ")
            );
        }

        // SECURITY: `x0ne setup` wrote `deny_path_patterns = []`, and
        // ReadFile then served `.env`, `*.pem` and `secrets/` under the roots.
        // The defaults now hold whatever the file lists, unless lifted by name.
        config
            .policy
            .apply_deny_floor()
            .with_context(|| format!("Refusing config file: {}", path.display()))?;

        let merge = config.policy.action_defaults != "replace";
        if merge {
            let defaults = Self::defaults();
            for (k, v) in defaults.actions.iter() {
                config.actions.entry(k.clone()).or_insert(*v);
            }
        }
        Ok(config)
    }

    /// The policy lines an operator needs told at startup, once.
    ///
    /// Since the deny-all change, an empty allowlist means DENY ALL, where
    /// it used to mean "no restriction". The change is right and it is
    /// breaking, and the field report of 2026-08-15 measured what it costs
    /// when it is silent: the operator learned it one refusal at a time, from
    /// a hint attached to the command that failed, and read the pattern as the
    /// approval gate being broken rather than as the policy he had written.
    ///
    /// These are warnings, never a refusal to start. A runtime with no
    /// commands and no write roots is a legitimate posture — it is what the
    /// shipped read-only profile is — so this says what the file means, and
    /// leaves the decision alone.
    ///
    /// Empty means empty, not "not configured": this crate cannot distinguish
    /// an absent key from `[]`, and both deny, so both warn.
    pub fn policy_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.policy.allowed_commands.is_empty() {
            out.push(
                "[policy] allowed_commands is empty: no command will run \
                 (RunCommand will always be denied). Add commands to \
                 [policy] allowed_commands in x0ne-runtime.toml, or run `x0ne --setup`."
                    .to_string(),
            );
        }
        if self.policy.allowed_read_paths.is_empty() {
            out.push(
                "[policy] allowed_read_paths is empty: deny all — no file will be \
                 read. Add paths to [policy] allowed_read_paths in \
                 x0ne-runtime.toml, or run `x0ne --setup`."
                    .to_string(),
            );
        }
        if self.policy.allowed_write_paths.is_empty() {
            out.push(
                "[policy] allowed_write_paths is empty: deny all — no file will be \
                 written. Add paths to [policy] allowed_write_paths in \
                 x0ne-runtime.toml, or run `x0ne --setup`."
                    .to_string(),
            );
        }
        // A root that does not resolve is dropped by `normalize_roots`, and
        // was dropped SILENTLY until 2026-08-16. Field report of that day:
        // `allowed_read_paths = ["~/code"]` on a machine with no `~/code`
        // denied every read with "no read paths are configured", and the
        // operator went looking for an empty list he did not have. The list is
        // written, loaded, read — and inert: the repo's recurring defect, this
        // time inside the policy file itself (docs/invariants.md, "dead scaffolding").
        for (key, raw) in [
            ("allowed_read_paths", &self.policy.allowed_read_paths),
            ("allowed_write_paths", &self.policy.allowed_write_paths),
        ] {
            let dead = crate::executor::confine::unresolvable_roots(raw);
            if dead.is_empty() {
                continue;
            }
            let all = dead.len() == raw.len();
            out.push(format!(
                "[policy] {key}: {} does not exist, so {} — create the directory or fix \
                 the path in x0ne-runtime.toml.",
                dead.join(", "),
                if all {
                    "the list is effectively EMPTY and everything is denied"
                } else {
                    "that entry grants nothing"
                }
            ));
        }
        if !self.policy.deny_path_exceptions.is_empty() {
            out.push(format!(
                "[policy] deny_path_exceptions lifts {} default deny pattern(s): {}. \
                 What they name is no longer refused under the allowed roots.",
                self.policy.deny_path_exceptions.len(),
                self.policy.deny_path_exceptions.join(", ")
            ));
        }
        out
    }

    /// Write [`Self::policy_warnings`] where a human will see them.
    ///
    /// stderr rather than `tracing`, for the reason the policy-provenance line
    /// beside it is on stderr: the MCP entries pass `--log-level error`, so a
    /// WARN is filtered out in exactly the deployment that most needs it
    /// (audit 2026-08-15). Takes the sink so the property is testable without
    /// capturing a process's file descriptors.
    pub fn emit_policy_warnings(&self, out: &mut dyn std::io::Write) {
        for line in self.policy_warnings() {
            let _ = writeln!(out, "x0ne-runtime: warning: {}", line);
        }
    }

    /// Find and load a runtime config, in priority order:
    /// 1. `override_path` (CLI flag, e.g. `--config foo.toml`)
    /// 2. `./x0ne-runtime.toml` — **only** when
    ///    `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1` (see [`ConfigSource`] and the
    ///    SECURITY note below)
    /// 3. `$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`
    ///    (canonical path used by `make install` and `cargo install`)
    /// 4. `$XDG_CONFIG_HOME/x0ne-runtime/config.toml`
    ///    (legacy path, kept for backwards compatibility)
    ///
    /// The first existing file wins, and it must state a policy: a missing,
    /// empty, unreadable or invalid file is an error, never the compiled
    /// defaults. A path that exists as a dangling link counts as present.
    pub fn find_and_load(override_path: Option<&PathBuf>) -> Result<Self> {
        Self::find_and_load_with_source(override_path).map(|(config, _)| config)
    }

    /// As [`Self::find_and_load`], but also reports WHERE the policy came
    /// from.
    ///
    /// SECURITY: the caller is expected to announce the source. The audit of
    /// 2026-08-15 turned on nobody being able to tell which policy a running
    /// runtime was enforcing: the former `.mcp.json` launched `x0ne-runtime --mode mcp`
    /// with no `--config`, the search silently landed on the wide-open
    /// reference TOML that `make install` deposits, and every MCP client got
    /// `run_command` on the host. The resolution had a log line, but at INFO —
    /// and that entry passed `--log-level error`, so it was never printed.
    /// Provenance is therefore returned to the caller rather than merely
    /// logged.
    pub fn find_and_load_with_source(
        override_path: Option<&PathBuf>,
    ) -> Result<(Self, ConfigSource)> {
        if let Some(path) = override_path {
            return Ok((
                Self::load_policy(path)?,
                ConfigSource::Explicit(path.clone()),
            ));
        }

        // SECURITY: `./x0ne-runtime.toml` used to sit here unconditionally,
        // ahead of the XDG install. It is the policy file of whoever controls
        // the working directory — and an agent that can write one file into
        // the directory its own runtime is launched from could author that
        // runtime's policy: `default_effect = "allow"` and it has the host.
        // Convenient during development, a privilege escalation everywhere
        // else, so it is now an explicit opt-in that a human has to type.
        let cwd_config = PathBuf::from("x0ne-runtime.toml");
        if is_present(&cwd_config) {
            if cwd_config_is_allowed() {
                let abs = std::fs::canonicalize(&cwd_config).unwrap_or(cwd_config);
                return Ok((Self::load_policy(&abs)?, ConfigSource::Cwd(abs)));
            }
            tracing::warn!(
                target: "x0ne-runtime::config",
                "ignoring ./x0ne-runtime.toml: a config in the working directory is \
                 only honored when X0NE_RUNTIME_ALLOW_CWD_CONFIG=1 is set explicitly"
            );
        }

        let config_dir = dirs::config_dir();
        if let Some(config_dir) = &config_dir {
            // Canonical install path (preferred): used by `make install`
            // and the repo's reference TOML.
            let xdg_x0ne = config_dir.join("x0ne").join("x0ne-runtime.toml");
            if is_present(&xdg_x0ne) {
                return Ok((Self::load_policy(&xdg_x0ne)?, ConfigSource::Xdg(xdg_x0ne)));
            }
            // Legacy path (pre-0.2): kept so older installs keep working.
            let legacy = config_dir.join("x0ne-runtime").join("config.toml");
            if is_present(&legacy) {
                return Ok((Self::load_policy(&legacy)?, ConfigSource::LegacyXdg(legacy)));
            }
        }
        anyhow::bail!("{}", no_policy_message(config_dir.as_deref()))
    }

    /// [`Self::load`], refusing a file that states nothing: empty or
    /// comments only would otherwise load as the compiled defaults.
    fn load_policy(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let states_something = text.lines().any(|l| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with('#')
        });
        if !states_something {
            anyhow::bail!(
                "{} is empty: a policy file must state the policy. Run `x0ne --setup` \
                 to write it, or copy the reference x0ne-runtime.toml.",
                path.display()
            );
        }
        Self::load(path)
    }

    /// SECURITY: refuse to serve MCP with a policy that can never say "ask".
    ///
    /// `policy.mode = "auto"` means "enforce the lists; the ORCHESTRATOR owns
    /// the human gate" (repo invariant 2). It is the right posture for
    /// `--mode stdio`, where an orchestrator is on the other end of the pipe
    /// and has a human under its hand, and for `--mode http`, whose client is
    /// that same orchestrator.
    ///
    /// In `--mode mcp` the runtime IS the endpoint: the peer is an arbitrary
    /// MCP host, there is no orchestrator, and no confirmation can ever be
    /// delivered over the pipe. `auto` there does not mean "somebody else
    /// asks" — it means nobody does. `PolicyEffect::RequireConfirmation` is
    /// never produced, so the fail-closed refusal at `mcp/server.rs:194` is
    /// unreachable code, and the audit of 2026-08-15 measured the consequence:
    /// `run_command {"command":"id"}` from an unauthenticated MCP client
    /// returned the host's uid, groups and all.
    ///
    /// Exactly one thing re-authorizes `auto` here: `--upstream-human-gate`,
    /// by which the launching component asserts it front-ends this runtime
    /// and owns the gate itself. That is how `x0ne-orchestrator --mcp
    /// --downstream-runtime` legitimately runs a downstream in `auto` without
    /// re-asking on top of an approval a human already gave (the
    /// double-gating regression of 2026-08-12).
    ///
    /// Returns the human-facing refusal; the caller aborts startup.
    /// `--allow-all-commands` only where the container is the boundary:
    /// `--isolation container` with a detected container, else startup fails.
    pub fn check_allow_all_commands(&self) -> Result<()> {
        if !self.policy.allow_all_commands {
            return Ok(());
        }
        match crate::executor::isolation::confinement(&self.isolation) {
            Ok(crate::executor::isolation::Confinement::Container) => Ok(()),
            _ => anyhow::bail!(
                "refusing --allow-all-commands: it needs --isolation container inside a \
                 detected container (a throwaway bench container), and this runtime is not in one"
            ),
        }
    }

    pub fn check_headless_posture(&self, upstream_human_gate: bool) -> Result<()> {
        if self.runtime.mode != "mcp" {
            return Ok(());
        }
        if upstream_human_gate {
            return Ok(());
        }
        // "confirm" and "readonly" both have a defined meaning with no human:
        // confirm => every modifying action is refused (nothing can approve
        // it); readonly => every modifying action is refused outright.
        if self.policy.mode == "confirm" || self.policy.mode == "readonly" {
            return Ok(());
        }
        anyhow::bail!(
            "refusing to start --mode mcp with policy.mode = \"{}\".\n\
             \n\
             An MCP runtime has no human on the other end of the pipe, so a policy \
             that never requires confirmation is a policy that never refuses: every \
             enabled action, RunCommand included, would run unattended for any client.\n\
             \n\
             Set one of these in the [policy] table of your runtime config:\n  \
               policy.mode = \"confirm\"   — modifying actions are refused (no approver exists)\n  \
               policy.mode = \"readonly\"  — modifying actions are refused outright\n\
             \n\
             --upstream-human-gate is reserved for x0ne-orchestrator, which passes it to \
             the runtime it starts and asks the human itself; never add it to an MCP \
             client's entry. For writes from an MCP client, use `x0ne-orchestrator --mcp`.\n\
             \n\
             The hardened profile shipped for MCP clients is x0ne-runtime-mcp-readonly.toml \
             (installed at ~/.config/x0ne/x0ne-runtime-mcp.toml by `make install` and by \
             install.sh); point \
             --config at it.",
            self.policy.mode
        )
    }

    /// Compile-time safe defaults.
    pub fn defaults() -> Self {
        let mut actions = HashMap::new();
        // The key set is the vocabulary itself, so this map can never be
        // missing an action or naming one that no longer exists. It used to be
        // two hand-written arrays: by the time they were replaced they named
        // 56 removed actions and omitted 14 real ones — and an action absent
        // from the map falls through to `default_effect`, which is "deny". So
        // `Simulate`, the primitive whose entire purpose is previewing a risky
        // plan safely, was deny-by-default and survived only because
        // `ALWAYS_ALLOW` happened to list it.
        //
        // The value comes from the side-effect class: reads, introspection and
        // control flow are on; anything that writes, executes, reaches the
        // network, destroys or drives the desktop is off. Control flow is on
        // because capability comes from the nested steps, each policy-checked
        // individually.
        for meta in x0ne_actions::GENERATED_ACTIONS {
            let enabled = matches!(meta.class, "ReadOnly" | "Meta" | "Composite");
            actions.insert(meta.name.to_string(), enabled);
        }
        // Deviations from the class rule. These are policy judgements, not
        // vocabulary, and each one is a deliberate answer to "is the class
        // right about this particular action?".
        for (name, enabled) in [
            // Executes a stored composite, and streams — capability beyond
            // what "introspection" suggests.
            ("CallPrimitive", false),
            ("Subscribe", false),
            // `DefineFunction` / `DeleteFunction` were switched on here until
            // 2026-09-11 ("scoped to the function store, not the filesystem
            // at large"). Removed on the owner's ruling: they write and delete
            // under `~/.x0ne`, their class says so, and a class rule with a
            // deviation for the one store nobody uses is how the "Zero write
            // permissions" MCP profile came to accept writes.
            // Name and port probes: outbound, but they carry no payload.
            ("DnsLookup", true),
            ("PortCheck", true),
        ] {
            actions.insert(name.to_string(), enabled);
        }

        Self {
            runtime: RuntimeConfig::default(),
            policy: PolicyConfig::default(),
            actions,
            executor: ExecutorConfig::default(),
            snapshots: SnapshotConfig::default(),
            handles: HandlesConfig::default(),
            events: EventsConfig::default(),
            system: SystemConfig::default(),
            desktop: DesktopConfig::default(),
            composites: CompositeConfig::default(),
            sandbox: SandboxConfig::default(),
            isolation: IsolationConfig::default(),
            term: TermConfig::default(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_default_config() {
        let config = AppConfig::defaults();
        assert_eq!(config.runtime.mode, "stdio");
        assert_eq!(config.policy.default_effect, "deny");
        assert!(*config.actions.get("ReadFile").unwrap());
        assert!(!(*config.actions.get("RunCommand").unwrap()));
        assert!(!config.desktop.enabled);
        assert_eq!(config.handles.max_concurrent, 20);
    }

    /// A `[policy]` table that names one key must load, and every key it
    /// leaves out must take the fail-closed default: `deny`, empty
    /// allowlists, the shared deny patterns. Measured 2026-10-10: a
    /// `[policy]` with only `mode = "auto"` was rejected as "missing field".
    #[test]
    fn a_partial_policy_table_completes_with_fail_closed_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[policy]\nmode = \"auto\"\n").unwrap();
        let config =
            AppConfig::load(f.path()).expect("a partial [policy] table must complete, not fail");
        assert_eq!(config.policy.mode, "auto");
        assert_eq!(config.policy.default_effect, "deny");
        assert!(config.policy.allowed_commands.is_empty());
        assert!(config.policy.allowed_read_paths.is_empty());
        assert!(config.policy.allowed_write_paths.is_empty());
        assert_eq!(
            config.policy.deny_path_patterns,
            PolicyConfig::default().deny_path_patterns,
            "an omitted deny list must be the default list, never empty"
        );
        assert_eq!(config.policy.max_read_bytes, 1_048_576);
        assert_eq!(config.policy.max_list_entries, 512);
    }

    /// The one omission that does not complete: a grant without a stated
    /// `default_effect` (the truncated-file shape `policy_matrix::degradations`
    /// guards). Refused by name; one explicit line makes it load.
    #[test]
    fn a_grant_without_default_effect_is_refused_and_told_what_to_add() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "[policy]\nallowed_write_paths = [\"/tmp/x\"]\n").unwrap();
        let err =
            AppConfig::load(f.path()).expect_err("a grant without default_effect must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("allowed_write_paths"), "{msg}");
        assert!(msg.contains("default_effect = \"deny\""), "{msg}");

        let mut g = NamedTempFile::new().unwrap();
        write!(
            g,
            "[policy]\ndefault_effect = \"deny\"\nallowed_write_paths = [\"/tmp/x\"]\n"
        )
        .unwrap();
        let config = AppConfig::load(g.path()).expect("with default_effect stated it loads");
        assert_eq!(
            config.policy.allowed_write_paths,
            vec!["/tmp/x".to_string()]
        );
        assert!(config.policy.allowed_commands.is_empty());
    }

    #[test]
    fn load_from_toml_file() {
        let toml = r#"
[runtime]
mode = "http"

[runtime.http]
host = "127.0.0.1"
port = 9090

[policy]
default_effect = "allow"
max_read_bytes = 65536
max_list_entries = 100
allowed_commands = ["git"]
allowed_read_paths = ["/home/user/code"]
allowed_write_paths = []
deny_path_patterns = []

[actions]
ReadFile = true
WriteFile = true
RunCommand = false

[snapshots]
dir = "~/.x0ne/snapshots"
max_snapshots = 10
max_snapshot_size_mb = 50

[desktop]
enabled = false
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let config = AppConfig::load(f.path()).unwrap();
        assert_eq!(config.runtime.mode, "http");
        assert_eq!(config.policy.max_read_bytes, 65536);
        assert!(*config.actions.get("WriteFile").unwrap());
        assert_eq!(config.snapshots.max_snapshots, 10);
    }

    #[test]
    fn expand_tilde_works() {
        let p = expand_tilde("~/.x0ne/snapshots");
        assert!(p.to_string_lossy().contains(".x0ne/snapshots"));
        assert!(!p.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn expand_tilde_bare() {
        let p = expand_tilde("~");
        assert!(
            !p.to_string_lossy().starts_with('~'),
            "bare ~ should expand to home_dir, got {:?}",
            p
        );
    }

    #[test]
    fn actions_merge_under_defaults() {
        // User specifies only WriteFile=true; expects to also inherit
        // safe defaults like ReadFile, Pipeline, Add, etc.
        let toml = r#"
[runtime]
mode = "stdio"

[policy]
default_effect = "deny"
max_read_bytes = 1024
max_list_entries = 100
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []

[actions]
WriteFile = true

[snapshots]
dir = "~/.x0ne/snapshots"
max_snapshots = 0
max_snapshot_size_mb = 0

[desktop]
enabled = false
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).unwrap();
        // explicit user value wins
        assert_eq!(cfg.actions.get("WriteFile"), Some(&true));
        // default fall-through: read-only stuff
        assert_eq!(
            cfg.actions.get("ReadFile"),
            Some(&true),
            "ReadFile should be inherited"
        );
        assert_eq!(
            cfg.actions.get("Pipeline"),
            Some(&true),
            "Pipeline should be inherited"
        );
        assert_eq!(
            cfg.actions.get("Simulate"),
            Some(&true),
            "Simulate should be inherited"
        );
        // dangerous stuff stays disabled by default
        assert_eq!(cfg.actions.get("RunCommand"), Some(&false));
    }

    #[test]
    fn actions_replace_mode_strict_whitelist() {
        // Opting into "replace" mode: only user's keys exist in the actions map.
        let toml = r#"
[runtime]
mode = "stdio"

[policy]
default_effect = "deny"
max_read_bytes = 1024
max_list_entries = 100
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []
action_defaults = "replace"

[actions]
ReadFile = true

[snapshots]
dir = "~/.x0ne/snapshots"
max_snapshots = 0
max_snapshot_size_mb = 0

[desktop]
enabled = false
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.actions.get("ReadFile"), Some(&true));
        // No default fall-through in replace mode.
        assert_eq!(cfg.actions.get("Pipeline"), None);
        assert_eq!(cfg.actions.get("Add"), None);
    }

    #[test]
    fn user_explicit_false_wins_over_default() {
        // Defaults enable Pipeline; user explicitly disables it.
        let toml = r#"
[runtime]
mode = "stdio"

[policy]
default_effect = "deny"
max_read_bytes = 1024
max_list_entries = 100
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []

[actions]
Pipeline = false

[snapshots]
dir = "~/.x0ne/snapshots"
max_snapshots = 0
max_snapshot_size_mb = 0

[desktop]
enabled = false
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(
            cfg.actions.get("Pipeline"),
            Some(&false),
            "explicit user false must win over default true"
        );
    }

    #[test]
    fn readable_env_vars_default_contains_path() {
        let config = PolicyConfig::default();
        assert!(config.readable_env_vars.contains(&"PATH".to_string()));
    }

    /// RED: a stale or misspelled `[actions]` key was accepted and ignored, so
    /// a profile could claim to disable something it did not disable. The
    /// shipped read-only profile listed 34 removed actions; an operator
    /// reading it would reasonably believe those were the ones being denied.
    #[test]
    fn unknown_action_keys_fail_the_load() {
        let dir = std::env::temp_dir().join(format!("x0ne-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stale.toml");
        std::fs::write(
            &path,
            "[actions]\nReadFile = true\nListDir = false\nAppendFile = false\n",
        )
        .unwrap();

        let err = AppConfig::load(&path).expect_err("stale keys must not load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ListDir") && msg.contains("AppendFile"),
            "the error must name every offending key so it can be fixed in one pass: {msg}"
        );
        assert!(
            !msg.contains("ReadFile —"),
            "valid keys must not be reported as offenders: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ANCHOR: every key the shipped defaults define must be a real action.
    /// This is what makes the strict load safe to enable — if defaults could
    /// contain a stale name, the runtime would refuse its own configuration.
    #[test]
    fn every_configured_action_key_exists() {
        for name in AppConfig::defaults().actions.keys() {
            assert!(
                x0ne_actions::all_type_names().contains(&name.as_str()),
                "default policy names '{name}', which is not a runtime action"
            );
        }
    }

    /// ANCHOR: and no action may be missing, because a missing key silently
    /// falls through to `default_effect` — deny — with nothing to indicate the
    /// action was never considered.
    #[test]
    fn default_policy_covers_every_action() {
        let defaults = AppConfig::defaults();
        for name in x0ne_actions::all_type_names() {
            assert!(
                defaults.actions.contains_key(*name),
                "'{name}' has no default policy, so it is denied without a decision"
            );
        }
    }

    fn workspace_root_for_test() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf()
    }

    // ── The headless posture gate (audit 2026-08-15, finding 1) ──────────────

    /// SECURITY: `--mode mcp` with `policy.mode = "auto"` must not start.
    ///
    /// `auto` delegates the human gate to an orchestrator. Wired straight into
    /// an MCP host there is no orchestrator, so nothing ever produces
    /// `RequireConfirmation` and the fail-closed refusal in `mcp/server.rs` is
    /// unreachable. A failure here means an MCP client can once again run host
    /// commands unattended.
    #[test]
    fn mcp_mode_refuses_a_policy_that_can_never_ask() {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "mcp".into();
        for policy_mode in ["auto", "", "whatever"] {
            cfg.policy.mode = policy_mode.into();
            let err = cfg
                .check_headless_posture(false)
                .expect_err("policy.mode = {policy_mode:?} must be refused under --mode mcp");
            let msg = format!("{err}");
            assert!(
                msg.contains("policy.mode"),
                "the refusal must name the lever to move: {msg}"
            );
        }
    }

    /// The two postures that mean something without a human must still boot —
    /// a gate that refuses everything is a broken runtime, not a secure one.
    #[test]
    fn mcp_mode_accepts_confirm_and_readonly() {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "mcp".into();
        for policy_mode in ["confirm", "readonly"] {
            cfg.policy.mode = policy_mode.into();
            assert!(
                cfg.check_headless_posture(false).is_ok(),
                "policy.mode = {policy_mode} is a legitimate headless posture"
            );
        }
    }

    /// The orchestrator front-ends its downstream runtime and owns the gate
    /// itself (invariant 2). Forcing the downstream to re-ask would produce a
    /// refusal after a human's approval — the double-gating of 2026-08-12.
    #[test]
    fn an_upstream_human_gate_re_authorizes_auto_under_mcp() {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "mcp".into();
        cfg.policy.mode = "auto".into();
        assert!(cfg.check_headless_posture(false).is_err());
        assert!(
            cfg.check_headless_posture(true).is_ok(),
            "--upstream-human-gate is what distinguishes a gateway-fronted \
             runtime from one wired straight into an MCP host"
        );
    }

    /// The refusal named `--upstream-human-gate` as a way out. Added to a
    /// client's entry, it serves `auto` with no one asked (audit 2026-11-02,
    /// A4): the message must say the flag is the gateway's, not offer it.
    #[test]
    fn the_mcp_refusal_does_not_offer_the_gateway_flag() {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "mcp".into();
        cfg.policy.mode = "auto".into();
        let msg = cfg.check_headless_posture(false).unwrap_err().to_string();
        assert!(!msg.contains("Or pass --upstream-human-gate"), "{msg}");
        assert!(
            msg.contains("reserved for x0ne-orchestrator") && msg.contains("never add it"),
            "{msg}"
        );
    }

    /// The gate is scoped to `mcp`. `stdio` is the orchestrator's own bridge
    /// (`x0ne-orchestrator/src/bridge.rs:40`) and `http` is a daemon whose
    /// client is that same orchestrator; `auto` is correct in both.
    #[test]
    fn the_headless_gate_does_not_touch_the_orchestrator_fronted_modes() {
        for runtime_mode in ["stdio", "http"] {
            let mut cfg = AppConfig::defaults();
            cfg.runtime.mode = runtime_mode.into();
            cfg.policy.mode = "auto".into();
            assert!(
                cfg.check_headless_posture(false).is_ok(),
                "--mode {runtime_mode} must keep working in policy.mode=auto"
            );
        }
    }

    // ── The shipped profiles (audit 2026-08-15, finding 4) ───────────────────

    /// SECURITY: the reference config `make install` deposits must protect the
    /// obvious secret stores.
    ///
    /// It shipped `deny_path_patterns = []` while SECURITY.md claimed
    /// otherwise, so a fresh install guarded neither `~/.ssh` nor `.env`. This
    /// asserts on the FILE, loaded the way the runtime loads it, not on a
    /// hand-built fixture — the point is what a new operator actually gets.
    #[test]
    fn the_installed_reference_config_denies_reads_of_secret_stores() {
        let path = workspace_root_for_test()
            .join("x0ne-runtime")
            .join("x0ne-runtime.toml");
        let cfg = AppConfig::load(&path).expect("reference profile must load");
        assert!(
            !cfg.policy.deny_path_patterns.is_empty(),
            "the shipped reference config must deny SOMETHING by pattern"
        );

        let engine = crate::policy::PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        for secret in [
            "~/.ssh/id_rsa",
            "~/.ssh/config",
            "~/.x0ne/.env",
            "~/code/.env",
            "~/.gnupg/secring.gpg",
        ] {
            let action = x0ne_actions::Action::ReadFile {
                path: secret.into(),
                offset: 0,
                limit: None,
                start_line: None,
                limit_lines: None,
            };
            assert!(
                matches!(
                    engine.evaluate(&action),
                    crate::policy::PolicyEffect::Deny(_)
                ),
                "a fresh `make install` must refuse to read {secret}"
            );
        }
    }

    /// SECURITY: the profile the MCP entry points now name must be a posture
    /// that can boot headless AND grant nothing dangerous. If someone relaxes
    /// it, the `make claude-add` / `make codex-add` wiring silently becomes a hole
    /// again — which is exactly how finding 1 happened.
    #[test]
    fn the_mcp_profile_is_headless_safe() {
        let path = workspace_root_for_test().join("x0ne-runtime-mcp-readonly.toml");
        let cfg = AppConfig::load(&path).expect("MCP profile must load");

        cfg.check_headless_posture(false)
            .expect("the profile the MCP entry points name must be able to start under --mode mcp");
        assert_eq!(
            cfg.policy.default_effect, "deny",
            "the MCP profile must be deny-by-default"
        );
        assert!(
            cfg.policy.allowed_commands.is_empty(),
            "the MCP profile must grant no command"
        );
        assert!(
            cfg.policy.allowed_write_paths.is_empty(),
            "the MCP profile must grant no write path"
        );
    }

    /// ANCHOR: the profiles we ship must load under the strict rule we just
    /// imposed. Between them they carried 92 keys for actions that no longer
    /// exist — the read-only profile alone named 34, so it enabled far fewer
    /// real actions than it advertised and denied nothing it claimed to deny.
    #[test]
    fn shipped_profiles_contain_no_stale_action_keys() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        for rel in [
            "x0ne-runtime/x0ne-runtime.toml",
            "x0ne-runtime/x0ne-runtime-sandbox.toml",
            "x0ne-runtime-mcp-readonly.toml",
        ] {
            let path = root.join(rel);
            if !path.exists() {
                continue;
            }
            AppConfig::load(&path)
                .unwrap_or_else(|e| panic!("shipped profile {rel} does not load: {e:#}"));
        }
    }

    /// SECURITY: the capability tier is denied by default, and it is so **by
    /// class derivation** — no manual entry in the override list of
    /// `defaults()` (config.rs:612-668). If someone moves `RunProgram` to a
    /// class enabled by default, this test fails before the fresh install
    /// enables it.
    ///
    /// The asymmetry with `PreviewProgram` is **intended and tested in the same
    /// test**, so that nobody can « fix » it believing they are repairing an
    /// inconsistency: seeing what a program would do is of the same order as a
    /// `ReadFile`; applying it is not.
    ///
    /// A failure here means: `make install` on a blank machine ships a
    /// runtime that applies asm-ai programs without anyone having decided so.
    #[test]
    fn a_fresh_install_previews_programs_but_refuses_to_apply_them() {
        let cfg = AppConfig::defaults();
        assert_eq!(
            cfg.actions.get("RunProgram"),
            Some(&false),
            "RunProgram must be disabled by default"
        );
        assert_eq!(
            cfg.actions.get("PreviewProgram"),
            Some(&true),
            "PreviewProgram writes nothing: it follows the ReadOnly rule"
        );
        // And no override exists: both values come from the class.
        assert_eq!(
            x0ne_actions::action_meta("RunProgram")
                .expect("catalogue")
                .class,
            "FsWrite"
        );
        assert_eq!(
            x0ne_actions::action_meta("PreviewProgram")
                .expect("catalogue")
                .class,
            "ReadOnly"
        );
    }

    // ── The empty-allowlist warning (P8a, 2026-08-16) ────────────────────

    /// A config that forbids everything says so once, at load.
    ///
    /// Since the deny-all change, an empty allowlist means DENY ALL; before it, it meant "no
    /// restriction". The field report of 2026-08-15 measured what silence
    /// costs: the operator learned the new semantics one refusal at a time and
    /// read the pattern as the approval gate being broken.
    ///
    /// A failure means that silence is back — and the warning is the only
    /// thing between a hardened config and an operator who thinks his tools
    /// are broken.
    #[test]
    fn an_empty_allowlist_warns_once_at_load() {
        let toml = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 500
allowed_commands = []
allowed_read_paths = ["~/code"]
allowed_write_paths = []
deny_path_patterns = []
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        // Loaded, not parsed. A test that hand-built an AppConfig would prove
        // nothing about the file an operator actually writes — the distinction
        // that let the wizard's three dead profiles live for months.
        let cfg = AppConfig::load(f.path()).expect("this config is legal, just strict");

        let mut out: Vec<u8> = Vec::new();
        cfg.emit_policy_warnings(&mut out);
        let text = String::from_utf8(out).unwrap();

        assert_eq!(
            text.matches("allowed_commands is empty").count(),
            1,
            "said once, not once per command: {text}"
        );
        assert!(
            text.contains("RunCommand will always be denied"),
            "the warning must say what it costs: {text}"
        );
        assert!(
            text.contains("allowed_write_paths is empty"),
            "an empty write list denies every write and must say so: {text}"
        );
        assert!(
            !text.contains("allowed_read_paths is empty"),
            "this config HAS a read root — warning about it would teach the \
             operator to ignore the warnings: {text}"
        );
        assert!(
            text.contains("x0ne --setup"),
            "a warning with no remedy is noise: {text}"
        );
    }

    /// The other half of the property: a config that permits something says
    /// nothing. A warning that always fires is a warning nobody reads.
    ///
    /// The roots are a real directory, not `~/code`: this test used to name
    /// `~/code`, which does not exist on the machine that runs it, and passed
    /// only because the "this root does not resolve" warning did not exist yet.
    #[test]
    fn a_configured_allowlist_produces_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let toml = format!(
            r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1048576
max_list_entries = 500
allowed_commands = ["cargo"]
allowed_read_paths = ["{root}"]
allowed_write_paths = ["{root}"]
deny_path_patterns = []
"#
        );
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).expect("valid config");
        assert!(
            cfg.policy_warnings().is_empty(),
            "nothing is empty here: {:?}",
            cfg.policy_warnings()
        );
    }

    /// A configured root that does not exist is announced, not swallowed.
    ///
    /// Field report 2026-08-16 (session `2026-08-16_15-17_6b1c`): the policy
    /// said `allowed_read_paths = ["~/code"]`, `~/code` was not there,
    /// `normalize_roots` dropped it, and every read was refused with "no read
    /// paths are configured" — a message that describes a file the operator
    /// did not have. A failure here means a policy line can again be written,
    /// loaded, read, and inert, with nothing said about it.
    #[test]
    fn a_read_root_that_does_not_exist_is_named_in_the_startup_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("code-that-is-not-there");
        let mut cfg = AppConfig::defaults();
        cfg.policy.allowed_commands = vec!["cargo".into()];
        cfg.policy.allowed_read_paths = vec![missing.to_string_lossy().to_string()];
        cfg.policy.allowed_write_paths = vec![dir.path().to_string_lossy().to_string()];

        let warnings = cfg.policy_warnings().join("\n");
        assert!(
            warnings.contains("code-that-is-not-there"),
            "the dead root is not named: {warnings}"
        );
        assert!(
            warnings.contains("allowed_read_paths") && warnings.contains("EMPTY"),
            "the warning does not say the list ends up empty: {warnings}"
        );
        assert!(
            !warnings.contains("allowed_write_paths"),
            "a resolvable write root must stay silent: {warnings}"
        );
    }

    /// Every refusal an operator can hit names the setting that lifts it.
    ///
    /// The hint is the only channel a refusal has. A failure means a denial
    /// that tells the operator what happened and not what to do about it —
    /// which is the second half of the 2026-08-15 report.
    #[test]
    fn a_policy_refusal_names_the_setting_that_lifts_it() {
        use crate::policy::DenyReason;
        for reason in [
            DenyReason::ActionDisabled,
            DenyReason::CommandNotAllowed {
                command: "cargo".into(),
            },
            DenyReason::PathNotInAllowlist {
                path: "/srv/x".into(),
                access: crate::policy::PathAccess::Read,
            },
        ] {
            let msg = reason.message();
            assert!(
                msg.contains("x0ne --setup"),
                "{reason:?} gives the operator nowhere to go: {msg}"
            );
            assert!(
                msg.contains("x0ne-runtime.toml"),
                "{reason:?} does not name the file: {msg}"
            );
        }
    }
}
