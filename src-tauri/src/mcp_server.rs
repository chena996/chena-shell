//! MCP Streamable HTTP service on the local loopback interface.
//!
//! This module owns HTTP transport, authentication and MCP session lifecycle.
//! JSON-RPC routing is in `mcp_protocol`; tool business logic is in `mcp_tools`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch, Semaphore};

use crate::approval;
use crate::mcp_auth;
use crate::mcp_protocol::{self, ClientContext, McpRequest, McpResponse};
use crate::state::AppState;

const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
// Must exceed the maximum tool execution timeout (300 seconds).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(310);

#[derive(Clone)]
struct ClientSession {
    context: ClientContext,
    initialized: bool,
}

#[derive(Clone)]
struct HttpRuntime {
    state: AppState,
    app: Option<tauri::AppHandle>,
    sessions: Arc<DashMap<String, ClientSession>>,
    semaphore: Arc<Semaphore>,
}

type ReadySender = oneshot::Sender<Result<(), String>>;

/// Start the service until `stop` is called. The caller decides whether
/// autostart is desired; this function always honors mcp_enabled.
pub async fn start(state: AppState, app: tauri::AppHandle) -> anyhow::Result<()> {
    start_with_ready(state, app, None).await
}

async fn start_with_ready(
    state: AppState,
    app: tauri::AppHandle,
    mut ready: Option<ReadySender>,
) -> anyhow::Result<()> {
    {
        let mut inner = state.inner.write();
        if inner.mcp_listening || inner.mcp_starting {
            if let Some(sender) = ready.take() {
                let _ = sender.send(Ok(()));
            }
            return Ok(());
        }
        if !inner.settings.mcp_enabled {
            let error = "MCP 服务已禁用".to_string();
            if let Some(sender) = ready.take() {
                let _ = sender.send(Err(error.clone()));
            }
            return Err(anyhow::anyhow!(error));
        }
        inner.mcp_starting = true;
    }

    let result = start_inner(state.clone(), app, ready).await;
    let mut inner = state.inner.write();
    inner.mcp_starting = false;
    inner.mcp_listening = false;
    inner.mcp_shutdown = None;
    result
}

async fn start_inner(
    state: AppState,
    app: tauri::AppHandle,
    mut ready: Option<ReadySender>,
) -> anyhow::Result<()> {
    let prepared = async {
        let (host, port) = {
            let settings = state.inner.read().settings.clone();
            (settings.mcp_host, settings.mcp_port)
        };
        validate_loopback(&host)?;
        let address = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let listener = tokio::net::TcpListener::bind(&address).await?;
        Ok::<_, anyhow::Error>((listener, address))
    }
    .await;
    let (listener, address) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(sender) = ready.take() {
                let _ = sender.send(Err(error.to_string()));
            }
            return Err(error);
        }
    };
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    {
        let mut inner = state.inner.write();
        inner.mcp_listening = true;
        inner.mcp_shutdown = Some(shutdown_tx);
    }
    if let Some(sender) = ready.take() {
        let _ = sender.send(Ok(()));
    }
    tracing::info!("MCP Streamable HTTP listening on http://{address}/mcp");

    let runtime = HttpRuntime {
        state: state.clone(),
        app: Some(app.clone()),
        sessions: Arc::new(DashMap::new()),
        semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
    };
    let router = Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::from_fn(add_utf8_charset))
        .with_state(runtime);

    let cleanup_state = state.clone();
    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            approval::expire_requests(&cleanup_state);
            let handles = cleanup_state
                .inner
                .read()
                .sessions
                .iter()
                .map(|entry| entry.value().clone())
                .collect::<Vec<_>>();
            for handle in handles {
                handle.expire_attach_if_needed(&cleanup_state);
            }
        }
    });

    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await;
    cleanup.abort();
    state.inner.write().mcp_listening = false;
    if let Err(error) = serve_result {
        return Err(error.into());
    }
    Ok(())
}

/// Spawn the service without blocking the Tauri setup callback.
pub fn spawn_start(state: AppState, app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(error) = start(state, app).await {
            tracing::error!("MCP server error: {error}");
        }
    });
}

/// Start and report only after the loopback listener is actually bound.
pub async fn start_and_wait(state: AppState, app: tauri::AppHandle) -> anyhow::Result<()> {
    let (ready_tx, ready_rx) = oneshot::channel();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = start_with_ready(state, app, Some(ready_tx)).await {
            tracing::error!("MCP server error: {error}");
        }
    });
    match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(anyhow::anyhow!(error)),
        Ok(Err(error)) => Err(anyhow::anyhow!("MCP 启动结果通道已关闭: {error}")),
        Err(_) => Err(anyhow::anyhow!("MCP 启动等待超时")),
    }
}

