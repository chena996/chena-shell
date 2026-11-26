//! Tauri commands — 前端通过 `invoke()` 调用

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use serde::Serialize;
use tauri::State;

use crate::session::SessionHandle;
use crate::state::{
    AppState, AuthConfig, GlobalSettings, SessionConfig, SessionConfigPatch, SessionDto,
};

pub(crate) fn persist(state: &AppState) -> Result<(), String> {
    let list: Vec<crate::state::SessionConfig> = state
        .inner
        .read()
        .sessions
        .iter()
        .map(|e| e.value().config_snapshot())
        .collect();
    crate::state::save_persisted_sessions(&list)
}

// ---------- 会话列表 ----------

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> Vec<SessionDto> {
    let sessions = state.inner.read();
    sessions
        .sessions
        .iter()
        .map(|entry| SessionDto::new(&entry.value().config_snapshot(), entry.value().status()))
        .collect()
}

// ---------- 新建会话 ----------

#[tauri::command]
pub fn create_session(
    mut config: SessionConfig,
    state: State<'_, AppState>,
) -> Result<String, String> {
    validate_auth_config(&config.auth)?;
    if state.inner.read().sessions.contains_key(&config.id) {
        return Err("会话 ID 已存在".into());
    }
    let saved = state
        .inner
        .read()
        .sessions
        .iter()
        .map(|entry| entry.value().config_snapshot())
        .collect::<Vec<_>>();
    crate::state::resolve_proxy_chain(&config, &saved)?;
    let original = config.clone();
    if let Err(error) = crate::state::externalize_session_secrets(&mut config) {
        let _ = crate::state::cleanup_obsolete_session_secrets(&config, &original);
        return Err(format!("秘密保存失败: {error}"));
    }
    let settings = state.inner.read().settings.clone();
    let handle = SessionHandle::new(
        config.clone(),
        settings.global_blocklist,
        settings.confirm_list,
    );
    let id = config.id.clone();
    state
        .inner
        .write()
        .sessions
        .insert(id.clone(), Arc::new(handle));
    if let Err(error) = persist(&state) {
        state.inner.write().sessions.remove(&id);
        if let Err(cleanup) = crate::state::cleanup_obsolete_session_secrets(&config, &original) {
            return Err(format!("会话保存失败: {error}；新秘密清理失败: {cleanup}"));
        }
        return Err(format!("会话保存失败: {error}"));
    }
    Ok(id)
}

// ---------- 连接 / 断开 / 删除 ----------

#[tauri::command(async)]
pub fn connect_session(
    session_id: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let handle = match state.inner.read().sessions.get(&session_id) {
        Some(g) => (*g).clone(),
        None => return Err("会话不存在".into()),
    };
    handle.connect((*state).clone(), app)
}

#[tauri::command]
pub fn disconnect_session(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let handle = match state.inner.read().sessions.get(&session_id) {
        Some(g) => (*g).clone(),
        None => return Err("会话不存在".into()),
    };
    handle.force_detach(&state);
    handle.close();
    crate::approval::cancel_session(&state, &session_id, "会话已断开，未决审批已取消");
    state.inner.write().monitor_samples.remove(&session_id);
    Ok(())
}

#[tauri::command]
pub fn delete_session(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    if let Some(h) = &handle {
        h.close();
    }
    state.inner.write().sessions.remove(&session_id);
    state.inner.write().ai_events.remove(&session_id);
    state.inner.write().monitor_samples.remove(&session_id);
    if let Err(error) = persist(&state) {
        if let Some(handle) = &handle {
            state
                .inner
                .write()
                .sessions
                .insert(session_id.clone(), handle.clone());
        }
        return Err(format!("会话删除保存失败: {error}"));
    }
    if let Some(handle) = handle {
        if let Err(error) = crate::state::cleanup_session_secrets(&handle.config_snapshot()) {
            tracing::warn!("会话已删除，但秘密引用清理失败: {error}");
        }
    }
    Ok(())
}

// ---------- 更新会话配置 ----------

pub(crate) fn validate_auth_config(auth: &AuthConfig) -> Result<(), String> {
    match auth {
        AuthConfig::Password { password_ref } if password_ref.is_empty() => {
            Err("新建会话的密码不能为空".into())
        }
        AuthConfig::Key {
            private_key_path,
            private_key_inline_ref,
            ..
        } if private_key_path.trim().is_empty()
            && private_key_inline_ref
                .as_ref()
                .map_or(true, |value| value.trim().is_empty()) =>
        {
            Err("私钥路径或私钥内容不能为空".into())
        }
        _ => Ok(()),
    }
}

fn apply_auth_patch(current: &mut AuthConfig, incoming: AuthConfig) -> Result<(), String> {
    match (current, incoming) {
        (
            AuthConfig::Password {
                password_ref: current,
            },
            AuthConfig::Password { password_ref: next },
        ) => {
            if !next.is_empty() {
                *current = next;
            }
        }
        (
            AuthConfig::Key {
                private_key_path: current_path,
                private_key_inline_ref: current_inline,
                passphrase_ref: current_passphrase,
            },
            AuthConfig::Key {
                private_key_path: next_path,
                private_key_inline_ref: next_inline,
                passphrase_ref: next_passphrase,
            },
        ) => {
            if !next_path.trim().is_empty() {
                *current_path = next_path;
                *current_inline = None;
            }
            if let Some(next_inline) = next_inline.filter(|value| !value.trim().is_empty()) {
                *current_path = String::new();
                *current_inline = Some(next_inline);
            }
            if let Some(next_passphrase) = next_passphrase.filter(|value| !value.is_empty()) {
                *current_passphrase = Some(next_passphrase);
            }
        }
        (current, next) => {
            validate_auth_config(&next)?;
            *current = next;
        }
    }
    Ok(())
}

fn apply_session_patch(
    config: &mut SessionConfig,
    patch: SessionConfigPatch,
) -> Result<(), String> {
    if let Some(name) = patch.name {
        config.name = name;
    }
    if let Some(host) = patch.host {
        config.host = host;
    }
    if let Some(port) = patch.port {
        config.port = port;
    }
    if let Some(username) = patch.username {
        config.username = username;
    }
    if let Some(proxy_jump) = patch.proxy_jump {
        config.proxy_jump = proxy_jump;
    }
    if let Some(group) = patch.group {
        config.group = group;
    }
    if let Some(ai_allowed) = patch.ai_allowed {
        config.ai_allowed = ai_allowed;
    }
    if let Some(ai_policy) = patch.ai_policy {
        config.ai_policy = ai_policy;
    }
    if let Some(regexes) = patch.ai_blocklist_regexes {
        config.ai_blocklist_regexes = regexes;
    }
    if let Some(auto_connect) = patch.auto_connect {
        config.auto_connect = auto_connect;
    }
    if let Some(keepalive_secs) = patch.keepalive_secs {
        if keepalive_secs > 3600 {
            return Err("Keepalive 间隔必须在 0–3600 秒之间".into());
        }
        config.keepalive_secs = keepalive_secs;
    }
    if let Some(connect_timeout_secs) = patch.connect_timeout_secs {
        if !(1..=300).contains(&connect_timeout_secs) {
            return Err("连接超时必须在 1–300 秒之间".into());
        }
        config.connect_timeout_secs = connect_timeout_secs;
    }
    if let Some(port_forwards) = patch.port_forwards {
        config.port_forwards = port_forwards;
    }
    if let Some(log_enabled) = patch.log_enabled {
        config.log_enabled = log_enabled;
    }
    if let Some(auth) = patch.auth {
        apply_auth_patch(&mut config.auth, auth)?;
    }
    if patch.clear_secret {
        match &mut config.auth {
            AuthConfig::Password { password_ref } => password_ref.clear(),
            AuthConfig::Key { passphrase_ref, .. } => *passphrase_ref = None,
            AuthConfig::Agent => {}
        }
    }
    Ok(())
}

#[cfg(test)]
fn update_handle_config(
    handle: &Arc<SessionHandle>,
    patch: SessionConfigPatch,
) -> Result<(), String> {
    {
        let mut config = handle.config.write();
        apply_session_patch(&mut config, patch)?;
    }
    handle.refresh_session_security();
    Ok(())
}

#[tauri::command]
pub fn update_session(
    session_id: String,
    config: SessionConfigPatch,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner
            .sessions
            .get(&session_id)
            .map(|entry| entry.value().clone())
            .ok_or("会话不存在")?
    };
    let previous = handle.config_snapshot();
    let mut next = previous.clone();
    apply_session_patch(&mut next, config)?;
    let saved = state
        .inner
        .read()
        .sessions
        .iter()
        .map(|entry| entry.value().config_snapshot())
        .collect::<Vec<_>>();
    crate::state::resolve_proxy_chain(&next, &saved)?;
    if let Err(error) = crate::state::externalize_session_secrets(&mut next) {
        let _ = crate::state::cleanup_obsolete_session_secrets(&next, &previous);
        return Err(format!("秘密保存失败: {error}"));
    }
    *handle.config.write() = next.clone();
    handle.refresh_session_security();
    if let Err(error) = persist(&state) {
        *handle.config.write() = previous.clone();
        handle.refresh_session_security();
        if let Err(cleanup) = crate::state::cleanup_obsolete_session_secrets(&next, &previous) {
            return Err(format!(
                "会话修改保存失败: {error}；新秘密清理失败: {cleanup}"
            ));
        }
        return Err(format!("会话修改保存失败: {error}"));
    }
    if let Err(error) = crate::state::cleanup_obsolete_session_secrets(&previous, &next) {
        tracing::warn!("会话配置已保存，但旧秘密引用清理失败: {error}");
    }
    Ok(())
}

// ---------- 终端 I/O ----------

