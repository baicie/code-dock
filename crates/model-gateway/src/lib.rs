//! Model Gateway：Provider 适配、路由、重试、流式输出、成本统计（§6 / §11）。
//!
//! 关键约束（§11.4）：
//! - Provider 必须声明 Capability，不能假设全部模型都一样（§18.6）；
//! - Fallback 不得静默跨越隐私级别或成本上限；
//! - Provider 不能读取其他 Provider 的密钥；
//! - API Key 只存系统 Keychain（见 `codedock-secret-store`）。

pub mod mock;
pub mod openai;
pub mod registry;

pub use mock::MockProvider;
pub use openai::{OpenAICompatibleProvider, OpenAIProviderConfig};
pub use registry::ProviderRegistry;

use async_trait::async_trait;
use codedock_protocol::{Classification, ContextSnapshot};
use futures::Stream;
use thiserror::Error;

/// Provider 能力声明（§11.1 / §18.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    ToolCalling,
    StructuredOutput,
    Streaming,
    ImageInput,
    PromptCache,
    ReasoningControl,
}

/// 统一 Provider 接口（§11.1）。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider 标识（路由配置中的 `provider` 字段）。
    fn id(&self) -> &str;

    fn capabilities(&self) -> &[ProviderCapability];

    /// 模型清单。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ModelGatewayError>;

    /// Token 计数（估算即可，Provider 差异大，§18.6）。
    async fn count_tokens(&self, text: &str) -> Result<u64, ModelGatewayError>;

    /// 流式对话。输入是最终确认的 Context Snapshot（§8.4.10 一对一关系）。
    async fn stream_chat(
        &self,
        snapshot: &ContextSnapshot,
    ) -> Result<
        Box<dyn Stream<Item = Result<ChatDelta, ModelGatewayError>> + Send + Unpin>,
        ModelGatewayError,
    >;

    /// 取消进行中的请求。
    async fn cancel_request(&self, request_id: &str) -> Result<(), ModelGatewayError>;

    /// 健康检查。
    async fn health_check(&self) -> Result<(), ModelGatewayError>;
}

/// 模型信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: u64,
    pub max_output: u64,
}

/// 流式输出片段（`message.delta` 的来源）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChatDelta {
    Text(String),
    /// 模型提出的 Tool Proposal（不能直接执行，§8.3.1）。
    ToolProposal {
        name: String,
        arguments: serde_json::Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
}

#[derive(Debug, Error, PartialEq)]
pub enum ModelGatewayError {
    #[error("provider {0}: {1}")]
    Provider(String, String),
    #[error("模型请求被取消")]
    Cancelled,
    #[error("超出费用/Token 预算上限（§18.3）")]
    BudgetExceeded,
    #[error("隐私策略阻止该请求（数据分类 {0:?} 不允许发送给此 Provider）")]
    PrivacyBlocked(Classification),
    #[error("密钥不可用: {0}")]
    SecretUnavailable(String),
    #[error("网络错误: {0}")]
    Network(String),
}

/// 模型路由（§11.3）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelRoute {
    pub provider: String,
    pub model: String,
}

/// 按任务类型的路由表（§11.3）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub planning: Option<ModelRoute>,
    #[serde(default)]
    pub coding: Option<ModelRoute>,
    #[serde(default)]
    pub summarization: Option<ModelRoute>,
}

/// Session 级预算上限（§18.3：无限循环和费用失控防护）。
///
/// 字段级 serde 默认值允许配置文件只覆盖关心的项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionBudgetLimits {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_max_model_calls")]
    pub max_model_calls: u32,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default = "default_max_duration_ms")]
    pub max_duration_ms: u64,
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u64,
}

fn default_max_turns() -> u32 {
    200
}

fn default_max_model_calls() -> u32 {
    400
}

fn default_max_tool_calls() -> u32 {
    800
}

fn default_max_duration_ms() -> u64 {
    1_800_000
}

fn default_max_total_tokens() -> u64 {
    4_000_000
}

impl Default for SessionBudgetLimits {
    /// 阶段 1 保守默认值；生产部署应由配置显式给出（§18.3）。
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            max_model_calls: default_max_model_calls(),
            max_tool_calls: default_max_tool_calls(),
            max_duration_ms: default_max_duration_ms(),
            max_total_tokens: default_max_total_tokens(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_config_parses_yaml_shape_as_json() {
        let cfg: RoutingConfig = serde_json::from_value(serde_json::json!({
            "planning": { "provider": "cloud-a", "model": "reasoning-model" },
            "coding": { "provider": "local", "model": "code-model" },
            "summarization": { "provider": "local", "model": "small-model" }
        }))
        .unwrap();
        assert_eq!(cfg.coding.as_ref().unwrap().model, "code-model");
    }
}
