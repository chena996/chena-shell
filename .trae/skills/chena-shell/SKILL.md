---
name: chena-shell
description: Attach to a user's Chena-Shell SSH session over MCP and run diagnostic commands, read terminal replay, inspect resource snapshots, use restricted SFTP, or create/connect a session when the user provides host and credentials. Use when the user says "接管我的 SSH 会话", "帮我排查服务器", "连到 10.0.0.5 看看", "用 Chena-Shell 连", or asks for remote debugging through Chena-Shell.
---

# Chena-Shell — AI 接入 Skill

Chena-Shell 是用户 Windows 本机上的桌面 SSH 终端，内置标准 MCP Server。你以访客身份临时接入用户已打开的会话，执行命令、读取回放、查看资源快照，结束后释放控制权。

本文件自包含：安装、验证、调用、错误处理都在这里。不要凭记忆猜工具名或参数。

---

## 1. 安装

绿色版解压目录已经带有本文件，文件名为 `SKILL.md`；同目录还应有 `AI-使用说明.md` 和 `使用手册.md`。先阅读 `AI-使用说明.md`，再选择下面一种安装方式。不要假设用户机器上存在源码目录。

### 1.1 项目级（推荐）

把绿色版根目录的 `SKILL.md` 复制为：

```text
<project>/.trae/skills/chena-shell/SKILL.md
```

### 1.2 用户级（全局可用）

复制到客户端的用户 skills 根目录：

- Trae CN：`%USERPROFILE%\.trae-cn\skills\chena-shell\SKILL.md`
- 其他客户端：`<skills root>/chena-shell/SKILL.md`

在**绿色版解压目录**执行 PowerShell：

```powershell
$src = Join-Path (Get-Location) 'SKILL.md'
if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
    throw '未找到 SKILL.md；请先解压完整绿色版，并在其根目录执行'
}
$dst = Join-Path $env:USERPROFILE '.trae-cn\skills\chena-shell'
New-Item -ItemType Directory -Force -Path $dst | Out-Null
Copy-Item -LiteralPath $src -Destination (Join-Path $dst 'SKILL.md') -Force
```

安装后确认 skill 列表里能看到 `chena-shell`，再继续。

---

## 2. 前置检查

1. Chena-Shell 已启动。
2. 「设置 → MCP服务」：
   - 「启用 MCP 服务」已勾选；
   - 监听地址 `127.0.0.1`；
   - 端口默认 `8767`；
   - 已生成访问凭证（只显示一次，需要用户提供）。
3. 目标会话已连接。
4. 目标会话允许 AI：`session_list` 中 `config.ai_allowed === true`。

### 2.1 必须引导用户完成 MCP Token 配置

如果用户还没有配置 MCP 访问凭证，AI 必须逐步给出以下操作，而不是只说「请配置 Token」：

1. 打开「设置 → MCP服务」，勾选「启用 MCP 服务」，确认地址为 `127.0.0.1` 并记录端口（默认 `8767`）。
2. 点击「重新生成凭证」并在应用内确认，立即复制完整凭证；完整值只显示一次，取消不会生成。
3. 优先让用户自己把凭证填入 MCP 客户端，不要求粘贴到聊天中。HTTP 使用 `Authorization: Bearer <凭证>`；stdio 使用 `CHENA_MCP_TOKEN=<凭证>`，`command` 必须指向绿色版 `Chena-Shell_V1.0.0.12.exe` 的实际完整路径。
4. 保存后重启或重新连接客户端，再验证初始化和 `tools/list`。

如果用户明确让 AI 修改本机客户端配置，凭证只能写入该配置，不得出现在回复、日志、报告或命令行参数中，也不得提交仓库。不要从加密配置、Windows 凭据管理器或其他文件中寻找明文凭证。

MCP 访问凭证与 `session_attach` 返回的 `attach_token` 是两种不同秘密，不能互相替代。凭证遗失或泄露时，让用户重新生成并更新所有客户端；旧凭证会立即失效。

连接被拒绝说明服务未启动或端口不对，让用户检查设置，不要猜测其他端口；401 说明凭证缺失、错误或已轮换。

---

## 3. 接入方式

### 3.1 HTTP（Streamable HTTP）

