// 内置编辑器使用的语言映射与纯文本工具函数；保持无副作用，便于单元测试。

export const LANGUAGE_LABELS = Object.freeze({
  markdown: 'Markdown',
  shell: 'Shell',
  python: 'Python',
  yaml: 'YAML',
  json: 'JSON',
  plaintext: '纯文本',
});

const EXTENSION_LANGUAGES = Object.freeze({
  md: 'markdown',
  markdown: 'markdown',
  mdx: 'markdown',
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  ksh: 'shell',
  py: 'python',
  pyw: 'python',
  pyi: 'python',
  yaml: 'yaml',
  yml: 'yaml',
  json: 'json',
  jsonc: 'json',
});

export function editorLanguageFor(remotePath) {
  const name = String(remotePath || '').split('/').pop() || '';
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return 'plaintext';
  const ext = name.slice(dot + 1).toLowerCase();
  return EXTENSION_LANGUAGES[ext] || 'plaintext';
}

export function languageLabel(language) {
  return LANGUAGE_LABELS[language] || LANGUAGE_LABELS.plaintext;
}

/** 检测文本的主要换行符，保存时按原样还原。 */
export function detectEol(text) {
  return String(text || '').includes('\r\n') ? '\r\n' : '\n';
}

export function formatBytes(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '0 B';
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${(value / 1024 / 1024).toFixed(2)} MiB`;
}

export function basenameOf(remotePath) {
  const name = String(remotePath || '').replace(/\/+$/, '').split('/').pop();
  return name || '未命名';
}

/** 判断是否为内置编辑器支持的文本语言（用于决定是否启用诊断）。 */
export function supportsDiagnostics(language) {
  return language === 'python' || language === 'shell' || language === 'json' || language === 'yaml';
}
