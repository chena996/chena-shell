//! SSH 会话句柄 — 基于 russh（纯 Rust，支持 curve25519 / ed25519 等现代算法）
//!
//! 每个会话有一个专用线程 + 单线程 tokio 运行时，独占 SSH 连接与 shell 通道。
//! 外部通过 mpsc 发送 IoCmd；shell 输出通过 Tauri 事件推给前端。

use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    mpsc as std_mpsc, Arc,
};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::{mpsc as tokio_mpsc, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use russh::client;
use russh::ChannelMsg;

use crate::security::SecurityEngine;
use crate::state::{AIEventKind, AppState, AuthConfig, SessionConfig, SessionStatus};

/// 追加 SSH 渠道（同一连接上的独立 shell 通道）指令
pub enum ExtraChannelCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

/// 复制渠道的挂起输出状态：终端挂载前先缓存，避免丢失首次提示符。
/// replay 与前端挂载缓存分开维护，供 MCP 按连接读取，互不影响。
pub struct ExtraChannelState {
    attached: AtomicBool,
    buffer: Mutex<String>,
    replay: Mutex<TerminalReplayBuffer>,
    connected_at: DateTime<Utc>,
    generation: u64,
}

#[derive(Clone)]
struct ExtraChannelEntry {
    tx: tokio_mpsc::UnboundedSender<ExtraChannelCmd>,
    state: std::sync::Arc<ExtraChannelState>,
}

/// 发送给会话线程的指令
pub enum IoCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
    Exec {
        cmd: String,
        timeout: Duration,
        reply: std_mpsc::Sender<(i32, String, String)>,
    },
    /// 以 root 执行（方案 B）：密码只经通道 stdin 传入，绝不进入 argv。
    ExecRoot {
        cmd: String,
        cwd: String,
        timeout: Duration,
        password: Option<String>,
        reply: std_mpsc::Sender<Result<(i32, String, String), String>>,
    },
    Close,
}

#[derive(Debug)]
pub enum ClientError {
    Russh(russh::Error),
    HostKey(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Russh(error) => write!(f, "{error}"),
            Self::HostKey(message) => f.write_str(message),
        }
    }
}

impl From<russh::Error> for ClientError {
    fn from(error: russh::Error) -> Self {
        Self::Russh(error)
    }
}

/// SSH 客户端回调：统一执行主机密钥 TOFU 校验，同时把握手细节写入会话日志。
pub struct ClientHandler {
    session_id: String,
    host: String,
    port: u16,
    log_enabled: bool,
}

impl ClientHandler {
    fn new(cfg: &SessionConfig) -> Self {
        Self {
            session_id: cfg.id.clone(),
            host: cfg.host.clone(),
            port: cfg.port,
            log_enabled: cfg.log_enabled,
        }
    }

    fn log(&self, level: &str, message: &str) {
        if self.log_enabled {
            crate::logging::log_event(&self.session_id, level, message);
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = ClientError;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let record = crate::host_key::verify_or_trust(&self.host, self.port, server_public_key)
            .map_err(ClientError::HostKey)?;
        self.log(
            "INFO",
            &format!(
                "握手：主机密钥 {} 校验通过，指纹 {}",
                record.algorithm, record.fingerprint
            ),
        );
        Ok(true)
    }

    /// 远端转发（ssh -R）：服务器接受连接后发起通道，接到本地目标
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        match crate::forward::remote_target_lookup(&self.session_id, connected_port) {
            Some((host, port)) => {
                self.log(
                    "INFO",
                    &format!(
                        "远端转发：服务器 {connected_address}:{connected_port} 有入站连接 → 本地 {host}:{port}"
                    ),
                );
                let sid = self.session_id.clone();
                let enabled = self.log_enabled;
                tokio::spawn(async move {
                    crate::forward::pump_channel(channel, host, port, sid, enabled).await;
                });
            }
            None => {
                tokio::spawn(async move {
                    let _ = channel.close().await;
                });
            }
        }
        Ok(())
    }
}

pub type ClientHandle = client::Handle<ClientHandler>;

/// 最终 SSH 客户端及其跳板连接。跳板必须和最终连接同生命周期，不能在建链后提前析构。
pub struct Handle {
    client: ClientHandle,
    _proxy_hops: Vec<ClientHandle>,
}

impl std::ops::Deref for Handle {
    type Target = ClientHandle;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl std::ops::DerefMut for Handle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

type DynamicAgent = russh::keys::agent::client::AgentClient<
    Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin + 'static>,
>;

#[cfg(windows)]
async fn connect_system_agents() -> Result<Vec<DynamicAgent>, String> {
    use russh::keys::agent::client::AgentClient;

    let mut agents = Vec::new();
    if let Ok(Ok(agent)) = tokio::time::timeout(
        Duration::from_secs(2),
        AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent"),
    )
    .await
    {
        agents.push(agent.dynamic());
    }
    // Pageant 的连接是惰性的；身份查询失败时仍要保留另一个后端的回退机会。
    agents.push(AgentClient::connect_pageant().await.dynamic());
    Ok(agents)
}

#[cfg(unix)]
async fn connect_system_agents() -> Result<Vec<DynamicAgent>, String> {
    russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map(|agent| vec![agent.dynamic()])
        .map_err(|error| format!("连接 SSH_AUTH_SOCK 失败: {error}"))
}

#[cfg(not(any(windows, unix)))]
async fn connect_system_agents() -> Result<Vec<DynamicAgent>, String> {
    Err("当前平台不支持 SSH Agent".into())
}

async fn authenticate_with_agent(
    handle: &mut ClientHandle,
    cfg: &SessionConfig,
) -> Result<(), String> {
    let agents = connect_system_agents().await.map_err(|error| {
        let msg = format!("SSH Agent 不可用: {error}");
        log_info(cfg, &format!("ERROR {msg}"));
        msg
    })?;
    let mut last_query_error = None;
    let mut last_auth_error = None;
    let mut saw_identity = false;

    for mut agent in agents {
        let identities = match agent.request_identities().await {
            Ok(identities) => identities,
            Err(error) => {
                last_query_error = Some(error.to_string());
                continue;
            }
        };
        for identity in identities {
            saw_identity = true;
            match handle
                .authenticate_publickey_with(&cfg.username, identity, None, &mut agent)
                .await
            {
                Ok(client::AuthResult::Success) => return Ok(()),
                Ok(_) => continue,
                Err(error) => last_auth_error = Some(error.to_string()),
            }
        }
    }
    let msg = if let Some(error) = last_auth_error {
        format!("SSH Agent 认证失败: {error}")
    } else if saw_identity {
        "SSH Agent 中的身份均未通过认证".into()
    } else if let Some(error) = last_query_error {
        format!("读取 SSH Agent 身份失败: {error}")
    } else {
        "SSH Agent 中没有可用身份，请先添加私钥".into()
    };
    log_info(cfg, &format!("ERROR {msg}"));
    Err(msg)
}

fn client_config(cfg: &SessionConfig) -> Arc<client::Config> {
    let mut config = client::Config::default();
    config.keepalive_interval =
        (cfg.keepalive_secs > 0).then(|| Duration::from_secs(u64::from(cfg.keepalive_secs)));
    config.keepalive_max = 3;
    Arc::new(config)
}

async fn authenticate_handle(handle: &mut ClientHandle, cfg: &SessionConfig) -> Result<(), String> {
    let method = match &cfg.auth {
        AuthConfig::Password { .. } => "密码",
        AuthConfig::Key { .. } => "公钥",
        AuthConfig::Agent => "Agent",
    };
    log_info(cfg, &format!("开始{method}认证，用户 {}", cfg.username));

    match &cfg.auth {
        AuthConfig::Password { password_ref } => {
            let password = crate::secret_store::resolve_secret(password_ref)
                .map_err(|error| format!("读取已保存密码失败: {error}"))?;
            let res = handle
                .authenticate_password(&cfg.username, password)
                .await
                .map_err(|e| {
                    let msg = format!("密码认证失败: {e}");
                    log_info(cfg, &format!("ERROR {msg}"));
                    msg
                })?;
            if !matches!(res, client::AuthResult::Success) {
                let msg = "认证失败：用户名或密码不正确".to_string();
                log_info(cfg, &format!("ERROR {msg}"));
                return Err(msg);
            }
        }
        AuthConfig::Key {
            private_key_path,
            private_key_inline_ref,
            passphrase_ref,
        } => {
            let passphrase = passphrase_ref
                .as_deref()
                .map(crate::secret_store::resolve_secret)
                .transpose()
                .map_err(|error| format!("读取已保存私钥口令失败: {error}"))?;
            let inline = private_key_inline_ref
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(crate::secret_store::resolve_secret)
                .transpose()
                .map_err(|error| format!("读取已保存私钥内容失败: {error}"))?;
            let key = match inline.as_deref() {
                Some(content) => russh::keys::decode_secret_key(content, passphrase.as_deref())
                    .map_err(|e| {
                        let msg = format!("解析私钥内容失败: {e}");
                        log_info(cfg, &format!("ERROR {msg}"));
                        msg
                    })?,
                None => russh::keys::load_secret_key(private_key_path, passphrase.as_deref())
                    .map_err(|e| {
                        let msg = format!("读取私钥失败: {e}");
                        log_info(cfg, &format!("ERROR {msg}"));
                        msg
                    })?,
            };
            let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let res = handle
                .authenticate_publickey(&cfg.username, key)
                .await
                .map_err(|e| {
                    let msg = format!("密钥认证失败: {e}");
                    log_info(cfg, &format!("ERROR {msg}"));
                    msg
                })?;
            if !matches!(res, client::AuthResult::Success) {
                let msg = "密钥认证未通过".to_string();
                log_info(cfg, &format!("ERROR {msg}"));
                return Err(msg);
            }
        }
        AuthConfig::Agent => authenticate_with_agent(handle, cfg).await?,
    }
    log_info(cfg, "认证成功，SSH 会话已建立");
    Ok(())
}

async fn connect_first_hop(cfg: &SessionConfig) -> Result<ClientHandle, String> {
    log_info(cfg, &format!("TCP 连接 {}:{} …", cfg.host, cfg.port));
    if cfg.log_enabled {
        match probe_server_banner(&cfg.host, cfg.port).await {
            Ok(b) => log_info(cfg, &format!("握手：服务器 SSH 标识 {b}")),
            Err(e) => log_info(cfg, &format!("WARN 握手：读取服务器标识失败（{e}）")),
        }
        log_info(
            cfg,
            &format!(
                "握手：开始密钥交换（客户端 russh {}，支持 curve25519-sha256 / ssh-ed25519）",
                env!("CARGO_PKG_VERSION")
            ),
        );
    }
    let connect_timeout = Duration::from_secs(u64::from(cfg.connect_timeout_secs.max(1)));
    match tokio::time::timeout(
        connect_timeout,
        client::connect(
            client_config(cfg),
            (cfg.host.clone(), cfg.port),
            ClientHandler::new(cfg),
        ),
    )
    .await
    {
        Ok(Ok(handle)) => {
            log_info(cfg, "握手：密钥交换完成，已建立加密通道");
            Ok(handle)
        }
        Ok(Err(error)) => {
            let detail = error.to_string();
            if detail.starts_with("HOST_KEY_CHANGED:") {
                return Err(detail);
            }
            let msg = format!("SSH 连接 {}:{} 失败: {detail}", cfg.host, cfg.port);
            log_info(cfg, &format!("ERROR {msg}"));
            Err(msg)
        }
        Err(_) => {
            let msg = format!(
                "SSH 连接 {}:{} 超过 {} 秒未完成",
                cfg.host, cfg.port, cfg.connect_timeout_secs
            );
            log_info(cfg, &format!("ERROR {msg}"));
            Err(msg)
        }
    }
}

async fn connect_tunneled_hop(
    previous: &ClientHandle,
    cfg: &SessionConfig,
) -> Result<ClientHandle, String> {
    log_info(
        cfg,
        &format!(
            "ProxyJump：经前一跳 direct-tcpip 连接 {}:{}",
            cfg.host, cfg.port
        ),
    );
    let connect_timeout = Duration::from_secs(u64::from(cfg.connect_timeout_secs.max(1)));
    let channel = match tokio::time::timeout(
        connect_timeout,
        previous.channel_open_direct_tcpip(cfg.host.clone(), u32::from(cfg.port), "127.0.0.1", 0),
    )
    .await
    {
        Ok(Ok(channel)) => channel,
        Ok(Err(error)) => {
            return Err(format!(
                "ProxyJump 打开到 {}:{} 的 direct-tcpip 通道失败: {error}",
                cfg.host, cfg.port
            ));
        }
        Err(_) => {
            return Err(format!(
                "ProxyJump 打开到 {}:{} 的 direct-tcpip 通道超过 {} 秒未完成",
                cfg.host, cfg.port, cfg.connect_timeout_secs
            ));
        }
    };
    match tokio::time::timeout(
        connect_timeout,
        client::connect_stream(
            client_config(cfg),
            channel.into_stream(),
            ClientHandler::new(cfg),
        ),
    )
    .await
    {
        Ok(Ok(handle)) => {
            log_info(cfg, "握手：密钥交换完成，已建立加密通道");
            Ok(handle)
        }
        Ok(Err(error)) => {
            let detail = error.to_string();
            if detail.starts_with("HOST_KEY_CHANGED:") {
                return Err(detail);
            }
            Err(format!(
                "ProxyJump SSH 连接 {}:{} 失败: {detail}",
                cfg.host, cfg.port
            ))
        }
        Err(_) => Err(format!(
            "ProxyJump SSH 连接 {}:{} 超过 {} 秒未完成",
            cfg.host, cfg.port, cfg.connect_timeout_secs
        )),
    }
}

/// 建立并认证一条 SSH 连接；跳转链中的每一跳使用各自配置独立执行 TOFU 与认证。
pub async fn connect_handle(cfg: &SessionConfig) -> Result<Handle, String> {
    if cfg.proxy_jump.is_empty() {
        let mut client = connect_first_hop(cfg).await?;
        authenticate_handle(&mut client, cfg).await?;
        return Ok(Handle {
            client,
            _proxy_hops: Vec::new(),
        });
    }

    let saved = crate::state::load_persisted_sessions();
    let mut route = crate::state::resolve_proxy_chain(cfg, &saved)?;
    route.push(cfg.clone());
    let mut route = route.into_iter();
    let first = route.next().expect("route includes target");
    let mut current = connect_first_hop(&first).await?;
    authenticate_handle(&mut current, &first).await?;
    let mut proxy_hops = Vec::new();

    for next in route {
        let next_handle = match connect_tunneled_hop(&current, &next).await {
            Ok(mut handle) => {
                if let Err(error) = authenticate_handle(&mut handle, &next).await {
                    return Err(error);
                }
                handle
            }
            Err(error) => return Err(error),
        };
        proxy_hops.push(current);
        current = next_handle;
    }

    Ok(Handle {
        client: current,
        _proxy_hops: proxy_hops,
    })
}

/// 写一条连接日志（未开启日志时为空操作）
fn log_info(cfg: &SessionConfig, message: &str) {
    if cfg.log_enabled {
        crate::logging::log_event(&cfg.id, "INFO", message);
    }
}

struct EstablishedConnection {
    handle: Arc<Handle>,
    shell: russh::Channel<client::Msg>,
    latency_ms: u64,
}

async fn open_shell(cfg: &SessionConfig) -> Result<EstablishedConnection, String> {
    let handle = Arc::new(connect_handle(cfg).await?);
    let started = std::time::Instant::now();
    let shell = handle.channel_open_session().await.map_err(|error| {
        let message = format!("打开会话通道失败: {error}");
        log_info(cfg, &format!("ERROR {message}"));
        message
    })?;
    // channel_open_session 会等待服务端 CHANNEL_OPEN_CONFIRMATION，是真实 SSH 请求 RTT。
    let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    shell
        .request_pty(true, "xterm-256color", 120, 40, 0, 0, &[])
        .await
        .map_err(|error| {
            let message = format!("请求 PTY 失败: {error}");
            log_info(cfg, &format!("ERROR {message}"));
            message
        })?;
    shell.request_shell(true).await.map_err(|error| {
        let message = format!("启动远程 shell 失败: {error}");
        log_info(cfg, &format!("ERROR {message}"));
        message
    })?;
    log_info(cfg, "PTY 已建立（120x40），进入交互 Shell");
    Ok(EstablishedConnection {
        handle,
        shell,
        latency_ms,
    })
}

const RECONNECT_BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];

