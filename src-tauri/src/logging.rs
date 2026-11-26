//! Event logging — in-memory ring buffer per session + optional file append

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Thread-safe bounded ring buffer
pub struct EventRingBuffer<T> {
    inner: Mutex<VecDeque<T>>,
    capacity: usize,
}

impl<T> EventRingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn push(&self, event: T) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back(event);
    }

    pub fn snapshot(&self) -> Vec<T>
    where
        T: Clone,
    {
        let q = self.inner.lock().unwrap();
        q.iter().cloned().collect()
    }
}

// ================= 终端日志归档 =================

#[derive(serde::Serialize)]
pub struct LogFileInfo {
    pub name: String,
    pub size: u64,
    pub modified: i64,
}

/// 所有日志的统一根目录：%APPDATA%\chena-shell\logs
pub fn root_dir() -> std::path::PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(crate::state::APP_DIR)
        .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 终端（shell）会话日志：%APPDATA%\chena-shell\logs\sessions
pub fn sessions_dir() -> std::path::PathBuf {
    let dir = root_dir().join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// SFTP 操作日志：%APPDATA%\chena-shell\logs\sftp
pub fn sftp_dir() -> std::path::PathBuf {
    let dir = root_dir().join("sftp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// AI 审计日志：%APPDATA%\chena-shell\logs\ai
pub fn ai_dir() -> std::path::PathBuf {
    let dir = root_dir().join("ai");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// AI 回显日志：%APPDATA%\chena-shell\logs\ai\terminal
/// 记录 AI 只读标签中的命令、输出与状态，应用重启后仍可查阅。
pub fn ai_terminal_dir() -> std::path::PathBuf {
    let dir = ai_dir().join("terminal");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 兼容旧调用：终端会话日志目录。
pub fn logs_dir() -> std::path::PathBuf {
    sessions_dir()
}

/// 追加一行按天切分的日志，返回实际文件路径。
pub fn append_daily_log(
    folder: &str,
    prefix: &str,
    line: &str,
) -> std::io::Result<std::path::PathBuf> {
    let dir = root_dir().join(folder);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{prefix}-{}.log",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    write_line(&path, line)?;
    Ok(path)
}

/// 去掉 ANSI 转义序列，便于日志阅读
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    // 跳过 CSI 参数字节，直到终止字符（@ 到 ~）
                    for cc in chars.by_ref() {
                        if ('@'..='~').contains(&cc) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    // OSC 序列：以 BEL 或 ESC \ 结束（如窗口标题）
                    chars.next();
                    while let Some(cc) = chars.next() {
                        if cc == '\u{7}' {
                            break;
                        }
                        if cc == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if c == '\r' {
            // 丢弃 CR，保留 LF
        } else if c == '\u{7}' {
            // 丢弃 BEL
        } else {
            out.push(c);
        }
    }
    out
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

const MAX_PENDING_LINE_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct StreamRedactor {
    pending: String,
    in_private_key: bool,
}

impl StreamRedactor {
    fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let complete_len = self.pending.rfind('\n').map(|i| i + 1).unwrap_or(0);
        if complete_len == 0 {
            return self.suppress_overlong_line();
        }
        let complete = self.pending[..complete_len].to_string();
        self.pending.drain(..complete_len);
        let mut out = redact_complete_lines(&complete, &mut self.in_private_key);
        out.push_str(&self.suppress_overlong_line());
        out
    }

    fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        redact_complete_lines(&pending, &mut self.in_private_key)
    }

    fn suppress_overlong_line(&mut self) -> String {
        if self.pending.len() <= MAX_PENDING_LINE_BYTES {
            return String::new();
        }
        self.pending.clear();
        self.in_private_key = false;
        "[REDACTED: 超长无换行终端输出已抑制]\n".into()
    }
}

fn redactions() -> &'static [(regex::Regex, &'static str)] {
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            (r"(?s)-----BEGIN [^\r\n]*PRIVATE KEY-----.*?-----END [^\r\n]*PRIVATE KEY-----", "[REDACTED PRIVATE KEY]"),
            (r#"(?i)\b((?:password|passwd|passphrase|pwd|access_?token|refresh_?token|attach_?token|api_?key|token)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s;]+)"#, "${1}[REDACTED]"),
            (r"(?i)(\b(?:authorization\s*[:=]\s*)?bearer\s+)[A-Za-z0-9._~+/=-]+", "${1}[REDACTED]"),
            (r#"(?i)(--(?:password|passphrase|token|api-key)(?:=|\s+))(?:"[^"]*"|'[^']*'|[^\s;]+)"#, "${1}[REDACTED]"),
            (r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\b", "[REDACTED TOKEN]"),
            (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b|\bgh[pousr]_[A-Za-z0-9]{20,}\b|\bsk-[A-Za-z0-9_-]{20,}\b|\bxox[baprs]-[A-Za-z0-9-]{10,}\b", "[REDACTED KEY]"),
        ]
        .into_iter()
        .filter_map(|(pattern, replacement)| {
            regex::Regex::new(pattern)
                .ok()
                .map(|regex| (regex, replacement))
        })
        .collect()
    })
}

pub(crate) fn redact_text(text: &str) -> String {
    redactions()
        .iter()
        .fold(text.to_string(), |current, (regex, replacement)| {
            regex.replace_all(&current, *replacement).into_owned()
        })
}

fn pem_marker(line: &str, kind: &str) -> Option<(usize, usize)> {
    let start = line.find(&format!("-----{kind} "))?;
    let relative_end = line[start..].find(" PRIVATE KEY-----")?;
    Some((start, start + relative_end + " PRIVATE KEY-----".len()))
}

fn redact_complete_lines(input: &str, in_private_key: &mut bool) -> String {
    let mut out = String::with_capacity(input.len());
    for segment in input.split_inclusive('\n') {
        let mut line = segment;
        loop {
            if *in_private_key {
                let Some((_, end)) = pem_marker(line, "END") else {
                    break;
                };
                *in_private_key = false;
                line = &line[end..];
                continue;
            }
            if let Some((begin, _)) = pem_marker(line, "BEGIN") {
                out.push_str(&redact_text(&line[..begin]));
                out.push_str("[REDACTED PRIVATE KEY]");
                if let Some((_, end)) = pem_marker(&line[begin..], "END") {
                    line = &line[begin + end..];
                    continue;
                }
                *in_private_key = true;
                if line.ends_with('\n') {
                    out.push('\n');
                }
                break;
            }
            out.push_str(&redact_text(line));
            break;
        }
    }
    out
}

struct ActiveLog {
    path: std::path::PathBuf,
    redactor: StreamRedactor,
}

fn active_files() -> &'static Mutex<std::collections::HashMap<String, ActiveLog>> {
    static ACTIVE: OnceLock<Mutex<std::collections::HashMap<String, ActiveLog>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn active_path(session_id: &str) -> Option<std::path::PathBuf> {
    active_files()
        .lock()
        .unwrap()
        .get(session_id)
        .map(|active| active.path.clone())
}

/// AI 回显日志的活跃文件；与人工终端日志分开登记，互不覆盖。
struct ActiveAiLog {
    path: std::path::PathBuf,
    redactor: StreamRedactor,
}

fn active_ai_files() -> &'static Mutex<std::collections::HashMap<String, ActiveAiLog>> {
    static ACTIVE: OnceLock<Mutex<std::collections::HashMap<String, ActiveAiLog>>> =
        OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn ai_file_suffix(session_id: &str, host: &str, port: u16) -> String {
    format!(
        "_{}_ai_{}_{}.log",
        sanitize(session_id),
        sanitize(host),
        port
    )
}

fn ensure_ai_log(session_id: &str, host: &str, port: u16) -> Option<std::path::PathBuf> {
    let mut files = active_ai_files().lock().unwrap();
    if let Some(active) = files.get(session_id) {
        return Some(active.path.clone());
    }
    let path = ai_terminal_dir().join(format!(
        "{}{}",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        ai_file_suffix(session_id, host, port)
    ));
    files.insert(
        session_id.to_string(),
        ActiveAiLog {
            path: path.clone(),
            redactor: StreamRedactor::default(),
        },
    );
    drop(files);
    if write_line(
        &path,
        &format!("{} [INFO] === AI 回显日志开始 ===", stamp_full()),
    )
    .is_ok()
    {
        emit_log_appended(session_id, &path);
    }
    Some(path)
}

/// 追加 AI 只读标签的回显内容。始终记录，不受会话终端日志开关影响。
pub fn append_ai_terminal_log(session_id: &str, host: &str, port: u16, data: &str) {
    let clean = strip_ansi(data);
    if clean.trim().is_empty() {
        return;
    }
    let Some(path) = ensure_ai_log(session_id, host, port) else {
        return;
    };
    let output = {
        let mut files = active_ai_files().lock().unwrap();
        let Some(active) = files.get_mut(session_id) else {
            return;
        };
        active.redactor.push(&clean)
    };
    if !output.is_empty() && append_output(&path, &output).is_ok() {
        emit_log_appended(session_id, &path);
    }
}

/// 列出某个会话的 AI 回显日志。
pub fn list_ai_logs(session_id: &str, host: &str, port: u16) -> Vec<LogFileInfo> {
    let suffix = ai_file_suffix(session_id, host, port);
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(ai_terminal_dir()) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(&suffix) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                out.push(LogFileInfo {
                    name,
                    size: meta.len(),
                    modified,
                });
            }
        }
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// 读取某个会话 AI 回显日志的尾部（最多 200KB）。
pub fn read_ai_log(session_id: &str, host: &str, port: u16, name: &str) -> Result<String, String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("非法文件名".into());
    }
    if !name.ends_with(&ai_file_suffix(session_id, host, port)) {
        return Err("非法文件名".into());
    }
    let path = ai_terminal_dir().join(name);
    let bytes = std::fs::read(&path).map_err(|e| format!("读取失败: {e}"))?;
    Ok(decode_log_tail(&bytes, 204_800))
}

