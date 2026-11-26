import { useState, useEffect, useRef, useCallback } from 'react';
import { Terminal as XTerm } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { SearchAddon } from 'xterm-addon-search';
import { WebLinksAddon } from 'xterm-addon-web-links';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import 'xterm/css/xterm.css';
import { getXtermTheme, terminalBg } from '../lib/xtermTheme.js';

const DEFAULT_FONT_SIZE = 14;
const WEB_LINK_REGEX = /https?:\/\/[^\s<>"']+/i;

function clampFontSize(size) {
  const value = Number(size);
  if (!Number.isFinite(value)) return DEFAULT_FONT_SIZE;
  return Math.max(8, Math.min(32, Math.round(value)));
}

function countPasteLines(text) {
  return typeof text === 'string' && text ? text.split(/\r\n|\r|\n/).length : 0;
}

// xterm onBinary 使用单字节字符串；不能用 TextEncoder 将高位字节扩展成 UTF-8。
function binaryStringToBytes(value) {
  return Array.from(value, ch => ch.charCodeAt(0) & 0xff);
}

function isHttpUrl(value) {
  try {
    const url = new URL(value);
    return (url.protocol === 'http:' || url.protocol === 'https:') && !!url.hostname;
  } catch {
    return false;
  }
}

function parseOsc7(payload) {
  if (typeof payload !== 'string' || !payload) return null;
  try {
    const url = new URL(payload);
    if (url.protocol !== 'file:') return null;
    const path = decodeURIComponent(url.pathname) || '/';
    if (!path.startsWith('/')) return null;
    return { host: url.hostname, path };
  } catch {
    return null;
  }
}

// 常量导出便于对不接触终端、剪贴板和磁盘的纯函数做单元测试。
// eslint-disable-next-line react-refresh/only-export-components
export const terminalUtils = Object.freeze({ clampFontSize, countPasteLines, binaryStringToBytes, isHttpUrl, parseOsc7 });

export default function Terminal({ session, connecting, onKick, onConnect, theme, prefs, onOsc7, channelId = null }) {
  const containerRef = useRef(null);
  const termRef = useRef(null);
  const fitAddonRef = useRef(null);
  const searchAddonRef = useRef(null);
  const webLinksAddonRef = useRef(null);
  const searchInputRef = useRef(null);
  const sessionRef = useRef(session);
  const sessionIdRef = useRef(null);
  const channelIdRef = useRef(channelId ?? null);
  channelIdRef.current = channelId ?? null;
  const wasConnected = useRef(false);
  const fontSizeRef = useRef(DEFAULT_FONT_SIZE);
  const themeRef = useRef(theme);
  const prefsRef = useRef(null);
  const onOsc7Ref = useRef(onOsc7);
  const actionsRef = useRef(null);
  const hintTimer = useRef(null);
  const [ctxMenu, setCtxMenu] = useState({ visible: false, x: 0, y: 0, hasSel: false });
  const [hint, setHint] = useState('');
  const [searchVisible, setSearchVisible] = useState(false);
  const [searchQuery, setSearchQuery] = useState('');
  const sessionId = session?.id ?? null;
  const sessionConnected = session?.connected;
  const copyOnSelect = prefs?.copy_on_select === true;
  const confirmMultilinePaste = prefs?.confirm_multiline_paste !== false;
  const linksEnabled = prefs?.links_enabled !== false;

  const showHint = useCallback((msg) => {
    setHint(msg);
    if (hintTimer.current) clearTimeout(hintTimer.current);
    hintTimer.current = setTimeout(() => setHint(''), 1600);
  }, []);

  const closeSearch = useCallback(() => {
    setSearchVisible(false);
    searchAddonRef.current?.clearDecorations();
    termRef.current?.focus();
  }, []);

  useEffect(() => { themeRef.current = theme; }, [theme]);
  useEffect(() => { sessionRef.current = session; }, [session]);
  useEffect(() => { onOsc7Ref.current = onOsc7; }, [onOsc7]);
  useEffect(() => {
    prefsRef.current = {
      copy_on_select: copyOnSelect,
      confirm_multiline_paste: confirmMultilinePaste,
      links_enabled: linksEnabled,
    };
  }, [copyOnSelect, confirmMultilinePaste, linksEnabled]);
  useEffect(() => () => {
    if (hintTimer.current) clearTimeout(hintTimer.current);
  }, []);
  useEffect(() => {
    if (!searchVisible) return;
    searchInputRef.current?.focus();
    searchInputRef.current?.select();
  }, [searchVisible]);

  // 右键菜单：点击别处 / Esc 关闭
  useEffect(() => {
    if (!ctxMenu.visible) return;
    const close = () => setCtxMenu(m => ({ ...m, visible: false }));
    const onKey = (e) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', close);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', close);
      document.removeEventListener('keydown', onKey);
    };
  }, [ctxMenu.visible]);

  // 字号由 App 统一加载；避免异步恢复覆盖用户刚刚执行的缩放。
  useEffect(() => {
    const size = clampFontSize(prefs?.terminal_font_size ?? DEFAULT_FONT_SIZE);
    if (size === fontSizeRef.current) return;
    fontSizeRef.current = size;
    const t = termRef.current;
    if (t) {
      t.options.fontSize = size;
      try { fitAddonRef.current?.fit(); } catch { /* ignore */ }
    }
  }, [prefs?.terminal_font_size]);

  // 创建/销毁 xterm（仅会话切换时重建）
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const currentSession = sessionRef.current;
    const term = new XTerm({
      fontFamily: "JetBrains Mono, Menlo, Consolas, monospace",
      fontSize: fontSizeRef.current,
      lineHeight: 1.5,
      cursorBlink: true,
      cursorStyle: 'bar',
      scrollback: 50000,
      convertEol: true,
      theme: getXtermTheme(themeRef.current),
    });
    const fitAddon = new FitAddon();
    const searchAddon = new SearchAddon();
    term.loadAddon(fitAddon);
    term.loadAddon(searchAddon);
    term.open(container);
    try { fitAddon.fit(); } catch { /* ignore */ }

    // 普通 shell 没有应用鼠标协议；仅在当前命令行内复用 xterm 的 Alt+点击定位。
    // SelectionService 仍会限制短点击并处理换行与应用光标键，拖拽选择和历史区域不受影响。
    const shellClickMouseUp = event => {
      const buffer = term.buffer.active;
      const protocol = term._core?.coreMouseService?.activeProtocol;
      if (event.button !== 0 || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey
        || buffer.type !== 'normal' || (protocol && protocol !== 'NONE')
        || buffer.baseY !== buffer.viewportY || term.getSelection()) return;
      const coords = term._core?._mouseService?.getCoords(event, term.element, term.cols, term.rows, false);
      if (!coords) return;
      const cursorLine = buffer.baseY + buffer.cursorY;
      const targetLine = buffer.viewportY + coords[1] - 1;
      const lineStart = index => {
        while (index > 0 && buffer.getLine(index)?.isWrapped) index--;
        return index;
      };
      if (lineStart(cursorLine) !== lineStart(targetLine)) return;
      Object.defineProperty(event, 'altKey', { configurable: true, value: true });
    };
    container.addEventListener('mouseup', shellClickMouseUp, true);

    termRef.current = term;
    fitAddonRef.current = fitAddon;
    searchAddonRef.current = searchAddon;
    sessionIdRef.current = sessionId;
    wasConnected.current = false;

    term.writeln('\x1b[38;5;111mChena-Shell\x1b[0m — AI 友好的 SSH 终端 · by Chena');
    if (!currentSession) {
      term.writeln('\x1b[90m左侧选择或新建一个会话开始连接。\x1b[0m');
    } else if (!currentSession.connected) {
      term.writeln(`\x1b[90m会话 ${currentSession.name} 尚未连接，点击右上角「连接」。\x1b[0m`);
    }

    // 键盘与鼠标协议输入 → 后端；旧式鼠标协议的高位坐标必须按原始字节发送。
    const sendInput = (data, binary = false) => {
      if (!sessionIdRef.current) return;
      const channel = channelIdRef.current;
      const command = channel
        ? (binary ? 'channel_input_bytes' : 'channel_input')
        : (binary ? 'terminal_input_bytes' : 'terminal_input');
      const payload = {
        sessionId: sessionIdRef.current,
        data: binary ? binaryStringToBytes(data) : data,
      };
      if (channel) payload.channelId = channel;
      invoke(command, payload).catch(() => {});
    };
    const dataSub = term.onData(data => {
      sendInput(data);
    });
    const binarySub = term.onBinary(data => {
      sendInput(data, true);
    });

    // 输出监听：默认渠道用 terminal-data，复制渠道用 session-channel-output
    const paneChannel = channelIdRef.current;
    const unlisten = paneChannel
      ? listen('session-channel-output', ev => {
          const { session_id, channel_id, data } = ev.payload || {};
          if (session_id === sessionIdRef.current && channel_id === paneChannel) {
            term.write(data);
          }
        })
      : listen('terminal-data', ev => {
          const { session_id, data } = ev.payload || {};
          if (session_id === sessionIdRef.current) {
            term.write(data);
          }
        });
    const unlistenClosed = paneChannel
      ? listen('session-channel-closed', ev => {
          const { session_id, channel_id } = ev.payload || {};
          if (session_id === sessionIdRef.current && channel_id === paneChannel) {
            term.writeln('\r\n\x1b[38;5;203m● SSH 渠道已断开\x1b[0m');
          }
        })
      : null;
    if (paneChannel) {
      invoke('attach_session_channel', { sessionId: sessionIdRef.current, channelId: paneChannel })
        .then(pending => { if (pending) term.write(pending); })
        .catch(() => {});
    }

    // 终端尺寸变化 → 后端 PTY
    const resizeSub = term.onResize(({ cols, rows }) => {
      if (!sessionIdRef.current) return;
      const channel = channelIdRef.current;
      if (channel) {
        invoke('channel_resize', { sessionId: sessionIdRef.current, channelId: channel, cols, rows }).catch(() => {});
      } else {
        invoke('terminal_resize', { sessionId: sessionIdRef.current, cols, rows }).catch(() => {});
      }
    });

    // OSC 7 只通过回调上报当前内存中的 file URL 主机和路径，不写日志或配置。
    const osc7Sub = term.parser.registerOscHandler(7, payload => {
      const cwd = parseOsc7(payload);
      if (cwd) onOsc7Ref.current?.(cwd);
      return true;
    });

    // ===== 剪贴板：复制 / 带格式复制 / 粘贴 / 粘贴所选 =====
    const copyPlain = async () => {
      const text = term.getSelection();
      if (!text) { showHint('未选中内容'); return; }
      try {
        await navigator.clipboard.writeText(text);
        showHint('已复制');
      } catch {
        try {
          const ta = document.createElement('textarea');
          ta.value = text;
          ta.style.position = 'fixed';
          ta.style.opacity = '0';
          document.body.appendChild(ta);
          ta.select();
          const ok = document.execCommand('copy');
          document.body.removeChild(ta);
          showHint(ok ? '已复制' : '复制失败');
        } catch { showHint('复制失败'); }
      }
    };

    const selectionSub = term.onSelectionChange(() => {
      if (!prefsRef.current?.copy_on_select) return;
      const text = term.getSelection();
      if (!text) return;
      try {
        navigator.clipboard?.writeText(text)?.catch(() => {});
      } catch { /* 自动复制失败时保留选区，不打扰终端输入 */ }
    });

    /** 把当前选区按终端颜色生成 HTML（带格式复制） */
    const buildSelectionHtml = () => {
      const pos = term.getSelectionPosition();
      if (!pos) return '';
      const buf = term.buffer.active;
      const th = getXtermTheme(themeRef.current);
      const palette = [
        th.black, th.red, th.green, th.yellow, th.blue, th.magenta, th.cyan, th.white,
        th.brightBlack, th.brightRed, th.brightGreen, th.brightYellow,
        th.brightBlue, th.brightMagenta, th.brightCyan, th.brightWhite,
      ];
      const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
      const rows = [];
      for (let y = pos.start.y; y <= pos.end.y; y++) {
        const line = buf.getLine(y);
        if (!line) { rows.push(''); continue; }
        const x0 = y === pos.start.y ? pos.start.x : 0;
        const x1 = y === pos.end.y ? pos.end.x : line.length;
        let html = '';
        let runColor;
        let runBold = false;
        let runText = '';
        const flush = () => {
          if (!runText) return;
          const style = [runColor ? `color:${runColor}` : '', runBold ? 'font-weight:600' : ''].filter(Boolean).join(';');
          html += style ? `<span style="${style}">${esc(runText)}</span>` : esc(runText);
          runText = '';
        };
        for (let x = x0; x < x1; x++) {
          const cell = line.getCell(x);
          if (!cell) continue;
          if (cell.getWidth() === 0) continue; // 宽字符占位格
          let color;
          const mode = cell.getFgColorMode();
          if (mode === 0x2000000) {
            color = '#' + (cell.getFgColor() >>> 0).toString(16).padStart(6, '0');
          } else if (mode === 0x1000000) {
            color = palette[cell.getFgColor()];
          }
          const bold = !!cell.isBold();
          if (color !== runColor || bold !== runBold) { flush(); runColor = color; runBold = bold; }
          runText += cell.getChars() || ' ';
        }
        flush();
        rows.push(html);
      }
      return `<pre style="margin:0;padding:8px;background:${th.background};color:${th.foreground};font-family:'JetBrains Mono',Consolas,monospace;font-size:12px;white-space:pre-wrap;">${rows.join('\n')}</pre>`;
    };

    const copyRich = async () => {
      const text = term.getSelection();
      if (!text) { showHint('未选中内容'); return; }
      const html = buildSelectionHtml();
      try {
        if (navigator.clipboard?.write && window.ClipboardItem) {
          await navigator.clipboard.write([new ClipboardItem({
            'text/html': new Blob([html], { type: 'text/html' }),
            'text/plain': new Blob([text], { type: 'text/plain' }),
          })]);
          showHint('已复制（带格式）');
        } else {
          await navigator.clipboard.writeText(text);
          showHint('已复制（纯文本）');
        }
      } catch {
        try {
          await navigator.clipboard.writeText(text);
          showHint('已复制（纯文本）');
        } catch { showHint('复制失败'); }
      }
    };

    const pasteText = (text) => {
      const lineCount = countPasteLines(text);
      if (lineCount > 1 && prefsRef.current?.confirm_multiline_paste !== false) {
        const confirmed = window.confirm(`确认粘贴 ${lineCount} 行内容？多行粘贴可能执行多条命令。`);
        if (!confirmed) return;
      }
      term.paste(text);
      term.focus();
    };

    const pastePlain = async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (!text) { showHint('剪贴板为空'); return; }
        pasteText(text);
      } catch {
        showHint('无法读取剪贴板');
      }
    };

    const pasteSelection = () => {
      const text = term.getSelection();
      if (!text) { showHint('未选中内容'); return; }
      pasteText(text);
    };

    actionsRef.current = { copyPlain, copyRich, pastePlain, pasteSelection };

    let fitDebounceTimer = null;
    const scheduleFit = () => {
      if (fitDebounceTimer) clearTimeout(fitDebounceTimer);
      fitDebounceTimer = setTimeout(() => {
        try { fitAddon.fit(); } catch { /* ignore */ }
      }, 200);
    };
    const updateFontSize = (size) => {
      const newSize = clampFontSize(size);
      if (newSize === fontSizeRef.current) return;
      fontSizeRef.current = newSize;
      term.options.fontSize = newSize;
      invoke('save_ui_prefs', { terminalFontSize: newSize }).catch(() => {});
      scheduleFit();
    };

    // 快捷键：复制 / 粘贴 / 搜索 / 字号缩放；无选区 Ctrl+C 仍发送 ^C。
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true;
      const ctrl = e.ctrlKey && !e.altKey && !e.metaKey;
      if (!ctrl) return true;
      if (e.code === 'KeyC' && e.shiftKey) { e.preventDefault(); copyRich(); return false; }
      if (e.code === 'KeyV' && e.shiftKey) { e.preventDefault(); pastePlain(); return false; }
      if (e.code === 'KeyC') {
        if (term.getSelection()) { e.preventDefault(); copyPlain(); return false; }
        return true; // 无选区 → 交给 xterm 发送 ^C
      }
      if (e.code === 'KeyV') { e.preventDefault(); pastePlain(); return false; }
      if (e.code === 'KeyF' && !e.shiftKey) { e.preventDefault(); setSearchVisible(true); return false; }
      if (e.code === 'Equal' || e.code === 'NumpadAdd' || e.key === '+') {
        e.preventDefault();
        updateFontSize(fontSizeRef.current + 1);
        return false;
      }
      if (e.code === 'Minus' || e.code === 'NumpadSubtract') {
        e.preventDefault();
        updateFontSize(fontSizeRef.current - 1);
        return false;
      }
      if (e.code === 'Digit0' || e.code === 'Numpad0') {
        e.preventDefault();
        updateFontSize(DEFAULT_FONT_SIZE);
        return false;
      }
      return true;
    });

    // 右键菜单
    const ctxHandler = (e) => {
      e.preventDefault();
      const hasSel = !!term.getSelection();
      setCtxMenu({
        visible: true,
        x: Math.max(4, Math.min(e.clientX, window.innerWidth - 200)),
        y: Math.max(4, Math.min(e.clientY, window.innerHeight - 150)),
        hasSel,
      });
    };
    container.addEventListener('contextmenu', ctxHandler);

    // Ctrl + 鼠标滚轮 → 缩放字体（debounce fit 避免跳动）
    const wheelHandler = (e) => {
      if (e.ctrlKey || e.metaKey) {
        e.preventDefault();
        updateFontSize(fontSizeRef.current + (e.deltaY > 0 ? -1 : 1));
      }
    };
    container.addEventListener('wheel', wheelHandler, { passive: false });

    const ro = new ResizeObserver(() => {
      try { fitAddon.fit(); } catch { /* ignore */ }
    });
    ro.observe(container);

    return () => {
      container.removeEventListener('mouseup', shellClickMouseUp, true);
      container.removeEventListener('wheel', wheelHandler);
      container.removeEventListener('contextmenu', ctxHandler);
      if (fitDebounceTimer) clearTimeout(fitDebounceTimer);
      actionsRef.current = null;
      ro.disconnect();
      dataSub.dispose();
      binarySub.dispose();
      resizeSub.dispose();
      selectionSub.dispose();
      osc7Sub.dispose();
      unlisten.then(f => f()).catch(() => {});
      unlistenClosed?.then(f => f()).catch(() => {});
      webLinksAddonRef.current?.dispose();
      webLinksAddonRef.current = null;
      term.dispose();
      termRef.current = null;
      fitAddonRef.current = null;
      searchAddonRef.current = null;
    };
  }, [sessionId, showHint]);

  // 链接偏好变化时只重建链接 addon，不中断当前 SSH 会话。
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    webLinksAddonRef.current?.dispose();
    webLinksAddonRef.current = null;
    if (!linksEnabled) return;

    const addon = new WebLinksAddon((_event, uri) => {
      if (!isHttpUrl(uri)) return;
      const opened = window.open(uri, '_blank', 'noopener,noreferrer');
      if (opened) opened.opener = null;
    }, { urlRegex: WEB_LINK_REGEX });
    term.loadAddon(addon);
    webLinksAddonRef.current = addon;
  }, [linksEnabled, sessionId]);

  // 主题切换 → 热更新 xterm theme + 强制 renderer 重建 + 容器背景同步
  useEffect(() => {
    const term = termRef.current;
    // 直接用 theme prop，**不读 DOM**
    const newTheme = getXtermTheme(theme);
    const bg = terminalBg(theme);

    if (term) {
      term.options.theme = newTheme;
      try {
        // resize 相同尺寸 → 触发 canvas renderer 重建 color manager
        term.resize(term.cols, term.rows);
        // 重绘当前视口所有行
        term.refresh(0, term.rows - 1);
      } catch { /* ignore */ }
    }
    if (containerRef.current) {
      containerRef.current.style.background = bg;
    }
  }, [theme]);

  // 连接状态变化 → 提示 + 同步尺寸
  useEffect(() => {
    const term = termRef.current;
    const currentSession = sessionRef.current;
    if (!term || !currentSession) return;

    if (currentSession.connected && !wasConnected.current) {
      wasConnected.current = true;
      term.writeln(`\r\n\x1b[38;5;114m● 已连接 ${currentSession.username}@${currentSession.host}:${currentSession.port}\x1b[0m`);
      const fit = fitAddonRef.current;
      if (fit) {
        try {
          fit.fit();
          invoke(channelIdRef.current ? 'channel_resize' : 'terminal_resize', channelIdRef.current
            ? { sessionId: currentSession.id, channelId: channelIdRef.current, cols: term.cols, rows: term.rows }
            : { sessionId: currentSession.id, cols: term.cols, rows: term.rows }).catch(() => {});
        } catch { /* ignore */ }
      }
      term.focus();
    } else if (!currentSession.connected && wasConnected.current) {
      wasConnected.current = false;
      term.writeln('\r\n\x1b[38;5;203m● 连接已断开\x1b[0m');
    }
  }, [sessionConnected, sessionId]);

  const findNext = () => {
    if (searchQuery) searchAddonRef.current?.findNext(searchQuery);
  };
  const findPrevious = () => {
    if (searchQuery) searchAddonRef.current?.findPrevious(searchQuery);
  };
  const onSearchKeyDown = (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeSearch();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (e.shiftKey) findPrevious();
      else findNext();
    }
  };
  const onSearchBoxKeyDown = (e) => {
    if (e.key === 'Escape') onSearchKeyDown(e);
  };

  return (
    <div style={{ flex: 1, display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0 }}>
      {/* Toolbar */}
      <div style={{
        height: 38, flexShrink: 0, background: 'var(--md-surface-container)',
        borderBottom: '1px solid var(--md-outline-variant)',
        display: 'flex', alignItems: 'center', padding: '0 12px', gap: 10,
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, fontFamily: "'JetBrains Mono',monospace", fontSize: 12, color: 'var(--md-on-surface-variant)' }}>
          <span className="material-icons-round" style={{ fontSize: 14, color: 'var(--md-primary)' }}>dns</span>
          {session ? `${session.name} · ${session.username}@${session.host}:${session.port}` : '未选择会话'}
        </div>

        {(session?.ai_attached || session?.ai_mode || (session?.ai_readers ?? 0) > 0) && (
          <span style={{
            display: 'inline-flex', alignItems: 'center', gap: 6,
            padding: '3px 10px', borderRadius: 12, fontSize: 11, fontWeight: 500,
            background: 'var(--ai-accent-soft)', color: 'var(--ai-accent)',
          }}>
            {session.ai_attached
              ? `AI 接入中 ${session.attach_token ? `(${session.attach_token})` : ''}`
              : session.ai_mode === 'diag' ? 'AI 诊断观察中' : 'AI 只读观察中'}
            <button onClick={() => onKick(session.id)} style={{
              background: 'transparent', border: 0, color: 'inherit', cursor: 'pointer',
              fontSize: 13, padding: '0 0 0 4px', lineHeight: 1,
            }} title="踢出 AI">✕</button>
          </span>
        )}

        <div style={{ flex: 1 }} />

        <span style={{
          display: 'flex', alignItems: 'center', gap: 6, padding: '3px 10px',
          borderRadius: 10, background: 'var(--md-surface-container-high)',
          fontSize: 11, color: 'var(--md-on-surface-variant)',
        }}>
          <span style={{
            width: 6, height: 6, borderRadius: '50%',
            background: session?.connected ? '#10B981' : '#9CA3AF',
          }} />
          {connecting ? '连接中…' : session?.connected ? '已连接' : '未连接'}
        </span>

        {session && (
          <button onClick={onConnect} disabled={connecting} style={{
            height: 26, padding: '0 12px', borderRadius: 8, cursor: connecting ? 'default' : 'pointer',
            border: '1px solid var(--md-outline)', background: 'transparent',
            color: 'var(--md-on-surface)', fontFamily: 'inherit', fontSize: 11,
          }}>
            {session.connected ? '断开' : '连接'}
          </button>
        )}
      </div>

      <div style={{ position: 'relative', flex: 1, minHeight: 0, display: 'flex' }}>
        {/* 终端容器 —— 直接用 theme prop，**绝不读 DOM** */}
        {/* minWidth/overflow 必填：否则 xterm 的固有宽度会撑大 flex 项，
            FitAddon 又把撑大后的宽度当成可用宽度，列数会无限放大。 */}
        <div ref={containerRef} style={{
          flex: 1, minWidth: 0, minHeight: 0, overflow: 'hidden', padding: 8,
          background: terminalBg(theme),
        }} />

        {searchVisible && (
          <div role="search" aria-label="终端搜索" onKeyDown={onSearchBoxKeyDown} style={{
            position: 'absolute', top: 12, right: 16, zIndex: 10,
            display: 'flex', alignItems: 'center', gap: 4, padding: 6,
            borderRadius: 8, background: 'var(--md-surface-container-high)',
            border: '1px solid var(--md-outline-variant)',
            boxShadow: '0 4px 14px rgba(0,0,0,.2)',
          }}>
            <input
              ref={searchInputRef}
              value={searchQuery}
              onChange={e => setSearchQuery(e.target.value)}
              onKeyDown={onSearchKeyDown}
              placeholder="搜索终端"
              aria-label="搜索内容"
              style={{
                width: 180, height: 26, boxSizing: 'border-box', padding: '0 8px',
                border: '1px solid var(--md-outline)', borderRadius: 5,
                background: 'var(--md-surface)', color: 'var(--md-on-surface)',
                fontFamily: 'inherit', fontSize: 12, outline: 'none',
              }}
            />
            <button type="button" onClick={findPrevious} disabled={!searchQuery} aria-label="上一个搜索结果" title="上一个 (Shift+Enter)">↑</button>
            <button type="button" onClick={findNext} disabled={!searchQuery} aria-label="下一个搜索结果" title="下一个 (Enter)">↓</button>
            <button type="button" onClick={closeSearch} aria-label="关闭搜索" title="关闭 (Esc)">✕</button>
          </div>
        )}
      </div>

      {/* 右键菜单 */}
      {ctxMenu.visible && (
        <div
          onMouseDown={e => e.stopPropagation()}
          style={{
            position: 'fixed', left: ctxMenu.x, top: ctxMenu.y, zIndex: 9999,
            minWidth: 184, padding: '4px 0', borderRadius: 8,
            background: 'var(--md-surface-container-high)',
            border: '1px solid var(--md-outline-variant)',
            boxShadow: '0 8px 24px rgba(0,0,0,.24)',
          }}
        >
          {[
            { icon: 'content_copy', label: '复制', key: 'Ctrl+C', disabled: !ctxMenu.hasSel, act: () => actionsRef.current?.copyPlain() },
            { icon: 'content_paste', label: '粘贴', key: 'Ctrl+V', disabled: false, act: () => actionsRef.current?.pastePlain() },
            { icon: 'content_paste_go', label: '粘贴所选', key: '', disabled: !ctxMenu.hasSel, act: () => actionsRef.current?.pasteSelection() },
          ].map((it, i) => (
            <div key={i}
              onClick={() => {
                if (it.disabled) return;
                setCtxMenu(m => ({ ...m, visible: false }));
                it.act();
              }}
              onMouseEnter={e => { if (!it.disabled) e.currentTarget.style.background = 'var(--md-primary-container)'; }}
              onMouseLeave={e => { e.currentTarget.style.background = 'transparent'; }}
              style={{
                display: 'flex', alignItems: 'center', gap: 8, padding: '7px 12px', fontSize: 12,
                cursor: it.disabled ? 'default' : 'pointer',
                color: it.disabled ? 'var(--md-on-surface-variant)' : 'var(--md-on-surface)',
                opacity: it.disabled ? 0.45 : 1,
              }}>
              <span className="material-icons-round" style={{ fontSize: 16, color: it.disabled ? 'inherit' : 'var(--md-primary)' }}>{it.icon}</span>
              <span>{it.label}</span>
              {it.key && (
                <span style={{ marginLeft: 'auto', fontSize: 10, color: 'var(--md-on-surface-variant)', fontFamily: "'JetBrains Mono',monospace" }}>{it.key}</span>
              )}
            </div>
          ))}
        </div>
      )}

      {/* 操作提示 */}
      {hint && (
        <div style={{
          position: 'fixed', bottom: 48, right: 20, zIndex: 9999, pointerEvents: 'none',
          padding: '6px 12px', borderRadius: 8, fontSize: 11,
          background: 'var(--md-inverse-surface, rgba(32,32,32,.92))',
          color: 'var(--md-inverse-on-surface, #FFFFFF)',
          boxShadow: '0 4px 14px rgba(0,0,0,.25)',
        }}>{hint}</div>
      )}
    </div>
  );
}