#[tauri::command]
pub fn terminal_input(
    session_id: String,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        Some(h) => h.send_input(&data),
        None => Err("会话不存在".into()),
    }
}

#[tauri::command]
pub fn terminal_input_bytes(
    session_id: String,
    data: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        Some(h) => h.send_input_bytes(&data),
        None => Err("会话不存在".into()),
    }
}

#[tauri::command]
pub fn terminal_resize(
    session_id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    if let Some(h) = handle {
        h.resize(cols, rows);
    }
    Ok(())
}

#[tauri::command]
pub async fn open_session_channel(
    session_id: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    let handle = handle.ok_or("会话不存在")?;
    handle.open_extra_channel().await
}

#[tauri::command]
pub fn attach_session_channel(
    session_id: String,
    channel_id: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        Some(h) => h.attach_extra_channel(&channel_id),
        None => Err("会话不存在".into()),
    }
}

#[tauri::command]
pub fn channel_input(
    session_id: String,
    channel_id: String,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        Some(h) => h.channel_input(&channel_id, &data),
        None => Err("会话不存在".into()),
    }
}

#[tauri::command]
pub fn channel_input_bytes(
    session_id: String,
    channel_id: String,
    data: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        Some(h) => h.channel_input_bytes(&channel_id, &data),
        None => Err("会话不存在".into()),
    }
}

#[tauri::command]
pub fn channel_resize(
    session_id: String,
    channel_id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    if let Some(h) = handle {
        h.channel_resize(&channel_id, cols, rows);
    }
    Ok(())
}

#[tauri::command]
pub fn read_ai_terminal(
    session_id: String,
    after_seq: Option<u64>,
    state: State<'_, AppState>,
) -> Result<crate::session::TerminalReplay, String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    }
    .ok_or("会话不存在")?;
    Ok(handle.read_ai_terminal(after_seq))
}

#[tauri::command]
pub fn set_active_terminal(
    session_id: Option<String>,
    channel_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<Option<u64>, String> {
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    match handle {
        // channel_id 为空表示主 Shell；未知渠道由后端回退到主 Shell。
        Some(h) => Ok(Some(h.set_active_terminal(channel_id.as_deref()))),
        None => Ok(None),
    }
}

#[tauri::command]
pub fn close_session_channel(
    session_id: String,
    channel_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    if let Some(h) = handle {
        h.close_extra_channel(&channel_id);
    }
    Ok(())
}

// ---------- AI 活动 ----------

#[tauri::command]
pub fn list_ai_events(
    session_id: String,
    state: State<'_, AppState>,
) -> Vec<crate::state::AIEvent> {
    state
        .inner
        .read()
        .ai_events
        .get(&session_id)
        .map(|b| b.snapshot())
        .unwrap_or_default()
}

#[tauri::command]
pub fn kick_ai(
    session_id: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner.sessions.get(&session_id).map(|g| g.value().clone())
    };
    if let Some(h) = handle {
        h.force_detach(&state);
    }
    use tauri::Emitter;
    let _ = app.emit(
        "session-status",
        serde_json::json!({"session_id": session_id, "ai_attached": false}),
    );
    Ok(())
}

// ---------- 监控 ----------

#[tauri::command]
pub fn get_monitor_snapshot(
    session_id: String,
    state: State<'_, AppState>,
) -> Option<crate::monitor::MonitorSnapshot> {
    crate::monitor::get_snapshot(&state, &session_id)
}

// ---------- SFTP（russh-sftp 协议，不经过远端 shell） ----------

pub use crate::sftp::SftpEntry;

fn connected_session(state: &AppState, session_id: &str) -> Result<Arc<SessionHandle>, String> {
    let handle = state
        .inner
        .read()
        .sessions
        .get(session_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| format!("会话不存在: {session_id}"))?;
    if !handle.connected.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("会话未连接".into());
    }
    Ok(handle)
}

fn sftp_config(state: &AppState, session_id: &str) -> Result<SessionConfig, String> {
    Ok(connected_session(state, session_id)?.config_snapshot())
}

#[tauri::command(async)]
pub async fn sftp_list(
    session_id: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<SftpEntry>, String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let entries = crate::sftp::list(&connection.session, &path).await?;
    crate::sftp_log::log(
        &session_id,
        "LIST",
        &format!("dir={path} entries={}", entries.len()),
    );
    Ok(entries)
}

/// 获取远端当前工作目录（用于把 "." 解析成真实路径）
#[tauri::command(async)]
pub async fn sftp_pwd(session_id: String, state: State<'_, AppState>) -> Result<String, String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    crate::sftp::pwd(&connection.session).await
}

/// 远端文件 stat
#[tauri::command(async)]
pub async fn sftp_stat(
    session_id: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<SftpEntry, String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    crate::sftp::stat(&connection.session, &path).await
}

const MAX_SFTP_EDIT_SIZE: u64 = 5 * 1024 * 1024;
/// 内置文本编辑器允许的上限；外部编辑器仍使用 5 MiB。
const MAX_BUILTIN_EDIT_SIZE: u64 = 20 * 1024 * 1024;
const SFTP_EDIT_CHUNK_SIZE: usize = 64 * 1024;
const SFTP_EDIT_CONFLICT_PREFIX: &str = "SFTP_EDIT_CONFLICT:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SftpEditRemoteVersion {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
}

