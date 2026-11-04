//! `x0ne hook pretooluse`: the native tools of a coding agent (Bash,
//! Edit, Write… of Claude Code; shell and apply_patch of Codex) go through
//! the x0ne-runtime policy before they run.
//!
//! Protocol: one JSON object on stdin, a `hookSpecificOutput` decision on
//! stdout, exit 0 (<https://code.claude.com/docs/en/hooks>, PreToolUse
//! decision control). Any failure — no policy, unreadable input, a panic —
//! exits 2, which Claude Code and Codex both treat as a block. Every decision
//! is written to the system journal (see `journal`).

pub mod journal;
pub mod shell;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};
use x0ne_runtime::action::Action;
use x0ne_runtime::config::AppConfig;
use x0ne_runtime::policy::{PolicyEffect, PolicyEngine};

/// Largest hook payload read. A Write carries the whole file.
const MAX_INPUT: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    pub fn parse(s: &str) -> Self {
        if s == "codex" {
            Agent::Codex
        } else {
            Agent::ClaudeCode
        }
    }
    fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }
}

pub struct HookArgs {
    pub policy: Option<PathBuf>,
    pub journal_socket: Option<PathBuf>,
    pub agent: Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Allowed,
    Asked,
    Refused,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::Allowed => "allowed",
            Outcome::Asked => "asked",
            Outcome::Refused => "refused",
        }
    }
}

#[derive(Debug, Clone)]
struct Verdict {
    outcome: Outcome,
    /// Stable name: a `DenyReason` variant, or a rule of this hook.
    rule: String,
    reason: String,
    /// Policy action the deciding gesture was judged as.
    action: String,
    /// Command line or path the decision is about.
    target: String,
    /// Hook failure (no policy, bad input): exit 2 on top of the JSON.
    failure: bool,
}

impl Verdict {
    fn refused(rule: &str, reason: impl Into<String>, action: &str, target: &str) -> Self {
        Self {
            outcome: Outcome::Refused,
            rule: rule.into(),
            reason: reason.into(),
            action: action.into(),
            target: target.into(),
            failure: false,
        }
    }
    fn failure(rule: &str, reason: impl Into<String>) -> Self {
        Self {
            failure: true,
            ..Self::refused(rule, reason, "-", "-")
        }
    }
}

/// What was read from the payload, for the journal.
#[derive(Default)]
struct Context {
    tool: String,
    session: String,
    tool_use_id: String,
    cwd: String,
    subagent: String,
    policy: String,
}

/// Run the hook and exit. Never returns.
pub fn run_pretooluse(args: HookArgs) -> ! {
    // A panic would exit 101: a NON-blocking error, the tool would run.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("x0ne hook: internal error, refusing the tool call: {info}");
        std::process::exit(2);
    }));

    let mut ctx = Context::default();
    let mut input = String::new();
    let verdict = match std::io::stdin().take(MAX_INPUT).read_to_string(&mut input) {
        Err(e) => Verdict::failure("BadInput", format!("cannot read the hook payload: {e}")),
        Ok(_) => decide(&input, &args, &mut ctx),
    };

    record(&verdict, &ctx, &args);
    respond(&verdict, args.agent)
}

