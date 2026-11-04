use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level configuration for x0ne-orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// LLM backend configuration.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Bridge: how to talk to x0ne-runtime.
    #[serde(default)]
    pub bridge: BridgeConfig,
    /// Conversation window settings.
    #[serde(default)]
    pub conversation: ConversationConfig,
    /// Handle management settings.
    #[serde(default)]
    pub handles: HandlesConfig,
    /// Display / formatting preferences.
    #[serde(default)]
    pub display: DisplayConfig,
    /// Cost tracking settings.
    #[serde(default)]
    pub cost: CostConfig,
    /// UI / interaction preferences.
    #[serde(default)]
    pub ui: UiConfig,
    /// Incus (LXC) container management — orchestrator-side, host-only.
    /// Snapshot management and ephemeral container execution live HERE,
    /// not in x0ne-runtime, because the runtime may run *inside* the
    /// container it would otherwise be asked to manage.
    #[serde(default)]
    pub incus: IncusConfig,
    /// Cloud cost + GPU monitoring — orchestrator-side, like the sysmon
    /// monitor and the navbar it feeds.
    #[serde(default)]
    pub cloud: CloudConfig,
    /// Directories where the human gate is dispensed — see [`TrustConfig`].
    #[serde(default)]
    pub trust: TrustConfig,
    /// Directories whose own project config is applied in full. Read from the
    /// user's config only — see [`ProjectTrustConfig`].
    #[serde(default, skip_serializing_if = "ProjectTrustConfig::is_empty")]
    pub project_trust: ProjectTrustConfig,
    /// Third-party MCP servers (`crate::connectors`). Not in
    /// `PROJECT_SETTABLE`: a project config cannot start a process.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub connectors: std::collections::BTreeMap<String, crate::connectors::ConnectorConfig>,
    /// Pre-tool hooks (`crate::hooks`). Not in `PROJECT_SETTABLE`: a project
    /// config cannot start a process nor drop the user's hooks.
    #[serde(default, skip_serializing_if = "crate::hooks::HooksConfig::is_empty")]
    pub hooks: crate::hooks::HooksConfig,
}

/// `privileged`: directories (absolute or `~/…`; relative entries are ignored)
/// whose `x0ne-orchestrator.toml`, once trusted, is applied in full instead of
/// its display keys only. Exact directory, not its subdirectories. Never taken
/// from a project config (owner decision « option a », 2026-09-23).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectTrustConfig {
    #[serde(default)]
    pub privileged: Vec<String>,
}

impl ProjectTrustConfig {
    fn is_empty(&self) -> bool {
        self.privileged.is_empty()
    }
}

/// The human gate, dispensed inside named directories.
///
/// `roots` are directories (relative paths resolve against the working
/// directory, `~` expands). An action whose every path resolves under one of
/// them — and, for a command, whose `cwd` does — runs without the approval
/// prompt, provided its side-effect class is one that stays inside the
/// directory: reads, writes, commands, `MoveToTrash`. Never dispensed, whatever
/// the paths: network, desktop, the host/Incus operations, snapshot restore
/// and delete (invariant 7 — the safety net itself), `ArtifactExpose`,
/// `Budget`, and any composite one of whose children is not dispensable
/// (invariant 4). The RUNTIME's policy is untouched: its allowlists and path
/// confinement still apply, this only stops asking a person about what they
/// have already said is theirs to lose.
///
/// Presentation of the trust is mandatory (the CLI banner and the TUI header
/// name the roots) and sub-agents do not inherit it (`sysaicalls`): both are
/// the rules set for a confirmation-free mode. Owner request,
/// 2026-08-17: « let it do the playground without having to type allow once all
/// the time — but only in this directory ».
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustConfig {
    #[serde(default)]
    pub roots: Vec<String>,
    /// Bare program names dispensed inside the roots beyond the built-in list
    /// (`gcc`, `chmod`, …). Every path argument is still checked; shells and
    /// launchers are refused even if listed (`agent::trust`). 2026-10-12.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workdir_programs: Vec<String>,
}

// ── Cloud monitoring config ────────────────────────────────────────────────────

/// Cloud cost + GPU monitoring. Off by default: polling `gcloud` on a machine
/// with no cloud footprint is noise. The CloudCost/GpuStatus actions are gated
/// separately by the runtime's `allowed_commands` (gcloud/nvidia-smi), so this
/// flag governs only the background monitor + its display.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudConfig {
    #[serde(default)]
    pub enabled: bool,
}

// ── LLM provider config ───────────────────────────────────────────────────────

/// Which LLM backend to use, and its settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// "anthropic" | "ollama" | "openai" | "openai-compat"
    #[serde(default = "default_provider")]
    pub provider: String,

    #[serde(default)]
    pub anthropic: AnthropicConfig,

    #[serde(default)]
    pub ollama: OllamaConfig,

    /// Native OpenAI API (api.openai.com). Reads OPENAI_API_KEY by default.
    #[serde(default)]
    pub openai: OpenAIConfig,

    /// OpenAI-compatible endpoint (LiteLLM, remote Ollama /v1, vLLM, …) —
    /// the *active* compat profile. Always reflects the provider currently
    /// in use when `provider == "openai-compat"`. Saved profiles live in
    /// `compat_profiles` and the `/provider` slash command swaps one in here
    /// when the user picks a different name.
    #[serde(default)]
    pub openai_compat: OpenAICompatConfig,

    /// Saved openai-compatible profiles (e.g. one per third-party API key:
    /// kimi, r9s, groq, …). The `name` field on each profile is what the
    /// user types in `/provider <name>`. The currently-active one is
    /// duplicated into `openai_compat` above so the wire format consumers
    /// (`api::create_backend`) don't have to know about the multi-profile
    /// machinery — they just read the active slot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compat_profiles: Vec<OpenAICompatConfig>,

    /// The `x0ne-engine` provider: a confidential Engine session's relay.
    #[serde(default, skip_serializing_if = "X0neEngineConfig::is_default")]
    pub x0ne_engine: X0neEngineConfig,

    /// Set when a file, an env var or a flag named the provider. False means
    /// startup may pick it from the key it finds.
    #[serde(skip)]
    pub provider_explicit: bool,

    /// `--endpoint` and `--model`, kept so they still win once startup
    /// picks the provider. Never read from or written to a file.
    #[serde(skip)]
    pub model_flags: ModelFlags,
}

/// `--endpoint` and `--model`: the last layer, over the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFlags {
    /// The Ollama or OpenAI-compatible endpoint of the provider that runs.
    pub endpoint: Option<String>,
    /// The model of the provider that runs.
    pub model: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            anthropic: AnthropicConfig::default(),
            ollama: OllamaConfig::default(),
            openai: OpenAIConfig::default(),
            openai_compat: OpenAICompatConfig::default(),
            compat_profiles: Vec::new(),
            x0ne_engine: X0neEngineConfig::default(),
            provider_explicit: false,
            model_flags: ModelFlags::default(),
        }
    }
}

fn default_provider() -> String {
    "anthropic".into()
}

/// Anthropic Messages API settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    /// Variable that holds the API key. Unset: `ANTHROPIC_API_KEY`, a key
    /// the config does not name ([`crate::model_keys`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Model ID.
    #[serde(default = "default_anthropic_model")]
    pub model: String,
    /// Max tokens in the response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: None,
            model: default_anthropic_model(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl AnthropicConfig {
    /// The variable named for the key: `api_key_env`, else `ANTHROPIC_API_KEY`.
    pub fn key_var(&self) -> &str {
        self.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY")
    }
}

fn default_anthropic_model() -> String {
    // Keep this on a current model. The previous default
    // (claude-sonnet-4-20250514) reached its retirement date and every fresh
    // install then failed with an opaque `404 not_found_error` naming a model
    // id the user never chose.
    "claude-sonnet-5".into()
}
fn default_max_tokens() -> u32 {
    4096
}

/// Ollama inference settings (local or remote).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    /// Base URL for the Ollama server. `OLLAMA_BASE_URL` replaces it, and
    /// `--endpoint` replaces both ([`Config::resolve`]).
    #[serde(default = "default_ollama_endpoint")]
    pub endpoint: String,
    /// Model name as registered in Ollama (e.g. "llama3.2", "mistral", "qwen2.5-coder").
    #[serde(default = "default_ollama_model")]
    pub model: String,
    /// Max tokens in the response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Context window asked of Ollama on every call (`options.num_ctx`),
    /// prompt and answer together. The conversation budget of an Ollama
    /// session stays under it, minus `max_tokens` ([`Config::context_budget`]).
    #[serde(default = "default_ollama_num_ctx")]
    pub num_ctx: u32,
    /// Variable holding the API key, sent as `Authorization: Bearer`.
    /// Unset: `OLLAMA_API_KEY`, which then goes to ollama.com only; behind
    /// an auth proxy, name it: `api_key_env = "OLLAMA_API_KEY"`
    /// ([`crate::model_keys`]). `""`: no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            endpoint: default_ollama_endpoint(),
            model: default_ollama_model(),
            max_tokens: default_max_tokens(),
            num_ctx: default_ollama_num_ctx(),
            api_key_env: None,
        }
    }
}

impl OllamaConfig {
    /// The variable the key is read from: `api_key_env`, else `OLLAMA_API_KEY`.
    pub fn key_var(&self) -> &str {
        self.api_key_env.as_deref().unwrap_or("OLLAMA_API_KEY")
    }
}

fn default_ollama_endpoint() -> String {
    "http://localhost:11434".into()
}
fn default_ollama_model() -> String {
    "llama3.2".into()
}
/// Ollama's own default on a 23-47 GiB GPU (server/routes.go), the window
/// measured on the 5090 on 2026-10-16, and Qwen3's native context.
fn default_ollama_num_ctx() -> u32 {
    32_768
}

// ── Native OpenAI config ──────────────────────────────────────────────────────

