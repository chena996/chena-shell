//! 端口转发 —— 支持两个方向
//!
//! - `Local`（等价 `ssh -L`）：本机监听 → direct-tcpip 通道 → 远端目标
//! - `Remote`（等价 `ssh -R`）：让服务器监听 → 服务器发起的 forwarded-tcpip 通道 → 本地目标
//!
//! 每条转发规则使用一条独立 SSH 连接 + 一个后台线程/单线程运行时。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use tokio::io::AsyncWriteExt;

use crate::session::{connect_handle, Handle};
use crate::state::{AppState, ForwardKind, PortForwardConfig, SessionConfig};

/// 远端转发目标表：(会话 ID, 服务器监听端口) → 本地目标
///
/// 以会话维度做键，两个会话使用同一远端端口时互不覆盖。
fn remote_targets() -> &'static DashMap<(String, u16), (String, u16)> {
    static TABLE: OnceLock<DashMap<(String, u16), (String, u16)>> = OnceLock::new();
    TABLE.get_or_init(DashMap::new)
}

/// 服务器发起 forwarded-tcpip 通道时，按会话与实际监听端口查本地目标
pub fn remote_target_lookup(session_id: &str, port: u32) -> Option<(String, u16)> {
    remote_targets()
        .get(&(session_id.to_string(), port as u16))
        .map(|g| g.value().clone())
}

fn remove_remote_target(session_id: &str, port: u16) {
    remote_targets().remove(&(session_id.to_string(), port));
}

/// 仅当表里登记的仍是本次启动的停止标志时才移除，避免误删重启后的新条目。
fn remove_forward_entry(st: &AppState, id: &str, stop: &Arc<AtomicBool>) {
    st.inner
        .read()
        .forwards
        .remove_if(id, |_, current| Arc::ptr_eq(current, stop));
}

/// 把服务器发起的转发通道接到本地目标（handler 内 spawn）
pub async fn pump_channel(
    channel: russh::Channel<russh::client::Msg>,
    host: String,
    port: u16,
    session_id: String,
    log_enabled: bool,
) {
    let log = |msg: &str| {
        if log_enabled {
            crate::logging::log_event(&session_id, "INFO", msg);
        }
    };

    log(&format!("远端转发：连接本地 {host}:{port} …"));
    let mut local = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(e) => {
            log(&format!("ERROR 远端转发：连接本地 {host}:{port} 失败: {e}"));
            let _ = channel.close().await;
            return;
        }
    };

    let remote = channel.into_stream();
    let (mut remote_r, mut remote_w) = tokio::io::split(remote);
    let (mut local_r, mut local_w) = local.split();

    // 远端 → 本地
    let up = async move {
        let n = tokio::io::copy(&mut remote_r, &mut local_w)
            .await
            .unwrap_or(0);
        let _ = local_w.shutdown().await;
        n
    };
    // 本地 → 远端：关闭前稍等，避免紧跟 EOF 的数据被丢弃
    let down = async move {
        let n = tokio::io::copy(&mut local_r, &mut remote_w)
            .await
            .unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = remote_w.shutdown().await;
        n
    };

    let (from_remote, to_remote) = tokio::join!(up, down);
    log(&format!(
        "远端转发：通道结束（远端→本地 {from_remote} 字节，本地→远端 {to_remote} 字节）"
    ));
}

fn log_line(cfg: &SessionConfig, message: &str) {
    if cfg.log_enabled {
        crate::logging::log_event(&cfg.id, "INFO", message);
    }
}

/// 启动一条转发（本地或远端）
pub fn start(state: &AppState, session_id: &str, fwd: PortForwardConfig) -> Result<(), String> {
    let cfg = match state.inner.read().sessions.get(session_id) {
        Some(g) => g.value().config_snapshot(),
        None => return Err("会话不存在".into()),
    };

    if state.inner.read().forwards.contains_key(&fwd.id) {
        return Err("该转发已在运行".into());
    }

    // 本地转发需要立即占用本地端口；远端转发由服务器分配
    let listener = if fwd.kind == ForwardKind::Local {
        let addr = format!("{}:{}", fwd.listen_host, fwd.listen_port);
        let l = std::net::TcpListener::bind(&addr).map_err(|e| format!("监听 {addr} 失败: {e}"))?;
        l.set_nonblocking(true)
            .map_err(|e| format!("设置非阻塞失败: {e}"))?;
        Some(l)
    } else {
        None
    };

    let stop = Arc::new(AtomicBool::new(false));
    let _ = state
        .inner
        .read()
        .forwards
        .insert(fwd.id.clone(), stop.clone());

    let spec = fwd.clone();
    let cleanup_state = state.clone();
    let cleanup_id = spec.id.clone();
    let cleanup_stop = stop.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => {
                remove_forward_entry(&cleanup_state, &cleanup_id, &cleanup_stop);
                return;
            }
        };

        rt.block_on(async move {
            let handle = match connect_handle(&cfg).await {
                Ok(h) => h,
                Err(e) => {
                    log_line(&cfg, &format!("ERROR 端口转发 SSH 连接失败: {e}"));
                    return;
                }
            };

            match spec.kind {
                ForwardKind::Remote => run_remote(handle, cfg, spec, stop).await,
                ForwardKind::Local => {
                    let Some(listener) = listener else { return };
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(l) => l,
                        Err(_) => return,
                    };
                    log_line(
                        &cfg,
                        &format!(
                            "本地转发已启动：{}:{} → 远端 {}:{}",
                            spec.listen_host, spec.listen_port, spec.dest_host, spec.dest_port
                        ),
                    );
                    let handle = Arc::new(handle);
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        tokio::select! {
                            accepted = listener.accept() => {
                                match accepted {
                                    Ok((local, peer)) => {
                                        let _ = local.set_nodelay(true);
                                        let handle = handle.clone();
                                        let dest_host = spec.dest_host.clone();
                                        let dest_port = spec.dest_port as u32;
                                        tokio::task::spawn(async move {
                                            bridge_direct(handle, local, peer.to_string(), dest_host, dest_port).await;
                                        });
                                    }
                                    Err(_) => break,
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                        }
                    }
                    log_line(&cfg, "本地转发已停止");
                }
            }
        });

        // 所有退出路径统一回收：SSH 失败、监听失败、正常停止都不会残留规则。
        remove_forward_entry(&cleanup_state, &cleanup_id, &cleanup_stop);
    });

    Ok(())
}

