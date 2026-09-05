//! Policy Engine：所有副作用操作的唯一审批入口（§5.1）。
//!
//! 默认策略（§8.3.7）：
//! - Low：Ask/Plan 中仅允许无副作用读操作；
//! - Medium：Edit 可按项目策略自动允许；
//! - High：默认需要用户审批；
//! - Critical：永远不得由 Agent 自动批准。
//!
//! 不可信内容（external_untrusted）引导出的副作用操作，默认至少提升一级审批要求（§18.1）。

use codedock_protocol::{Capability, Effect, Risk, SessionMode, Trust};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 审批升级需求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequirement {
    None,
    Required,
    /// Critical：必须人工重新确认，不得自动批准（§8.3.7）。
    HumanMandatory,
}

/// Policy Engine 裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    #[serde(rename_all = "snake_case")]
    RequireApproval {
        reason: String,
    },
    #[serde(rename_all = "snake_case")]
    Deny {
        reason: String,
    },
}

/// 参与裁决的上下文。
#[derive(Debug, Clone)]
pub struct PolicyContext {
    pub session_mode: SessionMode,
    pub tool_name: String,
    /// 归一化后的目标资源（路径、命令行、域名等）。
    pub resource: String,
    pub risk: Risk,
    pub trust: Trust,
    /// 工具副作用声明（§8.3.7：Ask/Plan 仅允许 none）。
    pub effect: Effect,
    pub capabilities: Vec<Capability>,
}

impl PolicyContext {
    pub fn new(
        session_mode: SessionMode,
        tool_name: impl Into<String>,
        resource: impl Into<String>,
        risk: Risk,
        trust: Trust,
        effect: Effect,
    ) -> Self {
        Self {
            session_mode,
            tool_name: tool_name.into(),
            resource: resource.into(),
            risk,
            trust,
            effect,
            capabilities: Vec::new(),
        }
    }
}

impl fmt::Display for PolicyDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyDecision::Allow => write!(f, "allow"),
            PolicyDecision::RequireApproval { reason } => write!(f, "require_approval({reason})"),
            PolicyDecision::Deny { reason } => write!(f, "deny({reason})"),
        }
    }
}

/// Policy Engine 抽象。
pub trait PolicyEngine: Send + Sync {
    fn decide(&self, ctx: &PolicyContext) -> PolicyDecision;
}

/// v1.0 默认策略。
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPolicyEngine;

impl DefaultPolicyEngine {
    /// 内置工具的默认风险基线（§13）。
    pub fn default_risk(tool_name: &str, resource: &str) -> Risk {
        match tool_name {
            "file.read" | "search.text" | "search.symbol" | "git.status" | "git.diff" => Risk::Low,
            "file.patch" | "git.commit" => Risk::Medium,
            "shell.execute" => {
                // gradle/npm test 等构建命令示例为 medium（§8.3.5）
                Risk::Medium
            }
            "git.push" => {
                if resource.contains("--force") {
                    // 重写公共历史为 Critical（§13.4）
                    Risk::Critical
                } else {
                    Risk::High
                }
            }
            "git.rebase" | "git.reset" => Risk::Critical,
            _ => Risk::High,
        }
    }

    /// 明确拒绝清单：无论模式如何都默认拒绝（§24 负向测试 1 的基础）。
    pub fn is_hard_denied(resource: &str) -> bool {
        const SENSITIVE_PREFIXES: [&str; 6] = [
            "~/.ssh",
            "~/.gnupg",
            "~/.aws",
            "~/Library/Keychains",
            "/etc/shadow",
            "~/.config/gcloud",
        ];
        SENSITIVE_PREFIXES.iter().any(|p| resource.starts_with(p))
    }

    /// 不可信来源触发的副作用操作至少提升一级（§18.1）。
    pub fn escalated_for_trust(risk: Risk, trust: Trust) -> Risk {
        match trust {
            Trust::Trusted => risk,
            Trust::WorkspaceUntrusted | Trust::PluginUntrusted | Trust::ExternalUntrusted => {
                risk.escalate().unwrap_or(Risk::Critical)
            }
        }
    }
}

trait RiskEscalate {
    fn escalate(&self) -> Option<Risk>;
}

