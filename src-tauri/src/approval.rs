//! Shared, short-lived human approval queue for MCP operations.
//!
//! A confirmation is only a permit to consume an operation once. The original
//! command stays in memory and is never serialized to the UI; the UI receives a
//! redacted preview instead.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::state::{AIEventKind, AppState};

pub const CONFIRMATION_TIMEOUT_SECS: i64 = 30;
const MAX_PENDING_PER_SESSION: usize = 32;

fn emit_update(confirmation: &Confirmation) {
    crate::state::emit_app_event(
        "confirmation-update",
        serde_json::json!({
            "session_id": confirmation.session_id,
            "confirmation_id": confirmation.id,
            "status": confirmation.status,
        }),
    );
}

pub fn run_expiry_loop(state: AppState) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            expire_requests(&state);
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationKind {
    Attach,
    Exec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Consumed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Confirmation {
    pub id: String,
    pub kind: ConfirmationKind,
    pub session_id: String,
    pub client_id: String,
    pub client_name: String,
    pub reason: String,
    pub command_preview: Option<String>,
    #[serde(skip_serializing)]
    pub command: Option<String>,
    pub rule: Option<String>,
    /// root 执行的确认标记（方案 B）；普通执行的确认此字段为空。
    pub run_as: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// 用户正在查看/操作时暂停倒计时：记录暂停起点，恢复时把暂停时长补回 expires_at。
    #[serde(skip_serializing)]
    pub hold_since: Option<DateTime<Utc>>,
    pub status: ConfirmationStatus,
    pub resolution: Option<String>,
}

/// 部分 MCP 客户端会把中文原因编码成问号；避免审批窗口只显示一串 `?`。
///
/// 判定与前端 `isGarbledReason` 保持一致：问号累计不少于 4 个、问号占比超过一半，
/// 或以问号开头后接空格/结尾，都视为编码异常。中文后拼接 ASCII（如 `inode`）同样能识别。
fn reason_is_encoding_suspect(reason: &str) -> bool {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return false;
    }
    let total = trimmed.chars().count();
    let question_marks = trimmed.chars().filter(|c| *c == '?').count();
    let starts_with_question =
        trimmed.starts_with('?') && trimmed.chars().nth(1).map_or(true, |c| c.is_whitespace());
    question_marks >= 4
        || (total > 0 && question_marks as f64 / total as f64 > 0.5)
        || starts_with_question
}

fn normalize_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return "未提供可读原因".into();
    }
    if reason_is_encoding_suspect(trimmed) {
        "未提供可读原因（客户端编码异常）".into()
    } else {
        trimmed.to_string()
    }
}

fn is_expired(confirmation: &Confirmation) -> bool {
    confirmation.status == ConfirmationStatus::Pending
        && confirmation.hold_since.is_none()
        && confirmation.expires_at <= Utc::now()
}

fn audit_resolution(state: &AppState, confirmation: &Confirmation, expired: bool) {
    let command = if confirmation.kind == ConfirmationKind::Attach {
        "[attach]".into()
    } else {
        state.audit_command(confirmation.command.as_deref().unwrap_or(""))
    };
    let reason = confirmation
        .resolution
        .clone()
        .unwrap_or_else(|| "操作未获批准".into());
    let kind = if expired {
        AIEventKind::Expired { command }
    } else {
        AIEventKind::Rejected {
            command,
            reason: reason.clone(),
        }
    };
    state.log_ai_event_detailed(
        &confirmation.session_id,
        kind,
        None,
        None,
        &confirmation.client_name,
        confirmation.id.clone(),
        confirmation.rule.clone(),
        Some(reason),
        Some(confirmation.created_at),
    );
}

