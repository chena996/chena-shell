use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use russh::keys::known_hosts::known_host_keys_path;
use russh::keys::{parse_public_key_base64, HashAlg, PublicKey, PublicKeyBase64};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CHANGED_PREFIX: &str = "HOST_KEY_CHANGED:";
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostKeyRecord {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub public_key: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HostKeyChallenge {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub public_key: String,
    pub fingerprint: String,
    pub previous_fingerprint: String,
}

pub fn known_hosts_path() -> PathBuf {
    crate::state::app_dir().join("known_hosts")
}

pub fn has_known_host(host: &str, port: u16) -> Result<bool, String> {
    known_host_keys_path(host, port, known_hosts_path())
        .map(|records| !records.is_empty())
        .map_err(|e| format!("读取主机密钥记录失败: {e}"))
}

pub fn verify_or_trust(host: &str, port: u16, key: &PublicKey) -> Result<HostKeyRecord, String> {
    verify_or_trust_at(host, port, key, &known_hosts_path())
}

fn key_record(host: &str, port: u16, key: &PublicKey) -> Result<HostKeyRecord, String> {
    let openssh = key
        .to_openssh()
        .map_err(|e| format!("无法编码服务器主机密钥: {e}"))?;
    let algorithm = openssh
        .split_whitespace()
        .next()
        .ok_or_else(|| "服务器主机密钥格式无效".to_string())?
        .to_string();
    Ok(HostKeyRecord {
        host: host.to_string(),
        port,
        algorithm,
        public_key: key.public_key_base64(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    })
}

fn verify_or_trust_at(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: &Path,
) -> Result<HostKeyRecord, String> {
    let _guard = STORE_LOCK.lock();
    let current = key_record(host, port, key)?;
    let recorded =
        known_host_keys_path(host, port, path).map_err(|e| format!("读取主机密钥记录失败: {e}"))?;

    if recorded.is_empty() {
        let mut content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("读取 known_hosts 失败: {error}")),
        };
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        let openssh = key
            .to_openssh()
            .map_err(|e| format!("无法编码服务器主机密钥: {e}"))?;
        content.push_str(&format!("{} {openssh}\n", host_label(host, port)));
        replace_file(path, content.as_bytes())?;
        tracing::info!(
            "首次连接已记录并信任主机密钥 {host}:{port} {}",
            current.fingerprint
        );
        return Ok(current);
    }
    if recorded.iter().any(|(_, known)| known == key) {
        return Ok(current);
    }

    let previous = recorded
        .first()
        .map(|(_, known)| known.fingerprint(HashAlg::Sha256).to_string())
        .unwrap_or_else(|| "未知".to_string());
    let challenge = HostKeyChallenge {
        host: current.host,
        port: current.port,
        algorithm: current.algorithm,
        public_key: current.public_key,
        fingerprint: current.fingerprint,
        previous_fingerprint: previous,
    };
    let payload =
        serde_json::to_string(&challenge).map_err(|e| format!("生成主机密钥确认信息失败: {e}"))?;
    Err(format!("{CHANGED_PREFIX}{payload}"))
}