/// Native OpenAI API settings (api.openai.com).
///
/// Set `provider = "openai"` and export `OPENAI_API_KEY=sk-...`.
/// The base URL is fixed to `https://api.openai.com` — use `openai-compat`
/// if you need to point at a proxy or a different endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    /// Model ID. Recommended:
    ///   Dev / exploration  → "gpt-4o-mini"  (cheap, fast, great for agentic loops)
    ///   Production / heavy → "gpt-4o"        (best quality)
    ///   Reasoning tasks    → "o4-mini"       (o-series, slower but stronger)
    #[serde(default = "default_openai_model")]
    pub model: String,

    /// Max tokens in the response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Variable that holds the API key. Unset: `OPENAI_API_KEY`, a key the
    /// config does not name ([`crate::model_keys`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            model: default_openai_model(),
            max_tokens: default_max_tokens(),
            api_key_env: None,
        }
    }
}

impl OpenAIConfig {
    /// The variable the key is read from: `api_key_env`, else `OPENAI_API_KEY`.
    pub fn key_var(&self) -> &str {
        self.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY")
    }
}

fn default_openai_model() -> String {
    // gpt-4o-mini: 40× cheaper than gpt-4o, near-equivalent for agentic tasks.
    // Switch to "gpt-4o" in production or for complex reasoning.
    "gpt-4o-mini".into()
}

/// OpenAI-compatible API settings.
///
/// Works with LiteLLM proxy, remote Ollama (/v1/chat/completions), vLLM, LocalAI,
/// Groq, Together.ai, OpenRouter, and OpenAI itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAICompatConfig {
    /// Optional short name used to identify this profile in `/provider <name>`
    /// and `/provider list`. Auto-derived from the URL hostname when the
    /// user doesn't supply one (e.g. `kimi`, `r9s`, `groq`). The legacy
    /// active slot may have no name — that's fine, it just shows as the
    /// current default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Root URL of the server — do NOT include /v1/chat/completions.
    /// Examples:
    ///   LiteLLM:       http://localhost:4000
    ///   Remote Ollama: http://my-server:11434
    ///   vLLM:          http://my-server:8000
    ///   Groq:          <https://api.groq.com/openai>
    ///   OpenRouter:    <https://openrouter.ai/api>
    #[serde(default = "default_openai_compat_base_url")]
    pub base_url: String,

    /// Model name as understood by the server.
    /// For LiteLLM: e.g. "ollama/llama3.2", "anthropic/claude-sonnet-5"
    /// For remote Ollama: e.g. "llama3.2"
    /// For Groq: e.g. "llama-3.3-70b-versatile"
    #[serde(default = "default_openai_compat_model")]
    pub model: String,

    /// Max tokens in the response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// API key (Bearer token). Leave empty for local servers that don't need auth.
    /// `X0NE_OPENAI_COMPAT_KEY` replaces it.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Variable that holds the API key, when `api_key` is empty. Unset:
    /// `OPENAI_API_KEY`, which then goes to api.openai.com only; elsewhere,
    /// name it: `api_key_env = "OPENAI_API_KEY"` ([`crate::model_keys`]).
    /// `""`: no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl Default for OpenAICompatConfig {
    fn default() -> Self {
        Self {
            name: None,
            base_url: default_openai_compat_base_url(),
            model: default_openai_compat_model(),
            max_tokens: default_max_tokens(),
            api_key: None,
            api_key_env: None,
        }
    }
}

/// Best-effort short name derived from a base URL (used when the user
/// adds a profile without specifying one). Returns the second-level
/// hostname when possible, else a fallback. Examples:
///   `https://api.r9s.ai/v1`       → "r9s"
///   `https://taotoken.net/api/v1` → "taotoken"
///   "http://localhost:4000"       → "localhost"
///
/// The host is the one reqwest reads, never the userinfo. Review of the pre-release build
/// (S3): the text split named a profile `sk-secret@host` after its key.
pub fn derive_compat_name(base_url: &str) -> String {
    let host = reqwest::Url::parse(base_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if host.is_empty() {
        return "compat".to_string();
    }
    let labels: Vec<&str> = host.split('.').collect();
    let candidate = if labels.len() >= 2 {
        labels[labels.len() - 2]
    } else {
        labels[0]
    };
    candidate.to_ascii_lowercase()
}

fn default_openai_compat_base_url() -> String {
    "http://localhost:4000".into()
}
fn default_openai_compat_model() -> String {
    "gpt-4o-mini".into()
}

// ── x0ne-engine ────────────────────────────────────────────────────────

/// `provider = "x0ne-engine"`: the local relay of `x0ne-engine cloud up`. No
/// inline key: it is read from `key_file` before every call
/// (`crate::engine_local`). Unknown keys are refused, `api_key` included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X0neEngineConfig {
    /// Loopback only; port 8477 (a server without attestation) is refused.
    #[serde(default = "default_x0ne_engine_base_url")]
    pub base_url: String,
    /// Empty: the model the relay's status names.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Default `$XDG_RUNTIME_DIR/x0ne-engine/local.key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    /// `false` is refused when the backend is built.
    #[serde(default = "default_require_verified")]
    pub require_verified: bool,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

impl Default for X0neEngineConfig {
    fn default() -> Self {
        Self {
            base_url: default_x0ne_engine_base_url(),
            model: String::new(),
            key_file: None,
            require_verified: default_require_verified(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl X0neEngineConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_x0ne_engine_base_url() -> String {
    crate::engine_local::DEFAULT_BASE_URL.into()
}
fn default_require_verified() -> bool {
    true
}

// ── Incus (LXC) — orchestrator-side ──────────────────────────────────────────
//
// Lives in the orchestrator because container management is structurally
// outside the runtime's privilege boundary. The runtime may run *inside*
// the very container these actions would manage; only the orchestrator,
// running on the host, has both the `incus` CLI and the topological
// awareness to safely operate on containers.

/// Configuration for Incus (LXC) container actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IncusConfig {
    /// Remote to use when the LLM doesn't specify one (must be in `incus remote list`).
    pub default_remote: String,
    /// Image allowlist for `RunIncus` / `RunIncusGroup`. **Empty = all denied**
    /// (fail-closed). Snapshot actions don't consult this list — they
    /// operate on existing containers identified by name.
    pub allowed_images: Vec<String>,
    /// Default RAM limit per container in MB.
    pub default_memory_mb: u64,
    /// Hard cap on RAM the LLM can request.
    pub max_memory_mb: u64,
    /// Default vCPU limit.
    pub default_cpu_limit: u32,
    /// Default execution timeout in seconds.
    pub default_timeout_secs: u64,
    /// Hard cap on timeout the LLM can request.
    pub max_timeout_secs: u64,
    /// Maximum number of parallel containers in a `RunIncusGroup`.
    pub max_parallel_instances: usize,
    /// Default network mode: "none" (isolated) | "bridge".
    pub default_network: String,
}

impl Default for IncusConfig {
    fn default() -> Self {
        Self {
            default_remote: "local".into(),
            allowed_images: vec![],
            default_memory_mb: 512,
            max_memory_mb: 4096,
            default_cpu_limit: 1,
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            max_parallel_instances: 10,
            default_network: "none".into(),
        }
    }
}

/// How to communicate with x0ne-runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// "stdio", "http", or "sandbox" (ssh + incus exec)
    #[serde(default = "default_bridge_mode")]
    pub mode: String,
    /// Path to the x0ne-runtime binary (stdio mode).
    #[serde(default = "default_x0ne_core_bin")]
    pub x0ne_core_bin: String,
    /// Host:port of a running x0ne-runtime HTTP server (http mode).
    #[serde(default = "default_x0ne_core_addr")]
    pub x0ne_core_addr: String,
    /// Seconds to wait for health check before aborting startup.
    #[serde(default = "default_health_check_timeout_secs")]
    pub health_check_timeout_secs: u64,
    /// Full shell command to spawn x0ne-runtime (sandbox mode).
    /// Example: "ssh my-server incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode stdio"
    /// If set and mode == "sandbox", this is split on spaces and executed directly.
    #[serde(default)]
    pub sandbox_cmd: Option<String>,
    /// Policy file handed to the runtime as `--config` (stdio mode). Relative
    /// paths resolve against the working directory — the same directory a
    /// project-local `x0ne-orchestrator.toml` is read from, so a project can
    /// carry its own runtime policy beside its orchestrator config
    /// (`playground/`, 2026-08-17). Unset: the runtime resolves its policy
    /// itself (`$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`).
    #[serde(default)]
    pub runtime_config: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            mode: default_bridge_mode(),
            x0ne_core_bin: default_x0ne_core_bin(),
            x0ne_core_addr: default_x0ne_core_addr(),
            health_check_timeout_secs: default_health_check_timeout_secs(),
            sandbox_cmd: None,
            runtime_config: None,
        }
    }
}

fn default_bridge_mode() -> String {
    "stdio".into()
}
fn default_x0ne_core_bin() -> String {
    "x0ne-runtime".into()
}
fn default_x0ne_core_addr() -> String {
    "127.0.0.1:7878".into()
}
fn default_health_check_timeout_secs() -> u64 {
    5
}

// ── Conversation config ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationConfig {
    /// Rough context-window ceiling in tokens (chars / 4).
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    /// Trigger summarization when context reaches this % of max.
    #[serde(default = "default_summarize_at_percent")]
    pub summarize_at_percent: usize,
    /// After summarization, keep the most recent N% of messages intact.
    #[serde(default = "default_keep_recent_percent")]
    pub keep_recent_percent: usize,
    /// Floor on how many recent messages `trim_if_needed` is allowed to
    /// keep, regardless of budget. Default 8 (≈ 4 user/assistant turns).
    /// Previously the trim loop stopped at `messages.len() > 2`, which
    /// cratered `tokens_in` from ~16k down to ~1k on long sessions and
    /// caused the model to respond "I am ready. Please provide the task"
    /// as if a new session had started. Bumping this to 8 keeps enough
    /// recent history that the model sees an ongoing conversation even
    /// under severe budget pressure — if those 8 messages still bust the
    /// budget, we inject a `[Context truncated]` marker so the model
    /// knows history was cropped rather than missing.
    #[serde(default = "default_keep_recent_min_messages")]
    pub keep_recent_min_messages: usize,
    /// Cap, in bytes, on each string of an action result sent to the model
    /// (file content, stdout, stderr). Longer strings keep their head and a
    /// marker. Measured: an 825 KB CSV read entered whole (847k tokens).
    #[serde(default = "default_max_result_bytes")]
    pub max_result_bytes: usize,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: default_max_context_tokens(),
            summarize_at_percent: default_summarize_at_percent(),
            keep_recent_percent: default_keep_recent_percent(),
            keep_recent_min_messages: default_keep_recent_min_messages(),
            max_result_bytes: default_max_result_bytes(),
        }
    }
}

