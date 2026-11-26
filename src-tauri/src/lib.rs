//! Chena-Shell — AI-friendly SSH terminal backend (by Chena)
//!
//! This is the Tauri backend. It manages SSH sessions, exposes them as an MCP Server,
//! and hosts resource monitors + security policy enforcement.

// 以下为既有实现约定的风格 lint，集中放行；CI 仍以 `-D warnings` 阻断其余警告。
#![allow(
    clippy::empty_line_after_doc_comments,
    clippy::type_complexity,
    clippy::manual_pattern_char_comparison,
    clippy::field_reassign_with_default,
    clippy::question_mark,
    clippy::manual_async_fn,
    clippy::derivable_impls
)]

mod approval;
mod commands;
mod forward;
mod host_key;
mod logging;
mod mcp_auth;
#[cfg(feature = "mcp-harness")]
mod mcp_harness;
mod mcp_protocol;
mod mcp_server;
mod mcp_stdio;
mod mcp_tools;
mod monitor;
#[cfg(test)]
mod real_openssh;
mod secret_store;
mod security;
mod session;
mod sftp;
mod sftp_log;
mod sftp_transfer;
mod state;

use state::AppState;
use tauri::{Manager, WindowEvent};
use tracing::info;

/// Run the stdio adapter without constructing Tauri or application session state.
pub fn run_mcp_stdio() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(mcp_stdio::run())
}

#[cfg(feature = "mcp-harness")]
pub fn run_mcp_harness() -> anyhow::Result<()> {
    mcp_harness::run()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    info!("Chena-Shell starting…");

    tauri::Builder::default()
        .setup(|app| {
            // Global state
            let app_state = AppState::new();
            app.manage(app_state.clone());
            state::register_app_handle(app.handle());

            // 主窗口销毁时，强制关闭所有派生窗口（SFTP / 内置编辑器等）并退出进程：
            // 「主窗口没了，程序就彻底关了」。用 destroy() 绕过子窗口的未保存确认，
            // 避免阻断退出；随后 exit(0) 结束整个进程。
            let exit_handle = app.handle().clone();
            if let Some(main) = app.get_webview_window("main") {
                main.on_window_event(move |event| {
                    if matches!(event, WindowEvent::Destroyed) {
                        for (label, win) in exit_handle.webview_windows() {
                            if label != "main" {
                                let _ = win.destroy();
                            }
                        }
                        exit_handle.exit(0);
                    }
                });
            }

            approval::run_expiry_loop(app_state.clone());
            // 恢复上次保存的会话列表，并准备统一日志目录、清理过期日志
            app_state.restore_sessions();
            let _ = logging::sessions_dir();
            let _ = logging::sftp_dir();
            let _ = logging::ai_dir();
            let _ = logging::ai_terminal_dir();
            logging::cleanup_logs(app_state.inner.read().settings.log_retention_days);
            commands::cleanup_edit_cache();

            // 仅在启用且允许自动启动时运行 MCP 服务。
            let should_start_mcp = {
                let inner = app_state.inner.read();
                inner.settings.mcp_enabled && inner.settings.mcp_autostart
            };
            if should_start_mcp {
                mcp_server::spawn_start(app_state.clone(), app.handle().clone());
            }

            // 每个会话使用独立监控连接，并通过 Tauri 事件推送快照。
            let state: AppState = (*app.state::<AppState>()).clone();
            monitor::run_loop(state, app.handle().clone());

            info!("Chena-Shell ready");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Session lifecycle
            commands::list_sessions,
            commands::create_session,
            commands::connect_session,
            commands::disconnect_session,
            commands::delete_session,
            commands::update_session,
            // Terminal I/O
            commands::terminal_input,
            commands::terminal_input_bytes,
            commands::terminal_resize,
            commands::open_session_channel,
            commands::attach_session_channel,
            commands::channel_input,
            commands::channel_input_bytes,
            commands::channel_resize,
            commands::close_session_channel,
            commands::set_active_terminal,
            commands::read_ai_terminal,
            // AI activity
            commands::list_ai_events,
            commands::kick_ai,
            // Monitor
            commands::get_monitor_snapshot,
            // SFTP
            commands::sftp_list,
            commands::sftp_pwd,
            commands::sftp_stat,
            commands::sftp_edit_begin,
            commands::sftp_edit_open,
            commands::sftp_edit_read,
            commands::sftp_edit_save,
            commands::sftp_edit_cancel,
            commands::sftp_edit_confirm_overwrite,
            commands::open_edit_window,
            commands::get_editor_prefs,
            commands::sftp_upload,
            commands::sftp_download,
            commands::sftp_chmod,
            commands::sftp_delete,
            commands::sftp_mkdir,
            commands::get_home_dir,
            commands::open_in_explorer,
            commands::read_local_dir,
            commands::read_local_dir_recursive,
            commands::read_local_file_base64,
            commands::sftp_download_to_local,
            commands::sftp_download_dir_to_local,
            commands::open_sftp_window,
            commands::close_sftp_window,
            commands::list_transfers,
            commands::pause_transfer,
            commands::resume_transfer,
            commands::cancel_transfer,
            commands::retry_transfer,
            // Settings
            commands::get_global_settings,
            commands::update_global_settings,
            commands::submit_sudo_password,
            commands::accept_host_key,
            commands::take_restore_sessions_status,
            // MCP
            commands::mcp_status,
            commands::generate_mcp_credential,
            commands::start_mcp,
            commands::stop_mcp,
            commands::restart_mcp,
            commands::list_confirmations,
            commands::resolve_confirmation,
            commands::set_confirmation_hold,
            // Port forwarding
            commands::list_forwards,
            commands::add_port_forward,
            commands::remove_port_forward,
            commands::start_port_forward,
            commands::stop_port_forward,
            // Logs
            commands::list_session_logs,
            commands::read_session_log,
            commands::list_ai_terminal_logs,
            commands::read_ai_terminal_log,
            commands::open_logs_dir,
            // Quick commands
            commands::get_ui_prefs,
            commands::save_ui_prefs,
            commands::list_quick_commands,
            commands::save_quick_command,
            commands::delete_quick_command,
            commands::run_quick_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Chena-Shell");
}
