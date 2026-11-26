import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import NewSessionDialog, {
  MAX_PROXY_JUMPS,
  proxyJumpValidationError,
  resolveProxyJumpChain,
} from './NewSessionDialog.jsx';

const initial = {
  id: 'sess-1',
  name: '生产机',
  host: '10.0.0.8',
  port: 22,
  username: 'root',
  auth: { type: 'Password', has_password: true, has_passphrase: false },
  group: '默认分组',
  ai_allowed: true,
  ai_policy: 'Recommended',
  ai_blocklist_regexes: ['^rm\\s+-rf'],
  auto_connect: false,
  keepalive_secs: 45,
  connect_timeout_secs: 20,
  proxy_jump: [],
  port_forwards: [],
  log_enabled: true,
};

const keyInitial = {
  ...initial,
  auth: {
    type: 'Key',
    private_key_path: '/home/me/.ssh/id_ed25519',
    has_password: false,
    has_passphrase: true,
  },
};

function setup(editInitial, sessions = []) {
  const user = userEvent.setup();
  const onCreate = vi.fn();
  const onCancel = vi.fn();
  render(<NewSessionDialog initial={editInitial} sessions={sessions} onCancel={onCancel} onCreate={onCreate} />);
  return { user, onCreate, onCancel };
}

function lastConfig(onCreate) {
  return onCreate.mock.calls.at(-1)[0];
}

describe('NewSessionDialog 新建认证', () => {
  it('新建 Password 会话要求密码，提交完整 SessionConfig，关闭自动连接时使用创建会话文案', async () => {
    const { user, onCreate } = setup();
    expect(screen.getByRole('button', { name: '创建并连接' })).toBeDisabled();

    await user.type(screen.getByPlaceholderText('10.0.1.22'), '10.0.0.9');
    await user.type(screen.getByLabelText('密码'), 'new-secret');
    await user.click(screen.getByLabelText('创建后立即连接'));
    const submit = screen.getByRole('button', { name: '创建会话' });
    expect(screen.queryByRole('button', { name: '创建并连接' })).not.toBeInTheDocument();
    await user.click(submit);

    expect(onCreate).toHaveBeenCalledOnce();
    const config = lastConfig(onCreate);
    expect(config).toEqual({
      id: expect.stringMatching(/^sess-/),
      name: '10.0.0.9',
      host: '10.0.0.9',
      port: 22,
      username: 'root',
      auth: { type: 'Password', password_ref: 'new-secret' },
      group: '默认分组',
      ai_allowed: true,
      ai_policy: 'Inherit',
      ai_blocklist_regexes: [],
      auto_connect: false,
      keepalive_secs: 30,
      connect_timeout_secs: 30,
      proxy_jump: [],
      port_forwards: [],
      log_enabled: true,
    });
    expect(config).not.toHaveProperty('clear_secret');
  });

  it('新建 Key 会话要求私钥路径并提交可选 passphrase', async () => {
    const { user, onCreate } = setup();
    await user.selectOptions(screen.getByLabelText('认证类型'), 'Key');
    expect(screen.getByRole('button', { name: '创建并连接' })).toBeDisabled();

    await user.type(screen.getByPlaceholderText('10.0.1.22'), 'key.example.com');
    await user.type(screen.getByLabelText('私钥路径 *'), '/home/me/.ssh/new_key');
    await user.type(screen.getByLabelText('私钥口令（passphrase）'), 'key-passphrase');
    await user.click(screen.getByRole('button', { name: '创建并连接' }));

    expect(lastConfig(onCreate).auth).toEqual({
      type: 'Key',
      private_key_path: '/home/me/.ssh/new_key',
      passphrase_ref: 'key-passphrase',
    });
  });

  it('新建 Key 会话可直接粘贴私钥内容，无需选择文件', async () => {
    const { user, onCreate } = setup();
    await user.selectOptions(screen.getByLabelText('认证类型'), 'Key');
    await user.type(screen.getByPlaceholderText('10.0.1.22'), 'inline.example.com');
    await user.click(screen.getByLabelText('直接粘贴私钥内容'));
    expect(screen.queryByLabelText('私钥路径 *')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: '创建并连接' })).toBeDisabled();

    const content = '-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----';
    fireEvent.change(screen.getByLabelText('私钥内容 *'), { target: { value: content } });
    await user.click(screen.getByRole('button', { name: '创建并连接' }));

    expect(lastConfig(onCreate).auth).toEqual({
      type: 'Key',
      private_key_path: '',
      private_key_inline_ref: content,
      passphrase_ref: null,
    });
  });

  it('新建 Agent 会话无需密码、私钥路径或口令', async () => {
    const { user, onCreate } = setup();
    await user.selectOptions(screen.getByLabelText('认证类型'), 'Agent');
    expect(screen.queryByLabelText('密码')).not.toBeInTheDocument();
    expect(screen.queryByLabelText('私钥路径 *')).not.toBeInTheDocument();

    await user.type(screen.getByPlaceholderText('10.0.1.22'), 'agent.example.com');
    await user.click(screen.getByRole('button', { name: '创建并连接' }));
    expect(lastConfig(onCreate).auth).toEqual({ type: 'Agent' });
  });
});

