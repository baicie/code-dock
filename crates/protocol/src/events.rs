//! 核心事件类型（§8.2.4）。
//!
//! 客户端遇到未知事件类型或枚举值时，必须保留原始数据并降级展示，不能崩溃（§8.1），
//! 因此信封层使用字符串 `event_type`；此枚举用于 Runtime 内部强类型分发。

use serde::{Deserialize, Serialize};

macro_rules! event_types {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// 全量核心事件类型清单（v1.0）。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum EventType {
            $(
                #[serde(rename = $name)]
                $variant,
            )+
        }

        impl EventType {
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(EventType::$variant => $name,)+
                }
            }
        }

        impl std::fmt::Display for EventType {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for EventType {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($name => Ok(EventType::$variant),)+
                    other => Err(format!("未知事件类型: {other}")),
                }
            }
        }
    };
}

event_types! {
    // Session
    SessionCreated => "session.created",
    SessionStarted => "session.started",
    SessionModeChanged => "session.mode_changed",
    SessionPaused => "session.paused",
    SessionResumed => "session.resumed",
    // v1.0 补充事件类型：Tool 审批等待（§8.2.7 waiting_approval 状态）。
    SessionWaitingApproval => "session.waiting_approval",
    SessionCompleted => "session.completed",
    SessionFailed => "session.failed",
    SessionCancelled => "session.cancelled",

    // Turn
    TurnStarted => "turn.started",
    TurnCompleted => "turn.completed",
    TurnFailed => "turn.failed",

    // Message
    MessageCreated => "message.created",
    MessageDelta => "message.delta",
    MessageCompleted => "message.completed",

    // Plan
    PlanCreated => "plan.created",
    PlanUpdated => "plan.updated",
    PlanStepStarted => "plan.step.started",
    PlanStepCompleted => "plan.step.completed",
    PlanStepFailed => "plan.step.failed",

    // Context
    ContextSnapshotCreated => "context.snapshot.created",

    // Model
    ModelRequestStarted => "model.request.started",
    ModelRequestCompleted => "model.request.completed",
    ModelRequestFailed => "model.request.failed",
    ModelRequestCancelled => "model.request.cancelled",

    // Tool
    ToolCallProposed => "tool.call.proposed",
    ToolCallPreflighted => "tool.call.preflighted",
    ToolCallApprovalRequired => "tool.call.approval_required",
    ToolCallApproved => "tool.call.approved",
    ToolCallRejected => "tool.call.rejected",
    ToolCallStarted => "tool.call.started",
    ToolCallOutput => "tool.call.output",
    ToolCallCompleted => "tool.call.completed",
    ToolCallFailed => "tool.call.failed",
    ToolCallCancelled => "tool.call.cancelled",

    // Change
    ChangeProposed => "change.proposed",
    ChangeApplied => "change.applied",
    ChangeConflicted => "change.conflicted",
    ChangeReverted => "change.reverted",

    // Checkpoint
    CheckpointCreated => "checkpoint.created",
    CheckpointRestored => "checkpoint.restored",

    // Security
    PolicyDecisionMade => "policy.decision_made",
    SecurityWarning => "security.warning",
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn roundtrip_all_event_types() {
        for name in [
            "session.created",
            "tool.call.approval_required",
            "context.snapshot.created",
            "policy.decision_made",
        ] {
            let t = EventType::from_str(name).unwrap();
            assert_eq!(t.as_str(), name);
            assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn unknown_event_type_is_rejected_at_enum_level() {
        // 信封层 payload/event_type 为字符串，未知类型由上层降级处理。
        assert!(EventType::from_str("future.new_event").is_err());
    }
}
