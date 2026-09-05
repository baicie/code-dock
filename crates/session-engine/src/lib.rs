//! Session Engine：Agent 状态机、Turn、计划、暂停、恢复、取消（§6 / §8.2.7）。
//!
//! - [`SessionRecord`] 是当前状态 Projection，可由 Event Store 重建（§17.1）；
//! - [`manager::EventSourcedSessionManager`] 把每次状态迁移作为 Durable Event
//!   追加到 Event Store，并保证命令幂等（§8.2.6）。

pub mod manager;
pub mod turn;

use codedock_protocol::{SessionId, SessionMode, SessionStatus};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use manager::{EventSourcedSessionManager, SessionInfo};
pub use turn::{TurnEngine, TurnError, TurnOutcome};

/// 计划步骤状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// 计划步骤（`plan.step.*` 事件的载体）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub index: usize,
    pub title: String,
    pub status: PlanStepStatus,
}

/// 计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub revision: u64,
}

/// Session 当前状态记录（Projection，可由 Event Store 重建）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub mode: SessionMode,
    pub status: SessionStatus,
    /// 创建会话时的用户任务描述。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub plan: Option<Plan>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl SessionRecord {
    pub fn new(mode: SessionMode) -> Self {
        Self {
            session_id: SessionId::generate(),
            mode,
            status: SessionStatus::Created,
            task: None,
            plan: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// 迁移状态；非法迁移返回错误，不改变自身。
    pub fn transition(&mut self, to: SessionStatus) -> Result<(), SessionError> {
        let from = self.status;
        self.status = from
            .transition(to)
            .map_err(|to| SessionError::InvalidTransition { from, to })?;
        Ok(())
    }
}

/// Turn 记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn_id: codedock_protocol::TurnId,
    pub session_id: SessionId,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("非法状态迁移: {from:?} → {to:?}")]
    InvalidTransition {
        from: SessionStatus,
        to: SessionStatus,
    },
    #[error("session {0} 不存在")]
    NotFound(SessionId),
    #[error("事件存储失败: {0}")]
    EventStore(String),
    #[error("命令序列化失败: {0}")]
    Encoding(String),
}

/// Session 管理抽象（供 Daemon 装配；幂等键见 §8.2.6）。
#[async_trait::async_trait]
pub trait SessionManager: Send + Sync {
    async fn create(
        &self,
        mode: SessionMode,
        task: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    async fn status(&self, id: SessionId) -> Result<SessionInfo, SessionError>;

    async fn pause(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    async fn resume(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    async fn cancel(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    async fn set_mode(
        &self,
        id: SessionId,
        mode: SessionMode,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    /// 重放事件（§8.2.5：先补发 Durable Event）。
    async fn events(
        &self,
        id: SessionId,
        after_sequence: u64,
        durable_only: bool,
        limit: usize,
    ) -> Result<Vec<codedock_protocol::EventEnvelope>, SessionError>;

    /// 进入等待审批状态（running → waiting_approval，§8.2.7）。
    async fn enter_waiting_approval(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;

    /// 审批裁决后回到运行状态（waiting_approval → running）。
    async fn exit_waiting_approval(
        &self,
        id: SessionId,
        idempotency_key: Option<String>,
    ) -> Result<SessionInfo, SessionError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle() {
        let mut s = SessionRecord::new(SessionMode::Plan);
        assert_eq!(s.status, SessionStatus::Created);
        s.transition(SessionStatus::Running).unwrap();
        s.transition(SessionStatus::Paused).unwrap();
        s.transition(SessionStatus::Running).unwrap();
        s.transition(SessionStatus::Failed).unwrap();
        assert!(s.status.is_terminal());
    }

    #[test]
    fn invalid_transition_rejected() {
        let mut s = SessionRecord::new(SessionMode::Ask);
        let err = s.transition(SessionStatus::WaitingApproval).unwrap_err();
        assert!(matches!(err, SessionError::InvalidTransition { .. }));
    }
}
