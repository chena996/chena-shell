import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import HostKeyDialog, { parseHostKeyChallenge } from './HostKeyDialog.jsx';

const challenge = {
  host: '10.0.0.8',
  port: 22,
  algorithm: 'ssh-ed25519',
  public_key: 'AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G2sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X',
  fingerprint: 'SHA256:new-key',
  previous_fingerprint: 'SHA256:old-key',
};

describe('主机密钥变化确认', () => {
  it('只解析带专用前缀且字段完整的错误', () => {
    expect(parseHostKeyChallenge(`HOST_KEY_CHANGED:${JSON.stringify(challenge)}`)).toEqual(challenge);
    expect(parseHostKeyChallenge('普通连接失败')).toBeNull();
    expect(parseHostKeyChallenge('HOST_KEY_CHANGED:{bad json')).toBeNull();
    expect(parseHostKeyChallenge('HOST_KEY_CHANGED:{}')).toBeNull();
  });

  it('展示新旧指纹，取消不会触发信任操作', async () => {
    const user = userEvent.setup();
    const onTrust = vi.fn();
    const onCancel = vi.fn();
    render(<HostKeyDialog challenge={challenge} onTrust={onTrust} onCancel={onCancel} />);

    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByText('SHA256:old-key')).toBeInTheDocument();
    expect(screen.getByText('SHA256:new-key')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '取消连接' }));
    expect(onCancel).toHaveBeenCalledOnce();
    expect(onTrust).not.toHaveBeenCalled();
  });

  it('明确确认后才提交完整质询', async () => {
    const user = userEvent.setup();
    const onTrust = vi.fn();
    render(<HostKeyDialog challenge={challenge} onTrust={onTrust} onCancel={vi.fn()} />);
    await user.click(screen.getByRole('button', { name: '信任新密钥并连接' }));
    expect(onTrust).toHaveBeenCalledWith(challenge);
  });
});
