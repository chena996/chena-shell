//! Application-wide shared state

use parking_lot::RwLock;
use std::sync::{Arc, OnceLock};
use tauri::Emitter;

static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

pub fn register_app_handle(app: &tauri::AppHandle) {
    let _ = APP_HANDLE.set(app.clone());
}

pub fn emit_app_event(event: &str, payload: serde_json::Value) {
    if let Some(app) = APP_HANDLE.get() {
        let _ = app.emit(event, payload);
    }
}

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::logging::EventRingBuffer;
use crate::security::SecurityPolicy;
use crate::session::SessionHandle;

/// Per-session configuration stored on disk / in memory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthConfig,
    #[serde(default)]
    pub proxy_jump: Vec<String>,
    pub group: String,
    pub ai_allowed: bool,
    pub ai_policy: AIPolicyOverride,
    pub ai_blocklist_regexes: Vec<String>,
    pub auto_connect: bool,
    pub keepalive_secs: u32,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u32,
    #[serde(default)]
    pub port_forwards: Vec<PortForwardConfig>,
    #[serde(default = "default_true")]
    pub log_enabled: bool,
}

/// 端口转发方向
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// 本地监听 → 经 SSH → 远端目标（本地转发）
    Local,
    /// 远端监听（服务器上） → 经 SSH → 本地目标（远端转发）
    Remote,
}

impl Default for ForwardKind {
    fn default() -> Self {
        Self::Local
    }
}

/// 端口转发规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForwardConfig {
    pub id: String,
    #[serde(default)]
    pub kind: ForwardKind,
    pub listen_host: String,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_connect_timeout_secs() -> u32 {
    30
}

fn default_remote_editor() -> String {
    "notepad".into()
}

fn default_remote_editor_mode() -> String {
    "builtin".into()
}

pub const MAX_PROXY_JUMPS: usize = 5;

/// 按声明顺序展开跳转链。被引用会话自己的 proxy_jump 会先展开，因此每跳仍只经前一跳连接。
pub fn resolve_proxy_chain(
    target: &SessionConfig,
    saved: &[SessionConfig],
) -> Result<Vec<SessionConfig>, String> {
    fn append_hop(
        id: &str,
        target: &SessionConfig,
        saved: &[SessionConfig],
        visiting: &mut Vec<String>,
        ordered: &mut Vec<SessionConfig>,
    ) -> Result<(), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("ProxyJump 会话 ID 不能为空".into());
        }
        if visiting.len() > MAX_PROXY_JUMPS {
            return Err(format!("展开后的 ProxyJump 链最多 {MAX_PROXY_JUMPS} 跳"));
        }
        if visiting.iter().any(|item| item == id) {
            let mut cycle = visiting.clone();
            cycle.push(id.into());
            return Err(format!("ProxyJump 存在循环引用: {}", cycle.join(" → ")));
        }
        if ordered.iter().any(|item| item.id == id) {
            return Err(format!("ProxyJump 会话重复: {id}"));
        }
        let config = if id == target.id {
            target
        } else {
            saved
                .iter()
                .find(|item| item.id == id)
                .ok_or_else(|| format!("ProxyJump 会话不存在: {id}"))?
        };
        if config.proxy_jump.len() > MAX_PROXY_JUMPS {
            return Err(format!(
                "会话 {} 的 ProxyJump 最多配置 {MAX_PROXY_JUMPS} 跳",
                config.id
            ));
        }

        visiting.push(id.into());
        for parent in &config.proxy_jump {
            append_hop(parent, target, saved, visiting, ordered)?;
        }
        visiting.pop();
        if ordered.len() >= MAX_PROXY_JUMPS {
            return Err(format!("展开后的 ProxyJump 链最多 {MAX_PROXY_JUMPS} 跳"));
        }
        ordered.push(config.clone());
        Ok(())
    }

    if target.proxy_jump.len() > MAX_PROXY_JUMPS {
        return Err(format!("ProxyJump 最多配置 {MAX_PROXY_JUMPS} 跳"));
    }
    let mut ordered = Vec::new();
    let mut visiting = vec![target.id.clone()];
    for id in &target.proxy_jump {
        append_hop(id, target, saved, &mut visiting, &mut ordered)?;
    }
    Ok(ordered)
}

/// 快速命令
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    #[serde(default)]
    pub group: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthConfig {
    Password {
        password_ref: String,
    },
    Key {
        private_key_path: String,
        /// 直接粘贴的私钥内容在凭据管理中的引用；与路径二选一。
        #[serde(default)]
        private_key_inline_ref: Option<String>,
        passphrase_ref: Option<String>,
    },
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AIPolicyOverride {
    Inherit,
    Strict,
    Recommended,
    Permissive,
}

/// update_session 的补丁参数：缺省字段保留原值，秘密只能通过非空值替换或显式清除。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionConfigPatch {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth: Option<AuthConfig>,
    pub proxy_jump: Option<Vec<String>>,
    pub group: Option<String>,
    pub ai_allowed: Option<bool>,
    pub ai_policy: Option<AIPolicyOverride>,
    pub ai_blocklist_regexes: Option<Vec<String>>,
    pub auto_connect: Option<bool>,
    pub keepalive_secs: Option<u32>,
    pub connect_timeout_secs: Option<u32>,
    pub port_forwards: Option<Vec<PortForwardConfig>>,
    pub log_enabled: Option<bool>,
    #[serde(default)]
    pub clear_secret: bool,
}

/// 前端/MCP 可见的认证信息，不包含密码或私钥口令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthDto {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    pub has_password: bool,
    pub has_passphrase: bool,
    pub has_inline_key: bool,
}

impl From<&AuthConfig> for AuthDto {
    fn from(auth: &AuthConfig) -> Self {
        match auth {
            AuthConfig::Password { password_ref } => Self {
                kind: "Password".into(),
                private_key_path: None,
                has_password: !password_ref.is_empty(),
                has_passphrase: false,
                has_inline_key: false,
            },
            AuthConfig::Key {
                private_key_path,
                private_key_inline_ref,
                passphrase_ref,
            } => Self {
                kind: "Key".into(),
                private_key_path: Some(private_key_path.clone()),
                has_password: false,
                has_passphrase: passphrase_ref.as_ref().is_some_and(|p| !p.is_empty()),
                has_inline_key: private_key_inline_ref
                    .as_ref()
                    .is_some_and(|p| !p.is_empty()),
            },
            AuthConfig::Agent => Self {
                kind: "Agent".into(),
                private_key_path: None,
                has_password: false,
                has_passphrase: false,
                has_inline_key: false,
            },
        }
    }
}

/// 会话配置的安全序列化视图。
#[derive(Debug, Clone, Serialize)]
pub struct PublicConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthDto,
    pub proxy_jump: Vec<String>,
    pub group: String,
    pub ai_allowed: bool,
    pub ai_policy: AIPolicyOverride,
    pub ai_blocklist_regexes: Vec<String>,
    pub auto_connect: bool,
    pub keepalive_secs: u32,
    pub connect_timeout_secs: u32,
    pub port_forwards: Vec<PortForwardConfig>,
    pub log_enabled: bool,
}

