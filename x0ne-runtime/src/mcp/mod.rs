//! MCP server for the runtime.
//!
//! Wire-level types (JSON-RPC, stdio transport) live in the `x0ne-mcp`
//! crate so they can be reused by x0ne-orchestrator. This module
//! contributes the runtime-specific parts: the tool registry built from
//! the `Action` enum, and the dispatch into `executor::execute`.

pub use x0ne_mcp::jsonrpc;
pub use x0ne_mcp::transport;
pub mod registry;
pub mod server;
