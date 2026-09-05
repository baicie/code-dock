//! `search.text`：工作区文本搜索（§8.3.3 / §12.1）。
//!
//! 阶段 2 的纯 Rust 实现：目录遍历跳过 `.git`/`target`/`node_modules` 等构建目录，
//! 跳过超大与非 UTF-8 文件，大小写不敏感子串匹配，命中数封顶防上下文爆炸。
//! TODO(阶段3)：ripgrep + SQLite FTS5 + Tree-sitter 符号索引（§12.1）。

use std::path::{Path, PathBuf};

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

const DEFAULT_MAX_MATCHES: usize = 100;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const SKIP_DIRS: [&str; 5] = [".git", "target", "node_modules", "dist", ".codedock"];

/// 工作区文本搜索工具。
pub struct SearchTextTool {
    root: PathBuf,
}

impl SearchTextTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

#[derive(Debug)]
struct Match {
    path: String,
    line_no: usize,
    line: String,
}

/// 同步遍历（在小中型工作区内足够快；ripgrep 在阶段 3 引入）。
fn walk(
    root: &Path,
    dir: &Path,
    query: &str,
    max: usize,
    out: &mut Vec<Match>,
) -> std::io::Result<()> {
    if out.len() >= max {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= max {
            return Ok(());
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, query, max, out)?;
        } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            // 非 UTF-8（二进制）文件跳过。
            let Ok(text) = String::from_utf8(content) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let q = query.to_lowercase();
            for (idx, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    out.push(Match {
                        path: rel.clone(),
                        line_no: idx + 1,
                        line: line.chars().take(200).collect(),
                    });
                    if out.len() >= max {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

fn format_matches(matches: &[Match], truncated: bool) -> String {
    if matches.is_empty() {
        return "未找到匹配".to_string();
    }
    let mut text = String::new();
    let mut current_file = String::new();
    for m in matches {
        if m.path != current_file {
            current_file = m.path.clone();
            text.push_str(&format!("{current_file}\n"));
        }
        text.push_str(&format!("  {}: {}\n", m.line_no, m.line));
    }
    if truncated {
        text.push_str(&format!("（结果截断，仅显示前 {} 条）\n", matches.len()));
    }
    text
}

#[async_trait]
impl ToolExecutor for SearchTextTool {
    fn definition(&self) -> &ToolDefinition {
        static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEF.get_or_init(|| {
            let mut def = ToolDefinition::builtin("search.text", vec![Capability::FsRead]);
            def.description =
                "在工作区文本文件中搜索关键词（大小写不敏感），返回文件、行号与内容。".into();
            def.input_schema = json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词" },
                    "max_results": { "type": "integer", "description": "命中上限（默认 100）" }
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
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 query".into()))?
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(ToolRuntimeError::InvalidArguments("query 不能为空".into()));
        }

        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: json!({ "query": query }),
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
                operation_digest(&[
                    "search.text",
                    arguments["query"].as_str().unwrap_or_default(),
                ])
                .trim_start_matches("sha256:")
                .to_string(),
            ),
            preview: format!(
                "在工作区搜索 {:?}",
                arguments["query"].as_str().unwrap_or_default()
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
        let query = plan.normalized_arguments["query"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let max = plan
            .normalized_arguments
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|n| n.min(1_000) as usize)
            .unwrap_or(DEFAULT_MAX_MATCHES);

        let query_for_task = query.clone();
        let root = self.root.to_path_buf();
        // 遍历是 CPU/IO 混合的同步代码，放到阻塞线程避免卡住运行时。
        let walked = tokio::task::spawn_blocking(move || {
            let mut matches = Vec::new();
            let result = walk(&root, &root, &query_for_task, max, &mut matches);
            (matches, result.err())
        })
        .await
        .map_err(|e| ToolRuntimeError::ExecutionFailed(e.to_string()))?;
        let (matches, walk_err) = walked;
        if let Some(err) = walk_err {
            return Err(ToolRuntimeError::ExecutionFailed(format!(
                "遍历失败: {err}"
            )));
        }

        let truncated = matches.len() >= max;
        let text = format_matches(&matches, truncated);
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
            "codedock-search-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::create_dir_all(dir.join("src/deep"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.join("target")).await.unwrap();
        tokio::fs::write(dir.join("src/a.rs"), "fn main() {}\n// TODO fix\n")
            .await
            .unwrap();
        tokio::fs::write(dir.join("src/deep/b.rs"), "let todo_marker = 1;\n")
            .await
            .unwrap();
        // target 目录必须被跳过
        tokio::fs::write(dir.join("target/junk.rs"), "TODO in target\n")
            .await
            .unwrap();
        // 非 UTF-8 跳过
        tokio::fs::write(dir.join("bin.dat"), [0xffu8, 0xfe, b'T', b'O', b'D', b'O'])
            .await
            .unwrap();
        dir
    }

    fn tool(dir: &PathBuf) -> SearchTextTool {
        SearchTextTool::new(dir)
    }

    #[tokio::test]
    async fn finds_matches_case_insensitive_and_skips_build_dirs() {
        let dir = workspace_dir().await;
        let t = tool(&dir);
        let plan = t.preflight(json!({ "query": "TODO" })).await.unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        let text = match &result.content[0] {
            ToolResultContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("src/deep/b.rs"));
        assert!(!text.contains("target/junk.rs"), "构建目录必须跳过: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_match_reports_cleanly_and_validates_args() {
        let dir = workspace_dir().await;
        let t = tool(&dir);
        let plan = t
            .preflight(json!({ "query": "no-such-thing" }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        assert!(
            matches!(&result.content[0], ToolResultContent::Text { text } if text.contains("未找到"))
        );

        assert!(t.preflight(json!({ "query": "  " })).await.is_err());
        assert!(t.preflight(json!({})).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
