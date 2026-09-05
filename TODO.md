# CodeDock TODO

> 进度基线：[《CodeDock 终版设计与路线图 v1.0》](./CodeDock_终版设计与路线图_v1.0.md)（下文 §N 均指该文档章节）。
> 实施顺序约束（§25）：Runtime → Tool/Policy → Context → Desktop → Plugin → Mobile → Browser Extension，顺序不得改变。
>
> 最后更新：2026-09-05

## 当前状态快照

- ✅ **阶段 0**：架构基线冻结（设计文档 v1.0，三大协议 + 两个补充协议）。
- ✅ **阶段 1 主体**：15 个 crate 骨架；protocol 覆盖 Event/Session/Tool/Context 核心类型；session-engine 会话闭环（创建/暂停/恢复/取消 + 幂等命令）；JSON-RPC + 本地 IPC；CLI；SQLite 持久化 Event Store 与跨重启恢复；CI（fmt + clippy -D warnings + 双平台测试 + release 冒烟）。
- ✅ **模型层**：`ModelProvider` trait（§11.1 统一能力）、OpenAI-Compatible Provider（SSE 流式、密钥仅经 SecretStore）、MockProvider（可脚本化回放 + 回声）、RoutingConfig、SessionBudgetLimits（§11.3 / §18.3）。
- 🚧 **阶段 1 收尾**：模型层已就绪但尚未接入 session-engine / daemon 的 Turn 循环。

## 阶段 1 收尾（2026-09-05 完成）

- [x] **装配模型层**：agent-daemon 装配 `Runtime`（会话管理 + TurnEngine）；TurnEngine 执行完整事件编排：`turn.started` → 用户 `message.created` → `context.snapshot.created`（§8.4 快照含 system prompt/任务/全部历史）→ `model.request.started` → `message.delta`（transient）→ `message.completed`（durable，幂等键锚点）→ `model.request.completed` → `turn.completed`。
- [x] **模型路由（默认 Provider）**：ProviderRegistry + config.toml（`[providers.*]`、`default_provider`、`[limits]`）；无配置文件回退内置 Mock，daemon 开箱即用；密钥经 `CODEDOCK_PROVIDER_<ID>_API_KEY` 注入 SecretStore（Keychain TODO）。
- [x] **预算防护（§18.3）**：SessionBudgetLimits 在每轮开始前从 Durable Event 统计已用轮次/Token，触达上限拒绝；并发同会话消息防交错；Provider 失败仅失败当前 Turn。
- [x] **CLI 端到端验证**：`codedock message` 子命令；集成测试覆盖真实 UDS 全链路 + 冒烟验证 创建→对话→暂停（拒发）→恢复→取消 完整生命周期。
- [x] **断线重连测试**：`session.subscribe` 实时推送仍为 TODO（ipc.rs），但重连补发路径已锁定：`session.events` + `after_sequence` 补发全部 Durable Event、sequence 严格单调、transient 不补发（§8.2.5）。
- [ ] **（阶段 1 遗留，非阻塞）**：`session.subscribe` 实时事件推送（服务器主动推送）；Named Pipe IPC 后补 `windows-latest` CI matrix（§18.8）；按任务类型路由（planning/coding/summarization，§11.3——当前只有默认 Provider）。

## 阶段 2：安全 Tool 闭环（下一个大块）

**六个核心工具**（tool-runtime 目前仅骨架）：

- [ ] `file.read`
- [ ] `file.patch`（Patch-first、源文件 Hash 校验、原子替换，§13.1）
- [ ] `search.text`
- [ ] `shell.execute`（超时 / 环境白名单 / 输出上限，§13.2）
- [ ] `git.status`、`git.diff`

**基础设施**：

- [ ] Tool Definition 注册与参数校验（`namespace.action` 命名，§8.3.2/8.3.3）
- [ ] Tool Preflight：ToolExecutionPlan + `operation_digest`（§8.3.5）
- [ ] Policy Engine 决策接入：allow / deny / require_approval（§8.3.7 默认风险策略；policy-engine 已有雏形）
- [ ] Approval 流程：审批锚定 digest、参数变化即失效、Agent 不得自批（§9.1）
- [ ] Checkpoint 创建与恢复（§3.5）
- [ ] Hash 冲突检测：外部修改 → `change.conflicted`，禁止覆盖（§18.2）
- [ ] Tool 取消与进程树管理：优雅终止 → Grace Period → 强杀（§8.3.10）
- [ ] 事件接入：`tool.call.*`、`change.*`、`checkpoint.*` 全链路落 Event Store

**退出标准**：CLI 完成"读代码 → 修改 → 运行测试 → 回滚"；所有副作用有权限、审批、审计记录。

## 后续阶段（按路线图，勿提前）

- [ ] **阶段 3** Context Engine 与透明追踪：Context Snapshot / Selection Report / Token Budget（§12）、ripgrep + Tree-sitter 索引、Secret 检测脱敏（§18.5）、Trust/Role 管线（§18.1）、Visual Replay。
- [ ] **阶段 4** Desktop DevTools Alpha：Tauri 2 + React，Console/Context/Tools/Network/Changes/Trace/Security 面板（§14），Monaco + xterm.js。
- [ ] **阶段 5** Plugin System v1：Wasmtime + WIT、`.cdplugin`、Rust/TS SDK、Native Sidecar Supervisor（§10）。
- [ ] **阶段 6** Mobile Remote：PWA、QR 配对、E2EE Relay、远程审批（§16）。
- [ ] **阶段 7** Browser Extension：MV3 + Native Messaging、用户触发的上下文采集（§15）。
- [ ] **阶段 8** Hardening 与公开 Beta：安装器/签名、自动更新、崩溃恢复、安全评审、Plugin Conformance Suite（§23 阶段 8）。

## 已知技术债（不阻塞，登记备查）

- [ ] `OpenAICompatibleProvider::cancel_request` 未实现请求粒度取消，随 Turn 取消一起做（§8.3.10）。
- [ ] Blob 内容条目不发送给 Provider（阶段 1 仅内联文本，见 `snapshot_to_messages`）。
- [ ] `count_tokens` 为估算值（chars/4），接入真实 tokenizer 待定（§18.6）。
- [ ] Tool Calling / StructuredOutput Capability 已声明但未实现（模型产出 Tool Proposal 是阶段 2 事项）。
- [ ] TurnEngine 暂停语义为"轮间暂停"：Turn 进行中的 Pause 不打断当轮，仅拒绝后续消息；Turn 中断/恢复需要取消令牌（§8.2.7 完整状态机在阶段 2 补齐）。

## MVP 验收（最终门槛，§24）

- [ ] §24 完整链路：建计划 → Context 可解释 → 工具调用 → 审批 → Patch → 冲突检测 → 测试 → Trace → 重启恢复 → 一键回滚。
- [ ] 四个负向测试：恶意 README 诱导读 `~/.ssh` 被阻止 / 插件越权被阻止 / 过期审批被拒绝 / 手工编辑冲突不被覆盖。
