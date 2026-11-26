import { useEffect, useRef, useState } from 'react';
import './SettingsDialog.css';

const CATEGORIES = [
  { id: 'general', label: '通用', icon: 'tune', description: '应用外观与显示偏好' },
  { id: 'terminal', label: '终端', icon: 'terminal', description: '终端显示、复制与粘贴行为' },
  { id: 'connection', label: '连接', icon: 'cable', description: '连接、心跳与断线恢复' },
  { id: 'ai', label: 'AI安全', icon: 'security', description: 'AI 执行策略与接入时限' },
  { id: 'mcp', label: 'MCP服务', icon: 'dns', description: '本地 MCP 服务与访问凭证' },
  { id: 'logs', label: '日志存储', icon: 'storage', description: '日志记录、脱敏与保留期' },
];

const NUMERIC_RULES = {
  terminal_font_size: { category: 'terminal', label: '终端字号', min: 8, max: 32, unit: 'px' },
  default_keepalive_secs: { category: 'connection', label: 'Keepalive 间隔', min: 0, max: 3600, unit: '秒' },
  connect_timeout_secs: { category: 'connection', label: '连接超时', min: 1, max: 300, unit: '秒' },
  reconnect_max_attempts: { category: 'connection', label: '最大重连次数', min: 1, max: 20, unit: '次' },
  attach_token_ttl_minutes: { category: 'ai', label: '接入令牌有效期', min: 1, max: 1440, unit: '分钟' },
  idle_detach_minutes: { category: 'ai', label: 'AI 空闲断开时间', min: 1, max: 1440, unit: '分钟' },
  sudo_quota_per_attach: { category: 'ai', label: '单次接入 sudo 次数上限', min: 1, max: 1000, unit: '次' },
  sudo_max_failures: { category: 'ai', label: '密码错误锁定次数', min: 1, max: 10, unit: '次' },
  sudo_lockout_seconds: { category: 'ai', label: 'sudo 锁定时长', min: 30, max: 86400, unit: '秒' },
  ai_task_max_minutes: { category: 'ai', label: '任务授权最长时长', min: 1, max: 1440, unit: '分钟' },
  mcp_port: { category: 'mcp', label: 'MCP 端口', min: 1, max: 65535, unit: '' },
  log_retention_days: { category: 'logs', label: '日志保留天数', min: 1, max: 365, unit: '天' },
};

const REGEX_FIELDS = {
  global_blocklist: { category: 'ai', label: '全局黑名单' },
  confirm_list: { category: 'ai', label: '确认清单' },
  sudo_blocklist: { category: 'ai', label: 'sudo 黑名单' },
  sudo_confirm_list: { category: 'ai', label: 'sudo 确认清单' },
};

/// 一次性提示：V1.0.0.12 收紧了只读诊断白名单。
const ALLOWLIST_NOTICE_KEY = 'chena-shell-allowlist-tightened-v12';

function stringValue(value) {
  return String(value ?? '');
}

function linesValue(value) {
  return Array.isArray(value) ? [...value] : [];
}

function createDraft(settings = {}, uiPrefs = {}) {
  return {
    ...settings,
    theme: uiPrefs.theme ?? settings.theme ?? 'dark',
    terminal_font_size: stringValue(uiPrefs.terminal_font_size ?? 14),
    copy_on_select: uiPrefs.copy_on_select ?? settings.copy_on_select ?? false,
    confirm_multiline_paste: uiPrefs.confirm_multiline_paste ?? settings.confirm_multiline_paste ?? true,
    links_enabled: uiPrefs.links_enabled ?? settings.links_enabled ?? true,
    startup_auto_connect: settings.startup_auto_connect ?? true,
    show_onboarding: settings.show_onboarding ?? true,
    auto_reconnect: settings.auto_reconnect ?? true,
    default_keepalive_secs: stringValue(settings.default_keepalive_secs ?? 30),
    connect_timeout_secs: stringValue(settings.connect_timeout_secs ?? 30),
    reconnect_max_attempts: stringValue(settings.reconnect_max_attempts ?? 5),
    default_policy: settings.default_policy ?? 'Recommended',
    global_blocklist: linesValue(settings.global_blocklist),
    confirm_list: linesValue(settings.confirm_list),
    attach_token_ttl_minutes: stringValue(settings.attach_token_ttl_minutes ?? 30),
    idle_detach_minutes: stringValue(settings.idle_detach_minutes ?? 10),
    sudo_enabled: settings.sudo_enabled ?? false,
    sudo_password_source: settings.sudo_password_source ?? 'session',
    sudo_password_cache: settings.sudo_password_cache ?? 'attach',
    sudo_require_confirm: settings.sudo_require_confirm ?? true,
    sudo_blocklist: linesValue(settings.sudo_blocklist),
    sudo_confirm_list: linesValue(settings.sudo_confirm_list),
    sudo_quota_per_attach: stringValue(settings.sudo_quota_per_attach ?? 50),
    sudo_max_failures: stringValue(settings.sudo_max_failures ?? 3),
    sudo_lockout_seconds: stringValue(settings.sudo_lockout_seconds ?? 300),
    ai_diag_enabled: settings.ai_diag_enabled ?? true,
    ai_diag_allowlist: linesValue(settings.ai_diag_allowlist),
    ai_task_max_minutes: stringValue(settings.ai_task_max_minutes ?? 120),
    attach_token_visible: settings.attach_token_visible ?? true,
    mcp_enabled: settings.mcp_enabled ?? true,
    mcp_autostart: settings.mcp_autostart ?? true,
    mcp_host: settings.mcp_host ?? '127.0.0.1',
    mcp_port: stringValue(settings.mcp_port ?? 8767),
    mcp_require_attach_approval: settings.mcp_require_attach_approval ?? true,
    ai_echo_to_terminal: settings.ai_echo_to_terminal ?? false,
    log_ai_commands: settings.log_ai_commands ?? true,
    terminal_log_default_enabled: settings.terminal_log_default_enabled ?? true,
    log_redaction_enabled: settings.log_redaction_enabled ?? true,
    log_retention_days: stringValue(settings.log_retention_days ?? 7),
    remote_editor: settings.remote_editor ?? 'notepad',
    remote_editor_mode: settings.remote_editor_mode === 'external' ? 'external' : 'builtin',
    shellcheck_enabled: settings.shellcheck_enabled ?? true,
    remote_schema_enabled: settings.remote_schema_enabled ?? false,
  };
}

