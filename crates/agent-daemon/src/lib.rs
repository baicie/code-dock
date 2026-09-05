//! CodeDock Agent Daemon 库：进程生命周期、RPC、客户端连接、模块装配（§6）。
//!
//! 拆分为 lib + bin：bin（`codedock-daemon`）只负责 CLI 参数与启动，
//! 可测试的装配与 IPC 逻辑在 lib 中。
//!
//! [`Runtime`] 是 RPC 层可见的模块组合：会话管理（状态机）+ Turn 引擎（模型调用）。

pub mod config;
pub mod ipc;
pub mod rpc;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use codedock_event_store::{InMemoryEventStore, SqliteEventStore};
use codedock_model_gateway::{
    MockProvider, OpenAICompatibleProvider, OpenAIProviderConfig, ProviderRegistry,
};
use codedock_secret_store::{SecretScope, SecretStore, scoped_key};
use codedock_session_engine::{EventSourcedSessionManager, SessionManager, TurnEngine};

use crate::config::{ModelLayerConfig, ProviderSettings};

/// RPC 层可见的运行时模块组合。
pub struct Runtime {
    pub sessions: Arc<dyn SessionManager>,
    pub turns: Arc<TurnEngine>,
}

/// 依据配置构建 Provider 注册表（§11.2）；OpenAI-Compatible 的密钥
/// 由 [`load_env_secrets`] 预先注入 SecretStore，运行时按需读取。
pub fn build_registry(
    model: &ModelLayerConfig,
    secrets: &Arc<dyn SecretStore>,
) -> anyhow::Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    for (id, settings) in &model.providers {
        let provider: Arc<dyn codedock_model_gateway::ModelProvider> = match settings {
            ProviderSettings::Mock { model } => {
                Arc::new(MockProvider::new(id.clone(), model.clone()))
            }
            ProviderSettings::OpenAICompatible {
                base_url,
                model,
                context_window,
            } => Arc::new(OpenAICompatibleProvider::new(
                id.clone(),
                OpenAIProviderConfig {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    context_window: *context_window,
                },
                secrets.clone(),
            )),
        };
        registry.register(provider);
    }
    registry
        .set_default(&model.default_provider)
        .map_err(anyhow::Error::msg)
        .context("装配 Provider 注册表失败")?;
    Ok(registry)
}

/// OpenAI-Compatible Provider 的密钥环境变量名：`CODEDOCK_PROVIDER_<ID>_API_KEY`。
pub fn provider_env_key(id: &str) -> String {
    let upper: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_ascii_uppercase();
    format!("CODEDOCK_PROVIDER_{upper}_API_KEY")
}

/// 启动时从环境变量注入密钥到 SecretStore（阶段 1 的开发路径；
/// TODO：接入 OS Keychain，§11.4）。永不记录密钥值。
pub async fn load_env_secrets(model: &ModelLayerConfig, secrets: &Arc<dyn SecretStore>) {
    for id in model.providers.keys() {
        let Ok(value) = std::env::var(provider_env_key(id)) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if let Err(err) = secrets
            .set(&scoped_key(SecretScope::Model, id), &value)
            .await
        {
            tracing::warn!(provider = %id, %err, "注入环境变量密钥失败");
        } else {
            tracing::info!(provider = %id, "已从环境变量注入 API Key");
        }
    }
}

/// 装配一个内存 Runtime（开发 / 测试用，内置 Mock Provider）。
pub async fn assemble_in_memory() -> Arc<Runtime> {
    let event_store: Arc<dyn codedock_event_store::EventStore> =
        Arc::new(InMemoryEventStore::new());
    Arc::new(assemble_runtime(event_store, &ModelLayerConfig::default()).await)
}

/// 生产装配：SQLite Event Store（WAL + 显式版本迁移，§18.9），
/// 从磁盘事件流重建会话 Projection、幂等缓存（§17.1），并装配模型层。
pub async fn assemble_sqlite(data_dir: &Path, model: &ModelLayerConfig) -> anyhow::Result<Runtime> {
    let event_store: Arc<dyn codedock_event_store::EventStore> = Arc::new(
        SqliteEventStore::open(data_dir.join("events.db"))
            .await
            .context("打开 SQLite Event Store 失败")?,
    );
    Ok(assemble_runtime(event_store, model).await)
}

