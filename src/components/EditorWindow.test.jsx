import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import EditorWindow from './EditorWindow.jsx';
import { collectShellMarkers, collectPythonMarkers } from '../editor/diagnostics.js';

const winMocks = vi.hoisted(() => ({ onCloseRequested: vi.fn(), close: vi.fn() }));

const monacoMocks = vi.hoisted(() => {
  const state = { model: null, editor: null, setModelMarkers: null, handlers: {} };
  function reset() {
    state.handlers = {};
    state.setModelMarkers = vi.fn();
    state.model = {
      getValue: vi.fn(() => 'echo $foo\n'),
      getVersionId: vi.fn(() => 1),
      isDisposed: vi.fn(() => false),
      setEOL: vi.fn(),
      onDidChangeContent: vi.fn(cb => { state.handlers.modelContent = cb; return { dispose: vi.fn() }; }),
      dispose: vi.fn(),
    };
    state.editor = {
      getValue: vi.fn(() => 'echo $foo\n'),
      setValue: vi.fn(),
      addCommand: vi.fn(),
      onDidChangeModelContent: vi.fn(cb => { state.handlers.content = cb; return { dispose: vi.fn() }; }),
      onDidChangeCursorPosition: vi.fn(cb => { state.handlers.cursor = cb; return { dispose: vi.fn() }; }),
      focus: vi.fn(),
      dispose: vi.fn(),
      updateOptions: vi.fn(),
      getAction: vi.fn(() => ({ run: vi.fn() })),
    };
  }
  reset();
  return { state, reset };
});

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ onCloseRequested: winMocks.onCloseRequested, close: winMocks.close }),
}));
vi.mock('../editor/monacoCore.js', () => ({
  configureEditor: () => ({
    editor: {
      createModel: () => monacoMocks.state.model,
      create: () => monacoMocks.state.editor,
      setModelMarkers: (...args) => monacoMocks.state.setModelMarkers(...args),
      EndOfLineSequence: { CRLF: 1 },
    },
    KeyMod: { CtrlCmd: 2048 },
    KeyCode: { KeyS: 49 },
  }),
}));
vi.mock('../editor/diagnostics.js', () => ({
  collectPythonMarkers: vi.fn().mockResolvedValue([]),
  collectShellMarkers: vi.fn().mockResolvedValue([]),
}));

const defaultRead = { remote_path: '/home/user/test.sh', size: 10, mtime: 100, content: 'echo $foo\n' };

function setupInvoke(overrides = {}) {
  invoke.mockReset();
  invoke.mockImplementation(async (command, args) => {
    if (overrides[command]) return overrides[command](args);
    switch (command) {
      case 'get_editor_prefs': return { mode: 'builtin', shellcheck_enabled: true, remote_schema_enabled: false };
      case 'get_ui_prefs': return { theme: 'dark' };
      case 'sftp_edit_read': return { ...defaultRead };
      case 'sftp_edit_save': return { size: 12, mtime: 200 };
      case 'sftp_edit_cancel': return null;
      default: throw new Error(`unexpected command: ${command}`);
    }
  });
}

let closeRequestedHandler;
beforeEach(() => {
  monacoMocks.reset();
  setupInvoke();
  collectShellMarkers.mockReset().mockResolvedValue([]);
  collectPythonMarkers.mockReset().mockResolvedValue([]);
  closeRequestedHandler = null;
  winMocks.onCloseRequested.mockReset().mockImplementation(cb => {
    closeRequestedHandler = cb;
    return Promise.resolve(vi.fn());
  });
  winMocks.close.mockReset().mockResolvedValue(undefined);
});

async function renderEditor(props = {}) {
  render(<EditorWindow sessionId="sess-1" editId="edit-1" {...props} />);
  await waitFor(() => expect(screen.queryByText('正在加载编辑器…')).toBeNull(), { timeout: 2000 });
}

function editContent() {
  monacoMocks.state.editor.getValue.mockReturnValue('echo $foo bar\n');
  monacoMocks.state.model.getValue.mockReturnValue('echo $foo bar\n');
  act(() => { monacoMocks.state.handlers.content(); });
}

