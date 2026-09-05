//! 端到端集成测试：真实 UDS 上的 JSON-RPC 往返（阶段 1 退出标准的基础：
//! CLI 无 GUI 完成会话控制；断线重放无丢失）。

use codedock_agent_daemon::{assemble_in_memory, ipc};
use codedock_protocol::{JsonRpcId, JsonRpcRequest, JsonRpcResponse};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn socket_path() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("codedock-it-{}-{n}.sock", std::process::id()))
        .to_string_lossy()
        .to_string()
}

async fn spawn_daemon() -> String {
    let path = socket_path();
    let listener = ipc::bind(&path).await.unwrap();
    let runtime = assemble_in_memory().await;
    tokio::spawn(async move { ipc::accept_loop(listener, runtime).await });
    path
}

struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &str) -> Self {
        let (r, w) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
        }
    }

    /// 读取一行（带超时），返回原始字节。
    async fn read_line(&mut self, timeout: std::time::Duration) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        tokio::time::timeout(timeout, self.reader.read_until(b'\n', &mut buf)).await??;
        assert!(!buf.is_empty(), "daemon 提前断开");
        Ok(buf)
    }

    async fn call(&mut self, id: &str, method: &str, params: Value) -> JsonRpcResponse {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::String(id.into()),
            method: method.into(),
            params: Some(params),
        };
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();

        let mut buf = Vec::new();
        self.reader.read_until(b'\n', &mut buf).await.unwrap();
        assert!(!buf.is_empty(), "daemon 提前断开");
        serde_json::from_slice(&buf).unwrap()
    }
}

#[tokio::test]
async fn session_lifecycle_over_ipc() {
    let path = spawn_daemon().await;
    let mut cli = Client::connect(&path).await;

    // runtime.info
    let resp = cli.call("1", "runtime.info", json!({})).await;
    assert!(resp.error.is_none());
    assert_eq!(resp.result.as_ref().unwrap()["schema_version"], "1.0");

    // create
    let resp = cli
        .call(
            "2",
            "session.create",
            json!({ "mode": "plan", "task": "修复一个真实 Bug", "idempotency_key": "k-create" }),
        )
        .await;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    let sid = resp.result.as_ref().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(resp.result.as_ref().unwrap()["status"], "running");

    // 幂等重放：相同 key 返回同一 session
    let resp = cli
        .call(
            "3",
            "session.create",
            json!({ "mode": "plan", "task": "修复一个真实 Bug", "idempotency_key": "k-create" }),
        )
        .await;
    assert_eq!(
        resp.result.as_ref().unwrap()["session_id"].as_str(),
        Some(sid.as_str())
    );

    // pause → resume
    let resp = cli
        .call(
            "4",
            "session.pause",
            json!({ "session_id": sid, "idempotency_key": "k-pause" }),
        )
        .await;
    assert_eq!(resp.result.as_ref().unwrap()["status"], "paused");

    let resp = cli
        .call(
            "5",
            "session.resume",
            json!({ "session_id": sid, "idempotency_key": "k-resume" }),
        )
        .await;
    assert_eq!(resp.result.as_ref().unwrap()["status"], "running");

    // 非法迁移：running → running（resume on running）被拒
    let resp = cli
        .call("6", "session.resume", json!({ "session_id": sid }))
        .await;
    assert_eq!(resp.error.as_ref().unwrap().code, -32000);

    // 事件重放：created/started/paused/resumed = 4 条 durable
    let resp = cli
        .call(
            "7",
            "session.events",
            json!({ "session_id": sid, "after_sequence": 0 }),
        )
        .await;
    let events = resp.result.as_ref().unwrap()["events"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["event_type"], "session.created");
    assert_eq!(events[3]["event_type"], "session.resumed");
    // sequence 严格递增（§8.2.3）
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| e["sequence"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));

    // cancel 后进入终态，再 pause 被拒（终态事实不可篡改，§8.2.7）
    let resp = cli
        .call(
            "8",
            "session.cancel",
            json!({ "session_id": sid, "idempotency_key": "k-cancel" }),
        )
        .await;
    assert_eq!(resp.result.as_ref().unwrap()["status"], "cancelled");
    let resp = cli
        .call("9", "session.pause", json!({ "session_id": sid }))
        .await;
    assert_eq!(resp.error.as_ref().unwrap().code, -32000);

    // 未知方法
    let resp = cli.call("10", "no.such.method", json!({})).await;
    assert_eq!(resp.error.as_ref().unwrap().code, -32601);

    // 损坏请求行 → PARSE_ERROR 且连接继续可用
    cli.writer.write_all(b"{not json\n").await.unwrap();
    let mut buf = Vec::new();
    cli.reader.read_until(b'\n', &mut buf).await.unwrap();
    let resp: JsonRpcResponse = serde_json::from_slice(&buf).unwrap();
    assert_eq!(resp.error.as_ref().unwrap().code, -32700);
    let resp = cli.call("11", "runtime.info", json!({})).await;
    assert!(resp.error.is_none());

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn concurrent_connections_are_served() {
    let path = spawn_daemon().await;
    let mut a = Client::connect(&path).await;
    let mut b = Client::connect(&path).await;

    let ra = a
        .call(
            "a1",
            "session.create",
            json!({ "mode": "ask", "idempotency_key": "a" }),
        )
        .await;
    let rb = b
        .call(
            "b1",
            "session.create",
            json!({ "mode": "auto", "idempotency_key": "b" }),
        )
        .await;
    assert!(ra.error.is_none());
    assert!(rb.error.is_none());
    let sid_a = ra.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let sid_b = rb.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(sid_a, sid_b);

    let _ = std::fs::remove_file(&path);
}

