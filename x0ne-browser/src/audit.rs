//! JSONL append-only audit log of every browser tool invocation.
//!
//! Path: `~/.x0ne/logs/browser/<YYYY-MM-DD>.jsonl`
//! One JSON object per line, append-only — safe to tail in real time.
//!
//! Args are passed through a redactor that masks values whose keys look
//! sensitive (password, token, secret, api_key, …).

use chrono::{Local, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::fs::{DirBuilder, OpenOptions};
use tokio::io::AsyncWriteExt;

static SENSITIVE_KEY: OnceLock<Regex> = OnceLock::new();

fn sensitive_re() -> &'static Regex {
    SENSITIVE_KEY.get_or_init(|| {
        Regex::new(r"(?i)(password|passwd|secret|token|api_?key|auth|credential|bearer)").unwrap()
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: String,
    pub session: String,
    pub tool: String,
    pub args: Value,
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
    pub screenshot_path: Option<String>,
}

impl AuditEntry {
    pub fn new(session: &str, tool: &str, args: Value) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            session: session.to_string(),
            tool: tool.to_string(),
            args: redact_for(tool, &args),
            ok: true,
            code: None,
            error: None,
            screenshot_path: None,
        }
    }
    pub fn fail(mut self, code: &str, msg: &str) -> Self {
        self.ok = false;
        self.code = Some(code.to_string());
        self.error = Some(msg.to_string());
        self
    }
    pub fn with_screenshot(mut self, path: String) -> Self {
        self.screenshot_path = Some(path);
        self
    }
}

pub fn log_path() -> PathBuf {
    let day = Local::now().format("%Y-%m-%d").to_string();
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".x0ne")
        .join("logs")
        .join("browser")
        .join(format!("{day}.jsonl"))
}

pub async fn append(entry: &AuditEntry) -> std::io::Result<()> {
    append_to(&log_path(), entry).await
}

/// One line of `entry` at the end of the log at `path`, created on the way.
pub async fn append_to(path: &std::path::Path, entry: &AuditEntry) -> std::io::Result<()> {
    // The journal names every URL, selector and argument of the session:
    // the owner's alone, whatever the umask. (Audit of 2026-10-30, H4-3.)
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .await?;
    }
    let mut line = serde_json::to_string(entry).unwrap_or_else(|_| String::from("{}"));
    line.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await?;
    // A journal created before this rule keeps its mode until its next line.
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    f.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Keys whose value is a secret by the tool's nature, whatever the key says:
/// what `browser_type` types into the page is the login form's password as
/// often as a search term. (Audit of 2026-10-30, H4-3.)
fn typed_in(tool: &str) -> &'static [&'static str] {
    match tool {
        "browser_type" => &["text"],
        _ => &[],
    }
}

/// [`redact`], plus the keys [`typed_in`] names for this tool.
fn redact_for(tool: &str, args: &Value) -> Value {
    let mut v = redact(args);
    if let Value::Object(map) = &mut v {
        for key in typed_in(tool) {
            if map.get(*key).is_some_and(|x| !x.is_null()) {
                map.insert((*key).to_string(), Value::String("<redacted>".into()));
            }
        }
    }
    v
}

/// Recursively walk a JSON value and replace values of "sensitive" keys
/// (password, token, secret, etc.) with the literal string `<redacted>`.
/// Keys are matched case-insensitively against a regex.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map.iter() {
                if sensitive_re().is_match(k) && !val.is_null() {
                    out.insert(k.clone(), Value::String("<redacted>".into()));
                } else {
                    out.insert(k.clone(), redact(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact).collect()),
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H4-3 (audit of 2026-10-30): `browser_type` logged what the agent typed
    /// into the page. The redactor looks at key names, and the key is `text`:
    /// a password typed into a login form went to the journal in clear.
    #[test]
    fn the_text_typed_into_a_page_is_not_journaled() {
        let e = AuditEntry::new(
            "s",
            "browser_type",
            json!({"selector": "#password", "text": "hunter2", "clear": true}),
        );
        assert_eq!(e.args["text"], "<redacted>", "{}", e.args);
        assert_eq!(e.args["selector"], "#password");
        assert_eq!(e.args["clear"], true);
    }

    /// H4-3: the journal was created with the process umask, 0644 under the
    /// usual 022, in a 0755 directory. It holds every URL, selector and
    /// argument of the session: owner only.
    #[tokio::test]
    async fn the_journal_is_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("x0ne-browser-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("logs").join("day.jsonl");
        append_to(
            &path,
            &AuditEntry::new("s", "browser_navigate", json!({"url": "https://a"})),
        )
        .await
        .unwrap();
        let file = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(file, 0o600, "journal mode {file:o}");
        assert_eq!(dir, 0o700, "journal directory mode {dir:o}");
    }
    use serde_json::json;

    #[test]
    fn redacts_password_field() {
        let r = redact(&json!({"email":"a@b.c","password":"hunter2"}));
        assert_eq!(r["password"], "<redacted>");
        assert_eq!(r["email"], "a@b.c");
    }

    #[test]
    fn redacts_nested_token() {
        let r = redact(&json!({
            "outer": { "API_KEY": "sk-123", "ok": true }
        }));
        assert_eq!(r["outer"]["API_KEY"], "<redacted>");
        assert_eq!(r["outer"]["ok"], true);
    }

    #[test]
    fn case_insensitive_matching() {
        let r = redact(&json!({
            "Password": "x", "auth_token": "y", "BEARER": "z", "name": "alice"
        }));
        assert_eq!(r["Password"], "<redacted>");
        assert_eq!(r["auth_token"], "<redacted>");
        assert_eq!(r["BEARER"], "<redacted>");
        assert_eq!(r["name"], "alice");
    }

    #[test]
    fn arrays_traversed() {
        let r = redact(&json!({"users":[{"password":"x"},{"password":"y"}]}));
        assert_eq!(r["users"][0]["password"], "<redacted>");
        assert_eq!(r["users"][1]["password"], "<redacted>");
    }

    #[test]
    fn null_secret_left_as_null() {
        // Don't replace null — that's not actually a leak.
        let r = redact(&json!({"password": null}));
        assert_eq!(r["password"], Value::Null);
    }
}