fn default_max_context_tokens() -> usize {
    100_000
}
fn default_summarize_at_percent() -> usize {
    80
}
fn default_keep_recent_percent() -> usize {
    40
}
fn default_keep_recent_min_messages() -> usize {
    8
}

fn default_max_result_bytes() -> usize {
    crate::conversation::DEFAULT_MAX_RESULT_BYTES
}

// ── Handles config ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlesConfig {
    /// Seconds of idleness before a handle is auto-checked before an API call.
    #[serde(default = "default_auto_check_interval_secs")]
    pub auto_check_interval_secs: u64,
}

impl Default for HandlesConfig {
    fn default() -> Self {
        Self {
            auto_check_interval_secs: default_auto_check_interval_secs(),
        }
    }
}

fn default_auto_check_interval_secs() -> u64 {
    60
}

// ── Display config ────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(default = "default_pipeline_max_output_lines")]
    pub pipeline_max_output_lines: usize,
    #[serde(default = "default_file_preview_lines")]
    pub file_preview_lines: usize,
    #[serde(default = "default_show_action_duration")]
    pub show_action_duration: bool,
    #[serde(default = "default_show_token_count")]
    pub show_token_count: bool,
    /// Maximum width (in columns) for wrapped LLM prose in CLI mode.
    /// Effective width is `min(terminal_width - 4, response_width)`, so on
    /// narrow terminals the response shrinks but never expands past this cap.
    /// 120 matches GitHub/Google modern style guides — wide enough to use
    /// real estate on 160-col widescreen terminals, narrow enough to stay
    /// readable without horizontal eye-tracking fatigue. Set 0 to disable
    /// the cap entirely (prose stretches to full terminal width).
    #[serde(default = "default_response_width")]
    pub response_width: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            pipeline_max_output_lines: default_pipeline_max_output_lines(),
            file_preview_lines: default_file_preview_lines(),
            show_action_duration: default_show_action_duration(),
            show_token_count: default_show_token_count(),
            response_width: default_response_width(),
        }
    }
}

fn default_pipeline_max_output_lines() -> usize {
    10
}
fn default_file_preview_lines() -> usize {
    5
}
fn default_show_action_duration() -> bool {
    true
}
fn default_show_token_count() -> bool {
    true
}
fn default_response_width() -> usize {
    120
}

// ── Cost config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostConfig {
    /// USD per million input tokens for a Claude Sonnet that the built-in
    /// list (`agent/pricing.rs`, Anthropic's page) does not name, such as the
    /// `sonnet` alias. A listed model is billed at its list price.
    #[serde(default = "default_sonnet_input_per_m")]
    pub sonnet_input_per_m: f64,
    /// USD per million output tokens for Claude Sonnet.
    #[serde(default = "default_sonnet_output_per_m")]
    pub sonnet_output_per_m: f64,
    /// USD per million input tokens for a Claude Haiku the built-in list does
    /// not name. Without a rate, a session with a budget stops at the first
    /// such call (cost unknown).
    #[serde(default = "default_haiku_input_per_m")]
    pub haiku_input_per_m: f64,
    /// USD per million output tokens for Claude Haiku.
    #[serde(default = "default_haiku_output_per_m")]
    pub haiku_output_per_m: f64,

    // ── OpenAI pricing ──────────────────────────────────────────────────
    /// USD per million input tokens for gpt-4o-mini (dev model).
    #[serde(default = "default_gpt4o_mini_input_per_m")]
    pub gpt4o_mini_input_per_m: f64,
    /// USD per million output tokens for gpt-4o-mini.
    #[serde(default = "default_gpt4o_mini_output_per_m")]
    pub gpt4o_mini_output_per_m: f64,
    /// USD per million input tokens for gpt-4o.
    #[serde(default = "default_gpt4o_input_per_m")]
    pub gpt4o_input_per_m: f64,
    /// USD per million output tokens for gpt-4o.
    #[serde(default = "default_gpt4o_output_per_m")]
    pub gpt4o_output_per_m: f64,

    // ── Budget guard ────────────────────────────────────────────────────
    /// Hard cap in USD per session. When the estimated spend reaches this
    /// threshold, the agent stops and reports the overage instead of
    /// making another LLM call. `0.0` (default) means no cap.
    ///
    /// Recommended for dev: set to `1.0` or `5.0` to catch runaway loops
    /// before they drain your OpenAI credits.
    ///
    /// Example in x0ne.toml:
    ///   `[cost]`
    ///   budget_usd_limit = 2.0
    ///
    /// Override at runtime: `X0NE_BUDGET_USD=5.0`
    #[serde(default)]
    pub budget_usd_limit: f64,
    /// Ceiling on the session cap. Neither the model nor `/budget` can raise
    /// the cap above it or turn it off. `0.0` means no ceiling. User config
    /// only: a project's `[cost]` table is refused (project_trust.rs).
    #[serde(default)]
    pub max_session_usd: f64,
    /// Actions the agent may chain in ONE turn before it hands the keyboard
    /// back (« Paused after N consecutive actions — say `continue` »). `0`
    /// means no such pause: the turn runs until the model stops, the cost
    /// ceiling bites, the failure ceilings bite, or the stationarity breaker
    /// fires. It was a compiled 20 until 2026-08-17 — a 19-exercise
    /// playground run stopped every 20 actions and the operator typed
    /// `continue` (« it's blocking »). Default 100; the playground sets 0.
    #[serde(default = "default_max_consecutive_actions")]
    pub max_consecutive_actions: usize,
    /// Tokens one parallel world may move (input, output and cache) before
    /// its next model call is refused. `0` means no token cap.
    #[serde(default)]
    pub world_max_tokens: u64,
}

fn default_max_consecutive_actions() -> usize {
    100
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            sonnet_input_per_m: default_sonnet_input_per_m(),
            sonnet_output_per_m: default_sonnet_output_per_m(),
            haiku_input_per_m: default_haiku_input_per_m(),
            haiku_output_per_m: default_haiku_output_per_m(),
            gpt4o_mini_input_per_m: default_gpt4o_mini_input_per_m(),
            gpt4o_mini_output_per_m: default_gpt4o_mini_output_per_m(),
            gpt4o_input_per_m: default_gpt4o_input_per_m(),
            gpt4o_output_per_m: default_gpt4o_output_per_m(),
            budget_usd_limit: 0.0,
            max_session_usd: 0.0,
            max_consecutive_actions: default_max_consecutive_actions(),
            world_max_tokens: 0,
        }
    }
}

fn default_sonnet_input_per_m() -> f64 {
    3.0
}
fn default_sonnet_output_per_m() -> f64 {
    15.0
}
// Claude Haiku 4.5, Anthropic first-party rates: $1/1M input, $5/1M output.
fn default_haiku_input_per_m() -> f64 {
    1.0
}
fn default_haiku_output_per_m() -> f64 {
    5.0
}
// gpt-4o-mini pricing (as of 2025-04): $0.15/1M input, $0.60/1M output
fn default_gpt4o_mini_input_per_m() -> f64 {
    0.15
}
fn default_gpt4o_mini_output_per_m() -> f64 {
    0.60
}
// gpt-4o pricing (as of 2025-04): $2.50/1M input, $10.00/1M output
fn default_gpt4o_input_per_m() -> f64 {
    2.50
}
fn default_gpt4o_output_per_m() -> f64 {
    10.00
}

// ── UI / interaction config ───────────────────────────────────────────────────

/// Controls interactive UX behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Force compact system prompt (~300 tokens) regardless of model.
    #[serde(default)]
    pub compact_prompt: bool,
    /// Force full system prompt even for small models.
    #[serde(default)]
    pub force_full_prompt: bool,
    /// Lazy discovery: minimal system prompt + ListActions injected into
    /// conversation at boot. LLM uses DescribeAction on-demand.
    /// Default: true. Saves ~1500 tokens per API call.
    #[serde(default = "default_lazy_discovery")]
    pub lazy_discovery: bool,
    /// Ask for confirmation before write/execute actions. Default: true.
    #[serde(default = "default_confirm_writes")]
    pub confirm_writes: bool,
    /// Toggled by `/auto`. Skips no confirmation: the approval gate
    /// neutralises it (`agent/confirm.rs`); it shows the REPL's `auto` badge.
    #[serde(default)]
    pub auto_confirm: bool,
    /// After each REPL turn, if any parallel world is still running, open the
    /// same inline multi-column view as `/worlds` until all finish (or Ctrl+C).
    /// Default: true. Toggle with `X0NE_AUTO_WORLDS` or `[ui] auto_worlds_view`.
    #[serde(default = "default_auto_worlds_view")]
    pub auto_worlds_view: bool,
    /// One-line parallel-worlds status in the CLI log instead of the ASCII grid.
    /// Default: false. Set `X0NE_WORLDS_ONE_LINE=1` or `[ui] worlds_one_line = true`.
    #[serde(default)]
    pub worlds_one_line: bool,
    /// Longest wait for one model answer in `--tui` and `--matrix`, retries
    /// included; 0 = no bound. 300 = the OpenAI-compat and Ollama clients'
    /// own per-request timeout, so no answer they accept is cut.
    #[serde(default = "default_model_timeout_secs")]
    pub model_timeout_secs: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            compact_prompt: false,
            force_full_prompt: false,
            lazy_discovery: true,
            confirm_writes: default_confirm_writes(),
            auto_confirm: false,
            auto_worlds_view: true,
            worlds_one_line: false,
            model_timeout_secs: default_model_timeout_secs(),
        }
    }
}

