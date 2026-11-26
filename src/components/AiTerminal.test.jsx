import { act, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import AiTerminal from './AiTerminal.jsx';

const mocks = vi.hoisted(() => ({
  terms: [],
  fits: [],
  observers: [],
  frames: new Map(),
  nextFrameId: 1,
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock('xterm', () => ({
  Terminal: class {
    constructor(options) {
      this.options = { ...options };
      this.open = vi.fn();
      this.write = vi.fn();
      this.dispose = vi.fn();
      this.loadAddon = vi.fn();
      this.getSelection = vi.fn(() => '');
      this.attachCustomKeyEventHandler = vi.fn();
      mocks.terms.push(this);
    }
  },
}));

vi.mock('xterm-addon-fit', () => ({
  FitAddon: class {
    constructor() {
      this.fit = vi.fn();
      mocks.fits.push(this);
    }
  },
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen }));

function flushAnimationFrames() {
  const callbacks = [...mocks.frames.values()];
  mocks.frames.clear();
  callbacks.forEach(callback => callback());
}

function renderAiTerminal(props = {}) {
  return render(
    <AiTerminal
      sessionId="session-1"
      sessionName="server"
      theme="dark"
      fontSize={14}
      active
      {...props}
    />,
  );
}

beforeEach(() => {
  mocks.terms.length = 0;
  mocks.fits.length = 0;
  mocks.observers.length = 0;
  mocks.frames.clear();
  mocks.nextFrameId = 1;
  mocks.invoke.mockReset().mockResolvedValue({ chunks: [] });
  mocks.listen.mockReset().mockResolvedValue(vi.fn());
  vi.stubGlobal('ResizeObserver', class {
    constructor(callback) {
      this.callback = callback;
      this.observe = vi.fn();
      this.disconnect = vi.fn();
      mocks.observers.push(this);
    }
  });
  vi.stubGlobal('requestAnimationFrame', callback => {
    const id = mocks.nextFrameId++;
    mocks.frames.set(id, callback);
    return id;
  });
  vi.stubGlobal('cancelAnimationFrame', id => {
    mocks.frames.delete(id);
  });
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: { writeText: vi.fn().mockResolvedValue(undefined) },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('AiTerminal 尺寸适配', () => {
  it('连续尺寸变化按动画帧执行 fit，不会拖到停下后才重排', () => {
    renderAiTerminal();
    const fit = mocks.fits[0].fit;
    const observer = mocks.observers[0];

    act(flushAnimationFrames);
    expect(fit).toHaveBeenCalledTimes(1);

    act(() => {
      observer.callback();
      observer.callback();
    });
    expect(mocks.frames.size).toBe(1);
    act(flushAnimationFrames);
    expect(fit).toHaveBeenCalledTimes(2);

    act(() => observer.callback());
    act(flushAnimationFrames);
    expect(fit).toHaveBeenCalledTimes(3);
  });

  it('隐藏标签重新显示时主动重算，设置字号后复用终端并重排', () => {
    const { rerender } = renderAiTerminal({ active: false });
    const fit = mocks.fits[0].fit;
    const terminal = mocks.terms[0];
    act(flushAnimationFrames);
    fit.mockClear();

    rerender(
      <AiTerminal
        sessionId="session-1"
        sessionName="server"
        theme="dark"
        fontSize={18}
        active
      />,
    );

    expect(terminal.options.fontSize).toBe(18);
    expect(mocks.terms).toHaveLength(1);
    expect(mocks.frames.size).toBe(1);
    act(flushAnimationFrames);
    expect(fit).toHaveBeenCalledTimes(1);
  });
});

describe('AI 只读标签复制', () => {
  it('选中文本后 Ctrl+C 复制到剪贴板', async () => {
    renderAiTerminal();
    const terminal = mocks.terms[0];
    const handler = terminal.attachCustomKeyEventHandler.mock.calls[0][0];
    terminal.getSelection.mockReturnValue('ai output');

    const event = {
      type: 'keydown',
      code: 'KeyC',
      key: 'c',
      ctrlKey: true,
      shiftKey: false,
      altKey: false,
      metaKey: false,
      preventDefault: vi.fn(),
    };
    let result;
    await act(async () => { result = handler(event); });

    expect(result).toBe(false);
    expect(event.preventDefault).toHaveBeenCalled();
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith('ai output');
  });

  it('没有选区时不拦截，保留默认行为', () => {
    renderAiTerminal();
    const terminal = mocks.terms[0];
    const handler = terminal.attachCustomKeyEventHandler.mock.calls[0][0];
    const event = {
      type: 'keydown',
      code: 'KeyC',
      key: 'c',
      ctrlKey: true,
      shiftKey: false,
      altKey: false,
      metaKey: false,
      preventDefault: vi.fn(),
    };

    expect(handler(event)).toBe(true);
    expect(event.preventDefault).not.toHaveBeenCalled();
    expect(navigator.clipboard.writeText).not.toHaveBeenCalled();
  });
});