fn reconnect_delay_with_jitter(attempt: u32, jitter_per_mille: u16) -> Duration {
    let index = attempt
        .saturating_sub(1)
        .min(RECONNECT_BACKOFF_SECS.len() as u32 - 1);
    let base_ms = RECONNECT_BACKOFF_SECS[index as usize] * 1000;
    let max_jitter_ms = base_ms / 5;
    let jitter_ms = max_jitter_ms * u64::from(jitter_per_mille.min(1000)) / 1000;
    Duration::from_millis(base_ms + jitter_ms)
}

fn reconnect_delay(session_id: &str, generation: u64, attempt: u32) -> Duration {
    // FNV-1a 让不同会话/断线周期获得稳定且可复现的 jitter，测试无需依赖随机数。
    let mut hash = 0xcbf29ce484222325u64;
    for byte in session_id
        .bytes()
        .chain(generation.to_le_bytes())
        .chain(attempt.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    reconnect_delay_with_jitter(attempt, (hash % 1001) as u16)
}

fn is_non_retryable_reconnect_error(error: &str) -> bool {
    error.contains("HOST_KEY_CHANGED:")
        || error.contains("HOSTKEY_CHANGED")
        || error.contains("认证失败")
        || error.contains("认证未通过")
        || error.contains("身份均未通过认证")
}

/// 重连期间只接受 Close；旧输入和任务绝不递交给新一代连接。
fn discard_command_while_disconnected(cmd: Option<IoCmd>) -> bool {
    match cmd {
        None | Some(IoCmd::Close) => true,
        Some(IoCmd::Input(_)) | Some(IoCmd::Resize(_, _)) => false,
        Some(IoCmd::Exec { reply, .. }) => {
            let _ = reply.send((
                -1,
                String::new(),
                "连接已断开，命令不会在重连后自动重放".into(),
            ));
            false
        }
        Some(IoCmd::ExecRoot { reply, .. }) => {
            let _ = reply.send(Err("连接已断开，命令不会在重连后自动重放".into()));
            false
        }
    }
}

enum ReconnectResult {
    Connected(EstablishedConnection),
    Closed,
    Failed(String),
}

async fn reconnect_connection(
    cfg: &SessionConfig,
    rx: &mut tokio_mpsc::UnboundedReceiver<IoCmd>,
    max_attempts: u32,
    generation: u64,
) -> ReconnectResult {
    let mut last_error = "自动重连失败".to_string();
    for attempt in 1..=max_attempts {
        let delay = reconnect_delay(&cfg.id, generation, attempt);
        log_info(
            cfg,
            &format!(
                "自动重连第 {attempt}/{max_attempts} 次将在 {:.3} 秒后开始",
                delay.as_secs_f64()
            ),
        );
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = rx.recv() => {
                    if discard_command_while_disconnected(cmd) {
                        return ReconnectResult::Closed;
                    }
                }
            }
        }

        let attempt_result = {
            let connect = open_shell(cfg);
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    result = &mut connect => break result,
                    cmd = rx.recv() => {
                        if discard_command_while_disconnected(cmd) {
                            return ReconnectResult::Closed;
                        }
                    }
                }
            }
        };
        match attempt_result {
            Ok(connection) => {
                log_info(cfg, &format!("自动重连第 {attempt} 次成功"));
                return ReconnectResult::Connected(connection);
            }
            Err(error) if is_non_retryable_reconnect_error(&error) => {
                log_info(cfg, &format!("ERROR 自动重连终止：{error}"));
                return ReconnectResult::Failed(error);
            }
            Err(error) => {
                log_info(cfg, &format!("WARN 自动重连第 {attempt} 次失败：{error}"));
                last_error = error;
            }
        }
    }
    ReconnectResult::Failed(format!(
        "自动重连次数已用尽（{max_attempts} 次）：{last_error}"
    ))
}

fn emit_event(app: &tauri::AppHandle, event: &str, payload: &serde_json::Value) {
    let _ = app.emit(event, payload.clone());
}

const TERMINAL_REPLAY_CAPACITY_BYTES: usize = 256 * 1024;
const TERMINAL_REPLAY_MAX_CHUNKS: usize = 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TerminalChunk {
    pub seq: u64,
    pub generation: u64,
    pub data: String,
}

#[derive(Debug, Serialize)]
pub struct TerminalReplay {
    pub generation: u64,
    pub chunks: Vec<TerminalChunk>,
    pub next_seq: u64,
    pub truncated: bool,
}

#[derive(Default)]
struct TerminalReplayBuffer {
    chunks: VecDeque<TerminalChunk>,
    bytes: usize,
    latest_seq: u64,
}

/// 可接入的终端连接描述：主 Shell 与每个复制渠道各一条。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionDescriptor {
    pub connection_id: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<DateTime<Utc>>,
    pub login_user: String,
    /// 交互 Shell 当前用户：只有可靠 shell integration 上报时才填写，否则为 null。
    pub prompt_user: Option<String>,
    pub prompt_user_source: &'static str,
    pub prompt_observed_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub label: String,
}

/// attach 的不可变连接绑定：token 生效期间不允许改绑其他连接或新 generation。
#[derive(Debug, Clone)]
pub struct AttachBinding {
    pub connection_id: String,
    pub generation: u64,
    pub require_active: bool,
}

#[derive(Default)]
struct ActiveTerminal {
    channel_id: Option<String>,
    known: bool,
    revision: u64,
}

impl TerminalReplayBuffer {
    fn push(&mut self, generation: u64, data: &str) -> Option<TerminalChunk> {
        if data.is_empty() {
            return None;
        }
        let data = if data.len() > TERMINAL_REPLAY_CAPACITY_BYTES {
            let mut start = data.len() - TERMINAL_REPLAY_CAPACITY_BYTES;
            while !data.is_char_boundary(start) {
                start += 1;
            }
            data[start..].to_string()
        } else {
            data.to_string()
        };
        self.latest_seq = self.latest_seq.saturating_add(1);
        let chunk = TerminalChunk {
            seq: self.latest_seq,
            generation,
            data,
        };
        self.bytes = self.bytes.saturating_add(chunk.data.len());
        self.chunks.push_back(chunk.clone());
        while self.bytes > TERMINAL_REPLAY_CAPACITY_BYTES
            || self.chunks.len() > TERMINAL_REPLAY_MAX_CHUNKS
        {
            if let Some(removed) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.data.len());
            }
        }
        Some(chunk)
    }

    fn read(&self, generation: u64, after_seq: Option<u64>) -> TerminalReplay {
        let after = after_seq.unwrap_or(0);
        let chunks = self
            .chunks
            .iter()
            .filter(|chunk| after_seq.is_none() || chunk.seq > after)
            .cloned()
            .collect::<Vec<_>>();
        let truncated = after_seq.is_some()
            && self
                .chunks
                .front()
                .is_some_and(|oldest| oldest.seq > after.saturating_add(1));
        TerminalReplay {
            generation,
            chunks,
            next_seq: self.latest_seq,
            truncated,
        }
    }
}

/// 把 SSH 的任意字节边界转换为合法 UTF-8；跨消息的多字节字符不会被拆成替换字符。
fn decode_utf8_stream(pending: &mut Vec<u8>, bytes: &[u8], flush: bool) -> String {
    pending.extend_from_slice(bytes);
    let mut offset = 0;
    let mut output = String::new();
    while offset < pending.len() {
        match std::str::from_utf8(&pending[offset..]) {
            Ok(text) => {
                output.push_str(text);
                offset = pending.len();
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    output.push_str(
                        std::str::from_utf8(&pending[offset..offset + valid])
                            .expect("valid_up_to 必须指向合法 UTF-8"),
                    );
                    offset += valid;
                }
                match error.error_len() {
                    Some(invalid_len) => {
                        output.push('\u{fffd}');
                        offset += invalid_len;
                    }
                    None if flush => {
                        output.push('\u{fffd}');
                        offset = pending.len();
                    }
                    None => break,
                }
            }
        }
    }
    pending.drain(..offset);
    output
}

fn emit_terminal_text(
    session: &SessionHandle,
    generation: u64,
    app: &tauri::AppHandle,
    session_id: &str,
    text: String,
) {
    if session.generation.load(Ordering::Acquire) != generation {
        return;
    }
    let Some(chunk) = session.replay.lock().push(generation, &text) else {
        return;
    };
    emit_event(
        app,
        "terminal-data",
        &serde_json::json!({
            "session_id": session_id,
            "generation": generation,
            "seq": chunk.seq,
            "data": text,
        }),
    );
}

fn emit_ai_text(
    session: &SessionHandle,
    generation: u64,
    app: &tauri::AppHandle,
    session_id: &str,
    text: String,
) {
    if session.generation.load(Ordering::Acquire) != generation {
        return;
    }
    let Some(chunk) = session.ai_replay.lock().push(generation, &text) else {
        return;
    };
    // AI 回显始终落盘，保证应用重启后仍能追溯 AI 执行过什么、看到了什么。
    let config = session.config_snapshot();
    crate::logging::append_ai_terminal_log(session_id, &config.host, config.port, &text);
    let echo_to_terminal = app
        .state::<AppState>()
        .inner
        .read()
        .settings
        .ai_echo_to_terminal;
    if echo_to_terminal {
        if config.log_enabled {
            crate::logging::append_session_log(session_id, &text);
        }
        emit_terminal_text(session, generation, app, session_id, text.clone());
    }
    emit_event(
        app,
        "ai-terminal-data",
        &serde_json::json!({
            "session_id": session_id,
            "connection_id": session.ai_connection_id(),
            "generation": generation,
            "seq": chunk.seq,
            "data": text,
        }),
    );
}

fn emit_for_generation(
    session: &SessionHandle,
    generation: u64,
    app: &tauri::AppHandle,
    event: &str,
    mut payload: serde_json::Value,
) {
    if session.generation.load(Ordering::Acquire) != generation {
        return;
    }
    payload["generation"] = serde_json::json!(generation);
    emit_event(app, event, &payload);
}

/// 读取服务器 SSH 版本标识（握手第一阶段），仅用于日志
async fn probe_server_banner(host: &str, port: u16) -> Result<String, String> {
    use tokio::io::AsyncReadExt;

    let connect = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .map_err(|_| "连接超时".to_string())?
    .map_err(|e| e.to_string())?;

    let mut stream = connect;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(4), stream.read(&mut chunk))
            .await
            .map_err(|_| "读取超时".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            buf.truncate(pos);
            break;
        }
        if buf.len() > 1024 {
            break;
        }
    }
    if buf.is_empty() {
        return Err("无响应".into());
    }
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// 执行一条命令并收集输出
///
/// 注意：部分服务器在通道 CLOSE 之前不保证投递 exit-status（russh 在 Close 后
/// 会丢弃该消息），会导致退出码丢失、被误判为失败。因此这里在命令末尾追加
/// 一个回显退出码的标记行，作为可靠的退出码来源。
const RC_MARK: &str = "__FORGE_RC__";

/// 单条命令输出采集上限（stdout / stderr 各自），超出即截断。
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
/// 截断后仍保留的尾部字节数，用于解析退出码标记。
const TAIL_KEEP_BYTES: usize = 8 * 1024;

/// 有上限的输出缓冲区：保留开头用于展示，保留结尾用于解析退出码标记。
#[derive(Default)]
struct CaptureBuffer {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl CaptureBuffer {
    fn push(&mut self, data: &[u8]) {
        self.total += data.len();
        let room = MAX_CAPTURE_BYTES.saturating_sub(self.head.len());
        if room > 0 {
            let take = room.min(data.len());
            self.head.extend_from_slice(&data[..take]);
            if take < data.len() {
                self.push_tail(&data[take..]);
            }
        } else {
            self.push_tail(data);
        }
    }

    fn push_tail(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        if self.tail.len() > TAIL_KEEP_BYTES {
            let excess = self.tail.len() - TAIL_KEEP_BYTES;
            self.tail.drain(..excess);
        }
    }

    /// 返回（解析出的退出码，展示文本）。截断时只展示开头并追加 [truncated]。
    fn finish(self) -> (Option<i32>, String) {
        let truncated = self.total > MAX_CAPTURE_BYTES;
        let code = if truncated {
            parse_exit_marker(String::from_utf8_lossy(&self.tail).to_string()).0
        } else {
            parse_exit_marker(String::from_utf8_lossy(&self.head).to_string()).0
        };
        let mut text = String::from_utf8_lossy(&self.head).to_string();
        if truncated {
            text.push_str("\n[truncated]");
        } else {
            text = parse_exit_marker(text).1;
        }
        (code, text)
    }
}

/// 打开独立 exec channel 执行命令，可选把密码写入 stdin（仅用于 sudo -S）。
async fn exec_channel(
    handle: &Handle,
    exec_cmd: &str,
    timeout: Duration,
    stdin_payload: Option<&str>,
) -> (i32, String, String) {
    let mut ch = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => return (-1, String::new(), e.to_string()),
    };
    if let Err(e) = ch.exec(true, exec_cmd).await {
        return (-1, String::new(), e.to_string());
    }
    if let Some(payload) = stdin_payload {
        // 等 sudo 挂起密码读取后再写；密码只经 stdin，不进入 argv / 环境变量。
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut line = String::with_capacity(payload.len() + 1);
        line.push_str(payload);
        line.push('\n');
        if let Err(e) = ch.data(line.as_bytes()).await {
            return (-1, String::new(), format!("写入 sudo 密码失败: {e}"));
        }
        // 立刻 EOF：口令我只会被 sudo 读走一次，后续命令不能从 stdin 读到残留。
        let _ = ch.eof().await;
    }

