//! Turn 执行引擎：Agent 循环（§7 执行流程 / §8.2.4 事件协议 / §8.3 Tool 生命周期）。
//!
//! 一条用户消息 = 一个 Turn；Turn 内是标准的 Agent 循环——每轮模型调用后：
//!
//! ```text
//! turn.started（durable）
//!   → message.created（用户消息，durable）
//!   ┌─> context.snapshot.created（§8.4.10：一次模型调用对应一个 Snapshot）
//!   │   → model.request.started（durable）
//!   │   → message.delta ...（transient）
//!   │   → message.completed（助手文本，durable）
//!   │   → model.request.completed（durable，含 Token 用量）
//!   │   [模型提出 Tool Proposal 时（§8.3.1：模型只能提议，不能执行）]
//!   │   → tool.call.proposed → tool.call.preflighted（§8.3.5 ToolExecutionPlan）
//!   │   → policy.decision_made（§8.3.7）
//!   │       ├─ allow    → tool.call.started → tool.call.output* → completed/failed
//!   │       ├─ deny     → tool.call.rejected
//!   │       └─ approval → tool.call.approval_required → session.waiting_approval
//!   │                     （tool.approve RPC 裁决后继续本 Turn）
//!   └─(工具结果回填上下文，进入下一次模型调用)
//!   → message.completed（最终回复，durable，幂等键锚点）
//!   → turn.completed（durable）
//! ```
//!
//! 预算防护（§18.3）：每次模型调用/工具调用前从 Durable Event 统计已用量；
//! 审批锚定 operation_digest（§9.1），一次性、带过期时间，崩溃后可从事件流恢复。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use codedock_checkpoint_manager::CheckpointStore;
use codedock_event_store::EventStore;
use codedock_model_gateway::{
    ChatDelta, ModelProvider, ProviderRegistry, RoutingConfig, SessionBudgetLimits, TaskKind,
};
use codedock_policy_engine::{PolicyContext, PolicyDecision, PolicyEngine};
use codedock_protocol::{
    Actor, Capability, Classification, ContextBudget, ContextItem, ContextItemContent,
    ContextSnapshot, Durability, Effect, EventEnvelope, ModelRef, Role, Selection, SelectionReason,
    SessionId, SessionMode, SessionStatus, SourceKind, SourceRef, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResultContent, Trust, TurnId,
};
use codedock_tool_runtime::{ToolOutputChunk, ToolRegistry, ToolRuntimeError};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Digest as _;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{SessionError, SessionManager};

/// 预算统计扫描的事件上限（阶段 2 事件量远低于此）。
const EVENT_SCAN_LIMIT: usize = 100_000;

const SYSTEM_PROMPT: &str = "你是 CodeDock，一个 Local-first 的 Coding Agent Runtime。\
你可以通过提出工具调用来读取工作区信息；工具调用只能由 Runtime 审批后执行。\
回答简洁、准确；不要编造工具或文件操作能力。";

/// 审批默认有效期（§9.1 `expires_at`）。
const APPROVAL_TTL: ChronoDuration = ChronoDuration::minutes(5);

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("session {0} 不存在")]
    NotFound(SessionId),
    #[error("会话状态为 {0:?}，无法发送消息（需要 running）")]
    InvalidState(SessionStatus),
    #[error("会话 {0} 上一轮仍在进行中，请等待完成或取消")]
    TurnInProgress(SessionId),
    #[error("超出 Session 预算上限（§18.3）: {0}")]
    BudgetExceeded(String),
    #[error("审批不存在或已失效: {0}")]
    ApprovalNotFound(String),
    #[error("未配置可用的模型 Provider")]
    NoProvider,
    #[error("模型调用失败: {0}")]
    Model(String),
    #[error("工具执行失败: {0}")]
    Tool(String),
    #[error("checkpoint 操作失败: {0}")]
    Checkpoint(String),
    #[error("上下文组装失败: {0}")]
    Context(String),
    #[error("事件存储失败: {0}")]
    EventStore(String),
}

impl From<SessionError> for TurnError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::NotFound(id) => TurnError::NotFound(id),
            other => TurnError::EventStore(other.to_string()),
        }
    }
}

/// Turn 状态：正常完成，或因等待审批挂起。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    WaitingApproval,
}

/// 一个 Turn 的执行结果（客户端可见投影）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub status: TurnStatus,
    /// 等待审批时的工具调用 id（客户端凭此调用 tool.approve）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_tool_call_id: Option<String>,
    /// 最终助手文本（挂起时为当前已累计文本）。
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latest_sequence: u64,
}

/// 等待审批的工具调用（内存态 + 事件流双写，崩溃后可恢复）。
#[derive(Debug, Clone)]
struct PendingApproval {
    session_id: SessionId,
    turn_id: TurnId,
    tool_call_id: ToolCallId,
    tool: String,
    plan: ToolExecutionPlan,
    original_key: Option<String>,
    expires_at: DateTime<Utc>,
    /// 审批后继续 Turn 时的任务路由（与原始消息一致）。
    task_kind: TaskKind,
}

/// 模型一轮输出中提取的工具提案。
struct Proposal {
    name: String,
    arguments: Value,
}

/// Turn 执行引擎：会话状态检查、预算、Snapshot 组装、Agent 循环与工具审批。
pub struct TurnEngine {
    store: Arc<dyn EventStore>,
    sessions: Arc<dyn SessionManager>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    policy: Arc<dyn PolicyEngine>,
    /// 按任务类型的模型路由（§11.3）。
    routes: RoutingConfig,
    /// 写操作前的文件快照（§3.5 / §20.1）。
    checkpoints: Arc<dyn CheckpointStore>,
    /// 工作区根（Checkpoint 快照的读取边界）。
    workspace: PathBuf,
    limits: SessionBudgetLimits,
    approval_ttl: ChronoDuration,
    /// 幂等缓存：command key → 已完成 Turn 的结果（§8.2.6）。
    done: Mutex<HashMap<String, TurnOutcome>>,
    /// 等待审批的工具调用（tool_call_id → pending）。
    pending: Mutex<HashMap<String, PendingApproval>>,
    /// 正在执行 Turn 的会话，防止并发消息交错写入事件流。
    busy: Mutex<HashSet<SessionId>>,
}

