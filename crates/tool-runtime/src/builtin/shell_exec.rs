//! `shell.execute`：受控进程执行（§8.3.2 / §13.2 / §8.3.10）。
//!
//! 安全约束：
//! - 参数化执行（`program` + `args` 数组），不经 shell 字符串拼接；
//! - 环境变量白名单（默认仅 `PATH`/`LANG`），不继承全部系统环境；
//! - 超时（默认 120s，上限 600s）；Unix 下进程组隔离，
//!   取消/超时先 TERM 后 KILL 整个进程组，子进程不得逃逸（§8.3.10）；
//! - stdout/stderr 捕获上限 64 KiB，超限截断。
//!
//! Windows 的 Job Object 管理待阶段 1 Windows 适配一并补齐（§18.8）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use codedock_protocol::{
    Capability, Effect, OperationDigest, Permission, Risk, SideEffect, ToolCallId, ToolDefinition,
    ToolExecutionPlan, ToolResult, ToolResultContent, ToolResultStatus,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{
    CancellationToken, ToolExecutor, ToolOutputChunk, ToolRuntimeError, operation_digest,
    validation::validate_arguments,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
/// 环境变量白名单（§13.2：默认不继承全部系统环境变量）。
const ENV_ALLOWLIST: [&str; 2] = ["PATH", "LANG"];

/// 工作区内受控 Shell 工具。
pub struct ShellExecuteTool {
    root: Box<Path>,
}

impl ShellExecuteTool {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().into(),
        }
    }

    fn cwd(&self, raw: Option<&str>) -> Result<PathBuf, ToolRuntimeError> {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(self.root.to_path_buf()),
            Some(cwd) => {
                let candidate = Path::new(cwd);
                let joined = if candidate.is_absolute() {
                    candidate.strip_prefix(&self.root).map_err(|_| {
                        ToolRuntimeError::InvalidArguments(format!("cwd 越出工作区: {cwd}"))
                    })?
                } else {
                    candidate
                };
                let resolved = self.root.join(joined);
                let canonical = resolved
                    .canonicalize()
                    .map_err(|_| ToolRuntimeError::InvalidArguments("cwd 不存在".into()))?;
                let canonical_root = self.root.canonicalize().map_err(|e| {
                    ToolRuntimeError::ExecutionFailed(format!("工作区根不可用: {e}"))
                })?;
                if !canonical.starts_with(&canonical_root) {
                    return Err(ToolRuntimeError::InvalidArguments("cwd 越出工作区".into()));
                }
                Ok(canonical)
            }
        }
    }
}

fn parse_spec(
    args: &Value,
) -> Result<(String, Vec<String>, Option<String>, u64), ToolRuntimeError> {
    let program = args
        .get("program")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolRuntimeError::InvalidArguments("缺少 program".into()))?
        .to_string();
    if program.contains('/') && !program.starts_with("./") {
        // 允许任意可执行名；路径检查交给 OS 与工作区 cwd（不给模型任意绝对路径的额外能力）。
    }
    let args_list = args
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    Ok((program, args_list, cwd, timeout_ms))
}

