// 聊天记录窗口（?chat=1 模式渲染）：QQ 风格完整聊天记录
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface Msg {
  role: string;
  content: string;
  ts?: string;
}

export default function ChatLogView() {
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const bottomRef = useRef<HTMLDivElement>(null);

  // 每 1.5 秒轮询聊天记录（简单可靠，跨窗口同步）
  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const list = await invoke<Msg[]>("get_chat_log");
        if (alive) setMsgs(list);
      } catch {
        /* 忽略，下轮重试 */
      }
    };
    load();
    const t = setInterval(load, 1500);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  // 新消息自动滚到底部
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [msgs]);

  const clear = async () => {
    try {
      await invoke("clear_chat_log");
      setMsgs([]);
    } catch {
      /* ignore */
    }
  };

  return (
    <div className="chat-log">
      <div className="chat-log-header">
        <span>💬 聊天记录 · 优香</span>
        <button onClick={clear} className="chat-clear" title="清空聊天记录">
          🗑️ 清空
        </button>
      </div>
      <div className="chat-log-body">
        {msgs.length === 0 && <div className="chat-empty">还没有聊天记录，去和优香聊聊吧～</div>}
        {msgs.map((m, i) => (
          <div key={i} className={`chat-row ${m.role === "user" ? "me" : "yuka"}`}>
            <div className="chat-bubble">
              <div className="chat-content">{m.content}</div>
              {m.ts && <div className="chat-ts">{m.ts}</div>}
            </div>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>
    </div>
  );
}
