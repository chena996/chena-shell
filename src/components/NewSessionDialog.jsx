import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import './NewSessionDialog.css';

export const MAX_PROXY_JUMPS = 5;

export function resolveProxyJumpChain(sessionId, chain, sessions) {
  const saved = new Map(sessions.map(session => [session.id, session]));
  const target = { id: sessionId || '', proxy_jump: chain };
  const ordered = [];
  const visiting = [target.id];

  function appendHop(rawId) {
    const id = rawId.trim();
    if (!id) throw new Error('ProxyJump 会话 ID 不能为空');
    if (visiting.length > MAX_PROXY_JUMPS) {
      throw new Error(`展开后的 ProxyJump 链最多 ${MAX_PROXY_JUMPS} 跳`);
    }
    if (visiting.includes(id)) {
      throw new Error(`ProxyJump 存在循环引用: ${[...visiting, id].join(' → ')}`);
    }
    if (ordered.includes(id)) throw new Error(`ProxyJump 会话重复: ${id}`);
    const config = id === target.id ? target : saved.get(id);
    if (!config) throw new Error(`ProxyJump 会话不存在: ${id}`);
    const parents = Array.isArray(config.proxy_jump) ? config.proxy_jump : [];
    if (parents.length > MAX_PROXY_JUMPS) {
      throw new Error(`会话 ${id} 的 ProxyJump 最多配置 ${MAX_PROXY_JUMPS} 跳`);
    }
    visiting.push(id);
    parents.forEach(appendHop);
    visiting.pop();
    if (ordered.length >= MAX_PROXY_JUMPS) {
      throw new Error(`展开后的 ProxyJump 链最多 ${MAX_PROXY_JUMPS} 跳`);
    }
    ordered.push(id);
  }

  if (chain.length > MAX_PROXY_JUMPS) {
    throw new Error(`ProxyJump 最多配置 ${MAX_PROXY_JUMPS} 跳`);
  }
  chain.forEach(appendHop);
  return ordered;
}

export function proxyJumpValidationError(sessionId, chain, sessions) {
  if (sessionId && chain.includes(sessionId)) return '不能将当前会话作为跳板（自引用）';
  try {
    resolveProxyJumpChain(sessionId, chain, sessions);
    return '';
  } catch (error) {
    return error.message;
  }
}

let forwardSequence = 0;

function createForwardId() {
  forwardSequence += 1;
  return `fwd-${Date.now()}-${forwardSequence}`;
}

function createForward(forward = {}) {
  return {
    id: forward.id || createForwardId(),
    kind: forward.kind === 'remote' ? 'remote' : 'local',
    listen_host: forward.listen_host ?? '127.0.0.1',
    listen_port: String(forward.listen_port ?? ''),
    dest_host: forward.dest_host ?? '127.0.0.1',
    dest_port: String(forward.dest_port ?? ''),
    enabled: forward.enabled ?? true,
  };
}

function createDraft(initial, defaults = {}) {
  const authType = initial?.auth?.type || 'Password';
  return {
    name: initial?.name ?? '',
    host: initial?.host ?? '',
    port: String(initial?.port ?? 22),
    username: initial?.username ?? 'root',
    group: initial?.group ?? '默认分组',
    authType,
    keySource: authType === 'Key' && initial?.auth?.has_inline_key ? 'inline' : 'path',
    privateKeyPath: authType === 'Key' ? initial?.auth?.private_key_path ?? '' : '',
    privateKeyContent: '',
    password: '',
    passphrase: '',
    secretAction: initial && authType !== 'Agent' ? 'keep' : 'replace',
    proxyJump: Array.isArray(initial?.proxy_jump) ? [...initial.proxy_jump] : [],
    autoConnect: initial?.auto_connect ?? true,
    keepaliveSecs: String(initial?.keepalive_secs ?? defaults.default_keepalive_secs ?? 30),
    connectTimeoutSecs: String(initial?.connect_timeout_secs ?? defaults.connect_timeout_secs ?? 30),
    logEnabled: initial?.log_enabled ?? defaults.terminal_log_default_enabled ?? true,
    aiAllowed: initial?.ai_allowed ?? true,
    aiPolicy: initial?.ai_policy || 'Inherit',
    aiBlocklist: Array.isArray(initial?.ai_blocklist_regexes)
      ? initial.ai_blocklist_regexes.join('\n')
      : '',
    forwards: Array.isArray(initial?.port_forwards)
      ? initial.port_forwards.map(createForward)
      : [],
  };
}

