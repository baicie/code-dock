//! 事件溯源的 SessionManager 实现。
//!
//! 每次状态迁移都作为 Durable Event 追加（§8.2.3），命令幂等由
//! `method + idempotency_key` 去重保证（§8.2.6）：重复命令直接返回缓存会话的
//! 当前投影，不再产生任何副作用。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use codedock_event_store::EventStore;
use codedock_protocol::{Actor, Durability, EventEnvelope, SessionId, SessionMode, SessionStatus};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{SessionError, SessionManager, SessionRecord};

/// 给 payload 附加命令键（携带幂等键时），用于重启后恢复幂等缓存（§8.2.6）。
fn command_payload(mut payload: Value, dk: &Option<String>) -> Value {
    if let Some(key) = dk {
        payload["command_key"] = json!(key);
    }
    payload
}

/// 重建时应用状态迁移；非法迁移记录日志并保留当前状态（历史事件应总是合法）。
fn apply_status(record: &mut Option<SessionRecord>, to: SessionStatus) {
    if let Some(rec) = record.as_mut() {
        if let Err(err) = rec.transition(to) {
            tracing::debug!(session = %rec.session_id, ?to, ?err, "重建时忽略非法状态迁移");
        }
    }
}

/// 客户端可见的 Session 投影。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub mode: SessionMode,
    pub status: SessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub latest_sequence: u64,
}

/// 基于 Event Store 的 SessionManager。
pub struct EventSourcedSessionManager {
    store: Arc<dyn EventStore>,
    /// 状态 Projection（重启后由 [`EventSourcedSessionManager::restore`] 从事件流重建，§17.1）。
    sessions: Mutex<HashMap<SessionId, SessionRecord>>,
    /// 幂等缓存：`method:key` → 受影响 session。
    done: Mutex<HashMap<String, SessionId>>,
}

impl EventSourcedSessionManager {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            sessions: Mutex::new(HashMap::new()),
            done: Mutex::new(HashMap::new()),
        }
    }

    /// 从 Event Store 重建状态 Projection 与幂等缓存（§17.1：重启恢复）。
    pub async fn restore(store: Arc<dyn EventStore>) -> Result<Self, SessionError> {
        let sessions = store
            .load_all_sessions()
            .await
            .map_err(|e| SessionError::EventStore(e.to_string()))?;
        let mgr = Self::new(store);

        for (session_id, events) in sessions {
            let mut record: Option<SessionRecord> = None;
            for event in events {
                match event.event_type.as_str() {
                    "session.created" => {
                        let mode = event
                            .payload
                            .get("mode")
                            .and_then(|v| serde_json::from_value::<SessionMode>(v.clone()).ok());
                        let Some(mode) = mode else {
                            tracing::warn!(
                                %session_id,
                                "session.created 缺少合法 mode，跳过该会话重建"
                            );
                            record = None;
                            break;
                        };
                        record = Some(SessionRecord {
                            session_id,
                            mode,
                            status: SessionStatus::Created,
                            task: event
                                .payload
                                .get("task")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            plan: None,
                            created_at: event.occurred_at,
                        });
                    }
                    "session.started" => apply_status(&mut record, SessionStatus::Running),
                    "session.paused" => apply_status(&mut record, SessionStatus::Paused),
                    "session.resumed" => apply_status(&mut record, SessionStatus::Running),
                    "session.waiting_approval" => {
                        apply_status(&mut record, SessionStatus::WaitingApproval)
                    }
                    "session.cancelled" => apply_status(&mut record, SessionStatus::Cancelled),
                    "session.mode_changed" => {
                        if let Some(rec) = record.as_mut() {
                            if let Some(mode) = event
                                .payload
                                .get("to")
                                .and_then(|v| serde_json::from_value::<SessionMode>(v.clone()).ok())
                            {
                                rec.mode = mode;
                            }
                        }
                    }
                    // 未知事件类型不参与状态重建（§8.1：降级保留）。
                    _ => {}
                }

                // 恢复幂等缓存：command_key 已含 method 前缀（如 "create:k1"）。
                if let Some(key) = event.payload.get("command_key").and_then(Value::as_str) {
                    mgr.done
                        .lock()
                        .expect("done map poisoned")
                        .insert(key.to_string(), session_id);
                }
            }
            if let Some(rec) = record {
                mgr.remember(rec);
            }
        }
        Ok(mgr)
    }

    async fn info(&self, rec: &SessionRecord) -> Result<SessionInfo, SessionError> {
        let latest_sequence = self
            .store
            .latest_sequence(rec.session_id)
            .await
            .map_err(|e| SessionError::EventStore(e.to_string()))?;
        Ok(SessionInfo {
            session_id: rec.session_id,
            mode: rec.mode,
            status: rec.status,
            task: rec.task.clone(),
            latest_sequence,
        })
    }

    fn record_of(&self, id: SessionId) -> Result<SessionRecord, SessionError> {
        self.sessions
            .lock()
            .expect("session map poisoned")
            .get(&id)
            .cloned()
            .ok_or(SessionError::NotFound(id))
    }

    fn remember(&self, rec: SessionRecord) {
        self.sessions
            .lock()
            .expect("session map poisoned")
            .insert(rec.session_id, rec);
    }

    /// 幂等命中：返回受影响 session 的当前投影。
    async fn replay_done(&self, dk: &Option<String>) -> Result<Option<SessionInfo>, SessionError> {
        if let Some(k) = dk {
            let cached = self.done.lock().expect("done map poisoned").get(k).copied();
            if let Some(sid) = cached {
                let rec = self.record_of(sid)?;
                return Ok(Some(self.info(&rec).await?));
            }
        }
        Ok(None)
    }

    fn remember_done(&self, dk: &Option<String>, sid: SessionId) {
        if let Some(k) = dk {
            self.done
                .lock()
                .expect("done map poisoned")
                .insert(k.clone(), sid);
        }
    }

    /// 追加一条 Durable Event（§8.2.3）。sequence 由 Event Store 单调分配。
    async fn append(
        &self,
        session_id: SessionId,
        event_type: &str,
        payload: Value,
    ) -> Result<u64, SessionError> {
        let envelope = EventEnvelope::draft(
            session_id,
            None,
            event_type,
            Durability::Durable,
            Actor::user("local"),
            payload,
        );
        self.store
            .append(envelope)
            .await
            .map_err(|e| SessionError::EventStore(e.to_string()))
    }
}

