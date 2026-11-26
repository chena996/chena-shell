//! SFTP 协议操作。远端路径始终使用 `/`，不经过远端 shell。

use russh_sftp::{
    client::{error::Error, SftpSession},
    protocol::{FileAttributes, StatusCode},
};
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;

use crate::{session, state::SessionConfig};

pub struct SftpConnection {
    // 字段按声明顺序析构：SFTP channel 必须先于 SSH handle 关闭，
    // permit 在 channel 之后释放，把名额还给会话级信号量。
    pub session: SftpSession,
    _handle: std::sync::Arc<session::Handle>,
    _permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub perms: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNode {
    pub path: String,
    pub is_dir: bool,
}

async fn open(handle: std::sync::Arc<session::Handle>) -> Result<SftpConnection, String> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开 SFTP channel 失败: {e}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("请求 SFTP subsystem 失败: {e}"))?;
    let mut sftp_config = russh_sftp::client::Config::default();
    // 每个 chunk flush 时只等待一个写请求，进度对应服务端已确认的字节。
    sftp_config.max_concurrent_writes = 1;
    let session = SftpSession::new_with_config(channel.into_stream(), sftp_config)
        .await
        .map_err(|e| format!("初始化 SFTP 失败: {e}"))?;
    Ok(SftpConnection {
        _handle: handle,
        session,
        _permit: None,
    })
}

pub fn connect(
    config: SessionConfig,
) -> impl std::future::Future<Output = Result<SftpConnection, String>> + Send + 'static {
    async move {
        tokio::task::spawn_blocking(move || {
            tauri::async_runtime::block_on(async move {
                let handle = std::sync::Arc::new(session::connect_handle(&config).await?);
                open(handle).await
            })
        })
        .await
        .map_err(|e| format!("SFTP 建连任务失败: {e}"))?
    }
}

/// 在已连接会话上打开独立 SFTP channel，不再重复 TCP 连接和 SSH 认证。
/// 同时占用一个会话级 SFTP 名额，避免单连接上 channel 数量超过服务器上限。
pub async fn connect_shared(
    session_handle: std::sync::Arc<session::SessionHandle>,
) -> Result<SftpConnection, String> {
    let permit = session_handle.acquire_sftp_permit().await?;
    let handle = session_handle.shared_ssh_handle()?;
    let mut connection = open(handle).await?;
    connection._permit = Some(permit);
    Ok(connection)
}

pub fn remote_join(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "." {
        return name.to_string();
    }
    if parent == "/" {
        return format!("/{name}");
    }
    format!("{}/{}", parent.trim_end_matches('/'), name)
}

pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(index) => trimmed[..index].to_string(),
    }
}

pub fn remote_file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn entry(name: String, metadata: &FileAttributes) -> SftpEntry {
    SftpEntry {
        name,
        is_dir: metadata.is_dir(),
        size: metadata.size.unwrap_or(0),
        mtime: metadata.mtime.unwrap_or(0) as i64,
        perms: format!("{:o}", metadata.permissions.unwrap_or(0) & 0o7777),
    }
}

pub async fn list(sftp: &SftpSession, path: &str) -> Result<Vec<SftpEntry>, String> {
    let path = if path.is_empty() { "." } else { path };
    let entries = sftp
        .read_dir(path)
        .await
        .map_err(|e| format!("列目录失败: {e}"))?;
    let mut result = entries
        .map(|item| entry(item.file_name(), &item.metadata()))
        .collect::<Vec<_>>();
    result.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(result)
}

pub async fn pwd(sftp: &SftpSession) -> Result<String, String> {
    sftp.canonicalize(".")
        .await
        .map_err(|e| format!("解析远端当前目录失败: {e}"))
}

pub async fn stat(sftp: &SftpSession, path: &str) -> Result<SftpEntry, String> {
    let metadata = sftp
        .symlink_metadata(path)
        .await
        .map_err(|e| format!("读取远端属性失败: {e}"))?;
    Ok(entry(remote_file_name(path), &metadata))
}

pub fn parse_mode(mode: &str) -> Result<u32, String> {
    if mode.is_empty() || mode.len() > 4 || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err("权限必须是 3 或 4 位八进制数字".into());
    }
    u32::from_str_radix(mode, 8).map_err(|_| "权限必须是 3 或 4 位八进制数字".into())
}

pub async fn chmod(sftp: &SftpSession, path: &str, mode: &str) -> Result<(), String> {
    let permissions = parse_mode(mode)?;
    sftp.set_metadata(
        path,
        FileAttributes {
            permissions: Some(permissions),
            ..FileAttributes::default()
        },
    )
    .await
    .map_err(|e| format!("chmod 失败 ({mode} {path}): {e}"))
}