fn stamp_full() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn write_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    f.flush()
}

fn append_output(path: &std::path::Path, output: &str) -> std::io::Result<()> {
    use std::io::Write;
    if output.is_empty() {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let stamp = chrono::Local::now().format("%H:%M:%S");
    for line in output.split_inclusive('\n') {
        write!(f, "[{stamp}] {line}")?;
    }
    if !output.ends_with('\n') {
        writeln!(f)?;
    }
    f.flush()
}

fn emit_log_appended(session_id: &str, path: &std::path::Path) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::state::emit_app_event(
        "log-appended",
        serde_json::json!({"session_id": session_id, "name": name}),
    );
}

fn is_managed_log_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 20
        && bytes[..14].iter().all(u8::is_ascii_digit)
        && bytes[14] == b'_'
        && name.ends_with(".log")
        && !name.contains('/')
        && !name.contains('\\')
}

fn should_prune(modified: std::time::SystemTime, now: std::time::SystemTime, days: u32) -> bool {
    if days == 0 {
        return false;
    }
    let max_age = std::time::Duration::from_secs(u64::from(days.clamp(1, 365)) * 86_400);
    now.duration_since(modified)
        .map(|age| age > max_age)
        .unwrap_or(false)
}

/// AI 回显日志：`20261123001000_<会话>_ai_<主机>_<端口>.log`
fn is_ai_log_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 20
        && bytes[..14].iter().all(u8::is_ascii_digit)
        && bytes[14] == b'_'
        && name.contains("_ai_")
        && name.ends_with(".log")
        && !name.contains('/')
        && !name.contains('\\')
}

