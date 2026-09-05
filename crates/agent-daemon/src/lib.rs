//! CodeDock Agent Daemon 库：进程生命周期、RPC、客户端连接、模块装配（§6）。
//!
//! 拆分为 lib + bin：bin（`codedock-daemon`）只负责 CLI 参数与启动，
//! 可测试的装配与 IPC 逻辑在 lib 中。

pub mod config;
pub mod ipc;
pub mod rpc;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use codedock_event_store::{InMemoryEventStore, SqliteEventStore};
use codedock_session_engine::{EventSourcedSessionManager, SessionManager};

/// 装配一个内存 Runtime（开发 / 测试用）。
pub fn assemble_in_memory() -> (Arc<InMemoryEventStore>, Arc<dyn SessionManager>) {
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn SessionManager> =
        Arc::new(EventSourcedSessionManager::new(event_store.clone()));
    (event_store, runtime)
}

/// 生产装配：SQLite Event Store（WAL + 显式版本迁移，§18.9），
/// 并从磁盘事件流重建会话 Projection 与幂等缓存（§17.1）。
pub async fn assemble_sqlite(
    data_dir: &Path,
) -> anyhow::Result<(Arc<SqliteEventStore>, Arc<dyn SessionManager>)> {
    let event_store = Arc::new(
        SqliteEventStore::open(data_dir.join("events.db"))
            .await
            .context("打开 SQLite Event Store 失败")?,
    );
    let runtime: Arc<dyn SessionManager> = Arc::new(
        EventSourcedSessionManager::restore(event_store.clone())
            .await
            .context("重建会话 Projection 失败")?,
    );
    Ok((event_store, runtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{SessionMode, SessionStatus};

    #[tokio::test]
    async fn sqlite_assembly_restores_state_across_restart() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-daemon-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));

        let session_id;
        {
            let (_store, runtime) = assemble_sqlite(&dir).await.unwrap();
            let info = runtime
                .create(
                    SessionMode::Plan,
                    Some("持久化任务".into()),
                    Some("k1".into()),
                )
                .await
                .unwrap();
            runtime.pause(info.session_id, None).await.unwrap();
            session_id = info.session_id;
        }
        // 模拟重启：重新装配，状态与幂等缓存都应恢复。
        {
            let (_store, runtime) = assemble_sqlite(&dir).await.unwrap();
            let info = runtime.status(session_id).await.unwrap();
            assert_eq!(info.status, SessionStatus::Paused);

            let again = runtime
                .create(
                    SessionMode::Plan,
                    Some("持久化任务".into()),
                    Some("k1".into()),
                )
                .await
                .unwrap();
            assert_eq!(again.session_id, session_id, "幂等键跨重启仍命中同一会话");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
