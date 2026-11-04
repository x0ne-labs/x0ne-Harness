//! Tamper-evident JSONL journals.
//!
//! Every line carries `seq`, `prev` (the `h` of the line before) and `h`
//! (SHA-256 of the line's own bytes up to `h`). The first line of a chain is a
//! `chain_header` naming the session, the time and the SHA-256 of the binary
//! that wrote it; its `prev` is the SHA-256 of the bytes that precede it in
//! the file (empty for a new journal). Strings are masked BEFORE hashing, so a
//! secret never reaches the file and the hash covers the masked text.
//!
//! A `chain_header` inside a chain resumes it; inside a signed chain it must
//! name the key that signed it, or the verdict is a break at that line.
//!
//! What a result does not cover is printed with it, as notes, by
//! `x0ne audit verify`.

pub mod chain;
pub mod keys;
pub mod redact;
pub mod verify;

pub use chain::{Chain, ChainedFile, Sealed};
pub use keys::Signer;
pub use verify::{verify_file, verify_path, Motif, Report, Status};

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

/// Bytes of a lowercase or uppercase hex string; `None` if malformed.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(Sha256::digest(bytes).as_slice())
}

/// True for a 64-char lowercase hex digest.
pub(crate) fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
