use std::fs::{self as stdfs};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::action::Action;
use crate::config::AppConfig;
use crate::executor::confine;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

// ── ReadFile ──────────────────────────────────────────────────────────────────

pub async fn read_file(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::ReadFile {
        path,
        offset,
        limit,
        start_line,
        limit_lines,
    } = action
    else {
        unreachable!()
    };
    // SECURITY: `limit` is model-written. Until 2026-08-15 it *replaced*
    // `max_read_bytes` instead of being bounded by it — `limit: u64::MAX` on a
    // 4 MiB file returned 4 195 453 bytes under a 65 536 policy, and the same
    // number sizes the `vec![0u8; to_read]` in `read_file_inner`, so it was an
    // allocation the model chose too. `ReadFiles` next door had the clamp
    // (`efficient.rs`); this line is the divergence closing.
    let max_bytes = crate::executor::limits::effective_read_limit(*limit, &config.policy);

    // Resolve + validate ONCE, then read through the returned handle. The raw
    // `path` string is never opened again after this point.
    let confined = match confine::resolve_read(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("ReadFile", e.kind(), e.message(), 0),
    };

    // Line mode: coordinates the model can cross-reference with SearchCode
    // results, instead of byte offsets that slice mid-line.
    if let Some(first) = start_line {
        return match read_file_lines(&confined, *first, limit_lines.unwrap_or(200), max_bytes) {
            Ok((content, start, end, total, truncated)) => ActionResult::ok(
                "ReadFile",
                ActionOutput::FileChunk {
                    path: path.clone(),
                    start_line: start,
                    end_line: end,
                    total_lines: total,
                    content,
                    truncated,
                },
                0,
            ),
            Err(e) => ActionResult::err("ReadFile", io_error_kind(&e), e.to_string(), 0),
        };
    }

    let result = read_file_inner(&confined, *offset, max_bytes);
    match result {
        Ok((content, size_bytes, truncated)) => ActionResult::ok(
            "ReadFile",
            ActionOutput::FileContent {
                path: path.clone(),
                content,
                size_bytes,
                truncated,
            },
            0,
        ),
        Err(e) => ActionResult::err("ReadFile", io_error_kind(&e), e.to_string(), 0),
    }
}

/// Line-oriented read: 1-based inclusive window, still bounded by
/// `max_bytes` so a pathological single-line file cannot flood the context.
fn read_file_lines(
    confined: &confine::ConfinedPath,
    start_line: usize,
    limit_lines: usize,
    max_bytes: u64,
) -> io::Result<(String, usize, usize, usize, bool)> {
    let bytes = confined
        .read_bytes()
        .map_err(|e| io::Error::other(e.message()))?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let first = start_line.max(1);
    if first > total && total > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "start_line {} is past the end of the file ({} lines)",
                first, total
            ),
        ));
    }
    let end = (first - 1 + limit_lines.max(1)).min(total);
    let mut out = String::new();
    let mut last_included = first - 1; // 0 lines included yet
    let mut byte_truncated = false;
    for (i, l) in lines[(first - 1)..end].iter().enumerate() {
        if out.len() + l.len() + 1 > max_bytes as usize {
            byte_truncated = true;
            break;
        }
        out.push_str(l);
        out.push('\n');
        last_included = first + i;
    }
    let truncated = byte_truncated || end < total;
    Ok((out, first, last_included, total, truncated))
}

fn read_file_inner(
    confined: &confine::ConfinedPath,
    offset: u64,
    max_bytes: u64,
) -> io::Result<(String, u64, bool)> {
    use std::io::Seek;
    // Opened with O_NOFOLLOW + descriptor identity verification.
    let mut file = confined
        .open_read()
        .map_err(|e| io::Error::other(e.message()))?;
    let size_bytes = file.metadata()?.len();

    if offset > 0 {
        file.seek(io::SeekFrom::Start(offset))?;
    }

    let to_read = max_bytes.min(size_bytes.saturating_sub(offset));
    let mut buf = vec![0u8; to_read as usize];
    let n = file.read(&mut buf)?;
    buf.truncate(n);

    let content = String::from_utf8_lossy(&buf).to_string();
    let truncated = (offset + n as u64) < size_bytes;
    Ok((content, size_bytes, truncated))
}