describe('NewSessionDialog 秘密 Keep / Replace / Clear', () => {
  it('Password Keep 留空且省略认证补丁和 clear_secret，同时保留已有配置字段', async () => {
    const { user, onCreate } = setup(initial);
    expect(screen.getByPlaceholderText('已保存，留空保持不变')).toBeDisabled();
    await user.click(screen.getByRole('button', { name: '保存修改' }));

    const patch = lastConfig(onCreate);
    expect(patch).not.toHaveProperty('auth');
    expect(patch).not.toHaveProperty('clear_secret');
    expect(patch).toEqual(expect.objectContaining({
      id: initial.id,
      name: initial.name,
      host: initial.host,
      port: initial.port,
      username: initial.username,
      group: initial.group,
      ai_allowed: initial.ai_allowed,
      ai_policy: initial.ai_policy,
      ai_blocklist_regexes: initial.ai_blocklist_regexes,
      auto_connect: initial.auto_connect,
      keepalive_secs: initial.keepalive_secs,
      connect_timeout_secs: initial.connect_timeout_secs,
      log_enabled: initial.log_enabled,
    }));
  });

  it('Password Replace 只有输入新密码后才提交替换补丁', async () => {
    const { user, onCreate } = setup(initial);
    await user.click(screen.getByLabelText('替换（Replace）'));
    const password = screen.getByLabelText('密码');
    expect(password).toBeEnabled();
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();

    await user.type(password, 'new-secret');
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({ type: 'Password', password_ref: 'new-secret' });
    expect(lastConfig(onCreate)).not.toHaveProperty('clear_secret');
  });

  it('Password Clear 只发送明确清除标记', async () => {
    const { user, onCreate } = setup(initial);
    await user.click(screen.getByLabelText('清除（Clear）'));
    expect(screen.getByLabelText('密码')).toBeDisabled();
    await user.click(screen.getByRole('button', { name: '保存修改' }));

    expect(lastConfig(onCreate)).not.toHaveProperty('auth');
    expect(lastConfig(onCreate).clear_secret).toBe(true);
  });

  it('Key Keep 不发送无用认证补丁，修改路径时仅发送必要路径补丁并保留口令', async () => {
    const { user, onCreate } = setup(keyInitial);
    expect(screen.getByLabelText('私钥口令（passphrase）')).toBeDisabled();
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate)).not.toHaveProperty('auth');
    expect(lastConfig(onCreate)).not.toHaveProperty('clear_secret');

    fireEvent.change(screen.getByLabelText('私钥路径 *'), { target: { value: '/home/me/.ssh/other_key' } });
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({
      type: 'Key',
      private_key_path: '/home/me/.ssh/other_key',
      passphrase_ref: null,
    });
    expect(lastConfig(onCreate)).not.toHaveProperty('clear_secret');
  });

  it('Key Replace 替换 passphrase，Clear 明确清除口令且保留私钥路径', async () => {
    const { user, onCreate } = setup(keyInitial);
    await user.click(screen.getByLabelText('替换（Replace）'));
    await user.type(screen.getByLabelText('私钥口令（passphrase）'), 'new-passphrase');
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({
      type: 'Key',
      private_key_path: keyInitial.auth.private_key_path,
      passphrase_ref: 'new-passphrase',
    });

    await user.click(screen.getByLabelText('清除（Clear）'));
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate)).not.toHaveProperty('auth');
    expect(lastConfig(onCreate).clear_secret).toBe(true);
  });

  it('切认证类型时重新校验必填秘密，并可切换到无需秘密的 Agent', async () => {
    const { user, onCreate } = setup(initial);
    await user.selectOptions(screen.getByLabelText('认证类型'), 'Key');
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
    expect(screen.queryByLabelText('保留（Keep）')).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('私钥路径 *'), { target: { value: '/new/key' } });
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({
      type: 'Key',
      private_key_path: '/new/key',
      passphrase_ref: null,
    });

    await user.selectOptions(screen.getByLabelText('认证类型'), 'Agent');
    expect(screen.getByRole('button', { name: '保存修改' })).toBeEnabled();
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({ type: 'Agent' });
    expect(lastConfig(onCreate)).not.toHaveProperty('clear_secret');
  });

  it('从 Key 切到 Password 时必须填写新密码', async () => {
    const { user, onCreate } = setup(keyInitial);
    await user.selectOptions(screen.getByLabelText('认证类型'), 'Password');
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
    await user.type(screen.getByLabelText('密码'), 'replacement-password');
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).auth).toEqual({ type: 'Password', password_ref: 'replacement-password' });
  });
});