- 端点：`http://127.0.0.1:8767/mcp`
- 认证：`Authorization: Bearer <凭证>`
- 必须发送 `Accept: application/json, text/event-stream`
- 初始化后携带服务端返回的 `mcp-session-id` 与 `mcp-protocol-version`

```json
{
  "mcpServers": {
    "chena-shell": {
      "url": "http://127.0.0.1:8767/mcp",
      "headers": { "Authorization": "Bearer <凭证>" }
    }
  }
}
```

### 3.2 stdio

```json
{
  "mcpServers": {
    "chena-shell": {
      "command": "C:\\path\\to\\Chena-Shell_V1.0.0.12.exe",
      "args": ["--mcp-stdio"],
      "env": {
        "CHENA_MCP_TOKEN": "<凭证>",
        "CHENA_MCP_URL": "http://127.0.0.1:8767/mcp"
      }
    }
  }
}
```

stdio 进程缺少 `CHENA_MCP_TOKEN` 会立即退出。只允许 loopback 地址，`CHENA_MCP_URL` 不能内嵌用户名密码。

---

## 4. 工具清单

先调用 `tools/list`；以下是 V1.0.0.12 的实际工具。更新 Chena-Shell 后必须让 AI 客户端重连 MCP 服务以刷新工具列表。

### 4.1 会话

| 工具 | 必填参数 | 用途 |
| --- | --- | --- |
| `session_list` | — | 列出会话、连接状态、AI 状态；不返回秘密 |
| `session_create` | host, username, password | 新建会话并默认立即连接；会出现在用户界面。需人工确认：首次返回 WAIT_CONFIRM，确认后带 confirmation_id 重试；主机密钥未确认时返回 HOST_KEY_CONFIRM_REQUIRED |
| `session_connect` | session_id | 连接已有会话。需人工确认；主机密钥未确认时返回 HOST_KEY_CONFIRM_REQUIRED |
| `session_disconnect` | session_id | 断开连接，保留配置。需人工确认 |
| `session_attach` | session_id, reason | 申请接入；可带 connection_id / require_active / mode / permissions |
| `session_exec` | session_id, attach_token, command | 执行命令；可带 run_as / cwd / timeout_ms / confirmation_id |
| `session_read` | session_id, attach_token | 读 token 绑定连接的终端回放 |
| `session_detach` | session_id, attach_token, reason | 释放接入 |
| `session_monitor` | session_id | 资源快照 |

#### 多终端连接（复制 SSH 渠道）

`session_list` 的会话带有 `connections`（每项含 `connection_id`、`kind`、`channel_id`、`generation`、`connected_at`、`login_user`、`is_active`）、`active_connection_id`、`active_revision`。

选择规则：传 `connection_id` 明确绑定（`selection_reason="explicit"`）；传 `require_active=true` 要求活跃终端，否则 `CONNECTION_NOT_ACTIVE`；都不传时单连接直接绑定，多连接绑定活跃连接，活跃未知返回 `AMBIGUOUS_CONNECTION` 与 `candidates`，不会回退主 Shell。

`session_read` 的连接由 token 推导，不接受每次 read 换连接。`require_active=true` 后用户切换标签会返回 `ACTIVE_CONNECTION_CHANGED`；连接关闭或重连返回 `CONNECTION_CLOSED` / `CONNECTION_CHANGED`。此时不要沿用旧 `after_seq`，应重新 attach。`prompt_user` 可能为 null，不要用正则或 `id -u` 猜测当前交互用户，自检以 `connection_id + generation + is_active` 为准。

### 4.2 端口转发

| 工具 | 必填参数 |
| --- | --- |
| `forward_list` | session_id |
| `forward_add` | session_id, attach_token, listen_port, dest_port（可选 kind、dest_host、listen_host；listen_host 默认 127.0.0.1，非回环需审批） |
| `forward_stop` | forward_id, attach_token（需为拥有该规则的会话 writer token） |

### 4.3 快速命令

| 工具 | 必填参数 |
| --- | --- |
| `quickcmd_list` | — |
| `quickcmd_run` | session_id, attach_token, command |

### 4.4 SFTP（需授权）

