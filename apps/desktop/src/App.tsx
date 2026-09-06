import { useEffect, useRef, useState } from "react";
import {
  createSession,
  defaultSocket,
  setSocket,
  listEvents,
  onSessionEvent,
  onSessionReplay,
  onSessionResync,
  runtimeInfo,
  sendMessage,
  subscribeSession,
  type EventEnvelope,
  type UnlistenFn,
} from "./api";

/** 助手消息按 turn 聚合：delta 追加、completed 定稿。 */
interface ChatMessage {
  turnId: string;
  text: string;
  done: boolean;
}

interface TimelineEntry {
  sequence: number;
  type: string;
  durability: string;
}

/** 从事件载荷提取用户/助手可见文本。 */
function describeEvent(ev: EventEnvelope): string | null {
  const p = ev.payload as Record<string, unknown>;
  switch (ev.event_type) {
    case "message.created":
      return p.text ? `用户：${p.text}` : null;
    case "message.completed":
      return p.role === "assistant" && p.text ? `助手：${p.text}` : null;
    case "tool.call.proposed":
      return `🔧 提议工具 ${p.tool}(${JSON.stringify(p.arguments ?? {})})`;
    case "policy.decision_made":
      return `⚖️ 策略 ${p.decision}：${p.tool}（risk ${p.risk}${p.reason ? `，${p.reason}` : ""}）`;
    case "tool.call.approval_required":
      return `⏸ 等待审批 ${p.tool_call_id}`;
    case "tool.call.completed":
      return `✅ 工具完成（${p.duration_ms ?? "?"}ms）`;
    case "tool.call.failed":
      return `❌ 工具失败：${p.error}`;
    case "change.conflicted":
      return `⚠️ 冲突：${p.resource}`;
    case "checkpoint.created":
      return `💾 Checkpoint ${p.checkpoint_id}`;
    case "session.created":
      return "会话已创建";
    default:
      return null;
  }
}

