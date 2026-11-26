//! MCP JSON-RPC protocol primitives and method routing.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mcp_tools;
use crate::state::AppState;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

#[derive(Debug, Deserialize)]
pub struct McpRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct McpResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl McpResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.unwrap_or(Value::Null),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.unwrap_or(Value::Null),
            result: None,
            error: Some(json!({"code": code, "message": message.into()})),
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        if let Some(error) = self.error.as_mut() {
            error["data"] = data;
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct ClientContext {
    pub client_id: String,
    pub client_name: String,
    pub protocol_version: String,
}

#[derive(Debug, Clone)]
pub struct InitializeInfo {
    pub client_name: String,
    pub protocol_version: String,
    pub result: Value,
}

pub fn initialize(params: &Value) -> Result<InitializeInfo, String> {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or("initialize.params.protocolVersion is required")?;
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        return Err(format!(
            "unsupported protocolVersion {requested}; supported: {}",
            SUPPORTED_PROTOCOL_VERSIONS.join(", ")
        ));
    }
    let client_name = params
        .get("clientInfo")
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("unknown-mcp-client")
        .chars()
        .take(128)
        .collect::<String>();
    Ok(InitializeInfo {
        client_name: client_name.clone(),
        protocol_version: requested.into(),
        result: json!({
            "protocolVersion": requested,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": {},
                "prompts": {}
            },
            "serverInfo": {
                "name": "Chena-Shell",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Chena-Shell local MCP service. Attach and potentially dangerous commands require human approval in the main window."
        }),
    })
}

pub async fn dispatch(
    request: McpRequest,
    state: &AppState,
    app: Option<&tauri::AppHandle>,
    client: &ClientContext,
) -> McpResponse {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return McpResponse::err(None, -32600, "jsonrpc must be 2.0");
    }
    match request.method.as_str() {
        "ping" => McpResponse::ok(id, json!({})),
        "tools/list" => McpResponse::ok(id, mcp_tools::tools_list()),
        "tools/call" => {
            let Some(app) = app else {
                return McpResponse::err(id, -32600, "MCP application runtime is unavailable");
            };
            let value = mcp_tools::call(request.params, state, app, client).await;
            let is_error = value.get("error").is_some()
                || value.get("ok").and_then(Value::as_bool) == Some(false);
            let text = serde_json::to_string(&value)
                .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".into());
            let mut result = json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            });
            if value.is_object() {
                result["structuredContent"] = value;
            }
            McpResponse::ok(id, result)
        }
        "resources/list" => McpResponse::ok(id, json!({"resources": []})),
        "prompts/list" => McpResponse::ok(id, json!({"prompts": []})),
        "notifications/initialized" | "notifications/cancelled" => McpResponse::ok(id, json!({})),
        "initialize" => McpResponse::err(id, -32600, "initialize must start a new MCP session"),
        method => McpResponse::err(id, -32601, format!("method not found: {method}")),
    }
}

pub fn parse_request(value: Value) -> Result<McpRequest, McpResponse> {
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpResponse::err(None, -32600, "jsonrpc must be 2.0"));
    }
    serde_json::from_value(value).map_err(|error| McpResponse::err(None, -32600, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_supported_versions() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let info = initialize(&json!({
                "protocolVersion": version,
                "clientInfo": {"name": "test", "version": "1"}
            }))
            .unwrap();
            assert_eq!(info.protocol_version, *version);
            assert_eq!(info.result["protocolVersion"], *version);
        }
        assert!(initialize(&json!({"protocolVersion": "1900-01-01"})).is_err());
    }

    #[test]
    fn json_rpc_errors_have_null_id_and_standard_codes() {
        let response = parse_request(json!({"method": "ping", "id": 1})).unwrap_err();
        assert_eq!(response.id, Value::Null);
        assert_eq!(response.error.unwrap()["code"], -32600);
    }
}
