import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import App from './App.jsx';

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  emit: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen, emit: mocks.emit }));
vi.mock('./components/Sidebar.jsx', () => ({
  default: ({ activeId }) => <div data-testid="sidebar">当前侧边栏会话：{activeId}</div>,
}));
vi.mock('./components/BottomPanel.jsx', () => ({ default: () => null }));
vi.mock('./components/TerminalWorkspace.jsx', () => ({
  default: ({ activeId }) => <div data-testid="workspace-active-session">{activeId || 'none'}</div>,
}));

const baseSettings = {
  default_policy: 'Recommended',
  global_blocklist: ['(?i)rm\\s+-rf'],
  confirm_list: [],
  mcp_enabled: true,
  mcp_host: '127.0.0.1',
  mcp_port: 8767,
  mcp_autostart: true,
  attach_token_ttl_minutes: 30,
  mcp_require_attach_approval: true,
  idle_detach_minutes: 10,
  attach_token_visible: true,
  theme: 'dark',
  log_ai_commands: true,
  log_retention_days: 7,
  startup_auto_connect: false,
  show_onboarding: true,
  terminal_font_size: 14,
  copy_on_select: false,
  confirm_multiline_paste: true,
  links_enabled: true,
  default_keepalive_secs: 30,
  auto_reconnect: true,
  reconnect_max_attempts: 5,
  connect_timeout_secs: 30,
  terminal_log_default_enabled: false,
  log_redaction_enabled: true,
  future_mcp_setting: { preserve: 'unchanged' },
};
const basePrefs = {
  theme: 'dark',
  terminal_font_size: 14,
  copy_on_select: false,
  confirm_multiline_paste: true,
  links_enabled: true,
  bottom_panel_height: 280,
};
const sessions = [
  {
    config: { id: 'a', name: 'Alpha', group: '默认分组', username: 'root', host: 'a.example', port: 22 },
    status: { connected: false },
  },
  {
    config: { id: 'b', name: 'Beta', group: '生产', username: 'ops', host: 'b.example', port: 22 },
    status: { connected: true },
  },
];

beforeEach(() => {
  localStorage.clear();
  mocks.invoke.mockReset().mockImplementation(command => {
    if (command === 'get_global_settings') return Promise.resolve({ ...baseSettings });
    if (command === 'get_ui_prefs') return Promise.resolve({ ...basePrefs });
    if (command === 'list_sessions') return Promise.resolve(sessions);
    if (command === 'mcp_status') return Promise.resolve({ listening: false, host: '127.0.0.1', port: 8767 });
    if (command === 'take_restore_sessions_status') return Promise.resolve({});
    if (command === 'list_confirmations' || command === 'list_ai_events') return Promise.resolve([]);
    if (command === 'update_global_settings' || command === 'save_ui_prefs') return Promise.resolve(undefined);
    return Promise.resolve({});
  });
  mocks.listen.mockReset().mockImplementation(() => Promise.resolve(vi.fn()));
  mocks.emit.mockReset().mockResolvedValue(undefined);
});

