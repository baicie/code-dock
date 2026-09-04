//! Context Engine：候选上下文、排序、预算、变换、快照（§6 / §12）。
//!
//! 检索顺序（§12.1）：
//! 1. 用户显式 Pin 和当前编辑文件
//! 2. 项目规则和任务直接关联文件
//! 3. ripgrep 关键词搜索
//! 4. Tree-sitter 符号定义、引用和依赖
//! 5. Git Diff 和最近修改关联
//! 6. Diagnostics 和测试失败关联
//! 7. 可选语义检索（v1.0 不依赖向量数据库）
//!
//! TODO(阶段3)：Selection Report、Token Budget 分配、变换管线（redact 等）。

use async_trait::async_trait;
use codedock_protocol::{
    Classification, ContextBudget, ContextItem, ContextSnapshot, ModelRef, SelectionReason, Trust,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("token 预算不足以容纳任何上下文")]
    BudgetExhausted,
    #[error("secret 分类内容禁止进入模型上下文（§8.4.6）")]
    SecretBlocked,
}

/// 候选上下文条目（进入 Snapshot 前的形态）。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub item: ContextItem,
    pub score: f32,
}

/// 候选选择报告（§8.4.9）：记录为什么选中、为什么排除。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SelectionReport {
    pub considered: Vec<ConsideredItem>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConsideredItem {
    pub title: String,
    pub reason: SelectionReason,
    pub score: f32,
    pub selected: bool,
    pub excluded_because: Option<String>,
}

/// 上下文检索器抽象（对应 §12.1 的各检索来源）。
#[async_trait]
pub trait Retriever: Send + Sync {
    fn name(&self) -> &str;

    /// 按当前任务检索候选条目。
    async fn retrieve(&self, query: &RetrievalQuery) -> Result<Vec<Candidate>, ContextError>;
}

/// 检索查询。
#[derive(Debug, Clone, Default)]
pub struct RetrievalQuery {
    pub user_task: String,
    pub pinned_paths: Vec<String>,
    pub open_files: Vec<String>,
    pub keywords: Vec<String>,
}

/// 组装快照：按优先级 + 预算装箱候选，产出不可变 Context Snapshot。
pub struct SnapshotBuilder {
    model: ModelRef,
    budget: ContextBudget,
}

impl SnapshotBuilder {
    pub fn new(model: ModelRef, budget: ContextBudget) -> Self {
        Self { model, budget }
    }

    /// 将候选装入快照：贪心按 score 降序，超出预算即排除并记录到报告（§8.4.9）。
    ///
    /// `secret` 分类内容永远不进入上下文（§8.4.6）。
    pub fn build(
        &self,
        session_id: codedock_protocol::SessionId,
        mut candidates: Vec<Candidate>,
    ) -> Result<(ContextSnapshot, SelectionReport), ContextError> {
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut snapshot = ContextSnapshot::new(session_id, self.model.clone(), self.budget);
        let mut report = SelectionReport::default();
        let mut used = 0u64;

        for cand in candidates {
            if cand.item.classification == Classification::Secret {
                report.considered.push(ConsideredItem {
                    title: cand.item.title,
                    reason: cand.item.selection.reason,
                    score: cand.score,
                    selected: false,
                    excluded_because: Some("classification=secret".into()),
                });
                continue;
            }
            if used + cand.item.tokens <= self.budget.available_input_tokens {
                used += cand.item.tokens;
                report.considered.push(ConsideredItem {
                    title: cand.item.title.clone(),
                    reason: cand.item.selection.reason,
                    score: cand.score,
                    selected: true,
                    excluded_because: None,
                });
                snapshot.items.push(cand.item);
            } else {
                report.considered.push(ConsideredItem {
                    title: cand.item.title,
                    reason: cand.item.selection.reason,
                    score: cand.score,
                    selected: false,
                    excluded_because: Some("token_budget".into()),
                });
            }
        }

        snapshot.budget.used_input_tokens = used;
        Ok((snapshot, report))
    }
}

/// 演示/测试用：固定信任级别的辅助。
pub fn trust_for(kind: &codedock_protocol::SourceKind) -> Trust {
    use codedock_protocol::SourceKind::*;
    match kind {
        File | Git => Trust::WorkspaceUntrusted,
        Web | ToolOutput => Trust::ExternalUntrusted,
        Message => Trust::Trusted,
        Plugin => Trust::PluginUntrusted,
        Other(_) => Trust::ExternalUntrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codedock_protocol::ids::EventId;
    use codedock_protocol::{
        Classification, ContextItem, ContextItemContent, Role, Selection, SourceKind, SourceRef,
    };

    fn item(title: &str, tokens: u64, score: f32, classification: Classification) -> Candidate {
        Candidate {
            item: ContextItem {
                item_id: EventId::generate(),
                kind: "code".into(),
                role: Role::Data,
                source: SourceRef {
                    kind: SourceKind::File,
                    uri: format!("workspace://{title}"),
                    revision: None,
                },
                title: title.into(),
                content: ContextItemContent::Inline {
                    text: "fn main() {}".into(),
                },
                range: None,
                selection: Selection {
                    reason: SelectionReason::KeywordMatch,
                    selected_by: "test".into(),
                    score,
                    priority: 0,
                },
                trust: Trust::WorkspaceUntrusted,
                classification,
                tokens,
                transformations: vec![],
            },
            score,
        }
    }

    #[test]
    fn budget_respected_and_secret_blocked() {
        let candidates = vec![
            item("a.rs", 100, 0.9, Classification::Internal),
            item("secret.env", 10, 0.99, Classification::Secret),
            item("b.rs", 100, 0.8, Classification::Internal),
        ];
        // 限制预算只够装一个
        let builder = SnapshotBuilder::new(
            ModelRef {
                provider: "local".into(),
                model: "small".into(),
                context_window: 200,
            },
            ContextBudget::new(200, 100),
        );
        let (snapshot, report) = builder
            .build(codedock_protocol::SessionId::generate(), candidates)
            .unwrap();
        assert_eq!(snapshot.items.len(), 1, "预算只够一个");
        assert_eq!(snapshot.items[0].title, "a.rs", "按 score 排序选中");
        let secret = report
            .considered
            .iter()
            .find(|c| c.title == "secret.env")
            .unwrap();
        assert!(!secret.selected);
        assert_eq!(
            secret.excluded_because.as_deref(),
            Some("classification=secret")
        );
    }
}
