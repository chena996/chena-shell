import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { localizeCommand, localizeReason, localizeRule } from '../lib/aiLabels.js';

const TABS = [
  { id:'monitor', icon:'monitoring', label:'资源监控' },
  { id:'ai',      icon:'smart_toy',  label:'AI 活动' },
  { id:'pf',      icon:'alt_route',  label:'端口转发' },
  { id:'log',     icon:'terminal',   label:'日志' },
  { id:'qcmd',    icon:'bolt',       label:'快速命令' },
];

function fmtBytes(v) {
  if (!v) return '0 B/s';
  if (v < 1024) return `${v} B/s`;
  if (v < 1024 * 1024) return `${(v / 1024).toFixed(1)} KB/s`;
  return `${(v / 1024 / 1024).toFixed(2)} MB/s`;
}

const MAX_MONITOR_HISTORY_MS = 15 * 60 * 1000;

function finiteNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

// eslint-disable-next-line react-refresh/only-export-components
export function appendMonitorHistory(history, snapshot, now = Date.now()) {
  const cutoff = now - MAX_MONITOR_HISTORY_MS;
  const retained = (history || []).filter(sample => Number.isFinite(sample.ts) && sample.ts >= cutoff);
  if (!snapshot?.online || snapshot.stale || snapshot.last_error) return retained;
  const ts = Date.parse(snapshot.ts);
  if (!Number.isFinite(ts)) return retained;
  const baseline = snapshot.baseline_ready === true;
  const point = {
    ts,
    cpu: baseline ? finiteNumber(snapshot.cpu_pct) : null,
    mem: finiteNumber(snapshot.mem_pct),
    disk: finiteNumber(snapshot.disk_pct),
    load: snapshot.capabilities?.load_average === false ? null : finiteNumber(snapshot.load_1m),
    netIn: baseline ? finiteNumber(snapshot.net_in_bytes_per_s) : null,
    netOut: baseline ? finiteNumber(snapshot.net_out_bytes_per_s) : null,
  };
  return retained.filter(sample => sample.ts !== ts).concat(point)
    .filter(sample => sample.ts >= cutoff).sort((a, b) => a.ts - b.ts);
}

// eslint-disable-next-line react-refresh/only-export-components
export function selectMonitorHistory(history, minutes, now = Date.now()) {
  const cutoff = now - minutes * 60 * 1000;
  return (history || []).filter(sample => sample.ts >= cutoff);
}

function fmtKb(kb) {
  if (!Number.isFinite(kb)) return '—';
  if (kb >= 1024 * 1024) return `${(kb / 1024 / 1024).toFixed(1)}G`;
  return `${(kb / 1024).toFixed(0)}M`;
}

const PLATFORM_LABELS = {
  linux: 'Linux',
  windows: 'Windows',
  darwin: 'macOS',
};

function platformLabel(platform) {
  return PLATFORM_LABELS[String(platform || '').toLowerCase()] || '未知';
}

// eslint-disable-next-line react-refresh/only-export-components
export function getNetworkHover(netHistIn, netHistOut, netHover, netW = 600, netH = 110) {
  const netLen = Math.max(netHistIn.length, netHistOut.length);
  if (!netHover || netLen < 2) return null;
  const netMax = Math.max(...netHistIn, ...netHistOut, 1);
  const idx = Math.min(netHover.idx, netLen - 1);
  const send = netHistOut[idx] ?? 0;
  const recv = netHistIn[idx] ?? 0;
  return {
    idx,
    x: (idx * netW) / (netLen - 1),
    send,
    recv,
    sendY: netH - (Math.min(send, netMax) / netMax) * netH,
    recvY: netH - (Math.min(recv, netMax) / netMax) * netH,
    ago: netLen - 1 - idx,
  };
}

const NETWORK_TOOLTIP_WIDTH = 150;
const NETWORK_TOOLTIP_HEIGHT = 60;

// eslint-disable-next-line react-refresh/only-export-components
export function getNetworkTooltipPosition(hover) {
  if (!hover) return null;
  const maxLeft = Math.max(4, (finiteNumber(hover.w) ?? 0) - NETWORK_TOOLTIP_WIDTH - 4);
  const maxTop = Math.max(4, (finiteNumber(hover.h) ?? 0) - NETWORK_TOOLTIP_HEIGHT - 4);
  const below = hover.y + 12;
  const preferredTop = below <= maxTop
    ? below : hover.y - NETWORK_TOOLTIP_HEIGHT - 12;
  return {
    left: Math.max(4, Math.min(hover.x + 12, maxLeft)),
    top: Math.max(4, Math.min(preferredTop, maxTop)),
  };
}

/** 把后端 AIEvent 转成展示行 */
function toRows(events) {
  return (events || []).slice().reverse().map(ev => {
    const k = ev.kind || {};
    const kind = k.kind;
    const time = ev.ts ? new Date(ev.ts).toLocaleTimeString('zh-CN', { hour12: false }) : '';
    const denialReason = ev.denial_reason || (kind === 'Rejected' ? k.reason : '');
    const meta = [
      ev.source ? `来源：${ev.source}` : '',
      (ev.rule || k.rule) ? `规则：${localizeRule(ev.rule || k.rule)}` : '',
      denialReason ? `原因：${localizeReason(denialReason)}` : '',
    ].filter(Boolean).join(' · ');
    const base = { id: ev.id, time, meta };
    switch (kind) {
      case 'Attach':
        return { ...base, icon:'smart_toy', color:'#7C3AED', title:`AI 接入 · ${localizeReason(k.reason)}`, tag:'开始' };
      case 'Detach':
        return { ...base, icon:'check_circle', color:'#10B981', title:`AI 断开 · ${localizeReason(k.reason)}`, tag:'完成' };
      case 'ForceDetach':
        return { ...base, icon:'block', color:'#F59E0B', title:`AI 被踢出 · ${localizeReason(k.reason)}`, tag:'强制' };
      case 'Exec':
        return {
          ...base, icon:'play_arrow', color:'#3B82F6', title: localizeCommand(k.command),
          tag: ev.exit_code == null ? '完成' : `退出码 ${ev.exit_code}`,
          time: ev.duration_ms != null ? `${time} · ${ev.duration_ms} 毫秒` : time,
        };
      case 'Blocked':
        return { ...base, icon:'shield', color:'#EF4444', title: localizeCommand(k.command), tag:'已拦截' };
      case 'WaitingConfirm':
        return { ...base, icon:'help', color:'#F59E0B', title: localizeCommand(k.command), tag:'待确认' };
      case 'Confirmed':
        return { ...base, icon:'verified_user', color:'#10B981', title: localizeCommand(k.command), tag:'已批准' };
      case 'Rejected':
        return { ...base, icon:'cancel', color:'#EF4444', title: localizeCommand(k.command), tag:'已拒绝' };
      case 'Expired':
        return { ...base, icon:'timer_off', color:'#F59E0B', title: localizeCommand(k.command), tag:'已超时' };
      default:
        return { ...base, icon:'info', color:'#6B7280', title: kind || '事件', tag: '' };
    }
  });
}