describe('EditorWindow 初始化', () => {
  it('加载远端内容并显示标题与状态栏语言', async () => {
    await renderEditor();
    expect(screen.getByText('test.sh — 内置编辑器')).toBeInTheDocument();
    expect(screen.getByText('Shell')).toBeInTheDocument();
    expect(invoke).toHaveBeenCalledWith('sftp_edit_read', { sessionId: 'sess-1', editId: 'edit-1' });
  });

  it('读取失败时显示致命错误', async () => {
    setupInvoke({ sftp_edit_read: () => { throw new Error('读取失败 boom'); } });
    render(<EditorWindow sessionId="sess-1" editId="edit-1" />);
    expect(await screen.findByText(/读取失败 boom/)).toBeInTheDocument();
  });
});

describe('EditorWindow 编辑与保存', () => {
  it('内容变更后置脏并显示未保存标记', async () => {
    await renderEditor();
    expect(screen.queryByText('● 未保存')).toBeNull();
    editContent();
    expect(screen.getByText('● 未保存')).toBeInTheDocument();
  });

  it('保存成功后清脏、提示已保存，并关闭窗口（仅一次上传）', async () => {
    const user = userEvent.setup();
    await renderEditor();
    editContent();
    await user.click(screen.getByRole('button', { name: '保存' }));
    expect(invoke).toHaveBeenCalledWith('sftp_edit_save', expect.objectContaining({
      sessionId: 'sess-1', editId: 'edit-1', confirmOverwrite: false,
    }));
    expect(await screen.findByText(/已保存到远端/)).toBeInTheDocument();
    expect(screen.queryByText('● 未保存')).toBeNull();
    // 保存成功后自动关闭：cancel 仅做本地清理（不上传），随后关窗。
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('sftp_edit_cancel', { sessionId: 'sess-1', editId: 'edit-1' }));
    expect(winMocks.close).toHaveBeenCalled();
    // 只有 sftp_edit_save 会写远端，cancel 不触发第二次上传。
    const saveCalls = invoke.mock.calls.filter(([cmd]) => cmd === 'sftp_edit_save');
    expect(saveCalls).toHaveLength(1);
  });

  it('保存冲突时显示冲突条与覆盖按钮，且不关闭窗口', async () => {
    const user = userEvent.setup();
    setupInvoke({ sftp_edit_save: () => { throw 'CONFLICT:远端已变更'; } });
    await renderEditor();
    editContent();
    await user.click(screen.getByRole('button', { name: '保存' }));
    expect(await screen.findByText('覆盖远端')).toBeInTheDocument();
    expect(screen.getByText(/远端已变更/)).toBeInTheDocument();
    // 保存失败（冲突）→ saveThenClose 不调用 finishClose，窗口保持打开。
    expect(winMocks.close).not.toHaveBeenCalled();
  });
});

describe('EditorWindow 关闭确认', () => {
  it('脏状态下请求关闭弹出未保存确认，放弃修改后关闭窗口', async () => {
    await renderEditor();
    editContent();
    await act(async () => { await closeRequestedHandler({ preventDefault: vi.fn() }); });
    expect(screen.getByText('有未保存的修改')).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: '放弃修改' }));
    expect(invoke).toHaveBeenCalledWith('sftp_edit_cancel', { sessionId: 'sess-1', editId: 'edit-1' });
    expect(winMocks.close).toHaveBeenCalled();
  });

  it('干净状态下请求关闭直接关闭，不弹确认', async () => {
    await renderEditor();
    await act(async () => { await closeRequestedHandler({ preventDefault: vi.fn() }); });
    expect(screen.queryByText('有未保存的修改')).toBeNull();
    expect(winMocks.close).toHaveBeenCalled();
  });
});

describe('EditorWindow 诊断调度', () => {
  it('shell 文件加载后调度 shell 诊断并更新标记数', async () => {
    collectShellMarkers.mockResolvedValue([
      { severity: 2, message: '[SC2086] m', startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 5 },
    ]);
    await renderEditor();
    await waitFor(() => expect(collectShellMarkers).toHaveBeenCalled(), { timeout: 2000 });
    expect(await screen.findByText('诊断 1')).toBeInTheDocument();
  });

  it('关闭 shellcheck 开关时不调用 shell 诊断', async () => {
    setupInvoke({ get_editor_prefs: () => ({ shellcheck_enabled: false }) });
    await renderEditor();
    await new Promise(resolve => setTimeout(resolve, 900));
    expect(collectShellMarkers).not.toHaveBeenCalled();
  });
});
