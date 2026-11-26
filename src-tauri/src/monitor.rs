//! 资源监控 — 每个会话使用独立 task 和 SSH 连接采样。
//!
//! 支持 Linux、Windows、macOS 与常见 BSD。采样失败会保留旧数据，并通过快照元数据明确错误和陈旧状态。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use russh::ChannelMsg;
use serde::Serialize;
use tauri::Emitter;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::session::{Handle, SessionHandle};
use crate::state::AppState;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const EXEC_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const STALE_AFTER: Duration = Duration::from_secs(3);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiskUsage {
    pub filesystem: String,
    pub mount_point: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub used_pct: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcessUsage {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f64,
    pub mem_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct MonitorCapabilities {
    pub disk_by_mount: bool,
    pub process_top: bool,
    pub load_average: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorSnapshot {
    pub session_id: String,
    pub ts: DateTime<Utc>,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub disk_pct: f64,
    pub disk_used_gb: u64,
    pub disk_total_gb: u64,
    pub disks: Vec<DiskUsage>,
    pub top_cpu_processes: Vec<ProcessUsage>,
    pub top_mem_processes: Vec<ProcessUsage>,
    pub capabilities: MonitorCapabilities,
    pub net_in_bytes_per_s: u64,
    pub net_out_bytes_per_s: u64,
    pub load_1m: f64,
    pub load_5m: f64,
    pub load_15m: f64,
    pub online: bool,
    pub cores: u32,
    pub hostname: String,
    pub last_success_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub last_error: Option<String>,
    pub platform: String,
    pub baseline_ready: bool,
}

impl Default for MonitorSnapshot {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            ts: Utc::now(),
            cpu_pct: 0.0,
            mem_pct: 0.0,
            mem_used_mb: 0,
            mem_total_mb: 0,
            disk_pct: 0.0,
            disk_used_gb: 0,
            disk_total_gb: 0,
            disks: Vec::new(),
            top_cpu_processes: Vec::new(),
            top_mem_processes: Vec::new(),
            capabilities: MonitorCapabilities::default(),
            net_in_bytes_per_s: 0,
            net_out_bytes_per_s: 0,
            load_1m: 0.0,
            load_5m: 0.0,
            load_15m: 0.0,
            online: false,
            cores: 0,
            hostname: String::new(),
            last_success_at: None,
            stale: false,
            last_error: None,
            platform: "unknown".into(),
            baseline_ready: false,
        }
    }
}

/// 用于计算 CPU / 网络速率的上一次采样。
#[derive(Default)]
pub struct PrevSample {
    cpu_total: u64,
    cpu_idle: u64,
    rx: u64,
    tx: u64,
    at: Option<Instant>,
    has_cpu: bool,
    has_net: bool,
}

/// POSIX 采集脚本。macOS/BSD 先归一化为 Linux 风格的计数器，解析器无需依赖远端工具的具体表头。
pub const SAMPLE_CMD: &str = r#"LC_ALL=C
export LC_ALL
echo @@PLATFORM
platform=$(uname -s 2>/dev/null || echo unknown)
echo "$platform"
case "$platform" in
Linux)
    echo @@LOAD
    cat /proc/loadavg 2>/dev/null
    echo @@STAT
    head -1 /proc/stat 2>/dev/null
    echo @@MEM
    grep -E '^(MemTotal|MemAvailable):' /proc/meminfo 2>/dev/null
    echo @@NET
    cat /proc/net/dev 2>/dev/null
    echo @@PS
    (ps -eo pid=,pcpu=,pmem=,comm= 2>/dev/null || ps -o pid,pcpu,pmem,comm 2>/dev/null)
    ;;
Darwin)
    echo @@LOAD
    sysctl -n vm.loadavg 2>/dev/null | awk '{gsub(/[{}]/, ""); print $1, $2, $3}'
    echo @@CPU
    top -l 2 -n 0 -s 1 2>/dev/null | awk '/CPU usage/ {line=$0} END {if (line != "") {sub(/^.*sys, /, "", line); sub(/% idle.*$/, "", line); printf "%.3f\n", 100-line}}'
    echo @@MEM
    total=$(sysctl -n hw.memsize 2>/dev/null)
    vm_stat 2>/dev/null | awk -v total="$total" '
        /page size of/ {pagesize=$8}
        /Pages free:/ {value=$3; gsub(/\./, "", value); free=value}
        /Pages inactive:/ {value=$3; gsub(/\./, "", value); inactive=value}
        /Pages speculative:/ {value=$3; gsub(/\./, "", value); speculative=value}
        END {
            if (total > 0 && pagesize > 0 && free != "") {
                available=(free+inactive+speculative)*pagesize
                if (available > total) available=total
                printf "MemTotal: %.0f kB\nMemAvailable: %.0f kB\n", total/1024, available/1024
            }
        }'
    echo @@NET
    netstat -ibn 2>/dev/null | awk '
        NR == 1 {
            for (i=1; i<=NF; i++) {
                if ($i == "Name") name=i
                if ($i == "Network") network=i
                if ($i == "Ibytes") ibytes=i
                if ($i == "Obytes") obytes=i
            }
            next
        }
        name && network && ibytes && obytes && $network ~ /^<Link#/ && $name !~ /^lo[0-9]*$/ {
            rx += $ibytes; tx += $obytes; seen=1
        }
        END {if (seen) printf "all: %.0f 0 0 0 0 0 0 0 %.0f 0 0 0 0 0 0 0\n", rx, tx}'
    echo @@PS
    (ps -axo pid=,pcpu=,pmem=,comm= 2>/dev/null || ps -axo pid,pcpu,pmem,comm 2>/dev/null)
    ;;
FreeBSD|OpenBSD|NetBSD|DragonFly)
    echo @@LOAD
    sysctl -n vm.loadavg 2>/dev/null | awk '{gsub(/[{}]/, ""); print $1, $2, $3}'
    echo @@STAT
    case "$platform" in
    OpenBSD|NetBSD)
        sysctl -n kern.cp_time 2>/dev/null | awk '{gsub(/,/, " ")} NF >= 6 {print "cpu " $1 " " $2 " " $3 " " $6 " 0 " $5 " " $4}'
        ;;
    *)
        sysctl -n kern.cp_time 2>/dev/null | awk '{gsub(/,/, " ")} NF >= 5 {print "cpu " $1 " " $2 " " $3 " " $5 " 0 " $4 " 0"}'
        ;;
    esac
    echo @@MEM
    total=$(sysctl -n hw.physmem 2>/dev/null)
    pagesize=$(sysctl -n hw.pagesize 2>/dev/null)
    if [ "$platform" = FreeBSD ]; then
        free=$(sysctl -n vm.stats.vm.v_free_count 2>/dev/null)
        inactive=$(sysctl -n vm.stats.vm.v_inactive_count 2>/dev/null)
        cache=$(sysctl -n vm.stats.vm.v_cache_count 2>/dev/null)
        available=$(awk -v free="$free" -v inactive="$inactive" -v cache="$cache" 'BEGIN {if (free != "") print free+inactive+cache}')
    else
        available=$(vmstat -s 2>/dev/null | awk '/pages free$/ {free=$1} /pages inactive$/ {inactive=$1} END {if (free != "") print free+inactive}')
    fi
    awk -v total="$total" -v available="$available" -v pagesize="$pagesize" 'BEGIN {
        if (total > 0 && available != "" && pagesize > 0) {
            bytes=available*pagesize
            if (bytes > total) bytes=total
            printf "MemTotal: %.0f kB\nMemAvailable: %.0f kB\n", total/1024, bytes/1024
        }
    }'
    echo @@NET
    netstat -ibn 2>/dev/null | awk '
        NR == 1 {
            for (i=1; i<=NF; i++) {
                if ($i == "Name") name=i
                if ($i == "Network") network=i
                if ($i == "Ibytes") ibytes=i
                if ($i == "Obytes") obytes=i
            }
            next
        }
        name && network && ibytes && obytes && $network ~ /^<Link#/ && $name !~ /^lo[0-9]*$/ {
            rx += $ibytes; tx += $obytes; seen=1
        }
        END {if (seen) printf "all: %.0f 0 0 0 0 0 0 0 %.0f 0 0 0 0 0 0 0\n", rx, tx}'
    echo @@PS
    (ps -axo pid=,pcpu=,pmem=,comm= 2>/dev/null || ps -axo pid,pcpu,pmem,comm 2>/dev/null)
    ;;
