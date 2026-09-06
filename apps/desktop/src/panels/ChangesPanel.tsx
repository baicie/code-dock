/**
 * Changes 面板（§14.1）：Patch 视图、Checkpoint 列表与一键回滚（§24）、
 * 冲突记录——变更全链路（change.* / checkpoint.*）可追溯、可恢复。
 */
import type { ConsoleState, PatchView } from "../events";
import { derivePatchViews } from "../events";
import { restoreCheckpoint } from "../api";

function PatchCard({
  patch,
  onJumpToTools,
}: {
  patch: PatchView;
  onJumpToTools?: (toolCallId: string) => void;
}) {
  return (
    <div className={`toolcall patch ${patch.conflicted ? "conflicted" : ""}`}>
      <div className="toolcall-head">
        <strong>file.patch</strong>
        <span className="patch-path">{patch.path}</span>
        {onJumpToTools && (
          <button className="link" onClick={() => onJumpToTools(patch.toolCallId)}>
            工具详情
          </button>
        )}
        {patch.conflicted && <span className="badge status-failed">⚠ 冲突，未应用</span>}
        {patch.status === "completed" && <span className="badge status-completed">已应用</span>}
        {patch.status === "rejected" && <span className="badge status-rejected">已拒绝</span>}
      </div>
      <div className="diff">
        {patch.patches.map((p, i) => (
          <div key={i} className="diff-block">
            <div className="diff-row del">- {p.find}</div>
            <div className="diff-row add">+ {p.replace}</div>
          </div>
        ))}
      </div>
      {(patch.shaBefore || patch.shaAfter) && (
        <div className="kv">
          <span className="k">哈希</span>
          <span className="mono">
            {patch.shaBefore?.slice(0, 12) ?? "—"} → {patch.shaAfter?.slice(0, 12) ?? "—"}
          </span>
        </div>
      )}
    </div>
  );
}

export function ChangesPanel({
  state,
  sessionId,
  onJumpToTools,
}: {
  state: ConsoleState;
  sessionId: string | null;
  onJumpToTools?: (toolCallId: string) => void;
}) {
  const patches = derivePatchViews(state);
  const hasAnything = state.checkpoints.length > 0 || patches.length > 0 || state.conflicts.length > 0;

  async function handleRestore(checkpointId: string) {
    if (!sessionId) return;
    if (!window.confirm(`确认回滚到 ${checkpointId.slice(0, 13)}…？快照后的修改将被覆盖。`)) {
      return;
    }
    try {
      await restoreCheckpoint(sessionId, checkpointId);
      // checkpoint.restored 事件经订阅实时到达，列表状态自动更新
    } catch (err) {
      alert(`回滚失败：${err}`);
    }
  }

  return (
    <div className="panel">
      <h5>Checkpoints（变更前快照，§3.5）</h5>
      {state.checkpoints.length === 0 ? (
        <p className="muted">还没有 Checkpoint（任何写操作执行前自动创建）</p>
      ) : (
        <div className="checkpoints">
          {state.checkpoints.map((c) => (
            <div key={c.checkpointId} className={`checkpoint ${c.restored ? "restored" : ""}`}>
              <div className="toolcall-head">
                <strong className="mono">{c.checkpointId.slice(0, 13)}…</strong>
                <span className="muted">
                  来自 {c.tool} · {c.files.length} 个文件 · #{c.sequence}
                </span>
                {c.restored ? (
                  <span className="badge status-started">已回滚</span>
                ) : (
                  <button className="restore" onClick={() => handleRestore(c.checkpointId)}>
                    回滚到此
                  </button>
                )}
              </div>
              <div className="checkpoint-files">
                {c.files.map((f) => (
                  <span key={f.path} className="mono file" title={`sha256 ${f.sha256.slice(0, 16)}…`}>
                    {f.path}
                  </span>
                ))}
              </div>
              {c.restoredFiles && (
                <div className="kv">
                  <span className="k">已恢复</span>
                  <span>{c.restoredFiles.join("、")}</span>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      <h5>补丁（Patch-first，§13.1）</h5>
      {patches.length === 0 ? (
        <p className="muted">还没有补丁记录</p>
      ) : (
        patches.map((p) => (
          <PatchCard key={p.toolCallId} patch={p} onJumpToTools={onJumpToTools} />
        ))
      )}

      {hasAnything && state.conflicts.length > 0 && (
        <>
          <h5>冲突（§18.2：禁止最后写入者覆盖）</h5>
          <ul className="excluded">
            {state.conflicts.map((c, i) => (
              <li key={i}>
                {c.resource}（#{c.sequence}）—— 补丁基于过期内容生成，已在 Preflight 拦截
              </li>
            ))}
          </ul>
        </>
      )}
      {!hasAnything && (
        <p className="muted panel-empty">
          还没有变更记录（file.patch 执行后这里会出现补丁、Checkpoint 与冲突记录）
        </p>
      )}
    </div>
  );
}
