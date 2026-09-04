//! CodeDock 协议层共享类型。
//!
//! 对应《CodeDock 终版设计与路线图 v1.0》§8 三个核心协议：
//! - Session Event Protocol（[`envelope`] / [`events`] / [`session`]）
//! - Tool Capability Protocol（[`tool`] / [`capability`] / [`error`])
//! - Context Snapshot Protocol（[`context`]）
//!
//! 以及两个补充协议的类型：Policy & Approval（[`approval`]）、
//! Client & Remote Control（[`rpc`]）。
//!
//! 版本约定（§8.1）：1.x 只允许新增可选字段，禁止改变既有字段语义。

pub mod approval;
pub mod capability;
pub mod context;
pub mod envelope;
pub mod error;
pub mod events;
pub mod ids;
pub mod rpc;
pub mod session;
pub mod time;
pub mod tool;

/// 领域对象 `schema_version` 当前值。
pub const SCHEMA_VERSION: &str = "1.0";

/// 大对象直传事件的体积阈值（字节），超过应保存为 Blob（§8.1）。
pub const INLINE_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

pub use approval::{ApprovalDecision, ApprovalRequest};
pub use capability::Capability;
pub use context::SourceKind;
pub use context::{
    Classification, ContextBudget, ContextItem, ContextItemContent, ContextSnapshot, LineRange,
    ModelRef, Role, Selection, SelectionReason, SourceRef, TransformationKind, Trust,
};
pub use envelope::{Actor, ActorKind, Durability, EventEnvelope};
pub use error::{ErrorCode, ToolError};
pub use events::EventType;
pub use ids::*;
pub use rpc::{
    JsonRpcErrorObject, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RpcCommand,
};
pub use session::{SessionMode, SessionStatus};
pub use tool::{
    Effect, ExecutionConstraints, Permission, Risk, SideEffect, ToolCallStatus, ToolDefinition,
    ToolExecutionPlan, ToolProvider, ToolResult, ToolResultContent, ToolResultStatus, TrustLevel,
};
