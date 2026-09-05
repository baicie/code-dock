//! SQLite 持久化 Event Store（§17.1 / §18.9）。
//!
//! - WAL + `synchronous = NORMAL`，兼顾持久性与写入吞吐（§20.1）；
//! - `schema_migrations` 显式版本迁移：升级失败即报错退出，绝不带病运行，
//!   历史数据保持可读（§18.9）；
//! - 事件表 Append-only，`(session_id, sequence)` 唯一约束保证 sequence 分配
//!   严格单调；append 在事务内完成"分配 sequence + 写入"。

use std::path::Path;

use chrono::{DateTime, Utc};
use codedock_protocol::{ActorKind, Durability, EventEnvelope, EventId, SessionId, TurnId};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use crate::{EventStore, EventStoreError};

/// 显式版本迁移表（§18.9）。新版本只在末尾追加，禁止修改历史迁移。
const MIGRATIONS: &[(i64, &str, &str)] = &[(
    1,
    "create_events_table",
    "
    CREATE TABLE IF NOT EXISTS events (
        schema_version      TEXT    NOT NULL,
        event_id            TEXT    NOT NULL PRIMARY KEY,
        session_id          TEXT    NOT NULL,
        turn_id             TEXT,
        sequence            INTEGER NOT NULL,
        event_type          TEXT    NOT NULL,
        durability          TEXT    NOT NULL,
        occurred_at         TEXT    NOT NULL,
        actor_kind          TEXT    NOT NULL,
        actor_id            TEXT    NOT NULL,
        correlation_id      TEXT,
        causation_event_id  TEXT,
        payload             TEXT    NOT NULL,
        UNIQUE (session_id, sequence)
    );
    CREATE INDEX IF NOT EXISTS idx_events_session_sequence ON events (session_id, sequence);
    ",
)];

const SELECT_COLUMNS: &str = "
    SELECT schema_version, event_id, session_id, turn_id, sequence, event_type,
           durability, occurred_at, actor_kind, actor_id, correlation_id,
           causation_event_id, payload
    FROM events";

/// SQLite 实现（阶段 1 默认存储）。
pub struct SqliteEventStore {
    pool: SqlitePool,
    /// 阶段 1 单进程写入；串行化 append，避免 sequence 并发分配竞争。
    write_lock: tokio::sync::Mutex<()>,
}

fn db_err(err: sqlx::Error) -> EventStoreError {
    EventStoreError::Db(err.to_string())
}

fn durability_str(durability: Durability) -> &'static str {
    match durability {
        Durability::Durable => "durable",
        Durability::Transient => "transient",
    }
}

fn actor_kind_str(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::User => "user",
        ActorKind::Agent => "agent",
        ActorKind::System => "system",
        ActorKind::Tool => "tool",
        ActorKind::Plugin => "plugin",
    }
}

impl SqliteEventStore {
    /// 打开（或创建）位于 `path` 的数据库文件，并执行未应用的迁移。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, EventStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    EventStoreError::Connect(format!("创建数据目录 {parent:?} 失败: {e}"))
                })?;
            }
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|e| EventStoreError::Connect(e.to_string()))?;

        let store = Self {
            pool,
            write_lock: tokio::sync::Mutex::new(()),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// 应用所有未执行的迁移（§18.9：显式版本 + 迁移记录表）。
    async fn migrate(&self) -> Result<(), EventStoreError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version     INTEGER PRIMARY KEY,
                name        TEXT NOT NULL,
                applied_at  TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        let applied: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_migrations")
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;

        for &(version, name, sql) in MIGRATIONS {
            if version <= applied {
                continue;
            }
            let mut tx = self.pool.begin().await.map_err(db_err)?;
            for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                sqlx::query(statement)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| EventStoreError::Migration {
                        version,
                        message: e.to_string(),
                    })?;
            }
            sqlx::query(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?, ?, ?)",
            )
            .bind(version)
            .bind(name)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await
            .map_err(|e| EventStoreError::Migration {
                version,
                message: e.to_string(),
            })?;
            tx.commit().await.map_err(db_err)?;
            tracing::info!(version, name, "已应用数据库迁移");
        }
        Ok(())
    }
}

