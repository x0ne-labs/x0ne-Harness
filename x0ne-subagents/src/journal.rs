//! Append-only, hash-chained JSONL journal, one file per node, written by
//! the orchestrator only. The journal dir is never bound into a sandbox.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::node::NodeId;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub kind: String,
    pub data: serde_json::Value,
}

impl Event {
    pub fn new(kind: &str, data: serde_json::Value) -> Self {
        Event {
            kind: kind.to_string(),
            data,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub ts_ms: u64,
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub prev: String,
    pub hash: String,
    pub event: Event,
}

pub struct Journal {
    dir: PathBuf,
    chains: Mutex<HashMap<NodeId, (u64, String)>>,
}

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn entry_hash(
    prev: &str,
    seq: u64,
    ts_ms: u64,
    node: &NodeId,
    parent: &Option<NodeId>,
    event: &Event,
) -> String {
    let body = serde_json::json!({
        "seq": seq, "ts_ms": ts_ms, "node": node, "parent": parent, "event": event,
    });
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(serde_json::to_vec(&body).expect("json"));
    crate::hex(h.finalize().as_slice())
}

impl Journal {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Journal {
            dir: dir.to_path_buf(),
            chains: Mutex::new(HashMap::new()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_of(&self, node: &NodeId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", node.file_stem()))
    }

    pub fn append(&self, node: &NodeId, event: Event) -> std::io::Result<Entry> {
        let mut chains = self.chains.lock().expect("journal lock");
        let (seq, prev) = chains
            .get(node)
            .cloned()
            .unwrap_or((0, GENESIS.to_string()));
        let ts_ms = crate::now_ms();
        let parent = node.parent();
        let hash = entry_hash(&prev, seq, ts_ms, node, &parent, &event);
        let entry = Entry {
            seq,
            ts_ms,
            node: node.clone(),
            parent,
            prev,
            hash: hash.clone(),
            event,
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_of(node))?;
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        f.write_all(&line)?;
        f.flush()?;
        chains.insert(node.clone(), (seq + 1, hash));
        Ok(entry)
    }

    pub fn read(&self, node: &NodeId) -> Result<Vec<Entry>, String> {
        let raw = std::fs::read_to_string(self.path_of(node)).map_err(|e| e.to_string())?;
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Entry>(l).map_err(|e| format!("{e}: {l}")))
            .collect()
    }

    /// Recompute every hash from genesis; `Ok(n)` lines verified.
    pub fn verify_chain(&self, node: &NodeId) -> Result<usize, String> {
        let entries = self.read(node)?;
        let mut prev = GENESIS.to_string();
        for (i, e) in entries.iter().enumerate() {
            if e.seq != i as u64 {
                return Err(format!("seq gap at line {i}: {}", e.seq));
            }
            if e.prev != prev {
                return Err(format!("broken link at seq {}", e.seq));
            }
            let h = entry_hash(&e.prev, e.seq, e.ts_ms, &e.node, &e.parent, &e.event);
            if h != e.hash {
                return Err(format!("hash mismatch at seq {}", e.seq));
            }
            prev = e.hash.clone();
        }
        Ok(entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_links_and_detects_tampering() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        let n = NodeId::parse("root/2").unwrap();
        j.append(&n, Event::new("a", serde_json::json!({"x": 1})))
            .unwrap();
        j.append(&n, Event::new("b", serde_json::json!({})))
            .unwrap();
        assert_eq!(j.verify_chain(&n).unwrap(), 2);
        let p = j.path_of(&n);
        let s = std::fs::read_to_string(&p)
            .unwrap()
            .replace("\"x\":1", "\"x\":2");
        std::fs::write(&p, s).unwrap();
        assert!(j.verify_chain(&n).unwrap_err().contains("hash mismatch"));
        let e = j.read(&n).unwrap();
        assert_eq!(e[0].parent, Some(NodeId::root()));
    }
}
