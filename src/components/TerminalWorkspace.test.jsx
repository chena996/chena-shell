import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import TerminalWorkspace from './TerminalWorkspace.jsx';

vi.mock('./Terminal.jsx', () => ({
  default: function TerminalMock({ session, channelId, onConnect, onOsc7 }) {
    const id = session?.id || 'none';
    return (
      <div data-testid={`terminal-${id}`} data-channel={channelId || 'default'}>
        <span>独立终端：{id}</span>
        <button type="button" onClick={onConnect}>连接 {id}</button>
        <button type="button" onClick={() => onOsc7({ path: `/srv/${id}` })}>OSC7 {id}</button>
      </div>
    );
  },
}));

vi.mock('./AiTerminal.jsx', () => ({
  default: function AiTerminalMock({ sessionId, attached, active, fontSize }) {
    return (
      <div
        data-testid={`ai-terminal-${sessionId}`}
        data-active={active}
        data-font-size={fontSize}
      >AI 只读 {attached ? '附着中' : '历史'}</div>
    );
  },
}));

const sessions = [
  { id: 'a', name: 'Alpha', username: 'root', host: 'a.example', port: 22, connected: true },
  { id: 'b', name: 'Beta', username: 'ops', host: 'b.example', port: 22, connected: false },
];

function renderWorkspace(overrides = {}) {
  const props = {
    sessions,
    activeId: 'a',
    onActiveSessionChange: vi.fn(),
    connectingId: null,
    onToggleConnect: vi.fn(),
    onDisconnectSession: vi.fn().mockResolvedValue(undefined),
    onKick: vi.fn(),
    theme: 'dark',
    prefs: {},
    onOsc7: vi.fn(),
    onNewSession: vi.fn(),
    newTabRequest: null,
    ...overrides,
  };
  const view = render(<TerminalWorkspace {...props} />);
  return { ...view, props };
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe('TerminalWorkspace', () => {
  it('AI 附着时创建独立只读标签，关闭后不自动重现，重新附着可恢复', async () => {
    const aiSessions = [
      { ...sessions[0], ai_attached: true, ai_connection_id: '3:primary', ai_mode: 'write' },
      sessions[1],
    ];
    const { props, rerender } = renderWorkspace({
      sessions: aiSessions,
      activeId: 'a',
      prefs: { terminal_font_size: 18 },
    });

    expect(await screen.findByText('Alpha · AI')).toBeInTheDocument();
    expect(screen.getByTestId('ai-terminal-a')).toBeInTheDocument();
    expect(screen.getByText('只读')).toBeInTheDocument();
    expect(screen.getByTestId('ai-terminal-a').parentElement).not.toHaveAttribute('hidden');
    expect(screen.getByTestId('ai-terminal-a').parentElement).toHaveClass('terminal-workspace__panel--ai');
    expect(screen.getByTestId('ai-terminal-a')).toHaveAttribute('data-active', 'true');
    expect(screen.getByTestId('ai-terminal-a')).toHaveAttribute('data-font-size', '18');

    // 关闭 AI 标签后，同一附着状态下的刷新不应让它自动重现。
    fireEvent.click(screen.getByLabelText('关闭 AI 标签 Alpha · AI'));
    expect(screen.queryByText('Alpha · AI')).not.toBeInTheDocument();
    rerender(<TerminalWorkspace {...props} sessions={[...aiSessions]} />);
    expect(screen.queryByText('Alpha · AI')).not.toBeInTheDocument();

    // 断开再重新附着后，应重新出现。
    rerender(<TerminalWorkspace {...props} sessions={[{ ...aiSessions[0], ai_attached: false, ai_mode: null, ai_connection_id: null }, sessions[1]]} />);
    rerender(<TerminalWorkspace {...props} sessions={[...aiSessions]} />);
    expect(await screen.findByText('Alpha · AI')).toBeInTheDocument();
  });

  it('diag 只读观察者同样创建独立 AI 标签', async () => {
    const diagSessions = [
      {
        ...sessions[0],
        ai_attached: false,
        ai_connection_id: '3:ch1',
        ai_mode: 'diag',
        ai_readers: 1,
      },
      sessions[1],
    ];
    renderWorkspace({ sessions: diagSessions, activeId: 'a' });

    expect(await screen.findByText('Alpha · AI')).toBeInTheDocument();
    expect(screen.getByTestId('ai-terminal-a')).toBeInTheDocument();
    expect(screen.getByText('只读')).toBeInTheDocument();
  });

  it('标签页右键复用会话菜单，可复制 SSH 渠道和关闭标签', async () => {
    const props = { onOpenInNewTab: vi.fn(), onEdit: vi.fn(), onDelete: vi.fn() };
    renderWorkspace(props);

    fireEvent.contextMenu(screen.getAllByRole('tab')[0]);
    expect(screen.getByText('复制 SSH 渠道')).toBeInTheDocument();
    expect(screen.getByText('关闭标签')).toBeInTheDocument();

    fireEvent.click(screen.getByText('复制 SSH 渠道'));
    expect(props.onOpenInNewTab).toHaveBeenCalledWith(expect.objectContaining({ id: 'a' }));
    expect(screen.queryByText('复制 SSH 渠道')).not.toBeInTheDocument();
  });

  it('分屏创建独立 Terminal，连接和 OSC7 都使用当前会话', async () => {
    const { props } = renderWorkspace();
    fireEvent.click(screen.getByRole('button', { name: '拆分当前终端' }));
    expect(screen.getAllByTestId('terminal-a')).toHaveLength(2);

    fireEvent.click(screen.getAllByRole('button', { name: '连接 a', exact: true })[1]);
    expect(props.onToggleConnect.mock.calls.map(call => call[0].id)).toEqual(['a']);
    fireEvent.click(screen.getAllByRole('button', { name: 'OSC7 a', exact: true })[1]);
    expect(props.onOsc7).toHaveBeenCalledWith('a', { path: '/srv/a' });

    fireEvent.click(screen.getByRole('button', { name: '关闭窗格 2' }));
    expect(screen.getAllByTestId('terminal-a')).toHaveLength(1);
    expect(props.onActiveSessionChange).toHaveBeenLastCalledWith('a');
  });

  it('外部选择会话会打开或激活标签，来回切换不卸载终端', async () => {
    const { props, rerender } = renderWorkspace();
    const originalTerminal = screen.getByTestId('terminal-a');

    rerender(<TerminalWorkspace {...props} activeId="b" />);
    const betaTab = await screen.findByRole('tab', { name: 'Beta' });
    expect(betaTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getAllByRole('tab')).toHaveLength(2);
    expect(screen.getByTestId('terminal-a')).toBe(originalTerminal);
    expect(originalTerminal.closest('[role="tabpanel"]')).not.toBeVisible();

    const alphaTab = screen.getByRole('tab', { name: 'Alpha' });
    fireEvent.click(alphaTab);
    expect(props.onActiveSessionChange).toHaveBeenLastCalledWith('a');
    rerender(<TerminalWorkspace {...props} activeId="a" />);
    expect(alphaTab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getAllByRole('tab')).toHaveLength(2);
    expect(screen.getByTestId('terminal-b').closest('[role="tabpanel"]')).not.toBeVisible();
    expect(originalTerminal.closest('[role="tabpanel"]')).toBeVisible();

    fireEvent.keyDown(alphaTab, { key: 'ArrowRight' });
    expect(betaTab).toHaveFocus();
    expect(props.onActiveSessionChange).toHaveBeenLastCalledWith('b');
    rerender(<TerminalWorkspace {...props} activeId="b" />);
    expect(screen.getAllByRole('tab')).toHaveLength(2);
  });

  it('新建按钮打开新建会话，复用会话由外部请求创建独立标签', async () => {
    const { props, rerender } = renderWorkspace();
    fireEvent.click(screen.getByRole('button', { name: '新建会话' }));
    expect(props.onNewSession).toHaveBeenCalledOnce();

    rerender(<TerminalWorkspace {...props} newTabRequest={{ sessionId: 'a', channelId: 'ch1', nonce: 1 }} />);
    await waitFor(() => expect(screen.getAllByRole('tab')).toHaveLength(2));
    expect(props.onActiveSessionChange).toHaveBeenLastCalledWith('a');
    expect(screen.getAllByRole('tab').filter(tab => tab.textContent.includes('Alpha'))).toHaveLength(2);
    expect(screen.getByRole('tab', { name: 'Alpha [1]' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Alpha [2]' })).toBeInTheDocument();
    expect(screen.getAllByTestId('terminal-a').map(node => node.getAttribute('data-channel')))
      .toEqual(['default', 'ch1']);
  });

  it('主动连接完成后切换到该会话时，对应标签被激活', async () => {
    const { props, rerender } = renderWorkspace({ sessions: sessions.map(session => ({ ...session, connected: false })), activeId: 'a' });
    // Beta 后台连接：标签补齐但不抢焦点
    rerender(<TerminalWorkspace {...props} sessions={sessions.map(session => ({ ...session, connected: true }))} activeId="a" />);
    const betaTab = await screen.findByRole('tab', { name: 'Beta' });
    expect(betaTab).toHaveAttribute('aria-selected', 'false');

    // 用户主动连接 Beta 后，App 将其设为当前会话，标签应随之激活
    rerender(<TerminalWorkspace {...props} sessions={sessions.map(session => ({ ...session, connected: true }))} activeId="b" />);
    await waitFor(() => expect(screen.getByRole('tab', { name: 'Beta' })).toHaveAttribute('aria-selected', 'true'));
    expect(screen.getByRole('tab', { name: 'Alpha' })).toHaveAttribute('aria-selected', 'false');
  });

  it('自动连接但未选中的会话会补齐后台标签，不改变当前选择', async () => {
    const disconnected = sessions.map(session => ({ ...session, connected: false }));
    const { props, rerender } = renderWorkspace({ sessions: disconnected, activeId: null });
    expect(screen.getAllByRole('tab')).toHaveLength(1);

    rerender(<TerminalWorkspace {...props} sessions={sessions} activeId={null} />);
    expect(await screen.findByRole('tab', { name: 'Alpha' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: '未选择会话' })).toHaveAttribute('aria-selected', 'true');
    expect(props.onActiveSessionChange).not.toHaveBeenCalled();
  });

  it('同一连接有复制渠道标签时，关闭非最后标签不会断开 SSH', async () => {
    const { props, rerender } = renderWorkspace();
    rerender(<TerminalWorkspace {...props} newTabRequest={{ sessionId: 'a', channelId: 'ch1', nonce: 1 }} />);
    await waitFor(() => expect(screen.getAllByRole('tab')).toHaveLength(2));

    fireEvent.click(screen.getByRole('button', { name: '关闭标签 Alpha [2]' }));
    await waitFor(() => expect(screen.getAllByRole('tab')).toHaveLength(1));
    expect(props.onDisconnectSession).not.toHaveBeenCalled();
    expect(screen.getByRole('tab', { name: 'Alpha' })).toBeInTheDocument();
    expect(screen.getByTestId('terminal-a')).toBeInTheDocument();
    expect(screen.queryByRole('alertdialog', { name: '确认关闭并断开连接' })).not.toBeInTheDocument();
  });

  it('最后标签取消时全部保留，确认后才断连并留下空工作区', async () => {
    const { props, rerender } = renderWorkspace();
    fireEvent.click(screen.getByRole('button', { name: '关闭标签 Alpha' }));
    expect(await screen.findByRole('alertdialog', { name: '确认关闭并断开连接' })).toBeInTheDocument();
    expect(props.onDisconnectSession).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '取消', exact: true }));
    expect(screen.queryByRole('alertdialog', { name: '确认关闭并断开连接' })).not.toBeInTheDocument();
    expect(screen.getByTestId('terminal-a')).toBeInTheDocument();
    expect(props.onDisconnectSession).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '关闭标签 Alpha' }));
    fireEvent.click(await screen.findByRole('button', { name: '关闭并断开', exact: true }));
    await waitFor(() => expect(props.onDisconnectSession).toHaveBeenCalledWith('a'));
    expect(props.onActiveSessionChange).toHaveBeenLastCalledWith(null);
    rerender(<TerminalWorkspace {...props} activeId={null} />);
    expect(await screen.findByTestId('terminal-none')).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: '未选择会话' })).toHaveAttribute('aria-selected', 'true');
  });
});
