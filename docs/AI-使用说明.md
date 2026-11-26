# Chena-Shell AI 使用说明

> 适用版本：V1.0.0.13
> 适用对象：AI 助手（也适用于想手动配置 MCP 的人）
> 目标：把本文件和配套 skill 交给 AI 后，AI 能自行安装 skill、连接 Chena-Shell 的 MCP 服务并正确调用工具

---

## 0. 给 AI 的最短路径

如果你是被用户直接要求「接管 Chena-Shell」「连上我的服务器看看」的 AI，按下面顺序执行：

1. 阅读本文件和同目录的 `SKILL.md`；
2. 按第 6 节安装/确认 skill：绿色版的 `SKILL.md` 就在解压根目录，不要去猜测源码路径；
3. 如果用户尚未配置 MCP Token，必须按第 2.2 节逐步引导用户在 Chena-Shell 内生成凭证并填入 MCP 客户端，不能用 `attach_token` 代替；
4. 确认 Chena-Shell 已启动且 MCP 服务在线；
5. 调用 `session_list`，选择已连接且 `ai_allowed=true` 的会话；
6. 调用 `session_attach` 申请接入（可能返回 `WAIT_CONFIRM`，等待用户在界面批准后带 `confirmation_id` 重试）；
7. 用 `session_exec` 执行只读诊断命令；
8. 结束时**必须**调用 `session_detach`。

不要自己运行 `ssh`、`sshpass` 或 `plink` 去连接同一台机器。Chena-Shell 是唯一入口。

---

## 1. Chena-Shell 是什么

Chena-Shell 是运行在用户 Windows 本机的桌面 SSH 终端，内置标准 MCP Server。AI 以「访客」身份临时接入用户已打开的会话，执行命令、读取终端回放、查看资源快照，并在结束后释放控制权。

关键安全模型：

- 接入需要用户批准（可在设置中关闭，但默认开启）；
- 命令经过三层策略：会话黑名单 → 全局黑名单 → 确认清单 → 放行；
- 黑名单命中直接拒绝，不能重试；
- 确认清单命中需要用户在界面点一次批准，且**一次批准只能消费一次**；
- 写接入是排他的；读接入只能看终端回放，不能执行命令；
- SFTP 权限需要在 attach 时单独申请。

---

## 2. 前置条件与检查

### 2.1 用户侧需要完成的事

1. Chena-Shell 已启动；
2. 「设置 → MCP服务」中：
   - 「启用 MCP 服务」已勾选；
   - 监听地址 `127.0.0.1`，端口默认 `8767`；
   - 已生成访问凭证（完整值只显示一次）。
3. 至少有一个会话已连接；
4. 该会话允许 AI：会话配置中 `ai_allowed=true`（界面「AI 安全」相关开关）。

### 2.2 AI 必须如何引导用户配置 MCP Token

当其他 agent 尚未接入时，不要只回复「请填写 Token」，也不要试图从应用配置或凭据管理器中读取秘密。应按以下步骤引导用户操作：

1. 让用户启动 Chena-Shell，打开「设置 → MCP服务」。
2. 勾选「启用 MCP 服务」，确认监听地址为 `127.0.0.1`，记录实际端口（默认 `8767`）。
3. 让用户点击「重新生成凭证」并在应用内确认，然后立即复制完整凭证。完整值只显示一次；点击「取消」不会生成新凭证。
4. **优先让用户自己把凭证粘贴到 MCP 客户端配置中**，不要求用户把凭证发到聊天里。如果用户明确交由 AI 修改本机客户端配置，只能把凭证写入相应配置，不得在回复、日志、报告或命令行参数中显示，也不能提交到仓库。
5. HTTP 客户端把凭证填为 `Authorization: Bearer <凭证>`；stdio 客户端把凭证填为环境变量 `CHENA_MCP_TOKEN`，并将 `command` 设为绿色版 `Chena-Shell_V1.0.0.13.exe` 的实际完整路径。
6. 保存配置后，让用户重启或重新连接 MCP 客户端；AI 再执行初始化和 `tools/list` 验证，而不是在未配置凭证时反复调用业务工具。

必须向用户区分两种凭证：

- **MCP Token / MCP 访问凭证**：用户在「设置 → MCP服务」生成，用于客户端连接 MCP Server；
- **`attach_token`**：MCP 连接成功后由 `session_attach` 返回，只用于后续会话操作，不能代替 MCP Token。