    let mut out = CaptureBuffer::default();
    let mut err = CaptureBuffer::default();
    let mut status: Option<i32> = None;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            msg = ch.wait() => {
                match msg {
                Some(ChannelMsg::Data { data }) => out.push(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => err.push(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status as i32),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
                }
            },
            _ = tokio::time::sleep_until(deadline) => {
                err.push(b"[timeout] command channel closed");
                let _ = ch.close().await;
                break;
            }
        }
    }

    let (marker_code, out_s) = out.finish();
    let (_, err_s) = err.finish();
    let code = marker_code
        .or(status)
        .unwrap_or(if err_s.trim().is_empty() { 0 } else { 1 });

    (code, out_s, err_s)
}

async fn exec_collect(handle: &Handle, cmd: &str, timeout: Duration) -> (i32, String, String) {
    let wrapped = format!(
        "({cmd}); __forge_rc=$?; printf '\\n{marker}%d\\n' \"$__forge_rc\"",
        marker = RC_MARK
    );
    exec_channel(handle, &wrapped, timeout, None).await
}

/// 单引号转义，保证 AI 提交的命令不会被 shell 提前解释。
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 以 root 执行一条命令。密码只经 stdin 写入，命令与密码都不落日志。
async fn exec_collect_root(
    handle: &Handle,
    cmd: &str,
    cwd: &str,
    timeout: Duration,
    password: Option<&str>,
) -> Result<(i32, String, String), String> {
    let inner = format!(
        "cd {cwd} && ({cmd}); __forge_rc=$?; printf '\\n{marker}%d\\n' \"$__forge_rc\"; exit $__forge_rc",
        cwd = shell_single_quote(cwd),
        marker = RC_MARK
    );
    // 先探测 sudo 凭据缓存：命中时 `sudo -S` 不会读取 stdin，
    // 写入口令反而会残留在后续命令的标准输入里。
    let password = match password {
        Some(pw) => {
            let (probe_code, _, _) = exec_channel(handle, "sudo -n true", timeout, None).await;
            if probe_code == 0 {
                None
            } else {
                crate::logging::register_secret(pw);
                Some(pw)
            }
        }
        None => None,
    };
    let exec_cmd = if password.is_some() {
        format!("sudo -S -p '' -- sh -lc {}", shell_single_quote(&inner))
    } else {
        format!("sudo -n -- sh -lc {}", shell_single_quote(&inner))
    };
    let (code, stdout, stderr) = exec_channel(handle, &exec_cmd, timeout, password).await;
    if code != 0 {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("not in the sudoers") || lower.contains("is not allowed to execute") {
            return Err("SUDO_NOT_PERMITTED".into());
        }
        if lower.contains("a password is required") || lower.contains("no tty present") {
            return Err("SUDO_PASSWORD_REQUIRED".into());
        }
        if lower.contains("incorrect password")
            || lower.contains("sorry, try again")
            || lower.contains("authentication failure")
        {
            return Err("SUDO_AUTH_FAILED".into());
        }
    }
    Ok((code, stdout, stderr))
}

fn parse_exit_marker(mut out_s: String) -> (Option<i32>, String) {
    let mut code: Option<i32> = None;
    if let Some(pos) = out_s.rfind(RC_MARK) {
        let after = &out_s[pos + RC_MARK.len()..];
        let digits: String = after
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        code = digits.parse::<i32>().ok();
        // 截掉标记行（含其前面那个换行）
        let mut cut = pos;
        while cut > 0 && (out_s.as_bytes()[cut - 1] == b'\n' || out_s.as_bytes()[cut - 1] == b'\r')
        {
            cut -= 1;
        }
        out_s.truncate(cut);
    }
    (code, out_s)
}

/// 命令文本来自外部 MCP 客户端，显示前过滤控制字符，避免伪造终端界面。
fn sanitize_ai_command_for_terminal(command: &str) -> String {
    command
        .chars()
        .map(|ch| match ch {
            '\n' | '\t' => ch,
            '\u{1b}' => '␛',
            ch if ch.is_control() => '�',
            _ => ch,
        })
        .collect()
}

fn format_ai_exec_start(command: &str) -> String {
    format!(
        "\r\n\x1b[38;5;111m[AI 执行]\x1b[0m {}\r\n",
        sanitize_ai_command_for_terminal(command)
    )
}

fn push_ai_output_section(text: &mut String, title: &str, output: &str) {
    text.push_str(title);
    text.push_str("\r\n");
    if output.is_empty() {
        text.push_str("（无输出）\r\n");
    } else {
        text.push_str(output);
        if !output.ends_with(['\r', '\n']) {
            text.push_str("\r\n");
        }
    }
}

fn format_ai_exec_result(
    command: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    duration_ms: u128,
) -> String {
    let timed_out = stderr.contains("[timeout]") || stderr.contains("命令执行超时");
    let status = if timed_out {
        "超时"
    } else if exit_code == 0 {
        "成功"
    } else {
        "失败"
    };
    let color = if exit_code == 0 && !timed_out {
        114
    } else {
        203
    };
    let mut text = format!(
        "\r\n\x1b[38;5;{color}m[AI 结果 · {status}]\x1b[0m {}\r\n",
        sanitize_ai_command_for_terminal(command)
    );
    push_ai_output_section(&mut text, "\x1b[90m[标准输出]\x1b[0m", stdout);
    push_ai_output_section(&mut text, "\x1b[90m[错误输出]\x1b[0m", stderr);
    text.push_str(&format!(
        "\x1b[38;5;{color}m[AI 状态]\x1b[0m 退出码={exit_code}，耗时={duration_ms} 毫秒\r\n"
    ));
    text
}

fn format_ai_exec_start_root(command: &str) -> String {
    format!(
        "\r\n\x1b[38;5;214m[AI 执行 root]\x1b[0m {}\r\n",
        sanitize_ai_command_for_terminal(command)
    )
}

