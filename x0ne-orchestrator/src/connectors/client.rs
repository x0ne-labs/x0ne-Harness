//! One MCP client per connector: a child process (stdio) or a streamable
//! HTTP endpoint. Errors never carry environment or header values.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use x0ne_mcp::HttpMcpClient;
use x0ne_mcp::{ChildMcpClient, SpawnOptions};

/// Variables a stdio server inherits; nothing else from this process.
pub const INHERITED_ENV: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "USER", "TERM"];

pub enum McpClient {
    Stdio(Box<ChildMcpClient>),
    Http(HttpMcpClient),
}

impl McpClient {
    pub async fn stdio(
        label: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let opts = SpawnOptions {
            clear_env: true,
            inherit: INHERITED_ENV.iter().map(|s| s.to_string()).collect(),
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut c = ChildMcpClient::spawn_with(label, command, &argv, &opts).await?;
        c.initialize().await?;
        Ok(McpClient::Stdio(Box::new(c)))
    }

    pub async fn http(url: &str, headers: &BTreeMap<String, String>) -> Result<Self> {
        let mut c = HttpMcpClient::new(url, headers)?;
        c.initialize().await?;
        Ok(McpClient::Http(c))
    }

    pub async fn tools_list(&mut self) -> Result<Vec<Value>> {
        match self {
            McpClient::Stdio(c) => c.tools_list().await,
            McpClient::Http(c) => {
                let r = c.call("tools/list", json!({})).await?;
                Ok(r.get("tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default())
            }
        }
    }

    pub async fn tools_call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            McpClient::Stdio(c) => c.tools_call(name, arguments).await,
            McpClient::Http(c) => {
                c.call("tools/call", json!({"name": name, "arguments": arguments}))
                    .await
            }
        }
    }
}