impl From<&SessionConfig> for PublicConfig {
    fn from(config: &SessionConfig) -> Self {
        Self {
            id: config.id.clone(),
            name: config.name.clone(),
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth: AuthDto::from(&config.auth),
            proxy_jump: config.proxy_jump.clone(),
            group: config.group.clone(),
            ai_allowed: config.ai_allowed,
            ai_policy: config.ai_policy,
            ai_blocklist_regexes: config.ai_blocklist_regexes.clone(),
            auto_connect: config.auto_connect,
            keepalive_secs: config.keepalive_secs,
            connect_timeout_secs: config.connect_timeout_secs,
            port_forwards: config.port_forwards.clone(),
            log_enabled: config.log_enabled,
        }
    }
}

/// Runtime session status (not persisted)
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub connected: bool,
    pub latency_ms: Option<u64>,
    pub ai_attached: bool,
    pub ai_attached_at: Option<DateTime<Utc>>,
    /// AI 绑定的终端连接；用于界面显示“AI 附着在哪块屏幕”。
    pub ai_connection_id: Option<String>,
    /// write / read / diag
    pub ai_mode: Option<String>,
    pub ai_expires_at: Option<DateTime<Utc>>,
    pub ai_sudo: bool,
    pub ai_readers: u32,
    /// 任务级授权的目标（write 模式 attach 时声明）。
    pub ai_task: Option<String>,
    pub ai_task_expires_at: Option<DateTime<Utc>>,
}

/// Tauri list_sessions 与 MCP session_list 共用的安全返回结构。
#[derive(Debug, Clone, Serialize)]
pub struct SessionDto {
    pub config: PublicConfig,
    pub status: SessionStatus,
}

impl SessionDto {
    pub fn new(config: &SessionConfig, status: SessionStatus) -> Self {
        Self {
            config: PublicConfig::from(config),
            status,
        }
    }
}

/// Global settings (persisted)
/// sudo 密码来源（方案 B）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SudoPasswordSource {
    /// 优先复用会话登录密码
    Session,
    /// 只允许用户在弹窗中手动输入
    Manual,
    /// 只支持服务器已配置 NOPASSWD 的免密 sudo
    Passwordless,
}

impl Default for SudoPasswordSource {
    fn default() -> Self {
        Self::Session
    }
}

/// sudo 密码缓存策略（方案 B）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SudoPasswordCache {
    /// 不缓存，每次都要输入
    None,
    /// 仅在本次 attach 内有效，detach 即清除
    Attach,
    /// 本次应用运行期间有效（按 主机:端口:用户名 记忆）
    App,
}

impl Default for SudoPasswordCache {
    fn default() -> Self {
        Self::Attach
    }
}

/// Global settings (persisted)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    pub default_policy: SecurityPolicy,
    /// 远端编辑使用的编辑器：默认 notepad；留空表示使用系统默认关联程序。
    #[serde(default = "default_remote_editor")]
    pub remote_editor: String,
    /// 远端编辑方式：builtin=内置编辑器，external=外部编辑器。
    #[serde(default = "default_remote_editor_mode")]
    pub remote_editor_mode: String,
    /// 内置编辑器是否对 shell 文件运行 ShellCheck。
    #[serde(default = "default_true")]
    pub shellcheck_enabled: bool,
    /// 是否允许按需从网络拉取 YAML/JSON schema。
    #[serde(default)]
    pub remote_schema_enabled: bool,
    pub global_blocklist: Vec<String>,
    pub confirm_list: Vec<String>,
    pub mcp_enabled: bool,
    pub mcp_host: String,
    pub mcp_port: u16,
    pub mcp_autostart: bool,
    #[serde(default = "default_attach_token_ttl_minutes")]
    pub attach_token_ttl_minutes: u32,
    #[serde(default = "default_true")]
    pub mcp_require_attach_approval: bool,
    pub idle_detach_minutes: u32,
    pub attach_token_visible: bool,
    pub theme: String,
    pub log_ai_commands: bool,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
    pub startup_auto_connect: bool,
    pub show_onboarding: bool,
    pub terminal_font_size: u32,
    pub copy_on_select: bool,
    pub confirm_multiline_paste: bool,
    pub links_enabled: bool,
    pub default_keepalive_secs: u32,
    pub auto_reconnect: bool,
    pub reconnect_max_attempts: u32,
    pub connect_timeout_secs: u32,
    pub terminal_log_default_enabled: bool,
    pub log_redaction_enabled: bool,
    // ---- AI sudo 提权（方案 B）----
    /// 总开关，默认关闭；关闭时 AI 申请 sudo 直接返回 SUDO_DISABLED
    #[serde(default)]
    pub sudo_enabled: bool,
    #[serde(default)]
    pub sudo_password_source: SudoPasswordSource,
    #[serde(default)]
    pub sudo_password_cache: SudoPasswordCache,
    /// root 命令是否逐条确认（默认开启）
    #[serde(default = "default_true")]
    pub sudo_require_confirm: bool,
    #[serde(default = "default_sudo_blocklist")]
    pub sudo_blocklist: Vec<String>,
    #[serde(default = "default_sudo_confirm_list")]
    pub sudo_confirm_list: Vec<String>,
    /// 单次 attach 允许的 sudo 执行次数上限
    #[serde(default = "default_sudo_quota_per_attach")]
    pub sudo_quota_per_attach: u32,
    /// 连续密码错误次数达到该值即锁定
    #[serde(default = "default_sudo_max_failures")]
    pub sudo_max_failures: u32,
    /// 锁定持续时间（秒）
    #[serde(default = "default_sudo_lockout_seconds")]
    pub sudo_lockout_seconds: u64,
    /// AI 执行回显是否同时写入人工终端（默认关闭，只进入独立 AI 只读标签）。
    #[serde(default)]
    pub ai_echo_to_terminal: bool,
    /// 是否允许 AI 以 diag 模式只读执行白名单命令。
    #[serde(default = "default_true")]
    pub ai_diag_enabled: bool,
    /// diag 模式与任务内只读自动放行共用的命令前缀白名单。
    #[serde(default = "default_diag_allowlist")]
    pub ai_diag_allowlist: Vec<String>,
    /// 任务级授权最长有效时长（分钟）。
    #[serde(default = "default_ai_task_max_minutes")]
    pub ai_task_max_minutes: u32,
}

fn default_sudo_blocklist() -> Vec<String> {
    vec![
        r"\bsudo\s+-i\b".into(),
        r"\bsudo\s+-s\b".into(),
        r"\bsudo\s+su\b".into(),
        r"\bvisudo\b".into(),
        r"/etc/sudoers".into(),
        r"\bpasswd\b".into(),
        r"\.ssh/authorized_keys".into(),
        r"\bchattr\b\s+\+i".into(),
        r"\bdd\b.*\bof=/dev/".into(),
        r"\brm\s+-rf\s+/\s*$".into(),
    ]
}

fn default_sudo_confirm_list() -> Vec<String> {
    vec![
        r"\brm\b".into(),
        r"\bsystemctl\s+(restart|stop|disable)".into(),
        r"\bkill\b|\bpkill\b|\bkillall\b".into(),
        r"\biptables\b|\bnft\b|\bufw\b".into(),
        r"\btee\b|\bsed\s+-i\b|>\s*/etc/".into(),
        r"\buseradd\b|\busermod\b|\buserdel\b".into(),
        r"\bchmod\b|\bchown\b".into(),
        r"\bmodprobe\b|\bsysctl\s+-w\b".into(),
    ]
}

