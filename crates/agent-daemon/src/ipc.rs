//! Local IPC：Unix Domain Socket / Windows Named Pipe（§2）。
//!
//! 帧：换行分隔的 JSON-RPC 2.0（一行一请求，一行一响应）。
//! TODO(阶段1)：session.subscribe 实时事件推送（§8.2.5）、Windows Named Pipe（§18.8）。

use std::sync::Arc;

use anyhow::Context as _;
use codedock_protocol::{JsonRpcRequest, JsonRpcResponse};
use codedock_session_engine::SessionManager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::rpc;

/// 绑定 Unix Domain Socket（Windows Named Pipe 在阶段 1 补充，§18.8）。
pub async fn bind(socket_path: &str) -> anyhow::Result<UnixListener> {
    // 清理残留 socket 文件
    let _ = std::fs::remove_file(socket_path);
    let listener =
        UnixListener::bind(socket_path).with_context(|| format!("无法绑定 {socket_path}"))?;
    Ok(listener)
}

/// 接受连接并逐连接处理（并发安全；runtime 内部自带同步）。
pub async fn accept_loop(
    listener: UnixListener,
    runtime: Arc<dyn SessionManager>,
) -> anyhow::Result<()> {
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

/// 换行分隔 JSON-RPC 服务循环：直到客户端断开。
async fn serve(runtime: Arc<dyn SessionManager>, stream: UnixStream) -> anyhow::Result<()> {
    tracing::debug!("客户端已连接");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).split(b'\n');

    while let Some(chunk) = lines.next_segment().await? {
        if chunk.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let response: JsonRpcResponse = match serde_json::from_slice::<JsonRpcRequest>(&chunk) {
            Ok(req) => {
                tracing::debug!(method = %req.method, id = %req.id, "收到 RPC");
                rpc::dispatch(runtime.as_ref(), req).await
            }
            Err(err) => rpc::parse_error(format!("请求解析失败: {err}")),
        };
        let mut out = serde_json::to_vec(&response)?;
        out.push(b'\n');
        write_half.write_all(&out).await?;
    }

    tracing::debug!("客户端断开");
    Ok(())
}
