//! CodeDock Agent Daemon 启动入口。
//!
//! 职责（§6）：进程生命周期、RPC、客户端连接、模块装配。
//!
//! 阶段 1（§23）交付：Daemon、JSON-RPC、Local IPC、Session Engine、
//! Event Store、SQLite Migration、OpenAI-Compatible Provider。

use anyhow::Context as _;
use clap::Parser;
use codedock_agent_daemon::{assemble_sqlite, config, ipc};

#[derive(Parser, Debug)]
#[command(name = "codedock-daemon", version, about = "CodeDock Agent Runtime")]
struct Args {
    /// 数据目录（SQLite、Blob Store、索引）。
    #[arg(long, default_value = "~/.codedock/data")]
    data_dir: String,

    /// Local IPC 监听地址（Unix Domain Socket 路径）。
    #[arg(long, default_value = "/tmp/codedock.sock")]
    socket: String,
}

/// 展开 `~` 前缀为用户主目录（阶段 1 只处理 Unix 约定）。
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(path));
    }
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| std::path::PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| std::path::PathBuf::from(path)),
        None => std::path::PathBuf::from(path),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 结构化日志：RUST_LOG=debug 调整级别
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let data_dir = expand_tilde(&args.data_dir);
    let config = config::DaemonConfig {
        data_dir: data_dir.display().to_string(),
        socket_path: args.socket.clone(),
    };
    tracing::info!(?config, "CodeDock daemon 正在启动");

    // ---- 模块装配（§5 架构）----
    // SQLite Event Store（WAL + 显式版本迁移 §18.9）+ 事件流重建 Projection（§17.1）。
    // TODO(阶段1)：磁盘 Blob Store + OS Keychain；
    // TODO(阶段1/2)：装配 model-gateway / tool-runtime / plugin-host。
    let (_event_store, runtime) = assemble_sqlite(&data_dir).await?;
    let _blob_store = codedock_artifact_store::InMemoryBlobStore::new();
    let _secret_store = codedock_secret_store::InMemorySecretStore::new();
    let _policy = codedock_policy_engine::DefaultPolicyEngine;

    tracing::info!(
        "模块装配完成: sqlite_event_store / session_runtime / blob_store / secret_store / policy_engine"
    );

    // ---- Local IPC：Unix Domain Socket（§2）----
    let listener = ipc::bind(&config.socket_path)
        .await
        .with_context(|| format!("绑定 IPC 失败: {}", config.socket_path))?;
    tracing::info!(socket = %config.socket_path, "Local IPC 已就绪，等待客户端连接");

    // ---- 连接循环 + 优雅停机 ----
    let shutdown = tokio::signal::ctrl_c();
    tokio::select! {
        res = ipc::accept_loop(listener, runtime.clone()) => {
            res.context("IPC accept loop 异常退出")?;
        }
        _ = shutdown => {
            tracing::info!("收到 Ctrl-C，开始优雅停机");
        }
    }

    // TODO(阶段1)：停机前 flush Event Store、标记悬挂 Tool Call 为 interrupted（§20.1）
    tracing::info!("CodeDock daemon 已退出");
    Ok(())
}
