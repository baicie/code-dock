//! Tree-sitter 符号索引（§12.1 #4 / §12.2）。
//!
//! 支持 Rust / Python / TypeScript / JavaScript 四类主流语言；
//! 提取函数、结构体、枚举、trait、类、方法的名称与行范围。
//!
//! 阶段 3 边界：
//! - 索引在内存中，按需全量扫描 + 单文件增量（[`WorkspaceIndex::update_file`]）；
//!   文件监听（notify）与 SQLite 持久化在后续版本接入（§12.2 / §20）；
//! - `.gitignore` 语义暂以 [`crate::DEFAULT_EXCLUDED_DIRS`] 近似。

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Parser;

use crate::{DEFAULT_EXCLUDED_DIRS, MAX_INDEXED_FILE_SIZE, SymbolEntry};

/// 支持的语言（按文件扩展名分派）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
}

fn language_for(path: &str) -> Option<Language> {
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some(Language::Rust),
        "py" => Some(Language::Python),
        "ts" | "tsx" => Some(Language::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        _ => None,
    }
}

/// 提取单个文件中的符号；语言不支持或解析失败返回空。
pub fn extract_symbols(rel_path: &str, source: &str) -> Vec<SymbolEntry> {
    let Some(language) = language_for(rel_path) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    let tree_language = match language {
        Language::Rust => tree_sitter_rust::LANGUAGE,
        Language::Python => tree_sitter_python::LANGUAGE,
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Language::JavaScript => tree_sitter_javascript::LANGUAGE,
    };
    if parser.set_language(&tree_language.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let wanted: &[&str] = match language {
        Language::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "type_item",
        ],
        Language::Python => &["function_definition", "class_definition"],
        Language::TypeScript | Language::JavaScript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "lexical_declaration",
        ],
    };

    let mut symbols = Vec::new();
    collect(
        tree.root_node(),
        source.as_bytes(),
        rel_path,
        wanted,
        &mut symbols,
    );
    symbols.sort_by_key(|s| s.start_line);
    symbols
}

fn collect(
    node: tree_sitter::Node,
    source: &[u8],
    rel_path: &str,
    wanted: &[&str],
    out: &mut Vec<SymbolEntry>,
) {
    if wanted.contains(&node.kind()) {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source) {
                if !name.is_empty() {
                    out.push(SymbolEntry {
                        path: rel_path.to_string(),
                        name: name.to_string(),
                        kind: node.kind().to_string(),
                        start_line: (node.start_position().row + 1) as u32,
                        end_line: (node.end_position().row + 1) as u32,
                    });
                }
            }
        }
    }
    // lexical_declaration（const x = ...）的 name 藏在 declaration 内，特殊处理：
    if node.kind() == "lexical_declaration" {
        if let Some(decl) = node.child(0) {
            if let Some(name_node) = decl.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(source) {
                    out.push(SymbolEntry {
                        path: rel_path.to_string(),
                        name: name.to_string(),
                        kind: "const".into(),
                        start_line: (node.start_position().row + 1) as u32,
                        end_line: (node.end_position().row + 1) as u32,
                    });
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, rel_path, wanted, out);
    }
}

/// 工作区符号索引：内存态，按需全量 + 单文件增量（§12.2）。
#[derive(Default)]
pub struct WorkspaceIndex {
    /// rel_path → 该文件的符号。
    symbols: std::sync::Mutex<HashMap<String, Vec<SymbolEntry>>>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全量扫描工作区（首次索引，§12.2）。
    pub fn index_all(&self, root: &Path) -> std::io::Result<usize> {
        let mut files = Vec::new();
        collect_source_files(root, root, &mut files)?;
        let mut fresh: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
        for rel in files {
            let Ok(content) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            fresh.insert(rel.clone(), extract_symbols(&rel, &content));
        }
        let count: usize = fresh.values().map(|v| v.len()).sum();
        *self.symbols.lock().expect("symbol map poisoned") = fresh;
        Ok(count)
    }

    /// 增量更新单个文件（文件监听回调，§12.2；内容变化时重建该文件符号）。
    pub fn update_file(&self, root: &Path, rel_path: &str) -> usize {
        let entries = std::fs::read_to_string(root.join(rel_path))
            .map(|content| extract_symbols(rel_path, &content))
            .unwrap_or_default();
        let count = entries.len();
        self.symbols
            .lock()
            .expect("symbol map poisoned")
            .insert(rel_path.to_string(), entries);
        count
    }

    /// 索引是否为空（惰性重建的判断依据）。
    pub fn is_empty(&self) -> bool {
        self.symbols.lock().expect("symbol map poisoned").is_empty()
    }

    /// 符号查询：精确匹配优先，其次包含（大小写不敏感）。
    pub fn search_symbol(&self, name: &str) -> Vec<SymbolEntry> {
        let guard = self.symbols.lock().expect("symbol map poisoned");
        let query = name.to_lowercase();
        let mut exact = Vec::new();
        let mut contains = Vec::new();
        for entries in guard.values() {
            for symbol in entries {
                if symbol.name == name {
                    exact.push(symbol.clone());
                } else if symbol.name.to_lowercase().contains(&query) {
                    contains.push(symbol.clone());
                }
            }
        }
        exact.extend(contains);
        exact.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));
        exact
    }
}

/// 收集可索引的源码文件（相对路径，排除构建目录与超大文件）。
fn collect_source_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if DEFAULT_EXCLUDED_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            collect_source_files(root, &path, out)?;
        } else if meta.is_file()
            && meta.len() <= MAX_INDEXED_FILE_SIZE
            && language_for(&name).is_some()
        {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codedock-idx-sym-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "struct Config { a: u32 }\n\nfn main() {\n    let x = 1;\n}\n\nenum Mode { On, Off }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/tool.py"),
            "class Runner:\n    def run(self):\n        pass\n\ndef setup():\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/app.ts"),
            "export function boot(): void {}\n\nexport class App {}\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn extracts_symbols_across_languages() {
        let root = workspace();
        let index = WorkspaceIndex::new();
        index.index_all(&root).unwrap();

        let main_fn = index.search_symbol("main");
        assert_eq!(main_fn.len(), 1);
        assert_eq!(main_fn[0].kind, "function_item");
        assert_eq!(main_fn[0].path, "src/main.rs");
        assert_eq!(main_fn[0].start_line, 3);

        assert!(
            index
                .search_symbol("Config")
                .iter()
                .any(|s| s.kind == "struct_item")
        );
        assert!(
            index
                .search_symbol("Mode")
                .iter()
                .any(|s| s.kind == "enum_item")
        );
        assert!(
            index
                .search_symbol("Runner")
                .iter()
                .any(|s| s.kind == "class_definition" && s.path == "src/tool.py")
        );
        assert!(
            index
                .search_symbol("boot")
                .iter()
                .any(|s| s.path == "src/app.ts")
        );
        // 包含匹配
        assert!(
            index
                .search_symbol("setu")
                .iter()
                .any(|s| s.name == "setup")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_update_replaces_file_symbols() {
        let root = workspace();
        let index = WorkspaceIndex::new();
        index.index_all(&root).unwrap();
        assert!(!index.search_symbol("main").is_empty());

        std::fs::write(root.join("src/main.rs"), "// main 已移除\nfn helper() {}\n").unwrap();
        index.update_file(&root, "src/main.rs");
        assert!(
            index.search_symbol("main").is_empty(),
            "旧符号应随增量更新消失"
        );
        assert_eq!(index.search_symbol("helper").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