// ── WriteFile ─────────────────────────────────────────────────────────────────

pub async fn write_file(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::WriteFile { path, content } = action else {
        unreachable!()
    };

    let confined = match confine::resolve_write(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("WriteFile", e.kind(), e.message(), 0),
    };

    match confined
        .write_bytes(content.as_bytes())
        .map_err(|e| io::Error::other(e.message()))
    {
        Ok(()) => ActionResult::ok(
            "WriteFile",
            ActionOutput::FileWritten {
                path: path.clone(),
                bytes_written: content.len() as u64,
            },
            0,
        ),
        Err(e) => ActionResult::err("WriteFile", io_error_kind(&e), e.to_string(), 0),
    }
}

// ── EditFile ──────────────────────────────────────────────────────────────────

fn truncate_preview(text: &str, len: usize) -> String {
    let t = text.replace('\n', "\\n");
    if t.chars().count() > len {
        let truncated: String = t.chars().take(len).collect();
        format!("{}...", truncated)
    } else {
        t
    }
}

/// The replaced region as it now reads, padded to whole lines with one line
/// of context on each side, capped so a large `new` cannot flood the result.
fn snippet_around(content: &str, start: usize, new_len: usize) -> String {
    let end = (start + new_len).min(content.len());
    // Widen to line boundaries, then one extra line each side.
    let line_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_start = content[..line_start.saturating_sub(1)]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = content[end..]
        .find('\n')
        .map(|i| end + i)
        .unwrap_or(content.len());
    let line_end = content[(line_end + 1).min(content.len())..]
        .find('\n')
        .map(|i| line_end + 1 + i)
        .unwrap_or(content.len());
    const CAP: usize = 400;
    let mut s = &content[line_start..line_end];
    if s.len() > CAP {
        let mut cut = CAP;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s = &s[..cut];
    }
    s.to_string()
}

pub async fn edit_file(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::EditFile {
        path,
        edits,
        create_if_missing,
    } = action
    else {
        unreachable!()
    };

    // EditFile mutates, so it is gated by the WRITE roots and resolved once.
    // Everything below operates on `confined`, never on `path`.
    let confined = match confine::resolve_write(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("EditFile", e.kind(), e.message(), 0),
    };

    let mut edits_applied = 0;
    let mut edits_failed = 0;
    use crate::result::EditDetail;
    let mut details = Vec::new();
    let mut rejections: Vec<String> = Vec::new();

    let result = (|| -> io::Result<()> {
        let create = create_if_missing.unwrap_or(false);
        let path_obj = confined.as_path();

        let mut content = if path_obj.exists() {
            let bytes = confined
                .read_bytes()
                .map_err(|e| io::Error::other(e.message()))?;
            String::from_utf8_lossy(&bytes).into_owned()
        } else if create {
            if let Some(first_edit) = edits.first() {
                first_edit.new.clone()
            } else {
                String::new()
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("File not found: {}", path),
            ));
        };

        if create && !path_obj.exists() {
            confined
                .write_bytes(content.as_bytes())
                .map_err(|e| io::Error::other(e.message()))?;
            if !edits.is_empty() {
                edits_applied += 1;
                details.push(EditDetail {
                    old_preview: "<created missing file>".to_string(),
                    replacements: 1,
                    line: Some(1),
                    snippet: None,
                });
            }
            for edit in edits.iter().skip(1) {
                edits_failed += 1;
                details.push(EditDetail {
                    old_preview: truncate_preview(&edit.old, 30),
                    replacements: 0,
                    line: None,
                    snippet: None,
                });
            }
            return Ok(());
        }

        // Atomic: every edit must resolve cleanly against the file BEFORE
        // anything is written. A pattern that matches nothing — or matches
        // more than once without `replace_all` — aborts the whole action
        // with nothing on disk, so the model adjusts and retries instead of
        // discovering later that "success" rewrote the wrong occurrence.
        for edit in edits {
            let replace_all = edit.replace_all.unwrap_or(false);
            let occurrences = content.matches(&edit.old).count();
            match occurrences {
                0 => rejections.push(format!(
                    "`{}` not found in the file",
                    truncate_preview(&edit.old, 60)
                )),
                n if n > 1 && !replace_all => rejections.push(format!(
                    "`{}` matches {} times; include more surrounding context \
                     to make it unique, or set replace_all",
                    truncate_preview(&edit.old, 60),
                    n
                )),
                _ => {}
            }
        }
        if !rejections.is_empty() {
            return Ok(());
        }

        for edit in edits {
            let replace_all = edit.replace_all.unwrap_or(false);
            let occurrences = content.matches(&edit.old).count();
            let replacements = if replace_all { occurrences } else { 1 };
            // Position of the first replacement, before mutating.
            let first_at = content.find(&edit.old).unwrap_or(0);
            let line = content[..first_at].matches('\n').count() + 1;

            if replace_all {
                content = content.replace(&edit.old, &edit.new);
            } else {
                content = content.replacen(&edit.old, &edit.new, 1);
            }

            edits_applied += 1;
            details.push(EditDetail {
                old_preview: truncate_preview(&edit.old, 30),
                replacements,
                line: Some(line),
                snippet: Some(snippet_around(&content, first_at, edit.new.len())),
            });
        }

        confined
            .write_bytes(content.as_bytes())
            .map_err(|e| io::Error::other(e.message()))?;
        Ok(())
    })();

    if let Err(e) = result {
        return ActionResult::err("EditFile", io_error_kind(&e), e.to_string(), 0);
    }

    if !rejections.is_empty() {
        return ActionResult::err(
            "EditFile",
            ErrorKind::InvalidInput,
            format!(
                "No edits applied (the file is untouched): {}",
                rejections.join("; ")
            ),
            0,
        );
    }

    ActionResult::ok(
        "EditFile",
        ActionOutput::FileEdited {
            path: path.clone(),
            edits_applied,
            edits_failed,
            details,
        },
        0,
    )
}