describe('NewSessionDialog 连接、AI 与转发', () => {
  it('提交全部连接和 AI 字段，并逐项校验端口、keepalive、超时及逐行正则', async () => {
    const { user, onCreate } = setup(initial);
    fireEvent.change(screen.getByLabelText('会话名称'), { target: { value: '新名称' } });
    fireEvent.change(screen.getByLabelText('主机地址 *'), { target: { value: 'new.example.com' } });
    fireEvent.change(screen.getByLabelText('端口'), { target: { value: '2222' } });
    fireEvent.change(screen.getByLabelText('用户名 *'), { target: { value: 'deploy' } });
    fireEvent.change(screen.getByLabelText('分组'), { target: { value: '后端' } });
    fireEvent.change(screen.getByLabelText('Keepalive（秒）'), { target: { value: '0' } });
    fireEvent.change(screen.getByLabelText('连接超时（秒）'), { target: { value: '120' } });
    await user.click(screen.getByLabelText('自动连接'));
    await user.click(screen.getByLabelText('启用终端日志'));
    await user.click(screen.getByLabelText('允许 AI 访问'));
    await user.selectOptions(screen.getByLabelText('AI 策略'), 'Strict');
    fireEvent.change(screen.getByLabelText('AI 黑名单（每行一个正则）'), {
      target: { value: '(?i)^rm\\s+-rf\n\n^shutdown' },
    });
    await user.click(screen.getByRole('button', { name: '保存修改' }));

    expect(lastConfig(onCreate)).toEqual(expect.objectContaining({
      name: '新名称',
      host: 'new.example.com',
      port: 2222,
      username: 'deploy',
      group: '后端',
      auto_connect: true,
      keepalive_secs: 0,
      connect_timeout_secs: 120,
      log_enabled: false,
      ai_allowed: false,
      ai_policy: 'Strict',
      ai_blocklist_regexes: ['(?i)^rm\\s+-rf', '^shutdown'],
    }));

    fireEvent.change(screen.getByLabelText('端口'), { target: { value: '65536' } });
    fireEvent.change(screen.getByLabelText('Keepalive（秒）'), { target: { value: '-1' } });
    fireEvent.change(screen.getByLabelText('连接超时（秒）'), { target: { value: '0' } });
    fireEvent.change(screen.getByLabelText('AI 黑名单（每行一个正则）'), {
      target: { value: '(?i)^valid$\n[invalid' },
    });
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
    expect(screen.getByText('端口必须是 1–65535 之间的整数')).toBeInTheDocument();
    expect(screen.getByText('Keepalive 必须是 0–3600 秒之间的整数（0 表示关闭）')).toBeInTheDocument();
    expect(screen.getByText('连接超时必须是 1–300 秒之间的整数')).toBeInTheDocument();
    expect(screen.getByText('AI 黑名单第 2 行不是有效正则表达式')).toBeInTheDocument();
    expect(onCreate).toHaveBeenCalledOnce();
  });

  it('支持 Local / Remote 转发的新增、编辑、启停、删除和端口校验', async () => {
    const forward = {
      id: 'fwd-existing',
      kind: 'local',
      listen_host: '127.0.0.1',
      listen_port: 8080,
      dest_host: 'db.internal',
      dest_port: 5432,
      enabled: true,
    };
    const { user, onCreate } = setup({ ...initial, port_forwards: [forward] });

    await user.selectOptions(screen.getByLabelText('转发 1 类型'), 'remote');
    fireEvent.change(screen.getByLabelText('转发 1 监听地址'), { target: { value: '0.0.0.0' } });
    fireEvent.change(screen.getByLabelText('转发 1 监听端口'), { target: { value: '9000' } });
    fireEvent.change(screen.getByLabelText('转发 1 目标地址'), { target: { value: 'localhost' } });
    fireEvent.change(screen.getByLabelText('转发 1 目标端口'), { target: { value: '5433' } });
    await user.click(screen.getByLabelText('转发 1 启用'));
    await user.click(screen.getByRole('button', { name: '添加转发' }));
    expect(screen.getByLabelText('转发 2 监听端口')).toHaveValue('8080');

    fireEvent.change(screen.getByLabelText('转发 2 监听端口'), { target: { value: '70000' } });
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
    fireEvent.change(screen.getByLabelText('转发 2 监听端口'), { target: { value: '8081' } });
    await user.click(screen.getByRole('button', { name: '保存修改' }));

    expect(lastConfig(onCreate).port_forwards).toHaveLength(2);
    expect(lastConfig(onCreate).port_forwards[0]).toEqual({
      id: 'fwd-existing',
      kind: 'remote',
      listen_host: '0.0.0.0',
      listen_port: 9000,
      dest_host: 'localhost',
      dest_port: 5433,
      enabled: false,
    });
    expect(lastConfig(onCreate).port_forwards[1]).toEqual(expect.objectContaining({
      kind: 'local',
      listen_host: '127.0.0.1',
      listen_port: 8081,
      dest_host: '127.0.0.1',
      dest_port: 80,
      enabled: true,
    }));

    await user.click(screen.getByRole('button', { name: '删除转发 1' }));
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).port_forwards).toHaveLength(1);
    expect(lastConfig(onCreate).port_forwards[0].id).not.toBe('fwd-existing');
  });
});

