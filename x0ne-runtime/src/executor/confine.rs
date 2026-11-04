//! Intrinsic filesystem confinement for the executor.
//!
//! # Why this exists
//!
//! Before the August 2026 hardening pass, confinement was advisory: the policy
//! engine canonicalized a path, checked *that* string against the allowlists,
//! and then the executor re-opened the **original, caller-supplied string** with
//! an independent syscall. Two objects, two moments — a textbook TOCTOU. Swapping
//! the final component for a symlink between the check and the open redirected
//! the I/O anywhere on the host. Worse, an empty allowlist meant "allow every
//! path" rather than "allow none", so the shipped default read any file.
//!
//! # The invariant
//!
//! A path is resolved to a canonical object **once**, validated, and the very
//! same object is then opened. Callers cannot hand a raw string to the I/O layer:
//! [`ConfinedPath`] is the only key that opens a file, and it can only be minted
//! by [`resolve_read`] / [`resolve_write`], which enforce the roots. That makes
//! the boundary intrinsic to the executor rather than something each call site
//! must remember to ask for.
//!
//! # Defence in depth
//!
//! 1. **Canonicalize once.** `fs::canonicalize` resolves every `..` and symlink,
//!    so prefix comparison happens on a real, fully-resolved path.
//! 2. **Component-wise containment.** Roots are compared by path *component*,
//!    never by string prefix, so `/srv/rootEVIL` is not inside `/srv/root`.
//! 3. **Never write through a symlink.** A write target that is itself a symlink
//!    is refused outright (this also covers dangling symlinks, which `exists()`
//!    reports as absent and which would otherwise be created through).
//! 4. **No symlink anywhere at open.** On Linux the open goes through
//!    `openat2(2)` with `RESOLVE_NO_SYMLINKS`. The validated path is canonical,
//!    so a symlink in any of its components is a swap: the kernel refuses it
//!    (`ELOOP`) before a file is created. Where `openat2` is missing (`ENOSYS`,
//!    or `EPERM` from a seccomp filter) and on other systems, `O_NOFOLLOW`
//!    protects the final component only.
//! 5. **Post-open identity check.** On Linux the opened descriptor is resolved
//!    through `/proc/self/fd/<n>` and must still equal the validated path, which
//!    also catches a swapped *parent* directory. On Linux, an open that neither
//!    `openat2` nor `/proc` can vouch for is refused.
//! 6. **Truncate last.** A write opens without `O_TRUNC` and empties the file
//!    only once 4 and 5 have passed.
//!
//! The only `unsafe` is the `openat2` call, which std does not wrap, and the
//! adoption of the descriptor it returns.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::config::PolicyConfig;
use crate::result::ErrorKind;