fn envelope_from_row(row: &SqliteRow) -> Result<EventEnvelope, EventStoreError> {
    let parse_uuid = |value: &str, what: &str| {
        uuid::Uuid::parse_str(value)
            .map_err(|e| EventStoreError::Db(format!("解析 {what} 失败: {e}")))
    };

    let event_id = EventId(parse_uuid(row.get("event_id"), "event_id")?);
    let session_id = SessionId(parse_uuid(row.get("session_id"), "session_id")?);
    let turn_id: Option<String> = row.get("turn_id");
    let turn_id = match turn_id {
        Some(raw) => Some(TurnId(parse_uuid(&raw, "turn_id")?)),
        None => None,
    };
    let sequence: i64 = row.get("sequence");
    let durability: String = row.get("durability");
    let durability = match durability.as_str() {
        "durable" => Durability::Durable,
        "transient" => Durability::Transient,
        other => return Err(EventStoreError::Db(format!("未知 durability: {other}"))),
    };
    let occurred_at: String = row.get("occurred_at");
    let occurred_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&occurred_at)
        .map_err(|e| EventStoreError::Db(format!("解析 occurred_at 失败: {e}")))?
        .with_timezone(&Utc);
    let actor_kind: String = row.get("actor_kind");
    let kind = match actor_kind.as_str() {
        "user" => ActorKind::User,
        "agent" => ActorKind::Agent,
        "system" => ActorKind::System,
        "tool" => ActorKind::Tool,
        "plugin" => ActorKind::Plugin,
        other => return Err(EventStoreError::Db(format!("未知 actor kind: {other}"))),
    };
    let payload: String = row.get("payload");
    let payload = serde_json::from_str(&payload)
        .map_err(|e| EventStoreError::Db(format!("解析 payload 失败: {e}")))?;
    let causation: Option<String> = row.get("causation_event_id");
    let causation_event_id = match causation {
        Some(raw) => Some(EventId(parse_uuid(&raw, "causation_event_id")?)),
        None => None,
    };

    Ok(EventEnvelope {
        schema_version: row.get("schema_version"),
        event_id,
        session_id,
        turn_id,
        sequence: sequence.max(0) as u64,
        event_type: row.get("event_type"),
        durability,
        occurred_at,
        actor: codedock_protocol::Actor {
            kind,
            id: row.get("actor_id"),
        },
        correlation_id: row.get("correlation_id"),
        causation_event_id,
        payload,
    })
}

