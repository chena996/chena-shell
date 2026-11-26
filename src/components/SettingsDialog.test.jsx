import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import SettingsDialog from './SettingsDialog.jsx';

const settings = {
  default_policy: 'Recommended',
  global_blocklist: [String.raw`(?i)rm\s+-rf\s+/`, String.raw`(?i)mkfs|fdisk\s`],
  confirm_list: [String.raw`(?i)systemctl\s+restart`],
  mcp_enabled: true,
  mcp_host: '127.0.0.1',
  mcp_port: 8767,
  mcp_autostart: true,
  attach_token_ttl_minutes: 30,
  mcp_require_attach_approval: true,
  idle_detach_minutes: 10,
  attach_token_visible: true,
  theme: 'light',
  log_ai_commands: true,
  log_retention_days: 7,
  future_backend_field: { must_be_preserved: true },
};

const uiPrefs = { theme: 'dark', terminal_font_size: 16 };
const mcpStatus = {
  enabled: true,
  listening: true,
  starting: false,
  host: '127.0.0.1',
  port: 8767,
  endpoint: 'http://127.0.0.1:8767/mcp',
  credential_configured: true,
};

function renderDialog(overrides = {}) {
  return render(<SettingsDialog
    open
    settings={settings}
    uiPrefs={uiPrefs}
    mcpStatus={mcpStatus}
    onCancel={vi.fn()}
    onSave={vi.fn().mockResolvedValue(undefined)}
    onGenerateCredential={vi.fn().mockResolvedValue('new-credential')}
    {...overrides}
  />);
}

describe('SettingsDialog 分类与完整草稿', () => {
  it('提供六个已确认分类并显示对应面板', async () => {
    const user = userEvent.setup();
    renderDialog();

    for (const category of ['通用', '终端', '连接', 'AI安全', 'MCP服务', '日志存储']) {
      await user.click(screen.getByRole('button', { name: category, exact: true }));
      expect(screen.getByRole('main', { name: `${category}设置` })).toBeInTheDocument();
    }
    expect(screen.getByRole('dialog')).toHaveAttribute('aria-modal', 'true');
  });

  it('open=false 时不渲染对话框', () => {
    renderDialog({ open: false });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('跨分类保留草稿，并提交所有既有字段、UI 偏好和扩展字段', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({ initialCategory: 'mcp', onSave });

    const port = screen.getByLabelText('MCP 端口');
    await user.clear(port);
    await user.type(port, '9000');
    await user.click(screen.getByRole('button', { name: '日志存储', exact: true }));
    const retention = screen.getByLabelText('日志保留天数');
    await user.clear(retention);
    await user.type(retention, '30');

    await user.click(screen.getByRole('button', { name: 'MCP服务', exact: true }));
    expect(screen.getByLabelText('MCP 端口')).toHaveValue(9000);
    await user.click(screen.getByRole('button', { name: '保存设置' }));

    await waitFor(() => expect(onSave).toHaveBeenCalledOnce());
    const payload = onSave.mock.calls[0][0];
    expect(payload).toMatchObject({
      ...settings,
      mcp_port: 9000,
      log_retention_days: 30,
      theme: 'dark',
      terminal_font_size: 16,
      default_keepalive_secs: 30,
      connect_timeout_secs: 30,
      copy_on_select: false,
      startup_auto_connect: true,
      terminal_log_default_enabled: true,
    });
    expect(payload.global_blocklist).toEqual(settings.global_blocklist);
    expect(payload.confirm_list).toEqual(settings.confirm_list);
    expect(payload.future_backend_field).toEqual({ must_be_preserved: true });
  });

  it('采用 onSave 返回的设置、MCP 状态和通知并清除脏状态', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue({
      settings: { ...settings, mcp_port: 9999 },
      mcpStatus: { ...mcpStatus, port: 9999, endpoint: 'http://127.0.0.1:9999/mcp' },
      notice: '设置已应用',
    });
    renderDialog({ initialCategory: 'mcp', onSave });

    await user.click(screen.getByRole('button', { name: '保存设置' }));
    expect(await screen.findByText('设置已应用')).toBeInTheDocument();
    expect(screen.getByText('http://127.0.0.1:9999/mcp', {
      selector: '.settings-mcp-status > span',
    })).toBeInTheDocument();
    expect(screen.getByLabelText('MCP 端口')).toHaveValue(9999);
    expect(screen.queryByText('有未保存的修改')).not.toBeInTheDocument();
  });
});

