//! 工作区路径 confinement 共享逻辑（§13.1）。
//!
//! 所有文件类内置工具必须经由 [`resolve_in_workspace`] 解析目标路径：
//! 三层防护（`~` 拒绝 / 词法 `..` 逃逸 / 符号链接逃逸）+ 执行前二次解析。

use std::path::{Component, Path, PathBuf};

use crate::ToolRuntimeError;

/// 解析并做 confinement 检查，返回（canonical 绝对路径、展示用相对路径）。
///
/// 三层防护（§13.1）：
/// 1. `~` 前缀直接拒绝（家目录不在工作区内）；
/// 2. 词法归一化后必须仍在 root 内（拒绝 `..` 逃逸与 root 外绝对路径）；
/// 3. canonicalize 后必须仍在 canonical root 内（拒绝符号链接逃逸）。
pub fn resolve_in_workspace(root: &Path, raw: &str) -> Result<(PathBuf, String), ToolRuntimeError> {
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
            .strip_prefix(root)
            .map_err(|_| ToolRuntimeError::InvalidArguments(format!("路径越出工作区: {raw}")))?
            .to_path_buf()
    } else {
        candidate.to_path_buf()
    };

    let normalized = lexical_normalize(&root.join(&relative))?;
    if !normalized.starts_with(root) {
        return Err(ToolRuntimeError::InvalidArguments(format!(
            "路径越出工作区: {raw}"
        )));
    }
    let rel_display = normalized
        .strip_prefix(root)
        .expect("starts_with 已确认前缀")
        .to_string_lossy()
        .into_owned();

    // 符号链接逃逸检查；canonical root 同时防 root 本身是链接。
    let canonical_root = root
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-ws-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    #[test]
    fn rejects_all_escape_forms() {
        let root = temp_root();
        assert!(resolve_in_workspace(&root, "../../etc/passwd").is_err());
        assert!(resolve_in_workspace(&root, "/etc/passwd").is_err());
        assert!(resolve_in_workspace(&root, "~/.ssh/id_rsa").is_err());
        assert!(resolve_in_workspace(&root, "").is_err());
        assert!(resolve_in_workspace(&root, "src/missing.rs").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accepts_relative_and_in_root_absolute() {
        let root = temp_root();
        let (_, rel) = resolve_in_workspace(&root, "src/main.rs").unwrap();
        assert_eq!(rel, "src/main.rs");
        let abs = root.join("src/main.rs");
        let (_, rel2) = resolve_in_workspace(&root, abs.to_str().unwrap()).unwrap();
        assert_eq!(rel2, "src/main.rs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let root = temp_root();
        std::os::unix::fs::symlink("/etc/passwd", root.join("evil")).unwrap();
        assert!(resolve_in_workspace(&root, "evil").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
