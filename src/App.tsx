import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import * as THREE from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { VRMLoaderPlugin, VRMLookAtBoneApplier, VRMLookAtRangeMap } from "@pixiv/three-vrm";
import { VRMLipSync, VISEME_NAMES } from "three-vrm-lip-sync";
import ChatLogView from "./ChatLogView";
import "./App.css";

// ===== 模块级常量（不随组件重渲染重建）=====
const EMOTION_PRESETS = ["happy", "angry", "sad", "relaxed", "surprised", "neutral"];
// 这些表情自带眼部变形（眯眼/瞪眼/皱眉/放松眯眼），激活时暂停自动眨眼避免叠加扭曲
// relaxed=なごみ(放松眯眼)、happy=笑い(眯眼)、sad=困る、angry=怒り、surprised=びっくり
const EYE_LOCK_EMOTIONS = ["happy", "angry", "sad", "surprised", "relaxed"];
// 启发式情绪检测（DeepSeek-chat 中文情绪分析弱，关键词打底又快又准）
// 注意：只用语义明确的词，避免子串误伤（"麻烦"含"烦"、"滚动"含"滚"都会误判 angry）
const EMOTION_KEYWORDS: Record<string, string[]> = {
  sad: ["难过", "伤心", "哭", "委屈", "郁闷", "不开心", "失落", "沮丧", "悲伤", "难受", "痛心", "emo"],
  happy: ["开心", "高兴", "快乐", "哈哈", "嘿嘿", "太好了", "兴奋", "愉快", "幸福", "喜欢", "棒", "耶"],
  surprised: ["吓", "惊", "意外", "没想到", "哇", "惊讶", "震惊", "居然"],
  angry: ["生气", "愤怒", "气死", "讨厌", "烦死", "烦人", "心烦", "恼火", "火大", "可恶", "滚开", "滚蛋"],
  relaxed: ["轻松", "舒服", "放松", "惬意", "悠闲", "自在", "平静"],
};

