//! 真实 OpenSSH/BusyBox 联调。仅在显式运行 ignored 测试并设置 CHENA_E2E_ROOT 时执行。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use russh::ChannelMsg;
use sha2::{Digest, Sha256};
use tauri::Manager;
use uuid::Uuid;

use crate::{
    session, sftp, sftp_transfer,
    state::{AIPolicyOverride, AppState, AuthConfig, SessionConfig, TransferStatus},
};

fn required_root() -> PathBuf {
    std::env::var_os("CHENA_E2E_ROOT")
        .map(PathBuf::from)
        .expect("真实联调需要设置 CHENA_E2E_ROOT")
}

fn remote_path(path: PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn config(root: &PathBuf) -> SessionConfig {
    SessionConfig {
        id: "real-openssh-e2e".into(),
        name: "真实 OpenSSH 联调".into(),
        host: std::env::var("CHENA_E2E_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("CHENA_E2E_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(22222),
        username: std::env::var("CHENA_E2E_USER").unwrap_or_else(|_| "chena".into()),
        auth: AuthConfig::Key {
            private_key_path: root.join("chena_e2e_key").to_string_lossy().into_owned(),
            private_key_inline_ref: None,
            passphrase_ref: None,
        },
        proxy_jump: Vec::new(),
        group: "e2e".into(),
        ai_allowed: false,
        ai_policy: AIPolicyOverride::Inherit,
        ai_blocklist_regexes: Vec::new(),
        auto_connect: false,
        keepalive_secs: 15,
        connect_timeout_secs: 10,
        port_forwards: Vec::new(),
        log_enabled: false,
    }
}

async fn exec(handle: &session::Handle, command: &str) -> Result<String, String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|error| format!("打开 exec channel 失败: {error}"))?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| format!("启动 exec 失败: {error}"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    if !stderr.is_empty() {
        return Err(format!(
            "远端命令 stderr: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the portable OpenSSH/BusyBox server and CHENA_E2E_ROOT"]
async fn russh_session_and_sftp_work_against_real_openssh() -> Result<(), String> {
    let root = required_root();
    let test_id = Uuid::new_v4().simple().to_string();
    let isolated_config = root.join(format!("rust-config-{test_id}"));
    let local_work = root.join("local").join(format!("rust 传输 {test_id}"));
    std::fs::create_dir_all(&local_work).map_err(|error| error.to_string())?;
    std::env::set_var("CHENA_SHELL_TEST_CONFIG_DIR", &isolated_config);

    tokio::time::timeout(Duration::from_secs(120), async move {
        let config = config(&root);
        let handle = session::connect_handle(&config).await?;
        let output = exec(&handle, "printf 'CHENA_RUSSH_OK\\n'; uname -s; df -k .").await?;
        let wrapped_output = exec(&handle, "printf 'CHENA_WRAPPED_OK\\n'").await?;
        assert!(
            wrapped_output.contains("CHENA_WRAPPED_OK"),
            "包装命令实际输出: {wrapped_output}"
        );
        assert!(output.contains("CHENA_RUSSH_OK"), "实际输出: {output}");
        assert!(
            output.to_ascii_lowercase().contains("busybox")
                || output.contains("Windows_NT")
                || output.contains("Linux"),
            "实际输出: {output}"
        );
        drop(handle);

        let known_hosts = crate::host_key::known_hosts_path();
        assert!(known_hosts.starts_with(&isolated_config));
        assert!(known_hosts.is_file(), "首次连接未记录 TOFU 主机密钥");
        session::connect_handle(&config)
            .await
            .map_err(|error| format!("相同主机密钥的第二次连接失败: {error}"))?;

        let remote_dir = format!(
            "{}/rust 中文 空格 {test_id}",
            remote_path(root.join("remote"))
        );
        let remote_file = format!("{remote_dir}/大文件 upload.bin");
        let connection = sftp::connect(config.clone()).await?;
        assert!(!sftp::pwd(&connection.session).await?.is_empty());
        sftp::mkdir_p(&connection.session, &remote_dir).await?;

        let source = local_work.join("源 文件.bin");
        let downloaded = local_work.join("下载 文件.bin");
        let bytes = (0..(3 * 1024 * 1024 + 517))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        tokio::fs::write(&source, &bytes)
            .await
            .map_err(|error| format!("写入本地测试文件失败: {error}"))?;

        let state = AppState::new();
        sftp_transfer::upload(
            &state,
            config.clone(),
            source.to_string_lossy().into_owned(),
            remote_file.clone(),
        )
        .await?;
        let metadata = sftp::stat(&connection.session, &remote_file).await?;
        assert_eq!(metadata.size, bytes.len() as u64);
        sftp::chmod(&connection.session, &remote_file, "600").await?;

        sftp_transfer::download(
            &state,
            config.clone(),
            remote_file.clone(),
            downloaded.to_string_lossy().into_owned(),
        )
        .await?;
        let downloaded_bytes = tokio::fs::read(&downloaded)
            .await
            .map_err(|error| format!("读取下载文件失败: {error}"))?;
        assert_eq!(downloaded_bytes, bytes, "SFTP 下载内容与大文件源不一致");

        let jobs = state
            .inner
            .read()
            .transfers
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| job.status == TransferStatus::Done));
        assert!(jobs
            .iter()
            .all(|job| job.bytes_transferred == bytes.len() as u64));

        let names = sftp::list(&connection.session, &remote_dir)
            .await?
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["大文件 upload.bin".to_string()]);
        sftp::delete(&connection.session, &remote_dir).await?;
        drop(connection);
        tokio::fs::remove_dir_all(&local_work)
            .await
            .map_err(|error| format!("清理本地联调文件失败: {error}"))?;
        tokio::fs::remove_dir_all(&isolated_config)
            .await
            .map_err(|error| format!("清理隔离配置失败: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "真实 OpenSSH/SFTP 联调超过 120 秒".to_string())?
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("真实联调需要设置 {name}"))
}

fn use_config_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    std::env::set_var("CHENA_SHELL_TEST_CONFIG_DIR", path);
    Ok(())
}

async fn assert_exec_ok(config: &SessionConfig, marker: &str) -> Result<(), String> {
    let handle = session::connect_handle(config).await?;
    let output = exec(&handle, &format!("printf '{marker}\\n'")).await?;
    assert!(output.contains(marker), "远端命令实际输出: {output}");
    Ok(())
}

async fn wait_transfer_job(
    state: &AppState,
    local_path: &str,
    remote_path: &str,
) -> Result<String, String> {
    for _ in 0..1000 {
        let job = state
            .inner
            .read()
            .transfers
            .iter()
            .find(|entry| entry.local_path == local_path && entry.remote_path == remote_path)
            .map(|entry| entry.id.clone());
        if let Some(id) = job {
            return Ok(id);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("等待 SFTP 传输任务创建超时".into())
}

async fn wait_transfer_bytes(state: &AppState, id: &str, minimum: u64) -> Result<u64, String> {
    for _ in 0..2000 {
        let job = state
            .inner
            .read()
            .transfers
            .get(id)
            .map(|job| job.clone())
            .ok_or_else(|| "传输任务在等待进度时消失".to_string())?;
        if job.bytes_transferred >= minimum {
            return Ok(job.bytes_transferred);
        }
        if job.status == TransferStatus::Error {
            return Err(job.error.unwrap_or_else(|| "传输任务失败".into()));
        }
        if job.status == TransferStatus::Done {
            return Err(format!(
                "传输在产生足够部分数据前已完成: {} 字节",
                job.bytes_transferred
            ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("等待 SFTP 部分传输进度超时".into())
}

async fn file_sha256(path: &Path) -> Result<String, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

async fn write_remote_text(
    connection: &russh_sftp::client::SftpSession,
    path: &str,
    content: &str,
) -> Result<(), String> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;

    let mut file = connection
        .open_with_flags(
            path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|error| format!("打开远端测试文件失败: {error}"))?;
    file.write_all(content.as_bytes())
        .await
        .map_err(|error| format!("写入远端测试文件失败: {error}"))?;
    file.close()
        .await
        .map_err(|error| format!("关闭远端测试文件失败: {error}"))
}

async fn read_remote_text(
    connection: &russh_sftp::client::SftpSession,
    path: &str,
) -> Result<String, String> {
    use tokio::io::AsyncReadExt;

    let mut file = connection
        .open(path)
        .await
        .map_err(|error| format!("打开远端测试文件失败: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("读取远端测试文件失败: {error}"))?;
    file.close()
        .await
        .map_err(|error| format!("关闭远端测试文件失败: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("远端测试文件不是 UTF-8: {error}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a real OpenSSH server whose host key is rotated between prepare and verify"]
async fn real_tofu_rotation_requires_fingerprint_binding() -> Result<(), String> {
    let root = required_root();
    let phase = required_env("CHENA_E2E_TOFU_PHASE");
    let isolated_config = root.join("tofu-config");
    if phase == "prepare" && isolated_config.exists() {
        std::fs::remove_dir_all(&isolated_config).map_err(|error| error.to_string())?;
    }
    use_config_dir(&isolated_config)?;
    let config = config(&root);

    if phase == "prepare" {
        return assert_exec_ok(&config, "CHENA_TOFU_PREPARED").await;
    }
    assert_eq!(
        phase, "verify",
        "CHENA_E2E_TOFU_PHASE 必须为 prepare 或 verify"
    );

    let error = match session::connect_handle(&config).await {
        Ok(_) => return Err("主机密钥变化后连接竟然成功，TOFU 未拒绝".into()),
        Err(error) => error,
    };
    let payload = error
        .strip_prefix("HOST_KEY_CHANGED:")
        .ok_or_else(|| format!("主机密钥变化错误格式不正确: {error}"))?;
    let challenge: crate::host_key::HostKeyChallenge = serde_json::from_str(payload)
        .map_err(|error| format!("解析主机密钥确认信息失败: {error}"))?;
    let before = std::fs::read_to_string(crate::host_key::known_hosts_path())
        .map_err(|error| error.to_string())?;
    assert!(crate::host_key::accept_changed_key(
        &challenge.host,
        challenge.port,
        &challenge.algorithm,
        &challenge.public_key,
        "SHA256:definitely-not-the-previous-key",
    )
    .is_err());
    let after_rejected = std::fs::read_to_string(crate::host_key::known_hosts_path())
        .map_err(|error| error.to_string())?;
    assert_eq!(before, after_rejected, "错误旧指纹不得替换 known_hosts");

    crate::host_key::accept_changed_key(
        &challenge.host,
        challenge.port,
        &challenge.algorithm,
        &challenge.public_key,
        &challenge.previous_fingerprint,
    )?;
    assert_exec_ok(&config, "CHENA_TOFU_ROTATION_ACCEPTED").await?;
    tokio::fs::remove_dir_all(&isolated_config)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CHENA_E2E_PASSWORD and a password-enabled real OpenSSH server"]
async fn real_password_auth_works() -> Result<(), String> {
    let root = required_root();
    let isolated_config = root.join(format!("password-config-{}", Uuid::new_v4().simple()));
    use_config_dir(&isolated_config)?;
    let mut config = config(&root);
    let password_ref = crate::secret_store::store_session_secret(
        &config.id,
        "password",
        &required_env("CHENA_E2E_PASSWORD"),
    )?;
    config.auth = AuthConfig::Password {
        password_ref: password_ref.clone(),
    };
    let result = assert_exec_ok(&config, "CHENA_PASSWORD_AUTH_OK").await;
    crate::secret_store::delete_reference(&password_ref)?;
    result?;
    tokio::fs::remove_dir_all(isolated_config)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CHENA_E2E_KEY_PATH and CHENA_E2E_PASSPHRASE for an encrypted private key"]
async fn real_encrypted_key_passphrase_auth_works() -> Result<(), String> {
    let root = required_root();
    let isolated_config = root.join(format!("passphrase-config-{}", Uuid::new_v4().simple()));
    use_config_dir(&isolated_config)?;
    let mut config = config(&root);
    let passphrase_ref = crate::secret_store::store_session_secret(
        &config.id,
        "passphrase",
        &required_env("CHENA_E2E_PASSPHRASE"),
    )?;
    config.auth = AuthConfig::Key {
        private_key_path: required_env("CHENA_E2E_KEY_PATH"),
        private_key_inline_ref: None,
        passphrase_ref: Some(passphrase_ref.clone()),
    };
    let result = assert_exec_ok(&config, "CHENA_PASSPHRASE_AUTH_OK").await;
    crate::secret_store::delete_reference(&passphrase_ref)?;
    result?;
    tokio::fs::remove_dir_all(isolated_config)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a Windows OpenSSH Agent or Pageant containing the E2E key"]
async fn real_agent_auth_works() -> Result<(), String> {
    let root = required_root();
    let isolated_config = root.join(format!("agent-config-{}", Uuid::new_v4().simple()));
    use_config_dir(&isolated_config)?;
    let mut config = config(&root);
    config.auth = AuthConfig::Agent;
    assert_exec_ok(&config, "CHENA_AGENT_AUTH_OK").await?;
    tokio::fs::remove_dir_all(isolated_config)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the real OpenSSH server for remote edit conflict and atomic-save tests"]
async fn real_sftp_remote_edit_conflict_and_atomic_save_work() -> Result<(), String> {
    let root = required_root();
    let test_id = Uuid::new_v4().simple().to_string();
    let isolated_config = root.join(format!("edit-config-{test_id}"));
    use_config_dir(&isolated_config)?;

    tokio::time::timeout(Duration::from_secs(120), async move {
        let config = config(&root);
        let session_id = config.id.clone();
        let state = AppState::new();
        let handle = Arc::new(session::SessionHandle::new(
            config.clone(),
            Vec::new(),
            Vec::new(),
        ));
        let shared = Arc::new(session::connect_handle(&config).await?);
        handle.install_shared_ssh_handle_for_test(shared);
        handle
            .connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state
            .inner
            .read()
            .sessions
            .insert(session_id.clone(), Arc::clone(&handle));
        let app = tauri::test::mock_app();
        app.manage(state);

        crate::commands::set_external_editor_enabled_for_test(false);

        let connection = sftp::connect_shared(Arc::clone(&handle)).await?;
        let remote_dir = format!(
            "{}/rust 远端编辑 {test_id}",
            remote_path(root.join("remote"))
        );
        sftp::mkdir_p(&connection.session, &remote_dir).await?;
        let remote_file = format!("{remote_dir}/冲突 编辑.txt");
        write_remote_text(&connection.session, &remote_file, "original\n").await?;

        // begin 应下载到本地缓存并留存原始备份，但不再返回内嵌 content；
        // 测试禁用编辑器后，后台任务会判定未修改并清理工作文件、保留备份。
        let begin =
            crate::commands::sftp_edit_begin(session_id.clone(), remote_file.clone(), app.state())
                .await?;
        let begin = serde_json::to_value(begin).map_err(|error| error.to_string())?;
        assert!(
            begin.get("content").is_none(),
            "外部编辑器流程不应返回内嵌 content"
        );
        assert_eq!(begin["remote_path"].as_str(), Some(remote_file.as_str()));
        let local_path = PathBuf::from(begin["local_path"].as_str().unwrap());
        let backup_path = PathBuf::from(begin["backup_path"].as_str().unwrap());
        assert!(
            local_path.is_file(),
            "未创建本地工作文件: {}",
            local_path.display()
        );
        assert!(
            backup_path.is_file(),
            "未创建原始备份文件: {}",
            backup_path.display()
        );

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!local_path.exists(), "未修改时应清理本地工作文件");
        assert!(backup_path.exists(), "原始备份文件必须保留");

        // 手动构造编辑记录，独立验证保存时的冲突检测与原子覆盖，
        // 不依赖 begin 后台任务的生命周期。
        let edited = "Chena-Shell 已确认保存\n第二行 UTF-8\n";
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        tokio::fs::write(&local_path, edited)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::copy(&local_path, &backup_path)
            .await
            .map_err(|error| error.to_string())?;
        let version = crate::commands::sftp_edit_remote_version(&connection.session, &remote_file)
            .await?
            .ok_or_else(|| "远端文件不存在".to_string())?;
        let manual_id = Uuid::new_v4().to_string();
        crate::commands::sftp_edit_records()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                manual_id.clone(),
                crate::commands::SftpEditRecord {
                    session_id: session_id.clone(),
                    remote_path: remote_file.clone(),
                    local_path: local_path.clone(),
                    backup_path: backup_path.clone(),
                    original: version,
                },
            );

        write_remote_text(&connection.session, &remote_file, "外部并发修改\n").await?;
        let conflict = crate::commands::sftp_edit_save(
            session_id.clone(),
            manual_id.clone(),
            edited.to_string(),
            version.size,
            version.mtime,
            false,
            app.state(),
        )
        .await
        .expect_err("未确认时必须拒绝覆盖外部修改");
        assert!(
            conflict.starts_with("SFTP_EDIT_CONFLICT:"),
            "实际冲突错误: {conflict}"
        );
        assert_eq!(
            read_remote_text(&connection.session, &remote_file).await?,
            "外部并发修改\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(&local_path)
                .await
                .map_err(|error| error.to_string())?,
            edited,
            "冲突后必须保留本地编辑"
        );

        crate::commands::sftp_edit_save(
            session_id.clone(),
            manual_id.clone(),
            edited.to_string(),
            version.size,
            version.mtime,
            true,
            app.state(),
        )
        .await?;
        assert_eq!(
            read_remote_text(&connection.session, &remote_file).await?,
            edited
        );
        let part_path = format!("{remote_file}.chena-edit-{manual_id}.part");
        let part_error = connection
            .session
            .symlink_metadata(&part_path)
            .await
            .expect_err("原子 rename 后不应残留编辑 .part 文件");
        assert!(sftp::is_not_found(&part_error), "实际错误: {part_error}");
        assert!(!local_path.exists(), "保存成功后必须删除本地工作文件");

        let second =
            crate::commands::sftp_edit_begin(session_id.clone(), remote_file.clone(), app.state())
                .await?;
        let second = serde_json::to_value(second).map_err(|error| error.to_string())?;
        let second_id = second["edit_id"].as_str().unwrap().to_string();
        let second_backup = PathBuf::from(second["backup_path"].as_str().unwrap());
        assert!(second_backup.is_file(), "第二次编辑未创建原始备份");
        crate::commands::sftp_edit_cancel(session_id.clone(), second_id).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(second_backup.exists(), "取消编辑后原始备份仍应保留");

        // 同一 SSH 连接上并发初始化多个 SFTP channel，信号量应排队而非被服务器拒绝。
        let begins = futures::future::join_all((0..12).map(|_| {
            crate::commands::sftp_edit_begin(session_id.clone(), remote_file.clone(), app.state())
        }))
        .await;
        for begin in begins {
            let begin = serde_json::to_value(begin?).map_err(|error| error.to_string())?;
            let edit_id = begin["edit_id"]
                .as_str()
                .ok_or_else(|| "并发编辑结果缺少 edit_id".to_string())?
                .to_string();
            crate::commands::sftp_edit_cancel(session_id.clone(), edit_id).await?;
        }

        crate::commands::set_external_editor_enabled_for_test(true);

        crate::commands::sftp_delete(session_id, remote_dir, app.state()).await?;
        drop(connection);
        handle.close();
        tokio::fs::remove_dir_all(&isolated_config)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .await
    .map_err(|_| "真实 SFTP 远端编辑联调超过 120 秒".to_string())?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CHENA_E2E_JUMP_PORTS with two real OpenSSH jump servers"]
async fn real_ordered_two_hop_proxy_jump_works() -> Result<(), String> {
    let root = required_root();
    let isolated_config = root.join(format!("jump-config-{}", Uuid::new_v4().simple()));
    use_config_dir(&isolated_config)?;
    let ports = required_env("CHENA_E2E_JUMP_PORTS")
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<u16>()
                .map_err(|error| format!("ProxyJump 端口无效: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(ports.len(), 2, "真实联调需要两个有序跳板端口");

    let suffix = Uuid::new_v4().simple().to_string();
    let mut jump_one = config(&root);
    jump_one.id = format!("jump-one-{suffix}");
    jump_one.port = ports[0];
    let mut jump_two = config(&root);
    jump_two.id = format!("jump-two-{suffix}");
    jump_two.port = ports[1];
    jump_two.proxy_jump = vec![jump_one.id.clone()];
    let mut target = config(&root);
    target.id = format!("target-{suffix}");
    target.proxy_jump = vec![jump_two.id.clone()];
    crate::state::save_persisted_sessions(&[jump_one, jump_two])?;

    let result = assert_exec_ok(&target, "CHENA_TWO_HOP_PROXY_OK").await;
    crate::state::save_persisted_sessions(&[])?;
    result?;
    tokio::fs::remove_dir_all(isolated_config)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the real OpenSSH/BusyBox server for transfer control and conflict tests"]
async fn real_sftp_transfer_controls_conflicts_and_tree_work() -> Result<(), String> {
    let root = required_root();
    let test_id = Uuid::new_v4().simple().to_string();
    let isolated_config = root.join(format!("transfer-config-{test_id}"));
    let local_work = root.join("local").join(format!("rust 控制 {test_id}"));
    std::fs::create_dir_all(&local_work).map_err(|error| error.to_string())?;
    use_config_dir(&isolated_config)?;

    tokio::time::timeout(Duration::from_secs(300), async move {
        let config = config(&root);
        let state = AppState::new();
        let connection = sftp::connect(config.clone()).await?;
        let remote_dir = format!(
            "{}/rust 控制 中文 {test_id}",
            remote_path(root.join("remote"))
        );
        sftp::mkdir_p(&connection.session, &remote_dir).await?;

        let conflict_source = local_work.join("冲突 源.txt");
        let conflict_download = local_work.join("冲突 下载.txt");
        tokio::fs::write(&conflict_source, b"original-chena-content")
            .await
            .map_err(|error| error.to_string())?;
        let conflict_remote = format!("{remote_dir}/冲突 目标.txt");
        sftp_transfer::upload(
            &state,
            config.clone(),
            conflict_source.to_string_lossy().into_owned(),
            conflict_remote.clone(),
        )
        .await?;
        let conflict_error = sftp_transfer::upload(
            &state,
            config.clone(),
            conflict_source.to_string_lossy().into_owned(),
            conflict_remote.clone(),
        )
        .await
        .expect_err("已有目标必须拒绝静默覆盖");
        assert!(
            conflict_error.contains("目标文件已存在"),
            "实际错误: {conflict_error}"
        );
        let failed_id = state
            .inner
            .read()
            .transfers
            .iter()
            .find(|entry| {
                entry.remote_path == conflict_remote && entry.status == TransferStatus::Error
            })
            .map(|entry| entry.id.clone())
            .ok_or_else(|| "冲突任务未保留为可重试失败状态".to_string())?;
        sftp::remove_file_if_exists(&connection.session, &conflict_remote).await?;
        sftp_transfer::retry(&state, config.clone(), failed_id).await?;
        sftp_transfer::download(
            &state,
            config.clone(),
            conflict_remote.clone(),
            conflict_download.to_string_lossy().into_owned(),
        )
        .await?;
        assert_eq!(
            tokio::fs::read(&conflict_download)
                .await
                .map_err(|error| error.to_string())?,
            b"original-chena-content"
        );

        let paused_source = local_work.join("暂停 源.bin");
        let paused_download = local_work.join("恢复 下载.bin");
        let bytes = (0..(64 * 1024 * 1024 + 333))
            .map(|index| (index % 253) as u8)
            .collect::<Vec<_>>();
        tokio::fs::write(&paused_source, &bytes)
            .await
            .map_err(|error| error.to_string())?;
        let source_hash = file_sha256(&paused_source).await?;
        let paused_remote = format!("{remote_dir}/暂停 恢复.bin");
        let paused_local_string = paused_source.to_string_lossy().into_owned();
        let task_state = state.clone();
        let task_config = config.clone();
        let task_local = paused_local_string.clone();
        let task_remote = paused_remote.clone();
        let upload_task = tokio::spawn(async move {
            sftp_transfer::upload(&task_state, task_config, task_local, task_remote).await
        });
        let paused_id = wait_transfer_job(&state, &paused_local_string, &paused_remote).await?;
        wait_transfer_bytes(&state, &paused_id, 2 * 64 * 1024).await?;
        sftp_transfer::pause(&state, &paused_id)?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let paused_bytes = state
            .inner
            .read()
            .transfers
            .get(&paused_id)
            .map(|job| (job.bytes_transferred, job.status))
            .ok_or_else(|| "暂停后任务消失".to_string())?;
        assert_eq!(paused_bytes.1, TransferStatus::Paused);
        assert!(
            paused_bytes.0 < bytes.len() as u64,
            "暂停发生得太晚，传输已完成"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let stable_bytes = state
            .inner
            .read()
            .transfers
            .get(&paused_id)
            .map(|job| job.bytes_transferred)
            .unwrap_or_default();
        assert_eq!(paused_bytes.0, stable_bytes, "暂停后仍持续传输数据");
        let part_metadata =
            sftp::stat(&connection.session, &format!("{paused_remote}.part")).await?;
        assert!(part_metadata.size > 0, "暂停后未保留远端 .part 文件");
        sftp_transfer::resume(&state, &paused_id)?;
        upload_task
            .await
            .map_err(|error| format!("上传任务 join 失败: {error}"))??;

        sftp_transfer::download(
            &state,
            config.clone(),
            paused_remote.clone(),
            paused_download.to_string_lossy().into_owned(),
        )
        .await?;
        assert_eq!(file_sha256(&paused_download).await?, source_hash);

        let cancelled_download = local_work.join("取消 下载.bin");
        let cancelled_local_string = cancelled_download.to_string_lossy().into_owned();
        let task_state = state.clone();
        let task_config = config.clone();
        let task_local = cancelled_local_string.clone();
        let task_remote = paused_remote.clone();
        let download_task = tokio::spawn(async move {
            sftp_transfer::download(&task_state, task_config, task_remote, task_local).await
        });
        let cancelled_id =
            wait_transfer_job(&state, &cancelled_local_string, &paused_remote).await?;
        wait_transfer_bytes(&state, &cancelled_id, 2 * 64 * 1024).await?;
        sftp_transfer::cancel(&state, &cancelled_id).await?;
        let cancel_result = download_task
            .await
            .map_err(|error| format!("下载任务 join 失败: {error}"))?;
        assert!(cancel_result.is_err(), "取消的工作线程不应返回成功");
        assert!(!cancelled_download.exists());
        assert!(!PathBuf::from(format!("{}.part", cancelled_download.display())).exists());
        assert!(
            !PathBuf::from(format!("{}.part.chena.json", cancelled_download.display())).exists()
        );
        assert!(state.inner.read().transfers.get(&cancelled_id).is_none());

        sftp_transfer::download(
            &state,
            config.clone(),
            paused_remote.clone(),
            cancelled_download.to_string_lossy().into_owned(),
        )
        .await?;
        assert_eq!(file_sha256(&cancelled_download).await?, source_hash);

        let nested_remote = format!("{remote_dir}/递归/空格 子目录");
        sftp::mkdir_p(&connection.session, &nested_remote).await?;
        for (name, content) in [("一.txt", "one"), ("二 三.txt", "two")] {
            let local = local_work.join(name);
            tokio::fs::write(&local, content)
                .await
                .map_err(|error| error.to_string())?;
            sftp_transfer::upload(
                &state,
                config.clone(),
                local.to_string_lossy().into_owned(),
                format!("{nested_remote}/{name}"),
            )
            .await?;
        }
        let tree = sftp::collect_tree(&connection.session, &format!("{remote_dir}/递归")).await?;
        assert!(tree
            .iter()
            .any(|node| node.is_dir && node.path.ends_with("空格 子目录")));
        assert!(tree
            .iter()
            .any(|node| !node.is_dir && node.path.ends_with("一.txt")));
        assert!(tree
            .iter()
            .any(|node| !node.is_dir && node.path.ends_with("二 三.txt")));

        sftp::delete(&connection.session, &remote_dir).await?;
        drop(connection);
        tokio::fs::remove_dir_all(&local_work)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::remove_dir_all(&isolated_config)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .await
    .map_err(|_| "真实 SFTP 控制联调超过 300 秒".to_string())?
}
