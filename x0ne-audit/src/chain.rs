//! Writing side: mask, number, chain, and optionally sign.
//!
//! A line is `{…fields…,"seq":n,"prev":"<h of line n-1>","h":"<sha256>"}`.
//! `h` is the SHA-256 of the line's bytes before `,"h":`, so the verifier
//! never re-serialises JSON: it hashes the bytes it reads.

use serde_json::{json, Value};

use crate::keys::{head_message, Signer};
use crate::redact::redact_value;
use crate::sha256_hex;

/// `,"h":"` + 64 hex + `"}`.
pub(crate) const H_SUFFIX_LEN: usize = 6 + 64 + 2;

/// One chain inside a journal file.
#[derive(Debug)]
pub struct Chain {
    seq: u64,
    last_h: String,
    header_h: String,
    signer: Option<Signer>,
}

/// What [`Chain::seal`] signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub head_seq: u64,
    pub head_h: String,
    pub public_key: String,
    pub signature: String,
}

impl Chain {
    /// Start a chain after `preceding`, the bytes already in the file.
    /// Returns the chain and its `chain_header` line (with its `\n`).
    pub fn open(
        preceding: &[u8],
        session: &str,
        signer: Option<Signer>,
        unsigned_reason: &str,
    ) -> (Self, String) {
        let mut header = json!({
            "type": "chain_header",
            "v": 1,
            "session": session,
            "ts": chrono::Utc::now().to_rfc3339(),
            "binary_sha256": binary_sha256(),
            "signer": signer.as_ref().map(|s| s.public_hex().to_string()),
        });
        if signer.is_none() {
            header["unsigned"] = json!(unsigned_reason);
        }
        let mut chain = Self {
            seq: 0,
            last_h: sha256_hex(preceding),
            header_h: String::new(),
            signer,
        };
        let line = chain.emit(header, 0);
        chain.header_h = chain.last_h.clone();
        (chain, line)
    }

    /// The next line for `value` (with its `\n`). Every string is masked
    /// before hashing: the secret never reaches the file.
    pub fn line(&mut self, mut value: Value) -> String {
        redact_value(&mut value);
        let seq = self.seq + 1;
        self.emit(value, seq)
    }

    /// A `chain_seal` line signing the current head; `None` without a key.
    pub fn seal(&mut self) -> Option<(String, Sealed)> {
        let signer = self.signer.clone()?;
        let sealed = Sealed {
            head_seq: self.seq,
            head_h: self.last_h.clone(),
            public_key: signer.public_hex().to_string(),
            signature: signer.sign(&head_message(&self.header_h, self.seq, &self.last_h)),
        };
        let v = json!({
            "type": "chain_seal",
            "head_seq": sealed.head_seq,
            "head_h": sealed.head_h,
            "key": sealed.public_key,
            "sig": sealed.signature,
        });
        let seq = self.seq + 1;
        Some((self.emit(v, seq), sealed))
    }

    pub fn is_signed(&self) -> bool {
        self.signer.is_some()
    }

    fn emit(&mut self, value: Value, seq: u64) -> String {
        let mut obj = match value {
            Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        obj.remove("h");
        obj.insert("seq".into(), json!(seq));
        obj.insert("prev".into(), json!(self.last_h));
        let body = serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| "{}".into());
        // `body` ends with `}`; the hashed prefix is everything before it.
        let prefix = &body[..body.len() - 1];
        let h = sha256_hex(prefix.as_bytes());
        let line = format!("{prefix},\"h\":\"{h}\"}}\n");
        self.seq = seq;
        self.last_h = h;
        line
    }
}

/// SHA-256 of the running executable, once per process.
pub fn binary_sha256() -> String {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::read)
            .map(|b| sha256_hex(&b))
            .unwrap_or_else(|e| format!("unknown: {e}"))
    })
    .clone()
}

/// Synchronous append-only chained journal. Opening reads what the file
/// already holds, so the new header's `prev` binds it.
#[derive(Debug)]
pub struct ChainedFile {
    file: std::fs::File,
    chain: Chain,
}

impl ChainedFile {
    /// Signed with `~/.x0ne/keys/journal-ed25519` when it exists.
    pub fn open(path: &std::path::Path, session: &str) -> std::io::Result<Self> {
        let (signer, why) = Signer::load_default();
        Self::open_with(path, session, signer, &why)
    }

    pub fn open_with(
        path: &std::path::Path,
        session: &str,
        signer: Option<Signer>,
        unsigned_reason: &str,
    ) -> std::io::Result<Self> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        let preceding = std::fs::read(path)?;
        let (chain, header) = Chain::open(&preceding, session, signer, unsigned_reason);
        file.write_all(header.as_bytes())?;
        Ok(Self { file, chain })
    }

    pub fn write(&mut self, value: Value) -> std::io::Result<()> {
        use std::io::Write;
        let line = self.chain.line(value);
        self.file.write_all(line.as_bytes())
    }

    pub fn seal(&mut self) -> std::io::Result<Option<Sealed>> {
        use std::io::Write;
        match self.chain.seal() {
            Some((line, s)) => {
                self.file.write_all(line.as_bytes())?;
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_hashes_its_own_prefix_and_links_to_the_last() {
        let (mut c, header) = Chain::open(b"", "s", None, "no key");
        let l1 = c.line(json!({"type": "x", "n": 1}));
        let hv: Value = serde_json::from_str(&header).unwrap();
        let v: Value = serde_json::from_str(&l1).unwrap();
        assert_eq!(hv["prev"], sha256_hex(b""));
        assert_eq!(v["seq"], 1);
        assert_eq!(v["prev"], hv["h"]);
        let t = l1.trim_end();
        assert_eq!(
            sha256_hex(t[..t.len() - H_SUFFIX_LEN].as_bytes()),
            v["h"].as_str().unwrap()
        );
    }

    #[test]
    fn a_secret_is_masked_before_it_is_hashed() {
        let (mut c, _) = Chain::open(b"", "s", None, "no key");
        let l = c.line(
            json!({"text": "export OPENAI_API_KEY=sk-proj-Ab3dEf6hIj9kLmNoPq",
                              "api_key": "plainvalue"}),
        );
        assert!(!l.contains("Ab3dEf6"), "{l}");
        assert!(
            !l.contains("plainvalue"),
            "a sensitive key name masks its value: {l}"
        );
        assert!(l.contains(crate::redact::MASK));
    }
}
