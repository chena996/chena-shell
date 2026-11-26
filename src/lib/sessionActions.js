import { invoke } from '@tauri-apps/api/core';

/** 打开 Chena-Sftp 窗口；未连接时先自动连接。 */
export async function openSftpWindow(session) {
  if (!session) return;
  if (session.connected) {
    invoke('open_sftp_window', { sessionId: session.id }).catch(error => alert(`Chena-Sftp 打开失败：${error}`));
    return;
  }
  try {
    await invoke('connect_session', { sessionId: session.id });
    await new Promise(resolve => setTimeout(resolve, 300));
    invoke('open_sftp_window', { sessionId: session.id }).catch(error => alert(`Chena-Sftp 打开失败：${error}`));
  } catch (error) {
    alert(`Chena-Sftp 打开失败：先连接 SSH 会话\n\n${error}`);
  }
}

/** 根据鼠标位置计算不越界的菜单坐标。 */
export function contextMenuPosition(event, width = 200, height = 320) {
  let x = event.clientX;
  let y = event.clientY;
  if (x + width > window.innerWidth) x = window.innerWidth - width - 8;
  if (y + height > window.innerHeight) y = window.innerHeight - height - 8;
  return { x: Math.max(4, x), y: Math.max(4, y) };
}
