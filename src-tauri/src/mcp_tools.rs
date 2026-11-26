//! MCP business tool handlers. Transport and JSON-RPC framing live elsewhere.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::approval::{self, ConfirmationKind, ConfirmationStatus};
use crate::mcp_protocol::ClientContext;
use crate::session::{AttachBinding, AttachRole, ConnectionDescriptor, SessionHandle};
use crate::state::{
    AIEventKind, AIPolicyOverride, AppState, AuthConfig, PortForwardConfig, SessionConfig,
    SessionDto, SettingsDto, SudoPasswordCache, SudoPasswordSource,
};

const DEFAULT_EXEC_TIMEOUT_MS: u64 = 30_000;
const MAX_EXEC_TIMEOUT_MS: u64 = 300_000;
const MAX_SFTP_BYTES: usize = 1024 * 1024;
const DEFAULT_SFTP_LIST_LIMIT: usize = 100;
const MAX_SFTP_LIST_LIMIT: usize = 500;

pub fn tools_list() -> Value {
    json!({
      "tools": [
        {
          "name": "session_create",
          "description": "Create a new SSH session (host/port/username/password) and optionally connect immediately. Saved to Chena-Shell's session list, so the human sees it in the UI. Requires human approval: the first call returns WAIT_CONFIRM, retry with confirmation_id after approval.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "host": { "type": "string", "description": "Target IP or hostname" },
              "port": { "type": "integer", "description": "SSH port, default 22" },
              "username": { "type": "string", "description": "Login user" },
              "password": { "type": "string", "description": "Password (password auth)" },
              "name": { "type": "string", "description": "Display name, defaults to host" },
              "connect": { "type": "boolean", "description": "Connect right after creating, default true" },
              "auto_connect": { "type": "boolean", "description": "Auto-connect on next app start, default false" },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["host", "username", "password"]
          }
        },
        {
          "name": "session_connect",
          "description": "Connect an existing (configured) SSH session by id. Requires human approval: the first call returns WAIT_CONFIRM, retry with confirmation_id after approval.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["session_id"]
          }
        },
        {
          "name": "session_disconnect",
          "description": "Close an SSH session's connection (the session config is kept). Requires human approval: the first call returns WAIT_CONFIRM, retry with confirmation_id after approval.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["session_id"]
          }
        },
        {
          "name": "session_list",
          "description": "List all configured SSH sessions and their connection + AI attach status. Secrets are never returned.",
          "inputSchema": { "type": "object", "properties": {} }
        },
        {
          "name": "session_attach",
          "description": "Request access to one terminal connection of an SSH session. mode=write is exclusive and permits exec; mode=read is replay-only; mode=diag is a non-exclusive read-only observer that may run allowlisted diagnostic commands without per-command approval. Multiple connections exist when the user duplicated the SSH channel. Pass connection_id from session_list.connections to bind a specific terminal; session_read then always reads that connection. If connection_id is omitted: one connection binds directly, multiple connections bind the user's active tab, and unknown active state returns AMBIGUOUS_CONNECTION. The request may be shown to a human for approval. If WAIT_CONFIRM is returned, retry with the same arguments and confirmation_id after approval. Optional task declares the troubleshooting goal for mode=write: read-only allowlisted commands inside the task skip per-command approval, while writes, high-risk commands and root still require confirmation; the task ends automatically.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string", "description": "Session ID from session_list" },
              "connection_id": { "type": "string", "description": "Terminal connection to bind, e.g. \"7:ch1\" or \"7:primary\" (see session_list.connections)" },
              "require_active": { "type": "boolean", "default": false, "description": "Require the target to be the terminal the user is currently viewing; later reads fail with ACTIVE_CONNECTION_CHANGED if the user switches tabs" },
              "reason": { "type": "string", "description": "Why you want to attach" },
              "mode": { "type": "string", "enum": ["write", "read", "diag"], "default": "write", "description": "write is exclusive and permits exec; read is replay-only; diag is non-exclusive and permits allowlisted read-only exec" },
              "permissions": { "type": "array", "items": { "type": "string", "enum": ["sftp", "sudo"] }, "description": "Optional writer permissions: sftp (file access), sudo (root exec, requires human approval per command)" },
              "task": { "type": "object", "description": "Optional task-level authorization for mode=write; read-only allowlisted commands inside the task skip per-command approval", "properties": { "goal": { "type": "string", "description": "Troubleshooting goal, e.g. locate why nginx returns 502" }, "ttl_minutes": { "type": "integer", "description": "Task lifetime in minutes; defaults to the configured maximum" } }, "required": ["goal"] },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["session_id", "reason"]
          }
        },
        {
          "name": "session_exec",
          "description": "Execute a shell command inside the attached session. writable tokens use blocklist → human confirmation → allow; a token with an active task auto-approves only allowlisted read-only commands. diag tokens can run allowlisted read-only commands only (no pipes, redirection, chaining or root), with no approval. read tokens cannot execute. If WAIT_CONFIRM is returned, retry with confirmation_id; an approval can be consumed only once. Set run_as=root to execute as root (requires sudo permission at attach; password is entered by the human in the UI and never passed to you).",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "command": { "type": "string", "description": "Command line to run" },
              "run_as": { "type": "string", "enum": ["user", "root"], "default": "user", "description": "Run as the login user (default) or as root (requires sudo permission)" },
              "cwd": { "type": "string", "description": "Working directory for run_as=root; absolute path, default /root" },
              "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds, 1000–300000; default 30000" },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["session_id", "attach_token", "command"]
          }
        },
        {
          "name": "session_read",
          "description": "Read retained terminal replay using a read or writer attach token. Pass after_seq from the previous next_seq for incremental reads.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "after_seq": { "type": "integer", "description": "Return chunks newer than this sequence; omit for the current retained window" }
            },
            "required": ["session_id", "attach_token"]
          }
        },
        {
          "name": "session_detach",
          "description": "Release the session. Your control token is invalidated and buffered human input is replayed after writer detach.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "reason": { "type": "string" }
            },
            "required": ["session_id", "attach_token", "reason"]
          }
        },
        {
          "name": "session_monitor",
          "description": "Get the latest resource snapshot for a session (CPU, memory, disk, network, load averages).",
          "inputSchema": {
            "type": "object",
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"]
          }
        },
        {
          "name": "forward_list",
          "description": "List port-forward rules of a session (local listen → remote dest) with running state.",
          "inputSchema": {
            "type": "object",
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"]
          }
        },
        {
          "name": "forward_add",
          "description": "Add a port-forward rule and start it immediately. kind=local (ssh -L), kind=remote (ssh -R). Requires a writer attach_token. listen_host defaults to 127.0.0.1; any non-loopback listen address triggers human approval (first call returns WAIT_CONFIRM).",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string", "description": "Writer attach token for this session" },
              "kind": { "type": "string", "enum": ["local", "remote"], "default": "local" },
              "listen_port": { "type": "integer" },
              "dest_host": { "type": "string", "default": "127.0.0.1" },
              "dest_port": { "type": "integer" },
              "listen_host": { "type": "string", "description": "Defaults to 127.0.0.1; non-loopback values require approval" },
              "confirmation_id": { "type": "string", "description": "ID returned by a previous WAIT_CONFIRM response" }
            },
            "required": ["session_id", "attach_token", "listen_port", "dest_port"]
          }
        },
        {
          "name": "forward_stop",
          "description": "Stop a running port-forward rule by id. Requires the writer attach_token of the session that owns the rule.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "forward_id": { "type": "string" },
              "attach_token": { "type": "string" }
            },
            "required": ["forward_id", "attach_token"]
          }
        },
        {
          "name": "quickcmd_list",
          "description": "List saved quick commands (label + command + group).",
          "inputSchema": { "type": "object", "properties": {} }
        },
        {
          "name": "quickcmd_run",
          "description": "Run a quick command through the same attached-token and security checks as session_exec.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "command": { "type": "string" },
              "timeout_ms": { "type": "integer" }
            },
            "required": ["session_id", "attach_token", "command"]
          }
        },
        {
          "name": "sftp_list",
          "description": "List a remote directory over an independent authenticated SFTP connection. Requires a writer token with sftp permission.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "path": { "type": "string", "default": "." },
              "limit": { "type": "integer", "description": "1-500 entries, default 100" }
            },
            "required": ["session_id", "attach_token"]
          }
        },
        {
          "name": "sftp_read",
          "description": "Read at most 1 MiB from a remote regular file as UTF-8 text or base64. Requires a writer token with sftp permission.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "path": { "type": "string" },
              "encoding": { "type": "string", "enum": ["text", "base64"], "default": "text" }
            },
            "required": ["session_id", "attach_token", "path"]
          }
        },
        {
          "name": "sftp_write",
          "description": "Write at most 1 MiB using a unique .part file and rename. Existing targets require one-time human approval. Requires a writer token with sftp permission.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "session_id": { "type": "string" },
              "attach_token": { "type": "string" },
              "path": { "type": "string" },
              "content": { "type": "string" },
              "encoding": { "type": "string", "enum": ["text", "base64"], "default": "text" },
              "confirmation_id": { "type": "string", "description": "Required after WAIT_CONFIRM when the target exists" }
            },
            "required": ["session_id", "attach_token", "path", "content"]
          }
        },
        {
          "name": "settings_get",
          "description": "Get an explicit allowlist of non-secret application settings. Security rules and credentials are never returned.",
          "inputSchema": { "type": "object", "properties": {} }
        }
      ]
    })
}

pub async fn call(
    params: Value,
    state: &AppState,
    app: &tauri::AppHandle,
    client: &ClientContext,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return json!({"error": "missing tool name"});
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "session_create" => session_create(args, state, app, client).await,
        "session_connect" => session_connect(args, state, app, client).await,
        "session_disconnect" => session_disconnect(args, state, client),
        "session_list" => session_list(state),
        "session_attach" => session_attach(args, state, client),
        "session_exec" => session_exec(args, state, client).await,
        "session_read" => session_read(args, state, client),
        "session_detach" => session_detach(args, state, client),
        "session_monitor" => session_monitor(args, state),
        "forward_list" => forward_list(args, state),
        "forward_add" => forward_add(args, state, client),
        "forward_stop" => forward_stop(args, state, client),
        "quickcmd_list" => json!({"quick_commands": state.inner.read().quick_commands.clone()}),
        "quickcmd_run" => session_exec(args, state, client).await,
        "sftp_list" => sftp_list(args, state, client).await,
        "sftp_read" => sftp_read(args, state, client).await,
        "sftp_write" => sftp_write(args, state, client).await,
        "settings_get" => json!({"settings": SettingsDto::from(&state.inner.read().settings)}),
        other => json!({"error": format!("unknown tool: {other}")}),
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, Value> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::trim)
        .ok_or_else(|| json!({"error": format!("missing {key}")}))
}

fn port_arg(args: &Value, key: &str, default: Option<u16>) -> Result<u16, Value> {
    let Some(raw) = args.get(key) else {
        return default.ok_or_else(|| json!({"error": format!("missing {key}")}));
    };
    let value = raw
        .as_u64()
        .ok_or_else(|| json!({"error": format!("{key} must be an integer")}))?;
    u16::try_from(value)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| json!({"error": format!("{key} must be between 1 and 65535")}))
}

fn get_handle(state: &AppState, session_id: &str) -> Result<Arc<SessionHandle>, Value> {
    state
        .inner
        .read()
        .sessions
        .get(session_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| json!({"error": "session not found"}))
}

async fn session_create(
    args: Value,
    state: &AppState,
    app: &tauri::AppHandle,
    client: &ClientContext,
) -> Value {
    let host = match string_arg(&args, "host") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let username = args
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("root")
        .trim()
        .to_string();
    let password = args
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let port = match port_arg(&args, "port", Some(22)) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let display_name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&host)
        .to_string();
    let auto_connect = args
        .get("auto_connect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let connect_now = args.get("connect").and_then(Value::as_bool).unwrap_or(true);
    let settings = state.inner.read().settings.clone();
    let id = format!("sess-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let config = SessionConfig {
        id: id.clone(),
        name: display_name.clone(),
        host,
        port,
        username,
        auth: AuthConfig::Password {
            password_ref: password,
        },
        proxy_jump: Vec::new(),
        group: "AI 创建".into(),
        ai_allowed: true,
        ai_policy: AIPolicyOverride::Inherit,
        ai_blocklist_regexes: Vec::new(),
        auto_connect,
        keepalive_secs: settings.default_keepalive_secs,
        connect_timeout_secs: settings.connect_timeout_secs,
        port_forwards: Vec::new(),
        log_enabled: settings.terminal_log_default_enabled,
    };
    if let Err(error) = crate::commands::validate_auth_config(&config.auth) {
        return json!({"error": error});
    }
    // 新建会话会把凭据写入本机凭据库并可能对外发起连接，必须人工确认一次。
    let preview = format!(
        "创建 SSH 会话 {}@{}:{}",
        config.username, config.host, config.port
    );
    if let Some(response) = require_approval(
        &args,
        state,
        client,
        &format!("mcp-create:{}", client.client_id),
        "AI 请求新建 SSH 会话",
        &preview,
    ) {
        return response;
    }
    let handle = Arc::new(SessionHandle::new(
        config,
        settings.global_blocklist,
        settings.confirm_list,
    ));
    state
        .inner
        .write()
        .sessions
        .insert(id.clone(), handle.clone());
    if let Err(error) = crate::commands::persist(state) {
        let _ = state.inner.read().sessions.remove(&id);
        return json!({"error": format!("会话保存失败: {error}")});
    }

    let mut connected = false;
    let mut error = None;
    // AI 不能在用户核验指纹前自动建立首条连接，只提示用户手动连接一次。
    let created = handle.config_snapshot();
    let host_key_pending =
        !crate::host_key::has_known_host(&created.host, created.port).unwrap_or(false);
    if connect_now && host_key_pending {
        error = Some(
            "HOST_KEY_CONFIRM_REQUIRED: 该主机密钥尚未人工确认，请在 Chena-Shell 界面中手动连接一次并核对指纹"
                .to_string(),
        );
    } else if connect_now {
        let st = state.clone();
        let ap = app.clone();
        match tokio::task::spawn_blocking(move || handle.connect(st, ap)).await {
            Ok(Ok(())) => connected = true,
            Ok(Err(e)) => error = Some(e),
            Err(e) => error = Some(format!("连接线程异常: {e}")),
        }
    }
    json!({
        "ok": true,
        "session_id": id,
        "name": display_name,
        "connected": connected,
        "error": error,
    })
}