describe('App 阶段 6 集成', () => {
  it('完成首次引导时只调用一次更新并保留全部设置、UI 偏好和 MCP 配置', async () => {
    render(<App />);
    expect(await screen.findByRole('dialog', { name: '新建一个 SSH 会话' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '下一步' }));
    fireEvent.click(screen.getByRole('button', { name: '下一步' }));
    fireEvent.click(screen.getByRole('button', { name: '完成并开始使用' }));

    await waitFor(() => {
      expect(mocks.invoke).toHaveBeenCalledWith('update_global_settings', {
        settings: { ...baseSettings, show_onboarding: false },
        uiPrefs: basePrefs,
      });
    });
    expect(mocks.invoke.mock.calls.filter(call => call[0] === 'update_global_settings')).toHaveLength(1);
    expect(mocks.invoke.mock.calls.filter(call => call[0] === 'save_ui_prefs')).toHaveLength(0);
    expect(await screen.findByTestId('workspace-active-session')).toHaveTextContent('none');
    expect(screen.queryByRole('dialog', { name: '打开 SFTP 管理文件' })).not.toBeInTheDocument();
  });

  it('show_onboarding=false 时不显示引导，Ctrl+K 可搜索并切换会话', async () => {
    mocks.invoke.mockImplementation(command => {
      if (command === 'get_global_settings') return Promise.resolve({ ...baseSettings, show_onboarding: false });
      if (command === 'get_ui_prefs') return Promise.resolve({ ...basePrefs });
      if (command === 'list_sessions') return Promise.resolve(sessions);
      if (command === 'mcp_status') return Promise.resolve({ listening: false, host: '127.0.0.1', port: 8767 });
      if (command === 'take_restore_sessions_status') return Promise.resolve({});
      if (command === 'list_confirmations' || command === 'list_ai_events') return Promise.resolve([]);
      return Promise.resolve({});
    });
    render(<App />);
    expect(await screen.findByTestId('workspace-active-session')).toHaveTextContent('none');
    expect(screen.queryByRole('dialog', { name: '新建一个 SSH 会话' })).not.toBeInTheDocument();

    fireEvent.keyDown(window, { key: 'k', code: 'KeyK', ctrlKey: true });
    const input = screen.getByRole('combobox', { name: '搜索命令' });
    fireEvent.change(input, { target: { value: 'b.example' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    await waitFor(() => expect(screen.getByTestId('workspace-active-session')).toHaveTextContent('b'));
    expect(screen.queryByRole('dialog', { name: '命令面板' })).not.toBeInTheDocument();
  });

  it('命令连接失败时通过统一 alert Toast 保留后端错误', async () => {
    mocks.invoke.mockImplementation(command => {
      if (command === 'get_global_settings') return Promise.resolve({ ...baseSettings, show_onboarding: false });
      if (command === 'get_ui_prefs') return Promise.resolve({ ...basePrefs });
      if (command === 'list_sessions') return Promise.resolve(sessions);
      if (command === 'mcp_status') return Promise.resolve({ listening: false, host: '127.0.0.1', port: 8767 });
      if (command === 'take_restore_sessions_status') return Promise.resolve({});
      if (command === 'list_confirmations' || command === 'list_ai_events') return Promise.resolve([]);
      if (command === 'connect_session') return Promise.reject(new Error('backend authentication denied'));
      return Promise.resolve({});
    });
    render(<App />);
    await screen.findByTestId('workspace-active-session');
    fireEvent.keyDown(window, { key: 'k', code: 'KeyK', ctrlKey: true });
    const switchInput = screen.getByRole('combobox', { name: '搜索命令' });
    fireEvent.change(switchInput, { target: { value: '切换到 Alpha' } });
    fireEvent.keyDown(switchInput, { key: 'Enter' });
    await waitFor(() => expect(screen.getByTestId('workspace-active-session')).toHaveTextContent('a'));

    fireEvent.keyDown(window, { key: 'k', code: 'KeyK', ctrlKey: true });
    const input = screen.getByRole('combobox', { name: '搜索命令' });
    fireEvent.change(input, { target: { value: '连接当前会话' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('backend authentication denied');
    expect(alert).toHaveAttribute('aria-live', 'assertive');
  });

  it('主题切换立即写入共享偏好，供 SFTP 的 storage 监听实时跟随', async () => {
    mocks.invoke.mockImplementation(command => {
      if (command === 'get_global_settings') return Promise.resolve({ ...baseSettings, show_onboarding: false });
      if (command === 'get_ui_prefs') return Promise.resolve({ ...basePrefs });
      if (command === 'list_sessions') return Promise.resolve(sessions);
      if (command === 'mcp_status') return Promise.resolve({ listening: false, host: '127.0.0.1', port: 8767 });
      if (command === 'take_restore_sessions_status') return Promise.resolve({});
      if (command === 'list_confirmations' || command === 'list_ai_events') return Promise.resolve([]);
      if (command === 'save_ui_prefs') return Promise.resolve(undefined);
      return Promise.resolve({});
    });
    render(<App />);
    await screen.findByTestId('workspace-active-session');
    fireEvent.click(screen.getByRole('button', { name: '切换主题' }));
    await waitFor(() => expect(localStorage.getItem('chena-shell-theme')).toBe('light'));
    expect(document.documentElement.dataset.theme).toBe('light');
    expect(mocks.invoke).toHaveBeenCalledWith('save_ui_prefs', { theme: 'light' });
  });
});
