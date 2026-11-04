use crate::action::Action;
use crate::config::AppConfig;
use crate::result::{ActionOutput, ActionResult, ErrorKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<PatchLine>,
}

pub struct PatchResult {
    pub output: String,
    pub hunks_applied: usize,
    pub hunks_failed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
}

pub async fn apply_patch(action: &Action, config: &AppConfig) -> ActionResult {
    let Action::ApplyPatch {
        path,
        patch,
        reverse,
    } = action
    else {
        unreachable!()
    };

    let is_reverse = reverse.unwrap_or(false);

    // ApplyPatch mutates a file: resolve+validate ONCE against the write roots
    // and operate on that object, never on the raw `path` string again.
    let confined = match crate::executor::confine::resolve_write(path, &config.policy) {
        Ok(c) => c,
        Err(e) => return ActionResult::err("ApplyPatch", e.kind(), e.message(), 0),
    };

    let result = (|| -> Result<PatchResult, ActionError> {
        let hunks = parse_unified_diff(patch).map_err(|e| ActionError {
            kind: ErrorKind::InvalidInput,
            message: format!("Failed to parse patch: {}", e),
        })?;

        if hunks.is_empty() {
            return Err(ActionError {
                kind: ErrorKind::InvalidInput,
                message: "No valid hunks found in patch".to_string(),
            });
        }

        let original_bytes = confined.read_bytes().map_err(|e| ActionError {
            kind: e.kind(),
            message: e.message(),
        })?;
        let original_content = String::from_utf8_lossy(&original_bytes).into_owned();

        let patch_res = apply_hunks(&original_content, &hunks, is_reverse);

        if patch_res.hunks_applied > 0 {
            confined
                .write_bytes(patch_res.output.as_bytes())
                .map_err(|e| ActionError {
                    kind: e.kind(),
                    message: format!("Failed to write patched file: {}", e.message()),
                })?;
        }

        Ok(patch_res)
    })();

    match result {
        Ok(res) => {
            if res.hunks_applied == 0 {
                ActionResult::err(
                    "ApplyPatch",
                    ErrorKind::ExecutionFailed,
                    "All hunks failed to apply.",
                    0,
                )
            } else {
                ActionResult::ok(
                    "ApplyPatch",
                    ActionOutput::PatchApplied {
                        path: path.clone(),
                        hunks_total: res.hunks_applied + res.hunks_failed,
                        hunks_applied: res.hunks_applied,
                        hunks_failed: res.hunks_failed,
                        lines_added: res.lines_added,
                        lines_removed: res.lines_removed,
                    },
                    0,
                )
            }
        }
        Err(e) => ActionResult::err("ApplyPatch", e.kind, e.message, 0),
    }
}

pub fn parse_unified_diff(patch: &str) -> std::result::Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<Hunk> = None;

    for line in patch.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            // Header lines, ignore
            continue;
        }

        if line.starts_with("@@ ") {
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }

            // Parse @@ -old_start,old_count +new_start,new_count @@
            // The count part is optional, defaults to 1 if missing.
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }

            let parse_range = |s: &str| -> (usize, usize) {
                let s = &s[1..]; // Remove prefix (- or +)
                let mut p = s.split(',');
                let start = p.next().unwrap_or("0").parse().unwrap_or(0);
                let count = p.next().map(|c| c.parse().unwrap_or(1)).unwrap_or(1);
                (start, count)
            };

            let (old_start, old_count) = parse_range(parts[1]);
            let (new_start, new_count) = parse_range(parts[2]);

            current_hunk = Some(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(ref mut hunk) = current_hunk {
            if line.starts_with(' ') {
                hunk.lines.push(PatchLine::Context(line[1..].to_string()));
            } else if line.starts_with('+') {
                hunk.lines.push(PatchLine::Add(line[1..].to_string()));
            } else if line.starts_with('-') {
                hunk.lines.push(PatchLine::Remove(line[1..].to_string()));
            } else if line.is_empty() {
                // Empty lines might be context lines with the leading space stripped
                hunk.lines.push(PatchLine::Context(String::new()));
            }
        }
    }

    if let Some(h) = current_hunk {
        hunks.push(h);
    }

    Ok(hunks)
}

pub fn apply_hunks(content: &str, hunks: &[Hunk], reverse: bool) -> PatchResult {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    // Normalizing empty file case.
    if lines.is_empty() && content.is_empty() {
        lines = Vec::new();
    }

    let mut hunks_applied = 0;
    let mut hunks_failed = 0;
    let mut lines_added = 0;
    let mut lines_removed = 0;

    for hunk in hunks {
        let actual_hunk = if reverse {
            let mut rev_lines = Vec::new();
            for l in &hunk.lines {
                match l {
                    PatchLine::Context(c) => rev_lines.push(PatchLine::Context(c.clone())),
                    PatchLine::Add(a) => rev_lines.push(PatchLine::Remove(a.clone())),
                    PatchLine::Remove(r) => rev_lines.push(PatchLine::Add(r.clone())),
                }
            }
            Hunk {
                old_start: hunk.new_start,
                old_count: hunk.new_count,
                new_start: hunk.old_start,
                new_count: hunk.old_count,
                lines: rev_lines,
            }
        } else {
            hunk.clone()
        };

        match apply_single_hunk(&mut lines, &actual_hunk) {
            Ok((added, removed)) => {
                hunks_applied += 1;
                lines_added += added;
                lines_removed += removed;
            }
            Err(_) => {
                hunks_failed += 1;
            }
        }
    }

    let output = if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + if content.ends_with('\n') { "\n" } else { "" }
    };

    PatchResult {
        output,
        hunks_applied,
        hunks_failed,
        lines_added,
        lines_removed,
    }
}

