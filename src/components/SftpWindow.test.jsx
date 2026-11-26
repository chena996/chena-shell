import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import SftpWindow, { transferPercent, transferStatusLabel } from './SftpWindow.jsx';

const eventMocks = vi.hoisted(() => ({ callbacks: new Map() }));
const windowMocks = vi.hoisted(() => ({ onCloseRequested: vi.fn(), close: vi.fn() }));

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn() }));
vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ onCloseRequested: windowMocks.onCloseRequested, close: windowMocks.close }),
}));

const session = {
  config: { id: 'sess-1', name: '测试机', host: '10.0.0.8', username: 'root' },
  status: { connected: true },
};

function mockCommands(overrides = {}) {
  invoke.mockImplementation(async (command, args) => {
    if (overrides[command]) return overrides[command](args);
    switch (command) {
      case 'list_sessions': return [session];
      case 'sftp_pwd': return '/home/test';
      case 'get_home_dir': return 'C:\\Users\\Test\\Desktop';
      case 'read_local_dir': return [];
      case 'sftp_list': return [{ name: 'report.txt', is_dir: false, size: 12, mtime: 100, perms: '644' }];
      case 'sftp_delete': return null;
      case 'list_transfers': return [];
      case 'get_ui_prefs': return { theme: 'dark' };
      case 'get_editor_prefs': return { mode: 'external', shellcheck_enabled: true, remote_schema_enabled: false };
      default: return null;
    }
  });
}

async function emitEvent(name, payload) {
  const callback = eventMocks.callbacks.get(name);
  expect(callback, `${name} 监听器已注册`).toBeTypeOf('function');
  await act(async () => { callback({ payload }); });
}

beforeEach(() => {
  invoke.mockReset();
  mockCommands();
  eventMocks.callbacks.clear();
  listen.mockReset().mockImplementation((name, callback) => {
    eventMocks.callbacks.set(name, callback);
    return Promise.resolve(vi.fn());
  });
  windowMocks.onCloseRequested.mockReset().mockResolvedValue(vi.fn());
  windowMocks.close.mockReset().mockResolvedValue(undefined);
});

describe('SFTP 删除确认', () => {
  async function openDeleteDialog(user) {
    render(<SftpWindow sessionId="sess-1" />);
    await user.click(await screen.findByText('report.txt'));
    await user.click(screen.getByTitle('删除远端: report.txt (Delete)'));
    return screen.getByRole('dialog', { name: '确认删除' });
  }

  it('确认前不调用删除，确认后删除完整远端路径', async () => {
    const user = userEvent.setup();
    await openDeleteDialog(user);

    expect(invoke).not.toHaveBeenCalledWith('sftp_delete', expect.anything());
    await user.click(screen.getByRole('button', { name: '删除', exact: true }));

    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_delete', {
      sessionId: 'sess-1', remotePath: '/home/test/report.txt',
    }));
    await waitFor(() => expect(screen.queryByRole('dialog', { name: '确认删除' })).not.toBeInTheDocument());
  });

  it('删除过程中显示进度并禁止重复提交', async () => {
    let finishDelete;
    mockCommands({ sftp_delete: () => new Promise(resolve => { finishDelete = resolve; }) });
    const user = userEvent.setup();
    await openDeleteDialog(user);

    await user.click(screen.getByRole('button', { name: '删除', exact: true }));
    expect(await screen.findByRole('button', { name: '删除中...' })).toBeDisabled();
    expect(screen.getByRole('button', { name: '取消', exact: true })).toBeDisabled();

    await act(async () => { finishDelete(null); });
    await waitFor(() => expect(screen.queryByRole('dialog', { name: '确认删除' })).not.toBeInTheDocument());
  });

  it('删除失败后保留完整错误并允许重试', async () => {
    mockCommands({ sftp_delete: async () => { throw '初始化 SFTP 失败: 连接被服务器重置'; } });
    const user = userEvent.setup();
    const dialog = await openDeleteDialog(user);

    await user.click(screen.getByRole('button', { name: '删除', exact: true }));
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('删除失败: 初始化 SFTP 失败: 连接被服务器重置');
    expect(screen.getByRole('dialog', { name: '确认删除' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '删除', exact: true })).toBeEnabled();
  });

  it('删除远端文件后仍可正常关闭窗口', async () => {
    const user = userEvent.setup();
    await openDeleteDialog(user);
    await user.click(screen.getByRole('button', { name: '删除', exact: true }));
    await waitFor(() => expect(screen.queryByRole('dialog', { name: '确认删除' })).not.toBeInTheDocument());

    await user.click(screen.getByRole('button', { name: '返回主窗口' }));

    await waitFor(() => expect(invoke).toHaveBeenCalledWith('close_sftp_window', { sessionId: 'sess-1' }));
    expect(windowMocks.close).toHaveBeenCalledTimes(1);
  });
});

