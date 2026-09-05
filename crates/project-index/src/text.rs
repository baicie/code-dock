//! 关键词文本搜索（ripgrep 语义的纯 Rust 实现，§12.1 #3）。
//!
//! `search.text` 工具与 Context 检索共用此实现；ripgrep 二进制/FTS5 在
//! 中型项目性能瓶颈出现时替换（§20：增量更新不做全量重建）。
//! 排除规则与符号索引一致（[`crate::DEFAULT_EXCLUDED_DIRS`]）。

use std::path::Path;

use crate::{DEFAULT_EXCLUDED_DIRS, MAX_INDEXED_FILE_SIZE, TextMatch};
use codedock_protocol::SourceKind;

const MAX_LINE_CHARS: usize = 200;

/// 在工作区内做大小写不敏感的子串搜索。
///
/// 返回按（文件路径、行号）排序的命中；`limit` 封顶防止上下文爆炸。
pub fn search_text(root: &Path, pattern: &str, limit: usize) -> std::io::Result<Vec<TextMatch>> {
    let query = pattern.to_lowercase();
    let mut matches = Vec::new();
    walk(root, root, &query, limit, &mut matches)?;
    Ok(matches)
}

fn walk(
    root: &Path,
    dir: &Path,
    query: &str,
    limit: usize,
    out: &mut Vec<TextMatch>,
) -> std::io::Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= limit {
            return Ok(());
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if DEFAULT_EXCLUDED_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, query, limit, out)?;
        } else if meta.is_file() && meta.len() <= MAX_INDEXED_FILE_SIZE {
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            // 非 UTF-8（二进制）文件跳过（§12.2）。
            let Ok(text) = String::from_utf8(content) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            for (idx, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(query) {
                    let column = line.to_lowercase().find(query).map(|c| c + 1).unwrap_or(1);
                    out.push(TextMatch {
                        path: rel.clone(),
                        line: (idx + 1) as u32,
                        column: column as u32,
                        text: line.chars().take(MAX_LINE_CHARS).collect(),
                        source: SourceKind::File,
                    });
                    if out.len() >= limit {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-idx-text-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {}\n// TODO fix\n").unwrap();
        std::fs::write(dir.join("src/deep/b.rs"), "let todo_marker = 1;\n").unwrap();
        std::fs::write(dir.join("target/junk.rs"), "TODO in target\n").unwrap();
        dir
    }

    #[test]
    fn finds_matches_case_insensitive_and_skips_build_dirs() {
        let root = workspace();
        let matches = search_text(&root, "TODO", 100).unwrap();
        let paths: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
        assert!(paths.contains(&"src/a.rs"));
        assert!(paths.contains(&"src/deep/b.rs"));
        assert!(!paths.iter().any(|p| p.starts_with("target/")));
        let first = matches.iter().find(|m| m.path == "src/a.rs").unwrap();
        assert_eq!(first.line, 2);
        assert_eq!(first.column, 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn limit_caps_results() {
        let root = workspace();
        let matches = search_text(&root, "todo", 1).unwrap();
        assert_eq!(matches.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