pub fn stop(state: &AppState) {
    let shutdown = {
        let mut inner = state.inner.write();
        inner.mcp_listening = false;
        inner.mcp_shutdown.take()
    };
    if let Some(shutdown) = shutdown {
        let _ = shutdown.send(true);
    }
}

/// Stop and wait until the old listener has released its socket.
pub async fn stop_and_wait(state: &AppState) -> anyhow::Result<()> {
    stop(state);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !state.inner.read().mcp_starting {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("MCP 旧服务停止超时"))
}

pub async fn restart(state: AppState, app: tauri::AppHandle) -> anyhow::Result<()> {
    stop_and_wait(&state).await?;
    if !state.inner.read().settings.mcp_enabled {
        return Ok(());
    }
    start_and_wait(state, app).await
}

pub fn validate_loopback(host: &str) -> anyhow::Result<()> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }
    let address = host
        .parse::<std::net::IpAddr>()
        .map_err(|_| anyhow::anyhow!("MCP 只能监听 loopback 地址"))?;
    if !address.is_loopback() {
        return Err(anyhow::anyhow!("MCP 只能监听 loopback 地址"));
    }
    Ok(())
}

async fn handle_post(
    State(runtime): State<HttpRuntime>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = validate_request_headers(&runtime.state, &headers) {
        return response;
    }
    let permit = match runtime.semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                McpResponse::err(None, -32000, "MCP request concurrency limit reached"),
                None,
            )
        }
    };
    let result =
        tokio::time::timeout(REQUEST_TIMEOUT, process_body(&runtime, &headers, body)).await;
    drop(permit);
    match result {
        Ok(response) => response,
        Err(_) => json_error(
            StatusCode::GATEWAY_TIMEOUT,
            McpResponse::err(None, -32000, "MCP request timed out"),
            None,
        ),
    }
}

async fn handle_get(State(runtime): State<HttpRuntime>, headers: HeaderMap) -> Response {
    if let Some(response) = validate_request_headers(&runtime.state, &headers) {
        return response;
    }
    // This server does not have server-initiated notifications yet. Advertise
    // that SSE is unavailable rather than leaving a client hanging on GET.
    let mut response = (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({
            "error": "server-initiated SSE is not supported; use POST /mcp"
        })),
    )
        .into_response();
    response.headers_mut().insert(
        axum::http::header::ALLOW,
        HeaderValue::from_static("POST, DELETE"),
    );
    response
}

async fn handle_delete(State(runtime): State<HttpRuntime>, headers: HeaderMap) -> Response {
    if let Some(response) = validate_request_headers(&runtime.state, &headers) {
        return response;
    }
    let Some(session_id) = header(&headers, "mcp-session-id") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Mcp-Session-Id is required"})),
        )
            .into_response();
    };
    if runtime.sessions.remove(session_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "MCP session not found"})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

fn validate_request_headers(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if let Some(origin) = header(headers, "origin") {
        if !valid_origin(origin) {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Origin is not allowed"})),
                )
                    .into_response(),
            );
        }
    }
    if let Some(accept) = header(headers, "accept") {
        if !accept.contains("application/json")
            && !accept.contains("text/event-stream")
            && !accept.contains("*/*")
        {
            return Some(
                (
                    StatusCode::NOT_ACCEPTABLE,
                    Json(json!({"error": "Accept must allow application/json"})),
                )
                    .into_response(),
            );
        }
    }
    if !mcp_auth::configured(state) {
        return Some((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "MCP access credential is not configured; generate it in Settings"})),
        )
            .into_response());
    }
    let Some(token) = bearer_token(headers) else {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authorization: Bearer <MCP access credential> is required"})),
            )
                .into_response(),
        );
    };
    if !mcp_auth::verify(state, token) {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid MCP access credential"})),
            )
                .into_response(),
        );
    }
    None
}

