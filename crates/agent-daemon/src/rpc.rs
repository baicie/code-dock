//! JSON-RPC 2.0 分发器（§8.1 / §8.2.6）。
//!
//! 纯函数式的 `dispatch`：不关心传输层，可脱离 Socket 单测。

use codedock_protocol::rpc::codes;
use codedock_protocol::{JsonRpcId, JsonRpcRequest, JsonRpcResponse, SessionId, SessionMode};
use codedock_session_engine::{SessionError, SessionManager};
use serde::Deserialize;
use serde_json::{Value, json};

/// 单条请求分发：成功返回 `JsonRpcResponse::success`，业务/参数错误返回 failure。
pub async fn dispatch(runtime: &dyn SessionManager, req: JsonRpcRequest) -> JsonRpcResponse {
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
}

fn parse_mode(raw: &str) -> Result<SessionMode, RpcFail> {
    serde_json::from_value::<SessionMode>(Value::String(raw.to_string()))
        .map_err(|_| invalid(format!("未知模式: {raw}（可选 ask/plan/edit/auto）")))
}

async fn run(runtime: &dyn SessionManager, method: &str, params: Value) -> Result<Value, RpcFail> {
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
            let info = runtime.create(mode, cmd.task.clone(), cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.status" => {
            let info = runtime.status(cmd.require_session()?).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.pause" => {
            let info = runtime.pause(cmd.require_session()?, cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.resume" => {
            let info = runtime.resume(cmd.require_session()?, cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.cancel" => {
            let info = runtime.cancel(cmd.require_session()?, cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.mode" => {
            let id = cmd.require_session()?;
            let mode = parse_mode(cmd.mode.as_deref().ok_or_else(|| invalid("缺少 mode"))?)?;
            let info = runtime.set_mode(id, mode, cmd.key()).await?;
            Ok(serde_json::to_value(info)?)
        }

        "session.events" => {
            let id = cmd.require_session()?;
            let events = runtime
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