fn decide(input: &str, args: &HookArgs, ctx: &mut Context) -> Verdict {
    let payload: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => return Verdict::failure("BadInput", format!("hook payload is not JSON: {e}")),
    };
    let s = |k: &str| {
        payload
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    ctx.tool = s("tool_name");
    ctx.session = s("session_id");
    ctx.tool_use_id = s("tool_use_id");
    ctx.subagent = s("agent_type");
    ctx.cwd = s("cwd");

    let event = s("hook_event_name");
    if !event.is_empty() && event != "PreToolUse" {
        return Verdict::failure(
            "BadInput",
            format!("`x0ne hook pretooluse` received a {event} event: wire it under PreToolUse"),
        );
    }
    if ctx.tool.is_empty() {
        return Verdict::failure("BadInput", "hook payload has no tool_name");
    }
    let Some(tool_input) = payload.get("tool_input").filter(|v| v.is_object()) else {
        return Verdict::failure("BadInput", "hook payload has no tool_input object");
    };
    let cwd = if ctx.cwd.is_empty() {
        match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => {
                return Verdict::failure(
                    "BadInput",
                    format!("no cwd in the payload and none here: {e}"),
                )
            }
        }
    } else {
        PathBuf::from(&ctx.cwd)
    };
    if !cwd.is_absolute() {
        return Verdict::failure("BadInput", "cwd in the payload is not absolute");
    }

    // No policy, no gesture.
    let (cfg, source) = match AppConfig::find_and_load_with_source(args.policy.as_ref()) {
        Ok(v) => v,
        Err(e) => {
            return Verdict::failure(
                "NoPolicy",
                format!("refusing: no usable x0ne policy. {e:#}"),
            )
        }
    };
    ctx.policy = source
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".into());

    let judge = Judge::new(cfg, source.path());
    let gestures = match gestures_of(&ctx.tool, tool_input, &cwd, args.agent) {
        Ok(g) => g,
        Err(v) => return v,
    };
    judge.all(&gestures)
}

/// A gesture expressed as what the policy judges.
enum Gesture {
    Action {
        action: Value,
        target: String,
    },
    PathArg {
        probe: String,
        resolved: Option<PathBuf>,
    },
    Write {
        path: PathBuf,
    },
}

/// The environment the agent's shell inherits: this process's, which is
/// the agent's own.
fn agent_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    if !env.contains_key("HOME") {
        if let Some(h) = dirs::home_dir() {
            env.insert("HOME".into(), h.to_string_lossy().into_owned());
        }
    }
    env
}

/// `pattern` of Glob, `glob` of Grep: an absolute pattern or one with `..`
/// reaches outside `root`; its fixed directory is searched too.
fn pattern_root(root: &Path, pattern: Option<&str>) -> Option<PathBuf> {
    let p = pattern?;
    if !p.starts_with('/') && !p.split('/').any(|c| c == "..") {
        return None;
    }
    let fixed: PathBuf = root
        .join(p)
        .components()
        .take_while(|c| {
            !c.as_os_str()
                .to_string_lossy()
                .contains(['*', '?', '[', '{'])
        })
        .collect();
    Some(lexical(&fixed))
}