/// Why a path was refused. Distinct from `io::Error` so callers can map it to a
/// `policy_denied` result rather than a generic I/O failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfineError {
    /// No roots configured for this access kind — deny-all by design.
    NoRootsConfigured { kind: &'static str },
    /// Roots ARE configured, and not one of them resolves to a directory that
    /// exists. Distinct from [`Self::NoRootsConfigured`] because the operator's
    /// file is not empty, and telling him it is sends him to fix the wrong
    /// thing.
    ///
    /// Field report 2026-08-16 (session `2026-08-16_15-17_6b1c`): the policy
    /// carried `allowed_read_paths = ["~/code"]`, `~/code` did not exist on
    /// that machine, `normalize_roots` dropped it silently, and every read
    /// came back "no read paths are configured". The operator went looking for
    /// an empty list he did not have. `allowed_write_paths` held `/tmp` too,
    /// which resolved — so writes got the ordinary "outside the roots" and the
    /// two messages disagreed about the same file.
    RootsUnresolvable {
        kind: &'static str,
        configured: Vec<String>,
    },
    /// Resolved outside every configured root.
    OutsideRoots { path: String, kind: &'static str },
    /// The target itself is a symlink and the operation refuses to follow it.
    RefusesSymlink { path: String },
    /// The path (or its parent) could not be resolved to a real object.
    Unresolvable { path: String, reason: String },
    /// The object opened was not the object validated (swap detected).
    IdentityChanged { path: String },
    /// The target does not exist, but its parent IS inside an allowed root, so
    /// the caller was authorized to look. Reported as a genuine `not_found`
    /// rather than a policy denial — collapsing the two would make "typo" and
    /// "forbidden" indistinguishable to the agent. Existence is never revealed
    /// for paths whose parent lies outside the roots; those stay denied.
    NotFoundInRoot { path: String },
}

impl ConfineError {
    pub fn message(&self) -> String {
        match self {
            // One line, on purpose. The paragraph this replaced carried the
            // Aug-2026 breaking-change note into every refusal; that history
            // belongs in the reference TOML (which states it beside the key)
            // and in the CHANGELOG, not in a red wall the operator reads
            // while he is trying to unblock a turn.
            ConfineError::NoRootsConfigured { kind } => format!(
                "no {kind} paths configured: every {kind} is denied · answer 1/2 at the \
                 prompt or set [policy] allowed_{kind}_paths"
            ),
            ConfineError::RootsUnresolvable { kind, configured } => format!(
                "none of the [policy] allowed_{kind}_paths entries exist, so every {kind} \
                 is denied: {} · create them or fix the paths in x0ne-runtime.toml",
                configured.join(", ")
            ),
            ConfineError::OutsideRoots { path, kind } => {
                format!("path '{path}' resolves outside every allowed_{kind}_paths root.")
            }
            ConfineError::RefusesSymlink { path } => format!(
                "path '{path}' is a symbolic link; the runtime refuses to write through \
                 symlinks because the target can be repointed outside the sandbox."
            ),
            ConfineError::Unresolvable { path, reason } => {
                format!("path '{path}' could not be resolved: {reason}")
            }
            ConfineError::IdentityChanged { path } => format!(
                "path '{path}' changed between validation and open (possible symlink \
                 swap); the operation was aborted."
            ),
            ConfineError::NotFoundInRoot { path } => {
                format!("path '{path}' does not exist.")
            }
        }
    }

    /// Confinement refusals are policy decisions. The one exception is a target
    /// that simply does not exist *inside an allowed root*: that is an ordinary
    /// `not_found`, and reporting it as `policy_denied` would tell the agent to
    /// stop rather than to fix the path.
    pub fn kind(&self) -> ErrorKind {
        match self {
            ConfineError::NotFoundInRoot { .. } => ErrorKind::NotFound,
            _ => ErrorKind::PolicyDenied,
        }
    }
}

/// A path that has been resolved and validated against the configured roots.
///
/// Holding one is proof the enclosed path passed confinement. It is deliberately
/// the only way to reach the I/O helpers below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedPath {
    canonical: PathBuf,
}

impl ConfinedPath {
    /// The validated, fully-resolved path. Use this for I/O — never the caller's
    /// original string.
    pub fn as_path(&self) -> &Path {
        &self.canonical
    }

    pub fn display(&self) -> String {
        self.canonical.to_string_lossy().into_owned()
    }

    /// SECURITY: `#[cfg(test)]` only. A public constructor would bypass
    /// confinement, which is the only reason this type exists.
    #[cfg(all(test, feature = "asm-exec"))]
    pub(crate) fn for_test(p: PathBuf) -> Self {
        Self { canonical: p }
    }

    /// Open for reading, then verify the descriptor still refers to the
    /// validated object.
    pub fn open_read(&self) -> Result<File, ConfineError> {
        let (file, kernel_checked) = self.open_confined(false)?;
        self.verify_identity(&file, kernel_checked)?;
        Ok(file)
    }

    /// Open for writing (created if absent), verify identity, then truncate.
    ///
    /// SECURITY: `O_TRUNC` at open emptied whatever the path reached before
    /// `verify_identity` could refuse it. With a parent directory swapped for a
    /// symlink after validation, a file outside the roots was truncated, then
    /// the write was refused (audit of 2026-11-04).
    pub fn open_write(&self) -> Result<File, ConfineError> {
        let (file, kernel_checked) = self.open_confined(true)?;
        self.verify_identity(&file, kernel_checked)?;
        file.set_len(0).map_err(|e| ConfineError::Unresolvable {
            path: self.display(),
            reason: e.to_string(),
        })?;
        Ok(file)
    }