/// 只读诊断的内置白名单。
///
/// 这里的每条都必须是「只读」命令，且不依赖参数就能保持只读：
/// 匹配器按前缀放行参数，所以不要把 `ip`、`mount`、`date`、`hostname` 这类
/// 带写子命令的裸命令名放进来。会话以 root 登录时也只放行本列表中的条目。
pub(crate) fn default_diag_allowlist() -> Vec<String> {
    vec![
        // 系统与资源概览
        "uptime".into(),
        "free".into(),
        "vmstat".into(),
        "iostat".into(),
        "mpstat".into(),
        "sar".into(),
        "nproc".into(),
        "lscpu".into(),
        "lsmem".into(),
        "uname".into(),
        "whoami".into(),
        "id".into(),
        "timedatectl status".into(),
        "timedatectl show".into(),
        // 进程
        "ps".into(),
        "pgrep".into(),
        "pidof".into(),
        "pstree".into(),
        "top -b -n 1".into(),
        // 磁盘与内存
        "df".into(),
        "du".into(),
        "lsblk".into(),
        "findmnt".into(),
        "swapon --show".into(),
        "blkid".into(),
        "lsmod".into(),
        "lsof".into(),
        "ls".into(),
        "stat".into(),
        "wc".into(),
        "readlink".into(),
        // 系统日志
        "journalctl".into(),
        "dmesg".into(),
        "crontab -l".into(),
        "last".into(),
        "lastb".into(),
        "w".into(),
        "who".into(),
        "getenforce".into(),
        "sestatus".into(),
        "systemd-analyze blame".into(),
        // 网络（只保留 show 子命令，避免 ip link set 之类的写操作）
        "ss".into(),
        "netstat".into(),
        "ip addr show".into(),
        "ip route show".into(),
        "ip link show".into(),
        "ip neigh show".into(),
        "getent".into(),
        "nslookup".into(),
        "dig".into(),
        "ping -c".into(),
        "traceroute".into(),
        "tracepath".into(),
        "curl -I".into(),
        // 服务与容器
        "systemctl status".into(),
        "systemctl show".into(),
        "systemctl cat".into(),
        "systemctl is-active".into(),
        "systemctl is-enabled".into(),
        "systemctl list-units".into(),
        "systemctl list-unit-files".into(),
        "systemctl list-timers".into(),
        "docker ps".into(),
        "docker images".into(),
        "docker logs --tail".into(),
        "docker inspect".into(),
        "docker stats --no-stream".into(),
        "docker version".into(),
        "docker info".into(),
        "kubectl get pods".into(),
        "kubectl get nodes".into(),
        "kubectl get svc".into(),
        "kubectl get deploy".into(),
        "kubectl get events".into(),
        "kubectl describe pod".into(),
        "kubectl describe node".into(),
        "kubectl describe deploy".into(),
        "kubectl describe svc".into(),
        "kubectl logs --tail".into(),
        "kubectl top".into(),
        "nginx -t".into(),
        // 包管理
        "apt list --installed".into(),
        "dpkg -l".into(),
        // 只读文件：具体文件，或受目录边界约束的目录前缀
        "cat /proc/meminfo".into(),
        "cat /proc/cpuinfo".into(),
        "cat /proc/loadavg".into(),
        "cat /proc/uptime".into(),
        "cat /proc/mounts".into(),
        "cat /proc/net/".into(),
        "cat /etc/os-release".into(),
        "cat /etc/hosts".into(),
        "cat /etc/resolv.conf".into(),
        "cat /etc/nginx/nginx.conf".into(),
        "cat /etc/nginx/conf.d/".into(),
        "cat /etc/nginx/sites-enabled/".into(),
        "cat /var/log/".into(),
        "tail /var/log/".into(),
    ]
}

fn default_ai_task_max_minutes() -> u32 {
    120
}

fn default_sudo_quota_per_attach() -> u32 {
    50
}

fn default_sudo_max_failures() -> u32 {
    3
}

fn default_sudo_lockout_seconds() -> u64 {
    300
}

/// MCP settings_get 的显式白名单。安全策略、凭证哈希与审批规则不得加入此 DTO。
#[derive(Debug, Clone, Serialize)]
pub struct SettingsDto {
    pub mcp_enabled: bool,
    pub mcp_host: String,
    pub mcp_port: u16,
    pub mcp_autostart: bool,
    pub attach_token_ttl_minutes: u32,
    pub idle_detach_minutes: u32,
    pub attach_token_visible: bool,
    pub theme: String,
    pub log_ai_commands: bool,
    pub log_retention_days: u32,
    pub startup_auto_connect: bool,
    pub show_onboarding: bool,
    pub terminal_font_size: u32,
    pub copy_on_select: bool,
    pub confirm_multiline_paste: bool,
    pub links_enabled: bool,
    pub default_keepalive_secs: u32,
    pub auto_reconnect: bool,
    pub reconnect_max_attempts: u32,
    pub connect_timeout_secs: u32,
    pub terminal_log_default_enabled: bool,
    pub log_redaction_enabled: bool,
    pub sudo_enabled: bool,
    pub sudo_require_confirm: bool,
    pub sudo_quota_per_attach: u32,
}

impl From<&GlobalSettings> for SettingsDto {
    fn from(settings: &GlobalSettings) -> Self {
        Self {
            mcp_enabled: settings.mcp_enabled,
            mcp_host: settings.mcp_host.clone(),
            mcp_port: settings.mcp_port,
            mcp_autostart: settings.mcp_autostart,
            attach_token_ttl_minutes: settings.attach_token_ttl_minutes,
            idle_detach_minutes: settings.idle_detach_minutes,
            attach_token_visible: settings.attach_token_visible,
            theme: settings.theme.clone(),
            log_ai_commands: settings.log_ai_commands,
            log_retention_days: settings.log_retention_days,
            startup_auto_connect: settings.startup_auto_connect,
            show_onboarding: settings.show_onboarding,
            terminal_font_size: settings.terminal_font_size,
            copy_on_select: settings.copy_on_select,
            confirm_multiline_paste: settings.confirm_multiline_paste,
            links_enabled: settings.links_enabled,
            default_keepalive_secs: settings.default_keepalive_secs,
            auto_reconnect: settings.auto_reconnect,
            reconnect_max_attempts: settings.reconnect_max_attempts,
            connect_timeout_secs: settings.connect_timeout_secs,
            terminal_log_default_enabled: settings.terminal_log_default_enabled,
            log_redaction_enabled: settings.log_redaction_enabled,
            sudo_enabled: settings.sudo_enabled,
            sudo_require_confirm: settings.sudo_require_confirm,
            sudo_quota_per_attach: settings.sudo_quota_per_attach,
        }
    }
}

fn default_log_retention_days() -> u32 {
    7
}

