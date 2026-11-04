use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::{expand_tilde, AppConfig};
use crate::result::{ActionOutput, ActionResult, ErrorKind, RestoreDetail, SnapshotMeta};

// ── Reflink-aware copy ────────────────────────────────────────────────────────
//
// Reflink (btrfs, XFS reflink=1, APFS) avoids copying a file's bytes; the
// walk is still one copy per file, and a restore is not one atomic swap
// (HA-08). Other filesystems fall back to a byte copy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// CoW reflink: the file's blocks are shared, not copied.
    Reflink,
    /// Byte copy fallback — used when FS doesn't support reflinks.
    Copy,
}

/// Try to clone via reflink; fall back to a regular byte copy on any failure.
/// Returns the method actually used so callers can surface it to users.
#[cfg(test)]
fn try_reflink_or_copy(src: &Path, dst: &Path) -> std::io::Result<CopyMethod> {
    let f = std::fs::File::open(src)?;
    copy_from_open(&f, src, dst)
}

/// Copy from an already-open source descriptor: reflink first, byte copy
/// otherwise. Restore uses this so the file that was validated (opened with
/// `O_NOFOLLOW`, checked regular) is the file that gets copied.
fn copy_from_open(src: &std::fs::File, src_path: &Path, dst: &Path) -> std::io::Result<CopyMethod> {
    if reflink_copy_open(src, src_path, dst).is_ok() {
        return Ok(CopyMethod::Reflink);
    }
    let _ = std::fs::remove_file(dst);
    let mut src = src;
    let mut dst_f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    std::io::copy(&mut src, &mut dst_f)?;
    Ok(CopyMethod::Copy)
}

#[cfg(target_os = "linux")]
fn reflink_copy_open(src_f: &std::fs::File, _src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // FICLONE = _IOW(0x94, 9, int). `libc::Ioctl` is c_ulong on glibc and
    // c_int on musl; a fixed c_ulong broke the musl release build (2026-10-05).
    const FICLONE: libc::Ioctl = 0x4004_9409;
    let dst_f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let rc = unsafe {
        libc::ioctl(
            dst_f.as_raw_fd(),
            FICLONE,
            src_f.as_raw_fd() as libc::c_ulong,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn reflink_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let src_f = std::fs::File::open(src)?;
    reflink_copy_open(&src_f, src, dst)
}

/// clonefile is path-based; CLONE_NOFOLLOW already refuses a symlink there.
#[cfg(not(target_os = "linux"))]
fn reflink_copy_open(_src_f: &std::fs::File, src: &Path, dst: &Path) -> std::io::Result<()> {
    reflink_copy(src, dst)
}

#[cfg(target_os = "macos")]
fn reflink_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "src has NUL"))?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "dst has NUL"))?;
    // CLONE_NOFOLLOW = 0x0001 — refuse symlinks, same as we want.
    let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0x0001) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reflink_copy(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reflink not implemented on this platform",
    ))
}

// ── Manifest ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotManifest {
    name: String,
    created_at: String,
    files: Vec<ManifestEntry>,
    /// The original input paths the user passed to CreateSnapshot. For
    /// directory roots, restore-time cleanup walks the directory and
    /// removes any file that exists today but wasn't in the snapshot
    /// (i.e. files the agent added after the snapshot was taken).
    /// Empty / missing in legacy manifests → restore falls back to the
    /// pre-recursion behaviour (no cleanup).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    roots: Vec<String>,
    /// Copy method used at create time: "reflink", "copy", or "mixed".
    /// Optional for backward compatibility with manifests written before
    /// reflink support landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    original_path: String,
    snapshot_file: String, // e.g. "0.dat"
    size_bytes: u64,
    /// Permission bits (0o777) at snapshot time. Absent in older manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Validate a snapshot name: alphanumeric, hyphens, underscores, max 64 chars.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "Snapshot name must be 1–64 characters, got {}",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "Snapshot name must contain only alphanumeric characters, hyphens, and underscores."
                .into(),
        );
    }
    Ok(())
}

fn snapshot_root(config: &AppConfig) -> PathBuf {
    expand_tilde(&config.snapshots.dir)
}

/// Map a snapshot name to its directory, refusing anything that is not a
/// plain name.
///
/// Validation lives here rather than at each call site because this is the one
/// place a model-controlled string becomes a path. `validate_name` previously
/// had a single caller — `create_snapshot` — so `restore`, `delete` and every
/// lookup took the name verbatim into a bare `join`. `join` with an absolute
/// path replaces the base outright, and `delete` ends in `remove_dir_all`.
fn snapshot_dir(config: &AppConfig, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    Ok(snapshot_root(config).join(name))
}

fn manifest_path(snap_dir: &Path) -> PathBuf {
    snap_dir.join("manifest.json")
}

/// Map a manifest `snapshot_file` to its path, refusing anything that is not
/// a single plain component. `join` with an absolute path replaces the base,
/// and `..` or a separator walks out of the snapshot dir.
fn backup_path(snap_dir: &Path, snapshot_file: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let mut comps = Path::new(snapshot_file).components();
    let plain = matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    );
    if !plain || snapshot_file.contains(['/', '\\', '\0']) {
        return Err(format!(
            "manifest entry has an invalid snapshot_file {:?}: must be a plain file name",
            snapshot_file
        ));
    }
    Ok(snap_dir.join(snapshot_file))
}

/// Open a backup for reading without following symlinks, and require a
/// regular file. A link planted under a valid name must not reach outside.
fn open_backup(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "backup is a symlink",
        ));
    }
    let f = opts.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "backup is not a regular file",
        ));
    }
    Ok(f)
}

fn load_manifest(snap_dir: &Path) -> Result<SnapshotManifest> {
    let content = std::fs::read_to_string(manifest_path(snap_dir))
        .context("Failed to read snapshot manifest")?;
    serde_json::from_str(&content).context("Failed to parse snapshot manifest")
}

// ── Public API (called by executor/mod.rs and pipeline.rs) ────────────────────

