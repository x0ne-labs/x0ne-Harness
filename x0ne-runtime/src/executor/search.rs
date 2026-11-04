use std::cell::Cell;
use std::path::Path;

use regex::RegexSet;
use walkdir::{DirEntry, WalkDir};

use crate::action::Action;
use crate::config::AppConfig;
use crate::policy::PolicyEngine;
use crate::result::{ActionOutput, ActionResult, CodeMatch, ErrorKind};

/// Build/vendor trees dwarf the source they wrap (this workspace's
/// `target/` alone is GBs); ripgrep skips them via .gitignore, we skip them
/// by name. Walk roots are exempt (depth 0), so searching INSIDE one of
/// these directories on purpose still works.
pub(crate) fn is_vendor_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | "dist"
            | "build"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".cache"
            | "vendor"
    )
}

/// The regular files under `root` that the gate would let `ReadFile` open.
///
/// SECURITY (audit 2026-11-02, B2): the gate judges the walk root only. Every
/// entry below it now meets `PolicyEngine::read_refusal`, the gate's own
/// function: a refused directory is pruned, a refused file skipped, and
/// `omitted` counts both. Vendor trees are pruned below the root, and
/// symlinks are skipped: walkdir does not follow them, reading the entry would.
pub(crate) fn policy_walk<'a>(
    root: &Path,
    policy: &'a PolicyEngine,
    omitted: &'a Cell<usize>,
) -> impl Iterator<Item = DirEntry> + 'a {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(move |e| {
            // Depth 0 is the walk root — never pruned as a vendor tree, so a
            // deliberate search inside e.g. `.git` still works.
            if e.depth() > 0
                && e.file_type().is_dir()
                && is_vendor_dir(&e.file_name().to_string_lossy())
            {
                return false;
            }
            if policy.read_refusal(&e.path().to_string_lossy()).is_some() {
                omitted.set(omitted.get() + 1);
                return false;
            }
            true
        })
        .flatten()
        .filter(|e| !e.file_type().is_dir() && !e.path_is_symlink())
}

/// Search for one or more regex patterns across file contents under a directory.
///
/// `patterns` (Vec) takes precedence over `pattern` (single string). When
/// `patterns` is non-empty, all patterns are compiled into one `RegexSet`
/// and OR-matched in a single walk — no extra round-trips for the LLM.
pub async fn search_code(
    action: &Action,
    config: &AppConfig,
    policy: &PolicyEngine,
) -> ActionResult {
    let Action::SearchCode {
        root,
        pattern,
        patterns,
        file_glob,
        limit,
    } = action
    else {
        unreachable!()
    };

    let max_matches = limit
        .unwrap_or(config.policy.max_list_entries)
        .min(config.policy.max_list_entries);
    // Two ceilings, both needed: `max_matches` bounds how many places are
    // named, `budget` bounds how many bytes come back.
    let max_bytes = config.policy.max_read_bytes as usize;
    let mut budget = crate::executor::limits::ByteBudget::new(config.policy.max_read_bytes);

    // Resolve effective patterns: prefer `patterns` if non-empty, otherwise
    // fall back to the single `pattern` field. Empty input is rejected.
    let effective: Vec<String> = if !patterns.is_empty() {
        patterns.clone()
    } else if !pattern.is_empty() {
        vec![pattern.clone()]
    } else {
        return ActionResult::err(
            "SearchCode",
            ErrorKind::InvalidInput,
            "No pattern provided — supply either `pattern` (string) or `patterns` (array)"
                .to_string(),
            0,
        );
    };

    let regex_set = match RegexSet::new(&effective) {
        Ok(r) => r,
        Err(e) => {
            return ActionResult::err(
                "SearchCode",
                ErrorKind::InvalidInput,
                format!("Invalid regex in patterns {:?}: {}", effective, e),
                0,
            )
        }
    };

    let glob_pat = file_glob
        .as_deref()
        .and_then(|g| glob::Pattern::new(g).ok());

    let mut matches: Vec<CodeMatch> = Vec::new();
    let mut truncated = false;

    // Confine the walk root once; every entry below is then inside it.
    let confined_root = match crate::executor::confine::resolve_read(root, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("SearchCode", e.kind(), e.message(), 0),
    };
    let walk_root = confined_root.as_path().to_path_buf();
    let omitted = Cell::new(0usize);
    'outer: for entry in policy_walk(&walk_root, policy, &omitted) {
        let file_path = entry.path().to_string_lossy().into_owned();
        let file_name = entry.file_name().to_string_lossy();

        // Glob filter: a pattern with `/` matches against the path relative
        // to the walk root (`src/**/*.rs`); a bare pattern keeps the old
        // filename-only semantics (`*.rs`).
        if let Some(ref pat) = glob_pat {
            let hit = if pat.as_str().contains('/') {
                entry
                    .path()
                    .strip_prefix(&walk_root)
                    .map(|rel| pat.matches_path(rel))
                    .unwrap_or(false)
            } else {
                pat.matches(&file_name)
            };
            if !hit {
                continue;
            }
        }

        // Skip binary-ish files (heuristic: check extension)
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if is_likely_binary_ext(ext) {
            continue;
        }

        // Read file lines
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue, // Skip unreadable / binary files gracefully
        };

        for (line_no, line) in content.lines().enumerate() {
            if regex_set.is_match(line) {
                // SECURITY: `max_matches` counts matches, and a count bounds no
                // payload — 512 hits on 4 KiB lines is 2 MiB, measured, under a
                // 65 536 policy. The matched line is shortened rather than
                // dropped so a hit in a minified file still reports its
                // location.
                let content = crate::executor::limits::truncate_to(line, max_bytes);
                if !budget.charge(content.len() + file_path.len()) {
                    truncated = true;
                    break 'outer;
                }
                matches.push(CodeMatch {
                    file: file_path.clone(),
                    line: (line_no + 1) as u64,
                    content,
                });
                if matches.len() >= max_matches {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    // Reported `pattern` field reflects what was actually used: a single
    // pattern stays a string ; multi-pattern is rendered as `(p1|p2|p3)`
    // for human readability — the executor uses RegexSet under the hood.
    let pattern_display = if effective.len() == 1 {
        effective[0].clone()
    } else {
        format!("({})", effective.join("|"))
    };

    let total = matches.len();
    ActionResult::ok(
        "SearchCode",
        ActionOutput::SearchMatches {
            root: root.clone(),
            pattern: pattern_display,
            matches,
            total,
            truncated,
            omitted_by_policy: omitted.get(),
        },
        0,
    )
}

fn is_likely_binary_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "ico"
            | "webp"
            | "svg"
            | "pdf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "zip"
            | "tar"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "wasm"
            | "mp3"
            | "mp4"
            | "wav"
            | "ogg"
            | "mkv"
            | "avi"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
            | "bin"
            | "dat"
    )
}