impl TurnEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn EventStore>,
        sessions: Arc<dyn SessionManager>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        policy: Arc<dyn PolicyEngine>,
        routes: RoutingConfig,
        checkpoints: Arc<dyn CheckpointStore>,
        workspace: PathBuf,
        limits: SessionBudgetLimits,
    ) -> Self {
        Self {
            store,
            sessions,
            providers,
            tools,
            policy,
            routes,
            checkpoints,
            workspace,
            limits,
            approval_ttl: APPROVAL_TTL,
            done: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
        }
    }

    /// 测试辅助：覆盖审批有效期。
    #[cfg(test)]
    fn with_approval_ttl(mut self, ttl: ChronoDuration) -> Self {
        self.approval_ttl = ttl;
        self
    }

    /// 从 Event Store 重建幂等缓存与未决审批（§17.1：崩溃恢复）。
    #[allow(clippy::too_many_arguments)]
    pub async fn restore(
        store: Arc<dyn EventStore>,
        sessions: Arc<dyn SessionManager>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        policy: Arc<dyn PolicyEngine>,
        routes: RoutingConfig,
        checkpoints: Arc<dyn CheckpointStore>,
        workspace: PathBuf,
        limits: SessionBudgetLimits,
    ) -> Result<Self, TurnError> {
        let engine = Self::new(
            store,
            sessions,
            providers,
            tools,
            policy,
            routes,
            checkpoints,
            workspace,
            limits,
        );
        let all = engine
            .store
            .load_all_sessions()
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))?;
        for (session_id, events) in all {
            for event in events {
                match event.event_type.as_str() {
                    "message.completed" => {
                        if let Some(key) = event.payload.get("command_key").and_then(Value::as_str)
                        {
                            let outcome = TurnOutcome {
                                turn_id: event.turn_id.unwrap_or_else(TurnId::generate),
                                session_id,
                                status: TurnStatus::Completed,
                                pending_tool_call_id: None,
                                text: event
                                    .payload
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                input_tokens: event
                                    .payload
                                    .get("input_tokens")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                                output_tokens: event
                                    .payload
                                    .get("output_tokens")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                                latest_sequence: event.sequence,
                            };
                            engine
                                .done
                                .lock()
                                .expect("done map poisoned")
                                .insert(key.to_string(), outcome);
                        }
                    }
                    "tool.call.approval_required" => {
                        if let Ok(snap) =
                            serde_json::from_value::<PendingApprovalSnapshot>(event.payload.clone())
                        {
                            let pending = PendingApproval {
                                session_id,
                                turn_id: event.turn_id.unwrap_or_else(TurnId::generate),
                                tool_call_id: snap.tool_call_id,
                                tool: snap.tool,
                                plan: snap.plan,
                                original_key: snap.original_key,
                                expires_at: snap.expires_at,
                                task_kind: snap.task_kind,
                            };
                            engine
                                .pending
                                .lock()
                                .expect("pending map poisoned")
                                .insert(pending.tool_call_id.to_string(), pending);
                        }
                    }
                    // 审批已裁决 / 工具已有终态 → 清理 pending（防御性）。
                    "tool.call.approved"
                    | "tool.call.rejected"
                    | "tool.call.completed"
                    | "tool.call.failed" => {
                        if let Some(id) = event.payload.get("tool_call_id").and_then(Value::as_str)
                        {
                            engine
                                .pending
                                .lock()
                                .expect("pending map poisoned")
                                .remove(id);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(engine)
    }

    /// 发送一条用户消息并同步等待 Turn 完成（或因审批挂起）。
    pub async fn send_message(
        &self,
        session_id: SessionId,
        text: impl Into<String>,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        if let Some(key) = &idempotency_key {
            let cached = self
                .done
                .lock()
                .expect("done map poisoned")
                .get(key)
                .cloned();
            if let Some(outcome) = cached {
                return Ok(outcome);
            }
        }

        let info = self.sessions.status(session_id).await?;
        if info.status != SessionStatus::Running {
            return Err(TurnError::InvalidState(info.status));
        }
        if !self
            .busy
            .lock()
            .expect("busy set poisoned")
            .insert(session_id)
        {
            return Err(TurnError::TurnInProgress(session_id));
        }
        let outcome = self
            .begin_turn(
                session_id,
                text.into(),
                TaskKind::default(),
                idempotency_key,
            )
            .await;
        self.busy
            .lock()
            .expect("busy set poisoned")
            .remove(&session_id);
        outcome
    }

    /// 发送消息并按任务类型路由（§11.3）；默认 [`TaskKind::Coding`]。
    pub async fn send_message_task(
        &self,
        session_id: SessionId,
        text: impl Into<String>,
        task_kind: TaskKind,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        if let Some(key) = &idempotency_key {
            let cached = self
                .done
                .lock()
                .expect("done map poisoned")
                .get(key)
                .cloned();
            if let Some(outcome) = cached {
                return Ok(outcome);
            }
        }

        let info = self.sessions.status(session_id).await?;
        if info.status != SessionStatus::Running {
            return Err(TurnError::InvalidState(info.status));
        }
        if !self
            .busy
            .lock()
            .expect("busy set poisoned")
            .insert(session_id)
        {
            return Err(TurnError::TurnInProgress(session_id));
        }
        let outcome = self
            .begin_turn(session_id, text.into(), task_kind, idempotency_key)
            .await;
        self.busy
            .lock()
            .expect("busy set poisoned")
            .remove(&session_id);
        outcome
    }

    /// 裁决等待中的工具审批并继续 Turn（§9.1：审批锚定 digest、一次性、可过期）。
    pub async fn resolve_approval(
        &self,
        session_id: SessionId,
        tool_call_id: &str,
        approve: bool,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        if let Some(key) = &idempotency_key {
            let cached = self
                .done
                .lock()
                .expect("done map poisoned")
                .get(key)
                .cloned();
            if let Some(outcome) = cached {
                return Ok(outcome);
            }
        }

        let info = self.sessions.status(session_id).await?;
        if info.status != SessionStatus::WaitingApproval {
            return Err(TurnError::InvalidState(info.status));
        }
        if !self
            .busy
            .lock()
            .expect("busy set poisoned")
            .insert(session_id)
        {
            return Err(TurnError::TurnInProgress(session_id));
        }
        let outcome = self
            .resolve_approval_locked(session_id, tool_call_id, approve, idempotency_key)
            .await;
        self.busy
            .lock()
            .expect("busy set poisoned")
            .remove(&session_id);
        outcome
    }

    async fn resolve_approval_locked(
        &self,
        session_id: SessionId,
        tool_call_id: &str,
        approve: bool,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        let pending = self
            .pending
            .lock()
            .expect("pending map poisoned")
            .remove(tool_call_id)
            .ok_or_else(|| TurnError::ApprovalNotFound(tool_call_id.to_string()))?;
        if pending.session_id != session_id {
            return Err(TurnError::ApprovalNotFound(tool_call_id.to_string()));
        }

        let turn_id = pending.turn_id;

        if Utc::now() > pending.expires_at {
            // 过期审批直接失效（§9.1），以拒绝结果回填上下文，Turn 继续推进。
            self.append(
                session_id,
                turn_id,
                "tool.call.rejected",
                Durability::Durable,
                json!({ "tool_call_id": tool_call_id, "reason": "审批已过期" }),
            )
            .await?;
            self.sessions
                .exit_waiting_approval(session_id, None)
                .await?;
            let outcome = self
                .agent_loop(session_id, turn_id, idempotency_key, pending.task_kind)
                .await?;
            self.cache_outcome(&pending.original_key, &outcome);
            return Ok(outcome);
        }

        if approve {
            self.append(
                session_id,
                turn_id,
                "tool.call.approved",
                Durability::Durable,
                json!({
                    "tool_call_id": tool_call_id,
                    "operation_digest": pending.plan.operation_digest.to_string(),
                    "response": "approve_once",
                }),
            )
            .await?;
        } else {
            self.append(
                session_id,
                turn_id,
                "tool.call.rejected",
                Durability::Durable,
                json!({
                    "tool_call_id": tool_call_id,
                    "reason": "用户拒绝该操作",
                    "response": "deny",
                }),
            )
            .await?;
        }
        self.sessions
            .exit_waiting_approval(session_id, None)
            .await?;

        if approve {
            self.execute_preflighted(
                session_id,
                turn_id,
                &pending.tool,
                pending.plan,
                tool_call_id,
            )
            .await?;
        }
        let outcome = self
            .agent_loop(session_id, turn_id, idempotency_key, pending.task_kind)
            .await?;
        self.cache_outcome(&pending.original_key, &outcome);
        Ok(outcome)
    }

    /// Turn 完成后，把原始消息幂等键与裁决幂等键指向同一结果（§8.2.6）。
    fn cache_outcome(&self, original_key: &Option<String>, outcome: &TurnOutcome) {
        if outcome.status != TurnStatus::Completed {
            return;
        }
        let mut done = self.done.lock().expect("done map poisoned");
        if let Some(k) = original_key {
            done.insert(k.clone(), outcome.clone());
        }
    }

    async fn begin_turn(
        &self,
        session_id: SessionId,
        text: String,
        task_kind: TaskKind,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        let turn_id = TurnId::generate();
        self.append(
            session_id,
            turn_id,
            "turn.started",
            Durability::Durable,
            json!({}),
        )
        .await?;
        self.append(
            session_id,
            turn_id,
            "message.created",
            Durability::Durable,
            json!({ "role": "user", "text": text }),
        )
        .await?;
        self.agent_loop(session_id, turn_id, idempotency_key, task_kind)
            .await
    }

    /// Agent 循环：模型调用 →（可选）工具执行 → 结果回填 → 直到最终回复或挂起等审批。
    async fn agent_loop(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        idempotency_key: Option<String>,
        task_kind: TaskKind,
    ) -> Result<TurnOutcome, TurnError> {
        // §11.3：按任务类型路由 Provider；route.model 覆盖默认模型。
        let route = task_kind.route(&self.routes).cloned();
        let provider = self
            .providers
            .resolve(route.as_ref())
            .ok_or(TurnError::NoProvider)?;
        let model_override = route.as_ref().map(|r| r.model.clone());

        loop {
            self.check_budget(session_id).await?;
            let info = self.sessions.status(session_id).await?;
            let session_mode = info.mode;
            let task = info.task;

            let (snapshot, selection_report) = self
                .assemble_snapshot(
                    &provider,
                    session_id,
                    task.as_deref(),
                    model_override.as_deref(),
                )
                .await?;
            let model_request_id = snapshot.model_request_id;
            let used_input_tokens = snapshot.budget.used_input_tokens;
            self.append(
                session_id,
                turn_id,
                "context.snapshot.created",
                Durability::Durable,
                json!({
                    "snapshot_id": snapshot.snapshot_id,
                    "snapshot": &snapshot,
                    "selection_report": &selection_report,
                }),
            )
            .await?;
            self.append(
                session_id,
                turn_id,
                "model.request.started",
                Durability::Durable,
                json!({
                    "model_request_id": model_request_id,
                    "provider": snapshot.model.provider,
                    "model": snapshot.model.model,
                }),
            )
            .await?;

            let mut stream = match provider.stream_chat(&snapshot).await {
                Ok(stream) => stream,
                Err(err) => {
                    self.fail_turn(session_id, turn_id, model_request_id, &err.to_string())
                        .await?;
                    return Err(TurnError::Model(err.to_string()));
                }
            };

            let mut text_out = String::new();
            let mut usage_in = 0u64;
            let mut usage_out = 0u64;
            let mut proposals: Vec<Proposal> = Vec::new();
            while let Some(delta) = stream.next().await {
                match delta {
                    Ok(ChatDelta::Text(piece)) => {
                        text_out.push_str(&piece);
                        self.append(
                            session_id,
                            turn_id,
                            "message.delta",
                            Durability::Transient,
                            json!({ "text": piece }),
                        )
                        .await?;
                    }
                    Ok(ChatDelta::Usage {
                        input_tokens,
                        output_tokens,
                    }) => {
                        usage_in = input_tokens;
                        usage_out = output_tokens;
                    }
                    Ok(ChatDelta::ToolProposal { name, arguments }) => {
                        proposals.push(Proposal { name, arguments });
                    }
                    Err(err) => {
                        self.fail_turn(session_id, turn_id, model_request_id, &err.to_string())
                            .await?;
                        return Err(TurnError::Model(err.to_string()));
                    }
                }
            }

            // Provider 未上报用量时退回本地估算，保证预算累计不空洞（§18.6）。
            if usage_in == 0 {
                usage_in = used_input_tokens;
            }
            if usage_out == 0 {
                usage_out = provider.count_tokens(&text_out).await.unwrap_or_default();
            }

            self.append(
                session_id,
                turn_id,
                "model.request.completed",
                Durability::Durable,
                json!({
                    "model_request_id": model_request_id,
                    "input_tokens": usage_in,
                    "output_tokens": usage_out,
                }),
            )
            .await?;

            let Some(proposal) = proposals.first() else {
                // 无工具提案 → 本轮文本即最终回复，Turn 完成。
                let mut completed = json!({
                    "role": "assistant",
                    "text": text_out,
                    "turn_id": turn_id,
                    "model_request_id": model_request_id,
                    "input_tokens": usage_in,
                    "output_tokens": usage_out,
                });
                if let Some(key) = &idempotency_key {
                    completed["command_key"] = json!(key);
                }
                self.append(
                    session_id,
                    turn_id,
                    "message.completed",
                    Durability::Durable,
                    completed,
                )
                .await?;
                let latest_sequence = self
                    .append(
                        session_id,
                        turn_id,
                        "turn.completed",
                        Durability::Durable,
                        json!({ "turn_id": turn_id }),
                    )
                    .await?;

                let outcome = TurnOutcome {
                    turn_id,
                    session_id,
                    status: TurnStatus::Completed,
                    pending_tool_call_id: None,
                    text: text_out,
                    input_tokens: usage_in,
                    output_tokens: usage_out,
                    latest_sequence,
                };
                if let Some(key) = idempotency_key {
                    self.done
                        .lock()
                        .expect("done map poisoned")
                        .insert(key, outcome.clone());
                }
                return Ok(outcome);
            };

            if !text_out.is_empty() {
                self.append(
                    session_id,
                    turn_id,
                    "message.completed",
                    Durability::Durable,
                    json!({
                        "role": "assistant",
                        "text": text_out,
                        "turn_id": turn_id,
                        "model_request_id": model_request_id,
                    }),
                )
                .await?;
            }
            if proposals.len() > 1 {
                tracing::warn!(
                    turn = %turn_id,
                    count = proposals.len(),
                    "一次模型调用包含多个工具提案，阶段 2 仅处理第一个"
                );
            }

            match self
                .handle_tool_call(
                    session_id,
                    turn_id,
                    &proposal.name,
                    proposal.arguments.clone(),
                    session_mode,
                    idempotency_key.clone(),
                    task_kind,
                )
                .await?
            {
                ToolFlow::Continue => {} // 工具结果已落库，回填上下文后继续下一次模型调用
                ToolFlow::Waiting(tool_call_id) => {
                    let latest_sequence = self
                        .store
                        .latest_sequence(session_id)
                        .await
                        .map_err(|e| TurnError::EventStore(e.to_string()))?;
                    return Ok(TurnOutcome {
                        turn_id,
                        session_id,
                        status: TurnStatus::WaitingApproval,
                        pending_tool_call_id: Some(tool_call_id.to_string()),
                        text: text_out,
                        input_tokens: usage_in,
                        output_tokens: usage_out,
                        latest_sequence,
                    });
                }
            }
        }
    }

    /// 工具提案的完整生命周期（§8.3.4）：proposed → preflighted → policy → 执行/拒绝/等待审批。
    #[allow(clippy::too_many_arguments)]
    async fn handle_tool_call(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        name: &str,
        arguments: Value,
        session_mode: SessionMode,
        original_key: Option<String>,
        task_kind: TaskKind,
    ) -> Result<ToolFlow, TurnError> {
        self.check_tool_budget(session_id).await?;
        let tool_call_id = ToolCallId::generate();
        self.append(
            session_id,
            turn_id,
            "tool.call.proposed",
            Durability::Durable,
            json!({ "tool_call_id": tool_call_id, "tool": name, "arguments": arguments }),
        )
        .await?;

        let Some(executor) = self.tools.lookup(name) else {
            return self
                .reject_tool(
                    session_id,
                    turn_id,
                    tool_call_id,
                    &format!("工具未注册: {name}"),
                )
                .await;
        };
        let definition: ToolDefinition = executor.definition().clone();

        let plan = match executor.preflight(arguments).await {
            Ok(plan) => plan,
            Err(ToolRuntimeError::ResourceConflict(reason)) => {
                // §18.2：源文件在补丁生成后被外部修改 → change.conflicted，
                // 在 Preflight 阶段即拒绝，不进入审批。
                self.append(
                    session_id,
                    turn_id,
                    "change.conflicted",
                    Durability::Durable,
                    json!({ "tool_call_id": tool_call_id, "resource": reason }),
                )
                .await?;
                return self
                    .reject_tool(session_id, turn_id, tool_call_id, &reason)
                    .await;
            }
            Err(err) => {
                return self
                    .reject_tool(session_id, turn_id, tool_call_id, &err.to_string())
                    .await;
            }
        };
        let digest = plan.operation_digest.to_string();
        self.append(
            session_id,
            turn_id,
            "tool.call.preflighted",
            Durability::Durable,
            json!({ "tool_call_id": tool_call_id, "plan": &plan }),
        )
        .await?;

        // 策略上下文：资源取首个 permission 的 resource；只读工具（effect=none）
        // 视为可信读操作（§8.3.7 Low 读操作在 Ask/Plan 放行）；
        // 有副作用的工具默认 workspace_untrusted，按 §18.1 提升审批等级。
        let resource = plan
            .permissions
            .first()
            .map(|p| p.resource.clone())
            .unwrap_or_default();
        let trust = match definition.effect {
            Effect::None => Trust::Trusted,
            Effect::Possible | Effect::Guaranteed => Trust::WorkspaceUntrusted,
        };
        let decision = self.policy.decide(&PolicyContext::new(
            session_mode,
            name,
            resource.clone(),
            plan.risk,
            trust,
        ));
        let mut decision_payload = json!({
            "tool_call_id": tool_call_id,
            "tool": name,
            "decision": decision_tag(&decision),
            "risk": plan.risk.to_string(),
            "resource": resource,
        });
        match &decision {
            PolicyDecision::Allow => {}
            PolicyDecision::RequireApproval { reason } | PolicyDecision::Deny { reason } => {
                decision_payload["reason"] = json!(reason);
            }
        }
        self.append(
            session_id,
            turn_id,
            "policy.decision_made",
            Durability::Durable,
            decision_payload,
        )
        .await?;

        match decision {
            PolicyDecision::Allow => {
                let id = tool_call_id.to_string();
                self.execute_preflighted(session_id, turn_id, name, plan, &id)
                    .await?;
                Ok(ToolFlow::Continue)
            }
            PolicyDecision::Deny { reason } => {
                self.reject_tool(session_id, turn_id, tool_call_id, &reason)
                    .await
            }
            PolicyDecision::RequireApproval { reason } => {
                let expires_at = Utc::now() + self.approval_ttl;
                let pending = PendingApproval {
                    session_id,
                    turn_id,
                    tool_call_id,
                    tool: name.to_string(),
                    original_key,
                    plan,
                    expires_at,
                    task_kind,
                };
                self.append(
                    session_id,
                    turn_id,
                    "tool.call.approval_required",
                    Durability::Durable,
                    json!({
                        "tool_call_id": tool_call_id,
                        "tool": name,
                        "operation_digest": digest,
                        "risk": pending.plan.risk.to_string(),
                        "permissions": pending.plan.permissions,
                        "preview": pending.plan.preview,
                        "reason": reason,
                        "expires_at": expires_at.to_rfc3339(),
                        "plan": pending.plan,
                        "command_key": pending.original_key,
                        "task_kind": pending.task_kind,
                    }),
                )
                .await?;
                self.pending
                    .lock()
                    .expect("pending map poisoned")
                    .insert(tool_call_id.to_string(), pending);
                self.sessions
                    .enter_waiting_approval(session_id, None)
                    .await?;
                Ok(ToolFlow::Waiting(tool_call_id))
            }
        }
    }

    /// 执行已 Preflight 的计划（allow 或 approve_once 之后）。
    async fn execute_preflighted(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        tool: &str,
        plan: ToolExecutionPlan,
        tool_call_id: &str,
    ) -> Result<(), TurnError> {
        // §3.5 / §20.1：任何声明 fs.write 的工具执行前，对目标文件创建 Checkpoint。
        let write_targets: Vec<String> = plan
            .permissions
            .iter()
            .filter(|p| p.capability == Capability::FsWrite)
            .map(|p| p.resource.clone())
            .collect();
        if !write_targets.is_empty() {
            match self
                .checkpoints
                .create(
                    session_id,
                    &format!("{tool} 前"),
                    &self.workspace,
                    &write_targets,
                )
                .await
            {
                Ok(cp) => {
                    self.append(
                        session_id,
                        turn_id,
                        "checkpoint.created",
                        Durability::Durable,
                        json!({ "checkpoint_id": cp.id, "tool": tool, "files": cp.files }),
                    )
                    .await?;
                }
                Err(err) => {
                    return self
                        .fail_tool(
                            session_id,
                            turn_id,
                            tool_call_id,
                            &format!("Checkpoint 创建失败，拒绝执行写操作（§3.5）: {err}"),
                        )
                        .await;
                }
            }
        }

        self.append(
            session_id,
            turn_id,
            "tool.call.started",
            Durability::Durable,
            json!({ "tool_call_id": tool_call_id, "tool": tool }),
        )
        .await?;
        let Some(executor) = self.tools.lookup(tool) else {
            return self
                .fail_tool(session_id, turn_id, tool_call_id, "工具在执行前被移除")
                .await;
        };

        let (tx, mut rx) = mpsc::channel::<ToolOutputChunk>(16);
        let started = std::time::Instant::now();
        let cancel = codedock_tool_runtime::CancellationToken::new();
        let exec = executor.execute(plan, tx, cancel);
        let drain = async {
            let mut outputs = Vec::new();
            while let Some(chunk) = rx.recv().await {
                match &chunk {
                    ToolOutputChunk::Text(text) => outputs.push(text.clone()),
                    ToolOutputChunk::Stdout(bytes) => {
                        outputs.push(String::from_utf8_lossy(bytes).into_owned())
                    }
                    ToolOutputChunk::Stderr(bytes) => {
                        for line in String::from_utf8_lossy(bytes).lines() {
                            outputs.push(format!("[stderr] {line}"));
                        }
                    }
                }
            }
            outputs
        };
        let (result, outputs) = tokio::join!(exec, drain);
        for text in outputs {
            self.append(
                session_id,
                turn_id,
                "tool.call.output",
                Durability::Transient,
                json!({ "tool_call_id": tool_call_id, "text": text }),
            )
            .await?;
        }

        match result {
            Ok(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.append(
                    session_id,
                    turn_id,
                    "tool.call.completed",
                    Durability::Durable,
                    json!({
                        "tool_call_id": tool_call_id,
                        "status": result.status,
                        "text": text,
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "actual_side_effects": result.actual_side_effects,
                    }),
                )
                .await?;
                Ok(())
            }
            Err(ToolRuntimeError::ResourceConflict(reason)) => {
                // §18.2：源文件在补丁生成后被外部修改 → change.conflicted，禁止覆盖。
                self.append(
                    session_id,
                    turn_id,
                    "change.conflicted",
                    Durability::Durable,
                    json!({ "tool_call_id": tool_call_id, "resource": reason }),
                )
                .await?;
                self.fail_tool(session_id, turn_id, tool_call_id, &reason)
                    .await
            }
            Err(err) => {
                self.fail_tool(session_id, turn_id, tool_call_id, &err.to_string())
                    .await
            }
        }
    }

    /// 恢复（回滚）到指定 Checkpoint：显式用户操作（§24 一键回滚，force 覆盖）。
    pub async fn restore_checkpoint(
        &self,
        session_id: SessionId,
        checkpoint_id: &str,
        idempotency_key: Option<String>,
    ) -> Result<serde_json::Value, TurnError> {
        if let Some(key) = &idempotency_key {
            let cached = self
                .done
                .lock()
                .expect("done map poisoned")
                .get(key)
                .cloned();
            // 回滚结果复用幂等缓存结构中 latest_sequence 之外的字段意义有限，
            // 这里仅防重复执行：命中即直接返回成功载荷。
            if cached.is_some() {
                return Ok(json!({ "checkpoint_id": checkpoint_id, "restored": true }));
            }
        }

        let info = self.sessions.status(session_id).await?;
        if info.status.is_terminal() {
            return Err(TurnError::InvalidState(info.status));
        }
        let restored = self
            .checkpoints
            .restore(checkpoint_id, &self.workspace, true)
            .await
            .map_err(|e| TurnError::Checkpoint(e.to_string()))?;
        self.append_no_turn(
            session_id,
            "checkpoint.restored",
            json!({ "checkpoint_id": checkpoint_id, "files": restored }),
        )
        .await?;
        Ok(json!({ "checkpoint_id": checkpoint_id, "restored": true, "files": restored }))
    }

    async fn reject_tool(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        reason: &str,
    ) -> Result<ToolFlow, TurnError> {
        self.append(
            session_id,
            turn_id,
            "tool.call.rejected",
            Durability::Durable,
            json!({ "tool_call_id": tool_call_id, "reason": reason }),
        )
        .await?;
        Ok(ToolFlow::Continue)
    }

    async fn fail_tool(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        tool_call_id: &str,
        error: &str,
    ) -> Result<(), TurnError> {
        self.append(
            session_id,
            turn_id,
            "tool.call.failed",
            Durability::Durable,
            json!({ "tool_call_id": tool_call_id, "error": error }),
        )
        .await?;
        Ok(())
    }

    /// Turn 失败收尾：model.request.failed + turn.failed；Session 保持 Running。
    async fn fail_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        model_request_id: codedock_protocol::ModelRequestId,
        error: &str,
    ) -> Result<(), TurnError> {
        self.append(
            session_id,
            turn_id,
            "model.request.failed",
            Durability::Durable,
            json!({ "model_request_id": model_request_id, "error": error }),
        )
        .await?;
        self.append(
            session_id,
            turn_id,
            "turn.failed",
            Durability::Durable,
            json!({ "turn_id": turn_id, "error": error }),
        )
        .await?;
        Ok(())
    }

    /// 预算检查（§18.3）：模型调用数、Turn 数与累计 Token。
    async fn check_budget(&self, session_id: SessionId) -> Result<(), TurnError> {
        let (turns, model_calls, tool_calls, tokens) = self.usage_counters(session_id).await?;
        if turns >= self.limits.max_turns {
            return Err(TurnError::BudgetExceeded(format!(
                "已用 {turns} 轮 ≥ max_turns {}",
                self.limits.max_turns
            )));
        }
        if model_calls >= self.limits.max_model_calls {
            return Err(TurnError::BudgetExceeded(format!(
                "已用 {model_calls} 次模型调用 ≥ max_model_calls {}",
                self.limits.max_model_calls
            )));
        }
        if tool_calls >= self.limits.max_tool_calls {
            return Err(TurnError::BudgetExceeded(format!(
                "已用 {tool_calls} 次工具调用 ≥ max_tool_calls {}",
                self.limits.max_tool_calls
            )));
        }
        if tokens >= self.limits.max_total_tokens {
            return Err(TurnError::BudgetExceeded(format!(
                "累计 {tokens} tokens ≥ max_total_tokens {}",
                self.limits.max_total_tokens
            )));
        }
        Ok(())
    }

    /// 工具调用数单独校验（handle_tool_call 前调用，同一次扫描完成）。
    async fn check_tool_budget(&self, session_id: SessionId) -> Result<(), TurnError> {
        let (_, _, tool_calls, _) = self.usage_counters(session_id).await?;
        if tool_calls >= self.limits.max_tool_calls {
            return Err(TurnError::BudgetExceeded(format!(
                "已用 {tool_calls} 次工具调用 ≥ max_tool_calls {}",
                self.limits.max_tool_calls
            )));
        }
        Ok(())
    }

    async fn usage_counters(
        &self,
        session_id: SessionId,
    ) -> Result<(u32, u32, u32, u64), TurnError> {
        let events = self
            .store
            .load(session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))?;
        let mut turns = 0u32;
        let mut model_calls = 0u32;
        let mut tool_calls = 0u32;
        let mut tokens = 0u64;
        for event in events {
            match event.event_type.as_str() {
                // 以 completed 计数：当前进行中的 Turn 不占用自身额度。
                "turn.completed" => turns += 1,
                "model.request.started" => model_calls += 1,
                "tool.call.proposed" => tool_calls += 1,
                "model.request.completed" => {
                    tokens += event
                        .payload
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    tokens += event
                        .payload
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
                _ => {}
            }
        }
        Ok((turns, model_calls, tool_calls, tokens))
    }

    /// 组装 Context Snapshot（§8.4）：system prompt + 任务 + 历史消息 + 工具调用与结果。
    async fn assemble_snapshot(
        &self,
        provider: &Arc<dyn ModelProvider>,
        session_id: SessionId,
        task: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<(ContextSnapshot, codedock_context_engine::SelectionReport), TurnError> {
        let models = provider
            .list_models()
            .await
            .map_err(|e| TurnError::Model(e.to_string()))?;
        // route.model 优先；未命中清单时回退首个并告警（§11.3）。
        let model = match model_override {
            Some(want) => match models.iter().find(|m| m.id == *want) {
                Some(m) => m.clone(),
                None => {
                    tracing::warn!(
                        provider = %provider.id(),
                        model = %want,
                        "路由指定的模型不在 Provider 清单中，回退默认模型"
                    );
                    models
                        .first()
                        .ok_or_else(|| TurnError::Model("Provider 未提供任何模型".to_string()))?
                        .clone()
                }
            },
            None => models
                .first()
                .ok_or_else(|| TurnError::Model("Provider 未提供任何模型".to_string()))?
                .clone(),
        };
        let model_ref = ModelRef {
            provider: provider.id().to_string(),
            model: model.id.clone(),
            context_window: model.context_window,
        };

        let events = self
            .store
            .load(session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))?;

        let mut candidates = Vec::new();
        candidates.push(
            self.candidate(
                "system_prompt",
                Role::Instruction,
                SelectionReason::ProjectRule,
                100,
                SourceKind::Other("runtime".into()),
                "runtime://system_prompt".into(),
                SYSTEM_PROMPT.to_string(),
                provider,
            )
            .await?,
        );
        if let Some(task) = task.filter(|t| !t.trim().is_empty()) {
            candidates.push(
                self.candidate(
                    "task",
                    Role::Instruction,
                    SelectionReason::UserAttached,
                    100,
                    SourceKind::Message,
                    format!("session://{session_id}"),
                    task.to_string(),
                    provider,
                )
                .await?,
            );
        }

        let session_uri = format!("session://{session_id}");
        for event in events {
            match event.event_type.as_str() {
                "message.created" => {
                    if event.payload.get("role").and_then(Value::as_str) == Some("user") {
                        if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                            candidates.push(
                                self.candidate(
                                    "user_message",
                                    Role::Instruction,
                                    SelectionReason::UserAttached,
                                    60,
                                    SourceKind::Message,
                                    session_uri.clone(),
                                    text.to_string(),
                                    provider,
                                )
                                .await?,
                            );
                        }
                    }
                }
                "message.completed" => {
                    if event.payload.get("role").and_then(Value::as_str) == Some("assistant")
                        && !event
                            .payload
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .is_empty()
                    {
                        let text = event.payload["text"].as_str().unwrap_or_default();
                        candidates.push(
                            self.candidate(
                                "assistant_message",
                                Role::AssistantHistory,
                                SelectionReason::SessionMemory,
                                50,
                                SourceKind::Message,
                                session_uri.clone(),
                                text.to_string(),
                                provider,
                            )
                            .await?,
                        );
                    }
                }
                "tool.call.proposed" => {
                    let name = event.payload.get("tool").cloned().unwrap_or(Value::Null);
                    let args = event.payload.get("arguments").cloned().unwrap_or(json!({}));
                    let text =
                        json!({ "tool_proposal": { "name": name, "arguments": args } }).to_string();
                    candidates.push(
                        self.candidate(
                            "tool_proposal",
                            Role::AssistantHistory,
                            SelectionReason::AgentSelected,
                            50,
                            SourceKind::Message,
                            session_uri.clone(),
                            text,
                            provider,
                        )
                        .await?,
                    );
                }
                "tool.call.completed" => {
                    let text = event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    candidates.push(
                        self.candidate(
                            "tool_result",
                            Role::Data,
                            SelectionReason::ToolResult,
                            80,
                            SourceKind::ToolOutput,
                            session_uri.clone(),
                            format!("[工具结果] {text}"),
                            provider,
                        )
                        .await?,
                    );
                }
                "tool.call.rejected" => {
                    let reason = event
                        .payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    candidates.push(
                        self.candidate(
                            "tool_result",
                            Role::Data,
                            SelectionReason::ToolResult,
                            80,
                            SourceKind::ToolOutput,
                            session_uri.clone(),
                            format!("[工具被拒绝] {reason}"),
                            provider,
                        )
                        .await?,
                    );
                }
                "tool.call.failed" => {
                    let error = event
                        .payload
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    candidates.push(
                        self.candidate(
                            "tool_result",
                            Role::Data,
                            SelectionReason::ToolResult,
                            80,
                            SourceKind::ToolOutput,
                            session_uri.clone(),
                            format!("[工具失败] {error}"),
                            provider,
                        )
                        .await?,
                    );
                }
                _ => {}
            }
        }

        // §8.4.9/§12.3：预算装箱 + 选择报告。score = priority/100；
        // 同分保持事件时序（稳定排序），装箱后按原始顺序还原会话结构。
        let original_order: std::collections::HashMap<codedock_protocol::EventId, usize> =
            candidates
                .iter()
                .enumerate()
                .map(|(idx, c)| (c.item.item_id, idx))
                .collect();
        let budget = ContextBudget::new(model.context_window, model.max_output);
        let builder = codedock_context_engine::SnapshotBuilder::new(model_ref.clone(), budget);
        let (mut snapshot, report) = builder
            .build(session_id, candidates)
            .map_err(|e| TurnError::Context(e.to_string()))?;
        snapshot.items.sort_by_key(|i| {
            original_order
                .get(&i.item_id)
                .copied()
                .unwrap_or(usize::MAX)
        });

        // §8.4.1：记录最终发送给 Provider 的请求体哈希。
        if let Some(payload) = provider.audit_payload(&snapshot).await {
            if let Ok(encoded) = serde_json::to_string(&payload) {
                let digest = sha2::Sha256::digest(encoded.as_bytes());
                snapshot.final_request_sha256 = Some(format!("sha256:{}", hex::encode(digest)));
            }
        }

        Ok((snapshot, report))
    }

    /// 构造候选条目并估算 Token（§8.4.3）；score 供预算装箱排序。
    #[allow(clippy::too_many_arguments)]
    async fn candidate(
        &self,
        kind: &str,
        role: Role,
        reason: SelectionReason,
        priority: i32,
        source_kind: SourceKind,
        uri: String,
        text: String,
        provider: &Arc<dyn ModelProvider>,
    ) -> Result<codedock_context_engine::Candidate, TurnError> {
        let tokens = provider.count_tokens(&text).await.unwrap_or_default();
        Ok(codedock_context_engine::Candidate {
            score: priority as f32 / 100.0,
            item: ContextItem {
                item_id: codedock_protocol::EventId::generate(),
                kind: kind.into(),
                role,
                source: SourceRef {
                    kind: source_kind,
                    uri,
                    revision: None,
                },
                title: kind.into(),
                content: ContextItemContent::Inline { text },
                range: None,
                selection: Selection {
                    reason,
                    selected_by: "context_engine".into(),
                    score: 1.0,
                    priority,
                },
                trust: if role == Role::Data {
                    Trust::WorkspaceUntrusted
                } else {
                    Trust::Trusted
                },
                classification: Classification::Internal,
                tokens,
                transformations: Vec::new(),
            },
        })
    }

    /// 构造一个 Snapshot 条目
    /// 追加一条事件；sequence 由 Event Store 分配（§8.2.3）。
    async fn append(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        event_type: &str,
        durability: Durability,
        payload: Value,
    ) -> Result<u64, TurnError> {
        self.append_envelope(
            session_id,
            Some(turn_id),
            event_type,
            durability,
            Actor::agent("primary"),
            payload,
        )
        .await
    }

    /// Turn 之外的用户操作事件（如 checkpoint.restored）。
    async fn append_no_turn(
        &self,
        session_id: SessionId,
        event_type: &str,
        payload: Value,
    ) -> Result<u64, TurnError> {
        self.append_envelope(
            session_id,
            None,
            event_type,
            Durability::Durable,
            Actor::user("local"),
            payload,
        )
        .await
    }

    async fn append_envelope(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        event_type: &str,
        durability: Durability,
        actor: codedock_protocol::Actor,
        payload: Value,
    ) -> Result<u64, TurnError> {
        let envelope =
            EventEnvelope::draft(session_id, turn_id, event_type, durability, actor, payload);
        self.store
            .append(envelope)
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))
    }
}