describe('SettingsDialog 输入校验', () => {
  it.each([
    ['mcp', 'MCP 端口', '0', /MCP 端口必须是 1–65535/],
    ['logs', '日志保留天数', '366', /日志保留天数必须是 1–365/],
    ['ai', '接入令牌有效期', '0', /接入令牌有效期必须是 1–1440/],
    ['ai', 'AI 空闲断开时间', '1441', /AI 空闲断开时间必须是 1–1440/],
    ['connection', 'Keepalive 间隔', '-1', /Keepalive 间隔必须是 0–3600/],
    ['connection', '连接超时', '301', /连接超时必须是 1–300/],
  ])('阻止 %s 分类的越界数值', async (initialCategory, label, value, message) => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    renderDialog({ initialCategory, onSave });

    const input = screen.getByLabelText(label);
    await user.clear(input);
    await user.type(input, value);
    await user.click(screen.getByRole('button', { name: '保存设置' }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent(message);
    expect(input).toHaveAttribute('aria-invalid', 'true');
  });

  it('逐行校验正则，同时接受后端默认的 (?i) 标记', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({ initialCategory: 'ai', onSave });
    const blacklist = screen.getByLabelText('全局黑名单（每行一个正则）');

    fireEvent.change(blacklist, { target: { value: '(?i)rm\\s+-rf\\s+/\n[invalid' } });
    await user.click(screen.getByRole('button', { name: '保存设置' }));
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByText('全局黑名单第 2 行不是有效正则表达式')).toBeInTheDocument();

    fireEvent.change(blacklist, { target: { value: '(?i)rm\\s+-rf\\s+/' } });
    await user.click(screen.getByRole('button', { name: '保存设置' }));
    await waitFor(() => expect(onSave).toHaveBeenCalledOnce());
    expect(onSave.mock.calls[0][0].global_blocklist).toEqual(['(?i)rm\\s+-rf\\s+/']);
  });

  it('其他分类存在非法草稿时也会阻止保存并跳转到对应分类', async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    renderDialog({ initialCategory: 'mcp', onSave });

    const port = screen.getByLabelText('MCP 端口');
    await user.clear(port);
    await user.type(port, '70000');
    await user.click(screen.getByRole('button', { name: '通用', exact: true }));
    await user.click(screen.getByRole('button', { name: '保存设置' }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole('main', { name: 'MCP服务设置' })).toBeInTheDocument();
    expect(screen.getByText(/MCP 端口必须是/)).toBeInTheDocument();
  });
});

