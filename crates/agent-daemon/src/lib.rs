//! CodeDock Agent Daemon 库：进程生命周期、RPC、客户端连接、模块装配（§6）。
//!
//! 拆分为 lib + bin：bin（`codedock-daemon`）只负责 CLI 参数与启动，
//! 可测试的装配与 IPC 逻辑在 lib 中。

pub mod config;
pub mod ipc;
pub mod rpc;

use std::sync::Arc;

use codedock_event_store::InMemoryEventStore;
use codedock_session_engine::{EventSourcedSessionManager, SessionManager};

/// 装配一个完整的 Runtime（当前为内存实现；阶段 1 换 SQLite + 磁盘 Blob Store + Keychain）。
pub fn assemble_in_memory() -> (Arc<InMemoryEventStore>, Arc<dyn SessionManager>) {
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn SessionManager> =
        Arc::new(EventSourcedSessionManager::new(event_store.clone()));
    (event_store, runtime)
}
