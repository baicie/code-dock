//! Project Index：文件、文本、符号、依赖和 Git 增量索引（§6 / §12.2）。
//!
//! 索引规则（§12.2）：
//! - 默认遵循 `.gitignore`、`.ignore` 和项目排除配置；
//! - 二进制、大文件、构建目录默认不索引；
//! - 文件监听增量更新；
//! - 中型项目二次索引为增量更新，不做全量重建（§20）。
//!
//! TODO(阶段3)：ripgrep 文本搜索（`search.text`）与 Tree-sitter 符号索引（`search.symbol`）。

use async_trait::async_trait;
use codedock_protocol::SourceKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("workspace 路径无效: {0}")]
    InvalidWorkspace(String),
    #[error("索引失败: {0}")]
    Io(String),
}

/// 文件条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对 workspace 根的路径。
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: chrono::DateTime<chrono::Utc>,
    /// 当前内容 SHA-256（用于 §12.2 的 Hash 冲突检测）。
    pub content_sha256: String,
}

/// 符号条目（Tree-sitter 产出）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub path: String,
    pub name: String,
    /// `function` / `class` / `method` / `struct` ...（Tree-sitter kind）
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// 项目索引抽象。
#[async_trait]
pub trait ProjectIndex: Send + Sync {
    /// 全量扫描（首次索引）。
    async fn index_all(&mut self, workspace_root: &str) -> Result<(), IndexError>;

    /// 增量更新单个文件（文件监听回调）。
    async fn update_file(&mut self, path: &str) -> Result<(), IndexError>;

    /// 关键词搜索（ripgrep 语义，`search.text` 工具的底层）。
    async fn search_text(&self, pattern: &str, limit: usize) -> Result<Vec<TextMatch>, IndexError>;

    /// 符号查询（`search.symbol` 工具的底层）。
    async fn search_symbol(&self, name: &str) -> Result<Vec<SymbolEntry>, IndexError>;
}

/// 文本搜索结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMatch {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
    /// 该结果重新进入上下文时的信任级别：Tool Output / 仓库内容为不可信数据（§18.1）。
    pub source: SourceKind,
}

/// 索引默认排除的目录（构建目录等）。
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".git",
    ".venv",
    "vendor",
    "__pycache__",
];

/// 默认不索引的大文件阈值（字节）。
pub const MAX_INDEXED_FILE_SIZE: u64 = 1_048_576;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dirs_excluded_by_default() {
        assert!(DEFAULT_EXCLUDED_DIRS.contains(&"node_modules"));
        assert!(DEFAULT_EXCLUDED_DIRS.contains(&"target"));
        assert!(DEFAULT_EXCLUDED_DIRS.contains(&".git"));
    }
}