fn absolute(cwd: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn gestures_of(
    tool: &str,
    input: &Value,
    cwd: &Path,
    agent: Agent,
) -> Result<Vec<Gesture>, Verdict> {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let need = |k: &str| {
        field(k)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                Verdict::failure("BadInput", format!("{tool}: tool_input.{k} is missing"))
            })
    };
    let file_action = |kind: &str, path: PathBuf| -> Vec<Gesture> {
        let p = path.to_string_lossy().into_owned();
        let action = match kind {
            "WriteFile" => json!({"type": "WriteFile", "path": p, "content": ""}),
            "EditFile" => json!({"type": "EditFile", "path": p, "edits": []}),
            "ReadFile" => json!({"type": "ReadFile", "path": p}),
            _ => json!({"type": "SearchCode", "root": p, "pattern": ""}),
        };
        let mut v = Vec::new();
        if kind == "WriteFile" || kind == "EditFile" {
            v.push(Gesture::Write { path: path.clone() });
        }
        v.push(Gesture::Action { action, target: p });
        v
    };

    // Codex sends apply_patch with the patch in `command` (also matched as Edit/Write).
    if let Some(cmd) = field("command").filter(|c| c.trim_start().starts_with("*** Begin Patch")) {
        let mut out = Vec::new();
        for (kind, path) in patch_paths(cmd) {
            out.extend(file_action(kind, absolute(cwd, &path)));
        }
        if out.is_empty() {
            return Err(Verdict::refused(
                "ShellSyntax",
                "apply_patch names no file",
                "-",
                "-",
            ));
        }
        return Ok(out);
    }

    match tool {
        "Bash" | "shell" | "exec_command" | "local_shell" => {
            let mut sctx = shell::Ctx::new(cwd.to_path_buf(), agent_env());
            let (line, read) = match input.get("command") {
                Some(Value::String(s)) => (s.clone(), shell::analyse(s, &mut sctx)),
                Some(Value::Array(a)) => {
                    let argv: Vec<String> = a
                        .iter()
                        .map(|v| v.as_str().unwrap_or("").to_string())
                        .collect();
                    let line = argv.join(" ");
                    (line, shell::analyse_argv(&argv, &mut sctx))
                }
                _ => {
                    return Err(Verdict::failure(
                        "BadInput",
                        format!("{tool}: tool_input.command is missing"),
                    ))
                }
            };
            let gestures =
                read.map_err(|r| Verdict::refused(r.rule, r.reason, "RunCommand", &line))?;
            // The directory the line starts in is under the roots like its arguments.
            let start = Gesture::PathArg {
                probe: cwd.to_string_lossy().into_owned(),
                resolved: Some(cwd.to_path_buf()),
            };
            Ok(std::iter::once(start)
                .chain(gestures.into_iter().flat_map(|g| match g {
                    shell::Gesture::Run {
                        command,
                        args,
                        env,
                        cwd,
                    } => {
                        let target = std::iter::once(command.clone())
                            .chain(args.iter().cloned())
                            .collect::<Vec<_>>()
                            .join(" ");
                        vec![Gesture::Action {
                            action: json!({
                                "type": "RunCommand",
                                "command": command,
                                "args": args,
                                "cwd": cwd.map(|c| c.to_string_lossy().into_owned()),
                                "env": env,
                            }),
                            target,
                        }]
                    }
                    shell::Gesture::Open { path, write } => {
                        file_action(if write { "WriteFile" } else { "ReadFile" }, path)
                    }
                    shell::Gesture::PathArg { probe, resolved } => {
                        vec![Gesture::PathArg { probe, resolved }]
                    }
                }))
                .collect())
        }
        "Write" => Ok(file_action("WriteFile", absolute(cwd, &need("file_path")?))),
        "Edit" | "MultiEdit" => Ok(file_action("EditFile", absolute(cwd, &need("file_path")?))),
        "NotebookEdit" => Ok(file_action(
            "EditFile",
            absolute(cwd, &need("notebook_path")?),
        )),
        "Read" => Ok(file_action("ReadFile", absolute(cwd, &need("file_path")?))),
        "Grep" | "Glob" => {
            let root = field("path")
                .filter(|p| !p.is_empty())
                .map(|p| absolute(cwd, p))
                .unwrap_or_else(|| cwd.to_path_buf());
            let pattern = if tool == "Glob" {
                field("pattern")
            } else {
                field("glob")
            };
            let mut out = Vec::new();
            if let Some(reach) = pattern_root(&root, pattern) {
                out.extend(file_action("SearchCode", reach));
            }
            out.extend(file_action("SearchCode", root));
            Ok(out)
        }
        _ => {
            let hint = if agent == Agent::Codex {
                ""
            } else {
                " Restrict the hook matcher to Bash|Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Glob."
            };
            Err(Verdict::refused(
                "UnknownTool",
                format!("tool `{tool}` has no mapping to an x0ne policy action, so it is refused.{hint}"),
                "-",
                tool,
            ))
        }
    }
}

/// Files an apply_patch envelope touches: (policy action, path as written).
fn patch_paths(patch: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for line in patch.lines() {
        for (prefix, kind) in [
            ("*** Add File: ", "WriteFile"),
            ("*** Delete File: ", "WriteFile"),
            ("*** Update File: ", "EditFile"),
            ("*** Move to: ", "WriteFile"),
        ] {
            if let Some(p) = line.strip_prefix(prefix) {
                out.push((kind, p.trim().to_string()));
            }
        }
    }
    out
}

