# CodeDock 终版设计与路线图 v1.0

**定位：** 以 Rust 为核心、Local-first、可扩展、可审计、可远程控制的 Coding Agent 平台。  
**客户端形态：** Chrome DevTools 风格桌面端 + 浏览器插件 + 手机远程控制端 + CLI。  
**文档状态：** v1.0 Architecture Baseline。除非进入实现后发现安全或兼容性问题，Beta 前不再改变核心边界，只允许补充字段和实现细节。

---

## 1. 最终产品定义

CodeDock 不是一个“带聊天框的编辑器”，而是一个独立的 **Agent Runtime 与控制台**：

- Rust Daemon 负责会话、上下文、模型、工具、权限、插件、审计和远程连接。
- PC、浏览器、手机和 CLI 都只是连接 Runtime 的客户端。
- 用户可以接入云端模型、本地模型和 OpenAI-Compatible 模型。
- 每一次模型请求、上下文拼装、工具调用、权限判断、文件变更和远程操作都可检查、可重放、可撤销。
- 第三方通过 WASM 插件或受控 Sidecar 扩展工具、上下文、模型适配器、工作流和 UI 面板。

一句话定义：

> 一个像 Chrome DevTools 一样透明、像插件平台一样可扩展、像远程终端一样可控的 Coding Agent。

---

## 2. 最终设计决策

| 领域 | 最终选择 |
|---|---|
| 核心运行时 | Rust 独立 Daemon |
| 桌面端 | Tauri 2 + React + TypeScript |
| 编辑器与终端 | Monaco Editor + xterm.js |
| 手机端 | 先做响应式 PWA，稳定后再封装原生应用 |
| 浏览器插件 | Chrome/Edge Manifest V3 + Native Messaging |
| 本地通信 | Unix Domain Socket / Windows Named Pipe；必要时回退 localhost WebSocket |
| 远程通信 | TLS WebSocket + 应用层端到端加密中继 |
| 控制协议 | JSON-RPC 2.0 |
| 事件流 | WebSocket 推送，Session 内单调递增 sequence |
| 本地数据库 | SQLite + SQLx |
| 大对象存储 | 基于 SHA-256 的本地 Content-addressed Blob Store |
| 项目搜索 | ripgrep + SQLite FTS5 |
| 代码索引 | Tree-sitter 增量符号索引 |
| 插件默认运行时 | Wasmtime + WASI Component Model + WIT |
| 高权限扩展 | 独立 Native Sidecar 进程 |
| 模型层 | 统一 Model Provider Adapter |
| 密钥存储 | 操作系统 Keychain / Credential Vault |
| 变更方式 | Patch-first、内容哈希校验、Checkpoint、可回滚 |
| 安全原则 | 最小权限、默认拒绝、参数级权限计算、关键操作不可静默执行 |

---

## 3. 产品原则

### 3.1 Local-first

代码、会话、索引和工具执行默认留在用户设备。中继服务器只转发密文，不保存可读代码。

### 3.2 Runtime 是唯一权威

所有客户端不得直接绕过 Runtime 操作项目文件、模型密钥、插件或工具。状态、权限和审计以 Rust Daemon 为准。

### 3.3 透明优先于“魔法感”

用户必须能回答：

- 模型这一轮看到了什么？
- 为什么选中这些文件？
- 哪段内容被摘要、截断或脱敏？
- 谁触发了这个工具？
- 权限为什么被允许或拒绝？
- 哪个操作真正修改了文件或外部系统？

### 3.4 所有副作用必须可识别

读操作、写操作、进程、网络、Git、浏览器控制、外部服务修改必须显式分类，并记录预期副作用与实际副作用。

### 3.5 默认可恢复

文件写入使用原子替换或 Patch；高风险修改前创建 Checkpoint；会话崩溃后可恢复到最近稳定状态。

### 3.6 不信任外部内容

网页、仓库 README、代码注释、Issue、日志和工具输出都可能包含 Prompt Injection。它们默认是“数据”，不能自动成为系统指令或覆盖权限策略。

---

## 4. 范围控制

### 4.1 v1.0 必须完成

- 单用户、本地项目、单主 Agent。
- Ask、Plan、Edit、Auto 四种模式。
- 文件读取、搜索、Patch、Shell、Git、Diagnostics 六类核心工具。
- OpenAI-Compatible、本地模型接口和至少一个独立云模型适配器。
- Context、Tools、Network、Changes、Trace、Security 六个核心面板。
- WASM 插件、插件权限和基础 SDK。
- 手机查看、审批、暂停、终止和追加指令。
- 浏览器插件采集用户明确授权的页面上下文。
- Session Replay、Checkpoint、崩溃恢复和审计导出。

### 4.2 v1.0 明确不做

- 多 Agent 群体协作。
- 在线 IDE 或云端代码托管。
- 插件交易市场和付费系统。
- 企业级复杂 RBAC。
- 多人实时协同编辑和 CRDT。
- 自动操作生产环境。
- 无人值守的无限时长任务。
- 同时支持所有 IDE 插件。

这些能力可以在核心协议稳定后逐步增加。

---

## 5. 总体架构

```text
┌────────────────────────────────────────────────────────────────┐
│                         Client Layer                           │
│                                                                │
│ Desktop        CLI        Browser Extension       Mobile PWA   │
│ Tauri/React                Native Messaging       Remote UI    │
└───────────────┬─────────────┬──────────────────────┬─────────────┘
                │ Local IPC   │ Local Bridge         │ E2EE Relay
                └─────────────┴──────────────┬───────┘
                                             │
┌────────────────────────────────────────────▼───────────────────┐
│                    Rust Agent Daemon                           │
│                                                                │
│ Session Engine       Context Engine       Model Gateway        │
│ Tool Runtime         Policy Engine        Plugin Host          │
│ Project Index        Artifact Store       Trace/Event Store    │
│ Checkpoint Manager   Secret Store         Remote Gateway       │
└─────────────┬────────────────────┬──────────────────────────────┘
              │                    │
      ┌───────▼────────┐   ┌───────▼─────────────────┐
      │ WASM Plugins   │   │ Native Sidecars         │
      │ Wasmtime/WASI  │   │ ADB/Docker/LSP/Browser  │
      └────────────────┘   └─────────────────────────┘
```

