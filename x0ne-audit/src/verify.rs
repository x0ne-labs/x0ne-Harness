//! Reading side: recompute a journal's chain and report the first break.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::chain::H_SUFFIX_LEN;
use crate::keys::{head_message, verify_signature};
use crate::{is_digest, sha256_hex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motif {
    Modified,
    Deleted,
    Inserted,
    Reordered,
    Truncated,
}

impl Motif {
    pub fn as_str(self) -> &'static str {
        match self {
            Motif::Modified => "modified",
            Motif::Deleted => "deleted",
            Motif::Inserted => "inserted",
            Motif::Reordered => "reordered",
            Motif::Truncated => "truncated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Every chained line verifies.
    Ok,
    /// No chained line at all: a journal written before chaining existed.
    Unchained,
    /// First break: 1-based line number, motif, detail.
    Broken {
        line: usize,
        motif: Motif,
        detail: String,
    },
}

#[derive(Debug, Clone)]
pub struct Report {
    pub path: PathBuf,
    pub status: Status,
    /// Non-blank lines in the file.
    pub lines: usize,
    /// Lines carrying `h`.
    pub chained: usize,
    /// Unchained lines before the first header (an old journal resumed).
    pub legacy_prefix: usize,
    /// `Some(pubkey)` when the head signature was checked and is valid.
    pub signed_by: Option<String>,
    /// What this result does not cover, said plainly.
    pub notes: Vec<String>,
}

pub fn verify_file(path: &Path, trusted_public: Option<&str>) -> std::io::Result<Report> {
    let bytes = std::fs::read(path)?;
    Ok(verify_bytes(path, &bytes, trusted_public))
}

/// One physical line: 1-based number, byte offset, text, ended by `\n`.
struct Line<'a> {
    no: usize,
    offset: usize,
    text: &'a [u8],
    complete: bool,
}

fn split_lines(bytes: &[u8]) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut no = 0;
    while offset < bytes.len() {
        no += 1;
        let (end, complete) = match bytes[offset..].iter().position(|b| *b == b'\n') {
            Some(i) => (offset + i, true),
            None => (bytes.len(), false),
        };
        out.push(Line {
            no,
            offset,
            text: &bytes[offset..end],
            complete,
        });
        offset = end + 1;
    }
    out
}

/// `(prefix, h)` when the line ends in `,"h":"<64 hex>"}`.
fn split_h(text: &str) -> Option<(&str, &str)> {
    let cut = text.len().checked_sub(H_SUFFIX_LEN)?;
    let suffix = text.get(cut..)?;
    let h = suffix.strip_prefix(",\"h\":\"")?.strip_suffix("\"}")?;
    is_digest(h).then(|| (&text[..cut], h))
}

fn seq_of(line: &Line) -> Option<u64> {
    let v: Value = serde_json::from_slice(line.text).ok()?;
    v.get("h")?;
    v.get("seq")?.as_u64()
}

struct Walk<'a> {
    bytes: &'a [u8],
    trusted: Option<&'a str>,
    report: Report,
    in_chain: bool,
    expected: u64,
    last_h: String,
    header_h: String,
    chain_signed: bool,
    /// The key the current chain's header names, when it names one.
    chain_key: Option<String>,
    unsigned_reason: Option<String>,
    since_seal: usize,
    sealed_once: bool,
}