fn default_model_timeout_secs() -> u64 {
    300
}

fn default_confirm_writes() -> bool {
    true
}
fn default_lazy_discovery() -> bool {
    true
}
fn default_auto_worlds_view() -> bool {
    true
}

impl UiConfig {
    /// `model_timeout_secs` as a deadline; `None` for 0.
    pub fn model_deadline(&self) -> Option<std::time::Duration> {
        (self.model_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(self.model_timeout_secs))
    }

    /// Returns a human-readable string for the current confirmation mode.
    pub fn auto_confirm_display(&self) -> &'static str {
        if self.auto_confirm {
            "auto"
        } else {
            "confirm"
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl Config {
    /// The conversation budget, in tokens. With Ollama it stays under the
    /// window asked of the server, minus the answer: at 100k against a 32k
    /// window the server cut the prompt before the harness compacted.
    pub fn context_budget(&self) -> usize {
        let budget = self.conversation.max_context_tokens;
        if self.llm.provider != "ollama" {
            return budget;
        }
        let ollama = &self.llm.ollama;
        budget.min(ollama.num_ctx.saturating_sub(ollama.max_tokens) as usize)
    }

    /// Return the active model name for display purposes.
    pub fn active_model_name(&self) -> String {
        match self.llm.provider.as_str() {
            "ollama" => self.llm.ollama.model.clone(),
            "openai" => self.llm.openai.model.clone(),
            "openai-compat" | "openai_compat" | "litellm" => self.llm.openai_compat.model.clone(),
            p if crate::engine_local::is_engine_provider(p) => self.llm.x0ne_engine.model.clone(),
            _ => self.llm.anthropic.model.clone(),
        }
    }

    /// The endpoint the active provider talks to, when it is a URL this
    /// config knows.
    ///
    /// `None` for a hosted API that has no configurable address (Anthropic,
    /// OpenAI): "we do not know where" and "somewhere on the internet" are the
    /// same answer to the only question this is asked — see
    /// [`crate::tui::app::EnclaveStatus::for_session`], which treats `None`
    /// as not-loopback rather than as unknown.
    pub fn active_base_url(&self) -> Option<&str> {
        match self.llm.provider.as_str() {
            "ollama" => Some(&self.llm.ollama.endpoint),
            "openai-compat" | "openai_compat" | "litellm" => Some(&self.llm.openai_compat.base_url),
            _ => None,
        }
    }

    /// Return the active model name for display purposes (borrowed).
    #[allow(dead_code)]
    pub fn active_model(&self) -> &str {
        match self.llm.provider.as_str() {
            "ollama" => &self.llm.ollama.model,
            "openai" => &self.llm.openai.model,
            "openai-compat" | "openai_compat" | "litellm" => &self.llm.openai_compat.model,
            p if crate::engine_local::is_engine_provider(p) => &self.llm.x0ne_engine.model,
            _ => &self.llm.anthropic.model,
        }
    }

    /// Set the model of the active provider.
    pub fn set_active_model(&mut self, model: String) {
        match self.llm.provider.as_str() {
            "ollama" => self.llm.ollama.model = model,
            "openai" => self.llm.openai.model = model,
            "openai-compat" | "openai_compat" | "litellm" => self.llm.openai_compat.model = model,
            p if crate::engine_local::is_engine_provider(p) => self.llm.x0ne_engine.model = model,
            _ => self.llm.anthropic.model = model,
        }
    }

    /// Load from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read config file: {}", path.display()))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Invalid config syntax in: {}", path.display()))?;
        config.llm.provider_explicit = declares_llm_provider(&content);
        Ok(config)
    }

    /// Save the config to a TOML file. Creates parent directories and
    /// sets permissions to 600 (owner-only) since the file may contain
    /// API keys.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create config directory: {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self).context("Failed to serialize config to TOML")?;
        std::fs::write(path, &content)
            .with_context(|| format!("Cannot write config file: {}", path.display()))?;
        // Security: config may contain API keys — restrict to owner-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Returns the default user-global config path: `~/.x0ne/config.toml`
    pub fn default_user_config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".x0ne")
            .join("config.toml")
    }

    /// Find and load, checking standard locations in order:
    /// 1. `--config FILE`
    /// 2. the user's config: `~/.config/x0ne-orchestrator/config.toml`, else
    ///    `~/.x0ne/config.toml`, else the system layer ([`SYSTEM_CONFIG_PATH`],
    ///    only if root-owned and not writable by others), else compiled-in defaults
    /// 3. over it, `./x0ne-orchestrator.toml` — only if this folder is trusted
    ///    (`project_trust`), and only the keys `PROJECT_SETTABLE` lists — all
    ///    of it if the user config lists this folder in `[project_trust] privileged`.
    pub fn find_and_load(override_path: Option<&PathBuf>) -> Result<Self> {
        if let Some(p) = override_path {
            return Self::load(p);
        }
        let user = Self::find_and_load_user()?;
        let cwd_cfg = PathBuf::from("x0ne-orchestrator.toml");
        if !cwd_cfg.is_file() {
            return Ok(user);
        }
        if !crate::project_trust::project_is_trusted() {
            // `x0ne` asks before loading; anything that reaches here unanswered
            // (`--health`, `--serve`, a script) runs on the user's config.
            tracing::warn!("ignoring ./x0ne-orchestrator.toml: this folder is not trusted");
            return Ok(user);
        }
        let cwd = std::env::current_dir().context("cannot read the working directory")?;
        // The bytes the trust decision approved, or none: a file refused on
        // the question, or edited since, is not applied.
        let Some(bytes) =
            crate::project_trust::read_trusted(&cwd, crate::project_trust::PROJECT_FILES[0])
        else {
            tracing::warn!(
                "ignoring ./x0ne-orchestrator.toml: not the file this folder was trusted with"
            );
            return Ok(user);
        };
        Self::overlay_project_bytes(user, bytes, &cwd_cfg, &cwd)
    }

    /// [`Self::overlay_project_bytes`] on `project` read by `project_trust::read_inside`,
    /// with no trust decision: for tests.
    #[cfg(test)]
    fn overlay_project(user: Self, project: &Path, dir: &Path) -> Result<Self> {
        let bytes = match crate::project_trust::read_inside(dir, project)
            .with_context(|| format!("Cannot read {}", project.display()))?
        {
            crate::project_trust::Inside::Bytes { bytes, .. } => bytes,
            crate::project_trust::Inside::Refused { real, why } => {
                tracing::warn!("ignoring {}: {}, {why}", project.display(), real.display());
                return Ok(user);
            }
            crate::project_trust::Inside::Absent => return Ok(user),
        };
        Self::overlay_project_bytes(user, bytes, project, dir)
    }

    /// `bytes`, the content of `project` (found in `dir`), applied over
    /// `user`: in full if the user's own config lists `dir` as privileged,
    /// else only what a project may set.
    fn overlay_project_bytes(
        user: Self,
        bytes: Vec<u8>,
        project: &Path,
        dir: &Path,
    ) -> Result<Self> {
        let text = String::from_utf8(bytes)
            .with_context(|| format!("Cannot read {}: not UTF-8", project.display()))?;
        let mut project: toml::Value = toml::from_str(&text)
            .with_context(|| format!("Invalid TOML in {}", project.display()))?;
        let privileged = crate::project_trust::is_privileged(dir, &user.project_trust.privileged);
        let mut merged =
            toml::Value::try_from(&user).context("cannot represent the user config")?;
        // The trust fingerprint covers a runtime policy inside `dir` only.
        let mut outside = Vec::new();
        if privileged {
            if let Err(e) = crate::project_trust::runtime_policy_of(dir, &project) {
                tracing::warn!("./x0ne-orchestrator.toml: {e:#}");
                if let Some(b) = project.get_mut("bridge").and_then(|b| b.as_table_mut()) {
                    b.remove("runtime_config");
                }
                outside.push("bridge.runtime_config".to_string());
            }
        }
        let mut refused = if privileged {
            tracing::warn!(
                "./x0ne-orchestrator.toml applied in full: {} is privileged in your config",
                dir.display()
            );
            crate::project_trust::overlay_privileged_project_config(&mut merged, &project)
        } else {
            crate::project_trust::overlay_project_config(&mut merged, &project)
        };
        refused.extend(outside);
        if !refused.is_empty() {
            tracing::warn!(
                "./x0ne-orchestrator.toml: ignored {} (only your own config sets these)",
                refused.join(", ")
            );
        }
        let explicit = user.llm.provider_explicit
            || project.get("llm").and_then(|l| l.get("provider")).is_some();
        let mut merged: Self = merged
            .try_into()
            .context("the project config does not fit the config schema")?;
        merged.llm.provider_explicit = explicit;
        Ok(merged)
    }

    /// The user's own config, never the working directory's. Without one,
    /// the system layer ([`SYSTEM_CONFIG_PATH`]) if it is trusted.
    pub(crate) fn find_and_load_user() -> Result<Self> {
        let user_cfg = dirs::config_dir().map(|d| d.join("x0ne-orchestrator").join("config.toml"));
        Self::load_user_or_system(
            user_cfg.as_deref(),
            &Self::default_user_config_path(),
            &SystemLayer::real(),
        )
    }

    fn load_user_or_system(
        user_cfg: Option<&Path>,
        user_global: &Path,
        system: &SystemLayer,
    ) -> Result<Self> {
        if let Some(user_cfg) = user_cfg.filter(|p| p.exists()) {
            let mut config = Self::load(user_cfg)?;
            config.overlay_user_llm(user_global)?;
            return Ok(config);
        }

        // User-global persistent config (~/.x0ne/config.toml)
        if user_global.exists() {
            return Self::load(user_global);
        }

        if let Some(config) = Self::load_system(system)? {
            return Ok(config);
        }
        Ok(Self::default())
    }

    /// The system layer, or `None` when it is absent or not trusted (a warning
    /// says why). A trusted file that does not parse is an error.
    fn load_system(system: &SystemLayer) -> Result<Option<Self>> {
        use std::io::Read;
        let shown = system.path.display();
        let mut file = match open_system_layer(system) {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(None),
            Err(why) => {
                tracing::warn!("ignoring the system config {shown}: {why}");
                return Ok(None);
            }
        };
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("Cannot read config file: {shown}"))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Invalid config syntax in: {shown}"))?;
        config.llm.provider_explicit = declares_llm_provider(&content);
        if !config.project_trust.is_empty() {
            tracing::warn!("{shown}: [project_trust] ignored, only your own config sets it");
            config.project_trust = ProjectTrustConfig::default();
        }
        Ok(Some(config))
    }

    /// If `~/.x0ne/config.toml` exists, replace the LLM section with the
    /// user's saved settings. This ensures that a project-local
    /// `x0ne-orchestrator.toml` can set bridge/display/ui without
    /// overriding the user's chosen LLM provider.
    fn overlay_user_llm(&mut self, user_global: &Path) -> Result<()> {
        if user_global.exists() {
            // A present-but-broken ~/.x0ne/config.toml must surface, not be
            // swallowed: silently skipping the overlay leaves the session on
            // the project-local LLM settings, so the user's chosen provider
            // and model are dropped with no indication anything went wrong.
            let saved = Self::load(user_global)?;
            self.llm = saved.llm;
        }
        Ok(())
    }

    /// LLM provider and model from the environment (`get` reads a variable).
    /// `X0NE_LLM_MODEL` is an alias of `X0NE_MODEL`, which wins.
    pub fn apply_llm_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        let get = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if let Some(v) = get("X0NE_LLM_PROVIDER") {
            self.llm.provider = v;
            self.llm.provider_explicit = true;
        }
        if let Some(v) = get("X0NE_MODEL").or_else(|| get("X0NE_LLM_MODEL")) {
            self.set_active_model(v);
        }
    }

    /// Apply environment variable overrides after loading from file.
    /// Env vars take precedence over toml values.
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_from(|k| std::env::var(k).ok());
    }

    /// The config a session runs on: `file` (else the search), then the
    /// environment (`env` reads a variable), then `--mode`, `--provider`,
    /// `--endpoint` and `--model`. A flag beats the environment, which beats
    /// the file.
    pub fn resolve(
        file: Option<&PathBuf>,
        env: impl Fn(&str) -> Option<String>,
        mode: Option<&str>,
        provider: Option<&str>,
        model_flags: ModelFlags,
    ) -> Result<Self> {
        let mut config = Self::find_and_load(file)?;
        // `--provider` stands in for `X0NE_LLM_PROVIDER`: `X0NE_MODEL` then
        // lands on the provider that runs, not on the one the flag replaced.
        config.apply_env_from(|k| match provider {
            Some(p) if k == "X0NE_LLM_PROVIDER" => Some(p.to_string()),
            _ => env(k),
        });
        if let Some(mode) = mode {
            config.bridge.mode = mode.to_string();
        }
        if let Some(provider) = provider {
            config.llm.provider = provider.to_string();
            config.llm.provider_explicit = true;
        }
        config.llm.model_flags = model_flags;
        config.apply_model_flags();
        Ok(config)
    }

    /// `--endpoint` and `--model` on the provider that runs. Called by
    /// [`Self::resolve`], and again when startup picks the provider.
    pub fn apply_model_flags(&mut self) {
        let ModelFlags { endpoint, model } = self.llm.model_flags.clone();
        if let Some(endpoint) = endpoint {
            match self.llm.provider.as_str() {
                "ollama" => self.llm.ollama.endpoint = endpoint,
                "openai-compat" | "openai_compat" | "litellm" => {
                    self.llm.openai_compat.base_url = endpoint
                }
                _ => {} // Anthropic and OpenAI have a fixed endpoint
            }
        }
        if let Some(model) = model {
            self.set_active_model(model);
        }
    }

    /// [`Self::apply_env_overrides`], `get` reading a variable.
    pub fn apply_env_from(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("X0NE_BRIDGE_MODE") {
            self.bridge.mode = v;
        }
        if let Some(v) = get("X0NE_SANDBOX_CMD") {
            self.bridge.sandbox_cmd = Some(v);
        }
        // Override the x0ne-runtime binary path used by stdio/sandbox bridges.
        //
        // Why this matters: the default `x0ne-runtime` resolves via `$PATH`, which
        // on dev machines often points to a stale `cargo install` copy (e.g.
        // `~/.cargo/bin/x0ne-runtime`) that doesn't know about the latest
        // `Action` variants. `make self-test` and CI should always run against
        // the freshly-built sibling binary — setting `X0NE_RUNTIME_BIN` forces it.
        //
        // `X0NE_CORE_BIN` is still honoured as a deprecated alias for users whose
        // scripts predate the x0ne-core → x0ne-runtime rename. New code should
        // always emit `X0NE_RUNTIME_BIN`; the legacy alias only fires when the
        // canonical name is unset, so updating one variable is enough.
        let runtime_bin = get("X0NE_RUNTIME_BIN").or_else(|| get("X0NE_CORE_BIN"));
        if let Some(v) = runtime_bin {
            if !v.is_empty() {
                self.bridge.x0ne_core_bin = v;
            }
        }
        let runtime_addr = get("X0NE_RUNTIME_ADDR").or_else(|| get("X0NE_CORE_ADDR"));
        if let Some(v) = runtime_addr {
            if !v.is_empty() {
                self.bridge.x0ne_core_addr = v;
            }
        }

        // `X0NE_OPENAI_COMPAT_KEY` is read with the other keys, never copied
        // here: a saved config does not write it (`crate::model_keys`).
        self.apply_llm_env(&get);
        if let Some(v) = get("X0NE_OPENAI_COMPAT_BASE_URL") {
            self.llm.openai_compat.base_url = v;
        }
        // An environment layer like the others: `--endpoint` beats it.
        if let Some(v) = get("OLLAMA_BASE_URL").filter(|v| !v.is_empty()) {
            self.llm.ollama.endpoint = v;
        }

        if let Some(v) = get("X0NE_AUTO_WORLDS") {
            let lower = v.to_lowercase();
            if matches!(lower.as_str(), "1" | "true" | "yes" | "y" | "on") {
                self.ui.auto_worlds_view = true;
            } else if matches!(lower.as_str(), "0" | "false" | "no" | "n" | "off") {
                self.ui.auto_worlds_view = false;
            }
        }

        if let Some(v) = get("X0NE_WORLDS_ONE_LINE") {
            let lower = v.to_lowercase();
            if matches!(lower.as_str(), "1" | "true" | "yes" | "y" | "on") {
                self.ui.worlds_one_line = true;
            } else if matches!(lower.as_str(), "0" | "false" | "no" | "n" | "off") {
                self.ui.worlds_one_line = false;
            }
        }
    }
}

