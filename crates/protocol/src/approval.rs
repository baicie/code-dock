//! Policy & Approval Protocol 类型（§9.1）。
//!
//! 规则：审批一次只对应一个 Digest；Agent、Tool 和 Plugin 不能充当 Approver。

use crate::ids::{ApprovalId, OperationDigest, SessionId, ToolCallId};
use crate::tool::{Permission, Risk};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 审批请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: ApprovalId,
    pub session_id: SessionId,
    pub tool_call_id: ToolCallId,
    pub operation_digest: OperationDigest,
    pub risk: Risk,
    pub permissions: Vec<Permission>,
    pub preview: String,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
    pub allowed_responses: Vec<ApprovalDecision>,
}

impl ApprovalRequest {
    /// 检查某决策是否被允许（Critical 默认不提供"永久允许"，§9.1）。
    pub fn allows(&self, decision: ApprovalDecision) -> bool {
        self.allowed_responses.contains(&decision)
    }
}

/// 审批响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// 仅批准当前 Digest 一次。
    ApproveOnce,
    /// 拒绝。
    Deny,
}

/// 审批失效原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalInvalidation {
    Expired,
    DigestChanged,
    SessionStateChanged,
    DeviceRevoked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use chrono::Duration;

    #[test]
    fn approval_expiration_binding() {
        let req = ApprovalRequest {
            approval_id: ApprovalId::generate(),
            session_id: SessionId::generate(),
            tool_call_id: ToolCallId::generate(),
            operation_digest: OperationDigest::from_sha256_hex("ff"),
            risk: Risk::High,
            permissions: vec![Permission {
                capability: Capability::GitPush,
                resource: "origin/dev".to_string(),
            }],
            preview: "Push branch dev to origin".to_string(),
            reason: "Network write and remote repository modification".to_string(),
            expires_at: Utc::now() + Duration::minutes(5),
            allowed_responses: vec![ApprovalDecision::ApproveOnce, ApprovalDecision::Deny],
        };
        assert!(req.allows(ApprovalDecision::ApproveOnce));
        assert!(req.allows(ApprovalDecision::Deny));
    }
}
