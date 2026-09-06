import { useEffect, useRef, useState } from "react";
import {
  createSession,
  defaultSocket,
  listEvents,
  onSessionEvent,
  onSessionReplay,
  onSessionResync,
  runtimeInfo,
  sendMessage,
  setSocket,
  subscribeSession,
  type EventEnvelope,
} from "./api";
import { foldEvent, foldEvents, initialState, type ConsoleState } from "./events";
import { ChangesPanel } from "./panels/ChangesPanel";
import { ContextPanel } from "./panels/ContextPanel";
import { ToolsPanel } from "./panels/ToolsPanel";

type Tab = "chat" | "context" | "tools" | "changes";

const TAB_ZH: Record<Tab, string> = {
  chat: "对话",
  context: "Context",
  tools: "Tools",
  changes: "Changes",
};

export default function App() {
  const [daemonInfo, setDaemonInfo] = useState("未连接");
  const [socket, setSocketState] = useState(defaultSocket);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [mode, setMode] = useState("ask");
  const [task, setTask] = useState("");
  const [input, setInput] = useState("");
  const [tab, setTab] = useState<Tab>("chat");
  const [busy, setBusy] = useState(false);
  const [state, setState] = useState<ConsoleState>(initialState());
  const subscribedSession = useRef<string | null>(null);
  const chatBottom = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    runtimeInfo()
      .then((info) => setDaemonInfo(`${info.name} v${info.version}`))
      .catch(() => setDaemonInfo("未连接（先启动 daemon）"));
  }, []);

  // 订阅当前会话：补发 durable → 实时推送，统一走折叠器（与重放同路径）
  useEffect(() => {
    if (!sessionId || subscribedSession.current === sessionId) return;
    subscribedSession.current = sessionId;
    setState(initialState());
    const unlistens = [
      onSessionReplay((events) => {
        setState((prev) => foldEvents({ ...prev, messages: [] }, events));
      }),
      onSessionResync((missed) => {
        setState((prev) => ({
          ...prev,
          timeline: [
            ...prev.timeline,
            { sequence: 0, type: `⚠ 落后 ${missed} 条，请重新对齐`, durability: "transient" },
          ],
        }));
      }),
      onSessionEvent(handleEvent),
    ];
    subscribeSession(sessionId, 0).catch((err) => console.error("订阅失败", err));
    listEvents(sessionId)
      .then(({ events }) => setState(foldEvents(initialState(), events)))
      .catch(() => undefined);
    return () => {
      unlistens.forEach((p) => p.then((un) => un()).catch(() => undefined));
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  function handleEvent(ev: EventEnvelope) {
    setState((prev) => foldEvent(prev, ev));
  }

  async function handleCreate() {
    setBusy(true);
    try {
      const info = await createSession(mode, task.trim() || null);
      setSessionId(info.session_id);
      setState(initialState());
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
      await sendMessage(sessionId, text);
    } catch (err) {
      setState((prev) => ({
        ...prev,
        messages: [...prev.messages, { turnId: "err", text: `错误：${err}`, done: true }],
      }));
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    chatBottom.current?.scrollIntoView({ behavior: "smooth" });
  }, [state.messages]);

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
          <div className="tabs">
            {(Object.keys(TAB_ZH) as Tab[]).map((t) => (
              <button
                key={t}
                className={`tab ${tab === t ? "active" : ""}`}
                onClick={() => setTab(t)}
              >
                {TAB_ZH[t]}
                {t === "tools" && state.toolCalls.length > 0 && (
                  <span className="count">{state.toolCalls.length}</span>
                )}
                {t === "context" && state.snapshots.length > 0 && (
                  <span className="count">{state.snapshots.length}</span>
                )}
                {t === "changes" && state.checkpoints.length > 0 && (
                  <span className="count">{state.checkpoints.length}</span>
                )}
              </button>
            ))}
          </div>

          <div className="panel-area">
            {tab === "chat" && (
              <>
                <section className="chat">
                  {state.messages.map((m, i) => (
                    <div key={`${m.turnId}-${i}`} className={`msg ${m.done ? "done" : "streaming"}`}>
                      <pre>{m.text}</pre>
                    </div>
                  ))}
                  <div ref={chatBottom} />
                </section>
                <section className="timeline">
                  <h4>事件时间线</h4>
                  <div className="events">
                    {state.timeline.map((t, i) => (
                      <div key={`${t.sequence}-${i}`} className={`ev ${t.durability}`}>
                        <span className="seq">#{t.sequence}</span> {t.type}
                      </div>
                    ))}
                  </div>
                </section>
              </>
            )}
            {tab === "context" && <ContextPanel state={state} />}
            {tab === "tools" && <ToolsPanel state={state} />}
            {tab === "changes" && <ChangesPanel state={state} sessionId={sessionId} />}
          </div>
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
