//! Policy Engine：所有副作用操作的唯一审批入口（§5.1）。
//!
//! 默认策略（§8.3.7）：
//! - Low：Ask/Plan 中仅允许无副作用读操作；
//! - Medium：Edit 可按项目策略自动允许；
//! - High：默认需要用户审批；
//! - Critical：永远不得由 Agent 自动批准。
//!
//! 不可信内容（external_untrusted）引导出的副作用操作，默认至少提升一级审批要求（§18.1）。

use codedock_protocol::{Capability, Risk, SessionMode, Trust};
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
    pub capabilities: Vec<Capability>,
}

impl PolicyContext {
    pub fn new(
        session_mode: SessionMode,
        tool_name: impl Into<String>,
        resource: impl Into<String>,
        risk: Risk,
        trust: Trust,
    ) -> Self {
        Self {
            session_mode,
            tool_name: tool_name.into(),
            resource: resource.into(),
            risk,
            trust,
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
            Trust::WorkspaceUntrusted,
        );
        assert!(matches!(
            DefaultPolicyEngine.decide(&ctx),
            PolicyDecision::Deny { .. }
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
