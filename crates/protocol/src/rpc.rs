//! JSON-RPC 2.0 传输层类型与幂等命令约定（§8.1 / §8.2.6）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC id：数字或字符串；解析失败时允许 `null`（JSON-RPC 2.0 规范）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
    Null(()),
}

impl std::fmt::Display for JsonRpcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonRpcId::Number(n) => write!(f, "{n}"),
            JsonRpcId::String(s) => write!(f, "{s}"),
            JsonRpcId::Null(()) => write!(f, "null"),
        }
    }
}

/// 幂等命令参数信封（§8.2.6）：所有改变状态的命令必须携带，
/// 重复命令不得重复执行副作用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(flatten)]
    pub params: Value,
}

/// JSON-RPC 2.0 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 通知（无 id）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 错误对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// 标准 JSON-RPC 错误码。
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// CodeDock 扩展区间起始（服务端自定义错误，如权限拒绝、审批失效）。
    pub const CODEDOCK_ERROR: i64 = -32000;
}

impl JsonRpcResponse {
    pub fn success(id: JsonRpcId, result: impl Into<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result.into()),
            error: None,
        }
    }

    pub fn failure(id: JsonRpcId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrip() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "session.subscribe",
            "params": { "session_id": "0199...", "after_sequence": 152 }
        });
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.method, "session.subscribe");
        assert_eq!(
            serde_json::from_value::<JsonRpcId>(json!("req-1")).unwrap(),
            JsonRpcId::String("req-1".into())
        );
    }

    #[test]
    fn command_preserves_idempotency_fields() {
        let cmd: RpcCommand = serde_json::from_value(json!({
            "command_id": "c-1",
            "idempotency_key": "k-1",
            "task": "fix bug"
        }))
        .unwrap();
        assert_eq!(cmd.command_id.as_deref(), Some("c-1"));
        assert_eq!(cmd.params["task"], "fix bug");
    }

    #[test]
    fn null_id_roundtrip_for_parse_errors() {
        let id: JsonRpcId = serde_json::from_value(json!(null)).unwrap();
        assert_eq!(id, JsonRpcId::Null(()));
        assert_eq!(serde_json::to_value(&id).unwrap(), json!(null));
    }
}
