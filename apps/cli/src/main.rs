//! CodeDock CLI。
//!
//! 通过 Local IPC（Unix Domain Socket）以换行分隔 JSON-RPC 2.0 与 Daemon 通信。
//! 所有状态变更命令自动携带 `idempotency_key`（§8.2.6）。

use clap::{Parser, Subcommand};
use codedock_protocol::{JsonRpcId, JsonRpcRequest, JsonRpcResponse};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "codedock", version, about = "CodeDock Coding Agent CLI")]
struct Cli {
    /// Daemon 的 Local IPC socket 路径。
    #[arg(long, default_value = "/tmp/codedock.sock")]
    socket: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 创建并启动一个 Session。
    Create {
        /// Session 模式：ask | plan | edit | auto。
        #[arg(long, default_value = "ask")]
        mode: String,
        /// 用户任务描述。
        task: String,
    },
    /// 查看会话状态。
    Status { session_id: String },
    /// 暂停会话。
    Pause { session_id: String },
    /// 恢复会话。
    Resume { session_id: String },
    /// 取消会话。
    Cancel { session_id: String },
    /// 切换会话模式。
    Mode {
        session_id: String,
        /// ask | plan | edit | auto
        #[arg(long)]
        mode: String,
    },
    /// 重放会话事件流（默认只含 Durable 事件，§8.2.5）。
    Events {
        session_id: String,
        /// 从该 sequence 之后开始重放。
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
}

/// 每次调用生成新的幂等键（§8.2.6）。
fn new_key() -> String {
    Uuid::now_v7().to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let (method, params) = match &cli.command {
        Commands::Create { mode, task } => (
            "session.create",
            json!({ "mode": mode, "task": task, "idempotency_key": new_key() }),
        ),
        Commands::Status { session_id } => ("session.status", json!({ "session_id": session_id })),
        Commands::Pause { session_id } => (
            "session.pause",
            json!({ "session_id": session_id, "idempotency_key": new_key() }),
        ),
        Commands::Resume { session_id } => (
            "session.resume",
            json!({ "session_id": session_id, "idempotency_key": new_key() }),
        ),
        Commands::Cancel { session_id } => (
            "session.cancel",
            json!({ "session_id": session_id, "idempotency_key": new_key() }),
        ),
        Commands::Mode { session_id, mode } => (
            "session.mode",
            json!({ "session_id": session_id, "mode": mode, "idempotency_key": new_key() }),
        ),
        Commands::Events {
            session_id,
            after,
            limit,
        } => (
            "session.events",
            json!({ "session_id": session_id, "after_sequence": after, "durable_only": true, "limit": limit }),
        ),
    };

    let result = call(&cli.socket, method, params).await?;

    match method {
        "session.create" => {
            println!(
                "session {} ({}, {})",
                result["session_id"].as_str().unwrap_or("?"),
                result["mode"].as_str().unwrap_or("?"),
                result["status"].as_str().unwrap_or("?"),
            );
        }
        "session.events" => {
            let empty: Vec<Value> = Vec::new();
            let events = result["events"].as_array().unwrap_or(&empty);
            if events.is_empty() {
                println!("(无事件)");
            }
            for ev in events {
                println!(
                    "#{} {} [{}] {}",
                    ev["sequence"],
                    ev["event_type"].as_str().unwrap_or("?"),
                    ev["durability"].as_str().unwrap_or("?"),
                    ev["occurred_at"].as_str().unwrap_or("?"),
                );
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(&result)?),
    }
    Ok(())
}

/// 连接 Daemon 并执行一次 JSON-RPC 调用，返回 `result`；RPC 错误转为 Err。
async fn call(socket: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("连接 daemon 失败（{socket}）: {e}"))?;

    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: JsonRpcId::String(format!("cli-{}", Uuid::now_v7())),
        method: method.into(),
        params: Some(params),
    };
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    stream.write_all(&line).await?;

    let mut buf = Vec::new();
    BufReader::new(stream).read_until(b'\n', &mut buf).await?;
    if buf.is_empty() {
        anyhow::bail!("daemon 关闭了连接");
    }
    let resp: JsonRpcResponse = serde_json::from_slice(&buf)?;
    if let Some(err) = resp.error {
        anyhow::bail!("RPC 错误 {}: {}", err.code, err.message);
    }
    resp.result
        .ok_or_else(|| anyhow::anyhow!("响应缺少 result"))
}
