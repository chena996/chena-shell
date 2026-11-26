import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import BottomPanel, {
  appendMonitorHistory, getNetworkHover, getNetworkTooltipPosition, LogTab, selectMonitorHistory,
} from './BottomPanel.jsx';

const mocks = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen }));

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.listen.mockReset();
  mocks.listen.mockResolvedValue(vi.fn());
});

describe('网络曲线悬停映射', () => {
  it('发送读取 netOut，接收读取 netIn，并计算采样点位置', () => {
    const hover = getNetworkHover([100, 200, 300], [1000, 2000, 3000], { idx: 1 });

    expect(hover).toMatchObject({ idx: 1, x: 300, send: 2000, recv: 200, ago: 1 });
    expect(hover.sendY).toBeLessThan(hover.recvY);
  });

  it('索引越界时固定到最后一个采样点', () => {
    const hover = getNetworkHover([10, 20], [30, 40], { idx: 99 });

    expect(hover).toMatchObject({ idx: 1, x: 600, send: 40, recv: 20, ago: 0 });
  });

  it('不足两个采样点时不显示悬停取值', () => {
    expect(getNetworkHover([10], [20], { idx: 0 })).toBeNull();
    expect(getNetworkHover([], [], null)).toBeNull();
  });

  it('悬浮框在右侧、底部和过小的图表内都不会越界', () => {
    expect(getNetworkTooltipPosition({ x: 770, y: 20, w: 800, h: 100 })).toEqual({ left: 646, top: 32 });
    expect(getNetworkTooltipPosition({ x: 100, y: 95, w: 800, h: 100 })).toEqual({ left: 112, top: 23 });
    expect(getNetworkTooltipPosition({ x: 0, y: 0, w: 100, h: 40 })).toEqual({ left: 4, top: 4 });
    expect(getNetworkTooltipPosition(null)).toBeNull();
  });
});

function monitorFixture(ts, overrides = {}) {
  return {
    session_id: 's1', ts, cpu_pct: 0, mem_pct: 50, mem_used_mb: 512, mem_total_mb: 1024,
    disk_pct: 25, disk_used_gb: 1, disk_total_gb: 4, net_in_bytes_per_s: 0,
    net_out_bytes_per_s: 0, load_1m: 0.1, load_5m: 0.2, load_15m: 0.3,
    online: true, cores: 4, last_success_at: ts, stale: false, last_error: null,
    platform: 'linux', baseline_ready: false, disks: [], top_cpu_processes: [], top_mem_processes: [],
    ...overrides,
  };
}

