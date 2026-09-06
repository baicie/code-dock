/**
 * 事件折叠器：把 Session 事件流折叠成 Console 各面板的视图状态。
 *
 * 纯函数（foldEvent / foldEvents），durable 重放与实时推送走同一折叠路径，
 * 保证"重启后从事件流恢复视图"与"实时观察"渲染一致（§8.2.5 / §14）。
 */
import type { EventEnvelope } from "./api";

// ---------- 视图模型 ----------

export interface ChatMessage {
  turnId: string;
  text: string;
  done: boolean;
}

export interface TimelineEntry {
  sequence: number;
  type: string;
  durability: string;
}

export interface SnapshotItem {
  item_id: string;
  kind: string;
  role: string;
  source: { kind: string; uri: string };
  title: string;
  content: { storage: string; text?: string };
  selection: { reason: string; selected_by: string; score: number; priority: number };
  trust: string;
  classification: string;
  tokens: number;
  transformations: string[];
}

export interface ConsideredItem {
  title: string;
  reason: string;
  score: number;
  selected: boolean;
  excluded_because?: string;
}

export interface SnapshotView {
  sequence: number;
  turnId?: string;
  provider: string;
  model: string;
  items: SnapshotItem[];
  budget: { maxInput: number; reservedOutput: number; used: number };
  finalRequestSha256?: string;
  considered: ConsideredItem[];
}

export interface ToolPlan {
  normalized_arguments?: unknown;
  permissions: { capability: string; resource: string }[];
  risk: string;
  operation_digest: string;
  preview: string;
}

export interface ToolDecision {
  decision: string;
  trust: string;
  context_untrusted: boolean;
  risk: string;
  reason?: string;
}

export interface ToolCallView {
  toolCallId: string;
  tool?: string;
  arguments?: unknown;
  plan?: ToolPlan;
  decision?: ToolDecision;
  approval?: "required" | "approved" | "denied";
  status?: "started" | "completed" | "failed" | "rejected";
  outputText: string;
  resultText?: string;
  durationMs?: number;
  sideEffects?: unknown[];
  error?: string;
}

export interface CheckpointFile {
  path: string;
  sha256: string;
}

export interface CheckpointView {
  checkpointId: string;
  sequence: number;
  occurredAt: string;
  tool: string;
  files: CheckpointFile[];
  restored: boolean;
  restoredFiles?: string[];
}

export interface ChangeConflict {
  toolCallId: string;
  resource: string;
  sequence: number;
}

export interface PatchView {
  toolCallId: string;
  path: string;
  patches: { find: string; replace: string }[];
  shaBefore?: string;
  shaAfter?: string;
  conflicted: boolean;
  status?: string;
}

export interface ConsoleState {
  messages: ChatMessage[];
  timeline: TimelineEntry[];
  snapshots: SnapshotView[];
  toolCalls: ToolCallView[];
  checkpoints: CheckpointView[];
  conflicts: ChangeConflict[];
}

// ---------- 折叠 ----------

export function initialState(): ConsoleState {
  return {
    messages: [],
    timeline: [],
    snapshots: [],
    toolCalls: [],
    checkpoints: [],
    conflicts: [],
  };
}

export function foldEvents(state: ConsoleState, events: EventEnvelope[]): ConsoleState {
  let next = state;
  for (const ev of events) {
    next = foldEvent(next, ev);
  }
  return next;
}

export function foldEvent(state: ConsoleState, ev: EventEnvelope): ConsoleState {
  let next: ConsoleState = {
    ...state,
    timeline: [...state.timeline, describe(ev)],
    toolCalls: state.toolCalls,
  };
  next = foldMessage(next, ev);
  next = foldSnapshot(next, ev);
  next = foldToolCall(next, ev);
  next = foldChanges(next, ev);
  return next;
}

