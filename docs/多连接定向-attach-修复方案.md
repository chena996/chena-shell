# 多连接定向 attach 修复方案（P0 产品缺陷）

> 问题域：同一会话配置下存在多个终端连接（主 Shell + 复制 SSH 渠道）时，MCP 的 `session_attach` / `session_read` 无法表达「绑定到哪个连接」，AI 可能读到与用户屏幕不一致的回放。
> 结论：按 P0 产品缺陷修复。核心原则是 **attach 时固定绑定一个终端连接；后续读取始终针对该连接；活跃标签变化必须可检测，不能静默改绑**。

---

## 一、现状根因

已确认：

- `session_attach` 只接收 `session_id`，token 绑定的是整个 `SessionHandle`。
- `session_read` 固定调用 `handle.read_terminal()`，读取主 Shell replay。
- 复制 SSH 渠道虽然有 `channel_id`，但 `ExtraChannelState` 只有挂载前缓存，没有可供 MCP 读取的独立序列化 replay。
- 前端 `selectTab` / `selectPane` 只向 App 上报 `sessionId`，后端不知道当前活跃的是哪个 `channelId`。

补充说明：当前「复制 SSH 渠道」是同一 SSH transport 上的独立 Shell channel。因此 `connection_id` 应定义为「可接入的终端连接实例」，不要等同于 TCP 连接。

**仅在 attach 返回中增加几个字段不够，必须同时改造 连接注册、replay、token 绑定和活跃标签上报。**

---

## 二、统一连接模型

为主 Shell 和每个复制渠道生成连接描述：

```json
{
  "connection_id": "7:ch1",
  "kind": "extra",
  "channel_id": "ch1",
  "generation": 7,
  "connected_at": "2026-11-22T10:30:00Z",
  "login_user": "kali",
  "prompt_user": null,
  "prompt_user_source": "unavailable",
  "prompt_observed_at": null,
  "is_active": true,
  "label": "Kali [2]"
}
```

规则：

- 主 Shell：`connection_id = "<generation>:primary"`
- 复制渠道：`connection_id = "<generation>:<channel_id>"`
- `generation` 必须包含在身份中，防止重连后把旧 token 静默绑定到新连接。
- `connected_at` 在 PTY / Shell 建立成功后记录。
- `tab_id`、`pane_id` 只作为 UI 元数据，不能作为 MCP 的稳定绑定键。

---

## 三、MCP 接口改造

### 1. session_list 暴露候选连接

每个会话增加：

```json
{
  "active_connection_id": "7:ch1",
  "connections": [
    {
      "connection_id": "7:primary",
      "kind": "primary",
      "connected_at": "2026-11-22T10:00:00Z",
      "login_user": "kali",
      "is_active": false
    },
    {
      "connection_id": "7:ch1",
      "kind": "extra",
      "channel_id": "ch1",
      "connected_at": "2026-11-22T10:30:00Z",
      "login_user": "kali",
      "is_active": true
    }
  ]
}
```

AI 可以先列连接，再明确选择，不必新增强制调用的工具。

### 2. session_attach 增加定向参数

```json
{
  "session_id": "sess-...",
  "connection_id": "7:ch1",
  "require_active": true,
  "reason": "读取用户当前终端并排查问题"
}
```

语义：

- `connection_id` 可选：明确绑定某个连接。
- `require_active=true`：要求目标是用户当前活跃终端，并在后续读取时持续检查。
- 两者都省略：
  - 只有一个连接：保持旧行为，直接绑定。
  - 多个连接且活跃连接明确：绑定活跃连接，返回 `selection_reason="active"`。
  - 多个连接但活跃状态未知：返回 `AMBIGUOUS_CONNECTION` 和候选列表，**不再默认读主 Shell**。

成功响应：

```json
{
  "ok": true,
  "attach_token": "<token>",
  "session_id": "sess-...",
  "connection_id": "7:ch1",
  "generation": 7,
  "connected_at": "2026-11-22T10:30:00Z",
  "login_user": "kali",
  "prompt_user": null,
  "prompt_user_source": "unavailable",
  "is_active": true,
  "active_revision": 42,
  "selection_reason": "explicit"
}
```

### 3. session_read 从 token 推导连接

不允许调用方在每次 read 时临时换 `connection_id`，防止跨连接混用 `after_seq`。

响应固定带：

```json
{
  "ok": true,
  "connection_id": "7:ch1",
  "generation": 7,
  "is_active": true,
  "active_revision": 42,
  "chunks": [],
  "next_seq": 128
}
```