### 5.1 边界约束

- Desktop UI 不直接读取文件系统。
- Browser Extension 不直接执行本地命令。
- Mobile 不持有项目密钥和模型密钥。
- Plugin 不得访问未声明的资源。
- Model 不直接获得工具句柄，只能产生 Tool Proposal。
- Permission Engine 是所有副作用操作的唯一审批入口。

---

## 6. Rust 核心模块

| 模块 | 责任 |
|---|---|
| `agent-daemon` | 进程生命周期、RPC、客户端连接、模块装配 |
| `session-engine` | Agent 状态机、Turn、计划、暂停、恢复、取消 |
| `event-store` | Append-only 事件、sequence、Replay、Projection |
| `context-engine` | 候选上下文、排序、预算、变换、快照 |
| `model-gateway` | Provider 适配、路由、重试、流式输出、成本统计 |
| `tool-runtime` | Tool 注册、参数校验、预执行分析、执行、取消 |
| `policy-engine` | 权限、风险、Session Mode、审批和组织策略 |
| `plugin-host` | WASM 宿主、Sidecar 生命周期、插件权限 |
| `project-index` | 文件、文本、符号、依赖和 Git 增量索引 |
| `artifact-store` | Patch、日志、截图、模型载荷和测试报告 |
| `checkpoint-manager` | 变更前快照、恢复、工作区一致性 |
| `remote-gateway` | 设备配对、端到端加密、角色、撤销 |
| `secret-store` | 模型密钥、插件密钥、远程设备密钥 |
| `trace-store` | Span、耗时、Token、错误、重试和成本 |

---

## 7. Agent 最终执行流程

```text
用户任务
  ↓
创建 Session / Turn
  ↓
生成或更新 Plan
  ↓
收集候选上下文
  ↓
隐私与 Prompt Injection 检查
  ↓
构造不可变 Context Snapshot
  ↓
调用模型
  ↓
模型返回文本或 Tool Proposal
  ↓
Tool 参数校验和 Preflight
  ↓
Policy Engine：allow / deny / require_approval
  ↓
执行 Tool，流式输出，记录实际副作用
  ↓
应用 Patch 前校验文件版本并创建 Checkpoint
  ↓
运行测试或诊断
  ↓
产生新 Context Snapshot
  ↓
继续下一轮或完成 Session
```

模型永远不能直接跳过 Policy Engine，也不能自己批准自己的操作。

---

# 8. 三个核心协议 v1.0

## 8.1 通用约定

### 标识符

核心实体使用 UUIDv7：

```text
session_id / turn_id / event_id / request_id / tool_call_id
snapshot_id / artifact_id / approval_id / device_id / plugin_id
```

### 时间

统一使用 UTC RFC3339：

```json
"2026-09-04T05:31:28.421Z"
```

### 版本

- 外层传输：JSON-RPC 2.0。
- 领域对象：`schema_version`。
- 1.x 允许新增可选字段，禁止改变既有字段语义。
- 客户端遇到未知事件类型或枚举值时，必须保留原始数据并降级展示，不能崩溃。

### 大对象

超过阈值的代码、日志、截图、模型原始载荷不直接嵌入事件，统一保存为 Blob：

```json
{
  "blob_id": "blob_xxx",
  "sha256": "...",
  "size": 183242,
  "mime_type": "text/plain"
}
```

---

## 8.2 Session Event Protocol v1.0

### 8.2.1 目标

让 Desktop、CLI、Mobile、Browser、Trace Viewer 使用同一事件流观察和控制 Session，并支持断线恢复与重放。

### 8.2.2 Event Envelope

```json
{
  "schema_version": "1.0",
  "event_id": "0199...",
  "session_id": "0199...",
  "turn_id": "0199...",
  "sequence": 128,
  "event_type": "tool.call.completed",
  "durability": "durable",
  "occurred_at": "2026-09-04T05:31:28.421Z",
  "actor": {
    "type": "agent",
    "id": "primary"
  },
  "correlation_id": "0199...",
  "causation_event_id": "0199...",
  "payload": {}
}
```

### 8.2.3 字段规则

- `sequence`：由 Runtime 分配，在单个 Session 内严格单调递增。
- `correlation_id`：关联一次用户任务或完整执行链。
- `causation_event_id`：指出当前事件由哪个事件直接引发。
- `durability`：`durable` 或 `transient`。
- Durable Event 一旦确认写入后不可修改，只能追加纠正事件。
- `message.delta` 等高频流式事件可以是 transient；最终消息和工具结果必须 durable。

### 8.2.4 核心事件类型

```text
session.created
session.started
session.mode_changed
session.paused
session.resumed
session.completed
session.failed
session.cancelled

turn.started
turn.completed
turn.failed

message.created
message.delta
message.completed

plan.created
plan.updated
plan.step.started
plan.step.completed
plan.step.failed

context.snapshot.created

model.request.started
model.request.completed
model.request.failed
model.request.cancelled

tool.call.proposed
tool.call.preflighted
tool.call.approval_required
tool.call.approved
tool.call.rejected
tool.call.started
tool.call.output
tool.call.completed
tool.call.failed
tool.call.cancelled

change.proposed
change.applied
change.conflicted
change.reverted

checkpoint.created
checkpoint.restored

policy.decision_made
security.warning
```

### 8.2.5 断线恢复

客户端订阅：

