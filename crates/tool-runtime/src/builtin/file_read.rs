//! `file.read`：读取工作区内文件（§8.3.2 / §13.1）。
//!
//! 只读（`effect = none`，capability `fs.read`），默认风险 Low；
//! 内容上限默认 64 KiB（可调至 256 KiB），避免大文件挤爆上下文与事件流。
//! 路径 confinement 见 [`crate::builtin::workspace`]。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use codedock_protocol::{
    Capability, Effect, OperationDigest, Permission, Risk, SideEffect, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResult, ToolResultContent, ToolResultStatus,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    CancellationToken, ToolExecutor, ToolOutputChunk, ToolRuntimeError, builtin::workspace,
    operation_digest, validation::validate_arguments,
};

const DEFAULT_MAX_BYTES: u64 = 64 * 1024;
const MAX_BYTES_CEILING: u64 = 256 * 1024;

/// 工作区内只读文件工具。
pub struct FileReadTool {
    root: PathBuf,
}

impl FileReadTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// 工作区根。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn arguments_path(args: &Value) -> Result<String, ToolRuntimeError> {
    args.get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 path".into()))
}

#[async_trait]
impl ToolExecutor for FileReadTool {
    fn definition(&self) -> &ToolDefinition {
        static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEF.get_or_init(|| {
            let mut def = ToolDefinition::builtin("file.read", vec![Capability::FsRead]);
            def.description = "读取工作区内一个文本文件的内容。".into();
            def.input_schema = json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "工作区内的路径（相对或工作区内绝对路径）" },
                    "max_bytes": { "type": "integer", "description": "内容上限（字节）" }
                }
            });
            def.output_schema = json!({
                "type": "object",
                "properties": { "text": { "type": "string" } }
            });
            def.effect = Effect::None;
            def
        })
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        let raw_path = arguments_path(&arguments)?;
        let (resolved, display) = workspace::resolve_in_workspace(&self.root, &raw_path)?;

        let mut normalized = arguments.clone();
        normalized["path"] = json!(display);

        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: normalized,
            permissions: vec![Permission {
                capability: Capability::FsRead,
                resource: display.clone(),
            }],
            risk: Risk::Low,
            expected_side_effects: vec![SideEffect {
                kind: "file.read".into(),
                resource: display.clone(),
                details: None,
            }],
            operation_digest: OperationDigest::from_sha256_hex(
                operation_digest(&["file.read", &display])
                    .trim_start_matches("sha256:")
                    .to_string(),
            ),
            preview: format!(
                "读取工作区文件 {display}（canonical: {}）",
                resolved.display()
            ),
        })
    }

    async fn execute(
        &self,
        plan: ToolExecutionPlan,
        output: mpsc::Sender<ToolOutputChunk>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolRuntimeError> {
        let started = std::time::Instant::now();
        if cancel.is_cancelled() {
            return Err(ToolRuntimeError::Cancelled);
        }
        // 执行前重新 resolve：preflight 与 execute 之间的文件系统变化不能绕过 confinement。
        let raw_path = arguments_path(&plan.normalized_arguments)?;
        let (resolved, _) = workspace::resolve_in_workspace(&self.root, &raw_path)?;

        let max_bytes = plan
            .normalized_arguments
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(MAX_BYTES_CEILING);

        let meta = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("读取失败: {e}")))?;
        if meta.len() > max_bytes {
            return Err(ToolRuntimeError::ExecutionFailed(format!(
                "文件 {} 字节超过上限 {max_bytes}",
                meta.len()
            )));
        }
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("读取失败: {e}")))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = output.send(ToolOutputChunk::Text(text.clone())).await;

        Ok(ToolResult {
            tool_call_id: plan.tool_call_id,
            status: ToolResultStatus::Success,
            content: vec![ToolResultContent::Text { text }],
            artifacts: Vec::new(),
            diagnostics: Vec::new(),
            actual_side_effects: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn workspace_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-file-read-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/main.rs"), "fn main() {}\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("src/lib.rs"), "pub mod x;\n")
            .await
            .unwrap();
        dir
    }

    fn args(path: &str) -> Value {
        json!({ "path": path })
    }

    #[tokio::test]
    async fn reads_file_inside_workspace() {
        let dir = workspace_dir().await;
        let tool = FileReadTool::new(&dir);
        let plan = tool.preflight(args("src/main.rs")).await.unwrap();
        assert_eq!(plan.risk, Risk::Low);
        assert_eq!(plan.permissions[0].resource, "src/main.rs");
        assert_eq!(plan.normalized_arguments["path"], "src/main.rs");

        let (tx, mut rx) = mpsc::channel(8);
        let result = tool
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.status, ToolResultStatus::Success);
        assert!(
            matches!(&result.content[0], ToolResultContent::Text { text } if text.contains("fn main"))
        );
        assert!(matches!(rx.recv().await, Some(ToolOutputChunk::Text(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_escape_attempts_and_validates_args() {
        let dir = workspace_dir().await;
        let tool = FileReadTool::new(&dir);
        assert!(tool.preflight(args("../../etc/passwd")).await.is_err());
        assert!(tool.preflight(args("/etc/passwd")).await.is_err());
        assert!(tool.preflight(args("~/.ssh/id_rsa")).await.is_err());
        assert!(tool.preflight(args("src/missing.rs")).await.is_err());
        assert!(tool.preflight(json!({})).await.is_err());
        assert!(tool.preflight(json!({ "path": 1 })).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn absolute_path_inside_workspace_is_allowed() {
        let dir = workspace_dir().await;
        let tool = FileReadTool::new(&dir);
        let abs = dir.join("src/main.rs");
        let plan = tool.preflight(args(abs.to_str().unwrap())).await.unwrap();
        assert_eq!(plan.normalized_arguments["path"], "src/main.rs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        let dir = workspace_dir().await;
        std::os::unix::fs::symlink("/etc/passwd", dir.join("evil")).unwrap();
        let tool = FileReadTool::new(&dir);
        assert!(tool.preflight(args("evil")).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enforces_size_limit() {
        let dir = workspace_dir().await;
        tokio::fs::write(dir.join("big.txt"), "x".repeat(2048))
            .await
            .unwrap();
        let tool = FileReadTool::new(&dir);
        let plan = tool
            .preflight(json!({ "path": "big.txt", "max_bytes": 1024 }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let err = tool
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolRuntimeError::ExecutionFailed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn digest_is_stable_per_path_and_differs_across_paths() {
        let dir = workspace_dir().await;
        let tool = FileReadTool::new(&dir);
        let a = tool.preflight(args("src/main.rs")).await.unwrap();
        let b = tool.preflight(args("src/main.rs")).await.unwrap();
        let c = tool.preflight(args("src/lib.rs")).await.unwrap();
        assert_eq!(a.operation_digest, b.operation_digest);
        assert_ne!(a.operation_digest, c.operation_digest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
