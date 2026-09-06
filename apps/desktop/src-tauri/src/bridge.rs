//! Daemon 桥接层：Desktop（Tauri）↔ Runtime 的唯一通道（§5 边界约束）。
//!
//! Desktop UI 不直接读文件系统、不内嵌 Runtime——一切经 Local IPC
//! （UDS + 换行分隔 JSON-RPC 2.0，与 CLI 同一协议）。
//!
//! 连接策略：请求/响应一次性连接（与 CLI 一致，alpha 期够用）；
//! 订阅为长连接，`session.event` notification 经回调交给上层
//! （Tauri 命令层转发为前端事件）。TODO(阶段4)：连接复用池。

use codedock_protocol::{EventEnvelope, JsonRpcId, JsonRpcRequest, JsonRpcResponse};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("连接 daemon 失败（{socket}）: {source}")]
    Connect {
        socket: String,
        #[source]
        source: std::io::Error,
    },
    #[error("请求解析失败: {0}")]
    Encoding(String),
    #[error("RPC 错误 {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("IO 错误: {0}")]
    Io(String),
}

/// Desktop ↔ Daemon 的 JSON-RPC 客户端。
#[derive(Debug, Clone)]
pub struct DaemonClient {
    pub socket: String,
}

impl DaemonClient {
    pub fn new(socket: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// 单次请求：连接 → 发送 → 读取响应 → 断开。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, BridgeError> {
        let mut stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| BridgeError::Connect {
                    socket: self.socket.clone(),
                    source,
                })?;
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::String(format!("desktop-{}", uuid_like())),
            method: method.into(),
            params: Some(params),
        };
        let mut line =
            serde_json::to_vec(&req).map_err(|e| BridgeError::Encoding(e.to_string()))?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .await
            .map_err(|e| BridgeError::Io(e.to_string()))?;

        let mut buf = Vec::new();
        BufReader::new(&mut stream)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| BridgeError::Io(e.to_string()))?;
        if buf.is_empty() {
            return Err(BridgeError::Io("daemon 关闭了连接".into()));
        }
        let resp: JsonRpcResponse =
            serde_json::from_slice(&buf).map_err(|e| BridgeError::Encoding(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(BridgeError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        resp.result
            .ok_or_else(|| BridgeError::Encoding("响应缺少 result".into()))
    }

    /// 长连接订阅：先收到补发响应（交给 `on_replay`），此后每个
    /// `session.event` notification 交给 `on_event`；阻塞直到连接断开。
    pub async fn subscribe(
        &self,
        session_id: &str,
        after_sequence: u64,
        mut on_replay: impl FnMut(Vec<Value>),
        mut on_event: impl FnMut(EventEnvelope),
        mut on_resync: impl FnMut(u64),
    ) -> Result<(), BridgeError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| BridgeError::Connect {
                    socket: self.socket.clone(),
                    source,
                })?;
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::String(format!("desktop-sub-{}", uuid_like())),
            method: "session.subscribe".into(),
            params: Some(json!({
                "session_id": session_id,
                "after_sequence": after_sequence,
            })),
        };
        let mut line =
            serde_json::to_vec(&req).map_err(|e| BridgeError::Encoding(e.to_string()))?;
        line.push(b'\n');
        let mut stream = stream;
        stream
            .write_all(&line)
            .await
            .map_err(|e| BridgeError::Io(e.to_string()))?;

        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        let mut replay_done = false;
        loop {
            buf.clear();
            let n = reader
                .read_until(b'\n', &mut buf)
                .await
                .map_err(|e| BridgeError::Io(e.to_string()))?;
            if n == 0 {
                return Ok(()); // daemon 断开（正常关机/停机）
            }
            let v: Value =
                serde_json::from_slice(&buf).map_err(|e| BridgeError::Encoding(e.to_string()))?;
            if let Some(err) = v.get("error") {
                return Err(BridgeError::Rpc {
                    code: err["code"].as_i64().unwrap_or(-1),
                    message: err["message"].as_str().unwrap_or_default().to_string(),
                });
            }
            match v.get("method").and_then(Value::as_str) {
                Some("session.event") => {
                    let envelope: EventEnvelope = serde_json::from_value(v["params"].clone())
                        .map_err(|e| BridgeError::Encoding(e.to_string()))?;
                    on_event(envelope);
                }
                Some("session.resync") => {
                    let missed = v["params"]["missed"].as_u64().unwrap_or(0);
                    on_resync(missed);
                }
                _ if !replay_done => {
                    // 订阅响应：补发的 durable 事件
                    let events = v["result"]["events"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    replay_done = true;
                    on_replay(events);
                }
                _ => {}
            }
        }
    }
}

