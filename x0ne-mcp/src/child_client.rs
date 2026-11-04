//! A minimal MCP client that spawns a child process and exchanges JSON-RPC
//! frames over its stdin/stdout pipes.
//!
//! Used by x0ne-orchestrator's gateway mode to proxy tool calls through to
//! a downstream x0ne-runtime, and by its connectors (third-party stdio
//! servers, started with [`SpawnOptions`]). Designed to be generic enough that later
//! Phase 5 (meta-MCP across many upstreams) can drop a `Vec<ChildMcpClient>`
//! into the gateway with no further changes.
//!
//! Concurrency model: synchronous request/response. The gateway is itself
//! single-threaded against its upstream client, so we don't need an in-flight
//! request map — each `call()` writes one frame and reads frames until it
//! sees the matching id. Notifications from the child are silently dropped.
//! This keeps the implementation tiny and easy to reason about for the
//! gateway use-case; meta-MCP fan-out can layer concurrency on top later.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse};

/// How to start a child server. `Default` keeps [`ChildMcpClient::spawn`]'s
/// behaviour: inherited environment, nothing added.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Start from an empty environment instead of this process's.
    pub clear_env: bool,
    /// Variables copied from this process when `clear_env` is set.
    pub inherit: Vec<String>,
    /// Variables set on the child, after `inherit`.
    pub env: Vec<(String, String)>,
}

/// Longest frame accepted from a child. Matches the server side's bound.
pub const MAX_CHILD_FRAME_BYTES: usize = crate::transport::MAX_MESSAGE_BYTES;

pub struct ChildMcpClient {
    /// Kept alive — dropped on Drop, kill_on_drop ensures the child dies.
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pub server_info: Option<Value>,
    /// Human-readable name for diagnostics. e.g. "x0ne-runtime".
    pub label: String,
}

impl ChildMcpClient {
    /// Spawn the binary and prepare the stdio pipes. Does not run
    /// `initialize` — call `initialize()` next.
    pub async fn spawn(label: impl Into<String>, cmd: &str, args: &[&str]) -> Result<Self> {
        Self::spawn_with(label, cmd, args, &SpawnOptions::default()).await
    }

    /// [`Self::spawn`] with an explicit environment. With `clear_env`, the
    /// child sees only `env` plus the `inherit` names, so a third-party
    /// server never receives this process's own credentials.
    pub async fn spawn_with(
        label: impl Into<String>,
        cmd: &str,
        args: &[&str],
        opts: &SpawnOptions,
    ) -> Result<Self> {
        let label = label.into();
        let mut command = Command::new(cmd);
        if opts.clear_env {
            command.env_clear();
            for name in &opts.inherit {
                if let Some(v) = std::env::var_os(name) {
                    command.env(name, v);
                }
            }
        }
        command.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            // The command line, not the environment: env values may be secrets.
            .with_context(|| format!("[{}] failed to spawn `{} {}`", label, cmd, args.join(" ")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("[{}] child stdin pipe missing", label))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("[{}] child stdout pipe missing", label))?;

        Ok(Self {
            _child: child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
            server_info: None,
            label,
        })
    }

    /// MCP `initialize` handshake. Stores the result in `server_info`.
    pub async fn initialize(&mut self) -> Result<&Value> {
        let result = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "x0ne-mcp-child-client",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        self.server_info = Some(result);
        // Send the `initialized` notification — required by the MCP spec.
        self.send_notification("notifications/initialized", json!({}))
            .await?;
        Ok(self.server_info.as_ref().unwrap())
    }

    /// MCP `tools/list`. Returns the array of tool schemas.
    pub async fn tools_list(&mut self) -> Result<Vec<Value>> {
        let result = self.call("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools)
    }

    /// MCP `tools/call`. Returns the raw result Value (`{ content: [...], isError: bool }`).
    pub async fn tools_call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.call(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await
    }

    /// Generic JSON-RPC call: write a request, await the matching response.
    /// Notifications received in between are dropped (we have no use for
    /// progress events in the gateway-forward model).
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: method.to_string(),
            params: Some(params),
        };
        let line = serde_json::to_string(&req)?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("[{}] write to child stdin failed", self.label))?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        // Read frames until we see one matching our id. A child should not
        // emit unrelated responses, but to be defensive against future
        // server-initiated notifications we skip anything without a matching id.
        loop {
            let mut buf = String::new();
            let n = (&mut self.stdout)
                .take(MAX_CHILD_FRAME_BYTES as u64)
                .read_line(&mut buf)
                .await
                .with_context(|| format!("[{}] read from child stdout failed", self.label))?;
            if n == 0 {
                bail!(
                    "[{}] child closed stdout while awaiting response to {}",
                    self.label,
                    method
                );
            }
            if n >= MAX_CHILD_FRAME_BYTES && !buf.ends_with('\n') {
                bail!(
                    "[{}] child frame exceeds {} bytes",
                    self.label,
                    MAX_CHILD_FRAME_BYTES
                );
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    // The frame itself is not logged: it may carry a secret.
                    tracing::warn!("[{}] could not parse a child frame: {}", self.label, e);
                    continue;
                }
            };
            // A server-initiated request or notification carries `method`.
            // Read as a response, its `id` could collide with ours.
            if frame.get("method").is_some() {
                tracing::debug!("[{}] dropping a server-initiated frame", self.label);
                continue;
            }
            let resp: JsonRpcResponse = match serde_json::from_value(frame) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("[{}] malformed child response: {}", self.label, e);
                    continue;
                }
            };
            if resp.id != json!(id) {
                tracing::debug!("[{}] dropping unrelated frame id={:?}", self.label, resp.id);
                continue;
            }
            if let Some(err) = resp.error {
                bail!(
                    "[{}] {} failed: {} (code {})",
                    self.label,
                    method,
                    err.message,
                    err.code
                );
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.to_string(),
            params: Some(params),
        };
        let line = serde_json::to_string(&req)?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }
}
