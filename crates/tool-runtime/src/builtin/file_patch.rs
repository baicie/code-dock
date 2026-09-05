//! `file.patch`：对工作区内文件应用查找/替换补丁（§8.3.2 / §13.1）。
//!
//! Patch-first 约束：
//! - 不允许整文件重写（input_schema 只提供 find/replace 块）；
//! - `expected_sha256`（推荐提供）：与当前文件不符即 [`ToolRuntimeError::ResourceConflict`]，
//!   对应 `change.conflicted`（§18.2，禁止"最后写入者覆盖"）；
//! - 写入使用临时文件 + 原子替换；每个 `find` 必须恰好命中一次；
//! - 路径 confinement 与 file.read 相同；风险 Medium（§13.4 基线），
//!   有副作用（effect=possible）→ 默认语境下提升为审批（§18.1）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use codedock_protocol::{
    Capability, Effect, OperationDigest, Permission, Risk, SideEffect, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResult, ToolResultContent, ToolResultStatus,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::{
    CancellationToken, ToolExecutor, ToolOutputChunk, ToolRuntimeError, builtin::workspace,
    operation_digest, validation::validate_arguments,
};

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 工作区内文件补丁工具。
pub struct FilePatchTool {
    root: PathBuf,
}

impl FilePatchTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

/// 归一化参数中提取（path, expected_sha256, patches）。
struct PatchSpec {
    display: String,
    expected_sha256: Option<String>,
    patches: Vec<(String, String)>,
}

fn parse_spec(args: &Value) -> Result<PatchSpec, ToolRuntimeError> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 path".into()))?;
    let patches = args
        .get("patches")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 patches 数组".into()))?;
    if patches.is_empty() {
        return Err(ToolRuntimeError::InvalidArguments(
            "patches 不能为空（禁止整文件重写，请用 find/replace）".into(),
        ));
    }
    let mut parsed = Vec::new();
    for p in patches {
        let find = p
            .get("find")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRuntimeError::InvalidArguments("patch 缺少 find".into()))?;
        let replace = p
            .get("replace")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRuntimeError::InvalidArguments("patch 缺少 replace".into()))?;
        if find.is_empty() {
            return Err(ToolRuntimeError::InvalidArguments("find 不能为空".into()));
        }
        parsed.push((find.to_string(), replace.to_string()));
    }
    Ok(PatchSpec {
        display: path.to_string(),
        expected_sha256: args
            .get("expected_sha256")
            .and_then(Value::as_str)
            .map(str::to_string),
        patches: parsed,
    })
}

fn arguments_path(args: &Value) -> Result<String, ToolRuntimeError> {
    args.get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 path".into()))
}