/// Create a snapshot with the given name and file paths.
/// Returns the canonical snapshot name on success.
pub async fn create_snapshot(name: &str, paths: &[String], config: &AppConfig) -> Result<String> {
    validate_name(name).map_err(|e| anyhow::anyhow!(e))?;

    // Check snapshot count limit
    let root = snapshot_root(config);
    std::fs::create_dir_all(&root).context("Failed to create snapshot root dir")?;
    let existing = count_snapshots(&root);
    if existing >= config.snapshots.max_snapshots {
        anyhow::bail!(
            "Maximum snapshots reached ({}/{}). Delete some first.",
            existing,
            config.snapshots.max_snapshots
        );
    }

    let snap_dir = snapshot_dir(config, name).map_err(|e| anyhow::anyhow!(e))?;
    if snap_dir.exists() {
        anyhow::bail!("Snapshot '{}' already exists.", name);
    }
    std::fs::create_dir_all(&snap_dir).context("Failed to create snapshot directory")?;
    // A half-written snapshot (size limit, unreadable file) used to stay on
    // disk: it counted against max_snapshots and its name could not be reused.
    let filled = fill_snapshot(name, paths, config, &snap_dir);
    if filled.is_err() {
        let _ = std::fs::remove_dir_all(&snap_dir);
    }
    filled
}

fn fill_snapshot(
    name: &str,
    paths: &[String],
    config: &AppConfig,
    snap_dir: &Path,
) -> Result<String> {
    let max_bytes = config.snapshots.max_snapshot_size_mb * 1024 * 1024;
    let mut total_bytes: u64 = 0;
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut reflink_count = 0usize;
    let mut copy_count = 0usize;
    let mut next_id: usize = 0;

    // Snapshot one file: assigns the next .dat name, copies (reflink-aware),
    // updates the running counters, and pushes a manifest entry.
    let snapshot_one = |src: &Path,
                        entries: &mut Vec<ManifestEntry>,
                        total_bytes: &mut u64,
                        reflink_count: &mut usize,
                        copy_count: &mut usize,
                        next_id: &mut usize|
     -> Result<()> {
        let meta =
            std::fs::metadata(src).with_context(|| format!("Cannot stat {}", src.display()))?;
        let size = meta.len();
        // The cap bounds bytes COPIED; a clone shares blocks and is not
        // counted. 0 still refuses every snapshot (mcp-readonly profile).
        if max_bytes == 0 {
            anyhow::bail!("Snapshot size limit exceeded (0 MB).");
        }
        let dest_name = format!("{}.dat", *next_id);
        *next_id += 1;
        let dest = snap_dir.join(&dest_name);
        let method = if reflink_copy(src, &dest).is_ok() {
            CopyMethod::Reflink
        } else {
            let _ = std::fs::remove_file(&dest);
            *total_bytes += size;
            if *total_bytes > max_bytes {
                anyhow::bail!(
                    "Snapshot size limit exceeded ({} MB copied; reflink unavailable).",
                    config.snapshots.max_snapshot_size_mb
                );
            }
            std::fs::copy(src, &dest)
                .with_context(|| format!("Failed to copy {} to snapshot", src.display()))?;
            CopyMethod::Copy
        };
        // HA-03: a reflinked backup got the umask mode, 0644 measured 2026-10-05.
        set_mode(&dest, 0o600).with_context(|| format!("Failed to restrict {}", dest.display()))?;
        match method {
            CopyMethod::Reflink => *reflink_count += 1,
            CopyMethod::Copy => *copy_count += 1,
        }
        entries.push(ManifestEntry {
            original_path: src
                .canonicalize()
                .unwrap_or_else(|_| src.to_path_buf())
                .to_string_lossy()
                .into_owned(),
            snapshot_file: dest_name,
            size_bytes: size,
            mode: mode_of(&meta),
        });
        Ok(())
    };

    for path_str in paths.iter() {
        let src = PathBuf::from(path_str);
        if !src.exists() {
            // Skip non-existent paths silently (they may not need snapshotting).
            continue;
        }
        let canonical_root = src
            .canonicalize()
            .unwrap_or_else(|_| src.clone())
            .to_string_lossy()
            .into_owned();
        let meta =
            std::fs::metadata(&src).with_context(|| format!("Cannot stat {}", src.display()))?;
        if meta.is_file() {
            roots.push(canonical_root);
            snapshot_one(
                &src,
                &mut entries,
                &mut total_bytes,
                &mut reflink_count,
                &mut copy_count,
                &mut next_id,
            )?;
        } else if meta.is_dir() {
            roots.push(canonical_root);
            // Recurse, files only, no symlink-following (avoid escapes & loops).
            for entry in walkdir::WalkDir::new(&src).follow_links(false) {
                let entry = entry.with_context(|| format!("Failed to walk {}", src.display()))?;
                if entry.file_type().is_file() {
                    snapshot_one(
                        entry.path(),
                        &mut entries,
                        &mut total_bytes,
                        &mut reflink_count,
                        &mut copy_count,
                        &mut next_id,
                    )?;
                }
            }
        }
        // symlinks / other types: silently skipped — same as before.
    }

    let method = match (reflink_count, copy_count) {
        (0, 0) => None,
        (_, 0) => Some("reflink".to_string()),
        (0, _) => Some("copy".to_string()),
        _ => Some("mixed".to_string()),
    };
    let manifest = SnapshotManifest {
        name: name.to_string(),
        created_at: Utc::now().to_rfc3339(),
        files: entries,
        roots,
        method,
    };
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("Failed to serialize manifest")?;
    std::fs::write(manifest_path(snap_dir), manifest_json).context("Failed to write manifest")?;

    Ok(name.to_string())
}

fn count_snapshots(root: &Path) -> usize {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count()
        })
        .unwrap_or(0)
}

// ── Action handlers ───────────────────────────────────────────────────────────

