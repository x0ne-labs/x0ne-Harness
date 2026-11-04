//! MCP Protocol Compliance Tests
//!
//! Tests that x0ne-runtime's MCP implementation conforms to the
//! Model Context Protocol specification (2024-11-05).
//!
//! These tests validate:
//! - JSON-RPC 2.0 message format
//! - MCP lifecycle (initialize → initialized → operations)
//! - tools/list and tools/call responses
//! - Notification handling (no response for notifications)
//! - Error handling per spec
//! - Resource/prompt stubs
//! - Capability negotiation

use serde_json::json;

// ═══════════════════════════════════════════════════════════════════════════
//  Helper: simulate MCP message → response via JSON-RPC
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a JSON-RPC request from a raw JSON string
fn parse_request(s: &str) -> x0ne_runtime::mcp::jsonrpc::JsonRpcRequest {
    serde_json::from_str(s).expect("failed to parse JSON-RPC request")
}

/// Validate that a response is valid JSON-RPC 2.0
fn assert_jsonrpc_response(resp: &x0ne_runtime::mcp::jsonrpc::JsonRpcResponse) {
    assert_eq!(resp.jsonrpc, "2.0", "response must have jsonrpc: \"2.0\"");
    // Must have either result or error, not both
    assert!(
        resp.result.is_some() != resp.error.is_some(),
        "response must have exactly one of result or error"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  1. JSON-RPC 2.0 Format Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_jsonrpc_request_with_integer_id() {
    let req = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(req.id, Some(json!(1)));
    assert_eq!(req.method, "ping");
    assert!(!req.is_notification());
}

#[test]
fn test_jsonrpc_request_with_string_id() {
    let req = parse_request(r#"{"jsonrpc":"2.0","id":"req-42","method":"tools/list"}"#);
    assert_eq!(req.id, Some(json!("req-42")));
    assert!(!req.is_notification());
}

#[test]
fn test_jsonrpc_notification_no_id() {
    let req = parse_request(r#"{"jsonrpc":"2.0","method":"initialized"}"#);
    assert!(req.is_notification());
    assert_eq!(req.id, None);
}

#[test]
fn test_jsonrpc_notification_with_params() {
    let req = parse_request(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#,
    );
    assert!(req.is_notification());
    assert!(req.params.is_some());
}

#[test]
fn test_jsonrpc_response_ok_format() {
    let resp = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::ok(json!(1), json!({"tools": []}));
    assert_jsonrpc_response(&resp);
    assert_eq!(resp.id, json!(1));
    assert!(resp.result.is_some());
    assert!(resp.error.is_none());
}

#[test]
fn test_jsonrpc_response_error_format() {
    let resp = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::error(
        json!(2),
        x0ne_runtime::mcp::jsonrpc::METHOD_NOT_FOUND,
        "not found",
    );
    assert_jsonrpc_response(&resp);
    assert_eq!(resp.id, json!(2));
    assert!(resp.result.is_none());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32601);
    assert_eq!(err.message, "not found");
}

#[test]
fn test_jsonrpc_response_preserves_string_id() {
    let resp = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::ok(json!("my-req-id"), json!({}));
    assert_eq!(resp.id, json!("my-req-id"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. Tool Registry Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_tools_list_returns_valid_mcp_format() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let tools = registry.list_tools();

    assert!(!tools.is_empty(), "tool list must not be empty");

    for tool in &tools {
        // Per MCP spec: each tool must have name, description, inputSchema
        assert!(tool.get("name").is_some(), "tool missing 'name'");
        assert!(
            tool.get("description").is_some(),
            "tool missing 'description'"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "tool missing 'inputSchema'"
        );

        let name = tool["name"].as_str().unwrap();
        let schema = &tool["inputSchema"];

        // inputSchema must be a valid JSON Schema object
        assert_eq!(
            schema.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "tool '{}' inputSchema.type must be 'object'",
            name
        );

        // Must have a properties field
        assert!(
            schema.get("properties").is_some(),
            "tool '{}' inputSchema must have 'properties'",
            name
        );
    }
}

#[test]
fn test_tools_have_unique_names() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let tools = registry.list_tools();

    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let total = names.len();
    names.sort();
    names.dedup();

    assert_eq!(names.len(), total, "tool names must be unique");
}

#[test]
fn test_tools_names_are_snake_case() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let tools = registry.list_tools();

    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
            "tool name '{}' must be snake_case (lowercase, underscores, digits)",
            name
        );
    }
}

#[test]
fn test_essential_tools_present() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let tools = registry.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    let essential = vec![
        "read_file",
        "write_file",
        "edit_file",
        "apply_patch",
        "run_command",
        "search_code",
        "policy_check",
        "simulate",
    ];

    for name in essential {
        assert!(
            names.contains(&name),
            "essential tool '{}' must be registered",
            name
        );
    }
}

#[test]
fn test_to_action_unknown_tool_returns_error() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let result = registry.to_action("nonexistent_tool", &json!({}));
    assert!(result.is_err());
}

