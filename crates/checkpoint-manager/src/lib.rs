//! Checkpoint Manager：变更前快照、恢复、工作区一致性（§6 / §20.1）。
//!
//! 规则：
//! - 高风险修改前创建 Checkpoint（§3.5）；
//! - 所有 Patch 应用前校验源文件 Hash，避免覆盖用户刚修改的内容（§12.2 / §18.2）；
//! - 文件修改前创建 Checkpoint；崩溃后可恢复到最近 Checkpoint（§20.1）。
//!
//! TODO(阶段2)：基于 Git object / 内容寻址 Blob 的真实快照存储。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use codedock_protocol::SessionId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint 不存在: {0}")]
    NotFound(String),
    #[error("快照创建失败: {0}")]
    SnapshotFailed(String),
    #[error("恢复冲突：文件在快照后被外部修改（§18.2）: {0}")]
    Conflicted(String),
}

/// 单文件快照记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: String,
    /// 快照时的内容哈希。
    pub sha256: String,
    /// 内容在 Blob Store 中的引用（TODO 阶段2 接入 artifact-store）。
    pub blob_ref: Option<String>,
}

/// Checkpoint。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: codedock_protocol::EventId,
    pub session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub description: String,
    pub files: Vec<FileSnapshot>,
}

impl Checkpoint {
    /// 计算文件内容哈希的辅助。
    pub fn hash_content(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex::encode(h.finalize())
    }
}

/// Checkpoint 管理抽象。
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// 在变更前创建快照。
    async fn create(
        &self,
        session_id: SessionId,
        description: &str,
        files: Vec<FileSnapshot>,
    ) -> Result<Checkpoint, CheckpointError>;

    /// 恢复到指定 Checkpoint；若当前文件 Hash 与快照不一致则返回冲突（§18.2，
    /// 禁止"最后写入者覆盖"）。
    async fn restore(
        &self,
        checkpoint_id: &codedock_protocol::EventId,
    ) -> Result<(), CheckpointError>;

    /// 列出 Session 的所有 Checkpoint（时间倒序）。
    async fn list(&self, session_id: SessionId) -> Result<Vec<Checkpoint>, CheckpointError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            Checkpoint::hash_content(b"abc"),
            Checkpoint::hash_content(b"abc")
        );
        assert_ne!(
            Checkpoint::hash_content(b"abc"),
            Checkpoint::hash_content(b"abd")
        );
    }
}