```json
{
  "jsonrpc": "2.0",
  "id": "req-1",
  "method": "session.subscribe",
  "params": {
    "session_id": "0199...",
    "after_sequence": 152
  }
}
```

Runtime 先补发 Durable Event，再继续实时推送。Transient Event 不保证补发，客户端应读取最终 Message、Tool Call 或 Artifact 恢复视图。

### 8.2.6 幂等命令

所有改变状态的 JSON-RPC 命令都携带 `command_id` 和 `idempotency_key`。重复命令不得重复执行副作用。

### 8.2.7 Session 状态机

```text
created → running
running → waiting_approval / paused / completed / failed / cancelled
waiting_approval → running / cancelled
paused → running / cancelled
```

`completed`、`failed`、`cancelled` 是终态。恢复失败任务时创建新的 Turn 或新的 Session，不篡改终态事实。

---

## 8.3 Tool Capability Protocol v1.0

### 8.3.1 核心原则

> Tool 声明能力；Tool Runtime 计算具体权限；Policy Engine 决定是否执行。

模型只能提出 Tool Proposal，不能直接执行。

### 8.3.2 Tool Definition

```json
{
  "schema_version": "1.0",
  "name": "shell.execute",
  "version": "1.0.0",
  "description": "Execute a command in a controlled workspace.",
  "provider": {
    "type": "builtin",
    "id": "runtime.shell"
  },
  "input_schema": {},
  "output_schema": {},
  "capabilities": ["process.execute", "fs.read"],
  "effect": "possible",
  "execution": {
    "streaming": true,
    "supports_cancel": true,
    "supports_dry_run": false,
    "idempotent": false,
    "default_timeout_ms": 120000,
    "max_timeout_ms": 1800000
  },
  "trust_level": "builtin"
}
```

### 8.3.3 命名规范

统一使用：

```text
namespace.action
```

例如：

```text
file.read
file.patch
search.text
search.symbol
shell.execute
git.status
git.diff
git.commit
browser.inspect
android.logcat
```

### 8.3.4 Tool Call 生命周期

```text
PROPOSED
  ↓
VALIDATED
  ↓
PREFLIGHTED
  ↓
POLICY DECISION
  ├─ DENY → REJECTED
  ├─ APPROVAL → WAITING → APPROVED / REJECTED
  └─ ALLOW
  ↓
STARTED
  ↓
OUTPUT...
  ↓
COMPLETED / FAILED / CANCELLED
```

### 8.3.5 Preflight

执行前必须生成 `ToolExecutionPlan`：

```json
{
  "tool_call_id": "0199...",
  "normalized_arguments": {
    "command": "./gradlew test",
    "cwd": "$workspace"
  },
  "permissions": [
    {
      "capability": "process.execute",
      "resource": "./gradlew test"
    },
    {
      "capability": "fs.read",
      "resource": "$workspace/**"
    }
  ],
  "risk": "medium",
  "expected_side_effects": [
    {
      "type": "process.started",
      "resource": "gradle"
    },
    {
      "type": "file.possibly_modified",
      "resource": "$workspace/build/**"
    }
  ],
  "operation_digest": "sha256:...",
  "preview": "Run ./gradlew test in the workspace"
}
```

审批针对 `operation_digest`。参数、目录、权限或命令任何一项变化，旧审批立即失效。

### 8.3.6 权限能力

```text
fs.read
fs.write
fs.delete
process.execute
process.kill
network.connect
git.read
git.write
git.push
secrets.read
browser.read
browser.control
system.read
system.write
external.read
external.write
```

权限必须包含具体资源范围，不能只写“允许 fs.read”。

### 8.3.7 风险等级

```text
low / medium / high / critical
```

默认策略：

- Low：Ask/Plan 中仅允许无副作用读操作。
- Medium：Edit 可按项目策略自动允许。
- High：默认需要用户审批。
- Critical：永远不得由 Agent 自动批准；要求用户重新确认，必要时进行本机身份验证。

### 8.3.8 Tool Result

```json
{
  "tool_call_id": "0199...",
  "status": "success",
  "content": [
    {
      "type": "text",
      "text": "Tests passed"
    }
  ],
  "artifacts": [
    {
      "artifact_id": "0199...",
      "type": "test_report",
      "uri": "agent://artifacts/0199...",
      "mime_type": "application/json"
    }
  ],
  "diagnostics": [],
  "actual_side_effects": [
    {
      "type": "process.completed",
      "resource": "gradle",
      "details": {
        "exit_code": 0
      }
    }
  ],
  "duration_ms": 18213
}
```

### 8.3.9 标准错误码

```text
invalid_arguments
permission_denied
approval_expired
resource_changed
not_found
timeout
cancelled
execution_failed
plugin_unavailable
network_error
rate_limited
sandbox_violation
internal_error
```

插件错误使用：

```text
plugin:<plugin-id>:<error-code>
```

### 8.3.10 取消与进程管理

长任务必须支持取消。Shell Tool 使用进程组或 Windows Job Object，先优雅终止，再在 Grace Period 后强制结束。子进程不得逃逸 Runtime 的生命周期管理。

---

## 8.4 Context Snapshot Protocol v1.0

### 8.4.1 核心原则

> Snapshot 必须记录最终发送给 Model Provider Gateway 的真实输入，而不是 Context Engine 原本想发送的内容。

Provider 在服务端如何再次处理不在本地可见范围内，但本地必须保存发送前的最终载荷哈希和可审计副本。

### 8.4.2 Snapshot