impl From<&russh_sftp::protocol::FileAttributes> for SftpEditRemoteVersion {
    fn from(metadata: &russh_sftp::protocol::FileAttributes) -> Self {
        Self {
            size: metadata.size.unwrap_or(0),
            mtime: metadata.mtime.unwrap_or(0) as i64,
            is_dir: metadata.is_dir(),
            is_symlink: metadata.is_symlink(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SftpEditRecord {
    pub(crate) session_id: String,
    pub(crate) remote_path: String,
    pub(crate) local_path: PathBuf,
    pub(crate) backup_path: PathBuf,
    pub(crate) original: SftpEditRemoteVersion,
}

#[derive(Debug, Serialize)]
pub struct SftpEditBeginResult {
    edit_id: String,
    remote_path: String,
    local_path: String,
    backup_path: String,
    size: u64,
    mtime: i64,
}

pub(crate) fn sftp_edit_records() -> &'static Mutex<HashMap<String, SftpEditRecord>> {
    static RECORDS: OnceLock<Mutex<HashMap<String, SftpEditRecord>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn set_external_editor_enabled_for_test(enabled: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ENABLED: AtomicBool = AtomicBool::new(true);
    ENABLED.store(enabled, Ordering::SeqCst);
}

#[cfg(test)]
fn external_editor_enabled_for_test() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ENABLED: AtomicBool = AtomicBool::new(true);
    ENABLED.load(Ordering::SeqCst)
}

fn sftp_edit_record(edit_id: &str, session_id: &str) -> Result<SftpEditRecord, String> {
    let record = sftp_edit_records()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(edit_id)
        .cloned()
        .ok_or_else(|| "编辑会话不存在或已关闭，请重新打开文件".to_string())?;
    if record.session_id != session_id {
        return Err("编辑会话与 SSH 会话不匹配".into());
    }
    Ok(record)
}

pub(crate) async fn sftp_edit_remote_version(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> Result<Option<SftpEditRemoteVersion>, String> {
    match sftp.symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(SftpEditRemoteVersion::from(&metadata))),
        Err(error) if crate::sftp::is_not_found(&error) => Ok(None),
        Err(error) => Err(format!("读取远端属性失败 ({path}): {error}")),
    }
}

fn sftp_edit_conflict(
    expected: Option<SftpEditRemoteVersion>,
    current: Option<SftpEditRemoteVersion>,
) -> String {
    fn describe(version: Option<SftpEditRemoteVersion>) -> String {
        version.map_or_else(
            || "文件不存在".to_string(),
            |version| format!("size={} mtime={}", version.size, version.mtime),
        )
    }
    format!(
        "{SFTP_EDIT_CONFLICT_PREFIX} 远端文件已变化（期望 {}，当前 {}）；本地编辑已保留，必须确认后才能覆盖",
        describe(expected),
        describe(current)
    )
}

#[derive(Debug, Serialize, Clone)]
struct SftpEditSyncEvent {
    edit_id: String,
    session_id: String,
    remote_path: String,
    local_path: String,
    backup_path: String,
    status: String,
    message: String,
}

enum SftpEditUploadOutcome {
    Uploaded(SftpEditRemoteVersion),
    Conflict(String),
}

fn cancelled_edits() -> &'static Mutex<HashSet<String>> {
    static CANCELLED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CANCELLED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn sftp_edit_is_cancelled(edit_id: &str) -> bool {
    cancelled_edits()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(edit_id)
}

fn sftp_edit_safe_name(remote_path: &str) -> String {
    let name = remote_path.rsplit('/').next().unwrap_or("file");
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized.starts_with('.') {
        format!("file{sanitized}")
    } else {
        sanitized
    }
}

async fn open_in_default_editor(editor: &str, path: &Path) -> Result<(), String> {
    #[cfg(test)]
    {
        if !external_editor_enabled_for_test() {
            let _ = path;
            return Ok(());
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 现代 Notepad 会派生后台 broker 进程，Start-Process -Wait 可能等待 broker 而非
        // 实际编辑窗口，导致用户关闭窗口后仍不返回、编辑无法回传。这里启动后轮询“命令行包含
        // 该文件字面路径”的进程（broker 不携带字面路径，不会被误等），出现后等待其全部退出
        // 即视为编辑器已关闭。若编辑器未在命令行暴露路径（旧版 DDE 关联），则退化为启动后
        // 立即比较，与原 -Wait 瞬时返回行为一致。Get-CimInstance 偶发失败用 try/catch 容错，
        // 避免脚本非零退出误报 open_error；失败时捕获 stderr 写入日志便于定位。
        let script = concat!(
            "$ErrorActionPreference='Stop';",
            "$path=$env:CHENA_EDIT_PATH;",
            "$editor=$env:CHENA_EDITOR;",
            "if($editor){ Start-Process -FilePath $editor -ArgumentList ('\"' + $path + '\"') } else { Start-Process -FilePath $path };",
            "$seen=$false;",
            "for($i=0;$i -lt 40;$i++){",
            "  try{$p=Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($path) }}catch{$p=$null};",
            "  if($p){$seen=$true;break}",
            "  Start-Sleep -Milliseconds 250",
            "}",
            "if($seen){",
            "  while($true){",
            "    try{$p=Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($path) }}catch{$p=$null};",
            "    if(-not $p){break}",
            "    Start-Sleep -Milliseconds 500",
            "  }",
            "}",
            "exit 0",
        );
        let out = tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .env("CHENA_EDIT_PATH", path)
            .env("CHENA_EDITOR", editor)
            .output()
            .await
            .map_err(|e| format!("启动默认编辑器失败: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "默认编辑器退出异常: {:?} {}",
                out.status,
                stderr.trim()
            ))
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 未配置（或仍为 Windows 默认的 notepad）时退回系统默认关联程序。
        let custom = !editor.trim().is_empty() && editor.trim() != "notepad";
        let status = if custom {
            tokio::process::Command::new(editor)
                .arg(path)
                .status()
                .await
        } else if cfg!(target_os = "macos") {
            tokio::process::Command::new("open")
                .arg("-W")
                .arg(path)
                .status()
                .await
        } else {
            tokio::process::Command::new("xdg-open")
                .arg(path)
                .status()
                .await
        };
        let status = status.map_err(|e| format!("启动默认编辑器失败: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("默认编辑器退出异常: {status:?}"))
        }
    }
}

async fn local_files_equal(a: &Path, b: &Path) -> Result<bool, String> {
    let a = tokio::fs::read(a)
        .await
        .map_err(|e| format!("读取本地工作文件失败: {e}"))?;
    let b = tokio::fs::read(b)
        .await
        .map_err(|e| format!("读取本地备份文件失败: {e}"))?;
    Ok(a == b)
}

async fn upload_edited_file(
    state: &AppState,
    record: &SftpEditRecord,
    edit_id: &str,
    confirm_overwrite: bool,
    max_size: u64,
) -> Result<SftpEditUploadOutcome, String> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let session_id = &record.session_id;
    let connection = crate::sftp::connect_shared(connected_session(state, session_id)?).await?;
    let sftp = &connection.session;
    let current = sftp_edit_remote_version(sftp, &record.remote_path).await?;
    if current != Some(record.original) && !confirm_overwrite {
        let error = sftp_edit_conflict(Some(record.original), current);
        crate::sftp_log::log(session_id, "EDIT_CONFLICT", &error);
        return Ok(SftpEditUploadOutcome::Conflict(error));
    }
    if current.is_some_and(|version| version.is_dir || version.is_symlink) {
        return Err("远端路径已不是普通文件，未覆盖；本地编辑已保留".into());
    }
    let approved = current;

    let expected_size = tokio::fs::metadata(&record.local_path)
        .await
        .map_err(|e| format!("读取编辑临时文件大小失败: {e}"))?
        .len();
    if expected_size > max_size {
        return Err(format!(
            "编辑内容超过 {} MiB，不能保存",
            max_size / 1024 / 1024
        ));
    }

    let part_path = format!("{}.chena-edit-{edit_id}.part", record.remote_path);
    if sftp_edit_remote_version(sftp, &part_path)
        .await?
        .is_some_and(|version| version.is_dir || version.is_symlink)
    {
        return Err("远端编辑临时路径不是普通文件，未上传".into());
    }
    let mut destination = sftp
        .open_with_flags(
            part_path.clone(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| format!("打开远端编辑临时文件失败 ({part_path}): {e}"))?;
    let source = tokio::fs::File::open(&record.local_path)
        .await
        .map_err(|e| {
            format!(
                "打开编辑临时文件失败 ({}): {e}",
                record.local_path.display()
            )
        })?;
    let mut source = source.take(max_size + 1);
    let mut buffer = vec![0u8; SFTP_EDIT_CHUNK_SIZE];
    let mut written = 0u64;
    loop {
        let count = source
            .read(&mut buffer)
            .await
            .map_err(|e| format!("读取编辑临时文件失败: {e}"))?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .await
            .map_err(|e| format!("写入远端编辑临时文件失败: {e}"))?;
        destination
            .flush()
            .await
            .map_err(|e| format!("确认远端编辑临时文件写入失败: {e}"))?;
        written += count as u64;
    }
    destination
        .close()
        .await
        .map_err(|e| format!("关闭远端编辑临时文件失败: {e}"))?;
    if written != expected_size {
        return Err("本地编辑临时文件在上传期间发生变化，已保留供重试".into());
    }

    let before_rename = sftp_edit_remote_version(sftp, &record.remote_path).await?;
    if before_rename != approved {
        let error = sftp_edit_conflict(approved, before_rename);
        crate::sftp_log::log(session_id, "EDIT_CONFLICT", &error);
        return Ok(SftpEditUploadOutcome::Conflict(error));
    }
    sftp.posix_rename(part_path, record.remote_path.clone())
        .await
        .map_err(|e| format!("完成远端编辑保存时原子重命名失败: {e}"))?;
    let saved = sftp_edit_remote_version(sftp, &record.remote_path)
        .await?
        .ok_or_else(|| "保存后远端文件属性读取失败".to_string())?;
    Ok(SftpEditUploadOutcome::Uploaded(saved))
}

async fn remove_sftp_edit_file(path: &PathBuf) -> Result<(), String> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "删除编辑临时文件失败 ({}): {error}",
            path.display()
        )),
    }
}

/// 下载远端文件到本地缓存，留存原始备份，并用系统默认编辑器打开；
/// 编辑器关闭后比较工作文件与备份，有修改则自动回传。
#[tauri::command(async)]
pub async fn sftp_edit_begin(
    session_id: String,
    remote_path: String,
    state: State<'_, AppState>,
) -> Result<SftpEditBeginResult, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let sftp = &connection.session;
    let original = sftp_edit_remote_version(sftp, &remote_path)
        .await?
        .ok_or_else(|| format!("远端文件不存在: {remote_path}"))?;
    if original.is_dir || original.is_symlink {
        return Err("只能编辑普通远端文件".into());
    }
    if original.size > MAX_SFTP_EDIT_SIZE {
        return Err("远端文件超过 5 MiB，不能编辑".into());
    }

    let mut source = sftp
        .open(remote_path.clone())
        .await
        .map_err(|e| format!("打开远端编辑文件失败: {e}"))?;
    let mut bytes = Vec::with_capacity(original.size as usize);
    let read = (&mut source)
        .take(MAX_SFTP_EDIT_SIZE + 1)
        .read_to_end(&mut bytes)
        .await;
    let close = source.close().await;
    read.map_err(|e| format!("读取远端编辑文件失败: {e}"))?;
    close.map_err(|e| format!("关闭远端编辑文件失败: {e}"))?;
    if bytes.len() as u64 > MAX_SFTP_EDIT_SIZE {
        return Err("远端文件超过 5 MiB，不能编辑".into());
    }
    if sftp_edit_remote_version(sftp, &remote_path).await? != Some(original) {
        return Err("远端文件在加载期间发生变化，请重试".into());
    }

    let edit_id = uuid::Uuid::new_v4().to_string();
    let base_name = sftp_edit_safe_name(&remote_path);
    let edit_dir = crate::state::app_dir().join("edit").join(&edit_id);
    tokio::fs::create_dir_all(&edit_dir)
        .await
        .map_err(|e| format!("创建编辑缓存目录失败 ({}): {e}", edit_dir.display()))?;
    let local_path = edit_dir.join(&base_name);
    let backup_path = edit_dir.join(format!("{base_name}.original"));

    let write_result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&local_path)
            .await
            .map_err(|e| format!("创建编辑缓存文件失败 ({}): {e}", local_path.display()))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| format!("写入编辑缓存文件失败: {e}"))?;
        file.flush()
            .await
            .map_err(|e| format!("确认编辑缓存文件写入失败: {e}"))?;
        tokio::fs::copy(&local_path, &backup_path)
            .await
            .map_err(|e| format!("创建原始备份文件失败: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = write_result {
        let _ = tokio::fs::remove_dir_all(&edit_dir).await;
        return Err(error);
    }

    let record = SftpEditRecord {
        session_id: session_id.clone(),
        remote_path: remote_path.clone(),
        local_path: local_path.clone(),
        backup_path: backup_path.clone(),
        original,
    };
    sftp_edit_records()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(edit_id.clone(), record.clone());
    cancelled_edits()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&edit_id);

    let app_state = state.inner().clone();
    let task_editor = state.inner.read().settings.remote_editor.clone();
    let task_edit_id = edit_id.clone();
    let task_record = record;
    tokio::spawn(async move {
        let (status, message) = if sftp_edit_is_cancelled(&task_edit_id) {
            ("cancelled".to_string(), "已取消".to_string())
        } else {
            match open_in_default_editor(&task_editor, &task_record.local_path).await {
                Ok(()) => {
                    if sftp_edit_is_cancelled(&task_edit_id) {
                        ("cancelled".to_string(), "已取消".to_string())
                    } else if local_files_equal(&task_record.local_path, &task_record.backup_path)
                        .await
                        .unwrap_or(false)
                    {
                        ("unchanged".to_string(), "未检测到修改，未回传".to_string())
                    } else {
                        match upload_edited_file(
                            &app_state,
                            &task_record,
                            &task_edit_id,
                            false,
                            MAX_SFTP_EDIT_SIZE,
                        )
                        .await
                        {
                            Ok(SftpEditUploadOutcome::Uploaded(_)) => (
                                "uploaded".to_string(),
                                format!("已回传修改: {}", task_record.remote_path),
                            ),
                            Ok(SftpEditUploadOutcome::Conflict(error)) => {
                                ("conflict".to_string(), error)
                            }
                            Err(error) => ("error".to_string(), error),
                        }
                    }
                }
                Err(error) => ("open_error".to_string(), error),
            }
        };

        let cleanup = matches!(status.as_str(), "uploaded" | "unchanged" | "cancelled");
        if cleanup {
            let _ = remove_sftp_edit_file(&task_record.local_path).await;
            sftp_edit_records()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&task_edit_id);
        }
        cancelled_edits()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&task_edit_id);

        crate::sftp_log::log(
            &task_record.session_id,
            "EDIT_SYNC",
            &format!("{} | {} | {}", status, task_record.remote_path, message),
        );
        let payload = serde_json::to_value(SftpEditSyncEvent {
            edit_id: task_edit_id,
            session_id: task_record.session_id.clone(),
            remote_path: task_record.remote_path.clone(),
            local_path: task_record.local_path.to_string_lossy().to_string(),
            backup_path: task_record.backup_path.to_string_lossy().to_string(),
            status,
            message,
        })
        .unwrap_or(serde_json::Value::Null);
        crate::state::emit_app_event("sftp-edit-sync", payload);
    });

    crate::sftp_log::log(&session_id, "EDIT_BEGIN", &remote_path);
    Ok(SftpEditBeginResult {
        edit_id,
        remote_path,
        local_path: local_path.to_string_lossy().to_string(),
        backup_path: backup_path.to_string_lossy().to_string(),
        size: original.size,
        mtime: original.mtime,
    })
}