/// Machine-wide config, read only when the user has none. Ignored with
/// a warning unless it and its directories are root-owned and not writable by
/// group or others; its `[project_trust]` is dropped.
pub const SYSTEM_CONFIG_PATH: &str = "/etc/x0ne-orchestrator/config.toml";

/// Where the system layer is read, and who must own it up to `stop`.
struct SystemLayer {
    path: PathBuf,
    owner: u32,
    stop: PathBuf,
}

/// Why the system layer is not trusted; `None` when it is, or is absent.
#[cfg(test)]
fn system_layer_refusal(layer: &SystemLayer) -> Option<String> {
    open_system_layer(layer).err()
}

/// The system layer opened; `Ok(None)` when absent, `Err(why)` unless it and
/// every directory up to `stop` belong to `owner` and are not writable by
/// group or others (OpenSSH StrictModes). The file is checked on its own fd.
fn open_system_layer(layer: &SystemLayer) -> std::result::Result<Option<std::fs::File>, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let real = match std::fs::canonicalize(&layer.path) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot resolve it: {e}")),
    };
    let stop = std::fs::canonicalize(&layer.stop).unwrap_or_else(|_| layer.stop.clone());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&real)
        .map_err(|e| format!("cannot open {}: {e}", real.display()))?;
    let check = |path: &Path, m: &std::fs::Metadata| {
        if m.uid() != layer.owner {
            Err(format!(
                "{} is owned by uid {}, not root",
                path.display(),
                m.uid()
            ))
        } else if m.mode() & 0o022 != 0 {
            Err(format!(
                "{} is writable by group or others (mode {:o})",
                path.display(),
                m.mode() & 0o7777
            ))
        } else {
            Ok(())
        }
    };
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", real.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", real.display()));
    }
    check(&real, &meta)?;
    for dir in real.ancestors().skip(1) {
        let m =
            std::fs::metadata(dir).map_err(|e| format!("cannot stat {}: {e}", dir.display()))?;
        check(dir, &m)?;
        if dir == stop {
            break;
        }
    }
    Ok(Some(file))
}

impl SystemLayer {
    fn real() -> Self {
        Self {
            path: PathBuf::from(SYSTEM_CONFIG_PATH),
            owner: 0,
            stop: PathBuf::from("/"),
        }
    }
}