    /// Open the validated path, never with `O_TRUNC`. The flag says whether
    /// the kernel refused a symlink in every component (`openat2`).
    fn open_confined(&self, write: bool) -> Result<(File, bool), ConfineError> {
        #[cfg(target_os = "linux")]
        {
            match openat2_no_symlinks(&self.canonical, write) {
                Ok(file) => return Ok((file, true)),
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) => {}
                Err(e) => return Err(self.open_err(e)),
            }
        }
        let mut options = OpenOptions::new();
        if write {
            options.write(true).create(true);
        } else {
            options.read(true);
        }
        let file = options
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.canonical)
            .map_err(|e| self.open_err(e))?;
        Ok((file, false))
    }

    /// Read the whole (validated) file as bytes.
    pub fn read_bytes(&self) -> Result<Vec<u8>, ConfineError> {
        use std::io::Read;
        let mut f = self.open_read()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| ConfineError::Unresolvable {
                path: self.display(),
                reason: e.to_string(),
            })?;
        Ok(buf)
    }

    /// Write bytes to the (validated) file.
    pub fn write_bytes(&self, data: &[u8]) -> Result<(), ConfineError> {
        use std::io::Write;
        let mut f = self.open_write()?;
        f.write_all(data).map_err(|e| ConfineError::Unresolvable {
            path: self.display(),
            reason: e.to_string(),
        })
    }

    fn open_err(&self, e: std::io::Error) -> ConfineError {
        // ELOOP here means a component became a symlink after we validated
        // it (the last one under O_NOFOLLOW, any one under openat2) — i.e. we
        // just defeated a swap.
        if e.raw_os_error() == Some(libc::ELOOP) {
            return ConfineError::IdentityChanged {
                path: self.display(),
            };
        }
        ConfineError::Unresolvable {
            path: self.display(),
            reason: e.to_string(),
        }
    }

    /// Confirm the open descriptor still resolves to the validated path.
    ///
    /// `O_NOFOLLOW` only protects the final component; this catches a parent
    /// directory being swapped. Linux-only (`/proc/self/fd`); elsewhere the
    /// `O_NOFOLLOW` guarantee stands alone, a platform capability difference.
    fn verify_identity(&self, file: &File, kernel_checked: bool) -> Result<(), ConfineError> {
        use std::os::unix::io::AsRawFd;
        let link = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        match identity(fs::read_link(&link), &self.canonical, kernel_checked) {
            Identity::Holds => Ok(()),
            Identity::Changed => Err(ConfineError::IdentityChanged {
                path: self.display(),
            }),
            Identity::Unverifiable => Err(ConfineError::Unresolvable {
                path: self.display(),
                reason: "neither openat2 nor /proc/self/fd can confirm which file was opened"
                    .into(),
            }),
        }
    }
}

/// What the post-open check concludes, apart from the syscalls.
#[derive(Debug, PartialEq, Eq)]
enum Identity {
    Holds,
    Changed,
    Unverifiable,
}

/// `readlink` is the target of `/proc/self/fd/<n>`; `kernel_checked` says
/// whether `openat2` refused symlinks in every component.
fn identity(
    readlink: std::io::Result<PathBuf>,
    canonical: &Path,
    kernel_checked: bool,
) -> Identity {
    match readlink {
        Ok(actual) if actual == canonical => Identity::Holds,
        Ok(_) => Identity::Changed,
        // SECURITY: on Linux without /proc, only openat2 still vouches for the
        // object. Any error used to pass: the check failed open (2026-11-04).
        Err(_) if cfg!(target_os = "linux") && !kernel_checked => Identity::Unverifiable,
        // Elsewhere there is no /proc to read: O_NOFOLLOW stands alone.
        Err(_) => Identity::Holds,
    }
}