凭证遗失或可能泄露时，引导用户重新生成；旧凭证会立即失效，所有客户端都要同步更新。`settings_get`、加密配置和 Windows 凭据管理器都不是 AI 获取明文凭据的途径。

### 2.3 端点

| 传输 | 配置 |
| --- | --- |
| Streamable HTTP | `POST http://127.0.0.1:8767/mcp` |
| HTTP 认证 | `Authorization: Bearer <凭证>` |
| HTTP 额外要求 | `Accept: application/json, text/event-stream`；初始化后携带 `mcp-session-id` 与 `mcp-protocol-version` |
| stdio | `Chena-Shell_V1.0.0.13.exe --mcp-stdio` |
| stdio 环境变量 | `CHENA_MCP_TOKEN=<凭证>`；非默认端口时加 `CHENA_MCP_URL=http://127.0.0.1:<端口>/mcp` |

只允许 loopback 地址。服务默认不接受浏览器跨站请求，Origin 受限制。

### 2.4 MCP 客户端配置示例

HTTP：

```json
{
  "mcpServers": {
    "chena-shell": {
      "url": "http://127.0.0.1:8767/mcp",
      "headers": {
        "Authorization": "Bearer <凭证>"
      }
    }
  }
}
```

stdio：

```json
{
  "mcpServers": {
    "chena-shell": {
      "command": "C:\\path\\to\\Chena-Shell_V1.0.0.13.exe",
      "args": ["--mcp-stdio"],
      "env": {
        "CHENA_MCP_TOKEN": "<凭证>",
        "CHENA_MCP_URL": "http://127.0.0.1:8767/mcp"
      }
    }
  }
}
```

> 不同客户端配置字段名可能不同，按客户端文档调整，但地址、路径、环境变量名保持一致。

### 2.5 连通性自检

- HTTP：不带凭证请求 `http://127.0.0.1:8767/mcp`，若返回认证错误（而非连接被拒绝），说明服务在监听。
- 连接被拒绝：服务未启动或端口不对，请用户在设置中确认端口并点「测试 MCP 重启」。
- stdio：进程启动即退出，通常是缺少 `CHENA_MCP_TOKEN`。

---

## 3. 工具清单（18 个）

以 `tools/list` 的实际返回为准；下面是 V1.0.0.13 的工具与参数。

### 3.1 会话与接入

| 工具 | 必填参数 | 说明 |
| --- | --- | --- |
| `session_list` | — | 列出会话及连接、AI 状态；不返回秘密 |
| `session_create` | host, username, password | 新建会话，写入用户界面；默认立即连接。需人工确认：首次返回 `WAIT_CONFIRM`，确认后带 `confirmation_id` 重试；主机密钥未确认时返回 `HOST_KEY_CONFIRM_REQUIRED` |
| `session_connect` | session_id | 连接已有会话。需人工确认，流程同上；主机密钥未确认时返回 `HOST_KEY_CONFIRM_REQUIRED` |
| `session_disconnect` | session_id | 断开连接，保留配置。需人工确认，确认后带 `confirmation_id` 重试 |
| `session_attach` | session_id, reason | 申请接入；可带 `connection_id`、`require_active`、`mode`、`permissions`、`confirmation_id` |
| `session_exec` | session_id, attach_token, command | 执行命令；可带 `run_as`、`cwd`、`timeout_ms`、`confirmation_id` |
| `session_read` | session_id, attach_token | 读取绑定连接的终端回放；可带 `after_seq` |
| `session_detach` | session_id, attach_token, reason | 释放接入，令令牌失效 |
| `session_monitor` | session_id | 最近一次资源快照 |

`session_attach` 的 `mode`：

- `write`（默认）：排他写接入，可执行命令；
- `read`：只读观察，只能 `session_read`，不能申请 `sftp` 或 `sudo` 权限；
- `diag`：非独占只读诊断，可执行白名单内的只读命令（`session_exec`），不能申请 `sftp` / `sudo`，也不接受审批单。

`write` 模式可附带 `task` 声明排障目标（`{"goal": "...", "ttl_minutes": 30}`）：

- 任务内白名单只读命令免逐条审批；写操作、高危命令和 root 仍需单独确认；
- 任务到期后自动 detach，AI 只读标签会写入「任务已结束」提示；
- `session_list` 的 `ai_task` / `ai_task_expires_at` 反映当前任务。

`diag` 与任务内只读白名单共用「设置 → AI安全 → diag 只读命令白名单」，匹配规则：