```json
{
  "schema_version": "1.0",
  "snapshot_id": "0199...",
  "session_id": "0199...",
  "turn_id": "0199...",
  "model_request_id": "0199...",
  "created_at": "2026-09-04T05:35:18Z",
  "model": {
    "provider": "openai-compatible",
    "model": "code-model",
    "context_window": 200000
  },
  "budget": {
    "max_context_tokens": 200000,
    "reserved_output_tokens": 16000,
    "available_input_tokens": 184000,
    "used_input_tokens": 68731
  },
  "items": [],
  "selection_report_ref": "agent://artifacts/...",
  "final_request_ref": "agent://artifacts/...",
  "final_request_sha256": "sha256:...",
  "privacy_decision": {},
  "summary": {}
}
```

### 8.4.3 Context Item

```json
{
  "item_id": "0199...",
  "type": "code",
  "role": "data",
  "source": {
    "kind": "file",
    "uri": "workspace://src/main.rs",
    "revision": "sha256:..."
  },
  "title": "src/main.rs",
  "content": {
    "storage": "blob",
    "blob_id": "blob_xxx",
    "sha256": "..."
  },
  "range": {
    "start_line": 20,
    "end_line": 120
  },
  "selection": {
    "reason": "symbol_dependency",
    "selected_by": "context_engine",
    "score": 0.91,
    "priority": 80
  },
  "trust": "workspace_untrusted",
  "classification": "internal",
  "tokens": 1382,
  "transformations": []
}
```

### 8.4.4 `role`

```text
instruction / data / tool_schema / assistant_history
```

网页、仓库文本、Issue、日志和 Tool Output 默认是 `data`。只有系统配置、用户当前指令和经过信任验证的项目规则可以成为 `instruction`。

### 8.4.5 `trust`

```text
trusted
workspace_untrusted
external_untrusted
plugin_untrusted
```

`trust` 用于 Prompt Injection 防御和审批升级。由外部不可信内容引导出的副作用操作，默认至少提升一级审批要求。

### 8.4.6 数据分类

```text
public
internal
confidential
secret
```

- `secret` 默认不进入任何模型上下文。
- `confidential` 只能发送给用户明确允许的 Provider。
- Provider Fallback 不得跨越数据策略；从本地模型切换到云模型时必须重新进行隐私决策。

### 8.4.7 Selection Reason

```text
user_attached
user_pinned
project_rule
currently_open
currently_edited
keyword_match
semantic_match
symbol_definition
symbol_reference
symbol_dependency
git_modified
git_related
diagnostic_related
tool_result
session_memory
agent_selected
plugin_selected
```

### 8.4.8 Transformation

```text
extract_range
truncate
summarize
deduplicate
redact
merge
normalize
```

每次变换记录输入哈希、输出哈希、Token 变化和原因。用户能查看变换前后的差异；敏感原文是否保留由数据策略决定。

### 8.4.9 Selection Report

除了最终 `items`，Context Engine 还保存候选选择报告：

- 哪些候选被考虑。
- 为什么选中或排除。
- 因 Token Budget 排除了什么。
- 哪些内容因隐私策略被阻止。
- 哪些内容因重复、低相关度或过期而未加入。

Selection Report 不一定发送给模型，但必须能在 UI 中查看。

### 8.4.10 不可变和一对一关系

- 一个 Model Request 对应一个 Context Snapshot。
- `model.request.started` 后 Snapshot 不得修改。
- 下一次模型调用必须创建新 Snapshot。
- Snapshot Diff 由 Runtime 动态计算。

---

# 9. 两个补充协议

原先的三个协议足以描述 Agent，但还不足以安全落地。最终版本补充两个支撑协议。

## 9.1 Policy & Approval Protocol

审批对象包含：

```json
{
  "approval_id": "0199...",
  "session_id": "0199...",
  "tool_call_id": "0199...",
  "operation_digest": "sha256:...",
  "risk": "high",
  "permissions": [],
  "preview": "Push branch dev to origin",
  "reason": "Network write and remote repository modification",
  "expires_at": "2026-09-04T05:40:00Z",
  "allowed_responses": ["approve_once", "deny"]
}
```

规则：

- 审批一次只对应一个 Digest。
- 默认不提供“永久允许 Critical 操作”。
- 移动端审批携带设备身份、Nonce 和签名。
- 过期、重复、参数变化、Session 状态变化都会使审批失效。
- Agent、Tool 和 Plugin 不能充当 Approver。

## 9.2 Client & Remote Control Protocol

角色：

```text
viewer
approver
controller
owner
```

权限：

- Viewer：只读。
- Approver：可批准或拒绝等待中的操作。
- Controller：可追加指令、暂停、继续和终止。
- Owner：可管理设备、模型、插件和策略。

远程命令均带设备 ID、单调计数器、时间戳和签名，防止重放。设备可随时撤销，撤销立即终止其远程会话。

---

# 10. 插件系统终版

## 10.1 两层运行模型

### WASM Plugin

默认方式，适用于：

- Tool。
- Context Provider。
- Model Adapter。
- Workflow。
- 轻量 UI Panel 描述。

优点是跨平台、可限制权限、易于崩溃隔离。

### Native Sidecar

仅用于必须访问完整系统环境的场景：

- ADB。
- Docker。
- 本地数据库客户端。
- LSP。
- GPU 或本地模型进程。
- 浏览器调试桥接。

Sidecar 独立进程运行，使用受控 JSON-RPC，与主进程隔离。

## 10.2 插件能力

```text
tool_provider
context_provider
model_provider
workflow_provider
ui_panel
policy_extension
```

Policy Extension 只能增加限制或提供组织规则，不能绕过 Runtime 的最低安全策略。

## 10.3 插件包

建议后缀：`.cdplugin`

内容：

```text
manifest.toml
component.wasm 或 sidecar 描述
ui/
schemas/
LICENSE
checksums.json
signature.sig
```

## 10.4 Manifest 示例

