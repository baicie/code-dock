//! Turn 执行引擎（§7 执行流程 / §8.2.4 事件协议）。
//!
//! 一条用户消息 = 一个 Turn，事件编排固定为：
//!
//! ```text
//! turn.started（durable）
//!   → message.created（用户消息，durable）
//!   → context.snapshot.created（§8.4.10：一次模型调用对应一个 Snapshot）
//!   → model.request.started（durable）
//!   → message.delta ...（transient，不保证补发）
//!   → message.completed（助手最终消息，durable，幂等键锚点）
//!   → model.request.completed（durable，含 Token 用量）
//!   → turn.completed（durable）
//! ```
//!
//! 预算防护（§18.3）：每轮开始前从 Durable Event 统计已用 Turn 数与累计
//! Token，达到上限直接拒绝；Provider 错误只失败当前 Turn，不污染 Session 状态。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use codedock_event_store::EventStore;
use codedock_model_gateway::{ChatDelta, ModelProvider, ProviderRegistry, SessionBudgetLimits};
use codedock_protocol::{
    Actor, Classification, ContextBudget, ContextItem, ContextItemContent, ContextSnapshot,
    Durability, EventEnvelope, ModelRef, Role, Selection, SelectionReason, SessionId,
    SessionStatus, SourceKind, SourceRef, Trust, TurnId,
};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::{SessionError, SessionManager};

/// 预算统计扫描的事件上限（阶段 1 纯对话事件量远低于此）。
const EVENT_SCAN_LIMIT: usize = 100_000;

const SYSTEM_PROMPT: &str = "你是 CodeDock，一个 Local-first 的 Coding Agent Runtime。\
当前为纯对话阶段：回答简洁、准确；不要编造工具或文件操作能力。";

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
    #[error("未配置可用的模型 Provider")]
    NoProvider,
    #[error("模型调用失败: {0}")]
    Model(String),
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

/// 一个 Turn 的执行结果（客户端可见投影）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latest_sequence: u64,
}

/// Turn 执行引擎：会话状态检查、预算、Snapshot 组装与模型事件编排。
pub struct TurnEngine {
    store: Arc<dyn EventStore>,
    sessions: Arc<dyn SessionManager>,
    providers: Arc<ProviderRegistry>,
    limits: SessionBudgetLimits,
    /// 幂等缓存：command key → 已完成 Turn 的结果（§8.2.6）。
    done: Mutex<HashMap<String, TurnOutcome>>,
    /// 正在执行 Turn 的会话，防止并发消息交错写入事件流。
    busy: Mutex<HashSet<SessionId>>,
}

impl TurnEngine {
    pub fn new(
        store: Arc<dyn EventStore>,
        sessions: Arc<dyn SessionManager>,
        providers: Arc<ProviderRegistry>,
        limits: SessionBudgetLimits,
    ) -> Self {
        Self {
            store,
            sessions,
            providers,
            limits,
            done: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
        }
    }

