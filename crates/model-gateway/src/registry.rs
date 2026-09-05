//! Provider 注册表（§11.2）。
//!
//! Daemon 装配时把可用 Provider 注册进来；会话对话默认走 default Provider。
//! 按任务类型的路由（§11.3）在接入 Context Engine 后按 `RoutingConfig` 扩展。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ModelProvider;

/// 已注册 Provider 集合 + 默认 Provider。
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    default: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 Provider；首个注册者自动成为默认。
    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) {
        if self.default.is_none() {
            self.default = Some(provider.id().to_string());
        }
        self.providers.insert(provider.id().to_string(), provider);
    }

    /// 指定默认 Provider（必须已注册）。
    pub fn set_default(&mut self, id: &str) -> Result<(), String> {
        if !self.providers.contains_key(id) {
            return Err(format!("默认 Provider `{id}` 未注册"));
        }
        self.default = Some(id.to_string());
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ModelProvider>> {
        self.providers.get(id).cloned()
    }

    /// 默认 Provider（未注册任何 Provider 时为 None）。
    pub fn default_provider(&self) -> Option<Arc<dyn ModelProvider>> {
        self.default.as_deref().and_then(|id| self.get(id))
    }

    /// 已注册 Provider id（字典序，用于诊断展示）。
    pub fn ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockProvider;

    #[test]
    fn first_registered_becomes_default() {
        let mut reg = ProviderRegistry::new();
        assert!(reg.default_provider().is_none());

        reg.register(Arc::new(MockProvider::new("mock", "m1")));
        reg.register(Arc::new(MockProvider::new("cloud", "m2")));

        assert_eq!(reg.ids(), vec!["cloud".to_string(), "mock".to_string()]);
        assert_eq!(reg.default_provider().unwrap().id(), "mock");

        reg.set_default("cloud").unwrap();
        assert_eq!(reg.default_provider().unwrap().id(), "cloud");

        assert!(reg.set_default("missing").is_err());
        assert!(reg.get("missing").is_none());
    }
}
