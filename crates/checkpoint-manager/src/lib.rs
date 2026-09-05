//! Checkpoint Manager：变更前快照、恢复、工作区一致性（§6 / §20.1）。
//!
//! 规则：
//! - 高风险修改前创建 Checkpoint（§3.5）：TurnEngine 在执行任何声明
//!   `fs.write` 权限的工具前，对目标文件调用 [`CheckpointStore::create`]；
//! - 恢复（§24 一键回滚）：显式用户操作，`force = true` 覆盖快照后的变更；
//!   非强制恢复遇到快照后变更的文件返回 [`CheckpointError::Conflicted`]（§18.2，
//!   禁止"最后写入者覆盖"）；
//! - 快照内容按 SHA-256 寻址存盘，SQLite/事件流只存索引（§17.2 精神；
//!   TODO：接入统一 Blob Store 与保留期清理）。

use std::path::{Path, PathBuf};

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
    /// 内容副本在 checkpoint 目录内的引用。
    pub blob_ref: Option<String>,
}

/// Checkpoint。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub description: String,
    pub files: Vec<FileSnapshot>,
}

/// Checkpoint 管理抽象。
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// 在变更前创建快照：读取 `workspace` 下 `rel_paths` 指向的文件并持久化副本。
    async fn create(
        &self,
        session_id: SessionId,
        description: &str,
        workspace: &Path,
        rel_paths: &[String],
    ) -> Result<Checkpoint, CheckpointError>;

    /// 恢复到指定 Checkpoint。
    ///
    /// `force = false`：当前文件 Hash 与快照不一致即返回冲突（§18.2）；
    /// `force = true`：显式回滚，覆盖快照后的变更（§24 一键回滚）。
    /// 返回被恢复的文件路径列表。
    async fn restore(
        &self,
        checkpoint_id: &str,
        workspace: &Path,
        force: bool,
    ) -> Result<Vec<String>, CheckpointError>;

    /// 列出 Session 的所有 Checkpoint（时间正序）。
    async fn list(&self, session_id: SessionId) -> Result<Vec<Checkpoint>, CheckpointError>;
}

/// 磁盘实现：`<root>/<checkpoint_id>/manifest.json` + 按 SHA 寻址的内容副本。
pub struct DiskCheckpointStore {
    root: PathBuf,
}

impl DiskCheckpointStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn checkpoint_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex::encode(h.finalize())
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    checkpoint: Checkpoint,
}

#[async_trait]
impl CheckpointStore for DiskCheckpointStore {
    async fn create(
        &self,
        session_id: SessionId,
        description: &str,
        workspace: &Path,
        rel_paths: &[String],
    ) -> Result<Checkpoint, CheckpointError> {
        let id = format!("cp_{}", uuid::Uuid::now_v7().simple());
        let dir = self.checkpoint_dir(&id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;

        let mut files = Vec::new();
        for rel in rel_paths {
            let abs = workspace.join(rel);
            let data = tokio::fs::read(&abs)
                .await
                .map_err(|e| CheckpointError::SnapshotFailed(format!("读取 {rel} 失败: {e}")))?;
            let sha = Self::sha256_hex(&data);
            // 内容按 sha 寻址：同内容去重（§17.2）。
            let blob_ref = format!("{}.blob", &sha[..16.min(sha.len())]);
            let blob_path = self.checkpoint_dir(&id).join(&blob_ref);
            if !tokio::fs::try_exists(&blob_path)
                .await
                .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?
            {
                tokio::fs::write(&blob_path, &data)
                    .await
                    .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
            }
            files.push(FileSnapshot {
                path: rel.clone(),
                sha256: sha,
                blob_ref: Some(format!("{id}/{blob_ref}")),
            });
        }

        let checkpoint = Checkpoint {
            id: id.clone(),
            session_id,
            created_at: Utc::now(),
            description: description.to_string(),
            files,
        };
        let manifest = serde_json::to_vec_pretty(&Manifest {
            checkpoint: checkpoint.clone(),
        })
        .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
        tokio::fs::write(dir.join("manifest.json"), manifest)
            .await
            .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
        Ok(checkpoint)
    }

    async fn restore(
        &self,
        checkpoint_id: &str,
        workspace: &Path,
        force: bool,
    ) -> Result<Vec<String>, CheckpointError> {
        let manifest_path = self.checkpoint_dir(checkpoint_id).join("manifest.json");
        let raw = tokio::fs::read(&manifest_path)
            .await
            .map_err(|_| CheckpointError::NotFound(checkpoint_id.to_string()))?;
        let manifest: Manifest = serde_json::from_slice(&raw)
            .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;

        let mut restored = Vec::new();
        for file in &manifest.checkpoint.files {
            let blob = self.root.join(file.blob_ref.as_deref().ok_or_else(|| {
                CheckpointError::SnapshotFailed(format!("{} 缺少内容副本", file.path))
            })?);
            let data = tokio::fs::read(&blob)
                .await
                .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;

            let abs = workspace.join(&file.path);
            if !force {
                // 非强制：快照后的变更视为冲突（§18.2）。
                if let Ok(current) = tokio::fs::read(&abs).await {
                    if Self::sha256_hex(&current) != file.sha256 {
                        return Err(CheckpointError::Conflicted(file.path.clone()));
                    }
                }
            }
            if let Some(parent) = abs.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
            }
            // 临时文件 + 原子替换（§13.1）。
            let tmp = abs.with_extension("cdckpt-tmp");
            tokio::fs::write(&tmp, &data)
                .await
                .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
            tokio::fs::rename(&tmp, &abs)
                .await
                .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
            restored.push(file.path.clone());
        }
        Ok(restored)
    }

