//! Daemon 配置。
//!
//! 模型层配置（§11.2/§11.3）从 TOML 文件加载（默认 `~/.codedock/config.toml`）；
//! 文件不存在时回退到内置 Mock Provider——daemon 开箱即用、测试零外部依赖。
//! API Key 永远不出现在配置文件里，只经 SecretStore（§11.4）。

use std::collections::BTreeMap;
use std::path::Path;

use codedock_model_gateway::SessionBudgetLimits;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// 数据目录（SQLite、Blob Store、索引）。
    pub data_dir: String,
    pub socket_path: String,
    pub model: ModelLayerConfig,
}

/// 模型层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelLayerConfig {
    /// 默认 Provider id（纯对话 Turn 的路由目标）。
    #[serde(default = "default_provider_id")]
    pub default_provider: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderSettings>,
    #[serde(default)]
    pub limits: SessionBudgetLimits,
}

/// 单个 Provider 的配置（`type` 决定变体，不含密钥）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderSettings {
    /// 内置 Mock（开发 / 测试）。
    Mock {
        #[serde(default = "default_model")]
        model: String,
    },
    /// OpenAI-Compatible（OpenAI / Ollama / LM Studio / vLLM，§11.2）。
    #[serde(rename = "openai_compatible")]
    OpenAICompatible {
        base_url: String,
        model: String,
        #[serde(default = "default_context_window")]
        context_window: u64,
    },
}

impl Default for ModelLayerConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "mock".to_string(),
            ProviderSettings::Mock {
                model: "mock-model".to_string(),
            },
        );
        Self {
            default_provider: default_provider_id(),
            providers,
            limits: SessionBudgetLimits::default(),
        }
    }
}

impl ModelLayerConfig {
    /// 从 TOML 文件加载；文件不存在时返回内置默认（Mock）。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "未找到配置文件，使用内置 Mock Provider");
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("读取配置失败: {}", path.display()))
                );
            }
        };
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("解析配置失败 ({}): {e}", path.display()))?;
        if cfg.providers.is_empty() {
            anyhow::bail!("配置未定义任何 [providers.*]");
        }
        if !cfg.providers.contains_key(&cfg.default_provider) {
            anyhow::bail!(
                "default_provider `{}` 未在 [providers.*] 中定义",
                cfg.default_provider
            );
        }
        Ok(cfg)
    }
}

fn default_provider_id() -> String {
    "mock".to_string()
}

fn default_model() -> String {
    "mock-model".to_string()
}

fn default_context_window() -> u64 {
    128_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_file_falls_back_to_mock() {
        let cfg = ModelLayerConfig::load(Path::new("/nonexistent/codedock.toml")).unwrap();
        assert_eq!(cfg.default_provider, "mock");
        assert!(matches!(
            cfg.providers.get("mock"),
            Some(ProviderSettings::Mock { .. })
        ));
    }

    #[test]
    fn toml_providers_and_limits_parse() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-cfg-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
default_provider = "local"

[providers.local]
type = "openai_compatible"
base_url = "http://127.0.0.1:11434/v1"
model = "qwen-coder"
context_window = 32000

[limits]
max_turns = 10
max_total_tokens = 100000
"#,
        )
        .unwrap();

        let cfg = ModelLayerConfig::load(&path).unwrap();
        assert_eq!(cfg.default_provider, "local");
        match cfg.providers.get("local") {
            Some(ProviderSettings::OpenAICompatible {
                base_url,
                model,
                context_window,
            }) => {
                assert_eq!(base_url, "http://127.0.0.1:11434/v1");
                assert_eq!(model, "qwen-coder");
                assert_eq!(*context_window, 32000);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(cfg.limits.max_turns, 10);
        assert_eq!(cfg.limits.max_total_tokens, 100_000);
        // 未给出的字段使用默认值
        assert_eq!(cfg.limits.max_model_calls, 400);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_config_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-cfg-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let bad_default = dir.join("bad-default.toml");
        std::fs::write(&bad_default, "[providers.a]\ntype = \"mock\"\n").unwrap();
        assert!(ModelLayerConfig::load(&bad_default).is_err());

        let empty = dir.join("empty.toml");
        std::fs::write(&empty, "default_provider = \"a\"\n").unwrap();
        assert!(ModelLayerConfig::load(&empty).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