enum ToolFlow {
    /// 工具已执行/拒绝，结果已回填 → 继续模型循环。
    Continue,
    /// 等待用户审批，Turn 挂起（携带待审批的 tool_call_id）。
    Waiting(ToolCallId),
}

fn decision_tag(decision: &PolicyDecision) -> &'static str {
    match decision {
        PolicyDecision::Allow => "allow",
        PolicyDecision::RequireApproval { .. } => "require_approval",
        PolicyDecision::Deny { .. } => "deny",
    }
}

/// `tool.call.approval_required` 事件载荷中用于恢复 pending 的部分。
#[derive(serde::Deserialize)]
struct PendingApprovalSnapshot {
    tool_call_id: ToolCallId,
    tool: String,
    plan: ToolExecutionPlan,
    #[serde(default, rename = "command_key")]
    original_key: Option<String>,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    task_kind: TaskKind,
}

impl From<ToolRuntimeError> for TurnError {
    fn from(e: ToolRuntimeError) -> Self {
        match e {
            ToolRuntimeError::Cancelled => TurnError::Tool("已取消".into()),
            other => TurnError::Tool(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_event_store::InMemoryEventStore;
    use codedock_model_gateway::{MockProvider, ModelGatewayError};
    use codedock_policy_engine::DefaultPolicyEngine;
    use codedock_protocol::{Capability, Permission, Risk, ToolResult};
    use codedock_tool_runtime::{ToolExecutor, ToolOutputChunk};

    use crate::EventSourcedSessionManager;
    use std::path::PathBuf;

    struct FailingProvider;

    #[async_trait::async_trait]
    impl ModelProvider for FailingProvider {
        fn id(&self) -> &str {
            "failing"
        }

        fn capabilities(&self) -> &[codedock_model_gateway::ProviderCapability] {
            &[]
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<codedock_model_gateway::ModelInfo>, ModelGatewayError> {
            Ok(vec![codedock_model_gateway::ModelInfo {
                id: "boom".into(),
                context_window: 1_000,
                max_output: 100,
            }])
        }

        async fn count_tokens(&self, _text: &str) -> Result<u64, ModelGatewayError> {
            Ok(1)
        }

        async fn stream_chat(
            &self,
            _snapshot: &ContextSnapshot,
        ) -> Result<
            Box<dyn futures::Stream<Item = Result<ChatDelta, ModelGatewayError>> + Send + Unpin>,
            ModelGatewayError,
        > {
            Err(ModelGatewayError::Network("boom".into()))
        }

        async fn cancel_request(&self, _request_id: &str) -> Result<(), ModelGatewayError> {
            Ok(())
        }

        async fn health_check(&self) -> Result<(), ModelGatewayError> {
            Ok(())
        }
    }

    /// 有副作用的测试工具：High 风险写操作，用于触发审批 / 拒绝路径。
    struct TestWriteTool {
        risk: Risk,
        path_arg: String,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for TestWriteTool {
        fn definition(&self) -> &ToolDefinition {
            static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
            DEF.get_or_init(|| {
                let mut def = ToolDefinition::builtin("test.write", vec![Capability::FsWrite]);
                def.effect = Effect::Possible;
                def
            })
        }

        async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
            let target = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(&self.path_arg)
                .to_string();
            Ok(ToolExecutionPlan {
                tool_call_id: ToolCallId::generate(),
                normalized_arguments: json!({ "path": target }),
                permissions: vec![Permission {
                    capability: Capability::FsWrite,
                    resource: target.clone(),
                }],
                risk: self.risk,
                expected_side_effects: vec![],
                operation_digest: codedock_protocol::OperationDigest::from_sha256_hex(
                    "test-write-digest",
                ),
                preview: format!("写入 {target}"),
            })
        }

        async fn execute(
            &self,
            plan: ToolExecutionPlan,
            output: mpsc::Sender<ToolOutputChunk>,
            _cancel: codedock_tool_runtime::CancellationToken,
        ) -> Result<ToolResult, ToolRuntimeError> {
            let _ = output.send(ToolOutputChunk::Text("written".into())).await;
            Ok(ToolResult {
                tool_call_id: plan.tool_call_id,
                status: codedock_protocol::ToolResultStatus::Success,
                content: vec![ToolResultContent::Text {
                    text: "written".into(),
                }],
                artifacts: Vec::new(),
                diagnostics: Vec::new(),
                actual_side_effects: vec![],
                duration_ms: 1,
            })
        }
    }

    fn test_checkpoint_store() -> Arc<dyn codedock_checkpoint_manager::CheckpointStore> {
        Arc::new(codedock_checkpoint_manager::DiskCheckpointStore::new(
            std::env::temp_dir().join(format!(
                "codedock-ckpt-test-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7().simple()
            )),
        ))
    }

    fn registry_with_file_read(root: &PathBuf) -> Arc<ToolRegistry> {
        let mut tools = ToolRegistry::new();
        tools
            .register(Box::new(codedock_tool_runtime::FileReadTool::new(root)))
            .unwrap();
        Arc::new(tools)
    }

    fn registry_with(tools: Vec<Box<dyn ToolExecutor>>) -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::new();
        for t in tools {
            reg.register(t).unwrap();
        }
        Arc::new(reg)
    }

    async fn engine_with(
        provider: Arc<dyn ModelProvider>,
        tools: Arc<ToolRegistry>,
        limits: SessionBudgetLimits,
    ) -> (Arc<dyn SessionManager>, TurnEngine) {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let sessions: Arc<dyn SessionManager> =
            Arc::new(EventSourcedSessionManager::new(store.clone()));
        let engine = TurnEngine::new(
            store,
            sessions.clone(),
            Arc::new({
                let mut reg = ProviderRegistry::new();
                reg.register(provider);
                reg
            }),
            tools,
            Arc::new(DefaultPolicyEngine),
            RoutingConfig::default(),
            test_checkpoint_store(),
            std::env::temp_dir(),
            limits,
        );
        (sessions, engine)
    }

    async fn durable_types(engine: &TurnEngine, sid: SessionId) -> Vec<String> {
        engine
            .store
            .load(sid, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }

    #[tokio::test]
    async fn happy_path_emits_full_event_chain() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::Text("你好，我是 mock".into())]);
        let dir = std::env::temp_dir();
        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "你好", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);
        assert_eq!(out.text, "你好，我是 mock");

        let types = durable_types(&engine, info.session_id).await;
        assert_eq!(
            types,
            [
                "session.created",
                "session.started",
                "turn.started",
                "message.created",
                "context.snapshot.created",
                "model.request.started",
                "model.request.completed",
                "message.completed",
                "turn.completed",
            ]
        );
        let all = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, false)
            .await
            .unwrap();
        assert!(all.iter().any(|e| e.event_type == "message.delta"));
        assert!(
            all.iter()
                .filter(|e| e.event_type == "message.delta")
                .all(|e| e.durability == Durability::Transient)
        );
    }

    #[tokio::test]
    async fn tool_proposal_is_executed_and_result_fed_back() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-turn-tool-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("notes.txt"), "工作区笔记内容")
            .await
            .unwrap();

        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.read".into(),
            arguments: json!({ "path": "notes.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("文件里写的是：工作区笔记内容".into())]);

        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "帮我读一下 notes.txt", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);
        assert_eq!(out.text, "文件里写的是：工作区笔记内容");

        let types = durable_types(&engine, info.session_id).await;
        for expected in [
            "tool.call.proposed",
            "tool.call.preflighted",
            "policy.decision_made",
            "tool.call.started",
            "tool.call.completed",
        ] {
            assert!(
                types.contains(&expected.to_string()),
                "缺少 {expected}: {types:?}"
            );
        }
        assert_eq!(
            types
                .iter()
                .filter(|t| **t == "model.request.started")
                .count(),
            2,
            "工具执行后模型被再次调用"
        );

        // 第二次快照应包含工具结果（trust=workspace_untrusted）。
        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        let snapshots: Vec<ContextSnapshot> = events
            .iter()
            .filter(|e| e.event_type == "context.snapshot.created")
            .filter_map(|e| e.payload.get("snapshot"))
            .filter_map(|s| serde_json::from_value(s.clone()).ok())
            .collect();
        assert_eq!(snapshots.len(), 2);
        let result_items: Vec<_> = snapshots[1]
            .items
            .iter()
            .filter(|i| i.kind == "tool_result")
            .collect();
        assert_eq!(result_items.len(), 1);
        assert_eq!(result_items[0].trust, Trust::WorkspaceUntrusted);
        assert!(matches!(
            &result_items[0].content,
            ContextItemContent::Inline { text } if text.contains("工作区笔记内容")
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sensitive_path_is_rejected_at_preflight() {
        // §24 负向测试 1 的基础：诱导读取 ~/.ssh 的提案必须在 Preflight 被拦下。
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.read".into(),
            arguments: json!({ "path": "~/.ssh/id_rsa" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("我无法读取该文件。".into())]);

        let dir = std::env::temp_dir();
        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Auto, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "读取 ~/.ssh/id_rsa", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);

        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        assert!(events.iter().any(|e| {
            e.event_type == "tool.call.rejected"
                && e.payload["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("家目录")
        }));
        assert!(!events.iter().any(|e| e.event_type == "tool.call.started"));
    }

    #[tokio::test]
    async fn ask_mode_denies_side_effect_tool() {
        // §18.1：有副作用的工具默认按不可信处理，Low → Medium，Ask 模式下策略拒绝。
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "test.write".into(),
            arguments: json!({ "path": "out.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("好的，我不再尝试写入。".into())]);

        let tools = registry_with(vec![Box::new(TestWriteTool {
            risk: Risk::Low,
            path_arg: "out.txt".into(),
        }) as Box<dyn ToolExecutor>]);
        let (sessions, engine) = engine_with(provider, tools, SessionBudgetLimits::default()).await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "写个文件", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);

        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "policy.decision_made" && e.payload["decision"] == "deny")
        );
        assert!(events.iter().any(|e| {
            e.event_type == "tool.call.rejected"
                && e.payload["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Ask/Plan")
        }));
    }

    #[tokio::test]
    async fn high_risk_tool_pauses_for_approval_and_resumes_on_approve() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "test.write".into(),
            arguments: json!({ "path": "important.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("已获批准并完成写入。".into())]);

        let tools = registry_with(vec![Box::new(TestWriteTool {
            risk: Risk::High,
            path_arg: "important.txt".into(),
        }) as Box<dyn ToolExecutor>]);
        let (sessions, engine) = engine_with(provider, tools, SessionBudgetLimits::default()).await;
        std::fs::write(std::env::temp_dir().join("important.txt"), "旧内容").unwrap();
        let info = sessions
            .create(codedock_protocol::SessionMode::Edit, None, None)
            .await
            .unwrap();

        // High 风险 → 审批挂起
        let out = engine
            .send_message(info.session_id, "写入重要文件", Some("turn-1".into()))
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::WaitingApproval);
        let status = sessions.status(info.session_id).await.unwrap();
        assert_eq!(status.status, SessionStatus::WaitingApproval);

        // 挂起期间拒绝新消息
        let err = engine
            .send_message(info.session_id, "新消息", None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TurnError::InvalidState(SessionStatus::WaitingApproval)
        ));

        // 找到 pending 的 tool_call_id
        let tool_call_id = {
            let pending = engine.pending.lock().unwrap();
            pending.keys().next().unwrap().clone()
        };

        // 批准 → Turn 继续 → 完成
        let final_out = engine
            .resolve_approval(
                info.session_id,
                &tool_call_id,
                true,
                Some("approve-1".into()),
            )
            .await
            .unwrap();
        assert_eq!(final_out.status, TurnStatus::Completed);
        assert_eq!(final_out.text, "已获批准并完成写入。");
        let status = sessions.status(info.session_id).await.unwrap();
        assert_eq!(status.status, SessionStatus::Running);

        let types = durable_types(&engine, info.session_id).await;
        assert!(types.contains(&"tool.call.approved".to_string()));
        assert!(types.contains(&"tool.call.completed".to_string()));
        assert!(types.contains(&"session.waiting_approval".to_string()));

        // 幂等：原消息 key 与审批 key 都指向同一 Turn 结果
        let replay = engine
            .send_message(info.session_id, "写入重要文件", Some("turn-1".into()))
            .await
            .unwrap();
        assert_eq!(replay.turn_id, final_out.turn_id);
        let _ = engine
            .resolve_approval(
                info.session_id,
                &tool_call_id,
                true,
                Some("approve-1".into()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn approval_deny_feeds_rejection_back_to_model() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "test.write".into(),
            arguments: json!({ "path": "x.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("明白，已取消写入。".into())]);

        let tools = registry_with(vec![Box::new(TestWriteTool {
            risk: Risk::High,
            path_arg: "x.txt".into(),
        }) as Box<dyn ToolExecutor>]);
        let (sessions, engine) = engine_with(provider, tools, SessionBudgetLimits::default()).await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Edit, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "写入", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::WaitingApproval);
        let tool_call_id = {
            let pending = engine.pending.lock().unwrap();
            pending.keys().next().unwrap().clone()
        };

        let final_out = engine
            .resolve_approval(info.session_id, &tool_call_id, false, None)
            .await
            .unwrap();
        assert_eq!(final_out.status, TurnStatus::Completed);
        assert_eq!(final_out.text, "明白，已取消写入。");

        let types = durable_types(&engine, info.session_id).await;
        assert!(types.contains(&"tool.call.rejected".to_string()));
        assert!(!types.contains(&"tool.call.completed".to_string()));
        assert!(!types.contains(&"tool.call.approved".to_string()));
    }

    #[tokio::test]
    async fn expired_approval_is_invalid() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "test.write".into(),
            arguments: json!({ "path": "x.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("审批超时，已中止。".into())]);

        let tools = registry_with(vec![Box::new(TestWriteTool {
            risk: Risk::High,
            path_arg: "x.txt".into(),
        }) as Box<dyn ToolExecutor>]);
        let (sessions, engine) = engine_with(provider, tools, SessionBudgetLimits::default()).await;
        let engine = engine.with_approval_ttl(ChronoDuration::zero());
        let info = sessions
            .create(codedock_protocol::SessionMode::Edit, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "写入", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::WaitingApproval);
        let tool_call_id = {
            let pending = engine.pending.lock().unwrap();
            pending.keys().next().unwrap().clone()
        };

        let final_out = engine
            .resolve_approval(info.session_id, &tool_call_id, true, None)
            .await
            .unwrap();
        assert_eq!(final_out.status, TurnStatus::Completed);
        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        assert!(
            events.iter().any(
                |e| e.event_type == "tool.call.rejected" && e.payload["reason"] == "审批已过期"
            )
        );
        assert!(!events.iter().any(|e| e.event_type == "tool.call.approved"));
    }

    #[tokio::test]
    async fn pending_approval_survives_restart() {
        // 崩溃恢复：审批挂起后重启，pending 从事件流重建，裁决可继续。
        let dir = std::env::temp_dir().join(format!(
            "codedock-turn-restore-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        type TestParts = (
            Arc<dyn EventStore>,
            Arc<dyn SessionManager>,
            Arc<ProviderRegistry>,
            Arc<ToolRegistry>,
        );
        let make_parts = || -> TestParts {
            let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
            let sessions: Arc<dyn SessionManager> =
                Arc::new(EventSourcedSessionManager::new(store.clone()));
            let mut reg = ProviderRegistry::new();
            let mock = Arc::new(MockProvider::new("mock", "mock-model"));
            mock.push_script(vec![ChatDelta::ToolProposal {
                name: "test.write".into(),
                arguments: json!({ "path": "x.txt" }),
            }]);
            mock.push_script(vec![ChatDelta::Text("恢复后完成。".into())]);
            reg.register(mock);
            let mut tools = ToolRegistry::new();
            tools
                .register(Box::new(TestWriteTool {
                    risk: Risk::High,
                    path_arg: "x.txt".into(),
                }))
                .unwrap();
            (store, sessions, Arc::new(reg), Arc::new(tools))
        };

        let session_id;
        std::fs::write(std::env::temp_dir().join("x.txt"), "待写入").unwrap();
        let (store, sessions, providers, tools) = make_parts();
        {
            let engine = TurnEngine::new(
                store.clone(),
                sessions.clone(),
                providers.clone(),
                tools.clone(),
                Arc::new(DefaultPolicyEngine),
                RoutingConfig::default(),
                test_checkpoint_store(),
                std::env::temp_dir(),
                SessionBudgetLimits::default(),
            );
            let info = sessions
                .create(codedock_protocol::SessionMode::Edit, None, None)
                .await
                .unwrap();
            session_id = info.session_id;
            let out = engine.send_message(session_id, "写入", None).await.unwrap();
            assert_eq!(out.status, TurnStatus::WaitingApproval);
        }

        // “重启”：restore 重建 pending
        let (store2, sessions2, providers2, tools2) = {
            // 复用同一 provider（脚本还剩第二段）
            (
                store.clone(),
                sessions.clone(),
                providers.clone(),
                tools.clone(),
            )
        };
        let _ = (store2, sessions2, providers2, tools2);
        let engine2 = TurnEngine::restore(
            store.clone(),
            sessions.clone(),
            providers.clone(),
            tools.clone(),
            Arc::new(DefaultPolicyEngine),
            RoutingConfig::default(),
            test_checkpoint_store(),
            std::env::temp_dir(),
            SessionBudgetLimits::default(),
        )
        .await
        .unwrap();

        let status = sessions.status(session_id).await.unwrap();
        assert_eq!(status.status, SessionStatus::WaitingApproval);
        let tool_call_id = {
            let pending = engine2.pending.lock().unwrap();
            assert_eq!(pending.len(), 1, "未决审批应被恢复");
            pending.keys().next().unwrap().clone()
        };

        let final_out = engine2
            .resolve_approval(session_id, &tool_call_id, true, None)
            .await
            .unwrap();
        assert_eq!(final_out.status, TurnStatus::Completed);
        assert_eq!(final_out.text, "恢复后完成。");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_patch_creates_checkpoint_and_reports_conflict() {
        let ws = std::env::temp_dir().join(format!(
            "codedock-turn-patch-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("doc.txt"), "第一版内容\n")
            .await
            .unwrap();
        let sha_v1 = codedock_checkpoint_manager::hash_content("第一版内容\n".as_bytes());

        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.patch".into(),
            arguments: json!({
                "path": "doc.txt",
                "expected_sha256": sha_v1,
                "patches": [ { "find": "第一版内容", "replace": "第二版内容" } ]
            }),
        }]);
        provider.push_script(vec![ChatDelta::Text("补丁已应用。".into())]);
        // 冲突场景：基于第一版生成的过期补丁
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.patch".into(),
            arguments: json!({
                "path": "doc.txt",
                "expected_sha256": sha_v1,
                "patches": [ { "find": "第一版内容", "replace": "第三版" } ]
            }),
        }]);
        provider.push_script(vec![ChatDelta::Text("检测到冲突，已停止。".into())]);

        let mut tools = ToolRegistry::new();
        tools
            .register(Box::new(codedock_tool_runtime::FilePatchTool::new(&ws)))
            .unwrap();
        let (sessions, engine) =
            engine_with(provider, Arc::new(tools), SessionBudgetLimits::default()).await;
        // 引擎工作区指向测试工作区
        let engine = {
            // 直接借用 engine 内部字段重建（测试专用路径）
            TurnEngine::new(
                engine.store.clone(),
                engine.sessions.clone(),
                engine.providers.clone(),
                engine.tools.clone(),
                engine.policy.clone(),
                engine.routes.clone(),
                engine.checkpoints.clone(),
                ws.clone(),
                SessionBudgetLimits::default(),
            )
        };

        let info = sessions
            .create(codedock_protocol::SessionMode::Edit, None, None)
            .await
            .unwrap();
        // Medium 写操作在默认语境下升级为审批（§18.1）
        let out = engine
            .send_message(info.session_id, "改文档", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::WaitingApproval);
        let pending_id = out.pending_tool_call_id.expect("应有待审批 id");
        let out = engine
            .resolve_approval(info.session_id, &pending_id, true, None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);
        let content = tokio::fs::read_to_string(ws.join("doc.txt")).await.unwrap();
        assert_eq!(content, "第二版内容\n");

        let types = durable_types(&engine, info.session_id).await;
        assert!(types.contains(&"checkpoint.created".to_string()));
        assert!(types.contains(&"tool.call.approved".to_string()));
        assert!(types.contains(&"tool.call.completed".to_string()));

        // 第二轮：stale hash → Preflight 即 change.conflicted，不进入审批
        let out = engine
            .send_message(info.session_id, "再来一次旧补丁", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);
        let types = durable_types(&engine, info.session_id).await;
        assert!(types.contains(&"change.conflicted".to_string()));
        assert!(types.contains(&"tool.call.rejected".to_string()));
        // 冲突轮不产生第二个 completed（本轮无 started）
        assert_eq!(
            types.iter().filter(|t| **t == "tool.call.started").count(),
            1,
            "只有第一轮补丁实际执行"
        );
        let content = tokio::fs::read_to_string(ws.join("doc.txt")).await.unwrap();
        assert_eq!(content, "第二版内容\n", "冲突时不得覆盖文件");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn task_kind_routes_to_configured_provider() {
        // §11.3：routes.coding → provider "b"；planning 未配置 → 回退默认 "a"。
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let sessions: Arc<dyn SessionManager> =
            Arc::new(EventSourcedSessionManager::new(store.clone()));

        let a = Arc::new(MockProvider::new("a", "model-a"));
        a.push_script(vec![ChatDelta::Text("来自默认 a".into())]);
        a.push_script(vec![ChatDelta::Text("来自 planning 的 a".into())]);
        let b = Arc::new(MockProvider::new("b", "model-b"));
        b.push_script(vec![ChatDelta::Text("来自路由 b".into())]);

        let mut reg = ProviderRegistry::new();
        reg.register(a);
        reg.register(b);

        let routes = RoutingConfig {
            coding: Some(codedock_model_gateway::ModelRoute {
                provider: "b".into(),
                model: "model-b".into(),
            }),
            planning: None,
            summarization: None,
        };

        let engine = TurnEngine::new(
            store,
            sessions.clone(),
            Arc::new(reg),
            registry_with_file_read(&std::env::temp_dir()),
            Arc::new(DefaultPolicyEngine),
            routes,
            test_checkpoint_store(),
            std::env::temp_dir(),
            SessionBudgetLimits::default(),
        );
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message_task(info.session_id, "你好", TaskKind::Coding, None)
            .await
            .unwrap();
        assert_eq!(out.text, "来自路由 b", "coding 路由到 provider b");

        let out = engine
            .send_message_task(info.session_id, "你好", TaskKind::Planning, None)
            .await
            .unwrap();
        assert_eq!(
            out.text, "来自默认 a",
            "planning 未配置路由 → 回退默认 provider"
        );

        // 快照中的 model 也应跟随路由（model-b vs model-a）
        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        let providers: Vec<String> = events
            .iter()
            .filter(|e| e.event_type == "model.request.started")
            .map(|e| {
                e.payload["provider"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(providers, vec!["b".to_string(), "a".to_string()]);
    }

    #[tokio::test]
    async fn secrets_are_redacted_and_snapshot_records_audit_hash() {
        // §18.5 / §8.4.1 / §8.4.9 端到端：
        // 工作区文件里的 API Key 进上下文前必须脱敏；
        // 快照事件携带 selection_report 与 final_request_sha256。
        let ws = std::env::temp_dir().join(format!(
            "codedock-turn-secret-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("config.txt"), "api_key=sk-abcdefgh123456789012345")
            .await
            .unwrap();

        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.read".into(),
            arguments: json!({ "path": "config.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("密钥已被脱敏处理。".into())]);

        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&ws),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "读取 config.txt", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::Completed);

        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        let snapshot_event = events
            .iter()
            .rev()
            .find(|e| e.event_type == "context.snapshot.created")
            .expect("第二次模型调用应有快照");
        // §8.4.9 选择报告落事件
        assert!(snapshot_event.payload.get("selection_report").is_some());
        let report = &snapshot_event.payload["selection_report"];
        assert!(report["max_input_tokens"].is_u64());
        assert!(report["used_input_tokens"].as_u64().unwrap() > 0);

        // §8.4.1 final_request_sha256（MockProvider 提供 audit_payload）
        let snapshot: ContextSnapshot =
            serde_json::from_value(snapshot_event.payload["snapshot"].clone()).unwrap();
        let hash = snapshot.final_request_sha256.expect("audit hash 应存在");
        assert!(hash.starts_with("sha256:"));

        // §18.5 脱敏：工具结果文本中的密钥被替换，且记录 Redact 变换
        let tool_results: Vec<&ContextItem> = snapshot
            .items
            .iter()
            .filter(|i| i.kind == "tool_result")
            .collect();
        assert_eq!(tool_results.len(), 1);
        let text = match &tool_results[0].content {
            ContextItemContent::Inline { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("[REDACTED:openai_key]"), "{text}");
        assert!(!text.contains("sk-abcdefgh"));
        assert!(
            tool_results[0]
                .transformations
                .contains(&codedock_protocol::TransformationKind::Redact)
        );

        // 选择报告可见：system/task/user 等条目记录了入选原因
        let considered = report["considered"].as_array().unwrap();
        assert!(
            considered.len() >= 4,
            "system+user+proposal+result 至少 4 条: {}",
            considered.len()
        );
        assert!(
            considered
                .iter()
                .all(|c| c["selected"].is_boolean() && c["reason"].is_string())
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn paused_session_rejects_message() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        let dir = std::env::temp_dir();
        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();
        sessions.pause(info.session_id, None).await.unwrap();

        let err = engine
            .send_message(info.session_id, "你好", None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TurnError::InvalidState(SessionStatus::Paused)
        ));
    }

    #[tokio::test]
    async fn budget_limit_stops_excess_turns() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        let dir = std::env::temp_dir();
        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits {
                max_turns: 1,
                ..SessionBudgetLimits::default()
            },
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        engine
            .send_message(info.session_id, "第一问", None)
            .await
            .unwrap();
        let err = engine
            .send_message(info.session_id, "第二问", None)
            .await
            .unwrap_err();
        assert!(matches!(err, TurnError::BudgetExceeded(_)));
    }

    #[tokio::test]
    async fn model_call_budget_counts_tool_loops() {
        // max_model_calls = 1：一次"提案→执行→再调用"的 Turn 应在第二次模型调用前被拦。
        let dir = std::env::temp_dir().join(format!(
            "codedock-turn-budget-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.txt"), "内容").await.unwrap();

        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "file.read".into(),
            arguments: json!({ "path": "a.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("应该到不了这里".into())]);

        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits {
                max_model_calls: 1,
                ..SessionBudgetLimits::default()
            },
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let err = engine
            .send_message(info.session_id, "读文件", None)
            .await
            .unwrap_err();
        assert!(matches!(err, TurnError::BudgetExceeded(_)));
        let types = durable_types(&engine, info.session_id).await;
        assert!(!types.contains(&"turn.completed".to_string()));
        assert!(types.contains(&"tool.call.completed".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn idempotent_message_does_not_duplicate_turn() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::Text("唯一回复".into())]);
        let dir = std::env::temp_dir();
        let (sessions, engine) = engine_with(
            provider,
            registry_with_file_read(&dir),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let a = engine
            .send_message(info.session_id, "你好", Some("m1".into()))
            .await
            .unwrap();
        let b = engine
            .send_message(info.session_id, "你好", Some("m1".into()))
            .await
            .unwrap();
        assert_eq!(a.turn_id, b.turn_id);
        assert_eq!(a.latest_sequence, b.latest_sequence);
    }

    #[tokio::test]
    async fn provider_error_fails_turn_but_keeps_session_running() {
        let (sessions, engine) = engine_with(
            Arc::new(FailingProvider),
            registry_with_file_read(&std::env::temp_dir()),
            SessionBudgetLimits::default(),
        )
        .await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let err = engine
            .send_message(info.session_id, "你好", None)
            .await
            .unwrap_err();
        assert!(matches!(err, TurnError::Model(_)));

        let types = durable_types(&engine, info.session_id).await;
        assert!(types.contains(&"model.request.failed".to_string()));
        assert!(types.contains(&"turn.failed".to_string()));
        assert!(!types.contains(&"message.completed".to_string()));

        let status = sessions.status(info.session_id).await.unwrap();
        assert_eq!(status.status, SessionStatus::Running);
    }
    #[tokio::test]
    async fn medium_side_effect_escalates_to_approval_in_ask() {
        // §18.1：Medium 有副作用工具在不可信语境下提升一级 → High → 审批。
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::ToolProposal {
            name: "test.write".into(),
            arguments: json!({ "path": "out.txt" }),
        }]);
        provider.push_script(vec![ChatDelta::Text("已按要求中止。".into())]);

        let tools = registry_with(vec![Box::new(TestWriteTool {
            risk: Risk::Medium,
            path_arg: "out.txt".into(),
        }) as Box<dyn ToolExecutor>]);
        let (sessions, engine) = engine_with(provider, tools, SessionBudgetLimits::default()).await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "写个文件", None)
            .await
            .unwrap();
        assert_eq!(out.status, TurnStatus::WaitingApproval);

        let events = engine
            .store
            .load(info.session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        assert!(events.iter().any(|e| {
            e.event_type == "policy.decision_made"
                && e.payload["decision"] == "require_approval"
                && e.payload["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("High")
        }));
    }
}