describe('监控历史与状态', () => {
  it('首轮 baseline 未就绪时保留真实内存数据但不写入伪造 CPU/网络零速率', () => {
    const now = Date.parse('2026-11-19T00:00:00Z');
    const history = appendMonitorHistory([], monitorFixture(new Date(now).toISOString()), now);

    expect(history).toHaveLength(1);
    expect(history[0]).toMatchObject({ ts: now, cpu: null, netIn: null, netOut: null, mem: 50, disk: 25 });
  });

  it('平台不支持负载平均值时不写入伪造的零负载历史点', () => {
    const now = Date.parse('2026-11-19T00:00:00Z');
    const history = appendMonitorHistory([], monitorFixture(new Date(now).toISOString(), {
      platform: 'windows',
      capabilities: { disk_by_mount: true, process_top: true, load_average: false },
    }), now);

    expect(history).toHaveLength(1);
    expect(history[0].load).toBeNull();
    expect(history[0].mem).toBe(50);
  });

  it('按真实时间戳最多保留 15 分钟，并正确选择 1m/5m/15m 窗口', () => {
    const now = Date.parse('2026-11-19T00:20:00Z');
    const old = [
      { ts: now - 16 * 60_000 },
      { ts: now - 14 * 60_000 },
      { ts: now - 2 * 60_000 },
    ];
    const retained = appendMonitorHistory(old, monitorFixture(new Date(now).toISOString(), { baseline_ready: true }), now);
    expect(retained.map(point => point.ts)).toEqual([now - 14 * 60_000, now - 2 * 60_000, now]);

    const points = [
      { ts: now - 10 * 60_000 }, { ts: now - 2 * 60_000 },
      { ts: now - 61_000 }, { ts: now },
    ];
    expect(selectMonitorHistory(points, 1, now)).toHaveLength(1);
    expect(selectMonitorHistory(points, 5, now)).toHaveLength(3);
    expect(selectMonitorHistory(points, 15, now)).toHaveLength(4);
  });

  it('明确显示 stale、last_error、last_success_at，首轮不显示伪造零速率', () => {
    const ts = new Date().toISOString();
    const { rerender } = render(<BottomPanel session={{ id: 's1' }} monitor={monitorFixture(ts)} />);
    expect(screen.getByText('首轮基线未就绪')).toBeInTheDocument();
    expect(screen.queryByText('0%')).not.toBeInTheDocument();
    expect(screen.queryByText('0 B/s')).not.toBeInTheDocument();
    expect(screen.getByText(/最近成功采样：/)).toBeInTheDocument();

    rerender(<BottomPanel session={{ id: 's1' }} monitor={monitorFixture(ts, {
      baseline_ready: true, stale: true, last_error: '采样超时',
    })} />);
    expect(screen.getByText('数据已过期')).toBeInTheDocument();
    expect(screen.getByText(/采样错误：采样超时/)).toBeInTheDocument();
    expect(screen.getByText(/最近成功采样：/)).toBeInTheDocument();
  });

  it('按跨平台能力展示 Windows 磁盘与进程，并隐藏不支持的负载平均值', async () => {
    const user = userEvent.setup();
    const ts = new Date().toISOString();
    const windows = monitorFixture(ts, {
      platform: 'windows',
      baseline_ready: true,
      capabilities: { disk_by_mount: true, process_top: true, load_average: false },
      disks: [{
        filesystem: 'C:', mount_point: 'C:\\', total_kb: 4_194_304,
        used_kb: 1_048_576, used_pct: 25,
      }],
      top_cpu_processes: [{ pid: 42, name: 'Code Helper', cpu_pct: 12.5, mem_pct: 8 }],
      top_mem_processes: [{ pid: 84, name: 'Windows Service', cpu_pct: 2, mem_pct: 20 }],
    });
    const { rerender } = render(<BottomPanel session={{ id: 's1' }} monitor={windows} />);

    const loadCard = screen.getByText('负载').parentElement.parentElement;
    expect(within(loadCard).getAllByText('—')).toHaveLength(2);
    expect(within(loadCard).queryByText('0.10')).not.toBeInTheDocument();
    expect(within(loadCard).queryByText('5m 0.2')).not.toBeInTheDocument();

    await user.click(screen.getByText(/磁盘挂载点（1）与进程 TOP/));
    expect(screen.getByText('C:\\')).toBeInTheDocument();
    expect(screen.getByTitle('Code Helper (42)')).toBeInTheDocument();
    expect(screen.getByTitle('Windows Service (84)')).toBeInTheDocument();
    expect(screen.queryByText(/仅支持 Linux/)).not.toBeInTheDocument();

    rerender(<BottomPanel session={{ id: 's1' }} monitor={monitorFixture(ts, {
      platform: 'windows',
      capabilities: { disk_by_mount: false, process_top: false, load_average: false },
    })} />);
    expect(screen.getByText('当前平台不支持按挂载点采集磁盘')).toBeInTheDocument();
    expect(screen.getAllByText('当前平台不支持进程 TOP 采集')).toHaveLength(2);
  });

  it('时间窗可切换，切换 session 会清空历史采样点', async () => {
    const user = userEvent.setup();
    const now = Date.now();
    const first = monitorFixture(new Date(now - 1000).toISOString(), {
      baseline_ready: true, net_in_bytes_per_s: 100, net_out_bytes_per_s: 200,
    });
    const second = monitorFixture(new Date(now).toISOString(), {
      baseline_ready: true, net_in_bytes_per_s: 300, net_out_bytes_per_s: 400,
    });
    const { rerender } = render(<BottomPanel session={{ id: 's1' }} monitor={first} />);
    rerender(<BottomPanel session={{ id: 's1' }} monitor={second} />);
    expect(await screen.findByText(/最近 1 分钟 · 2 个采样点/)).toBeInTheDocument();
    expect(document.querySelector('svg[viewBox="0 0 600 110"]').parentElement)
      .toHaveStyle({ minHeight: '80px' });
    await user.click(screen.getByRole('button', { name: '5m' }));
    expect(screen.getByText(/最近 5 分钟 · 2 个采样点/)).toBeInTheDocument();

    rerender(<BottomPanel session={{ id: 's2' }} monitor={null} />);
    expect(await screen.findByText(/最近 5 分钟 · 0 个采样点/)).toBeInTheDocument();
  });
});