*)
    echo @@DF
    df -Pk 2>/dev/null
    ;;
esac
if [ "$platform" != SunOS ] && [ "$platform" != AIX ]; then
    case "$platform" in
    Linux|Darwin|FreeBSD|OpenBSD|NetBSD|DragonFly)
        echo @@DF
        df -Pk 2>/dev/null
        ;;
    esac
fi
echo @@HOST
hostname 2>/dev/null
echo @@NPROC
(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null)"#;

const WINDOWS_PROBE_CMD: &str = "cmd.exe /d /s /c \"echo Windows\"";

/// 所有数值均转为不变区域性，输出与 POSIX 分支共享相同的分节格式。
const WINDOWS_SAMPLE_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
[Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::InvariantCulture
[Threading.Thread]::CurrentThread.CurrentUICulture = [Globalization.CultureInfo]::InvariantCulture
Write-Output '@@PLATFORM'
Write-Output 'Windows'
Write-Output '@@CPU'
try {
    $cpu = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'"
    if ($null -ne $cpu) { Write-Output ([double]$cpu.PercentProcessorTime) }
} catch {}
Write-Output '@@MEM'
$os = $null
try { $os = Get-CimInstance Win32_OperatingSystem } catch {}
if ($null -ne $os) {
    Write-Output ('MemTotal: {0} kB' -f [uint64]$os.TotalVisibleMemorySize)
    Write-Output ('MemAvailable: {0} kB' -f [uint64]$os.FreePhysicalMemory)
}
Write-Output '@@DF'
try {
    Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | ForEach-Object {
        if ($null -ne $_.Size -and $null -ne $_.FreeSpace -and [double]$_.Size -gt 0) {
            $totalKb = [math]::Floor([double]$_.Size / 1024)
            $freeKb = [math]::Floor([double]$_.FreeSpace / 1024)
            $usedKb = [math]::Max(0, $totalKb - $freeKb)
            Write-Output ('{0} {1} {2} {3} 0% {4}' -f $_.DeviceID, [uint64]$totalKb, [uint64]$usedKb, [uint64]$freeKb, ($_.DeviceID + '\'))
        }
    }
} catch {}
Write-Output '@@NET'
try {
    $interfaces = @(Get-CimInstance Win32_PerfRawData_Tcpip_NetworkInterface | Where-Object {
        $_.Name -ne '_Total' -and $_.Name -notmatch 'loopback|isatap|teredo'
    })
    $rx = ($interfaces | Measure-Object -Property BytesReceivedPersec -Sum).Sum
    $tx = ($interfaces | Measure-Object -Property BytesSentPersec -Sum).Sum
    if ($null -ne $rx -and $null -ne $tx) {
        Write-Output ('all: {0} 0 0 0 0 0 0 0 {1} 0 0 0 0 0 0 0' -f [uint64]$rx, [uint64]$tx)
    }
} catch {}
Write-Output '@@PS'
try {
    $totalKb = if ($null -ne $os) { [double]$os.TotalVisibleMemorySize } else { 0 }
    Get-CimInstance Win32_PerfFormattedData_PerfProc_Process | Where-Object {
        $_.Name -ne '_Total' -and $_.Name -ne 'Idle' -and [uint32]$_.IDProcess -gt 0
    } | ForEach-Object {
        $memPct = if ($totalKb -gt 0) { ([double]$_.WorkingSet / 1024) * 100 / $totalKb } else { 0 }
        Write-Output ('{0} {1} {2} {3}' -f [uint32]$_.IDProcess, [double]$_.PercentProcessorTime, $memPct, $_.Name)
    }
} catch {}
Write-Output '@@HOST'
Write-Output $env:COMPUTERNAME
Write-Output '@@NPROC'
Write-Output ([Environment]::ProcessorCount)"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    Posix,
    Windows,
}

fn windows_sample_command() -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        WINDOWS_SAMPLE_SCRIPT
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    format!(
        "cmd.exe /d /v:on /s /c \"powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded} & echo {RC_MARK}!ERRORLEVEL!\""
    )
}

fn supported_platform(platform: &str) -> bool {
    matches!(
        platform,
        "linux" | "windows" | "darwin" | "freebsd" | "openbsd" | "netbsd" | "dragonfly"
    )
}

struct MonitorTask {
    cancel: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

/// 启动监控协调器；协调器只管理 task，不在会话之间串行执行 SSH 命令。
pub fn run_loop<R: tauri::Runtime>(state: AppState, app: tauri::AppHandle<R>) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("创建监控运行时失败: {error}");
                return;
            }
        };
        runtime.block_on(tokio::task::LocalSet::new().run_until(supervise(state, app)));
    });
}

async fn supervise<R: tauri::Runtime>(state: AppState, app: tauri::AppHandle<R>) {
    let mut tasks: HashMap<String, MonitorTask> = HashMap::new();
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let sessions: Vec<(String, Arc<SessionHandle>)> = {
            let inner = state.inner.read();
            inner
                .sessions
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().clone()))
                .collect()
        };
        let connected: HashSet<String> = sessions
            .iter()
            .filter(|(_, handle)| handle.connected.load(Ordering::Relaxed))
            .map(|(session_id, _)| session_id.clone())
            .collect();

        tasks.retain(|session_id, task| {
            if connected.contains(session_id) && !task.join.is_finished() {
                true
            } else {
                let _ = task.cancel.send(true);
                false
            }
        });

        for (session_id, handle) in sessions {
            if !connected.contains(&session_id) || tasks.contains_key(&session_id) {
                continue;
            }
            let (cancel, cancelled) = watch::channel(false);
            let join = tokio::task::spawn_local(monitor_session(
                state.clone(),
                app.clone(),
                session_id.clone(),
                handle,
                cancelled,
            ));
            tasks.insert(session_id, MonitorTask { cancel, join });
        }

        emit_newly_stale(&state, &app);
    }
}

