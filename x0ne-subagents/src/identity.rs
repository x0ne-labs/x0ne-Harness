//! Per-node identity issued by the orchestrator. Messages that cross the
//! node boundary are HMAC-SHA256 signed; a node cannot speak for another.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::node::NodeId;

const BLOCK: usize = 64;

#[derive(Clone)]
pub struct NodeKey([u8; 32]);

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeKey(..)")
    }
}

impl NodeKey {
    pub fn generate() -> Self {
        use std::io::Read;
        let mut k = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut k))
            .expect("/dev/urandom");
        NodeKey(k)
    }

    pub fn mac(&self, msg: &[u8]) -> [u8; 32] {
        let mut ipad = [0x36u8; BLOCK];
        let mut opad = [0x5cu8; BLOCK];
        for (i, b) in self.0.iter().enumerate() {
            ipad[i] ^= b;
            opad[i] ^= b;
        }
        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(msg);
        let ih = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(ih.as_slice());
        let oh = outer.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(oh.as_slice());
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signed<T> {
    pub node: NodeId,
    pub payload: T,
    pub mac: [u8; 32],
}

impl<T: Serialize> Signed<T> {
    fn bytes(node: &NodeId, payload: &T) -> Vec<u8> {
        let mut b = node.to_string().into_bytes();
        b.push(b'\n');
        b.extend(serde_json::to_vec(payload).expect("payload serialises"));
        b
    }

    pub fn sign(node: NodeId, key: &NodeKey, payload: T) -> Self {
        let mac = key.mac(&Self::bytes(&node, &payload));
        Signed { node, payload, mac }
    }

    pub fn verify(&self, key: &NodeKey) -> bool {
        let expect = key.mac(&Self::bytes(&self.node, &self.payload));
        let mut diff = 0u8;
        for (a, b) in expect.iter().zip(self.mac.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mut k = [0u8; 32];
        k[..4].copy_from_slice(b"Jefe");
        // RFC 4231 keys are 4 bytes; ours are zero-padded to 32, which HMAC
        // defines as equivalent (keys shorter than the block are zero-padded).
        let mac = NodeKey(k).mac(b"what do ya want for nothing?");
        assert_eq!(
            crate::hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
