//! Remote Gateway：设备配对、端到端加密、角色、撤销（§6 / §9.2 / §16）。
//!
//! 角色（§9.2）：
//! - Viewer：只读；Approver：可批准/拒绝等待中的操作；
//! - Controller：可追加指令、暂停、继续、终止；Owner：可管理设备、模型、插件和策略。
//!
//! 远程命令均带设备 ID、单调计数器、时间戳和签名，防止重放；设备可随时撤销。
//!
//! TODO(阶段6)：QR 配对流程、E2EE 通道（使用经过审计的加密库与标准协议组合，
//! 不自研密码学算法，§16.2）、中继协议（见 `services/relay`）。

use codedock_protocol::DeviceId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("设备已撤销")]
    DeviceRevoked,
    #[error("重放攻击：计数器回退或时间戳过期")]
    ReplayDetected,
    #[error("签名验证失败")]
    BadSignature,
    #[error("角色权限不足: {0:?}")]
    InsufficientRole(Role),
    #[error("配对码无效或已过期")]
    PairingExpired,
}

/// 设备角色（§9.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Approver,
    Controller,
    Owner,
}

impl Role {
    /// 角色能力等级：Viewer < Approver < Controller < Owner。
    pub const fn rank(self) -> u8 {
        match self {
            Role::Viewer => 0,
            Role::Approver => 1,
            Role::Controller => 2,
            Role::Owner => 3,
        }
    }

    pub fn can(&self, required: Role) -> bool {
        self.rank() >= required.rank()
    }
}

/// 已配对设备记录（`devices` 表 Projection）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub name: String,
    pub role: Role,
    pub public_key: String,
    /// 最近一次见到的命令计数器（防重放：必须单调递增，§9.2）。
    pub last_counter: u64,
    pub revoked: bool,
    pub paired_at: chrono::DateTime<chrono::Utc>,
}

/// 一次性配对二维码内容（§16.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingPayload {
    /// 桌面端公钥。
    pub desktop_public_key: String,
    /// 短期配对 Token。
    pub pairing_token: String,
    /// 中继地址。
    pub relay_url: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// 远程命令信封：设备 ID + 单调计数器 + 时间戳 + 签名（§9.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteCommand {
    pub device_id: DeviceId,
    pub counter: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub params: serde_json::Value,
    pub signature: String,
}

/// 校验远程命令的防重放基本条件（签名与 E2EE 由上层完成）。
pub fn check_replay(
    device: &DeviceRecord,
    cmd: &RemoteCommand,
    max_clock_skew_ms: i64,
) -> Result<(), RemoteError> {
    if device.revoked {
        return Err(RemoteError::DeviceRevoked);
    }
    if cmd.counter <= device.last_counter {
        return Err(RemoteError::ReplayDetected);
    }
    let skew = (cmd.timestamp - chrono::Utc::now())
        .num_milliseconds()
        .abs();
    if skew > max_clock_skew_ms {
        return Err(RemoteError::ReplayDetected);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceRecord {
        DeviceRecord {
            device_id: DeviceId::generate(),
            name: "Pixel".into(),
            role: Role::Approver,
            public_key: "pk".into(),
            last_counter: 10,
            revoked: false,
            paired_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn replay_counter_must_increase() {
        let d = device();
        let cmd = |c: u64| RemoteCommand {
            device_id: d.device_id,
            counter: c,
            timestamp: chrono::Utc::now(),
            method: "approval.decide".into(),
            params: serde_json::json!({}),
            signature: "sig".into(),
        };
        assert!(matches!(
            check_replay(&d, &cmd(9), 5_000),
            Err(RemoteError::ReplayDetected)
        ));
        assert!(check_replay(&d, &cmd(11), 5_000).is_ok());
    }

    #[test]
    fn revoked_device_rejected() {
        let mut d = device();
        d.revoked = true;
        let cmd = RemoteCommand {
            device_id: d.device_id,
            counter: 11,
            timestamp: chrono::Utc::now(),
            method: "x".into(),
            params: serde_json::json!({}),
            signature: "sig".into(),
        };
        assert!(matches!(
            check_replay(&d, &cmd, 5_000),
            Err(RemoteError::DeviceRevoked)
        ));
    }

    #[test]
    fn role_ranking() {
        assert!(Role::Owner.can(Role::Controller));
        assert!(Role::Approver.can(Role::Approver));
        assert!(!Role::Viewer.can(Role::Approver));
    }
}