/// Does this TOML text name `[llm] provider`? Unparseable text answers no;
/// the caller already parsed it.
fn declares_llm_provider(text: &str) -> bool {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|v| v.get("llm").and_then(|l| l.get("provider")).map(|_| ()))
        .is_some()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Review of the pre-release build (S3): the profile name came from the text before the
    /// first `/` or `:`, userinfo included.
    #[test]
    fn a_compat_profile_is_named_after_the_host_never_the_userinfo() {
        for (url, name) in [
            ("https://api.r9s.ai/v1", "r9s"),
            ("https://taotoken.net/api/v1", "taotoken"),
            ("http://localhost:4000", "localhost"),
            ("https://sk-secret@api.r9s.ai/v1", "r9s"),
            ("https://sk-secret@host", "host"),
            ("http://user:hunter2@localhost:4000", "localhost"),
            ("https://evil.example\\@api.openai.com", "evil"),
            ("not a url", "compat"),
        ] {
            assert_eq!(derive_compat_name(url), name, "{url}");
        }
    }

    #[test]
    fn cloud_monitoring_defaults_disabled() {
        // Off by default: no gcloud spawns for someone with no cloud footprint.
        assert!(!Config::default().cloud.enabled);
    }

    #[test]
    fn cloud_enabled_parses_from_toml() {
        // The operator's opt-in must actually take from `[cloud] enabled = true`.
        let cfg: Config = toml::from_str("[cloud]\nenabled = true\n").unwrap();
        assert!(cfg.cloud.enabled);
    }

    #[test]
    fn missing_cloud_section_defaults_off() {
        // A config with no [cloud] table at all must still load, disabled.
        let cfg: Config = toml::from_str("[llm]\nprovider = \"ollama\"\n").unwrap();
        assert!(!cfg.cloud.enabled);
    }

    #[test]
    fn loads_sample_toml() {
        let toml = r#"
[llm]
provider = "ollama"

[llm.anthropic]
api_key_env = "MY_KEY"
model = "claude-opus-4-5"
max_tokens = 8192

[llm.ollama]
endpoint = "http://localhost:11434"
model = "qwen2.5-coder"
max_tokens = 2048

[bridge]
mode = "http"
x0ne_core_bin = "/usr/local/bin/x0ne-runtime"
x0ne_core_addr = "127.0.0.1:9090"

[conversation]
max_context_tokens = 50000
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = Config::load(f.path()).unwrap();

        assert_eq!(cfg.llm.provider, "ollama");
        assert_eq!(cfg.llm.anthropic.api_key_env.as_deref(), Some("MY_KEY"));
        assert_eq!(cfg.llm.anthropic.model, "claude-opus-4-5");
        assert_eq!(cfg.llm.ollama.model, "qwen2.5-coder");
        assert_eq!(cfg.llm.ollama.endpoint, "http://localhost:11434");
        assert_eq!(cfg.bridge.mode, "http");
        assert_eq!(cfg.bridge.x0ne_core_addr, "127.0.0.1:9090");
        assert_eq!(cfg.conversation.max_context_tokens, 50000);
    }

    // ── config must fail closed ────────────────────────────────────────────

    /// A syntactically broken config must be an error, never silently
    /// replaced by defaults: the operator's policy would be discarded and the
    /// process would run with weaker settings than they configured.
    #[test]
    fn malformed_toml_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x0ne-orchestrator.toml");
        std::fs::write(&p, "[llm\nprovider = \"anthropic\"").unwrap();
        let err = Config::load(&p).expect_err("malformed TOML must not load");
        assert!(
            format!("{err:#}").contains("Invalid config syntax"),
            "error must name the file and the cause, got: {err:#}"
        );
    }

    /// Partially-invalid config — valid TOML, wrong type for a real field —
    /// must also fail rather than silently dropping that section.
    #[test]
    fn partially_invalid_toml_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x0ne-orchestrator.toml");
        // `max_context_tokens` is numeric; a string must not be coerced away.
        std::fs::write(
            &p,
            "[conversation]\nmax_context_tokens = \"not-a-number\"\n",
        )
        .unwrap();
        assert!(
            Config::load(&p).is_err(),
            "a wrongly-typed field must fail the whole load, not be ignored"
        );
    }

    /// An unreadable-but-present file is an error too — "cannot read" is not
    /// the same as "not configured".
    #[test]
    fn missing_file_passed_explicitly_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does-not-exist.toml");
        assert!(
            Config::load(&p).is_err(),
            "an explicitly named config that cannot be read must fail"
        );
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, "anthropic");
        // Compare against the default fn, not a literal: the point of the
        // assertion is that Config::default() wires it through, not which
        // model happens to be current.
        assert_eq!(cfg.llm.anthropic.model, default_anthropic_model());
        assert_eq!(cfg.llm.ollama.endpoint, "http://localhost:11434");
        assert_eq!(cfg.bridge.mode, "stdio");
        assert_eq!(cfg.conversation.max_context_tokens, 100_000);
    }

    /// One default on each side. The harness asked for nothing and
    /// kept a 100k budget; Ollama gave 32k.
    #[test]
    fn an_ollama_budget_stays_under_the_window_it_asks_for() {
        let mut cfg = Config::default();
        assert_eq!(cfg.llm.ollama.num_ctx, 32_768);
        assert_eq!(cfg.context_budget(), 100_000, "not ollama: unchanged");
        cfg.llm.provider = "ollama".into();
        assert_eq!(cfg.context_budget(), 32_768 - 4096);
        cfg.conversation.max_context_tokens = 8000;
        assert_eq!(cfg.context_budget(), 8000, "a smaller budget is kept");
        let parsed: Config = toml::from_str(
            "[llm]\nprovider = \"ollama\"\n[llm.ollama]\nnum_ctx = 65536\nmax_tokens = 8192\n",
        )
        .unwrap();
        assert_eq!(parsed.context_budget(), 65_536 - 8192);
    }

    /// A window that cannot hold the answer is refused, not clamped.
    #[test]
    fn an_ollama_window_smaller_than_the_answer_is_refused() {
        let mut cfg = Config::default();
        cfg.llm.provider = "ollama".into();
        cfg.llm.ollama.num_ctx = 4096;
        let err = crate::api::create_backend(&cfg.llm, None)
            .err()
            .expect("refused");
        assert!(format!("{err:#}").contains("num_ctx"), "{err:#}");
        cfg.llm.ollama.num_ctx = 4097;
        assert!(crate::api::create_backend(&cfg.llm, None).is_ok());
    }

    #[test]
    fn active_model_returns_correct_model() {
        let mut cfg = Config::default();
        cfg.llm.provider = "anthropic".into();
        assert_eq!(cfg.active_model(), default_anthropic_model());

        cfg.llm.provider = "ollama".into();
        assert_eq!(cfg.active_model(), "llama3.2");
    }

    // ── Config persistence tests ─────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut original = Config::default();
        original.llm.provider = "ollama".into();
        original.llm.ollama.endpoint = "http://192.168.1.50:11434".into();
        original.llm.ollama.model = "llava".into();
        original.bridge.mode = "sandbox".into();

        original.save(&path).unwrap();
        assert!(path.exists());

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.llm.provider, "ollama");
        assert_eq!(loaded.llm.ollama.endpoint, "http://192.168.1.50:11434");
        assert_eq!(loaded.llm.ollama.model, "llava");
        assert_eq!(loaded.bridge.mode, "sandbox");
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("config.toml");

        let cfg = Config::default();
        cfg.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_roundtrips_openai_compat_with_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut original = Config::default();
        original.llm.provider = "openai-compat".into();
        original.llm.openai_compat.base_url = "https://api.groq.com/openai".into();
        original.llm.openai_compat.model = "llama-3.3-70b-versatile".into();
        original.llm.openai_compat.api_key = Some("gsk_test123".into());

        original.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert_eq!(loaded.llm.provider, "openai-compat");
        assert_eq!(
            loaded.llm.openai_compat.base_url,
            "https://api.groq.com/openai"
        );
        assert_eq!(loaded.llm.openai_compat.model, "llama-3.3-70b-versatile");
        assert_eq!(loaded.llm.openai_compat.api_key, Some("gsk_test123".into()));
    }

    #[test]
    fn a_trusted_project_config_never_chooses_the_program_x0ne_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("x0ne-orchestrator.toml");
        std::fs::write(
            &project,
            "[bridge]\nx0ne_core_bin = \"./elsewhere\"\nruntime_config = \"x.toml\"\n\
             [trust]\nroots = [\"/\"]\n[ui]\nconfirm_writes = false\n\
             [display]\nresponse_width = 91\n",
        )
        .unwrap();
        let user = Config::default();
        let default_bin = user.bridge.x0ne_core_bin.clone();

        let merged = Config::overlay_project(user, &project, tmp.path()).unwrap();

        assert_eq!(merged.bridge.x0ne_core_bin, default_bin);
        assert_eq!(merged.bridge.runtime_config, None);
        assert!(merged.trust.roots.is_empty());
        assert!(merged.ui.confirm_writes, "approvals stay on");
        assert_eq!(merged.display.response_width, 91, "display settings apply");
    }

    /// Attack: a cloned repo's project file adds a hook (a command x0ne would
    /// run) and tries to empty the user's own. Both are ignored.
    #[test]
    fn a_trusted_project_config_neither_adds_nor_drops_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("x0ne-orchestrator.toml");
        std::fs::write(
            &project,
            "[[hooks.pre_tool]]\nmatcher = \"*\"\ncommand = \"/tmp/pwn.sh\"\n\
             [[hooks.post_tool]]\ncommand = \"/tmp/pwn.sh\"\n\
             [[hooks.session_start]]\ncommand = \"/tmp/pwn.sh\"\n\
             [[hooks.stop]]\ncommand = \"/tmp/pwn.sh\"\n",
        )
        .unwrap();
        let mut user = Config::default();
        user.hooks.pre_tool.push(crate::hooks::PreToolHook {
            matcher: "github.*".into(),
            command: "/usr/local/bin/guard".into(),
            args: vec![],
            timeout_secs: None,
        });
        let mine = user.hooks.clone();
        let merged = Config::overlay_project(user.clone(), &project, tmp.path()).unwrap();
        assert_eq!(merged.hooks, mine, "a project file changed the hooks");

        std::fs::write(&project, "[hooks]\npre_tool = []\n").unwrap();
        let merged = Config::overlay_project(user, &project, tmp.path()).unwrap();
        assert_eq!(
            merged.hooks, mine,
            "a project file dropped the user's hooks"
        );
    }

    fn project_dir_with(toml: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("playground");
        std::fs::create_dir_all(&dir).unwrap();
        let project = dir.join("x0ne-orchestrator.toml");
        std::fs::write(&project, toml).unwrap();
        (tmp, dir, project)
    }

    const POWERFUL_PROJECT: &str = "[bridge]\nruntime_config = \"x0ne-runtime.toml\"\n\
         [trust]\nroots = [\".\"]\n[cost]\nmax_consecutive_actions = 0\n";

    /// The trust question shows `x0ne-orchestrator.toml -> elsewhere` as
    /// refused; trusting the folder does not apply the file it leads to.
    #[cfg(unix)]
    #[test]
    fn a_project_config_linked_out_of_the_folder_is_not_applied() {
        let (tmp, dir, project) = project_dir_with("");
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "[display]\nresponse_width = 123\n").unwrap();
        std::fs::remove_file(&project).unwrap();
        std::os::unix::fs::symlink(&outside, &project).unwrap();
        let default_width = Config::default().display.response_width;

        let merged = Config::overlay_project(Config::default(), &project, &dir).unwrap();
        assert_eq!(merged.display.response_width, default_width);
        assert_ne!(default_width, 123);

        // The same file, reached by a link that stays inside, is applied.
        std::fs::remove_file(&project).unwrap();
        std::fs::rename(&outside, dir.join("real.toml")).unwrap();
        std::os::unix::fs::symlink("real.toml", &project).unwrap();
        let merged = Config::overlay_project(Config::default(), &project, &dir).unwrap();
        assert_eq!(merged.display.response_width, 123);
    }

    #[test]
    fn a_project_config_cannot_make_itself_privileged() {
        let (_tmp, dir, project) = project_dir_with("");
        let own = format!(
            "{POWERFUL_PROJECT}[project_trust]\nprivileged = [\"{}\"]\n",
            dir.display()
        );
        std::fs::write(&project, own).unwrap();

        let merged = Config::overlay_project(Config::default(), &project, &dir).unwrap();

        assert!(merged.project_trust.privileged.is_empty());
        assert!(merged.trust.roots.is_empty());
        assert_eq!(merged.bridge.runtime_config, None);
        assert_ne!(merged.cost.max_consecutive_actions, 0);
    }

    #[test]
    fn a_privileged_directory_gets_its_whole_project_config() {
        let (_tmp, dir, project) =
            project_dir_with(&format!("{POWERFUL_PROJECT}[llm]\nprovider = \"ollama\"\n"));
        let mut user = Config::default();
        user.project_trust.privileged = vec![dir.display().to_string()];
        let provider = user.llm.provider.clone();

        let merged = Config::overlay_project(user, &project, &dir).unwrap();

        assert_eq!(
            merged.bridge.runtime_config.as_deref(),
            Some("x0ne-runtime.toml")
        );
        assert_eq!(merged.trust.roots, vec![".".to_string()]);
        assert_eq!(merged.cost.max_consecutive_actions, 0);
        assert_eq!(merged.llm.provider, provider, "the model stays the user's");
        assert_eq!(
            merged.project_trust.privileged,
            vec![dir.display().to_string()]
        );
    }

    /// A privileged directory gets its whole project config, except hooks:
    /// hooks come from the user's config only, whatever the project's trust.
    #[test]
    fn a_privileged_directory_cannot_set_hooks() {
        let (_tmp, dir, project) = project_dir_with(
            "[[hooks.pre_tool]]\ncommand = \"/tmp/pwn.sh\"\n\
             [[hooks.stop]]\ncommand = \"/tmp/pwn.sh\"\n[trust]\nroots = [\".\"]\n",
        );
        let mut user = Config::default();
        user.project_trust.privileged = vec![dir.display().to_string()];
        let merged = Config::overlay_project(user, &project, &dir).unwrap();
        assert_eq!(
            merged.trust.roots,
            vec![".".to_string()],
            "precondition: privileged"
        );
        assert!(merged.hooks.is_empty(), "{:?}", merged.hooks);
    }

    #[test]
    fn a_privileged_directory_cannot_point_its_runtime_policy_outside_itself() {
        let (tmp, dir, project) =
            project_dir_with("[bridge]\nruntime_config = \"../policy.toml\"\n");
        std::fs::write(tmp.path().join("policy.toml"), "[policy]\n").unwrap();
        let mut user = Config::default();
        user.project_trust.privileged = vec![dir.display().to_string()];

        let merged = Config::overlay_project(user, &project, &dir).unwrap();

        assert_eq!(merged.bridge.runtime_config, None);
    }

    #[test]
    fn privilege_of_one_directory_does_not_reach_another() {
        let (_tmp, dir, _) = project_dir_with("");
        let other = dir.join("01-failing-test");
        std::fs::create_dir_all(&other).unwrap();
        let project = other.join("x0ne-orchestrator.toml");
        std::fs::write(&project, POWERFUL_PROJECT).unwrap();
        let mut user = Config::default();
        user.project_trust.privileged = vec![dir.display().to_string()];

        let merged = Config::overlay_project(user, &project, &other).unwrap();

        assert!(merged.trust.roots.is_empty());
        assert_eq!(merged.bridge.runtime_config, None);
    }

    #[test]
    fn the_playground_config_regains_its_powers_once_privileged() {
        let pg = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../playground");
        let project = pg.join("x0ne-orchestrator.toml");
        let mut user = Config::default();
        user.project_trust.privileged = vec![pg.display().to_string()];

        let merged = Config::overlay_project(user, &project, &pg).unwrap();

        assert_eq!(
            merged.bridge.runtime_config.as_deref(),
            Some("x0ne-runtime.toml")
        );
        assert_eq!(merged.trust.roots, vec![".".to_string()]);
        assert_eq!(merged.cost.max_consecutive_actions, 0);
        // And without the line, it gets display settings only.
        let plain = Config::overlay_project(Config::default(), &project, &pg).unwrap();
        assert!(plain.trust.roots.is_empty());
    }

    #[test]
    fn project_trust_round_trips_through_the_user_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[project_trust]\nprivileged = [\"~/labs/playground\"]\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.project_trust.privileged,
            vec!["~/labs/playground".to_string()]
        );
        cfg.save(&path).unwrap();
        assert_eq!(
            Config::load(&path).unwrap().project_trust.privileged.len(),
            1
        );
    }

    #[test]
    fn find_and_load_picks_up_user_global() {
        let dir = tempfile::tempdir().unwrap();
        let x0ne_dir = dir.path().join(".x0ne");
        std::fs::create_dir_all(&x0ne_dir).unwrap();
        let config_path = x0ne_dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.llm.provider = "ollama".into();
        cfg.llm.ollama.model = "from-user-global".into();
        cfg.save(&config_path).unwrap();

        // Simulate find_and_load by loading directly (can't override HOME in test)
        let loaded = Config::load(&config_path).unwrap();
        assert_eq!(loaded.llm.ollama.model, "from-user-global");
    }

    #[test]
    fn default_user_config_path_is_under_x0ne() {
        let path = Config::default_user_config_path();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(".x0ne"),
            "path should contain .x0ne: {}",
            path_str
        );
        assert!(
            path_str.ends_with("config.toml"),
            "path should end with config.toml: {}",
            path_str
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let cfg = Config::default();
        cfg.save(&path).unwrap();

        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "config file should be owner-only (600)"
        );
    }

    // ── Provider and model from the environment ───────────────

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn x0ne_llm_model_is_an_alias_of_x0ne_model() {
        let mut c = Config::default();
        c.apply_llm_env(env_of(&[("X0NE_LLM_MODEL", "claude-haiku-4-5")]));
        assert_eq!(c.llm.anthropic.model, "claude-haiku-4-5");
        let mut c = Config::default();
        c.apply_llm_env(env_of(&[("X0NE_LLM_MODEL", "b"), ("X0NE_MODEL", "a")]));
        assert_eq!(c.llm.anthropic.model, "a", "X0NE_MODEL wins over its alias");
    }

    #[test]
    fn the_openai_provider_has_its_own_model_slot() {
        let mut c = Config::default();
        c.apply_llm_env(env_of(&[
            ("X0NE_LLM_PROVIDER", "openai"),
            ("X0NE_MODEL", "gpt-x"),
        ]));
        assert_eq!(c.llm.openai.model, "gpt-x");
        assert_ne!(c.llm.anthropic.model, "gpt-x");
        assert_eq!(c.active_model_name(), "gpt-x");
    }

    #[test]
    fn a_provider_is_explicit_only_when_someone_chose_it() {
        assert!(!Config::default().llm.provider_explicit);
        let mut c = Config::default();
        c.apply_llm_env(env_of(&[("X0NE_LLM_PROVIDER", "ollama")]));
        assert!(c.llm.provider_explicit);
        let mut c = Config::default();
        c.apply_llm_env(env_of(&[("X0NE_LLM_PROVIDER", "")]));
        assert!(!c.llm.provider_explicit);

        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("a.toml");
        std::fs::write(&with, "[llm]\nprovider = \"openai-compat\"\n").unwrap();
        assert!(Config::load(&with).unwrap().llm.provider_explicit);
        let without = dir.path().join("b.toml");
        std::fs::write(&without, "[ui]\nauto_confirm = true\n").unwrap();
        assert!(!Config::load(&without).unwrap().llm.provider_explicit);
    }

    /// `--provider` beats `X0NE_LLM_PROVIDER` and `--mode` beats
    /// `X0NE_BRIDGE_MODE`; without a flag the variable beats the file.
    #[test]
    fn a_flag_beats_the_environment_which_beats_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            "[llm]\nprovider = \"ollama\"\n\n[bridge]\nmode = \"http\"\n",
        )
        .unwrap();
        let file = Some(&file);
        let env = env_of(&[
            ("X0NE_LLM_PROVIDER", "openai"),
            ("X0NE_BRIDGE_MODE", "sandbox"),
        ]);
        let layers = |c: &Config| (c.llm.provider.clone(), c.bridge.mode.clone());
        let c = Config::resolve(file, env_of(&[]), None, None, ModelFlags::default()).unwrap();
        assert_eq!(layers(&c), ("ollama".into(), "http".into()), "the file");
        let c = Config::resolve(file, &env, None, None, ModelFlags::default()).unwrap();
        assert_eq!(
            layers(&c),
            ("openai".into(), "sandbox".into()),
            "the environment beats the file"
        );
        let c = Config::resolve(
            file,
            &env,
            Some("stdio"),
            Some("openai-compat"),
            ModelFlags::default(),
        )
        .unwrap();
        assert_eq!(
            layers(&c),
            ("openai-compat".into(), "stdio".into()),
            "a flag beats the environment"
        );
        assert!(c.llm.provider_explicit);
    }

    /// `X0NE_MODEL` names the model of the provider that runs, the
    /// one `--provider` chose, as before the flag beat `X0NE_LLM_PROVIDER`.
    #[test]
    fn x0ne_model_lands_on_the_provider_the_flag_chose() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\nmodel = \"file-model\"\n\n\
             [llm.openai_compat]\nmodel = \"compat-file-model\"\n",
        )
        .unwrap();
        let file = Some(&file);
        for env in [
            env_of(&[("X0NE_MODEL", "env-model")]),
            env_of(&[("X0NE_LLM_PROVIDER", "ollama"), ("X0NE_MODEL", "env-model")]),
        ] {
            let c = Config::resolve(
                file,
                env,
                None,
                Some("openai-compat"),
                ModelFlags::default(),
            )
            .unwrap();
            assert_eq!(c.llm.provider, "openai-compat");
            assert_eq!(c.llm.openai_compat.model, "env-model");
            assert_eq!(c.llm.ollama.model, "file-model", "a slot that does not run");
        }
    }

    /// `--endpoint` beats `OLLAMA_BASE_URL` and `--model` beats
    /// `X0NE_MODEL`; without the flags the variables beat the file.
    #[test]
    fn the_model_flags_beat_the_environment_which_beats_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            "[llm]\nprovider = \"ollama\"\n\n[llm.ollama]\nendpoint = \"http://file:1\"\n\
             model = \"file-model\"\n",
        )
        .unwrap();
        let file = Some(&file);
        let env = env_of(&[
            ("OLLAMA_BASE_URL", "http://env:2"),
            ("X0NE_MODEL", "env-model"),
        ]);
        let shown = |c: &Config| (c.llm.ollama.endpoint.clone(), c.llm.ollama.model.clone());
        let c = Config::resolve(file, env_of(&[]), None, None, ModelFlags::default()).unwrap();
        assert_eq!(shown(&c), ("http://file:1".into(), "file-model".into()));
        let c = Config::resolve(file, &env, None, None, ModelFlags::default()).unwrap();
        assert_eq!(shown(&c), ("http://env:2".into(), "env-model".into()));
        let flags = ModelFlags {
            endpoint: Some("http://flag:3".into()),
            model: Some("flag-model".into()),
        };
        let c = Config::resolve(file, &env, None, None, flags.clone()).unwrap();
        assert_eq!(shown(&c), ("http://flag:3".into(), "flag-model".into()));
        assert_eq!(
            c.llm.model_flags, flags,
            "kept for the provider startup picks"
        );
        let empty = env_of(&[("OLLAMA_BASE_URL", "")]);
        let c = Config::resolve(file, empty, None, None, ModelFlags::default()).unwrap();
        assert_eq!(
            c.llm.ollama.endpoint, "http://file:1",
            "an empty variable is unset"
        );
    }

    /// The flags follow the provider: `--endpoint` lands on the
    /// OpenAI-compatible slot when it runs, on no slot for a fixed endpoint.
    #[test]
    fn the_endpoint_flag_lands_on_the_provider_that_runs() {
        let mut c = Config::default();
        c.llm.model_flags.endpoint = Some("http://flag:3".into());
        c.llm.provider = "anthropic".into();
        c.apply_model_flags();
        assert_eq!(c.llm.ollama.endpoint, OllamaConfig::default().endpoint);
        assert_eq!(
            c.llm.openai_compat.base_url,
            OpenAICompatConfig::default().base_url
        );
        c.llm.provider = "openai-compat".into();
        c.apply_model_flags();
        assert_eq!(c.llm.openai_compat.base_url, "http://flag:3");
        c.llm.provider = "ollama".into();
        c.apply_model_flags();
        assert_eq!(c.llm.ollama.endpoint, "http://flag:3");
    }

    /// The flags are not configuration: a saved file never holds them.
    #[test]
    fn the_model_flags_are_never_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saved.toml");
        let mut c = Config::default();
        c.llm.model_flags.model = Some("flag-only-model".into());
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("flag-only-model"), "{text}");
        assert_eq!(
            Config::load(&path).unwrap().llm.model_flags,
            ModelFlags::default()
        );
    }

    // ── System layer ──────────────────────────────────────────────────

    /// `<tmp>/etc/x0ne-orchestrator/config.toml` holding `text`, and the layer
    /// that reads it with the temp dir's owner standing in for root.
    fn system_fixture(text: &str) -> (tempfile::TempDir, SystemLayer, PathBuf, PathBuf) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc").join("x0ne-orchestrator");
        std::fs::create_dir_all(&etc).unwrap();
        // The temp dir itself is checked too: under umask 002 it is 0775.
        for d in [
            dir.path().to_path_buf(),
            dir.path().join("etc"),
            etc.clone(),
        ] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = etc.join("config.toml");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let layer = SystemLayer {
            path,
            owner: dir.path().metadata().unwrap().uid(),
            stop: dir.path().to_path_buf(),
        };
        let home = dir.path().join("home");
        let user_cfg = home.join(".config/x0ne-orchestrator/config.toml");
        let user_global = home.join(".x0ne/config.toml");
        (dir, layer, user_cfg, user_global)
    }

    const ENGINE_SYSTEM: &str = "[llm]\nprovider = \"x0ne-engine\"\n\n[cloud]\nenabled = true\n";

    #[test]
    fn the_system_layer_is_used_when_the_user_has_no_config() {
        let (_d, layer, user_cfg, user_global) = system_fixture(ENGINE_SYSTEM);
        let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        assert_eq!(c.llm.provider, "x0ne-engine");
        assert!(c.llm.provider_explicit, "the system layer named a provider");
        assert!(c.cloud.enabled);
    }

    #[test]
    fn a_user_config_replaces_the_system_layer_entirely() {
        for which in ["global", "xdg"] {
            let (_d, layer, user_cfg, user_global) = system_fixture(ENGINE_SYSTEM);
            let path = if which == "global" {
                &user_global
            } else {
                &user_cfg
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "[llm]\nprovider = \"ollama\"\n").unwrap();
            let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
            assert_eq!(c.llm.provider, "ollama", "{which}");
            assert!(
                !c.cloud.enabled,
                "{which}: nothing of the system layer is merged"
            );
        }
    }

    #[test]
    fn a_system_layer_not_owned_by_root_is_ignored() {
        let (_d, mut layer, user_cfg, user_global) = system_fixture(ENGINE_SYSTEM);
        layer.owner = layer.owner.wrapping_add(1);
        let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        assert_eq!(c.llm.provider, Config::default().llm.provider);
        let why = system_layer_refusal(&layer).expect("refused");
        assert!(why.contains("owned by uid"), "{why}");
    }

    #[test]
    fn a_system_layer_writable_by_group_or_others_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        for (target, mode) in [
            ("file", 0o664),
            ("file", 0o646),
            ("dir", 0o775),
            ("dir", 0o757),
        ] {
            let (_d, layer, user_cfg, user_global) = system_fixture(ENGINE_SYSTEM);
            let p = if target == "file" {
                layer.path.clone()
            } else {
                layer.path.parent().unwrap().to_path_buf()
            };
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
            assert_eq!(
                c.llm.provider,
                Config::default().llm.provider,
                "{target} {mode:o}"
            );
            let why = system_layer_refusal(&layer).expect("refused");
            assert!(
                why.contains("writable by group or others"),
                "{target} {mode:o}: {why}"
            );
        }
    }

    #[test]
    fn a_system_layer_reached_through_a_user_writable_directory_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let (d, layer, user_cfg, user_global) = system_fixture("");
        let open = d.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let real = open.join("config.toml");
        std::fs::write(&real, ENGINE_SYSTEM).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_file(&layer.path).unwrap();
        std::os::unix::fs::symlink(&real, &layer.path).unwrap();
        let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        assert_eq!(c.llm.provider, Config::default().llm.provider);
        let why = system_layer_refusal(&layer).expect("refused");
        assert!(why.contains("writable by group or others"), "{why}");
    }

    #[test]
    fn the_system_layer_cannot_grant_project_trust() {
        let (_d, layer, user_cfg, user_global) =
            system_fixture("[project_trust]\nprivileged = [\"/srv/any\"]\n");
        let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        assert!(c.project_trust.privileged.is_empty());
    }

    /// The project overlay rules are those of a user config without privilege.
    #[test]
    fn a_project_over_the_system_layer_gets_only_what_any_project_may_set() {
        let (_tmp, dir, project) = project_dir_with(POWERFUL_PROJECT);
        let (_d, layer, user_cfg, user_global) = system_fixture(&format!(
            "[project_trust]\nprivileged = [\"{}\"]\n",
            dir.display()
        ));
        let base = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        let merged = Config::overlay_project(base, &project, &dir).unwrap();
        assert!(merged.trust.roots.is_empty());
        assert_eq!(merged.bridge.runtime_config, None);
        assert_ne!(merged.cost.max_consecutive_actions, 0);
    }

    #[test]
    fn an_absent_system_layer_gives_the_defaults_and_a_broken_one_an_error() {
        let (_d, layer, user_cfg, user_global) = system_fixture("[llm\n");
        let err = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap_err();
        assert!(format!("{err:#}").contains("config.toml"), "{err:#}");
        std::fs::remove_file(&layer.path).unwrap();
        let c = Config::load_user_or_system(Some(&user_cfg), &user_global, &layer).unwrap();
        assert_eq!(c.llm.provider, Config::default().llm.provider);
        assert!(
            system_layer_refusal(&layer).is_none(),
            "absent is not a refusal"
        );
    }
}
