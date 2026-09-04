//! Artifact Store：Patch、日志、截图、模型载荷、测试报告（§6 / §17.2）。
//!
//! Blob Store 规则（§17.2）：
//! - 文件名基于 SHA-256，去重存储；
//! - SQLite 仅保存索引和元数据；
//! - 可配置保留期和最大容量；
//! - 删除 Session 时执行引用计数清理。
//!
//! TODO(阶段1)：磁盘目录布局（`blobs/aa/bb/<sha256>`）+ SQLite 索引。

use async_trait::async_trait;
use codedock_protocol::{ArtifactId, BlobId};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("blob 不存在: {0}")]
    NotFound(String),
    #[error("IO 错误: {0}")]
    Io(String),
    #[error("超出容量上限（§17.2）")]
    CapacityExceeded,
}

/// Blob 元数据（§8.1 大对象引用）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub blob_id: BlobId,
    pub sha256: String,
    pub size: u64,
    pub mime_type: String,
}

/// Content-addressed Blob Store 抽象。
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// 写入内容，返回引用；相同内容（sha256 相同）去重。
    async fn put(&self, data: Vec<u8>, mime_type: &str) -> Result<BlobRef, BlobError>;

    async fn get(&self, blob_id: &BlobId) -> Result<Vec<u8>, BlobError>;

    /// 引用计数 +1 / -1（§17.2 引用计数清理）。
    async fn retain(&self, blob_id: &BlobId, by: u32) -> Result<(), BlobError>;

    async fn release(&self, blob_id: &BlobId, by: u32) -> Result<(), BlobError>;
}

/// 构造产物 URI（§8.3.8）：`agent://artifacts/<id>`。
pub fn artifact_uri(id: &ArtifactId) -> String {
    format!("agent://artifacts/{id}")
}

/// 计算 SHA-256 十六进制摘要。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 内存存储条目：内容 + MIME + 引用计数。
type BlobEntry = (Vec<u8>, String, u32);

/// 内存 Blob Store（开发 / 测试用；TODO 阶段1 替换为磁盘实现）。
#[derive(Default)]
pub struct InMemoryBlobStore {
    inner: std::sync::Mutex<std::collections::HashMap<BlobId, BlobEntry>>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BlobStore for InMemoryBlobStore {
    async fn put(&self, data: Vec<u8>, mime_type: &str) -> Result<BlobRef, BlobError> {
        let sha = sha256_hex(&data);
        let blob_id = BlobId::new(format!("blob_{sha}"));
        let mut map = self.inner.lock().expect("blob store poisoned");
        let entry = map
            .entry(blob_id.clone())
            .or_insert_with(|| (data, mime_type.to_string(), 0));
        let size = entry.0.len() as u64;
        Ok(BlobRef {
            blob_id,
            sha256: sha,
            size,
            mime_type: mime_type.to_string(),
        })
    }

    async fn get(&self, blob_id: &BlobId) -> Result<Vec<u8>, BlobError> {
        let map = self.inner.lock().expect("blob store poisoned");
        map.get(blob_id)
            .map(|(data, _, _)| data.clone())
            .ok_or_else(|| BlobError::NotFound(blob_id.to_string()))
    }

    async fn retain(&self, blob_id: &BlobId, by: u32) -> Result<(), BlobError> {
        let mut map = self.inner.lock().expect("blob store poisoned");
        let entry = map
            .get_mut(blob_id)
            .ok_or_else(|| BlobError::NotFound(blob_id.to_string()))?;
        entry.2 += by;
        Ok(())
    }

    async fn release(&self, blob_id: &BlobId, by: u32) -> Result<(), BlobError> {
        let mut map = self.inner.lock().expect("blob store poisoned");
        let entry = map
            .get_mut(blob_id)
            .ok_or_else(|| BlobError::NotFound(blob_id.to_string()))?;
        entry.2 = entry.2.saturating_sub(by);
        if entry.2 == 0 {
            map.remove(blob_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn content_addressed_dedup() {
        let store = InMemoryBlobStore::new();
        let a = store.put(b"hello".to_vec(), "text/plain").await.unwrap();
        let b = store.put(b"hello".to_vec(), "text/plain").await.unwrap();
        assert_eq!(a.blob_id, b.blob_id, "相同内容去重");
        assert_eq!(a.sha256, sha256_hex(b"hello"));
        assert_eq!(store.get(&a.blob_id).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn refcount_cleanup() {
        let store = InMemoryBlobStore::new();
        let r = store.put(b"data".to_vec(), "text/plain").await.unwrap();
        store.retain(&r.blob_id, 2).await.unwrap();
        store.release(&r.blob_id, 1).await.unwrap();
        store.get(&r.blob_id).await.unwrap(); // 仍存在
        store.release(&r.blob_id, 1).await.unwrap();
        assert!(store.get(&r.blob_id).await.is_err(), "引用清零后删除");
    }
}