/// `openat2(2)` with `RESOLVE_NO_SYMLINKS`: `ELOOP` on a symlink in ANY
/// component of `path`, the last one included. Never `O_TRUNC`.
#[cfg(target_os = "linux")]
fn openat2_no_symlinks(path: &Path, write: bool) -> std::io::Result<File> {
    use std::os::fd::{FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let access = if write {
        libc::O_WRONLY | libc::O_CREAT
    } else {
        libc::O_RDONLY
    };
    // SAFETY: `open_how` is three integers, for which all zero is a valid value.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (access | libc::O_CLOEXEC) as u64;
    how.mode = if write { 0o666 } else { 0 };
    how.resolve = libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: `c_path` is NUL-terminated and `how` initialized; both outlive
    // the call, and the size passed is the size of `how`.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            &raw const how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor from the kernel, owned by nothing else.
    Ok(unsafe { File::from_raw_fd(fd as RawFd) })
}

/// Expand `~`, then canonicalize each configured root.
///
/// A root that does not exist is dropped: it cannot contain anything, and
/// keeping it would let a later `mkdir` silently widen the sandbox.
pub fn normalize_roots(raw: &[String]) -> Vec<PathBuf> {
    raw.iter()
        .map(|r| crate::config::expand_tilde(r))
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect()
}

/// The configured entries that `normalize_roots` throws away.
///
/// SECURITY-adjacent: dropping them is the right containment answer (a root
/// that does not resolve cannot contain anything), but doing it SILENTLY is
/// what turned a one-character typo into "your list is empty". Every caller
/// that can report to a human should ask for this list.
pub fn unresolvable_roots(raw: &[String]) -> Vec<String> {
    raw.iter()
        .filter(|r| fs::canonicalize(crate::config::expand_tilde(r)).is_err())
        .cloned()
        .collect()
}

/// Which "nothing is allowed" this is: an empty list, or a list whose entries
/// all failed to resolve. The operator fixes those two by doing different
/// things, so they are not the same message.
fn empty_roots_error(kind: &'static str, configured: &[String]) -> ConfineError {
    if configured.is_empty() {
        ConfineError::NoRootsConfigured { kind }
    } else {
        ConfineError::RootsUnresolvable {
            kind,
            configured: configured.to_vec(),
        }
    }
}

/// The parent of a bare relative filename, made resolvable.
///
/// `Path::new("notes.md").parent()` is `Some("")`, and `fs::canonicalize("")`
/// is always `ENOENT`. Both resolvers below ask the parent where a file lives:
/// `resolve_read` to tell "missing" apart from "forbidden", `resolve_write` to
/// place a new file. With an empty parent both gave up, so a bare relative name
/// was unusable — `WriteFile { path: "notes.md" }` failed with "parent
/// directory unresolvable" inside a home the policy granted in full, and a
/// missing `ReadFile { path: "X0NE.md" }` was reported as a policy denial
/// rather than a not-found (measured in the demo enclave, 2026-08-15, run 9).
///
/// SECURITY: `.` is what `fs::canonicalize` already resolves a bare relative
/// name against on every other branch, so this makes the resolvers agree with
/// themselves rather than granting anything. The canonical result is still
/// checked against the roots by the caller: a cwd outside them stays refused.
fn dir_or_cwd(parent: &Path) -> &Path {
    if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    }
}

/// Component-wise containment test.
///
/// Compared by [`Component`] rather than string prefix so that `/srv/rootEVIL`
/// is NOT considered inside `/srv/root`. Both sides must already be canonical.
pub fn is_within_roots(candidate: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        let mut r = root.components();
        let mut c = candidate.components();
        loop {
            match (r.next(), c.next()) {
                // root exhausted → candidate is at or below it
                (None, _) => return true,
                (Some(a), Some(b)) if a == b => continue,
                _ => return false,
            }
        }
    })
}

/// Expand a leading `~` in a path the *model* supplied, exactly as
/// [`normalize_roots`] already expands one in a path the *operator* supplied.
///
/// # Why this is not a widening
///
/// The asymmetry it removes was measured on 2026-08-16: `allowed_read_paths =
/// ["~/work"]` was expanded by `normalize_roots` (line above), while
/// `ReadFile { path: "~/work/notes.md" }` was handed to `fs::canonicalize`
/// verbatim and came back "could not be resolved". The operator's file and the
/// model's argument were reading the same character two different ways, and
/// the model was told the file did not exist.
///
/// SECURITY: three properties make this safe to do on model input.
///
/// 1. **The home is the runtime's own, never the model's.** `dirs::home_dir()`
///    reads the environment of *this* process. Nothing on the action path can
///    set it: an action's `env` never reaches the runtime's own environment
///    (since 2026-08-15), and children are `env_clear`ed. A model cannot choose
///    where `~` points, it can only name it.
/// 2. **Expansion happens BEFORE confinement, not instead of it.** The result
///    goes through the same `fs::canonicalize` + [`is_within_roots`] as every
///    other path, so `~/../etc/passwd` resolves to `/etc/passwd` and is refused
///    on its resolved location like any other escape attempt. A `~` cannot
///    leave the roots; it can only name something inside them.
/// 3. **Only a leading `~` or `~/` is touched.** `~user` is not expanded — no
///    passwd lookup, and a directory literally named `~user` keeps its name.
fn expand_model_path(raw: &str) -> PathBuf {
    crate::config::expand_tilde(raw)
}