impl Walk<'_> {
    fn brk(&mut self, line: usize, motif: Motif, detail: impl Into<String>) {
        self.report.status = Status::Broken {
            line,
            motif,
            detail: detail.into(),
        };
    }

    /// `false` once the chain is broken.
    fn step(&mut self, lines: &[Line], i: usize) -> bool {
        let line = &lines[i];
        let truncated = !line.complete && i + 1 == lines.len();
        let Ok(text) = std::str::from_utf8(line.text) else {
            if truncated {
                self.brk(line.no, Motif::Truncated, "last line cut mid-character");
            } else if self.in_chain {
                self.brk(line.no, Motif::Modified, "not UTF-8");
            } else {
                self.report.legacy_prefix += 1;
                return true;
            }
            return false;
        };
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) if truncated && (self.in_chain || text.contains("\"seq\"")) => {
                self.brk(line.no, Motif::Truncated, "last line is incomplete JSON");
                return false;
            }
            Err(_) if self.in_chain => {
                self.brk(line.no, Motif::Modified, "not JSON");
                return false;
            }
            Err(_) => {
                self.report.legacy_prefix += 1;
                return true;
            }
        };
        if value.get("h").is_none() {
            if self.in_chain {
                self.brk(
                    line.no,
                    Motif::Inserted,
                    "an unchained line inside the chain",
                );
                return false;
            }
            self.report.legacy_prefix += 1;
            return true;
        }
        self.report.chained += 1;
        let Some((prefix, h)) = split_h(text) else {
            self.brk(
                line.no,
                Motif::Modified,
                "`h` is not the last field, or malformed",
            );
            return false;
        };
        if sha256_hex(prefix.as_bytes()) != h {
            self.brk(line.no, Motif::Modified, "content does not match its hash");
            return false;
        }
        let seq = value.get("seq").and_then(Value::as_u64);
        let prev = value.get("prev").and_then(Value::as_str).unwrap_or("");
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");

        if kind == "chain_header" && seq == Some(0) {
            if prev != sha256_hex(&self.bytes[..line.offset]) {
                self.brk(
                    line.no,
                    Motif::Modified,
                    "the bytes before this chain header changed",
                );
                return false;
            }
            let signer = value
                .get("signer")
                .and_then(Value::as_str)
                .map(str::to_string);
            if self.in_chain {
                // A header inside a chain resumes it. Audit of 2026-10-30,
                // H4-6: a signed chain is resumed by its key or not at all,
                // and the lines after the last signed head stay counted
                // across the resume; before, the header reset the count and
                // a journal cut after its seal and continued by anyone read
                // "OK, signed by <key>".
                if let Some(key) = &self.chain_key {
                    let same = signer
                        .as_deref()
                        .is_some_and(|k| k.eq_ignore_ascii_case(key));
                    if !same {
                        self.brk(
                            line.no,
                            Motif::Inserted,
                            format!(
                                "a chain header without the key {key} that signed this \
                                 journal: the lines from here on were written by someone else"
                            ),
                        );
                        return false;
                    }
                }
                self.report.notes.push(format!(
                    "chain resumed at line {}: {} line(s) before it are not covered by a seal",
                    line.no, self.since_seal
                ));
                self.since_seal += 1;
            } else {
                self.since_seal = 0;
            }
            self.in_chain = true;
            self.expected = 1;
            self.last_h = h.to_string();
            self.header_h = h.to_string();
            self.chain_signed = signer.is_some();
            self.chain_key = signer;
            self.unsigned_reason = value
                .get("unsigned")
                .and_then(Value::as_str)
                .map(str::to_string);
            return true;
        }
        if !self.in_chain {
            self.brk(
                line.no,
                Motif::Deleted,
                "chained line with no chain header before it",
            );
            return false;
        }
        let Some(seq) = seq else {
            self.brk(line.no, Motif::Modified, "no `seq`");
            return false;
        };
        if seq > self.expected {
            let want = self.expected;
            if lines[i + 1..].iter().any(|l| seq_of(l) == Some(want)) {
                self.brk(
                    line.no,
                    Motif::Reordered,
                    format!("seq {seq} where {want} was expected; {want} comes later"),
                );
            } else {
                self.brk(
                    line.no,
                    Motif::Deleted,
                    format!(
                        "seq {seq} where {want} was expected: {} line(s) missing",
                        seq - want
                    ),
                );
            }
            return false;
        }
        if seq < self.expected {
            self.brk(
                line.no,
                Motif::Inserted,
                format!("seq {seq} again, {} expected", self.expected),
            );
            return false;
        }
        if prev != self.last_h {
            self.brk(
                line.no,
                Motif::Modified,
                format!(
                    "`prev` does not match line {}: that line was rewritten",
                    line.no - 1
                ),
            );
            return false;
        }
        if kind == "chain_seal" && !self.check_seal(line.no, &value, seq) {
            return false;
        }
        self.since_seal = if kind == "chain_seal" {
            0
        } else {
            self.since_seal + 1
        };
        self.expected += 1;
        self.last_h = h.to_string();
        true
    }

    fn check_seal(&mut self, no: usize, v: &Value, seq: u64) -> bool {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (key, sig, head_h) = (s("key"), s("sig"), s("head_h"));
        let head_seq = v.get("head_seq").and_then(Value::as_u64);
        if head_seq != Some(seq - 1) || head_h != self.last_h {
            self.brk(
                no,
                Motif::Modified,
                "the seal does not name the line before it",
            );
            return false;
        }
        let msg = head_message(&self.header_h, seq - 1, &head_h);
        if !verify_signature(&key, &msg, &sig) {
            self.brk(no, Motif::Modified, "head signature does not verify");
            return false;
        }
        if let Some(t) = self.trusted {
            if !t.eq_ignore_ascii_case(&key) {
                self.brk(
                    no,
                    Motif::Modified,
                    format!("head signed by {key}, not by the trusted key {t}"),
                );
                return false;
            }
        }
        self.sealed_once = true;
        self.report.signed_by = Some(key);
        true
    }
}

