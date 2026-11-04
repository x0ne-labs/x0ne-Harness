//! Memory executor — persistent key/value store for agent context.
//!
//! Storage layout:
//! ```text
//! ~/.x0ne/memory/
//! ├── default/
//! │   ├── key1.json
//! │   └── key2.json
//! └── project_foo/
//!     └── notes.json
//! ```

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::result::{ActionOutput, ActionResult, ErrorKind};

/// Get the memory storage base directory.
/// Override with X0NE_MEMORY_DIR env var (used by tests to avoid HOME race).
fn memory_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("X0NE_MEMORY_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".x0ne")
        .join("memory")
}

/// Sanitize a key to be safe as a filename.
fn safe_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .chars()
        .take(128)
        .collect()
}

/// Sanitize a namespace name.
fn safe_namespace(ns: Option<&str>) -> String {
    let ns = ns.unwrap_or("default");
    ns.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .chars()
        .take(64)
        .collect()
}

/// Remember — persist a key/value pair.
pub async fn remember(key: &str, value: &Value, namespace: Option<&str>) -> ActionResult {
    let ns = safe_namespace(namespace);
    let safe = safe_key(key);

    if safe.is_empty() {
        return ActionResult::err(
            "Remember",
            ErrorKind::InvalidInput,
            "key cannot be empty",
            0,
        );
    }

    let dir = memory_dir().join(&ns);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return ActionResult::err(
            "Remember",
            ErrorKind::ExecutionFailed,
            format!("failed to create memory dir: {}", e),
            0,
        );
    }

    let path = dir.join(format!("{}.json", safe));
    let entry = json!({
        "key": key,
        "value": value,
        "namespace": ns,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });

    match tokio::fs::write(
        &path,
        serde_json::to_string_pretty(&entry).unwrap_or_default(),
    )
    .await
    {
        Ok(_) => ActionResult::ok(
            "Remember",
            ActionOutput::Generic {
                data: json!({
                    "key": key,
                    "namespace": ns,
                    "stored": true,
                    "path": path.display().to_string(),
                }),
            },
            0,
        ),
        Err(e) => ActionResult::err(
            "Remember",
            ErrorKind::ExecutionFailed,
            format!("write failed: {}", e),
            0,
        ),
    }
}

/// Recall — retrieve a stored memory by key.
pub async fn recall(key: &str, namespace: Option<&str>) -> ActionResult {
    let ns = safe_namespace(namespace);
    let safe = safe_key(key);

    if safe.is_empty() {
        return ActionResult::err("Recall", ErrorKind::InvalidInput, "key cannot be empty", 0);
    }

    let path = memory_dir().join(&ns).join(format!("{}.json", safe));

    match tokio::fs::read_to_string(&path).await {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(entry) => {
                let value = entry.get("value").cloned().unwrap_or(Value::Null);
                let updated = entry.get("updated_at").cloned().unwrap_or(Value::Null);
                ActionResult::ok(
                    "Recall",
                    ActionOutput::Generic {
                        data: json!({
                            "key": key,
                            "namespace": ns,
                            "value": value,
                            "updated_at": updated,
                            "found": true,
                        }),
                    },
                    0,
                )
            }
            Err(e) => ActionResult::err(
                "Recall",
                ErrorKind::ExecutionFailed,
                format!("corrupt memory entry: {}", e),
                0,
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ActionResult::ok(
            "Recall",
            ActionOutput::Generic {
                data: json!({
                    "key": key,
                    "namespace": ns,
                    "found": false,
                    "value": null,
                }),
            },
            0,
        ),
        Err(e) => ActionResult::err(
            "Recall",
            ErrorKind::ExecutionFailed,
            format!("read failed: {}", e),
            0,
        ),
    }
}

/// Forget — delete a memory entry.
pub async fn forget(key: &str, namespace: Option<&str>) -> ActionResult {
    let ns = safe_namespace(namespace);
    let safe = safe_key(key);

    if safe.is_empty() {
        return ActionResult::err("Forget", ErrorKind::InvalidInput, "key cannot be empty", 0);
    }

    let path = memory_dir().join(&ns).join(format!("{}.json", safe));

    match tokio::fs::remove_file(&path).await {
        Ok(_) => ActionResult::ok(
            "Forget",
            ActionOutput::Generic {
                data: json!({
                    "key": key,
                    "namespace": ns,
                    "deleted": true,
                }),
            },
            0,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ActionResult::ok(
            "Forget",
            ActionOutput::Generic {
                data: json!({
                    "key": key,
                    "namespace": ns,
                    "deleted": false,
                    "reason": "not found",
                }),
            },
            0,
        ),
        Err(e) => ActionResult::err(
            "Forget",
            ErrorKind::ExecutionFailed,
            format!("delete failed: {}", e),
            0,
        ),
    }
}

/// ListMemory — list all stored keys in a namespace.
pub async fn list_memory(namespace: Option<&str>) -> ActionResult {
    let base = memory_dir();
    let all = namespace.map(|n| n == "*").unwrap_or(false);

    if all {
        // List all namespaces and their keys
        let mut namespaces: Vec<Value> = Vec::new();

        match tokio::fs::read_dir(&base).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        let ns_name = entry.file_name().to_string_lossy().to_string();
                        let keys = list_keys_in_dir(&entry.path()).await;
                        let count = keys.len();
                        namespaces.push(json!({
                            "namespace": ns_name,
                            "keys": keys,
                            "count": count,
                        }));
                    }
                }
            }
            Err(_) => {
                // No memory directory yet
            }
        }

        let total: usize = namespaces
            .iter()
            .filter_map(|n| n["count"].as_u64())
            .map(|c| c as usize)
            .sum();

        ActionResult::ok(
            "ListMemory",
            ActionOutput::Generic {
                data: json!({
                    "namespaces": namespaces,
                    "total_keys": total,
                }),
            },
            0,
        )
    } else {
        let ns = safe_namespace(namespace);
        let dir = base.join(&ns);
        let keys = list_keys_in_dir(&dir).await;
        let count = keys.len();

        ActionResult::ok(
            "ListMemory",
            ActionOutput::Generic {
                data: json!({
                    "namespace": ns,
                    "keys": keys,
                    "count": count,
                }),
            },
            0,
        )
    }
}

