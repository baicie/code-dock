/**
 * Trace 面板（§14.1）：从用户指令到模型、工具和变更的完整因果链，
 * 按 turn 分组；跨面板跳转入口（§14.2 关键交互）。
 */
import type { ConsoleState } from "../events";
import { deriveTrace } from "../events";

const STATUS_ZH: Record<string, string> = {
  completed: "已完成",
  failed: "失败",
  waiting_approval: "等待审批",
  open: "进行中",
};

export interface JumpTarget {
  tab: "chat" | "context" | "tools" | "changes" | "trace";
  toolCallId?: string;
  snapshotSequence?: number;
}

export function TracePanel({
  state,
  onJump,
}: {
  state: ConsoleState;
  onJump: (target: JumpTarget) => void;
}) {
  const turns = deriveTrace(state);
  if (turns.length === 0) {
    return <p className="muted panel-empty">还没有 Turn（发起对话后这里出现完整因果链）</p>;
  }
  const toolName = (id: string) => state.toolCalls.find((t) => t.toolCallId === id)?.tool ?? id.slice(0, 8);

  return (
    <div className="panel">
      {turns.map((turn) => (
        <div key={turn.turnId} className={`trace-turn status-${turn.status}`}>
          <div className="trace-head">
            <strong>{turn.userText ?? "(无用户消息)"}</strong>
            <span className={`badge status-${turn.status === "open" ? "started" : turn.status}`}>
              {STATUS_ZH[turn.status]}
            </span>
            <span className="muted mono">turn {turn.turnId.slice(0, 8)}…</span>
          </div>
          <div className="trace-entries">
            {turn.entries.map((e, i) => (
              <div key={i} className={`ev ${e.durability}`}>
                <span className="seq">#{e.sequence}</span> {e.type}
              </div>
            ))}
          </div>
          <div className="trace-links">
            {turn.snapshotSequences.length > 0 && (
              <button
                className="link"
                onClick={() =>
                  onJump({
                    tab: "context",
                    snapshotSequence:
                      turn.snapshotSequences[turn.snapshotSequences.length - 1],
                  })
                }
              >
                Context 快照（{turn.snapshotSequences.length}）
              </button>
            )}
            {turn.toolCallIds.map((id) => (
              <button key={id} className="link" onClick={() => onJump({ tab: "tools", toolCallId: id })}>
                工具：{toolName(id)}
              </button>
            ))}
            {state.checkpoints.some((c) => c.sequence > turn.startSequence) && (
              <button
                className="link"
                onClick={() => onJump({ tab: "changes" })}
              >
                Changes
              </button>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