async fn session_connect(
    args: Value,
    state: &AppState,
    app: &tauri::AppHandle,
    client: &ClientContext,
) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let handle = match get_handle(state, &sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let config = handle.config_snapshot();
    // AI 不能在用户未核验指纹前建立首条连接。
    if !crate::host_key::has_known_host(&config.host, config.port).unwrap_or(false) {
        return json!({
            "ok": false,
            "error": "HOST_KEY_CONFIRM_REQUIRED",
            "detail": "该主机的密钥尚未人工确认；请让用户在 Chena-Shell 界面中手动连接一次并核对指纹，然后再重试。"
        });
    }
    let preview = format!(
        "连接 SSH 会话 {sid}（{}@{}:{}）",
        config.username, config.host, config.port
    );
    if let Some(response) = require_approval(
        &args,
        state,
        client,
        &sid,
        "AI 请求连接已有 SSH 会话",
        &preview,
    ) {
        return response;
    }
    let st = state.clone();
    let ap = app.clone();
    match tokio::task::spawn_blocking(move || handle.connect(st, ap)).await {
        Ok(Ok(())) => json!({"ok": true, "session_id": sid, "connected": true}),
        Ok(Err(error)) => json!({"ok": false, "session_id": sid, "error": error}),
        Err(error) => {
            json!({"ok": false, "session_id": sid, "error": format!("连接线程异常: {error}")})
        }
    }
}

