//! 核心实体标识符，统一使用 UUIDv7（§8.1）。

use std::fmt;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            /// 生成新的 UUIDv7 标识。
            pub fn generate() -> Self {
                Self(uuid::Uuid::now_v7())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(
    /// 会话标识。
    SessionId
);
define_id!(
    /// 回合标识（一次用户任务的一次执行单元）。
    TurnId
);
define_id!(
    /// 事件标识（Event Store 内 Append-only）。
    EventId
);
define_id!(
    /// RPC / 模型请求标识。
    RequestId
);
define_id!(
    /// 工具调用标识。
    ToolCallId
);
define_id!(
    /// 上下文快照标识。
    SnapshotId
);
define_id!(
    /// 产物标识（Patch、日志、截图、测试报告等）。
    ArtifactId
);
define_id!(
    /// 审批标识。
    ApprovalId
);
define_id!(
    /// 远程设备标识。
    DeviceId
);
define_id!(
    /// 插件标识。
    PluginId
);
define_id!(
    /// 模型请求标识（与 Context Snapshot 一对一，§8.4.10）。
    ModelRequestId
);
define_id!(
    /// 幂等命令标识（§8.2.6）。
    CommandId
);

/// Blob 标识，格式为 `blob_<id>`（§8.1）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        debug_assert!(id.starts_with("blob_"), "BlobId 必须以 blob_ 开头");
        Self(id)
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 操作摘要：审批的唯一对象（§8.3.5），格式 `sha256:<hex>`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct OperationDigest(pub String);

impl OperationDigest {
    pub fn from_sha256_hex(hex: impl Into<String>) -> Self {
        Self(format!("sha256:{}", hex.into()))
    }
}

impl fmt::Display for OperationDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_uuid_v7() {
        let id = SessionId::generate();
        assert_eq!(id.0.get_version_num(), 7);
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = SessionId::generate();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
    }
}
