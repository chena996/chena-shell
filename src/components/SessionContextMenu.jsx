import { useEffect } from 'react';

/**
 * 会话右键菜单的共享实现：左侧会话列表与终端标签页复用同一套菜单与行为。
 * menu = { visible, x, y }；items = [{ icon, label, danger, divider, onClick }]。
 */
export default function SessionContextMenu({ menu, header, items, onClose }) {
  useEffect(() => {
    if (!menu.visible) return undefined;
    const close = () => onClose();
    const esc = event => { if (event.key === 'Escape') onClose(); };
    document.addEventListener('click', close);
    document.addEventListener('keydown', esc);
    return () => {
      document.removeEventListener('click', close);
      document.removeEventListener('keydown', esc);
    };
  }, [menu.visible, onClose]);

  if (!menu.visible) return null;

  return (
    <div
      style={{
        position: 'fixed', left: menu.x, top: menu.y, zIndex: 9998,
        background: 'var(--md-surface-container-high)',
        border: '1px solid var(--md-outline-variant)', borderRadius: 10,
        boxShadow: '0 8px 24px rgba(0,0,0,.18)', padding: '6px 0', minWidth: 190,
      }}
      onClick={event => event.stopPropagation()}
    >
      {header && (
        <div style={{
          padding: '4px 14px 8px', fontSize: 10, color: 'var(--md-on-surface-variant)',
          fontFamily: "'JetBrains Mono',monospace", overflow: 'hidden',
          textOverflow: 'ellipsis', whiteSpace: 'nowrap',
        }}>{header}</div>
      )}
      {items.map((item, index) => item.divider ? (
        <div key={index} style={{ height: 1, background: 'var(--md-outline-variant)', margin: '4px 0' }} />
      ) : (
        <div
          key={index}
          onClick={() => { onClose(); item.onClick?.(); }}
          onMouseEnter={event => { event.currentTarget.style.background = 'var(--md-primary-container)'; }}
          onMouseLeave={event => { event.currentTarget.style.background = 'transparent'; }}
          style={{
            padding: '7px 14px', display: 'flex', alignItems: 'center', gap: 10,
            fontSize: 12, cursor: 'pointer',
            color: item.danger ? '#DC2626' : 'var(--md-on-surface)',
            transition: 'background .08s',
          }}
        >
          <span
            className="material-icons-round"
            style={{ fontSize: 16, color: item.danger ? '#DC2626' : 'var(--md-on-surface-variant)' }}
          >{item.icon}</span>
          {item.label}
        </div>
      ))}
    </div>
  );
}
