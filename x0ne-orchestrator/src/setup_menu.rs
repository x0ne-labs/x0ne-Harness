//! `/setup` as a re-entrant configuration menu.
//!
//! The old wizard was a one-shot first-run flow that REGENERATED the runtime
//! config from a template. Fine the first time, destructive on re-entry: come
//! back to flip one flag and your allowed paths, allowed commands and every
//! inline comment are reset. This module edits surgically instead — load the
//! document, set one key, write it back — so the file survives being visited.
//!
//! It also hides a split the operator should not have to care about: settings
//! live in two files (`~/.x0ne/config.toml` for the orchestrator,
//! `~/.config/x0ne/x0ne-runtime.toml` for the policy) plus `~/.x0ne/.env` for
//! secrets. You pick `[4] Cloud`; the menu knows where that lives.
//!
//! Deliberately NOT depending on `x0ne-runtime`: the menu reads the handful of
//! policy values it displays straight out of the TOML document into a local
//! [`RuntimeSummary`], and writes back the same keys. No new crate dependency,
//! no layering change.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use toml_edit::{value, Array, DocumentMut};

use crate::config::Config;

// ── Paths ─────────────────────────────────────────────────────────────────────

/// Where secrets live. One place, so a key set from `~/x0ne` is still found
/// when X0NE runs from `/tmp` — the exact failure the old cwd-relative `.env`
/// produced.
pub fn secrets_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join(".env")
}

/// The runtime policy config the menu edits.
fn runtime_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("x0ne")
        .join("x0ne-runtime.toml")
}

// ── Secrets ───────────────────────────────────────────────────────────────────

/// Whether a credential is present, WITHOUT carrying its value. The renderer
/// takes this rather than the secret so the key cannot reach a display path by
/// accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPresence {
    /// Set in the process environment (an explicit `export` wins over the file).
    FromEnv,
    /// Present in `~/.x0ne/.env`.
    FromFile,
    Absent,
}

impl KeyPresence {
    fn label(self) -> String {
        match self {
            Self::FromEnv => "● key OK (env)".to_string(),
            Self::FromFile => "● key OK".to_string(),
            Self::Absent => "key missing".to_string(),
        }
    }
}

/// Resolve whether `var` is available, and from where. Reads the file directly
/// rather than the process env for the file case, so the menu can report
/// truthfully even before the next launch loads it.
pub fn key_presence(var: &str) -> KeyPresence {
    if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
        return KeyPresence::FromEnv;
    }
    let path = secrets_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return KeyPresence::Absent;
    };
    if env_file_has_key(&text, var) {
        KeyPresence::FromFile
    } else {
        KeyPresence::Absent
    }
}

/// Does this `.env` body define `var` with a non-empty value? Commented lines
/// do not count.
pub fn env_file_has_key(body: &str, var: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim();
        if l.starts_with('#') {
            return false;
        }
        match l.split_once('=') {
            Some((k, v)) => k.trim() == var && !v.trim().is_empty(),
            None => false,
        }
    })
}

/// Mask a secret for display: enough tail to recognise it, never enough to use
/// it. Short values are fully masked rather than mostly revealed.
pub fn mask_secret(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return "…".repeat(4);
    }
    let tail: String = value.chars().skip(n - 4).collect();
    format!("…{}", tail)
}

/// Write a secret to `~/.x0ne/.env`, merging (an unrelated key already there is
/// preserved) and then tightening the file to 0600.
pub fn save_secret(var: &str, value: &str) -> Result<PathBuf> {
    let path = secrets_path();
    save_secret_at(&path, var, value)?;
    Ok(path)
}

/// [`save_secret`] into a given file. The file is created owner-only, so the
/// key is never readable by others, not even between write and chmod.
pub fn save_secret_at(path: &Path, var: &str, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if !path.exists() {
        write_owner_only(path, b"")?;
    }
    crate::setup::merge_env_file(
        &path.to_string_lossy(),
        &format!("{}={}\n", var, value.trim()),
    );
    restrict_to_owner(path)
}

/// Write `bytes` to `path`, created with mode 0600 on unix.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    f.write_all(bytes)?;
    restrict_to_owner(path)
}