describe('SettingsDialog 取消与键盘可访问性', () => {
  it('干净状态直接取消，脏数据必须二次确认且可继续编辑', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
    renderDialog({ onCancel });

    await user.click(screen.getByRole('button', { name: '取消', exact: true }));
    expect(onCancel).toHaveBeenCalledOnce();
    expect(confirm).not.toHaveBeenCalled();

    await user.selectOptions(screen.getByLabelText('界面主题'), 'light');
    await user.click(screen.getByRole('button', { name: '取消', exact: true }));
    expect(confirm).toHaveBeenCalledOnce();
    expect(onCancel).toHaveBeenCalledOnce();
    expect(screen.getByRole('dialog')).toBeInTheDocument();

    confirm.mockReturnValue(true);
    await user.click(screen.getByRole('button', { name: '关闭设置' }));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  it('Esc 对脏数据执行同样的放弃确认', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const confirm = vi.spyOn(window, 'confirm').mockReturnValueOnce(false).mockReturnValueOnce(true);
    renderDialog({ initialCategory: 'terminal', onCancel });

    const fontSize = screen.getByLabelText('终端字号');
    await user.clear(fontSize);
    await user.type(fontSize, '18');
    await user.keyboard('{Escape}');
    expect(confirm).toHaveBeenCalledOnce();
    expect(onCancel).not.toHaveBeenCalled();

    await user.keyboard('{Escape}');
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('方向键切换分类，Tab 和 Shift+Tab 在对话框内循环', async () => {
    const user = userEvent.setup();
    renderDialog();
    const general = screen.getByRole('button', { name: '通用', exact: true });
    const terminal = screen.getByRole('button', { name: '终端', exact: true });

    expect(general).toHaveFocus();
    await user.keyboard('{ArrowDown}');
    expect(terminal).toHaveFocus();
    expect(screen.getByRole('main', { name: '终端设置' })).toBeInTheDocument();
    await user.keyboard('{ArrowUp}');
    expect(general).toHaveFocus();
    await user.keyboard('{End}');
    expect(screen.getByRole('button', { name: '日志存储', exact: true })).toHaveFocus();
    await user.keyboard('{Home}');
    expect(general).toHaveFocus();

    const save = screen.getByRole('button', { name: '保存设置' });
    const close = screen.getByRole('button', { name: '关闭设置' });
    save.focus();
    await user.tab();
    expect(close).toHaveFocus();
    await user.tab({ shift: true });
    expect(save).toHaveFocus();
  });
});

describe('SettingsDialog MCP 凭证与操作', () => {
  it('重新生成前确认，完整凭证只显示一次且可主动隐藏', async () => {
    const user = userEvent.setup();
    const onGenerateCredential = vi.fn().mockResolvedValue('mcp-full-secret-value');
    renderDialog({ initialCategory: 'mcp', onGenerateCredential });

    await user.click(screen.getByRole('button', { name: '重新生成凭证' }));
    const dialog = await screen.findByRole('alertdialog');
    expect(onGenerateCredential).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole('button', { name: '确定' }));
    expect(onGenerateCredential).toHaveBeenCalledOnce();
    expect(await screen.findByDisplayValue('mcp-full-secret-value')).toHaveAttribute('readonly');

    await user.click(screen.getByRole('button', { name: '我已保存，隐藏凭证' }));
    expect(screen.queryByDisplayValue('mcp-full-secret-value')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '通用', exact: true }));
    await user.click(screen.getByRole('button', { name: 'MCP服务', exact: true }));
    expect(screen.queryByDisplayValue('mcp-full-secret-value')).not.toBeInTheDocument();
  });

  it('拒绝确认时不会调用凭证生成', async () => {
    const user = userEvent.setup();
    const onGenerateCredential = vi.fn();
    renderDialog({ initialCategory: 'mcp', onGenerateCredential });

    await user.click(screen.getByRole('button', { name: '重新生成凭证' }));
    const dialog = await screen.findByRole('alertdialog');
    await user.click(within(dialog).getByRole('button', { name: '取消' }));
    expect(onGenerateCredential).not.toHaveBeenCalled();
    expect(screen.queryByLabelText('新凭证（仅此一次完整显示）')).not.toBeInTheDocument();
    expect(screen.getByText('已取消，未生成新凭证')).toBeInTheDocument();
  });

  it('可选的 MCP 重启测试展示后端结果', async () => {
    const user = userEvent.setup();
    const onTestMcpRestart = vi.fn().mockResolvedValue({
      mcpStatus: { ...mcpStatus, listening: false },
      notice: 'MCP 重启测试完成',
    });
    renderDialog({ initialCategory: 'mcp', onTestMcpRestart });

    await user.click(screen.getByRole('button', { name: '测试 MCP 重启' }));
    expect(await screen.findByText('MCP 重启测试完成')).toBeInTheDocument();
    expect(screen.getByText('未监听')).toBeInTheDocument();
  });
});
