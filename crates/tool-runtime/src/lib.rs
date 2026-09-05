//! Tool Runtime：Tool 注册、参数校验、预执行分析、执行、取消（§6）。
//!
//! Tool Call 生命周期（§8.3.4）：
//! `PROPOSED → VALIDATED → PREFLIGHTED → POLICY DECISION → STARTED → OUTPUT... → 终态`。
//!
//! TODO(阶段2)：内置工具（file.read / file.patch / search.text / shell.execute /
//! git.status / git.diff）实现、进程树管理与 Hash 冲突检测。

pub mod builtin;
pub mod validation;

pub use builtin::{
    FilePatchTool, FileReadTool, GitTool, SearchSymbolTool, SearchTextTool, ShellExecuteTool,
};

use async_trait::async_trait;
use codedock_protocol::{ToolDefinition, ToolExecutionPlan, ToolResult};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::mpsc;
pub use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum ToolRuntimeError {
    #[error("工具未注册: {0}")]
    UnknownTool(String),
    #[error("参数校验失败: {0}")]
    InvalidArguments(String),
    #[error("资源已变化（§18.2 change.conflicted）: {0}")]
    ResourceConflict(String),
    #[error("执行失败: {0}")]
    ExecutionFailed(String),
    #[error("任务已取消")]
    Cancelled,
}

/// 工具输出流事件（`tool.call.output`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Text(String),
}

/// Tool Executor 抽象：每个工具实现它并由 [`ToolRegistry`] 注册。
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// 工具声明（§8.3.2）。
    fn definition(&self) -> &ToolDefinition;

    /// Preflight：参数归一化 + 计算权限、风险与 operation_digest（§8.3.5）。
    async fn preflight(
        &self,
        arguments: serde_json::Value,
    ) -> Result<ToolExecutionPlan, ToolRuntimeError>;

    /// 执行；流式输出通过 `output` 通道发送；`cancel` 触发优雅终止。
    async fn execute(
        &self,
        plan: ToolExecutionPlan,
        output: mpsc::Sender<ToolOutputChunk>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolRuntimeError>;
}

/// 计算操作摘要：审批的唯一锚点（§8.3.5）。
///
/// 参数、目录、权限或命令任何一项变化，digest 随之变化，旧审批立即失效。
pub fn operation_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update([0x1f]); // 单元分隔符，避免拼接歧义
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Tool 注册表：`namespace.action` → Executor。
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具；同名注册视为错误，防止插件覆盖内置工具。
    pub fn register(&mut self, executor: Box<dyn ToolExecutor>) -> Result<(), ToolRuntimeError> {
        let name = executor.definition().name.clone();
        if self.tools.contains_key(&name) {
            return Err(ToolRuntimeError::ExecutionFailed(format!(
                "工具重复注册: {name}"
            )));
        }
        self.tools.insert(name, executor);
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<&dyn ToolExecutor> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_changes_on_any_part_change() {
        let base = operation_digest(&["shell.execute", "./gradlew test", "$workspace"]);
        let diff_cmd =
            operation_digest(&["shell.execute", "./gradlew test --dry-run", "$workspace"]);
        let diff_cwd = operation_digest(&["shell.execute", "./gradlew test", "/tmp"]);
        assert_ne!(base, diff_cmd, "命令变化 → 审批失效");
        assert_ne!(base, diff_cwd, "目录变化 → 审批失效");
    }
}