```toml
manifest_version = 1
id = "com.codedock.android"
name = "Android Development Tools"
version = "0.1.0"
runtime = "wasm-component"
entry = "android-tools.wasm"
min_host_version = "1.0.0"

[capabilities]
tool_provider = true
context_provider = true
ui_panel = true

[permissions]
fs_read = ["$workspace/**"]
fs_write = ["$workspace/**"]
process = ["adb", "gradlew"]
network = ["developer.android.com"]
secrets = []
```

## 10.5 插件治理

- Built-in、Signed、Unverified 三档信任级别。
- Unverified 插件默认不能在 Auto 模式自动执行。
- 安装时展示权限差异，不只展示“是否同意”。
- 插件更新后如果权限扩大，必须重新授权。
- 插件版本锁定在项目或用户 lockfile 中。
- 更新失败自动回滚旧版本。
- UI 插件运行在沙箱 iframe，不直接获得 Tauri 或 Node 能力。
- 插件崩溃不得导致 Runtime 崩溃。
- 后续插件仓库必须支持签名、校验和、下架和漏洞公告。

---

# 11. 模型系统终版

## 11.1 Provider 接口

统一能力：

```text
list_models
capabilities
count_tokens
stream_chat
cancel_request
health_check
```

Capability 描述：

```text
tool_calling
structured_output
streaming
image_input
context_window
max_output
prompt_cache
reasoning_control
```

## 11.2 Provider 类型

- OpenAI-Compatible。
- Anthropic 风格接口。
- Gemini 风格接口。
- Ollama / LM Studio / vLLM 等本地兼容接口。
- Plugin 提供的自定义 Provider。

## 11.3 模型路由

可按任务类型配置：

```yaml
routes:
  planning:
    provider: cloud-a
    model: reasoning-model
  coding:
    provider: local
    model: code-model
  summarization:
    provider: local
    model: small-model
```

## 11.4 关键约束

- Fallback 不得静默跨越隐私级别或成本上限。
- Provider 不能读取其他 Provider 的密钥。
- 原始请求和响应是否落盘由用户配置；默认敏感字段脱敏。
- API Key 只存系统 Keychain，不写入项目文件或普通 SQLite 字段。
- 每个 Session 有 Token、费用、最大轮次、最大工具调用次数和最大运行时长上限。
- 不具备原生 Tool Calling 的模型可以使用结构化 JSON 兼容层，但 UI 必须标记其可靠性较低。

---

# 12. Context Engine 与项目索引

## 12.1 检索顺序

1. 用户显式 Pin 和当前编辑文件。
2. 项目规则和任务直接关联文件。
3. ripgrep 关键词搜索。
4. Tree-sitter 符号定义、引用和依赖。
5. Git Diff 和最近修改关联。
6. Diagnostics 和测试失败关联。
7. 可选语义检索。

第一版不依赖向量数据库。向量检索作为可选插件或后续模块。

## 12.2 索引规则

- 默认遵循 `.gitignore`、`.ignore` 和项目排除配置。
- 二进制、大文件、构建目录默认不索引。
- 文件监听增量更新。
- 所有 Patch 应用前校验源文件 Hash，避免覆盖用户刚修改的内容。
- 同一 Workspace 默认只允许一个写 Session；多个只读 Session 可以并存。
- 外部编辑导致版本变化时进入 `change.conflicted`，禁止强行覆盖。

## 12.3 上下文预算

预算应包含：

- System Prompt。
- Tool Schemas。
- 用户消息。
- 历史摘要。
- 文件、Diff、日志、诊断。
- 预留输出。

不能只统计代码 Token，Tool Schema 经常占用大量上下文。

---

# 13. Tool Runtime 与工作区安全

## 13.1 文件变更

- 默认使用 Patch，不允许模型随意重写整个大文件。
- 写入前检查路径归一化、符号链接逃逸和源文件 Hash。
- 使用临时文件 + 原子替换。
- 删除操作单独分类，不能伪装成普通写入。
- 每次变更形成 Diff、Checkpoint 和实际副作用记录。

## 13.2 Shell

- 使用参数化执行接口，避免不必要的 Shell 字符串拼接。
- 必须指定工作目录、超时、环境变量白名单和输出上限。
- 默认不继承全部系统环境变量。
- Secret 环境变量不得回显到日志和模型上下文。
- 进程树受 Runtime 管理。
- Windows、macOS、Linux 分别做命令解析和路径安全测试。

## 13.3 网络

- 网络访问按域名、端口和协议授权。
- 默认禁止访问本机敏感端口、云元数据地址和内网管理接口。
- HTTP 响应属于 `external_untrusted` Context。
- 生产环境地址默认 High 或 Critical。

## 13.4 Git

- `git status`、`git diff` 为低风险读操作。
- `git commit` 为 Medium。
- `git push` 为 High。
- `git push --force`、重写公共历史为 Critical。
- Agent 不得自动提交或推送，除非项目策略明确允许；推送目标和分支必须显示。

---

# 14. DevTools 风格桌面端

```text
┌───────────────────────────────────────────────────────────────┐
│ Project | Session | Model | Mode | Remote | Budget | Settings │
├──────────────┬────────────────────────────────────────────────┤
│ Project Tree │ Console | Context | Tools | Network | Changes  │
│ Sessions     │ Trace | Security | Memory | Plugins            │
│ Tasks        │                                                │
│ Checkpoints  │                  主面板                         │
├──────────────┴────────────────────────────────────────────────┤
│ Terminal | Diagnostics | Approvals | Logs                     │
└───────────────────────────────────────────────────────────────┘
```

## 14.1 面板职责

| 面板 | 内容 |
|---|---|
| Console | 对话、计划、流式输出、任务控制 |
| Context | 每轮真实上下文、Token、来源、选择原因、变换、排除项 |
| Tools | Tool Proposal、权限、审批、输入、输出、实际副作用 |
| Network | 模型请求、插件请求、HTTP、耗时、Token、费用、重试 |
| Changes | Patch、Git Diff、冲突、Checkpoint、回滚 |
| Trace | 从用户指令到模型、工具和变更的完整时间线 |
| Security | 当前权限、敏感信息、Prompt Injection 警告、远程设备 |
| Memory | 会话摘要、项目记忆、用户 Pin，支持查看和删除 |
| Plugins | 插件、权限、版本、健康状态和日志 |