// ── MoveToTrash ───────────────────────────────────────────────────────────────

pub async fn move_to_trash(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::MoveToTrash { path } = action else {
        unreachable!()
    };

    // Removing a file is a mutation: gate it on the WRITE roots, and resolve
    // once so the object trashed is the object validated. Refusing symlinks
    // also stops "trash the link, delete the out-of-root target" games.
    let confined = match confine::resolve_write(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("MoveToTrash", e.kind(), e.message(), 0),
    };

    match move_to_trash_inner(&confined.display()) {
        Ok(()) => ActionResult::ok(
            "MoveToTrash",
            ActionOutput::MovedToTrash { path: path.clone() },
            0,
        ),
        Err(e) => ActionResult::err("MoveToTrash", io_error_kind(&e), e.to_string(), 0),
    }
}

fn move_to_trash_inner(path: &str) -> io::Result<()> {
    let src = Path::new(path);
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Path not found: {}", path),
        ));
    }

    if let Some(home) = dirs::home_dir() {
        let trash_files = home.join(".local/share/Trash/files");
        if trash_files.exists() || stdfs::create_dir_all(&trash_files).is_ok() {
            let file_name = src.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Path has no filename")
            })?;
            let dest = trash_files.join(file_name);
            let dest = unique_dest(dest);
            if rename_or_copy(src, &dest).is_ok() {
                return Ok(());
            }
        }
    }

    let file_name = src.file_name().unwrap_or_default();
    let dest: PathBuf = PathBuf::from("/tmp").join(format!(
        "__x0ne_trash_{}_{}",
        chrono::Utc::now().timestamp(),
        file_name.to_string_lossy()
    ));
    rename_or_copy(src, &dest)
}

/// `std::fs::rename` errors with EXDEV (os 18) when src and dest live on
/// different filesystems — common in CI where /tmp and $HOME are different
/// mounts. Fall back to copy + remove so the agent's "soft delete" works
/// regardless of mount topology.
fn rename_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    match stdfs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            stdfs::copy(src, dest)?;
            stdfs::remove_file(src)
        }
        Err(e) => Err(e),
    }
}

