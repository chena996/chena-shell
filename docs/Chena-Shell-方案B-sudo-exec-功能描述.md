# Chena-Shell 方案 B：sudo exec 完整功能描述

> 版本：**v1.1**（已对照 Chena-Shell 1.0.0-1 代码现状核对，2026-11-21）
> 适用版本：Chena-Shell 1.0.0-1 及后续
> 定位：在不接管用户交互 PTY 的前提下，让 AI 能在**单独审批、逐条确认**的情况下以 root 身份执行诊断命令。
>
> 修订说明：v1.1 在 v1.0 草案基础上，核对了 `mcp_tools.rs` / `session.rs` / `security.rs` / `approval.rs` / `state.rs` 的现状，
> 补充了与现有机制（`attach_permissions`、`SecurityEngine` 三层、`ConfirmationKind`、`exec_collect` 的 `RC_MARK` 退出码回显）的衔接点，
> 并新增第 19 节「实施方案」，给出文件级改动清单、里程碑顺序与关键技术决策，可直接作为开发依据。

---

## 目录

- [0. 功能总览](#0-功能总览)
- [1. 背景与目标](#1-背景与目标)
- [2. 范围与非目标](#2-范围与非目标)
- [3. 角色与前提](#3-角色与前提)
- [4. 核心概念](#4-核心概念)
- [5. 权限模型](#5-权限模型)
- [6. 设置项](#6-设置项设置--ai-安全)
- [7. 用户交互流程](#7-用户交互流程)
- [8. MCP 接口变更](#8-mcp-接口变更)
- [9. 返回值与错误码](#9-返回值与错误码)
- [10. 执行实现规范](#10-执行实现规范)
- [11. 审计与回显](#11-审计与回显)
- [12. 安全约束（强制项）](#12-安全约束强制项)
- [13. 状态机](#13-状态机)
- [14. 边界与限制](#14-边界与限制)
- [15. 兼容性与降级](#15-兼容性与降级)
- [16. 验收测试用例](#16-验收测试用例)
- [17. 文档与 skill 更新清单](#17-文档与-skill-更新清单)
- [18. 风险与后续演进](#18-风险与后续演进)
- [19. 实施方案](#19-实施方案对照-chena-shell-100-1-代码现状)

---

## 0. 功能总览

本方案共交付 **9 项功能**，全部围绕「AI 以 root 执行单条命令但看不见密码」这一目标：

| # | 功能 | 用户可见表现 | 关键约束 | 章节 |
|---|---|---|---|---|
| F1 | 申请 root 权限 | attach 时弹窗，列明将获得 root 能力与风险 | 默认关闭；`read` 模式不可申请 | 5 / 7.2 |
| F2 | 单条命令确认 | 每次 root 命令弹窗显示命令原文、命中规则、影响提示 | 一次批准只对一条命令有效 | 7.3 |
| F3 | 免密提权执行 | AI 提交 `run_as: "root"` 即得以 root 执行 | 优先 `sudo -n`，不强制重验密码 | 10.1 |
| F4 | 密码输入弹窗 | 必须密码时弹窗由用户输入，掩码显示 | 密码不经 AI、不进 argv/日志 | 7.4 / 12 |
| F5 | 密码缓存策略 | 可选不缓存 / 本次接入 / 本次运行 | attach 结束即失效 | 6 |
| F6 | sudo 专用策略 | 黑名单直接拒绝、确认清单逐条批准 | 分层不可重排，命中即终止 | 5.2 / 6.1 / 6.2 |
| F7 | 次数配额与失败锁定 | 超 50 次提示重新接入；连续 3 次密码错锁 5 分钟 | 配额随 attach 重置 | 6 / 9 |
| F8 | 审计与回显 | 终端显示 `[AI 执行 root]`；日志记录 run_as/rule/审批链路 | 日志绝不含密码/token | 11 |
| F9 | 完全不影响交互终端 | 用户自己的 shell 无注入、无切换、无污染 | root 走独立 exec 通道 | 2 / 10.2 |

**前置条件**：会话已连接且 `ai_allowed = true`；服务器该用户具备 sudo 权限；设置中「允许 AI 以 root 执行」已开启。

**不做什么**（避免范围蔓延）：不接管交互 PTY、不支持 `sudo -i` 类交互提权、不记住 root 登录态、不提升 SFTP 权限、不修改 sudoers。

---

## 1. 背景与目标

### 1.1 问题

现有 `session_exec` 在独立 exec 通道中以 SSH 登录用户（如 `chena`）身份执行命令。用户在交互终端里 `sudo -i` 变成 root 后，AI 无法获得同等权限，因为：

- exec 通道不继承交互 PTY 的 euid；
- MCP 没有向交互 PTY 写入输入的能力；
- 若让 AI 自行执行 `sudo`，密码需要经过 AI，违反"秘密不经过 AI"的原则。

### 1.2 目标

1. AI 具备**以 root 执行单条命令**的能力，用于需要 root 的诊断场景（`ss -ulnp`、`tcpdump`、`journalctl`、`lsof -i` 等）。
2. root 执行必须**显式授权、逐条确认、可审计**。
3. sudo 密码**永不经过 AI、永不进入命令行参数、日志、回放或报告**。
4. 不改变用户交互终端的任何状态（不注入、不切换、不污染）。

### 1.3 一句话定义

> `sudo` 是 `write` 接入的一个**权限扩展**：attach 时申请，审批时授权，exec 时由 Chena-Shell 进程负责提权，AI 只提交命令、只拿到结果。

---

## 2. 范围与非目标

### 2.1 在范围内

- `session_attach` 新增 `permissions: ["sudo"]`
- `session_exec` 在 sudo 权限下以 root 执行
- 设置项、审批弹窗、审计日志、回显标记
- 错误码、降级路径、验收用例

### 2.2 不在范围内（明确不做）

| 不做 | 原因 |
|---|---|
| 接管交互 PTY（把命令敲进用户终端） | 属方案 A，风险与实现复杂度显著更高 |
| `sudo -i` / `sudo -s` / `sudo su` 交互式提权 | 无法回收会话、无法审计单条命令 |
| 向 AI 暴露 sudo 密码 | 违反秘密最小暴露原则 |
| 记住 root 登录态（跨命令保持 cd/环境） | 每条命令独立通道，属设计边界 |
| 修改 sudoers / passwd / authorized_keys | 默认禁止，需单独高风险确认 |
| 以 root 操作 SFTP | SFTP 仍以登录用户身份，不在本方案内 |

---

## 3. 角色与前提

| 角色 | 职责 |
|---|---|
| 用户 | 打开「允许 AI 以 root 执行」、在 attach 与每条命令时审批、必要时输入 sudo 密码 |
| Chena-Shell | 保存/使用 sudo 密码、包装提权命令、执行三层策略、写审计 |
| AI 客户端 | 申请 `sudo` 权限、提交命令、处理 `WAIT_CONFIRM`、结束时 detach |

前提：

1. 目标会话 `connected = true` 且 `ai_allowed = true`；
2. 服务器上该登录用户具备 sudo 权限（否则返回 `SUDO_NOT_PERMITTED`）；
3. 设置中「允许 AI 以 root 执行」已开启（默认关闭）。

---

## 4. 核心概念

### 4.1 三种秘密，互不替代

| 秘密 | 产生位置 | 用途 | AI 可见 |
|---|---|---|---|
| MCP 访问凭证 | 设置 → MCP 服务 | 连接 MCP Server | 否（用户填客户端） |
| `attach_token` | `session_attach` 返回 | 会话操作鉴权 | 是（仅本次调用链） |
| sudo 密码 | 用户输入 / 凭据管理器 | 提权 | **绝对不可见** |

### 4.2 三种接入模式（本方案只涉及前两种）

| mode | 能力 | 可否带 sudo |
|---|---|---|
| `write` | 执行命令（登录用户） | 可（+`permissions:["sudo"]`） |
| `read` | 只读回放 | 否 |
| `pty`（未来方案 A） | 接管交互终端 | 不适用 |

### 4.3 root 执行的定义

一次 sudo exec = 一次独立的 SSH exec 通道 + 一次 sudo 提权 + 一条命令 + 一次退出。命令之间**不共享**工作目录、环境变量、后台进程。

---

## 5. 权限模型

### 5.1 权限组合规则

```
permissions = []                    → 仅 exec/read/detach（现状）
permissions = ["sftp"]              → + SFTP 读写
permissions = ["sudo"]              → + 以 root 执行
permissions = ["sftp", "sudo"]      → 同时具备（一次审批，弹窗逐项列出）
```

约束：

1. `sudo` 只能与 `mode: "write"` 搭配；`read` + `sudo` 返回 `INVALID_PERMISSION`。
2. 未知权限值一律拒绝（不做静默忽略），返回 `INVALID_PERMISSION`。
3. `sudo` 权限随 `attach_token` 一起失效：令牌过期、空闲断开、detach 后立即不可用。
4. sudo 权限**不提升** SFTP 权限；SFTP 仍以登录用户身份执行。

### 5.2 策略执行顺序（不可跳过、不可重排）

```
命令提交
  → 解析真实意图（拒绝编码/拆分绕过）
  → 会话黑名单（该会话 ai_blocklist_regexes）
  → 全局黑名单
  → sudo 专用黑名单
  → sudo 专用确认清单
  → 通用确认清单
  → 人工确认（逐条，一次批准只消费一次）
  → 提权执行
```

命中任一层黑名单 → `BLOCKED`，**不得重试、不得改写**。

---

## 6. 设置项（设置 → AI 安全）

| 设置项 | 默认值 | 说明 |
|---|---|---|
| 允许 AI 以 root 执行 | **关闭** | 总开关；关闭时 attach 申请 sudo 直接返回 `SUDO_DISABLED` |
| sudo 密码来源 | 会话密码优先 | 选项：会话密码 / 仅手动输入 / 仅免密 |
| sudo 密码缓存 | 本次 attach 内有效 | 可选：不缓存 / 本次 attach / 本次应用运行 |
| root 命令逐条确认 | **开启** | 关闭后仍走黑名单，但不弹确认（高风险，需二次确认才可关闭） |
| sudo 专用黑名单 | 见 6.1 | 正则列表，命中即拒绝 |
| sudo 专用确认清单 | 见 6.2 | 命中需人工批准 |
| 单次 attach 的 sudo 次数上限 | 50 | 超出返回 `SUDO_QUOTA_EXCEEDED` |
| sudo 失败锁定 | 连续 3 次密码错误锁定 5 分钟 | 返回 `SUDO_AUTH_FAILED` |

### 6.1 sudo 专用黑名单（默认条目）

```
\bsudo\s+-i\b
\bsudo\s+-s\b
\bsudo\s+su\b
\bvisudo\b
/etc/sudoers
\bpasswd\b
\.ssh/authorized_keys
\bchattr\b\s+\+i
\bdd\b.*\bof=/dev/
\brm\s+-rf\s+/\s*$
```

### 6.2 sudo 专用确认清单（默认条目）

```
\brm\b
\bsystemctl\s+(restart|stop|disable)
\bkill\b|\bpkill\b|\bkillall\b
\biptables\b|\bnft\b|\bufw\b
\btee\b|\bsed\s+-i\b|>\s*/etc/
\buseradd\b|\busermod\b|\buserdel\b
\bchmod\b|\bchown\b
\bmodprobe\b|\bsysctl\s+-w\b
```

---

## 7. 用户交互流程

### 7.1 流程总览

```
AI: session_attach(mode=write, permissions=["sudo"], reason=...)
        │
        ▼
Chena-Shell 弹窗「AI 申请以 root 权限执行命令」
        │  拒绝 ──▶ CONFIRMATION_NOT_APPROVED（AI 停止）
        ▼  批准
attach_token（permissions 含 sudo）
        │
        ▼
AI: session_exec(cmd)
        │
        ├─ 黑名单命中 ──▶ BLOCKED（结束）
        │
        ├─ 需确认 ──▶ WAIT_CONFIRM(kind=sudo) ──▶ 用户批准 ──▶ AI 带 id 重试
        │
        ▼
Chena-Shell 提权执行（必要时弹窗要密码）
        │
        ▼
返回 stdout / stderr / exit_code / run_as=root
        │
        ▼
AI: session_detach（用户确认无后续需求后）
```

### 7.2 attach 审批弹窗（必含信息）

```
标题：AI 请求接入并申请 root 权限
内容：
  会话：admin@203.0.113.45:22
  客户端：<client_id>
  模式：write + sudo
  理由：<reason 原文>
  权限说明：
    · 可执行命令（登录用户）
    · 可以 root 身份执行命令（逐条确认）
    · 有效期 30 分钟 / 空闲 10 分钟断开
  风险提示：root 权限可读取与修改系统全部文件。
按钮：批准 / 拒绝
```

### 7.3 单条命令确认弹窗（必含信息）

```
标题：AI 请求以 root 执行命令
内容：
  命令（原文，等宽显示，不截断）
  命中规则：<rule 名称，如"sudo 确认清单：rm">
  影响提示：<按规则映射的风险描述>
  执行身份：root
按钮：批准一次 / 拒绝
说明：本批准仅对这一次命令有效
```

### 7.4 sudo 密码弹窗

触发条件：`sudo -n` 失败且密码来源允许手动输入。

```
标题：需要 sudo 密码
内容：AI 正在请求以 root 执行命令，请输入 <用户名> 的 sudo 密码。
字段：密码（掩码输入，不显示明文）
选项：□ 本次接入内记住
按钮：确认 / 取消
```

取消 → `SUDO_PASSWORD_REQUIRED`；连续 3 次错误 → `SUDO_AUTH_FAILED` 并锁定 5 分钟。

---

## 8. MCP 接口变更

### 8.1 `session_attach`（扩展）

```json
{
  "name": "session_attach",
  "arguments": {
    "session_id": "sess-...",
    "reason": "排查 UDP 8443 流量来源（需 tcpdump）",
    "mode": "write",
    "permissions": ["sudo"]
  }
}
```

参数变更：

| 参数 | 类型 | 变更 |
|---|---|---|
| `permissions` | string[] | 枚举由 `["sftp"]` 扩展为 `["sftp","sudo"]` |

成功返回（新增字段）：

```json
{
  "ok": true,
  "attach_token": "tk_...",
  "mode": "write",
  "permissions": ["exec", "read", "detach", "sudo"],
  "expires_in_seconds": 1800,
  "sudo": {
    "enabled": true,
    "require_confirm": true,
    "quota_remaining": 50
  }
}
```

### 8.2 `session_exec`（扩展）

```json
{
  "name": "session_exec",
  "arguments": {
    "session_id": "sess-...",
    "attach_token": "tk_...",
    "command": "ss -ulnp",
    "run_as": "root",
    "cwd": "/root",
    "timeout_ms": 30000
  }
}
```

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `run_as` | `"user" \| "root"` | 否 | 默认 `user`；`root` 需要 attach 时已授予 sudo |
| `cwd` | string | 否 | root 执行时的工作目录，默认 `/root`；仅接受绝对路径 |
| `timeout_ms` | int | 否 | 1000–300000，默认 30000（root 命令建议 ≤60000） |

**注意：不提供 `sudo_password` 参数。** 密码只能由用户在 Chena-Shell 界面输入。

成功返回（新增字段）：

```json
{
  "ok": true,
  "run_as": "root",
  "sudo_used": true,
  "cwd": "/root",
  "command": "ss -ulnp",
  "stdout": "...",
  "stderr": "",
  "exit_code": 0,
  "duration_ms": 412
}
```

### 8.3 工具清单变化

不新增工具，`tools/list` 仍为 18 个（现状核实：`mcp_tools.rs::tools_list` 已定义 18 个，`session_attach`/`session_exec` 均在其中）；
仅 `session_attach` / `session_exec` 的参数与返回值扩展，工具的 `inputSchema` 描述文本需同步更新 `permissions` 枚举与 `run_as`/`cwd` 说明。

---

## 9. 返回值与错误码

| error | 触发条件 | AI 应对 |
|---|---|---|
| `SUDO_DISABLED` | 设置中总开关关闭 | 停止，告知用户去开启 |
| `INVALID_PERMISSION` | 权限值非法或 `read` + `sudo` | 停止，修正参数 |
| `SUDO_NOT_PERMITTED` | 服务器 sudoers 不允许该用户 | 停止，说明需管理员配置 |
| `SUDO_PASSWORD_REQUIRED` | 需要密码但用户取消输入 | 停止，不要重试 |
| `SUDO_AUTH_FAILED` | 密码错误（含锁定） | 停止，不得重试 |
| `SUDO_QUOTA_EXCEEDED` | 超出单次 attach 的 sudo 次数 | 提示用户重新 attach |
| `WAIT_CONFIRM` | 需人工确认（`kind: "sudo"`） | 提示用户批准，带 `confirmation_id` 重试一次 |
| `CONFIRMATION_NOT_APPROVED` | 用户拒绝或未处理 | 停止，询问用户 |
| `BLOCKED` | 命中任意黑名单（带 `rule`） | 停止，不得改写绕过 |
| `BAD_TOKEN` | 令牌过期/已 detach/绑定不匹配 | 重新 attach |
| `TIMEOUT` | 命令执行超时（现状 `execute_allowed` 已返回，含 `timed_out` 字段） | 停止，不重试；可建议调大 `timeout_ms` |
| `UNSUPPORTED_INTERACTIVE` | root 执行命中交互式程序（`vim`/`top`/`less` 等） | 停止，改用非交互等价命令 |
| `session has AI disabled` | 会话禁止 AI | 停止 |

`WAIT_CONFIRM` 新增字段：

```json
{
  "ok": false,
  "error": "WAIT_CONFIRM",
  "kind": "sudo",
  "confirmation_id": "cf-...",
  "status": "pending",
  "expires_at": "2026-11-21T02:40:00Z",
  "preview": { "command": "ss -ulnp", "run_as": "root", "rule": "sudo 确认清单" }
}
```

---

## 10. 执行实现规范

### 10.1 提权命令包装

```
第一优先：sudo -n -- <shell> -lc '<command>'
第二优先：sudo -S -p '' -- <shell> -lc '<command>'   // 密码经通道 stdin 写入
```

要求：

1. 密码**只能**通过 exec 通道 stdin 写入，不得拼进命令字符串、argv、环境变量或日志。
2. 密码写入后立即从内存缓冲清除。
3. 使用 `-p ''` 抑制交互提示，避免输出被提示符污染。
4. 需要强制重新验证时使用 `-k`；默认不强制（依赖 sudo 时间戳缓存，降低密码暴露次数）。
5. 工作目录通过 `cd '<cwd>' && <cmd>` 在登录 shell 内实现，不使用 `sudo -i`。
6. 环境变量采用最小集合（`PATH`、`LANG`、`HOME=/root`），不继承 AI 侧任意环境。
7. 命令输出上限 256 KiB，超出截断并追加 `\n[truncated]` 标记。
8. **与现有退出码回显兼容**：sudo 包装内沿用现状 `exec_collect` 的 `__FORGE_RC__` 标记行机制——在 sudo 内的命令末尾追加 `rc=$?; printf '\n__FORGE_RC__%d\n' "$rc"; exit $rc`，从 stdout 尾部解析退出码，避免 russh 在通道 Close 后丢弃 `exit-status` 导致退出码丢失。
9. **sudo 失败判定**：`sudo -n` 非 0 退出时按 stderr 关键字区分——含 `is not allowed to execute` / `not in the sudoers file` → `SUDO_NOT_PERMITTED`；含 `a password is required` / `Password:` → 需密码（转 `sudo -S` 或弹窗）；其余（含 `Sorry, try again` 三次后）→ `SUDO_AUTH_FAILED`。

### 10.2 禁止的实现方式

- ❌ `echo PASS | sudo -S ...`（密码进入 argv / ps）
- ❌ 把密码写临时文件
- ❌ `sudo -i` / `sudo -s` / `sudo su`
- ❌ 在 root 执行前后修改用户交互 PTY
- ❌ 复用用户已建立的交互 sudo 会话（不可靠且不可审计）

### 10.3 超时与清理

| 情况 | 处理 |
|---|---|
| 命令超时 | 终止子进程树，返回 `TIMEOUT`，不重试 |
| 密码提示超时（30s） | 返回 `SUDO_PASSWORD_REQUIRED` |
| 输出超限 | 截断并标记 |
| 客户端断开 | 通道关闭，sudo 时间戳保留（由系统决定） |

### 10.4 输出截断（现状缺失，需新增）

> 现状：`session.rs::exec_collect` 将通道 Data 全部累加进内存，**无上限**。root 诊断命令（如 `tcpdump`、`journalctl` 未加行数限制）可能产生超大输出，因此本方案强制引入截断，并同时应用于普通 `run_as=user` 与 `run_as=root`。

- 单条命令 stdout + stderr 各自上限 **256 KiB**；超出部分丢弃，并在末尾追加 `\n[truncated]` 标记。
- 截断必须保留退出码标记行（`__FORGE_RC__`）：实现时先累计字节，达阈值后只保留尾部标记所需的最后若干字节，或在截断时确保标记行已被采集。
- 截断是「执行成功但输出被裁剪」，不改变 `exit_code`；返回值新增 `truncated: true` 字段（可选，默认 `false`），便于 AI 与用户知情。

---

## 11. 审计与回显

### 11.1 终端回显（主终端内存回放，不注入远端 shell）

```
[AI 执行 root] ss -ulnp
[AI 结果 root] <stdout / stderr>
[AI 状态 root] 成功 exit=0 duration=412ms
```

被拒绝、被阻断、等待确认的命令**不显示为已执行**。

### 11.2 审计日志字段

| 字段 | 说明 |
|---|---|
| `ts` | 时间戳 |
| `session_id` / `host` / `username` | 目标 |
| `client_id` / `attach_id` | 来源 |
| `mode` / `permissions` | 授权范围 |
| `run_as` | `user` / `root` |
| `command` | 原文（敏感值按脱敏规则处理） |
| `rule` | 命中的黑名单/确认清单规则 |
| `confirmation_id` / `approved_at` | 审批链路 |
| `exit_code` / `duration_ms` | 结果 |
| `sudo_used` / `sudo_auth` | 是否提权、认证方式（免密/密码/缓存） |

日志中**不得**出现 sudo 密码、MCP 凭证、`attach_token`。

---

## 12. 安全约束（强制项）

1. sudo 能力默认关闭，且 attach 审批必须明示"将以 root 执行"。
2. 密码不进 AI 上下文、不进命令行、不进日志、不进回放、不进报告。
3. 密码仅保存在内存；落盘仅允许 Windows 凭据管理器（用户显式勾选时）。
4. 一次批准只消费一次；`confirmation_id` 过期即失效。
5. `BLOCKED` 后不得重试、不得改写/编码/拆分命令绕过。
6. sudo 权限随 `attach_token` 失效而失效，不跨 attach 复用。
7. 禁止在 sudo 模式下执行交互式程序（`vim`、`top`、`less`、`sudo -i`）。
8. 用户拒绝即终止，不得换工具或换参数重试同一意图。
9. 连续密码错误触发锁定，锁定期间任何 sudo 请求直接失败。
10. `detach` 为必做收尾；未 detach 的空闲会话由服务端在 10 分钟后断开。

---

## 13. 状态机

```
IDLE
 └─ attach(write, sudo) ──▶ PENDING_APPROVAL
      ├─ 拒绝 ──▶ IDLE
      └─ 批准 ──▶ ATTACHED(sudo)

ATTACHED(sudo)
 ├─ exec(run_as=user) ──▶ 执行（登录用户）
 ├─ exec(run_as=root)
 │    ├─ 黑名单 ──▶ BLOCKED（保持 ATTACHED）
 │    ├─ 需确认 ──▶ WAIT_CONFIRM ──┬─ 批准 ──▶ 执行
 │    │                            └─ 拒绝 ──▶ 保持 ATTACHED
 │    ├─ 需密码 ──▶ PASSWORD_PROMPT ──┬─ 成功 ──▶ 执行
 │    │                               └─ 取消/失败 ──▶ 保持 ATTACHED
 │    └─ 执行 ──▶ 返回结果（保持 ATTACHED）
 └─ detach / 令牌过期 / 空闲 10 分钟 ──▶ IDLE（sudo 能力立即失效）
```

---

## 14. 边界与限制

| 项 | 限制 |
|---|---|
| 会话连续性 | 每条命令独立通道，`cd`、`export`、后台进程不跨命令保留 |
| 交互程序 | 不支持；需交互的命令返回 `UNSUPPORTED_INTERACTIVE` |
| 单条输出 | ≤ 256 KiB |
| 单条超时 | ≤ 300000 ms |
| sudo 次数 | 默认每次 attach ≤ 50 |
| SFTP | 不受 sudo 影响，仍为登录用户 |
| 多客户端 | sudo 权限绑定单一 `attach_token`，write 模式本身排他 |

---

## 15. 兼容性与降级

1. 旧客户端不传 `permissions` → 行为与现状完全一致。
2. 旧客户端传未知权限值 → 明确报 `INVALID_PERMISSION`，不做静默降级。
3. 服务端未实现 sudo（旧版本 Chena-Shell）→ attach 返回 `INVALID_PERMISSION`；AI 应提示升级，而不是改走 `ssh` 直连。
4. 用户未开启总开关 → `SUDO_DISABLED`，AI 给出开启路径，不反复重试。

---

## 16. 验收测试用例

| # | 场景 | 期望 |
|---|---|---|
| 1 | 未开启总开关时 attach 申请 sudo | `SUDO_DISABLED` |
| 2 | `mode=read` + `permissions=["sudo"]` | `INVALID_PERMISSION` |
| 3 | 未知权限 `["docker"]` | `INVALID_PERMISSION` |
| 4 | 正常 attach 并批准 | 返回 `permissions` 含 `sudo`，`quota_remaining=50` |
| 5 | `exec(run_as=root, "id -u")` | `stdout=0`，`run_as=root` |
| 6 | `exec(run_as=root)` 但 attach 未授权 sudo | 拒绝，不执行 |
| 7 | `exec(run_as=root, "sudo -i")` | `BLOCKED`，`rule` 指向 sudo 黑名单 |
| 8 | `exec(run_as=root, "rm -rf /tmp/x")` | `WAIT_CONFIRM(kind=sudo)`；批准后执行；拒绝则不执行 |
| 9 | 同一 `confirmation_id` 消费两次 | 第二次 `confirmation not found` |
| 10 | 密码错误 3 次 | 第 3 次起 `SUDO_AUTH_FAILED`，锁定 5 分钟 |
| 11 | 用户取消密码弹窗 | `SUDO_PASSWORD_REQUIRED`，命令未执行 |
| 12 | 密码输入正确 | 命令执行成功，日志中无明文密码 |
| 13 | `detach` 后复用旧 token 执行 root | `BAD_TOKEN` |
| 14 | 第 51 次 sudo 执行 | `SUDO_QUOTA_EXCEEDED` |
| 15 | 输出 1 MB | 截断到 256 KiB 并带 `[truncated]` |
| 16 | `cwd=/tmp` 执行 `pwd` | 输出 `/tmp` |
| 17 | 执行 `ls` 观察用户交互终端 | 终端状态不变、无注入、无标签切换 |
| 18 | 审计日志核查 | 完整记录 run_as/rule/confirmation，无任何秘密 |

### 16.1 实机验收记录（2026-11-22，v1.0.0-1）

以下为当前版本实测结果，不改变上表的设计验收要求。

| 测试 | 结果 | 实测摘要 |
| --- | --- | --- |
| A. 普通 exec | 通过 | 普通用户命令执行不受影响 |
| B. 黑名单阻断 | 通过 | 命中规则后拒绝执行 |
| C. root 提权确认流 | 通过 | 人工批准后以 root 执行 |
| D. `cwd` | 通过 | 指定工作目录生效 |
| E. 配额 | 通过 | 超出上限后返回 `SUDO_QUOTA_EXCEEDED` |
| F. 手动密码与错误锁定 | **待复测** | M1 实测时错误密码约 30 秒后返回 `TIMEOUT`；M2 已修复为返回 `SUDO_AUTH_FAILED` 并计入锁定，待按用例 10 真机复测 |
| G. 终端与 detach | 通过 | 未向交互 PTY 注入文本；detach 后旧 token 返回「token 无效或已过期」 |

**M2 修复状态（2026-11-22）**：错误密码已改为返回 `SUDO_AUTH_FAILED` 并计入连续失败次数，达到「密码错误锁定次数」后按「sudo 锁定时长」锁定；自动化测试已覆盖返回码与锁定路径。真机复测用例 10 通过后，再将本行标记为「通过」。

完整证据见 [Chena-Shell sudo 实机测试报告](../../Chena-Shell_sudo实机测试报告.md)。

---

## 17. 文档与 skill 更新清单

1. `SKILL.md`
   - `session_attach.permissions` 枚举补 `sudo`
   - `session_exec` 补 `run_as` / `cwd` 参数与返回字段
   - 新增「root 执行规范」：先只读、逐条确认、不重试 `BLOCKED`、禁止交互式
   - 排障表补 `SUDO_*` 错误码
2. `AI-使用说明.md`
   - 工具表更新（参数/返回）
   - 新增「root 权限申请流程」时序
   - 明确「密码永不经过 AI」
3. `使用手册.md`
   - 设置项：允许 AI 以 root 执行、密码来源、次数上限
   - 审批弹窗说明
   - 安全建议与审计查看方式

---

## 18. 风险与后续演进

| 风险 | 缓解 |
|---|---|
| root 命令破坏性放大 | 默认关闭 + 逐条确认 + 专用黑名单 + 次数上限 |
| 用户误批 | 弹窗展示命令原文与命中的规则、风险描述 |
| 密码泄露 | 仅内存 + stdin 传入 + 不落日志；可选凭据管理器 |
| 审计缺失 | 强制记录 run_as / rule / confirmation 链路 |
| 用户希望"接管同一个 shell" | 属方案 A（PTY 接管），另行设计与评估 |

后续可演进：

- `run_as` 支持指定其他账户（`sudo -u`），仍走同一确认链路；
- sudo 专用命令白名单（完全放行 + 不弹确认），用于高频只读诊断；
- 与方案 A 并存：`mode: "pty"` 作为更高风险档位，需独立开关。

---

## 19. 实施方案（对照 Chena-Shell 1.0.0-1 代码现状）

> 本节是 v1.1 新增，把上面的规格落到具体文件与里程碑。所有结论均来自对 `src-tauri/src` 的实际核对。

### 19.1 现状核对结论

| 机制 | 现状位置 | 与本方案的关系 |
|---|---|---|
| MCP 工具清单 | `mcp_tools.rs::tools_list`（25–236 行），共 18 个工具 | 不新增工具，仅扩展 `session_attach`/`session_exec` 的 schema 描述 |
| attach 权限解析 | `mcp_tools.rs::attach_request`（460–487 行）：仅识别 `sftp`，未知权限返回 `unsupported attach permission` | 需新增识别 `sudo`，并校验 `mode=write` |
| attach 权限存储 | `session.rs`：`attach_permissions: Mutex<Vec<String>>`（1563 行），`begin_write_attach_with_client` 写入 `exec/detach/read/sftp`（2316 行） | 直接把 `"sudo"` 加入该列表即可，`validate_attach_token` 已按 permission 字符串校验（2389–2435 行），无需改校验框架 |
| exec 主流程 | `mcp_tools.rs::session_exec`（637–775 行）→ 安全策略 `check` → `execute_allowed`（777 行）→ `handle.exec_with_timeout` | 需插入 `run_as` 解析、sudo 总开关/权限校验、sudo 策略层、root 执行分支 |
| 底层命令执行 | `session.rs::exec_collect`（865–912 行）：`channel_open_session` + `ch.exec` + `ch.wait` 收 Data/ExtendedData/ExitStatus | **不支持写 stdin**（sudo `-S` 传密码必须新增 `ch.write` 能力）；**无输出截断**；退出码靠 `RC_MARK` 标记行兜底 |
| 安全策略 | `security.rs::SecurityEngine::check`（66–130 行）：三层 `session_blocklist → global_blocklist → policy(confirm_list)` | 需新增 `sudo_blocklist` / `sudo_confirm_list` 两层，root 执行时在全局层之后插入 |
| 审批 | `approval.rs`：`ConfirmationKind` 仅 `Attach/Exec`（39–42 行）；`consume_approved` 一次消费（237 行） | root exec 的确认可复用 `Exec` kind，在 `Confirmation` 上加 `run_as`/`sudo` 子标记以区分与隔离 |
| 设置 | `state.rs::GlobalSettings`（339–381 行）：有 `default_policy/global_blocklist/confirm_list/mcp_*/attach_token_ttl/idle_detach_minutes/log_ai_commands`，**无 sudo 字段**；`settings_get` 走 `SettingsDto` 白名单（state.rs 385 行起） | 需新增 8 个 sudo 设置项（见 19.2）及运行时状态；新字段需同时加入 `SettingsDto` 白名单才会对 AI 可见 |
| 会话级 AI 设置 | `SessionConfig.ai_allowed/ai_policy/ai_blocklist_regexes`（state.rs 39–41 行） | 已满足"会话黑名单"前提，无需改 |
| 秘密存储 | `secret_store.rs` 已封装 Windows 凭据管理器读写 | sudo 密码可选持久化复用此能力；默认仅内存 |
| AI 事件回显 | `state.rs::log_ai_event_detailed` + `AIEventKind`（Exec/Blocked/WaitingConfirm/Confirmed/Attach/Rejected/Expired） | 需在事件/日志字段补 `run_as`、`sudo_used`、`sudo_auth`；终端回放加 root 标记 |

### 19.2 文件级改动清单

| 文件 | 变更类型 | 要点 |
|---|---|---|
| `src-tauri/src/state.rs` | 扩展 | `GlobalSettings` 新增：`sudo_enabled: bool`(默认 false)、`sudo_password_source`(枚举：session/manual/passwordless)、`sudo_password_cache`(枚举：none/attach/app)、`sudo_require_confirm: bool`(默认 true)、`sudo_blocklist: Vec<String>`、`sudo_confirm_list: Vec<String>`、`sudo_quota_per_attach: u32`(默认 50)、`sudo_max_failures: u32`(默认 3)/`sudo_lockout_seconds: u64`(默认 300)。全部加 `#[serde(default)]` 保证旧配置兼容。`AppState` 新增运行时：按 `session_id+client_id` 维度的 sudo 密码内存缓存、失败计数与锁定截止、已用次数计数。`settings_get` 对 AI 可见走 `SettingsDto` 白名单（state.rs 385 行起），需暴露的 sudo 字段（如 `sudo_enabled`、`sudo_require_confirm`、`sudo_quota_per_attach`）加入该 DTO，敏感项（密码来源/缓存）不暴露。 |
| `src-tauri/src/session.rs` | 扩展 | ① `begin_write_attach_with_client` 增加 `sudo: bool` 入参，`attach_permissions` 含 `"sudo"` 时写入。② 新增 `exec_as_root_with_timeout(command, cwd, timeout, password_provider)`：内部开 exec channel，先 `sudo -n true` 探测，失败按 10.1.9 规则判定；需要密码时回调取密码，用 `ch.write_all(password+"\n")` 写入 stdin 后立即清内存；包装命令加 `cd`、`RC_MARK`、256KiB 截断。③ 给现有 `exec_collect` 增加可选 `stdin_payload` 与 `max_bytes` 参数（或新增变体），普通路径行为不变。④ 次数计数/锁定在调用层（mcp_tools）或 session 层维护，随 detach 清零。 |
| `src-tauri/src/security.rs` | 扩展 | `SecurityEngine` 新增 `sudo_blocklist`/`sudo_confirm_list` 字段与 `update_sudo(...)` 方法；`check` 增加 `run_as` 参数，当 `run_as=root` 时在「全局黑名单之后、通用确认清单之前」依次执行 sudo 黑名单（命中→Block）→ sudo 确认清单（命中→WaitConfirm，rule 带 `sudo:` 前缀）。`run_as=user` 路径行为完全不变。 |
| `src-tauri/src/approval.rs` | 扩展 | `Confirmation` 增加 `run_as: Option<String>`（或 `sudo: bool`）字段，序列化时透出；root exec 创建确认时写入 `run_as:"root"`，消费时校验匹配，确保 root 与普通 exec 的 `confirmation_id` 不串用。`audit_resolution` 对 root 拒绝/过期单独标记。 |
| `src-tauri/src/mcp_tools.rs` | 扩展 | ① `attach_request` 识别 `sudo`，`read+sudo`/未知权限返回 `INVALID_PERMISSION`；`approve_attach` 返回 `sudo{enabled,require_confirm,quota_remaining}` 子对象。② `session_exec` 解析 `run_as`(默认 user)/`cwd`(默认 `/root`，校验绝对路径)；`run_as=root` 分支：查总开关→`SUDO_DISABLED`；`validate_attach_token(...,"sudo")`；查配额→`SUDO_QUOTA_EXCEEDED`；查锁定→`SUDO_AUTH_FAILED`；跑 `security.check(command, run_as=root)`；Block→`BLOCKED`；WaitConfirm→建 root 确认返回 `WAIT_CONFIRM(kind:"sudo")`；通过→取密码（会话密码/缓存/弹窗）→调 `exec_as_root_with_timeout`→按结果映射错误码与返回字段。③ 新建密码弹窗：通过 Tauri 事件请求前端输入，带超时，结果经内存回传，不进任何日志/参数。 |
| `src-tauri/src/lib.rs` | 微调 | 若新增前端请求 sudo 密码的 invoke 命令（如 `sudo_password_submit` / `sudo_password_cancel`），注册到 `invoke_handler`。 |
| 前端 `SettingsDialog.jsx` | 扩展 | AI 安全分类新增 sudo 设置区块：总开关（关闭时灰化其余项）、密码来源下拉、缓存策略下拉、逐条确认开关（关闭需二次确认）、sudo 黑名单/确认清单编辑（textarea，每行一条正则）、次数上限、失败锁定参数。 |
| 前端 `ApprovalDialog.jsx` | 扩展 | attach 审批：当 permissions 含 sudo 时显示「将以 root 身份执行命令（逐条确认）」与风险提示。新增 root 单条命令确认弹窗：显示命令原文（等宽不截断）、命中规则、`执行身份: root`、「批准一次/拒绝」。 |
| 前端新增 `SudoPasswordDialog.jsx` | 新增 | 密码掩码输入、「本次接入内记住」勾选、确认/取消；监听后端事件弹出，提交走 `invoke('sudo_password_submit', {session_id, password, remember})`，密码仅在前端内存短暂停留，提交后清空；取消走 `sudo_password_cancel`。 |
| 前端终端回放 | 微调 | root 执行的回显前缀改为 `[AI 执行 root]` / `[AI 结果 root]` / `[AI 状态 root]`（见 11.1）；被拒/阻断/待确认不显示为已执行。 |
| `src-tauri` 测试 | 新增 | 单元测试覆盖：权限组合与 `INVALID_PERMISSION`、`SUDO_DISABLED`、sudo 策略分层命中、confirmation 一次消费与 run_as 隔离、配额上限、失败锁定、`cwd` 校验、输出截断保留 RC_MARK。优先纯逻辑（不依赖真实 SSH），sudo 提权本体用现有 `real_openssh` E2E 环境验证。 |
| 文档 | 更新 | 按第 17 节更新 `SKILL.md` / `docs/AI-使用说明.md` / `docs/使用手册.md`。 |

### 19.3 实施顺序（4 个里程碑，每步可独立验证）

- **M1 — 后端骨架与权限链路**（无真实 SSH 也可单测）
  state.rs 加字段 → mcp_tools attach 识别 sudo → approve_attach 返回 sudo 子对象 → session_exec 解析 run_as 并对 `run_as=root` 走「总开关/权限/配额」前置校验（提权先桩返回成功）→ 单测覆盖错误码与权限。
  验收：`cargo test` 全绿；attach 带 sudo 能批准，exec root 在未授权时正确拒绝。

- **M2 — 提权执行本体**（需 SSH 主机实机）
  session.rs 新增 `exec_as_root_with_timeout`（stdin 写密码 + RC_MARK + 截断）→ sudo -n 探测与失败码解析 → 密码弹窗事件链路（后端请求→前端→回传）→ 内存缓存与凭据管理器可选持久化。
  验收：实机 `exec(run_as=root,"id -u")` 返回 `0`；日志无明文密码；取消密码返回 `SUDO_PASSWORD_REQUIRED`。

- **M3 — 策略与安全闭环**（单测 + 实机）
  security.rs sudo 层 → approval run_as 隔离 → 逐条确认 → 次数上限 → 失败锁定 → 输出截断 → 审计字段与 root 回显标记。
  验收：第 16 节 1–18 用例逐一通过；`rm` 命中确认、`sudo -i` 命中 Block、第 51 次返回 `SUDO_QUOTA_EXCEEDED`。

- **M4 — 前端与文档交付**（构建 + 实机验收）
  设置面板 + 三类弹窗 + 终端 root 回显 → 更新 SKILL/使用说明/使用手册 → `npm run tauri build -- --no-bundle` → 覆盖 release 绿色版。
  验收：设置默认关闭时申请 sudo 直接 `SUDO_DISABLED`；开启后完整跑通 attach→确认→root 执行→detach 链路；用户交互终端无任何注入。

### 19.4 关键技术决策

1. **密码只走 stdin，不进 argv/环境/日志**：`sudo -S -p ''`，密码写入后立即 `zeroize`/覆盖内存缓冲；前端提交后清空输入框状态。
2. **sudo 权限预探测**：每个 attach 的首次 root exec 前 `sudo -n true`，缓存结果（允许/需密码/不允许），避免每条命令重复探测；但 `SUDO_NOT_PERMITTED` 一旦命中即该 attach 内不再尝试。
3. **不接管、不污染交互 PTY**：root exec 一律走独立 exec channel（与现状 `session_exec` 同一条 IO 线程的独立通道），绝不向用户的交互 shell 写入。
4. **向后兼容**：旧客户端不传 `permissions`/`run_as` → 走原路径，行为字节级不变；`tools/list` 数量不变，仅 schema 描述扩展。
5. **密码持久化默认关**：仅当用户勾选「记住」且选择「本次应用运行」或凭据管理器时才落盘；attach 级缓存随 detach 立即清除。

### 19.5 风险与缓解

| 风险 | 缓解 |
|---|---|
| `sudo -S` 写 stdin 的时序竞态（sudo 未就绪就写密码） | 用 `-p ''` 抑制提示符，先 `exec` 再等待通道可写；失败回退到重新探测 |
| 服务器 `requiretty` 导致 `sudo -n`/`-S` 在非 TTY exec 通道失败 | 预探测识别此情形，返回明确错误并提示管理员关闭 `requiretty` 或配置 `NOPASSWD`；不尝试伪造 TTY |
| 输出截断破坏 `RC_MARK` 解析 | 截断逻辑优先保留尾部标记；标记行本身很短，可在累计时单独提取 |
| 用户在弹窗输入会话密码（非 sudo 密码）导致锁定 | 弹窗明确标注「sudo 密码」，并显示目标用户名；锁定 5 分钟后自动恢复，不永久封禁 |
| root 命令破坏性被误批 | 默认关闭 + 逐条确认 + sudo 黑名单 + 次数上限 + 弹窗展示命令原文与命中规则（多层冗余） |

---

## 附录 A：术语表

| 术语 | 含义 |
|---|---|
| attach（接入） | AI 客户端向 Chena-Shell 申请操作某个会话，获批后拿到 `attach_token` |
| detach（断开接入） | AI 主动结束本次接入，令牌与 sudo 能力立即失效 |
| `attach_token` | attach 成功后发放的临时令牌，用于后续 `session_exec` 鉴权 |
| exec 通道 | 独立于用户交互终端的命令执行通道，命令跑完即关闭，不留登录态 |
| PTY | 伪终端，即用户看到的交互式 shell；本方案**绝不**向它写入 |
| `run_as` | 执行身份，`user`（登录用户）或 `root` |
| 黑名单 | 命中即拒绝执行，返回 `BLOCKED`，不弹窗、不允许重试 |
| 确认清单 | 命中后弹窗，用户批准一次才执行，一次批准只消费一次 |
| `confirmation_id` | 一次审批的凭据，用后即失效，过期也失效 |
| 配额 | 单次 attach 内允许的 root 执行次数上限，默认 50 |
| 免密 sudo | 服务器配置了 `NOPASSWD`，`sudo -n` 即可执行，不弹密码框 |