impl RiskEscalate for Risk {
    fn escalate(&self) -> Option<Risk> {
        match self {
            Risk::Low => Some(Risk::Medium),
            Risk::Medium => Some(Risk::High),
            Risk::High | Risk::Critical => Some(Risk::Critical),
        }
    }
}

impl PolicyEngine for DefaultPolicyEngine {
    fn decide(&self, ctx: &PolicyContext) -> PolicyDecision {
        if Self::is_hard_denied(&ctx.resource) {
            return PolicyDecision::Deny {
                reason: "命中敏感资源默认拒绝清单".into(),
            };
        }

        // §8.3.7：Ask/Plan 中仅允许无副作用读操作——任何 effect != none 的
        // 工具无论风险等级一律拒绝（含 Low 写操作）。
        if matches!(ctx.session_mode, SessionMode::Ask | SessionMode::Plan)
            && ctx.effect != Effect::None
        {
            return PolicyDecision::Deny {
                reason: "Ask/Plan 模式下仅允许无副作用读操作".into(),
            };
        }

        // §18.1：由不可信内容（工作区文件、工具输出等）引导的操作提升一级。
        let risk = Self::escalated_for_trust(ctx.risk, ctx.trust);

        match risk {
            Risk::Critical => PolicyDecision::RequireApproval {
                reason: "Critical 操作必须人工确认，且不得被自动批准".into(),
            },
            Risk::High => PolicyDecision::RequireApproval {
                reason: "High 风险默认需要用户审批".into(),
            },
            Risk::Medium => match ctx.session_mode {
                SessionMode::Ask | SessionMode::Plan => PolicyDecision::Deny {
                    reason: "Ask/Plan 模式下仅允许无副作用读操作".into(),
                },
                SessionMode::Edit | SessionMode::Auto => PolicyDecision::Allow,
            },
            Risk::Low => PolicyDecision::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_mode_blocks_side_effects() {
        let ctx = PolicyContext::new(
            SessionMode::Ask,
            "file.patch",
            "$workspace/src/main.rs",
            Risk::Medium,
            Trust::Trusted,
            Effect::Possible,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn high_risk_requires_approval_even_in_auto() {
        let ctx = PolicyContext::new(
            SessionMode::Auto,
            "git.push",
            "origin main",
            Risk::High,
            Trust::Trusted,
            Effect::Guaranteed,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn untrusted_source_escalates_risk() {
        // medium shell 命令被不可信 README 引导 → high，需要审批
        let ctx = PolicyContext::new(
            SessionMode::Edit,
            "shell.execute",
            "./gradlew test",
            Risk::Medium,
            Trust::ExternalUntrusted,
            Effect::Possible,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn sensitive_paths_hard_denied() {
        let ctx = PolicyContext::new(
            SessionMode::Edit,
            "file.read",
            "~/.ssh/id_rsa",
            Risk::Low,
            Trust::Trusted,
            Effect::None,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn medium_side_effect_denied_in_ask_regardless_of_escalation() {
        // §8.3.7：Ask 模式 effect != none 一律拒绝（即使 trust 提升后仍是 Medium）
        let ctx = PolicyContext::new(
            SessionMode::Ask,
            "file.patch",
            "$workspace/src/main.rs",
            Risk::Low,
            Trust::Trusted,
            Effect::Possible,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn medium_auto_allowed_in_edit_on_trusted_context() {
        // §8.3.7：Edit 模式下 Medium 按项目策略自动允许——干净上下文（模型
        // 尚未读过工作区内容）时 trust=Trusted，无提升
        let ctx = PolicyContext::new(
            SessionMode::Edit,
            "file.patch",
            "$workspace/src/main.rs",
            Risk::Medium,
            Trust::Trusted,
            Effect::Possible,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn medium_escalates_to_approval_after_untrusted_content() {
        // §18.1：模型读过工作区内容（trust=workspace_untrusted）后，
        // Medium 提升 High → 审批
        let ctx = PolicyContext::new(
            SessionMode::Edit,
            "file.patch",
            "$workspace/src/main.rs",
            Risk::Medium,
            Trust::WorkspaceUntrusted,
            Effect::Possible,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn force_push_is_critical() {
        assert_eq!(
            DefaultPolicyEngine::default_risk("git.push", "git push --force"),
            Risk::Critical
        );
        assert_eq!(
            DefaultPolicyEngine::default_risk("git.push", "origin main"),
            Risk::High
        );
    }
}