#[cfg(test)]
mod tests {

    /// SearchCode now confines its walk root; an empty allowlist means
    /// deny-all, so these tests declare the temp dir they search.
    fn tmp_cfg() -> AppConfig {
        let mut c = AppConfig::defaults();
        c.policy.allowed_read_paths = vec![std::env::temp_dir().to_string_lossy().into_owned()];
        c
    }
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// The gate built from the same config, as the dispatcher passes it.
    async fn search(action: &Action, cfg: &AppConfig) -> ActionResult {
        let policy = PolicyEngine::new(cfg.clone(), cfg.actions.clone());
        search_code(action, cfg, &policy).await
    }

    #[tokio::test]
    async fn search_skips_vendor_dirs_but_searches_them_as_root() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("real.rs"), "needle in source\n").unwrap();
        let target = dir.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("junk.rs"), "needle in build output\n").unwrap();

        let action = Action::SearchCode {
            root: dir.path().to_string_lossy().to_string(),
            pattern: "needle".into(),
            patterns: vec![],
            file_glob: None,
            limit: None,
        };
        let res = search(&action, &tmp_cfg()).await;
        let Some(ActionOutput::SearchMatches { matches, .. }) = res.output else {
            panic!()
        };
        assert_eq!(matches.len(), 1, "target/ should be pruned: {:?}", matches);
        assert!(matches[0].file.ends_with("real.rs"));

        // Deliberately rooting the search INSIDE target/ still works.
        let action = Action::SearchCode {
            root: target.to_string_lossy().to_string(),
            pattern: "needle".into(),
            patterns: vec![],
            file_glob: None,
            limit: None,
        };
        let res = search(&action, &tmp_cfg()).await;
        let Some(ActionOutput::SearchMatches { matches, .. }) = res.output else {
            panic!()
        };
        assert_eq!(matches.len(), 1, "explicit root must not be pruned");
    }

    #[tokio::test]
    async fn file_glob_with_slash_matches_relative_path() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src").join("deep");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("inner.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("outer.rs"), "needle\n").unwrap();

        let action = Action::SearchCode {
            root: dir.path().to_string_lossy().to_string(),
            pattern: "needle".into(),
            patterns: vec![],
            file_glob: Some("src/**/*.rs".into()),
            limit: None,
        };
        let res = search(&action, &tmp_cfg()).await;
        let Some(ActionOutput::SearchMatches { matches, .. }) = res.output else {
            panic!()
        };
        assert_eq!(matches.len(), 1, "{:?}", matches);
        assert!(matches[0].file.ends_with("inner.rs"));
    }

    #[tokio::test]
    async fn search_code_supports_multi_pattern() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"),
            "fn ok() { /* TODO: ship */ }\nfn bad() { /* FIXME: bug */ }\nfn meh() { /* XXX hack */ }\nfn fine() {}\n").unwrap();

        let action = Action::SearchCode {
            root: dir.path().to_string_lossy().to_string(),
            pattern: String::new(),
            patterns: vec!["TODO".into(), "FIXME".into(), "XXX".into()],
            file_glob: None,
            limit: None,
        };
        let config = tmp_cfg();

        let res = search(&action, &config).await;
        assert!(res.success, "search_code failed: {:?}", res.error);
        if let Some(ActionOutput::SearchMatches {
            matches,
            total,
            pattern,
            ..
        }) = res.output
        {
            assert_eq!(total, 3, "expected 3 OR-matches, got {}", total);
            assert!(
                pattern.starts_with("(") && pattern.ends_with(")"),
                "multi-pattern display should be `(p1|p2|...)`, got `{}`",
                pattern
            );
            assert!(matches.iter().any(|m| m.content.contains("TODO")));
            assert!(matches.iter().any(|m| m.content.contains("FIXME")));
            assert!(matches.iter().any(|m| m.content.contains("XXX")));
        } else {
            panic!("Expected SearchMatches");
        }
    }

    #[tokio::test]
    async fn search_code_rejects_empty_input() {
        let dir = tempdir().unwrap();
        let action = Action::SearchCode {
            root: dir.path().to_string_lossy().to_string(),
            pattern: String::new(),
            patterns: vec![],
            file_glob: None,
            limit: None,
        };
        let res = search(&action, &tmp_cfg()).await;
        assert!(!res.success);
    }
}
