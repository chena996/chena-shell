import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

const labelStyle = {
  display: 'block',
  fontSize: 11,
  fontWeight: 700,
  letterSpacing: '.02em',
  color: 'var(--md-on-surface-variant)',
  marginBottom: 4,
};

const valueStyle = {
  fontSize: 13,
  lineHeight: 1.6,
  color: 'var(--md-on-surface)',
  wordBreak: 'break-word',
};

/**
 * sudo 密码弹窗：AI 请求 root 执行且需要密码时由后端触发。
 * 密码只在本组件内存中短暂停留，提交后立即清空，不会交给 AI 或写入日志。
 */
export default function SudoPasswordDialog() {
  const [request, setRequest] = useState(null);
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const inputRef = useRef(null);

  useEffect(() => {
    let disposed = false;
    let unlisten = null;
    listen('sudo-password-request', event => {
      if (disposed) return;
      setPassword('');
      setError('');
      setRequest(event.payload || null);
      window.setTimeout(() => inputRef.current?.focus(), 30);
    })
      .then(fn => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      if (unlisten) unlisten();
    };
  }, []);

  if (!request) return null;

  const submit = async value => {
    if (busy) return;
    setBusy(true);
    setError('');
    try {
      const accepted = await invoke('submit_sudo_password', {
        requestId: request.request_id,
        password: value,
      });
      if (!accepted) {
        setError('请求已超时或已被取消，请让 AI 重新发起。');
        return;
      }
      setRequest(null);
      setPassword('');
    } catch {
      setError('提交失败，请重试。');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      role="presentation"
      style={{
        position: 'fixed', inset: 0, zIndex: 1200, padding: 24,
        display: 'grid', placeItems: 'center', background: 'rgba(0,0,0,.56)',
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="sudo-password-title"
        style={{
          width: 460, maxWidth: '100%', boxSizing: 'border-box', padding: 24,
          borderRadius: 18, background: 'var(--md-surface)',
          border: '1px solid var(--md-outline-variant)',
          boxShadow: '0 24px 64px rgba(0,0,0,.34)',
        }}
      >
        <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start', marginBottom: 16 }}>
          <div style={{
            width: 42, height: 42, borderRadius: 12, flexShrink: 0,
            display: 'grid', placeItems: 'center', color: '#B45309', background: '#F59E0B1f',
          }}>
            <span className="material-icons-round" style={{ fontSize: 24 }}>key</span>
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <h2 id="sudo-password-title" style={{ margin: 0, fontSize: 18 }}>需要 sudo 密码</h2>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)', marginTop: 5 }}>
              请在 {request.expires_in_seconds ?? 30} 秒内输入，超时将取消本次 root 执行
            </div>
          </div>
        </div>

        <div style={{ marginBottom: 14 }}>
          <span style={labelStyle}>目标主机</span>
          <div style={valueStyle}>
            <strong>{request.username}</strong>@{request.host}:{request.port}
          </div>
        </div>

        <form onSubmit={event => { event.preventDefault(); if (password) submit(password); }}>
          <label htmlFor="sudo-password-input" style={labelStyle}>sudo 密码</label>
          <input
            ref={inputRef}
            id="sudo-password-input"
            type="password"
            autoComplete="off"
            value={password}
            onChange={event => setPassword(event.target.value)}
            disabled={busy}
            style={{
              width: '100%', height: 38, padding: '0 12px', boxSizing: 'border-box',
              borderRadius: 9, border: '1px solid var(--md-outline)',
              background: 'var(--md-surface-container-lowest)',
              color: 'var(--md-on-surface)', font: 'inherit',
            }}
          />
          <p style={{ margin: '8px 0 0', fontSize: 11, lineHeight: 1.6, color: 'var(--md-on-surface-variant)' }}>
            密码只保存在本机内存，不会交给 AI，也不会写入命令行、日志或回放。
          </p>
          {error && (
            <p style={{ margin: '8px 0 0', fontSize: 12, color: '#B91C1C' }}>{error}</p>
          )}
          <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 10, marginTop: 18 }}>
            <button
              type="button"
              onClick={() => submit(null)}
              disabled={busy}
              style={{
                height: 38, padding: '0 16px', borderRadius: 10, cursor: 'pointer',
                border: '1px solid var(--md-outline)', background: 'transparent',
                color: 'var(--md-on-surface-variant)', fontFamily: 'inherit',
              }}
            >取消</button>
            <button
              type="submit"
              disabled={busy || !password}
              style={{
                height: 38, padding: '0 18px', borderRadius: 10, border: 'none',
                background: 'var(--md-primary)', color: 'var(--md-on-primary)',
                fontWeight: 600, fontFamily: 'inherit',
                cursor: busy || !password ? 'not-allowed' : 'pointer',
                opacity: busy || !password ? .6 : 1,
              }}
            >确认</button>
          </div>
        </form>
      </div>
    </div>
  );
}
