//! 事件总线：`append` 成功即向订阅者广播（§8.2.5 实时推送的基础）。
//!
//! [`BroadcastingStore`] 包装任意 [`EventStore`]：写入成功后把带 sequence 的
//! 信封发给所有订阅者（durable 与 transient 都实时推送——流式 delta 的意义
//! 就在实时性；补发仍只含 durable，见 rpc `session.subscribe`）。
//!
//! 订阅端使用 `tokio::sync::broadcast`：落后超过缓冲的订阅者收到 `Lagged`，
//! 客户端应以 `session.events` + `after_sequence` 重新对齐（§8.2.5 断线恢复）。

use std::sync::Arc;

use async_trait::async_trait;
use codedock_event_store::EventStore;
use codedock_protocol::EventEnvelope;
use tokio::sync::broadcast;

/// 事件总线：广播 sender 的持有者。
pub struct EventHub {
    tx: broadcast::Sender<EventEnvelope>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    /// 订阅实时事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// 用广播包装一个底层 Event Store（写入成功即广播）。
    pub fn wrap_store(&self, inner: Arc<dyn EventStore>) -> Arc<dyn EventStore> {
        Arc::new(BroadcastingStore {
            inner,
            tx: self.tx.clone(),
        })
    }
}

/// 写入成功后广播的 Event Store 装饰器。
struct BroadcastingStore {
    inner: Arc<dyn EventStore>,
    tx: broadcast::Sender<EventEnvelope>,
}

#[async_trait]
impl EventStore for BroadcastingStore {
    async fn append(
        &self,
        mut envelope: EventEnvelope,
    ) -> Result<u64, codedock_event_store::EventStoreError> {
        let sequence = self.inner.append(envelope.clone()).await?;
        // append 内部会覆盖 sequence；回填后广播，保证订阅者看到的与存储一致。
        envelope.sequence = sequence;
        // 无订阅者时 send 返回 Err——属正常情况，忽略。
        let _ = self.tx.send(envelope);
        Ok(sequence)
    }

    async fn load(
        &self,
        session_id: codedock_protocol::SessionId,
        after_sequence: u64,
        limit: usize,
        durable_only: bool,
    ) -> Result<Vec<EventEnvelope>, codedock_event_store::EventStoreError> {
        self.inner
            .load(session_id, after_sequence, limit, durable_only)
            .await
    }

    async fn latest_sequence(
        &self,
        session_id: codedock_protocol::SessionId,
    ) -> Result<u64, codedock_event_store::EventStoreError> {
        self.inner.latest_sequence(session_id).await
    }

    async fn load_all_sessions(
        &self,
    ) -> Result<
        Vec<(codedock_protocol::SessionId, Vec<EventEnvelope>)>,
        codedock_event_store::EventStoreError,
    > {
        self.inner.load_all_sessions().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_event_store::InMemoryEventStore;
    use codedock_protocol::{Actor, Durability, SessionId};

    fn draft(session: SessionId, event_type: &str) -> EventEnvelope {
        EventEnvelope::draft(
            session,
            None,
            event_type,
            Durability::Durable,
            Actor::system("test"),
            serde_json::json!({}),
        )
    }

    #[tokio::test]
    async fn append_broadcasts_envelope_with_sequence() {
        let hub = Arc::new(EventHub::new());
        let mut rx = hub.subscribe();
        let store = hub.wrap_store(Arc::new(InMemoryEventStore::new()));
        let s = SessionId::generate();

        store.append(draft(s, "session.created")).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_type, "session.created");
        assert_eq!(received.sequence, 1, "广播的信封携带分配后的 sequence");
    }

    #[tokio::test]
    async fn no_subscribers_is_not_an_error() {
        let hub = Arc::new(EventHub::new());
        let store = hub.wrap_store(Arc::new(InMemoryEventStore::new()));
        let s = SessionId::generate();
        let seq = store.append(draft(s, "turn.started")).await.unwrap();
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn load_delegates_to_inner_store() {
        let hub = Arc::new(EventHub::new());
        let _rx = hub.subscribe();
        let store = hub.wrap_store(Arc::new(InMemoryEventStore::new()));
        let s = SessionId::generate();
        store.append(draft(s, "a")).await.unwrap();
        store.append(draft(s, "b")).await.unwrap();
        let events = store.load(s, 0, 100, true).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(store.latest_sequence(s).await.unwrap(), 2);
    }
}