fn unique_dest(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    let stem = dest
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let parent = dest.parent().unwrap_or(Path::new(""));
    let ext = dest
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut i = 1;
    loop {
        let candidate = if ext.is_empty() {
            parent.join(format!("{}_{}", stem, i))
        } else {
            parent.join(format!("{}_{}.{}", stem, i, ext))
        };
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn io_error_kind(e: &io::Error) -> ErrorKind {
    match e.kind() {
        io::ErrorKind::NotFound => ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::ExecutionFailed,
    }
}

#[cfg(test)]
mod confinement_tests {
    //! Filesystem confinement regression suite (August 2026 audit, findings 3
    //! and 4). Every test here encodes an attack that succeeded against the
    //! pre-hardening executor, which validated a canonicalized path in the
    //! policy layer and then re-opened the raw, user-controlled string.
    use super::*;
    use crate::action::Edit;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    /// Config whose read+write roots are exactly `roots`.
    fn cfg_with_roots(roots: &[&Path]) -> AppConfig {
        let mut c = AppConfig::defaults();
        let v: Vec<String> = roots.iter().map(|p| p.to_string_lossy().into()).collect();
        c.policy.allowed_read_paths = v.clone();
        c.policy.allowed_write_paths = v;
        c
    }

    /// Config with NO roots configured at all (the shipped default).
    fn cfg_empty_roots() -> AppConfig {
        let mut c = AppConfig::defaults();
        c.policy.allowed_read_paths = vec![];
        c.policy.allowed_write_paths = vec![];
        c
    }

    fn read(path: &Path) -> Action {
        Action::ReadFile {
            path: path.to_string_lossy().into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        }
    }
    fn write(path: &Path) -> Action {
        Action::WriteFile {
            path: path.to_string_lossy().into(),
            content: "pwned".into(),
        }
    }

    // ── empty allowlist must mean DENY, never "allow anything" ──────────────

    #[tokio::test]
    async fn empty_read_allowlist_denies_reads() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        stdfs::write(&f, "secret").unwrap();
        let res = read_file(&read(&f), &cfg_empty_roots()).await;
        assert!(
            !res.success,
            "an empty allowed_read_paths must deny ALL reads, not allow them"
        );
        assert!(matches!(
            res.error.as_ref().map(|e| &e.kind),
            Some(ErrorKind::PolicyDenied)
        ));
    }

    #[tokio::test]
    async fn empty_write_allowlist_denies_writes() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("out.txt");
        let res = write_file(&write(&f), &cfg_empty_roots()).await;
        assert!(
            !res.success,
            "an empty allowed_write_paths must deny ALL writes"
        );
        assert!(!f.exists(), "denied write must not have touched the disk");
    }

    #[tokio::test]
    async fn explicit_allowed_root_succeeds() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("ok.txt");
        let cfg = cfg_with_roots(&[dir.path()]);
        assert!(write_file(&write(&f), &cfg).await.success);
        assert!(read_file(&read(&f), &cfg).await.success);
    }

    // ── prefix / traversal confusion ────────────────────────────────────────

    #[tokio::test]
    async fn sibling_prefix_confusion_is_denied() {
        // Root "/tmp/x/root" must NOT authorize "/tmp/x/rootEVIL/secret".
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        let sibling = base.path().join("rootEVIL");
        stdfs::create_dir_all(&root).unwrap();
        stdfs::create_dir_all(&sibling).unwrap();
        let victim = sibling.join("secret");
        stdfs::write(&victim, "secret").unwrap();

        let res = read_file(&read(&victim), &cfg_with_roots(&[&root])).await;
        assert!(
            !res.success,
            "a sibling directory sharing a name prefix must not be inside the root"
        );
    }

    #[tokio::test]
    async fn dotdot_traversal_is_denied() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let outside = base.path().join("outside.txt");
        stdfs::write(&outside, "secret").unwrap();

        let escape = root.join("..").join("outside.txt");
        let res = read_file(&read(&escape), &cfg_with_roots(&[&root])).await;
        assert!(!res.success, "`..` must not escape the configured root");
    }

    // ── symlinks ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn symlink_escaping_root_is_denied() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let secret = base.path().join("secret.txt");
        stdfs::write(&secret, "TOPSECRET").unwrap();

        // A symlink INSIDE the root pointing OUTSIDE it.
        let link = root.join("innocent.txt");
        symlink(&secret, &link).unwrap();

        let res = read_file(&read(&link), &cfg_with_roots(&[&root])).await;
        assert!(
            !res.success,
            "a symlink whose target leaves the root must be denied"
        );
        if let Some(ActionOutput::FileContent { content, .. }) = res.output {
            assert!(!content.contains("TOPSECRET"), "leaked out-of-root content");
        }
    }

    #[tokio::test]
    async fn symlink_swap_between_validation_and_use_is_denied() {
        // THE TOCTOU REGRESSION. Policy used to canonicalize and approve one
        // object while the executor later re-opened the raw string; swapping
        // the final component to a symlink in between redirected the I/O.
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let secret = base.path().join("secret.txt");
        stdfs::write(&secret, "TOPSECRET").unwrap();

        let target = root.join("file.txt");
        stdfs::write(&target, "benign").unwrap();

        // Validate the benign, in-root path exactly as a caller would.
        let cfg = cfg_with_roots(&[&root]);
        let action = read(&target);
        assert!(
            read_file(&action, &cfg).await.success,
            "precondition: the in-root file is readable"
        );

        // Attacker swaps the validated name for a symlink pointing outside.
        stdfs::remove_file(&target).unwrap();
        symlink(&secret, &target).unwrap();

        let res = read_file(&action, &cfg).await;
        assert!(
            !res.success,
            "after the swap the same action must fail closed, not follow the symlink"
        );
        if let Some(ActionOutput::FileContent { content, .. }) = res.output {
            assert!(
                !content.contains("TOPSECRET"),
                "TOCTOU swap leaked out-of-root content"
            );
        }
    }

    #[tokio::test]
    async fn write_through_symlink_out_of_root_is_denied() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let victim = base.path().join("victim.txt");
        stdfs::write(&victim, "original").unwrap();

        let link = root.join("innocent.txt");
        symlink(&victim, &link).unwrap();

        let res = write_file(&write(&link), &cfg_with_roots(&[&root])).await;
        assert!(
            !res.success,
            "writing through an escaping symlink must fail"
        );
        assert_eq!(
            stdfs::read_to_string(&victim).unwrap(),
            "original",
            "out-of-root file must be untouched"
        );
    }

    #[tokio::test]
    async fn dangling_symlink_target_behaves_safely() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let link = root.join("dangling.txt");
        symlink(base.path().join("does_not_exist"), &link).unwrap();

        let cfg = cfg_with_roots(&[&root]);
        // Must not panic, must not resolve to something outside the root.
        let res = read_file(&read(&link), &cfg).await;
        assert!(!res.success, "dangling symlink read must fail cleanly");
        let res = write_file(&write(&link), &cfg).await;
        assert!(
            !res.success,
            "writing through a dangling symlink must fail closed rather than \
             creating the out-of-root target"
        );
        assert!(!base.path().join("does_not_exist").exists());
    }

    // ── the invariant must hold across primitives, not just ReadFile ────────

    #[tokio::test]
    async fn edit_file_shares_the_same_confinement() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let victim = base.path().join("victim.txt");
        stdfs::write(&victim, "original").unwrap();
        let link = root.join("innocent.txt");
        symlink(&victim, &link).unwrap();

        let action = Action::EditFile {
            path: link.to_string_lossy().into(),
            edits: vec![Edit {
                old: "original".into(),
                new: "pwned".into(),
                replace_all: None,
            }],
            create_if_missing: Some(false),
        };
        let res = edit_file(&action, &cfg_with_roots(&[&root])).await;
        assert!(!res.success, "EditFile must enforce the same confinement");
        assert_eq!(stdfs::read_to_string(&victim).unwrap(), "original");
    }

    /// A directory walk must not read through a symlink planted inside the
    /// root. walkdir itself does not follow links, but reading the yielded
    /// entry would — so SearchCode could have exfiltrated /etc/shadow via a
    /// link dropped in the workspace.
    #[tokio::test]
    async fn search_does_not_read_through_symlinks_in_the_root() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let secret = base.path().join("secret.txt");
        stdfs::write(&secret, "NEEDLE_TOPSECRET").unwrap();
        symlink(&secret, root.join("innocent.rs")).unwrap();
        // A real in-root file that does NOT contain the needle.
        stdfs::write(root.join("real.rs"), "fn main() {}").unwrap();

        let action = Action::SearchCode {
            root: root.to_string_lossy().into(),
            pattern: "NEEDLE_TOPSECRET".into(),
            patterns: vec![],
            file_glob: None,
            limit: None,
        };
        let cfg = cfg_with_roots(&[&root]);
        let policy = crate::policy::PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        let res = crate::executor::search::search_code(&action, &cfg, &policy).await;
        // NB: assert on the MATCHES, not on the debug dump — the output echoes
        // the search pattern, so the needle string appears there legitimately.
        match res.output {
            Some(ActionOutput::SearchMatches { matches, total, .. }) => {
                assert!(
                    matches.is_empty() && total == 0,
                    "search read through a symlink escaping the root: {matches:?}"
                );
            }
            other => panic!("expected SearchMatches, got {other:?}"),
        }
    }

    /// `Diff{files:true}` reads two agent-supplied paths. It was unconfined at
    /// BOTH layers — absent from the policy engine's path extraction and
    /// reading the raw strings in the executor — so it was a clean two-file
    /// read primitive that ignored the sandbox entirely.
    #[tokio::test]
    async fn diff_files_is_confined() {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        stdfs::create_dir_all(&root).unwrap();
        let inside = root.join("a.txt");
        stdfs::write(&inside, "alpha").unwrap();
        let outside = base.path().join("secret.txt");
        stdfs::write(&outside, "TOPSECRET").unwrap();

        let res = crate::executor::validate::diff(
            &inside.to_string_lossy(),
            &outside.to_string_lossy(),
            true,
            None,
            &cfg_with_roots(&[&root]),
        )
        .await;
        assert!(
            !res.success,
            "Diff must not read a file outside the configured roots"
        );
        let dump = format!("{:?}", res.output);
        assert!(
            !dump.contains("TOPSECRET"),
            "Diff leaked out-of-root content"
        );
    }

    #[tokio::test]
    async fn edit_file_denied_by_empty_allowlist() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        stdfs::write(&f, "original").unwrap();
        let action = Action::EditFile {
            path: f.to_string_lossy().into(),
            edits: vec![Edit {
                old: "original".into(),
                new: "pwned".into(),
                replace_all: None,
            }],
            create_if_missing: Some(false),
        };
        assert!(!edit_file(&action, &cfg_empty_roots()).await.success);
        assert_eq!(stdfs::read_to_string(&f).unwrap(), "original");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Edit;
    use std::path::Path;
    use tempfile::tempdir;

    /// Existing mechanics tests operate on a tempdir; give them an explicit
    /// root now that an empty allowlist means deny-all.
    fn cfg_rooted_at(p: &Path) -> AppConfig {
        let mut c = AppConfig::defaults();
        let r = vec![p.to_string_lossy().into()];
        c.policy.allowed_read_paths = r.clone();
        c.policy.allowed_write_paths = r;
        c
    }

    #[tokio::test]
    async fn read_file_returns_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        stdfs::write(&path, "hello world").unwrap();
        let action = Action::ReadFile {
            path: path.to_string_lossy().into(),
            offset: 0,
            limit: None,
            start_line: None,
            limit_lines: None,
        };
        let res = read_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success);
        if let Some(ActionOutput::FileContent { content, .. }) = res.output {
            assert_eq!(content, "hello world");
        } else {
            panic!("expected FileContent");
        }
    }

    #[tokio::test]
    async fn write_file_creates_and_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let action = Action::WriteFile {
            path: path.to_string_lossy().into(),
            content: "hi".into(),
        };
        let res = write_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success);
        assert_eq!(stdfs::read_to_string(&path).unwrap(), "hi");
    }

    #[tokio::test]
    async fn read_file_line_mode_returns_a_line_window() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.txt");
        stdfs::write(&path, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let action = Action::ReadFile {
            path: path.to_string_lossy().into(),
            offset: 0,
            limit: None,
            start_line: Some(2),
            limit_lines: Some(3),
        };
        let res = read_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success, "{:?}", res.error);
        let json = serde_json::to_value(&res.output).unwrap();
        assert_eq!(json["start_line"], 2);
        assert_eq!(json["end_line"], 4);
        assert_eq!(json["total_lines"], 5);
        assert_eq!(json["content"], "l2\nl3\nl4\n");
        assert_eq!(json["truncated"], true); // l5 exists past the window
    }

    #[tokio::test]
    async fn read_file_line_mode_rejects_start_past_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.txt");
        stdfs::write(&path, "only\n").unwrap();
        let action = Action::ReadFile {
            path: path.to_string_lossy().into(),
            offset: 0,
            limit: None,
            start_line: Some(50),
            limit_lines: None,
        };
        let res = read_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(!res.success);
        assert!(res.error.as_ref().unwrap().message.contains("past the end"));
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match_and_leaves_file_untouched() {
        // The old behavior silently rewrote the FIRST of several matches and
        // reported success — the model then "verified" an edit that landed
        // somewhere it never looked. Ambiguity is now an error, like Claude
        // Code's Edit: add context or say replace_all.
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        stdfs::write(&path, "foo bar foo").unwrap();
        let action = Action::EditFile {
            path: path.to_string_lossy().into(),
            edits: vec![Edit {
                old: "foo".into(),
                new: "qux".into(),
                replace_all: Some(false),
            }],
            create_if_missing: Some(false),
        };
        let res = edit_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(!res.success);
        let msg = res.error.as_ref().unwrap().message.clone();
        assert!(msg.contains("matches 2 times"), "got: {}", msg);
        assert_eq!(stdfs::read_to_string(&path).unwrap(), "foo bar foo");
    }

    #[tokio::test]
    async fn edit_file_unique_match_reports_line_and_snippet() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        stdfs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let action = Action::EditFile {
            path: path.to_string_lossy().into(),
            edits: vec![Edit {
                old: "beta".into(),
                new: "BETA".into(),
                replace_all: None,
            }],
            create_if_missing: Some(false),
        };
        let res = edit_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success, "{:?}", res.error);
        assert_eq!(
            stdfs::read_to_string(&path).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
        let json = serde_json::to_value(&res.output).unwrap();
        let detail = &json["details"][0];
        assert_eq!(detail["line"], 2);
        let snippet = detail["snippet"].as_str().unwrap();
        assert!(snippet.contains("BETA"), "snippet: {}", snippet);
    }

    #[tokio::test]
    async fn edit_file_replace_all_still_replaces_every_occurrence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        stdfs::write(&path, "foo bar foo").unwrap();
        let action = Action::EditFile {
            path: path.to_string_lossy().into(),
            edits: vec![Edit {
                old: "foo".into(),
                new: "qux".into(),
                replace_all: Some(true),
            }],
            create_if_missing: Some(false),
        };
        let res = edit_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success, "{:?}", res.error);
        assert_eq!(stdfs::read_to_string(&path).unwrap(), "qux bar qux");
    }

    #[tokio::test]
    async fn edit_file_atomic_one_bad_edit_blocks_the_good_one() {
        // 2 edits, one resolves and one does not: NOTHING is written. The
        // model gets one error naming the bad pattern, fixes it, retries —
        // instead of a half-applied batch it has to reverse-engineer.
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        stdfs::write(&path, "alpha\nbeta\n").unwrap();
        let action = Action::EditFile {
            path: path.to_string_lossy().into(),
            edits: vec![
                Edit {
                    old: "alpha".into(),
                    new: "ALPHA".into(),
                    replace_all: None,
                },
                Edit {
                    old: "does-not-exist".into(),
                    new: "x".into(),
                    replace_all: None,
                },
            ],
            create_if_missing: Some(false),
        };
        let res = edit_file(&action, &cfg_rooted_at(dir.path())).await;
        assert!(!res.success);
        let msg = res.error.as_ref().unwrap().message.clone();
        assert!(msg.contains("not found"), "got: {}", msg);
        assert_eq!(stdfs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn move_to_trash_relocates_file() {
        let dir = tempdir().unwrap();
        // Unique name per test run so we don't collide with prior runs in the
        // trash dir (the unique_dest helper renames on collision, but having a
        // fresh name keeps the test deterministic and skips any homedir-related
        // edge case on CI without HOME).
        let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let path = dir.path().join(format!("doomed_{}.txt", nonce));
        stdfs::write(&path, "x").unwrap();
        let action = Action::MoveToTrash {
            path: path.to_string_lossy().into(),
        };
        let res = move_to_trash(&action, &cfg_rooted_at(dir.path())).await;
        assert!(res.success, "move_to_trash failed: {:?}", res.error);
        assert!(!path.exists());
    }
}
