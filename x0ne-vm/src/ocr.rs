//! OCR helpers — subprocess wrapper around `tesseract` for `vm_click_text`
//! and `vm_wait_for_text`.
//!
//! Tesseract is invoked with `-c tessedit_create_tsv=1` which produces a
//! tab-separated layout: level, page, block, par, line, word_num, x, y, w,
//! h, conf, text. We only care about word-level rows (level=5) with non-
//! empty text and non-zero confidence.
//!
//! No tesseract crate dep — keeps the build deps light and makes the
//! tesseract version concern OS-level (apt install tesseract-ocr).

use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// One OCR word with its bounding box and confidence (0-100).
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub conf: i32,
}

impl Word {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Run tesseract against a PNG buffer (via stdin) and return the word list.
///
/// Requires `tesseract` binary in PATH. Returns a clear error if missing so
/// the agent can suggest `apt install tesseract-ocr tesseract-ocr-eng`.
pub async fn tesseract_words(png: &[u8]) -> Result<Vec<Word>> {
    let mut child = Command::new("tesseract")
        .args([
            "-l",
            "eng",
            "-c",
            "tessedit_create_tsv=1",
            "stdin",
            "stdout",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "failed to spawn `tesseract` — install it with: \
             sudo apt install tesseract-ocr tesseract-ocr-eng",
        )?;

    {
        let mut stdin = child.stdin.take().context("no stdin on tesseract")?;
        stdin
            .write_all(png)
            .await
            .context("write PNG to tesseract stdin")?;
        // Drop closes stdin so tesseract knows it has all the input.
    }

    let out = child
        .wait_with_output()
        .await
        .context("waiting on tesseract")?;
    if !out.status.success() {
        bail!(
            "tesseract failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let tsv = String::from_utf8_lossy(&out.stdout);
    Ok(parse_tsv(&tsv))
}

/// Parse the TSV output of `tesseract -c tessedit_create_tsv=1`. Pure fn,
/// unit-tested below.
pub fn parse_tsv(tsv: &str) -> Vec<Word> {
    let mut out = Vec::new();
    for (i, line) in tsv.lines().enumerate() {
        if i == 0 || line.is_empty() {
            continue; // header + blank lines
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        // Tesseract TSV columns: level page block par line word x y w h conf text
        let level: i32 = cols[0].parse().unwrap_or(0);
        if level != 5 {
            continue; // word-level only
        }
        let conf: i32 = cols[10].parse().unwrap_or(-1);
        if conf <= 0 {
            continue;
        }
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let x: i32 = cols[6].parse().unwrap_or(0);
        let y: i32 = cols[7].parse().unwrap_or(0);
        let w: i32 = cols[8].parse().unwrap_or(0);
        let h: i32 = cols[9].parse().unwrap_or(0);
        out.push(Word {
            text: text.to_string(),
            x,
            y,
            w,
            h,
            conf,
        });
    }
    out
}

/// Find a word/phrase in an OCR-extracted word list.
/// Matching strategy (in priority order):
///   1. exact case-insensitive equality of a single word
///   2. exact case-insensitive substring of a single word
///   3. consecutive multi-word phrase match (joined by single space)
///
/// `occurrence` (0-indexed) lets the caller disambiguate when multiple matches
/// exist (e.g. two "login" links — first vs second).
pub fn find_match(words: &[Word], target: &str, occurrence: usize) -> Option<MatchedRegion> {
    let needle = target.trim();
    if needle.is_empty() || words.is_empty() {
        return None;
    }
    let needle_lc = needle.to_lowercase();
    let parts: Vec<&str> = needle_lc.split_whitespace().collect();

    let mut hits: Vec<MatchedRegion> = Vec::new();

    if parts.len() == 1 {
        // Single-word case: exact equality first, then substring.
        for w in words {
            if w.text.to_lowercase() == needle_lc {
                hits.push(MatchedRegion::from_single(w));
            }
        }
        if hits.is_empty() {
            for w in words {
                if w.text.to_lowercase().contains(&needle_lc) {
                    hits.push(MatchedRegion::from_single(w));
                }
            }
        }
    } else {
        // Multi-word phrase: slide a window across words and match in order.
        for start in 0..words.len().saturating_sub(parts.len() - 1) {
            let window = &words[start..start + parts.len()];
            let all_match = window
                .iter()
                .zip(parts.iter())
                .all(|(w, p)| w.text.to_lowercase() == *p);
            if all_match {
                hits.push(MatchedRegion::from_span(window));
            }
        }
    }

    hits.into_iter().nth(occurrence)
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRegion {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Average confidence across the matched words.
    pub conf: i32,
}

impl MatchedRegion {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    fn from_single(w: &Word) -> Self {
        Self {
            text: w.text.clone(),
            x: w.x,
            y: w.y,
            w: w.w,
            h: w.h,
            conf: w.conf,
        }
    }

    fn from_span(words: &[Word]) -> Self {
        // Union bbox + average confidence.
        let x = words.iter().map(|w| w.x).min().unwrap_or(0);
        let y = words.iter().map(|w| w.y).min().unwrap_or(0);
        let xr = words.iter().map(|w| w.x + w.w).max().unwrap_or(0);
        let yb = words.iter().map(|w| w.y + w.h).max().unwrap_or(0);
        let conf = words.iter().map(|w| w.conf).sum::<i32>() / words.len().max(1) as i32;
        Self {
            text: words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            x,
            y,
            w: xr - x,
            h: yb - y,
            conf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSV_SAMPLE: &str = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t1280\t800\t-1\t
5\t1\t1\t1\t1\t1\t460\t250\t60\t20\t95\tLogin
5\t1\t1\t1\t1\t2\t540\t250\t130\t20\t91\tcreate
5\t1\t1\t1\t1\t3\t680\t250\t80\t20\t91\taccount
5\t1\t1\t1\t1\t4\t100\t100\t0\t0\t-1\t
5\t1\t1\t1\t1\t5\t100\t100\t40\t20\t0\t
5\t1\t1\t1\t1\t6\t300\t300\t50\t20\t88\temail
5\t1\t1\t1\t1\t7\t300\t500\t90\t20\t90\tpassword
5\t1\t1\t1\t1\t8\t300\t590\t60\t30\t97\tlogin
";

    #[test]
    fn parse_tsv_filters_to_word_level_only() {
        let words = parse_tsv(TSV_SAMPLE);
        // 6 valid words (Login, create, account, email, password, login).
        // The empty-text and zero-conf rows are filtered.
        assert_eq!(words.len(), 6);
        assert_eq!(words[0].text, "Login");
        assert_eq!(words[0].x, 460);
        assert_eq!(words[0].conf, 95);
    }

    #[test]
    fn find_match_prefers_exact_over_substring() {
        let words = parse_tsv(TSV_SAMPLE);
        // "login" matches both "Login" (top) and "login" (bottom button).
        // Both are exact case-insensitive — return first by default.
        let m = find_match(&words, "login", 0).unwrap();
        assert_eq!(m.text, "Login");
        assert_eq!(m.center(), (490, 260));

        // Second occurrence = the bottom button.
        let m2 = find_match(&words, "login", 1).unwrap();
        assert_eq!(m2.text, "login");
        assert_eq!(m2.center(), (330, 605));
    }

    #[test]
    fn find_match_multi_word_phrase() {
        let words = parse_tsv(TSV_SAMPLE);
        let m = find_match(&words, "create account", 0).unwrap();
        assert_eq!(m.text, "create account");
        // Union bbox: x=540, y=250, xr=760, yb=270 → bbox 540,250,220,20
        assert_eq!(m.x, 540);
        assert_eq!(m.y, 250);
        assert_eq!(m.w, 220);
        assert_eq!(m.h, 20);
    }

    #[test]
    fn find_match_substring_fallback() {
        let words = parse_tsv(TSV_SAMPLE);
        // "log" doesn't equal any word but is a substring of "Login" and "login".
        let m = find_match(&words, "log", 0).unwrap();
        assert!(m.text.eq_ignore_ascii_case("login"));
    }

    #[test]
    fn find_match_returns_none_when_missing() {
        let words = parse_tsv(TSV_SAMPLE);
        assert!(find_match(&words, "definitely-not-on-screen", 0).is_none());
        assert!(find_match(&words, "", 0).is_none());
    }
}