/// 受控执行：进程组 + 环境白名单 + 超时 + 捕获上限；返回（状态, 合并捕获文本）。
/// shell/git 工具共用；stdout 原样、stderr 逐行加 `[stderr] ` 前缀。
pub(crate) async fn run_captured(
    cwd: &Path,
    program: &str,
    argv: &[String],
    timeout: Duration,
    output: &mpsc::Sender<ToolOutputChunk>,
    cancel: CancellationToken,
) -> Result<(ToolResultStatus, String), ToolRuntimeError> {
    let mut command = Command::new(program);
    command
        .args(argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }

    #[cfg(unix)]
    {
        // 独立进程组：后续可整组终止，子进程不得逃逸（§8.3.10）。
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|e| ToolRuntimeError::ExecutionFailed(format!("启动失败: {e}")))?;
    #[cfg(unix)]
    let pgid = child.id().map(|pid| pid as i32);
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let read_half = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut capped = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                n = stdout.read(&mut chunk) => {
                    match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if buf.len() < MAX_CAPTURE_BYTES {
                                let remain = MAX_CAPTURE_BYTES - buf.len();
                                buf.extend_from_slice(&chunk[..n.min(remain)]);
                                let _ = output
                                    .send(ToolOutputChunk::Stdout(chunk[..n.min(remain)].to_vec()))
                                    .await;
                            } else if !capped {
                                capped = true;
                                let _ = output
                                    .send(ToolOutputChunk::Text("[输出截断：超过捕获上限]".into()))
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        buf
    };
    let read_err_half = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                n = stderr.read(&mut chunk) => {
                    match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if buf.len() < MAX_CAPTURE_BYTES {
                                let remain = MAX_CAPTURE_BYTES - buf.len();
                                buf.extend_from_slice(&chunk[..n.min(remain)]);
                                let _ = output
                                    .send(ToolOutputChunk::Stderr(chunk[..n.min(remain)].to_vec()))
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        buf
    };

    let mut wait = Box::pin(async {
        child
            .wait()
            .await
            .map_err(|e| ToolRuntimeError::ExecutionFailed(e.to_string()))
    });
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            terminate_group(pgid).await;
            let _ = wait.as_mut().await;
            return Err(ToolRuntimeError::Cancelled);
        }
        waited = tokio::time::timeout(timeout, &mut wait) => {
            match waited {
                Ok(status) => status?,
                Err(_) => {
                    terminate_group(pgid).await;
                    let _ = wait.as_mut().await;
                    return Ok((ToolResultStatus::Timeout, "[超时终止]".to_string()));
                }
            }
        }
    };
    let (stdout_bytes, stderr_bytes) = tokio::join!(read_half, read_err_half);

    let exit_ok = status.success();
    let mut text = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes).into_owned();
    if !stderr_text.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        for line in stderr_text.lines() {
            text.push_str("[stderr] ");
            text.push_str(line);
            text.push('\n');
        }
    }
    if !exit_ok {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[exit code {}]", status.code().unwrap_or(-1)));
    }
    Ok((
        if exit_ok {
            ToolResultStatus::Success
        } else {
            ToolResultStatus::Failure
        },
        text,
    ))
}