#[async_trait::async_trait]
impl SessionManager for EventSourcedSessionManager {
    async fn create(
        &self,
        mode: SessionMode,
        task: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("create:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }

        let mut rec = SessionRecord::new(mode);
        rec.task = task.clone();
        rec.transition(SessionStatus::Running)?; // created → running（§8.2.7）

        self.append(
            rec.session_id,
            "session.created",
            command_payload(
                json!({
                    "mode": mode, "task": task, "status": "created"
                }),
                &dk,
            ),
        )
        .await?;
        self.append(rec.session_id, "session.started", json!({ "mode": mode }))
            .await?;

        self.remember(rec.clone());
        self.remember_done(&dk, rec.session_id);
        self.info(&rec).await
    }

    async fn status(&self, id: SessionId) -> Result<SessionInfo, SessionError> {
        let rec = self.record_of(id)?;
        self.info(&rec).await
    }

    async fn pause(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("pause:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.status;
        rec.transition(SessionStatus::Paused)?;
        self.append(
            id,
            "session.paused",
            json!({ "from": from, "to": rec.status }),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }

    async fn resume(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("resume:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.status;
        rec.transition(SessionStatus::Running)?;
        self.append(
            id,
            "session.resumed",
            json!({ "from": from, "to": rec.status }),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }

    async fn cancel(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("cancel:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.status;
        rec.transition(SessionStatus::Cancelled)?;
        self.append(
            id,
            "session.cancelled",
            json!({ "from": from, "to": rec.status }),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }

    async fn set_mode(
        &self,
        id: SessionId,
        mode: SessionMode,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("mode:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.mode;
        rec.mode = mode;
        self.append(
            id,
            "session.mode_changed",
            command_payload(json!({ "from": from, "to": mode }), &dk),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }

    async fn events(
        &self,
        id: SessionId,
        after_sequence: u64,
        durable_only: bool,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, SessionError> {
        self.store
            .load(id, after_sequence, limit, durable_only)
            .await
            .map_err(|e| SessionError::EventStore(e.to_string()))
    }

    async fn enter_waiting_approval(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("wait_approval:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.status;
        rec.transition(SessionStatus::WaitingApproval)?;
        self.append(
            id,
            "session.waiting_approval",
            json!({ "from": from, "to": rec.status }),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }

    async fn exit_waiting_approval(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError> {
        let dk = idempotency_key.map(|k| format!("exit_approval:{k}"));
        if let Some(info) = self.replay_done(&dk).await? {
            return Ok(info);
        }
        let mut rec = self.record_of(id)?;
        let from = rec.status;
        rec.transition(SessionStatus::Running)?;
        self.append(
            id,
            "session.resumed",
            json!({ "from": from, "to": rec.status, "reason": "approval_resolved" }),
        )
        .await?;
        self.remember(rec.clone());
        self.remember_done(&dk, id);
        self.info(&rec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_event_store::InMemoryEventStore;

    fn manager() -> (EventSourcedSessionManager, Arc<InMemoryEventStore>) {
        let store = Arc::new(InMemoryEventStore::new());
        let mgr = EventSourcedSessionManager::new(store.clone());
        (mgr, store)
    }

    #[tokio::test]
    async fn full_lifecycle_appends_durable_events() {
        let (mgr, _store) = manager();
        let info = mgr
            .create(SessionMode::Plan, Some("修复一个真实 Bug".into()), None)
            .await
            .unwrap();
        assert_eq!(info.status, SessionStatus::Running);
        assert_eq!(info.latest_sequence, 2, "created + started");

        let info = mgr.pause(info.session_id, None).await.unwrap();
        assert_eq!(info.status, SessionStatus::Paused);
        let info = mgr.resume(info.session_id, None).await.unwrap();
        assert_eq!(info.status, SessionStatus::Running);
        let info = mgr.cancel(info.session_id, None).await.unwrap();
        assert_eq!(info.status, SessionStatus::Cancelled);
        assert_eq!(info.latest_sequence, 5);

        let events = mgr.events(info.session_id, 0, true, 100).await.unwrap();
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "session.created",
                "session.started",
                "session.paused",
                "session.resumed",
                "session.cancelled"
            ]
        );
        assert!(events.iter().all(|e| e.durability == Durability::Durable));
        // sequence 严格单调递增
        assert!(events.windows(2).all(|w| w[0].sequence < w[1].sequence));
    }

    #[tokio::test]
    async fn idempotency_prevents_double_execution() {
        let (mgr, _store) = manager();
        let a = mgr
            .create(SessionMode::Ask, Some("task".into()), Some("k1".into()))
            .await
            .unwrap();
        let b = mgr
            .create(SessionMode::Ask, Some("task".into()), Some("k1".into()))
            .await
            .unwrap();
        assert_eq!(a.session_id, b.session_id, "相同幂等键返回同一会话");
        assert_eq!(b.latest_sequence, 2, "不重复追加事件");

        mgr.cancel(a.session_id, Some("c1".to_string()))
            .await
            .unwrap();
        let after = mgr
            .cancel(a.session_id, Some("c1".to_string()))
            .await
            .unwrap();
        assert_eq!(after.status, SessionStatus::Cancelled);
        assert_eq!(after.latest_sequence, 3, "重复 cancel 不重复追加");
    }

    #[tokio::test]
    async fn invalid_transition_is_rejected_without_side_effect() {
        let (mgr, store) = manager();
        let info = mgr.create(SessionMode::Ask, None, None).await.unwrap();
        mgr.pause(info.session_id, None).await.unwrap();
        let err = mgr.pause(info.session_id, None).await.unwrap_err();
        assert!(matches!(err, SessionError::InvalidTransition { .. }));
        assert_eq!(
            mgr.events(info.session_id, 0, true, 100)
                .await
                .unwrap()
                .len(),
            3
        );
        drop(store);
    }

    #[tokio::test]
    async fn mode_change_and_unknown_session() {
        let (mgr, _store) = manager();
        let info = mgr.create(SessionMode::Ask, None, None).await.unwrap();
        let info = mgr
            .set_mode(info.session_id, SessionMode::Edit, None)
            .await
            .unwrap();
        assert_eq!(info.mode, SessionMode::Edit);

        let err = mgr.status(SessionId::generate()).await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn projection_and_idempotency_survive_restart() {
        let store = Arc::new(InMemoryEventStore::new());
        let mgr = EventSourcedSessionManager::new(store.clone());
        let info = mgr
            .create(
                SessionMode::Plan,
                Some("重构模块".into()),
                Some("k1".into()),
            )
            .await
            .unwrap();
        mgr.pause(info.session_id, Some("p1".to_string()))
            .await
            .unwrap();
        mgr.set_mode(info.session_id, SessionMode::Edit, None)
            .await
            .unwrap();
        drop(mgr);

        let restored = EventSourcedSessionManager::restore(store).await.unwrap();
        let after = restored.status(info.session_id).await.unwrap();
        assert_eq!(after.status, SessionStatus::Paused);
        assert_eq!(after.mode, SessionMode::Edit);
        assert_eq!(after.task.as_deref(), Some("重构模块"));

        // 幂等缓存跨重启恢复：重复 create 返回同一会话，不追加事件。
        let again = restored
            .create(
                SessionMode::Plan,
                Some("重构模块".into()),
                Some("k1".into()),
            )
            .await
            .unwrap();
        assert_eq!(again.session_id, info.session_id);
        assert_eq!(again.latest_sequence, 4, "重启后重复命令不追加事件");
    }
}
