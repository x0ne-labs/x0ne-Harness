use anyhow::Result;
use serde_json::json;

use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, METHOD_NOT_FOUND};
use super::registry::ToolRegistry;
use super::transport::StdioTransport;
use crate::server::ServerState;

pub struct McpServer {
    transport: StdioTransport,
    registry: ToolRegistry,
    state: ServerState,
}

impl McpServer {
    pub fn new(registry: ToolRegistry, state: ServerState) -> Self {
        Self {
            transport: StdioTransport::new(),
            registry,
            state,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            let msg = match self.transport.read_message().await {
                Ok(m) => m,
                Err(e) => {
                    if e.to_string() == "EOF" {
                        break; // Connection closed
                    }
                    tracing::error!("Failed to read JSON-RPC: {}", e);
                    continue;
                }
            };

            let response = self.handle_message(msg).await;
            if let Some(resp) = response {
                if let Err(e) = self.transport.write_message(&resp).await {
                    tracing::error!("Failed to write JSON-RPC response: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_message(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // Notifications have no id — never respond to them
        if req.is_notification() {
            match req.method.as_str() {
                "initialized" | "notifications/initialized" => {
                    tracing::debug!("Client initialized");
                }
                "notifications/cancelled" => {
                    tracing::debug!("Client cancelled request");
                }
                "notifications/roots/list_changed" => {
                    tracing::debug!("Roots changed notification");
                }
                _ => {
                    tracing::debug!("Unknown notification: {}", req.method);
                }
            }
            return None;
        }

        let id = req.id.clone().unwrap_or(serde_json::Value::Null);

        match req.method.as_str() {
            "initialize" => Some(self.handle_initialize(id)),
            "tools/list" => Some(self.handle_tools_list(id, &req)),
            "tools/call" => Some(self.handle_tools_call(id, &req).await),
            "ping" => Some(self.handle_ping(id)),

            // Resources — X0NE doesn't serve resources, return empty
            "resources/list" => Some(JsonRpcResponse::ok(
                id,
                json!({
                    "resources": []
                }),
            )),
            "resources/templates/list" => Some(JsonRpcResponse::ok(
                id,
                json!({
                    "resourceTemplates": []
                }),
            )),
            "resources/read" => Some(JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                "No resources available",
            )),

            // Prompts — X0NE doesn't serve prompts, return empty
            "prompts/list" => Some(JsonRpcResponse::ok(
                id,
                json!({
                    "prompts": []
                }),
            )),
            "prompts/get" => Some(JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                "No prompts available",
            )),

            // Completion — not supported
            "completion/complete" => Some(JsonRpcResponse::ok(
                id,
                json!({
                    "completion": { "values": [], "hasMore": false }
                }),
            )),

            // Logging
            "logging/setLevel" => {
                tracing::info!("Client requested log level change");
                Some(JsonRpcResponse::ok(id, json!({})))
            }

            _ => Some(JsonRpcResponse::error(
                id,
                METHOD_NOT_FOUND,
                &format!("method not found: {}", req.method),
            )),
        }
    }

    fn handle_initialize(&self, id: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse::ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    },
                    "resources": {
                        "subscribe": false,
                        "listChanged": false
                    },
                    "prompts": {
                        "listChanged": false
                    },
                    "logging": {}
                },
                "serverInfo": {
                    "name": "x0ne-runtime",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
    }

    fn handle_tools_list(&self, id: serde_json::Value, req: &JsonRpcRequest) -> JsonRpcResponse {
        // MCP supports pagination via cursor, but we return all tools at once
        let _cursor = req
            .params
            .as_ref()
            .and_then(|p| p.get("cursor"))
            .and_then(|c| c.as_str());

        let tools = self.registry.list_tools();
        JsonRpcResponse::ok(id, json!({ "tools": tools }))
    }

    async fn handle_tools_call(
        &self,
        id: serde_json::Value,
        req: &JsonRpcRequest,
    ) -> JsonRpcResponse {
        let params = match req.params.as_ref() {
            Some(p) => p,
            None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing params"),
        };
        let tool_name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return JsonRpcResponse::error(id, INVALID_PARAMS, "missing tool name"),
        };
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        // 1. Convert to Action
        let action = match self.registry.to_action(tool_name, &arguments) {
            Ok(a) => a,
            Err(e) => {
                return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Unknown tool: {}", e))
            }
        };

        // 2. Policy check
        match self.state.policy.evaluate(&action) {
            crate::policy::PolicyEffect::Allow => {}
            crate::policy::PolicyEffect::RequireConfirmation(msg) => {
                return JsonRpcResponse::ok(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Action requires user approval: {}", msg)
                        }],
                        "isError": true
                    }),
                );
            }
            crate::policy::PolicyEffect::Deny(reason) => {
                return JsonRpcResponse::ok(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Policy denied: {}", reason.message())
                        }],
                        "isError": true
                    }),
                );
            }
        }

        // 3. Execute
        let result = crate::executor::execute(
            &action,
            &self.state.config,
            self.state.handles.clone(),
            &self.state.policy,
            &self.state.rate_limiter,
            &self.state.composites,
        )
        .await;

        // 4. Format MCP response
        // Per spec: tool execution errors use isError: true in the result,
        // not JSON-RPC level errors
        if result.success {
            let text_output = if let Some(err) = &result.error {
                format!("Completed with errors: {}", err.message)
            } else {
                serde_json::to_string_pretty(&result.output).unwrap_or_default()
            };

            JsonRpcResponse::ok(
                id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": text_output
                    }],
                    "isError": false
                }),
            )
        } else {
            JsonRpcResponse::ok(
                id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": result.error.map(|e| e.message).unwrap_or_else(|| "unknown error".into())
                    }],
                    "isError": true
                }),
            )
        }
    }

    fn handle_ping(&self, id: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse::ok(id, json!({}))
    }
}
