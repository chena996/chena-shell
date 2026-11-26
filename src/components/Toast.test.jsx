import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import ToastRegion from './Toast.jsx';

describe('ToastRegion', () => {
  it('以 live region 同时展示通知和完整后端错误，并可分别关闭', () => {
    const onDismissError = vi.fn();
    const onDismissNotice = vi.fn();
    const backendError = '连接失败: backend permission denied\nhost key unchanged';
    render(<ToastRegion
      notice="安全升级完成"
      error={backendError}
      onDismissError={onDismissError}
      onDismissNotice={onDismissNotice}
    />);
    expect(screen.getByRole('status')).toHaveAttribute('aria-live', 'polite');
    expect(screen.getByRole('alert')).toHaveAttribute('aria-live', 'assertive');
    expect(screen.getByRole('alert').querySelector('.toast__message').textContent).toBe(backendError);
    fireEvent.click(screen.getByRole('button', { name: '关闭提示' }));
    fireEvent.click(screen.getByRole('button', { name: '关闭错误' }));
    expect(onDismissNotice).toHaveBeenCalledOnce();
    expect(onDismissError).toHaveBeenCalledOnce();
  });
});
