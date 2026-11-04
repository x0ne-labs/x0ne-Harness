//! Recursive sub-agent tree for X0NE. Prototype, 2026-10-06.
//!
//! Invariants enforced by the orchestrator, never by a node:
//! capabilities and budget only shrink down the tree, the budget is debited
//! from the parent (so total spend <= root), every node acts only inside its
//! own CoW clone under bubblewrap, reports are typed data verified by a
//! verifier command, journals are hash-chained and written here only.

pub mod anthropic;
pub mod budget;
pub mod caps;
pub mod identity;
pub mod journal;
pub mod llm;
pub mod merge;
pub mod mock_http;
pub mod node;
pub mod report;
pub mod sandbox;
pub mod tree;

pub(crate) fn hex(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 15) as usize] as char);
    }
    s
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