#[derive(Debug, Serialize)]
pub struct SftpEditSaveResult {
    size: u64,
    mtime: i64,
}

/// 保存内置编辑器内容；远端版本变化时必须由前端显式确认覆盖。
/// 保存成功后保留编辑会话与本地工作文件，便于继续编辑和再次保存。
#[tauri::command(async)]
pub async fn sftp_edit_save(
    session_id: String,
    edit_id: String,
    content: String,
    original_size: u64,
    original_mtime: i64,
    confirm_overwrite: bool,
    state: State<'_, AppState>,
) -> Result<SftpEditSaveResult, String> {
    let record = sftp_edit_record(&edit_id, &session_id)?;
    if original_size != record.original.size || original_mtime != record.original.mtime {
        return Err("传入的原始 size/mtime 与编辑会话不一致，请重新加载文件".into());
    }
    if content.len() as u64 > MAX_BUILTIN_EDIT_SIZE {
        return Err("编辑内容超过 20 MiB，不能保存".into());
    }
    tokio::fs::write(&record.local_path, content.as_bytes())
        .await
        .map_err(|e| {
            format!(
                "写入编辑缓存文件失败 ({}): {e}",
                record.local_path.display()
            )
        })?;

    match upload_edited_file(
        state.inner(),
        &record,
        &edit_id,
        confirm_overwrite,
        MAX_BUILTIN_EDIT_SIZE,
    )
    .await
    {
        Ok(SftpEditUploadOutcome::Uploaded(saved)) => {
            if let Some(current) = sftp_edit_records()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_mut(&edit_id)
            {
                current.original = saved;
            }
            // 同步备份，避免后续比较仍以旧内容为基线。
            let _ = tokio::fs::copy(&record.local_path, &record.backup_path).await;
            crate::sftp_log::log(&session_id, "EDIT_SAVE", &record.remote_path);
            Ok(SftpEditSaveResult {
                size: saved.size,
                mtime: saved.mtime,
            })
        }
        Ok(SftpEditUploadOutcome::Conflict(error)) => Err(format!("CONFLICT:{error}")),
        Err(error) => Err(error),
    }
}

/// 内置编辑器打开文件：读取远端内容并建立编辑会话，不启动外部编辑器。
#[derive(Debug, Serialize)]
pub struct SftpEditOpenResult {
    edit_id: String,
    remote_path: String,
    size: u64,
    mtime: i64,
    content: String,
}

fn decode_edit_text(bytes: Vec<u8>, remote_path: &str) -> Result<String, String> {
    if bytes.contains(&0) {
        return Err(format!(
            "{remote_path} 看起来是二进制文件，内置编辑器不支持"
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{remote_path} 不是 UTF-8 文本，内置编辑器不支持"))
}

#[tauri::command(async)]
pub async fn sftp_edit_open(
    session_id: String,
    remote_path: String,
    state: State<'_, AppState>,
) -> Result<SftpEditOpenResult, String> {
    use tokio::io::AsyncReadExt;

    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let sftp = &connection.session;
    let original = sftp_edit_remote_version(sftp, &remote_path)
        .await?
        .ok_or_else(|| format!("远端文件不存在: {remote_path}"))?;
    if original.is_dir || original.is_symlink {
        return Err("只能编辑普通远端文件".into());
    }
    if original.size > MAX_BUILTIN_EDIT_SIZE {
        return Err("远端文件超过 20 MiB，请改用外部编辑器".into());
    }

    let mut source = sftp
        .open(remote_path.clone())
        .await
        .map_err(|e| format!("打开远端文件失败: {e}"))?;
    let mut bytes = Vec::with_capacity(original.size as usize);
    let read = (&mut source)
        .take(MAX_BUILTIN_EDIT_SIZE + 1)
        .read_to_end(&mut bytes)
        .await;
    let close = source.close().await;
    read.map_err(|e| format!("读取远端文件失败: {e}"))?;
    close.map_err(|e| format!("关闭远端文件失败: {e}"))?;
    if bytes.len() as u64 > MAX_BUILTIN_EDIT_SIZE {
        return Err("远端文件超过 20 MiB，请改用外部编辑器".into());
    }
    if sftp_edit_remote_version(sftp, &remote_path).await? != Some(original) {
        return Err("远端文件在加载期间发生变化，请重试".into());
    }
    let content = decode_edit_text(bytes.clone(), &remote_path)?;

    let edit_id = uuid::Uuid::new_v4().to_string();
    let base_name = sftp_edit_safe_name(&remote_path);
    let edit_dir = crate::state::app_dir().join("edit").join(&edit_id);
    tokio::fs::create_dir_all(&edit_dir)
        .await
        .map_err(|e| format!("创建编辑缓存目录失败 ({}): {e}", edit_dir.display()))?;
    let local_path = edit_dir.join(&base_name);
    let backup_path = edit_dir.join(format!("{base_name}.original"));

    let write_result = async {
        tokio::fs::write(&local_path, &bytes)
            .await
            .map_err(|e| format!("写入编辑缓存文件失败 ({}): {e}", local_path.display()))?;
        tokio::fs::copy(&local_path, &backup_path)
            .await
            .map_err(|e| format!("创建原始备份文件失败: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = write_result {
        let _ = tokio::fs::remove_dir_all(&edit_dir).await;
        return Err(error);
    }

    sftp_edit_records()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            edit_id.clone(),
            SftpEditRecord {
                session_id: session_id.clone(),
                remote_path: remote_path.clone(),
                local_path,
                backup_path,
                original,
            },
        );
    cancelled_edits()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&edit_id);
    crate::sftp_log::log(&session_id, "EDIT_OPEN", &remote_path);

    Ok(SftpEditOpenResult {
        edit_id,
        remote_path,
        size: original.size,
        mtime: original.mtime,
        content,
    })
}

/// 编辑器窗口重新加载时读取工作文件内容与远端基线。
#[tauri::command(async)]
pub async fn sftp_edit_read(
    session_id: String,
    edit_id: String,
) -> Result<SftpEditOpenResult, String> {
    let record = sftp_edit_record(&edit_id, &session_id)?;
    let bytes = tokio::fs::read(&record.local_path)
        .await
        .map_err(|e| format!("读取编辑缓存文件失败: {e}"))?;
    let content = decode_edit_text(bytes, &record.remote_path)?;
    Ok(SftpEditOpenResult {
        edit_id,
        remote_path: record.remote_path,
        size: record.original.size,
        mtime: record.original.mtime,
        content,
    })
}

/// 内置编辑器相关的全局设置快照。
#[derive(Debug, Serialize)]
pub struct EditorPrefs {
    mode: String,
    shellcheck_enabled: bool,
    remote_schema_enabled: bool,
}

#[tauri::command]
pub fn get_editor_prefs() -> EditorPrefs {
    let settings = crate::state::load_settings();
    EditorPrefs {
        mode: settings.remote_editor_mode,
        shellcheck_enabled: settings.shellcheck_enabled,
        remote_schema_enabled: settings.remote_schema_enabled,
    }
}

/// 打开独立的内置编辑器窗口。
#[tauri::command]
pub fn open_edit_window(
    session_id: String,
    edit_id: String,
    title: String,
    app: AppHandle,
) -> Result<(), String> {
    let label = format!("chena-edit-{edit_id}");
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.show();
        let _ = win.set_focus();
        let _ = win.unminimize();
        return Ok(());
    }
    let url = format!("index.html#/edit?session_id={session_id}&edit_id={edit_id}");
    std::thread::spawn(move || {
        let built = WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
            .title(&title)
            .inner_size(1100.0, 720.0)
            .min_inner_size(720.0, 480.0)
            .position(120.0, 80.0)
            .resizable(true)
            .decorations(true)
            .build();
        if let Err(error) = built {
            tracing::error!("[EDIT] build FAILED: {error}");
        }
    });
    Ok(())
}

/// 取消编辑：阻止后台回传，删除本地工作文件，原始备份保留在 AppData 中。
#[tauri::command(async)]
pub async fn sftp_edit_cancel(session_id: String, edit_id: String) -> Result<(), String> {
    cancelled_edits()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(edit_id.clone());
    let record = sftp_edit_records()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&edit_id)
        .cloned();
    let Some(record) = record else {
        return Ok(());
    };
    if record.session_id != session_id {
        return Err("编辑会话与 SSH 会话不匹配".into());
    }
    // 编辑器可能仍持有工作文件句柄（尤其 Windows），删除失败不应阻止取消标记；
    // 待编辑器关闭后后台任务会再次清理。
    let _ = remove_sftp_edit_file(&record.local_path).await;
    sftp_edit_records()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&edit_id);
    crate::sftp_log::log(&session_id, "EDIT_CANCEL", &record.remote_path);
    Ok(())
}