describe('SFTP 传输状态', () => {
  it('使用真实字节计算进度与百分比', () => {
    const job = { size: 200, bytes_transferred: 50, status: 'running' };
    expect(transferPercent(job)).toBe(25);
    expect(transferStatusLabel(job)).toBe('25%');
  });

  it('区分完成、失败、暂停和未知大小状态', () => {
    expect(transferPercent({ size: 0, status: 'done' })).toBe(100);
    expect(transferStatusLabel({ size: 0, status: 'done' })).toBe('✓');
    expect(transferStatusLabel({ size: 0, status: 'error' })).toBe('✕');
    expect(transferPercent({ size: 0, status: 'paused' })).toBe(50);
    expect(transferStatusLabel({ size: 0, status: 'paused' })).toBe('暂停');
  });

  it('初始快照后按事件 upsert，合并缺失字段且不接受最终状态回退', async () => {
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');
    expect(invoke.mock.calls.filter(([name]) => name === 'list_transfers')).toHaveLength(1);

    await emitEvent('transfer-progress', {
      id: 'job-1', session_id: 'sess-1', direction: 'upload',
      local_path: 'C:\\data\\a.txt', remote_path: '/tmp/a.txt',
      size: 100, bytes_transferred: 25, status: 'running', error: null,
    });
    expect(await screen.findByText('25%')).toBeInTheDocument();

    await emitEvent('transfer-progress', { id: 'job-1', bytes_transferred: 75 });
    expect(await screen.findByText('75%')).toBeInTheDocument();
    expect(screen.getByText('C:\\data\\a.txt → /tmp/a.txt')).toBeInTheDocument();

    await emitEvent('transfer-progress', {
      id: 'job-2', session_id: 'other-session', status: 'running', local_path: 'x', remote_path: 'y',
    });
    expect(screen.queryByText('x → y')).not.toBeInTheDocument();

    await emitEvent('transfer-progress', { id: 'job-1', status: 'done', bytes_transferred: 100 });
    expect(await screen.findByText('✓')).toBeInTheDocument();
    await emitEvent('transfer-progress', { id: 'job-1', status: 'running', bytes_transferred: 30 });
    expect(screen.getByText('✓')).toBeInTheDocument();
    expect(screen.queryByTitle('暂停传输')).not.toBeInTheDocument();
  });
});

describe('SFTP OSC7 与主题事件', () => {
  it('只导航匹配会话的 path/cwd.path，同路径不重复加载', async () => {
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');

    await emitEvent('sftp-osc7', { session_id: 'other', path: '/etc' });
    expect(invoke).not.toHaveBeenCalledWith('sftp_list', { sessionId: 'sess-1', path: '/etc' });

    await emitEvent('sftp-osc7', { session_id: 'sess-1', cwd: { path: '/srv/中文目录' } });
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_list', {
      sessionId: 'sess-1', path: '/srv/中文目录',
    }));
    expect(screen.getByLabelText('远端路径')).toHaveValue('/srv/中文目录');
    const loadCount = invoke.mock.calls.filter(([name, args]) => name === 'sftp_list' && args.path === '/srv/中文目录').length;

    await emitEvent('sftp-osc7', { session_id: 'sess-1', path: '/srv/中文目录' });
    expect(invoke.mock.calls.filter(([name, args]) => name === 'sftp_list' && args.path === '/srv/中文目录')).toHaveLength(loadCount);
  });

  it('响应主窗口写入的 storage 主题事件', async () => {
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');
    fireEvent(window, new StorageEvent('storage', { key: 'chena-shell-theme', newValue: 'light' }));
    expect(document.documentElement.dataset.theme).toBe('light');
  });
});