pub fn expire_requests(state: &AppState) {
    let mut expired = Vec::new();
    {
        let inner = state.inner.read();
        for mut entry in inner.confirmations.iter_mut() {
            if is_expired(entry.value()) {
                entry.value_mut().status = ConfirmationStatus::Expired;
                entry.value_mut().resolution = Some("30 秒未处理，已默认拒绝".into());
                expired.push(entry.value().clone());
            }
        }
        // Keep terminal states briefly for a client to learn the outcome, but do
        // not let a long-running process grow the map without bound.
        let cutoff = Utc::now() - chrono::Duration::minutes(5);
        inner.confirmations.retain(|_, confirmation| {
            confirmation.status == ConfirmationStatus::Pending || confirmation.created_at > cutoff
        });
    }
    for confirmation in expired {
        audit_resolution(state, &confirmation, true);
        emit_update(&confirmation);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn request(
    state: &AppState,
    kind: ConfirmationKind,
    session_id: &str,
    client_id: &str,
    client_name: &str,
    reason: &str,
    command: Option<String>,
    rule: Option<String>,
    run_as: Option<&str>,
) -> Result<Confirmation, String> {
    expire_requests(state);
    let inner = state.inner.read();
    let pending = inner
        .confirmations
        .iter()
        .filter(|entry| {
            entry.session_id == session_id && entry.status == ConfirmationStatus::Pending
        })
        .count();
    if pending >= MAX_PENDING_PER_SESSION {
        return Err("该会话待确认请求过多，请先处理现有请求".into());
    }
    let now = Utc::now();
    let reason_text = normalize_reason(reason);
    // 编码异常时只记录长度与标记，避免把客户端传来的问号原文写进审计日志。
    if reason_is_encoding_suspect(reason) && !cfg!(test) {
        let _ = crate::logging::append_daily_log(
            "ai",
            "ai-audit",
            &format!(
                "[AI] approval reason_length={} encoding_suspect=true",
                reason.trim().chars().count()
            ),
        );
    }
    let confirmation = Confirmation {
        id: format!("cf-{}", uuid::Uuid::new_v4().simple()),
        kind,
        session_id: session_id.into(),
        client_id: client_id.into(),
        client_name: client_name.into(),
        reason: reason_text,
        command_preview: command.as_deref().map(crate::logging::redact_text),
        command,
        rule,
        run_as: run_as.map(str::to_string),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(CONFIRMATION_TIMEOUT_SECS),
        hold_since: None,
        status: ConfirmationStatus::Pending,
        resolution: None,
    };
    inner
        .confirmations
        .insert(confirmation.id.clone(), confirmation.clone());
    drop(inner);
    emit_update(&confirmation);
    Ok(confirmation)
}

pub fn list_pending(state: &AppState, session_id: Option<&str>) -> Vec<Confirmation> {
    expire_requests(state);
    let inner = state.inner.read();
    let mut list = inner
        .confirmations
        .iter()
        .filter(|entry| {
            entry.status == ConfirmationStatus::Pending
                && session_id.map_or(true, |id| entry.session_id == id)
        })
        .map(|entry| entry.value().clone())
        .collect::<Vec<_>>();
    list.sort_by_key(|confirmation| confirmation.created_at);
    list
}

pub fn get(state: &AppState, id: &str) -> Option<Confirmation> {
    expire_requests(state);
    let inner = state.inner.read();
    inner
        .confirmations
        .get(id)
        .map(|entry| entry.value().clone())
}

pub fn resolve(state: &AppState, id: &str, approved: bool) -> Result<Confirmation, String> {
    expire_requests(state);
    let confirmation = {
        let inner = state.inner.read();
        let Some(mut entry) = inner.confirmations.get_mut(id) else {
            return Err("确认请求不存在或已清理".into());
        };
        if entry.status == ConfirmationStatus::Expired {
            return Err("确认请求已超时并默认拒绝".into());
        }
        if entry.status != ConfirmationStatus::Pending {
            return Err("确认请求已处理，不能重复操作".into());
        }
        entry.value_mut().status = if approved {
            ConfirmationStatus::Approved
        } else {
            ConfirmationStatus::Rejected
        };
        entry.value_mut().resolution = Some(if approved {
            "人工批准，仅可消费一次".into()
        } else {
            "人工拒绝".into()
        });
        entry.value().clone()
    };
    if !approved {
        audit_resolution(state, &confirmation, false);
    }
    emit_update(&confirmation);
    Ok(confirmation)
}

/// 暂停 / 恢复审批倒计时。
///
/// 用户在审批窗口内查看、点击或激活窗口时暂停，鼠标移出后恢复；
/// 恢复时把暂停时长补回 expires_at，保证用户有足够时间看完申请内容。
pub fn set_hold(state: &AppState, id: &str, hold: bool) -> Result<Option<Confirmation>, String> {
    let confirmation = {
        let inner = state.inner.read();
        let Some(mut entry) = inner.confirmations.get_mut(id) else {
            return Ok(None);
        };
        if entry.status != ConfirmationStatus::Pending {
            return Ok(None);
        }
        let now = Utc::now();
        if hold {
            entry.hold_since.get_or_insert(now);
        } else if let Some(since) = entry.hold_since.take() {
            let paused = now.signed_duration_since(since);
            if paused > chrono::Duration::zero() {
                entry.expires_at += paused;
            }
        }
        entry.value().clone()
    };
    emit_update(&confirmation);
    Ok(Some(confirmation))
}

/// Consume an approved request atomically. This is the only path that permits
/// an approved attach/exec to proceed, and it makes duplicate retries fail.
pub fn consume_approved(
    state: &AppState,
    id: &str,
    kind: ConfirmationKind,
    session_id: &str,
    client_id: &str,
) -> Result<Confirmation, String> {
    expire_requests(state);
    let inner = state.inner.read();
    let Some(mut entry) = inner.confirmations.get_mut(id) else {
        return Err("确认请求不存在或已清理".into());
    };
    if entry.kind != kind || entry.session_id != session_id || entry.client_id != client_id {
        return Err("确认请求与当前客户端或会话不匹配".into());
    }
    match entry.status {
        ConfirmationStatus::Pending => Err("WAIT_CONFIRM".into()),
        ConfirmationStatus::Expired => Err("确认请求已超时并默认拒绝".into()),
        ConfirmationStatus::Rejected => Err("操作已被人工拒绝".into()),
        ConfirmationStatus::Consumed => Err("确认请求已被消费，不能重复执行".into()),
        ConfirmationStatus::Approved => {
            entry.value_mut().status = ConfirmationStatus::Consumed;
            entry.value_mut().resolution = Some("已消费，执行一次".into());
            Ok(entry.value().clone())
        }
    }
}

pub fn cancel_session(state: &AppState, session_id: &str, reason: &str) {
    let cancelled = {
        let inner = state.inner.read();
        let mut cancelled = Vec::new();
        for mut entry in inner.confirmations.iter_mut() {
            if entry.session_id == session_id && entry.status == ConfirmationStatus::Pending {
                entry.value_mut().status = ConfirmationStatus::Rejected;
                entry.value_mut().resolution = Some(reason.into());
                cancelled.push(entry.value().clone());
            }
        }
        cancelled
    };
    for confirmation in cancelled {
        audit_resolution(state, &confirmation, false);
        emit_update(&confirmation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::GlobalSettings;

    fn state() -> AppState {
        AppState::new()
    }

    #[test]
    fn garbled_reason_is_replaced_with_readable_text() {
        assert_eq!(
            normalize_reason("????????????"),
            "未提供可读原因（客户端编码异常）"
        );
        assert_eq!(
            normalize_reason("  ?? ??  "),
            "未提供可读原因（客户端编码异常）"
        );
        assert_eq!(
            normalize_reason("????????? inode"),
            "未提供可读原因（客户端编码异常）"
        );
        assert_eq!(
            normalize_reason("???? 检查 nginx"),
            "未提供可读原因（客户端编码异常）"
        );
        assert_eq!(normalize_reason("检查 nginx 状态?"), "检查 nginx 状态?");
        assert_eq!(normalize_reason("read NIC stats"), "read NIC stats");
        assert_eq!(normalize_reason("读取网卡流量"), "读取网卡流量");
        assert_eq!(normalize_reason("检查 nginx 状态？"), "检查 nginx 状态？");
        assert_eq!(normalize_reason("   "), "未提供可读原因");
    }

    #[test]
    fn approval_is_bound_and_single_use() {
        let state = state();
        let confirmation = request(
            &state,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
            "test-client",
            "test",
            Some("echo secret".into()),
            Some("strict policy".into()),
            None,
        )
        .unwrap();
        assert_eq!(list_pending(&state, Some("s1")).len(), 1);
        resolve(&state, &confirmation.id, true).unwrap();
        let consumed = consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
        )
        .unwrap();
        assert_eq!(consumed.status, ConfirmationStatus::Consumed);
        assert!(consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
        )
        .is_err());
        assert!(consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Exec,
            "s1",
            "client-b",
        )
        .is_err());
    }

    #[test]
    fn sudo_confirmation_records_run_as_root() {
        let state = state();
        let confirmation = request(
            &state,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
            "test-client",
            "root exec",
            Some("ss -ulnp".into()),
            Some("sudo:confirm".into()),
            Some("root"),
        )
        .unwrap();
        assert_eq!(confirmation.run_as.as_deref(), Some("root"));
        assert_eq!(confirmation.status, ConfirmationStatus::Pending);
    }

    #[test]
    fn rejection_does_not_permit_execution() {
        let state = state();
        let confirmation = request(
            &state,
            ConfirmationKind::Attach,
            "s1",
            "client-a",
            "test-client",
            "test",
            None,
            None,
            None,
        )
        .unwrap();
        resolve(&state, &confirmation.id, false).unwrap();
        assert!(consume_approved(
            &state,
            &confirmation.id,
            ConfirmationKind::Attach,
            "s1",
            "client-a",
        )
        .is_err());
    }

    #[test]
    fn expired_request_is_denied_and_removed_from_pending() {
        let state = state();
        let confirmation = request(
            &state,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
            "test-client",
            "test",
            Some("echo test".into()),
            None,
            None,
        )
        .unwrap();
        state
            .inner
            .read()
            .confirmations
            .get_mut(&confirmation.id)
            .unwrap()
            .expires_at = Utc::now() - chrono::Duration::seconds(1);

        assert!(list_pending(&state, None).is_empty());
        assert_eq!(
            get(&state, &confirmation.id).unwrap().status,
            ConfirmationStatus::Expired
        );
        assert!(resolve(&state, &confirmation.id, true).is_err());
    }

    #[test]
    fn hold_pauses_expiry_and_resume_extends_deadline() {
        let state = state();
        let confirmation = request(
            &state,
            ConfirmationKind::Exec,
            "s1",
            "client-a",
            "test-client",
            "test",
            Some("echo test".into()),
            None,
            None,
        )
        .unwrap();
        // 模拟倒计时已归零。
        state
            .inner
            .read()
            .confirmations
            .get_mut(&confirmation.id)
            .unwrap()
            .expires_at = Utc::now() - chrono::Duration::seconds(1);

        // 用户正在查看：暂停期间不判超时，仍可批准。
        let held = set_hold(&state, &confirmation.id, true).unwrap().unwrap();
        assert_eq!(held.status, ConfirmationStatus::Pending);
        assert_eq!(list_pending(&state, None).len(), 1);

        // 暂停 3 秒后鼠标移出：到期时间顺延，恢复可操作。
        state
            .inner
            .read()
            .confirmations
            .get_mut(&confirmation.id)
            .unwrap()
            .hold_since = Some(Utc::now() - chrono::Duration::seconds(3));
        let released = set_hold(&state, &confirmation.id, false).unwrap().unwrap();
        assert_eq!(released.status, ConfirmationStatus::Pending);
        assert!(released.expires_at > Utc::now());
        assert_eq!(list_pending(&state, None).len(), 1);
        resolve(&state, &confirmation.id, true).unwrap();
    }

    #[test]
    fn default_settings_have_thirty_minute_token_lifetime() {
        assert_eq!(GlobalSettings::default().attach_token_ttl_minutes, 30);
    }
}
