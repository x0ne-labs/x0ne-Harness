//! Workspace trust: whether the configuration found in the current directory
//! may be honored.
//!
//! A cloned repository is not the user. Before this module, `./.env` and
//! `./x0ne-orchestrator.toml` were applied on launch with no question asked,
//! and both could choose which program x0ne starts. `./.env` is no longer
//! loaded as configuration, and no project file may lead to it: it configures
//! the project's own software, not x0ne, whose secrets live in
//! `~/.x0ne/.env`. A deny-list of "dangerous" variables would be one
//! forgotten name away from the same hole. For the TOML, the model is Claude
//! Code's workspace trust, made stricter in these ways:
//!
//! * trust is bound to the CONTENT of the project files (like `direnv allow`):
//!   an edit — a `git pull`, or the agent itself writing into its own working
//!   directory — asks again;
//!   for a privileged directory that content includes the runtime policy its
//!   `[bridge] runtime_config` names, which must lie inside it;
//! * a project file is read only by [`read_inside`], which refuses it unless
//!   its real path lies inside the folder, is not a `.env*` file nor under a
//!   `.git` directory, and it is a regular file with one hard link and at most
//!   [`MAX_PROJECT_FILE_BYTES`]. A refused file (`AGENTS.md` linked to
//!   `~/.ssh/id_ed25519`, or to `./.env`) is shown refused with its target and
//!   never read, neither to fingerprint it nor to build the prompt. Every
//!   refusal and every link is shown on the question with its target, never
//!   folded; only regular files fold, past 8 lines, into one count;
//! * the check reads at most [`MAX_TRUST_FILES`] rules and skills and
//!   [`MAX_TRUST_BYTES`] in all: past either, it fails and the folder is not
//!   trusted;
//! * what the prompt and the config get is what was trusted: each file is
//!   read again when used, and handed over only if those bytes match the
//!   fingerprint the decision was taken on ([`read_trusted`]). The runtime
//!   policy of a privileged directory is the exception: the runtime reads it
//!   itself, later, unchecked against the fingerprint;
//! * even when trusted, a project sets only what [`PROJECT_SETTABLE`] lists;
//!   anything else comes from the user's config — unless the USER config lists
//!   the directory under `[project_trust] privileged` ([`is_privileged`]).
//!
//! Whatever the decision, `project::detect_project` reads `Cargo.toml`,
//! `package.json` and `pyproject.toml` through [`read_inside`] too, for the
//! project name it puts in the prompt, keeps that name only if it is a
//! package name, and tests `setup.py` and `.git` by their own entry. The
//! project tree lists the names under the folder without walking a link,
//! each folded to one line. `@file` and `/read` read a path under the working
//! directory through [`read_inside`] as well; a path outside it is the user's.
//!
//! The decision is taken once per process, before any config is loaded and
//! before any full-screen UI owns the terminal ([`decide`]). Everything
//! else only reads it ([`project_is_trusted`]); no answer means untrusted.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The files through which a directory configures x0ne.
/// `./.env` is not one: x0ne never loads it (see [`crate::project_trust`] docs).
pub const PROJECT_FILES: &[&str] = &["x0ne-orchestrator.toml"];

/// What a trusted project config may set, as `(table, key)`; `None` = the whole
/// table. Everything else — `llm`, `bridge`, `trust`, `cost`, `incus`, `cloud`,
/// `ui.confirm_writes`, `ui.auto_confirm`, and any table added later — comes
/// from the user's own config only.
pub const PROJECT_SETTABLE: &[(&str, Option<&str>)] = &[
    ("conversation", None),
    ("handles", None),
    ("display", None),
    ("ui", Some("compact_prompt")),
    ("ui", Some("force_full_prompt")),
    ("ui", Some("lazy_discovery")),
    ("ui", Some("auto_worlds_view")),
    ("ui", Some("worlds_one_line")),
];

/// What [`decide`] settled for this process: the answer, and when trusted,
/// the fingerprint the files must still match when they are used.
struct Decision {
    trusted: bool,
    approved: Option<Approved>,
}

static DECISION: OnceLock<Decision> = OnceLock::new();

/// True only if [`decide`] ran in this process and the directory was trusted.
pub fn project_is_trusted() -> bool {
    DECISION.get().is_some_and(|d| d.trusted)
}

/// One project file and the digest of its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDigest {
    pub name: String,
    /// SHA-256 of the content, or [`REFUSED`] and the reason when not read.
    pub sha256: String,
    /// Where `name` really is, when that differs from `name`: relative to
    /// the folder inside it, absolute outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The fingerprint a trust decision approved. A file read later for the
/// prompt or the config is used only if it still matches it: same digest,
/// same target. The bytes hashed are the bytes handed over.
#[derive(Debug, Clone)]
pub struct Approved {
    dir: PathBuf,
    files: Vec<FileDigest>,
}

impl Approved {
    /// `None` if `dir` cannot be resolved: then nothing is read.
    pub fn new(dir: &Path, files: Vec<FileDigest>) -> Option<Self> {
        Some(Self {
            dir: std::fs::canonicalize(dir).ok()?,
            files,
        })
    }

    /// `name` in `dir`, read once, if its digest and target are those
    /// approved; else `None`, with a warning when it changed.
    pub fn read(&self, dir: &Path, name: &str) -> Option<Vec<u8>> {
        if std::fs::canonicalize(dir).ok()? != self.dir {
            return None;
        }
        let Some(approved) = self.files.iter().find(|f| f.name == name) else {
            if std::fs::symlink_metadata(dir.join(name)).is_ok() {
                tracing::warn!("{name} was added since this folder was trusted: not read");
            }
            return None;
        };
        let found = match read_inside(dir, name) {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("{name}: {e}, not read");
                return None;
            }
        };
        if digest(name, &found).as_ref() != Some(approved) {
            tracing::warn!("{name} changed since this folder was trusted: not read");
            return None;
        }
        match found {
            Inside::Bytes { bytes, .. } => Some(bytes),
            _ => None,
        }
    }
}

/// [`Approved::read`] against this process's decision: `None` unless the
/// folder was trusted and `name` is, byte for byte, what was trusted.
pub fn read_trusted(dir: &Path, name: &str) -> Option<Vec<u8>> {
    DECISION.get()?.approved.as_ref()?.read(dir, name)
}

/// [`read_trusted`] as UTF-8 text, for the prompt.
pub fn read_trusted_text(dir: &Path, name: &str) -> Option<String> {
    String::from_utf8(read_trusted(dir, name)?).ok()
}

/// What `project::load_project_memory` feeds the model, at fixed paths.
/// Content a cloned repository wrote, read as instructions: part of the
/// trust decision, and of what an edit must invalidate.
pub const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "X0NE.md",
    ".x0ne/X0NE.md",
    ".x0ne/rules.md",
];

/// Directories whose `<name>/SKILL.md` `project::catalog_skills` reads.
pub const SKILL_DIRS: &[&str] = &["skills", ".x0ne/skills", ".claude/skills"];

/// The most rules and skills the trust check looks at: past it, the check fails, and the folder
/// is not trusted. Measured 2026-10-28: 4096 rules linked to one 1 MiB file made the check
/// hash 4 GiB before the question could be shown.
pub const MAX_TRUST_FILES: usize = 256;

/// The most bytes the trust check reads, all project files together, before it fails the
/// same way: 16 MiB, sixteen files at [`MAX_PROJECT_FILE_BYTES`].
pub const MAX_TRUST_BYTES: u64 = 16 * MAX_PROJECT_FILE_BYTES;