async fn process_body(runtime: &HttpRuntime, headers: &HeaderMap, body: Bytes) -> Response {
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => {
            return json_error(
                StatusCode::OK,
                McpResponse::err(None, -32700, format!("parse error: {error}")),
                None,
            )
        }
    };
    if value.is_array() {
        return json_error(
            StatusCode::OK,
            McpResponse::err(
                None,
                -32600,
                "JSON-RPC batches are not supported by this MCP endpoint",
            ),
            None,
        );
    }
    let request = match mcp_protocol::parse_request(value) {
        Ok(request) => request,
        Err(response) => return json_error(StatusCode::OK, response, None),
    };
    process_request(runtime, headers, request).await
}

async fn process_request(
    runtime: &HttpRuntime,
    headers: &HeaderMap,
    request: McpRequest,
) -> Response {
    if request.method == "initialize" {
        if request.id.is_none() {
            return json_error(
                StatusCode::OK,
                McpResponse::err(None, -32600, "initialize must be a request"),
                None,
            );
        }
        let info = match mcp_protocol::initialize(&request.params) {
            Ok(info) => info,
            Err(error) => {
                return json_error(
                    StatusCode::OK,
                    McpResponse::err(request.id, -32602, error).with_data(json!({
                        "supportedProtocolVersions": mcp_protocol::SUPPORTED_PROTOCOL_VERSIONS,
                    })),
                    None,
                )
            }
        };
        let session_id = format!("mcp-{}", uuid::Uuid::new_v4().simple());
        let client_id = format!("client-{}", uuid::Uuid::new_v4().simple());
        let context = ClientContext {
            client_id,
            client_name: info.client_name,
            protocol_version: info.protocol_version,
        };
        runtime.sessions.insert(
            session_id.clone(),
            ClientSession {
                context,
                initialized: false,
            },
        );
        return json_error(
            StatusCode::OK,
            McpResponse::ok(request.id, info.result),
            Some(session_id),
        );
    }

    let Some(session_id) = header(headers, "mcp-session-id").map(str::to_string) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            McpResponse::err(
                request.id,
                -32001,
                "Mcp-Session-Id is required after initialize",
            ),
            None,
        );
    };
    let Some(session) = runtime
        .sessions
        .get(&session_id)
        .map(|entry| entry.value().clone())
    else {
        return json_error(
            StatusCode::NOT_FOUND,
            McpResponse::err(request.id, -32001, "MCP session not found"),
            None,
        );
    };
    if let Some(version) = header(headers, "mcp-protocol-version") {
        if !mcp_protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return json_error(
                StatusCode::BAD_REQUEST,
                McpResponse::err(request.id, -32602, "unsupported MCP protocol version"),
                Some(session_id),
            );
        }
        if version != session.context.protocol_version {
            return json_error(
                StatusCode::BAD_REQUEST,
                McpResponse::err(
                    request.id,
                    -32602,
                    "MCP protocol version does not match this session",
                ),
                Some(session_id),
            );
        }
    }

    if request.method == "notifications/initialized" {
        if let Some(mut entry) = runtime.sessions.get_mut(&session_id) {
            entry.initialized = true;
        }
        return empty_response(StatusCode::ACCEPTED, Some(session_id));
    }
    if !session.initialized {
        return json_error(
            StatusCode::OK,
            McpResponse::err(
                request.id,
                -32002,
                "client has not sent notifications/initialized",
            ),
            Some(session_id),
        );
    }

    let is_notification = request.id.is_none();
    let response = mcp_protocol::dispatch(
        request,
        &runtime.state,
        runtime.app.as_ref(),
        &session.context,
    )
    .await;
    if is_notification {
        empty_response(StatusCode::ACCEPTED, Some(session_id))
    } else {
        json_error(StatusCode::OK, response, Some(session_id))
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = header(headers, "authorization")?;
    let token = value.strip_prefix("Bearer ")?;
    (!token.is_empty()).then_some(token)
}

pub(crate) fn valid_origin(origin: &str) -> bool {
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(scheme) = uri.scheme_str() else {
        return false;
    };
    if !matches!(scheme, "http" | "https" | "tauri") {
        return false;
    }
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("tauri.localhost")
        || host == "127.0.0.1"
        || host == "::1"
}