async fn monitor_session<R: tauri::Runtime>(
    state: AppState,
    app: tauri::AppHandle<R>,
    session_id: String,
    session: Arc<SessionHandle>,
    mut cancelled: watch::Receiver<bool>,
) {
    let mut previous = PrevSample::default();
    let mut connection: Option<Arc<Handle>> = None;
    let mut sample_kind = None;
    let mut failures = 0u32;
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = cancelled.changed() => {
                if changed.is_err() || *cancelled.borrow() {
                    break;
                }
                continue;
            }
            _ = ticker.tick() => {}
        }
        if *cancelled.borrow() || !session.connected.load(Ordering::Relaxed) {
            break;
        }

        if connection.is_none() {
            let cfg = session.config.read().clone();
            let result = tokio::select! {
                changed = cancelled.changed() => {
                    if changed.is_err() || *cancelled.borrow() {
                        break;
                    }
                    continue;
                }
                result = tokio::time::timeout(CONNECT_TIMEOUT, crate::session::connect_handle(&cfg)) => {
                    match result {
                        Ok(result) => result,
                        Err(_) => Err("监控 SSH 连接超时".into()),
                    }
                }
            };
            match result {
                Ok(handle) => connection = Some(Arc::new(handle)),
                Err(error) => {
                    publish_failure(&state, &app, &session_id, None, error, &session);
                    failures = failures.saturating_add(1);
                    if !wait_backoff(&mut cancelled, backoff_delay(failures)).await {
                        break;
                    }
                    continue;
                }
            }
        }

        if !session.connected.load(Ordering::Relaxed) {
            break;
        }
        let exec_handle = connection.as_ref().expect("connection checked").clone();
        let result = tokio::select! {
            changed = cancelled.changed() => {
                if changed.is_err() || *cancelled.borrow() {
                    break;
                }
                continue;
            }
            result = tokio::time::timeout(EXEC_TIMEOUT, exec_collect(&exec_handle, EXEC_TIMEOUT, &mut sample_kind)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err("监控采样命令超时".into()),
                }
            }
        };

        match result {
            Ok(out) => {
                let sampled_at = Instant::now();
                match parse_sample_at(&out, &mut previous, sampled_at) {
                    Some(mut snapshot)
                        if supported_platform(&snapshot.platform) && snapshot.mem_total_mb > 0 =>
                    {
                        failures = 0;
                        snapshot.session_id = session_id.clone();
                        snapshot.online = true;
                        snapshot.stale = false;
                        snapshot.last_error = None;
                        publish(&state, &app, snapshot);
                    }
                    Some(snapshot) if supported_platform(&snapshot.platform) => {
                        let platform = snapshot.platform.clone();
                        connection = None;
                        publish_failure(
                            &state,
                            &app,
                            &session_id,
                            Some(platform),
                            "监控采样输出不完整：缺少有效内存数据".into(),
                            &session,
                        );
                        failures = failures.saturating_add(1);
                        if !wait_backoff(&mut cancelled, backoff_delay(failures)).await {
                            break;
                        }
                    }
                    Some(snapshot) => {
                        let platform = snapshot.platform.clone();
                        let error = if platform == "unknown" {
                            "无法识别远端平台或采样输出不完整".to_string()
                        } else {
                            format!("暂不支持的平台: {platform}")
                        };
                        connection = None;
                        publish_failure(&state, &app, &session_id, Some(platform), error, &session);
                        failures = failures.saturating_add(1);
                        if !wait_backoff(&mut cancelled, backoff_delay(failures)).await {
                            break;
                        }
                    }
                    None => {
                        connection = None;
                        publish_failure(
                            &state,
                            &app,
                            &session_id,
                            None,
                            "监控采样输出为空".into(),
                            &session,
                        );
                        failures = failures.saturating_add(1);
                        if !wait_backoff(&mut cancelled, backoff_delay(failures)).await {
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                connection = None;
                publish_failure(&state, &app, &session_id, None, error, &session);
                failures = failures.saturating_add(1);
                if !wait_backoff(&mut cancelled, backoff_delay(failures)).await {
                    break;
                }
            }
        }
    }
}

async fn wait_backoff(cancelled: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        changed = cancelled.changed() => changed.is_ok() && !*cancelled.borrow(),
        _ = tokio::time::sleep(delay) => !*cancelled.borrow(),
    }
}

fn backoff_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    Duration::from_secs((1u64 << shift).min(MAX_BACKOFF.as_secs()))
}

fn publish<R: tauri::Runtime>(
    state: &AppState,
    app: &tauri::AppHandle<R>,
    snapshot: MonitorSnapshot,
) {
    state
        .inner
        .write()
        .monitor_samples
        .insert(snapshot.session_id.clone(), snapshot.clone());
    let _ = app.emit("monitor-update", snapshot);
}

fn publish_failure<R: tauri::Runtime>(
    state: &AppState,
    app: &tauri::AppHandle<R>,
    session_id: &str,
    platform: Option<String>,
    error: String,
    session: &SessionHandle,
) {
    let previous = {
        let inner = state.inner.read();
        inner
            .monitor_samples
            .get(session_id)
            .map(|entry| entry.clone())
    };
    let mut snapshot = previous.unwrap_or_default();
    let now = Utc::now();
    snapshot.session_id = session_id.to_string();
    snapshot.ts = now;
    snapshot.online = session.connected.load(Ordering::Relaxed);
    snapshot.stale = is_stale(snapshot.last_success_at.as_ref(), now);
    snapshot.last_error = Some(error);
    if let Some(platform) = platform {
        snapshot.platform = platform;
    }
    publish(state, app, snapshot);
}

fn is_stale(last_success_at: Option<&DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_success_at {
        Some(last) => {
            now.signed_duration_since(last)
                > chrono::Duration::from_std(STALE_AFTER)
                    .unwrap_or_else(|_| chrono::Duration::seconds(3))
        }
        None => true,
    }
}

fn emit_newly_stale<R: tauri::Runtime>(state: &AppState, app: &tauri::AppHandle<R>) {
    let session_ids: Vec<String> = {
        let inner = state.inner.read();
        inner
            .monitor_samples
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    };
    let now = Utc::now();
    for session_id in session_ids {
        let snapshot = {
            let inner = state.inner.read();
            let Some(mut entry) = inner.monitor_samples.get_mut(&session_id) else {
                continue;
            };
            if entry.stale || !is_stale(entry.last_success_at.as_ref(), now) {
                continue;
            }
            entry.stale = true;
            entry.clone()
        };
        let _ = app.emit("monitor-update", snapshot);
    }
}

pub fn get_snapshot(state: &AppState, session_id: &str) -> Option<MonitorSnapshot> {
    let inner = state.inner.read();
    inner.monitor_samples.get(session_id).map(|entry| {
        let mut snapshot = entry.clone();
        snapshot.stale = is_stale(snapshot.last_success_at.as_ref(), Utc::now());
        snapshot
    })
}

