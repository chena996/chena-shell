//! 有界流式 SFTP 传输、断点续传与任务控制。

use std::{
    collections::HashMap,
    ffi::OsString,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use russh_sftp::{
    client::SftpSession,
    protocol::{FileAttributes, OpenFlags},
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard},
};

use crate::{
    sftp,
    state::{AppState, SessionConfig, TransferDirection, TransferJob, TransferStatus},
};

const CHUNK_SIZE: usize = 64 * 1024;
const MAX_RESUME_METADATA: u64 = 16 * 1024;
const REMOTE_PART_SUFFIX: &str = ".part";
const RESUME_SUFFIX: &str = ".part.chena.json";
const TRANSFER_PROGRESS_EVENT: &str = "transfer-progress";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// 单次 SFTP 读写超时：黑洞网络下也能及时失败而不是永久挂起。
const SFTP_IO_TIMEOUT: Duration = Duration::from_secs(60);
/// 建立 SFTP 连接超时。
const SFTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlState {
    Running,
    Paused,
    Cancelled,
}

struct TransferControl {
    state: Mutex<ControlState>,
    changed: Notify,
    done: Notify,
    completed: AtomicBool,
}

impl TransferControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(ControlState::Running),
            changed: Notify::new(),
            done: Notify::new(),
            completed: AtomicBool::new(false),
        }
    }

    fn pause(&self) -> Result<(), String> {
        if self.completed.load(Ordering::Acquire) {
            return Err("任务已结束".into());
        }
        let mut state = lock(&self.state);
        if *state == ControlState::Cancelled {
            return Err("任务正在取消".into());
        }
        *state = ControlState::Paused;
        Ok(())
    }

    fn resume(&self) -> Result<(), String> {
        if self.completed.load(Ordering::Acquire) {
            return Err("任务已结束".into());
        }
        let mut state = lock(&self.state);
        if *state == ControlState::Cancelled {
            return Err("任务正在取消".into());
        }
        *state = ControlState::Running;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn cancel(&self) -> Result<(), String> {
        if self.completed.load(Ordering::Acquire) {
            return Err("任务已结束".into());
        }
        *lock(&self.state) = ControlState::Cancelled;
        self.changed.notify_waiters();
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        *lock(&self.state) == ControlState::Cancelled
    }

    async fn wait_ready(&self) -> Result<(), TransferError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // 先注册等待者再读状态，避免状态检查与 await 之间丢失恢复通知。
            changed.as_mut().enable();
            let state = { *lock(&self.state) };
            match state {
                ControlState::Running => return Ok(()),
                ControlState::Cancelled => return Err(TransferError::Cancelled),
                ControlState::Paused => changed.await,
            }
        }
    }

    fn finish(&self) {
        self.completed.store(true, Ordering::Release);
        self.done.notify_waiters();
    }

    async fn wait_done(&self) {
        loop {
            let done = self.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            if self.completed.load(Ordering::Acquire) {
                return;
            }
            done.await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn controls() -> &'static Mutex<HashMap<String, std::sync::Arc<TransferControl>>> {
    static CONTROLS: OnceLock<Mutex<HashMap<String, std::sync::Arc<TransferControl>>>> =
        OnceLock::new();
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn target_locks() -> &'static Mutex<HashMap<String, std::sync::Arc<AsyncMutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<AsyncMutex<()>>>>> =
        OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct TargetGuard {
    key: String,
    _guard: OwnedMutexGuard<()>,
}

impl Drop for TargetGuard {
    fn drop(&mut self) {
        lock(target_locks()).remove(&self.key);
    }
}

fn acquire_target(key: &str) -> Result<TargetGuard, String> {
    let mut locks = lock(target_locks());
    if locks.contains_key(key) {
        return Err("同一目标已有传输在进行，未覆盖目标文件".into());
    }
    let mutex = std::sync::Arc::new(AsyncMutex::new(()));
    let guard = mutex
        .clone()
        .try_lock_owned()
        .map_err(|_| "同一目标已有传输在进行，未覆盖目标文件".to_string())?;
    locks.insert(key.to_string(), mutex);
    Ok(TargetGuard {
        key: key.to_string(),
        _guard: guard,
    })
}

#[derive(Debug, Clone)]
struct TransferSpec {
    session_id: String,
    direction: TransferDirection,
    local_path: String,
    remote_path: String,
}

impl TransferSpec {
    fn target_key(&self) -> String {
        let target = match self.direction {
            TransferDirection::Upload => &self.remote_path,
            TransferDirection::Download => &self.local_path,
        };
        format!("{}\0{:?}\0{target}", self.session_id, self.direction)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SourceFingerprint {
    size: u64,
    mtime: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ResumeMetadata {
    version: u32,
    source_path: String,
    source: SourceFingerprint,
}

impl ResumeMetadata {
    fn new(source_path: &str, source: SourceFingerprint) -> Self {
        Self {
            version: 1,
            source_path: source_path.to_string(),
            source,
        }
    }
}

fn can_resume(
    saved: Option<&ResumeMetadata>,
    source_path: &str,
    source: &SourceFingerprint,
    part_size: u64,
) -> bool {
    source.mtime.is_some()
        && part_size <= source.size
        && saved.is_some_and(|saved| {
            saved.version == 1 && saved.source_path == source_path && saved.source == *source
        })
}

fn remote_part_path(target: &str) -> String {
    format!("{target}{REMOTE_PART_SUFFIX}")
}

fn remote_resume_path(target: &str) -> String {
    format!("{target}{RESUME_SUFFIX}")
}

fn local_path_with_suffix(target: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(target.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn local_part_path(target: &Path) -> PathBuf {
    local_path_with_suffix(target, REMOTE_PART_SUFFIX)
}

fn local_resume_path(target: &Path) -> PathBuf {
    local_path_with_suffix(target, RESUME_SUFFIX)
}

fn ensure_target_absent(exists: bool) -> Result<(), String> {
    if exists {
        Err("目标文件已存在；为避免覆盖，传输未开始".into())
    } else {
        Ok(())
    }
}

fn progress_event_times() -> &'static Mutex<HashMap<String, Instant>> {
    static EVENTS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn should_emit_progress(force: bool, last: Option<Instant>, now: Instant) -> bool {
    force || last.map_or(true, |last| now.duration_since(last) >= PROGRESS_INTERVAL)
}

fn emit_transfer_job(state: &AppState, job_id: &str) {
    let job = state
        .inner
        .read()
        .transfers
        .get(job_id)
        .map(|job| job.clone());
    if let Some(job) = job {
        if let Ok(payload) = serde_json::to_value(job) {
            crate::state::emit_app_event(TRANSFER_PROGRESS_EVENT, payload);
        }
    }
}

fn emit_progress(state: &AppState, job_id: &str, force: bool) {
    let now = Instant::now();
    {
        let mut events = lock(progress_event_times());
        if !should_emit_progress(force, events.get(job_id).copied(), now) {
            return;
        }
        events.insert(job_id.to_string(), now);
    }
    emit_transfer_job(state, job_id);
}

fn clear_progress_event(job_id: &str) {
    lock(progress_event_times()).remove(job_id);
}

fn job_start(state: &AppState, spec: &TransferSpec, size: u64) -> String {
    let id = format!("job-{}", uuid::Uuid::new_v4());
    let inner = state.inner.read();
    let _ = inner.transfers.insert(
        id.clone(),
        TransferJob {
            id: id.clone(),
            session_id: spec.session_id.clone(),
            direction: spec.direction,
            local_path: spec.local_path.clone(),
            remote_path: spec.remote_path.clone(),
            size,
            bytes_transferred: 0,
            status: TransferStatus::Running,
            error: None,
        },
    );
    if inner.transfers.len() > 60 {
        let stale = inner
            .transfers
            .iter()
            .filter(|entry| {
                matches!(
                    entry.value().status,
                    TransferStatus::Done | TransferStatus::Error
                )
            })
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        for key in stale {
            if inner.transfers.len() <= 60 {
                break;
            }
            inner.transfers.remove(&key);
        }
    }
    id
}

fn job_done(state: &AppState, id: &str, ok: bool, error: Option<String>) {
    if let Some(mut job) = state.inner.read().transfers.get_mut(id) {
        if ok && job.size == 0 {
            job.size = job.bytes_transferred;
        }
        job.status = if ok {
            TransferStatus::Done
        } else {
            TransferStatus::Error
        };
        job.error = error;
    }
    emit_progress(state, id, true);
    clear_progress_event(id);
}

fn update_progress(state: &AppState, id: &str, size: u64, bytes: u64) {
    if let Some(mut job) = state.inner.read().transfers.get_mut(id) {
        job.size = size;
        job.bytes_transferred = bytes;
    }
    emit_progress(state, id, false);
}

fn current_bytes(state: &AppState, id: &str) -> u64 {
    state
        .inner
        .read()
        .transfers
        .get(id)
        .map(|job| job.bytes_transferred)
        .unwrap_or(0)
}

#[derive(Debug)]
enum TransferError {
    Cancelled,
    Failed(String),
}

impl From<String> for TransferError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<&str> for TransferError {
    fn from(error: &str) -> Self {
        Self::Failed(error.to_string())
    }
}

pub fn upload(
    state: &AppState,
    config: SessionConfig,
    local_path: String,
    remote_path: String,
) -> impl std::future::Future<Output = Result<(), String>> + Send + '_ {
    async move {
        let spec = TransferSpec {
            session_id: config.id.clone(),
            direction: TransferDirection::Upload,
            local_path,
            remote_path,
        };
        run(state, config, spec, None).await
    }
}

pub fn download(
    state: &AppState,
    config: SessionConfig,
    remote_path: String,
    local_path: String,
) -> impl std::future::Future<Output = Result<(), String>> + Send + '_ {
    async move {
        let spec = TransferSpec {
            session_id: config.id.clone(),
            direction: TransferDirection::Download,
            local_path,
            remote_path,
        };
        run(state, config, spec, None).await
    }
}

async fn run(
    state: &AppState,
    config: SessionConfig,
    spec: TransferSpec,
    existing_job: Option<String>,
) -> Result<(), String> {
    let _target = acquire_target(&spec.target_key())?;
    let job_id = existing_job.unwrap_or_else(|| job_start(state, &spec, 0));
    if let Some(mut job) = state.inner.read().transfers.get_mut(&job_id) {
        job.status = TransferStatus::Running;
        job.error = None;
    }
    let control = std::sync::Arc::new(TransferControl::new());
    lock(controls()).insert(job_id.clone(), control.clone());
    emit_progress(state, &job_id, true);

    let result = execute(state, &config, &spec, &job_id, &control).await;
    lock(controls()).remove(&job_id);
    match result {
        Ok(bytes) => {
            update_progress(state, &job_id, bytes, bytes);
            job_done(state, &job_id, true, None);
            control.finish();
            Ok(())
        }
        Err(TransferError::Cancelled) => {
            let cleanup = cleanup_partial(&config, &spec).await;
            match cleanup {
                Ok(()) => {
                    clear_progress_event(&job_id);
                    let _ = state.inner.read().transfers.remove(&job_id);
                    control.finish();
                    Err("传输已取消，临时文件已清理".into())
                }
                Err(error) => {
                    let error = format!("取消清理失败，临时文件已保留供重试: {error}");
                    job_done(state, &job_id, false, Some(error.clone()));
                    control.finish();
                    Err(error)
                }
            }
        }
        Err(TransferError::Failed(_error)) if control.is_cancelled() => {
            match cleanup_partial(&config, &spec).await {
                Ok(()) => {
                    clear_progress_event(&job_id);
                    let _ = state.inner.read().transfers.remove(&job_id);
                }
                Err(cleanup_error) => job_done(
                    state,
                    &job_id,
                    false,
                    Some(format!(
                        "取消清理失败，临时文件已保留供重试: {cleanup_error}"
                    )),
                ),
            }
            control.finish();
            Err("传输已取消".into())
        }
        Err(TransferError::Failed(error)) => {
            job_done(state, &job_id, false, Some(error.clone()));
            control.finish();
            Err(error)
        }
    }
}

async fn execute(
    state: &AppState,
    config: &SessionConfig,
    spec: &TransferSpec,
    job_id: &str,
    control: &TransferControl,
) -> Result<u64, TransferError> {
    control.wait_ready().await?;
    match spec.direction {
        TransferDirection::Upload => execute_upload(state, config, spec, job_id, control).await,
        TransferDirection::Download => execute_download(state, config, spec, job_id, control).await,
    }
}

fn local_fingerprint(metadata: &std::fs::Metadata) -> SourceFingerprint {
    SourceFingerprint {
        size: metadata.len(),
        mtime: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
    }
}

fn remote_fingerprint(metadata: &FileAttributes) -> SourceFingerprint {
    SourceFingerprint {
        size: metadata.size.unwrap_or(0),
        mtime: metadata.mtime.map(u64::from),
    }
}

async fn remote_exists(sftp: &SftpSession, path: &str) -> Result<bool, String> {
    match sftp.symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if sftp::is_not_found(&error) => Ok(false),
        Err(error) => Err(format!("检查目标文件失败 ({path}): {error}")),
    }
}

async fn local_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("检查本地目标失败 ({}): {error}", path.display())),
    }
}

async fn remote_part_size(sftp: &SftpSession, path: &str) -> Result<u64, String> {
    match sftp.symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.is_dir() || metadata.is_symlink() {
                Err(format!("临时路径不是普通文件: {path}"))
            } else {
                Ok(metadata.size.unwrap_or(0))
            }
        }
        Err(error) if sftp::is_not_found(&error) => Ok(0),
        Err(error) => Err(format!("读取临时文件属性失败 ({path}): {error}")),
    }
}

