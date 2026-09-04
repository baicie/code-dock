//! 标准错误码与 Tool 错误（§8.3.9）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// 标准错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArguments,
    PermissionDenied,
    ApprovalExpired,
    ResourceChanged,
    NotFound,
    Timeout,
    Cancelled,
    ExecutionFailed,
    PluginUnavailable,
    NetworkError,
    RateLimited,
    SandboxViolation,
    InternalError,
}

impl ErrorCode {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InvalidArguments => "invalid_arguments",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::ApprovalExpired => "approval_expired",
            ErrorCode::ResourceChanged => "resource_changed",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::ExecutionFailed => "execution_failed",
            ErrorCode::PluginUnavailable => "plugin_unavailable",
            ErrorCode::NetworkError => "network_error",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::SandboxViolation => "sandbox_violation",
            ErrorCode::InternalError => "internal_error",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 工具/插件错误。插件错误码格式：`plugin:<plugin-id>:<error-code>`（§8.3.9）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            message: message.into(),
            data: None,
        }
    }

    pub fn plugin(plugin_id: &str, code: impl fmt::Display, message: impl Into<String>) -> Self {
        Self {
            code: format!("plugin:{plugin_id}:{code}"),
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_error_code_format() {
        let e = ToolError::plugin(
            "com.codedock.android",
            "adb_not_found",
            "adb binary missing",
        );
        assert_eq!(e.code, "plugin:com.codedock.android:adb_not_found");
    }
}
