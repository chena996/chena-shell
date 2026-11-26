//! Newline-delimited JSON-RPC stdio adapter for the running GUI MCP service.
//!
//! This process deliberately has no `AppState`: it neither restores sessions nor
//! starts another SSH/MCP service. stdout is reserved for protocol messages.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp_protocol::McpResponse;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8767/mcp";
const MAX_LINE_BYTES: usize = 256 * 1024;

fn validate_endpoint(text: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(text)?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow::anyhow!(
            "CHENA_MCP_URL 只允许无内嵌凭证的 HTTP 地址"
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("CHENA_MCP_URL 缺少主机"))?;
    let ip_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    if !loopback {
        return Err(anyhow::anyhow!("CHENA_MCP_URL 只允许 loopback 地址"));
    }
    Ok(url)
}

fn response_value(id: Option<Value>, code: i32, message: impl Into<String>) -> Value {
    serde_json::to_value(McpResponse::err(id, code, message)).unwrap_or_else(|_| {
        serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}})
    })
}

async fn forward(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    token: &str,
    session_id: &mut Option<String>,
    protocol_version: &mut Option<String>,
    value: Value,
) -> Option<Value> {
    let id = value.get("id").cloned();
    let is_initialize = value.get("method").and_then(Value::as_str) == Some("initialize");
    let mut request = client
        .post(endpoint.clone())
        .bearer_auth(token)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&value);
    if !is_initialize {
        if let Some(session) = session_id.as_deref() {
            request = request.header("mcp-session-id", session);
        }
        if let Some(version) = protocol_version.as_deref() {
            request = request.header("mcp-protocol-version", version);
        }
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return Some(response_value(
                id,
                -32000,
                format!("无法连接 Chena-Shell MCP 服务: {error}"),
            ))
        }
    };
    let status = response.status();
    let returned_session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return Some(response_value(
                id,
                -32000,
                format!("读取 MCP 响应失败: {error}"),
            ))
        }
    };
    if body.is_empty() {
        return None;
    }
    let parsed = match serde_json::from_slice::<Value>(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(response_value(
                id,
                -32603,
                format!("MCP 服务返回了非 JSON 响应（HTTP {status}）: {error}"),
            ))
        }
    };
    if is_initialize && status.is_success() {
        if let Some(session) = returned_session {
            *session_id = Some(session);
        }
        if let Some(version) = parsed
            .get("result")
            .and_then(|result| result.get("protocolVersion"))
            .and_then(Value::as_str)
        {
            *protocol_version = Some(version.into());
        }
    }
    if !status.is_success() && parsed.get("jsonrpc").is_none() {
        let detail = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("MCP HTTP 请求失败");
        return Some(response_value(
            id,
            -32000,
            format!("HTTP {status}: {detail}"),
        ));
    }
    Some(parsed)
}

pub async fn run() -> anyhow::Result<()> {
    let token = std::env::var("CHENA_MCP_TOKEN").map_err(|_| {
        anyhow::anyhow!("缺少 CHENA_MCP_TOKEN，请先在 Chena-Shell 设置中生成 MCP 凭证")
    })?;
    if token.is_empty() {
        return Err(anyhow::anyhow!("CHENA_MCP_TOKEN 不能为空"));
    }
    let endpoint = validate_endpoint(
        &std::env::var("CHENA_MCP_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.into()),
    )?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(310))
        .build()?;
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    let mut session_id = None;
    let mut protocol_version = None;

    loop {
        let line = lines.next_line().await?;
        let Some(line) = line else {
            break;
        };
        if line.len() > MAX_LINE_BYTES {
            let error = response_value(None, -32600, "JSON-RPC line exceeds 256 KiB");
            stdout.write_all(format!("{error}\n").as_bytes()).await?;
            stdout.flush().await?;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                let response = response_value(None, -32700, format!("parse error: {error}"));
                stdout.write_all(format!("{response}\n").as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };
        if let Some(response) = forward(
            &client,
            &endpoint,
            &token,
            &mut session_id,
            &mut protocol_version,
            value,
        )
        .await
        {
            stdout.write_all(format!("{response}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    if let Some(session) = session_id {
        let mut request = client
            .delete(endpoint)
            .bearer_auth(token)
            .header("mcp-session-id", session)
            .timeout(std::time::Duration::from_secs(2));
        if let Some(version) = protocol_version {
            request = request.header("mcp-protocol-version", version);
        }
        let _ = request.send().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_endpoint_cannot_exfiltrate_credentials_to_remote_hosts() {
        assert!(validate_endpoint(DEFAULT_ENDPOINT).is_ok());
        assert!(validate_endpoint("http://localhost:9000/mcp").is_ok());
        assert!(validate_endpoint("http://[::1]:8767/mcp").is_ok());
        assert!(validate_endpoint("https://127.0.0.1:8767/mcp").is_err());
        assert!(validate_endpoint("http://192.168.1.2:8767/mcp").is_err());
        assert!(validate_endpoint("http://user:pass@127.0.0.1:8767/mcp").is_err());
    }
}