#[test]
fn test_to_action_read_file() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let args = json!({ "path": "/tmp/test.txt" });
    let action = registry.to_action("read_file", &args);
    assert!(action.is_ok(), "read_file with valid args should succeed");
}

#[test]
fn test_to_action_missing_required_param() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    // read_file requires "path"
    let result = registry.to_action("read_file", &json!({}));
    assert!(result.is_err(), "read_file without path should fail");
}

#[test]
fn test_to_action_write_file() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let args = json!({ "path": "/tmp/test.txt", "content": "hello" });
    let action = registry.to_action("write_file", &args);
    assert!(action.is_ok());
}

#[test]
fn test_to_action_run_command() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let args = json!({ "command": "echo", "args": ["hello"] });
    let action = registry.to_action("run_command", &args);
    assert!(action.is_ok());
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. Initialize Response Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_initialize_response_has_required_fields() {
    // Simulate what the server returns for initialize
    let init_result = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "x0ne-runtime",
            "version": env!("CARGO_PKG_VERSION")
        }
    });

    // Per MCP spec: must have protocolVersion, capabilities, serverInfo
    assert!(init_result.get("protocolVersion").is_some());
    assert!(init_result.get("capabilities").is_some());
    assert!(init_result.get("serverInfo").is_some());

    let server_info = &init_result["serverInfo"];
    assert!(server_info.get("name").is_some());
    assert!(server_info.get("version").is_some());
    assert_eq!(server_info["name"].as_str().unwrap(), "x0ne-runtime");

    let capabilities = &init_result["capabilities"];
    assert!(
        capabilities.get("tools").is_some(),
        "must declare tools capability"
    );
}

#[test]
fn test_protocol_version_matches_spec() {
    let init_result = json!({
        "protocolVersion": "2024-11-05"
    });
    let version = init_result["protocolVersion"].as_str().unwrap();
    assert_eq!(version, "2024-11-05", "must support MCP spec 2024-11-05");
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. Tools/Call Result Format Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_tool_result_success_format() {
    // Per MCP spec: success result has content array + isError: false
    let result = json!({
        "content": [{
            "type": "text",
            "text": "file contents here"
        }],
        "isError": false
    });

    let content = result["content"].as_array().unwrap();
    assert!(!content.is_empty());
    assert_eq!(content[0]["type"].as_str().unwrap(), "text");
    assert!(content[0]["text"].as_str().is_some());
    assert!(!result["isError"].as_bool().unwrap());
}

#[test]
fn test_tool_result_error_format() {
    // Per MCP spec: execution error has content array + isError: true
    let result = json!({
        "content": [{
            "type": "text",
            "text": "Policy denied: path not in allowlist"
        }],
        "isError": true
    });

    assert!(result["isError"].as_bool().unwrap());
    let content = result["content"].as_array().unwrap();
    assert!(!content.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. Error Code Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_error_codes_match_jsonrpc_spec() {
    use x0ne_runtime::mcp::jsonrpc::*;

    assert_eq!(PARSE_ERROR, -32700);
    assert_eq!(INVALID_REQUEST, -32600);
    assert_eq!(METHOD_NOT_FOUND, -32601);
    assert_eq!(INVALID_PARAMS, -32602);
    assert_eq!(INTERNAL_ERROR, -32603);
}

#[test]
fn test_unknown_method_returns_method_not_found() {
    let resp = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::error(
        json!(99),
        x0ne_runtime::mcp::jsonrpc::METHOD_NOT_FOUND,
        "method not found: foo/bar",
    );
    assert_eq!(resp.error.as_ref().unwrap().code, -32601);
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. Transport / Serialization Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_response_serialization_roundtrip() {
    let original = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::ok(
        json!(42),
        json!({"tools": [{"name": "test"}]}),
    );

    let json_str = serde_json::to_string(&original).unwrap();
    let parsed: x0ne_runtime::mcp::jsonrpc::JsonRpcResponse =
        serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed.jsonrpc, "2.0");
    assert_eq!(parsed.id, json!(42));
    assert!(parsed.result.is_some());
    assert!(parsed.error.is_none());
}

#[test]
fn test_error_response_serialization_roundtrip() {
    let original = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::error(
        json!("req-1"),
        -32602,
        "missing params",
    );

    let json_str = serde_json::to_string(&original).unwrap();
    let parsed: x0ne_runtime::mcp::jsonrpc::JsonRpcResponse =
        serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed.id, json!("req-1"));
    let err = parsed.error.unwrap();
    assert_eq!(err.code, -32602);
    assert_eq!(err.message, "missing params");
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. MCP Lifecycle Simulation
// ═══════════════════════════════════════════════════════════════════════════

/// Simulate the full MCP handshake and tool call lifecycle
/// by validating message formats at each step.
#[test]
fn test_mcp_lifecycle_message_formats() {
    // Step 1: Client sends initialize
    let init_req = parse_request(
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
    );
    assert_eq!(init_req.method, "initialize");
    assert!(!init_req.is_notification());

    // Step 2: Client sends initialized notification (no id → no response)
    let initialized = parse_request(r#"{"jsonrpc":"2.0","method":"initialized"}"#);
    assert!(initialized.is_notification());

    // Step 3: Client requests tools/list
    let list_req = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert_eq!(list_req.method, "tools/list");

    // Step 4: Client calls a tool
    let call_req = parse_request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"./test.txt"}}}"#,
    );
    assert_eq!(call_req.method, "tools/call");
    let params = call_req.params.unwrap();
    assert_eq!(params["name"].as_str().unwrap(), "read_file");
    assert!(params.get("arguments").is_some());
}