fn format_ai_exec_result_root(
    command: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    duration_ms: u128,
) -> String {
    let timed_out = stderr.contains("[timeout]") || stderr.contains("命令执行超时");
    let status = if timed_out {
        "超时"
    } else if exit_code == 0 {
        "成功"
    } else {
        "失败"
    };
    let color = if exit_code == 0 && !timed_out {
        114
    } else {
        203
    };
    let mut text = format!(
        "\r\n\x1b[38;5;{color}m[AI 结果（root）· {status}]\x1b[0m {}\r\n",
        sanitize_ai_command_for_terminal(command)
    );
    push_ai_output_section(&mut text, "\x1b[90m[标准输出]\x1b[0m", stdout);
    push_ai_output_section(&mut text, "\x1b[90m[错误输出]\x1b[0m", stderr);
    text.push_str(&format!(
        "\x1b[38;5;{color}m[AI 状态（root）]\x1b[0m 退出码={exit_code}，耗时={duration_ms} 毫秒\r\n"
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_marker_parses_normal_exit_code_and_removes_marker_line() {
        let (code, out) = parse_exit_marker("hello\n__FORGE_RC__0\n".into());
        assert_eq!(code, Some(0));
        assert_eq!(out, "hello");
    }

    #[test]
    fn exit_marker_parses_negative_exit_code() {
        let (code, out) = parse_exit_marker("failed\n__FORGE_RC__-42\n".into());
        assert_eq!(code, Some(-42));
        assert_eq!(out, "failed");
    }

    #[test]
    fn invalid_exit_marker_has_no_code_but_retains_existing_truncation_behavior() {
        let (code, out) = parse_exit_marker("before\n__FORGE_RC__not-a-code\n".into());
        assert_eq!(code, None);
        assert_eq!(out, "before");
    }

    #[test]
    fn missing_exit_marker_leaves_output_unchanged() {
        let (code, out) = parse_exit_marker("plain output\n".into());
        assert_eq!(code, None);
        assert_eq!(out, "plain output\n");
    }

    #[test]
    fn exit_marker_removes_crlf_before_marker_without_rewriting_other_lines() {
        let (code, out) = parse_exit_marker("line1\r\nline2\r\n__FORGE_RC__7\r\n".into());
        assert_eq!(code, Some(7));
        assert_eq!(out, "line1\r\nline2");
    }

    #[test]
    fn ai_exec_start_preserves_multiline_commands_but_neutralizes_controls() {
        let text = format_ai_exec_start("echo '\u{1b}[2J'\r\nwhoami\t--all");
        assert!(text.contains("[AI 执行]"));
        assert!(text.contains("␛[2J"));
        assert!(!text.contains("\u{1b}[2J"));
        assert!(text.contains("�\nwhoami\t--all"));
    }

    #[test]
    fn ai_exec_result_contains_complete_success_output_without_mutating_result() {
        let result = (0, "hello".to_string(), String::new());
        let original = result.clone();
        let text = format_ai_exec_result("printf hello", result.0, &result.1, &result.2, 42);
        assert!(text.contains("[AI 结果 · 成功]"));
        assert!(text.contains("printf hello"));
        assert!(text.contains("hello\r\n"));
        assert!(text.contains("（无输出）"));
        assert!(text.contains("退出码=0，耗时=42 毫秒"));
        assert!(text.ends_with("\r\n"));
        assert_eq!(result, original);
    }

    #[test]
    fn ai_exec_result_marks_empty_output_and_nonzero_exit() {
        let text = format_ai_exec_result("false", 7, "", "failed\n", 9);
        assert!(text.contains("[AI 结果 · 失败]"));
        assert_eq!(text.matches("（无输出）").count(), 1);
        assert!(text.contains("failed\n\x1b[38;5;203m[AI 状态]"));
        assert!(text.contains("退出码=7，耗时=9 毫秒"));
    }

    #[test]
    fn ai_exec_result_distinguishes_timeout_and_channel_failure() {
        let timeout =
            format_ai_exec_result("sleep 60", -1, "", "[timeout] command channel closed", 1000);
        assert!(timeout.contains("[AI 结果 · 超时]"));
        assert!(timeout.contains("[timeout] command channel closed"));

        let failure = format_ai_exec_result("pwd", -1, "", "channel open failed", 3);
        assert!(failure.contains("[AI 结果 · 失败]"));
        assert!(failure.contains("channel open failed"));
        assert!(failure.contains("退出码=-1"));
    }

    #[test]
    fn reconnect_backoff_has_deterministic_bounded_jitter_and_a_30_second_cap() {
        let bases = [1, 2, 4, 8, 16, 30];
        for (index, seconds) in bases.into_iter().enumerate() {
            let attempt = index as u32 + 1;
            assert_eq!(
                reconnect_delay_with_jitter(attempt, 0),
                Duration::from_secs(seconds)
            );
            assert_eq!(
                reconnect_delay_with_jitter(attempt, 1000),
                Duration::from_millis(seconds * 1200)
            );
        }
        assert_eq!(reconnect_delay_with_jitter(20, 0), Duration::from_secs(30));
        assert_eq!(
            reconnect_delay("session-test", 7, 3),
            reconnect_delay("session-test", 7, 3)
        );
    }

    #[test]
    fn reconnect_rejects_host_key_and_authentication_failures_only() {
        assert!(is_non_retryable_reconnect_error("HOST_KEY_CHANGED:{}"));
        assert!(is_non_retryable_reconnect_error("HOSTKEY_CHANGED"));
        assert!(is_non_retryable_reconnect_error("密码认证失败: denied"));
        assert!(is_non_retryable_reconnect_error("密钥认证未通过"));
        assert!(!is_non_retryable_reconnect_error(
            "SSH 连接失败: connection reset"
        ));
        assert!(!is_non_retryable_reconnect_error(
            "打开会话通道失败: disconnected"
        ));
    }

    #[test]
    fn disconnected_commands_are_discarded_without_replay() {
        assert!(!discard_command_while_disconnected(Some(IoCmd::Input(
            b"stale".to_vec()
        ))));

        let (reply, response) = std_mpsc::channel();
        assert!(!discard_command_while_disconnected(Some(IoCmd::Exec {
            cmd: "should-not-run".into(),
            timeout: Duration::from_secs(1),
            reply,
        })));
        let (code, stdout, stderr) = response.recv().unwrap();
        assert_eq!(code, -1);
        assert!(stdout.is_empty());
        assert!(stderr.contains("不会"));

        assert!(discard_command_while_disconnected(Some(IoCmd::Close)));
    }

    #[test]
    fn generation_invalidates_stale_latency_and_disconnect_clears_samples() {
        let handle = attachable_handle();
        let first = handle.activate_connection(17);
        assert_eq!(handle.status().latency_ms, Some(17));
        handle.record_latency(first, 23);
        assert_eq!(handle.status().latency_ms, Some(23));

        handle.invalidate_connection();
        assert!(!handle.status().connected);
        assert_eq!(handle.status().latency_ms, None);
        handle.record_latency(first, 99);
        assert_eq!(handle.status().latency_ms, None);

        let second = handle.activate_connection(8);
        assert_ne!(first, second);
        handle.record_latency(first, 100);
        assert_eq!(handle.status().latency_ms, Some(8));
        handle.close();
        assert_eq!(handle.status().latency_ms, None);
    }

    #[test]
    fn disconnected_input_is_rejected_but_close_still_reaches_the_io_channel() {
        let handle = attachable_handle();
        handle.invalidate_connection();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        *handle.io_tx.lock() = Some(tx);

        assert!(handle.send_input("must-not-reach-old-shell").is_err());
        assert!(rx.try_recv().is_err());
        handle.close();
        assert!(matches!(rx.try_recv(), Ok(IoCmd::Close)));
    }

    #[test]
    fn ai_replay_is_independent_from_human_terminal_replay() {
        let handle = attachable_handle();
        handle.push_test_primary_output("HUMAN-MARK");
        handle.push_test_ai_output("AI-MARK");
        let ai = handle.read_ai_terminal(None);
        let ai_text = ai
            .chunks
            .iter()
            .map(|chunk| chunk.data.as_str())
            .collect::<String>();
        assert!(ai_text.contains("AI-MARK"));
        assert!(!ai_text.contains("HUMAN-MARK"));
        let human = handle
            .read_connection(&handle.ai_connection_id(), None)
            .unwrap()
            .1;
        let human_text = human
            .chunks
            .iter()
            .map(|chunk| chunk.data.as_str())
            .collect::<String>();
        assert!(human_text.contains("HUMAN-MARK"));
        assert!(!human_text.contains("AI-MARK"));
    }

    #[tokio::test]
    async fn close_interrupts_pending_reconnect_backoff_without_a_network_attempt() {
        let handle = attachable_handle();
        let config = handle.config_snapshot();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(IoCmd::Close);
        });
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            reconnect_connection(&config, &mut rx, 3, 1),
        )
        .await
        .expect("Close 应立即打断退避等待");
        assert!(matches!(result, ReconnectResult::Closed));
        closer.await.unwrap();
    }

    fn attachable_handle() -> SessionHandle {
        let config = SessionConfig {
            id: "session-test".into(),
            name: "session-test".into(),
            host: "127.0.0.1".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig::Agent,
            proxy_jump: Vec::new(),
            group: String::new(),
            ai_allowed: true,
            ai_policy: crate::state::AIPolicyOverride::Inherit,
            ai_blocklist_regexes: Vec::new(),
            auto_connect: false,
            keepalive_secs: 30,
            connect_timeout_secs: 30,
            port_forwards: Vec::new(),
            log_enabled: false,
        };
        let handle = SessionHandle::new(config, Vec::new(), Vec::new());
        handle.connected.store(true, Ordering::Relaxed);
        handle
    }

    #[test]
    fn sudo_quota_lockout_and_password_cache_track_attach_state() {
        let handle = attachable_handle();
        assert_eq!(handle.sudo_quota_used(), 0);
        assert_eq!(handle.sudo_consume_quota(), 1);
        assert_eq!(handle.sudo_quota_used(), 1);

        assert!(!handle.sudo_record_failure(3, 300));
        assert!(!handle.sudo_record_failure(3, 300));
        assert!(handle.sudo_record_failure(3, 300));
        assert!(handle.sudo_lockout_remaining_secs().is_some());
        handle.sudo_clear_failures();
        assert!(handle.sudo_lockout_remaining_secs().is_none());

        assert!(handle.sudo_cached_password().is_none());
        handle.sudo_store_password("temporary");
        assert_eq!(handle.sudo_cached_password().as_deref(), Some("temporary"));
        handle.sudo_forget_password();
        assert!(handle.sudo_cached_password().is_none());
    }

    #[test]
    fn sudo_permission_is_granted_only_when_requested() {
        let state = AppState::new();
        let handle = attachable_handle();
        let token = handle
            .begin_write_attach_with_client(&state, "test", "client-a", 30, false, true)
            .unwrap();
        assert!(handle
            .validate_attach_token(&token, "client-a", "sudo")
            .is_ok());

        let plain = attachable_handle();
        let plain_token = plain
            .begin_write_attach_with_client(&state, "test", "client-b", 30, false, false)
            .unwrap();
        assert!(plain
            .validate_attach_token(&plain_token, "client-b", "sudo")
            .is_err());
    }

    #[test]
    fn attach_token_is_bound_to_client_and_expires() {
        let state = AppState::new();
        let handle = attachable_handle();
        let token = handle
            .begin_attach_with_client(&state, "test", "client-a", 30)
            .unwrap();
        assert!(token.len() >= 36);
        handle
            .validate_attach_token(&token, "client-a", "exec")
            .unwrap();
        assert!(handle
            .validate_attach_token(&token, "client-b", "exec")
            .is_err());
        assert!(handle
            .validate_attach_token(&token, "client-a", "admin")
            .is_err());

        *handle.attach_expires_at.lock() = Some(Utc::now() - chrono::Duration::seconds(1));
        handle.expire_attach_if_needed(&state);
        assert!(!handle.attached.load(Ordering::Relaxed));
        assert!(handle.attach_token.lock().is_none());
        assert!(handle
            .validate_attach_token(&token, "client-a", "exec")
            .is_err());
    }

    #[test]
    fn idle_attach_detaches_and_replays_buffered_input() {
        let state = AppState::new();
        state.inner.write().settings.idle_detach_minutes = 10;
        let handle = attachable_handle();
        handle
            .begin_attach_with_client(&state, "test", "client-a", 30)
            .unwrap();
        handle.send_input("pwd\n").unwrap();
        *handle.attach_last_activity.lock() = Some(Utc::now() - chrono::Duration::minutes(11));

        handle.expire_attach_if_needed(&state);
        assert!(!handle.attached.load(Ordering::Relaxed));
        assert!(handle.pending_human_input.lock().is_empty());
    }

    #[test]
    fn attached_human_input_applies_backpressure_without_dropping_data() {
        let state = AppState::new();
        let handle = attachable_handle();
        let token = handle
            .begin_attach_with_client(&state, "test", "client-a", 30)
            .unwrap();
        let input = "x".repeat(512);
        handle.send_input(&input).unwrap();
        assert!(handle.send_input("y").is_err());
        assert_eq!(handle.pending_human_input.lock().len(), 512);
        assert!(handle
            .pending_human_input
            .lock()
            .iter()
            .all(|byte| *byte == b'x'));

        handle
            .end_attach_with_client(&state, &token, "client-a", "done")
            .unwrap();
        assert!(handle.pending_human_input.lock().is_empty());
    }

    #[test]
    fn byte_input_preserves_mouse_reports_and_string_wrapper_keeps_utf8() {
        let handle = attachable_handle();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        *handle.io_tx.lock() = Some(tx);
        let mouse_report = [0x1b, b'[', b'M', 0xff, 0x80, 0x00];

        handle.send_input_bytes(&mouse_report).unwrap();
        assert!(
            matches!(rx.try_recv(), Ok(IoCmd::Input(actual)) if actual == mouse_report.to_vec())
        );
        handle.send_input("中文").unwrap();
        assert!(matches!(rx.try_recv(), Ok(IoCmd::Input(actual)) if actual == "中文".as_bytes()));
    }

    #[test]
    fn attached_byte_input_preserves_backpressure_and_replays_exact_bytes() {
        let state = AppState::new();
        let handle = attachable_handle();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        *handle.io_tx.lock() = Some(tx);
        let token = handle
            .begin_attach_with_client(&state, "test", "client-a", 30)
            .unwrap();
        let mut input = vec![b'x'; 510];
        input.extend([0x80, 0xff]);

        handle.send_input_bytes(&input).unwrap();
        assert!(handle.send_input_bytes(&[0x81]).is_err());
        assert_eq!(handle.pending_human_input.lock().len(), 512);
        assert!(rx.try_recv().is_err());

        handle
            .end_attach_with_client(&state, &token, "client-a", "done")
            .unwrap();
        assert!(matches!(rx.try_recv(), Ok(IoCmd::Input(actual)) if actual == input));
        assert!(handle.pending_human_input.lock().is_empty());
    }

    #[test]
    fn extra_channel_byte_input_preserves_mouse_reports_and_utf8() {
        let handle = attachable_handle();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let state = std::sync::Arc::new(ExtraChannelState {
            attached: AtomicBool::new(false),
            buffer: Mutex::new(String::new()),
            replay: Mutex::new(TerminalReplayBuffer::default()),
            connected_at: Utc::now(),
            generation: handle.generation_now(),
        });
        handle
            .extra_channels
            .lock()
            .insert("ch-test".into(), ExtraChannelEntry { tx, state });
        let mouse_report = [0x1b, b'[', b'M', 0xfe, 0x81, 0x20];

        handle
            .channel_input_bytes("ch-test", &mouse_report)
            .unwrap();
        assert!(
            matches!(rx.try_recv(), Ok(ExtraChannelCmd::Input(actual)) if actual == mouse_report.to_vec())
        );
        handle.channel_input("ch-test", "中文").unwrap();
        assert!(
            matches!(rx.try_recv(), Ok(ExtraChannelCmd::Input(actual)) if actual == "中文".as_bytes())
        );
    }

    #[test]
    fn replay_is_bounded_sequenced_and_incremental() {
        let mut replay = TerminalReplayBuffer::default();
        for index in 0..=TERMINAL_REPLAY_MAX_CHUNKS {
            replay.push(7, &format!("chunk-{index}"));
        }
        assert_eq!(replay.chunks.len(), TERMINAL_REPLAY_MAX_CHUNKS);
        let retained = replay.read(7, Some(0));
        assert!(retained.truncated);
        assert_eq!(retained.chunks.first().unwrap().seq, 2);
        assert_eq!(retained.next_seq, 1025);
        assert!(retained.chunks.iter().all(|chunk| chunk.generation == 7));
        assert!(replay.read(7, Some(retained.next_seq)).chunks.is_empty());

        let oversized = format!("{}中文尾", "x".repeat(TERMINAL_REPLAY_CAPACITY_BYTES));
        let chunk = replay.push(8, &oversized).unwrap();
        assert!(chunk.data.len() <= TERMINAL_REPLAY_CAPACITY_BYTES);
        assert!(chunk.data.is_char_boundary(0));
        assert!(chunk.data.ends_with("中文尾"));
        assert_eq!(chunk.seq, 1026);
    }

    #[test]
    fn utf8_decoder_preserves_multibyte_characters_across_ssh_messages() {
        let mut pending = Vec::new();
        assert_eq!(decode_utf8_stream(&mut pending, &[0xe4, 0xbd], false), "");
        assert_eq!(
            decode_utf8_stream(&mut pending, &[0xa0, b'!'], false),
            "你!"
        );
        assert!(pending.is_empty());
        assert_eq!(decode_utf8_stream(&mut pending, &[0xf0, 0x9f], true), "�");
        assert!(pending.is_empty());
    }

    #[test]
    fn read_observers_coexist_with_one_writer_without_blocking_human_input() {
        let state = AppState::new();
        let handle = attachable_handle();
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        *handle.io_tx.lock() = Some(tx);
        let reader_a = handle
            .begin_read_attach_with_client(&state, "observe", "client-a", 30)
            .unwrap();
        let reader_b = handle
            .begin_read_attach_with_client(&state, "observe", "client-b", 30)
            .unwrap();

        handle.send_input("pwd\n").unwrap();
        assert!(matches!(rx.try_recv(), Ok(IoCmd::Input(bytes)) if bytes == b"pwd\n"));
        assert!(handle.pending_human_input.lock().is_empty());
        handle
            .validate_attach_token(&reader_a, "client-a", "read")
            .unwrap();
        assert!(handle
            .validate_attach_token(&reader_a, "client-a", "exec")
            .is_err());
        assert!(handle
            .validate_attach_token(&reader_a, "client-a", "sftp")
            .is_err());
        assert!(handle
            .validate_attach_token(&reader_a, "client-b", "read")
            .is_err());

        let writer = handle
            .begin_write_attach_with_client(&state, "write", "client-w", 30, true, false)
            .unwrap();
        handle
            .validate_attach_token(&writer, "client-w", "read")
            .unwrap();
        handle
            .validate_attach_token(&writer, "client-w", "sftp")
            .unwrap();
        assert!(handle
            .begin_write_attach_with_client(&state, "other", "client-z", 30, false, false)
            .is_err());
        handle
            .validate_attach_token(&reader_b, "client-b", "read")
            .unwrap();

        handle
            .end_attach_with_client(&state, &reader_a, "client-a", "done")
            .unwrap();
        assert!(handle.attached.load(Ordering::Relaxed));
        handle
            .end_attach_with_client(&state, &writer, "client-w", "done")
            .unwrap();
        handle
            .validate_attach_token(&reader_b, "client-b", "read")
            .unwrap();

        handle.close();
        assert!(handle.read_observers.lock().is_empty());
        assert!(handle.attach_token.lock().is_none());
    }

    #[test]
    fn read_observer_idle_expiry_does_not_detach_writer() {
        let state = AppState::new();
        state.inner.write().settings.idle_detach_minutes = 10;
        let handle = attachable_handle();
        let writer = handle
            .begin_attach_with_client(&state, "write", "client-w", 30)
            .unwrap();
        let reader = handle
            .begin_read_attach_with_client(&state, "observe", "client-r", 30)
            .unwrap();
        handle
            .read_observers
            .lock()
            .get_mut(&reader)
            .unwrap()
            .last_activity = Utc::now() - chrono::Duration::minutes(11);

        handle.expire_attach_if_needed(&state);
        assert!(handle.read_observers.lock().is_empty());
        handle
            .validate_attach_token(&writer, "client-w", "exec")
            .unwrap();
        assert!(handle
            .validate_attach_token(&writer, "client-w", "sftp")
            .is_err());
    }
}

fn spawn_latency_probe(
    session: &Arc<SessionHandle>,
    handle: &Arc<Handle>,
    keepalive_secs: u32,
    generation: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    if keepalive_secs == 0 {
        return None;
    }
    let session = session.clone();
    let handle = handle.clone();
    let interval = Duration::from_secs(u64::from(keepalive_secs));
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !session.connection_is_current(generation) {
                break;
            }
            let started = std::time::Instant::now();
            match tokio::time::timeout(Duration::from_secs(5), handle.channel_open_session()).await
            {
                Ok(Ok(channel)) => {
                    let millis = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    session.record_latency(generation, millis);
                    let _ = channel.close().await;
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }))
}

#[derive(Clone, Copy)]
struct LatencySample {
    generation: u64,
    millis: u64,
}

struct ReadObserver {
    client_id: String,
    expires_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    idle_timeout: Duration,
    binding: AttachBinding,
    /// true 表示该观察者使用 diag 模式，可在白名单内只读执行命令。
    diag: bool,
}

/// attach token 的实际角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    Writer,
    Diag,
    Read,
}

impl AttachRole {
    /// 与 MCP 返回值和审计日志一致的模式字符串。
    pub fn as_mode_str(self) -> &'static str {
        match self {
            Self::Writer => "write",
            Self::Diag => "diag",
            Self::Read => "read",
        }
    }
}

/// 任务级授权：任务内只读命令免逐条审批，写/高危/root 仍需单独确认。
#[derive(Debug, Clone)]
pub struct AttachTask {
    pub goal: String,
    pub expires_at: DateTime<Utc>,
}