fn verify_bytes(path: &Path, bytes: &[u8], trusted: Option<&str>) -> Report {
    let lines = split_lines(bytes);
    let mut w = Walk {
        bytes,
        trusted,
        report: Report {
            path: path.to_path_buf(),
            status: Status::Ok,
            lines: lines.iter().filter(|l| !l.text.is_empty()).count(),
            chained: 0,
            legacy_prefix: 0,
            signed_by: None,
            notes: vec![],
        },
        in_chain: false,
        expected: 0,
        last_h: String::new(),
        header_h: String::new(),
        chain_signed: false,
        chain_key: None,
        unsigned_reason: None,
        since_seal: 0,
        sealed_once: false,
    };
    for i in 0..lines.len() {
        if lines[i].text.is_empty() {
            continue;
        }
        if !w.step(&lines, i) {
            return w.report;
        }
    }
    if w.report.chained == 0 {
        w.report.status = Status::Unchained;
        w.report
            .notes
            .push("no chained line: written before journals were chained".into());
        return w.report;
    }
    let notes = &mut w.report.notes;
    if w.report.legacy_prefix > 0 {
        notes.push(format!(
            "{} unchained line(s) before the first header: bound by its `prev`, not individually",
            w.report.legacy_prefix
        ));
    }
    if !w.sealed_once && w.chain_signed {
        // Measured 2026-10-06: a signed one-turn journal cut before its seal
        // read "not signed", which hid the cut.
        notes.push(
            "UNSEALED: the header names a signing key but no seal follows; the end \
             was cut, or the first turn never finished"
                .into(),
        );
    } else if !w.sealed_once {
        notes.push(format!(
            "not signed ({}): the chain catches an edit, not a whole rewrite, nor a cut at the end",
            w.unsigned_reason.as_deref().unwrap_or("no seal line")
        ));
    } else if w.since_seal > 0 || !w.chain_signed {
        notes.push(format!(
            "{} line(s) after the last signed head: a cut there is not detectable",
            w.since_seal
        ));
    }
    if w.sealed_once && trusted.is_none() {
        notes.push(
            "signature checked against the key written in the file; no trusted key given".into(),
        );
    }
    w.report
}

