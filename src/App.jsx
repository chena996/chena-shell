import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { emit, listen } from '@tauri-apps/api/event';
import Sidebar from './components/Sidebar.jsx';
import TerminalWorkspace from './components/TerminalWorkspace.jsx';
import BottomPanel from './components/BottomPanel.jsx';
import NewSessionDialog from './components/NewSessionDialog.jsx';
import HostKeyDialog, { parseHostKeyChallenge } from './components/HostKeyDialog.jsx';
import ApprovalDialog from './components/ApprovalDialog.jsx';
import SudoPasswordDialog from './components/SudoPasswordDialog.jsx';
import SettingsDialog from './components/SettingsDialog.jsx';
import CommandPalette from './components/CommandPalette.jsx';
import ToastRegion from './components/Toast.jsx';
import Onboarding from './components/Onboarding.jsx';
import MainMenu from './components/MainMenu.jsx';

// React 渲染前就设好主题，避免首帧白屏
try {
  document.documentElement.dataset.theme = localStorage.getItem('chena-shell-theme') || 'dark';
} catch {}

/** 后端 {config, status} → 前端扁平结构 */
function flatten(items) {
  return items.map(({ config, status }) => ({
    ...config,
    group: config.group || '默认分组',
    connected: status.connected,
    latency_ms: status.latency_ms,
    ai_attached: status.ai_attached,
    ai_attached_at: status.ai_attached_at,
    ai_connection_id: status.ai_connection_id ?? null,
    ai_mode: status.ai_mode ?? null,
    ai_expires_at: status.ai_expires_at ?? null,
    ai_sudo: Boolean(status.ai_sudo),
    ai_readers: status.ai_readers ?? 0,
    ai_task: status.ai_task ?? null,
    ai_task_expires_at: status.ai_task_expires_at ?? null,
  }));
}

