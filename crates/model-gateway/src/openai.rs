//! OpenAI-Compatible Provider（§11.2）。
//!
//! 覆盖 OpenAI / Ollama / LM Studio / vLLM 等兼容 `POST /chat/completions`
//! 的服务（§11.2）。约束（§11.4）：
//! - API Key 只经 [`SecretStore`] 获取（键 `provider:<id>`），不写入配置文件；
//! - 流式输出走 SSE；Token 统计为估算值（§18.6）。

use std::sync::Arc;

use async_trait::async_trait;
use codedock_protocol::{ContextItemContent, ContextSnapshot, Role};
use codedock_secret_store::{SecretScope, SecretStore, scoped_key};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    ChatDelta, ModelGatewayError, ModelInfo, ModelProvider, ProviderCapability,
    mock::estimate_tokens,
};

/// OpenAI-Compatible Provider 配置（不含密钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAIProviderConfig {
    /// API 根地址，如 `https://api.openai.com/v1`（末尾 `/` 可省略）。
    pub base_url: String,
    pub model: String,
    pub context_window: u64,
}

pub struct OpenAICompatibleProvider {
    id: String,
    config: OpenAIProviderConfig,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
}

impl OpenAICompatibleProvider {
    pub fn new(
        id: impl Into<String>,
        config: OpenAIProviderConfig,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            id: id.into(),
            config,
            secrets,
            http: reqwest::Client::new(),
        }
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    async fn api_key(&self) -> Result<String, ModelGatewayError> {
        self.secrets
            .get(&scoped_key(SecretScope::Model, &self.id))
            .await
            .map_err(|e| ModelGatewayError::SecretUnavailable(e.to_string()))
    }
}

/// 快照 → OpenAI chat messages（§8.4.10：Snapshot 是发送给 Provider 的真实输入）。
///
/// 映射规则：
/// - `kind == "system_prompt"` → `system`
/// - `kind == "user_message"` → `user`
/// - `role == AssistantHistory` → `assistant`
/// - 其余 Data 条目 → `user`（作为上下文数据注入）
pub(crate) fn snapshot_to_messages(snapshot: &ContextSnapshot) -> Vec<(String, String)> {
    snapshot
        .items
        .iter()
        .filter_map(|item| {
            let text = match &item.content {
                ContextItemContent::Inline { text } => text.clone(),
                ContextItemContent::Blob { .. } => {
                    tracing::debug!(item = %item.title, "跳过 Blob 内容条目（阶段 1 仅内联文本）");
                    return None;
                }
            };
            let role = if item.kind == "system_prompt" {
                "system"
            } else if item.role == Role::AssistantHistory {
                "assistant"
            } else {
                "user"
            };
            Some((role.to_string(), text))
        })
        .collect()
}

/// SSE 数据行解析结果。
#[derive(Debug, PartialEq)]
pub(crate) enum SseLine {
    /// `data: {json}`。
    Data(Value),
    /// `data: [DONE]`。
    Done,
    /// 注释 / 空行 / 其他字段（event:、id: 等）。
    Other,
}

/// 解析一条 SSE 行（§ OpenAI 流式约定）。
pub(crate) fn parse_sse_line(line: &str) -> SseLine {
    let trimmed = line.trim_end_matches(['\r']);
    let Some(data) = trimmed.strip_prefix("data:") else {
        return SseLine::Other;
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        return SseLine::Done;
    }
    match serde_json::from_str(data) {
        Ok(value) => SseLine::Data(value),
        Err(_) => SseLine::Other,
    }
}