    /// 从 Event Store 重建幂等缓存（§17.1：锚点在 assistant `message.completed`）。
    pub async fn restore(
        store: Arc<dyn EventStore>,
        sessions: Arc<dyn SessionManager>,
        providers: Arc<ProviderRegistry>,
        limits: SessionBudgetLimits,
    ) -> Result<Self, TurnError> {
        let engine = Self::new(store, sessions, providers, limits);
        let all = engine
            .store
            .load_all_sessions()
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))?;
        for (session_id, events) in all {
            for event in events {
                if event.event_type != "message.completed" {
                    continue;
                }
                let Some(key) = event.payload.get("command_key").and_then(Value::as_str) else {
                    continue;
                };
                let outcome = TurnOutcome {
                    turn_id: event.turn_id.unwrap_or_else(TurnId::generate),
                    session_id,
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
        Ok(engine)
    }

    /// 发送一条用户消息并同步等待本轮完成。
    ///
    /// 流式增量以 transient `message.delta` 落库；客户端实时订阅（阶段 1 TODO）
    /// 之前，可通过 `session.events(durable_only=false)` 近似观察。
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
        let result = self
            .run_turn(session_id, info.task, text.into(), idempotency_key)
            .await;
        self.busy
            .lock()
            .expect("busy set poisoned")
            .remove(&session_id);
        result
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        task: Option<String>,
        text: String,
        idempotency_key: Option<String>,
    ) -> Result<TurnOutcome, TurnError> {
        self.check_budget(session_id).await?;
        let provider = self
            .providers
            .default_provider()
            .ok_or(TurnError::NoProvider)?;

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

        // 当前用户消息已落库，快照组装统一从事件流读取（含本轮输入）。
        let snapshot = self
            .assemble_snapshot(&provider, session_id, task.as_deref())
            .await?;
        let model_request_id = snapshot.model_request_id;
        let used_input_tokens = snapshot.budget.used_input_tokens;
        self.append(
            session_id,
            turn_id,
            "context.snapshot.created",
            Durability::Durable,
            json!({ "snapshot_id": snapshot.snapshot_id, "snapshot": &snapshot }),
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
                Ok(ChatDelta::ToolProposal { .. }) => {
                    tracing::debug!(turn = %turn_id, "收到 Tool Proposal（阶段 2 接入 Tool Runtime）");
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
        Ok(outcome)
    }

    /// 预算检查（§18.3）：从 Durable Event 统计已用量，任一上限触达即拒绝。
    async fn check_budget(&self, session_id: SessionId) -> Result<(), TurnError> {
        let events = self
            .store
            .load(session_id, 0, EVENT_SCAN_LIMIT, true)
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))?;
        let mut turns = 0u32;
        let mut model_calls = 0u32;
        let mut tokens = 0u64;
        for event in events {
            match event.event_type.as_str() {
                "turn.started" => turns += 1,
                "model.request.completed" => {
                    model_calls += 1;
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
        if tokens >= self.limits.max_total_tokens {
            return Err(TurnError::BudgetExceeded(format!(
                "累计 {tokens} tokens ≥ max_total_tokens {}",
                self.limits.max_total_tokens
            )));
        }
        Ok(())
    }

    /// 组装 Context Snapshot（§8.4）：system prompt + 任务 + 全部历史消息（含本轮）。
    async fn assemble_snapshot(
        &self,
        provider: &Arc<dyn ModelProvider>,
        session_id: SessionId,
        task: Option<&str>,
    ) -> Result<ContextSnapshot, TurnError> {
        let models = provider
            .list_models()
            .await
            .map_err(|e| TurnError::Model(e.to_string()))?;
        let model = models
            .first()
            .ok_or_else(|| TurnError::Model("Provider 未提供任何模型".to_string()))?;
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
        let mut history: Vec<(bool, String)> = Vec::new();
        for event in events {
            let (is_user, expected) = match event.event_type.as_str() {
                "message.created" => (true, "user"),
                "message.completed" => (false, "assistant"),
                _ => continue,
            };
            let role = event.payload.get("role").and_then(Value::as_str);
            let text = event.payload.get("text").and_then(Value::as_str);
            if role == Some(expected) {
                if let Some(text) = text {
                    history.push((is_user, text.to_string()));
                }
            }
        }

        let mut items = Vec::new();
        items.push(
            self.item(
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
            items.push(
                self.item(
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
        let latest = history.len().saturating_sub(1);
        for (idx, (is_user, text)) in history.iter().enumerate() {
            let (kind, role, reason, priority) = if *is_user {
                (
                    "user_message",
                    Role::Instruction,
                    if idx == latest {
                        SelectionReason::UserAttached
                    } else {
                        SelectionReason::SessionMemory
                    },
                    if idx == latest { 100 } else { 50 },
                )
            } else {
                (
                    "assistant_message",
                    Role::AssistantHistory,
                    SelectionReason::SessionMemory,
                    50,
                )
            };
            items.push(
                self.item(
                    kind,
                    role,
                    reason,
                    priority,
                    SourceKind::Message,
                    format!("session://{session_id}"),
                    text.clone(),
                    provider,
                )
                .await?,
            );
        }

        let mut snapshot = ContextSnapshot::new(
            session_id,
            model_ref,
            ContextBudget::new(model.context_window, model.max_output),
        );
        snapshot.items = items;
        snapshot.budget.used_input_tokens = snapshot.items.iter().map(|i| i.tokens).sum();
        Ok(snapshot)
    }

    /// 构造一个 Snapshot 条目并估算 Token（§8.4.3）。
    #[allow(clippy::too_many_arguments)]
    async fn item(
        &self,
        kind: &str,
        role: Role,
        reason: SelectionReason,
        priority: i32,
        source_kind: SourceKind,
        uri: String,
        text: String,
        provider: &Arc<dyn ModelProvider>,
    ) -> Result<ContextItem, TurnError> {
        let tokens = provider.count_tokens(&text).await.unwrap_or_default();
        Ok(ContextItem {
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
                selected_by: "turn_engine".into(),
                score: 1.0,
                priority,
            },
            trust: Trust::Trusted,
            classification: Classification::Internal,
            tokens,
            transformations: Vec::new(),
        })
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

    /// 追加一条事件；sequence 由 Event Store 分配（§8.2.3）。
    async fn append(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        event_type: &str,
        durability: Durability,
        payload: Value,
    ) -> Result<u64, TurnError> {
        let envelope = EventEnvelope::draft(
            session_id,
            Some(turn_id),
            event_type,
            durability,
            Actor::agent("primary"),
            payload,
        );
        self.store
            .append(envelope)
            .await
            .map_err(|e| TurnError::EventStore(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventSourcedSessionManager;
    use codedock_event_store::InMemoryEventStore;
    use codedock_model_gateway::{MockProvider, ModelGatewayError};

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

    fn registry(provider: Arc<dyn ModelProvider>) -> Arc<ProviderRegistry> {
        let mut reg = ProviderRegistry::new();
        reg.register(provider);
        Arc::new(reg)
    }

    async fn engine_with(
        provider: Arc<dyn ModelProvider>,
        limits: SessionBudgetLimits,
    ) -> (Arc<dyn SessionManager>, TurnEngine) {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let sessions: Arc<dyn SessionManager> =
            Arc::new(EventSourcedSessionManager::new(store.clone()));
        let engine = TurnEngine::new(store, sessions.clone(), registry(provider), limits);
        (sessions, engine)
    }

    async fn durable_types(engine: &TurnEngine, sid: SessionId) -> Vec<String> {
        let store_events = engine
            .store
            .load(sid, 0, EVENT_SCAN_LIMIT, true)
            .await
            .unwrap();
        store_events.iter().map(|e| e.event_type.clone()).collect()
    }

    #[tokio::test]
    async fn happy_path_emits_full_event_chain() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::Text("你好，我是 mock".into())]);
        let (sessions, engine) = engine_with(provider, SessionBudgetLimits::default()).await;
        let info = sessions
            .create(codedock_protocol::SessionMode::Ask, None, None)
            .await
            .unwrap();

        let out = engine
            .send_message(info.session_id, "你好", None)
            .await
            .unwrap();
        assert_eq!(out.text, "你好，我是 mock");
        assert_eq!(out.session_id, info.session_id);
        assert!(out.output_tokens > 0);

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
                "message.completed",
                "model.request.completed",
                "turn.completed",
            ]
        );
        // delta 是 transient，不在 durable 重放里
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
        // 所有事件携带 turn_id
        let turn_events = all
            .iter()
            .filter(|e| e.event_type.starts_with("turn.") || e.event_type.starts_with("message."))
            .collect::<Vec<_>>();
        assert!(turn_events.iter().all(|e| e.turn_id.is_some()));
    }

    #[tokio::test]
    async fn second_turn_sees_history_in_snapshot() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::Text("第一轮回复".into())]);
        provider.push_script(vec![ChatDelta::Text("第二轮回复".into())]);
        let (sessions, engine) = engine_with(provider, SessionBudgetLimits::default()).await;
        let info = sessions
            .create(
                codedock_protocol::SessionMode::Ask,
                Some("演示任务".into()),
                None,
            )
            .await
            .unwrap();

        engine
            .send_message(info.session_id, "第一问", None)
            .await
            .unwrap();
        engine
            .send_message(info.session_id, "第二问", None)
            .await
            .unwrap();

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

        let kinds: Vec<&str> = snapshots[1].items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "system_prompt",
                "task",
                "user_message",
                "assistant_message",
                "user_message"
            ]
        );
        assert!(snapshots[1].budget.used_input_tokens > 0);
    }

    #[tokio::test]
    async fn paused_session_rejects_message() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        let (sessions, engine) = engine_with(provider, SessionBudgetLimits::default()).await;
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
        let (sessions, engine) = engine_with(
            provider,
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
    async fn idempotent_message_does_not_duplicate_turn() {
        let provider = Arc::new(MockProvider::new("mock", "mock-model"));
        provider.push_script(vec![ChatDelta::Text("唯一回复".into())]);
        let (sessions, engine) = engine_with(provider, SessionBudgetLimits::default()).await;
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

        // 幂等缓存跨重启恢复（§17.1）
        let restored = TurnEngine::restore(
            engine.store.clone(),
            sessions.clone(),
            registry(Arc::new(MockProvider::new("mock", "mock-model"))),
            SessionBudgetLimits::default(),
        )
        .await
        .unwrap();
        let again = restored
            .send_message(info.session_id, "你好", Some("m1".into()))
            .await
            .unwrap();
        assert_eq!(again.text, "唯一回复");
        assert_eq!(again.turn_id, a.turn_id);
    }

    #[tokio::test]
    async fn provider_error_fails_turn_but_keeps_session_running() {
        let (sessions, engine) =
            engine_with(Arc::new(FailingProvider), SessionBudgetLimits::default()).await;
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
}
