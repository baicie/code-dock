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
- [x] **`session.subscribe` 实时事件推送（§8.2.5，2026-09-05 补齐）**：EventHub 广播总线 + `session.event` notification 推送（durable/transient 都实时），CLI `follow` 实时跟踪；落后以 `session.resync` 通知重新对齐。
- [x] **按任务类型路由（§11.3，2026-09-05 补齐）**：`[routes.*]` 配置 + TaskKind + `session.message` 的 `task_type` 参数；未配置/未注册回退默认 Provider。
- [ ] **（阶段 1 遗留，非阻塞）**：Named Pipe IPC 后补 `windows-latest` CI matrix（§18.8）。

## 阶段 2：安全 Tool 闭环（下一个大块）

**六个核心工具**（tool-runtime 目前仅骨架）：

- [ ] `file.read`
- [ ] `file.patch`（Patch-first、源文件 Hash 校验、原子替换，§13.1）
- [ ] `search.text`
- [ ] `shell.execute`（超时 / 环境白名单 / 输出上限，§13.2）
- [ ] `git.status`、`git.diff`

**基础设施**（2026-09-05 完成）：

- [x] Tool Definition 注册与参数校验（`namespace.action` 命名，§8.3.2/8.3.3；最小 JSON Schema 子集校验器）
- [x] Tool Preflight：ToolExecutionPlan + `operation_digest`（§8.3.5）
- [x] Policy Engine 决策接入：allow / deny / require_approval（§8.3.7 默认风险策略；副作用工具按 §18.1 提升审批等级）
- [x] Approval 流程：审批锚定 digest、一次性、5 分钟过期；`tool.approve` RPC 裁决；会话进入 waiting_approval；未决审批跨重启恢复（§9.1）
- [x] 事件接入：`tool.call.*`、`policy.decision_made` 全链路落 Event Store
- [x] **Agent 循环（§7）**：一次 Turn 内 模型 → Proposal → Preflight → Policy → 执行 → 结果回填上下文 → 继续调用，直至最终回复
- [x] `file.read`（工作区 confinement：`~` 拒绝 / `..` 逃逸 / 符号链接逃逸三层防护 + 执行前二次 resolve + 64KiB 上限；§24 负向测试 1 的 Preflight 拦截已锁定）

**待做**（2026-09-05 工具集与安全基础设施补全后剩余）：

- [x] `file.patch`：find/replace 补丁、expected_sha256 冲突检测、原子替换、实际副作用记录
- [x] `search.text`：纯 Rust 遍历（ripgrep/FTS5 阶段 3）
- [x] `shell.execute`：参数化执行、环境白名单、Unix 进程组 TERM→KILL、超时/取消、捕获上限
- [x] `git.status`、`git.diff`：固定 argv 只读执行
- [x] Checkpoint：fs.write 工具执行前自动快照（DiskCheckpointStore，SHA 寻址）；`checkpoint.restore` RPC + CLI `rollback`（§3.5 / §24）
- [x] Hash 冲突检测：Preflight 阶段即拒绝并发出 `change.conflicted`，冲突不打扰审批（§18.2）
- [ ] 端到端验收：CLI 完成"读代码 → 修改 → 运行测试 → 回滚"全链路演练（需真实 Provider 提出多步工具提案，当前 Mock 只回声）
- [ ] `git.status/diff` 工具执行前的工作区 git 仓库检测错误码对齐（§8.3.9 not_found）

**退出标准**：CLI 完成"读代码 → 修改 → 运行测试 → 回滚"；所有副作用有权限、审批、审计记录。链路各环节（工具、审批、Checkpoint、冲突检测）已实现并测试，待真实模型端到端演练。

## 后续阶段（按路线图，勿提前）