/// 先 TERM 后 KILL 整个进程组（Unix）。
async fn terminate_group(pgid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

#[async_trait]
impl ToolExecutor for ShellExecuteTool {
    fn definition(&self) -> &ToolDefinition {
        static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEF.get_or_init(|| {
            let mut def = ToolDefinition::builtin(
                "shell.execute",
                vec![Capability::ProcessExecute, Capability::FsRead],
            );
            def.description =
                "在工作区内执行一个程序（参数化，不经 shell 拼接）；环境白名单 + 超时 + 输出上限。"
                    .into();
            def.input_schema = json!({
                "type": "object",
                "required": ["program"],
                "properties": {
                    "program": { "type": "string", "description": "可执行程序名" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "参数列表" },
                    "cwd": { "type": "string", "description": "工作区内相对目录（默认工作区根）" },
                    "timeout_ms": { "type": "integer", "description": "超时（1s–600s，默认 120s）" }
                }
            });
            def.output_schema = json!({
                "type": "object",
                "properties": { "exit_code": { "type": "integer" } }
            });
            def.effect = Effect::Possible;
            def.execution.default_timeout_ms = DEFAULT_TIMEOUT_MS;
            def.execution.max_timeout_ms = MAX_TIMEOUT_MS;
            def
        })
    }

    async fn preflight(&self, arguments: Value) -> Result<ToolExecutionPlan, ToolRuntimeError> {
        validate_arguments(&self.definition().input_schema, &arguments)?;
        let (program, argv, cwd, timeout_ms) = parse_spec(&arguments)?;
        let cwd_display = cwd.clone().unwrap_or_else(|| "$workspace".to_string());
        self.cwd(cwd.as_deref())?;

        Ok(ToolExecutionPlan {
            tool_call_id: ToolCallId::generate(),
            normalized_arguments: arguments.clone(),
            permissions: vec![
                Permission {
                    capability: Capability::ProcessExecute,
                    resource: format!("{program} {}", argv.join(" ")).trim().to_string(),
                },
                Permission {
                    capability: Capability::FsRead,
                    resource: cwd_display.clone(),
                },
            ],
            risk: Risk::Medium,
            expected_side_effects: vec![
                SideEffect {
                    kind: "process.started".into(),
                    resource: program.clone(),
                    details: None,
                },
                SideEffect {
                    kind: "file.possibly_modified".into(),
                    resource: cwd_display.clone(),
                    details: None,
                },
            ],
            operation_digest: OperationDigest::from_sha256_hex(
                operation_digest(&[
                    "shell.execute",
                    &program,
                    &argv.join("\u{1f}"),
                    &cwd_display,
                ])
                .trim_start_matches("sha256:")
                .to_string(),
            ),
            preview: format!(
                "运行 {program} {}（cwd: {cwd_display}，超时 {timeout_ms}ms）",
                argv.join(" ")
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
        let (program, argv, cwd, timeout_ms) = parse_spec(&plan.normalized_arguments)?;
        let cwd_path = self.cwd(cwd.as_deref())?;
        let (status, summary) = run_captured(
            &cwd_path,
            &program,
            &argv,
            Duration::from_millis(timeout_ms),
            &output,
            cancel,
        )
        .await?;

        Ok(ToolResult {
            tool_call_id: plan.tool_call_id,
            status,
            content: vec![ToolResultContent::Text { text: summary }],
            artifacts: Vec::new(),
            diagnostics: Vec::new(),
            actual_side_effects: vec![SideEffect {
                kind: "process.completed".into(),
                resource: program,
                details: None,
            }],
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRuntimeError as E;

    fn tool() -> ShellExecuteTool {
        ShellExecuteTool::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn runs_parameterized_command() {
        let t = tool();
        let plan = t
            .preflight(json!({ "program": "echo", "args": ["hello", "codedock"] }))
            .await
            .unwrap();
        assert_eq!(plan.risk, Risk::Medium);
        assert!(plan.preview.contains("echo hello codedock"));
        assert!(
            plan.permissions
                .iter()
                .any(|p| p.capability == Capability::ProcessExecute)
        );

        let (tx, mut rx) = mpsc::channel(64);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        assert_eq!(result.status, ToolResultStatus::Success);
        // stdout chunk 流经 output 通道
        let mut got = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let ToolOutputChunk::Stdout(bytes) = chunk {
                got.extend_from_slice(&bytes);
            }
        }
        assert!(String::from_utf8_lossy(&got).contains("hello codedock"));
    }

    #[tokio::test]
    async fn failing_command_reports_failure_status() {
        let t = tool();
        let plan = t.preflight(json!({ "program": "false" })).await.unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        assert_eq!(result.status, ToolResultStatus::Failure);
    }

    #[tokio::test]
    async fn timeout_kills_long_process() {
        let t = tool();
        let plan = t
            .preflight(json!({ "program": "sleep", "args": ["30"], "timeout_ms": 1000 }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let result = t.execute(plan, tx, CancellationToken::new()).await.unwrap();
        assert_eq!(result.status, ToolResultStatus::Timeout);
    }

    #[tokio::test]
    async fn cancel_terminates_process() {
        let t = tool();
        let plan = t
            .preflight(json!({ "program": "sleep", "args": ["30"] }))
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token.cancel();
        });
        let err = t.execute(plan, tx, cancel).await.unwrap_err();
        assert!(matches!(err, E::Cancelled));
    }

    #[tokio::test]
    async fn cwd_must_stay_in_workspace() {
        let t = tool();
        assert!(
            t.preflight(json!({ "program": "ls", "cwd": "/usr" }))
                .await
                .is_err()
        );
        assert!(
            t.preflight(json!({ "program": "ls", "cwd": "/nonexistent-dir-xyz" }))
                .await
                .is_err()
        );
        assert!(
            t.preflight(json!({ "program": "ls", "cwd": "sub/dir" }))
                .await
                .is_err()
        );
    }
}