- 普通条目按完整命令或参数边界匹配，例如 `ss` 放行 `ss -s`，不放行 `sss`；
- 以 `/` 结尾的条目按目录前缀匹配，例如 `cat /proc/` 放行 `cat /proc/net/dev`；
- 命令中出现的绝对路径必须落在白名单已声明的文件或目录内，`cat /etc/os-release /etc/shadow` 这类顺带读取会被拒绝；
- 一律拒绝 `; | & > <`、反引号、`$(`、`${`、反斜杠和换行；
- 拒绝 `..` 路径穿越以及 `$`、`~` 展开，避免白名单前缀被用来读取目录外的文件；
- 拒绝命令专属的危险参数，例如 `tail -f`、`journalctl --vacuum-*`、`crontab -r`、`curl -X`、`ss -K`、`dmesg -C`；
- 会话以 `root` 登录时只放行内置只读条目，自定义条目一律拒绝，需要时改用 `mode=write` 逐条审批。

白名单外命令返回 `DIAG_COMMAND_DENIED`。

### 3.1.1 多终端连接（复制 SSH 渠道）

同一会话可能存在多个终端连接：主 Shell 和用户复制的 SSH 渠道。`session_list` 的每个会话现在带有：

- `connections`：可接入的连接列表，每项含 `connection_id`、`kind`（`primary` / `extra`）、`channel_id`、`generation`、`connected_at`、`login_user`、`is_active`；
- `active_connection_id`：用户当前正在查看的连接；无法确定时为 `null`；
- `active_revision`：活跃连接版本号，切换标签时递增。

`session_attach` 的连接选择规则：

1. 传 `connection_id`：明确绑定该连接，响应 `selection_reason="explicit"`；
2. 传 `require_active=true`：要求目标就是用户当前活跃终端；目标不是活跃连接时返回 `CONNECTION_NOT_ACTIVE`；
3. 两者都不传：只有一个连接时直接绑定（`only_connection`）；多个连接时绑定活跃连接（`active`）；活跃未知时返回 `AMBIGUOUS_CONNECTION` 和 `candidates`，**不会偷偷回退主 Shell**。

`session_read` 的行为：

- 读取目标永远由 `attach_token` 推导，**不能**在每次 read 时临时换连接，避免跨连接混用 `after_seq`；
- 响应带 `connection_id`、`generation`、`is_active`、`active_revision`，用于自检「我读的是不是用户正在看的终端」；
- attach 时用了 `require_active=true`，之后用户切换标签会返回 `ACTIVE_CONNECTION_CHANGED`，同时给出 `active_connection_id`。此时**不要沿用旧 `after_seq`**，也不要假设能继续读，必须重新 `session_attach`；
- 连接关闭或会话重连后返回 `CONNECTION_CLOSED` / `CONNECTION_CHANGED`，旧 token 不能读取新连接的数据。

`prompt_user` 字段表示交互 Shell 的当前用户，只有在可靠 shell integration 上报时才非空，并附带 `prompt_user_source` 与 `prompt_observed_at`。**不要用正则猜提示符，也不要用 `id -u` 冒充当前交互 Shell 用户**；用户执行过 `su`、`sudo -i` 或进入容器后，这两种方式都会误判。自检以 `connection_id + generation + is_active` 为准。

### 3.1.2 AI 只读标签与回显

AI 执行过程通过独立事件 `ai-terminal-data` 输出，默认只进入界面的「AI 只读」标签，不写入人工终端。该标签人类可查看、可复制，但不能输入或执行命令；人工关闭任何终端标签都不会影响 AI 回显。人类可在设置中开启「AI 输出同时在人工终端回显」，默认关闭。

标签中的命令、标准输出、错误输出和状态会写入 `%APPDATA%\chena-shell\logs\ai\terminal` 下的 AI 回显日志，应用重启后仍可追溯；人类可在底部「日志 → AI 日志」查看。该日志始终记录，不受会话终端日志开关影响，并按保留天数统一清理。

> 命名说明：「AI 只读」表示**人类对该标签只读**，不是 AI 只读。AI 仍按接入模式执行命令，人类只能观察。


`session_list` 的每个会话同时返回 `ai_connection_id`、`ai_mode`、`ai_expires_at`、`ai_sudo`、`ai_readers`、`ai_task`、`ai_task_expires_at`，可在接入前后核对当前附着位置与状态。