/// Remove `.` and fold `..` without touching the disk.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The path as the kernel would open it: links of the existing part resolved.
fn real(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => lexical(p),
    }
}

/// Read by any confined command too (Landlock read set of the runtime).
const SYSTEM_DIRS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/opt",
    "/dev/null",
];

struct Judge {
    engine: PolicyEngine,
    mode: String,
    deny: x0ne_actions::deny_paths::DenyList,
    /// Where a path named in a Bash argument may point: read and write roots,
    /// plus the system directories a confined command may read anyway.
    arg_roots: Vec<PathBuf>,
    protected_files: Vec<PathBuf>,
    protected_dirs: Vec<PathBuf>,
}

impl Judge {
    fn new(cfg: AppConfig, policy_path: Option<&Path>) -> Self {
        let mode = cfg.policy.mode.clone();
        // Lists judged with `auto`; the mode is applied after, so a refusal
        // by the lists is never softened into a question.
        let mut lists = cfg.clone();
        lists.policy.mode = "auto".into();
        let deny = x0ne_actions::deny_paths::DenyList::new(&cfg.policy.deny_path_patterns);
        let engine = PolicyEngine::new(lists, cfg.actions.clone());
        let mut named: Vec<String> = cfg.policy.allowed_read_paths.clone();
        named.extend(cfg.policy.allowed_write_paths.iter().cloned());
        let mut arg_roots = x0ne_runtime::executor::confine::normalize_roots(&named);
        arg_roots.extend(SYSTEM_DIRS.iter().map(PathBuf::from));

        let mut protected_files = Vec::new();
        if let Some(p) = policy_path {
            protected_files.push(real(p));
        }
        if let Ok(exe) = std::env::current_exe() {
            protected_files.push(real(&exe));
        }
        let mut protected_dirs = Vec::new();
        if let Some(c) = dirs::config_dir() {
            protected_dirs.push(real(&c.join("x0ne")));
        }
        protected_dirs.push(PathBuf::from("/etc/claude-code"));
        protected_dirs.push(PathBuf::from("/etc/codex"));
        Self {
            engine,
            mode,
            deny,
            arg_roots,
            protected_files,
            protected_dirs,
        }
    }

    /// Files that switch the hook off or rewrite the policy.
    fn protected(&self, p: &Path) -> Option<String> {
        for c in [lexical(p), real(p)] {
            if self.protected_files.contains(&c)
                || self.protected_dirs.iter().any(|d| c.starts_with(d))
            {
                return Some(c.display().to_string());
            }
            let parts: Vec<String> = c
                .components()
                .map(|x| x.as_os_str().to_string_lossy().into_owned())
                .collect();
            for w in parts.windows(2) {
                let hit = matches!(
                    (w[0].as_str(), w[1].as_str()),
                    (".claude", "settings.json" | "settings.local.json" | "hooks")
                        | (".codex", "config.toml" | "hooks.json" | "hooks")
                );
                if hit {
                    return Some(c.display().to_string());
                }
            }
        }
        None
    }

    fn protected_verdict(&self, path: &str, action: &str, target: &str) -> Verdict {
        Verdict::refused(
            "ProtectedPath",
            format!(
                "{path} configures the agent's hooks or the x0ne policy: the agent may not change it. \
                 Edit it yourself, outside the agent."
            ),
            action,
            target,
        )
    }

    fn deny_pattern(&self, s: &str) -> Option<String> {
        self.deny.first_match(s).map(str::to_string)
    }