/// Resolve and validate a path for READING.
///
/// The target must already exist; canonicalization resolves every symlink, so a
/// link pointing outside the roots is rejected on its resolved location.
pub fn resolve_read(raw: &str, policy: &PolicyConfig) -> Result<ConfinedPath, ConfineError> {
    let roots = normalize_roots(&policy.allowed_read_paths);
    if roots.is_empty() {
        return Err(empty_roots_error("read", &policy.allowed_read_paths));
    }
    let expanded = expand_model_path(raw);
    let canonical = match fs::canonicalize(&expanded) {
        Ok(c) => c,
        Err(e) => {
            // Distinguish "missing" from "forbidden" WITHOUT leaking existence
            // outside the sandbox: only report not-found when the parent
            // directory itself resolves inside an allowed root. Anything else
            // (parent missing, parent outside the roots, dangling symlink) stays
            // a policy denial.
            if e.kind() == std::io::ErrorKind::NotFound {
                if let Some(parent) = expanded.parent().map(dir_or_cwd) {
                    if let Ok(cp) = fs::canonicalize(parent) {
                        if is_within_roots(&cp, &roots) {
                            return Err(ConfineError::NotFoundInRoot {
                                path: raw.to_string(),
                            });
                        }
                    }
                }
            }
            return Err(ConfineError::Unresolvable {
                path: raw.to_string(),
                reason: e.to_string(),
            });
        }
    };
    if !is_within_roots(&canonical, &roots) {
        return Err(ConfineError::OutsideRoots {
            path: canonical.to_string_lossy().into_owned(),
            kind: "read",
        });
    }
    Ok(ConfinedPath { canonical })
}