| 工具 | 必填参数 | 限制 |
| --- | --- | --- |
| `sftp_list` | session_id, attach_token | `path` 默认 `.`，`limit` 1–500，默认 100 |
| `sftp_read` | session_id, attach_token, path | 单文件 ≤ 1 MiB，`encoding` 为 text/base64 |
| `sftp_write` | session_id, attach_token, path, content | ≤ 1 MiB；覆盖已有文件需一次性审批 |

### 4.5 设置

| 工具 | 用途 |
| --- | --- |
| `settings_get` | 非敏感设置白名单 |

---

## 5. 标准流程

### 5.1 列会话

```json
{"name": "session_list", "arguments": {}}
```

返回结构：

```json
{"sessions": [{"config": {"id": "...", "name": "...", "host": "...", "username": "...", "ai_allowed": true}, "status": {"connected": true, "ai_attached": false}}]}
```

选择 `status.connected === true` 且 `config.ai_allowed === true` 的会话。

没有可用会话时：

- 用户明确给了主机、账号、密码 → `session_create`；
- 用户只想连已有配置 → `session_connect`。

不要猜测主机或凭据。

### 5.2 申请接入

```json
{
  "name": "session_attach",
  "arguments": {
    "session_id": "sess-...",
    "reason": "排查 nginx 502，查看状态与日志",
    "mode": "write",
    "permissions": ["sftp"]
  }
}
```

- `mode`：`write`（默认，可执行命令）、`read`（只读回放，不能带 `sftp` 或 `sudo`）或 `diag`（非独占只读诊断，只能执行白名单只读命令）。
- `task`（仅 `write`）：`{"goal": "排查 nginx 502", "ttl_minutes": 30}`。任务内白名单只读命令免逐条审批，写/高危/root 仍需单独确认，到期自动 detach。
- `permissions`：`["sftp"]`（文件读写）或 `["sudo"]`（以 root 执行单条命令），可同时申请；`sudo` 只能配 `write`。
- 默认需要人工批准；用户可在设置中关闭。

`reason` 会原样显示在人工审批窗口和 AI 活动里，必须是简短中文，写清目的与范围（建议 8–60 字）：

- 好：`读取网卡流量和系统版本，排查网络异常`
- 好：`检查 nginx 状态与磁盘占用`
- 不要：`????????`、空字符串、`test`、`diag`、emoji 或 Markdown。
- 如果客户端把中文编码成问号，改用简短英文，例如 `read NIC stats and OS version`，不要发送问号占位。
- Chena-Shell 会把「问号累计 ≥ 4 个、问号占比过半、或以问号开头后接空格/结尾」视为编码异常，因此 `????????? inode`、`???? 检查 nginx` 都会显示为「未提供可读原因（客户端编码异常）」；`检查 nginx 状态?` 这种单个问号会原样保留。

`diag` 与任务内只读白名单共用设置中的命令列表：普通条目按完整命令或参数边界匹配（`ss` 放行 `ss -s`，不放行 `sss`），以 `/` 结尾的条目按目录前缀匹配（`cat /proc/` 放行 `cat /proc/net/dev`）。命令中的绝对路径必须落在白名单声明的文件或目录内；拒绝管道、重定向、命令串联、`$(`、`${`、反引号、换行、`..` 路径穿越、`$`/`~` 展开，以及 `tail -f`、`journalctl --vacuum-*`、`crontab -r`、`curl -X`、`ss -K`、`dmesg -C` 等危险参数。会话以 `root` 登录时只放行内置只读条目，自定义条目需改用 `mode=write` 逐条审批；白名单外命令返回 `DIAG_COMMAND_DENIED`。只读 token 执行 `session_exec` 返回 `READ_ONLY_TOKEN`。

成功返回：

```json
{"ok": true, "attach_token": "<token>", "mode": "write", "permissions": ["exec", "read", "detach"], "expires_in_seconds": 1800}
```

申请 `sudo` 时额外返回：

```json
{"ok": true, "permissions": ["exec", "read", "detach", "sudo"], "sudo": {"enabled": true, "require_confirm": true, "quota_remaining": 50}}
```

令牌默认 30 分钟有效，并绑定当前 MCP 连接与会话；AI 客户端重连/重启后旧令牌立即失效，必须重新 `session_attach`，且旧令牌不能在新连接中 exec/read/detach。空闲 10 分钟可能被断开。

### 5.3 处理审批

`session_attach` 可能返回：

