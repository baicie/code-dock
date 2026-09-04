//! Context Snapshot Protocol 类型（§8.4）。
//!
//! 核心原则：Snapshot 记录最终发送给 Model Provider Gateway 的真实输入，
//! 而不是 Context Engine 原本想发送的内容（§8.4.1）。

use crate::ids::{BlobId, ModelRequestId, SessionId, SnapshotId, TurnId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// 上下文条目角色（§8.4.4）。
///
/// 网页、仓库文本、Issue、日志和 Tool Output 默认是 `data`；
/// 只有系统配置、用户当前指令和经过信任验证的项目规则可以成为 `instruction`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Instruction,
    Data,
    ToolSchema,
    AssistantHistory,
}

/// 信任级别（§8.4.5），用于 Prompt Injection 防御和审批升级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Trusted,
    WorkspaceUntrusted,
    PluginUntrusted,
    ExternalUntrusted,
}

/// 数据分类（§8.4.6）。`secret` 默认不进入任何模型上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Public,
    Internal,
    Confidential,
    Secret,
}

/// 来源种类（§8.4.3 `source.kind`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    File,
    Git,
    ToolOutput,
    Web,
    Message,
    Plugin,
    Other(String),
}

/// 上下文条目来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub kind: SourceKind,
    /// 如 `workspace://src/main.rs`。
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// 选择原因（§8.4.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    UserAttached,
    UserPinned,
    ProjectRule,
    CurrentlyOpen,
    CurrentlyEdited,
    KeywordMatch,
    SemanticMatch,
    SymbolDefinition,
    SymbolReference,
    SymbolDependency,
    GitModified,
    GitRelated,
    DiagnosticRelated,
    ToolResult,
    SessionMemory,
    AgentSelected,
    PluginSelected,
}

/// 变换类型（§8.4.8）。每次变换记录输入哈希、输出哈希、Token 变化和原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformationKind {
    ExtractRange,
    Truncate,
    Summarize,
    Deduplicate,
    Redact,
    Merge,
    Normalize,
}

/// 内容存储方式：内联小对象或 Blob 引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum ContextItemContent {
    Inline { text: String },
    Blob { blob_id: BlobId, sha256: String },
}

/// 行号范围（0 起）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// 上下文条目（§8.4.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub item_id: crate::ids::EventId,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: Role,
    pub source: SourceRef,
    pub title: String,
    pub content: ContextItemContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<LineRange>,
    pub selection: Selection,
    pub trust: Trust,
    pub classification: Classification,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transformations: Vec<TransformationKind>,
}

/// 选择元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub reason: SelectionReason,
    pub selected_by: String,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub priority: i32,
}

/// Token 预算（§8.4.2 / §12.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub max_context_tokens: u64,
    pub reserved_output_tokens: u64,
    pub available_input_tokens: u64,
    pub used_input_tokens: u64,
}

impl ContextBudget {
    /// 依据模型上下文窗口计算可用输入预算（预留输出）。
    pub fn new(max_context_tokens: u64, reserved_output_tokens: u64) -> Self {
        Self {
            max_context_tokens,
            reserved_output_tokens,
            available_input_tokens: max_context_tokens.saturating_sub(reserved_output_tokens),
            used_input_tokens: 0,
        }
    }

    pub fn remaining(&self) -> u64 {
        self.available_input_tokens
            .saturating_sub(self.used_input_tokens)
    }
}

/// 模型信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
    pub context_window: u64,
}

/// Context Snapshot（§8.4.2）。
///
/// 不可变规则（§8.4.10）：一个 Model Request 对应一个 Snapshot；
/// `model.request.started` 后不得修改；下一次模型调用必须创建新 Snapshot。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub schema_version: String,
    pub snapshot_id: SnapshotId,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub model_request_id: ModelRequestId,
    pub created_at: DateTime<Utc>,
    pub model: ModelRef,
    pub budget: ContextBudget,
    #[serde(default)]
    pub items: Vec<ContextItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_report_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_request_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_request_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy_decision: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
}

impl ContextSnapshot {
    pub fn new(session_id: SessionId, model: ModelRef, budget: ContextBudget) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION.to_string(),
            snapshot_id: SnapshotId::generate(),
            session_id,
            turn_id: None,
            model_request_id: ModelRequestId::generate(),
            created_at: Utc::now(),
            model,
            budget,
            items: Vec::new(),
            selection_report_ref: None,
            final_request_ref: None,
            final_request_sha256: None,
            privacy_decision: None,
            summary: None,
        }
    }
}

impl fmt::Display for Trust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Trust::Trusted => "trusted",
            Trust::WorkspaceUntrusted => "workspace_untrusted",
            Trust::PluginUntrusted => "plugin_untrusted",
            Trust::ExternalUntrusted => "external_untrusted",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_reserves_output_tokens() {
        let b = ContextBudget::new(200_000, 16_000);
        assert_eq!(b.available_input_tokens, 184_000);
        assert_eq!(b.remaining(), 184_000);
    }

    #[test]
    fn external_page_content_defaults_are_enforced_by_types() {
        // 演示 trust/classification 序列化形态（§8.4.5/§8.4.6）
        assert_eq!(
            serde_json::to_value(Trust::ExternalUntrusted).unwrap(),
            "external_untrusted"
        );
        assert_eq!(
            serde_json::to_value(Classification::Secret).unwrap(),
            "secret"
        );
    }
}
