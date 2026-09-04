//! Plugin Host：WASM 宿主、Sidecar 生命周期、插件权限（§6 / §10）。
//!
//! 两层运行模型（§10.1）：
//! - WASM Plugin（默认）：Wasmtime + WASI Component Model + WIT；
//! - Native Sidecar（高权限）：独立进程，受控 JSON-RPC。
//!
//! 治理（§10.5）：Built-in / Signed / Unverified 三档信任级别；
//! Unverified 插件默认不能在 Auto 模式自动执行；插件崩溃不得导致 Runtime 崩溃。
//!
//! TODO(阶段5)：Wasmtime 集成、WIT host 实现（见 `wit/plugin-api`）、
//! 插件安装/回滚、权限差异展示。

pub mod manifest;

pub use manifest::{PluginCapabilities, PluginManifest, PluginPermissions};

use codedock_protocol::TrustLevel;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("manifest 无效: {0}")]
    InvalidManifest(String),
    #[error("插件 {0} 未安装")]
    NotInstalled(String),
    #[error("权限被拒绝: {0}")]
    PermissionDenied(String),
    #[error("插件崩溃（Runtime 不受影响，§10.5）: {0}")]
    Crashed(String),
    #[error("插件执行超时")]
    Timeout,
}

/// 已安装插件的运行时状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Disabled,
    Enabled,
    Crashed,
    Updating,
}

/// 插件实例记录（`plugins` 表 Projection）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginRecord {
    pub manifest: PluginManifest,
    pub state: PluginState,
    pub trust_level: TrustLevel,
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

/// 依据信任级别与 Session 模式判断插件工具是否可自动执行（§10.5）。
pub fn can_auto_execute(trust: TrustLevel, auto_mode: bool) -> bool {
    match trust {
        TrustLevel::Builtin | TrustLevel::Signed => true,
        // Unverified 插件默认不能在 Auto 模式自动执行
        TrustLevel::Unverified => !auto_mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_plugins_blocked_in_auto_mode() {
        assert!(can_auto_execute(TrustLevel::Builtin, true));
        assert!(can_auto_execute(TrustLevel::Signed, true));
        assert!(!can_auto_execute(TrustLevel::Unverified, true));
        assert!(can_auto_execute(TrustLevel::Unverified, false));
    }
}