```json
{"ok": false, "error": "WAIT_CONFIRM", "confirmation_id": "...", "status": "pending"}
```

处理方式：

1. 告知用户「Chena-Shell 界面已弹出接入审批，请批准」；
2. 等用户确认后，用**相同参数**加 `confirmation_id` 重试；
3. `status: "approved"` 表示已批准但尚未消费，同样带上 `confirmation_id` 重试一次即可拿到令牌；
4. `CONFIRMATION_NOT_APPROVED` 表示用户拒绝或尚未处理，停止重试并询问用户。

一次批准只能消费一次。

### 5.4 执行命令

```json
{
  "name": "session_exec",
  "arguments": {
    "session_id": "sess-...",
    "attach_token": "<token>",
    "command": "systemctl status nginx --no-pager",
    "timeout_ms": 30000
  }
}
```

`timeout_ms` 范围 1000–300000，默认 30000。

| 返回 | 含义 | 应对 |
| --- | --- | --- |
| `ok=true, exit_code=0` | 成功 | 解析 stdout |
| `ok=true, exit_code!=0` | 已执行但失败 | 看 stderr 调整 |
| `ok=false, error="BLOCKED", rule="..."` | 命中黑名单 | 不要重试、不要改写绕过 |
| `ok=false, error="WAIT_CONFIRM", confirmation_id=...` | 需人工批准 | 说明风险，等批准后带 id 重试 |
| `ok=false, error="TIMEOUT"` | 超时 | 缩小范围或加大超时（≤300000） |
| `error="session has AI disabled"` | 会话禁止 AI | 停止，让用户开启 |
| `error="BAD_TOKEN"` / 令牌失效 | 过期或已 detach | 重新 attach |

### 5.4.1 以 root 执行（sudo）

需要 root 才能做的只读诊断（`ss -ulnp`、`lsof -i`、`journalctl`、`tcpdump` 等）才使用。前提：attach 时申请了 `permissions: ["sudo"]`，且用户在设置里开启了「允许 AI 以 root 执行」。

```json
{
  "name": "session_exec",
  "arguments": {
    "session_id": "sess-...",
    "attach_token": "<token>",
    "command": "ss -ulnp",
    "run_as": "root",
    "cwd": "/root",
    "timeout_ms": 30000
  }
}
```

强制规则：

1. **不要传密码**。工具没有 `sudo_password` 参数；需要密码时用户会在 Chena-Shell 弹窗中输入，密码永远不经过你。
2. root 命令默认逐条确认：命中确认清单会返回 `WAIT_CONFIRM` 且 `kind="sudo"`，等用户批准后带同一 `confirmation_id` 重试一次。
3. 命中 sudo 黑名单返回 `BLOCKED`：不得重试、不得改写/编码/拆分绕过。
4. 不要用 `sudo -i`、`sudo -s`、`sudo su` 提权（会被黑名单直接拒绝）；每条 root 命令都是独立通道，`cd`、环境变量不跨命令保留。
5. 优先只读命令；改配置、重启服务、删文件等高风险操作先向用户说明影响再执行。
6. 错误码：`SUDO_DISABLED`（总开关关闭）、`SUDO_NOT_PERMITTED`（服务器 sudoers 不允许）、`SUDO_PASSWORD_REQUIRED`（用户取消输入）、`SUDO_AUTH_FAILED`（密码错误或锁定）、`SUDO_QUOTA_EXCEEDED`（超出本次接入次数上限）。以上情况都**不要反复重试**，应告知用户。
7. `run_as` 默认 `user`；`cwd` 仅接受绝对路径，默认 `/root`。单条输出超过 256 KiB 会截断并带 `[truncated]`。
8. 密码错误返回 `SUDO_AUTH_FAILED` 并计入连续失败次数；连续输错达到「密码错误锁定次数」（默认 3 次）后，按「sudo 锁定时长」（默认 300 秒）锁定。锁定期间不要反复重试，应提示用户核对密码或稍后再试。

### 5.5 读回放与监控

```json
{"name": "session_read", "arguments": {"session_id": "sess-...", "attach_token": "<token>"}}
```

用返回的 `next_seq` 作为下一次 `after_seq` 做增量读取。

```json
{"name": "session_monitor", "arguments": {"session_id": "sess-..."}}
```