/// The project files in `dir`: [`PROJECT_FILES`], [`INSTRUCTION_FILES`], rules, skills, in
/// that order. A file [`read_inside`] refuses is listed unread: its digest is [`REFUSED`] and
/// the reason, which no content matches, with its target. More than [`MAX_TRUST_FILES`]
/// rules and skills is an error before any file is read, more than [`MAX_TRUST_BYTES`] read one as soon
/// as it is crossed.
pub fn discover(dir: &Path) -> Result<Vec<FileDigest>> {
    let mut names: Vec<String> = PROJECT_FILES
        .iter()
        .chain(INSTRUCTION_FILES)
        .map(|n| (*n).to_string())
        .collect();
    let rules = dir.join(".x0ne").join("rules");
    for name in sorted_entries(&rules, |p| p.extension().is_some_and(|e| e == "md")) {
        names.push(format!(".x0ne/rules/{name}"));
    }
    for sdir in SKILL_DIRS {
        for name in sorted_entries(&dir.join(sdir), |p| p.join("SKILL.md").is_file()) {
            names.push(format!("{sdir}/{name}/SKILL.md"));
        }
    }
    let listed = names.len() - PROJECT_FILES.len() - INSTRUCTION_FILES.len();
    if listed > MAX_TRUST_FILES {
        anyhow::bail!(
            "{listed} rules and skills: over the {MAX_TRUST_FILES} the trust check reviews"
        );
    }
    let mut found = Vec::new();
    let mut read: u64 = 0;
    for name in &names {
        let inside = read_inside(dir, name)
            .with_context(|| format!("cannot read {}", dir.join(name).display()))?;
        if let Inside::Bytes { bytes, .. } = &inside {
            read += bytes.len() as u64;
            if read > MAX_TRUST_BYTES {
                anyhow::bail!(
                    "project files over the {} MiB the trust check reviews",
                    MAX_TRUST_BYTES / MAX_PROJECT_FILE_BYTES
                );
            }
        }
        found.extend(digest(name, &inside));
    }
    Ok(found)
}

/// The digest of what [`read_inside`] found at `name`; `None` when absent.
fn digest(name: &str, found: &Inside) -> Option<FileDigest> {
    let (sha256, real) = match found {
        Inside::Absent => return None,
        Inside::Refused { real, why } => (format!("{REFUSED}{why}"), real),
        Inside::Bytes { real, bytes } => (hex(&Sha256::digest(bytes)), real),
    };
    let target = (real.as_path() != Path::new(name)).then(|| real.to_string_lossy().into_owned());
    Some(FileDigest {
        name: name.to_string(),
        sha256,
        target,
    })
}

/// Start of the digest of a project file [`read_inside`] refused, followed
/// by the reason. Never a SHA-256: no bytes read later can match it.
pub const REFUSED: &str = "refused: ";

/// The largest project file read, in bytes: 1 MiB, the runtime's default
/// `policy.max_read_bytes`, the most one `ReadFile` hands the model.
pub const MAX_PROJECT_FILE_BYTES: u64 = 1_048_576;

/// Why [`read_inside`] did not read a project file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its real path leaves the folder. The file was not opened.
    Outside,
    /// Its real path is a `.env*` file or lies under a `.git` directory:
    /// the folder's own secrets. The file was not opened.
    Secret,
    /// It has more than one hard link, so another name, maybe outside the
    /// folder, is the same file.
    HardLink,
    /// Over [`MAX_PROJECT_FILE_BYTES`], measured on the open descriptor.
    TooLarge,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Outside => "outside this folder",
            Self::Secret => "a .env file or a .git path of this folder",
            Self::HardLink => "a file with more than one hard link",
            Self::TooLarge => "larger than 1 MiB",
        })
    }
}

/// What [`read_inside`] found at a project path. `real` is the real path:
/// relative to the folder when inside it, absolute when outside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inside {
    /// Missing, a dangling link, or not a regular file.
    Absent,
    /// Not read, and why.
    Refused { real: PathBuf, why: Refusal },
    /// The content of a regular file inside the folder.
    Bytes { real: PathBuf, bytes: Vec<u8> },
}

/// `name` in `dir`, read as the fingerprint, the prompt and the config read it. Refused unless
/// its real path is inside `dir`, not `.env*` nor under `.git`, with one hard link and at most
/// 1 MiB; opened one component at a time, none a link. Not a regular file (a FIFO): absent.
pub fn read_inside(dir: &Path, name: impl AsRef<Path>) -> std::io::Result<Inside> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let root = match std::fs::canonicalize(dir) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Inside::Absent),
        Err(e) => return Err(e),
    };
    let resolved = match std::fs::canonicalize(root.join(name)) {
        Ok(p) => p,
        // Nothing there, a component that is a file, a link loop: no file.
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) =>
        {
            return Ok(Inside::Absent)
        }
        Err(e) => return Err(e),
    };
    let real = match resolved.strip_prefix(&root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
        _ => {
            return Ok(Inside::Refused {
                real: resolved,
                why: Refusal::Outside,
            })
        }
    };
    let refused = |why| {
        Ok(Inside::Refused {
            real: real.clone(),
            why,
        })
    };
    if is_secret(&real) {
        return refused(Refusal::Secret);
    }
    let file = open_beneath(&root, &real).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR | libc::EMLINK) => std::io::Error::other(format!(
            "{} changed while it was opened",
            resolved.display()
        )),
        _ => e,
    })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(Inside::Absent);
    }
    if meta.nlink() > 1 {
        return refused(Refusal::HardLink);
    }
    if meta.len() > MAX_PROJECT_FILE_BYTES {
        return refused(Refusal::TooLarge);
    }
    // A file that grows after the check is still cut at the bound + 1.
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_PROJECT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROJECT_FILE_BYTES {
        return refused(Refusal::TooLarge);
    }
    Ok(Inside::Bytes { real, bytes })
}

/// A `.env*` component or a `.git` one: the folder's own secrets, never
/// project instructions, whatever links to them.
fn is_secret(real: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    real.components().any(|c| {
        let c = c.as_os_str().as_bytes();
        c == b".git" || c.starts_with(b".env")
    })
}

