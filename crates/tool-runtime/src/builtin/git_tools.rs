//! `git.status` / `git.diff`：只读 Git 工具（§13.4：低风险读操作）。
//!
//! 固定参数执行（不暴露任意 git 子命令），工作区根即 cwd；
//! effect = none、风险 Low、capability `git.read`。

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use codedock_protocol::{
    Capability, Effect, OperationDigest, Permission, Risk, SideEffect, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResult, ToolResultContent, ToolResultStatus,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    CancellationToken, ToolExecutor, ToolOutputChunk, ToolRuntimeError, builtin::shell_exec,
    operation_digest, validation::validate_arguments,
};

/// 只读 Git 工具（`git.status` / `git.diff`）。
pub struct GitTool {
    name: &'static str,
    argv: Vec<&'static str>,
    description: &'static str,
    preview: &'static str,
    root: std::path::PathBuf,
    definition: std::sync::OnceLock<ToolDefinition>,
}

impl GitTool {
    /// `git status --porcelain=v1 -b`（含分支信息，机器可读）。
    pub fn status(root: impl AsRef<Path>) -> Self {
        Self::new(
            "git.status",
            vec!["git", "status", "--porcelain=v1", "-b"],
            "查看工作区 Git 状态（porcelain 格式）。",
            "查看 Git 状态",
            root,
        )
    }

    /// `git diff --no-color`（工作区未提交变更）。
    pub fn diff(root: impl AsRef<Path>) -> Self {
        Self::new(
            "git.diff",
            vec!["git", "diff", "--no-color"],
            "查看工作区未提交的 Git Diff。",
            "查看 Git Diff",
            root,
        )
    }

    fn new(
        name: &'static str,
        argv: Vec<&'static str>,
        description: &'static str,
        preview: &'static str,
        root: impl AsRef<Path>,
    ) -> Self {
        Self {
            name,
            argv,
            description,
            preview,
            root: root.as_ref().to_path_buf(),
            definition: std::sync::OnceLock::new(),
        }
    }

    fn build_definition(&self) -> ToolDefinition {
        let mut def = ToolDefinition::builtin(self.name, vec![Capability::GitRead]);
        def.description = self.description.to_string();
        def.input_schema = json!({ "type": "object" });
        def.effect = Effect::None;
        def
    }
}

#[async_trait]
impl ToolExecutor for GitTool {
    fn definition(&self) -> &ToolDefinition {
        self.definition.get_or_init(|| self.build_definition())
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: json!({}),
            permissions: vec![Permission {
                capability: Capability::GitRead,
                resource: "$workspace".to_string(),
            }],
            risk: Risk::Low,
            expected_side_effects: vec![SideEffect {
                kind: "git.read".into(),
                resource: "$workspace".to_string(),
                details: None,
            }],
            operation_digest: OperationDigest::from_sha256_hex(
                operation_digest(&[self.name])
                    .trim_start_matches("sha256:")
                    .to_string(),
            ),
            preview: self.preview.to_string(),
        })
    }

    async fn execute(
        &self,
        plan: ToolExecutionPlan,
        output: mpsc::Sender<ToolOutputChunk>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolRuntimeError> {
        let started = std::time::Instant::now();
        let argv: Vec<String> = self.argv.iter().map(|s| s.to_string()).collect();
        let (program, rest) = argv.split_first().expect("argv 非空");
        let (status, text) = shell_exec::run_captured(
            &self.root,
            program,
            rest,
            Duration::from_secs(30),
            &output,
            cancel,
        )
        .await?;
        // git 在非仓库目录等场景返回失败 → 对模型呈现为执行失败。
        if status != ToolResultStatus::Success {
            return Err(ToolRuntimeError::ExecutionFailed(text));
        }

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
    use std::process::Command;

    fn repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-git-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git 可用")
        };
        assert!(git(&["init", "-q"]).status.success());
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-qm", "init"]).status.success());
        dir
    }

    async fn run_in(_dir: &Path, tool: GitTool) -> ToolResult {
        let plan = tool.preflight(json!({})).await.unwrap();
        assert_eq!(plan.risk, Risk::Low);
        let (tx, _rx) = mpsc::channel(64);
        tool.execute(plan, tx, CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn status_reports_clean_tree_then_changes() {
        let dir = repo();
        let text = match &run_in(&dir, GitTool::status(&dir)).await.content[0] {
            ToolResultContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("## "), "分支行应存在: {text}");

        std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        let text = match &run_in(&dir, GitTool::diff(&dir)).await.content[0] {
            ToolResultContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        assert!(text.contains("hello world"), "diff 应包含变更: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_repo_directory_is_an_execution_failure() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-git-nonrepo-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = GitTool::status(&dir).preflight(json!({})).await.unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let err = GitTool::status(&dir)
            .execute(plan, tx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolRuntimeError::ExecutionFailed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