export default function App() {
  const [theme, setTheme] = useState(() => {
    try { return localStorage.getItem('chena-shell-theme') || 'dark'; } catch { return 'dark'; }
  });
  const [sessions, setSessions] = useState([]);
  const [activeId, setActiveId] = useState(null);
  const [mcp, setMcp] = useState({ listening: false, host: '127.0.0.1', port: 8767 });
  const [monitor, setMonitor] = useState(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editInitial, setEditInitial] = useState(null);  // 编辑模式时的初始 config
  const [error, setError] = useState('');
  const [connecting, setConnecting] = useState(null);
  const [aiEvents, setAiEvents] = useState([]);
  const [hostKeyPrompt, setHostKeyPrompt] = useState(null);
  const [hostKeyBusy, setHostKeyBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const [confirmations, setConfirmations] = useState([]);
  const [confirmationBusy, setConfirmationBusy] = useState('');
  const [dismissedConfirmationId, setDismissedConfirmationId] = useState('');
  const [settings, setSettings] = useState(null);
  const [uiPrefs, setUiPrefs] = useState({
    theme: 'dark', terminal_font_size: 14, copy_on_select: false,
    confirm_multiline_paste: true, links_enabled: true, bottom_panel_height: 280,
  });
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsCategory, setSettingsCategory] = useState('general');
  const [settingsBusy, setSettingsBusy] = useState(false);
  const mainColumnRef = useRef(null);
  const panelDragRef = useRef(280);
  const autoConnectStartedRef = useRef(false);
  const [sessionsLoaded, setSessionsLoaded] = useState(false);
  const [eventsReady, setEventsReady] = useState(false);
  const [migrationStatus, setMigrationStatus] = useState(null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [newTabRequest, setNewTabRequest] = useState(null);
  const [onboardingOpen, setOnboardingOpen] = useState(false);
  const [onboardingBusy, setOnboardingBusy] = useState(false);
  const closePalette = useCallback(() => setPaletteOpen(false), []);

  // 会话列表刷新
  const refresh = useCallback(async () => {
    try {
      const items = await invoke('list_sessions');
      const flat = flatten(items);
      setSessions(flat);
      setActiveId(prev => (prev && flat.some(s => s.id === prev) ? prev : null));
      setSessionsLoaded(true);
      return flat;
    } catch (e) {
      setSessionsLoaded(true);
      setError(`读取会话列表失败: ${e}`);
      return [];
    }
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem('chena-shell-theme', theme);
  }, [theme]);

  // 设置只在此处读取一次；保存必须显式成功，不能在渲染副作用中静默落盘。
  useEffect(() => {
    let alive = true;
    Promise.all([invoke('get_global_settings'), invoke('get_ui_prefs')]).then(([loadedSettings, prefs]) => {
      if (!alive) return;
      setSettings(loadedSettings);
      setUiPrefs(current => ({ ...current, ...prefs }));
      setOnboardingOpen(loadedSettings?.show_onboarding !== false);
      if (prefs?.theme === 'dark' || prefs?.theme === 'light') setTheme(prefs.theme);
    }).catch(e => setError(`读取全局设置失败: ${e}`));
    return () => { alive = false; };
  }, []);

  useEffect(() => {
    function handleGlobalShortcut(event) {
      const commandKey = (event.ctrlKey || event.metaKey) && !event.altKey;
      if (commandKey && (event.key.toLocaleLowerCase() === 'k' || event.code === 'KeyK')) {
        if (settingsOpen || dialogOpen || onboardingOpen || hostKeyPrompt || confirmations.length) return;
        event.preventDefault();
        event.stopPropagation();
        setPaletteOpen(current => !current);
        return;
      }
      if (!event.ctrlKey || event.altKey || event.metaKey) return;
      if (event.key !== ',' && event.code !== 'Comma') return;
      if (paletteOpen || dialogOpen || onboardingOpen || hostKeyPrompt || confirmations.length) return;
      event.preventDefault();
      setPaletteOpen(false);
      setSettingsCategory('general');
      setSettingsOpen(true);
    }
    window.addEventListener('keydown', handleGlobalShortcut, true);
    return () => window.removeEventListener('keydown', handleGlobalShortcut, true);
  }, [settingsOpen, dialogOpen, onboardingOpen, hostKeyPrompt, confirmations, paletteOpen]);

  useEffect(() => {
    refresh();
    invoke('mcp_status').then(setMcp).catch(() => {});
    invoke('take_restore_sessions_status').then(status => {
      setMigrationStatus(status);
      const messages = [];
      if (status?.log_default_migration_performed) {
        messages.push('已按全局默认值启用既有会话的终端日志，历史日志未删除。');
      }
      if (status?.secret_storage_migration_performed) {
        messages.push('密码和私钥口令已迁移到 Windows Credential Manager，加密配置仅保留引用。');
      }
      if (messages.length) setNotice(`安全升级：${messages.join(' ')}`);
    }).catch(() => {});

    // 全局阻止浏览器默认右键菜单（终端区域内也拦截，后续有需要再放行）
    const blockCtx = (e) => e.preventDefault();
    document.addEventListener('contextmenu', blockCtx);

    // 注册后端 eval 路径的全局回调（覆盖 emit_event 里的 win.eval 通道）
    window.__ON_SESSION_STATUS__ = (_sid, _connected) => {
      refresh();
    };

    const un = listen('session-status', event => {
      if (event.payload?.host_key_first_seen) {
        setNotice(`首次连接 ${event.payload.host}:${event.payload.port}：已记录并信任服务器主机密钥；后续密钥变化将默认拒绝。`);
      }
      refresh();
    });

    // 只有事件监听就绪后才允许启动自动连接，不再用启动轮询掩盖竞态。
    un.then(() => setEventsReady(true)).catch(e => setError(`会话事件监听失败: ${e}`));

    return () => {
      document.removeEventListener('contextmenu', blockCtx);
      delete window.__ON_SESSION_STATUS__;
      un.then(f => f());
    };
  }, [refresh]);

  useEffect(() => {
    if (autoConnectStartedRef.current || !eventsReady || !sessionsLoaded || !settings) return;
    autoConnectStartedRef.current = true;
    if (!settings.startup_auto_connect) return;
    const queue = sessions.filter(session => session.auto_connect && !session.connected);
    if (!queue.length) return;
    let cancelled = false;
    let nextIndex = 0;

    async function worker() {
      while (!cancelled) {
        const session = queue[nextIndex++];
        if (!session) return;
        setConnecting(session.id);
        try {
          await invoke('connect_session', { sessionId: session.id });
        } catch (e) {
          if (!cancelled) handleConnectError(e, session.id);
        } finally {
          setConnecting(current => current === session.id ? null : current);
          await refresh();
        }
      }
    }

    const workers = Array.from({ length: Math.min(3, queue.length) }, () => worker());
    Promise.all(workers).catch(e => { if (!cancelled) setError(String(e)); });
    return () => { cancelled = true; };
    // 仅消费事件监听和首次会话加载完成时的快照，后续状态更新由事件负责。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [eventsReady, sessionsLoaded, settings, refresh]);

  // 监控由独立后端连接采样；前端仅接收事件，切换会话时读取一次最新快照。
  useEffect(() => {
    let alive = true;
    setMonitor(null);
    if (!activeId) return () => { alive = false; };
    invoke('get_monitor_snapshot', { sessionId: activeId }).then(snapshot => {
      if (alive) setMonitor(snapshot);
    }).catch(() => {});
    const un = listen('monitor-update', event => {
      if (event.payload?.session_id === activeId) setMonitor(event.payload);
    });
    return () => { alive = false; un.then(f => f()); };
  }, [activeId]);

  useEffect(() => {
    let alive = true;
    setAiEvents([]);
    if (!activeId) return () => { alive = false; };
    invoke('list_ai_events', { sessionId: activeId }).then(events => {
      if (alive) setAiEvents(events);
    }).catch(() => {});
    const un = listen('ai-event', event => {
      if (event.payload?.session_id !== activeId) return;
      invoke('list_ai_events', { sessionId: activeId }).then(events => {
        if (alive) setAiEvents(events);
      }).catch(() => {});
    });
    return () => { alive = false; un.then(f => f()); };
  }, [activeId]);

  // AI 附着状态变化：刷新会话列表，驱动独立只读标签的创建与状态展示。
  useEffect(() => {
    const un = listen('ai-attach-changed', () => {
      refresh().catch(() => {});
    });
    return () => { un.then(f => f()).catch(() => {}); };
  }, [refresh]);

  // 全局审批队列：后端创建、处理和过期请求时主动推送变化。
  useEffect(() => {
    let alive = true;
    let loading = false;
    async function loadConfirmations() {
      if (loading) return;
      loading = true;
      try {
        const items = await invoke('list_confirmations');
        if (alive) setConfirmations(items);
      } catch { /* 浏览器环境忽略 */ }
      finally { loading = false; }
    }
    loadConfirmations();
    const un = listen('confirmation-update', loadConfirmations);
    return () => { alive = false; un.then(f => f()); };
  }, []);

  async function resolveConfirmation(confirmation, approved) {
    setConfirmationBusy(confirmation.id);
    setError('');
    try {
      await invoke('resolve_confirmation', { confirmationId: confirmation.id, approved });
      setConfirmations(current => current.filter(item => item.id !== confirmation.id));
      setNotice(approved
        ? `已批准 ${confirmation.client_name} 的一次请求，等待客户端重试原操作。`
        : `已拒绝 ${confirmation.client_name} 的请求。`);
      if (confirmation.session_id === activeId) {
        invoke('list_ai_events', { sessionId: activeId }).then(setAiEvents).catch(() => {});
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setConfirmationBusy('');
    }
  }

  function openSettings(category = 'general') {
    setSettingsCategory(category);
    setSettingsOpen(true);
  }

  async function toggleTheme() {
    const previous = theme;
    const next = previous === 'light' ? 'dark' : 'light';
    setTheme(next);
    setError('');
    try {
      await invoke('save_ui_prefs', { theme: next });
      setUiPrefs(current => ({ ...current, theme: next }));
    } catch (e) {
      setTheme(previous);
      setError(`主题保存失败: ${e}`);
    }
  }

  async function saveSettings(payload) {
    setSettingsBusy(true);
    setError('');
    const nextSettings = { ...settings, ...payload };
    const nextPrefs = {
      ...uiPrefs,
      theme: payload.theme,
      terminal_font_size: payload.terminal_font_size,
      copy_on_select: payload.copy_on_select,
      confirm_multiline_paste: payload.confirm_multiline_paste,
      links_enabled: payload.links_enabled,
    };
    try {
      await invoke('update_global_settings', { settings: nextSettings, uiPrefs: nextPrefs });
      const mcpStatus = await invoke('mcp_status');
      setSettings(nextSettings);
      setUiPrefs(nextPrefs);
      setTheme(nextPrefs.theme);
      setMcp(mcpStatus);
      return { settings: nextSettings, mcpStatus, notice: '全局设置已保存并应用' };
    } catch (e) {
      const message = `保存全局设置失败: ${e}`;
      setError(message);
      throw new Error(message, { cause: e });
    } finally {
      setSettingsBusy(false);
    }
  }

  async function completeOnboarding() {
    if (!settings || onboardingBusy) return;
    const nextSettings = { ...settings, show_onboarding: false };
    setOnboardingBusy(true);
    setError('');
    try {
      await invoke('update_global_settings', { settings: nextSettings, uiPrefs });
      setSettings(nextSettings);
      setOnboardingOpen(false);
    } catch (e) {
      setError(`保存引导状态失败: ${e}`);
    } finally {
      setOnboardingBusy(false);
    }
  }

  function dismissOnboarding(dontShowAgain) {
    if (dontShowAgain) completeOnboarding();
    else setOnboardingOpen(false);
  }

  async function restartMcp() {
    const mcpStatus = await invoke('restart_mcp');
    setMcp(mcpStatus);
    return {
      mcpStatus,
      notice: mcpStatus.listening ? 'MCP 已完成重启并恢复监听' : 'MCP 服务已停止',
    };
  }

  function beginPanelDrag(event) {
    event.preventDefault();
    const bounds = mainColumnRef.current?.getBoundingClientRect();
    if (!bounds) return;
    const startY = event.clientY;
    const startHeight = uiPrefs.bottom_panel_height;
    const minHeight = Math.max(160, Math.round(bounds.height * 0.2));
    const maxHeight = Math.min(640, Math.round(bounds.height * 0.8));
    panelDragRef.current = startHeight;

    function onMove(moveEvent) {
      // 分隔条位于底部面板上方：向上拖动应放大面板，向下拖动应缩小，边界跟随鼠标。
      const desired = startHeight - (moveEvent.clientY - startY);
      const height = Math.round(Math.max(minHeight, Math.min(maxHeight, desired)));
      panelDragRef.current = height;
      setUiPrefs(current => ({ ...current, bottom_panel_height: height }));
    }
    async function onUp() {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      try {
        await invoke('save_ui_prefs', { bottomPanelHeight: panelDragRef.current });
      } catch (e) {
        setUiPrefs(current => ({ ...current, bottom_panel_height: startHeight }));
        setError(`面板尺寸保存失败: ${e}`);
      }
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp, { once: true });
  }

  function handleOsc7(sessionId, cwd) {
    if (!sessionId || sessionId !== activeId || !cwd?.path) return;
    emit('sftp-osc7', { session_id: sessionId, ...cwd }).catch(() => {});
  }

  const activeSession = sessions.find(s => s.id === activeId);
  const activeConfirmation = confirmations[0] || null;
  const confirmationPrompt = activeConfirmation?.id === dismissedConfirmationId ? null : activeConfirmation;
  const confirmationSession = sessions.find(s => s.id === confirmationPrompt?.session_id);

  const disconnectSession = useCallback(async sessionId => {
    await invoke('disconnect_session', { sessionId });
    await refresh();
  }, [refresh]);

  async function toggleConnect(session) {
    if (!session) return;
    setError('');
    if (session.connected) {
      try {
        await disconnectSession(session.id);
      } catch (e) { setError(String(e)); }
      return;
    }
    setConnecting(session.id);
    try {
      await invoke('connect_session', { sessionId: session.id });
      await refresh();
      // 主动连接后切换到该会话，让新建标签立即成为当前标签。
      setActiveId(session.id);
    } catch (e) {
      handleConnectError(e, session.id);
    } finally {
      setConnecting(null);
    }
  }

  function handleConnectError(error, sessionId) {
    const challenge = parseHostKeyChallenge(error);
    if (challenge) {
      setHostKeyPrompt({ sessionId, challenge });
    } else {
      setError(String(error));
    }
  }

  async function trustHostKey(challenge) {
    if (!hostKeyPrompt) return;
    setHostKeyBusy(true);
    setError('');
    try {
      await invoke('accept_host_key', { challenge });
      await invoke('connect_session', { sessionId: hostKeyPrompt.sessionId });
      setHostKeyPrompt(null);
      await refresh();
      setActiveId(hostKeyPrompt.sessionId);
    } catch (e) {
      handleConnectError(e, hostKeyPrompt.sessionId);
    } finally {
      setHostKeyBusy(false);
    }
  }

  async function createAndConnect(config) {
    setError('');
    try {
      // 判断是新建还是编辑（initial 存在就是编辑）
      const isEdit = !!editInitial;
      if (isEdit) {
        await invoke('update_session', { sessionId: editInitial.id, config });
      } else {
        await invoke('create_session', { config });
      }
      await refresh();
      setActiveId(config.id);
      setDialogOpen(false);
      setEditInitial(null);
      // 编辑模式不自动连接（已连接的会话保持连接，断了也不自动连）
      if (!isEdit && config.auto_connect) {
        setConnecting(config.id);
        try { await invoke('connect_session', { sessionId: config.id }); }
        catch (e) { handleConnectError(e, config.id); }
        finally { setConnecting(null); }
        await refresh();
      }
    } catch (e) {
      setError(String(e));
      throw e;
    }
  }

  const openNewDialog = () => { setEditInitial(null); setDialogOpen(true); };
  const openEditDialog = (session) => { setEditInitial(session); setDialogOpen(true); };
  const openInNewTab = useCallback(async (session) => {
    if (!session) return;
    try {
      const channelId = await invoke('open_session_channel', { sessionId: session.id });
      setNewTabRequest({ sessionId: session.id, channelId, nonce: Date.now() });
    } catch (error) {
      setError(`复制 SSH 渠道失败: ${error}`);
    }
  }, []);

  async function openSftp(session) {
    if (!session) return;
    setError('');
    try {
      if (!session.connected) {
        setConnecting(session.id);
        await invoke('connect_session', { sessionId: session.id });
        await refresh();
      }
      await invoke('open_sftp_window', { sessionId: session.id });
      setNotice(`已打开 ${session.name} 的 SFTP 窗口。`);
    } catch (e) {
      setError(`打开 ${session.name} 的 SFTP 失败: ${e}`);
    } finally {
      setConnecting(current => current === session.id ? null : current);
    }
  }

  function reportCommandError(e) {
    setError(`命令执行失败: ${e}`);
  }

  const generalCommands = [
    { id: 'new-session', group: '操作', icon: 'add', label: '新建会话', description: '创建 SSH 会话配置', run: openNewDialog },
    {
      id: 'toggle-active-connection', group: '操作',
      icon: activeSession?.connected ? 'link_off' : 'power_settings_new',
      label: activeSession?.connected ? '断开当前会话' : '连接当前会话',
      description: activeSession ? `${activeSession.name} · ${activeSession.username}@${activeSession.host}` : '请先选择一个会话',
      keywords: activeSession?.name || '', disabled: !activeSession || connecting === activeSession?.id,
      run: () => toggleConnect(activeSession),
    },
    {
      id: 'open-active-sftp', group: '操作', icon: 'cloud', label: '打开当前会话 SFTP',
      description: activeSession ? `${activeSession.name} · 未连接时会先建立 SSH 连接` : '请先选择一个会话',
      keywords: '文件 传输 sftp', disabled: !activeSession || connecting === activeSession?.id,
      run: () => openSftp(activeSession),
    },
    { id: 'open-settings', group: '操作', icon: 'settings', label: '打开设置', description: '通用、终端、连接、AI 安全与 MCP 设置', shortcut: 'Ctrl+,', run: () => openSettings() },
    {
      id: 'toggle-theme', group: '操作', icon: theme === 'light' ? 'dark_mode' : 'light_mode',
      label: theme === 'light' ? '切换到深色主题' : '切换到浅色主题',
      description: '主窗口与 SFTP 窗口实时跟随主题', run: toggleTheme,
    },
  ];
  const paletteCommands = [
    ...generalCommands,
    ...sessions.map(session => ({
      id: `switch-${session.id}`, group: '会话', icon: 'dns', label: `切换到 ${session.name}`,
      description: `${session.username}@${session.host}:${session.port} · ${session.connected ? '已连接' : '未连接'}`,
      keywords: `${session.group} ${session.host} ${session.username}`,
      run: () => setActiveId(session.id),
    })),
  ];
  const menuCommands = [
    { id: 'command-palette', icon: 'search', label: '命令面板', shortcut: 'Ctrl+K', run: () => setPaletteOpen(true) },
    ...generalCommands,
  ];

  async function deleteSession(id) {
    try {
      await invoke('delete_session', { sessionId: id });
      await refresh();
    } catch (e) { setError(String(e)); }
  }

  async function kickAi(id) {
    try {
      await invoke('kick_ai', { sessionId: id });
      await refresh();
    } catch (e) { setError(String(e)); }
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', width: '100%', overflow: 'hidden' }}>
      {/* APP BAR */}
      <header style={{
        height: 56, flexShrink: 0, background: 'var(--md-surface)',
        borderBottom: '1px solid var(--md-outline-variant)',
        display: 'flex', alignItems: 'center', gap: 16, padding: '0 20px',
        boxSizing: 'border-box', width: '100%', overflow: 'hidden',
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, fontWeight: 600, fontSize: 15, flexShrink: 0, whiteSpace: 'nowrap' }}>
          <div style={{
            width: 32, height: 32, borderRadius: 10,
            background: 'linear-gradient(135deg,var(--md-primary),#7C3AED)',
            color: '#FFF', display: 'grid', placeItems: 'center',
          }}>
            <span className="material-icons-round" style={{ fontSize: 18 }}>terminal</span>
          </div>
          Chena-Shell
        </div>

        <div style={{ flex: 1, minWidth: 8 }} />

        <button type="button" className="app-command-trigger" onClick={() => setPaletteOpen(true)} aria-label="打开命令面板（Ctrl+K）" aria-keyshortcuts="Control+K">
          <span className="material-icons-round" aria-hidden="true">search</span>
          <span>命令</span>
          <kbd>Ctrl K</kbd>
        </button>

        <div style={{
          display: 'flex', alignItems: 'center', gap: 8, padding: '5px 12px',
          borderRadius: 14, border: '1px solid var(--md-outline-variant)',
          background: 'var(--md-surface-container-low)', fontSize: 12, fontWeight: 500,
          flexShrink: 0, whiteSpace: 'nowrap',
        }}>
          <span style={{
            width: 8, height: 8, borderRadius: '50%', flexShrink: 0,
            background: mcp.listening ? '#10B981' : '#EF4444',
            boxShadow: mcp.listening ? '0 0 0 3px color-mix(in srgb,#10B981 25%,transparent)' : 'none',
          }} />
          MCP {mcp.listening ? '运行中' : '未启动'} · :{mcp.port}
        </div>

        <MainMenu commands={menuCommands} onActionError={reportCommandError} />

        <button onClick={() => openSettings()} aria-label="设置（Ctrl+,）" style={{
          width: 36, height: 36, borderRadius: 10, border: 0, background: 'transparent',
          display: 'grid', placeItems: 'center', cursor: 'pointer', flexShrink: 0,
          color: 'var(--md-on-surface-variant)',
        }} title="设置（Ctrl+,）">
          <span className="material-icons-round">settings</span>
        </button>

        <button onClick={toggleTheme} aria-label="切换主题" style={{
          width: 36, height: 36, borderRadius: 10, border: 0, background: 'transparent',
          display: 'grid', placeItems: 'center', cursor: 'pointer', flexShrink: 0,
          color: 'var(--md-on-surface-variant)',
        }} title="切换主题">
          <span className="material-icons-round">{theme === 'light' ? 'dark_mode' : 'light_mode'}</span>
        </button>
      </header>

      {/* MAIN */}
      <div style={{ flex: 1, display: 'flex', minHeight: 0 }}>
        <Sidebar
          sessions={sessions}
          activeId={activeId}
          connectingId={connecting}
          onSelect={setActiveId}
          onNew={openNewDialog}
          onEdit={openEditDialog}
          onToggleConnect={toggleConnect}
          onDelete={deleteSession}
          onOpenInNewTab={openInNewTab}
        />
        <div ref={mainColumnRef} style={{ flex: 1, display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0 }}>
          <div style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
            <TerminalWorkspace
              sessions={sessions}
              activeId={activeId}
              onActiveSessionChange={setActiveId}
              connectingId={connecting}
              onToggleConnect={toggleConnect}
              onDisconnectSession={disconnectSession}
              onKick={kickAi}
              theme={theme}
              prefs={uiPrefs}
              onOsc7={handleOsc7}
              onNewSession={openNewDialog}
              onEdit={openEditDialog}
              onDelete={deleteSession}
              onOpenInNewTab={openInNewTab}
              newTabRequest={newTabRequest}
            />
          </div>

          <div
            role="separator"
            aria-label="调整底部面板高度"
            aria-orientation="horizontal"
            onMouseDown={beginPanelDrag}
            style={{ flex: '0 0 6px', cursor: 'row-resize', background: 'var(--md-outline-variant)' }}
          />
          <div style={{ flex: `0 0 ${uiPrefs.bottom_panel_height}px`, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
            <BottomPanel
              session={activeSession}
              monitor={monitor}
              aiEvents={aiEvents}
              confirmations={confirmations}
              confirmationBusy={confirmationBusy}
              onResolveConfirmation={resolveConfirmation}
              onSessionUpdated={refresh}
            />
          </div>
        </div>
      </div>

      <ToastRegion
        error={error}
        notice={notice}
        onDismissError={() => setError('')}
        onDismissNotice={() => setNotice('')}
      />

      {paletteOpen && (
        <CommandPalette commands={paletteCommands} onClose={closePalette} onActionError={reportCommandError} />
      )}
      {onboardingOpen && (
        <Onboarding busy={onboardingBusy} onFinish={completeOnboarding} onDismiss={dismissOnboarding} />
      )}

      <SettingsDialog
        open={settingsOpen}
        initialCategory={settingsCategory}
        settings={settings || {}}
        uiPrefs={uiPrefs}
        mcpStatus={mcp}
        migrationStatus={migrationStatus}
        busy={settingsBusy}
        onCancel={() => setSettingsOpen(false)}
        onSave={saveSettings}
        onGenerateCredential={() => invoke('generate_mcp_credential')}
        onTestMcpRestart={restartMcp}
        onOpenLogsDir={() => invoke('open_logs_dir')}
      />

      {dialogOpen && (
        <NewSessionDialog onCancel={() => { setDialogOpen(false); setEditInitial(null); }} onCreate={createAndConnect} initial={editInitial} defaults={settings || {}} />
      )}
      <ApprovalDialog
        confirmation={confirmationPrompt}
        session={confirmationSession}
        queueLength={confirmations.length}
        busy={confirmationBusy === confirmationPrompt?.id}
        onResolve={resolveConfirmation}
        onPostpone={confirmation => setDismissedConfirmationId(confirmation.id)}
      />
      <HostKeyDialog
        challenge={hostKeyPrompt?.challenge}
        busy={hostKeyBusy}
        onCancel={() => setHostKeyPrompt(null)}
        onTrust={trustHostKey}
      />
      <SudoPasswordDialog />
    </div>
  );
}