describe('SFTP 流式上传与面板拖拽', () => {
  it('直接传本地路径，不再读取 base64 内容', async () => {
    mockCommands({ read_local_dir: async () => [['notes.txt', false, 10]] });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    fireEvent.contextMenu(await screen.findByText('notes.txt'));
    await user.click(screen.getByText('上传到远端'));

    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_upload', {
      sessionId: 'sess-1',
      remotePath: '/home/test/notes.txt',
      localPath: 'C:\\Users\\Test\\Desktop\\notes.txt',
    }));
    expect(invoke).not.toHaveBeenCalledWith('read_local_file_base64', expect.anything());
  });

  it('本地文件拖到远侧面板触发上传', async () => {
    mockCommands({ read_local_dir: async () => [['拖拽.txt', false, 10]] });
    render(<SftpWindow sessionId="sess-1" />);
    const row = await screen.findByTestId('local-row-拖拽.txt');
    const values = new Map();
    const dataTransfer = {
      setData: (key, value) => values.set(key, value),
      getData: key => values.get(key) || '',
      effectAllowed: '', dropEffect: '',
    };

    fireEvent.dragStart(row, { dataTransfer });
    fireEvent.drop(screen.getByTestId('remote-panel'), { dataTransfer });

    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_upload', {
      sessionId: 'sess-1',
      remotePath: '/home/test/拖拽.txt',
      localPath: 'C:\\Users\\Test\\Desktop\\拖拽.txt',
    }));
  });

  it('远端文件拖到本地面板触发下载', async () => {
    render(<SftpWindow sessionId="sess-1" />);
    const row = await screen.findByTestId('remote-row-report.txt');
    const values = new Map();
    const dataTransfer = {
      setData: (key, value) => values.set(key, value),
      getData: key => values.get(key) || '',
      effectAllowed: '', dropEffect: '',
    };

    fireEvent.dragStart(row, { dataTransfer });
    fireEvent.drop(screen.getByTestId('local-panel'), { dataTransfer });

    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_download_to_local', {
      sessionId: 'sess-1',
      remotePath: '/home/test/report.txt',
      localPath: 'C:\\Users\\Test\\Desktop\\report.txt',
    }));
  });
});

describe('SFTP 导航、排序与隐藏文件', () => {
  it('支持目录前进后退和面包屑导航', async () => {
    mockCommands({
      sftp_list: async args => args.path === '/home/test'
        ? [{ name: 'docs', is_dir: true, size: 0, mtime: 100, perms: '755' }]
        : [],
    });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    await user.dblClick(await screen.findByText('docs'));
    await waitFor(() => expect(screen.getByLabelText('远端路径')).toHaveValue('/home/test/docs'));
    await user.click(screen.getByRole('button', { name: '远端后退' }));
    await waitFor(() => expect(screen.getByLabelText('远端路径')).toHaveValue('/home/test'));
    await user.click(screen.getByRole('button', { name: '远端前进' }));
    await waitFor(() => expect(screen.getByLabelText('远端路径')).toHaveValue('/home/test/docs'));

    const breadcrumb = screen.getByLabelText('远端路径面包屑');
    await user.click(within(breadcrumb).getByRole('button', { name: 'test', exact: true }));
    await waitFor(() => expect(screen.getByLabelText('远端路径')).toHaveValue('/home/test'));
  });

  it('过滤隐藏文件，并按大小和修改时间真实排序', async () => {
    mockCommands({
      read_local_dir: async () => [['b.txt', false, 2], ['.secret', false, 1], ['a.txt', false, 10]],
      sftp_list: async () => [
        { name: 'new.txt', is_dir: false, size: 1, mtime: 200, perms: '644' },
        { name: 'old.txt', is_dir: false, size: 1, mtime: 100, perms: '644' },
      ],
    });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByTestId('local-row-a.txt');

    expect(screen.queryByTestId('local-row-.secret')).not.toBeInTheDocument();
    await user.click(screen.getByLabelText('显示隐藏文件'));
    expect(screen.getByTestId('local-row-.secret')).toBeInTheDocument();

    await user.click(screen.getByLabelText('本地按大小排序'));
    expect(screen.getAllByTestId(/^local-row-/).map(row => row.getAttribute('data-testid'))).toEqual([
      'local-row-.secret', 'local-row-b.txt', 'local-row-a.txt',
    ]);

    await user.click(screen.getByLabelText('远端按时间排序'));
    expect(screen.getAllByTestId(/^remote-row-/).map(row => row.getAttribute('data-testid'))).toEqual([
      'remote-row-old.txt', 'remote-row-new.txt',
    ]);
  });
});

