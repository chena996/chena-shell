import { useState } from 'react';
import SessionContextMenu from './SessionContextMenu.jsx';
import { contextMenuPosition, openSftpWindow } from '../lib/sessionActions.js';

export default function Sidebar({ sessions, activeId, connectingId, onSelect, onNew, onToggleConnect, onDelete, onEdit, onOpenInNewTab }) {
  const [hovered, setHovered] = useState(null);
  const [ctxMenu, setCtxMenu] = useState({ visible:false, x:0, y:0, session:null });

  const groups = {};
  sessions.forEach(s => {
    if (!groups[s.group]) groups[s.group] = [];
    groups[s.group].push(s);
  });

  const iconBtn = {
    width: 26, height: 26, borderRadius: 7, border: 0, background: 'transparent',
    display: 'grid', placeItems: 'center', cursor: 'pointer', padding: 0,
    color: 'var(--md-on-surface-variant)',
  };

  // 全局点击/ESC 关闭由 SessionContextMenu 统一处理。

  async function handleSftpClick(e, s) {
    e.stopPropagation();
    await openSftpWindow(s);
  }

  function openCtxMenu(e, s) {
    e.preventDefault();
    e.stopPropagation();
    setCtxMenu({ visible:true, ...contextMenuPosition(e, 200, 280), session:s });
    setHovered(s.id);
  }

  const ctxItems = ctxMenu.session ? [
    { icon:'cloud', label:'打开 Chena-Sftp 窗口', onClick: () => openSftpWindow(ctxMenu.session) },
    { icon:'content_copy', label:'复制 SSH 渠道', onClick: () => onOpenInNewTab?.(ctxMenu.session) },
    { divider:true },
    { icon: ctxMenu.session.connected ? 'link_off' : 'power_settings_new',
      label: ctxMenu.session.connected ? '断开连接' : '连接 SSH',
      onClick: () => onToggleConnect(ctxMenu.session) },
    { divider:true },
    { icon:'edit', label:'编辑会话', onClick: () => onEdit?.(ctxMenu.session) },
    { icon:'content_copy', label:'复制主机信息', onClick: () => {
        const s = ctxMenu.session;
        navigator.clipboard?.writeText(`${s.username}@${s.host}:${s.port}`);
      }
    },
    { divider:true },
    { icon:'delete', label:'删除会话', danger:true, onClick: () => onDelete(ctxMenu.session.id) },
  ] : [];

  return (
    <aside style={{
      width: 264, flexShrink: 0, background: 'var(--md-surface)',
      borderRight: '1px solid var(--md-outline-variant)',
      display: 'flex', flexDirection: 'column', overflow: 'hidden',
    }}>
      <button onClick={onNew} style={{
        margin: '12px 12px 6px', padding: '10px 16px', borderRadius: 12,
        border: '1px solid var(--md-outline)', background: 'var(--md-surface-container-low)',
        color: 'var(--md-on-surface)', fontFamily: 'inherit', fontWeight: 500, fontSize: 13,
        display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 8,
        cursor: 'pointer', transition: 'background .15s,border-color .15s',
      }}
        onMouseOver={e => { e.currentTarget.style.borderColor = 'var(--md-primary)'; }}
        onMouseOut={e => { e.currentTarget.style.borderColor = 'var(--md-outline)'; }}
      >
        <span className="material-icons-round" style={{ fontSize: 18, color: 'var(--md-primary)' }}>add</span>
        新建会话
      </button>

      <div style={{ flex: 1, overflowY: 'auto', padding: '4px 8px 8px' }}>
        {sessions.length === 0 && (
          <div style={{
            margin: '28px 12px', padding: 16, borderRadius: 12,
            background: 'var(--md-surface-container-low)',
            fontSize: 12, color: 'var(--md-on-surface-variant)', lineHeight: 1.7,
          }}>
            还没有会话。点击上方「新建会话」填写主机、用户名和密码即可连接。
          </div>
        )}

        {Object.entries(groups).map(([group, items]) => (
          <div key={group} style={{ marginBottom: 2 }}>
            <div style={{
              padding: '6px 10px', fontSize: 11, fontWeight: 600,
              color: 'var(--md-on-surface-variant)', letterSpacing: '.06em', textTransform: 'uppercase',
            }}>{group}</div>
            {items.map(s => {
              const isActive = s.id === activeId;
              const isConnecting = s.id === connectingId;
              const isHovered = hovered === s.id;
              return (
                <div key={s.id}
                  onClick={() => onSelect(s.id)}
                  onDoubleClick={() => { if (!s.connected) onToggleConnect?.(s); }}
                  onContextMenu={e => openCtxMenu(e, s)}
                  onMouseEnter={() => setHovered(s.id)}
                  onMouseLeave={() => setHovered(null)}
                  style={{
                    display: 'flex', alignItems: 'center', gap: 10,
                    padding: '8px 10px', borderRadius: 10, cursor: 'pointer', fontSize: 13,
                    background: isActive ? 'var(--md-primary-container)' : 'transparent',
                    color: isActive ? 'var(--md-on-primary-container)' : 'var(--md-on-surface)',
                    fontWeight: isActive ? 500 : 400,
                  }}>
                  <div style={{
                    width: 28, height: 28, borderRadius: 8,
                    background: 'var(--md-surface-container-highest)',
                    display: 'grid', placeItems: 'center', flexShrink: 0,
                  }}>
                    <span className="material-icons-round" style={{ fontSize: 16, color: 'var(--md-primary)' }}>dns</span>
                  </div>

                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ fontWeight: 500, fontSize: 13, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{s.name}</div>
                    <div style={{ fontSize: 11, color: 'var(--md-on-surface-variant)', marginTop: 1, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                      {s.username}@{s.host}:{s.port}
                    </div>
                  </div>

                  {s.ai_attached && (
                    <span style={{
                      fontSize: 9, padding: '2px 5px', borderRadius: 5,
                      background: 'var(--ai-accent)', color: '#FFF', fontWeight: 600,
                    }}>AI</span>
                  )}

                  {isConnecting ? (
                    <span style={{ fontSize: 10, color: 'var(--md-primary)' }}>连接中…</span>
                  ) : (
                    <span style={{
                      width: 8, height: 8, borderRadius: '50%', flexShrink: 0,
                      background: s.connected ? '#10B981' : '#9CA3AF',
                      boxShadow: s.connected ? '0 0 0 3px color-mix(in srgb,#10B981 25%,transparent)' : 'none',
                    }} />
                  )}

                  {isHovered && (
                    <div style={{ display: 'flex', gap: 2 }}>
                      <button
                        style={iconBtn}
                        title="打开 Chena-Sftp 窗口（未连接则自动连）"
                        onClick={e => handleSftpClick(e, s)}
                      >
                        <span className="material-icons-round" style={{ fontSize: 16 }}>cloud</span>
                      </button>
                      <button
                        style={iconBtn}
                        title={s.connected ? '断开' : '连接'}
                        onClick={e => { e.stopPropagation(); onToggleConnect(s); }}
                      >
                        <span className="material-icons-round" style={{ fontSize: 16 }}>
                          {s.connected ? 'link_off' : 'power_settings_new'}
                        </span>
                      </button>
                      <button
                        style={iconBtn}
                        title="编辑会话"
                        onClick={e => { e.stopPropagation(); onEdit?.(s); }}
                      >
                        <span className="material-icons-round" style={{ fontSize: 16 }}>edit</span>
                      </button>
                      <button
                        style={iconBtn}
                        title="删除会话"
                        onClick={e => { e.stopPropagation(); onDelete(s.id); }}
                      >
                        <span className="material-icons-round" style={{ fontSize: 16 }}>delete</span>
                      </button>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </div>

      <div style={{
        borderTop: '1px solid var(--md-outline-variant)', padding: '8px 12px',
        fontSize: 11, color: 'var(--md-on-surface-variant)',
        display: 'flex', justifyContent: 'space-between',
      }}>
        <span>{sessions.length} 个会话</span>
        <span>{sessions.filter(s => s.connected).length} 个在线</span>
      </div>

      {/* 右键菜单 */}
      <SessionContextMenu
        menu={ctxMenu}
        header={ctxMenu.session?.name}
        items={ctxItems}
        onClose={() => setCtxMenu(m => ({ ...m, visible: false }))}
      />
    </aside>
  );
}