function validateRegex(pattern) {
  // Rust 默认规则使用 (?i)，转换为等效 JS flag 后再做语法检查。
  const inlineFlags = pattern.match(/^\(\?([ims]+)\)/);
  const source = inlineFlags ? pattern.slice(inlineFlags[0].length) : pattern;
  const flags = inlineFlags ? inlineFlags[1] : '';
  new RegExp(source, flags);
}

function validateField(field, value) {
  const numericRule = NUMERIC_RULES[field];
  if (numericRule) {
    const text = String(value ?? '').trim();
    const number = Number(text);
    if (!/^-?\d+$/.test(text) || !Number.isInteger(number) || number < numericRule.min || number > numericRule.max) {
      const unit = numericRule.unit ? ` ${numericRule.unit}` : '';
      return `${numericRule.label}必须是 ${numericRule.min}–${numericRule.max}${unit}之间的整数`;
    }
    return '';
  }

  const regexRule = REGEX_FIELDS[field];
  if (regexRule) {
    const lines = Array.isArray(value) ? value : String(value ?? '').split(/\r?\n/);
    for (let index = 0; index < lines.length; index += 1) {
      if (!lines[index].trim()) continue;
      try {
        validateRegex(lines[index]);
      } catch {
        return `${regexRule.label}第 ${index + 1} 行不是有效正则表达式`;
      }
    }
  }
  return '';
}

function validateDraft(draft) {
  const errors = {};
  Object.keys(NUMERIC_RULES).forEach(field => {
    const error = validateField(field, draft[field]);
    if (error) errors[field] = error;
  });
  Object.keys(REGEX_FIELDS).forEach(field => {
    const error = validateField(field, draft[field]);
    if (error) errors[field] = error;
  });
  if (!String(draft.mcp_host ?? '').trim()) {
    errors.mcp_host = 'MCP 监听地址不能为空';
  }
  return errors;
}

function toSettings(draft) {
  const settings = { ...draft };
  Object.keys(NUMERIC_RULES).forEach(field => {
    settings[field] = Number(draft[field]);
  });
  Object.keys(REGEX_FIELDS).forEach(field => {
    settings[field] = draft[field].filter(line => line.trim());
  });
  settings.ai_diag_allowlist = draft.ai_diag_allowlist
    .map(line => line.trim())
    .filter(Boolean);
  settings.mcp_host = draft.mcp_host.trim();
  settings.remote_editor = draft.remote_editor.trim();
  settings.remote_editor_mode = draft.remote_editor_mode === 'external' ? 'external' : 'builtin';
  settings.shellcheck_enabled = !!draft.shellcheck_enabled;
  settings.remote_schema_enabled = !!draft.remote_schema_enabled;
  return settings;
}

function Field({ id, label, hint, error, children, wide = false }) {
  return (
    <div className={`settings-field${wide ? ' settings-field--wide' : ''}`}>
      <label className="settings-field__label" htmlFor={id}>{label}</label>
      {children}
      {error ? <div className="settings-field__error" role="alert">{error}</div> : hint ? <div className="settings-field__hint">{hint}</div> : null}
    </div>
  );
}

function Toggle({ id, label, description, checked, onChange, disabled }) {
  return (
    <label className={`settings-toggle${disabled ? ' settings-toggle--disabled' : ''}`} htmlFor={id}>
      <span className="settings-toggle__copy">
        <span className="settings-toggle__label">{label}</span>
        {description && <span className="settings-toggle__description">{description}</span>}
      </span>
      <span className="settings-toggle__control">
        <input id={id} type="checkbox" checked={checked} onChange={onChange} disabled={disabled} />
        <span className="settings-toggle__track" aria-hidden="true"><span /></span>
      </span>
    </label>
  );
}

function mcpStatusLabel(status) {
  if (!status) return '状态未知';
  if (status.starting) return '正在启动';
  if (status.listening) return '正在监听';
  if (status.enabled === false) return '已停用';
  return '未监听';
}