返回 CPU、内存、磁盘、网络、负载；刚连接时可能暂无数据。

### 5.6 释放（必须）

```json
{
  "name": "session_detach",
  "arguments": {
    "session_id": "sess-...",
    "attach_token": "<token>",
    "reason": "诊断完成：80 端口被 pid 9876 占用"
  }
}
```

成功、被阻断、出错都要 detach。理由会进入 AI 活动记录，写具体。

返回固定为 `{"ok": true, "mode": "write|read|diag", "note": "..."}`，`mode` 是本次接入的真实模式：

- `write` → 「写入接入已释放」
- `diag` → 「只读诊断观察者已释放」
- `read` → 「只读观察者已释放」

审计日志的 detach 事件同时记录真实 `mode` 与 `connection_id`。

AI 只读标签中的命令、标准输出、错误输出和状态会写入 `%APPDATA%\chena-shell\logs\ai\terminal` 下的 AI 回显日志，人类可在底部「日志 → AI 日志」查看。该日志始终记录，不受会话终端日志开关影响；日志会自动脱敏常见口令、令牌和私钥，但不要主动让命令输出明文秘密。

> **使用规范（必须遵守）**：获得终端授权（`session_attach` 成功）后，先与用户确认本次所有需求是否都已完成，确认完成后再主动 `session_detach` 断开与 shell 的会话。不要在单个步骤刚验证完就提前 detach，否则后续仍需继续排查时会再次触发授权审批，造成问题没解决却反复授权的循环。

---

## 6. 命令策略

先只读：

```text
systemctl status <svc> --no-pager
journalctl -u <svc> -n 100 --no-pager
ss -tlnp
df -h
free -m
uptime
ps aux --sort=-%cpu | head -11
cat / grep 只读查看配置
```

高风险命令（`rm`、`systemctl restart`、`iptables`、`kill`、写配置）即使 Chena-Shell 允许，也要先向用户说明影响并征得同意。

禁止：

- 运行交互式 shell、`sudo -i`；
- 绕过黑名单（改写、编码、拆分命令）；
- 未经要求杀进程、重启服务、改防火墙；
- 在 `BLOCKED` 后反复尝试同一意图。

---

## 7. 安全约束

- 只在用户明确要求下接入，不主动扫描会话。
- 不输出密码、私钥、令牌；不把 `attach_token` 写进报告或仓库。
- 用户拒绝就是拒绝，不要换工具绕过。
- 结束后必须 detach。
- 不自行运行 `ssh`、`sshpass`、`plink` 连同一台机器；Chena-Shell 是唯一入口。

---

## 8. 输出给用户的报告格式

```markdown
## 诊断结果：<主机/会话>

### 执行的命令
- `systemctl status nginx` → failed
- `journalctl -u nginx -n 100` → bind 0.0.0.0:80 Address already in use
- `ss -tlnp` → 80 被 pid 9876 占用

### 根因
<一两句说明>

### 建议
1. <方案与影响>
2. <备选方案>

### 附注
- 已 detach，令牌已失效
```

---

## 9. 排障

| 现象 | 处理 |
| --- | --- |
| 连接被拒绝 | 服务未启动/端口错误，让用户到设置确认并重启 |
| 401 / 缺 Bearer | 凭证缺失或已轮换，让用户重新生成并更新配置 |
| `session not found` | 重新 `session_list` |
| `session has AI disabled` | 用户到会话设置允许 AI |
| `WAIT_CONFIRM` 反复出现 | 用户未批准或每次都在新建审批；用同一 `confirmation_id` 重试一次 |
| `confirmation not found` | 审批过期或被消费，重新发起 |
| SFTP 权限错误 | attach 时补 `permissions: ["sftp"]` |
| stdio 立即退出 | 缺 `CHENA_MCP_TOKEN` |

---

## 10. 版本边界

- 当前为 V1.0.0.12，Windows x64 绿色版（不提供 NSIS/MSI 安装包）。
- `sftp_read` / `sftp_write` 单次 ≤ 1 MiB，大文件让用户用 SFTP 窗口。
- 「复制 SSH 渠道」的真实双向字节流尚未完成本机复验，不要向用户保证该能力已验收。
- 主机监控依赖独立连接，刚连接时可能暂无快照。