fn default_attach_token_ttl_minutes() -> u32 {
    30
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            default_policy: SecurityPolicy::Recommended,
            global_blocklist: vec![
                r"(?i)rm\s+-rf\s+/".into(),
                r"(?i)mkfs|fdisk\s|format\s".into(),
                r"(?i)shutdown|poweroff|halt".into(),
                r"(?i)chmod\s+000\s+/etc/passwd".into(),
            ],
            confirm_list: vec![
                r"(?i)systemctl\s+restart".into(),
                r"(?i)iptables|nft\s+add".into(),
                r"(?i)git\s+push\s+--force".into(),
            ],
            mcp_enabled: true,
            mcp_host: "127.0.0.1".into(),
            mcp_port: 8767,
            mcp_autostart: true,
            attach_token_ttl_minutes: default_attach_token_ttl_minutes(),
            mcp_require_attach_approval: true,
            idle_detach_minutes: 10,
            attach_token_visible: true,
            theme: "light".into(),
            log_ai_commands: true,
            log_retention_days: default_log_retention_days(),
            startup_auto_connect: true,
            show_onboarding: true,
            terminal_font_size: 14,
            copy_on_select: false,
            confirm_multiline_paste: true,
            links_enabled: true,
            default_keepalive_secs: 30,
            auto_reconnect: true,
            reconnect_max_attempts: 8,
            connect_timeout_secs: 30,
            terminal_log_default_enabled: true,
            log_redaction_enabled: true,
            remote_editor: default_remote_editor(),
            remote_editor_mode: default_remote_editor_mode(),
            shellcheck_enabled: true,
            remote_schema_enabled: false,
            sudo_enabled: false,
            sudo_password_source: SudoPasswordSource::default(),
            sudo_password_cache: SudoPasswordCache::default(),
            sudo_require_confirm: true,
            sudo_blocklist: default_sudo_blocklist(),
            sudo_confirm_list: default_sudo_confirm_list(),
            sudo_quota_per_attach: default_sudo_quota_per_attach(),
            sudo_max_failures: default_sudo_max_failures(),
            sudo_lockout_seconds: default_sudo_lockout_seconds(),
            ai_echo_to_terminal: false,
            ai_diag_enabled: true,
            ai_diag_allowlist: default_diag_allowlist(),
            ai_task_max_minutes: default_ai_task_max_minutes(),
        }
    }
}

/// AI activity event (shown in AI Activity tab + stored to log)
#[derive(Debug, Clone, Serialize)]
pub struct AIEvent {
    pub id: String,
    pub session_id: String,
    pub attach_token: Option<String>,
    pub ts: DateTime<Utc>,
    pub kind: AIEventKind,
    pub detail: String,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub source: String,
    pub rule: Option<String>,
    pub denial_reason: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum AIEventKind {
    Attach { reason: String },
    Detach { reason: String },
    Exec { command: String, args: String },
    Blocked { command: String, rule: String },
    WaitingConfirm { command: String, rule: String },
    Confirmed { command: String },
    Rejected { command: String, reason: String },
    Expired { command: String },
    ForceDetach { reason: String },
}

/// 传输方向
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    Upload,
    Download,
}

/// 传输状态
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferStatus {
    Running,
    Paused,
    Done,
    Error,
}

/// SFTP 传输任务（所有字段 clone + serialize 方便跨窗口）
#[derive(Debug, Clone, Serialize)]
pub struct TransferJob {
    pub id: String,
    pub session_id: String,
    pub direction: TransferDirection,
    pub local_path: String,
    pub remote_path: String,
    pub size: u64,
    pub bytes_transferred: u64,
    pub status: TransferStatus,
    pub error: Option<String>,
}

/// Shared app state — held inside Tauri's manage()
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<RwLock<InnerState>>,
}

pub struct InnerState {
    pub settings: GlobalSettings,
    pub sessions: DashMap<String, Arc<SessionHandle>>,
    pub ai_events: DashMap<String, EventRingBuffer<AIEvent>>, // keyed by session_id
    pub mcp_listening: bool,
    pub mcp_starting: bool,
    pub mcp_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    pub mcp_credential_hash: Option<String>,
    pub confirmations: DashMap<String, crate::approval::Confirmation>,
    pub monitor_samples: DashMap<String, crate::monitor::MonitorSnapshot>,
    /// 运行中的端口转发：forward_id → stop 标志
    pub forwards: DashMap<String, Arc<std::sync::atomic::AtomicBool>>,
    pub quick_commands: Vec<QuickCommand>,
    /// SFTP 传输队列：job_id → TransferJob
    pub transfers: DashMap<String, TransferJob>,
    /// SFTP 已打开窗口：session_id → window label
    pub sftp_windows: DashMap<String, String>,
    /// 应用级 sudo 密码缓存：host:port:username → 密码（仅内存，退出即失）
    pub sudo_app_passwords: DashMap<String, String>,
    /// 等待用户输入 sudo 密码的请求：request_id → 一次性回传通道
    pub sudo_prompts: DashMap<String, tokio::sync::oneshot::Sender<Option<String>>>,
}

impl AppState {
    /// 从磁盘恢复上次的会话列表（密码以明文存储，MVP 限制）
    pub fn restore_sessions(&self) {
        let list = load_persisted_sessions();
        if list.is_empty() {
            return;
        }
        let settings = self.inner.read().settings.clone();
        let inner = self.inner.write();
        for cfg in list {
            let handle = SessionHandle::new(
                cfg.clone(),
                settings.global_blocklist.clone(),
                settings.confirm_list.clone(),
            );
            inner.sessions.insert(cfg.id.clone(), Arc::new(handle));
        }
    }
}

/// 应用数据目录名（%APPDATA%\chena-shell）
pub const APP_DIR: &str = "chena-shell";

pub fn app_dir() -> std::path::PathBuf {
    #[cfg(any(test, feature = "mcp-harness"))]
    if let Some(path) = std::env::var_os("CHENA_SHELL_TEST_CONFIG_DIR") {
        return std::path::PathBuf::from(path).join(APP_DIR);
    }
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(APP_DIR)
}

/// 统一加密配置文件
/// 文件名刻意保持中性（data.0），避免被一眼看出存的是会话/密码相关配置
pub fn store_file() -> std::path::PathBuf {
    app_dir().join("data.0")
}

/// 界面偏好（主题、终端字号）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub theme: String,
    pub terminal_font_size: u32,
    pub copy_on_select: bool,
    pub confirm_multiline_paste: bool,
    pub links_enabled: bool,
    pub bottom_panel_height: u32,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            terminal_font_size: 14,
            copy_on_select: false,
            confirm_multiline_paste: true,
            links_enabled: true,
            bottom_panel_height: 280,
        }
    }
}

/// 单一加密存储：会话 / 快速命令 / 全局设置 / 界面偏好
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigStore {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub sessions: Vec<SessionConfig>,
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    /// 内置快速命令种子版本：仅升级时补齐新增种子，避免每次启动复活用户删除的旧项。
    #[serde(default)]
    pub quick_seed_version: u32,
    #[serde(default)]
    pub settings: GlobalSettings,
    #[serde(default)]
    pub ui: UiPrefs,
    /// 只保存带盐的 MCP 接入凭证哈希，绝不保存可登录的原始凭证。
    #[serde(default)]
    pub mcp_credential_hash: Option<String>,
}