`permissions` 支持 `["sftp"]`（文件读写）与 `["sudo"]`（以 root 执行单条命令），可同时申请；两者都只能与 `write` 搭配。申请 `sudo` 要求用户在设置中开启「允许 AI 以 root 执行」。

**root 执行（sudo）**：`session_exec` 传 `run_as: "root"`，可选 `cwd`（绝对路径，默认 `/root`）。默认每条 root 命令都需用户在 Chena-Shell 弹窗中批准一次（返回 `WAIT_CONFIRM`，`kind="sudo"`）。**工具没有 `sudo_password` 参数**：需要密码时由用户在本机弹窗输入，密码不经过 AI、不进入命令行与日志。错误码 `SUDO_DISABLED` / `SUDO_NOT_PERMITTED` / `SUDO_PASSWORD_REQUIRED` / `SUDO_AUTH_FAILED` / `SUDO_QUOTA_EXCEEDED` 均不应重试。

> **sudo 错误密码行为（M2，2026-11-22）**：密码错误返回 `SUDO_AUTH_FAILED` 并计入连续失败次数；达到「密码错误锁定次数」（默认 3 次）后，按「sudo 锁定时长」（默认 300 秒）锁定。锁定期间不要重试，应提示用户核对密码或稍后再试。

### 3.2 端口转发

| 工具 | 必填参数 | 说明 |
| --- | --- | --- |
| `forward_list` | session_id | 列出现有转发规则与状态 |
| `forward_add` | session_id, attach_token, listen_port, dest_port | 新增并立即启动；`kind=local/remote`，`dest_host` 默认 127.0.0.1，`listen_host` 默认 127.0.0.1；非回环监听地址需人工确认 |
| `forward_stop` | forward_id, attach_token | 停止指定规则；token 需属于拥有该规则的会话且为 writer 权限 |

### 3.3 快速命令

| 工具 | 必填参数 | 说明 |
| --- | --- | --- |
| `quickcmd_list` | — | 列出用户保存的快速命令 |
| `quickcmd_run` | session_id, attach_token, command | 走与 `session_exec` 相同的令牌与安全策略 |

### 3.4 SFTP（受限）

| 工具 | 必填参数 | 说明 |
| --- | --- | --- |
| `sftp_list` | session_id, attach_token | 列远端目录；`path` 默认 `.`，`limit` 1–500，默认 100 |
| `sftp_read` | session_id, attach_token, path | 读取单个文件，最多 **1 MiB**；`encoding` 为 `text`/`base64` |
| `sftp_write` | session_id, attach_token, path, content | 写入单个文件，最多 **1 MiB**；目标已存在需一次性审批 |

SFTP 工具使用独立认证的 SFTP 连接，要求 attach 时已授予 `sftp` 权限。

### 3.5 设置

| 工具 | 说明 |
| --- | --- |
| `settings_get` | 返回非敏感设置白名单；不返回黑名单细节、凭证等秘密 |

---

## 4. 标准调用流程

### 4.1 找到会话

```json
{"name": "session_list", "arguments": {}}
```

返回 `{"sessions": [{"config": {...}, "status": {"connected": true, "ai_attached": false}}]}`。

选择 `status.connected=true` 且 `config.ai_allowed=true` 的会话。若没有可用会话：

- 用户已给主机和账号密码 → `session_create`；
- 用户只是让你连已有配置 → `session_connect`。

不要猜测主机、账号或密码。

### 4.2 申请接入

```json
{
  "name": "session_attach",
  "arguments": {
    "session_id": "sess-...",
    "reason": "排查 nginx 502，读取服务状态与日志",
    "mode": "write",
    "permissions": ["sftp"]
  }
}
```

- 成功：返回 `attach_token`，后续每次调用都要带上。
- `attach_token` 绑定签发它的 MCP 连接：AI 客户端重连或重启后旧 token 全部失效，必须重新 `session_attach`；旧 token 也不能在新连接里执行、读取或 detach。客户端断开且未 detach 时，旧接入会按有效期自然过期（默认 30 分钟），或由用户在 Chena-Shell 界面强制断开。
- 返回 `WAIT_CONFIRM`：用户在界面看到了审批请求，等待其点击批准；然后用**相同参数**加 `confirmation_id` 重试。审批默认 30 秒超时，但用户把鼠标移入审批窗口或激活应用窗口时倒计时会暂停，移出后继续；因此等待时间可能超过 30 秒，不要因为超时时间没到就重复发起新请求。
- 返回 `CONFIRMATION_NOT_APPROVED`：用户尚未批准，稍后重试，不要反复轰炸。
- 返回 `session has AI disabled`：停止，告知用户去会话设置中允许 AI。
- 返回 `AI attach disabled` / 认证失败：停止，让用户检查设置。

