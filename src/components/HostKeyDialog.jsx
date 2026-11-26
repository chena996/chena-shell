export function parseHostKeyChallenge(error) {
  const text = typeof error === 'string' ? error : String(error?.message || error || '');
  const marker = 'HOST_KEY_CHANGED:';
  const start = text.indexOf(marker);
  if (start < 0) return null;
  try {
    const value = JSON.parse(text.slice(start + marker.length));
    if (!value?.host || !value?.public_key || !value?.fingerprint || !value?.previous_fingerprint) return null;
    return value;
  } catch {
    return null;
  }
}

export default function HostKeyDialog({ challenge, onCancel, onTrust, busy = false }) {
  if (!challenge) return null;
  const labelStyle = { display: 'block', fontSize: 12, color: 'var(--md-on-surface-variant)', marginBottom: 6 };
  const fingerprintStyle = {
    display: 'block', padding: '10px 12px', borderRadius: 9, fontSize: 12,
    fontFamily: 'Consolas, monospace', overflowWrap: 'anywhere', userSelect: 'all',
    background: 'var(--md-surface-container-low)', color: 'var(--md-on-surface)',
    border: '1px solid var(--md-outline-variant)',
  };
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="host-key-title"
      style={{
        position: 'fixed', inset: 0, background: 'rgba(0,0,0,.5)', zIndex: 300,
        display: 'grid', placeItems: 'center', padding: 24, boxSizing: 'border-box',
      }}
    >
      <div style={{
        width: 560, maxWidth: '100%', padding: 24, borderRadius: 18,
        background: 'var(--md-surface)', border: '1px solid var(--md-outline-variant)',
        boxShadow: '0 24px 64px rgba(0,0,0,.3)',
      }}>
        <div style={{ display: 'flex', gap: 12, alignItems: 'center', marginBottom: 14 }}>
          <span className="material-icons-round" style={{ color: '#D97706', fontSize: 30 }}>warning</span>
          <div>
            <h2 id="host-key-title" style={{ margin: 0, fontSize: 18 }}>服务器主机密钥已变化</h2>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)', marginTop: 4 }}>
              {challenge.host}:{challenge.port} · {challenge.algorithm}
            </div>
          </div>
        </div>
        <p style={{ fontSize: 13, lineHeight: 1.65, color: 'var(--md-on-surface-variant)' }}>
          连接已阻止。密钥变化可能来自服务器重装或管理员更换，也可能表示中间人攻击。请先通过可信渠道向服务器管理员核对新指纹，再决定是否继续。
        </p>
        <div style={{ display: 'grid', gap: 14, margin: '18px 0 22px' }}>
          <div>
            <span style={labelStyle}>原记录指纹</span>
            <code style={fingerprintStyle}>{challenge.previous_fingerprint}</code>
          </div>
          <div>
            <span style={labelStyle}>本次连接指纹</span>
            <code style={{ ...fingerprintStyle, borderColor: '#D97706' }}>{challenge.fingerprint}</code>
          </div>
        </div>
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 10 }}>
          <button type="button" onClick={onCancel} disabled={busy} style={{
            height: 38, padding: '0 18px', borderRadius: 10, cursor: 'pointer',
            border: '1px solid var(--md-outline)', background: 'transparent',
            color: 'var(--md-on-surface)', fontFamily: 'inherit',
          }}>取消连接</button>
          <button type="button" onClick={() => onTrust(challenge)} disabled={busy} style={{
            height: 38, padding: '0 18px', borderRadius: 10, border: 0,
            cursor: busy ? 'wait' : 'pointer', background: '#B45309', color: '#FFF',
            fontFamily: 'inherit', fontWeight: 600,
          }}>{busy ? '正在更新…' : '信任新密钥并连接'}</button>
        </div>
      </div>
    </div>
  );
}