/// 外部编辑器关闭后若远端版本变化，用户确认覆盖时用本地工作文件重新回传。
#[tauri::command(async)]
pub async fn sftp_edit_confirm_overwrite(
    session_id: String,
    edit_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let record = sftp_edit_record(&edit_id, &session_id)?;
    match upload_edited_file(state.inner(), &record, &edit_id, true, MAX_SFTP_EDIT_SIZE).await {
        Ok(SftpEditUploadOutcome::Uploaded(_)) => {
            remove_sftp_edit_file(&record.local_path).await?;
            sftp_edit_records()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&edit_id);
            cancelled_edits()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&edit_id);
            crate::sftp_log::log(&session_id, "EDIT_SAVE", &record.remote_path);
            let payload = serde_json::to_value(SftpEditSyncEvent {
                edit_id: edit_id.clone(),
                session_id: session_id.clone(),
                remote_path: record.remote_path.clone(),
                local_path: record.local_path.to_string_lossy().to_string(),
                backup_path: record.backup_path.to_string_lossy().to_string(),
                status: "uploaded".to_string(),
                message: format!("已确认覆盖并回传: {}", record.remote_path),
            })
            .unwrap_or(serde_json::Value::Null);
            crate::state::emit_app_event("sftp-edit-sync", payload);
            Ok(())
        }
        Ok(SftpEditUploadOutcome::Conflict(error)) => Err(error),
        Err(error) => Err(error),
    }
}

/// 本地文件通过 SFTP 协议流式上传，完成后由 .part 原子重命名。
#[tauri::command(async)]
pub async fn sftp_upload(
    session_id: String,
    remote_path: String,
    local_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let config = sftp_config(&state, &session_id)?;
    let result =
        crate::sftp_transfer::upload(&state, config, local_path.clone(), remote_path.clone()).await;
    crate::sftp_log::log(
        &session_id,
        "UPLOAD",
        &match &result {
            Ok(()) => format!("OK   {local_path} → {remote_path}"),
            Err(error) => format!("FAIL {local_path} → {remote_path} | {error}"),
        },
    );
    result
}

/// 远端文件通过 SFTP 协议流式下载到指定本地路径。
#[tauri::command(async)]
pub async fn sftp_download(
    session_id: String,
    remote_path: String,
    local_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let config = sftp_config(&state, &session_id)?;
    crate::sftp_transfer::download(&state, config, remote_path, local_path).await
}

/// 在系统文件管理器中打开（文件则定位并选中，目录则直接打开）
#[tauri::command]
pub fn open_in_explorer(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        if p.is_dir() {
            cmd.arg(&path);
        } else {
            // /select 需要路径与参数在同一个参数里
            cmd.arg(format!("/select,{path}"));
        }
        cmd.spawn()
            .map_err(|e| format!("启动资源管理器失败: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(if p.is_dir() {
                "-R".to_string()
            } else {
                "-R".to_string()
            })
            .arg(&path)
            .spawn()
            .map_err(|e| format!("启动 Finder 失败: {e}"))?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if p.is_dir() {
            path.clone()
        } else {
            p.parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or(path.clone())
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map_err(|e| format!("启动文件管理器失败: {e}"))?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("当前平台不支持".into())
}

/// 获取本地桌面路径（SFTP 初始打开桌面）
#[tauri::command]
pub fn get_home_dir() -> String {
    // Windows: USERPROFILE\Desktop；Linux: ~/Desktop；都 fallback 到 HOME
    let profile = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match profile {
        Some(v) => {
            let mut p = std::path::PathBuf::from(&v);
            p.push("Desktop");
            if p.exists() {
                p.to_string_lossy().to_string()
            } else {
                std::path::PathBuf::from(v).to_string_lossy().to_string()
            }
        }
        None => ".".into(),
    }
}

/// 本地目录浏览（SFTP 左侧面板用）
/// 本地目录条目是否隐藏：Windows 依据文件属性（隐藏/系统），其他平台沿用点前缀。
fn local_entry_hidden(meta: Option<&std::fs::Metadata>, name: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        let _ = name;
        let attrs = meta.map(|m| m.file_attributes()).unwrap_or(0);
        attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = meta;
        name.starts_with('.')
    }
}

#[tauri::command]
pub fn read_local_dir(path: String) -> Result<Vec<(String, bool, u64, u64, bool)>, String> {
    let dir = if path.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        std::path::PathBuf::from(&path)
    };
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("读目录失败: {e}"))? {
        let entry = entry.map_err(|e| format!("读条目失败: {e}"))?;
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = entry.file_name().to_string_lossy().to_string();
        let hidden = local_entry_hidden(meta.as_ref(), &name);
        entries.push((name, is_dir, size, mtime, hidden));
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(entries)
}

/// 读取本地文件内容并返回 base64（用于右键直接上传）
#[tauri::command]
pub fn read_local_file_base64(path: String) -> Result<String, String> {
    let data = std::fs::read(&path).map_err(|e| format!("读文件失败: {e}"))?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(&data))
}

/// 远端文件 → 直接写到本地路径（避开浏览器下载到默认目录）
#[tauri::command(async)]
pub async fn sftp_download_to_local(
    session_id: String,
    remote_path: String,
    local_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let config = sftp_config(&state, &session_id)?;
    let result =
        crate::sftp_transfer::download(&state, config, remote_path.clone(), local_path.clone())
            .await;
    crate::sftp_log::log(
        &session_id,
        "DOWN_LOCAL",
        &match &result {
            Ok(()) => format!("OK   {remote_path} → {local_path}"),
            Err(error) => format!("FAIL {remote_path} → {local_path} | {error}"),
        },
    );
    result
}

/// 远端目录 → 通过 SFTP read_dir 递归下载到本地目录。
#[tauri::command(async)]
pub async fn sftp_download_dir_to_local(
    session_id: String,
    remote_path: String,
    local_path: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let config = sftp_config(&state, &session_id)?;
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let nodes = crate::sftp::collect_tree(&connection.session, &remote_path).await?;
    drop(connection);

    let base = std::path::PathBuf::from(&local_path);
    std::fs::create_dir_all(&base).map_err(|e| format!("创建本地目录失败: {e}"))?;
    let root = remote_path.trim_end_matches('/');
    let mut count = 0usize;
    for node in nodes {
        let relative = node
            .path
            .strip_prefix(root)
            .unwrap_or(&node.path)
            .trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        let native = relative.replace('/', std::path::MAIN_SEPARATOR_STR);
        let relative_path = std::path::Path::new(&native);
        if relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("远端目录包含不安全的相对路径: {relative}"));
        }
        let target = base.join(relative_path);
        if node.is_dir {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("创建本地目录失败 ({}): {e}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建本地目录失败 ({}): {e}", parent.display()))?;
        }
        crate::sftp_transfer::download(
            &state,
            config.clone(),
            node.path,
            target.to_string_lossy().to_string(),
        )
        .await?;
        count += 1;
    }
    crate::sftp_log::log(
        &session_id,
        "DOWN_DIR",
        &format!("OK   {remote_path} → {local_path} files={count}"),
    );
    Ok(count)
}

/// 远端文件权限修改（SFTP setstat）
#[tauri::command(async)]
pub async fn sftp_chmod(
    session_id: String,
    remote_path: String,
    mode: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let result = crate::sftp::chmod(&connection.session, &remote_path, &mode).await;
    crate::sftp_log::log(
        &session_id,
        "CHMOD",
        &match &result {
            Ok(()) => format!("OK   {mode} {remote_path}"),
            Err(error) => format!("FAIL {mode} {remote_path} | {error}"),
        },
    );
    result
}

/// 删除远端文件或目录（SFTP remove/rmdir，目录递归且不跟随符号链接）
#[tauri::command(async)]
pub async fn sftp_delete(
    session_id: String,
    remote_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let result = crate::sftp::delete(&connection.session, &remote_path).await;
    crate::sftp_log::log(
        &session_id,
        "DELETE",
        &match &result {
            Ok(()) => format!("OK   {remote_path}"),
            Err(error) => format!("FAIL {remote_path} | {error}"),
        },
    );
    result
}

/// 远端递归创建目录（SFTP mkdir）
#[tauri::command(async)]
pub async fn sftp_mkdir(
    session_id: String,
    remote_path: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let connection = crate::sftp::connect_shared(connected_session(&state, &session_id)?).await?;
    let result = crate::sftp::mkdir_p(&connection.session, &remote_path).await;
    crate::sftp_log::log(
        &session_id,
        "MKDIR",
        &match &result {
            Ok(()) => format!("OK   {remote_path}"),
            Err(error) => format!("FAIL {remote_path} | {error}"),
        },
    );
    result
}

