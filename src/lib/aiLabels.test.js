import { describe, expect, it } from 'vitest';
import {
  formatConnectionId,
  isGarbledReason,
  localizeCommand,
  localizeReason,
  localizeRule,
  modeLabel,
} from './aiLabels.js';

describe('aiLabels', () => {
  it('把模式值翻译成中文', () => {
    expect(modeLabel('diag')).toBe('只读诊断');
    expect(modeLabel('write')).toBe('可写接入');
    expect(modeLabel('read')).toBe('只读观察');
  });

  it('识别并替换问号乱码原因', () => {
    expect(isGarbledReason('????????????')).toBe(true);
    expect(isGarbledReason('????????? inode')).toBe(true);
    expect(isGarbledReason('???? 检查 nginx')).toBe(true);
    expect(isGarbledReason('检查 nginx 状态?')).toBe(false);
    expect(isGarbledReason('read NIC stats')).toBe(false);
    expect(localizeReason('????????????')).toBe('未提供可读原因（客户端编码异常）');
    expect(localizeReason('????????? inode')).toBe('未提供可读原因（客户端编码异常）');
    expect(localizeReason('???? 检查 nginx')).toBe('未提供可读原因（客户端编码异常）');
    expect(localizeReason('检查 nginx 状态?')).toBe('检查 nginx 状态?');
    expect(localizeReason('read NIC stats')).toBe('read NIC stats');
    expect(localizeReason('读取网卡流量')).toBe('读取网卡流量');
    expect(localizeReason('ai finished')).toBe('正常结束');
    expect(localizeReason('   ')).toBe('未提供可读原因');
  });

  it('翻译接入方式和连接标识', () => {
    expect(localizeCommand('[attach:diag → 1:primary]')).toBe('接入：只读诊断 → 主终端');
    expect(localizeCommand('[attach:write → 1:ch2]')).toBe('接入：可写接入 → 复制终端 2');
    expect(localizeCommand('[attach]')).toBe('AI 接入');
    expect(formatConnectionId('1:primary')).toBe('主终端');
    expect(formatConnectionId('1:ch3')).toBe('复制终端 3');
  });

  it('翻译规则名', () => {
    expect(localizeRule('diag allowlist')).toBe('只读诊断白名单');
    expect(localizeRule('attach requires human approval')).toBe('AI 接入需人工批准');
  });
});