export default function App() {
  const [daemonInfo, setDaemonInfo] = useState("未连接");
  const [socket, setSocketState] = useState(defaultSocket);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [mode, setMode] = useState("ask");
  const [task, setTask] = useState("");
  const [input, setInput] = useState("");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [timeline, setTimeline] = useState<TimelineEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const subscribedSession = useRef<string | null>(null);
  const chatBottom = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    runtimeInfo()
      .then((info) => setDaemonInfo(`${info.name} v${info.version}`))
      .catch(() => setDaemonInfo("未连接（先启动 daemon）"));
  }, []);

  // 订阅当前会话的实时事件
  useEffect(() => {
    if (!sessionId || subscribedSession.current === sessionId) return;
    subscribedSession.current = sessionId;
    const unlistens: Promise<UnlistenFn>[] = [
      onSessionReplay((events) => {
        setTimeline(events.map(toTimelineEntry));
      }),
      onSessionResync((missed) => {
        setTimeline((prev) => [
          ...prev,
          { sequence: 0, type: `⚠ 落后 ${missed} 条，请重新对齐`, durability: "transient" },
        ]);
      }),
      onSessionEvent(handleEvent),
    ];
    subscribeSession(sessionId, 0).catch((err) => console.error("订阅失败", err));
    listEvents(sessionId)
      .then(({ events }) => setTimeline(events.map(toTimelineEntry)))
      .catch(() => undefined);
    return () => {
      unlistens.forEach((p) => p.then((un: UnlistenFn) => un()).catch(() => undefined));
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // 流式 delta 追加到对应 turn 的助手消息
  function handleEvent(ev: EventEnvelope) {
    setTimeline((prev) => [...prev, toTimelineEntry(ev)]);
    const p = ev.payload as Record<string, unknown>;
    if (ev.event_type === "message.delta" && ev.turn_id) {
      setMessages((prev) => {
        const idx = prev.findIndex((m) => m.turnId === ev.turn_id && !m.done);
        if (idx >= 0) {
          const next = [...prev];
          next[idx] = { ...next[idx], text: next[idx].text + (p.text ?? "") };
          return next;
        }
        return [...prev, { turnId: ev.turn_id!, text: String(p.text ?? ""), done: false }];
      });
    } else if (ev.event_type === "message.completed" && p.role === "assistant") {
      setMessages((prev) => {
        const idx = prev.findIndex((m) => m.turnId === ev.turn_id && !m.done);
        const finalText = String(p.text ?? "");
        if (idx >= 0) {
          const next = [...prev];
          next[idx] = { ...next[idx], text: finalText, done: true };
          return next;
        }
        return prev.filter((m) => m.turnId !== ev.turn_id).concat({
          turnId: ev.turn_id ?? "",
          text: finalText,
          done: true,
        });
      });
    }
  }

  function toTimelineEntry(ev: EventEnvelope): TimelineEntry {
    const described = describeEvent(ev);
    return {
      sequence: ev.sequence,
      type: described ?? `${ev.event_type}${ev.durability === "transient" ? " ·t" : ""}`,
      durability: ev.durability,
    };
  }

  async function handleCreate() {
    setBusy(true);
    try {
      const info = await createSession(mode, task.trim() || null);
      setSessionId(info.session_id);
      setMessages([]);
      setTimeline([]);
      subscribedSession.current = null;
    } catch (err) {
      alert(`创建失败：${err}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleSend() {
    if (!sessionId || !input.trim()) return;
    const text = input;
    setInput("");
    setBusy(true);
    try {
      const outcome = await sendMessage(sessionId, text);
      if (outcome.status === "waiting_approval" && outcome.pending_tool_call_id) {
        setMessages((prev) => [
          ...prev,
          {
            turnId: outcome.turn_id,
            text: `⏸ 等待审批（tool_call_id=${outcome.pending_tool_call_id}）`,
            done: true,
          },
        ]);
      }
    } catch (err) {
      setMessages((prev) => [...prev, { turnId: "err", text: `错误：${err}`, done: true }]);
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    chatBottom.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  return (
    <div className="app">
      <header className="topbar">
        <strong>CodeDock Console</strong>
        <span className="status">{daemonInfo}</span>
        <input
          className="socket"
          value={socket}
          onChange={(e) => setSocketState(e.target.value)}
          spellCheck={false}
        />
        <button
          onClick={() => {
            setSocket(socket)
              .then(() => runtimeInfo())
              .then((info) => setDaemonInfo(`${info.name} v${info.version}`))
              .catch(() => setDaemonInfo("未连接"));
          }}
        >
          连接
        </button>
      </header>

      <div className="body">
        <aside className="sidebar">
          <h3>新会话</h3>
          <select value={mode} onChange={(e) => setMode(e.target.value)}>
            <option value="ask">ask</option>
            <option value="plan">plan</option>
            <option value="edit">edit</option>
            <option value="auto">auto</option>
          </select>
          <textarea
            placeholder="任务描述（可选）"
            value={task}
            onChange={(e) => setTask(e.target.value)}
          />
          <button disabled={busy} onClick={handleCreate}>
            创建会话
          </button>
          {sessionId && <p className="sid">当前：{sessionId.slice(0, 13)}…</p>}
        </aside>

        <main className="main">
          <section className="chat">
            {messages.map((m) => (
              <div key={m.turnId + m.text.length} className={`msg ${m.done ? "done" : "streaming"}`}>
                <pre>{m.text}</pre>
              </div>
            ))}
            <div ref={chatBottom} />
          </section>

          <section className="timeline">
            <h4>事件时间线</h4>
            <div className="events">
              {timeline.map((t, i) => (
                <div key={`${t.sequence}-${i}`} className={`ev ${t.durability}`}>
                  <span className="seq">#{t.sequence}</span> {t.type}
                </div>
              ))}
            </div>
          </section>
        </main>
      </div>

      <footer className="composer">
        <input
          placeholder={sessionId ? "输入消息（Enter 发送）" : "先创建会话"}
          disabled={!sessionId || busy}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleSend()}
        />
        <button disabled={!sessionId || busy} onClick={handleSend}>
          发送
        </button>
      </footer>
    </div>
  );
}