/// 本地递归读取目录（返回所有文件相对路径列表，用于文件夹上传）
#[tauri::command]
pub fn read_local_dir_recursive(path: String) -> Result<Vec<(String, bool, u64)>, String> {
    let root = std::path::PathBuf::from(&path);
    let mut out = Vec::new();
    fn walk(
        dir: &std::path::Path,
        prefix: &std::path::Path,
        out: &mut Vec<(String, bool, u64)>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = prefix.join(&name).to_string_lossy().replace('\\', "/");
            if meta.is_dir() {
                out.push((rel, true, 0));
                walk(&entry.path(), prefix, out)?;
            } else {
                out.push((rel, false, meta.len()));
            }
        }
        Ok(())
    }
    walk(&root, &std::path::PathBuf::new(), &mut out).map_err(|e| format!("遍历失败: {e}"))?;
    Ok(out)
}

// ---------- 设置 ----------

#[tauri::command]
pub fn accept_host_key(
    challenge: crate::host_key::HostKeyChallenge,
) -> Result<crate::host_key::HostKeyRecord, String> {
    crate::host_key::accept_changed_key(
        &challenge.host,
        challenge.port,
        &challenge.algorithm,
        &challenge.public_key,
        &challenge.previous_fingerprint,
    )
}

#[tauri::command]
pub fn take_restore_sessions_status() -> crate::state::RestoreSessionsStatus {
    crate::state::take_restore_sessions_status()
}

#[tauri::command]
pub fn get_global_settings(state: State<'_, AppState>) -> GlobalSettings {
    state.inner.read().settings.clone()
}

/// 前端密码弹窗回传：密码只在内存中短暂存在，立即交给等待中的 root 执行任务。
/// 传 None 表示用户取消。
#[tauri::command]
pub fn submit_sudo_password(
    state: State<'_, AppState>,
    request_id: String,
    password: Option<String>,
) -> bool {
    let sender = state
        .inner
        .read()
        .sudo_prompts
        .remove(&request_id)
        .map(|(_, sender)| sender);
    match sender {
        Some(sender) => sender.send(password).is_ok(),
        None => false,
    }
}

fn apply_global_settings(state: &AppState, settings: &GlobalSettings) {
    let handles = {
        let mut inner = state.inner.write();
        inner.settings = settings.clone();
        inner
            .sessions
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>()
    };
    for handle in handles {
        handle
            .security_engine
            .update_global(&settings.global_blocklist, &settings.confirm_list);
        handle
            .security_engine
            .update_sudo(&settings.sudo_blocklist, &settings.sudo_confirm_list);
    }
}

#[tauri::command(async)]
pub async fn update_global_settings(
    settings: GlobalSettings,
    ui_prefs: Option<crate::state::UiPrefs>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    static SAVE_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = SAVE_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    if !(1..=365).contains(&settings.log_retention_days) {
        return Err("日志保留天数必须在 1–365 天之间".into());
    }
    if settings.mcp_port == 0 {
        return Err("MCP 端口必须在 1–65535 之间".into());
    }
    crate::mcp_server::validate_loopback(&settings.mcp_host).map_err(|error| error.to_string())?;
    if !(1..=1440).contains(&settings.attach_token_ttl_minutes) {
        return Err("attach token 有效期必须在 1–1440 分钟之间".into());
    }
    if !(1..=1440).contains(&settings.idle_detach_minutes) {
        return Err("AI 空闲断开时间必须在 1–1440 分钟之间".into());
    }
    for (field, patterns) in [
        ("全局黑名单", &settings.global_blocklist),
        ("确认清单", &settings.confirm_list),
        ("sudo 黑名单", &settings.sudo_blocklist),
        ("sudo 确认清单", &settings.sudo_confirm_list),
    ] {
        for pattern in patterns {
            regex::Regex::new(pattern)
                .map_err(|error| format!("{field}包含无效正则 {pattern:?}: {error}"))?;
        }
    }
    if !(8..=32).contains(&settings.terminal_font_size) {
        return Err("终端字号必须在 8–32 px 之间".into());
    }
    if settings.default_keepalive_secs > 3600 {
        return Err("默认 Keepalive 间隔必须在 0–3600 秒之间".into());
    }
    if !(1..=300).contains(&settings.connect_timeout_secs) {
        return Err("连接超时必须在 1–300 秒之间".into());
    }
    if !(1..=20).contains(&settings.reconnect_max_attempts) {
        return Err("最大重连次数必须在 1–20 次之间".into());
    }
    if !(1..=1000).contains(&settings.sudo_quota_per_attach) {
        return Err("sudo 次数上限必须在 1–1000 之间".into());
    }
    if !(1..=10).contains(&settings.sudo_max_failures) {
        return Err("sudo 密码错误锁定次数必须在 1–10 之间".into());
    }
    if !(30..=86400).contains(&settings.sudo_lockout_seconds) {
        return Err("sudo 锁定时长必须在 30–86400 秒之间".into());
    }
    if !(1..=1440).contains(&settings.ai_task_max_minutes) {
        return Err("任务授权最长时长必须在 1–1440 分钟之间".into());
    }
    if settings
        .ai_diag_allowlist
        .iter()
        .any(|entry| entry.trim().is_empty())
    {
        return Err("diag 只读白名单不能包含空行".into());
    }

    let state = (*state).clone();
    let (previous, was_running) = {
        let inner = state.inner.read();
        (inner.settings.clone(), inner.mcp_starting)
    };
    let previous_ui_prefs = ui_prefs.as_ref().map(|_| crate::state::load_ui_prefs());
    crate::state::save_settings_and_ui_prefs(&settings, ui_prefs.as_ref())
        .map_err(|error| format!("设置保存失败: {error}"))?;
    apply_global_settings(&state, &settings);

    let endpoint_changed =
        previous.mcp_host != settings.mcp_host || previous.mcp_port != settings.mcp_port;
    let mut operation = Ok(());
    if !settings.mcp_enabled {
        if was_running {
            operation = crate::mcp_server::stop_and_wait(&state).await;
        }
    } else if !previous.mcp_enabled || (was_running && endpoint_changed) {
        if was_running {
            operation = crate::mcp_server::stop_and_wait(&state).await;
        }
        if operation.is_ok() {
            operation = crate::mcp_server::start_and_wait(state.clone(), app.clone()).await;
        }
    }

    if let Err(error) = operation {
        let mut rollback_errors = Vec::new();
        let restored =
            match crate::state::save_settings_and_ui_prefs(&previous, previous_ui_prefs.as_ref()) {
                Ok(()) => {
                    apply_global_settings(&state, &previous);
                    true
                }
                Err(restore_error) => {
                    rollback_errors.push(format!("旧配置恢复失败: {restore_error}"));
                    false
                }
            };
        if state.inner.read().mcp_starting {
            if let Err(stop_error) = crate::mcp_server::stop_and_wait(&state).await {
                rollback_errors.push(format!("新服务停止失败: {stop_error}"));
            }
        }
        if was_running && restored {
            if let Err(start_error) = crate::mcp_server::start_and_wait(state.clone(), app).await {
                rollback_errors.push(format!("旧 MCP 服务恢复失败: {start_error}"));
            }
        }
        let detail = if rollback_errors.is_empty() {
            "已恢复原配置和 MCP 服务".to_string()
        } else {
            rollback_errors.join("；")
        };
        return Err(format!("MCP 设置应用失败: {error}；{detail}"));
    }

    // 终端日志默认值对所有已有会话生效；已连接会话在下一次连接时应用。
    if previous.terminal_log_default_enabled != settings.terminal_log_default_enabled {
        let handles = {
            let inner = state.inner.read();
            inner
                .sessions
                .iter()
                .map(|entry| entry.value().clone())
                .collect::<Vec<_>>()
        };
        for handle in handles {
            handle.config.write().log_enabled = settings.terminal_log_default_enabled;
        }
        persist(&state).map_err(|error| format!("终端日志默认设置同步到会话失败: {error}"))?;
    }

    crate::logging::cleanup_logs(settings.log_retention_days);
    Ok(())
}

/// 界面偏好（主题 / 终端字号）— 与其他用户配置一起加密存储
#[tauri::command]
pub fn get_ui_prefs() -> crate::state::UiPrefs {
    crate::state::load_ui_prefs()
}

/// 界面偏好（主题 / 终端字号）— 与其他用户配置一起加密存储
/// 只更新传入的字段，避免互相覆盖
#[tauri::command]
pub fn save_ui_prefs(
    theme: Option<String>,
    terminal_font_size: Option<u32>,
    copy_on_select: Option<bool>,
    confirm_multiline_paste: Option<bool>,
    links_enabled: Option<bool>,
    bottom_panel_height: Option<u32>,
) -> Result<(), String> {
    let mut prefs = crate::state::load_ui_prefs();
    if let Some(t) = theme {
        prefs.theme = t;
    }
    if let Some(s) = terminal_font_size {
        prefs.terminal_font_size = s.clamp(8, 32);
    }
    if let Some(value) = copy_on_select {
        prefs.copy_on_select = value;
    }
    if let Some(value) = confirm_multiline_paste {
        prefs.confirm_multiline_paste = value;
    }
    if let Some(value) = links_enabled {
        prefs.links_enabled = value;
    }
    if let Some(value) = bottom_panel_height {
        prefs.bottom_panel_height = value.clamp(160, 640);
    }
    crate::state::save_ui_prefs(prefs).map_err(|error| format!("界面偏好保存失败: {error}"))
}

// ---------- MCP ----------

#[derive(Serialize)]
pub struct McpStatus {
    pub enabled: bool,
    pub listening: bool,
    pub starting: bool,
    pub host: String,
    pub port: u16,
    pub endpoint: String,
    pub credential_configured: bool,
}