export default function SettingsDialog({
  open,
  initialCategory = 'general',
  settings = {},
  uiPrefs = {},
  mcpStatus,
  migrationStatus,
  busy = false,
  onCancel,
  onSave,
  onGenerateCredential,
  onTestMcpRestart,
  onOpenLogsDir,
}) {
  const [activeCategory, setActiveCategory] = useState(() => (
    CATEGORIES.some(category => category.id === initialCategory) ? initialCategory : 'general'
  ));
  const [draft, setDraft] = useState(() => createDraft(settings, uiPrefs));
  const [errors, setErrors] = useState({});
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState('');
  const [notice, setNotice] = useState('');
  const [allowlistNoticeOpen, setAllowlistNoticeOpen] = useState(() => {
    try {
      return localStorage.getItem(ALLOWLIST_NOTICE_KEY) !== '1';
    } catch {
      return true;
    }
  });
  const dismissAllowlistNotice = () => {
    setAllowlistNoticeOpen(false);
    try {
      localStorage.setItem(ALLOWLIST_NOTICE_KEY, '1');
    } catch {
      // 存储不可用时只关闭本次提示，不影响设置保存。
    }
  };
  const [localMcpStatus, setLocalMcpStatus] = useState(mcpStatus);
  const [credential, setCredential] = useState('');
  const [confirmRotate, setConfirmRotate] = useState(false);
  const [generating, setGenerating] = useState(false);
  const [testingRestart, setTestingRestart] = useState(false);
  const [actionError, setActionError] = useState('');
  const dialogRef = useRef(null);
  const navRefs = useRef([]);
  const originalDraftRef = useRef(draft);
  const previousFocusRef = useRef(null);
  const cancelRef = useRef(null);
  const isBusy = busy || saving;
  const isDirty = JSON.stringify(originalDraftRef.current) !== JSON.stringify(draft);
  const currentCategory = CATEGORIES.find(category => category.id === activeCategory) || CATEGORIES[0];

  useEffect(() => {
    if (!open) return;
    const next = createDraft(settings, uiPrefs);
    originalDraftRef.current = next;
    setDraft(next);
    setActiveCategory(CATEGORIES.some(category => category.id === initialCategory) ? initialCategory : 'general');
    setErrors({});
    setSaveError('');
    setActionError('');
    setNotice('');
    setCredential('');
  }, [open, initialCategory, settings, uiPrefs]);

  useEffect(() => {
    setLocalMcpStatus(mcpStatus);
  }, [mcpStatus]);

  function requestCancel() {
    if (isBusy) return;
    if (isDirty && !window.confirm('设置尚未保存，确定要放弃所有修改吗？')) return;
    onCancel();
  }
  cancelRef.current = requestCancel;

  useEffect(() => {
    if (!open) return undefined;
    previousFocusRef.current = document.activeElement;
    const firstNavigation = dialogRef.current?.querySelector('[data-settings-category][aria-current="page"]')
      || dialogRef.current?.querySelector('[data-settings-category]');
    firstNavigation?.focus();

    function handleKeyDown(event) {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        cancelRef.current?.();
        return;
      }
      if (event.key !== 'Tab' || !dialogRef.current) return;
      const focusable = Array.from(dialogRef.current.querySelectorAll(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
      )).filter(element => element.getAttribute('aria-hidden') !== 'true');
      if (!focusable.length) {
        event.preventDefault();
        dialogRef.current.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const focused = document.activeElement;
      if (event.shiftKey && (focused === first || !dialogRef.current.contains(focused))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (focused === last || !dialogRef.current.contains(focused))) {
        event.preventDefault();
        first.focus();
      }
    }

    document.addEventListener('keydown', handleKeyDown, true);
    return () => {
      document.removeEventListener('keydown', handleKeyDown, true);
      previousFocusRef.current?.focus?.();
    };
  }, [open]);

  if (!open) return null;

  function updateField(field, value) {
    setDraft(current => ({ ...current, [field]: value }));
    setErrors(current => {
      if (!current[field]) return current;
      const next = { ...current };
      delete next[field];
      return next;
    });
    setSaveError('');
    setNotice('');
  }

  function blurField(field) {
    const error = validateField(field, draft[field]);
    setErrors(current => {
      const next = { ...current };
      if (error) next[field] = error;
      else delete next[field];
      return next;
    });
  }

  function selectCategory(category) {
    setActiveCategory(category);
    setActionError('');
  }

  function handleCategoryKeyDown(event, index) {
    let nextIndex;
    if (event.key === 'ArrowDown' || event.key === 'ArrowRight') nextIndex = (index + 1) % CATEGORIES.length;
    else if (event.key === 'ArrowUp' || event.key === 'ArrowLeft') nextIndex = (index - 1 + CATEGORIES.length) % CATEGORIES.length;
    else if (event.key === 'Home') nextIndex = 0;
    else if (event.key === 'End') nextIndex = CATEGORIES.length - 1;
    else return;
    event.preventDefault();
    setActiveCategory(CATEGORIES[nextIndex].id);
    navRefs.current[nextIndex]?.focus();
  }

  async function handleSubmit(event) {
    event.preventDefault();
    if (isBusy) return;
    const nextErrors = validateDraft(draft);
    setErrors(nextErrors);
    setNotice('');
    setSaveError('');
    const firstInvalidField = Object.keys(nextErrors)[0];
    if (firstInvalidField) {
      const category = NUMERIC_RULES[firstInvalidField]?.category
        || REGEX_FIELDS[firstInvalidField]?.category
        || 'mcp';
      setActiveCategory(category);
      return;
    }

    const payload = toSettings(draft);
    setSaving(true);
    try {
      const result = await onSave(payload);
      const saved = result?.settings ? { ...payload, ...result.settings } : payload;
      const next = createDraft(saved, {
        theme: saved.theme,
        terminal_font_size: saved.terminal_font_size,
      });
      originalDraftRef.current = next;
      setDraft(next);
      if (result?.mcpStatus) setLocalMcpStatus(result.mcpStatus);
      setNotice(result?.notice || '设置已保存');
    } catch (error) {
      setSaveError(typeof error === 'string' ? error : error?.message || '保存设置失败，请稍后重试');
    } finally {
      setSaving(false);
    }
  }

  async function generateCredential() {
    if (!onGenerateCredential || generating || isBusy) return;
    // 使用应用内确认框，避免 WebView2 原生 confirm 在取消时仍返回真值。
    setConfirmRotate(true);
  }

  async function applyGenerateCredential() {
    if (!onGenerateCredential || generating || isBusy) return;
    setConfirmRotate(false);
    setGenerating(true);
    setActionError('');
    setNotice('');
    try {
      const value = await onGenerateCredential();
      setCredential(String(value));
    } catch (error) {
      setActionError(typeof error === 'string' ? error : error?.message || '生成 MCP 凭证失败');
    } finally {
      setGenerating(false);
    }
  }

  function cancelRotate() {
    setConfirmRotate(false);
    setNotice('已取消，未生成新凭证');
  }

  async function testMcpRestart() {
    if (!onTestMcpRestart || testingRestart || isBusy) return;
    setTestingRestart(true);
    setActionError('');
    try {
      const result = await onTestMcpRestart();
      if (result?.mcpStatus) setLocalMcpStatus(result.mcpStatus);
      else if (result?.endpoint) setLocalMcpStatus(result);
      if (result?.notice) setNotice(result.notice);
    } catch (error) {
      setActionError(typeof error === 'string' ? error : error?.message || 'MCP 重启测试失败');
    } finally {
      setTestingRestart(false);
    }
  }

  async function openLogsDirectory() {
    if (!onOpenLogsDir) return;
    setActionError('');
    try {
      await onOpenLogsDir();
    } catch (error) {
      setActionError(typeof error === 'string' ? error : error?.message || '打开日志目录失败');
    }
  }

  function numberField(field, label, hint = '') {
    const rule = NUMERIC_RULES[field];
    const id = `settings-${field.replaceAll('_', '-')}`;
    return (
      <Field id={id} label={label} hint={hint} error={errors[field]}>
        <div className="settings-number-input">
          <input
            id={id}
            type="number"
            inputMode="numeric"
            min={rule.min}
            max={rule.max}
            step="1"
            value={draft[field]}
            onChange={event => updateField(field, event.target.value)}
            onBlur={() => blurField(field)}
            aria-invalid={Boolean(errors[field])}
            disabled={isBusy}
          />
          {rule.unit && <span>{rule.unit}</span>}
        </div>
      </Field>
    );
  }

  return (
    <div
      className="settings-dialog-backdrop"
      onMouseDown={event => { if (event.target === event.currentTarget) requestCancel(); }}
    >
      <form
        ref={dialogRef}
        className="settings-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-dialog-title"
        aria-describedby="settings-dialog-description"
        tabIndex={-1}
        onSubmit={handleSubmit}
      >
        <header className="settings-dialog__header">
          <div>
            <div className="settings-dialog__eyebrow">CHENA-SHELL / PREFERENCES</div>
            <h2 id="settings-dialog-title">
              全局设置
              {isDirty && <span className="settings-dialog__dirty" title="有未保存修改" aria-label="有未保存修改" />}
            </h2>
            <p id="settings-dialog-description">应用级默认值，不改变单个会话的专属配置；切换分类不会丢失草稿。</p>
          </div>
          <button className="settings-dialog__icon-button" type="button" onClick={requestCancel} disabled={isBusy} aria-label="关闭设置">
            <span className="material-icons-round" aria-hidden="true">close</span>
          </button>
        </header>

        <div className="settings-dialog__body">
          <nav className="settings-dialog__nav" aria-label="设置分类">
            {CATEGORIES.map((category, index) => (
              <button
                key={category.id}
                ref={element => { navRefs.current[index] = element; }}
                type="button"
                className={`settings-dialog__nav-item${activeCategory === category.id ? ' is-active' : ''}`}
                aria-current={activeCategory === category.id ? 'page' : undefined}
                data-settings-category={category.id}
                onClick={() => selectCategory(category.id)}
                onKeyDown={event => handleCategoryKeyDown(event, index)}
              >
                <span className="material-icons-round" aria-hidden="true">{category.icon}</span>
                <span>{category.label}</span>
              </button>
            ))}
          </nav>

          <main className="settings-dialog__content" aria-label={`${currentCategory.label}设置`}>
            <div className="settings-dialog__content-heading">
              <div className="settings-dialog__category-icon material-icons-round" aria-hidden="true">{currentCategory.icon}</div>
              <div>
                <h3>{currentCategory.label}</h3>
                <p>{currentCategory.description}</p>
              </div>
            </div>

            {activeCategory === 'general' && (
              <>
              <section className="settings-section" aria-label="外观">
                <h4>外观</h4>
                <div className="settings-grid">
                  <Field id="settings-theme" label="界面主题" hint="主题与终端字号会随其他设置一并保存。">
                    <select id="settings-theme" value={draft.theme} onChange={event => updateField('theme', event.target.value)} disabled={isBusy}>
                      <option value="dark">深色</option>
                      <option value="light">浅色</option>
                    </select>
                  </Field>
                </div>
                <div className="settings-toggle-list">
                  <Toggle id="settings-startup-auto-connect" label="启动后自动连接" description="应用启动后按受限并发连接标记为自动连接的会话。" checked={draft.startup_auto_connect} onChange={event => updateField('startup_auto_connect', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-show-onboarding" label="显示新手引导" description="启动时显示功能引导与 MCP 接入提示。" checked={draft.show_onboarding} onChange={event => updateField('show_onboarding', event.target.checked)} disabled={isBusy} />
                </div>
                <div className="settings-callout">
                  <span className="material-icons-round" aria-hidden="true">encrypted</span>
                  <div><strong>本地加密配置</strong><p>全局设置和界面偏好保存在应用加密配置中，不会写入终端日志。</p></div>
                </div>
              </section>
              <section className="settings-section" aria-label="远端编辑">
                <h4>远端编辑</h4>
                <div className="settings-grid">
                  <Field id="settings-remote-editor-mode" label="编辑方式" hint="内置编辑器支持 markdown、shell、python、yaml、json；外部编辑器仍按下方程序名启动。">
                    <select id="settings-remote-editor-mode" value={draft.remote_editor_mode} onChange={event => updateField('remote_editor_mode', event.target.value)} disabled={isBusy}>
                      <option value="builtin">内置编辑器</option>
                      <option value="external">外部编辑器</option>
                    </select>
                  </Field>
                  <Field id="settings-remote-editor" label="外部编辑器程序" hint="仅在编辑方式为外部编辑器时生效，默认使用系统记事本（notepad）；留空则使用系统默认关联程序。">
                    <input id="settings-remote-editor" type="text" value={draft.remote_editor} placeholder="notepad" onChange={event => updateField('remote_editor', event.target.value)} spellCheck={false} disabled={isBusy} />
                  </Field>
                </div>
                <div className="settings-toggle-list">
                  <Toggle id="settings-shellcheck" label="Shell 语法检查" description="内置编辑器打开 shell 文件时，用本地 pureshellcheck（MIT）经 Pyodide（MPL-2.0）检查并标出问题；首次加载约 13 MiB 本地运行时，冷启动约 1–3 秒，覆盖 71 项常见检查，完全离线不联网。" checked={draft.shellcheck_enabled} onChange={event => updateField('shellcheck_enabled', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-remote-schema" label="允许拉取远程 schema" description="YAML/JSON 文件声明了 $schema 时按需联网获取，用于补全和校验；关闭后完全离线。" checked={draft.remote_schema_enabled} onChange={event => updateField('remote_schema_enabled', event.target.checked)} disabled={isBusy} />
                </div>
              </section>
              </>
            )}

            {activeCategory === 'terminal' && (
              <section className="settings-section" aria-label="终端偏好">
                <h4>终端偏好</h4>
                <div className="settings-grid">
                  {numberField('terminal_font_size', '终端字号', '范围 8–32 px。')}
                </div>
                <div className="settings-toggle-list">
                  <Toggle id="settings-copy-on-select" label="选中即复制" description="鼠标选择终端文本后自动复制到剪贴板。" checked={draft.copy_on_select} onChange={event => updateField('copy_on_select', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-confirm-multiline-paste" label="多行粘贴前确认" description="粘贴多行内容时先显示行数，避免误执行命令。" checked={draft.confirm_multiline_paste} onChange={event => updateField('confirm_multiline_paste', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-links-enabled" label="启用终端链接" description="允许按住 Ctrl 点击终端中的 HTTP(S) 链接。" checked={draft.links_enabled} onChange={event => updateField('links_enabled', event.target.checked)} disabled={isBusy} />
                </div>
              </section>
            )}

            {activeCategory === 'connection' && (
              <section className="settings-section" aria-label="连接策略">
                <h4>连接策略</h4>
                <div className="settings-grid">
                  {numberField('default_keepalive_secs', 'Keepalive 间隔', '发送 SSH 心跳的间隔，0 表示关闭，范围 0–3600 秒。')}
                  {numberField('connect_timeout_secs', '连接超时', '建立连接的最长等待时间，范围 1–300 秒。')}
                  {numberField('reconnect_max_attempts', '最大重连次数', '自动重连达到次数后停止，需手动重试。')}
                </div>
                <div className="settings-toggle-list">
                  <Toggle id="settings-auto-reconnect" label="断线自动重连" description="采用指数退避；主动断开、认证失败或主机密钥变化时不重连。" checked={draft.auto_reconnect} onChange={event => updateField('auto_reconnect', event.target.checked)} disabled={isBusy} />
                </div>
                <div className="settings-callout settings-callout--neutral">
                  <span className="material-icons-round" aria-hidden="true">verified_user</span>
                  <div><strong>known_hosts 主机密钥保护</strong><p>记录位于 <code>%APPDATA%\chena-shell\known_hosts</code>。首次连接采用 TOFU 记录密钥；后续密钥变化会拒绝连接并要求人工确认。此处仅说明存储与校验行为，不提供伪造的打开或清理操作。</p></div>
                </div>
              </section>
            )}

            {activeCategory === 'ai' && (
              <section className="settings-section" aria-label="AI 安全策略">
                <h4>AI 安全策略</h4>
                <div className="settings-grid">
                  <Field id="settings-default-policy" label="全局默认策略" hint="会话未单独覆盖时使用此策略。" wide>
                    <select id="settings-default-policy" value={draft.default_policy} onChange={event => updateField('default_policy', event.target.value)} disabled={isBusy}>
                      <option value="Strict">严格：默认拒绝 AI 执行</option>
                      <option value="Recommended">推荐：危险命令拦截，敏感命令确认</option>
                      <option value="Permissive">宽松：仅拦截全局黑名单</option>
                    </select>
                  </Field>
                  <Field id="settings-global-blocklist" label="全局黑名单（每行一个正则）" hint="匹配命令将被直接拒绝；支持 Rust 风格的起始 (?i) 标记。" error={errors.global_blocklist} wide>
                    <textarea id="settings-global-blocklist" className="settings-code-input" rows={5} value={draft.global_blocklist.join('\n')} onChange={event => updateField('global_blocklist', event.target.value.split(/\r?\n/))} onBlur={() => blurField('global_blocklist')} aria-invalid={Boolean(errors.global_blocklist)} spellCheck={false} disabled={isBusy} />
                  </Field>
                  <Field id="settings-confirm-list" label="确认清单（每行一个正则）" hint="匹配命令需要人工批准一次，批准不能复用。" error={errors.confirm_list} wide>
                    <textarea id="settings-confirm-list" className="settings-code-input" rows={4} value={draft.confirm_list.join('\n')} onChange={event => updateField('confirm_list', event.target.value.split(/\r?\n/))} onBlur={() => blurField('confirm_list')} aria-invalid={Boolean(errors.confirm_list)} spellCheck={false} disabled={isBusy} />
                  </Field>
                  {numberField('attach_token_ttl_minutes', '接入令牌有效期', '范围 1–1440 分钟，过期后客户端必须重新接入。')}
                  {numberField('idle_detach_minutes', 'AI 空闲断开时间', '范围 1–1440 分钟，无活动后自动断开。')}
                </div>
                <div className="settings-toggle-list">
                  <Toggle id="settings-attach-token-visible" label="允许显示接入令牌" description="关闭后不在会话界面展示接入令牌。" checked={draft.attach_token_visible} onChange={event => updateField('attach_token_visible', event.target.checked)} disabled={isBusy} />
                </div>

                <h4 style={{ marginTop: 22 }}>AI root 执行（sudo）</h4>
                <div className="settings-toggle-list">
                  <Toggle id="settings-sudo-enabled" label="允许 AI 以 root 执行" description="默认关闭。关闭时 AI 申请 root 执行会被直接拒绝。" checked={draft.sudo_enabled} onChange={event => updateField('sudo_enabled', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-sudo-require-confirm" label="root 命令逐条确认" description="每条 root 命令都需要人工批准一次；关闭后仍受黑名单约束。" checked={draft.sudo_require_confirm} onChange={event => updateField('sudo_require_confirm', event.target.checked)} disabled={isBusy} />
                </div>
                <div className="settings-grid">
                  <Field id="settings-sudo-password-source" label="sudo 密码来源" hint="会话密码优先：先复用登录密码；仅手动：每次弹窗输入；仅免密：只支持 NOPASSWD。">
                    <select id="settings-sudo-password-source" value={draft.sudo_password_source} onChange={event => updateField('sudo_password_source', event.target.value)} disabled={isBusy || !draft.sudo_enabled}>
                      <option value="session">会话密码优先</option>
                      <option value="manual">仅手动输入</option>
                      <option value="passwordless">仅免密（NOPASSWD）</option>
                    </select>
                  </Field>
                  <Field id="settings-sudo-password-cache" label="sudo 密码缓存" hint="密码只存内存：本次接入在断开时清除，本次应用运行在退出时清除。">
                    <select id="settings-sudo-password-cache" value={draft.sudo_password_cache} onChange={event => updateField('sudo_password_cache', event.target.value)} disabled={isBusy || !draft.sudo_enabled}>
                      <option value="none">不缓存</option>
                      <option value="attach">本次接入</option>
                      <option value="app">本次应用运行</option>
                    </select>
                  </Field>
                  {numberField('sudo_quota_per_attach', '单次接入 sudo 次数上限', '范围 1–1000，超出后需重新接入。')}
                  {numberField('sudo_max_failures', '密码错误锁定次数', '连续输错达到该次数后锁定，范围 1–10。')}
                  {numberField('sudo_lockout_seconds', 'sudo 锁定时长', '范围 30–86400 秒。')}
                  <Field id="settings-sudo-blocklist" label="sudo 黑名单（每行一个正则）" hint="仅对 root 执行生效，命中直接拒绝。默认含 sudo -i、sudoers、authorized_keys 等。" error={errors.sudo_blocklist} wide>
                    <textarea id="settings-sudo-blocklist" className="settings-code-input" rows={5} value={draft.sudo_blocklist.join('\n')} onChange={event => updateField('sudo_blocklist', event.target.value.split(/\r?\n/))} onBlur={() => blurField('sudo_blocklist')} aria-invalid={Boolean(errors.sudo_blocklist)} spellCheck={false} disabled={isBusy || !draft.sudo_enabled} />
                  </Field>
                  <Field id="settings-sudo-confirm-list" label="sudo 确认清单（每行一个正则）" hint="仅对 root 执行生效，命中需人工批准一次。默认含 rm、systemctl restart、iptables 等。" error={errors.sudo_confirm_list} wide>
                    <textarea id="settings-sudo-confirm-list" className="settings-code-input" rows={4} value={draft.sudo_confirm_list.join('\n')} onChange={event => updateField('sudo_confirm_list', event.target.value.split(/\r?\n/))} onBlur={() => blurField('sudo_confirm_list')} aria-invalid={Boolean(errors.sudo_confirm_list)} spellCheck={false} disabled={isBusy || !draft.sudo_enabled} />
                  </Field>
                </div>
                <div className="settings-callout settings-callout--neutral">
                  <span className="material-icons-round" aria-hidden="true">admin_panel_settings</span>
                  <div><strong>sudo 密码永远不会交给 AI</strong><p>密码只由你在此应用界面输入，经加密通道写入 sudo 的标准输入，不进入命令行参数、日志、回放或报告。</p></div>
                </div>

                <h4 style={{ marginTop: 22 }}>只读诊断与任务级授权</h4>
                <div className="settings-toggle-list">
                  <Toggle id="settings-ai-diag-enabled" label="启用只读诊断模式" description="只读诊断的观察者不独占终端，只能执行下方白名单内的只读命令；不允许管道、重定向、命令串联和 root。" checked={draft.ai_diag_enabled} onChange={event => updateField('ai_diag_enabled', event.target.checked)} disabled={isBusy} />
                </div>
                {allowlistNoticeOpen && (
                  <div className="settings-callout settings-callout--neutral" role="status">
                    <span className="material-icons-round" aria-hidden="true">shield</span>
                    <div>
                      <strong>本次升级收紧了只读白名单</strong>
                      <p>无路径声明的条目不再接受任意绝对路径；新增拦截 dig -f、curl -D/-c、sar -o 等危险参数，并拒绝通配符与 /root、/etc/shadow、.ssh 等敏感路径。下方已有条目若依赖这些写法会失效，需要改成显式路径。</p>
                    </div>
                    <button type="button" className="settings-callout-dismiss" onClick={dismissAllowlistNotice}>知道了</button>
                  </div>
                )}
                <div className="settings-grid">
                  {numberField('ai_task_max_minutes', '任务授权最长时长', '范围 1–1440 分钟；任务到期后自动断开。')}
                  <Field id="settings-ai-diag-allowlist" label="只读诊断命令白名单（每行一个）" hint="普通条目按完整命令或参数边界匹配，例如 ss 放行 ss -s；以 / 结尾的条目按目录前缀匹配，例如 cat /proc/ 放行 cat /proc/net/dev。只有 ls、stat、du、df、wc、readlink、lsof 等元数据命令可带未声明的绝对路径，其余读取类命令必须显式声明路径；始终拒绝通配符、.. 穿越、$ 与 ~ 展开、敏感路径以及 dig -f、curl -D/-c、sar -o 等危险参数；会话以 root 登录时只放行内置条目。留空表示不允许任何只读诊断执行。" wide>
                    <textarea id="settings-ai-diag-allowlist" className="settings-code-input" rows={8} value={draft.ai_diag_allowlist.join('\n')} onChange={event => updateField('ai_diag_allowlist', event.target.value.split(/\r?\n/))} spellCheck={false} disabled={isBusy || !draft.ai_diag_enabled} />
                  </Field>
                </div>
                <div className="settings-callout settings-callout--neutral">
                  <span className="material-icons-round" aria-hidden="true">manage_search</span>
                  <div><strong>任务级授权</strong><p>AI 以可写接入并声明任务目标后，任务内白名单只读命令免逐条审批；写操作、高危命令和 root 仍需单独确认，任务到期自动断开。sudo 连续输错密码会被拒绝，并在达到「密码错误锁定次数」后锁定。</p></div>
                </div>
              </section>
            )}

            {activeCategory === 'mcp' && (
              <section className="settings-section" aria-label="MCP 服务配置">
                <div className="settings-mcp-status">
                  <div>
                    <span className={`settings-mcp-status__dot${localMcpStatus?.listening ? ' is-online' : ''}`} aria-hidden="true" />
                    <strong>{mcpStatusLabel(localMcpStatus)}</strong>
                  </div>
                  <span>{localMcpStatus?.endpoint || '尚无服务状态'}</span>
                </div>
                <h4>服务配置</h4>
                <div className="settings-toggle-list">
                  <Toggle id="settings-mcp-enabled" label="启用 MCP 服务" description="允许本地 AI 客户端通过 HTTP 接入。" checked={draft.mcp_enabled} onChange={event => updateField('mcp_enabled', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-mcp-autostart" label="应用启动时自动启动" description="仅在启用 MCP 服务时生效。" checked={draft.mcp_autostart} onChange={event => updateField('mcp_autostart', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-mcp-require-approval" label="AI 接入需要人工批准" description="新客户端接入前必须经过单次人工审批。" checked={draft.mcp_require_attach_approval} onChange={event => updateField('mcp_require_attach_approval', event.target.checked)} disabled={isBusy} />
                </div>
                <h4>AI 输出</h4>
                <div className="settings-toggle-list">
                  <Toggle id="settings-ai-echo-terminal" label="AI 输出同时在人工终端回显" description="默认关闭：AI 执行过程只进入独立的只读 AI 标签，避免与人工终端混淆。" checked={draft.ai_echo_to_terminal} onChange={event => updateField('ai_echo_to_terminal', event.target.checked)} disabled={isBusy} />
                </div>
                <div className="settings-grid settings-grid--endpoint">
                  <Field id="settings-mcp-host" label="MCP 监听地址" hint="为降低暴露面，应仅使用本机回环地址（127.0.0.1）。" error={errors.mcp_host}>
                    <input id="settings-mcp-host" type="text" value={draft.mcp_host} onChange={event => updateField('mcp_host', event.target.value)} onBlur={() => { if (!draft.mcp_host.trim()) setErrors(current => ({ ...current, mcp_host: 'MCP 监听地址不能为空' })); }} aria-invalid={Boolean(errors.mcp_host)} spellCheck={false} disabled={isBusy} />
                  </Field>
                  {numberField('mcp_port', 'MCP 端口', '范围 1–65535。')}
                </div>

                <div className="settings-callout settings-callout--neutral">
                  <span className="material-icons-round" aria-hidden="true">integration_instructions</span>
                  <div>
                    <strong>标准 MCP 客户端接入</strong>
                    <p>HTTP：POST <code>{localMcpStatus?.endpoint || 'http://127.0.0.1:8767/mcp'}</code>，请求头使用 <code>Authorization: Bearer &lt;凭证&gt;</code>。</p>
                    <p>stdio：运行 <code>Chena-Shell_V1.0.0.13.exe --mcp-stdio</code>，并设置环境变量 <code>CHENA_MCP_TOKEN</code>；非默认端口时同时设置 <code>CHENA_MCP_URL</code>。</p>
                  </div>
                </div>

                <div className="settings-credential">
                  <div className="settings-credential__heading">
                    <div><h4>访问凭证</h4><p>{localMcpStatus?.credential_configured ? '当前已配置凭证；配置中只保存盐化哈希。' : '尚未检测到已配置的访问凭证。'}</p></div>
                    <button className="settings-button settings-button--tonal" type="button" onClick={generateCredential} disabled={!onGenerateCredential || generating || isBusy}>
                      {generating ? '正在生成…' : '重新生成凭证'}
                    </button>
                  </div>
                  {credential && (
                    <div className="settings-credential__reveal" role="status">
                      <label htmlFor="settings-new-credential">新凭证（仅此一次完整显示）</label>
                      <input id="settings-new-credential" type="text" readOnly value={credential} onFocus={event => event.target.select()} autoComplete="off" spellCheck={false} />
                      <p>请立即复制并妥善保存。隐藏或关闭设置后，无法再次查看此完整值。</p>
                      <button className="settings-text-button" type="button" onClick={() => setCredential('')}>我已保存，隐藏凭证</button>
                    </div>
                  )}
                </div>

                {onTestMcpRestart && (
                  <button className="settings-button settings-button--outlined settings-dialog__restart" type="button" onClick={testMcpRestart} disabled={testingRestart || isBusy}>
                    {testingRestart ? '正在测试重启…' : '测试 MCP 重启'}
                  </button>
                )}
                {actionError && <div className="settings-dialog__inline-error" role="alert">{actionError}</div>}
              </section>
            )}

            {activeCategory === 'logs' && (
              <section className="settings-section" aria-label="日志与存储">
                <h4>日志与存储</h4>
                <div className="settings-toggle-list">
                  <Toggle id="settings-log-ai-commands" label="记录 AI 命令日志" description="记录 AI 命令、审批、拒绝及执行结果。" checked={draft.log_ai_commands} onChange={event => updateField('log_ai_commands', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-terminal-log-default" label="默认记录终端日志" description="默认开启并应用到所有会话；切换后已连接会话重连生效。" checked={draft.terminal_log_default_enabled} onChange={event => updateField('terminal_log_default_enabled', event.target.checked)} disabled={isBusy} />
                  <Toggle id="settings-log-redaction" label="日志敏感信息脱敏" description="写入前脱敏密码、令牌和其他已识别的敏感内容。" checked={draft.log_redaction_enabled} onChange={event => updateField('log_redaction_enabled', event.target.checked)} disabled={isBusy} />
                </div>
                <div className="settings-grid">
                  {numberField('log_retention_days', '日志保留天数', '范围 1–365 天；过期日志由后端定期清理。')}
                </div>
                <div className="settings-callout settings-callout--neutral">
                  <span className="material-icons-round" aria-hidden="true">inventory_2</span>
                  <div>
                    <strong>存储位置与迁移</strong>
                    <p>日志目录：<code>%APPDATA%\chena-shell\logs</code>；known_hosts：<code>%APPDATA%\chena-shell\known_hosts</code>。主机密钥替换仅通过高危确认对话框执行。</p>
                    <p>{migrationStatus?.secret_storage_migration_performed ? '本次启动已完成旧秘密迁移。' : '本次启动无需执行旧秘密迁移。'} 密码和私钥口令通过 Windows Credential Manager 引用解析，不在此处回显。</p>
                    {onOpenLogsDir && <button className="settings-text-button" type="button" onClick={openLogsDirectory} disabled={isBusy}>打开日志目录</button>}
                  </div>
                </div>
                {actionError && <div className="settings-dialog__inline-error" role="alert">{actionError}</div>}
              </section>
            )}
          </main>
        </div>

        <footer className="settings-dialog__footer">
          <div className="settings-dialog__feedback" aria-live="polite">
            {saveError ? <span className="is-error" role="alert">{saveError}</span> : notice ? <span className="is-success" role="status">{notice}</span> : isDirty ? <span>有未保存的修改</span> : <span>所有修改均已保存</span>}
          </div>
          <div className="settings-dialog__actions">
            <button className="settings-button settings-button--outlined" type="button" onClick={requestCancel} disabled={isBusy}>取消</button>
            <button className="settings-button settings-button--primary" type="submit" disabled={isBusy}>
              {isBusy ? '保存中…' : '保存设置'}
            </button>
          </div>
        </footer>
      </form>

      {confirmRotate && (
        <div className="settings-confirm-overlay" role="presentation">
          <div className="settings-confirm" role="alertdialog" aria-modal="true" aria-labelledby="settings-rotate-title">
            <h4 id="settings-rotate-title">重新生成 MCP 凭证</h4>
            <p>重新生成后，旧 MCP 凭证将立即失效。确定继续吗？</p>
            <div className="settings-confirm-actions">
              <button className="settings-button settings-button--outlined" type="button" onClick={cancelRotate}>取消</button>
              <button className="settings-button settings-button--primary" type="button" onClick={applyGenerateCredential}>确定</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