#[async_trait::async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&self, mut envelope: EventEnvelope) -> Result<u64, EventStoreError> {
        let _guard = self.write_lock.lock().await;
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE session_id = ?",
        )
        .bind(envelope.session_id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let sequence = u64::try_from(next.max(1)).expect("sequence 非负");
        envelope.sequence = sequence;

        sqlx::query(
            "INSERT INTO events (
                schema_version, event_id, session_id, turn_id, sequence, event_type,
                durability, occurred_at, actor_kind, actor_id, correlation_id,
                causation_event_id, payload
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&envelope.schema_version)
        .bind(envelope.event_id.to_string())
        .bind(envelope.session_id.to_string())
        .bind(envelope.turn_id.map(|t| t.to_string()))
        .bind(i64::try_from(sequence).expect("sequence 在 i64 范围内"))
        .bind(&envelope.event_type)
        .bind(durability_str(envelope.durability))
        .bind(envelope.occurred_at.to_rfc3339())
        .bind(actor_kind_str(envelope.actor.kind))
        .bind(&envelope.actor.id)
        .bind(envelope.correlation_id.clone())
        .bind(envelope.causation_event_id.map(|e| e.to_string()))
        .bind(
            serde_json::to_string(&envelope.payload)
                .map_err(|e| EventStoreError::Db(format!("序列化 payload 失败: {e}")))?,
        )
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(sequence)
    }

    async fn load(
        &self,
        session_id: SessionId,
        after_sequence: u64,
        limit: usize,
        durable_only: bool,
    ) -> Result<Vec<EventEnvelope>, EventStoreError> {
        let sql = if durable_only {
            format!(
                "{SELECT_COLUMNS} WHERE session_id = ? AND sequence > ? AND durability = 'durable' ORDER BY sequence LIMIT ?"
            )
        } else {
            format!(
                "{SELECT_COLUMNS} WHERE session_id = ? AND sequence > ? ORDER BY sequence LIMIT ?"
            )
        };
        let rows = sqlx::query(&sql)
            .bind(session_id.to_string())
            .bind(i64::try_from(after_sequence).unwrap_or(i64::MAX))
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.iter().map(envelope_from_row).collect()
    }

    async fn latest_sequence(&self, session_id: SessionId) -> Result<u64, EventStoreError> {
        let latest: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?",
        )
        .bind(session_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(latest.max(0) as u64)
    }

    async fn load_all_sessions(
        &self,
    ) -> Result<Vec<(SessionId, Vec<EventEnvelope>)>, EventStoreError> {
        let rows = sqlx::query(&format!("{SELECT_COLUMNS} ORDER BY session_id, sequence"))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        // ORDER BY session_id 保证同一会话的事件相邻，按序分组。
        let mut sessions: Vec<(SessionId, Vec<EventEnvelope>)> = Vec::new();
        for row in &rows {
            let envelope = envelope_from_row(row)?;
            match sessions.last_mut() {
                Some((sid, events)) if *sid == envelope.session_id => events.push(envelope),
                _ => sessions.push((envelope.session_id, vec![envelope])),
            }
        }
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::{Actor, EventId, TurnId};

    fn draft(session: SessionId, event_type: &str, durability: Durability) -> EventEnvelope {
        EventEnvelope::draft(
            session,
            None,
            event_type,
            durability,
            Actor::system("test"),
            serde_json::json!({}),
        )
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "codedock-es-{}-{}-{tag}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ))
    }

    #[tokio::test]
    async fn events_persist_across_reopen() {
        let dir = temp_dir("persist");
        let path = dir.join("events.db");
        let s = SessionId::generate();
        {
            let store = SqliteEventStore::open(&path).await.unwrap();
            assert_eq!(
                store
                    .append(draft(s, "session.created", Durability::Durable))
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                store
                    .append(draft(s, "message.delta", Durability::Transient))
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                store
                    .append(draft(s, "message.completed", Durability::Durable))
                    .await
                    .unwrap(),
                3
            );
        }

        let store = SqliteEventStore::open(&path).await.unwrap();
        assert_eq!(store.latest_sequence(s).await.unwrap(), 3);
        let all = store.load(s, 0, 100, false).await.unwrap();
        assert_eq!(all.len(), 3);
        let durable = store.load(s, 0, 100, true).await.unwrap();
        assert_eq!(durable.len(), 2, "补发只包含 durable 事件");

        // 重启后 sequence 继续递增，不复用。
        assert_eq!(
            store
                .append(draft(s, "turn.started", Durability::Durable))
                .await
                .unwrap(),
            4
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn migrations_are_recorded_once() {
        let dir = temp_dir("migrate");
        let path = dir.join("events.db");
        for _ in 0..2 {
            SqliteEventStore::open(&path).await.unwrap();
        }
        let store = SqliteEventStore::open(&path).await.unwrap();
        let (count, max_version): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(MAX(version), 0) FROM schema_migrations")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!((count, max_version), (1, 1), "重复打开不重复迁移");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_sessions_groups_by_session() {
        let dir = temp_dir("group");
        let store = SqliteEventStore::open(dir.join("events.db")).await.unwrap();
        let a = SessionId::generate();
        let b = SessionId::generate();
        store
            .append(draft(a, "session.created", Durability::Durable))
            .await
            .unwrap();
        store
            .append(draft(b, "session.created", Durability::Durable))
            .await
            .unwrap();
        store
            .append(draft(a, "session.started", Durability::Durable))
            .await
            .unwrap();

        let groups = store.load_all_sessions().await.unwrap();
        assert_eq!(groups.len(), 2);
        for (sid, events) in &groups {
            assert!(!events.is_empty());
            assert!(events.iter().all(|e| e.session_id == *sid));
            assert!(events.windows(2).all(|w| w[0].sequence < w[1].sequence));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn envelope_roundtrip_preserves_fields() {
        let dir = temp_dir("roundtrip");
        let store = SqliteEventStore::open(dir.join("events.db")).await.unwrap();
        let s = SessionId::generate();
        let mut envelope = EventEnvelope::draft(
            s,
            Some(TurnId::generate()),
            "turn.started",
            Durability::Durable,
            Actor::agent("model-x"),
            serde_json::json!({ "k": "v", "n": 1 }),
        );
        envelope.correlation_id = Some("corr-1".into());
        envelope.causation_event_id = Some(EventId::generate());

        store.append(envelope.clone()).await.unwrap();
        let loaded = store.load(s, 0, 10, false).await.unwrap();
        assert_eq!(loaded.len(), 1);
        let restored = &loaded[0];
        assert_eq!(restored.event_id, envelope.event_id);
        assert_eq!(restored.session_id, envelope.session_id);
        assert_eq!(restored.turn_id, envelope.turn_id);
        assert_eq!(restored.sequence, 1);
        assert_eq!(restored.event_type, envelope.event_type);
        assert_eq!(restored.durability, envelope.durability);
        assert_eq!(restored.occurred_at, envelope.occurred_at);
        assert_eq!(restored.actor, envelope.actor);
        assert_eq!(restored.correlation_id, envelope.correlation_id);
        assert_eq!(restored.causation_event_id, envelope.causation_event_id);
        assert_eq!(restored.payload, envelope.payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