/// 从 `/chat/completions` 流式 chunk 提取增量。
pub(crate) fn delta_from_chunk(chunk: &Value) -> Option<ChatDelta> {
    if let Some(content) = chunk
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
    {
        if !content.is_empty() {
            return Some(ChatDelta::Text(content.to_string()));
        }
    }
    // usage chunk（stream_options.include_usage，OpenAI 扩展；兼容服务可能缺失）。
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        return Some(ChatDelta::Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    None
}

#[async_trait]
impl ModelProvider for OpenAICompatibleProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &[ProviderCapability] {
        &[ProviderCapability::Streaming]
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ModelGatewayError> {
        Ok(vec![ModelInfo {
            id: self.config.model.clone(),
            context_window: self.config.context_window,
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
        let api_key = self.api_key().await?;
        let messages: Vec<Value> = snapshot_to_messages(snapshot)
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();

        let response = self
            .http
            .post(self.chat_completions_url())
            .bearer_auth(api_key)
            .json(&json!({
                "model": self.config.model,
                "messages": messages,
                "stream": true,
                "stream_options": { "include_usage": true },
            }))
            .send()
            .await
            .map_err(|e| ModelGatewayError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ModelGatewayError::Provider(
                self.id.clone(),
                format!("HTTP {status}: {}", truncate(&body, 512)),
            ));
        }

        // 后台泵：SSE 行解析 → mpsc channel → ReceiverStream。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ChatDelta, ModelGatewayError>>(64);
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                tokio::select! {
                    biased;
                    chunk = stream.next() => {
                        let Some(bytes) = chunk else { break };
                        match bytes {
                            Ok(bytes) => buf.extend_from_slice(&bytes),
                            Err(err) => {
                                let _ = tx.send(Err(ModelGatewayError::Network(err.to_string()))).await;
                                return;
                            }
                        }
                        // 按行分割（chunk 可能切断行）。
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            if let Ok(line) = String::from_utf8(line) {
                                match parse_sse_line(&line) {
                                    SseLine::Data(chunk) => {
                                        if let Some(delta) = delta_from_chunk(&chunk) {
                                            if tx.send(Ok(delta)).await.is_err() {
                                                return; // 接收端已取消
                                            }
                                        }
                                    }
                                    SseLine::Done => break,
                                    SseLine::Other => {}
                                }
                            }
                        }
                    }
                    else => break,
                }
            }
        });

        Ok(Box::new(ReceiverStream::new(rx)))
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), ModelGatewayError> {
        // 阶段 1：请求粒度取消随 Turn 取消一起实现（§8.3.10）。
        tracing::debug!(request_id, "cancel_request 暂未实现，忽略");
        Ok(())
    }

    async fn health_check(&self) -> Result<(), ModelGatewayError> {
        self.api_key().await.map(|_| ())
    }

    async fn audit_payload(&self, snapshot: &ContextSnapshot) -> Option<serde_json::Value> {
        // §8.4.1：与 stream_chat 完全一致的请求体视图。
        let messages: Vec<Value> = snapshot_to_messages(snapshot)
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();
        Some(json!({
            "model": self.config.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        }))
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_lines_parse() {
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(parse_sse_line(": keep-alive"), SseLine::Other);
        assert_eq!(parse_sse_line("event: ping"), SseLine::Other);
        assert_eq!(parse_sse_line(""), SseLine::Other);

        let raw = r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#;
        match parse_sse_line(raw) {
            SseLine::Data(v) => {
                assert_eq!(delta_from_chunk(&v), Some(ChatDelta::Text("Hi".into())))
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn usage_chunk_maps_to_usage_delta() {
        let chunk = json!({
            "choices": [],
            "usage": { "prompt_tokens": 12, "completion_tokens": 34 }
        });
        assert_eq!(
            delta_from_chunk(&chunk),
            Some(ChatDelta::Usage {
                input_tokens: 12,
                output_tokens: 34,
            })
        );
    }

    #[test]
    fn messages_follow_role_mapping() {
        let build = |kind: &str, role: Role| {
            json!({
                "item_id": codedock_protocol::EventId::generate(),
                "type": kind,
                "role": role,
                "source": { "kind": "message", "uri": "session://local" },
                "title": kind,
                "content": { "storage": "inline", "text": kind },
                "selection": { "reason": "session_memory", "selected_by": "test" },
                "trust": "trusted",
                "classification": "internal"
            })
        };
        let snapshot: ContextSnapshot = serde_json::from_value(json!({
            "schema_version": codedock_protocol::SCHEMA_VERSION,
            "snapshot_id": codedock_protocol::SnapshotId(codedock_protocol::SnapshotId::generate().0),
            "session_id": codedock_protocol::SessionId(codedock_protocol::SessionId::generate().0),
            "model_request_id": codedock_protocol::ModelRequestId(codedock_protocol::ModelRequestId::generate().0),
            "created_at": "2026-01-01T00:00:00Z",
            "model": { "provider": "openai", "model": "gpt-test", "context_window": 1000 },
            "budget": { "max_context_tokens": 1000, "reserved_output_tokens": 100, "available_input_tokens": 900, "used_input_tokens": 0 },
            "items": [
                build("system_prompt", Role::Instruction),
                build("user_message", Role::Instruction),
                build("assistant_message", Role::AssistantHistory),
                build("file", Role::Data)
            ]
        }))
        .unwrap();

        let msgs = snapshot_to_messages(&snapshot);
        assert_eq!(
            msgs,
            vec![
                ("system".into(), "system_prompt".into()),
                ("user".into(), "user_message".into()),
                ("assistant".into(), "assistant_message".into()),
                ("user".into(), "file".into()),
            ]
        );
    }
}