fn is_daily_log_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let Some(date) = rest.strip_suffix(".log") else {
        return false;
    };
    date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn prune_managed(
    dir: &std::path::Path,
    now: std::time::SystemTime,
    retention_days: u32,
    matches: impl Fn(&str) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !matches(&name)
            || !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        {
            continue;
        }
        let expired = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| should_prune(modified, now, retention_days))
            .unwrap_or(false);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 按保留天数清理所有日志类别（0 表示永久保留）。
pub fn cleanup_logs(retention_days: u32) {
    let now = std::time::SystemTime::now();
    prune_managed(&sessions_dir(), now, retention_days, is_managed_log_name);
    prune_managed(&ai_dir(), now, retention_days, |name| {
        is_daily_log_name(name, "ai-audit")
    });
    prune_managed(&ai_terminal_dir(), now, retention_days, is_ai_log_name);
    prune_managed(&sftp_dir(), now, retention_days, |name| {
        is_daily_log_name(name, "sftp")
    });
}

/// 开始记录一次连接：文件名包含 session id，同时保留旧 `_IP_PORT.log` 后缀兼容。
pub fn start_session_log_with_retention(
    session_id: &str,
    host: &str,
    port: u16,
    retention_days: u32,
) -> String {
    cleanup_logs(retention_days);
    let name = format!(
        "{}_{}_{}_{}.log",
        chrono::Local::now().format("%Y%m%d%H%M%S"),
        sanitize(session_id),
        sanitize(host),
        port
    );
    let path = logs_dir().join(&name);
    active_files().lock().unwrap().insert(
        session_id.to_string(),
        ActiveLog {
            path: path.clone(),
            redactor: StreamRedactor::default(),
        },
    );
    if write_line(&path, &format!("{} [INFO] === 日志开始 ===", stamp_full())).is_ok() {
        emit_log_appended(session_id, &path);
    }
    name
}