/// 二进制加密：nonce(12) + ciphertext(+GCM tag)，纯二进制、无明文特征。
fn seal_with_key(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    use aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use rand::RngCore;

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|e| format!("加密失败: {e}"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn seal(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = crate::secret_store::load_or_create_master_key()?;
    seal_with_key(plaintext, &key)
}

fn unseal_with_key(blob: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, String> {
    use aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err("数据太短".into());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    cipher
        .decrypt(Nonce::from_slice(&blob[..NONCE_LEN]), &blob[NONCE_LEN..])
        .map_err(|_| "解密失败 — 密钥不匹配或文件被篡改".to_string())
}

fn unseal(blob: &[u8]) -> Result<Vec<u8>, String> {
    if let Ok(key) = crate::secret_store::load_or_create_master_key() {
        if let Ok(plaintext) = unseal_with_key(blob, &key) {
            return Ok(plaintext);
        }
    }
    // 旧 SID/用户名派生密钥只用于读取并一次性迁移既有 data.0。
    unseal_with_key(blob, &legacy_derive_key())
}

pub fn externalize_session_secrets(session: &mut SessionConfig) -> Result<bool, String> {
    let mut changed = false;
    match &mut session.auth {
        AuthConfig::Password { password_ref } => {
            if !password_ref.is_empty() && !crate::secret_store::is_reference(password_ref) {
                let raw = password_ref.clone();
                *password_ref =
                    crate::secret_store::store_session_secret(&session.id, "password", &raw)?;
                changed = true;
            }
        }
        AuthConfig::Key {
            private_key_inline_ref,
            passphrase_ref,
            ..
        } => {
            if let Some(secret) = private_key_inline_ref {
                if !secret.is_empty() && !crate::secret_store::is_reference(secret) {
                    let raw = secret.clone();
                    *secret = crate::secret_store::store_session_secret(
                        &session.id,
                        "private_key",
                        &raw,
                    )?;
                    changed = true;
                }
            }
            if let Some(secret) = passphrase_ref {
                if !secret.is_empty() && !crate::secret_store::is_reference(secret) {
                    let raw = secret.clone();
                    *secret =
                        crate::secret_store::store_session_secret(&session.id, "passphrase", &raw)?;
                    changed = true;
                }
            }
        }
        AuthConfig::Agent => {}
    }
    Ok(changed)
}

fn secret_references(session: &SessionConfig) -> std::collections::BTreeSet<String> {
    let values = match &session.auth {
        AuthConfig::Password { password_ref } => vec![password_ref],
        AuthConfig::Key {
            private_key_inline_ref,
            passphrase_ref,
            ..
        } => passphrase_ref
            .iter()
            .chain(private_key_inline_ref.iter())
            .collect(),
        AuthConfig::Agent => Vec::new(),
    };
    values
        .into_iter()
        .filter(|value| crate::secret_store::is_reference(value))
        .cloned()
        .collect()
}

pub fn cleanup_obsolete_session_secrets(
    previous: &SessionConfig,
    current: &SessionConfig,
) -> Result<(), String> {
    let retained = secret_references(current);
    for reference in secret_references(previous).difference(&retained) {
        crate::secret_store::delete_reference(reference)?;
    }
    Ok(())
}

pub fn cleanup_session_secrets(session: &SessionConfig) -> Result<(), String> {
    for reference in secret_references(session) {
        crate::secret_store::delete_reference(&reference)?;
    }
    Ok(())
}

fn externalize_store_secrets(st: &mut ConfigStore) -> Result<bool, String> {
    let mut changed = st.schema_version != 2;
    for session in &mut st.sessions {
        changed |= externalize_session_secrets(session)?;
    }
    st.schema_version = 2;
    Ok(changed)
}

fn write_store_atomic(
    path: &std::path::Path,
    st: &ConfigStore,
    backup_name: Option<&str>,
) -> Result<(), String> {
    use std::io::Write;

    let mut normalized = st.clone();
    externalize_store_secrets(&mut normalized)?;
    let plaintext = serde_json::to_vec(&normalized).map_err(|e| e.to_string())?;
    let blob = seal(&plaintext)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let temp = path.with_file_name(format!("data.0.{}.tmp", uuid::Uuid::new_v4()));
    {
        let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        file.write_all(&blob).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    // 新文件先解密回读，再备份并替换旧文件。
    let verified = unseal_with_key(
        &std::fs::read(&temp).map_err(|e| e.to_string())?,
        &crate::secret_store::load_or_create_master_key()?,
    )?;
    let _: ConfigStore = serde_json::from_slice(&verified)
        .map_err(|error| format!("新配置回读校验失败: {error}"))?;
    if let (Some(name), true) = (backup_name, path.exists()) {
        std::fs::copy(path, path.with_file_name(name)).map_err(|e| e.to_string())?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.to_string());
    }
    Ok(())
}

pub fn save_store(st: &ConfigStore) -> Result<(), String> {
    write_store_atomic(&store_file(), st, None)
}

/// 一次性日志迁移专用：同目录临时文件 + 原子替换，并保留替换前的密文。
fn write_store_atomic_preserving_previous(
    path: &std::path::Path,
    st: &ConfigStore,
) -> Result<(), String> {
    write_store_atomic(path, st, Some("data.0.pre-log-default-migration.bak"))
}

/// 读取存储；首次运行会把旧的 sessions.enc / sessions.json / quickcmds.json 迁移进来并删除旧文件
pub fn load_store() -> ConfigStore {
    let path = store_file();
    if let Ok(blob) = std::fs::read(&path) {
        if let Ok(plain) = unseal(&blob) {
            if let Ok(mut st) = serde_json::from_slice::<ConfigStore>(&plain) {
                if st.quick_commands.is_empty() {
                    st.quick_commands = default_quick_commands();
                    st.quick_seed_version = QUICK_SEED_VERSION;
                } else if st.quick_seed_version < QUICK_SEED_VERSION
                    && merge_builtin_quick_commands(&mut st)
                {
                    if let Err(error) =
                        write_store_atomic(&path, &st, Some("data.0.pre-quick-seed.bak"))
                    {
                        tracing::error!("内置快捷指令补齐保存失败: {error}");
                    }
                }
                if st.schema_version < 2 {
                    let mut migrated = st.clone();
                    match externalize_store_secrets(&mut migrated).and_then(|_| {
                        write_store_atomic(
                            &path,
                            &migrated,
                            Some("data.0.pre-secret-migration.bak"),
                        )
                    }) {
                        Ok(()) => {
                            SECRET_STORAGE_MIGRATION_PERFORMED
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            st = migrated;
                        }
                        Err(error) => {
                            tracing::error!("秘密存储迁移失败，保留旧加密文件: {error}");
                        }
                    }
                }
                return st;
            }
        }
    }

    // ---- 迁移旧格式 ----
    let mut st = ConfigStore::default();

    // 旧版：base64(nonce + ciphertext) 的 sessions.enc
    let legacy_enc = path.with_file_name("sessions.enc");
    if let Ok(text) = std::fs::read_to_string(&legacy_enc) {
        if let Ok(plain) = decrypt_to_plaintext(text.trim()) {
            if let Ok(list) = serde_json::from_str::<Vec<SessionConfig>>(&plain) {
                st.sessions = list;
            }
        }
    }
    // 更旧版：明文 sessions.json
    if st.sessions.is_empty() {
        if let Ok(text) = std::fs::read_to_string(path.with_file_name("sessions.json")) {
            if let Ok(list) = serde_json::from_str::<Vec<SessionConfig>>(&text) {
                st.sessions = list;
            }
        }
    }
    // 旧版：exe 同目录的明文 quickcmds.json
    let old_qc = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("quickcmds.json")));
    if let Some(p) = &old_qc {
        if let Ok(text) = std::fs::read_to_string(p) {
            if let Ok(list) = serde_json::from_str::<Vec<QuickCommand>>(&text) {
                st.quick_commands = list;
            }
        }
    }
    if st.quick_commands.is_empty() {
        st.quick_commands = default_quick_commands();
    }
    merge_builtin_quick_commands(&mut st);

    match save_store(&st) {
        Ok(()) => {
            // 只有新加密存储写入并回读成功后才清理旧文件。
            let _ = std::fs::remove_file(&legacy_enc);
            let _ = std::fs::remove_file(path.with_file_name("sessions.json"));
            if let Some(p) = &old_qc {
                let _ = std::fs::remove_file(p);
            }
        }
        Err(error) => tracing::error!("配置保存失败，保留旧文件: {error}"),
    }

    st
}

/// 读-改-写，保证各配置项互不覆盖（加锁避免并发写入丢更新）
fn mutate_store<F: FnOnce(&mut ConfigStore)>(f: F) -> Result<(), String> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut st = load_store();
    f(&mut st);
    save_store(&st)
}

