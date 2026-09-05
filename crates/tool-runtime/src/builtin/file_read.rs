//! `file.read`：读取工作区内文件（§8.3.2 / §13.1）。
//!
//! 安全约束：
//! - 只允许工作区根内的路径：拒绝 `~` 展开、`..` 逃逸与符号链接逃逸；
//! - 只读（`effect = none`，capability `fs.read`），默认风险 Low；
//! - 内容上限默认 64 KiB（可调至 256 KiB），避免大文件挤爆上下文与事件流。

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use codedock_protocol::{
    Capability, Effect, OperationDigest, Permission, Risk, SideEffect, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResult, ToolResultContent, ToolResultStatus,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    CancellationToken, ToolExecutor, ToolOutputChunk, ToolRuntimeError, operation_digest,
    validation::validate_arguments,
};

const DEFAULT_MAX_BYTES: u64 = 64 * 1024;
const MAX_BYTES_CEILING: u64 = 256 * 1024;

/// 工作区内只读文件工具。
pub struct FileReadTool {
    root: PathBuf,
}

impl FileReadTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 工作区根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析并做 confinement 检查，返回（canonical 绝对路径、展示用相对路径）。
    ///
    /// 三层防护（§13.1）：
    /// 1. `~` 前缀直接拒绝（家目录不在工作区内）；
    /// 2. 词法归一化后必须仍在 root 内（拒绝 `..` 逃逸与 root 外绝对路径）；
    /// 3. canonicalize 后必须仍在 canonical root 内（拒绝符号链接逃逸）。
    fn resolve(&self, raw: &str) -> Result<(PathBuf, String), ToolRuntimeError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolRuntimeError::InvalidArguments("path 不能为空".into()));
        }
        if raw.starts_with('~') {
            return Err(ToolRuntimeError::InvalidArguments(
                "禁止 ~ 家目录路径（工作区外）".into(),
            ));
        }

        let candidate = Path::new(raw);
        let relative = if candidate.is_absolute() {
            candidate
                .strip_prefix(&self.root)
                .map_err(|_| ToolRuntimeError::InvalidArguments(format!("路径越出工作区: {raw}")))?
                .to_path_buf()
        } else {
            candidate.to_path_buf()
        };

        let normalized = lexical_normalize(&self.root.join(&relative))?;
        if !normalized.starts_with(&self.root) {
            return Err(ToolRuntimeError::InvalidArguments(format!(
                "路径越出工作区: {raw}"
            )));
        }
        let rel_display = normalized
            .strip_prefix(&self.root)
            .expect("starts_with 已确认前缀")
            .to_string_lossy()
            .into_owned();

        // 符号链接逃逸检查；canonical root 同时防 root 本身是链接。
        let canonical_root = self
            .root
            .canonicalize()
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("工作区根不可用: {e}")))?;
        let canonical = normalized
            .canonicalize()
            .map_err(|_| ToolRuntimeError::InvalidArguments("文件不存在".into()))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(ToolRuntimeError::InvalidArguments(
                "路径经符号链接越出工作区".into(),
            ));
        }

        Ok((canonical, rel_display))
    }

    fn definition_once() -> ToolDefinition {
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
    }
}

/// 纯词法归一化：消解 `.`/`..` 段，保留绝对性；`..` 弹空时返回错误
/// （调用方以 `root.join(rel)` 传入，正常不会发生）。
/// Windows Prefix 组件在阶段 1 暂不处理（§18.8：Windows 适配待 Named Pipe 一并补齐）。
fn lexical_normalize(path: &Path) -> Result<PathBuf, ToolRuntimeError> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    let mut is_absolute = false;
    for comp in path.components() {
        match comp {
            Component::RootDir => is_absolute = true,
            Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop().ok_or_else(|| {
                    ToolRuntimeError::InvalidArguments("路径 `..` 越出边界".into())
                })?;
            }
            Component::Normal(c) => parts.push(c.to_os_string()),
        }
    }
    let mut out = if is_absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for p in parts {
        out.push(p);
    }
    Ok(out)
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
        DEF.get_or_init(Self::definition_once)
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        let raw_path = arguments_path(&arguments)?;
        let (resolved, display) = self.resolve(&raw_path)?;

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
        let (resolved, _) = self.resolve(&raw_path)?;

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
    use std::sync::Arc;

    async fn workspace() -> (PathBuf, FileReadTool) {
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
        let tool = FileReadTool::new(&dir);
        (dir, tool)
    }

    fn args(path: &str) -> Value {
        json!({ "path": path })
    }

    #[tokio::test]
    async fn reads_file_inside_workspace() {
        let (_dir, tool) = workspace().await;
        let plan = tool.preflight(args("src/main.rs")).await.unwrap();
        assert_eq!(plan.risk, Risk::Low);
        assert_eq!(plan.permissions[0].resource, "src/main.rs");
        assert_eq!(plan.normalized_arguments["path"], "src/main.rs");
        assert!(plan.preview.contains("src/main.rs"));

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
    }

    #[tokio::test]
    async fn rejects_path_escape_attempts() {
        let (_dir, tool) = workspace().await;
        // 相对逃逸
        assert!(tool.preflight(args("../../etc/passwd")).await.is_err());
        // root 外绝对路径
        assert!(tool.preflight(args("/etc/passwd")).await.is_err());
        // 家目录
        assert!(tool.preflight(args("~/.ssh/id_rsa")).await.is_err());
        // 不存在
        assert!(tool.preflight(args("src/missing.rs")).await.is_err());
        // 参数校验
        assert!(tool.preflight(json!({})).await.is_err());
        assert!(tool.preflight(json!({ "path": 1 })).await.is_err());
    }

    #[tokio::test]
    async fn absolute_path_inside_workspace_is_allowed() {
        let (dir, tool) = workspace().await;
        let abs = dir.join("src/main.rs");
        let plan = tool.preflight(args(abs.to_str().unwrap())).await.unwrap();
        assert_eq!(plan.normalized_arguments["path"], "src/main.rs");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        let (dir, tool) = workspace().await;
        std::os::unix::fs::symlink("/etc/passwd", dir.join("evil")).unwrap();
        assert!(tool.preflight(args("evil")).await.is_err());
    }

    #[tokio::test]
    async fn enforces_size_limit() {
        let (dir, tool) = workspace().await;
        let big = dir.join("big.txt");
        tokio::fs::write(&big, "x".repeat(2048)).await.unwrap();
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
    }

    #[tokio::test]
    async fn digest_is_stable_per_path_and_differs_across_paths() {
        let (_dir, tool) = workspace().await;
        let a = tool.preflight(args("src/main.rs")).await.unwrap();
        let b = tool.preflight(args("src/main.rs")).await.unwrap();
        let c = tool.preflight(args("src/lib.rs")).await.unwrap();
        assert_eq!(a.operation_digest, b.operation_digest);
        assert_ne!(a.operation_digest, c.operation_digest);
        let _ = Arc::new(&tool);
    }
}
