# Chena-Shell

> AI 友好型本地 SSH 终端 · SFTP 文件传输 · 主机监控 · 内置 MCP Server

Chena-Shell 是一款运行在 Windows 本地的桌面 SSH 工具。它不经过任何云端中转，连接、密钥、日志和配置都留在你自己的机器上。

除了常规终端，它把「AI 助手接入」当作一等能力：AI 通过标准 MCP 协议以访客身份附加到指定会话，执行前需要人工审批，执行过程有独立只读回显，命令与输出会落盘留痕。

当前版本：**V1.0.0.12**（仅 Windows x64 绿色版）

## 功能

### 终端与会话

- 会话的创建、编辑、连接、断开、删除、分组与自动连接
- 密码、私钥文件、直接粘贴私钥内容、加密私钥口令、OpenSSH Agent、Pageant
- 有序多跳 ProxyJump，每跳独立认证与主机密钥校验
- TOFU 主机密钥信任；密钥变化时必须核对新旧指纹并明确批准
- 基于 xterm 的终端：分屏工作区、搜索、右键菜单、快捷指令、端口转发
- 同一会话可复制 SSH 渠道并开多个标签
- Vim 等应用的鼠标协议支持；Keepalive、连接超时、自动重连与有界退避
- 终端日志默认开启，支持保留天数、ANSI 清理与常见敏感内容脱敏

### Chena-Sftp 文件传输

- 真实 SFTP 协议，不依赖 shell `exec` 或 Base64 模拟
- 本地/远端目录浏览、上传、下载、递归目录传输、新建目录、删除、改权限
- 大文件流式传输，支持暂停、恢复、取消、重试与断点续传
- `.part` 临时文件 + `posix-rename@openssh.com` 原子替换
- 远端编辑冲突检测；内置 Monaco 编辑器，也可调用外部编辑器

### 主机监控

- 每个会话使用独立监控连接，不阻塞终端输入输出
- CPU、内存、磁盘、网络、进程采样
- 事件驱动更新，支持基线、超时、指数退避、数据过期状态与恢复
- 覆盖 Linux、BusyBox、Windows、macOS/BSD 的解析逻辑

### AI / MCP

- 支持 MCP Streamable HTTP 与 stdio 两种传输
- HTTP 默认仅监听 loopback，端点 `http://127.0.0.1:<端口>/mcp`，使用 `Authorization: Bearer <凭据>`
- stdio 通过 `Chena-Shell_V1.0.0.12.exe --mcp-stdio` 启动
- 读写接入令牌，与客户端、会话、连接和权限绑定，并带有效期
- `read` 只能读取终端回放；`write` 才能执行命令或按授权使用 SFTP、sudo；`diag` 只执行白名单只读诊断命令
- 任务级授权：有效期内白名单只读命令免逐条审批，写操作和高危命令仍需确认
- 审批与路径绑定、单次使用；会话/全局阻断与单次放行三级策略
- AI 执行过程显示在独立「AI 只读」标签，人类不能输入，但可选中后 `Ctrl+C` 复制
- AI 的命令、标准输出、错误输出与状态写入 AI 回显日志，重启后仍可在「日志 → AI 日志」查看

## 系统要求

- Windows 10/11 x64
- Microsoft Edge WebView2 Runtime（便携版不会自动安装，缺失时请从微软官网安装）
- 源码构建另需：Node.js 20+、Rust 1.80+、Visual Studio Build Tools（MSVC）与 Tauri 2 的 Windows 构建依赖

## 运行

1. 解压便携包 `Chena-Shell_V1.0.0.12_portable.zip`，不要只复制 EXE
2. 运行 `Chena-Shell_V1.0.0.12.exe`
3. 无需安装；配置与日志写入 `%APPDATA%\chena-shell`

> 当前可执行文件未做 Authenticode 代码签名，Windows SmartScreen 可能提示「未知发布者」。请核对发布页提供的 SHA-256 后再运行。

## 从源码构建

```powershell
npm install
npm run tauri dev
```

发布绿色版（lint、测试、构建、打包、启动冒烟一体）：

```powershell
npm run release
```

| 命令 | 用途 |
| --- | --- |
| `npm run dev` | 只启动 Vite 前端开发服务器 |
| `npm run tauri dev` | 启动 Tauri 桌面开发环境 |
| `npm run lint` | ESLint 检查 |
| `npm test` | 前端 Vitest 测试 |
| `npm run build` | 构建前端生产资源 |
| `npm run release` | 完整发布门禁并生成绿色版 EXE/ZIP |
| `npm run test:mcp:sdk` | 官方 MCP SDK 专项测试（需要真实测试环境） |

Rust 侧检查在 `src-tauri` 目录执行：

```powershell
cargo fmt --check
cargo test --locked
cargo check --release --locked
```

## 目录结构