export default function BottomPanel({
  session,
  monitor,
  aiEvents = [],
  confirmations = [],
  confirmationBusy = '',
  onResolveConfirmation = () => {},
  onSessionUpdated = async () => {},
}) {
  const [active, setActive] = useState('monitor');
  const [monitorHistory, setMonitorHistory] = useState([]);
  const [historyWindow, setHistoryWindow] = useState(1);
  const [netHover, setNetHover] = useState(null); // 网络图悬停 {idx,x,y,w,h}

  useEffect(() => {
    setMonitorHistory([]);
    setNetHover(null);
  }, [session?.id]);

  // 最多保留真实时间戳最近 15 分钟；失败/陈旧快照只触发裁剪，不伪造新采样。
  useEffect(() => {
    if (monitor?.session_id && session?.id && monitor.session_id !== session.id) return;
    setMonitorHistory(history => appendMonitorHistory(history, monitor));
  }, [monitor, session?.id]);

  const chartHistory = useMemo(() => {
    const visible = selectMonitorHistory(monitorHistory, historyWindow);
    const network = visible.filter(sample => sample.netIn != null && sample.netOut != null);
    return {
      cpu: visible.map(sample => sample.cpu).filter(value => value != null),
      mem: visible.map(sample => sample.mem).filter(value => value != null),
      disk: visible.map(sample => sample.disk).filter(value => value != null),
      load: visible.map(sample => sample.load).filter(value => value != null),
      netIn: network.map(sample => sample.netIn),
      netOut: network.map(sample => sample.netOut),
      netTimes: network.map(sample => sample.ts),
    };
  }, [monitorHistory, historyWindow]);

  const hasData = Boolean(monitor?.online && monitor?.last_success_at);
  const hasBaseline = hasData && monitor.baseline_ready === true;
  const cpu = hasBaseline ? Number(monitor.cpu_pct.toFixed(0)) : null;
  const mem = hasData ? Number(monitor.mem_pct.toFixed(0)) : null;
  const disk = hasData ? Number(monitor.disk_pct.toFixed(0)) : null;
  const disks = Array.isArray(monitor?.disks) ? monitor.disks : [];
  const rootDisk = disks.find(item => item.mount_point === '/') || disks[0];
  const topCpuProcesses = monitor?.top_cpu_processes || [];
  const topMemProcesses = monitor?.top_mem_processes || [];
  const hasLoadAverage = hasData && monitor.capabilities?.load_average !== false;
  const hasDiskByMount = monitor?.capabilities?.disk_by_mount !== false;
  const hasProcessTop = monitor?.capabilities?.process_top !== false;
  const monitorStatus = !monitor ? '等待监控快照'
    : monitor.stale ? '数据已过期'
      : monitor.last_error ? '采样失败'
        : !hasData ? '等待首次成功采样'
          : !hasBaseline ? '首轮基线未就绪' : '实时';

  const sessionConfirmations = confirmations.filter(item => item.session_id === session?.id);

  return (
    <div style={{
      height:'100%', flexShrink:0, background:'var(--md-surface)',
      borderTop:'1px solid var(--md-outline-variant)',
      display:'flex', flexDirection:'column', minHeight:0,
    }}>
      {/* TABS */}
      <div style={{
        height:40, flexShrink:0, display:'flex',
        borderBottom:'1px solid var(--md-outline-variant)',
        padding:'0 8px', gap:2,
      }}>
        {TABS.map(t => (
          <button key={t.id}
            onClick={() => setActive(t.id)}
            style={{
              padding:'0 14px', border:0, background:'transparent',
              color: active === t.id ? 'var(--md-primary)' : 'var(--md-on-surface-variant)',
              fontFamily:'inherit', fontSize:12, fontWeight:500, cursor:'pointer',
              position:'relative', display:'flex', alignItems:'center', gap:6,
              borderBottom: active === t.id ? '2px solid var(--md-primary)' : '2px solid transparent',
            }}>
            <span className="material-icons-round" style={{ fontSize:16 }}>{t.icon}</span>
            {t.label}
            {(t.id === 'ai' && sessionConfirmations.length > 0 ? sessionConfirmations.length : t.badge) && (
              <span style={{
                fontSize:10, padding:'1px 6px', borderRadius:8,
                background:'var(--ai-accent)', color:'#FFF', fontWeight:600,
              }}>{t.id === 'ai' && sessionConfirmations.length > 0 ? sessionConfirmations.length : t.badge}</span>
            )}
          </button>
        ))}
      </div>

      {/* CONTENT */}
      <div style={{ flex:1, minHeight:0, overflow:'hidden' }}>
        {active === 'monitor' && (() => {
          // ======== 紧凑 sparkline SVG ========
          const spark = (arr, maxY = 100, color = '#3B82F6') => {
            const w = 120, h = 22;
            if (!arr || arr.length < 2) {
              return <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" style={{ width:'100%', height:22, display:'block' }} />;
            }
            const stepX = w / (arr.length - 1);
            const pts = arr.map((v, i) => `${(i * stepX).toFixed(1)},${(h - (Math.min(v, maxY) / maxY) * h).toFixed(1)}`).join(' ');
            const areaPts = `0,${h} ${pts} ${w},${h}`;
            return (
              <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" style={{ width:'100%', height:22, display:'block' }}>
                <polygon points={areaPts} fill={color} fillOpacity="0.15" />
                <polyline fill="none" stroke={color} strokeWidth="1.5" points={pts} />
              </svg>
            );
          };

          // ======== Windows 风格网络大图 ========
          const netHistIn = chartHistory.netIn;
          const netHistOut = chartHistory.netOut;
          const netTimes = chartHistory.netTimes;
          const netLen = Math.max(netHistIn.length, netHistOut.length);
          const netMax = netLen ? Math.max(...netHistIn, ...netHistOut, 0) : 0;
          const netScale = Math.max(netMax, 1);
          const netW = 600, netH = 110;
          const buildNetPoints = (arr) => {
            if (!arr || arr.length < 2) return '';
            const stepX = netW / (arr.length - 1);
            return arr.map((v, i) => `${(i * stepX).toFixed(1)},${(netH - (v / netScale) * netH).toFixed(1)}`).join(' ');
          };
          const buildNetArea = (arr) => {
            if (!arr || arr.length < 2) return '';
            const stepX = netW / (arr.length - 1);
            const pts = arr.map((v, i) => `${(i * stepX).toFixed(1)},${(netH - (v / netScale) * netH).toFixed(1)}`).join(' ');
            return `0,${netH} ${pts} ${netW},${netH}`;
          };
          const netInPts = buildNetPoints(netHistIn);
          const netOutPts = buildNetPoints(netHistOut);
          const netInArea = buildNetArea(netHistIn);
          const netOutArea = buildNetArea(netHistOut);

          // 悬停取值：蓝线=发送(netOut)，绿线=接收(netIn)，与图例一致
          const hover = getNetworkHover(netHistIn, netHistOut, netHover, netW, netH);
          const hoverIdx = hover?.idx ?? null;
          const hoverX = hover?.x ?? 0;
          const hoverSend = hover?.send ?? 0;
          const hoverRecv = hover?.recv ?? 0;
          const hoverSendY = hover?.sendY ?? 0;
          const hoverRecvY = hover?.recvY ?? 0;
          const hoverAgo = hoverIdx == null ? 0
            : Math.max(0, Math.round((netTimes[netTimes.length - 1] - netTimes[hoverIdx]) / 1000));
          const tooltipPosition = getNetworkTooltipPosition(netHover);

          function onNetMove(e) {
            const rect = e.currentTarget.getBoundingClientRect();
            const containerRect = e.currentTarget.parentElement.getBoundingClientRect();
            const len = Math.max(netHistIn.length, netHistOut.length);
            if (len < 2) return;
            const rx = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
            setNetHover({
              idx: Math.round(rx * (len - 1)),
              x: e.clientX - containerRect.left,
              y: e.clientY - containerRect.top,
              w: containerRect.width,
              h: containerRect.height,
            });
          }

          return (
            <div style={{ padding:'8px 12px', height:'100%', display:'flex', flexDirection:'column', gap:8, minHeight:0, overflow:'auto' }}>
              <div role="status" style={{ display:'flex', alignItems:'center', gap:12, flexWrap:'wrap', minHeight:14, fontSize:10, color:'var(--md-on-surface-variant)' }}>
                <span style={{ color: monitor?.stale || monitor?.last_error ? '#B45309' : 'inherit' }}>{monitorStatus}</span>
                <span>平台：{platformLabel(monitor?.platform)}</span>
                {monitor?.last_success_at && (
                  <span>最近成功采样：{new Date(monitor.last_success_at).toLocaleTimeString('zh-CN', { hour12:false })}</span>
                )}
                {monitor?.last_error && <span style={{ color:'#B91C1C', overflowWrap:'anywhere' }}>采样错误：{monitor.last_error}</span>}
              </div>
              {/* 第一行：4 个紧凑指标 */}
              <div style={{ display:'grid', gridTemplateColumns:'repeat(4,1fr)', gap:10 }}>
                {[
                  { name:'CPU', val:cpu == null ? '—' : `${cpu}%`, sub: hasData ? `${monitor.cores || '?'}核` : '—', color:'#3B82F6', hist: chartHistory.cpu },
                  { name:'内存', val:mem == null ? '—' : `${mem}%`, sub: hasData ? `${(monitor.mem_used_mb/1024).toFixed(1)}/${(monitor.mem_total_mb/1024).toFixed(1)}GB` : '—', color:'#10B981', hist: chartHistory.mem },
                  { name:'磁盘', val:disk == null ? '—' : `${disk}%`, sub: hasData ? (rootDisk ? `${fmtKb(rootDisk.used_kb)}/${fmtKb(rootDisk.total_kb)} (${rootDisk.mount_point})` : `${monitor.disk_used_gb}G/${monitor.disk_total_gb}G (/)`) : '—', color:'#F59E0B', hist: chartHistory.disk },
                  { name:'负载', val: hasLoadAverage ? monitor.load_1m.toFixed(2) : '—', sub: hasLoadAverage ? `5m ${monitor.load_5m.toFixed(1)}` : '—', color:'#EF4444', hist: hasLoadAverage ? chartHistory.load : [], maxY: Math.max(...chartHistory.load, 1) },
                ].map((m, i) => (
                  <div key={i} style={{
                    display:'flex', alignItems:'center', gap:10, padding:'6px 10px',
                    background:'var(--md-surface-container)', borderRadius:10, border:'1px solid var(--md-outline-variant)',
                  }}>
                    <div style={{ flex:1, minWidth:0 }}>
                      <div style={{ fontSize:10, fontWeight:500, color:'var(--md-on-surface-variant)', letterSpacing:'.04em' }}>{m.name}</div>
                      <div style={{ fontSize:18, fontWeight:700, fontFamily:"'JetBrains Mono',monospace", color:'var(--md-on-surface)', lineHeight:1.1 }}>{m.val}</div>
                      <div style={{ fontSize:10, color:'var(--md-on-surface-variant)', marginTop:2 }}>{m.sub}</div>
                    </div>
                    <div style={{ width:100, flexShrink:0 }}>{spark(m.hist, m.maxY || 100, m.color)}</div>
                  </div>
                ))}
              </div>

              {/* 第二行：Windows 风格网络大图 */}
              <div style={{ flex:1, minHeight:100, background:'var(--md-surface-container)', borderRadius:12, border:'1px solid var(--md-outline-variant)', display:'flex', flexDirection:'column', overflow:'hidden' }}>
                <div style={{ display:'flex', alignItems:'center', gap:8, padding:'6px 12px', borderBottom:'1px solid var(--md-outline-variant)', fontSize:11, fontWeight:500 }}>
                  <span className="material-icons-round" style={{ fontSize:14, color:'#60A5FA' }}>lan</span>
                  <span style={{ color:'var(--md-on-surface-variant)' }}>网络吞吐量</span>
                  <div aria-label="历史时间窗" style={{ display:'flex', gap:2, marginLeft:4 }}>
                    {[1, 5, 15].map(minutes => (
                      <button key={minutes} type="button" aria-pressed={historyWindow === minutes}
                        onClick={() => { setHistoryWindow(minutes); setNetHover(null); }}
                        style={{ height:20, padding:'0 6px', border:'1px solid var(--md-outline-variant)', borderRadius:5,
                          background:historyWindow === minutes ? 'var(--md-primary-container)' : 'transparent',
                          color:'var(--md-on-surface-variant)', fontFamily:'inherit', fontSize:9, cursor:'pointer' }}>
                        {minutes}m
                      </button>
                    ))}
                  </div>
                  <div style={{ flex:1 }}></div>
                  <div style={{ display:'flex', alignItems:'center', gap:12, fontSize:10, color:'var(--md-on-surface-variant)' }}>
                    <div style={{ display:'flex', alignItems:'center', gap:4 }}>
                      <div style={{ width:14, height:3, background:'#3B82F6' }}></div>
                      <span>发送 ↑ {hasBaseline ? fmtBytes(monitor.net_out_bytes_per_s) : '—'}</span>
                    </div>
                    <div style={{ display:'flex', alignItems:'center', gap:4 }}>
                      <div style={{ width:14, height:3, background:'#10B981' }}></div>
                      <span>接收 ↓ {hasBaseline ? fmtBytes(monitor.net_in_bytes_per_s) : '—'}</span>
                    </div>
                  </div>
                </div>
                <div style={{ flex:1, padding:'4px 10px', minHeight:80, position:'relative' }}>
                  <svg
                    viewBox={`0 0 ${netW} ${netH}`}
                    preserveAspectRatio="none"
                    style={{ width:'100%', height:'100%', display:'block', cursor:'crosshair' }}
                    onMouseMove={onNetMove}
                    onMouseLeave={() => setNetHover(null)}
                  >
                    {/* 网格 */}
                    {[0.25, 0.5, 0.75].map((p, i) => (
                      <line key={i} x1="0" y1={netH * p} x2={netW} y2={netH * p}
                        stroke="currentColor" strokeOpacity="0.06" strokeWidth="0.5" strokeDasharray="2 3" />
                    ))}
                    {/* X 轴 */}
                    <line x1="0" y1={netH - 0.5} x2={netW} y2={netH - 0.5}
                      stroke="currentColor" strokeOpacity="0.1" strokeWidth="0.5" />
                    {/* 发送（蓝色面积 + 实线）*/}
                    <polygon points={netOutArea} fill="#3B82F6" fillOpacity="0.18" />
                    <polyline fill="none" stroke="#3B82F6" strokeWidth="1.5" strokeLinejoin="round" points={netOutPts} />
                    {/* 接收（绿色面积 + 实线）*/}
                    <polygon points={netInArea} fill="#10B981" fillOpacity="0.18" />
                    <polyline fill="none" stroke="#10B981" strokeWidth="1.5" strokeLinejoin="round" points={netInPts} />
                    {/* 悬停指示：竖线 + 两条曲线上的取值点 */}
                    {hoverIdx != null && (
                      <>
                        <line x1={hoverX} y1="0" x2={hoverX} y2={netH}
                          stroke="currentColor" strokeOpacity="0.35" strokeWidth="0.7" strokeDasharray="3 2" />
                        <circle cx={hoverX} cy={hoverSendY} r="2.4" fill="#3B82F6" stroke="#FFF" strokeWidth="0.8" />
                        <circle cx={hoverX} cy={hoverRecvY} r="2.4" fill="#10B981" stroke="#FFF" strokeWidth="0.8" />
                      </>
                    )}
                  </svg>
                  {/* 悬停数值浮层 */}
                  {hoverIdx != null && tooltipPosition && (
                    <div style={{
                      position:'absolute', pointerEvents:'none', zIndex:5,
                      left: tooltipPosition.left,
                      top: tooltipPosition.top,
                      width: NETWORK_TOOLTIP_WIDTH, height: NETWORK_TOOLTIP_HEIGHT,
                      padding:'6px 8px', borderRadius:8,
                      background:'var(--md-surface-container-highest, rgba(30,30,30,.92))',
                      border:'1px solid var(--md-outline-variant)',
                      boxShadow:'0 4px 14px rgba(0,0,0,.2)',
                      fontSize:10, lineHeight:1.35,
                    }}>
                      <div style={{ color:'var(--md-on-surface-variant)', marginBottom:3 }}>
                        {hoverAgo === 0 ? '刚刚' : `${hoverAgo} 秒前`}
                      </div>
                      <div style={{ display:'flex', alignItems:'center', gap:5, color:'#3B82F6', fontFamily:"'JetBrains Mono',monospace", fontWeight:600 }}>
                        <span>↑ 发送</span>
                        <span style={{ marginLeft:'auto' }}>{fmtBytes(hoverSend)}</span>
                      </div>
                      <div style={{ display:'flex', alignItems:'center', gap:5, color:'#10B981', fontFamily:"'JetBrains Mono',monospace", fontWeight:600 }}>
                        <span>↓ 接收</span>
                        <span style={{ marginLeft:'auto' }}>{fmtBytes(hoverRecv)}</span>
                      </div>
                    </div>
                  )}
                </div>
                <div style={{ padding:'2px 12px', fontSize:9, color:'var(--md-on-surface-variant)', textAlign:'right', fontFamily:"'JetBrains Mono',monospace" }}>
                  峰值 {netLen ? fmtBytes(netMax) : '—'} · 最近 {historyWindow} 分钟 · {netLen} 个采样点
                </div>
              </div>
              <details style={{ flexShrink:0, padding:'4px 10px', border:'1px solid var(--md-outline-variant)', borderRadius:8,
                background:'var(--md-surface-container)', fontSize:10, color:'var(--md-on-surface-variant)' }}>
                <summary style={{ cursor:'pointer' }}>磁盘挂载点（{disks.length}）与进程 TOP</summary>
                <div style={{ display:'grid', gridTemplateColumns:'1.2fr 1fr 1fr', gap:12, padding:'8px 0 2px' }}>
                  <section>
                    <strong>磁盘（按挂载点）</strong>
                    {!hasDiskByMount
                      ? <div style={{ marginTop:4 }}>当前平台不支持按挂载点采集磁盘</div>
                      : disks.length === 0 && <div style={{ marginTop:4 }}>暂无磁盘挂载点数据</div>}
                    {disks.map(item => (
                      <div key={item.mount_point} title={item.filesystem} style={{ marginTop:5, fontFamily:"'JetBrains Mono',monospace" }}>
                        <div style={{ display:'flex', gap:6 }}><span style={{ flex:1 }}>{item.mount_point}</span><span>{item.used_pct.toFixed(1)}%</span></div>
                        <div style={{ opacity:.75 }}>{fmtKb(item.used_kb)} / {fmtKb(item.total_kb)}</div>
                      </div>
                    ))}
                  </section>
                  <section>
                    <strong>CPU TOP</strong>
                    {!hasProcessTop
                      ? <div style={{ marginTop:4 }}>当前平台不支持进程 TOP 采集</div>
                      : topCpuProcesses.length === 0 && <div style={{ marginTop:4 }}>暂无进程 TOP 数据</div>}
                    {topCpuProcesses.map(process => (
                      <div key={`cpu-${process.pid}`} style={{ display:'flex', gap:5, marginTop:5, fontFamily:"'JetBrains Mono',monospace" }}>
                        <span style={{ flex:1, overflow:'hidden', textOverflow:'ellipsis', whiteSpace:'nowrap' }} title={`${process.name} (${process.pid})`}>{process.name} ({process.pid})</span>
                        <span>{process.cpu_pct.toFixed(1)}%</span>
                      </div>
                    ))}
                  </section>
                  <section>
                    <strong>内存 TOP</strong>
                    {!hasProcessTop
                      ? <div style={{ marginTop:4 }}>当前平台不支持进程 TOP 采集</div>
                      : topMemProcesses.length === 0 && <div style={{ marginTop:4 }}>暂无进程 TOP 数据</div>}
                    {topMemProcesses.map(process => (
                      <div key={`mem-${process.pid}`} style={{ display:'flex', gap:5, marginTop:5, fontFamily:"'JetBrains Mono',monospace" }}>
                        <span style={{ flex:1, overflow:'hidden', textOverflow:'ellipsis', whiteSpace:'nowrap' }} title={`${process.name} (${process.pid})`}>{process.name} ({process.pid})</span>
                        <span>{process.mem_pct.toFixed(1)}%</span>
                      </div>
                    ))}
                  </section>
                </div>
              </details>
            </div>
          );
        })()}

        {active === 'ai' && (
          <div style={{ padding:12, height:'100%', overflow:'auto', boxSizing:'border-box', display:'flex', flexDirection:'column', gap:6 }}>
            {sessionConfirmations.map(item => {
              const busy = confirmationBusy === item.id;
              const isExec = item.kind === 'exec';
              return (
                <section key={item.id} aria-label="待审批请求" style={{
                  padding:'10px 12px', borderRadius:10, border:'1px solid #F59E0B66',
                  background:'#F59E0B0d', display:'flex', flexDirection:'column', gap:8,
                }}>
                  <div style={{ display:'flex', alignItems:'center', gap:8, fontSize:12 }}>
                    <span className="material-icons-round" style={{ fontSize:18, color:'#D97706' }}>gpp_maybe</span>
                    <strong style={{ color:'var(--md-on-surface)' }}>{isExec ? '命令执行待审批' : 'AI 接入待审批'}</strong>
                    <span style={{ color:'var(--md-on-surface-variant)' }}>来源：{item.client_name}</span>
                    <span style={{ marginLeft:'auto', color:'#B45309', fontSize:11 }}>
                      截止 {new Date(item.expires_at).toLocaleTimeString('zh-CN', { hour12:false })}
                    </span>
                  </div>
                  {isExec && <code style={{
                    padding:'7px 9px', borderRadius:7, background:'var(--md-surface-container-low)',
                    color:'var(--md-on-surface)', fontSize:11, whiteSpace:'pre-wrap', overflowWrap:'anywhere',
                  }}>{item.command_preview || '（无命令预览）'}</code>}
                  <div style={{ fontSize:11, lineHeight:1.5, color:'var(--md-on-surface-variant)' }}>
                    原因：{localizeReason(item.reason)}{item.rule ? ` · 规则：${localizeRule(item.rule)}` : ''}
                  </div>
                  <div style={{ display:'flex', justifyContent:'flex-end', gap:8 }}>
                    <button type="button" disabled={busy} onClick={() => onResolveConfirmation(item, false)} style={{
                      height:28, padding:'0 12px', borderRadius:7, border:'1px solid #DC2626',
                      background:'transparent', color:'#DC2626', fontFamily:'inherit', fontSize:11, cursor:'pointer',
                    }}>拒绝</button>
                    <button type="button" disabled={busy} onClick={() => onResolveConfirmation(item, true)} style={{
                      height:28, padding:'0 12px', borderRadius:7, border:0,
                      background:'#B45309', color:'#FFF', fontFamily:'inherit', fontSize:11, cursor:'pointer',
                    }}>{busy ? '处理中…' : '批准一次'}</button>
                  </div>
                </section>
              );
            })}
            {aiEvents.length === 0 && sessionConfirmations.length === 0 && (
              <div style={{ padding:20, textAlign:'center', fontSize:13, color:'var(--md-on-surface-variant)' }}>
                {session?.connected ? '暂无 AI 活动。AI 通过 MCP 接入并执行命令后会显示在这里。' : '会话未连接。'}
              </div>
            )}
            {toRows(aiEvents).map(e => (
              <div key={e.id || `${e.time}-${e.title}`} style={{
                display:'flex', gap:12, padding:'8px 12px', borderRadius:10,
                alignItems:'center',
              }}>
                <div style={{
                  width:32, height:32, borderRadius:9, background:`${e.color}1f`, color:e.color,
                  display:'grid', placeItems:'center', flexShrink:0,
                }}>
                  <span className="material-icons-round" style={{ fontSize:16 }}>{e.icon}</span>
                </div>
                <div style={{ flex:1, minWidth:0 }}>
                  <div style={{ fontSize:13, fontWeight:500, display:'flex', gap:8, alignItems:'center', flexWrap:'wrap', overflowWrap:'anywhere' }}>
                    {e.title}
                    <span style={{
                      fontSize:10, padding:'2px 6px', borderRadius:5, fontWeight:600,
                      background:`${e.color}1f`, color:e.color,
                    }}>{e.tag}</span>
                  </div>
                  {e.meta && <div style={{ fontSize:11, color:'var(--md-on-surface-variant)', marginTop:3, lineHeight:1.45 }}>{e.meta}</div>}
                  <div style={{ fontSize:11, color:'var(--md-on-surface-variant)', marginTop:2 }}>{e.time}</div>
                </div>
              </div>
            ))}
          </div>
        )}


        {active === 'pf' && <ForwardTab session={session} />}

        {active === 'log' && <LogTab session={session} onSessionUpdated={onSessionUpdated} />}

        {active === 'qcmd' && <QuickCmdTab session={session} />}
      </div>
    </div>
  );
}

