// Shell 诊断 Worker：在独立线程中运行 Pyodide + pureshellcheck（MIT），
// 避免 CPython 冷启动阻塞编辑器主线程。所有资源均为本地嵌入，运行时不联网。
//
// 消息协议：
//   主线程 -> Worker: { type: 'init', indexURL }            初始化 Pyodide 并安装本地 wheel
//   主线程 -> Worker: { type: 'lint', id, text, shell }      对文本做检查
//   Worker -> 主线程: { type: 'ready' }                      初始化完成
//   Worker -> 主线程: { type: 'result', id, findings }       检查结果
//   Worker -> 主线程: { type: 'error', id?, message }        出错
//
// finding 字段（来自 pureshellcheck）：
//   { line, column, end_line, end_column, code(int), severity('error'|'warning'|'info'|'style'), message }

import { loadPyodide } from 'pyodide';

const WHEEL_NAME = 'pureshellcheck-0.2.2-py3-none-any.whl';

let pyodidePromise = null;
let indexURL = '';

function normalizeIndexURL(url) {
  return String(url || '').replace(/\/?$/, '/');
}

async function ensurePyodide() {
  if (pyodidePromise) return pyodidePromise;
  pyodidePromise = (async () => {
    const base = normalizeIndexURL(indexURL);
    const pyodide = await loadPyodide({ indexURL: base });
    // micropip 是 Pyodide 内置包，由本地 pyodide-lock.json 解析，不联网。
    await pyodide.loadPackage('micropip');
    const micropip = pyodide.pyimport('micropip');
    // 安装本地 wheel（绝对 URL，指向嵌入资源），完全离线。
    await micropip.install(base + WHEEL_NAME);
    // 预热导入，确保后续每次 lint 不再重复 import。
    await pyodide.runPythonAsync('import pureshellcheck');
    return pyodide;
  })();
  return pyodidePromise;
}

const LINT_SCRIPT = String.raw`
import json
import pureshellcheck
_findings = pureshellcheck.check(__chena_script, shell=__chena_shell)
json.dumps([{
    "line": int(f.line),
    "column": int(f.column),
    "end_line": int(getattr(f, "end_line", f.line)),
    "end_column": int(getattr(f, "end_column", f.column + 1)),
    "code": int(f.code),
    "severity": str(f.severity),
    "message": str(f.message),
} for f in _findings])
`;

self.onmessage = async (event) => {
  const msg = event.data || {};
  if (msg.type === 'init') {
    indexURL = msg.indexURL || '';
    try {
      await ensurePyodide();
      self.postMessage({ type: 'ready' });
    } catch (error) {
      pyodidePromise = null;
      self.postMessage({ type: 'error', message: String(error && error.message ? error.message : error) });
    }
    return;
  }
  if (msg.type === 'lint') {
    const id = msg.id;
    try {
      const pyodide = await ensurePyodide();
      pyodide.globals.set('__chena_script', String(msg.text || ''));
      pyodide.globals.set('__chena_shell', String(msg.shell || 'bash'));
      const raw = await pyodide.runPythonAsync(LINT_SCRIPT);
      const findings = JSON.parse(raw);
      self.postMessage({ type: 'result', id, findings });
    } catch (error) {
      self.postMessage({
        type: 'error',
        id,
        message: String(error && error.message ? error.message : error),
      });
    }
  }
};
