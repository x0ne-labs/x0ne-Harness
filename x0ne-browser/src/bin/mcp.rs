//! x0ne-browser-mcp — MCP stdio server exposing the browser_* toolbelt.
//!
//! Wire into Claude Code via `.mcp.json`:
//! ```jsonc
//! { "mcpServers": { "x0ne-browser": { "command": "x0ne-browser-mcp" } } }
//! ```

use serde_json::{json, Value};
use std::sync::Arc;
use x0ne_browser::error::BrowserError;
use x0ne_browser::session::BrowserConfig;
use x0ne_browser::ChromiumDriver;
use x0ne_mcp::{JsonRpcRequest, JsonRpcResponse, StdioTransport, INVALID_PARAMS, METHOD_NOT_FOUND};

fn main() {
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run());
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("x0ne_browser=info,info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn run() {
    let config = match BrowserConfig::from_default_path() {
        Ok(c) => c,
        Err(e) => {
            // A restriction the operator wrote and we cannot read is not one we may drop.
            tracing::error!("browser-tools.toml: {e}; not starting on the defaults");
            std::process::exit(2);
        }
    };
    let driver = Arc::new(ChromiumDriver::new(config));
    let mut transport = StdioTransport::new();
    tracing::info!(
        "x0ne-browser-mcp ready on stdio (session {})",
        driver.session_id
    );

    loop {
        let req = match transport.read_message().await {
            Ok(r) => r,
            Err(e) if e.to_string() == "EOF" => break,
            Err(e) => {
                tracing::error!("read JSON-RPC: {e}");
                continue;
            }
        };
        if let Some(resp) = handle(&driver, req).await {
            if let Err(e) = transport.write_message(&resp).await {
                tracing::error!("write JSON-RPC: {e}");
                break;
            }
        }
    }
    // Tear down browser on shutdown
    let _ = driver.close().await;
}

async fn handle(driver: &Arc<ChromiumDriver>, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse::ok(id, initialize_result())),
        "tools/list" => Some(JsonRpcResponse::ok(id, json!({"tools": tool_specs()}))),
        "tools/call" => Some(handle_call(driver, id, &req).await),
        "ping" => Some(JsonRpcResponse::ok(id, json!({}))),
        "resources/list" => Some(JsonRpcResponse::ok(id, json!({"resources": []}))),
        "prompts/list" => Some(JsonRpcResponse::ok(id, json!({"prompts": []}))),
        _ => Some(JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            &format!("method not found: {}", req.method),
        )),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "x0ne-browser", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// F5: tools that only observe. Every other tool is annotated as mutating.
const READ_ONLY_TOOLS: &[&str] = &[
    "browser_get_url",
    "browser_screenshot",
    "browser_read_text",
    "browser_read_dom",
    "browser_wait_for",
    "browser_status",
    "browser_list_tabs",
    "browser_console_logs",
    "browser_network_log",
];

fn tool_specs() -> Vec<Value> {
    fn s(name: &str, desc: &str, schema: Value) -> Value {
        let hints = x0ne_mcp::tool_hints(READ_ONLY_TOOLS.contains(&name));
        json!({ "name": name, "description": desc, "inputSchema": schema, "annotations": hints })
    }
    fn obj(props: Value, required: &[&str]) -> Value {
        json!({ "type": "object", "properties": props, "required": required })
    }
    let str_p = json!({"type": "string"});
    let bool_p = json!({"type": "boolean"});

    vec![
        s("browser_navigate",
          "Navigate to a URL. Refused if outside the allowlist (default: loopback + about:). Returns the resulting URL.",
          obj(json!({"url": str_p}), &["url"])),
        s("browser_get_url",
          "Return the current URL of the page.",
          obj(json!({}), &[])),
        s("browser_screenshot",
          "Capture a PNG (default) or JPEG of the page. `full_page=true` captures beyond the viewport. Returns base64 image data.",
          obj(json!({
              "full_page": bool_p,
              "format": {"type": "string", "enum": ["png", "jpeg"]}
          }), &[])),
        s("browser_click",
          "Click an element. Selector defaults to CSS; prefix with `xpath:` or `text:` to switch syntax (v2). Auto-waits up to the default timeout for the element to be visible.",
          obj(json!({"selector": str_p}), &["selector"])),
        s("browser_type",
          "Type text into a field. `clear=true` erases the existing value first.",
          obj(json!({"selector": str_p, "text": str_p, "clear": bool_p}), &["selector", "text"])),
        s("browser_press_key",
          "Send a single keystroke (Enter, Tab, Escape, ArrowDown, …). For text input, use browser_type.",
          obj(json!({"key": str_p}), &["key"])),
        s("browser_read_text",
          "Return the inner text of a matched element. Truncated past 256KB by default.",
          obj(json!({"selector": str_p}), &["selector"])),
        s("browser_read_dom",
          "Return the outer HTML of a matched element (or the document if no selector). Truncated like read_text.",
          obj(json!({"selector": str_p}), &[])),
        s("browser_wait_for",
          "Block until the selector matches an element on the page, or `timeout_ms` elapses (default 10000).",
          obj(json!({"selector": str_p, "timeout_ms": {"type": "integer"}}), &["selector"])),
        s("browser_back",
          "history.back() — navigate one entry back.",
          obj(json!({}), &[])),
        s("browser_forward",
          "history.forward() — navigate one entry forward.",
          obj(json!({}), &[])),
        s("browser_reload",
          "Reload the current page (Ctrl-R equivalent).",
          obj(json!({}), &[])),
        s("browser_close",
          "Kill the Chromium subprocess and wipe the session profile (unless keep_profile was set).",
          obj(json!({}), &[])),
        s("browser_status",
          "Diagnostics: session id, rate-limit tokens, allowlist, kill switch path, profile dir, headed/headless.",
          obj(json!({}), &[])),

        // Phase 2 stubs — agent sees them in the toolbelt but they return
        // NotImplemented with a clear hint about v1 scope.
        s("browser_hover",
          "[v2] Hover an element. Currently returns not_implemented.",
          obj(json!({"selector": str_p}), &["selector"])),
        s("browser_scroll",
          "[v2] Scroll the page. Currently returns not_implemented.",
          obj(json!({"direction": str_p, "amount": {"type": "integer"}}), &["direction"])),
        s("browser_list_tabs",
          "[v2] List open tabs. Returns not_implemented in v1 (single-tab session).",
          obj(json!({}), &[])),
        s("browser_console_logs",
          "[v2] Return JS console messages from the page. not_implemented in v1.",
          obj(json!({}), &[])),
        s("browser_network_log",
          "[v2] Return network requests captured since last call. not_implemented in v1.",
          obj(json!({}), &[])),
    ]
}