```text
chena-shell/
├─ src/                      # React 前端（终端、SFTP、设置、审批、监控）
├─ src-tauri/                # Rust 后端、能力清单与 Tauri 配置
│  └─ src/                   # session / sftp / monitor / mcp_* / logging / security
├─ vendor/                   # 本地修补的 pageant、russh-sftp
├─ docs/                     # 使用手册、AI 使用说明、更新记录与设计资料
├─ scripts/                  # 发布脚本与 MCP 冒烟脚本
├─ public/                   # 静态资源与 Pyodide 运行资源
├─ .trae/skills/chena-shell/ # 可自安装的 AI 接入 skill
└─ .github/workflows/        # Windows CI（lint / test / build / cargo check）
```

后端模块划分：

| 模块 | 职责 |
| --- | --- |
| `session.rs` | SSH 会话、终端 I/O、附加与重连 |
| `host_key.rs` | TOFU 与指纹绑定的主机密钥替换 |
| `sftp.rs`、`sftp_transfer.rs` | SFTP 操作与流式传输 |
| `monitor.rs` | 独立监控连接、采样解析与事件发布 |
| `mcp_server.rs`、`mcp_stdio.rs`、`mcp_tools.rs` | MCP 传输、工具、只读白名单与参数校验 |
| `approval.rs`、`security.rs` | 审批、安全策略与单次授权 |
| `state.rs`、`secret_store.rs` | 加密配置、迁移与秘密引用 |
| `logging.rs`、`sftp_log.rs` | 日志目录、脱敏、保留策略与 AI/SFTP 审计 |

## 技术栈

Tauri 2 · Rust 2021（MSRV 1.80）· React 18 · Vite 8 · xterm · `russh 0.53` · `russh-sftp 2.4.0`

## AI 接入

简要流程：

1. 在「设置 → AI 接入」启用 MCP 并生成本地访问凭据
2. 把 `docs/AI-使用说明.md` 与 `.trae/skills/chena-shell/SKILL.md` 一起交给 AI 助手
3. AI 依次调用 `session_attach`、`session_read`、`session_write` / `session_diag`、`session_detach`

完整工具清单、错误码与审批语义见 [AI 使用说明](docs/AI-使用说明.md)。

## 安全设计

- 会话、设置与快捷指令保存在加密配置中；配置主密钥由当前用户 Windows DPAPI 保护
- Windows 密码、私钥口令、粘贴的私钥内容存放在 Windows Credential Manager，配置文件只保存引用
- 正式版不编译 `devtools` 特性，不注册开发者工具命令，不响应 F12 / `Ctrl+Shift+I`
- 不注册 `tauri-plugin-shell` / `tauri-plugin-dialog`，`withGlobalTauri` 为 `false`，页面不暴露 `window.__TAURI__`
- 严格 CSP：`script-src 'self' 'wasm-unsafe-eval'`，禁止 frame 与外部连接
- 只读诊断白名单按完整命令或参数边界匹配，拒绝 shell 元字符、命令替换、通配符、`..` 穿越、敏感路径与危险参数；`root` 登录会话仅放行内置清单
- MCP 管理类操作（建会话、连接、断开）必须经人工确认；端口转发需要 writer attach_token，非回环监听额外审批
- AI 建立会话或连接前，主机密钥必须已由人工核验
- 日志统一脱敏，并按保留天数清理

安全边界不是「只读即安全」：日志、配置、进程参数以及 `docker inspect`、`systemctl show` 的输出仍可能包含口令或令牌，授权前请按目标主机敏感度控制范围。

## 已知限制

- 只在 Windows x64 上完成构建与验证，其他平台未验证
- 仅提供绿色版，不生成 NSIS/MSI 安装包
- 可执行文件未签名，SmartScreen 可能拦截
- AI 只读白名单是启发式防护，不等于沙箱隔离
- 部分 AI 安全策略（sudo、diag、任务级授权）在 Kali 实机上的完整验收仍在进行

## 文档

| 文档 | 说明 |
| --- | --- |
| [使用手册](docs/使用手册.md) | 安装、会话、终端、SFTP、设置与常见问题 |
| [AI 使用说明](docs/AI-使用说明.md) | MCP 端点、工具清单、审批与排障 |
| [更新记录](docs/更新记录.md) | 版本变更与升级注意事项 |
| [SKILL.md](.trae/skills/chena-shell/SKILL.md) | 供 AI 助手自安装的接入 skill |

## 许可证

本项目采用 **Apache License 2.0**，完整条款见 [LICENSE](LICENSE)。

- 允许商业使用、修改、分发和私人使用，需保留版权与许可声明，并附带许可证副本
- 分发修改后的文件时，需说明修改内容
- 附带专利授权条款；同时提供 NOTICE 相关要求
- 不提供任何担保

## 贡献

欢迎 Issue 与 Pull Request。提交前请确认：

- `npm run lint`、`npm test`、`cargo test` 全部通过
- 不提交真实主机地址、账号、密码、私钥、访问令牌或任何用户凭据
- 新增功能附带对应测试，并同步更新 `docs` 中的中文文档