fn mcp_status_snapshot(state: &AppState) -> McpStatus {
    let inner = state.inner.read();
    let host = inner.settings.mcp_host.clone();
    let port = inner.settings.mcp_port;
    let endpoint_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    McpStatus {
        enabled: inner.settings.mcp_enabled,
        listening: inner.mcp_listening,
        starting: inner.mcp_starting && !inner.mcp_listening,
        host,
        port,
        endpoint: format!("http://{endpoint_host}:{port}/mcp"),
        credential_configured: inner.mcp_credential_hash.is_some(),
    }
}

#[tauri::command]
pub fn mcp_status(state: State<'_, AppState>) -> McpStatus {
    mcp_status_snapshot(&state)
}

/// 原始凭证只通过本命令返回一次；状态和配置中只保留盐化哈希。
#[tauri::command]
pub fn generate_mcp_credential(state: State<'_, AppState>) -> Result<String, String> {
    crate::mcp_auth::generate_credential(&state)
}

#[tauri::command(async)]
pub async fn start_mcp(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<McpStatus, String> {
    let state = (*state).clone();
    if !state.inner.read().settings.mcp_enabled {
        return Err("请先在设置中启用 MCP 服务".into());
    }
    crate::mcp_server::start_and_wait(state.clone(), app)
        .await
        .map_err(|error| format!("MCP 启动失败: {error}"))?;
    Ok(mcp_status_snapshot(&state))
}

#[tauri::command(async)]
pub async fn stop_mcp(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let state = (*state).clone();
    crate::mcp_server::stop_and_wait(&state)
        .await
        .map_err(|error| format!("MCP 停止失败: {error}"))?;
    Ok(mcp_status_snapshot(&state))
}

#[tauri::command(async)]
pub async fn restart_mcp(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<McpStatus, String> {
    let state = (*state).clone();
    crate::mcp_server::restart(state.clone(), app)
        .await
        .map_err(|error| format!("MCP 重启失败: {error}"))?;
    Ok(mcp_status_snapshot(&state))
}

#[tauri::command]
pub fn list_confirmations(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> Vec<crate::approval::Confirmation> {
    crate::approval::list_pending(&state, session_id.as_deref())
}

#[tauri::command]
pub fn resolve_confirmation(
    confirmation_id: String,
    approved: bool,
    state: State<'_, AppState>,
) -> Result<crate::approval::Confirmation, String> {
    crate::approval::resolve(&state, &confirmation_id, approved)
}

/// 用户在审批窗口内查看时暂停倒计时，移出后恢复。
#[tauri::command]
pub fn set_confirmation_hold(
    confirmation_id: String,
    hold: bool,
    state: State<'_, AppState>,
) -> Result<Option<crate::approval::Confirmation>, String> {
    crate::approval::set_hold(&state, &confirmation_id, hold)
}

// ---------- 端口转发 ----------

#[derive(Serialize)]
pub struct ForwardView {
    pub id: String,
    pub kind: crate::state::ForwardKind,
    pub listen_host: String,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    pub enabled: bool,
    pub running: bool,
}

fn mutate_config(
    state: &AppState,
    session_id: &str,
    f: impl FnOnce(&mut crate::state::SessionConfig),
) -> Result<(), String> {
    let handle = {
        let inner = state.inner.read();
        inner
            .sessions
            .get(session_id)
            .map(|entry| entry.value().clone())
            .ok_or("会话不存在")?
    };
    f(&mut handle.config.write());
    persist(state)
}

#[tauri::command]
pub fn list_forwards(session_id: String, state: State<'_, AppState>) -> Vec<ForwardView> {
    let cfg = match state.inner.read().sessions.get(&session_id) {
        Some(g) => g.value().config_snapshot(),
        None => return Vec::new(),
    };
    let running_ids: Vec<String> = state
        .inner
        .read()
        .forwards
        .iter()
        .map(|e| e.key().clone())
        .collect();
    cfg.port_forwards
        .into_iter()
        .map(|f| ForwardView {
            running: running_ids.contains(&f.id),
            id: f.id,
            kind: f.kind,
            listen_host: f.listen_host,
            listen_port: f.listen_port,
            dest_host: f.dest_host,
            dest_port: f.dest_port,
            enabled: f.enabled,
        })
        .collect()
}

#[tauri::command]
pub fn add_port_forward(
    session_id: String,
    forward: crate::state::PortForwardConfig,
    state: State<'_, AppState>,
) -> Result<(), String> {
    mutate_config(&state, &session_id, |cfg| {
        cfg.port_forwards.retain(|f| f.id != forward.id);
        cfg.port_forwards.push(forward);
    })
}

#[tauri::command]
pub fn remove_port_forward(
    session_id: String,
    forward_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    crate::forward::stop(&state, &forward_id);
    mutate_config(&state, &session_id, |cfg| {
        cfg.port_forwards.retain(|f| f.id != forward_id);
    })
}

#[tauri::command(async)]
pub fn start_port_forward(
    session_id: String,
    forward_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let spec = {
        let inner = state.inner.read();
        let handle = inner.sessions.get(&session_id).ok_or("会话不存在")?;
        handle
            .value()
            .config_snapshot()
            .port_forwards
            .into_iter()
            .find(|f| f.id == forward_id)
            .ok_or("转发规则不存在")?
    };
    crate::forward::start(&state, &session_id, spec)
}

#[tauri::command]
pub fn stop_port_forward(forward_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::forward::stop(&state, &forward_id);
    Ok(())
}

// ---------- 日志归档 ----------

#[tauri::command]
pub fn list_session_logs(
    session_id: String,
    state: State<'_, AppState>,
) -> Vec<crate::logging::LogFileInfo> {
    let Some((host, port)) = session_endpoint(&state, &session_id) else {
        return Vec::new();
    };
    crate::logging::list_logs(&host, port)
}

#[tauri::command]
pub fn read_session_log(
    session_id: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let (host, port) = session_endpoint(&state, &session_id).ok_or("会话不存在")?;
    crate::logging::read_log(&host, port, &name)
}

#[tauri::command]
pub fn list_ai_terminal_logs(
    session_id: String,
    state: State<'_, AppState>,
) -> Vec<crate::logging::LogFileInfo> {
    let Some((host, port)) = session_endpoint(&state, &session_id) else {
        return Vec::new();
    };
    crate::logging::list_ai_logs(&session_id, &host, port)
}

#[tauri::command]
pub fn read_ai_terminal_log(
    session_id: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let (host, port) = session_endpoint(&state, &session_id).ok_or("会话不存在")?;
    crate::logging::read_ai_log(&session_id, &host, port, &name)
}

fn session_endpoint(state: &AppState, session_id: &str) -> Option<(String, u16)> {
    state.inner.read().sessions.get(session_id).map(|g| {
        let config = g.value().config_snapshot();
        (config.host, config.port)
    })
}

#[tauri::command]
pub fn open_logs_dir() -> Result<(), String> {
    // 打开统一根目录，用户可看到 sessions / sftp / ai 等分类。
    let dir = crate::logging::root_dir();
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dir;
    }
    Ok(())
}

// ---------- 快速命令 ----------

#[tauri::command]
pub fn list_quick_commands(state: State<'_, AppState>) -> Vec<crate::state::QuickCommand> {
    state.inner.read().quick_commands.clone()
}

#[tauri::command]
pub fn save_quick_command(
    command: crate::state::QuickCommand,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut list = state.inner.read().quick_commands.clone();
    list.retain(|c| c.id != command.id);
    list.push(command);
    crate::state::save_quick_commands(&list)
        .map_err(|error| format!("快速命令保存失败: {error}"))?;
    state.inner.write().quick_commands = list;
    Ok(())
}

#[tauri::command]
pub fn delete_quick_command(id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut list = state.inner.read().quick_commands.clone();
    list.retain(|c| c.id != id);
    crate::state::save_quick_commands(&list)
        .map_err(|error| format!("快速命令删除保存失败: {error}"))?;
    state.inner.write().quick_commands = list;
    Ok(())
}

#[tauri::command]
pub fn run_quick_command(
    session_id: String,
    command: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let handle = match state.inner.read().sessions.get(&session_id) {
        Some(g) => (*g).clone(),
        None => return Err("会话不存在".into()),
    };
    if !handle.connected.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("会话未连接".into());
    }
    handle.send_input(&format!("{command}\n"))
}

// ===================== SFTP 独立窗口管理 =====================

use tauri::{AppHandle, Manager, WebviewWindowBuilder};

/// 打开或激活 SFTP 独立窗口
#[tauri::command]
pub fn open_sftp_window(
    session_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    tracing::info!("[SFTP] open_sftp_window called, session_id={session_id}");

    let cfg = {
        let inner = state.inner.read();
        let handle = inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        (
            handle.config_snapshot(),
            handle.connected.load(std::sync::atomic::Ordering::Relaxed),
        )
    };

    let label = format!("chena-sftp-{session_id}");
    crate::sftp_log::log(
        &session_id,
        "WINDOW",
        &format!(
            "OPEN {}@{} connected={} label={label}",
            cfg.0.username, cfg.0.host, cfg.1
        ),
    );

    // 如果已存在窗口 — 直接 focus
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.show();
        let _ = win.set_focus();
        let _ = win.unminimize();
        return Ok(());
    }

    // === build 扔到独立线程 — WebviewWindowBuilder::build() 阻塞 Tauri invoke 线程 ===
    let url = format!("index.html#/sftp?session_id={session_id}");
    let title = format!(
        "Chena-Sftp — {} ({}@{})",
        cfg.0.name, cfg.0.username, cfg.0.host
    );
    let app_clone = app.clone();
    let label_clone = label.clone();

    std::thread::spawn(move || {
        tracing::info!("[SFTP] building window...");
        match WebviewWindowBuilder::new(
            &app_clone,
            &label_clone,
            tauri::WebviewUrl::App(url.into()),
        )
        .title(&title)
        .inner_size(1000.0, 640.0)
        .min_inner_size(720.0, 480.0)
        .position(80.0, 60.0)
        .resizable(true)
        .decorations(true)
        .build()
        {
            Ok(_w) => {
                tracing::info!("[SFTP] window built OK");
            }
            Err(e) => {
                tracing::error!("[SFTP] build FAILED: {e}");
            }
        }
    });

    // 注册到 state
    state.inner.write().sftp_windows.insert(session_id, label);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{CheckResult, SecurityPolicy};
    use crate::state::{AIPolicyOverride, SessionDto};

    fn password_config(password: &str) -> SessionConfig {
        SessionConfig {
            id: "s1".into(),
            name: "old-name".into(),
            host: "old-host".into(),
            port: 22,
            username: "root".into(),
            auth: AuthConfig::Password {
                password_ref: password.into(),
            },
            proxy_jump: Vec::new(),
            group: "old-group".into(),
            ai_allowed: true,
            ai_policy: AIPolicyOverride::Recommended,
            ai_blocklist_regexes: vec!["^old-danger$".into()],
            auto_connect: false,
            keepalive_secs: 30,
            connect_timeout_secs: 30,
            port_forwards: Vec::new(),
            log_enabled: false,
        }
    }

    fn current_password(config: &SessionConfig) -> &str {
        match &config.auth {
            AuthConfig::Password { password_ref } => password_ref,
            _ => panic!("expected password auth"),
        }
    }

    #[test]
    fn password_patch_supports_keep_replace_and_explicit_clear() {
        let mut config = password_config("old-secret");
        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                name: Some("new-name".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(current_password(&config), "old-secret");
        assert_eq!(config.host, "old-host");
        assert_eq!(config.group, "old-group");
        assert_eq!(config.name, "new-name");

        for auth in [
            None,
            Some(AuthConfig::Password {
                password_ref: String::new(),
            }),
        ] {
            apply_session_patch(
                &mut config,
                SessionConfigPatch {
                    auth,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(current_password(&config), "old-secret");
        }

        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                auth: Some(AuthConfig::Password {
                    password_ref: "new-secret".into(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(current_password(&config), "new-secret");

        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                clear_secret: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(current_password(&config), "");
    }

    #[test]
    fn key_passphrase_patch_supports_keep_replace_and_explicit_clear() {
        let mut config = password_config("unused");
        config.auth = AuthConfig::Key {
            private_key_path: "/old/key".into(),
            private_key_inline_ref: None,
            passphrase_ref: Some("old-passphrase".into()),
        };

        for passphrase_ref in [None, Some(String::new())] {
            apply_session_patch(
                &mut config,
                SessionConfigPatch {
                    auth: Some(AuthConfig::Key {
                        private_key_path: "/new/key".into(),
                        private_key_inline_ref: None,
                        passphrase_ref,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            match &config.auth {
                AuthConfig::Key {
                    private_key_path,
                    passphrase_ref,
                    ..
                } => {
                    assert_eq!(private_key_path, "/new/key");
                    assert_eq!(passphrase_ref.as_deref(), Some("old-passphrase"));
                }
                _ => unreachable!(),
            }
        }

        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                auth: Some(AuthConfig::Key {
                    private_key_path: "/new/key".into(),
                    private_key_inline_ref: None,
                    passphrase_ref: Some("new-passphrase".into()),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        match &config.auth {
            AuthConfig::Key { passphrase_ref, .. } => {
                assert_eq!(passphrase_ref.as_deref(), Some("new-passphrase"));
            }
            _ => unreachable!(),
        }

        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                clear_secret: true,
                ..Default::default()
            },
        )
        .unwrap();
        match &config.auth {
            AuthConfig::Key { passphrase_ref, .. } => assert!(passphrase_ref.is_none()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn create_validation_rejects_an_empty_password() {
        assert!(validate_auth_config(&AuthConfig::Password {
            password_ref: String::new(),
        })
        .is_err());
        assert!(validate_auth_config(&AuthConfig::Password {
            password_ref: "secret".into(),
        })
        .is_ok());
    }

    #[test]
    fn key_inline_content_accepts_empty_path_and_replaces_path_on_patch() {
        assert!(validate_auth_config(&AuthConfig::Key {
            private_key_path: String::new(),
            private_key_inline_ref: Some("-----BEGIN OPENSSH PRIVATE KEY-----".into()),
            passphrase_ref: None,
        })
        .is_ok());
        assert!(validate_auth_config(&AuthConfig::Key {
            private_key_path: String::new(),
            private_key_inline_ref: None,
            passphrase_ref: None,
        })
        .is_err());

        let mut config = password_config("unused");
        config.auth = AuthConfig::Key {
            private_key_path: "/old/key".into(),
            private_key_inline_ref: None,
            passphrase_ref: None,
        };
        apply_session_patch(
            &mut config,
            SessionConfigPatch {
                auth: Some(AuthConfig::Key {
                    private_key_path: String::new(),
                    private_key_inline_ref: Some("inline-key".into()),
                    passphrase_ref: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        match &config.auth {
            AuthConfig::Key {
                private_key_path,
                private_key_inline_ref,
                ..
            } => {
                assert!(private_key_path.is_empty());
                assert_eq!(private_key_inline_ref.as_deref(), Some("inline-key"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn session_dto_never_serializes_authentication_or_attach_secrets() {
        let mut config = password_config("password-secret");
        let password_public =
            serde_json::to_value(crate::state::PublicConfig::from(&config)).unwrap();
        assert_eq!(password_public["auth"]["type"], "Password");
        assert_eq!(password_public["auth"]["has_password"], true);
        assert!(!password_public.to_string().contains("password-secret"));
        assert!(password_public["auth"].get("password_ref").is_none());

        config.auth = AuthConfig::Key {
            private_key_path: "/safe/path".into(),
            private_key_inline_ref: None,
            passphrase_ref: Some("passphrase-secret".into()),
        };
        let handle = SessionHandle::new(config, Vec::new(), Vec::new());
        *handle.attach_token.lock() = Some("attach-secret".into());
        let dto = SessionDto::new(&handle.config_snapshot(), handle.status());
        let value = serde_json::to_value(&dto).unwrap();
        let serialized = value.to_string();

        for forbidden in [
            "password_ref",
            "passphrase_ref",
            "password-secret",
            "passphrase-secret",
            "ai_attach_token",
            "attach-secret",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
        assert_eq!(value["config"]["auth"]["type"], "Key");
        assert_eq!(value["config"]["auth"]["private_key_path"], "/safe/path");
        assert_eq!(value["config"]["auth"]["has_password"], false);
        assert_eq!(value["config"]["auth"]["has_passphrase"], true);
    }

    #[test]
    fn all_arc_holders_observe_in_place_config_and_security_updates() {
        let handle = Arc::new(SessionHandle::new(
            password_config("old-secret"),
            Vec::new(),
            vec!["^confirm$".into()],
        ));
        let other_holder = handle.clone();
        assert_eq!(
            handle
                .security_engine
                .check("old-danger", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^old-danger$".into()
            }
        );

        update_handle_config(
            &handle,
            SessionConfigPatch {
                name: Some("shared-name".into()),
                ai_policy: Some(AIPolicyOverride::Permissive),
                ai_blocklist_regexes: Some(vec!["^new-danger$".into()]),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(Arc::ptr_eq(&handle, &other_holder));
        assert_eq!(other_holder.config_snapshot().name, "shared-name");
        assert_eq!(
            current_password(&other_holder.config_snapshot()),
            "old-secret"
        );
        assert_eq!(
            other_holder
                .security_engine
                .check("old-danger", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
        assert_eq!(
            other_holder
                .security_engine
                .check("new-danger", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^new-danger$".into()
            }
        );
        assert_eq!(
            other_holder
                .security_engine
                .check("confirm", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
    }
}

/// 关闭 SFTP 窗口（清理窗口注册表）
///
/// 这里只清理注册表，窗口关闭由 SFTP 窗口前端自行执行。
/// 若在窗口自己的 IPC 回调里同步调用 `win.close()`，WebView2 可能在等待 IPC 返回时
/// 被同步销毁，导致窗口与主进程互相等待而卡死。
#[tauri::command]
pub fn close_sftp_window(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    // `sftp_windows` 是 DashMap，读取锁下即可移除条目，不需要再申请写锁。
    let _ = state.inner.read().sftp_windows.remove(&session_id);
    Ok(())
}

/// 列出所有传输任务（跨窗口共享）
#[tauri::command]
pub fn list_transfers(state: State<'_, AppState>) -> Vec<crate::state::TransferJob> {
    state
        .inner
        .read()
        .transfers
        .iter()
        .map(|kv| kv.value().clone())
        .collect()
}

/// 暂停传输：保留 .part 与续传信息，原传输命令保持等待。
#[tauri::command]
pub fn pause_transfer(job_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::sftp_transfer::pause(&state, &job_id)
}

/// 恢复当前进程内暂停的传输。
#[tauri::command]
pub fn resume_transfer(job_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::sftp_transfer::resume(&state, &job_id)
}

/// 取消传输，等待 worker 停止后清理 .part 与续传信息。
#[tauri::command(async)]
pub async fn cancel_transfer(job_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::sftp_transfer::cancel(&state, &job_id).await
}

/// 失败任务使用新的 SSH/SFTP channel 重试，并按来源指纹决定是否续传。
#[tauri::command(async)]
pub async fn retry_transfer(job_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let session_id = state
        .inner
        .read()
        .transfers
        .get(&job_id)
        .map(|job| job.session_id.clone())
        .ok_or_else(|| "任务不存在".to_string())?;
    let config = sftp_config(&state, &session_id)?;
    crate::sftp_transfer::retry(&state, config, job_id).await
}