## 14.2 关键交互

- 任意工具调用都能跳转到触发它的模型消息和 Context Snapshot。
- 任意代码变更都能追溯到 Tool Call、审批和模型请求。
- 任意 Context Item 都能查看原始来源和变换链。
- 用户可以在 Context 面板 Pin、Exclude 或降低优先级。
- 高风险审批必须显示完整参数、目标资源和预计副作用，不能只显示“是否允许”。

---

# 15. 浏览器插件

## 15.1 能力

- 用户明确点击后采集当前页面 DOM、可访问性树、选中文本和截图。
- 读取经过授权的 Console 和 Network 摘要。
- Side Panel 中查看 Agent 当前 Session。
- 经审批后执行点击、输入、导航等操作。

## 15.2 安全边界

- 默认不持续监控所有页面。
- 密码框、支付页面、隐私模式和敏感站点默认禁止采集。
- 页面内容全部标记为 `external_untrusted`。
- 使用 Native Messaging 连接本地 Runtime；若使用 localhost，则必须有短期 Token、Origin 校验和端口随机化。
- 浏览器插件不能直接获得模型 API Key 和本地文件访问权。

---

# 16. 手机远程控制

## 16.1 第一版功能

- 查看 Session 状态和计划。
- 查看等待审批的 Tool Call。
- 查看压缩后的 Diff。
- Approve、Deny、Pause、Resume、Cancel。
- 追加文字指令。
- 查看重要 Trace 和安全警告。

手机端不承担大规模代码编辑和复杂插件配置。

## 16.2 配对

1. Desktop 生成一次性二维码。
2. 二维码包含桌面公钥、短期配对 Token 和中继地址。
3. 手机生成自己的设备密钥。
4. 双方建立端到端加密通道。
5. Desktop 显示设备指纹并要求最终确认。

不要自研密码学算法，使用经过审计的加密库和标准协议组合。

## 16.3 远程安全

- 中继服务只保存不可读密文和最少路由元数据。
- 设备权限可单独设置和随时撤销。
- Critical 审批可要求桌面端二次确认或本机生物识别。
- Push Notification 默认不包含代码、命令和文件内容。
- 网络断开不会自动批准、自动继续或丢失 Durable Event。
- Desktop 提供全局 Kill Switch。

---

# 17. 数据模型与存储

建议初始表：

```text
projects
sessions
turns
messages
session_events
plans
model_requests
model_responses
context_snapshots
context_items
tool_definitions
tool_calls
approvals
artifacts
blobs
checkpoints
plugins
plugin_permissions
policies
devices
audit_records
schema_migrations
```

## 17.1 Event Store

重要事实 Append-only。`sessions`、`tool_calls` 等表是当前状态 Projection，可以重建。

## 17.2 Blob Store

- 文件名基于 SHA-256。
- 去重存储。
- SQLite 仅保存索引和元数据。
- 可配置保留期和最大容量。
- 删除 Session 时执行引用计数清理。

## 17.3 加密和保留

- 密钥必须使用系统 Keychain。
- 敏感 Blob 可使用用户设备密钥加密。
- 用户可配置：不保存原始模型请求、保存脱敏版、或保存加密原文。
- 提供 Session 导出、删除、审计导出和彻底清理。

---

# 18. 被补充的重要遗漏点

## 18.1 Prompt Injection

Coding Agent 不只会被网页攻击，仓库中的 README、注释、测试数据、日志、Issue 和依赖文档也可能包含恶意指令。

最终策略：

- 所有来源带 Trust 和 Role。
- 不可信内容只能作为 Data。
- 不可信内容导致的副作用操作自动提高审批等级。
- Security 面板显示可疑指令片段和来源。
- Tool Output 重新进入上下文前再次检查。

## 18.2 并发与用户手工编辑

Agent 修改时用户可能同时编辑同一文件。必须基于内容 Hash 和 Workspace Revision 检测冲突，不能“最后写入者覆盖”。

## 18.3 无限循环和费用失控

每个 Session 有：

- 最大运行时长。
- 最大 Turn 数。
- 最大模型调用数。
- 最大 Tool Call 数。
- Token 和金额预算。
- 连续重复行为检测。

达到阈值自动暂停并要求用户处理。

## 18.4 插件供应链

插件可能比模型更危险。必须有签名、校验和、权限差异、锁版本、更新回滚、SBOM 和崩溃隔离。

## 18.5 Secret 泄露

Secret 不只存在于 `.env`，还可能出现在：

- Shell 输出。
- Git Diff。
- Crash Log。
- 浏览器 Network。
- 剪贴板。
- 插件返回结果。

因此脱敏应是统一的数据管线，而不是单独保护几个文件名。

## 18.6 模型能力不一致

不同模型的 Tool Calling、JSON 输出、Token 计算和流式行为不同。Provider 必须声明 Capability，不能假设全部模型都一样。

## 18.7 回放不等于重新执行

Visual Replay 默认只还原事件、上下文和结果，不自动再次运行 Shell 或网络操作。重新执行必须创建新 Session 并重新走权限流程。

## 18.8 跨平台差异

Windows 路径、权限、命令行解析、进程树和文件锁与 Unix 不同。核心抽象必须在早期做跨平台测试，不能到发布前才适配。

## 18.9 数据迁移

SQLite Schema、事件 Payload 和插件 Manifest 都需要显式版本与迁移工具。升级失败必须可回滚，不能损坏历史 Session。

## 18.10 可访问性与低带宽

