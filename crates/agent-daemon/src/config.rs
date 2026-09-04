//! Daemon 配置。

/// 运行时配置（TODO 阶段1：从 `~/.codedock/config.toml` 加载并支持模型路由 §11.3）。
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// 数据目录（SQLite、Blob Store、索引）；阶段 1 接入持久化后读取。
    #[allow(dead_code)]
    pub data_dir: String,
    pub socket_path: String,
}