pub fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::Status(status) if status.status_code == StatusCode::NoSuchFile)
}

pub async fn remove_file_if_exists(sftp: &SftpSession, path: &str) -> Result<(), String> {
    match sftp.remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(format!("删除 {path} 失败: {error}")),
    }
}

pub async fn mkdir_p(sftp: &SftpSession, path: &str) -> Result<(), String> {
    if path.is_empty() || path == "." || path == "/" {
        return Ok(());
    }
    let mut current = if path.starts_with('/') { "/" } else { "" }.to_string();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        current = remote_join(&current, component);
        if component == ".." {
            continue;
        }
        match sftp.symlink_metadata(current.clone()).await {
            Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {}
            Ok(_) => return Err(format!("创建目录失败: {current} 已存在且不是目录")),
            Err(error) if is_not_found(&error) => {
                sftp.create_dir(current.clone())
                    .await
                    .map_err(|e| format!("创建目录失败 ({current}): {e}"))?;
            }
            Err(error) => return Err(format!("检查目录失败 ({current}): {error}")),
        }
    }
    Ok(())
}

pub async fn delete(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let metadata = sftp
        .symlink_metadata(path)
        .await
        .map_err(|e| format!("读取待删除路径属性失败 ({path}): {e}"))?;
    delete_with_metadata(sftp, path, &metadata).await
}

fn delete_with_metadata<'a>(
    sftp: &'a SftpSession,
    path: &'a str,
    metadata: &'a FileAttributes,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        if metadata.is_dir() && !metadata.is_symlink() {
            let entries = sftp
                .read_dir(path)
                .await
                .map_err(|e| format!("读取待删除目录失败 ({path}): {e}"))?;
            for item in entries {
                let child = item.path();
                delete_with_metadata(sftp, &child, &item.metadata()).await?;
            }
            sftp.remove_dir(path)
                .await
                .map_err(|e| format!("删除目录失败 ({path}): {e}"))
        } else {
            sftp.remove_file(path)
                .await
                .map_err(|e| format!("删除文件失败 ({path}): {e}"))
        }
    })
}

pub async fn collect_tree(sftp: &SftpSession, root: &str) -> Result<Vec<RemoteNode>, String> {
    fn walk<'a>(
        sftp: &'a SftpSession,
        path: &'a str,
        output: &'a mut Vec<RemoteNode>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let entries = sftp
                .read_dir(path)
                .await
                .map_err(|e| format!("读取远端目录失败 ({path}): {e}"))?;
            for item in entries {
                let child = item.path();
                let metadata = item.metadata();
                let is_dir = metadata.is_dir() && !metadata.is_symlink();
                output.push(RemoteNode {
                    path: child.clone(),
                    is_dir,
                });
                if is_dir {
                    walk(sftp, &child, output).await?;
                }
            }
            Ok(())
        })
    }

    let metadata = sftp
        .symlink_metadata(root)
        .await
        .map_err(|e| format!("读取远端目录属性失败 ({root}): {e}"))?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(format!("远端路径不是目录: {root}"));
    }
    let mut output = Vec::new();
    walk(sftp, root, &mut output).await?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_never_use_windows_separators() {
        assert_eq!(remote_join("/home/test", "a.txt"), "/home/test/a.txt");
        assert_eq!(remote_join("/", "a.txt"), "/a.txt");
        assert_eq!(remote_join(".", "a.txt"), "a.txt");
        assert_eq!(remote_parent("/home/test/a.txt"), "/home/test");
        assert_eq!(remote_parent("/a.txt"), "/");
        assert_eq!(remote_parent("a.txt"), ".");
        assert_eq!(remote_file_name("/home/test/a.txt"), "a.txt");
        assert_eq!(remote_file_name("/"), "/");
    }

    #[test]
    fn chmod_mode_is_strictly_octal() {
        assert_eq!(parse_mode("755"), Ok(0o755));
        assert_eq!(parse_mode("4755"), Ok(0o4755));
        assert!(parse_mode("888").is_err());
        assert!(parse_mode("rm -rf").is_err());
        assert!(parse_mode("").is_err());
    }

    #[test]
    fn entry_uses_protocol_metadata() {
        let metadata = FileAttributes {
            size: Some(42),
            permissions: Some(0o040755),
            mtime: Some(123),
            ..FileAttributes::default()
        };
        let result = entry("demo".into(), &metadata);
        assert_eq!(result.size, 42);
        assert_eq!(result.perms, "755");
        assert!(result.is_dir);
    }
}