pub fn load_ui_prefs() -> UiPrefs {
    load_store().ui
}

pub fn save_ui_prefs(prefs: UiPrefs) -> Result<(), String> {
    mutate_store(|st| st.ui = prefs)
}

pub fn load_settings() -> GlobalSettings {
    load_store().settings
}

/// 全局设置与界面偏好共享同一次原子写入，避免两步保存产生半提交状态。
pub fn save_settings_and_ui_prefs(
    settings: &GlobalSettings,
    ui_prefs: Option<&UiPrefs>,
) -> Result<(), String> {
    mutate_store(|store| {
        store.settings = settings.clone();
        if let Some(prefs) = ui_prefs {
            store.ui = prefs.clone();
        }
    })
}

pub fn load_mcp_credential_hash() -> Option<String> {
    load_store().mcp_credential_hash
}

pub fn save_mcp_credential_hash(hash: Option<String>) -> Result<(), String> {
    mutate_store(|st| st.mcp_credential_hash = hash)
}

/// 从当前 Windows 登录用户派生加密密钥
/// 方案: SHA256(SID + "\0" + 用户名 + "\0" + "chena")
fn legacy_derive_key() -> [u8; 32] {
    use sha2::{Digest, Sha256};

    // 获取当前用户名（env USERNAME 或 NAME）
    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("NAME"))
        .unwrap_or_else(|_| "unknown".into());

    // 尝试获取 Windows SID — 用 tasklist/whoami 都太笨，
    // 直接用 USERDOMAIN + USERNAME 组合（Windows 登录唯一标识）
    let domain = std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".into());
    let sid_fallback = format!("{}\\{}", domain, username);

    // 固定 salt（用户要求 "chena"）
    let salt = "chena";

    let mut hasher = Sha256::new();
    hasher.update(sid_fallback.as_bytes());
    hasher.update(b"\x00");
    hasher.update(username.as_bytes());
    hasher.update(b"\x00");
    hasher.update(salt.as_bytes());
    let hash = hasher.finalize();

    let mut key = [0u8; 32];
    key.copy_from_slice(&hash);
    key
}

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// 旧格式解密（base64(nonce+ciphertext)），仅用于迁移 sessions.enc

fn decrypt_to_plaintext(b64: &str) -> Result<String, String> {
    use aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use base64::Engine;

    let combined = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("base64 解码失败: {e}"))?;

    if combined.len() < NONCE_LEN + TAG_LEN {
        return Err("密文太短".into());
    }

    let key = legacy_derive_key();
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;

    let nonce = Nonce::from_slice(&combined[..NONCE_LEN]);
    let ciphertext = &combined[NONCE_LEN..];

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "解密失败 — 密钥不匹配或文件被篡改".to_string())?;

    String::from_utf8(plaintext).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RestoreSessionsStatus {
    pub log_default_migration_performed: bool,
    pub secret_storage_migration_performed: bool,
}

static LOG_DEFAULT_MIGRATION_PERFORMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static SECRET_STORAGE_MIGRATION_PERFORMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn sync_session_log_defaults(st: &mut ConfigStore) -> bool {
    if !st.settings.terminal_log_default_enabled {
        return false;
    }
    let mut changed = false;
    for session in &mut st.sessions {
        if !session.log_enabled {
            session.log_enabled = true;
            changed = true;
        }
    }
    changed
}

/// 供后续 UI 一次性读取本次恢复是否执行了日志默认值迁移。
pub fn take_restore_sessions_status() -> RestoreSessionsStatus {
    RestoreSessionsStatus {
        log_default_migration_performed: LOG_DEFAULT_MIGRATION_PERFORMED
            .swap(false, std::sync::atomic::Ordering::Relaxed),
        secret_storage_migration_performed: SECRET_STORAGE_MIGRATION_PERFORMED
            .swap(false, std::sync::atomic::Ordering::Relaxed),
    }
}

pub fn load_persisted_sessions() -> Vec<SessionConfig> {
    let mut st = load_store();
    if sync_session_log_defaults(&mut st) {
        match write_store_atomic_preserving_previous(&store_file(), &st) {
            Ok(()) => {
                LOG_DEFAULT_MIGRATION_PERFORMED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(error) => tracing::error!("终端日志默认状态同步保存失败: {error}"),
        }
    }
    st.sessions
}

pub fn save_persisted_sessions(list: &[SessionConfig]) -> Result<(), String> {
    mutate_store(|st| st.sessions = list.to_vec())
}

// ---------- 快速命令持久化（走统一加密存储） ----------

pub fn load_quick_commands() -> Vec<QuickCommand> {
    let list = load_store().quick_commands;
    if list.is_empty() {
        return default_quick_commands();
    }
    list
}

pub fn save_quick_commands(list: &[QuickCommand]) -> Result<(), String> {
    mutate_store(|st| st.quick_commands = list.to_vec())
}

fn default_quick_commands() -> Vec<QuickCommand> {
    let seeds = [
        ("磁盘使用", "df -h", "系统"),
        ("内存使用", "free -m", "系统"),
        ("系统负载", "uptime", "系统"),
        ("进程 TOP10", "ps aux --sort=-%cpu | head -11", "系统"),
        ("监听端口", "ss -tlnp", "网络"),
        ("最近登录", "last -n 10", "安全"),
        (
            "系统信息",
            "uname -a && cat /etc/os-release | head -5",
            "系统",
        ),
        ("CPU 信息", "lscpu | head -20", "系统"),
        ("内存 TOP10", "ps aux --sort=-%mem | head -11", "系统"),
        (
            "目录大小 TOP20",
            "du -sh * 2>/dev/null | sort -rh | head -20",
            "文件",
        ),
        ("磁盘 inode", "df -i", "文件"),
        ("网络连接", "ss -tnp", "网络"),
        ("网卡地址", "ip -brief addr", "网络"),
        ("路由表", "ip route", "网络"),
        ("当前用户", "who", "安全"),
        ("失败登录", "lastb -n 10 2>/dev/null", "安全"),
        ("异常服务", "systemctl --failed --no-pager", "服务"),
        ("最近日志", "journalctl -n 50 --no-pager", "日志"),
        ("内核日志", "dmesg | tail -30", "日志"),
        ("环境变量", "env | sort", "开发"),
    ];
    seeds
        .iter()
        .enumerate()
        .map(|(i, (label, cmd, group))| QuickCommand {
            id: format!("qc-{}", i + 1),
            label: label.to_string(),
            command: cmd.to_string(),
            group: group.to_string(),
        })
        .collect()
}

/// 内置快速命令种子版本，新增种子时递增。
const QUICK_SEED_VERSION: u32 = 2;

/// 增量补齐内置快速命令：只添加缺失的种子，不覆盖或重排用户已有项。
fn merge_builtin_quick_commands(st: &mut ConfigStore) -> bool {
    let mut changed = false;
    for seed in default_quick_commands() {
        if !st.quick_commands.iter().any(|item| item.id == seed.id) {
            st.quick_commands.push(seed);
            changed = true;
        }
    }
    if st.quick_seed_version != QUICK_SEED_VERSION {
        st.quick_seed_version = QUICK_SEED_VERSION;
        changed = true;
    }
    changed
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(InnerState {
                settings: load_settings(),
                sessions: DashMap::new(),
                ai_events: DashMap::new(),
                mcp_listening: false,
                mcp_starting: false,
                mcp_shutdown: None,
                mcp_credential_hash: load_mcp_credential_hash(),
                confirmations: DashMap::new(),
                monitor_samples: DashMap::new(),
                forwards: DashMap::new(),
                quick_commands: load_quick_commands(),
                transfers: DashMap::new(),
                sftp_windows: DashMap::new(),
                sudo_app_passwords: DashMap::new(),
                sudo_prompts: DashMap::new(),
            })),
        }
    }
}