fn host_label(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Replace a key only if the record still has the fingerprint shown in the confirmation dialog.
pub fn accept_changed_key(
    host: &str,
    port: u16,
    algorithm: &str,
    public_key: &str,
    expected_previous_fingerprint: &str,
) -> Result<HostKeyRecord, String> {
    accept_changed_key_at(
        host,
        port,
        algorithm,
        public_key,
        expected_previous_fingerprint,
        &known_hosts_path(),
    )
}

fn accept_changed_key_at(
    host: &str,
    port: u16,
    algorithm: &str,
    public_key: &str,
    expected_previous_fingerprint: &str,
    path: &Path,
) -> Result<HostKeyRecord, String> {
    let key = parse_public_key_base64(public_key)
        .map_err(|_| "主机公钥格式无效，未更新记录".to_string())?;
    let record = key_record(host, port, &key)?;
    if record.algorithm != algorithm {
        return Err("主机公钥与算法不一致，未更新记录".into());
    }

    let _guard = STORE_LOCK.lock();
    let recorded =
        known_host_keys_path(host, port, path).map_err(|e| format!("读取主机密钥记录失败: {e}"))?;
    let previous = recorded
        .first()
        .map(|(_, known)| known.fingerprint(HashAlg::Sha256).to_string())
        .ok_or_else(|| "主机密钥记录不存在，请重新连接".to_string())?;
    if previous != expected_previous_fingerprint {
        return Err("主机密钥记录已变化，请重新连接并核对最新指纹".into());
    }
    if recorded.iter().any(|(_, known)| known == &key) {
        return Ok(record);
    }

    let content = fs::read_to_string(path).map_err(|e| format!("读取 known_hosts 失败: {e}"))?;
    let label = host_label(host, port);
    let mut kept = String::new();
    for line in content.lines() {
        let is_target = line
            .split_whitespace()
            .next()
            .is_some_and(|hosts| hosts.split(',').any(|entry| entry == label));
        if !is_target {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept.push_str(&format!(
        "{label} {}\n",
        key.to_openssh().map_err(|e| e.to_string())?
    ));
    replace_file(path, kept.as_bytes())?;
    Ok(record)
}

#[cfg(not(windows))]
fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = temporary_path(path);
    write_temp(&tmp, bytes)?;
    fs::rename(&tmp, path).map_err(|e| format!("替换 known_hosts 失败: {e}"))
}

#[cfg(windows)]
fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = temporary_path(path);
    let backup = path.with_extension(format!("{}.bak", Uuid::new_v4()));
    write_temp(&tmp, bytes)?;
    let had_original = path.exists();
    if had_original {
        fs::rename(path, &backup).map_err(|e| format!("备份 known_hosts 失败: {e}"))?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => {
            if had_original {
                let _ = fs::remove_file(backup);
            }
            Ok(())
        }
        Err(e) => {
            if had_original {
                let _ = fs::rename(&backup, path);
            }
            Err(format!("替换 known_hosts 失败: {e}"))
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", Uuid::new_v4()))
}

fn write_temp(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建主机密钥目录失败: {e}"))?;
    }
    let mut file = fs::File::create(path).map_err(|e| format!("创建临时密钥记录失败: {e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("写入临时密钥记录失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_ONE: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_TWO: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G2sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("chena-shell-{name}-{}.known_hosts", Uuid::new_v4()))
    }

    fn key(value: &str) -> PublicKey {
        parse_public_key_base64(value).unwrap()
    }

    #[test]
    fn learns_then_accepts_the_same_key() {
        let path = test_path("same");
        let first = verify_or_trust_at("example.com", 22, &key(KEY_ONE), &path).unwrap();
        let second = verify_or_trust_at("example.com", 22, &key(KEY_ONE), &path).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_changed_key_without_overwriting() {
        let path = test_path("changed");
        verify_or_trust_at("example.com", 2222, &key(KEY_ONE), &path).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let error = verify_or_trust_at("example.com", 2222, &key(KEY_TWO), &path).unwrap_err();
        assert!(error.starts_with(CHANGED_PREFIX));
        let challenge: HostKeyChallenge =
            serde_json::from_str(error.trim_start_matches(CHANGED_PREFIX)).unwrap();
        assert_eq!(challenge.port, 2222);
        assert_ne!(challenge.fingerprint, challenge.previous_fingerprint);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_only_the_displayed_previous_fingerprint() {
        let path = test_path("accept");
        let original = verify_or_trust_at("example.com", 22, &key(KEY_ONE), &path).unwrap();
        let current = key_record("example.com", 22, &key(KEY_TWO)).unwrap();
        assert!(accept_changed_key_at(
            "example.com",
            22,
            &current.algorithm,
            &current.public_key,
            "SHA256:not-the-old-key",
            &path,
        )
        .is_err());
        let accepted = accept_changed_key_at(
            "example.com",
            22,
            &current.algorithm,
            &current.public_key,
            &original.fingerprint,
            &path,
        )
        .unwrap();
        assert_eq!(accepted, current);
        assert!(verify_or_trust_at("example.com", 22, &key(KEY_TWO), &path).is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn port_22_and_nonstandard_port_have_distinct_records() {
        let path = test_path("ports");
        verify_or_trust_at("example.com", 22, &key(KEY_ONE), &path).unwrap();
        verify_or_trust_at("example.com", 2222, &key(KEY_TWO), &path).unwrap();
        assert!(verify_or_trust_at("example.com", 22, &key(KEY_ONE), &path).is_ok());
        assert!(verify_or_trust_at("example.com", 2222, &key(KEY_TWO), &path).is_ok());
        let _ = fs::remove_file(path);
    }
}
