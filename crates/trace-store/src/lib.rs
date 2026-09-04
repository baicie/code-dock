//! Trace Store：Span、耗时、Token、错误、重试和成本（§6）。
//!
//! Trace 支撑 DevTools 的 Trace 面板：从用户指令到模型、工具和变更的完整
//! 时间线（§14.1），并支撑 §18.3 的费用失控检测。
//!
//! TODO(阶段3)：与 Event Store 关联（correlation_id / causation_event_id）。

use codedock_protocol::SessionId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("trace 写入失败: {0}")]
    WriteFailed(String),
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Span 种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    UserCommand,
    Turn,
    ModelRequest,
    ToolCall,
    PolicyDecision,
    Checkpoint,
    RemoteCommand,
}

/// 一个 Span 记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanRecord {
    pub span_id: String,
    pub session_id: SessionId,
    pub kind: SpanKind,
    pub name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub parent_span_id: Option<String>,
    /// 关联一次用户任务或完整执行链（§8.2.3）。
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// Token 与费用（§11.4：每个 Session 有 Token 和金额预算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 微单位成本（如 micro-USD），避免浮点。
    pub cost_micros: u64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn merge(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_micros += other.cost_micros;
    }
}

/// Trace 存储抽象。
#[async_trait::async_trait]
pub trait TraceStore: Send + Sync {
    async fn append(&self, span: SpanRecord) -> Result<(), TraceError>;

    /// Session 内按时间线查询。
    async fn timeline(&self, session_id: SessionId) -> Result<Vec<SpanRecord>, TraceError>;

    /// Session 累计用量（供 §18.3 预算暂停判断）。
    async fn session_usage(&self, session_id: SessionId) -> Result<TokenUsage, TraceError>;
}