/// Axum 的 `Json` 默认只声明 `application/json`，部分客户端会按本地代码页二次解码。
/// 统一补上 `charset=utf-8`，错误体也适用。
fn with_utf8_charset(mut response: Response) -> Response {
    let is_json = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if is_json {
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
}

async fn add_utf8_charset(request: Request, next: Next) -> Response {
    with_utf8_charset(next.run(request).await)
}

fn json_error(status: StatusCode, response: McpResponse, session_id: Option<String>) -> Response {
    let mut output = (status, Json(response)).into_response();
    if let Some(session_id) = session_id {
        if let Ok(value) = HeaderValue::from_str(&session_id) {
            output.headers_mut().insert("mcp-session-id", value);
        }
    }
    output
}

fn empty_response(status: StatusCode, session_id: Option<String>) -> Response {
    let mut output = status.into_response();
    if let Some(session_id) = session_id {
        if let Ok(value) = HeaderValue::from_str(&session_id) {
            output.headers_mut().insert("mcp-session-id", value);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_bind_addresses_are_accepted() {
        assert!(validate_loopback("127.0.0.1").is_ok());
        assert!(validate_loopback("::1").is_ok());
        assert!(validate_loopback("localhost").is_ok());
        assert!(validate_loopback("0.0.0.0").is_err());
        assert!(validate_loopback("192.168.1.2").is_err());
    }

    #[test]
    fn browser_origins_are_restricted_to_local_applications() {
        assert!(valid_origin("http://localhost:1420"));
        assert!(valid_origin("http://127.0.0.1:8767"));
        assert!(valid_origin("tauri://localhost"));
        assert!(!valid_origin("https://example.com"));
        assert!(!valid_origin("null"));
        assert!(!valid_origin("file://local"));
    }

    fn authorized_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers
    }

    #[test]
    fn json_responses_declare_utf8_charset() {
        let response =
            with_utf8_charset(Json(json!({"error": "确认请求与当前客户端不匹配"})).into_response());
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn http_authentication_origin_and_accept_fail_closed() {
        let token = "csh_a-very-long-local-token";
        let state = AppState::new();
        // 测试不应依赖本机已保存的 MCP 凭证状态，先清空再验证“未配置 -> 503”。
        state.inner.write().mcp_credential_hash = None;
        assert_eq!(
            validate_request_headers(&state, &HeaderMap::new())
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        mcp_auth::install_test_credential(&state, token);

        assert_eq!(
            validate_request_headers(&state, &HeaderMap::new())
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            validate_request_headers(&state, &authorized_headers("csh_wrong-token"))
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut remote_origin = authorized_headers(token);
        remote_origin.insert("origin", HeaderValue::from_static("https://example.com"));
        assert_eq!(
            validate_request_headers(&state, &remote_origin)
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );

        let mut invalid_accept = authorized_headers(token);
        invalid_accept.insert("accept", HeaderValue::from_static("text/plain"));
        assert_eq!(
            validate_request_headers(&state, &invalid_accept)
                .unwrap()
                .status(),
            StatusCode::NOT_ACCEPTABLE
        );

        let mut local_origin = authorized_headers(token);
        local_origin.insert("origin", HeaderValue::from_static("tauri://localhost"));
        assert!(validate_request_headers(&state, &local_origin).is_none());
    }

    #[tokio::test]
    async fn streamable_http_session_handshake_requires_initialization() {
        let token = "csh_handshake-test-token";
        let runtime = HttpRuntime {
            state: AppState::new(),
            app: None,
            sessions: Arc::new(DashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        };
        mcp_auth::install_test_credential(&runtime.state, token);
        let initialize = mcp_protocol::parse_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "handshake-test", "version": "1"}
            }
        }))
        .unwrap();
        let response = process_request(&runtime, &HeaderMap::new(), initialize).await;
        assert_eq!(response.status(), StatusCode::OK);
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(value["result"]["serverInfo"]["name"], "Chena-Shell");

        let mut headers = authorized_headers(token);
        headers.insert(
            "mcp-session-id",
            HeaderValue::from_str(&session_id).unwrap(),
        );
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2025-06-18"),
        );
        let early_tools = mcp_protocol::parse_request(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        }))
        .unwrap();
        let response = process_request(&runtime, &headers, early_tools).await;
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], -32002);

        let initialized = mcp_protocol::parse_request(json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .unwrap();
        let response = process_request(&runtime, &headers, initialized).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let tools = mcp_protocol::parse_request(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list"
        }))
        .unwrap();
        let response = process_request(&runtime, &headers, tools).await;
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value["result"]["tools"].as_array().unwrap().len() > 0);

        let ping = mcp_protocol::parse_request(json!({
            "jsonrpc": "2.0", "id": 4, "method": "ping"
        }))
        .unwrap();
        let response = process_request(&runtime, &headers, ping).await;
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["result"], json!({}));

        let response = handle_delete(State(runtime), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