function isIntegerInRange(value, min, max) {
  const text = String(value).trim();
  if (!/^\d+$/.test(text)) return false;
  const number = Number(text);
  return Number.isInteger(number) && number >= min && number <= max;
}

function validateRegex(pattern) {
  const inlineFlags = pattern.match(/^\(\?([ims]+)\)/);
  const source = inlineFlags ? pattern.slice(inlineFlags[0].length) : pattern;
  const flags = inlineFlags ? inlineFlags[1] : '';
  new RegExp(source, flags);
}

function validateDraft(draft, isEdit, initial, sessions) {
  const errors = {};
  if (!draft.host.trim()) errors.host = '主机地址不能为空';
  if (!draft.username.trim()) errors.username = '用户名不能为空';
  if (!isIntegerInRange(draft.port, 1, 65535)) errors.port = '端口必须是 1–65535 之间的整数';
  if (!isIntegerInRange(draft.keepaliveSecs, 0, 3600)) {
    errors.keepalive = 'Keepalive 必须是 0–3600 秒之间的整数（0 表示关闭）';
  }
  if (!isIntegerInRange(draft.connectTimeoutSecs, 1, 300)) {
    errors.connectTimeout = '连接超时必须是 1–300 秒之间的整数';
  }
  const proxyJumpError = proxyJumpValidationError(initial?.id, draft.proxyJump, sessions);
  if (proxyJumpError) errors.proxyJump = proxyJumpError;

  const originalType = initial?.auth?.type;
  const typeChanged = isEdit && draft.authType !== originalType;
  if (draft.authType === 'Password') {
    if ((!isEdit || typeChanged || draft.secretAction === 'replace') && !draft.password) {
      errors.password = typeChanged || !isEdit ? '新建密码认证必须输入密码' : '请输入用于替换的新密码';
    }
  } else if (draft.authType === 'Key') {
    if (draft.keySource === 'path') {
      if (!draft.privateKeyPath.trim()) errors.privateKeyPath = '私钥路径不能为空，或改为直接粘贴私钥内容';
    } else if (!draft.privateKeyContent.trim() && !(isEdit && initial?.auth?.has_inline_key)) {
      errors.privateKeyContent = '请粘贴私钥内容（以 -----BEGIN 开头）';
    }
    if (isEdit && !typeChanged && draft.secretAction === 'replace' && !draft.passphrase) {
      errors.passphrase = '请输入用于替换的新私钥口令';
    }
  }

  const regexLines = draft.aiBlocklist.split(/\r?\n/);
  for (let index = 0; index < regexLines.length; index += 1) {
    const pattern = regexLines[index].trim();
    if (!pattern) continue;
    try {
      validateRegex(pattern);
    } catch {
      errors.aiBlocklist = `AI 黑名单第 ${index + 1} 行不是有效正则表达式`;
      break;
    }
  }

  draft.forwards.forEach((forward, index) => {
    const prefix = `forward-${forward.id}`;
    if (!forward.listen_host.trim()) errors[`${prefix}-listen-host`] = `转发 ${index + 1} 的监听地址不能为空`;
    if (!forward.dest_host.trim()) errors[`${prefix}-dest-host`] = `转发 ${index + 1} 的目标地址不能为空`;
    if (!isIntegerInRange(forward.listen_port, 1, 65535)) {
      errors[`${prefix}-listen-port`] = `转发 ${index + 1} 的监听端口必须是 1–65535 之间的整数`;
    }
    if (!isIntegerInRange(forward.dest_port, 1, 65535)) {
      errors[`${prefix}-dest-port`] = `转发 ${index + 1} 的目标端口必须是 1–65535 之间的整数`;
    }
  });

  return errors;
}

function regexLines(value) {
  return value.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
}

function Field({ id, label, error, children, className = '' }) {
  return (
    <div className={`session-dialog__field ${className}`}>
      <label htmlFor={id}>{label}</label>
      {children}
      {error && <div className="session-dialog__error">{error}</div>}
    </div>
  );
}