describe('SFTP 传输控制与关闭确认', () => {
  it.each([
    ['running', '暂停传输', 'pause_transfer'],
    ['paused', '恢复传输', 'resume_transfer'],
    ['error', '重试传输', 'retry_transfer'],
  ])('%s 任务提供正确操作', async (status, title, command) => {
    const job = {
      id: 'job-1', session_id: 'sess-1', direction: 'upload',
      local_path: 'C:\\data\\a.txt', remote_path: '/tmp/a.txt',
      size: 100, bytes_transferred: 10, status, error: status === 'error' ? '连接中断' : null,
    };
    mockCommands({ list_transfers: async () => [job] });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    await user.click(await screen.findByTitle(title));
    expect(invoke).toHaveBeenCalledWith(command, { jobId: 'job-1' });
  });

  it('活动任务存在时，返回主窗口前必须确认', async () => {
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');
    await emitEvent('transfer-progress', {
      id: 'active', session_id: 'sess-1', direction: 'download',
      local_path: 'C:\\a', remote_path: '/a', size: 10, bytes_transferred: 1, status: 'running',
    });

    await user.click(screen.getByRole('button', { name: '返回主窗口' }));
    expect(screen.getByRole('dialog', { name: '确认关闭窗口' })).toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith('close_sftp_window', expect.anything());
    await user.click(screen.getByRole('button', { name: '继续查看' }));
    expect(screen.queryByRole('dialog', { name: '确认关闭窗口' })).not.toBeInTheDocument();
  });
});

