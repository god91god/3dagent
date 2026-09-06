# 🎀 优香 AI 桌宠（Yuuka AI Desktop Pet）

一个 3D 二次元 AI 办公助手桌宠。以 VRM 虚拟形象为外壳，以 DSH 办公执行 + Computer-Use 电脑操控 + 五维记忆 + 主动陪伴为内核，常驻桌面透明置顶窗口，陪你工作、聊天、记得你的一切。

> **Tauri 2 + React 19 + three.js + three-vrm + Rust 后端**

> 🤖 **不想看文档？** 把 [AI_SETUP_GUIDE.md](AI_SETUP_GUIDE.md) 全文丢给你的 AI 助手（Claude/ChatGPT/DeepSeek 等），它会一步步帮你装好。

## ✨ 功能总览

| 能力 | 说明 |
|---|---|
| 💬 **语音聊天** | 打字/语音输入（SenseVoice 本地 ASR）→ Qwen 对话 → Qwen TTS 语音回复 + lipSync/Audio2Face 表情口型 |
| 🧠 **五维记忆** | 事实提取 → 证据评分（正负双通道衰减）→ 反思合成 → 晋升人格；LLM 去重仲裁防记忆膨胀 |
| 🎯 **主动搭话** | 喝水/防摸鱼提醒 + 话题深池（你反复在意的事）+ **未收尾话题追问**（说一半的话优香会续上）+ 3-tier 打扰退避 |
| 👀 **屏幕感知** | 5s 轮询前台窗口/空闲 → 9 态状态机 → activity_guess 叙述"你在干嘛"；主动搭话前先看屏幕 |
| 💼 **办公执行** | 文件/命令/代码任务全部交给 **DSH**（headless agent），带任务历史注入（跨会话上下文） |
| 🖱️ **电脑操控** | Computer-Use Agent（N.E.K.O. Kimi OSWorld 范式）：Thought+Action+Code 多步闭环，点击/输入/快捷键，操作前确认 |
| 🔄 **聊天/任务模式** | 平常只聊天（防意图误判）；点 💼 按钮切任务模式才执行 DSH/电脑操控 |
| 🎭 **3D 形象** | VRM 1.0 模型（优香），眨眼/呼吸/11 种情绪表情/A2F 面部驱动/SpringBone 物理/Mixamo 动画 |

## 🏗️ 架构

```
┌─ Rust (Tauri backend) ────────────────────────────────┐
│  记忆系统（facts/reflections/persona JSON + 证据数学）   │
│  活动感知循环（5s 窗口轮询 → 状态机 → ActivitySnapshot） │
│  主动搭话调度（提醒/话题/搭话 + 3-tier 退避 + 防复读）   │
│  DSH 任务桥（任务历史注入 + 状态感知）                   │
│  Computer-Use 桥（Python 子进程 + 取消机制）            │
└───────────────────┬────────────────────────────────────┘
                    │ invoke / events
┌───────────────────▼────────────────────────────────────┐
│ React (WebView2)                                       │
│  three.js VRM 渲染 + 表情/口型/动画                     │
│  语音输入（MediaRecorder→WAV→SenseVoice）               │
│  语音输出（Qwen TTS → Web Audio + lipSync/A2F）         │
└────────────────────────────────────────────────────────┘
```

三层职责：
- **优香** = 形象/陪伴/感知（Qwen 系模型：对话 + 视觉 + 语音）
- **DSH** = 办公执行（headless agent 全权处理文件/命令/代码）
- **Computer-Use** = GUI 操控（视觉理解 + pyautogui 动作闭环）

## 🚀 运行

### 开发模式
```bash
pnpm install
pnpm tauri dev
```

### 构建独立版（桌面快捷方式）
```bash
pnpm tauri build --no-bundle   # 产物: src-tauri/target/release/dagent.exe
```

### 配置（config.json，已被 .gitignore 忽略，含 API key）
```jsonc
{
  "qianwen_api_key": "sk-...",        // 阿里云百炼 API key（对话/视觉/TTS/记忆全链路）
  "chat_model": "qwen3.6-flash",      // 聊天/意图/事实提取/压缩/搭话（便宜模型走这里）
  "vision_model": "qwen3-vl-plus",    // 视觉模型（识图/电脑操控）
  "memory_model": "qwen-max",         // 记忆高智力任务（去重仲裁/反思合成/信号检测/话题筛选）
  "tts_model": "qwen3-tts-vd-realtime-2026-01-15",   // TTS：vd 版（voice-design 音色，1 元/万字符）
  "tts_voice": "qwen-tts-vd-yuuka_pet-voice-..."      // 音色 ID：qwen-voice-design 创建的自定义音色
}
```

模型分级：`chat_model`（省 token 的日常任务）与 `memory_model`（记忆质量命门的强模型）分离；换模型只改 `config.json` 对应字段。

**TTS 音色定制**：`qwen3-tts-vd-realtime` 无内置音色，需先用 qwen-voice-design 创建（POST `https://dashscope.aliyuncs.com/api/v1/services/audio/tts/customization`，`action: "create"`，传 `voice_prompt` 文字描述 + `target_model`，返回 `output.voice` 即音色 ID）→ 填入 `tts_voice`。

## 🧠 记忆系统（借鉴 N.E.K.O.）

五维记忆闭环：每轮对话后台提取事实 → 去重仲裁 → 反思合成 → 证据信号（reinforcement 30天/ disputation 180天半衰期，"否定记得更久"）→ 晋升合入人格。聊天时自动注入相关记忆，问"关于我你知道些什么"可查看完整画像。

## 📜 致谢与版权

本项目大量借鉴以下开源项目的设计、算法与代码，在此致谢并保留其版权声明：

- **[Project N.E.K.O.](https://github.com/Project-N-E-K-O/N.E.K.O)**（Apache-2.0）—— 五维记忆系统、活动感知状态机、主动搭话引擎、话题深池、未收尾话题检测、Computer-Use Agent、Qwen TTS 协议。对应移植文件头均保留上游版权声明。
- **[枫云AI助手社区版](https://github.com/MewCo-AI/mewco_ai_assistant_comm)**（GPL-3.0）—— 架构设计参考（引擎抽象/形象与内核解耦）。
- **[deepseek-harness-pet](https://github.com/wraven68/deepseek-harness-pet)**、**[dsh-dafeiyu](https://github.com/QCYTSN/dsh-dafeiyu)** —— DSH 会话日志解析与状态感知。
- **[wav2arkit_cpu](https://huggingface.co/myned-ai/wav2arkit_cpu)**（Apache-2.0）—— Audio2Face 表情推理。
- 优香 VRM 模型来自作者 **Hayase Yuuka**（详见模型文件内元数据）。

## 📄 License

[GPL-3.0](LICENSE) © 2026 sb

> 说明：本仓库不含 `config.json`（含 API key，已被 .gitignore 排除）；**VRM 模型不随仓库分发**（模型版权归原作者，仅限个人使用），如需运行请自行准备 VRM 1.0 模型放入 `public/models/`。
