/**
 * Context 面板（§14.1）：每轮真实上下文、Token 预算、来源与选择原因、
 * 变换记录（脱敏等）、排除项、最终请求哈希（§8.4.1 / §8.4.9 可解释性）。
 */
import { useEffect, useState } from "react";
import type { ConsoleState, SnapshotView, ToolCallView } from "../events";

const REASON_ZH: Record<string, string> = {
  user_attached: "用户附加",
  user_pinned: "用户置顶",
  project_rule: "项目规则",
  currently_open: "当前打开",
  currently_edited: "当前编辑",
  keyword_match: "关键词命中",
  semantic_match: "语义命中",
  symbol_definition: "符号定义",
  symbol_reference: "符号引用",
  symbol_dependency: "符号依赖",
  git_modified: "Git 变更",
  git_related: "Git 关联",
  diagnostic_related: "诊断关联",
  tool_result: "工具结果",
  session_memory: "会话记忆",
  agent_selected: "Agent 选择",
  plugin_selected: "插件选择",
};

const TRUST_ZH: Record<string, string> = {
  trusted: "可信",
  workspace_untrusted: "工作区不可信",
  plugin_untrusted: "插件不可信",
  external_untrusted: "外部不可信",
};

function BudgetBar({ view }: { view: SnapshotView }) {
  const pct = view.budget.maxInput > 0 ? Math.min(100, (view.budget.used / view.budget.maxInput) * 100) : 0;
  return (
    <div className="budget">
      <div className="budget-bar">
        <div className="budget-used" style={{ width: `${pct}%` }} />
      </div>
      <span className="budget-text">
        输入 {view.budget.used} / {view.budget.maxInput} tokens（预留输出{" "}
        {view.budget.reservedOutput}）
      </span>
    </div>
  );
}

function TurnToolLinks({
  tools,
  onJumpToTools,
}: {
  tools: ToolCallView[];
  onJumpToTools?: (toolCallId: string) => void;
}) {
  if (tools.length === 0 || !onJumpToTools) return null;
  return (
    <div className="trace-links">
      {tools.map((t) => (
        <button key={t.toolCallId} className="link" onClick={() => onJumpToTools(t.toolCallId)}>
          本 Turn 工具：{t.tool ?? t.toolCallId.slice(0, 8)}
        </button>
      ))}
    </div>
  );
}

function SnapshotDetail({
  view,
  tools,
  onJumpToTools,
}: {
  view: SnapshotView;
  tools: ToolCallView[];
  onJumpToTools?: (toolCallId: string) => void;
}) {
  const excluded = view.considered.filter((c) => !c.selected);
  return (
    <div className="snapshot">
      <div className="snapshot-meta">
        #{view.sequence} · {view.provider}/{view.model}
        {view.turnId ? ` · turn ${view.turnId.slice(0, 8)}…` : ""}
        {view.finalRequestSha256 && (
          <span className="hash" title={view.finalRequestSha256}>
            {" "}
            · 载荷 sha256 {view.finalRequestSha256.slice(7, 19)}…
          </span>
        )}
      </div>
      <BudgetBar view={view} />
      <TurnToolLinks tools={tools} onJumpToTools={onJumpToTools} />

      <table className="items">
        <thead>
          <tr>
            <th>条目</th>
            <th>角色</th>
            <th>选择原因</th>
            <th>信任</th>
            <th>tokens</th>
            <th>变换</th>
          </tr>
        </thead>
        <tbody>
          {view.items.map((item) => (
            <tr key={item.item_id}>
              <td title={item.source.uri}>{item.title}</td>
              <td>{item.role}</td>
              <td>{REASON_ZH[item.selection.reason] ?? item.selection.reason}</td>
              <td className={item.trust !== "trusted" ? "warn" : ""}>
                {TRUST_ZH[item.trust] ?? item.trust}
              </td>
              <td>{item.tokens}</td>
              <td>{item.transformations.length > 0 ? item.transformations.join(", ") : "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <h5>
        排除项（{excluded.length}）——为什么没有进入上下文
      </h5>
      {excluded.length === 0 ? (
        <p className="muted">无排除项</p>
      ) : (
        <ul className="excluded">
          {excluded.map((c, i) => (
            <li key={i}>
              <strong>{c.title}</strong>（{REASON_ZH[c.reason] ?? c.reason}）——{" "}
              {c.excluded_because === "token_budget" ? "超出 Token 预算" : c.excluded_because}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function ContextPanel({
  state,
  focusSnapshotSequence,
  onJumpToTools,
}: {
  state: ConsoleState;
  focusSnapshotSequence?: number;
  onJumpToTools?: (toolCallId: string) => void;
}) {
  const [selected, setSelected] = useState(0);
  // 跨面板跳转：外部指定快照 sequence 时自动选中
  useEffect(() => {
    if (focusSnapshotSequence === undefined) return;
    const idx = state.snapshots.findIndex((s) => s.sequence === focusSnapshotSequence);
    if (idx >= 0) setSelected(idx);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusSnapshotSequence]);

  if (state.snapshots.length === 0) {
    return <p className="muted panel-empty">还没有上下文快照（发起一轮对话后出现）</p>;
  }
  const idx = Math.min(selected, state.snapshots.length - 1);
  const view = state.snapshots[idx];
  const turnTools = state.toolCalls.filter((t) => t.turnId === view.turnId);
  return (
    <div className="panel">
      <div className="snapshot-picker">
        <select value={idx} onChange={(e) => setSelected(Number(e.target.value))}>
          {state.snapshots.map((s, i) => (
            <option key={s.sequence} value={i}>
              #{s.sequence} · {s.provider}/{s.model} · {s.items.length} 条
            </option>
          ))}
        </select>
        <span className="muted">
          第 {idx + 1} / {state.snapshots.length} 次模型调用
        </span>
      </div>
      <SnapshotDetail view={view} tools={turnTools} onJumpToTools={onJumpToTools} />
    </div>
  );
}
