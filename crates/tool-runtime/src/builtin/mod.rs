//! 内置工具（§8.3.3 命名规范 `namespace.action`）。

pub mod file_patch;
pub mod file_read;
pub mod git_tools;
pub mod search_text;
pub mod shell_exec;
pub mod workspace;

pub use file_patch::FilePatchTool;
pub use file_read::FileReadTool;
pub use git_tools::GitTool;
pub use search_text::SearchTextTool;
pub use shell_exec::ShellExecuteTool;