async fn local_part_size(path: &Path) -> Result<u64, String> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.is_dir() || metadata.file_type().is_symlink() {
                Err(format!("临时路径不是普通文件: {}", path.display()))
            } else {
                Ok(metadata.len())
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!(
            "读取本地临时文件失败 ({}): {error}",
            path.display()
        )),
    }
}

async fn read_remote_resume(
    sftp: &SftpSession,
    path: &str,
) -> Result<Option<ResumeMetadata>, String> {
    let mut file = match sftp.open(path).await {
        Ok(file) => file,
        Err(error) if sftp::is_not_found(&error) => return Ok(None),
        Err(error) => return Err(format!("读取续传信息失败 ({path}): {error}")),
    };
    let mut bytes = Vec::new();
    let read = (&mut file)
        .take(MAX_RESUME_METADATA + 1)
        .read_to_end(&mut bytes)
        .await;
    let close = file.close().await;
    read.map_err(|e| format!("读取续传信息失败 ({path}): {e}"))?;
    close.map_err(|e| format!("关闭续传信息失败 ({path}): {e}"))?;
    if bytes.len() as u64 > MAX_RESUME_METADATA {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

async fn write_remote_resume(
    sftp: &SftpSession,
    path: &str,
    metadata: &ResumeMetadata,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(metadata).map_err(|e| format!("生成续传信息失败: {e}"))?;
    let mut file = sftp
        .open_with_flags(
            path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| format!("写入续传信息失败 ({path}): {e}"))?;
    file.write_all(&bytes)
        .await
        .map_err(|e| format!("写入续传信息失败 ({path}): {e}"))?;
    file.close()
        .await
        .map_err(|e| format!("关闭续传信息失败 ({path}): {e}"))
}

async fn read_local_resume(path: &Path) -> Result<Option<ResumeMetadata>, String> {
    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "读取本地续传信息失败 ({}): {error}",
                path.display()
            ))
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_RESUME_METADATA + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("读取本地续传信息失败 ({}): {e}", path.display()))?;
    if bytes.len() as u64 > MAX_RESUME_METADATA {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

async fn write_local_resume(path: &Path, metadata: &ResumeMetadata) -> Result<(), String> {
    let bytes = serde_json::to_vec(metadata).map_err(|e| format!("生成续传信息失败: {e}"))?;
    fs::write(path, bytes)
        .await
        .map_err(|e| format!("写入本地续传信息失败 ({}): {e}", path.display()))
}

async fn execute_upload(
    state: &AppState,
    config: &SessionConfig,
    spec: &TransferSpec,
    job_id: &str,
    control: &TransferControl,
) -> Result<u64, TransferError> {
    let source_metadata = fs::metadata(&spec.local_path)
        .await
        .map_err(|e| format!("读取本地源文件失败 ({}): {e}", spec.local_path))?;
    if !source_metadata.is_file() {
        return Err(format!("本地上传源不是文件: {}", spec.local_path).into());
    }
    let fingerprint = local_fingerprint(&source_metadata);
    update_progress(
        state,
        job_id,
        fingerprint.size,
        current_bytes(state, job_id),
    );

    let connection = tokio::time::timeout(SFTP_CONNECT_TIMEOUT, sftp::connect(config.clone()))
        .await
        .map_err(|_| format!("连接 SFTP 超时（{}s）", SFTP_CONNECT_TIMEOUT.as_secs()))??;
    let sftp = &connection.session;
    ensure_target_absent(remote_exists(sftp, &spec.remote_path).await?)?;
    sftp::mkdir_p(sftp, &sftp::remote_parent(&spec.remote_path)).await?;

    let part_path = remote_part_path(&spec.remote_path);
    let resume_path = remote_resume_path(&spec.remote_path);
    let part_size = remote_part_size(sftp, &part_path).await?;
    let saved = read_remote_resume(sftp, &resume_path).await?;
    let resume = can_resume(saved.as_ref(), &spec.local_path, &fingerprint, part_size);
    let mut offset = if resume { part_size } else { 0 };

    let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
    if !resume {
        flags |= OpenFlags::TRUNCATE;
    }
    let mut destination = sftp
        .open_with_flags(part_path.clone(), flags)
        .await
        .map_err(|e| format!("打开远端临时文件失败 ({part_path}): {e}"))?;
    if resume {
        destination
            .seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| format!("定位远端临时文件失败: {e}"))?;
    } else {
        let metadata = ResumeMetadata::new(&spec.local_path, fingerprint.clone());
        write_remote_resume(sftp, &resume_path, &metadata).await?;
    }

    let mut source = fs::File::open(&spec.local_path)
        .await
        .map_err(|e| format!("打开本地源文件失败 ({}): {e}", spec.local_path))?;
    source
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("定位本地源文件失败: {e}"))?;
    update_progress(state, job_id, fingerprint.size, offset);

    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        control.wait_ready().await?;
        let count = tokio::time::timeout(SFTP_IO_TIMEOUT, source.read(&mut buffer))
            .await
            .map_err(|_| format!("读取本地源文件超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
            .map_err(|e| format!("读取本地源文件失败: {e}"))?;
        if count == 0 {
            break;
        }
        tokio::time::timeout(SFTP_IO_TIMEOUT, destination.write_all(&buffer[..count]))
            .await
            .map_err(|_| format!("写入远端临时文件超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
            .map_err(|e| format!("写入远端临时文件失败: {e}"))?;
        tokio::time::timeout(SFTP_IO_TIMEOUT, destination.flush())
            .await
            .map_err(|_| format!("确认远端写入超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
            .map_err(|e| format!("确认远端写入失败: {e}"))?;
        offset += count as u64;
        if offset > fingerprint.size {
            return Err("本地源文件在传输期间增大，已保留临时文件".into());
        }
        update_progress(state, job_id, fingerprint.size, offset);
    }
    tokio::time::timeout(SFTP_IO_TIMEOUT, destination.close())
        .await
        .map_err(|_| format!("关闭远端临时文件超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
        .map_err(|e| format!("关闭远端临时文件失败: {e}"))?;
    if offset != fingerprint.size {
        return Err(format!(
            "本地源文件在传输期间变化（预期 {} 字节，实际 {offset} 字节）",
            fingerprint.size
        )
        .into());
    }
    let final_source = fs::metadata(&spec.local_path)
        .await
        .map_err(|e| format!("重新检查本地源文件失败: {e}"))?;
    if local_fingerprint(&final_source) != fingerprint {
        return Err("本地源文件的大小或修改时间已变化，已保留临时文件".into());
    }

    control.wait_ready().await?;
    ensure_target_absent(remote_exists(sftp, &spec.remote_path).await?)?;
    sftp.rename(part_path, spec.remote_path.clone())
        .await
        .map_err(|e| format!("完成上传时重命名失败: {e}"))?;
    sftp::remove_file_if_exists(sftp, &resume_path).await?;
    Ok(offset)
}

async fn execute_download(
    state: &AppState,
    config: &SessionConfig,
    spec: &TransferSpec,
    job_id: &str,
    control: &TransferControl,
) -> Result<u64, TransferError> {
    let connection = tokio::time::timeout(SFTP_CONNECT_TIMEOUT, sftp::connect(config.clone()))
        .await
        .map_err(|_| format!("连接 SFTP 超时（{}s）", SFTP_CONNECT_TIMEOUT.as_secs()))??;
    let sftp = &connection.session;
    let source_metadata = sftp
        .metadata(spec.remote_path.clone())
        .await
        .map_err(|e| format!("读取远端源文件失败 ({}): {e}", spec.remote_path))?;
    if source_metadata.is_dir() {
        return Err("远端下载源是目录，不是文件".into());
    }
    let fingerprint = remote_fingerprint(&source_metadata);
    update_progress(
        state,
        job_id,
        fingerprint.size,
        current_bytes(state, job_id),
    );

    let local_target = PathBuf::from(&spec.local_path);
    ensure_target_absent(local_exists(&local_target).await?)?;
    if let Some(parent) = local_target.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建本地目录失败 ({}): {e}", parent.display()))?;
    }
    let part_path = local_part_path(&local_target);
    let resume_path = local_resume_path(&local_target);
    let part_size = local_part_size(&part_path).await?;
    let saved = read_local_resume(&resume_path).await?;
    let resume = can_resume(saved.as_ref(), &spec.remote_path, &fingerprint, part_size);
    let mut offset = if resume { part_size } else { 0 };

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    if !resume {
        options.truncate(true);
    }
    let mut destination = options
        .open(&part_path)
        .await
        .map_err(|e| format!("打开本地临时文件失败 ({}): {e}", part_path.display()))?;
    if resume {
        destination
            .seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| format!("定位本地临时文件失败: {e}"))?;
    } else {
        let metadata = ResumeMetadata::new(&spec.remote_path, fingerprint.clone());
        write_local_resume(&resume_path, &metadata).await?;
    }

    let mut source = sftp
        .open(spec.remote_path.clone())
        .await
        .map_err(|e| format!("打开远端源文件失败: {e}"))?;
    source
        .seek(SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("定位远端源文件失败: {e}"))?;
    update_progress(state, job_id, fingerprint.size, offset);

    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        control.wait_ready().await?;
        let count = tokio::time::timeout(SFTP_IO_TIMEOUT, source.read(&mut buffer))
            .await
            .map_err(|_| format!("读取远端源文件超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
            .map_err(|e| format!("读取远端源文件失败: {e}"))?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .await
            .map_err(|e| format!("写入本地临时文件失败: {e}"))?;
        destination
            .flush()
            .await
            .map_err(|e| format!("写入本地临时文件失败: {e}"))?;
        offset += count as u64;
        if offset > fingerprint.size {
            return Err("远端源文件在传输期间增大，已保留临时文件".into());
        }
        update_progress(state, job_id, fingerprint.size, offset);
    }
    destination
        .flush()
        .await
        .map_err(|e| format!("完成本地临时文件写入失败: {e}"))?;
    drop(destination);
    tokio::time::timeout(SFTP_IO_TIMEOUT, source.close())
        .await
        .map_err(|_| format!("关闭远端源文件超时（{}s）", SFTP_IO_TIMEOUT.as_secs()))?
        .map_err(|e| format!("关闭远端源文件失败: {e}"))?;
    if offset != fingerprint.size {
        return Err(format!(
            "远端源文件在传输期间变化（预期 {} 字节，实际 {offset} 字节）",
            fingerprint.size
        )
        .into());
    }

    control.wait_ready().await?;
    ensure_target_absent(local_exists(&local_target).await?)?;
    fs::rename(&part_path, &local_target)
        .await
        .map_err(|e| format!("完成下载时重命名失败: {e}"))?;
    match fs::remove_file(&resume_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("清理本地续传信息失败 ({}): {error}", resume_path.display()).into())
        }
    }
    Ok(offset)
}

async fn cleanup_partial(config: &SessionConfig, spec: &TransferSpec) -> Result<(), String> {
    match spec.direction {
        TransferDirection::Upload => {
            // 原 worker 已停止；用全新的 SSH/SFTP channel 清理，避免复用失败连接。
            let connection =
                tokio::time::timeout(SFTP_CONNECT_TIMEOUT, sftp::connect(config.clone()))
                    .await
                    .map_err(|_| {
                        format!("连接 SFTP 超时（{}s）", SFTP_CONNECT_TIMEOUT.as_secs())
                    })??;
            let part = remote_part_path(&spec.remote_path);
            let resume = remote_resume_path(&spec.remote_path);
            let part_result = sftp::remove_file_if_exists(&connection.session, &part).await;
            let resume_result = sftp::remove_file_if_exists(&connection.session, &resume).await;
            part_result.and(resume_result)
        }
        TransferDirection::Download => {
            let target = PathBuf::from(&spec.local_path);
            let part = local_part_path(&target);
            let resume = local_resume_path(&target);
            let part_result = remove_local_if_exists(&part).await;
            let resume_result = remove_local_if_exists(&resume).await;
            part_result.and(resume_result)
        }
    }
}

async fn remove_local_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "删除本地临时文件失败 ({}): {error}",
            path.display()
        )),
    }
}

