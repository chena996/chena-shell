const MODE_LABELS = {
  write: '可写接入',
  read: '只读观察',
  diag: '只读诊断',
};

const RULE_LABELS = {
  'attach requires human approval': 'AI 接入需人工批准',
  'command requires human approval': '命令执行需人工批准',
  'sftp overwrite requires human approval': 'SFTP 覆盖需人工批准',
  'diag allowlist': '只读诊断白名单',
  'strict policy': '严格安全策略',
  blocklist: '高危命令黑名单',
};

const REASON_LABELS = {
  'ai finished': '正常结束',
  'client disconnected': '客户端断开',
  'user force detach': '用户强制断开',
  expired: '已到期',
  timeout: '已超时',
};

export function modeLabel(mode) {
  return MODE_LABELS[mode] || mode || '未知模式';
}

// 与后端 approval::reason_is_encoding_suspect 保持同一套规则：
// 问号累计不少于 4 个、占比超过一半，或以问号开头后接空格/结尾，都视为编码异常。
export function isGarbledReason(value) {
  const text = String(value || '').trim();
  if (!text) return false;
  const chars = [...text];
  const total = chars.length;
  const questionMarks = chars.filter(char => char === '?').length;
  const startsWithQuestion = chars[0] === '?' && (total === 1 || /\s/.test(chars[1]));
  return questionMarks >= 4 || questionMarks / total > 0.5 || startsWithQuestion;
}

export function localizeRule(rule) {
  const text = String(rule || '').trim();
  return RULE_LABELS[text] || text;
}

export function localizeReason(reason) {
  const text = String(reason || '').trim();
  if (!text) return '未提供可读原因';
  if (isGarbledReason(text)) return '未提供可读原因（客户端编码异常）';
  const mapped = REASON_LABELS[text.toLowerCase()] || text;
  return mapped.replace(/^\[(diag|write|read)\]\s*/, (_, mode) => `${modeLabel(mode)} · `);
}

export function formatConnectionId(id) {
  if (!id) return '—';
  const [base, channel] = String(id).split(':');
  if (channel === 'primary') return '主终端';
  if (/^ch\d+$/.test(channel || '')) return `复制终端 ${channel.slice(2)}`;
  return base ? `${base}（未命名终端）` : String(id);
}

export function localizeCommand(command) {
  const text = String(command || '').trim();
  if (!text) return '';
  const attach = text.match(/^\[attach(?::(diag|write|read))?\s*(?:→\s*(.+))?\]$/);
  if (attach) {
    return attach[1]
      ? `接入：${modeLabel(attach[1])} → ${formatConnectionId(attach[2])}`
      : 'AI 接入';
  }
  return text;
}
