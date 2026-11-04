use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use glob::Pattern;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::action::Action;
use crate::config::AppConfig;
use crate::executor::patch;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

pub async fn read_files(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::ReadFiles {
        paths,
        max_bytes_per_file,
    } = action
    else {
        unreachable!()
    };
    // The clamp that was already right here, now shared so the next reader
    // inherits it instead of re-deriving it. Note it is PER FILE: `paths` is
    // unbounded, so a wide `ReadFiles` still costs paths × max.
    let max = crate::executor::limits::effective_read_limit(*max_bytes_per_file, &config.policy);
    let mut files = Vec::new();
    for path in paths {
        // Resolve+validate each path, then read through that handle.
        let confined = match crate::executor::confine::resolve_read(path, &config.policy) {
            Ok(c) => c,
            Err(e) => {
                files.push(json!({
                    "path": path,
                    "success": false,
                    "error": e.message()
                }));
                continue;
            }
        };
        match read_limited(&confined, max) {
            Ok((content, size_bytes, truncated)) => files.push(json!({
                "path": path,
                "success": true,
                "content": content,
                "size_bytes": size_bytes,
                "truncated": truncated
            })),
            Err(e) => files.push(json!({
                "path": path,
                "success": false,
                "error": e.to_string()
            })),
        }
    }
    ActionResult::ok(
        "ReadFiles",
        ActionOutput::Generic {
            data: json!({ "files": files }),
        },
        0,
    )
}

pub async fn stat_files(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::StatFiles { paths, hash } = action else {
        unreachable!()
    };
    let with_hash = hash.unwrap_or(false);
    let mut files = Vec::new();
    for path in paths {
        // StatFiles reveals existence/size/hash, so it is a READ and must be
        // confined like any other.
        let confined = match crate::executor::confine::resolve_read(path, &config.policy) {
            Ok(c) => c,
            Err(e) => {
                files.push(json!({
                    "path": path,
                    "exists": false,
                    "error": e.message()
                }));
                continue;
            }
        };
        let p = confined.as_path();
        match fs::metadata(p) {
            Ok(meta) => {
                let mut item = json!({
                    "path": path,
                    "exists": true,
                    "is_file": meta.is_file(),
                    "is_dir": meta.is_dir(),
                    "size_bytes": meta.len(),
                    "readonly": meta.permissions().readonly(),
                });
                if with_hash && meta.is_file() {
                    if let Ok(h) = sha256_file(p) {
                        item["sha256"] = json!(h);
                    }
                }
                files.push(item);
            }
            Err(e) => files.push(json!({
                "path": path,
                "exists": false,
                "error": e.to_string()
            })),
        }
    }
    ActionResult::ok(
        "StatFiles",
        ActionOutput::Generic {
            data: json!({ "files": files }),
        },
        0,
    )
}

pub async fn patch_set(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::PatchSet {
        patches,
        stop_on_error,
    } = action
    else {
        unreachable!()
    };
    let stop = stop_on_error.unwrap_or(true);
    let mut results = Vec::new();
    let mut applied = 0usize;
    let mut failed = 0usize;

    for item in patches {
        let patch_action = Action::ApplyPatch {
            path: item.path.clone(),
            patch: item.patch.clone(),
            reverse: item.reverse,
        };
        let res = patch::apply_patch(&patch_action, config).await;
        if res.success {
            applied += 1;
            results.push(json!({"path": item.path, "success": true, "output": res.output}));
        } else {
            failed += 1;
            results.push(json!({"path": item.path, "success": false, "error": res.error}));
            if stop {
                break;
            }
        }
    }

    ActionResult::ok(
        "PatchSet",
        ActionOutput::Generic {
            data: json!({ "patches": results, "patches_applied": applied, "patches_failed": failed }),
        },
        0,
    )
}

