import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import Terminal from './Terminal.jsx';
import AiTerminal from './AiTerminal.jsx';
import SessionContextMenu from './SessionContextMenu.jsx';
import { contextMenuPosition, openSftpWindow } from '../lib/sessionActions.js';

let workspaceSequence = 0;

function nextId(prefix) {
  workspaceSequence += 1;
  return `${prefix}-${Date.now().toString(36)}-${workspaceSequence}`;
}

function createTab(sessionId, channelId = null) {
  const pane = { id: nextId('pane'), sessionId, channelId };
  return { id: nextId('tab'), panes: [pane], activePaneId: pane.id };
}

// write 模式的 ai_attached 为真；diag/read 观察者由 ai_mode 与 ai_readers 表示。
function isAiSessionActive(session) {
  return Boolean(
    session?.ai_attached
    || session?.ai_mode
    || (session?.ai_readers ?? 0) > 0,
  );
}

export default function TerminalWorkspace({
  sessions,
  activeId,
  onActiveSessionChange,
  connectingId,
  onToggleConnect,
  onDisconnectSession,
  onKick,
  theme,
  prefs,
  onOsc7,
  onNewSession = () => {},
  onEdit = () => {},
  onDelete = () => {},
  onOpenInNewTab = () => {},
  newTabRequest = null,
}) {
  const initialTabRef = useRef(null);
  if (!initialTabRef.current) initialTabRef.current = createTab(null);
  const [tabs, setTabs] = useState([initialTabRef.current]);
  const [activeTabId, setActiveTabId] = useState(initialTabRef.current.id);
  const [closeRequest, setCloseRequest] = useState(null);
  const closedConnectedIdsRef = useRef(new Set());
  const previousActiveIdRef = useRef(null);
  const tabRefs = useRef([]);
  const sessionById = useMemo(() => new Map(sessions.map(session => [session.id, session])), [sessions]);
  const activeTab = tabs.find(tab => tab.id === activeTabId) || tabs[0];

  // 以当前 activeTab + activePane 为唯一数据源，把用户正在看的终端上报给后端，
  // 供 MCP 定向 attach 判断活跃连接；切换标签、分屏、关闭、自动激活都会经过这里。
  useEffect(() => {
    const tab = tabs.find(item => item.id === activeTabId) || tabs[0];
    const pane = tab?.panes.find(item => item.id === tab.activePaneId) || tab?.panes[0];
    const sessionId = pane?.sessionId ?? null;
    if (!sessionId) return;
    invoke('set_active_terminal', {
      sessionId,
      channelId: pane?.channelId ?? null,
    }).catch(() => {});
  }, [activeTabId, tabs]);

  // 终端标签页复用与左侧会话列表相同的右键菜单。
  const [tabMenu, setTabMenu] = useState({ visible: false, x: 0, y: 0, session: null, tabId: null });

  function openTabMenu(event, tab, session) {
    if (!session) return;
    event.preventDefault();
    event.stopPropagation();
    if (tab.id !== activeTabId) selectTab(tab);
    setTabMenu({ visible: true, ...contextMenuPosition(event, 200, 360), session, tabId: tab.id });
  }

  const closeTabMenu = () => setTabMenu(menu => ({ ...menu, visible: false }));

  const tabMenuItems = tabMenu.session ? [
    { icon: 'cloud', label: '打开 Chena-Sftp 窗口', onClick: () => openSftpWindow(tabMenu.session) },
    { icon: 'content_copy', label: '复制 SSH 渠道', onClick: () => onOpenInNewTab(tabMenu.session) },
    { divider: true },
    {
      icon: tabMenu.session.connected ? 'link_off' : 'power_settings_new',
      label: tabMenu.session.connected ? '断开连接' : '连接 SSH',
      onClick: () => onToggleConnect?.(tabMenu.session),
    },
    { divider: true },
    { icon: 'edit', label: '编辑会话', onClick: () => onEdit(tabMenu.session) },
    {
      icon: 'content_copy',
      label: '复制主机信息',
      onClick: () => {
        const target = tabMenu.session;
        navigator.clipboard?.writeText(`${target.username}@${target.host}:${target.port}`);
      },
    },
    { icon: 'delete', label: '删除会话', danger: true, onClick: () => onDelete(tabMenu.session.id) },
    { divider: true },
    { icon: 'close', label: '关闭标签', danger: true, onClick: () => requestClose(tabMenu.tabId) },
  ] : [];

  // 同一会话存在多个标签（例如复制 SSH 渠道）时，按标签顺序编号，避免同名标签无法区分。
  const tabOrdinals = useMemo(() => {
    const paneOf = tab => tab.panes.find(pane => pane.id === tab.activePaneId) || tab.panes[0];
    const totals = new Map();
    tabs.forEach(tab => {
      const sessionId = paneOf(tab)?.sessionId;
      if (sessionId) totals.set(sessionId, (totals.get(sessionId) || 0) + 1);
    });
    const seen = new Map();
    const ordinals = new Map();
    tabs.forEach(tab => {
      const sessionId = paneOf(tab)?.sessionId;
      if (!sessionId || (totals.get(sessionId) || 0) < 2) return;
      const ordinal = (seen.get(sessionId) || 0) + 1;
      seen.set(sessionId, ordinal);
      ordinals.set(tab.id, ordinal);
    });
    return ordinals;
  }, [tabs]);

  // 侧边栏和命令面板选择会话时：已打开则激活对应标签/窗格，否则新建标签。
  useEffect(() => {
    if (previousActiveIdRef.current === activeId) return;
    previousActiveIdRef.current = activeId;
    if (!activeId) return;
    const currentTab = tabs.find(tab => tab.id === activeTabId);
    const currentPane = currentTab?.panes.find(pane => pane.id === currentTab.activePaneId);
    if (currentPane?.sessionId === activeId) return;

    const existingTab = tabs.find(tab => tab.panes.some(pane => pane.sessionId === activeId));
    const existingPane = existingTab?.panes.find(pane => pane.sessionId === activeId);
    if (existingTab && existingPane) {
      if (existingTab.activePaneId !== existingPane.id) {
        setTabs(current => current.map(tab => tab.id === existingTab.id
          ? { ...tab, activePaneId: existingPane.id }
          : tab));
      }
      setActiveTabId(existingTab.id);
      return;
    }

    // 关闭最后标签留下的空工作区直接复用，避免多出一个“未选择会话”标签。
    if (currentTab?.panes.length === 1 && !currentPane?.sessionId) {
      setTabs(current => current.map(tab => tab.id === currentTab.id
        ? { ...tab, panes: tab.panes.map(pane => ({ ...pane, sessionId: activeId })) }
        : tab));
      return;
    }

    const tab = createTab(activeId);
    setTabs(current => [...current, tab]);
    setActiveTabId(tab.id);
  }, [activeId, activeTabId, tabs]);

  // 手动或自动连接成功后，即使未选中会话，也必须为其保留一个后台标签。
  useEffect(() => {
    const connectedIds = sessions.filter(session => session.connected).map(session => session.id);
    for (const sessionId of closedConnectedIdsRef.current) {
      if (!connectedIds.includes(sessionId)) closedConnectedIdsRef.current.delete(sessionId);
    }
    setTabs(current => {
      const represented = new Set(current.flatMap(tab => tab.panes.map(pane => pane.sessionId)));
      const missing = connectedIds.filter(sessionId => !represented.has(sessionId) && !closedConnectedIdsRef.current.has(sessionId));
      if (!missing.length) return current;
      return [...current, ...missing.map(sessionId => createTab(sessionId))];
    });
  }, [sessions, tabs]);

  // 主动连接后补齐的标签若属于当前会话，则自动激活，避免新标签停在后台。
  useEffect(() => {
    if (!activeId) return;
    const currentPane = activeTab?.panes.find(pane => pane.id === activeTab.activePaneId) || activeTab?.panes[0];
    if (currentPane?.sessionId === activeId) return;
    const target = tabs.find(tab => tab.panes.some(pane => pane.sessionId === activeId));
    if (target) setActiveTabId(target.id);
  }, [activeId, tabs, activeTab]);

  // 侧边栏右键“在新标签页中打开”：复用会话新建一个独立标签。
  useEffect(() => {
    if (!newTabRequest?.sessionId) return;
    const tab = createTab(newTabRequest.sessionId, newTabRequest.channelId ?? null);
    setTabs(current => [...current, tab]);
    setActiveTabId(tab.id);
    onActiveSessionChange(newTabRequest.sessionId);
  }, [newTabRequest, onActiveSessionChange]);

  // AI 附着时创建独立的只读标签；detach 后保留历史，由用户手动关闭。
  const [aiTabs, setAiTabs] = useState([]);
  const [activeAiId, setActiveAiId] = useState(null);
  const seenAiIdsRef = useRef(new Set());
  const dismissedAiRef = useRef(new Set());

  useEffect(() => {
    // 断开后移除“已手动关闭”标记，下次 attach 时仍可重新出现。
    sessions.forEach(session => {
      if (!isAiSessionActive(session)) dismissedAiRef.current.delete(session.id);
    });
    const attachedIds = sessions.filter(isAiSessionActive).map(session => session.id);
    if (!attachedIds.length) return;
    setAiTabs(current => {
      const known = new Set(current.map(item => item.sessionId));
      const missing = attachedIds.filter(id => !known.has(id) && !dismissedAiRef.current.has(id));
      if (!missing.length) return current;
      return [...current, ...missing.map(id => ({ sessionId: id, createdAt: Date.now() }))];
    });
  }, [sessions]);

  // 仅首次出现时自动激活，避免后续 attach 抢走用户当前焦点。
  useEffect(() => {
    const fresh = aiTabs
      .map(item => item.sessionId)
      .filter(id => !seenAiIdsRef.current.has(id));
    aiTabs.forEach(item => seenAiIdsRef.current.add(item.sessionId));
    if (fresh.length) setActiveAiId(fresh[fresh.length - 1]);
  }, [aiTabs]);

  function closeAiTab(sessionId) {
    dismissedAiRef.current.add(sessionId);
    setAiTabs(current => current.filter(item => item.sessionId !== sessionId));
    if (activeAiId === sessionId) setActiveAiId(null);
  }

  function sessionName(sessionId) {
    return sessionById.get(sessionId)?.name || '未选择会话';
  }

  function selectTab(tab) {
    setActiveAiId(null);
    setActiveTabId(tab.id);
    onActiveSessionChange(tab.panes.find(pane => pane.id === tab.activePaneId)?.sessionId ?? null);
  }

  function selectPane(tabId, paneId) {
    const tab = tabs.find(item => item.id === tabId);
    const pane = tab?.panes.find(item => item.id === paneId);
    if (!tab || !pane) return;
    setActiveAiId(null);
    setActiveTabId(tabId);
    setTabs(current => current.map(item => item.id === tabId ? { ...item, activePaneId: paneId } : item));
    onActiveSessionChange(pane.sessionId);
  }

  function splitActiveTab() {
    if (activeAiId || !activeTab || activeTab.panes.length >= 2) return;
    const pane = { id: nextId('pane'), sessionId: activeId };
    setTabs(current => current.map(tab => tab.id === activeTab.id
      ? { ...tab, panes: [...tab.panes, pane], activePaneId: pane.id }
      : tab));
  }

  // 关闭复制渠道时通知后端释放对应 SSH 通道。
  function releasePaneChannel(pane) {
    if (pane?.channelId && pane.sessionId) {
      invoke('close_session_channel', { sessionId: pane.sessionId, channelId: pane.channelId }).catch(() => {});
    }
  }

  function removeTab(tabId) {
    const index = tabs.findIndex(tab => tab.id === tabId);
    if (index < 0) return;
    tabs[index].panes.forEach(releasePaneChannel);
    if (tabs.length === 1) {
      const replacement = createTab(null);
      setTabs([replacement]);
      setActiveTabId(replacement.id);
      onActiveSessionChange(null);
      return;
    }
    const remaining = tabs.filter(tab => tab.id !== tabId);
    setTabs(remaining);
    if (tabId === activeTabId) {
      const next = remaining[Math.min(index, remaining.length - 1)];
      setActiveTabId(next.id);
      onActiveSessionChange(next.panes.find(pane => pane.id === next.activePaneId)?.sessionId ?? null);
    }
  }

  function removePane(tabId, paneId) {
    const tab = tabs.find(item => item.id === tabId);
    if (!tab) return;
    const remaining = tab.panes.filter(pane => pane.id !== paneId);
    releasePaneChannel(tab.panes.find(pane => pane.id === paneId));
    const nextActivePaneId = tab.activePaneId === paneId ? remaining[0].id : tab.activePaneId;
    setTabs(current => current.map(item => item.id === tabId
      ? { ...item, panes: remaining, activePaneId: nextActivePaneId }
      : item));
    if (tabId === activeTabId && tab.activePaneId === paneId) {
      onActiveSessionChange(remaining.find(pane => pane.id === nextActivePaneId)?.sessionId ?? null);
    }
  }

  function requestClose(tabId, paneId = null) {
    const tab = tabs.find(item => item.id === tabId);
    if (!tab || (paneId && !tab.panes.some(pane => pane.id === paneId))) return;
    const wholeTab = !paneId || tab.panes.length === 1;
    const removedPanes = wholeTab ? tab.panes : tab.panes.filter(pane => pane.id === paneId);
    const remainingPanes = wholeTab ? [] : tab.panes.filter(pane => pane.id !== paneId);
    const nextActivePaneId = !wholeTab && tab.activePaneId === paneId ? remainingPanes[0].id : tab.activePaneId;
    const remainingTabs = wholeTab
      ? tabs.filter(item => item.id !== tabId)
      : tabs.map(item => item.id === tabId ? { ...item, panes: remainingPanes, activePaneId: nextActivePaneId } : item);
    const candidates = [...new Set(removedPanes.map(pane => pane.sessionId).filter(Boolean))];
    const sessionIds = candidates.filter(sessionId => sessionById.get(sessionId)?.connected
      && !remainingTabs.some(item => item.panes.some(pane => pane.sessionId === sessionId)));
    if (sessionIds.length) {
      setCloseRequest({ tabId, paneId: wholeTab ? null : paneId, sessionIds, busy: false, error: '' });
      return;
    }
    if (wholeTab) removeTab(tabId);
    else removePane(tabId, paneId);
  }

  async function confirmClose() {
    if (!closeRequest || closeRequest.busy) return;
    const request = closeRequest;
    setCloseRequest(current => current === request ? { ...current, busy: true, error: '' } : current);
    try {
      for (const sessionId of request.sessionIds) await onDisconnectSession(sessionId);
      request.sessionIds.forEach(sessionId => closedConnectedIdsRef.current.add(sessionId));
      if (request.paneId) removePane(request.tabId, request.paneId);
      else removeTab(request.tabId);
      setCloseRequest(null);
    } catch (error) {
      setCloseRequest(current => current === request ? { ...current, busy: false, error: String(error) } : current);
    }
  }

  function handleTabKeyDown(event, index) {
    let nextIndex;
    if (event.key === 'ArrowRight') nextIndex = (index + 1) % tabs.length;
    else if (event.key === 'ArrowLeft') nextIndex = (index - 1 + tabs.length) % tabs.length;
    else if (event.key === 'Home') nextIndex = 0;
    else if (event.key === 'End') nextIndex = tabs.length - 1;
    else return;
    event.preventDefault();
    selectTab(tabs[nextIndex]);
    tabRefs.current[nextIndex]?.focus();
  }

  return (
    <div className="terminal-workspace">
      <div className="terminal-workspace__tabs-row">
        <div className="terminal-workspace__tabs" role="tablist" aria-label="终端标签">
          {aiTabs.map(item => {
            const session = sessionById.get(item.sessionId);
            const selected = activeAiId === item.sessionId;
            const label = `${session?.name || item.sessionId} · AI`;
            return (
              <div className={`terminal-workspace__tab is-ai${selected ? ' is-active' : ''}`} key={`ai-${item.sessionId}`}>
                <button
                  type="button"
                  role="tab"
                  aria-selected={selected}
                  tabIndex={selected ? 0 : -1}
                  className="terminal-workspace__tab-button"
                  onClick={() => setActiveAiId(item.sessionId)}
                  title={`${label}（只读，不能执行命令）`}
                >
                  <span className="material-icons-round" aria-hidden="true">smart_toy</span>
                  <span>{label}</span>
                  <span className="terminal-workspace__ai-badge">只读</span>
                </button>
                <button
                  type="button"
                  className="terminal-workspace__tab-close"
                  onClick={() => closeAiTab(item.sessionId)}
                  aria-label={`关闭 AI 标签 ${label}`}
                >✕</button>
              </div>
            );
          })}
          {tabs.map((tab, index) => {
            const selected = activeAiId === null && tab.id === activeTabId;
            const pane = tab.panes.find(item => item.id === tab.activePaneId) || tab.panes[0];
            const ordinal = tabOrdinals.get(tab.id);
            const baseLabel = sessionName(pane?.sessionId);
            const label = ordinal ? `${baseLabel} [${ordinal}]` : baseLabel;
            return (
              <div
                className={`terminal-workspace__tab${selected ? ' is-active' : ''}`}
                key={tab.id}
                onContextMenu={event => openTabMenu(event, tab, pane?.sessionId ? sessionById.get(pane.sessionId) : null)}
              >
                <button
                  ref={element => { tabRefs.current[index] = element; }}
                  id={`${tab.id}-button`}
                  type="button"
                  role="tab"
                  aria-selected={selected}
                  aria-controls={`${tab.id}-panel`}
                  tabIndex={selected ? 0 : -1}
                  className="terminal-workspace__tab-button"
                  onClick={() => selectTab(tab)}
                  onKeyDown={event => handleTabKeyDown(event, index)}
                  title={label}
                >
                  <span className="material-icons-round" aria-hidden="true">{tab.panes.length > 1 ? 'vertical_split' : 'terminal'}</span>
                  <span>{label}</span>
                  {tab.panes.length > 1 && <span className="terminal-workspace__split-badge">2</span>}
                </button>
                <button type="button" className="terminal-workspace__tab-close" onClick={() => requestClose(tab.id)} aria-label={`关闭标签 ${label}`}>✕</button>
              </div>
            );
          })}
        </div>
        <div className="terminal-workspace__actions">
          <button type="button" onClick={onNewSession} aria-label="新建会话" title="新建会话">
            <span className="material-icons-round" aria-hidden="true">add</span>
          </button>
          <button type="button" onClick={splitActiveTab} disabled={!activeTab || activeTab.panes.length >= 2} aria-label="拆分当前终端" title="拆分当前终端">
            <span className="material-icons-round" aria-hidden="true">vertical_split</span>
          </button>
        </div>
      </div>

      <div className="terminal-workspace__panels">
        {aiTabs.map(item => {
          const session = sessionById.get(item.sessionId);
          return (
            <section
              key={`ai-${item.sessionId}`}
              role="tabpanel"
              hidden={activeAiId !== item.sessionId}
              className="terminal-workspace__panel terminal-workspace__panel--ai"
            >
              <AiTerminal
                active={activeAiId === item.sessionId}
                sessionId={item.sessionId}
                sessionName={session?.name || item.sessionId}
                connectionId={session?.ai_connection_id}
                mode={session?.ai_mode}
                expiresAt={session?.ai_expires_at}
                sudo={session?.ai_sudo}
                attached={session?.ai_attached}
                readers={session?.ai_readers ?? 0}
                task={session?.ai_task}
                taskExpiresAt={session?.ai_task_expires_at}
                theme={theme}
                fontSize={prefs?.terminal_font_size}
              />
            </section>
          );
        })}
        {tabs.map(tab => {
          const selected = activeAiId === null && tab.id === activeTabId;
          return (
            <section
              key={tab.id}
              id={`${tab.id}-panel`}
              role="tabpanel"
              aria-labelledby={`${tab.id}-button`}
              hidden={!selected}
              className="terminal-workspace__panel"
            >
              <div className="terminal-workspace__panes" data-count={tab.panes.length}>
                {tab.panes.map((pane, index) => {
                  const session = sessionById.get(pane.sessionId);
                  const paneActive = selected && tab.activePaneId === pane.id;
                  return (
                    <div
                      key={pane.id}
                      className={`terminal-workspace__pane${paneActive ? ' is-active' : ''}`}
                      data-session-id={pane.sessionId || ''}
                      onMouseDownCapture={() => selectPane(tab.id, pane.id)}
                      aria-label={`终端窗格 ${index + 1}`}
                    >
                      <div className="terminal-workspace__pane-bar">
                        <span className="terminal-workspace__pane-label">
                          <span className="terminal-workspace__pane-dot" aria-hidden="true" />
                          窗格 {index + 1}{paneActive ? ' · 当前' : ''}
                        </span>
                        <button
                          type="button"
                          className="terminal-workspace__pane-close"
                          onMouseDown={event => event.stopPropagation()}
                          onClick={() => requestClose(tab.id, pane.id)}
                          aria-label={`关闭窗格 ${index + 1}`}
                          title="关闭窗格"
                        >✕</button>
                      </div>
                      <div className="terminal-workspace__terminal">
                        <Terminal
                          session={session}
                          connecting={connectingId === session?.id}
                          onKick={onKick}
                          onConnect={() => session && onToggleConnect(session)}
                          theme={theme}
                          prefs={prefs}
                          onOsc7={cwd => onOsc7(pane.sessionId, cwd)}
                          channelId={pane.channelId ?? null}
                        />
                      </div>
                    </div>
                  );
                })}
              </div>
            </section>
          );
        })}
      </div>

      <SessionContextMenu
        menu={tabMenu}
        header={tabMenu.session?.name}
        items={tabMenuItems}
        onClose={closeTabMenu}
      />

      {closeRequest && (
        <div style={{ position: 'fixed', inset: 0, zIndex: 10005, display: 'grid', placeItems: 'center', padding: 24, background: 'rgba(0,0,0,.45)' }} onClick={() => { if (!closeRequest.busy) setCloseRequest(null); }}>
          <div role="alertdialog" aria-modal="true" aria-label="确认关闭并断开连接" style={{ width: 'min(420px, 100%)', padding: 22, borderRadius: 16, border: '1px solid var(--md-outline-variant)', background: 'var(--md-surface)', boxShadow: '0 20px 60px rgba(0,0,0,.3)' }} onClick={event => event.stopPropagation()}>
            <div style={{ fontSize: 15, fontWeight: 600, marginBottom: 10 }}>关闭最后入口并断开连接？</div>
            <div style={{ fontSize: 12, lineHeight: 1.7, color: 'var(--md-on-surface-variant)', marginBottom: 18 }}>
              {closeRequest.paneId ? '这将移除' : '这是'}{closeRequest.sessionIds.map(sessionName).join('、')}的最后一个{closeRequest.paneId ? '终端窗格' : '选项卡'}。继续后将断开对应 SSH 连接，是否继续？
            </div>
            {closeRequest.error && <div role="alert" style={{ marginBottom: 14, padding: '8px 10px', borderRadius: 8, background: '#fee2e2', color: '#991b1b', fontSize: 12, overflowWrap: 'anywhere' }}>断开失败: {closeRequest.error}</div>}
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button type="button" disabled={closeRequest.busy} onClick={() => setCloseRequest(null)}>取消</button>
              <button type="button" disabled={closeRequest.busy} onClick={confirmClose}>{closeRequest.busy ? '正在断开...' : '关闭并断开'}</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
