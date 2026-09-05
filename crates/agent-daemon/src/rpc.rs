//! JSON-RPC 2.0 分发器（§8.1 / §8.2.6）。
//!
//! 纯函数式的 `dispatch`：不关心传输层，可脱离 Socket 单测。

use codedock_protocol::rpc::codes;
use codedock_protocol::{JsonRpcId, JsonRpcRequest, JsonRpcResponse, SessionId, SessionMode};
use codedock_session_engine::{SessionError, TurnError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Runtime;

/// 单条请求分发：成功返回 `JsonRpcResponse::success`，业务/参数错误返回 failure。
pub async fn dispatch(runtime: &Runtime, req: JsonRpcRequest) -> JsonRpcResponse {
    let result = run(
        runtime,
        &req.method,
        req.params.clone().unwrap_or(Value::Null),
    )
    .await;
    match result {
        Ok(value) => JsonRpcResponse::success(req.id, value),
        Err(fail) => JsonRpcResponse::failure(req.id, fail.code, fail.message),
    }
}

#[derive(Debug)]
struct RpcFail {
    code: i64,
    message: String,
}

impl From<SessionError> for RpcFail {
    fn from(e: SessionError) -> Self {
        RpcFail {
            code: codes::CODEDOCK_ERROR,
            message: e.to_string(),
        }
    }
}

impl From<TurnError> for RpcFail {
    fn from(e: TurnError) -> Self {
        RpcFail {
            code: codes::CODEDOCK_ERROR,
            message: e.to_string(),
        }
    }
}

impl From<serde_json::Error> for RpcFail {
    fn from(e: serde_json::Error) -> Self {
        RpcFail {
            code: codes::INVALID_PARAMS,
            message: format!("参数无效: {e}"),
        }
    }
}

fn invalid(msg: impl Into<String>) -> RpcFail {
    RpcFail {
        code: codes::INVALID_PARAMS,
        message: msg.into(),
    }
}

/// 统一命令参数（§8.2.6 幂等字段 + 各方法域字段）。
#[derive(Debug, Default, Deserialize)]
struct Command {
    #[serde(default)]
    command_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "after_sequence")]
    after_sequence: Option<u64>,
    #[serde(default, rename = "durable_only")]
    durable_only: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Command {
    /// 幂等键：优先 `idempotency_key`，回退 `command_id`。
    fn key(&self) -> Option<String> {
        self.idempotency_key
            .clone()
            .or_else(|| self.command_id.clone())
    }

    fn require_session(&self) -> Result<SessionId, RpcFail> {
        self.session_id.ok_or_else(|| invalid("缺少 session_id"))
    }

    fn require_text(&self) -> Result<String, RpcFail> {
        match self.text.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => Ok(t.to_string()),
            _ => Err(invalid("缺少非空 text")),
        }
    }
}

fn parse_mode(raw: &str) -> Result<SessionMode, RpcFail> {
    serde_json::from_value::<SessionMode>(Value::String(raw.to_string()))
        .map_err(|_| invalid(format!("未知模式: {raw}（可选 ask/plan/edit/auto）")))
}