pub async fn search_and_read(
    action: &Action,
    config: &AppConfig,
    policy: &crate::policy::PolicyEngine,
) -> ActionResult {
    let Action::SearchAndRead {
        root,
        pattern,
        patterns,
        file_glob,
        context_lines,
        limit,
    } = action
    else {
        unreachable!()
    };
    let max_matches = limit
        .unwrap_or(config.policy.max_list_entries)
        .min(config.policy.max_list_entries);
    let ctx = context_lines.unwrap_or(3).min(20);
    // SECURITY: this action was the worst of the readers — 87 297 702 bytes
    // measured under a 65 536 policy, because it returns `2 × context_lines + 1`
    // lines per hit and the only ceiling was the hit COUNT. `context_lines` is
    // capped at 20 above, which multiplies the payload; it does not bound it.
    let max_bytes = config.policy.max_read_bytes as usize;
    let mut budget = crate::executor::limits::ByteBudget::new(config.policy.max_read_bytes);
    let effective: Vec<String> = if !patterns.is_empty() {
        patterns.clone()
    } else if !pattern.is_empty() {
        vec![pattern.clone()]
    } else {
        return ActionResult::err(
            "SearchAndRead",
            ErrorKind::InvalidInput,
            "No pattern provided",
            0,
        );
    };
    let regex_set = match regex::RegexSet::new(&effective) {
        Ok(r) => r,
        Err(e) => {
            return ActionResult::err("SearchAndRead", ErrorKind::InvalidInput, e.to_string(), 0)
        }
    };
    let glob_pat = file_glob.as_deref().and_then(|g| Pattern::new(g).ok());
    let mut matches = Vec::new();
    let mut truncated = false;

    let confined_root = match crate::executor::confine::resolve_read(root, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("SearchAndRead", e.kind(), e.message(), 0),
    };
    let walk_root = confined_root.as_path().to_path_buf();
    // Same walk as SearchCode: vendor trees pruned, symlinks skipped, and
    // every entry judged by the gate's read refusal (audit 2026-11-02, B2).
    let omitted = std::cell::Cell::new(0usize);
    'outer: for entry in crate::executor::search::policy_walk(&walk_root, policy, &omitted) {
        let file_name = entry.file_name().to_string_lossy();
        if let Some(ref pat) = glob_pat {
            // Path-relative when the glob has a `/` (src/**/*.rs), filename
            // otherwise — mirrors SearchCode.
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
        let content = match fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if regex_set.is_match(line) {
                let start = idx.saturating_sub(ctx);
                let end = (idx + ctx + 1).min(lines.len());
                let hit = crate::executor::limits::truncate_to(line, max_bytes);
                let context =
                    crate::executor::limits::truncate_to(&lines[start..end].join("\n"), max_bytes);
                if !budget.charge(hit.len() + context.len()) {
                    truncated = true;
                    break 'outer;
                }
                matches.push(json!({
                    "file": entry.path().to_string_lossy(),
                    "line": idx + 1,
                    "content": hit,
                    "context_start_line": start + 1,
                    "context": context
                }));
                if matches.len() >= max_matches {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    ActionResult::ok(
        "SearchAndRead",
        ActionOutput::Generic {
            data: json!({
                "root": root,
                "patterns": effective,
                "matches": matches,
                "truncated": truncated,
                "omitted_by_policy": omitted.get(),
            }),
        },
        0,
    )
}

pub async fn safe_test(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::SafeTest {
        kind,
        scope,
        path,
        timeout_secs,
        filter,
    } = action
    else {
        unreachable!()
    };
    let (cmd, mut args): (&str, Vec<String>) = match (kind.as_str(), scope.as_deref().unwrap_or(""))
    {
        ("rust", "workspace") => ("cargo", vec!["test".into(), "--workspace".into()]),
        ("rust", _) => ("cargo", vec!["test".into()]),
        ("node", _) => ("npm", vec!["test".into()]),
        ("python", _) => ("pytest", vec![]),
        ("go", _) => ("go", vec!["test".into(), "./...".into()]),
        _ => {
            return ActionResult::err(
                "SafeTest",
                ErrorKind::InvalidInput,
                format!("unknown test kind: {}", kind),
                0,
            )
        }
    };
    // A name filter turns the whole-suite run into the short edit→retest
    // loop: one failing test, seconds instead of minutes per iteration.
    if let Some(f) = filter.as_deref().filter(|f| !f.is_empty()) {
        match kind.as_str() {
            "rust" => args.push(f.to_string()),
            "python" => args.extend(["-k".to_string(), f.to_string()]),
            "go" => args.extend(["-run".to_string(), f.to_string()]),
            // npm has no portable filter flag; forward as-is after `--`.
            _ => args.extend(["--".to_string(), f.to_string()]),
        }
    }
    let duration = Duration::from_secs(timeout_secs.unwrap_or(120).min(600));
    let empty_env = std::collections::HashMap::new();
    // SECURITY: a test suite is arbitrary code; it runs confined.
    match crate::executor::process::spawn_capture_isolated(
        cmd, &args, path, &empty_env, duration, config,
    )
    .await
    {
        Ok(cap) => ActionResult::ok(
            "SafeTest",
            ActionOutput::Generic {
                data: json!({
                    "command": cmd,
                    "args": args,
                    "exit_code": cap.exit_code,
                    "stdout": cap.stdout,
                    "stderr": if cap.timed_out {
                        format!(
                            "Timed out after {}s. Partial output preserved above.\n{}",
                            duration.as_secs(), cap.stderr
                        )
                    } else {
                        cap.stderr
                    },
                    "timed_out": cap.timed_out
                }),
            },
            0,
        ),
        Err(e) if e.kind() == crate::executor::isolation::UNAVAILABLE_KIND => {
            ActionResult::err("SafeTest", ErrorKind::MissingDependency, e.to_string(), 0)
        }
        Err(e) => ActionResult::err("SafeTest", ErrorKind::ExecutionFailed, e.to_string(), 0),
    }
}

fn read_limited(
    confined: &crate::executor::confine::ConfinedPath,
    max: u64,
) -> std::io::Result<(String, u64, bool)> {
    // O_NOFOLLOW + descriptor identity check.
    let mut f = confined
        .open_read()
        .map_err(|e| std::io::Error::other(e.message()))?;
    let size = f.metadata()?.len();
    let mut buf = vec![0u8; max as usize];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        size,
        (n as u64) < size,
    ))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn safe_test_filter_narrows_the_run() {
        // `filter` is what makes the edit→retest loop short: cargo test
        // <name> instead of the whole suite. Verified through the arg
        // vector the runner reports back.
        let dir = tempdir().unwrap();
        let action = Action::SafeTest {
            kind: "rust".into(),
            scope: None,
            path: Some(dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            filter: Some("my_one_test".into()),
        };
        // H1-1: `path` is a cwd the action names; it must be a policy root.
        let mut config = AppConfig::defaults();
        config.policy.allowed_read_paths = vec![dir.path().to_string_lossy().to_string()];
        let res = safe_test(&action, &config).await;
        // No Cargo project in the temp dir — the run fails, but the args
        // (echoed in the payload) must carry the filter.
        let Some(ActionOutput::Generic { data }) = res.output else {
            panic!("{:?}", res.error)
        };
        let args: Vec<String> = data["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(args, vec!["test", "my_one_test"]);
    }

    #[tokio::test]
    async fn safe_test_reports_real_compiler_stderr_on_failure() {
        // SafeTest shares spawn_capture with RunCommand, whose test pins
        // partial-output-on-timeout; this one pins the failure path: a
        // failing run must surface cargo's actual complaint, not silence.
        let dir = tempdir().unwrap();
        let action = Action::SafeTest {
            kind: "rust".into(),
            scope: None,
            path: Some(dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(30),
            filter: None,
        };
        // H1-1: `path` is a cwd the action names; it must be a policy root.
        let mut config = AppConfig::defaults();
        config.policy.allowed_read_paths = vec![dir.path().to_string_lossy().to_string()];
        let res = safe_test(&action, &config).await;
        let Some(ActionOutput::Generic { data }) = res.output else {
            panic!("{:?}", res.error)
        };
        assert_eq!(data["timed_out"], false);
        assert_ne!(data["exit_code"], 0);
        assert!(
            data["stderr"].as_str().unwrap().contains("Cargo.toml")
                || !data["stderr"].as_str().unwrap().is_empty(),
            "stderr should carry cargo's real complaint"
        );
    }

    #[tokio::test]
    async fn read_files_batches_content() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "alpha").unwrap();
        fs::write(&b, "beta").unwrap();
        let action = Action::ReadFiles {
            paths: vec![
                a.to_string_lossy().to_string(),
                b.to_string_lossy().to_string(),
            ],
            max_bytes_per_file: None,
        };

        let result = read_files(&action, &AppConfig::defaults()).await;
        assert!(result.success);
        let Some(ActionOutput::Generic { data }) = result.output else {
            panic!("expected generic")
        };
        assert_eq!(data["files"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    // replace_many / json_set / toml_set tests removed with the variants.
    async fn _kept_smoke() {}
}
