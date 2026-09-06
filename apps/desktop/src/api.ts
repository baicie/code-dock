/**
 * Daemon API 桥（前端 ↔ Tauri commands）。
 * 所有 Runtime 交互经此层（§5：Desktop UI 不直接碰文件系统与 Runtime）。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type { UnlistenFn };

export interface EventEnvelope {
  sequence: number;
  event_type: string;
  durability: "durable" | "transient";
  occurred_at: string;
  turn_id?: string | null;
  payload: Record<string, unknown>;
}

export const defaultSocket = "/tmp/codedock.sock";

export function setSocket(socket: string): Promise<void> {
  return invoke("set_socket", { socket });
}

export function runtimeInfo(): Promise<{ name: string; version: string }> {
  return invoke("runtime_info");
}

export function createSession(
  mode: string,
  task: string | null,
): Promise<{ session_id: string; status: string }> {
  return invoke("create_session", { mode, task });
}

export function sendMessage(
  sessionId: string,
  text: string,
  taskType?: string,
): Promise<{
  turn_id: string;
  status: string;
  text: string;
  pending_tool_call_id?: string;
}> {
  return invoke("send_message", { sessionId, text, taskType: taskType ?? null });
}

export function listEvents(sessionId: string): Promise<{ events: EventEnvelope[] }> {
  return invoke("list_events", { sessionId });
}

export function restoreCheckpoint(
  sessionId: string,
  checkpointId: string,
): Promise<{ restored: boolean; files: string[] }> {
  return invoke("restore_checkpoint", { sessionId, checkpointId });
}

export function subscribeSession(sessionId: string, afterSequence = 0): Promise<void> {
  return invoke("subscribe_session", { sessionId, afterSequence });
}

export function onSessionEvent(handler: (envelope: EventEnvelope) => void): Promise<UnlistenFn> {
  return listen<EventEnvelope>("session-event", (e) => handler(e.payload));
}

export function onSessionReplay(handler: (events: EventEnvelope[]) => void): Promise<UnlistenFn> {
  return listen<EventEnvelope[]>("session-replay", (e) => handler(e.payload));
}

export function onSessionResync(handler: (missed: number) => void): Promise<UnlistenFn> {
  return listen<number>("session-resync", (e) => handler(e.payload));
}
