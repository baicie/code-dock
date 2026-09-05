//! `search.symbol`：工作区符号查询（§12.1 #4，Tree-sitter 索引）。
//!
//! 底层为 [`codedock_project_index::WorkspaceIndex`]（Rust/Python/TS/JS 的
//! 函数、结构体、枚举、trait、类、方法）。索引在构造时全量建立；
//! 只读、effect = none、风险 Low。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use codedock_project_index::WorkspaceIndex;
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

/// 工作区符号查询工具。
pub struct SearchSymbolTool {
    root: PathBuf,
    index: Arc<WorkspaceIndex>,
}

impl SearchSymbolTool {
    /// 构造并建立初始索引（CPU 密集，调用方在装配时执行）。
    pub fn new(root: impl AsRef<std::path::Path>, index: Arc<WorkspaceIndex>) -> Self {
        let root = root.as_ref().to_path_buf();
        let _ = index.index_all(&root);
        Self { root, index }
    }
}

fn format_symbols(symbols: &[codedock_project_index::SymbolEntry]) -> String {
    if symbols.is_empty() {
        return "未找到符号".to_string();
    }
    let mut text = String::new();
    for s in symbols {
        text.push_str(&format!(
            "{}:{}  {} {}\n",
            s.path, s.start_line, s.kind, s.name
        ));
    }
    text
}

#[async_trait]
impl ToolExecutor for SearchSymbolTool {
    fn definition(&self) -> &ToolDefinition {
        static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEF.get_or_init(|| {
            let mut def = ToolDefinition::builtin("search.symbol", vec![Capability::FsRead]);
            def.description =
                "在工作区源码中按名称查找符号（函数/结构体/类/方法等，Tree-sitter 索引）。".into();
            def.input_schema = json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "符号名（精确优先，其次包含匹配）" }
                }
            });
            def.output_schema = json!({
                "type": "object",
                "properties": { "matches": { "type": "integer" } }
            });
            def.effect = Effect::None;
            def
        })
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 name".into()))?
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(ToolRuntimeError::InvalidArguments("name 不能为空".into()));
        }
        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: json!({ "name": name }),
            permissions: vec![Permission {
                capability: Capability::FsRead,
                resource: "$workspace/**".to_string(),
            }],
            risk: Risk::Low,
            expected_side_effects: vec![SideEffect {
                kind: "file.read".into(),
                resource: "$workspace/**".to_string(),
                details: None,
            }],
            operation_digest: OperationDigest::from_sha256_hex(
                operation_digest(&["search.symbol", &name])
                    .trim_start_matches("sha256:")
                    .to_string(),
            ),
            preview: format!("查找符号 {name}"),
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
        let name = plan.normalized_arguments["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // 查询前增量刷新该符号可能所在文件的变化（文件监听接入前的折中，§12.2）。
        let index = self.index.clone();
        let root = self.root.clone();
        let name_for_refresh = name.clone();
        let symbols = tokio::task::spawn_blocking(move || {
            // 增量：对缓存里的每个文件 stat 检查成本高，这里仅在索引为空时重建；
            // 文件监听接入后由 watcher 驱动 update_file。
            if index.is_empty() {
                let _ = index.index_all(&root);
            }
            index.search_symbol(&name_for_refresh)
        })
        .await
        .map_err(|e| ToolRuntimeError::ExecutionFailed(e.to_string()))?;

        let text = format_symbols(&symbols);
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

    async fn workspace_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-sym-tool-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(
            dir.join("src/main.rs"),
            "struct Config { a: u32 }\n\nfn main() {}\n",
        )
        .await
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_symbols_by_name() {
        let dir = workspace_dir().await;
        let t = SearchSymbolTool::new(&dir, Arc::new(WorkspaceIndex::new()));
        let plan = t.preflight(json!({ "name": "Config" })).await.unwrap();
        assert_eq!(plan.risk, Risk::Low);
        assert!(plan.preview.contains("Config"));

        let (tx, _rx) = mpsc::channel(8);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        let text = match &result.content[0] {
            ToolResultContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("struct_item Config"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn validates_and_handles_missing() {
        let dir = workspace_dir().await;
        let t = SearchSymbolTool::new(&dir, Arc::new(WorkspaceIndex::new()));
        assert!(t.preflight(json!({})).await.is_err());
        assert!(t.preflight(json!({ "name": "  " })).await.is_err());

        let plan = t.preflight(json!({ "name": "nope" })).await.unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        assert!(
            matches!(&result.content[0], ToolResultContent::Text { text } if text.contains("未找到"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