export default function NewSessionDialog({ onCancel, onCreate, initial, defaults = {}, sessions: providedSessions }) {
  const isEdit = Boolean(initial);
  const [draft, setDraft] = useState(() => createDraft(initial, defaults));
  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState('');
  const [loadedSessions, setLoadedSessions] = useState([]);
  const [jumpCandidate, setJumpCandidate] = useState('');

  useEffect(() => {
    if (providedSessions) return undefined;
    let alive = true;
    invoke('list_sessions').then(items => {
      if (alive) setLoadedSessions(items.map(item => item.config || item));
    }).catch(() => {
      if (alive) setLoadedSessions([]);
    });
    return () => { alive = false; };
  }, [providedSessions]);

  const availableSessions = providedSessions || loadedSessions;
  const originalType = initial?.auth?.type;
  const sameAuthType = isEdit && draft.authType === originalType;
  const errors = validateDraft(draft, isEdit, initial, availableSessions);
  const invalid = Object.keys(errors).length > 0;

  function update(field, value) {
    setDraft(current => ({ ...current, [field]: value }));
  }

  function changeAuthType(authType) {
    setDraft(current => ({
      ...current,
      authType,
      keySource: authType === 'Key' && initial?.auth?.has_inline_key ? 'inline' : 'path',
      privateKeyPath: authType === 'Key' && originalType === 'Key'
        ? initial.auth.private_key_path || ''
        : '',
      privateKeyContent: '',
      password: '',
      passphrase: '',
      secretAction: isEdit && authType === originalType && authType !== 'Agent' ? 'keep' : 'replace',
    }));
  }

  function changeSecretAction(secretAction) {
    setDraft(current => ({ ...current, secretAction, password: '', passphrase: '' }));
  }

  function addProxyJump() {
    if (!jumpCandidate || draft.proxyJump.length >= MAX_PROXY_JUMPS) return;
    setDraft(current => ({ ...current, proxyJump: [...current.proxyJump, jumpCandidate] }));
    setJumpCandidate('');
  }

  function removeProxyJump(index) {
    setDraft(current => ({
      ...current,
      proxyJump: current.proxyJump.filter((_, itemIndex) => itemIndex !== index),
    }));
  }

  function moveProxyJump(index, offset) {
    setDraft(current => {
      const destination = index + offset;
      if (destination < 0 || destination >= current.proxyJump.length) return current;
      const proxyJump = [...current.proxyJump];
      [proxyJump[index], proxyJump[destination]] = [proxyJump[destination], proxyJump[index]];
      return { ...current, proxyJump };
    });
  }

  function updateForward(id, field, value) {
    setDraft(current => ({
      ...current,
      forwards: current.forwards.map(forward => (
        forward.id === id ? { ...forward, [field]: value } : forward
      )),
    }));
  }

  function addForward() {
    setDraft(current => ({
      ...current,
      forwards: [...current.forwards, createForward({
        listen_port: '8080',
        dest_port: '80',
      })],
    }));
  }

  function removeForward(id) {
    setDraft(current => ({
      ...current,
      forwards: current.forwards.filter(forward => forward.id !== id),
    }));
  }

  async function submit(event) {
    event.preventDefault();
    if (invalid || saving) return;
    setSubmitError('');
    setSaving(true);

    const config = {
      id: initial?.id || `sess-${Date.now()}`,
      name: draft.name.trim() || draft.host.trim(),
      host: draft.host.trim(),
      port: Number(draft.port),
      username: draft.username.trim(),
      group: draft.group.trim() || '默认分组',
      ai_allowed: draft.aiAllowed,
      ai_policy: draft.aiPolicy,
      ai_blocklist_regexes: regexLines(draft.aiBlocklist),
      auto_connect: draft.autoConnect,
      keepalive_secs: Number(draft.keepaliveSecs),
      connect_timeout_secs: Number(draft.connectTimeoutSecs),
      proxy_jump: [...draft.proxyJump],
      port_forwards: draft.forwards.map(forward => ({
        id: forward.id,
        kind: forward.kind,
        listen_host: forward.listen_host.trim(),
        listen_port: Number(forward.listen_port),
        dest_host: forward.dest_host.trim(),
        dest_port: Number(forward.dest_port),
        enabled: forward.enabled,
      })),
      log_enabled: draft.logEnabled,
    };

    if (!isEdit || !sameAuthType) {
      if (draft.authType === 'Password') {
        config.auth = { type: 'Password', password_ref: draft.password };
      } else if (draft.authType === 'Key') {
        config.auth = draft.keySource === 'inline'
          ? {
              type: 'Key',
              private_key_path: '',
              private_key_inline_ref: draft.privateKeyContent.trim(),
              passphrase_ref: draft.passphrase || null,
            }
          : {
              type: 'Key',
              private_key_path: draft.privateKeyPath.trim(),
              passphrase_ref: draft.passphrase || null,
            };
      } else {
        config.auth = { type: 'Agent' };
      }
    } else if (draft.authType === 'Password') {
      if (draft.secretAction === 'replace') {
        config.auth = { type: 'Password', password_ref: draft.password };
      } else if (draft.secretAction === 'clear') {
        config.clear_secret = true;
      }
    } else if (draft.authType === 'Key') {
      const originalPath = initial.auth.private_key_path || '';
      if (draft.keySource === 'inline') {
        const privateKeyContent = draft.privateKeyContent.trim();
        if (privateKeyContent || !initial.auth.has_inline_key || draft.secretAction === 'replace') {
          config.auth = {
            type: 'Key',
            private_key_path: '',
            private_key_inline_ref: privateKeyContent || null,
            passphrase_ref: draft.secretAction === 'replace' ? draft.passphrase : null,
          };
        }
        if (draft.secretAction === 'clear') config.clear_secret = true;
      } else {
        const privateKeyPath = draft.privateKeyPath.trim();
        const pathChanged = privateKeyPath !== originalPath;
        if (draft.secretAction === 'replace') {
          config.auth = {
            type: 'Key',
            private_key_path: privateKeyPath,
            passphrase_ref: draft.passphrase,
          };
        } else {
          if (pathChanged || initial.auth.has_inline_key) {
            config.auth = {
              type: 'Key',
              private_key_path: privateKeyPath,
              passphrase_ref: null,
            };
          }
          if (draft.secretAction === 'clear') config.clear_secret = true;
        }
      }
    }

    try {
      await onCreate(config);
    } catch (error) {
      setSubmitError(typeof error === 'string' ? error : error?.message || '会话保存失败');
    } finally {
      setSaving(false);
    }
  }

  const secretDisabled = sameAuthType && draft.secretAction !== 'replace';
  const secretLabel = draft.authType === 'Password' ? '密码' : '私钥口令（passphrase）';
  const hasSavedSecret = draft.authType === 'Password'
    ? initial?.auth?.has_password
    : initial?.auth?.has_passphrase;
  const secretPlaceholder = sameAuthType && hasSavedSecret
    ? '已保存，留空保持不变'
    : draft.authType === 'Password' ? '••••••' : '私钥口令（可选）';

  return (
    <div
      className="session-dialog__overlay"
      onMouseDown={event => { if (!saving && event.target === event.currentTarget) onCancel(); }}
    >
      <form
        className="session-dialog"
        onSubmit={submit}
        role="dialog"
        aria-modal="true"
        aria-labelledby="session-dialog-title"
      >
        <header className="session-dialog__header">
          <div>
            <h2 id="session-dialog-title">{isEdit ? '编辑会话' : '新建会话'}</h2>
            <p>配置 SSH 连接、认证、AI 安全策略和端口转发。</p>
          </div>
          <button type="button" className="session-dialog__close" onClick={onCancel} aria-label="关闭">×</button>
        </header>

        <div className="session-dialog__body">
          <section className="session-dialog__section">
            <h3>基本连接</h3>
            <div className="session-dialog__grid session-dialog__grid--two">
              <Field id="session-name" label="会话名称">
                <input id="session-name" value={draft.name} onChange={event => update('name', event.target.value)} placeholder="可留空，默认用主机名" />
              </Field>
              <Field id="session-group" label="分组">
                <input id="session-group" value={draft.group} onChange={event => update('group', event.target.value)} placeholder="默认分组" />
              </Field>
              <Field id="session-host" label="主机地址 *" error={errors.host}>
                <input id="session-host" value={draft.host} onChange={event => update('host', event.target.value)} placeholder="10.0.1.22" aria-invalid={Boolean(errors.host)} autoFocus />
              </Field>
              <Field id="session-port" label="端口" error={errors.port}>
                <input id="session-port" inputMode="numeric" value={draft.port} onChange={event => update('port', event.target.value)} placeholder="22" aria-invalid={Boolean(errors.port)} />
              </Field>
              <Field id="session-username" label="用户名 *" error={errors.username}>
                <input id="session-username" value={draft.username} onChange={event => update('username', event.target.value)} placeholder="root" aria-invalid={Boolean(errors.username)} />
              </Field>
              <Field id="session-auth-type" label="认证类型">
                <select id="session-auth-type" value={draft.authType} onChange={event => changeAuthType(event.target.value)}>
                  <option value="Password">密码（Password）</option>
                  <option value="Key">私钥（Key）</option>
                  <option value="Agent">SSH Agent</option>
                </select>
              </Field>
            </div>
          </section>

          <section className="session-dialog__section">
            <div className="session-dialog__section-heading">
              <h3>ProxyJump 跳板链</h3>
              <div className="session-dialog__jump-add">
                <select aria-label="选择跳板会话" value={jumpCandidate} onChange={event => setJumpCandidate(event.target.value)}>
                  <option value="">选择已保存会话…</option>
                  {availableSessions.map(session => (
                    <option key={session.id} value={session.id}>
                      {session.name || session.id}（{session.username}@{session.host}:{session.port}）
                    </option>
                  ))}
                </select>
                <button type="button" className="session-dialog__secondary-button" onClick={addProxyJump} disabled={!jumpCandidate || draft.proxyJump.length >= MAX_PROXY_JUMPS}>添加跳转</button>
              </div>
            </div>
            <p className="session-dialog__hint">按列表顺序经 direct-tcpip 连接目标；每跳使用其已保存认证和 TOFU，秘密不会显示或复制到目标会话。</p>
            {errors.proxyJump && <div className="session-dialog__error" role="alert">{errors.proxyJump}</div>}
            {draft.proxyJump.length === 0 && <p className="session-dialog__hint">未配置跳板，将直接连接目标。</p>}
            <ol className="session-dialog__jump-list">
              {draft.proxyJump.map((id, index) => {
                const session = availableSessions.find(item => item.id === id);
                return (
                  <li className="session-dialog__jump-item" key={`${id}-${index}`}>
                    <span className="session-dialog__jump-description">
                      <strong>跳转 {index + 1}：{session?.name || id}</strong>
                      <small>{session ? `${session.username}@${session.host}:${session.port}` : id}</small>
                    </span>
                    <button type="button" aria-label={`上移跳转 ${index + 1}`} onClick={() => moveProxyJump(index, -1)} disabled={index === 0}>↑</button>
                    <button type="button" aria-label={`下移跳转 ${index + 1}`} onClick={() => moveProxyJump(index, 1)} disabled={index === draft.proxyJump.length - 1}>↓</button>
                    <button type="button" className="session-dialog__jump-remove" aria-label={`删除跳转 ${index + 1}`} onClick={() => removeProxyJump(index)}>删除</button>
                  </li>
                );
              })}
            </ol>
          </section>

          <section className="session-dialog__section">
            <h3>认证</h3>
            {sameAuthType && draft.authType !== 'Agent' && (
              <fieldset className="session-dialog__secret-actions">
                <legend>{draft.authType === 'Password' ? '密码处理' : '私钥口令处理'}</legend>
                <label><input type="radio" name="secret-action" value="keep" checked={draft.secretAction === 'keep'} onChange={() => changeSecretAction('keep')} />保留（Keep）</label>
                <label><input type="radio" name="secret-action" value="replace" checked={draft.secretAction === 'replace'} onChange={() => changeSecretAction('replace')} />替换（Replace）</label>
                <label><input type="radio" name="secret-action" value="clear" checked={draft.secretAction === 'clear'} onChange={() => changeSecretAction('clear')} />清除（Clear）</label>
              </fieldset>
            )}

            {draft.authType === 'Key' && (
              <>
                <fieldset className="session-dialog__secret-actions">
                  <legend>私钥来源</legend>
                  <label><input type="radio" name="key-source" value="path" checked={draft.keySource === 'path'} onChange={() => update('keySource', 'path')} />使用私钥文件</label>
                  <label><input type="radio" name="key-source" value="inline" checked={draft.keySource === 'inline'} onChange={() => update('keySource', 'inline')} />直接粘贴私钥内容</label>
                </fieldset>
                {draft.keySource === 'path' ? (
                  <Field id="session-private-key" label="私钥路径 *" error={errors.privateKeyPath}>
                    <input id="session-private-key" value={draft.privateKeyPath} onChange={event => update('privateKeyPath', event.target.value)} placeholder="C:\\Users\\me\\.ssh\\id_ed25519 或 /home/me/.ssh/id_ed25519" aria-invalid={Boolean(errors.privateKeyPath)} spellCheck={false} />
                  </Field>
                ) : (
                  <Field id="session-private-key-content" label="私钥内容 *" error={errors.privateKeyContent}>
                    <textarea
                      id="session-private-key-content"
                      rows={6}
                      value={draft.privateKeyContent}
                      onChange={event => update('privateKeyContent', event.target.value)}
                      placeholder={sameAuthType && initial?.auth?.has_inline_key ? '已保存私钥，留空保持不变；粘贴新内容可替换' : '-----BEGIN OPENSSH PRIVATE KEY-----'}
                      aria-invalid={Boolean(errors.privateKeyContent)}
                      spellCheck={false}
                    />
                  </Field>
                )}
              </>
            )}

            {draft.authType !== 'Agent' && (
              <Field id="session-secret" label={secretLabel} error={errors.password || errors.passphrase}>
                <input
                  id="session-secret"
                  type="password"
                  value={draft.authType === 'Password' ? draft.password : draft.passphrase}
                  onChange={event => update(draft.authType === 'Password' ? 'password' : 'passphrase', event.target.value)}
                  placeholder={secretPlaceholder}
                  disabled={secretDisabled}
                  aria-invalid={Boolean(errors.password || errors.passphrase)}
                  autoComplete="new-password"
                />
              </Field>
            )}
            {draft.authType === 'Agent' && <p className="session-dialog__hint">使用本机 SSH Agent 中已加载的密钥，无需填写秘密。</p>}
            {sameAuthType && draft.secretAction === 'keep' && <p className="session-dialog__hint">秘密留空并保留后端已有值，不发送认证秘密补丁。</p>}
            {sameAuthType && draft.secretAction === 'clear' && <p className="session-dialog__hint session-dialog__hint--warning">保存后会明确清除后端已保存的{draft.authType === 'Password' ? '密码' : '私钥口令'}。</p>}
          </section>

          <section className="session-dialog__section">
            <h3>连接与日志</h3>
            <div className="session-dialog__grid session-dialog__grid--two">
              <Field id="session-keepalive" label="Keepalive（秒）" error={errors.keepalive}>
                <input id="session-keepalive" inputMode="numeric" value={draft.keepaliveSecs} onChange={event => update('keepaliveSecs', event.target.value)} aria-invalid={Boolean(errors.keepalive)} />
              </Field>
              <Field id="session-connect-timeout" label="连接超时（秒）" error={errors.connectTimeout}>
                <input id="session-connect-timeout" inputMode="numeric" value={draft.connectTimeoutSecs} onChange={event => update('connectTimeoutSecs', event.target.value)} aria-invalid={Boolean(errors.connectTimeout)} />
              </Field>
            </div>
            <div className="session-dialog__checks">
              <label><input type="checkbox" checked={draft.autoConnect} onChange={event => update('autoConnect', event.target.checked)} />{isEdit ? '自动连接' : '创建后立即连接'}</label>
              <label><input type="checkbox" checked={draft.logEnabled} onChange={event => update('logEnabled', event.target.checked)} />启用终端日志</label>
            </div>
          </section>

          <section className="session-dialog__section">
            <h3>AI 安全</h3>
            <div className="session-dialog__grid session-dialog__grid--two">
              <Field id="session-ai-policy" label="AI 策略">
                <select id="session-ai-policy" value={draft.aiPolicy} onChange={event => update('aiPolicy', event.target.value)}>
                  <option value="Inherit">继承全局策略（Inherit）</option>
                  <option value="Strict">严格（Strict）</option>
                  <option value="Recommended">推荐（Recommended）</option>
                  <option value="Permissive">宽松（Permissive）</option>
                </select>
              </Field>
              <div className="session-dialog__field session-dialog__field--check">
                <label><input type="checkbox" checked={draft.aiAllowed} onChange={event => update('aiAllowed', event.target.checked)} />允许 AI 访问</label>
              </div>
              <Field id="session-ai-blocklist" label="AI 黑名单（每行一个正则）" error={errors.aiBlocklist} className="session-dialog__field--wide">
                <textarea id="session-ai-blocklist" rows={4} value={draft.aiBlocklist} onChange={event => update('aiBlocklist', event.target.value)} placeholder="^rm\s+-rf\s+/" aria-invalid={Boolean(errors.aiBlocklist)} spellCheck={false} />
              </Field>
            </div>
          </section>

          <section className="session-dialog__section">
            <div className="session-dialog__section-heading">
              <h3>端口转发</h3>
              <button type="button" className="session-dialog__secondary-button" onClick={addForward}>添加转发</button>
            </div>
            {draft.forwards.length === 0 && <p className="session-dialog__hint">尚未配置转发规则。</p>}
            {draft.forwards.map((forward, index) => {
              const prefix = `forward-${forward.id}`;
              return (
                <div className="session-dialog__forward" key={forward.id}>
                  <div className="session-dialog__forward-heading">
                    <strong>转发 {index + 1}</strong>
                    <label><input type="checkbox" aria-label={`转发 ${index + 1} 启用`} checked={forward.enabled} onChange={event => updateForward(forward.id, 'enabled', event.target.checked)} />启用</label>
                    <button type="button" onClick={() => removeForward(forward.id)} aria-label={`删除转发 ${index + 1}`}>删除</button>
                  </div>
                  <div className="session-dialog__forward-grid">
                    <Field id={`${prefix}-kind`} label="类型">
                      <select id={`${prefix}-kind`} aria-label={`转发 ${index + 1} 类型`} value={forward.kind} onChange={event => updateForward(forward.id, 'kind', event.target.value)}>
                        <option value="local">Local</option>
                        <option value="remote">Remote</option>
                      </select>
                    </Field>
                    <Field id={`${prefix}-listen-host`} label="监听地址" error={errors[`${prefix}-listen-host`]}>
                      <input id={`${prefix}-listen-host`} aria-label={`转发 ${index + 1} 监听地址`} value={forward.listen_host} onChange={event => updateForward(forward.id, 'listen_host', event.target.value)} aria-invalid={Boolean(errors[`${prefix}-listen-host`])} />
                    </Field>
                    <Field id={`${prefix}-listen-port`} label="监听端口" error={errors[`${prefix}-listen-port`]}>
                      <input id={`${prefix}-listen-port`} aria-label={`转发 ${index + 1} 监听端口`} inputMode="numeric" value={forward.listen_port} onChange={event => updateForward(forward.id, 'listen_port', event.target.value)} aria-invalid={Boolean(errors[`${prefix}-listen-port`])} />
                    </Field>
                    <Field id={`${prefix}-dest-host`} label="目标地址" error={errors[`${prefix}-dest-host`]}>
                      <input id={`${prefix}-dest-host`} aria-label={`转发 ${index + 1} 目标地址`} value={forward.dest_host} onChange={event => updateForward(forward.id, 'dest_host', event.target.value)} aria-invalid={Boolean(errors[`${prefix}-dest-host`])} />
                    </Field>
                    <Field id={`${prefix}-dest-port`} label="目标端口" error={errors[`${prefix}-dest-port`]}>
                      <input id={`${prefix}-dest-port`} aria-label={`转发 ${index + 1} 目标端口`} inputMode="numeric" value={forward.dest_port} onChange={event => updateForward(forward.id, 'dest_port', event.target.value)} aria-invalid={Boolean(errors[`${prefix}-dest-port`])} />
                    </Field>
                  </div>
                </div>
              );
            })}
          </section>
        </div>

        <footer className="session-dialog__footer">
          {submitError && <div className="session-dialog__error" role="alert">{submitError}</div>}
          <button type="button" className="session-dialog__secondary-button" onClick={onCancel} disabled={saving}>取消</button>
          <button type="submit" className="session-dialog__primary-button" disabled={invalid || saving}>
            {saving ? '正在保存…' : isEdit ? '保存修改' : draft.autoConnect ? '创建并连接' : '创建会话'}
          </button>
        </footer>
      </form>
    </div>
  );
}