/** 端口转发：本地转发（ssh -L）与远端转发（ssh -R） */
function ForwardTab({ session }) {
  const [list, setList] = useState([]);
  const [err, setErr] = useState('');
  const [busy, setBusy] = useState('');
  const [kind, setKind] = useState('local');
  const [form, setForm] = useState({
    listen_host: '127.0.0.1', listen_port: '18080',
    dest_host: '', dest_port: '80',
  });

  const assetIp = session?.host || '';

  // 选中/连接会话后自动填好对端 IP
  useEffect(() => {
    if (!session) return;
    setForm(f => ({
      ...f,
      listen_host: kind === 'remote' ? (assetIp || '0.0.0.0') : '127.0.0.1',
      listen_port: kind === 'remote' ? '19080' : (f.listen_port || '18080'),
      dest_host: kind === 'remote' ? '127.0.0.1' : (assetIp || '127.0.0.1'),
    }));
    /* eslint-disable-next-line */
  }, [session?.id, session?.connected, kind]);

  const sessionId = session?.id;
  const refresh = useCallback(async () => {
    if (!sessionId) { setList([]); return; }
    try { setList(await invoke('list_forwards', { sessionId })); } catch (e) { setErr(String(e)); }
  }, [sessionId]);

  useEffect(() => { refresh(); }, [refresh, session?.connected]);

  async function add() {
    if (!session) return;
    const lp = Number(form.listen_port), dp = Number(form.dest_port);
    if (!lp || !dp) { setErr('端口必须填写'); return; }
    const fwd = {
      id: `fw-${Date.now()}`,
      kind,
      listen_host: form.listen_host || (kind === 'remote' ? '0.0.0.0' : '127.0.0.1'),
      listen_port: lp,
      dest_host: form.dest_host || '127.0.0.1',
      dest_port: dp,
      enabled: true,
    };
    setBusy('add');
    try {
      await invoke('add_port_forward', { sessionId: session.id, forward: fwd });
      await invoke('start_port_forward', { sessionId: session.id, forwardId: fwd.id });
      setErr('');
      await refresh();
    } catch (e) { setErr(String(e)); }
    setBusy('');
  }

  async function toggle(f) {
    setBusy(f.id);
    try {
      if (f.running) await invoke('stop_port_forward', { forwardId: f.id });
      else await invoke('start_port_forward', { sessionId: session.id, forwardId: f.id });
      setErr('');
      await refresh();
    } catch (e) { setErr(String(e)); }
    setBusy('');
  }

  async function del(f) {
    setBusy(f.id);
    try { await invoke('remove_port_forward', { sessionId: session.id, forwardId: f.id }); await refresh(); }
    catch (e) { setErr(String(e)); }
    setBusy('');
  }

  const input = {
    height: 28, padding: '0 8px', borderRadius: 7, border: '1px solid var(--md-outline)',
    background: 'var(--md-surface-container-low)', color: 'var(--md-on-surface)',
    fontFamily: "'JetBrains Mono',monospace", fontSize: 11, outline: 'none', boxSizing: 'border-box',
  };
  const btn = {
    height: 28, padding: '0 12px', borderRadius: 7, border: 0, cursor: 'pointer',
    background: 'var(--md-primary)', color: '#FFF', fontFamily: 'inherit', fontSize: 11,
  };

  return (
    <div style={{ padding: 12, height: '100%', display: 'flex', flexDirection: 'column', gap: 10, minHeight: 0 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap', fontSize: 11 }}>
        {['local', 'remote'].map(k => (
          <button key={k} onClick={() => setKind(k)} style={{
            height: 26, padding: '0 10px', borderRadius: 7, cursor: 'pointer',
            border: '1px solid ' + (kind === k ? 'var(--md-primary)' : 'var(--md-outline)'),
            background: kind === k ? 'var(--md-primary-container)' : 'transparent',
            color: kind === k ? 'var(--md-on-primary-container)' : 'var(--md-on-surface)',
            fontFamily: 'inherit', fontSize: 11,
          }}>{k === 'local' ? '本地 → 远端 (-L)' : '远端 → 本地 (-R)'}</button>
        ))}
        <span style={{ color: 'var(--md-on-surface-variant)' }}>
          {kind === 'local' ? '本机' : '服务器'}
        </span>
        <input style={{ ...input, width: 92 }} value={form.listen_host} onChange={e => setForm({ ...form, listen_host: e.target.value })} />
        <span style={{ color: 'var(--md-on-surface-variant)' }}>:</span>
        <input style={{ ...input, width: 58 }} value={form.listen_port} onChange={e => setForm({ ...form, listen_port: e.target.value })} />
        <span className="material-icons-round" style={{ fontSize: 14, color: 'var(--md-primary)' }}>arrow_forward</span>
        <span style={{ color: 'var(--md-on-surface-variant)' }}>
          {kind === 'local' ? '远端目标' : '本地目标'}
        </span>
        <input style={{ ...input, width: 110 }} value={form.dest_host} onChange={e => setForm({ ...form, dest_host: e.target.value })} />
        <span style={{ color: 'var(--md-on-surface-variant)' }}>:</span>
        <input style={{ ...input, width: 58 }} value={form.dest_port} onChange={e => setForm({ ...form, dest_port: e.target.value })} />
        <button style={{ ...btn, opacity: busy === 'add' ? .6 : 1 }} onClick={add} disabled={!session?.connected || busy === 'add'}>
          {busy === 'add' ? '启动中…' : '添加并启动'}
        </button>
        {!session?.connected && <span style={{ color: '#B45309' }}>会话需先连接</span>}
      </div>

      {err && <div style={{ fontSize: 11, color: '#B91C1C' }}>{err}</div>}

      <div style={{ flex: 1, overflow: 'auto', border: '1px solid var(--md-outline-variant)', borderRadius: 10 }}>
        {list.length === 0 && (
          <div style={{ padding: 16, fontSize: 12, color: 'var(--md-on-surface-variant)' }}>暂无转发规则</div>
        )}
        {list.map(f => (
          <div key={f.id} style={{
            display: 'grid', gridTemplateColumns: '1fr 70px 70px', alignItems: 'center', gap: 8,
            padding: '7px 10px', fontSize: 12, borderBottom: '1px solid var(--md-outline-variant)',
          }}>
            <span style={{ fontFamily: "'JetBrains Mono',monospace", display: 'flex', alignItems: 'center', gap: 6 }}>
              <span style={{
                width: 6, height: 6, borderRadius: '50%',
                background: f.running ? '#10B981' : '#9CA3AF',
              }} />
              <span style={{
                fontSize: 9, padding: '1px 5px', borderRadius: 4,
                background: 'var(--md-surface-container-high)', color: 'var(--md-on-surface-variant)',
              }}>{f.kind === 'remote' ? '远端→本地' : '本地→远端'}</span>
              {f.listen_host}:{f.listen_port} → {f.dest_host}:{f.dest_port}
            </span>
            <button onClick={() => toggle(f)} disabled={busy === f.id} style={{
              height: 24, borderRadius: 6, border: '1px solid var(--md-outline)', cursor: 'pointer',
              background: 'transparent', color: 'var(--md-on-surface)', fontFamily: 'inherit', fontSize: 11,
            }}>{f.running ? '停止' : '启动'}</button>
            <button onClick={() => del(f)} disabled={busy === f.id} style={{
              height: 24, borderRadius: 6, border: '1px solid var(--md-outline)', cursor: 'pointer',
              background: 'transparent', color: '#B91C1C', fontFamily: 'inherit', fontSize: 11,
            }}>删除</button>
          </div>
        ))}
      </div>
    </div>
  );
}

/** 日志归档：首次读取后由后端写入事件驱动刷新，不做轮询。 */
export function LogTab({ session, onSessionUpdated = async () => {} }) {
  const sessionId = session?.id;
  const [source, setSource] = useState('terminal');
  const [files, setFiles] = useState([]);
  const [name, setName] = useState('');
  const [content, setContent] = useState('');
  const [err, setErr] = useState('');
  const [enabling, setEnabling] = useState(false);
  const generationRef = useRef(0);
  const listRequestRef = useRef(0);
  const contentRequestRef = useRef(0);
  const selectedNameRef = useRef('');

  const load = useCallback(async (nextName) => {
    if (!sessionId || !nextName) return;
    const generation = generationRef.current;
    const request = ++contentRequestRef.current;
    selectedNameRef.current = nextName;
    setName(nextName);
    try {
      const nextContent = await invoke(
        source === 'ai' ? 'read_ai_terminal_log' : 'read_session_log',
        { sessionId, name: nextName },
      );
      if (generationRef.current !== generation || contentRequestRef.current !== request
        || selectedNameRef.current !== nextName) return;
      setContent(nextContent);
      setErr('');
    } catch (e) {
      if (generationRef.current === generation && contentRequestRef.current === request) {
        setErr(String(e));
      }
    }
  }, [sessionId, source]);

  const refresh = useCallback(async () => {
    if (!sessionId) {
      setFiles([]);
      setContent('');
      return;
    }
    const generation = generationRef.current;
    const request = ++listRequestRef.current;
    try {
      const nextFiles = await invoke(
        source === 'ai' ? 'list_ai_terminal_logs' : 'list_session_logs',
        { sessionId },
      );
      if (generationRef.current !== generation || listRequestRef.current !== request) return;
      setFiles(nextFiles);
      const currentName = selectedNameRef.current;
      const nextName = nextFiles.some(file => file.name === currentName)
        ? currentName : (nextFiles[0]?.name || '');
      if (nextName !== currentName) {
        selectedNameRef.current = nextName;
        setName(nextName);
        setContent('');
      }
      if (nextName) await load(nextName);
      else {
        ++contentRequestRef.current;
        setContent('');
        setErr('');
      }
    } catch (e) {
      if (generationRef.current === generation && listRequestRef.current === request) {
        setErr(String(e));
      }
    }
  }, [sessionId, load, source]);

  useEffect(() => {
    const generationCounter = generationRef;
    const listRequestCounter = listRequestRef;
    const contentRequestCounter = contentRequestRef;
    const generation = ++generationCounter.current;
    let active = true;
    selectedNameRef.current = '';
    setFiles([]);
    setName('');
    setContent('');
    setErr('');
    if (!sessionId) return () => { active = false; };

    const unlisten = listen('log-appended', event => {
      if (active && generationCounter.current === generation
        && event.payload?.session_id === sessionId) refresh();
    });
    refresh();
    return () => {
      active = false;
      ++generationCounter.current;
      ++listRequestCounter.current;
      ++contentRequestCounter.current;
      unlisten.then(dispose => dispose()).catch(() => {});
    };
  }, [sessionId, refresh]);

  async function enableLogging() {
    if (!sessionId || session?.log_enabled || enabling) return;
    setEnabling(true);
    setErr('');
    let updated = false;
    try {
      await invoke('update_session', { sessionId, config: { log_enabled: true } });
      updated = true;
      if (session.connected) {
        await invoke('disconnect_session', { sessionId });
        await invoke('connect_session', { sessionId });
      }
    } catch (e) {
      setErr(updated ? `日志已启用，但重新连接失败：${e}` : `启用终端日志失败：${e}`);
    } finally {
      try { await onSessionUpdated(); } catch { /* 列表刷新失败不覆盖操作结果 */ }
      await refresh();
      setEnabling(false);
    }
  }

  function fmtSize(n) {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  if (!session) {
    return <div style={{ padding: 16, fontSize: 12, color: 'var(--md-on-surface-variant)' }}>未选择会话</div>;
  }

  return (
    <div style={{ padding: 12, height: '100%', display: 'flex', flexDirection: 'column', gap: 8, minHeight: 0 }}>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', border: '1px solid var(--md-outline)', borderRadius: 7, overflow: 'hidden' }}>
          {[
            { id: 'terminal', label: '终端日志' },
            { id: 'ai', label: 'AI 日志' },
          ].map(item => (
            <button key={item.id} onClick={() => setSource(item.id)} style={{
              height: 26, padding: '0 10px', border: 0, cursor: 'pointer',
              fontFamily: 'inherit', fontSize: 11,
              background: source === item.id ? 'var(--md-primary-container)' : 'transparent',
              color: source === item.id ? 'var(--md-on-primary-container)' : 'var(--md-on-surface)',
            }}>{item.label}</button>
          ))}
        </div>
        {source === 'terminal' && !session.log_enabled && (
          <button onClick={enableLogging} disabled={enabling} style={{
            height: 26, padding: '0 10px', borderRadius: 7, border: 0,
            background: 'var(--md-primary)', color: 'var(--md-on-primary)', fontFamily: 'inherit', fontSize: 11,
            cursor: enabling ? 'wait' : 'pointer', opacity: enabling ? .65 : 1,
          }}>{enabling ? '正在启用…' : session.connected ? '启用日志并重连' : '启用终端日志'}</button>
        )}
        <button onClick={refresh} style={{
          height: 26, padding: '0 10px', borderRadius: 7, border: '1px solid var(--md-outline)',
          background: 'transparent', color: 'var(--md-on-surface)', fontFamily: 'inherit', fontSize: 11, cursor: 'pointer',
        }}>刷新</button>
        <button onClick={() => invoke('open_logs_dir').catch(() => {})} style={{
          height: 26, padding: '0 10px', borderRadius: 7, border: '1px solid var(--md-outline)',
          background: 'transparent', color: 'var(--md-on-surface)', fontFamily: 'inherit', fontSize: 11, cursor: 'pointer',
        }}>打开日志目录</button>
        <span style={{ fontSize: 11, color: 'var(--md-on-surface-variant)' }}>
          {files.length} 个文件 · {name || '未选择'} · {source === 'ai'
            ? 'AI 回显始终记录'
            : (session.log_enabled ? '本会话已开启' : '本会话未开启')}
        </span>
      </div>
      {err && <div style={{ fontSize: 11, color: '#B91C1C' }}>{err}</div>}
      <div style={{ flex: 1, display: 'grid', gridTemplateColumns: '220px 1fr', gap: 8, minHeight: 0 }}>
        <div style={{ overflow: 'auto', border: '1px solid var(--md-outline-variant)', borderRadius: 10 }}>
          {files.length === 0 && (
            <div style={{ padding: 12, fontSize: 11, color: 'var(--md-on-surface-variant)' }}>
              {source === 'ai'
                ? '还没有 AI 回显日志。AI 接入并执行命令后会自动记录命令、输出和状态，应用重启后仍可查阅。'
                : session?.log_enabled
                  ? session.connected ? '当前连接还没有日志文件，请点击上方「刷新」；若刚修改配置，请重新连接。' : '连接会话后会自动创建日志文件并写入输出。'
                  : '此会话未开启终端日志。点击上方按钮启用；全局默认值可在「设置 → 日志存储」调整。'}
            </div>
          )}
          {files.map(f => (
            <div key={f.name} onClick={() => load(f.name)} style={{
              padding: '6px 10px', fontSize: 11, cursor: 'pointer',
              fontFamily: "'JetBrains Mono',monospace",
              background: name === f.name ? 'var(--md-primary-container)' : 'transparent',
              color: name === f.name ? 'var(--md-on-primary-container)' : 'var(--md-on-surface)',
              borderBottom: '1px solid var(--md-outline-variant)',
            }}>
              <div>{f.name}</div>
              <div style={{ fontSize: 10, opacity: .7 }}>{fmtSize(f.size)}</div>
            </div>
          ))}
        </div>
        <pre style={{
          margin: 0, padding: 10, overflow: 'auto', borderRadius: 10,
          border: '1px solid var(--md-outline-variant)', background: 'var(--md-surface-container-low)',
          fontSize: 11, lineHeight: 1.5, fontFamily: "'JetBrains Mono',monospace",
          color: 'var(--md-on-surface)', whiteSpace: 'pre-wrap', wordBreak: 'break-all',
        }}>{content || '（空）'}</pre>
      </div>
    </div>
  );
}

/** 快速命令库：点击即在当前会话执行 */
function QuickCmdTab({ session }) {
  const [list, setList] = useState([]);
  const [err, setErr] = useState('');
  const [form, setForm] = useState({ label: '', command: '', group: '自定义' });

  async function refresh() {
    try { setList(await invoke('list_quick_commands')); } catch (e) { setErr(String(e)); }
  }
  useEffect(() => { refresh(); }, []);

  async function run(c) {
    if (!session?.connected) { setErr('会话未连接'); return; }
    try {
      await invoke('run_quick_command', { sessionId: session.id, command: c.command });
      setErr('');
    } catch (e) { setErr(String(e)); }
  }

  async function add() {
    if (!form.label.trim() || !form.command.trim()) { setErr('名称和命令都要填'); return; }
    try {
      await invoke('save_quick_command', {
        command: {
          id: `qc-${Date.now()}`,
          label: form.label.trim(),
          command: form.command.trim(),
          group: form.group.trim() || '自定义',
        },
      });
      setForm({ label: '', command: '', group: '自定义' });
      setErr('');
      await refresh();
    } catch (e) { setErr(String(e)); }
  }

  async function del(id) {
    try { await invoke('delete_quick_command', { id }); await refresh(); } catch (e) { setErr(String(e)); }
  }

  const groups = {};
  list.forEach(c => {
    const g = c.group || '默认';
    (groups[g] = groups[g] || []).push(c);
  });

  const input = {
    height: 28, padding: '0 8px', borderRadius: 7, border: '1px solid var(--md-outline)',
    background: 'var(--md-surface-container-low)', color: 'var(--md-on-surface)',
    fontFamily: 'inherit', fontSize: 11, outline: 'none', boxSizing: 'border-box',
  };

  return (
    <div style={{ padding: 12, height: '100%', display: 'flex', flexDirection: 'column', gap: 8, minHeight: 0 }}>
      <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
        <input style={{ ...input, width: 130 }} placeholder="名称" value={form.label} onChange={e => setForm({ ...form, label: e.target.value })} />
        <input style={{ ...input, flex: 1, minWidth: 180, fontFamily: "'JetBrains Mono',monospace" }} placeholder="命令，例如 df -h" value={form.command} onChange={e => setForm({ ...form, command: e.target.value })} />
        <input style={{ ...input, width: 90 }} placeholder="分组" value={form.group} onChange={e => setForm({ ...form, group: e.target.value })} />
        <button onClick={add} style={{
          height: 28, padding: '0 12px', borderRadius: 7, border: 0, cursor: 'pointer',
          background: 'var(--md-primary)', color: '#FFF', fontFamily: 'inherit', fontSize: 11,
        }}>保存</button>
      </div>

      {err && <div style={{ fontSize: 11, color: '#B91C1C' }}>{err}</div>}

      <div style={{ flex: 1, overflow: 'auto', display: 'flex', flexDirection: 'column', gap: 10 }}>
        {Object.entries(groups).map(([g, items]) => (
          <div key={g}>
            <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: '.06em', color: 'var(--md-on-surface-variant)', marginBottom: 4 }}>
              {g}
            </div>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
              {items.map(c => (
                <div key={c.id} style={{
                  display: 'flex', alignItems: 'center', gap: 6, padding: '5px 8px',
                  borderRadius: 8, border: '1px solid var(--md-outline-variant)',
                  background: 'var(--md-surface-container-low)', fontSize: 11,
                }}>
                  <button onClick={() => run(c)} title={c.command} style={{
                    border: 0, background: 'transparent', color: 'var(--md-primary)',
                    fontFamily: 'inherit', fontSize: 11, cursor: 'pointer', padding: 0, fontWeight: 500,
                  }}>{c.label}</button>
                  <span style={{ fontFamily: "'JetBrains Mono',monospace", fontSize: 10, color: 'var(--md-on-surface-variant)' }}>
                    {c.command.length > 26 ? `${c.command.slice(0, 26)}…` : c.command}
                  </span>
                  <button onClick={() => del(c.id)} title="删除" style={{
                    border: 0, background: 'transparent', color: 'var(--md-on-surface-variant)',
                    cursor: 'pointer', fontSize: 11, padding: 0,
                  }}>✕</button>
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

