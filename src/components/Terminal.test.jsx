import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import Terminal, { terminalUtils } from './Terminal.jsx';

const mocks = vi.hoisted(() => ({
  terms: [],
  searchAddons: [],
  webLinkAddons: [],
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock('xterm', () => ({
  Terminal: class {
    constructor(options) {
      this.options = { ...options };
      this.cols = 80;
      this.rows = 24;
      this.selection = '';
      this.addons = [];
      this.disposables = [];
      this.buffer = { active: { getLine: vi.fn() } };
      this.open = vi.fn();
      this.writeln = vi.fn();
      this.write = vi.fn();
      this.focus = vi.fn();
      this.paste = vi.fn();
      this.resize = vi.fn();
      this.refresh = vi.fn();
      this.dispose = vi.fn();
      this.loadAddon = vi.fn(addon => { this.addons.push(addon); });
      this.parser = {
        registerOscHandler: vi.fn((id, handler) => {
          this.oscHandler = handler;
          const disposable = { dispose: vi.fn() };
          this.oscDisposable = disposable;
          return disposable;
        }),
      };
      mocks.terms.push(this);
    }

    onData(handler) {
      this.dataHandler = handler;
      this.dataDisposable = { dispose: vi.fn() };
      return this.dataDisposable;
    }

    onBinary(handler) {
      this.binaryHandler = handler;
      this.binaryDisposable = { dispose: vi.fn() };
      return this.binaryDisposable;
    }

    onResize(handler) {
      this.resizeHandler = handler;
      return { dispose: vi.fn() };
    }

    onSelectionChange(handler) {
      this.selectionHandler = handler;
      this.selectionDisposable = { dispose: vi.fn() };
      return this.selectionDisposable;
    }

    attachCustomKeyEventHandler(handler) {
      this.keyHandler = handler;
    }

    getSelection() {
      return this.selection;
    }

    getSelectionPosition() {
      return null;
    }
  },
}));

vi.mock('xterm-addon-fit', () => ({
  FitAddon: class {
    constructor() {
      this.fit = vi.fn();
    }
  },
}));

vi.mock('xterm-addon-search', () => ({
  SearchAddon: class {
    constructor() {
      this.findNext = vi.fn();
      this.findPrevious = vi.fn();
      this.clearDecorations = vi.fn();
      mocks.searchAddons.push(this);
    }
  },
}));

vi.mock('xterm-addon-web-links', () => ({
  WebLinksAddon: class {
    constructor(handler, options) {
      this.handler = handler;
      this.options = options;
      this.dispose = vi.fn();
      mocks.webLinkAddons.push(this);
    }
  },
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen }));

const baseProps = {
  session: null,
  connecting: false,
  onKick: vi.fn(),
  onConnect: vi.fn(),
  theme: 'dark',
};

function renderTerminal(props = {}) {
  return render(<Terminal {...baseProps} {...props} />);
}

async function pressTermKey(term, init) {
  let result;
  await act(async () => {
    result = term.keyHandler({
      type: 'keydown',
      ctrlKey: true,
      altKey: false,
      metaKey: false,
      shiftKey: false,
      key: '',
      preventDefault: vi.fn(),
      ...init,
    });
  });
  return result;
}

beforeEach(() => {
  mocks.terms.length = 0;
  mocks.searchAddons.length = 0;
  mocks.webLinkAddons.length = 0;
  mocks.invoke.mockReset().mockImplementation(command => {
    if (command === 'get_ui_prefs') return Promise.resolve({ terminal_font_size: 14 });
    return Promise.resolve({});
  });
  mocks.listen.mockReset().mockResolvedValue(vi.fn());
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: {
      readText: vi.fn().mockResolvedValue(''),
      writeText: vi.fn().mockResolvedValue(undefined),
      write: vi.fn().mockResolvedValue(undefined),
    },
  });
  vi.stubGlobal('ResizeObserver', class {
    observe() {}
    disconnect() {}
  });
});

describe('Terminal 纯函数', () => {
  it('计算粘贴行数并限制字号范围', () => {
    expect(terminalUtils.countPasteLines('')).toBe(0);
    expect(terminalUtils.countPasteLines('one')).toBe(1);
    expect(terminalUtils.countPasteLines('one\r\ntwo\nthree')).toBe(3);
    expect(terminalUtils.clampFontSize(7)).toBe(8);
    expect(terminalUtils.clampFontSize(33)).toBe(32);
    expect(terminalUtils.clampFontSize('invalid')).toBe(14);
  });

  it('将 xterm 二进制字符串按原始单字节转换', () => {
    expect(terminalUtils.binaryStringToBytes('')).toEqual([]);
    expect(terminalUtils.binaryStringToBytes('\x00\x7f\x80\xffA')).toEqual([0, 127, 128, 255, 65]);
    expect(terminalUtils.binaryStringToBytes('\x1b[M\x20\x20\x20')).toEqual([27, 91, 77, 32, 32, 32]);
  });

  it('只接受 HTTP(S) 链接并安全解析 file OSC7', () => {
    expect(terminalUtils.isHttpUrl('https://example.com/docs')).toBe(true);
    expect(terminalUtils.isHttpUrl('http://localhost:8080')).toBe(true);
    expect(terminalUtils.isHttpUrl('file:///etc/passwd')).toBe(false);
    expect(terminalUtils.isHttpUrl('javascript:alert(1)')).toBe(false);

    expect(terminalUtils.parseOsc7('file://build-host/home/me/My%20Project')).toEqual({
      host: 'build-host',
      path: '/home/me/My Project',
    });
    expect(terminalUtils.parseOsc7('file:///tmp')).toEqual({ host: '', path: '/tmp' });
    expect(terminalUtils.parseOsc7('https://example.com/tmp')).toBeNull();
    expect(terminalUtils.parseOsc7('file:///bad%ZZpath')).toBeNull();
  });
});

describe('Terminal 组件', () => {
  it.each([
    { label: '默认', channelId: null, dataCommand: 'terminal_input', binaryCommand: 'terminal_input_bytes' },
    { label: '复制', channelId: 'ch-copy', dataCommand: 'channel_input', binaryCommand: 'channel_input_bytes' },
  ])('$label渠道原样转发文本鼠标事件和二进制鼠标事件', async ({ channelId, dataCommand, binaryCommand }) => {
    const session = {
      id: 'session-1', name: 'server', username: 'root', host: '127.0.0.1', port: 22, connected: true,
    };
    const { unmount } = renderTerminal({ session, channelId });
    const term = mocks.terms[0];
    const common = channelId ? { sessionId: session.id, channelId } : { sessionId: session.id };

    await act(async () => {
      term.dataHandler('\x1b[<0;120;24M');
      term.binaryHandler('\x1b[M\xff\x80\x21');
    });
    expect(mocks.invoke).toHaveBeenCalledWith(dataCommand, {
      ...common, data: '\x1b[<0;120;24M',
    });
    expect(mocks.invoke).toHaveBeenCalledWith(binaryCommand, {
      ...common, data: [27, 91, 77, 255, 128, 33],
    });

    unmount();
    expect(term.dataDisposable.dispose).toHaveBeenCalledOnce();
    expect(term.binaryDisposable.dispose).toHaveBeenCalledOnce();
  });

  it('普通 shell 当前命令行左键点击复用 xterm Alt+点击定位', () => {
    renderTerminal();
    const term = mocks.terms[0];
    const container = term.open.mock.calls[0][0];
    term.element = container;
    term.buffer.active = {
      type: 'normal', baseY: 0, viewportY: 0, cursorX: 15, cursorY: 2,
      getLine: vi.fn(() => ({ isWrapped: false })),
    };
    term._core = {
      coreMouseService: { activeProtocol: 'NONE' },
      _mouseService: { getCoords: vi.fn(() => [10, 3]) },
    };
    const nativeMouseUp = vi.fn();
    container.addEventListener('mouseup', nativeMouseUp);

    fireEvent.mouseUp(container, { button: 0, clientX: 100, clientY: 30 });

    expect(nativeMouseUp).toHaveBeenCalledOnce();
    expect(nativeMouseUp.mock.calls[0][0].altKey).toBe(true);
  });

  it.each([
    { label: '历史区域', buffer: { viewportY: 1 } },
    { label: '应用鼠标协议', protocol: 'SGR' },
    { label: '已有选区', selection: 'selected' },
    { label: '组合键点击', event: { ctrlKey: true } },
  ])('普通 shell 左键定位不干扰$label', ({ buffer: bufferPatch, protocol, selection, event: eventPatch }) => {
    renderTerminal();
    const term = mocks.terms[0];
    const container = term.open.mock.calls[0][0];
    term.element = container;
    term.selection = selection ?? '';
    term.buffer.active = {
      type: 'normal', baseY: 2, viewportY: 2, cursorX: 15, cursorY: 2,
      getLine: vi.fn(() => ({ isWrapped: false })),
      ...bufferPatch,
    };
    term._core = {
      coreMouseService: { activeProtocol: protocol ?? 'NONE' },
      _mouseService: { getCoords: vi.fn(() => [10, 3]) },
    };
    const nativeMouseUp = vi.fn();
    container.addEventListener('mouseup', nativeMouseUp);

    fireEvent.mouseUp(container, { button: 0, clientX: 100, clientY: 30, ...eventPatch });

    expect(nativeMouseUp).toHaveBeenCalledOnce();
    expect(nativeMouseUp.mock.calls[0][0].altKey).toBe(false);
  });

  it('Ctrl+F 打开搜索，支持前后查找和 Esc 还焦关闭', async () => {
    renderTerminal();
    const term = mocks.terms[0];
    const search = mocks.searchAddons[0];

    expect(await pressTermKey(term, { code: 'KeyF', key: 'f' })).toBe(false);
    const input = screen.getByRole('textbox', { name: '搜索内容' });
    expect(input).toHaveFocus();
    fireEvent.change(input, { target: { value: 'needle' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(search.findNext).toHaveBeenCalledWith('needle');
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });
    expect(search.findPrevious).toHaveBeenCalledWith('needle');

    fireEvent.click(screen.getByRole('button', { name: '下一个搜索结果' }));
    fireEvent.click(screen.getByRole('button', { name: '上一个搜索结果' }));
    expect(search.findNext).toHaveBeenCalledTimes(2);
    expect(search.findPrevious).toHaveBeenCalledTimes(2);

    fireEvent.keyDown(screen.getByRole('button', { name: '下一个搜索结果' }), { key: 'Escape' });
    expect(screen.queryByRole('search', { name: '终端搜索' })).not.toBeInTheDocument();
    expect(search.clearDecorations).toHaveBeenCalledOnce();
    expect(term.focus).toHaveBeenCalled();
  });

  it('支持键盘和 Ctrl 滚轮缩放并可重置字号', async () => {
    renderTerminal();
    const term = mocks.terms[0];
    const container = term.open.mock.calls[0][0];

    await pressTermKey(term, { code: 'Equal', key: '+' });
    expect(term.options.fontSize).toBe(15);
    await pressTermKey(term, { code: 'Minus', key: '-' });
    expect(term.options.fontSize).toBe(14);
    fireEvent.wheel(container, { ctrlKey: true, deltaY: -100 });
    expect(term.options.fontSize).toBe(15);
    await pressTermKey(term, { code: 'Digit0', key: '0' });
    expect(term.options.fontSize).toBe(14);
    expect(mocks.invoke).toHaveBeenCalledWith('save_ui_prefs', { terminalFontSize: 15 });
  });

  it('选中即复制默认关闭，开启后复制当前选区', async () => {
    const props = { ...baseProps };
    const { rerender } = render(<Terminal {...props} />);
    const term = mocks.terms[0];
    term.selection = 'selected text';

    await act(async () => { term.selectionHandler(); });
    expect(navigator.clipboard.writeText).not.toHaveBeenCalled();

    rerender(<Terminal {...props} prefs={{ copy_on_select: true }} />);
    await act(async () => { term.selectionHandler(); });
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith('selected text');
  });

  it('多行粘贴确认只显示行数，取消后不发送内容', async () => {
    navigator.clipboard.readText.mockResolvedValue('echo one\r\necho two\n');
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
    renderTerminal();
    const term = mocks.terms[0];

    await pressTermKey(term, { code: 'KeyV', key: 'v' });
    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0][0]).toContain('3 行');
    expect(confirm.mock.calls[0][0]).not.toContain('echo');
    expect(term.paste).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    await pressTermKey(term, { code: 'KeyV', key: 'v' });
    expect(term.paste).toHaveBeenCalledWith('echo one\r\necho two\n');
  });

  it('链接偏好只重建链接 addon，并限制可打开的协议', () => {
    const open = vi.spyOn(window, 'open').mockReturnValue({ opener: window });
    const props = { ...baseProps, prefs: { links_enabled: true } };
    const { rerender } = render(<Terminal {...props} />);
    const term = mocks.terms[0];
    const links = mocks.webLinkAddons[0];

    expect(links.options.urlRegex.test('https://example.com')).toBe(true);
    expect(links.options.urlRegex.test('ftp://example.com')).toBe(false);
    links.handler({}, 'file:///etc/passwd');
    expect(open).not.toHaveBeenCalled();
    links.handler({}, 'https://example.com');
    expect(open).toHaveBeenCalledWith('https://example.com', '_blank', 'noopener,noreferrer');

    rerender(<Terminal {...props} prefs={{ links_enabled: false }} />);
    expect(links.dispose).toHaveBeenCalledOnce();
    expect(mocks.terms).toHaveLength(1);
    expect(term.dispose).not.toHaveBeenCalled();
  });

  it('OSC7 仅通过 onOsc7 上报解码后的主机和路径', async () => {
    const onOsc7 = vi.fn();
    renderTerminal({ onOsc7 });
    const term = mocks.terms[0];

    await act(async () => {
      expect(term.oscHandler('file://server/home/user/project%20one')).toBe(true);
    });
    expect(onOsc7).toHaveBeenCalledWith({ host: 'server', path: '/home/user/project one' });
    await act(async () => { term.oscHandler('https://example.com/private'); });
    expect(onOsc7).toHaveBeenCalledTimes(1);
  });

  it('卸载时清理 DOM 监听和终端订阅', () => {
    const { unmount } = renderTerminal();
    const term = mocks.terms[0];
    const container = term.open.mock.calls[0][0];
    const removeListener = vi.spyOn(container, 'removeEventListener');

    unmount();
    expect(removeListener).toHaveBeenCalledWith('wheel', expect.any(Function));
    expect(removeListener).toHaveBeenCalledWith('contextmenu', expect.any(Function));
    expect(term.selectionDisposable.dispose).toHaveBeenCalledOnce();
    expect(term.oscDisposable.dispose).toHaveBeenCalledOnce();
    expect(term.dispose).toHaveBeenCalledOnce();
  });
});