describe('SFTP 远端文本编辑（外部编辑器）', () => {
  const beginResult = {
    edit_id: 'edit-1',
    remote_path: '/home/test/report.txt',
    local_path: 'C:\\Users\\Test\\AppData\\Roaming\\chena-shell\\edit\\edit-1\\report.txt',
    backup_path: 'C:\\Users\\Test\\AppData\\Roaming\\chena-shell\\edit\\edit-1\\report.txt.original',
    size: 12,
    mtime: 100,
  };

  async function openEdit(user) {
    const row = await screen.findByText('report.txt');
    fireEvent.contextMenu(row);
    return user.click(screen.getByRole('menuitem', { name: '编辑', exact: true }));
  }

  it('内置编辑模式下打开独立编辑器窗口，不调用外部编辑器流程', async () => {
    mockCommands({
      get_editor_prefs: async () => ({ mode: 'builtin', shellcheck_enabled: true, remote_schema_enabled: false }),
      sftp_edit_open: async () => ({ edit_id: 'edit-9', remote_path: '/home/test/report.txt', size: 12, mtime: 100, content: 'hello' }),
      open_edit_window: async () => null,
    });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    await openEdit(user);

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('sftp_edit_open', {
        sessionId: 'sess-1', remotePath: '/home/test/report.txt',
      });
    });
    expect(invoke).toHaveBeenCalledWith('open_edit_window', {
      sessionId: 'sess-1', editId: 'edit-9', title: 'report.txt — 内置编辑器',
    });
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_begin', expect.anything());
  });

  it('目录的编辑入口禁用，文件编辑后不在内嵌编辑器打开而是提示本地路径与备份', async () => {
    mockCommands({
      sftp_list: async () => [
        { name: 'report.txt', is_dir: false, size: 12, mtime: 100, perms: '644' },
        { name: 'docs', is_dir: true, size: 0, mtime: 100, perms: '755' },
      ],
      sftp_edit_begin: async () => beginResult,
    });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    fireEvent.contextMenu(await screen.findByText('docs'));
    const directoryEdit = screen.getByRole('menuitem', { name: '编辑', exact: true });
    expect(directoryEdit).toHaveAttribute('aria-disabled', 'true');
    await user.click(directoryEdit);
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_begin', expect.anything());

    await openEdit(user);

    expect(invoke).toHaveBeenCalledWith('sftp_edit_begin', {
      sessionId: 'sess-1', remotePath: '/home/test/report.txt',
    });
    expect(screen.queryByRole('dialog', { name: '编辑远端文件' })).not.toBeInTheDocument();
    expect(screen.queryByLabelText('文件内容')).not.toBeInTheDocument();

    const notice = await screen.findByTestId('edit-notice-opened');
    expect(notice).toHaveTextContent('report.txt');
    expect(notice).toHaveTextContent(beginResult.local_path);
    expect(notice).toHaveTextContent(beginResult.backup_path);
  });

  it('编辑打开失败时显示完整错误，且不调用任何保存类命令', async () => {
    mockCommands({ sftp_edit_begin: async () => { throw '远端文件超过 5 MiB，不能编辑'; } });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);

    await openEdit(user);

    const notice = await screen.findByTestId('edit-notice-error');
    expect(notice).toHaveTextContent('远端文件超过 5 MiB，不能编辑');
    expect(screen.queryByLabelText('文件内容')).not.toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_save', expect.anything());
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_confirm_overwrite', expect.anything());
  });

  it('编辑器关闭后回传成功，监听 uploaded 事件并刷新远端列表', async () => {
    mockCommands({ sftp_edit_begin: async () => beginResult });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');
    const listBefore = invoke.mock.calls.filter(c => c[0] === 'sftp_list').length;

    await openEdit(user);
    await screen.findByTestId('edit-notice-opened');

    await emitEvent('sftp-edit-sync', {
      edit_id: 'edit-1', session_id: 'sess-1', status: 'uploaded',
      message: '已回传修改: /home/test/report.txt',
    });

    expect(await screen.findByTestId('edit-notice-uploaded')).toHaveTextContent('已回传修改');
    await waitFor(() => expect(invoke.mock.calls.filter(c => c[0] === 'sftp_list').length).toBe(listBefore + 1));
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_cancel', expect.anything());
  });

  it('未修改关闭收到 unchanged，不回传也不取消', async () => {
    mockCommands({ sftp_edit_begin: async () => beginResult });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await openEdit(user);
    await screen.findByTestId('edit-notice-opened');

    await emitEvent('sftp-edit-sync', { edit_id: 'edit-1', session_id: 'sess-1', status: 'unchanged', message: '未检测到修改' });

    expect(await screen.findByTestId('edit-notice-unchanged')).toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_confirm_overwrite', expect.anything());
    expect(invoke).not.toHaveBeenCalledWith('sftp_edit_cancel', expect.anything());
  });

  it('远端冲突事件提供确认覆盖与取消回传，分别调用对应命令', async () => {
    mockCommands({ sftp_edit_begin: async () => beginResult });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await openEdit(user);
    await screen.findByTestId('edit-notice-opened');

    await emitEvent('sftp-edit-sync', {
      edit_id: 'edit-1', session_id: 'sess-1', status: 'conflict',
      message: 'SFTP_EDIT_CONFLICT: 远端文件已变化（期望 size=12，当前 size=20）',
    });

    const notice = await screen.findByTestId('edit-notice-conflict');
    expect(notice).toHaveTextContent('远端文件已变化');

    await user.click(screen.getByRole('button', { name: '确认覆盖', exact: true }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_edit_confirm_overwrite', {
      sessionId: 'sess-1', editId: 'edit-1',
    }));

    await user.click(screen.getByRole('button', { name: '取消回传', exact: true }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_edit_cancel', {
      sessionId: 'sess-1', editId: 'edit-1',
    }));
  });

  it('非本会话的编辑同步事件被忽略', async () => {
    mockCommands({ sftp_edit_begin: async () => beginResult });
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await openEdit(user);
    await screen.findByTestId('edit-notice-opened');

    await emitEvent('sftp-edit-sync', { edit_id: 'edit-1', session_id: 'sess-other', status: 'uploaded', message: 'x' });

    expect(screen.getByTestId('edit-notice-opened')).toBeInTheDocument();
    expect(screen.queryByTestId('edit-notice-uploaded')).not.toBeInTheDocument();
  });
});

describe('SFTP 刷新入口', () => {
  it('工具栏和右键菜单都能刷新当前目录', async () => {
    const user = userEvent.setup();
    render(<SftpWindow sessionId="sess-1" />);
    await screen.findByText('report.txt');

    const remoteCalls = () => invoke.mock.calls.filter(call => call[0] === 'sftp_list').length;
    const localCalls = () => invoke.mock.calls.filter(call => call[0] === 'read_local_dir').length;

    const remoteBefore = remoteCalls();
    await user.click(screen.getByRole('button', { name: '远端刷新' }));
    await waitFor(() => expect(remoteCalls()).toBe(remoteBefore + 1));

    const localBefore = localCalls();
    await user.click(screen.getByRole('button', { name: '本地刷新' }));
    await waitFor(() => expect(localCalls()).toBe(localBefore + 1));

    fireEvent.contextMenu(await screen.findByText('report.txt'));
    await user.click(screen.getByRole('menuitem', { name: '刷新', exact: true }));
    await waitFor(() => expect(remoteCalls()).toBe(remoteBefore + 2));
  });
});