/// Helper: list all .json keys in a directory.
async fn list_keys_in_dir(dir: &std::path::Path) -> Vec<Value> {
    let mut keys = Vec::new();

    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                let key = name.trim_end_matches(".json");
                // Try to read the updated_at from the entry
                let updated = if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
                    serde_json::from_str::<Value>(&content)
                        .ok()
                        .and_then(|v| v.get("updated_at").cloned())
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                keys.push(json!({
                    "key": key,
                    "updated_at": updated,
                }));
            }
        }
    }

    keys.sort_by(|a, b| {
        a["key"]
            .as_str()
            .unwrap_or("")
            .cmp(b["key"].as_str().unwrap_or(""))
    });
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    // X0NE_MEMORY_DIR is process-global → serialize tests that mutate it.
    static MEM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set up an isolated memory dir for this test.
    /// Returns TempDir + MutexGuard (both must be kept alive for the test duration).
    fn isolated_memory() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = MEM_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let mem_dir = tmp.path().join(".x0ne").join("memory");
        std::env::set_var("X0NE_MEMORY_DIR", mem_dir.to_str().unwrap());
        (tmp, guard)
    }

    #[tokio::test]
    async fn remember_and_recall() {
        let (_tmp, _lock) = isolated_memory();

        let val = json!({"note": "test data", "count": 42});
        let r = remember("test_key", &val, None).await;
        assert!(r.success, "Remember should succeed");

        let r = recall("test_key", None).await;
        assert!(r.success, "Recall should succeed");
        if let Some(ActionOutput::Generic { data: v }) = &r.output {
            assert_eq!(v["found"], true);
            assert_eq!(v["value"]["note"], "test data");
        } else {
            panic!("Expected Generic output");
        }
    }

    #[tokio::test]
    async fn recall_missing_returns_not_found() {
        let (_tmp, _lock) = isolated_memory();

        let r = recall("nonexistent", None).await;
        assert!(r.success);
        if let Some(ActionOutput::Generic { data: v }) = &r.output {
            assert_eq!(v["found"], false);
        }
    }

    #[tokio::test]
    async fn forget_deletes_key() {
        let (_tmp, _lock) = isolated_memory();

        remember("to_delete", &json!("hello"), None).await;
        let r = forget("to_delete", None).await;
        assert!(r.success);
        if let Some(ActionOutput::Generic { data: v }) = &r.output {
            assert_eq!(v["deleted"], true);
        }

        let r = recall("to_delete", None).await;
        if let Some(ActionOutput::Generic { data: v }) = &r.output {
            assert_eq!(v["found"], false);
        }
    }

    #[tokio::test]
    async fn namespaces_isolate() {
        let (_tmp, _lock) = isolated_memory();

        remember("key", &json!("ns1"), Some("ns1")).await;
        remember("key", &json!("ns2"), Some("ns2")).await;

        let r1 = recall("key", Some("ns1")).await;
        if let Some(ActionOutput::Generic { data: v }) = &r1.output {
            assert_eq!(v["value"], "ns1");
        }

        let r2 = recall("key", Some("ns2")).await;
        if let Some(ActionOutput::Generic { data: v }) = &r2.output {
            assert_eq!(v["value"], "ns2");
        }
    }

    #[tokio::test]
    async fn empty_key_rejected() {
        let r = remember("", &json!("val"), None).await;
        assert!(!r.success);
    }
}