/// 公共装配路径：Event Store → 会话管理 → 密钥/Provider 注册表 → Turn 引擎。
async fn assemble_runtime(
    event_store: Arc<dyn codedock_event_store::EventStore>,
    model: &ModelLayerConfig,
) -> Runtime {
    let sessions: Arc<dyn SessionManager> = Arc::new(
        EventSourcedSessionManager::restore(event_store.clone())
            .await
            .expect("重建会话 Projection 失败"),
    );

    let secrets: Arc<dyn SecretStore> = Arc::new(codedock_secret_store::InMemorySecretStore::new());
    load_env_secrets(model, &secrets).await;
    let registry = build_registry(model, &secrets).expect("装配 Provider 注册表失败");

    let turns = Arc::new(
        TurnEngine::restore(
            event_store,
            sessions.clone(),
            Arc::new(registry),
            model.limits,
        )
        .await
        .expect("重建 Turn 幂等缓存失败"),
    );

    Runtime { sessions, turns }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{SessionMode, SessionStatus};
    use codedock_secret_store::InMemorySecretStore;

    #[test]
    fn registry_built_from_config_respects_default() {
        let mut model = ModelLayerConfig::default();
        model.providers.insert(
            "second".to_string(),
            ProviderSettings::Mock { model: "m".into() },
        );
        model.default_provider = "second".to_string();
        let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());

        let registry = build_registry(&model, &secrets).unwrap();
        assert_eq!(registry.default_provider().unwrap().id(), "second");
        assert_eq!(registry.ids().len(), 2);
    }

    #[tokio::test]
    async fn env_secrets_are_loaded_into_store_without_echo() {
        // SAFETY: 测试进程内单线程操作环境变量（edition 2024 中 set_var 为 unsafe）。
        let key = provider_env_key("env-test");
        unsafe { std::env::set_var(&key, "sk-env-test-value") };
        let mut model = ModelLayerConfig::default();
        model.providers.insert(
            "env-test".to_string(),
            ProviderSettings::Mock { model: "m".into() },
        );
        let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());

        load_env_secrets(&model, &secrets).await;
        unsafe { std::env::remove_var(&key) };

        let loaded = secrets
            .get(&scoped_key(SecretScope::Model, "env-test"))
            .await
            .unwrap();
        assert_eq!(loaded, "sk-env-test-value");
    }

    #[test]
    fn provider_env_key_is_sanitized() {
        assert_eq!(
            provider_env_key("cloud-a"),
            "CODEDOCK_PROVIDER_CLOUD_A_API_KEY"
        );
        assert_eq!(
            provider_env_key("local.vllm"),
            "CODEDOCK_PROVIDER_LOCAL_VLLM_API_KEY"
        );
    }

    #[tokio::test]
    async fn sqlite_assembly_restores_state_across_restart() {
        let dir = std::env::temp_dir().join(format!(
            "codedock-daemon-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let model = ModelLayerConfig::default();

        let session_id;
        {
            let runtime = assemble_sqlite(&dir, &model).await.unwrap();
            let info = runtime
                .sessions
                .create(
                    SessionMode::Plan,
                    Some("持久化任务".into()),
                    Some("k1".into()),
                )
                .await
                .unwrap();
            runtime.sessions.pause(info.session_id, None).await.unwrap();
            session_id = info.session_id;
        }
        // 模拟重启：重新装配，状态与幂等缓存都应恢复。
        {
            let runtime = assemble_sqlite(&dir, &model).await.unwrap();
            let info = runtime.sessions.status(session_id).await.unwrap();
            assert_eq!(info.status, SessionStatus::Paused);

            let again = runtime
                .sessions
                .create(
                    SessionMode::Plan,
                    Some("持久化任务".into()),
                    Some("k1".into()),
                )
                .await
                .unwrap();
            assert_eq!(again.session_id, session_id, "幂等键跨重启仍命中同一会话");

            // Turn 引擎的幂等缓存同样从事件流恢复（会话需处于 Running）。
            runtime.sessions.resume(session_id, None).await.unwrap();
            let out = runtime
                .turns
                .send_message(session_id, "你好", Some("turn-k1".into()))
                .await
                .unwrap();
            assert_eq!(out.text, "echo: 你好", "Mock Provider 回声");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