fn session_disconnect(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let handle = match get_handle(state, &sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let preview = format!("断开 SSH 会话 {sid}");
    if let Some(response) =
        require_approval(&args, state, client, &sid, "AI 请求断开 SSH 会话", &preview)
    {
        return response;
    }
    handle.force_detach(state);
    handle.close();
    approval::cancel_session(state, &sid, "会话已断开，未决审批已取消");
    let _ = state.inner.read().monitor_samples.remove(&sid);
    json!({"ok": true, "session_id": sid, "connected": false})
}

fn session_list(state: &AppState) -> Value {
    let sessions = state
        .inner
        .read()
        .sessions
        .iter()
        .map(|entry| {
            let handle = entry.value();
            let mut dto =
                serde_json::to_value(SessionDto::new(&handle.config_snapshot(), handle.status()))
                    .unwrap_or_else(|_| json!({}));
            dto["connections"] = json!(handle.connection_descriptors());
            dto["active_connection_id"] = json!(handle.active_connection_id());
            dto["active_revision"] = json!(handle.active_revision());
            dto
        })
        .collect::<Vec<_>>();
    json!({"sessions": sessions})
}

/// attach 选定的连接及其原因。
struct AttachSelection {
    descriptor: ConnectionDescriptor,
    binding: AttachBinding,
    reason: &'static str,
}

/// 按显式 connection_id / 活跃标签 / 单连接规则选出绑定目标。
fn select_connection(handle: &SessionHandle, args: &Value) -> Result<AttachSelection, Value> {
    let require_active = args
        .get("require_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let requested = args
        .get("connection_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let candidates = handle.connection_descriptors();
    if candidates.is_empty() {
        return Err(json!({
            "ok": false,
            "error": "CONNECTION_CLOSED",
            "detail": "该会话当前没有可用终端连接，请先连接或重新连接"
        }));
    }
    let (descriptor, reason) = match requested {
        Some(connection_id) => {
            let Some(found) = candidates
                .iter()
                .find(|item| item.connection_id == connection_id)
                .cloned()
            else {
                return Err(json!({
                    "ok": false,
                    "error": "CONNECTION_NOT_FOUND",
                    "connection_id": connection_id,
                    "candidates": candidates
                }));
            };
            if require_active && !found.is_active {
                return Err(json!({
                    "ok": false,
                    "error": "CONNECTION_NOT_ACTIVE",
                    "connection_id": found.connection_id,
                    "active_connection_id": handle.active_connection_id(),
                    "detail": "目标连接不是用户当前正在查看的终端"
                }));
            }
            (found, "explicit")
        }
        None => {
            if candidates.len() == 1 {
                (candidates[0].clone(), "only_connection")
            } else if let Some(active_id) = handle.active_connection_id() {
                match candidates
                    .iter()
                    .find(|item| item.connection_id == active_id)
                    .cloned()
                {
                    Some(found) => (found, "active"),
                    None => return Err(ambiguous_connection(&candidates)),
                }
            } else {
                return Err(ambiguous_connection(&candidates));
            }
        }
    };
    Ok(AttachSelection {
        binding: AttachBinding {
            connection_id: descriptor.connection_id.clone(),
            generation: descriptor.generation,
            require_active,
        },
        descriptor,
        reason,
    })
}

fn ambiguous_connection(candidates: &[ConnectionDescriptor]) -> Value {
    json!({
        "ok": false,
        "error": "AMBIGUOUS_CONNECTION",
        "detail": "该会话存在多个终端连接，且无法确定用户当前活跃的连接。请传入 connection_id 明确选择，或使用 require_active=true 要求活跃终端。",
        "candidates": candidates
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachMode {
    Write,
    Read,
    Diag,
}

impl AttachMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::Diag => "diag",
        }
    }
}

fn attach_request(args: &Value) -> Result<(AttachMode, bool, bool, String), Value> {
    let mode = match args.get("mode").and_then(Value::as_str).unwrap_or("write") {
        "write" => AttachMode::Write,
        "read" => AttachMode::Read,
        "diag" => AttachMode::Diag,
        _ => return Err(json!({"error": "mode must be write, read or diag"})),
    };
    let mut sftp = false;
    let mut sudo = false;
    if let Some(permissions) = args.get("permissions") {
        let Some(permissions) = permissions.as_array() else {
            return Err(json!({"error": "permissions must be an array"}));
        };
        for permission in permissions {
            match permission.as_str() {
                Some("sftp") => sftp = true,
                Some("sudo") => sudo = true,
                _ => {
                    return Err(
                        json!({"error": "INVALID_PERMISSION", "detail": "unsupported attach permission"}),
                    )
                }
            }
        }
    }
    if mode != AttachMode::Write && sftp {
        return Err(json!({"error": "only mode=write can request sftp permission"}));
    }
    if mode != AttachMode::Write && sudo {
        return Err(json!({"error": "INVALID_PERMISSION", "detail": "sudo requires mode=write"}));
    }
    let mut grants = Vec::new();
    if sftp {
        grants.push("sftp");
    }
    if sudo {
        grants.push("sudo");
    }
    let descriptor = format!(
        "attach mode={} permissions={}",
        mode.as_str(),
        grants.join(",")
    );
    Ok((mode, sftp, sudo, descriptor))
}

/// 解析任务级授权参数：task 支持字符串或 {goal, ttl_minutes}。
fn attach_task_request(
    args: &Value,
    settings: &crate::state::GlobalSettings,
) -> Result<Option<(String, u32)>, Value> {
    let Some(task) = args.get("task") else {
        return Ok(None);
    };
    if task.is_null() {
        return Ok(None);
    }
    let (goal, ttl) = match task {
        Value::String(goal) => (goal.trim().to_string(), None),
        Value::Object(map) => {
            let goal = map
                .get("goal")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let ttl = match map.get("ttl_minutes") {
                None | Some(Value::Null) => None,
                Some(value) => match value.as_u64() {
                    Some(value) => Some(value as u32),
                    None => return Err(json!({"error": "task.ttl_minutes must be an integer"})),
                },
            };
            (goal, ttl)
        }
        _ => return Err(json!({"error": "task must be a string or {goal, ttl_minutes} object"})),
    };
    if goal.is_empty() {
        return Err(json!({"error": "task.goal must not be empty"}));
    }
    let max = settings.ai_task_max_minutes.max(1);
    let ttl = ttl.unwrap_or(max).clamp(1, max);
    Ok(Some((goal.chars().take(200).collect(), ttl)))
}

fn session_attach(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let (mode, sftp, sudo, descriptor) = match attach_request(&args) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let settings = state.inner.read().settings.clone();
    if sudo && !settings.sudo_enabled {
        return json!({"ok": false, "error": "SUDO_DISABLED", "detail": "请先在「设置 → AI 安全」中开启「允许 AI 以 root 执行」"});
    }
    if mode == AttachMode::Diag && !settings.ai_diag_enabled {
        return json!({"ok": false, "error": "DIAG_DISABLED", "detail": "diag 只读模式已在设置中关闭"});
    }
    let task = match attach_task_request(&args, &settings) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if task.is_some() && mode != AttachMode::Write {
        return json!({"ok": false, "error": "INVALID_TASK", "detail": "任务级授权仅支持 mode=write"});
    }
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("ai request")
        .chars()
        .take(500)
        .collect::<String>();
    let handle = match get_handle(state, sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    // 每次 attach 都重新解析目标连接：审批期间连接关闭或重连时会直接报错，而不是绑错。
    let selection = match select_connection(&handle, &args) {
        Ok(value) => value,
        Err(error) => return error,
    };
    // 先做冲突与状态校验，再消费审批：校验失败不得消耗人工审批单。
    let precheck = match mode {
        AttachMode::Write => handle.precheck_write_attach(),
        AttachMode::Read => handle.precheck_read_attach(),
        AttachMode::Diag => handle.precheck_diag_attach(),
    };
    if let Err(error) = precheck {
        return json!({"ok": false, "error": error});
    }
    let mut descriptor = format!(
        "{descriptor} connection={} require_active={}",
        selection.descriptor.connection_id, selection.binding.require_active
    );
    if let Some((goal, ttl)) = &task {
        descriptor.push_str(&format!(" task={goal} ttl={ttl}m"));
    }
    if let Some(confirmation_id) = args.get("confirmation_id").and_then(Value::as_str) {
        let Some(existing) = approval::get(state, confirmation_id) else {
            return json!({"ok": false, "error": "confirmation not found", "confirmation_id": confirmation_id});
        };
        if existing.command.as_deref() != Some(descriptor.as_str()) {
            return json!({"ok": false, "error": "attach mode, permissions or target connection do not match the approved request", "confirmation_id": confirmation_id});
        }
        match approval::consume_approved(
            state,
            confirmation_id,
            ConfirmationKind::Attach,
            sid,
            &client.client_id,
        ) {
            Ok(_) => {
                return approve_attach(
                    handle, state, client, sid, &reason, mode, sftp, sudo, selection, task,
                )
            }
            Err(error) if error == "WAIT_CONFIRM" => {
                return pending_response(state, confirmation_id)
            }
            Err(error) => {
                return json!({"ok": false, "error": error, "confirmation_id": confirmation_id})
            }
        }
    }
    if settings.mcp_require_attach_approval {
        let confirmation = match approval::request(
            state,
            ConfirmationKind::Attach,
            sid,
            &client.client_id,
            &client.client_name,
            &reason,
            Some(descriptor),
            Some("attach requires human approval".into()),
            None,
        ) {
            Ok(value) => value,
            Err(error) => return json!({"ok": false, "error": error}),
        };
        state.log_ai_event_detailed(
            sid,
            AIEventKind::WaitingConfirm {
                command: format!(
                    "[attach:{} → {}]",
                    mode.as_str(),
                    selection.descriptor.connection_id
                ),
                rule: "attach requires human approval".into(),
            },
            None,
            None,
            &client.client_name,
            confirmation.id.clone(),
            Some("attach requires human approval".into()),
            None,
            None,
        );
        let mut pending = pending_response(state, &confirmation.id);
        pending["target"] = json!(selection.descriptor);
        return pending;
    }
    approve_attach(
        handle, state, client, sid, &reason, mode, sftp, sudo, selection, task,
    )
}

#[allow(clippy::too_many_arguments)]
fn approve_attach(
    handle: Arc<SessionHandle>,
    state: &AppState,
    client: &ClientContext,
    sid: &str,
    reason: &str,
    mode: AttachMode,
    sftp: bool,
    sudo: bool,
    selection: AttachSelection,
    task: Option<(String, u32)>,
) -> Value {
    let settings = state.inner.read().settings.clone();
    let ttl = settings.attach_token_ttl_minutes;
    let binding = selection.binding.clone();
    let result = match mode {
        AttachMode::Write => handle.begin_write_attach_bound(
            state,
            reason,
            &client.client_id,
            ttl,
            sftp,
            sudo,
            binding,
        ),
        AttachMode::Read => {
            handle.begin_read_attach_bound(state, reason, &client.client_id, ttl, binding, false)
        }
        AttachMode::Diag => {
            handle.begin_read_attach_bound(state, reason, &client.client_id, ttl, binding, true)
        }
    };
    match result {
        Ok(token) => {
            let mut permissions: Vec<&str> = match mode {
                AttachMode::Write if sftp => vec!["exec", "read", "detach", "sftp"],
                AttachMode::Write => vec!["exec", "read", "detach"],
                AttachMode::Read => vec!["read", "detach"],
                AttachMode::Diag => vec!["read", "detach", "diag"],
            };
            if sudo {
                permissions.push("sudo");
            }
            let task_info = task
                .as_ref()
                .map(|(goal, ttl_minutes)| handle.set_attach_task(goal, *ttl_minutes));
            let target = &selection.descriptor;
            let mut response = json!({
                "ok": true,
                "attach_token": token,
                "session_id": sid,
                "client_id": client.client_id,
                "mode": mode.as_str(),
                "permissions": permissions,
                "connection_id": target.connection_id,
                "kind": target.kind,
                "channel_id": target.channel_id,
                "generation": target.generation,
                "connected_at": target.connected_at,
                "login_user": target.login_user,
                "prompt_user": target.prompt_user,
                "prompt_user_source": target.prompt_user_source,
                "prompt_observed_at": target.prompt_observed_at,
                "is_active": target.is_active,
                "active_revision": handle.active_revision(),
                "selection_reason": selection.reason,
                "expires_in_seconds": ttl.clamp(1, 1440).saturating_mul(60),
                "note": "Token is bound to this MCP client, session and terminal connection; reads always target this connection."
            });
            if sudo {
                response["sudo"] = json!({
                    "enabled": true,
                    "require_confirm": settings.sudo_require_confirm,
                    "quota_remaining": settings.sudo_quota_per_attach.saturating_sub(handle.sudo_quota_used()),
                });
            }
            if let Some(task_info) = task_info {
                response["task"] = json!({
                    "goal": task_info.goal,
                    "expires_at": task_info.expires_at,
                });
            }
            if mode == AttachMode::Diag {
                let root_login = target.login_user == "root";
                let allowlist =
                    diag_allowlist_for_login(&settings.ai_diag_allowlist, &target.login_user);
                response["diag_allowlist"] = json!(allowlist);
                response["diag_root_login"] = json!(root_login);
                response["note"] = if root_login {
                    json!("diag is a non-exclusive read-only observer; this session logs in as root, so only built-in read-only entries are accepted.")
                } else {
                    json!("diag is a non-exclusive read-only observer; session_exec only accepts allowlisted commands.")
                };
            }
            response
        }
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn pending_response(state: &AppState, confirmation_id: &str) -> Value {
    match approval::get(state, confirmation_id) {
        Some(confirmation) if confirmation.status == ConfirmationStatus::Pending => {
            let mut response = json!({
                "ok": false,
                "error": "WAIT_CONFIRM",
                "confirmation_id": confirmation.id,
                "status": "pending",
                "expires_at": confirmation.expires_at,
                "note": "Human approval is required in Chena-Shell; retry with this confirmation_id after approval."
            });
            if confirmation.run_as.as_deref() == Some("root") {
                response["kind"] = json!("sudo");
                response["preview"] = json!({
                    "command": confirmation.command_preview,
                    "run_as": "root",
                    "rule": confirmation.rule,
                });
            }
            response
        }
        Some(confirmation) if confirmation.status == ConfirmationStatus::Approved => json!({
            "ok": false,
            "error": "WAIT_CONFIRM",
            "confirmation_id": confirmation.id,
            "status": "approved",
            "note": "Approval is ready; retry with this confirmation_id to consume it once."
        }),
        Some(confirmation) => json!({
            "ok": false,
            "error": "CONFIRMATION_NOT_APPROVED",
            "confirmation_id": confirmation.id,
            "status": confirmation.status,
            "reason": confirmation.resolution,
        }),
        None => {
            json!({"ok": false, "error": "confirmation not found", "confirmation_id": confirmation_id})
        }
    }
}

/// 管理类 MCP 操作（建会话、连接、断开、非回环端口转发）必须经人工确认一次。
///
/// 返回 `None` 表示确认已通过，可以继续执行；返回 `Some(response)` 表示应直接返回给客户端。
fn require_approval(
    args: &Value,
    state: &AppState,
    client: &ClientContext,
    session_id: &str,
    reason: &str,
    preview: &str,
) -> Option<Value> {
    match args.get("confirmation_id").and_then(Value::as_str) {
        None => {
            match approval::request(
                state,
                ConfirmationKind::Exec,
                session_id,
                &client.client_id,
                &client.client_name,
                reason,
                Some(preview.to_string()),
                None,
                None,
            ) {
                Ok(confirmation) => Some(pending_response(state, &confirmation.id)),
                Err(error) => Some(json!({"ok": false, "error": error})),
            }
        }
        Some(confirmation_id) => match approval::consume_approved(
            state,
            confirmation_id,
            ConfirmationKind::Exec,
            session_id,
            &client.client_id,
        ) {
            Ok(confirmation) => {
                // 批准只对当次目标有效，不能用创建 A 的批准去创建 B。
                if let Some(original) = confirmation.command.as_deref() {
                    if original != preview {
                        return Some(json!({
                            "ok": false,
                            "error": "confirmation does not match this operation",
                            "confirmation_id": confirmation_id,
                        }));
                    }
                }
                None
            }
            Err(error) if error == "WAIT_CONFIRM" => Some(pending_response(state, confirmation_id)),
            Err(error) => {
                Some(json!({"ok": false, "error": error, "confirmation_id": confirmation_id}))
            }
        },
    }
}

async fn session_exec(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let token = match string_arg(&args, "attach_token") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let supplied_command = args.get("command").and_then(Value::as_str).unwrap_or("");
    let timeout_ms = match args.get("timeout_ms") {
        None => DEFAULT_EXEC_TIMEOUT_MS,
        Some(value) => match value.as_u64() {
            Some(value) => value,
            None => return json!({"error": "timeout_ms must be an integer"}),
        },
    };
    if !(1_000..=MAX_EXEC_TIMEOUT_MS).contains(&timeout_ms) {
        return json!({"error": "timeout_ms must be between 1000 and 300000 (1–300 seconds)"});
    }
    let handle = match get_handle(state, &sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if !handle.config_snapshot().ai_allowed {
        return json!({"error": "session has AI disabled"});
    }
    let role = match handle.validate_attach_role(token, &client.client_id) {
        Ok(role) => role,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    if role == AttachRole::Read {
        return json!({
            "ok": false,
            "error": "READ_ONLY_TOKEN",
            "detail": "只读观察者不能执行命令；诊断请以 mode=diag 接入，写操作请以 mode=write 接入。"
        });
    }

    let run_as = match args.get("run_as").and_then(Value::as_str).unwrap_or("user") {
        "user" => "user",
        "root" => "root",
        _ => {
            return json!({"ok": false, "error": "INVALID_PERMISSION", "detail": "run_as must be user or root"})
        }
    };
    if role == AttachRole::Diag {
        if run_as == "root" {
            return json!({"ok": false, "error": "DIAG_READ_ONLY_ONLY", "detail": "diag 模式不支持 run_as=root"});
        }
        return session_exec_diag(
            &args,
            state,
            client,
            handle,
            &sid,
            supplied_command,
            timeout_ms,
        )
        .await;
    }
    let cwd = match args.get("cwd") {
        None | Some(Value::Null) => "/root".to_string(),
        Some(value) => match value.as_str() {
            Some(path) if path.starts_with('/') => path.to_string(),
            _ => return json!({"ok": false, "error": "cwd must be an absolute path"}),
        },
    };
    if run_as == "root" {
        return session_exec_root(
            &args,
            state,
            client,
            handle,
            &sid,
            supplied_command,
            timeout_ms,
            &cwd,
        )
        .await;
    }

    let mut command = supplied_command.to_string();
    let mut approved_confirmation_id: Option<String> = None;
    if let Some(confirmation_id) = args.get("confirmation_id").and_then(Value::as_str) {
        let Some(existing) = approval::get(state, confirmation_id) else {
            return json!({"ok": false, "error": "confirmation not found", "confirmation_id": confirmation_id});
        };
        match existing.status {
            ConfirmationStatus::Pending => return pending_response(state, confirmation_id),
            ConfirmationStatus::Approved => {}
            _ => {
                return json!({
                    "ok": false,
                    "error": "CONFIRMATION_NOT_APPROVED",
                    "confirmation_id": confirmation_id,
                    "status": existing.status,
                    "reason": existing.resolution,
                })
            }
        }
        if let Some(original) = existing.command.as_deref() {
            if !supplied_command.is_empty() && supplied_command != original {
                return json!({"ok": false, "error": "command does not match the approved command", "confirmation_id": confirmation_id});
            }
            command = original.into();
        }
        // 审批单在真正执行前才消费：Block、参数错误或执行失败都不会白白消耗。
        approved_confirmation_id = Some(confirmation_id.to_string());
    }
    if command.trim().is_empty() {
        return json!({"error": "missing command"});
    }

    let settings = state.inner.read().settings.clone();
    let check = handle
        .security_engine
        .check(&command, settings.default_policy);
    match check {
        crate::security::CheckResult::Block { rule } => {
            let audit = state.audit_command(&command);
            state.log_ai_event_detailed(
                &sid,
                AIEventKind::Blocked {
                    command: audit,
                    rule: rule.clone(),
                },
                None,
                None,
                &client.client_name,
                String::new(),
                Some(rule.clone()),
                Some("命中阻断规则".into()),
                None,
            );
            json!({"ok": false, "error": "BLOCKED", "rule": rule})
        }
        crate::security::CheckResult::WaitConfirm { rule }
            if approved_confirmation_id.is_none() =>
        {
            // 任务级授权：任务有效期内，内置白名单只读命令免逐条审批（写/高危/root 仍走确认）。
            // 只认内置条目：用户自定义的宽条目无法借任务期绕过逐条审批。
            let task_active = handle
                .attach_task_snapshot()
                .is_some_and(|task| task.expires_at > chrono::Utc::now());
            let login_user = handle.config_snapshot().username;
            let task_allowlist = task_allowlist_for_login(&settings.ai_diag_allowlist, &login_user);
            if task_active && readonly_command_allowed(&command, &task_allowlist) {
                state.log_ai_event_detailed(
                    &sid,
                    AIEventKind::Confirmed {
                        command: state.audit_command(&command),
                    },
                    None,
                    None,
                    &client.client_name,
                    String::new(),
                    Some(rule),
                    Some("任务内只读命令自动放行".into()),
                    None,
                );
                return execute_allowed(handle, state, client, &sid, command, timeout_ms, "").await;
            }
            let confirmation = match approval::request(
                state,
                ConfirmationKind::Exec,
                &sid,
                &client.client_id,
                &client.client_name,
                "command requires human approval",
                Some(command.clone()),
                Some(rule.clone()),
                None,
            ) {
                Ok(value) => value,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            state.log_ai_event_detailed(
                &sid,
                AIEventKind::WaitingConfirm {
                    command: state.audit_command(&command),
                    rule: rule.clone(),
                },
                None,
                None,
                &client.client_name,
                confirmation.id.clone(),
                Some(rule.clone()),
                None,
                None,
            );
            let mut result = pending_response(state, &confirmation.id);
            result["rule"] = json!(rule);
            result
        }
        crate::security::CheckResult::Allow | crate::security::CheckResult::WaitConfirm { .. } => {
            let mut approved_confirmation = None;
            if let Some(confirmation_id) = approved_confirmation_id.as_deref() {
                match approval::consume_approved(
                    state,
                    confirmation_id,
                    ConfirmationKind::Exec,
                    &sid,
                    &client.client_id,
                ) {
                    Ok(value) => approved_confirmation = Some(value),
                    Err(error) if error == "WAIT_CONFIRM" => {
                        return pending_response(state, confirmation_id)
                    }
                    Err(error) => {
                        return json!({"ok": false, "error": error, "confirmation_id": confirmation_id})
                    }
                }
            }
            if let Some(confirmation) = approved_confirmation {
                state.log_ai_event_detailed(
                    &sid,
                    AIEventKind::Confirmed {
                        command: state.audit_command(&command),
                    },
                    None,
                    None,
                    &client.client_name,
                    confirmation.id,
                    confirmation.rule,
                    None,
                    None,
                );
            }
            let result =
                execute_allowed(handle, state, client, &sid, command, timeout_ms, "").await;
            if result.get("ok").and_then(Value::as_bool) != Some(true) {
                if let Some(confirmation_id) = approved_confirmation_id.as_deref() {
                    approval::revert_to_approved(state, confirmation_id);
                }
            }
            result
        }
    }
}

/// 以 root 执行单条命令（方案 B）。密码只由用户在本机 UI 输入，绝不经过 AI。
#[allow(clippy::too_many_arguments)]
async fn session_exec_root(
    args: &Value,
    state: &AppState,
    client: &ClientContext,
    handle: Arc<SessionHandle>,
    sid: &str,
    supplied_command: &str,
    timeout_ms: u64,
    cwd: &str,
) -> Value {
    let settings = state.inner.read().settings.clone();
    if !settings.sudo_enabled {
        return json!({"ok": false, "error": "SUDO_DISABLED"});
    }
    if let Some(remaining) = handle.sudo_lockout_remaining_secs() {
        return json!({"ok": false, "error": "SUDO_AUTH_FAILED", "locked_for_seconds": remaining});
    }
    if handle.sudo_quota_used() >= settings.sudo_quota_per_attach {
        return json!({
            "ok": false,
            "error": "SUDO_QUOTA_EXCEEDED",
            "quota": settings.sudo_quota_per_attach,
            "note": "请 detach 后重新 attach 以重置配额"
        });
    }

    let mut command = supplied_command.to_string();
    let mut approved_confirmation_id: Option<String> = None;
    if let Some(confirmation_id) = args.get("confirmation_id").and_then(Value::as_str) {
        let Some(existing) = approval::get(state, confirmation_id) else {
            return json!({"ok": false, "error": "confirmation not found", "confirmation_id": confirmation_id});
        };
        if existing.run_as.as_deref() != Some("root") {
            return json!({"ok": false, "error": "confirmation does not match run_as=root", "confirmation_id": confirmation_id});
        }
        match existing.status {
            ConfirmationStatus::Pending => return pending_response(state, confirmation_id),
            ConfirmationStatus::Approved => {}
            _ => {
                return json!({
                    "ok": false,
                    "error": "CONFIRMATION_NOT_APPROVED",
                    "confirmation_id": confirmation_id,
                    "status": existing.status,
                    "reason": existing.resolution,
                })
            }
        }
        if let Some(original) = existing.command.as_deref() {
            if !supplied_command.is_empty() && supplied_command != original {
                return json!({"ok": false, "error": "command does not match the approved command", "confirmation_id": confirmation_id});
            }
            command = original.to_string();
        }
        approved_confirmation_id = Some(confirmation_id.to_string());
    }
    if command.trim().is_empty() {
        return json!({"error": "missing command"});
    }

    let global_default = settings.default_policy;
    let check = handle
        .security_engine
        .check_with_run_as(&command, global_default, true);
    match check {
        crate::security::CheckResult::Block { rule } => {
            state.log_ai_event_detailed(
                sid,
                AIEventKind::Blocked {
                    command: state.audit_command(&command),
                    rule: rule.clone(),
                },
                None,
                None,
                &client.client_name,
                String::new(),
                Some(rule.clone()),
                Some("命中阻断规则（root）".into()),
                None,
            );
            return json!({"ok": false, "error": "BLOCKED", "rule": rule});
        }
        crate::security::CheckResult::WaitConfirm { rule }
            if approved_confirmation_id.is_none() && settings.sudo_require_confirm =>
        {
            let confirmation = match approval::request(
                state,
                ConfirmationKind::Exec,
                sid,
                &client.client_id,
                &client.client_name,
                "AI 请求以 root 执行命令",
                Some(command.clone()),
                Some(rule.clone()),
                Some("root"),
            ) {
                Ok(value) => value,
                Err(error) => return json!({"ok": false, "error": error}),
            };
            state.log_ai_event_detailed(
                sid,
                AIEventKind::WaitingConfirm {
                    command: state.audit_command(&command),
                    rule: rule.clone(),
                },
                None,
                None,
                &client.client_name,
                confirmation.id.clone(),
                Some(rule.clone()),
                None,
                None,
            );
            let mut result = pending_response(state, &confirmation.id);
            result["rule"] = json!(rule);
            return result;
        }
        _ => {}
    }

    let password = match resolve_sudo_password(&handle, state, &settings).await {
        Ok(value) => value,
        Err(value) => return value,
    };
    // 走到这里才会真正下发命令：此时才消费审批单，密码获取失败仍可复用审批。
    if let Some(confirmation_id) = approved_confirmation_id.as_deref() {
        if let Err(error) = approval::consume_approved(
            state,
            confirmation_id,
            ConfirmationKind::Exec,
            sid,
            &client.client_id,
        ) {
            return json!({"ok": false, "error": error, "confirmation_id": confirmation_id});
        }
    }
    let password_for_cache = password.clone();
    let config = handle.config_snapshot();
    handle.sudo_consume_quota();

    let started = std::time::Instant::now();
    let started_at = chrono::Utc::now();
    let handle_for_task = handle.clone();
    let command_for_task = command.clone();
    let cwd_for_task = cwd.to_string();
    let result = tokio::task::spawn_blocking(move || {
        handle_for_task.exec_as_root_with_timeout(
            &command_for_task,
            &cwd_for_task,
            Duration::from_millis(timeout_ms),
            password,
        )
    })
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let response = match result {
        Ok(Ok((code, stdout, stderr))) => {
            handle.sudo_clear_failures();
            cache_sudo_password(
                &handle,
                state,
                &settings,
                &config,
                password_for_cache.as_deref(),
            );
            let timed_out = stderr.contains("[timeout]") || stderr.contains("命令执行超时");
            state.log_ai_event_detailed(
                sid,
                AIEventKind::Exec {
                    command: state.audit_command(&command),
                    args: "run_as=root".into(),
                },
                Some(code),
                Some(duration_ms),
                &client.client_name,
                String::new(),
                None,
                timed_out.then(|| "命令执行超时，exec channel 已关闭".into()),
                Some(started_at),
            );
            if timed_out {
                json!({
                    "ok": false, "error": "TIMEOUT", "timed_out": true,
                    "run_as": "root", "sudo_used": password_for_cache.is_some(),
                    "cwd": cwd, "command": command,
                    "exit_code": code, "stdout": stdout, "stderr": stderr,
                    "duration_ms": duration_ms,
                })
            } else {
                json!({
                    "ok": true,
                    "run_as": "root",
                    "sudo_used": password_for_cache.is_some(),
                    "cwd": cwd,
                    "command": command,
                    "exit_code": code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "duration_ms": duration_ms,
                })
            }
        }
        Ok(Err(kind)) => {
            // 提权认证类错误：按类型返回并维护失败锁定。
            let mut response = json!({"ok": false, "error": kind, "run_as": "root"});
            match kind.as_str() {
                "SUDO_AUTH_FAILED" => {
                    handle.sudo_forget_password();
                    let locked = handle.sudo_record_failure(
                        settings.sudo_max_failures,
                        settings.sudo_lockout_seconds,
                    );
                    if let Some(remaining) = handle.sudo_lockout_remaining_secs() {
                        response["locked_for_seconds"] = json!(remaining);
                    } else if locked {
                        response["locked_for_seconds"] = json!(settings.sudo_lockout_seconds);
                    }
                }
                "SUDO_PASSWORD_REQUIRED" => {
                    handle.sudo_forget_password();
                }
                _ => {}
            }
            state.log_ai_event_detailed(
                sid,
                AIEventKind::Exec {
                    command: state.audit_command(&command),
                    args: "run_as=root".into(),
                },
                None,
                Some(duration_ms),
                &client.client_name,
                String::new(),
                None,
                Some(kind),
                Some(started_at),
            );
            response
        }
        Err(error) => json!({"ok": false, "error": format!("命令执行线程异常: {error}")}),
    };
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        // 执行失败/超时：把审批单回滚为 Approved，允许直接重试而不必重新审批。
        if let Some(confirmation_id) = approved_confirmation_id.as_deref() {
            approval::revert_to_approved(state, confirmation_id);
        }
    }
    response
}

/// 解析本次 root 执行使用的 sudo 密码。返回 None 表示走免密 `sudo -n`。
async fn resolve_sudo_password(
    handle: &Arc<SessionHandle>,
    state: &AppState,
    settings: &crate::state::GlobalSettings,
) -> Result<Option<String>, Value> {
    if matches!(
        settings.sudo_password_source,
        SudoPasswordSource::Passwordless
    ) {
        return Ok(None);
    }
    if let Some(cached) = handle.sudo_cached_password() {
        return Ok(Some(cached));
    }
    let config = handle.config_snapshot();
    let key = format!("{}:{}:{}", config.host, config.port, config.username);
    if matches!(settings.sudo_password_cache, SudoPasswordCache::App) {
        if let Some(entry) = state.inner.read().sudo_app_passwords.get(&key) {
            return Ok(Some(entry.clone()));
        }
    }
    if matches!(settings.sudo_password_source, SudoPasswordSource::Session) {
        if let AuthConfig::Password { password_ref } = &config.auth {
            if let Ok(password) = crate::secret_store::resolve_secret(password_ref) {
                if !password.is_empty() {
                    return Ok(Some(password));
                }
            }
        }
    }
    prompt_sudo_password(state, &config).await
}

/// 请求前端弹出密码框；超时或取消返回 SUDO_PASSWORD_REQUIRED。
async fn prompt_sudo_password(
    state: &AppState,
    config: &SessionConfig,
) -> Result<Option<String>, Value> {
    let request_id = format!("sp-{}", uuid::Uuid::new_v4().simple());
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .inner
        .read()
        .sudo_prompts
        .insert(request_id.clone(), tx);
    crate::state::emit_app_event(
        "sudo-password-request",
        json!({
            "request_id": request_id,
            "session_id": config.id,
            "host": config.host,
            "port": config.port,
            "username": config.username,
            "expires_in_seconds": 30,
        }),
    );
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(Some(password))) if !password.is_empty() => Ok(Some(password)),
        _ => {
            // 超时或用户取消：清理请求，返回明确错误码。
            state.inner.read().sudo_prompts.remove(&request_id);
            Err(json!({"ok": false, "error": "SUDO_PASSWORD_REQUIRED"}))
        }
    }
}

/// 按缓存策略保存密码；None 策略不落任何内存。
fn cache_sudo_password(
    handle: &Arc<SessionHandle>,
    state: &AppState,
    settings: &crate::state::GlobalSettings,
    config: &SessionConfig,
    password: Option<&str>,
) {
    let Some(password) = password else { return };
    match settings.sudo_password_cache {
        SudoPasswordCache::None => {}
        SudoPasswordCache::Attach => handle.sudo_store_password(password),
        SudoPasswordCache::App => {
            handle.sudo_store_password(password);
            let key = format!("{}:{}:{}", config.host, config.port, config.username);
            state
                .inner
                .read()
                .sudo_app_passwords
                .insert(key, password.to_string());
        }
    }
}

/// shell 元字符：diag 只读模式一律拒绝，防止白名单前缀被拼接成任意命令。
const SHELL_METACHARS: [&str; 9] = [";", "|", "&", ">", "<", "`", "$(", "${", "\\"];

/// 命令前缀对应的危险参数：即使命令名在白名单内，也不能借参数变成写操作或无限跟随。
const DANGEROUS_ARGS: &[(&str, &[&str])] = &[
    ("tail", &["-f", "-F", "--follow", "--retry", "--pid"]),
    (
        "journalctl",
        &[
            "-f",
            "--follow",
            "--vacuum-size",
            "--vacuum-time",
            "--vacuum-files",
            "--rotate",
            "--flush",
            "--setup-keys",
            "--relinquish-var",
            "--smart-relinquish-var",
        ],
    ),
    ("crontab", &["-r", "-e", "-i"]),
    ("date", &["-s", "--set"]),
    ("ping", &["-f", "--flood"]),
    ("file", &["-C", "--compile"]),
    // dig 的 -f 会把任意文件按查询逐行读入并回显解析内容。
    ("dig", &["-f", "--file"]),
    // sar 的 -o 写盘、-f 读任意文件。
    ("sar", &["-o", "-f"]),
    (
        "curl",
        &[
            "-X",
            "--request",
            "-o",
            "--output",
            "-O",
            "--remote-name",
            "-T",
            "--upload-file",
            "-K",
            "--config",
            "-d",
            "--data",
            "-F",
            "--form",
            "--json",
            "-D",
            "--dump-header",
            "-c",
            "--cookie-jar",
            "--trace",
            "--trace-ascii",
        ],
    ),
    ("docker logs", &["-f", "--follow"]),
    ("kubectl logs", &["-f", "--follow"]),
    ("ss", &["-K", "--kill"]),
    ("dmesg", &["-C", "-c", "--clear", "--read-clear"]),
];

/// 去掉成对引号后再比对，避免用 `"-f"` 这类写法绕过参数检查。
fn normalize_token(token: &str) -> &str {
    token.trim_matches(|c| c == '\'' || c == '"')
}

/// 命令是否以指定前缀开头，且前缀后是参数边界。
fn command_matches_prefix(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn has_dangerous_argument(command: &str) -> bool {
    DANGEROUS_ARGS.iter().any(|(prefix, args)| {
        command_matches_prefix(command, prefix)
            && command
                .split_whitespace()
                .map(normalize_token)
                .any(|token| args.iter().any(|arg| token.starts_with(*arg)))
    })
}

/// 拒绝变量/家目录展开、通配符与 `..` 路径穿越，避免白名单前缀被用来读目录外的文件。
fn has_unsafe_path_syntax(command: &str) -> bool {
    if command.contains(['$', '~']) {
        return true;
    }
    command
        .split_whitespace()
        .map(normalize_token)
        .any(|token| {
            // 通配符会让只读命令展开到白名单之外的路径。
            if token.contains(['*', '[']) {
                return true;
            }
            if token.contains('?') && token.contains('/') {
                return true;
            }
            token
                .split(|c| c == '/' || c == '\\')
                .any(|part| part == "..")
        })
}

/// 高敏感路径：一旦出现在参数里就拒绝，避免只读命令顺带泄露密钥与影子口令。
///
/// 说明：无路径声明的条目（如 `ls`、`stat`）允许多数绝对路径，否则 `df /var`
/// 这类日常诊断会被误伤；这里只拦真正敏感的目标。
const SENSITIVE_PATHS: &[&str] = &[
    "/root",
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    "/etc/ssh",
    "/var/lib/shadow",
    "/proc/sysrq-trigger",
];

fn has_sensitive_path(command: &str) -> bool {
    command
        .split_whitespace()
        .map(normalize_token)
        .any(|token| {
            let lower = token.to_ascii_lowercase();
            // 任意位置出现的私钥名与 .ssh 目录。
            if lower.contains("id_rsa")
                || lower.contains("id_ed25519")
                || lower.contains("id_ecdsa")
                || lower.contains("/.ssh")
                || lower.ends_with(".ssh")
            {
                return true;
            }
            // /proc/<pid>/environ 与 mem 会直接暴露进程环境变量和内存。
            if lower.starts_with("/proc/")
                && (lower.ends_with("/environ") || lower.ends_with("/mem"))
            {
                return true;
            }
            SENSITIVE_PATHS.iter().any(|path| {
                lower == *path
                    || lower
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
        })
}

/// 条目是否限定了路径（具体文件，或以 `/` 结尾的目录前缀）。
fn entry_has_path(entry: &str) -> bool {
    entry.ends_with('/') || entry.split_whitespace().any(|part| part.starts_with('/'))
}

/// 命令中的绝对路径必须落在白名单已声明的文件或目录内。
fn absolute_paths_allowed(command: &str, allowlist: &[String]) -> bool {
    command
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| token.starts_with('/'))
        .all(|token| {
            allowlist.iter().any(|entry| {
                let entry = entry.trim();
                let Some(entry_path) = entry
                    .split_whitespace()
                    .map(normalize_token)
                    .find(|part| part.starts_with('/'))
                else {
                    return false;
                };
                if entry.ends_with('/') {
                    token.starts_with(entry_path)
                } else {
                    token == entry_path
                }
            })
        })
}

/// root 登录会话下只放行内置只读条目：自定义前缀无法判断是否仍然只读。
fn diag_allowlist_for_login(allowlist: &[String], login_user: &str) -> Vec<String> {
    if login_user != "root" {
        return allowlist.to_vec();
    }
    builtin_only_allowlist(allowlist)
}

/// 只保留内置只读条目：用户自定义条目不参与任何免审批放行。
fn builtin_only_allowlist(allowlist: &[String]) -> Vec<String> {
    let builtin = crate::state::default_diag_allowlist();
    allowlist
        .iter()
        .filter(|entry| builtin.iter().any(|item| item == entry.trim()))
        .cloned()
        .collect()
}

/// 任务级自动放行只认内置只读条目，并沿用 root 登录收敛规则。
fn task_allowlist_for_login(allowlist: &[String], login_user: &str) -> Vec<String> {
    builtin_only_allowlist(&diag_allowlist_for_login(allowlist, login_user))
}

/// 判断命令是否完全落在只读白名单内。
///
/// 匹配规则：
/// - 普通条目按「完整命令或参数边界」匹配，避免 `ss` 误放行 `sss`；
/// - 以 `/` 结尾的条目按目录前缀匹配，例如 `cat /proc/` 可放行 `cat /proc/net/dev`；
/// - 拒绝 shell 元字符、`..` 路径穿越、`$`/`~` 展开和命令专属的危险参数。
fn readonly_command_allowed(command: &str, allowlist: &[String]) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains(['\n', '\r']) {
        return false;
    }
    if SHELL_METACHARS
        .iter()
        .any(|metachar| command.contains(metachar))
    {
        return false;
    }
    if has_dangerous_argument(command)
        || has_unsafe_path_syntax(command)
        || has_sensitive_path(command)
    {
        return false;
    }
    allowlist.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        let matched = command == entry
            || command.strip_prefix(entry).is_some_and(|rest| {
                if entry.ends_with('/') {
                    !rest.is_empty()
                } else {
                    rest.starts_with(char::is_whitespace)
                }
            });
        matched
            && (absolute_paths_allowed(command, allowlist)
                || (!entry_has_path(entry) && path_tolerant_command(command)))
    })
}

/// 只输出元数据、不读取文件内容的命令，允许带白名单未声明的绝对路径。
///
/// 这些命令仍受敏感路径与通配符检查约束，不能用来读取密钥或影子口令。
fn path_tolerant_command(command: &str) -> bool {
    const PATH_TOLERANT: &[&str] = &[
        "ls", "stat", "du", "df", "wc", "readlink", "lsof", "findmnt", "blkid",
    ];
    command
        .split_whitespace()
        .next()
        .map(normalize_token)
        .is_some_and(|head| PATH_TOLERANT.contains(&head))
}

/// diag 只读执行：白名单 + 阻断规则，既不申请审批也不允许 root。
async fn session_exec_diag(
    args: &Value,
    state: &AppState,
    client: &ClientContext,
    handle: Arc<SessionHandle>,
    sid: &str,
    supplied_command: &str,
    timeout_ms: u64,
) -> Value {
    let settings = state.inner.read().settings.clone();
    if !settings.ai_diag_enabled {
        return json!({"ok": false, "error": "DIAG_DISABLED", "detail": "diag 只读模式已在设置中关闭"});
    }
    if args
        .get("confirmation_id")
        .and_then(Value::as_str)
        .is_some()
    {
        return json!({"ok": false, "error": "DIAG_NO_CONFIRM", "detail": "diag 模式不接受审批单，只放行白名单只读命令"});
    }
    let command = supplied_command.trim();
    if command.is_empty() {
        return json!({"error": "missing command"});
    }
    let login_user = handle.config_snapshot().username;
    let root_login = login_user == "root";
    let allowlist = diag_allowlist_for_login(&settings.ai_diag_allowlist, &login_user);
    if !readonly_command_allowed(command, &allowlist) {
        let detail = if root_login {
            "当前会话以 root 登录，只读诊断仅放行内置只读条目；自定义条目请改用可写接入并逐条审批。"
        } else {
            "diag 模式只允许白名单内的只读命令，且不允许管道、重定向、命令串联、路径穿越或跟随输出。"
        };
        state.log_ai_event_detailed(
            sid,
            AIEventKind::Blocked {
                command: state.audit_command(command),
                rule: "diag allowlist".into(),
            },
            None,
            None,
            &client.client_name,
            String::new(),
            Some("diag allowlist".into()),
            Some(if root_login {
                "root 会话仅放行内置只读条目".into()
            } else {
                "diag 只读白名单拒绝".into()
            }),
            None,
        );
        return json!({
            "ok": false,
            "error": "DIAG_COMMAND_DENIED",
            "detail": detail,
            "allowlist": allowlist,
            "root_login": root_login,
        });
    }
    if let crate::security::CheckResult::Block { rule } = handle
        .security_engine
        .check(command, settings.default_policy)
    {
        state.log_ai_event_detailed(
            sid,
            AIEventKind::Blocked {
                command: state.audit_command(command),
                rule: rule.clone(),
            },
            None,
            None,
            &client.client_name,
            String::new(),
            Some(rule.clone()),
            Some("命中阻断规则".into()),
            None,
        );
        return json!({"ok": false, "error": "BLOCKED", "rule": rule});
    }
    execute_allowed(
        handle,
        state,
        client,
        sid,
        command.to_string(),
        timeout_ms,
        "diag",
    )
    .await
}

async fn execute_allowed(
    handle: Arc<SessionHandle>,
    state: &AppState,
    client: &ClientContext,
    sid: &str,
    command: String,
    timeout_ms: u64,
    audit_args: &str,
) -> Value {
    let started = std::time::Instant::now();
    let started_at = chrono::Utc::now();
    let command_for_task = command.clone();
    let handle_for_task = handle.clone();
    let result = tokio::task::spawn_blocking(move || {
        handle_for_task.exec_with_timeout(&command_for_task, Duration::from_millis(timeout_ms))
    })
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(Ok((code, stdout, stderr))) => {
            let timed_out = stderr.contains("[timeout]") || stderr.contains("命令执行超时");
            state.log_ai_event_detailed(
                sid,
                AIEventKind::Exec {
                    command: state.audit_command(&command),
                    args: audit_args.to_string(),
                },
                Some(code),
                Some(duration_ms),
                &client.client_name,
                String::new(),
                None,
                timed_out.then(|| "命令执行超时，exec channel 已关闭".into()),
                Some(started_at),
            );
            if timed_out {
                json!({
                    "ok": false,
                    "error": "TIMEOUT",
                    "timed_out": true,
                    "exit_code": code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "duration_ms": duration_ms,
                })
            } else {
                json!({
                    "ok": true,
                    "exit_code": code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "duration_ms": duration_ms,
                })
            }
        }
        Ok(Err(error)) => {
            state.log_ai_event_detailed(
                sid,
                AIEventKind::Exec {
                    command: state.audit_command(&command),
                    args: audit_args.to_string(),
                },
                None,
                Some(duration_ms),
                &client.client_name,
                String::new(),
                None,
                Some(error.clone()),
                Some(started_at),
            );
            json!({"ok": false, "error": error, "duration_ms": duration_ms})
        }
        Err(error) => json!({"ok": false, "error": format!("命令执行线程异常: {error}")}),
    }
}

fn session_read(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let token = match string_arg(&args, "attach_token") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let after_seq = match args.get("after_seq") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(value) => Some(value),
            None => return json!({"error": "after_seq must be a non-negative integer"}),
        },
    };
    let handle = match get_handle(state, sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = handle.validate_attach_token(token, &client.client_id, "read") {
        if error == "会话未连接" {
            return json!({
                "ok": false,
                "error": "CONNECTION_CLOSED",
                "detail": "会话已断开，绑定的终端连接已失效，请重新连接并 attach。"
            });
        }
        return json!({"ok": false, "error": error});
    }
    let binding = match handle.attach_binding(token, &client.client_id) {
        Ok(value) => value,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    // 绑定的 generation 与当前连接不一致说明已重连，禁止沿用旧 after_seq。
    if binding.generation != handle.generation_now() {
        return json!({
            "ok": false,
            "error": "CONNECTION_CHANGED",
            "connection_id": binding.connection_id,
            "detail": "会话已重新连接，旧 token 不能读取新连接的数据，请重新 attach。"
        });
    }
    if binding.require_active {
        let active = handle.active_connection_id();
        if active.as_deref() != Some(binding.connection_id.as_str()) {
            return json!({
                "ok": false,
                "error": "ACTIVE_CONNECTION_CHANGED",
                "connection_id": binding.connection_id,
                "active_connection_id": active,
                "detail": "用户已切换到其他终端。请重新 attach 到当前活跃连接，不要沿用旧的 after_seq。"
            });
        }
    }
    match handle.read_connection(&binding.connection_id, after_seq) {
        Ok((descriptor, replay)) => {
            let mut payload = json!(replay);
            payload["ok"] = json!(true);
            payload["session_id"] = json!(sid);
            payload["connection_id"] = json!(descriptor.connection_id);
            payload["kind"] = json!(descriptor.kind);
            payload["channel_id"] = json!(descriptor.channel_id);
            payload["generation"] = json!(descriptor.generation);
            payload["connected_at"] = json!(descriptor.connected_at);
            payload["login_user"] = json!(descriptor.login_user);
            payload["prompt_user"] = json!(descriptor.prompt_user);
            payload["prompt_user_source"] = json!(descriptor.prompt_user_source);
            payload["prompt_observed_at"] = json!(descriptor.prompt_observed_at);
            payload["is_active"] = json!(descriptor.is_active);
            payload["active_revision"] = json!(handle.active_revision());
            payload
        }
        Err(code) => json!({
            "ok": false,
            "error": code,
            "connection_id": binding.connection_id,
            "detail": "绑定的终端连接已关闭或已重连，请重新 attach。"
        }),
    }
}

fn session_detach(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let token = match string_arg(&args, "attach_token") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("ai finished");
    let handle = match get_handle(state, sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let role = match handle.attach_role_for_token(token, &client.client_id) {
        Some(role) => role,
        None => return json!({"ok": false, "error": "token 无效或已过期"}),
    };
    match handle.end_attach_with_client(state, token, &client.client_id, reason) {
        Ok(()) => {
            let (mode, note) = match role {
                AttachRole::Writer => ("write", "写入接入已释放"),
                AttachRole::Diag => ("diag", "只读诊断观察者已释放"),
                AttachRole::Read => ("read", "只读观察者已释放"),
            };
            json!({"ok": true, "mode": mode, "note": note})
        }
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn session_monitor(args: Value, state: &AppState) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value,
        Err(error) => return error,
    };
    match crate::monitor::get_snapshot(state, sid) {
        Some(snapshot) => json!(snapshot),
        None => json!({"error": "no snapshot available (session may be disconnected)"}),
    }
}

fn forward_list(args: Value, state: &AppState) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let (forwards, running) = {
        let inner = state.inner.read();
        let forwards = inner
            .sessions
            .get(sid)
            .map(|entry| entry.value().config_snapshot().port_forwards)
            .unwrap_or_default();
        let running = inner
            .forwards
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        (forwards, running)
    };
    let list = forwards
        .into_iter()
        .map(|forward| {
            json!({
                "id": forward.id,
                "kind": match forward.kind {
                    crate::state::ForwardKind::Local => "local",
                    crate::state::ForwardKind::Remote => "remote",
                },
                "listen": format!("{}:{}", forward.listen_host, forward.listen_port),
                "dest": format!("{}:{}", forward.dest_host, forward.dest_port),
                "running": running.contains(&forward.id),
            })
        })
        .collect::<Vec<_>>();
    json!({"forwards": list})
}

fn forward_add(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let sid = match string_arg(&args, "session_id") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let token = match string_arg(&args, "attach_token") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let listen_port = match port_arg(&args, "listen_port", None) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let dest_port = match port_arg(&args, "dest_port", None) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let kind = match args.get("kind").and_then(Value::as_str).unwrap_or("local") {
        "remote" => crate::state::ForwardKind::Remote,
        _ => crate::state::ForwardKind::Local,
    };
    let listen_host = args
        .get("listen_host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1")
        .to_string();
    let dest_host = args
        .get("dest_host")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1")
        .to_string();
    let handle = match get_handle(state, &sid) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match handle.validate_attach_role(token, &client.client_id) {
        Ok(AttachRole::Writer) => {}
        Ok(_) => {
            return json!({
                "ok": false,
                "error": "FORBIDDEN",
                "detail": "端口转发需要 writer 权限的 attach_token"
            })
        }
        Err(error) => return json!({"ok": false, "error": error}),
    }
    let spec = PortForwardConfig {
        id: format!("fw-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        kind,
        listen_host: listen_host.clone(),
        listen_port,
        dest_host: dest_host.clone(),
        dest_port,
        enabled: true,
    };
    // 非回环监听会把服务暴露到局域网，必须人工确认。
    if !is_loopback_host(&listen_host) {
        let preview = format!("端口转发 {listen_host}:{listen_port} → {dest_host}:{dest_port}");
        if let Some(response) = require_approval(
            &args,
            state,
            client,
            &sid,
            "AI 请求把端口转发监听在非回环地址",
            &preview,
        ) {
            return response;
        }
    }
    {
        let inner = state.inner.read();
        let Some(handle) = inner.sessions.get(&sid) else {
            return json!({"error": "session not found"});
        };
        handle.config.write().port_forwards.push(spec.clone());
    }
    if let Err(error) = crate::commands::persist(state) {
        return json!({"error": format!("端口转发保存失败: {error}")});
    }
    match crate::forward::start(state, &sid, spec.clone()) {
        Ok(()) => json!({
            "ok": true,
            "forward_id": spec.id,
            "listen": format!("{}:{}", spec.listen_host, spec.listen_port),
            "dest": format!("{}:{}", spec.dest_host, spec.dest_port),
        }),
        Err(error) => json!({"ok": false, "forward_id": spec.id, "error": error}),
    }
}

/// 只有回环地址不需要审批；其余监听地址会暴露到局域网甚至公网。
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "[::1]" | "localhost")
}

fn forward_stop(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let fid = match string_arg(&args, "forward_id") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let token = match string_arg(&args, "attach_token") {
        Ok(value) => value,
        Err(error) => return error,
    };
    // 先找到该转发属于哪个会话，再用该会话的 writer 权限确认身份。
    let owner = {
        let inner = state.inner.read();
        let found = inner
            .sessions
            .iter()
            .find(|entry| {
                entry
                    .value()
                    .config_snapshot()
                    .port_forwards
                    .iter()
                    .any(|forward| forward.id == fid)
            })
            .map(|entry| entry.key().clone());
        found
    };
    let Some(owner) = owner else {
        return json!({"ok": false, "error": "forward not found", "forward_id": fid});
    };
    let handle = match get_handle(state, &owner) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match handle.validate_attach_role(token, &client.client_id) {
        Ok(AttachRole::Writer) => {}
        Ok(_) => {
            return json!({
                "ok": false,
                "error": "FORBIDDEN",
                "detail": "停止端口转发需要 writer 权限的 attach_token"
            })
        }
        Err(error) => return json!({"ok": false, "error": error}),
    }
    crate::forward::stop(state, &fid);
    json!({"ok": true, "forward_id": fid, "running": false})
}

fn sftp_writer(
    args: &Value,
    state: &AppState,
    client: &ClientContext,
) -> Result<(String, Arc<SessionHandle>), Value> {
    let sid = string_arg(args, "session_id")?.to_string();
    let token = string_arg(args, "attach_token")?;
    let handle = get_handle(state, &sid)?;
    handle
        .validate_attach_token(token, &client.client_id, "sftp")
        .map_err(|error| json!({"ok": false, "error": error}))?;
    Ok((sid, handle))
}

fn audit_sftp(
    state: &AppState,
    client: &ClientContext,
    sid: &str,
    operation: &str,
    path: &str,
    error: Option<String>,
) {
    state.log_ai_event_detailed(
        sid,
        AIEventKind::Exec {
            command: state.audit_command(&format!("[{operation}] {path}")),
            args: String::new(),
        },
        error.is_none().then_some(0),
        None,
        &client.client_name,
        String::new(),
        None,
        error,
        None,
    );
}

fn sftp_encoding(args: &Value) -> Result<&str, Value> {
    match args
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or("text")
    {
        "text" => Ok("text"),
        "base64" => Ok("base64"),
        _ => Err(json!({"error": "encoding must be text or base64"})),
    }
}

async fn sftp_list(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let (sid, handle) = match sftp_writer(&args, state, client) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(".")
        .to_string();
    let limit = match args.get("limit") {
        None => DEFAULT_SFTP_LIST_LIMIT,
        Some(value) => match value.as_u64().and_then(|value| usize::try_from(value).ok()) {
            Some(value) if (1..=MAX_SFTP_LIST_LIMIT).contains(&value) => value,
            _ => return json!({"error": "limit must be between 1 and 500"}),
        },
    };
    let result = async {
        let connection = crate::sftp::connect(handle.config_snapshot()).await?;
        let mut entries = crate::sftp::list(&connection.session, &path).await?;
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        Ok::<_, String>((entries, truncated))
    }
    .await;
    match result {
        Ok((entries, truncated)) => {
            audit_sftp(state, client, &sid, "sftp_list", &path, None);
            json!({"ok": true, "path": path, "entries": entries, "truncated": truncated, "limit": limit})
        }
        Err(error) => {
            audit_sftp(state, client, &sid, "sftp_list", &path, Some(error.clone()));
            json!({"ok": false, "error": error})
        }
    }
}

async fn read_remote_limited(sftp: &SftpSession, path: &str) -> Result<Vec<u8>, String> {
    let metadata = sftp
        .metadata(path)
        .await
        .map_err(|error| format!("读取远端文件属性失败 ({path}): {error}"))?;
    if metadata.is_dir() {
        return Err("远端路径是目录，不是文件".into());
    }
    if metadata
        .size
        .is_some_and(|size| size > MAX_SFTP_BYTES as u64)
    {
        return Err("远端文件超过 1 MiB 限制".into());
    }
    let mut file = sftp
        .open(path)
        .await
        .map_err(|error| format!("打开远端文件失败 ({path}): {error}"))?;
    let mut bytes = Vec::new();
    let read = (&mut file)
        .take(MAX_SFTP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await;
    let close = file.close().await;
    read.map_err(|error| format!("读取远端文件失败 ({path}): {error}"))?;
    close.map_err(|error| format!("关闭远端文件失败 ({path}): {error}"))?;
    if bytes.len() > MAX_SFTP_BYTES {
        return Err("远端文件超过 1 MiB 限制".into());
    }
    Ok(bytes)
}

fn encode_sftp_read(bytes: Vec<u8>, encoding: &str) -> Result<String, String> {
    if bytes.len() > MAX_SFTP_BYTES {
        return Err("远端文件超过 1 MiB 限制".into());
    }
    match encoding {
        "text" => String::from_utf8(bytes)
            .map_err(|_| "远端文件不是合法 UTF-8，请使用 encoding=base64".into()),
        "base64" => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        _ => Err("encoding must be text or base64".into()),
    }
}

async fn sftp_read(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let (sid, handle) = match sftp_writer(&args, state, client) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let path = match string_arg(&args, "path") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let encoding = match sftp_encoding(&args) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let result = async {
        let connection = crate::sftp::connect(handle.config_snapshot()).await?;
        let bytes = read_remote_limited(&connection.session, &path).await?;
        let size = bytes.len();
        let content = encode_sftp_read(bytes, encoding)?;
        Ok::<_, String>((content, size))
    }
    .await;
    match result {
        Ok((content, size)) => {
            audit_sftp(state, client, &sid, "sftp_read", &path, None);
            json!({"ok": true, "path": path, "encoding": encoding, "size": size, "content": content})
        }
        Err(error) => {
            audit_sftp(state, client, &sid, "sftp_read", &path, Some(error.clone()));
            json!({"ok": false, "error": error})
        }
    }
}

fn decode_sftp_write(content: &str, encoding: &str) -> Result<Vec<u8>, String> {
    let bytes = match encoding {
        "text" => content.as_bytes().to_vec(),
        "base64" => {
            let max_encoded_len = 4 * (MAX_SFTP_BYTES + 2) / 3 + 4;
            if content.len() > max_encoded_len {
                return Err("写入内容超过 1 MiB 限制".into());
            }
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|error| format!("base64 内容无效: {error}"))?
        }
        _ => return Err("encoding must be text or base64".into()),
    };
    if bytes.len() > MAX_SFTP_BYTES {
        return Err("写入内容超过 1 MiB 限制".into());
    }
    Ok(bytes)
}

fn sftp_write_descriptor(path: &str) -> String {
    format!("sftp_write path={path}")
}

async fn remote_target(sftp: &SftpSession, path: &str) -> Result<(bool, bool), String> {
    match sftp.symlink_metadata(path).await {
        Ok(metadata) => Ok((true, metadata.is_dir())),
        Err(error) if crate::sftp::is_not_found(&error) => Ok((false, false)),
        Err(error) => Err(format!("检查远端目标失败 ({path}): {error}")),
    }
}

fn check_sftp_target(exists: bool, is_dir: bool, overwrite_approved: bool) -> Result<(), String> {
    if is_dir {
        return Err("远端目标是目录，不能按文件写入".into());
    }
    if exists && !overwrite_approved {
        return Err("远端目标已存在，必须经人工批准后才能覆盖".into());
    }
    Ok(())
}

async fn write_remote_atomic(
    sftp: &SftpSession,
    path: &str,
    bytes: &[u8],
    overwrite_approved: bool,
) -> Result<(), String> {
    let (exists, is_dir) = remote_target(sftp, path).await?;
    check_sftp_target(exists, is_dir, overwrite_approved)?;
    let part = format!("{path}.part.{}", uuid::Uuid::new_v4().simple());
    let result = async {
        let mut file = sftp
            .open_with_flags(
                &part,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            )
            .await
            .map_err(|error| format!("创建远端临时文件失败 ({part}): {error}"))?;
        file.write_all(bytes)
            .await
            .map_err(|error| format!("写入远端临时文件失败 ({part}): {error}"))?;
        file.flush()
            .await
            .map_err(|error| format!("确认远端临时文件失败 ({part}): {error}"))?;
        file.close()
            .await
            .map_err(|error| format!("关闭远端临时文件失败 ({part}): {error}"))?;
        let (exists, is_dir) = remote_target(sftp, path).await?;
        check_sftp_target(exists, is_dir, overwrite_approved)?;
        let renamed = if exists {
            sftp.posix_rename(&part, path).await
        } else {
            sftp.rename(&part, path).await
        };
        renamed.map_err(|error| format!("重命名远端临时文件失败 ({part} → {path}): {error}"))?;
        Ok::<_, String>(())
    }
    .await;
    if result.is_err() {
        let _ = crate::sftp::remove_file_if_exists(sftp, &part).await;
    }
    result
}

async fn sftp_write(args: Value, state: &AppState, client: &ClientContext) -> Value {
    let (sid, handle) = match sftp_writer(&args, state, client) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let path = match string_arg(&args, "path") {
        Ok(value) => value.to_string(),
        Err(error) => return error,
    };
    let encoding = match sftp_encoding(&args) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let Some(content) = args.get("content").and_then(Value::as_str) else {
        return json!({"error": "missing content"});
    };
    let bytes = match decode_sftp_write(content, encoding) {
        Ok(value) => value,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let descriptor = sftp_write_descriptor(&path);
    let connection = match crate::sftp::connect(handle.config_snapshot()).await {
        Ok(value) => value,
        Err(error) => {
            audit_sftp(
                state,
                client,
                &sid,
                "sftp_write",
                &path,
                Some(error.clone()),
            );
            return json!({"ok": false, "error": error});
        }
    };
    let (target_exists, target_is_dir) = match remote_target(&connection.session, &path).await {
        Ok(value) => value,
        Err(error) => {
            audit_sftp(
                state,
                client,
                &sid,
                "sftp_write",
                &path,
                Some(error.clone()),
            );
            return json!({"ok": false, "error": error});
        }
    };
    if target_is_dir {
        let error = "远端目标是目录，不能按文件写入".to_string();
        audit_sftp(
            state,
            client,
            &sid,
            "sftp_write",
            &path,
            Some(error.clone()),
        );
        return json!({"ok": false, "error": error});
    }

    let mut approved_confirmation = None;
    if let Some(confirmation_id) = args.get("confirmation_id").and_then(Value::as_str) {
        let Some(existing) = approval::get(state, confirmation_id) else {
            return json!({"ok": false, "error": "confirmation not found", "confirmation_id": confirmation_id});
        };
        if existing.command.as_deref() != Some(descriptor.as_str()) {
            return json!({"ok": false, "error": "remote path does not match the approved overwrite", "confirmation_id": confirmation_id});
        }
        match approval::consume_approved(
            state,
            confirmation_id,
            ConfirmationKind::Exec,
            &sid,
            &client.client_id,
        ) {
            Ok(confirmation) => approved_confirmation = Some(confirmation),
            Err(error) if error == "WAIT_CONFIRM" => {
                return pending_response(state, confirmation_id)
            }
            Err(error) => {
                return json!({"ok": false, "error": error, "confirmation_id": confirmation_id})
            }
        }
    }
    if target_exists && approved_confirmation.is_none() {
        let rule = "sftp overwrite requires human approval".to_string();
        let confirmation = match approval::request(
            state,
            ConfirmationKind::Exec,
            &sid,
            &client.client_id,
            &client.client_name,
            "sftp write would overwrite an existing remote file",
            Some(descriptor),
            Some(rule.clone()),
            None,
        ) {
            Ok(value) => value,
            Err(error) => return json!({"ok": false, "error": error}),
        };
        state.log_ai_event_detailed(
            &sid,
            AIEventKind::WaitingConfirm {
                command: state.audit_command(&format!("[sftp_write] {path}")),
                rule: rule.clone(),
            },
            None,
            None,
            &client.client_name,
            confirmation.id.clone(),
            Some(rule),
            None,
            None,
        );
        return pending_response(state, &confirmation.id);
    }
    let overwrite_approved = approved_confirmation.is_some();
    if let Some(confirmation) = approved_confirmation {
        state.log_ai_event_detailed(
            &sid,
            AIEventKind::Confirmed {
                command: state.audit_command(&format!("[sftp_write] {path}")),
            },
            None,
            None,
            &client.client_name,
            confirmation.id,
            confirmation.rule,
            None,
            None,
        );
    }
    let result = write_remote_atomic(&connection.session, &path, &bytes, overwrite_approved).await;
    match result {
        Ok(()) => {
            audit_sftp(state, client, &sid, "sftp_write", &path, None);
            json!({"ok": true, "path": path, "bytes_written": bytes.len(), "overwritten": target_exists})
        }
        Err(error) => {
            audit_sftp(
                state,
                client,
                &sid,
                "sftp_write",
                &path,
                Some(error.clone()),
            );
            json!({"ok": false, "error": error})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AIPolicyOverride;

    fn client(id: &str) -> ClientContext {
        ClientContext {
            client_id: id.into(),
            client_name: "test-client".into(),
            protocol_version: crate::mcp_protocol::LATEST_PROTOCOL_VERSION.into(),
        }
    }

    fn connected_session(state: &AppState, id: &str) -> Arc<SessionHandle> {
        let handle = Arc::new(SessionHandle::new(
            SessionConfig {
                id: id.into(),
                name: id.into(),
                host: "127.0.0.1".into(),
                port: 22,
                username: "root".into(),
                auth: AuthConfig::Agent,
                proxy_jump: Vec::new(),
                group: String::new(),
                ai_allowed: true,
                ai_policy: AIPolicyOverride::Inherit,
                ai_blocklist_regexes: Vec::new(),
                auto_connect: false,
                keepalive_secs: 30,
                connect_timeout_secs: 30,
                port_forwards: Vec::new(),
                log_enabled: false,
            },
            Vec::new(),
            Vec::new(),
        ));
        handle
            .connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state
            .inner
            .write()
            .sessions
            .insert(id.into(), handle.clone());
        handle
    }

    #[test]
    fn attach_defaults_to_legacy_write_and_read_cannot_request_sftp() {
        let (mode, sftp, _, _) = attach_request(&json!({})).unwrap();
        assert_eq!(mode, AttachMode::Write);
        assert!(!sftp);
        let (mode, sftp, _, _) = attach_request(&json!({"permissions": ["sftp"]})).unwrap();
        assert_eq!(mode, AttachMode::Write);
        assert!(sftp);
        assert!(attach_request(&json!({"mode": "read", "permissions": ["sftp"]})).is_err());
        assert!(attach_request(&json!({"mode": "admin"})).is_err());
    }

    #[tokio::test]
    async fn read_token_can_read_replay_but_cannot_exec_or_use_sftp() {
        let state = AppState::new();
        let handle = connected_session(&state, "s1");
        let token = handle
            .begin_read_attach_with_client(&state, "observe", "client-a", 30)
            .unwrap();
        let client = client("client-a");
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert_eq!(read["ok"], true);
        assert_eq!(read["chunks"], json!([]));

        let exec = session_exec(
            json!({"session_id": "s1", "attach_token": token, "command": "pwd"}),
            &state,
            &client,
        )
        .await;
        assert_eq!(exec["error"], "READ_ONLY_TOKEN");
        let sftp = sftp_writer(
            &json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        )
        .err()
        .unwrap();
        assert!(sftp["error"].as_str().unwrap().contains("sftp"));
    }

    #[test]
    fn writer_requires_explicit_sftp_permission() {
        let state = AppState::new();
        let handle = connected_session(&state, "s1");
        let token = handle
            .begin_write_attach_with_client(&state, "write", "client-a", 30, false, false)
            .unwrap();
        assert!(sftp_writer(
            &json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client("client-a"),
        )
        .is_err());
        handle.force_detach(&state);
        let token = handle
            .begin_write_attach_with_client(&state, "write", "client-a", 30, true, false)
            .unwrap();
        assert!(sftp_writer(
            &json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client("client-a"),
        )
        .is_ok());
    }

    #[test]
    fn sftp_payload_limits_are_one_mib_and_text_is_strict_utf8() {
        assert_eq!(
            decode_sftp_write(&"x".repeat(MAX_SFTP_BYTES), "text")
                .unwrap()
                .len(),
            MAX_SFTP_BYTES
        );
        assert!(decode_sftp_write(&"x".repeat(MAX_SFTP_BYTES + 1), "text").is_err());
        assert!(encode_sftp_read(vec![0; MAX_SFTP_BYTES + 1], "base64").is_err());
        assert!(encode_sftp_read(vec![0xff], "text").is_err());
        assert_eq!(encode_sftp_read(vec![0xff], "base64").unwrap(), "/w==");

        let bytes = vec![0x5a; MAX_SFTP_BYTES];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(decode_sftp_write(&encoded, "base64").unwrap(), bytes);
        assert!(decode_sftp_write("not base64!", "base64").is_err());
    }

    #[test]
    fn sftp_conflicts_require_approval_and_never_allow_directories() {
        assert!(check_sftp_target(false, false, false).is_ok());
        assert!(check_sftp_target(true, false, false).is_err());
        assert!(check_sftp_target(true, false, true).is_ok());
        assert!(check_sftp_target(true, true, true).is_err());
    }

    #[test]
    fn sftp_overwrite_approval_is_path_bound_and_single_use() {
        let state = AppState::new();
        let descriptor = sftp_write_descriptor("/tmp/a");
        assert_ne!(descriptor, sftp_write_descriptor("/tmp/b"));
        let confirmation = approval::request(
            &state,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
            "test-client",
            "overwrite",
            Some(descriptor),
            Some("sftp overwrite requires human approval".into()),
            None,
        )
        .unwrap();
        approval::resolve(&state, &confirmation.id, true).unwrap();
        approval::consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
        )
        .unwrap();
        assert!(approval::consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
        )
        .is_err());
    }

    #[test]
    fn tools_expose_only_the_new_read_sftp_and_settings_get_operations() {
        let tools = tools_list();
        let tools = tools["tools"].as_array().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 18);
        for expected in [
            "session_read",
            "sftp_list",
            "sftp_read",
            "sftp_write",
            "settings_get",
        ] {
            assert!(names.contains(&expected));
        }
        assert!(!names.contains(&"settings_set"));
    }

    fn attach_args(session_id: &str, extra: Value) -> Value {
        let mut args = json!({"session_id": session_id, "reason": "test"});
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                args[key.as_str()] = value.clone();
            }
        }
        args
    }

    fn read_text(response: &Value) -> String {
        response["chunks"]
            .as_array()
            .map(|chunks| {
                chunks
                    .iter()
                    .filter_map(|chunk| chunk["data"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    /// 定向 attach 测试直接走批准路径，避免每次构造人工确认。
    fn attachable_state() -> AppState {
        let state = AppState::new();
        state.inner.write().settings.mcp_require_attach_approval = false;
        state
    }

    #[test]
    fn failed_attach_does_not_consume_approval() {
        let state = AppState::new();
        let handle = connected_session(&state, "s1");

        // 客户端 B 先发起审批，此时没有 writer 冲突。
        let client_b = client("client-b");
        let pending = session_attach(attach_args("s1", json!({})), &state, &client_b);
        assert_eq!(pending["error"], "WAIT_CONFIRM");
        let pending_id = pending["confirmation_id"].as_str().unwrap().to_string();

        // 客户端 A 在此期间完成接入并占用 writer。
        let client_a = client("client-a");
        let first = session_attach(attach_args("s1", json!({})), &state, &client_a);
        assert_eq!(first["error"], "WAIT_CONFIRM");
        let first_id = first["confirmation_id"].as_str().unwrap().to_string();
        approval::resolve(&state, &first_id, true).unwrap();
        let attached = session_attach(
            attach_args("s1", json!({"confirmation_id": first_id})),
            &state,
            &client_a,
        );
        assert_eq!(attached["ok"], true);

        // B 已批准，但重试时因 writer 冲突失败：审批单必须保持可重试。
        approval::resolve(&state, &pending_id, true).unwrap();
        let blocked = session_attach(
            attach_args("s1", json!({"confirmation_id": pending_id})),
            &state,
            &client_b,
        );
        assert!(blocked["error"]
            .as_str()
            .unwrap()
            .contains("已有 AI writer"));
        assert_eq!(
            approval::get(&state, &pending_id).unwrap().status,
            ConfirmationStatus::Approved
        );

        // 释放 writer 后，同一个 confirmation_id 仍可成功使用。
        let token = attached["attach_token"].as_str().unwrap();
        handle
            .end_attach_with_client(&state, token, "client-a", "done")
            .unwrap();
        let retry = session_attach(
            attach_args("s1", json!({"confirmation_id": pending_id})),
            &state,
            &client_b,
        );
        assert_eq!(retry["ok"], true);
    }

    #[test]
    fn single_connection_attach_keeps_legacy_behaviour() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.push_test_primary_output("PRIMARY-MARK\r\n");
        let client = client("client-a");
        let attached = session_attach(attach_args("s1", json!({})), &state, &client);
        assert_eq!(attached["ok"], true);
        assert_eq!(attached["selection_reason"], "only_connection");
        assert_eq!(attached["kind"], "primary");
        assert_eq!(attached["is_active"], true);

        let token = attached["attach_token"].as_str().unwrap().to_string();
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert_eq!(read["ok"], true);
        assert_eq!(read["connection_id"], attached["connection_id"]);
        assert!(read_text(&read).contains("PRIMARY-MARK"));
    }

    #[test]
    fn multiple_connections_require_explicit_or_active_choice() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.push_test_primary_output("PRIMARY-MARK\r\n");
        handle.install_test_extra_channel("ch1", "EXTRA-MARK\r\n");
        let client = client("client-a");

        // 多连接且活跃状态未知：不允许静默回退主 Shell。
        let ambiguous = session_attach(attach_args("s1", json!({})), &state, &client);
        assert_eq!(ambiguous["error"], "AMBIGUOUS_CONNECTION");
        assert_eq!(ambiguous["candidates"].as_array().unwrap().len(), 2);

        // 活跃标签明确时默认绑定活跃连接。
        handle.set_active_terminal(Some("ch1"));
        let attached = session_attach(attach_args("s1", json!({})), &state, &client);
        assert_eq!(attached["selection_reason"], "active");
        assert_eq!(
            attached["connection_id"],
            format!("{}:ch1", handle.generation_now())
        );
        let token = attached["attach_token"].as_str().unwrap().to_string();
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert!(read_text(&read).contains("EXTRA-MARK"));
        assert!(!read_text(&read).contains("PRIMARY-MARK"));
        handle
            .end_attach_with_client(&state, &token, "client-a", "done")
            .unwrap();

        // 显式指定后台连接：读取正确且 is_active=false。
        let primary_id = format!("{}:primary", handle.generation_now());
        let attached = session_attach(
            attach_args("s1", json!({"connection_id": primary_id})),
            &state,
            &client,
        );
        assert_eq!(attached["selection_reason"], "explicit");
        assert_eq!(attached["is_active"], false);
        let token = attached["attach_token"].as_str().unwrap().to_string();
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert!(read_text(&read).contains("PRIMARY-MARK"));
        assert_eq!(read["is_active"], false);
    }

    #[test]
    fn unknown_connection_id_returns_candidates() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.install_test_extra_channel("ch1", "EXTRA-MARK\r\n");
        let client = client("client-a");
        let response = session_attach(
            attach_args("s1", json!({"connection_id": "999:ch9"})),
            &state,
            &client,
        );
        assert_eq!(response["error"], "CONNECTION_NOT_FOUND");
        assert_eq!(response["candidates"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn require_active_read_reports_tab_switch_without_rebinding() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.push_test_primary_output("PRIMARY-MARK\r\n");
        handle.install_test_extra_channel("ch1", "EXTRA-MARK\r\n");
        handle.set_active_terminal(Some("ch1"));
        let client = client("client-a");
        let attached = session_attach(
            attach_args("s1", json!({"require_active": true})),
            &state,
            &client,
        );
        assert_eq!(attached["ok"], true);
        let token = attached["attach_token"].as_str().unwrap().to_string();
        let bound = attached["connection_id"].as_str().unwrap().to_string();

        // 用户切回主 Shell：严格模式必须显式报错，且不返回另一个连接的回放。
        handle.set_active_terminal(None);
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert_eq!(read["error"], "ACTIVE_CONNECTION_CHANGED");
        assert_eq!(read["connection_id"], bound);
        assert_eq!(
            read["active_connection_id"],
            format!("{}:primary", handle.generation_now())
        );
        assert!(read.get("chunks").is_none());
    }

    #[test]
    fn stale_token_cannot_read_after_reconnect() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.push_test_primary_output("PRIMARY-MARK\r\n");
        let client = client("client-a");
        let attached = session_attach(attach_args("s1", json!({})), &state, &client);
        let token = attached["attach_token"].as_str().unwrap().to_string();

        handle.simulate_reconnect_for_test();
        handle.push_test_primary_output("NEW-CONNECTION-MARK\r\n");
        let read = session_read(
            json!({"session_id": "s1", "attach_token": token}),
            &state,
            &client,
        );
        assert_eq!(read["error"], "CONNECTION_CHANGED");
        assert!(!read_text(&read).contains("NEW-CONNECTION-MARK"));
    }

    #[test]
    fn session_list_exposes_connections_and_active_id() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        handle.install_test_extra_channel("ch1", "EXTRA-MARK\r\n");
        handle.set_active_terminal(Some("ch1"));
        let listed = session_list(&state);
        let session = &listed["sessions"][0];
        assert_eq!(session["connections"].as_array().unwrap().len(), 2);
        assert_eq!(
            session["active_connection_id"],
            format!("{}:ch1", handle.generation_now())
        );
        assert!(session["active_revision"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn diag_allowlist_matches_prefixes_and_rejects_shell_chaining() {
        let allowlist = vec!["ss".to_string(), "systemctl status".to_string()];
        assert!(readonly_command_allowed("ss -s", &allowlist));
        assert!(readonly_command_allowed(
            "systemctl status nginx",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "systemctl restart nginx",
            &allowlist
        ));
        assert!(!readonly_command_allowed("ss -s; whoami", &allowlist));
        assert!(!readonly_command_allowed("ss -s | grep 22", &allowlist));
        assert!(!readonly_command_allowed("ss -s > /tmp/out", &allowlist));
        assert!(!readonly_command_allowed("ss -s && id", &allowlist));
        assert!(!readonly_command_allowed("ss -s\nwhoami", &allowlist));
        assert!(!readonly_command_allowed("sss", &allowlist));
    }

    #[test]
    fn diag_allowlist_directory_prefix_matches_files() {
        let allowlist = vec!["cat /proc/".to_string(), "cat /etc/os-release".to_string()];
        assert!(readonly_command_allowed("cat /proc/net/dev", &allowlist));
        assert!(readonly_command_allowed("cat /proc/meminfo", &allowlist));
        assert!(readonly_command_allowed("cat /etc/os-release", &allowlist));
        assert!(!readonly_command_allowed("cat /proc", &allowlist));
        assert!(!readonly_command_allowed("cat /procfoo", &allowlist));
        assert!(!readonly_command_allowed(
            "cat /etc/os-release.bak",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "cat /proc/net/dev | head",
            &allowlist
        ));
        // 目录前缀不能靠 ../ 逃到白名单目录之外。
        assert!(!readonly_command_allowed(
            "cat /proc/../etc/shadow",
            &allowlist
        ));
        // 具体文件条目不能顺带读第二个未声明的文件。
        assert!(!readonly_command_allowed(
            "cat /etc/os-release /etc/shadow",
            &allowlist
        ));
        assert!(readonly_command_allowed(
            "cat /etc/os-release /proc/meminfo",
            &allowlist
        ));
    }

    #[test]
    fn diag_allowlist_rejects_dangerous_arguments() {
        let allowlist = vec![
            "tail /var/log/".to_string(),
            "journalctl".to_string(),
            "crontab -l".to_string(),
            "curl -I".to_string(),
            "ls".to_string(),
        ];
        assert!(readonly_command_allowed("tail /var/log/syslog", &allowlist));
        assert!(!readonly_command_allowed(
            "tail /var/log/syslog -f",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "tail /var/log/syslog \"-f\"",
            &allowlist
        ));
        assert!(readonly_command_allowed(
            "journalctl -u nginx -n 50",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "journalctl --vacuum-size=1M",
            &allowlist
        ));
        assert!(!readonly_command_allowed("crontab -l -r", &allowlist));
        assert!(readonly_command_allowed(
            "curl -I http://127.0.0.1/",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "curl -I -X DELETE http://127.0.0.1/api",
            &allowlist
        ));
    }

    #[test]
    fn diag_allowlist_blocks_parameter_based_file_access() {
        let allowlist = crate::state::default_diag_allowlist();
        // dig -f 会把任意文件逐行读入诊断输出。
        assert!(!readonly_command_allowed("dig -f /etc/shadow", &allowlist));
        assert!(!readonly_command_allowed(
            "dig --file /etc/passwd",
            &allowlist
        ));
        assert!(readonly_command_allowed("dig example.com", &allowlist));
        // curl 的 -D / -c 可写任意文件。
        assert!(!readonly_command_allowed(
            "curl -I -D /tmp/x http://127.0.0.1/",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "curl -I -c /tmp/x http://127.0.0.1/",
            &allowlist
        ));
        assert!(readonly_command_allowed(
            "curl -I http://127.0.0.1/",
            &allowlist
        ));
        // sar 写盘。
        assert!(!readonly_command_allowed("sar -o /tmp/x 1 1", &allowlist));
        // 未声明路径的读取类命令不能顺带读任意文件。
        assert!(!readonly_command_allowed("cat /etc/shadow", &allowlist));
        assert!(readonly_command_allowed(
            "cat /var/log/nginx/access.log",
            &allowlist
        ));
        // 元数据类命令允许常见路径，敏感路径仍拒绝。
        assert!(readonly_command_allowed("ls -la /tmp", &allowlist));
        assert!(!readonly_command_allowed("ls /root", &allowlist));
        assert!(!readonly_command_allowed("stat /etc/shadow", &allowlist));
        assert!(!readonly_command_allowed(
            "ls /home/deploy/.ssh",
            &allowlist
        ));
        // 通配符会让只读命令展开到白名单之外。
        assert!(!readonly_command_allowed("cat /var/log/*", &allowlist));
        assert!(!readonly_command_allowed("wc -l /etc/*", &allowlist));
    }

    #[test]
    fn diag_allowlist_rejects_variable_and_tilde_expansion() {
        let allowlist = vec!["ls".to_string(), "cat /proc/meminfo".to_string()];
        assert!(readonly_command_allowed("ls -la /tmp", &allowlist));
        assert!(!readonly_command_allowed(
            "cat /proc/meminfo $HOME/.ssh/id_rsa",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "cat /proc/meminfo ~/.ssh/id_rsa",
            &allowlist
        ));
        assert!(!readonly_command_allowed("ls ~", &allowlist));
    }

    #[test]
    fn diag_root_login_only_keeps_builtin_entries() {
        let custom = vec![
            "ls".to_string(),
            "ip".to_string(),
            "mount".to_string(),
            "cat /etc/os-release".to_string(),
        ];
        let restricted = diag_allowlist_for_login(&custom, "root");
        assert!(restricted.iter().any(|entry| entry == "ls"));
        assert!(restricted
            .iter()
            .any(|entry| entry == "cat /etc/os-release"));
        assert!(!restricted.iter().any(|entry| entry == "ip"));
        assert!(!restricted.iter().any(|entry| entry == "mount"));
        assert_eq!(diag_allowlist_for_login(&custom, "deploy"), custom);
    }

    #[test]
    fn task_allowlist_only_keeps_builtin_entries() {
        let custom = vec![
            "ls".to_string(),
            "git status".to_string(),
            "make build".to_string(),
        ];
        // 普通用户登录时，任务级自动放行也只认内置条目。
        let task = task_allowlist_for_login(&custom, "deploy");
        assert!(task.iter().any(|entry| entry == "ls"));
        assert!(!task.iter().any(|entry| entry == "git status"));
        assert!(!task.iter().any(|entry| entry == "make build"));
        // root 登录同样只保留内置条目，且与普通登录结果一致。
        assert_eq!(task_allowlist_for_login(&custom, "root"), task);
        assert!(!readonly_command_allowed("git status", &task));
        assert!(readonly_command_allowed("ls -la /tmp", &task));
    }

    #[test]
    fn default_diag_allowlist_is_read_only() {
        let allowlist = crate::state::default_diag_allowlist();
        assert!(readonly_command_allowed("ls -la /tmp", &allowlist));
        assert!(readonly_command_allowed("df -h", &allowlist));
        assert!(readonly_command_allowed(
            "systemctl status nginx",
            &allowlist
        ));
        assert!(readonly_command_allowed(
            "cat /var/log/nginx/access.log",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "ip link set eth0 down",
            &allowlist
        ));
        assert!(!readonly_command_allowed("ifconfig eth0 down", &allowlist));
        assert!(!readonly_command_allowed(
            "mount -o remount,rw /",
            &allowlist
        ));
        assert!(!readonly_command_allowed("hostname changed", &allowlist));
        assert!(!readonly_command_allowed("date -s 2020-01-01", &allowlist));
        assert!(!readonly_command_allowed(
            "systemctl stop nginx",
            &allowlist
        ));
        assert!(!readonly_command_allowed(
            "kubectl get secret -o yaml",
            &allowlist
        ));
        assert!(!readonly_command_allowed("ss -K dst 10.0.0.1", &allowlist));
        assert!(!readonly_command_allowed("dmesg -C", &allowlist));
    }

    #[tokio::test]
    async fn diag_observers_are_non_exclusive_and_read_tokens_cannot_exec() {
        let state = attachable_state();
        let _handle = connected_session(&state, "s1");
        let client_a = client("client-a");
        let client_b = client("client-b");

        let first = session_attach(
            attach_args("s1", json!({"mode": "diag", "reason": "disk check"})),
            &state,
            &client_a,
        );
        assert_eq!(first["ok"], true);
        assert_eq!(first["mode"], "diag");
        assert!(first["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "diag"));
        let second = session_attach(
            attach_args("s1", json!({"mode": "diag", "reason": "load check"})),
            &state,
            &client_b,
        );
        assert_eq!(second["ok"], true);

        let read = session_attach(
            attach_args("s1", json!({"mode": "read"})),
            &state,
            &client("client-c"),
        );
        assert_eq!(read["ok"], true);
        let read_token = read["attach_token"].as_str().unwrap();

        let denied = session_exec(
            json!({"session_id": "s1", "attach_token": read_token, "command": "ss -s"}),
            &state,
            &client("client-c"),
        )
        .await;
        assert_eq!(denied["error"], "READ_ONLY_TOKEN");
    }

    #[test]
    fn session_detach_reports_real_attach_mode() {
        let state = attachable_state();
        let _handle = connected_session(&state, "s1");

        let diag_client = client("client-diag");
        let diag = session_attach(
            attach_args("s1", json!({"mode": "diag", "reason": "disk check"})),
            &state,
            &diag_client,
        );
        assert_eq!(diag["ok"], true);
        let diag_detach = session_detach(
            json!({"session_id": "s1", "attach_token": diag["attach_token"], "reason": "done"}),
            &state,
            &diag_client,
        );
        assert_eq!(diag_detach["ok"], true);
        assert_eq!(diag_detach["mode"], "diag");
        assert_eq!(diag_detach["note"], "只读诊断观察者已释放");

        let read_client = client("client-read");
        let read = session_attach(
            attach_args("s1", json!({"mode": "read"})),
            &state,
            &read_client,
        );
        assert_eq!(read["ok"], true);
        let read_detach = session_detach(
            json!({"session_id": "s1", "attach_token": read["attach_token"], "reason": "done"}),
            &state,
            &read_client,
        );
        assert_eq!(read_detach["mode"], "read");
        assert_eq!(read_detach["note"], "只读观察者已释放");

        let write_client = client("client-write");
        let write = session_attach(attach_args("s1", json!({})), &state, &write_client);
        assert_eq!(write["ok"], true);
        let write_detach = session_detach(
            json!({"session_id": "s1", "attach_token": write["attach_token"], "reason": "done"}),
            &state,
            &write_client,
        );
        assert_eq!(write_detach["mode"], "write");
        assert_eq!(write_detach["note"], "写入接入已释放");
    }

    #[tokio::test]
    async fn diag_exec_rejects_writes_and_non_allowlisted_commands_without_approval() {
        let state = attachable_state();
        let _handle = connected_session(&state, "s1");
        let client = client("client-a");
        let attached = session_attach(attach_args("s1", json!({"mode": "diag"})), &state, &client);
        let token = attached["attach_token"].as_str().unwrap();

        let denied = session_exec(
            json!({"session_id": "s1", "attach_token": token, "command": "rm -rf /tmp/x"}),
            &state,
            &client,
        )
        .await;
        assert_eq!(denied["error"], "DIAG_COMMAND_DENIED");

        let chained = session_exec(
            json!({"session_id": "s1", "attach_token": token, "command": "ss -s | grep 22"}),
            &state,
            &client,
        )
        .await;
        assert_eq!(chained["error"], "DIAG_COMMAND_DENIED");

        let root = session_exec(
            json!({"session_id": "s1", "attach_token": token, "command": "ss -s", "run_as": "root"}),
            &state,
            &client,
        )
        .await;
        assert_eq!(root["error"], "DIAG_READ_ONLY_ONLY");
    }

    #[test]
    fn write_attach_with_task_exposes_task_and_auto_detaches_when_expired() {
        let state = attachable_state();
        let handle = connected_session(&state, "s1");
        let client = client("client-a");
        let attached = session_attach(
            attach_args(
                "s1",
                json!({"task": {"goal": "定位 nginx 502", "ttl_minutes": 30}}),
            ),
            &state,
            &client,
        );
        assert_eq!(attached["ok"], true);
        assert_eq!(attached["task"]["goal"], "定位 nginx 502");
        assert_eq!(handle.status().ai_task.as_deref(), Some("定位 nginx 502"));

        handle.expire_attach_task_for_test();
        handle.expire_attach_if_needed(&state);

        let status = handle.status();
        assert!(!status.ai_attached);
        assert_eq!(status.ai_task, None);
        let replay = handle.read_ai_terminal(None);
        let text = replay
            .chunks
            .iter()
            .map(|chunk| chunk.data.as_str())
            .collect::<String>();
        assert!(text.contains("任务「定位 nginx 502」已结束"));
    }
}