describe('ProxyJump 纯函数与表单', () => {
  const jumpSessions = [
    {
      id: 'jump-a', name: '跳板 A', username: 'admin', host: '10.0.0.1', port: 22,
      proxy_jump: [], auth: { type: 'Password', password_ref: 'must-not-render', has_password: true },
    },
    {
      id: 'jump-b', name: '跳板 B', username: 'ubuntu', host: '10.0.0.2', port: 2222,
      proxy_jump: [],
    },
  ];

  it('按声明顺序解析并先展开跳板自身链路，同时拒绝循环和超过 5 跳', () => {
    const sessions = [
      { id: 'a', proxy_jump: [] },
      { id: 'b', proxy_jump: ['a'] },
      { id: 'c', proxy_jump: [] },
    ];
    expect(resolveProxyJumpChain('target', ['b', 'c'], sessions)).toEqual(['a', 'b', 'c']);
    expect(proxyJumpValidationError('target', ['target'], sessions)).toContain('自引用');
    expect(proxyJumpValidationError('target', ['b'], [
      { id: 'a', proxy_jump: ['target'] },
      { id: 'b', proxy_jump: ['a'] },
    ])).toContain('循环引用');
    expect(proxyJumpValidationError(
      'target',
      Array.from({ length: MAX_PROXY_JUMPS + 1 }, (_, index) => `jump-${index}`),
      [],
    )).toContain('最多');
    const nested = Array.from({ length: MAX_PROXY_JUMPS + 1 }, (_, index) => ({
      id: `jump-${index}`,
      proxy_jump: index < MAX_PROXY_JUMPS ? [`jump-${index + 1}`] : [],
    }));
    expect(proxyJumpValidationError('target', ['jump-0'], nested)).toContain('最多');
  });

  it('可选择、上下排序、删除跳板并按顺序提交，且不回显或复制跳板秘密', async () => {
    const { user, onCreate } = setup(initial, jumpSessions);
    await user.selectOptions(screen.getByLabelText('选择跳板会话'), 'jump-a');
    await user.click(screen.getByRole('button', { name: '添加跳转' }));
    await user.selectOptions(screen.getByLabelText('选择跳板会话'), 'jump-b');
    await user.click(screen.getByRole('button', { name: '添加跳转' }));
    expect(screen.getByText('跳转 1：跳板 A')).toBeInTheDocument();
    expect(screen.getByText('跳转 2：跳板 B')).toBeInTheDocument();
    expect(document.body.textContent).not.toContain('must-not-render');

    await user.click(screen.getByRole('button', { name: '下移跳转 1' }));
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).proxy_jump).toEqual(['jump-b', 'jump-a']);
    expect(JSON.stringify(lastConfig(onCreate))).not.toContain('must-not-render');

    await user.click(screen.getByRole('button', { name: '删除跳转 1' }));
    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(lastConfig(onCreate).proxy_jump).toEqual(['jump-a']);
  });

  it('表单检测自引用，删除后可恢复提交', async () => {
    const { user } = setup({ ...initial, proxy_jump: [initial.id] }, [initial, ...jumpSessions]);
    expect(screen.getByText('不能将当前会话作为跳板（自引用）')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: '删除跳转 1' }));
    expect(screen.getByRole('button', { name: '保存修改' })).toBeEnabled();
  });

  it('表单检测经已保存会话形成的间接循环', () => {
    const cyclic = [
      { ...jumpSessions[0], proxy_jump: ['jump-b'] },
      { ...jumpSessions[1], proxy_jump: [initial.id] },
    ];
    setup({ ...initial, proxy_jump: ['jump-a'] }, cyclic);
    expect(screen.getByText(/ProxyJump 存在循环引用/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '保存修改' })).toBeDisabled();
  });
});