pub fn pause(state: &AppState, job_id: &str) -> Result<(), String> {
    let control = lock(controls()).get(job_id).cloned();
    let control = control.ok_or_else(|| "任务未在运行或已结束".to_string())?;
    control.pause()?;
    if let Some(mut job) = state.inner.read().transfers.get_mut(job_id) {
        job.status = TransferStatus::Paused;
        job.error = None;
    }
    emit_progress(state, job_id, true);
    Ok(())
}

pub fn resume(state: &AppState, job_id: &str) -> Result<(), String> {
    let control = lock(controls()).get(job_id).cloned();
    let control = control.ok_or_else(|| "暂停任务已不存在，请使用重试".to_string())?;
    control.resume()?;
    if let Some(mut job) = state.inner.read().transfers.get_mut(job_id) {
        job.status = TransferStatus::Running;
        job.error = None;
    }
    emit_progress(state, job_id, true);
    Ok(())
}

pub fn cancel<'a>(
    state: &'a AppState,
    job_id: &'a str,
) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
    async move {
        let control = lock(controls()).get(job_id).cloned();
        let control = control.ok_or_else(|| "任务未在运行或已结束".to_string())?;
        control.cancel()?;
        // 兜底：worker 卡在黑洞网络时，等待超时后强制标记失败并释放控制句柄。
        if tokio::time::timeout(Duration::from_secs(30), control.wait_done())
            .await
            .is_err()
        {
            control.finish();
            if let Some(mut job) = state.inner.read().transfers.get_mut(job_id) {
                job.status = TransferStatus::Error;
                job.error = Some("取消超时，传输线程未在 30 秒内退出".into());
            }
            lock(controls()).remove(job_id);
            return Err("取消超时，传输线程未在 30 秒内退出".into());
        }
        let job = state
            .inner
            .read()
            .transfers
            .get(job_id)
            .map(|job| job.clone());
        if let Some(job) = job {
            if job.status == TransferStatus::Error
                && job
                    .error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("取消清理失败"))
            {
                return Err(job.error.unwrap_or_else(|| "取消清理失败".into()));
            }
            if job.status != TransferStatus::Done {
                let _ = state.inner.read().transfers.remove(job_id);
            }
        }
        Ok(())
    }
}