/// 记录一条事件（连接过程、认证结果、错误、断开等）
pub fn log_event(session_id: &str, level: &str, message: &str) {
    if let Some(path) = active_path(session_id) {
        if write_line(
            &path,
            &format!("{} [{level}] {}", stamp_full(), redact_text(message)),
        )
        .is_ok()
        {
            emit_log_appended(session_id, &path);
        }
    }
}

/// 结束日志：flush 最后一行，写入收尾行并取消登记
pub fn end_session_log(session_id: &str, reason: &str) {
    let active = active_files().lock().unwrap().remove(session_id);
    if let Some(mut active) = active {
        let tail = active.redactor.finish();
        let mut wrote = !tail.is_empty() && append_output(&active.path, &tail).is_ok();
        wrote |= write_line(
            &active.path,
            &format!(
                "{} [INFO] === 日志结束：{} ===",
                stamp_full(),
                redact_text(reason)
            ),
        )
        .is_ok();
        if wrote {
            emit_log_appended(session_id, &active.path);
        }
    }
}

/// 追加终端输出到当前连接的日志文件；不完整行保留到下一 chunk，避免跨段秘密泄露。
pub fn append_session_log(session_id: &str, data: &str) {
    let clean = strip_ansi(data);
    if clean.is_empty() {
        return;
    }
    let output = {
        let mut files = active_files().lock().unwrap();
        let Some(active) = files.get_mut(session_id) else {
            return;
        };
        (active.path.clone(), active.redactor.push(&clean))
    };
    if !output.1.is_empty() && append_output(&output.0, &output.1).is_ok() {
        emit_log_appended(session_id, &output.0);
    }
}

/// 文件名后缀：`_IP_PORT.log`
fn file_suffix(host: &str, port: u16) -> String {
    format!("_{}_{}.log", sanitize(host), port)
}