/// Resolve and validate a path for WRITING (create or modify).
///
/// Rejects any target that is itself a symlink — including a *dangling* one,
/// which `Path::exists()` reports as absent and which would otherwise be
/// created through, planting a file at the link's out-of-root target.
pub fn resolve_write(raw: &str, policy: &PolicyConfig) -> Result<ConfinedPath, ConfineError> {
    let roots = normalize_roots(&policy.allowed_write_paths);
    if roots.is_empty() {
        return Err(empty_roots_error("write", &policy.allowed_write_paths));
    }

    let expanded = expand_model_path(raw);
    let raw_path = expanded.as_path();
    match fs::symlink_metadata(raw_path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(ConfineError::RefusesSymlink {
            path: raw.to_string(),
        }),
        // Exists as a real object: canonicalize and validate it.
        Ok(_) => {
            let canonical = fs::canonicalize(raw_path).map_err(|e| ConfineError::Unresolvable {
                path: raw.to_string(),
                reason: e.to_string(),
            })?;
            if !is_within_roots(&canonical, &roots) {
                return Err(ConfineError::OutsideRoots {
                    path: canonical.to_string_lossy().into_owned(),
                    kind: "write",
                });
            }
            Ok(ConfinedPath { canonical })
        }
        // New file: resolve the parent (which must exist) and validate there.
        Err(_) => {
            let parent =
                raw_path
                    .parent()
                    .map(dir_or_cwd)
                    .ok_or_else(|| ConfineError::Unresolvable {
                        path: raw.to_string(),
                        reason: "path has no parent directory".into(),
                    })?;
            let name = raw_path
                .file_name()
                .ok_or_else(|| ConfineError::Unresolvable {
                    path: raw.to_string(),
                    reason: "path has no final component".into(),
                })?;
            // Reject `..` in the final component outright; a parent traversal is
            // resolved below, but `a/..` as a *name* is nonsensical for a write.
            if raw_path
                .components()
                .any(|c| matches!(c, Component::ParentDir))
                && fs::canonicalize(parent).is_err()
            {
                return Err(ConfineError::Unresolvable {
                    path: raw.to_string(),
                    reason: "parent directory does not exist".into(),
                });
            }
            let canonical_parent =
                fs::canonicalize(parent).map_err(|e| ConfineError::Unresolvable {
                    path: raw.to_string(),
                    reason: format!("parent directory unresolvable: {e}"),
                })?;
            let canonical = canonical_parent.join(name);
            if !is_within_roots(&canonical, &roots) {
                return Err(ConfineError::OutsideRoots {
                    path: canonical.to_string_lossy().into_owned(),
                    kind: "write",
                });
            }
            Ok(ConfinedPath { canonical })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_wise_containment_rejects_prefix_siblings() {
        let root = PathBuf::from("/srv/root");
        let roots = vec![root];
        assert!(is_within_roots(Path::new("/srv/root"), &roots));
        assert!(is_within_roots(Path::new("/srv/root/a/b"), &roots));
        // The bug a string `starts_with` would have:
        assert!(!is_within_roots(Path::new("/srv/rootEVIL/secret"), &roots));
        assert!(!is_within_roots(Path::new("/srv/other"), &roots));
        assert!(!is_within_roots(Path::new("/"), &roots));
    }

    #[test]
    fn empty_roots_deny_rather_than_allow() {
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_read_paths = vec![];
        p.allowed_write_paths = vec![];
        assert_eq!(
            resolve_read("/etc/passwd", &p),
            Err(ConfineError::NoRootsConfigured { kind: "read" })
        );
        assert_eq!(
            resolve_write("/etc/cron.d/x", &p),
            Err(ConfineError::NoRootsConfigured { kind: "write" })
        );
    }

    /// A list whose entries all fail to resolve does not report as an empty
    /// list — it names the entries.
    ///
    /// Field report 2026-08-16: `allowed_read_paths = ["~/code"]` on a machine
    /// with no `~/code` refused every read with "no read paths are
    /// configured". The operator opened his policy file, found a read path in
    /// it, and had no way to reconcile the two. Containment is unchanged — a
    /// root that does not resolve still grants nothing; what changes is that
    /// the refusal says which sentence is the problem.
    #[test]
    fn a_root_that_does_not_resolve_is_reported_as_itself_not_as_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("code").to_string_lossy().into_owned();
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_read_paths = vec![missing.clone()];
        p.allowed_write_paths = vec![missing.clone()];

        for err in [
            resolve_read("/etc/passwd", &p).unwrap_err(),
            resolve_write("/etc/cron.d/x", &p).unwrap_err(),
        ] {
            assert!(
                matches!(err, ConfineError::RootsUnresolvable { .. }),
                "reported as an empty list: {err:?}"
            );
            assert!(
                err.message().contains(&missing),
                "the refusal does not name the entry that failed: {}",
                err.message()
            );
            // Still a policy denial: nothing was widened.
            assert_eq!(err.kind(), ErrorKind::PolicyDenied);
        }
    }

    /// The empty-list refusal is one line.
    ///
    /// It used to carry the Aug-2026 breaking-change note into every refusal —
    /// a five-row red paragraph in the middle of a turn. A failure means the
    /// history crept back into the message the operator reads while blocked.
    #[test]
    fn the_empty_list_refusal_is_a_single_line() {
        let msg = ConfineError::NoRootsConfigured { kind: "read" }.message();
        assert!(!msg.contains('\n'), "not one line: {msg}");
        assert!(
            msg.len() < 140,
            "still a paragraph ({} chars): {msg}",
            msg.len()
        );
        assert!(
            msg.contains("allowed_read_paths") && msg.contains("1/2"),
            "the one line must still say what to do: {msg}"
        );
        assert!(
            !msg.contains("CHANGELOG"),
            "the historical note belongs in the docs, not the refusal: {msg}"
        );
    }

    /// A missing file inside an allowed root is `not_found`; a missing file
    /// OUTSIDE the roots stays `policy_denied` so existence is never probed
    /// through the error channel.
    #[test]
    fn missing_inside_root_is_not_found_missing_outside_stays_denied() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_read_paths = vec![dir.path().to_string_lossy().into_owned()];

        let inside = dir.path().join("absent.txt");
        assert!(matches!(
            resolve_read(&inside.to_string_lossy(), &p),
            Err(ConfineError::NotFoundInRoot { .. })
        ));
        assert_eq!(
            resolve_read(&inside.to_string_lossy(), &p)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );

        // Outside the root: must NOT reveal whether it exists.
        let outside = "/etc/definitely-not-here-12345.conf";
        let err = resolve_read(outside, &p).unwrap_err();
        assert!(!matches!(err, ConfineError::NotFoundInRoot { .. }));
        assert_eq!(err.kind(), ErrorKind::PolicyDenied);
    }

    #[test]
    fn nonexistent_root_is_dropped_not_trusted() {
        let roots = normalize_roots(&["/definitely/not/here/at/all".to_string()]);
        assert!(
            roots.is_empty(),
            "a missing root must not widen the sandbox"
        );
    }

    /// A failure means the model and the operator read `~` two different ways:
    /// `allowed_read_paths = ["~/..."]` is expanded by `normalize_roots`, so a
    /// `ReadFile { path: "~/..." }` that is NOT expanded is refused inside a
    /// root the operator granted — reported as "could not be resolved", which
    /// reads as "your file is missing" (field report 2026-08-16).
    ///
    /// The second half is the one that must never be traded away for the
    /// first: expansion happens before confinement, so a `~` that walks back
    /// out of the roots is refused on its RESOLVED location like any other
    /// escape. A failure there would mean `~` had become a way out.
    #[test]
    fn a_tilde_in_a_model_path_expands_to_home_and_is_still_confined() {
        let Some(home) = dirs::home_dir() else {
            return; // no home on this host: nothing to expand against.
        };
        let Ok(home) = fs::canonicalize(&home) else {
            return;
        };

        // Granted the home itself: `~` names it, and naming it must work.
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_read_paths = vec!["~".to_string()];
        let resolved = resolve_read("~", &p).expect("`~` must resolve inside a granted home");
        assert_eq!(
            resolved.as_path(),
            home.as_path(),
            "`~` must expand to the runtime user's home, not stay literal"
        );

        // SECURITY: granted a temp dir instead, `~` is outside it and stays
        // outside it. Expansion is not permission.
        let dir = tempfile::tempdir().unwrap();
        p.allowed_read_paths = vec![dir.path().to_string_lossy().into_owned()];
        let err = resolve_read("~", &p).expect_err("`~` is not inside an unrelated root");
        assert_eq!(err.kind(), ErrorKind::PolicyDenied);

        // And the classic escape, spelled with a `~`: resolved, then refused.
        let err = resolve_read("~/../../etc/passwd", &p)
            .expect_err("a `~` that walks out of the roots is still out of the roots");
        assert_eq!(err.kind(), ErrorKind::PolicyDenied);
    }

    /// `~user` is NOT expanded: there is no passwd lookup here, and a directory
    /// whose name begins with `~` keeps its name. A failure would mean the
    /// runtime had started guessing at other users' home directories.
    #[test]
    fn a_tilde_user_prefix_is_left_literal() {
        let dir = tempfile::tempdir().unwrap();
        let literal = dir.path().join("~someoneelse");
        fs::create_dir(&literal).unwrap();
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_read_paths = vec![dir.path().to_string_lossy().into_owned()];

        let resolved = resolve_read(&literal.to_string_lossy(), &p)
            .expect("a real directory named `~someoneelse` is an ordinary path");
        assert_eq!(
            resolved.as_path(),
            fs::canonicalize(&literal).unwrap().as_path()
        );
    }

    /// The write resolver reads `~` the same way the read resolver does. A
    /// failure would mean a model could read `~/notes.md` and then be told the
    /// same path is unresolvable when it tries to write it back.
    #[test]
    fn the_write_resolver_expands_a_tilde_like_the_read_resolver() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let Ok(home) = fs::canonicalize(&home) else {
            return;
        };
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_write_paths = vec!["~".to_string()];

        // A name that (almost certainly) does not exist: the write resolver
        // places it from its parent, which is the expanded home.
        let resolved = resolve_write("~/x0ne-confine-tilde-probe-4f21.tmp", &p)
            .expect("a new file under a granted `~` must resolve");
        assert_eq!(
            resolved.as_path(),
            home.join("x0ne-confine-tilde-probe-4f21.tmp").as_path()
        );

        // SECURITY: and outside the granted root it stays refused.
        let dir = tempfile::tempdir().unwrap();
        p.allowed_write_paths = vec![dir.path().to_string_lossy().into_owned()];
        let err = resolve_write("~/x0ne-confine-tilde-probe-4f21.tmp", &p)
            .expect_err("a `~` outside the write roots is refused");
        assert_eq!(err.kind(), ErrorKind::PolicyDenied);
    }

    /// A root holding `sub/`, a directory outside it, and a write policy on the root.
    #[cfg(target_os = "linux")]
    fn swap_fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, PolicyConfig) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sub = fs::canonicalize(root.path()).unwrap().join("sub");
        fs::create_dir(&sub).unwrap();
        let mut p = crate::config::AppConfig::defaults().policy;
        p.allowed_write_paths = vec![root.path().to_string_lossy().into_owned()];
        (root, outside, sub, p)
    }

    /// The parent of a validated target swapped for a symlink to a directory
    /// outside the roots: the write is refused and the file there keeps its bytes.
    ///
    /// A failure would mean a refused write still emptied a file outside the
    /// roots: `O_TRUNC` at open ran before the identity check (audit of
    /// 2026-11-04).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_parent_swapped_after_validation_truncates_nothing_outside() {
        let (_root, outside, sub, p) = swap_fixture();
        fs::write(sub.join("f.txt"), b"inside").unwrap();
        fs::write(outside.path().join("f.txt"), b"precious").unwrap();
        let confined = resolve_write(&sub.join("f.txt").to_string_lossy(), &p).unwrap();

        fs::remove_dir_all(&sub).unwrap();
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();

        let err = confined.write_bytes(b"x").unwrap_err();
        assert!(
            matches!(err, ConfineError::IdentityChanged { .. }),
            "{err:?}"
        );
        assert_eq!(
            fs::read(outside.path().join("f.txt")).unwrap(),
            b"precious",
            "the file outside the roots was truncated"
        );
    }

    /// Same swap for a file that does not exist yet: nothing appears outside.
    ///
    /// A failure would mean a refused write still planted a file outside the
    /// roots. Needs openat2 (Linux 5.6): without it the test says so and returns.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_parent_swapped_after_validation_creates_nothing_outside() {
        let (root, outside, sub, p) = swap_fixture();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        if let Err(e) = openat2_no_symlinks(&canonical_root, false) {
            eprintln!("openat2 unavailable ({e}): the fallback cannot keep this promise");
            return;
        }
        let confined = resolve_write(&sub.join("new.txt").to_string_lossy(), &p).unwrap();

        fs::remove_dir_all(&sub).unwrap();
        std::os::unix::fs::symlink(outside.path(), &sub).unwrap();

        let err = confined.write_bytes(b"x").unwrap_err();
        assert!(
            matches!(err, ConfineError::IdentityChanged { .. }),
            "{err:?}"
        );
        assert!(
            !outside.path().join("new.txt").exists(),
            "a file was created outside the roots"
        );
        drop(root);
    }

    /// On Linux, an open that neither openat2 nor /proc vouches for is refused.
    ///
    /// A failure would mean the identity check passes when it cannot run, as
    /// `Err(_) => Ok(())` did until 2026-11-04.
    #[test]
    fn an_identity_nobody_can_check_is_refused_on_linux() {
        let canonical = Path::new("/srv/root/f");
        let gone = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(
            identity(Ok(canonical.into()), canonical, false),
            Identity::Holds
        );
        assert_eq!(
            identity(Ok("/srv/other/f".into()), canonical, true),
            Identity::Changed
        );
        assert_eq!(identity(gone(), canonical, true), Identity::Holds);
        let expected = if cfg!(target_os = "linux") {
            Identity::Unverifiable
        } else {
            Identity::Holds
        };
        assert_eq!(identity(gone(), canonical, false), expected);
    }
}