pub async fn action_create_snapshot(
    name: &str,
    paths: &[String],
    config: &AppConfig,
) -> ActionResult {
    match create_snapshot(name, paths, config).await {
        Ok(_) => {
            // `create_snapshot` already validated the name, so this cannot fail.
            let snap_dir = match snapshot_dir(config, name) {
                Ok(d) => d,
                Err(e) => {
                    return ActionResult::err("CreateSnapshot", ErrorKind::InvalidInput, e, 0)
                }
            };
            let manifest = match load_manifest(&snap_dir) {
                Ok(m) => m,
                Err(e) => {
                    return ActionResult::err(
                        "CreateSnapshot",
                        ErrorKind::SnapshotError,
                        e.to_string(),
                        0,
                    );
                }
            };
            let total_bytes: u64 = manifest.files.iter().map(|f| f.size_bytes).sum();
            ActionResult::ok(
                "CreateSnapshot",
                ActionOutput::SnapshotCreated {
                    name: name.to_string(),
                    files_count: manifest.files.len(),
                    total_bytes,
                    created_at: manifest.created_at,
                    method: manifest.method,
                    scope: Some("filesystem".to_string()),
                },
                0,
            )
        }
        Err(e) => ActionResult::err("CreateSnapshot", ErrorKind::SnapshotError, e.to_string(), 0),
    }
}

/// Is `raw` inside one of `roots`, even if it does not exist yet?
///
/// Restore legitimately recreates deleted files, so the target may be absent.
/// We therefore resolve the nearest EXISTING ancestor (which cannot be forged,
/// since canonicalization follows real symlinks), re-attach the remaining
/// components, and test containment on the result. `..` anywhere in the
/// not-yet-existing tail is refused outright rather than normalized away.
///
/// An empty `roots` denies everything, matching the executor-wide rule that an
/// empty allowlist means deny-all.
fn path_is_confined(raw: &str, roots: &[PathBuf]) -> bool {
    use std::path::Component;
    let p = Path::new(raw);
    if !p.is_absolute() {
        return false; // a relative manifest path has no well-defined location
    }
    let mut existing = p;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let canonical_base = loop {
        match std::fs::canonicalize(existing) {
            Ok(c) => break c,
            Err(_) => match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name);
                    existing = parent;
                }
                // Walked past the root without finding anything real.
                _ => return false,
            },
        }
    };
    // Reject traversal in the unresolved tail; the resolved base is already safe.
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let mut candidate = canonical_base;
    for name in tail.iter().rev() {
        candidate.push(name);
    }
    crate::executor::confine::is_within_roots(&candidate, roots)
}