async fn run(runtime: &Runtime, method: &str, params: Value) -> Result<Value, RpcFail> {
    let cmd: Command = serde_json::from_value(params)?;

    match method {
        "runtime.info" => Ok(json!({
            "name": "codedock-daemon",
            "version": env!("CARGO_PKG_VERSION"),
            "schema_version": codedock_protocol::SCHEMA_VERSION,
        })),

        "session.create" => {
            let mode = match cmd.mode.as_deref() {
                None => SessionMode::Ask,
                Some(m) => parse_mode(m)?,
            };
            let info = runtime
                .sessions
                .create(mode, cmd.task.clone(), cmd.key())
                .await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.status" => {
            let info = runtime.sessions.status(cmd.require_session()?).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.pause" => {
            let info = runtime
                .sessions
                .pause(cmd.require_session()?, cmd.key())
                .await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.resume" => {
            let info = runtime
                .sessions
                .resume(cmd.require_session()?, cmd.key())
                .await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.cancel" => {
            let info = runtime
                .sessions
                .cancel(cmd.require_session()?, cmd.key())
                .await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.mode" => {
            let id = cmd.require_session()?;
            let mode = parse_mode(cmd.mode.as_deref().ok_or_else(|| invalid("缺少 mode"))?)?;
            let info = runtime.sessions.set_mode(id, mode, cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.message" => {
            let id = cmd.require_session()?;
            let text = cmd.require_text()?;
            let outcome = runtime.turns.send_message(id, text, cmd.key()).await?;
            Ok(serde_json::to_value(outcome)?)
        }

        "session.events" => {
            let id = cmd.require_session()?;
            let events = runtime
                .sessions
                .events(
                    id,
                    cmd.after_sequence.unwrap_or(0),
                    cmd.durable_only.unwrap_or(true),
                    cmd.limit.unwrap_or(1000).min(10_000),
                )
                .await?;
            Ok(json!({ "session_id": id, "events": events }))
        }

        other => Err(RpcFail {
            code: codes::METHOD_NOT_FOUND,
            message: format!("未知方法: {other}"),
        }),
    }
}

/// id 为 null 的响应辅助（解析失败场景）。
pub fn parse_error(message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::failure(JsonRpcId::Null(()), codes::PARSE_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble_in_memory;
    use codedock_protocol::SessionStatus;

    async fn create_session(runtime: &Runtime, mode: &str) -> (SessionId, u64) {
        let resp = dispatch(
            runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-create".into()),
                method: "session.create".into(),
                params: Some(json!({ "mode": mode, "task": "修复一个真实 Bug" })),
            },
        )
        .await;
        assert!(resp.error.is_none(), "{resp:?}");
        let result = resp.result.unwrap();
        let sid: SessionId = serde_json::from_value(result["session_id"].clone()).unwrap();
        let seq = result["latest_sequence"].as_u64().unwrap();
        (sid, seq)
    }

    async fn send(runtime: &Runtime, sid: &SessionId, text: &str, key: Option<&str>) -> Value {
        let resp = dispatch(
            runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String(format!("t-msg-{text}")),
                method: "session.message".into(),
                params: Some(json!({
                    "session_id": sid,
                    "text": text,
                    "idempotency_key": key,
                })),
            },
        )
        .await;
        assert!(resp.error.is_none(), "{resp:?}");
        resp.result.unwrap()
    }

    #[tokio::test]
    async fn message_turn_end_to_end_via_rpc() {
        let runtime = assemble_in_memory().await;
        let (sid, seq_after_create) = create_session(&runtime, "ask").await;
        assert_eq!(seq_after_create, 2, "created + started");

        let out = send(&runtime, &sid, "你好", Some("k-msg-1")).await;
        assert_eq!(out["text"], "echo: 你好", "Mock Provider 回声");
        assert!(out["latest_sequence"].as_u64().unwrap() > seq_after_create);

        // 幂等：相同 key 返回同一 Turn，不追加事件。
        let again = send(&runtime, &sid, "你好", Some("k-msg-1")).await;
        assert_eq!(again["latest_sequence"], out["latest_sequence"]);

        let status = dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-status".into()),
                method: "session.status".into(),
                params: Some(json!({ "session_id": sid })),
            },
        )
        .await;
        assert_eq!(
            status.result.unwrap()["status"],
            serde_json::to_value(SessionStatus::Running).unwrap()
        );
    }

    #[tokio::test]
    async fn session_message_rejects_paused_and_empty_text() {
        let runtime = assemble_in_memory().await;
        let (sid, _) = create_session(&runtime, "ask").await;

        let resp = dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-empty".into()),
                method: "session.message".into(),
                params: Some(json!({ "session_id": sid, "text": "   " })),
            },
        )
        .await;
        assert_eq!(resp.error.unwrap().code, codes::INVALID_PARAMS);

        dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-pause".into()),
                method: "session.pause".into(),
                params: Some(json!({ "session_id": sid })),
            },
        )
        .await;

        let resp = dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-paused".into()),
                method: "session.message".into(),
                params: Some(json!({ "session_id": sid, "text": "你好" })),
            },
        )
        .await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, codes::CODEDOCK_ERROR);
        assert!(err.message.contains("Paused"), "{err:?}");
    }

    /// 断线重连（§8.2.5）：客户端以 `after_sequence` 重连后，Runtime 先补发
    /// Durable Event；补发流内 sequence 严格单调，且 transient delta 不出现。
    #[tokio::test]
    async fn reconnect_replays_durable_events_without_loss() {
        let runtime = assemble_in_memory().await;
        let (sid, _) = create_session(&runtime, "plan").await;
        send(&runtime, &sid, "第一问", Some("k-1")).await;
        let first_out = send(&runtime, &sid, "第二问", Some("k-2")).await;
        let seen = first_out["latest_sequence"].as_u64().unwrap();

        // 模拟断线：客户端重连时只声明"我已收到前 2 条（created+started）"，
        // Runtime 必须补发其后全部 Durable Event（§8.2.5）。
        let resp = dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-resync".into()),
                method: "session.events".into(),
                params: Some(json!({
                    "session_id": sid,
                    "after_sequence": 2,
                    "durable_only": true,
                    "limit": 10_000,
                })),
            },
        )
        .await;
        let result = resp.result.unwrap();
        let events = result["events"].as_array().unwrap();

        let sequences: Vec<u64> = events
            .iter()
            .map(|e| e["sequence"].as_u64().unwrap())
            .collect();
        assert!(!sequences.is_empty());
        assert!(
            sequences.windows(2).all(|w| w[0] < w[1]),
            "补发 sequence 必须严格单调: {sequences:?}"
        );
        assert!(sequences[0] > 2);
        assert_eq!(*sequences.last().unwrap(), seen);

        let types: Vec<&str> = events
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
            .collect();
        assert!(!types.contains(&"message.delta"), "transient 事件不补发");
        assert_eq!(
            types.iter().filter(|t| **t == "message.completed").count(),
            2,
            "两轮的最终消息都补发"
        );
        assert_eq!(
            types.iter().filter(|t| **t == "turn.completed").count(),
            2,
            "两轮的 Turn 完成事件都补发"
        );
        // 补发不遗漏：durable_only=false 的全量流中，> 2 的 durable 事件数一致。
        let all = dispatch(
            &runtime,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: JsonRpcId::String("t-all".into()),
                method: "session.events".into(),
                params: Some(json!({
                    "session_id": sid,
                    "after_sequence": 2,
                    "durable_only": false,
                    "limit": 10_000,
                })),
            },
        )
        .await;
        let all_events = all.result.unwrap()["events"].as_array().unwrap().clone();
        let durable_in_all = all_events
            .iter()
            .filter(|e| e["durability"] == "durable")
            .count();
        assert_eq!(durable_in_all, sequences.len(), "补发覆盖全部 durable 事件");
        assert!(all_events.len() > durable_in_all, "delta 仅存在于实时流");
    }
}
