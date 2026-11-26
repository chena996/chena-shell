// 前端诊断：Python 使用 tree-sitter 语法检查，Shell 使用 pureshellcheck（MIT）经 Pyodide（MPL-2.0）在 Web Worker 中运行。
// 两者都按需加载：未打开对应语言文件时不会下载或实例化 wasm，也不会启动 Python 解释器。

const PYTHON_MARKER_LIMIT = 200;

let pythonParserPromise = null;
let shellWorkerPromise = null;
let shellWorkerFailed = false;
let shellDiagNotified = false;
let lintSeq = 0;

async function loadPythonParser() {
  const [treeSitter, runtimeWasm, pythonWasm] = await Promise.all([
    import('web-tree-sitter'),
    import('web-tree-sitter/tree-sitter.wasm?url'),
    import('tree-sitter-wasms/out/tree-sitter-python.wasm?url'),
  ]);
  // 0.20 的导出形态随打包器不同会多包一层 default；真正可用的是可构造的 Parser 类。
  const describe = [];
  let Parser = null;
  let candidate = treeSitter;
  for (let depth = 0; depth < 4 && candidate; depth += 1) {
    describe.push({ type: typeof candidate, keys: Object.keys(candidate).slice(0, 6) });
    if (typeof candidate === 'function' && typeof candidate.init === 'function') { Parser = candidate; break; }
    if (candidate.Parser && typeof candidate.Parser === 'function' && typeof candidate.Parser.init === 'function') {
      Parser = candidate.Parser;
      break;
    }
    candidate = candidate.default;
  }
  if (!Parser) {
    throw new Error(`tree-sitter 导出异常: ${JSON.stringify(describe)}`);
  }
  await Parser.init({ locateFile: () => runtimeWasm.default });
  const Language = Parser.Language || (treeSitter.default && treeSitter.default.Language) || treeSitter.Language;
  if (!Language) {
    throw new Error(`tree-sitter Language 未初始化: ${JSON.stringify({
      parser: Object.getOwnPropertyNames(Parser).slice(0, 12),
      ns: Object.keys(treeSitter),
      def: treeSitter.default ? Object.getOwnPropertyNames(treeSitter.default).slice(0, 12) : null,
    })}`);
  }
  const language = await Language.load(pythonWasm.default);
  const parser = new Parser();
  parser.setLanguage(language);
  return parser;
}

export function getPythonParser() {
  if (!pythonParserPromise) {
    pythonParserPromise = loadPythonParser().catch(error => {
      pythonParserPromise = null;
      throw error;
    });
  }
  return pythonParserPromise;
}

function collectErrorNodes(node, out) {
  if (!node || out.length >= PYTHON_MARKER_LIMIT) return;
  if (node.type === 'ERROR' || node.isMissing) {
    const start = node.startPosition;
    const end = node.endPosition;
    const lastLine = end.row > start.row ? end : { row: start.row, column: start.column + 1 };
    out.push({
      severity: 8,
      message: node.isMissing ? `缺少 ${node.type}` : 'Python 语法错误',
      startLineNumber: start.row + 1,
      startColumn: start.column + 1,
      endLineNumber: end.row + 1,
      endColumn: Math.max(lastLine.column + 1, start.column + 2),
      source: 'tree-sitter',
    });
    // 一个 ERROR 节点内部往往还有多层错误节点，记录一次即可，避免重复标记。
    if (node.type === 'ERROR') return;
  }
  for (let i = 0; i < node.childCount && out.length < PYTHON_MARKER_LIMIT; i += 1) {
    collectErrorNodes(node.child(i), out);
  }
}

export async function collectPythonMarkers(text) {
  const parser = await getPythonParser();
  const tree = parser.parse(String(text || ''));
  try {
    const markers = [];
    collectErrorNodes(tree.rootNode, markers);
    return markers;
  } finally {
    tree.delete();
  }
}

// ---- Shell 诊断（pureshellcheck via Pyodide Worker）----

const SHELLCHECK_SEVERITY = {
  error: 8,
  warning: 4,
  info: 2,
  style: 1,
};

/**
 * 把 pureshellcheck 的 finding 列表映射为 Monaco marker。
 * 纯函数，便于单元测试；不依赖 Worker 或 Pyodide。
 * @param {Array<{line:number,column:number,end_line?:number,end_column?:number,code:number,severity:string,message:string}>} findings
 * @returns {Array} Monaco marker 数组
 */
export function shellFindingsToMarkers(findings) {
  const list = Array.isArray(findings) ? findings : [];
  return list.map(item => {
    const startLine = Math.max(1, Number(item.line) || 1);
    const startColumn = Math.max(1, Number(item.column) || 1);
    const endLine = Math.max(startLine, Number(item.end_line) || startLine);
    const endColumn = Math.max(startColumn + 1, Number(item.end_column) || startColumn + 1);
    return {
      severity: SHELLCHECK_SEVERITY[item.severity] ?? 4,
      message: `[SC${item.code}] ${item.message}`,
      startLineNumber: startLine,
      startColumn,
      endLineNumber: endLine,
      endColumn,
      source: 'shellcheck',
    };
  });
}

function getShellWorker() {
  if (shellWorkerFailed) {
    return Promise.reject(new Error('shell 诊断运行时初始化失败，已禁用'));
  }
  if (typeof Worker === 'undefined') {
    shellWorkerFailed = true;
    return Promise.reject(new Error('当前环境不支持 Web Worker'));
  }
  if (shellWorkerPromise) return shellWorkerPromise;
  shellWorkerPromise = (async () => {
    const worker = new Worker(new URL('./shellcheckWorker.js', import.meta.url), { type: 'module' });
    // 用主线程 location 推导嵌入资源的绝对基础 URL，兼容 Tauri 自定义协议与 Vite dev server。
    const indexURL = new URL('pyodide/', window.location.href).href;
    await new Promise((resolve, reject) => {
      const onMessage = (event) => {
        const data = event.data;
        if (!data) return;
        if (data.type === 'ready') { cleanup(); resolve(); }
        else if (data.type === 'error') { cleanup(); reject(new Error(data.message || 'pyodide 初始化失败')); }
      };
      const onError = (err) => { cleanup(); reject(new Error((err && err.message) || 'shellcheck worker 错误')); };
      const cleanup = () => {
        worker.removeEventListener('message', onMessage);
        worker.removeEventListener('error', onError);
      };
      worker.addEventListener('message', onMessage);
      worker.addEventListener('error', onError);
      worker.postMessage({ type: 'init', indexURL });
    });
    return worker;
  })().catch((error) => {
    shellWorkerFailed = true;
    shellWorkerPromise = null;
    throw error;
  });
  return shellWorkerPromise;
}

export async function collectShellMarkers(text) {
  try {
    const worker = await getShellWorker();
    const id = ++lintSeq;
    const findings = await new Promise((resolve, reject) => {
      const onMessage = (event) => {
        const data = event.data;
        if (!data || data.id !== id) return;
        worker.removeEventListener('message', onMessage);
        if (data.type === 'result') resolve(Array.isArray(data.findings) ? data.findings : []);
        else reject(new Error(data.message || 'shell 诊断失败'));
      };
      worker.addEventListener('message', onMessage);
      worker.postMessage({ type: 'lint', id, text: String(text || ''), shell: 'bash' });
    });
    return shellFindingsToMarkers(findings);
  } catch (error) {
    // 首次失败向上抛出，让编辑器显示一次"诊断不可用"；后续静默返回空，避免反复打扰。
    if (!shellDiagNotified) {
      shellDiagNotified = true;
      throw error;
    }
    return [];
  }
}