pub async fn action_restore_snapshot(name: &str, config: &AppConfig) -> ActionResult {
    let snap_dir = match snapshot_dir(config, name) {
        Ok(d) => d,
        Err(e) => return ActionResult::err("RestoreSnapshot", ErrorKind::InvalidInput, e, 0),
    };
    if !snap_dir.exists() {
        return ActionResult::err(
            "RestoreSnapshot",
            ErrorKind::NotFound,
            format!("Snapshot '{}' not found.", name),
            0,
        );
    }
    let manifest = match load_manifest(&snap_dir) {
        Ok(m) => m,
        Err(e) => {
            return ActionResult::err(
                "RestoreSnapshot",
                ErrorKind::SnapshotError,
                e.to_string(),
                0,
            )
        }
    };

    // SECURITY: a manifest is untrusted input even though it lives under
    // ~/.x0ne. An agent that can write there can forge `original_path` (to
    // overwrite anything) and `roots` (to have the cleanup pass recursively
    // delete anything). Validate EVERY path against the write policy up front
    // and refuse the whole restore if any of them escapes — a partial restore
    // of a tampered manifest is not a safe outcome.
    {
        let write_roots =
            crate::executor::confine::normalize_roots(&config.policy.allowed_write_paths);
        let mut offending: Vec<String> = Vec::new();
        for entry in &manifest.files {
            if !path_is_confined(&entry.original_path, &write_roots) {
                offending.push(entry.original_path.clone());
            }
        }
        for root in &manifest.roots {
            if !path_is_confined(root, &write_roots) {
                offending.push(root.clone());
            }
        }
        if !offending.is_empty() {
            offending.truncate(5);
            return ActionResult::err(
                "RestoreSnapshot",
                ErrorKind::PolicyDenied,
                format!(
                    "snapshot '{}' refers to path(s) outside allowed_write_paths and was \
                     refused: {}. A snapshot manifest is not trusted just because it lives \
                     in the X0NE storage directory.",
                    name,
                    offending.join(", ")
                ),
                0,
            );
        }
    }

    // Every backup name must be a plain file inside the snapshot dir,
    // checked before anything is touched, same rule as original_path above.
    let mut backups: Vec<PathBuf> = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        match backup_path(&snap_dir, &entry.snapshot_file) {
            Ok(p) => backups.push(p),
            Err(e) => {
                return ActionResult::err(
                    "RestoreSnapshot",
                    ErrorKind::PolicyDenied,
                    format!("snapshot '{}' was refused: {}", name, e),
                    0,
                )
            }
        }
    }

    // HA-02: every backup must exist before anything is touched.
    let missing: Vec<String> = manifest
        .files
        .iter()
        .zip(&backups)
        .filter(|(_, p)| {
            !std::fs::symlink_metadata(p)
                .map(|m| m.is_file())
                .unwrap_or(false)
        })
        .map(|(e, _)| e.original_path.clone())
        .collect();
    if !missing.is_empty() {
        return ActionResult::err(
            "RestoreSnapshot",
            ErrorKind::SnapshotError,
            format!(
                "snapshot '{}' is incomplete, nothing was restored; missing backup for: {}",
                name,
                missing.join(", ")
            ),
            0,
        );
    }

    let mut details = Vec::new();
    let mut files_restored = 0usize;
    let mut files_failed = 0usize;
    let mut reflink_count = 0usize;
    let mut copy_count = 0usize;

    for (i, (entry, src)) in manifest.files.iter().zip(&backups).enumerate() {
        let dest = PathBuf::from(&entry.original_path);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    files_failed += 1;
                    details.push(RestoreDetail {
                        path: entry.original_path.clone(),
                        status: format!("failed: could not recreate parent dir: {}", e),
                    });
                    continue;
                }
            }
        }
        // HA-02: copy beside the target, then rename over it. The current
        // file stays intact until the replacement is complete.
        let mode = entry
            .mode
            .or_else(|| std::fs::metadata(&dest).ok().as_ref().and_then(mode_of));
        let tmp = dest.with_file_name(format!(
            ".{}.x0ne-restore-{}-{}",
            dest.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            std::process::id(),
            i
        ));
        let _ = std::fs::remove_file(&tmp);
        let step = open_backup(src)
            .and_then(|f| copy_from_open(&f, src, &tmp))
            .and_then(|method| {
                if let Some(m) = mode {
                    set_mode(&tmp, m)?;
                }
                std::fs::rename(&tmp, &dest)?;
                Ok(method)
            });
        match step {
            Ok(method) => {
                files_restored += 1;
                match method {
                    CopyMethod::Reflink => reflink_count += 1,
                    CopyMethod::Copy => copy_count += 1,
                }
                details.push(RestoreDetail {
                    path: entry.original_path.clone(),
                    status: "restored".into(),
                });
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                files_failed += 1;
                details.push(RestoreDetail {
                    path: entry.original_path.clone(),
                    status: format!("failed, current file left in place: {}", e),
                });
            }
        }
    }

    // ── Atomic-restore cleanup ────────────────────────────────────────────
    //
    // For each directory root that was snapshotted, remove any file that
    // exists today but was NOT in the manifest — i.e. files the agent
    // added between snapshot and restore. Without this pass, restore is
    // "rewrite these files", not "rollback to this state".
    //
    // Legacy manifests (pre-recursion, no `roots` field) skip this pass —
    // their behaviour is unchanged and they remain restorable.
    let mut files_removed = 0usize;
    let snapshot_set: std::collections::HashSet<PathBuf> = manifest
        .files
        .iter()
        .map(|e| PathBuf::from(&e.original_path))
        .collect();
    // HA-02: no destructive cleanup on top of a failed restore.
    let cleanup_roots: &[String] = if files_failed == 0 {
        &manifest.roots
    } else {
        &[]
    };
    for root in cleanup_roots {
        let root_path = PathBuf::from(root);
        let Ok(meta) = std::fs::metadata(&root_path) else {
            continue;
        };
        if !meta.is_dir() {
            continue; // single-file roots have nothing to clean up
        }
        for entry in walkdir::WalkDir::new(&root_path).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let abs = entry
                .path()
                .canonicalize()
                .unwrap_or_else(|_| entry.path().to_path_buf());
            if !snapshot_set.contains(&abs) && std::fs::remove_file(entry.path()).is_ok() {
                files_removed += 1;
                details.push(RestoreDetail {
                    path: entry.path().to_string_lossy().into_owned(),
                    status: "removed (added after snapshot)".into(),
                });
            }
        }
    }

    let method = match (reflink_count, copy_count) {
        (0, 0) => None,
        (_, 0) => Some("reflink".to_string()),
        (0, _) => Some("copy".to_string()),
        _ => Some("mixed".to_string()),
    };

    let output = ActionOutput::SnapshotRestored {
        name: name.to_string(),
        files_restored,
        files_failed,
        files_removed,
        details,
        method,
        scope: Some("filesystem".to_string()),
    };
    if files_failed == 0 {
        return ActionResult::ok("RestoreSnapshot", output, 0);
    }
    let mut r = ActionResult::err(
        "RestoreSnapshot",
        ErrorKind::SnapshotError,
        format!(
            "partial restore of '{name}': {files_restored} restored, {files_failed} failed; \
             files added since the snapshot were kept"
        ),
        0,
    );
    r.output = Some(output);
    r
}

pub async fn action_list_snapshots(config: &AppConfig) -> ActionResult {
    let root = snapshot_root(config);
    if !root.exists() {
        return ActionResult::ok(
            "ListSnapshots",
            ActionOutput::SnapshotList { snapshots: vec![] },
            0,
        );
    }
    let mut snapshots: Vec<SnapshotMeta> = Vec::new();
    let read = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(e) => return ActionResult::err("ListSnapshots", ErrorKind::Internal, e.to_string(), 0),
    };
    for entry in read.filter_map(|e| e.ok()) {
        if entry.path().is_dir() {
            if let Ok(manifest) = load_manifest(&entry.path()) {
                let total: u64 = manifest.files.iter().map(|f| f.size_bytes).sum();
                snapshots.push(SnapshotMeta {
                    name: manifest.name,
                    files_count: manifest.files.len(),
                    total_bytes: total,
                    created_at: manifest.created_at,
                });
            }
        }
    }
    snapshots.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    ActionResult::ok("ListSnapshots", ActionOutput::SnapshotList { snapshots }, 0)
}

