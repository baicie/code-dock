/**
 * Tools 面板（§14.1）：Tool Proposal、Preflight、Policy 裁决、审批、
 * 输入/输出与实际副作用——工具调用全生命周期可追溯（§8.3.4）。
 */
import { useEffect, useRef } from "react";
import type { ConsoleState, ToolCallView } from "../events";

const STATUS_ZH: Record<string, string> = {
  started: "执行中",
  completed: "已完成",
  failed: "失败",
  rejected: "已拒绝",
};

const DECISION_ZH: Record<string, string> = {
  allow: "允许",
  require_approval: "需要审批",
  deny: "拒绝",
};

function Lifecycle({ call }: { call: ToolCallView }) {
  const steps: { label: string; done: boolean; tone?: string }[] = [
    { label: "提议", done: !!call.tool },
    { label: "预检", done: !!call.plan },
    { label: "裁决", done: !!call.decision, tone: call.decision?.decision },
    {
      label: "审批",
      done: call.approval === "approved" || call.approval === "denied" || call.approval === "required",
      tone: call.approval,
    },
    {
      label: "执行",
      done: !!call.status,
      tone: call.status,
    },
  ];
  return (
    <div className="lifecycle">
      {steps.map((s, i) => (
        <span
          key={i}
          className={[
            "step",
            s.done ? "done" : "",
            s.tone ? `tone-${s.tone}` : "",
          ].join(" ")}
        >
          {s.label}
        </span>
      ))}
    </div>
  );
}

function ToolCall({
  call,
  focused,
  snapshots,
  onJumpToContext,
}: {
  call: ToolCallView;
  focused: boolean;
  snapshots: ConsoleState["snapshots"];
  onJumpToContext?: (snapshotSequence: number) => void;
}) {
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (focused) ref.current?.scrollIntoView({ behavior: "smooth", block: "center" });
  }, [focused]);
  return (
    <div ref={ref} className={`toolcall ${call.status ?? ""} ${focused ? "focused" : ""}`}>
      <div className="toolcall-head">
        <strong>{call.tool ?? "未知工具"}</strong>
        <span className="tcid">{call.toolCallId.slice(0, 8)}…</span>
        {call.status && <span className={`badge status-${call.status}`}>{STATUS_ZH[call.status]}</span>}
      </div>
      <Lifecycle call={call} />

      {call.plan && (
        <div className="kv">
          <span className="k">预检</span>
          <span>
            {call.plan.preview} · 风险 {call.plan.risk} · digest{" "}
            {call.plan.operation_digest.slice(0, 16)}…
          </span>
        </div>
      )}
      {call.plan && call.plan.permissions.length > 0 && (
        <div className="kv">
          <span className="k">权限</span>
          <span>
            {call.plan.permissions.map((p) => `${p.capability}:${p.resource}`).join("；")}
          </span>
        </div>
      )}
      {call.decision && (
        <div className="kv">
          <span className="k">策略</span>
          <span>
            {DECISION_ZH[call.decision.decision] ?? call.decision.decision}（trust{" "}
            {call.decision.trust || "—"}
            {call.decision.context_untrusted ? "，上下文含不可信内容" : ""}）
            {call.decision.reason ? `：${call.decision.reason}` : ""}
          </span>
        </div>
      )}
      {call.approval && (
        <div className="kv">
          <span className="k">审批</span>
          <span>
            {call.approval === "required"
              ? "⏸ 等待用户裁决"
              : call.approval === "approved"
                ? "👍 approve_once"
                : "🚫 deny"}
          </span>
        </div>
      )}
      {call.arguments !== undefined && (
        <div className="kv">
          <span className="k">输入</span>
          <pre className="args">{JSON.stringify(call.arguments, null, 1)}</pre>
        </div>
      )}
      {call.outputText && (
        <div className="kv">
          <span className="k">输出</span>
          <pre className="args">{call.outputText}</pre>
        </div>
      )}
      {call.resultText && (
        <div className="kv">
          <span className="k">结果</span>
          <span>{call.resultText}</span>
        </div>
      )}
      {call.sideEffects && call.sideEffects.length > 0 && (
        <div className="kv">
          <span className="k">实际副作用</span>
          <span>{JSON.stringify(call.sideEffects)}</span>
        </div>
      )}
      {call.error && (
        <div className="kv">
          <span className="k">错误</span>
          <span className="warn">{call.error}</span>
        </div>
      )}
      {call.durationMs !== undefined && (
        <div className="kv">
          <span className="k">耗时</span>
          <span>{call.durationMs}ms</span>
        </div>
      )}
      {onJumpToContext && call.turnId && (
        <div className="trace-links">
          {snapshots
            .filter((s) => s.turnId === call.turnId)
            .map((s) => (
              <button key={s.sequence} className="link" onClick={() => onJumpToContext(s.sequence)}>
                上下文快照 #{s.sequence}
              </button>
            ))}
        </div>
      )}
    </div>
  );
}

export function ToolsPanel({
  state,
  focusToolCallId,
  onJumpToContext,
}: {
  state: ConsoleState;
  focusToolCallId?: string;
  onJumpToContext?: (snapshotSequence: number) => void;
}) {
  if (state.toolCalls.length === 0) {
    return <p className="muted panel-empty">还没有工具调用（模型提出 Tool Proposal 后出现）</p>;
  }
  return (
    <div className="panel">
      {state.toolCalls.map((call) => (
        <ToolCall
          key={call.toolCallId}
          call={call}
          focused={focusToolCallId === call.toolCallId}
          snapshots={state.snapshots}
          onJumpToContext={onJumpToContext}
        />
      ))}
    </div>
  );
}