const RC_MARK: &str = "__FORGE_MONITOR_RC__";

async fn exec_collect(
    handle: &Handle,
    timeout: Duration,
    sample_kind: &mut Option<SampleKind>,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let kind = if let Some(kind) = *sample_kind {
        kind
    } else {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("监控采样命令超时".into());
        }
        let windows = exec_command(handle, WINDOWS_PROBE_CMD, remaining)
            .await
            .map(|output| output.lines().any(|line| line.trim() == "Windows"))
            .unwrap_or(false);
        if windows {
            SampleKind::Windows
        } else {
            SampleKind::Posix
        }
    };

    let command = match kind {
        SampleKind::Posix => {
            format!("({SAMPLE_CMD}); __forge_rc=$?; printf '\\n{RC_MARK}%d\\n' \"$__forge_rc\"")
        }
        SampleKind::Windows => windows_sample_command(),
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err("监控采样命令超时".into());
    }
    let output = exec_command(handle, &command, remaining).await?;
    *sample_kind = Some(kind);
    Ok(output)
}

async fn exec_command(handle: &Handle, command: &str, timeout: Duration) -> Result<String, String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|error| format!("打开监控通道失败: {error}"))?;
    channel
        .exec(true, command.as_bytes())
        .await
        .map_err(|error| format!("启动监控采样失败: {error}"))?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut status = None;
    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => out.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => err.extend_from_slice(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status as i32),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
            _ = tokio::time::sleep_until(deadline) => {
                let _ = channel.close().await;
                return Err("监控采样命令超时".into());
            }
        }
    }

    let stdout = String::from_utf8_lossy(&out).to_string();
    let stderr = String::from_utf8_lossy(&err).to_string();
    let (marker_code, stdout) = parse_exit_marker(stdout);
    let code = marker_code
        .or(status)
        .unwrap_or(if stderr.trim().is_empty() { 0 } else { 1 });
    if code != 0 {
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            format!("监控采样命令退出码为 {code}")
        } else {
            format!("监控采样命令退出码为 {code}: {detail}")
        });
    }
    Ok(stdout)
}

fn parse_exit_marker(mut output: String) -> (Option<i32>, String) {
    let mut code = None;
    if let Some(position) = output.rfind(RC_MARK) {
        let after = &output[position + RC_MARK.len()..];
        let digits: String = after
            .trim_start()
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '-')
            .collect();
        code = digits.parse::<i32>().ok();
        let mut cut = position;
        while cut > 0 && matches!(output.as_bytes()[cut - 1], b'\n' | b'\r') {
            cut -= 1;
        }
        output.truncate(cut);
    }
    (code, output)
}

fn is_pseudo_mount(filesystem: &str, mount_point: &str) -> bool {
    let filesystem = filesystem.to_ascii_lowercase();
    let pseudo_filesystems = [
        "autofs",
        "binfmt_misc",
        "bpf",
        "cgroup",
        "cgroup2",
        "configfs",
        "debugfs",
        "devpts",
        "devtmpfs",
        "efivarfs",
        "fusectl",
        "hugetlbfs",
        "mqueue",
        "nfsd",
        "nsfs",
        "proc",
        "pstore",
        "ramfs",
        "rpc_pipefs",
        "securityfs",
        "sysfs",
        "tmpfs",
        "tracefs",
        "udev",
    ];
    pseudo_filesystems.contains(&filesystem.as_str())
        || mount_point == "/proc"
        || mount_point.starts_with("/proc/")
        || mount_point == "/sys"
        || mount_point.starts_with("/sys/")
        || matches!(
            mount_point,
            "/dev" | "/dev/pts" | "/dev/shm" | "/dev/mqueue"
        )
        || matches!(mount_point, "/run" | "/var/run")
}

fn parse_disks(input: &str) -> Vec<DiskUsage> {
    let mut by_mount = BTreeMap::new();
    for line in input.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 6 {
            continue;
        }
        let (Ok(total_kb), Ok(used_kb)) = (columns[1].parse::<u64>(), columns[2].parse::<u64>())
        else {
            continue;
        };
        let mount_point = columns[5..].join(" ");
        if total_kb == 0 || is_pseudo_mount(columns[0], &mount_point) {
            continue;
        }
        let used_pct = (used_kb as f64 / total_kb as f64 * 100.0).clamp(0.0, 100.0);
        by_mount.insert(
            mount_point.clone(),
            DiskUsage {
                filesystem: columns[0].to_string(),
                mount_point,
                total_kb,
                used_kb,
                used_pct,
            },
        );
    }
    by_mount.into_values().collect()
}

fn parse_processes(input: &str) -> Vec<ProcessUsage> {
    input
        .lines()
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            if columns.len() < 4 {
                return None;
            }
            let pid = columns[0].parse::<u32>().ok()?;
            let cpu_pct = columns[1].parse::<f64>().ok()?;
            let mem_pct = columns[2].parse::<f64>().ok()?;
            let name = columns[3..].join(" ");
            if name.is_empty() || !cpu_pct.is_finite() || !mem_pct.is_finite() {
                return None;
            }
            Some(ProcessUsage {
                pid,
                name,
                cpu_pct: cpu_pct.max(0.0),
                mem_pct: mem_pct.clamp(0.0, 100.0),
            })
        })
        .collect()
}

/// 解析采样命令的输出（保留原公开签名，输出格式由 SAMPLE_CMD 决定）。
#[cfg(test)]
pub fn parse_sample(out: &str, prev: &mut PrevSample) -> Option<MonitorSnapshot> {
    parse_sample_at(out, prev, Instant::now())
}

