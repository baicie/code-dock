//! 插件 Manifest（§10.3 / §10.4），格式为 TOML，包后缀 `.cdplugin`。

use serde::{Deserialize, Serialize};

/// Manifest（对应 §10.4 示例）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub manifest_version: u32,
    /// 如 `com.codedock.android`。
    pub id: String,
    pub name: String,
    pub version: String,
    /// `wasm-component` 或 `native-sidecar`（§10.1）。
    pub runtime: String,
    /// 入口：`component.wasm` 或 sidecar 描述。
    pub entry: String,
    pub min_host_version: String,

    #[serde(default)]
    pub capabilities: PluginCapabilities,
    #[serde(default)]
    pub permissions: PluginPermissions,
}

/// 插件能力声明（§10.2）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub tool_provider: bool,
    #[serde(default)]
    pub context_provider: bool,
    #[serde(default)]
    pub model_provider: bool,
    #[serde(default)]
    pub workflow_provider: bool,
    #[serde(default)]
    pub ui_panel: bool,
    /// Policy Extension 只能增加限制，不能绕过 Runtime 最低安全策略（§10.2）。
    #[serde(default)]
    pub policy_extension: bool,
}

/// 权限声明：必须具体到资源范围（§10.4）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginPermissions {
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    /// 可执行进程白名单，如 `["adb", "gradlew"]`。
    pub process: Vec<String>,
    /// 允许的网络域名。
    pub network: Vec<String>,
    pub secrets: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_design_doc_example() {
        let toml_src = r#"
manifest_version = 1
id = "com.codedock.android"
name = "Android Development Tools"
version = "0.1.0"
runtime = "wasm-component"
entry = "android-tools.wasm"
min_host_version = "1.0.0"

[capabilities]
tool_provider = true
context_provider = true
ui_panel = true

[permissions]
fs_read = ["$workspace/**"]
fs_write = ["$workspace/**"]
process = ["adb", "gradlew"]
network = ["developer.android.com"]
secrets = []
"#;
        let m: PluginManifest = toml::from_str(toml_src).unwrap();
        assert_eq!(m.id, "com.codedock.android");
        assert!(m.capabilities.tool_provider);
        assert_eq!(m.permissions.process, vec!["adb", "gradlew"]);
        assert_eq!(m.permissions.network, vec!["developer.android.com"]);
    }
}