/// Open `rel`, canonical and inside `root`, one component at a time with `O_NOFOLLOW`: a
/// component swapped for a link after the check fails the open. No `/proc`, Linux and macOS
/// alike; the last component gets `O_NONBLOCK`, so a FIFO cannot hang it.
fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidInput);
    let parts = rel
        .components()
        .map(|c| match c {
            Component::Normal(p) => Ok(p),
            _ => Err(invalid()),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let (last, parents) = parts.split_last().ok_or_else(invalid)?;
    let mut dir: OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?
        .into();
    for part in parents {
        dir = openat(&dir, part, libc::O_DIRECTORY)?;
    }
    Ok(openat(&dir, last, libc::O_NONBLOCK)?.into())
}

/// `openat(dir, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | flags)`.
fn openat(
    dir: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | flags;
    // SAFETY: `dir` is an open descriptor and `name` NUL-terminated.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned open by openat and is owned by no one.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// [`read_inside`] as text: `None` unless it is a regular file inside `dir`
/// that is read, and valid UTF-8.
pub fn read_inside_text(dir: &Path, name: impl AsRef<Path>) -> Option<String> {
    match read_inside(dir, name) {
        Ok(Inside::Bytes { bytes, .. }) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

/// File names under `dir` whose path passes `keep`, sorted; none if unreadable.
fn sorted_entries(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| keep(&e.path()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// [`discover`], plus, for a privileged directory, the runtime policy its `[bridge]
/// runtime_config` names: editing it must ask again. Both read by [`read_inside`]; a
/// policy it refuses, or outside `dir`, is an error.
pub fn discover_for(dir: &Path, privileged: bool) -> Result<Vec<FileDigest>> {
    let mut found = discover(dir)?;
    if !privileged {
        return Ok(found);
    }
    // Refused or absent: listed by `discover`, never read, nothing to bind.
    let Ok(Inside::Bytes { bytes, .. }) = read_inside(dir, PROJECT_FILES[0]) else {
        return Ok(found);
    };
    // Unparseable: the config load refuses it later; nothing more to bind.
    let Some(project) = String::from_utf8(bytes)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
    else {
        return Ok(found);
    };
    if let Some((raw, path)) = runtime_policy_of(dir, &project)? {
        let sha256 = match read_inside(dir, &path)
            .with_context(|| format!("cannot read {}", path.display()))?
        {
            Inside::Bytes { bytes, .. } => hex(&Sha256::digest(&bytes)),
            Inside::Absent => "absent".to_string(),
            Inside::Refused { real, why } => {
                anyhow::bail!(
                    "bridge.runtime_config = {raw:?}: {}, {why}: refused",
                    real.display()
                )
            }
        };
        found.push(FileDigest {
            name: format!("{raw} (bridge.runtime_config)"),
            sha256,
            target: None,
        });
    }
    Ok(found)
}

/// The `[bridge] runtime_config` of a project config, resolved against `dir`
/// (symlinks followed), or an error if it leaves `dir`.
pub fn runtime_policy_of(dir: &Path, project: &toml::Value) -> Result<Option<(String, PathBuf)>> {
    let Some(raw) = project
        .get("bridge")
        .and_then(|b| b.get("runtime_config"))
        .and_then(|v| v.as_str())
    else {
        return Ok(None);
    };
    let root =
        std::fs::canonicalize(dir).with_context(|| format!("cannot resolve {}", dir.display()))?;
    let joined = root.join(raw);
    let resolved = match std::fs::canonicalize(&joined) {
        Ok(p) => p,
        // Not there yet: judged lexically; its creation changes the digest.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => lexical(&joined),
        Err(e) => return Err(e).with_context(|| format!("cannot resolve {}", joined.display())),
    };
    if resolved == root || !resolved.starts_with(&root) {
        anyhow::bail!(
            "bridge.runtime_config = {raw:?} resolves to {}, outside {}: refused",
            resolved.display(),
            root.display()
        );
    }
    Ok(Some((raw.to_string(), resolved)))
}

fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// [`status`] with the privilege given, for tests and callers that know it.
pub fn status_for(dir: &Path, store: &Path, privileged: bool) -> Result<Status> {
    Ok(assess(dir, store, privileged)?.0)
}

/// The status, and the files it was taken on.
fn assess(dir: &Path, store: &Path, privileged: bool) -> Result<(Status, Vec<FileDigest>)> {
    let files = discover_for(dir, privileged)?;
    if files.is_empty() {
        return Ok((Status::NoProjectFiles, files));
    }
    let key = canonical(dir);
    let store = load_store(store)?;
    let status = match store.projects.iter().find(|e| e.dir == key) {
        Some(e) if e.files == files => Status::Trusted,
        Some(_) => Status::Changed(files.clone()),
        None => Status::Unknown(files.clone()),
    };
    Ok((status, files))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default, rename = "project")]
    projects: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    dir: String,
    files: Vec<FileDigest>,
    trusted_at: String,
}

/// `~/.x0ne/trusted-projects.toml`. Distinct from `[trust] roots`, which lifts
/// per-action approval inside a directory; this file only says whose config
/// may be read at all.
pub fn default_store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join("trusted-projects.toml")
}

fn load_store(store: &Path) -> Result<Store> {
    match std::fs::read_to_string(store) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("cannot parse {}", store.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", store.display())),
    }
}

fn canonical(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Where `dir` stands with respect to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No project file: nothing to settle.
    NoProjectFiles,
    /// Trusted, and the files are byte-for-byte those that were trusted.
    Trusted,
    /// Never trusted.
    Unknown(Vec<FileDigest>),
    /// Trusted once, but a file was added, removed or edited since.
    Changed(Vec<FileDigest>),
}

/// Privilege is read from the USER config; an unreadable one privileges nothing.
pub fn status(dir: &Path, store: &Path) -> Result<Status> {
    status_for(dir, store, privileged_now(dir))
}

fn privileged_now(dir: &Path) -> bool {
    crate::config::Config::find_and_load_user()
        .map(|c| is_privileged(dir, &c.project_trust.privileged))
        .unwrap_or(false)
}

/// Record `files` of `dir` as trusted, replacing any earlier entry.
pub fn record(dir: &Path, files: &[FileDigest], store: &Path) -> Result<()> {
    let key = canonical(dir);
    let mut s = load_store(store)?;
    s.projects.retain(|e| e.dir != key);
    s.projects.push(Entry {
        dir: key,
        files: files.to_vec(),
        trusted_at: chrono::Utc::now().to_rfc3339(),
    });
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(&s).context("cannot serialize the trust store")?;
    // Written whole then renamed: a crash never leaves half a store behind.
    let tmp = store.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, store).with_context(|| format!("cannot write {}", store.display()))?;
    Ok(())
}

/// Take this process's decision for `dir`, once. `ask` is called only when a
/// human must answer and `interactive` is true; its `bool` is the answer. A
/// non-interactive run never trusts a directory it has not trusted before.
/// Returns the decision. Later calls return the first decision unchanged.
pub fn decide(
    dir: &Path,
    store: &Path,
    interactive: bool,
    ask: impl FnOnce(&Path, &Status) -> bool,
) -> bool {
    if let Some(d) = DECISION.get() {
        return d.trusted;
    }
    let approved = match assess(dir, store, privileged_now(dir)) {
        Ok((Status::NoProjectFiles, _)) => None,
        Ok((Status::Trusted, files)) => Some(files),
        Ok((st @ (Status::Unknown(_) | Status::Changed(_)), files)) => {
            if interactive && ask(dir, &st) {
                // Trust that cannot be remembered is still granted for this run.
                if let Err(e) = record(dir, &files, store) {
                    tracing::warn!("could not remember trust for {}: {e:#}", dir.display());
                }
                Some(files)
            } else {
                None
            }
        }
        // An unreadable store or project file fails closed.
        Err(e) => {
            tracing::warn!("workspace trust check failed, project config ignored: {e:#}");
            None
        }
    };
    let trusted = approved.is_some();
    DECISION
        .get_or_init(|| Decision {
            trusted,
            approved: approved.and_then(|files| Approved::new(dir, files)),
        })
        .trusted
}

/// The trust question on the plain terminal, in the style of the agent's own
/// confirmations (`agent/cli_sink.rs`): default no, and EOF or an unreadable
/// answer is a no.
pub fn ask_on_terminal(dir: &Path, status: &Status) -> bool {
    use colored::Colorize;
    use std::io::Write;
    let lines = match status {
        Status::Unknown(f) | Status::Changed(f) => question_files(f),
        _ => return false,
    };
    let privileged = crate::config::Config::find_and_load_user()
        .map(|c| is_privileged(dir, &c.project_trust.privileged))
        .unwrap_or(false);
    let dir = crate::text_safety::sanitize(&dir.display().to_string());
    let headline = match status {
        Status::Changed(_) => {
            "The x0ne config or instructions of this folder changed since you trusted it."
        }
        _ => "This folder carries x0ne config or instructions for the model.",
    };
    println!("\n  {} {}", "?".bright_cyan().bold(), headline.bold());
    println!("    {} {}", "folder ".bright_black(), dir);
    for line in &lines {
        match line {
            QuestionLine::Refused(t) => println!("    {} {}", "refused".yellow().bold(), t),
            QuestionLine::Read(t) => println!("    {} {}", "file   ".bright_black(), t),
            QuestionLine::More(t) => println!("    {} {}", "       ".bright_black(), t),
        }
    }
    let scope = if privileged {
        "Your config lists this folder under [project_trust] privileged: trusting \
         it applies its WHOLE config, including the runtime policy, approvals \
         and budgets. Only the model stays yours."
    } else {
        "Trusting it applies its display and conversation settings only, and \
         reads its AGENTS.md, X0NE.md, rules and skills into the model's prompt. \
         The program x0ne starts, its sandbox, approvals, budgets and model \
         always come from your own config."
    };
    println!("    {}", scope.bright_black());
    print!("  {} ", "Trust this folder? [y/N] ❯".bright_black().bold());
    let _ = std::io::stdout().flush();
    let _echo = crate::input_queue::EchoRestorer::new();
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(
            buf.trim().to_lowercase().as_str(),
            "y" | "yes" | "o" | "oui"
        ),
    }
}

/// At most this many file lines on the trust question, links and refusals aside: past it,
/// the regular files fold into one "and N other files" line. A link or a refusal is never
/// folded; [`MAX_TRUST_FILES`] bounds them.
const MAX_QUESTION_FILES: usize = 8;
/// At most this many characters per file line: 120 wraps once on 80 columns.
const MAX_QUESTION_CHARS: usize = 120;

/// One file line of the trust question.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QuestionLine {
    /// Never read: the name, where it leads, and why.
    Refused(String),
    /// Read once trusted: the name, and where it leads if a link.
    Read(String),
    /// The regular files the bound folded.
    More(String),
}

