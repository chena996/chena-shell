import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import {
  basenameOf,
  detectEol,
  editorLanguageFor,
  formatBytes,
  languageLabel,
} from '../editor/languageSupport.js';
import { collectPythonMarkers, collectShellMarkers } from '../editor/diagnostics.js';

const DIAGNOSTICS_OWNER = 'chena-diagnostics';
const DIAGNOSTICS_DELAY = 700;

function tone_style(tone) {
  if (tone === 'error') return { background: '#FEE2E2', color: '#991B1B' };
  if (tone === 'ok') return { background: '#DCFCE7', color: '#166534' };
  if (tone === 'warn') return { background: '#FEF3C7', color: '#92400E' };
  return { background: 'var(--md-surface-container-low)', color: 'var(--md-on-surface)' };
}

export default function EditorWindow({ sessionId, editId }) {
  const hostRef = useRef(null);
  const editorRef = useRef(null);
  const monacoRef = useRef(null);
  const modelRef = useRef(null);
  const diagTimer = useRef(null);
  const baseRef = useRef({ size: 0, mtime: 0 });
  const dirtyRef = useRef(false);
  const allowCloseRef = useRef(false);
  const savingRef = useRef(false);
  const languageRef = useRef('plaintext');
  const shellcheckRef = useRef(true);

  const [remotePath, setRemotePath] = useState('');
  const [loading, setLoading] = useState(true);
  const [fatal, setFatal] = useState('');
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState(null);
  const [conflict, setConflict] = useState(false);
  const [closeAsk, setCloseAsk] = useState(false);
  const [readOnly, setReadOnly] = useState(false);
  const [wrap, setWrap] = useState(false);
  const [fontSize, setFontSize] = useState(14);
  const [status, setStatus] = useState({ line: 1, column: 1, markers: 0, bytes: 0 });
  const [theme, setTheme] = useState('dark');

  const setDirtyState = value => {
    dirtyRef.current = value;
    setDirty(value);
  };

  const runDiagnostics = useCallback(async model => {
    const monaco = monacoRef.current;
    const language = languageRef.current;
    if (!monaco || !model || model.isDisposed()) return;
    if (language !== 'python' && language !== 'shell') return;
    if (language === 'shell' && !shellcheckRef.current) {
      monaco.editor.setModelMarkers(model, DIAGNOSTICS_OWNER, []);
      return;
    }
    const version = model.getVersionId();
    const text = model.getValue();
    try {
      const markers = language === 'python'
        ? await collectPythonMarkers(text)
        : await collectShellMarkers(text);
      if (model.isDisposed() || model.getVersionId() !== version) return;
      monaco.editor.setModelMarkers(model, DIAGNOSTICS_OWNER, markers);
      setStatus(current => ({ ...current, markers: markers.length }));
    } catch (error) {
      setMessage({ tone: 'warn', text: `诊断不可用：${error}` });
    }
  }, []);

  const scheduleDiagnostics = useCallback(model => {
    if (diagTimer.current) clearTimeout(diagTimer.current);
    diagTimer.current = setTimeout(() => runDiagnostics(model), DIAGNOSTICS_DELAY);
  }, [runDiagnostics]);

  const save = useCallback(async (force = false) => {
    const editor = editorRef.current;
    if (!editor || savingRef.current) return false;
    savingRef.current = true;
    setSaving(true);
    setMessage(null);
    try {
      const result = await invoke('sftp_edit_save', {
        sessionId,
        editId,
        content: editor.getValue(),
        originalSize: baseRef.current.size,
        originalMtime: baseRef.current.mtime,
        confirmOverwrite: force,
      });
      baseRef.current = { size: result.size, mtime: result.mtime };
      setDirtyState(false);
      setConflict(false);
      setStatus(current => ({ ...current, bytes: editor.getValue().length }));
      setMessage({ tone: 'ok', text: `已保存到远端 · ${formatBytes(result.size)}` });
      return true;
    } catch (error) {
      const text = String(error);
      if (text.startsWith('CONFLICT:')) {
        setConflict(true);
        setMessage({ tone: 'warn', text: text.slice('CONFLICT:'.length) });
      } else {
        setMessage({ tone: 'error', text });
      }
      return false;
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  }, [editId, sessionId]);

  const finishClose = useCallback(async () => {
    allowCloseRef.current = true;
    try {
      await invoke('sftp_edit_cancel', { sessionId, editId });
    } catch {
      // 清理缓存失败不应阻止窗口关闭。
    }
    const appWindow = getCurrentWindow();
    try {
      await appWindow.close();
    } catch {
      window.close();
    }
  }, [editId, sessionId]);

  // 保存成功后关闭窗口：只有「保存」才上传远端（sftp_edit_save），
  // 关闭本身仅做本地清理（sftp_edit_cancel 不写远端），未保存关闭=丢弃不上传。
  const saveThenClose = useCallback(async (force = false) => {
    const saved = await save(force);
    if (saved) {
      await finishClose();
    }
  }, [save, finishClose]);

  useEffect(() => {
    let alive = true;
    const appWindow = getCurrentWindow();
    const unlisten = appWindow.onCloseRequested(async event => {
      if (allowCloseRef.current) return;
      event.preventDefault();
      if (!dirtyRef.current) {
        await finishClose();
        return;
      }
      setCloseAsk(true);
    });

    (async () => {
      try {
        const [editorPrefs, uiPrefs, data] = await Promise.all([
          invoke('get_editor_prefs'),
          invoke('get_ui_prefs').catch(() => null),
          invoke('sftp_edit_read', { sessionId, editId }),
        ]);
        if (!alive) return;
        shellcheckRef.current = editorPrefs?.shellcheck_enabled !== false;
        const nextTheme = uiPrefs?.theme === 'light' ? 'light' : 'dark';
        setTheme(nextTheme);
        setRemotePath(data.remote_path);
        setStatus(current => ({ ...current, bytes: data.size }));
        baseRef.current = { size: data.size, mtime: data.mtime };

        const core = await import('../editor/monacoCore.js');
        if (!alive) return;
        const monaco = core.configureEditor({
          enableSchemaRequest: !!editorPrefs?.remote_schema_enabled,
        });
        monacoRef.current = monaco;

        const language = editorLanguageFor(data.remote_path);
        languageRef.current = language;
        const model = monaco.editor.createModel(data.content, language);
        if (detectEol(data.content) === '\r\n') {
          model.setEOL(monaco.editor.EndOfLineSequence.CRLF);
        }
        modelRef.current = model;

        const editor = monaco.editor.create(hostRef.current, {
          model,
          theme: nextTheme === 'light' ? 'vs' : 'vs-dark',
          automaticLayout: true,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          fontSize,
          lineNumbers: 'on',
          folding: true,
          wordWrap: 'off',
          largeFileOptimizations: true,
          mouseWheelZoom: true,
          renderWhitespace: 'selection',
          smoothScrolling: true,
          padding: { top: 8, bottom: 8 },
        });
        editorRef.current = editor;
        editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => { saveThenClose(); });
        editor.onDidChangeModelContent(() => {
          setDirtyState(true);
          setStatus(current => ({ ...current, bytes: model.getValue().length }));
          scheduleDiagnostics(model);
        });
        editor.onDidChangeCursorPosition(event => {
          setStatus(current => ({
            ...current,
            line: event.position.lineNumber,
            column: event.position.column,
          }));
        });
        scheduleDiagnostics(model);
        setLoading(false);
        editor.focus();
      } catch (error) {
        if (!alive) return;
        setFatal(String(error));
        setLoading(false);
      }
    })();

    const onStorage = event => {
      if (event.key === 'chena-shell-theme') {
        setTheme(event.newValue === 'light' ? 'light' : 'dark');
      }
    };
    window.addEventListener('storage', onStorage);

    return () => {
      alive = false;
      unlisten.then(fn => fn()).catch(() => {});
      window.removeEventListener('storage', onStorage);
      if (diagTimer.current) clearTimeout(diagTimer.current);
      editorRef.current?.dispose();
      modelRef.current?.dispose();
      editorRef.current = null;
      modelRef.current = null;
    };
    // 仅在窗口初始化时执行一次；编辑器实例由 ref 管理。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    monacoRef.current?.editor.setTheme(theme === 'light' ? 'vs' : 'vs-dark');
  }, [theme]);

  useEffect(() => {
    editorRef.current?.updateOptions({ readOnly });
  }, [readOnly]);

  useEffect(() => {
    editorRef.current?.updateOptions({ wordWrap: wrap ? 'on' : 'off' });
  }, [wrap]);

  useEffect(() => {
    editorRef.current?.updateOptions({ fontSize });
  }, [fontSize]);

  const title = remotePath ? `${basenameOf(remotePath)} — 内置编辑器` : '内置编辑器';
  const language = languageRef.current;

  return (
    <div style={{ height: '100vh', display: 'flex', flexDirection: 'column', background: 'var(--md-surface)' }}>
      <header style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '8px 12px', borderBottom: '1px solid var(--md-outline-variant)', flexWrap: 'wrap' }}>
        <strong style={{ fontSize: 13, marginRight: 4 }}>{title}</strong>
        {dirty && <span style={{ fontSize: 11, color: '#B45309' }}>● 未保存</span>}
        <span style={{ flex: 1 }} />
        <button type="button" className="ed-toolbar__btn is-primary" onClick={() => saveThenClose()} disabled={saving || readOnly} title="保存 (Ctrl+S)">保存</button>
        <button type="button" className={`ed-toolbar__btn${readOnly ? ' is-active' : ''}`} onClick={() => setReadOnly(value => !value)} title="只读 / 可编辑">{readOnly ? '设为可编辑' : '设为只读'}</button>
        <button type="button" className={`ed-toolbar__btn${wrap ? ' is-active' : ''}`} onClick={() => setWrap(value => !value)} title="自动折行">{wrap ? '关闭折行' : '自动折行'}</button>
        <button type="button" className="ed-toolbar__btn" onClick={() => editorRef.current?.getAction('actions.find')?.run()} title="查找替换 (Ctrl+F)">查找</button>
        <button type="button" className="ed-toolbar__btn" onClick={() => setFontSize(size => Math.max(10, size - 1))} title="缩小字号">A−</button>
        <button type="button" className="ed-toolbar__btn" onClick={() => setFontSize(size => Math.min(28, size + 1))} title="放大字号">A+</button>
        <button type="button" className="ed-toolbar__btn is-close" onClick={() => { setCloseAsk(false); finishClose(); }} title="关闭编辑器">关闭</button>
      </header>

      {message && (
        <div style={{ ...tone_style(message.tone), padding: '6px 12px', fontSize: 12, display: 'flex', gap: 8, alignItems: 'center' }}>
          <span style={{ flex: 1 }}>{message.text}</span>
          <button type="button" onClick={() => setMessage(null)} style={{ background: 'transparent', border: 0, cursor: 'pointer', color: 'inherit' }}>✕</button>
        </div>
      )}

      {conflict && (
        <div style={{ background: '#FEF3C7', color: '#92400E', padding: '8px 12px', fontSize: 12, display: 'flex', gap: 8, alignItems: 'center' }}>
          <span style={{ flex: 1 }}>远端文件已被修改。确认覆盖会丢弃他人在此期间写入的内容。</span>
          <button type="button" className="ed-toolbar__btn is-primary" onClick={() => saveThenClose(true)} disabled={saving}>覆盖远端</button>
          <button type="button" className="ed-toolbar__btn" onClick={() => setConflict(false)}>取消</button>
        </div>
      )}

      <div style={{ position: 'relative', flex: 1, minHeight: 0 }}>
        <div ref={hostRef} style={{ position: 'absolute', inset: 0 }} />
        {loading && (
          <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center', background: 'var(--md-surface)', color: 'var(--md-on-surface-variant)', fontSize: 13 }}>
            正在加载编辑器…
          </div>
        )}
        {fatal && (
          <div style={{ position: 'absolute', inset: 0, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 24, background: 'var(--md-surface)', color: '#991B1B', fontSize: 13 }}>
            {fatal}
          </div>
        )}
      </div>

      <footer style={{ display: 'flex', gap: 16, padding: '4px 12px', fontSize: 11, borderTop: '1px solid var(--md-outline-variant)', color: 'var(--md-on-surface-variant)', flexWrap: 'wrap' }}>
        <span>{languageLabel(language)}</span>
        <span>Ln {status.line}, Col {status.column}</span>
        <span>{formatBytes(status.bytes)}</span>
        {(language === 'python' || language === 'shell') && <span>诊断 {status.markers}</span>}
        {readOnly && <span>只读</span>}
        <span style={{ flex: 1 }} />
        <span>{remotePath}</span>
      </footer>

      {closeAsk && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,.45)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 10000 }}>
          <div style={{ background: 'var(--md-surface)', borderRadius: 12, padding: 20, width: 420, display: 'flex', flexDirection: 'column', gap: 12 }}>
            <strong style={{ fontSize: 14 }}>有未保存的修改</strong>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)' }}>关闭窗口前可以先保存到远端，或放弃本次修改。</div>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button type="button" className="ed-toolbar__btn" onClick={() => setCloseAsk(false)}>取消</button>
              <button type="button" className="ed-toolbar__btn is-close" onClick={() => finishClose()}>放弃修改</button>
              <button
                type="button"
                className="ed-toolbar__btn is-primary"
                onClick={() => saveThenClose()}
                disabled={saving}
              >
                保存并关闭
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