impl AppState {
    pub fn log_ai_event(
        &self,
        session_id: &str,
        kind: AIEventKind,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
    ) {
        self.log_ai_event_detailed(
            session_id,
            kind,
            exit_code,
            duration_ms,
            "mcp",
            String::new(),
            None,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_ai_event_detailed(
        &self,
        session_id: &str,
        kind: AIEventKind,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        source: &str,
        detail: String,
        rule: Option<String>,
        denial_reason: Option<String>,
        started_at: Option<DateTime<Utc>>,
    ) {
        let event = AIEvent {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            attach_token: None,
            ts: Utc::now(),
            kind,
            detail,
            duration_ms,
            exit_code,
            source: source.into(),
            rule,
            denial_reason,
            started_at,
            finished_at: Some(Utc::now()),
        };
        let event_id = event.id.clone();
        let audit_line = format_ai_event_line(&event);
        {
            let inner = self.inner.write();
            inner
                .ai_events
                .entry(session_id.into())
                .or_insert_with(|| EventRingBuffer::new(1000))
                .push(event);
        }
        emit_app_event(
            "ai-event",
            serde_json::json!({"session_id": session_id, "event_id": event_id}),
        );
        if !cfg!(test) {
            let _ = crate::logging::append_daily_log("ai", "ai-audit", &audit_line);
        }
    }

    pub fn audit_command(&self, command: &str) -> String {
        if self.inner.read().settings.log_ai_commands {
            crate::logging::redact_text(command)
        } else {
            "[命令审计已关闭]".into()
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn ai_event_action(kind: &AIEventKind) -> String {
    match kind {
        AIEventKind::Attach { reason } => format!("ATTACH reason={}", single_line(reason)),
        AIEventKind::Detach { reason } => format!("DETACH reason={}", single_line(reason)),
        AIEventKind::Exec { command, args } => format!(
            "EXEC command={} args={}",
            single_line(command),
            single_line(args)
        ),
        AIEventKind::Blocked { command, rule } => format!(
            "BLOCKED command={} rule={}",
            single_line(command),
            single_line(rule)
        ),
        AIEventKind::WaitingConfirm { command, rule } => format!(
            "WAITING_CONFIRM command={} rule={}",
            single_line(command),
            single_line(rule)
        ),
        AIEventKind::Confirmed { command } => {
            format!("CONFIRMED command={}", single_line(command))
        }
        AIEventKind::Rejected { command, reason } => format!(
            "REJECTED command={} reason={}",
            single_line(command),
            single_line(reason)
        ),
        AIEventKind::Expired { command } => format!("EXPIRED command={}", single_line(command)),
        AIEventKind::ForceDetach { reason } => {
            format!("FORCE_DETACH reason={}", single_line(reason))
        }
    }
}

/// 将一条 AI 事件格式化为单行审计日志；所有字段统一脱敏。
pub fn format_ai_event_line(event: &AIEvent) -> String {
    let mut line = format!(
        "{} [AI] session={} source={} action={}",
        event
            .ts
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S%.3f"),
        single_line(&event.session_id),
        single_line(&event.source),
        ai_event_action(&event.kind)
    );
    if let Some(exit_code) = event.exit_code {
        line.push_str(&format!(" exit={exit_code}"));
    }
    if let Some(duration_ms) = event.duration_ms {
        line.push_str(&format!(" duration={duration_ms}ms"));
    }
    if let Some(rule) = &event.rule {
        line.push_str(&format!(" rule={}", single_line(rule)));
    }
    if let Some(reason) = &event.denial_reason {
        line.push_str(&format!(" denial={}", single_line(reason)));
    }
    if !event.detail.is_empty() {
        line.push_str(&format!(" detail={}", single_line(&event.detail)));
    }
    crate::logging::redact_text(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, log_enabled: bool) -> SessionConfig {
        SessionConfig {
            id: id.into(),
            name: id.into(),
            host: "127.0.0.1".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig::Agent,
            proxy_jump: Vec::new(),
            group: String::new(),
            ai_allowed: false,
            ai_policy: AIPolicyOverride::Inherit,
            ai_blocklist_regexes: Vec::new(),
            auto_connect: false,
            keepalive_secs: 30,
            connect_timeout_secs: 30,
            port_forwards: Vec::new(),
            log_enabled,
        }
    }

    #[test]
    fn settings_dto_serializes_only_the_explicit_safe_whitelist() {
        let settings = GlobalSettings {
            default_policy: SecurityPolicy::Permissive,
            global_blocklist: vec!["BLOCKLIST_SENTINEL_MUST_NOT_LEAK".into()],
            confirm_list: vec!["CONFIRM_RULE_SENTINEL_MUST_NOT_LEAK".into()],
            mcp_require_attach_approval: false,
            ..Default::default()
        };
        let value = serde_json::to_value(SettingsDto::from(&settings)).unwrap();
        let object = value.as_object().unwrap();
        let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
        actual.sort_unstable();
        let mut expected = vec![
            "mcp_enabled",
            "mcp_host",
            "mcp_port",
            "mcp_autostart",
            "attach_token_ttl_minutes",
            "idle_detach_minutes",
            "attach_token_visible",
            "theme",
            "log_ai_commands",
            "log_retention_days",
            "startup_auto_connect",
            "show_onboarding",
            "terminal_font_size",
            "copy_on_select",
            "confirm_multiline_paste",
            "links_enabled",
            "default_keepalive_secs",
            "auto_reconnect",
            "reconnect_max_attempts",
            "connect_timeout_secs",
            "terminal_log_default_enabled",
            "log_redaction_enabled",
            "sudo_enabled",
            "sudo_require_confirm",
            "sudo_quota_per_attach",
        ];
        expected.sort_unstable();
        assert_eq!(actual, expected);

        for forbidden in [
            "default_policy",
            "global_blocklist",
            "confirm_list",
            "mcp_require_attach_approval",
            "mcp_credential_hash",
            "sudo_blocklist",
            "sudo_confirm_list",
            "sudo_password_source",
            "sudo_password_cache",
        ] {
            assert!(!object.contains_key(forbidden));
        }
        let serialized = value.to_string();
        assert!(!serialized.contains("BLOCKLIST_SENTINEL_MUST_NOT_LEAK"));
        assert!(!serialized.contains("CONFIRM_RULE_SENTINEL_MUST_NOT_LEAK"));
    }

    #[test]
    fn log_default_sync_enables_sessions_without_logging_only_when_default_on() {
        let mut st = ConfigStore {
            sessions: vec![session("enabled", true), session("disabled", false)],
            ..Default::default()
        };
        assert!(st.settings.terminal_log_default_enabled);
        assert!(sync_session_log_defaults(&mut st));
        assert!(st.sessions.iter().all(|session| session.log_enabled));
        assert!(!sync_session_log_defaults(&mut st));

        let mut settings = GlobalSettings::default();
        settings.terminal_log_default_enabled = false;
        let mut off = ConfigStore {
            sessions: vec![session("disabled", false)],
            settings,
            ..Default::default()
        };
        assert!(!sync_session_log_defaults(&mut off));
        assert!(!off.sessions[0].log_enabled);
    }

    #[test]
    fn log_default_sync_keeps_explicitly_enabled_session_when_default_off() {
        let mut settings = GlobalSettings::default();
        settings.terminal_log_default_enabled = false;
        let mut st = ConfigStore {
            sessions: vec![session("enabled", true)],
            settings,
            ..Default::default()
        };

        assert!(!sync_session_log_defaults(&mut st));
        assert!(st.sessions[0].log_enabled);
    }

    #[test]
    fn missing_log_flags_and_retention_use_safe_defaults() {
        let mut config = serde_json::to_value(session("s", true)).unwrap();
        config.as_object_mut().unwrap().remove("log_enabled");
        let config: SessionConfig = serde_json::from_value(config).unwrap();
        assert!(config.log_enabled);

        let mut settings = serde_json::to_value(GlobalSettings::default()).unwrap();
        settings
            .as_object_mut()
            .unwrap()
            .remove("log_retention_days");
        let settings: GlobalSettings = serde_json::from_value(settings).unwrap();
        assert_eq!(settings.log_retention_days, 7);
    }

    #[test]
    fn proxy_jump_defaults_to_empty_for_legacy_configs() {
        let mut value = serde_json::to_value(session("legacy", false)).unwrap();
        value.as_object_mut().unwrap().remove("proxy_jump");
        let restored: SessionConfig = serde_json::from_value(value).unwrap();
        assert!(restored.proxy_jump.is_empty());
    }

    #[test]
    fn proxy_jump_resolution_preserves_order_and_expands_each_hop_before_it() {
        let target = SessionConfig {
            proxy_jump: vec!["jump-b".into(), "jump-c".into()],
            ..session("target", false)
        };
        let jump_a = session("jump-a", false);
        let jump_b = SessionConfig {
            proxy_jump: vec!["jump-a".into()],
            ..session("jump-b", false)
        };
        let jump_c = session("jump-c", false);
        let saved = vec![jump_a, jump_b, jump_c];

        let ordered = resolve_proxy_chain(&target, &saved).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["jump-a", "jump-b", "jump-c"]
        );
    }

    #[test]
    fn proxy_jump_rejects_self_cycles_duplicates_unknown_and_excess_depth() {
        let target = SessionConfig {
            proxy_jump: vec!["target".into()],
            ..session("target", false)
        };
        assert!(resolve_proxy_chain(&target, &[])
            .unwrap_err()
            .contains("循环"));

        let target = SessionConfig {
            proxy_jump: vec!["a".into()],
            ..session("target", false)
        };
        let a = SessionConfig {
            proxy_jump: vec!["b".into()],
            ..session("a", false)
        };
        let b = SessionConfig {
            proxy_jump: vec!["target".into()],
            ..session("b", false)
        };
        assert!(resolve_proxy_chain(&target, &[a, b])
            .unwrap_err()
            .contains("循环"));

        let target = SessionConfig {
            proxy_jump: vec!["a".into(), "a".into()],
            ..session("target", false)
        };
        assert!(resolve_proxy_chain(&target, &[session("a", false)])
            .unwrap_err()
            .contains("重复"));

        let target = SessionConfig {
            proxy_jump: vec!["missing".into()],
            ..session("target", false)
        };
        assert!(resolve_proxy_chain(&target, &[])
            .unwrap_err()
            .contains("不存在"));

        let target = SessionConfig {
            proxy_jump: (0..=MAX_PROXY_JUMPS)
                .map(|index| format!("jump-{index}"))
                .collect(),
            ..session("target", false)
        };
        assert!(resolve_proxy_chain(&target, &[])
            .unwrap_err()
            .contains("最多"));

        let target = SessionConfig {
            proxy_jump: vec!["jump-0".into()],
            ..session("target", false)
        };
        let saved = (0..=MAX_PROXY_JUMPS)
            .map(|index| SessionConfig {
                proxy_jump: if index < MAX_PROXY_JUMPS {
                    vec![format!("jump-{}", index + 1)]
                } else {
                    Vec::new()
                },
                ..session(&format!("jump-{index}"), false)
            })
            .collect::<Vec<_>>();
        assert!(resolve_proxy_chain(&target, &saved)
            .unwrap_err()
            .contains("最多"));
    }

    #[test]
    fn migration_atomic_save_preserves_the_previous_encrypted_file() {
        let dir =
            std::env::temp_dir().join(format!("chena-shell-state-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.0");
        let previous = b"previous encrypted bytes";
        std::fs::write(&path, previous).unwrap();
        let st = ConfigStore {
            sessions: vec![session("s", false)],
            ..Default::default()
        };

        write_store_atomic_preserving_previous(&path, &st).unwrap();

        let backup = std::fs::read(dir.join("data.0.pre-log-default-migration.bak")).unwrap();
        assert_eq!(backup, previous);
        let current = std::fs::read(&path).unwrap();
        assert_ne!(current.as_slice(), previous);
        let plaintext = unseal(&current).unwrap();
        let restored: ConfigStore = serde_json::from_slice(&plaintext).unwrap();
        assert!(!restored.sessions[0].log_enabled);
        assert!(!dir.join("data.0.log-default-migration.tmp").exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ai_audit_line_is_single_line_and_redacts_secrets() {
        let event = AIEvent {
            id: "evt-1".into(),
            session_id: "sess-1".into(),
            attach_token: None,
            ts: Utc::now(),
            kind: AIEventKind::Exec {
                command: "curl -H token=abc http://example.test/\nnext".into(),
                args: "diag".into(),
            },
            detail: "line1\nline2".into(),
            duration_ms: Some(12),
            exit_code: Some(0),
            source: "trae".into(),
            rule: None,
            denial_reason: None,
            started_at: None,
            finished_at: None,
        };
        let line = format_ai_event_line(&event);
        assert!(line.contains("[AI] session=sess-1 source=trae action=EXEC"));
        assert!(line.contains("exit=0"));
        assert!(line.contains("duration=12ms"));
        assert!(line.contains("args=diag"));
        assert!(!line.contains("abc"));
        assert!(!line.contains('\n'));
    }
}