describe('NewSessionDialog 异步保存', () => {
  it('保存期间禁止重复提交和取消', async () => {
    const user = userEvent.setup();
    let finishSave;
    const onCreate = vi.fn(() => new Promise(resolve => { finishSave = resolve; }));
    const onCancel = vi.fn();
    render(<NewSessionDialog initial={initial} sessions={[]} onCancel={onCancel} onCreate={onCreate} />);

    await user.click(screen.getByRole('button', { name: '保存修改' }));
    const saving = screen.getByRole('button', { name: '正在保存…' });
    expect(saving).toBeDisabled();
    expect(screen.getByRole('button', { name: '取消' })).toBeDisabled();
    await user.click(saving);
    expect(onCreate).toHaveBeenCalledOnce();
    expect(onCancel).not.toHaveBeenCalled();

    finishSave();
    await waitFor(() => expect(screen.getByRole('button', { name: '保存修改' })).toBeEnabled());
  });

  it('保存失败保留草稿并显示后端错误', async () => {
    const user = userEvent.setup();
    const onCreate = vi.fn().mockRejectedValue(new Error('配置目录为只读'));
    render(<NewSessionDialog initial={initial} sessions={[]} onCancel={vi.fn()} onCreate={onCreate} />);

    await user.click(screen.getByRole('button', { name: '保存修改' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('配置目录为只读');
    expect(screen.getByDisplayValue('生产机')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '保存修改' })).toBeEnabled();
  });
});
