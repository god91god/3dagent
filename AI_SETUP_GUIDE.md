# 🤖 AI 安装指南（给 AI 看的，不是给人看的）

> 如果你是人类：请把**本文件全文**复制粘贴给你的 AI 助手（Claude/ChatGPT/DeepSeek/通义等任意一个），然后让它按下面的步骤帮你安装。它读得懂下面的每一条指令。

---

## 你的任务

帮用户在本机安装并跑起「优香 AI 桌宠」这个 Tauri 2 桌面应用。按顺序执行以下步骤，**每步先检测再执行**，不要跳过检测。遇到错误先诊断再继续，不要卡死。

## 项目结构速览

```
3dagent/
├── src/                 # React 前端（three.js + VRM 渲染）
├── src-tauri/           # Rust 后端（Tauri 2）
│   └── src/             #   memory.rs / proactive.rs / vision.rs 等
├── tools/               # Python 脚本（TTS/ASR/A2F/CUA）
├── public/models/       # VRM 模型（优香_new.vrm 已入库）
└── config.json          # 配置（含 API key，需用户创建，已 gitignore）
```

## 步骤 1：环境检测

逐项检测，缺什么装什么：

| 依赖 | 最低版本 | 检测命令 | 备注 |
|---|---|---|---|
| Node.js | 20+ | `node -v` | 推荐 24 |
| pnpm | 9+ | `pnpm -v` | 没有就用 `npm i -g pnpm` |
| Rust | 1.75+ | `rustc -V` | 推荐用 rsproxy 镜像（国内） |
| Python | 3.10+ | `python --version` | 需要能跑 pip |
| ffmpeg | 任意 | `ffmpeg -version` | TTS/A2F 音频转换必需 |
| Git | 任意 | `git --version` | 拉代码用 |

**国内网络提示**：如果 npm/pnpm 安装慢或失败，配置国内镜像：
```bash
pnpm config set registry https://registry.npmmirror.com
cargo config set source.crates-io replace-with rsproxy-sparse  # 或用环境变量 RUSTUP_DIST_SERVER
```

## 步骤 2：拉取代码并装依赖

```bash
git clone https://github.com/god91god/3dagent.git
cd 3dagent
pnpm install          # 前端依赖
```

## 步骤 3：创建 config.json（关键！缺了会报错）

项目根目录没有 `config.json`（含 API key，不进仓库）。帮用户创建：

```json
{
  "qianwen_api_key": "<用户自己的阿里云百炼 API key>",
  "chat_model": "qwen3.6-flash",
  "vision_model": "qwen3-vl-plus",
  "memory_model": "qwen-max",
  "tts_model": "qwen3-tts-vd-realtime-2026-01-15",
  "tts_voice": "qwen-tts-vd-yuuka_pet-voice-..."
}
```

**API key 获取**（需要用户操作，你不能代办）：
1. 打开 https://bailian.console.aliyun.com/ 注册/登录阿里云
2. 开通「百炼」（模型服务平台），创建 API-KEY（形如 `sk-...`）
3. 让用户把 key 给你，填入上面 `qianwen_api_key` 字段

**音色说明**：`tts_voice` 是 voice-design 创建的自定义音色 ID。如果用户没有现成的，用以下两种方式之一：
- 方式 A（默认）：把 `tts_model` 改回内置音色模型 `qwen3-tts-flash-realtime-2025-11-27`、`tts_voice` 填 `Cherry`
- 方式 B（自定义音色）：参考 README 的「TTS 音色定制」章节，调 voice-design API 创建

## 步骤 4：Python 依赖

```bash
pip install -r requirements.txt    # 若项目里有
```
没有 requirements.txt 的话，按需安装：
```bash
pip install edge-tts websockets numpy onnxruntime scipy  # TTS + A2F
pip install pyautogui pyperclip pillow requests          # Computer-Use 操控
pip install sherpa-onnx                                     # 语音识别（可选，装不上不影响核心）
```
**国内镜像**：`pip config set global.index-url https://mirrors.aliyun.com/pypi/simple/`
**注意**：pyautogui 清华镜像可能没有，用阿里云镜像。

## 步骤 5：模型文件确认

```bash
ls public/models/
# 应看到：优香_new.vrm（主力模型，已入库）
```
如果缺模型（例如用户 clone 时没拉到 LFS），告诉用户需要自行准备 VRM 1.0 模型放入 `public/models/`，并在 `src/App.tsx` 里确认加载路径。

## 步骤 6：启动

```bash
pnpm tauri dev
```
首次运行会编译 Rust（几分钟），然后弹出透明置顶的优香窗口。

**验证清单**（让用户确认）：
- [ ] 窗口出现 3D 优香，会眨眼/呼吸
- [ ] 输入文字回车，优香用语音回复（TTS）
- [ ] 说"看看屏幕"，优香描述屏幕内容（视觉）
- [ ] 点 💼 按钮切任务模式，说"读取 C:\xxx 文件"，交给 DSH 执行
- [ ] 调试面板（右上角 🔍）显示 activity/lipSync/emotion 状态

## 常见问题排查（按优先级）

1. **`config.json 缺少 qianwen_api_key`** → 步骤 3 没做或 key 填错
2. **TTS 失败回退 edge-tts** → 检查 `tts_model`/`tts_voice` 是否匹配（vd 模型必须用 voice-design 音色，不能填 Cherry）
3. **Rust 编译失败** → 检查 rustc 版本、cargo 镜像、MSVC 构建工具（Windows 需装 Visual Studio Build Tools + C++ 组件）
4. **窗口白屏** → `pnpm install` 没装全；或 `pnpm tauri dev` 端口被占（杀掉残留 dagent.exe / node 再试）
5. **打字卡顿** → 正常现象已优化；如果卡得离谱，检查是否调试面板展开（展开才有轮询）
6. **Python 找不到模块** → 确认用对了 Python 解释器（Rust 后端调用的是固定路径，Windows 上可能是 `C:\Users\<用户名>\AppData\Local\Python\pythoncore-3.x-64\python.exe`）

## 给用户的最后说明

安装完成后告诉用户：
- 改模型/音色：只改 `config.json`，不用动代码
- 桌面快捷方式：`pnpm tauri build --no-bundle` 后指向 `src-tauri/target/release/dagent.exe`
- 记忆存在系统 AppData 目录（`%APPDATA%\com.tauri-app.dagent\memory\`），重装不丢
