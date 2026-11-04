//! `x0ne audit verify` and `x0ne audit keygen`.

use anyhow::{Context, Result};
use std::path::Path;
use x0ne_audit::{keys, verify_path, Status};

/// Prints one line per journal; `Ok(true)` if any is broken.
pub fn verify(path: Option<&Path>, key: Option<&str>, json: bool) -> Result<bool> {
    let default = x0ne_orchestrator::session_store::sessions_dir();
    let path = path.unwrap_or(&default);
    let trusted = key
        .map(str::to_string)
        .or_else(|| keys::trusted_public(&keys::keys_dir()));
    let reports =
        verify_path(path, trusted.as_deref()).with_context(|| format!("{}", path.display()))?;
    let mut broken = false;
    for r in &reports {
        let (word, detail) = match &r.status {
            Status::Ok => ("OK", String::new()),
            Status::Unchained => ("UNCHAINED", String::new()),
            Status::Broken {
                line,
                motif,
                detail,
            } => {
                broken = true;
                (
                    "BROKEN",
                    format!(" line {line}: {} ({detail})", motif.as_str()),
                )
            }
        };
        if json {
            let v = serde_json::json!({
                "path": r.path, "status": word.to_lowercase(), "detail": detail.trim(),
                "lines": r.lines, "chained": r.chained, "legacy_prefix": r.legacy_prefix,
                "signed_by": r.signed_by, "notes": r.notes,
            });
            println!("{v}");
            continue;
        }
        let signed = match &r.signed_by {
            Some(k) => format!(", signed {}…", &k[..k.len().min(16)]),
            None => String::new(),
        };
        println!(
            "{word} {}{detail} [{} lines, {} chained{signed}]",
            r.path.display(),
            r.lines,
            r.chained
        );
        for n in &r.notes {
            println!("  note: {n}");
        }
    }
    Ok(broken)
}

pub fn keygen() -> Result<()> {
    let dir = keys::keys_dir();
    let s = keys::Signer::generate(&dir).map_err(anyhow::Error::msg)?;
    println!("{}", s.public_hex());
    eprintln!(
        "key in {}; journals opened from now on are signed",
        dir.join(keys::KEY_FILE).display()
    );
    Ok(())
}