/// 轻量请求 id（避免引入 uuid 依赖）。
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    format!("{pid}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{Durability, SessionId};
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::{UnixListener, UnixStream};

    /// 模拟 daemon：响应 runtime.info；对 session.subscribe 先回补发响应，
    /// 再推两条 notification。
    async fn spawn_mock_daemon(path: String) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    serve_mock(stream).await;
                });
            }
        });
    }

    async fn serve_mock(stream: UnixStream) {
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).split(b'\n');
        while let Ok(Some(chunk)) = lines.next_segment().await {
            let req: JsonRpcRequest = match serde_json::from_slice(&chunk) {
                Ok(req) => req,
                Err(_) => continue,
            };
            match req.method.as_str() {
                "runtime.info" => {
                    let resp = JsonRpcResponse::success(
                        req.id,
                        json!({ "name": "mock-daemon", "version": "0" }),
                    );
                    write_line(&mut write_half, &resp).await;
                }
                "session.subscribe" => {
                    let resp = JsonRpcResponse::success(req.id, json!({ "subscribed": true }));
                    write_line(&mut write_half, &resp).await;
                    for seq in 1..=2u64 {
                        let mut envelope = EventEnvelope::draft(
                            SessionId::generate(),
                            None,
                            "message.delta",
                            Durability::Transient,
                            codedock_protocol::Actor::agent("mock"),
                            json!({ "text": "你好" }),
                        );
                        envelope.sequence = seq;
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": "session.event",
                            "params": envelope,
                        });
                        let mut out = serde_json::to_vec(&notification).unwrap();
                        out.push(b'\n');
                        write_half.write_all(&out).await.unwrap();
                    }
                }
                _ => {}
            }
        }
    }

    async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, resp: &JsonRpcResponse) {
        let mut out = serde_json::to_vec(resp).unwrap();
        out.push(b'\n');
        write.write_all(&out).await.unwrap();
    }

    #[tokio::test]
    async fn call_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mock.sock").to_string_lossy().into_owned();
        spawn_mock_daemon(path.clone()).await;
        let client = DaemonClient::new(&path);
        let info = client.call("runtime.info", json!({})).await.unwrap();
        assert_eq!(info["name"], "mock-daemon");

        // RPC 错误路径：连接不存在的方法 → daemon 无响应会挂起，因此
        // mock 对未知方法不回包；此处仅验证正常路径与错误传播由集成测试覆盖。
    }

    #[tokio::test]
    async fn subscribe_receives_replay_and_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("mock-sub.sock")
            .to_string_lossy()
            .into_owned();
        spawn_mock_daemon(path.clone()).await;
        let client = DaemonClient::new(&path);

        let replay_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let event_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let rc = replay_count.clone();
        let ec = event_count.clone();
        let handle = tokio::spawn(async move {
            client
                .subscribe(
                    "sess",
                    0,
                    move |replay| {
                        rc.store(replay.len(), std::sync::atomic::Ordering::SeqCst);
                    },
                    move |_event| {
                        ec.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    move |_missed| {},
                )
                .await
        });
        // 等 mock 推完两条 notification
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            replay_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "mock 无补发"
        );
        assert_eq!(event_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn connect_failure_is_reported() {
        let client = DaemonClient::new("/nonexistent/codedock.sock");
        let err = client.call("runtime.info", json!({})).await.unwrap_err();
        assert!(matches!(err, BridgeError::Connect { .. }));
    }
}
