//! CodeDock Agent Daemon 启动入口。
//!
//! 职责（§6）：进程生命周期、RPC、客户端连接、模块装配。
//!
//! 阶段 1（§23）交付：Daemon、JSON-RPC、Local IPC、Session Engine、
//! Event Store、SQLite Migration、OpenAI-Compatible Provider、纯对话 Turn 闭环。

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

    /// 工作区根目录（内置工具的文件访问边界，§13.1）。
    #[arg(long, default_value = ".")]
    workspace: String,

    /// Daemon 配置文件（模型 Provider 与预算上限，§11.2/§11.3/§18.3）。
    #[arg(long, default_value = "~/.codedock/config.toml")]
    config: String,
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
    let workspace = expand_tilde(&args.workspace);
    let model_config = config::ModelLayerConfig::load(&expand_tilde(&args.config))?;
    let config = config::DaemonConfig {
        data_dir: data_dir.display().to_string(),
        socket_path: args.socket.clone(),
        model: model_config,
    };
    tracing::info!(?config, "CodeDock daemon 正在启动");

    // ---- 模块装配（§5 架构）----
    // SQLite Event Store（WAL + 显式版本迁移 §18.9）→ 会话 Projection 重建（§17.1）
    // → 密钥注入 SecretStore → Provider 注册表 → Turn 引擎（含幂等缓存重建）。
    // TODO(阶段1)：磁盘 Blob Store；TODO(阶段2+)：plugin-host 装配。
    let runtime = assemble_sqlite(&data_dir, &config.model, &workspace).await?;
    tracing::info!(
        default_provider = %config.model.default_provider,
        providers = ?config.model.providers.keys().collect::<Vec<_>>(),
        tools = "file.read",
        workspace = %workspace.display(),
        "模块装配完成: sqlite_event_store / session_manager / turn_engine / tool_registry / policy_engine / secret_store / provider_registry"
    );

    // ---- Local IPC：Unix Domain Socket（§2）----
    let listener = ipc::bind(&config.socket_path)
        .await
        .with_context(|| format!("绑定 IPC 失败: {}", config.socket_path))?;
    tracing::info!(socket = %config.socket_path, "Local IPC 已就绪，等待客户端连接");

    // ---- 连接循环 + 优雅停机 ----
    let shutdown = tokio::signal::ctrl_c();
    tokio::select! {
        res = ipc::accept_loop(listener, runtime.into()) => {
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
