//! Shared MCP / JSON-RPC plumbing for x0ne-runtime and x0ne-orchestrator.
//!
//! This crate intentionally contains *only* the wire-level concerns:
//! - JSON-RPC 2.0 request/response/error types (`jsonrpc`)
//! - line-delimited stdio transport (`transport`)
//! - streamable-HTTP client (`http_client`), shared by connectors and WebSearch
//!
//! Tool registry, dispatch, and per-server state live in the consuming
//! crate. That keeps this crate dependency-free of any runtime/orchestrator
//! types and makes it trivially reusable for future MCP servers
//! (e.g. an upstream proxy or a meta-MCP gateway).

pub mod child_client;
pub mod http_client;
pub mod jsonrpc;
pub mod transport;

pub use child_client::{ChildMcpClient, SpawnOptions};
pub use http_client::{sse_data, HttpMcpClient};
pub use jsonrpc::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    METHOD_NOT_FOUND, PARSE_ERROR,
};
pub use transport::{read_bounded_line, BoundedLine, StdioTransport};

/// MCP tool annotations. A tool not listed read-only keeps the spec's
/// conservative defaults, written out: it may modify and may destroy.
pub fn tool_hints(read_only: bool) -> serde_json::Value {
    serde_json::json!({ "readOnlyHint": read_only, "destructiveHint": !read_only })
}

/// Fails if a tool lacks annotations or a read-only name is not a tool.
pub fn check_hints(tools: &[serde_json::Value], read_only: &[&str]) -> Result<(), String> {
    for t in tools {
        if !t["annotations"]["readOnlyHint"].is_boolean() {
            return Err(format!("{} has no annotations", t["name"]));
        }
    }
    for n in read_only {
        if !tools.iter().any(|t| t["name"] == *n) {
            return Err(format!("read-only list names {n}, which is not a tool"));
        }
    }
    Ok(())
}
