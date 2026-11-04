//! Merge is the only exit of a sandbox. The child's diff (work vs base) is
//! filtered by policy, checked for conflicts against the parent's current
//! state, then applied file by file. Never applied as-is.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct MergePolicy {
    pub deny_globs: Vec<glob::Pattern>,
    pub secret_markers: Vec<String>,
    pub max_file_bytes: u64,
    pub allow_new_executables: bool,
}

impl Default for MergePolicy {
    fn default() -> Self {
        let globs = [
            ".env",
            "**/.env",
            "**/.env.*",
            "**/*.pem",
            "**/*.key",
            "**/id_rsa*",
            "**/id_ed25519*",
            ".git/**",
            "**/.git/**",
            "**/.ssh/**",
            "**/*.p12",
            "**/*.pfx",
        ];
        MergePolicy {
            deny_globs: globs
                .iter()
                .map(|g| glob::Pattern::new(g).expect("static glob"))
                .collect(),
            secret_markers: ["-----BEGIN", "AKIA", "sk-ant-", "ghp_", "xoxb-"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_file_bytes: 1 << 20,
            allow_new_executables: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReport {
    pub applied: Vec<String>,
    pub rejected: Vec<(String, String)>,
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fp {
    hash: [u8; 32],
    mode: u32,
    len: u64,
}

fn fingerprint(p: &Path) -> Option<Fp> {
    let meta = std::fs::symlink_metadata(p).ok()?;
    if !meta.is_file() {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(h.finalize().as_slice());
    Some(Fp {
        hash,
        mode: meta.permissions().mode() & 0o777,
        len: meta.len(),
    })
}

fn index(root: &Path) -> BTreeMap<String, Fp> {
    let mut out = BTreeMap::new();
    for e in walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap_or(e.path())
            .to_string_lossy()
            .to_string();
        if let Some(fp) = fingerprint(e.path()) {
            out.insert(rel, fp);
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

/// Relative paths changed between `base` and `work`.
pub fn diff(base: &Path, work: &Path) -> Vec<(String, Change)> {
    let b = index(base);
    let w = index(work);
    let mut out = Vec::new();
    for (p, fp) in &w {
        match b.get(p) {
            None => out.push((p.clone(), Change::Added)),
            Some(bf) if bf != fp => out.push((p.clone(), Change::Modified)),
            _ => {}
        }
    }
    for p in b.keys() {
        if !w.contains_key(p) {
            out.push((p.clone(), Change::Deleted));
        }
    }
    out
}

fn policy_reason(
    policy: &MergePolicy,
    rel: &str,
    work_file: &Path,
    base_fp: Option<&Fp>,
) -> Option<String> {
    let path = Path::new(rel);
    if policy.deny_globs.iter().any(|g| g.matches_path(path)) {
        return Some("forbidden path".into());
    }
    let fp = fingerprint(work_file)?;
    if fp.len > policy.max_file_bytes {
        return Some(format!("file larger than {} bytes", policy.max_file_bytes));
    }
    let bytes = std::fs::read(work_file).unwrap_or_default();
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Some("binary content".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    if let Some(m) = policy
        .secret_markers
        .iter()
        .find(|m| text.contains(m.as_str()))
    {
        return Some(format!("secret marker {m:?}"));
    }
    let was_exec = base_fp.map(|b| b.mode & 0o111 != 0).unwrap_or(false);
    if !policy.allow_new_executables && fp.mode & 0o111 != 0 && !was_exec {
        return Some("new executable bit".into());
    }
    None
}

/// Apply the child's changes into `parent_work`. Conflict = the parent moved
/// the same file since the child was forked (parent != child's base) and the
/// child changed it too: the parent's version is kept, the path reported.
pub fn merge_into(
    child_base: &Path,
    child_work: &Path,
    parent_work: &Path,
    policy: &MergePolicy,
) -> Result<MergeReport, String> {
    let mut rep = MergeReport::default();
    let base_idx = index(child_base);
    for (rel, change) in diff(child_base, child_work) {
        let src = child_work.join(&rel);
        let dst = parent_work.join(&rel);
        let base_fp = base_idx.get(&rel);
        let parent_fp = fingerprint(&dst);
        let parent_moved = match (base_fp, parent_fp.as_ref()) {
            (Some(b), Some(p)) => b != p,
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (None, None) => false,
        };
        if parent_moved && fingerprint(&src).as_ref() != parent_fp.as_ref() {
            rep.conflicts.push(rel.clone());
            continue;
        }
        match change {
            Change::Deleted => {
                let _ = std::fs::remove_file(&dst);
                rep.applied.push(rel);
            }
            Change::Added | Change::Modified => {
                if let Some(why) = policy_reason(policy, &rel, &src, base_fp) {
                    rep.rejected.push((rel, why));
                    continue;
                }
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::copy(&src, &dst).map_err(|e| format!("{rel}: {e}"))?;
                rep.applied.push(rel);
            }
        }
    }
    Ok(rep)
}

pub fn rel_paths(root: &Path) -> Vec<PathBuf> {
    index(root).keys().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn merge_applies_filters_and_reports_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let (base, work, parent) = (d.path().join("b"), d.path().join("w"), d.path().join("p"));
        for r in [&base, &work, &parent] {
            w(&r.join("same.txt"), "same");
            w(&r.join("shared.txt"), "v0");
        }
        w(&work.join("new.txt"), "new");
        w(&work.join("shared.txt"), "child");
        w(&parent.join("shared.txt"), "parent");
        w(&work.join(".env"), "x");
        w(&work.join("k/id_rsa.pem"), "-----BEGIN");
        std::fs::remove_file(work.join("same.txt")).unwrap();
        let rep = merge_into(&base, &work, &parent, &MergePolicy::default()).unwrap();
        assert_eq!(rep.conflicts, vec!["shared.txt"]);
        assert_eq!(
            std::fs::read_to_string(parent.join("shared.txt")).unwrap(),
            "parent"
        );
        assert!(rep.applied.contains(&"new.txt".to_string()));
        assert!(rep.applied.contains(&"same.txt".to_string()));
        assert!(!parent.join("same.txt").exists());
        assert!(rep.rejected.iter().any(|(p, _)| p == ".env"));
        assert!(rep.rejected.iter().any(|(p, _)| p == "k/id_rsa.pem"));
        assert!(!parent.join(".env").exists());
    }
}