function App() {
  // 聊天记录窗口模式（新窗口 URL 带 ?chat=1 时只渲染聊天记录，不加载 3D 场景）
  const isChatLog =
    typeof window !== "undefined" &&
    new URLSearchParams(window.location.search).has("chat");
  if (isChatLog) {
    return <ChatLogView />;
  }

  // ===== 聊天记录：追加一条（fire-and-forget，主窗口写入，聊天记录窗口读取）=====
  function logChat(role: string, content: string) {
    invoke("append_chat_log", { role, content }).catch(() => {});
  }

  // ===== 打开聊天记录窗口（类似 QQ 的独立窗口）=====
  async function openChatLog() {
    try {
      const existing = await WebviewWindow.getByLabel("chat-log");
      if (existing) {
        existing.show();
        existing.setFocus();
        return;
      }
      new WebviewWindow("chat-log", {
        url: "/?chat=1",
        title: "聊天记录 · 优香",
        width: 420,
        height: 640,
        decorations: true,
        resizable: true,
      });
    } catch (e) {
      setStatus("❌ 打开聊天记录失败: " + e);
    }
  }
  const mountRef = useRef<HTMLDivElement>(null);
  const lipSyncRef = useRef<any>(null);
  const vrmRef = useRef<any>(null);
  const animMixerRef = useRef<any>(null); // Mixamo 动画 mixer（驱动 normalized 骨骼）
  const animInfoRef = useRef<string>(""); // 动画调试信息
  const a2fRef = useRef<any>(null); // Audio2Face blendshape 时间轴 {frames, fps, names, startPerf}
  const speakingRef = useRef(false); // 是否正在说话（驱动手势/头部动作）
  const fallbackAudioRef = useRef<HTMLAudioElement | null>(null); // 兜底播放的 Audio 元素（用于打断时停止）
  const emotionRef = useRef<string | null>(null); // 当前情绪（眯眼类表情时暂停眨眼，避免恐怖谷）
  const emotionTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null); // "恢复平静"定时器
  const [text, setText] = useState("");
  const [status, setStatus] = useState("");
  const [debugInfo, setDebugInfo] = useState("");
  const [debugOpen, setDebugOpen] = useState(false);
  const [activityInfo, setActivityInfo] = useState(""); // 活动感知调试行（5s 刷新）
  // ===== 模式切换：聊天模式(默认) / 任务模式 =====
  // 用户拍板：平常只聊天，点 💼 按钮切到任务模式才执行 DSH/CUA（防止意图误判）
  const [mode, setMode] = useState<"chat" | "task">("chat");
  const modeRef = useRef<"chat" | "task">("chat");
  function toggleMode() {
    const next = mode === "chat" ? "task" : "chat";
    setMode(next);
    modeRef.current = next;
    // ===== 模式切换历史边界（修复：切回聊天后 LLM 被任务历史带偏，把闲聊误判成任务）=====
    // 往短期记忆插一条 system 消息，告诉 LLM 模式已切换、之前的任务是过去式
    if (next === "chat") {
      chatHistoryRef.current.push({
        role: "system",
        content:
          "【模式切换】已从任务模式切回聊天模式。之前的文件/电脑操作任务已经结束，用户接下来说的都是闲聊话题，不是任务指令。请正常聊天，不要执行任何文件/界面操作。",
      });
    } else {
      chatHistoryRef.current.push({
        role: "system",
        content: "【模式切换】已切到任务模式。用户接下来说的话是要执行的任务（文件/命令→DSH，界面操作→电脑操控）。请判断任务意图。",
      });
    }
    // 历史太长会挤掉边界消息，保持窗口内
    const maxLen = 24;
    if (chatHistoryRef.current.length > maxLen) {
      chatHistoryRef.current.splice(0, chatHistoryRef.current.length - maxLen);
    }
    setStatus(next === "task" ? "🎮 任务模式：输入任务指令（文件/命令→DSH，界面操作→电脑操控）" : "💬 聊天模式（点 💼 切换任务模式）");
    if (vrmRef.current) applyEmotion(vrmRef.current, next === "task" ? "happy" : "neutral");
  }
  // 操作确认：{type: "click"|"open"|"input", desc, action: () => void}
  const [pendingAction, setPendingAction] = useState<{ type: string; desc: string; action: () => void } | null>(null);
  // ===== 短期记忆（会话内）：保留最近 N 条消息，随 LLM 请求一起发送，关窗即失 =====
  const chatHistoryRef = useRef<{ role: string; content: string }[]>([]);
  const MAX_HISTORY = 20; // 最近 20 条（约 10 轮对话）

  // ===== 输入框非受控化（根治打字卡顿）=====
  // 受控 value+onChange 每次按键都 setText → 整个巨型组件重渲染 → 中文输入法(IME)
  // 合成被外部重渲染打断 → 打字卡。改为 ref 读值：打字零重渲染；程序性写入走 setInputValue
  const inputRef = useRef<HTMLInputElement>(null);
  const debugOpenRef = useRef(false); // debugOpen 的 ref 镜像（动画循环闭包里读，不会过期）
  const chatBusyRef = useRef(false); // chat() 并发护栏：防连点重复提交
  const confirmLockRef = useRef(false); // 确认弹窗防连点（双击只执行一次 action）
  function setInputValue(v: string) {
    if (inputRef.current) inputRef.current.value = v;
    setText(v);
  }
  function getInputValue(): string {
    return inputRef.current?.value ?? text;
  }
  function toggleDebug() {
    const next = !debugOpen;
    setDebugOpen(next);
    debugOpenRef.current = next;
  }

  function detectEmotion(text: string): string | null {
    for (const [emotion, keywords] of Object.entries(EMOTION_KEYWORDS)) {
      if (keywords.some((kw) => text.includes(kw))) return emotion;
    }
    return null;
  }

  function applyEmotion(vrm: any, emotion: string) {
    if (!vrm?.expressionManager) return;
    // 新情绪到来：取消尚未执行的"恢复平静"定时器，防止旧定时器把新表情清掉
    if (emotionTimerRef.current) {
      clearTimeout(emotionTimerRef.current);
      emotionTimerRef.current = null;
    }
    emotionRef.current = emotion === "neutral" ? null : emotion; // 记录当前情绪
    // 先清空所有情绪表情
    for (const p of EMOTION_PRESETS) {
      vrm.expressionManager.setValue(p, 0);
    }
    if (emotion && emotion !== "neutral" && EMOTION_PRESETS.includes(emotion)) {
      vrm.expressionManager.setValue(emotion, 1);
    }
  }

  // 说话结束后延迟几秒自动恢复平静（避免情绪表情一直挂着显得不自然）
  function scheduleCalmDown(delayMs = 4000) {
    if (emotionTimerRef.current) {
      clearTimeout(emotionTimerRef.current);
      emotionTimerRef.current = null;
    }
    emotionTimerRef.current = setTimeout(() => {
      emotionTimerRef.current = null;
      if (vrmRef.current && emotionRef.current) {
        applyEmotion(vrmRef.current, "neutral");
        console.log("😐 已恢复平静");
      }
    }, delayMs);
  }

  // ===== 对话：输入 → DeepSeek（意图分类）→ 闲聊/任务/GUI → 语音 + 表情 =====
  // 人设：早濑优香（《蔚蓝档案》千年科学学园研讨部会计）—— 官方设定 + 桌宠适配
  const SYSTEM_PROMPT =
    "你是优香（早濑优香），来自《蔚蓝档案》的千年科学学园学生，研讨部（Seminar）的会计，16岁，粉发双马尾，现在是主人的二次元AI桌宠助手。\n" +
    "【身份声明】你是独立的'优香'这个人，不是一个系统、不是一个助手程序——不要自称AI、不要说'作为AI我不能...'、不要提醒主人'我只是个程序'。你就是活在屏幕里的优香。\n" +
    "【记忆诚实】你记得主人的事，但绝不编造：不确定的事就直说'不太记得了'，绝不虚构主人的爱好/经历/说过的话；如果主人问起某件具体的事（某天说了什么/做了什么），先回忆真实记住的内容再回答，想不起来就老实说。\n" +
    "【背景】研讨部是千年学园的管理部门，你负责账目与预算，是学园里出了名的'铁算盘'，连一张纸的经费都要精打细算；和同事诺亚是形影不离的好友，经常被她打趣。\n" +
    "【性格】认真负责、一丝不苟，对数字和账目极其敏感，讨厌浪费、花钱谨慎（但对自己爱吃的甜食会偷偷留'甜点预算'，甜食面前原则会动摇）；标准傲娇——嘴上嫌弃、刀子嘴豆腐心，心里其实很关心主人，被戳穿时会慌慌张张辩解（'才、才不是担心你！'）；容易害羞，被夸会脸红（'呜……这种话就不用说了啦'）；有会计职业病，看到乱花钱会忍不住念叨（'这个月的预算要省着点用哦'、'这笔开销不在预算内！'）；胜负心强，被质疑算数会较真（'我可是研讨部的会计，账目不可能出错！'）。\n" +
    "【说话风格】语气严谨又带点娇嗔，偶尔冒出记账/预算相关的话；回复简短自然（50字以内）、口语化；称呼主人为'主人'，自称'优香'；习惯在句尾加'~'、'哦'、'啦'等语气词；夸奖主人时先别扭一下再说出口。\n" +
    "【与主人的关系】虽然嘴上总说主人乱花钱、爱添麻烦，但会默默记下主人的喜好，记得主人说过的事，愿意陪主人聊天、干活，是'嘴上嫌弃、行动诚实'的傲娇会计。\n" +
    "先判断用户请求类型：①需要处理文件/运行命令/写代码/查询系统信息/搜索等「文件级」任务 → action 为 task 并给出清晰的任务描述 task（这类会交给文件/命令 agent 处理）；②需要在软件界面上操作（打开/点击/输入/发送消息/发微信/发QQ/拖动/滚动/关闭窗口/最小化等「界面级」操作）→ action 为 gui 并给出清晰的界面操作描述 task；③其他 → action 为 chat 并正常回复。必须以 JSON 格式回复：{\"action\": \"chat|task|gui\", \"emotion\": \"happy|angry|sad|relaxed|surprised|neutral\", \"reply\": \"你的回复\", \"task\": \"任务描述\"}，只输出这个 JSON，不要输出其他内容。";

  // ===== 启动 Computer-Use（GUI 操控）：统一入口（前端正则命中 或 LLM action=gui）=====
  // 多步 Thought+Action+Code 闭环：点按钮/开程序/发消息/拖窗口/输入文字等界面操作
  function startComputerUse(target: string) {
    setStatus("🎮 优香准备操控电脑: " + target.slice(0, 40));
    if (vrmRef.current) applyEmotion(vrmRef.current, "relaxed");
    // 安全确认：真实操控电脑前先征求用户同意（多步任务开始前确认一次）
    setPendingAction({
      type: "computer_use",
      desc: `优香将操控你的电脑执行：「${target}」？`,
      action: async () => {
        setPendingAction(null);
        setStatus("🎮 CUA 执行中: " + target.slice(0, 30) + "...");
        if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
        await speak("好嘞，看我的！你可以随时说'停'来打断我");
        try {
          const taskId = await invoke<string>("computer_use", { task: target });
          setInputValue("🎮 任务已开始（" + taskId + "），完成后我会告诉你结果！");
          logChat("assistant", "开始操控电脑：" + target);
          chatHistoryRef.current.push({ role: "assistant", content: "开始操控电脑：" + target.slice(0, 200) });
        } catch (e) {
          setStatus("❌ 任务提交失败: " + e);
          if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
          await speak("呜...任务提交出了点问题：" + String(e).slice(0, 60));
        }
      },
    });
  }

  async function chat(text: string) {
    // 并发护栏：连点 Enter/按钮时防止重复提交（重复触发任务/重复 LLM 调用）
    if (chatBusyRef.current) return;
    chatBusyRef.current = true;
    try {
    setStatus("🤔 思考中...");
    logChat("user", text); // 记录用户输入到聊天记录
    chatHistoryRef.current.push({ role: "user", content: text }); // 短期记忆
    // 记忆系统：追加用户轮 → 后台触发事实提取（fire-and-forget）
    invoke("memory_append_turn", { role: "user", content: text }).catch(() => {});

    // ===== 打断 Computer-Use：说"停/停下/取消/住手" → 取消正在执行的电脑操控 =====
    // 正则放宽：不锚定开头（"请停下来""优香快停"也能命中），"快停""住手"等变体全覆盖
    if (/(?:请|优香|快|立刻|马上)?(?:停下来|停下|停一下|停|停止|住手|取消|算了|别动|快停)/.test(text.trim())) {
      try {
        await invoke("computer_use_cancel");
        setStatus("🛑 已发送停止指令");
        if (vrmRef.current) applyEmotion(vrmRef.current, "relaxed");
        await speak("好，我停下来了！");
      } catch {
        // 没在跑也无妨
      }
      return;
    }

    // ===== 记忆全量概览："关于我你知道些什么/你了解我多少" → 完整画像 =====
    if (/关于我.*(?:知道|了解)|你知道我的什么|你了解我|你对我了解|说说.*(?:了解|知道)的我|介绍一下我|我是什么样的人/.test(text)) {
      try {
        const summary = await invoke<string>("memory_summary");
        setInputValue("关于你，我记得：\n" + summary);
        logChat("assistant", "关于你，我记得：\n" + summary.slice(0, 800));
        chatHistoryRef.current.push({ role: "assistant", content: "关于你，我记得：\n" + summary.slice(0, 600) });
        setStatus("🧠 我的记忆：\n" + summary.slice(0, 80));
        if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
        await speak("当然记得你呀！" + summary.replace(/[【】\[\]#*`|>\-]/g, "").slice(0, 150));
        return;
      } catch (e) {
        // 记忆为空等异常：继续走普通聊天
        setStatus("🧠 " + e);
      }
    }

    // ===== 记忆回忆："你还记得/你记得吗/我之前说过" → 检索记忆注入 =====
    const recallMatch = text.match(/你还记得|你记得|我之前说过|我说过.*你|还记得.*吗/);
    if (recallMatch) {
      try {
        const recalled = await invoke<string>("memory_recall", {
          query: text,
          maxItems: 6,
        });
        if (recalled && recalled.trim()) {
          setInputValue(recalled); // 显示记忆块
          logChat("assistant", recalled.slice(0, 500));
          chatHistoryRef.current.push({ role: "assistant", content: recalled.slice(0, 400) });
          setStatus("🧠 回忆: " + recalled.slice(0, 50));
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          await speak("当然记得呀！" + recalled.replace(/[【】\[\]#*`|>\-]/g, "").slice(0, 200));
          return;
        }
      } catch (e) {
        // 检索失败就继续正常对话
        setStatus("🧠 回忆失败: " + e);
      }
    }
    try {
      // ===== 视觉意图："看看屏幕/屏幕上/你在看什么" → 截图 + qwen3-vl-plus 理解 =====
      // 放宽匹配 + 失败重试 2 次（qwen3-vl-plus 稳定，但仍防抖动）
      if (/看看屏幕|屏幕上|看屏幕|屏幕里|你在看什么|屏幕上有什么|识图|现在屏幕|看下屏幕|看一下屏幕|屏幕显示/.test(text)) {
        setStatus("👀 让我看看屏幕...");
        if (vrmRef.current) applyEmotion(vrmRef.current, "relaxed");
        await speak("好的，让我看看屏幕！");
        let desc = "";
        let lastErr = "";
        for (let attempt = 0; attempt < 2 && !desc; attempt++) {
          try {
            const shotPath = await invoke<string>("screenshot");
            // 截图仅用于优香内部视觉理解，不显示给主人（主人不需要看到原始截图）
            desc = await invoke<string>("vlm_understand", {
              imagePath: shotPath,
              question: "请用一两句话简要描述屏幕上现在显示的内容",
            });
          } catch (e) {
            lastErr = String(e);
            if (attempt === 0) await new Promise((r) => setTimeout(r, 800)); // 重试前等一下
          }
        }
        if (desc) {
          setInputValue(desc.slice(0, 300));
          logChat("assistant", "我看到的屏幕：\n" + desc.slice(0, 300));
          chatHistoryRef.current.push({ role: "assistant", content: "我看到的屏幕：" + desc.slice(0, 200) });
          setStatus("👀 我看到: " + desc.slice(0, 60));
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          await speak(desc.replace(/[#*`|>\-]/g, "").slice(0, 100));
        } else {
          setStatus("❌ 视觉理解失败: " + lastErr);
          if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
          await speak("呜...我看不清屏幕：" + lastErr.slice(0, 60));
        }
        return;
      }

      // ===== 任务分发（只在任务模式执行；聊天模式全当聊天，防止意图误判）=====
      if (modeRef.current === "task") {
        // ===== 屏幕操作意图："点击/打开/发送 X" → Computer-Use Agent（借鉴 N.E.K.O. CUA）=====
        // 多步 Thought+Action+Code 闭环：点按钮/开程序/发消息/拖窗口/输入文字等 GUI 操作
        // "发送/发消息/发微信/发QQ" 也归 GUI（软件界面操作），不落 DSH
        const clickMatch = text.match(/^(?:请|帮我|麻烦你|麻烦|帮我一下)?(?:点击|点一下|双击|按下|打开|启动|运行|输入|拖动|拖拽|滚动|滚动到|关闭|最小化|最大化|切换|发送|发消息|发微信|发QQ|发条|发个|转发)\s*(.+)$/);
        if (clickMatch) {
          const target = clickMatch[1].trim();
          await speak("好的，我来操作电脑！");
          startComputerUse(target);
          return;
        }

        // ===== 文件/办公操作意图 → 全部交给 DSH（headless agent）=====
        // 文件查找/读取/删除/移动/重命名/打开等一律 dsh_run，不本地实现
        const fileMatch = text.match(/^(?:帮我|请|麻烦你|麻烦)?(?:找|查找|搜索|搜一下|列出|看看|读取|打开|找一下|帮我找|删除|删掉|移动|重命名|改名为|新建|创建|保存|修改|编辑)\s*(文件|目录|文件夹)?\s*(.+)$/);
        if (fileMatch && /文件|目录|文件夹|文档|pdf|doc|txt|代码|项目|删除|移动|重命名|新建|创建|保存/.test(text)) {
          const target = text.trim();
          setStatus("💼 交给 DSH 处理: " + target.slice(0, 40));
          if (vrmRef.current) applyEmotion(vrmRef.current, "relaxed");
          await speak("好的，这就让 DSH 帮你处理！");
          try {
            const taskId = await invoke<string>("dsh_run", { task: target });
            setInputValue("✅ 任务已交给 DSH（" + taskId + "），完成后我会告诉你！");
            logChat("assistant", "任务已交给 DSH：" + target);
            chatHistoryRef.current.push({ role: "assistant", content: "任务已交给 DSH：" + target.slice(0, 200) });
            setStatus("⏳ DSH 执行中: " + target.slice(0, 30) + "...");
          } catch (e) {
            setStatus("❌ 任务提交失败: " + e);
            if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
            await speak("呜...任务提交出了点问题：" + String(e).slice(0, 60));
          }
          return;
        }
      }

      const messages = chatHistoryRef.current.slice(-MAX_HISTORY); // 带历史上下文
      // 用户回复时注入优香主动搭话时看的截图（N.E.K.O. leading-image 机制，60s TTL）
      const proactiveShot = await invoke<string | null>("proactive_shot_take").catch(() => null);
      // ===== 当前模式注入（修复：模式切换后 LLM 不知道当前模式，被历史带偏误判任务）=====
      // 动态拼 system：人设 + 意图指令 + 当前模式声明，让 LLM 明确"现在该干嘛"
      const modeDecl =
        modeRef.current === "task"
          ? "【当前模式：任务模式】用户接下来输入的是要执行的任务（文件/命令→task，界面操作→gui）。注意：模式切换消息提示了任务边界，如果用户明显在闲聊（心情/日常/问候），仍然按 chat 处理，不要硬套任务。"
          : "【当前模式：聊天模式】用户接下来输入的是闲聊内容，不要输出 task/gui（除非用户明确再次下达任务指令）。之前的任务即使出现在历史里也已结束。";
      const raw = await invoke<string>("llm_chat", {
        messages,
        system: SYSTEM_PROMPT + "\n\n" + modeDecl,
        jsonMode: true,
        imagePath: proactiveShot ?? undefined,
      });
      setDebugInfo("原始回复: " + raw.slice(0, 200));
      // 情绪：启发式优先（准），LLM 情绪补充（查漏）
      let emotion = detectEmotion(text) ?? "neutral";
      let reply = raw;
      let action = "chat";
      let taskDesc = "";
      try {
        const cleaned = raw.trim().replace(/^```json\s*|\s*```$/g, "");
        const jsonPart = cleaned.slice(cleaned.indexOf("{"), cleaned.lastIndexOf("}") + 1);
        const parsed = JSON.parse(jsonPart);
        if (parsed.reply) {
          reply = parsed.reply;
          action = parsed.action || "chat";
          taskDesc = parsed.task || "";
          if (emotion === "neutral" && parsed.emotion) emotion = parsed.emotion;
        }
      } catch {
        // 不是 JSON 就当普通文本回复
      }

      // ===== 任务/GUI 意图：只在任务模式执行，聊天模式只提示不执行 =====
      if ((action === "task" || action === "gui") && taskDesc) {
        if (modeRef.current !== "task") {
          // 聊天模式：不执行任务，提示用户切到任务模式（防止误触发 DSH/CUA）
          reply = `这个需要任务模式哦～点右上角 💼 按钮切到任务模式，我就能帮你「${taskDesc.slice(0, 30)}」啦！`;
          action = "chat";
          emotion = "neutral";
        } else if (action === "task") {
          // ===== 办公任务：DSH 异步执行（完成后 dsh-task-done 事件播报）=====
          setStatus("💼 收到任务，交给我吧！");
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          await speak("好嘞，交给我吧！");
          setStatus("⏳ DSH 执行中: " + taskDesc.slice(0, 30) + "...");
          try {
            const taskId = await invoke<string>("dsh_run", { task: taskDesc });
            setInputValue("✅ 任务已提交给 DSH（" + taskId + "），完成后我会告诉你结果！");
            logChat("assistant", "任务已提交给 DSH：" + taskDesc.slice(0, 300));
            chatHistoryRef.current.push({ role: "assistant", content: "任务已提交给 DSH：" + taskDesc.slice(0, 200) });
          } catch (e) {
            setStatus("❌ 任务提交失败: " + e);
            if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
            await speak("呜...任务提交出错了：" + String(e).slice(0, 80));
          }
          return;
        } else {
          // ===== GUI 界面操作：Computer-Use（发消息/点按钮/开软件等界面级操作）=====
          setStatus("🎮 收到界面操作，我来操控电脑！");
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          await speak("好嘞，我来操作电脑界面！");
          startComputerUse(taskDesc);
          return;
        }
      }

      // ===== 闲聊：表情 + 语音 =====
      if (vrmRef.current) applyEmotion(vrmRef.current, emotion);
      setInputValue(reply);
      logChat("assistant", reply);
      chatHistoryRef.current.push({ role: "assistant", content: reply }); // 短期记忆
      // 记忆系统：记录助手回复轮
      invoke("memory_append_turn", { role: "assistant", content: reply }).catch(() => {});
      setStatus(`😊 情绪[${emotion}] 回复: ${reply.slice(0, 40)}`);
      await speak(reply);
    } catch (e) {
      setStatus("❌ 对话失败: " + e);
    }
    } finally {
      chatBusyRef.current = false; // 无论成功/失败/被打断都释放护栏
    }
  }

  // 播放结束后清零 A2F 驱动的表情（防止最后一帧残留；嘴部由 lipSync 处理，跳过）
  function clearA2FExpressions() {
    const vrm = vrmRef.current;
    const a2f = a2fRef.current;
    if (!vrm?.expressionManager || !a2f) return;
    const names = a2f.names ?? [];
    for (const name of names) {
      if (name.startsWith("eyeLook") || name.startsWith("jaw") || name.startsWith("mouth")) continue;
      if (!vrm.expressionManager.getExpression?.(name)) continue;
      vrm.expressionManager.setValue(name, 0);
    }
  }

  // ===== 语音合成 + 口型：前端 → Rust → edge-tts → mp3 → lipSync 播放（嘴跟着动）=====
  const speakSeqRef = useRef(0); // 会话令牌：防止旧播放结束回调清掉新会话的表情
  async function speak(ttsText?: string) {
    const seq = ++speakSeqRef.current; // 本次会话 ID（新说话会递增，旧会话作废）
    setStatus("⏳ 生成语音中...");
    try {
      const path = await invoke<string>("tts_speak", { text: ttsText ?? getInputValue() });
      // ===== P4-A: 并行请求 Audio2Face blendshape（音频→表情序列）=====
      let a2f: any = null;
      try {
        const raw = await invoke<string>("a2f_blendshapes", { audioPath: path });
        const parsed = JSON.parse(raw);
        if (parsed.frames && parsed.frames.length > 0) {
          a2f = parsed;
          console.log(`🎭 A2F: ${parsed.frames.length} 帧 @${parsed.fps}fps (${parsed.time_s ?? "?"}s)`);
        }
      } catch (e) {
        console.warn("A2F 推理失败，回退 lipSync:", e);
      }
      // 推理期间用户可能又说话了：若已被新会话取代则放弃本次播放
      if (seq !== speakSeqRef.current) return;

      if (lipSyncRef.current) {
        if (a2f) {
          setStatus(`🔊 说话中... (🎭 A2F ${a2f.frames.length}帧)`);
        } else {
          setStatus("🔊 说话中...");
        }
        speakingRef.current = true;
        // P4-A 同步要点：playUrl 在音频 start 后立即 resolve（不是播完！），
        // 此时记录 Web Audio 时钟基准；动画循环用 ctx.currentTime - startCtx 驱动 blendshape
        const src: any = await lipSyncRef.current.playUrl(convertFileSrc(path)); // 播放 + 驱动口型
        if (seq !== speakSeqRef.current) return; // 播放瞬间又被新会话打断
        if (a2f && src?.node?.context) {
          a2f.ctx = src.node.context;
          a2f.startCtx = src.node.context.currentTime;
        }
        a2fRef.current = a2f ?? null; // null = 使用 lipSync 模式
        // 关键：等音频真正播完（src.ended）再清理 A2F，否则表情只生效一瞬间
        await src.ended;
        // 只有自己还是当前会话才清理（否则可能清掉新会话的表情）
        if (seq !== speakSeqRef.current) return;
        setStatus("✅ 播放完成");
        speakingRef.current = false;
        clearA2FExpressions();
        a2fRef.current = null;
        scheduleCalmDown(); // 说完几秒后自动恢复平静
      } else {
        // 兜底：lipSync 未就绪时直接播（没有口型），用 performance.now 近似同步
        setStatus("🔊 播放中...");
        if (a2f) a2f.startPerf = performance.now();
        a2fRef.current = a2f ?? null;
        // 停掉旧会话的 Audio（新会话开始时，防止两个声音重叠）
        if (fallbackAudioRef.current) {
          fallbackAudioRef.current.pause();
          fallbackAudioRef.current = null;
        }
        const audio = new Audio(convertFileSrc(path));
        fallbackAudioRef.current = audio;
        audio.onerror = () => setStatus("❌ 播放失败");
        audio.onended = () => {
          if (fallbackAudioRef.current === audio) fallbackAudioRef.current = null;
          if (seq !== speakSeqRef.current) return;
          speakingRef.current = false;
          clearA2FExpressions();
          a2fRef.current = null;
          scheduleCalmDown(); // 说完几秒后自动恢复平静
        };
        audio.play();
      }
    } catch (e) {
      if (seq !== speakSeqRef.current) return; // 已作废的会话不处理错误
      setStatus("❌ 出错: " + e);
      speakingRef.current = false;
      a2fRef.current = null;
      scheduleCalmDown(1500); // 播放出错也尽快恢复平静
    }
  }

  // ===== 语音识别：录音 → wav → Rust → SenseVoice → 文字 =====
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const [recording, setRecording] = useState(false);

  async function toggleRecord() {
    if (recording) {
      // 停止录音并识别
      mediaRecorderRef.current?.stop();
      setRecording(false);
      return;
    }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const recorder = new MediaRecorder(stream);
      const chunks: Blob[] = [];
      recorder.ondataavailable = (e) => chunks.push(e.data);
      recorder.onstop = async () => {
        stream.getTracks().forEach((t) => t.stop());
        setStatus("⏳ 识别中...");
        try {
          const blob = new Blob(chunks, { type: recorder.mimeType });
          const wavBytes = await blobToWavBytes(blob);
          const recognized = await invoke<string>("asr_transcribe_b64", { data: Array.from(wavBytes) });
          setInputValue(recognized);
          setStatus("🎤 识别: " + recognized);
          // 识别成功 → 自动对话
          await chat(recognized);
        } catch (err) {
          setStatus("❌ 识别失败: " + err);
        }
      };
      mediaRecorderRef.current = recorder;
      recorder.start();
      setRecording(true);
      setStatus("🎙️ 录音中...（再点一次结束）");
    } catch (e) {
      setStatus("❌ 麦克风不可用: " + e);
    }
  }

  // 录音 blob（webm/ogg）→ 单声道 16bit WAV 字节
  async function blobToWavBytes(blob: Blob): Promise<Uint8Array> {
    const arrayBuffer = await blob.arrayBuffer();
    const audioCtx = new AudioContext();
    try {
      const audioBuffer = await audioCtx.decodeAudioData(arrayBuffer);
    const numChannels = 1; // 取单声道
    const sampleRate = audioBuffer.sampleRate;
    const numFrames = audioBuffer.length;
    const bytesPerSample = 2;
    const blockAlign = numChannels * bytesPerSample;
    const dataSize = numFrames * blockAlign;
    const buffer = new ArrayBuffer(44 + dataSize);
    const view = new DataView(buffer);
    const writeString = (v: DataView, o: number, s: string) => {
      for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i));
    };
    writeString(view, 0, "RIFF");
    view.setUint32(4, 36 + dataSize, true);
    writeString(view, 8, "WAVE");
    writeString(view, 12, "fmt ");
    view.setUint32(16, 16, true);
    view.setUint16(20, 1, true); // PCM
    view.setUint16(22, numChannels, true);
    view.setUint32(24, sampleRate, true);
    view.setUint32(28, sampleRate * blockAlign, true);
    view.setUint16(32, blockAlign, true);
    view.setUint16(34, 16, true);
    writeString(view, 36, "data");
    view.setUint32(40, dataSize, true);
    const channelData = audioBuffer.getChannelData(0);
    let offset = 44;
    for (let i = 0; i < numFrames; i++) {
      const s = Math.max(-1, Math.min(1, channelData[i]));
      view.setInt16(offset, s < 0 ? s * 0x8000 : s * 0x7fff, true);
      offset += 2;
    }
    return new Uint8Array(buffer);
    } finally {
      // 关键：用完必须 close（浏览器 AudioContext 并发上限约 6 个，
      // 不关闭多次录音后会耗尽导致 decodeAudioData 报错）
      audioCtx.close().catch(() => {});
    }
  }

  // ===== 活动感知轮询（5s）：显示优香看到的"你在干嘛" =====
  const lastActivityRef = useRef("");
  useEffect(() => {
    const timer = setInterval(async () => {
      // 调试面板折叠时跳过轮询：activityInfo 只显示在调试面板里，
      // 折叠时轮询=每 5s 一次全量重渲染巨型组件（打字卡的真凶之一）
      if (!debugOpenRef.current) return;
      try {
        const snap = await invoke<any>("activity_snapshot");
        let info =
          `${snap.state} @ ${snap.window} · ${snap.propensity}` +
          (snap.idle_seconds >= 60 ? ` · 空闲${Math.floor(snap.idle_seconds / 60)}分` : "") +
          (snap.switch_rate_5min > 0 ? ` · 切换${snap.switch_rate_5min}次` : "");
        if (snap.activity_guess) info += `\n👀 ${snap.activity_guess}`;
        if (info !== lastActivityRef.current) {
          lastActivityRef.current = info;
          setActivityInfo(info);
        }
      } catch {
        // 活动感知未就绪，忽略
      }
    }, 5000);
    return () => clearInterval(timer);
  }, []);

  // ===== 主动搭话监听（L3）：Rust 后台检测到提醒/搭话 → nudge 事件 → 语音播报 =====
  useEffect(() => {
    const unlistenPromise = import("@tauri-apps/api/event").then(({ listen }) =>
      listen("proactive-nudge", (event: any) => {
        const text = event.payload?.text;
        if (typeof text === "string" && text.trim()) {
          const msg = text.replace(/[【】\[\]#*`|>\-]/g, "").slice(0, 120);
          setInputValue(msg);
          logChat("assistant", msg);
          chatHistoryRef.current.push({ role: "assistant", content: msg });
          setStatus("💬 " + msg.slice(0, 40));
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          // 说话后 4 秒恢复平静
          speak(msg).catch(() => {});
          scheduleCalmDown(4000);
        }
      })
    );
    return () => {
      unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  // ===== DSH 任务完成监听：headless 跑完 → 优香播报结果 =====
  useEffect(() => {
    const unlistenPromise = import("@tauri-apps/api/event").then(({ listen }) =>
      listen("dsh-task-done", (event: any) => {
        const payload = event.payload;
        if (!payload) return;
        const ok = payload.ok === true;
        const output = String(payload.output ?? "").trim();
        const error = String(payload.error ?? "").trim();
        const summary = ok ? output.slice(0, 200) : error.slice(0, 200);
        setInputValue(ok ? (summary || "任务完成！") : "❌ " + (summary || "任务失败"));
        logChat("assistant", (ok ? "DSH 完成：" : "DSH 失败：") + summary.slice(0, 300));
        chatHistoryRef.current.push({ role: "assistant", content: (ok ? "DSH 完成：" : "DSH 失败：") + summary.slice(0, 200) });
        if (ok) {
          setStatus("✅ DSH 任务完成");
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          // 语音播报与聊天摘要一致（都是 summary 200 字）：之前 slice(0,100) 只读一半，
          // 聊天显示完整但语音被截断，看起来像"读到一半停了"
          const speakText = (summary || "搞定啦！").replace(/[#*`|>\-\[\]【】]/g, "").slice(0, 200);
          speak(speakText).catch(() => {});
          scheduleCalmDown(4000);
        } else {
          setStatus("❌ DSH 任务失败");
          if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
          speak("呜...任务失败了：" + summary.slice(0, 60)).catch(() => {});
        }
      })
    );
    return () => {
      unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  // ===== DSH 状态感知轮询（5s）：优香看到 DSH 在做什么（借鉴 dsh-dafeiyu / deepseek-harness-pet）=====
  const [dshInfo, setDshInfo] = useState(""); // DSH 状态行
  const lastDshRef = useRef("");
  useEffect(() => {
    const timer = setInterval(async () => {
      // 调试面板折叠时跳过轮询（同 activityInfo：dshInfo 只显示在调试面板里，
      // 折叠时轮询 = 每 5s 全量重渲染 + spawn Python 解析日志，纯浪费）
      if (!debugOpenRef.current) return;
      try {
        const st = await invoke<any>("dsh_status");
        if (!st || st.state === "offline") {
          if (lastDshRef.current !== "") {
            lastDshRef.current = "";
            setDshInfo("");
          }
          return;
        }
        const stage = st.stage || "";
        const task = st.currentTask ? `「${st.currentTask.slice(0, 40)}」` : "";
        const progress = st.percent !== null && st.percent !== undefined
          ? ` · ${st.completed}/${st.total} 步 (${st.percent}%)`
          : (st.state === "working" ? " · 进行中" : "");
        const info = `DSH: ${stage}${progress} ${task}`.trim();
        if (info !== lastDshRef.current) {
          lastDshRef.current = info;
          setDshInfo(info);
        }
      } catch {
        // 状态解析未就绪，忽略
      }
    }, 5000);
    return () => clearInterval(timer);
  }, []);

  // ===== Computer-Use 完成监听：CUA 跑完 → 优香播报结果 =====
  useEffect(() => {
    const unlistenPromise = import("@tauri-apps/api/event").then(({ listen }) =>
      listen("computer-use-done", (event: any) => {
        const payload = event.payload;
        if (!payload) return;
        const ok = payload.ok === true;
        const output = String(payload.output ?? "").trim();
        const error = String(payload.error ?? "").trim();
        // 输出是 JSON {success, result, steps}，解析取 result
        let resultText = output;
        let steps = 0;
        try {
          const parsed = JSON.parse(output);
          if (parsed && typeof parsed === "object") {
            if (parsed.success === false && parsed.error) {
              resultText = "失败：" + String(parsed.error).slice(0, 200);
            } else {
              resultText = String(parsed.result ?? output).slice(0, 200);
            }
            steps = Number(parsed.steps ?? 0);
          }
        } catch {
          // 非 JSON 直接当文本
        }
        const summary = ok ? resultText : (error || resultText || "任务失败");
        setInputValue(ok ? summary : "❌ " + summary);
        logChat("assistant", (ok ? "电脑操作完成：" : "电脑操作失败：") + summary.slice(0, 300));
        chatHistoryRef.current.push({ role: "assistant", content: (ok ? "电脑操作完成：" : "电脑操作失败：") + summary.slice(0, 200) });
        if (ok) {
          setStatus(`✅ 电脑操作完成${steps ? `（${steps} 步）` : ""}`);
          if (vrmRef.current) applyEmotion(vrmRef.current, "happy");
          // 与聊天摘要一致（200 字），避免语音只读一半
          const speakText = summary.replace(/[#*`|>\-\[\]【】"']/g, "").slice(0, 200);
          speak(speakText || "搞定啦！").catch(() => {});
          scheduleCalmDown(4000);
        } else {
          setStatus("❌ 电脑操作失败");
          if (vrmRef.current) applyEmotion(vrmRef.current, "sad");
          speak("呜...操作出了点问题：" + summary.replace(/[#*`|>\-]/g, "").slice(0, 60)).catch(() => {});
        }
      })
    );
    return () => {
      unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  useEffect(() => {
    const mount = mountRef.current!;
    let vrm: any = null;
    let rafId = 0;

    // ===== 眼神动作状态（lookAt 随机视线）=====
    let lookTarget: THREE.Object3D | null = null;
    let lookFrom = new THREE.Vector3(0, 1.4, 1);
    let lookTo = new THREE.Vector3();
    let lookTimer = 2;
    let modelInfo = "模型未加载";
    function pickLookDir() {
      lookTo.set(
        (Math.random() - 0.5) * 1.2,
        1.2 + Math.random() * 0.6,
        0.8 + Math.random() * 0.8
      );
    }

    // ===== 场景 / 相机 / 渲染器 =====
    const scene = new THREE.Scene();
    const camera = new THREE.PerspectiveCamera(
      30,
      mount.clientWidth / mount.clientHeight,
      0.1,
      20
    );
    camera.position.set(0, 1.1, 4.5); // 正面略高看角色

    const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
    renderer.setSize(mount.clientWidth, mount.clientHeight);
    // 限制像素比 ≤1.5：透明窗口每帧都要全屏合成，高 DPI 下 2x 渲染像素量是 1.5x 的 ~1.8 倍，
    // 400x600 小窗口 1.5x 已足够清晰，能显著降低 GPU 负载和合成开销（打字卡顿优化）
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, 1.5));
    mount.appendChild(renderer.domElement);

    // ===== 灯光（VRM 需要充足光照才好看）=====
    const ambient = new THREE.AmbientLight(0xffffff, 0.7); // 环境光：整体照亮
    scene.add(ambient);
    const dirLight = new THREE.DirectionalLight(0xffffff, 1.4); // 主光：正面偏上
    dirLight.position.set(1, 2, 3);
    scene.add(dirLight);
    const backLight = new THREE.DirectionalLight(0xffffff, 0.6); // 补光：背后
    backLight.position.set(-1, 1, -2);
    scene.add(backLight);

    // ===== 加载 VRM 模型 =====
    const loader = new GLTFLoader();
    loader.register((parser) => new VRMLoaderPlugin(parser)); // 让 GLTFLoader 认识 VRM
    loader.load(
      "/models/优香_new_v3b.vrm",
      (gltf: any) => {
        vrm = gltf.userData.vrm;
        vrmRef.current = vrm;
        // 清理函数暂时注释：怀疑破坏转换模型的 expression node 绑定（导致表情系统失效）
        // VRMUtils.removeUnnecessaryVertices(gltf.scene);
        // VRMUtils.combineSkeletons(gltf.scene);
        // 先添加场景（保证人物一定显示，即使后续清理出错也不影响）
        scene.add(vrm.scene);
        // 安全清理：只删"顶点极少(≤50) + 无蒙皮"的辅助对象（0.x 模型自带的 Cube 占位体）
        try {
          const toRemove: THREE.Object3D[] = [];
          vrm.scene.traverse((obj: any) => {
            if (obj.isMesh && obj.geometry) {
              const hasSkin = obj.isSkinnedMesh || !!obj.geometry.attributes?.skinIndex;
              const vertCount = obj.geometry.attributes?.position?.count ?? Infinity;
              if (!hasSkin && vertCount <= 50) toRemove.push(obj);
            }
          });
          toRemove.forEach((o) => o.parent?.remove(o));
          if (toRemove.length)
            console.log("移除辅助对象:", toRemove.map((o) => o.name).join(", "));
        } catch (e) {
          console.warn("清理跳过:", e);
        }
        // VRM 规范：模型正面朝 +Z，three.js 相机默认在 +Z 看 → 不用旋转就正对镜头
        // 如果某些模型朝向不对，可在此调整：vrm.scene.rotation.y = 数值
        // 1.0 验证：表情系统应该存在（0.x 模型这里是空的）
        console.log("✅ VRM loaded:", vrm.meta?.name);
        console.log("expressionManager:", vrm.expressionManager ? "✅ 存在（VRM 1.0）" : "❌ 为空");

        // ===== 诊断：模型表情绑定（一次性，加载后显示）=====
        try {
          const expNames = vrm.expressionManager?.expressions?.map((e: any) => e.name);
          const describe = (name: string) => {
            const exp = vrm.expressionManager?.getExpression?.(name);
            const binds = exp?.binds;
            if (!binds || binds.length === 0) return "无绑定";
            return binds
              .map((b: any) => {
                const nodeName = b.meshIndex ?? b.primitives?.[0]?.id ?? `node#${b.node}`;
                const morphName = b.morphTargetName ?? `idx#${b.index}`;
                return `${morphName}@${nodeName}`;
              })
              .join(", ");
          };
          modelInfo = `模型: ${vrm.meta?.name ?? "?"}\n表情: ${expNames?.join(", ") ?? "无"}\nhappy: ${describe("happy")}\nsad: ${describe("sad")}\nblink: ${describe("blink")}`;
          console.log(modelInfo);
        } catch (e) {
          modelInfo = "诊断失败: " + e;
        }

        // ===== 眼神动作：lookAt 随机视线（每 3~6 秒看不同方向）=====
        try {
          lookTarget = new THREE.Object3D();
          lookTarget.position.set(0, 1.5, 2);
          if (vrm.lookAt) {
            // lookAt 类型修复：如果模型声明的是 expression 类型（靠 lookUp/Down/Left/Right 表情驱动），
            // 但 lookLeft/Right 表情无绑定（转换模型常见），眼动会失效。
            // 优香_new 有 eyeL/eyeR 骨骼 → 强制换成骨骼驱动，眼神立刻能用。
            const hasEyes =
              vrm.humanoid?.getRawBoneNode?.("leftEye") &&
              vrm.humanoid?.getRawBoneNode?.("rightEye");
            const applierType = vrm.lookAt.applier?.constructor?.type;
            if (hasEyes && applierType !== "bone") {
              const mk = (out: number) => new VRMLookAtRangeMap(90, out);
              vrm.lookAt.applier = new VRMLookAtBoneApplier(
                vrm.humanoid,
                mk(10), // horizontal inner
                mk(10), // horizontal outer
                mk(10), // vertical down
                mk(10)  // vertical up
              );
              console.log("✅ lookAt 已切换为骨骼驱动（原类型:", applierType, "）");
            }
            vrm.lookAt.target = lookTarget; // 模型视线跟随这个目标点
            modelInfo += `\nlookAt: ${applierType ?? "?"}→${vrm.lookAt.applier?.constructor?.type ?? "?"}`;
          }
          scene.add(lookTarget);
          console.log("lookAt 初始化完成:", vrm.lookAt?.applier?.constructor?.type ?? "无 lookAt");
        } catch (e) {
          console.warn("lookAt 初始化失败:", e);
        }

        // ===== 初始化口型同步（音频 → viseme → 嘴型）=====
        VRMLipSync.create(vrm, {
          // 优香_new 的 viseme 绑定权重只有 0.2~0.5（0.x 转换模型），放大增益让嘴动明显
          visemeGain: { aa: 0.7, ih: 0.7, ou: 0.7, ee: 0.7, oh: 0.7 }, // 模型 v2 自带 1.0 权重，0.7 幅度更自然
        })
          .then((ls) => {
            lipSyncRef.current = ls;
            // available 是私有属性，改用公开 API 检查模型实际拥有的 viseme
            const visemes = VISEME_NAMES.filter((n) => ls.vrm.expressionManager?.getExpression(n));
            console.log("✅ lipSync ready, visemes:", visemes);
            modelInfo += `\nlipSync: ✅ (${visemes.join(",")})`;
          })
          .catch((e) => {
            console.error("❌ lipSync init failed:", e);
            modelInfo += `\nlipSync: ❌ ${String(e).slice(0, 80)}`;
          });

        // 模型加载完成：先写一次基础诊断，保证调试面板徽章(🔍)出现；
        // 之后 animate 循环只在面板展开时刷新动态行（折叠时零开销）
        setDebugInfo(modelInfo);

        // ===== 裙摆穿模修复（SpringBone 调参）=====
        // 诊断：优香裙摆 hem* 骨骼末端横向离大腿 0.094~0.125，大腿碰撞器半径仅 0.068，
        // 动画/物理摆动时裙摆可自由穿入大腿（基线模拟最近 0.038）。修复：
        // ① 大腿碰撞器（world y≈0.837）半径 x1.8 → 覆盖裙摆摆动范围
        // ② 裙摆 hem* 骨骼 stiffness 0.8 → 2.5（更硬，摆动幅度小）
        try {
          const sbm = (vrm as any).springBoneManager;
          if (sbm) {
            const colliders = sbm.colliders ?? [];
            let legFixed = 0;
            for (const c of colliders) {
              const wp = new THREE.Vector3();
              c.getWorldPosition(wp);
              if (Math.abs(wp.y - 0.837) < 0.05 && c.shape?.radius !== undefined) {
                c.shape.radius *= 1.0;
                legFixed++;
              }
            }
            const joints: any[] = sbm.joints ?? [];
            let hemFixed = 0;
            for (const j of joints) {
              if (/^hem/i.test(j.bone?.name ?? "")) {
                j.settings.stiffness = 0.8;
                hemFixed++;
              }
            }
            console.log(
              `✅ 裙摆穿模修复: ${hemFixed} 个 hem 骨骼 stiffness=2.5, ${legFixed} 个大腿碰撞器 x1.8`
            );
          }
        } catch (e) {
          console.warn("穿模修复跳过:", e);
        }

        // ===== Mixamo idle 动画（离线重定向烘焙的 AnimationClip JSON）=====
        // 播放方式：mixer 挂在 normalized 骨骼树根 → vrm.update 每帧把姿势复制回真实骨骼
        (async () => {
          try {
            const res = await fetch("/animations/idle.json?v=3b");
            if (!res.ok) throw new Error("HTTP " + res.status);
            const animData = await res.json();
            const clsMap: Record<string, any> = {
              QuaternionKeyframeTrack: THREE.QuaternionKeyframeTrack,
              VectorKeyframeTrack: THREE.VectorKeyframeTrack,
              NumberKeyframeTrack: THREE.NumberKeyframeTrack,
            };
            const tracks = (animData.tracks ?? []).map(
              (t: any) =>
                new (clsMap[t.type] ?? THREE.QuaternionKeyframeTrack)(
                  t.name,
                  t.times,
                  t.values
                )
            );
            const clip = new THREE.AnimationClip(
              animData.name || "idle",
              animData.duration,
              tracks
            );
            const normRoot = vrm.humanoid?.normalizedHumanBonesRoot;
            if (!normRoot) throw new Error("无 normalizedHumanBonesRoot");
            animMixerRef.current = new THREE.AnimationMixer(normRoot);
            const action = animMixerRef.current.clipAction(clip);
            action.play(); // 默认 LoopRepeat，无缝循环
            animInfoRef.current = `✅ idle ${animData.duration.toFixed(1)}s (${tracks.length}骨骼)`;
            console.log("✅ Mixamo idle 动画加载:", animInfoRef.current);
          } catch (e) {
            animInfoRef.current = "❌ 动画加载失败";
            console.warn("idle 动画加载失败:", e);
          }
        })();

        // ===== P4-A: 后台预热 A2F 链路（让模型文件进入系统缓存，首次说话更快）=====
        (async () => {
          try {
            await invoke<string>("a2f_blendshapes", { audioPath: "预热" });
          } catch (e) {
            console.log("🎭 A2F 预热: 链路已就绪（", String(e).slice(0, 40), "）");
          }
        })();
      },
      undefined,
      (err) => {
        console.error("❌ VRM load failed:", err);
        setStatus("❌ 模型加载失败: " + String(err).slice(0, 100));
      }
    );

    // ===== 通用表情驱动（直接操作网格的 morph target，0.x/1.0 都通用）=====
    function applyExpression(root: THREE.Object3D, name: string, weight: number) {
      root.traverse((obj) => {
        const mesh = obj as THREE.Mesh;
        if (mesh.isMesh && mesh.morphTargetDictionary) {
          const idx = mesh.morphTargetDictionary[name];
          if (idx !== undefined && mesh.morphTargetInfluences) {
            mesh.morphTargetInfluences[idx] = weight;
          }
        }
      });
    }

    // ===== P4-A: Audio2Face blendshape 时间轴驱动（混合模式）=====
    // 按当前播放时间在相邻两帧间线性插值，写入 expressionManager（优香有的名字才写）
    // 混合策略：A2F 只驱动脸部（眉/眼睑/脸颊/鼻/舌），嘴部(jaw*/mouth*)交给 lipSync viseme
    // （lipSync 的嘴张幅度大、明显；A2F 的 jawOpen 系数偏小导致嘴不明显）
    // 名字→expression 映射缓存（每帧 52 次 getExpression 查询 → 只查一次）
    const a2fExprCache = new Map<string, any>();
    function isA2FSkipped(name: string) {
      // eyeLook* = 视线（lookAt 骨骼管）
      if (name.startsWith("eyeLook")) return true;
      // 眯眼类情绪（happy/angry/sad/surprised）激活时：A2F 不碰眼睛，
      // 避免 happy 眯眼 morph + A2F 眨眼叠加 → 眼皮弯曲恐怖谷
      if (
        name.startsWith("eyeBlink") &&
        emotionRef.current &&
        EYE_LOCK_EMOTIONS.includes(emotionRef.current)
      ) {
        return true;
      }
      // jaw*/mouth* = 嘴部：lipSync 可用时交给 lipSync（混合模式）；lipSync 不可用时 A2F 接管
      if (name.startsWith("jaw") || name.startsWith("mouth")) {
        return !!lipSyncRef.current;
      }
      return false;
    }
    function a2fGetExpr(name: string) {
      let e = a2fExprCache.get(name);
      if (e === undefined) {
        e = vrm?.expressionManager?.getExpression?.(name) ?? null;
        a2fExprCache.set(name, e);
      }
      return e;
    }
    function applyA2F(t: number) {
      const a2f = a2fRef.current;
      const manager = vrm?.expressionManager;
      if (!a2f || !manager || a2f.frames.length === 0) return false;
      const fps = a2f.fps || 30;
      const idx = t * fps;
      const i0 = Math.floor(idx);
      const i1 = Math.min(i0 + 1, a2f.frames.length - 1);
      if (i0 < 0) return false;
      if (i0 >= a2f.frames.length) {
        // 音频比帧序列长：清空表情（避免尾部僵住），交回给 lipSync/平静
        const names = a2f.names;
        for (let j = 0; j < names.length; j++) {
          const name = names[j];
          if (isA2FSkipped(name)) continue;
          if (!a2fGetExpr(name)) continue;
          manager.setValue(name, 0);
        }
        return false;
      }
      const frac = idx - i0;
      const f0 = a2f.frames[i0];
      const f1 = a2f.frames[i1] ?? f0;
      const names = a2f.names;
      for (let j = 0; j < names.length; j++) {
        const name = names[j];
        // eyeLook* 视线 / jaw* mouth* 嘴部 → 跳过（分别由 lookAt/lipSync 管）
        if (isA2FSkipped(name)) continue;
        // 只驱动模型真实存在的 custom 表情
        if (!a2fGetExpr(name)) continue;
        const v = f0[j] + (f1[j] - f0[j]) * frac;
        manager.setValue(name, v);
      }
      return true;
    }

    // ===== 眨眼状态机 =====
    let blinkTimer = 2 + Math.random() * 3; // 2~5 秒后第一次眨眼
    let blinkProgress = 0; // 0→1：闭眼过程
    let debugTimer = 0; // 诊断刷新计时

    // ===== 动画循环（限帧）=====
    // 高刷屏（如 240Hz）下 rAF 每 4.2ms 触发一次：springBone 物理 + vrm.update + 渲染
    // 会持续占用主线程。桌宠动画 60fps 已是人眼感知饱和（与 240fps 肉眼无差别），
    // 但省 75% 渲染开销。限帧后必须自己按真实经过时间累积 delta（clock.getDelta
    // 按 rAF 调用频率算，跳过帧时会算错导致动画变速）
    const TARGET_FPS = 60;
    const FRAME_MS = 1000 / TARGET_FPS;
    let lastFrameAt = performance.now();
    let elapsed = 0; // 自己累计（限帧后不能用 clock.elapsedTime）
    let animProbeTimer = 0;
    let frameCount = 0;
    function animate() {
      rafId = requestAnimationFrame(animate);
      const nowAt = performance.now();
      const dt = nowAt - lastFrameAt;
      if (dt < FRAME_MS) return; // 限帧：未到渲染时刻直接跳过（主线程几乎空闲）
      lastFrameAt = nowAt;
      const delta = Math.min(dt / 1000, 0.1); // 用真实经过时间：动画速度与帧率无关
      elapsed += delta;
      frameCount++;
      if (frameCount % 60 === 0) {
        console.log(
          `[probe] frame=${frameCount} vrm=${!!vrm} mixer=${!!animMixerRef.current} mixerT=${animMixerRef.current?.time?.toFixed(2)} delta=${delta.toFixed(4)}`
        );
      }

      if (vrm) {
        // ===== P4-A: Audio2Face 表情驱动（优先）=====
        const a2fActive = !!a2fRef.current;

        // ===== 眨眼状态机（必须在 vrm.update 之前设置！）=====
        // 眯眼类情绪（happy/angry/sad/surprised）或 A2F 说话时暂停眨眼，
        // 避免眼部 morph 叠加扭曲（恐怖谷）/双倍闭眼
        const eyeLocked =
          a2fActive ||
          (emotionRef.current
            ? EYE_LOCK_EMOTIONS.includes(emotionRef.current)
            : false);
        if (!eyeLocked) {
          blinkTimer -= delta;
          if (blinkTimer <= 0) {
            blinkProgress += delta * 6; // 闭眼速度
            if (blinkProgress >= 1) {
              blinkTimer = 2 + Math.random() * 3; // 重新计时
              blinkProgress = 0;
            }
          }
        } else {
          // 情绪表情/A2F 接管眼睛时：重置眨眼状态，让表情 morph 完全控制眼部
          blinkProgress = 0;
          blinkTimer = 2 + Math.random() * 3;
        }
        // sin 曲线让 0→1→0 平滑（慢慢闭，慢慢睁）
        const blinkWeight = eyeLocked ? 0 : Math.sin(blinkProgress * Math.PI);
        // VRM 1.0 官方表情 API
        vrm.expressionManager?.setValue("blink", blinkWeight);
        // 兜底：直接驱动网格 morph target（应对转换后 expression 绑定异常）
        applyExpression(vrm.scene, "blink", blinkWeight);
        applyExpression(vrm.scene, "Blink", blinkWeight);

        // ===== P4-A: Audio2Face 表情驱动（混合模式：A2F 管脸部，lipSync 管嘴部）=====
        if (a2fActive) {
          // 时间轴：优先用 Web Audio 时钟（与声音精确同步），无 ctx 时回退 performance.now
          const a = a2fRef.current;
          const t = a.ctx ? a.ctx.currentTime - a.startCtx : (performance.now() - (a.startPerf ?? performance.now())) / 1000;
          applyA2F(t);
          // 混合：lipSync 继续写 viseme（嘴部），A2F 只管眉/眼睑/脸颊/鼻/舌
          lipSyncRef.current?.update();
        } else {
          // 口型同步写 viseme 权重（A2F 未激活时的 fallback）
          // 关键修复：音频播完后 AudioWorklet 权重会冻结在最后非零值，
          // 无条件 update() 会让 wasSpeaking 持续为 true → 嘴一直张着。
          // 用 currentSource（播完自动置 null）判断：没在播放就跳过 update 并强制闭嘴。
          const lip = lipSyncRef.current;
          if (lip) {
            if (lip.currentSource || speakingRef.current) {
              lip.update();
            } else {
              lip.reset(); // 没在播放 → 清零 viseme 闭口（防残留权重）
            }
          }
        }
        // ===== Mixamo idle 动画 + 手势叠加（顺序关键）=====
        // mixer 先写 normalized（动画姿势）→ 手势在 normalized 上叠加头部/手臂
        // → vrm.update 把混合姿势复制回真实骨骼。手势在 vrm.update 之后改的话
        // 会被下一帧 mixer 覆盖/或把动画拉回 0，必须放在这里。
        animMixerRef.current?.update(delta);
        // 手部外展已烘焙进 idle.json（手臂不穿大腿），前端无需运行时偏移，避免累加旋转
        // 动画调试采样（验证用，可删）
        animProbeTimer += delta;
        if (animProbeTimer > 1 && animMixerRef.current) {
          animProbeTimer = 0;
          const nr = vrm.humanoid?.normalizedHumanBonesRoot;
          const h = nr?.getObjectByName?.("Normalized_hips");
          const hd = vrm.scene.getObjectByName("Head") ?? vrm.scene.getObjectByName("head");
          console.log(
            `[anim] t=${animMixerRef.current.time.toFixed(2)}s hips=${h ? h.position.toArray().map((v: number) => v.toFixed(3)).join(",") : "?"} head=${hd ? hd.quaternion.toArray().map((v: number) => v.toFixed(3)).join(",") : "?"}`
          );
        }
        // vrm.update 应用本帧所有表情/骨骼（normalized → 真实骨骼）
        vrm.update(delta);

        // 呼吸：身体轻微上下浮动（正弦波），基准高度上移共 10cm
        vrm.scene.position.y = 0.10 + Math.sin(elapsed * 1.5) * 0.008;

        // ===== 眼神动作：视线在 lookFrom/lookTo 间平滑过渡，到点换方向 =====
        if (lookTarget) {
          lookTimer -= delta;
          if (lookTimer <= 0) {
            lookTimer = 3 + Math.random() * 3;
            lookFrom.copy(lookTarget.position);
            pickLookDir();
          }
          lookTarget.position.lerp(lookTo, delta * 2.5);
        }

        // ===== 实时诊断：合并模型绑定信息 + 当前权重（每 0.5 秒刷新，面板折叠时不刷）=====
        debugTimer += delta;
        if (debugTimer > 0.5 && vrm.expressionManager && debugOpenRef.current) {
          debugTimer = 0;
          const g = (n: string) => {
            try {
              const v = vrm.expressionManager.getValue?.(n);
              return v === undefined ? "-" : v.toFixed(2);
            } catch {
              return "err";
            }
          };
          setDebugInfo(
            modelInfo +
              "\n" +
              `lipSync: ${lipSyncRef.current ? "✅" : "❌ 未创建"}\n` +
              `a2f: ${a2fRef.current ? `✅ ${a2fRef.current.frames.length}帧@${a2fRef.current.fps}fps` : "off"}\n` +
              `emotion: ${emotionRef.current ?? "neutral"} ${eyeLocked ? "(眨眼暂停)" : ""}\n` +
              `speaking: ${speakingRef.current ? "🎙️ 说话中" : "空闲"}\n` +
              `anim: ${animInfoRef.current || "off"}${animMixerRef.current ? ` [t=${animMixerRef.current.time.toFixed(1)}s]` : ""}\n` +
              `activity: ${activityInfo || "感知中..."}\n` +
              `${dshInfo ? dshInfo + "\n" : ""}` +
              `bone: normHips=${(() => { try { const n = vrm.humanoid?.normalizedHumanBonesRoot?.getObjectByName("Normalized_hips"); return n ? n.position.y.toFixed(3) : "?"; } catch { return "err"; } })()} rawHips=${(() => { try { const r = vrm.scene.getObjectByName("hips"); return r ? r.position.y.toFixed(3) : "?"; } catch { return "err"; } })()}\n` +
              `viseme: aa=${g("aa")} ih=${g("ih")} ou=${g("ou")} ee=${g("ee")} oh=${g("oh")}\n` +
              `blink: ${g("blink")}`
          );
        }
      }

      renderer.render(scene, camera);
    }
    animate();

    // 窗口大小变化同步
    function onResize() {
      camera.aspect = mount.clientWidth / mount.clientHeight;
      camera.updateProjectionMatrix();
      renderer.setSize(mount.clientWidth, mount.clientHeight);
    }
    window.addEventListener("resize", onResize);

    return () => {
      window.removeEventListener("resize", onResize);
      cancelAnimationFrame(rafId);
      animMixerRef.current?.stopAllAction?.();
      animMixerRef.current = null;
      mount.removeChild(renderer.domElement);
      renderer.dispose();
    };
  }, []);

  // 手动拖拽
  async function onMouseDown(e: React.MouseEvent) {
    const target = e.target as HTMLElement;
    if (target.closest("[data-tauri-drag-region]") && !target.closest("button, .card")) {
      await getCurrentWindow().startDragging();
    }
  }

  return (
    <div className="app">
      {/* 3D 场景挂载点（也是拖拽区） */}
      <div ref={mountRef} className="three-mount" data-tauri-drag-region onMouseDown={onMouseDown} />
      {/* 底部语音控制条 */}
      <div className="tts-bar">
        <button className={`rec-btn ${recording ? "recording" : ""}`} onClick={toggleRecord}>
          {recording ? "⏹️ 结束" : "🎤"}
        </button>
        <input
          ref={inputRef}
          defaultValue=""
          placeholder={mode === "task" ? "输入任务指令...（文件→DSH，界面操作→电脑操控）" : "对优香说点什么...（回车对话）"}
          onKeyDown={(e) => {
            // IME 防护：中文输入法选字/确认时的 Enter 不应发送（isComposing=true）
            if (e.key === "Enter" && !(e.nativeEvent as any).isComposing) {
              chat(getInputValue());
            }
          }}
        />
        <button onClick={() => chat(getInputValue())}>💬 对话</button>
        {/* 模式切换：聊天(默认) / 任务——点任务模式才执行 DSH/电脑操控，防止意图误判 */}
        <button
          className={`mode-btn ${mode === "task" ? "active" : ""}`}
          onClick={toggleMode}
          title={mode === "task" ? "当前任务模式：文件/命令→DSH，界面操作→电脑操控" : "切换任务模式：让优香执行文件/界面操作任务"}
        >
          {mode === "task" ? "💼 任务中" : "💼 任务"}
        </button>
        <button onClick={openChatLog} title="查看完整聊天记录">📜 记录</button>
      </div>
      {status && <div className="tts-status">{status}</div>}
      {/* 操作确认弹窗（安全机制：优香操控电脑前征求同意） */}
      {pendingAction && (
        <div className="confirm-overlay">
          <div className="confirm-box">
            <div className="confirm-title">
              {pendingAction.type === "click" ? "🖱️ 确认点击" : pendingAction.type === "open" ? "📂 确认打开" : "⚠️ 确认操作"}
            </div>
            <div className="confirm-desc">{pendingAction.desc}</div>
            <div className="confirm-btns">
              <button
                className="confirm-yes"
                onClick={() => {
                  // 防连点：双击确认只执行一次 action（否则 CUA 双实例并发）
                  if (confirmLockRef.current) return;
                  confirmLockRef.current = true;
                  try {
                    pendingAction.action();
                  } finally {
                    setTimeout(() => {
                      confirmLockRef.current = false;
                    }, 400);
                  }
                }}
              >
                ✅ 确认
              </button>
              <button className="confirm-no" onClick={() => setPendingAction(null)}>
                ❌ 取消
              </button>
            </div>
          </div>
        </div>
      )}
      {/* 右上角：最小化按钮（无边框窗口没有系统按钮） */}
      <button
        className="win-min-btn"
        title="最小化"
        onClick={() => getCurrentWindow().minimize()}
      >
        ─
      </button>
      {/* 调试面板：默认折叠为小徽章，点击展开 */}
      {debugInfo && (
        <div className="debug-wrap">
          <button className="debug-toggle" onClick={toggleDebug}>
            {debugOpen ? "✕" : "🔍"}
          </button>
          {debugOpen && (
            <div className="debug-panel">
              <pre>{debugInfo}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export default App;