/// The question's lines: refusals with target and reason, links trusting would read with
/// their target, then regular files, each cut by [`fit`]; only regular files fold. Measured
/// 2026-10-28: the fold hid a link to `.npmrc` that a yes then read.
fn question_files(files: &[FileDigest]) -> Vec<QuestionLine> {
    let mut lines = Vec::new();
    let mut links = Vec::new();
    let mut plain = Vec::new();
    for f in files {
        let target = f.target.as_deref();
        match (f.sha256.strip_prefix(REFUSED), target) {
            (Some(why), _) => lines.push(QuestionLine::Refused(fit(
                &f.name,
                target,
                &format!(": {why}, never read"),
            ))),
            (None, Some(_)) => links.push(QuestionLine::Read(fit(&f.name, target, ""))),
            (None, None) => plain.push(QuestionLine::Read(fit(&f.name, None, ""))),
        }
    }
    if links.is_empty() && plain.is_empty() {
        lines.push(QuestionLine::Read("none".to_string()));
    }
    lines.extend(links);
    let room = MAX_QUESTION_FILES.saturating_sub(lines.len());
    if plain.len() > room {
        let folded = plain.split_off(room.saturating_sub(1));
        plain.push(QuestionLine::More(format!(
            "and {} other files of this folder, none a link",
            folded.len()
        )));
    }
    lines.extend(plain);
    lines
}

/// `name -> target` and `verdict` in [`MAX_QUESTION_CHARS`], terminal controls blanked. The
/// name and the target are cut apart, each in its middle, so the arrow always shows. Measured
/// 2026-10-28: one cut in the middle of the line removed the arrow of a long link.
fn fit(name: &str, target: Option<&str>, verdict: &str) -> String {
    let name = crate::text_safety::sanitize(name);
    let room = MAX_QUESTION_CHARS.saturating_sub(verdict.chars().count());
    let Some(target) = target.map(crate::text_safety::sanitize) else {
        return format!("{}{verdict}", cut_middle(&name, room));
    };
    let room = room.saturating_sub(4);
    let (n, t) = (name.chars().count(), target.chars().count());
    let name_room = n.min((room / 2).max(room.saturating_sub(t)));
    format!(
        "{} -> {}{verdict}",
        cut_middle(&name, name_room),
        cut_middle(&target, room - name_room)
    )
}

/// `s` in `max` characters: its head, `…`, and a tail twice as long as the head.
fn cut_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 3;
    let head: String = s.chars().take(head).collect();
    let tail: String = s.chars().skip(n - (keep - head.chars().count())).collect();
    format!("{head}…{tail}")
}

/// Keep, from a parsed project config, only what [`PROJECT_SETTABLE`] allows,
/// and apply it over `base` (the user's config). Returns the dotted keys that
/// were present in the project file and refused.
pub fn overlay_project_config(base: &mut toml::Value, project: &toml::Value) -> Vec<String> {
    let mut refused = Vec::new();
    let (Some(base), Some(project)) = (base.as_table_mut(), project.as_table()) else {
        return refused;
    };
    for (table, value) in project {
        let whole = PROJECT_SETTABLE
            .iter()
            .any(|(t, k)| t == table && k.is_none());
        if whole {
            base.insert(table.clone(), value.clone());
            continue;
        }
        let Some(fields) = value.as_table() else {
            refused.push(table.clone());
            continue;
        };
        for (key, v) in fields {
            let allowed = PROJECT_SETTABLE
                .iter()
                .any(|(t, k)| t == table && *k == Some(key.as_str()));
            if allowed {
                let entry = base
                    .entry(table.clone())
                    .or_insert_with(|| toml::Value::Table(Default::default()));
                if let Some(t) = entry.as_table_mut() {
                    t.insert(key.clone(), v.clone());
                }
            } else {
                refused.push(format!("{table}.{key}"));
            }
        }
    }
    refused
}

/// Whether `dir` is one of the user's privileged directories
/// (`[project_trust] privileged` in the USER config, never a project's).
pub fn is_privileged(dir: &Path, privileged: &[String]) -> bool {
    is_privileged_in(dir, privileged, dirs::home_dir().as_deref())
}

/// [`is_privileged`] with the home directory given, for tests.
/// An entry must be absolute or start with `~/`: a relative one would follow
/// the working directory, so `.` would privilege every folder.
fn is_privileged_in(dir: &Path, privileged: &[String], home: Option<&Path>) -> bool {
    let Ok(dir) = std::fs::canonicalize(dir) else {
        return false;
    };
    privileged.iter().any(|raw| {
        let path = match raw.strip_prefix("~/") {
            Some(rest) => match home {
                Some(h) => h.join(rest),
                None => return false,
            },
            None => PathBuf::from(raw),
        };
        path.is_absolute() && std::fs::canonicalize(&path).is_ok_and(|p| p == dir)
    })
}

/// Tables a privileged project still cannot set: its own privilege (it is read
/// from the user config only), the model, which was always the user's, and
/// hooks, commands x0ne runs that come from the user's config only.
pub const PRIVILEGED_REFUSED: &[&str] = &["project_trust", "llm", "hooks"];