pub fn list_logs(host: &str, port: u16) -> Vec<LogFileInfo> {
    let suffix = file_suffix(host, port);
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(logs_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(&suffix) {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                out.push(LogFileInfo {
                    name,
                    size: meta.len(),
                    modified,
                });
            }
        }
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn decode_log_tail(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let cut = bytes.len() - max_bytes;
    let boundary = bytes[cut..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| cut + i + 1)
        .unwrap_or(cut);
    format!(
        "…（仅显示最后 200KB）\n{}",
        String::from_utf8_lossy(&bytes[boundary..])
    )
}

/// 读取日志文件尾部（最多 200KB）
pub fn read_log(host: &str, port: u16, name: &str) -> Result<String, String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("非法文件名".into());
    }
    if !name.ends_with(&file_suffix(host, port)) {
        return Err("非法文件名".into());
    }
    let path = logs_dir().join(name);
    let bytes = std::fs::read(&path).map_err(|e| format!("读取失败: {e}"))?;
    Ok(decode_log_tail(&bytes, 204_800))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_handles_csi_osc_and_control_characters() {
        assert_eq!(strip_ansi("\x1b[31m红色\x1b[0m"), "红色");
        assert_eq!(strip_ansi("前\x1b]0;标题\x07后"), "前后");
        assert_eq!(strip_ansi("前\x1b]0;标题\x1b\\后"), "前后");
        assert_eq!(strip_ansi("一\r\n二\x07三"), "一\n二三");
    }

    #[test]
    fn log_tail_preserves_short_chinese_text() {
        assert_eq!(decode_log_tail("中文日志".as_bytes(), 204_800), "中文日志");
    }

    #[test]
    fn log_tail_handles_a_cut_inside_a_chinese_character() {
        let bytes = "中".repeat(350_000).into_bytes();
        let decoded = decode_log_tail(&bytes, 204_800);

        assert!(decoded.starts_with("…（仅显示最后 200KB）\n"));
        assert!(decoded.contains('\u{fffd}'));
        assert!(decoded.ends_with('中'));
        assert!(decoded.len() < bytes.len());
    }

    #[test]
    fn log_tail_lossily_decodes_incomplete_utf8() {
        assert_eq!(decode_log_tail(&[b'a', 0xe4, 0xb8], 204_800), "a�");
    }

    #[test]
    fn redacts_common_secret_forms() {
        let input = "password=secret Authorization: Bearer abc.def.ghi token: \"attach-secret\" AKIAABCDEFGHIJKLMNOP";
        let output = redact_text(input);
        assert!(!output.contains("secret"));
        assert!(!output.contains("abc.def.ghi"));
        assert!(!output.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(output.contains("password=[REDACTED]"));
        assert!(output.contains("Bearer [REDACTED]"));
    }

    #[test]
    fn stream_redactor_handles_keywords_split_across_chunks() {
        let mut redactor = StreamRedactor::default();
        assert_eq!(redactor.push("before password=secr"), "");
        let output = redactor.push("et\nafter token=abc");
        assert_eq!(output, "before password=[REDACTED]\n");
        assert!(!redactor.finish().contains("abc"));
    }

    #[test]
    fn stream_redactor_suppresses_split_private_key_blocks() {
        let mut redactor = StreamRedactor::default();
        let first = redactor.push("before\n-----BEGIN OPENSSH PRIVATE KEY-----\nsecret-line\n");
        assert!(first.contains("before"));
        assert!(first.contains("[REDACTED PRIVATE KEY]"));
        assert!(!first.contains("secret-line"));
        let second = redactor.push("still-secret\n-----END OPENSSH PRIVATE KEY-----\nafter\n");
        assert!(second.contains("after"));
        assert!(!second.contains("still-secret"));
        assert!(!second.contains("OPENSSH PRIVATE KEY"));
    }

    #[test]
    fn retention_only_matches_managed_names_and_clamps_age() {
        assert!(is_managed_log_name(
            "20261119120000_sess-1_127.0.0.1_22.log"
        ));
        assert!(is_managed_log_name("20261119120000_127.0.0.1_22.log"));
        assert!(!is_managed_log_name("other.log"));
        assert!(!is_managed_log_name(
            "20261119120000_sess-1_127.0.0.1_22.log.txt"
        ));
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10 * 86_400);
        let old = now - std::time::Duration::from_secs(8 * 86_400);
        assert!(should_prune(old, now, 7));
        assert!(!should_prune(old, now, 0));
        assert!(!should_prune(now, now, 7));
    }

    #[test]
    fn ai_log_names_match_only_ai_terminal_files() {
        assert!(is_ai_log_name("20261123001000_sess-1_ai_127.0.0.1_22.log"));
        assert!(!is_ai_log_name("20261123001000_sess-1_127.0.0.1_22.log"));
        assert!(!is_ai_log_name("ai-audit-2026-11-23.log"));
        assert!(!is_ai_log_name(
            "20261123001000_sess-1_ai_127.0.0.1_22.log.bak"
        ));
    }

    #[test]
    fn daily_log_names_match_only_expected_prefixes() {
        assert!(is_daily_log_name("ai-audit-2026-11-22.log", "ai-audit"));
        assert!(is_daily_log_name("sftp-2026-11-22.log", "sftp"));
        assert!(!is_daily_log_name(
            "ai-audit-2026-11-22.log.bak",
            "ai-audit"
        ));
        assert!(!is_daily_log_name("other-2026-11-22.log", "ai-audit"));
        assert!(!is_daily_log_name("ai-audit-20261122.log", "ai-audit"));
    }
}