fn parse_sample_at(
    out: &str,
    prev: &mut PrevSample,
    sampled_at: Instant,
) -> Option<MonitorSnapshot> {
    if out.trim().is_empty() {
        return None;
    }

    let now = Utc::now();
    let mut snap = MonitorSnapshot {
        ts: now,
        online: true,
        last_success_at: Some(now),
        ..Default::default()
    };
    let mut cpu_delta_ready = false;
    let mut net_delta_ready = false;

    for section in out.split("@@") {
        let body = section.trim();
        if let Some(rest) = body.strip_prefix("PLATFORM") {
            snap.platform = rest
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("unknown")
                .trim()
                .to_ascii_lowercase();
            if snap.platform.is_empty() {
                snap.platform = "unknown".into();
            }
        } else if let Some(rest) = body.strip_prefix("LOAD") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if let (Some(load_1m), Some(load_5m), Some(load_15m)) = (
                parts.first().and_then(|value| value.parse::<f64>().ok()),
                parts.get(1).and_then(|value| value.parse::<f64>().ok()),
                parts.get(2).and_then(|value| value.parse::<f64>().ok()),
            ) {
                if load_1m.is_finite() && load_5m.is_finite() && load_15m.is_finite() {
                    snap.load_1m = load_1m;
                    snap.load_5m = load_5m;
                    snap.load_15m = load_15m;
                    snap.capabilities.load_average = true;
                }
            }
        } else if let Some(rest) = body.strip_prefix("CPU") {
            if let Some(cpu_pct) = rest
                .lines()
                .filter_map(|line| line.trim().parse::<f64>().ok())
                .find(|value| value.is_finite())
            {
                snap.cpu_pct = cpu_pct.clamp(0.0, 100.0);
                cpu_delta_ready = true;
            }
        } else if let Some(rest) = body.strip_prefix("STAT") {
            // cpu  user nice system idle iowait irq softirq steal
            let nums: Vec<u64> = rest
                .split_whitespace()
                .skip(1)
                .filter_map(|value| value.parse::<u64>().ok())
                .collect();
            if nums.len() >= 4 {
                let total: u64 = nums.iter().sum();
                let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
                if prev.has_cpu && total > prev.cpu_total && idle >= prev.cpu_idle {
                    let total_delta = (total - prev.cpu_total) as f64;
                    let idle_delta = (idle - prev.cpu_idle) as f64;
                    snap.cpu_pct = ((1.0 - idle_delta / total_delta) * 100.0).clamp(0.0, 100.0);
                    cpu_delta_ready = true;
                }
                prev.cpu_total = total;
                prev.cpu_idle = idle;
                prev.has_cpu = true;
            }
        } else if let Some(rest) = body.strip_prefix("MEM") {
            let mut total_kb = 0u64;
            let mut avail_kb = 0u64;
            for line in rest.lines() {
                let mut columns = line.split_whitespace();
                let key = columns.next().unwrap_or("");
                let value: u64 = columns
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                match key {
                    "MemTotal:" => total_kb = value,
                    "MemAvailable:" => avail_kb = value,
                    _ => {}
                }
            }
            if total_kb > 0 {
                let used_kb = total_kb.saturating_sub(avail_kb);
                snap.mem_total_mb = total_kb / 1024;
                snap.mem_used_mb = used_kb / 1024;
                snap.mem_pct = (used_kb as f64 / total_kb as f64 * 100.0).clamp(0.0, 100.0);
            }
        } else if let Some(rest) = body.strip_prefix("DF") {
            // GNU coreutils 与 BusyBox 的 df -Pk 均为六列，-P 避免长设备名换行。
            let disks = parse_disks(rest);
            if let Some(disk) = disks
                .iter()
                .find(|disk| disk.mount_point == "/")
                .or_else(|| disks.first())
                .cloned()
            {
                snap.disk_total_gb = disk.total_kb / 1024 / 1024;
                snap.disk_used_gb = disk.used_kb / 1024 / 1024;
                snap.disk_pct = disk.used_pct;
            }
            snap.disks = disks;
        } else if let Some(rest) = body.strip_prefix("NET") {
            let mut rx = 0u64;
            let mut tx = 0u64;
            let mut seen = false;
            for line in rest.lines() {
                let Some((interface, data)) = line.split_once(':') else {
                    continue;
                };
                let name = interface.trim();
                if name == "lo" || name.is_empty() || name.contains('|') {
                    continue;
                }
                let nums: Vec<u64> = data
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u64>().ok())
                    .collect();
                if nums.len() >= 9 {
                    rx = rx.saturating_add(nums[0]);
                    tx = tx.saturating_add(nums[8]);
                    seen = true;
                }
            }
            if seen {
                if prev.has_net && rx >= prev.rx && tx >= prev.tx {
                    if let Some(seconds) = prev
                        .at
                        .and_then(|at| sampled_at.checked_duration_since(at))
                        .map(|elapsed| elapsed.as_secs_f64())
                        .filter(|seconds| *seconds > 0.0)
                    {
                        snap.net_in_bytes_per_s = ((rx - prev.rx) as f64 / seconds) as u64;
                        snap.net_out_bytes_per_s = ((tx - prev.tx) as f64 / seconds) as u64;
                        net_delta_ready = true;
                    }
                }
                prev.rx = rx;
                prev.tx = tx;
                prev.at = Some(sampled_at);
                prev.has_net = true;
            }
        } else if let Some(rest) = body.strip_prefix("PS") {
            let processes = parse_processes(rest);
            let mut by_cpu = processes.clone();
            by_cpu.sort_by(|a, b| {
                b.cpu_pct
                    .total_cmp(&a.cpu_pct)
                    .then_with(|| a.pid.cmp(&b.pid))
            });
            by_cpu.truncate(5);
            let mut by_mem = processes;
            by_mem.sort_by(|a, b| {
                b.mem_pct
                    .total_cmp(&a.mem_pct)
                    .then_with(|| a.pid.cmp(&b.pid))
            });
            by_mem.truncate(5);
            snap.top_cpu_processes = by_cpu;
            snap.top_mem_processes = by_mem;
        } else if let Some(rest) = body.strip_prefix("HOST") {
            snap.hostname = rest
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
        } else if let Some(rest) = body.strip_prefix("NPROC") {
            snap.cores = rest.trim().parse().unwrap_or(0);
        }
    }

    snap.capabilities.disk_by_mount = supported_platform(&snap.platform) && !snap.disks.is_empty();
    snap.capabilities.process_top =
        supported_platform(&snap.platform) && !snap.top_cpu_processes.is_empty();
    snap.baseline_ready = cpu_delta_ready && net_delta_ready;
    Some(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(cpu: &str, eth0_rx: u64, eth0_tx: u64) -> String {
        format!(
            "@@PLATFORM\n\
             Linux\n\
             @@LOAD\n\
             0.10 0.20 0.30 1/100 123\n\
             @@STAT\n\
             cpu  {cpu}\n\
             @@MEM\n\
             MemTotal: 2048000 kB\n\
             MemAvailable: 512000 kB\n\
             @@DF\n\
             Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             proc 0 0 0 0% /proc\n\
             tmpfs 204800 1024 203776 1% /tmp\n\
             /dev/sda1 4194304 1048576 3145728 25% /\n\
             /dev/sdb1 20971520 10485760 10485760 50% /data\n\
             @@NET\n\
             Inter-| Receive | Transmit\n\
             face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n\
             lo: 900000 1 1 1 1 1 1 1 800000 1 1 1 1 1 1 1\n\
             eth0: {eth0_rx} 2 3 4 5 6 7 8 {eth0_tx} 10 11 12 13 14 15 16\n\
             @@PS\n\
             PID %CPU %MEM COMMAND\n\
             1 0.1 1.0 systemd\n\
             2 50.0 2.0 sshd\n\
             3 10.0 30.0 postgres\n\
             4 5.0 4.0 nginx worker\n\
             5 2.0 80.0 java\n\
             6 90.0 0.5 stress\n\
             @@HOST\n\
             forge-test\n\
             @@NPROC\n\
             8\n"
        )
    }

    fn instant_cpu_fixture(platform: &str, cpu: f64, rx: u64, tx: u64) -> String {
        fixture("100 0 50 850 0 0 0 0", rx, tx)
            .replacen("@@PLATFORM\nLinux", &format!("@@PLATFORM\n{platform}"), 1)
            .replacen(
                "@@STAT\ncpu  100 0 50 850 0 0 0 0",
                &format!("@@CPU\n{cpu}"),
                1,
            )
    }

    #[test]
    fn parses_full_fixture_and_computes_second_round_cpu() {
        let mut prev = PrevSample::default();
        let started_at = Instant::now();
        let first = parse_sample_at(
            &fixture("100 0 50 850 0 0 0 0", 1_000, 2_000),
            &mut prev,
            started_at,
        )
        .unwrap();

        assert!(first.online);
        assert!(!first.stale);
        assert!(first.last_error.is_none());
        assert!(first.last_success_at.is_some());
        assert_eq!(first.platform, "linux");
        assert!(!first.baseline_ready);
        assert_eq!(first.load_1m, 0.10);
        assert_eq!(first.load_5m, 0.20);
        assert_eq!(first.load_15m, 0.30);
        assert_eq!(first.cpu_pct, 0.0);
        assert_eq!(first.mem_total_mb, 2_000);
        assert_eq!(first.mem_used_mb, 1_500);
        assert_eq!(first.mem_pct, 75.0);
        assert_eq!(first.disk_total_gb, 4);
        assert_eq!(first.disk_used_gb, 1);
        assert_eq!(first.disk_pct, 25.0);
        assert_eq!(first.disks.len(), 2);
        assert!(first.disks.iter().all(|disk| disk.filesystem != "tmpfs"));
        assert!(first.capabilities.disk_by_mount);
        assert!(first.capabilities.process_top);
        assert_eq!(first.top_cpu_processes.len(), 5);
        assert_eq!(first.top_mem_processes.len(), 5);
        assert_eq!(first.top_cpu_processes[0].pid, 6);
        assert_eq!(first.top_mem_processes[0].pid, 5);
        assert_eq!(first.net_in_bytes_per_s, 0);
        assert_eq!(first.net_out_bytes_per_s, 0);
        assert_eq!(first.hostname, "forge-test");
        assert_eq!(first.cores, 8);

        let second = parse_sample_at(
            &fixture("150 0 100 950 0 0 0 0", 3_000, 6_000),
            &mut prev,
            started_at + SAMPLE_INTERVAL,
        )
        .unwrap();
        assert_eq!(second.cpu_pct, 50.0);
        assert!(second.net_in_bytes_per_s > 0);
        assert!(second.net_out_bytes_per_s > 0);
        assert!(second.baseline_ready);
    }

    #[test]
    fn busybox_style_disk_and_process_output_is_supported() {
        let openssh = fixture("100 0 50 850 0 0 0 0", 1_000, 2_000);
        let busybox = openssh
            .replace(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                 proc 0 0 0 0% /proc\n\
                 tmpfs 204800 1024 203776 1% /tmp\n\
                 /dev/sda1 4194304 1048576 3145728 25% /\n\
                 /dev/sdb1 20971520 10485760 10485760 50% /data",
                "Filesystem 1K-blocks Used Available Use% Mounted on\n\
                 devtmpfs 102400 0 102400 0% /dev\n\
                 /dev/mmcblk0p2 4194304 1048576 3145728 25% /\n\
                 tmpfs 204800 1024 203776 1% /tmp\n\
                 /dev/mmcblk0p1 1048576 262144 786432 25% /boot",
            )
            .replace("PID %CPU %MEM COMMAND", "PID CPU MEM COMMAND");
        let mut prev = PrevSample::default();
        let snapshot = parse_sample(&busybox, &mut prev).unwrap();

        assert_eq!(snapshot.disks.len(), 2);
        assert_eq!(snapshot.disks[0].mount_point, "/");
        assert_eq!(snapshot.disks[1].mount_point, "/boot");
        assert_eq!(snapshot.disks[0].filesystem, "/dev/mmcblk0p2");
        assert_eq!(snapshot.disk_total_gb, 4);
        assert_eq!(snapshot.top_cpu_processes.len(), 5);
        assert_eq!(snapshot.top_mem_processes.len(), 5);
        assert_eq!(snapshot.top_cpu_processes[0].name, "stress");
        assert_eq!(snapshot.top_mem_processes[0].name, "java");
        assert!(snapshot
            .top_cpu_processes
            .iter()
            .any(|process| process.name == "nginx worker"));
    }

    #[test]
    fn removable_disks_below_run_are_not_filtered_as_pseudo_mounts() {
        let disks = parse_disks(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             tmpfs 204800 1024 203776 1% /run\n\
             /dev/sdc1 1048576 262144 786432 25% /run/media/usb\n\
             tmpfs 204800 1024 203776 1% /var/run",
        );

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount_point, "/run/media/usb");
        assert_eq!(disks[0].filesystem, "/dev/sdc1");
    }

    #[test]
    fn loopback_is_excluded_from_network_counters() {
        let mut prev = PrevSample::default();
        parse_sample(&fixture("100 0 50 850 0 0 0 0", 1_000, 2_000), &mut prev).unwrap();

        assert_eq!(prev.rx, 1_000);
        assert_eq!(prev.tx, 2_000);
    }

    #[test]
    fn counter_wrap_does_not_create_cpu_or_network_spikes() {
        let mut prev = PrevSample {
            cpu_total: 10_000,
            cpu_idle: 8_000,
            rx: 5_000,
            tx: 6_000,
            at: Some(Instant::now()),
            has_cpu: true,
            has_net: true,
        };
        let snap = parse_sample(&fixture("10 0 10 80 0 0 0 0", 100, 200), &mut prev).unwrap();

        assert_eq!(snap.cpu_pct, 0.0);
        assert_eq!(snap.net_in_bytes_per_s, 0);
        assert_eq!(snap.net_out_bytes_per_s, 0);
        assert!(!snap.baseline_ready);
        assert_eq!(prev.cpu_total, 100);
        assert_eq!(prev.cpu_idle, 80);
        assert_eq!(prev.rx, 100);
        assert_eq!(prev.tx, 200);
    }

    #[test]
    fn empty_sample_returns_none() {
        let mut prev = PrevSample::default();
        assert!(parse_sample(" \n\t", &mut prev).is_none());
    }

    #[test]
    fn network_rates_use_real_elapsed_interval() {
        let mut prev = PrevSample::default();
        let started_at = Instant::now();
        parse_sample_at(
            &fixture("100 0 50 850 0 0 0 0", 1_000, 2_000),
            &mut prev,
            started_at,
        )
        .unwrap();
        let snapshot = parse_sample_at(
            &fixture("150 0 100 950 0 0 0 0", 3_000, 6_000),
            &mut prev,
            started_at + Duration::from_secs(2),
        )
        .unwrap();

        assert_eq!(snapshot.net_in_bytes_per_s, 1_000);
        assert_eq!(snapshot.net_out_bytes_per_s, 2_000);
        assert!(snapshot.baseline_ready);
    }

    #[test]
    fn stale_fixture_requires_more_than_three_periods_without_success() {
        let last_success = Utc::now();
        let threshold = chrono::Duration::from_std(STALE_AFTER).unwrap();
        assert!(!is_stale(Some(&last_success), last_success + threshold));
        assert!(is_stale(
            Some(&last_success),
            last_success + threshold + chrono::Duration::milliseconds(1)
        ));
        assert!(is_stale(None, last_success));
    }

    #[test]
    fn darwin_fixture_preserves_supported_disk_and_processes() {
        let mut prev = PrevSample::default();
        let darwin = fixture("100 0 50 850 0 0 0 0", 1_000, 2_000).replacen(
            "@@PLATFORM\nLinux",
            "@@PLATFORM\nDarwin",
            1,
        );
        let snapshot = parse_sample(&darwin, &mut prev).unwrap();

        assert_eq!(snapshot.platform, "darwin");
        assert_eq!(snapshot.disks.len(), 2);
        assert_eq!(snapshot.top_cpu_processes.len(), 5);
        assert!(snapshot.capabilities.disk_by_mount);
        assert!(snapshot.capabilities.process_top);
        assert!(snapshot.capabilities.load_average);
        assert!(!snapshot.baseline_ready);
    }

    #[test]
    fn darwin_instant_cpu_and_network_baselines_are_parsed() {
        let mut prev = PrevSample::default();
        let started_at = Instant::now();
        let first = parse_sample_at(
            &instant_cpu_fixture("Darwin", 12.5, 1_000, 2_000),
            &mut prev,
            started_at,
        )
        .unwrap();
        assert_eq!(first.cpu_pct, 12.5);
        assert!(!first.baseline_ready);
        assert!(first.capabilities.load_average);
        assert!(first.capabilities.disk_by_mount);
        assert!(first.capabilities.process_top);

        let second = parse_sample_at(
            &instant_cpu_fixture("Darwin", 25.0, 3_000, 6_000),
            &mut prev,
            started_at + SAMPLE_INTERVAL,
        )
        .unwrap();
        assert_eq!(second.cpu_pct, 25.0);
        assert_eq!(second.net_in_bytes_per_s, 2_000);
        assert_eq!(second.net_out_bytes_per_s, 4_000);
        assert!(second.baseline_ready);
    }

    #[test]
    fn windows_fixture_has_no_load_average_and_keeps_drive_details() {
        let mut prev = PrevSample::default();
        let started_at = Instant::now();
        let windows = instant_cpu_fixture("Windows", 37.5, 1_000, 2_000)
            .replacen("@@LOAD\n0.10 0.20 0.30 1/100 123\n", "", 1)
            .replacen(
                "/dev/sda1 4194304 1048576 3145728 25% /\n\
                 /dev/sdb1 20971520 10485760 10485760 50% /data",
                "C: 4194304 1048576 3145728 0% C:\\\n\
                 D: 20971520 10485760 10485760 0% D:\\",
                1,
            );
        let first = parse_sample_at(&windows, &mut prev, started_at).unwrap();

        assert_eq!(first.platform, "windows");
        assert_eq!(first.cpu_pct, 37.5);
        assert_eq!(first.mem_total_mb, 2_000);
        assert_eq!(first.disks[0].mount_point, "C:\\");
        assert_eq!(first.disk_total_gb, 4);
        assert!(first.capabilities.disk_by_mount);
        assert!(first.capabilities.process_top);
        assert!(!first.capabilities.load_average);
        assert!(!first.baseline_ready);

        let second = parse_sample_at(
            &instant_cpu_fixture("Windows", 50.0, 3_000, 6_000).replacen(
                "@@LOAD\n0.10 0.20 0.30 1/100 123\n",
                "",
                1,
            ),
            &mut prev,
            started_at + SAMPLE_INTERVAL,
        )
        .unwrap();
        assert_eq!(second.cpu_pct, 50.0);
        assert!(second.baseline_ready);
        assert!(!second.capabilities.load_average);
    }

    #[test]
    fn bsd_fixtures_share_normalized_cpu_memory_disk_and_process_metrics() {
        for platform in ["FreeBSD", "OpenBSD", "NetBSD", "DragonFly"] {
            let mut prev = PrevSample::default();
            let output = fixture("100 0 50 850 0 40 0 0", 1_000, 2_000).replacen(
                "@@PLATFORM\nLinux",
                &format!("@@PLATFORM\n{platform}"),
                1,
            );
            let snapshot = parse_sample(&output, &mut prev).unwrap();

            assert_eq!(snapshot.platform, platform.to_ascii_lowercase());
            assert_eq!(snapshot.mem_total_mb, 2_000);
            assert_eq!(snapshot.disks.len(), 2);
            assert_eq!(snapshot.top_cpu_processes.len(), 5);
            assert!(snapshot.capabilities.load_average);
            assert!(snapshot.capabilities.disk_by_mount);
            assert!(snapshot.capabilities.process_top);
            assert!(!snapshot.baseline_ready);
        }
    }

    #[test]
    fn missing_network_counters_do_not_create_a_zero_baseline() {
        let mut prev = PrevSample::default();
        let invalid = fixture("100 0 50 850 0 0 0 0", 1_000, 2_000).replace(
            "eth0: 1000 2 3 4 5 6 7 8 2000 10 11 12 13 14 15 16",
            "eth0: unavailable",
        );
        let first = parse_sample(&invalid, &mut prev).unwrap();
        assert!(!prev.has_net);
        assert!(!first.baseline_ready);

        let second =
            parse_sample(&fixture("150 0 100 950 0 0 0 0", 3_000, 6_000), &mut prev).unwrap();
        assert!(prev.has_net);
        assert!(!second.baseline_ready);
        assert_eq!(second.net_in_bytes_per_s, 0);
        assert_eq!(second.net_out_bytes_per_s, 0);
    }

    #[test]
    fn windows_encoded_command_round_trips_and_fits_cmd_limit() {
        use base64::Engine as _;

        let command = windows_sample_command();
        assert!(
            command.len() <= 8_000,
            "Windows 命令长度为 {}，超过 CMD 安全限制",
            command.len()
        );
        assert!(command.contains("!ERRORLEVEL!"));
        let encoded = command
            .split("-EncodedCommand ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), WINDOWS_SAMPLE_SCRIPT);
    }

    async fn wait_for_snapshot(
        state: &AppState,
        session_id: &str,
        timeout: Duration,
        predicate: impl Fn(&MonitorSnapshot) -> bool,
    ) -> MonitorSnapshot {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(snapshot) = get_snapshot(state, session_id) {
                    if predicate(&snapshot) {
                        return snapshot;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("等待真实监控快照超时")
    }

    fn next_monitor_event(
        receiver: &std::sync::mpsc::Receiver<(Instant, serde_json::Value)>,
        timeout: Duration,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> (Instant, serde_json::Value) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (received_at, payload) = receiver
                .recv_timeout(remaining)
                .expect("等待 monitor-update 事件超时");
            if predicate(&payload) {
                return (received_at, payload);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires a controlled real OpenSSH server and CHENA_MONITOR_E2E_MODE_FILE"]
    async fn real_openssh_monitor_events_timeout_backoff_stale_and_recovery() {
        use crate::state::{AIPolicyOverride, AuthConfig, SessionConfig};
        use std::fs;
        use tauri::Listener;

        let root = std::env::var_os("CHENA_E2E_ROOT")
            .map(std::path::PathBuf::from)
            .expect("CHENA_E2E_ROOT is required");
        let mode_file = std::env::var_os("CHENA_MONITOR_E2E_MODE_FILE")
            .map(std::path::PathBuf::from)
            .expect("CHENA_MONITOR_E2E_MODE_FILE is required");
        let config_dir = root.join(format!("monitor-config-{}", uuid::Uuid::new_v4()));
        std::env::set_var("CHENA_SHELL_TEST_CONFIG_DIR", &config_dir);
        fs::write(&mode_file, "ok").unwrap();
        let config = SessionConfig {
            id: "monitor-real-openssh".into(),
            name: "受控真实 OpenSSH 监控联调".into(),
            host: std::env::var("CHENA_MONITOR_E2E_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("CHENA_MONITOR_E2E_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(22224),
            username: std::env::var("CHENA_MONITOR_E2E_USER").unwrap_or_else(|_| "chena".into()),
            auth: AuthConfig::Key {
                private_key_path: root.join("chena_e2e_key").to_string_lossy().into_owned(),
                private_key_inline_ref: None,
                passphrase_ref: None,
            },
            proxy_jump: Vec::new(),
            group: "monitor-e2e".into(),
            ai_allowed: false,
            ai_policy: AIPolicyOverride::Inherit,
            ai_blocklist_regexes: Vec::new(),
            auto_connect: false,
            keepalive_secs: 15,
            connect_timeout_secs: 10,
            port_forwards: Vec::new(),
            log_enabled: false,
        };
        let session = Arc::new(SessionHandle::new(config, Vec::new(), Vec::new()));
        session.connected.store(true, Ordering::Relaxed);
        let state = AppState::new();
        state
            .inner
            .write()
            .sessions
            .insert("monitor-real-openssh".into(), session.clone());
        let app = tauri::test::mock_app();
        let (event_sender, event_receiver) = std::sync::mpsc::channel();
        app.handle().listen("monitor-update", move |event| {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload()) {
                let _ = event_sender.send((Instant::now(), payload));
            }
        });
        run_loop(state.clone(), app.handle().clone());

        let first = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(30),
            |snapshot| snapshot.last_error.is_none() && snapshot.last_success_at.is_some(),
        )
        .await;
        assert_eq!(first.platform, "windows");
        assert!(first.online);
        assert!(!first.stale);
        assert!(first.mem_total_mb > 0);
        assert!(first.capabilities.disk_by_mount);
        assert!(first.capabilities.process_top);
        assert!(!first.capabilities.load_average);
        assert!(session.io_tx.lock().is_none(), "监控不得启动主 shell 连接");

        let baseline = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(15),
            |snapshot| snapshot.baseline_ready && snapshot.last_error.is_none(),
        )
        .await;
        let initial_success = baseline.last_success_at.unwrap();
        let initial_mem_used = baseline.mem_used_mb;
        assert!(baseline.cpu_pct.is_finite());

        fs::write(&mode_file, "timeout").unwrap();
        let timeout_started = Instant::now();
        let timed_out = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(20),
            |snapshot| {
                snapshot
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("超时"))
            },
        )
        .await;
        assert!(timeout_started.elapsed() >= Duration::from_secs(7));
        assert_eq!(timed_out.last_success_at.as_ref(), Some(&initial_success));
        assert_eq!(timed_out.mem_used_mb, initial_mem_used);
        next_monitor_event(&event_receiver, Duration::from_secs(2), |payload| {
            payload["session_id"] == "monitor-real-openssh"
                && payload["last_error"]
                    .as_str()
                    .is_some_and(|error| error.contains("超时"))
        });

        fs::write(&mode_file, "ok").unwrap();
        let timeout_recovered = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(30),
            |snapshot| {
                snapshot.last_error.is_none()
                    && !snapshot.stale
                    && snapshot
                        .last_success_at
                        .as_ref()
                        .is_some_and(|success| success > &initial_success)
            },
        )
        .await;
        let stable_success = timeout_recovered.last_success_at.unwrap();
        let stable_mem_used = timeout_recovered.mem_used_mb;
        while event_receiver.try_recv().is_ok() {}

        fs::write(&mode_file, "fail").unwrap();
        let mut failure_events: Vec<(Instant, serde_json::Value)> = Vec::new();
        for _ in 0..3 {
            let event = next_monitor_event(&event_receiver, Duration::from_secs(20), |payload| {
                payload["session_id"] == "monitor-real-openssh"
                    && payload["last_error"]
                        .as_str()
                        .is_some_and(|error| error.contains("controlled monitor failure"))
                    && !failure_events
                        .iter()
                        .any(|(_, previous)| previous["ts"] == payload["ts"])
            });
            failure_events.push(event);
        }
        // 用快照发布时间验证退避，避免事件回调分发延迟影响计时。
        let failure_time = |payload: &serde_json::Value| {
            chrono::DateTime::parse_from_rfc3339(payload["ts"].as_str().unwrap()).unwrap()
        };
        let first_gap = failure_time(&failure_events[1].1)
            .signed_duration_since(failure_time(&failure_events[0].1))
            .to_std()
            .unwrap();
        let second_gap = failure_time(&failure_events[2].1)
            .signed_duration_since(failure_time(&failure_events[1].1))
            .to_std()
            .unwrap();
        assert!(
            first_gap >= Duration::from_millis(750),
            "第一次退避间隔: {first_gap:?}"
        );
        assert!(
            second_gap >= Duration::from_millis(1_600),
            "第二次退避间隔: {second_gap:?}"
        );
        assert!(
            second_gap > first_gap,
            "退避间隔未增长: {first_gap:?} -> {second_gap:?}"
        );

        let stale = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(10),
            |snapshot| snapshot.stale,
        )
        .await;
        assert!(stale.last_error.is_some());
        assert_eq!(stale.last_success_at.as_ref(), Some(&stable_success));
        assert_eq!(stale.mem_used_mb, stable_mem_used);

        fs::write(&mode_file, "ok").unwrap();
        let recovered = wait_for_snapshot(
            &state,
            "monitor-real-openssh",
            Duration::from_secs(30),
            |snapshot| {
                snapshot.last_error.is_none()
                    && !snapshot.stale
                    && snapshot
                        .last_success_at
                        .as_ref()
                        .is_some_and(|success| success > &stable_success)
            },
        )
        .await;
        assert!(recovered.online);
        next_monitor_event(&event_receiver, Duration::from_secs(2), |payload| {
            payload["session_id"] == "monitor-real-openssh"
                && payload["last_error"].is_null()
                && payload["stale"] == false
        });

        println!(
            "REAL_MONITOR_OK platform={} cores={} mem_total_mb={} disks={} first_backoff={:?} second_backoff={:?}",
            recovered.platform,
            recovered.cores,
            recovered.mem_total_mb,
            recovered.disks.len(),
            first_gap,
            second_gap,
        );
        session.connected.store(false, Ordering::Relaxed);
        state.inner.write().sessions.remove("monitor-real-openssh");
    }

    #[test]
    fn failures_back_off_exponentially_with_a_cap() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(10), MAX_BACKOFF);
    }
}