describe('日志事件驱动刷新', () => {
  it('首次读取列表和首个文件，匹配事件刷新且没有轮询，卸载时清理监听', async () => {
    const unlisten = vi.fn();
    let onLogAppended;
    let version = 1;
    mocks.listen.mockImplementation(async (_event, callback) => {
      onLogAppended = callback;
      return unlisten;
    });
    mocks.invoke.mockImplementation(async command => {
      if (command === 'list_session_logs') return [{ name: 'session.log', size: version }];
      if (command === 'read_session_log') return `日志版本 ${version}`;
      return null;
    });
    const interval = vi.spyOn(globalThis, 'setInterval');
    const { unmount } = render(<LogTab session={{ id: 's1', connected: true }} />);

    expect(await screen.findByText('日志版本 1')).toBeInTheDocument();
    await waitFor(() => expect(onLogAppended).toBeTypeOf('function'));
    version = 2;
    await act(async () => {
      await onLogAppended({ payload: { session_id: 's1', name: 'session.log' } });
    });
    expect(await screen.findByText('日志版本 2')).toBeInTheDocument();
    expect(mocks.invoke.mock.calls.filter(([command]) => command === 'list_session_logs')).toHaveLength(2);
    expect(mocks.invoke.mock.calls.filter(([command]) => command === 'read_session_log')).toHaveLength(2);

    const callCount = mocks.invoke.mock.calls.length;
    await act(async () => {
      await onLogAppended({ payload: { session_id: 'other', name: 'session.log' } });
    });
    expect(mocks.invoke).toHaveBeenCalledTimes(callCount);
    expect(interval.mock.calls.filter(([, delay]) => delay === 5000)).toHaveLength(0);
    unmount();
    await act(async () => {});
    expect(unlisten).toHaveBeenCalledOnce();
  });

  it('较晚返回的旧文件响应不会覆盖当前选中文件', async () => {
    const user = userEvent.setup();
    const pending = {};
    mocks.invoke.mockImplementation((command, args) => {
      if (command === 'list_session_logs') {
        return Promise.resolve([{ name: 'a.log', size: 1 }, { name: 'b.log', size: 1 }]);
      }
      if (command === 'read_session_log') {
        return new Promise(resolve => { pending[args.name] = resolve; });
      }
      return Promise.resolve(null);
    });
    render(<LogTab session={{ id: 's1', connected: true }} />);

    await waitFor(() => expect(pending['a.log']).toBeTypeOf('function'));
    await user.click(screen.getByText('b.log'));
    await waitFor(() => expect(pending['b.log']).toBeTypeOf('function'));
    await act(async () => { pending['b.log']('当前文件内容'); });
    expect(await screen.findByText('当前文件内容')).toBeInTheDocument();
    await act(async () => { pending['a.log']('陈旧响应'); });
    expect(screen.queryByText('陈旧响应')).not.toBeInTheDocument();
    expect(screen.getByText('当前文件内容')).toBeInTheDocument();
  });

  it('已有会话可直接启用日志，已连接时先保存配置再重连', async () => {
    const user = userEvent.setup();
    const onSessionUpdated = vi.fn().mockResolvedValue(undefined);
    mocks.invoke.mockImplementation(async command => {
      if (command === 'list_session_logs') return [];
      return undefined;
    });
    render(<LogTab
      session={{ id: 's1', connected: true, log_enabled: false }}
      onSessionUpdated={onSessionUpdated}
    />);

    await user.click(await screen.findByRole('button', { name: '启用日志并重连' }));
    await waitFor(() => expect(onSessionUpdated).toHaveBeenCalledOnce());
    expect(mocks.invoke).toHaveBeenCalledWith('update_session', {
      sessionId: 's1', config: { log_enabled: true },
    });
    const commands = mocks.invoke.mock.calls.map(([command]) => command)
      .filter(command => ['update_session', 'disconnect_session', 'connect_session'].includes(command));
    expect(commands).toEqual(['update_session', 'disconnect_session', 'connect_session']);
  });

  it('切换到 AI 日志后读取 AI 回显文件，不再受终端日志开关限制', async () => {
    const user = userEvent.setup();
    mocks.invoke.mockImplementation(async (command) => {
      if (command === 'list_session_logs') return [];
      if (command === 'list_ai_terminal_logs') {
        return [{ name: '20261123001000_s1_ai_host_22.log', size: 128 }];
      }
      if (command === 'read_ai_terminal_log') return '[AI 执行] whoami\n[AI 结果 · 成功]\nroot';
      return undefined;
    });
    render(<LogTab session={{ id: 's1', connected: true, log_enabled: false }} />);

    await user.click(screen.getByRole('button', { name: 'AI 日志' }));

    expect(await screen.findByText(/\[AI 执行\] whoami/)).toBeInTheDocument();
    expect(screen.getByText(/AI 回显始终记录/)).toBeInTheDocument();
    expect(mocks.invoke).toHaveBeenCalledWith('list_ai_terminal_logs', { sessionId: 's1' });
    expect(mocks.invoke).toHaveBeenCalledWith('read_ai_terminal_log', {
      sessionId: 's1', name: '20261123001000_s1_ai_host_22.log',
    });
    expect(screen.queryByRole('button', { name: '启用终端日志' })).not.toBeInTheDocument();
  });
});