/// Remove a secret from the file, leaving the others untouched.
pub fn forget_secret(var: &str) -> Result<()> {
    let path = secrets_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(()); // nothing stored is already the desired state
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| match l.trim().split_once('=') {
            Some((k, _)) => k.trim() != var,
            None => true,
        })
        .collect();
    let mut body = kept.join("\n");
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(&path, body).with_context(|| format!("cannot write {}", path.display()))?;
    restrict_to_owner(&path)
}

/// chmod 0600 — the file holds credentials, so nobody but the owner reads it.
/// A no-op on non-Unix, where the mode concept does not apply.
fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod 600 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// ── Runtime policy summary ────────────────────────────────────────────────────

/// The few policy values the menu displays, lifted out of the runtime TOML.
///
/// A local struct rather than `x0ne_runtime::config::AppConfig`: the
/// orchestrator does not depend on the runtime crate and must not start doing
/// so to read four fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeSummary {
    pub mode: String,
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
    pub command_count: usize,
    pub desktop_enabled: bool,
}

/// Extract the summary from a parsed runtime config. Missing tables and
/// missing keys yield defaults instead of panicking — a hand-edited or
/// partial config must still render a menu.
pub fn summarize_runtime(doc: &DocumentMut) -> RuntimeSummary {
    let policy = doc.get("policy").and_then(|p| p.as_table());
    let strings = |key: &str| -> Vec<String> {
        policy
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    RuntimeSummary {
        mode: policy
            .and_then(|p| p.get("mode"))
            .and_then(|v| v.as_str())
            .unwrap_or("confirm")
            .to_string(),
        read_paths: strings("allowed_read_paths"),
        write_paths: strings("allowed_write_paths"),
        command_count: strings("allowed_commands").len(),
        desktop_enabled: doc
            .get("desktop")
            .and_then(|d| d.as_table())
            .and_then(|d| d.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Load the runtime config as an editable document. A file that exists but does
/// not parse is an ERROR, never silently replaced by defaults — that would
/// discard the operator's policy, which is the one thing a security config must
/// never do quietly.
pub fn load_runtime_doc() -> Result<DocumentMut> {
    let path = runtime_config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .with_context(|| format!("invalid TOML in {}", path.display())),
        // Absent is the one legitimate empty document. Any OTHER read error
        // (permissions, a directory at that path, I/O) used to become an
        // empty document too — and the next write produced a policy without
        // `default_effect`, the 2026-08-10 incident through another door
        // (audit 2026-09-11, 02-I10a).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Set `table.key = <bool>` in place, creating the table if absent, and write
/// the document back. Comments, ordering and every other key survive.
pub fn set_runtime_bool(doc: &mut DocumentMut, table: &str, key: &str, on: bool) {
    doc[table][key] = value(on);
}

/// Set `table.key = <string>` in place.
pub fn set_runtime_str(doc: &mut DocumentMut, table: &str, key: &str, v: &str) {
    doc[table][key] = value(v);
}

/// Replace a string array (`allowed_read_paths`, `allowed_commands`, …).
pub fn set_runtime_array(doc: &mut DocumentMut, table: &str, key: &str, items: &[String]) {
    let mut arr = Array::new();
    for i in items {
        arr.push(i.as_str());
    }
    doc[table][key] = value(arr);
}

/// Persist an edited runtime document.
pub fn save_runtime_doc(doc: &DocumentMut) -> Result<PathBuf> {
    let path = runtime_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

// ── Confinement guard ─────────────────────────────────────────────────────────

/// Would `read_paths` let the agent read the secrets file?
///
/// This is the protection that actually matters for a key on disk. Encryption
/// at rest does nothing against a prompt-injected agent running as the
/// operator; keeping the file outside the agent's read roots does. The menu
/// refuses to save a read path that covers it.
pub fn read_paths_expose_secrets(read_paths: &[String], secrets: &Path) -> bool {
    let secrets = expand_tilde(&secrets.to_string_lossy());
    read_paths.iter().any(|p| {
        let root = expand_tilde(p);
        // A bare "." is the launch directory; treat it as covering nothing,
        // since it is resolved at runtime and the menu cannot know it here.
        if root == Path::new(".") {
            return false;
        }
        secrets.starts_with(&root)
    })
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest);
    }
    if p == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    PathBuf::from(p)
}

// ── The state view ────────────────────────────────────────────────────────────

/// Render the menu's state view. A pure function so it is testable without a
/// terminal — and so the secret can never reach it: it takes [`KeyPresence`],
/// not a value.
pub fn render_menu(cfg: &Config, rt: &RuntimeSummary, key: KeyPresence) -> Vec<String> {
    let model = cfg.active_model_name();
    let provider = cfg.llm.provider.clone();
    let paths = if rt.read_paths.is_empty() {
        "none (everything denied)".to_string()
    } else {
        rt.read_paths.join(", ")
    };
    vec![
        String::new(),
        "  X0NE configuration".to_string(),
        String::new(),
        format!(
            "  [1] LLM        {} / {}   {}",
            provider,
            model,
            key.label()
        ),
        format!("  [2] Workspace  {}", paths),
        format!(
            "  [3] Policy     {} · {} allowed commands",
            rt.mode, rt.command_count
        ),
        format!(
            "  [4] Cloud      {}",
            if cfg.cloud.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        format!(
            "  [5] Desktop    {}",
            if rt.desktop_enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        "  [6] Secrets    API keys (set here, never in the TOML)".to_string(),
        String::new(),
        "  [q] quit".to_string(),
        String::new(),
    ]
}

/// Print the state view.
pub fn print_menu(cfg: &Config, rt: &RuntimeSummary, key: KeyPresence) {
    for line in render_menu(cfg, rt, key) {
        println!("{}", line);
    }
}

// ── The menu loop ─────────────────────────────────────────────────────────────

fn read_line() -> Result<String> {
    use std::io::BufRead;
    let mut buf = String::new();
    std::io::stdin().lock().read_line(&mut buf)?;
    Ok(buf)
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// The environment variable each provider's key lives under.
fn key_var_for(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "openai-compat" | "openai_compat" | "litellm" => "X0NE_OPENAI_COMPAT_KEY",
        _ => "ANTHROPIC_API_KEY",
    }
}

/// Open the configuration menu. Returns when the operator quits.
pub fn run() -> Result<()> {
    loop {
        // Reload every pass so the view reflects edits made in this session,
        // including ones made by the delegated LLM setup.
        let cfg = Config::find_and_load(None).unwrap_or_default();
        let doc = load_runtime_doc()?;
        let rt = summarize_runtime(&doc);
        let var = key_var_for(&cfg.llm.provider);

        print_menu(&cfg, &rt, key_presence(var));
        print!("  choice › ");
        flush();

        let choice = read_line()?.trim().to_string();
        match choice.as_str() {
            "1" => section_llm()?,
            "2" => section_workspace(&rt)?,
            "3" => section_policy(&rt)?,
            "4" => section_cloud(&cfg)?,
            "5" => section_desktop(&rt)?,
            "6" => section_secrets(&cfg)?,
            "q" | "quit" | "" => return Ok(()),
            other => {
                println!("  {} unknown choice: {}", "✗".red(), other);
            }
        }
    }
}

/// [1] LLM — delegate to the existing service picker, which already does the
/// provider list, model listing and connection check.
fn section_llm() -> Result<()> {
    let mut cfg = Config::find_and_load(None).unwrap_or_default();
    cfg.apply_env_overrides();
    let handle = tokio::runtime::Handle::try_current()
        .context("the LLM section needs the async runtime; run /setup from X0NE")?;
    // Asked here because the user opened this section: loopback only.
    let found = tokio::task::block_in_place(|| {
        handle.block_on(crate::local_models::discover(
            crate::local_models::PRESETS,
            crate::local_models::TIMEOUT,
        ))
    });
    println!();
    println!("  Local model servers (loopback only)");
    for line in crate::local_models::report(&found) {
        println!("  {} {line}", "·".dimmed());
    }
    println!();
    let result =
        tokio::task::block_in_place(|| handle.block_on(crate::llm_setup::setup_llm(&mut cfg)));
    match result {
        Ok(backend) => println!("  {} {}", "[ok]".green().bold(), backend.display_name()),
        Err(e) => println!("  {} LLM configuration aborted: {}", "⚠".yellow(), e),
    }
    Ok(())
}

/// The commands a developer session needs. Offered as one choice because
/// picking 27 binaries one at a time is not configuration, it is data entry —
/// and an operator who is told "0 allowed commands" has no way to guess
/// which ones to type.
const DEV_COMMANDS: &[&str] = &[
    "cargo", "git", "npm", "node", "python3", "python", "pytest", "make", "grep", "find", "ls",
    "cat", "head", "tail", "wc", "sort", "rustc", "go", "docker", "podman", "curl", "wget",
    "mkdir", "cp", "mv", "rm", "chmod", "echo", "env",
];

/// [2] Workspace — which directories the agent may read and write.
///
/// Leads with the launch directory, because "X0NE cannot see the project I
/// just opened it in" is the single most common way a session dies: every
/// probe comes back `not under any allowed path prefix` and the model burns a
/// turn guessing (field report, 2026-08-12).
fn section_workspace(rt: &RuntimeSummary) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_s = cwd.to_string_lossy().to_string();
    let covered = path_is_covered(&rt.read_paths, &cwd);

    println!();
    println!("  Directories the agent can access");
    println!(
        "  {} read : {}",
        "·".dimmed(),
        if rt.read_paths.is_empty() {
            "none (everything denied)".to_string()
        } else {
            rt.read_paths.join(", ")
        }
    );
    println!(
        "  {} write: {}",
        "·".dimmed(),
        if rt.write_paths.is_empty() {
            "none (everything denied)".to_string()
        } else {
            rt.write_paths.join(", ")
        }
    );
    println!();
    if covered {
        println!("  {} the current directory is accessible.", "✓".green());
    } else {
        println!(
            "  {} the current directory {} is NOT accessible, the agent will see nothing there.",
            "⚠".yellow().bold(),
            cwd_s.bold()
        );
    }
    println!();
    println!("  [1] allow the current directory (read + write)");
    println!("  [2] add a path");
    println!("  [3] reset everything (no access)");
    println!("  [q] back");
    print!("  choice › ");
    flush();

    let mut doc = load_runtime_doc()?;
    let mut read = rt.read_paths.clone();
    let mut write = rt.write_paths.clone();
    match read_line()?.trim() {
        "1" => {
            if !read.contains(&cwd_s) {
                read.push(cwd_s.clone());
            }
            if !write.contains(&cwd_s) {
                write.push(cwd_s.clone());
            }
        }
        "2" => {
            print!("  path › ");
            flush();
            let p = read_line()?.trim().to_string();
            if p.is_empty() {
                println!("  {} cancelled.", "·".dimmed());
                println!();
                return Ok(());
            }
            if !read.contains(&p) {
                read.push(p.clone());
            }
            print!("  also allow writing? [y/N] › ");
            flush();
            if matches!(
                read_line()?.trim().to_lowercase().as_str(),
                "y" | "yes" | "o" | "oui"
            ) && !write.contains(&p)
            {
                write.push(p);
            }
        }
        "3" => {
            read.clear();
            write.clear();
        }
        _ => {
            println!();
            return Ok(());
        }
    }

    // An allowlist that reaches the secrets file hands the agent its own
    // credentials. That is the protection encryption at rest cannot give, so
    // it is stated plainly and confirmed rather than applied silently.
    if read_paths_expose_secrets(&read, &secrets_path()) {
        println!();
        println!(
            "  {} these paths cover {}, the agent could READ your API keys.",
            "⚠".yellow().bold(),
            secrets_path().display()
        );
        print!("  confirm anyway? [y/N] › ");
        flush();
        if !matches!(
            read_line()?.trim().to_lowercase().as_str(),
            "y" | "yes" | "o" | "oui"
        ) {
            println!("  {} cancelled, nothing changed.", "·".dimmed());
            println!();
            return Ok(());
        }
    }

    set_runtime_array(&mut doc, "policy", "allowed_read_paths", &read);
    set_runtime_array(&mut doc, "policy", "allowed_write_paths", &write);
    let path = save_runtime_doc(&doc)?;
    println!(
        "  {} saved — {}",
        "[ok]".green().bold(),
        path.display().to_string().dimmed()
    );
    println!(
        "  {} restart X0NE so the runtime rereads its policy.",
        "ℹ".truecolor(132, 142, 157)
    );
    println!();
    Ok(())
}

/// [3] Policy — confirmation mode and the command allowlist.
fn section_policy(rt: &RuntimeSummary) -> Result<()> {
    println!();
    println!("  Execution policy");
    println!(
        "  {} mode : {} {}",
        "·".dimmed(),
        rt.mode.bold(),
        if rt.mode == "auto" {
            "(runs without asking if the allowlists pass)".dimmed()
        } else {
            "(asks before write/command)".dimmed()
        }
    );
    println!(
        "  {} allowed commands : {}",
        "·".dimmed(),
        if rt.command_count == 0 {
            "0 — RunCommand will always fail".yellow().to_string()
        } else {
            format!("{}", rt.command_count)
        }
    );
    println!();
    println!(
        "  [1] allow the development command set ({})",
        DEV_COMMANDS.len()
    );
    println!("  [2] allow no command");
    println!("  [3] toggle the mode (confirm ↔ auto)");
    println!("  [q] back");
    print!("  choice › ");
    flush();

    let mut doc = load_runtime_doc()?;
    match read_line()?.trim() {
        "1" => {
            let cmds: Vec<String> = DEV_COMMANDS.iter().map(|s| s.to_string()).collect();
            set_runtime_array(&mut doc, "policy", "allowed_commands", &cmds);
        }
        "2" => set_runtime_array(&mut doc, "policy", "allowed_commands", &[]),
        "3" => {
            let next = if rt.mode == "auto" { "confirm" } else { "auto" };
            if next == "auto" {
                println!(
                    "  {} in `auto` the agent no longer waits for your confirmation on what the allowlists permit.",
                    "⚠".yellow().bold()
                );
                print!("  confirm? [y/N] › ");
                flush();
                if !matches!(
                    read_line()?.trim().to_lowercase().as_str(),
                    "y" | "yes" | "o" | "oui"
                ) {
                    println!("  {} cancelled.", "·".dimmed());
                    println!();
                    return Ok(());
                }
            }
            set_runtime_str(&mut doc, "policy", "mode", next);
        }
        _ => {
            println!();
            return Ok(());
        }
    }
    let path = save_runtime_doc(&doc)?;
    println!(
        "  {} saved — {}",
        "[ok]".green().bold(),
        path.display().to_string().dimmed()
    );
    println!(
        "  {} restart X0NE so the runtime rereads its policy.",
        "ℹ".truecolor(132, 142, 157)
    );
    println!();
    Ok(())
}

/// Is `target` inside any of these allowlist roots? Used to tell the operator
/// whether the directory they launched in is actually usable.
pub fn path_is_covered(roots: &[String], target: &Path) -> bool {
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    roots.iter().any(|r| {
        let root = expand_tilde(r);
        let root = root.canonicalize().unwrap_or(root);
        target.starts_with(&root)
    })
}

/// [4] Cloud — one flag in the orchestrator config, plus the distinction the
/// operator otherwise has to learn the hard way.
fn section_cloud(cfg: &Config) -> Result<()> {
    println!();
    println!(
        "  Cost + GPU monitoring — currently {}",
        if cfg.cloud.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  {} the monitor queries gcloud/nvidia-smi directly (your dashboard).",
        "·".dimmed()
    );
    println!(
        "  {} for the MODEL to query the cost, gcloud and nvidia-smi must also be",
        "·".dimmed()
    );
    println!("    in policy.allowed_commands, which is a separate setting.",);
    print!("  enable? [y/N] › ");
    flush();
    let on = matches!(
        read_line()?.trim().to_lowercase().as_str(),
        "y" | "yes" | "o" | "oui"
    );

    let mut user = load_user_doc()?;
    user["cloud"]["enabled"] = value(on);
    let path = save_user_doc(&user)?;
    println!(
        "  {} cloud {} — {}",
        "[ok]".green().bold(),
        if on { "enabled" } else { "disabled" },
        path.display().to_string().dimmed()
    );
    println!();
    Ok(())
}

/// [5] Desktop — a real capability grant, so it says what it opens.
fn section_desktop(rt: &RuntimeSummary) -> Result<()> {
    println!();
    println!(
        "  Desktop control — currently {}",
        if rt.desktop_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  {} gives the agent keyboard, mouse, screenshots and clipboard.",
        "⚠".yellow()
    );
    print!("  enable? [y/N] › ");
    flush();
    let on = matches!(
        read_line()?.trim().to_lowercase().as_str(),
        "y" | "yes" | "o" | "oui"
    );

    let mut doc = load_runtime_doc()?;
    set_runtime_bool(&mut doc, "desktop", "enabled", on);
    let path = save_runtime_doc(&doc)?;
    println!(
        "  {} desktop {} — {}",
        "[ok]".green().bold(),
        if on { "enabled" } else { "disabled" },
        path.display().to_string().dimmed()
    );
    println!();
    Ok(())
}

/// [6] Secrets — the section this whole menu was asked for: put an API key in
/// from inside X0NE, without touching a file by hand.
fn section_secrets(cfg: &Config) -> Result<()> {
    let var = key_var_for(&cfg.llm.provider);
    println!();
    println!(
        "  Key for {} — variable {}",
        cfg.llm.provider.bold(),
        var.dimmed()
    );
    match key_presence(var) {
        KeyPresence::FromEnv => println!(
            "  {} an environment variable is already set; it takes precedence over the file.",
            "ℹ".truecolor(132, 142, 157)
        ),
        KeyPresence::FromFile => println!("  {} a key is stored.", "●".green()),
        KeyPresence::Absent => println!("  {} no key stored.", "·".dimmed()),
    }
    println!("  [1] set / replace the key");
    println!("  [2] remove the stored key");
    println!("  [q] back");
    print!("  choice › ");
    flush();

    match read_line()?.trim() {
        "1" => {
            print!("  key (it will not be shown again) › ");
            flush();
            let key = read_line()?.trim().to_string();
            if key.is_empty() {
                println!("  {} cancelled, nothing was written.", "·".dimmed());
                println!();
                return Ok(());
            }
            let path = save_secret(var, &key)?;
            println!(
                "  {} key {} saved in {} (0600)",
                "[ok]".green().bold(),
                mask_secret(&key),
                path.display().to_string().dimmed()
            );
            warn_if_agent_can_read_secrets();
        }
        "2" => {
            forget_secret(var)?;
            println!("  {} key removed.", "[ok]".green().bold());
        }
        _ => {}
    }
    println!();
    Ok(())
}

/// The protection that actually matters for a key on disk: it must sit outside
/// the agent's read roots. Encryption at rest does nothing against a
/// prompt-injected agent running as the operator; confinement does.
fn warn_if_agent_can_read_secrets() {
    let Ok(doc) = load_runtime_doc() else { return };
    let rt = summarize_runtime(&doc);
    if read_paths_expose_secrets(&rt.read_paths, &secrets_path()) {
        println!(
            "  {} the agent can READ this file: {} covers {}.",
            "⚠".yellow().bold(),
            rt.read_paths.join(", "),
            secrets_path().display()
        );
        println!(
            "    {} remove this path from policy.allowed_read_paths.",
            "→".dimmed()
        );
    }
}

// ── Orchestrator config document ──────────────────────────────────────────────

fn user_config_path() -> PathBuf {
    Config::default_user_config_path()
}

/// Load `~/.x0ne/config.toml` as an editable document. Same fail-closed rule as
/// the runtime config: unparseable is an error, not a silent reset.
fn load_user_doc() -> Result<DocumentMut> {
    let path = user_config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .with_context(|| format!("invalid TOML in {}", path.display())),
        Err(_) => Ok(DocumentMut::new()),
    }
}

fn save_user_doc(doc: &DocumentMut) -> Result<PathBuf> {
    let path = user_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RUNTIME: &str = r#"
# x0ne-runtime configuration
[policy]
# This comment must survive a menu visit.
mode = "confirm"
default_effect = "allow"
allowed_read_paths = ["~/code", "/tmp"]
allowed_write_paths = ["/tmp"]
allowed_commands = ["git", "ls", "cat"]

[desktop]
enabled = false
"#;

    // ── RuntimeSummary ───────────────────────────────────────────────

    #[test]
    fn summarize_reads_the_policy_fields() {
        let doc: DocumentMut = SAMPLE_RUNTIME.parse().unwrap();
        let s = summarize_runtime(&doc);
        assert_eq!(s.mode, "confirm");
        assert_eq!(s.read_paths, vec!["~/code", "/tmp"]);
        assert_eq!(s.write_paths, vec!["/tmp"]);
        assert_eq!(s.command_count, 3);
        assert!(!s.desktop_enabled);
    }

    #[test]
    fn summarize_survives_a_config_missing_everything() {
        // A hand-edited or half-written config must still render a menu
        // rather than panicking the operator out of their only repair tool.
        let doc: DocumentMut = "".parse().unwrap();
        let s = summarize_runtime(&doc);
        assert_eq!(s.mode, "confirm", "must default, not panic");
        assert!(s.read_paths.is_empty());
        assert_eq!(s.command_count, 0);
        assert!(!s.desktop_enabled);
    }

    // ── Surgical edits ───────────────────────────────────────────────

    #[test]
    fn editing_one_key_preserves_comments_and_neighbours() {
        // THE load-bearing property. The old wizard rewrote the whole file
        // from a template; a re-entrant menu doing that would wipe the
        // operator's policy every visit.
        let mut doc: DocumentMut = SAMPLE_RUNTIME.parse().unwrap();
        set_runtime_bool(&mut doc, "desktop", "enabled", true);
        let out = doc.to_string();

        assert!(
            out.contains("This comment must survive a menu visit."),
            "comments were lost:\n{out}"
        );
        assert!(
            out.contains(r#"default_effect = "allow""#),
            "neighbour key lost"
        );
        assert!(out.contains(r#"allowed_commands = ["git", "ls", "cat"]"#));
        // And the edit landed.
        let s = summarize_runtime(&doc);
        assert!(s.desktop_enabled);
    }

    #[test]
    fn editing_creates_a_missing_table_without_touching_the_rest() {
        let mut doc: DocumentMut = SAMPLE_RUNTIME.parse().unwrap();
        set_runtime_str(&mut doc, "policy", "mode", "auto");
        set_runtime_array(
            &mut doc,
            "policy",
            "allowed_commands",
            &["git".into(), "cargo".into()],
        );
        let s = summarize_runtime(&doc);
        assert_eq!(s.mode, "auto");
        assert_eq!(s.command_count, 2);
        assert!(doc.to_string().contains("must survive"));
    }

    #[test]
    fn a_malformed_runtime_config_is_an_error_not_a_silent_reset() {
        // Fail closed: replacing an unparseable security config with defaults
        // would quietly widen the policy.
        let bad = "[policy\nmode = ".parse::<DocumentMut>();
        assert!(bad.is_err(), "malformed TOML must not parse");
    }

    // ── Secrets ──────────────────────────────────────────────────────

    #[test]
    fn env_file_key_detection_ignores_comments_and_empties() {
        assert!(env_file_has_key(
            "ANTHROPIC_API_KEY=sk-ant-x\n",
            "ANTHROPIC_API_KEY"
        ));
        assert!(!env_file_has_key(
            "#ANTHROPIC_API_KEY=sk\n",
            "ANTHROPIC_API_KEY"
        ));
        assert!(!env_file_has_key(
            "ANTHROPIC_API_KEY=\n",
            "ANTHROPIC_API_KEY"
        ));
        assert!(!env_file_has_key("OTHER=1\n", "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn mask_never_reveals_enough_to_use() {
        let masked = mask_secret("sk-ant-api03-abcdefghijklmnop4f2a");
        assert_eq!(masked, "…4f2a");
        assert!(!masked.contains("sk-ant"), "prefix leaked: {masked}");
        // Short values are fully masked rather than mostly revealed.
        assert_eq!(
            mask_secret("short"),
            "………………".chars().take(4).collect::<String>()
        );
    }

    #[test]
    fn presence_label_never_contains_a_value() {
        for p in [
            KeyPresence::FromEnv,
            KeyPresence::FromFile,
            KeyPresence::Absent,
        ] {
            let l = p.label();
            assert!(!l.contains("sk-"), "label leaks a key shape: {l}");
        }
    }

    // ── Confinement guard ────────────────────────────────────────────

    #[test]
    fn a_read_path_covering_the_secrets_file_is_refused() {
        // Encryption at rest does nothing against a prompt-injected agent
        // running as the operator; keeping the file out of the agent's read
        // roots does. This is the guard that matters.
        let secrets = PathBuf::from("/home/u/.x0ne/.env");
        assert!(read_paths_expose_secrets(&["/home/u".into()], &secrets));
        assert!(read_paths_expose_secrets(
            &["/home/u/.x0ne".into()],
            &secrets
        ));
        assert!(!read_paths_expose_secrets(
            &["/home/u/code".into()],
            &secrets
        ));
        assert!(!read_paths_expose_secrets(&["/tmp".into()], &secrets));
    }

    // ── Launch-directory coverage ────────────────────────────────────

    #[test]
    fn path_coverage_answers_the_can_the_agent_see_here_question() {
        let tmp = std::env::temp_dir();
        // A root that contains the target covers it.
        assert!(path_is_covered(&[tmp.to_string_lossy().into()], &tmp));
        // An unrelated root does not.
        assert!(!path_is_covered(&["/definitely/not/here".into()], &tmp));
        // An EMPTY allowlist covers nothing — deny-all, which is exactly the
        // state that made a live session fail every probe.
        assert!(!path_is_covered(&[], &tmp));
    }

    #[test]
    fn path_coverage_is_not_fooled_by_a_prefix_that_is_not_a_parent() {
        // "/srv/root" must not be read as covering "/srv/rootEVIL" — the same
        // component-wise rule the runtime's own confinement uses.
        let target = PathBuf::from("/srv/rootEVIL/x");
        assert!(!path_is_covered(&["/srv/root".into()], &target));
    }

    #[test]
    fn the_dev_command_set_is_not_empty_and_covers_the_basics() {
        // "0 allowed commands" is a dead end an operator cannot escape by
        // guessing; the offered set has to actually let a dev session work.
        assert!(DEV_COMMANDS.len() > 10);
        for needed in ["git", "cargo", "ls", "cat", "pytest"] {
            assert!(DEV_COMMANDS.contains(&needed), "missing {needed}");
        }
    }

    #[test]
    fn a_bare_dot_read_path_is_not_treated_as_exposing() {
        // "." resolves at launch time; the menu cannot know what it covers,
        // and flagging it would make the common default unusable.
        let secrets = PathBuf::from("/home/u/.x0ne/.env");
        assert!(!read_paths_expose_secrets(&[".".into()], &secrets));
    }

    // ── State view ───────────────────────────────────────────────────

    fn cfg_with(provider: &str, cloud: bool) -> Config {
        let mut c = Config::default();
        c.llm.provider = provider.into();
        c.cloud.enabled = cloud;
        c
    }

    #[test]
    fn the_view_shows_each_section_and_its_state() {
        let rt = RuntimeSummary {
            mode: "confirm".into(),
            read_paths: vec!["~/x0ne".into()],
            write_paths: vec![],
            command_count: 27,
            desktop_enabled: false,
        };
        let lines = render_menu(&cfg_with("anthropic", true), &rt, KeyPresence::FromFile);
        let text = lines.join("\n");
        assert!(text.contains("[1] LLM"), "{text}");
        assert!(text.contains("anthropic"), "{text}");
        assert!(text.contains("key OK"), "{text}");
        assert!(text.contains("~/x0ne"), "{text}");
        assert!(text.contains("27 allowed commands"), "{text}");
        assert!(text.contains("[4] Cloud      enabled"), "{text}");
        assert!(text.contains("[5] Desktop    disabled"), "{text}");
        assert!(text.contains("[6] Secrets"), "{text}");
        assert!(text.contains("[q] quit"), "{text}");
    }

    #[test]
    fn the_view_says_when_no_key_is_set() {
        let lines = render_menu(
            &cfg_with("anthropic", false),
            &RuntimeSummary::default(),
            KeyPresence::Absent,
        );
        let text = lines.join("\n");
        assert!(text.contains("key missing"), "{text}");
        assert!(text.contains("[4] Cloud      disabled"), "{text}");
    }

    #[test]
    fn the_view_calls_an_empty_read_allowlist_what_it_is() {
        // An empty allowlist means DENY ALL. Rendering it as a blank would
        // read as "no restriction", the exact opposite of the truth.
        let lines = render_menu(
            &cfg_with("ollama", false),
            &RuntimeSummary::default(),
            KeyPresence::Absent,
        );
        let text = lines.join("\n");
        assert!(text.contains("everything denied"), "{text}");
    }
}

#[cfg(test)]
mod snapshot {
    use super::*;
    #[test]
    fn print_menu_snapshot() {
        let mut c = Config::default();
        c.llm.provider = "anthropic".into();
        c.cloud.enabled = false;
        let rt = RuntimeSummary {
            mode: "confirm".into(),
            read_paths: vec![".".into()],
            write_paths: vec![],
            command_count: 8,
            desktop_enabled: false,
        };
        println!();
        for l in render_menu(&c, &rt, KeyPresence::Absent) {
            println!("{}", l);
        }
        println!("  choice › ");
    }
}
