import { useEffect, useRef, useState } from 'react';
import { Terminal as XTerm } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import 'xterm/css/xterm.css';
import { getXtermTheme, terminalBg } from '../lib/xtermTheme.js';
import { formatConnectionId, modeLabel } from '../lib/aiLabels.js';

const MIN_FONT_SIZE = 10;
const MAX_FONT_SIZE = 24;

function formatRemaining(expiresAt, now) {
  if (!expiresAt) return null;
  const timestamp = new Date(expiresAt).getTime();
  if (!Number.isFinite(timestamp)) return null;
  const seconds = Math.max(0, Math.ceil((timestamp - now) / 1000));
  if (seconds <= 0) return '已到期';
  const minutes = Math.floor(seconds / 60);
  return minutes > 0 ? `${minutes} 分 ${seconds % 60} 秒` : `${seconds} 秒`;
}

/**
 * AI 只读标签：只展示 AI 执行过程与结果，人类不能输入或执行命令。
 * 数据来自独立的 ai-terminal-data 事件与 read_ai_terminal 回放。
 */
export default function AiTerminal({
  sessionId,
  sessionName,
  connectionId,
  mode,
  expiresAt,
  sudo,
  attached,
  readers = 0,
  task,
  taskExpiresAt,
  theme = 'dark',
  fontSize = 14,
  active = true,
}) {
  const containerRef = useRef(null);
  const terminalRef = useRef(null);
  const fitFrameRef = useRef(null);
  const scheduleFitRef = useRef(null);
  const fontSizeRef = useRef(fontSize);
  const lastSeqRef = useRef(0);
  const [now, setNow] = useState(Date.now());
  const [copyHint, setCopyHint] = useState('');

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return undefined;
    const terminal = new XTerm({
      theme: getXtermTheme(theme),
      fontFamily: "'JetBrains Mono', 'Cascadia Code', Consolas, monospace",
      fontSize: fontSizeRef.current,
      cursorBlink: false,
      disableStdin: true,
      scrollback: 5000,
      convertEol: true,
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(container);
    terminalRef.current = terminal;

    let hintTimer = null;
    let disposed = false;
    const showCopyHint = (text) => {
      if (disposed) return;
      setCopyHint(text);
      if (hintTimer) clearTimeout(hintTimer);
      hintTimer = setTimeout(() => setCopyHint(''), 1500);
    };

    // 标签不接收输入，但允许选中后复制；Ctrl+C 与 Ctrl+Shift+C 都执行复制。
    terminal.attachCustomKeyEventHandler((event) => {
      if (event.type !== 'keydown') return true;
      const ctrl = (event.ctrlKey || event.metaKey) && !event.altKey;
      if (!ctrl || event.code !== 'KeyC') return true;
      const text = terminal.getSelection();
      if (!text) return true;
      event.preventDefault();
      const copy = async () => {
        try {
          await navigator.clipboard.writeText(text);
          showCopyHint('已复制');
        } catch {
          try {
            const area = document.createElement('textarea');
            area.value = text;
            area.style.position = 'fixed';
            area.style.opacity = '0';
            document.body.appendChild(area);
            area.select();
            const ok = document.execCommand('copy');
            document.body.removeChild(area);
            showCopyHint(ok ? '已复制' : '复制失败');
          } catch {
            showCopyHint('复制失败');
          }
        }
      };
      copy();
      return false;
    });

    const writeChunk = (chunk) => {
      if (!chunk || typeof chunk.data !== 'string' || !chunk.data) return;
      const seq = Number(chunk.seq || 0);
      if (seq && seq <= lastSeqRef.current) return;
      if (seq) lastSeqRef.current = seq;
      terminal.write(chunk.data);
    };

    const load = async () => {
      try {
        const replay = await invoke('read_ai_terminal', { sessionId });
        if (disposed) return;
        for (const chunk of replay?.chunks || []) writeChunk(chunk);
      } catch {
        // 会话可能已删除，保持空白即可。
      }
    };
    load();

    const unlisten = listen('ai-terminal-data', event => {
      const payload = event.payload || {};
      if (payload.session_id !== sessionId) return;
      writeChunk({ seq: payload.seq, data: payload.data });
    });

    // 持续拖动时每个动画帧最多 fit 一次；不能用尾部去抖，否则拖动期间始终不重排。
    const scheduleFit = () => {
      if (fitFrameRef.current !== null) return;
      fitFrameRef.current = window.requestAnimationFrame(() => {
        fitFrameRef.current = null;
        try { fit.fit(); } catch { /* 容器不可见时忽略 */ }
      });
    };
    scheduleFitRef.current = scheduleFit;
    scheduleFit();
    const applyFontSize = (next) => {
      const size = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, next));
      if (size === fontSizeRef.current) return;
      fontSizeRef.current = size;
      terminal.options.fontSize = size;
      scheduleFit();
      invoke('save_ui_prefs', { terminalFontSize: size }).catch(() => {});
    };
    const wheelHandler = (event) => {
      if (!event.ctrlKey && !event.metaKey) return;
      event.preventDefault();
      applyFontSize(fontSizeRef.current + (event.deltaY > 0 ? -1 : 1));
    };
    container.addEventListener('wheel', wheelHandler, { passive: false });

    const observer = new ResizeObserver(scheduleFit);
    observer.observe(container);

    return () => {
      disposed = true;
      observer.disconnect();
      if (hintTimer) clearTimeout(hintTimer);
      container.removeEventListener('wheel', wheelHandler);
      if (fitFrameRef.current !== null) window.cancelAnimationFrame(fitFrameRef.current);
      fitFrameRef.current = null;
      scheduleFitRef.current = null;
      unlisten.then(fn => fn()).catch(() => {});
      terminal.dispose();
      terminalRef.current = null;
      lastSeqRef.current = 0;
    };
  }, [sessionId, theme]);

  // 设置页或其它终端改了字号后，同步到当前只读终端，不重建实例。
  useEffect(() => {
    const terminal = terminalRef.current;
    const size = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, fontSize));
    fontSizeRef.current = size;
    if (!terminal || terminal.options.fontSize === size) return;
    terminal.options.fontSize = size;
    scheduleFitRef.current?.();
  }, [fontSize]);

  // display:none 中的标签尺寸不可靠，重新显示后必须在下一帧主动重算。
  useEffect(() => {
    if (active) scheduleFitRef.current?.();
  }, [active]);

  const remaining = formatRemaining(expiresAt, now);
  const observing = !attached && readers > 0;
  const attachLabel = attached
    ? 'AI 附着中'
    : observing
      ? (mode === 'diag' ? 'AI 诊断观察中' : 'AI 只读观察中')
      : 'AI 已断开 · 历史记录';
  const statusParts = [
    attachLabel,
    formatConnectionId(connectionId),
    modeLabel(mode),
  ];
  if (attached && remaining) statusParts.push(`剩余 ${remaining}`);
  if (attached && task) statusParts.push(`任务：${task}`);
  const taskRemaining = attached ? formatRemaining(taskExpiresAt, now) : null;
  if (taskRemaining) statusParts.push(`任务剩余 ${taskRemaining}`);
  if (sudo) statusParts.push('sudo 已授权');

  return (
    <div style={{ flex: 1, minHeight: 0, minWidth: 0, display: 'flex', flexDirection: 'column', overflow: 'hidden', background: terminalBg(theme) }}>
      <div style={{
        display: 'flex', alignItems: 'center', gap: 12, padding: '6px 12px',
        background: theme === 'dark' ? '#252526' : '#F3F3F3',
        borderBottom: '1px solid var(--md-outline-variant)',
        fontFamily: "'JetBrains Mono', monospace", fontSize: 11,
        color: 'var(--md-on-surface-variant)', flexWrap: 'wrap',
      }}>
        <span style={{
          padding: '1px 7px', borderRadius: 4, fontWeight: 700, letterSpacing: .5,
          background: 'var(--md-primary)', color: 'var(--md-on-primary)', fontSize: 10,
        }}>AI 只读</span>
        <span style={{ color: 'var(--md-on-surface)', fontWeight: 600 }}>{sessionName}</span>
        {statusParts.map((part, index) => (
          <span key={index}>{part}</span>
        ))}
        <span style={{ marginLeft: 'auto', opacity: .75 }}>{copyHint || '此标签仅供查看，可选中后按 Ctrl+C 复制，Ctrl+滚轮缩放'}</span>
      </div>
      <div ref={containerRef} style={{ flex: 1, minHeight: 0, minWidth: 0, overflow: 'hidden', padding: '4px 2px 2px 8px' }} />
    </div>
  );
}