    async fn list(&self, session_id: SessionId) -> Result<Vec<Checkpoint>, CheckpointError> {
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.root)
            .await
            .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| CheckpointError::SnapshotFailed(e.to_string()))?
        {
            let manifest_path = entry.path().join("manifest.json");
            let Ok(raw) = tokio::fs::read(&manifest_path).await else {
                continue;
            };
            if let Ok(manifest) = serde_json::from_slice::<Manifest>(&raw) {
                if manifest.checkpoint.session_id == session_id {
                    out.push(manifest.checkpoint);
                }
            }
        }
        out.sort_by_key(|c| c.created_at);
        Ok(out)
    }
}

/// 计算内容 SHA-256（hex）。
pub fn hash_content(data: &[u8]) -> String {
    DiskCheckpointStore::sha256_hex(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "codedock-ckpt-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ))
    }

    async fn workspace() -> (PathBuf, DiskCheckpointStore) {
        let ws = std::env::temp_dir().join(format!(
            "codedock-ckpt-ws-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("a.txt"), "原始内容")
            .await
            .unwrap();
        (ws, DiskCheckpointStore::new(store_dir()))
    }

    #[tokio::test]
    async fn create_snapshot_and_restore_with_conflict_rules() {
        let (ws, store) = workspace().await;
        let sid = SessionId::generate();

        let cp = store
            .create(sid, "patch 前", &ws, &["a.txt".to_string()])
            .await
            .unwrap();
        assert_eq!(cp.files.len(), 1);
        assert_eq!(cp.files[0].sha256, hash_content("原始内容".as_bytes()));

        // 快照后修改 → 非强制恢复冲突（§18.2）
        tokio::fs::write(ws.join("a.txt"), "被修改了")
            .await
            .unwrap();
        let err = store.restore(&cp.id, &ws, false).await.unwrap_err();
        assert!(matches!(err, CheckpointError::Conflicted(p) if p == "a.txt"));

        // 强制恢复 = 一键回滚（§24）
        let restored = store.restore(&cp.id, &ws, true).await.unwrap();
        assert_eq!(restored, vec!["a.txt".to_string()]);
        let back = tokio::fs::read_to_string(ws.join("a.txt")).await.unwrap();
        assert_eq!(back, "原始内容");

        // 再次恢复：当前 == 快照 → 非强制也允许
        store.restore(&cp.id, &ws, false).await.unwrap();

        assert_eq!(store.list(sid).await.unwrap().len(), 1);
        assert!(store.list(SessionId::generate()).await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn restore_unknown_checkpoint_is_not_found() {
        let (ws, store) = workspace().await;
        let err = store.restore("cp_missing", &ws, true).await.unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound(_)));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