`reason` 会被用户看到，必须是简短中文，写清目的与范围（建议 8–60 字），例如「读取网卡流量和系统版本，排查网络异常」。不要用 `????????`、空字符串、`test`、`diag`、emoji 或 Markdown。若客户端把中文编码成问号，改用简短英文，例如 `read NIC stats and OS version`，不要发送问号占位。服务端按「问号累计 ≥ 4 个、问号占比过半、或以问号开头后接空格/结尾」判定编码异常，`????????? inode` 与 `???? 检查 nginx` 都会被替换为「未提供可读原因（客户端编码异常）」，单个问号如 `检查 nginx 状态?` 会原样保留。

### 4.3 执行命令

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

返回含义：

| 返回 | 含义 | 应对 |
| --- | --- | --- |
| `ok=true, exit_code=0` | 执行成功 | 解析 stdout |
| `ok=true, exit_code!=0` | 命令执行了但失败 | 读 stderr，调整命令 |
| `ok=false, error="BLOCKED", rule=...` | 命中阻断规则 | **不要重试**，换更安全的方案 |
| `ok=false, error="WAIT_CONFIRM", confirmation_id=...` | 需要人工批准 | 告知用户风险，等批准后带 id 重试 |
| `ok=false, error="TIMEOUT"` | 超时 | 缩小命令范围或提高 `timeout_ms`（上限 300000） |

`timeout_ms` 范围 1000–300000，默认 30000。

终端可见性：

- `session_exec` 与 `quickcmd_run` 的命令实际开始执行时，主窗口对应会话的虚拟终端会显示 `[AI 执行]` 和命令文本；结束后显示 `[AI 结果]`、stdout、stderr、退出码与耗时。
- 被阻断或仍等待人工审批的命令不会显示为已执行；非零退出、超时和执行 channel 失败会明确标记。
- 回显只写入主终端及其内存回放，不向远端交互 PTY 注入文本，也不写入交互 shell 日志；`session_read` 读取回放时可能读到这些 `[AI …]` 行。
- 回显不会自动切换用户当前标签，也不会写入复制 SSH 渠道的独立终端。

### 4.4 读取回放与监控

```json
{"name": "session_read", "arguments": {"session_id": "sess-...", "attach_token": "<token>", "after_seq": 128}}
```

用上一次返回的 `next_seq` 作为下一次的 `after_seq`，实现增量读取。

```json
{"name": "session_monitor", "arguments": {"session_id": "sess-..."}}
```

获取 CPU、内存、磁盘、网络、负载快照，适合判断资源问题。

### 4.5 结束并释放

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

无论成功、被阻断还是出错，**都要 detach**。

返回固定为 `{"ok": true, "mode": "write|read|diag", "note": "..."}`，`mode` 是本次接入的真实模式，不会把只读诊断误报成只读观察：

- `write` → 「写入接入已释放」
- `diag` → 「只读诊断观察者已释放」
- `read` → 「只读观察者已释放」

审计日志的 detach 事件同样记录真实 `mode` 与 `connection_id`，可用它核对这次是否执行过命令。

> **使用规范（必须遵守）**：获得终端授权（`session_attach` 成功）后，先确认用户本次的所有需求是否已经全部完成，再主动 `session_detach` 断开与 shell 的会话。不要在单个问题刚验证完就提前 detach；否则后续仍需排查时必须重新申请授权，会出现问题没解决却要反复授权的情况。

---

## 5. 推荐命令策略

先只读，后变更：

1. 服务状态：`systemctl status <svc> --no-pager`
2. 日志：`journalctl -u <svc> -n 100 --no-pager`
3. 端口：`ss -tlnp`
4. 资源：`df -h`、`free -m`、`uptime`、`ps aux --sort=-%cpu | head`
5. 配置：`cat` / `grep` 只读查看

需要变更时（`rm`、`systemctl restart`、`iptables`、`kill`、写配置），先向用户说明影响并征得同意，即使 Chena-Shell 允许执行。

不要：

- 运行交互式 shell 或 `sudo -i`；
- 尝试绕过黑名单（改写命令、编码、分片）；
- 未经用户要求擅自杀进程、重启服务、修改防火墙；
- 在 `BLOCKED` 后反复换写法重试同一意图。