    fn one(&self, g: &Gesture) -> Option<Verdict> {
        match g {
            Gesture::Write { path } => {
                let shown = path.display().to_string();
                self.protected(path)
                    .map(|p| self.protected_verdict(&p, "WriteFile", &shown))
            }
            Gesture::PathArg { probe, resolved } => {
                let mut forms = vec![
                    probe.clone(),
                    lexical(Path::new(probe)).display().to_string(),
                ];
                if let Some(r) = resolved {
                    if let Some(p) = self.protected(r) {
                        return Some(self.protected_verdict(&p, "RunCommand", probe));
                    }
                    forms.push(real(r).display().to_string());
                } else if let Some(p) = self.protected(Path::new(probe)) {
                    return Some(self.protected_verdict(&p, "RunCommand", probe));
                }
                let denied = forms.iter().find_map(|f| {
                    self.deny_pattern(f).map(|pat| {
                        Verdict::refused(
                            "PathMatchesDenyPattern",
                            format!("argument '{probe}' matches deny pattern '{pat}' ([policy] deny_path_patterns)."),
                            "RunCommand",
                            probe,
                        )
                    })
                });
                if denied.is_some() {
                    return denied;
                }
                // A known path must stay under the roots, as written and as linked.
                let r = resolved.as_ref()?;
                let within =
                    |p: &Path| x0ne_runtime::executor::confine::is_within_roots(p, &self.arg_roots);
                if within(&lexical(r)) && within(&real(r)) {
                    return None;
                }
                Some(Verdict::refused(
                    "ArgumentOutsideRoots",
                    format!(
                        "argument '{probe}' names {} outside [policy] allowed_read_paths and \
                         allowed_write_paths: a program could read or write it.",
                        real(r).display()
                    ),
                    "RunCommand",
                    probe,
                ))
            }
            Gesture::Action { action, target } => Some(self.action(action, target)),
        }
    }

    fn action(&self, raw: &Value, target: &str) -> Verdict {
        let action: Action = match serde_json::from_value(raw.clone()) {
            Ok(a) => a,
            Err(e) => {
                return Verdict::failure(
                    "BadInput",
                    format!("cannot express the gesture as an action: {e}"),
                )
            }
        };
        let name = action.type_name().to_string();
        let modifying = action.side_effect_class().touches_host();
        match self.engine.evaluate(&action) {
            PolicyEffect::Deny(r) => {
                let rule = match serde_json::to_value(&r) {
                    Ok(Value::String(s)) => s,
                    Ok(Value::Object(m)) => m.keys().next().cloned().unwrap_or_default(),
                    _ => "Deny".into(),
                };
                Verdict::refused(&rule, r.message(), &name, target)
            }
            PolicyEffect::RequireConfirmation(m) => Verdict {
                outcome: Outcome::Asked,
                rule: "RequireConfirmation".into(),
                reason: m,
                action: name,
                target: target.into(),
                failure: false,
            },
            PolicyEffect::Allow if modifying && self.mode == "readonly" => Verdict::refused(
                "ReadonlyMode",
                x0ne_runtime::policy::DenyReason::ReadonlyMode.message(),
                &name,
                target,
            ),
            PolicyEffect::Allow if modifying && self.mode == "confirm" => Verdict {
                outcome: Outcome::Asked,
                rule: "ConfirmMode".into(),
                reason: format!(
                    "x0ne policy mode is \"confirm\": {name} `{target}` needs your yes"
                ),
                action: name,
                target: target.into(),
                failure: false,
            },
            PolicyEffect::Allow => Verdict {
                outcome: Outcome::Allowed,
                rule: "Allow".into(),
                reason: format!("x0ne policy allows {name} `{target}`"),
                action: name,
                target: target.into(),
                failure: false,
            },
        }
    }

    /// The first refusal wins; otherwise the first question; otherwise allow.
    fn all(&self, gestures: &[Gesture]) -> Verdict {
        let mut worst: Option<Verdict> = None;
        // Programs and opened files first: their refusal names the lever.
        let (named, rest): (Vec<&Gesture>, Vec<&Gesture>) = gestures
            .iter()
            .partition(|g| matches!(g, Gesture::PathArg { .. }));
        for g in rest.into_iter().chain(named) {
            let Some(v) = self.one(g) else { continue };
            if v.outcome == Outcome::Refused {
                return v;
            }
            if worst.as_ref().is_none_or(|w| v.outcome > w.outcome) {
                worst = Some(v);
            }
        }
        worst.unwrap_or(Verdict {
            outcome: Outcome::Allowed,
            rule: "Allow".into(),
            reason: "nothing to run".into(),
            action: "-".into(),
            target: "-".into(),
            failure: false,
        })
    }
}

