import { beforeEach, describe, expect, it, vi } from 'vitest';

describe('shellFindingsToMarkers（纯函数映射）', () => {
  let shellFindingsToMarkers;

  beforeEach(async () => {
    vi.resetModules();
    ({ shellFindingsToMarkers } = await import('./diagnostics.js'));
  });

  it('把 finding 映射为带 SC 前缀的 Monaco marker', () => {
    const markers = shellFindingsToMarkers([
      { line: 3, column: 5, end_line: 3, end_column: 12, code: 2086, severity: 'info', message: 'Double quote to prevent globbing' },
    ]);
    expect(markers).toHaveLength(1);
    expect(markers[0]).toMatchObject({
      severity: 2,
      message: '[SC2086] Double quote to prevent globbing',
      startLineNumber: 3,
      startColumn: 5,
      endLineNumber: 3,
      endColumn: 12,
      source: 'shellcheck',
    });
  });

  it('severity 映射正确，未知级别降级为 warning', () => {
    const markers = shellFindingsToMarkers([
      { line: 1, column: 1, code: 1, severity: 'error', message: 'a' },
      { line: 1, column: 1, code: 2, severity: 'warning', message: 'b' },
      { line: 1, column: 1, code: 3, severity: 'info', message: 'c' },
      { line: 1, column: 1, code: 4, severity: 'style', message: 'd' },
      { line: 1, column: 1, code: 5, severity: 'unknown', message: 'e' },
    ]);
    expect(markers.map(m => m.severity)).toEqual([8, 4, 2, 1, 4]);
  });

  it('缺失 end_line / end_column 时兜底到起始位置', () => {
    const [marker] = shellFindingsToMarkers([
      { line: 2, column: 4, code: 999, severity: 'warning', message: 'x' },
    ]);
    expect(marker.endLineNumber).toBe(2);
    expect(marker.endColumn).toBe(5);
  });

  it('line / column 最小钳制为 1', () => {
    const [marker] = shellFindingsToMarkers([
      { line: 0, column: 0, end_line: 0, end_column: 0, code: 1, severity: 'error', message: 'x' },
    ]);
    expect(marker.startLineNumber).toBe(1);
    expect(marker.startColumn).toBe(1);
    expect(marker.endColumn).toBe(2);
  });

  it('空数组、null、undefined 均返回空数组', () => {
    expect(shellFindingsToMarkers([])).toEqual([]);
    expect(shellFindingsToMarkers(null)).toEqual([]);
    expect(shellFindingsToMarkers(undefined)).toEqual([]);
  });
});

describe('collectShellMarkers（Worker 交互）', () => {
  let collectShellMarkers;
  let workerInstances;
  let workerFactory;

  function makeWorker({ ready = true, findings = [], lintError = null } = {}) {
    const messageListeners = new Set();
    return {
      addEventListener: vi.fn((type, fn) => { if (type === 'message') messageListeners.add(fn); }),
      removeEventListener: vi.fn(),
      postMessage: vi.fn((msg) => {
        if (msg.type === 'init') {
          setTimeout(() => {
            const data = ready ? { type: 'ready' } : { type: 'error', message: 'init boom' };
            messageListeners.forEach(fn => fn({ data }));
          }, 0);
        } else if (msg.type === 'lint') {
          setTimeout(() => {
            const data = lintError
              ? { type: 'error', id: msg.id, message: lintError }
              : { type: 'result', id: msg.id, findings };
            messageListeners.forEach(fn => fn({ data }));
          }, 0);
        }
      }),
    };
  }

  // 必须是可被 `new` 调用的 class（箭头函数没有 [[Construct]]）。
  class MockWorker {
    constructor() {
      workerInstances.push(this);
      Object.assign(this, workerFactory());
    }
  }

  beforeEach(async () => {
    vi.resetModules();
    workerInstances = [];
    workerFactory = () => makeWorker();
    vi.stubGlobal('Worker', MockWorker);
    ({ collectShellMarkers } = await import('./diagnostics.js'));
  });

  it('Worker 就绪后返回映射后的 markers', async () => {
    workerFactory = () => makeWorker({
      findings: [{ line: 1, column: 1, code: 2086, severity: 'info', message: 'm' }],
    });
    const markers = await collectShellMarkers('echo $foo');
    expect(markers).toHaveLength(1);
    expect(markers[0].message).toBe('[SC2086] m');
    expect(workerInstances).toHaveLength(1);
  });

  it('初始化失败时首次向上抛出错误', async () => {
    workerFactory = () => makeWorker({ ready: false });
    await expect(collectShellMarkers('x')).rejects.toThrow('init boom');
  });

  it('初始化失败后再次调用静默返回空数组，不再打扰', async () => {
    workerFactory = () => makeWorker({ ready: false });
    await expect(collectShellMarkers('x')).rejects.toThrow();
    const markers = await collectShellMarkers('x');
    expect(markers).toEqual([]);
    expect(workerInstances).toHaveLength(1);
  });
});
