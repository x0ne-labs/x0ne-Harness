//! The optional ed25519 key that signs a journal's chain head.
//!
//! One key per machine account: `~/.x0ne/keys/journal-ed25519` holds the
//! 32-byte seed in hex, mode 0600, directory 0700; `journal-ed25519.pub` the
//! public key. No key: journals are chained but not signed, and the chain
//! header says so. A key file readable by group or others is refused, not used.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};

pub const KEY_FILE: &str = "journal-ed25519";
pub const PUB_FILE: &str = "journal-ed25519.pub";

/// `~/.x0ne/keys`.
pub fn keys_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x0ne")
        .join("keys")
}

#[derive(Clone)]
pub struct Signer {
    pair: Arc<Ed25519KeyPair>,
    public_hex: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signer(ed25519:{})", self.public_hex)
    }
}

impl Signer {
    fn from_seed(seed: &[u8]) -> Result<Self, String> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| "malformed ed25519 seed".to_string())?;
        let public_hex = crate::hex(pair.public_key().as_ref());
        Ok(Self {
            pair: Arc::new(pair),
            public_hex,
        })
    }

    /// The key in `dir`. `Ok(None)` when there is none; `Err` when there is
    /// one that must not be used (malformed, or readable by others).
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join(KEY_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} is mode {:o}; refused (must be 0600)",
                    path.display(),
                    mode & 0o777
                ));
            }
        }
        let seed = crate::unhex(&text)
            .filter(|s| s.len() == 32)
            .ok_or_else(|| format!("{}: not a 32-byte hex seed", path.display()))?;
        Self::from_seed(&seed).map(Some)
    }

    /// [`Signer::load`] on `~/.x0ne/keys`, folded into what a journal header
    /// records: the signer, or the reason there is none.
    pub fn load_default() -> (Option<Self>, String) {
        let dir = keys_dir();
        match Self::load(&dir) {
            Ok(Some(s)) => (Some(s), String::new()),
            Ok(None) => (
                None,
                format!("no key in {} (x0ne audit keygen)", dir.display()),
            ),
            Err(e) => (None, format!("key refused: {e}")),
        }
    }

    /// Create a new key in `dir`. Refuses to replace an existing one: a new
    /// key silently orphans every head the old one signed.
    pub fn generate(dir: &Path) -> Result<Self, String> {
        let path = dir.join(KEY_FILE);
        if path.exists() {
            return Err(format!("{} already exists; not replaced", path.display()));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut seed = [0u8; 32];
        SystemRandom::new()
            .fill(&mut seed)
            .map_err(|_| "no system randomness".to_string())?;
        let signer = Self::from_seed(&seed)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(format!("{}\n", crate::hex(&seed)).as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        std::fs::write(dir.join(PUB_FILE), format!("{}\n", signer.public_hex))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(signer)
    }

    pub fn public_hex(&self) -> &str {
        &self.public_hex
    }

    pub fn sign(&self, msg: &[u8]) -> String {
        crate::hex(self.pair.sign(msg).as_ref())
    }
}

/// The public key this machine trusts: from the secret key if readable, else
/// from the `.pub` file (a verifier may hold only the public half).
pub fn trusted_public(dir: &Path) -> Option<String> {
    if let Ok(Some(s)) = Signer::load(dir) {
        return Some(s.public_hex);
    }
    let text = std::fs::read_to_string(dir.join(PUB_FILE)).ok()?;
    crate::unhex(&text)
        .filter(|b| b.len() == 32)
        .map(|b| crate::hex(&b))
}

/// What a head signature covers: the chain (its header's `h`), the last
/// `seq` and the last `h`.
pub fn head_message(header_h: &str, seq: u64, h: &str) -> Vec<u8> {
    format!("x0ne-journal-head-v1\n{header_h}\n{seq}\n{h}").into_bytes()
}

pub fn verify_signature(public_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let (Some(pk), Some(sig)) = (crate::unhex(public_hex), crate::unhex(sig_hex)) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, pk)
        .verify(msg, &sig)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_load_sign_verify() {
        let d = tempfile::tempdir().unwrap();
        let s = Signer::generate(d.path()).unwrap();
        let l = Signer::load(d.path()).unwrap().unwrap();
        assert_eq!(s.public_hex(), l.public_hex());
        let sig = l.sign(b"m");
        assert!(verify_signature(s.public_hex(), b"m", &sig));
        assert!(!verify_signature(s.public_hex(), b"n", &sig));
        assert!(
            Signer::generate(d.path()).is_err(),
            "must not replace a key"
        );
        assert_eq!(trusted_public(d.path()).as_deref(), Some(s.public_hex()));
    }

    #[cfg(unix)]
    #[test]
    fn a_key_readable_by_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        Signer::generate(d.path()).unwrap();
        let p = d.path().join(KEY_FILE);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = Signer::load(d.path()).unwrap_err();
        assert!(e.contains("refused"), "{e}");
    }

    #[test]
    fn no_key_is_none_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(Signer::load(d.path()).unwrap().is_none());
    }
}
