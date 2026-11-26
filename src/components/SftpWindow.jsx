import { useState, useEffect, useRef, useCallback, Fragment } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './SftpWindow.css';

function applyTheme(theme) {
  if (theme === 'dark' || theme === 'light') document.documentElement.dataset.theme = theme;
}

try {
  applyTheme(localStorage.getItem('chena-shell-theme') || 'dark');
} catch {}

const DRAG_MIME = 'application/x-chena-sftp-entry';
const FINAL_STATUSES = new Set(['done', 'error']);
const EDIT_CONFLICT_PREFIX = 'SFTP_EDIT_CONFLICT:';

export function transferPercent(job) {
  return job.size ? ((job.bytes_transferred || 0) / job.size * 100) : (job.status === 'done' ? 100 : 50);
}

export function transferStatusLabel(job) {
  if (job.status === 'done') return '✓';
  if (job.status === 'error') return '✕';
  if (job.status === 'paused') return '暂停';
  return job.size ? `${Math.round((job.bytes_transferred || 0) / job.size * 100)}%` : '…';
}

function mergeTransferJob(existing, incoming, allowRestart = false) {
  if (existing && FINAL_STATUSES.has(existing.status) && incoming.status
    && incoming.status !== existing.status && !allowRestart) return existing;
  const patch = Object.fromEntries(Object.entries(incoming).filter(([, value]) => value !== undefined));
  return { ...existing, ...patch };
}

function mergeTransferSnapshot(snapshot, current) {
  const currentById = new Map(current.map(job => [job.id, job]));
  const merged = snapshot.map(job => {
    const newer = currentById.get(job.id);
    currentById.delete(job.id);
    return newer ? mergeTransferJob(job, newer, true) : job;
  });
  return [...merged, ...currentById.values()];
}

function normalizeLocalEntry(entry) {
  if (Array.isArray(entry)) {
    return {
      name: entry[0],
      is_dir: !!entry[1],
      size: Number(entry[2]) || 0,
      mtime: Number(entry[3]) || 0,
      hidden: entry.length > 4 ? !!entry[4] : entry[0].startsWith('.'),
      perms: '',
    };
  }
  return {
    name: entry.name,
    is_dir: !!entry.is_dir,
    size: Number(entry.size) || 0,
    mtime: Number(entry.mtime ?? entry.modified) || 0,
    hidden: entry.hidden ?? entry.name.startsWith('.'),
    perms: entry.perms || '',
  };
}

function isEntryHidden(entry) {
  return entry.hidden ?? entry.name.startsWith('.');
}

function visibleEntries(entries, sort, showHidden) {
  const direction = sort.direction === 'asc' ? 1 : -1;
  return entries
    .filter(entry => showHidden || !isEntryHidden(entry))
    .sort((a, b) => {
      if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
      let result = 0;
      if (sort.key === 'name') result = a.name.localeCompare(b.name, 'zh-CN', { numeric: true, sensitivity: 'base' });
      if (sort.key === 'size') result = (a.size || 0) - (b.size || 0);
      if (sort.key === 'mtime') result = (a.mtime || 0) - (b.mtime || 0);
      if (result === 0) result = a.name.localeCompare(b.name, 'zh-CN', { numeric: true, sensitivity: 'base' });
      return result * direction;
    });
}

function joinRemotePath(base, name) {
  if (!base || base === '.') return name;
  if (base === '/') return `/${name}`;
  return `${base.replace(/\/+$/, '')}/${name}`;
}

function joinLocalPath(base, name) {
  if (!base || base === '.') return name;
  if (/[\\/]$/.test(base)) return `${base}${name}`;
  const separator = base.includes('/') && !base.includes('\\') ? '/' : '\\';
  return `${base}${separator}${name}`;
}

function remoteParent(path) {
  if (!path || path === '.' || path === '/') return path || '.';
  const trimmed = path.replace(/\/+$/, '');
  const index = trimmed.lastIndexOf('/');
  if (index < 0) return '.';
  return index === 0 ? '/' : trimmed.slice(0, index);
}

function localParent(path) {
  if (!path || path === '.') return '.';
  const trimmed = path.replace(/[\\/]+$/, '');
  if (/^[A-Za-z]:$/.test(trimmed)) return `${trimmed}\\`;
  const index = Math.max(trimmed.lastIndexOf('\\'), trimmed.lastIndexOf('/'));
  if (index < 0) return '.';
  if (/^[A-Za-z]:/.test(trimmed) && index === 2) return trimmed.slice(0, 3);
  return trimmed.slice(0, index) || '.';
}

