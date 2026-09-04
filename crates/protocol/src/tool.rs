//! Tool Capability Protocol 类型（§8.3）。
//!
//! 核心原则：Tool 声明能力；Tool Runtime 计算具体权限；Policy Engine 决定是否执行。
//! 模型只能提出 Tool Proposal，不能直接执行。

use crate::capability::Capability;
use crate::ids::{ArtifactId, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// 风险等级（§8.3.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
            Risk::Critical => "critical",
        })
    }
}

/// 副作用声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    None,
    Possible,
    Guaranteed,
}

/// 工具信任级别（§10.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Builtin,
    Signed,
    Unverified,
}

/// 工具提供方。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolProvider {
    /// Runtime 内置工具。
    Builtin { id: String },
    /// WASM 插件工具。
    Plugin { id: String },
    /// Native Sidecar 工具。
    Sidecar { id: String },
}

/// 执行约束（§8.3.2 `execution`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionConstraints {
    pub streaming: bool,
    pub supports_cancel: bool,
    pub supports_dry_run: bool,
    pub idempotent: bool,
    pub default_timeout_ms: u64,
    pub max_timeout_ms: u64,
}

impl Default for ExecutionConstraints {
    fn default() -> Self {
        Self {
            streaming: true,
            supports_cancel: true,
            supports_dry_run: false,
            idempotent: false,
            default_timeout_ms: 120_000,
            max_timeout_ms: 1_800_000,
        }
    }
}

/// Tool Definition（§8.3.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub schema_version: String,
    /// 统一命名 `namespace.action`（§8.3.3），如 `shell.execute`。
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub provider: ToolProvider,
    pub input_schema: Value,
    pub output_schema: Value,
    pub capabilities: Vec<Capability>,
    pub effect: Effect,
    #[serde(default)]
    pub execution: ExecutionConstraints,
    pub trust_level: TrustLevel,
}

impl ToolDefinition {
    pub fn builtin(name: impl Into<String>, capabilities: Vec<Capability>) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION.to_string(),
            name: name.into(),
            version: "1.0.0".to_string(),
            description: String::new(),
            provider: ToolProvider::Builtin {
                id: "runtime".to_string(),
            },
            input_schema: serde_json::json!({ "type": "object" }),
            output_schema: serde_json::json!({ "type": "object" }),
            capabilities,
            effect: Effect::None,
            execution: ExecutionConstraints::default(),
            trust_level: TrustLevel::Builtin,
        }
    }
}

/// 带具体资源范围的权限（§8.3.6：不能只写"允许 fs.read"）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    pub capability: Capability,
    pub resource: String,
}

/// 预期/实际副作用（§8.3.5 / §8.3.8）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SideEffect {
    /// 如 `process.started`、`file.possibly_modified`、`process.completed`。
    #[serde(rename = "type")]
    pub kind: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Tool Call 生命周期（§8.3.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Proposed,
    Validated,
    Preflighted,
    WaitingApproval,
    Approved,
    Rejected,
    Started,
    Completed,
    Failed,
    Cancelled,
}

impl ToolCallStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Cancelled
        )
    }
}

/// Preflight 产出的 ToolExecutionPlan（§8.3.5）。
/// 审批针对 `operation_digest`；参数、目录、权限或命令任何一项变化，旧审批立即失效。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionPlan {
    pub tool_call_id: ToolCallId,
    pub normalized_arguments: Value,
    pub permissions: Vec<Permission>,
    pub risk: Risk,
    pub expected_side_effects: Vec<SideEffect>,
    pub operation_digest: crate::ids::OperationDigest,
    pub preview: String,
}

/// Tool Result（§8.3.8）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub status: ToolResultStatus,
    pub content: Vec<ToolResultContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ToolResultArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actual_side_effects: Vec<SideEffect>,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Failure,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
    Image { mime_type: String, blob_id: String },
    Json { value: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultArtifact {
    pub artifact_id: ArtifactId,
    #[serde(rename = "type")]
    pub kind: String,
    /// 如 `agent://artifacts/<id>`。
    pub uri: String,
    pub mime_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::OperationDigest;

    #[test]
    fn tool_definition_roundtrip() {
        let mut def = ToolDefinition::builtin(
            "shell.execute",
            vec![Capability::ProcessExecute, Capability::FsRead],
        );
        def.effect = Effect::Possible;
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["capabilities"][0], "process_execute");
        let back: ToolDefinition = serde_json::from_value(json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn plan_serializes_with_digest() {
        let plan = ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: serde_json::json!({ "command": "./gradlew test" }),
            permissions: vec![Permission {
                capability: Capability::ProcessExecute,
                resource: "./gradlew test".to_string(),
            }],
            risk: Risk::Medium,
            expected_side_effects: vec![],
            operation_digest: OperationDigest::from_sha256_hex("abc"),
            preview: "Run ./gradlew test in the workspace".to_string(),
        };
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["operation_digest"], "sha256:abc");
        assert_eq!(json["risk"], "medium");
    }
}
