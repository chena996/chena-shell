import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import Onboarding from './Onboarding.jsx';

describe('Onboarding', () => {
  it('依次展示三步并在完成时请求持久化', () => {
    const onFinish = vi.fn();
    render(<Onboarding busy={false} onFinish={onFinish} onDismiss={vi.fn()} />);
    expect(screen.getByRole('dialog', { name: '新建一个 SSH 会话' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '下一步' }));
    expect(screen.getByRole('dialog', { name: '连接并开始工作' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '下一步' }));
    expect(screen.getByRole('dialog', { name: '打开 SFTP 管理文件' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '完成并开始使用' }));
    expect(onFinish).toHaveBeenCalledOnce();
  });

  it('暂时跳过与不再显示使用不同的关闭语义', () => {
    const onDismiss = vi.fn();
    render(<Onboarding busy={false} onFinish={vi.fn()} onDismiss={onDismiss} />);
    fireEvent.click(screen.getByRole('button', { name: '暂时跳过' }));
    expect(onDismiss).toHaveBeenLastCalledWith(false);
    fireEvent.click(screen.getByRole('checkbox', { name: '不再显示此引导' }));
    fireEvent.click(screen.getByRole('button', { name: '不再显示并跳过' }));
    expect(onDismiss).toHaveBeenLastCalledWith(true);
    expect(onDismiss).toHaveBeenCalledTimes(2);
  });

  it('Escape 可退出且保存期间禁用操作', () => {
    const onDismiss = vi.fn();
    const { rerender } = render(<Onboarding busy={false} onFinish={vi.fn()} onDismiss={onDismiss} />);
    fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });
    expect(onDismiss).toHaveBeenCalledWith(false);
    rerender(<Onboarding busy onFinish={vi.fn()} onDismiss={onDismiss} />);
    expect(screen.getByRole('button', { name: '暂时跳过' })).toBeDisabled();
    fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});