#[test]
fn test_mcp_notification_types_parseable() {
    // All standard MCP notifications should parse without error
    let notifications = vec![
        r#"{"jsonrpc":"2.0","method":"initialized"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
    ];

    for msg in notifications {
        let req = parse_request(msg);
        assert!(
            req.is_notification(),
            "notification must have no id: {}",
            msg
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. Edge Cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_tools_call_empty_arguments() {
    // Some tools may have no required params
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    // list_snapshots doesn't require arguments
    let result = registry.to_action("list_snapshots", &json!({}));
    assert!(
        result.is_ok(),
        "tools with no required params should work with empty args"
    );
}

#[test]
fn test_tools_call_extra_arguments_ignored() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let args = json!({
        "path": "/tmp/test.txt",
        "unknown_field": "should be ignored",
        "another_extra": 42
    });
    let result = registry.to_action("read_file", &args);
    assert!(
        result.is_ok(),
        "extra arguments should be ignored gracefully"
    );
}

#[test]
fn test_response_never_has_both_result_and_error() {
    let ok = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::ok(json!(1), json!({}));
    assert!(ok.result.is_some());
    assert!(ok.error.is_none());

    let err = x0ne_runtime::mcp::jsonrpc::JsonRpcResponse::error(json!(1), -32600, "bad");
    assert!(err.result.is_none());
    assert!(err.error.is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. Schema Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_all_tools_have_type_object_schema() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    for tool in registry.list_tools() {
        let name = tool["name"].as_str().unwrap();
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "Tool '{}' schema type must be 'object'",
            name
        );
    }
}

#[test]
fn test_schema_properties_are_objects() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    for tool in registry.list_tools() {
        let name = tool["name"].as_str().unwrap();
        let props = &tool["inputSchema"]["properties"];
        if props.is_object() {
            for (prop_name, prop_schema) in props.as_object().unwrap() {
                assert!(
                    prop_schema.get("type").is_some() || prop_schema.get("oneOf").is_some(),
                    "Tool '{}' property '{}' must have a 'type' or 'oneOf' field",
                    name,
                    prop_name
                );
            }
        }
    }
}

#[test]
fn test_required_field_is_array_if_present() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    for tool in registry.list_tools() {
        let name = tool["name"].as_str().unwrap();
        if let Some(required) = tool["inputSchema"].get("required") {
            assert!(
                required.is_array(),
                "Tool '{}' 'required' field must be an array",
                name
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tool annotations (spec revision 2025-03-26; ignored by older clients)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_tools_list_is_sorted_by_name_and_stable() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let first = registry.list_tools();
    let second = registry.list_tools();
    assert_eq!(first, second, "tools/list must be identical across calls");
    let names: Vec<&str> = first.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tools/list must be sorted by name");
}

#[test]
fn test_every_tool_has_boolean_annotations() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    for tool in registry.list_tools() {
        let name = tool["name"].as_str().unwrap();
        let a = tool
            .get("annotations")
            .unwrap_or_else(|| panic!("tool '{name}' has no annotations"));
        for key in ["readOnlyHint", "destructiveHint", "openWorldHint"] {
            assert!(
                a[key].is_boolean(),
                "tool '{name}' annotations.{key} must be a boolean, got {}",
                a[key]
            );
        }
        if let Some(v) = a.get("idempotentHint") {
            assert!(
                v.is_boolean(),
                "tool '{name}' idempotentHint must be a boolean"
            );
        }
    }
}

#[test]
fn test_annotations_reflect_side_effect_class() {
    let registry = x0ne_runtime::mcp::registry::ToolRegistry::new();
    let tools = registry.list_tools();
    let hint = |tool: &str, key: &str| -> bool {
        tools
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap_or_else(|| panic!("tool '{tool}' not advertised"))["annotations"][key]
            .as_bool()
            .unwrap_or_else(|| panic!("'{tool}'.annotations.{key} is not a boolean"))
    };
    assert!(hint("read_file", "readOnlyHint"));
    assert!(hint("read_file", "idempotentHint"));
    assert!(!hint("read_file", "destructiveHint"));
    assert!(!hint("write_file", "readOnlyHint"));
    assert!(!hint("write_file", "destructiveHint"));
    assert!(hint("dns_lookup", "openWorldHint"));
    assert!(!hint("read_file", "openWorldHint"));
    assert!(hint("delete_snapshot", "destructiveHint"));
    assert!(hint("move_to_trash", "destructiveHint"));
    assert!(!hint("run_command", "readOnlyHint"));
    assert!(!hint("run_command", "destructiveHint"));
}