function describe(ev: EventEnvelope): TimelineEntry {
  const p = ev.payload as Record<string, unknown>;
  let type = ev.event_type;
  switch (ev.event_type) {
    case "message.created":
      type = p.text ? `用户：${p.text}` : type;
      break;
    case "message.completed":
      if (p.role === "assistant" && p.text) type = `助手：${p.text}`;
      break;
    case "tool.call.proposed":
      type = `🔧 提议 ${p.tool}`;
      break;
    case "tool.call.preflighted":
      type = `🧪 预检通过（${(p.plan as ToolPlan | undefined)?.preview ?? ""}）`;
      break;
    case "policy.decision_made":
      type = `⚖️ ${p.decision}：${p.tool}${p.reason ? `（${p.reason}）` : ""}`;
      break;
    case "tool.call.approval_required":
      type = `⏸ 等待审批 ${p.tool}`;
      break;
    case "tool.call.approved":
      type = "👍 已批准（approve_once）";
      break;
    case "tool.call.rejected":
      type = `🚫 已拒绝：${p.reason ?? ""}`;
      break;
    case "tool.call.completed":
      type = "✅ 工具完成";
      break;
    case "tool.call.failed":
      type = `❌ 工具失败：${p.error ?? ""}`;
      break;
    case "change.conflicted":
      type = `⚠️ 冲突：${p.resource ?? ""}`;
      break;
    case "checkpoint.created":
      type = `💾 Checkpoint ${(p.files as unknown[] | undefined)?.length ?? 0} 个文件`;
      break;
  }
  return { sequence: ev.sequence, type, durability: ev.durability };
}

function foldMessage(state: ConsoleState, ev: EventEnvelope): ConsoleState {
  const p = ev.payload as Record<string, unknown>;
  if (ev.event_type === "message.delta" && ev.turn_id) {
    const idx = state.messages.findIndex((m) => m.turnId === ev.turn_id && !m.done);
    if (idx >= 0) {
      const messages = [...state.messages];
      messages[idx] = { ...messages[idx], text: messages[idx].text + (p.text ?? "") };
      return { ...state, messages };
    }
    return {
      ...state,
      messages: [...state.messages, { turnId: ev.turn_id, text: String(p.text ?? ""), done: false }],
    };
  }
  if (ev.event_type === "message.completed" && p.role === "assistant") {
    const finalText = String(p.text ?? "");
    const idx = state.messages.findIndex((m) => m.turnId === ev.turn_id && !m.done);
    if (idx >= 0) {
      const messages = [...state.messages];
      messages[idx] = { ...messages[idx], text: finalText, done: true };
      return { ...state, messages };
    }
    if (finalText) {
      return {
        ...state,
        messages: [...state.messages, { turnId: ev.turn_id ?? "", text: finalText, done: true }],
      };
    }
  }
  return state;
}

interface RawSnapshot {
  model: { provider: string; model: string };
  budget: {
    available_input_tokens: number;
    reserved_output_tokens: number;
    used_input_tokens: number;
  };
  items: SnapshotItem[];
  final_request_sha256?: string;
}

interface RawReport {
  max_input_tokens?: number;
  reserved_output_tokens?: number;
  used_input_tokens?: number;
  considered?: ConsideredItem[];
}

function foldSnapshot(state: ConsoleState, ev: EventEnvelope): ConsoleState {
  if (ev.event_type !== "context.snapshot.created") return state;
  const p = ev.payload as Record<string, unknown>;
  const snapshot = p.snapshot as RawSnapshot;
  const report = (p.selection_report ?? {}) as RawReport;
  const view: SnapshotView = {
    sequence: ev.sequence,
    turnId: ev.turn_id ?? undefined,
    provider: snapshot.model.provider,
    model: snapshot.model.model,
    items: snapshot.items ?? [],
    budget: {
      maxInput: report.max_input_tokens ?? snapshot.budget.available_input_tokens ?? 0,
      reservedOutput: report.reserved_output_tokens ?? snapshot.budget.reserved_output_tokens ?? 0,
      used: report.used_input_tokens ?? snapshot.budget.used_input_tokens ?? 0,
    },
    finalRequestSha256: snapshot.final_request_sha256,
    considered: report.considered ?? [],
  };
  return { ...state, snapshots: [...state.snapshots, view] };
}

function upsertToolCall(
  state: ConsoleState,
  toolCallId: string,
  patch: (prev: ToolCallView) => ToolCallView,
): ConsoleState {
  const existing = state.toolCalls.find((t) => t.toolCallId === toolCallId);
  const base: ToolCallView = existing ?? { toolCallId, outputText: "" };
  const next = patch(base);
  if (!existing) {
    return { ...state, toolCalls: [...state.toolCalls, next] };
  }
  return {
    ...state,
    toolCalls: state.toolCalls.map((t) => (t.toolCallId === toolCallId ? next : t)),
  };
}

