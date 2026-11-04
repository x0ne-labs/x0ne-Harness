//! Connector credentials: resolved at spawn time, handed to the server
//! process only, and masked in everything the model, the log or the screen
//! receives. The secrets file must be mode 0600, or it is refused.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Mask written in place of a known secret value.
pub const MASK: &str = "[secret]";

/// Where `${secret:NAME}` and `${keyring:NAME}` are looked up.
#[derive(Debug, Clone)]
pub struct SecretStore {
    pub file: PathBuf,
    /// `secret-tool` binary; replaced in tests.
    pub keyring_cmd: String,
}

impl Default for SecretStore {
    fn default() -> Self {
        Self {
            file: default_secrets_file(),
            keyring_cmd: "secret-tool".into(),
        }
    }
}

pub fn default_secrets_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("x0ne")
        .join("connector-secrets.toml")
}

impl SecretStore {
    fn file_value(&self, name: &str) -> Result<String, String> {
        let path = &self.file;
        let meta = std::fs::metadata(path).map_err(|_| {
            format!(
                "secret `{name}` not found: {} does not exist (create it, mode 0600, with `{name} = \"…\"`)",
                path.display()
            )
        })?;
        check_private(path, &meta)?;
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // The parse error is not shown: it can quote a line of the file.
        let table: BTreeMap<String, toml::Value> = toml::from_str(&text)
            .map_err(|_| format!("{} is not a flat TOML table of strings", path.display()))?;
        match table.get(name) {
            Some(toml::Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            Some(_) => Err(format!(
                "secret `{name}` in {} is not a string",
                path.display()
            )),
            None => Err(format!(
                "secret `{name}` is missing from {} (add `{name} = \"…\"`)",
                path.display()
            )),
        }
    }

    fn keyring_value(&self, name: &str) -> Result<String, String> {
        let out = std::process::Command::new(&self.keyring_cmd)
            .args(["lookup", "service", "x0ne-connectors", "account", name])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|_| {
                format!(
                    "secret `{name}`: `{}` is not available (libsecret tools)",
                    self.keyring_cmd
                )
            })?;
        let v = String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string();
        if !out.status.success() || v.is_empty() {
            return Err(format!(
                "secret `{name}` is not in the keyring (service x0ne-connectors, account {name})"
            ));
        }
        Ok(v)
    }

    /// Expand every `${secret:…}`, `${keyring:…}` and `${env:…}` in `raw`.
    /// Returns the value and the secret values it used, for masking.
    pub fn expand(&self, raw: &str) -> Result<(String, Vec<String>), String> {
        let mut out = String::new();
        let mut used = Vec::new();
        let mut rest = raw;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find('}') else {
                return Err("unterminated `${` in a connector value".into());
            };
            let inner = &rest[start + 2..start + end];
            let (kind, name) = inner.split_once(':').unwrap_or(("", inner));
            let value = match kind {
                "secret" => self.file_value(name)?,
                "keyring" => self.keyring_value(name)?,
                "env" => std::env::var(name)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("environment variable `{name}` is not set"))?,
                _ => {
                    return Err(format!(
                        "unknown reference `${{{inner}}}` (secret, keyring, env)"
                    ))
                }
            };
            used.push(value.clone());
            out.push_str(&value);
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        Ok((out, used))
    }
}

#[cfg(unix)]
fn check_private(path: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is mode {mode:o}; connector secrets are read only from a 0600 file (chmod 600)",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _meta: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

/// Replaces known secret values. Literal match; values under 4 characters
/// are not registered (they would mask ordinary text).
#[derive(Debug, Clone, Default)]
pub struct Masker {
    values: Vec<String>,
}

impl Masker {
    pub fn add(&mut self, value: &str) {
        if value.chars().count() >= 4 && !self.values.iter().any(|v| v == value) {
            self.values.push(value.to_string());
            // Longest first, so a secret containing another is masked whole.
            self.values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        }
    }

    pub fn mask(&self, s: &str) -> String {
        let mut out = s.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), MASK);
            }
        }
        out
    }

    /// Every string in `v`, and every value under a key that names a secret.
    pub fn mask_value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.mask(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.mask_value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        let masked = if crate::redact::sensitive_name(k) && !x.is_null() {
                            Value::String(MASK.into())
                        } else {
                            self.mask_value(x)
                        };
                        (self.mask(k), masked)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(content: &str, mode: u32) -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.toml");
        std::fs::write(&file, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        (
            dir,
            SecretStore {
                file,
                keyring_cmd: "/nonexistent/secret-tool".into(),
            },
        )
    }

    #[test]
    fn a_secret_is_expanded_from_a_private_file() {
        let (_d, s) = store_with("tok = \"CANARY-abc123\"\n", 0o600);
        let (v, used) = s.expand("Bearer ${secret:tok}").unwrap();
        assert_eq!(v, "Bearer CANARY-abc123");
        assert_eq!(used, vec!["CANARY-abc123".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_readable_secrets_file_is_refused_without_quoting_it() {
        let (_d, s) = store_with("tok = \"CANARY-abc123\"\n", 0o644);
        let err = s.expand("${secret:tok}").unwrap_err();
        assert!(err.contains("0600"), "{err}");
        assert!(!err.contains("CANARY"), "{err}");
    }

    #[test]
    fn missing_secret_and_keyring_errors_name_the_fix_not_the_value() {
        let (_d, s) = store_with("other = \"CANARY-zzz999\"\n", 0o600);
        let err = s.expand("${secret:tok}").unwrap_err();
        assert!(err.contains("tok") && !err.contains("CANARY"), "{err}");
        let err = s.expand("${keyring:tok}").unwrap_err();
        assert!(err.contains("secret-tool"), "{err}");
    }

    #[test]
    fn the_masker_hides_values_and_secret_named_keys() {
        let mut m = Masker::default();
        m.add("CANARY-abc123");
        let v = serde_json::json!({
            "title": "x CANARY-abc123 y",
            "api_token": "anything",
            "nested": ["CANARY-abc123"]
        });
        let s = m.mask_value(&v).to_string();
        assert!(!s.contains("CANARY") && !s.contains("anything"), "{s}");
    }
}