/// A file, or every `*.jsonl` under a directory (sorted).
pub fn verify_path(path: &Path, trusted_public: Option<&str>) -> std::io::Result<Vec<Report>> {
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        files
            .iter()
            .map(|f| verify_file(f, trusted_public))
            .collect()
    } else {
        Ok(vec![verify_file(path, trusted_public)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Chain;
    use crate::keys::Signer;
    use serde_json::json;

    fn journal(n: usize) -> Vec<String> {
        let (mut c, h) = Chain::open(b"", "t", None, "no key");
        let mut v = vec![h];
        for i in 0..n {
            v.push(c.line(json!({"type": "action", "i": i})));
        }
        v
    }

    fn check(lines: &[String]) -> Report {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j.jsonl");
        std::fs::write(&p, lines.concat()).unwrap();
        verify_file(&p, None).unwrap()
    }

    fn broken(r: &Report) -> (usize, Motif) {
        match &r.status {
            Status::Broken { line, motif, .. } => (*line, *motif),
            s => panic!("expected a break, got {s:?}"),
        }
    }

    #[test]
    fn an_intact_journal_verifies() {
        let r = check(&journal(4));
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!((r.lines, r.chained), (5, 5));
    }

    #[test]
    fn a_modified_line_is_found() {
        let mut j = journal(4);
        j[2] = j[2].replace("\"i\":1", "\"i\":7");
        assert_eq!(broken(&check(&j)), (3, Motif::Modified));
    }

    #[test]
    fn a_deleted_line_is_found() {
        let mut j = journal(4);
        j.remove(2);
        assert_eq!(broken(&check(&j)), (3, Motif::Deleted));
    }

    #[test]
    fn an_inserted_line_is_found() {
        let mut j = journal(4);
        j.insert(2, "{\"type\":\"action\",\"i\":99}\n".into());
        assert_eq!(broken(&check(&j)), (3, Motif::Inserted));
        let mut j = journal(4);
        let dup = j[2].clone();
        j.insert(3, dup);
        assert_eq!(broken(&check(&j)), (4, Motif::Inserted));
    }

    #[test]
    fn swapped_lines_are_found() {
        let mut j = journal(4);
        j.swap(2, 3);
        assert_eq!(broken(&check(&j)), (3, Motif::Reordered));
    }

    #[test]
    fn a_cut_last_line_is_truncation() {
        let mut j = journal(4);
        let last = j.pop().unwrap();
        j.push(last[..last.len() / 2].to_string());
        assert_eq!(broken(&check(&j)), (5, Motif::Truncated));
    }

    #[test]
    fn an_old_journal_is_unchained_not_broken() {
        let r = check(&["{\"type\":\"user_input\"}\n".to_string()]);
        assert_eq!(r.status, Status::Unchained);
    }

    #[test]
    fn a_resumed_old_journal_binds_its_prefix() {
        let old = "{\"type\":\"user_input\",\"content\":\"hi\"}\n".to_string();
        let (mut c, h) = Chain::open(old.as_bytes(), "t", None, "no key");
        let l = c.line(json!({"type": "x"}));
        let r = check(&[old.clone(), h.clone(), l.clone()]);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(r.legacy_prefix, 1);
        let r = check(&[old.replace("hi", "ho"), h, l]);
        assert_eq!(broken(&r), (2, Motif::Modified));
    }

    #[test]
    fn a_second_chain_in_the_same_file_verifies() {
        let first = journal(2).concat();
        let (mut c, h) = Chain::open(first.as_bytes(), "t", None, "no key");
        let l = c.line(json!({"type": "x"}));
        assert_eq!(check(&[first, h, l]).status, Status::Ok);
    }

    #[test]
    fn a_signed_head_is_checked_against_the_trusted_key() {
        let kd = tempfile::tempdir().unwrap();
        let s = Signer::generate(kd.path()).unwrap();
        let (mut c, h) = Chain::open(b"", "t", Some(s.clone()), "");
        let l = c.line(json!({"type": "x"}));
        let (seal, _) = c.seal().unwrap();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j.jsonl");
        std::fs::write(&p, [h.clone(), l.clone(), seal.clone()].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(r.signed_by.as_deref(), Some(s.public_hex()));

        let other = Signer::generate(tempfile::tempdir().unwrap().path()).unwrap();
        let r = verify_file(&p, Some(other.public_hex())).unwrap();
        assert!(matches!(r.status, Status::Broken { line: 3, .. }), "{r:?}");

        // A rewritten journal re-chained by someone without the key.
        let (mut c2, h2) = Chain::open(b"", "t", None, "");
        let l2 = c2.line(json!({"type": "y"}));
        let forged = seal.replace(
            &serde_json::from_str::<serde_json::Value>(&seal).unwrap()["head_h"]
                .as_str()
                .unwrap()
                .to_string(),
            "0".repeat(64).as_str(),
        );
        std::fs::write(&p, [h2, l2, forged].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert!(matches!(r.status, Status::Broken { .. }), "{r:?}");
    }

    #[test]
    fn a_signed_journal_cut_before_its_first_seal_says_unsealed() {
        let kd = tempfile::tempdir().unwrap();
        let s = Signer::generate(kd.path()).unwrap();
        let (mut c, h) = Chain::open(b"", "t", Some(s.clone()), "");
        let l = c.line(json!({"type": "x"}));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j.jsonl");
        std::fs::write(&p, [h, l].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert!(
            r.notes.iter().any(|n| n.starts_with("UNSEALED")),
            "{:?}",
            r.notes
        );
        assert!(
            !r.notes.iter().any(|n| n.contains("not signed")),
            "{:?}",
            r.notes
        );
    }

    /// Audit of 2026-10-30, H4-6. A sealed journal, cut after its seal and
    /// continued under a new header by someone without the key, verified
    /// "OK, signed by <key>" with nothing said about the resume: the new
    /// header reset the count of lines after the last signed head. A header
    /// that resumes a signed chain names the key that signed it, or it is a
    /// rewrite; one that names it is accepted, and the lines after the last
    /// signed head stay counted across it.
    #[test]
    fn a_signed_journal_resumed_without_the_key_is_a_rewrite() {
        let kd = tempfile::tempdir().unwrap();
        let s = Signer::generate(kd.path()).unwrap();
        let (mut c, h) = Chain::open(b"", "t", Some(s.clone()), "");
        let l = c.line(json!({"type": "x"}));
        let (seal, _) = c.seal().unwrap();
        let sealed = [h, l, seal].concat();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j.jsonl");

        // Someone without the key reopens the file and appends.
        let (mut c2, h2) = Chain::open(sealed.as_bytes(), "t", None, "no key");
        let l2 = c2.line(json!({"type": "action", "forged": true}));
        std::fs::write(&p, [sealed.clone(), h2, l2].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert_eq!(broken(&r), (4, Motif::Inserted), "{r:?}");

        // The key holder resumes: accepted, and the resume is said.
        let (mut c3, h3) = Chain::open(sealed.as_bytes(), "t", Some(s.clone()), "");
        let l3 = c3.line(json!({"type": "action"}));
        std::fs::write(&p, [sealed, h3, l3].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("2 line(s) after the last signed head")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn unsealed_lines_after_the_last_seal_are_said() {
        let kd = tempfile::tempdir().unwrap();
        let s = Signer::generate(kd.path()).unwrap();
        let (mut c, h) = Chain::open(b"", "t", Some(s.clone()), "");
        let l = c.line(json!({"type": "x"}));
        let (seal, _) = c.seal().unwrap();
        let tail = c.line(json!({"type": "z"}));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("j.jsonl");
        std::fs::write(&p, [h, l, seal, tail].concat()).unwrap();
        let r = verify_file(&p, Some(s.public_hex())).unwrap();
        assert_eq!(r.status, Status::Ok);
        assert!(
            r.notes.iter().any(|n| n.contains("1 line")),
            "{:?}",
            r.notes
        );
    }
}