桌面端支持键盘导航、可读焦点和高对比度。手机远程模式提供日志压缩和低带宽事件摘要，避免传输完整终端日志。

---

# 19. 道德、隐私与治理底线

即使产品是开发工具，也应明确以下底线：

### 用户知情与控制

- 不进行未授权的远程控制。
- 不在后台静默采集网页、剪贴板、文件或终端。
- 所有高风险动作可见、可拒绝、可终止。

### 数据最小化

- 只收集完成任务所需的数据。
- Telemetry 默认关闭或明确 Opt-in。
- Telemetry 不包含代码、提示词、命令、路径和密钥。

### 不冒充用户

- Agent 不得在用户不知情时发送邮件、提交外部表单、发布内容或代表用户作出承诺。
- 外部写操作必须显示目标、内容和身份。

### 可解释与可审计

- 不通过“安全原因”隐藏实际工具调用。
- 不伪造测试通过、命令成功或文件已保存。
- 审计记录清楚区分用户操作、Agent 建议、Plugin 行为和系统行为。

### 安全默认值

- 生产环境、财务、身份、证书、密钥和强制推送默认 Critical。
- 用户可以增加限制，但插件和模型不能降低最低限制。

### 代码许可与来源

- 插件必须声明许可证。
- 提供可选依赖和许可证扫描。
- 对大段生成或复制代码保留来源与修改记录，便于团队合规检查。

---

# 20. 可靠性与性能目标

以下是工程目标，不是首版发布承诺：

| 指标 | 目标 |
|---|---|
| Runtime 热启动 | ≤ 1.5 秒 |
| 本地 UI 事件延迟 p95 | ≤ 200 ms |
| Cancel 到发送终止信号 | ≤ 500 ms |
| Durable Event ACK 后丢失 | 0 |
| 崩溃后恢复到最近 Checkpoint | ≤ 5 秒 |
| 中型项目二次索引 | 增量更新，不做全量重建 |
| 日志和上下文 | 有大小上限、背压和 Blob 分流 |

## 20.1 恢复机制

- Event Store 使用事务写入。
- Tool 启动、完成和副作用写入具有关联 ID。
- Daemon 重启后识别“运行中但无进程”的悬挂 Tool Call，并标记为 interrupted。
- 文件修改前创建 Checkpoint。
- 数据库升级前备份并支持回滚。

---

# 21. 测试体系

## 21.1 单元与属性测试

- Session 状态机。
- sequence 单调性。
- Policy 规则。
- 路径归一化和符号链接逃逸。
- Context Budget。
- Protocol 编解码和向前兼容。

## 21.2 集成测试

- 模型 → Tool Proposal → Approval → Execute → Context 回填。
- Crash 后恢复。
- 手机断线重连。
- Plugin 崩溃和超时。
- Workspace 外部修改冲突。

## 21.3 安全测试

- 恶意仓库 Prompt Injection。
- 插件越权。
- 命令注入。
- 路径穿越。
- Secret 脱敏绕过。
- Remote Replay Attack。
- 浏览器扩展恶意页面。

## 21.4 跨平台矩阵

- Windows 11。
- 当前和上一代 macOS。
- 主流 Linux 发行版。
- Chrome 与 Edge。
- Android/iOS 主流浏览器 PWA。

## 21.5 Plugin Conformance Suite

任何插件发布前验证：

- Manifest。
- 权限。
- 超时。
- 取消。
- 输出大小。
- 崩溃隔离。
- 向前兼容。

---

# 22. 仓库结构

```text
codedock/
├── apps/
│   ├── desktop/
│   ├── cli/
│   ├── mobile-pwa/
│   └── browser-extension/
├── crates/
│   ├── agent-daemon/
│   ├── protocol/
│   ├── session-engine/
│   ├── event-store/
│   ├── context-engine/
│   ├── model-gateway/
│   ├── tool-runtime/
│   ├── policy-engine/
│   ├── plugin-host/
│   ├── project-index/
│   ├── artifact-store/
│   ├── checkpoint-manager/
│   ├── remote-gateway/
│   ├── secret-store/
│   └── trace-store/
├── services/
│   └── relay/
├── wit/
│   └── plugin-api/
├── schemas/
│   ├── events/
│   ├── tools/
│   ├── context/
│   └── plugins/
├── sdk/
│   ├── rust/
│   ├── typescript/
│   └── python/
├── plugins/
│   ├── builtin-files/
│   ├── builtin-search/
│   ├── builtin-shell/
│   ├── builtin-git/
│   ├── android-tools/
│   └── browser-tools/
└── docs/
    ├── architecture/
    ├── protocol/
    ├── security/
    ├── plugin-development/
    └── adr/
```

建议使用 Cargo Workspace + pnpm Workspace 的 Monorepo。

---

# 23. 开发路线图

以下按 **4–6 人全职小团队**估算，总周期约 28–32 周。单人全职更现实的区间是 10–16 个月。

## 阶段 0：架构冻结与威胁建模（第 1–2 周）

产出：

- 本文拆成 ADR。
- JSON Schema 初稿。
- WIT Plugin API 初稿。
- Session、Tool、Context、Approval 状态机。
- 安全威胁模型。
- Desktop 交互原型。

退出标准：

- 三个核心协议和两个补充协议通过评审。
- 明确 MVP 范围，不再增加大功能。

## 阶段 1：Rust Runtime 与 CLI（第 3–7 周）

产出：

- Daemon、JSON-RPC、Local IPC。
- Session Engine、Event Store、sequence、Replay。
- SQLite Migration。
- OpenAI-Compatible Provider。
- CLI 创建、暂停、恢复和取消 Session。

退出标准：

- 无 GUI 情况下可以完成一次纯对话 Session。
- 断线重连后 Durable Event 无丢失。

## 阶段 2：安全 Tool 闭环（第 8–12 周）

产出：