fn connect_wait_timeout(cfg: &SessionConfig) -> Duration {
    let target_timeout = u64::from(cfg.connect_timeout_secs.max(1));
    if cfg.proxy_jump.is_empty() {
        return Duration::from_secs(target_timeout + 10);
    }

    let saved = crate::state::load_persisted_sessions();
    let Ok(mut route) = crate::state::resolve_proxy_chain(cfg, &saved) else {
        // 图校验错误会由实际建连路径立即返回，这里只需保留接收错误的基本窗口。
        return Duration::from_secs(target_timeout + 10);
    };
    route.push(cfg.clone());
    let seconds = route
        .iter()
        .enumerate()
        .fold(10_u64, |total, (index, hop)| {
            // 第一跳只需 TCP/SSH 握手；后续跳还包含 direct-tcpip 打开阶段。
            let phases = if index == 0 { 1 } else { 2 };
            total.saturating_add(phases * u64::from(hop.connect_timeout_secs.max(1)))
        });
    Duration::from_secs(seconds)
}

/// 单个 SSH 连接上允许同时打开的 SFTP channel 上限。
/// 常见服务器（如 OpenSSH 默认 MaxSessions=10）会拒绝超额的 channel，
/// 这里预留主 shell、监控、复制渠道等通道，把 SFTP 并发限制在 4。
pub const SFTP_MAX_CONCURRENT_CHANNELS: usize = 4;

/// 会话运行时句柄（共享状态用 Arc 包裹）
pub struct SessionHandle {
    pub config: RwLock<SessionConfig>,
    pub connected: AtomicBool,
    generation: AtomicU64,
    latency_ms: Mutex<Option<LatencySample>>,
    worker_running: AtomicBool,
    /// 建连尝试的单调编号：超时或新请求到来后，旧线程不再改写共享状态。
    connect_epoch: AtomicU64,
    close_requested: AtomicBool,
    pub attached: AtomicBool,
    pub attach_token: Mutex<Option<String>>,
    pub attach_since: Mutex<Option<DateTime<Utc>>>,
    attach_client_id: Mutex<Option<String>>,
    attach_expires_at: Mutex<Option<DateTime<Utc>>>,
    attach_last_activity: Mutex<Option<DateTime<Utc>>>,
    attach_idle_timeout: Mutex<Duration>,
    attach_permissions: Mutex<Vec<String>>,
    /// 本次 attach 已消耗的 sudo 执行次数
    sudo_used: AtomicU32,
    /// 连续 sudo 密码错误次数
    sudo_failures: Mutex<u32>,
    /// 密码错误锁定截止时间
    sudo_locked_until: Mutex<Option<DateTime<Utc>>>,
    /// attach 级 sudo 密码缓存（detach 即清除）
    sudo_password: Mutex<Option<String>>,
    read_observers: Mutex<HashMap<String, ReadObserver>>,
    replay: Mutex<TerminalReplayBuffer>,
    /// AI 独立回放：与人工终端、复制渠道互不影响。
    ai_replay: Mutex<TerminalReplayBuffer>,
    /// 主 Shell 连接建立成功的时刻
    primary_connected_at: Mutex<Option<DateTime<Utc>>>,
    /// 用户当前正在看的终端（由前端上报），revision 单调递增
    active_terminal: Mutex<ActiveTerminal>,
    /// writer 的固定连接绑定
    attach_binding: Mutex<Option<AttachBinding>>,
    /// writer 本次接入声明的任务目标
    attach_task: Mutex<Option<AttachTask>>,
    pub security_engine: SecurityEngine,
    pub pending_human_input: Mutex<VecDeque<u8>>,
    pub io_tx: Mutex<Option<tokio_mpsc::UnboundedSender<IoCmd>>>,
    ssh_handle: Mutex<Option<Arc<Handle>>>,
    sftp_permits: Arc<Semaphore>,
    extra_channels: std::sync::Arc<Mutex<HashMap<String, ExtraChannelEntry>>>,
    extra_channel_seq: AtomicU64,
}

impl SessionHandle {
    pub fn new(
        config: SessionConfig,
        global_blocklist: Vec<String>,
        confirm_list: Vec<String>,
    ) -> Self {
        let engine = SecurityEngine::new(global_blocklist, confirm_list, config.ai_policy);
        engine.update_session(config.ai_policy, &config.ai_blocklist_regexes);
        Self {
            config: RwLock::new(config),
            connected: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            latency_ms: Mutex::new(None),
            worker_running: AtomicBool::new(false),
            connect_epoch: AtomicU64::new(0),
            close_requested: AtomicBool::new(false),
            attached: AtomicBool::new(false),
            attach_token: Mutex::new(None),
            attach_since: Mutex::new(None),
            attach_client_id: Mutex::new(None),
            attach_expires_at: Mutex::new(None),
            attach_last_activity: Mutex::new(None),
            attach_idle_timeout: Mutex::new(Duration::from_secs(10 * 60)),
            attach_permissions: Mutex::new(Vec::new()),
            sudo_used: AtomicU32::new(0),
            sudo_failures: Mutex::new(0),
            sudo_locked_until: Mutex::new(None),
            sudo_password: Mutex::new(None),
            read_observers: Mutex::new(HashMap::new()),
            replay: Mutex::new(TerminalReplayBuffer::default()),
            ai_replay: Mutex::new(TerminalReplayBuffer::default()),
            primary_connected_at: Mutex::new(None),
            active_terminal: Mutex::new(ActiveTerminal::default()),
            attach_binding: Mutex::new(None),
            attach_task: Mutex::new(None),
            security_engine: engine,
            pending_human_input: Mutex::new(VecDeque::new()),
            io_tx: Mutex::new(None),
            ssh_handle: Mutex::new(None),
            sftp_permits: Arc::new(Semaphore::new(SFTP_MAX_CONCURRENT_CHANNELS)),
            extra_channels: std::sync::Arc::new(Mutex::new(HashMap::new())),
            extra_channel_seq: AtomicU64::new(0),
        }
    }

    pub fn config_snapshot(&self) -> SessionConfig {
        self.config.read().clone()
    }