function foldChanges(state: ConsoleState, ev: EventEnvelope): ConsoleState {
  const p = ev.payload as Record<string, unknown>;
  if (ev.event_type === "checkpoint.created") {
    const view: CheckpointView = {
      checkpointId: String(p.checkpoint_id ?? ""),
      sequence: ev.sequence,
      occurredAt: ev.occurred_at,
      tool: String(p.tool ?? ""),
      files: (p.files as CheckpointFile[] | undefined)?.map((f) => ({
        path: f.path,
        sha256: f.sha256,
      })) ?? [],
      restored: false,
    };
    return { ...state, checkpoints: [...state.checkpoints, view] };
  }
  if (ev.event_type === "checkpoint.restored") {
    const checkpointId = String(p.checkpoint_id ?? "");
    const restoredFiles = (p.files as string[] | undefined) ?? [];
    return {
      ...state,
      checkpoints: state.checkpoints.map((c) =>
        c.checkpointId === checkpointId
          ? { ...c, restored: true, restoredFiles }
          : c,
      ),
    };
  }
  if (ev.event_type === "change.conflicted") {
    return {
      ...state,
      conflicts: [
        ...state.conflicts,
        {
          toolCallId: String(p.tool_call_id ?? ""),
          resource: String(p.resource ?? ""),
          sequence: ev.sequence,
        },
      ],
    };
  }
  return state;
}

/** file.patch 的工具调用 → 补丁伪 diff 视图（含冲突与前后哈希）。 */
export function derivePatchViews(state: ConsoleState): PatchView[] {
  return state.toolCalls
    .filter((t) => t.tool === "file.patch" && t.arguments !== undefined)
    .map((t) => {
      const args = t.arguments as { path?: string; patches?: { find: string; replace: string }[] };
      const modified = (t.sideEffects ?? []).find(
        (s) => (s as { kind?: string }).kind === "file.modified",
      ) as { details?: { sha256_before?: string; sha256_after?: string } } | undefined;
      return {
        toolCallId: t.toolCallId,
        path: args.path ?? "",
        patches: args.patches ?? [],
        shaBefore: modified?.details?.sha256_before,
        shaAfter: modified?.details?.sha256_after,
        conflicted: state.conflicts.some((c) => c.toolCallId === t.toolCallId),
        status: t.status,
      };
    });
}

function foldToolCall(state: ConsoleState, ev: EventEnvelope): ConsoleState {
  const p = ev.payload as Record<string, unknown>;
  const id = p.tool_call_id as string | undefined;
  if (!id) return state;

  switch (ev.event_type) {
    case "tool.call.proposed":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        tool: p.tool as string,
        arguments: p.arguments,
      }));
    case "tool.call.preflighted":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        plan: p.plan as ToolPlan,
      }));
    case "policy.decision_made":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        decision: {
          decision: String(p.decision),
          trust: String(p.trust ?? ""),
          context_untrusted: Boolean(p.context_untrusted),
          risk: String(p.risk ?? ""),
          reason: p.reason as string | undefined,
        },
      }));
    case "tool.call.approval_required":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        approval: "required",
      }));
    case "tool.call.approved":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        approval: "approved",
      }));
    case "tool.call.rejected":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        approval: p.response === "deny" ? "denied" : prev?.approval,
        status: "rejected",
        error: p.reason as string | undefined,
      }));
    case "tool.call.started":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        tool: (p.tool as string) ?? prev?.tool,
        status: "started",
      }));
    case "tool.call.output":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        outputText: (prev?.outputText ?? "") + String(p.text ?? ""),
      }));
    case "tool.call.completed":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        status: "completed",
        resultText: p.text as string | undefined,
        durationMs: p.duration_ms as number | undefined,
        sideEffects: p.actual_side_effects as unknown[] | undefined,
      }));
    case "tool.call.failed":
      return upsertToolCall(state, id, (prev) => ({
        ...prev,
        toolCallId: id,
        status: "failed",
        error: p.error as string | undefined,
      }));
    default:
      return state;
  }
}