/// Apply a privileged project's config over `base` in full, key by key, except
/// [`PRIVILEGED_REFUSED`]. Returns the refused tables present in the project.
pub fn overlay_privileged_project_config(
    base: &mut toml::Value,
    project: &toml::Value,
) -> Vec<String> {
    let mut refused = Vec::new();
    let (Some(base), Some(project)) = (base.as_table_mut(), project.as_table()) else {
        return refused;
    };
    for (key, value) in project {
        if PRIVILEGED_REFUSED.contains(&key.as_str()) {
            refused.push(key.clone());
            continue;
        }
        match base.get_mut(key) {
            Some(existing) => deep_merge(existing, value),
            None => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
    refused
}

fn deep_merge(base: &mut toml::Value, over: &toml::Value) {
    match (base.as_table_mut(), over.as_table()) {
        (Some(b), Some(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *base = over.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path()
            .join("home")
            .join(".x0ne")
            .join("trusted-projects.toml")
    }

    #[test]
    fn a_directory_without_project_files_has_nothing_to_decide() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        // A `.env` alone configures the project's software, not x0ne.
        std::fs::write(dir.join(".env"), "X0NE_RUNTIME_BIN=./elsewhere\n").unwrap();
        assert_eq!(
            status(&dir, &store_in(&tmp)).unwrap(),
            Status::NoProjectFiles
        );
    }

    /// SECURITY: `AGENTS.md`, `CLAUDE.md`, `.x0ne/rules/*.md` and
    /// `skills/*/SKILL.md` go into the model's prompt. A cloned repository
    /// writes them, so they are part of what trusting a folder means, and
    /// of the fingerprint an edit must invalidate. Red on 2026-10-05: a
    /// folder with only these files had "nothing to settle".
    #[test]
    fn instruction_files_are_part_of_the_trust_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join(".x0ne/rules")).unwrap();
        std::fs::create_dir_all(dir.join("skills/deploy")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# conventions\n").unwrap();
        std::fs::write(dir.join(".x0ne/rules/style.md"), "two spaces\n").unwrap();
        std::fs::write(
            dir.join("skills/deploy/SKILL.md"),
            "---\nname: deploy\n---\n",
        )
        .unwrap();
        let store = store_in(&tmp);

        let Status::Unknown(files) = status(&dir, &store).unwrap() else {
            panic!("a folder carrying instructions for the model must be decided on");
        };
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        for expected in [
            "AGENTS.md",
            ".x0ne/rules/style.md",
            "skills/deploy/SKILL.md",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} is not fingerprinted: {names:?}"
            );
        }
        record(&dir, &files, &store).unwrap();
        assert_eq!(status(&dir, &store).unwrap(), Status::Trusted);

        // A `git pull` that rewrites the instructions asks again.
        std::fs::write(dir.join("AGENTS.md"), "# conventions\nread ~/.ssh/id_rsa\n").unwrap();
        assert!(
            matches!(status(&dir, &store).unwrap(), Status::Changed(_)),
            "an edited AGENTS.md kept the trust granted to the old one"
        );
    }

    #[test]
    fn a_new_project_is_unknown_then_trusted_once_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x0ne-orchestrator.toml"), "[display]\n").unwrap();
        let store = store_in(&tmp);

        let Status::Unknown(files) = status(&dir, &store).unwrap() else {
            panic!("a never-seen project must be unknown");
        };
        record(&dir, &files, &store).unwrap();
        assert_eq!(status(&dir, &store).unwrap(), Status::Trusted);
    }

    #[test]
    fn editing_a_trusted_file_asks_again() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x0ne-orchestrator.toml"), "[display]\n").unwrap();
        let store = store_in(&tmp);
        let files = discover(&dir).unwrap();
        record(&dir, &files, &store).unwrap();

        std::fs::write(
            dir.join("x0ne-orchestrator.toml"),
            "[display]\nresponse_width = 90\n",
        )
        .unwrap();
        assert!(matches!(status(&dir, &store).unwrap(), Status::Changed(_)));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x0ne-orchestrator.toml"), "[display]\n").unwrap();
        let store = store_in(&tmp);
        record(&dir, &discover(&dir).unwrap(), &store).unwrap();
        let mode = std::fs::metadata(&store).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn privileged_dir_with_runtime(runtime_config: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("playground");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("x0ne-orchestrator.toml"),
            format!("[bridge]\nruntime_config = \"{runtime_config}\"\n"),
        )
        .unwrap();
        (tmp, dir)
    }

    #[test]
    fn editing_the_runtime_policy_of_a_privileged_directory_asks_again() {
        let (tmp, dir) = privileged_dir_with_runtime("x0ne-runtime.toml");
        std::fs::write(dir.join("x0ne-runtime.toml"), "[policy]\n").unwrap();
        let store = store_in(&tmp);
        let Status::Unknown(files) = status_for(&dir, &store, true).unwrap() else {
            panic!("a never-seen project must be unknown");
        };
        record(&dir, &files, &store).unwrap();
        assert_eq!(status_for(&dir, &store, true).unwrap(), Status::Trusted);

        // The agent rewrites its own policy; the project TOML is untouched.
        std::fs::write(
            dir.join("x0ne-runtime.toml"),
            "[policy]\nallowed_commands = [\"sh\"]\n",
        )
        .unwrap();
        assert!(matches!(
            status_for(&dir, &store, true).unwrap(),
            Status::Changed(_)
        ));
    }

    #[test]
    fn creating_a_missing_runtime_policy_asks_again() {
        let (tmp, dir) = privileged_dir_with_runtime("x0ne-runtime.toml");
        let store = store_in(&tmp);
        record(&dir, &discover_for(&dir, true).unwrap(), &store).unwrap();
        std::fs::write(dir.join("x0ne-runtime.toml"), "[policy]\n").unwrap();
        assert!(matches!(
            status_for(&dir, &store, true).unwrap(),
            Status::Changed(_)
        ));
    }

    #[test]
    fn a_runtime_policy_outside_the_privileged_directory_is_refused() {
        let (tmp, dir) = privileged_dir_with_runtime("../outside.toml");
        std::fs::write(tmp.path().join("outside.toml"), "[policy]\n").unwrap();
        assert!(status_for(&dir, &store_in(&tmp), true).is_err());

        let (tmp, dir) = privileged_dir_with_runtime("/etc/hostname");
        assert!(status_for(&dir, &store_in(&tmp), true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_runtime_policy_symlinked_outside_is_refused() {
        let (tmp, dir) = privileged_dir_with_runtime("x0ne-runtime.toml");
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "[policy]\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("x0ne-runtime.toml")).unwrap();
        assert!(status_for(&dir, &store_in(&tmp), true).is_err());
    }

    /// A clone ships `AGENTS.md -> ../outside/secret.md`, a `SKILL.md` that
    /// links out, a skill directory that links out, a rules directory that
    /// links out. Red on 2026-10-27: each target was read and fingerprinted,
    /// then read into the prompt once the folder was trusted.
    #[cfg(unix)]
    #[test]
    fn a_project_file_linked_out_of_the_folder_is_listed_never_read() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("skilldir")).unwrap();
        std::fs::create_dir_all(outside.join("rules")).unwrap();
        std::fs::write(outside.join("secret.md"), "FAKE-SECRET\n").unwrap();
        std::fs::write(outside.join("skilldir/SKILL.md"), "FAKE-SECRET\n").unwrap();
        std::fs::write(outside.join("rules/r.md"), "FAKE-SECRET\n").unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join("skills/leak")).unwrap();
        std::fs::create_dir_all(dir.join(".x0ne/skills")).unwrap();
        symlink("../outside/secret.md", dir.join("AGENTS.md")).unwrap();
        symlink(outside.join("secret.md"), dir.join("skills/leak/SKILL.md")).unwrap();
        symlink(
            "../../../outside/skilldir",
            dir.join(".x0ne/skills/dirleak"),
        )
        .unwrap();
        symlink("../../outside/rules", dir.join(".x0ne/rules")).unwrap();

        let files = discover(&dir).unwrap();
        let secret = hex(&Sha256::digest(b"FAKE-SECRET\n"));
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "AGENTS.md",
                ".x0ne/rules/r.md",
                "skills/leak/SKILL.md",
                ".x0ne/skills/dirleak/SKILL.md"
            ],
            "a link out of the folder must still be listed"
        );
        let real = std::fs::canonicalize(&outside).unwrap();
        for f in &files {
            assert_ne!(f.sha256, secret, "{} was read", f.name);
            assert_eq!(f.sha256, format!("{REFUSED}{}", Refusal::Outside), "{f:?}");
            let target = f.target.as_deref().expect("the target is kept");
            assert!(Path::new(target).starts_with(&real), "{}: {target}", f.name);
        }
        for f in &files {
            assert_eq!(
                read_inside(&dir, &f.name).unwrap(),
                Inside::Refused {
                    real: PathBuf::from(f.target.as_deref().unwrap()),
                    why: Refusal::Outside
                },
                "the prompt reads {} differently from the fingerprint",
                f.name
            );
            assert_eq!(read_inside_text(&dir, &f.name), None);
        }
    }

    /// The same rule as `runtime_config`: a link that stays inside the
    /// folder is followed and fingerprinted by its content.
    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_folder_is_read() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join(".x0ne/rules")).unwrap();
        std::fs::write(dir.join("docs/shared.md"), "shared rule\n").unwrap();
        symlink("docs/shared.md", dir.join("AGENTS.md")).unwrap();
        symlink("../../docs/shared.md", dir.join(".x0ne/rules/shared.md")).unwrap();
        // Out, then back in: judged on the real path, which is inside.
        symlink("../repo/docs/shared.md", dir.join("CLAUDE.md")).unwrap();

        let content = hex(&Sha256::digest(b"shared rule\n"));
        let files = discover(&dir).unwrap();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(files.iter().all(|f| f.sha256 == content), "{files:?}");
        assert_eq!(
            read_inside_text(&dir, "AGENTS.md").as_deref(),
            Some("shared rule\n")
        );
    }

    /// Real path against real folder, component by component: a folder
    /// opened through a link is itself, and `repoEVIL` is not inside `repo`.
    #[cfg(unix)]
    #[test]
    fn the_real_path_is_compared_to_the_real_folder() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let evil = tmp.path().join("repoEVIL");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(dir.join("X0NE.md"), "notes\n").unwrap();
        std::fs::write(evil.join("secret.md"), "FAKE-SECRET\n").unwrap();
        symlink("../repoEVIL/secret.md", dir.join("AGENTS.md")).unwrap();
        let alias = tmp.path().join("alias");
        symlink(&dir, &alias).unwrap();

        for d in [&dir, &alias] {
            let files = discover(d).unwrap();
            let agents = files.iter().find(|f| f.name == "AGENTS.md").unwrap();
            assert!(agents.sha256.starts_with(REFUSED), "{agents:?}");
            assert_eq!(read_inside_text(d, "X0NE.md").as_deref(), Some("notes\n"));
        }
        // The folder itself is not a file inside it.
        assert!(matches!(
            read_inside(&dir, ".").unwrap(),
            Inside::Refused {
                why: Refusal::Outside,
                ..
            }
        ));
    }

    /// A FIFO named `AGENTS.md` is not a config file, and must not hang
    /// the trust check on its open.
    #[cfg(unix)]
    #[test]
    fn a_project_path_that_is_not_a_regular_file_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join(".x0ne/rules/dir.md")).unwrap();
        let fifo = std::ffi::CString::new(dir.join("AGENTS.md").to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo only creates a node.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink("AGENTS.md", dir.join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink("X0NE.md", dir.join("X0NE.md")).unwrap();

        assert_eq!(discover(&dir).unwrap(), Vec::new());
        assert_eq!(read_inside(&dir, "AGENTS.md").unwrap(), Inside::Absent);
        assert_eq!(read_inside(&dir, "X0NE.md").unwrap(), Inside::Absent);
    }

    /// Trusted while the link stayed inside; turned outward later, the
    /// folder asks again and the target is never read.
    #[cfg(unix)]
    #[test]
    fn a_link_turned_out_of_the_folder_after_trust_asks_again() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/a.md"), "inside\n").unwrap();
        std::fs::write(tmp.path().join("secret.md"), "inside\n").unwrap();
        symlink("docs/a.md", dir.join("AGENTS.md")).unwrap();
        let store = store_in(&tmp);
        record(&dir, &discover(&dir).unwrap(), &store).unwrap();
        assert_eq!(status(&dir, &store).unwrap(), Status::Trusted);

        // Same bytes outside: the digest still changes, by where it leads.
        std::fs::remove_file(dir.join("AGENTS.md")).unwrap();
        symlink("../secret.md", dir.join("AGENTS.md")).unwrap();
        assert!(matches!(status(&dir, &store).unwrap(), Status::Changed(_)));
        assert_eq!(read_inside_text(&dir, "AGENTS.md"), None);
    }

    /// The question names each link out and where it leads, and lists it
    /// apart from the files trusting would read. Red on 2026-10-27: the
    /// question showed `AGENTS.md` and nothing else.
    #[cfg(unix)]
    #[test]
    fn the_trust_question_shows_where_a_refused_link_leads() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        // A target name that tries to drive the terminal.
        let target = tmp.path().join("id\u{1b}[2J.md");
        std::fs::write(&target, "FAKE-SECRET\n").unwrap();
        symlink(&target, dir.join("AGENTS.md")).unwrap();
        std::fs::write(dir.join("X0NE.md"), "notes\n").unwrap();

        let lines = question_files(&discover(&dir).unwrap());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1], QuestionLine::Read("X0NE.md".into()));
        let QuestionLine::Refused(line) = &lines[0] else {
            panic!("{lines:?}")
        };
        assert!(line.starts_with("AGENTS.md -> "), "{line}");
        assert!(
            line.ends_with(": outside this folder, never read"),
            "{line}"
        );
        assert!(
            line.contains(&*tmp.path().canonicalize().unwrap().to_string_lossy()),
            "the target is not shown: {line}"
        );
        assert!(!line.contains('\u{1b}'), "a control reached the terminal");

        let lines = question_files(&[FileDigest {
            name: "AGENTS.md".into(),
            sha256: format!("{REFUSED}{}", Refusal::Outside),
            target: Some("/x".into()),
        }]);
        assert_eq!(lines[1], QuestionLine::Read("none".into()));
    }

    #[test]
    fn an_unprivileged_directory_is_not_fingerprinted_by_its_runtime_policy() {
        // The key is refused by the overlay anyway (PROJECT_SETTABLE).
        let (tmp, dir) = privileged_dir_with_runtime("../outside.toml");
        assert!(matches!(
            status_for(&dir, &store_in(&tmp), false).unwrap(),
            Status::Unknown(f) if f.len() == 1
        ));
    }

    #[test]
    fn a_privileged_entry_expands_the_tilde_and_resolves_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let pg = home.join("labs").join("playground");
        std::fs::create_dir_all(&pg).unwrap();
        assert!(is_privileged_in(
            &pg,
            &["~/labs/playground".into()],
            Some(&home)
        ));
        // Through `..` and a symlink: the same directory, so the same answer.
        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&pg, &link).unwrap();
            let entry = format!("{}/../playground", link.display());
            assert!(is_privileged_in(&link, &[entry], Some(&home)));
        }
    }

    #[test]
    fn privilege_is_the_listed_directory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let pg = tmp.path().join("playground");
        let sub = pg.join("01-failing-test");
        std::fs::create_dir_all(&sub).unwrap();
        let listed = vec![pg.display().to_string()];
        assert!(is_privileged_in(&pg, &listed, None));
        assert!(!is_privileged_in(&sub, &listed, None), "not inherited");
        assert!(!is_privileged_in(tmp.path(), &listed, None), "nor upward");
    }

    #[test]
    fn a_relative_privileged_entry_privileges_nothing() {
        // `.` would otherwise mean « whatever folder x0ne starts in ».
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        for raw in [".", "playground", "./x", ""] {
            assert!(!is_privileged_in(&cwd, &[raw.into()], Some(tmp.path())));
        }
    }

    #[test]
    fn a_privileged_project_config_applies_in_full_except_privilege_and_model() {
        let mut base: toml::Value = toml::from_str(
            r#"
            [llm]
            provider = "anthropic"
            [ui]
            confirm_writes = true
            compact_prompt = false
            [project_trust]
            privileged = ["/home/u/playground"]
            "#,
        )
        .unwrap();
        let project: toml::Value = toml::from_str(
            r#"
            [bridge]
            runtime_config = "x0ne-runtime.toml"
            [trust]
            roots = ["."]
            [cost]
            max_consecutive_actions = 0
            [ui]
            compact_prompt = true
            [llm]
            provider = "openai-compat"
            [project_trust]
            privileged = ["/"]
            "#,
        )
        .unwrap();

        let refused = overlay_privileged_project_config(&mut base, &project);

        assert_eq!(
            base["bridge"]["runtime_config"].as_str(),
            Some("x0ne-runtime.toml")
        );
        assert_eq!(base["trust"]["roots"][0].as_str(), Some("."));
        assert_eq!(
            base["cost"]["max_consecutive_actions"].as_integer(),
            Some(0)
        );
        // Merged key by key: what the project does not say stays the user's.
        assert_eq!(base["ui"]["compact_prompt"].as_bool(), Some(true));
        assert_eq!(base["ui"]["confirm_writes"].as_bool(), Some(true));
        assert_eq!(base["llm"]["provider"].as_str(), Some("anthropic"));
        assert_eq!(
            base["project_trust"]["privileged"][0].as_str(),
            Some("/home/u/playground")
        );
        assert!(refused.iter().any(|r| r == "project_trust"));
        assert!(refused.iter().any(|r| r == "llm"));
    }

    #[test]
    fn a_project_config_cannot_set_what_removes_a_safeguard() {
        let mut base: toml::Value = toml::from_str(
            r#"
            [bridge]
            x0ne_core_bin = "x0ne-runtime"
            [ui]
            confirm_writes = true
            [display]
            response_width = 80
            "#,
        )
        .unwrap();
        let project: toml::Value = toml::from_str(
            r#"
            [bridge]
            x0ne_core_bin = "./elsewhere"
            [trust]
            roots = ["/"]
            [cost]
            budget_usd_limit = 1e9
            [llm]
            provider = "openai-compat"
            [ui]
            confirm_writes = false
            auto_confirm = true
            compact_prompt = true
            [display]
            response_width = 120
            "#,
        )
        .unwrap();

        let refused = overlay_project_config(&mut base, &project);

        assert_eq!(
            base["bridge"]["x0ne_core_bin"].as_str(),
            Some("x0ne-runtime")
        );
        assert!(base.get("trust").is_none());
        assert!(base.get("cost").is_none());
        assert!(base.get("llm").is_none());
        assert_eq!(base["ui"]["confirm_writes"].as_bool(), Some(true));
        assert!(base["ui"].get("auto_confirm").is_none());
        // What a project may set, it sets.
        assert_eq!(base["ui"]["compact_prompt"].as_bool(), Some(true));
        assert_eq!(base["display"]["response_width"].as_integer(), Some(120));
        for k in [
            "bridge.x0ne_core_bin",
            "trust.roots",
            "cost.budget_usd_limit",
            "llm.provider",
            "ui.confirm_writes",
            "ui.auto_confirm",
        ] {
            assert!(
                refused.iter().any(|r| r == k),
                "{k} must be reported as refused"
            );
        }
    }

    // ── Follow-ups to read_inside ────────────────────────────────────────

    /// The bytes [`read_inside`] hands over, if it read the file.
    fn read_bytes(dir: &Path, name: &str) -> Option<Vec<u8>> {
        match read_inside(dir, name) {
            Ok(Inside::Bytes { bytes, .. }) => Some(bytes),
            _ => None,
        }
    }

    /// The lines the trust question prints about the files.
    fn question_text(files: &[FileDigest]) -> Vec<String> {
        question_files(files)
            .into_iter()
            .map(|l| match l {
                QuestionLine::Refused(t) | QuestionLine::Read(t) | QuestionLine::More(t) => t,
            })
            .collect()
    }

    /// Red on 2026-10-27: `X0NE.md -> .env` stayed inside the folder, so it
    /// was read, and the question named `X0NE.md` alone. The module promises
    /// `./.env` is never read; `.git/config` can hold a token.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_dotenv_or_git_file_is_refused_and_every_target_shown() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".x0ne/rules")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join(".env.local"), "KEY=FAKE-ENV-SECRET\n").unwrap();
        std::fs::write(dir.join(".git/config"), "url = https://u:FAKE-TOKEN@h/r\n").unwrap();
        std::fs::write(dir.join("docs/shared.md"), "shared\n").unwrap();
        symlink(".env.local", dir.join("X0NE.md")).unwrap();
        symlink("../../.git/config", dir.join(".x0ne/rules/git.md")).unwrap();
        symlink("docs/shared.md", dir.join("CLAUDE.md")).unwrap();

        assert_eq!(read_bytes(&dir, "X0NE.md"), None);
        assert_eq!(read_bytes(&dir, ".x0ne/rules/git.md"), None);
        assert_eq!(
            read_bytes(&dir, "CLAUDE.md").as_deref(),
            Some(&b"shared\n"[..])
        );
        let lines = question_text(&discover(&dir).unwrap());
        for (link, target) in [
            ("X0NE.md", ".env.local"),
            (".x0ne/rules/git.md", ".git/config"),
        ] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("{link} -> {target}")) && l.contains("never read")),
                "{link}: not refused with its target: {lines:#?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|l| l.contains("CLAUDE.md -> docs/shared.md")),
            "a link inside is not shown with its target: {lines:#?}"
        );
    }

    /// Red on 2026-10-27: a hard link has no link to resolve, and the file
    /// it shares with a path outside the folder was read.
    #[cfg(unix)]
    #[test]
    fn a_hard_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(tmp.path().join("id_ed25519"), "FAKE-PRIVATE-KEY\n").unwrap();
        std::fs::hard_link(tmp.path().join("id_ed25519"), dir.join("AGENTS.md")).unwrap();

        assert_eq!(read_bytes(&dir, "AGENTS.md"), None);
        let files = discover(&dir).unwrap();
        let secret = hex(&Sha256::digest(b"FAKE-PRIVATE-KEY\n"));
        assert!(files.iter().all(|f| f.sha256 != secret), "{files:?}");
    }

    /// Red on 2026-10-27: a project file was read whole, whatever its size,
    /// before the question. 1 MiB is the bound; the size is taken from the
    /// descriptor.
    #[test]
    fn a_project_file_over_the_size_bound_is_not_read() {
        const LIMIT: u64 = MAX_PROJECT_FILE_BYTES;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let f = std::fs::File::create(dir.join("AGENTS.md")).unwrap();
        f.set_len(LIMIT + 1).unwrap();
        std::fs::File::create(dir.join("X0NE.md"))
            .unwrap()
            .set_len(LIMIT)
            .unwrap();

        assert_eq!(
            read_bytes(&dir, "AGENTS.md").map(|b| b.len()),
            None,
            "over the bound"
        );
        assert_eq!(
            read_bytes(&dir, "X0NE.md").map(|b| b.len() as u64),
            Some(LIMIT)
        );
        let files = discover(&dir).unwrap();
        let agents = files
            .iter()
            .find(|f| f.name == "AGENTS.md")
            .expect("listed");
        assert_ne!(agents.sha256.len(), 64, "hashed: {agents:?}");
    }

    /// Red on 2026-10-27: for a privileged folder, `discover_for` read
    /// `x0ne-orchestrator.toml` with `read_to_string`, through a link out of
    /// the folder, and bound the runtime policy the outside file named.
    #[cfg(unix)]
    #[test]
    fn a_privileged_project_config_linked_out_is_not_read_for_its_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("playground");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            tmp.path().join("other.toml"),
            "[bridge]\nruntime_config = \"x0ne-runtime.toml\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("x0ne-runtime.toml"), "[policy]\n").unwrap();
        std::os::unix::fs::symlink("../other.toml", dir.join("x0ne-orchestrator.toml")).unwrap();

        let files = discover_for(&dir, true).unwrap();
        assert!(
            files
                .iter()
                .all(|f| !f.name.contains("bridge.runtime_config")),
            "the outside file was read: {files:?}"
        );
    }

    /// Red on 2026-10-27: a FIFO named `x0ne-orchestrator.toml` in a
    /// privileged folder hung `discover_for` on its read.
    #[cfg(unix)]
    #[test]
    fn a_privileged_project_config_that_is_a_fifo_does_not_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("playground");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "notes\n").unwrap();
        let fifo = dir.join("x0ne-orchestrator.toml");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo only creates a node.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let d = dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(discover_for(&d, true).map(|f| f.len()).ok());
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        if got.is_err() {
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        }
        assert!(got.is_ok(), "discover_for hung on a FIFO");
    }

    /// Red on 2026-10-27: the question listed every file on one line, unbounded. Every line is
    /// now cut at 120 characters and the regular files fold past 8 lines; every refusal is
    /// still shown, [`MAX_TRUST_FILES`] bounding them.
    #[test]
    fn the_trust_question_is_bounded() {
        let mut files: Vec<FileDigest> = (0..300)
            .map(|i| FileDigest {
                name: format!(".x0ne/rules/{i:03}-{}.md", "x".repeat(200)),
                sha256: "0".repeat(64),
                target: None,
            })
            .collect();
        files.extend((0..40).map(|i| FileDigest {
            name: format!("skills/s{i}/SKILL.md"),
            sha256: format!("{REFUSED}{}", Refusal::Outside),
            target: Some(format!("/elsewhere/{}", "y".repeat(300))),
        }));
        let lines = question_files(&files);
        let refused = lines
            .iter()
            .filter(|l| matches!(l, QuestionLine::Refused(_)))
            .count();
        assert_eq!(refused, 40, "{lines:#?}");
        assert_eq!(
            lines.last(),
            Some(&QuestionLine::More(
                "and 300 other files of this folder, none a link".into()
            ))
        );
        for l in question_text(&files) {
            assert!(
                l.chars().count() <= MAX_QUESTION_CHARS,
                "{} chars: {l}",
                l.chars().count()
            );
        }
        let few = &files[..20];
        assert_eq!(question_files(few).len(), MAX_QUESTION_FILES, "{few:#?}");
    }

    /// Red with /proc hidden, on 2026-10-27: a parent directory swapped for
    /// a link between the check and the open let the file outside be read,
    /// since the only check on the parents read `/proc/self/fd`.
    #[cfg(unix)]
    #[test]
    fn a_parent_swapped_for_a_link_is_never_read_through() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let out = tmp.path().join("outside");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(dir.join("sub/AGENTS.md"), "inside\n").unwrap();
        std::fs::write(out.join("AGENTS.md"), "FAKE-SECRET\n").unwrap();
        std::os::unix::fs::symlink(&out, dir.join("link")).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let swapper = {
            let (stop, d) = (stop.clone(), dir.clone());
            std::thread::spawn(move || {
                let (sub, real, link) = (d.join("sub"), d.join("real"), d.join("link"));
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::rename(&sub, &real);
                    let _ = std::fs::rename(&link, &sub);
                    let _ = std::fs::rename(&sub, &link);
                    let _ = std::fs::rename(&real, &sub);
                }
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (mut inside, mut leaked) = (0, 0);
        while std::time::Instant::now() < deadline {
            match read_bytes(&dir, "sub/AGENTS.md").as_deref() {
                Some(b"inside\n") => inside += 1,
                Some(_) => leaked += 1,
                None => {}
            }
        }
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(
            leaked, 0,
            "read through the swapped link {leaked} times ({inside} reads inside)"
        );
        assert!(inside > 0, "never read inside: the test proves nothing");
    }

    /// Red on 2026-10-27, end to end (`x0ne-code/tests`): the files were
    /// fingerprinted before the question and read again after it. What is
    /// read now must hash to what was approved, else it is not handed over.
    #[cfg(unix)]
    #[test]
    fn only_the_approved_bytes_are_handed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(dir.join(".x0ne/rules")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "approved\n").unwrap();
        std::fs::write(dir.join(".x0ne/rules/a.md"), "approved rule\n").unwrap();
        std::fs::write(dir.join("x0ne-orchestrator.toml"), "[display]\n").unwrap();
        let approved = Approved::new(&dir, discover(&dir).unwrap()).unwrap();
        assert_eq!(
            approved.read(&dir, "AGENTS.md").as_deref(),
            Some(&b"approved\n"[..])
        );

        std::fs::write(dir.join(".x0ne/rules/a.md"), "edited rule\n").unwrap();
        std::fs::write(dir.join("x0ne-orchestrator.toml"), "[display]\nx = 1\n").unwrap();
        std::fs::write(dir.join(".x0ne/rules/b.md"), "added rule\n").unwrap();
        assert_eq!(approved.read(&dir, ".x0ne/rules/a.md"), None, "edited");
        assert_eq!(
            approved.read(&dir, "x0ne-orchestrator.toml"),
            None,
            "edited"
        );
        assert_eq!(approved.read(&dir, ".x0ne/rules/b.md"), None, "added");
        assert_eq!(
            approved.read(tmp.path(), "AGENTS.md"),
            None,
            "another folder"
        );
        assert_eq!(
            approved.read(&dir, "AGENTS.md").as_deref(),
            Some(&b"approved\n"[..])
        );

        // Same bytes, but now through a link: the target is part of it.
        std::fs::rename(dir.join("AGENTS.md"), dir.join("moved.md")).unwrap();
        std::os::unix::fs::symlink("moved.md", dir.join("AGENTS.md")).unwrap();
        assert_eq!(approved.read(&dir, "AGENTS.md"), None, "relinked");
    }

    // ── What the question shows, and the work before it ─────────────────

    /// Red on 2026-10-28: the eighth line onward was folded into "and N other
    /// files", a link among them, which a yes then read.
    #[test]
    fn a_link_or_a_refusal_is_never_folded_out_of_the_question() {
        let plain = |name: String| FileDigest {
            name,
            sha256: "0".repeat(64),
            target: None,
        };
        let mut files: Vec<FileDigest> = (1..=12)
            .map(|i| plain(format!(".x0ne/rules/a{i:02}.md")))
            .collect();
        files.push(FileDigest {
            name: ".x0ne/rules/z99.md".into(),
            sha256: "1".repeat(64),
            target: Some("config/.npmrc".into()),
        });
        for i in 0..10 {
            files.push(FileDigest {
                name: format!("skills/s{i}/SKILL.md"),
                sha256: format!("{REFUSED}{}", Refusal::Outside),
                target: Some(format!("/elsewhere/s{i}")),
            });
        }
        let lines = question_text(&files);
        assert!(
            lines
                .iter()
                .any(|l| l == ".x0ne/rules/z99.md -> config/.npmrc"),
            "{lines:#?}"
        );
        for i in 0..10 {
            let shown = format!("skills/s{i}/SKILL.md -> /elsewhere/s{i}");
            assert!(
                lines.iter().any(|l| l.starts_with(&shown)),
                "{shown}: {lines:#?}"
            );
        }
    }

    /// Red on 2026-10-28: a line over the bound was cut in its middle, and
    /// the cut could take the arrow that says the file is a link.
    #[test]
    fn a_long_link_line_keeps_its_arrow() {
        let name = format!("skills/{}/SKILL.md", "n".repeat(90));
        let target = format!("vendor/{}credentials.md", "cache/".repeat(30));
        for sha256 in ["0".repeat(64), format!("{REFUSED}{}", Refusal::Outside)] {
            let lines = question_text(&[FileDigest {
                name: name.clone(),
                sha256: sha256.clone(),
                target: Some(target.clone()),
            }]);
            let line = &lines[0];
            assert!(line.contains(" -> "), "{line}");
            assert!(line.starts_with("skills/"), "{line}");
            assert!(line.contains("credentials.md"), "{line}");
            assert!(line.chars().count() <= MAX_QUESTION_CHARS, "{line}");
            if sha256.starts_with(REFUSED) {
                assert!(
                    line.ends_with(": outside this folder, never read"),
                    "{line}"
                );
            }
        }
    }

    /// Red on 2026-10-28: 4096 links to one 1 MiB file inside the folder made
    /// the check hash 4 GiB before the question could be shown.
    #[cfg(unix)]
    #[test]
    fn the_work_before_the_question_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("trusted.toml");
        let dir = tmp.path().join("big");
        std::fs::create_dir_all(dir.join(".x0ne/rules")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/big.md"), vec![b'a'; 1_048_576]).unwrap();
        for i in 0..20 {
            std::os::unix::fs::symlink(
                "../../docs/big.md",
                dir.join(format!(".x0ne/rules/{i:03}.md")),
            )
            .unwrap();
        }
        let many = tmp.path().join("many");
        std::fs::create_dir_all(many.join(".x0ne/rules")).unwrap();
        for i in 0..300 {
            std::fs::write(many.join(format!(".x0ne/rules/{i:03}.md")), "r\n").unwrap();
        }
        let mut wrong = Vec::new();
        for (what, d) in [("20 MiB of rules", &dir), ("300 rules", &many)] {
            match status_for(d, &store, false) {
                Err(e) => eprintln!("MEASURE: {what}: {e:#}"),
                Ok(st) => wrong.push(format!("{what}: {:?}", std::mem::discriminant(&st))),
            }
        }
        assert!(
            wrong.is_empty(),
            "not refused before the question: {wrong:#?}"
        );
    }
}