若 attach 使用了 `require_active=true`，之后用户切换了标签：

```json
{
  "ok": false,
  "error": "ACTIVE_CONNECTION_CHANGED",
  "connection_id": "7:ch1",
  "active_connection_id": "7:primary"
}
```

此时不返回另一个连接的回放。AI 应停止并重新 attach，而不是自动沿用旧 `after_seq`。

---

## 四、后端必要修改

### 1. 每个连接独立 replay（`session.rs`）

- 给 `ExtraChannelState` 增加独立的 `TerminalReplayBuffer`。
- 无论前端是否挂载，都持续写入 replay。
- 前端 xterm 的挂载缓存与 MCP replay 分开，互不影响。
- 主 Shell 和复制渠道各自维护 `seq`，`after_seq` 只在所属连接内有效。

### 2. token 增加不可变绑定

```rust
struct AttachBinding {
    connection_id: String,
    generation: u64,
    require_active: bool,
}
```

- writer 和 `ReadObserver` 都保存该绑定。
- token 校验成功后返回 `AttachContext`，read / exec 使用其中的连接信息。
- 连接关闭或重连后返回 `CONNECTION_CLOSED` 或 `CONNECTION_CHANGED`，禁止自动改绑到新 generation，要求重新 attach。

### 3. 审批请求也绑定连接

审批上下文除 mode / permissions 外，还需写入：

- `connection_id`
- `generation`
- `require_active`
- 连接标签、登录用户、登录时间

用户批准时能看到「AI 要接入哪个终端」。批准消费时重新校验连接仍存在；严格模式下还要确认活跃连接未变化。

---

## 五、前端活跃连接上报

新增 Tauri 命令：

```text
set_active_terminal(session_id, channel_id, tab_id, pane_id)
```

后端将 `channel_id = null` 映射为当前主 Shell，并维护：

- `active_connection_id`
- 单调递增的 `active_revision`
- 可选的 `window_focused`

`TerminalWorkspace.jsx` 中以当前 `activeTab + activePane` 为唯一数据源，通过 effect 上报，覆盖：

- 新建 / 复制渠道
- 切换标签
- 切换分屏
- 关闭标签或窗格
- 自动激活后台连接
- 工作区清空

不建议在大量事件处理器里分别拼装上报逻辑，容易漏掉关闭和自动切换场景。

---

## 六、prompt_user 的可信边界

该字段可以提供，但必须允许为 `null`：

- `login_user`：来自会话认证配置，可信。
- `prompt_user`：表示交互 Shell 当前用户，只有拿到可靠 shell integration 上报时才填写。
- 同时返回 `prompt_user_source` 和 `prompt_observed_at`，避免把过期信息当事实。

**不要默认用正则猜提示符，也不要用独立 exec 的 `id -u` 冒充当前交互 Shell 用户。** 用户执行过 `su`、`sudo -i` 或进入容器后，这两种方式都会误判。

AI 自检应以 `connection_id + is_active + generation` 为准，提示符用户只做辅助展示。

---

## 七、实施顺序与验收

### 实施顺序

1. 后端连接注册与独立 replay：先解决复制渠道不可读的问题。
2. 活跃连接上报：打通标签、分屏与后端状态。
3. MCP 定向 attach 和 token 绑定：加入错误码及审批校验。
4. 返回元数据与 AI 自检：同步 `session_list` / `session_read` / `session_exec` 响应。
5. 更新使用手册、AI 使用说明和项目 / 发布版 skill。

### 必测场景

- 主 Shell 与复制渠道分别输出不同标记，attach 后只能读到绑定连接的标记。
- 多连接时默认绑定当前活跃标签。
- 明确指定后台 `connection_id` 时，读取正确且 `is_active=false`。
- 多连接但活跃状态未知时返回 `AMBIGUOUS_CONNECTION`，不秘密回退主 Shell。
- 审批等待期间切换标签，严格模式不能绑错。
- attach 后切换标签，`require_active=true` 的 read 返回 `ACTIVE_CONNECTION_CHANGED`。
- 关闭复制渠道、主连接重连后，旧 token 不能读取新连接数据。
- 单连接旧客户端不传新参数仍能正常使用。
- 分屏切换、关闭当前窗格、只读 observer 分别绑定不同连接均正常。

### 明确不建议

不建议「自动跟随活跃标签」：它会令同一 token 的 replay 序列跨连接，反而制造更隐蔽的数据错配。**固定绑定并显式报告变化，才是安全修复。**