/// 纯对话 Turn 走真实 IPC 全链路（阶段 1 退出标准：无 GUI 完成一次对话）。
#[tokio::test]
async fn chat_turn_over_real_ipc() {
    let path = spawn_daemon().await;
    let mut cli = Client::connect(&path).await;

    let resp = cli
        .call(
            "1",
            "session.create",
            json!({ "mode": "ask", "task": "演示对话" }),
        )
        .await;
    let sid = resp.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 一轮完整对话：用户消息 → Mock 回声。
    let resp = cli
        .call(
            "2",
            "session.message",
            json!({ "session_id": sid, "text": "你好", "idempotency_key": "k-chat" }),
        )
        .await;
    let out = resp.result.unwrap();
    assert_eq!(out["text"], "echo: 你好");
    let turn_id = out["turn_id"].as_str().unwrap().to_string();
    assert!(!turn_id.is_empty());

    // 事件流可重放整个 Turn（durable），transient delta 不在其中。
    let resp = cli
        .call(
            "3",
            "session.events",
            json!({ "session_id": sid, "after_sequence": 0, "durable_only": true }),
        )
        .await;
    let events = resp.result.unwrap()["events"].as_array().unwrap().clone();
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    for expected in [
        "turn.started",
        "message.created",
        "context.snapshot.created",
        "model.request.started",
        "message.completed",
        "model.request.completed",
        "turn.completed",
    ] {
        assert!(types.contains(&expected), "缺少 {expected}: {types:?}");
    }
    assert!(!types.contains(&"message.delta"));
    // 助手最终消息内容可从事件流恢复。
    let completed = events
        .iter()
        .find(|e| e["event_type"] == "message.completed")
        .unwrap();
    assert_eq!(completed["payload"]["text"], "echo: 你好");
    assert_eq!(completed["payload"]["role"], "assistant");

    // 取消后为终态：再发消息被拒。
    cli.call("4", "session.cancel", json!({ "session_id": sid }))
        .await;
    let resp = cli
        .call(
            "5",
            "session.message",
            json!({ "session_id": sid, "text": "hi" }),
        )
        .await;
    assert!(resp.error.is_some());

    let _ = std::fs::remove_file(&path);
}

/// §8.2.5 实时推送：订阅后另一连接产生的 durable 与 transient 事件
/// 都以 `session.event` notification 实时到达。
#[tokio::test]
async fn subscribe_receives_live_pushed_events() {
    let path = spawn_daemon().await;
    let mut watcher = Client::connect(&path).await;
    let mut actor = Client::connect(&path).await;

    let resp = actor
        .call("1", "session.create", json!({ "mode": "ask" }))
        .await;
    let sid = resp.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 订阅：响应携带补发的 durable 事件
    let resp = watcher
        .call(
            "2",
            "session.subscribe",
            json!({ "session_id": sid, "after_sequence": 0 }),
        )
        .await;
    let replay = resp.result.unwrap();
    assert_eq!(replay["subscribed"], true);
    let replayed = replay["events"].as_array().unwrap();
    assert_eq!(replayed.len(), 2, "created + started 补发");
    assert_eq!(replayed[0]["event_type"], "session.created");

    // 另一连接触发一轮对话 → watcher 实时收到推送
    actor
        .call(
            "3",
            "session.message",
            json!({ "session_id": sid, "text": "你好" }),
        )
        .await;

    let mut types: Vec<(u64, String)> = Vec::new();
    loop {
        let line = watcher
            .read_line(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["method"], "session.event", "只应收到事件通知: {v}");
        let params = &v["params"];
        types.push((
            params["sequence"].as_u64().unwrap(),
            params["event_type"].as_str().unwrap().to_string(),
        ));
        if types.last().unwrap().1 == "turn.completed" {
            break;
        }
    }

    let seqs: Vec<u64> = types.iter().map(|(s, _)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "推送 sequence 严格单调: {seqs:?}"
    );
    let names: Vec<&str> = types.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        names.contains(&"message.delta"),
        "transient 事件实时推送: {names:?}"
    );
    assert!(names.contains(&"message.completed"));
    assert_eq!(*names.last().unwrap(), "turn.completed");

    let _ = std::fs::remove_file(&path);
}