/// 远端转发：请求服务器监听，然后把进站通道接到本地目标
async fn run_remote(
    mut handle: Handle,
    cfg: SessionConfig,
    spec: PortForwardConfig,
    stop: Arc<AtomicBool>,
) {
    let bind = if spec.listen_host.trim().is_empty() {
        "0.0.0.0".to_string()
    } else {
        spec.listen_host.clone()
    };

    match handle
        .tcpip_forward(bind.clone(), spec.listen_port as u32)
        .await
    {
        Ok(assigned) => {
            // port != 0 时服务器返回 0，即实际监听端口就是我们请求的端口
            let actual_port = if assigned == 0 {
                spec.listen_port as u32
            } else {
                assigned
            };
            let _ = remote_targets().insert(
                (cfg.id.clone(), actual_port as u16),
                (spec.dest_host.clone(), spec.dest_port),
            );
            log_line(
                &cfg,
                &format!(
                    "远端转发已启动：{}:{} → 本地 {}:{}",
                    bind, actual_port, spec.dest_host, spec.dest_port
                ),
            );

            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            let _ = handle
                .cancel_tcpip_forward(bind.clone(), spec.listen_port as u32)
                .await;
            remove_remote_target(&cfg.id, actual_port as u16);
            log_line(&cfg, "远端转发已停止");
        }
        Err(e) => {
            log_line(&cfg, &format!("ERROR 远端转发启动失败: {e}"));
        }
    }
}

pub fn stop(state: &AppState, id: &str) {
    let flag = state.inner.read().forwards.remove(id).map(|(_, f)| f);
    if let Some(f) = flag {
        f.store(true, Ordering::Relaxed);
    }
}

/// 本地转发的单连接搬运（任一方向结束即关闭）
async fn bridge_direct(
    handle: Arc<Handle>,
    mut local: tokio::net::TcpStream,
    peer: String,
    dest_host: String,
    dest_port: u32,
) {
    let origin = peer.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
    let ch = match handle
        .channel_open_direct_tcpip(dest_host, dest_port, origin.to_string(), 0)
        .await
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut remote = ch.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_targets_are_isolated_per_session_and_port() {
        // 两个会话可以使用同一个远端端口而不互相覆盖。
        let _ = remote_targets().insert(
            ("forward-test-s1".to_string(), 18080),
            ("127.0.0.1".to_string(), 8000),
        );
        let _ = remote_targets().insert(
            ("forward-test-s2".to_string(), 18080),
            ("127.0.0.1".to_string(), 8001),
        );
        assert_eq!(
            remote_target_lookup("forward-test-s1", 18080),
            Some(("127.0.0.1".to_string(), 8000))
        );
        assert_eq!(
            remote_target_lookup("forward-test-s2", 18080),
            Some(("127.0.0.1".to_string(), 8001))
        );
        assert_eq!(remote_target_lookup("forward-test-s3", 18080), None);
        remove_remote_target("forward-test-s1", 18080);
        assert_eq!(remote_target_lookup("forward-test-s1", 18080), None);
        // 移除一个会话的规则不会影响另一个会话。
        assert_eq!(
            remote_target_lookup("forward-test-s2", 18080),
            Some(("127.0.0.1".to_string(), 8001))
        );
        remove_remote_target("forward-test-s2", 18080);
    }

    #[test]
    fn forward_entry_cleanup_only_removes_its_own_flag() {
        let state = AppState::new();
        let mine = Arc::new(AtomicBool::new(false));
        let other = Arc::new(AtomicBool::new(false));
        let _ = state
            .inner
            .read()
            .forwards
            .insert("fwd-test".to_string(), mine.clone());
        remove_forward_entry(&state, "fwd-test", &other);
        assert!(state.inner.read().forwards.contains_key("fwd-test"));
        remove_forward_entry(&state, "fwd-test", &mine);
        assert!(!state.inner.read().forwards.contains_key("fwd-test"));
    }
}