fn record(v: &Verdict, ctx: &Context, args: &HookArgs) {
    let priority = match v.outcome {
        Outcome::Refused => "4",
        Outcome::Asked => "5",
        Outcome::Allowed => "6",
    };
    let tool = if ctx.tool.is_empty() { "-" } else { &ctx.tool };
    let message = format!(
        "x0ne-harness: {} {} `{}`: {}",
        v.outcome.word(),
        tool,
        v.target,
        v.rule
    );
    let or_dash = |s: &str| {
        if s.is_empty() {
            "-".to_string()
        } else {
            s.to_string()
        }
    };
    let fields: Vec<(&str, String)> = vec![
        ("MESSAGE", message),
        ("PRIORITY", priority.into()),
        ("SYSLOG_IDENTIFIER", journal::IDENTIFIER.into()),
        ("X0NE_EVENT", "harness".into()),
        ("X0NE_OUTCOME", v.outcome.word().into()),
        (
            "X0NE_TIME",
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        ),
        ("X0NE_AGENT", args.agent.name().into()),
        ("X0NE_TOOL", tool.into()),
        ("X0NE_ACTION", v.action.clone()),
        ("X0NE_RULE", v.rule.clone()),
        ("X0NE_REASON", v.reason.clone()),
        ("X0NE_TARGET", v.target.clone()),
        ("X0NE_CWD", or_dash(&ctx.cwd)),
        ("X0NE_SESSION", or_dash(&ctx.session)),
        ("X0NE_TOOL_USE_ID", or_dash(&ctx.tool_use_id)),
        ("X0NE_SUBAGENT", or_dash(&ctx.subagent)),
        (
            "X0NE_POLICY",
            if ctx.policy.is_empty() {
                "none".into()
            } else {
                ctx.policy.clone()
            },
        ),
    ];
    let socket = args
        .journal_socket
        .clone()
        .unwrap_or_else(|| PathBuf::from(journal::DEFAULT_SOCKET));
    if let Err(e) = journal::send(&socket, &fields) {
        eprintln!(
            "x0ne hook: journal unavailable ({}: {e}); decision: {}",
            socket.display(),
            v.outcome.word()
        );
    }
}

fn respond(v: &Verdict, agent: Agent) -> ! {
    let (decision, reason) = match (v.outcome, agent) {
        (Outcome::Allowed, _) => ("allow", v.reason.clone()),
        (Outcome::Asked, Agent::ClaudeCode) => ("ask", v.reason.clone()),
        // Codex parses "ask" but does not support it: the call would go on.
        (Outcome::Asked, Agent::Codex) => (
            "deny",
            format!(
                "x0ne policy asks for a human yes and Codex hooks cannot ask: {}",
                v.reason
            ),
        ),
        (Outcome::Refused, _) => (
            "deny",
            format!("x0ne policy refused ({}): {}", v.rule, v.reason),
        ),
    };
    let silent_allow = decision == "allow" && agent == Agent::Codex;
    if !silent_allow {
        let out = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason,
            }
        });
        println!("{out}");
    }
    if v.failure {
        eprintln!("{reason}");
        std::process::exit(2);
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_envelope_names_every_file() {
        let p = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Update File: src/b.rs\n*** Move to: c.rs\n*** Delete File: /etc/d\n*** End Patch";
        let got: Vec<_> = patch_paths(p).into_iter().map(|(_, p)| p).collect();
        assert_eq!(got, ["a.txt", "src/b.rs", "c.rs", "/etc/d"]);
    }

    #[test]
    fn lexical_folds_dot_dot() {
        assert_eq!(
            lexical(Path::new("/a/b/../../etc/./x")),
            PathBuf::from("/etc/x")
        );
    }
}