// Applies a single hunk. Uses fuzzy matching to find the context lines.
fn apply_single_hunk(file_lines: &mut Vec<String>, hunk: &Hunk) -> Result<(usize, usize), ()> {
    let expected_old: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|l| match l {
            PatchLine::Context(c) | PatchLine::Remove(c) => Some(c.as_str()),
            PatchLine::Add(_) => None,
        })
        .collect();

    // The start in the hunk is 1-indexed.
    let base_idx = if hunk.old_start == 0 {
        0
    } else {
        hunk.old_start.saturating_sub(1)
    };

    // Fuzzy search: 0 offset, then +/- 1, then +/- 2 up to 20 lines.
    let mut best_match: Option<usize> = None;
    let max_offset = 20;

    if expected_old.is_empty() {
        // Special case: adding lines to an empty file
        if file_lines.is_empty() {
            best_match = Some(0);
        }
    } else {
        for offset in 0..=max_offset {
            // Check positive offset
            let mut check_idx = base_idx.saturating_add(offset);
            if matches_at(file_lines, check_idx, &expected_old) {
                best_match = Some(check_idx);
                break;
            }

            // Check negative offset
            check_idx = base_idx.saturating_sub(offset);
            if matches_at(file_lines, check_idx, &expected_old) {
                best_match = Some(check_idx);
                break;
            }
        }
    }

    if let Some(start_idx) = best_match {
        let mut added = 0;
        let mut removed = 0;
        let mut current_idx = start_idx;

        // Apply edits
        for line in &hunk.lines {
            match line {
                PatchLine::Context(_) => {
                    current_idx += 1;
                }
                PatchLine::Remove(_) => {
                    if current_idx < file_lines.len() {
                        file_lines.remove(current_idx);
                        removed += 1;
                    }
                }
                PatchLine::Add(text) => {
                    let text_copy = if current_idx == file_lines.len()
                        && !text.is_empty()
                        && text.ends_with("\n\\ No newline at end of file")
                    {
                        text.trim_end_matches("\n\\ No newline at end of file")
                            .to_string()
                    } else if text == "\\ No newline at end of file" {
                        continue;
                    } else {
                        text.clone()
                    };

                    if current_idx <= file_lines.len() {
                        file_lines.insert(current_idx, text_copy);
                        current_idx += 1;
                        added += 1;
                    }
                }
            }
        }
        Ok((added, removed))
    } else {
        Err(())
    }
}

fn matches_at(file_lines: &[String], start_idx: usize, expected: &[&str]) -> bool {
    if start_idx + expected.len() > file_lines.len() {
        return false;
    }

    for (i, exp) in expected.iter().enumerate() {
        if file_lines[start_idx + i] != *exp {
            return false;
        }
    }
    true
}

// Temporary struct for action error internally
struct ActionError {
    kind: ErrorKind,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_hunk() {
        let diff = "--- a\n+++ b\n@@ -1,3 +1,3 @@\n context 1\n-removed 1\n+added 1\n context 2\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 3);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 3);
        assert_eq!(h.lines.len(), 4);
        assert!(matches!(h.lines[0], PatchLine::Context(_)));
        assert!(matches!(h.lines[1], PatchLine::Remove(_)));
        assert!(matches!(h.lines[2], PatchLine::Add(_)));
    }

    #[test]
    fn apply_exact_hunk() {
        let content = "line 1\nline 2\nline 3\nline 4";
        let patch = "@@ -2,2 +2,2 @@\n line 2\n-line 3\n+line 3 modified\n";
        let hunks = parse_unified_diff(patch).unwrap();
        let res = apply_hunks(content, &hunks, false);
        assert_eq!(res.hunks_applied, 1);
        assert_eq!(res.output, "line 1\nline 2\nline 3 modified\nline 4");
    }

    #[test]
    fn apply_fuzzy_hunk() {
        let content = "line 1\nline 2\nline 3\nline 4";
        let patch = "@@ -1,2 +1,2 @@\n line 2\n-line 3\n+line 3 mod\n";
        let hunks = parse_unified_diff(patch).unwrap();
        let res = apply_hunks(content, &hunks, false);
        assert_eq!(res.hunks_applied, 1);
        assert_eq!(res.output, "line 1\nline 2\nline 3 mod\nline 4");
    }

    #[test]
    fn reverse_patch() {
        let content = "line 1\nline 2\nline 3 modified\nline 4";
        let patch = "@@ -2,2 +2,2 @@\n line 2\n-line 3\n+line 3 modified\n";
        let hunks = parse_unified_diff(patch).unwrap();
        let res = apply_hunks(content, &hunks, true);
        assert_eq!(res.hunks_applied, 1);
        assert_eq!(res.output, "line 1\nline 2\nline 3\nline 4");
    }
}
