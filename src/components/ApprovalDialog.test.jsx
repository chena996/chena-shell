import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import ApprovalDialog from './ApprovalDialog.jsx';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(() => Promise.resolve(null)),
}));

const confirmation = {
  id: 'cf-1',
  kind: 'exec',
  session_id: 's1',
  client_id: 'client-a',
  client_name: 'Codex',
  reason: 'command requires human approval',
  command_preview: 'echo [REDACTED]',
  rule: 'strict policy',
  created_at: '2026-01-01T00:00:00Z',
  expires_at: new Date(Date.now() + 30_000).toISOString(),
  status: 'pending',
};

const session = {
  id: 's1',
  name: '生产机',
  host: '10.0.0.8',
  port: 22,
  username: 'root',
};

describe('MCP 人工审批弹窗', () => {
  it('展示来源、目标会话、脱敏命令和触发规则', () => {
    render(<ApprovalDialog
      confirmation={confirmation}
      session={session}
      queueLength={2}
      onResolve={vi.fn()}
      onPostpone={vi.fn()}
    />);

    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByText('Codex')).toBeInTheDocument();
    expect(screen.getByText(/生产机 · root@10\.0\.0\.8:22/)).toBeInTheDocument();
    expect(screen.getByText('echo [REDACTED]')).toBeInTheDocument();
    expect(screen.getByText('严格安全策略')).toBeInTheDocument();
    expect(screen.getByText(/共 2 条待处理/)).toBeInTheDocument();
  });

  it('明确区分单次批准、拒绝和稍后处理', async () => {
    const user = userEvent.setup();
    const onResolve = vi.fn();
    const onPostpone = vi.fn();
    render(<ApprovalDialog
      confirmation={confirmation}
      session={session}
      onResolve={onResolve}
      onPostpone={onPostpone}
    />);

    await user.click(screen.getByRole('button', { name: '批准一次' }));
    expect(onResolve).toHaveBeenCalledWith(confirmation, true);
    await user.click(screen.getByRole('button', { name: '拒绝' }));
    expect(onResolve).toHaveBeenCalledWith(confirmation, false);
    await user.click(screen.getByRole('button', { name: '稍后处理' }));
    expect(onPostpone).toHaveBeenCalledWith(confirmation);
  });

  it('到期后禁止批准或拒绝，等待后端默认拒绝', () => {
    render(<ApprovalDialog
      confirmation={{ ...confirmation, expires_at: new Date(Date.now() - 1000).toISOString() }}
      session={session}
      onResolve={vi.fn()}
      onPostpone={vi.fn()}
    />);

    expect(screen.getByText('已过期')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '批准一次' })).toBeDisabled();
    expect(screen.getByRole('button', { name: '拒绝' })).toBeDisabled();
  });

  it('鼠标移入时暂停倒计时并通知后端，移出后恢复', async () => {
    const user = userEvent.setup();
    render(<ApprovalDialog
      confirmation={confirmation}
      session={session}
      onResolve={vi.fn()}
      onPostpone={vi.fn()}
    />);
    const dialog = screen.getByRole('dialog');

    await user.hover(dialog);
    expect(screen.getByText(/倒计时已暂停/)).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith('set_confirmation_hold', {
      confirmationId: 'cf-1',
      hold: true,
    });

    await user.unhover(dialog);
    expect(screen.queryByText(/倒计时已暂停/)).not.toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith('set_confirmation_hold', {
      confirmationId: 'cf-1',
      hold: false,
    });
  });
});