    /// 获取当前已认证的 SSH 连接，供 SFTP 等子系统复用，避免重复 TCP/SSH 建连。
    pub fn shared_ssh_handle(&self) -> Result<Arc<Handle>, String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("会话未连接".into());
        }
        self.ssh_handle
            .lock()
            .clone()
            .ok_or_else(|| "SSH 连接尚未就绪，请稍后重试".into())
    }

    /// 在单个 SSH 连接上申请一个 SFTP channel 名额，防止超额打开被服务器拒绝。
    pub async fn acquire_sftp_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        Arc::clone(&self.sftp_permits)
            .acquire_owned()
            .await
            .map_err(|_| "SFTP 并发控制已关闭".to_string())
    }

    #[cfg(test)]
    pub fn install_shared_ssh_handle_for_test(&self, handle: Arc<Handle>) {
        *self.ssh_handle.lock() = Some(handle);
    }

    /// 测试用：注入一个已带输出的复制渠道，避免真实 SSH 握手。
    #[cfg(test)]
    pub fn install_test_extra_channel(&self, channel_id: &str, output: &str) {
        let generation = self.generation.load(Ordering::Acquire);
        let state = Arc::new(ExtraChannelState {
            attached: AtomicBool::new(true),
            buffer: Mutex::new(String::new()),
            replay: Mutex::new(TerminalReplayBuffer::default()),
            connected_at: Utc::now(),
            generation,
        });
        state.replay.lock().push(generation, output);
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        self.extra_channels
            .lock()
            .insert(channel_id.to_string(), ExtraChannelEntry { tx, state });
    }

    /// 测试用：向主 Shell replay 写入一段输出。
    #[cfg(test)]
    pub fn push_test_primary_output(&self, output: &str) {
        let generation = self.generation.load(Ordering::Acquire);
        self.replay.lock().push(generation, output);
    }

    /// 测试用：向 AI 独立回放写入一段输出。
    #[cfg(test)]
    pub fn push_test_ai_output(&self, output: &str) {
        let generation = self.generation.load(Ordering::Acquire);
        self.ai_replay.lock().push(generation, output);
    }

    /// 测试用：模拟一次断线重连（generation 递增）。
    #[cfg(test)]
    pub fn simulate_reconnect_for_test(&self) {
        self.invalidate_connection();
        self.activate_connection(1);
    }

    // ---- 连接注册与定向读取 ----

    fn connection_id_for(generation: u64, channel_id: Option<&str>) -> String {
        match channel_id {
            Some(channel) => format!("{generation}:{channel}"),
            None => format!("{generation}:primary"),
        }
    }

    /// 当前连接代次；重连后会递增，旧 connection_id 因此自动失效。
    pub fn generation_now(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 前端上报“用户当前正在看的终端”。channel_id 为 None 表示主 Shell。
    /// 未知渠道回退到主 Shell，避免把不存在的渠道当成活跃连接。
    pub fn set_active_terminal(&self, channel_id: Option<&str>) -> u64 {
        let channel = match channel_id {
            Some(id) if self.extra_channels.lock().contains_key(id) => Some(id.to_string()),
            _ => None,
        };
        let mut active = self.active_terminal.lock();
        active.channel_id = channel;
        active.known = true;
        active.revision = active.revision.saturating_add(1);
        active.revision
    }

    pub fn active_revision(&self) -> u64 {
        self.active_terminal.lock().revision
    }

    /// 当前活跃连接 ID。单连接时无需前端上报即可确定；多连接且未上报时返回 None。
    pub fn active_connection_id(&self) -> Option<String> {
        let generation = self.generation.load(Ordering::Acquire);
        if !self.connected.load(Ordering::Acquire) {
            return None;
        }
        let channels = self
            .extra_channels
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        if channels.is_empty() {
            return Some(Self::connection_id_for(generation, None));
        }
        let active = self.active_terminal.lock();
        if !active.known {
            return None;
        }
        let channel = active
            .channel_id
            .as_deref()
            .filter(|id| channels.iter().any(|item| item.as_str() == *id));
        Some(Self::connection_id_for(generation, channel))
    }

    /// 列出当前可接入的全部终端连接（主 Shell 在前）。
    pub fn connection_descriptors(&self) -> Vec<ConnectionDescriptor> {
        let generation = self.generation.load(Ordering::Acquire);
        if !self.connected.load(Ordering::Acquire) {
            return Vec::new();
        }
        let config = self.config_snapshot();
        let active_id = self.active_connection_id();
        let primary_id = Self::connection_id_for(generation, None);
        let primary_connected_at = *self.primary_connected_at.lock();
        let extras = self
            .extra_channels
            .lock()
            .iter()
            .filter(|(_, entry)| entry.state.generation == generation)
            .map(|(id, entry)| (id.clone(), entry.state.connected_at, entry.state.generation))
            .collect::<Vec<_>>();

        let mut list = Vec::with_capacity(extras.len() + 1);
        list.push(ConnectionDescriptor {
            is_active: active_id.as_deref() == Some(primary_id.as_str()),
            connection_id: primary_id,
            kind: "primary",
            channel_id: None,
            generation,
            connected_at: primary_connected_at,
            login_user: config.username.clone(),
            prompt_user: None,
            prompt_user_source: "unavailable",
            prompt_observed_at: None,
            label: config.name.clone(),
        });
        for (channel, connected_at, channel_generation) in extras {
            let connection_id = Self::connection_id_for(channel_generation, Some(&channel));
            list.push(ConnectionDescriptor {
                is_active: active_id.as_deref() == Some(connection_id.as_str()),
                connection_id,
                kind: "extra",
                channel_id: Some(channel.clone()),
                generation: channel_generation,
                connected_at: Some(connected_at),
                login_user: config.username.clone(),
                prompt_user: None,
                prompt_user_source: "unavailable",
                prompt_observed_at: None,
                label: format!("{} [{}]", config.name, channel),
            });
        }
        list
    }

    /// 解析 connection_id，成功时返回其当前描述。
    pub fn resolve_connection(&self, connection_id: &str) -> Result<ConnectionDescriptor, String> {
        if let Some(found) = self
            .connection_descriptors()
            .into_iter()
            .find(|item| item.connection_id == connection_id)
        {
            return Ok(found);
        }
        if !self.connected.load(Ordering::Acquire) {
            return Err("CONNECTION_CLOSED".into());
        }
        // 同一渠道在重连后属于新 generation，旧 token 不能读取新连接。
        if let Some((generation, _)) = connection_id.split_once(':') {
            match generation.parse::<u64>() {
                Ok(value) if value != self.generation.load(Ordering::Acquire) => {
                    return Err("CONNECTION_CHANGED".into())
                }
                _ => {}
            }
        }
        Err("CONNECTION_CLOSED".into())
    }

    /// 按连接读取 replay，after_seq 只在所属连接内有效。
    pub fn read_connection(
        &self,
        connection_id: &str,
        after_seq: Option<u64>,
    ) -> Result<(ConnectionDescriptor, TerminalReplay), String> {
        let descriptor = self.resolve_connection(connection_id)?;
        let replay = match descriptor.channel_id.as_deref() {
            None => self.replay.lock().read(descriptor.generation, after_seq),
            Some(channel) => {
                let entry = self
                    .extra_channels
                    .lock()
                    .get(channel)
                    .cloned()
                    .ok_or("CONNECTION_CLOSED")?;
                let channel_state = std::sync::Arc::clone(&entry.state);
                let replay = channel_state
                    .replay
                    .lock()
                    .read(descriptor.generation, after_seq);
                replay
            }
        };
        Ok((descriptor, replay))
    }

    /// 取 token 对应的不可变连接绑定。
    pub fn attach_binding(&self, token: &str, client_id: &str) -> Result<AttachBinding, String> {
        if self.attach_token.lock().as_deref() == Some(token) {
            if self.attach_client_id.lock().as_deref() != Some(client_id) {
                return Err("token 与当前客户端或会话不匹配".into());
            }
            if let Some(binding) = self.attach_binding.lock().clone() {
                return Ok(binding);
            }
        }
        let observers = self.read_observers.lock();
        let observer = observers.get(token).ok_or("token 无效或已过期")?;
        if observer.client_id != client_id {
            return Err("token 与当前客户端或会话不匹配".into());
        }
        Ok(observer.binding.clone())
    }

    /// AI 当前绑定的连接；writer 优先，其次是只读/diag 观察者，最后回退主 Shell 标识。
    pub fn ai_connection_id(&self) -> String {
        if let Some(binding) = self.attach_binding.lock().clone() {
            return binding.connection_id;
        }
        if let Some(connection_id) = self
            .read_observers
            .lock()
            .values()
            .next()
            .map(|observer| observer.binding.connection_id.clone())
        {
            return connection_id;
        }
        let generation = self.generation.load(Ordering::Acquire);
        Self::connection_id_for(generation, None)
    }

    /// 读取 AI 独立回放（只读标签晚挂载时补齐历史）。
    pub fn read_ai_terminal(&self, after_seq: Option<u64>) -> TerminalReplay {
        let generation = self.generation.load(Ordering::Acquire);
        self.ai_replay.lock().read(generation, after_seq)
    }

    /// 向 AI 只读标签追加一条系统提示（不进入人工终端）。
    fn append_ai_notice(&self, text: &str) {
        let generation = self.generation.load(Ordering::Acquire);
        let Some(chunk) = self.ai_replay.lock().push(generation, text) else {
            return;
        };
        let config = self.config_snapshot();
        crate::logging::append_ai_terminal_log(&config.id, &config.host, config.port, text);
        crate::state::emit_app_event(
            "ai-terminal-data",
            serde_json::json!({
                "session_id": config.id,
                "connection_id": self.ai_connection_id(),
                "generation": generation,
                "seq": chunk.seq,
                "data": text,
            }),
        );
    }

    /// 通知界面 AI 附着状态变化（用于创建/更新独立 AI 标签）。
    fn notify_ai_attach_changed(&self) {
        let attached = self.attached.load(Ordering::Relaxed);
        let readers = self.read_observers.lock();
        let mode = if attached {
            Some("write")
        } else if readers.values().any(|observer| observer.diag) {
            Some("diag")
        } else if !readers.is_empty() {
            Some("read")
        } else {
            None
        };
        let task = self.attach_task.lock().clone();
        drop(readers);
        crate::state::emit_app_event(
            "ai-attach-changed",
            serde_json::json!({
                "session_id": self.config_snapshot().id,
                "attached": attached,
                "mode": mode,
                "connection_id": self.attach_binding.lock().as_ref().map(|binding| binding.connection_id.clone()),
                "expires_at": *self.attach_expires_at.lock(),
                "readers": self.read_observers.lock().len(),
                "task": task.as_ref().map(|task| task.goal.clone()),
                "task_expires_at": task.map(|task| task.expires_at),
            }),
        );
    }

    pub fn refresh_session_security(&self) {
        let config = self.config_snapshot();
        self.security_engine
            .update_session(config.ai_policy, &config.ai_blocklist_regexes);
    }

    pub fn status(&self) -> SessionStatus {
        let connected = self.connected.load(Ordering::Acquire);
        let generation = self.generation.load(Ordering::Acquire);
        let latency_ms = if connected {
            self.latency_ms
                .lock()
                .and_then(|sample| (sample.generation == generation).then_some(sample.millis))
        } else {
            None
        };
        let attached = self.attached.load(Ordering::Relaxed);
        let readers = self.read_observers.lock().len() as u32;
        let has_diag = self
            .read_observers
            .lock()
            .values()
            .any(|observer| observer.diag);
        let mode = if attached {
            Some("write".to_string())
        } else if has_diag {
            Some("diag".to_string())
        } else if readers > 0 {
            Some("read".to_string())
        } else {
            None
        };
        let task = self.attach_task.lock().clone().filter(|_| attached);
        SessionStatus {
            connected,
            latency_ms,
            ai_attached: attached,
            ai_attached_at: *self.attach_since.lock(),
            ai_connection_id: if attached || readers > 0 {
                Some(self.ai_connection_id())
            } else {
                None
            },
            ai_mode: mode,
            ai_expires_at: *self.attach_expires_at.lock(),
            ai_sudo: attached
                && self
                    .attach_permissions
                    .lock()
                    .iter()
                    .any(|item| item == "sudo"),
            ai_readers: readers,
            ai_task: task.as_ref().map(|task| task.goal.clone()),
            ai_task_expires_at: task.map(|task| task.expires_at),
        }
    }

    fn activate_connection(&self, latency_ms: u64) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.latency_ms.lock() = Some(LatencySample {
            generation,
            millis: latency_ms,
        });
        *self.primary_connected_at.lock() = Some(Utc::now());
        self.reset_active_terminal();
        self.connected.store(true, Ordering::Release);
        generation
    }

    fn invalidate_connection(&self) -> u64 {
        self.connected.store(false, Ordering::Release);
        *self.latency_ms.lock() = None;
        *self.primary_connected_at.lock() = None;
        self.reset_active_terminal();
        // 复制渠道跟随 SSH transport 一起失效，避免残留旧 generation 的连接描述。
        self.extra_channels.lock().clear();
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 连接代次变化后，活跃终端回到未知状态，等待前端重新上报。
    fn reset_active_terminal(&self) {
        let mut active = self.active_terminal.lock();
        active.channel_id = None;
        active.known = false;
        active.revision = active.revision.saturating_add(1);
    }

    fn connection_is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
            && self.connected.load(Ordering::Acquire)
    }

    /// 建连线程是否仍属于最新一次连接尝试。
    fn connect_is_current(&self, epoch: u64) -> bool {
        self.connect_epoch.load(Ordering::Acquire) == epoch
    }

    fn record_latency(&self, generation: u64, millis: u64) {
        if !self.connection_is_current(generation) {
            return;
        }
        *self.latency_ms.lock() = Some(LatencySample { generation, millis });
    }

    /// 建立连接并启动会话线程（同步等待建连结果）
    pub fn connect(self: Arc<Self>, state: AppState, app: tauri::AppHandle) -> Result<(), String> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.worker_running.swap(true, Ordering::AcqRel) {
            return Err("会话正在连接中，请稍后再试".into());
        }
        self.close_requested.store(false, Ordering::Release);
        let connect_epoch = self.connect_epoch.fetch_add(1, Ordering::AcqRel) + 1;

        let cfg = self.config_snapshot();
        let session_id = cfg.id.clone();
        let wait_timeout = connect_wait_timeout(&cfg);
        let retention_days = state.inner.read().settings.log_retention_days;
        let first_host_key = crate::host_key::has_known_host(&cfg.host, cfg.port)
            .map(|known| !known)
            .unwrap_or(false);
        if cfg.log_enabled {
            let file = crate::logging::start_session_log_with_retention(
                &cfg.id,
                &cfg.host,
                cfg.port,
                retention_days,
            );
            crate::logging::log_event(
                &cfg.id,
                "INFO",
                &format!(
                    "--- 连接 {}@{}:{}（会话「{}」，日志文件 {file}）---",
                    cfg.username, cfg.host, cfg.port, cfg.name
                ),
            );
        }
        let (tx, rx) = tokio_mpsc::unbounded_channel::<IoCmd>();
        *self.io_tx.lock() = Some(tx);

        let (res_tx, res_rx) = std_mpsc::channel::<Result<(), String>>();
        let me = self.clone();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log_info(&cfg, &format!("ERROR 初始化运行时失败: {e}"));
                    crate::logging::end_session_log(&cfg.id, "初始化运行时失败");
                    if me.connect_is_current(connect_epoch) {
                        *me.io_tx.lock() = None;
                        me.worker_running.store(false, Ordering::Release);
                    }
                    let _ = res_tx.send(Err(format!("初始化运行时失败: {e}")));
                    return;
                }
            };

            rt.block_on(async move {
                let mut established = match open_shell(&cfg).await {
                    Ok(connection) => connection,
                    Err(error) => {
                        crate::logging::end_session_log(&cfg.id, "连接失败");
                        if me.connect_is_current(connect_epoch) {
                            *me.io_tx.lock() = None;
                            me.worker_running.store(false, Ordering::Release);
                        }
                        let _ = res_tx.send(Err(error));
                        return;
                    }
                };
                if !me.connect_is_current(connect_epoch) {
                    // 建连期间已超时或被新的连接请求取代：丢弃结果，保持状态与真实连接一致。
                    crate::logging::end_session_log(&cfg.id, "连接已取消");
                    return;
                }
                let mut res_tx = Some(res_tx);
                let mut rx = rx;
                let end_reason;

                'connections: loop {
                    let EstablishedConnection {
                        handle,
                        mut shell,
                        latency_ms,
                    } = established;
                    *me.ssh_handle.lock() = Some(Arc::clone(&handle));
                    let generation = me.activate_connection(latency_ms);
                    emit_for_generation(
                        &me,
                        generation,
                        &app,
                        "session-status",
                        serde_json::json!({
                            "session_id": cfg.id,
                            "connected": true,
                            "host": cfg.host,
                            "port": cfg.port,
                            "host_key_first_seen": first_host_key,
                        }),
                    );
                    if let Some(reply) = res_tx.take() {
                        let _ = reply.send(Ok(()));
                    }

                    let latency_task =
                        spawn_latency_probe(&me, &handle, cfg.keepalive_secs, generation);
                    let mut exec_tasks = tokio::task::JoinSet::new();
                    let mut utf8_pending = Vec::new();
                    let (reason, user_closed) = 'io: loop {
                        tokio::select! {
                            cmd = rx.recv() => {
                                match cmd {
                                    None | Some(IoCmd::Close) => {
                                        let _ = shell.eof().await;
                                        break 'io ("用户主动断开".to_string(), true);
                                    }
                                    Some(IoCmd::Input(bytes)) => {
                                        if let Err(error) = shell.data(&bytes[..]).await {
                                            break 'io (format!("写入 SSH shell 失败: {error}"), false);
                                        }
                                    }
                                    Some(IoCmd::Resize(cols, rows)) => {
                                        if let Err(error) = shell.window_change(cols as u32, rows as u32, 0, 0).await {
                                            break 'io (format!("调整 SSH 终端大小失败: {error}"), false);
                                        }
                                    }
                                    Some(IoCmd::Exec { cmd, timeout, reply }) => {
                                        // MCP exec 使用独立 channel；这里仅回显到主终端，不写入交互 PTY。
                                        emit_ai_text(&me, generation, &app, &cfg.id, format_ai_exec_start(&cmd));
                                        let exec_handle = handle.clone();
                                        let exec_session = me.clone();
                                        let exec_app = app.clone();
                                        let exec_session_id = cfg.id.clone();
                                        exec_tasks.spawn(async move {
                                            let started = std::time::Instant::now();
                                            let result = exec_collect(&exec_handle, &cmd, timeout).await;
                                            if exec_session.connection_is_current(generation) {
                                                let echo = format_ai_exec_result(
                                                    &cmd,
                                                    result.0,
                                                    &result.1,
                                                    &result.2,
                                                    started.elapsed().as_millis(),
                                                );
                                                emit_ai_text(
                                                    &exec_session,
                                                    generation,
                                                    &exec_app,
                                                    &exec_session_id,
                                                    echo,
                                                );
                                                let _ = reply.send(result);
                                            }
                                        });
                                    }
                                    Some(IoCmd::ExecRoot { cmd, cwd, timeout, password, reply }) => {
                                        // root 执行同样使用独立 exec channel，绝不写入交互 PTY。
                                        emit_ai_text(&me, generation, &app, &cfg.id, format_ai_exec_start_root(&cmd));
                                        let exec_handle = handle.clone();
                                        let exec_session = me.clone();
                                        let exec_app = app.clone();
                                        let exec_session_id = cfg.id.clone();
                                        exec_tasks.spawn(async move {
                                            let started = std::time::Instant::now();
                                            let result = exec_collect_root(
                                                &exec_handle,
                                                &cmd,
                                                &cwd,
                                                timeout,
                                                password.as_deref(),
                                            )
                                            .await;
                                            if exec_session.connection_is_current(generation) {
                                                let (code, stdout, stderr) = match &result {
                                                    Ok((code, out, err)) => (*code, out.as_str(), err.as_str()),
                                                    Err(error) => (-1, "", error.as_str()),
                                                };
                                                let echo = format_ai_exec_result_root(
                                                    &cmd,
                                                    code,
                                                    stdout,
                                                    stderr,
                                                    started.elapsed().as_millis(),
                                                );
                                                emit_ai_text(
                                                    &exec_session,
                                                    generation,
                                                    &exec_app,
                                                    &exec_session_id,
                                                    echo,
                                                );
                                                let _ = reply.send(result);
                                            }
                                        });
                                    }
                                }
                            }
                            msg = shell.wait() => {
                                match msg {
                                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                                        let text = decode_utf8_stream(&mut utf8_pending, &data, false);
                                        if cfg.log_enabled {
                                            crate::logging::append_session_log(&cfg.id, &text);
                                        }
                                        // 人工 Shell 输出必须走 terminal-data；AI 回显才走 emit_ai_text。
                                        emit_terminal_text(&me, generation, &app, &cfg.id, text);
                                    }
                                    Some(ChannelMsg::Eof) => {
                                        let text = decode_utf8_stream(&mut utf8_pending, &[], true);
                                        if cfg.log_enabled {
                                            crate::logging::append_session_log(&cfg.id, &text);
                                        }
                                        emit_terminal_text(&me, generation, &app, &cfg.id, text);
                                        break 'io ("远端关闭连接(EOF)".to_string(), false);
                                    }
                                    Some(ChannelMsg::Close) => {
                                        let text = decode_utf8_stream(&mut utf8_pending, &[], true);
                                        if cfg.log_enabled {
                                            crate::logging::append_session_log(&cfg.id, &text);
                                        }
                                        emit_terminal_text(&me, generation, &app, &cfg.id, text);
                                        break 'io ("远端关闭连接(CLOSE)".to_string(), false);
                                    }
                                    None => {
                                        let text = decode_utf8_stream(&mut utf8_pending, &[], true);
                                        if cfg.log_enabled {
                                            crate::logging::append_session_log(&cfg.id, &text);
                                        }
                                        emit_terminal_text(&me, generation, &app, &cfg.id, text);
                                        break 'io ("远端关闭连接(None)".to_string(), false);
                                    }
                                    _ => {}
                                }
                            }
                            Some(_) = exec_tasks.join_next(), if !exec_tasks.is_empty() => {}
                        }
                    };

                    if let Some(task) = latency_task {
                        task.abort();
                    }
                    exec_tasks.abort_all();
                    while exec_tasks.join_next().await.is_some() {}
                    let disconnected_generation = me.invalidate_connection();
                    *me.ssh_handle.lock() = None;
                    me.extra_channels.lock().clear();
                    me.clear_all_attachments();
                    me.pending_human_input.lock().clear();
                    crate::approval::cancel_session(&state, &cfg.id, "会话已断开，未决审批已取消");

                    if user_closed || me.close_requested.load(Ordering::Acquire) {
                        end_reason = "用户主动断开".into();
                        break 'connections;
                    }
                    let (auto_reconnect, max_attempts) = {
                        let settings = &state.inner.read().settings;
                        (settings.auto_reconnect, settings.reconnect_max_attempts)
                    };
                    if !auto_reconnect || max_attempts == 0 {
                        end_reason = reason;
                        break 'connections;
                    }

                    log_info(&cfg, &format!("连接中断：{reason}，准备自动重连"));
                    emit_for_generation(
                        &me,
                        disconnected_generation,
                        &app,
                        "session-status",
                        serde_json::json!({
                            "session_id": cfg.id,
                            "connected": false,
                            "reconnecting": true,
                        }),
                    );
                    match reconnect_connection(
                        &cfg,
                        &mut rx,
                        max_attempts,
                        disconnected_generation,
                    )
                    .await
                    {
                        ReconnectResult::Connected(connection) => {
                            established = connection;
                        }
                        ReconnectResult::Closed => {
                            end_reason = "用户主动断开".into();
                            break 'connections;
                        }
                        ReconnectResult::Failed(error) => {
                            end_reason = error;
                            break 'connections;
                        }
                    }
                }

                log_info(&cfg, &format!("连接结束：{end_reason}"));
                crate::logging::end_session_log(&cfg.id, &end_reason);
                if me.connect_is_current(connect_epoch) {
                    *me.io_tx.lock() = None;
                    me.worker_running.store(false, Ordering::Release);
                }
                let generation = me.generation.load(Ordering::Acquire);
                emit_for_generation(
                    &me,
                    generation,
                    &app,
                    "session-status",
                    serde_json::json!({
                        "session_id": cfg.id,
                        "connected": false,
                        "reconnecting": false,
                    }),
                );
            });
        });

        match res_rx.recv_timeout(wait_timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                *self.io_tx.lock() = None;
                Err(e)
            }
            Err(_) => {
                // 建连超时：取消仍在进行的尝试并复位状态，避免下次重连被静默忽略。
                self.close_requested.store(true, Ordering::Release);
                self.connect_epoch.fetch_add(1, Ordering::AcqRel);
                let _ = self.io_tx.lock().take();
                self.worker_running.store(false, Ordering::Release);
                crate::logging::end_session_log(&session_id, "连接超时");
                Err("连接超时".into())
            }
        }
    }

    pub fn send_input(&self, data: &str) -> Result<(), String> {
        self.send_input_bytes(data.as_bytes())
    }

    pub fn send_input_bytes(&self, data: &[u8]) -> Result<(), String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("会话未连接".into());
        }
        if self.attached.load(Ordering::Relaxed) {
            // AI 占用期间，人类输入进入缓冲，detach 后回放；满载时明确背压，绝不静默丢旧数据。
            let mut q = self.pending_human_input.lock();
            if q.len().saturating_add(data.len()) > 512 {
                return Err("AI 接入期间的人工输入缓冲已满，请先等待 AI 断开或手动踢出 AI".into());
            }
            q.extend(data.iter().copied());
            return Ok(());
        }
        let tx = self.io_tx.lock().clone().ok_or("会话未连接")?;
        tx.send(IoCmd::Input(data.to_vec()))
            .map_err(|_| "会话线程已退出".to_string())
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(tx) = self.io_tx.lock().clone() {
            let _ = tx.send(IoCmd::Resize(cols, rows));
        }
    }

    /// 在同一 SSH 连接上复制一个独立 shell 渠道（类似 Xshell 的“复制 SSH 渠道”）。
    pub async fn open_extra_channel(self: &Arc<Self>) -> Result<String, String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("会话未连接，无法复制 SSH 渠道".into());
        }
        let handle = self
            .ssh_handle
            .lock()
            .clone()
            .ok_or("SSH 连接尚未就绪，请稍后重试")?;
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|error| format!("打开 SSH 渠道失败: {error}"))?;
        channel
            .request_pty(true, "xterm-256color", 120, 40, 0, 0, &[])
            .await
            .map_err(|error| format!("请求 PTY 失败: {error}"))?;
        channel
            .request_shell(true)
            .await
            .map_err(|error| format!("启动远程 shell 失败: {error}"))?;

        let channel_id = format!(
            "ch{}",
            self.extra_channel_seq.fetch_add(1, Ordering::AcqRel) + 1
        );
        let generation = self.generation.load(Ordering::Acquire);
        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<ExtraChannelCmd>();
        let state = std::sync::Arc::new(ExtraChannelState {
            attached: AtomicBool::new(false),
            buffer: Mutex::new(String::new()),
            replay: Mutex::new(TerminalReplayBuffer::default()),
            connected_at: Utc::now(),
            generation,
        });
        self.extra_channels.lock().insert(
            channel_id.clone(),
            ExtraChannelEntry {
                tx,
                state: std::sync::Arc::clone(&state),
            },
        );

        let me = Arc::clone(self);
        let session_id = self.config_snapshot().id;
        let log_enabled = self.config_snapshot().log_enabled;
        let task_channel_id = channel_id.clone();
        tauri::async_runtime::spawn(async move {
            let mut channel = channel;
            let mut utf8_pending = Vec::new();
            let push = |text: &str| {
                if text.is_empty() {
                    return;
                }
                // MCP 回放与前端挂载缓存互不影响：无论终端是否挂载都持续积累。
                state.replay.lock().push(generation, text);
                if state.attached.load(Ordering::Acquire) {
                    crate::state::emit_app_event(
                        "session-channel-output",
                        serde_json::json!({
                            "session_id": session_id,
                            "channel_id": task_channel_id,
                            "data": text,
                        }),
                    );
                } else {
                    let mut buffer = state.buffer.lock();
                    let max = 64 * 1024;
                    if buffer.len() + text.len() > max {
                        let excess = (buffer.len() + text.len() - max).min(buffer.len());
                        buffer.drain(..excess);
                    }
                    buffer.push_str(text);
                }
            };
            loop {
                tokio::select! {
                    cmd = rx.recv() => match cmd {
                        None | Some(ExtraChannelCmd::Close) => {
                            let _ = channel.eof().await;
                            break;
                        }
                        Some(ExtraChannelCmd::Input(bytes)) => {
                            if channel.data(&bytes[..]).await.is_err() {
                                break;
                            }
                        }
                        Some(ExtraChannelCmd::Resize(cols, rows)) => {
                            if channel.window_change(cols as u32, rows as u32, 0, 0).await.is_err() {
                                break;
                            }
                        }
                    },
                    msg = channel.wait() => match msg {
                        Some(ChannelMsg::Data { data })
                        | Some(ChannelMsg::ExtendedData { data, .. }) => {
                            let text = decode_utf8_stream(&mut utf8_pending, &data, false);
                            if log_enabled {
                                crate::logging::append_session_log(&session_id, &text);
                            }
                            push(&text);
                        }
                        Some(ChannelMsg::Eof)
                        | Some(ChannelMsg::Close)
                        | None => {
                            let text = decode_utf8_stream(&mut utf8_pending, &[], true);
                            if log_enabled {
                                crate::logging::append_session_log(&session_id, &text);
                            }
                            push(&text);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            me.extra_channels.lock().remove(&task_channel_id);
            me.drop_active_channel(&task_channel_id);
            crate::state::emit_app_event(
                "session-channel-closed",
                serde_json::json!({
                    "session_id": session_id,
                    "channel_id": task_channel_id,
                }),
            );
        });
        Ok(channel_id)
    }

    /// 复制渠道关闭后，如果它正是活跃终端，则回退到主 Shell 并提升 revision。
    fn drop_active_channel(&self, channel_id: &str) {
        let mut active = self.active_terminal.lock();
        if active.channel_id.as_deref() == Some(channel_id) {
            active.channel_id = None;
            active.known = true;
            active.revision = active.revision.saturating_add(1);
        }
    }

    /// 终端挂载后接管渠道输出：返回挂载前缓存的文本。
    pub fn attach_extra_channel(&self, channel_id: &str) -> Result<String, String> {
        let entry = self
            .extra_channels
            .lock()
            .get(channel_id)
            .cloned()
            .ok_or("SSH 渠道不存在或已关闭")?;
        entry.state.attached.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *entry.state.buffer.lock());
        Ok(pending)
    }

    pub fn channel_input(&self, channel_id: &str, data: &str) -> Result<(), String> {
        self.channel_input_bytes(channel_id, data.as_bytes())
    }

    pub fn channel_input_bytes(&self, channel_id: &str, data: &[u8]) -> Result<(), String> {
        let entry = self
            .extra_channels
            .lock()
            .get(channel_id)
            .cloned()
            .ok_or("SSH 渠道不存在或已关闭")?;
        entry
            .tx
            .send(ExtraChannelCmd::Input(data.to_vec()))
            .map_err(|_| "SSH 渠道已关闭".to_string())
    }

    pub fn channel_resize(&self, channel_id: &str, cols: u16, rows: u16) {
        if let Some(entry) = self.extra_channels.lock().get(channel_id).cloned() {
            let _ = entry.tx.send(ExtraChannelCmd::Resize(cols, rows));
        }
    }

    pub fn close_extra_channel(&self, channel_id: &str) {
        if let Some(entry) = self.extra_channels.lock().remove(channel_id) {
            let _ = entry.tx.send(ExtraChannelCmd::Close);
        }
        self.drop_active_channel(channel_id);
    }

    pub fn exec_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<(i32, String, String), String> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("会话未连接".into());
        }
        let tx = self.io_tx.lock().clone().ok_or("会话未连接")?;
        let (rtx, rrx) = std_mpsc::channel();
        tx.send(IoCmd::Exec {
            cmd: command.to_string(),
            timeout,
            reply: rtx,
        })
        .map_err(|_| "会话线程已退出".to_string())?;
        rrx.recv_timeout(timeout + Duration::from_secs(5))
            .map_err(|_| "命令执行超时".to_string())
    }

    /// 以 root 执行一条命令（方案 B）。密码只经 exec 通道 stdin 传入。
    pub fn exec_as_root_with_timeout(
        &self,
        command: &str,
        cwd: &str,
        timeout: Duration,
        password: Option<String>,
    ) -> Result<(i32, String, String), String> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("会话未连接".into());
        }
        let tx = self.io_tx.lock().clone().ok_or("会话未连接")?;
        let (rtx, rrx) = std_mpsc::channel();
        tx.send(IoCmd::ExecRoot {
            cmd: command.to_string(),
            cwd: cwd.to_string(),
            timeout,
            password,
            reply: rtx,
        })
        .map_err(|_| "会话线程已退出".to_string())?;
        rrx.recv_timeout(timeout + Duration::from_secs(5))
            .map_err(|_| "命令执行超时".to_string())?
    }

    pub fn close(&self) {
        self.close_requested.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.connected.store(false, Ordering::Release);
        *self.latency_ms.lock() = None;
        self.clear_all_attachments();
        self.pending_human_input.lock().clear();
        if let Some(tx) = self.io_tx.lock().clone() {
            let _ = tx.send(IoCmd::Close);
        }
    }

    fn clear_attach_metadata(&self, held: &mut Option<String>) {
        *held = None;
        *self.attach_binding.lock() = None;
        *self.attach_task.lock() = None;
        *self.attach_since.lock() = None;
        *self.attach_client_id.lock() = None;
        *self.attach_expires_at.lock() = None;
        *self.attach_last_activity.lock() = None;
        self.attach_permissions.lock().clear();
        self.sudo_used.store(0, Ordering::Relaxed);
        *self.sudo_failures.lock() = 0;
        *self.sudo_locked_until.lock() = None;
        *self.sudo_password.lock() = None;
        self.attached.store(false, Ordering::Relaxed);
    }

    fn clear_all_attachments(&self) {
        let mut held = self.attach_token.lock();
        self.clear_attach_metadata(&mut held);
        drop(held);
        self.read_observers.lock().clear();
        self.notify_ai_attach_changed();
    }

    fn replay_pending_input(&self) {
        let buffered: Vec<u8> = self.pending_human_input.lock().drain(..).collect();
        if !buffered.is_empty() {
            if let Some(tx) = self.io_tx.lock().clone() {
                let _ = tx.send(IoCmd::Input(buffered));
            }
        }
    }

    fn attach_expiration_reason(&self, now: DateTime<Utc>) -> Option<String> {
        if self
            .attach_task
            .lock()
            .as_ref()
            .is_some_and(|task| task.expires_at <= now)
        {
            return Some("任务已结束（到期），已自动 detach".into());
        }
        if self
            .attach_expires_at
            .lock()
            .is_some_and(|expires_at| expires_at <= now)
        {
            return Some("attach token 已到期，自动断开".into());
        }
        let idle_timeout = *self.attach_idle_timeout.lock();
        let last_activity = *self.attach_last_activity.lock();
        if let Some(last_activity) = last_activity {
            let idle_limit = chrono::Duration::from_std(idle_timeout)
                .unwrap_or_else(|_| chrono::Duration::minutes(10));
            if now.signed_duration_since(last_activity) >= idle_limit {
                return Some("AI 空闲超时，已自动断开并回放人工输入".into());
            }
        }
        None
    }

    fn observer_expiration_reason(observer: &ReadObserver, now: DateTime<Utc>) -> Option<String> {
        if observer.expires_at <= now {
            return Some("read token 已到期，自动断开".into());
        }
        let idle_limit = chrono::Duration::from_std(observer.idle_timeout)
            .unwrap_or_else(|_| chrono::Duration::minutes(10));
        if now.signed_duration_since(observer.last_activity) >= idle_limit {
            return Some("只读观察者空闲超时，已自动断开".into());
        }
        None
    }

    fn ensure_attach_allowed(&self) -> Result<SessionConfig, String> {
        let config = self.config_snapshot();
        if !config.ai_allowed {
            return Err("该会话已禁用 AI 接入".into());
        }
        if !self.connected.load(Ordering::Relaxed) {
            return Err("会话未连接".into());
        }
        Ok(config)
    }

    fn attach_times(
        state: &AppState,
        ttl_minutes: u32,
        now: DateTime<Utc>,
    ) -> (DateTime<Utc>, Duration) {
        let idle_minutes = state.inner.read().settings.idle_detach_minutes;
        (
            now + chrono::Duration::minutes(i64::from(ttl_minutes.clamp(1, 1440))),
            Duration::from_secs(u64::from(idle_minutes.clamp(1, 1440)) * 60),
        )
    }

    #[cfg(test)]
    pub fn begin_attach_with_client(
        &self,
        state: &AppState,
        reason: &str,
        client_id: &str,
        ttl_minutes: u32,
    ) -> Result<String, String> {
        self.begin_write_attach_with_client(state, reason, client_id, ttl_minutes, false, false)
    }

    #[cfg(test)]
    pub fn begin_write_attach_with_client(
        &self,
        state: &AppState,
        reason: &str,
        client_id: &str,
        ttl_minutes: u32,
        sftp: bool,
        sudo: bool,
    ) -> Result<String, String> {
        let binding = self.default_binding();
        self.begin_write_attach_bound(state, reason, client_id, ttl_minutes, sftp, sudo, binding)
    }

    /// 兼容单连接旧行为：未显式选择时绑定主 Shell（仅测试使用）。
    #[cfg(test)]
    fn default_binding(&self) -> AttachBinding {
        let generation = self.generation_now();
        AttachBinding {
            connection_id: Self::connection_id_for(generation, None),
            generation,
            require_active: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_write_attach_bound(
        &self,
        state: &AppState,
        reason: &str,
        client_id: &str,
        ttl_minutes: u32,
        sftp: bool,
        sudo: bool,
        binding: AttachBinding,
    ) -> Result<String, String> {
        self.expire_attach_if_needed(state);
        let config = self.ensure_attach_allowed()?;
        self.resolve_connection(&binding.connection_id)?;
        let now = Utc::now();
        let (expires_at, idle_timeout) = Self::attach_times(state, ttl_minutes, now);
        let token = format!("tk_{}", Uuid::new_v4());
        let mut held = self.attach_token.lock();
        if self.attached.load(Ordering::Relaxed) || held.is_some() {
            return Err("已有 AI writer 接入中".into());
        }
        *held = Some(token.clone());
        *self.attach_binding.lock() = Some(binding);
        *self.attach_since.lock() = Some(now);
        *self.attach_client_id.lock() = Some(client_id.into());
        *self.attach_expires_at.lock() = Some(expires_at);
        *self.attach_last_activity.lock() = Some(now);
        *self.attach_idle_timeout.lock() = idle_timeout;
        *self.attach_permissions.lock() =
            vec!["exec".into(), "detach".into(), "read".into(), "sftp".into()];
        if !sftp {
            self.attach_permissions.lock().retain(|item| item != "sftp");
        }
        if sudo {
            self.attach_permissions.lock().push("sudo".into());
            self.sudo_used.store(0, Ordering::Relaxed);
            *self.sudo_failures.lock() = 0;
            *self.sudo_locked_until.lock() = None;
        }
        self.attached.store(true, Ordering::Relaxed);
        drop(held);

        state.log_ai_event_detailed(
            &config.id,
            AIEventKind::Attach {
                reason: reason.into(),
            },
            None,
            None,
            client_id,
            String::new(),
            None,
            None,
            Some(now),
        );
        self.notify_ai_attach_changed();
        Ok(token)
    }

    #[cfg(test)]
    pub fn begin_read_attach_with_client(
        &self,
        state: &AppState,
        reason: &str,
        client_id: &str,
        ttl_minutes: u32,
    ) -> Result<String, String> {
        let binding = self.default_binding();
        self.begin_read_attach_bound(state, reason, client_id, ttl_minutes, binding, false)
    }

    pub fn begin_read_attach_bound(
        &self,
        state: &AppState,
        reason: &str,
        client_id: &str,
        ttl_minutes: u32,
        binding: AttachBinding,
        diag: bool,
    ) -> Result<String, String> {
        self.expire_attach_if_needed(state);
        let config = self.ensure_attach_allowed()?;
        self.resolve_connection(&binding.connection_id)?;
        let now = Utc::now();
        let (expires_at, idle_timeout) = Self::attach_times(state, ttl_minutes, now);
        let token = format!("tk_{}", Uuid::new_v4());
        let mut observers = self.read_observers.lock();
        if observers.len() >= 32 {
            return Err("该会话的只读观察者数量已达上限".into());
        }
        observers.insert(
            token.clone(),
            ReadObserver {
                client_id: client_id.into(),
                expires_at,
                last_activity: now,
                idle_timeout,
                binding,
                diag,
            },
        );
        drop(observers);
        state.log_ai_event_detailed(
            &config.id,
            AIEventKind::Attach {
                reason: format!("[{}] {reason}", if diag { "diag" } else { "read" }),
            },
            None,
            None,
            client_id,
            String::new(),
            None,
            None,
            Some(now),
        );
        self.notify_ai_attach_changed();
        Ok(token)
    }

    /// attach 前置校验：失败时不应消耗人工审批单。
    pub fn precheck_write_attach(&self) -> Result<(), String> {
        self.ensure_attach_allowed()?;
        if self.attached.load(Ordering::Relaxed) || self.attach_token.lock().is_some() {
            return Err("已有 AI writer 接入中".into());
        }
        Ok(())
    }

    /// 只读 attach 前置校验：失败时不应消耗人工审批单。
    pub fn precheck_read_attach(&self) -> Result<(), String> {
        self.ensure_attach_allowed()?;
        if self.read_observers.lock().len() >= 32 {
            return Err("该会话的只读观察者数量已达上限".into());
        }
        Ok(())
    }

    /// diag attach 前置校验：与只读观察者一样非独占，失败时不消耗审批单。
    pub fn precheck_diag_attach(&self) -> Result<(), String> {
        self.precheck_read_attach()
    }

    /// 解析 token 的实际角色，并顺带刷新活跃时间。
    pub fn validate_attach_role(&self, token: &str, client_id: &str) -> Result<AttachRole, String> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("会话未连接".into());
        }
        let now = Utc::now();
        {
            let held = self.attach_token.lock();
            if self.attached.load(Ordering::Relaxed) && held.as_deref() == Some(token) {
                if self.attach_client_id.lock().as_deref() != Some(client_id) {
                    return Err("token 与当前客户端或会话不匹配".into());
                }
                if let Some(reason) = self.attach_expiration_reason(now) {
                    return Err(reason);
                }
                *self.attach_last_activity.lock() = Some(now);
                return Ok(AttachRole::Writer);
            }
        }

        let mut observers = self.read_observers.lock();
        let Some(observer) = observers.get_mut(token) else {
            return Err("token 无效或已过期".into());
        };
        if observer.client_id != client_id {
            return Err("token 与当前客户端或会话不匹配".into());
        }
        if let Some(reason) = Self::observer_expiration_reason(observer, now) {
            return Err(reason);
        }
        observer.last_activity = now;
        Ok(if observer.diag {
            AttachRole::Diag
        } else {
            AttachRole::Read
        })
    }

    /// 为当前的 writer attach 声明任务目标；有效期不会超过 attach token。
    pub fn set_attach_task(&self, goal: &str, ttl_minutes: u32) -> AttachTask {
        let now = Utc::now();
        let mut expires_at = now + chrono::Duration::minutes(i64::from(ttl_minutes.max(1)));
        if let Some(attach_expires_at) = *self.attach_expires_at.lock() {
            if attach_expires_at < expires_at {
                expires_at = attach_expires_at;
            }
        }
        let task = AttachTask {
            goal: goal.chars().take(200).collect(),
            expires_at,
        };
        *self.attach_task.lock() = Some(task.clone());
        self.notify_ai_attach_changed();
        task
    }

    pub fn attach_task_snapshot(&self) -> Option<AttachTask> {
        self.attach_task.lock().clone()
    }

    /// 测试用：立即让任务进入到期状态。
    #[cfg(test)]
    pub fn expire_attach_task_for_test(&self) {
        if let Some(task) = self.attach_task.lock().as_mut() {
            task.expires_at = Utc::now() - chrono::Duration::seconds(1);
        }
    }

    /// 查询 token 的真实接入角色（write / diag / read）。
    ///
    /// 不刷新活跃时间，也不因已到期而报错，供 detach 收尾时核对模式使用。
    pub fn attach_role_for_token(&self, token: &str, client_id: &str) -> Option<AttachRole> {
        {
            let held = self.attach_token.lock();
            if self.attached.load(Ordering::Relaxed)
                && held.as_deref() == Some(token)
                && self.attach_client_id.lock().as_deref() == Some(client_id)
            {
                return Some(AttachRole::Writer);
            }
        }
        self.read_observers
            .lock()
            .get(token)
            .filter(|observer| observer.client_id == client_id)
            .map(|observer| {
                if observer.diag {
                    AttachRole::Diag
                } else {
                    AttachRole::Read
                }
            })
    }

    pub fn validate_attach_token(
        &self,
        token: &str,
        client_id: &str,
        permission: &str,
    ) -> Result<(), String> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("会话未连接".into());
        }
        let now = Utc::now();
        let held = self.attach_token.lock();
        if self.attached.load(Ordering::Relaxed) && held.as_deref() == Some(token) {
            if self.attach_client_id.lock().as_deref() != Some(client_id) {
                return Err("token 与当前客户端或会话不匹配".into());
            }
            if !self
                .attach_permissions
                .lock()
                .iter()
                .any(|allowed| allowed == permission)
            {
                return Err(format!("token 没有 {permission} 权限"));
            }
            if let Some(reason) = self.attach_expiration_reason(now) {
                return Err(reason);
            }
            *self.attach_last_activity.lock() = Some(now);
            return Ok(());
        }
        drop(held);

        let mut observers = self.read_observers.lock();
        let Some(observer) = observers.get_mut(token) else {
            return Err("token 无效或已过期".into());
        };
        if observer.client_id != client_id {
            return Err("token 与当前客户端或会话不匹配".into());
        }
        let observer_permission = permission == "read"
            || permission == "detach"
            || (permission == "diag" && observer.diag);
        if !observer_permission {
            return Err(format!("token 没有 {permission} 权限"));
        }
        if let Some(reason) = Self::observer_expiration_reason(observer, now) {
            return Err(reason);
        }
        observer.last_activity = now;
        Ok(())
    }

    // ---- sudo（方案 B）----

    /// 本次 attach 已使用的 root 执行次数。
    pub fn sudo_quota_used(&self) -> u32 {
        self.sudo_used.load(Ordering::Relaxed)
    }

    /// 消耗一次配额，返回消耗后的已用次数。
    pub fn sudo_consume_quota(&self) -> u32 {
        self.sudo_used.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 若处于密码错误锁定中，返回剩余秒数。
    pub fn sudo_lockout_remaining_secs(&self) -> Option<u64> {
        let until = *self.sudo_locked_until.lock();
        until.and_then(|at| {
            let remaining = at.signed_duration_since(Utc::now()).num_seconds();
            (remaining > 0).then_some(remaining as u64)
        })
    }

    /// 记录一次密码失败；达到阈值时进入锁定并返回 true。
    pub fn sudo_record_failure(&self, max_failures: u32, lockout_seconds: u64) -> bool {
        let mut failures = self.sudo_failures.lock();
        *failures += 1;
        if *failures >= max_failures.max(1) {
            *self.sudo_locked_until.lock() =
                Some(Utc::now() + chrono::Duration::seconds(lockout_seconds.max(1) as i64));
            *failures = 0;
            true
        } else {
            false
        }
    }

    pub fn sudo_clear_failures(&self) {
        *self.sudo_failures.lock() = 0;
        *self.sudo_locked_until.lock() = None;
    }

    pub fn sudo_cached_password(&self) -> Option<String> {
        self.sudo_password.lock().clone()
    }

    pub fn sudo_store_password(&self, password: &str) {
        *self.sudo_password.lock() = Some(password.to_string());
    }

    pub fn sudo_forget_password(&self) {
        *self.sudo_password.lock() = None;
    }

    pub fn end_attach_with_client(
        &self,
        state: &AppState,
        token: &str,
        client_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        self.validate_attach_token(token, client_id, "detach")?;
        let role;
        let connection_id;
        let mut held = self.attach_token.lock();
        if held.as_deref() == Some(token) {
            if self.attach_client_id.lock().as_deref() != Some(client_id) {
                return Err("token 无效或已过期".into());
            }
            role = AttachRole::Writer;
            connection_id = self
                .attach_binding
                .lock()
                .as_ref()
                .map(|binding| binding.connection_id.clone())
                .unwrap_or_default();
            self.clear_attach_metadata(&mut held);
            drop(held);
            self.replay_pending_input();
        } else {
            drop(held);
            let mut observers = self.read_observers.lock();
            let Some(observer) = observers.get(token) else {
                return Err("token 无效或已过期".into());
            };
            if observer.client_id != client_id {
                return Err("token 无效或已过期".into());
            }
            role = if observer.diag {
                AttachRole::Diag
            } else {
                AttachRole::Read
            };
            connection_id = observer.binding.connection_id.clone();
            observers.remove(token);
        }
        state.log_ai_event_detailed(
            &self.config_snapshot().id,
            AIEventKind::Detach {
                reason: reason.into(),
            },
            None,
            None,
            client_id,
            format!("mode={} connection_id={connection_id}", role.as_mode_str()),
            None,
            None,
            None,
        );
        self.notify_ai_attach_changed();
        Ok(())
    }

    pub fn expire_attach_if_needed(&self, state: &AppState) {
        let now = Utc::now();
        let mut ended_task = None;
        let writer_reason = {
            let mut held = self.attach_token.lock();
            if held.is_none() {
                None
            } else if let Some(reason) = self.attach_expiration_reason(now) {
                if reason.starts_with("任务已结束") {
                    ended_task = self
                        .attach_task
                        .lock()
                        .as_ref()
                        .map(|task| task.goal.clone());
                }
                self.clear_attach_metadata(&mut held);
                Some(reason)
            } else {
                None
            }
        };
        let writer_expired = writer_reason.is_some();
        if let Some(reason) = writer_reason.clone() {
            if let Some(goal) = ended_task {
                self.append_ai_notice(&format!(
                    "\r\n\x1b[38;5;214m[AI 任务]\x1b[0m 任务「{}」已结束，已自动 detach。\r\n",
                    goal
                ));
            }
            self.replay_pending_input();
            state.log_ai_event_detailed(
                &self.config_snapshot().id,
                AIEventKind::Detach {
                    reason: reason.clone(),
                },
                None,
                None,
                "system",
                String::new(),
                None,
                Some(reason),
                None,
            );
        }

        let mut expired = Vec::new();
        self.read_observers.lock().retain(|_, observer| {
            if let Some(reason) = Self::observer_expiration_reason(observer, now) {
                expired.push((observer.client_id.clone(), reason));
                false
            } else {
                true
            }
        });
        let had_expired = !expired.is_empty();
        for (client_id, reason) in expired {
            state.log_ai_event_detailed(
                &self.config_snapshot().id,
                AIEventKind::Detach {
                    reason: reason.clone(),
                },
                None,
                None,
                &client_id,
                String::new(),
                None,
                Some(reason),
                None,
            );
        }
        if writer_expired || had_expired {
            self.notify_ai_attach_changed();
        }
    }

    /// 人类在 UI 上踢出 AI
    pub fn force_detach(&self, state: &AppState) {
        if !self.attached.load(Ordering::Relaxed)
            && self.attach_token.lock().is_none()
            && self.read_observers.lock().is_empty()
        {
            return;
        }
        self.clear_all_attachments();
        self.replay_pending_input();
        state.log_ai_event(
            &self.config_snapshot().id,
            AIEventKind::ForceDetach {
                reason: "用户手动踢出".into(),
            },
            None,
            None,
        );
    }
}