- [ ] **阶段 3** Context Engine 与透明追踪（2026-09-05 第一切口已落地，见下）：
  - [x] SnapshotBuilder 预算装箱 + Selection Report 落事件（§8.4.9 / §12.3，退出标准"用户能查看选中和未选中上下文的原因"达成）
  - [x] Secret 检测/脱敏统一管线：五类常见密钥形态，内容级 Redact 变换记录（§18.5）
  - [x] `audit_payload` + `final_request_sha256`（§8.4.1，退出标准"任意模型请求可还原本地发送的最终载荷"达成）
  - [x] Tree-sitter 符号索引（project-index：Rust/Python/TS/JS）+ `search.symbol` 工具（§12.1 #4）；文本搜索统一到 project-index
  - [x] 项目规则进上下文：CodeDock.md + .codedock/rules/*.md 作为 project_rule 候选（§12.1 #2）
  - [ ] ripgrep 二进制 / SQLite FTS5 替换纯 Rust 文本搜索（性能瓶颈出现时，§12.1 #3）
  - [ ] 文件监听增量更新（notify）与索引持久化（§12.2）
  - [ ] Git/Diagnostics 检索来源作为 Retriever（§12.1 #5/#6）
  - [x] Trust/Role 溯源（§18.1 + §8.3.7 完整语义）：副作用工具按上下文溯源——干净上下文按 §8.3.7 默认策略（Edit/Auto 自动放行 Medium），上下文含 Data 条目时提升一级审批；Ask/Plan 一律拒绝副作用操作
  - [ ] Visual Replay 事件回放视图（阶段 4 UI 的前置）
- [ ] **阶段 4** Desktop DevTools Alpha（2026-09-05 第一切口已落地）：
  - [x] Tauri 2 + React 骨架：`apps/desktop`（独立 Cargo workspace + pnpm），桥接层复用 CLI 同款 UDS/JSON-RPC 协议（§5：Desktop 不内嵌 Runtime）
  - [x] Console 面板最小版：会话创建（四模式）、流式对话（`message.delta` 实时渲染）、事件时间线（策略裁决/审批/冲突/Checkpoint 可解释事件）
  - [x] Context 面板：预算条 + 条目表（选择原因/信任/变换）+ 排除项 + 载荷哈希（§8.4.9 可解释性）
  - [x] Tools 面板：工具全生命周期链（提议→预检→策略→审批→执行）+ 权限/输入/输出/实际副作用（§8.3.4）
  - [x] Changes 面板：Patch 伪 diff（find/replace + 前后哈希）、Checkpoint 列表与**一键回滚按钮**（§24）、冲突记录（§18.2）
  - [ ] Network / Trace / Security / Memory / Plugins 面板（§14），Monaco diff + xterm.js
  - [ ] CI 增加 desktop job（ubuntu 需 webkit2gtk 系统依赖）
  - [ ] 图标/打包/签名（bundle.active 当前关闭）
- [ ] **阶段 5** Plugin System v1：Wasmtime + WIT、`.cdplugin`、Rust/TS SDK、Native Sidecar Supervisor（§10）。
- [ ] **阶段 6** Mobile Remote：PWA、QR 配对、E2EE Relay、远程审批（§16）。
- [ ] **阶段 7** Browser Extension：MV3 + Native Messaging、用户触发的上下文采集（§15）。
- [ ] **阶段 8** Hardening 与公开 Beta：安装器/签名、自动更新、崩溃恢复、安全评审、Plugin Conformance Suite（§23 阶段 8）。

## 已知技术债（不阻塞，登记备查）

- [ ] 有副作用工具在默认语境下一律升级审批（§18.1 保守解释）；§8.3.7 的"Medium 在 Edit/Auto 自动允许"待阶段 3 Trust/Role 溯源管线后按内容来源放宽。
- [ ] `search.text` 为朴素子串遍历；ripgrep + SQLite FTS5 + Tree-sitter 在阶段 3 替换（§12.1）。
- [ ] shell.execute 的 Windows Job Object 管理待 Windows 适配（§18.8）。
- [ ] 一次模型调用仅处理第一个 Tool Proposal，其余丢弃并告警（多提案并行是后续优化）。
- [ ] 工具结果以文本协议回填上下文；OpenAI 原生 tool-calling 消息格式（`tool` role / `tool_calls`）待 model-gateway 实现。
- [ ] `OpenAICompatibleProvider::cancel_request` 未实现请求粒度取消，随 Turn 取消一起做（§8.3.10）。
- [ ] Blob 内容条目不发送给 Provider（仅内联文本，见 `snapshot_to_messages`）；大工具输出落 Blob 待 artifact-store 接入。
- [ ] `count_tokens` 为估算值（chars/4），接入真实 tokenizer 待定（§18.6）。
- [ ] Tool Calling / StructuredOutput Capability 已声明但 Provider 侧未实现（当前由 Mock 脚本驱动提案）。
- [ ] TurnEngine 暂停语义为"轮间暂停"：Turn 进行中的 Pause 不打断当轮，仅拒绝后续消息；Turn 中断/恢复需要取消令牌（§8.2.7 完整状态机）。
- [x] ~~审批的实时推送通知~~：`session.subscribe` 已上线，客户端可实时收到 `tool.call.approval_required`。

## MVP 验收（最终门槛，§24）

- [ ] §24 完整链路：建计划 → Context 可解释 → 工具调用 → 审批 → Patch → 冲突检测 → 测试 → Trace → 重启恢复 → 一键回滚。
- [ ] 四个负向测试：恶意 README 诱导读 `~/.ssh` 被阻止 / 插件越权被阻止 / 过期审批被拒绝 / 手工编辑冲突不被覆盖。
