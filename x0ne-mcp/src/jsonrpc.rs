use serde::{Deserialize, Serialize};
use serde_json::Value;

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC 2.0 request or notification.
/// Notifications have `id: None` — no response should be sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// `None` for notifications; `Some(Value)` for requests that expect a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Returns true if this is a notification (no id → no response expected).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_valid_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"foo":"bar"}}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "ping");
        assert_eq!(req.id, Some(json!(1)));
        assert!(req.params.is_some());
        assert!(!req.is_notification());
    }

    #[test]
    fn test_parse_notification_no_id() {
        let json = r#"{"jsonrpc":"2.0","method":"initialized"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "initialized");
        assert!(req.id.is_none());
        assert!(req.is_notification());
    }

    #[test]
    fn test_parse_notification_with_params() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42,"reason":"timeout"}}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "notifications/cancelled");
        assert!(req.is_notification());
        assert!(req.params.is_some());
    }

    #[test]
    fn test_parse_request_string_id() {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, Some(json!("abc-123")));
        assert!(!req.is_notification());
    }

    #[test]
    fn test_parse_missing_method() {
        let json = r#"{"jsonrpc":"2.0","id":1}"#;
        let req: Result<JsonRpcRequest, _> = serde_json::from_str(json);
        assert!(req.is_err());
    }

    #[test]
    fn test_serialize_response_result() {
        let resp = JsonRpcResponse::ok(json!(1), json!({"status":"ok"}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\":{\"status\":\"ok\"}"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_serialize_response_error() {
        let resp = JsonRpcResponse::error(json!(2), METHOD_NOT_FOUND, "Not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\":"));
        assert!(json.contains("\"code\":-32601"));
        assert!(!json.contains("\"result\""));
    }
}