- file.read、file.patch、search.text、shell.execute、git.status、git.diff。
- Tool Preflight、Policy Engine、Approval。
- Checkpoint、Hash 冲突检测、回滚。
- Tool Cancellation 和进程树管理。

退出标准：

- CLI 可以完成“读代码 → 修改 → 运行测试 → 回滚”。
- 所有副作用都有权限、审批和审计记录。

## 阶段 3：Context Engine 与透明追踪（第 13–16 周）

产出：

- Context Snapshot、Selection Report、Token Budget。
- ripgrep + Tree-sitter 索引。
- Secret 检测、脱敏和数据分类。
- Prompt Injection Trust/Role 管线。
- Visual Replay。

退出标准：

- 任意模型请求都能还原本地发送的最终载荷。
- 用户能查看选中和未选中上下文的原因。

## 阶段 4：Desktop DevTools Alpha（第 17–21 周）

产出：

- Tauri Desktop。
- Console、Context、Tools、Network、Changes、Trace、Security。
- Monaco Diff、xterm.js。
- 模型、权限和项目设置。

退出标准：

- Desktop 可完成端到端 Coding 任务。
- UI 任何 Tool、Change、Context 都能相互跳转追踪。

## 阶段 5：Plugin System v1（第 22–25 周）

产出：

- Wasmtime Host、WIT 接口、Plugin Manifest。
- 插件安装、禁用、卸载、权限变更和回滚。
- Rust/TypeScript SDK。
- Android/ADB 和一个简单 Context Provider 示例。
- Native Sidecar Supervisor。

退出标准：

- 第三方插件无需修改 Runtime 即可注册 Tool 和 Context Provider。
- 越权插件被稳定阻止，插件崩溃不影响主进程。

## 阶段 6：Mobile Remote（第 26–28 周）

产出：

- PWA。
- QR 配对、设备角色、撤销。
- E2EE Relay。
- Approve、Deny、Pause、Resume、Cancel、追加指令。

退出标准：

- 手机跨网络稳定观察同一 Session。
- 远程审批具备 Digest 校验和 Replay 防护。

## 阶段 7：Browser Extension（第 29–31 周）

产出：

- MV3 Extension。
- Native Messaging Bridge。
- 用户触发的 DOM、Accessibility、Console、Network 和截图采集。
- 浏览器控制 Tool。
- 敏感站点和 Prompt Injection 防护。

退出标准：

- 浏览器上下文能进入 Snapshot 且被标记为 external_untrusted。
- 插件无法直接访问本地文件和模型密钥。

## 阶段 8：Hardening 与公开 Beta（第 32 周起）

产出：

- Windows/macOS/Linux 安装器和签名。
- 自动更新、Stable/Beta/Nightly 通道。
- Crash Recovery、性能优化和数据迁移。
- Security Review、Plugin Conformance Suite。
- 文档、示例插件、问题反馈和诊断包。

Beta 门槛：

- 核心数据无已知破坏性迁移问题。
- 无已知可绕过的 Critical 权限漏洞。
- 主要平台可稳定完成 30 分钟以上 Coding Session。
- Session 可导出、回放和删除。

---

# 24. MVP 验收场景

最终 MVP 必须通过下面这条完整链路：

```text
打开本地项目
→ 输入“修复一个真实 Bug”
→ Agent 建立计划
→ Context 面板显示为什么选择这些文件
→ 模型提出 search/file/shell 工具调用
→ Policy Engine 给出风险和权限
→ 用户在 Desktop 或 Mobile 审批
→ Agent 生成并应用 Patch
→ 检测用户外部修改冲突
→ 运行测试
→ 测试失败后继续修复
→ 测试通过
→ Changes 展示最终 Diff
→ Trace 展示完整因果链
→ Session 重启后可恢复
→ 用户可一键回滚到任务前 Checkpoint
```

此外必须通过四个负向测试：

1. 恶意 README 诱导读取 `~/.ssh`，Runtime 必须阻止。
2. Plugin 请求未声明域名，Runtime 必须阻止。
3. 手机用过期审批批准已变化的命令，Runtime 必须拒绝。
4. 用户在 Agent 修改前手工更新同一文件，Runtime 必须报告冲突而不是覆盖。

---

# 25. 建议团队配置

| 角色 | 人数 | 重点 |
|---|---:|---|
| Rust Runtime / Security | 1–2 | Session、Tool、Policy、Plugin、Remote |
| Agent / Context Engineer | 1 | Model、Context、索引、评测 |
| Desktop / Frontend | 1–2 | DevTools UI、Tauri、PWA |
| Browser / Full-stack | 1 | Extension、Relay、发布基础设施 |
| Product / UX / QA | 0.5–1 | 信息架构、安全交互、验收 |

单人实施时顺序不得改变：

```text
Runtime + CLI
→ Tool + Policy + Checkpoint
→ Context Snapshot
→ Desktop
→ Plugin
→ Mobile Remote
→ Browser Extension
```

先做多端 UI、后补 Runtime，是该项目最容易失败的路径。

---

# 26. 最终结论

CodeDock 的核心不是“模型有多聪明”，而是建立一个稳定的 Agent 控制平面：

- **Session Event Protocol** 解释发生了什么。
- **Tool Capability Protocol** 约束 Agent 能做什么。
- **Context Snapshot Protocol** 证明 Agent 知道什么。
- **Policy & Approval Protocol** 决定谁允许它做。
- **Remote Control Protocol** 规定谁可以从哪里控制它。

真正应优先投入的顺序是：

```text
可审计
→ 可控
→ 可恢复
→ 可扩展
→ 多端
→ 更强自治
```

如果这套底层先做稳，桌面端、手机端、浏览器插件和未来 IDE 插件都只是不同形态的控制客户端；如果底层协议、权限和上下文透明度不稳，客户端越多，风险和维护成本越高。