pub fn retry(
    state: &AppState,
    config: SessionConfig,
    job_id: String,
) -> impl std::future::Future<Output = Result<(), String>> + Send + '_ {
    async move {
        let job = state
            .inner
            .read()
            .transfers
            .get(&job_id)
            .map(|job| job.clone())
            .ok_or_else(|| "任务不存在".to_string())?;
        if job.status != TransferStatus::Error {
            return Err("只有失败任务可以重试；暂停任务请使用恢复".into());
        }
        let spec = TransferSpec {
            session_id: job.session_id,
            direction: job.direction,
            local_path: job.local_path,
            remote_path: job.remote_path,
        };
        run(state, config, spec, Some(job_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(size: u64, mtime: Option<u64>) -> SourceFingerprint {
        SourceFingerprint { size, mtime }
    }

    #[test]
    fn part_paths_preserve_the_full_target_name() {
        assert_eq!(remote_part_path("/tmp/a.tar.gz"), "/tmp/a.tar.gz.part");
        assert_eq!(
            remote_resume_path("/tmp/a.tar.gz"),
            "/tmp/a.tar.gz.part.chena.json"
        );
        let target = PathBuf::from("C:\\data\\a.tar.gz");
        assert_eq!(
            local_part_path(&target),
            PathBuf::from("C:\\data\\a.tar.gz.part")
        );
        assert_eq!(
            local_resume_path(&target),
            PathBuf::from("C:\\data\\a.tar.gz.part.chena.json")
        );
    }

    #[test]
    fn resume_requires_identical_source_size_mtime_and_path() {
        let source = fingerprint(100, Some(123));
        let saved = ResumeMetadata::new("/source/a", source.clone());
        assert!(can_resume(Some(&saved), "/source/a", &source, 50));
        assert!(can_resume(Some(&saved), "/source/a", &source, 100));
        assert!(!can_resume(Some(&saved), "/source/a", &source, 101));
        assert!(!can_resume(
            Some(&saved),
            "/source/a",
            &fingerprint(101, Some(123)),
            50
        ));
        assert!(!can_resume(
            Some(&saved),
            "/source/a",
            &fingerprint(100, Some(124)),
            50
        ));
        assert!(!can_resume(Some(&saved), "/source/b", &source, 50));
        assert!(!can_resume(None, "/source/a", &source, 50));
        assert!(!can_resume(
            Some(&saved),
            "/source/a",
            &fingerprint(100, None),
            50
        ));
    }

    #[test]
    fn conflicts_are_never_silent_overwrites() {
        assert!(ensure_target_absent(false).is_ok());
        assert!(ensure_target_absent(true).is_err());
    }

    #[test]
    fn progress_is_throttled_and_terminal_events_bypass_throttling() {
        let start = Instant::now();
        assert!(should_emit_progress(false, None, start));
        assert!(!should_emit_progress(
            false,
            Some(start),
            start + Duration::from_millis(199)
        ));
        assert!(should_emit_progress(
            false,
            Some(start),
            start + PROGRESS_INTERVAL
        ));
        assert!(should_emit_progress(true, Some(start), start));
    }

    #[test]
    fn progress_payload_is_a_complete_transfer_job() {
        let payload = serde_json::to_value(TransferJob {
            id: "job-1".into(),
            session_id: "session-1".into(),
            direction: TransferDirection::Upload,
            local_path: "C:\\data\\a.txt".into(),
            remote_path: "/tmp/a.txt".into(),
            size: 100,
            bytes_transferred: 50,
            status: TransferStatus::Running,
            error: None,
        })
        .unwrap();
        assert_eq!(payload["id"], "job-1");
        assert_eq!(payload["session_id"], "session-1");
        assert_eq!(payload["direction"], "upload");
        assert_eq!(payload["local_path"], "C:\\data\\a.txt");
        assert_eq!(payload["remote_path"], "/tmp/a.txt");
        assert_eq!(payload["size"], 100);
        assert_eq!(payload["bytes_transferred"], 50);
        assert_eq!(payload["status"], "running");
        assert!(payload.get("error").is_some());
    }

    #[test]
    fn control_state_supports_pause_resume_and_cancel() {
        let control = TransferControl::new();
        control.pause().unwrap();
        assert_eq!(*lock(&control.state), ControlState::Paused);
        control.resume().unwrap();
        assert_eq!(*lock(&control.state), ControlState::Running);
        control.cancel().unwrap();
        assert!(control.is_cancelled());
        assert!(control.pause().is_err());
    }

    #[tokio::test]
    async fn resume_wakes_a_paused_worker() {
        let control = std::sync::Arc::new(TransferControl::new());
        control.pause().unwrap();
        let waiter = {
            let control = control.clone();
            tokio::spawn(async move { control.wait_ready().await })
        };
        tokio::task::yield_now().await;
        control.resume().unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("resume 后 worker 必须醒来")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn same_target_lock_rejects_concurrent_transfer() {
        let key = "test\0Upload\0/same-target";
        let guard = acquire_target(key).unwrap();
        assert!(acquire_target(key).is_err());
        drop(guard);
        let reacquired = acquire_target(key).unwrap();
        drop(reacquired);
    }
}