describe('AI 审批与活动展示', () => {
  it('展示拒绝、超时以及来源、规则和原因', async () => {
    const user = userEvent.setup();
    render(<BottomPanel
      session={{ id: 's1', connected: true }}
      monitor={null}
      aiEvents={[
        {
          id: 'event-1', ts: '2026-01-01T00:00:00Z', source: 'Codex', rule: 'strict policy',
          denial_reason: '人工拒绝', kind: { kind: 'Rejected', command: 'echo [REDACTED]', reason: '人工拒绝' },
        },
        {
          id: 'event-2', ts: '2026-01-01T00:00:01Z', source: 'Codex', rule: 'strict policy',
          denial_reason: '30 秒未处理，已默认拒绝', kind: { kind: 'Expired', command: 'echo [REDACTED]' },
        },
      ]}
    />);

    await user.click(screen.getByRole('button', { name: /AI 活动/ }));
    expect(screen.getByText('已拒绝')).toBeInTheDocument();
    expect(screen.getByText('已超时')).toBeInTheDocument();
    expect(screen.getAllByText(/来源：Codex/)).toHaveLength(2);
    expect(screen.getAllByText(/规则：严格安全策略/)).toHaveLength(2);
    expect(screen.getByText(/原因：人工拒绝/)).toBeInTheDocument();
    expect(screen.getByText(/原因：30 秒未处理，已默认拒绝/)).toBeInTheDocument();
  });

  it('当前会话可直接从 AI 活动面板批准一次', async () => {
    const user = userEvent.setup();
    const onResolveConfirmation = vi.fn();
    const confirmation = {
      id: 'cf-1', kind: 'exec', session_id: 's1', client_id: 'client-a', client_name: 'Codex',
      reason: 'command requires human approval', command_preview: 'echo [REDACTED]', rule: 'strict policy',
      expires_at: new Date(Date.now() + 30_000).toISOString(), status: 'pending',
    };
    render(<BottomPanel
      session={{ id: 's1', connected: true }}
      monitor={null}
      confirmations={[confirmation]}
      onResolveConfirmation={onResolveConfirmation}
    />);

    await user.click(screen.getByRole('button', { name: /AI 活动/ }));
    expect(screen.getByText('echo [REDACTED]')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '批准一次' }));
    expect(onResolveConfirmation).toHaveBeenCalledWith(confirmation, true);
  });
});