pub async fn action_delete_snapshot(name: &str, config: &AppConfig) -> ActionResult {
    let snap_dir = match snapshot_dir(config, name) {
        Ok(d) => d,
        Err(e) => return ActionResult::err("DeleteSnapshot", ErrorKind::InvalidInput, e, 0),
    };
    if !snap_dir.exists() {
        return ActionResult::err(
            "DeleteSnapshot",
            ErrorKind::NotFound,
            format!("Snapshot '{}' not found.", name),
            0,
        );
    }
    match std::fs::remove_dir_all(&snap_dir) {
        Ok(_) => ActionResult::ok(
            "DeleteSnapshot",
            ActionOutput::SnapshotDeleted {
                name: name.to_string(),
                deleted: true,
            },
            0,
        ),
        Err(e) => ActionResult::err("DeleteSnapshot", ErrorKind::SnapshotError, e.to_string(), 0),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn test_config(temp: &TempDir) -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.snapshots.dir = temp.path().to_string_lossy().into_owned();
        // Restore now validates every manifest path against the write roots,
        // and an empty allowlist means DENY ALL. These tests snapshot/restore
        // files under the system temp dir, so declare it. The forged-manifest
        // test overrides this with a narrower root on purpose.
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        cfg.policy.allowed_write_paths = vec![tmp.clone()];
        cfg.policy.allowed_read_paths = vec![tmp];
        cfg
    }

    /// SECURITY regression (August 2026 audit, finding 5). `RestoreSnapshot`
    /// took `entry.original_path` and `manifest.roots` verbatim from an
    /// on-disk manifest. An agent able to write under the snapshots directory
    /// could forge one and get:
    ///   * arbitrary file OVERWRITE  (restore writes to original_path), and
    ///   * arbitrary recursive DELETE (cleanup removes everything under
    ///     roots[] that is not in the snapshot).
    /// Living in ~/.x0ne does not make a manifest trustworthy.
    #[tokio::test]
    async fn restore_refuses_manifest_paths_outside_write_roots() {
        let temp_snap = TempDir::new().unwrap();
        let victim_dir = TempDir::new().unwrap();
        let allowed_dir = TempDir::new().unwrap();

        let mut cfg = test_config(&temp_snap);
        // Only `allowed_dir` may be written. The manifest will point elsewhere.
        cfg.policy.allowed_write_paths = vec![allowed_dir.path().to_string_lossy().into_owned()];
        cfg.policy.allowed_read_paths = vec![allowed_dir.path().to_string_lossy().into_owned()];

        let victim = victim_dir.path().join("precious.txt");
        std::fs::write(&victim, b"ORIGINAL").unwrap();
        let bystander = victim_dir.path().join("bystander.txt");
        std::fs::write(&bystander, b"KEEP ME").unwrap();

        // Forge a snapshot directory by hand.
        let snap_dir = snapshot_dir(&cfg, "evil").unwrap();
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("payload"), b"PWNED").unwrap();
        let manifest = serde_json::json!({
            "name": "evil",
            "created_at": "2026-08-08T00:00:00Z",
            "files": [{
                "original_path": victim.to_string_lossy(),
                "snapshot_file": "payload",
                "size_bytes": 5
            }],
            // Cleanup pass would walk this and delete everything not snapshotted.
            "roots": [victim_dir.path().to_string_lossy()],
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let res = action_restore_snapshot("evil", &cfg).await;

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL",
            "manifest original_path outside the write roots must NOT be overwritten"
        );
        assert!(
            bystander.exists(),
            "cleanup must not delete files under a manifest-supplied root outside the write roots"
        );
        assert!(
            !res.success,
            "a manifest pointing outside the write roots must be refused, not silently partially applied"
        );
    }

    /// A model-controlled snapshot name maps to a filesystem path. Only
    /// `create` validated it, so `restore` and `delete` accepted traversal and
    /// absolute paths — and `delete` ends in `remove_dir_all`.
    #[tokio::test]
    async fn restore_and_delete_reject_traversal_names() {
        let cfg = AppConfig::defaults();
        for name in [
            "../../../etc",
            "/etc/cron.d",
            "..",
            "a/../../b",
            "with space",
            "semi;colon",
        ] {
            let r = action_restore_snapshot(name, &cfg).await;
            assert!(!r.success, "restore accepted {name:?}");
            let d = action_delete_snapshot(name, &cfg).await;
            assert!(!d.success, "delete accepted {name:?}");
        }
    }

    /// Forge a snapshot whose single entry restores `backup` (as named in the
    /// manifest) over `target`. Returns the config and the snapshot dir.
    fn forge_snapshot(
        temp_snap: &TempDir,
        allowed_dir: &TempDir,
        target: &Path,
        backup: &str,
    ) -> (AppConfig, PathBuf) {
        let mut cfg = test_config(temp_snap);
        cfg.policy.allowed_write_paths = vec![allowed_dir.path().to_string_lossy().into_owned()];
        cfg.policy.allowed_read_paths = vec![allowed_dir.path().to_string_lossy().into_owned()];
        let snap_dir = snapshot_dir(&cfg, "forged").unwrap();
        std::fs::create_dir_all(&snap_dir).unwrap();
        let manifest = serde_json::json!({
            "name": "forged",
            "created_at": "2026-10-13T00:00:00Z",
            "files": [{
                "original_path": target.to_string_lossy(),
                "snapshot_file": backup,
                "size_bytes": 6
            }],
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        (cfg, snap_dir)
    }

    /// `snapshot_file` was joined onto the snapshot dir unchecked. An
    /// absolute path, `..`, or a separator made restore COPY a file from
    /// outside the snapshot into a path inside the write roots, where ReadFile
    /// then serves it. A manifest is not trusted for its backup names either.
    #[tokio::test]
    async fn restore_refuses_snapshot_file_that_is_not_a_plain_name() {
        let temp_snap = TempDir::new().unwrap();
        let allowed_dir = TempDir::new().unwrap();
        let secret_dir = TempDir::new().unwrap();
        let secret = secret_dir.path().join("secret.txt");
        std::fs::write(&secret, b"SECRET").unwrap();
        let target = allowed_dir.path().join("exfil.txt");

        // Every case names a file that really exists, so a missing-file error
        // cannot mask a missing check.
        let abs = secret.to_string_lossy().into_owned();
        let secret_name = secret_dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // snap_dir is <temp_snap>/forged; secret_dir is a sibling of temp_snap.
        let dotdot = format!("../../{secret_name}/secret.txt");
        let cases: Vec<String> = vec![
            abs,
            dotdot,
            "sub/payload".into(),
            "..".into(),
            ".".into(),
            String::new(),
        ];
        for backup in cases {
            let _ = std::fs::remove_file(&target);
            let (cfg, snap_dir) = forge_snapshot(&temp_snap, &allowed_dir, &target, &backup);
            if backup == "sub/payload" {
                std::fs::create_dir_all(snap_dir.join("sub")).unwrap();
                std::fs::write(snap_dir.join("sub").join("payload"), b"SECRET").unwrap();
            }
            let res = action_restore_snapshot("forged", &cfg).await;
            assert!(!res.success, "restore accepted snapshot_file {backup:?}");
            let msg = res
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_default();
            assert!(
                msg.contains("snapshot_file"),
                "refusal must name snapshot_file for {backup:?}, got {msg:?}"
            );
            assert!(
                !target.exists(),
                "snapshot_file {backup:?} must not produce a file inside the write roots"
            );
            std::fs::remove_dir_all(&snap_dir).unwrap();
        }
    }

    /// A symlink planted in the snapshot dir under a valid backup name
    /// must not be followed either. The backup is opened without following
    /// links, so a swapped `0.dat` is refused rather than copied.
    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_symlinked_backup_in_snapshot_dir() {
        let temp_snap = TempDir::new().unwrap();
        let allowed_dir = TempDir::new().unwrap();
        let secret_dir = TempDir::new().unwrap();
        let secret = secret_dir.path().join("secret.txt");
        std::fs::write(&secret, b"SECRET").unwrap();
        let target = allowed_dir.path().join("exfil.txt");

        let (cfg, snap_dir) = forge_snapshot(&temp_snap, &allowed_dir, &target, "0.dat");
        std::os::unix::fs::symlink(&secret, snap_dir.join("0.dat")).unwrap();

        let res = action_restore_snapshot("forged", &cfg).await;
        assert!(!res.success, "restore followed a symlinked backup: {res:?}");
        assert!(
            !target.exists(),
            "a symlinked backup must not be copied into the write roots"
        );
    }

    /// Nominal: a plain backup name inside the snapshot dir still restores.
    #[tokio::test]
    async fn restore_accepts_plain_backup_name() {
        let temp_snap = TempDir::new().unwrap();
        let allowed_dir = TempDir::new().unwrap();
        let target = allowed_dir.path().join("restored.txt");

        let (cfg, snap_dir) = forge_snapshot(&temp_snap, &allowed_dir, &target, "0.dat");
        std::fs::write(snap_dir.join("0.dat"), b"BACKUP").unwrap();

        let res = action_restore_snapshot("forged", &cfg).await;
        assert!(res.success, "plain backup name refused: {res:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "BACKUP");
    }

    /// The validator is applied where the name becomes a path, so every
    /// lookup inherits it rather than each call site remembering.
    #[test]
    fn every_name_to_path_mapping_is_validated() {
        let cfg = AppConfig::defaults();
        assert!(snapshot_dir(&cfg, "../escape").is_err());
        assert!(snapshot_dir(&cfg, "ok-name_1").is_ok());
    }

    #[test]
    fn validate_name_ok() {
        assert!(validate_name("my-snapshot_01").is_ok());
    }

    #[test]
    fn validate_name_rejects_special_chars() {
        assert!(validate_name("bad/name").is_err());
        assert!(validate_name("bad name").is_err());
        assert!(validate_name("").is_err());
    }

    #[tokio::test]
    async fn create_list_delete_snapshot() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        // Create source file
        let src_file = temp_src.path().join("hello.txt");
        let mut f = std::fs::File::create(&src_file).unwrap();
        writeln!(f, "hello world").unwrap();

        // CreateSnapshot
        let r = action_create_snapshot("test01", &[src_file.to_string_lossy().into_owned()], &cfg)
            .await;
        assert!(r.success, "{:?}", r.error);
        matches!(
            r.output.unwrap(),
            ActionOutput::SnapshotCreated { files_count: 1, .. }
        );

        // ListSnapshots
        let r = action_list_snapshots(&cfg).await;
        assert!(r.success);
        if let Some(ActionOutput::SnapshotList { snapshots }) = r.output {
            assert_eq!(snapshots.len(), 1);
            assert_eq!(snapshots[0].name, "test01");
        }

        // DeleteSnapshot
        let r = action_delete_snapshot("test01", &cfg).await;
        assert!(r.success);

        // Gone
        let r = action_list_snapshots(&cfg).await;
        if let Some(ActionOutput::SnapshotList { snapshots }) = r.output {
            assert_eq!(snapshots.len(), 0);
        }
    }

    #[tokio::test]
    async fn restore_snapshot_restores_file() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        let src_file = temp_src.path().join("restore_me.txt");
        std::fs::write(&src_file, b"original content").unwrap();

        // Snapshot it
        action_create_snapshot(
            "restore-test",
            &[src_file.to_string_lossy().into_owned()],
            &cfg,
        )
        .await;

        // Overwrite the file
        std::fs::write(&src_file, b"overwritten").unwrap();
        assert_eq!(std::fs::read_to_string(&src_file).unwrap(), "overwritten");

        // Restore
        let r = action_restore_snapshot("restore-test", &cfg).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            std::fs::read_to_string(&src_file).unwrap(),
            "original content"
        );
    }

    #[tokio::test]
    async fn restore_recreates_missing_parent_dirs() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        let nested = temp_src.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_file = nested.join("nested.txt");
        std::fs::write(&nested_file, b"deep file").unwrap();

        action_create_snapshot(
            "deep-restore",
            &[nested_file.to_string_lossy().into_owned()],
            &cfg,
        )
        .await;

        std::fs::remove_dir_all(temp_src.path().join("a")).unwrap();
        assert!(!nested_file.exists());

        let r = action_restore_snapshot("deep-restore", &cfg).await;
        assert!(r.success, "{:?}", r.error);
        assert!(nested_file.exists());
        assert_eq!(std::fs::read_to_string(&nested_file).unwrap(), "deep file");
    }

    #[test]
    fn try_reflink_or_copy_falls_back_to_copy_on_unsupported_fs() {
        // /tmp on most CI / dev boxes is tmpfs or ext4 → no FICLONE.
        // The helper must succeed via the byte-copy path and produce
        // an identical destination.
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload = b"reflink-fallback-test\n\x00\x01\x02";
        std::fs::write(&src, payload).unwrap();

        let method = try_reflink_or_copy(&src, &dst).expect("must succeed via fallback");
        assert!(dst.exists(), "destination must be created");
        assert_eq!(std::fs::read(&dst).unwrap(), payload, "bytes must match");
        // We can't assert which method ran (depends on the FS the test
        // happens to run on), but the result must be one of the two.
        assert!(matches!(method, CopyMethod::Reflink | CopyMethod::Copy));
    }

    #[tokio::test]
    async fn snapshot_records_copy_method_in_output() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        let src_file = temp_src.path().join("payload.txt");
        std::fs::write(&src_file, b"check method field").unwrap();

        let r = action_create_snapshot(
            "method-test",
            &[src_file.to_string_lossy().into_owned()],
            &cfg,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let method = match r.output.unwrap() {
            ActionOutput::SnapshotCreated { method, .. } => method,
            other => panic!("wrong variant: {:?}", other),
        };
        // Must be populated and one of the known values.
        let m = method.expect("method must be set when files were copied");
        assert!(
            matches!(m.as_str(), "reflink" | "copy" | "mixed"),
            "unexpected method: {m}"
        );

        // Restore should also surface a method value.
        std::fs::remove_file(&src_file).unwrap();
        let r = action_restore_snapshot("method-test", &cfg).await;
        assert!(r.success, "{:?}", r.error);
        let method = match r.output.unwrap() {
            ActionOutput::SnapshotRestored { method, .. } => method,
            other => panic!("wrong variant: {:?}", other),
        };
        let m = method.expect("restore method must be set when files were restored");
        assert!(matches!(m.as_str(), "reflink" | "copy" | "mixed"));
    }

    #[tokio::test]
    async fn snapshot_recurses_into_directory() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        // Create a small project tree.
        std::fs::create_dir_all(temp_src.path().join("src/components")).unwrap();
        std::fs::create_dir_all(temp_src.path().join("tests")).unwrap();
        std::fs::write(temp_src.path().join("package.json"), b"{}").unwrap();
        std::fs::write(temp_src.path().join("src/index.js"), b"console.log(1)").unwrap();
        std::fs::write(temp_src.path().join("src/components/Btn.jsx"), b"<button/>").unwrap();
        std::fs::write(temp_src.path().join("tests/main.test.js"), b"test()").unwrap();

        let r = action_create_snapshot(
            "dir-recursion",
            &[temp_src.path().to_string_lossy().into_owned()],
            &cfg,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let count = match r.output.unwrap() {
            ActionOutput::SnapshotCreated { files_count, .. } => files_count,
            other => panic!("wrong variant: {:?}", other),
        };
        assert_eq!(count, 4, "must capture all 4 files via recursion");
    }

    #[tokio::test]
    async fn restore_removes_files_added_after_snapshot() {
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        // Initial state: 1 file under a subdir.
        std::fs::create_dir_all(temp_src.path().join("src")).unwrap();
        std::fs::write(temp_src.path().join("src/keep.js"), b"keep").unwrap();

        // Snapshot the whole directory.
        let r = action_create_snapshot(
            "atomic-test",
            &[temp_src.path().to_string_lossy().into_owned()],
            &cfg,
        )
        .await;
        assert!(r.success, "{:?}", r.error);

        // Agent adds two rogue files.
        std::fs::write(temp_src.path().join("src/rogue.js"), b"BAD").unwrap();
        std::fs::write(temp_src.path().join("ROGUE.md"), b"BAD").unwrap();
        // …and modifies the existing one.
        std::fs::write(temp_src.path().join("src/keep.js"), b"corrupted").unwrap();

        let r = action_restore_snapshot("atomic-test", &cfg).await;
        assert!(r.success, "{:?}", r.error);
        let (restored, removed) = match r.output.unwrap() {
            ActionOutput::SnapshotRestored {
                files_restored,
                files_removed,
                ..
            } => (files_restored, files_removed),
            other => panic!("wrong variant: {:?}", other),
        };
        assert_eq!(restored, 1, "the one snapshotted file must be restored");
        assert_eq!(removed, 2, "both rogue files must be deleted");

        // Filesystem is back to the snapshot state, byte-identical.
        assert_eq!(
            std::fs::read_to_string(temp_src.path().join("src/keep.js")).unwrap(),
            "keep"
        );
        assert!(
            !temp_src.path().join("src/rogue.js").exists(),
            "rogue.js must be gone"
        );
        assert!(
            !temp_src.path().join("ROGUE.md").exists(),
            "ROGUE.md must be gone"
        );
    }

    #[tokio::test]
    async fn restore_legacy_manifest_without_roots_skips_cleanup() {
        // Backwards compatibility: a manifest written before the
        // recursion change has no `roots` field (deserialised as empty).
        // Restore must still rewrite the listed files but must not
        // attempt cleanup (no roots → nothing to scan).
        let temp_snap = TempDir::new().unwrap();
        let temp_src = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);

        let f = temp_src.path().join("legacy.txt");
        std::fs::write(&f, b"original").unwrap();

        action_create_snapshot("legacy", &[f.to_string_lossy().into_owned()], &cfg).await;
        // Hand-strip the `roots` field from the on-disk manifest to
        // simulate a pre-recursion snapshot.
        let snap_dir = snapshot_dir(&cfg, "legacy").unwrap();
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(manifest_path(&snap_dir)).unwrap())
                .unwrap();
        m.as_object_mut().unwrap().remove("roots");
        std::fs::write(manifest_path(&snap_dir), m.to_string()).unwrap();

        // An agent dropped a sibling rogue file — it must NOT be deleted
        // because the legacy manifest didn't claim ownership of the dir.
        let rogue = temp_src.path().join("rogue-not-touched.txt");
        std::fs::write(&rogue, b"safe").unwrap();
        std::fs::write(&f, b"clobbered").unwrap();

        let r = action_restore_snapshot("legacy", &cfg).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
        assert!(
            rogue.exists(),
            "legacy restore must NOT touch unrelated files"
        );
    }

    #[tokio::test]
    async fn a_failed_snapshot_leaves_nothing_behind() {
        let temp = TempDir::new().unwrap();
        let mut cfg = test_config(&temp);
        cfg.snapshots.max_snapshot_size_mb = 0;
        let f = temp.path().join("a.txt");
        std::fs::write(&f, "alpha").unwrap();
        let paths = vec![f.to_string_lossy().into_owned()];
        assert!(create_snapshot("half", &paths, &cfg).await.is_err());
        assert!(
            !snapshot_root(&cfg).join("half").exists(),
            "no half-written snapshot"
        );
        cfg.snapshots.max_snapshot_size_mb = 1;
        assert!(
            create_snapshot("half", &paths, &cfg).await.is_ok(),
            "the name is free again"
        );
    }

    #[tokio::test]
    async fn delete_nonexistent_snapshot_errors() {
        let temp_snap = TempDir::new().unwrap();
        let cfg = test_config(&temp_snap);
        let r = action_delete_snapshot("nope", &cfg).await;
        assert!(!r.success);
        assert!(matches!(r.error.unwrap().kind, ErrorKind::NotFound));
    }

    /// A directory on a filesystem that may clone: `X0NE_REFLINK_TEST_DIR`
    /// (btrfs in CI), else the system temp dir.
    fn reflink_test_dir() -> TempDir {
        match std::env::var("X0NE_REFLINK_TEST_DIR") {
            Ok(d) if !d.is_empty() => TempDir::new_in(d).unwrap(),
            _ => TempDir::new().unwrap(),
        }
    }

    fn can_clone(dir: &Path) -> bool {
        let src = dir.join(".probe-src");
        let dst = dir.join(".probe-dst");
        std::fs::write(&src, b"probe").unwrap();
        let ok = reflink_copy(&src, &dst).is_ok();
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        ok
    }

    fn config_in(dir: &Path, snaps: &Path, cap_mb: u64) -> AppConfig {
        let mut cfg = AppConfig::defaults();
        cfg.snapshots.dir = snaps.display().to_string();
        cfg.snapshots.max_snapshot_size_mb = cap_mb;
        cfg.policy.allowed_write_paths = vec![dir.display().to_string()];
        cfg.policy.allowed_read_paths = vec![dir.display().to_string()];
        cfg
    }

    fn method_of(r: ActionResult) -> String {
        assert!(r.success, "{:?}", r.error);
        match r.output.unwrap() {
            ActionOutput::SnapshotCreated { method, .. } => method.unwrap_or_default(),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A clone shares blocks, so the size cap (a guard on bytes copied)
    /// does not apply to it; a byte copy still meets the cap and says so.
    #[tokio::test]
    async fn reflinked_bytes_do_not_count_against_the_copy_cap() {
        let tmp = reflink_test_dir();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("big.bin"), vec![7u8; 2 * 1024 * 1024]).unwrap();
        let cfg = config_in(tmp.path(), &tmp.path().join("snaps"), 1);
        let paths = vec![work.display().to_string()];
        let r = action_create_snapshot("cap", &paths, &cfg).await;
        if can_clone(tmp.path()) {
            assert_eq!(method_of(r), "reflink");
            let dat = tmp.path().join("snaps/cap/0.dat");
            if let Ok(out) = std::process::Command::new("filefrag")
                .arg("-v")
                .arg(&dat)
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(text.contains("shared"), "extents not shared: {text}");
            }
        } else {
            let e = r.error.expect("a 2 MB copy is over a 1 MB cap");
            assert!(e.message.contains("size limit"), "{}", e.message);
        }
    }

    #[tokio::test]
    async fn a_zero_cap_refuses_even_a_reflink() {
        let tmp = reflink_test_dir();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "alpha").unwrap();
        let cfg = config_in(tmp.path(), &tmp.path().join("snaps"), 0);
        let r = action_create_snapshot("zero", &[f.display().to_string()], &cfg).await;
        assert!(!r.success, "max_snapshot_size_mb = 0 disables snapshots");
    }

    /// Snapshot store on another filesystem: FICLONE fails with EXDEV, the
    /// copy is taken and announced as "copy".
    #[tokio::test]
    async fn a_snapshot_across_filesystems_is_a_copy_and_says_so() {
        use std::os::unix::fs::MetadataExt;
        let tmp = reflink_test_dir();
        let snaps = TempDir::new().unwrap();
        let dev = |p: &Path| std::fs::metadata(p).unwrap().dev();
        if dev(tmp.path()) == dev(snaps.path()) {
            eprintln!("skipped: X0NE_REFLINK_TEST_DIR is on the temp dir's filesystem");
            return;
        }
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "alpha").unwrap();
        let cfg = config_in(tmp.path(), snaps.path(), 1);
        let r = action_create_snapshot("x", &[f.display().to_string()], &cfg).await;
        assert_eq!(method_of(r), "copy");
    }

    /// measurement: one checkpoint of a 2 GB tree, reflink then copy.
    /// `X0NE_REFLINK_TEST_DIR` on btrfs; the copy goes to the temp dir.
    #[tokio::test]
    #[ignore = "writes 4 GB; run by hand"]
    async fn checkpoint_timing_2gb() {
        let tmp = reflink_test_dir();
        let work = tmp.path().join("work");
        let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        for d in 0..16 {
            let dir = work.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..128 {
                std::fs::write(dir.join(format!("f{f}.bin")), &chunk).unwrap();
            }
        }
        // FICLONE flushes dirty pages first: a tree just written would time
        // the writeback, not the clone. A project at rest is on disk.
        unsafe { libc::sync() };
        let paths = vec![work.display().to_string()];
        let same_fs = config_in(tmp.path(), &tmp.path().join("snaps"), 4096);
        let t = std::time::Instant::now();
        let m1 = method_of(action_create_snapshot("r", &paths, &same_fs).await);
        let reflink = t.elapsed();
        let other = TempDir::new().unwrap();
        let other_fs = config_in(tmp.path(), other.path(), 4096);
        let t = std::time::Instant::now();
        let m2 = method_of(action_create_snapshot("c", &paths, &other_fs).await);
        let copy = t.elapsed();
        eprintln!("TIMING 2 GB (2048 files): {m1} {reflink:?}, {m2} {copy:?}");
    }
}