function breadcrumbs(path, isRemote) {
  if (!path || path === '.') return [{ label: '.', path: '.' }];
  if (isRemote) {
    if (path === '/') return [{ label: '/', path: '/' }];
    const absolute = path.startsWith('/');
    const parts = path.split('/').filter(Boolean);
    const result = absolute ? [{ label: '/', path: '/' }] : [];
    let current = absolute ? '/' : '';
    for (const part of parts) {
      current = joinRemotePath(current, part);
      result.push({ label: part, path: current });
    }
    return result;
  }
  const normalized = path.replace(/\//g, '\\');
  const drive = normalized.match(/^[A-Za-z]:/);
  const unc = normalized.startsWith('\\\\');
  const root = drive ? `${drive[0]}\\` : (unc ? '\\\\' : (normalized.startsWith('\\') ? '\\' : ''));
  const rest = root ? normalized.slice(root.length) : normalized;
  const result = root ? [{ label: root, path: root }] : [];
  let current = root;
  for (const part of rest.split('\\').filter(Boolean)) {
    current = joinLocalPath(current, part);
    result.push({ label: part, path: current });
  }
  return result.length ? result : [{ label: path, path }];
}

function emptyHistory() {
  return { local: { past: [], future: [] }, remote: { past: [], future: [] } };
}

function permsToBits(perms) {
  const p = (perms || '').trim();
  if (/^[0-7]{3,4}$/.test(p)) {
    const d = p.slice(-3);
    const one = ch => ({ r: !!(+ch & 4), w: !!(+ch & 2), x: !!(+ch & 1) });
    return { owner: one(d[0]), group: one(d[1]), other: one(d[2]) };
  }
  const s = p.replace(/^[dlbcps-]/, '').padEnd(9, '-').slice(0, 9);
  return {
    owner: { r: s[0] === 'r', w: s[1] === 'w', x: s[2] === 'x' },
    group: { r: s[3] === 'r', w: s[4] === 'w', x: s[5] === 'x' },
    other: { r: s[6] === 'r', w: s[7] === 'w', x: s[8] === 'x' },
  };
}

function readDragEntry(dataTransfer) {
  if (!dataTransfer) return null;
  try {
    const raw = dataTransfer.getData(DRAG_MIME) || dataTransfer.getData('text/plain');
    const value = JSON.parse(raw);
    if (!value || !['local', 'remote'].includes(value.side) || typeof value.name !== 'string') return null;
    return { side: value.side, name: value.name, isDir: !!value.isDir };
  } catch {
    return null;
  }
}

/** SFTP 独立窗口 — WinSCP 风格左右双面板 */
export default function SftpWindow({ sessionId }) {
  const [session, setSession] = useState(null);
  const [localPath, setLocalPath] = useState('.');
  const [localPathInput, setLocalPathInput] = useState('.');
  const [localEntries, setLocalEntries] = useState([]);
  const [remotePath, setRemotePath] = useState('.');
  const [remotePathInput, setRemotePathInput] = useState('.');
  const [remoteEntries, setRemoteEntries] = useState([]);
  const [selectedLocal, setSelectedLocal] = useState(null);
  const [selectedRemote, setSelectedRemote] = useState(null);
  const [transferMsg, setTransferMsg] = useState('');
  const [ctxMenu, setCtxMenu] = useState({ visible: false, x: 0, y: 0, target: null, isRemote: false });
  const [chmodDialog, setChmodDialog] = useState(null);
  const [chmodState, setChmodState] = useState({ owner: { r: true, w: true, x: true }, group: { r: true, w: false, x: true }, other: { r: true, w: false, x: true } });
  const [tpOpen, setTpOpen] = useState(true);
  const [tpHeight, setTpHeight] = useState(140);
  const [transfers, setTransfers] = useState([]);
  const [confirmDel, setConfirmDel] = useState(null);
  const [detailsDialog, setDetailsDialog] = useState(null);
  const [editNotices, setEditNotices] = useState([]);
  const [closeConfirm, setCloseConfirm] = useState(false);
  const [showHidden, setShowHidden] = useState(false);
  const [syncBrowse, setSyncBrowse] = useState(false);
  const [dropTarget, setDropTarget] = useState(null);
  const [sorts, setSorts] = useState({ local: { key: 'name', direction: 'asc' }, remote: { key: 'name', direction: 'asc' } });
  const [navHistory, setNavHistory] = useState(emptyHistory);

  const tpDrag = useRef(null);
  const mounted = useRef(true);
  const localRequest = useRef(0);
  const remoteRequest = useRef(0);
  const pathsRef = useRef({ local: localPath, remote: remotePath });
  const historyRef = useRef(navHistory);
  const retryingJobs = useRef(new Set());
  const activeTransfersRef = useRef(false);
  const allowCloseRef = useRef(false);
  const connected = !!session?.status?.connected;

  pathsRef.current = { local: localPath, remote: remotePath };
  historyRef.current = navHistory;
  activeTransfersRef.current = transfers.some(job => job.status === 'running' || job.status === 'paused');

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => { setLocalPathInput(localPath); }, [localPath]);
  useEffect(() => { setRemotePathInput(remotePath); }, [remotePath]);

  useEffect(() => {
    let alive = true;
    setSession(null);
    invoke('list_sessions').then(async list => {
      const found = list.find(item => item.config.id === sessionId);
      if (!alive) return;
      setSession(found);
      if (found?.status?.connected) {
        try {
          const path = await invoke('sftp_pwd', { sessionId });
          if (alive && path) setRemotePath(path);
        } catch {}
      }
      try {
        const home = await invoke('get_home_dir');
        if (alive && home) setLocalPath(home);
      } catch {}
    }).catch(() => {});
    return () => { alive = false; };
  }, [sessionId]);

  const loadLocal = useCallback(async path => {
    const request = ++localRequest.current;
    try {
      const list = await invoke('read_local_dir', { path });
      if (mounted.current && request === localRequest.current) setLocalEntries(list.map(normalizeLocalEntry));
    } catch (error) {
      if (mounted.current && request === localRequest.current) setTransferMsg('❌ 本地目录: ' + String(error));
    }
  }, []);

  const loadRemote = useCallback(async path => {
    const request = ++remoteRequest.current;
    if (!connected) {
      setRemoteEntries([]);
      return;
    }
    try {
      const list = await invoke('sftp_list', { sessionId, path });
      if (mounted.current && request === remoteRequest.current) setRemoteEntries(list.map(normalizeLocalEntry));
    } catch (error) {
      if (mounted.current && request === remoteRequest.current) setTransferMsg('❌ 远端目录: ' + String(error));
    }
  }, [connected, sessionId]);

  useEffect(() => { loadLocal(localPath); }, [loadLocal, localPath]);
  useEffect(() => { loadRemote(remotePath); }, [loadRemote, remotePath]);

  useEffect(() => {
    let disposed = false;
    let stopListening = null;
    setTransfers([]);
    retryingJobs.current.clear();

    const unlisten = listen('transfer-progress', event => {
      if (disposed) return;
      const incoming = event.payload?.job || event.payload;
      if (!incoming?.id) return;
      setTransfers(current => {
        const existing = current.find(job => job.id === incoming.id);
        const jobSession = incoming.session_id ?? existing?.session_id;
        if (jobSession !== sessionId) return current;
        const allowRestart = retryingJobs.current.has(incoming.id);
        const merged = mergeTransferJob(existing, { ...incoming, session_id: jobSession }, allowRestart);
        if (incoming.status === 'running') retryingJobs.current.delete(incoming.id);
        if (merged === existing) return current;
        const index = current.findIndex(job => job.id === incoming.id);
        if (index < 0) return [...current, merged];
        const next = current.slice();
        next[index] = merged;
        return next;
      });
    });
    unlisten.then(stop => {
      if (disposed) {
        stop();
        return;
      }
      stopListening = stop;
      return invoke('list_transfers').then(list => {
        if (disposed) return;
        const snapshot = list.filter(job => job.session_id === sessionId);
        setTransfers(current => mergeTransferSnapshot(snapshot, current));
      });
    }).catch(error => {
      if (!disposed) setTransferMsg('❌ 无法监听传输进度: ' + String(error));
    });

    return () => {
      disposed = true;
      stopListening?.();
    };
  }, [sessionId]);

  useEffect(() => {
    let disposed = false;
    const unlisten = listen('sftp-osc7', event => {
      if (disposed || event.payload?.session_id !== sessionId) return;
      const path = event.payload?.path || event.payload?.cwd?.path;
      if (typeof path !== 'string' || !path.trim() || path === pathsRef.current.remote) return;
      const current = pathsRef.current.remote;
      const nextHistory = {
        ...historyRef.current,
        remote: { past: [...historyRef.current.remote.past, current], future: [] },
      };
      historyRef.current = nextHistory;
      setNavHistory(nextHistory);
      pathsRef.current = { ...pathsRef.current, remote: path };
      setRemotePath(path);
      setSelectedRemote(null);
    });
    unlisten.catch(() => {});
    return () => {
      disposed = true;
      unlisten.then(stop => stop()).catch(() => {});
    };
  }, [sessionId]);

  useEffect(() => {
    let disposed = false;
    const unlisten = listen('sftp-edit-sync', event => {
      if (disposed) return;
      const payload = event.payload || {};
      if (payload.session_id !== sessionId) return;
      const editId = payload.edit_id;
      setEditNotices(current => {
        const index = current.findIndex(item => item.editId === editId);
        if (index < 0) return current;
        const next = current.slice();
        next[index] = {
          ...next[index],
          status: payload.status,
          message: payload.message || '',
          confirming: false,
          cancelling: false,
        };
        return next;
      });
      if (payload.status === 'uploaded') {
        loadRemote(pathsRef.current.remote);
      }
    });
    unlisten.catch(() => {});
    return () => {
      disposed = true;
      unlisten.then(stop => stop()).catch(() => {});
    };
  }, [sessionId, loadRemote]);

  useEffect(() => {
    let alive = true;
    const onStorage = event => {
      if (event.key === 'chena-shell-theme') applyTheme(event.newValue);
    };
    window.addEventListener('storage', onStorage);
    invoke('get_ui_prefs').then(prefs => {
      if (alive) applyTheme(prefs?.theme);
    }).catch(() => {});
    return () => {
      alive = false;
      window.removeEventListener('storage', onStorage);
    };
  }, []);

  const closeWindow = useCallback(async () => {
    allowCloseRef.current = true;
    // 清理注册表不需要阻塞关闭；即使清理失败也不能让窗口卡在关闭流程里。
    invoke('close_sftp_window', { sessionId }).catch(() => {});
    const appWindow = getCurrentWindow();
    try {
      // 由窗口自己关闭，避免后端在 IPC 回调中同步销毁当前 WebView 造成卡死。
      // 此前的 onCloseRequested 已设置 allowCloseRef，重复触发时不会再次拦截。
      await appWindow.close();
    } catch {
      window.close();
    }
  }, [sessionId]);

  useEffect(() => {
    const unlisten = getCurrentWindow().onCloseRequested(event => {
      if (allowCloseRef.current) return;
      event.preventDefault();
      if (activeTransfersRef.current) setCloseConfirm(true);
      else closeWindow();
    });
    return () => { unlisten.then(stop => stop()).catch(() => {}); };
  }, [closeWindow]);

  useEffect(() => {
    const block = event => event.preventDefault();
    document.addEventListener('contextmenu', block);
    return () => document.removeEventListener('contextmenu', block);
  }, []);

  useEffect(() => {
    const onKey = event => {
      if (event.key !== 'Delete') return;
      const target = event.target;
      if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) return;
      if (!connected || !selectedRemote) return;
      const entry = remoteEntries.find(item => item.name === selectedRemote);
      if (entry) setConfirmDel({ name: entry.name, isDir: entry.is_dir });
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [connected, selectedRemote, remoteEntries]);

  useEffect(() => {
    function onMove(event) {
      if (!tpDrag.current) return;
      const dy = tpDrag.current.startY - event.clientY;
      const max = Math.max(120, window.innerHeight - 280);
      setTpHeight(Math.min(max, Math.max(40, tpDrag.current.startH + dy)));
    }
    function onUp() {
      if (!tpDrag.current) return;
      tpDrag.current = null;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    }
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
  }, []);

  useEffect(() => {
    if (chmodDialog?.perms) setChmodState(permsToBits(chmodDialog.perms));
  }, [chmodDialog?.perms]);

  function setSidePath(side, path) {
    pathsRef.current = { ...pathsRef.current, [side]: path };
    if (side === 'remote') {
      setRemotePath(path);
      setSelectedRemote(null);
    } else {
      setLocalPath(path);
      setSelectedLocal(null);
    }
  }

  function navigate(side, path, recordHistory = true) {
    if (typeof path !== 'string' || !path.trim()) return;
    const current = pathsRef.current[side];
    if (path === current) {
      if (side === 'remote') setRemotePathInput(path);
      else setLocalPathInput(path);
      return;
    }
    if (recordHistory) {
      const nextHistory = {
        ...historyRef.current,
        [side]: { past: [...historyRef.current[side].past, current], future: [] },
      };
      historyRef.current = nextHistory;
      setNavHistory(nextHistory);
    }
    setSidePath(side, path);
  }

  function travelHistory(side, direction) {
    const stack = historyRef.current[side];
    const source = direction === 'back' ? stack.past : stack.future;
    if (!source.length) return;
    const target = source[source.length - 1];
    const current = pathsRef.current[side];
    const nextStack = direction === 'back'
      ? { past: stack.past.slice(0, -1), future: [...stack.future, current] }
      : { past: [...stack.past, current], future: stack.future.slice(0, -1) };
    const nextHistory = { ...historyRef.current, [side]: nextStack };
    historyRef.current = nextHistory;
    setNavHistory(nextHistory);
    setSidePath(side, target);
  }

  function openDirectory(side, entry) {
    const target = side === 'remote'
      ? joinRemotePath(pathsRef.current.remote, entry.name)
      : joinLocalPath(pathsRef.current.local, entry.name);
    navigate(side, target);
    if (!syncBrowse) return;
    const other = side === 'remote' ? 'local' : 'remote';
    const otherEntries = other === 'remote' ? remoteEntries : localEntries;
    if (!otherEntries.some(item => item.name === entry.name && item.is_dir)) return;
    const otherTarget = other === 'remote'
      ? joinRemotePath(pathsRef.current.remote, entry.name)
      : joinLocalPath(pathsRef.current.local, entry.name);
    navigate(other, otherTarget);
  }

  function goUp(side) {
    const current = pathsRef.current[side];
    navigate(side, side === 'remote' ? remoteParent(current) : localParent(current));
    if (!syncBrowse) return;
    const other = side === 'remote' ? 'local' : 'remote';
    const otherPath = pathsRef.current[other];
    navigate(other, other === 'remote' ? remoteParent(otherPath) : localParent(otherPath));
  }

  function changeSort(side, key) {
    setSorts(current => {
      const previous = current[side];
      return {
        ...current,
        [side]: { key, direction: previous.key === key && previous.direction === 'asc' ? 'desc' : 'asc' },
      };
    });
  }

  function localFullPath(name) { return joinLocalPath(localPath, name); }
  function remoteFullPath(name) { return joinRemotePath(remotePath, name); }

  async function uploadFile(localName) {
    const fullLocal = localFullPath(localName);
    const remoteFile = remoteFullPath(localName);
    setTransferMsg(`⏫ 上传 ${localName}...`);
    try {
      await invoke('sftp_upload', { sessionId, remotePath: remoteFile, localPath: fullLocal });
      setTransferMsg(`✅ ${localName} 上传成功`);
      loadRemote(remotePath);
    } catch (error) {
      setTransferMsg(`❌ 上传失败: ${error}`);
    }
  }

  async function uploadFolder(localName) {
    const fullLocal = localFullPath(localName);
    const baseRemote = remoteFullPath(localName);
    setTransferMsg(`⏫ 遍历文件夹 ${localName}...`);
    try {
      const files = await invoke('read_local_dir_recursive', { path: fullLocal });
      const dirs = files.filter(file => file[1]).map(file => file[0]);
      for (const dir of dirs) await invoke('sftp_mkdir', { sessionId, remotePath: `${baseRemote}/${dir}` });
      const fileList = files.filter(file => !file[1]);
      let done = 0;
      for (const file of fileList) {
        const localFile = joinLocalPath(fullLocal, file[0].replace(/\//g, '\\'));
        const remoteFile = `${baseRemote}/${file[0]}`;
        try {
          await invoke('sftp_upload', { sessionId, remotePath: remoteFile, localPath: localFile });
          done++;
          setTransferMsg(`⏫ 文件夹上传 ${done}/${fileList.length}: ${file[0]}`);
        } catch {}
      }
      setTransferMsg(`✅ 文件夹 ${localName} 上传完成 (${done}/${fileList.length})`);
      loadRemote(remotePath);
    } catch (error) {
      setTransferMsg(`❌ 文件夹上传失败: ${error}`);
    }
  }

  async function downloadFile(remoteName) {
    const remoteFile = remoteFullPath(remoteName);
    const localFile = localFullPath(remoteName);
    setTransferMsg(`⏬ 下载 ${remoteName} → ${localFile}`);
    try {
      await invoke('sftp_download_to_local', { sessionId, remotePath: remoteFile, localPath: localFile });
      setTransferMsg(`✅ 下载成功 → ${localFile}`);
      loadLocal(localPath);
    } catch (error) {
      setTransferMsg(`❌ 下载失败: ${error}`);
    }
  }

  async function downloadFolder(remoteName) {
    const remoteDir = remoteFullPath(remoteName);
    const localDir = localFullPath(remoteName);
    setTransferMsg(`⏬ 下载文件夹 ${remoteName} → ${localDir}`);
    try {
      const count = await invoke('sftp_download_dir_to_local', { sessionId, remotePath: remoteDir, localPath: localDir });
      setTransferMsg(`✅ 文件夹下载完成：${count} 个文件 → ${localDir}`);
      loadLocal(localPath);
    } catch (error) {
      setTransferMsg(`❌ 文件夹下载失败: ${error}`);
    }
  }

  function onDragStart(event, side, entry) {
    const payload = JSON.stringify({ side, name: entry.name, isDir: entry.is_dir });
    event.dataTransfer.setData(DRAG_MIME, payload);
    event.dataTransfer.setData('text/plain', payload);
    event.dataTransfer.effectAllowed = 'copy';
  }

  function onDrop(event, destination) {
    event.preventDefault();
    setDropTarget(null);
    const entry = readDragEntry(event.dataTransfer);
    if (!entry || entry.side === destination) return;
    if (entry.side === 'local' && destination === 'remote') {
      if (!connected) {
        setTransferMsg('❌ 会话未连接，无法上传');
        return;
      }
      if (entry.isDir) uploadFolder(entry.name);
      else uploadFile(entry.name);
    } else if (entry.side === 'remote' && destination === 'local') {
      if (entry.isDir) downloadFolder(entry.name);
      else downloadFile(entry.name);
    }
  }

  async function deleteRemote(name) {
    const remoteFile = remoteFullPath(name);
    setConfirmDel(current => current?.name === name ? { ...current, deleting: true, error: '' } : current);
    try {
      await invoke('sftp_delete', { sessionId, remotePath: remoteFile });
      setTransferMsg(`✅ ${name} 已删除`);
      setConfirmDel(null);
      loadRemote(remotePath);
    } catch (error) {
      const message = `删除失败: ${String(error)}`;
      setTransferMsg(`❌ ${message}`);
      setConfirmDel(current => current?.name === name ? { ...current, deleting: false, error: message } : current);
    }
  }

  function upsertEditNotice(notice) {
    const key = notice.editId || notice.key;
    setEditNotices(current => {
      const index = current.findIndex(item => (item.editId || item.key) === key);
      if (index < 0) return [...current, notice];
      const next = current.slice();
      next[index] = { ...next[index], ...notice };
      return next;
    });
  }

  function dismissEditNotice(key) {
    setEditNotices(current => current.filter(item => (item.editId || item.key) !== key));
  }

  async function beginRemoteEdit(remoteName) {
    const remoteFile = remoteFullPath(remoteName);
    const openingKey = `opening:${remoteFile}`;
    try {
      const editorPrefs = await invoke('get_editor_prefs');
      if (editorPrefs?.mode !== 'external') {
        upsertEditNotice({ key: openingKey, name: remoteName, remotePath: remoteFile, status: 'opening', message: '正在打开内置编辑器…' });
        const opened = await invoke('sftp_edit_open', { sessionId, remotePath: remoteFile });
        await invoke('open_edit_window', {
          sessionId,
          editId: opened.edit_id,
          title: `${remoteName} — 内置编辑器`,
        });
        dismissEditNotice(openingKey);
        return;
      }
    } catch (error) {
      dismissEditNotice(openingKey);
      setEditNotices(current => [...current, {
        key: `open-error:${remoteFile}`,
        name: remoteName,
        remotePath: remoteFile,
        status: 'error',
        message: `打开内置编辑器失败: ${String(error)}`,
      }]);
      return;
    }

    upsertEditNotice({ key: openingKey, name: remoteName, remotePath: remoteFile, status: 'opening', message: '正在打开本地编辑器…' });
    try {
      const result = await invoke('sftp_edit_begin', { sessionId, remotePath: remoteFile });
      setEditNotices(current => {
        const next = current.filter(item => (item.editId || item.key) !== openingKey);
        return [...next, {
          editId: result.edit_id,
          name: remoteName,
          remotePath: result.remote_path,
          localPath: result.local_path,
          backupPath: result.backup_path,
          status: 'opened',
          message: '已在本地编辑器打开，关闭后若有修改将自动回传',
        }];
      });
    } catch (error) {
      const message = String(error);
      setEditNotices(current => {
        const next = current.filter(item => (item.editId || item.key) !== openingKey);
        return [...next, {
          key: `open-error:${remoteFile}`,
          name: remoteName,
          remotePath: remoteFile,
          status: 'error',
          message: `打开编辑器失败: ${message}`,
        }];
      });
    }
  }

  async function confirmOverwriteEdit(editId) {
    if (!editId) return;
    setEditNotices(current => current.map(item => item.editId === editId ? { ...item, confirming: true } : item));
    try {
      await invoke('sftp_edit_confirm_overwrite', { sessionId, editId });
      // 成功后后端会发出 sftp-edit-sync(uploaded) 事件，由监听器刷新列表与状态
    } catch (error) {
      const message = String(error);
      const status = message.includes(EDIT_CONFLICT_PREFIX) ? 'conflict' : 'error';
      setEditNotices(current => current.map(item => item.editId === editId ? {
        ...item, confirming: false, status, message,
      } : item));
    }
  }

  async function cancelRemoteEdit(editId) {
    if (!editId) return;
    setEditNotices(current => current.map(item => item.editId === editId ? { ...item, cancelling: true } : item));
    try {
      await invoke('sftp_edit_cancel', { sessionId, editId });
      // 取消标记已生效；编辑器关闭后后端会再发 cancelled 事件，这里先更新为已取消
      setEditNotices(current => current.map(item => item.editId === editId ? {
        ...item, cancelling: false, status: 'cancelled', message: '已取消回传',
      } : item));
    } catch (error) {
      setEditNotices(current => current.map(item => item.editId === editId ? {
        ...item, cancelling: false, status: 'error', message: `取消回传失败: ${String(error)}`,
      } : item));
    }
  }

  async function onChangeMode(remoteName, mode) {
    const remoteFile = remoteFullPath(remoteName);
    try {
      await invoke('sftp_chmod', { sessionId, remotePath: remoteFile, mode });
      setTransferMsg(`✅ 权限已更新: ${remoteFile} → ${mode}`);
      setChmodDialog(null);
      loadRemote(remotePath);
    } catch (error) {
      setTransferMsg(`❌ chmod 失败: ${error}`);
    }
  }

  function openCtxMenu(event, target, isRemote) {
    event.preventDefault();
    event.stopPropagation();
    const MENU_W = 220, MENU_H = 340;
    let x = event.clientX, y = event.clientY;
    if (x + MENU_W > window.innerWidth) x = window.innerWidth - MENU_W - 8;
    if (y + MENU_H > window.innerHeight) y = window.innerHeight - MENU_H - 8;
    if (x < 4) x = 4;
    if (y < 4) y = 4;
    if (isRemote) setSelectedRemote(target.name);
    else setSelectedLocal(target.name);
    setCtxMenu({ visible: true, x, y, target, isRemote });
  }

  function transferControl(command, jobId, message) {
    if (command === 'retry_transfer') retryingJobs.current.add(jobId);
    invoke(command, { jobId })
      .then(() => setTransferMsg(message))
      .catch(error => {
        retryingJobs.current.delete(jobId);
        setTransferMsg(`❌ 传输操作失败: ${error}`);
      });
  }

  async function copyPath(text) {
    try {
      if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(text);
      else throw new Error('Clipboard API 不可用');
      setTransferMsg('✅ 路径已复制');
    } catch {
      try {
        const textarea = document.createElement('textarea');
        textarea.value = text;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        const ok = document.execCommand('copy');
        document.body.removeChild(textarea);
        setTransferMsg(ok ? '✅ 路径已复制' : '❌ 复制失败');
      } catch (error) {
        setTransferMsg(`❌ 复制失败: ${error}`);
      }
    }
  }

  async function openInExplorer(name) {
    try {
      await invoke('open_in_explorer', { path: localFullPath(name) });
    } catch (error) {
      setTransferMsg(`❌ 打开失败: ${error}`);
    }
  }

  function fmtSize(size) {
    if (size == null) return '—';
    if (size < 1024) return size + ' B';
    if (size < 1024 * 1024) return (size / 1024).toFixed(1) + ' KB';
    if (size < 1024 * 1024 * 1024) return (size / 1024 / 1024).toFixed(1) + ' MB';
    return (size / 1024 / 1024 / 1024).toFixed(2) + ' GB';
  }

  function fmtTime(mtime) {
    if (!mtime) return '—';
    return new Date(mtime * 1000).toLocaleString('zh-CN', { hour12: false });
  }

  const bits = value => (value.r ? 4 : 0) + (value.w ? 2 : 0) + (value.x ? 1 : 0);
  const modeStr = () => `${bits(chmodState.owner)}${bits(chmodState.group)}${bits(chmodState.other)}`;

  function renderHeader(side) {
    const label = side === 'remote' ? '远端' : '本地';
    const sort = sorts[side];
    const arrow = key => sort.key === key ? (sort.direction === 'asc' ? ' ↑' : ' ↓') : '';
    return (
      <div className="sftp-file-header">
        <div></div>
        <button className="sftp-sort-button" onClick={() => changeSort(side, 'name')} aria-label={`${label}按名称排序`}>名称{arrow('name')}</button>
        <button className="sftp-sort-button align-right" onClick={() => changeSort(side, 'size')} aria-label={`${label}按大小排序`}>大小{arrow('size')}</button>
        <button className="sftp-sort-button align-right sftp-time-column" disabled={side === 'local' && localEntries.every(item => !item.mtime)} title={side === 'local' && localEntries.every(item => !item.mtime) ? '本地目录命令暂未提供修改时间' : ''} onClick={() => changeSort(side, 'mtime')} aria-label={`${label}按时间排序`}>时间{arrow('mtime')}</button>
        <div className="sftp-file-meta">权限</div>
      </div>
    );
  }

  function renderRow(item, side) {
    const isRemote = side === 'remote';
    const selected = (isRemote ? selectedRemote : selectedLocal) === item.name;
    const iconColor = item.is_dir ? 'var(--md-primary)' : 'var(--md-on-surface-variant)';
    const bgIcon = item.is_dir ? 'var(--md-primary-container)' : 'var(--md-surface-container-high)';
    return (
      <div
        key={`${side}-${item.name}`}
        data-testid={`${side}-row-${item.name}`}
        className={`sftp-file-row${selected ? ' selected' : ''}`}
        draggable
        onDragStart={event => onDragStart(event, side, item)}
        onClick={() => isRemote ? setSelectedRemote(item.name) : setSelectedLocal(item.name)}
        onDoubleClick={() => { if (item.is_dir) openDirectory(side, item); }}
        onContextMenu={event => openCtxMenu(event, { name: item.name, isDir: item.is_dir, size: item.size, mtime: item.mtime, perms: item.perms || '' }, isRemote)}
      >
        <div style={{ width: 28, height: 28, borderRadius: 6, display: 'grid', placeItems: 'center', background: bgIcon, color: iconColor }}>
          <span className="material-icons-round" style={{ fontSize: 14 }}>{item.is_dir ? 'folder' : 'description'}</span>
        </div>
        <div className="sftp-file-name" title={item.name}>{item.name}</div>
        <div className="sftp-file-meta">{item.is_dir ? '—' : fmtSize(item.size)}</div>
        <div className="sftp-file-meta sftp-time-column" title={fmtTime(item.mtime)}>{fmtTime(item.mtime)}</div>
        <div className="sftp-file-meta">{item.perms || '—'}</div>
      </div>
    );
  }

  function renderPanel(side) {
    const isRemote = side === 'remote';
    const label = isRemote ? '远端' : '本地';
    const path = isRemote ? remotePath : localPath;
    const pathInput = isRemote ? remotePathInput : localPathInput;
    const entries = isRemote ? remoteEntries : localEntries;
    const items = visibleEntries(entries.slice(), sorts[side], showHidden);
    const hiddenCount = showHidden ? 0 : entries.filter(isEntryHidden).length;
    return (
      <section
        aria-label={`${label}文件面板`}
        data-testid={`${side}-panel`}
        className={`sftp-drop-panel${dropTarget === side ? ' drop-target' : ''}`}
        onDragOver={event => { event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; setDropTarget(side); }}
        onDragLeave={event => { if (!event.currentTarget.contains(event.relatedTarget)) setDropTarget(null); }}
        onDrop={event => onDrop(event, side)}
        style={{ display: 'flex', flexDirection: 'column', minHeight: 0, background: 'var(--md-surface-container-lowest)' }}
      >
        <div style={{ height: 36, flexShrink: 0, background: 'var(--md-surface-container)', borderBottom: '1px solid var(--md-outline-variant)', display: 'flex', alignItems: 'center', padding: '0 10px', gap: 8, fontSize: 12, fontWeight: 600 }}>
          <span className="material-icons-round" style={{ fontSize: 16, color: isRemote ? '#10B981' : 'var(--md-primary)' }}>{isRemote ? 'cloud_queue' : 'computer'}</span>
          {isRemote ? `远端 (${session.config.name || session.config.host})` : '本地'}
        </div>
        <div style={{ minHeight: 32, flexShrink: 0, background: 'var(--md-surface-container-low)', borderBottom: '1px solid var(--md-outline-variant)', display: 'flex', alignItems: 'center', gap: 2, padding: '3px 6px' }}>
          <button className="sftp-icon-button" disabled={!navHistory[side].past.length} onClick={() => travelHistory(side, 'back')} title={`${label}后退`} aria-label={`${label}后退`}><span className="material-icons-round" style={{ fontSize: 16 }}>arrow_back</span></button>
          <button className="sftp-icon-button" disabled={!navHistory[side].future.length} onClick={() => travelHistory(side, 'forward')} title={`${label}前进`} aria-label={`${label}前进`}><span className="material-icons-round" style={{ fontSize: 16 }}>arrow_forward</span></button>
          <button className="sftp-icon-button" onClick={() => goUp(side)} title={`${label}上一级`} aria-label={`${label}上一级`}><span className="material-icons-round" style={{ fontSize: 16 }}>arrow_upward</span></button>
          <button className="sftp-icon-button" onClick={() => (isRemote ? loadRemote(remotePath) : loadLocal(localPath))} disabled={isRemote && !connected} title={`${label}刷新`} aria-label={`${label}刷新`}><span className="material-icons-round" style={{ fontSize: 16 }}>refresh</span></button>
          <input
            aria-label={`${label}路径`}
            className="sftp-path-input"
            value={pathInput}
            onChange={event => isRemote ? setRemotePathInput(event.target.value) : setLocalPathInput(event.target.value)}
            onKeyDown={event => { if (event.key === 'Enter') navigate(side, pathInput); }}
          />
        </div>
        <nav className="sftp-breadcrumbs" aria-label={`${label}路径面包屑`}>
          {breadcrumbs(path, isRemote).map((crumb, index, all) => (
            <Fragment key={`${crumb.path}-${index}`}>
              {index > 0 && <span className="sftp-breadcrumb-separator">›</span>}
              <button className="sftp-breadcrumb" onClick={() => navigate(side, crumb.path)} title={crumb.path}>{crumb.label}</button>
              {index === all.length - 1 && <span style={{ width: 2 }}></span>}
            </Fragment>
          ))}
        </nav>
        <div style={{ flex: 1, overflow: 'auto' }}>
          {isRemote && !connected && <div style={{ padding: 40, textAlign: 'center', color: 'var(--md-on-surface-variant)', fontSize: 12 }}>会话未连接，请先在主窗口连接</div>}
          {(!isRemote || connected) && (
            <>
              {renderHeader(side)}
              {items.length === 0 && hiddenCount > 0 && (
                <div style={{ padding: 20, textAlign: 'center', fontSize: 12, color: 'var(--md-on-surface-variant)' }}>
                  该目录有 {hiddenCount} 个隐藏项，勾选上方「显示隐藏文件」查看
                </div>
              )}
              {items.length === 0 && hiddenCount === 0 && (
                <div style={{ padding: 20, textAlign: 'center', fontSize: 12, color: 'var(--md-on-surface-variant)' }}>空目录</div>
              )}
              {items.map(item => renderRow(item, side))}
            </>
          )}
        </div>
      </section>
    );
  }

  if (!session) return <div style={{ padding: 40, color: 'var(--md-on-surface-variant)', background: 'var(--md-surface)', height: '100vh' }}>加载会话中...</div>;

  return (
    <div style={{ width: '100vw', height: '100vh', display: 'flex', flexDirection: 'column', background: 'var(--md-surface)' }}>
      <div style={{ minHeight: 48, flexShrink: 0, background: 'var(--md-surface-container)', borderBottom: '1px solid var(--md-outline-variant)', display: 'flex', alignItems: 'center', gap: 6, padding: '5px 10px', flexWrap: 'wrap' }}>
        <button aria-label="返回主窗口" onClick={() => activeTransfersRef.current ? setCloseConfirm(true) : closeWindow()} style={{ height: 30, padding: '0 12px', borderRadius: 8, border: '1px solid var(--md-outline)', background: 'transparent', color: 'var(--md-on-surface)', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: 6, fontSize: 12, fontFamily: 'inherit' }}>
          <span className="material-icons-round" style={{ fontSize: 18 }}>arrow_back</span>返回主窗口
        </button>
        <div style={{ width: 1, height: 22, background: 'var(--md-outline-variant)', margin: '0 4px' }}></div>
        <button onClick={() => loadLocal(localPath)} className="sftp-icon-button" style={{ border: '1px solid var(--md-outline)', width: 30, height: 30 }} title="刷新本地"><span className="material-icons-round" style={{ fontSize: 18 }}>refresh</span></button>
        <button onClick={() => loadRemote(remotePath)} disabled={!connected} className="sftp-icon-button" style={{ border: '1px solid var(--md-outline)', width: 30, height: 30 }} title="刷新远端"><span className="material-icons-round" style={{ fontSize: 18 }}>refresh</span></button>
        <button
          onClick={() => {
            if (!connected || !selectedRemote) return;
            const entry = remoteEntries.find(item => item.name === selectedRemote);
            if (entry) setConfirmDel({ name: entry.name, isDir: entry.is_dir });
          }}
          disabled={!connected || !selectedRemote}
          title={selectedRemote ? `删除远端: ${selectedRemote} (Delete)` : '删除远端文件/目录（先选中一项）'}
          className="sftp-icon-button"
          style={{ border: '1px solid var(--md-outline)', width: 30, height: 30, color: connected && selectedRemote ? '#DC2626' : undefined }}
        ><span className="material-icons-round" style={{ fontSize: 18 }}>delete</span></button>
        <label className="sftp-toggle" title="显示或隐藏以点开头的文件和目录">
          <input type="checkbox" checked={showHidden} onChange={event => setShowHidden(event.target.checked)} />显示隐藏文件
        </label>
        <label className="sftp-toggle" title="进入目录时，若另一侧存在同名目录则一起进入；上一级也会同步">
          <input type="checkbox" checked={syncBrowse} onChange={event => setSyncBrowse(event.target.checked)} />同步浏览
        </label>
        <div style={{ flex: 1 }}></div>
        <div style={{ display: 'inline-flex', alignItems: 'center', gap: 6, padding: '4px 12px', borderRadius: 12, background: 'var(--md-surface-container-low)', fontSize: 11, color: 'var(--md-on-surface-variant)' }}>
          <span style={{ width: 8, height: 8, borderRadius: '50%', background: connected ? '#10B981' : '#DC2626', boxShadow: connected ? '0 0 8px #10B981' : 'none' }}></span>
          <span>{session.config.username}@{session.config.host} {connected ? '已连接' : '未连接'}</span>
        </div>
      </div>

      {transferMsg && (
        <div style={{ padding: '6px 12px', background: transferMsg.startsWith('❌') ? '#FEE2E2' : '#DBEAFE', color: transferMsg.startsWith('❌') ? '#991B1B' : '#1E40AF', fontSize: 12, display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexShrink: 0 }}>
          <span>{transferMsg}</span>
          <button onClick={() => setTransferMsg('')} style={{ background: 'transparent', border: 0, cursor: 'pointer', color: 'inherit', opacity: .6 }}>✕</button>
        </div>
      )}

      {editNotices.length > 0 && (
        <div style={{ flexShrink: 0, display: 'flex', flexDirection: 'column', gap: 4, padding: '6px 12px', background: 'var(--md-surface-container-low)', borderBottom: '1px solid var(--md-outline-variant)' }}>
          {editNotices.map(notice => {
            const key = notice.editId || notice.key;
            const isConflict = notice.status === 'conflict';
            const isError = notice.status === 'error' || notice.status === 'open_error';
            const uploaded = notice.status === 'uploaded';
            const cancelled = notice.status === 'cancelled';
            const tone = isError ? '#991B1B' : isConflict ? '#92400E' : uploaded ? '#065F46' : '#1E40AF';
            const bg = isError ? '#FEE2E2' : isConflict ? '#FEF3C7' : uploaded ? '#D1FAE5' : '#DBEAFE';
            const label = ({
              opening: '打开中', opened: '已打开', uploaded: '已回传', unchanged: '未修改',
              conflict: '远端冲突', error: '失败', open_error: '打开失败', cancelled: '已取消',
            })[notice.status] || notice.status;
            const icon = isError ? 'error' : isConflict ? 'warning' : uploaded ? 'check_circle' : cancelled ? 'cancel' : 'edit';
            const message = notice.message ? notice.message.replace(EDIT_CONFLICT_PREFIX, '').trim() : '';
            return (
              <div key={key} role="status" data-testid={`edit-notice-${notice.status}`} data-edit-id={notice.editId || ''} style={{ display: 'flex', alignItems: 'flex-start', gap: 8, padding: '6px 10px', borderRadius: 8, background: bg, color: tone, fontSize: 12 }}>
                <span className="material-icons-round" style={{ fontSize: 16 }} aria-hidden="true">{icon}</span>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontWeight: 600 }}>{notice.name} · {label}</div>
                  {message && <div style={{ marginTop: 2, wordBreak: 'break-all' }}>{message}</div>}
                  {notice.localPath && <div style={{ marginTop: 2, opacity: .75, wordBreak: 'break-all' }}>本地: {notice.localPath}</div>}
                  {notice.backupPath && <div style={{ opacity: .75, wordBreak: 'break-all' }}>原始备份: {notice.backupPath}</div>}
                  {notice.editId && (isConflict || isError) && (
                    <div style={{ display: 'flex', gap: 6, marginTop: 4, flexWrap: 'wrap' }}>
                      {isConflict && (
                        <button className="sftp-dialog-button danger" disabled={notice.confirming} onClick={() => confirmOverwriteEdit(notice.editId)}>{notice.confirming ? '覆盖中…' : '确认覆盖'}</button>
                      )}
                      <button className="sftp-dialog-button" disabled={notice.cancelling} onClick={() => cancelRemoteEdit(notice.editId)}>{notice.cancelling ? '取消中…' : '取消回传'}</button>
                    </div>
                  )}
                </div>
                <button onClick={() => dismissEditNotice(key)} style={{ background: 'transparent', border: 0, cursor: 'pointer', color: 'inherit', opacity: .6 }} aria-label="关闭编辑通知">✕</button>
              </div>
            );
          })}
        </div>
      )}

      <div style={{ flex: 1, display: 'grid', gridTemplateColumns: '1fr 4px 1fr', minHeight: 0 }}>
        {renderPanel('local')}
        <div style={{ background: 'var(--md-outline-variant)' }}></div>
        {renderPanel('remote')}
      </div>

      <div style={{ flexShrink: 0, borderTop: '1px solid var(--md-outline-variant)', background: 'var(--md-surface-container)', display: 'flex', flexDirection: 'column' }}>
        {tpOpen && (
          <div title="拖动调整传输列表高度" onMouseDown={event => {
            event.preventDefault();
            tpDrag.current = { startY: event.clientY, startH: tpHeight };
            document.body.style.cursor = 'row-resize';
            document.body.style.userSelect = 'none';
          }} style={{ height: 6, flexShrink: 0, cursor: 'row-resize', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
            <div style={{ width: 44, height: 3, borderRadius: 2, background: 'var(--md-outline-variant)' }}></div>
          </div>
        )}
        <div style={{ height: 32, flexShrink: 0, display: 'flex', alignItems: 'center', gap: 8, padding: '0 12px', cursor: 'pointer', fontSize: 12, fontWeight: 600 }} onClick={() => setTpOpen(open => !open)}>
          <span className="material-icons-round" style={{ fontSize: 16, color: 'var(--md-primary)' }}>queue</span>传输队列
          {transfers.length > 0 && <span style={{ background: 'var(--md-primary)', color: 'var(--md-on-primary)', fontSize: 10, padding: '1px 6px', borderRadius: 8 }}>{transfers.length}</span>}
          <div style={{ flex: 1 }}></div>
          <span style={{ fontSize: 18, transition: 'transform .2s', transform: tpOpen ? 'rotate(0)' : 'rotate(-90deg)' }} className="material-icons-round">expand_more</span>
        </div>
        {tpOpen && (
          <div style={{ height: tpHeight, flexShrink: 0, overflowY: 'auto', padding: '4px 8px' }}>
            {transfers.length === 0 && <div style={{ padding: 16, textAlign: 'center', color: 'var(--md-on-surface-variant)', fontSize: 11 }}>暂无传输任务</div>}
            {[...transfers].reverse().map(job => {
              const local = job.local_path || '';
              const remote = job.remote_path || '';
              return (
                <div key={job.id} style={{ display: 'grid', gridTemplateColumns: '24px 1fr 120px 60px 76px', alignItems: 'center', gap: 8, padding: '5px 8px', borderRadius: 6, background: 'var(--md-surface-container-low)', marginBottom: 4, fontSize: 11, borderLeft: `3px solid ${job.direction === 'upload' ? 'var(--md-primary)' : '#10B981'}` }}>
                  <div style={{ fontSize: 14, textAlign: 'center', color: job.direction === 'upload' ? 'var(--md-primary)' : '#10B981' }}>{job.direction === 'upload' ? '⬆' : '⬇'}</div>
                  <div>
                    <div style={{ fontWeight: 500, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{local.split(/[\\/]/).pop() || remote}</div>
                    <div style={{ fontSize: 10, color: 'var(--md-on-surface-variant)', fontFamily: "'JetBrains Mono',monospace", overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{local} → {remote}</div>
                    {job.error && <div style={{ fontSize: 10, color: '#DC2626', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={job.error}>{job.error}</div>}
                  </div>
                  <div><div style={{ height: 6, background: 'var(--md-surface-container-highest)', borderRadius: 3, overflow: 'hidden' }}><div style={{ height: '100%', width: `${transferPercent(job)}%`, borderRadius: 3, background: job.status === 'done' ? '#10B981' : (job.direction === 'upload' ? 'var(--md-primary)' : '#10B981'), transition: 'width .2s' }}></div></div></div>
                  <div style={{ fontFamily: "'JetBrains Mono',monospace", textAlign: 'right', color: 'var(--md-on-surface-variant)' }}>{transferStatusLabel(job)}</div>
                  <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 2 }}>
                    {job.status === 'running' && <button onClick={() => transferControl('pause_transfer', job.id, '已请求暂停')} title="暂停传输" className="sftp-icon-button" style={{ width: 22, height: 22, borderRadius: '50%' }}><span className="material-icons-round" style={{ fontSize: 14 }}>pause</span></button>}
                    {job.status === 'paused' && <button onClick={() => transferControl('resume_transfer', job.id, '已恢复传输')} title="恢复传输" className="sftp-icon-button" style={{ width: 22, height: 22, borderRadius: '50%' }}><span className="material-icons-round" style={{ fontSize: 14 }}>play_arrow</span></button>}
                    {job.status === 'error' && <button onClick={() => transferControl('retry_transfer', job.id, '正在重试')} title="重试传输" className="sftp-icon-button" style={{ width: 22, height: 22, borderRadius: '50%' }}><span className="material-icons-round" style={{ fontSize: 14 }}>refresh</span></button>}
                    {(job.status === 'running' || job.status === 'paused') && <button onClick={() => transferControl('cancel_transfer', job.id, '已取消并清理临时文件')} title="取消传输" className="sftp-icon-button" style={{ width: 22, height: 22, borderRadius: '50%' }}><span className="material-icons-round" style={{ fontSize: 14 }}>close</span></button>}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>

      {ctxMenu.visible && ctxMenu.target && (() => {
        const target = ctxMenu.target;
        const items = [{ title: true, label: target.name }];
        items.push({
          icon: 'refresh',
          label: '刷新',
          disabled: ctxMenu.isRemote && !connected,
          action: () => (ctxMenu.isRemote ? loadRemote(remotePath) : loadLocal(localPath)),
        });
        items.push({ divider: true });
        if (ctxMenu.isRemote) {
          items.push({ icon: 'edit', label: '编辑', disabled: target.isDir, action: () => beginRemoteEdit(target.name) });
          items.push(target.isDir
            ? { icon: 'download', label: '下载整个文件夹', action: () => downloadFolder(target.name) }
            : { icon: 'download', label: '下载', action: () => downloadFile(target.name) });
          items.push({ icon: 'delete', label: '删除...', action: () => setConfirmDel({ name: target.name, isDir: target.isDir }) });
          items.push({ divider: true });
          items.push({ icon: 'content_copy', label: '复制完整路径', action: () => copyPath(remoteFullPath(target.name)) });
          items.push({ icon: 'info', label: '查看详情', action: () => setDetailsDialog({ ...target, path: remoteFullPath(target.name), remote: true }) });
          items.push({ divider: true });
          items.push({ icon: 'lock', label: '修改权限 (chmod)...', action: () => setChmodDialog({ target, perms: target.perms }) });
        } else {
          items.push(target.isDir
            ? { icon: 'upload', label: '上传整个文件夹', action: () => uploadFolder(target.name) }
            : { icon: 'upload', label: '上传到远端', action: () => uploadFile(target.name) });
          items.push({ icon: 'folder_open', label: '在资源管理器中打开', action: () => openInExplorer(target.name) });
          items.push({ divider: true });
          items.push({ icon: 'content_copy', label: '复制完整路径', action: () => copyPath(localFullPath(target.name)) });
          items.push({ icon: 'info', label: '属性', action: () => setDetailsDialog({ ...target, path: localFullPath(target.name), remote: false }) });
        }
        return (
          <div style={{ position: 'fixed', left: ctxMenu.x, top: ctxMenu.y, zIndex: 9999, background: 'var(--md-surface-container-high)', border: '1px solid var(--md-outline-variant)', borderRadius: 10, boxShadow: '0 8px 24px rgba(0,0,0,.18)', padding: '6px 0', minWidth: 200 }} onClick={event => event.stopPropagation()}>
            {items.map((item, index) => {
              if (item.divider) return <div key={index} style={{ height: 1, background: 'var(--md-outline-variant)', margin: '4px 0' }}></div>;
              if (item.title) return <div key={index} style={{ padding: '4px 14px 8px', fontSize: 10, color: 'var(--md-on-surface-variant)', fontFamily: "'JetBrains Mono',monospace", overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{item.label}</div>;
              return (
                <div key={index} role="menuitem" aria-label={item.label} aria-disabled={item.disabled ? 'true' : undefined} onClick={() => { if (item.disabled) return; setCtxMenu(menu => ({ ...menu, visible: false })); item.action?.(); }} onMouseEnter={event => { if (!item.disabled) event.currentTarget.style.background = 'var(--md-primary-container)'; }} onMouseLeave={event => { event.currentTarget.style.background = 'transparent'; }} style={{ padding: '7px 14px', display: 'flex', alignItems: 'center', gap: 10, cursor: item.disabled ? 'not-allowed' : 'pointer', opacity: item.disabled ? 0.4 : 1, color: 'var(--md-on-surface)', fontSize: 12, transition: 'background .08s' }}>
                  <span aria-hidden="true" className="material-icons-round" style={{ fontSize: 16, color: 'var(--md-on-surface-variant)' }}>{item.icon}</span>{item.label}
                </div>
              );
            })}
          </div>
        );
      })()}

      {confirmDel && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,.4)', display: 'grid', placeItems: 'center', zIndex: 10001 }} onClick={() => { if (!confirmDel.deleting) setConfirmDel(null); }}>
          <div role="dialog" aria-modal="true" aria-label="确认删除" style={{ background: 'var(--md-surface)', borderRadius: 14, width: 380, padding: 22, boxShadow: '0 20px 60px rgba(0,0,0,.3)', border: '1px solid var(--md-outline-variant)' }} onClick={event => event.stopPropagation()}>
            <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 8, display: 'flex', alignItems: 'center', gap: 8 }}><span className="material-icons-round" style={{ fontSize: 20, color: '#DC2626' }}>warning</span>确认删除</div>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)', marginBottom: 18, fontFamily: "'JetBrains Mono',monospace" }}>{confirmDel.isDir ? '文件夹' : '文件'}: {confirmDel.name}<div style={{ color: '#DC2626', marginTop: 6 }}>此操作不可撤销！</div></div>
            {confirmDel.error && <div role="alert" style={{ color: '#991b1b', background: '#fee2e2', borderRadius: 8, padding: '8px 10px', marginBottom: 14, fontSize: 12, overflowWrap: 'anywhere' }}>{confirmDel.error}</div>}
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button disabled={confirmDel.deleting} onClick={() => setConfirmDel(null)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: '1px solid var(--md-outline)', background: 'transparent', color: 'var(--md-on-surface)', cursor: 'pointer', fontSize: 12 }}>取消</button>
              <button disabled={confirmDel.deleting} onClick={() => deleteRemote(confirmDel.name)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: 0, background: '#DC2626', color: 'white', cursor: 'pointer', fontSize: 12, fontWeight: 500 }}>{confirmDel.deleting ? '删除中...' : '删除'}</button>
            </div>
          </div>
        </div>
      )}

      {closeConfirm && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,.4)', display: 'grid', placeItems: 'center', zIndex: 10002 }} onClick={() => setCloseConfirm(false)}>
          <div role="dialog" aria-label="确认关闭窗口" style={{ background: 'var(--md-surface)', borderRadius: 14, width: 400, padding: 22, border: '1px solid var(--md-outline-variant)' }} onClick={event => event.stopPropagation()}>
            <div style={{ fontSize: 15, fontWeight: 600, marginBottom: 10 }}>仍有传输任务正在进行</div>
            <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)', marginBottom: 20 }}>关闭窗口不会自动取消任务，但将无法在此查看进度。确定要关闭吗？</div>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button onClick={() => setCloseConfirm(false)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: '1px solid var(--md-outline)', background: 'transparent', color: 'var(--md-on-surface)', cursor: 'pointer', fontSize: 12 }}>继续查看</button>
              <button onClick={closeWindow} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: 0, background: '#DC2626', color: 'white', cursor: 'pointer', fontSize: 12 }}>仍要关闭</button>
            </div>
          </div>
        </div>
      )}

      {chmodDialog && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,.4)', display: 'grid', placeItems: 'center', zIndex: 10000 }} onClick={() => setChmodDialog(null)}>
          <div style={{ background: 'var(--md-surface)', borderRadius: 16, width: 440, padding: 22, boxShadow: '0 20px 60px rgba(0,0,0,.3)', border: '1px solid var(--md-outline-variant)' }} onClick={event => event.stopPropagation()}>
            <div style={{ fontSize: 15, fontWeight: 600, marginBottom: 4 }}>🔧 修改权限</div>
            <div style={{ fontSize: 11, color: 'var(--md-on-surface-variant)', marginBottom: 16, fontFamily: "'JetBrains Mono',monospace", overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{chmodDialog.target?.name} {chmodDialog.perms && `(${chmodDialog.perms})`}</div>
            <div style={{ display: 'grid', gridTemplateColumns: '64px repeat(3, 1fr)', gap: 8 }}>
              <div></div><div style={{ fontSize: 11, color: 'var(--md-on-surface-variant)', textAlign: 'center' }}>读 (r)</div><div style={{ fontSize: 11, color: 'var(--md-on-surface-variant)', textAlign: 'center' }}>写 (w)</div><div style={{ fontSize: 11, color: 'var(--md-on-surface-variant)', textAlign: 'center' }}>执行 (x)</div>
              {[{ key: 'owner', label: '拥有者' }, { key: 'group', label: '组' }, { key: 'other', label: '其他' }].map(row => (
                <Fragment key={row.key}>
                  <div style={{ fontSize: 12, display: 'flex', alignItems: 'center', gap: 4 }}>{row.label}<span style={{ fontFamily: "'JetBrains Mono',monospace", fontSize: 11, color: 'var(--md-primary)' }}>{bits(chmodState[row.key])}</span></div>
                  {['r', 'w', 'x'].map(bit => (
                    <button key={bit} onClick={() => setChmodState(state => ({ ...state, [row.key]: { ...state[row.key], [bit]: !state[row.key][bit] } }))} style={{ height: 34, borderRadius: 8, cursor: 'pointer', border: '1px solid var(--md-outline-variant)', background: chmodState[row.key][bit] ? 'var(--md-primary)' : 'var(--md-surface-container-low)', color: chmodState[row.key][bit] ? 'var(--md-on-primary)' : 'var(--md-on-surface-variant)', fontFamily: "'JetBrains Mono',monospace", fontSize: 14, fontWeight: 700 }}>{chmodState[row.key][bit] ? bit.toUpperCase() : '—'}</button>
                  ))}
                </Fragment>
              ))}
            </div>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginTop: 18 }}>
              <div style={{ fontSize: 12, color: 'var(--md-on-surface-variant)' }}>当前权限: <span style={{ fontFamily: "'JetBrains Mono',monospace", color: 'var(--md-primary)', fontSize: 20, fontWeight: 700 }}>{modeStr()}</span></div>
              <div style={{ display: 'flex', gap: 8 }}>
                <button onClick={() => setChmodDialog(null)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: '1px solid var(--md-outline)', background: 'transparent', color: 'var(--md-on-surface)', cursor: 'pointer', fontSize: 12 }}>取消</button>
                <button onClick={() => onChangeMode(chmodDialog.target.name, modeStr())} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: 0, background: 'var(--md-primary)', color: 'var(--md-on-primary)', cursor: 'pointer', fontSize: 12, fontWeight: 500 }}>应用 ({modeStr()})</button>
              </div>
            </div>
          </div>
        </div>
      )}

      {detailsDialog && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,.4)', display: 'grid', placeItems: 'center', zIndex: 10000 }} onClick={() => setDetailsDialog(null)}>
          <div style={{ background: 'var(--md-surface)', borderRadius: 16, width: 520, padding: 22, boxShadow: '0 20px 60px rgba(0,0,0,.3)', border: '1px solid var(--md-outline-variant)' }} onClick={event => event.stopPropagation()}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 16 }}><span className="material-icons-round" style={{ fontSize: 22, color: 'var(--md-primary)' }}>{detailsDialog.isDir ? 'folder' : 'description'}</span><div style={{ fontSize: 15, fontWeight: 600 }}>{detailsDialog.remote ? '远程文件详情' : '本地文件属性'}</div></div>
            {[
              ['名称', detailsDialog.name],
              ['类型', detailsDialog.isDir ? '文件夹' : '文件'],
              ['大小', detailsDialog.isDir ? '—' : `${fmtSize(detailsDialog.size)} (${detailsDialog.size ?? 0} B)`],
              ['修改时间', fmtTime(detailsDialog.mtime)],
              ...(detailsDialog.remote ? [['权限', detailsDialog.perms || '未知']] : []),
              ['完整路径', detailsDialog.path],
            ].map(([key, value]) => (
              <div key={key} style={{ display: 'flex', gap: 12, padding: '7px 0', borderBottom: '1px solid var(--md-outline-variant)' }}><div style={{ width: 70, flexShrink: 0, fontSize: 12, color: 'var(--md-on-surface-variant)' }}>{key}</div><div style={{ flex: 1, fontSize: 12, fontFamily: key === '完整路径' ? "'JetBrains Mono',monospace" : 'inherit', wordBreak: 'break-all' }}>{value}</div></div>
            ))}
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, marginTop: 18 }}>
              <button onClick={() => copyPath(detailsDialog.path)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: '1px solid var(--md-outline)', background: 'transparent', color: 'var(--md-on-surface)', cursor: 'pointer', fontSize: 12 }}>复制路径</button>
              <button onClick={() => setDetailsDialog(null)} style={{ height: 34, padding: '0 16px', borderRadius: 8, border: 0, background: 'var(--md-primary)', color: 'var(--md-on-primary)', cursor: 'pointer', fontSize: 12, fontWeight: 500 }}>关闭</button>
            </div>
          </div>
        </div>
      )}

      {ctxMenu.visible && <div style={{ position: 'fixed', inset: 0, zIndex: 9998 }} onClick={() => setCtxMenu(menu => ({ ...menu, visible: false }))}></div>}
    </div>
  );
}
