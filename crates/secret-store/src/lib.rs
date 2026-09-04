//! Secret Store：模型密钥、插件密钥、远程设备密钥（§6）。
//!
//! 硬约束（§11.4 / §17.3）：
//! - API Key 只存操作系统 Keychain / Credential Vault，不写入项目文件或普通 SQLite 字段；
//! - Secret 数据分类默认不进入任何模型上下文（§8.4.6）；
//! - Secret 环境变量不得回显到日志和模型上下文（§13.2）；
//! - 脱敏是统一的数据管线，而不是单独保护几个文件名（§18.5）。
//!
//! TODO(阶段1)：接入 OS Keychain（macOS Keychain / Windows Credential Manager /
//! libsecret）。当前内存实现仅用于开发与测试。

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secret 不存在: {0}")]
    NotFound(String),
    #[error("Keychain 后端不可用: {0}")]
    BackendUnavailable(String),
}

/// Secret 作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScope {
    /// 模型 Provider 密钥，键格式 `provider:<id>`。
    Model,
    /// 插件密钥，键格式 `plugin:<plugin_id>:<key>`。
    Plugin,
    /// 远程设备密钥，键格式 `device:<device_id>`。
    Device,
}

/// Secret 存储抽象。
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<String, SecretError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    async fn delete(&self, key: &str) -> Result<(), SecretError>;
    /// 列出键名（只返回键，永不返回值——防回显，§13.2）。
    async fn keys(&self) -> Result<Vec<String>, SecretError>;
}

/// 规范化键名，避免不同作用域的键冲突。
pub fn scoped_key(scope: SecretScope, id: &str) -> String {
    match scope {
        SecretScope::Model => format!("provider:{id}"),
        SecretScope::Plugin => format!("plugin:{id}"),
        SecretScope::Device => format!("device:{id}"),
    }
}

/// 内存实现：仅限开发与测试，发布构建必须使用 OS Keychain 后端。
#[derive(Default)]
pub struct InMemorySecretStore {
    inner: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SecretStore for InMemorySecretStore {
    async fn get(&self, key: &str) -> Result<String, SecretError> {
        self.inner
            .lock()
            .expect("secret store poisoned")
            .get(key)
            .cloned()
            .ok_or_else(|| SecretError::NotFound(key.to_string()))
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .expect("secret store poisoned")
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.inner
            .lock()
            .expect("secret store poisoned")
            .remove(key)
            .map(|_| ())
            .ok_or_else(|| SecretError::NotFound(key.to_string()))
    }

    async fn keys(&self) -> Result<Vec<String>, SecretError> {
        let mut keys: Vec<String> = self
            .inner
            .lock()
            .expect("secret store poisoned")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }
}

/// 统一脱敏辅助：任何进入日志/上下文的文本先经过这里（§18.5）。
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s, "[REDACTED]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crud_and_key_listing() {
        let store = InMemorySecretStore::new();
        let k = scoped_key(SecretScope::Model, "cloud-a");
        store.set(&k, "sk-test").await.unwrap();
        assert_eq!(store.get(&k).await.unwrap(), "sk-test");
        let keys = store.keys().await.unwrap();
        assert_eq!(keys, vec![k.clone()]);
        assert_eq!(keys[0], "provider:cloud-a");
        store.delete(&k).await.unwrap();
        assert!(store.get(&k).await.is_err());
    }

    #[test]
    fn redaction_is_pipeline_friendly() {
        let out = redact(
            "token is sk-secret123 and more",
            &["sk-secret123".to_string()],
        );
        assert_eq!(out, "token is [REDACTED] and more");
    }
}