async fn handle_call(
    driver: &Arc<ChromiumDriver>,
    id: Value,
    req: &JsonRpcRequest,
) -> JsonRpcResponse {
    let params = match req.params.as_ref() {
        Some(p) => p,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing params"),
    };
    let tool = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing tool name"),
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let res: Result<Value, BrowserError> = match tool.as_str() {
        "browser_navigate" => {
            driver
                .navigate(args.get("url").and_then(|v| v.as_str()).unwrap_or(""))
                .await
        }
        "browser_get_url" => driver.get_url().await,
        "browser_screenshot" => {
            driver
                .screenshot(
                    args.get("full_page")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    args.get("format").and_then(|v| v.as_str()).unwrap_or("png"),
                )
                .await
        }
        "browser_click" => {
            driver
                .click(args.get("selector").and_then(|v| v.as_str()).unwrap_or(""))
                .await
        }
        "browser_type" => {
            driver
                .type_text(
                    args.get("selector").and_then(|v| v.as_str()).unwrap_or(""),
                    args.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false),
                )
                .await
        }
        "browser_press_key" => {
            driver
                .press_key(args.get("key").and_then(|v| v.as_str()).unwrap_or(""))
                .await
        }
        "browser_read_text" => {
            driver
                .read_text(args.get("selector").and_then(|v| v.as_str()).unwrap_or(""))
                .await
        }
        "browser_read_dom" => {
            driver
                .read_dom(
                    args.get("selector")
                        .and_then(|v| v.as_str())
                        .unwrap_or("html"),
                )
                .await
        }
        "browser_wait_for" => {
            driver
                .wait_for(
                    args.get("selector").and_then(|v| v.as_str()).unwrap_or(""),
                    args.get("timeout_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(10_000),
                )
                .await
        }
        "browser_back" => driver.back().await,
        "browser_forward" => driver.forward().await,
        "browser_reload" => driver.reload().await,
        "browser_close" => driver.close().await,
        "browser_status" => Ok(driver.status()),
        // Phase 2 stubs
        "browser_hover"
        | "browser_scroll"
        | "browser_list_tabs"
        | "browser_console_logs"
        | "browser_network_log" => driver.not_implemented(&tool).await,
        other => {
            return JsonRpcResponse::ok(
                id,
                json!({
                    "content": [{"type": "text", "text": format!("unknown tool: {other}")}],
                    "isError": true,
                }),
            );
        }
    };

    let payload: Value = match res {
        Ok(v) => v,
        Err(e) => e.into(),
    };
    JsonRpcResponse::ok(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&payload).unwrap_or_default()
            }],
            "isError": !payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
    )
}

#[cfg(test)]
mod annotation_tests {
    #[test]
    fn every_tool_is_annotated() {
        let tools = super::tool_specs();
        x0ne_mcp::check_hints(&tools, super::READ_ONLY_TOOLS).unwrap();
    }
}
