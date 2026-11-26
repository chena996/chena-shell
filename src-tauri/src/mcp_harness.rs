//! 隔离的官方 MCP SDK 联调入口；只有显式启用 mcp-harness feature 才会编译。

use std::{sync::Arc, time::Duration};

use tauri::Manager;

use crate::{
    approval, mcp_auth, mcp_server,
    session::SessionHandle,
    state::{AIPolicyOverride, AppState, AuthConfig, SessionConfig},
};

fn real_openssh_session(state: &AppState) -> Option<Arc<SessionHandle>> {
    let root = std::env::var_os("CHENA_E2E_ROOT").map(std::path::PathBuf::from)?;
    let settings = state.inner.read().settings.clone();
    let config = SessionConfig {
        id: "mcp-real-openssh".into(),
        name: "MCP 真实 OpenSSH 联调".into(),
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
        ai_allowed: true,
        ai_policy: AIPolicyOverride::Inherit,
        ai_blocklist_regexes: Vec::new(),
        auto_connect: false,
        keepalive_secs: 15,
        connect_timeout_secs: 10,
        port_forwards: Vec::new(),
        log_enabled: false,
    };
    Some(Arc::new(SessionHandle::new(
        config,
        settings.global_blocklist,
        settings.confirm_list,
    )))
}

pub fn run() -> anyhow::Result<()> {
    let token = std::env::var("CHENA_MCP_TOKEN")
        .map_err(|_| anyhow::anyhow!("MCP harness 需要 CHENA_MCP_TOKEN"))?;
    let port = std::env::var("CHENA_MCP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(18767);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let app = tauri::Builder::default().build(tauri::generate_context!())?;
        let state = AppState::new();
        {
            let mut inner = state.inner.write();
            inner.settings.mcp_enabled = true;
            inner.settings.mcp_host = "127.0.0.1".into();
            inner.settings.mcp_port = port;
            inner.settings.mcp_require_attach_approval = true;
        }
        mcp_auth::install_test_credential(&state, &token);
        app.manage(state.clone());
        crate::state::register_app_handle(app.handle());

        if let Some(handle) = real_openssh_session(&state) {
            handle
                .clone()
                .connect(state.clone(), app.handle().clone())
                .map_err(|error| anyhow::anyhow!(error))?;
            state
                .inner
                .write()
                .sessions
                .insert("mcp-real-openssh".into(), handle);
        }

        let server_state = state.clone();
        let server_app = app.handle().clone();
        let server = tokio::spawn(async move { mcp_server::start(server_state, server_app).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if state.inner.read().mcp_listening {
                    break;
                }
                if server.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP harness 监听等待超时"))?;
        if !state.inner.read().mcp_listening {
            server.await??;
            return Err(anyhow::anyhow!("MCP harness 未能开始监听"));
        }

        if std::env::var("CHENA_MCP_HARNESS_AUTO_APPROVE").as_deref() == Ok("1") {
            let approval_state = state.clone();
            let terminal = approval_state
                .inner
                .read()
                .sessions
                .get("mcp-real-openssh")
                .map(|entry| entry.value().clone());
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(50));
                let mut replay_sequence = 0_u64;
                loop {
                    interval.tick().await;
                    for confirmation in approval::list_pending(&approval_state, None) {
                        if confirmation.kind == approval::ConfirmationKind::Attach {
                            if let Some(handle) = &terminal {
                                replay_sequence += 1;
                                let input = format!(
                                    "printf 'CHENA_MCP_TERMINAL_REPLAY_{replay_sequence}\\n'\n"
                                );
                                let _ = handle.send_input(&input);
                            }
                        }
                        let _ = approval::resolve(&approval_state, &confirmation.id, true);
                    }
                }
            });
        }
        println!("CHENA_MCP_HARNESS_READY port={port}");
        server.await??;
        Ok(())
    })
}
