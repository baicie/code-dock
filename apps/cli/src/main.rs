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
    /// 发送一条用户消息并等待本轮对话完成（同步返回助手最终回复）。
    Message { session_id: String, text: String },
    /// 一键回滚到指定 Checkpoint（§24；覆盖快照后的变更）。
    Rollback {
        session_id: String,
        /// 用 `codedock events <session_id>` 查找 checkpoint.created 事件获取 id。
        checkpoint_id: String,
    },
    /// 裁决等待中的工具审批（§9.1：approve_once / deny）。
    Approve {
        session_id: String,
        tool_call_id: String,
        /// approve_once | deny
        #[arg(long)]
        response: String,
    },
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
    /// 实时跟踪会话事件流（先补发 durable，再推送实时事件；Ctrl-C 退出）。
    Follow {
        session_id: String,
        /// 从该 sequence 之后开始补发。
        #[arg(long, default_value_t = 0)]
        after: u64,
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

    if let Commands::Follow { session_id, after } = &cli.command {
        return run_follow(&cli.socket, session_id, *after).await;
    }

    let (method, params) = match &cli.command {
        // Follow 已在函数开头分支处理。
        Commands::Follow { .. } => unreachable!("Follow 已在上方分支处理"),
        Commands::Create { mode, task } => (
            "session.create",
            json!({ "mode": mode, "task": task, "idempotency_key": new_key() }),
        ),
        Commands::Status { session_id } => ("session.status", json!({ "session_id": session_id })),
        Commands::Message { session_id, text } => (
            "session.message",
            json!({ "session_id": session_id, "text": text, "idempotency_key": new_key() }),
        ),
        Commands::Rollback {
            session_id,
            checkpoint_id,
        } => (
            "checkpoint.restore",
            json!({
                "session_id": session_id,
                "checkpoint_id": checkpoint_id,
                "idempotency_key": new_key()
            }),
        ),
        Commands::Approve {
            session_id,
            tool_call_id,
            response,
        } => (
            "tool.approve",
            json!({
                "session_id": session_id,
                "tool_call_id": tool_call_id,
                "response": response,
                "idempotency_key": new_key()
            }),
        ),
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
        "session.message" => {
            if let Some(text) = result["text"].as_str() {
                println!("{text}");
            }
            let status = result["status"].as_str().unwrap_or("completed");
            match status {
                "waiting_approval" => {
                    if let Some(id) = result["pending_tool_call_id"].as_str() {
                        println!(
                            "⏸ 等待审批: tool_call_id={id}\n  批准: codedock approve {sid} {id} --response approve_once\n  拒绝: codedock approve {sid} {id} --response deny",
                            sid = result["session_id"].as_str().unwrap_or("?"),
                        );
                    } else {
                        println!("⏸ 等待审批（用 `codedock events` 查看 tool_call_id）");
                    }
                }
                _ => println!(
                    "--- turn {} | tokens in {} / out {} | seq {}",
                    result["turn_id"].as_str().unwrap_or("?"),
                    result["input_tokens"],
                    result["output_tokens"],
                    result["latest_sequence"],
                ),
            }
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

/// 实时跟踪会话事件流（§8.2.5）。
async fn run_follow(socket: &str, session_id: &str, after: u64) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("连接 daemon 失败（{socket}）: {e}"))?;
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: JsonRpcId::String(format!("follow-{}", Uuid::now_v7())),
        method: "session.subscribe".into(),
        params: Some(json!({ "session_id": session_id, "after_sequence": after })),
    };
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    stream.write_all(&line).await?;

    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        reader.read_until(b'\n', &mut buf).await?;
        if buf.is_empty() {
            anyhow::bail!("daemon 关闭了连接");
        }
        let v: Value = serde_json::from_slice(&buf)?;
        if let Some(err) = v.get("error") {
            anyhow::bail!("RPC 错误: {err}");
        }
        if v.get("method").and_then(Value::as_str) == Some("session.resync") {
            println!("⚠ 事件落后，请用 `codedock events` 重新对齐");
            continue;
        }
        if v.get("method").and_then(Value::as_str) == Some("session.event") {
            let ev = &v["params"];
            println!(
                "#{} {} [{}] {}",
                ev["sequence"],
                ev["event_type"].as_str().unwrap_or("?"),
                ev["durability"].as_str().unwrap_or("?"),
                ev["occurred_at"].as_str().unwrap_or("?"),
            );
            continue;
        }
        // 订阅响应：补发的 durable 事件
        if let Some(events) = v["result"]["events"].as_array() {
            println!("── 补发 {} 条 durable 事件 ──", events.len());
            for ev in events {
                println!(
                    "#{} {} [{}]",
                    ev["sequence"],
                    ev["event_type"].as_str().unwrap_or("?"),
                    ev["durability"].as_str().unwrap_or("?"),
                );
            }
            println!("── 实时跟踪中（Ctrl-C 退出）──");
        }
    }
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
