//! Append-only Event Store（§17.1）。
//!
//! 重要事实 Append-only；`sessions`、`tool_calls` 等表是当前状态 Projection，可以重建。
//! - `sequence` 由 Runtime 在单个 Session 内严格单调递增分配（§8.2.3）；
//! - Durable Event 确认写入后不可修改，断线恢复时先补发 Durable Event（§8.2.5）；
//! - 事件使用事务写入（§20.1）。
//!
//! 持久化实现见 [`sqlite::SqliteEventStore`]（WAL + `schema_migrations` 显式版本迁移，§18.9）。

pub mod sqlite;

pub use sqlite::SqliteEventStore;

use async_trait::async_trait;
use codedock_protocol::{Durability, EventEnvelope, SessionId};
use std::collections::HashMap;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventStoreError {
    #[error("session {0} 不存在")]
    SessionNotFound(SessionId),
    #[error("事件写入失败: {0}")]
    WriteFailed(String),
    #[error("数据库操作失败: {0}")]
    Db(String),
    #[error("数据库连接失败: {0}")]
    Connect(String),
    #[error("数据库迁移失败 (v{version}): {message}")]
    Migration { version: i64, message: String },
}

/// 事件存储抽象。
#[async_trait]
pub trait EventStore: Send + Sync {
    /// 追加事件并分配 `sequence`，返回最终 sequence。
    async fn append(&self, envelope: EventEnvelope) -> Result<u64, EventStoreError>;

    /// 从 `after_sequence` 之后读取事件（用于断线恢复与 Replay）。
    ///
    /// `durable_only = true` 时只补发 Durable Event（§8.2.5）。
    async fn load(
        &self,
        session_id: SessionId,
        after_sequence: u64,
        limit: usize,
        durable_only: bool,
    ) -> Result<Vec<EventEnvelope>, EventStoreError>;

    /// Session 当前最新 sequence；无事件返回 0。
    async fn latest_sequence(&self, session_id: SessionId) -> Result<u64, EventStoreError>;

    /// 读取所有会话的事件流（按 session 分组、组内 sequence 升序）。
    ///
    /// 用于进程重启后重建状态 Projection 与幂等缓存（§17.1）。
    async fn load_all_sessions(
        &self,
    ) -> Result<Vec<(SessionId, Vec<EventEnvelope>)>, EventStoreError>;
}

/// 内存实现（开发 / 测试用）。
#[derive(Default)]
pub struct InMemoryEventStore {
    inner: Mutex<Store>,
}

#[derive(Default)]
struct Store {
    /// session -> (next_sequence, events)
    sessions: HashMap<SessionId, (u64, Vec<EventEnvelope>)>,
}

impl InMemoryEventStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(&self, mut envelope: EventEnvelope) -> Result<u64, EventStoreError> {
        let mut store = self.inner.lock().expect("event store poisoned");
        let entry = store
            .sessions
            .entry(envelope.session_id)
            .or_insert((1, Vec::new()));
        let seq = entry.0;
        envelope.sequence = seq;
        entry.0 = seq + 1;
        entry.1.push(envelope);
        Ok(seq)
    }

    async fn load(
        &self,
        session_id: SessionId,
        after_sequence: u64,
        limit: usize,
        durable_only: bool,
    ) -> Result<Vec<EventEnvelope>, EventStoreError> {
        let store = self.inner.lock().expect("event store poisoned");
        let Some((_, events)) = store.sessions.get(&session_id) else {
            return Ok(Vec::new());
        };
        let matched = events
            .iter()
            .filter(|e| e.sequence > after_sequence)
            .filter(|e| !durable_only || e.durability == Durability::Durable)
            .take(limit)
            .cloned()
            .collect();
        Ok(matched)
    }

    async fn latest_sequence(&self, session_id: SessionId) -> Result<u64, EventStoreError> {
        let store = self.inner.lock().expect("event store poisoned");
        Ok(store
            .sessions
            .get(&session_id)
            .map(|(next, _)| next - 1)
            .unwrap_or(0))
    }

    async fn load_all_sessions(
        &self,
    ) -> Result<Vec<(SessionId, Vec<EventEnvelope>)>, EventStoreError> {
        let store = self.inner.lock().expect("event store poisoned");
        let mut sessions: Vec<(SessionId, Vec<EventEnvelope>)> = store
            .sessions
            .iter()
            .map(|(sid, (_, events))| (*sid, events.clone()))
            .collect();
        sessions.sort_by_key(|(sid, _)| *sid);
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{Actor, SessionId};

    fn draft(session: SessionId, event_type: &str, durability: Durability) -> EventEnvelope {
        EventEnvelope::draft(
            session,
            None,
            event_type,
            durability,
            Actor::system("test"),
            serde_json::json!({}),
        )
    }

    #[tokio::test]
    async fn sequence_is_strictly_monotonic_per_session() {
        let store = InMemoryEventStore::new();
        let s = SessionId::generate();
        let a = store
            .append(draft(s, "session.created", Durability::Durable))
            .await
            .unwrap();
        let b = store
            .append(draft(s, "turn.started", Durability::Durable))
            .await
            .unwrap();
        let c = store
            .append(draft(s, "message.delta", Durability::Transient))
            .await
            .unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(store.latest_sequence(s).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn replay_after_sequence_and_durable_only() {
        let store = InMemoryEventStore::new();
        let s = SessionId::generate();
        for (i, (t, d)) in [
            ("session.created", Durability::Durable),
            ("message.delta", Durability::Transient),
            ("message.completed", Durability::Durable),
        ]
        .into_iter()
        .enumerate()
        {
            let mut e = draft(s, t, d);
            e.sequence = (i + 1) as u64; // 仅测试标记，实际由 append 覆盖
            store.append(e).await.unwrap();
        }

        let all = store.load(s, 0, 100, false).await.unwrap();
        assert_eq!(all.len(), 3);

        let durable = store.load(s, 0, 100, true).await.unwrap();
        assert_eq!(durable.len(), 2, "补发只包含 durable 事件");
        assert_eq!(durable[0].event_type, "session.created");

        let after = store.load(s, 2, 100, false).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].event_type, "message.completed");
    }
}