---

## 6. 安装 skill

绿色版已经在解压根目录提供 `SKILL.md`，与 `Chena-Shell_V1.0.0.13.exe`、`AI-使用说明.md` 放在一起。如果只有 EXE，应先向用户索要完整绿色版压缩包，不要猜测或联网下载其他来源的 skill。源码目录中的对应文件是 `.trae/skills/chena-shell/SKILL.md`，但操作绿色版时不需要访问源码。

AI 可自行执行：

1. 在绿色版根目录确认 `SKILL.md` 存在；项目级安装时复制为 `<项目>/.trae/skills/chena-shell/SKILL.md`。
2. 用户希望全局可用时：
   - Trae CN：复制到 `%USERPROFILE%\.trae-cn\skills\chena-shell\SKILL.md`；
   - 其他 MCP/Agent 客户端：放入其 skills 根目录下的 `chena-shell/SKILL.md`。
3. 安装后确认 skill 能被列出，再按本文件第 4 节调用。

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

---

## 7. 故障排查

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 连接被拒绝 | MCP 未启动/端口错误 | 用户到「设置 → MCP服务」确认并重启 |
| 401 / 缺少 Bearer | 凭证缺失或已轮换 | 用户重新生成凭证，更新客户端配置 |
| `session not found` | session_id 错误或会话被删 | 重新 `session_list` |
| `session has AI disabled` | 会话禁止 AI | 用户到会话设置开启 |
| `BAD_TOKEN` / 令牌失效 | 令牌过期、已 detach、或用于其他 MCP 连接 | 重新 attach |
| `token 与当前客户端或会话不匹配` | 令牌是在另一条 MCP 连接签发的，或客户端已重连/重启 | 停止复用旧 token，重新 `session_attach` |
| `READ_ONLY_TOKEN` | 用 `mode=read` 的 token 执行命令 | 只读诊断改用 `mode=diag`，写操作改用 `mode=write` |
| `DIAG_COMMAND_DENIED` | 命令不在 diag 白名单，或含管道/重定向/串联 | 改用白名单内的只读命令；需要写操作时重新以 `mode=write` 接入 |
| `DIAG_READ_ONLY_ONLY` | diag token 请求 `run_as=root` | diag 不支持 root，root 需 `mode=write` + `sudo` |
| `任务已结束` / token 失效 | 任务到期自动 detach | 重新 attach 并声明新的 task |
| `WAIT_CONFIRM` 反复出现 | 用户未批准，或每次都在请求新的审批 | 等用户批准后用同一 `confirmation_id` 重试一次 |
| `confirmation not found` | 审批已过期或被消费 | 重新发起请求 |
| SFTP 返回权限错误 | attach 时未申请 `sftp` | 重新 attach 并带 `permissions: ["sftp"]` |
| stdio 立即退出 | 缺 `CHENA_MCP_TOKEN` | 补环境变量 |

---

## 8. 安全与合规约束

- 只能在用户明确要求下接入；不要主动扫描用户会话。
- 不读取、不输出密码、私钥、令牌；`session_list` 不会返回秘密，也不要去别处找。
- 不把 attach_token 写进日志、报告或提交到仓库。
- 接入理由、命令和结果都可能被用户审计；保持可解释。
- 用户拒绝审批就是拒绝，不要换工具或换参数绕过。
- 完成后必须 detach，并给用户一份结构化结论：做了什么、发现了什么、建议怎么修。

---

## 9. 升级注意事项与版本边界

- **更新 Chena-Shell 后，需要在 AI 客户端重连 MCP 服务（或重启客户端）以刷新 `tools/list`。** 多数 MCP 客户端会缓存工具列表；只升级服务端而不重连，会出现「文档里有新工具/新参数、调用却报错」的情况。例如新增的 `diag` 模式与 `task` 参数，未刷新时可能仍按旧 schema 校验。
- 当前交付为 V1.0.0.13，Windows x64 绿色版（不提供 NSIS/MSI 安装包）。
- 「复制 SSH 渠道」的真实双向字节流尚未完成本机复验，AI 不应把它当作已验收能力向用户保证；但多连接下的定向 attach、独立回放与活跃连接自检已实现，可用 `session_list.connections` 确认当前可绑定的连接。
- `sftp_read` / `sftp_write` 单次上限 1 MiB；更大的文件请让用户用 SFTP 窗口操作。
- 主机监控快照依赖独立监控连接，刚连接时可能暂无数据，稍后再取。
