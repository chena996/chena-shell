import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import CommandPalette from './CommandPalette.jsx';
import MainMenu from './MainMenu.jsx';

const commands = [
  { id: 'new', label: '新建会话', description: '创建连接', run: vi.fn() },
  { id: 'settings', label: '打开设置', description: '修改偏好', shortcut: 'Ctrl+,', run: vi.fn() },
  { id: 'build', label: '切换到 Build', description: 'root@build.example:22', keywords: '生产', run: vi.fn() },
];

function renderPalette(overrides = {}) {
  return render(<CommandPalette commands={commands} onClose={vi.fn()} onActionError={vi.fn()} {...overrides} />);
}

describe('CommandPalette', () => {
  it('搜索会话并通过方向键、Enter 执行命令，ARIA 活动项保持同步', () => {
    const runNew = vi.fn();
    const runSettings = vi.fn();
    const onClose = vi.fn();
    renderPalette({
      onClose,
      commands: [
        { ...commands[0], run: runNew },
        { ...commands[1], run: runSettings },
        commands[2],
      ],
    });
    const input = screen.getByRole('combobox', { name: '搜索命令' });
    expect(input).toHaveFocus();
    expect(screen.getByRole('listbox', { name: '可用命令' })).toBeInTheDocument();

    fireEvent.keyDown(input, { key: 'ArrowDown' });
    const settingsOption = screen.getByRole('option', { name: /打开设置/ });
    expect(settingsOption).toHaveAttribute('aria-selected', 'true');
    expect(input).toHaveAttribute('aria-activedescendant', settingsOption.id);
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(runSettings).toHaveBeenCalledOnce();
    expect(runNew).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledOnce();
  });

  it('支持按主机和关键词过滤、跳过禁用项以及 Escape 关闭', () => {
    const onClose = vi.fn();
    const runBuild = vi.fn();
    renderPalette({
      onClose,
      commands: [
        { ...commands[0], disabled: true },
        { ...commands[2], run: runBuild },
      ],
    });
    const input = screen.getByRole('combobox');
    expect(screen.getByRole('option', { name: /切换到 Build/ })).toHaveAttribute('aria-selected', 'true');
    fireEvent.change(input, { target: { value: 'build.example' } });
    expect(screen.getAllByRole('option')).toHaveLength(1);
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(runBuild).toHaveBeenCalledOnce();

    fireEvent.keyDown(input, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it('命令失败时把后端错误交给统一错误通道', async () => {
    const onActionError = vi.fn();
    renderPalette({
      onActionError,
      commands: [{ id: 'fail', label: '失败命令', run: () => Promise.reject(new Error('backend denied')) }],
    });
    fireEvent.keyDown(screen.getByRole('combobox'), { key: 'Enter' });
    await waitFor(() => expect(onActionError).toHaveBeenCalledOnce());
    expect(onActionError.mock.calls[0][0].message).toBe('backend denied');
  });
});

describe('MainMenu', () => {
  it('提供 menu/menuitem 角色、焦点、方向键和 Escape 还焦', () => {
    const trigger = document.createElement('button');
    document.body.appendChild(trigger);
    trigger.focus();
    render(<MainMenu commands={commands} onActionError={vi.fn()} />);
    const button = screen.getByRole('button', { name: '应用菜单' });
    fireEvent.click(button);
    const menu = screen.getByRole('menu', { name: '应用操作' });
    const items = screen.getAllByRole('menuitem');
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(menu, { key: 'ArrowDown' });
    expect(items[1]).toHaveFocus();
    fireEvent.keyDown(menu, { key: 'End' });
    expect(items[2]).toHaveFocus();
    fireEvent.keyDown(menu, { key: 'Home' });
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(menu, { key: 'Escape' });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
    expect(button).toHaveFocus();
    trigger.remove();
  });
});