#[async_trait]
impl ToolExecutor for FilePatchTool {
    fn definition(&self) -> &ToolDefinition {
        static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEF.get_or_init(|| {
            let mut def = ToolDefinition::builtin("file.patch", vec![Capability::FsWrite]);
            def.description =
                "对工作区内文本文件应用查找/替换补丁；写入前校验源文件哈希，原子替换。".into();
            def.input_schema = json!({
                "type": "object",
                "required": ["path", "patches"],
                "properties": {
                    "path": { "type": "string", "description": "工作区内的文件路径" },
                    "expected_sha256": { "type": "string", "description": "期望的当前文件内容 SHA-256（冲突检测）" },
                    "patches": {
                        "type": "array",
                        "description": "查找/替换块；每个 find 必须恰好命中一次",
                        "items": {
                            "type": "object",
                            "required": ["find", "replace"],
                            "properties": {
                                "find": { "type": "string" },
                                "replace": { "type": "string" }
                            }
                        }
                    }
                }
            });
            def.output_schema = json!({
                "type": "object",
                "properties": { "applied": { "type": "integer" } }
            });
            def.effect = Effect::Possible;
            def
        })
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        let raw_path = arguments_path(&arguments)?;
        let (resolved, display) = workspace::resolve_in_workspace(&self.root, &raw_path)?;

        // §12.2 / §18.2：Patch 应用前校验源文件 Hash，避免覆盖用户刚修改的内容。
        let current = tokio::fs::read(&resolved)
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("读取失败: {e}")))?;
        let current_sha = sha256_hex(&current);
        if let Some(expected) = arguments.get("expected_sha256").and_then(Value::as_str) {
            if !expected.eq_ignore_ascii_case(&current_sha) {
                return Err(ToolRuntimeError::ResourceConflict(format!(
                    "{display} 在补丁生成后被修改（当前 sha256={}，期望 {expected}）",
                    &current_sha[..16.min(current_sha.len())]
                )));
            }
        }

        let mut normalized = arguments.clone();
        normalized["path"] = json!(display);
        let spec = parse_spec(&normalized)?;

        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: normalized,
            permissions: vec![Permission {
                capability: Capability::FsWrite,
                resource: display.clone(),
            }],
            risk: Risk::Medium,
            expected_side_effects: vec![SideEffect {
                kind: "file.modified".into(),
                resource: display.clone(),
                details: None,
            }],
            operation_digest: OperationDigest::from_sha256_hex(
                operation_digest(&[
                    "file.patch",
                    &display,
                    &current_sha,
                    &spec.patches.len().to_string(),
                ])
                .trim_start_matches("sha256:")
                .to_string(),
            ),
            preview: format!(
                "对 {} 应用 {} 个查找/替换补丁（base sha256 {}）",
                spec.display,
                spec.patches.len(),
                &current_sha[..16.min(current_sha.len())]
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
        let raw_path = arguments_path(&plan.normalized_arguments)?;
        let (resolved, display) = workspace::resolve_in_workspace(&self.root, &raw_path)?;
        let spec = parse_spec(&plan.normalized_arguments)?;

        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("读取失败: {e}")))?;
        let current_sha = sha256_hex(&bytes);
        // 执行前二次校验（TOCTOU 防护）。
        if let Some(expected) = &spec.expected_sha256 {
            if !expected.eq_ignore_ascii_case(&current_sha) {
                return Err(ToolRuntimeError::ResourceConflict(format!(
                    "{display} 在执行前又被修改（§18.2）"
                )));
            }
        }

        let mut content = String::from_utf8_lossy(&bytes).into_owned();
        for (find, replace) in &spec.patches {
            let hits = content.matches(find.as_str()).count();
            if hits != 1 {
                return Err(ToolRuntimeError::ExecutionFailed(format!(
                    "find 命中 {hits} 次（要求恰好 1 次）：{}",
                    truncate_for_message(find)
                )));
            }
            content = content.replacen(find.as_str(), replace.as_str(), 1);
        }

        // 临时文件 + 原子替换（§13.1）。
        let tmp = resolved.with_extension("cdpatch-tmp");
        tokio::fs::write(&tmp, content.as_bytes())
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("写入失败: {e}")))?;
        tokio::fs::rename(&tmp, &resolved)
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("替换失败: {e}")))?;

        let applied = spec.patches.len();
        let new_sha = sha256_hex(content.as_bytes());
        let _ = output
            .send(ToolOutputChunk::Text(format!(
                "已应用 {applied} 个补丁到 {display}；新 sha256 {}",
                &new_sha[..16]
            )))
            .await;

        Ok(ToolResult {
            tool_call_id: plan.tool_call_id,
            status: ToolResultStatus::Success,
            content: vec![ToolResultContent::Text {
                text: format!("已应用 {applied} 个补丁到 {display}"),
            }],
            artifacts: Vec::new(),
            diagnostics: Vec::new(),
            actual_side_effects: vec![SideEffect {
                kind: "file.modified".into(),
                resource: display,
                details: Some(json!({ "sha256_before": current_sha, "sha256_after": new_sha })),
            }],
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn truncate_for_message(s: &str) -> String {
    if s.len() <= 80 {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(80).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn workspace_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-file-patch-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("app.txt"), "hello world\nsecond line\n")
            .await
            .unwrap();
        dir
    }

    fn sha_of(dir: &Path, rel: &str) -> String {
        let data = std::fs::read(dir.join(rel)).unwrap();
        sha256_hex(&data)
    }

    #[tokio::test]
    async fn applies_patch_atomically() {
        let dir = workspace_dir().await;
        let tool = FilePatchTool::new(&dir);
        let args = json!({
            "path": "app.txt",
            "expected_sha256": sha_of(&dir, "app.txt"),
            "patches": [ { "find": "hello world", "replace": "hello codedock" } ]
        });
        let plan = tool.preflight(args).await.unwrap();
        assert_eq!(plan.risk, Risk::Medium);
        assert!(plan.preview.contains("app.txt"));

        let (tx, _rx) = mpsc::channel(8);
        let result = tool
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.status, ToolResultStatus::Success);
        let content = tokio::fs::read_to_string(dir.join("app.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello codedock\nsecond line\n");
        assert_eq!(
            result.actual_side_effects[0].kind, "file.modified",
            "实际副作用必须记录"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_hash_is_resource_conflict() {
        let dir = workspace_dir().await;
        let tool = FilePatchTool::new(&dir);
        let stale = json!({
            "path": "app.txt",
            "expected_sha256": "deadbeef",
            "patches": [ { "find": "hello", "replace": "hi" } ]
        });
        let err = tool.preflight(stale).await.unwrap_err();
        assert!(matches!(err, ToolRuntimeError::ResourceConflict(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn multi_match_and_missing_find_are_rejected() {
        let dir = workspace_dir().await;
        tokio::fs::write(dir.join("dup.txt"), "a a a")
            .await
            .unwrap();
        let tool = FilePatchTool::new(&dir);
        let plan = tool
            .preflight(json!({
                "path": "dup.txt",
                "patches": [ { "find": "a", "replace": "b" } ]
            }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let err = tool
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolRuntimeError::ExecutionFailed(m) if m.contains("命中 3 次")));

        // find 不存在 → 0 次
        let plan = tool
            .preflight(json!({
                "path": "dup.txt",
                "patches": [ { "find": "zzz", "replace": "b" } ]
            }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let err = tool
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolRuntimeError::ExecutionFailed(m) if m.contains("命中 0 次")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_escape_and_empty_patches() {
        let dir = workspace_dir().await;
        let tool = FilePatchTool::new(&dir);
        assert!(
            tool.preflight(json!({
                "path": "../../etc/passwd",
                "patches": [ { "find": "x", "replace": "y" } ]
            }))
            .await
            .is_err()
        );
        assert!(
            tool.preflight(json!({ "path": "app.txt", "patches": [] }))
                .await
                .is_err()
        );
        assert!(
            tool.preflight(
                json!({ "path": "app.txt", "patches": [ { "find": "", "replace": "y" } ] })
            )
            .await
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
