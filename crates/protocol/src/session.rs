//! Session 状态机与模式（§8.2.7、§4.1）。

use serde::{Deserialize, Serialize};

/// 四种 Session 模式（§4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// 只读问答。
    Ask,
    /// 只读 + 计划。
    Plan,
    /// 可按项目策略自动执行 Medium 及以下写操作。
    Edit,
    /// 自治模式，但 Critical 永远需要人工审批（§8.3.7）。
    Auto,
}

/// Session 状态机（§8.2.7）。
///
/// ```text
/// created → running
/// running → waiting_approval / paused / completed / failed / cancelled
/// waiting_approval → running / cancelled
/// paused → running / cancelled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    WaitingApproval,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl SessionStatus {
    /// 是否终态。终态事实不可篡改；恢复失败任务需创建新 Turn / Session。
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
        )
    }

    /// 校验状态迁移是否合法；非法迁移返回 `Err(self)`。
    pub fn transition(self, to: SessionStatus) -> Result<SessionStatus, SessionStatus> {
        let allowed = match self {
            SessionStatus::Created => {
                matches!(to, SessionStatus::Running | SessionStatus::Cancelled)
            }
            SessionStatus::Running => matches!(
                to,
                SessionStatus::WaitingApproval
                    | SessionStatus::Paused
                    | SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Cancelled
            ),
            SessionStatus::WaitingApproval => {
                matches!(to, SessionStatus::Running | SessionStatus::Cancelled)
            }
            SessionStatus::Paused => {
                matches!(to, SessionStatus::Running | SessionStatus::Cancelled)
            }
            // 终态不可再迁移
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled => false,
        };
        if allowed { Ok(to) } else { Err(to) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_transitions() {
        let s = SessionStatus::Created;
        let s = s.transition(SessionStatus::Running).unwrap();
        let s = s.transition(SessionStatus::WaitingApproval).unwrap();
        let s = s.transition(SessionStatus::Running).unwrap();
        let s = s.transition(SessionStatus::Completed).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn terminal_states_are_immutable() {
        for t in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
        ] {
            assert!(t.transition(SessionStatus::Running).is_err());
        }
    }

    #[test]
    fn created_cannot_jump_to_waiting_approval() {
        assert!(
            SessionStatus::Created
                .transition(SessionStatus::WaitingApproval)
                .is_err()
        );
    }
}
