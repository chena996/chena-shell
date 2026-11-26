import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { localizeReason, localizeRule } from '../lib/aiLabels.js';

function remainingSeconds(deadline, now) {
  if (!Number.isFinite(deadline)) return null;
  return Math.max(0, Math.ceil((deadline - now) / 1000));
}

export default function ApprovalDialog({
  confirmation,
  session,
  queueLength = 0,
  busy = false,
  onResolve,
  onPostpone,
}) {
  const [deadline, setDeadline] = useState(() => new Date(confirmation?.expires_at || 0).getTime());
  const [paused, setPaused] = useState(false);
  const [now, setNow] = useState(Date.now());
  const pauseStartRef = useRef(0);
  const pausedRef = useRef(false);

  // 仅在切换到另一条审批时重置本地倒计时；后端顺延不影响本地暂停状态。
  useEffect(() => {
    setDeadline(new Date(confirmation?.expires_at || 0).getTime());
    pausedRef.current = false;
    setPaused(false);
    pauseStartRef.current = 0;
    setNow(Date.now());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [confirmation?.id]);

  // 暂停时冻结 now，倒计时自然停在原秒数。
  useEffect(() => {
    if (paused || !confirmation?.id) return undefined;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(timer);
  }, [paused, confirmation?.id]);

  const pauseCountdown = useCallback(() => {
    if (!confirmation?.id || pausedRef.current) return;
    pausedRef.current = true;
    pauseStartRef.current = Date.now();
    setPaused(true);
    invoke('set_confirmation_hold', { confirmationId: confirmation.id, hold: true }).catch(() => {});
  }, [confirmation?.id]);

  const resumeCountdown = useCallback(() => {
    if (!confirmation?.id || !pausedRef.current) return;
    pausedRef.current = false;
    const pausedFor = Date.now() - pauseStartRef.current;
    if (pausedFor > 0) setDeadline(value => value + pausedFor);
    setNow(Date.now());
    setPaused(false);
    invoke('set_confirmation_hold', { confirmationId: confirmation.id, hold: false }).catch(() => {});
  }, [confirmation?.id]);

  // 激活应用窗口时也暂停，避免用户刚切回窗口就被默认拒绝；离开窗口则继续。
  useEffect(() => {
    if (!confirmation?.id) return undefined;
    const onFocus = () => pauseCountdown();
    const onBlur = () => resumeCountdown();
    window.addEventListener('focus', onFocus);
    window.addEventListener('blur', onBlur);
    return () => {
      window.removeEventListener('focus', onFocus);
      window.removeEventListener('blur', onBlur);
    };
  }, [confirmation?.id, pauseCountdown, resumeCountdown]);

  if (!confirmation) return null;

  const isExec = confirmation.kind === 'exec';
  const isRoot = confirmation.run_as === 'root';
  const isSudoAttach = !isExec && String(confirmation.command_preview || '').includes('sudo');
  const needsRootNotice = isRoot || isSudoAttach;
  const seconds = remainingSeconds(deadline, now);
  const expired = seconds === 0;
  const sessionLabel = session
    ? `${session.name || session.host} · ${session.username}@${session.host}:${session.port}`
    : confirmation.session_id;
  const expiryLabel = new Date(deadline).toLocaleTimeString('zh-CN', { hour12: false });
  const labelStyle = {
    display: 'block', marginBottom: 5, fontSize: 11, fontWeight: 600,
    color: 'var(--md-on-surface-variant)', letterSpacing: '.04em',
  };
  const valueStyle = {
    padding: '9px 11px', borderRadius: 8, fontSize: 12, lineHeight: 1.55,
    background: 'var(--md-surface-container-low)', color: 'var(--md-on-surface)',
    border: '1px solid var(--md-outline-variant)', overflowWrap: 'anywhere',
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-dialog-title"
      onMouseEnter={pauseCountdown}
      onMouseLeave={resumeCountdown}
      style={{
        position: 'fixed', inset: 0, background: 'rgba(0,0,0,.56)', zIndex: 320,
        display: 'grid', placeItems: 'center', padding: 24, boxSizing: 'border-box',
      }}
    >
      <div style={{
        width: 600, maxWidth: '100%', maxHeight: 'calc(100vh - 48px)', overflow: 'auto',
        padding: 24, borderRadius: 18, boxSizing: 'border-box',
        background: 'var(--md-surface)', border: '1px solid var(--md-outline-variant)',
        boxShadow: '0 24px 64px rgba(0,0,0,.34)',
      }}>
        <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start', marginBottom: 18 }}>
          <div style={{
            width: 42, height: 42, borderRadius: 12, flexShrink: 0,
            display: 'grid', placeItems: 'center', color: '#B45309', background: '#F59E0B1f',
          }}>
            <span className="material-icons-round" style={{ fontSize: 24 }}>gpp_maybe</span>
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <h2 id="approval-dialog-title" style={{ margin: 0, fontSize: 18 }}>
              {isExec ? (isRoot ? 'AI 请求以 root 执行命令' : 'AI 命令执行审批') : (isSudoAttach ? 'AI 请求接入并申请 root 权限' : 'AI 接入审批')}
            </h2>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)', marginTop: 5 }}>
              {queueLength > 1 ? `共 ${queueLength} 条待处理 · ` : ''}
              {expired
                ? '已超时，正在等待默认拒绝生效'
                : paused
                  ? `倒计时已暂停（剩余 ${seconds ?? '—'} 秒）· 鼠标移出后继续`
                  : `请在 ${seconds ?? '—'} 秒内处理`}
            </div>
          </div>
          <span style={{
            padding: '3px 8px', borderRadius: 6, fontSize: 10, fontWeight: 700,
            color: expired ? '#B91C1C' : '#B45309', background: expired ? '#EF44441a' : '#F59E0B1a',
          }}>{expired ? '已过期' : '单次授权'}</span>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
          <div>
            <span style={labelStyle}>请求来源</span>
            <div style={valueStyle}>{confirmation.client_name || confirmation.client_id}</div>
          </div>
          <div>
            <span style={labelStyle}>目标会话</span>
            <div style={valueStyle}>{sessionLabel}</div>
          </div>
        </div>

        <div style={{ marginTop: 12 }}>
          <span style={labelStyle}>申请原因</span>
          <div style={valueStyle}>{localizeReason(confirmation.reason)}</div>
        </div>

        {isExec && (
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>命令预览（已脱敏）</span>
            <code style={{
              ...valueStyle, display: 'block', maxHeight: 130, overflow: 'auto',
              fontFamily: 'Consolas, monospace', whiteSpace: 'pre-wrap', userSelect: 'all',
            }}>{confirmation.command_preview || '（无命令预览）'}</code>
          </div>
        )}

        {needsRootNotice && (
          <div style={{
            marginTop: 12, padding: '10px 12px', borderRadius: 9, fontSize: 12, lineHeight: 1.6,
            background: '#EF44441a', border: '1px solid #EF444455', color: '#B91C1C',
          }}>
            <strong>执行身份：root（管理员）</strong>
            <div style={{ marginTop: 4, color: 'var(--md-on-surface-variant)' }}>
              {isExec
                ? '本批准仅对这一次命令有效；root 可读取与修改系统全部文件。'
                : '批准后 AI 可以 root 身份执行命令，但每条 root 命令仍需你单独确认。'}
            </div>
          </div>
        )}

        {confirmation.rule && (
          <div style={{ marginTop: 12 }}>
            <span style={labelStyle}>触发规则</span>
            <div style={valueStyle}>{localizeRule(confirmation.rule)}</div>
          </div>
        )}

        <div style={{
          margin: '16px 0 20px', padding: '10px 12px', borderRadius: 9,
          background: 'var(--md-surface-container-low)', fontSize: 11, lineHeight: 1.6,
          color: 'var(--md-on-surface-variant)', borderLeft: '3px solid #F59E0B',
        }}>
          到期时间：{expiryLabel}。鼠标移入审批窗口或激活应用窗口时倒计时暂停，移出后继续。未处理将默认拒绝；批准仅对当前请求生效一次，客户端需重试原请求，不能复用于其他命令或客户端。
        </div>

        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 10, flexWrap: 'wrap' }}>
          <button type="button" onClick={() => onPostpone(confirmation)} disabled={busy} style={{
            height: 38, padding: '0 16px', borderRadius: 10, cursor: 'pointer', marginRight: 'auto',
            border: '1px solid var(--md-outline)', background: 'transparent',
            color: 'var(--md-on-surface-variant)', fontFamily: 'inherit',
          }}>稍后处理</button>
          <button type="button" onClick={() => onResolve(confirmation, false)} disabled={busy || expired} style={{
            height: 38, padding: '0 18px', borderRadius: 10, cursor: 'pointer',
            border: '1px solid #DC2626', background: 'transparent', color: '#DC2626',
            fontFamily: 'inherit', fontWeight: 600, opacity: busy || expired ? .5 : 1,
          }}>拒绝</button>
          <button type="button" onClick={() => onResolve(confirmation, true)} disabled={busy || expired} style={{
            height: 38, padding: '0 18px', borderRadius: 10, border: 0,
            cursor: busy || expired ? 'not-allowed' : 'pointer', background: '#B45309', color: '#FFF',
            fontFamily: 'inherit', fontWeight: 600, opacity: busy || expired ? .55 : 1,
          }}>{busy ? '正在提交…' : '批准一次'}</button>
        </div>
      </div>
    </div>
  );
}
