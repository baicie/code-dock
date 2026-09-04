//! Session Event Protocol 的 Event Envelope（§8.2.2 / §8.2.3）。

use crate::ids::{EventId, SessionId, TurnId};
use crate::time;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 事件持久化级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// 确认写入后不可修改，断线补发时保证送达。
    Durable,
    /// 高频流式事件（如 `message.delta`），不保证补发。
    Transient,
}

/// 事件参与者类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Agent,
    System,
    Tool,
    Plugin,
}

/// 事件参与者。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    #[serde(rename = "type")]
    pub kind: ActorKind,
    pub id: String,
}

impl Actor {
    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: ActorKind::Agent,
            id: id.into(),
        }
    }

    pub fn user(id: impl Into<String>) -> Self {
        Self {
            kind: ActorKind::User,
            id: id.into(),
        }
    }

    pub fn system(id: impl Into<String>) -> Self {
        Self {
            kind: ActorKind::System,
            id: id.into(),
        }
    }
}

/// 统一事件信封。
///
/// 字段规则（§8.2.3）：
/// - `sequence` 由 Runtime 分配，在单个 Session 内严格单调递增；
/// - Durable Event 一旦确认写入后不可修改，只能追加纠正事件；
/// - `payload` 保持为原始 JSON，未知事件类型必须保留原始数据并降级展示（§8.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: String,
    pub event_id: EventId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    pub event_type: String,
    pub durability: Durability,
    pub occurred_at: DateTime<Utc>,
    pub actor: Actor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_event_id: Option<EventId>,
    #[serde(default)]
    pub payload: Value,
}

impl EventEnvelope {
    /// 创建一个待分配 `sequence` 的事件草稿（sequence 由 Event Store 在追加时统一分配）。
    pub fn draft(
        session_id: SessionId,
        turn_id: Option<TurnId>,
        event_type: impl Into<String>,
        durability: Durability,
        actor: Actor,
        payload: Value,
    ) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION.to_string(),
            event_id: EventId::generate(),
            session_id,
            turn_id,
            sequence: 0,
            event_type: event_type.into(),
            durability,
            occurred_at: Utc::now(),
            actor,
            correlation_id: None,
            causation_event_id: None,
            payload,
        }
    }
}

impl std::fmt::Display for EventEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "#{} {} [{}] {}",
            self.sequence,
            self.event_type,
            self.durability_str(),
            time::format_rfc3339(self.occurred_at)
        )
    }
}

impl EventEnvelope {
    fn durability_str(&self) -> &'static str {
        match self.durability {
            Durability::Durable => "durable",
            Durability::Transient => "transient",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_roundtrip_matches_protocol_shape() {
        let session = SessionId::generate();
        let envelope = EventEnvelope::draft(
            session,
            None,
            "session.created",
            Durability::Durable,
            Actor::system("runtime"),
            json!({}),
        );
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], crate::SCHEMA_VERSION);
        assert_eq!(json["event_type"], "session.created");
        assert_eq!(json["durability"], "durable");
        assert_eq!(json["actor"]["type"], "system");

        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back.event_id, envelope.event_id);
    }
}
