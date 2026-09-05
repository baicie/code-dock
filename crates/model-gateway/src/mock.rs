//! Mock Provider（开发 / 测试用，无网络）。
//!
//! 回放注入的脚本片段；脚本用尽后回声快照中最后一条用户消息，
//! 保证"纯对话 Turn 闭环"可脱离外部模型验证。

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use codedock_protocol::{ContextItemContent, ContextSnapshot};
use futures::Stream;

use crate::{ChatDelta, ModelGatewayError, ModelInfo, ModelProvider, ProviderCapability};

/// 可编程的测试 Provider：按序回放 `script`，用尽后回声用户消息。
pub struct MockProvider {
    id: String,
    model: String,
    script: Mutex<VecDeque<Vec<ChatDelta>>>,
}

impl MockProvider {
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            script: Mutex::new(VecDeque::new()),
        }
    }

    /// 注入一段按序回放的输出（每次 stream_chat 消耗一段）。
    pub fn push_script(&self, deltas: Vec<ChatDelta>) {
        self.script
            .lock()
            .expect("script poisoned")
            .push_back(deltas);
    }
}

/// 提取快照中最后一条用户输入文本（用于回声）。
fn last_user_text(snapshot: &ContextSnapshot) -> String {
    snapshot
        .items
        .iter()
        .rev()
        .find(|item| matches!(item.kind.as_str(), "user_message" | "message"))
        .and_then(|item| match &item.content {
            ContextItemContent::Inline { text } => Some(text.clone()),
            ContextItemContent::Blob { .. } => None,
        })
        .unwrap_or_default()
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &[ProviderCapability] {
        &[ProviderCapability::Streaming]
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ModelGatewayError> {
        Ok(vec![ModelInfo {
            id: self.model.clone(),
            context_window: 128_000,
            max_output: 4_096,
        }])
    }

    async fn count_tokens(&self, text: &str) -> Result<u64, ModelGatewayError> {
        Ok(estimate_tokens(text))
    }

    async fn stream_chat(
        &self,
        snapshot: &ContextSnapshot,
    ) -> Result<
        Box<dyn Stream<Item = Result<ChatDelta, ModelGatewayError>> + Send + Unpin>,
        ModelGatewayError,
    > {
        let segment = self
            .script
            .lock()
            .expect("script poisoned")
            .pop_front()
            .unwrap_or_else(|| {
                vec![ChatDelta::Text(format!(
                    "echo: {}",
                    last_user_text(snapshot)
                ))]
            });
        let stream = futures::stream::iter(segment.into_iter().map(Ok));
        Ok(Box::new(stream))
    }

    async fn cancel_request(&self, _request_id: &str) -> Result<(), ModelGatewayError> {
        Ok(())
    }

    async fn health_check(&self) -> Result<(), ModelGatewayError> {
        Ok(())
    }
}

/// 粗略 Token 估算（§18.6：Provider 差异大，估算即可）。
pub(crate) fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{
        Classification, Role, Selection, SelectionReason, SourceKind, SourceRef, Trust,
    };

    fn item(kind: &str, role: Role, text: &str) -> codedock_protocol::ContextItem {
        codedock_protocol::ContextItem {
            item_id: codedock_protocol::EventId::generate(),
            kind: kind.into(),
            role,
            source: SourceRef {
                kind: SourceKind::Message,
                uri: "session://local".into(),
                revision: None,
            },
            title: kind.into(),
            content: ContextItemContent::Inline { text: text.into() },
            range: None,
            selection: Selection {
                reason: SelectionReason::SessionMemory,
                selected_by: "test".into(),
                score: 0.0,
                priority: 0,
            },
            trust: Trust::Trusted,
            classification: Classification::Internal,
            tokens: 0,
            transformations: Vec::new(),
        }
    }

    fn snapshot(items: Vec<codedock_protocol::ContextItem>) -> ContextSnapshot {
        let mut s = ContextSnapshot::new(
            codedock_protocol::SessionId::generate(),
            codedock_protocol::ModelRef {
                provider: "mock".into(),
                model: "mock-model".into(),
                context_window: 128_000,
            },
            codedock_protocol::ContextBudget::new(128_000, 4_096),
        );
        s.items = items;
        s
    }

    #[tokio::test]
    async fn echo_falls_back_after_script_exhausted() {
        let provider = MockProvider::new("mock", "mock-model");
        provider.push_script(vec![ChatDelta::Text("脚本输出".into())]);

        let snap = snapshot(vec![item("user_message", Role::Instruction, "你好")]);
        let stream = provider.stream_chat(&snap).await.unwrap();
        let deltas: Vec<Result<ChatDelta, ModelGatewayError>> =
            futures::StreamExt::collect(stream).await;
        assert_eq!(deltas, vec![Ok(ChatDelta::Text("脚本输出".into()))]);

        // 脚本用尽 → 回声用户消息
        let stream = provider.stream_chat(&snap).await.unwrap();
        let deltas: Vec<Result<ChatDelta, ModelGatewayError>> =
            futures::StreamExt::collect(stream).await;
        assert_eq!(deltas, vec![Ok(ChatDelta::Text("echo: 你好".into()))]);
    }

    #[test]
    fn token_estimate_is_quarter_of_chars() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }
}
