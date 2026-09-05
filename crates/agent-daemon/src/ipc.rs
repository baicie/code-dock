//! Local IPC：Unix Domain Socket / Windows Named Pipe（§2）。
//!
//! 帧：换行分隔的 JSON-RPC 2.0（一行一请求，一行一响应/通知）。
//!
//! `session.subscribe`（§8.2.5）：订阅请求的响应携带补发的 Durable Event；
//! 此后连接上的实时事件以 JSON-RPC notification 推送：
//!
//! ```json
//! {"jsonrpc":"2.0","method":"session.event","params":{ ...EventEnvelope... }}
//! ```
//!
//! 订阅者落后超过总线缓冲时收到 `session.resync` 通知，应以
//! `session.events` + `after_sequence` 重新对齐。
//! TODO(阶段1)：Windows Named Pipe（§18.8）。

use std::sync::Arc;

use anyhow::Context as _;
use codedock_protocol::{EventEnvelope, JsonRpcRequest, JsonRpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::{Runtime, rpc};

/// 绑定 Unix Domain Socket（Windows Named Pipe 在阶段 1 补充，§18.8）。
pub async fn bind(socket_path: &str) -> anyhow::Result<UnixListener> {
    // 清理残留 socket 文件
    let _ = std::fs::remove_file(socket_path);
    let listener =
        UnixListener::bind(socket_path).with_context(|| format!("无法绑定 {socket_path}"))?;
    Ok(listener)
}

/// 接受连接并逐连接处理（并发安全；runtime 内部自带同步）。
pub async fn accept_loop(listener: UnixListener, runtime: Arc<Runtime>) -> anyhow::Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        let rt = runtime.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(rt, stream).await {
                tracing::warn!(%err, "客户端连接处理失败");
            }
        });
    }
}

/// 换行分隔 JSON-RPC 服务循环：请求响应与订阅推送在同一连接上复用。
async fn serve(runtime: Arc<Runtime>, stream: UnixStream) -> anyhow::Result<()> {
    tracing::debug!("客户端已连接");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).split(b'\n');
    let mut subscriber: Option<broadcast::Receiver<EventEnvelope>> = None;

    loop {
        tokio::select! {
            biased;

            // 实时事件推送（仅订阅后启用；`if` 前置条件防止空轮询）。
            pushed = async {
                match subscriber.as_mut() {
                    Some(rx) => Some(rx.recv().await),
                    None => None,
                }
            }, if subscriber.is_some() => {
                match pushed.expect("前置条件已保证 Some") {
                    Ok(event) => {
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session.event",
                            "params": event,
                        });
                        let mut out = serde_json::to_vec(&notification)?;
                        out.push(b'\n');
                        write_half.write_all(&out).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // 落后超过缓冲：要求客户端重新对齐（§8.2.5）。
                        let notification = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session.resync",
                            "params": { "missed": missed },
                        });
                        let mut out = serde_json::to_vec(&notification)?;
                        out.push(b'\n');
                        write_half.write_all(&out).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("事件总线关闭，断开订阅连接");
                        break;
                    }
                }
            }

            chunk = lines.next_segment() => {
                let chunk = match chunk {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => {
                        tracing::debug!("客户端断开");
                        return Ok(());
                    }
                    Err(err) => return Err(err.into()),
                };
                if chunk.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let response: JsonRpcResponse = match serde_json::from_slice::<JsonRpcRequest>(&chunk) {
                    Ok(req) => {
                        let is_subscribe = req.method == "session.subscribe";
                        tracing::debug!(method = %req.method, id = %req.id, "收到 RPC");
                        let response = rpc::dispatch(&runtime, req).await;
                        // 订阅成功后挂接实时广播。
                        if is_subscribe && response.error.is_none() {
                            subscriber = Some(runtime.hub.subscribe());
                        }
                        response
                    }
                    Err(err) => rpc::parse_error(format!("请求解析失败: {err}")),
                };
                let mut out = serde_json::to_vec(&response)?;
                out.push(b'\n');
                write_half.write_all(&out).await?;
            }
        }
    }
    Ok(())
}
