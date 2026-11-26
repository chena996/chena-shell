//! SFTP 操作日志
//!
//! 独立写文件：%APPDATA%\chena-shell\logs\sftp\sftp-YYYY-MM-DD.log
//! 按天切分，纯文本，每行一条操作记录。

use chrono::Local;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub fn dir() -> Option<PathBuf> {
    Some(crate::logging::sftp_dir())
}

fn short_sid(s: &str) -> String {
    let s = s.strip_prefix("sess-").unwrap_or(s);
    s.chars().take(16).collect()
}

/// 追加一条操作日志
pub fn log(session_id: &str, op: &str, detail: &str) {
    let Some(dir) = dir() else { return };
    let _ = create_dir_all(&dir);
    let file = dir.join(format!("sftp-{}.log", Local::now().format("%Y-%m-%d")));
    let line = format!(
        "{} [{}] {:<11} {}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        short_sid(session_id),
        op,
        crate::logging::redact_text(detail)
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = f.write_all(line.as_bytes());
    }
}
